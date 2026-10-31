//! A skin's files and image definitions, checked at load.
//!
//! [`Sheet::new`] checks every count before it touches the disk, then each
//! file's path and length, then each header, then each image's rectangle
//! and grid. It reads headers only: pixels are decoded when an atlas is
//! built ([`Sheet::atlas`]). [`Sheet::with_fonts`] adds the skin's font
//! files, checked by their first bytes only.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

pub(crate) mod builder;
pub(crate) mod decode;
pub(crate) mod font;
pub(crate) mod path;

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::TimeUs;

use crate::atlas::Variant;
use crate::quad::Filter;
use crate::skin::TimerRef;
use crate::track::MAX_KEY_US;
use decode::DecodeError;
use path::PathError;

/// The most files one skin may name.
pub const MAX_FILES: usize = 4_096;
/// The most image definitions one skin may hold.
pub const MAX_IMAGES: usize = 16_384;
/// The most frames (columns × rows) one image may hold.
pub const MAX_IMAGE_FRAMES: u32 = 1_024;
/// The most frames across one skin's images: the length of its uv table.
pub const MAX_SKIN_FRAMES: usize = 65_536;
/// The longest file name, in bytes.
pub const MAX_PATH_BYTES: usize = 1_024;
/// The largest file, in bytes, encoded.
pub const MAX_FILE_BYTES: u64 = 32 << 20;
/// The most bytes read to find a file's header.
pub const HEADER_BYTES: u64 = 1 << 20;
/// The most encoded bytes across one variant's files.
pub const MAX_TOTAL_FILE_BYTES: u64 = 512 << 20;
/// The largest width or height of a file.
pub const MAX_SIDE: u32 = 16_384;
/// The most texels of one file. With [`MAX_FILE_BYTES`] it bounds one
/// file's decoding at 96 MiB: the larger of 2 × 32 MiB encoded, 32 MiB
/// encoded + 8 B × 8 Mi native, and (8 + 4) B × 8 Mi native and RGBA8.
pub const MAX_IMAGE_TEXELS: u64 = 8 << 20;
/// The most texels across one variant's files.
pub const MAX_TOTAL_TEXELS: u64 = 128 << 20;

/// A file of the skin: a name relative to the skin folder, and an optional
/// file at exactly twice its size for high-resolution targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDef {
    pub path: String,
    pub hi: Option<String>,
}

/// A frame cycle: every frame in turn over `period`, on the image's own
/// timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CycleDef {
    /// All frames, 1 µs to [`MAX_KEY_US`].
    pub period: TimeUs,
    pub timer: TimerRef,
}

/// An image: a rectangle of a file cut into a grid of frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageDef {
    /// An index into the files.
    pub file: u32,
    /// Left, top, width, height in the file's 1x pixels; `None` is the
    /// whole file.
    pub rect: Option<[u32; 4]>,
    /// Columns, rows. Frames run row by row; a remainder of the rectangle
    /// is not part of any frame.
    pub grid: [u16; 2],
    pub cycle: Option<CycleDef>,
    pub filter: Filter,
}

/// A file of a sheet, or its high-resolution version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileAt {
    pub file: usize,
    pub hi: bool,
}

impl fmt::Display for FileAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.hi {
            write!(f, "file {}'s @2x version", self.file)
        } else {
            write!(f, "file {}", self.file)
        }
    }
}

