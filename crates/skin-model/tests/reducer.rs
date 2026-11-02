//! The reducer through `Skin::build`: one script of every effect kind split
//! at every point, long-note feedback, the bomb threshold, `full_combo`,
//! `last_note`, `gauge_max`, `failed` and `combo.up`, with lost effects.

mod play_support;

use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};
use eval::{GaugeState, JudgeEvent, JudgeEventKind};
use play_support::{
    GAUGES, Harness, MS, bool_program, empty_poor, ev, full, gauge, hold_ended, judged, lane,
    ln_held, mine, pct, state, tally,
};
use rules::{GaugeValue, Judgment};
use skin_model::{Grade, LaneTimer, PlayContext, Side, Skin, TIMER_OFF, lane_timer, timer};

const BPM_150: i64 = 150_000;

fn bomb(l: u8) -> u16 {
    lane_timer(LaneTimer::Bomb, lane(l))
}
fn hold(l: u8) -> u16 {
    lane_timer(LaneTimer::Hold, lane(l))
}
fn mine_t(l: u8) -> u16 {
    lane_timer(LaneTimer::Mine, lane(l))
}

#[test]
fn table_slots_are_the_lane_timers() {
    assert_eq!([B1, B2, B3, B5], [bomb(1), bomb(2), bomb(3), bomb(5)]);
    assert_eq!([H2, H5], [hold(2), hold(5)]);
    assert_eq!([M0, M7], [mine_t(0), mine_t(7)]);
}

/// The script's chart: five targets (taps on lanes 1 and 3, long notes on
/// lanes 2, 5 and 6) and a mine on the scratch.
fn script_chart() -> Chart {
    ChartBuilder::new(BPM_150)
        .tap(lane(3), TimeUs(500 * MS))
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
        .mine(lane(0), TimeUs(1_050 * MS), 10)
        .long(lane(5), TimeUs(1_500 * MS), TimeUs(1_800 * MS))
        .long(lane(6), TimeUs(1_500 * MS), TimeUs(1_900 * MS))
        .tap(lane(1), TimeUs(2_000 * MS))
        .build()
        .unwrap()
}

/// Fifteen effects, every kind.
fn script() -> Vec<JudgeEvent> {
    use Judgment::{Bad, Good, Great, Perfect, Poor};
    vec![
        ev(500, judged(3, 0, Perfect, Some(0), None, 1)),
        ev(1_000, ln_held(2, 0, Great)),
        ev(1_050, mine(0, 0)),
        ev(1_050, gauge(0, GaugeValue::FULL)),
        ev(1_100, empty_poor(4)),
        ev(1_250, hold_ended(2, 0)),
        ev(1_250, judged(2, 0, Good, Some(10), Some(-50), 2)),
        ev(1_400, judged(5, 0, Bad, Some(-100), None, 0)),
        ev(1_450, hold_ended(5, 0)),
        ev(1_700, judged(6, 0, Poor, None, None, 0)),
        ev(1_700, gauge(0, pct(98))),
        ev(1_750, mine(7, 4_000)),
        ev(
            2_000,
            JudgeEventKind::PlaySound {
                lane: lane(1),
                sound: SoundId(0),
            },
        ),
        ev(2_000, judged(1, 0, Perfect, Some(0), None, 1)),
        ev(2_500, JudgeEventKind::SongEnd),
    ]
}

// Slots, for the tables below.
const GMAX: u16 = timer::GAUGE_MAX;
const J: u16 = timer::JUDGE;
const UP: u16 = timer::COMBO_UP;
const JP: u16 = Grade::Perfect.timer();
const JGR: u16 = Grade::Great.timer();
const JGO: u16 = Grade::Good.timer();
const JB: u16 = Grade::Bad.timer();
const JPO: u16 = Grade::Poor.timer();
const JE: u16 = Grade::EmptyPoor.timer();
// bomb.N, hold.N, mine.N.
const B1: u16 = 31 + 1;
const B2: u16 = 31 + 2;
const B3: u16 = 31 + 3;
const B5: u16 = 31 + 5;
const H2: u16 = 39 + 2;
const H5: u16 = 39 + 5;
const M0: u16 = 47;
const M7: u16 = 47 + 7;

