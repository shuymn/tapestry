//! The input thread's side of each play: recording opens and closes at a
//! boundary between two batches, on request from the game thread, and the
//! input thread wakes the logic step after every publication (D8).
//!
//! The requests go through a channel the hook polls at every wake (an
//! empty poll allocates nothing); the replies carry what only the input
//! thread can read: the snapshot, the readers, the arrivals and the
//! overflow flags. Between requests the hook publishes the gate's state
//! and whether a participating device is still connected, for the abort
//! checks of the play and its audio tail.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::Thread;

use bms_model::TimeUs;
use eval::{ControlId, InputStart};
use input::{Arrival, DeviceId, DeviceKind, SourceReader, SourceSet};

use crate::controls::Controls;

/// What the game thread asks of the input thread.
pub enum Request {
    /// Open recording at the next boundary; chart time 0 lies `lead_in`
    /// after it.
    Open {
        /// The countdown's length.
        lead_in: TimeUs,
        /// The thread to wake after each publication.
        logic: Thread,
        /// The controls recorded from keyboards and from controllers.
        keyboard: Vec<ControlId>,
        /// See `keyboard`.
        controller: Vec<ControlId>,
    },
    /// Close recording at the next boundary.
    Close,
}

/// The input thread's answers.
pub enum Reply {
    /// Recording is open.
    Opened(Opened),
    /// Recording is closed.
    Closed(Closed),
}

/// An opened recording.
pub struct Opened {
    /// Where chart time 0 lies on the clock.
    pub origin: TimeUs,
    /// The controls held at the opening.
    pub start: InputStart,
    /// The logic step's ends of the source queues.
    pub readers: Vec<SourceReader>,
    /// The participants' kinds, indexed by source id.
    pub kinds: Vec<DeviceKind>,
}

/// A closed recording.
pub struct Closed {
    /// Devices that arrived while it was open.
    pub arrivals: Vec<Arrival>,
    /// Whether a queue overflowed or a device's held state was lost.
    pub overflowed: bool,
    /// Whether a device did not take part because of the source limit.
    pub source_limited: bool,
}

/// What the input thread publishes between requests.
#[derive(Debug, Default)]
pub struct InputState {
    tripped: AtomicBool,
    connected: AtomicBool,
}

/// The game thread's end of the input side.
pub struct InputLink {
    requests: Sender<Request>,
    replies: Receiver<Reply>,
    state: Arc<InputState>,
}

impl InputLink {
    /// Opens recording for a play.
    ///
    /// # Panics
    ///
    /// If the input thread is gone.
    pub fn open(&self, request: Request) -> Opened {
        self.requests.send(request).expect("the input thread runs");
        match self.replies.recv().expect("the input thread runs") {
            Reply::Opened(o) => o,
            Reply::Closed(_) => unreachable!("an open is answered by an opening"),
        }
    }

    /// Closes recording.
    ///
    /// # Panics
    ///
    /// If the input thread is gone.
    pub fn close(&self) -> Closed {
        self.requests
            .send(Request::Close)
            .expect("the input thread runs");
        match self.replies.recv().expect("the input thread runs") {
            Reply::Closed(c) => c,
            Reply::Opened(_) => unreachable!("a close is answered by a closing"),
        }
    }

    /// Whether the focus gate has tripped since recording last opened.
    pub fn tripped(&self) -> bool {
        self.state.tripped.load(Ordering::Acquire)
    }

    /// Whether a device that took part in the last recording is still
    /// connected (the same connection).
    pub fn connected(&self) -> bool {
        self.state.connected.load(Ordering::Acquire)
    }
}

/// How the input thread maps its batch times onto a play.
pub enum Mode {
    /// Raw Input: batches carry the clock's time, and recording opens with
    /// the play's origin, so the source set moves them onto the play.
    Clock,
    /// A script on the play's timeline, polled with the play's time.
    #[cfg(feature = "dev-tools")]
    Script(Box<Script>),
}

/// A recorded play replayed by every play of the run: each play starts from
/// the log's first item, on a source set in its initial state.
#[cfg(feature = "dev-tools")]
pub struct Script {
    /// The log as a script, never polled.
    pub log: input::ScriptedInput,
    /// The current play's copy.
    pub current: input::ScriptedInput,
    /// The source set's sizes, for a fresh one at each play.
    pub sources: input::SourceConfig,
}