/// Why a sheet was refused or a file could not be read; `image` is an
/// index into the image definitions.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("the skin names {n} files, more than {MAX_FILES}")]
    TooManyFiles { n: usize },
    #[error("the skin has {n} images, more than {MAX_IMAGES}")]
    TooManyImages { n: usize },
    #[error("image {image}'s file does not exist")]
    UnknownFile { image: usize },
    #[error("image {image} has no columns or no rows")]
    EmptyGrid { image: usize },
    #[error("image {image} has {n} frames, more than {MAX_IMAGE_FRAMES}")]
    TooManyImageFrames { image: usize, n: u32 },
    #[error("the skin's images have more than {MAX_SKIN_FRAMES} frames")]
    TooManySkinFrames,
    #[error("image {image}'s cycle period is outside 1 µs to {MAX_KEY_US} µs")]
    CycleOutOfRange { image: usize },
    #[error("the skin folder")]
    Root(#[source] io::Error),
    #[error("{at}'s name may leave the skin folder")]
    PathRejected { at: FileAt },
    #[error("{at} does not exist")]
    Missing { at: FileAt },
    #[error("{at} resolves outside the skin folder")]
    PathEscapes { at: FileAt },
    #[error("{at} is not a regular file")]
    NotAFile { at: FileAt },
    #[error("{at} is larger than {MAX_FILE_BYTES} bytes")]
    FileTooLarge { at: FileAt },
    #[error("the files up to {at} pass {MAX_TOTAL_FILE_BYTES} bytes")]
    TotalBytesTooLarge { at: FileAt },
    #[error("{at}'s header is not within its first bytes")]
    HeaderTooLarge { at: FileAt },
    #[error("{at} is not a PNG, JPEG or BMP image")]
    Unsupported { at: FileAt },
    #[error("{at} has no pixels")]
    EmptyFile { at: FileAt },
    #[error("{at} is {w}×{h}, wider or taller than {MAX_SIDE}")]
    TooWide { at: FileAt, w: u32, h: u32 },
    #[error("{at} has more than {MAX_IMAGE_TEXELS} texels")]
    TooManyTexels { at: FileAt },
    #[error("the files up to {at} pass {MAX_TOTAL_TEXELS} texels")]
    TotalTooLarge { at: FileAt },
    #[error("file {file}'s @2x version is not exactly twice its size")]
    HiSizeMismatch { file: usize },
    #[error("image {image}'s rectangle is outside its file")]
    RectOutside { image: usize },
    #[error("image {image}'s frames are empty")]
    EmptyCell { image: usize },
    #[error("{at}'s dimensions changed since the skin was loaded")]
    DimensionsChanged { at: FileAt },
    #[error("{at}")]
    Decode {
        at: FileAt,
        #[source]
        source: ::image::ImageError,
    },
    #[error("{at}")]
    Io {
        at: FileAt,
        #[source]
        source: io::Error,
    },
}

impl ImageError {
    pub(crate) fn path(at: FileAt, e: PathError) -> Self {
        match e {
            PathError::Rejected => Self::PathRejected { at },
            PathError::Missing => Self::Missing { at },
            PathError::Escapes => Self::PathEscapes { at },
            PathError::NotAFile => Self::NotAFile { at },
            PathError::TooLarge => Self::FileTooLarge { at },
            PathError::Io(source) => Self::Io { at, source },
        }
    }

    pub(crate) fn decode(at: FileAt, e: DecodeError) -> Self {
        match e {
            DecodeError::HeaderTooLarge => Self::HeaderTooLarge { at },
            DecodeError::Unsupported => Self::Unsupported { at },
            DecodeError::Changed => Self::DimensionsChanged { at },
            DecodeError::Image(source) => Self::Decode { at, source },
            DecodeError::Io(source) => Self::Io { at, source },
        }
    }
}

/// One version of a file: its name and the dimensions its header gave at
/// load.
#[derive(Clone, Debug)]
pub(crate) struct Version {
    pub name: String,
    pub dims: [u32; 2],
}

#[derive(Clone, Debug)]
pub(crate) struct File {
    pub lo: Version,
    pub hi: Option<Version>,
}

impl File {
    /// The version a variant reads, and whether it is the @2x one.
    pub(crate) fn version(&self, variant: Variant) -> (&Version, bool) {
        match (variant, &self.hi) {
            (Variant::Hi, Some(hi)) => (hi, true),
            _ => (&self.lo, false),
        }
    }
}

/// An image as the skin draws it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Image {
    /// Its first frame's index in the uv table.
    pub first: u32,
    pub frames: u16,
    pub cycle: Option<CycleDef>,
    pub filter: Filter,
}

/// A rectangle of a file packed once, however many frames show it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Cell {
    pub file: u32,
    /// Left, top, width, height in 1x pixels.
    pub rect: [u32; 4],
}

static NEXT_SHEET: AtomicU64 = AtomicU64::new(1);

/// A skin's files and images, checked against the disk at load.
#[derive(Clone, Debug)]
pub struct Sheet {
    id: u64,
    root: PathBuf,
    files: Box<[File]>,
    images: Box<[Image]>,
    /// Per frame, in uv-table order: its cell.
    frames: Box<[u32]>,
    /// The distinct cells, ordered by file, then rectangle.
    cells: Box<[Cell]>,
    /// The font files' names.
    fonts: Box<[String]>,
}

/// Image `image`'s frame count: its grid has a column and a row, and at
/// most [`MAX_IMAGE_FRAMES`] frames.
fn image_frames(image: usize, def: &ImageDef) -> Result<u32, ImageError> {
    let [cols, rows] = def.grid;
    if cols == 0 || rows == 0 {
        return Err(ImageError::EmptyGrid { image });
    }
    let n = u32::from(cols) * u32::from(rows);
    if n > MAX_IMAGE_FRAMES {
        return Err(ImageError::TooManyImageFrames { image, n });
    }
    Ok(n)
}

