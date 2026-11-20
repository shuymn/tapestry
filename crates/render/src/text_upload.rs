//! Preparing rasterized text, an attempt's or the loading presentation's
//! (design.md "Rendering and Skins"): the lines placed in the text layer's
//! cache ([`crate::text_cache`]), then rasterized and uploaded, one at a
//! time, before READY and outside every frame build.
//!
//! Nothing is written until placement has succeeded. Each new line is
//! rasterized, expanded to premultiplied white `(c, c, c, c)` over its
//! whole returned rectangle with a zero gutter, given mips 1 to 3 by the
//! pages' box filter, and written level by level, all inside one pair of
//! error scopes. After the first write, every exit submits the queued
//! writes inside the scopes, pops them, waits for the GPU, and only then
//! publishes the new lines or releases them: a rectangle is never freed
//! while a write to it may still be pending.
//!
//! The loading text goes through the uploader instead ([`TextUpload`]):
//! placed when it is taken, then written in steps of at most
//! [`UPLOAD_WRITE`](crate::UPLOAD_WRITE) bytes, each in its own pair of
//! scopes and submission, and published or released only once its last
//! step's completion is seen, with no wait.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use skin_model::{GUTTER, MIP_LEVELS, TextRequest, TextTable, downsample_rgba};

use crate::decl::AttemptText;
use crate::gpu::Gpu;
use crate::outline::{LineBounds, MAX_LINE_H, MAX_LINE_W, TextNote};
use crate::renderer::{RenderError, wait_for_gpu};
use crate::skin_atlas::{self, AtlasTexture, UploadError};
use crate::text_cache::{Holder, Placement, TextCache, TextCounts};

/// The tallest rectangle `etagere` returns for a line: [`MAX_LINE_H`] with
/// its gutter, 1 040, on a shelf rounded to 1 088, plus its split
/// threshold.
pub const MAX_RECT_H: u32 = 1_096;
/// The widest rectangle: [`MAX_LINE_W`].
pub const MAX_RECT_W: u32 = MAX_LINE_W;
/// The largest glyph mask a producer renders, live beside the
/// preparation's buffers: a line's pixels at most, 4 080 × 1 024.
pub const MAX_TEXT_MASK_BYTES: u64 = (MAX_LINE_W as u64 - 2 * GUTTER as u64) * MAX_LINE_H as u64;

/// The most bytes the queue stages for one preparation's text on a layer
/// `side` texels square: the whole layer at every level, plus each of 64
/// lines' rows padded to 256 bytes at every level (a rectangle's row is a
/// multiple of 32 bytes at level 0, 16, 8 and 4 below). 52 970 240 bytes at
/// side 2048.
#[must_use]
pub const fn text_upload_bound(side: u32) -> u64 {
    let s = side as u64;
    s * s * 4 * 85 / 64 + 64 * (1_096 * 224 + 548 * 240 + 274 * 248 + 137 * 252)
}

/// What the last preparation of rasterized text did, an attempt's or the
/// loading text's.
#[derive(Clone, Debug, Default)]
pub struct TextReport {
    /// One line per request without an image: its slot and why.
    pub notes: Vec<(u16, TextNote)>,
    /// The placement's counts.
    pub counts: TextCounts,
    /// The bytes the queue staged for its writes, rows padded.
    pub upload_bytes: u64,
    /// The renderer's own submission of its writes, if it wrote any.
    pub submission: Option<wgpu::SubmissionIndex>,
}

/// The renderer's buffers for text preparation, reserved at their bounds
/// on first use and kept: a line's A8 pixels, and a rectangle's RGBA
/// levels.
#[derive(Debug, Default)]
pub(crate) struct TextScratch {
    a8: Vec<u8>,
    rgba: [Vec<u8>; MIP_LEVELS],
    /// The stepped upload and its new line whose levels the RGBA buffers
    /// hold, prepared for a band not yet written: cleared by any other
    /// rasterization.
    holds: Option<(u64, usize)>,
}

