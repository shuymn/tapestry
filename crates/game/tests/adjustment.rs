//! The play adjustments' reducer and arithmetic (design.md "Play
//! Adjustments"; docs/stage3.md S10-04a-1): the exact witnesses with
//! injected tempos, every dispatch cell, opposite inputs, held transitions,
//! gestures and drain splits.

use bms_model::{Speed, TimeUs};
use game::adjustment::{
    Adjustment, AxisQuantum, ControlId, ControlRoles, Event, Head, HeldSet, Observation,
    PresentationReader, PresentationSnapshot, PresentationWriter, Role, TempoMap,
    presentation_link,
};
use game::{
    Cover, CoverKind, Covers, HiSpeedReference, HiSpeedSettings, PlayOptions, PlayOptionsError,
    Ratio, References, SpeedMode, duration_ms, green, solve, step_hispeed,
};

const START: ControlId = ControlId(0);
const SELECT: ControlId = ControlId(1);
const AUX_NEG: ControlId = ControlId(2);
const AUX_POS: ControlId = ControlId(3);
const SCRATCH_POS: ControlId = ControlId(4);
const SCRATCH_NEG: ControlId = ControlId(5);
const WHITE: ControlId = ControlId(6);
const BLACK: ControlId = ControlId(7);
const F1: ControlId = ControlId(8);
const F2: ControlId = ControlId(9);
const START_2: ControlId = ControlId(10);
const AXIS: ControlId = ControlId(11);
const MS: i64 = 1_000;

fn r(num: u64, den: u64) -> Ratio {
    Ratio::new(num, den).unwrap()
}

fn bpm(n: u64) -> Ratio {
    Ratio::integer(n)
}

fn roles() -> ControlRoles {
    let mut roles = ControlRoles::default();
    for (c, role) in [
        (START, Role::Start),
        (SELECT, Role::Select),
        (AUX_NEG, Role::AuxNegative),
        (AUX_POS, Role::AuxPositive),
        (SCRATCH_POS, Role::ScratchPositive),
        (SCRATCH_NEG, Role::ScratchNegative),
        (WHITE, Role::White),
        (BLACK, Role::Black),
        (F1, Role::Slower),
        (F2, Role::Faster),
        (START_2, Role::Start),
        (AXIS, Role::ScratchPositive),
    ] {
        assert!(roles.set(c, role));
    }
    roles
}

/// A tempo map: each tempo from its chart time on, the first before it.
#[derive(Clone, Debug)]
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

