//! CPU benchmarks of the exact playhead (design.md "Measurement",
//! "Rendering and Skins").
//!
//! `skin_model_playhead_query` is one `ExactPlayhead::position_at` on a
//! timeline of 1 000 000 rows, each at its own µs (499 999 STOPs at
//! 1 000 BPM, then a tap, loaded as a session loads it): a frame's one
//! sample at the worst row count. `skin_model_playhead_prepare` is
//! `ExactPlayhead::prepare` of that chart, load-time work.

use std::hint::black_box;

use bms_model::{Chart, TimeUs};
use criterion::{Criterion, criterion_group, criterion_main};
use skin_model::ExactPlayhead;

/// The chart of the benchmarks, F17c's.
fn chart() -> Chart {
    let mut text = String::from("#BPM 1000\n#WAV01 a.wav\n#STOP01 1\n");
    let (stops, per) = (499_999_usize, 600);
    let measures = stops.div_ceil(per);
    for m in 1..=measures {
        let n = per.min(stops - (m - 1) * per);
        text.push_str(&format!("#{m:03}09:{}\n", "01".repeat(n)));
    }
    text.push_str(&format!("#{:03}11:01\n", measures + 1));
    let dir = tempfile::tempdir().expect("temporary folder");
    bms_parser::load_bms(
        text.as_bytes(),
        dir.path(),
        &bms_parser::LoadOptions::default(),
    )
    .expect("the chart loads")
    .chart
}

fn bench_playhead(c: &mut Criterion) {
    let chart = chart();
    assert_eq!(chart.timeline.points().len(), 1_000_000);
    let p = ExactPlayhead::prepare(&chart, None).expect("the chart prepares");
    let end = chart.timeline.points().last().map_or(0, |r| r.at.0);
    // Query times spread over the chart, 16 ms apart as frames are.
    let mut t = 0_i64;
    c.bench_function("skin_model_playhead_query", |b| {
        b.iter(|| {
            t = (t + 16_667) % end.max(1);
            black_box(p.position_at(black_box(TimeUs(t))))
        });
    });
    c.bench_function("skin_model_playhead_prepare", |b| {
        b.iter(|| ExactPlayhead::prepare(black_box(&chart), None).expect("prepares"));
    });
}

criterion_group!(benches, bench_playhead);
criterion_main!(benches);
