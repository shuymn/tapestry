//! A chart's own images: the stage file and back image a chart's header
//! names (system images 100 and 101 of beatoraja skins), read from the
//! chart's folder and fitted into a slot of an atlas layer (docs/stage3.md
//! S10-12). S15 reads BGA images through the same path, with its own
//! extension fallbacks and slots.
//!
//! - **Confinement** (design.md "Threat Model", T08: a chart's files are
//!   restricted to its folder). A name is checked for length before it is
//!   copied, then by the skin names' rules ([`crate::image::path`]):
//!   relative, no drive or stream colon, no `..`, at most
//!   [`MAX_PATH_BYTES`]. Its canonical path must lie inside the canonical
//!   chart folder, and it must be a regular file of at most
//!   [`MAX_FILE_BYTES`], checked before it is opened and again on the
//!   handle. Any refusal makes the image absent; it never fails the chart.
//! - **Caps**: the skin images' header, side and per-file byte caps, and a
//!   whole decode of at most [`MAX_IMAGE_TEXELS`]. A larger image is absent
//!   (no banded path: a slot needs a downscale while streaming). PNG, JPEG
//!   and BMP only.
//! - **Freshness** (design.md "Load Pipeline"): a read returns the
//!   observation whose bytes it decoded ([`Read::binding`]), checked on the
//!   handle before and after reading; a difference starts again from
//!   resolution once. Two observations match ([`Binding::same_as`]) only
//!   when both file identities are available and equal.
//! - **Pixels** (design.md "Rendering and Skins", Scaling quality): the
//!   decoded texels are premultiplied before any filtering, downscaled by
//!   an area filter when they do not fit the slot, placed [`GUTTER`]
//!   texels into an allocation aligned to [`ALIGN`] whose other texels
//!   repeat the nearest content texel, and given the atlas's box-filtered
//!   mip levels ([`downsample_rgba`]).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fs::{File, Metadata};
use std::io::{self, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::RgbaImage;

use crate::atlas::uv::FrameUv;
use crate::atlas::{ALIGN, GUTTER, MAX_PAGE_SIDE, MIP_LEVELS, downsample_rgba, premultiply};
use crate::canvas::f32_of;
use crate::image::decode::{self, DecodeError, Mode, Site};
use crate::image::path::{self, PathError};
use crate::image::{MAX_FILE_BYTES, MAX_IMAGE_TEXELS, MAX_PATH_BYTES, MAX_SIDE};
use crate::place::Native;

/// A chart's folder, canonical: every name is resolved inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartDir {
    dir: PathBuf,
}

impl ChartDir {
    /// The canonical form of `dir`.
    ///
    /// # Errors
    ///
    /// The folder cannot be made canonical (it does not exist, or cannot
    /// be read).
    pub fn new(dir: &Path) -> io::Result<Self> {
        Ok(Self {
            dir: dir.canonicalize()?,
        })
    }

    /// The canonical folder.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

/// A file's identity on its volume (the NTFS file id, or a device and
/// inode), as the caller's platform gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity(pub u128);

/// How a caller gives an open file's identity; `None` when the platform
/// cannot.
pub type Identity<'a> = &'a dyn Fn(&File) -> Option<FileIdentity>;

/// What a file looked like when it was observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileKey {
    /// Its length in bytes.
    pub len: u64,
    /// Its modification time, when the platform gives one.
    pub modified: Option<SystemTime>,
    /// Its identity, when the caller could give one.
    pub identity: Option<FileIdentity>,
}

impl FileKey {
    fn of(meta: &Metadata, file: &File, identity: Identity<'_>) -> Self {
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            identity: identity(file),
        }
    }

    /// Whether `other` is a verified observation of the same unchanged
    /// file: equal length and modification time, and both identities
    /// available and equal.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        self.len == other.len
            && self.modified == other.modified
            && self.identity.is_some()
            && self.identity == other.identity
    }
}

/// Why a name did not lead to a file that may be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unresolved {
    #[error("the name is longer than {MAX_PATH_BYTES} bytes")]
    Overlong,
    #[error("the name is not a relative path inside the chart folder")]
    Rejected,
    #[error("the name resolves outside the chart folder")]
    Escapes,
    #[error("no such file")]
    Missing,
    #[error("not a regular file")]
    NotAFile,
    #[error("the file is larger than {MAX_FILE_BYTES} bytes")]
    TooLarge,
    #[error("the file cannot be read: {0}")]
    Io(io::ErrorKind),
}

