//! The `gauge` widget: the lit count, the border, the looks from the
//! presets, the motions and the flicker's tip, and its load errors.
//!
//! Unless stated: a gauge at x = 0, y = 100, 500 wide and 20 tall in 50
//! segments (10 px each), the shown gauge's maximum full.

mod lanes_support;
mod play_support;
mod widget_support;

use bms_model::TimeUs;
use play_support::{Harness, MS, pct};
use rules::{GaugeKind, GaugeValue};
use skin_model::{
    Attempt, GaugeDef, GaugeDesc, GaugeError, GaugeLook, GaugePart, LoadError, MAX_KEY_US,
    MAX_SEGMENTS, MAX_WIDGET_QUADS, Motion, Place, PlayContext, QuadBatch, Side, Skin, Source,
    WidgetDefs,
};
use widget_support::{GAUGE, chart, desc, drawn, element, gauge, load};

const RECT: [i32; 4] = [0, 100, 500, 20];

fn skin_of(g: GaugeDef) -> Result<Skin, LoadError> {
    load(
        vec![element(Source::Gauge(0), RECT, Place::SCREEN)],
        WidgetDefs {
            gauges: vec![g],
            ..WidgetDefs::default()
        },
    )
}

/// A harness over `g` with the descriptors `descs` (normal at 80 %, hard
/// at 0 % by default).
fn harness_with(g: GaugeDef, descs: &[GaugeDesc]) -> Harness {
    let descs: &'static [GaugeDesc] = Box::leak(descs.to_vec().into_boxed_slice());
    let ctx = PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: descs,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
    };
    Harness::new(skin_of(g).unwrap(), &chart(), &ctx)
}

fn harness(g: GaugeDef) -> Harness {
    harness_with(g, &[desc(GaugeLook::Normal, 80), desc(GaugeLook::Hard, 0)])
}

/// A segment as drawn: its part (0 front, 1 back, 2 tip), whether it is
/// below the border, the look set (0 or 1) and its quad.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Seg {
    part: u32,
    below: bool,
    set: u32,
    rect: [f32; 4],
    alpha: f32,
}

fn segs(h: &Harness) -> Vec<Seg> {
    drawn(h)
        .into_iter()
        .map(|(i, _, q)| {
            let k = i - GAUGE;
            Seg {
                part: (k % 6) / 2,
                below: k % 2 == 1,
                set: k / 6,
                rect: q.rect,
                alpha: q.color[3],
            }
        })
        .collect()
}

/// The parts of every segment, as a string: `f` front, `b` back, `t` tip.
fn parts(h: &Harness) -> String {
    segs(h)
        .iter()
        .map(|s| ['f', 'b', 't'][s.part as usize])
        .collect()
}

/// The lit segments: fronts and tips.
fn lit(h: &Harness) -> usize {
    segs(h).iter().filter(|s| s.part != 1).count()
}

fn still() -> GaugeDef {
    gauge(50, Motion::Still)
}

#[test]
fn any_value_above_0_lights_a_segment() {
    let mut h = harness(still());
    let mut at = |v: GaugeValue| {
        h.gauges[0].value = v;
        h.frame(0, &[]);
        assert_eq!(h.batch.quads().len(), 50);
        (lit(&h), parts(&h))
    };
    assert_eq!(at(GaugeValue(0)).0, 0);
    // One unit lights one: the `max(1, ·)`.
    assert_eq!(at(GaugeValue(1)), (1, format!("t{}", "b".repeat(49))));
    // Each segment is 2 %: just below 4 % lights 1, 4 % lights 2.
    assert_eq!(at(GaugeValue(pct(4).0 - 1)).0, 1);
    assert_eq!(at(pct(4)), (2, format!("ft{}", "b".repeat(48))));
    assert_eq!(at(GaugeValue::FULL), (50, format!("{}t", "f".repeat(49))));
    // A failed survival gauge reads 0.
    h.gauge = 1;
    h.gauges[1].value = GaugeValue(0);
    h.gauges[1].failed_at = Some(TimeUs(0));
    h.frame(0, &[]);
    assert_eq!(lit(&h), 0);
}

