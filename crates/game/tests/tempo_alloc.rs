//! The tempo provider's allocations (design.md "Play Adjustments", tempo;
//! "Scope of No Allocation"): preparing the references allocates MAIN's
//! histogram once and frees it before returning, a histogram over its
//! limit is refused before anything is allocated, and the reducer's
//! lookups through the provider in play allocate nothing. This file holds
//! a single test, so the harness's other threads are not tracked.

mod counting;

use bms_model::{ChartBuilder, Lane, TimeUs};
use game::adjustment::{
    Adjustment, ControlRoles, Event, Head, Observation, TempoMap, presentation_link,
};
use game::{AttemptTempo, PlayOptions};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

#[test]
fn the_provider_allocates_only_its_histogram_outside_play() {
    let lane = Lane::new(1).unwrap();
    // Three runs of rows by tempo: 96 bytes of bins.
    let chart = ChartBuilder::new(150_000)
        .tap(lane, TimeUs(100_000))
        .bpm(TimeUs(1_000_000), 300_000)
        .tap(lane, TimeUs(1_100_000))
        .bpm(TimeUs(2_000_000), 150_000)
        .tap(lane, TimeUs(2_100_000))
        .build()
        .unwrap();
    let options = PlayOptions::default();
    let mut a = Adjustment::new(options, ControlRoles::default(), TimeUs(-1_000_000), 0);
    let (mut w, mut r) = presentation_link(&a.snapshot());

    counting::track(true);
    counting::self_check();
    let before = counting::calls();
    let refused = AttemptTempo::prepare_within(&chart, None, 95);
    let refused_calls = counting::calls() - before;
    let before = counting::calls();
    let tempo = AttemptTempo::prepare_within(&chart, None, 96);
    let prepared_calls = counting::calls() - before;
    let before = counting::calls();
    let script = [
        Event::Prepare {
            references: tempo.references().unwrap(),
            speed: tempo.speed(),
        },
        Event::Phase {
            head: Head::Clock {
                at: TimeUs(0),
                chart: TimeUs(1_500_000),
            },
            generation: 1,
        },
        Event::DisplayOffset(600_000),
        Event::Tick,
    ];
    for (seq, event) in script.into_iter().enumerate() {
        let o = Observation {
            time: TimeUs(seq as i64),
            seq: seq as u64,
            event,
        };
        a.drain([o], &tempo, &mut w);
        let _ = r.read();
    }
    // The lookup itself, before, inside and after the 300 BPM part.
    for at in [-1, 1_000_000, 2_000_000, i64::MAX] {
        assert!(std::hint::black_box(&tempo).bpm_at(TimeUs(at)).is_some());
    }
    let play_calls = counting::calls() - before;
    counting::track(false);

    assert!(refused.references().is_err());
    assert_eq!(refused_calls, 0, "a refused histogram was allocated");
    // One allocation and its free: nothing outlives preparation.
    assert_eq!(prepared_calls, 2);
    assert!(tempo.references().is_ok());
    assert_eq!(play_calls, 0, "the reducer's lookups allocated");
    assert!(r.read().hispeed.is_some());
}
