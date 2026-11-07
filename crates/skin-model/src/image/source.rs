//! The one seam every read of a skin's image or font content goes
//! through: a name's resolution and check, an image header, a bitmap
//! font's text, a bitmap font page's validating decode, an outline font's
//! magic and bytes, and an atlas file's encoded bytes. [`FileSource`] is
//! the disk by default; [`FileSource::cached`] puts a [`ResourceCache`]
//! of one skin folder's file observations in front of it, so another
//! import of that skin reads only what the cache has not seen.
//!
//! The cache records observations of files, never what an import made of
//! them: indices, notes, charges and limits are the importer's, worked out
//! again on every run. It keeps no decoded pixels and no encoded image
//! bytes: an atlas's encoded reads always reach the disk. A failure is
//! recorded only when it is the file's own and can be given again exactly
//! (a path or header verdict); a cancellation, a refused charge or an I/O
//! error is not. Past its bounds ([`CacheBounds`]) a miss is served from
//! the disk and not recorded.
//!
//! Not in-play code: it locks and allocates freely.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use super::decode::{self, Bands, DecodeError, Mode, Probe};
use super::font::{FontError, MAX_TOTAL_FONT_BYTES};
use super::path::{self, PathError};
use super::{FileAt, ImageError, MAX_FILES, Stop};

/// A file verdict that is the file's own and can be given again exactly:
/// a path check or a header check, with no I/O error or decoder source
/// behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failed {
    Rejected,
    Missing,
    Escapes,
    NotAFile,
    TooLarge,
    HeaderTooLarge,
    Unsupported,
    Changed,
    HeaderChanged,
    DecoderLimit,
}

impl Failed {
    fn of_path(e: &PathError) -> Option<Self> {
        match e {
            PathError::Rejected => Some(Self::Rejected),
            PathError::Missing => Some(Self::Missing),
            PathError::Escapes => Some(Self::Escapes),
            PathError::NotAFile => Some(Self::NotAFile),
            PathError::TooLarge => Some(Self::TooLarge),
            PathError::Io(_) => None,
        }
    }

    fn of_decode(e: &DecodeError) -> Option<Self> {
        match e {
            DecodeError::HeaderTooLarge => Some(Self::HeaderTooLarge),
            DecodeError::Unsupported => Some(Self::Unsupported),
            DecodeError::Changed => Some(Self::Changed),
            DecodeError::HeaderChanged => Some(Self::HeaderChanged),
            DecodeError::DecoderLimit => Some(Self::DecoderLimit),
            DecodeError::Image(_) | DecodeError::Png(_) | DecodeError::Io(_) => None,
        }
    }

    fn of_image(e: &ImageError) -> Option<Self> {
        match e {
            ImageError::PathRejected { .. } => Some(Self::Rejected),
            ImageError::Missing { .. } => Some(Self::Missing),
            ImageError::PathEscapes { .. } => Some(Self::Escapes),
            ImageError::NotAFile { .. } => Some(Self::NotAFile),
            ImageError::FileTooLarge { .. } => Some(Self::TooLarge),
            ImageError::HeaderTooLarge { .. } => Some(Self::HeaderTooLarge),
            ImageError::Unsupported { .. } => Some(Self::Unsupported),
            ImageError::DimensionsChanged { .. } => Some(Self::Changed),
            ImageError::HeaderChanged { .. } => Some(Self::HeaderChanged),
            ImageError::DecoderLimit { .. } => Some(Self::DecoderLimit),
            _ => None,
        }
    }

    /// The path error, for a path verdict.
    fn path(self) -> Option<PathError> {
        match self {
            Self::Rejected => Some(PathError::Rejected),
            Self::Missing => Some(PathError::Missing),
            Self::Escapes => Some(PathError::Escapes),
            Self::NotAFile => Some(PathError::NotAFile),
            Self::TooLarge => Some(PathError::TooLarge),
            _ => None,
        }
    }

    /// The header failure, path or decode, as the disk gave it.
    fn header(self) -> HeaderFail {
        match self.path() {
            Some(p) => HeaderFail::Path(p),
            None => HeaderFail::Decode(match self {
                Self::HeaderTooLarge => DecodeError::HeaderTooLarge,
                Self::Changed => DecodeError::Changed,
                Self::HeaderChanged => DecodeError::HeaderChanged,
                Self::DecoderLimit => DecodeError::DecoderLimit,
                _ => DecodeError::Unsupported,
            }),
        }
    }