#[test]
fn segments_below_the_border_use_the_below_images() {
    let mut h = harness(still());
    h.gauges[0].value = GaugeValue::FULL;
    h.frame(0, &[]);
    let s = segs(&h);
    // Border 80 % of 50: segment i is below when i·100 < 80·50.
    assert!(s[38].below, "segment 39");
    assert!(!s[39].below, "segment 40");
    assert_eq!(s.iter().filter(|s| s.below).count(), 39);
    // Segment i is [10·(i − 1), 100, 10, 20].
    assert_eq!(s[38].rect, [380.0, 100.0, 10.0, 20.0]);
}

#[test]
fn looks_and_borders_come_from_the_preset_gauges() {
    let check = |preset: rules::RulePreset, fails: GaugeValue| {
        let got: Vec<_> = preset
            .gauges
            .iter()
            .map(|g| {
                let d = GaugeDesc::from_rule(g);
                (g.id.clone(), d.look, d.border, d.start, d.max)
            })
            .collect();
        let want: Vec<_> = [
            ("normal", GaugeLook::Normal, pct(80)),
            ("assist_easy", GaugeLook::AssistEasy, pct(60)),
            ("easy", GaugeLook::Easy, pct(80)),
            ("hard", GaugeLook::Hard, fails),
            ("ex_hard", GaugeLook::ExHard, fails),
            ("hazard", GaugeLook::Hazard, fails),
        ]
        .into_iter()
        .zip(&preset.gauges)
        .map(|((id, look, border), g)| (id.to_owned(), look, border, g.start, g.max))
        .collect();
        assert_eq!(got, want);
    };
    check(rules::beatoraja_style(), GaugeValue::EMPTY);
    check(rules::lr2_style(), pct(2));
    // Another id: by its kind.
    let preset = rules::beatoraja_style();
    let mut end = preset.gauges[0].clone();
    end.id = "groove".to_owned();
    assert_eq!(GaugeDesc::from_rule(&end).look, GaugeLook::Normal);
    let mut survival = preset.gauges[3].clone();
    survival.id = "grade".to_owned();
    assert!(matches!(survival.kind, GaugeKind::Survival { .. }));
    assert_eq!(GaugeDesc::from_rule(&survival).look, GaugeLook::Hard);
}

#[test]
fn the_shown_gauge_picks_its_look_each_frame() {
    let mut h = harness(still());
    h.gauges[1].value = pct(50);
    h.frame(0, &[]);
    assert!(segs(&h).iter().all(|s| s.set == 0));
    h.gauge = 1;
    h.frame(16, &[]);
    assert!(segs(&h).iter().all(|s| s.set == 1));
    // Hard's border is 0: nothing is below it.
    assert!(segs(&h).iter().all(|s| !s.below));
    h.gauge = 0;
    h.frame(32, &[]);
    assert!(segs(&h).iter().all(|s| s.set == 0));
    // A shown index past the snapshot, or past the descriptors, draws
    // nothing.
    h.gauge = 2;
    h.frame(48, &[]);
    assert!(h.batch.quads().is_empty());
    let mut one = harness_with(still(), &[desc(GaugeLook::Normal, 80)]);
    one.gauge = 1;
    one.frame(0, &[]);
    assert!(one.batch.quads().is_empty());
    one.gauge = 0;
    one.frame(0, &[]);
    assert_eq!(one.batch.quads().len(), 50);
}

/// The offset `a` a still-shaped motion shows at `t_us` with 10 lit: the
/// fronts below the tip are `9 − a`.
fn offset(h: &mut Harness, t_us: i64) -> usize {
    h.frame_us(t_us, &[]);
    let p = parts(h);
    assert_eq!(&p[9..10], "t", "{p}");
    9 - p[..9].matches('f').count()
}

fn motion(m: Motion) -> Harness {
    let mut h = harness(gauge(50, m));
    // 20 % lights 10.
    h.gauges[0].value = pct(20);
    h
}

const STEP: TimeUs = TimeUs(100 * MS);

