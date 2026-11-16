//! A chart's BGA pictures (design.md "Rendering and Skins", BGA): each
//! `#BMPxx` name resolved in the chart's folder by the source's format
//! priority, its picture read under the chart images' caps, laid out on a
//! 256×256 canvas when it is small, packed with its fellows into pages of
//! an array of their own by a ladder of resolution steps that always ends
//! within the budget, and the table a frame draws from.
//!
//! - **Resolution** ([`resolve`]): a name that exists with a picture or
//!   video extension is searched for again with that format's extensions
//!   in order, its own extension replaced; any other existing name is used
//!   as written; a missing name is searched for with the video extensions,
//!   then the picture ones. Every candidate passes the chart images' name
//!   rules and containment (T08, [`chart_image`](crate::chart_image)).
//! - **Pictures** ([`probe`], [`read_cell`]): PNG, JPEG and BMP within the
//!   chart images' caps; the decoded texels are premultiplied, a picture
//!   whose longer side is at most [`CANVAS_SIDE`] is drawn at the top
//!   centre of a transparent canvas of that side, and the result is
//!   downscaled by an area filter to its cell and given a
//!   [`CELL_GUTTER`]-texel border repeating its edge. Every transient
//!   allocation is fallible, and refused at a caller's limit, so a refused
//!   allocation leaves the picture absent ([`Absent::Refused`]).
//! - **Packing** ([`pack`]): pages of [`PAGE_SIDE`], no mips; a set that
//!   does not fit [`PAGE_BUDGET`] pages caps the longer side at each of
//!   [`STEP_CAPS`] in turn, then fits every picture to [`GRID_SIDE`] in a
//!   uniform grid, which always fits two pages.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bms_model::BgaImageId;

use crate::atlas::pack::{self, MAX_CELLS_PER_PAGE};
use crate::atlas::premultiply;
use crate::atlas::uv::FrameUv;
use crate::canvas::f32_of;
use crate::chart_image::{
    self, Absent, Cap, ChartDir, ChartStretch, FileKey, Identity, Stop, Unresolved, fitted_size,
};
use crate::image::decode::{self, Mode, Site};
use crate::image::path::{self, PathError};
use crate::image::{MAX_IMAGE_TEXELS, MAX_PATH_BYTES, MAX_SIDE};
use crate::place::Native;

/// The side of the canvas a small picture is drawn on, and the longest
/// side a picture drawn on it may have.
pub const CANVAS_SIDE: u32 = 256;
/// The side of a page of the BGA array.
pub const PAGE_SIDE: u32 = 4_096;
/// The texels around each cell repeating its edge, so a linear sample
/// never reads a neighbour.
pub const CELL_GUTTER: u32 = 1;
/// The longest side a cell's content may have: a page less its gutters.
pub const MAX_CELL_SIDE: u32 = PAGE_SIDE - 2 * CELL_GUTTER;
/// The most pages one chart's pictures take.
pub const PAGE_BUDGET: u32 = 6;
/// The caps on a picture's longer side a set over budget steps through,
/// each shrinking only the pictures above it.
pub const STEP_CAPS: [u32; 4] = [2_048, 1_024, 512, 256];
/// The side every picture is fitted to at the last step.
pub const GRID_SIDE: u32 = 128;
/// A grid cell's side at the last step, its gutters included.
const GRID_CELL: u32 = GRID_SIDE + 2 * CELL_GUTTER;
/// Grid cells to a page's row at the last step.
const GRID_ROW: u32 = PAGE_SIDE / GRID_CELL;
/// Grid cells to a page at the last step.
const GRID_PAGE: u32 = GRID_ROW * GRID_ROW;

/// The picture extensions in the source's priority order.
pub const PICTURE_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "gif", "bmp", "png", "tga"];
/// The video extensions in the source's priority order.
pub const VIDEO_EXTENSIONS: [&str; 9] = [
    "mp4", "m4v", "wmv", "webm", "mpg", "mpeg", "m1v", "m2v", "avi",
];

/// What a `#BMPxx` name led to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BgaBinding {
    /// No readable file.
    Unresolved(Unresolved),
    /// The regular file `path` in the folder, observed as `key`; `video`
    /// when its extension is a video's.
    Resolved {
        path: PathBuf,
        key: FileKey,
        video: bool,
    },
}

