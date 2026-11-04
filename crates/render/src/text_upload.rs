//! Preparing an attempt's rasterized text (design.md "Rendering and
//! Skins"): the lines placed in the text layer's cache
//! ([`crate::text_cache`]), then rasterized and uploaded, one at a time,
//! before READY and outside every frame build.
//!
//! Nothing is written until placement has succeeded. Each new line is
//! rasterized, expanded to premultiplied white `(c, c, c, c)` over its
//! whole returned rectangle with a zero gutter, given mips 1 to 3 by the
//! pages' box filter, and written level by level, all inside one pair of
//! error scopes. After the first write, every exit submits the queued
//! writes inside the scopes, pops them, waits for the GPU, and only then
//! publishes the new lines or releases them: a rectangle is never freed
//! while a write to it may still be pending.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use skin_model::{ChartText, GUTTER, Layout, MIP_LEVELS, Skin, TextTable, downsample_rgba};

use crate::decl::AttemptText;
use crate::gpu::Gpu;
use crate::outline::{LineBounds, MAX_LINE_H, MAX_LINE_W, TextNote};
use crate::renderer::{RenderError, wait_for_gpu};
use crate::skin_atlas::{self, AtlasTexture, UploadError};
use crate::text_cache::{Holder, TextCache, TextCounts};

/// The tallest rectangle `etagere` returns for a line: [`MAX_LINE_H`] with
/// its gutter, 1 040, on a shelf rounded to 1 088, plus its split
/// threshold.
pub const MAX_RECT_H: u32 = 1_096;
/// The widest rectangle: [`MAX_LINE_W`].
pub const MAX_RECT_W: u32 = MAX_LINE_W;

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

/// What the last preparation of an attempt's text did.
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
}

impl TextScratch {
    /// The bytes it holds.
    pub(crate) fn bytes(&self) -> u64 {
        let rgba: usize = self.rgba.iter().map(Vec::capacity).sum();
        u64::try_from(self.a8.capacity() + rgba).unwrap_or(u64::MAX)
    }

    fn reserve(&mut self) {
        let a8 = (MAX_LINE_W as usize) * (MAX_LINE_H as usize);
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
    StageReturned,
}

/// A fault a unit test makes the device raise.
#[cfg(test)]
pub(crate) type Fault = fn(&Gpu);

/// Unit tests' hooks into an upload.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Hooks {
    /// The steps, in order.
    pub(crate) steps: Vec<Step>,
    /// Run inside the scopes after this many writes.
    pub(crate) fault_after: Option<(usize, Fault)>,
}

/// Everything one preparation reads and writes.
pub(crate) struct Prepare<'a> {
    pub(crate) gpu: &'a Gpu,
    pub(crate) atlas: &'a AtlasTexture,
    pub(crate) cache: &'a mut TextCache,
    pub(crate) scratch: &'a mut TextScratch,
    pub(crate) producer: &'a dyn AttemptText,
    /// The holders whose lines must stay.
    pub(crate) held: &'a [&'a Holder],
    #[cfg(test)]
    pub(crate) hooks: &'a mut Hooks,
}

/// Prepares `chart`'s rasterized lines of `skin` on a `target`-pixel
/// target: the table, the holder of its lines, and the report.
///
/// # Errors
///
/// The producer's refusal of a line ([`RenderError::Skin`]), the device's
/// refusal of a write ([`RenderError::Atlas`]) or its failure waiting: the
/// new lines are released after the GPU has finished, and the lines placed
/// before stay.
pub(crate) fn prepare(
    p: Prepare<'_>,
    skin: &Skin,
    chart: &ChartText<'_>,
    target: [u32; 2],
) -> Result<(TextTable, Holder, TextReport), RenderError> {
    let requests = skin.text_requests(chart, target, &Layout::AUTHOR);
    let side = p.cache.side();
    let bounds = LineBounds::for_side(side);
    let producer = p.producer;
    let placement = p.cache.place(&requests, bounds, p.held, |r, em, b| {
        producer.layout(r, em, b)
    });
    let mut report = TextReport {
        counts: placement.counts,
        ..TextReport::default()
    };
    if !placement.new.is_empty() {
        p.scratch.reserve();
    }
    let device = p.gpu.device();
    let queue = p.gpu.queue();
    let texture = p.atlas.texture();
    let layer = p.atlas.text_layer();
    let scratch = &mut *p.scratch;
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
            if let Err(e) = rasterize(producer, &n.layout, scratch) {
                raster = Err(RenderError::Skin(e));
                break;
            }
            expand(scratch, n.rect, n.layout.size);
            for k in 0..MIP_LEVELS {
                let [x, y, w, h] = n.rect.map(|v| v >> k);
                if let Some(level) = scratch.rgba.get(k) {
                    write(queue, texture, z, k, [x, y, w, h], level);
                    bytes += padded(w) * u64::from(h);
                }
                written += 1;
                #[cfg(test)]
                {
                    hooks.steps.push(Step::Written);
                    if let Some((after, fault)) = hooks.fault_after
                        && written == after
                    {
                        fault(p.gpu);
                    }
                }
            }
        }
        // Inside the scopes: the queued writes reach the device only at a
        // submission.
        let submission = (written > 0).then(|| queue.submit([]));
        #[cfg(test)]
        if submission.is_some() {
            hooks.steps.push(Step::Submitted);
        }
        (raster, submission)
    });
    report.upload_bytes = bytes;
    report.submission = submission.clone();
    let waited = if submission.is_some() {
        let w = wait_for_gpu(p.gpu);
        #[cfg(test)]
        hooks.steps.push(Step::WaitReturned);
        w
    } else {
        Ok(())
    };
    let failed = match (raster, refused, waited) {
        (Err(e), _, _) => Some(e),
        (Ok(()), Some(e), _) => Some(RenderError::Atlas(UploadError::Device(e))),
        (Ok(()), None, Err(e)) => Some(e),
        (Ok(()), None, Ok(())) => None,
    };
    if let Some(e) = failed {
        p.cache.rollback(&placement);
        #[cfg(test)]
        hooks.steps.push(Step::RolledBack);
        return Err(e);
    }
    let (lines, holder, notes) = p.cache.publish(placement, skin.text_slots(), p.held);
    #[cfg(test)]
    hooks.steps.push(Step::Published);
    report.notes = notes;
    Ok((TextTable::new(lines), holder, report))
}

/// Rasterizes `line` into the scratch's A8 buffer, zeroed first.
fn rasterize(
    producer: &dyn AttemptText,
    line: &crate::outline::LineLayout,
    scratch: &mut TextScratch,
) -> Result<(), crate::SkinError> {
    let [w, h] = line.size;
    let n = usize::try_from(u64::from(w) * u64::from(h)).unwrap_or(usize::MAX);
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

/// A row of `w` RGBA texels padded to `COPY_BYTES_PER_ROW_ALIGNMENT`, as
/// the queue stages it.
fn padded(w: u32) -> u64 {
    let row = u64::from(w) * 4;
    let align = u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    row.div_ceil(align) * align
}

/// Writes `bytes`, a `w` × `h` RGBA rectangle, at `(x, y)` of `layer`'s
/// level `level`.
fn write(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    layer: u32,
    level: usize,
    [x, y, w, h]: [u32; 4],
    bytes: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: u32::try_from(level).unwrap_or(0),
            origin: wgpu::Origin3d { x, y, z: layer },
            aspect: wgpu::TextureAspect::All,
        },
        bytes,
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
