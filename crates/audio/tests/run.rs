//! The audio thread loop: it steps until stopped, and a stop requested
//! while the output waits ends it after that step.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use audio::{
    AudioEngine, AudioOutput, OutputError, RunReport, SimConfig, SimDevice, Sound, SoundBank,
    SoundCmd, Wake, run_audio,
};
use bms_model::TimeUs;
use rtrb::RingBuffer;

const RATE: u32 = 48_000;
const PERIOD: u32 = 480;
/// A loop that ignores its stop fails here, and one that ignores a failure
/// stops here, instead of running forever.
const BOUND: u32 = 100;

/// A simulated device that sets `stop` during its `stop_at`th wait and
/// fails the write of its `fail_at`th.
struct Probe<'a> {
    inner: SimDevice,
    stop: &'a AtomicBool,
    waits: u32,
    stop_at: u32,
    fail_at: u32,
}

impl AudioOutput for Probe<'_> {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }

    fn period(&self) -> u32 {
        self.inner.period()
    }

    fn wait(&mut self) -> Wake {
        self.waits += 1;
        if self.waits == self.stop_at {
            self.stop.store(true, Ordering::Release);
        }
        self.inner.wait()
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        if self.waits == self.fail_at {
            return Err(OutputError::DeviceLost);
        }
        self.inner.write(f)
    }
}

fn run(stop_at: u32, fail_at: u32, stopped: bool) -> (Result<RunReport, OutputError>, u32) {
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![Sound::mono(vec![1_000; 64])],
    ));
    let mut engine = AudioEngine::new(bank, &[], RATE, PERIOD, 4, 4).unwrap();
    let (_tx, mut rx) = RingBuffer::<SoundCmd>::new(4);
    let stop = AtomicBool::new(stopped);
    let mut out = Probe {
        inner: SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        stop: &stop,
        waits: 0,
        stop_at,
        fail_at,
    };
    let r = run_audio(&mut out, &mut engine, &mut rx, &stop);
    (r, out.waits)
}

#[test]
fn a_set_stop_ends_the_loop_before_any_wait() {
    let (r, waits) = run(u32::MAX, BOUND, true);
    assert_eq!(r, Ok(RunReport::default()));
    assert_eq!(waits, 0);
}

#[test]
fn a_stop_requested_during_a_wait_ends_the_loop_after_that_step() {
    let (r, waits) = run(5, BOUND, false);
    // Two queued periods: the first step fills both, each later one tops
    // up one (by hand: 960 + 4 × 480).
    assert_eq!(
        r,
        Ok(RunReport {
            steps: 5,
            frames: 2_880
        })
    );
    assert_eq!(waits, 5);
}

#[test]
fn an_output_failure_ends_the_loop_with_it() {
    let (r, waits) = run(BOUND, 3, false);
    assert_eq!(r, Err(OutputError::DeviceLost));
    assert_eq!(waits, 3);
}