/// After each prefix of the script: the effect-sourced timers that are on
/// (slot, start ms).
const EXPECTED: [&[(u16, i64)]; 16] = [
    &[],
    &[(J, 500), (JP, 500), (UP, 500), (B3, 500)],
    &[
        (J, 1_000),
        (JGR, 1_000),
        (UP, 500),
        (B3, 500),
        (B2, 1_000),
        (H2, 1_000),
    ],
    &[
        (J, 1_000),
        (JGR, 1_000),
        (UP, 500),
        (B3, 500),
        (B2, 1_000),
        (H2, 1_000),
        (M0, 1_050),
    ],
    &[
        (J, 1_000),
        (JGR, 1_000),
        (UP, 500),
        (B3, 500),
        (B2, 1_000),
        (H2, 1_000),
        (M0, 1_050),
        (GMAX, 1_050),
    ],
    &[
        (J, 1_100),
        (JE, 1_100),
        (UP, 500),
        (B3, 500),
        (B2, 1_000),
        (H2, 1_000),
        (M0, 1_050),
        (GMAX, 1_050),
    ],
    &[
        (J, 1_100),
        (JE, 1_100),
        (UP, 500),
        (B3, 500),
        (B2, 1_000),
        (M0, 1_050),
        (GMAX, 1_050),
    ],
    &[
        (J, 1_250),
        (JGO, 1_250),
        (UP, 1_250),
        (B3, 500),
        (B2, 1_000),
        (M0, 1_050),
        (GMAX, 1_050),
    ],
    &[
        (J, 1_400),
        (JB, 1_400),
        (B3, 500),
        (B2, 1_000),
        (B5, 1_400),
        (H5, 1_400),
        (M0, 1_050),
        (GMAX, 1_050),
    ],
    &[
        (J, 1_400),
        (JB, 1_400),
        (B3, 500),
        (B2, 1_000),
        (B5, 1_400),
        (M0, 1_050),
        (GMAX, 1_050),
    ],
    &[
        (J, 1_700),
        (JPO, 1_700),
        (B3, 500),
        (B2, 1_000),
        (B5, 1_400),
        (M0, 1_050),
        (GMAX, 1_050),
    ],
    &[
        (J, 1_700),
        (JPO, 1_700),
        (B3, 500),
        (B2, 1_000),
        (B5, 1_400),
        (M0, 1_050),
    ],
    &[
        (J, 1_700),
        (JPO, 1_700),
        (B3, 500),
        (B2, 1_000),
        (B5, 1_400),
        (M0, 1_050),
        (M7, 1_750),
    ],
    &[
        (J, 1_700),
        (JPO, 1_700),
        (B3, 500),
        (B2, 1_000),
        (B5, 1_400),
        (M0, 1_050),
        (M7, 1_750),
    ],
    &[
        (J, 2_000),
        (JP, 2_000),
        (UP, 2_000),
        (B1, 2_000),
        (B3, 500),
        (B2, 1_000),
        (B5, 1_400),
        (M0, 1_050),
        (M7, 1_750),
    ],
    &[
        (J, 2_000),
        (JP, 2_000),
        (UP, 2_000),
        (B1, 2_000),
        (B3, 500),
        (B2, 1_000),
        (B5, 1_400),
        (M0, 1_050),
        (M7, 1_750),
    ],
];

/// Every built-in timer after the whole script, in µs, at 2.5 s with a 1 s
/// lead-in: `ready` at −1 s, `play` at 0, `beat` at the first µs whose
/// display position reaches beat 6 (2.4 s at 150 BPM; the map floors each
/// µs's position, so the crossing is 1 µs later), and the reducer's
/// timers.
const FINAL: &[(u16, i64)] = &[
    (timer::READY, -1_000 * MS),
    (timer::SCREEN, -1_000 * MS),
    (timer::PLAY, 0),
    (timer::BEAT, 2_400 * MS + 1),
    (J, 2_000 * MS),
    (JP, 2_000 * MS),
    (UP, 2_000 * MS),
    (B1, 2_000 * MS),
    (B3, 500 * MS),
    (B2, 1_000 * MS),
    (B5, 1_400 * MS),
    (M0, 1_050 * MS),
    (M7, 1_750 * MS),
];

