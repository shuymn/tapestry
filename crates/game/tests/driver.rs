//! The adjustments' driver on a real tap (design.md "Play Adjustments"):
//! the session-level path the player's rig takes, from packets through
//! the tap and the plan into the reducer and its snapshot, with the
//! attempt's tempo provider, the lifecycle's markers, attempts entered
//! and attached, the display offset cut in at a drain, recoveries, and
//! the window's speed keys.

use bms_model::{ChartBuilder, Speed, TimeUs};
use eval::ControlId;
use game::adjustment::{
    Adjustment, AttachError, Augmented, ControlRoles, Event, Head, Observation, PresentationReader,
    PresentationSnapshot, Role, TempoMap, presentation_link,
};
use game::{
    AdjustDriver, AdjustmentConfig, AttemptTempo, Cover, Covers, DeviceSettings, HiSpeedSettings,
    InputPump, InputWaitError, KeyConfigs, PlayOptions, Ratio, References, SpeedMode,
};
use input::{
    DeviceId, DeviceKind, Packet, PresentationFocus, SourceConfig, SourceSet, WindowKeyEvent,
    WindowKeys, window_keys,
};

const E: u32 = 0x12;
const W: u32 = 0x11;
const DOWN: u32 = 0xE050;
const Z: u32 = 0x2c;
const F1: u32 = 0x3B;
const F2: u32 = 0x3C;
const MS: i64 = 1_000;

fn r(num: u64, den: u64) -> Ratio {
    Ratio::new(num, den).unwrap()
}

fn options(mode: SpeedMode, classic_x100: u32, g: u32, covers: Covers) -> PlayOptions {
    PlayOptions::new(
        HiSpeedSettings {
            mode,
            classic_hispeed_x100: classic_x100,
            green_target: g,
            ..HiSpeedSettings::default()
        },
        covers,
    )
    .unwrap()
}

fn sudden(amount: u16) -> Covers {
    Covers {
        sudden: Cover { amount, on: true },
        ..Covers::default()
    }
}

/// A chart at `bpm` (whole BPM), changing to `then` at `at`.
fn tempo(bpm: i64, change: Option<(i64, i64)>) -> AttemptTempo {
    let mut b = ChartBuilder::new(bpm * 1_000);
    if let Some((at, then)) = change {
        b = b.bpm(TimeUs(at), then * 1_000);
    }
    AttemptTempo::prepare(&b.build().unwrap(), None)
}

/// The input side, synchronous: a fence is answered at once.
struct Pump<'a> {
    set: &'a mut SourceSet,
    now: TimeUs,
    offset: i64,
}

impl InputPump for Pump<'_> {
    fn request_fence(&mut self, n: u64) {
        let now = self.now;
        self.set.adjustment_mut().unwrap().fence(n, now);
    }
    fn wake(&mut self) {
        self.set.tick_presentation(self.now);
    }
    fn display_offset_us(&self) -> i64 {
        self.offset
    }
    fn park(&mut self) -> Result<(), InputWaitError> {
        Err(InputWaitError::Timeout)
    }
}

/// A keyboard reaching a tap planned on the default bindings, the driver
/// and its reader.
struct Rig {
    set: SourceSet,
    driver: AdjustDriver,
    reader: PresentationReader,
    focus: PresentationFocus,
    now: i64,
    offset: i64,
}

impl Rig {
    fn new(options: PlayOptions) -> Self {
        Self::with_window(options).0
    }

    fn with_window(options: PlayOptions) -> (Self, WindowKeys) {
        let device = DeviceSettings::default();
        let keys = KeyConfigs {
            keyboard: device.keyboard.clone(),
            controller: device.controller.clone(),
            controller_device: None,
        };
        let plan = AdjustmentConfig::default().plan(&keys, None, None).unwrap();
        let (driver, mut tap, reader) = AdjustDriver::new(&plan, options, 0).unwrap();
        let number = |code| {
            plan.bindings
                .controls
                .iter()
                .find(|(c, _)| *c == input::TapControl::Key(ControlId(code)))
                .map(|&(_, n)| n)
        };
        let (window, receiver) = window_keys();
        tap.set_window_keys(receiver, [number(F1), number(F2)]);
        let mut set = SourceSet::new(SourceConfig {
            max_devices: 2,
            max_sources: 2,
            max_held: 8,
            queue_capacity: 64,
            max_arrivals: 2,
        })
        .unwrap();
        let focus = PresentationFocus::new();
        set.set_presentation_focus(focus.clone());
        set.begin_batch(TimeUs(0));
        set.connect(DeviceId(1), DeviceKind::Keyboard);
        set.end_batch(TimeUs(1));
        set.set_adjustment(tap);
        let mut rig = Self {
            set,
            driver,
            reader,
            focus,
            now: 1,
            offset: 0,
        };
        rig.drain();
        (rig, window)
    }

    /// A batch at `at` µs of key packets (code, held).
    fn keys(&mut self, at: i64, keys: &[(u32, bool)]) -> &mut Self {
        assert!(at > self.now);
        self.now = at;
        self.set.begin_batch(TimeUs(at));
        for &(code, held) in keys {
            self.set.packet(&Packet {
                device: DeviceId(1),
                control: ControlId(code),
                value: i32::from(held),
                background: false,
            });
        }
        self.set.end_batch(TimeUs(at + 1));
        self
    }

    fn tap(&mut self, at: i64, code: u32) -> &mut Self {
        self.keys(at, &[(code, true), (code, false)])
    }

    /// The input thread wakes at `at`: its ticks.
    fn tick(&mut self, at: i64) -> &mut Self {
        assert!(at > self.now);
        self.now = at;
        self.set.tick_presentation(TimeUs(at));
        self
    }

    fn drain(&mut self) -> PresentationSnapshot {
        let mut pump = Pump {
            set: &mut self.set,
            now: TimeUs(self.now),
            offset: self.offset,
        };
        self.driver.pump(&mut pump);
        *self.reader.read()
    }

    /// Closes the open group a millisecond later, then drains.
    fn snap(&mut self) -> PresentationSnapshot {
        let at = self.now + MS;
        self.tick(at);
        self.drain()
    }

    fn enter(&mut self, opens_at: i64) -> &mut Self {
        let mut pump = Pump {
            set: &mut self.set,
            now: TimeUs(self.now),
            offset: self.offset,
        };
        self.driver
            .enter_attempt(Some(TimeUs(opens_at)), &mut pump)
            .unwrap();
        self
    }

    fn attach(&mut self, tempo: AttemptTempo, opens_at: i64) -> Result<u64, AttachError> {
        let mut pump = Pump {
            set: &mut self.set,
            now: TimeUs(self.now),
            offset: self.offset,
        };
        self.driver
            .attach(tempo, TimeUs(opens_at), &mut pump)
            .map_err(|e| e.unwrap())
    }

    /// Opens READY candidate `generation` at `at`, its chart time 0 at
    /// `origin` on the clock.
    fn open(&mut self, at: i64, origin: i64, generation: u32) -> &mut Self {
        assert!(at > self.now);
        self.now = at;
        self.set.begin_batch(TimeUs(at));
        assert!(
            self.set
                .open_candidate(TimeUs(at), TimeUs(origin), generation)
                .is_some()
        );
        self.set.end_batch(TimeUs(at + 1));
        self
    }
}

#[test]
fn a_retry_restores_floating_from_the_target() {
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-2_000_000);
    let first = rig.attach(tempo(150, None), -2_000_000).unwrap();
    assert_eq!(rig.drain().hispeed, Some(r(16, 5)));
    assert_eq!(rig.drain().epoch, first);
    rig.tap(10 * MS, F2);
    let s = rig.snap();
    assert_eq!(s.hispeed, Some(r(37, 10)), "an override");
    assert_eq!(s.options.hispeed().green_target, 300);
    // A retry: entered, then attached as a new epoch, solved from G.
    rig.enter(-2_000_000);
    assert_eq!(rig.drain().hispeed, None, "forgotten with the chart");
    let second = rig.attach(tempo(150, None), -2_000_000).unwrap();
    assert!(second > first);
    let s = rig.drain();
    assert_eq!((s.hispeed, s.epoch), (Some(r(16, 5)), second));
}