    /// The error for version `at`.
    fn image(self, at: FileAt) -> ImageError {
        match self.header() {
            HeaderFail::Path(p) => ImageError::path(at, p),
            HeaderFail::Decode(d) => ImageError::decode(at, d),
        }
    }
}

/// Why a header was not read: its file's path, or the header itself.
#[derive(Debug)]
pub(crate) enum HeaderFail {
    Path(PathError),
    Decode(DecodeError),
}

impl HeaderFail {
    fn recorded(&self) -> Option<Failed> {
        match self {
            Self::Path(p) => Failed::of_path(p),
            Self::Decode(d) => Failed::of_decode(d),
        }
    }
}

/// How many observations a [`ResourceCache`] keeps: past any of these, a
/// miss is served from the disk and not recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheBounds {
    /// Resolution entries, by name.
    pub resolutions: usize,
    /// Content entries, by canonical path and kind.
    pub contents: usize,
    /// Bitmap font text bytes.
    pub fnt_bytes: u64,
    /// Outline font bytes.
    pub font_bytes: u64,
}

impl CacheBounds {
    /// The bounds of design D13: [`MAX_FILES`] resolution and content
    /// entries, and [`MAX_TOTAL_FONT_BYTES`] each of bitmap font text and
    /// outline font bytes, the caps those reads have per import.
    pub const DEFAULT: Self = Self {
        resolutions: MAX_FILES,
        contents: MAX_FILES,
        fnt_bytes: MAX_TOTAL_FONT_BYTES,
        font_bytes: MAX_TOTAL_FONT_BYTES,
    };
}

/// A content observation, by canonical path.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum ContentKey {
    Header(PathBuf),
    FntText(PathBuf),
    /// A page's validating decode, under the header admission read.
    Page(PathBuf),
    FontMagic(PathBuf),
    FontBytes(PathBuf),
}

#[derive(Clone, Debug)]
enum Content {
    Header(Result<Probe, Failed>),
    FntText(Arc<[u8]>),
    /// The page's dimensions and mode, and whether it decoded under them.
    Page([u32; 2], Mode, Result<(), Failed>),
    /// The first four bytes, or `None` for a shorter file.
    FontMagic(Result<Option<[u8; 4]>, Failed>),
    FontBytes(Arc<[u8]>),
}

#[derive(Debug, Default)]
struct Entries {
    resolutions: HashMap<String, Result<(PathBuf, u64), Failed>>,
    contents: HashMap<ContentKey, Content>,
    fnt_bytes: u64,
    font_bytes: u64,
}

/// One skin folder's file observations, shared by every import of that
/// skin that reads through [`FileSource::cached`].
pub struct ResourceCache {
    root: PathBuf,
    bounds: CacheBounds,
    entries: Mutex<Entries>,
}

impl fmt::Debug for ResourceCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let e = self.lock();
        f.debug_struct("ResourceCache")
            .field("root", &self.root)
            .field("resolutions", &e.resolutions.len())
            .field("contents", &e.contents.len())
            .field("fnt_bytes", &e.fnt_bytes)
            .field("font_bytes", &e.font_bytes)
            .finish()
    }
}

/// What a [`ResourceCache`] holds, against its [`CacheBounds`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheUsage {
    pub resolutions: usize,
    pub contents: usize,
    pub fnt_bytes: u64,
    pub font_bytes: u64,
}

impl ResourceCache {
    /// An empty cache of `root`'s files, within [`CacheBounds::DEFAULT`].
    #[must_use]
    pub fn new(root: &path::Root) -> Arc<Self> {
        Self::bounded(root, CacheBounds::DEFAULT)
    }

    /// An empty cache of `root`'s files within `bounds`, for tests that
    /// reach a bound.
    #[cfg(any(test, feature = "dev-tools"))]
    #[must_use]
    pub fn with_bounds(root: &path::Root, bounds: CacheBounds) -> Arc<Self> {
        Self::bounded(root, bounds)
    }

    fn bounded(root: &path::Root, bounds: CacheBounds) -> Arc<Self> {
        Arc::new(Self {
            root: root.dir().to_path_buf(),
            bounds,
            entries: Mutex::new(Entries::default()),
        })
    }

