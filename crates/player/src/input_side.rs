//! The input thread's side of each play: recording opens and closes at a
//! boundary between two batches, on request from the game thread, and the input
//! thread wakes the logic step after every publication (design.md "Overall
//! Architecture").
//!
//! The requests go through a channel the hook polls at every wake (an
//! empty poll allocates nothing), and the input thread's wake event is
//! signaled after each, so an idle thread serves it at once; the replies
//! carry what only the input thread can read: the snapshot, the readers,
//! the arrivals and the overflow flags. Between requests the hook
//! publishes the gate's state and whether a participating device is still
//! connected, for the abort checks of the play and its audio tail.
//!
//! A completed play's close is a handoff (design.md "Play Lifecycle", the
//! audio tail): the game thread asks for it through a bounded queue and the
//! input thread answers at its next boundary with the closing, copied into
//! storage reserved at the opening, so the handoff allocates nothing; the
//! sources stay, and input goes on reaching the play's queues for the
//! tail's feedback until the close that follows the tail. The hook's lines
//! for the play are made at that close, outside the play.
//!
//! An aborted play's close can be the same exchange, sealed
//! ([`InputLink::abort_close`]): the closing comes back in the same
//! storage and nothing more reaches the play's queues, while the sources
//! stay, and the hook's lines wait, until the close that follows.
//!
//! The hook reports the thread's phase (`input::cadence`): Open from an
//! open through a handoff to the close, Tail from a close, and Idle only from [`Request::Idle`], which the
//! game thread sends once a play's audio tail is over. At each boundary
//! the hook also sets the controller's menu active only while Idle with the
//! window focused, publishes START and SELECT as the select screen's panel
//! level by the same rule, and wakes the game thread when the batch sent a
//! menu event or the level changed (design.md "Input").
//!
//! The game thread waits for each reply at most [`INPUT_TIMEOUT`] of awake
//! time; an input thread that is gone or does not answer fails the link for
//! good. The input thread prints nothing: what the hook has to say travels
//! with its reply and is printed by the game thread, so a blocked console
//! never stalls the input thread.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::Thread;
use std::time::Duration;

use bms_model::TimeUs;
use eval::ControlId;
use game::{Closing, InputWaitError, Opening, ScratchConfig};
use input::hid::HidProduct;
use input::{DeviceId, DeviceKind, Phase, SourceReader, SourceSet, Waker};

use crate::controls::Controls;

/// How long the game thread waits for the input thread to open or close
/// recording: far beyond a wake, at which a live input thread answers.
pub const INPUT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long each wait for a reply lasts before the awake time is read
/// again.
const WAIT_SLICE: Duration = Duration::from_millis(50);

/// How long the game thread sleeps between looks for a rearm's answer,
/// which the input thread gives at its next boundary.
const REARM_POLL: Duration = Duration::from_micros(200);

/// The time the system has been awake, which stands still during a suspend
/// (`QueryUnbiasedInterruptTime`), so a suspend during a handshake does
/// not count against [`INPUT_TIMEOUT`].
fn awake_time() -> Duration {
    let mut t = 0_u64;
    // SAFETY: one u64 written.
    let _ =
        unsafe { windows::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime(&mut t) };
    // 100 ns units.
    Duration::from_nanos(t.saturating_mul(100))
}

/// What the game thread asks of the input thread.
pub enum Request {
    /// Open recording at the next boundary; chart time 0 lies `lead_in`
    /// after it.
    Open {
        /// READY's length.
        lead_in: TimeUs,
        /// The thread to wake after each publication.
        logic: Thread,
        /// What the play records.
        recorded: Recorded,
        /// With the start gate (design.md "Play Lifecycle", the start
        /// gate), the READY candidate's generation: recording opens only
        /// where START and SELECT are verifiably released
        /// ([`input::SourceSet::open_candidate`]), else the opening is
        /// deferred ([`Reply::Deferred`]).
        gate: Option<u32>,
        /// With a gate out of the loading state, the window's focus-loss
        /// generation the tuning acceptance closed with: a loss since
        /// defers the opening too.
        focus: Option<u64>,
    },
    /// Close recording at the next boundary.
    Close,
    /// Feed the loading script ([`Script::loading`]) from now on, its time 0
    /// at `at` on the clock, while recording is closed: its packets reach
    /// only the adjustment tap, and the thread wakes for each of its items
    /// though Idle. It ends at the boundary of the first successful
    /// opening, which the play's script takes over from on the same
    /// devices, or at [`Request::EndLoadingScript`]. Not answered (dev-tools).
    #[cfg(feature = "dev-tools")]
    LoadingScript {
        /// The script's time 0 on the clock.
        at: TimeUs,
    },
    /// Stop feeding the loading script: the load was cancelled or refused,
    /// or the window closes. Not answered (dev-tools).
    #[cfg(feature = "dev-tools")]
    EndLoadingScript,
    /// Answer fence `n` in the adjustment tap's stream at the next
    /// boundary ([`input::TapEvent::Fence`]). Not answered otherwise.
    Fence(u64),
    /// The adjustment controls' tap from now on, whose hold latch and
    /// start gate the gated openings and rearms use. Not answered.
    Tap(Box<input::AdjustmentTap>),
    /// Nothing waits on the input thread's publications until the next
    /// open: it stops its heartbeat. Not answered; ignored while
    /// recording is open.
    Idle,
    /// Autoplay of another chart, between plays: its generated input, and
    /// the events each source's queue holds for it. Not answered; ignored
    /// by the other modes. The replaced input is freed on the input
    /// thread, outside any play.
    Feed {
        /// The chart's generated input.
        input: input::AutoplayInput,
        /// Its source queue's size.
        queue_capacity: usize,
    },
    /// Another chart of a development scenario, between plays: the input
    /// thread starts over from a source set of `sources`, keeping its focus
    /// gate and menu, and feeds every play from `mode`. Not answered; the
    /// replaced mode is freed on the input thread, outside any play.
    #[cfg(feature = "dev-tools")]
    Switch {
        /// How the chart's plays are fed.
        mode: Box<Mode>,
        /// The source set's sizes.
        sources: input::SourceConfig,
    },
    /// Stops the thread at its next fence ([`FenceGate`]; dev-tools
    /// tests). Not answered.
    #[cfg(all(test, feature = "dev-tools"))]
    FenceGate(Box<FenceGate>),
    /// Stops the thread at its next gated opening ([`OpenGate`]; tests).
    /// Not answered.
    #[cfg(test)]
    OpenGate(Box<OpenGate>),
}

/// An interleaving with the game thread at the input thread's next fence
/// (dev-tools tests): `before` runs on the source set as the fence is
/// taken, and once the fence is answered the thread sends `reached` and
/// waits for `resume` (at most 60 s) before the rest of its boundary.
#[cfg(all(test, feature = "dev-tools"))]
pub struct FenceGate {
    /// Runs on the source set before the fence is answered.
    pub before: Box<dyn FnOnce(&mut SourceSet) + Send>,
    /// Sent once the fence is answered.
    pub reached: mpsc::Sender<()>,
    /// Waited for after `reached`.
    pub resume: mpsc::Receiver<()>,
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
    /// How the controller's bound axes are converted, for the range their
    /// values are counted against.
    pub scratch: ScratchConfig,
}

/// The input thread's answers, each with the console lines the hook has
/// for it.
pub enum Reply {
    /// Recording is open.
    Opened(Opening<SourceReader>, Vec<String>),
    /// Recording is closed.
    Closed(Closing, Vec<String>),
    /// A gated opening found START or SELECT held, or possibly held:
    /// nothing opened.
    Deferred,
}

/// A rearm of the READY candidate (design.md "Play Lifecycle", the start
/// gate), exchanged whole between the game thread and the input thread
/// through a bounded queue, so the exchange allocates nothing: the
/// readers go to the input thread and come back, and the buffers, reserved
/// with the play's setup, carry what the input thread found.
#[derive(Debug)]
pub struct Rearm {
    /// The attempt's readers, drained by the game thread into the
    /// cancelled opening's history.
    pub readers: Vec<SourceReader>,
    /// READY's length.
    pub lead_in: TimeUs,
    /// The next candidate's generation.
    pub generation: u32,
    /// The next opening's held-input snapshot, copied within its capacity.
    pub start: eval::InputStart,
    /// The sealed opening's arrivals, within the capacity.
    pub arrivals: Vec<input::Arrival>,
    /// The sealed opening's events pushed after the game thread's last
    /// drain, within the capacity.
    pub leftover: Vec<eval::PhysicalInputEvent>,
    /// How many of them the history still takes: more refuses the rearm.
    pub room: usize,
    /// What the input thread did.
    pub outcome: RearmOutcome,
}

/// What a [`Rearm`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RearmOutcome {
    /// Not yet taken.
    Pending,
    /// START or SELECT is held: nothing changed.
    Held,
    /// The cancelled opening was sealed at `closed` on its own timeline,
    /// and the next opened with chart time 0 at `origin` on the clock.
    Opened {
        /// Where the next candidate's chart time 0 lies.
        origin: TimeUs,
        /// When the sealed opening closed, on its timeline.
        closed: TimeUs,
    },
    /// The sealed opening's events or arrivals did not fit their buffers,
    /// or the queues did not reopen: nothing opened, and the capture is
    /// sealed.
    Refused,
}

/// What a handoff does with the play's sources once its closing is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandoffKind {
    /// A completed play's: input goes on reaching the play's queues for
    /// the tail's feedback ([`input::SourceSet::hand_off`]).
    Feedback,
    /// An aborted play's: nothing more is pushed into the play's queues
    /// ([`input::SourceSet::seal_recording`]).
    Sealed,
}

/// A play's closing, made at its handoff ([`HandoffKind`]) in storage the
/// input thread reserved at the opening, and kept by the game thread until
/// the close that follows, so neither side allocates or frees for it
/// during the play.
#[derive(Debug, Default)]
pub struct Handoff {
    /// The closing, moved out by the game thread.
    pub closing: Closing,
}

impl Handoff {
    /// Storage for a closing of up to `arrivals` arrivals and `sources`
    /// sources.
    fn reserved(arrivals: usize, sources: usize) -> Box<Self> {
        Box::new(Self {
            closing: Closing {
                arrivals: Vec::with_capacity(arrivals),
                overflowed: false,
                source_limited: false,
                axis_out_of_range: Vec::with_capacity(sources),
            },
        })
    }

    /// Fills the closing from `sources`, within the reserved storage.
    fn fill(&mut self, sources: &SourceSet) {
        let c = &mut self.closing;
        c.arrivals.clear();
        let room = c.arrivals.capacity();
        c.arrivals
            .extend(sources.arrivals().iter().take(room).copied());
        c.overflowed = sources.overflowed();
        c.source_limited = sources.source_limited();
        c.axis_out_of_range.clear();
        let room = c.axis_out_of_range.capacity();
        c.axis_out_of_range
            .extend(sources.axis_out_of_range().take(room));
    }
}

/// What the input thread publishes between requests.
#[derive(Debug, Default)]
pub struct InputState {
    tripped: AtomicBool,
    connected: AtomicBool,
    /// The hook was dropped: the input thread ended, or is unwinding.
    gone: AtomicBool,
    /// The first handshake of the link that failed: once set, no request
    /// is sent and no reply read, so a late reply to a request that timed
    /// out is never taken for the answer to another.
    failed: std::sync::OnceLock<InputWaitError>,
    /// The hook's wakes so far (dev-tools).
    #[cfg(feature = "dev-tools")]
    wakes: std::sync::atomic::AtomicU64,
}

impl InputState {
    /// Why the input thread failed: a handshake's failure, or the hook
    /// gone. The one reading of it.
    fn failure(&self) -> Option<InputWaitError> {
        self.failed.get().copied().or_else(|| {
            self.gone
                .load(Ordering::Acquire)
                .then_some(InputWaitError::Gone)
        })
    }
}

/// The input link's failure, readable from any thread
/// ([`InputLink::failure_handle`]).
#[derive(Clone, Debug)]
pub struct InputFailure(Arc<InputState>);

impl InputFailure {
    /// As [`InputLink::failure`].
    pub fn get(&self) -> Option<InputWaitError> {
        self.0.failure()
    }
}

/// Candidate `generation`'s recording opened at this boundary: published
/// as soon as the source set opened it, before the hook builds the opening
/// and replies (design.md "Play Lifecycle", tuning while loading).
fn published_opening(controls: &Controls, generation: u32) {
    controls.tuning().activity().set_opened(generation);
}

/// Whether an opening that accepted the window's focus-loss generation
/// `accepted` finds focus lost since: the window wrote each loss into the
/// activity record before the input thread could sample it, so a loss and
/// return this boundary already sampled still shows here (design.md "Play
/// Lifecycle", the loading state: focus).
fn focus_moved(controls: &Controls, accepted: Option<u64>) -> bool {
    accepted.is_some_and(|g| controls.tuning().focus_losses() != g)
}

/// Stops the input thread once a gated opening's generation is published,
/// before the hook builds the opening (tests): it sends `reached` and waits
/// for `resume`, at most 60 s.
#[cfg(test)]
pub struct OpenGate {
    /// Sent once the generation is published.
    pub reached: mpsc::Sender<()>,
    /// Waited for after `reached`.
    pub resume: mpsc::Receiver<()>,
}

/// The game thread's end of the input side.
pub struct InputLink {
    requests: Sender<Request>,
    replies: Receiver<Reply>,
    state: Arc<InputState>,
    /// Wakes the input thread after each request.
    waker: Waker,
    /// The most awake time a reply is waited for.
    timeout: Duration,
    /// The awake-time clock the wait is measured on.
    awake: fn() -> Duration,
    /// Rearms to the input thread, and back ([`rearm`](Self::rearm)).
    rearms: rtrb::Producer<Box<Rearm>>,
    rearmed: rtrb::Consumer<Box<Rearm>>,
    /// Handoffs asked of the input thread, and their answers
    /// ([`hand_off`](Self::hand_off), [`abort_close`](Self::abort_close)).
    handoffs: rtrb::Producer<HandoffKind>,
    handed: rtrb::Consumer<Box<Handoff>>,
    /// The last handoff's storage, freed at the close that follows.
    spent: Option<Box<Handoff>>,
}

impl InputLink {
    /// Opens recording for a play.
    ///
    /// # Errors
    ///
    /// [`InputWaitError`] if the input thread is gone or did not answer
    /// within [`INPUT_TIMEOUT`], now or at an earlier handshake.
    pub fn open(&mut self, request: Request) -> Result<Opening<SourceReader>, InputWaitError> {
        match self.open_candidate(request)? {
            Some(o) => Ok(o),
            None => unreachable!("an ungated open is never deferred"),
        }
    }

    /// Opens recording for a play, or a gated play's READY candidate:
    /// `None` when the input thread deferred it ([`Reply::Deferred`]).
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub fn open_candidate(
        &mut self,
        request: Request,
    ) -> Result<Option<Opening<SourceReader>>, InputWaitError> {
        match self.ask(request)? {
            Reply::Opened(o, lines) => {
                print_lines(&lines);
                Ok(Some(o))
            }
            Reply::Deferred => Ok(None),
            Reply::Closed(..) => unreachable!("an open is answered by an opening"),
        }
    }

    /// Hands the input thread the adjustment controls' tap: sent without
    /// waiting, as [`feed`](Self::feed). A failed link sends nothing.
    pub fn tap(&self, tap: input::AdjustmentTap) {
        if self.state.failed.get().is_none()
            && self.requests.send(Request::Tap(Box::new(tap))).is_ok()
        {
            self.waker.wake();
        }
    }

    /// Asks the input thread to answer fence `n` in the adjustment tap's
    /// stream at its next boundary: sent without waiting; the answer is
    /// the tap's marker. A failed link sends nothing.
    pub fn request_fence(&self, n: u64) {
        if self.state.failed.get().is_none() && self.requests.send(Request::Fence(n)).is_ok() {
            self.waker.wake();
        }
    }

    /// Starts feeding the play's loading script, its time 0 at `at`
    /// ([`Request::LoadingScript`]; dev-tools). A failed link sends nothing.
    #[cfg(feature = "dev-tools")]
    pub fn loading_script(&self, at: TimeUs) {
        if self.state.failed.get().is_none()
            && self.requests.send(Request::LoadingScript { at }).is_ok()
        {
            self.waker.wake();
        }
    }