#[test]
fn rise_and_fall_step_the_gap() {
    let mut h = motion(Motion::Rise {
        range: 3,
        step: STEP,
    });
    let a: Vec<_> = (0..5).map(|k| offset(&mut h, k * 100 * MS)).collect();
    assert_eq!(a, [0, 3, 2, 1, 0]);
    // The step boundary.
    assert_eq!(offset(&mut h, 99_999), 0);
    assert_eq!(offset(&mut h, 100_000), 3);
    assert_eq!(parts(&h)[..10], *"ffffffbbbt");
    let mut h = motion(Motion::Fall {
        range: 3,
        step: STEP,
    });
    let a: Vec<_> = (0..5).map(|k| offset(&mut h, k * 100 * MS)).collect();
    assert_eq!(a, [0, 1, 2, 3, 0]);
}

#[test]
fn random_is_splitmix_of_the_step() {
    let mut h = motion(Motion::Random {
        range: 3,
        step: STEP,
    });
    let first: Vec<_> = (0..8).map(|k| offset(&mut h, k * 100 * MS)).collect();
    assert_eq!(first, [3, 1, 2, 1, 2, 2, 0, 3]);
    let mut again = motion(Motion::Random {
        range: 3,
        step: STEP,
    });
    let mut seen = [0; 4];
    for k in 0..256 {
        let t = k * 100 * MS + 50 * MS;
        let a = offset(&mut h, t);
        assert_eq!(offset(&mut again, t), a, "equal at equal t");
        seen[a] += 1;
    }
    assert!(seen.iter().all(|&n| n > 0), "{seen:?}");
}

#[test]
fn still_shows_the_tip_at_the_lit_end_only() {
    let mut h = motion(Motion::Still);
    for t in [0, 99_999, 100_000, 7_000_000] {
        assert_eq!(offset(&mut h, t), 0);
    }
    assert_eq!(parts(&h), format!("{}t{}", "f".repeat(9), "b".repeat(40)));
}

#[test]
fn flicker_overlays_the_tip_with_a_triangle_alpha() {
    let period = TimeUs(1_000 * MS);
    let mut h = motion(Motion::Flicker { period });
    assert_eq!(h.skin.quad_capacity(), 51);
    for (ms, alpha) in [(0, 0.0), (250, 0.5), (500, 1.0), (750, 0.5), (1_000, 0.0)] {
        h.frame(ms, &[]);
        let s = segs(&h);
        assert_eq!(s.len(), 51);
        assert!(h.batch.is_full());
        assert_eq!(h.batch.dropped(), 0);
        // Segment 10 is front, and the tip lies over it.
        assert_eq!(s[9].part, 0);
        assert_eq!((s[10].part, s[10].rect), (2, s[9].rect));
        assert_eq!(s[10].alpha, alpha, "{ms} ms");
        assert_eq!(s[11..].iter().filter(|s| s.part != 1).count(), 0);
    }
    h.gauges[0].value = GaugeValue(0);
    h.frame(500, &[]);
    assert_eq!(segs(&h).len(), 50);
    assert!(segs(&h).iter().all(|s| s.part == 1));
}

#[test]
fn the_most_segments_draw_with_the_flicker_tip() {
    // One segment fewer than the most leaves the tip its quad: the whole
    // widget budget, the tip pushed at alpha 0 at the period's start, right
    // after the last lit segment.
    let p = MAX_SEGMENTS - 1;
    let flicker = gauge(p, Motion::Flicker { period: TimeUs(MS) });
    let mut h = harness(flicker);
    assert_eq!(h.skin.quad_capacity(), MAX_WIDGET_QUADS);
    h.gauges[0].value = pct(50);
    h.frame(0, &[]);
    assert_eq!(h.batch.quads().len(), 16_384);
    assert!(h.batch.is_full());
    assert_eq!(h.batch.dropped(), 0);
    let s = segs(&h);
    // 50 % of 16 383 lights 8 191.
    assert_eq!((s[8_190].part, s[8_191].part), (0, 2));
    assert_eq!(s[8_191].rect, s[8_190].rect);
    assert_eq!(s[8_191].alpha, 0.0);
    assert_eq!(s[8_192].part, 1);
    // At 0 every segment is back and the tip is not drawn; the capacity
    // keeps its quad.
    h.gauges[0].value = GaugeValue(0);
    h.frame(16, &[]);
    assert_eq!(segs(&h).len(), usize::from(p));
    assert!(segs(&h).iter().all(|s| s.part == 1));
    assert_eq!(h.skin.quad_capacity(), MAX_WIDGET_QUADS);
}

