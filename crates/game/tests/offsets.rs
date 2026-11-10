//! The judgment offsets ring through the logic step (design.md "Rendering
//! and Skins"): each judgment the tally counts as PERFECT to BAD records
//! note time minus press, truncated toward 0 to ms; a long note records
//! once, by the side its judgment took; the play view carries the newest
//! 100 with a count that never stops at 100, whatever effects are lost,
//! and a rearm clears them.

mod common;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, device, judgments, lane, offset_taps, press, release, tap_offset_ms, tap_time};
use eval::{InputStart, JudgeEventKind, PhysicalInputEvent};
use game::{Logic, LogicConfig, OFFSET_SLOTS, OffsetRing, RenderSide};
use input::{SourceReader, SourceWriter, source_queue};
use rules::{Judgment, RulePreset};

/// A play of `chart` over one source: the logic step, its render side and
/// the source's writer, holding `inputs` inputs and `effects` effects.
fn play<'a>(
    chart: &'a Chart,
    rule: &'a RulePreset,
    dev: &'a eval::DeviceProfile,
    effects: usize,
) -> (Logic<'a, SourceReader>, RenderSide, SourceWriter) {
    let (writer, reader) = source_queue(1_024);
    let (logic, side) = Logic::new(
        chart,
        rule,
        dev,
        &InputStart::default(),
        vec![reader],
        LogicConfig {
            inputs: 1_024,
            effects,
        },
        Vec::new(),
    )
    .unwrap();
    (logic, side, writer)
}

/// Pushes the inputs received before `until` (those after stay in
/// `events`), publishes the source through `until` and steps there.
fn feed(
    logic: &mut Logic<'_, SourceReader>,
    writer: &mut SourceWriter,
    events: &mut Vec<PhysicalInputEvent>,
    until: i64,
) {
    let n = events.iter().take_while(|e| e.received.0 < until).count();
    for ev in events.drain(..n) {
        writer.push(ev).unwrap();
    }
    writer.publish(TimeUs(until));
    logic.advance_to(TimeUs(until));
}

fn recorded(ring: &OffsetRing) -> Vec<i32> {
    ring.recorded().collect()
}

fn ms(values: impl IntoIterator<Item = i64>) -> Vec<i32> {
    values
        .into_iter()
        .map(|v| i32::try_from(v).unwrap())
        .collect()
}

/// Presses judged PERFECT to BAD record note time minus press, early
/// positive, truncated toward 0: 12 345 µs early gives 12, 999 µs late
/// gives 0 (not −1), 50 500 µs late −50 and 150 900 µs early 150 (BAD). A miss
/// and an empty POOR record nothing, each in the stream and the tally.
#[test]
fn counted_judgments_record_their_offsets_and_nothing_else_does() {
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(2_000 * MS))
        .tap(lane(3), TimeUs(3_000 * MS))
        .tap(lane(4), TimeUs(4_000 * MS))
        .tap(lane(5), TimeUs(5_000 * MS))
        .tap(lane(6), TimeUs(6_000 * MS))
        .build()
        .unwrap();
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let (mut logic, mut side, mut writer) = play(&chart, &rule, &dev, 64);
    let mut events = vec![
        press(1_000 * MS - 12_345, 0, lane(1)),
        release(1_100 * MS, 0, lane(1)),
        press(2_000 * MS + 999, 0, lane(2)),
        release(2_100 * MS, 0, lane(2)),
        // Lane 3 is never pressed: a miss.
        // 300 ms ahead of lane 4's note, outside its BAD window: an empty
        // POOR; the note itself is then missed.
        press(3_700 * MS, 0, lane(4)),
        release(3_750 * MS, 0, lane(4)),
        press(5_000 * MS + 50_500, 0, lane(5)),
        release(5_100 * MS, 0, lane(5)),
        press(6_000 * MS - 150_900, 0, lane(6)),
        release(6_100 * MS, 0, lane(6)),
    ];
    let mut stream = Vec::new();
    for until in (500..=7_000).step_by(500) {
        feed(&mut logic, &mut writer, &mut events, until * MS);
        stream.extend_from_slice(logic.events());
    }
    assert!(events.is_empty());
    let judged: Vec<_> = judgments(&stream)
        .into_iter()
        .map(|(l, _, j, d)| (l, j, d))
        .collect();
    assert_eq!(
        judged,
        [
            (1, Judgment::Perfect, Some(-12_345)),
            (2, Judgment::Perfect, Some(999)),
            (3, Judgment::Poor, None),
            (4, Judgment::Poor, None),
            (5, Judgment::Good, Some(50_500)),
            (6, Judgment::Bad, Some(-150_900)),
        ]
    );
    let empty = stream
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::EmptyPoor { .. }))
        .count();
    assert_eq!(empty, 1);
    let view = logic.view();
    assert_eq!(view.tally.counts.poor, 2);
    assert_eq!(view.tally.empty_poor, 1);
    assert_eq!(recorded(&view.offsets), [12, 0, -50, 150]);
    assert_eq!(view.offsets.count(), 4);
    // The render side's view carries the same ring.
    assert_eq!(side.view().offsets, view.offsets);
}