impl From<PathError> for Unresolved {
    fn from(e: PathError) -> Self {
        match e {
            PathError::Rejected => Self::Rejected,
            PathError::Missing => Self::Missing,
            PathError::Escapes => Self::Escapes,
            PathError::NotAFile => Self::NotAFile,
            PathError::TooLarge => Self::TooLarge,
            PathError::Io(e) => Self::Io(e.kind()),
        }
    }
}

/// One observation of what a header's name leads to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// The header is absent or empty: nothing was read.
    NoHeader,
    /// The name leads to no readable file. `name` is the name as kept, or
    /// empty for an overlong one, which is never copied.
    Unresolved { name: String, why: Unresolved },
    /// The name leads to the regular file `path` inside the folder.
    Resolved {
        name: String,
        path: PathBuf,
        key: FileKey,
    },
}

impl Binding {
    /// Whether `other` observes the same thing, so what was read for one
    /// may be reused for the other: the same absence, or the same file
    /// unchanged ([`FileKey::same_as`]).
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::NoHeader, Self::NoHeader) => true,
            (Self::Unresolved { name: a, why: x }, Self::Unresolved { name: b, why: y }) => {
                a == b && x == y
            }
            (
                Self::Resolved {
                    name: a,
                    path: p,
                    key: k,
                },
                Self::Resolved {
                    name: b,
                    path: q,
                    key: l,
                },
            ) => a == b && p == q && k.same_as(l),
            _ => false,
        }
    }
}

/// Which cap an image exceeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    /// A side past [`MAX_SIDE`].
    Side,
    /// More than [`MAX_IMAGE_TEXELS`] texels.
    Texels,
}

/// Why an image is absent: the source draws nothing for each.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Absent {
    #[error("no header names an image")]
    NoHeader,
    #[error(transparent)]
    Unresolved(Unresolved),
    #[error("the image exceeds the {0:?} cap")]
    OverCap(Cap),
    #[error("the image is not a PNG, JPEG or BMP")]
    Format,
    #[error("the image cannot be decoded")]
    Decode,
    #[error("the file changed while it was read")]
    Changed,
    #[error("the read was cancelled")]
    Cancelled,
}

/// A slot of an atlas layer an image is fitted into, its gutters
/// included: each side a multiple of [`ALIGN`], at least
/// `2 × GUTTER + ALIGN` and at most [`MAX_PAGE_SIDE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    size: [u32; 2],
}

impl Slot {
    /// The slot of `size` texels, gutters included; `None` when a side is
    /// not a multiple of [`ALIGN`], below `2 × GUTTER + ALIGN`, or past
    /// [`MAX_PAGE_SIDE`].
    #[must_use]
    pub fn new(size: [u32; 2]) -> Option<Self> {
        size.iter()
            .all(|&s| s % ALIGN == 0 && (2 * GUTTER + ALIGN..=MAX_PAGE_SIDE).contains(&s))
            .then_some(Self { size })
    }

    /// Its size, gutters included.
    #[must_use]
    pub fn size(self) -> [u32; 2] {
        self.size
    }

    /// The largest content it holds: its size less a gutter on each side.
    #[must_use]
    pub fn max_content(self) -> [u32; 2] {
        self.size.map(|s| s - 2 * GUTTER)
    }
}

/// An image fitted into a slot, premultiplied, with its mip levels.
#[derive(Clone, PartialEq, Eq)]
pub struct Fitted {
    native: [u32; 2],
    content: [u32; 2],
    padded: [u32; 2],
    levels: Box<[Box<[u8]>]>,
}

impl std::fmt::Debug for Fitted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fitted")
            .field("native", &self.native)
            .field("content", &self.content)
            .field("padded", &self.padded)
            .finish_non_exhaustive()
    }
}

impl Fitted {
    /// The image's own size, for stretch.
    #[must_use]
    pub fn native(&self) -> [u32; 2] {
        self.native
    }

    /// The content's size: the native size, or the downscaled one. It sits
    /// [`GUTTER`] texels into the allocation on each axis.
    #[must_use]
    pub fn content(&self) -> [u32; 2] {
        self.content
    }

