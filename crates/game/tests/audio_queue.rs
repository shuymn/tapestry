//! The audio output's queue between plays (stage1.md D12): a change asked
//! for between plays is applied at the audio thread's next step and
//! answered, each play's underruns are counted from its chart time 0 until
//! it ends, and a play with any raises the next play's queue to three
//! periods, never lowering it.

mod common;

use std::time::{Duration, Instant};

use audio::{
    AudioEngine, AudioOutput, MAX_QUEUE_PERIODS, MAX_VOICES, OutputError, SimConfig, SimDevice,
    Wake,
};
use bms_model::{ChartBuilder, TimeUs};
use common::{MS, PERIOD, RATE, bank, lane};
use game::{AudioControl, AudioWaitError, AudioWorker, QueueChoice, audio_control, audio_link_at};

fn pair() -> (AudioControl, AudioWorker) {
    let chart = ChartBuilder::new(150_000)
        .sound("key.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let engine = AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    audio_control(engine)
}

fn steady() -> SimDevice {
    SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)))
}

/// An output whose queue is fixed: it cannot say what it keeps.
struct Fixed(SimDevice);

impl AudioOutput for Fixed {
    fn rate(&self) -> u32 {
        self.0.rate()
    }

    fn period(&self) -> u32 {
        self.0.period()
    }

    fn wait(&mut self) -> Wake {
        self.0.wait()
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        self.0.write(f)
    }
}

/// An output whose device went away.
struct Lost;

impl AudioOutput for Lost {
    fn rate(&self) -> u32 {
        RATE
    }

    fn period(&self) -> u32 {
        PERIOD
    }

    fn wait(&mut self) -> Wake {
        Wake {
            queued_from: 0,
            at: TimeUs(0),
            padding: 0,
            priming: true,
        }
    }

    fn write(&mut self, _: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        Err(OutputError::DeviceLost)
    }
}

#[test]
fn a_queue_change_applies_at_the_next_step_and_is_answered() {
    let (mut control, mut worker) = pair();
    let mut out = steady();
    for _ in 0..3 {
        worker.step(&mut out).unwrap();
    }
    let before = worker.engine().written();
    control.request_queued_periods(3);
    assert_eq!(control.queued_periods(), None, "answered before a step");
    assert_eq!(out.queue_periods(), Some(2));
    worker.step(&mut out).unwrap();
    assert_eq!(control.queued_periods(), Some(Some(3)));
    assert_eq!(out.queue_periods(), Some(3));
    // That step refilled the period the device read and added one; each
    // later step refills one.
    assert_eq!(worker.engine().written() - before, 2 * u64::from(PERIOD));
    worker.step(&mut out).unwrap();
    assert_eq!(worker.engine().written() - before, 3 * u64::from(PERIOD));
    // More than any output holds: it keeps the most it can. The same
    // number asked again is a new change, answered again.
    control.request_queued_periods(7);
    worker.step(&mut out).unwrap();
    assert_eq!(control.queued_periods(), Some(Some(MAX_QUEUE_PERIODS)));
    control.request_queued_periods(2);
    worker.step(&mut out).unwrap();
    assert_eq!(control.queued_periods(), Some(Some(2)));
    control.request_queued_periods(2);
    assert_eq!(control.queued_periods(), None);
    worker.step(&mut out).unwrap();
    assert_eq!(control.queued_periods(), Some(Some(2)));
    // An output with a fixed queue answers that it cannot say.
    let (mut control, mut worker) = pair();
    let mut fixed = Fixed(steady());
    control.request_queued_periods(3);
    worker.step(&mut fixed).unwrap();
    assert_eq!(control.queued_periods(), Some(None));
}

#[test]
fn underruns_count_from_chart_time_zero_until_the_play_ends() {
    let (mut control, mut worker) = pair();
    // A missed wake leaves the next one an empty buffer: underruns at the
    // wakes of passes 6 (70 ms), 31 (320 ms) and 61 (620 ms).
    let mut out = SimDevice::new(SimConfig {
        missed_wakes: vec![5, 30, 60],
        ..SimConfig::steady(RATE, PERIOD, TimeUs(0))
    });
    for _ in 0..2 {
        worker.step(&mut out).unwrap();
    }
    let counters = control.counters();
    // Chart time 0 at 200 ms: the underrun at 70 ms is in the countdown.
    let (_sink, queue) = audio_link_at(4, TimeUs(200 * MS));
    control.play(TimeUs(200 * MS), queue).unwrap();
    let first = control.generation();
    while worker.engine().written() < 45 * u64::from(PERIOD) {
        worker.step(&mut out).unwrap();
    }
    assert_eq!(counters.underruns(), 2);
    assert_eq!(counters.steady_underruns(first), 1);
    // After the play is cancelled, underruns no longer count for it.
    control.cancel();
    while worker.engine().written() < 75 * u64::from(PERIOD) {
        worker.step(&mut out).unwrap();
    }
    assert_eq!(counters.underruns(), 3);
    assert_eq!(counters.steady_underruns(first), 1);
    assert_eq!(counters.steady_underruns(first + 1), 0);
    // The next play starts from none, and the last one's count is gone.
    let (_sink, queue) = audio_link_at(4, TimeUs(800 * MS));
    control.play(TimeUs(800 * MS), queue).unwrap();
    worker.step(&mut out).unwrap();
    assert_eq!(control.generation(), first + 1);
    assert_eq!(counters.steady_underruns(first + 1), 0);
    assert_eq!(counters.steady_underruns(first), 0);
}

#[test]
fn a_failed_output_is_not_asked_and_a_lost_worker_is_an_error() {
    let (mut control, mut worker) = pair();
    assert_eq!(worker.step(&mut Lost), Err(OutputError::DeviceLost));
    let start = Instant::now();
    assert_eq!(control.set_queued_periods(3), Ok(None));
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(worker);
    assert_eq!(control.set_queued_periods(3), Err(AudioWaitError::Gone));
    // A worker that never steps does not answer.
    let (mut control, _worker) = pair();
    assert_eq!(control.set_queued_periods(3), Err(AudioWaitError::Timeout));
}

/// The state × event table of the queue choice: underruns after the
/// countdown raise it from 2 to 3 whatever ended the play, once; none, or
/// a choice already at 3, leaves it.
#[test]
fn the_queue_rises_once_on_steady_underruns_and_never_falls() {
    assert_eq!(
        (game::DEFAULT_QUEUED_PERIODS, game::RAISED_QUEUED_PERIODS),
        (2, 3)
    );
    let mut q = QueueChoice::new(2);
    assert!(!q.play_ended(0));
    assert_eq!(q.periods(), 2);
    assert!(q.play_ended(1));
    assert_eq!(q.periods(), 3);
    assert!(!q.play_ended(5));
    assert!(!q.play_ended(0));
    assert_eq!(q.periods(), 3);
}
