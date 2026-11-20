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

/// A payload of the uploader's, as its gate is asked about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Payload {
    /// A staging's array or text, the attempt's or a result bundle's.
    Staging,
    /// The loading text of `lifetime` at `revision`.
    LoadingText { lifetime: u64, revision: u64 },
    /// The attempt's chart-image publication.
    Chart,
    /// A BGA batch.
    Bga,
    /// The result pages upload.
    Pages,
}

/// Why a write was not made ([`UploadGate::admit_write`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The boundary begins nothing more now: the payload keeps its band
    /// for a later step.
    Past,
    /// The payload is no longer its owner's current one: it ends, nothing
    /// more written.
    Stale,
}

/// What a boundary's uploader asks before each heavy operation it begins
/// and immediately before each write (design.md "Memory Budget", the
/// uploader's admission gate): its owner answers for the frame grant or
/// the clock deadline, READY's cutoff and the hold's bound, and for the
/// currency of the payload in hand, which only its owner can tell. A
/// closure is a gate that answers [`past`](Self::past) alone.
pub trait UploadGate {
    /// Whether the boundary must begin nothing more now: a frame is due
    /// (its grant found or its deadline passed), READY's cutoff holds, or
    /// a held pass reached the hold's bound. A write begun before ends
    /// first.
    fn past(&mut self) -> bool;
    /// Whether `payload` is still its owner's current one, read live:
    /// before each write and before its publication or installation.
    /// One superseded is dropped, never published or installed.
    fn current(&mut self, payload: Payload) -> bool {
        let _ = payload;
        true
    }
    /// Whether a write of `payload` is made now, asked by the uploader's
    /// one write function immediately before the write, after any work
    /// that prepares its band: its currency, then [`past`](Self::past).
    ///
    /// # Errors
    ///
    /// [`Refused::Stale`] for a payload no longer current,
    /// [`Refused::Past`] when the boundary begins nothing more.
    fn admit_write(&mut self, payload: Payload) -> Result<(), Refused> {
        if !self.current(payload) {
            return Err(Refused::Stale);
        }
        if self.past() {
            return Err(Refused::Past);
        }
        Ok(())
    }
}

impl<F: FnMut() -> bool> UploadGate for F {
    fn past(&mut self) -> bool {
        self()
    }
}

/// One write of texels into a texture's layer and level: `size` texels,
/// rows tightly packed in `bytes`, at `origin`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Band<'a> {
    pub(crate) texture: &'a wgpu::Texture,
    pub(crate) layer: u32,
    pub(crate) level: u32,
    pub(crate) origin: [u32; 2],
    pub(crate) size: [u32; 2],
    pub(crate) bytes: &'a [u8],
}

impl Band<'_> {
    /// The bytes the queue stages for it, rows padded to
    /// `COPY_BYTES_PER_ROW_ALIGNMENT`.
    pub(crate) fn staged(&self) -> u64 {
        let [w, h] = self.size;
        crate::text_upload::padded(w) * u64::from(h)
    }
}

/// The uploader's write (design.md "Memory Budget", the uploader): `band`
/// queued for `payload` only when `gate` admits it, read immediately
/// before the write ([`UploadGate::admit_write`]); a refused band is not
/// written and its payload keeps it. The bytes the queue staged.
///
/// # Errors
///
/// The gate's refusal: nothing written.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
pub(crate) fn write(
    gpu: &Gpu,
    gate: &mut dyn UploadGate,
    payload: Payload,
    band: &Band<'_>,
) -> Result<u64, Refused> {
    gate.admit_write(payload)?;
    put(gpu, band);
    Ok(band.staged())
}

/// A write that is no uploader payload's, made at once (design.md "Memory
/// Budget", the uploader: a renderer built again restoring its
/// installations, an attempt's text prepared in one call, a chart set
/// published whole, the decision labels' fill, a selection pool write,
/// the built-in atlas and the placeholder). Only the functions
/// `cargo xtask check-arch` lists call it.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
pub(crate) fn write_unpaced(gpu: &Gpu, band: &Band<'_>) {
    put(gpu, band);
}

/// A frame's instance data, written at once into `buffer` at `offset`
/// (no uploader payload's: the frame's own buffers). Only the functions
/// `cargo xtask check-arch` lists call it.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
pub(crate) fn write_buffer_unpaced(gpu: &Gpu, buffer: &wgpu::Buffer, offset: u64, bytes: &[u8]) {
    gpu.count_write();
    gpu.queue().write_buffer(buffer, offset, bytes);
}