/// `total` frames and `n` more, at most [`MAX_SKIN_FRAMES`].
fn add_frames(total: usize, n: u32) -> Result<usize, ImageError> {
    let total = total.saturating_add(usize::try_from(n).unwrap_or(usize::MAX));
    if total > MAX_SKIN_FRAMES {
        return Err(ImageError::TooManySkinFrames);
    }
    Ok(total)
}

/// A frame total, saturating.
fn frame_total(images: &[ImageDef]) -> Result<usize, ImageError> {
    let mut total = 0_usize;
    for (image, def) in images.iter().enumerate() {
        total = add_frames(total, image_frames(image, def)?)?;
    }
    Ok(total)
}

/// Whether image `image`'s cycle period is 1 µs to [`MAX_KEY_US`].
fn check_cycle(image: usize, def: &ImageDef) -> Result<(), ImageError> {
    if def
        .cycle
        .is_some_and(|c| c.period.0 < 1 || c.period.0 > MAX_KEY_US)
    {
        return Err(ImageError::CycleOutOfRange { image });
    }
    Ok(())
}

/// Running totals for one variant, checked against a cap.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Total {
    pub(crate) sum: u64,
    pub(crate) cap: u64,
}

impl Total {
    pub(crate) fn add(&mut self, n: u64) -> bool {
        self.sum = self.sum.saturating_add(n);
        self.sum <= self.cap
    }

    /// The total with `n` more, if it stays within the cap; `self` is
    /// unchanged.
    pub(crate) fn with(self, n: u64) -> Option<Self> {
        let mut next = self;
        next.add(n).then_some(next)
    }

    /// No encoded bytes yet, against [`MAX_TOTAL_FILE_BYTES`].
    pub(crate) const BYTES: Self = Self {
        sum: 0,
        cap: MAX_TOTAL_FILE_BYTES,
    };

    /// No texels yet, against [`MAX_TOTAL_TEXELS`].
    pub(crate) const TEXELS: Self = Self {
        sum: 0,
        cap: MAX_TOTAL_TEXELS,
    };
}

/// A file's name resolved inside the canonical skin folder `root`, with
/// its length: the metadata checks, before any header is read.
fn meta(root: &Path, at: FileAt, name: &str) -> Result<(PathBuf, u64), ImageError> {
    path::resolve(root, name).map_err(|e| ImageError::path(at, e))
}

/// One file's versions resolved ([`meta`]); no hi path when there is no
/// hi version, whose variant then reads the lo one.
struct Resolved {
    lo: PathBuf,
    hi: Option<PathBuf>,
}

/// Resolves file `file`'s versions and checks the running byte totals,
/// returning the totals with this file added.
fn resolve_file(
    root: &Path,
    file: usize,
    def: &FileDef,
    lo_bytes: Total,
    hi_bytes: Total,
) -> Result<(Resolved, Total, Total), ImageError> {
    let lo_at = FileAt { file, hi: false };
    let (lo, lo_len) = meta(root, lo_at, &def.path)?;
    let hi = match &def.hi {
        Some(name) => Some(meta(root, FileAt { file, hi: true }, name)?),
        None => None,
    };
    let lo_bytes = lo_bytes
        .with(lo_len)
        .ok_or(ImageError::TotalBytesTooLarge { at: lo_at })?;
    let hi_len = hi.as_ref().map_or(lo_len, |&(_, len)| len);
    let hi_at = FileAt {
        file,
        hi: hi.is_some(),
    };
    let hi_bytes = hi_bytes
        .with(hi_len)
        .ok_or(ImageError::TotalBytesTooLarge { at: hi_at })?;
    let resolved = Resolved {
        lo,
        hi: hi.map(|(p, _)| p),
    };
    Ok((resolved, lo_bytes, hi_bytes))
}

