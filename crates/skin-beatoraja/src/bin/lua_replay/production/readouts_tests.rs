//! The play frames' tempo readouts: the BPM range, the main BPM and the
//! current BPM come from the script's chart as the player prepares them,
//! and the reference answers the reads of them with the frame's values.

use skin_model::field;
use skin_model::present::Val;
use skin_model::present::trace::Event;

use super::FrameKind;
use super::compare::compare;
use crate::observed::{Outcome, Point, Raw};
use crate::script::Script;
use crate::tests::{dst, pieces_over};

/// The tempo changes: 180 BPM from 1.6 s, 120 from 2.4 s, so of the
/// seven notes (1.0 s to 3.4 s, 0.4 s apart) two are at the initial 150,
/// two at 180 and three at 120.
const CHANGES: [(i64, i64); 2] = [(1_600_000, 180_000), (2_400_000, 120_000)];

/// The current tempo in hundredths of a BPM at chart time `now`.
fn current_x100(now: i64) -> i32 {
    match now {
        ..1_600_000 => 15_000,
        1_600_000..2_400_000 => 18_000,
        _ => 12_000,
    }
}

/// A number of ids `0` to `4` showing `number(90)`, `number(91)`,
/// `number(92)`, `number(160)`, and a rate whose branches read them under
/// the hi-speed fixes MAX and MAIN and return nothing otherwise.
fn body() -> String {
    let num = |id: &str, read: &str| {
        format!(
            "{{ id = \"{id}\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,\n\
               value = function() {read} end }},\n"
        )
    };
    let mut values = String::new();
    for (id, n) in [("n90", 90), ("n91", 91), ("n92", 92), ("n160", 160)] {
        values.push_str(&num(id, &format!("return ms.number({n})")));
    }
    values.push_str(&num(
        "rate",
        "local e = ms.event_index(55)\n\
         if e == 2 then return ms.number(160) / ms.number(90)\n\
         elseif e == 3 then return ms.number(160) / ms.number(92) end",
    ));
    let mut body = format!("skin.value = {{\n{values}}}\n");
    for (i, id) in ["n90", "n91", "n92", "n160", "rate"].iter().enumerate() {
        body.push_str(&dst(
            &format!("\"{id}\""),
            100 * i32::try_from(i).unwrap(),
            "",
        ));
    }
    body
}

/// A number's raw value as an integer.
fn int(r: Raw) -> Option<i64> {
    match r {
        Raw::Int(n) => Some(n),
        #[expect(clippy::cast_possible_truncation, reason = "integral in the test")]
        Raw::Float(f) if f.fract() == 0.0 => Some(f as i64),
        _ => None,
    }
}

/// Every play frame carries the chart's BPM range (120 to 180), its MAIN
/// (120, the tempo of three of the seven notes) and its current tempo;
/// the reference's `number(90)`, `(91)`, `(92)` and `(160)` answer those
/// values; and the rate, which takes no branch under Classic, is the
/// reference's `nil` against production's 0, drawn as 0 on both sides,
/// and agrees.
#[test]
fn play_frames_carry_the_charts_tempo_readouts() {
    let mut script = Script::full();
    script.tempo = CHANGES.to_vec();
    let (run, obs) = pieces_over(&body(), script);
    let rate = run
        .recorded
        .occurrences
        .iter()
        .find(|o| o.destination == Some(4) && o.field == "value")
        .and_then(|o| o.program)
        .expect("the rate is a program callback");
    let (mut play, mut reads, mut nil, mut zero) = (0, 0, 0, 0);
    for (pf, rf) in run.frames.iter().zip(&obs.frames) {
        if !matches!(pf.kind, FrameKind::Play { .. }) {
            continue;
        }
        play += 1;
        let current = current_x100(pf.now.0);
        let at = |f: u16| pf.fields.get(usize::from(f)).copied();
        for (f, want) in [
            (field::BPM_MIN_X100, 12_000),
            (field::BPM_MAX_X100, 18_000),
            (field::BPM_RANGE_VALID, 1),
            (field::CHART_MAIN_BPM, 120),
            (field::CHART_MAIN_BPM_VALID, 1),
            (field::BPM_CURRENT_X100, current),
            (field::BPM_CURRENT_VALID, 1),
        ] {
            assert_eq!(at(f), Some(want), "field {f} at {}", pf.now.0);
        }
        for p in &rf.points {
            let Point::Ran { key, outcome, .. } = p else {
                continue;
            };
            let want = match key.dest {
                0 => 180,
                1 | 2 => 120,
                3 => i64::from(current / 100),
                4 => {
                    assert_eq!(*outcome, Outcome::Value(Raw::Nil), "at {}", pf.now.0);
                    nil += 1;
                    continue;
                }
                _ => continue,
            };
            let Outcome::Value(r) = outcome else {
                panic!("dest {} at {}: {outcome:?}", key.dest, pf.now.0);
            };
            assert_eq!(int(*r), Some(want), "dest {} at {}", key.dest, pf.now.0);
            reads += 1;
        }
        zero += pf
            .events
            .iter()
            .filter(|e| matches!(e, Event::Ran { k, result: Ok(Val::Int(0)), .. } if *k == rate))
            .count();
    }
    assert!(play > 0);
    assert_eq!(reads, play * 4);
    assert_eq!(nil, play);
    assert_eq!(zero, play);
    let report = compare(&run, &obs, &[]);
    assert!(report.passes(), "{}", report.text(0, true, true));
}
