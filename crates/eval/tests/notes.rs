//! The note facts the evaluator keeps for drawing (`Evaluator::notes`):
//! taps judged, long-note heads taken, mines hit and each lane's visual
//! long-note hold, by the chart's own note identities, following the
//! judgments step by step. The tapestry preset: BAD ±180 ms, LN-end edges
//! GOOD 200 ms before the end.

mod common;

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, device, lane, press, release};
use eval::{Evaluator, InputStart, JudgeEvent, NoteHold, PhysicalInputEvent};

const BPM: i64 = 150_000;

/// Advances `ev` to each watermark in turn after pushing the inputs
/// before it, and returns the facts after each: whether key 1's notes 0
/// to 2 are done, key 2's hold, and whether key 3's mine is hit.
fn steps(
    ev: &mut Evaluator<'_>,
    inputs: &[PhysicalInputEvent],
    marks: &[i64],
) -> Vec<([bool; 3], Option<NoteHold>, bool)> {
    let mut out: Vec<JudgeEvent> = Vec::new();
    let mut rest = inputs;
    marks
        .iter()
        .map(|&w| {
            while let Some((first, tail)) = rest.split_first()
                && first.received.0 <= w
            {
                ev.push_input(*first).unwrap();
                rest = tail;
            }
            ev.advance_to(TimeUs(w), &mut out).unwrap();
            let n = ev.notes();
            (
                [0, 1, 2].map(|i| n.is_done(lane(1), i)),
                n.hold(lane(2)),
                n.is_done(lane(3), 0),
            )
        })
        .collect()
}

/// Taps on key 1 at 1, 2 and 3 s; on key 2 a long note from 4 s to 5 s
/// and one from 6 s to 7 s; a mine on key 3 at 8 s.
fn chart() -> bms_model::Chart {
    ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(2_000 * MS))
        .tap(lane(1), TimeUs(3_000 * MS))
        .long(lane(2), TimeUs(4_000 * MS), TimeUs(5_000 * MS))
        .long(lane(2), TimeUs(6_000 * MS), TimeUs(7_000 * MS))
        .mine(lane(3), TimeUs(8_000 * MS), 10)
        .build()
        .unwrap()
}

/// A judged tap, a missed one, a held head ended by an early release, a
/// BAD head held through its end, and a mine hit: each fact lands with
/// the step that decides it; holding a lane on does not bring an ended
/// hold back; a reset clears everything and advances the revision.
#[test]
fn facts_follow_the_judgments() {
    let chart = chart();
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let mut ev = Evaluator::new(&chart, &rule, &dev, &InputStart::default(), None, 64).unwrap();
    let inputs = [
        // Tap 0 at +10 ms.
        press(1_010 * MS, 0, 1),
        release(1_050 * MS, 0, 1),
        // Tap 1 is missed; tap 2 at −30 ms.
        press(2_970 * MS, 0, 1),
        release(3_010 * MS, 0, 1),
        // The first long note held at +5 ms, released 400 ms early, held
        // again before its end.
        press(4_005 * MS, 0, 2),
        release(4_600 * MS, 0, 2),
        press(4_700 * MS, 0, 2),
        release(4_800 * MS, 0, 2),
        // The second taken BAD at +150 ms and held through its end.
        press(6_150 * MS, 0, 2),
        // A mine passing while key 3 is held.
        press(7_900 * MS, 0, 3),
    ];
    let hold = |note, start: i64, end: i64| NoteHold {
        note,
        start: TimeUs(start * MS),
        end: TimeUs(end * MS),
    };
    let got = steps(
        &mut ev,
        &inputs,
        &[
            1_000, 1_010, 2_100, 2_180, 2_970, 4_005, 4_599, 4_600, 4_700, 6_150, 6_999, 7_000,
            8_000,
        ]
        .map(|m| m * MS),
    );
    let none = [false; 3];
    let want = [
        (none, None, false),
        ([true, false, false], None, false),
        // Tap 1's miss lands at its late BAD edge.
        ([true, false, false], None, false),
        ([true, true, false], None, false),
        ([true, true, true], None, false),
        ([true, true, true], Some(hold(0, 4_005, 5_000)), false),
        ([true, true, true], Some(hold(0, 4_005, 5_000)), false),
        // Released early: the hold ends.
        ([true, true, true], None, false),
        // Pressed again on the same long note: no hold comes back.
        ([true, true, true], None, false),
        // A BAD head: scored at the press, its hold shown.
        ([true, true, true], Some(hold(1, 6_150, 7_000)), false),
        ([true, true, true], Some(hold(1, 6_150, 7_000)), false),
        // Its end passed while held.
        ([true, true, true], None, false),
        ([true, true, true], None, true),
    ];
    assert_eq!(got, want);
    // Each long note's head is taken.
    assert!(ev.notes().is_done(lane(2), 0) && ev.notes().is_done(lane(2), 1));
    let revision = ev.notes().revision();
    ev.reset(&InputStart::default());
    assert!(ev.notes().revision() > revision);
    let n = ev.notes();
    for (l, notes) in chart.lanes.iter().enumerate() {
        let l = lane(u8::try_from(l).unwrap());
        assert!((0..notes.len()).all(|i| !n.is_done(l, i)), "{l:?}");
        assert_eq!(n.hold(l), None);
    }
}