    /// The entries and bytes held now.
    #[must_use]
    pub fn usage(&self) -> CacheUsage {
        let e = self.lock();
        CacheUsage {
            resolutions: e.resolutions.len(),
            contents: e.contents.len(),
            fnt_bytes: e.fnt_bytes,
            font_bytes: e.font_bytes,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn resolution(&self, name: &str) -> Option<Result<(PathBuf, u64), Failed>> {
        self.lock().resolutions.get(name).cloned()
    }

    fn record_resolution(&self, name: &str, r: Result<(PathBuf, u64), Failed>) {
        let mut e = self.lock();
        if e.resolutions.len() < self.bounds.resolutions {
            e.resolutions.insert(name.to_owned(), r);
        }
    }

    fn content(&self, key: &ContentKey) -> Option<Content> {
        self.lock().contents.get(key).cloned()
    }

    /// Records `content` under `key` unless a bound is past: the entries,
    /// or the bytes of its kind.
    fn record(&self, key: ContentKey, content: Content) {
        let mut e = self.lock();
        if e.contents.len() >= self.bounds.contents || e.contents.contains_key(&key) {
            return;
        }
        match &content {
            Content::FntText(b) => {
                let n = u64::try_from(b.len()).unwrap_or(u64::MAX);
                if e.fnt_bytes.saturating_add(n) > self.bounds.fnt_bytes {
                    return;
                }
                e.fnt_bytes += n;
            }
            Content::FontBytes(b) => {
                let n = u64::try_from(b.len()).unwrap_or(u64::MAX);
                if e.font_bytes.saturating_add(n) > self.bounds.font_bytes {
                    return;
                }
                e.font_bytes += n;
            }
            _ => {}
        }
        e.contents.insert(key, content);
    }
}

/// The disk operations a [`FileSource`] made, by kind, and the decodes it
/// ran: a cache hit counts nothing.
#[cfg(any(test, feature = "dev-tools"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceCounts {
    /// Names resolved and checked against the folder.
    pub resolutions: u32,
    pub headers: u32,
    pub fnt_texts: u32,
    /// Bitmap font pages read and decoded to validate them.
    pub page_validations: u32,
    pub font_magics: u32,
    pub font_bytes: u32,
    /// Encoded image reads, a page validation's included.
    pub encoded_reads: u32,
    /// `decode_file` runs, a page validation's included.
    pub decodes: u32,
}

#[cfg(any(test, feature = "dev-tools"))]
#[derive(Debug, Default)]
struct Counters(Mutex<SourceCounts>);

/// Where an import's image and font content comes from: the disk, or a
/// [`ResourceCache`] in front of it. Clones share the cache and, under
/// dev-tools, the counts.
#[derive(Clone)]
pub struct FileSource {
    cache: Option<Arc<ResourceCache>>,
    #[cfg(any(test, feature = "dev-tools"))]
    counts: Arc<Counters>,
}

impl fmt::Debug for FileSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cache {
            None => f.write_str("FileSource::Disk"),
            Some(c) => f.debug_tuple("FileSource::Cached").field(c).finish(),
        }
    }
}

impl Default for FileSource {
    fn default() -> Self {
        Self::disk()
    }
}

impl FileSource {
    /// The disk, read on every request.
    #[must_use]
    pub fn disk() -> Self {
        Self {
            cache: None,
            #[cfg(any(test, feature = "dev-tools"))]
            counts: Arc::default(),
        }
    }

    /// `cache` in front of the disk.
    #[must_use]
    pub fn cached(cache: Arc<ResourceCache>) -> Self {
        Self {
            cache: Some(cache),
            #[cfg(any(test, feature = "dev-tools"))]
            counts: Arc::default(),
        }
    }