#[test]
fn classic_keeps_its_multiplier_across_enter_attempt() {
    let mut rig = Rig::new(options(SpeedMode::Classic, 320, 300, Covers::default()));
    rig.enter(-2_000_000);
    rig.attach(tempo(150, None), -2_000_000).unwrap();
    rig.tap(10 * MS, F2);
    assert_eq!(rig.snap().hispeed, Some(r(345, 100)));
    rig.enter(-2_000_000);
    let s = rig.drain();
    assert_eq!(s.hispeed, Some(r(345, 100)));
    assert_eq!(s.options.hispeed().classic_hispeed_x100, 345);
}

#[test]
fn a_loading_edit_before_the_chart_is_read_uses_no_previous_tempo() {
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-2_000_000);
    rig.attach(tempo(300, None), -2_000_000).unwrap();
    assert_eq!(rig.drain().hispeed, Some(r(8, 5)));
    // The next chart's loading: SELECT + a white key lowers G and solves
    // nothing, since no tempo is known; nothing is rolled back.
    rig.enter(-2_000_000);
    rig.keys(10 * MS, &[(W, true)])
        .tap(20 * MS, Z)
        .keys(30 * MS, &[(W, false)]);
    let s = rig.snap();
    assert_eq!(s.options.hispeed().green_target, 299);
    assert_eq!(s.hispeed, None);
    assert_eq!(rig.driver.diagnostics().unrepresentable, 0);
    // Chart B at 150 BPM: solved once with the edited target.
    rig.attach(tempo(150, None), -2_000_000).unwrap();
    assert_eq!(rig.drain().hispeed, Some(r(960, 299)));
}

#[test]
fn a_start_held_across_a_retry_keeps_adjusting_and_acts_after_a_fresh_press() {
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-2_000_000);
    rig.attach(tempo(150, None), -2_000_000).unwrap();
    rig.keys(10 * MS, &[(E, true)]);
    assert!(rig.snap().adjusting);
    rig.enter(-2_000_000);
    rig.attach(tempo(150, None), -2_000_000).unwrap();
    let s = rig.snap();
    assert!(s.adjusting, "START is still held");
    assert!(!s.options.covers().sudden.on);
    // Released, then pressed once: with the press before the retry this
    // would be a double press, but the history started afresh.
    rig.keys(100 * MS, &[(E, false)])
        .keys(150 * MS, &[(E, true)])
        .keys(160 * MS, &[(E, false)]);
    assert!(!rig.snap().options.covers().sudden.on);
    // Two fresh presses toggle SUDDEN+.
    rig.keys(200 * MS, &[(E, true)])
        .keys(210 * MS, &[(E, false)]);
    assert!(rig.snap().options.covers().sudden.on);
}

#[test]
fn a_loss_survives_the_attempts_entry_and_a_verified_baseline_ends_it() {
    let mut rig = Rig::new(PlayOptions::default());
    rig.keys(10 * MS, &[(E, true)]);
    assert!(rig.snap().adjusting);
    rig.focus.set(false);
    rig.tick(20 * MS);
    assert!(!rig.drain().adjusting, "focus loss clears adjusting");
    rig.enter(-2_000_000);
    rig.attach(tempo(150, None), -2_000_000).unwrap();
    assert!(!rig.snap().adjusting, "the loss stands across the entry");
    rig.focus.set(true);
    rig.tick(40 * MS);
    let s = rig.drain();
    assert!(s.adjusting, "a verified baseline: START is held");
    // Held across the loss: an auxiliary key under it acts, START itself
    // takes no new action until pressed afresh.
    assert_eq!(rig.driver.diagnostics().out_of_order, 0);
}

#[test]
fn a_select_before_a_cancelling_hold_solves_under_the_candidate_clock_across_a_tempo_change() {
    // 150 BPM, 300 from chart time 57.85 s; SUDDEN+ 200, G 300.
    for split in [false, true] {
        let mut rig = Rig::new(options(SpeedMode::Floating, 100, 300, sudden(200)));
        rig.enter(56_000_000);
        rig.attach(tempo(150, Some((57_850_000, 300))), 56_000_000)
            .unwrap();
        // The candidate's chart time 0 at -56.8 s: at 1 s it is 57.8 s.
        rig.open(1_000_000, -56_800_000, 1);
        if split {
            rig.drain();
        }
        // SELECT and a white key at 57.86 s: the hold cancels the candidate
        // after its batch, so G−1 is made under the candidate's clock;
        // START held at 57.87 s cancels nothing more.
        rig.keys(1_060_000, &[(W, true), (Z, true)]);
        if split {
            rig.drain();
        }
        rig.keys(1_070_000, &[(W, false), (Z, false), (E, true)]);
        let s = rig.snap();
        // 144000 · 0.8 / (300 · 299) = 384/299, then frozen.
        assert_eq!(s.hispeed, Some(r(384, 299)), "split {split}");
        assert_eq!(s.options.hispeed().green_target, 299);
        assert_eq!(rig.driver.diagnostics().out_of_order, 0);
    }
}

#[test]
fn owed_recovery_flushes_on_consumption_with_no_device_activity() {
    let mut rig = Rig::new(PlayOptions::default());
    // START held: every wake ticks, more than the queue holds.
    rig.keys(10 * MS, &[(E, true)]);
    for k in 1..=1_100 {
        rig.tick(10 * MS + k);
    }
    // Losses after the queue filled: one recovery owed.
    rig.keys(20 * MS, &[(W, true)]);
    rig.keys(21 * MS, &[(W, false)]);
    rig.keys(22 * MS, &[(W, true)]);
    // The driver drains, sees the tap owed, and wakes the input thread,
    // which flushes the recovery with no packet arriving.
    assert!(rig.driver.drain(0), "the tap is owed room");
    // The wake the consumer sends.
    rig.tick(23 * MS);
    let s = rig.drain();
    assert!(s.adjusting, "the baseline: START and SELECT held");
    assert_eq!(rig.driver.diagnostics().out_of_order, 0);
}

#[test]
fn window_speed_keys_replay_their_events_in_order() {
    // F2 repeat, release, press; F1 press: 3.2.
    // F2 release, press, repeat; F1 press: 3.7.
    for (history, want) in [
        (
            [
                (1, WindowKeyEvent::Repeat),
                (1, WindowKeyEvent::Release),
                (1, WindowKeyEvent::Press),
                (0, WindowKeyEvent::Press),
            ],
            r(16, 5),
        ),
        (
            [
                (1, WindowKeyEvent::Release),
                (1, WindowKeyEvent::Press),
                (1, WindowKeyEvent::Repeat),
                (0, WindowKeyEvent::Press),
            ],
            r(37, 10),
        ),
    ] {
        let (mut rig, mut window) = Rig::with_window(PlayOptions::default());
        rig.enter(-2_000_000);
        rig.attach(tempo(150, None), -2_000_000).unwrap();
        for (key, event) in history {
            window.key(key, event, false);
        }
        rig.tick(10 * MS);
        assert_eq!(rig.snap().hispeed, Some(want), "{history:?}");
    }
}