impl TextScratch {
    /// The bytes it holds.
    pub(crate) fn bytes(&self) -> u64 {
        let rgba: usize = self.rgba.iter().map(Vec::capacity).sum();
        u64::try_from(self.a8.capacity() + rgba).unwrap_or(u64::MAX)
    }

    /// The producer's glyph mask's bound once a line was rasterized
    /// ([`MAX_TEXT_MASK_BYTES`]), 0 before.
    pub(crate) fn mask_bound(&self) -> u64 {
        if self.a8.capacity() > 0 {
            MAX_TEXT_MASK_BYTES
        } else {
            0
        }
    }

    fn reserve(&mut self) {
        let a8 = usize::try_from(MAX_TEXT_MASK_BYTES).unwrap_or(0);
        let level0 = (MAX_RECT_W as usize) * (MAX_RECT_H as usize) * 4;
        self.a8.reserve_exact(a8.saturating_sub(self.a8.len()));
        for (k, v) in self.rgba.iter_mut().enumerate() {
            v.reserve_exact((level0 >> (2 * k)).saturating_sub(v.len()));
        }
    }
}

/// An ordered step of an upload, as a unit test sees it.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Written,
    Submitted,
    WaitReturned,
    Published,
    RolledBack,
    Abandoned,
    StageReturned,
}

/// A fault a unit test makes the device raise.
#[cfg(test)]
pub(crate) type Fault = fn(&Gpu);

/// What a unit test runs at each step, seeing the cache as it is then.
#[cfg(test)]
pub(crate) type Observe = fn(&Gpu, &TextCache, Step);

/// Unit tests' hooks into an upload.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Hooks {
    /// The steps, in order.
    pub(crate) steps: Vec<Step>,
    /// Run inside the scopes after this many writes.
    pub(crate) fault_after: Option<(usize, Fault)>,
    /// Run at each step.
    pub(crate) observe: Option<Observe>,
    /// The wait's failure, in place of waiting.
    pub(crate) wait: Option<RenderError>,
}

#[cfg(test)]
impl Hooks {
    fn step(&mut self, gpu: &Gpu, cache: &TextCache, step: Step) {
        self.steps.push(step);
        if let Some(observe) = self.observe {
            observe(gpu, cache, step);
        }
    }
}

/// Everything one preparation reads and writes.
pub(crate) struct Prepare<'a> {
    pub(crate) gpu: &'a Gpu,
    pub(crate) atlas: &'a AtlasTexture,
    pub(crate) cache: &'a mut TextCache,
    pub(crate) scratch: &'a mut TextScratch,
    pub(crate) producer: &'a dyn AttemptText,
    /// The producer's bundle's text scope, which keys its lines.
    pub(crate) scope: u64,
    /// The holders whose lines must stay.
    pub(crate) held: &'a [&'a Holder],
    #[cfg(test)]
    pub(crate) hooks: &'a mut Hooks,
    /// Development builds: the wait fails, taking it, as a device failing
    /// would ([`crate::Renderer::fail_next_text_wait`]).
    #[cfg(feature = "dev-tools")]
    pub(crate) wait_fault: &'a mut bool,
}

