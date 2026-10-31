//! The 1× autoplay schedule as an iterator allocates nothing, however long
//! the chart: the loudness analysis walks it in memory independent of the
//! chart's duration (design.md "Algorithms"). The counting allocator sums
//! every tracked thread, so this file holds a single test.

mod counting;

use bms_model::{ChartBuilder, Lane, SoundId, TimeUs};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

#[test]
fn schedule_iter_allocates_nothing() {
    let mut b = ChartBuilder::new(150_000).sound("bgm.wav").sound("key.wav");
    for k in 0..2_000 {
        b = b.bgm(TimeUs(k * 100_000), SoundId(0));
        b = b.tap(
            Lane::new(1 + (k % 7) as u8).unwrap(),
            TimeUs(k * 100_000 + 50_000),
        );
    }
    let mut chart = b.build().unwrap();
    for note in chart.lanes.iter_mut().flatten() {
        note.sound = Some(SoundId(1));
    }

    counting::track(true);
    counting::self_check();
    let before = counting::calls();
    let mut n = 0_usize;
    let mut last = TimeUs(i64::MIN);
    for event in game::schedule_iter(&chart) {
        assert!(event.time >= last);
        last = event.time;
        n += 1;
    }
    let calls = counting::calls() - before;
    counting::track(false);

    assert_eq!(n, 4_000);
    assert_eq!(calls, 0);
}