#[test]
fn a_synthetic_window_key_changes_nothing_until_a_fresh_press() {
    let (mut rig, mut window) = Rig::with_window(PlayOptions::default());
    rig.enter(-2_000_000);
    rig.attach(tempo(150, None), -2_000_000).unwrap();
    // F2 held through focus loss and return: the window makes up a press.
    window.key(1, WindowKeyEvent::Press, true);
    window.key(1, WindowKeyEvent::Repeat, true);
    rig.tick(10 * MS);
    assert_eq!(rig.snap().hispeed, Some(r(16, 5)));
    window.key(1, WindowKeyEvent::Release, false);
    window.key(1, WindowKeyEvent::Press, false);
    rig.tick(30 * MS);
    assert_eq!(rig.snap().hispeed, Some(r(37, 10)));
}

/// A tempo map: each tempo from its chart time on, the first before it.
struct Map(Vec<(i64, Ratio)>);

impl TempoMap for Map {
    fn bpm_at(&self, head: TimeUs) -> Option<Ratio> {
        let first = self.0.first()?.1;
        Some(
            self.0
                .iter()
                .rev()
                .find(|(at, _)| *at <= head.0)
                .map_or(first, |e| e.1),
        )
    }
}

fn roles() -> ControlRoles {
    let mut roles = ControlRoles::default();
    assert!(roles.set(game::adjustment::ControlId(0), Role::Select));
    assert!(roles.set(game::adjustment::ControlId(1), Role::White));
    roles
}

#[test]
fn an_offset_cut_is_ordered_by_its_position_in_the_augmented_stream() {
    // Base head 57.8 s at time 0; 150 BPM, 300 from 57.85 s; SUDDEN+ 200,
    // G 300; the offset becomes +100 ms; SELECT + a white key lowers G at
    // 10 ms (base 57.81 s).
    let map = Map(vec![
        (0, Ratio::integer(150)),
        (57_850_000, Ratio::integer(300)),
    ]);
    let o = |t: i64, seq, event| {
        Augmented::Obs(Observation {
            time: TimeUs(t),
            seq,
            event,
        })
    };
    let select = game::adjustment::ControlId(0);
    let white = game::adjustment::ControlId(1);
    let head = o(
        0,
        1,
        Event::Phase {
            head: Head::Clock {
                at: TimeUs(0),
                chart: TimeUs(57_800_000),
            },
            generation: 1,
        },
    );
    let g_minus = [
        o(10 * MS, 2, Event::Press(select)),
        o(10 * MS, 3, Event::Press(white)),
    ];
    let close = o(11 * MS, 4, Event::Tick);
    let cut = Augmented::Cut(100_000);
    let s1: Vec<Augmented> = [head]
        .into_iter()
        .chain(g_minus)
        .chain([cut, close])
        .collect();
    let s2: Vec<Augmented> = [head, cut]
        .into_iter()
        .chain(g_minus)
        .chain([close])
        .collect();
    for (stream, want) in [(s1, r(768, 299)), (s2, r(384, 299))] {
        // Every packing of the same augmented stream: one drain, a drain
        // per item, and a split at each position.
        let mut packings: Vec<Vec<usize>> = vec![vec![stream.len()], vec![1; stream.len()]];
        packings.extend((1..stream.len()).map(|k| vec![k, stream.len() - k]));
        for packing in packings {
            let mut a = Adjustment::new(
                options(SpeedMode::Floating, 100, 300, sudden(200)),
                roles(),
                TimeUs(0),
                0,
            );
            a.attach(
                TimeUs(0),
                Some(References::all(Ratio::integer(150))),
                Speed::NORMAL,
                &map,
            )
            .unwrap();
            let (mut w, mut rd) = presentation_link(&a.snapshot());
            let mut items = stream.iter().copied();
            for n in &packing {
                a.feed(items.by_ref().take(*n), &map, &mut w);
            }
            let s = *rd.read();
            assert_eq!(s.hispeed, Some(want), "{packing:?}");
            assert_eq!(a.diagnostics().out_of_order, 0);
        }
    }
}

#[test]
fn practice_150_percent_solves_32_15() {
    let map = Map(vec![(0, Ratio::integer(150))]);
    let mut a = Adjustment::new(PlayOptions::default(), roles(), TimeUs(0), 0);
    a.attach(
        TimeUs(0),
        Some(References::all(Ratio::integer(150))),
        Speed::from_twentieths(30).unwrap(),
        &map,
    )
    .unwrap();
    // The playback tempo 225: 144000 / (225 · 300) = 32/15, 500 ms.
    assert_eq!(a.snapshot().hispeed, Some(r(32, 15)));
}

#[test]
fn floating_without_references_is_refused_and_classic_plays() {
    let map = Map(vec![(0, Ratio::integer(150))]);
    let mut a = Adjustment::new(PlayOptions::default(), roles(), TimeUs(0), 0);
    assert_eq!(
        a.attach(TimeUs(0), None, Speed::NORMAL, &map),
        Err(AttachError::NoReferences)
    );
    assert_eq!(a.snapshot().hispeed, None);
    let mut c = Adjustment::new(
        options(SpeedMode::Classic, 250, 300, Covers::default()),
        roles(),
        TimeUs(0),
        0,
    );
    assert_eq!(c.attach(TimeUs(0), None, Speed::NORMAL, &map), Ok(()));
    assert_eq!(c.snapshot().hispeed, Some(r(5, 2)));
}

#[test]
fn the_driver_gives_one_state_whatever_the_drain_splits() {
    // 300 BPM, SUDDEN+ 500, G 360: H 2/3, 600 ms.
    let run = |drains: &[usize]| {
        let mut rig = Rig::new(options(SpeedMode::Floating, 100, 360, sudden(500)));
        rig.enter(-2_000_000);
        rig.attach(tempo(300, None), -2_000_000).unwrap();
        assert_eq!(rig.drain().hispeed, Some(r(2, 3)));
        let steps: [&dyn Fn(&mut Rig); 6] = [
            &|rig| {
                rig.keys(10 * MS, &[(W, true)]);
            },
            &|rig| {
                rig.tap(20 * MS, Z);
            },
            &|rig| {
                rig.keys(30 * MS, &[(W, false), (E, true)]);
            },
            &|rig| {
                rig.tap(40 * MS, DOWN);
            },
            &|rig| {
                rig.keys(50 * MS, &[(E, false)]);
            },
            &|rig| {
                rig.tick(60 * MS);
            },
        ];
        for (i, step) in steps.iter().enumerate() {
            step(&mut rig);
            if drains.contains(&i) {
                rig.drain();
            }
        }
        let mut s = rig.drain();
        s.sequence = 0;
        s
    };
    let one = run(&[]);
    assert_eq!(one.options.hispeed().green_target, 359);
    assert_eq!(one.options.covers().sudden.amount, 510);
    for drains in [&[0, 1, 2, 3, 4][..], &[1, 3], &[2]] {
        assert_eq!(run(drains), one, "{drains:?}");
    }
}

impl Rig {
    /// The audio side commits READY candidate `generation`: holds are
    /// ordinary adjustments from now on.
    fn commit(&mut self, generation: u32) -> &mut Self {
        assert!(
            self.set
                .adjustment_mut()
                .unwrap()
                .start_gate()
                .commit(generation)
        );
        self
    }

    /// Rearms READY candidate `generation` at `at`, its chart time 0 at
    /// `origin` on the clock: the cancelled one sealed, readers back.
    fn rearm(&mut self, at: i64, origin: i64, generation: u32) -> &mut Self {
        assert!(at > self.now);
        self.now = at;
        self.set.begin_batch(TimeUs(at));
        let readers = self.set.take_readers();
        self.set.return_readers(readers);
        assert!(
            self.set
                .reopen_candidate(TimeUs(at), TimeUs(origin), generation)
                .is_ok()
        );
        self.set.end_batch(TimeUs(at + 1));
        self
    }
}

const AUX_UP: u32 = 0xE050;

