//! Autoplay on notes that share a µs (design.md "What is rounded"): each
//! is pressed, released before the next press of its lane at that µs, and
//! judged PERFECT. Each timestamp's whole batch of inputs is queued before
//! evaluation is finalized at it, as the logic step does.

mod common;

use bms_model::{Chart, NoteKind, TimeUs};
use bms_parser::{LoadOptions, load_bms};
use common::device;
use eval::{Evaluator, InputStart, JudgeEvent, JudgeEventKind, LnOutcome};
use rules::{GaugeValue, Judgment};

fn load(text: &str) -> Chart {
    let dir = tempfile::tempdir().unwrap();
    load_bms(text.as_bytes(), dir.path(), &LoadOptions::default())
        .unwrap()
        .chart
}

/// Plays `chart` with autoplay, queueing each timestamp's inputs before
/// advancing to it, then past every deadline.
fn autoplayed(chart: &Chart) -> Vec<JudgeEvent> {
    let device = device(&[0]);
    let inputs = input::autoplay(chart, &device.sources[0]).unwrap();
    let preset = rules::custom();
    let mut ev = Evaluator::new(
        chart,
        &preset,
        &device,
        &InputStart::default(),
        inputs.len(),
    )
    .unwrap();
    let mut out = Vec::new();
    let mut rest = inputs.as_slice();
    while let Some(first) = rest.first() {
        let at = first.received;
        let n = rest.iter().take_while(|e| e.received == at).count();
        for &e in &rest[..n] {
            ev.push_input(e).unwrap();
        }
        rest = &rest[n..];
        ev.advance_to(at, &mut out).unwrap();
    }
    let end = ev.final_deadline();
    ev.advance_to(end, &mut out).unwrap();
    assert!(ev.is_complete());
    out
}

/// The (lane, note) pairs of `chart` whose kind `pick` accepts.
fn notes_where(chart: &Chart, pick: fn(&NoteKind) -> bool) -> Vec<(usize, usize)> {
    chart
        .lanes
        .iter()
        .enumerate()
        .flat_map(|(l, notes)| {
            notes
                .iter()
                .enumerate()
                .filter(move |(_, n)| pick(&n.kind))
                .map(move |(i, _)| (l, i))
        })
        .collect()
}

/// Every note judged once and PERFECT, every long note completed once,
/// nothing else judged or missed.
#[track_caller]
fn all_perfect(chart: &Chart) {
    let events = autoplayed(chart);
    let mut judged = Vec::new();
    let mut completed = Vec::new();
    for e in &events {
        match e.kind {
            JudgeEventKind::Judged {
                lane,
                note,
                judgment,
                ..
            } => {
                assert_eq!(judgment, Judgment::Perfect, "{e:?}");
                judged.push((lane.index(), note));
            }
            JudgeEventKind::LnEnd {
                lane,
                note,
                outcome,
            } => {
                assert_eq!(outcome, LnOutcome::Completed, "{e:?}");
                completed.push((lane.index(), note));
            }
            JudgeEventKind::EmptyPoor { .. } | JudgeEventKind::MineHit { .. } => {
                panic!("{e:?}");
            }
            _ => {}
        }
    }
    judged.sort_unstable();
    completed.sort_unstable();
    assert_eq!(
        judged,
        notes_where(chart, |k| !matches!(k, NoteKind::Mine { .. }))
    );
    assert_eq!(
        completed,
        notes_where(chart, |k| matches!(k, NoteKind::Long { .. }))
    );
}

/// Measure 0 lasts 2 µs at 120 BPM (4e-6 beat), measure 1 is ordinary.
const HEAD: &str = "#BPM 120\n#LNTYPE 1\n#LNOBJ ZZ\n#WAV01 a.wav\n#00002:0.000001\n";

#[test]
fn autoplay_hits_taps_stacked_in_one_microsecond() {
    // Key 1: taps at 0, 1/8 and 1/4 of measure 0, all in µs 0.
    let chart = load(&format!("{HEAD}#00011:0101010000000000\n#00111:01\n"));
    assert_eq!(chart.lanes[1].len(), 4);
    assert_eq!(chart.lanes[1][2].time, TimeUs(0));
    all_perfect(&chart);
}