    /// The whole allocation, gutters included:
    /// `align_up(content, ALIGN) + 2 × GUTTER` on each axis, at most the
    /// slot. Level 0 is uploaded at the slot's origin.
    #[must_use]
    pub fn padded(&self) -> [u32; 2] {
        self.padded
    }

    /// Level `k`'s size, `padded >> k`; `None` past the levels.
    #[must_use]
    pub fn level_size(&self, k: usize) -> Option<[u32; 2]> {
        (k < MIP_LEVELS).then(|| self.padded.map(|s| s >> k))
    }

    /// The [`MIP_LEVELS`] levels, premultiplied RGBA8 rows tightly packed:
    /// level `k` is [`level_size`](Self::level_size)`(k)`.
    #[must_use]
    pub fn levels(&self) -> &[Box<[u8]>] {
        &self.levels
    }
}

/// What a read observed and what it made of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read {
    /// The final verified observation: the one whose bytes `image` holds,
    /// or whose failure it reports.
    pub binding: Binding,
    pub image: Result<Fitted, Absent>,
}

/// Resolves the name `label` (a chart header's value) in `dir`: the name
/// as written, then with each of `fallbacks` as its extension; the first
/// candidate that exists decides. Opens it to observe its identity, but
/// reads nothing.
#[must_use]
pub fn resolve(dir: &ChartDir, label: &str, fallbacks: &[&str], identity: Identity<'_>) -> Binding {
    let name = label.trim();
    if name.is_empty() {
        return Binding::NoHeader;
    }
    if name.len() > MAX_PATH_BYTES {
        return Binding::Unresolved {
            name: String::new(),
            why: Unresolved::Overlong,
        };
    }
    let unresolved = |why| Binding::Unresolved {
        name: name.to_owned(),
        why,
    };
    let slashed = name.replace('\\', "/");
    let candidates = std::iter::once(name.to_owned()).chain(fallbacks.iter().filter_map(|ext| {
        let alt = Path::new(&slashed).with_extension(ext).to_str()?.to_owned();
        (alt != slashed).then_some(alt)
    }));
    for candidate in candidates {
        let canonical = match path::resolve(&dir.dir, &candidate) {
            Ok((canonical, _)) => canonical,
            Err(PathError::Missing) => continue,
            Err(e) => return unresolved(e.into()),
        };
        return match observe(&canonical, identity) {
            Ok((_, key)) => Binding::Resolved {
                name: name.to_owned(),
                path: canonical,
                key,
            },
            Err(e) => unresolved(e.into()),
        };
    }
    unresolved(Unresolved::Missing)
}

/// Opens `canonical`, checked again as a regular file within the byte cap
/// on the handle, and observes it.
fn observe(canonical: &Path, identity: Identity<'_>) -> Result<(File, FileKey), PathError> {
    let (file, _) = path::open(canonical)?;
    let meta = file.metadata().map_err(PathError::Io)?;
    let key = FileKey::of(&meta, &file, identity);
    Ok((file, key))
}

/// Why one attempt at reading a resolved file stopped.
enum Stop {
    Absent(Absent),
    /// The file is not the one resolved: start again.
    Changed,
}

impl From<Absent> for Stop {
    fn from(a: Absent) -> Self {
        Self::Absent(a)
    }
}

/// Reads the image `label` names in `dir` and fits it into `slot`.
///
/// Resolves as [`resolve`]; opens the file and checks its key on the
/// handle; reads its header within the side and texel caps, then the
/// whole file; checks the key again; decodes it; and fits it (see the
/// module). A key that differs starts again from resolution once; a
/// second difference is [`Absent::Changed`]. The returned binding is the
/// last observation, the one the image was read from.
///
/// `cancel` is asked before the file is opened, before each piece of the
/// read, after the decode, after the fit and before each mip level.
///
/// Memory: the skin images' whole-decode bound (at most 64 MiB encoded
/// and 12 Mi texels, see [`decode`](crate::image::decode)), then the
/// premultiplied native texels beside the fitted output and its levels.
#[must_use]
pub fn read(
    dir: &ChartDir,
    label: &str,
    fallbacks: &[&str],
    slot: Slot,
    cancel: &dyn Fn() -> bool,
    identity: Identity<'_>,
) -> Read {
    let mut retried = false;
    loop {
        let binding = resolve(dir, label, fallbacks, identity);
        let (canonical, key) = match &binding {
            Binding::NoHeader => {
                return Read {
                    binding,
                    image: Err(Absent::NoHeader),
                };
            }
            Binding::Unresolved { why, .. } => {
                let why = *why;
                return Read {
                    binding,
                    image: Err(Absent::Unresolved(why)),
                };
            }
            Binding::Resolved { path, key, .. } => (path.clone(), key.clone()),
        };
        match read_resolved(&canonical, &key, slot, cancel, identity) {
            Ok(fitted) => {
                return Read {
                    binding,
                    image: Ok(fitted),
                };
            }
            Err(Stop::Absent(a)) => {
                return Read {
                    binding,
                    image: Err(a),
                };
            }
            Err(Stop::Changed) if !retried => retried = true,
            Err(Stop::Changed) => {
                return Read {
                    binding,
                    image: Err(Absent::Changed),
                };
            }
        }
    }
}