#[test]
fn ready_adjustments_use_the_candidate_clock() {
    // 150 BPM, 300 from chart time 1 s; the attempt opens at -1 s.
    let mut rig = Rig::new(options(SpeedMode::Floating, 100, 300, sudden(200)));
    rig.enter(-1_000_000);
    rig.attach(tempo(150, Some((1_000_000, 300))), -1_000_000)
        .unwrap();
    assert_eq!(rig.drain().hispeed, Some(r(64, 25)), "at the frozen head");
    // READY's chart time 0 at 0.5 s on the clock; at 2 s it is 1.5 s, past
    // the change: a START + auxiliary cover step solves at 300.
    // START is a hold that cancels the candidate, after its batch: the
    // step pressed with it is made under the candidate's clock.
    rig.open(100 * MS, 500_000, 1);
    rig.keys(2_000_000, &[(E, true), (AUX_UP, true)])
        .keys(2_020_000, &[(AUX_UP, false), (E, false)]);
    let s = rig.snap();
    assert_eq!(s.options.covers().sudden.amount, 210);
    // 144000 · 0.79 / (300 · 300) = 158/125.
    assert_eq!(s.hispeed, Some(r(158, 125)));
    assert_eq!(rig.driver.diagnostics().out_of_order, 0);
}

#[test]
fn a_rearm_keeps_h_and_the_double_press() {
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.open(100 * MS, 1_100_000, 1);
    rig.tap(200 * MS, F2);
    assert_eq!(rig.snap().hispeed, Some(r(37, 10)));
    // START cancels the candidate; released, READY rearms.
    rig.keys(300 * MS, &[(E, true)])
        .keys(350 * MS, &[(E, false)]);
    rig.drain();
    rig.rearm(400 * MS, 1_400_000, 2);
    let s = rig.snap();
    assert_eq!(s.hispeed, Some(r(37, 10)), "no new solve at the rearm");
    assert_eq!(s.generation, 2);
    assert!(!s.options.covers().sudden.on);
    // The press that cancelled and one after the rearm, 300 ms apart: a
    // double press.
    rig.keys(600 * MS, &[(E, true)])
        .keys(610 * MS, &[(E, false)]);
    assert!(rig.snap().options.covers().sudden.on);
}

#[test]
fn in_play_adjustments_follow_the_transition_table() {
    // Covers off, 150 BPM then 300 from 2 s: a fine step adds 0.01 and
    // solves nothing, so 3.20 becomes 3.21 at 300 BPM.
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-1_000_000);
    rig.attach(tempo(150, Some((2_000_000, 300))), -1_000_000)
        .unwrap();
    rig.open(100 * MS, 1_100_000, 1);
    rig.commit(1);
    rig.keys(4_000_000, &[(E, true)])
        .tap(4_010_000, AUX_UP)
        .keys(4_020_000, &[(E, false)]);
    assert_eq!(rig.snap().hispeed, Some(r(321, 100)));
    // SUDDEN+ shown at 0 by a double press solves at the event's tempo:
    // 144000 / (300 · 300) = 1.6.
    rig.keys(5_000_000, &[(E, true)])
        .keys(5_010_000, &[(E, false)])
        .keys(5_100_000, &[(E, true)])
        .keys(5_110_000, &[(E, false)]);
    let s = rig.snap();
    assert!(s.options.covers().sudden.on);
    assert_eq!(s.hispeed, Some(r(8, 5)));
    // At 300 BPM with SUDDEN+ 300, G 300 → 299 gives 336/299.
    let mut rig = Rig::new(options(SpeedMode::Floating, 100, 300, sudden(300)));
    rig.enter(-1_000_000);
    rig.attach(tempo(300, None), -1_000_000).unwrap();
    rig.open(100 * MS, 1_100_000, 1);
    rig.commit(1);
    rig.keys(3_000_000, &[(W, true)])
        .tap(3_010_000, Z)
        .keys(3_020_000, &[(W, false)]);
    assert_eq!(rig.snap().hispeed, Some(r(336, 299)));
    assert_eq!(rig.driver.diagnostics().out_of_order, 0);
}

#[test]
fn screen_recovery_keeps_the_floating_override() {
    // The screen's recovery touches no attempt: a pending attachment is
    // kept, so the next preparation does not solve again.
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-1_000_000);
    let epoch = rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.tap(10 * MS, F2);
    assert_eq!(rig.snap().hispeed, Some(r(37, 10)));
    assert!(rig.driver.has_pending());
    assert_eq!(rig.driver.prepared(), Some(epoch));
    assert_eq!(rig.drain().hispeed, Some(r(37, 10)));
}

#[test]
fn a_loading_double_press_survives_into_the_play() {
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-1_000_000);
    rig.keys(10 * MS, &[(E, true)]).keys(20 * MS, &[(E, false)]);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.keys(300 * MS, &[(E, true)])
        .keys(310 * MS, &[(E, false)]);
    assert!(rig.snap().options.covers().sudden.on);
}

#[test]
fn the_previous_charts_double_press_does_not() {
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.keys(10 * MS, &[(E, true)]).keys(20 * MS, &[(E, false)]);
    // The next chart's loading starts within 500 ms.
    rig.enter(-1_000_000);
    rig.keys(300 * MS, &[(E, true)])
        .keys(310 * MS, &[(E, false)]);
    assert!(!rig.snap().options.covers().sudden.on);
}

#[test]
fn a_device_change_while_loading_is_a_new_baseline() {
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-1_000_000);
    // A second keyboard, then START held on the first.
    rig.set.begin_batch(TimeUs(5 * MS));
    rig.set.connect(DeviceId(2), DeviceKind::Keyboard);
    rig.set.end_batch(TimeUs(5 * MS + 1));
    rig.now = 5 * MS;
    rig.keys(10 * MS, &[(E, true)]);
    assert!(rig.snap().adjusting);
    // The second keyboard leaves: an ownership change, a new baseline with
    // START still held and suppressed.
    rig.set.begin_batch(TimeUs(100 * MS));
    rig.set.disconnect(DeviceId(2));
    rig.set.end_batch(TimeUs(100 * MS + 1));
    rig.now = 100 * MS;
    assert!(rig.snap().adjusting);
    // A press before the baseline does not count toward a double press
    // with the next.
    rig.keys(200 * MS, &[(E, false)])
        .keys(300 * MS, &[(E, true)])
        .keys(310 * MS, &[(E, false)]);
    assert!(!rig.snap().options.covers().sudden.on);
    assert_eq!(rig.driver.diagnostics().out_of_order, 0);
}

#[test]
fn a_gesture_accepted_with_the_cancelling_hold_keeps_repeating() {
    // START and the scratch's positive direction in one batch during an
    // armed READY: START cancels the candidate after the batch, and the
    // scratch's fine steps go on repeating every 50 ms.
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.open(100 * MS, 1_100_000, 1);
    rig.keys(200 * MS, &[(E, true), (0x2a, true)]);
    for k in 1..=4 {
        rig.tick(200 * MS + k * 50 * MS);
    }
    // 3.20 and five steps of 0.01: the press and four repeats.
    assert_eq!(rig.snap().hispeed, Some(r(325, 100)));
}

#[test]
fn a_scene_change_stops_held_controls_until_a_fresh_press() {
    let mut rig = Rig::new(options(SpeedMode::Classic, 320, 300, Covers::default()));
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    // F2 held: its press and a keyboard repeat step.
    rig.keys(10 * MS, &[(F2, true)])
        .keys(20 * MS, &[(F2, true)]);
    assert_eq!(rig.snap().hispeed, Some(r(370, 100)));
    // The play ends and its result shows: the repeats step nothing more.
    let mut pump = Pump {
        set: &mut rig.set,
        now: TimeUs(rig.now),
        offset: 0,
    };
    rig.driver.scene_changed(&mut pump).unwrap();
    rig.keys(40 * MS, &[(F2, true)])
        .keys(50 * MS, &[(F2, true)]);
    let s = rig.snap();
    assert_eq!(s.hispeed, Some(r(370, 100)));
    assert_eq!(s.options.hispeed().classic_hispeed_x100, 370);
    // Released and pressed afresh, it steps again.
    rig.keys(60 * MS, &[(F2, false)])
        .keys(70 * MS, &[(F2, true)]);
    assert_eq!(rig.snap().hispeed, Some(r(395, 100)));
}

