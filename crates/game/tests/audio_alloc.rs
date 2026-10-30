//! The audio thread neither allocates nor frees across retries and loads:
//! each play's command queue, and each bank a load replaces, goes back to
//! the game thread to be dropped there, and the control messages travel
//! through a preallocated queue. Only the audio loop's steps are counted
//! (see `counting`); this file holds a single test. Profiles and the
//! positive control: design.md "Scope of No Allocation".

mod common;
mod counting;

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice};
use bms_model::{ChartBuilder, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, bank, lane};
use eval::{JudgeEvent, JudgeEventKind};
use game::{EventSink, audio_control, audio_link_at};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

#[test]
fn audio_loop_neither_allocates_nor_frees_across_retries() {
    let chart = ChartBuilder::new(150_000)
        .sound("s.wav")
        .bgm(TimeUs(0), SoundId(0))
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let engine = AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let (mut control, mut worker) = audio_control(engine);
    let mut out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        0,
    );
    let mut steps = |worker: &mut game::AudioWorker, n: usize| {
        counting::track(true);
        counting::self_check();
        for _ in 0..n {
            worker.step(&mut out).unwrap();
        }
        counting::track(false);
    };
    steps(&mut worker, 5);
    // The first play and 3 retries, each with key sounds; the second is
    // cancelled and the last sealed.
    for play in 0..4_i64 {
        let origin = TimeUs((100 + play * 400) * MS);
        // Between plays the queue deepens to three periods and back.
        control.request_queued_periods(if play % 2 == 0 { 3 } else { 2 });
        // Before the third play another chart.s sounds are loaded: the
        // audio thread swaps them in and hands the old ones back.
        if play == 2 {
            // First one that times out while the audio thread stalls: the
            // thread takes it late, in a counted step, and its answer is
            // freed by the next load, on the game thread.
            assert_eq!(
                control.load(bank(&chart), chart.bgm.clone()),
                Err(game::AudioLoadError::Wait(game::AudioWaitError::Timeout))
            );
            steps(&mut worker, 1);
            let next = bank(&chart);
            let schedule = chart.bgm.clone();
            std::thread::scope(|s| {
                let load = s.spawn(|| control.load(next, schedule));
                while !load.is_finished() {
                    steps(&mut worker, 1);
                }
                load.join().unwrap().unwrap();
            });
        }
        let (mut sink, queue) = audio_link_at(64, origin);
        control.play(origin, queue).unwrap();
        for k in 0..3 {
            sink.receive(&JudgeEvent {
                seq: k,
                time: TimeUs(0),
                origin: Some(TimeUs(i64::try_from(k).unwrap() * 20 * MS)),
                kind: JudgeEventKind::PlaySound {
                    lane: lane(1),
                    sound: SoundId(0),
                },
            });
        }
        match play {
            1 => control.cancel(),
            3 => control.seal(),
            _ => {}
        }
        steps(&mut worker, 40);
    }
    assert!(control.done());
    assert_eq!(control.queued_periods(), Some(Some(2)));
    assert_eq!(counting::calls(), 0);
}
