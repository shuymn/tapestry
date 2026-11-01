//! The sprite pipelines' atlas: one `Rgba8Unorm` texture array whose layers
//! all share one power-of-two side. Layer 0 is reserved for the built-in
//! atlas (the bitmap font and the solid block, [`crate::atlas`]) at its top
//! left; a skin's pages follow it, each with its four supplied mip levels
//! (docs/stage3.md R7). Without a skin's pages the array is the built-in
//! atlas alone, one 128-texel layer.
//!
//! Uploading is preparation work, never a frame's: the pages are checked
//! against the device's limits and against themselves before anything is
//! allocated on the GPU ([`check_pages`]), and the renderer builds the
//! whole replacement inside error scopes ([`scoped`]), so what the device
//! still refuses is a typed error and the atlas in use stays untouched.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use skin_model::{AtlasLimits, MAX_ATLAS_TEXELS, MIP_LEVELS};

use crate::atlas::{ATLAS_SIZE, build_atlas};
use crate::gpu::Gpu;

/// The layers before a skin's pages: the built-in atlas's.
pub const RESERVED_LAYERS: u32 = 1;

/// A page's mip levels, level 0 first: level `k` is `side >> k` texels
/// square, RGBA8, premultiplied, row by row (`skin_model::Page::level`).
pub type PageLevels<'a> = [&'a [u8]; MIP_LEVELS];

/// A skin's atlas pages to upload, every page `side` texels square: page
/// `i` becomes layer `RESERVED_LAYERS + i`, as the skin's uv table has it.
#[derive(Clone, Copy, Debug)]
pub struct AtlasPages<'a> {
    /// Every page's side at level 0.
    pub side: u32,
    /// The pages, in layer order.
    pub pages: &'a [PageLevels<'a>],
}

/// Why a skin's atlas was not uploaded. The atlas in use stays.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// The side is not a power of two at least the built-in atlas's, or
    /// is past the device's largest texture.
    #[error("the page side {side} is not a power of two from {ATLAS_SIZE} to the device's {max}")]
    Side {
        /// The pages' side.
        side: u32,
        /// The device's largest texture side.
        max: u32,
    },
    /// The pages and the reserved layer are more layers than the device's
    /// arrays hold, or than a sprite's 16-bit layer names.
    #[error("{layers} layers, past the device's {max}")]
    Layers {
        /// The layers needed, the reserved one included.
        layers: u64,
        /// The most the device and a sprite allow.
        max: u32,
    },
    /// More level-0 texels than an atlas may hold (design.md "Memory
    /// Budget").
    #[error("{texels} texels, past {MAX_ATLAS_TEXELS}")]
    Texels {
        /// The level-0 texels of every layer, the reserved one included.
        texels: u64,
    },
    /// A level whose bytes are not its square of RGBA8 texels.
    #[error("page {page}'s level {level} holds {len} bytes, not {expected}")]
    Level {
        /// The page.
        page: usize,
        /// The level.
        level: usize,
        /// The bytes given.
        len: usize,
        /// The bytes its side needs.
        expected: u64,
    },
    /// The device refused the atlas's texture, its upload or its binding
    /// (out of memory, or a validation the checks above did not foresee).
    #[error("the device refused the atlas")]
    Device(#[source] wgpu::Error),
}

/// The limits a skin's atlas is built within on `device`: its largest
/// texture side and array layers, and the reserved layer.
#[must_use]
pub fn atlas_limits(device: &wgpu::Device) -> AtlasLimits {
    let l = device.limits();
    AtlasLimits {
        max_side: l.max_texture_dimension_2d,
        max_layers: l.max_texture_array_layers,
        reserved_layers: RESERVED_LAYERS,
    }
}

/// Checks `pages` against `device`'s limits and against themselves, before
/// anything is allocated; returns the array's layers.
///
/// # Errors
///
/// The first check that fails ([`check`]).
pub(crate) fn check_pages(
    pages: &AtlasPages<'_>,
    device: &wgpu::Device,
) -> Result<u32, UploadError> {
    check(pages, &device.limits())
}

