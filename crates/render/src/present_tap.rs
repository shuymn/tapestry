//! Development builds: copies of a window's frames as they are presented,
//! the engine's own screenshot (docs/test-device.md "Run Guard", shots).
//! A window frame the tap wants is copied from its swap chain image in the
//! frame's own command buffer, after every pass drew into it and before it
//! is presented, so the copy holds exactly the pixels handed to the
//! present; nothing else (another window, the compositor, an older frame)
//! can be in it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What a window renderer asks of a tap ([`crate::Renderer::set_present_tap`]).
pub trait PresentTap: Send + Sync {
    /// At each window frame acquired, after its passes are encoded: whether
    /// to copy this frame. `selection` says whether it is a frame of the
    /// selection screen: only those are copied (a copy allocates its
    /// readback buffer and pixels, which play's frames never do, design.md
    /// "Scope of No Allocation"), so a tap refuses a request any
    /// other frame meets, without allocating.
    fn wanted(&self, selection: bool) -> bool;
    /// The copied frame was handed to the present: the swap chain's present
    /// count read from the same chain right before and right after it
    /// (`None` when a read failed). A present that did not advance the
    /// count presented nothing.
    fn presented(&self, before: Option<u32>, after: Option<u32>);
    /// The copied frame's pixels, or why they could not be read.
    fn deliver(&self, frame: Result<PresentFrame, String>);
}

/// A presented frame's pixels: RGBA8 rows from the top, whatever the swap
/// chain's own channel order, alpha as the frame left it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A window frame's present on the host counter (raw QPC ticks, the
/// clock a system present trace stamps), read right before and right
/// after the call that presents it, and the swap chain it was presented
/// on: a run record joins its presents with that trace's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentStamps {
    /// The counter right before the present call.
    pub pre_qpc: i64,
    /// The counter right after it returned.
    pub post_qpc: i64,
    /// The swap chain's interface address.
    pub swap_chain: usize,
}

/// The host counter now, raw QPC ticks.
#[must_use]
pub fn qpc_now() -> i64 {
    let mut ticks = 0;
    // SAFETY: writes one i64; the counter cannot fail on Windows XP and
    // later.
    let _ = unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&raw mut ticks) };
    ticks
}

/// How long a copy may take to be readable once its frame was submitted.
const COPY_WAIT: Duration = Duration::from_secs(2);

/// A copy of a frame's swap chain image, recorded in the frame's command
/// buffer.
pub(crate) struct Copy {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    padded: u32,
    bgra: bool,
}

/// Whether `format` is one a copy can be read from, and whether its channels
/// are in BGRA order.
fn order(format: wgpu::TextureFormat) -> Option<bool> {
    match format {
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => Some(true),
        wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb => Some(false),
        _ => None,
    }
}

/// Records in `encoder` the copy of `texture` (`width`×`height`, `format`)
/// into a new readable buffer.
///
/// # Errors
///
/// A format a copy is not read from.
pub(crate) fn record(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    format: wgpu::TextureFormat,
) -> Result<Copy, String> {
    let bgra = order(format).ok_or_else(|| format!("a swap chain of format {format:?}"))?;
    let (width, height) = (texture.width(), texture.height());
    let padded = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("present copy"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    Ok(Copy {
        buffer,
        width,
        height,
        padded,
        bgra,
    })
}

/// Reads `copy` back once the submission `submitted` that recorded it is
/// done, within [`COPY_WAIT`].
///
/// # Errors
///
/// The wait timed out or failed, or the buffer did not map.
pub(crate) fn read(
    device: &wgpu::Device,
    copy: &Copy,
    submitted: wgpu::SubmissionIndex,
) -> Result<PresentFrame, String> {
    let slice = copy.buffer.slice(..);
    let mapped: Arc<Mutex<Option<Result<(), wgpu::BufferAsyncError>>>> = Arc::default();
    let into = Arc::clone(&mapped);
    slice.map_async(wgpu::MapMode::Read, move |r| {
        if let Ok(mut m) = into.lock() {
            *m = Some(r);
        }
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submitted),
            timeout: Some(COPY_WAIT),
        })
        .map_err(|e| format!("the copy's submission: {e}"))?;
    let done = mapped
        .lock()
        .map_err(|_| "the copy's map callback panicked".to_owned())?
        .take();
    match done {
        Some(Ok(())) => {}
        Some(Err(e)) => return Err(format!("the copy did not map: {e}")),
        None => return Err(format!("the copy was not mapped within {COPY_WAIT:?}")),
    }
    let data = slice
        .get_mapped_range()
        .map_err(|e| format!("the copy's mapped range: {e}"))?;
    let row = copy.width as usize * 4;
    let mut rgba = Vec::with_capacity(row * copy.height as usize);
    for line in data.chunks(copy.padded as usize).take(copy.height as usize) {
        let line = line.get(..row).ok_or("a short row in the copy")?;
        if copy.bgra {
            for &[b, g, r, a] in line.as_chunks::<4>().0 {
                rgba.extend_from_slice(&[r, g, b, a]);
            }
        } else {
            rgba.extend_from_slice(line);
        }
    }
    drop(data);
    copy.buffer.unmap();
    if rgba.len() != row * copy.height as usize {
        return Err("the copy holds fewer rows than its frame".into());
    }
    Ok(PresentFrame {
        width: copy.width,
        height: copy.height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only 8-bit RGBA and BGRA swap chains are read, each in its own order.
    #[test]
    fn a_copy_reads_only_eight_bit_rgba_or_bgra() {
        assert_eq!(order(wgpu::TextureFormat::Bgra8Unorm), Some(true));
        assert_eq!(order(wgpu::TextureFormat::Bgra8UnormSrgb), Some(true));
        assert_eq!(order(wgpu::TextureFormat::Rgba8Unorm), Some(false));
        assert_eq!(order(wgpu::TextureFormat::Rgba16Float), None);
    }
}