    /// Stops feeding the loading script ([`Request::EndLoadingScript`];
    /// dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn end_loading_script(&self) {
        if self.state.failed.get().is_none()
            && self.requests.send(Request::EndLoadingScript).is_ok()
        {
            self.waker.wake();
        }
    }

    /// The awake time now: the clock the input thread's answers are bounded
    /// on, which stands still through a suspend.
    #[must_use]
    pub fn awake_now(&self) -> Duration {
        (self.awake)()
    }

    /// Measures the awake time with `awake` from now on (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub(crate) fn set_awake(&mut self, awake: fn() -> Duration) {
        self.awake = awake;
    }

    /// Wakes the input thread: the adjustment tap waits for room.
    pub fn wake_input(&self) {
        self.waker.wake();
    }

    /// Whether the input thread is gone.
    #[must_use]
    pub fn gone(&self) -> bool {
        self.state.gone.load(Ordering::Acquire)
    }

    /// Stops the input thread at its next fence ([`FenceGate`]; dev-tools
    /// tests). A failed link sends nothing.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn fence_gate(&self, gate: FenceGate) {
        if self.state.failed.get().is_none()
            && self
                .requests
                .send(Request::FenceGate(Box::new(gate)))
                .is_ok()
        {
            self.waker.wake();
        }
    }

    /// Stops the input thread at its next gated opening, once its
    /// generation is published ([`OpenGate`]; tests). A failed link sends
    /// nothing.
    #[cfg(test)]
    pub fn open_gate(&self, gate: OpenGate) {
        if self.state.failed.get().is_none()
            && self
                .requests
                .send(Request::OpenGate(Box::new(gate)))
                .is_ok()
        {
            self.waker.wake();
        }
    }

    /// Marks the input thread gone, as its hook's end does, so the game
    /// thread sees its failure (dev-tools: a script's input failure).
    #[cfg(feature = "dev-tools")]
    pub fn fail_for_script(&self) {
        self.state.gone.store(true, Ordering::Release);
    }

    /// [`fail_for_script`](Self::fail_for_script) from another thread
    /// (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn failer(&self) -> impl Fn() + Send + Sync + use<> {
        let state = Arc::clone(&self.state);
        move || state.gone.store(true, Ordering::Release)
    }

    /// Asks the input thread to seal the cancelled READY candidate's
    /// opening and open the next at its next boundary ([`Rearm`]), and
    /// waits for the answer as [`open`](Self::open) does. Allocates
    /// nothing: `rearm` goes through a bounded queue and comes back.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open); `rearm` and its readers are then lost with
    /// the link.
    pub fn rearm(&mut self, mut rearm: Box<Rearm>) -> Result<Box<Rearm>, InputWaitError> {
        if let Some(e) = self.state.failed.get().copied() {
            return Err(e);
        }
        rearm.outcome = RearmOutcome::Pending;
        let sent = self.rearms.push(rearm).map_err(|_| InputWaitError::Gone);
        let answer = sent.and_then(|()| {
            self.waker.wake();
            let since = (self.awake)();
            loop {
                if let Ok(r) = self.rearmed.pop() {
                    return Ok(r);
                }
                if self.state.gone.load(Ordering::Acquire) || self.rearmed.is_abandoned() {
                    return Err(InputWaitError::Gone);
                }
                if (self.awake)().saturating_sub(since) >= self.timeout {
                    return Err(InputWaitError::Timeout);
                }
                std::thread::sleep(REARM_POLL);
            }
        });
        if let Err(e) = answer {
            let _ = self.state.failed.set(e);
        }
        answer
    }

    /// Hands a completed play's recording off to its tail at the input
    /// thread's next boundary ([`input::SourceSet::hand_off`]), and waits
    /// for the closing as [`rearm`](Self::rearm) waits. Allocates nothing:
    /// the request and the answer go through bounded queues, and the
    /// answer's storage is kept until [`close`](Self::close).
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub fn hand_off(&mut self) -> Result<Closing, InputWaitError> {
        self.exchange(HandoffKind::Feedback)
    }

    /// Closes an aborted play's recording at the input thread's next
    /// boundary without ending it: the recording is sealed there
    /// ([`input::SourceSet::seal_recording`]), so nothing more reaches the
    /// play's queues, while its sources stay until [`close`](Self::close),
    /// which prints the hook's lines for the play. Waits and allocates as
    /// [`hand_off`](Self::hand_off).
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub fn abort_close(&mut self) -> Result<Closing, InputWaitError> {
        self.exchange(HandoffKind::Sealed)
    }

    /// Asks the input thread for a handoff of `kind` and waits for its
    /// closing ([`hand_off`](Self::hand_off)). A failed wait fails the
    /// link, so a late answer is never taken for another request's.
    fn exchange(&mut self, kind: HandoffKind) -> Result<Closing, InputWaitError> {
        if let Some(e) = self.state.failed.get().copied() {
            return Err(e);
        }
        let sent = self.handoffs.push(kind).map_err(|_| InputWaitError::Gone);
        let answer = sent.and_then(|()| {
            self.waker.wake();
            let since = (self.awake)();
            loop {
                if let Ok(h) = self.handed.pop() {
                    return Ok(h);
                }
                if self.state.gone.load(Ordering::Acquire) || self.handed.is_abandoned() {
                    return Err(InputWaitError::Gone);
                }
                if (self.awake)().saturating_sub(since) >= self.timeout {
                    return Err(InputWaitError::Timeout);
                }
                std::thread::sleep(REARM_POLL);
            }
        });
        match answer {
            Ok(mut h) => {
                let closing = std::mem::take(&mut h.closing);
                self.spent = Some(h);
                Ok(closing)
            }
            Err(e) => {
                let _ = self.state.failed.set(e);
                Err(e)
            }
        }
    }

    /// Closes recording, or the sources of a play handed off to its tail
    /// or sealed ([`hand_off`](Self::hand_off),
    /// [`abort_close`](Self::abort_close)), and prints the hook's lines
    /// for the play.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub fn close(&mut self) -> Result<Closing, InputWaitError> {
        self.spent = None;
        match self.ask(Request::Close)? {
            Reply::Closed(c, lines) => {
                print_lines(&lines);
                Ok(c)
            }
            Reply::Opened(..) | Reply::Deferred => unreachable!("a close is answered by a closing"),
        }
    }

    /// Hands the input thread another chart's generated input (autoplay),
    /// between plays: sent without waiting, since nothing answers it; the
    /// next open, sent after it, plays it. A failed link sends nothing.
    pub fn feed(&self, input: input::AutoplayInput, queue_capacity: usize) {
        let feed = Request::Feed {
            input,
            queue_capacity,
        };
        if self.state.failed.get().is_none() && self.requests.send(feed).is_ok() {
            self.waker.wake();
        }
    }

    /// Hands the input thread another chart's input (a development
    /// scenario), between plays: sent without waiting, as
    /// [`feed`](Self::feed). A failed link sends nothing.
    #[cfg(feature = "dev-tools")]
    pub fn switch(&self, mode: Mode, sources: input::SourceConfig) {
        let switch = Request::Switch {
            mode: Box::new(mode),
            sources,
        };
        if self.state.failed.get().is_none() && self.requests.send(switch).is_ok() {
            self.waker.wake();
        }
    }

    /// Lets the input thread go idle: sent without waiting, since nothing
    /// answers it. A failed link sends nothing.
    pub fn idle(&self) {
        if self.state.failed.get().is_none() && self.requests.send(Request::Idle).is_ok() {
            self.waker.wake();
        }
    }

    /// Sends `request` and waits for its reply. Every request is answered
    /// once, in order, so with the link failed at the first wait that does
    /// not end in a reply, the reply read is always this request's.
    fn ask(&mut self, request: Request) -> Result<Reply, InputWaitError> {
        if let Some(e) = self.state.failed.get().copied() {
            return Err(e);
        }
        let reply = self.wait(request);
        if let Err(e) = reply {
            let _ = self.state.failed.set(e);
        }
        reply
    }

    fn wait(&self, request: Request) -> Result<Reply, InputWaitError> {
        self.requests
            .send(request)
            .map_err(|_| InputWaitError::Gone)?;
        // After the send: the wake that follows finds the request.
        self.waker.wake();
        let since = (self.awake)();
        loop {
            match self.replies.recv_timeout(WAIT_SLICE) {
                Ok(reply) => return Ok(reply),
                Err(RecvTimeoutError::Disconnected) => return Err(InputWaitError::Gone),
                Err(RecvTimeoutError::Timeout) => {
                    if (self.awake)().saturating_sub(since) >= self.timeout {
                        return Err(InputWaitError::Timeout);
                    }
                }
            }
        }
    }

    /// Why the input thread failed, once known: a handshake's failure, or
    /// the hook gone (the thread ended or panicked). One atomic load.
    pub fn failure(&self) -> Option<InputWaitError> {
        self.state.failure()
    }

    /// The link's failure as a handle read from any thread
    /// ([`crate::failures::Failures`]).
    pub fn failure_handle(&self) -> InputFailure {
        InputFailure(Arc::clone(&self.state))
    }

    /// The input thread's wakes so far: what its cadence costs while
    /// nothing plays (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn wakes(&self) -> u64 {
        self.state.wakes.load(Ordering::Relaxed)
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

/// Prints the hook's lines for a reply, on the game thread.
fn print_lines(lines: &[String]) {
    for line in lines {
        eprintln!("{line}");
    }
}

/// How the input thread maps its batch times onto a play.
pub enum Mode {
    /// Raw Input: batches carry the clock's time, and recording opens with
    /// the play's origin, so the source set moves them onto the play.
    Clock,
    /// Autoplay (design.md "Autoplay"): the generated input on the play's
    /// timeline, fed at every wake with the play's time, on the one source
    /// set the clocked input thread keeps for the session. Each play feeds
    /// it from its first event on the generated keyboard connected again,
    /// and no other device takes part.
    Autoplay(input::AutoplayInput),
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
    /// The keys held while the play loads, before its first opening
    /// ([`Request::LoadingScript`]), if any.
    pub loading: Option<input::ScriptedInput>,
    /// How long after the release the script is held at a rearm is
    /// answered at the earliest (`--dev-rearm-delay`): a late game thread,
    /// made on purpose.
    pub rearm_delay: TimeUs,
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
    /// The input thread's phase.
    phase: Phase,
    /// The ingress word the last boundary saw: a new generation masks the
    /// panels held across it.
    ingress_seen: input::Ingress,
    /// A script's timeline: the first opening's chart time 0 on the
    /// clock, which a rearm moves by the wait between the release the
    /// script is held at and the rearm (dev-tools).
    script_origin: Option<TimeUs>,
    /// A scripted release (dev-tools): the script time of the batch where
    /// a cancelled READY candidate's hold read released, which the script
    /// is fed no further than until the rearm opens there, and the clock
    /// time it was seen at.
    #[cfg(feature = "dev-tools")]
    released: Option<(TimeUs, TimeUs)>,
    /// Rearms from the game thread, and back.
    rearms: rtrb::Consumer<Box<Rearm>>,
    rearmed: rtrb::Producer<Box<Rearm>>,
    /// Handoffs from the game thread, and their answers.
    handoffs: rtrb::Consumer<HandoffKind>,
    handed: rtrb::Producer<Box<Handoff>>,
    /// The open recording's handoff storage, reserved at its opening.
    handoff: Option<Box<Handoff>>,
    /// The loading script being fed, with its time 0 on the clock
    /// (dev-tools).
    #[cfg(feature = "dev-tools")]
    loading: Option<(input::ScriptedInput, TimeUs)>,
    /// The gate at the next fence (dev-tools tests).
    #[cfg(all(test, feature = "dev-tools"))]
    fence_gate: Option<Box<FenceGate>>,
    /// The gate at the next gated opening (tests).
    #[cfg(test)]
    open_gate: Option<Box<OpenGate>>,
}

/// A linked hook and link; the link wakes the input thread through
/// `waker`.
pub fn link(controls: Arc<Controls>, mode: Mode, waker: Waker) -> (Hook, InputLink) {
    let (req_tx, req_rx) = mpsc::channel();
    let (rep_tx, rep_rx) = mpsc::channel();
    let state = Arc::new(InputState::default());
    // One rearm is in flight at a time.
    let (rearm_tx, rearm_rx) = rtrb::RingBuffer::new(1);
    let (rearmed_tx, rearmed_rx) = rtrb::RingBuffer::new(1);
    // One handoff is in flight at a time.
    let (handoff_tx, handoff_rx) = rtrb::RingBuffer::new(1);
    let (handed_tx, handed_rx) = rtrb::RingBuffer::new(1);
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
            phase: Phase::Tail,
            ingress_seen: input::Ingress::INITIAL,
            script_origin: None,
            #[cfg(feature = "dev-tools")]
            released: None,
            rearms: rearm_rx,
            rearmed: rearmed_tx,
            handoffs: handoff_rx,
            handed: handed_tx,
            handoff: None,
            #[cfg(feature = "dev-tools")]
            loading: None,
            #[cfg(all(test, feature = "dev-tools"))]
            fence_gate: None,
            #[cfg(test)]
            open_gate: None,
        },
        InputLink {
            requests: req_tx,
            replies: rep_rx,
            state,
            waker,
            timeout: INPUT_TIMEOUT,
            awake: awake_time,
            rearms: rearm_tx,
            rearmed: rearmed_rx,
            handoffs: handoff_tx,
            handed: handed_rx,
            spent: None,
        },
    )
}

impl Drop for Hook {
    /// Runs when the input thread ends, and while it unwinds from a panic.
    fn drop(&mut self) {
        self.state.gone.store(true, Ordering::Release);
    }
}

impl Hook {
    /// One boundary, `now` being the clock read there; returns the input
    /// thread's phase from then on. Generated input or a script is first
    /// fed up to the play's time; between plays, autoplay's wake still
    /// checks the focus gate, so a focus loss during the audio tail or on
    /// the end screen is published.
    pub fn wake(&mut self, sources: &mut SourceSet, now: TimeUs) -> Phase {
        #[cfg(feature = "dev-tools")]
        self.state.wakes.fetch_add(1, Ordering::Relaxed);
        match (&mut self.mode, self.origin) {
            (Mode::Autoplay(feeder), Some(origin)) => {
                input::InputAdapter::poll(feeder, TimeUs(now.0 - origin.0), sources);
            }
            (Mode::Autoplay(_), None) => sources.end_batch(now),
            // A script runs on its own timeline, held at a release until the
            // rearm opens there; before its first opening a loading script
            // runs on its own.
            #[cfg(feature = "dev-tools")]
            (Mode::Script(script), origin) => {
                if let Some((loading, at)) = &mut self.loading {
                    sources.set_presentation_offset(*at);
                    input::InputAdapter::poll(loading, TimeUs(now.0 - at.0), sources);
                } else if let Some((released, _)) = self.released {
                    input::InputAdapter::poll(&mut script.current, released, sources);
                } else if let Some(origin) = self.script_origin.or(origin)
                    && let Some(released) =
                        feed_to_release(&mut script.current, TimeUs(now.0 - origin.0), sources)
                {
                    self.released = Some((released, now));
                }
            }
            _ => {}
        }
        while let Ok(request) = self.requests.try_recv() {
            let reply = match request {
                Request::Open {
                    lead_in,
                    logic,
                    recorded,
                    gate,
                    focus,
                } => match self.open(sources, now, lead_in, logic, &recorded, (gate, focus)) {
                    Some((opening, lines)) => {
                        self.phase = Phase::Open;
                        Reply::Opened(opening, lines)
                    }
                    None => Reply::Deferred,
                },
                Request::Tap(mut tap) => {
                    sources.set_presentation_focus(self.controls.presentation_focus().clone());
                    tap.set_activity(Arc::clone(self.controls.tuning().activity()));
                    sources.set_adjustment(*tap);
                    continue;
                }
                #[cfg(feature = "dev-tools")]
                Request::LoadingScript { at } => {
                    if let Mode::Script(script) = &self.mode
                        && let Some(loading) = &script.loading
                        && self.origin.is_none()
                    {
                        self.loading = Some((loading.clone(), at));
                    }
                    continue;
                }
                #[cfg(feature = "dev-tools")]
                Request::EndLoadingScript => {
                    self.loading = None;
                    sources.set_wake_at(None);
                    continue;
                }
                Request::Fence(n) => {
                    #[cfg(all(test, feature = "dev-tools"))]
                    let gate = self.fence_gate.take().map(|gate| {
                        let FenceGate {
                            before,
                            reached,
                            resume,
                        } = *gate;
                        before(sources);
                        (reached, resume)
                    });
                    // The panels' level before the fence's answer: a game
                    // thread the answer lets go reads the level as of it.
                    if self.publish_panels(sources) {
                        self.controls.wake_game();
                    }
                    if let Some(tap) = sources.adjustment_mut() {
                        tap.fence(n, now);
                    }
                    #[cfg(all(test, feature = "dev-tools"))]
                    if let Some((reached, resume)) = gate {
                        let _ = reached.send(());
                        let _ = resume.recv_timeout(Duration::from_secs(60));
                    }
                    continue;
                }
                #[cfg(all(test, feature = "dev-tools"))]
                Request::FenceGate(gate) => {
                    self.fence_gate = Some(gate);
                    continue;
                }
                #[cfg(test)]
                Request::OpenGate(gate) => {
                    self.open_gate = Some(gate);
                    continue;
                }
                Request::Close => {
                    let (closing, lines) = self.close(sources);
                    self.phase = Phase::Tail;
                    Reply::Closed(closing, lines)
                }
                Request::Idle => {
                    if self.phase != Phase::Open {
                        self.phase = Phase::Idle;
                    }
                    continue;
                }
                Request::Feed {
                    input,
                    queue_capacity,
                } => {
                    if let Mode::Autoplay(feeder) = &mut self.mode {
                        *feeder = input;
                        sources.set_queue_capacity(queue_capacity);
                    }
                    continue;
                }
                // Sent only between plays, after the last close was
                // answered; with recording open, or a set that cannot be
                // made, the old mode and set stay.
                #[cfg(feature = "dev-tools")]
                Request::Switch {
                    mode,
                    sources: config,
                } => {
                    if self.origin.is_none() && sources.reset(config).is_ok() {
                        self.mode = *mode;
                    }
                    continue;
                }
            };
            // Published before the reply, so the game thread never reads
            // the previous play's state after it.
            self.publish(sources);
            // The game thread waits for the reply; if it is gone, so is
            // the play.
            let _ = self.replies.send(reply);
        }
        // A rearm, at this boundary: one in flight at a time, so the
        // answer always has room.
        if self.rearm_due(now)
            && let Ok(mut rearm) = self.rearms.pop()
        {
            self.rearm(sources, now, &mut rearm);
            let _ = self.rearmed.push(rearm);
        }
        // A handoff, at this boundary: one in flight at a time, so the
        // answer always has room. Only an opening reserved its storage; a
        // handoff without one answers an empty closing.
        if let Ok(kind) = self.handoffs.pop() {
            let mut handoff = self.handoff.take().unwrap_or_default();
            handoff.fill(sources);
            match kind {
                HandoffKind::Feedback => sources.hand_off(),
                HandoffKind::Sealed => sources.seal_recording(),
            }
            let _ = self.handed.push(handoff);
        }
        // A loading script wakes the thread for its next item, Idle or not.
        #[cfg(feature = "dev-tools")]
        if let Some((loading, at)) = &self.loading {
            sources.set_wake_at(loading.next_at().map(|t| TimeUs(t.0.saturating_add(at.0))));
        }
        self.publish(sources);
        if self.origin.is_some()
            && let Some(logic) = &self.logic
        {
            logic.unpark();
        }
        self.menu(sources);
        self.phase
    }

    /// The controller's menu: active from here to the next boundary only
    /// while the thread is Idle and the window has focus, so a request
    /// that opens recording or a focus change ends it at the boundary it
    /// is seen at. The game thread is woken when this batch sent events.
    /// START and SELECT are published as the panels' level by the same
    /// rule: held only while Idle with focus, so a focus loss, leaving
    /// Idle or a device's removal (which drops what it held) closes a
    /// panel at the boundary it is seen at; the game thread is woken when
    /// the level changed. The level is published before that wake and a
    /// fence's answer, so a game thread they let go reads it.
    ///
    /// While the decision screen's epoch is open (design.md "Play
    /// Lifecycle", the decision screen) neither is: the menu source only
    /// once Back is let through past the screen's time, and a START or
    /// SELECT held across a new ingress generation opens no panel until
    /// released.
    fn menu(&mut self, sources: &mut SourceSet) {
        let mut wake = self.publish_panels(sources);
        let active = self.phase == Phase::Idle
            && self.controls.focused()
            && (self.ingress_seen.is_open() || self.controls.escaping());
        if let Some(menu) = sources.menu_mut() {
            menu.set_active(active);
            wake |= menu.take_sent();
        }
        if wake {
            self.controls.wake_game();
        }
    }

