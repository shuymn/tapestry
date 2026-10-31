//! A sheet built one file, image and font at a time, for a front end that
//! decides what to load while it reads the skin.
//!
//! [`SheetBuilder`] runs the checks of [`Sheet::new`] and
//! [`Sheet::with_fonts`] per item, with the same primitives, and keeps
//! every running total across calls: a failed admission leaves the totals
//! as they were but counts as an attempt, so a front end cannot retry
//! without bound.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use super::fnt::{
    Fnt, FntError, FntErrorKind, FntFile, FntLimits, FntPage, MAX_FNT_BYTES, MAX_FONT_GLYPHS,
    MAX_FONT_KERNINGS, MAX_SHEET_GLYPHS, MAX_SHEET_KERNINGS,
};
use super::font::{self, FontDef, FontError, MAX_FONTS, MAX_TOTAL_FONT_BYTES};
use super::path::{self, PathError, Root};
use super::{
    Cell, File, FileAt, FileDef, Image, ImageDef, ImageError, MAX_FILE_BYTES, MAX_FILES,
    MAX_IMAGES, NEXT_SHEET, Sheet, Total, Version, add_frames, check_cycle, checked_header, cut,
    decode, image_frames, push_frames, read_encoded, read_file, renumber, resolve_file, texels,
};
use crate::quad::Filter;
use crate::widget::text::MAX_TEXT_ELEMENTS;

/// The most bitmap fonts (a primary file and its fallbacks) one sheet
/// may hold: one per text element at most.
pub const MAX_BITMAP_FONTS: usize = MAX_TEXT_ELEMENTS;
/// The most fallback files one bitmap font may name: the font slots
/// beside its primary.
pub const MAX_FALLBACKS: usize = MAX_FONTS - 1;

/// Why [`SheetBuilder::admit_file`] refused a file.
#[derive(Debug, thiserror::Error)]
pub enum AdmitError {
    /// An aggregate limit: the attempts, or a variant's running byte or
    /// texel total. Admitting anything more cannot succeed.
    #[error(transparent)]
    Limit(ImageError),
    /// This file alone: its name, path, metadata or header.
    #[error(transparent)]
    File(ImageError),
}

/// Why [`SheetBuilder::admit_font`] refused a font.
#[derive(Debug, thiserror::Error)]
pub enum FontAdmitError {
    /// An aggregate limit: the font count or the running byte total.
    #[error(transparent)]
    Limit(FontError),
    /// This font alone: its name, path, metadata or first bytes.
    #[error(transparent)]
    File(FontError),
}

/// A piece of work [`SheetBuilder::admit_fnt`] asks its front end to
/// charge, immediately before doing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charge {
    /// One file opened: the `.fnt` file or a page.
    FileAttempt,
    /// The `.fnt` file's bytes, before they are read.
    FontBytes(u64),
    /// A page's encoded bytes, per variant, before its header is read.
    ImageBytes { lo: u64, hi: u64 },
    /// A page's image.
    Image,
    /// A page image's frames.
    Frames(u32),
}

/// The front end refused a [`Charge`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused;

/// An aggregate limit [`SheetBuilder::admit_fnt`] reached: admitting
/// anything more of its kind cannot succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FntLimit {
    #[error("the front end refused {0:?}")]
    Charge(Charge),
    #[error("more than {MAX_FILES} file admissions")]
    Attempts,
    #[error("more than {MAX_FONTS} font files")]
    Fonts,
    #[error("the font files' bytes pass {MAX_TOTAL_FONT_BYTES}")]
    FontBytes,
    #[error("a variant's image files pass their total bytes")]
    ImageBytes,
    #[error("a variant's image files pass their total texels")]
    Texels,
    #[error("more than {MAX_IMAGES} images")]
    Images,
    #[error("more frames than the skin may hold")]
    Frames,
    #[error("more than {MAX_SHEET_GLYPHS} glyphs across the bitmap fonts")]
    Glyphs,
    #[error("more than {MAX_SHEET_KERNINGS} kerning pairs across the bitmap fonts")]
    Kernings,
}

/// Why one `.fnt` file was refused.
#[derive(Debug, thiserror::Error)]
pub enum FntFileError {
    /// The file's name, path, metadata or read.
    #[error(transparent)]
    Font(FontError),
    /// Its text, or a glyph outside its page.
    #[error(transparent)]
    Fnt(FntError),
    /// One of its pages.
    #[error("{error}")]
    Page {
        error: FntError,
        #[source]
        source: ImageError,
    },
}

impl FntFileError {
    /// What was wrong with the text or a page; `None` when the file
    /// itself could not be found or read.
    #[must_use]
    pub fn fnt(&self) -> Option<FntError> {
        match self {
            Self::Font(_) => None,
            Self::Fnt(e) | Self::Page { error: e, .. } => Some(*e),
        }
    }
}

/// Why [`SheetBuilder::admit_fnt`] refused a `.fnt` file.
#[derive(Debug, thiserror::Error)]
pub enum FntAdmitError {
    #[error(transparent)]
    Limit(FntLimit),
    #[error(transparent)]
    File(FntFileError),
}

/// Why [`SheetBuilder::add_bitmap_font`] refused a bitmap font.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BitmapFontError {
    #[error("more than {MAX_BITMAP_FONTS} bitmap fonts")]
    TooManyFonts,
    #[error("more than {MAX_FALLBACKS} fallback files")]
    TooManyFallbacks,
    #[error("a file that was not admitted")]
    UnknownFile,
}