/// Development builds: a validation error raised in the error scopes
/// open, as a device refusing a step's write would raise one (a texture of
/// no texels).
#[cfg(feature = "dev-tools")]
pub(crate) fn refuse(gpu: &Gpu) {
    let _ = gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("refused"),
        size: wgpu::Extent3d {
            width: 0,
            height: 0,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
}

/// The one call that queues texels: every texture write the renderer
/// makes, counted.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn put(gpu: &Gpu, band: &Band<'_>) {
    let [x, y] = band.origin;
    let [w, h] = band.size;
    gpu.count_write();
    gpu.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: band.texture,
            mip_level: band.level,
            origin: wgpu::Origin3d {
                x,
                y,
                z: band.layer,
            },
            aspect: wgpu::TextureAspect::All,
        },
        band.bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
}

/// The attempt's chart-image publication a boundary steps: the payload in
/// hand and its images, which its caller keeps.
pub type ChartBands<'a> = (
    &'a mut crate::ChartUpload,
    [Option<&'a skin_model::chart_image::Fitted>; 3],
);

/// Development builds: what one loading lifetime's text did on a renderer
/// (design.md "Rendering and Skins"): its preparations by cause (the
/// lifetime's first, another target, key or revision), its installs and
/// its drops.
#[cfg(feature = "dev-tools")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextCount {
    pub first: u32,
    pub target: u32,
    pub key: u32,
    pub revision: u32,
    pub installs: u32,
    pub drops: u32,
}

/// What one boundary's steps came to, per payload.
#[derive(Debug)]
pub struct Uploaded {
    /// The BGA batch's: answered once it is [`BgaStep::Done`](crate::BgaStep::Done).
    pub bga: crate::BgaStep,
    /// The staging's end in this boundary, its one completion event: its
    /// last step's completion seen (it answers ready from now on) or a
    /// step refused (its request answers the refusal once asked again).
    pub staged: Option<StagingEnd>,
    /// The loading text in hand's end in this boundary: installed at its
    /// last step's completion, or why not.
    pub text: Option<crate::TextDone>,
    /// The chart-image publication's end in this boundary: its table,
    /// published at the completion that passed its checks, or why it
    /// ended (a check failed, nothing more written; a band refused, the
    /// all-absent table published).
    pub chart: Option<Result<skin_model::chart_image::ChartTable, crate::RenderError>>,
    /// The result pages upload's end in this boundary: written (its owner
    /// publishes or withdraws it), refused, or withdrawn by the renderer.
    pub pages: Option<crate::PagesStep>,
}

/// How a staging ended in a boundary ([`Uploaded::staged`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagingEnd {
    /// Its last step's completion was seen: the token it answers ready
    /// with.
    Ready(crate::PreparedToken),
    /// A step of the staging of `request` was refused: it is retired, and
    /// its request answers the refusal once asked again.
    Refused(u64),
}

/// The step in flight: its completion's flag and its submission, and what
/// it staged, for whom.
pub(crate) struct InFlight {
    done: Arc<AtomicBool>,
    index: wgpu::SubmissionIndex,
    /// The bytes its writes staged in the queue, rows padded, and the
    /// payload they were for.
    staged: (u64, Payload),
    /// When it was submitted (a development build's injected completion
    /// delay counts from here).
    #[cfg(feature = "dev-tools")]
    at: std::time::Instant,
}

impl InFlight {
    /// Registers a flag-only completion for every submission made so far,
    /// the step's `index` the last of them, its writes having staged
    /// `staged` bytes for `payload`.
    pub(crate) fn register(
        gpu: &Gpu,
        index: wgpu::SubmissionIndex,
        staged: u64,
        payload: Payload,
    ) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        gpu.queue()
            .on_submitted_work_done(move || flag.store(true, Ordering::Release));
        Self {
            done,
            index,
            staged: (staged, payload),
            #[cfg(feature = "dev-tools")]
            at: std::time::Instant::now(),
        }
    }

    /// The bytes its writes staged for `payload` while its completion is
    /// not seen; 0 once it is, or for another payload's step.
    pub(crate) fn staged_for(&self, payload: Payload) -> u64 {
        let (bytes, of) = self.staged;
        if of == payload && !self.done.load(Ordering::Acquire) {
            bytes
        } else {
            0
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