    /// Publishes the panels' level as [`menu`](Self::menu) says; `true`
    /// when it changed. The one point every panel level and every fence's
    /// answer is published after: a new ingress generation seen here first
    /// masks the START and SELECT held across it, so neither a level nor a
    /// fence answer shows a panel held through the decision screen.
    fn publish_panels(&mut self, sources: &mut SourceSet) -> bool {
        let word = self.controls.ingress();
        if word != self.ingress_seen {
            self.ingress_seen = word;
            if let Some(tap) = sources.adjustment_mut() {
                tap.mask_held_panels();
            }
        }
        let active = self.phase == Phase::Idle && self.controls.focused() && word.is_open();
        let (_, losses) = self.controls.presentation_focus().read();
        sources
            .adjustment_mut()
            .is_some_and(|tap| tap.publish_panels(active, losses))
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
        (gate, focus): (Option<u32>, Option<u64>),
    ) -> Option<(Opening<SourceReader>, Vec<String>)> {
        let origin = TimeUs(now.0 + lead_in.0);
        let mut lines = Vec::new();
        let start = match &mut self.mode {
            Mode::Clock => {
                if !take_part(sources, recorded) {
                    lines.push(
                        "input: the controller set up in `tapestry keys` is not connected; this play uses the keyboard bindings only".to_owned()
                    );
                }
                reset_gate(sources, &self.controls);
                let start = match gate {
                    None => sources.open_play(now, origin),
                    Some(generation) => {
                        if focus_moved(&self.controls, focus) {
                            return None;
                        }
                        let start = candidate(sources.open_candidate(now, origin, generation))?;
                        published_opening(&self.controls, generation);
                        start
                    }
                };
                // Devices the source limit left out: keyboards, when any
                // is (controllers come first), possibly one with lanes.
                let n = sources.left_out();
                if n > 0 {
                    lines.push(format!(
                        "input: {n} connected device(s) take no part in this play: more devices than sources; a keyboard left out records nothing, and the log is marked incomplete"
                    ));
                }
                start
            }
            Mode::Autoplay(feeder) => {
                feeder.restart(sources, recorded.keyboard.clone());
                // No device but the generated keyboard plays, whatever the
                // set has seen.
                let others: Vec<DeviceId> = sources
                    .devices()
                    .map(|(d, _)| d)
                    .filter(|&d| d != input::AUTOPLAY_DEVICE)
                    .collect();
                for d in others {
                    sources.set_takes_part(d, false);
                }
                reset_gate(sources, &self.controls);
                // The feeder's batches are on the play's time: the tap
                // stamps them on the clock from the opening's origin.
                sources.set_presentation_offset(origin);
                match gate {
                    None => sources.open_recording(TimeUs(-lead_in.0)),
                    Some(generation) => {
                        if focus_moved(&self.controls, focus) {
                            return None;
                        }
                        let start = candidate(sources.open_candidate(
                            TimeUs(-lead_in.0),
                            TimeUs(0),
                            generation,
                        ))?;
                        published_opening(&self.controls, generation);
                        start
                    }
                }
            }
            #[cfg(feature = "dev-tools")]
            Mode::Script(script) => {
                // A gated opening deferred before keeps the script's
                // timeline and devices; otherwise the log starts again
                // from its start, on devices as they were before it: they
                // connect, with their recorded controls, before the
                // opening.
                // A loading script's devices are the play's: the play's
                // script takes over on the same set, its held state kept.
                let loading = self.loading.is_some();
                self.released = None;
                let script_origin = match self.script_origin.filter(|_| gate.is_some()) {
                    Some(o) => o,
                    None if loading => {
                        script.current = script.log.clone();
                        origin
                    }
                    None => {
                        script.current = script.log.clone();
                        // The same gate: the latch the controls hold trips it.
                        #[expect(
                            clippy::expect_used,
                            reason = "the run's source set was made with these sizes"
                        )]
                        sources
                            .reset(script.sources.clone())
                            .expect("the run's source set was made with these sizes");
                        origin
                    }
                };
                let open_at = TimeUs(now.0 - script_origin.0);
                input::InputAdapter::poll(&mut script.current, TimeUs(open_at.0 - 1), sources);
                reset_gate(sources, &self.controls);
                self.script_origin = Some(script_origin);
                // The script's batches are on its timeline: the tap stamps
                // them on the clock from its origin.
                sources.set_presentation_offset(script_origin);
                match gate {
                    None => sources.open_recording(open_at),
                    Some(generation) => {
                        let chart0 = TimeUs(open_at.0 + lead_in.0);
                        if focus_moved(&self.controls, focus) {
                            return None;
                        }
                        let start = candidate(sources.open_candidate(open_at, chart0, generation))?;
                        published_opening(&self.controls, generation);
                        // The first successful opening ends the loading
                        // script; a deferred one keeps feeding it, so the
                        // release it waits for arrives.
                        self.loading = None;
                        sources.set_wake_at(None);
                        start
                    }
                }
            }
        };
        #[cfg(test)]
        if gate.is_some()
            && let Some(g) = self.open_gate.take()
        {
            let _ = g.reached.send(());
            let _ = g.resume.recv_timeout(Duration::from_secs(60));
        }
        self.origin = Some(origin);
        self.logic = Some(logic);
        self.handoff = Some(Handoff::reserved(
            sources.max_arrivals(),
            sources.participants().count(),
        ));
        self.participants = sources
            .participants()
            .map(|(d, _)| (d, sources.connection(d)))
            .collect();
        let opening = Opening {
            origin,
            start,
            readers: sources.take_readers(),
            kinds: sources.participants().map(|(_, k)| k).collect(),
            axes: sources
                .participants()
                .map(|(d, _)| sources.axes(d).to_vec())
                .collect(),
        };
        Some((opening, lines))
    }

    /// Seals the cancelled READY candidate's opening and opens the next on
    /// the attempt's source table at this boundary (design.md "Play
    /// Lifecycle", the start gate), only where START and SELECT are
    /// verifiably released: the readers come back drained of what was
    /// pushed since the game thread's last drain, the sealed opening's
    /// arrivals and closing time, and the next opening's snapshot, all
    /// within the buffers `rearm` carries. Allocates nothing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn rearm(&mut self, sources: &mut SourceSet, now: TimeUs, rearm: &mut Rearm) {
        rearm.leftover.clear();
        // A scripted release is taken here: a rearm that does not open
        // lets the script go on from it.
        #[cfg(feature = "dev-tools")]
        let released = self.released.take().map(|(at, _)| at);
        #[cfg(not(feature = "dev-tools"))]
        let released: Option<TimeUs> = None;
        let Some(old) = self.origin else {
            rearm.outcome = RearmOutcome::Refused;
            return;
        };
        if sources.holding() {
            rearm.outcome = RearmOutcome::Held;
            return;
        }
        // Everything the sealed opening still holds, in source order.
        let mut fits = true;
        for reader in &mut rearm.readers {
            while let Some(ev) = reader.pop() {
                if rearm.leftover.len() < rearm.leftover.capacity().min(rearm.room) {
                    rearm.leftover.push(ev);
                } else {
                    fits = false;
                }
            }
        }
        let arrivals = sources.arrivals();
        rearm.arrivals.clear();
        let room = rearm.arrivals.capacity();
        rearm.arrivals.extend(arrivals.iter().take(room).copied());
        fits &= arrivals.len() <= room;
        if !fits {
            rearm.outcome = RearmOutcome::Refused;
            return;
        }
        // The input side's batch clock: the clock, or a script's timeline.
        // A script held at a release opens the rearm at the release, its
        // timeline moved by the wait since, so its later items keep their
        // chart times however late the rearm came.
        let (batch, script_origin) = match released {
            Some(at) => (at, Some(TimeUs(now.0.saturating_sub(at.0)))),
            None => (
                TimeUs(now.0 - self.script_origin.map_or(0, |o| o.0)),
                self.script_origin,
            ),
        };
        if let Some(o) = script_origin {
            sources.set_presentation_offset(o);
        }
        let chart0 = TimeUs(batch.0.saturating_add(rearm.lead_in.0));
        sources.return_readers(std::mem::take(&mut rearm.readers));
        let opened = sources.reopen_candidate(batch, chart0, rearm.generation);
        rearm.outcome = match opened {
            Ok(start) => {
                published_opening(&self.controls, rearm.generation);
                self.script_origin = script_origin;
                rearm.start.opened_at = start.opened_at;
                rearm.start.held.clear();
                let room = rearm.start.held.capacity();
                rearm
                    .start
                    .held
                    .extend(start.held.iter().take(room).copied());
                let origin = TimeUs(now.0.saturating_add(rearm.lead_in.0));
                self.origin = Some(origin);
                RearmOutcome::Opened {
                    origin,
                    closed: TimeUs(now.0.saturating_sub(old.0)),
                }
            }
            Err(input::ReopenError::Held) => RearmOutcome::Held,
            Err(_) => RearmOutcome::Refused,
        };
        // Nothing opened: the script's timeline stays where it was.
        if !matches!(rearm.outcome, RearmOutcome::Opened { .. })
            && let Some(o) = self.script_origin
        {
            sources.set_presentation_offset(o);
        }
        rearm.readers = sources.take_readers();
    }

    /// Whether a rearm may be answered at this boundary at `now`: always,
    /// but for a script held at a release, not before its rearm delay has
    /// passed since the release was seen (dev-tools).
    fn rearm_due(&self, now: TimeUs) -> bool {
        #[cfg(feature = "dev-tools")]
        if let (Mode::Script(script), Some((_, seen))) = (&self.mode, self.released) {
            return now.0 >= seen.0.saturating_add(script.rearm_delay.0);
        }
        #[cfg(not(feature = "dev-tools"))]
        let _ = (self, now);
        true
    }

    fn close(&mut self, sources: &mut SourceSet) -> (Closing, Vec<String>) {
        let closed = Closing {
            arrivals: sources.arrivals().to_vec(),
            overflowed: sources.overflowed(),
            source_limited: sources.source_limited(),
            axis_out_of_range: sources.axis_out_of_range().collect(),
        };
        // Shown once per play: the record keeps the counts.
        let lines = out_of_range_warnings(sources);
        sources.close_recording();
        self.origin = None;
        // Between plays every batch is on the clock.
        sources.set_presentation_offset(TimeUs(0));
        self.script_origin = None;
        #[cfg(feature = "dev-tools")]
        {
            self.released = None;
        }
        self.handoff = None;
        (closed, lines)
    }
}

/// Feeds `script` up to `now` on its timeline, as far as the batch where a
/// cancelled READY candidate's hold reads released (design.md "Shortening
/// Launch-Based Checks", scripted input): that batch's time, which the
/// script is then held at for the rearm to open there. While the
/// candidate can still be cancelled, or waits cancelled, the items go one
/// batch at a time, so a hold and its release are found at their own
/// times even when one wake reaches both (dev-tools).
#[cfg(feature = "dev-tools")]
fn feed_to_release(
    script: &mut input::ScriptedInput,
    now: TimeUs,
    sources: &mut SourceSet,
) -> Option<TimeUs> {
    fn phase(sources: &mut SourceSet) -> Option<input::GatePhase> {
        sources
            .adjustment_mut()
            .map(|t| t.start_gate().state().phase)
    }
    // Cancelled with both roles released: where a rearm may open.
    fn free(sources: &mut SourceSet) -> bool {
        phase(sources) == Some(input::GatePhase::Cancelled) && !sources.holding()
    }
    let mut was_free = free(sources);
    while matches!(
        phase(sources),
        Some(input::GatePhase::Armed | input::GatePhase::Cancelled)
    ) && let Some(at) = script.next_at().filter(|&at| at <= now)
    {
        input::InputAdapter::poll(script, at, sources);
        let is_free = free(sources);
        if is_free && !was_free {
            return Some(at);
        }
        was_free = is_free;
    }
    input::InputAdapter::poll(script, now, sources);
    None
}

/// A gated opening's snapshot, with the storage the input side reserved for
/// the attempt's snapshots, so the game thread copies a rearm's into it
/// without allocating; `None` when the opening was deferred.
fn candidate(start: Option<&eval::InputStart>) -> Option<eval::InputStart> {
    let start = start?;
    let mut copy = eval::InputStart {
        opened_at: start.opened_at,
        held: Vec::with_capacity(start.held.capacity()),
    };
    copy.held.extend_from_slice(&start.held);
    Some(copy)
}

/// The console lines for the open recording's axis values outside the
/// range their conversion uses: per source, one for the values counted
/// against a range `device.toml` sets and one for those counted against a
/// declared range, each only when there are any.
fn out_of_range_warnings(sources: &SourceSet) -> Vec<String> {
    let mut lines = Vec::new();
    let counts = sources
        .axis_out_of_range()
        .zip(sources.axis_out_of_set_range());
    for (i, (n, set)) in counts.enumerate() {
        if set > 0 {
            lines.push(format!(
                "input: source {i} reported {set} axis values outside the scratch range device.toml sets; correct it there or capture the scratch again in `tapestry keys`"
            ));
        }
        let declared = n - set;
        if declared > 0 {
            lines.push(format!(
                "input: source {i} reported {declared} axis values outside the range its controller declares; set its scratch range in device.toml"
            ));
        }
    }
    lines
}