#[test]
fn the_most_segments_fit_without_a_flicker() {
    // The parameter allows 16 384 segments under every motion; the widget
    // budget refuses the flicker's tip one past it.
    assert_eq!(u32::from(MAX_SEGMENTS), MAX_WIDGET_QUADS);
    let mut h = harness(gauge(MAX_SEGMENTS, Motion::Still));
    h.gauges[0].value = GaugeValue::FULL;
    h.frame(0, &[]);
    assert_eq!(h.batch.quads().len(), 16_384);
    assert_eq!(h.batch.dropped(), 0);
    let fall = Motion::Fall {
        range: MAX_SEGMENTS,
        step: TimeUs(1),
    };
    assert!(skin_of(gauge(MAX_SEGMENTS, fall)).is_ok());
    assert!(matches!(
        skin_of(gauge(MAX_SEGMENTS, Motion::Flicker { period: TimeUs(MS) })),
        Err(LoadError::TooManyWidgetQuads { n: 16_385 })
    ));
}

/// A descriptor of the raw maximum `max` and border `border`.
fn raw(max: i64, border: i64) -> GaugeDesc {
    GaugeDesc {
        start: GaugeValue(0),
        max: GaugeValue(max),
        look: GaugeLook::Normal,
        border: GaugeValue(border),
    }
}

#[test]
fn many_segments_read_the_raw_value() {
    // 16 384 segments of a maximum of 10 000 000: 5 000 000 and 5 000 611
    // share 500 ‰ but light 8 192 and 8 193; 5 000 610 still 8 192.
    let mut h = harness_with(gauge(MAX_SEGMENTS, Motion::Still), &[raw(10_000_000, 0)]);
    for (value, n) in [
        (5_000_000, 8_192),
        (5_000_610, 8_192),
        (5_000_611, 8_193),
        (1, 1),
        (10_000_000, 16_384),
        (i64::MAX, 16_384),
        (0, 0),
        (i64::MIN, 0),
    ] {
        h.gauges[0].value = GaugeValue(value);
        h.frame(0, &[]);
        assert_eq!(lit(&h), n, "{value}");
    }
    // The extremes of the raw maximum: one below it lights all but one.
    let mut h = harness_with(gauge(MAX_SEGMENTS, Motion::Still), &[raw(i64::MAX, 0)]);
    h.gauges[0].value = GaugeValue(i64::MAX - 1);
    h.frame(0, &[]);
    assert_eq!(lit(&h), 16_383);
}

#[test]
fn many_segments_meet_the_border_exactly() {
    // A maximum of 16 384 and a border of 8 192: segment 8 192 is exactly
    // on it, so at or above; 8 191 below. A maximum of 3 (not dividing
    // 16 384) and a border of 1: `i·3 < 16 384` below up to 5 461.
    let below = |max: i64, border: i64| {
        let mut h = harness_with(gauge(MAX_SEGMENTS, Motion::Still), &[raw(max, border)]);
        h.gauges[0].value = GaugeValue(max);
        h.frame(0, &[]);
        segs(&h).iter().filter(|s| s.below).count()
    };
    assert_eq!(below(16_384, 8_192), 8_191);
    assert_eq!(below(3, 1), 5_461);
}

#[test]
fn a_full_range_motion_wraps_over_every_segment() {
    // Fall over every segment, a step of 1 µs, all lit: the fronts below
    // the tip are `16 383 − a`, `a = k mod 16 385`.
    let fall = Motion::Fall {
        range: MAX_SEGMENTS,
        step: TimeUs(1),
    };
    let mut h = harness(gauge(MAX_SEGMENTS, fall));
    h.gauges[0].value = GaugeValue::FULL;
    let mut fronts = |t: i64| {
        h.frame_us(t, &[]);
        segs(&h).iter().filter(|s| s.part == 0).count()
    };
    let start = 1_000;
    assert_eq!(fronts(start), 15_383);
    assert_eq!(fronts(start), 15_383, "a repeated time");
    for (k, want) in [(16_383, 0), (16_384, 0), (16_385, 16_383), (16_386, 16_382)] {
        assert_eq!(fronts(k), want, "{k}");
    }
}

