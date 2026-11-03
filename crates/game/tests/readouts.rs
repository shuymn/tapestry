//! The play readouts' exact projections (design.md "Play readouts";
//! docs/stage3.md S10-04b): the source hi-speed and durations, the eight
//! duration variants and the native block from one presentation snapshot
//! and the tempos, each witness worked out by hand.

use bms_model::Speed;
use game::adjustment::PresentationSnapshot;
use game::readouts::{Projection, READOUT_MAX, project};
use game::{Cover, Covers, HiSpeedSettings, PlayOptions, Ratio, References, SpeedMode, solve};

fn r(num: u64, den: u64) -> Ratio {
    Ratio::new(num, den).unwrap()
}

fn bpm(n: u64) -> Ratio {
    Ratio::integer(n)
}

fn on(amount: u16) -> Cover {
    Cover { amount, on: true }
}

fn off(amount: u16) -> Cover {
    Cover { amount, on: false }
}

fn covers(sudden: Cover, lift: Cover, hidden: Cover) -> Covers {
    Covers {
        sudden,
        lift,
        hidden,
    }
}

fn snapshot(mode: SpeedMode, green: u32, covers: Covers, h: Option<Ratio>) -> PresentationSnapshot {
    let hispeed = HiSpeedSettings {
        mode,
        green_target: green,
        ..HiSpeedSettings::default()
    };
    PresentationSnapshot {
        options: PlayOptions::new(hispeed, covers).unwrap(),
        hispeed: h,
        target: None,
        display_offset_us: 0,
        adjusting: false,
        generation: 0,
        sequence: 1,
        epoch: 0,
    }
}

fn floating(covers: Covers, h: Ratio) -> PresentationSnapshot {
    snapshot(SpeedMode::Floating, 300, covers, Some(h))
}

fn classic(covers: Covers, h: Ratio) -> PresentationSnapshot {
    snapshot(SpeedMode::Classic, 300, covers, Some(h))
}

/// The projection at one tempo, every reference at it, at 1×.
fn at(s: &PresentationSnapshot, b: Ratio) -> Projection {
    project(s, Some(b), Some(References::all(b)), Speed::NORMAL)
}

/// `(ms, green)` of a duration: each rounded half up from the exact ratio.
fn shown(t: Option<Ratio>) -> Option<(u64, u64)> {
    t.map(|t| {
        let (n, d) = (u128::from(t.num()), u128::from(t.den()));
        let half_up = |n: u128, d: u128| u64::try_from((2 * n + d) / (2 * d)).unwrap();
        (half_up(n, d), half_up(3 * n, 5 * d))
    })
}

#[test]
fn floating_150_projects_3_2_500_300() {
    let p = at(&floating(Covers::default(), r(16, 5)), bpm(150));
    assert_eq!(p.gain, Some(r(16, 5)));
    assert_eq!(p.bpm_current_x100, Some(15_000));
    assert_eq!(p.hispeed_x100, Some(320));
    assert_eq!(p.duration, Some(bpm(500)));
    assert_eq!(p.native_duration, Some(bpm(500)));
    assert_eq!(shown(p.native_duration), Some((500, 300)));
    assert_eq!(p.variants, [Some(bpm(500)); 8]);
    assert_eq!(p.target_green, Some(300));
    assert!(p.floating);
}

#[test]
fn lift_and_sudden_project_source_and_native() {
    // v = 0.7 · 0.8 = 0.56; Floating solves 144000 · 0.56 / (150 · 300).
    let c = covers(on(200), on(300), off(0));
    let h = solve(bpm(150), 300, &c).unwrap();
    assert_eq!(h, r(224, 125));
    let p = at(&floating(c, h), bpm(150));
    // Hs = 1.792 / 0.7 = 2.56.
    assert_eq!(p.hispeed_x100, Some(256));
    assert_eq!(p.native_duration, Some(bpm(500)));
    assert_eq!(shown(p.native_duration), Some((500, 300)));
    assert_eq!(p.duration, Some(bpm(500)));
    // Uncovered: 240000 · 0.7 / (150 · 1.792) = 625.
    assert_eq!(p.variants[1], Some(bpm(625)));
}

#[test]
fn lift_counts_in_the_native_green_number() {
    let p = at(
        &floating(covers(off(0), on(300), off(0)), r(16, 5)),
        bpm(150),
    );
    // 240000 · 0.7 / (150 · 3.2) = 350; green 210.
    assert_eq!(shown(p.native_duration), Some((350, 210)));
    // Hs = 3.2 / 0.7 = 4.571…
    assert_eq!(p.hispeed_x100, Some(457));
}

#[test]
fn a_target_step_gives_336_299() {
    let c = covers(on(300), off(0), off(0));
    let h = solve(bpm(300), 299, &c).unwrap();
    assert_eq!(h, r(336, 299));
    let p = at(&snapshot(SpeedMode::Floating, 299, c, Some(h)), bpm(300));
    // 240000 · 0.7 · 299 / (300 · 336) = 1495/3.
    assert_eq!(p.native_duration, Some(r(1495, 3)));
    assert_eq!(shown(p.native_duration), Some((498, 299)));
    assert_eq!(p.target_green, Some(299));
}