/// Runs `work` inside out-of-memory and validation error scopes on
/// `device`, and returns its result with the first error the device
/// reported meanwhile: validation first, then out of memory.
pub(crate) fn scoped<T>(
    device: &wgpu::Device,
    work: impl FnOnce() -> T,
) -> (T, Option<wgpu::Error>) {
    let out_of_memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let invalid = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let out = work();
    // Popped in the reverse order of their pushes.
    let refused = pollster::block_on(invalid.pop());
    let refused = refused.or(pollster::block_on(out_of_memory.pop()));
    (out, refused)
}

/// Checks `pages` against the device's `limits` and against themselves,
/// in that order, before anything is allocated: the side, the layers,
/// the texels, then each level's bytes. Returns the array's layers.
fn check(pages: &AtlasPages<'_>, limits: &wgpu::Limits) -> Result<u32, UploadError> {
    let side = pages.side;
    let max = limits.max_texture_dimension_2d;
    if !side.is_power_of_two() || side < ATLAS_SIZE || side > max {
        return Err(UploadError::Side { side, max });
    }
    let layers = u64::try_from(pages.pages.len())
        .unwrap_or(u64::MAX)
        .saturating_add(u64::from(RESERVED_LAYERS));
    // A sprite names its layer in 16 bits.
    let max_layers = limits.max_texture_array_layers.min(1 << 16);
    let Some(layer_count) = u32::try_from(layers).ok().filter(|&n| n <= max_layers) else {
        return Err(UploadError::Layers {
            layers,
            max: max_layers,
        });
    };
    let texels = u64::from(side) * u64::from(side) * layers;
    if texels > MAX_ATLAS_TEXELS {
        return Err(UploadError::Texels { texels });
    }
    for (page, levels) in pages.pages.iter().enumerate() {
        for (level, bytes) in levels.iter().enumerate() {
            let s = u64::from(side >> level);
            let expected = s * s * 4;
            if u64::try_from(bytes.len()).ok() != Some(expected) {
                return Err(UploadError::Level {
                    page,
                    level,
                    len: bytes.len(),
                    expected,
                });
            }
        }
    }
    Ok(layer_count)
}

/// The atlas texture array on the GPU.
#[derive(Debug)]
pub(crate) struct AtlasTexture {
    view: wgpu::TextureView,
    side: u32,
    layers: u32,
}

impl AtlasTexture {
    /// The built-in atlas alone: one 128-texel layer without mips.
    pub(crate) fn builtin(gpu: &Gpu) -> Self {
        let texture = create(gpu.device(), ATLAS_SIZE, 1, 1);
        write(gpu.queue(), &texture, 0, 0, ATLAS_SIZE, &build_atlas());
        Self::of(&texture, ATLAS_SIZE, 1)
    }

    /// The array of `pages`, of `layers` layers as [`check_pages`] gave
    /// them, with the built-in atlas in layer 0. The uploads are queued,
    /// not yet submitted; what the device refuses is reported to its error
    /// scopes ([`scoped`]).
    pub(crate) fn upload(gpu: &Gpu, pages: &AtlasPages<'_>, layers: u32) -> Self {
        let device = gpu.device();
        let side = pages.side;
        let levels = u32::try_from(MIP_LEVELS).unwrap_or(1);
        let texture = create(device, side, layers, levels);
        // The built-in atlas at the top left of layer 0's level 0; the
        // shader samples nothing else of layer 0.
        write(gpu.queue(), &texture, 0, 0, ATLAS_SIZE, &build_atlas());
        for (layer, page) in (RESERVED_LAYERS..).zip(pages.pages) {
            for (level, bytes) in (0..).zip(page) {
                write(gpu.queue(), &texture, layer, level, side >> level, bytes);
            }
        }
        Self::of(&texture, side, layers)
    }

