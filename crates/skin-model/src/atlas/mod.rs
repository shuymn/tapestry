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
//! The atlas keeps where it placed each cell, by the cell's identity, so
//! another sheet of the same skin folder whose cells it holds is drawn from
//! the same pages with no file read ([`Atlas::rebind`]).
//!
//! Building an atlas is load-stage work: it allocates and reads the files.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

mod mip;
pub use mip::downsample_rgba;
mod pack;
pub(crate) mod uv;

use std::path::PathBuf;
use std::sync::Arc;

use ::image::RgbaImage;

use crate::image::decode::{self, Bands, Site};
use crate::image::path;
use crate::image::{self, Stop};
use crate::image::{CellSource, FileAt, ImageError, MAX_FILE_BYTES, MAX_TOTAL_FILE_BYTES, Sheet};
use pack::PackError;
use uv::{FrameUv, UvTable};

/// The bytes a cell keeps in the atlas's tables while its files are read
/// and decoded: its rectangle and its place.
pub const CELL_TABLE_BYTES: usize = size_of::<[u32; 4]>() + size_of::<pack::Place>();
/// The bytes a file with cells keeps in the atlas's tables while files are
/// read and decoded: its index and its cells' range.
pub const FILE_TABLE_BYTES: usize = size_of::<FileCells>();
/// The most the atlas's tables hold while files are read and decoded:
/// [`MAX_SKIN_FRAMES`](crate::MAX_SKIN_FRAMES) cells and
/// [`MAX_FILES`](crate::MAX_FILES) files, 65 536 × 36 B + 4 096 × 24 B =
/// 2 457 600 B. No file's path is kept: each is resolved again as it is
/// read, so only the one being read has its canonical path live.
pub const ATLAS_TABLE_BYTES: usize =
    crate::MAX_SKIN_FRAMES * CELL_TABLE_BYTES + crate::image::MAX_FILES * FILE_TABLE_BYTES;

/// A file with cells: its index and its cells' range.
type FileCells = (usize, usize, usize);

/// Mip levels per page, level 0 included.
pub const MIP_LEVELS: usize = 4;
/// Texels of edge copies around each cell.
pub const GUTTER: u32 = 8;
/// Every allocation's origin and size are multiples of this: `2^(levels - 1)`.
pub const ALIGN: u32 = 1 << (MIP_LEVELS - 1);
/// Every page side is a multiple of this: the built-in atlas's side, so
/// it fits a reserved layer, and a multiple of [`ALIGN`], so every mip
/// level halves the side exactly.
pub const PAGE_SIDE_STEP: u32 = 128;
/// The smallest page side.
pub const MIN_PAGE_SIDE: u32 = 256;
/// The largest page side, also capped by the device's.
pub const MAX_PAGE_SIDE: u32 = 8_192;

const _: () = assert!(PAGE_SIDE_STEP.is_multiple_of(ALIGN));
const _: () = assert!(
    MIN_PAGE_SIDE.is_multiple_of(PAGE_SIDE_STEP) && MAX_PAGE_SIDE.is_multiple_of(PAGE_SIDE_STEP)
);
/// The most skin pages, also capped by the device's layers.
pub const MAX_PAGES: u32 = 64;
/// The most level-0 texels across every layer of the array, the reserved
/// ones and the text layer included; the mips add a third.
pub const MAX_ATLAS_TEXELS: u64 = 192 << 20;
/// The smallest page side of an atlas with a text layer.
pub const MIN_TEXT_SIDE: u32 = 2_048;

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
    /// A rebind's sheet has a cell the atlas does not hold, or another
    /// skin folder, variant or text layer ([`Atlas::rebind`]).
    #[error("the atlas does not hold the sheet's cells for its folder, variant and text layer")]
    NotHeld,
    #[error(transparent)]
    Image(#[from] ImageError),
    /// The build was cancelled: neither a file's failure nor a limit.
    #[error("the atlas build was cancelled")]
    Cancelled,
}