/// Whether a slot is effect-sourced: every built-in but `ready`, `play`,
/// `failed`, `last_note`, `beat` and `screen`.
fn effect_sourced(slot: u16) -> bool {
    !matches!(slot, 0..=3 | 6) && slot != timer::SCREEN
}

#[track_caller]
fn check_prefix(h: &Harness, k: usize, context: &str) {
    let on = EXPECTED[k];
    for slot in (0..timer::BUILTIN).filter(|&s| effect_sourced(s)) {
        let want = on.iter().find(|&&(s, _)| s == slot).map(|&(_, ms)| ms);
        assert_eq!(h.start_ms(slot), want, "slot {slot} after {k}: {context}");
    }
}

/// The skin of the split test: one `when(on(mine.0))` timer.
fn mine_when_skin() -> Skin {
    play_support::load(vec![bool_program("on(mine.0)")], Vec::new()).unwrap()
}

#[test]
fn every_split_of_the_script_reaches_the_same_states() {
    let chart = script_chart();
    let events = script();
    let n = events.len();
    let mut h = Harness::new(mine_when_skin(), &chart, &full(1_000));
    // The snapshot's gauge stays at the maximum, so `gauge_max` shows the
    // reducer's state.
    h.gauges[0] = state(GaugeValue::FULL);
    for mask in 0..1_u32 << (n - 1) {
        h.play = h.skin.prepare(&chart, &full(1_000)).unwrap();
        let mut start = 0;
        let mut now = i64::MIN;
        let mut mine_frame = None;
        for cut in 1..=n {
            if cut < n && mask & (1 << (cut - 1)) == 0 {
                continue;
            }
            let drain = &events[start..cut];
            now = now.max(drain[drain.len() - 1].time.0);
            h.frame_us(now, drain);
            check_prefix(&h, cut, &format!("mask {mask:#x}"));
            if (start..cut).contains(&2) {
                mine_frame = Some(now);
            }
            assert_eq!(h.start(timer::BUILTIN), mine_frame, "mask {mask:#x}");
            // An empty frame changes nothing.
            h.frame_us(now, &[]);
            check_prefix(&h, cut, &format!("mask {mask:#x}, empty frame"));
            start = cut;
        }
        for slot in 0..timer::BUILTIN {
            let want = FINAL.iter().find(|&&(s, _)| s == slot).map(|&(_, us)| us);
            assert_eq!(
                h.start(slot),
                want,
                "slot {slot} at the end, mask {mask:#x}"
            );
        }
        // Lane input is not fed yet.
        for l in 0..8 {
            assert_eq!(h.elapsed(lane_timer(LaneTimer::Press, lane(l))), TIMER_OFF);
            assert_eq!(
                h.elapsed(lane_timer(LaneTimer::Release, lane(l))),
                TIMER_OFF
            );
        }
    }
}

fn ln_chart() -> Chart {
    ChartBuilder::new(BPM_150)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
        .long(lane(2), TimeUs(2_000 * MS), TimeUs(2_300 * MS))
        .tap(lane(3), TimeUs(3_000 * MS))
        .build()
        .unwrap()
}

#[test]
fn a_completed_long_note_adds_its_text_but_no_second_bomb() {
    let mut h = Harness::empty(&ln_chart(), &full(0));
    h.frame(1_000, &[ev(1_000, ln_held(2, 0, Judgment::Great))]);
    assert_eq!(h.start_ms(bomb(2)), Some(1_000));
    assert_eq!(h.start_ms(Grade::Great.timer()), Some(1_000));
    assert_eq!(h.start_ms(timer::JUDGE), Some(1_000));
    assert_eq!(h.start_ms(hold(2)), Some(1_000));
    h.frame(
        1_250,
        &[
            ev(1_250, hold_ended(2, 0)),
            ev(1_250, judged(2, 0, Judgment::Good, Some(0), Some(-50), 1)),
        ],
    );
    assert_eq!(h.start_ms(bomb(2)), Some(1_000));
    assert_eq!(h.start_ms(Grade::Good.timer()), Some(1_250));
    assert_eq!(h.start_ms(Grade::Great.timer()), None);
    assert_eq!(h.start_ms(hold(2)), None);
}