#[cfg(test)]
thread_local! {
    /// The `.fnt` files this thread has read: a witness that the slot,
    /// attempt and metadata checks come first.
    pub(crate) static FNT_READS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Page `page`'s failure, by whether the source player also fails to
/// load it: a missing or unreadable image fails there too.
fn page_error(page: usize, e: ImageError) -> FntAdmitError {
    let page = u8::try_from(page).unwrap_or(u8::MAX);
    let kind = match e {
        ImageError::Missing { .. } => FntErrorKind::PageMissing { page },
        ImageError::Decode { .. }
        | ImageError::Io { .. }
        | ImageError::DimensionsChanged { .. } => FntErrorKind::PageBroken { page },
        _ => FntErrorKind::PageRefused { page },
    };
    FntAdmitError::File(FntFileError::Page {
        error: FntError { line: 0, kind },
        source: e,
    })
}

/// The folder part of a skin-relative name, `""` for none.
fn folder(name: &str) -> &str {
    name.rfind(['/', '\\'])
        .map_or("", |i| name.get(..i).unwrap_or(""))
}

fn charged(c: Charge) -> FntAdmitError {
    FntAdmitError::Limit(FntLimit::Charge(c))
}

/// A [`Sheet`] under construction against one skin folder.
#[derive(Debug)]
pub struct SheetBuilder {
    root: PathBuf,
    attempts: usize,
    files: Vec<File>,
    lo_bytes: Total,
    hi_bytes: Total,
    lo_texels: Total,
    hi_texels: Total,
    images: Vec<Image>,
    frames: Vec<u32>,
    cell_ids: BTreeMap<Cell, u32>,
    fonts: Vec<String>,
    font_bytes: Total,
    fnts: Vec<Fnt>,
    bitmap_fonts: Vec<Box<[u16]>>,
    glyphs: usize,
    kernings: usize,
}

impl SheetBuilder {
    /// An empty builder over the skin folder `root`.
    #[must_use]
    pub fn new(root: &Root) -> Self {
        Self {
            root: root.dir().to_path_buf(),
            attempts: 0,
            files: Vec::new(),
            lo_bytes: Total::BYTES,
            hi_bytes: Total::BYTES,
            lo_texels: Total::TEXELS,
            hi_texels: Total::TEXELS,
            images: Vec::new(),
            frames: Vec::new(),
            cell_ids: BTreeMap::new(),
            fonts: Vec::new(),
            font_bytes: Total {
                sum: 0,
                cap: MAX_TOTAL_FONT_BYTES,
            },
            fnts: Vec::new(),
            bitmap_fonts: Vec::new(),
            glyphs: 0,
            kernings: 0,
        }
    }

    /// Admits a file and its optional @2x version, returning its index: the
    /// number of files admitted before it.
    ///
    /// Counts one attempt first, at most [`MAX_FILES`] across the builder,
    /// failed ones included. Then, as [`Sheet::new`] does for one file:
    /// each version's name, path and metadata, and each variant's running
    /// byte total, all before any header is read; then the headers, an @2x
    /// version at exactly twice the size, and each variant's running texel
    /// total. Nothing is kept unless every check passes.
    ///
    /// # Errors
    ///
    /// [`AdmitError::Limit`] for the attempts ([`ImageError::TooManyFiles`])
    /// and the running totals; [`AdmitError::File`] for anything else.
    /// The [`FileAt`](super::FileAt) in an error names the index the file would have had.
    pub fn admit_file(&mut self, def: FileDef) -> Result<u32, AdmitError> {
        self.attempts = self.attempts.saturating_add(1);
        if self.attempts > MAX_FILES {
            return Err(AdmitError::Limit(ImageError::TooManyFiles {
                n: self.attempts,
            }));
        }
        let file = self.files.len();
        let index = u32::try_from(file)
            .map_err(|_| AdmitError::Limit(ImageError::TooManyFiles { n: self.attempts }))?;
        let (resolved, lo_bytes, hi_bytes) =
            resolve_file(&self.root, file, &def, self.lo_bytes, self.hi_bytes).map_err(classify)?;
        let (loaded, lo_texels, hi_texels) =
            read_file(file, def, &resolved, self.lo_texels, self.hi_texels).map_err(classify)?;
        self.files.push(loaded);
        (self.lo_bytes, self.hi_bytes) = (lo_bytes, hi_bytes);
        (self.lo_texels, self.hi_texels) = (lo_texels, hi_texels);
        Ok(index)
    }

    /// An admitted file's 1x width and height.
    #[must_use]
    pub fn file_dims(&self, file: u32) -> Option<[u32; 2]> {
        let f = self.files.get(usize::try_from(file).ok()?)?;
        Some(f.lo.dims)
    }

    /// Adds an image of an admitted file, returning its index. No file is
    /// read.
    ///
    /// Checks, in order: at most [`MAX_IMAGES`] images; an admitted file;
    /// the cycle's period; a grid of a column and a row with at most
    /// [`MAX_IMAGE_FRAMES`](super::MAX_IMAGE_FRAMES) frames, and at most
    /// [`MAX_SKIN_FRAMES`](super::MAX_SKIN_FRAMES) across the images; the
    /// rectangle inside the file; non-empty frames. Nothing is kept unless
    /// every check passes.
    ///
    /// # Errors
    ///
    /// The first check that fails; `image` in it is the index the image
    /// would have had.
    pub fn add_image(&mut self, def: ImageDef) -> Result<u32, ImageError> {
        let image = self.images.len();
        if image >= MAX_IMAGES {
            return Err(ImageError::TooManyImages { n: image + 1 });
        }
        let Some(file) = usize::try_from(def.file)
            .ok()
            .and_then(|f| self.files.get(f))
        else {
            return Err(ImageError::UnknownFile { image });
        };
        check_cycle(image, &def)?;
        let n = image_frames(image, &def)?;
        add_frames(self.frames.len(), n)?;
        let (rect, cell) = cut(image, &def, file.lo.dims)?;
        let drawn = push_frames(&def, rect, cell, &mut self.cell_ids, &mut self.frames);
        self.images.push(drawn);
        u32::try_from(image).map_err(|_| ImageError::TooManyImages { n: image + 1 })
    }

    /// Admits a font file, returning its index.
    ///
    /// Checks, as [`Sheet::with_fonts`] does for one font: at most
    /// [`MAX_FONTS`] fonts, `.fnt` files included; its name, path and metadata, and the running
    /// byte total; then its first four bytes. Nothing is kept unless every
    /// check passes.
    ///
    /// # Errors
    ///
    /// [`FontAdmitError::Limit`] for the count and the running total;
    /// [`FontAdmitError::File`] for anything else.
    pub fn admit_font(&mut self, def: FontDef) -> Result<u16, FontAdmitError> {
        let index = self.fonts.len();
        if index + self.fnts.len() >= MAX_FONTS {
            return Err(FontAdmitError::Limit(FontError::TooManyFonts {
                n: index + 1,
            }));
        }
        let (path, len) =
            font::resolve(&self.root, index, &def.path).map_err(FontAdmitError::File)?;
        let total =
            self.font_bytes
                .with(len)
                .ok_or(FontAdmitError::Limit(FontError::TotalTooLarge {
                    font: index,
                }))?;
        font::check_magic(index, &path).map_err(FontAdmitError::File)?;
        self.font_bytes = total;
        self.fonts.push(def.path);
        u16::try_from(index)
            .map_err(|_| FontAdmitError::Limit(FontError::TooManyFonts { n: index + 1 }))
    }

    /// Admits a bitmap font file `name` and its pages, returning its index
    /// among the `.fnt` files. `charge` is called immediately before each
    /// guarded piece of work; a refusal aborts with [`FntLimit::Charge`],
    /// and what it charged before stays charged.
    ///
    /// In order: a font slot (outline fonts and `.fnt` files at most
    /// [`MAX_FONTS`]); a file attempt (shared with
    /// [`admit_file`](Self::admit_file)); the name, path and metadata, a
    /// regular file of at most [`MAX_FNT_BYTES`]; its length added to the
    /// font bytes ([`MAX_TOTAL_FONT_BYTES`]), never refunded; the read, on
    /// the checked handle; the parse ([`FntFile::parse`]) with at most
    /// [`MAX_FONT_GLYPHS`] glyphs and [`MAX_FONT_KERNINGS`] pairs, and no
    /// more than the sheet has left of [`MAX_SHEET_GLYPHS`] and
    /// [`MAX_SHEET_KERNINGS`]. Then the pages, all or nothing: each page's
    /// attempt, metadata, byte totals, image and frame; then each header
    /// and the texel totals; then every glyph inside its page; then each
    /// page decoded and dropped, one at a time, so a corrupt page fails
    /// this font rather than the atlas. Each page becomes a whole-file
    /// image of one frame, filtered linearly.
    ///
    /// On any failure the builder is as it was, but for the attempts and
    /// the font bytes spent.
    ///
    /// # Errors
    ///
    /// [`FntAdmitError::Limit`] for a refused charge and every aggregate
    /// limit, the sheet's glyph and kerning totals included (also when the
    /// file's own cap is reached at the same glyph or pair);
    /// [`FntAdmitError::File`] for this file alone.
    pub fn admit_fnt(
        &mut self,
        name: &str,
        charge: &mut dyn FnMut(Charge) -> Result<(), Refused>,
    ) -> Result<u16, FntAdmitError> {
        let limit = FntAdmitError::Limit;
        let file_err = |e| FntAdmitError::File(FntFileError::Fnt(e));
        let slot = self.fonts.len() + self.fnts.len();
        if slot >= MAX_FONTS {
            return Err(limit(FntLimit::Fonts));
        }
        let index = u16::try_from(self.fnts.len()).map_err(|_| limit(FntLimit::Fonts))?;
        self.attempt(charge)?;
        let font_err =
            |e: PathError| FntAdmitError::File(FntFileError::Font(FontError::path(slot, e)));
        let (resolved, len) = path::resolve(&self.root, name).map_err(font_err)?;
        let too_large = FntError {
            line: 0,
            kind: FntErrorKind::TooLarge,
        };
        if len > MAX_FNT_BYTES {
            return Err(file_err(too_large));
        }
        let c = Charge::FontBytes(len);
        charge(c).map_err(|_| charged(c))?;
        self.font_bytes = self
            .font_bytes
            .with(len)
            .ok_or(limit(FntLimit::FontBytes))?;
        #[cfg(test)]
        FNT_READS.with(|n| n.set(n.get() + 1));
        let (handle, len) = path::open(&resolved).map_err(font_err)?;
        if len > MAX_FNT_BYTES {
            return Err(file_err(too_large));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        handle
            .take(MAX_FNT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| font_err(PathError::Io(e)))?;
        let glyphs_left = MAX_SHEET_GLYPHS.saturating_sub(self.glyphs);
        let kernings_left = MAX_SHEET_KERNINGS.saturating_sub(self.kernings);
        let limits = FntLimits {
            glyphs: MAX_FONT_GLYPHS.min(glyphs_left),
            kernings: MAX_FONT_KERNINGS.min(kernings_left),
        };
        let file = FntFile::parse(&bytes, folder(name), limits).map_err(|e| match e.kind {
            FntErrorKind::TooManyGlyphs if glyphs_left <= MAX_FONT_GLYPHS => {
                limit(FntLimit::Glyphs)
            }
            FntErrorKind::TooManyKernings if kernings_left <= MAX_FONT_KERNINGS => {
                limit(FntLimit::Kernings)
            }
            _ => file_err(e),
        })?;
        drop(bytes);

        // The pages: attempts, metadata, bytes, images and frames first.
        let first = self.files.len();
        let (mut lo_bytes, mut hi_bytes) = (self.lo_bytes, self.hi_bytes);
        let mut paths = Vec::with_capacity(file.pages().len());
        for (p, page) in file.pages().iter().enumerate() {
            let at = FileAt {
                file: first + p,
                hi: false,
            };
            self.attempt(charge)?;
            let (resolved, len) = path::resolve(&self.root, page)
                .map_err(|e| page_error(p, ImageError::path(at, e)))?;
            let c = Charge::ImageBytes { lo: len, hi: len };
            charge(c).map_err(|_| charged(c))?;
            lo_bytes = lo_bytes.with(len).ok_or(limit(FntLimit::ImageBytes))?;
            hi_bytes = hi_bytes.with(len).ok_or(limit(FntLimit::ImageBytes))?;
            charge(Charge::Image).map_err(|_| charged(Charge::Image))?;
            if self.images.len() + p >= MAX_IMAGES {
                return Err(limit(FntLimit::Images));
            }
            charge(Charge::Frames(1)).map_err(|_| charged(Charge::Frames(1)))?;
            add_frames(self.frames.len() + p, 1).map_err(|_| limit(FntLimit::Frames))?;
            paths.push(resolved);
        }

        // Headers and texels, then the glyphs inside their pages.
        let (mut lo_texels, mut hi_texels) = (self.lo_texels, self.hi_texels);
        let mut dims = Vec::with_capacity(paths.len());
        for (p, resolved) in paths.iter().enumerate() {
            let at = FileAt {
                file: first + p,
                hi: false,
            };
            let d = checked_header(at, resolved).map_err(|e| page_error(p, e))?;
            lo_texels = lo_texels.with(texels(d)).ok_or(limit(FntLimit::Texels))?;
            hi_texels = hi_texels.with(texels(d)).ok_or(limit(FntLimit::Texels))?;
            dims.push(d);
        }
        for g in file.glyphs().iter().chain(file.missing().as_ref()) {
            let [x, y, w, h] = g.src.map(u32::from);
            let inside = dims
                .get(usize::from(g.page))
                .is_some_and(|&[pw, ph]| x + w <= pw && y + h <= ph);
            if !inside {
                return Err(file_err(FntError {
                    line: 0,
                    kind: FntErrorKind::GlyphOutside { code: g.code },
                }));
            }
        }

        // Every page decodes, one at a time, so a corrupt page fails this
        // font here rather than the atlas later.
        for (p, (resolved, &d)) in paths.iter().zip(&dims).enumerate() {
            let at = FileAt {
                file: first + p,
                hi: false,
            };
            let bytes = read_encoded(resolved, at, MAX_FILE_BYTES).map_err(|e| page_error(p, e))?;
            decode::decode(bytes, d).map_err(|e| page_error(p, ImageError::decode(at, e)))?;
        }

        // Kept.
        let mut pages = Vec::with_capacity(dims.len());
        for (name, d) in file.pages().iter().zip(dims) {
            let file_index =
                u32::try_from(self.files.len()).map_err(|_| limit(FntLimit::Attempts))?;
            self.files.push(File {
                lo: Version {
                    name: name.clone(),
                    dims: d,
                },
                hi: None,
            });
            let def = ImageDef {
                file: file_index,
                rect: None,
                grid: [1, 1],
                cycle: None,
                filter: Filter::Linear,
            };
            let image = u32::try_from(self.images.len()).map_err(|_| limit(FntLimit::Images))?;
            let [w, h] = d;
            let drawn = push_frames(&def, [0, 0, w, h], d, &mut self.cell_ids, &mut self.frames);
            self.images.push(drawn);
            pages.push(FntPage {
                image,
                dims: d.map(|v| u16::try_from(v).unwrap_or(u16::MAX)),
            });
        }
        (self.lo_bytes, self.hi_bytes) = (lo_bytes, hi_bytes);
        (self.lo_texels, self.hi_texels) = (lo_texels, hi_texels);
        self.glyphs += file.glyphs().len() + usize::from(file.missing().is_some());
        self.kernings += file.kernings().len();
        self.fnts.push(Fnt {
            file,
            pages: pages.into_boxed_slice(),
        });
        Ok(index)
    }

    /// One file attempt: charged, then counted against [`MAX_FILES`].
    fn attempt(
        &mut self,
        charge: &mut dyn FnMut(Charge) -> Result<(), Refused>,
    ) -> Result<(), FntAdmitError> {
        charge(Charge::FileAttempt).map_err(|_| charged(Charge::FileAttempt))?;
        self.attempts = self.attempts.saturating_add(1);
        if self.attempts > MAX_FILES {
            return Err(FntAdmitError::Limit(FntLimit::Attempts));
        }
        Ok(())
    }

    /// Adds a bitmap font: the admitted `.fnt` file `primary`, then the
    /// `fallbacks` in order, returning its index. No file is read.
    ///
    /// # Errors
    ///
    /// More than [`MAX_BITMAP_FONTS`] fonts or [`MAX_FALLBACKS`]
    /// fallbacks, or a file index not admitted.
    pub fn add_bitmap_font(
        &mut self,
        primary: u16,
        fallbacks: &[u16],
    ) -> Result<u32, BitmapFontError> {
        if self.bitmap_fonts.len() >= MAX_BITMAP_FONTS {
            return Err(BitmapFontError::TooManyFonts);
        }
        if fallbacks.len() > MAX_FALLBACKS {
            return Err(BitmapFontError::TooManyFallbacks);
        }
        if std::iter::once(&primary)
            .chain(fallbacks)
            .any(|&f| usize::from(f) >= self.fnts.len())
        {
            return Err(BitmapFontError::UnknownFile);
        }
        let index =
            u32::try_from(self.bitmap_fonts.len()).map_err(|_| BitmapFontError::TooManyFonts)?;
        let chain = std::iter::once(primary).chain(fallbacks.iter().copied());
        self.bitmap_fonts.push(chain.collect());
        Ok(index)
    }

    /// The files admitted: image files and bitmap font pages.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// The glyphs and kerning pairs the admitted `.fnt` files keep.
    #[must_use]
    pub fn fnt_totals(&self) -> (usize, usize) {
        (self.glyphs, self.kernings)
    }

    /// The sheet, with the cells renumbered in file and rectangle order as
    /// [`Sheet::new`] does.
    #[must_use]
    pub fn finish(self) -> Sheet {
        let mut frames = self.frames;
        let cells = renumber(self.cell_ids, &mut frames);
        Sheet {
            id: NEXT_SHEET.fetch_add(1, Ordering::Relaxed),
            root: self.root,
            files: self.files.into_boxed_slice(),
            images: self.images.into_boxed_slice(),
            frames: frames.into_boxed_slice(),
            cells: cells.into_boxed_slice(),
            fonts: self.fonts.into_boxed_slice(),
            fnts: self.fnts.into_boxed_slice(),
            bitmap_fonts: self.bitmap_fonts.into_boxed_slice(),
        }
    }
}

/// An aggregate limit, or a failure of the file alone.
fn classify(e: ImageError) -> AdmitError {
    match e {
        ImageError::TooManyFiles { .. }
        | ImageError::TotalBytesTooLarge { .. }
        | ImageError::TotalTooLarge { .. } => AdmitError::Limit(e),
        _ => AdmitError::File(e),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use ::image::{ImageFormat, Rgba, RgbaImage};

    use super::super::{HEADER_READS, MAX_SKIN_FRAMES};
    use super::*;
    use crate::quad::Filter;

    /// An image definition and the refusal it must meet.
    type Case = (ImageDef, fn(&ImageError) -> bool);

    fn reads() -> u32 {
        HEADER_READS.with(std::cell::Cell::get)
    }

    fn png(dir: &Path, name: &str, w: u32, h: u32) {
        RgbaImage::from_pixel(w, h, Rgba([1, 2, 3, 255]))
            .save_with_format(dir.join(name), ImageFormat::Png)
            .unwrap();
    }

    fn file(path: &str) -> FileDef {
        FileDef {
            path: path.into(),
            hi: None,
        }
    }

    fn image(file: u32, grid: [u16; 2]) -> ImageDef {
        ImageDef {
            file,
            rect: None,
            grid,
            cycle: None,
            filter: Filter::Linear,
        }
    }

    fn builder(dir: &Path) -> SheetBuilder {
        SheetBuilder::new(&Root::new(dir).unwrap())
    }

    #[test]
    fn a_late_metadata_failure_reads_no_header() {
        let dir = tempfile::tempdir().unwrap();
        png(dir.path(), "a.png", 2, 2);
        let mut b = builder(dir.path());
        let before = reads();
        let def = FileDef {
            path: "a.png".into(),
            hi: Some("missing.png".into()),
        };
        let e = b.admit_file(def).unwrap_err();
        assert!(matches!(e, AdmitError::File(ImageError::Missing { at }) if at.hi));
        assert_eq!(reads(), before);
        assert_eq!(b.file_dims(0), None);
    }

    #[test]
    fn indices_count_admitted_files_and_duplicates_admit_twice() {
        let dir = tempfile::tempdir().unwrap();
        png(dir.path(), "a.png", 4, 2);
        let mut b = builder(dir.path());
        assert!(b.admit_file(file("missing.png")).is_err());
        assert_eq!(b.admit_file(file("a.png")).unwrap(), 0);
        assert_eq!(b.admit_file(file("a.png")).unwrap(), 1);
        assert_eq!(b.add_image(image(1, [2, 1])).unwrap(), 0);
        assert_eq!(b.file_dims(1), Some([4, 2]));
        assert_eq!(b.file_dims(2), None);
        let sheet = b.finish();
        assert_eq!(sheet.image_count(), 1);
        assert_eq!(sheet.frame_count(), 2);
        assert_eq!(sheet.files().len(), 2);
        assert_eq!(sheet.cells()[0].file, 1);
    }

    #[test]
    fn a_hi_version_must_be_twice_the_size() {
        let dir = tempfile::tempdir().unwrap();
        png(dir.path(), "a.png", 4, 2);
        png(dir.path(), "a@2x.png", 8, 5);
        let mut b = builder(dir.path());
        let def = FileDef {
            path: "a.png".into(),
            hi: Some("a@2x.png".into()),
        };
        let e = b.admit_file(def).unwrap_err();
        assert!(matches!(
            e,
            AdmitError::File(ImageError::HiSizeMismatch { file: 0 })
        ));
    }

    #[test]
    fn attempts_are_capped_and_failures_count() {
        let dir = tempfile::tempdir().unwrap();
        png(dir.path(), "a.png", 1, 1);
        let mut b = builder(dir.path());
        for _ in 0..MAX_FILES - 1 {
            assert!(matches!(
                b.admit_file(file("missing.png")),
                Err(AdmitError::File(_))
            ));
        }
        assert_eq!(b.admit_file(file("a.png")).unwrap(), 0);
        let before = reads();
        let e = b.admit_file(file("a.png")).unwrap_err();
        assert!(
            matches!(e, AdmitError::Limit(ImageError::TooManyFiles { n }) if n == MAX_FILES + 1)
        );
        assert_eq!(reads(), before);
    }

    #[test]
    fn a_failed_admission_keeps_the_totals() {
        let dir = tempfile::tempdir().unwrap();
        png(dir.path(), "a.png", 1, 1);
        fs::write(dir.path().join("bad.png"), b"not an image").unwrap();
        let mut b = builder(dir.path());
        b.admit_file(file("a.png")).unwrap();
        let (lo, texels) = (b.lo_bytes.sum, b.lo_texels.sum);
        let before = reads();
        assert!(matches!(
            b.admit_file(file("bad.png")),
            Err(AdmitError::File(ImageError::Unsupported { .. }))
        ));
        // A malformed file's admission reads its header once.
        assert_eq!(reads(), before + 1);
        assert_eq!((b.lo_bytes.sum, b.lo_texels.sum), (lo, texels));
        assert_eq!(b.attempts, 2);
    }

    #[test]
    fn a_running_total_is_a_limit() {
        let dir = tempfile::tempdir().unwrap();
        png(dir.path(), "a.png", 2, 2);
        let mut b = builder(dir.path());
        b.lo_texels.sum = b.lo_texels.cap - 3;
        let e = b.admit_file(file("a.png")).unwrap_err();
        assert!(matches!(
            e,
            AdmitError::Limit(ImageError::TotalTooLarge { .. })
        ));
        b.lo_texels.sum = 0;
        b.lo_bytes.sum = b.lo_bytes.cap;
        let before = reads();
        let e = b.admit_file(file("a.png")).unwrap_err();
        assert!(matches!(
            e,
            AdmitError::Limit(ImageError::TotalBytesTooLarge { .. })
        ));
        assert_eq!(reads(), before);
    }

    #[test]
    fn each_image_refusal_keeps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        png(dir.path(), "a.png", 4, 4);
        let mut b = builder(dir.path());
        b.admit_file(file("a.png")).unwrap();
        let cases: [Case; 6] = [
            (image(1, [1, 1]), |e| {
                matches!(e, ImageError::UnknownFile { image: 0 })
            }),
            (
                ImageDef {
                    cycle: Some(crate::image::CycleDef {
                        period: bms_model::TimeUs(0),
                        timer: crate::skin::TimerRef::Builtin(0),
                    }),
                    ..image(0, [1, 1])
                },
                |e| matches!(e, ImageError::CycleOutOfRange { image: 0 }),
            ),
            (image(0, [0, 1]), |e| {
                matches!(e, ImageError::EmptyGrid { image: 0 })
            }),
            (image(0, [33, 32]), |e| {
                matches!(e, ImageError::TooManyImageFrames { image: 0, n: 1056 })
            }),
            (
                ImageDef {
                    rect: Some([1, 0, 4, 4]),
                    ..image(0, [1, 1])
                },
                |e| matches!(e, ImageError::RectOutside { image: 0 }),
            ),
            (image(0, [5, 1]), |e| {
                matches!(e, ImageError::EmptyCell { image: 0 })
            }),
        ];
        for (def, want) in cases {
            let e = b.add_image(def).unwrap_err();
            assert!(want(&e), "{e:?}");
            assert_eq!((b.images.len(), b.frames.len()), (0, 0));
        }
    }

    #[test]
    fn image_and_frame_caps_hold_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        png(dir.path(), "a.png", 32, 32);
        let mut b = builder(dir.path());
        b.admit_file(file("a.png")).unwrap();
        // 64 images of 1,024 frames reach the frame cap exactly.
        for _ in 0..MAX_SKIN_FRAMES / 1024 {
            b.add_image(image(0, [32, 32])).unwrap();
        }
        assert_eq!(b.frames.len(), MAX_SKIN_FRAMES);
        assert!(matches!(
            b.add_image(image(0, [1, 1])),
            Err(ImageError::TooManySkinFrames)
        ));
        b.frames.clear();
        b.images.resize(MAX_IMAGES - 1, b.images[0]);
        b.add_image(image(0, [1, 1])).unwrap();
        assert!(matches!(
            b.add_image(image(0, [1, 1])),
            Err(ImageError::TooManyImages { n }) if n == MAX_IMAGES + 1
        ));
    }

    #[test]
    fn fonts_are_admitted_one_by_one() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.ttf"), [0, 1, 0, 0, 9]).unwrap();
        fs::write(dir.path().join("bad.ttf"), b"nope").unwrap();
        let mut b = builder(dir.path());
        assert!(matches!(
            b.admit_font(FontDef {
                path: "bad.ttf".into()
            }),
            Err(FontAdmitError::File(FontError::Unsupported { font: 0 }))
        ));
        for i in 0..MAX_FONTS {
            let n = b.admit_font(FontDef {
                path: "a.ttf".into(),
            });
            assert_eq!(usize::from(n.unwrap()), i);
        }
        assert!(matches!(
            b.admit_font(FontDef {
                path: "a.ttf".into()
            }),
            Err(FontAdmitError::Limit(FontError::TooManyFonts { n })) if n == MAX_FONTS + 1
        ));
        assert!(b.root.is_absolute());
        assert_eq!(b.finish().font_count(), MAX_FONTS);
    }

    use crate::image::fnt::{FntErrorKind, MAX_FNT_BYTES};

    fn fnt_reads() -> u32 {
        FNT_READS.with(std::cell::Cell::get)
    }

    /// A `.fnt` file of `glyphs` 1×1 glyphs (from code 100) and `kernings`
    /// pairs, on `pages` pages named `p0.png`, `p1.png`…
    fn fnt_text(glyphs: usize, kernings: usize, pages: usize) -> String {
        let mut s = format!(
            "info face=\"x\" size=8 padding=0,0,0,0\n\
             common lineHeight=8 base=6 scaleW=4 scaleH=4 pages={pages}\n"
        );
        for p in 0..pages {
            s.push_str(&format!("page id={p} file=\"p{p}.png\"\n"));
        }
        for i in 0..glyphs {
            s.push_str(&format!(
                "char id={} x=0 y=0 width=1 height=1 xoffset=0 yoffset=0 xadvance=1 page=0\n",
                100 + i
            ));
        }
        if kernings > 0 {
            s.push_str(&format!("kernings count={kernings}\n"));
            for i in 0..kernings {
                s.push_str(&format!(
                    "kerning first=100 second={} amount=1\n",
                    i % 60_000
                ));
            }
        }
        s
    }

    /// `dir/name` holding `text`, with 4×4 pages `p0.png`… for its count.
    fn write_fnt(dir: &Path, name: &str, text: &str, pages: usize) {
        fs::write(dir.join(name), text).unwrap();
        for p in 0..pages {
            png(dir, &format!("p{p}.png"), 4, 4);
        }
    }

    fn free(_: Charge) -> Result<(), Refused> {
        Ok(())
    }

    fn admit(b: &mut SheetBuilder, name: &str) -> Result<u16, FntAdmitError> {
        b.admit_fnt(name, &mut free)
    }

    /// What a failed admission must leave as it was.
    fn kept(b: &SheetBuilder) -> (usize, usize, usize, u64, u64, (usize, usize), usize) {
        (
            b.files.len(),
            b.images.len(),
            b.frames.len(),
            b.lo_bytes.sum,
            b.lo_texels.sum,
            b.fnt_totals(),
            b.fnts.len(),
        )
    }

    #[test]
    fn a_font_admits_its_pages_as_whole_images() {
        let dir = tempfile::tempdir().unwrap();
        write_fnt(dir.path(), "a.fnt", &fnt_text(3, 2, 2), 2);
        let mut b = builder(dir.path());
        let mut log = Vec::new();
        let i = b
            .admit_fnt("a.fnt", &mut |c| {
                log.push(c);
                Ok(())
            })
            .unwrap();
        assert_eq!(i, 0);
        let len = fs::metadata(dir.path().join("a.fnt")).unwrap().len();
        let page = fs::metadata(dir.path().join("p0.png")).unwrap().len();
        let bytes = Charge::ImageBytes { lo: page, hi: page };
        assert_eq!(
            log,
            [
                Charge::FileAttempt,
                Charge::FontBytes(len),
                Charge::FileAttempt,
                bytes,
                Charge::Image,
                Charge::Frames(1),
                Charge::FileAttempt,
                bytes,
                Charge::Image,
                Charge::Frames(1),
            ]
        );
        // Three glyphs, a synthesised space; two pairs.
        assert_eq!(b.fnt_totals(), (4, 2));
        assert_eq!((b.files.len(), b.images.len(), b.frames.len()), (2, 2, 2));
        assert_eq!(b.add_bitmap_font(0, &[0]).unwrap(), 0);
        let sheet = b.finish();
        assert_eq!(sheet.bitmap_font_count(), 1);
        assert_eq!(sheet.fnts()[0].pages[1].image, 1);
        assert_eq!(sheet.fnts()[0].pages[1].dims, [4, 4]);
        assert_eq!(sheet.images()[1].filter, Filter::Linear);
    }

    #[test]
    fn slots_are_checked_before_anything_is_read() {
        let dir = tempfile::tempdir().unwrap();
        write_fnt(dir.path(), "a.fnt", &fnt_text(1, 0, 1), 1);
        fs::write(dir.path().join("a.ttf"), [0, 1, 0, 0, 9]).unwrap();
        let mut b = builder(dir.path());
        for _ in 0..MAX_FONTS / 2 {
            admit(&mut b, "a.fnt").unwrap();
            b.admit_font(FontDef {
                path: "a.ttf".into(),
            })
            .unwrap();
        }
        let (reads, attempts) = (fnt_reads(), b.attempts);
        let e = admit(&mut b, "a.fnt").unwrap_err();
        assert!(matches!(e, FntAdmitError::Limit(FntLimit::Fonts)));
        assert_eq!((fnt_reads(), b.attempts), (reads, attempts));
        assert!(matches!(
            b.admit_font(FontDef {
                path: "a.ttf".into()
            }),
            Err(FontAdmitError::Limit(FontError::TooManyFonts { .. }))
        ));
    }

    #[test]
    fn metadata_comes_before_any_read() {
        let dir = tempfile::tempdir().unwrap();
        write_fnt(dir.path(), "two.fnt", &fnt_text(1, 0, 2), 1);
        let mut b = builder(dir.path());
        let (fnts, headers) = (fnt_reads(), reads());
        assert!(matches!(
            admit(&mut b, "missing.fnt"),
            Err(FntAdmitError::File(FntFileError::Font(
                FontError::Missing { .. }
            )))
        ));
        assert_eq!(fnt_reads(), fnts);
        // Page 1 is missing: no page header is read.
        let e = admit(&mut b, "two.fnt").unwrap_err();
        let FntAdmitError::File(e) = e else {
            panic!("{e:?}")
        };
        assert_eq!(e.fnt().unwrap().kind, FntErrorKind::PageMissing { page: 1 });
        assert!(e.fnt().unwrap().kind.upstream_fails());
        assert_eq!((fnt_reads(), reads()), (fnts + 1, headers));
    }

    #[test]
    fn a_failure_leaves_the_builder_but_for_attempts_and_font_bytes() {
        let dir = tempfile::tempdir().unwrap();
        write_fnt(dir.path(), "ok.fnt", &fnt_text(2, 1, 1), 1);
        fs::write(dir.path().join("bad.fnt"), "info size=8 padding=0,0,0,0\n").unwrap();
        fs::write(dir.path().join("p1.png"), b"\x89PNG\r\n\x1a\nbroken").unwrap();
        fs::write(dir.path().join("broken.fnt"), fnt_text(2, 1, 2)).unwrap();
        let outside = fnt_text(1, 0, 1).replace("x=0 y=0 width=1", "x=4 y=0 width=1");
        fs::write(dir.path().join("outside.fnt"), outside).unwrap();
        let mut b = builder(dir.path());
        admit(&mut b, "ok.fnt").unwrap();
        let before = kept(&b);
        let (mut attempts, mut font_bytes) = (b.attempts, b.font_bytes.sum);
        for (name, kind, pages) in [
            ("bad.fnt", FntErrorKind::Common, 0),
            ("broken.fnt", FntErrorKind::PageBroken { page: 1 }, 2),
            ("outside.fnt", FntErrorKind::GlyphOutside { code: 100 }, 1),
        ] {
            let len = fs::metadata(dir.path().join(name)).unwrap().len();
            let FntAdmitError::File(e) = admit(&mut b, name).unwrap_err() else {
                panic!("{name}")
            };
            assert_eq!(e.fnt().unwrap().kind, kind, "{name}");
            assert_eq!(kept(&b), before, "{name}");
            assert_eq!(b.attempts, attempts + 1 + pages, "{name}");
            assert_eq!(b.font_bytes.sum, font_bytes + len, "{name}");
            (attempts, font_bytes) = (b.attempts, b.font_bytes.sum);
        }
        // The slot and the capacity are free for the next font.
        assert_eq!(admit(&mut b, "ok.fnt").unwrap(), 1);
        assert_eq!(b.fnt_totals(), (6, 2));
        assert_eq!(b.fnts.len() + b.fonts.len(), 2);
    }

    #[test]
    fn a_refused_charge_stops_before_its_work() {
        let dir = tempfile::tempdir().unwrap();
        write_fnt(dir.path(), "a.fnt", &fnt_text(1, 0, 2), 2);
        let mut b = builder(dir.path());
        let mut images = 0;
        let headers = reads();
        let e = b
            .admit_fnt("a.fnt", &mut |c| {
                if c == Charge::Image {
                    images += 1;
                    if images == 2 {
                        return Err(Refused);
                    }
                }
                Ok(())
            })
            .unwrap_err();
        assert!(matches!(
            e,
            FntAdmitError::Limit(FntLimit::Charge(Charge::Image))
        ));
        assert_eq!(images, 2);
        assert_eq!(reads(), headers);
        assert_eq!((b.images.len(), b.files.len()), (0, 0));
    }

    #[test]
    fn page_attempts_share_the_file_cap() {
        let dir = tempfile::tempdir().unwrap();
        write_fnt(dir.path(), "a.fnt", &fnt_text(1, 0, 1), 1);
        let mut b = builder(dir.path());
        b.attempts = MAX_FILES - 2;
        admit(&mut b, "a.fnt").unwrap();
        b.attempts = MAX_FILES - 1;
        let reads = fnt_reads();
        assert!(matches!(
            admit(&mut b, "a.fnt"),
            Err(FntAdmitError::Limit(FntLimit::Attempts))
        ));
        assert_eq!(fnt_reads(), reads + 1);
    }

    #[test]
    fn font_bytes_are_never_refunded_and_page_bytes_are_image_bytes() {
        let dir = tempfile::tempdir().unwrap();
        // A 4 MiB file the parser refuses: each read is charged.
        let mut big = b"BMF".to_vec();
        big.resize(usize::try_from(MAX_FNT_BYTES).unwrap(), b'x');
        fs::write(dir.path().join("big.fnt"), &big).unwrap();
        let mut b = builder(dir.path());
        for _ in 0..MAX_TOTAL_FONT_BYTES / MAX_FNT_BYTES {
            assert!(matches!(
                admit(&mut b, "big.fnt"),
                Err(FntAdmitError::File(FntFileError::Fnt(_)))
            ));
        }
        let reads = fnt_reads();
        assert!(matches!(
            admit(&mut b, "big.fnt"),
            Err(FntAdmitError::Limit(FntLimit::FontBytes))
        ));
        assert_eq!(fnt_reads(), reads);
        // One byte over the file cap is refused before it is read.
        big.push(b'x');
        fs::write(dir.path().join("over.fnt"), &big).unwrap();
        let mut b = builder(dir.path());
        let FntAdmitError::File(e) = admit(&mut b, "over.fnt").unwrap_err() else {
            panic!()
        };
        assert_eq!(e.fnt().unwrap().kind, FntErrorKind::TooLarge);
        assert_eq!(b.font_bytes.sum, 0);

        // Pages of more than the font bytes in all count as images.
        let dir = tempfile::tempdir().unwrap();
        write_fnt(dir.path(), "a.fnt", &fnt_text(1, 0, 3), 3);
        for p in 0..3 {
            let path = dir.path().join(format!("p{p}.png"));
            let mut bytes = fs::read(&path).unwrap();
            bytes.resize(25 << 20, 0);
            fs::write(&path, bytes).unwrap();
        }
        let mut b = builder(dir.path());
        admit(&mut b, "a.fnt").unwrap();
        assert!(b.lo_bytes.sum > MAX_TOTAL_FONT_BYTES);
        assert!(b.font_bytes.sum < 1 << 10);
    }

    #[test]
    fn sheet_totals_are_limits_and_file_caps_are_not() {
        let dir = tempfile::tempdir().unwrap();
        write_fnt(
            dir.path(),
            "full.fnt",
            &fnt_text(MAX_FONT_GLYPHS - 1, 0, 1),
            1,
        );
        write_fnt(dir.path(), "over.fnt", &fnt_text(MAX_FONT_GLYPHS, 0, 1), 1);
        write_fnt(dir.path(), "one.fnt", &fnt_text(1, 0, 1), 1);
        let mut b = builder(dir.path());
        // One file past its own cap: this file alone.
        let FntAdmitError::File(e) = admit(&mut b, "over.fnt").unwrap_err() else {
            panic!()
        };
        assert_eq!(e.fnt().unwrap().kind, FntErrorKind::TooManyGlyphs);
        for _ in 0..3 {
            admit(&mut b, "full.fnt").unwrap();
        }
        // The sheet has exactly a file's cap left: both are met at the
        // same glyph, and the sheet's limit wins.
        assert!(matches!(
            admit(&mut b, "over.fnt"),
            Err(FntAdmitError::Limit(FntLimit::Glyphs))
        ));
        admit(&mut b, "full.fnt").unwrap();
        assert_eq!(b.fnt_totals().0, MAX_SHEET_GLYPHS);
        // The fifth file meets the sheet's total at its first glyph.
        assert!(matches!(
            admit(&mut b, "one.fnt"),
            Err(FntAdmitError::Limit(FntLimit::Glyphs))
        ));
    }

    #[test]
    fn sheet_kerning_totals_are_limits() {
        let dir = tempfile::tempdir().unwrap();
        write_fnt(
            dir.path(),
            "full.fnt",
            &fnt_text(1, MAX_FONT_KERNINGS, 1),
            1,
        );
        write_fnt(
            dir.path(),
            "over.fnt",
            &fnt_text(1, MAX_FONT_KERNINGS + 1, 1),
            1,
        );
        let mut b = builder(dir.path());
        let FntAdmitError::File(e) = admit(&mut b, "over.fnt").unwrap_err() else {
            panic!()
        };
        assert_eq!(e.fnt().unwrap().kind, FntErrorKind::TooManyKernings);
        for _ in 0..3 {
            admit(&mut b, "full.fnt").unwrap();
        }
        assert!(matches!(
            admit(&mut b, "over.fnt"),
            Err(FntAdmitError::Limit(FntLimit::Kernings))
        ));
        admit(&mut b, "full.fnt").unwrap();
        assert_eq!(b.fnt_totals().1, MAX_SHEET_KERNINGS);
        assert!(matches!(
            admit(&mut b, "full.fnt"),
            Err(FntAdmitError::Limit(FntLimit::Kernings))
        ));
    }

    #[test]
    fn bitmap_fonts_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        write_fnt(dir.path(), "a.fnt", &fnt_text(1, 0, 1), 1);
        let mut b = builder(dir.path());
        admit(&mut b, "a.fnt").unwrap();
        assert_eq!(b.add_bitmap_font(0, &[0; MAX_FALLBACKS]).unwrap(), 0);
        assert_eq!(
            b.add_bitmap_font(0, &[0; MAX_FALLBACKS + 1]),
            Err(BitmapFontError::TooManyFallbacks)
        );
        assert_eq!(b.add_bitmap_font(1, &[]), Err(BitmapFontError::UnknownFile));
        for _ in 1..MAX_BITMAP_FONTS {
            b.add_bitmap_font(0, &[]).unwrap();
        }
        assert_eq!(
            b.add_bitmap_font(0, &[]),
            Err(BitmapFontError::TooManyFonts)
        );
    }

    #[test]
    fn page_limits_on_images_and_frames_are_limits() {
        let dir = tempfile::tempdir().unwrap();
        write_fnt(dir.path(), "a.fnt", &fnt_text(1, 0, 2), 2);
        png(dir.path(), "x.png", 1, 1);
        let mut b = builder(dir.path());
        b.admit_file(file("x.png")).unwrap();
        let one = b.add_image(image(0, [1, 1])).unwrap();
        b.images
            .resize(MAX_IMAGES - 1, b.images[usize::try_from(one).unwrap()]);
        assert!(matches!(
            admit(&mut b, "a.fnt"),
            Err(FntAdmitError::Limit(FntLimit::Images))
        ));
        b.images.truncate(1);
        b.frames.resize(MAX_SKIN_FRAMES - 1, 0);
        assert!(matches!(
            admit(&mut b, "a.fnt"),
            Err(FntAdmitError::Limit(FntLimit::Frames))
        ));
        b.frames.truncate(1);
        admit(&mut b, "a.fnt").unwrap();
    }
}