impl From<Stop> for AtlasError {
    fn from(s: Stop) -> Self {
        match s {
            Stop::Cancelled => Self::Cancelled,
            Stop::Image(e) => Self::Image(e),
        }
    }
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

/// An atlas's pages with where it placed each cell: what every atlas
/// rebound from it shares ([`Atlas::rebind`]).
#[derive(Debug)]
pub struct PackedPages {
    side: u32,
    pages: Vec<Page>,
    variant: Variant,
    text_layer: Option<u16>,
    /// The layers kept before the pages when they were packed
    /// ([`AtlasLimits::reserved_layers`]): page `i` is layer
    /// `reserved_layers + i`.
    reserved_layers: u32,
    /// The skin folder the files were read from.
    root: PathBuf,
    /// The files read, by their admitted identity, ordered by name.
    files: Box<[HeldFile]>,
    /// Every cell placed, ordered by its source and rectangle.
    cells: Box<[HeldCell]>,
}

/// The held files and cells of a build.
type Held = (Box<[HeldFile]>, Box<[HeldCell]>);

/// A file read, by its admitted identity: its name in the skin folder in
/// the version the variant reads, and what its header gave at load.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeldFile {
    name: String,
    dims: [u32; 2],
    mode: decode::Mode,
}

/// Where a held cell's texels came from: an index into the held files, or
/// a generated colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum HeldSource {
    File(u32),
    Solid([u8; 4]),
}

/// A cell placed: its source, its rectangle in the texels of the version
/// read (`[0, 0, w, h]` for a generated cell), and where its content lies.
#[derive(Clone, Copy, Debug)]
struct HeldCell {
    source: HeldSource,
    rect: [u32; 4],
    /// The content's left and top on its page, past the gutter.
    at: [u32; 2],
    layer: u16,
}

impl HeldCell {
    fn key(&self) -> (HeldSource, [u32; 4]) {
        (self.source, self.rect)
    }
}

impl PackedPages {
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

    /// The layer kept for rasterized text, after the pages, if any.
    #[must_use]
    pub fn text_layer(&self) -> Option<u16> {
        self.text_layer
    }

    #[must_use]
    pub fn variant(&self) -> Variant {
        self.variant
    }

    /// The layers kept before the pages when they were packed: page `i`
    /// is layer `reserved_layers + i`, whichever frames a table uses.
    #[must_use]
    pub fn reserved_layers(&self) -> u32 {
        self.reserved_layers
    }

    /// The bytes the placements keep beside the pages: every held cell
    /// and every held file with its name.
    #[must_use]
    pub fn placement_bytes(&self) -> u64 {
        let names: usize = self.files.iter().map(|f| f.name.capacity()).sum();
        u64::try_from(size_of_val(&*self.cells) + size_of_val(&*self.files) + names)
            .unwrap_or(u64::MAX)
    }

    /// The held file of `version`'s identity.
    fn file(&self, version: &image::Version) -> Option<u32> {
        let from = self
            .files
            .partition_point(|f| f.name.as_str() < version.name.as_str());
        let i = self
            .files
            .get(from..)?
            .iter()
            .take_while(|f| f.name == version.name)
            .position(|f| f.dims == version.dims && f.mode == version.mode)?;
        u32::try_from(from + i).ok()
    }

    /// `sheet`'s uv table on these pages: one entry per frame, in the
    /// sheet's frame order, each its cell's placement. O(frames + cells
    /// log cells); it reads no file.
    fn uvs_for(&self, sheet: &Sheet, variant: Variant, text: bool) -> Result<UvTable, AtlasError> {
        if variant != self.variant
            || text != self.text_layer.is_some()
            || sheet.root() != self.root.as_path()
        {
            return Err(AtlasError::NotHeld);
        }
        let files = sheet.files();
        let mut held = Vec::with_capacity(sheet.cells().len());
        for cell in sheet.cells() {
            let key = match cell.source {
                CellSource::File(f) => {
                    let (version, hi) = usize::try_from(f)
                        .ok()
                        .and_then(|f| files.get(f))
                        .ok_or(AtlasError::NotHeld)?
                        .version(variant);
                    let file = self.file(version).ok_or(AtlasError::NotHeld)?;
                    let rect = if hi {
                        cell.rect.map(|v| v * 2)
                    } else {
                        cell.rect
                    };
                    (HeldSource::File(file), rect)
                }
                CellSource::Solid(color) => (HeldSource::Solid(color), cell.rect),
            };
            let i = self
                .cells
                .binary_search_by(|h| h.key().cmp(&key))
                .map_err(|_| AtlasError::NotHeld)?;
            held.push(i);
        }
        let to_uv = |v: u32| v as f32 / self.side as f32;
        let mut frames = Vec::with_capacity(sheet.frames().len());
        for &cell in sheet.frames() {
            let Some(c) = usize::try_from(cell)
                .ok()
                .and_then(|c| held.get(c))
                .and_then(|&h| self.cells.get(h))
            else {
                return Err(AtlasError::NotHeld);
            };
            let ([x, y], [_, _, w, h]) = (c.at, c.rect);
            frames.push(FrameUv {
                uv: [to_uv(x), to_uv(y), to_uv(x + w), to_uv(y + h)],
                layer: c.layer,
            });
        }
        Ok(UvTable::new(sheet.id(), frames.into_boxed_slice()))
    }
}

