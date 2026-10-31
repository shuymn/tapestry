//! CPU benchmark of a skin's build (design.md "Measurement"): `Skin::build`
//! of a skin of just the lanes, laid out as the simple skin's, on the
//! render benchmark's dense chart and frame (crates/render/benches/skin.rs),
//! so the two compare. The chart is built here; the skin and the frame
//! helpers are the lanes tests'.

#[path = "../tests/lanes_support/mod.rs"]
mod lanes_support;
#[path = "../tests/play_support/mod.rs"]
mod play_support;

use std::hint::black_box;

use bms_model::{ChartBuilder, TimeUs};
use criterion::{Criterion, criterion_group, criterion_main};
use lanes_support::{lanes_skin, simple};
use play_support::{Harness, MS, full, lane};

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

criterion_group!(benches, bench_build);
criterion_main!(benches);