/// A long note records once, at its counted judgment: a release judged
/// GREAT 120 ms before the end, farther than its head's 30 ms early press,
/// records the release's side (120, early); one held through its end
/// records its head's 7 ms late press. Its `LnHeld` records nothing.
#[test]
fn a_long_note_records_once_by_its_judgments_side() {
    let chart = ChartBuilder::new(150_000)
        .long(lane(1), TimeUs(1_000 * MS), TimeUs(2_000 * MS))
        .long(lane(2), TimeUs(3_000 * MS), TimeUs(4_000 * MS))
        .build()
        .unwrap();
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let (mut logic, _side, mut writer) = play(&chart, &rule, &dev, 64);
    let mut events = vec![
        press(970 * MS, 0, lane(1)),
        release(1_880 * MS, 0, lane(1)),
        press(3_007 * MS, 0, lane(2)),
        release(4_300 * MS, 0, lane(2)),
    ];
    let mut stream = Vec::new();
    for until in (500..=5_000).step_by(500) {
        feed(&mut logic, &mut writer, &mut events, until * MS);
        stream.extend_from_slice(logic.events());
    }
    let held = stream
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::LnHeld { .. }))
        .count();
    assert_eq!(held, 2);
    let judged: Vec<_> = stream
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Judged {
                judgment,
                deviation,
                release,
                ..
            } => Some((judgment, deviation.map(|d| d.0), release.map(|r| r.0))),
            _ => None,
        })
        .collect();
    assert_eq!(
        judged,
        [
            (Judgment::Great, Some(-30_000), Some(-120_000)),
            (Judgment::Perfect, Some(7_000), None),
        ]
    );
    let view = logic.view();
    assert_eq!(recorded(&view.offsets), [120, -7]);
    assert_eq!(view.offsets.count(), 2);
}

/// The ring through the play view: empty before any judgment; 37 offsets
/// in order; with the effect queue full from the first step, 130
/// judgments leave the newest 100 and a count of 130, and a reader that
/// last read at 30 sees exactly 100 new ones (one whole revolution, the
/// head back where it was); a rearm clears it, and the next candidate's
/// judgments count from 0.
#[test]
fn the_ring_keeps_the_newest_100_whatever_effects_are_lost_and_a_rearm_clears_it() {
    let (chart, all) = offset_taps(130);
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let (mut logic, mut side, mut writer) = play(&chart, &rule, &dev, 1);
    let mut events = all.clone();

    let empty = side.view().offsets;
    assert_eq!(empty.count(), 0);
    assert_eq!(empty.head(), 0);
    assert_eq!(recorded(&empty), Vec::<i32>::new());
    assert_eq!(empty.since(0).count(), 0);

    // Past the 30th note's press, before the 31st's: what a reader last
    // read.
    feed(&mut logic, &mut writer, &mut events, tap_time(29) + 50 * MS);
    let read = side.view().offsets.count();
    assert_eq!(read, 30);
    // Past the 37th's.
    feed(&mut logic, &mut writer, &mut events, tap_time(36) + 50 * MS);
    let partial = side.view().offsets;
    assert_eq!(partial.count(), 37);
    assert_eq!(partial.head(), 37);
    assert_eq!(recorded(&partial), ms((0..37).map(tap_offset_ms)));

    feed(
        &mut logic,
        &mut writer,
        &mut events,
        tap_time(129) + 200 * MS,
    );
    assert!(events.is_empty());
    let full = side.view().offsets;
    assert!(side.dropped() > 0, "the effect queue never filled");
    let counts = logic.view().tally.counts;
    assert_eq!((counts.perfect + counts.great, counts.poor), (130, 0));
    assert_eq!(full.count(), 130);
    assert_eq!(recorded(&full), ms((30..130).map(tap_offset_ms)));
    // The reader that last read 30: one whole revolution since.
    assert_eq!(full.head(), 30);
    assert_eq!(
        full.since(read).collect::<Vec<_>>(),
        ms((30..130).map(tap_offset_ms))
    );
    // One that last read 37 sees the 93 after; one that read none, the
    // newest 100.
    assert_eq!(
        full.since(partial.count()).collect::<Vec<_>>(),
        ms((37..130).map(tap_offset_ms))
    );
    assert_eq!(full.since(0).count(), OFFSET_SLOTS);

    // The next candidate, its source reopened as the input side does.
    drop(logic.lend_readers());
    let (mut writer, reader) = source_queue(16);
    logic.restore_readers(vec![reader]);
    logic.rearm(&InputStart::default(), TimeUs(20_000 * MS));
    let cleared = side.view().offsets;
    assert_eq!(cleared, OffsetRing::EMPTY);
    assert_eq!(logic.view().offsets, OffsetRing::EMPTY);

    let mut again = vec![
        press(tap_time(0) - 3_000, 0, lane(1)),
        release(tap_time(0) + 50 * MS, 0, lane(1)),
    ];
    feed(&mut logic, &mut writer, &mut again, tap_time(0) + 200 * MS);
    let next = side.view().offsets;
    assert_eq!(next.count(), 1);
    assert_eq!(recorded(&next), [3]);
}