impl BgaBinding {
    /// Whether `other` observes the same thing: the same absence, or the
    /// same file unchanged ([`FileKey::same_as`]).
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Unresolved(a), Self::Unresolved(b)) => a == b,
            (
                Self::Resolved {
                    path: p,
                    key: k,
                    video: v,
                },
                Self::Resolved {
                    path: q,
                    key: l,
                    video: w,
                },
            ) => p == q && v == w && k.same_as(l),
            _ => false,
        }
    }
}

/// The lowercase extension of `name`, if it has one.
fn extension(name: &str) -> Option<String> {
    let (_, ext) = name.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

/// `name` without its extension (all of it when it has none).
fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// Resolves the `#BMPxx` name `name` in `dir` by the source's format
/// priority (see the module) and observes the file it leads to, reading
/// nothing.
#[must_use]
pub fn resolve(dir: &ChartDir, name: &str, identity: Identity<'_>) -> BgaBinding {
    let name = name.trim();
    if name.len() > MAX_PATH_BYTES {
        return BgaBinding::Unresolved(Unresolved::Overlong);
    }
    let name = name.replace('\\', "/");
    // The first candidate that exists, or a refusal that ends the search.
    let first =
        |candidates: &mut dyn Iterator<Item = String>| -> Option<Result<PathBuf, Unresolved>> {
            for c in candidates {
                match path::resolve(dir.path(), &c) {
                    Ok((canonical, _)) => return Some(Ok(canonical)),
                    Err(PathError::Missing) => {}
                    Err(e) => return Some(Err(e.into())),
                }
            }
            None
        };
    let with = |exts: &'static [&'static str], base: &str| {
        let base = base.to_owned();
        exts.iter().map(move |e| format!("{base}.{e}"))
    };
    let found = match path::resolve(dir.path(), &name) {
        Ok((written, _)) => match extension(&name) {
            Some(ext) if VIDEO_EXTENSIONS.contains(&ext.as_str()) => {
                first(&mut with(&VIDEO_EXTENSIONS, stem(&name))).or(Some(Ok(written)))
            }
            Some(ext) if PICTURE_EXTENSIONS.contains(&ext.as_str()) => {
                first(&mut with(&PICTURE_EXTENSIONS, stem(&name))).or(Some(Ok(written)))
            }
            Some(_) => Some(Ok(written)),
            // A name with no extension is never read as written.
            None => None,
        },
        Err(PathError::Missing) => None,
        Err(e) => return BgaBinding::Unresolved(e.into()),
    };
    let found = found.or_else(|| {
        let base = stem(&name);
        first(&mut with(&VIDEO_EXTENSIONS, base).chain(with(&PICTURE_EXTENSIONS, base)))
    });
    let canonical = match found {
        Some(Ok(c)) => c,
        Some(Err(why)) => return BgaBinding::Unresolved(why),
        None => return BgaBinding::Unresolved(Unresolved::Missing),
    };
    let video = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(extension)
        .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.as_str()));
    match chart_image::observe(&canonical, identity) {
        Ok((_, key)) => BgaBinding::Resolved {
            path: canonical,
            key,
            video,
        },
        Err(e) => BgaBinding::Unresolved(e.into()),
    }
}

/// The size a picture of `native` texels is drawn at and met by its
/// stretch: the canvas for a small picture, else its own.
#[must_use]
pub fn drawn_size(native: [u32; 2]) -> [u32; 2] {
    if native[0].max(native[1]) <= CANVAS_SIDE {
        [CANVAS_SIDE, CANVAS_SIDE]
    } else {
        native
    }
}

/// The size a picture of `native` texels is kept at at full quality: its
/// drawn size fitted within [`MAX_CELL_SIDE`].
#[must_use]
pub fn stored_size(native: [u32; 2]) -> [u32; 2] {
    fitted_size(drawn_size(native), [MAX_CELL_SIDE, MAX_CELL_SIDE])
}

/// A step of the packing ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Every picture at its stored size.
    Full,
    /// The longer side capped at this many texels.
    Cap(u32),
    /// Every picture fitted to [`GRID_SIDE`] in a uniform grid.
    Grid,
}