#[test]
fn hiding_and_showing_sudden_projects_both_modes() {
    // Hidden: 240000 / (300 · 2.56) = 312.5 ms, green 187.5 → 188.
    let hidden = at(
        &floating(covers(off(200), off(0), off(0)), r(64, 25)),
        bpm(300),
    );
    assert_eq!(hidden.native_duration, Some(r(625, 2)));
    assert_eq!(shown(hidden.native_duration), Some((313, 188)));
    // The covered variant counts the configured 200 while it is off: 250.
    assert_eq!(hidden.variants[0], Some(bpm(250)));
    assert_eq!(hidden.variants[1], Some(r(625, 2)));
    // Shown in Floating: H 1.28, 500 ms, green 300.
    let c = covers(on(200), off(0), off(0));
    assert_eq!(solve(bpm(300), 300, &c).unwrap(), r(32, 25));
    let f = at(&floating(c, r(32, 25)), bpm(300));
    assert_eq!(shown(f.native_duration), Some((500, 300)));
    // Shown in Classic: H stays 2.56, 250 ms, green 150.
    let k = at(&classic(c, r(64, 25)), bpm(300));
    assert_eq!(shown(k.native_duration), Some((250, 150)));
    assert_eq!(k.target_green, None);
    assert!(!k.floating);
}

#[test]
fn half_ms_rounds_each_order_its_own_way() {
    // Classic 3.20 at 100 BPM, SUDDEN+ 330: 240000 · 0.67 / 320 = 502.5.
    let p = at(
        &classic(covers(on(330), off(0), off(0)), r(16, 5)),
        bpm(100),
    );
    assert_eq!(p.native_duration, Some(r(1005, 2)));
    assert_eq!(shown(p.native_duration), Some((503, 302)));
    // The source ratio is the same; 313's floor of the rounded ms is the
    // skin model's: ⌊3 · 503 / 5⌋ = 301.
    assert_eq!(p.duration, Some(r(1005, 2)));
    assert_eq!(shown(p.duration).map(|(ms, _)| 3 * ms / 5), Some(301));
    assert_eq!(shown(p.variants[0]), Some((503, 302)));
    assert_eq!(shown(p.variants[1]), Some((750, 450)));
}

#[test]
fn the_clamps_keep_the_target_distinct() {
    let none = Covers::default();
    // 10 BPM: a requested 48 clamps to 20.
    assert_eq!(solve(bpm(10), 300, &none).unwrap(), bpm(20));
    let slow = at(&floating(none, bpm(20)), bpm(10));
    assert_eq!(shown(slow.native_duration), Some((1200, 720)));
    assert_eq!(slow.hispeed_x100, Some(2000));
    assert_eq!(slow.target_green, Some(300));
    // 100000 BPM: a requested 0.0048 clamps to 0.01.
    assert_eq!(solve(bpm(100_000), 300, &none).unwrap(), r(1, 100));
    let fast = at(&floating(none, r(1, 100)), bpm(100_000));
    assert_eq!(shown(fast.native_duration), Some((240, 144)));
    assert_eq!(fast.bpm_current_x100, Some(10_000_000));
    assert_eq!(fast.hispeed_x100, Some(1));
    assert_eq!(fast.target_green, Some(300));
}

#[test]
fn full_sudden_projects_zero_and_reopens() {
    let full = covers(on(1000), off(0), off(0));
    assert_eq!(solve(bpm(150), 300, &full).unwrap(), r(1, 100));
    let p = at(&floating(full, r(1, 100)), bpm(150));
    assert_eq!(p.gain, Some(r(1, 100)));
    assert_eq!(p.hispeed_x100, Some(1));
    assert_eq!(shown(p.native_duration), Some((0, 0)));
    assert_eq!(shown(p.duration), Some((0, 0)));
    assert_eq!(shown(p.variants[0]), Some((0, 0)));
    // Uncovered: 240000 / (150 · 0.01) = 160000, green 96000.
    assert_eq!(shown(p.variants[1]), Some((160_000, 96_000)));
    // Reopened to 900: v 0.1, H 14400/45000 = 0.32.
    let open = covers(on(900), off(0), off(0));
    assert_eq!(solve(bpm(150), 300, &open).unwrap(), r(8, 25));
    let q = at(&floating(open, r(8, 25)), bpm(150));
    assert_eq!(shown(q.native_duration), Some((500, 300)));
    assert_eq!(shown(q.variants[1]), Some((5000, 3000)));
}