fn checkpoint(cancel: &dyn Fn() -> bool, site: Site) -> Result<(), Absent> {
    decode::checkpoint(cancel, site).map_err(|_| Absent::Cancelled)
}

fn absent_of(e: DecodeError) -> Stop {
    match e {
        DecodeError::Unsupported => Stop::Absent(Absent::Format),
        DecodeError::Changed | DecodeError::HeaderChanged => Stop::Changed,
        _ => Stop::Absent(Absent::Decode),
    }
}

/// One attempt at reading the file resolved to `canonical` with `key`.
fn read_resolved(
    canonical: &Path,
    key: &FileKey,
    slot: Slot,
    cancel: &dyn Fn() -> bool,
    identity: Identity<'_>,
) -> Result<Fitted, Stop> {
    checkpoint(cancel, Site::Read)?;
    let unchanged = |file: &File| -> Result<(), Stop> {
        let meta = file.metadata().map_err(|_| Stop::Changed)?;
        let now = FileKey::of(&meta, file, identity);
        if now.len == key.len && now.modified == key.modified && now.identity == key.identity {
            Ok(())
        } else {
            Err(Stop::Changed)
        }
    };
    let Ok((mut file, len)) = path::open(canonical) else {
        return Err(Stop::Changed);
    };
    unchanged(&file)?;
    // Every outcome of the handle's bytes but a cancellation, a failure as
    // much as a success, is verified against the key on the handle once
    // the bytes are taken: a file changed meanwhile is not the one
    // resolved, so its failure is not that observation's either.
    let snapshot = take(&mut file, len, cancel);
    if !matches!(snapshot, Err(Stop::Absent(Absent::Cancelled))) {
        unchanged(&file)?;
    }
    drop(file);
    let (bytes, dims, header) = snapshot?;
    decode::check_whole(&bytes, dims, header).map_err(absent_of)?;
    let image = decode::decode(bytes, dims).map_err(absent_of)?;
    checkpoint(cancel, Site::Decoded)?;
    Ok(fit(image, slot, cancel)?)
}

/// What a read decodes: the whole bytes, the dimensions and a PNG's
/// header fields.
type Snapshot = (Vec<u8>, [u32; 2], Option<decode::PngHeader>);

/// The header within the side and texel caps, and the whole bytes, of the
/// open `file` of `len` bytes.
fn take(file: &mut File, len: u64, cancel: &dyn Fn() -> bool) -> Result<Snapshot, Stop> {
    let probe = decode::header(file, len).map_err(absent_of)?;
    let [w, h] = probe.dims;
    if w > MAX_SIDE || h > MAX_SIDE {
        return Err(Absent::OverCap(Cap::Side).into());
    }
    if u64::from(w) * u64::from(h) > MAX_IMAGE_TEXELS {
        return Err(Absent::OverCap(Cap::Texels).into());
    }
    let Ok(Mode::Full(header)) = probe.mode else {
        return Err(Absent::OverCap(Cap::Texels).into());
    };
    file.seek(SeekFrom::Start(0)).map_err(|_| Absent::Decode)?;
    let (bytes, more) = match decode::read_capped(file, len, MAX_FILE_BYTES, cancel) {
        Ok(read) => read,
        Err(e) if decode::is_cancelled(&e) => return Err(Absent::Cancelled.into()),
        Err(_) => return Err(Absent::Decode.into()),
    };
    if more || u64::try_from(bytes.len()).ok() != Some(len) {
        return Err(Stop::Changed);
    }
    Ok((bytes, probe.dims, header))
}