impl Step {
    /// The content a picture stored at `stored` texels takes at this step.
    #[must_use]
    pub fn content(self, stored: [u32; 2]) -> [u32; 2] {
        match self {
            Self::Full => stored,
            Self::Cap(c) => fitted_size(stored, [c, c]),
            Self::Grid => fitted_size(stored, [GRID_SIDE, GRID_SIDE]),
        }
    }
}

/// Where a picture's cell lies: its page, the origin of its allocation
/// (its gutter included) and its content's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellPlace {
    pub page: u32,
    pub origin: [u32; 2],
    pub content: [u32; 2],
}

impl CellPlace {
    /// Its allocation's size: its content and a gutter on every side.
    #[must_use]
    pub fn padded(&self) -> [u32; 2] {
        self.content.map(|c| c + 2 * CELL_GUTTER)
    }

    /// Its content's uv on its page (never the gutter).
    #[must_use]
    pub fn uv(&self) -> FrameUv {
        let s = f64::from(PAGE_SIDE);
        let at = |v: u32| f32_of(f64::from(v) / s);
        let [x, y] = self.origin.map(|v| v + CELL_GUTTER);
        let [w, h] = self.content;
        FrameUv {
            uv: [at(x), at(y), at(x + w), at(y + h)],
            layer: u16::try_from(self.page).unwrap_or(u16::MAX),
        }
    }
}

/// A set's packing: the step it ended at, its page count, and each
/// picture's cell in the order given (`None` for an absent one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packing {
    pub step: Step,
    pub pages: u32,
    pub cells: Vec<Option<CellPlace>>,
}

/// Packs pictures stored at `stored` sizes (`None` for absent ones) into
/// at most `budget` pages (at least 2 for the last step): the first step
/// of the ladder (see the module) at which they fit.
#[must_use]
pub fn pack(stored: &[Option<[u32; 2]>], budget: u32) -> Packing {
    let present: Vec<(usize, [u32; 2])> = stored
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.map(|s| (i, s)))
        .collect();
    if present.is_empty() {
        return Packing {
            step: Step::Full,
            pages: 0,
            cells: vec![None; stored.len()],
        };
    }
    let steps = std::iter::once(Step::Full).chain(STEP_CAPS.iter().map(|&c| Step::Cap(c)));
    for step in steps {
        let contents: Vec<[u32; 2]> = present.iter().map(|&(_, s)| step.content(s)).collect();
        let padded: Vec<[u32; 2]> = contents
            .iter()
            .map(|c| c.map(|v| v + 2 * CELL_GUTTER))
            .collect();
        let order = pack::order(&padded);
        let Ok(Some(p)) =
            pack::pack_side_aligned(&padded, &order, PAGE_SIDE, budget, MAX_CELLS_PER_PAGE, 1)
        else {
            continue;
        };
        let mut cells = vec![None; stored.len()];
        for ((&(i, _), place), content) in present.iter().zip(&p.places).zip(&contents) {
            if let Some(c) = cells.get_mut(i) {
                *c = Some(CellPlace {
                    page: place.page,
                    origin: [place.alloc[0], place.alloc[1]],
                    content: *content,
                });
            }
        }
        return Packing {
            step,
            pages: p.pages,
            cells,
        };
    }
    let mut cells = vec![None; stored.len()];
    let mut k: u32 = 0;
    for &(i, s) in &present {
        let slot = k % GRID_PAGE;
        if let Some(c) = cells.get_mut(i) {
            *c = Some(CellPlace {
                page: k / GRID_PAGE,
                origin: [(slot % GRID_ROW) * GRID_CELL, (slot / GRID_ROW) * GRID_CELL],
                content: Step::Grid.content(s),
            });
        }
        k += 1;
    }
    Packing {
        step: Step::Grid,
        pages: k.div_ceil(GRID_PAGE),
        cells,
    }
}

/// Opens `canonical` and checks it is still the file observed as `key`.
fn open_unchanged(
    canonical: &Path,
    key: &FileKey,
    identity: Identity<'_>,
) -> Result<(File, u64), Absent> {
    let Ok((file, len)) = path::open(canonical) else {
        return Err(Absent::Changed);
    };
    unchanged(&file, key, identity)?;
    Ok((file, len))
}