#[test]
fn narrow_and_degenerate_gauges_keep_their_cells() {
    // 500 px in 16 384 cells: each 500 / 16 384 wide, the last from
    // 500 · 16 383 / 16 384; a zero width gives finite empty cells.
    let mut h = harness(gauge(MAX_SEGMENTS, Motion::Still));
    h.gauges[0].value = GaugeValue::FULL;
    h.frame(0, &[]);
    let s = segs(&h);
    let w = 500.0 / 16_384.0;
    assert_eq!(s[0].rect, [0.0, 100.0, w as f32, 20.0]);
    let last = s[16_383].rect;
    assert!((f64::from(last[0]) - 500.0 * 16_383.0 / 16_384.0).abs() < 1e-3);
    assert!((f64::from(last[2]) - w).abs() < 1e-6);
    for r in [[0, 100, 0, 20], [500, 100, -500, 20]] {
        let skin = load(
            vec![element(Source::Gauge(0), r, Place::SCREEN)],
            WidgetDefs {
                gauges: vec![gauge(MAX_SEGMENTS, Motion::Still)],
                ..WidgetDefs::default()
            },
        )
        .unwrap();
        let ctx = PlayContext {
            meta: skin_model::ChartMetadata::UNKNOWN,
            autoplay: false,
            gauges: &[desc(GaugeLook::Normal, 80)],
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: false,
            empty_poor_counted: false,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(0),
            bpm_range: None,
            bpm_whole_change: None,
            play_level: None,
        };
        let mut h = Harness::new(skin, &chart(), &ctx);
        h.gauges.truncate(1);
        h.gauges[0].value = GaugeValue::FULL;
        h.frame(0, &[]);
        let q = h.batch.quads();
        assert_eq!(q.len(), 16_384, "{r:?}");
        assert!(q.iter().all(|q| q.rect.iter().all(|v| v.is_finite())));
    }
}

#[test]
fn a_short_batch_keeps_the_first_segments_in_order() {
    // A batch of 100 quads keeps segments 1 to 100 as a full one draws
    // them, and drops the rest.
    let mut h = harness(gauge(MAX_SEGMENTS, Motion::Still));
    h.gauges[0].value = GaugeValue::FULL;
    h.frame(0, &[]);
    let full: Vec<[f32; 4]> = h.batch.quads().iter().map(|q| q.rect).collect();
    h.batch = QuadBatch::with_capacity(100, 8);
    h.frame(16, &[]);
    let short: Vec<[f32; 4]> = h.batch.quads().iter().map(|q| q.rect).collect();
    assert_eq!(short, full[..100]);
    // The first refused push stops the gauge.
    assert_eq!(h.batch.dropped(), 1);
}

fn refused(g: GaugeDef) -> Option<GaugeError> {
    match skin_of(g) {
        Ok(_) => None,
        Err(LoadError::Gauge { element: 0, error }) => Some(error),
        Err(e) => panic!("{e:?}"),
    }
}

#[test]
fn gauges_are_checked_at_their_limits() {
    assert_eq!(refused(gauge(MAX_SEGMENTS, Motion::Still)), None);
    assert_eq!(
        refused(gauge(MAX_SEGMENTS + 1, Motion::Still)),
        Some(GaugeError::Segments)
    );
    assert_eq!(refused(gauge(0, Motion::Still)), Some(GaugeError::Segments));
    let at = |us| TimeUs(us);
    for (step, ok) in [
        (1, true),
        (MAX_KEY_US, true),
        (0, false),
        (MAX_KEY_US + 1, false),
    ] {
        let rise = Motion::Rise {
            range: 1,
            step: at(step),
        };
        let want = (!ok).then_some(GaugeError::Step);
        assert_eq!(refused(gauge(50, rise)), want, "step {step}");
        let flicker = Motion::Flicker { period: at(step) };
        let want = (!ok).then_some(GaugeError::Period);
        assert_eq!(refused(gauge(50, flicker)), want, "period {step}");
    }
    let random = |range| Motion::Random { range, step: STEP };
    assert_eq!(refused(gauge(50, random(50))), None);
    assert_eq!(refused(gauge(50, random(51))), Some(GaugeError::Range));
    let mut missing = gauge(50, Motion::Still);
    missing.looks[GaugeLook::ExHard.index()].tip[1] = 999;
    assert_eq!(
        refused(missing),
        Some(GaugeError::UnknownImage {
            look: GaugeLook::ExHard,
            part: GaugePart::Tip,
            below: true,
        })
    );
}