/// The largest size within `max` that keeps the aspect of `native`,
/// rounded down, each side at least 1; `native` itself when it fits.
fn fitted_size(native: [u32; 2], max: [u32; 2]) -> [u32; 2] {
    let [w, h] = native;
    let [mw, mh] = max;
    if w <= mw && h <= mh {
        return native;
    }
    let (w, h, mw64, mh64) = (u64::from(w), u64::from(h), u64::from(mw), u64::from(mh));
    let down = |n: u64, d: u64, m: u32| u32::try_from(n / d).unwrap_or(m).clamp(1, m);
    if w * mh64 >= h * mw64 {
        [mw, down(h * mw64, w, mh)]
    } else {
        [down(w * mh64, h, mw), mh]
    }
}

fn zeroed(len: u64) -> Result<Vec<u8>, Absent> {
    let n = usize::try_from(len).map_err(|_| Absent::Decode)?;
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| Absent::Decode)?;
    v.resize(n, 0);
    Ok(v)
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Fits the straight-alpha `image` into `slot` (see the module).
fn fit(image: RgbaImage, slot: Slot, cancel: &dyn Fn() -> bool) -> Result<Fitted, Absent> {
    let native = [image.width(), image.height()];
    let content = fitted_size(native, slot.max_content());
    let mut texels = image.into_raw();
    for t in texels.as_chunks_mut::<4>().0 {
        let [r, g, b, a] = *t;
        *t = [premultiply(r, a), premultiply(g, a), premultiply(b, a), a];
    }
    let texels = if content == native {
        texels
    } else {
        area(&texels, native, content)?
    };
    checkpoint(cancel, Site::Fitted)?;
    let padded = content.map(|c| c.next_multiple_of(ALIGN) + 2 * GUTTER);
    let mut levels = Vec::with_capacity(MIP_LEVELS);
    levels.push(extrude(&texels, content, padded)?.into_boxed_slice());
    drop(texels);
    for k in 1..MIP_LEVELS {
        checkpoint(cancel, Site::Mip)?;
        let [pw, ph] = padded.map(|s| s >> (k - 1));
        let mut dst = zeroed(u64::from(pw / 2) * u64::from(ph / 2) * 4)?;
        if let Some(src) = levels.last() {
            downsample_rgba(src, pw, &mut dst);
        }
        levels.push(dst.into_boxed_slice());
    }
    Ok(Fitted {
        native,
        content,
        padded,
        levels: levels.into_boxed_slice(),
    })
}

/// `src`, premultiplied RGBA8 of `from` texels, resampled to `to` (each
/// side at most `from`'s) by an area filter: each output texel is the
/// mean of the source area it covers, weighted by the exact overlap, in
/// integers, rounded to nearest.
fn area(src: &[u8], from: [u32; 2], to: [u32; 2]) -> Result<Vec<u8>, Absent> {
    let [w, h] = from.map(u64::from);
    let [cw, ch] = to.map(u64::from);
    let mut out = zeroed(cw * ch * 4)?;
    // One output row's column sums over the source rows it covers, per
    // source column and channel.
    let mut acc = vec![0_u64; to_usize(from[0]) * 4];
    let row_bytes = to_usize(from[0]) * 4;
    let total = w * h;
    for (oy, out_row) in (0..ch).zip(out.chunks_exact_mut(to_usize(to[0]) * 4)) {
        acc.iter_mut().for_each(|a| *a = 0);
        // Output row `oy` covers source rows [oy·h, (oy+1)·h) in units of
        // 1/ch of a row; source row `sy` covers [sy·ch, (sy+1)·ch).
        let (top, bottom) = (oy * h, (oy + 1) * h);
        for sy in top / ch..bottom.div_ceil(ch) {
            let wy = bottom.min((sy + 1) * ch) - top.max(sy * ch);
            let start = usize::try_from(sy).unwrap_or(usize::MAX) * row_bytes;
            let Some(row) = src.get(start..start + row_bytes) else {
                continue;
            };
            for (a, &v) in acc.iter_mut().zip(row) {
                *a += u64::from(v) * wy;
            }
        }
        for (ox, texel) in (0..cw).zip(out_row.as_chunks_mut::<4>().0) {
            let (left, right) = (ox * w, (ox + 1) * w);
            let mut sum = [0_u64; 4];
            for sx in left / cw..right.div_ceil(cw) {
                let wx = right.min((sx + 1) * cw) - left.max(sx * cw);
                let at = usize::try_from(sx).unwrap_or(usize::MAX) * 4;
                let Some(&[r, g, b, a]) = acc.get(at..at + 4) else {
                    continue;
                };
                for (s, v) in sum.iter_mut().zip([r, g, b, a]) {
                    *s += v * wx;
                }
            }
            *texel = sum.map(|s| u8::try_from((s + total / 2) / total).unwrap_or(u8::MAX));
        }
    }
    Ok(out)
}