#[test]
fn a_press_queued_before_a_scene_change_stays_in_the_old_scene() {
    // F2 pressed and repeating after the last drain, before the result
    // shows: the scene change fences the tap first, so the press is taken
    // in the play's context and its repeats on the result act no more.
    let mut rig = Rig::new(options(SpeedMode::Classic, 320, 300, Covers::default()));
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.drain();
    rig.keys(10 * MS, &[(F2, true)]);
    let mut pump = Pump {
        set: &mut rig.set,
        now: TimeUs(rig.now),
        offset: 0,
    };
    rig.driver.scene_changed(&mut pump).unwrap();
    rig.keys(40 * MS, &[(F2, true)])
        .keys(50 * MS, &[(F2, true)]);
    let s = rig.snap();
    assert_eq!(s.hispeed, Some(r(345, 100)), "the press alone");
    assert_eq!(s.options.hispeed().classic_hispeed_x100, 345);
}

/// An input side that answers nothing for `stall` parks, then answers:
/// the fences it was asked for come with the next park.
struct Slow<'a> {
    set: &'a mut SourceSet,
    now: TimeUs,
    asked: Vec<u64>,
    parks: u32,
    stall: u32,
    /// Parks after which the pump's awake deadline has passed.
    deadline: u32,
}

impl InputPump for Slow<'_> {
    fn request_fence(&mut self, n: u64) {
        self.asked.push(n);
    }
    fn wake(&mut self) {}
    fn display_offset_us(&self) -> i64 {
        0
    }
    fn park(&mut self) -> Result<(), InputWaitError> {
        self.parks += 1;
        if self.parks >= self.deadline {
            return Err(InputWaitError::Timeout);
        }
        if self.parks >= self.stall {
            let now = self.now;
            for &n in &self.asked {
                self.set.adjustment_mut().unwrap().fence(n, now);
            }
            self.asked.clear();
        }
        Ok(())
    }
}

#[test]
fn a_wait_ends_only_at_the_pumps_awake_deadline() {
    // Early wakes do not end a wait: an answer after 5,000 parks, all
    // within the awake deadline, is taken.
    let mut rig = Rig::new(PlayOptions::default());
    let now = TimeUs(rig.now);
    let mut slow = Slow {
        set: &mut rig.set,
        now,
        asked: Vec::new(),
        parks: 0,
        stall: 5_000,
        deadline: u32::MAX,
    };
    assert_eq!(rig.driver.fence(&mut slow), Ok(()));
    assert_eq!(slow.parks, 5_000);
    assert_eq!(rig.driver.failure(), None);
}

#[test]
fn a_failed_wait_is_terminal_even_when_the_input_recovers() {
    // The cancellation's marker never comes before the awake deadline:
    // the wait fails, and the failure is latched.
    let mut rig = Rig::new(PlayOptions::default());
    let now = TimeUs(rig.now);
    let mut stalled = Slow {
        set: &mut rig.set,
        now,
        asked: Vec::new(),
        parks: 0,
        stall: u32::MAX,
        deadline: 10,
    };
    assert_eq!(
        rig.driver.after_cancel(1, &mut stalled),
        Err(InputWaitError::Timeout)
    );
    assert_eq!(rig.driver.failure(), Some(InputWaitError::Timeout));
    // The input thread recovers: nothing more is asked of it, and every
    // later wait (a retry's entry, a scene change, an opening) answers
    // with the first failure.
    let mut recovered = Slow {
        set: &mut rig.set,
        now,
        asked: Vec::new(),
        parks: 0,
        stall: 0,
        deadline: u32::MAX,
    };
    assert_eq!(
        rig.driver.enter_attempt(None, &mut recovered),
        Err(InputWaitError::Timeout)
    );
    assert_eq!(
        rig.driver.scene_changed(&mut recovered),
        Err(InputWaitError::Timeout)
    );
    assert_eq!(
        rig.driver.before_opening(&mut recovered),
        Err(InputWaitError::Timeout)
    );
    assert!(
        recovered.asked.is_empty(),
        "nothing asked: {:?}",
        recovered.asked
    );
}

/// The driver gives an opening's starting tuning for its own candidate
/// only (design.md "Input Log Format"): candidate 1's tuning once its
/// opening is drained, nothing for another generation, and none before
/// any opening.
#[test]
fn an_openings_tuning_is_given_for_its_own_candidate_only() {
    let mut rig = Rig::new(options(SpeedMode::Classic, 100, 300, Covers::default()));
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.tap(10 * MS, F2);
    rig.drain();
    assert_eq!(rig.driver.opened(1), None, "no opening yet");
    rig.open(20 * MS, 1_020 * MS, 1);
    rig.drain();
    let t = rig.driver.opened(1).unwrap();
    assert_eq!(t.hispeed, Some(replay::HiSpeedRatio { num: 5, den: 4 }));
    assert_eq!(rig.driver.opened(2), None, "another candidate");
    assert_eq!(rig.driver.opened(0), None, "another candidate");
}

/// The scratch's digital direction reaches the snapshot through a drain
/// (docs/stage3.md, offset 1) and outlives an attempt's entry, which
/// suppresses the hold for the adjustments only.
#[test]
fn the_snapshot_publishes_the_held_scratch_across_an_attempts_entry() {
    const SCRATCH: u32 = 0x2a;
    let mut rig = Rig::new(PlayOptions::default());
    assert_eq!(rig.drain().scratch, game::ScratchInput::NONE);
    rig.keys(10 * MS, &[(SCRATCH, true)]);
    let s = rig.drain();
    assert!(s.scratch.positive && !s.scratch.negative);
    rig.enter(-2_000_000);
    assert!(rig.drain().scratch.positive);
    rig.keys(20 * MS, &[(SCRATCH, false)]);
    assert_eq!(rig.drain().scratch, game::ScratchInput::NONE);
}

// ---- The Loading opener's presentation epoch and acceptance ----

const SCRATCH: u32 = 0x2a;

/// Reads any `.chart` file as the chart it was built with.
struct Built(bms_model::Chart);

impl game::ChartIngest for Built {
    fn max_bytes(&self) -> usize {
        1 << 20
    }

    fn accepts(&self, path: &std::path::Path) -> bool {
        path.extension().is_some_and(|e| e == "chart")
    }

    fn ingest(
        &self,
        _bytes: &[u8],
        _dir: &std::path::Path,
        _seed: u64,
        _speed: Speed,
    ) -> Result<game::Ingested, game::IngestError> {
        Ok(game::Ingested {
            labels: game::ChartLabels::default(),
            profile: bms_model::ChartProfile::of(&self.0, &[]),
            chart: self.0.clone(),
            warnings: Vec::new(),
        })
    }
}

