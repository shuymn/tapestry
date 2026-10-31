//! A sheet's atlas for one variant: pages of a texture array with their
//! mip levels, and the uv table that finds each frame in them.
//!
//! Every distinct cell is packed on its own with a gutter of [`GUTTER`]
//! texels and its allocation aligned to [`ALIGN`], so each texel of level
//! `k` (below [`MIP_LEVELS`]) covers an aligned `2^k` block of level 0
//! inside one allocation, and no level mixes two cells. The gutter repeats
//! the cell's edge texels, which samples like clamp-to-edge on a texture of
//! its own. Texels are premultiplied before anything is filtered, so a
//! transparent texel's colour never shows.
//!
//! Building an atlas is load-stage work: it allocates and reads the files.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

mod mip;
mod pack;
pub(crate) mod uv;

use crate::image::path;
use crate::image::{self, decode};
use crate::image::{FileAt, ImageError, MAX_FILE_BYTES, MAX_TOTAL_FILE_BYTES, Sheet};
use pack::PackError;
use uv::{FrameUv, UvTable};

/// Mip levels per page, level 0 included.
pub const MIP_LEVELS: usize = 4;
/// Texels of edge copies around each cell.
pub const GUTTER: u32 = 8;
/// Every allocation's origin and size are multiples of this: `2^(levels - 1)`.
pub const ALIGN: u32 = 1 << (MIP_LEVELS - 1);
/// The smallest page side; the bitmap font's 128 fits a reserved layer.
pub const MIN_PAGE_SIDE: u32 = 256;
/// The largest page side, also capped by the device's.
pub const MAX_PAGE_SIDE: u32 = 8_192;
/// The most skin pages, also capped by the device's layers.
pub const MAX_PAGES: u32 = 64;
/// The most level-0 texels across every layer of the array, the reserved
/// ones included; the mips add a third.
pub const MAX_ATLAS_TEXELS: u64 = 128 << 20;

/// Which files an atlas reads: the 1x ones, or each file's @2x version
/// where it has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Lo,
    Hi,
}

/// What the device allows: its largest texture side and array layers, and
/// the layers kept for other use before the skin's pages (the bitmap font).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtlasLimits {
    pub max_side: u32,
    pub max_layers: u32,
    pub reserved_layers: u32,
}

/// Why an atlas was not built.
#[derive(Debug, thiserror::Error)]
pub enum AtlasError {
    #[error("the atlas limits leave no layer, a side below {MIN_PAGE_SIDE} or a layer past u16")]
    Limits,
    #[error("a frame of {w}×{h} with its gutter does not fit a page")]
    CellTooLarge { w: u32, h: u32 },
    #[error("the frames need more pages than the device's layers or {MAX_PAGES}")]
    TooManyPages,
    #[error("the atlas needs {texels} texels, more than {MAX_ATLAS_TEXELS}")]
    AtlasTooLarge { texels: u64 },
    #[error("the atlas pages could not be allocated")]
    AtlasAlloc,
    #[error("the packer placed a frame off the alignment")]
    Misaligned,
    #[error(transparent)]
    Image(#[from] ImageError),
}

/// One page: its mip levels, each RGBA8, premultiplied, row by row.
#[derive(Clone, Debug)]
pub struct Page {
    levels: [Vec<u8>; MIP_LEVELS],
}

impl Page {
    /// Level `k`'s texels, `side >> k` per row and column.
    #[must_use]
    pub fn level(&self, k: usize) -> Option<&[u8]> {
        self.levels.get(k).map(Vec::as_slice)
    }
}

/// A sheet's atlas for one variant.
#[derive(Clone, Debug)]
pub struct Atlas {
    side: u32,
    pages: Vec<Page>,
    uvs: UvTable,
    variant: Variant,
}

impl Atlas {
    /// Every page's side at level 0.
    #[must_use]
    pub fn side(&self) -> u32 {
        self.side
    }

