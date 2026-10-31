//! The sound-only recording of a headless run with song selection
//! (dev-tools). Its `write` runs on the audio thread, so it is in play
//! (design.md "Scope of No Allocation"): it allocates nothing and uses
//! checked access.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use audio::{AudioOutput, OutputError, SimDevice, Wake};
use bms_model::TimeUs;
use game::Wakes;

/// A recording of a simulated device that keeps every period that sounds
/// and the first silent period of each silence. Recording allocates
/// nothing.
pub struct SoundRecording {
    inner: SimDevice,
    samples: Vec<f32>,
    /// The last period kept was silent.
    quiet: bool,
}

impl SoundRecording {
    /// Records `inner`, keeping at most `frames` frames.
    pub fn new(inner: SimDevice, frames: usize) -> Self {
        Self {
            inner,
            samples: Vec::with_capacity(frames * audio::CHANNELS),
            quiet: false,
        }
    }

    /// The interleaved stereo samples kept.
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }
}

impl AudioOutput for SoundRecording {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }

    fn period(&self) -> u32 {
        self.inner.period()
    }

    fn wait(&mut self) -> Wake {
        self.inner.wait()
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        let samples = &mut self.samples;
        let quiet = &mut self.quiet;
        self.inner.write(|buf| {
            f(buf);
            let silent = buf.iter().all(|&x| x == 0.0);
            if silent && *quiet {
                return;
            }
            *quiet = silent;
            let room = samples.capacity() - samples.len();
            if let Some(kept) = buf.get(..buf.len().min(room)) {
                samples.extend_from_slice(kept);
            }
        })
    }

    fn queue_periods(&self) -> Option<u32> {
        self.inner.queue_periods()
    }

    fn set_queue_periods(&mut self, periods: u32) -> Option<u32> {
        self.inner.set_queue_periods(periods)
    }
}

impl Wakes for SoundRecording {
    fn next_wake(&self) -> TimeUs {
        self.inner.next_wake()
    }
}