/// The allocation of `padded` texels holding `content` texels of `src`
/// [`GUTTER`] texels in, every other texel repeating the nearest content
/// texel.
fn extrude(src: &[u8], content: [u32; 2], padded: [u32; 2]) -> Result<Vec<u8>, Absent> {
    let [cw, ch] = content;
    let [pw, ph] = padded;
    let mut out = zeroed(u64::from(pw) * u64::from(ph) * 4)?;
    let nearest = |d: u32, n: u32| to_usize(d.saturating_sub(GUTTER).min(n - 1));
    for (dy, row) in (0..ph).zip(out.chunks_exact_mut(to_usize(pw) * 4)) {
        let sy = nearest(dy, ch);
        for (dx, texel) in (0..pw).zip(row.as_chunks_mut::<4>().0) {
            let at = (sy * to_usize(cw) + nearest(dx, cw)) * 4;
            if let Some(&[r, g, b, a]) = src.get(at..at + 4) {
                *texel = [r, g, b, a];
            }
        }
    }
    Ok(out)
}

/// Which of a chart's own images an element draws: the stage file
/// (system image 100) or the back image (101).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChartImage {
    Stage,
    Back,
}

impl ChartImage {
    /// Both, in table order.
    pub const ALL: [Self; 2] = [Self::Stage, Self::Back];

    /// Its index in a [`ChartTable`].
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn index(self) -> usize {
        match self {
            Self::Stage => 0,
            Self::Back => 1,
        }
    }

    /// Its slot in a chart layer of side `side`, gutters included, and
    /// the slot's origin: the stage file at the top left, the back image
    /// at the top right, each half the side square (S10-12 design §4.3).
    /// `None` for a side whose half is no [`Slot`].
    #[must_use]
    pub fn slot(self, side: u32) -> Option<([u32; 2], Slot)> {
        let half = side / 2;
        let origin = match self {
            Self::Stage => [0, 0],
            Self::Back => [half, 0],
        };
        Slot::new([half, half]).map(|s| (origin, s))
    }
}

/// Where a published chart image lies in its installation's chart layer:
/// its content's uv (never the padding or gutter), and its native size,
/// which stretch reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChartUv {
    pub uv: FrameUv,
    pub native: [u32; 2],
}

impl ChartUv {
    /// The uv of `fitted` written in `which`'s slot of layer `layer`, a
    /// chart layer of side `side`: level 0 at the slot's origin, its
    /// content [`GUTTER`] texels in. `None` for a side with no such slot,
    /// or content that does not fit it.
    #[must_use]
    pub fn of(fitted: &Fitted, which: ChartImage, side: u32, layer: u16) -> Option<Self> {
        let ([ox, oy], slot) = which.slot(side)?;
        let [pw, ph] = fitted.padded;
        let [sw, sh] = slot.size();
        if pw > sw || ph > sh {
            return None;
        }
        let [cw, ch] = fitted.content;
        let s = f64::from(side);
        let at = |v: u32| f32_of(f64::from(v) / s);
        Some(Self {
            uv: FrameUv {
                uv: [
                    at(ox + GUTTER),
                    at(oy + GUTTER),
                    at(ox + GUTTER + cw),
                    at(oy + GUTTER + ch),
                ],
                layer,
            },
            native: fitted.native,
        })
    }
}

/// The chart images an installation draws, by [`ChartImage::index`]: an
/// entry without a uv is absent, and draws nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChartTable {
    uvs: [Option<ChartUv>; 2],
}

impl ChartTable {
    /// Both absent.
    pub const ABSENT: Self = Self { uvs: [None, None] };