#[test]
fn long_note_heads_judged_at_a_press() {
    use Judgment::{Bad, Poor};
    // A BAD head before the end: a hold and a bomb; `LnHoldEnded` ends it.
    let mut h = Harness::empty(&ln_chart(), &full(0));
    h.frame(900, &[ev(900, judged(2, 0, Bad, Some(-100), None, 0))]);
    assert_eq!(h.start_ms(hold(2)), Some(900));
    assert_eq!(h.start_ms(bomb(2)), Some(900));
    // `LnHoldEnded` for another note leaves it.
    h.frame(950, &[ev(950, hold_ended(2, 1))]);
    assert_eq!(h.start_ms(hold(2)), Some(900));
    h.frame(1_000, &[ev(1_000, hold_ended(2, 0))]);
    assert_eq!(h.start_ms(hold(2)), None);

    // A BAD head pressed at its end: a bomb, no hold.
    let mut h = Harness::empty(&ln_chart(), &full(0));
    h.frame(1_300, &[ev(1_300, judged(2, 0, Bad, Some(300), None, 0))]);
    assert_eq!(h.start_ms(hold(2)), None);
    assert_eq!(h.start_ms(bomb(2)), Some(1_300));

    // A missed head: no bomb, no hold, `judge.poor` on.
    let mut h = Harness::empty(&ln_chart(), &full(0));
    h.frame(1_200, &[ev(1_200, judged(2, 0, Poor, None, None, 0))]);
    assert_eq!(h.start_ms(hold(2)), None);
    assert_eq!(h.start_ms(bomb(2)), None);
    assert_eq!(h.start_ms(Grade::Poor.timer()), Some(1_200));

    // A tap POOR: no bomb. An empty POOR: its grade, no bomb.
    let mut h = Harness::empty(&ln_chart(), &full(0));
    h.frame(3_200, &[ev(3_200, judged(3, 0, Poor, None, None, 0))]);
    assert_eq!(h.start_ms(bomb(3)), None);
    h.frame(3_300, &[ev(3_300, empty_poor(3))]);
    assert_eq!(h.start_ms(bomb(3)), None);
    assert_eq!(h.start_ms(Grade::EmptyPoor.timer()), Some(3_300));
    assert_eq!(h.start_ms(Grade::Poor.timer()), None);
    assert_eq!(h.start_ms(timer::JUDGE), Some(3_300));
}

#[test]
fn a_lost_hold_end_still_ends_the_hold_at_the_long_notes_end() {
    // Held from its head; the `LnHoldEnded` and the judgment are lost.
    let mut h = Harness::empty(&ln_chart(), &full(0));
    h.frame(1_000, &[ev(1_000, ln_held(2, 0, Judgment::Perfect))]);
    h.frame_us(1_300 * MS - 1, &[]);
    assert_eq!(h.start_ms(hold(2)), Some(1_000));
    h.frame(1_300, &[]);
    assert_eq!(h.start_ms(hold(2)), None);
    // A BAD head, its `LnHoldEnded` lost.
    let mut h = Harness::empty(&ln_chart(), &full(0));
    h.frame(
        1_950,
        &[ev(1_950, judged(2, 1, Judgment::Bad, Some(-50), None, 0))],
    );
    h.frame_us(2_300 * MS - 1, &[]);
    assert_eq!(h.start_ms(hold(2)), Some(1_950));
    h.frame(2_300, &[]);
    assert_eq!(h.start_ms(hold(2)), None);
    // A judgment of the held note ends its hold even without the end event.
    let mut h = Harness::empty(&ln_chart(), &full(0));
    h.frame(1_000, &[ev(1_000, ln_held(2, 0, Judgment::Perfect))]);
    h.frame(
        1_100,
        &[ev(
            1_100,
            judged(2, 0, Judgment::Great, Some(0), Some(-200), 1),
        )],
    );
    assert_eq!(h.start_ms(hold(2)), None);
    assert_eq!(h.start_ms(bomb(2)), Some(1_000));
}