fn unchanged(file: &File, key: &FileKey, identity: Identity<'_>) -> Result<(), Absent> {
    let meta = file.metadata().map_err(|_| Absent::Changed)?;
    let now = FileKey {
        len: meta.len(),
        modified: meta.modified().ok(),
        identity: identity(file),
    };
    if now.len == key.len && now.modified == key.modified && now.identity == key.identity {
        Ok(())
    } else {
        Err(Absent::Changed)
    }
}

fn absent(stop: Stop) -> Absent {
    match stop {
        Stop::Absent(a) => a,
        Stop::Changed => Absent::Changed,
    }
}

/// The size of the picture in the file at `canonical`, observed as `key`,
/// from its header: within the side and texel caps, and decodable whole.
///
/// # Errors
///
/// Why the picture is absent.
pub fn probe(canonical: &Path, key: &FileKey, identity: Identity<'_>) -> Result<[u32; 2], Absent> {
    let (mut file, len) = open_unchanged(canonical, key, identity)?;
    let probe = decode::header(&mut file, len).map_err(|e| absent(chart_image::absent_of(e)))?;
    let [w, h] = probe.dims;
    if w > MAX_SIDE || h > MAX_SIDE {
        return Err(Absent::OverCap(Cap::Side));
    }
    if u64::from(w) * u64::from(h) > MAX_IMAGE_TEXELS || !matches!(probe.mode, Ok(Mode::Full(_))) {
        return Err(Absent::OverCap(Cap::Texels));
    }
    if w == 0 || h == 0 {
        return Err(Absent::Decode);
    }
    Ok(probe.dims)
}

/// A cell's texels: premultiplied RGBA8 rows of `padded` texels, its
/// content [`CELL_GUTTER`] texels in.
#[derive(Clone, PartialEq, Eq)]
pub struct CellPixels {
    /// The picture's size as decoded.
    pub native: [u32; 2],
    /// The allocation's size, its gutter included.
    pub padded: [u32; 2],
    pub texels: Vec<u8>,
}

impl std::fmt::Debug for CellPixels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellPixels")
            .field("native", &self.native)
            .field("padded", &self.padded)
            .finish_non_exhaustive()
    }
}

/// An allocation of `bytes` within `limit`, `None` past it or when the
/// allocator refuses it.
fn alloc(bytes: u64, limit: Option<u64>) -> Result<Vec<u8>, Absent> {
    if limit.is_some_and(|l| bytes > l) {
        return Err(Absent::Refused);
    }
    let n = usize::try_from(bytes).map_err(|_| Absent::Refused)?;
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| Absent::Refused)?;
    v.resize(n, 0);
    Ok(v)
}

/// Reads the picture in the file at `canonical`, observed as `key` with
/// `native` texels ([`probe`]), into a cell whose content is `content`
/// (see the module). Every allocation of its own, the file's bytes and the
/// cell included, is at most `limit` bytes when one is given, and an
/// allocation past it or refused leaves the picture
/// [`Absent::Refused`]. `cancel` is asked before the read, before each
/// piece of it and after the decode.
///
/// Memory: the chart images' whole-decode bound, then the premultiplied
/// native texels (or the canvas) beside their downscale and the cell.
///
/// # Errors
///
/// Why the picture is absent.
pub fn read_cell(
    canonical: &Path,
    key: &FileKey,
    native: [u32; 2],
    content: [u32; 2],
    cancel: &dyn Fn() -> bool,
    identity: Identity<'_>,
    limit: Option<u64>,
) -> Result<CellPixels, Absent> {
    chart_image::checkpoint(cancel, Site::Read)?;
    let (mut file, len) = open_unchanged(canonical, key, identity)?;
    if limit.is_some_and(|l| len > l) {
        return Err(Absent::Refused);
    }
    let snapshot = chart_image::take(&mut file, len, cancel);
    if !matches!(snapshot, Err(Stop::Absent(Absent::Cancelled))) {
        unchanged(&file, key, identity)?;
    }
    drop(file);
    let (bytes, dims, header) = snapshot.map_err(absent)?;
    if dims != native {
        return Err(Absent::Changed);
    }
    decode::check_whole(&bytes, dims, header).map_err(|e| absent(chart_image::absent_of(e)))?;
    let image = decode::decode(bytes, dims).map_err(|e| absent(chart_image::absent_of(e)))?;
    chart_image::checkpoint(cancel, Site::Decoded)?;
    cell_of(image.into_raw(), dims, content, limit).map(|(padded, texels)| CellPixels {
        native: dims,
        padded,
        texels,
    })
}

