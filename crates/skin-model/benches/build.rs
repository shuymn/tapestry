//! CPU benchmarks of a skin's build (design.md "Measurement").
//!
//! `skin_model_build_dense_frame` is `Skin::build` of a skin of just the
//! lanes, laid out as the simple skin's, on the render benchmark's dense
//! chart and frame (crates/render/benches/skin.rs), so the two compare.
//! The chart is built here; the skin and the frame helpers are the lanes
//! tests'.
//!
//! `skin_model_build_widgets` is `Skin::build` of a skin of widgets only:
//! four 11-cell numbers, a 50-segment flicker gauge and a judgment display
//! with every grade and 4-cell combos, all drawn. The skin helpers are the
//! widget tests'.

#[path = "../tests/lanes_support/mod.rs"]
mod lanes_support;
#[path = "../tests/play_support/mod.rs"]
mod play_support;
#[path = "../tests/widget_support/mod.rs"]
mod widget_support;

use std::hint::black_box;

use bms_model::{ChartBuilder, TimeUs};
use criterion::{Criterion, criterion_group, criterion_main};
use lanes_support::{lanes_skin, simple};
use play_support::{Harness, MS, full, lane, pct};
use rules::Judgment;
use skin_model::{Align, DigitsDef, GlyphSet, JudgeSlot, Motion, Pad, Place, Source, WidgetDefs};
use widget_support::{
    TEN, combo, digits, element, gauge, harness, judge_all, judge_element, judgment, load, number,
};

/// Notes on every lane every 6 ms for 30 s, with a long note on lane 3
/// every 20th step: a frame shows the whole visible window full.
fn bench_build(c: &mut Criterion) {
    let mut builder = ChartBuilder::new(150_000);
    for step in 0..5_000_i64 {
        let at = TimeUs(1_000 * MS + step * 6 * MS);
        for l in 0..8_u8 {
            builder = match (l, step % 20) {
                (3, 0) => builder.long(lane(l), at, TimeUs(at.0 + 100 * MS)),
                // Lane 3 is inside its long note.
                (3, 1..=16) => builder,
                _ => builder.tap(lane(l), at),
            };
        }
    }
    let chart = builder.build().expect("valid chart");
    let mut h = Harness::new(lanes_skin(simple()), &chart, &full(0));
    h.offset = TimeUs(8 * MS);
    let now = 10_000 * MS;
    h.frame_us(now, &[]);
    assert!(h.batch.quads().len() > 500, "a dense frame");
    assert_eq!(h.batch.dropped(), 0, "the batch has room");
    c.bench_function("skin_model_build_dense_frame", |b| {
        b.iter(|| {
            h.frame_us(black_box(now), &[]);
            black_box(h.batch.quads().len())
        });
    });
}

/// 4 · 11 cells, 50 segments and the flicker's tip, and a grade with a
/// 4-cell combo: 44 + 51 + 5 quads a frame.
fn bench_widgets(c: &mut Criterion) {
    let mut j = judge_all([800, 500, 200, 80]);
    let strip = DigitsDef {
        space: 0,
        ..digits(TEN, GlyphSet::Ten, 4, Align::Right, Pad::Zero)
    };
    for s in JudgeSlot::ALL {
        j.combos[s.index()] = Some(combo(strip, [210, 10, 20, 60]));
    }
    let values = ["combo", "ex_score", "combo * 7", "bp"];
    let mut elements: Vec<_> = (0..4_u32)
        .map(|i| {
            let y = i32::try_from(i).expect("4 numbers") * 40;
            element(Source::Number(i), [100, y, 20, 30], Place::SCREEN)
        })
        .collect();
    elements.push(element(Source::Gauge(0), [0, 200, 500, 20], Place::SCREEN));
    elements.push(judge_element(0));
    let skin = load(
        elements,
        WidgetDefs {
            numbers: values
                .iter()
                .map(|v| number(digits(TEN, GlyphSet::Ten, 11, Align::Right, Pad::Zero), v))
                .collect(),
            gauges: vec![gauge(
                50,
                Motion::Flicker {
                    period: TimeUs(1_000_000),
                },
            )],
            judges: vec![j],
        },
    )
    .expect("valid skin");
    let mut h = harness(skin);
    h.tally = play_support::tally(1_234);
    h.gauges[0].value = pct(50);
    h.frame(500, &[judgment(500, Judgment::Great, 1_234)]);
    assert_eq!(h.batch.quads().len(), 100, "every widget drawn");
    assert_eq!(h.batch.dropped(), 0, "the batch has room");
    c.bench_function("skin_model_build_widgets", |b| {
        b.iter(|| {
            h.frame(black_box(500), &[]);
            black_box(h.batch.quads().len())
        });
    });
}

criterion_group!(benches, bench_build, bench_widgets);
criterion_main!(benches);