/// Three taps: combo 3 is a full combo.
fn taps_chart() -> Chart {
    ChartBuilder::new(BPM_150)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(2_000 * MS))
        .tap(lane(1), TimeUs(3_000 * MS))
        .build()
        .unwrap()
}

#[test]
fn full_combo() {
    use Judgment::{Great, Perfect, Poor};
    let chart = taps_chart();
    let mut h = Harness::empty(&chart, &full(0));
    assert_eq!(h.field(skin_model::field::NOTES), 0);
    h.frame(
        2_000,
        &[
            ev(1_000, judged(1, 0, Perfect, Some(0), None, 1)),
            ev(2_000, judged(1, 1, Great, Some(0), None, 2)),
        ],
    );
    assert_eq!(h.field(skin_model::field::NOTES), 3);
    assert_eq!(h.start_ms(timer::FULL_COMBO), None);
    h.frame(3_000, &[ev(3_000, judged(1, 2, Perfect, Some(0), None, 3))]);
    assert_eq!(h.start_ms(timer::FULL_COMBO), Some(3_000));

    // A break keeps it off.
    let mut h = Harness::empty(&chart, &full(0));
    h.frame(
        3_000,
        &[
            ev(1_000, judged(1, 0, Perfect, Some(0), None, 1)),
            ev(2_000, judged(1, 1, Poor, None, None, 0)),
            ev(3_000, judged(1, 2, Perfect, Some(0), None, 1)),
        ],
    );
    assert_eq!(h.start_ms(timer::FULL_COMBO), None);

    // A snapshot at combo 3 without the effect starts nothing.
    let mut h = Harness::empty(&chart, &full(0));
    h.tally = tally(3);
    h.frame(3_000, &[]);
    assert_eq!(h.field(skin_model::field::COMBO), 3);
    assert_eq!(h.start_ms(timer::FULL_COMBO), None);
}

/// `last_note` is sampled, not effect-sourced: it starts at the first
/// playing frame whose whole ms is past the notes' end and within the
/// finish margin (`play/BMSPlayer.java:683-697`), at that frame's time.
#[test]
fn last_note_is_sampled_past_the_notes_end() {
    use Judgment::Perfect;
    let chart = taps_chart();
    let ctx = PlayContext {
        last_note_end: TimeUs(3_000 * MS),
        bpm_range: None,
        ..full(0)
    };
    // The frame at the end does not start it; 1 ms after does.
    let mut h = Harness::empty(&chart, &ctx);
    h.frame(3_000, &[ev(3_000, judged(1, 2, Perfect, Some(0), None, 3))]);
    assert_eq!(h.start(timer::LAST_NOTE), None);
    // Within the same whole ms of the end: still off.
    h.frame_us(3_000 * MS + 999, &[]);
    assert_eq!(h.start(timer::LAST_NOTE), None);
    h.frame(3_001, &[]);
    assert_eq!(h.start(timer::LAST_NOTE), Some(3_001 * MS));
    // A late miss after the end and `SongEnd` do not move it.
    h.frame(
        3_400,
        &[
            ev(3_300, judged(1, 2, Judgment::Poor, None, None, 0)),
            ev(3_400, JudgeEventKind::SongEnd),
        ],
    );
    assert_eq!(h.start(timer::LAST_NOTE), Some(3_001 * MS));

    // A frame 100 ms late records its own time.
    let mut h = Harness::empty(&chart, &ctx);
    h.frame(2_990, &[]);
    h.frame_us(3_100 * MS + 500, &[]);
    assert_eq!(h.start(timer::LAST_NOTE), Some(3_100 * MS + 500));

    // The last moment before the finish starts it; one past does not.
    let mut h = Harness::empty(&chart, &ctx);
    h.frame_us(8_000 * MS + 999, &[]);
    assert_eq!(h.start(timer::LAST_NOTE), Some(8_000 * MS + 999));
    let mut h = Harness::empty(&chart, &ctx);
    h.frame(8_001, &[]);
    assert_eq!(h.start(timer::LAST_NOTE), None);
    h.frame(9_000, &[]);
    assert_eq!(h.start(timer::LAST_NOTE), None);

    // A jump from before the end to past the finish starts nothing.
    let mut h = Harness::empty(&chart, &ctx);
    h.frame(2_900, &[]);
    h.frame(8_001, &[]);
    assert_eq!(h.start(timer::LAST_NOTE), None);

    // A failed gauge that keeps playing still starts it.
    let mut h = Harness::empty(&chart, &ctx);
    h.gauges[0] = GaugeState {
        value: GaugeValue(0),
        reference: GaugeValue(0),
        failed_at: Some(TimeUs(2_000 * MS)),
    };
    h.frame(2_500, &[]);
    h.frame(3_001, &[]);
    assert_eq!(h.start(timer::LAST_NOTE), Some(3_001 * MS));

    // Frames that are not playing (a stopped play) never start it.
    let mut h = Harness::empty(&chart, &ctx);
    h.playing = false;
    for ms in [3_001, 4_000, 8_000] {
        h.frame(ms, &[]);
    }
    assert_eq!(h.start(timer::LAST_NOTE), None);
    h.playing = true;
    h.frame(8_000, &[]);
    assert_eq!(h.start(timer::LAST_NOTE), Some(8_000 * MS));
}

