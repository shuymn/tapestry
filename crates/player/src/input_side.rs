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

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::Thread;

use bms_model::TimeUs;
use eval::ControlId;
use game::{Closing, Opening, ScratchConfig};
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
    /// How the controller's bound axes are converted, for the range their
    /// values are counted against.
    pub scratch: ScratchConfig,
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
    #[expect(
        clippy::expect_used,
        reason = "the documented panic: the input thread outlives every play"
    )]
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
    #[expect(
        clippy::expect_used,
        reason = "the documented panic: the input thread outlives every play"
    )]
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
    /// One boundary, `now` being the clock read there. Generated input or
    /// a script is first fed up to the play's time; between plays,
    /// autoplay's wake still checks the focus gate, so a focus loss during
    /// the audio tail or on the end screen is published.
    pub fn wake(&mut self, sources: &mut SourceSet, now: TimeUs) {
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
                let start = sources.open_play(now, origin);
                // Devices the source limit left out: keyboards, when any
                // is (controllers come first), possibly one with lanes.
                let n = sources.left_out();
                if n > 0 {
                    eprintln!(
                        "input: {n} connected device(s) take no part in this play: more devices than sources; a keyboard left out records nothing, and the log is marked incomplete"
                    );
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
                #[expect(
                    clippy::expect_used,
                    reason = "the run's source set was made with these sizes"
                )]
                {
                    *sources = SourceSet::new(script.sources.clone())
                        .expect("the run's source set was made with these sizes");
                }
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
        for line in out_of_range_warnings(sources) {
            eprintln!("{line}");
        }
        sources.close_recording();
        self.origin = None;
        closed
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
        SourceSet,
    };

    use super::{Hook, InputLink, Mode, Recorded, Reply, Request, link};
    use crate::controls::Controls;

    const MS: i64 = 1_000;
    const LEAD_IN: TimeUs = TimeUs(1_000 * MS);
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

    fn recorded() -> Recorded {
        Recorded {
            keyboard: vec![Z],
            controller: vec![ControlId(1)],
            controller_device: None,
            scratch: ScratchConfig::default(),
        }
    }

    fn set() -> SourceSet {
        SourceSet::new(SourceConfig {
            max_devices: 4,
            max_sources: 4,
            max_held: 4,
            queue_capacity: 8,
            max_arrivals: 2,
        })
        .unwrap()
    }

    /// The hook of an autoplay session and its link, with the window's
    /// controls holding the source set's latch before any play opens.
    fn hooked(sources: &SourceSet) -> (Arc<Controls>, Hook, InputLink) {
        let controls = Arc::new(Controls::new(QpcClock::start()));
        controls.set_latch(sources.gate().handle());
        let (hook, link) = link(
            Arc::clone(&controls),
            Mode::Autoplay(AutoplayInput::new(events())),
        );
        (controls, hook, link)
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
            Reply::Opened(o) => o,
            Reply::Closed(_) => panic!("an open is answered by an opening"),
        }
    }

    fn close(hook: &mut Hook, link: &InputLink, sources: &mut SourceSet, at: i64) {
        link.requests.send(Request::Close).unwrap();
        hook.wake(sources, TimeUs(at));
        assert!(matches!(link.replies.try_recv(), Ok(Reply::Closed(_))));
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
}