/// The family conditions an importer writes for the shown gauge's kind.
const GROOVE: &str =
    "gauge.valid && (gauge.kind == assist_easy || gauge.kind == easy || gauge.kind == normal)";
const HARD: &str =
    "gauge.valid && (gauge.kind == hard || gauge.kind == ex_hard || gauge.kind == hazard)";
const EX: &str = "gauge.valid && (gauge.kind == assist_easy || gauge.kind == easy \
                  || gauge.kind == ex_hard || gauge.kind == hazard)";

/// `gauge.kind`, `gauge.valid` and the three family conditions with their
/// negations, as of the last build.
fn kind(h: &Harness) -> (i32, i32, [bool; 6]) {
    use skin_model::{PLAY, Scope, Ty, compile, field};
    let state = h.play.state();
    let cond = |src: &str| {
        compile(src, &Scope::new(&PLAY), Ty::Bool)
            .unwrap()
            .eval_bool(&state)
    };
    let not = |src: &str| cond(&format!("!({src})"));
    (
        h.field(field::GAUGE_KIND),
        h.field(field::GAUGE_VALID),
        [
            cond(GROOVE),
            cond(HARD),
            cond(EX),
            not(GROOVE),
            not(HARD),
            not(EX),
        ],
    )
}

#[test]
fn the_gauge_kind_follows_the_shown_gauge() {
    let descs: Vec<_> = GaugeLook::ALL.iter().map(|&l| desc(l, 80)).collect();
    let mut h = harness_with(still(), &descs);
    h.gauges = vec![play_support::state(pct(50)); GaugeLook::COUNT];
    // (groove, hard, ex) per look, in look order.
    let families = [
        [true, false, true],
        [true, false, true],
        [true, false, false],
        [false, true, false],
        [false, true, true],
        [false, true, true],
    ];
    // Every look, switched with no gauge effect: the enum, the conditions
    // and the widget's look set agree in the same build.
    for (i, (look, want)) in GaugeLook::ALL.iter().zip(families).enumerate() {
        h.gauge = i;
        h.frame(i64::try_from(i).unwrap() * 16, &[]);
        let (k, valid, conds) = kind(&h);
        assert_eq!(usize::try_from(k).unwrap(), look.index(), "{look:?}");
        assert_eq!(valid, 1);
        assert_eq!(conds[..3], want, "{look:?}");
        assert_eq!(conds[3..], want.map(|c| !c), "{look:?}");
        let set = u32::from(*look == GaugeLook::Hard);
        assert!(segs(&h).iter().all(|s| s.set == set), "{look:?}");
    }
    // A shown index past the snapshot: invalid, `normal`, every family
    // false and every negation true; the widget draws nothing.
    h.gauges.truncate(2);
    h.gauge = 3;
    h.frame(200, &[]);
    assert_eq!(kind(&h), (2, 0, [false, false, false, true, true, true]));
    assert!(h.batch.quads().is_empty());
    // Past the descriptors, with a snapshot.
    let mut one = harness_with(still(), &[desc(GaugeLook::Hard, 0)]);
    one.gauge = 1;
    one.frame(0, &[]);
    assert_eq!(kind(&one), (2, 0, [false, false, false, true, true, true]));
    one.gauge = 0;
    one.frame(16, &[]);
    assert_eq!(kind(&one), (3, 1, [false, true, false, true, false, true]));
}

#[test]
fn custom_gauges_show_their_fallback_kind() {
    let preset = rules::beatoraja_style();
    let mut end = preset.gauges[0].clone();
    end.id = "groove".to_owned();
    let mut survival = preset.gauges[3].clone();
    survival.id = "grade".to_owned();
    let descs = [GaugeDesc::from_rule(&end), GaugeDesc::from_rule(&survival)];
    let mut h = harness_with(still(), &descs);
    h.frame(0, &[]);
    assert_eq!(kind(&h), (2, 1, [true, false, false, false, true, true]));
    h.gauge = 1;
    h.frame(16, &[]);
    assert_eq!(kind(&h), (3, 1, [false, true, false, true, false, true]));
}