/// The bomb threshold covers grades from PERFECT through `bomb_up_to`,
/// an empty POOR on its lane last (`play/JudgeManager.java:673`).
#[test]
fn the_bomb_threshold() {
    use Judgment::{Bad, Good, Great, Perfect, Poor};
    let chart = taps_chart();
    let presses = [
        (1_u8, judged(1, 0, Perfect, Some(0), None, 1)),
        (2, judged(2, 0, Great, Some(0), None, 1)),
        (3, judged(3, 0, Good, Some(0), None, 1)),
        (4, judged(4, 0, Bad, Some(0), None, 0)),
        (5, judged(5, 0, Poor, Some(0), None, 0)),
        (6, empty_poor(6)),
    ];
    for (up_to, bombs) in [
        (None, 0),
        (Some(Grade::Perfect), 1),
        (Some(Grade::Great), 2),
        (Some(Grade::Good), 3),
        (Some(Grade::Bad), 4),
        (Some(Grade::Poor), 5),
        (Some(Grade::EmptyPoor), 6),
    ] {
        let skin = Skin::new(skin_model::SkinDef {
            bomb_up_to: up_to,
            ..play_support::def(Vec::new(), play_support::white_sheet(), Vec::new())
        })
        .unwrap();
        let mut h = Harness::new(skin, &chart, &full(0));
        let effects: Vec<_> = presses.iter().map(|&(_, k)| ev(1_000, k)).collect();
        h.frame(1_000, &effects);
        for &(l, _) in &presses {
            let want = (usize::from(l) <= bombs).then_some(1_000);
            assert_eq!(h.start_ms(bomb(l)), want, "lane {l}, up to {up_to:?}");
        }
    }
}

#[test]
fn gauge_max_follows_the_shown_gauge() {
    let chart = taps_chart();
    let mut h = Harness::empty(&chart, &full(0));
    h.gauges[0] = state(GaugeValue::FULL);
    h.frame(1_000, &[ev(1_000, gauge(0, GaugeValue::FULL))]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), Some(1_000));
    // Still at the maximum: the start stays.
    h.frame(1_100, &[ev(1_100, gauge(0, GaugeValue::FULL))]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), Some(1_000));
    // Another gauge's events leave it.
    h.frame(
        1_200,
        &[
            ev(1_200, gauge(1, GaugeValue::FULL)),
            ev(1_300, gauge(1, pct(50))),
        ],
    );
    assert_eq!(h.start_ms(timer::GAUGE_MAX), Some(1_000));
    // Switching the shown gauge reads its own state.
    h.gauge = 1;
    h.frame(1_400, &[]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), None);
    h.gauges[1] = state(GaugeValue::FULL);
    h.frame(1_500, &[ev(1_500, gauge(1, GaugeValue::FULL))]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), Some(1_500));
    h.gauge = 0;
    h.frame(1_600, &[]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), Some(1_000));
    // A drop turns it off.
    h.gauges[0] = state(pct(98));
    h.frame(1_700, &[ev(1_700, gauge(0, pct(98)))]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), None);
}