/// A sheet's atlas for one variant: its pages, shared, and the uv table of
/// the sheet it was built from or rebound to.
#[derive(Clone, Debug)]
pub struct Atlas {
    pages: Arc<PackedPages>,
    uvs: UvTable,
}

impl Atlas {
    /// The layer kept for rasterized text, after the pages, if the atlas
    /// has one ([`Sheet::atlas_reserving`]). It has no CPU page: the
    /// renderer creates it zeroed and writes lines into it.
    #[must_use]
    pub fn text_layer(&self) -> Option<u16> {
        self.pages.text_layer
    }

    /// Every page's side at level 0.
    #[must_use]
    pub fn side(&self) -> u32 {
        self.pages.side
    }

    /// The skin's pages; page `i` is layer `reserved_layers + i`.
    #[must_use]
    pub fn pages(&self) -> &[Page] {
        &self.pages.pages
    }

    /// The pages with their placements, shared with every atlas rebound
    /// from this one.
    #[must_use]
    pub fn shared_pages(&self) -> &Arc<PackedPages> {
        &self.pages
    }

    #[must_use]
    pub fn uvs(&self) -> &UvTable {
        &self.uvs
    }

    #[must_use]
    pub fn variant(&self) -> Variant {
        self.pages.variant
    }

    /// The shared pages, to upload, and the uv table, to keep.
    #[must_use]
    pub fn into_parts(self) -> (Arc<PackedPages>, UvTable) {
        (self.pages, self.uvs)
    }

    /// The same pages with `sheet`'s uv table, made from the placements
    /// kept with them, with no file read and no pixel decoded: one entry
    /// per frame of `sheet`, in its frame order, each its cell's
    /// placement, under `sheet`'s id. A cell is held when a cell of the
    /// same identity was packed: the same file, by its name in the skin
    /// folder in the version `variant` reads and the header admission
    /// recorded, and the same rectangle; or the same generated colour and
    /// size. `sheet` may use any subset of the cells, on any of the pages.
    /// Load-stage work: it allocates the table.
    ///
    /// # Errors
    ///
    /// [`AtlasError::NotHeld`] when `sheet` is of another skin folder,
    /// `variant` or `text` (whether a text layer is kept,
    /// [`Sheet::atlas_reserving`]) is not the atlas's, or a cell of
    /// `sheet` is not held.
    pub fn rebind(&self, sheet: &Sheet, variant: Variant, text: bool) -> Result<Self, AtlasError> {
        let uvs = self.pages.uvs_for(sheet, variant, text)?;
        Ok(Self {
            pages: Arc::clone(&self.pages),
            uvs,
        })
    }
}

fn alloc_level(bytes: u64) -> Result<Vec<u8>, AtlasError> {
    let n = usize::try_from(bytes).map_err(|_| AtlasError::AtlasAlloc)?;
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| AtlasError::AtlasAlloc)?;
    v.resize(n, 0);
    Ok(v)
}