    /// The stage file `stage` and the back image `back`.
    #[must_use]
    pub const fn new(stage: Option<ChartUv>, back: Option<ChartUv>) -> Self {
        Self { uvs: [stage, back] }
    }

    /// `which`'s uv, `None` when it is absent.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn get(&self, which: ChartImage) -> Option<ChartUv> {
        self.uvs.get(which.index()).copied().flatten()
    }

    /// Whether `which` is present: drawable from this table.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn present(&self, which: ChartImage) -> bool {
        self.get(which).is_some()
    }
}

/// How a chart image meets its destination: the stretch a skin names
/// (`skin/StretchType.java` at `0.8.9`), applied at placement against the
/// image's native size, which no import can know. The importer's cropping
/// stretches have no case here: it drops them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartStretch {
    /// The destination as given (0, and any other mode the importer keeps).
    Fill,
    /// 1: the image's aspect, inside the destination, centred.
    Inner,
    /// 2: the image's aspect, covering the destination, centred.
    Outer,
    /// 4: the destination's width, the image's aspect.
    FitWidth,
    /// 6: the destination's height, the image's aspect.
    FitHeight,
    /// 8: the native size, shrunk to fit and never enlarged, at placement.
    NoExpanding,
    /// 9: exactly the native size, at placement.
    NoResize,
}

impl ChartStretch {
    /// The destination `[x, y, w, h]` (author pixels) of an image of
    /// `native` texels, and the native size the quad keeps at placement
    /// for [`NoExpanding`](Self::NoExpanding) and
    /// [`NoResize`](Self::NoResize), as an imported image's stretch is
    /// baked (the importer's `track::stretch`, in `f64`), its edges then
    /// rounded half up as the importer rounds a baked key's (y down,
    /// `Rect::model_from`). An image without area keeps the destination.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn place(self, [x, y, w, h]: [f32; 4], native: [u32; 2]) -> ([f32; 4], Option<Native>) {
        let [iw, ih] = native;
        if iw == 0 || ih == 0 {
            return ([x, y, w, h], None);
        }
        let (fx, fy, fw, fh) = (f64::from(x), f64::from(y), f64::from(w), f64::from(h));
        let (iw, ih) = (f64::from(iw), f64::from(ih));
        let (sx, sy) = (fw / iw, fh / ih);
        // The edges left, top, right and bottom, rounded half up.
        let edges = |l: f64, t: f64, r: f64, b: f64| {
            let up = |v: f64| (v + 0.5).floor();
            let (l, t) = (up(l), up(t));
            [f32_of(l), f32_of(t), f32_of(up(r) - l), f32_of(up(b) - t)]
        };
        let fit_height = |nh: f64| {
            let top = fy + (fh - nh) / 2.0;
            edges(fx, top, fx + fw, top + nh)
        };
        let fit_width = |nw: f64| {
            let left = fx + (fw - nw) / 2.0;
            edges(left, fy, left + nw, fy + fh)
        };
        let rect = match self {
            Self::Fill => [x, y, w, h],
            Self::Inner if sx <= sy => fit_height(ih * sx),
            Self::Inner => fit_width(iw * sy),
            Self::Outer if sx >= sy => fit_height(ih * sx),
            Self::Outer => fit_width(iw * sy),
            Self::FitWidth => fit_height(ih * fw / iw),
            Self::FitHeight => fit_width(iw * fh / ih),
            Self::NoExpanding => return ([x, y, w, h], Some(Native::NoLarger(native))),
            Self::NoResize => return ([x, y, w, h], Some(Native::Exact(native))),
        };
        (rect, None)
    }
}

#[cfg(test)]
mod tests {
    use super::fitted_size;

    #[test]
    fn the_fitted_size_keeps_the_aspect_within_the_slot() {
        assert_eq!(fitted_size([640, 480], [1008, 1008]), [640, 480]);
        assert_eq!(fitted_size([1920, 1080], [1008, 1008]), [1008, 567]);
        assert_eq!(fitted_size([1080, 1920], [1008, 1008]), [567, 1008]);
        assert_eq!(fitted_size([4000, 1], [1008, 1008]), [1008, 1]);
        assert_eq!(fitted_size([1, 4000], [1008, 1008]), [1, 1008]);
        assert_eq!(fitted_size([2016, 2016], [1008, 1008]), [1008, 1008]);
    }
}