/// The input thread's end: called at every boundary between batches.
pub struct Hook {
    requests: Receiver<Request>,
    replies: Sender<Reply>,
    state: Arc<InputState>,
    controls: Arc<Controls>,
    mode: Mode,
    logic: Option<Thread>,
    /// Where the open play's time 0 lies on the clock.
    origin: Option<TimeUs>,
    /// The last recording's participants and their connections.
    participants: Vec<(DeviceId, Option<u64>)>,
}

/// A linked hook and link.
pub fn link(controls: Arc<Controls>, mode: Mode) -> (Hook, InputLink) {
    let (req_tx, req_rx) = mpsc::channel();
    let (rep_tx, rep_rx) = mpsc::channel();
    let state = Arc::new(InputState::default());
    (
        Hook {
            requests: req_rx,
            replies: rep_tx,
            state: Arc::clone(&state),
            controls,
            mode,
            logic: None,
            origin: None,
            participants: Vec::new(),
        },
        InputLink {
            requests: req_tx,
            replies: rep_rx,
            state,
        },
    )
}

impl Hook {
    /// One boundary, `now` being the clock read there. A script is first
    /// fed up to the play's time.
    pub fn wake(&mut self, sources: &mut SourceSet, now: TimeUs) {
        #[cfg(feature = "dev-tools")]
        if let (Mode::Script(script), Some(origin)) = (&mut self.mode, self.origin) {
            input::InputAdapter::poll(&mut script.current, TimeUs(now.0 - origin.0), sources);
        }
        while let Ok(request) = self.requests.try_recv() {
            let reply = match request {
                Request::Open {
                    lead_in,
                    logic,
                    keyboard,
                    controller,
                } => Reply::Opened(self.open(sources, now, lead_in, logic, &keyboard, &controller)),
                Request::Close => Reply::Closed(self.close(sources)),
            };
            // Published before the reply, so the game thread never reads
            // the previous play's state after it.
            self.publish(sources);
            // The game thread waits for the reply; if it is gone, so is
            // the play.
            let _ = self.replies.send(reply);
        }
        self.publish(sources);
        if self.origin.is_some()
            && let Some(logic) = &self.logic
        {
            logic.unpark();
        }
    }

    fn publish(&self, sources: &SourceSet) {
        self.state
            .tripped
            .store(sources.gate().is_tripped(), Ordering::Release);
        let connected = self
            .participants
            .iter()
            .any(|&(d, c)| c.is_some() && sources.connection(d) == c);
        self.state.connected.store(connected, Ordering::Release);
    }

    fn open(
        &mut self,
        sources: &mut SourceSet,
        now: TimeUs,
        lead_in: TimeUs,
        logic: Thread,
        keyboard: &[ControlId],
        controller: &[ControlId],
    ) -> Opened {
        let origin = TimeUs(now.0 + lead_in.0);
        let start = match &mut self.mode {
            Mode::Clock => {
                let devices: Vec<_> = sources.devices().collect();
                for (d, kind) in devices {
                    let list = match kind {
                        DeviceKind::Keyboard => keyboard,
                        DeviceKind::Controller => controller,
                    };
                    sources.set_recorded(d, list.to_vec());
                }
                reset_gate(sources, &self.controls);
                sources.open_play(now, origin)
            }
            #[cfg(feature = "dev-tools")]
            Mode::Script(script) => {
                // The log again from its start, on devices as they were
                // before it: they connect, with their recorded controls,
                // before the opening.
                script.current = script.log.clone();
                *sources = SourceSet::new(script.sources.clone())
                    .expect("the run's source set was made with these sizes");
                let open_at = TimeUs(-lead_in.0);
                input::InputAdapter::poll(&mut script.current, TimeUs(open_at.0 - 1), sources);
                reset_gate(sources, &self.controls);
                sources.open_recording(open_at)
            }
        };
        self.origin = Some(origin);
        self.logic = Some(logic);
        self.participants = sources
            .participants()
            .map(|(d, _)| (d, sources.connection(d)))
            .collect();
        Opened {
            origin,
            start,
            readers: sources.take_readers(),
            kinds: sources.participants().map(|(_, k)| k).collect(),
        }
    }

    fn close(&mut self, sources: &mut SourceSet) -> Closed {
        let closed = Closed {
            arrivals: sources.arrivals().to_vec(),
            overflowed: sources.overflowed(),
            source_limited: sources.source_limited(),
        };
        sources.close_recording();
        self.origin = None;
        closed
    }
}

/// Opens the gate for a new play; a window without focus latches it again
/// at once, so a play never records while the game is in the background.
fn reset_gate(sources: &mut SourceSet, controls: &Controls) {
    sources.gate_mut().reset();
    if !controls.focused() {
        sources.gate().latch();
    }
}