#[test]
fn no_field_leaves_source_hs_unavailable() {
    // LIFT 1000: v = 0, Floating's solve clamps to 0.01.
    let c = covers(on(200), on(1000), on(300));
    assert_eq!(solve(bpm(150), 300, &c).unwrap(), r(1, 100));
    let p = at(&floating(c, r(1, 100)), bpm(150));
    assert_eq!(p.gain, Some(r(1, 100)));
    assert_eq!(shown(p.native_duration), Some((0, 0)));
    assert_eq!(p.target_green, Some(300));
    assert_eq!(p.hispeed_x100, None);
    assert_eq!(p.duration, None);
    assert_eq!(p.variants, [None; 8]);
    assert_eq!(p.bpm_current_x100, Some(15_000));
}

#[test]
fn extreme_slow_tempo_leaves_durations_unavailable() {
    // 0.001 BPM, H 0.01: T = 240000 / (0.001 · 0.01) = 2.4·10¹⁰ ms.
    let p = at(&floating(Covers::default(), r(1, 100)), r(1, 1000));
    assert!(24_000_000_000 > i64::from(READOUT_MAX));
    assert_eq!(p.native_duration, None);
    assert_eq!(p.duration, None);
    assert_eq!(p.variants, [None; 8]);
    assert_eq!(p.gain, Some(r(1, 100)));
    assert_eq!(p.hispeed_x100, Some(1));
    assert_eq!(p.bpm_current_x100, Some(0));
}

#[test]
fn all_sixteen_variant_values_are_distinct() {
    // Classic H 2, SUDDEN+ 250 configured: ms = 120000 · f / B.
    let s = classic(covers(off(250), off(0), off(0)), bpm(2));
    let refs = References {
        start: bpm(150),
        main: bpm(125),
        minimum: bpm(96),
        maximum: bpm(240),
    };
    let p = project(&s, Some(bpm(150)), Some(refs), Speed::NORMAL);
    let expected = [
        (600, 360),
        (800, 480),
        (720, 432),
        (960, 576),
        (938, 563),
        (1250, 750),
        (375, 225),
        (500, 300),
    ];
    for (i, (v, e)) in p.variants.into_iter().zip(expected).enumerate() {
        assert_eq!(shown(v), Some(e), "variant {i}");
    }
    let mut all: Vec<u64> = expected.iter().flat_map(|&(a, b)| [a, b]).collect();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 16);
}

#[test]
fn a_variant_past_the_maximum_is_unavailable_alone() {
    // Classic H 0.10, SUDDEN+ 250, MIN 0.001 BPM.
    let s = classic(covers(on(250), off(0), off(0)), r(1, 10));
    let refs = References {
        minimum: r(1, 1000),
        ..References::all(bpm(150))
    };
    let p = project(&s, Some(bpm(150)), Some(refs), Speed::NORMAL);
    // 240000 · 0.75 / (0.001 · 0.1) = 1.8·10⁹; uncovered 2.4·10⁹.
    assert_eq!(shown(p.variants[4]), Some((1_800_000_000, 1_080_000_000)));
    assert_eq!(p.variants[5], None);
    assert!(
        p.variants[..4]
            .iter()
            .chain(&p.variants[6..])
            .all(Option::is_some)
    );
}

#[test]
fn unavailable_references_leave_only_the_current_variants() {
    let s = classic(covers(off(250), off(0), off(0)), bpm(2));
    let p = project(&s, Some(bpm(150)), None, Speed::NORMAL);
    assert_eq!(shown(p.variants[0]), Some((600, 360)));
    assert_eq!(shown(p.variants[1]), Some((800, 480)));
    assert_eq!(p.variants[2..], [None; 6]);
    // An unknown current tempo leaves every duration unavailable.
    let q = project(&s, None, None, Speed::NORMAL);
    assert_eq!(q.variants, [None; 8]);
    assert_eq!(
        (q.duration, q.native_duration, q.bpm_current_x100),
        (None, None, None)
    );
    assert_eq!(q.hispeed_x100, Some(200));
}

#[test]
fn an_unknown_hispeed_leaves_its_dependants_unavailable() {
    let s = snapshot(SpeedMode::Floating, 300, Covers::default(), None);
    let p = at(&s, bpm(150));
    assert_eq!(p.gain, None);
    assert_eq!(p.hispeed_x100, None);
    assert_eq!((p.duration, p.native_duration), (None, None));
    assert_eq!(p.variants, [None; 8]);
    assert_eq!(p.bpm_current_x100, Some(15_000));
    assert_eq!(p.target_green, Some(300));
}

#[test]
fn the_practice_speed_applies_once() {
    // 150 % from START 150 plays at 225: H 32/15 gives 500 ms.
    let s = floating(Covers::default(), r(32, 15));
    let refs = References {
        main: bpm(100),
        ..References::all(bpm(150))
    };
    let p = project(&s, Some(bpm(150)), Some(refs), Speed::FASTEST);
    assert_eq!(p.bpm_current_x100, Some(22_500));
    assert_eq!(p.native_duration, Some(bpm(500)));
    // MAIN 100 plays at 150: 240000 / (150 · 32/15) = 750.
    assert_eq!(p.variants[2], Some(bpm(750)));
}