/// Reads file `file`'s headers (lo, then hi at exactly twice its size)
/// and checks the running texel totals, returning the file and the
/// totals with it added.
fn read_file(
    file: usize,
    def: FileDef,
    paths: &Resolved,
    lo_texels: Total,
    hi_texels: Total,
) -> Result<(File, Total, Total), ImageError> {
    let lo_at = FileAt { file, hi: false };
    let lo_dims = checked_header(lo_at, &paths.lo)?;
    let lo_texels = lo_texels
        .with(texels(lo_dims))
        .ok_or(ImageError::TotalTooLarge { at: lo_at })?;
    let hi = match (def.hi, &paths.hi) {
        (Some(name), Some(hi_path)) => {
            let dims = checked_header(FileAt { file, hi: true }, hi_path)?;
            let [w, h] = lo_dims;
            if dims != [w * 2, h * 2] {
                return Err(ImageError::HiSizeMismatch { file });
            }
            Some(Version { name, dims })
        }
        _ => None,
    };
    let hi_dims = hi.as_ref().map_or(lo_dims, |v| v.dims);
    let hi_at = FileAt {
        file,
        hi: hi.is_some(),
    };
    let hi_texels = hi_texels
        .with(texels(hi_dims))
        .ok_or(ImageError::TotalTooLarge { at: hi_at })?;
    let loaded = File {
        lo: Version {
            name: def.path,
            dims: lo_dims,
        },
        hi,
    };
    Ok((loaded, lo_texels, hi_texels))
}

/// Image `image`'s rectangle in its file of 1x size `dims` and its cell
/// size: the rectangle inside the file and the cells not empty.
fn cut(image: usize, def: &ImageDef, dims: [u32; 2]) -> Result<([u32; 4], [u32; 2]), ImageError> {
    let [fw, fh] = dims;
    let rect = def.rect.unwrap_or([0, 0, fw, fh]);
    let [x, y, w, h] = rect;
    let inside = |at: u32, len: u32, max: u32| at.checked_add(len).is_some_and(|e| e <= max);
    if !inside(x, w, fw) || !inside(y, h, fh) {
        return Err(ImageError::RectOutside { image });
    }
    let [cols, rows] = def.grid;
    let (cw, ch) = (w / u32::from(cols), h / u32::from(rows));
    if cw == 0 || ch == 0 {
        return Err(ImageError::EmptyCell { image });
    }
    Ok((rect, [cw, ch]))
}

/// Appends a checked image's frames, row by row, each its cell's id in
/// first-sight order, and returns how the skin draws it.
fn push_frames(
    def: &ImageDef,
    [x, y, ..]: [u32; 4],
    [cw, ch]: [u32; 2],
    cell_ids: &mut BTreeMap<Cell, u32>,
    frames: &mut Vec<u32>,
) -> Image {
    let first = u32::try_from(frames.len()).unwrap_or(u32::MAX);
    let [cols, rows] = def.grid;
    for r in 0..u32::from(rows) {
        for c in 0..u32::from(cols) {
            let cell = Cell {
                file: def.file,
                rect: [x + c * cw, y + r * ch, cw, ch],
            };
            let next = u32::try_from(cell_ids.len()).unwrap_or(u32::MAX);
            frames.push(*cell_ids.entry(cell).or_insert(next));
        }
    }
    Image {
        first,
        frames: u16::try_from(u32::from(cols) * u32::from(rows)).unwrap_or(u16::MAX),
        cycle: def.cycle,
        filter: def.filter,
    }
}

/// The distinct cells in cell order, with `frames` renumbered from
/// first-sight ids to that order, so the atlas reads each file once, in
/// file order.
fn renumber(cell_ids: BTreeMap<Cell, u32>, frames: &mut [u32]) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(cell_ids.len());
    let mut renumber = vec![0_u32; cell_ids.len()];
    for (sorted, (cell, id)) in (0_u32..).zip(cell_ids) {
        if let Some(slot) = usize::try_from(id).ok().and_then(|i| renumber.get_mut(i)) {
            *slot = sorted;
        }
        cells.push(cell);
    }
    for f in frames {
        if let Some(&n) = usize::try_from(*f).ok().and_then(|i| renumber.get(i)) {
            *f = n;
        }
    }
    cells
}