/// Sets what each connected device records and whether it takes part:
/// every keyboard records the keyboard controls, and the first controller
/// of the configured model ([`Recorded::controller_device`]) the
/// controller's; any other controller, a second of that model included,
/// takes no part. The controller's bound axes to which `device.toml`
/// gives a range other than the declared one ([`ScratchConfig::range`])
/// are counted against it. Returns `false`
/// when a controller is configured and none of its model is connected.
fn take_part(sources: &mut SourceSet, recorded: &Recorded) -> bool {
    let devices: Vec<_> = sources.devices().collect();
    let mut matched = false;
    for (d, kind) in devices {
        let (list, takes_part) = match kind {
            DeviceKind::Keyboard => (&recorded.keyboard, true),
            DeviceKind::Controller => {
                let mine = !matched
                    && recorded
                        .controller_device
                        .is_some_and(|p| sources.has_product(d, p));
                matched |= mine;
                let ranges = if mine {
                    sources
                        .axes(d)
                        .iter()
                        .filter(|c| recorded.controller.contains(&c.control))
                        .map(|c| (c.control, recorded.scratch.range(c), c))
                        .filter(|&(_, range, c)| range != (!c.relative).then_some((c.min, c.max)))
                        .map(|(control, range, _)| (control, range))
                        .collect()
                } else {
                    Vec::new()
                };
                sources.set_axis_ranges(d, ranges);
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
    use game::ScratchConfig;
    use input::hid::{
        AxisCap, HidDevice, HidLayout, HidProduct, HidReport, axis_cap, axis_control,
    };
    use input::{Capture, DeviceId, DeviceKind, Packet, SourceConfig, SourceSet};

    use super::{Recorded, out_of_range_warnings, take_part};

    /// The configured controller: function 1 of a composite device.
    const CONFIGURED: HidProduct = HidProduct {
        vendor_id: 0x1A2B,
        product_id: 0x3C4D,
        interface: Some(1),
    };
    /// Function 0 of the same device: the same ids and, here, the same
    /// control numbers, another interface.
    const OTHER_FUNCTION: HidProduct = HidProduct {
        interface: Some(0),
        ..CONFIGURED
    };
    /// Another model.
    const GAMEPAD: HidProduct = HidProduct {
        vendor_id: 0x5E6F,
        product_id: 0x7081,
        interface: None,
    };
    /// Button 1 plays key 1 on the controller; retry and quit are keys.
    const BUTTON: ControlId = ControlId(1);

    fn recorded(device: Option<HidProduct>) -> Recorded {
        Recorded {
            keyboard: vec![ControlId(0x0e), ControlId(0x01)],
            controller: vec![BUTTON],
            controller_device: device,
            scratch: ScratchConfig::default(),
        }
    }

    fn set_of(max_sources: usize) -> SourceSet {
        SourceSet::new(SourceConfig {
            max_devices: 8,
            max_sources,
            max_held: 8,
            queue_capacity: 64,
            max_arrivals: 2,
        })
        .unwrap()
    }

    /// Keyboards 1 and 2, then `controllers` in order, each with its
    /// product.
    fn devices(controllers: &[(u64, HidProduct)]) -> SourceSet {
        let mut set = set_of(8);
        set.begin_batch(TimeUs(0));
        for d in [1, 2] {
            set.connect(DeviceId(d), DeviceKind::Keyboard);
        }
        for &(d, product) in controllers {
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

    /// Opens a play and presses button 1 on every device of `pressed`:
    /// the sources' devices, and those whose press was recorded.
    fn play(set: &mut SourceSet, pressed: &[u64]) -> (Vec<u64>, Vec<u64>) {
        set.open_recording(TimeUs(10));
        let mut readers = set.take_readers();
        let sources: Vec<u64> = set.participants().map(|(d, _)| d.0).collect();
        set.begin_batch(TimeUs(20));
        for &d in pressed {
            set.packet(&press(d));
        }
        set.end_batch(TimeUs(21));
        let recorded = readers
            .iter_mut()
            .zip(&sources)
            .filter_map(|(r, &d)| r.pop().map(|_| d))
            .collect();
        (sources, recorded)
    }

    /// Only the first controller of the configured model and function
    /// plays: a gamepad, the device's other function with the same ids and
    /// control numbers, and a second unit of the model take no part and
    /// record nothing, whichever order they are found in.
    #[test]
    fn only_the_configured_controller_takes_part() {
        let found: [&[(u64, HidProduct)]; 2] = [
            &[
                (10, GAMEPAD),
                (11, OTHER_FUNCTION),
                (12, CONFIGURED),
                (13, CONFIGURED),
            ],
            &[
                (13, CONFIGURED),
                (12, CONFIGURED),
                (11, OTHER_FUNCTION),
                (10, GAMEPAD),
            ],
        ];
        for (controllers, first) in found.into_iter().zip([12, 13]) {
            let mut set = devices(controllers);
            assert!(take_part(&mut set, &recorded(Some(CONFIGURED))));
            let (sources, pressed) = play(&mut set, &[10, 11, 12, 13]);
            assert_eq!(sources, [first, 1, 2]);
            assert_eq!(pressed, [first]);
            assert!(!set.source_limited());
        }
    }

    /// With the configured controller not connected, or no controller
    /// configured, only keyboards take part; only the first case is
    /// reported as missing.
    #[test]
    fn without_the_configured_controller_only_keyboards_take_part() {
        let controllers = [(10, GAMEPAD), (11, OTHER_FUNCTION)];
        for (device, found) in [(Some(CONFIGURED), false), (None, true)] {
            let mut set = devices(&controllers);
            assert_eq!(take_part(&mut set, &recorded(device)), found);
            let (sources, pressed) = play(&mut set, &[10, 11]);
            assert_eq!(sources, [1, 2]);
            assert!(pressed.is_empty());
        }
    }

    /// At the source limit the configured controller takes part and the
    /// keyboards beyond it are counted as left out, which the console
    /// reports before the play; nothing is left out below the limit.
    #[test]
    fn devices_left_out_at_the_limit_are_counted() {
        let mut set = set_of(2);
        set.begin_batch(TimeUs(0));
        for d in [1, 2, 3] {
            set.connect(DeviceId(d), DeviceKind::Keyboard);
        }
        set.connect(DeviceId(10), DeviceKind::Controller);
        set.set_product(DeviceId(10), CONFIGURED);
        assert!(take_part(&mut set, &recorded(Some(CONFIGURED))));
        set.open_recording(TimeUs(1));
        assert_eq!(set.left_out(), 2);
        let sources: Vec<u64> = set.participants().map(|(d, _)| d.0).collect();
        assert_eq!(sources, [10, 1]);
        let mut set = devices(&[(10, CONFIGURED)]);
        assert!(take_part(&mut set, &recorded(Some(CONFIGURED))));
        set.open_recording(TimeUs(1));
        assert_eq!(set.left_out(), 0);
    }

    const X: ControlId = axis_control(0x01, 0x30);
    const Y: ControlId = axis_control(0x01, 0x31);

    /// A play on the configured controller, with axes `caps`, X bound to a
    /// lane, and `scratch` from `device.toml`: each of `reports` (a value
    /// per axis of `caps`) goes through the controller's HID reader.
    /// Returns each source's count (the controller, then keyboards 1 and
    /// 2) and the warnings closing prints.
    fn hid_play(
        caps: &[AxisCap],
        scratch: ScratchConfig,
        reports: &[&[Option<i32>]],
    ) -> (Vec<u64>, Vec<String>) {
        let mut set = devices(&[(10, CONFIGURED)]);
        set.add_axes(DeviceId(10), caps);
        let mut recorded = recorded(Some(CONFIGURED));
        recorded.controller.push(X);
        recorded.scratch = scratch;
        assert!(take_part(&mut set, &recorded));
        set.open_recording(TimeUs(1));
        let layout = HidLayout {
            collection: 0,
            axes: caps,
            buttons: &[],
        };
        let mut pad = HidDevice::new(DeviceId(10), &layout);
        let capture = Capture::new();
        assert!(set.begin_batch(TimeUs(2)));
        for &axes in reports {
            let report = HidReport {
                id: 0,
                buttons: None,
                axes,
            };
            pad.report(&report, false, &capture, &mut set);
        }
        set.end_batch(TimeUs(3));
        (
            set.axis_out_of_range().collect(),
            out_of_range_warnings(&set),
        )
    }

    fn wrap(n: u32) -> ScratchConfig {
        ScratchConfig {
            wrap: Some(n),
            ..ScratchConfig::default()
        }
    }

    fn set_line(n: u64) -> String {
        format!(
            "input: source 0 reported {n} axis values outside the scratch range device.toml sets; correct it there or capture the scratch again in `tapestry keys`"
        )
    }

    fn declared_line(n: u64) -> String {
        format!(
            "input: source 0 reported {n} axis values outside the range its controller declares; set its scratch range in device.toml"
        )
    }

    /// The bound axis is counted against the range its conversion uses:
    /// the declared 0..=255, or the 128-value revolution `device.toml`
    /// sets (0..=127), which the warning names; none when it sets the axis
    /// relative.
    #[test]
    fn axis_values_are_counted_against_the_range_conversion_uses() {
        let caps = [axis_cap(X, true, 0, 255, 8).unwrap()];
        let reports: [&[Option<i32>]; 3] = [&[Some(200)], &[Some(255)], &[Some(100)]];
        let (counts, lines) = hid_play(&caps, ScratchConfig::default(), &reports);
        assert_eq!((counts, lines.len()), (vec![0, 0, 0], 0));
        let (counts, lines) = hid_play(&caps, wrap(128), &reports);
        assert_eq!(counts, [2, 0, 0]);
        assert_eq!(lines, [set_line(2)]);
        let outside: [&[Option<i32>]; 2] = [&[Some(-5)], &[Some(300)]];
        let (counts, lines) = hid_play(&caps, ScratchConfig::default(), &outside);
        assert_eq!(counts, [2, 0, 0]);
        assert_eq!(lines, [declared_line(2)]);
        let relative = ScratchConfig {
            relative: Some(true),
            ..ScratchConfig::default()
        };
        let (counts, lines) = hid_play(&caps, relative, &outside);
        assert_eq!((counts, lines.len()), (vec![0, 0, 0], 0));
    }

    /// The warning names the range each value fell outside: with X's
    /// revolution set, an unbound Y declaring 0..=99 is still checked
    /// against its declaration, and so is X when `device.toml` sets it
    /// absolute without changing its range.
    #[test]
    fn the_warning_names_the_range_the_values_fell_outside() {
        let caps = [
            axis_cap(X, true, 0, 127, 8).unwrap(),
            axis_cap(Y, true, 0, 99, 8).unwrap(),
        ];
        let reports: [&[Option<i32>]; 2] = [&[Some(200), Some(100)], &[Some(201), Some(5)]];
        let (counts, lines) = hid_play(&caps, wrap(256), &reports);
        assert_eq!(counts, [1, 0, 0]);
        assert_eq!(lines, [declared_line(1)]);
        let (counts, lines) = hid_play(&caps, wrap(64), &reports);
        assert_eq!(counts, [3, 0, 0]);
        assert_eq!(lines, [set_line(2), declared_line(1)]);
        let absolute = ScratchConfig {
            relative: Some(false),
            ..ScratchConfig::default()
        };
        let (counts, lines) = hid_play(&caps, absolute, &reports);
        assert_eq!(counts, [3, 0, 0]);
        assert_eq!(lines, [declared_line(3)]);
    }

    /// A signed 8-bit position field its descriptor declares relative,
    /// set absolute with 256 values in `device.toml`: its revolution is
    /// -128..=127, so valid negative positions are not counted, and values
    /// beyond it are.
    #[test]
    fn a_signed_field_declared_relative_keeps_its_signedness() {
        let caps = [axis_cap(X, false, -127, 127, 8).unwrap()];
        let set = ScratchConfig {
            relative: Some(false),
            ..wrap(256)
        };
        let inside: [&[Option<i32>]; 4] = [&[Some(-100)], &[Some(-128)], &[Some(127)], &[Some(5)]];
        let (counts, lines) = hid_play(&caps, set, &inside);
        assert_eq!((counts, lines.len()), (vec![0, 0, 0], 0));
        let (counts, lines) = hid_play(&caps, set, &[&[Some(-129)], &[Some(128)]]);
        assert_eq!(counts, [2, 0, 0]);
        assert_eq!(lines, [set_line(2)]);
    }
}

#[cfg(test)]
mod autoplay_tests {
    use std::sync::Arc;

    use bms_model::TimeUs;
    use eval::{ControlId, InputKind, PhysicalInputEvent, SourceId};
    use game::ScratchConfig;
    use input::{
        AUTOPLAY_DEVICE, AutoplayInput, DeviceId, DeviceKind, Packet, QpcClock, SourceConfig,
        SourceSet, Waker,
    };

    use super::{Hook, InputLink, Mode, Recorded, Reply, Request, link};
    use crate::controls::Controls;

    const MS: i64 = 1_000;
    pub(super) const LEAD_IN: TimeUs = TimeUs(1_000 * MS);
    /// Lane 1's key in the built-in key config.
    const Z: ControlId = ControlId(0x2c);

    /// Z pressed at 0.5 s and released at 0.55 s, on the play's timeline.
    fn events() -> Vec<PhysicalInputEvent> {
        [(500, 1), (550, 0)]
            .map(|(ms, value)| PhysicalInputEvent {
                received: TimeUs(ms * MS),
                source: SourceId(0),
                kind: InputKind::Control { control: Z, value },
            })
            .to_vec()
    }

    pub(super) fn recorded() -> Recorded {
        Recorded {
            keyboard: vec![Z],
            controller: vec![ControlId(1)],
            controller_device: None,
            scratch: ScratchConfig::default(),
        }
    }

    pub(super) fn set() -> SourceSet {
        SourceSet::new(SourceConfig {
            max_devices: 4,
            max_sources: 4,
            max_held: 4,
            queue_capacity: 8,
            max_arrivals: 2,
        })
        .unwrap()
    }

    /// The hook of an autoplay session and its link, waking through
    /// `waker`, with the window's controls holding the source set's latch
    /// before any play opens (and not the waker).
    pub(super) fn hooked_with(
        sources: &SourceSet,
        waker: Waker,
    ) -> (Arc<Controls>, Hook, InputLink) {
        let controls = Arc::new(Controls::new(QpcClock::start()));
        controls.set_latch(sources.gate().handle());
        let (hook, link) = link(
            Arc::clone(&controls),
            Mode::Autoplay(AutoplayInput::new(events())),
            waker,
        );
        (controls, hook, link)
    }

    /// As [`hooked_with`], with a waker of its own.
    pub(super) fn hooked(sources: &SourceSet) -> (Arc<Controls>, Hook, InputLink) {
        hooked_with(sources, Waker::new().unwrap())
    }

    /// Opens recording at clock time `at` through the request channel, as
    /// the game thread does.
    fn open(
        hook: &mut Hook,
        link: &InputLink,
        sources: &mut SourceSet,
        at: i64,
    ) -> game::Opening<input::SourceReader> {
        link.requests
            .send(Request::Open {
                lead_in: LEAD_IN,
                logic: std::thread::current(),
                recorded: recorded(),
                gate: None,
                focus: None,
            })
            .unwrap();
        hook.wake(sources, TimeUs(at));
        match link.replies.try_recv().unwrap() {
            Reply::Opened(o, _) => o,
            Reply::Closed(..) | Reply::Deferred => panic!("an open is answered by an opening"),
        }
    }

    fn close(hook: &mut Hook, link: &InputLink, sources: &mut SourceSet, at: i64) {
        link.requests.send(Request::Close).unwrap();
        hook.wake(sources, TimeUs(at));
        assert!(matches!(link.replies.try_recv(), Ok(Reply::Closed(..))));
    }

    /// P1: the latch the controls got before the first play still trips
    /// the gate after a retry: a focus loss in play is published on the
    /// first play and on the next.
    #[test]
    fn autoplay_hook_keeps_the_focus_latch_across_plays() {
        let mut sources = set();
        let (controls, mut hook, link) = hooked(&sources);
        for play in 0..2_i64 {
            let t = play * 10_000 * MS;
            controls.focus(true);
            let _opening = open(&mut hook, &link, &mut sources, t);
            hook.wake(&mut sources, TimeUs(t + MS));
            assert!(!link.tripped(), "play {play}");
            controls.focus(false);
            hook.wake(&mut sources, TimeUs(t + 2 * MS));
            assert!(link.tripped(), "play {play}");
            close(&mut hook, &link, &mut sources, t + 3 * MS);
        }
    }

    /// A3: with recording closed (the audio tail, the end screen), every
    /// wake still checks the gate: a focus loss then is published, on the
    /// first play and after a retry.
    #[test]
    fn autoplay_hook_sees_a_focus_loss_after_recording_closes() {
        let mut sources = set();
        let (controls, mut hook, link) = hooked(&sources);
        for play in 0..2_i64 {
            let t = play * 10_000 * MS;
            controls.focus(true);
            let _opening = open(&mut hook, &link, &mut sources, t);
            hook.wake(&mut sources, TimeUs(t + 2_000 * MS));
            close(&mut hook, &link, &mut sources, t + 2_001 * MS);
            assert!(!link.tripped(), "play {play}");
            controls.focus(false);
            hook.wake(&mut sources, TimeUs(t + 2_002 * MS));
            assert!(link.tripped(), "play {play}");
        }
    }

    /// A play that opens while the window has no focus is latched at once,
    /// and the next one, opened focused, plays.
    #[test]
    fn autoplay_hook_latches_a_play_opened_without_focus() {
        let mut sources = set();
        let (controls, mut hook, link) = hooked(&sources);
        controls.focus(false);
        let _opening = open(&mut hook, &link, &mut sources, 0);
        hook.wake(&mut sources, TimeUs(MS));
        assert!(link.tripped());
        close(&mut hook, &link, &mut sources, 2 * MS);
        controls.focus(true);
        let _opening = open(&mut hook, &link, &mut sources, 3 * MS);
        hook.wake(&mut sources, TimeUs(4 * MS));
        assert!(!link.tripped());
    }

    /// P2: a keyboard and a controller the set knew before the opening
    /// take no part; only the generated keyboard plays, and a press on the
    /// other keyboard is not recorded.
    #[test]
    fn autoplay_hook_takes_only_the_generated_source() {
        let mut sources = set();
        sources.begin_batch(TimeUs(0));
        sources.connect(DeviceId(1), DeviceKind::Keyboard);
        sources.set_recorded(DeviceId(1), vec![Z]);
        sources.connect(DeviceId(10), DeviceKind::Controller);
        sources.set_recorded(DeviceId(10), vec![ControlId(1)]);
        sources.end_batch(TimeUs(0));
        let (_controls, mut hook, link) = hooked(&sources);
        let mut opening = open(&mut hook, &link, &mut sources, 0);
        assert_eq!(opening.kinds, [DeviceKind::Keyboard]);
        let participants: Vec<_> = sources.participants().map(|(d, _)| d).collect();
        assert_eq!(participants, [AUTOPLAY_DEVICE]);
        // A press on the other keyboard during the play.
        sources.begin_batch(TimeUs(200 * MS));
        sources.packet(&Packet {
            device: DeviceId(1),
            control: Z,
            value: 1,
            background: false,
        });
        sources.end_batch(TimeUs(200 * MS));
        hook.wake(&mut sources, TimeUs(1_600 * MS));
        let reader = &mut opening.readers[0];
        let mut got = Vec::new();
        while let Some(e) = reader.pop() {
            got.push(e.received);
        }
        assert_eq!(got, [TimeUs(500 * MS), TimeUs(550 * MS)]);
    }

    /// The set's sizes for a switch.
    #[cfg(feature = "dev-tools")]
    fn switched() -> SourceConfig {
        SourceConfig {
            max_devices: 4,
            max_sources: 4,
            max_held: 4,
            queue_capacity: 8,
            max_arrivals: 2,
        }
    }

    /// A set whose every device slot holds a keyboard, as a log of many
    /// sources leaves it.
    #[cfg(feature = "dev-tools")]
    fn full_set() -> SourceSet {
        let mut sources = set();
        sources.begin_batch(TimeUs(0));
        for d in 1..=4 {
            sources.connect(DeviceId(d), DeviceKind::Keyboard);
        }
        sources.end_batch(TimeUs(0));
        sources
    }

    /// A switch between plays starts the input thread over: the devices a
    /// log left in every slot go, so the generated keyboard of the next
    /// chart takes part, and the latch the controls took before still
    /// trips the gate.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_switch_starts_over_and_keeps_the_latch() {
        let mut sources = full_set();
        let (controls, mut hook, link) = hooked(&sources);
        link.switch(Mode::Autoplay(AutoplayInput::new(events())), switched());
        hook.wake(&mut sources, TimeUs(0));
        assert_eq!(sources.devices().count(), 0);
        let opening = open(&mut hook, &link, &mut sources, MS);
        assert_eq!(opening.kinds, [DeviceKind::Keyboard]);
        let participants: Vec<_> = sources.participants().map(|(d, _)| d).collect();
        assert_eq!(participants, [AUTOPLAY_DEVICE]);
        controls.focus(false);
        hook.wake(&mut sources, TimeUs(2 * MS));
        assert!(link.tripped());
    }

    /// Without the switch, the generated keyboard finds no slot: nothing
    /// takes part.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn without_a_switch_a_full_set_leaves_autoplay_out() {
        let mut sources = full_set();
        let (_controls, mut hook, link) = hooked(&sources);
        let opening = open(&mut hook, &link, &mut sources, MS);
        assert!(opening.kinds.is_empty());
    }

    /// A switch that arrives with recording open is ignored: the open
    /// play keeps its devices and mode.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_switch_while_recording_is_ignored() {
        let mut sources = set();
        let (_controls, mut hook, link) = hooked(&sources);
        let _opening = open(&mut hook, &link, &mut sources, 0);
        link.switch(Mode::Autoplay(AutoplayInput::new(Vec::new())), switched());
        hook.wake(&mut sources, TimeUs(MS));
        assert_eq!(
            sources.participants().map(|(d, _)| d).collect::<Vec<_>>(),
            [AUTOPLAY_DEVICE]
        );
        assert!(matches!(&hook.mode, Mode::Autoplay(a) if a.fed() == 0));
        hook.wake(&mut sources, TimeUs(1_600 * MS));
        assert!(matches!(&hook.mode, Mode::Autoplay(a) if a.fed() == 2));
    }

    /// A failed link sends no switch: the input thread keeps its set.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_failed_link_sends_no_switch() {
        let mut sources = full_set();
        let (_controls, mut hook, link) = hooked(&sources);
        let _ = link.state.failed.set(game::InputWaitError::Timeout);
        link.switch(Mode::Autoplay(AutoplayInput::new(events())), switched());
        hook.wake(&mut sources, TimeUs(0));
        assert_eq!(sources.devices().count(), 4);
    }
}

#[cfg(test)]
mod link_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use bms_model::TimeUs;
    use game::{InputWaitError, ScratchConfig};
    use input::hid::HidProduct;
    use input::{DeviceId, DeviceKind, QpcClock, SourceSet};

    use super::autoplay_tests::{LEAD_IN, hooked, recorded, set};
    use super::{Hook, Mode, Recorded, Reply, Request, link};
    use crate::controls::Controls;

    /// A short bound for the tests that wait it out.
    const SHORT: Duration = Duration::from_millis(100);

    /// An input thread double: wakes the hook about every millisecond
    /// while not `stuck`, until stopped, and gives the hook back. It ends
    /// by itself after 30 s, so a failing test cannot hang.
    struct Waker {
        stop: Arc<AtomicBool>,
        stuck: Arc<AtomicBool>,
        /// Set by the thread once it has seen `stuck` between two wakes:
        /// no wake is under way, and none starts until `stuck` clears.
        paused: Arc<AtomicBool>,
        wakes: Arc<AtomicU64>,
        handle: JoinHandle<Hook>,
    }

    impl Waker {
        fn spawn(mut hook: Hook, mut sources: SourceSet) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let stuck = Arc::new(AtomicBool::new(false));
            let paused = Arc::new(AtomicBool::new(false));
            let wakes = Arc::new(AtomicU64::new(0));
            let (s, k, p, w) = (
                Arc::clone(&stop),
                Arc::clone(&stuck),
                Arc::clone(&paused),
                Arc::clone(&wakes),
            );
            let handle = std::thread::spawn(move || {
                let begun = Instant::now();
                let mut now = 0;
                while !s.load(Ordering::Acquire) && begun.elapsed() < Duration::from_secs(30) {
                    std::thread::sleep(Duration::from_millis(1));
                    if k.load(Ordering::Acquire) {
                        p.store(true, Ordering::Release);
                    } else {
                        p.store(false, Ordering::Release);
                        now += 1_000;
                        hook.wake(&mut sources, TimeUs(now));
                        w.fetch_add(1, Ordering::AcqRel);
                    }
                }
                hook
            });
            Self {
                stop,
                stuck,
                paused,
                wakes,
                handle,
            }
        }

        /// Stops the wakes and waits (at most 5 s) until the thread says
        /// its last wake has finished: a request sent after this is not
        /// answered until [`resume`](Self::resume).
        fn pause(&self) {
            self.paused.store(false, Ordering::Release);
            self.stuck.store(true, Ordering::Release);
            let begun = Instant::now();
            while !self.paused.load(Ordering::Acquire) {
                assert!(begun.elapsed() < Duration::from_secs(5), "not paused");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn resume(&self) {
            self.stuck.store(false, Ordering::Release);
        }

        /// Waits (at most 5 s) until the hook has woken twice more: a
        /// request sent before has been answered.
        fn two_more_wakes(&self) {
            let from = self.wakes.load(Ordering::Acquire);
            let begun = Instant::now();
            while self.wakes.load(Ordering::Acquire) < from + 2 {
                assert!(begun.elapsed() < Duration::from_secs(5), "no wakes");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn stop(self) -> Hook {
            self.stop.store(true, Ordering::Release);
            self.handle.join().unwrap()
        }
    }

    fn open_request() -> Request {
        Request::Open {
            lead_in: LEAD_IN,
            logic: std::thread::current(),
            recorded: recorded(),
            gate: None,
            focus: None,
        }
    }

    /// P1: with the hook gone, opening is an error, not a panic, and stays
    /// one.
    #[test]
    fn open_after_the_hook_is_gone_is_an_error() {
        let sources = set();
        let (_controls, hook, mut link) = hooked(&sources);
        drop(hook);
        for _ in 0..2 {
            assert_eq!(link.open(open_request()).err(), Some(InputWaitError::Gone));
            assert_eq!(link.close().err(), Some(InputWaitError::Gone));
        }
        assert_eq!(link.failure(), Some(InputWaitError::Gone));
    }

    /// P2: a close the input thread does not answer times out, and the link
    /// stays failed: once the late reply has arrived, the next open fails
    /// at once without reading it.
    #[test]
    fn an_unanswered_close_times_out_and_the_link_stays_failed() {
        let sources = set();
        let (_controls, hook, mut link) = hooked(&sources);
        let waker = Waker::spawn(hook, sources);
        // The open is answered however late the machine runs the thread:
        // its bound only catches one that never answers. Only the close
        // below, which nothing answers, is given the short bound.
        link.timeout = Duration::from_secs(60);
        assert!(link.open(open_request()).is_ok());
        assert_eq!(link.failure(), None);
        link.timeout = SHORT;
        waker.pause();
        // Measured on the awake clock the bound is kept on, which moves in
        // timer ticks and can reach it just before the wall clock does.
        let begun = link.awake_now();
        assert_eq!(link.close().err(), Some(InputWaitError::Timeout));
        let waited = link.awake_now().saturating_sub(begun);
        assert!(waited >= SHORT, "{waited:?}");
        waker.resume();
        // The late reply to the close arrives.
        waker.two_more_wakes();
        assert_eq!(
            link.open(open_request()).err(),
            Some(InputWaitError::Timeout)
        );
        assert_eq!(link.failure(), Some(InputWaitError::Timeout));
        // Never read, and no open was sent after it: a wait always sends
        // first, so the open failed without waiting.
        assert!(matches!(link.replies.try_recv(), Ok(Reply::Closed(..))));
        waker.two_more_wakes();
        assert!(link.replies.try_recv().is_err());
        drop(waker.stop());
    }

    /// A2: a handshake whose wall time passes the bound during a suspend,
    /// while the awake time stands still, is answered and leaves the link
    /// healthy.
    #[test]
    fn a_handshake_across_a_suspend_is_answered_and_not_poisoned() {
        let sources = set();
        let (_controls, hook, mut link) = hooked(&sources);
        link.timeout = SHORT;
        // Asleep the whole time: the awake time does not move.
        link.awake = || Duration::ZERO;
        let waker = Waker::spawn(hook, sources);
        waker.pause();
        let begun = Instant::now();
        let stuck = Arc::clone(&waker.stuck);
        let resume = std::thread::spawn(move || {
            std::thread::sleep(SHORT * 4);
            stuck.store(false, Ordering::Release);
        });
        assert!(link.open(open_request()).is_ok());
        assert!(begun.elapsed() >= SHORT * 4, "{:?}", begun.elapsed());
        assert_eq!(link.failure(), None);
        resume.join().unwrap();
        assert!(link.close().is_ok());
        drop(waker.stop());
    }

    /// An aborted play's close the input thread does not answer times out
    /// and fails the link: once the late answer has arrived, no later
    /// request reads it, the final close included, and none is sent.
    #[test]
    fn an_unanswered_abort_close_times_out_and_its_late_answer_is_never_read() {
        let sources = set();
        let (_controls, hook, mut link) = hooked(&sources);
        let waker = Waker::spawn(hook, sources);
        link.timeout = Duration::from_secs(60);
        assert!(link.open(open_request()).is_ok());
        link.timeout = SHORT;
        waker.pause();
        assert_eq!(link.abort_close().err(), Some(InputWaitError::Timeout));
        waker.resume();
        // The late answer arrives.
        waker.two_more_wakes();
        assert_eq!(link.abort_close().err(), Some(InputWaitError::Timeout));
        assert_eq!(link.hand_off().err(), Some(InputWaitError::Timeout));
        assert_eq!(link.close().err(), Some(InputWaitError::Timeout));
        assert_eq!(link.failure(), Some(InputWaitError::Timeout));
        // Still there, never read; nothing was asked after it.
        assert!(link.handed.pop().is_ok());
        waker.two_more_wakes();
        assert!(link.handed.pop().is_err());
        assert!(link.replies.try_recv().is_err());
        drop(waker.stop());
    }

    /// With the hook gone, an aborted play's close is `Gone`, and stays
    /// so.
    #[test]
    fn an_abort_close_after_the_hook_is_gone_is_an_error() {
        let sources = set();
        let (_controls, hook, mut link) = hooked(&sources);
        drop(hook);
        for _ in 0..2 {
            assert_eq!(link.abort_close().err(), Some(InputWaitError::Gone));
        }
        assert_eq!(link.failure(), Some(InputWaitError::Gone));
    }

    /// An aborted play's close whose wall time passes the bound during a
    /// suspend, while the awake time stands still, is answered and leaves
    /// the link healthy for the final close.
    #[test]
    fn an_abort_close_across_a_suspend_is_answered_and_not_poisoned() {
        let sources = set();
        let (_controls, hook, mut link) = hooked(&sources);
        let waker = Waker::spawn(hook, sources);
        link.timeout = Duration::from_secs(60);
        assert!(link.open(open_request()).is_ok());
        link.timeout = SHORT;
        // Asleep from here: the awake time does not move.
        link.awake = || Duration::ZERO;
        waker.pause();
        let begun = Instant::now();
        let stuck = Arc::clone(&waker.stuck);
        let resume = std::thread::spawn(move || {
            std::thread::sleep(SHORT * 4);
            stuck.store(false, Ordering::Release);
        });
        assert!(link.abort_close().is_ok());
        assert!(begun.elapsed() >= SHORT * 4, "{:?}", begun.elapsed());
        assert_eq!(link.failure(), None);
        resume.join().unwrap();
        assert!(link.close().is_ok());
        drop(waker.stop());
    }

    /// P3: a hook dropped while its thread unwinds from a panic is seen as
    /// gone, without any handshake.
    #[test]
    fn a_dropped_hook_is_seen_as_gone() {
        let sources = set();
        let (_controls, hook, link) = hooked(&sources);
        assert_eq!(link.failure(), None);
        let panicked = std::thread::spawn(move || {
            let _hook = hook;
            panic!("the input thread panics");
        })
        .join();
        assert!(panicked.is_err());
        assert_eq!(link.failure(), Some(InputWaitError::Gone));
    }

    /// P4: the open's console line (the configured controller is not
    /// connected) travels with the reply instead of being printed by the
    /// input thread.
    #[test]
    fn the_open_reply_carries_the_console_lines() {
        let mut sources = set();
        sources.begin_batch(TimeUs(0));
        sources.connect(DeviceId(1), DeviceKind::Keyboard);
        sources.end_batch(TimeUs(0));
        let controls = Arc::new(Controls::new(QpcClock::start()));
        controls.set_latch(sources.gate().handle());
        let (mut hook, link) = link(
            Arc::clone(&controls),
            Mode::Clock,
            input::Waker::new().unwrap(),
        );
        let recorded = Recorded {
            controller_device: Some(HidProduct {
                vendor_id: 0x1A2B,
                product_id: 0x3C4D,
                interface: None,
            }),
            scratch: ScratchConfig::default(),
            ..recorded()
        };
        link.requests
            .send(Request::Open {
                lead_in: LEAD_IN,
                logic: std::thread::current(),
                recorded,
                gate: None,
                focus: None,
            })
            .unwrap();
        hook.wake(&mut sources, TimeUs(1_000));
        let Ok(Reply::Opened(_, lines)) = link.replies.try_recv() else {
            panic!("an open is answered by an opening");
        };
        assert_eq!(
            lines,
            [
                "input: the controller set up in `tapestry keys` is not connected; this play uses the keyboard bindings only"
            ]
        );
    }
}

/// The hook on a real clocked input thread: how requests and focus
/// changes reach a thread in its tail or idle.
#[cfg(test)]
mod wake_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use input::{ClockedThread, QpcClock, SourceSet, Waker};

    use super::autoplay_tests::{LEAD_IN, hooked_with, recorded, set};
    use super::{Hook, InputLink, Request};

    /// The hook on a clocked thread, counting its wakes.
    fn spawn(mut hook: Hook, sources: SourceSet, waker: &Waker) -> (ClockedThread, Arc<AtomicU64>) {
        let wakes = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&wakes);
        let thread = ClockedThread::spawn(QpcClock::start(), sources, waker, move |s, now| {
            counted.fetch_add(1, Ordering::AcqRel);
            hook.wake(s, now)
        })
        .unwrap();
        (thread, wakes)
    }

    fn open(link: &mut InputLink) {
        let opened = link.open(Request::Open {
            lead_in: LEAD_IN,
            logic: std::thread::current(),
            recorded: recorded(),
            gate: None,
            focus: None,
        });
        assert!(opened.is_ok());
    }

    /// Waits (at most 5 s) until `f` holds.
    fn until(f: impl Fn() -> bool) -> bool {
        let begun = Instant::now();
        while begun.elapsed() < Duration::from_secs(5) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    /// Waits until `wakes` stays still for 50 ms: the thread is idle.
    fn settles(wakes: &AtomicU64) {
        assert!(until(|| {
            let before = wakes.load(Ordering::Acquire);
            std::thread::sleep(Duration::from_millis(50));
            wakes.load(Ordering::Acquire) == before
        }));
    }

    /// An idle thread, which has no timer, serves an open at once: the
    /// link wakes it after the request.
    #[test]
    fn request_wakes_the_idle_thread() {
        let sources = set();
        let waker = Waker::new().unwrap();
        let (_controls, hook, mut link) = hooked_with(&sources, waker.clone());
        let (thread, wakes) = spawn(hook, sources, &waker);
        link.idle();
        settles(&wakes);
        let begun = Instant::now();
        open(&mut link);
        assert!(
            begun.elapsed() < Duration::from_millis(500),
            "{:?}",
            begun.elapsed()
        );
        // Open again: the heartbeat runs.
        let from = wakes.load(Ordering::Acquire);
        assert!(until(|| wakes.load(Ordering::Acquire) >= from + 10));
        let report = thread.stop().unwrap();
        assert!(report.causes.event >= 2, "{:?}", report.causes);
    }

    /// In an autoplay tail with no input and no wake from the window, the
    /// heartbeat still checks the focus gate: a focus loss is published,
    /// so the tail ends.
    #[test]
    fn focus_loss_in_the_tail_without_input_ends_the_tail() {
        let sources = set();
        let waker = Waker::new().unwrap();
        let (controls, hook, mut link) = hooked_with(&sources, waker.clone());
        let (thread, _) = spawn(hook, sources, &waker);
        open(&mut link);
        assert!(link.close().is_ok());
        assert!(!link.tripped());
        // The window's controls hold no waker here.
        controls.focus(false);
        assert!(until(|| link.tripped()));
        thread.stop().unwrap();
    }

    /// An idle thread sees a focus loss through the window's wake, with no
    /// timer.
    #[test]
    fn a_focus_loss_wakes_an_idle_input_thread() {
        let sources = set();
        let waker = Waker::new().unwrap();
        let (controls, hook, link) = hooked_with(&sources, waker.clone());
        controls.set_waker(waker.clone());
        let (thread, wakes) = spawn(hook, sources, &waker);
        link.idle();
        settles(&wakes);
        let from = wakes.load(Ordering::Acquire);
        controls.focus(false);
        assert!(until(|| link.tripped()));
        assert_eq!(wakes.load(Ordering::Acquire), from + 1);
        let report = thread.stop().unwrap();
        assert!(report.causes.heartbeat <= 2, "{:?}", report.causes);
    }
}

/// The controller's menu through the hook: menu events flow only while the
/// thread is Idle and the window has focus, never from a background
/// packet, and wake the game thread.
#[cfg(test)]
mod menu_tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bms_model::TimeUs;
    use eval::ControlId;
    use game::{KeyConfig, LaneKey, ScratchConfig};
    use input::hid::{HidDevice, HidLayout, HidProduct, HidReport, axis_cap, axis_control};
    use input::{
        Capture, DeviceId, DeviceKind, MenuEvent, MenuReceiver, QpcClock, SourceConfig, SourceSet,
        Waker,
    };

    use super::{Hook, InputLink, Mode, Recorded, Reply, Request, link};
    use crate::controls::Controls;
    use crate::select::menu_bindings;

    const PAD: HidProduct = HidProduct {
        vendor_id: 0x1A2B,
        product_id: 0x3C4D,
        interface: None,
    };
    const X: ControlId = axis_control(0x01, 0x30);

    /// One batch's reports: the buttons held and X's position.
    type Reports<'a> = &'a [(&'a [u16], Option<i32>)];

    /// Buttons 1 to 7 play keys 1 to 7, X the scratch.
    fn keys() -> KeyConfig {
        let mut lanes: Vec<LaneKey> = (1..=7)
            .map(|n| LaneKey {
                control: u32::from(n),
                lane: n,
            })
            .collect();
        lanes.push(LaneKey {
            control: X.0,
            lane: 0,
        });
        KeyConfig {
            lanes,
            retry: None,
            quit: None,
            ..KeyConfig::default()
        }
    }

    struct Rig {
        controls: Arc<Controls>,
        hook: Hook,
        link: InputLink,
        sources: SourceSet,
        pad: HidDevice,
        capture: Capture,
        menu: MenuReceiver,
        now: i64,
    }

    impl Rig {
        /// The configured controller connected, with its menu, the window
        /// focused, and the thread in its tail as at its start.
        fn new() -> Self {
            Self::with(ScratchConfig::default())
        }

        /// [`new`](Self::new) with the menu bound under `scratch`.
        fn with(scratch: ScratchConfig) -> Self {
            let mut sources = SourceSet::new(SourceConfig {
                max_devices: 4,
                max_sources: 4,
                max_held: 16,
                queue_capacity: 64,
                max_arrivals: 2,
            })
            .unwrap();
            let caps = [axis_cap(X, true, 0, 255, 8).unwrap()];
            sources.begin_batch(TimeUs(0));
            sources.connect(DeviceId(10), DeviceKind::Controller);
            sources.set_product(DeviceId(10), PAD);
            sources.add_axes(DeviceId(10), &caps);
            sources.end_batch(TimeUs(0));
            let bindings = menu_bindings(&keys(), Some(PAD), &scratch).unwrap();
            let (tap, menu) = input::menu_queue(bindings);
            sources.set_menu(tap);
            let controls = Arc::new(Controls::new(QpcClock::start()));
            controls.set_latch(sources.gate().handle());
            controls.set_game(std::thread::current());
            let (hook, link) = link(Arc::clone(&controls), Mode::Clock, Waker::new().unwrap());
            let layout = HidLayout {
                collection: 0,
                axes: &caps,
                buttons: &[],
            };
            Self {
                controls,
                hook,
                link,
                sources,
                pad: HidDevice::new(DeviceId(10), &layout),
                capture: Capture::new(),
                menu,
                now: 1_000,
            }
        }

        /// A boundary with no batch before it.
        fn wake(&mut self) {
            self.now += 1_000;
            self.hook.wake(&mut self.sources, TimeUs(self.now));
        }

        /// One batch of reports through the controller's reader, then its
        /// boundary.
        fn batch(&mut self, reports: Reports<'_>, background: bool) {
            self.now += 1_000;
            self.sources.begin_batch(TimeUs(self.now));
            for &(buttons, x) in reports {
                let axes = [x];
                let report = HidReport {
                    id: 0,
                    buttons: Some(buttons),
                    axes: &axes,
                };
                self.pad
                    .report(&report, background, &self.capture, &mut self.sources);
            }
            self.sources.end_batch(TimeUs(self.now));
            self.wake();
        }

        fn ask(&mut self, request: Request) -> Option<Reply> {
            self.link.requests.send(request).unwrap();
            self.wake();
            self.link.replies.try_recv().ok()
        }

        fn idle(&mut self) {
            assert!(self.ask(Request::Idle).is_none());
        }

        fn open(&mut self) {
            let recorded = Recorded {
                keyboard: Vec::new(),
                controller: keys().recorded(),
                controller_device: Some(PAD),
                scratch: ScratchConfig::default(),
            };
            let reply = self.ask(Request::Open {
                lead_in: TimeUs(1_000_000),
                logic: std::thread::current(),
                recorded,
                gate: None,
                focus: None,
            });
            assert!(matches!(reply, Some(Reply::Opened(..))));
        }

        fn events(&mut self) -> Vec<MenuEvent> {
            std::iter::from_fn(|| self.menu.pop()).collect()
        }

        /// Takes any unpark left, runs `reports`, and says whether the game
        /// thread (this one) was unparked by them.
        fn unparks(&mut self, reports: Reports<'_>) -> bool {
            std::thread::park_timeout(Duration::ZERO);
            self.batch(reports, false);
            let begun = Instant::now();
            std::thread::park_timeout(Duration::from_secs(5));
            begun.elapsed() < Duration::from_secs(1)
        }
    }

    /// A press and release of each button of `buttons`.
    fn presses(buttons: &[u16]) -> Vec<(&[u16], Option<i32>)> {
        buttons
            .iter()
            .flat_map(|b| [(std::slice::from_ref(b), None), (&[][..], None)])
            .collect()
    }

    /// Idle and focused, every bound control of the configured controller
    /// becomes its menu event, in order: the white keys 1, 3 and 5 confirm,
    /// key 7 sends its own event, the black keys go back, and the scratch axis scrolls a line per press
    /// threshold (2), higher values down; an unbound button sends nothing.
    /// The game thread is woken for them.
    #[test]
    fn menu_events_from_bound_controls() {
        use MenuEvent::{Back, Confirm, Down, Extra, Up};
        let mut rig = Rig::new();
        rig.idle();
        assert!(rig.unparks(&presses(&[1, 2, 3, 4, 5, 6, 7, 9])));
        assert_eq!(
            rig.events(),
            [Confirm, Back, Confirm, Back, Confirm, Back, Extra]
        );
        let turns: Reports<'_> = &[
            (&[], Some(100)),
            (&[], Some(102)),
            (&[], Some(103)),
            (&[], Some(101)),
            (&[], Some(100)),
        ];
        assert!(rig.unparks(turns));
        assert_eq!(rig.events(), [Down, Up]);
    }

    /// With the menu direction reversed in `device.toml`, the scratch axis
    /// scrolls the other way on every menu that takes it: toward higher
    /// values up the select screen's list, to the result's previous preset
    /// and up the practice page's rows, toward lower values the other way.
    #[test]
    fn a_reversed_scratch_axis_scrolls_every_menu_the_other_way() {
        use crate::result_input::{Owner, ResultInput};
        use crate::select::MenuQueue;
        use game::{MenuCommand, PracticeInput};

        let higher_then_lower: Reports<'_> =
            &[(&[], Some(100)), (&[], Some(102)), (&[], Some(100))];
        // As the default has it, for comparison.
        let mut rig = Rig::new();
        rig.idle();
        rig.batch(higher_then_lower, false);
        assert_eq!(rig.events(), [MenuEvent::Down, MenuEvent::Up]);

        let reversed = ScratchConfig {
            menu_reversed: true,
            ..ScratchConfig::default()
        };
        let mut rig = Rig::with(reversed);
        rig.idle();
        // The select screen takes the events as they come.
        rig.batch(higher_then_lower, false);
        assert_eq!(rig.events(), [MenuEvent::Up, MenuEvent::Down]);
        // The result screen and the practice page read them through the
        // menus' shared input.
        let (_, spare) = input::menu_queue(menu_bindings(&keys(), Some(PAD), &reversed).unwrap());
        let pad = std::mem::replace(&mut rig.menu, spare);
        let mut menus = ResultInput::new(Arc::new(MenuQueue::default()), Some(pad));
        menus.take_over(Owner::Result);
        rig.batch(higher_then_lower, false);
        let result: Vec<_> = std::iter::from_fn(|| menus.take()).collect();
        assert_eq!(result, [MenuCommand::PrevPreset, MenuCommand::NextPreset]);
        menus.take_over(Owner::Practice);
        rig.batch(higher_then_lower, false);
        let practice: Vec<_> = std::iter::from_fn(|| menus.take_practice()).collect();
        assert_eq!(practice, [PracticeInput::Up, PracticeInput::Down]);
    }

    /// While Open or Tail no menu events are sent; once Idle again they
    /// are.
    #[test]
    fn no_menu_events_while_recording() {
        let mut rig = Rig::new();
        let press = presses(&[1]);
        // The thread starts in its tail.
        rig.batch(&press, false);
        assert_eq!(rig.events(), []);
        rig.idle();
        rig.open();
        rig.batch(&press, false);
        rig.batch(&[(&[], Some(10)), (&[], Some(20))], false);
        assert_eq!(rig.events(), []);
        assert!(matches!(rig.ask(Request::Close), Some(Reply::Closed(..))));
        rig.batch(&press, false);
        rig.batch(&[(&[], Some(30)), (&[], Some(40))], false);
        assert_eq!(rig.events(), []);
        rig.idle();
        rig.batch(&press, false);
        assert_eq!(rig.events(), [MenuEvent::Confirm]);
    }

    /// A packet received in the background never reaches the menu, nor
    /// does any packet while the window has no focus; with focus back the
    /// controls send again.
    #[test]
    fn background_packets_never_reach_the_menu() {
        let mut rig = Rig::new();
        rig.idle();
        let press = presses(&[1]);
        rig.batch(&press, true);
        rig.batch(&[(&[], Some(10)), (&[], Some(20))], true);
        assert_eq!(rig.events(), []);
        rig.controls.focus(false);
        rig.wake();
        rig.batch(&press, false);
        assert_eq!(rig.events(), []);
        rig.controls.focus(true);
        rig.wake();
        rig.batch(&press, false);
        assert_eq!(rig.events(), [MenuEvent::Confirm]);
    }
}

/// The input thread's side of the start gate (design.md "Play Lifecycle",
/// the start gate), through `Hook::wake` as the clocked input thread calls
/// it: a gated opening deferred while START is held, the input side's
/// cancellation, and a rearm at one boundary that seals the cancelled
/// opening with what came since the game thread's drain and opens the next
/// on the same source table, a device removed and reconnected meanwhile
/// keeping its source id, allocating nothing.
#[cfg(test)]
mod gate_tests {
    use std::sync::Arc;

    use bms_model::TimeUs;
    use eval::{ControlId, InputKind, InputStart, SourceId};
    use input::{
        AdjustmentBindings, DeviceId, DeviceKind, GatePhase, Packet, QpcClock, SourceConfig,
        SourceSet, StartGate, TapControl, Waker, adjustment_queue,
    };

    use super::autoplay_tests::{LEAD_IN, recorded};
    use super::{Hook, InputLink, Mode, Rearm, RearmOutcome, Reply, Request, link};
    use crate::controls::Controls;

    const MS: i64 = 1_000;
    const Q: ControlId = ControlId(0x10);
    const Z: ControlId = ControlId(0x2c);

    fn packet(device: u64, control: ControlId, value: i32) -> Packet {
        Packet {
            device: DeviceId(device),
            control,
            value,
            background: false,
        }
    }

    /// Two keyboards, the hook of a device play with a START tap, and the
    /// tap's gate.
    fn rig() -> (SourceSet, Hook, InputLink, StartGate) {
        let mut sources = SourceSet::new(SourceConfig {
            max_devices: 4,
            max_sources: 4,
            max_held: 4,
            queue_capacity: 16,
            max_arrivals: 4,
        })
        .unwrap();
        sources.begin_batch(TimeUs(0));
        sources.connect(DeviceId(1), DeviceKind::Keyboard);
        sources.connect(DeviceId(2), DeviceKind::Keyboard);
        sources.end_batch(TimeUs(1));
        let controls = Arc::new(Controls::new(QpcClock::start()));
        controls.set_latch(sources.gate().handle());
        let (hook, link) = link(controls, Mode::Clock, Waker::new().unwrap());
        let (tap, _rx) = adjustment_queue(AdjustmentBindings {
            controls: vec![(TapControl::Key(Q), 0)],
            latched: 1,
            ..AdjustmentBindings::default()
        })
        .unwrap();
        let gate = tap.start_gate();
        link.tap(tap);
        (sources, hook, link, gate)
    }

    /// One batch at `at` ms of `packets`, then the hook's boundary.
    fn batch(hook: &mut Hook, sources: &mut SourceSet, at: i64, packets: &[Packet]) {
        sources.begin_batch(TimeUs(at * MS));
        for p in packets {
            sources.packet(p);
        }
        sources.end_batch(TimeUs(at * MS + 1));
        hook.wake(sources, TimeUs(at * MS + 1));
    }

    fn open(
        hook: &mut Hook,
        link: &InputLink,
        sources: &mut SourceSet,
        at: i64,
    ) -> Option<game::Opening<input::SourceReader>> {
        link.requests
            .send(Request::Open {
                lead_in: LEAD_IN,
                logic: std::thread::current(),
                recorded: recorded(),
                gate: Some(1),
                focus: None,
            })
            .unwrap();
        batch(hook, sources, at, &[]);
        match link.replies.try_recv().unwrap() {
            Reply::Opened(o, _) => Some(o),
            Reply::Deferred => None,
            Reply::Closed(..) => panic!("an open is answered by an opening"),
        }
    }

    /// [`rig`]'s two keyboards and hook, its START published as START's
    /// panel role, with the controls the hook publishes tuning into.
    fn tuning_rig() -> (SourceSet, Hook, InputLink, Arc<Controls>) {
        let mut sources = SourceSet::new(SourceConfig {
            max_devices: 4,
            max_sources: 4,
            max_held: 4,
            queue_capacity: 16,
            max_arrivals: 4,
        })
        .unwrap();
        sources.begin_batch(TimeUs(0));
        sources.connect(DeviceId(1), DeviceKind::Keyboard);
        sources.connect(DeviceId(2), DeviceKind::Keyboard);
        sources.end_batch(TimeUs(1));
        let controls = Arc::new(Controls::new(QpcClock::start()));
        controls.set_latch(sources.gate().handle());
        let (hook, link) = link(Arc::clone(&controls), Mode::Clock, Waker::new().unwrap());
        let (mut tap, _rx) = adjustment_queue(AdjustmentBindings {
            controls: vec![(TapControl::Key(Q), 0)],
            latched: 1,
            ..AdjustmentBindings::default()
        })
        .unwrap();
        tap.set_panel_roles(1, 0);
        tap.set_tuning_controls(1);
        link.tap(tap);
        (sources, hook, link, controls)
    }

    /// A7 and §2.10 R13, the input side's half: START held by two
    /// keyboards is one hold in the activity record's input section. The
    /// first one's removal leaves it held, moving nothing; the second's
    /// releases it, moving the revision and the last activity to that
    /// batch. The press counts at its batch too.
    #[test]
    fn a_removed_source_leaves_only_its_own_holds() {
        let (mut sources, mut hook, _link, controls) = tuning_rig();
        let read = || controls.tuning().activity().read().unwrap().input;
        batch(&mut hook, &mut sources, 0, &[]);
        let idle = read();
        assert!(!idle.held);
        batch(&mut hook, &mut sources, 10, &[packet(1, Q, 1)]);
        let one = read();
        assert!(one.held);
        assert_eq!(one.revision, idle.revision + 1);
        assert_eq!(one.last, Some(TimeUs(10 * MS)));
        assert!(controls.tuning().snapshot().held, "the snapshot holds");
        // The second keyboard holds it too: one hold.
        batch(&mut hook, &mut sources, 20, &[packet(2, Q, 1)]);
        assert_eq!(read(), one);
        // The first keyboard removed while it holds: the other's stays.
        sources.begin_batch(TimeUs(30 * MS));
        sources.disconnect(DeviceId(1));
        sources.end_batch(TimeUs(30 * MS + 1));
        hook.wake(&mut sources, TimeUs(30 * MS + 1));
        assert_eq!(read(), one);
        assert!(controls.tuning().snapshot().held);
        // The second removed: released at that batch.
        sources.begin_batch(TimeUs(40 * MS));
        sources.disconnect(DeviceId(2));
        sources.end_batch(TimeUs(40 * MS + 1));
        hook.wake(&mut sources, TimeUs(40 * MS + 1));
        let none = read();
        assert!(!none.held);
        assert_eq!(none.revision, one.revision + 1);
        assert_eq!(none.last, Some(TimeUs(40 * MS)));
        let s = controls.tuning().snapshot();
        assert!(s.stable && !s.held);
        assert_eq!(s.revision.input, none.revision);
        assert_eq!(s.last_activity, Some(TimeUs(40 * MS)));
    }

    /// §2.10 R16 and I5: an opening out of the loading state carries the
    /// window's focus-loss generation its tuning acceptance closed with.
    /// Focus lost and back since, and the return sampled at a batch, so the
    /// source set's own focus check passes: the hook defers it all the
    /// same, before the source set opens anything. Asked again with the
    /// generation now, it opens.
    #[test]
    fn an_opening_with_a_moved_focus_generation_is_deferred() {
        let (mut sources, mut hook, link, controls) = tuning_rig();
        batch(&mut hook, &mut sources, 0, &[]);
        let closed = controls.tuning().close(1);
        assert_eq!(closed.focus_losses, 0);
        controls.focus(false);
        controls.focus(true);
        assert_eq!(controls.tuning().focus_losses(), 1);
        // The input thread samples the return.
        batch(&mut hook, &mut sources, 10, &[]);
        let ask = |focus| Request::Open {
            lead_in: LEAD_IN,
            logic: std::thread::current(),
            recorded: recorded(),
            gate: Some(1),
            focus: Some(focus),
        };
        link.requests.send(ask(closed.focus_losses)).unwrap();
        batch(&mut hook, &mut sources, 20, &[]);
        assert!(matches!(link.replies.try_recv(), Ok(Reply::Deferred)));
        assert_eq!(controls.tuning().activity().opened(), 0, "nothing opened");
        link.requests.send(ask(1)).unwrap();
        batch(&mut hook, &mut sources, 30, &[]);
        assert!(matches!(link.replies.try_recv(), Ok(Reply::Opened(..))));
        assert_eq!(controls.tuning().activity().opened(), 1);
    }

    /// A16 and A18p: the opening linearizes at the input thread's
    /// focus-generation check. A focus loss after that check, while the
    /// hook builds the opening (stopped at its gate), also lost and back
    /// again, opens recording all the same, and the focus gate, reset
    /// before the check, trips at the next batch: the play is aborted for
    /// focus (`focus_lost`) and nothing after the loss is recorded. With
    /// focus kept, the same press is recorded.
    #[test]
    fn a_focus_loss_after_the_generation_check_aborts_the_play() {
        use std::time::Duration;

        let watchdog = Duration::from_secs(
            std::env::var("TAPESTRY_TEST_WATCHDOG")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(600),
        );
        for (case, lose, back) in [
            ("kept", false, false),
            ("lost", true, false),
            ("lost and back", true, true),
        ] {
            let (mut sources, mut hook, link, controls) = tuning_rig();
            batch(&mut hook, &mut sources, 0, &[]);
            let closed = controls.tuning().close(1);
            let (reached_tx, reached) = std::sync::mpsc::channel();
            let (resume, resume_rx) = std::sync::mpsc::channel();
            link.open_gate(super::OpenGate {
                reached: reached_tx,
                resume: resume_rx,
            });
            link.requests
                .send(Request::Open {
                    lead_in: LEAD_IN,
                    logic: std::thread::current(),
                    recorded: recorded(),
                    gate: Some(1),
                    focus: Some(closed.focus_losses),
                })
                .unwrap();
            std::thread::scope(|s| {
                let opener = s.spawn(|| batch(&mut hook, &mut sources, 40, &[]));
                reached
                    .recv_timeout(watchdog)
                    .expect("the opening reached its gate");
                if lose {
                    controls.focus(false);
                }
                if back {
                    controls.focus(true);
                }
                resume.send(()).unwrap();
                opener.join().unwrap();
            });
            let Ok(Reply::Opened(mut opening, _)) = link.replies.try_recv() else {
                panic!("{case}: the opening is answered opened");
            };
            batch(&mut hook, &mut sources, 50, &[packet(1, Z, 1)]);
            assert_eq!(link.tripped(), lose, "{case}");
            let mut recorded = Vec::new();
            for reader in &mut opening.readers {
                while let Some(e) = reader.pop() {
                    recorded.push(e.received);
                }
            }
            // The press, on the play's clock from the opening's origin.
            let expected: &[TimeUs] = if lose {
                &[]
            } else {
                &[TimeUs(50 * MS - (40 * MS + 1 + LEAD_IN.0))]
            };
            assert_eq!(recorded, expected, "{case}");
        }
    }

    /// A16: the input thread publishes candidate generation 1 as opened as
    /// soon as the source set opened it, before it builds the opening and
    /// replies. Stopped there, tuning acceptance closed until generation
    /// 1 reads open, and a settings key pressed in the window then steps
    /// the in-play values and holds, while the starting tuning the close
    /// returned keeps the value before it.
    #[test]
    fn a_press_after_recording_opens_before_the_hook_returns_is_in_play() {
        use std::time::Duration;

        use game::Adjust;

        use crate::tuning::{Acceptance, HoldClass, KeyEvent};

        let watchdog = Duration::from_secs(
            std::env::var("TAPESTRY_TEST_WATCHDOG")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(600),
        );
        let (mut sources, mut hook, link, controls) = tuning_rig();
        batch(&mut hook, &mut sources, 0, &[]);
        let starting = controls.tuning().close(1);
        assert_eq!(
            controls.tuning().acceptance(),
            Acceptance::Closed { reopen_at: 1 }
        );
        // Closed: a press is discarded.
        controls.settings_key(Adjust::DisplayOffsetUp, KeyEvent::Press);
        controls.settings_key(Adjust::DisplayOffsetUp, KeyEvent::Release);
        assert_eq!(controls.tuning().get(), starting.values.unwrap());
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        link.open_gate(super::OpenGate {
            reached: reached_tx,
            resume: resume_rx,
        });
        link.requests
            .send(Request::Open {
                lead_in: LEAD_IN,
                logic: std::thread::current(),
                recorded: recorded(),
                gate: Some(1),
                focus: None,
            })
            .unwrap();
        std::thread::scope(|s| {
            let opener = s.spawn(|| batch(&mut hook, &mut sources, 40, &[]));
            reached
                .recv_timeout(watchdog)
                .expect("the opening reached its gate");
            // Opened, published, not yet answered.
            assert_eq!(controls.tuning().activity().opened(), 1);
            assert!(link.replies.try_recv().is_err(), "no reply yet");
            controls.settings_key(Adjust::DisplayOffsetUp, KeyEvent::Press);
            assert_eq!(controls.tuning().acceptance(), Acceptance::Open);
            assert_eq!(
                controls.tuning().held(),
                [(Adjust::DisplayOffsetUp, HoldClass::Fresh)]
            );
            assert_eq!(
                controls.tuning().get().display_offset_us,
                starting.values.unwrap().display_offset_us + 1_000
            );
            resume.send(()).unwrap();
            opener.join().unwrap();
        });
        assert!(matches!(link.replies.try_recv(), Ok(Reply::Opened(..))));
        assert_eq!(
            starting.values.unwrap().display_offset_us + 1_000,
            controls.tuning().get().display_offset_us,
            "the starting tuning kept the value before the press"
        );
    }

    fn rearm_buffers(readers: Vec<input::SourceReader>, snapshot: usize) -> Box<Rearm> {
        Box::new(Rearm {
            readers,
            lead_in: LEAD_IN,
            generation: 2,
            start: InputStart {
                opened_at: TimeUs(0),
                held: Vec::with_capacity(snapshot),
            },
            arrivals: Vec::with_capacity(16),
            leftover: Vec::with_capacity(64),
            room: 64,
            outcome: RearmOutcome::Pending,
        })
    }

    #[test]
    fn a_rearm_seals_the_cancelled_opening_and_opens_the_next_at_one_boundary_alloc() {
        let (mut sources, mut hook, mut link, gate) = rig();
        // START held: the opening is deferred, nothing opens.
        batch(&mut hook, &mut sources, 10, &[packet(1, Q, 1)]);
        assert!(open(&mut hook, &link, &mut sources, 20).is_none());
        assert!(!sources.is_recording());
        // Released: candidate 1 opens and is armed.
        batch(&mut hook, &mut sources, 30, &[packet(1, Q, 0)]);
        let opening = open(&mut hook, &link, &mut sources, 40).unwrap();
        assert_eq!(gate.state().phase, GatePhase::Armed);
        let snapshot = opening.start.held.capacity();
        let mut readers = opening.readers;
        assert_eq!(readers.len(), 2);
        // A lane press, then START: the input side cancels at once.
        batch(&mut hook, &mut sources, 100, &[packet(2, Z, 1)]);
        batch(&mut hook, &mut sources, 110, &[packet(1, Q, 1)]);
        assert_eq!(gate.state().phase, GatePhase::Cancelled);
        // The game thread drains what came so far; Z's release comes after.
        for r in &mut readers {
            while r.pop().is_some() {}
        }
        batch(&mut hook, &mut sources, 120, &[packet(2, Z, 0)]);
        // Keyboard 2 removed and connected again while waiting.
        sources.begin_batch(TimeUs(130 * MS));
        sources.disconnect(DeviceId(2));
        sources.connect(DeviceId(2), DeviceKind::Keyboard);
        sources.end_batch(TimeUs(130 * MS + 1));
        // Still held: the rearm changes nothing.
        let rearm = rearm_buffers(readers, snapshot);
        link.rearms.push(rearm).unwrap();
        batch(&mut hook, &mut sources, 140, &[]);
        let rearm = link.rearmed.pop().unwrap();
        assert_eq!(rearm.outcome, RearmOutcome::Held);
        // Released: one boundary seals and reopens, allocating nothing.
        batch(&mut hook, &mut sources, 150, &[packet(1, Q, 0)]);
        link.rearms.push(rearm).unwrap();
        // The positive control: this thread's allocations are counted.
        #[cfg(feature = "dev-tools")]
        {
            let control = crate::alloc_count::this_thread();
            drop(std::hint::black_box(vec![1_u8; 8]));
            assert!(
                crate::alloc_count::this_thread() > control,
                "the allocation was not counted"
            );
        }
        #[cfg(feature = "dev-tools")]
        let before = crate::alloc_count::this_thread();
        batch(&mut hook, &mut sources, 160, &[]);
        #[cfg(feature = "dev-tools")]
        assert_eq!(
            crate::alloc_count::this_thread(),
            before,
            "the rearm allocated"
        );
        let rearm = link.rearmed.pop().unwrap();
        let origin = TimeUs(160 * MS + 1 + LEAD_IN.0);
        assert_eq!(
            rearm.outcome,
            RearmOutcome::Opened {
                origin,
                closed: TimeUs(160 * MS + 1 - opening.origin.0),
            }
        );
        assert_eq!(gate.state().phase, GatePhase::Armed);
        assert_eq!(gate.state().generation, 2);
        // What came since the drain: Z's release, and keyboard 2's removal.
        let kinds: Vec<_> = rearm.leftover.iter().map(|e| (e.source, e.kind)).collect();
        assert!(kinds.contains(&(
            SourceId(1),
            InputKind::Control {
                control: Z,
                value: 0
            }
        )));
        assert!(kinds.contains(&(SourceId(1), InputKind::Disconnected)));
        // Its reconnection is an arrival of the sealed opening.
        assert_eq!(rearm.arrivals.len(), 1);
        // The same source table: two readers, keyboard 2 still source 1 and
        // absent in the next opening, which opened at −1 s.
        assert_eq!(rearm.readers.len(), 2);
        assert_eq!(rearm.start.opened_at, TimeUs(-LEAD_IN.0));
        let mut readers = rearm.readers;
        batch(&mut hook, &mut sources, 170, &[packet(1, Z, 1)]);
        let next: Vec<_> = readers
            .iter_mut()
            .flat_map(|r| std::iter::from_fn(move || r.pop()))
            .map(|e| (e.source, e.kind))
            .collect();
        assert!(next.contains(&(SourceId(1), InputKind::Disconnected)));
        assert!(next.contains(&(
            SourceId(0),
            InputKind::Control {
                control: Z,
                value: 1
            }
        )));
    }

    /// A rearm whose sealed opening holds more than the history takes is
    /// refused: nothing opens, and the capture is sealed.
    #[test]
    fn a_rearm_past_the_historys_room_opens_nothing() {
        let (mut sources, mut hook, mut link, gate) = rig();
        let opening = open(&mut hook, &link, &mut sources, 10).unwrap();
        batch(&mut hook, &mut sources, 20, &[packet(1, Q, 1)]);
        batch(
            &mut hook,
            &mut sources,
            30,
            &[packet(2, Z, 1), packet(2, Z, 0)],
        );
        batch(&mut hook, &mut sources, 40, &[packet(1, Q, 0)]);
        let mut rearm = rearm_buffers(opening.readers, opening.start.held.capacity());
        rearm.room = 1;
        link.rearms.push(rearm).unwrap();
        batch(&mut hook, &mut sources, 50, &[]);
        let rearm = link.rearmed.pop().unwrap();
        assert_eq!(rearm.outcome, RearmOutcome::Refused);
        assert_eq!(gate.state().generation, 1, "nothing was armed");
    }

    /// A scripted play's rearm opens at the release the script reads, not
    /// at the boundary that answers it (design.md "Shortening Launch-Based
    /// Checks", scripted input; "Play Lifecycle", the start gate): START
    /// pressed at −300 ms cancels READY and released at 200 ms holds the
    /// script there; the rearm, asked late and answered no sooner than the
    /// rearm delay after the release, opens chart time 0 at 1.2 s on the
    /// script's timeline, so Z pressed at 1.7 s on it lands at 500 ms, as
    /// the log has it, 40 ms of clock after the script would have fed it.
    /// The stamps go on rising across the sealed and the new opening.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_late_rearm_opens_at_the_scripted_release() {
        let (mut sources, mut hook, mut link, gate, opening) = scripted();
        // START held: READY is cancelled.
        hook.wake(&mut sources, script_clock(-299_500));
        assert_eq!(gate.state().phase, GatePhase::Cancelled);
        let mut readers = opening.readers;
        let mut sealed: Vec<_> = readers
            .iter_mut()
            .flat_map(|r| std::iter::from_fn(move || r.pop()))
            .collect();
        // Released at 200 ms, seen 0.5 ms later at a heartbeat.
        hook.wake(&mut sources, script_clock(100_000));
        hook.wake(&mut sources, script_clock(200_500));
        // The game thread asks 15 ms later; the delay holds the answer
        // until 30 ms after the release was seen.
        link.rearms
            .push(rearm_buffers(readers, opening.start.held.capacity()))
            .unwrap();
        hook.wake(&mut sources, script_clock(215_000));
        assert!(link.rearmed.pop().is_err(), "answered before the delay");
        hook.wake(&mut sources, script_clock(240_000));
        let rearm = link.rearmed.pop().unwrap();
        assert_eq!(
            rearm.outcome,
            RearmOutcome::Opened {
                origin: TimeUs(script_clock(240_000).0 + LEAD_IN.0),
                closed: TimeUs(240_000),
            }
        );
        assert_eq!(rearm.start.opened_at, TimeUs(-LEAD_IN.0));
        sealed.extend(rearm.leftover.iter().copied());
        // Z at 1.7 s on the script's timeline, now 40 ms later on the clock.
        let mut readers = rearm.readers;
        let next = feed_z(&mut hook, &mut sources, &mut readers, 40_000);
        // On the script's timeline (the sealed opening's chart time 0 at
        // its 0, the next one's at 1.2 s), every stamp of the sealed
        // opening lies at or before the release and every one of the next
        // after it.
        let release = TimeUs(200 * MS);
        assert!(
            sealed.iter().any(|e| e.kind
                == InputKind::Control {
                    control: Q,
                    value: 1
                }),
            "{sealed:?}"
        );
        assert!(sealed.iter().all(|e| e.received <= release), "{sealed:?}");
        assert!(
            next.iter().all(|e| e.received.0 + 1_200 * MS >= release.0),
            "{next:?}"
        );
    }

    /// The same with START's press and release reached by one wake, the
    /// first after the opening, at 215 ms: the script is still fed one
    /// batch at a time, so it is held at the release's own 200 ms.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_hold_and_its_release_in_one_wake_hold_the_script_at_the_release() {
        let (mut sources, mut hook, mut link, gate, opening) = scripted();
        hook.wake(&mut sources, script_clock(215_000));
        assert_eq!(gate.state().phase, GatePhase::Cancelled);
        assert_eq!(
            hook.released,
            Some((TimeUs(200 * MS), script_clock(215_000)))
        );
        let mut readers = opening.readers;
        for r in &mut readers {
            while r.pop().is_some() {}
        }
        link.rearms
            .push(rearm_buffers(readers, opening.start.held.capacity()))
            .unwrap();
        hook.wake(&mut sources, script_clock(250_000));
        let rearm = link.rearmed.pop().unwrap();
        assert!(
            matches!(rearm.outcome, RearmOutcome::Opened { .. }),
            "{:?}",
            rearm.outcome
        );
        let mut readers = rearm.readers;
        feed_z(&mut hook, &mut sources, &mut readers, 50_000);
    }

    /// A rearm that opens nothing leaves the script's timeline as it was:
    /// the release is let go, the presentation offset is the script's
    /// origin again, and Z is fed at 1.7 s on the old timeline.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_refused_scripted_rearm_keeps_the_scripts_timeline() {
        let (mut sources, mut hook, mut link, _gate, _opening) = scripted();
        hook.wake(&mut sources, script_clock(-299_500));
        hook.wake(&mut sources, script_clock(200_500));
        assert!(hook.released.is_some());
        // No readers: the reopening is refused after the offset moved.
        link.rearms.push(rearm_buffers(Vec::new(), 4)).unwrap();
        hook.wake(&mut sources, script_clock(240_000));
        let rearm = link.rearmed.pop().unwrap();
        assert_eq!(rearm.outcome, RearmOutcome::Refused);
        assert_eq!(hook.released, None);
        assert_eq!(hook.script_origin, Some(script_clock(0)));
        assert_eq!(sources.presentation_time(TimeUs(0)), script_clock(0));
        hook.wake(&mut sources, script_clock(1_700_000));
        let Mode::Script(script) = &hook.mode else {
            panic!("not a script");
        };
        assert_eq!(script.current.next_at(), Some(TimeUs(1_750 * MS)));
    }

    /// Where the scripted play of [`scripted`] opens on the clock: 10 s.
    #[cfg(feature = "dev-tools")]
    const SCRIPT_OPENED: i64 = 10_000 * MS;

    /// Script time `us` on the clock: its 0 one lead-in after the opening.
    #[cfg(feature = "dev-tools")]
    const fn script_clock(us: i64) -> TimeUs {
        TimeUs(SCRIPT_OPENED + LEAD_IN.0 + us)
    }

    /// Feeds the script until Z's release on the clock, its timeline moved
    /// `moved` µs by the rearm, and checks that Z landed as the log has it:
    /// pressed at 500 ms and released at 550 ms of the rearmed READY.
    /// Returns what the readers held.
    #[cfg(feature = "dev-tools")]
    fn feed_z(
        hook: &mut Hook,
        sources: &mut SourceSet,
        readers: &mut [input::SourceReader],
        moved: i64,
    ) -> Vec<eval::PhysicalInputEvent> {
        for at in [1_700_000, 1_705_000 + moved, 1_755_000 + moved] {
            hook.wake(sources, script_clock(at));
        }
        let next: Vec<_> = readers
            .iter_mut()
            .flat_map(|r| std::iter::from_fn(move || r.pop()))
            .collect();
        let z = |value| InputKind::Control { control: Z, value };
        let at = |kind| next.iter().find(|e| e.kind == kind).map(|e| e.received);
        assert_eq!(at(z(1)), Some(TimeUs(500 * MS)), "{next:?}");
        assert_eq!(at(z(0)), Some(TimeUs(550 * MS)), "{next:?}");
        next
    }

    /// A scripted play's hook, opened at [`SCRIPT_OPENED`] on a keyboard
    /// that holds START at −300 ms and releases it at 200 ms, then presses
    /// Z at 1.7 s and releases it at 1.75 s, with a rearm delay of 30 ms;
    /// its START tap's gate, and the first opening.
    #[cfg(feature = "dev-tools")]
    fn scripted() -> (
        SourceSet,
        Hook,
        InputLink,
        StartGate,
        game::Opening<input::SourceReader>,
    ) {
        use input::{Action, ScriptItem, ScriptedInput};

        let config = SourceConfig {
            max_devices: 4,
            max_sources: 4,
            max_held: 4,
            queue_capacity: 16,
            max_arrivals: 4,
        };
        let item = |ms: i64, action| ScriptItem {
            at: TimeUs(ms * MS),
            action,
        };
        let key = |ms, control, value| item(ms, Action::Packet(packet(1, control, value)));
        let script = ScriptedInput::new(vec![
            item(-5_000, Action::Connect(DeviceId(1), DeviceKind::Keyboard)),
            key(-300, Q, 1),
            key(200, Q, 0),
            key(1_700, Z, 1),
            key(1_750, Z, 0),
        ])
        .with_recorded(DeviceId(1), vec![Q, Z]);
        let mode = Mode::Script(Box::new(super::Script {
            log: script.clone(),
            current: script,
            sources: config.clone(),
            loading: None,
            rearm_delay: TimeUs(30 * MS),
        }));
        let mut sources = SourceSet::new(config).unwrap();
        let controls = Arc::new(Controls::new(QpcClock::start()));
        controls.set_latch(sources.gate().handle());
        let (mut hook, link) = link(controls, mode, Waker::new().unwrap());
        let (tap, _rx) = adjustment_queue(AdjustmentBindings {
            controls: vec![(TapControl::Key(Q), 0)],
            latched: 1,
            ..AdjustmentBindings::default()
        })
        .unwrap();
        let gate = tap.start_gate();
        link.tap(tap);
        hook.wake(&mut sources, TimeUs(SCRIPT_OPENED - MS));
        link.requests
            .send(Request::Open {
                lead_in: LEAD_IN,
                logic: std::thread::current(),
                recorded: recorded(),
                gate: Some(1),
                focus: None,
            })
            .unwrap();
        hook.wake(&mut sources, TimeUs(SCRIPT_OPENED));
        let Ok(Reply::Opened(opening, _)) = link.replies.try_recv() else {
            panic!("the first READY did not open");
        };
        (sources, hook, link, gate, opening)
    }

    /// A loading script holding START from the start of loading to 600 ms
    /// (`--dev-loading`), on a scripted play whose log presses Z at 1.7 s
    /// and releases it at 1.75 s: the opening asked at 400 ms is deferred,
    /// the release is fed at 610 ms, and the opening asked again at 650 ms
    /// opens on the timeline of the one first asked, so Z lands 250 ms
    /// earlier on its chart time. Asked again later, the log's items before
    /// the opening are fed before recording opens and stay out of it: past
    /// Z's press only its release is recorded, past both nothing.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_deferred_opening_keeps_the_scripts_timeline() {
        let us = |ms: &[i64]| ms.iter().map(|&m| TimeUs(m * MS)).collect::<Vec<_>>();
        assert_eq!(deferred_opening(650), us(&[1_450, 1_500]));
        // Opened at 1.72 s on the script's timeline: its chart time 0 is
        // at 2.72 s.
        assert_eq!(deferred_opening(3_120), us(&[-970]));
        assert_eq!(deferred_opening(3_200), us(&[]));
    }

    /// The times of Z's events recorded in
    /// [`a_deferred_opening_keeps_the_scripts_timeline`] with the opening
    /// asked again `reopen` ms after loading started.
    #[cfg(feature = "dev-tools")]
    fn deferred_opening(reopen: i64) -> Vec<TimeUs> {
        use input::{Action, ScriptItem, ScriptedInput};

        const LOADING: i64 = 10_000 * MS;
        let config = SourceConfig {
            max_devices: 4,
            max_sources: 4,
            max_held: 4,
            queue_capacity: 16,
            max_arrivals: 4,
        };
        let item = |ms: i64, action| ScriptItem {
            at: TimeUs(ms * MS),
            action,
        };
        let key = |ms, control, value| item(ms, Action::Packet(packet(1, control, value)));
        let script = ScriptedInput::new(vec![
            item(-5_000, Action::Connect(DeviceId(1), DeviceKind::Keyboard)),
            key(1_700, Z, 1),
            key(1_750, Z, 0),
        ])
        .with_recorded(DeviceId(1), vec![Q, Z]);
        let loading = ScriptedInput::new(vec![
            item(0, Action::Connect(DeviceId(1), DeviceKind::Keyboard)),
            key(0, Q, 1),
            key(600, Q, 0),
        ]);
        let mode = Mode::Script(Box::new(super::Script {
            log: script.clone(),
            current: script,
            sources: config.clone(),
            loading: Some(loading),
            rearm_delay: TimeUs(0),
        }));
        let mut sources = SourceSet::new(config).unwrap();
        let controls = Arc::new(Controls::new(QpcClock::start()));
        controls.set_latch(sources.gate().handle());
        let (mut hook, link) = link(controls, mode, Waker::new().unwrap());
        let (tap, _rx) = adjustment_queue(AdjustmentBindings {
            controls: vec![(TapControl::Key(Q), 0)],
            latched: 1,
            ..AdjustmentBindings::default()
        })
        .unwrap();
        link.tap(tap);
        let ask = |hook: &mut Hook, sources: &mut SourceSet, ms: i64| {
            link.requests
                .send(Request::Open {
                    lead_in: LEAD_IN,
                    logic: std::thread::current(),
                    recorded: recorded(),
                    gate: Some(1),
                    focus: None,
                })
                .unwrap();
            hook.wake(sources, TimeUs(LOADING + ms * MS));
            link.replies.try_recv().unwrap()
        };
        link.loading_script(TimeUs(LOADING));
        hook.wake(&mut sources, TimeUs(LOADING));
        hook.wake(&mut sources, TimeUs(LOADING + 100 * MS));
        assert!(sources.holding());
        assert!(matches!(ask(&mut hook, &mut sources, 400), Reply::Deferred));
        hook.wake(&mut sources, TimeUs(LOADING + 610 * MS));
        assert!(!sources.holding());
        let Reply::Opened(opening, _) = ask(&mut hook, &mut sources, reopen) else {
            panic!("the opening asked again did not open");
        };
        // The script's time 0 is the first opening's chart time 0.
        let first_origin = LOADING + 400 * MS + LEAD_IN.0;
        let opened = LOADING + reopen * MS;
        for ms in [1_700, 1_760, 2_000] {
            let at = first_origin + ms * MS;
            if at > opened {
                hook.wake(&mut sources, TimeUs(at));
            }
        }
        let mut readers = opening.readers;
        readers
            .iter_mut()
            .flat_map(|r| std::iter::from_fn(move || r.pop()))
            .filter(|e| matches!(e.kind, InputKind::Control { control, .. } if control == Z))
            .map(|e| e.received)
            .collect()
    }

    /// The player's gate admits what its exchange with the input side
    /// keeps, by the capacities reserved at the opening: an attempt whose
    /// recording fills the reservation exactly with the history and the
    /// snapshots is refused once the exchange is counted.
    #[test]
    fn the_player_gates_exchange_counts_in_the_admission() {
        let (mut sources, mut hook, link, _) = rig();
        let (mut tap, _rx) = adjustment_queue(AdjustmentBindings {
            controls: vec![(TapControl::Key(Q), 0)],
            latched: 1,
            ..AdjustmentBindings::default()
        })
        .unwrap();
        tap.set_active(true);
        let mut gate = crate::start_gate::PlayerGate::install(&link, tap);
        let opening = open(&mut hook, &link, &mut sources, 10).unwrap();
        gate.opened(1, &opening);
        assert!(
            gate.exchange()
                >= replay::MAX_CANCELLED_EVENTS * size_of::<eval::PhysicalInputEvent>()
                    + replay::MAX_OPENING_ARRIVALS * size_of::<input::Arrival>(),
            "{}",
            gate.exchange()
        );
        let base = game::Reservation {
            fixed: 4_096,
            per_input: 1,
        };
        let without = base.with_gate(game::GateStorage {
            held: gate.snapshot(),
            exchange: 0,
        });
        let with = base.with_gate(game::GateStorage {
            held: gate.snapshot(),
            exchange: gate.exchange(),
        });
        let inputs = without.capacity();
        assert!(without.fits(inputs));
        assert!(!with.fits(inputs), "the exchange was not admitted");
    }

    /// The same refusal through the player's gate, with one event of room
    /// left in the history and two arriving between its drain and the
    /// input side's boundary: the capture is sealed, and the history the
    /// record saves keeps the one that fits.
    #[test]
    fn a_refused_rearm_keeps_in_the_capture_what_fits_the_history() {
        let mut sources = SourceSet::new(SourceConfig {
            max_devices: 4,
            max_sources: 4,
            max_held: 4,
            queue_capacity: 16,
            max_arrivals: 4,
        })
        .unwrap();
        sources.begin_batch(TimeUs(0));
        sources.connect(DeviceId(1), DeviceKind::Keyboard);
        sources.connect(DeviceId(2), DeviceKind::Keyboard);
        sources.end_batch(TimeUs(1));
        let controls = Arc::new(Controls::new(QpcClock::start()));
        controls.set_latch(sources.gate().handle());
        let (mut hook, mut link) = link(controls, Mode::Clock, Waker::new().unwrap());
        let (mut tap, _rx) = adjustment_queue(AdjustmentBindings {
            controls: vec![(TapControl::Key(Q), 0)],
            latched: 1,
            ..AdjustmentBindings::default()
        })
        .unwrap();
        tap.set_active(true);
        let mut gate = crate::start_gate::PlayerGate::install(&link, tap);
        let opening = open(&mut hook, &link, &mut sources, 10).unwrap();
        gate.opened(1, &opening);
        batch(&mut hook, &mut sources, 20, &[packet(1, Q, 1)]);
        assert_eq!(gate.state().phase, GatePhase::Cancelled);
        batch(&mut hook, &mut sources, 40, &[packet(1, Q, 0)]);
        // The history: the cancelled opening begun, one event of room left.
        let mut history = game::HistoryRecorder::new(4);
        history.begin(&opening.start, opening.origin).unwrap();
        let filler = eval::PhysicalInputEvent {
            received: TimeUs(0),
            source: SourceId(0),
            kind: InputKind::Disconnected,
        };
        history
            .take(&vec![filler; replay::MAX_CANCELLED_EVENTS - 1])
            .unwrap();
        assert_eq!(history.room(), 1);
        let mut readers = opening.readers;
        let mut start = InputStart {
            opened_at: TimeUs(0),
            held: Vec::with_capacity(opening.start.held.capacity()),
        };
        let answer = std::thread::scope(|s| {
            // The input thread: Z pressed and released after the game
            // thread's drain, at the boundary that takes the rearm.
            s.spawn(|| {
                while hook.rearms.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                batch(
                    &mut hook,
                    &mut sources,
                    50,
                    &[packet(2, Z, 1), packet(2, Z, 0)],
                );
            });
            gate.rearm(
                &mut link,
                &mut readers,
                LEAD_IN,
                2,
                &mut history,
                &mut start,
            )
        });
        assert_eq!(answer, Ok(game::Rearm::Held));
        assert!(history.exhausted(), "the capture was not sealed");
        let saved = history.history(opening.origin);
        assert_eq!(
            saved.events.last().map(|e| (e.source, e.kind)),
            Some((
                SourceId(1),
                InputKind::Control {
                    control: Z,
                    value: 1
                }
            )),
            "the event that fits was dropped"
        );
    }

    /// A completed play's handoff, served at the input thread's boundary
    /// while the game thread waits: the closing comes back in the storage
    /// reserved at the opening, and the boundaries allocate nothing; the
    /// start gate is disarmed, and the queues go on taking input, a key
    /// held through the handoff released with no press repeated, until the
    /// close after the tail.
    #[test]
    fn a_handoff_keeps_the_sources_until_the_close_alloc() {
        let (mut sources, mut hook, mut link, gate) = rig();
        let opening = open(&mut hook, &link, &mut sources, 10).unwrap();
        assert_eq!(gate.state().phase, GatePhase::Armed);
        let mut readers = opening.readers;
        batch(&mut hook, &mut sources, 20, &[packet(2, Z, 1)]);
        for r in &mut readers {
            while r.pop().is_some() {}
        }
        #[cfg(feature = "dev-tools")]
        {
            let control = crate::alloc_count::this_thread();
            drop(std::hint::black_box(vec![1_u8; 8]));
            assert!(
                crate::alloc_count::this_thread() > control,
                "the allocation was not counted"
            );
        }
        // Each side's own allocations (counted with dev-tools only): the
        // input thread's boundaries, the thread's spawn aside, and the game
        // thread's whole handoff, each thread with its positive control.
        let count = || {
            #[cfg(feature = "dev-tools")]
            return crate::alloc_count::this_thread();
            #[cfg(not(feature = "dev-tools"))]
            0
        };
        let control = || {
            #[cfg(feature = "dev-tools")]
            {
                let before = count();
                drop(std::hint::black_box(vec![1_u8; 8]));
                assert!(count() > before, "the allocation was not counted");
            }
        };
        let mut allocated = 0;
        let (closing, asking) = std::thread::scope(|s| {
            let asked = s.spawn(|| {
                control();
                let before = count();
                let closing = link.hand_off();
                (closing, count() - before)
            });
            let mut at = 30;
            while !asked.is_finished() {
                let before = count();
                batch(&mut hook, &mut sources, at, &[]);
                allocated += count() - before;
                at += 1;
            }
            asked.join().unwrap()
        });
        let closing = closing.unwrap();
        assert_eq!(allocated, 0, "the input thread's handoff allocated");
        assert_eq!(asking, 0, "the game thread's handoff allocated");
        assert_eq!(
            closing,
            game::Closing {
                arrivals: Vec::new(),
                overflowed: false,
                source_limited: false,
                axis_out_of_range: vec![0, 0],
            }
        );
        assert!(sources.is_recording() && sources.is_handed_off());
        assert_ne!(gate.state().phase, GatePhase::Armed);
        batch(&mut hook, &mut sources, 100, &[packet(2, Z, 0)]);
        let tail: Vec<_> = readers[1].pop().into_iter().map(|e| e.kind).collect();
        assert_eq!(
            tail,
            [InputKind::Control {
                control: Z,
                value: 0
            }]
        );
        assert!(readers.iter().all(input::SourceReader::is_empty));
        // The close after the tail: nothing more reaches the queues.
        link.requests.send(Request::Close).unwrap();
        batch(&mut hook, &mut sources, 110, &[]);
        assert!(matches!(link.replies.try_recv(), Ok(Reply::Closed(..))));
        assert!(!sources.is_recording());
        batch(&mut hook, &mut sources, 120, &[packet(2, Z, 1)]);
        assert!(readers.iter().all(input::SourceReader::is_empty));
    }

    /// Asks `ask` of the link on a thread of its own, as the game thread
    /// does, while the input thread's batches run from `at` ms one per
    /// millisecond until it is answered.
    fn served<R: Send>(
        hook: &mut Hook,
        sources: &mut SourceSet,
        link: &mut InputLink,
        at: i64,
        ask: impl FnOnce(&mut InputLink) -> R + Send,
    ) -> R {
        std::thread::scope(|s| {
            let asked = s.spawn(|| ask(link));
            let mut at = at;
            while !asked.is_finished() {
                batch(hook, sources, at, &[]);
                at += 1;
            }
            asked.join().unwrap()
        })
    }

    /// An aborted play's close is answered at the input thread's next
    /// boundary with the closing in the storage reserved at the opening:
    /// its arrivals come back in the reserved buffer, capped at its
    /// capacity, and the recording is sealed there, not closed.
    #[test]
    fn an_abort_close_is_answered_in_the_reserved_storage() {
        let (mut sources, mut hook, mut link, _gate) = rig();
        let opening = open(&mut hook, &link, &mut sources, 10).unwrap();
        let reserved = hook.handoff.as_ref().unwrap();
        let (buffer, room) = (
            reserved.closing.arrivals.as_ptr(),
            reserved.closing.arrivals.capacity(),
        );
        assert_eq!(room, 4);
        // More devices arrive than the reservation holds.
        sources.begin_batch(TimeUs(20 * MS));
        for d in 3..9 {
            sources.connect(DeviceId(d), DeviceKind::Keyboard);
        }
        sources.end_batch(TimeUs(20 * MS + 1));
        hook.wake(&mut sources, TimeUs(20 * MS + 1));
        let closing = served(
            &mut hook,
            &mut sources,
            &mut link,
            30,
            InputLink::abort_close,
        );
        let closing = closing.unwrap();
        assert_eq!(closing.arrivals.as_ptr(), buffer);
        assert_eq!(
            (closing.arrivals.len(), closing.arrivals.capacity()),
            (room, room)
        );
        assert!(hook.handoff.is_none());
        assert!(sources.is_recording() && sources.is_sealed());
        // The game thread's reply channel is untouched: the exchange went
        // through the bounded queues.
        assert!(link.replies.try_recv().is_err());
        drop(opening);
    }

    /// After the seal nothing more reaches the play's queues, a removal
    /// included, and the start gate is disarmed; the sources and what the
    /// record keeps stay as sealed until the final close, while every
    /// device's held state goes on following its packets, so the next
    /// opening's snapshot holds what is held then.
    #[test]
    fn a_sealed_recording_pushes_nothing_and_keeps_the_held_state() {
        let (mut sources, mut hook, mut link, gate) = rig();
        let opening = open(&mut hook, &link, &mut sources, 10).unwrap();
        assert_eq!(gate.state().phase, GatePhase::Armed);
        let mut readers = opening.readers;
        // Z held on keyboard 2 through the seal.
        batch(&mut hook, &mut sources, 20, &[packet(2, Z, 1)]);
        for r in &mut readers {
            while r.pop().is_some() {}
        }
        let closing = served(
            &mut hook,
            &mut sources,
            &mut link,
            30,
            InputLink::abort_close,
        );
        let closing = closing.unwrap();
        assert!(closing.arrivals.is_empty());
        assert!(sources.is_sealed());
        assert_ne!(gate.state().phase, GatePhase::Armed);
        // Z released on keyboard 2, pressed on keyboard 1; a device
        // arrives, and keyboard 1 is pressed again past its queue's room.
        batch(&mut hook, &mut sources, 100, &[packet(2, Z, 0)]);
        batch(&mut hook, &mut sources, 101, &[packet(1, Z, 1)]);
        sources.begin_batch(TimeUs(102 * MS));
        sources.connect(DeviceId(3), DeviceKind::Keyboard);
        sources.end_batch(TimeUs(102 * MS + 1));
        for i in 0..40 {
            batch(&mut hook, &mut sources, 103 + i, &[packet(1, Z, 1)]);
        }
        assert!(readers.iter().all(input::SourceReader::is_empty));
        assert_eq!(sources.participants().count(), 2);
        assert_eq!(sources.devices().count(), 3);
        // The final close: what the record keeps is as sealed.
        link.requests.send(Request::Close).unwrap();
        batch(&mut hook, &mut sources, 200, &[]);
        match link.replies.try_recv() {
            Ok(Reply::Closed(c, lines)) => {
                assert_eq!(c.arrivals, closing.arrivals);
                assert!(!c.overflowed);
                assert!(lines.is_empty());
            }
            _ => panic!("a close is answered by a closing"),
        }
        assert!(!sources.is_recording());
        // The next opening's snapshot: Z held on keyboard 1, released on
        // keyboard 2.
        let next = open(&mut hook, &link, &mut sources, 300).unwrap();
        assert_eq!(next.start.held, [(SourceId(0), Z, 1)]);
    }

    /// The out-of-range warnings wait for the final close, which prints
    /// them once with what was counted before the seal, and closes
    /// recording.
    #[test]
    fn the_final_close_after_an_abort_close_prints_the_warnings_once() {
        let (mut sources, mut hook, mut link, _gate) = rig();
        let _opening = open(&mut hook, &link, &mut sources, 10).unwrap();
        let cap =
            input::hid::axis_cap(input::hid::axis_control(0x01, 0x30), true, 0, 255, 8).unwrap();
        sources.axis_value(DeviceId(1), &cap, 300);
        sources.axis_value(DeviceId(1), &cap, -1);
        let closing = served(
            &mut hook,
            &mut sources,
            &mut link,
            20,
            InputLink::abort_close,
        );
        assert_eq!(closing.unwrap().axis_out_of_range, [2, 0]);
        assert!(link.replies.try_recv().is_err(), "the abort close printed");
        // Counted no more once sealed.
        sources.axis_value(DeviceId(1), &cap, 400);
        let declared = "input: source 0 reported 2 axis values outside the range its controller declares; set its scratch range in device.toml";
        let close = |hook: &mut Hook, sources: &mut SourceSet, at| {
            link.requests.send(Request::Close).unwrap();
            batch(hook, sources, at, &[]);
            match link.replies.try_recv() {
                Ok(Reply::Closed(_, lines)) => lines,
                _ => panic!("a close is answered by a closing"),
            }
        };
        assert_eq!(close(&mut hook, &mut sources, 30), [declared]);
        assert!(!sources.is_recording());
        assert!(close(&mut hook, &mut sources, 40).is_empty());
    }

    /// Nothing is allocated on either thread for an aborted play's close:
    /// the game thread's request and wait, and the input thread's
    /// boundaries that answer it and seal the recording, each thread with
    /// a positive control proving its counter counts. The counter is the
    /// development builds' allocator, so the test runs with dev-tools.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn abort_close_alloc() {
        use crate::alloc_count::this_thread;
        let control = || {
            let before = this_thread();
            drop(std::hint::black_box(vec![1_u8; 8]));
            assert!(this_thread() > before, "the allocation was not counted");
        };
        let (mut sources, mut hook, mut link, _gate) = rig();
        let opening = open(&mut hook, &link, &mut sources, 10).unwrap();
        let mut readers = opening.readers;
        batch(&mut hook, &mut sources, 20, &[packet(2, Z, 1)]);
        for r in &mut readers {
            while r.pop().is_some() {}
        }
        control();
        let mut allocated = 0;
        let (closing, asking) = std::thread::scope(|s| {
            let asked = s.spawn(|| {
                control();
                let before = this_thread();
                let closing = link.abort_close();
                (closing, this_thread() - before)
            });
            let mut at = 30;
            while !asked.is_finished() {
                let before = this_thread();
                batch(&mut hook, &mut sources, at, &[packet(2, Z, 0)]);
                allocated += this_thread() - before;
                at += 1;
            }
            asked.join().unwrap()
        });
        assert!(closing.is_ok());
        assert!(sources.is_sealed());
        assert_eq!(allocated, 0, "the input thread's abort close allocated");
        assert_eq!(asking, 0, "the game thread's abort close allocated");
    }
}

