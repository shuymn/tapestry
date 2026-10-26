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
    /// Whether `device.toml` sets the range the controller's axis values
    /// are counted against in the open recording. Only the controller has
    /// axes, so only its values are counted.
    range_set: bool,
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
            range_set: false,
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
        self.range_set = matches!(self.mode, Mode::Clock) && recorded.scratch.overrides_range();
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
            if let Some(line) = out_of_range_warning(i, n, self.range_set) {
                eprintln!("{line}");
            }
        }
        sources.close_recording();
        self.origin = None;
        closed
    }
}

/// The console line for source `source` having reported `n` axis values
/// outside the range their conversion uses, naming where that range comes
/// from: `device.toml` when `set`, else the controller's declaration.
/// `None` when nothing was outside.
fn out_of_range_warning(source: usize, n: u64, set: bool) -> Option<String> {
    if n == 0 {
        return None;
    }
    Some(if set {
        format!(
            "input: source {source} reported {n} axis values outside the scratch range device.toml sets; correct it there or capture the scratch again in `tapestry keys`"
        )
    } else {
        format!(
            "input: source {source} reported {n} axis values outside the range its controller declares; set its scratch range in device.toml"
        )
    })
}

/// Sets what each connected device records and whether it takes part:
/// every keyboard records the keyboard controls, and the first controller
/// of the configured model ([`Recorded::controller_device`]) the
/// controller's; any other controller, a second of that model included,
/// takes no part. The controller's bound axes are counted against the
/// range their conversion uses ([`ScratchConfig::range`]). Returns `false`
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
                        .map(|c| (c.control, recorded.scratch.range(c)))
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
    use input::hid::{HidProduct, axis_cap, axis_control};
    use input::{DeviceId, DeviceKind, Packet, SourceConfig, SourceSet};

    use super::{Recorded, out_of_range_warning, take_part};

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

    /// The configured controller's bound axis is counted against the range
    /// its conversion uses: the declared 0..=127 without an override, the
    /// 256-value revolution `device.toml` sets (0..=255) with one, and no
    /// range when it sets the axis relative. An unbound axis keeps its
    /// declared range (0..=99).
    #[test]
    fn axis_values_are_counted_against_the_range_conversion_uses() {
        let x = axis_control(0x01, 0x30);
        let y = axis_control(0x01, 0x31);
        let caps = [
            axis_cap(x, true, 0, 127, 8).unwrap(),
            axis_cap(y, true, 0, 99, 8).unwrap(),
        ];
        let count = |scratch: ScratchConfig, values: &[(usize, i32)]| {
            let mut set = devices(&[(10, CONFIGURED)]);
            assert_eq!(set.add_axes(DeviceId(10), &caps)[..2], [true, true]);
            let mut recorded = recorded(Some(CONFIGURED));
            recorded.controller.push(x);
            recorded.scratch = scratch;
            assert!(take_part(&mut set, &recorded));
            set.open_recording(TimeUs(1));
            for &(axis, value) in values {
                set.axis_value(DeviceId(10), &caps[axis], value);
            }
            set.axis_out_of_range().collect::<Vec<_>>()
        };
        let x_values = [(0, 127), (0, 128), (0, 200), (0, 255), (0, 3)];
        let declared = ScratchConfig::default();
        let wrap = ScratchConfig {
            wrap: Some(256),
            ..ScratchConfig::default()
        };
        let relative = ScratchConfig {
            relative: Some(true),
            ..ScratchConfig::default()
        };
        // Sources: the controller, then keyboards 1 and 2.
        assert_eq!(count(declared, &x_values), [3, 0, 0]);
        assert_eq!(count(wrap, &x_values), [0, 0, 0]);
        assert_eq!(count(wrap, &[(0, 256), (0, -1)]), [2, 0, 0]);
        assert_eq!(count(relative, &x_values), [0, 0, 0]);
        assert_eq!(count(wrap, &[(1, 99), (1, 100), (1, 101)]), [2, 0, 0]);
    }

    /// The warning names where the range came from, and there is none
    /// for a source with nothing outside it.
    #[test]
    fn the_out_of_range_warning_names_the_range_used() {
        assert_eq!(out_of_range_warning(0, 0, true), None);
        assert_eq!(out_of_range_warning(0, 0, false), None);
        let declared = out_of_range_warning(2, 11, false).unwrap();
        assert!(declared.contains("source 2 reported 11 axis values"));
        assert!(declared.contains("the range its controller declares"));
        let set = out_of_range_warning(1, 7, true).unwrap();
        assert!(set.contains("source 1 reported 7 axis values"));
        assert!(set.contains("the scratch range device.toml sets"));
        assert!(!set.contains("declares"));
    }
}
