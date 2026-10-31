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
//! The hook reports the thread's phase (`input::cadence`): Open from an
//! open, Tail from a close, and Idle only from [`Request::Idle`], which the
//! game thread sends once a play's audio tail is over. At each boundary
//! the hook also sets the controller's menu active only while Idle with the
//! window focused, and wakes the game thread when the batch sent a menu
//! event (design.md "Input").
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
        /// The countdown's length.
        lead_in: TimeUs,
        /// The thread to wake after each publication.
        logic: Thread,
        /// What the play records.
        recorded: Recorded,
    },
    /// Close recording at the next boundary.
    Close,
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
}

/// What the input thread publishes between requests.
#[derive(Debug, Default)]
pub struct InputState {
    tripped: AtomicBool,
    connected: AtomicBool,
    /// The hook was dropped: the input thread ended, or is unwinding.
    gone: AtomicBool,
    /// The hook's wakes so far (dev-tools).
    #[cfg(feature = "dev-tools")]
    wakes: std::sync::atomic::AtomicU64,
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
    /// The first handshake's failure: once set, no request is sent and no
    /// reply read, so a late reply to a request that timed out is never
    /// taken for the answer to another.
    failed: Option<InputWaitError>,
}

impl InputLink {
    /// Opens recording for a play.
    ///
    /// # Errors
    ///
    /// [`InputWaitError`] if the input thread is gone or did not answer
    /// within [`INPUT_TIMEOUT`], now or at an earlier handshake.
    pub fn open(&mut self, request: Request) -> Result<Opening<SourceReader>, InputWaitError> {
        match self.ask(request)? {
            Reply::Opened(o, lines) => {
                print_lines(&lines);
                Ok(o)
            }
            Reply::Closed(..) => unreachable!("an open is answered by an opening"),
        }
    }

    /// Closes recording.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub fn close(&mut self) -> Result<Closing, InputWaitError> {
        match self.ask(Request::Close)? {
            Reply::Closed(c, lines) => {
                print_lines(&lines);
                Ok(c)
            }
            Reply::Opened(..) => unreachable!("a close is answered by a closing"),
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
        if self.failed.is_none() && self.requests.send(feed).is_ok() {
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
        if self.failed.is_none() && self.requests.send(switch).is_ok() {
            self.waker.wake();
        }
    }

    /// Lets the input thread go idle: sent without waiting, since nothing
    /// answers it. A failed link sends nothing.
    pub fn idle(&self) {
        if self.failed.is_none() && self.requests.send(Request::Idle).is_ok() {
            self.waker.wake();
        }
    }

    /// Sends `request` and waits for its reply. Every request is answered
    /// once, in order, so with the link failed at the first wait that does
    /// not end in a reply, the reply read is always this request's.
    fn ask(&mut self, request: Request) -> Result<Reply, InputWaitError> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let reply = self.wait(request);
        if let Err(e) = reply {
            self.failed = Some(e);
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
        self.failed.or(self
            .state
            .gone
            .load(Ordering::Acquire)
            .then_some(InputWaitError::Gone))
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
}

/// A linked hook and link; the link wakes the input thread through
/// `waker`.
pub fn link(controls: Arc<Controls>, mode: Mode, waker: Waker) -> (Hook, InputLink) {
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
            phase: Phase::Tail,
        },
        InputLink {
            requests: req_tx,
            replies: rep_rx,
            state,
            waker,
            timeout: INPUT_TIMEOUT,
            awake: awake_time,
            failed: None,
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
            #[cfg(feature = "dev-tools")]
            (Mode::Script(script), Some(origin)) => {
                input::InputAdapter::poll(&mut script.current, TimeUs(now.0 - origin.0), sources);
            }
            _ => {}
        }
        while let Ok(request) = self.requests.try_recv() {
            let reply = match request {
                Request::Open {
                    lead_in,
                    logic,
                    recorded,
                } => {
                    let (opening, lines) = self.open(sources, now, lead_in, logic, &recorded);
                    self.phase = Phase::Open;
                    Reply::Opened(opening, lines)
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
    fn menu(&self, sources: &mut SourceSet) {
        let active = self.phase == Phase::Idle && self.controls.focused();
        if let Some(menu) = sources.menu_mut() {
            menu.set_active(active);
            if menu.take_sent() {
                self.controls.wake_game();
            }
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
    ) -> (Opening<SourceReader>, Vec<String>) {
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
                let start = sources.open_play(now, origin);
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
                sources.open_recording(TimeUs(-lead_in.0))
            }
            #[cfg(feature = "dev-tools")]
            Mode::Script(script) => {
                // The log again from its start, on devices as they were
                // before it: they connect, with their recorded controls,
                // before the opening.
                script.current = script.log.clone();
                // The same gate: the latch the controls hold trips it.
                #[expect(
                    clippy::expect_used,
                    reason = "the run's source set was made with these sizes"
                )]
                sources
                    .reset(script.sources.clone())
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
        (opening, lines)
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
        (closed, lines)
    }
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
            })
            .unwrap();
        hook.wake(sources, TimeUs(at));
        match link.replies.try_recv().unwrap() {
            Reply::Opened(o, _) => o,
            Reply::Closed(..) => panic!("an open is answered by an opening"),
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
        let (_controls, mut hook, mut link) = hooked(&sources);
        link.failed = Some(game::InputWaitError::Timeout);
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
        let begun = Instant::now();
        assert_eq!(link.close().err(), Some(InputWaitError::Timeout));
        assert!(begun.elapsed() >= SHORT, "{:?}", begun.elapsed());
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