/// C16: the select screen's panels through `Hook::wake` as the input
/// thread calls it, with the production key configs (START on E, SELECT on
/// W) and the controller's menu table: START, both, SELECT, none; a release
/// lost to a focus loss, a device's removal, menu events overflowing their
/// queue, and leaving Idle for a play and coming back never keep a panel
/// open, and a focus loss between two wakes restarts one; a panel still held at the next entry opens with a fresh start.
/// The timers' starts are the select screen's update's, at the times the
/// game thread reads the level.
#[cfg(test)]
mod panel_tests {
    use std::sync::Arc;

    use bms_model::TimeUs;
    use eval::ControlId;
    use game::{AdjustmentConfig, DeviceSettings, KeyConfigs};
    use input::hid::HidProduct;
    use input::{
        DeviceId, DeviceKind, MenuEvent, MenuReceiver, Packet, PanelLevel, QpcClock, SourceConfig,
        SourceSet, Waker,
    };

    use super::autoplay_tests::{LEAD_IN, recorded};
    use super::{Hook, InputLink, Mode, Reply, Request, link};
    use crate::controls::Controls;
    use crate::selection_skin::SkinFeed;

    const E: ControlId = ControlId(0x12);
    const W: ControlId = ControlId(0x11);
    const KEYBOARD: DeviceId = DeviceId(1);
    const PAD: DeviceId = DeviceId(7);
    const PRODUCT: HidProduct = HidProduct {
        vendor_id: 0x1A2B,
        product_id: 0x3C4D,
        interface: None,
    };