/// Prepares the rasterized lines of `requests` (in slot order) for a
/// table of `slots` slots: the table, the holder of its lines, and the
/// report.
///
/// # Errors
///
/// The producer's refusal of a line ([`RenderError::Skin`]) or the
/// device's refusal of a write ([`RenderError::Atlas`]): the new lines are
/// released after the GPU has finished, and the lines placed before stay.
/// The device failing while it is waited for ([`RenderError::Poll`], not a
/// refusal, so the renderer is built again as after any device failure):
/// whether the writes finished is unknown, so nothing is released or
/// published, and the cache takes no more lines.
pub(crate) fn prepare(
    p: Prepare<'_>,
    requests: &[TextRequest],
    slots: usize,
) -> Result<(TextTable, Holder, TextReport), RenderError> {
    #[cfg(feature = "dev-tools")]
    let p_began = crate::probe::Span::start();
    #[cfg(feature = "dev-tools")]
    let mut p_raster = std::time::Duration::ZERO;
    #[cfg(feature = "dev-tools")]
    let mut p_write = std::time::Duration::ZERO;
    #[cfg(feature = "dev-tools")]
    let mut p_submit = std::time::Duration::ZERO;
    let side = p.cache.side();
    let bounds = LineBounds::for_side(side);
    let producer = p.producer;
    let placement = p
        .cache
        .place(requests, bounds, p.held, p.scope, |r, em, b| {
            producer.layout(r, em, b)
        });
    let mut report = TextReport {
        counts: placement.counts,
        ..TextReport::default()
    };
    if !placement.new.is_empty() {
        p.scratch.reserve();
    }
    #[cfg(feature = "dev-tools")]
    let p_placed = p_began.elapsed();
    let device = p.gpu.device();
    let texture = p.atlas.texture();
    let layer = p.atlas.text_layer();
    let scratch = &mut *p.scratch;
    let cache = &mut *p.cache;
    #[cfg(test)]
    let hooks = &mut *p.hooks;
    let mut written = 0_usize;
    let mut bytes = 0_u64;
    let ((raster, submission), refused) = skin_atlas::scoped(device, || {
        let mut raster = Ok(());
        for n in &placement.new {
            let Some(z) = layer else {
                raster = Err(RenderError::Atlas(UploadError::Side { side, max: 0 }));
                break;
            };
            #[cfg(feature = "dev-tools")]
            let p_r = crate::probe::Span::start();
            if let Err(e) = rasterize(producer, &n.layout, scratch) {
                raster = Err(RenderError::Skin(e));
                break;
            }
            expand(scratch, n.rect, n.layout.size);
            #[cfg(feature = "dev-tools")]
            let p_w = crate::probe::Span::start();
            #[cfg(feature = "dev-tools")]
            {
                p_raster += p_r.until(p_w);
            }
            for k in 0..MIP_LEVELS {
                let [x, y, w, h] = n.rect.map(|v| v >> k);
                if let Some(level) = scratch.rgba.get(k) {
                    crate::upload::write_unpaced(
                        p.gpu,
                        &crate::upload::Band {
                            texture,
                            layer: z,
                            level: u32::try_from(k).unwrap_or(0),
                            origin: [x, y],
                            size: [w, h],
                            bytes: level,
                        },
                    );
                    bytes += padded(w) * u64::from(h);
                }
                written += 1;
                #[cfg(test)]
                {
                    hooks.step(p.gpu, cache, Step::Written);
                    if let Some((after, fault)) = hooks.fault_after
                        && written == after
                    {
                        fault(p.gpu);
                    }
                }
            }
            #[cfg(feature = "dev-tools")]
            {
                p_write += p_w.elapsed();
            }
        }
        // Inside the scopes: the queued writes reach the device only at a
        // submission.
        #[cfg(feature = "dev-tools")]
        let p_s = crate::probe::Span::start();
        // The array's last use is this submission's, for its retirement.
        let submission = (written > 0).then(|| {
            let (n, index) = p.gpu.submit([]);
            p.atlas.mark(n);
            index
        });
        #[cfg(feature = "dev-tools")]
        {
            p_submit = p_s.elapsed();
        }
        #[cfg(test)]
        if submission.is_some() {
            hooks.step(p.gpu, cache, Step::Submitted);
        }
        (raster, submission)
    });
    #[cfg(feature = "dev-tools")]
    let p_scoped = p_began.elapsed();
    report.upload_bytes = bytes;
    report.submission = submission.clone();
    // A device lost while its writes are queued is not detected here: on
    // the pinned wgpu, a destroyed device drops the writes and the wait
    // still returns, so the lines are published unwritten. A device-lost
    // callback is registered (`Gpu::lost`), but this path does not read
    // it after its wait, a gap left open here. Only a failing wait is
    // treated as the device's failure below.
    #[cfg(feature = "dev-tools")]
    let p_wait = crate::probe::Span::start();
    let waited = if submission.is_some() {
        // A unit test, or a development build, may make the wait fail in
        // place of waiting.
        #[cfg(test)]
        let injected = hooks.wait.take();
        #[cfg(not(test))]
        let injected: Option<RenderError> = None;
        #[cfg(feature = "dev-tools")]
        let injected = injected.or_else(|| {
            std::mem::take(&mut *p.wait_fault)
                .then_some(RenderError::Poll(wgpu::PollError::Timeout))
        });
        let w = match injected {
            Some(e) => Err(e),
            None => wait_for_gpu(p.gpu),
        };
        #[cfg(test)]
        hooks.step(p.gpu, cache, Step::WaitReturned);
        w
    } else {
        Ok(())
    };
    // The wait's end, before any mark: none when every line was cached.
    #[cfg(feature = "dev-tools")]
    let p_waited = if submission.is_some() {
        p_wait.elapsed()
    } else {
        std::time::Duration::ZERO
    };
    #[cfg(feature = "dev-tools")]
    if crate::probe::caller() != crate::probe::OTHER {
        use crate::probe::{cmark, us};
        cmark!("tx", "begin", 0);
        cmark!(
            "tx",
            "requests",
            i64::try_from(requests.len()).unwrap_or(-1)
        );
        cmark!(
            "tx",
            "new_lines",
            i64::try_from(placement.new.len()).unwrap_or(-1)
        );
        cmark!("tx", "bytes", i64::try_from(bytes).unwrap_or(-1));
        cmark!("tx", "place_us", us(p_placed));
        cmark!("tx", "raster_us", us(p_raster));
        cmark!("tx", "write_us", us(p_write));
        cmark!("tx", "submit_us", us(p_submit));
        cmark!(
            "tx",
            "scope_pop_us",
            crate::probe::POP_US.load(std::sync::atomic::Ordering::Relaxed)
        );
        cmark!("tx", "to_scoped_us", us(p_scoped));
        cmark!("tx", "gpu_wait_us", us(p_waited));
        cmark!(
            "tx",
            "max_rect_w",
            placement
                .new
                .iter()
                .map(|n| i64::from(n.rect[2]))
                .max()
                .unwrap_or(0)
        );
    }
    if let Err(e) = waited {
        // The device failed: the writes may still be pending, so their
        // rectangles stay taken and unpublished, and the cache is closed
        // until the renderer is built again with a fresh one.
        cache.abandon();
        #[cfg(test)]
        hooks.step(p.gpu, cache, Step::Abandoned);
        return Err(e);
    }
    let failed = match (raster, refused) {
        (Err(e), _) => Some(e),
        (Ok(()), Some(e)) => Some(RenderError::Atlas(UploadError::Device(e))),
        (Ok(()), None) => None,
    };
    if let Some(e) = failed {
        cache.rollback(&placement);
        #[cfg(test)]
        hooks.step(p.gpu, cache, Step::RolledBack);
        return Err(e);
    }
    let (lines, holder, notes) = cache.publish(placement, slots, p.held);
    #[cfg(test)]
    hooks.step(p.gpu, cache, Step::Published);
    report.notes = notes;
    Ok((TextTable::new(lines), holder, report))
}