fn constant(n: u64) -> Map {
    Map(vec![(0, bpm(n))])
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

struct Rig {
    a: Adjustment,
    w: PresentationWriter,
    r: PresentationReader,
    map: Map,
    now: i64,
    seq: u64,
    log: Vec<Observation>,
}

impl Rig {
    fn new(options: PlayOptions, map: Map) -> Self {
        let a = Adjustment::new(options, roles(), TimeUs(-2_000_000), 0);
        let (w, r) = presentation_link(&a.snapshot());
        Self {
            a,
            w,
            r,
            map,
            now: 0,
            seq: 0,
            log: Vec::new(),
        }
    }

    /// Floating prepared from START at `start` BPM, at 1×.
    fn prepared(options: PlayOptions, map: Map) -> Self {
        let mut rig = Self::new(options, map);
        let start = rig.map.0[0].1;
        rig.ev(Event::Prepare {
            references: References::all(start),
            speed: Speed::NORMAL,
        });
        rig
    }

    fn at(&mut self, us: i64) -> &mut Self {
        assert!(us >= self.now);
        self.now = us;
        self
    }

    fn ev(&mut self, event: Event) -> &mut Self {
        self.seq += 1;
        let o = Observation {
            time: TimeUs(self.now),
            seq: self.seq,
            event,
        };
        self.log.push(o);
        self.a.observe(o, &self.map);
        self
    }

    fn press(&mut self, c: ControlId) -> &mut Self {
        self.ev(Event::Press(c))
    }

    fn release(&mut self, c: ControlId) -> &mut Self {
        self.ev(Event::Release(c))
    }

    fn tap(&mut self, c: ControlId) -> &mut Self {
        self.press(c).release(c)
    }

    /// One microsecond later, a tick closes the open group; then publishes.
    fn snap(&mut self) -> PresentationSnapshot {
        self.now += 1;
        self.ev(Event::Tick);
        self.a.drain([], &self.map, &mut self.w);
        *self.r.read()
    }
}

fn h(s: &PresentationSnapshot) -> Ratio {
    s.hispeed.unwrap()
}

fn g(s: &PresentationSnapshot) -> u32 {
    s.options.hispeed().green_target
}

fn native_green(b: Ratio, s: &PresentationSnapshot) -> u64 {
    green(b, h(s), &s.options.covers()).unwrap().round_half_up()
}

// ---- Exact witnesses ----

#[test]
fn floating_solves_150_bpm_to_3_2_with_500_ms_and_green_300() {
    let mut rig = Rig::prepared(PlayOptions::default(), constant(150));
    let s = rig.snap();
    assert_eq!(h(&s), r(16, 5));
    assert_eq!(
        duration_ms(bpm(150), h(&s), &Covers::default()).unwrap(),
        bpm(500)
    );
    assert_eq!(native_green(bpm(150), &s), 300);
}

#[test]
fn lift_and_sudden_solve_together_and_lift_counts_in_the_green_number() {
    let covers = Covers {
        sudden: Cover {
            amount: 200,
            on: true,
        },
        lift: Cover {
            amount: 300,
            on: true,
        },
        ..Covers::default()
    };
    let hs = solve(bpm(150), 300, &covers).unwrap();
    assert_eq!(hs, r(224, 125));
    assert_eq!(duration_ms(bpm(150), hs, &covers).unwrap(), bpm(500));
    assert_eq!(green(bpm(150), hs, &covers).unwrap(), bpm(300));
    // H 3.2 with LIFT 300 alone: 350 ms, green 210.
    let lift = Covers {
        lift: Cover {
            amount: 300,
            on: true,
        },
        ..Covers::default()
    };
    assert_eq!(duration_ms(bpm(150), r(16, 5), &lift).unwrap(), bpm(350));
    assert_eq!(green(bpm(150), r(16, 5), &lift).unwrap(), bpm(210));
}

#[test]
fn a_floating_faster_key_is_an_override_that_a_retry_replaces() {
    let mut rig = Rig::prepared(PlayOptions::default(), constant(150));
    rig.at(10 * MS).tap(F2);
    let s = rig.snap();
    assert_eq!(h(&s), r(37, 10));
    assert_eq!(
        duration_ms(bpm(150), h(&s), &Covers::default()).unwrap(),
        r(16000, 37)
    );
    assert_eq!(native_green(bpm(150), &s), 259);
    assert_eq!(g(&s), 300);
    // A retry restores Floating from G.
    let saved = rig.a.options();
    rig.a.begin_attempt(saved, TimeUs(-2_000_000), 0);
    rig.ev(Event::Prepare {
        references: References::all(bpm(150)),
        speed: Speed::NORMAL,
    });
    assert_eq!(h(&rig.snap()), r(16, 5));
}

#[test]
fn classic_keeps_its_edited_multiplier_across_a_retry() {
    let mut rig = Rig::prepared(
        options(SpeedMode::Classic, 320, 300, Covers::default()),
        constant(150),
    );
    rig.at(10 * MS).tap(F2);
    let s = rig.snap();
    assert_eq!(h(&s), r(345, 100));
    assert_eq!(s.options.hispeed().classic_hispeed_x100, 345);
    let saved = rig.a.options();
    rig.a.begin_attempt(saved, TimeUs(-2_000_000), 0);
    assert_eq!(h(&rig.snap()), r(345, 100));
}

/// The chart at 150 BPM, 300 from 5 s; the head on the chart clock in the
/// 300 BPM part.
fn tempo_change(options: PlayOptions) -> Rig {
    let mut rig = Rig::prepared(options, Map(vec![(0, bpm(150)), (5_000_000, bpm(300))]));
    rig.at(MS).ev(Event::Phase {
        head: Head::Clock {
            at: TimeUs(MS),
            chart: TimeUs(10_000_000),
        },
        generation: 1,
    });
    rig
}

#[test]
fn a_fine_step_with_every_cover_off_does_not_solve() {
    let mut rig = tempo_change(PlayOptions::default());
    rig.at(10 * MS).press(START).tap(SCRATCH_POS).release(START);
    let s = rig.snap();
    assert_eq!(h(&s), r(321, 100));
    assert_eq!(g(&s), 300);
}

#[test]
fn a_clamped_sudden_decrease_still_solves_at_the_current_tempo() {
    let mut rig = tempo_change(options(SpeedMode::Floating, 100, 300, sudden(0)));
    assert_eq!(h(&rig.snap()), r(16, 5));
    rig.at(10 * MS).press(START).tap(SCRATCH_NEG).release(START);
    let s = rig.snap();
    assert_eq!(s.options.covers().sudden.amount, 0);
    assert_eq!(h(&s), r(8, 5));
    assert_eq!(native_green(bpm(300), &s), 300);
}

#[test]
fn a_target_step_solves_exactly() {
    let mut rig = Rig::prepared(
        options(SpeedMode::Floating, 100, 300, sudden(300)),
        constant(300),
    );
    rig.at(10 * MS).press(SELECT).tap(WHITE).release(SELECT);
    let s = rig.snap();
    assert_eq!(g(&s), 299);
    assert_eq!(h(&s), r(336, 299));
    assert_eq!(
        duration_ms(bpm(300), h(&s), &s.options.covers()).unwrap(),
        r(1495, 3)
    );
    assert_eq!(native_green(bpm(300), &s), 299);
}

fn double_start(rig: &mut Rig, at: i64) {
    rig.at(at).tap(START);
    rig.at(at + 100 * MS).tap(START);
}

#[test]
fn showing_sudden_again_solves_in_floating_only() {
    // Prepared at 150 BPM to 2.56, then played in the 300 BPM part.
    let mut rig = tempo_change(options(SpeedMode::Floating, 100, 300, sudden(200)));
    assert_eq!(h(&rig.snap()), r(64, 25));
    double_start(&mut rig, 10 * MS);
    let s = rig.snap();
    assert!(!s.options.covers().sudden.on);
    assert_eq!(h(&s), r(64, 25));
    assert_eq!(
        duration_ms(bpm(300), h(&s), &s.options.covers()).unwrap(),
        r(625, 2)
    );
    assert_eq!(native_green(bpm(300), &s), 188);
    double_start(&mut rig, 1_000 * MS);
    let s = rig.snap();
    assert!(s.options.covers().sudden.on);
    assert_eq!(h(&s), r(32, 25));
    assert_eq!(native_green(bpm(300), &s), 300);

    let mut rig = tempo_change(options(SpeedMode::Classic, 256, 300, sudden(200)));
    double_start(&mut rig, 10 * MS);
    double_start(&mut rig, 1_000 * MS);
    let s = rig.snap();
    assert!(s.options.covers().sudden.on);
    assert_eq!(h(&s), r(64, 25));
    assert_eq!(native_green(bpm(300), &s), 150);
}

#[test]
fn native_green_rounds_half_up() {
    // T = 502.5 ms exactly.
    let t = duration_ms(bpm(150), r(640, 201), &Covers::default()).unwrap();
    assert_eq!(t, r(1005, 2));
    assert_eq!(
        green(bpm(150), r(640, 201), &Covers::default())
            .unwrap()
            .round_half_up(),
        302
    );
}

#[test]
fn an_unreachable_target_clamps_the_multiplier_and_keeps_the_target() {
    let mut rig = Rig::prepared(PlayOptions::default(), constant(10));
    let s = rig.snap();
    assert_eq!(h(&s), bpm(20));
    assert_eq!(
        duration_ms(bpm(10), h(&s), &Covers::default()).unwrap(),
        bpm(1200)
    );
    assert_eq!(native_green(bpm(10), &s), 720);
    assert_eq!(g(&s), 300);
    let mut rig = Rig::prepared(PlayOptions::default(), constant(100_000));
    let s = rig.snap();
    assert_eq!(h(&s), r(1, 100));
    assert_eq!(
        duration_ms(bpm(100_000), h(&s), &Covers::default()).unwrap(),
        bpm(240)
    );
    assert_eq!(native_green(bpm(100_000), &s), 144);
    assert_eq!(g(&s), 300);
}

#[test]
fn steps_clamp_the_result_and_inward_steps_always_work() {
    assert_eq!(step_hispeed(r(198, 10), 50).unwrap(), bpm(20));
    assert_eq!(step_hispeed(bpm(20), -50).unwrap(), r(195, 10));
    assert_eq!(step_hispeed(r(3, 10), -50).unwrap(), r(1, 100));
    assert_eq!(step_hispeed(r(1, 100), 50).unwrap(), r(51, 100));
    // A positive result below the bound clamps too.
    assert_eq!(step_hispeed(r(1, 75), -1).unwrap(), r(1, 100));
    // A saturated step over a large denominator reaches the end without
    // overflowing.
    assert_eq!(step_hispeed(r(1, u64::MAX), i64::MAX).unwrap(), bpm(20));
}

#[test]
fn a_full_sudden_gives_zero_and_reopening_solves() {
    let mut rig = Rig::prepared(
        options(SpeedMode::Floating, 100, 300, sudden(1000)),
        constant(150),
    );
    let s = rig.snap();
    assert_eq!(h(&s), r(1, 100));
    assert_eq!(
        duration_ms(bpm(150), h(&s), &s.options.covers()).unwrap(),
        bpm(0)
    );
    assert_eq!(native_green(bpm(150), &s), 0);
    rig.at(10 * MS).press(START);
    for i in 0..10 {
        rig.at((20 + i) * MS).tap(AUX_NEG);
    }
    rig.release(START);
    let s = rig.snap();
    assert_eq!(s.options.covers().sudden.amount, 900);
    assert_eq!(h(&s), r(32, 100));
    assert_eq!(
        duration_ms(bpm(150), h(&s), &s.options.covers()).unwrap(),
        bpm(500)
    );
    assert_eq!(native_green(bpm(150), &s), 300);
}

#[test]
fn a_full_lift_gives_zero_duration() {
    let lift = Covers {
        lift: Cover {
            amount: 1000,
            on: true,
        },
        ..Covers::default()
    };
    assert_eq!(duration_ms(bpm(150), r(16, 5), &lift).unwrap(), bpm(0));
    assert_eq!(green(bpm(150), r(16, 5), &lift).unwrap(), bpm(0));
}

#[test]
fn the_practice_speed_applies_once() {
    let mut rig = Rig::new(PlayOptions::default(), constant(150));
    rig.ev(Event::Prepare {
        references: References::all(bpm(150)),
        speed: Speed::FASTEST,
    });
    let s = rig.snap();
    assert_eq!(h(&s), r(32, 15));
    assert_eq!(
        duration_ms(bpm(225), h(&s), &Covers::default()).unwrap(),
        bpm(500)
    );
}

/// G 360, SUDDEN+ 500: +1 at 150 BPM, then −1 in the 300 BPM part.
fn delayed_drain() -> Vec<Observation> {
    let mut rig = Rig::prepared(
        options(SpeedMode::Floating, 100, 360, sudden(500)),
        Map(vec![(0, bpm(150)), (5_000_000, bpm(300))]),
    );
    rig.at(MS).ev(Event::Phase {
        head: Head::Clock {
            at: TimeUs(MS),
            chart: TimeUs(0),
        },
        generation: 1,
    });
    rig.at(10 * MS).press(START).tap(SCRATCH_POS);
    rig.at(6_000 * MS).tap(SCRATCH_NEG).release(START);
    rig.now += 1;
    rig.ev(Event::Tick);
    rig.log
}

/// `log` from a fresh reducer with `options` on `map`, drained at each of
/// `splits`; the publication counter cleared.
fn drained(
    options: PlayOptions,
    map: &Map,
    log: &[Observation],
    splits: &[usize],
) -> PresentationSnapshot {
    let mut a = Adjustment::new(options, roles(), TimeUs(-2_000_000), 0);
    let (mut w, mut r) = presentation_link(&a.snapshot());
    let mut from = 0;
    for &to in splits.iter().chain([&log.len()]) {
        a.drain(log[from..to].iter().copied(), map, &mut w);
        from = to;
    }
    let mut s = *r.read();
    s.sequence = 0;
    s
}

#[test]
fn a_delayed_drain_gives_one_state_whatever_its_splits() {
    let log = delayed_drain();
    let o = options(SpeedMode::Floating, 100, 360, sudden(500));
    let map = Map(vec![(0, bpm(150)), (5_000_000, bpm(300))]);
    let replay_split = |log: &[Observation], splits: &[usize]| drained(o, &map, log, splits);
    let whole = replay_split(&log, &[]);
    assert_eq!(whole.options.covers().sudden.amount, 500);
    assert_eq!(h(&whole), r(2, 3));
    assert_eq!(
        duration_ms(bpm(300), h(&whole), &whole.options.covers()).unwrap(),
        bpm(600)
    );
    for i in 0..=log.len() {
        assert_eq!(replay_split(&log, &[i]), whole, "split at {i}");
        for j in i..=log.len() {
            assert_eq!(replay_split(&log, &[i, j]), whole, "split at {i}, {j}");
        }
    }
}

#[test]
fn speed_then_target_at_one_time_in_either_order() {
    for order in [[F1, WHITE], [WHITE, F1]] {
        let mut rig = Rig::prepared(PlayOptions::default(), constant(300));
        rig.at(10 * MS).press(SELECT);
        rig.at(20 * MS).press(order[0]).press(order[1]);
        rig.at(30 * MS)
            .release(order[0])
            .release(order[1])
            .release(SELECT);
        let s = rig.snap();
        assert_eq!(h(&s), r(480, 299), "{order:?}");
        assert_eq!(g(&s), 299);
    }
}

#[test]
fn the_head_while_loading_is_frozen_at_the_opening_never_the_loading_clock() {
    // 150 BPM, 300 from 50 ms; frozen at −2 s: the target solves at 150.
    let map = Map(vec![(0, bpm(150)), (50_000, bpm(300))]);
    let mut rig = Rig::prepared(options(SpeedMode::Floating, 100, 300, sudden(200)), map);
    rig.at(MS).press(SELECT).tap(BLACK);
    rig.at(10_000 * MS).tap(WHITE).release(SELECT);
    let s = rig.snap();
    assert_eq!(g(&s), 300);
    assert_eq!(h(&s), r(64, 25));
}

#[test]
fn a_practice_round_head_takes_the_display_offset_once() {
    // Section at 60 s, lead-in 2 s, restore 200 ms ahead: base 57.8 s;
    // 150 BPM changing to 300 at 57.85 s.
    let map = Map(vec![(0, bpm(150)), (57_850_000, bpm(300))]);
    for (offset, expected) in [(100_000, r(32, 25)), (0, r(64, 25))] {
        let mut rig = Rig::new(
            options(SpeedMode::Floating, 100, 300, sudden(200)),
            map.clone(),
        );
        rig.ev(Event::Phase {
            head: Head::Frozen {
                opens_at: TimeUs(57_800_000),
            },
            generation: 0,
        });
        rig.ev(Event::Prepare {
            references: References::all(bpm(150)),
            speed: Speed::NORMAL,
        });
        rig.at(MS).ev(Event::DisplayOffset(offset));
        rig.at(2 * MS).press(SELECT).tap(BLACK);
        rig.at(3 * MS).tap(WHITE).release(SELECT);
        assert_eq!(h(&rig.snap()), expected, "offset {offset}");
    }
}

// ---- The dispatch table ----

#[derive(Clone, Copy, Debug)]
enum Held {
    Neither,
    Start,
    Select,
    Both,
}

/// What one tap of `key` under `held` must do, from H, G and SUDDEN+ before.
fn expected(
    held: Held,
    key: ControlId,
    mode: SpeedMode,
    covered: bool,
    h0: Ratio,
    g0: u32,
) -> (Ratio, u32, u16) {
    let floating = mode == SpeedMode::Floating;
    let step: i64 = if floating { 50 } else { 25 };
    let cover0 = if covered { 500 } else { 0 };
    let covers = |amount| {
        if covered {
            sudden(amount)
        } else {
            Covers::default()
        }
    };
    let speed = |by: i64| (step_hispeed(h0, by).unwrap(), g0, cover0);
    let target = |by: i64| {
        let g = (i64::from(g0) + by) as u32;
        (solve(bpm(150), g, &covers(cover0)).unwrap(), g, cover0)
    };
    let cover = |by: i64, fine: i64| {
        if covered {
            let amount = (cover0 as i64 + by) as u16;
            let h = if floating {
                solve(bpm(150), g0, &covers(amount)).unwrap()
            } else {
                h0
            };
            (h, g0, amount)
        } else {
            speed(fine)
        }
    };
    let none = (h0, g0, cover0);
    match (held, key) {
        (Held::Both, _) => none,
        (_, F1) => speed(-step),
        (_, F2) => speed(step),
        (Held::Neither, _) => none,
        (Held::Start, WHITE) => speed(-step),
        (Held::Start, BLACK) => speed(step),
        (Held::Start, SCRATCH_POS) => cover(1, 1),
        (Held::Start, SCRATCH_NEG) => cover(-1, -1),
        (Held::Start, AUX_NEG) => cover(-10, -1),
        (Held::Start, AUX_POS) => cover(10, 1),
        (Held::Select, _) if !floating => none,
        (Held::Select, SCRATCH_POS | BLACK | AUX_POS) => target(1),
        (Held::Select, SCRATCH_NEG | WHITE | AUX_NEG) => target(-1),
        other => panic!("no cell for {other:?}"),
    }
}

#[test]
fn every_dispatch_cell_does_only_its_action() {
    let keys = [
        SCRATCH_POS,
        SCRATCH_NEG,
        WHITE,
        BLACK,
        AUX_NEG,
        AUX_POS,
        F1,
        F2,
    ];
    for mode in [SpeedMode::Floating, SpeedMode::Classic] {
        for covered in [true, false] {
            for held in [Held::Neither, Held::Start, Held::Select, Held::Both] {
                for key in keys {
                    let covers = if covered {
                        sudden(500)
                    } else {
                        Covers::default()
                    };
                    let mut rig = Rig::prepared(options(mode, 200, 300, covers), constant(150));
                    let before = rig.snap();
                    let mods: &[ControlId] = match held {
                        Held::Neither => &[],
                        Held::Start => &[START],
                        Held::Select => &[SELECT],
                        Held::Both => &[START, SELECT],
                    };
                    rig.at(10 * MS);
                    for &m in mods {
                        rig.press(m);
                    }
                    rig.at(20 * MS).tap(key);
                    let s = rig.snap();
                    let (eh, eg, ec) = expected(held, key, mode, covered, h(&before), g(&before));
                    let cell = format!("{mode:?} covered {covered} {held:?} {key:?}");
                    assert_eq!(h(&s), eh, "{cell}");
                    assert_eq!(g(&s), eg, "{cell}");
                    assert_eq!(s.options.covers().sudden.amount, ec, "{cell}");
                    assert_eq!(s.options.hispeed().mode, mode, "{cell}");
                    assert_eq!(s.target, before.target, "{cell}");
                    assert_eq!(s.options.covers().sudden.on, covered, "{cell}");
                    let saved = s.options.hispeed().classic_hispeed_x100;
                    if mode == SpeedMode::Classic {
                        assert_eq!(saved, eh.hundredths().unwrap(), "{cell}");
                    } else {
                        assert_eq!(saved, 200, "{cell}");
                    }
                    assert_eq!(s.adjusting, !mods.is_empty(), "{cell}");
                }
            }
        }
    }
}

#[test]
fn lift_and_hidden_take_the_opposite_sign_and_hidden_never_solves() {
    for (kind, solves) in [(CoverKind::Lift, true), (CoverKind::Hidden, false)] {
        let mut covers = Covers::default();
        covers.get_mut(kind).amount = 300;
        covers.get_mut(kind).on = true;
        let mut rig = Rig::prepared(
            options(SpeedMode::Floating, 100, 300, covers),
            constant(150),
        );
        // An override first, so a solve would show.
        rig.at(5 * MS).tap(F2);
        let h0 = h(&rig.snap());
        rig.at(10 * MS).press(START).tap(AUX_POS).release(START);
        let s = rig.snap();
        assert_eq!(s.target, Some(kind));
        assert_eq!(s.options.covers().get(kind).amount, 290);
        if solves {
            assert_eq!(h(&s), solve(bpm(150), 300, &s.options.covers()).unwrap());
            assert_ne!(h(&s), h0);
        } else {
            assert_eq!(h(&s), h0);
        }
    }
}

#[test]
fn pressing_both_only_switches_the_target() {
    let covers = Covers {
        lift: Cover {
            amount: 100,
            on: true,
        },
        hidden: Cover {
            amount: 100,
            on: true,
        },
        ..Covers::default()
    };
    let mut rig = Rig::prepared(
        options(SpeedMode::Floating, 100, 300, covers),
        constant(150),
    );
    let before = rig.snap();
    assert_eq!(before.target, Some(CoverKind::Lift));
    rig.at(10 * MS)
        .press(START)
        .press(SELECT)
        .release(SELECT)
        .release(START);
    let s = rig.snap();
    assert_eq!(s.target, Some(CoverKind::Hidden));
    assert_eq!(s.options, before.options);
    assert_eq!(s.hispeed, before.hispeed);
    // With one of them on, nothing.
    let mut rig = Rig::prepared(
        options(
            SpeedMode::Floating,
            100,
            300,
            Covers {
                lift: Cover {
                    amount: 100,
                    on: true,
                },
                ..Covers::default()
            },
        ),
        constant(150),
    );
    rig.at(10 * MS)
        .press(SELECT)
        .press(START)
        .release(START)
        .release(SELECT);
    assert_eq!(rig.snap().target, Some(CoverKind::Lift));
}

#[test]
fn classic_select_changes_neither_the_mode_nor_the_target() {
    let mut rig = Rig::prepared(
        options(SpeedMode::Classic, 200, 300, sudden(100)),
        constant(150),
    );
    let before = rig.snap();
    rig.at(10 * MS).press(SELECT);
    for (i, key) in [SCRATCH_POS, SCRATCH_NEG, WHITE, BLACK, AUX_NEG, AUX_POS]
        .into_iter()
        .enumerate()
    {
        rig.at((20 + i as i64) * MS).tap(key);
    }
    rig.release(SELECT);
    let s = rig.snap();
    assert_eq!(s.options, before.options);
    assert_eq!(s.hispeed, before.hispeed);
}

// ---- Opposite inputs ----

#[test]
fn opposite_requests_at_one_time_cancel_and_solve_nothing() {
    // An override first, so a solve would show.
    let mut rig = Rig::prepared(
        options(SpeedMode::Floating, 100, 300, sudden(200)),
        constant(150),
    );
    rig.at(5 * MS).tap(F2);
    let before = rig.snap();
    rig.at(10 * MS).press(START);
    rig.at(20 * MS).press(AUX_POS).press(AUX_NEG);
    rig.at(30 * MS)
        .release(AUX_POS)
        .release(AUX_NEG)
        .release(START);
    let s = rig.snap();
    assert_eq!(s.options, before.options);
    assert_eq!(s.hispeed, before.hispeed);
}

#[test]
fn opposite_scratch_directions_held_together_suppress_both_until_a_fresh_press() {
    let mut rig = Rig::prepared(
        options(SpeedMode::Floating, 100, 300, sudden(500)),
        constant(150),
    );
    rig.at(10 * MS).press(START).press(SCRATCH_POS);
    assert_eq!(rig.snap().options.covers().sudden.amount, 501);
    rig.at(20 * MS).press(SCRATCH_NEG);
    rig.at(200 * MS).release(SCRATCH_NEG);
    for t in (250..800).step_by(50) {
        rig.at(t * MS).ev(Event::Tick);
    }
    // Still suppressed after the other was released.
    assert_eq!(rig.snap().options.covers().sudden.amount, 501);
    rig.at(900 * MS).release(SCRATCH_POS).press(SCRATCH_POS);
    assert_eq!(rig.snap().options.covers().sudden.amount, 502);
}

// ---- Gestures ----

#[test]
fn a_digital_scratch_repeats_every_50_ms_skipping_a_stalled_backlog_and_accelerates() {
    let mut rig = Rig::prepared(
        options(SpeedMode::Classic, 200, 300, sudden(0)),
        constant(150),
    );
    rig.at(0).press(START).press(SCRATCH_POS);
    let amount = |rig: &mut Rig| rig.snap().options.covers().sudden.amount;
    assert_eq!(amount(&mut rig), 1);
    rig.at(49 * MS).ev(Event::Tick);
    assert_eq!(amount(&mut rig), 1);
    rig.at(50 * MS).ev(Event::Tick);
    assert_eq!(amount(&mut rig), 2);
    // The producer stalled 300 ms: one step, not six.
    rig.at(350 * MS).ev(Event::Tick);
    assert_eq!(amount(&mut rig), 3);
    rig.at(400 * MS).ev(Event::Tick);
    assert_eq!(amount(&mut rig), 4);
    // From 500 ms in one direction, 10 ‰.
    rig.at(500 * MS).ev(Event::Tick);
    assert_eq!(amount(&mut rig), 14);
    // A reversal starts slow again.
    rig.at(520 * MS).release(SCRATCH_POS).press(ControlId(5));
    assert_eq!(amount(&mut rig), 13);
    rig.at(570 * MS).ev(Event::Tick);
    assert_eq!(amount(&mut rig), 12);
}

#[test]
fn select_and_fine_repeats_never_accelerate() {
    let mut rig = Rig::prepared(PlayOptions::default(), constant(150));
    rig.at(0).press(SELECT).press(SCRATCH_POS);
    for t in (50..=1_000).step_by(50) {
        rig.at(t * MS).ev(Event::Tick);
    }
    assert_eq!(g(&rig.snap()), 321);
    let mut rig = Rig::prepared(PlayOptions::default(), constant(150));
    rig.at(0).press(START).press(SCRATCH_POS);
    for t in (50..=1_000).step_by(50) {
        rig.at(t * MS).ev(Event::Tick);
    }
    assert_eq!(h(&rig.snap()), r(341, 100));
}

#[test]
fn two_start_only_activations_within_500_ms_toggle_sudden_once() {
    let covers = sudden(100);
    let on = |rig: &mut Rig| rig.snap().options.covers().sudden.on;
    let mut rig = Rig::prepared(options(SpeedMode::Classic, 200, 300, covers), constant(150));
    rig.at(0).tap(START);
    rig.at(499_999).tap(START);
    assert!(!on(&mut rig));
    // The pair was consumed: a third press starts a new pair.
    rig.at(600 * MS).tap(START);
    assert!(!on(&mut rig));
    // 500 ms apart is too slow.
    let mut rig = Rig::prepared(options(SpeedMode::Classic, 200, 300, covers), constant(150));
    rig.at(0).tap(START);
    rig.at(500 * MS).tap(START);
    assert!(on(&mut rig));
    // Entering both held clears a pending first press.
    let mut rig = Rig::prepared(options(SpeedMode::Classic, 200, 300, covers), constant(150));
    rig.at(0).tap(START);
    rig.at(100 * MS)
        .press(SELECT)
        .press(START)
        .release(START)
        .release(SELECT);
    rig.at(200 * MS).tap(START);
    assert!(on(&mut rig));
    // Releasing SELECT while START stays held is no START activation, nor
    // is another START binding pressed while START is held.
    let mut rig = Rig::prepared(options(SpeedMode::Classic, 200, 300, covers), constant(150));
    rig.at(0).press(START).press(SELECT).release(SELECT);
    rig.at(100 * MS)
        .press(START_2)
        .release(START_2)
        .release(START);
    rig.at(200 * MS).tap(START);
    assert!(on(&mut rig));
}

#[test]
fn a_wrapping_axis_accumulates_exactly_from_a_fresh_baseline() {
    // 256 counts a revolution: 9·256/2000 = 1.152 counts a step.
    let q = AxisQuantum::wrapping(256).unwrap();
    let axis = |delta| Event::Axis {
        control: AXIS,
        delta,
        quantum: q,
    };
    let mut rig = Rig::prepared(PlayOptions::default(), constant(150));
    rig.at(0).press(SELECT);
    // The first report in a context is the baseline.
    rig.at(MS).ev(axis(100));
    assert_eq!(g(&rig.snap()), 300);
    // 1 + 1 + 1 counts: 2000·3/2304 → 2 steps, remainder kept.
    rig.at(2 * MS).ev(axis(1));
    rig.at(3 * MS).ev(axis(1));
    rig.at(4 * MS).ev(axis(1));
    assert_eq!(g(&rig.snap()), 302);
    // The remainder (1392) carries: 1392 + 1150·2000 → 998 more steps.
    rig.at(5 * MS).ev(axis(1_150));
    assert_eq!(g(&rig.snap()), 1_300);
    // A context change drops the remainder and takes a new baseline.
    rig.at(6 * MS).release(SELECT).press(SELECT);
    rig.at(7 * MS).ev(axis(-500));
    assert_eq!(g(&rig.snap()), 1_300);
    // 1152 counts are exactly 1000 steps from a zero remainder.
    rig.at(8 * MS).ev(axis(-1_152));
    assert_eq!(g(&rig.snap()), 300);
}

// ---- Held transitions ----

#[test]
fn a_held_control_crossing_a_context_change_acts_only_after_a_fresh_press() {
    let covers = sudden(500);
    let amount = |rig: &mut Rig| rig.snap().options.covers().sudden.amount;
    // Mode entry and exit: SELECT pressed and released while the scratch
    // is held under START.
    let mut rig = Rig::prepared(options(SpeedMode::Classic, 200, 300, covers), constant(150));
    rig.at(0).press(START).press(SCRATCH_POS);
    assert_eq!(amount(&mut rig), 501);
    rig.at(10 * MS).press(SELECT);
    rig.at(20 * MS).release(SELECT);
    for t in (50..=600).step_by(50) {
        rig.at(t * MS).ev(Event::Tick);
    }
    assert_eq!(amount(&mut rig), 501);
    // A scratch held before START is pressed takes no action from it.
    let mut rig = Rig::prepared(options(SpeedMode::Classic, 200, 300, covers), constant(150));
    rig.at(0).press(SCRATCH_POS);
    rig.at(10 * MS).press(START);
    for t in (50..=600).step_by(50) {
        rig.at(t * MS).ev(Event::Tick);
    }
    assert_eq!(amount(&mut rig), 500);
    // A scene change, a READY generation, the display offset and a target
    // change each end the context of a held F2.
    for change in [
        Event::Ownership,
        Event::Phase {
            head: Head::Frozen {
                opens_at: TimeUs(0),
            },
            generation: 2,
        },
        Event::DisplayOffset(1_000),
    ] {
        let mut rig = Rig::prepared(options(SpeedMode::Classic, 200, 300, covers), constant(150));
        rig.at(0).press(F2);
        rig.at(10 * MS).ev(Event::Repeat(F2));
        rig.at(20 * MS).ev(change);
        rig.at(30 * MS).ev(Event::Repeat(F2));
        assert_eq!(h(&rig.snap()), r(25 + 25 + 200, 100), "{change:?}");
        rig.at(40 * MS).release(F2).press(F2);
        assert_eq!(h(&rig.snap()), r(275, 100), "{change:?}");
    }
    // A press after a target change takes the new target's action: with
    // SUDDEN+ hidden and no other cover, a fine step.
    let mut rig = Rig::prepared(options(SpeedMode::Classic, 200, 300, covers), constant(150));
    rig.at(0).tap(START);
    rig.at(100 * MS).press(START).press(SCRATCH_POS);
    let s = rig.snap();
    assert!(!s.options.covers().sudden.on);
    assert_eq!(s.options.covers().sudden.amount, 500);
    assert_eq!(h(&s), r(201, 100));
}

#[test]
fn a_loss_acts_on_nothing_until_resynchronized_and_never_makes_up_a_release() {
    let mut rig = Rig::prepared(
        options(SpeedMode::Classic, 200, 300, sudden(500)),
        constant(150),
    );
    rig.at(0).press(START);
    rig.at(10 * MS).ev(Event::Lost);
    let s = rig.snap();
    assert!(!s.adjusting);
    assert!(rig.a.modifiers_held());
    rig.at(20 * MS).tap(AUX_POS).tap(F2);
    assert_eq!(rig.snap().options.covers().sudden.amount, 500);
    // START verified held, the scratch too: neither acts until released.
    rig.at(30 * MS).ev(Event::Resync(
        HeldSet::default().with(START).with(SCRATCH_POS).with(F2),
    ));
    rig.at(40 * MS).ev(Event::Repeat(F2));
    assert_eq!(h(&rig.snap()), r(200, 100));
    assert!(rig.a.modifiers_held());
    assert!(rig.snap().adjusting);
    for t in (50..=600).step_by(50) {
        rig.at(t * MS).ev(Event::Tick);
    }
    assert_eq!(rig.snap().options.covers().sudden.amount, 500);
    rig.at(700 * MS).release(SCRATCH_POS).tap(SCRATCH_POS);
    assert_eq!(rig.snap().options.covers().sudden.amount, 501);
    rig.at(800 * MS).release(START);
    assert!(!rig.a.modifiers_held());
}

// ---- The rest of the contract ----

#[test]
fn before_the_tempo_is_known_covers_and_the_target_change_and_speed_does_nothing() {
    let mut rig = Rig::new(
        options(SpeedMode::Floating, 100, 300, sudden(200)),
        constant(150),
    );
    rig.at(0).press(SELECT).tap(BLACK).release(SELECT);
    rig.at(10 * MS)
        .press(START)
        .tap(AUX_POS)
        .tap(BLACK)
        .release(START);
    rig.at(20 * MS).tap(F2);
    let s = rig.snap();
    assert_eq!(s.hispeed, None);
    assert_eq!(g(&s), 301);
    assert_eq!(s.options.covers().sudden.amount, 210);
    // Then the tempo: the solve uses every change.
    rig.at(30 * MS).ev(Event::Prepare {
        references: References::all(bpm(150)),
        speed: Speed::NORMAL,
    });
    assert_eq!(h(&rig.snap()), solve(bpm(150), 301, &sudden(210)).unwrap());
    // Classic's multiplier needs no tempo.
    let mut rig = Rig::new(
        options(SpeedMode::Classic, 200, 300, Covers::default()),
        constant(150),
    );
    rig.at(0).tap(F2);
    assert_eq!(h(&rig.snap()), r(225, 100));
}

#[test]
fn the_reference_chooses_the_preparation_tempo() {
    let refs = References {
        start: bpm(150),
        maximum: bpm(300),
        main: bpm(200),
        minimum: bpm(100),
    };
    for (reference, b) in [
        (HiSpeedReference::Start, 150),
        (HiSpeedReference::Maximum, 300),
        (HiSpeedReference::Main, 200),
        (HiSpeedReference::Minimum, 100),
    ] {
        let o = PlayOptions::new(
            HiSpeedSettings {
                reference,
                ..HiSpeedSettings::default()
            },
            Covers::default(),
        )
        .unwrap();
        let mut rig = Rig::new(o, constant(150));
        rig.ev(Event::Prepare {
            references: refs,
            speed: Speed::NORMAL,
        });
        assert_eq!(
            h(&rig.snap()),
            solve(bpm(b), 300, &Covers::default()).unwrap()
        );
    }
}

#[test]
fn a_ready_restart_keeps_the_settings_the_target_and_the_double_press() {
    let mut rig = Rig::prepared(
        options(SpeedMode::Floating, 100, 300, sudden(100)),
        constant(150),
    );
    rig.at(0).tap(F2);
    rig.at(10 * MS).tap(START);
    rig.at(20 * MS).ev(Event::Phase {
        head: Head::Clock {
            at: TimeUs(20 * MS),
            chart: TimeUs(-1_000_000),
        },
        generation: 1,
    });
    let before = rig.snap();
    // The second START of the pair, in the new READY.
    rig.at(100 * MS).tap(START);
    let s = rig.snap();
    assert_eq!(s.generation, 1);
    assert_eq!(s.hispeed, before.hispeed);
    assert!(!s.options.covers().sudden.on);
}

#[test]
fn an_unrepresentable_transaction_changes_nothing() {
    // A tempo whose exact terms cannot meet the target's.
    let d = 1_u64 << 56;
    let huge = Ratio::new(150 * d + 1, d).unwrap();
    let mut rig = Rig::prepared(PlayOptions::default(), Map(vec![(0, bpm(150))]));
    rig.map = Map(vec![(0, huge)]);
    let before = rig.snap();
    rig.at(10 * MS).press(SELECT).tap(BLACK).release(SELECT);
    let s = rig.snap();
    assert_eq!(rig.a.diagnostics().unrepresentable, 1);
    assert_eq!(s.options, before.options);
    assert_eq!(s.hispeed, before.hispeed);
}

#[test]
fn every_drain_publishes_one_numbered_snapshot() {
    let mut rig = Rig::prepared(PlayOptions::default(), constant(150));
    let a = rig.snap();
    let b = rig.snap();
    assert_eq!(b.sequence, a.sequence + 1);
    assert_eq!(b.hispeed, Some(r(16, 5)));
}

#[test]
fn play_options_refuse_each_value_outside_its_range_by_name() {
    let base = HiSpeedSettings::default();
    assert_eq!(base.mode, SpeedMode::Floating);
    assert_eq!(base.reference, HiSpeedReference::Start);
    assert_eq!(
        (
            base.classic_hispeed_x100,
            base.classic_step_x100,
            base.floating_step_x100,
            base.green_target
        ),
        (100, 25, 50, 300)
    );
    let cases = [
        (
            "classic_hispeed_x100",
            HiSpeedSettings {
                classic_hispeed_x100: 0,
                ..base
            },
            0,
        ),
        (
            "classic_hispeed_x100",
            HiSpeedSettings {
                classic_hispeed_x100: 2001,
                ..base
            },
            2001,
        ),
        (
            "classic_step_x100",
            HiSpeedSettings {
                classic_step_x100: 101,
                ..base
            },
            101,
        ),
        (
            "floating_step_x100",
            HiSpeedSettings {
                floating_step_x100: 4,
                ..base
            },
            4,
        ),
        (
            "floating_step_x100",
            HiSpeedSettings {
                floating_step_x100: 52,
                ..base
            },
            52,
        ),
        (
            "green_target",
            HiSpeedSettings {
                green_target: 0,
                ..base
            },
            0,
        ),
        (
            "green_target",
            HiSpeedSettings {
                green_target: 6001,
                ..base
            },
            6001,
        ),
    ];
    for (name, settings, value) in cases {
        match PlayOptions::new(settings, Covers::default()) {
            Err(PlayOptionsError::Invalid {
                setting, value: v, ..
            }) => {
                assert_eq!((setting, v), (name, value));
            }
            other => panic!("{name}: {other:?}"),
        }
    }
    let mut o = PlayOptions::default();
    assert!(o.set_covers(sudden(1001)).is_err());
    assert_eq!(o.covers(), Covers::default());
    assert!(o.set_covers(sudden(1000)).is_ok());
    for ok in [
        HiSpeedSettings {
            classic_hispeed_x100: 1,
            classic_step_x100: 1,
            floating_step_x100: 5,
            green_target: 1,
            ..base
        },
        HiSpeedSettings {
            classic_hispeed_x100: 2000,
            classic_step_x100: 100,
            floating_step_x100: 100,
            green_target: 6000,
            ..base
        },
    ] {
        assert!(o.set_hispeed(ok).is_ok());
    }
}

// ---- Exactness at the edges ----

/// 150 + 10⁻¹⁷ BPM: exact, yet its terms leave no room in a product.
fn tight_bpm() -> Ratio {
    r(15_000_000_000_000_000_001, 100_000_000_000_000_000)
}

#[test]
fn the_session_clock_runs_in_real_time_and_the_speed_scales_only_the_tempo() {
    // At 150 %, 150 BPM changing to 300 at session time 1.2 s: an
    // observation 1 s after the anchor is still at 150 (225 playing).
    let map = Map(vec![(0, bpm(150)), (1_200_000, bpm(300))]);
    let mut rig = Rig::new(options(SpeedMode::Floating, 100, 300, sudden(200)), map);
    rig.ev(Event::Prepare {
        references: References::all(bpm(150)),
        speed: Speed::FASTEST,
    });
    rig.ev(Event::Phase {
        head: Head::Clock {
            at: TimeUs(0),
            chart: TimeUs(0),
        },
        generation: 1,
    });
    rig.at(1_000 * MS).press(SELECT).tap(BLACK);
    rig.at(1_001 * MS).tap(WHITE).release(SELECT);
    assert_eq!(h(&rig.snap()), solve(bpm(225), 300, &sudden(200)).unwrap());
}

#[test]
fn a_duration_whose_lowest_terms_fit_is_exact() {
    // B = 150 + 1/(2·10¹⁶): H fits, and T is exactly 500 ms.
    let b = r(3_000_000_000_000_000_001, 20_000_000_000_000_000);
    let hs = solve(b, 300, &Covers::default()).unwrap();
    assert_eq!(duration_ms(b, hs, &Covers::default()).unwrap(), bpm(500));
    assert_eq!(green(b, hs, &Covers::default()).unwrap(), bpm(300));
}

#[test]
fn no_field_left_is_zero_before_any_product() {
    let b = r(3_000_000_000_000_000_001, 20_000_000_000_000_000);
    let hs = solve(b, 300, &Covers::default()).unwrap();
    let lift = Covers {
        lift: Cover {
            amount: 1000,
            on: true,
        },
        ..Covers::default()
    };
    assert_eq!(duration_ms(b, hs, &lift).unwrap(), bpm(0));
}

#[test]
fn a_saturated_step_from_a_large_multiplier_reaches_the_end() {
    let hs = Ratio::new(u64::MAX, u64::MAX - 1).unwrap();
    assert_eq!(step_hispeed(hs, i64::MAX).unwrap(), bpm(20));
    assert_eq!(step_hispeed(hs, i64::MIN).unwrap(), r(1, 100));
}

#[test]
fn a_failed_reattachment_keeps_the_switch_and_the_multiplier() {
    let covers = Covers {
        sudden: Cover {
            amount: 200,
            on: false,
        },
        ..Covers::default()
    };
    let mut rig = Rig::new(
        options(SpeedMode::Floating, 100, 300, covers),
        Map(vec![(0, tight_bpm())]),
    );
    rig.ev(Event::Prepare {
        references: References::all(bpm(150)),
        speed: Speed::NORMAL,
    });
    double_start(&mut rig, 10 * MS);
    let s = rig.snap();
    assert_eq!(rig.a.diagnostics().unrepresentable, 1);
    assert!(!s.options.covers().sudden.on);
    assert_eq!(h(&s), r(16, 5));
}

#[test]
fn a_playback_tempo_that_does_not_fit_fails_the_transaction() {
    let mut rig = Rig::new(PlayOptions::default(), Map(vec![(0, tight_bpm())]));
    rig.ev(Event::Prepare {
        references: References::all(bpm(150)),
        speed: Speed::FASTEST,
    });
    let before = rig.snap();
    rig.at(10 * MS).press(SELECT).tap(BLACK).release(SELECT);
    let s = rig.snap();
    assert_eq!(rig.a.diagnostics().unrepresentable, 1);
    assert_eq!(s.options, before.options);
    assert_eq!(s.hispeed, before.hispeed);
}

#[test]
fn opposite_scratch_presses_at_one_time_cancel_in_either_order() {
    for order in [[SCRATCH_POS, SCRATCH_NEG], [SCRATCH_NEG, SCRATCH_POS]] {
        let mut rig = Rig::prepared(
            options(SpeedMode::Classic, 200, 300, sudden(500)),
            constant(150),
        );
        rig.at(10 * MS).press(START);
        rig.at(20 * MS).press(order[0]).press(order[1]);
        for t in (70..=700).step_by(50) {
            rig.at(t * MS).ev(Event::Tick);
        }
        assert_eq!(rig.snap().options.covers().sudden.amount, 500, "{order:?}");
    }
}

#[test]
fn axis_steps_stay_wide_until_the_result_is_clamped() {
    let q = AxisQuantum::per_step(1).unwrap();
    let axis = |delta| Event::Axis {
        control: AXIS,
        delta,
        quantum: q,
    };
    let mut rig = Rig::prepared(PlayOptions::default(), constant(150));
    rig.at(0).press(SELECT);
    rig.at(MS).ev(axis(0));
    rig.at(2 * MS).ev(axis(1 << 31)).ev(axis(-((1 << 31) - 1)));
    assert_eq!(g(&rig.snap()), 301);
}

#[test]
fn same_time_axis_steps_cancel_exactly_at_the_i64_bounds() {
    let q = AxisQuantum::per_step(1).unwrap();
    let axis = |delta| Event::Axis {
        control: AXIS,
        delta,
        quantum: q,
    };
    for (deltas, expected) in [
        (&[i64::MIN, i64::MAX][..], 299),
        (&[i64::MAX, 1, -i64::MAX][..], 301),
    ] {
        let mut rig = Rig::prepared(PlayOptions::default(), constant(150));
        rig.at(0).press(SELECT);
        rig.at(MS).ev(axis(0));
        rig.at(2 * MS);
        for &d in deltas {
            rig.ev(axis(d));
        }
        assert_eq!(g(&rig.snap()), expected, "{deltas:?}");
    }
}

#[test]
fn an_opposite_press_takes_back_a_repeat_at_its_time_in_either_order() {
    let o = options(SpeedMode::Classic, 200, 300, sudden(500));
    for tick_first in [true, false] {
        let mut rig = Rig::prepared(o, constant(150));
        rig.at(0).press(START).press(SCRATCH_POS);
        for t in (50..=450).step_by(50) {
            rig.at(t * MS).ev(Event::Tick);
        }
        // At 500 ms the held direction's repeat is accelerated (+10).
        rig.at(500 * MS);
        if tick_first {
            rig.ev(Event::Tick).press(SCRATCH_NEG);
        } else {
            rig.press(SCRATCH_NEG).ev(Event::Tick);
        }
        for t in (550..=800).step_by(50) {
            rig.at(t * MS).ev(Event::Tick);
        }
        rig.now += 1;
        rig.ev(Event::Tick);
        let log = rig.log.clone();
        let whole = drained(o, &constant(150), &log, &[]);
        assert_eq!(
            whole.options.covers().sudden.amount,
            510,
            "tick first: {tick_first}"
        );
        for i in 0..=log.len() {
            assert_eq!(
                drained(o, &constant(150), &log, &[i]),
                whole,
                "tick first: {tick_first}, split {i}"
            );
        }
    }
}

#[test]
fn a_step_applied_in_an_ended_context_is_never_taken_back() {
    // At one time: the scratch's +1 applies when `Ownership` ends its
    // context; the auxiliary +10 opens a new group, which the opposite
    // press must leave whole.
    let o = options(SpeedMode::Classic, 200, 300, sudden(500));
    let mut rig = Rig::prepared(o, constant(150));
    rig.at(0).press(START);
    rig.at(10 * MS)
        .press(SCRATCH_POS)
        .ev(Event::Ownership)
        .press(AUX_POS)
        .press(SCRATCH_NEG);
    rig.now += 1;
    rig.ev(Event::Tick);
    let log = rig.log.clone();
    let whole = drained(o, &constant(150), &log, &[]);
    assert_eq!(whole.options.covers().sudden.amount, 511);
    for i in 0..=log.len() {
        assert_eq!(drained(o, &constant(150), &log, &[i]), whole, "split {i}");
    }
}
