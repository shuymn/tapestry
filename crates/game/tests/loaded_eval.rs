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

fn digest(evaluation: &Evaluation) -> String {
    let text = format!("{evaluation:?}");
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The digests were recorded with loader version 4, before display
/// positions entered the chart model.
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
    let (input, preset) = (frozen_input(), rules::custom());
    let evaluations: Vec<(&str, Evaluation)> = charts
        .iter()
        .map(|(n, c)| (*n, evaluate(c, &input, &preset).unwrap()))
        .collect();
    let got: Vec<(&str, String)> = evaluations.iter().map(|(n, e)| (*n, digest(e))).collect();
    let expected: [(&str, &str); 6] = [
        (
            "mixed",
            "d882206b9e6d3ad24eacd344111d98ed2e2aa622ea80f513586f011537891059",
        ),
        (
            "cycling",
            "64afb2282e7b703e7129bee9c0eb5c5905ae2de7f76262e80019ff904297c54f",
        ),
        (
            "bpm_stop",
            "e5942d72236e70991959973b08771c4b6048be8b875bf45421c7ed40ad6cc1ee",
        ),
        (
            "mines",
            "aa46638aa1dc8539cf81e8d45ae9a9734623bbc0a33917b452f83a0159cc26b9",
        ),
        (
            "lntype1",
            "738b6e6c2c1676d39214388615faf6309e785fb4ce03b25741b2ce459ae8cd16",
        ),
        (
            "lnobj",
            "738b6e6c2c1676d39214388615faf6309e785fb4ce03b25741b2ce459ae8cd16",
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