#[cfg(test)]
thread_local! {
    /// The headers this thread has read: a witness that metadata checks
    /// come first and that a file is read once.
    pub(crate) static HEADER_READS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The header of a resolved file, checked: non-empty, at most
/// [`MAX_SIDE`] per side and [`MAX_IMAGE_TEXELS`] in all.
fn checked_header(at: FileAt, path: &Path) -> Result<[u32; 2], ImageError> {
    #[cfg(test)]
    HEADER_READS.with(|n| n.set(n.get() + 1));
    let (mut file, len) = path::open(path).map_err(|e| ImageError::path(at, e))?;
    let dims = decode::header(&mut file, len).map_err(|e| ImageError::decode(at, e))?;
    let [w, h] = dims;
    if w == 0 || h == 0 {
        return Err(ImageError::EmptyFile { at });
    }
    if w > MAX_SIDE || h > MAX_SIDE {
        return Err(ImageError::TooWide { at, w, h });
    }
    if u64::from(w) * u64::from(h) > MAX_IMAGE_TEXELS {
        return Err(ImageError::TooManyTexels { at });
    }
    Ok(dims)
}

fn texels([w, h]: [u32; 2]) -> u64 {
    u64::from(w) * u64::from(h)
}

impl Sheet {
    /// A sheet with no files and no images.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            id: NEXT_SHEET.fetch_add(1, Ordering::Relaxed),
            root: PathBuf::new(),
            files: Box::new([]),
            images: Box::new([]),
            frames: Box::new([]),
            cells: Box::new([]),
            fonts: Box::new([]),
        }
    }

    /// Loads `files` and `images` against the skin folder `root`.
    ///
    /// Checks, in order, nothing growing with the input before its cap:
    /// the file and image counts; each image's file index, grid and frame
    /// count, the frames across images, and each cycle's period; then the
    /// folder; then each file and its @2x version: its name, its path
    /// inside the folder, a regular file of at most [`MAX_FILE_BYTES`], and
    /// each variant's running byte total; then each header (at most
    /// [`HEADER_BYTES`] read): its format, its size, each variant's
    /// running texel total, and an @2x version at exactly twice the size;
    /// then each image's rectangle inside its file and a non-empty frame.
    ///
    /// These counts check what was built. A front end enforces the file,
    /// image and frame limits cumulatively while it builds the input.
    ///
    /// # Errors
    ///
    /// The first check that fails.
    pub fn new(
        root: &Path,
        files: Vec<FileDef>,
        images: Vec<ImageDef>,
    ) -> Result<Self, ImageError> {
        if files.len() > MAX_FILES {
            return Err(ImageError::TooManyFiles { n: files.len() });
        }
        if images.len() > MAX_IMAGES {
            return Err(ImageError::TooManyImages { n: images.len() });
        }
        if let Some(image) = images
            .iter()
            .position(|d| usize::try_from(d.file).map_or(true, |f| f >= files.len()))
        {
            return Err(ImageError::UnknownFile { image });
        }
        let frame_count = frame_total(&images)?;
        for (image, def) in images.iter().enumerate() {
            check_cycle(image, def)?;
        }
        let root = root.canonicalize().map_err(ImageError::Root)?;

        // Paths and lengths, before any header is read.
        let mut paths = Vec::with_capacity(files.len());
        let (mut lo_bytes, mut hi_bytes) = (Total::BYTES, Total::BYTES);
        for (file, def) in files.iter().enumerate() {
            let (resolved, lo, hi) = resolve_file(&root, file, def, lo_bytes, hi_bytes)?;
            (lo_bytes, hi_bytes) = (lo, hi);
            paths.push(resolved);
        }

        // Headers.
        let (mut lo_texels, mut hi_texels) = (Total::TEXELS, Total::TEXELS);
        let mut loaded = Vec::with_capacity(files.len());
        for (file, (def, resolved)) in files.into_iter().zip(paths).enumerate() {
            let (f, lo, hi) = read_file(file, def, &resolved, lo_texels, hi_texels)?;
            (lo_texels, hi_texels) = (lo, hi);
            loaded.push(f);
        }

        // Rectangles, frames and their cells.
        let mut cell_ids = BTreeMap::new();
        let mut frames = Vec::with_capacity(frame_count);
        let mut drawn = Vec::with_capacity(images.len());
        for (image, def) in images.iter().enumerate() {
            let Some(file) = usize::try_from(def.file).ok().and_then(|f| loaded.get(f)) else {
                return Err(ImageError::UnknownFile { image });
            };
            let (rect, cell) = cut(image, def, file.lo.dims)?;
            drawn.push(push_frames(def, rect, cell, &mut cell_ids, &mut frames));
        }
        let cells = renumber(cell_ids, &mut frames);
        Ok(Self {
            id: NEXT_SHEET.fetch_add(1, Ordering::Relaxed),
            root,
            files: loaded.into_boxed_slice(),
            images: drawn.into_boxed_slice(),
            frames: frames.into_boxed_slice(),
            cells: cells.into_boxed_slice(),
            fonts: Box::new([]),
        })
    }

    /// The frames across all images: the length of every uv table built
    /// from this sheet.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// The image definitions.
    #[must_use]
    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    /// An identity no other sheet in this process has; a uv table carries
    /// its sheet's.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn files(&self) -> &[File] {
        &self.files
    }

    pub(crate) fn images(&self) -> &[Image] {
        &self.images
    }

    pub(crate) fn frames(&self) -> &[u32] {
        &self.frames
    }

    pub(crate) fn cells(&self) -> &[Cell] {
        &self.cells
    }
}
