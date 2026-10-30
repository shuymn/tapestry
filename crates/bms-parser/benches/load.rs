//! CPU benchmark of the loader (design.md "Measurement"): loading generated BMS
//! text with many BPM changes, at 1× and at a practice speed. The text is built here; the sound folder is a
//! temporary one holding the single sound the text names.

use std::fmt::Write as _;
use std::fs;
use std::hint::black_box;

use bms_model::Speed;
use bms_parser::{LoadOptions, load_bms};
use criterion::{Criterion, criterion_group, criterion_main};

const MEASURES: usize = 400;
/// Slots per measure: one BPM change and one note in each.
const SLOTS: usize = 16;

/// A chart of `MEASURES` measures whose every sixteenth sets a new tempo
/// (channel 03, hexadecimal) and carries a tap, with one more on the
/// scratch every fourth.
fn chart_text() -> String {
    let mut text = String::from("#PLAYER 1\n#TITLE Bench\n#BPM 120\n#RANK 2\n#WAV01 click.wav\n");
    for m in 1..=MEASURES {
        let bpm: String = (0..SLOTS)
            .map(|s| format!("{:02X}", 60 + (m * SLOTS + s) * 7 % 180))
            .collect();
        let taps = "01".repeat(SLOTS);
        let scratch: String = (0..SLOTS)
            .map(|s| if s % 4 == 0 { "01" } else { "00" })
            .collect();
        writeln!(text, "#{m:03}03:{bpm}").expect("write to a string");
        writeln!(text, "#{m:03}11:{taps}").expect("write to a string");
        writeln!(text, "#{m:03}16:{scratch}").expect("write to a string");
    }
    text
}

fn bench_load(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("temporary folder");
    fs::write(dir.path().join("click.wav"), b"").expect("write the sound");
    let text = chart_text();
    let opts = LoadOptions::default();
    let loaded = load_bms(text.as_bytes(), dir.path(), &opts).expect("the text loads");
    assert!(loaded.chart.lanes.iter().map(Vec::len).sum::<usize>() > MEASURES * SLOTS);
    c.bench_function("load_many_bpm_changes", |b| {
        b.iter(|| load_bms(black_box(text.as_bytes()), dir.path(), &opts).expect("the text loads"));
    });
    // 65 %: the real timeline and its scroll map are built beside the 1×
    // ones in the same pass.
    let slow = LoadOptions {
        speed: Speed::from_twentieths(13).expect("on the ladder"),
        ..opts
    };
    c.bench_function("load_many_bpm_changes_at_65_percent", |b| {
        b.iter(|| load_bms(black_box(text.as_bytes()), dir.path(), &slow).expect("the text loads"));
    });
}

criterion_group!(benches, bench_load);
criterion_main!(benches);
