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

const Q: u32 = 0x10;
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
    rig.keys(10 * MS, &[(Q, true)]);
    assert!(rig.snap().adjusting);
    rig.enter(-2_000_000);
    rig.attach(tempo(150, None), -2_000_000).unwrap();
    let s = rig.snap();
    assert!(s.adjusting, "START is still held");
    assert!(!s.options.covers().sudden.on);
    // Released, then pressed once: with the press before the retry this
    // would be a double press, but the history started afresh.
    rig.keys(100 * MS, &[(Q, false)])
        .keys(150 * MS, &[(Q, true)])
        .keys(160 * MS, &[(Q, false)]);
    assert!(!rig.snap().options.covers().sudden.on);
    // Two fresh presses toggle SUDDEN+.
    rig.keys(200 * MS, &[(Q, true)])
        .keys(210 * MS, &[(Q, false)]);
    assert!(rig.snap().options.covers().sudden.on);
}

#[test]
fn a_loss_survives_the_attempts_entry_and_a_verified_baseline_ends_it() {
    let mut rig = Rig::new(PlayOptions::default());
    rig.keys(10 * MS, &[(Q, true)]);
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
        rig.keys(1_070_000, &[(W, false), (Z, false), (Q, true)]);
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
    rig.keys(10 * MS, &[(Q, true)]);
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
                rig.keys(30 * MS, &[(W, false), (Q, true)]);
            },
            &|rig| {
                rig.tap(40 * MS, DOWN);
            },
            &|rig| {
                rig.keys(50 * MS, &[(Q, false)]);
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
    rig.keys(2_000_000, &[(Q, true), (AUX_UP, true)])
        .keys(2_020_000, &[(AUX_UP, false), (Q, false)]);
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
    rig.keys(300 * MS, &[(Q, true)])
        .keys(350 * MS, &[(Q, false)]);
    rig.drain();
    rig.rearm(400 * MS, 1_400_000, 2);
    let s = rig.snap();
    assert_eq!(s.hispeed, Some(r(37, 10)), "no new solve at the rearm");
    assert_eq!(s.generation, 2);
    assert!(!s.options.covers().sudden.on);
    // The press that cancelled and one after the rearm, 300 ms apart: a
    // double press.
    rig.keys(600 * MS, &[(Q, true)])
        .keys(610 * MS, &[(Q, false)]);
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
    rig.keys(4_000_000, &[(Q, true)])
        .tap(4_010_000, AUX_UP)
        .keys(4_020_000, &[(Q, false)]);
    assert_eq!(rig.snap().hispeed, Some(r(321, 100)));
    // SUDDEN+ shown at 0 by a double press solves at the event's tempo:
    // 144000 / (300 · 300) = 1.6.
    rig.keys(5_000_000, &[(Q, true)])
        .keys(5_010_000, &[(Q, false)])
        .keys(5_100_000, &[(Q, true)])
        .keys(5_110_000, &[(Q, false)]);
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
    rig.keys(10 * MS, &[(Q, true)]).keys(20 * MS, &[(Q, false)]);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.keys(300 * MS, &[(Q, true)])
        .keys(310 * MS, &[(Q, false)]);
    assert!(rig.snap().options.covers().sudden.on);
}

#[test]
fn the_previous_charts_double_press_does_not() {
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-1_000_000);
    rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.keys(10 * MS, &[(Q, true)]).keys(20 * MS, &[(Q, false)]);
    // The next chart's loading starts within 500 ms.
    rig.enter(-1_000_000);
    rig.keys(300 * MS, &[(Q, true)])
        .keys(310 * MS, &[(Q, false)]);
    assert!(!rig.snap().options.covers().sudden.on);
}

#[test]
fn a_withdrawn_attempt_re_solves_with_a_new_epoch() {
    let mut rig = Rig::new(PlayOptions::default());
    rig.enter(-1_000_000);
    let first = rig.attach(tempo(150, None), -1_000_000).unwrap();
    rig.tap(10 * MS, F2);
    assert_eq!(rig.snap().hispeed, Some(r(37, 10)));
    // Withdrawn before recording opened: nothing pending or active.
    rig.driver.invalidate();
    assert!(!rig.driver.has_pending());
    rig.enter(-1_000_000);
    let second = rig.attach(tempo(150, None), -1_000_000).unwrap();
    assert!(second > first);
    let s = rig.drain();
    assert_eq!((s.hispeed, s.epoch), (Some(r(16, 5)), second));
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
    rig.keys(10 * MS, &[(Q, true)]);
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
    rig.keys(200 * MS, &[(Q, false)])
        .keys(300 * MS, &[(Q, true)])
        .keys(310 * MS, &[(Q, false)]);
    assert!(!rig.snap().options.covers().sudden.on);
    assert_eq!(rig.driver.diagnostics().out_of_order, 0);
}