#[test]
fn gauge_max_is_masked_by_the_snapshot() {
    let chart = taps_chart();
    // The drop's effect is lost and nothing changes the gauge later: the
    // snapshot below the maximum turns it off.
    let mut h = Harness::empty(&chart, &full(0));
    h.gauges[0] = state(GaugeValue::FULL);
    h.frame(1_000, &[ev(1_000, gauge(0, GaugeValue::FULL))]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), Some(1_000));
    h.gauges[0] = state(pct(98));
    h.frame(1_500, &[]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), None);

    // A split drain: the maximum at 1 s delivered with the snapshot below
    // it, then another report at the maximum with the snapshot there: the
    // start stays at 1 s.
    let mut h = Harness::empty(&chart, &full(0));
    h.gauges[0] = state(pct(98));
    h.frame(1_000, &[ev(1_000, gauge(0, GaugeValue::FULL))]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), None);
    h.gauges[0] = state(GaugeValue::FULL);
    h.frame(2_000, &[ev(2_000, gauge(0, GaugeValue::FULL))]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), Some(1_000));
}

#[test]
fn gauge_max_from_an_initial_full_gauge() {
    let gauges = [
        skin_model::GaugeDesc {
            start: GaugeValue::FULL,
            max: GaugeValue::FULL,
            ..GAUGES[0]
        },
        GAUGES[1],
    ];
    let ctx = PlayContext {
        gauges: &gauges,
        ..full(2_000)
    };
    let mut h = Harness::empty(&taps_chart(), &ctx);
    h.gauges[0] = state(GaugeValue::FULL);
    // Off during READY; on from the play's start
    // (`play/BMSPlayer.java:639`).
    h.frame(-1_500, &[]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), None);
    h.frame_us(-1, &[]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), None);
    h.frame(0, &[]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), Some(0));
    h.frame(500, &[]);
    assert_eq!(h.start_ms(timer::GAUGE_MAX), Some(0));
}

#[test]
fn failed_comes_from_the_shown_gauges_fail_time() {
    let chart = taps_chart();
    let mut h = Harness::empty(&chart, &full(0));
    h.gauges[1] = GaugeState {
        value: GaugeValue(0),
        reference: pct(10),
        failed_at: Some(TimeUs(2_000 * MS)),
    };
    h.frame(2_055, &[]);
    assert_eq!(h.start_ms(timer::FAILED), None);
    h.gauge = 1;
    h.frame(2_055, &[]);
    assert_eq!(h.start_ms(timer::FAILED), Some(2_000));
    assert_eq!(h.elapsed(timer::FAILED), 55);
    // A gauge at 0 that has not failed.
    h.gauge = 0;
    h.gauges[0] = state(GaugeValue(0));
    h.frame(2_100, &[]);
    assert_eq!(h.start_ms(timer::FAILED), None);
}

#[test]
fn combo_up_follows_judgments_and_the_empty_poor_policy() {
    use Judgment::{Perfect, Poor};
    let chart = taps_chart();
    for breaks in [false, true] {
        let ctx = PlayContext {
            empty_poor_breaks_combo: breaks,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            ..full(0)
        };
        let mut h = Harness::empty(&chart, &ctx);
        // The snapshot is ahead of the drain: the effect decides.
        h.tally = tally(0);
        h.frame(1_000, &[ev(1_000, judged(1, 0, Perfect, Some(0), None, 1))]);
        assert_eq!(h.start_ms(timer::COMBO_UP), Some(1_000));
        h.frame(1_050, &[]);
        assert_eq!(h.start_ms(timer::COMBO_UP), Some(1_000));
        h.frame(1_100, &[ev(1_100, empty_poor(1))]);
        let after_empty = if breaks { None } else { Some(1_000) };
        assert_eq!(h.start_ms(timer::COMBO_UP), after_empty, "breaks {breaks}");
        h.tally = tally(2);
        h.frame(2_000, &[ev(2_000, judged(1, 1, Perfect, Some(0), None, 2))]);
        assert_eq!(h.start_ms(timer::COMBO_UP), Some(2_000));
        // A break delivered one drain after the snapshot already shows 0.
        h.tally = tally(0);
        h.frame(2_900, &[]);
        assert_eq!(h.start_ms(timer::COMBO_UP), Some(2_000));
        h.frame(3_000, &[ev(3_000, judged(1, 2, Poor, None, None, 0))]);
        assert_eq!(h.start_ms(timer::COMBO_UP), None);
    }
}

