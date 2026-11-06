//! Evaluation of loaded charts under one frozen input stream: the event
//! stream depends on the chart's judgment times, kinds and sounds only, so
//! display geometry never changes it (design.md "Chart Model").

mod common;

use std::fs;
use std::path::Path;

use bms_model::Chart;
use bms_parser::{LoadOptions, load_bms};
use common::{MS, device, lane, press};
use eval::{EvalInput, Evaluation, InputKind, PhysicalInputEvent, evaluate};
use sha2::{Digest, Sha256};

/// Few BPMs, STOPs at changes, measure lengths, long notes, BGM.
const MIXED: &str = "\
#BPM 137.5
#BPM01 211.25
#BPM02 89.875
#STOP01 96
#STOP02 13.5
#WAV01 a.wav
#WAV02 b.wav
#00111:01020102
#00102:0.75
#00109:0001
#00203:B4
#00211:0000000201
#00208:000002
#00302:1.375
#00316:0102010201
#00309:02
#00408:01
#00451:01000002
#00401:0000000001
#00513:01
";

/// Sixty measures cycling three 6-digit BPMs, notes at sevenths, ninths and
/// elevenths, STOPs (one shorter than a microsecond) and short measures.
fn cycling() -> String {
    let mut text = String::from(
        "#BPM 147.123456\n#BPM01 333.333333\n#BPM02 98.765432\n#BPM03 222.222\n\
         #STOP01 7.654321\n#STOP02 0.000003\n#WAV01 a.wav\n",
    );
    let sevenths = "01".repeat(7);
    let ninths = "0100".repeat(9);
    let elevenths = "000001".repeat(11);
    for m in 0..60 {
        let bpm = ["01", "02", "03"][m % 3];
        text.push_str(&format!("#{m:03}08:00{bpm}00\n"));
        text.push_str(&format!("#{m:03}11:{sevenths}\n"));
        text.push_str(&format!("#{m:03}14:{ninths}\n"));
        text.push_str(&format!("#{m:03}19:{elevenths}\n"));
        if m % 4 == 1 {
            text.push_str(&format!("#{m:03}09:0001000200\n"));
        }
        if m % 5 == 2 {
            text.push_str(&format!("#{m:03}02:0.{:06}\n", 123_457 + m * 1_013));
        }
    }
    text
}

fn load(bytes: &[u8]) -> Chart {
    let dir = tempfile::tempdir().unwrap();
    load_bms(bytes, dir.path(), &LoadOptions::default())
        .unwrap()
        .chart
}

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../bms-parser/tests/charts")
        .join(name);
    fs::read(path).unwrap()
}

/// The frozen input: one keyboard pressing the lanes in turn every
/// 29.017 ms from 0.3 s and releasing each 41.003 ms later, never derived
/// from the chart it is played on.
fn frozen_input() -> EvalInput {
    let mut events = Vec::new();
    for i in 0..1_200_i64 {
        let l = lane(u8::try_from(i % 8).unwrap());
        let at = 300 * MS + i * 29_017;
        events.push(press(at, 0, l));
        let mut up = press(at + 41_003, 0, l);
        if let InputKind::Control { value, .. } = &mut up.kind {
            *value = 0;
        }
        events.push(up);
    }
    events.sort_by_key(|e: &PhysicalInputEvent| e.received);
    EvalInput {
        events,
        device: device(&[0]),
        ..EvalInput::default()
    }
}

/// The evaluation as printed, less what only presentation reads (design.md
/// "Policies, Combinations and Versions"): an empty POOR's deviation and
/// each event's presses before it in its µs, so a change of results changes
/// the digest and a presentation value does not.
fn results(evaluation: &Evaluation) -> String {
    let full = format!("{evaluation:?}");
    let mut printed = String::with_capacity(full.len());
    let mut rest = full.as_str();
    while let Some(i) = rest.find(", presses: ") {
        printed.push_str(&rest[..i]);
        let tail = &rest[i + ", presses: ".len()..];
        let digits = tail
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(tail.len());
        rest = &tail[digits..];
    }
    printed.push_str(rest);
    let mut text = String::with_capacity(printed.len());
    let mut rest = printed.as_str();
    while let Some(i) = rest.find("EmptyPoor {") {
        let (head, tail) = rest.split_at(i);
        text.push_str(head);
        let end = tail.find('}').unwrap();
        let body = &tail[..end];
        let cut = body.find(", deviation").unwrap_or(body.len());
        text.push_str(&body[..cut]);
        text.push(' ');
        rest = &tail[end..];
    }
    text.push_str(rest);
    text
}

fn digest(evaluation: &Evaluation) -> String {
    let text = results(evaluation);
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The digests pin the results of standard evaluator version 4, its
/// events as printed less presentation-only values ([`results`]); display
/// geometry takes no part in it.
#[test]
fn loaded_charts_evaluate_as_before() {
    let charts = [
        ("mixed", load(MIXED.as_bytes())),
        ("cycling", load(cycling().as_bytes())),
        ("bpm_stop", load(&fixture("bpm_stop.bms"))),
        ("mines", load(&fixture("mines.bms"))),
        ("lntype1", load(&fixture("long_notes_lntype1.bms"))),
        ("lnobj", load(&fixture("long_notes_lnobj.bms"))),
    ];
    let (input, preset) = (frozen_input(), rules::tapestry());
    let evaluations: Vec<(&str, Evaluation)> = charts
        .iter()
        .map(|(n, c)| (*n, evaluate(c, &input, &preset).unwrap()))
        .collect();
    let got: Vec<(&str, String)> = evaluations.iter().map(|(n, e)| (*n, digest(e))).collect();
    let expected: [(&str, &str); 6] = [
        (
            "mixed",
            "0ac86e4990f2e43d9aefe2881f6389bc365a1ee030bbae0baafebaf020498faf",
        ),
        (
            "cycling",
            "49429a3a6cf6ca1baf4c4c7d1e2d3471f9bca58ca7f8143acffddb0c115a759b",
        ),
        (
            "bpm_stop",
            "f2f39c9ca77d208e4ca9d3c658453cfb1acc533d86dfc3233a6b886af64c505d",
        ),
        (
            "mines",
            "c2fd0d676d6a76cd5c89d7b7a0a11ecb81baa06de863c4c1a2418e41ec4d73c6",
        ),
        (
            "lntype1",
            "e9549920fa034c5c1a141563e588f512a5055269ad2590a0fe37a64a7fde60c4",
        ),
        (
            "lnobj",
            "e9549920fa034c5c1a141563e588f512a5055269ad2590a0fe37a64a7fde60c4",
        ),
    ];
    let got: Vec<(&str, &str)> = got.iter().map(|(n, d)| (*n, d.as_str())).collect();
    assert_eq!(got, expected);
    // The input reaches notes: every chart has judgments made by a press.
    for (name, evaluation) in &evaluations {
        let pressed = evaluation.events.iter().filter(|e| {
            matches!(
                e.kind,
                eval::JudgeEventKind::Judged {
                    deviation: Some(_),
                    ..
                }
            )
        });
        assert!(pressed.count() > 0, "{name}");
    }
}