/// The cell of a decoded picture of `dims` texels (straight RGBA8 rows in
/// `texels`) at `content`: premultiplied, on the canvas when small,
/// downscaled and given its gutter. Each allocation is at most `limit`.
fn cell_of(
    mut texels: Vec<u8>,
    dims: [u32; 2],
    content: [u32; 2],
    limit: Option<u64>,
) -> Result<([u32; 2], Vec<u8>), Absent> {
    for t in texels.as_chunks_mut::<4>().0 {
        let [r, g, b, a] = *t;
        *t = [premultiply(r, a), premultiply(g, a), premultiply(b, a), a];
    }
    let drawn = drawn_size(dims);
    let texels = if drawn == dims {
        texels
    } else {
        let mut canvas = alloc(u64::from(drawn[0]) * u64::from(drawn[1]) * 4, limit)?;
        place_on_canvas(&texels, dims, &mut canvas);
        drop(texels);
        canvas
    };
    let texels = if content == drawn {
        texels
    } else {
        let bytes = u64::from(content[0]) * u64::from(content[1]) * 4;
        if limit.is_some_and(|l| bytes > l) {
            return Err(Absent::Refused);
        }
        let out = chart_image::area(&texels, drawn, content).map_err(|_| Absent::Refused)?;
        drop(texels);
        out
    };
    let padded = content.map(|c| c + 2 * CELL_GUTTER);
    let bytes = u64::from(padded[0]) * u64::from(padded[1]) * 4;
    if limit.is_some_and(|l| bytes > l) {
        return Err(Absent::Refused);
    }
    let cell =
        chart_image::extrude(&texels, content, padded, CELL_GUTTER).map_err(|_| Absent::Refused)?;
    Ok((padded, cell))
}

/// Draws `src`, `dims` texels, at the top centre of `canvas`
/// ([`CANVAS_SIDE`] square, transparent): its left edge at
/// `⌊(256 − w) / 2⌋`, as the source draws it.
fn place_on_canvas(src: &[u8], dims: [u32; 2], canvas: &mut [u8]) {
    let [w, h] = dims.map(|v| usize::try_from(v).unwrap_or(0));
    let side = usize::try_from(CANVAS_SIDE).unwrap_or(0);
    let left = (side.saturating_sub(w)) / 2;
    for y in 0..h.min(side) {
        let from = y * w * 4;
        let to = (y * side + left) * 4;
        if let (Some(s), Some(d)) = (src.get(from..from + w * 4), canvas.get_mut(to..to + w * 4)) {
            d.copy_from_slice(s);
        }
    }
}

/// A picture's cell in the BGA array, and the size its stretch meets
/// ([`drawn_size`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BgaCell {
    pub uv: FrameUv,
    pub native: [u32; 2],
}

/// What a `#BMPxx` definition draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BgaEntry {
    /// Nothing could be prepared: drawn as the source draws a missing
    /// picture.
    Absent,
    /// A picture in the array.
    Picture(BgaCell),
    /// A video, whose frames come from the media provider.
    Video,
}

/// The BGA array a table describes: the renderer that wrote it and the
/// array's own serial. A table draws only in a frame whose live key is
/// equal ([`BgaTable::drawable`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BgaKey {
    pub renderer: u64,
    pub array: u64,
}

/// Each `#BMPxx` definition's entry, by [`BgaImageId`], and the array its
/// cells describe.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BgaTable {
    entries: Vec<BgaEntry>,
    key: Option<BgaKey>,
}

impl BgaTable {
    /// No entry, of no array: every definition absent.
    pub const ABSENT: Self = Self {
        entries: Vec::new(),
        key: None,
    };

    /// `entries` by definition, of no array until [`keyed`](Self::keyed).
    #[must_use]
    pub fn new(entries: Vec<BgaEntry>) -> Self {
        Self { entries, key: None }
    }

    /// The same table, describing the array `key` names.
    #[must_use]
    pub fn keyed(self, key: BgaKey) -> Self {
        Self {
            key: Some(key),
            ..self
        }
    }