/// A preparation of rasterized lines taken by the uploader in steps
/// (design.md "Memory Budget", the uploader; a loading text): placed when
/// it begins, its new lines rasterized and written a step at a time, each
/// step at most [`UPLOAD_WRITE`](crate::UPLOAD_WRITE) bytes as the queue
/// stages them (at least one line) inside one pair of error scopes, and
/// published, or rolled back after a failure, once its last step's
/// completion is seen. Its new lines are unpublished until then, so no
/// other preparation may place in its cache meanwhile: the cache would
/// find them as hits, unwritten.
#[derive(Debug)]
pub(crate) struct TextUpload {
    /// Its number, unique in the process: the scratch names the line it
    /// holds by it.
    id: u64,
    placement: Placement,
    slots: usize,
    /// The next new line to write, and within it the level and the row
    /// its next band begins at.
    next: usize,
    level: usize,
    row: u32,
    report: TextReport,
    /// Why it failed: a line refused by its producer or a step by the
    /// device; nothing more is written.
    failed: Option<RenderError>,
}

impl TextUpload {
    /// Places `requests` (in slot order) for a table of `slots` slots in
    /// `cache`, never writing to or freeing a line a holder of `held`
    /// holds; nothing is written.
    pub(crate) fn begin(
        cache: &mut TextCache,
        scratch: &mut TextScratch,
        producer: &dyn AttemptText,
        scope: u64,
        held: &[&Holder],
        requests: &[TextRequest],
        slots: usize,
    ) -> Self {
        let bounds = LineBounds::for_side(cache.side());
        let placement = cache.place(requests, bounds, held, scope, |r, em, b| {
            producer.layout(r, em, b)
        });
        if !placement.new.is_empty() {
            scratch.reserve();
        }
        static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            id: IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            report: TextReport {
                counts: placement.counts,
                ..TextReport::default()
            },
            placement,
            slots,
            next: 0,
            level: 0,
            row: 0,
            failed: None,
        }
    }

    /// What it has done so far: its placement's counts, and the bytes its
    /// steps staged.
    pub(crate) const fn report(&self) -> &TextReport {
        &self.report
    }

    /// The bytes its placement holds.
    pub(crate) fn bytes(&self) -> u64 {
        self.placement.bytes()
    }

    /// Whether nothing is left to write: every new line written, or it
    /// failed.
    pub(crate) fn written(&self) -> bool {
        self.failed.is_some() || self.next >= self.placement.new.len()
    }

    /// Writes its next step into `atlas`'s text layer: one band, the next
    /// rows of a line's level, at most [`UPLOAD_WRITE`](crate::UPLOAD_WRITE)
    /// bytes as the queue stages them (a level larger than that takes
    /// several), then submitted, inside one pair of error scopes. The line
    /// is rasterized through `producer` into `scratch` first unless the
    /// scratch still holds it from an earlier step; `gate` is read for
    /// `payload` immediately before the write, after that work, and a
    /// band it refuses is kept, prepared, for a later step. The step's
    /// submission, `None` with nothing left to write. A line its producer
    /// refuses, or a write the device refuses, fails it: nothing more is
    /// written, and its lines are rolled back at [`finish`](Self::finish).
    ///
    /// # Errors
    ///
    /// The gate's refusal: nothing written or submitted.
    pub(crate) fn write_step(
        &mut self,
        gpu: &Gpu,
        atlas: &AtlasTexture,
        scratch: &mut TextScratch,
        producer: &dyn AttemptText,
        gate: &mut dyn crate::UploadGate,
        payload: crate::Payload,
    ) -> Result<Option<wgpu::SubmissionIndex>, crate::Refused> {
        let Some(z) = atlas.text_layer() else {
            self.failed = Some(RenderError::Atlas(UploadError::Side {
                side: atlas.side(),
                max: 0,
            }));
            return Ok(None);
        };
        // The next band to write, its line rasterized unless the scratch
        // still holds it.
        let (rect, band) = loop {
            let Some(n) = self.placement.new.get(self.next) else {
                return Ok(None);
            };
            if scratch.holds != Some((self.id, self.next)) {
                if let Err(e) = rasterize(producer, &n.layout, scratch) {
                    self.failed = Some(RenderError::Skin(e));
                    return Ok(None);
                }
                expand(scratch, n.rect, n.layout.size);
                scratch.holds = Some((self.id, self.next));
            }
            let [x, y, w, h] = n.rect.map(|v| v >> self.level);
            let band = band_rows(w, h - self.row.min(h));
            if band > 0 {
                break ([x, y, w, h], band);
            }
            self.advance(band, h);
        };
        let [x, y, w, h] = rect;
        let row_bytes = usize::try_from(u64::from(w) * 4).unwrap_or(0);
        let from = usize::try_from(self.row).unwrap_or(0) * row_bytes;
        let to = from + usize::try_from(band).unwrap_or(0) * row_bytes;
        let Some(rows) = scratch.rgba.get(self.level).and_then(|l| l.get(from..to)) else {
            self.advance(band, h);
            return Ok(None);
        };
        let level = u32::try_from(self.level).unwrap_or(0);
        let row = self.row;
        let (written, refused) = skin_atlas::scoped(gpu.device(), || {
            let staged = crate::upload::write(
                gpu,
                gate,
                payload,
                &crate::upload::Band {
                    texture: atlas.texture(),
                    layer: z,
                    level,
                    origin: [x, y + row],
                    size: [w, band],
                    bytes: rows,
                },
            )?;
            // Inside the scopes: the queued write reaches the device only
            // at a submission, the array's last use for its retirement.
            let (n, index) = gpu.submit([]);
            atlas.mark(n);
            Ok((staged, index))
        });
        let (staged, index) = written?;
        self.report.upload_bytes += staged;
        self.report.submission = Some(index.clone());
        self.advance(band, h);
        if let Some(e) = refused {
            self.failed = Some(RenderError::Atlas(UploadError::Device(e)));
        }
        Ok(Some(index))
    }

    /// Moves past `band` rows of the current level, `h` rows high: to the
    /// next level, and the next line, once one is written.
    fn advance(&mut self, band: u32, h: u32) {
        self.row += band;
        if self.row >= h {
            self.row = 0;
            self.level += 1;
            if self.level >= MIP_LEVELS {
                self.level = 0;
                self.next += 1;
            }
        }
    }

    /// Settles it once its last step's completion was seen: its table, the
    /// holder of its lines and its report, its lines published in `cache`
    /// (never touching a line a holder of `held` holds).
    ///
    /// # Errors
    ///
    /// Why it failed ([`write_step`](Self::write_step)): its new lines are
    /// rolled back, and the lines placed before stay.
    pub(crate) fn finish(
        self,
        cache: &mut TextCache,
        held: &[&Holder],
    ) -> Result<(TextTable, Holder, TextReport), RenderError> {
        if let Some(e) = self.failed {
            cache.rollback(&self.placement);
            return Err(e);
        }
        let mut report = self.report;
        let (lines, holder, notes) = cache.publish(self.placement, self.slots, held);
        report.notes = notes;
        Ok((TextTable::new(lines), holder, report))
    }

    /// Drops it unpublished: its new lines are released in `cache`. A step
    /// still in flight writes them before any later write reuses them.
    pub(crate) fn abort(self, cache: &mut TextCache) {
        cache.rollback(&self.placement);
    }
}

