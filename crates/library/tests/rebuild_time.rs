//! How long each step of the bests phase takes on its worst inputs
//! (design.md "Personal Bests"): a chart near the loader's 500,000-object
//! limit with a record of few inputs, and a record as dense as admission
//! allows. A pause is acknowledged once the step in hand finishes, so these
//! bound a countdown's wait for the rebuild. Indicators, not gates; run in
//! release:
//!
//! `cargo test --release -p library --test rebuild_time -- --ignored --nocapture`

mod common;

use std::fmt::Write as _;
use std::fs;
use std::time::{Duration, Instant};

use common::{Fixture, MS, key, record_of};
use library::{Contribution, PlayId, REBUILD_MEMORY, Verified};
use replay::{DecodeLimits, PlayRecord};

/// 999 measures of 71 taps on each of the 7 keys: 496,503 objects.
fn near_cap_chart() -> String {
    let mut text = String::from("#BPM 240\n");
    let slots = "01".repeat(71);
    for m in 1..1_000 {
        for lane in 1..=7 {
            let _ = writeln!(text, "#{m:03}1{lane}:{slots}");
        }
    }
    text
}

/// Times each step of counting `record` of the chart in `text`.
fn time_steps(label: &str, text: &str, record: &PlayRecord) {
    let fx = Fixture::new();
    let chart_path = fx.chart("c.bms", text);
    let (rel, file) = fx.save(record, "1-1.tprl");
    let lib = fx.open();
    let mut steps: Vec<(&str, Duration)> = Vec::new();
    let mut step = |name, began: Instant| steps.push((name, began.elapsed()));

    let t = Instant::now();
    let bytes = fs::read(&file).unwrap();
    let decoded = replay::decode(&bytes, &DecodeLimits::STANDARD).unwrap();
    let id = PlayId::of(rel, &decoded, 0, 0);
    step("read+decode", t);
    let t = Instant::now();
    let preset = replay::verify::check_record(&decoded).unwrap();
    step("check", t);
    let t = Instant::now();
    let chart_bytes = fs::read(&chart_path).unwrap();
    step("chart read", t);
    let t = Instant::now();
    let chart =
        replay::verify::load_recorded_chart(&decoded, &chart_bytes, chart_path.parent().unwrap())
            .unwrap();
    step("load", t);
    let t = Instant::now();
    replay::verify::admit(&decoded, &chart, &preset, REBUILD_MEMORY).unwrap();
    step("admission", t);
    let t = Instant::now();
    let re = replay::verify::evaluate_record(&decoded, &chart, None).unwrap();
    step("evaluation", t);
    let c = Contribution::of(
        id,
        &decoded,
        &Verified {
            completed: re.evaluation.completed,
            random: false,
        },
        &re.evaluation,
        &preset,
    );
    drop(re);
    let t = Instant::now();
    lib.best_writer().unwrap().ingest(&c).unwrap();
    step("commit", t);
    let needed = replay::verify::batch_bytes(&decoded, &chart, &preset).unwrap();
    println!(
        "{label}: {} objects, {} inputs, {} MiB admitted of {} MiB",
        chart.lanes.iter().map(Vec::len).sum::<usize>(),
        decoded.events.len(),
        needed >> 20,
        REBUILD_MEMORY >> 20
    );
    for (name, d) in steps {
        println!("  {name:12} {:>8.1} ms", d.as_secs_f64() * 1_000.0);
    }
}

#[test]
#[ignore = "timing indicator; run in release"]
fn rebuild_steps_on_a_near_cap_chart() {
    let text = near_cap_chart();
    // A few presses at the start.
    let events = (0..50)
        .flat_map(|i| {
            [
                key(1, 1_000 * MS + i * 20 * MS, 1),
                key(1, 1_005 * MS + i * 20 * MS, 0),
            ]
        })
        .collect();
    let record = record_of(&text, 0, events);
    time_steps("near-cap chart", &text, &record);
}

#[test]
#[ignore = "timing indicator; run in release"]
fn rebuild_steps_on_a_dense_record() {
    let text = common::CHART;
    let base = record_of(text, 0, Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let chart = replay::verify::load_recorded_chart(&base, text.as_bytes(), dir.path()).unwrap();
    let preset = rules::tapestry();
    // As many inputs as admission allows, within the decoder's limit.
    let with = |n: usize| {
        let mut r = base.clone();
        r.events = (0..n)
            .map(|i| {
                let i = i64::try_from(i).unwrap();
                key(3, 10 * MS + i * 10, i32::from(i % 2 == 0))
            })
            .collect();
        r
    };
    let fits =
        |n| replay::verify::batch_bytes(&with(n), &chart, &preset).unwrap() <= REBUILD_MEMORY;
    let (mut lo, mut hi) = (0, replay::max_record_events(&DecodeLimits::STANDARD));
    if fits(hi) {
        lo = hi;
    }
    while hi - lo > 1_000 {
        let mid = lo + (hi - lo) / 2;
        if fits(mid) { lo = mid } else { hi = mid }
    }
    let mut record = with(lo);
    let last = record.events.last().map_or(0, |e| e.received.0);
    record.status.finalized = bms_model::TimeUs(last.max(record.status.finalized.0) + 1_000 * MS);
    time_steps("dense record", text, &record);
}