    /// The disk operations and decodes this source and its clones made.
    #[cfg(any(test, feature = "dev-tools"))]
    #[must_use]
    pub fn counts(&self) -> SourceCounts {
        *self.counts.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(any(test, feature = "dev-tools"))]
    fn count(&self, f: impl FnOnce(&mut SourceCounts)) {
        f(&mut self.counts.0.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// The cache, when it serves the folder `root`.
    fn cache_for(&self, root: &Path) -> Option<&ResourceCache> {
        self.cache.as_deref().filter(|c| c.root == root)
    }

    /// The cache, when it serves the folder holding `canonical`.
    fn cache_holding(&self, canonical: &Path) -> Option<&ResourceCache> {
        self.cache
            .as_deref()
            .filter(|c| canonical.starts_with(&c.root))
    }

    /// `name` resolved inside the canonical skin folder `root`, with its
    /// length ([`path::resolve`]).
    pub(crate) fn resolve(&self, root: &Path, name: &str) -> Result<(PathBuf, u64), PathError> {
        let cache = self.cache_for(root);
        if let Some(r) = cache.and_then(|c| c.resolution(name)) {
            return r.map_err(|f| f.path().unwrap_or(PathError::Missing));
        }
        #[cfg(any(test, feature = "dev-tools"))]
        self.count(|c| c.resolutions += 1);
        let r = path::resolve(root, name);
        if let Some(c) = cache {
            match &r {
                Ok(ok) => c.record_resolution(name, Ok(ok.clone())),
                Err(e) => {
                    if let Some(f) = Failed::of_path(e) {
                        c.record_resolution(name, Err(f));
                    }
                }
            }
        }
        r
    }

    /// The header of the file at `canonical` ([`decode::header`] on the
    /// opened file).
    pub(crate) fn header(&self, canonical: &Path) -> Result<Probe, HeaderFail> {
        let key = ContentKey::Header(canonical.to_path_buf());
        let cache = self.cache_holding(canonical);
        if let Some(Content::Header(r)) = cache.and_then(|c| c.content(&key)) {
            return r.map_err(Failed::header);
        }
        #[cfg(any(test, feature = "dev-tools"))]
        self.count(|c| c.headers += 1);
        let r = path::open(canonical)
            .map_err(HeaderFail::Path)
            .and_then(|(mut file, len)| decode::header(&mut file, len).map_err(HeaderFail::Decode));
        if let Some(c) = cache {
            let recorded = match &r {
                Ok(p) => Some(Ok(*p)),
                Err(e) => e.recorded().map(Err),
            };
            if let Some(v) = recorded {
                c.record(key, Content::Header(v));
            }
        }
        r
    }

    /// A bitmap font's text: the file at `canonical` opened, its handle's
    /// length given to `check`, then at most `max + 1` bytes read.
    pub(crate) fn fnt_text<E>(
        &self,
        canonical: &Path,
        max: u64,
        check: impl FnOnce(u64) -> Result<(), E>,
        path_err: impl Fn(PathError) -> E,
    ) -> Result<Arc<[u8]>, E> {
        let key = ContentKey::FntText(canonical.to_path_buf());
        let cache = self.cache_holding(canonical);
        if let Some(Content::FntText(b)) = cache.and_then(|c| c.content(&key)) {
            check(u64::try_from(b.len()).unwrap_or(u64::MAX))?;
            return Ok(b);
        }
        #[cfg(any(test, feature = "dev-tools"))]
        self.count(|c| c.fnt_texts += 1);
        let (handle, len) = path::open(canonical).map_err(&path_err)?;
        check(len)?;
        let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        handle
            .take(max.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| path_err(PathError::Io(e)))?;
        let bytes: Arc<[u8]> = bytes.into();
        if let Some(c) = cache {
            c.record(key, Content::FntText(Arc::clone(&bytes)));
        }
        Ok(bytes)
    }

    /// Whether the bitmap font page at `canonical`, version `at`, decodes
    /// completely under `header`: read ([`Self::encoded`]) and decoded
    /// ([`Self::decode`]) unless the cache has the verdict.
    pub(crate) fn validate_page(
        &self,
        canonical: &Path,
        at: FileAt,
        header: ([u32; 2], Mode),
        max: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(), Stop> {
        let key = ContentKey::Page(canonical.to_path_buf());
        let cache = self.cache_holding(canonical);
        if let Some(Content::Page(dims, mode, r)) = cache.and_then(|c| c.content(&key))
            && (dims, mode) == header
        {
            return r.map_err(|f| Stop::Image(f.image(at)));
        }
        #[cfg(any(test, feature = "dev-tools"))]
        self.count(|c| c.page_validations += 1);
        let r = self
            .encoded(canonical, at, max, cancel)
            .and_then(|bytes| self.decode(bytes, at, header, cancel, &mut Validate))
            .map(drop);
        if let Some(c) = cache {
            let recorded = match &r {
                Ok(()) => Some(Ok(())),
                Err(Stop::Image(e)) => Failed::of_image(e).map(Err),
                Err(Stop::Cancelled) => None,
            };
            if let Some(v) = recorded {
                c.record(key, Content::Page(header.0, header.1, v));
            }
        }
        r
    }

    /// Whether the outline font at `canonical` starts with a font magic:
    /// its first four bytes, `None` for a shorter file.
    pub(crate) fn font_magic(
        &self,
        font: usize,
        canonical: &Path,
    ) -> Result<Option<[u8; 4]>, FontError> {
        let cache = self.cache_holding(canonical);
        if let Some(c) = cache {
            if let Some(Content::FontBytes(b)) =
                c.content(&ContentKey::FontBytes(canonical.to_path_buf()))
            {
                return Ok(b.first_chunk::<4>().copied());
            }
            if let Some(Content::FontMagic(r)) =
                c.content(&ContentKey::FontMagic(canonical.to_path_buf()))
            {
                return r
                    .map_err(|f| FontError::path(font, f.path().unwrap_or(PathError::Missing)));
            }
        }
        #[cfg(any(test, feature = "dev-tools"))]
        self.count(|c| c.font_magics += 1);
        let opened = path::open(canonical);
        let r = match opened {
            Err(e) => {
                if let (Some(c), Some(f)) = (cache, Failed::of_path(&e)) {
                    c.record(
                        ContentKey::FontMagic(canonical.to_path_buf()),
                        Content::FontMagic(Err(f)),
                    );
                }
                return Err(FontError::path(font, e));
            }
            Ok((mut file, _)) => {
                let mut magic = [0_u8; 4];
                match file.read_exact(&mut magic) {
                    Ok(()) => Some(magic),
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => None,
                    Err(source) => return Err(FontError::Io { font, source }),
                }
            }
        };
        if let Some(c) = cache {
            c.record(
                ContentKey::FontMagic(canonical.to_path_buf()),
                Content::FontMagic(Ok(r)),
            );
        }
        Ok(r)
    }

    /// An outline font's bytes: the file at `canonical` opened, its
    /// handle's length given to `check`, then read by `read` (which returns
    /// the bytes taken from a budget with the outcome).
    pub(crate) fn font_bytes(
        &self,
        font: usize,
        canonical: &Path,
        check: impl FnOnce(u64) -> Result<(), FontError>,
        read: impl FnOnce(std::fs::File, u64) -> (u64, Result<Vec<u8>, FontError>),
    ) -> (u64, Result<Arc<[u8]>, FontError>) {
        let key = ContentKey::FontBytes(canonical.to_path_buf());
        let cache = self.cache_holding(canonical);
        if let Some(Content::FontBytes(b)) = cache.and_then(|c| c.content(&key)) {
            let len = u64::try_from(b.len()).unwrap_or(u64::MAX);
            if let Err(e) = check(len) {
                return (0, Err(e));
            }
            return (len, Ok(b));
        }
        #[cfg(any(test, feature = "dev-tools"))]
        self.count(|c| c.font_bytes += 1);
        let (file, len) = match path::open(canonical) {
            Ok(opened) => opened,
            Err(e) => return (0, Err(FontError::path(font, e))),
        };
        if let Err(e) = check(len) {
            return (0, Err(e));
        }
        let (taken, r) = read(file, len);
        let r = r.map(Arc::<[u8]>::from);
        if let (Some(c), Ok(b)) = (cache, &r) {
            c.record(key, Content::FontBytes(Arc::clone(b)));
        }
        (taken, r)
    }

    /// The encoded bytes of the file at `canonical`, always from the disk
    /// ([`super::read_encoded`]).
    pub(crate) fn encoded(
        &self,
        canonical: &Path,
        at: FileAt,
        limit: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Vec<u8>, Stop> {
        #[cfg(any(test, feature = "dev-tools"))]
        self.count(|c| c.encoded_reads += 1);
        super::read_encoded(canonical, at, limit, cancel)
    }

    /// [`super::decode_file`], counted.
    pub(crate) fn decode(
        &self,
        bytes: Vec<u8>,
        at: FileAt,
        header: ([u32; 2], Mode),
        cancel: &dyn Fn() -> bool,
        bands: &mut dyn Bands,
    ) -> Result<Option<::image::RgbaImage>, Stop> {
        #[cfg(any(test, feature = "dev-tools"))]
        self.count(|c| c.decodes += 1);
        super::decode_file(bytes, at, header, cancel, bands)
    }
}

/// Bands that are only checked: a validating decode keeps no rows.
pub(crate) struct Validate;

impl Bands for Validate {
    fn wants(&mut self, _: u32, _: u32) -> bool {
        false
    }

    fn put(&mut self, _: u32, _: &::image::RgbaImage) {}
}