/// `chart` loaded with a lead-in of `lead_in_ms`, in a data root that lives
/// as long as the directory returned with it.
fn loaded(chart: &bms_model::Chart, lead_in_ms: i64) -> (tempfile::TempDir, game::Loaded) {
    let dir = tempfile::Builder::new()
        .prefix("driver")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = game::DataRoot::new(dir.path().to_owned());
    std::fs::create_dir_all(root.path()).unwrap();
    let path = root.path().join("play.chart");
    std::fs::write(&path, b"built in code").unwrap();
    let settings = game::Settings::load(&root).unwrap();
    let parts = game::Parts {
        ingest: Box::new(Built(chart.clone())),
        ..game::Parts::standard(&root, &settings.player)
    };
    let config = game::SessionConfig {
        lead_in: TimeUs(lead_in_ms * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 7,
        speed: Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: replay::BuildProvenance::Development,
        receive: replay::ReceiveMethod::Scripted,
        autoplay: None,
    };
    let loaded = game::Loaded::load(root, settings, &path, config, parts).unwrap();
    (dir, loaded)
}

fn chart_150() -> bms_model::Chart {
    ChartBuilder::new(150_000).build().unwrap()
}

/// A chart at 150.000000000000000001 BPM: no exact ratio, so its
/// references are unavailable and Floating cannot solve for it.
fn no_references_chart() -> bms_model::Chart {
    let mut chart = chart_150();
    let fine = bms_model::Tempo::from_units(150 * bms_model::Tempo::UNITS_PER_BPM + 1).unwrap();
    chart.timeline = bms_model::ChartTimeline::new(
        vec![bms_model::TimelinePoint {
            at: TimeUs(0),
            pos: bms_model::Position(0),
            tempo: fine,
        }],
        Vec::new(),
        fine,
        Speed::NORMAL,
    )
    .unwrap();
    chart
}

fn lift_and_hidden() -> Covers {
    Covers {
        lift: Cover {
            amount: 100,
            on: true,
        },
        hidden: Cover {
            amount: 300,
            on: true,
        },
        ..Covers::default()
    }
}

fn hidden_only() -> Covers {
    Covers {
        hidden: Cover {
            amount: 300,
            on: true,
        },
        ..Covers::default()
    }
}

fn hidden(s: &PresentationSnapshot) -> u16 {
    s.options.covers().hidden.amount
}

impl Rig {
    fn attach_loaded(&mut self, loaded: &game::Loaded) -> Option<game::PreparationRefusal> {
        let mut pump = Pump {
            set: &mut self.set,
            now: TimeUs(self.now),
            offset: self.offset,
        };
        self.driver.attach_loaded(loaded, &mut pump).unwrap()
    }

    fn refresh(&mut self, loaded: &game::Loaded) -> Option<game::PreparationRefusal> {
        let mut pump = Pump {
            set: &mut self.set,
            now: TimeUs(self.now),
            offset: self.offset,
        };
        self.driver.refresh_presentation(loaded, &mut pump).unwrap()
    }

    fn close(&mut self) -> &mut Self {
        let mut pump = Pump {
            set: &mut self.set,
            now: TimeUs(self.now),
            offset: self.offset,
        };
        self.driver.close_acceptance(&mut pump).unwrap();
        self
    }

    fn reopen(&mut self) -> &mut Self {
        let mut pump = Pump {
            set: &mut self.set,
            now: TimeUs(self.now),
            offset: self.offset,
        };
        self.driver.reopen_acceptance(&mut pump).unwrap();
        self
    }

    fn close_provisionally(&mut self) -> &mut Self {
        let mut pump = Pump {
            set: &mut self.set,
            now: TimeUs(self.now),
            offset: self.offset,
        };
        self.driver.close_provisionally(&mut pump).unwrap();
        self
    }

    /// Decides a provisional close: kept, or reversed.
    fn decide(&mut self, keep: bool) -> PresentationSnapshot {
        let mut pump = Pump {
            set: &mut self.set,
            now: TimeUs(self.now),
            offset: self.offset,
        };
        if keep {
            self.driver.commit_close(&mut pump);
        } else {
            self.driver.reverse_close(&mut pump);
        }
        *self.reader.read()
    }
}

/// §2.10 R19 and R21 (A11's interleaving): the revalidation's close is
/// provisional; F2 pressed after its fence's answer and before the
/// decision waits undrained (a drain meanwhile takes nothing), and once
/// the close is reversed at its fence it acts, open; held, its key
/// repeat acts too, so nothing is left suppressed.
#[test]
fn a_press_after_the_closing_fence_acts_when_the_close_is_reversed() {
    let mut rig = Rig::new(options(SpeedMode::Classic, 320, 300, Covers::default()));
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.drain();
    let accepted = rig.driver.accepted();
    rig.close_provisionally();
    assert!(!rig.driver.accepting(), "closed at the fence");
    rig.keys(10 * MS, &[(F2, true)]);
    let waiting = rig.drain();
    assert_eq!(waiting.hispeed, Some(r(320, 100)), "it waits");
    assert_eq!(rig.driver.accepted(), accepted);
    rig.decide(false);
    assert!(rig.driver.accepting());
    assert_eq!(rig.snap().hispeed, Some(r(345, 100)), "F2 acted, open");
    // Still held: its repeat acts.
    rig.keys(20 * MS, &[(F2, true)]);
    assert_eq!(rig.snap().hispeed, Some(r(370, 100)));
    assert_eq!(rig.driver.accepted(), accepted + 2);
    assert_eq!(rig.driver.diagnostics().out_of_order, 0);
}

/// §2.10 R20: the same provisional close kept: the F2 press that waited
/// is taken closed, discarded, and its repeat too; released and pressed
/// afresh after a reopening, it acts.
#[test]
fn a_press_after_the_closing_fence_is_discarded_when_the_close_commits() {
    let mut rig = Rig::new(options(SpeedMode::Classic, 320, 300, Covers::default()));
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.drain();
    let accepted = rig.driver.accepted();
    rig.close_provisionally();
    rig.keys(10 * MS, &[(F2, true)]);
    rig.decide(true);
    assert!(!rig.driver.accepting());
    assert_eq!(rig.snap().hispeed, Some(r(320, 100)), "discarded");
    rig.keys(20 * MS, &[(F2, true)]);
    assert_eq!(rig.snap().hispeed, Some(r(320, 100)));
    rig.reopen();
    rig.keys(30 * MS, &[(F2, false)])
        .keys(40 * MS, &[(F2, true)]);
    assert_eq!(rig.snap().hispeed, Some(r(345, 100)));
    assert_eq!(rig.driver.accepted(), accepted + 1);
}

/// A10: the Loading opener's epoch refresh, with HIDDEN+ selected (LIFT
/// and HIDDEN+ on, SUDDEN+ off), a Floating override and START with the
/// scratch held and repeating: a new epoch on the standing attachment;
/// the selection, the override and the repeating hold all stay.
#[test]
fn the_epoch_refresh_keeps_the_selected_cover_and_holds() {
    let chart = chart_150();
    let (_dir, loaded) = loaded(&chart, 1_000);
    let mut rig = Rig::new(options(SpeedMode::Floating, 100, 300, lift_and_hidden()));
    rig.enter(loaded.opens_at().0);
    assert_eq!(rig.attach_loaded(&loaded), None);
    let first = rig.driver.prepared().unwrap();
    let solved = rig.drain().hispeed.unwrap();
    // F2 overrides H; START and SELECT together select HIDDEN+.
    rig.tap(5 * MS, F2);
    rig.keys(10 * MS, &[(E, true), (W, true)])
        .keys(20 * MS, &[(E, false), (W, false)]);
    let s = rig.snap();
    let overridden = s.hispeed.unwrap();
    assert_ne!(overridden, solved);
    assert_eq!(s.target, Some(game::CoverKind::Hidden));
    // START and the scratch held: HIDDEN+ steps on the press and at 650 ms.
    rig.keys(600 * MS, &[(E, true), (SCRATCH, true)]);
    rig.tick(650 * MS);
    assert_eq!(hidden(&rig.snap()), 298);
    let accepted = rig.driver.accepted();
    assert_eq!(rig.refresh(&loaded), None);
    assert!(rig.driver.has_pending());
    let second = rig.driver.prepared().unwrap();
    assert!(second > first);
    let s = rig.drain();
    assert_eq!(s.epoch, second);
    assert_eq!(s.target, Some(game::CoverKind::Hidden), "still selected");
    assert_eq!(s.hispeed, Some(overridden), "the attachment stands");
    assert!(s.adjusting, "START still held");
    assert_eq!(rig.driver.accepted(), accepted, "the refresh is no change");
    // The hold keeps repeating at its deadline.
    rig.tick(700 * MS);
    assert_eq!(hidden(&rig.snap()), 297);
    assert_eq!(rig.driver.diagnostics().out_of_order, 0);
}

/// A10b: a refresh whose opening moved (a refitted lead-in) solves again
/// within the dispatch context: the held scratch's next repeat comes at
/// its old deadline, not a fresh press's, and is not suppressed. The axis
/// remainder's half is at the reducer
/// (`resolve_keeps_repeat_deadlines_and_axis_remainders`).
#[test]
fn a_changed_refresh_keeps_repeat_deadlines_and_axis_remainders() {
    let chart = chart_150();
    let (_d1, before) = loaded(&chart, 1_000);
    let (_d2, after) = loaded(&chart, 2_000);
    assert_ne!(before.opens_at(), after.opens_at());
    let mut rig = Rig::new(options(SpeedMode::Floating, 100, 300, hidden_only()));
    rig.enter(before.opens_at().0);
    assert_eq!(rig.attach_loaded(&before), None);
    let first = rig.driver.prepared().unwrap();
    let solved = rig.drain().hispeed.unwrap();
    rig.tap(5 * MS, F2);
    assert_ne!(rig.snap().hispeed, Some(solved), "an override");
    // START and the scratch at 600 ms: a step, the next due at 650 ms.
    rig.keys(600 * MS, &[(E, true), (SCRATCH, true)]);
    rig.tick(620 * MS);
    assert_eq!(hidden(&rig.drain()), 299);
    assert_eq!(rig.refresh(&after), None);
    let second = rig.driver.prepared().unwrap();
    assert!(second > first);
    let s = rig.drain();
    assert_eq!(s.epoch, second);
    assert_eq!(
        s.hispeed,
        Some(solved),
        "solved again: the override is gone"
    );
    // Before the old deadline nothing; at it, one step.
    rig.tick(649 * MS);
    assert_eq!(hidden(&rig.drain()), 299);
    rig.tick(650 * MS);
    assert_eq!(hidden(&rig.snap()), 298);
    rig.tick(700 * MS);
    assert_eq!(hidden(&rig.snap()), 297);
    // The refresh's attachment stands: a second refresh at the same
    // opening reuses it and keeps a new override.
    rig.tap(800 * MS, F2);
    let overridden = rig.snap().hispeed;
    assert_ne!(overridden, Some(solved));
    assert_eq!(rig.refresh(&after), None);
    rig.driver.prepared().unwrap();
    assert_eq!(rig.drain().hispeed, overridden);
}

/// A10c, the driver's half: the load-time attachment of a chart Floating
/// cannot solve for (no references) is refused and ignored, as the player
/// ignores it; the refresh attaches again and reports the same refusal,
/// with no epoch pending.
#[test]
fn a_failed_load_attachment_is_refused_at_refresh() {
    let chart = no_references_chart();
    let (_dir, loaded) = loaded(&chart, 1_000);
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(loaded.opens_at().0);
    let at_load = rig.attach_loaded(&loaded).expect("refused at load");
    assert!(!rig.driver.has_pending());
    let at_refresh = rig.refresh(&loaded).expect("refused at the refresh");
    assert_eq!(at_refresh, at_load);
    assert_eq!(
        at_refresh.location,
        Some(loaded.path().display().to_string())
    );
    assert!(
        at_refresh
            .reason
            .contains(&AttachError::NoReferences.to_string()),
        "{}",
        at_refresh.reason
    );
    assert!(!rig.driver.has_pending());
    assert_eq!(rig.driver.prepared(), None);
    assert_eq!(rig.drain().hispeed, None);
    // Every refresh refuses again: no failed attachment is reused.
    assert_eq!(rig.refresh(&loaded), Some(at_load));
}

/// A11: a close reversed at once drops nothing. A press queued before the
/// close (not yet drained) is taken by the close's fence and acts; the
/// repeating hold, the selection and the acceptance are as before.
#[test]
fn a_provisional_close_reversed_drops_nothing() {
    let mut rig = Rig::new(options(SpeedMode::Floating, 100, 300, hidden_only()));
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.keys(600 * MS, &[(E, true), (SCRATCH, true)]);
    rig.tick(650 * MS);
    let s = rig.snap();
    assert_eq!(hidden(&s), 298);
    assert_eq!(s.hispeed, Some(r(16, 5)));
    let accepted = rig.driver.accepted();
    // F2 under START, queued and not drained when the close is asked.
    rig.tap(660 * MS, F2);
    rig.close();
    assert!(!rig.driver.accepting());
    rig.reopen();
    assert!(rig.driver.accepting());
    let after = rig.drain();
    assert_eq!(after.hispeed, Some(r(37, 10)), "the queued F2 acted");
    assert_eq!(rig.driver.accepted(), accepted + 1);
    assert_eq!(after.target, s.target);
    assert!(after.adjusting);
    // The scratch held before the close repeats at its deadline.
    rig.tick(700 * MS);
    assert_eq!(hidden(&rig.snap()), 297);
    assert_eq!(rig.driver.accepted(), accepted + 2);
}

/// A12 and A13 at the reducer: from the close, a press acts on nothing and
/// its repeats neither, before or after the reopening; a START tapped in
/// the gap is no double-press history; once released, a fresh press after
/// the reopening acts.
#[test]
fn a_press_in_the_closed_gap_is_discarded_and_rearms() {
    let mut rig = Rig::new(options(SpeedMode::Classic, 320, 300, Covers::default()));
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.drain();
    let accepted = rig.driver.accepted();
    rig.close();
    // F2 pressed and repeating in the gap.
    rig.keys(10 * MS, &[(F2, true)])
        .keys(20 * MS, &[(F2, true)]);
    assert_eq!(rig.snap().hispeed, Some(r(320, 100)));
    // START held in the gap is held, and acts on nothing.
    rig.keys(30 * MS, &[(E, true)]);
    assert!(rig.snap().adjusting);
    rig.keys(40 * MS, &[(E, false)]);
    rig.reopen();
    // F2's repeat after the reopening: still nothing.
    rig.keys(50 * MS, &[(F2, true)]);
    assert_eq!(rig.snap().hispeed, Some(r(320, 100)));
    // One START after the reopening, 30 ms after the gap's: no double press.
    rig.keys(60 * MS, &[(E, true)]).keys(70 * MS, &[(E, false)]);
    assert!(!rig.snap().options.covers().sudden.on);
    assert_eq!(rig.driver.accepted(), accepted);
    // Released and pressed afresh, F2 steps, and repeats.
    rig.keys(80 * MS, &[(F2, false)])
        .keys(90 * MS, &[(F2, true)]);
    assert_eq!(rig.snap().hispeed, Some(r(345, 100)));
    rig.keys(100 * MS, &[(F2, true)]);
    let s = rig.snap();
    assert_eq!(s.hispeed, Some(r(370, 100)));
    assert_eq!(s.options.hispeed().classic_hispeed_x100, 370);
    assert_eq!(rig.driver.accepted(), accepted + 2);
}

/// A16: the opening's phase (recording opened) reopens the acceptance,
/// with no reopening asked: the starting tuning has none of the gap's
/// presses, a press held from the gap stays discarded, and a fresh press
/// acts in play.
#[test]
fn the_opened_phase_reopens_acceptance() {
    let mut rig = Rig::new(options(SpeedMode::Classic, 320, 300, Covers::default()));
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.drain();
    rig.close();
    rig.keys(10 * MS, &[(F2, true)]);
    assert_eq!(rig.snap().hispeed, Some(r(320, 100)));
    assert!(!rig.driver.accepting());
    rig.open(20 * MS, 1_020 * MS, 1);
    rig.drain();
    assert!(rig.driver.accepting());
    assert_eq!(
        rig.driver.opened(1).unwrap().hispeed,
        Some(replay::HiSpeedRatio { num: 16, den: 5 })
    );
    rig.keys(30 * MS, &[(F2, true)]);
    assert_eq!(rig.snap().hispeed, Some(r(320, 100)), "held from the gap");
    rig.keys(40 * MS, &[(F2, false)])
        .keys(50 * MS, &[(F2, true)]);
    assert_eq!(rig.snap().hispeed, Some(r(345, 100)));
}

/// [`Pump`] with an ingress gate: the word the drain validates against.
struct GatedPump<'a> {
    pump: Pump<'a>,
    word: input::Ingress,
}