    struct Rig {
        controls: Arc<Controls>,
        hook: Hook,
        link: InputLink,
        sources: SourceSet,
        level: PanelLevel,
        menu: MenuReceiver,
        feed: SkinFeed,
        screen: crate::select::SelectScreen,
        now: i64,
    }

    impl Rig {
        /// The default keyboard and a controller whose button 1 plays key
        /// 1 and button 8 the scratch, planned and tapped as the player
        /// installs them;
        /// the thread Idle with focus, selection entered at 1 ms.
        fn new() -> Self {
            let mut device = DeviceSettings::default();
            device.controller.lanes = vec![
                game::LaneKey {
                    control: 1,
                    lane: 1,
                },
                game::LaneKey {
                    control: 8,
                    lane: 0,
                },
            ];
            device.controller_device = Some(PRODUCT.into());
            let keys = KeyConfigs::from(&device);
            let plan = AdjustmentConfig::default().plan(&keys, None, None).unwrap();
            let options =
                game::PlayOptions::new(game::HiSpeedSettings::default(), game::Covers::default())
                    .unwrap();
            let (_driver, tap, _reader) = game::AdjustDriver::new(&plan, options, 0).unwrap();
            let level = tap.panel_level();
            let mut sources = SourceSet::new(SourceConfig {
                max_devices: 4,
                max_sources: 4,
                max_held: 16,
                queue_capacity: 64,
                max_arrivals: 2,
            })
            .unwrap();
            sources.begin_batch(TimeUs(0));
            sources.connect(KEYBOARD, DeviceKind::Keyboard);
            sources.connect(PAD, DeviceKind::Controller);
            sources.set_product(PAD, PRODUCT);
            sources.end_batch(TimeUs(0));
            let bindings =
                crate::select::menu_bindings(&device.controller, Some(PRODUCT), &device.scratch)
                    .unwrap();
            let (menu_tap, menu) = input::menu_queue(bindings);
            sources.set_menu(menu_tap);
            let controls = Arc::new(Controls::new(QpcClock::start()));
            controls.set_latch(sources.gate().handle());
            controls.set_game(std::thread::current());
            sources.set_presentation_focus(controls.presentation_focus().clone());
            sources.set_adjustment(tap);
            let (hook, link) = link(Arc::clone(&controls), Mode::Clock, Waker::new().unwrap());
            let mut rig = Self {
                controls,
                hook,
                link,
                sources,
                level,
                menu,
                feed: SkinFeed::default(),
                screen: crate::select::SelectScreen::new(Vec::new()),
                now: 0,
            };
            rig.link.requests.send(Request::Idle).unwrap();
            rig.wake();
            rig.feed.enter(TimeUs(1_000));
            rig
        }