    /// The skin's pages; page `i` is layer `reserved_layers + i`.
    #[must_use]
    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    #[must_use]
    pub fn uvs(&self) -> &UvTable {
        &self.uvs
    }

    #[must_use]
    pub fn variant(&self) -> Variant {
        self.variant
    }

    /// The pages, to upload and drop, and the uv table, to keep.
    #[must_use]
    pub fn into_parts(self) -> (Vec<Page>, UvTable) {
        (self.pages, self.uvs)
    }
}

fn alloc_level(bytes: u64) -> Result<Vec<u8>, AtlasError> {
    let n = usize::try_from(bytes).map_err(|_| AtlasError::AtlasAlloc)?;
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| AtlasError::AtlasAlloc)?;
    v.resize(n, 0);
    Ok(v)
}

/// The layer count the limits allow for skin pages.
fn page_cap(limits: &AtlasLimits) -> Result<u32, AtlasError> {
    let AtlasLimits {
        max_side,
        max_layers,
        reserved_layers,
    } = *limits;
    if reserved_layers >= max_layers || max_side < MIN_PAGE_SIDE {
        return Err(AtlasError::Limits);
    }
    let pages = (max_layers - reserved_layers).min(MAX_PAGES);
    let last = u64::from(reserved_layers) + u64::from(pages) - 1;
    if last > u64::from(u16::MAX) {
        return Err(AtlasError::Limits);
    }
    Ok(pages)
}

impl Sheet {
    /// Builds the atlas of `variant` within `limits`.
    ///
    /// Checks, in order, before any pixel is allocated: the limits; every
    /// frame with its gutter fits a page; the frames fit the pages allowed
    /// at some side; the array's texels, reserved layers included, stay
    /// within [`MAX_ATLAS_TEXELS`]. Then it allocates the pages, checks
    /// every file to be read again (its path, at most [`MAX_FILE_BYTES`],
    /// and at most [`MAX_TOTAL_FILE_BYTES`] across them), and reads and
    /// decodes one file at a time, each into memory freed before the next:
    /// a file whose dimensions changed since load is refused.
    ///
    /// # Errors
    ///
    /// The first check that fails.
    pub fn atlas(&self, variant: Variant, limits: AtlasLimits) -> Result<Atlas, AtlasError> {
        let max_pages = page_cap(&limits)?;
        let cap = pack::side_cap(limits.max_side.min(MAX_PAGE_SIDE));
        let files = self.files();
        // Each cell's rectangle in the version the variant reads.
        let mut rects = Vec::with_capacity(self.cells().len());
        let mut padded = Vec::with_capacity(self.cells().len());
        for cell in self.cells() {
            let file = usize::try_from(cell.file).ok().and_then(|f| files.get(f));
            let hi = file.is_some_and(|f| f.version(variant).1);
            let rect = if hi {
                cell.rect.map(|v| v * 2)
            } else {
                cell.rect
            };
            let [_, _, w, h] = rect;
            let size = pack::padded([w, h]).ok_or(AtlasError::CellTooLarge { w, h })?;
            if size.iter().any(|&s| s > cap) {
                return Err(AtlasError::CellTooLarge { w, h });
            }
            rects.push(rect);
            padded.push(size);
        }
        let packing =
            pack::pack(&padded, cap, max_pages, limits.reserved_layers).map_err(|e| match e {
                PackError::TooManyPages => AtlasError::TooManyPages,
                PackError::Misaligned => AtlasError::Misaligned,
            })?;
        let side = packing.side;
        let texels = pack::cost(limits.reserved_layers, packing.pages, side);
        if texels > MAX_ATLAS_TEXELS {
            return Err(AtlasError::AtlasTooLarge { texels });
        }

        let mut pages = Vec::new();
        pages
            .try_reserve_exact(usize::try_from(packing.pages).unwrap_or(usize::MAX))
            .map_err(|_| AtlasError::AtlasAlloc)?;
        for _ in 0..packing.pages {
            let level = |k: u32| alloc_level(u64::from(side >> k) * u64::from(side >> k) * 4);
            pages.push(Page {
                levels: [level(0)?, level(1)?, level(2)?, level(3)?],
            });
        }

        self.fill(variant, &rects, &packing, &mut pages)?;
        for page in &mut pages {
            for k in 1..MIP_LEVELS {
                let (done, rest) = page.levels.split_at_mut(k);
                if let (Some(src), Some(dst)) = (done.last(), rest.first_mut()) {
                    let src_side = side >> (k - 1);
                    mip::downsample(src, src_side, dst);
                }
            }
        }

        let to_uv = |v: u32| v as f32 / side as f32;
        let mut frames = Vec::with_capacity(self.frames().len());
        for &cell in self.frames() {
            let i = usize::try_from(cell).unwrap_or(usize::MAX);
            let (Some(place), Some(&[_, _, w, h])) = (packing.places.get(i), rects.get(i)) else {
                return Err(AtlasError::Misaligned);
            };
            let [x, y, ..] = place.alloc;
            let (x, y) = (x + GUTTER, y + GUTTER);
            let layer = u16::try_from(limits.reserved_layers + place.page)
                .map_err(|_| AtlasError::Limits)?;
            frames.push(FrameUv {
                uv: [to_uv(x), to_uv(y), to_uv(x + w), to_uv(y + h)],
                layer,
            });
        }
        Ok(Atlas {
            side,
            pages,
            uvs: UvTable::new(self.id(), frames.into_boxed_slice()),
            variant,
        })
    }

