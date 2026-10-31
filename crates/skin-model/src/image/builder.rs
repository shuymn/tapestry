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
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use super::font::{self, FontDef, FontError, MAX_FONTS, MAX_TOTAL_FONT_BYTES};
use super::path::Root;
use super::{
    Cell, File, FileDef, Image, ImageDef, ImageError, MAX_FILES, MAX_IMAGES, NEXT_SHEET, Sheet,
    Total, add_frames, check_cycle, cut, image_frames, push_frames, read_file, renumber,
    resolve_file,
};

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
    /// [`MAX_FONTS`] fonts; its name, path and metadata, and the running
    /// byte total; then its first four bytes. Nothing is kept unless every
    /// check passes.
    ///
    /// # Errors
    ///
    /// [`FontAdmitError::Limit`] for the count and the running total;
    /// [`FontAdmitError::File`] for anything else.
    pub fn admit_font(&mut self, def: FontDef) -> Result<u16, FontAdmitError> {
        let index = self.fonts.len();
        if index >= MAX_FONTS {
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
}
