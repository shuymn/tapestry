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
use eval::ControlId;
use game::{Closing, Opening};
use input::hid::HidProduct;
use input::{DeviceId, DeviceKind, SourceReader, SourceSet};

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
        /// What the play records.
        recorded: Recorded,
    },
    /// Close recording at the next boundary.
    Close,
}

/// What a play records, from the key configs.
#[derive(Clone, Debug)]
pub struct Recorded {
    /// The controls recorded from every keyboard.
    pub keyboard: Vec<ControlId>,
    /// The controls recorded from the controller.
    pub controller: Vec<ControlId>,
    /// The controller they belong to; `None` when no controller binds a
    /// lane.
    pub controller_device: Option<HidProduct>,
}

/// The input thread's answers.
pub enum Reply {
    /// Recording is open.
    Opened(Opening<SourceReader>),
    /// Recording is closed.
    Closed(Closing),
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
    pub fn open(&self, request: Request) -> Opening<SourceReader> {
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
    pub fn close(&self) -> Closing {
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
                    recorded,
                } => Reply::Opened(self.open(sources, now, lead_in, logic, &recorded)),
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
        recorded: &Recorded,
    ) -> Opening<SourceReader> {
        let origin = TimeUs(now.0 + lead_in.0);
        let start = match &mut self.mode {
            Mode::Clock => {
                if !take_part(sources, recorded) {
                    eprintln!(
                        "input: the controller set up in `tapestry keys` is not connected; this play uses the keyboard bindings only"
                    );
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
        Opening {
            origin,
            start,
            readers: sources.take_readers(),
            kinds: sources.participants().map(|(_, k)| k).collect(),
            axes: sources
                .participants()
                .map(|(d, _)| sources.axes(d).to_vec())
                .collect(),
        }
    }

    fn close(&mut self, sources: &mut SourceSet) -> Closing {
        let closed = Closing {
            arrivals: sources.arrivals().to_vec(),
            overflowed: sources.overflowed(),
            source_limited: sources.source_limited(),
            axis_out_of_range: sources.axis_out_of_range().collect(),
        };
        // Shown once per play: the record keeps the counts.
        for (i, &n) in closed.axis_out_of_range.iter().enumerate() {
            if n > 0 {
                eprintln!(
                    "input: source {i} reported {n} axis values outside the range its controller declares; set its scratch range in device.toml"
                );
            }
        }
        sources.close_recording();
        self.origin = None;
        closed
    }
}

/// Sets what each connected device records and whether it takes part:
/// every keyboard records the keyboard controls, and the first controller
/// of the configured model ([`Recorded::controller_device`]) the
/// controller's; any other controller, a second of that model included,
/// takes no part. Returns `false` when a controller is configured and none
/// of its model is connected.
fn take_part(sources: &mut SourceSet, recorded: &Recorded) -> bool {
    let devices: Vec<_> = sources.devices().collect();
    let mut matched = false;
    for (d, kind) in devices {
        let (list, takes_part) = match kind {
            DeviceKind::Keyboard => (&recorded.keyboard, true),
            DeviceKind::Controller => {
                let mine = !matched
                    && recorded.controller_device.is_some()
                    && sources.product(d) == recorded.controller_device;
                matched |= mine;
                (&recorded.controller, mine)
            }
        };
        sources.set_recorded(d, list.clone());
        sources.set_takes_part(d, takes_part);
    }
    matched || recorded.controller_device.is_none()
}

/// Opens the gate for a new play; a window without focus latches it again
/// at once, so a play never records while the game is in the background.
fn reset_gate(sources: &mut SourceSet, controls: &Controls) {
    sources.gate_mut().reset();
    if !controls.focused() {
        sources.gate().latch();
    }
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;
    use eval::ControlId;
    use input::hid::HidProduct;
    use input::{DeviceId, DeviceKind, Packet, SourceConfig, SourceSet};

    use super::{Recorded, take_part};

    const TURNTABLE: HidProduct = HidProduct {
        vendor_id: 0x1A2B,
        product_id: 0x3C4D,
    };
    const GAMEPAD: HidProduct = HidProduct {
        vendor_id: 0x5E6F,
        product_id: 0x7081,
    };
    /// Button 1 plays key 1 on the controller; retry and quit are keys.
    const BUTTON: ControlId = ControlId(1);
    const RETRY: ControlId = ControlId(0x0e);

    fn recorded(device: Option<HidProduct>) -> Recorded {
        Recorded {
            keyboard: vec![RETRY, ControlId(0x01)],
            controller: vec![BUTTON],
            controller_device: device,
        }
    }

    /// Keyboards 1 and 2, then a gamepad (10), the turntable (11) and a
    /// second turntable of the same model (12), each with its product.
    fn devices() -> SourceSet {
        let mut set = SourceSet::new(SourceConfig {
            max_devices: 8,
            max_sources: 8,
            max_held: 8,
            queue_capacity: 64,
            max_arrivals: 2,
        })
        .unwrap();
        set.begin_batch(TimeUs(0));
        for d in [1, 2] {
            set.connect(DeviceId(d), DeviceKind::Keyboard);
        }
        for (d, product) in [(10, GAMEPAD), (11, TURNTABLE), (12, TURNTABLE)] {
            set.connect(DeviceId(d), DeviceKind::Controller);
            set.set_product(DeviceId(d), product);
        }
        set.end_batch(TimeUs(0));
        set
    }

    fn press(device: u64) -> Packet {
        Packet {
            device: DeviceId(device),
            control: BUTTON,
            value: 1,
            background: false,
        }
    }

    /// Only the first controller of the configured model plays: a gamepad
    /// and a second controller of that model, with the same control ids,
    /// take no part and record nothing.
    #[test]
    fn only_the_configured_controller_takes_part() {
        let mut set = devices();
        assert!(take_part(&mut set, &recorded(Some(TURNTABLE))));
        set.open_recording(TimeUs(10));
        let mut readers = set.take_readers();
        let sources: Vec<_> = set.participants().map(|(d, _)| d.0).collect();
        assert_eq!(sources, [11, 1, 2]);
        assert!(!set.source_limited());
        set.begin_batch(TimeUs(20));
        for d in [10, 11, 12] {
            set.packet(&press(d));
        }
        set.end_batch(TimeUs(21));
        let events: Vec<_> = std::iter::from_fn(|| readers[0].pop()).collect();
        assert_eq!(events.len(), 1);
        assert!(readers[1..].iter_mut().all(|r| r.pop().is_none()));
    }

    /// With the configured model not connected, or no controller
    /// configured, only keyboards take part; only the first case is
    /// reported as missing.
    #[test]
    fn without_the_configured_controller_only_keyboards_take_part() {
        let other = HidProduct {
            vendor_id: 0x0A0B,
            product_id: 0x0C0D,
        };
        for (device, found) in [(Some(other), false), (None, true)] {
            let mut set = devices();
            assert_eq!(take_part(&mut set, &recorded(device)), found);
            set.open_recording(TimeUs(10));
            let sources: Vec<_> = set.participants().map(|(d, _)| d.0).collect();
            assert_eq!(sources, [1, 2]);
        }
    }
}