impl InputPump for GatedPump<'_> {
    fn request_fence(&mut self, n: u64) {
        self.pump.request_fence(n);
    }
    fn wake(&mut self) {
        self.pump.wake();
    }
    fn display_offset_us(&self) -> i64 {
        self.pump.display_offset_us()
    }
    fn with_ingress(&mut self, f: &mut game::IngressDrain<'_>) {
        f(Some(self.word), &|| {});
    }
    fn park(&mut self) -> Result<(), InputWaitError> {
        self.pump.park()
    }
}

/// E16 and E16b at the driver: F2 read by the input thread under the
/// word open before the choice and left undrained in transit, or read
/// during the decision screen under the closed word, does not act when
/// drained after the choice or after the reopening: the hi-speed and the
/// reducer's count are unchanged. The same tap drained under the word it
/// was stamped with acts (the positive control), and after the reopening
/// a fresh press acts.
#[test]
fn an_observation_stamped_under_another_word_does_not_act() {
    for case in ["in transit", "in the epoch", "control"] {
        let mut rig = Rig::new(PlayOptions::default());
        let record = std::sync::Arc::new(input::ActivityRecord::default());
        rig.set
            .adjustment_mut()
            .unwrap()
            .set_activity(std::sync::Arc::clone(&record));
        rig.enter(-2_000_000);
        rig.attach(tempo(150, None), -2_000_000).unwrap();
        let mut word = record.ingress();
        let drain = |rig: &mut Rig, word| {
            let mut pump = GatedPump {
                pump: Pump {
                    set: &mut rig.set,
                    now: TimeUs(rig.now),
                    offset: rig.offset,
                },
                word,
            };
            rig.driver.pump(&mut pump);
            *rig.reader.read()
        };
        let before = drain(&mut rig, word);
        let accepted = rig.driver.accepted();
        let close = |word: &mut input::Ingress| {
            *word = word.next(false);
            record.set_ingress(*word);
        };
        match case {
            "in transit" => {
                rig.tap(10 * MS, F2);
                close(&mut word);
            }
            "in the epoch" => {
                close(&mut word);
                rig.tap(10 * MS, F2);
            }
            _ => {
                rig.tap(10 * MS, F2);
            }
        }
        if case != "control" {
            word = word.next(true);
            record.set_ingress(word);
        }
        rig.tick(11 * MS);
        let after = drain(&mut rig, word);
        if case == "control" {
            assert_eq!(after.hispeed, Some(r(37, 10)), "{case}");
            assert_eq!(rig.driver.accepted(), accepted + 1, "{case}");
            continue;
        }
        assert_eq!(after.hispeed, before.hispeed, "{case}");
        assert_eq!(rig.driver.accepted(), accepted, "{case}");
        rig.tap(20 * MS, F2);
        rig.tick(21 * MS);
        assert_eq!(drain(&mut rig, word).hispeed, Some(r(37, 10)), "{case}");
    }
}