#[test]
fn autoplay_hits_long_notes_of_one_microsecond() {
    // Key 1: #LNTYPE 1 from 0 to 1/4; key 2: #LNOBJ over the same span.
    let chart = load(&format!(
        "{HEAD}#00051:01010000\n#00012:01ZZ0000\n#00111:01\n"
    ));
    assert!(matches!(
        chart.lanes[1][0].kind,
        NoteKind::Long { end: TimeUs(0), .. }
    ));
    all_perfect(&chart);
}

#[test]
fn autoplay_hits_a_head_at_the_microsecond_a_long_note_ends() {
    // Key 3: a long note from 0 to 1/8 and a tap at 1/4, all in µs 0, then
    // a long note from 1/2 (1 µs) to measure 1 and a tap after it.
    let chart = load(&format!(
        "{HEAD}#00053:0101000000000000\n#00013:0000010000000000\n\
         #00053:0000000001000000\n#00153:01\n#00113:0001\n"
    ));
    assert_eq!(chart.lanes[3].len(), 4);
    all_perfect(&chart);
}

#[test]
fn autoplay_releases_before_a_mine_later_in_its_microsecond() {
    // Key 4: a tap at 0 and a mine at 1/4 of measure 0, both in µs 0: the
    // tap is released at the mine's µs, before its deadline.
    let chart = load(&format!(
        "{HEAD}#00014:01000000\n#000D4:000A0000\n#00114:01\n"
    ));
    assert_eq!(chart.lanes[4].len(), 3);
    all_perfect(&chart);
}

#[test]
fn autoplay_holds_its_press_over_a_mine_earlier_in_its_microsecond() {
    // Key 4: a mine of damage 10 at 0 and a tap at 1/4 of measure 0, both
    // in µs 0. Autoplay presses the tap at that µs and holds it, so the
    // mine, whose deadline is at the same µs, hits after the press.
    let chart = load(&format!(
        "{HEAD}#000D4:0A000000\n#00014:00010000\n#00114:01\n"
    ));
    assert_eq!(
        chart.lanes[4]
            .iter()
            .map(|n| (n.time, n.kind))
            .collect::<Vec<_>>()[..2],
        [
            (TimeUs(0), NoteKind::Mine { damage: 10 }),
            (TimeUs(0), NoteKind::Tap)
        ]
    );
    let events = autoplayed(&chart);
    let mut judged = Vec::new();
    let mut mines = Vec::new();
    // Each gauge's value before the latest event, and its change by the
    // mine hit.
    let mut value = [20 * GaugeValue::PERCENT, 100 * GaugeValue::PERCENT];
    let mut by_mine = [None, None];
    let mut after_mine = false;
    for e in &events {
        match e.kind {
            JudgeEventKind::Judged { note, judgment, .. } => {
                judged.push((note, judgment, e.time));
                after_mine = false;
            }
            JudgeEventKind::MineHit { note, .. } => {
                mines.push((note, e.time));
                after_mine = true;
            }
            JudgeEventKind::Gauge {
                gauge,
                value: GaugeValue(v),
                ..
            } => {
                if after_mine {
                    by_mine[gauge] = Some(v - value[gauge]);
                }
                value[gauge] = v;
            }
            _ => after_mine = false,
        }
    }
    assert_eq!(
        judged,
        [
            (1, Judgment::Perfect, TimeUs(0)),
            (2, Judgment::Perfect, TimeUs(2))
        ]
    );
    assert_eq!(mines, [(0, TimeUs(0))]);
    // Half a percent a damage unit: 5 % off each gauge, from the normal
    // gauge's 100 % (20 % start plus 80 % for the first of two notes at
    // `#TOTAL` 160) and the survival gauge's capped 100 %.
    let five = Some(-5 * GaugeValue::PERCENT);
    assert_eq!(by_mine, [five, five]);
}
