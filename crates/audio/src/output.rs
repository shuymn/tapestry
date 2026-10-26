//! The output boundary: a device the engine waits on and writes to.

use bms_model::TimeUs;

use crate::CHANNELS;

/// What a wake reports about the device buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wake {
    /// The first frame still queued (frames written minus padding). The
    /// next engine pass, one period after the wake, reads it.
    pub queued_from: u64,
    /// When the wake happened.
    pub at: TimeUs,
    /// Frames written but not yet read by the engine.
    pub padding: u32,
    /// The wake only asks for the queue to be filled before the stream
    /// starts: `queued_from` and `at` describe no engine pass, so the engine
    /// takes no timing from it and fills the queue with silence.
    pub priming: bool,
}

/// A device failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OutputError {
    /// The device went away.
    #[error("the audio device went away")]
    DeviceLost,
    /// No buffer event came within the output's timeout.
    #[error("the audio device stopped signaling buffer events")]
    Stalled,
    /// Any other device error, with the platform's code.
    #[error("the audio device failed ({0:#010x})")]
    Failed(i32),
}

/// The most periods an output keeps queued ahead of playback. Outputs size
/// their buffers for it when they are made, so changing the queue between
/// plays allocates nothing.
pub const MAX_QUEUE_PERIODS: u32 = 3;

/// An audio device in shared, event-driven operation.
pub trait AudioOutput {
    /// Frames per second.
    fn rate(&self) -> u32;
    /// Frames per engine pass.
    fn period(&self) -> u32;
    /// Waits for the next buffer event. The first wake comes before
    /// anything is written, so the engine can fill the queue; an output
    /// that cannot time it marks it [`Wake::priming`].
    fn wait(&mut self) -> Wake;
    /// Calls `f` with the interleaved stereo frames to fill next, as many
    /// as the queue has room for (possibly none).
    ///
    /// # Errors
    ///
    /// The device's failure; nothing was written.
    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError>;
    /// Periods kept queued ahead of playback, when the output can say.
    fn queue_periods(&self) -> Option<u32> {
        None
    }
    /// Keeps `periods` periods queued from the next write on, as many as
    /// the output holds and at least one, and returns the periods it now
    /// keeps; `None` for an output whose queue is fixed. A deeper queue is
    /// filled at the next write, a shallower one drains as the device reads
    /// it. Call it between plays: a change moves where key sounds start.
    fn set_queue_periods(&mut self, periods: u32) -> Option<u32> {
        let _ = periods;
        None
    }
}

/// An output that keeps a copy of everything written to the output it wraps,
/// up to a capacity fixed at construction (design.md "Mechanisms for
/// Testing"). Recording allocates nothing.
#[derive(Clone, Debug)]
pub struct RecordingOutput<O> {
    inner: O,
    samples: Vec<f32>,
    dropped: u64,
}

impl<O: AudioOutput> RecordingOutput<O> {
    /// Wraps `inner`, recording at most `capacity` frames.
    #[must_use]
    pub fn new(inner: O, capacity: usize) -> Self {
        Self {
            inner,
            samples: Vec::with_capacity(capacity * CHANNELS),
            dropped: 0,
        }
    }

    /// The recorded interleaved stereo samples, frame 0 first.
    #[must_use]
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Frames written beyond the capacity, not recorded.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The wrapped output.
    #[must_use]
    pub fn inner(&self) -> &O {
        &self.inner
    }
}

impl<O: AudioOutput> AudioOutput for RecordingOutput<O> {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }

    fn period(&self) -> u32 {
        self.inner.period()
    }

    fn wait(&mut self) -> Wake {
        self.inner.wait()
    }

    fn queue_periods(&self) -> Option<u32> {
        self.inner.queue_periods()
    }

    fn set_queue_periods(&mut self, periods: u32) -> Option<u32> {
        self.inner.set_queue_periods(periods)
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        let samples = &mut self.samples;
        let dropped = &mut self.dropped;
        self.inner.write(|buf| {
            f(buf);
            let room = samples.capacity() - samples.len();
            let keep = buf.len().min(room);
            samples.extend_from_slice(&buf[..keep]);
            *dropped += ((buf.len() - keep) / CHANNELS) as u64;
        })
    }
}
