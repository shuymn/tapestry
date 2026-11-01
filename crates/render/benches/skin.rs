//! CPU benchmark of the simple skin (design.md "Measurement"): `SimpleSkin::build`
//! on a dense frame, without a GPU. The chart is built here and the frame
//! helpers are the WARP tests'.

#[path = "../tests/common/mod.rs"]
mod common;

use std::hint::black_box;

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, input, lane, view};
use criterion::{Criterion, criterion_group, criterion_main};
use render::{MAX_SPRITES, PlaySkin, SimpleSkin, SpriteBatch};

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
    let mut skin = SimpleSkin::new();
    skin.load(&chart, None);
    let mut batch = SpriteBatch::with_capacity(MAX_SPRITES);
    let v = view(42, 1_234);
    let now = TimeUs(10_000 * MS);
    let f = input(&v, now, TimeUs(8 * MS), &[]);
    batch.clear();
    skin.build(&f, &mut batch).expect("the simple skin builds");
    assert!(batch.len() > 500, "a dense frame");
    assert_eq!(batch.dropped(), 0, "the batch has room");
    c.bench_function("simple_skin_build_dense_frame", |b| {
        b.iter(|| {
            batch.clear();
            skin.build(black_box(&f), &mut batch)
                .expect("the simple skin builds");
            black_box(batch.len())
        });
    });
}

criterion_group!(benches, bench_build);
criterion_main!(benches);
