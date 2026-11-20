//! The render thread's uploader (design.md "Memory Budget", the uploader):
//! every upload made in steps at frame boundaries, outside every frame,
//! shares one step in flight. A step's writes and its submission share one
//! pair of error scopes and a flag-only completion is registered for that
//! submission; no further step is written, by any payload, until a
//! non-blocking poll has seen it. An atlas step is up to
//! [`UPLOAD_STEP`] / [`UPLOAD_WRITE`] writes under one submission; a text,
//! chart-image or BGA step is one write. Within a boundary steps go on while
//! each completion is seen at once and the boundary's [`UPLOAD_TIME_SLICE`]
//! has time left, the first step always; a held pass also stops at its
//! hold's bound, read before each write.
//!
//! A pass that presents nothing waits only for what the uploader needs
//! ([`Renderer::upload_state`](crate::Renderer::upload_state),
//! [`Renderer::wait_step`](crate::Renderer::wait_step)): not at all while
//! a step is ready to write, else for the step in flight's completion,
//! bounded.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::gpu::Gpu;

/// The most bytes one write of a step stages, rows padded as the queue
/// pads them (design.md "Memory Budget"; provisional): a text, chart-image
/// or BGA step is one such write.
pub const UPLOAD_WRITE: u64 = 4 << 20;

/// The most bytes one step stages in flight, every payload's, rows padded:
/// an atlas step is up to eight [`UPLOAD_WRITE`] writes under one
/// submission (design.md "Memory Budget"; provisional: steps of this size,
/// each completed before the next is written, reused the backend's staging
/// block in the measured runs, where twice as much took a new block a
/// step).
pub const UPLOAD_STEP: u64 = 8 * UPLOAD_WRITE;

/// The render thread's time for upload steps at one frame boundary,
/// outside every frame: a write after the boundary's first begins only
/// while the boundary's steps so far took less (design.md "Memory
/// Budget"; a provisional limit, a quarter of a 60 Hz frame).
pub const UPLOAD_TIME_SLICE: Duration = Duration::from_millis(4);

/// What the uploader can do, as a pass that presents nothing reads it
/// before it waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadState {
    /// Nothing in hand, nothing in flight.
    Idle,
    /// A boundary can act at once: no step is in flight (or its completion
    /// is seen) and a payload has a step to write or an answer to give.
    Ready,
    /// A step is in flight, its completion not seen yet.
    InFlight,
}

/// What one boundary's steps came to, per payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uploaded {
    /// The BGA batch's: answered once it is [`BgaStep::Done`](crate::BgaStep::Done).
    pub bga: crate::BgaStep,
    /// The staging whose last step's completion this boundary saw: it
    /// answers ready from now on.
    pub staged: Option<crate::PreparedToken>,
}

/// The step in flight: its completion's flag and its submission.
pub(crate) struct InFlight {
    done: Arc<AtomicBool>,
    index: wgpu::SubmissionIndex,
    /// When it was submitted (a development build's injected completion
    /// delay counts from here).
    #[cfg(feature = "dev-tools")]
    at: std::time::Instant,
}

impl InFlight {
    /// Registers a flag-only completion for every submission made so far,
    /// the step's `index` the last of them.
    pub(crate) fn register(gpu: &Gpu, index: wgpu::SubmissionIndex) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        gpu.queue()
            .on_submitted_work_done(move || flag.store(true, Ordering::Release));
        Self {
            done,
            index,
            #[cfg(feature = "dev-tools")]
            at: std::time::Instant::now(),
        }
    }

    /// Whether its completion was seen; a development build's injected
    /// delay (`delay`) holds it unseen until that long after its submit.
    pub(crate) fn seen(&self, #[cfg(feature = "dev-tools")] delay: Option<Duration>) -> bool {
        #[cfg(feature = "dev-tools")]
        if delay.is_some_and(|d| self.at.elapsed() < d) {
            return false;
        }
        self.done.load(Ordering::Acquire)
    }

    /// Waits for its completion at most `timeout` (wgpu's bounded wait),
    /// never longer; a development build's injected delay is slept out
    /// within the same bound. Whether it was seen.
    ///
    /// # Errors
    ///
    /// The device's error from the poll (a lost device among them).
    pub(crate) fn wait(
        &self,
        gpu: &Gpu,
        timeout: Duration,
        #[cfg(feature = "dev-tools")] delay: Option<Duration>,
    ) -> Result<bool, wgpu::PollError> {
        let start = std::time::Instant::now();
        if !self.done.load(Ordering::Acquire) {
            match gpu.device().poll(wgpu::PollType::Wait {
                submission_index: Some(self.index.clone()),
                timeout: Some(timeout),
            }) {
                Ok(_) | Err(wgpu::PollError::Timeout) => {}
                Err(e) => return Err(e),
            }
        }
        #[cfg(feature = "dev-tools")]
        if let Some(d) = delay {
            let due = d.saturating_sub(self.at.elapsed());
            let left = timeout.saturating_sub(start.elapsed());
            std::thread::sleep(due.min(left));
            return Ok(self.seen(Some(d)));
        }
        let _ = start;
        Ok(self.done.load(Ordering::Acquire))
    }
}