        /// A boundary with no batch before it.
        fn wake(&mut self) {
            self.now += 1_000;
            self.hook.wake(&mut self.sources, TimeUs(self.now));
        }

        /// One batch of `packets`, each `(device, control, value)`, then
        /// its boundary.
        fn batch(&mut self, packets: &[(DeviceId, ControlId, i32)], background: bool) {
            self.now += 1_000;
            self.sources.begin_batch(TimeUs(self.now));
            for &(device, control, value) in packets {
                self.sources.packet(&Packet {
                    device,
                    control,
                    value,
                    background,
                });
            }
            self.sources.end_batch(TimeUs(self.now));
            self.hook.wake(&mut self.sources, TimeUs(self.now));
        }

        /// The game thread's look at `at`: the level read, and the
        /// update's panel timers.
        fn look(&mut self, at: i64) -> [Option<TimeUs>; 3] {
            self.feed.panels(self.level.read(), false, TimeUs(at));
            let page = self.screen.page("");
            self.feed
                .update(&self.screen, page, TimeUs(at))
                .events
                .panels
        }
    }

    fn key(control: ControlId, value: i32) -> (DeviceId, ControlId, i32) {
        (KEYBOARD, control, value)
    }

    #[test]
    fn panels_follow_start_and_select_and_no_lost_release_keeps_one() {
        let mut r = Rig::new();
        let t = |ms: i64| Some(TimeUs(ms * 1_000));
        assert_eq!(r.look(5_000), [None; 3]);
        // START: panel 1, the game thread woken for it; START and
        // SELECT: panel 3; START released: panel 2; SELECT released: none.
        std::thread::park_timeout(std::time::Duration::ZERO);
        r.batch(&[key(E, 1)], false);
        let began = std::time::Instant::now();
        std::thread::park_timeout(std::time::Duration::from_secs(60));
        assert!(
            began.elapsed() < std::time::Duration::from_secs(30),
            "the game thread was not woken"
        );
        assert_eq!(r.look(10_000), [t(10), None, None]);
        assert_eq!(r.look(11_000), [t(10), None, None], "still open");
        r.batch(&[key(W, 1)], false);
        assert_eq!(r.look(20_000), [None, None, t(20)]);
        r.batch(&[key(E, 0)], false);
        assert_eq!(r.look(30_000), [None, t(30), None]);
        r.batch(&[key(W, 0)], false);
        assert_eq!(r.look(40_000), [None; 3]);
        // Released and held again between two looks: the same panel,
        // opened afresh.
        r.batch(&[key(E, 1)], false);
        assert_eq!(r.look(41_000), [t(41), None, None]);
        r.batch(&[key(E, 0)], false);
        r.batch(&[key(E, 1)], false);
        assert_eq!(r.look(42_000), [t(42), None, None]);
        // Held at an entry that never left Idle (back from the practice
        // page): open from that entry.
        r.feed.enter(TimeUs(43_000));
        assert_eq!(r.look(43_000), [t(43), None, None]);
        r.batch(&[key(E, 0)], false);
        assert_eq!(r.look(44_000), [None; 3]);

        // Menu events overflowing their queue while START is held: the
        // controller's events say panel 1, and the release closes it.
        r.batch(&[key(E, 1)], false);
        let mut presses = Vec::new();
        for _ in 0..=input::MENU_QUEUE {
            presses.extend([(PAD, ControlId(1), 1), (PAD, ControlId(1), 0)]);
        }
        r.batch(&presses, false);
        assert!(r.menu.dropped() > 0, "overflowed");
        assert_eq!(r.menu.pop_panel(), Some((MenuEvent::Confirm, Some(0))));
        assert_eq!(r.look(50_000), [t(50), None, None]);
        r.batch(&[key(E, 0)], false);
        assert_eq!(r.look(60_000), [None; 3]);

        // Focus lost while START is held, released in the background:
        // closed at the loss, and closed when focus comes back.
        r.batch(&[key(E, 1)], false);
        assert_eq!(r.look(70_000), [t(70), None, None]);
        r.controls.focus(false);
        r.wake();
        assert_eq!(r.look(80_000), [None; 3]);
        r.batch(&[key(E, 0)], true);
        r.controls.focus(true);
        r.wake();
        assert_eq!(r.look(90_000), [None; 3]);
        // Held through a focus loss: open again at its return, fresh.
        r.batch(&[key(E, 1)], false);
        r.controls.focus(false);
        r.wake();
        assert_eq!(r.look(100_000), [None; 3]);
        r.controls.focus(true);
        r.wake();
        assert_eq!(r.look(110_000), [t(110), None, None]);
        // Focus lost and back between two wakes, START held throughout:
        // the loss closed the panel, which opens again afresh.
        r.controls.focus(false);
        r.controls.focus(true);
        r.wake();
        assert_eq!(r.look(115_000), [t(115), None, None]);

        // The keyboard removed while it holds START: closed.
        r.sources.begin_batch(TimeUs(r.now + 500));
        r.sources.disconnect(KEYBOARD);
        r.sources.end_batch(TimeUs(r.now + 500));
        r.wake();
        assert_eq!(r.look(120_000), [None; 3]);
        r.sources.begin_batch(TimeUs(r.now + 500));
        r.sources.connect(KEYBOARD, DeviceKind::Keyboard);
        r.sources.end_batch(TimeUs(r.now + 500));

        // Held while the thread leaves Idle for a play (a chart chosen),
        // released before the play ends: closed at the opening and at the
        // next entry.
        r.batch(&[key(E, 1)], false);
        assert_eq!(r.look(130_000), [t(130), None, None]);
        r.open();
        assert_eq!(r.look(140_000), [None; 3]);
        r.batch(&[key(E, 0)], false);
        r.close();
        r.link.requests.send(Request::Idle).unwrap();
        r.wake();
        r.feed.enter(TimeUs(150_000));
        assert_eq!(r.look(150_000), [None; 3]);

        // Held across the play and still held at the next entry: open at
        // the entry, its start the entry's, not the press's.
        r.batch(&[key(E, 1)], false);
        assert_eq!(r.look(160_000), [t(160), None, None]);
        r.open();
        r.close();
        r.link.requests.send(Request::Idle).unwrap();
        r.wake();
        r.feed.enter(TimeUs(170_000));
        assert_eq!(r.look(170_000), [t(170), None, None]);
    }

    impl Rig {
        /// Recording opened at the next boundary, as a chosen chart does.
        fn open(&mut self) {
            self.link
                .requests
                .send(Request::Open {
                    lead_in: LEAD_IN,
                    logic: std::thread::current(),
                    recorded: recorded(),
                    gate: None,
                    focus: None,
                })
                .unwrap();
            self.wake();
            assert!(matches!(
                self.link.replies.try_recv(),
                Ok(Reply::Opened(..))
            ));
        }

        /// Recording closed at the next boundary: the thread's tail.
        fn close(&mut self) {
            self.link.requests.send(Request::Close).unwrap();
            self.wake();
            assert!(matches!(
                self.link.replies.try_recv(),
                Ok(Reply::Closed(..))
            ));
        }
    }
}