    /// The array it describes, if any.
    #[must_use]
    pub const fn key(&self) -> Option<BgaKey> {
        self.key
    }

    /// Its entries, by definition.
    #[must_use]
    pub fn entries(&self) -> &[BgaEntry] {
        &self.entries
    }

    /// Whether a frame whose live key is `live` may draw it: both known
    /// and equal.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn drawable(&self, live: Option<BgaKey>) -> bool {
        live.is_some() && self.key == live
    }

    /// `id`'s entry: absent past the table.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn get(&self, id: BgaImageId) -> BgaEntry {
        self.entries
            .get(usize::from(id.0))
            .copied()
            .unwrap_or(BgaEntry::Absent)
    }
}

/// The table of no array, every definition absent, to borrow.
pub static NO_TABLE: BgaTable = BgaTable::ABSENT;

/// While set, each BGA element's draw allocates once (dev-tools: the
/// positive control of the frame path's allocation count).
#[cfg(feature = "dev-tools")]
pub static ALLOCATE_IN_DRAW: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// A shared empty table, for a play before any is set.
pub(crate) fn absent_table() -> Arc<BgaTable> {
    Arc::new(BgaTable::ABSENT)
}

/// The media provider standing in until videos are decoded: no video has
/// a frame, so each draws as a missing picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NullMedia;

impl NullMedia {
    /// The frame `id`'s video shows: none.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn frame(self, _id: BgaImageId) -> Option<BgaCell> {
        None
    }
}

/// Which part of a cropping stretch's picture stays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crop {
    /// 3: the picture's aspect, covering the destination, its overflow
    /// cropped.
    Outer,
    /// 5: the destination's width, the picture's aspect, its height
    /// cropped to the destination.
    FitWidth,
    /// 7: the destination's height, the picture's aspect, its width
    /// cropped to the destination.
    FitHeight,
    /// 10: the native size at the canvas's scale, cropped to the
    /// destination.
    NoResize,
}

/// How a BGA picture meets its destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgaStretch {
    /// A stretch that keeps the whole picture.
    Whole(ChartStretch),
    /// A stretch that crops it, at each frame.
    Crop(Crop),
    /// None given: the player's BGA expansion.
    Expand,
}

impl BgaStretch {
    /// The stretch of a destination's `stretch` mode
    /// (`skin/StretchType.java` at `0.8.9`): a negative or unknown mode
    /// is none.
    #[must_use]
    pub const fn of(mode: i32) -> Self {
        match mode {
            0 => Self::Whole(ChartStretch::Fill),
            1 => Self::Whole(ChartStretch::Inner),
            2 => Self::Whole(ChartStretch::Outer),
            3 => Self::Crop(Crop::Outer),
            4 => Self::Whole(ChartStretch::FitWidth),
            5 => Self::Crop(Crop::FitWidth),
            6 => Self::Whole(ChartStretch::FitHeight),
            7 => Self::Crop(Crop::FitHeight),
            8 => Self::Whole(ChartStretch::NoExpanding),
            9 => Self::Whole(ChartStretch::NoResize),
            10 => Self::Crop(Crop::NoResize),
            _ => Self::Expand,
        }
    }

    /// The player's BGA expansion: the picture's aspect inside the
    /// destination, until the BGA setting gives another.
    pub const EXPAND: ChartStretch = ChartStretch::Inner;