/// The layer count the limits allow for skin pages, with `extra` layers
/// kept after them.
fn page_cap(limits: &AtlasLimits, extra: u32) -> Result<u32, AtlasError> {
    let AtlasLimits {
        max_side,
        max_layers,
        reserved_layers,
    } = *limits;
    let kept = reserved_layers.saturating_add(extra);
    if kept >= max_layers || max_side < MIN_PAGE_SIDE {
        return Err(AtlasError::Limits);
    }
    let pages = (max_layers - kept).min(MAX_PAGES);
    let last = u64::from(kept) + u64::from(pages) - 1;
    if last > u64::from(u16::MAX) {
        return Err(AtlasError::Limits);
    }
    Ok(pages)
}

/// A cell's allocation size, its content with its gutter and alignment,
/// when that fits a page of side `cap`.
fn allocation(cell: [u32; 2], cap: u32) -> Option<[u32; 2]> {
    pack::padded(cell).filter(|size| size.iter().all(|&s| s <= cap))
}

/// Whether a cell of `[w, h]` texels, in the version an atlas reads, fits
/// the largest page any atlas has ([`MAX_PAGE_SIDE`]): the check
/// [`Sheet::atlas_reserving`] makes of each cell. A lower device limit, the
/// pages all the cells need and the array's texels are decided by that
/// build.
#[must_use]
pub fn cell_fits_page(cell: [u32; 2]) -> bool {
    allocation(cell, pack::side_cap(MAX_PAGE_SIDE)).is_some()
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
    /// whole, or in bands of rows when it was admitted to be
    /// ([`MAX_STREAMED_TEXELS`](crate::MAX_STREAMED_TEXELS)), each band
    /// written to every cell it crosses. A file whose dimensions or format,
    /// or a PNG whose header fields, changed since load is refused. Generated cells
    /// ([`SolidImageDef`](crate::SolidImageDef)) are written last, at one
    /// size in both variants, reading nothing.
    ///
    /// Memory beside the pages: the tables ([`ATLAS_TABLE_BYTES`] at
    /// most), the canonical path of the file being read, and that one file
    /// read and decoded within its own bound
    /// ([`MAX_IMAGE_TEXELS`](crate::MAX_IMAGE_TEXELS),
    /// [`MAX_STREAMED_TEXELS`](crate::MAX_STREAMED_TEXELS)).
    ///
    /// `cancel` is asked as each file's bytes are read, before each band
    /// and each read after a banded file's rows, and before each page's
    /// mip level.
    ///
    /// # Errors
    ///
    /// The first check that fails; [`AtlasError::Cancelled`] once `cancel`
    /// said so.
    pub fn atlas(
        &self,
        variant: Variant,
        limits: AtlasLimits,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Atlas, AtlasError> {
        self.atlas_reserving(variant, limits, false, cancel)
    }

    /// [`atlas`](Self::atlas), with a text layer kept after the pages when
    /// `text` is set: the page side is then at least [`MIN_TEXT_SIDE`], the
    /// packing's cost and the [`MAX_ATLAS_TEXELS`] check count the layer,
    /// and the device's layers must hold it. The layer has no CPU page
    /// ([`Atlas::text_layer`]).
    ///
    /// # Errors
    ///
    /// As [`atlas`](Self::atlas).
    pub fn atlas_reserving(
        &self,
        variant: Variant,
        limits: AtlasLimits,
        text: bool,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Atlas, AtlasError> {
        let extra = u32::from(text);
        let min_side = if text { MIN_TEXT_SIDE } else { 0 };
        let max_pages = page_cap(&limits, extra)?;
        let cap = pack::side_cap(limits.max_side.min(MAX_PAGE_SIDE));
        let files = self.files();
        // Each cell's rectangle in the version the variant reads.
        let mut rects = Vec::with_capacity(self.cells().len());
        let mut padded = Vec::with_capacity(self.cells().len());
        for cell in self.cells() {
            // A generated cell has one size in both variants.
            let hi = match cell.source {
                CellSource::File(f) => usize::try_from(f)
                    .ok()
                    .and_then(|f| files.get(f))
                    .is_some_and(|f| f.version(variant).1),
                CellSource::Solid(_) => false,
            };
            let rect = if hi {
                cell.rect.map(|v| v * 2)
            } else {
                cell.rect
            };
            let [_, _, w, h] = rect;
            let size = allocation([w, h], cap).ok_or(AtlasError::CellTooLarge { w, h })?;
            rects.push(rect);
            padded.push(size);
        }
        let kept = limits.reserved_layers + extra;
        let packing = pack::pack(&padded, cap, max_pages, kept, min_side).map_err(|e| match e {
            PackError::TooManyPages => AtlasError::TooManyPages,
            PackError::Misaligned => AtlasError::Misaligned,
        })?;
        // Only the rectangles and the places stay beside the files
        // ([`CELL_TABLE_BYTES`]).
        drop(padded);
        let side = packing.side;
        let texels = pack::cost(kept, packing.pages, side);
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

        self.fill(variant, &rects, &packing, &mut pages, cancel)?;
        for page in &mut pages {
            for k in 1..MIP_LEVELS {
                decode::checkpoint(cancel, Site::Mip).map_err(|_| AtlasError::Cancelled)?;
                let (done, rest) = page.levels.split_at_mut(k);
                if let (Some(src), Some(dst)) = (done.last(), rest.first_mut()) {
                    let src_side = side >> (k - 1);
                    mip::downsample_rgba(src, src_side, dst);
                }
            }
        }

        let text_layer = if text {
            let layer = limits.reserved_layers + packing.pages;
            Some(u16::try_from(layer).map_err(|_| AtlasError::Limits)?)
        } else {
            None
        };
        // The placements are made only once every file is read and freed.
        let (files, cells) = self.held(variant, limits.reserved_layers, &rects, &packing)?;
        drop((rects, packing));
        let pages = PackedPages {
            side,
            pages,
            variant,
            text_layer,
            reserved_layers: limits.reserved_layers,
            root: self.root().to_path_buf(),
            files,
            cells,
        };
        // The sheet's own table is a rebind to the pages it just filled.
        let uvs = pages
            .uvs_for(self, variant, text)
            .map_err(|_| AtlasError::Misaligned)?;
        Ok(Atlas {
            pages: Arc::new(pages),
            uvs,
        })
    }

    /// The held files and cells of a packing: each cell's identity, its
    /// rectangle in the version read and its place, ordered for lookup.
    fn held(
        &self,
        variant: Variant,
        reserved: u32,
        rects: &[[u32; 4]],
        packing: &pack::Packing,
    ) -> Result<Held, AtlasError> {
        let identity = |file: u32| {
            let (version, _) = self
                .files()
                .get(usize::try_from(file).ok()?)?
                .version(variant);
            Some(HeldFile {
                name: version.name.clone(),
                dims: version.dims,
                mode: version.mode,
            })
        };
        // File cells are grouped by file ([`CellSource`]'s order).
        let mut files: Vec<HeldFile> = Vec::new();
        let mut last = None;
        for cell in self.cells() {
            if let CellSource::File(f) = cell.source
                && last != Some(f)
            {
                last = Some(f);
                files.push(identity(f).ok_or(AtlasError::Misaligned)?);
            }
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        files.dedup();
        let files = files.into_boxed_slice();
        let index = |file: u32| {
            let held = identity(file)?;
            let from = files.partition_point(|f| f.name < held.name);
            let i = files.get(from..)?.iter().position(|f| *f == held)?;
            u32::try_from(from + i).ok()
        };
        let mut cells = Vec::with_capacity(self.cells().len());
        for (i, cell) in self.cells().iter().enumerate() {
            let (Some(place), Some(&rect)) = (packing.places.get(i), rects.get(i)) else {
                return Err(AtlasError::Misaligned);
            };
            let source = match cell.source {
                CellSource::File(f) => HeldSource::File(index(f).ok_or(AtlasError::Misaligned)?),
                CellSource::Solid(color) => HeldSource::Solid(color),
            };
            let [x, y, ..] = place.alloc;
            cells.push(HeldCell {
                source,
                rect,
                at: [x + GUTTER, y + GUTTER],
                layer: u16::try_from(reserved + place.page).map_err(|_| AtlasError::Limits)?,
            });
        }
        // Two files of one identity are one file: a rectangle of both is
        // looked up once, at the first placement.
        cells.sort_by_key(HeldCell::key);
        cells.dedup_by_key(|c| c.key());
        Ok((files, cells.into_boxed_slice()))
    }

    /// Reads each file with cells, in file order, and writes its cells;
    /// then writes each generated cell's colour, reading nothing.
    fn fill(
        &self,
        variant: Variant,
        rects: &[[u32; 4]],
        packing: &pack::Packing,
        pages: &mut [Page],
        cancel: &dyn Fn() -> bool,
    ) -> Result<(), AtlasError> {
        let files = self.files();
        let cells = self.cells();
        // The files with cells, checked and summed before any is read.
        // File cells come first ([`CellSource`]'s order).
        let mut used: Vec<FileCells> = Vec::new();
        let mut start = 0;
        while let Some(&CellSource::File(file)) = cells.get(start).map(|c| &c.source) {
            let end = start
                + cells.get(start..).map_or(0, |rest| {
                    rest.iter()
                        .take_while(|c| c.source == CellSource::File(file))
                        .count()
                });
            used.push((usize::try_from(file).unwrap_or(usize::MAX), start, end));
            start = end;
        }
        // At most [`MAX_FILES`](crate::MAX_FILES) entries
        // ([`ATLAS_TABLE_BYTES`]), whatever the growth left.
        used.shrink_to_fit();
        // Each file's path and length, checked and summed before any is
        // read; the canonical paths are not kept.
        let version = |file: usize| {
            let (version, hi) = files.get(file)?.version(variant);
            Some((version, FileAt { file, hi }))
        };
        let mut total = 0_u64;
        for &(file, _, _) in &used {
            let Some((version, at)) = version(file) else {
                continue;
            };
            let (_, len) =
                path::resolve(self.root(), &version.name).map_err(|e| ImageError::path(at, e))?;
            total = total.saturating_add(len);
            if total > MAX_TOTAL_FILE_BYTES {
                return Err(ImageError::TotalBytesTooLarge { at }.into());
            }
        }

        let mut budget = MAX_TOTAL_FILE_BYTES;
        for &(file, start, end) in &used {
            let Some((version, at)) = version(file) else {
                continue;
            };
            let header = (version.dims, version.mode);
            // Resolved again as it is read: only this file's path is live.
            let (canonical, _) =
                path::resolve(self.root(), &version.name).map_err(|e| ImageError::path(at, e))?;
            let bytes = image::read_encoded(&canonical, at, MAX_FILE_BYTES.min(budget), cancel)?;
            drop(canonical);
            budget = budget.saturating_sub(u64::try_from(bytes.len()).unwrap_or(budget));
            // This file's placements and window, at their exact size, made
            // after its bytes are read and freed with them: nothing of one
            // file's routing is live while the next file is read.
            let banded = matches!(header.1, decode::Mode::Banded(_));
            let mut places = Vec::with_capacity(end.saturating_sub(start));
            // Only bands are routed through a window.
            let mut active = Vec::with_capacity(if banded { places.capacity() } else { 0 });
            places.extend((start..end).filter_map(|i| {
                let place = packing.places.get(i)?;
                Some(Placement {
                    rect: *rects.get(i)?,
                    page: usize::try_from(place.page).ok()?,
                    alloc: place.alloc,
                })
            }));
            if banded {
                places.sort_unstable_by_key(Placement::top);
            }
            let mut router = Router::new(&places, &mut active, pages, packing.side);
            match image::decode_file(bytes, at, header, cancel, &mut router)? {
                Some(image) => router.blit(&image),
                None => router.complete(),
            }
        }

        // The generated cells, after the files.
        for (i, cell) in cells.iter().enumerate().skip(start) {
            let CellSource::Solid(color) = cell.source else {
                continue;
            };
            let Some(place) = packing.places.get(i) else {
                continue;
            };
            let Some(level) = usize::try_from(place.page)
                .ok()
                .and_then(|p| level0(pages, p))
            else {
                continue;
            };
            mip::blit_solid(level, packing.side, color, place.alloc);
        }
        Ok(())
    }
}

/// Page `page`'s level 0.
fn level0(pages: &mut [Page], page: usize) -> Option<&mut Vec<u8>> {
    pages.get_mut(page)?.levels.first_mut()
}

/// Where one cell of a file goes: its rectangle in the file, its page and
/// its allocation there.
#[derive(Clone, Copy, Debug)]
struct Placement {
    rect: [u32; 4],
    page: usize,
    alloc: [u32; 4],
}

impl Placement {
    fn top(&self) -> u32 {
        self.rect[1]
    }

    /// The row past the cell's last.
    fn bottom(&self) -> u32 {
        self.rect[1].saturating_add(self.rect[3])
    }
}

/// Routes a file's bands to its cells: the placements, in order of their
/// first row for bands, and a window of the ones the current band crosses, entered
/// at their first row and left after their last, so the routing holds
/// O(cells) and does O(cells + rows) work besides the texels it writes.
struct Router<'a> {
    places: &'a [Placement],
    /// The next placement to enter, by first row.
    next: usize,
    /// The window: indices into `places`.
    active: &'a mut Vec<usize>,
    pages: &'a mut [Page],
    side: u32,
}

impl<'a> Router<'a> {
    /// A router over `places`, sorted by first row when bands are routed,
    /// with `active` empty.
    fn new(
        places: &'a [Placement],
        active: &'a mut Vec<usize>,
        pages: &'a mut [Page],
        side: u32,
    ) -> Self {
        Self {
            places,
            next: 0,
            active,
            pages,
            side,
        }
    }

    /// Writes a whole file's every placement.
    fn blit(self, image: &RgbaImage) {
        for p in self.places {
            if let Some(level) = level0(self.pages, p.page) {
                mip::blit(level, self.side, image, p.rect, p.alloc);
            }
        }
    }

    /// Completes every placement's allocation once all its rows are
    /// written: its first and last rows repeated through the gutter and
    /// the alignment padding, as [`mip::blit`] fills them.
    fn complete(self) {
        for p in self.places {
            if let Some(level) = level0(self.pages, p.page) {
                mip::complete(level, self.side, p.rect[3], p.alloc);
            }
        }
    }
}

impl Bands for Router<'_> {
    fn wants(&mut self, top: u32, bottom: u32) -> bool {
        let places = self.places;
        self.active
            .retain(|&i| places.get(i).is_some_and(|p| p.bottom() > top));
        while let Some(p) = places.get(self.next)
            && p.top() < bottom
        {
            if p.bottom() > top {
                self.active.push(self.next);
            }
            self.next += 1;
        }
        !self.active.is_empty()
    }

    fn put(&mut self, top: u32, band: &RgbaImage) {
        for &i in self.active.iter() {
            let Some(p) = self.places.get(i) else {
                continue;
            };
            if let Some(level) = level0(self.pages, p.page) {
                mip::scatter(level, self.side, band, top, p.rect, p.alloc);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_hold_36_bytes_a_cell_and_24_a_file() {
        assert_eq!(CELL_TABLE_BYTES, 36);
        assert_eq!(FILE_TABLE_BYTES, 24);
        assert_eq!(ATLAS_TABLE_BYTES, 65_536 * 36 + 4_096 * 24);
        assert_eq!(ATLAS_TABLE_BYTES, 2_457_600);
    }

    #[test]
    fn the_window_holds_the_cells_a_band_crosses() {
        // Cells of rows 0..40, 10..20 and 30..60, out of order.
        let mut places: Vec<Placement> = [[0, 30, 1, 30], [0, 0, 1, 40], [0, 10, 1, 10]]
            .into_iter()
            .map(|rect| Placement {
                rect,
                page: 0,
                alloc: [0, 0, 24, 80],
            })
            .collect();
        places.sort_unstable_by_key(Placement::top);
        let mut active = Vec::new();
        let mut router = Router::new(&places, &mut active, &mut [], 256);
        let mut seen = Vec::new();
        for top in (0..64).step_by(16) {
            let wants = router.wants(top, top + 16);
            let mut tops: Vec<u32> = router
                .active
                .iter()
                .map(|&i| router.places[i].top())
                .collect();
            tops.sort_unstable();
            seen.push((wants, tops));
        }
        assert_eq!(
            seen,
            [
                (true, vec![0, 10]),
                (true, vec![0, 10, 30]),
                (true, vec![0, 30]),
                (true, vec![30]),
            ]
        );
        assert!(!router.wants(64, 80));
    }
}