/// One line's RGBA levels for a fixed place of exactly its padded size
/// (a decision label's row, [`crate::decision_labels`]): `line`
/// rasterized into `scratch`'s A8 buffer, expanded as [`prepare`] expands
/// a line into a rectangle of `pad(size)` (each side with its gutters,
/// rounded up to the mips' alignment), with mips 1 to 3. Returns that
/// size and the levels, taken from the scratch, which keeps its A8 buffer
/// for the next line. Preparation work: it allocates.
///
/// # Errors
///
/// The producer's refusal of the line.
pub(crate) fn line_levels(
    producer: &dyn AttemptText,
    line: &crate::outline::LineLayout,
    scratch: &mut TextScratch,
) -> Result<([u32; 2], [Vec<u8>; MIP_LEVELS]), crate::SkinError> {
    rasterize(producer, line, scratch)?;
    let size = line.size.map(padded_side);
    let [w, h] = size;
    expand(scratch, [0, 0, w, h], line.size);
    Ok((size, std::mem::take(&mut scratch.rgba)))
}

/// A line's side `v` with its gutters, rounded up to the mips'
/// alignment: the side of its rectangle.
pub(crate) const fn padded_side(v: u32) -> u32 {
    let align = skin_model::ALIGN;
    (v + 2 * GUTTER).div_ceil(align) * align
}