    /// The destination `rect` (canvas pixels) of a picture of `native`
    /// texels whose content is `uv`, at `scale` target pixels per canvas
    /// pixel: the rect drawn, the uv sampled, and the native size the quad
    /// keeps at placement. A cropping stretch keeps the part of the
    /// picture's centre the rect shows, exactly.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn place(
        self,
        rect: [f32; 4],
        native: [u32; 2],
        uv: FrameUv,
        scale: f32,
    ) -> ([f32; 4], FrameUv, Option<Native>) {
        let crop = match self {
            Self::Whole(s) => {
                let (r, n) = s.place(rect, native);
                return (r, uv, n);
            }
            Self::Expand => {
                let (r, n) = Self::EXPAND.place(rect, native);
                return (r, uv, n);
            }
            Self::Crop(c) => c,
        };
        let [x, y, w, h] = rect.map(f64::from);
        let [iw, ih] = native.map(f64::from);
        if iw <= 0.0 || ih <= 0.0 || w <= 0.0 || h <= 0.0 {
            return (rect, uv, None);
        }
        let (sx, sy) = (w / iw, h / ih);
        // The scale each axis is drawn at, canvas pixels per texel.
        let (kx, ky) = match crop {
            Crop::Outer => (sx.max(sy), sx.max(sy)),
            Crop::FitWidth => (sx, sx),
            Crop::FitHeight => (sy, sy),
            Crop::NoResize => {
                let k = if scale > 0.0 {
                    1.0 / f64::from(scale)
                } else {
                    1.0
                };
                (k, k)
            }
        };
        // Per axis: the picture at that scale centred on the rect, cropped
        // to it when it is larger: (start, length, kept fraction's start,
        // its end).
        let axis = |at: f64, len: f64, pixels: f64| {
            let drawn = pixels;
            if drawn > len {
                let keep = len / drawn;
                (at, len, (1.0 - keep) / 2.0, (1.0 + keep) / 2.0)
            } else {
                (at + (len - drawn) / 2.0, drawn, 0.0, 1.0)
            }
        };
        let (nx, nw, u0, u1) = axis(x, w, iw * kx);
        let (ny, nh, v0, v1) = axis(y, h, ih * ky);
        let [a, b, c, d] = uv.uv.map(f64::from);
        let lerp = |p: f64, q: f64, t: f64| f32_of(p + (q - p) * t);
        (
            [f32_of(nx), f32_of(ny), f32_of(nw), f32_of(nh)],
            FrameUv {
                uv: [
                    lerp(a, c, u0),
                    lerp(b, d, v0),
                    lerp(a, c, u1),
                    lerp(b, d, v1),
                ],
                layer: uv.layer,
            },
            None,
        )
    }
}

/// The built-in atlas's solid white texel, which the black placeholder is
/// drawn with: layer 0, the middle of its 8×8 block at the bottom right.
pub const SOLID: FrameUv = FrameUv {
    uv: [124.0 / 128.0; 4],
    layer: 0,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn uv() -> FrameUv {
        FrameUv {
            uv: [0.0, 0.0, 1.0, 1.0],
            layer: 3,
        }
    }

    #[test]
    fn stretch_3_crops_the_overflowing_axis_about_the_centre() {
        // A 200×100 picture in a 100×100 rect: height fits, width crops
        // to its middle half.
        let (r, f, n) = BgaStretch::of(3).place([10.0, 20.0, 100.0, 100.0], [200, 100], uv(), 1.0);
        assert_eq!(r, [10.0, 20.0, 100.0, 100.0]);
        assert_eq!(f.uv, [0.25, 0.0, 0.75, 1.0]);
        assert_eq!(f.layer, 3);
        assert_eq!(n, None);
    }

    #[test]
    fn stretch_5_centres_a_short_picture_and_crops_a_tall_one() {
        let (r, f, _) = BgaStretch::of(5).place([0.0, 0.0, 100.0, 100.0], [100, 50], uv(), 1.0);
        assert_eq!(r, [0.0, 25.0, 100.0, 50.0]);
        assert_eq!(f.uv, [0.0, 0.0, 1.0, 1.0]);
        let (r, f, _) = BgaStretch::of(5).place([0.0, 0.0, 100.0, 100.0], [100, 400], uv(), 1.0);
        assert_eq!(r, [0.0, 0.0, 100.0, 100.0]);
        assert_eq!(f.uv, [0.0, 0.375, 1.0, 0.625]);
    }

    #[test]
    fn stretch_10_keeps_the_native_size_and_crops_past_the_rect() {
        let (r, f, _) = BgaStretch::of(10).place([0.0, 0.0, 100.0, 100.0], [50, 400], uv(), 1.0);
        assert_eq!(r, [25.0, 0.0, 50.0, 100.0]);
        assert_eq!(f.uv, [0.0, 0.375, 1.0, 0.625]);
    }

    #[test]
    fn unset_and_unknown_modes_take_the_expansion() {
        assert_eq!(BgaStretch::of(-1), BgaStretch::Expand);
        assert_eq!(BgaStretch::of(11), BgaStretch::Expand);
    }
}
