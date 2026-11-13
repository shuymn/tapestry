//! A rearmed practice round's restore is planned again in its storage
//! (design.md "Play Lifecycle", the start gate): `Restorer::replan`
//! allocates and frees nothing however often a round is rearmed, and gives
//! the plan `Restorer::plan` makes afresh. This file holds a single test,
//! so the harness's other threads are not tracked.

mod common;
mod counting;

use audio::{AudioEngine, MAX_VOICES, SimConfig, SimDevice};
use bms_model::{ChartBuilder, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, bank, lane};
use game::Restorer;

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

#[test]
fn replanning_a_restore_allocates_nothing_and_matches_a_fresh_plan() {
    // A BGM sound every 50 ms for 20 s, over four sounds.
    let mut b = ChartBuilder::new(150_000)
        .sound("a.wav")
        .sound("b.wav")
        .sound("c.wav")
        .sound("d.wav")
        .tap(lane(1), TimeUs(15_000 * MS));
    for k in 0..400_i64 {
        b = b.bgm(TimeUs(k * 50 * MS), SoundId(u16::try_from(k % 4).unwrap()));
    }
    let chart = b.build().unwrap();
    let mut engine =
        AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let mut out = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let (_, mut cmds) = rtrb::RingBuffer::new(1);
    for _ in 0..3 {
        engine.step(&mut out, &mut cmds).unwrap();
    }
    let restorer = Restorer::new(
        engine.seek_source(),
        chart.bgm.iter().map(|&e| e.into()).collect(),
    );
    let line = |origin: i64| engine.frame_line_at(TimeUs(origin * MS));
    let mut restore = restorer.plan(line(100), TimeUs(8_000 * MS)).unwrap();

    counting::track(true);
    counting::self_check();
    let before = counting::calls();
    for k in 1..=60_i64 {
        restorer
            .replan(
                &mut restore,
                line(100 + k * 37),
                TimeUs((8_000 + k * 13) * MS),
            )
            .unwrap();
    }
    let calls = counting::calls() - before;
    counting::track(false);
    assert_eq!(calls, 0);
    let fresh = restorer
        .plan(line(100 + 60 * 37), TimeUs((8_000 + 60 * 13) * MS))
        .unwrap();
    assert_eq!(format!("{restore:?}"), format!("{fresh:?}"));
}