    /// Reads each file with cells, in file order, and writes its cells.
    fn fill(
        &self,
        variant: Variant,
        rects: &[[u32; 4]],
        packing: &pack::Packing,
        pages: &mut [Page],
    ) -> Result<(), AtlasError> {
        let files = self.files();
        let cells = self.cells();
        // The files with cells, checked and summed before any is read.
        let mut used: Vec<(usize, usize, usize)> = Vec::new();
        let mut start = 0;
        while let Some(first) = cells.get(start) {
            let end = start
                + cells.get(start..).map_or(0, |rest| {
                    rest.iter().take_while(|c| c.file == first.file).count()
                });
            used.push((
                usize::try_from(first.file).unwrap_or(usize::MAX),
                start,
                end,
            ));
            start = end;
        }
        let mut total = 0_u64;
        let mut resolved = Vec::with_capacity(used.len());
        for &(file, _, _) in &used {
            let Some(f) = files.get(file) else {
                continue;
            };
            let (version, hi) = f.version(variant);
            let at = FileAt { file, hi };
            let (canonical, len) =
                path::resolve(self.root(), &version.name).map_err(|e| ImageError::path(at, e))?;
            total = total.saturating_add(len);
            if total > MAX_TOTAL_FILE_BYTES {
                return Err(ImageError::TotalBytesTooLarge { at }.into());
            }
            resolved.push((at, canonical, version.dims));
        }

        let mut budget = MAX_TOTAL_FILE_BYTES;
        for ((at, canonical, dims), &(_, start, end)) in resolved.into_iter().zip(&used) {
            let bytes = image::read_encoded(&canonical, at, MAX_FILE_BYTES.min(budget))?;
            budget = budget.saturating_sub(u64::try_from(bytes.len()).unwrap_or(budget));
            let image = decode::decode(bytes, dims).map_err(|e| ImageError::decode(at, e))?;
            for i in start..end {
                let (Some(place), Some(&rect)) = (packing.places.get(i), rects.get(i)) else {
                    continue;
                };
                let Some(page) = usize::try_from(place.page)
                    .ok()
                    .and_then(|p| pages.get_mut(p))
                else {
                    continue;
                };
                let Some(level) = page.levels.first_mut() else {
                    continue;
                };
                mip::blit(level, packing.side, &image, rect, place.alloc);
            }
        }
        Ok(())
    }
}