    fn of(texture: &wgpu::Texture, side: u32, layers: u32) -> Self {
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("atlas"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..wgpu::TextureViewDescriptor::default()
        });
        Self { view, side, layers }
    }

    /// The array's view, for the sprites' bind group.
    pub(crate) const fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The pages' side.
    pub(crate) const fn side(&self) -> u32 {
        self.side
    }

    /// The layers, the reserved one included.
    pub(crate) const fn layers(&self) -> u32 {
        self.layers
    }

    /// What layer 0's uv is multiplied by at staging: the built-in atlas's
    /// side over the page side, a power of two, so exact.
    pub(crate) fn font_scale(&self) -> f32 {
        ATLAS_SIZE as f32 / self.side as f32
    }
}

fn create(device: &wgpu::Device, side: u32, layers: u32, levels: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("atlas"),
        size: wgpu::Extent3d {
            width: side,
            height: side,
            depth_or_array_layers: layers,
        },
        mip_level_count: levels,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// Writes `side`-texel-square `bytes` at the top left of `layer`'s
/// `level`.
fn write(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    layer: u32,
    level: u32,
    side: u32,
    bytes: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: level,
            origin: wgpu::Origin3d {
                x: 0,
                y: 0,
                z: layer,
            },
            aspect: wgpu::TextureAspect::All,
        },
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(side * 4),
            rows_per_image: Some(side),
        },
        wgpu::Extent3d {
            width: side,
            height: side,
            depth_or_array_layers: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use skin_model::MIP_LEVELS;

    use super::{AtlasPages, PageLevels, UploadError, check};

    fn limits() -> wgpu::Limits {
        wgpu::Limits::default()
    }

    /// The default limits: 8192-texel sides and 256 layers.
    #[test]
    fn checks_come_before_any_allocation() {
        let empty: PageLevels<'_> = [&[]; MIP_LEVELS];
        let all = [empty; 300];
        let some = |side, n: usize| AtlasPages {
            side,
            pages: &all[..n],
        };
        for side in [0, 64, 100, 129, 16_384] {
            assert!(
                matches!(
                    check(&some(side, 1), &limits()),
                    Err(UploadError::Side { side: s, max: 8_192 }) if s == side
                ),
                "side {side}"
            );
        }
        assert!(matches!(
            check(&some(256, 256), &limits()),
            Err(UploadError::Layers {
                layers: 257,
                max: 256
            })
        ));
        // 8192² × 3 layers: 192 Mi texels, past 128 Mi.
        assert!(matches!(
            check(&some(8_192, 2), &limits()),
            Err(UploadError::Texels { texels }) if texels == 3 << 26
        ));
        // Within the limits, the bytes are checked: level 0 of page 0 is
        // 256 × 256 × 4.
        assert!(matches!(
            check(&some(256, 1), &limits()),
            Err(UploadError::Level {
                page: 0,
                level: 0,
                len: 0,
                expected: 262_144
            })
        ));
    }

    #[test]
    fn whole_levels_pass() {
        let l0 = vec![0u8; 256 * 256 * 4];
        let l1 = vec![0u8; 128 * 128 * 4];
        let l2 = vec![0u8; 64 * 64 * 4];
        let l3 = vec![0u8; 32 * 32 * 4];
        let short = vec![0u8; 32 * 32 * 4 - 1];
        let page: PageLevels<'_> = [&l0, &l1, &l2, &l3];
        let pages = [page, page];
        let ok = AtlasPages {
            side: 256,
            pages: &pages,
        };
        assert_eq!(check(&ok, &limits()).unwrap(), 3);
        let bad_page: PageLevels<'_> = [&l0, &l1, &l2, &short];
        let pages = [page, bad_page];
        let bad = AtlasPages {
            side: 256,
            pages: &pages,
        };
        assert!(matches!(
            check(&bad, &limits()),
            Err(UploadError::Level {
                page: 1,
                level: 3,
                len: 4_095,
                expected: 4_096
            })
        ));
        // No pages: the reserved layer alone, at a skin's side.
        let none = AtlasPages {
            side: 512,
            pages: &[],
        };
        assert_eq!(check(&none, &limits()).unwrap(), 1);
    }
}
