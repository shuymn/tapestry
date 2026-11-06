//! Each lane event's presses before it in its µs (design.md "The Only
//! Output Is the Judgment Event Stream"), and the lane facts' count of
//! presses at the latest press's µs: counted edge by edge, a reversal's
//! release before its press, and equal between a full log and the
//! section opening equivalent to it.

mod common;

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use common::{MS, device, lane, press, release};
use eval::{EvalInput, Evaluator, InputStart, JudgeEvent, JudgeEventKind, Section, evaluate};
use rules::Judgment;

/// Key 1's tap at 1 s; key 2's two taps at 2 s; key 3's long notes from
/// 3 to 3.5 s and from 4 to 4.5 s.
fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(2_000 * MS))
        .tap(lane(2), TimeUs(2_000 * MS))
        .long(lane(3), TimeUs(3_000 * MS), TimeUs(3_500 * MS))
        .long(lane(3), TimeUs(4_000 * MS), TimeUs(4_500 * MS))
        .build()
        .unwrap()
}

/// The lane events in order, as (lane, time in ms, judgment, presses):
/// `None` for an empty POOR's or a held head's judgment.
fn lane_events(out: &[JudgeEvent]) -> Vec<(u8, i64, Option<Judgment>, u32)> {
    out.iter()
        .filter_map(|e| {
            let at = e.time.0 / MS;
            let l = |lane: Lane| u8::try_from(lane.index()).unwrap();
            match e.kind {
                JudgeEventKind::Judged {
                    lane,
                    judgment,
                    presses,
                    ..
                } => Some((l(lane), at, Some(judgment), presses)),
                JudgeEventKind::LnHeld { lane, presses, .. }
                | JudgeEventKind::EmptyPoor { lane, presses, .. } => {
                    Some((l(lane), at, None, presses))
                }
                _ => None,
            }
        })
        .collect()
}

/// Key 1 pressed, released and pressed in one µs: the BAD counts its
/// press, the second press counts two in the facts. Key 2's two taps of
/// one µs, each taken by a press of that µs: one and two. Key 3's long
/// note released 100 ms early and pressed again in one µs: the release's
/// judgment counts no press, the facts one after it; the next pressed and
/// released in one µs: the release's judgment counts the press. A press
/// at a later µs counts from 1.
#[test]
fn each_event_counts_the_presses_before_it_in_its_microsecond() {
    let chart = chart();
    let dev = device(&[0]);
    let rule = rules::tapestry();
    let mut ev = Evaluator::new(&chart, &rule, &dev, &InputStart::default(), None, 64).unwrap();
    let t1 = 1_160 * MS;
    let t2 = 2_000 * MS;
    let t3 = 3_400 * MS;
    let t4 = 4_000 * MS;
    for i in [
        press(t1, 0, 1),
        release(t1, 0, 1),
        press(t1, 0, 1),
        release(t1 + 10 * MS, 0, 1),
        press(t2, 0, 2),
        release(t2, 0, 2),
        press(t2, 0, 2),
        release(t2 + 10 * MS, 0, 2),
        press(3_000 * MS, 0, 3),
        release(t3, 0, 3),
        press(t3, 0, 3),
        release(t3 + 10 * MS, 0, 3),
        press(t4, 0, 3),
        release(t4, 0, 3),
    ] {
        ev.push_input(i).unwrap();
    }
    let mut out = Vec::new();
    ev.advance_to(TimeUs(t1), &mut out).unwrap();
    assert_eq!(lane_events(&out), [(1, 1_160, Some(Judgment::Bad), 1)]);
    assert_eq!(ev.input().lane(lane(1)).presses, 2);
    out.clear();
    ev.advance_to(TimeUs(t2), &mut out).unwrap();
    assert_eq!(
        lane_events(&out),
        [
            (2, 2_000, Some(Judgment::Perfect), 1),
            (2, 2_000, Some(Judgment::Perfect), 2)
        ]
    );
    assert_eq!(ev.input().lane(lane(2)).presses, 2);
    out.clear();
    ev.advance_to(TimeUs(3_000 * MS), &mut out).unwrap();
    assert_eq!(lane_events(&out), [(3, 3_000, None, 1)]);
    out.clear();
    ev.advance_to(TimeUs(t3), &mut out).unwrap();
    let released = lane_events(&out);
    assert_eq!(released.len(), 1, "{released:?}");
    assert_eq!((released[0].0, released[0].1, released[0].3), (3, 3_400, 0));
    assert_eq!(ev.input().lane(lane(3)).presses, 1);
    out.clear();
    ev.advance_to(TimeUs(t4), &mut out).unwrap();
    // Its head held at the press, then its release, BAD, both counting
    // the press.
    assert_eq!(
        lane_events(&out),
        [(3, 4_000, None, 1), (3, 4_000, Some(Judgment::Bad), 1)]
    );
    // A press at a later µs: counted from 1 again.
    ev.push_input(press(5_000 * MS, 0, 1)).unwrap();
    ev.advance_to(TimeUs(5_000 * MS), &mut out).unwrap();
    assert_eq!(ev.input().lane(lane(1)).presses, 1);
}

/// A section's events carry the same counts from a full log, whose input
/// before the lead-in only converts, as from the opening equivalent to it,
/// which starts with no input before the section.
#[test]
fn a_section_counts_alike_from_its_full_log_and_its_opening() {
    let chart = chart();
    let rule = rules::tapestry();
    let section = Section {
        start: TimeUs(1_800 * MS),
        end: TimeUs(6_000 * MS),
        lead_in: TimeUs(500 * MS),
        initial_gauges: rule.gauges.iter().map(|g| g.start).collect(),
    };
    let t2 = 2_000 * MS;
    let later = vec![
        press(t2, 0, 2),
        release(t2, 0, 2),
        press(t2, 0, 2),
        release(t2 + 10 * MS, 0, 2),
        press(4_000 * MS, 0, 3),
        release(4_000 * MS, 0, 3),
    ];
    let mut earlier = vec![
        press(100 * MS, 0, 2),
        release(100 * MS, 0, 2),
        press(100 * MS, 0, 2),
        release(200 * MS, 0, 2),
        press(1_000 * MS, 0, 1),
        release(1_010 * MS, 0, 1),
    ];
    earlier.extend(later.iter().copied());
    let input = |events| EvalInput {
        events,
        device: device(&[0]),
        section: Some(section.clone()),
        ..EvalInput::default()
    };
    let full = evaluate(&chart, &input(earlier), &rule).unwrap();
    let opening = evaluate(&chart, &input(later), &rule).unwrap();
    assert_eq!(full, opening);
    let counted = lane_events(&full.events);
    assert!(
        counted.contains(&(2, 2_000, Some(Judgment::Perfect), 2)),
        "{counted:?}"
    );
}
