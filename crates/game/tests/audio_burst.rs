//! A burst of plays started while the audio thread is held: the second
//! start must wait for the first switch to be taken, so when the audio
//! thread runs again every retired command queue comes back to the game
//! thread to be dropped there, none is leaked, and the audio thread
//! neither allocates nor frees. Only the audio thread is counted (see
//! `counting`); this file holds a single test.

mod common;
mod counting;

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice};
use bms_model::{ChartBuilder, TimeUs};
use common::{MS, PERIOD, RATE, bank, lane};
use game::{audio_control, audio_link_at};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

const PLAYS: usize = 10;

/// Sends `PLAYS` starts from a game thread while the audio thread is held
/// for 100 ms, then lets it run. With `keep`, the test keeps each play's
/// sink and returns how many saw their queue dropped; without, the sinks
/// go at once, as a finished play's logic step drops them, so the queue's
/// last owner frees it, and a free on the audio thread is counted.
fn burst(keep: bool) -> usize {
    let chart = ChartBuilder::new(150_000)
        .sound("s.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let engine = AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let (mut control, mut worker) = audio_control(engine);
    let mut out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        0,
    );
    let held = AtomicBool::new(true);
    let stop = AtomicBool::new(false);
    let sinks = thread::scope(|s| {
        let audio = s.spawn(|| {
            counting::track(true);
            while held.load(Ordering::Acquire) {
                thread::yield_now();
            }
            while !stop.load(Ordering::Acquire) {
                worker.step(&mut out).unwrap();
                thread::yield_now();
            }
            counting::track(false);
            worker
        });
        let game = s.spawn(|| {
            let mut sinks = Vec::with_capacity(PLAYS);
            for play in 0..PLAYS {
                let origin = TimeUs(i64::try_from(play).unwrap() * 10 * MS);
                let (sink, queue) = audio_link_at(4, origin);
                if keep {
                    sinks.push(sink);
                }
                control.play(origin, queue).unwrap();
            }
            control.ready().unwrap();
            (sinks, control)
        });
        // Every start but the first waits for the held audio thread.
        thread::sleep(Duration::from_millis(100));
        held.store(false, Ordering::Release);
        let (sinks, control) = game.join().unwrap();
        stop.store(true, Ordering::Release);
        drop(audio.join().unwrap());
        drop(control);
        sinks
    });
    sinks.iter().filter(|s| s.is_abandoned()).count()
}

#[test]
fn a_burst_of_plays_returns_every_queue_without_audio_side_frees() {
    // Every queue came back (the last one with the worker, dropped after
    // it): none was leaked.
    assert_eq!(burst(true), PLAYS);
    assert_eq!(counting::calls(), 0);
    // Queues whose last owner is the retired end are freed on the game
    // thread, never the audio thread.
    assert_eq!(burst(false), 0);
    assert_eq!(counting::calls(), 0);
}
