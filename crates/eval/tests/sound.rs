//! Hand examples for the key sound of a press (design.md "The Only Output
//! Is the Judgment Event Stream"): the note it judges, else the note its
//! empty POOR is against, else the lane's latest note before it, else the
//! lane's first note. Note `k` of key 1 has sound `k`.

mod common;

use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};
use common::{MS, device, empty_poors, judgments, lane, press, release, run_with};
use eval::{InputStart, JudgeEventKind, PhysicalInputEvent};
use rules::Judgment::{Good, Perfect, Poor};
use rules::RulePreset;

const BPM: i64 = 150_000;
/// The first note's time.
const T: i64 = 1_000 * MS;

/// Taps on key 1 at `times` (µs); tap `k` has sound `k`.
fn taps(times: &[i64]) -> Chart {
    let mut b = ChartBuilder::new(BPM);
    for k in 0..times.len() {
        b = b.sound(&format!("s{k}.wav"));
    }
    for &t in times {
        b = b.tap(lane(1), TimeUs(t));
    }
    let mut chart = b.build().unwrap();
    for (k, note) in chart.lanes[1].iter_mut().enumerate() {
        note.sound = Some(SoundId(u16::try_from(k).unwrap()));
    }
    chart
}

/// Key 1 pressed at each of `times` and released 1 ms later.
fn presses(times: &[i64]) -> Vec<PhysicalInputEvent> {
    times
        .iter()
        .flat_map(|&t| [press(t, 0, 1), release(t + MS, 0, 1)])
        .collect()
}

/// The sounds played, with their times.
fn sounds(chart: &Chart, preset: &RulePreset, times: &[i64]) -> Vec<(i64, u16)> {
    sounds_and_events(chart, preset, times).0
}

fn sounds_and_events(
    chart: &Chart,
    preset: &RulePreset,
    times: &[i64],
) -> (Vec<(i64, u16)>, Vec<eval::JudgeEvent>) {
    let (events, _) = run_with(
        chart,
        preset,
        &device(&[0]),
        &InputStart::default(),
        &presses(times),
        T + 20_000 * MS,
    );
    let played = events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::PlaySound { sound, .. } => Some((e.time.0, sound.0)),
            _ => None,
        })
        .collect();
    (played, events)
}

fn presets() -> [RulePreset; 2] {
    [rules::custom(), rules::beatoraja_style()]
}

#[test]
fn a_judged_press_plays_its_note() {
    // The second press lies 100 ms after note 1 and on note 2. The nearest
    // note rule takes note 2 and passes note 1, which is POOR at the press;
    // combo priority keeps note 1, inside its late GOOD edge (160 ms). Each
    // press plays the note it takes.
    let chart = taps(&[T, T + 1_000 * MS, T + 1_100 * MS]);
    let expected = [(Poor, 2), (Good, 1)];
    for (preset, (passed, taken)) in presets().into_iter().zip(expected) {
        let (played, events) = sounds_and_events(&chart, &preset, &[T, T + 1_100 * MS]);
        assert_eq!(played, [(T, 0), (T + 1_100 * MS, taken)], "{}", preset.id);
        let at_press: Vec<_> = judgments(&events)
            .into_iter()
            .filter(|j| j.1 == 1)
            .map(|j| j.2)
            .collect();
        assert_eq!(at_press, [passed], "{}", preset.id);
    }
}

#[test]
fn an_empty_poor_plays_the_note_it_is_against() {
    let preset = rules::custom();
    // Behind a judged note: 100 ms after note 0, which is judged, with
    // note 1 two seconds later. The empty POOR is against note 0.
    let chart = taps(&[T, T + 2_000 * MS]);
    let (played, events) = sounds_and_events(&chart, &preset, &[T, T + 100 * MS]);
    assert_eq!(played, [(T, 0), (T + 100 * MS, 0)]);
    assert_eq!(empty_poors(&events), 1);

    // Ahead of an unjudged note: 400 ms before note 1, outside its BAD
    // window (180 ms) and inside the empty POOR range (500 ms).
    let (played, events) = sounds_and_events(&chart, &preset, &[T, T + 1_600 * MS]);
    assert_eq!(played, [(T, 0), (T + 1_600 * MS, 1)]);
    assert_eq!(empty_poors(&events), 1);

    // Two judged notes in range, 100 ms behind and ahead of the press: the
    // earlier one on the tie, then the nearer one. Note 1 is judged 140 ms
    // early by the second press.
    let chart = taps(&[T, T + 200 * MS, T + 2_000 * MS]);
    let times = [T + 50 * MS, T + 60 * MS, T + 100 * MS, T + 110 * MS];
    let (played, events) = sounds_and_events(&chart, &preset, &times);
    assert_eq!(
        played,
        [
            (T + 50 * MS, 0),
            (T + 60 * MS, 1),
            (T + 100 * MS, 0),
            (T + 110 * MS, 1)
        ]
    );
    assert_eq!(empty_poors(&events), 2);
}

#[test]
fn a_press_on_nothing_plays_the_latest_earlier_note() {
    let preset = rules::custom();
    let chart = taps(&[T, T + 2_000 * MS, T + 4_000 * MS]);
    let times = [
        // Before the lane's first note: that note's.
        T - 1_000 * MS,
        // Between notes 0 and 1, note 0 unjudged: note 0's.
        T + 1_000 * MS,
        // Note 1, then between notes 1 and 2 with note 1 judged: note 1's.
        T + 2_000 * MS,
        T + 3_000 * MS,
        // After the last note: note 2's, which was missed.
        T + 6_000 * MS,
    ];
    let (played, events) = sounds_and_events(&chart, &preset, &times);
    assert_eq!(
        played,
        [
            (T - 1_000 * MS, 0),
            (T + 1_000 * MS, 0),
            (T + 2_000 * MS, 1),
            (T + 3_000 * MS, 1),
            (T + 6_000 * MS, 2)
        ]
    );
    assert_eq!(empty_poors(&events), 0);

    // Inside the scan envelope: 170 ms after judged note 0, past the
    // empty POOR range (150 ms) and inside the BAD window (180 ms).
    let played = sounds(&chart, &preset, &[T, T + 170 * MS]);
    assert_eq!(played, [(T, 0), (T + 170 * MS, 0)]);
}

#[test]
fn the_fallback_sound_is_the_same_under_every_preset() {
    let chart = taps(&[T, T + 2_000 * MS, T + 4_000 * MS]);
    let times = [
        T - 1_000 * MS,
        T + 1_000 * MS,
        T + 2_000 * MS,
        T + 3_000 * MS,
        T + 6_000 * MS,
    ];
    let [tapestry, beatoraja] = presets();
    let (played, events) = sounds_and_events(&chart, &tapestry, &times);
    // Every press but the one on note 1 judges nothing under both.
    assert_eq!(empty_poors(&events), 0);
    let (other, events) = sounds_and_events(&chart, &beatoraja, &times);
    assert_eq!(empty_poors(&events), 0);
    assert_eq!(
        judgments(&events)
            .iter()
            .filter(|j| j.2 != Poor)
            .collect::<Vec<_>>(),
        [&(1, 1, Perfect, Some(0))]
    );
    assert_eq!(played, other);
    assert_eq!(
        played.iter().map(|p| p.1).collect::<Vec<_>>(),
        [0, 0, 1, 1, 2]
    );
}