/// The A8 buffer's bytes: a decision worker's scratch, counted in its own
/// term ([`crate::decision_labels`]).
pub(crate) fn a8_bytes(scratch: &TextScratch) -> u64 {
    u64::try_from(scratch.a8.capacity()).unwrap_or(u64::MAX)
}

/// Rasterizes `line` into the scratch's A8 buffer, zeroed first.
fn rasterize(
    producer: &dyn AttemptText,
    line: &crate::outline::LineLayout,
    scratch: &mut TextScratch,
) -> Result<(), crate::SkinError> {
    let [w, h] = line.size;
    let n = usize::try_from(u64::from(w) * u64::from(h)).unwrap_or(usize::MAX);
    scratch.holds = None;
    scratch.a8.clear();
    scratch.a8.resize(n, 0);
    producer.raster(line, &mut scratch.a8)
}

/// Expands the scratch's A8 line of `size` into its RGBA levels over the
/// returned rectangle `rect`: premultiplied white, the line `GUTTER`
/// texels in, every other texel zero; then levels 1 to 3.
fn expand(scratch: &mut TextScratch, rect: [u32; 4], [w, h]: [u32; 2]) {
    let [_, _, rw, rh] = rect;
    let row = usize::try_from(rw).unwrap_or(0) * 4;
    let Some((level0, rest)) = scratch.rgba.split_first_mut() else {
        return;
    };
    level0.clear();
    level0.resize(row * usize::try_from(rh).unwrap_or(0), 0);
    let (g, lw) = (
        usize::try_from(GUTTER).unwrap_or(0),
        usize::try_from(w).unwrap_or(0),
    );
    for (y, src) in scratch
        .a8
        .chunks_exact(lw.max(1))
        .take(usize::try_from(h).unwrap_or(0))
        .enumerate()
    {
        let start = (g + y) * row + g * 4;
        let Some(dst) = level0.get_mut(start..start + lw * 4) else {
            continue;
        };
        for (t, &c) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src) {
            *t = [c; 4];
        }
    }
    let mut prev: &Vec<u8> = level0;
    let mut pw = rw;
    for (k, next) in rest.iter_mut().enumerate() {
        let (nw, nh) = (rw >> (k + 1), rh >> (k + 1));
        next.clear();
        next.resize(
            usize::try_from(u64::from(nw) * u64::from(nh) * 4).unwrap_or(0),
            0,
        );
        downsample_rgba(prev, pw, next);
        prev = next;
        pw = nw;
    }
}

/// The rows of a level `w` texels wide one band writes, of `left` rows
/// left: as many as fit [`UPLOAD_WRITE`](crate::UPLOAD_WRITE) padded
/// bytes, at least one.
pub(crate) fn band_rows(w: u32, left: u32) -> u32 {
    let per = (crate::UPLOAD_WRITE / padded(w).max(1)).max(1);
    left.min(u32::try_from(per).unwrap_or(u32::MAX))
}

/// A row of `w` RGBA texels padded to `COPY_BYTES_PER_ROW_ALIGNMENT`, as
/// the queue stages it.
pub(crate) fn padded(w: u32) -> u64 {
    let row = u64::from(w) * 4;
    let align = u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    row.div_ceil(align) * align
}
