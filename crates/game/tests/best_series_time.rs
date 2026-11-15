//! How long a play takes to build its best's pace from the stored progress
//! (design.md "Personal Bests", skin projections) at the loader's
//! 500,000-object limit, the most judged targets a chart has: one pass of
//! saturating adds into one allocation, on the game thread before the
//! attempt is prepared. An indicator, not a gate; run in release:
//!
//! `cargo test --release -p game --test best_series_time -- --ignored --nocapture`

use std::time::Instant;

use rules::Judgment;

/// The loader's object cap, the bound on a progress's length.
const TARGETS: u32 = 500_000;

#[test]
#[ignore = "timing indicator; run in release"]
fn best_series_on_a_cap_size_progress() {
    let preset = rules::tapestry();
    // Every judgment in turn, so each weight is read.
    let judgments: Vec<Judgment> = (0..TARGETS)
        .map(|i| Judgment::ALL[usize::try_from(i % 5).unwrap()])
        .collect();
    let best = judgments
        .iter()
        .map(|&j| *preset.score.ex.get(j))
        .sum::<u32>();
    let mut runs = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let series = game::best_series(&judgments, &preset, best, TARGETS);
        runs.push(t.elapsed());
        let series = series.unwrap();
        assert_eq!(series.len(), judgments.len() + 1);
        assert_eq!(series.last(), Some(&best));
    }
    println!("best_series: {TARGETS} judgments, EX {best}");
    for (i, d) in runs.iter().enumerate() {
        println!("  run {i}  {:>8.2} ms", d.as_secs_f64() * 1_000.0);
    }
}