/// The input side inside a decision screen being left: the fence asked is
/// never answered, and every look leaves.
struct Leaving {
    asked: Option<u64>,
}

impl InputPump for Leaving {
    fn request_fence(&mut self, n: u64) {
        self.asked = Some(n);
    }
    fn wake(&mut self) {}
    fn display_offset_us(&self) -> i64 {
        0
    }
    fn look(&mut self) -> game::Verdict {
        game::Verdict::End
    }
    fn park(&mut self) -> Result<(), InputWaitError> {
        Err(InputWaitError::Timeout)
    }
}

/// A1 (driver), A5: a provisional close whose fence a look left stops
/// unanswered with nothing latched, and keeps no hold: its marker,
/// answered late, stops no drain (F2 pressed after it acts), and a newer
/// fence is answered by its own number.
#[test]
fn a_stopped_provisional_close_leaves_no_hold() {
    let mut rig = Rig::new(options(SpeedMode::Classic, 320, 300, Covers::default()));
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.drain();
    let mut leaving = Leaving { asked: None };
    rig.driver.close_provisionally(&mut leaving).unwrap();
    let late = leaving.asked.expect("its fence asked");
    // The late answer, then a press after it.
    let now = TimeUs(rig.now);
    rig.set.adjustment_mut().unwrap().fence(late, now);
    rig.keys(10 * MS, &[(F2, true)]);
    assert_eq!(
        rig.snap().hispeed,
        Some(r(345, 100)),
        "drained past the marker"
    );
    // Nothing latched: the next fence is asked and answered.
    rig.close();
    assert!(!rig.driver.accepting());
}

/// A1 (driver): an attempt's entry whose fence a look left is not entered
/// and latches nothing: the served form returns the verdict, and the next
/// entry goes through.
#[test]
fn a_left_entry_fence_latches_nothing() {
    let mut rig = Rig::new(options(SpeedMode::Classic, 320, 300, Covers::default()));
    let mut leaving = Leaving { asked: None };
    assert_eq!(
        rig.driver.enter_attempt_served(None, &mut leaving),
        Ok(game::Served::Left(game::Verdict::End))
    );
    assert!(rig.driver.failure_cell().get().is_none());
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
}

/// A cover switched between attempts (the select screen's skin) is
/// published at once, saved with the options, and solved at the next
/// attachment: Floating at 150 BPM and green 300 gives 16/5 with no
/// cover and 56/25 with SUDDEN+ at 300‰ on. Refused while an attempt is
/// attached and not yet prepared, changing nothing.
#[test]
fn a_cover_set_between_attempts_reaches_the_next_attachment() {
    let off = Covers {
        sudden: Cover {
            amount: 300,
            on: false,
        },
        ..Covers::default()
    };
    let mut rig = Rig::new(options(SpeedMode::Floating, 100, 300, off));
    rig.enter(-2_000_000);
    rig.attach(tempo(150, None), -2_000_000).unwrap();
    assert_eq!(rig.drain().hispeed, Some(r(16, 5)));
    assert_eq!(
        rig.driver.set_cover_on(game::CoverKind::Sudden, true),
        Err(game::CoverRefused::Attached)
    );
    assert!(!rig.driver.options().covers().sudden.on);
    // The attempt prepared and played: back between attempts.
    let _ = rig.driver.prepared();
    rig.driver
        .set_cover_on(game::CoverKind::Sudden, true)
        .unwrap();
    assert!(rig.reader.read().options.covers().sudden.on);
    assert_eq!(rig.driver.options().covers(), sudden(300));
    rig.enter(-2_000_000);
    rig.attach(tempo(150, None), -2_000_000).unwrap();
    assert_eq!(rig.drain().hispeed, Some(r(56, 25)));
    // Off again, and the lift on: each switch alone.
    rig.driver.invalidate();
    rig.driver
        .set_cover_on(game::CoverKind::Sudden, false)
        .unwrap();
    rig.driver
        .set_cover_on(game::CoverKind::Lift, true)
        .unwrap();
    let c = rig.driver.options().covers();
    assert_eq!((c.sudden.on, c.lift.on, c.hidden.on), (false, true, false));
    assert_eq!(c.sudden.amount, 300);
}