/// The evaluator's real stream for one long note of 50 ms on lane 1 with
/// `LnHeld` removed: a completion never bombs nor holds; a head pressed at
/// or after its end bombs without a hold.
#[test]
fn real_streams_without_ln_held() {
    use eval::{Binding, ControlId, DeviceProfile, Evaluator, InputKind, InputStart};
    use eval::{PhysicalInputEvent, SourceId, SourceProfile};

    const HEAD: i64 = 1_000 * MS;
    const END: i64 = HEAD + 50 * MS;
    let chart = ChartBuilder::new(BPM_150)
        .long(lane(1), TimeUs(HEAD), TimeUs(END))
        .build()
        .unwrap();
    let preset = rules::tapestry();
    let device = DeviceProfile {
        sources: vec![SourceProfile {
            input_offset: TimeUs(0),
            bindings: vec![Binding {
                control: ControlId(1),
                lane: lane(1),
            }],
            axes: Vec::new(),
        }],
    };
    let input = |at: i64, value: i32| PhysicalInputEvent {
        received: TimeUs(at),
        source: SourceId(0),
        kind: InputKind::Control {
            control: ControlId(1),
            value,
        },
    };
    let stream = |inputs: &[PhysicalInputEvent]| -> Vec<JudgeEvent> {
        let mut ev =
            Evaluator::new(&chart, &preset, &device, &InputStart::default(), None, 16).unwrap();
        for &i in inputs {
            ev.push_input(i).unwrap();
        }
        let mut out = Vec::new();
        ev.advance_to(TimeUs(END + 2_000 * MS), &mut out).unwrap();
        out
    };
    let ctx = PlayContext {
        gauges: &GAUGES,
        ..full(0)
    };
    // (name, inputs, whether `LnHeld` was in the stream, expected bomb
    // start)
    let cases: [(&str, Vec<PhysicalInputEvent>, bool, Option<i64>); 5] = [
        (
            "released early",
            vec![input(HEAD, 1), input(HEAD + 30 * MS, 0)],
            true,
            None,
        ),
        ("held through", vec![input(HEAD, 1)], true, None),
        ("pressed at end - 1", vec![input(END - 1, 1)], true, None),
        ("pressed at end", vec![input(END, 1)], false, Some(END)),
        (
            "pressed at end + 1",
            vec![input(END + 1, 1)],
            false,
            Some(END + 1),
        ),
    ];
    for (name, inputs, had_held, want_bomb) in cases {
        let full_stream = stream(&inputs);
        let held = full_stream
            .iter()
            .any(|e| matches!(e.kind, JudgeEventKind::LnHeld { .. }));
        assert_eq!(held, had_held, "{name}");
        assert!(
            full_stream
                .iter()
                .any(|e| matches!(e.kind, JudgeEventKind::Judged { .. })),
            "{name}"
        );
        let stripped: Vec<_> = full_stream
            .into_iter()
            .filter(|e| !matches!(e.kind, JudgeEventKind::LnHeld { .. }))
            .collect();
        let gauges = vec![state(preset.gauges[0].start), state(preset.gauges[1].start)];
        let mut h = Harness::empty(&chart, &ctx);
        h.gauges = gauges;
        // One frame per event, at its time, so a hold would show.
        for e in &stripped {
            h.frame_us(e.time.0, std::slice::from_ref(e));
            assert_eq!(h.start(hold(1)), None, "{name}: hold at {}", e.time.0);
        }
        assert_eq!(h.start(bomb(1)), want_bomb, "{name}");
        assert!(h.start(timer::JUDGE).is_some(), "{name}");
    }
}
