//! Assets: file choices, source paths, file admission through the
//! model's [`SheetBuilder`] with request and failure caches, images per
//! (object, filter, cut), and fonts.

use std::collections::HashMap;

use skin_model::{
    AdmitError, BitmapFontError, Charge, FileDef, Filter, FntAdmitError, FntError, FntFileError,
    FntLimit, FontAdmitError, FontDef, FontError, ImageDef, ImageError, MAX_PATH_BYTES, Refused,
    Root, Sheet, SheetBuilder, SolidImageDef,
};

use super::Config;
use super::budget::{Budget, ImportLimit};
use super::report::{FileChoice, NoteFallback, Rule};
use crate::header::Header;

/// The movie extensions the source plays as images
/// (`VideoFormat.java:7-13` at `0.8.9`).
const MOVIES: [&str; 8] = [
    ".mp4", ".m4v", ".wmv", ".webm", ".mpg", ".mpeg", ".m1v", ".m2v",
];
const AVI: &str = ".avi";

/// The candidate file choice `spec` uses, by owner decision 2 (D8,
/// `SkinConfiguration.java:300-345`): the configured name if it is a
/// candidate; else the first candidate equal to `def` ignoring ASCII case
/// or whose stem is; else the first candidate.
pub(crate) fn choose(
    candidates: &[String],
    name: Option<&str>,
    def: Option<&str>,
    config: &Config,
) -> (Option<usize>, Rule, bool) {
    if let Some(chosen) = name.and_then(|n| config.files.get(n))
        && let Some(i) = candidates.iter().position(|c| c == chosen)
    {
        return (Some(i), Rule::Configured, false);
    }
    if let Some(def) = def {
        let matches = |c: &String| {
            c.eq_ignore_ascii_case(def)
                || c.rsplit_once('.')
                    .is_some_and(|(stem, _)| stem.eq_ignore_ascii_case(def))
        };
        let mut hits = candidates.iter().enumerate().filter(|(_, c)| matches(c));
        if let Some((i, _)) = hits.next() {
            return (Some(i), Rule::Default, hits.next().is_some());
        }
    }
    if candidates.is_empty() {
        (None, Rule::None, false)
    } else {
        (Some(0), Rule::First, false)
    }
}

/// A source path after file-choice substitution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Path {
    /// A name relative to the skin folder.
    File(String),
    /// Covered by a choice with no candidate.
    Missing,
    /// A glob no choice covers: the source picks a match at random.
    Glob,
}

/// The file choices as path prefixes (`SkinLoader.java:95-130`): each
/// choice's path and the candidate it uses, if any.
#[derive(Debug, Default)]
pub(crate) struct Choices {
    prefixes: Vec<(String, Option<String>)>,
}

impl Choices {
    /// Decides every file choice of `header`, reporting each.
    pub(crate) fn new(header: &Header, config: &Config, out: &mut Vec<FileChoice>) -> Self {
        let mut prefixes = Vec::with_capacity(header.files.len());
        for (choice, spec) in (0_u16..).zip(&header.files) {
            let (pick, rule, ambiguous) = choose(
                &spec.candidates,
                spec.name.as_deref(),
                spec.def.as_deref(),
                config,
            );
            out.push(FileChoice {
                choice,
                candidate: pick.and_then(|i| u16::try_from(i).ok()),
                rule,
                ambiguous,
            });
            if let Some(path) = &spec.path {
                let chosen = pick.and_then(|i| spec.candidates.get(i)).cloned();
                prefixes.push((path.clone(), chosen));
            }
        }
        Self { prefixes }
    }

    /// Source path `path`, relative to the definition's folder, with the
    /// first choice whose path prefixes it substituted: the part up to its
    /// last `*` replaced by the chosen name.
    pub(crate) fn substitute(&self, path: &str) -> Path {
        for (prefix, chosen) in &self.prefixes {
            if let Some(foot) = path.strip_prefix(prefix.as_str()) {
                let Some(chosen) = chosen else {
                    return Path::Missing;
                };
                let head = path.rfind('*').map_or("", |i| path.get(..i).unwrap_or(""));
                return Path::File(format!("{head}{chosen}{foot}"));
            }
        }
        if path.contains('*') {
            Path::Glob
        } else {
            Path::File(path.to_owned())
        }
    }
}

/// Whether `path` names a movie.
pub(crate) fn is_movie(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    MOVIES.iter().chain([&AVI]).any(|ext| lower.ends_with(ext))
}

/// `name` joined to the definition's folder `dir`.
pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// Why a file was not admitted, for its users.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failed {
    /// A name of the wrong form: no file was looked at.
    Name,
    /// The file failed a check ([`Assets::take_error`]): `refused` when
    /// only this importer's confinement, format or limits refuse it, so
    /// the source may draw it, else it is missing or unreadable for the
    /// source too.
    File { refused: bool },
}

/// A cached file failure: whether it was refused ([`Failed::File`]) and
/// its error until a critical user takes it.
type FileFailure = (bool, Option<ImageError>);

/// Whether `e` refuses a file the source may still draw: its name or
/// place, its size or dimensions past the caps, a header past the
/// decoder's allocation cap, or a format only the source decodes.
fn refused(e: &ImageError) -> bool {
    matches!(
        e,
        ImageError::Decode {
            source: image::ImageError::Limits(_),
            ..
        } | ImageError::PathRejected { .. }
            | ImageError::PathEscapes { .. }
            | ImageError::FileTooLarge { .. }
            | ImageError::TotalBytesTooLarge { .. }
            | ImageError::HeaderTooLarge { .. }
            | ImageError::Unsupported { .. }
            | ImageError::TooWide { .. }
            | ImageError::TooManyTexels { .. }
            | ImageError::TotalTooLarge { .. }
    )
}

/// A font as a text element uses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Font {
    File(u16),
    Failed,
}

/// Why a `.fnt` file was not admitted, for its users.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FntFailure {
    /// The file could not be found or read.
    Missing,
    /// Its name or size refused by the folder's confinement and file
    /// limits, where the source may load it.
    Refused,
    /// Its text or a page was refused.
    Fnt(FntError),
}

/// An image cache key: the source object, a linear filter, the cut
/// (columns and rows kept), and whether an image choice uses it, without
/// the sheet-level cycle its entries replace.
pub(crate) type ImageKey = (u32, bool, [u32; 2], bool);

/// The source rows an image keeps of its single frame: the first row and
/// the count.
pub(crate) type Crop = [u32; 2];

/// The size of a note part's generated paint, as the source generates
/// it (`play/SkinNote.java:113-138`).
pub(crate) const FALLBACK_SIZE: [u32; 2] = [32, 8];

/// The sheet being built, with the importer's caches.
pub(crate) struct Assets {
    builder: SheetBuilder,
    /// Admitted files and cached failures (whether refused, and the error
    /// until a critical user takes it), by name and @2x name.
    files: HashMap<(String, Option<String>), Result<u32, FileFailure>>,
    /// Each key's uncropped image and cell, once one of its images is
    /// added.
    bases: HashMap<ImageKey, (ImageDef, [u32; 2])>,
    images: HashMap<(ImageKey, Option<Crop>), (u32, [u32; 2])>,
    fonts: HashMap<String, Font>,
    /// Admitted `.fnt` files and cached failures, by name.
    fnt_files: HashMap<String, Result<u16, FntFailure>>,
    /// Bitmap fonts by font record, or the primary file's failure.
    bitmap_fonts: HashMap<u32, Result<u32, FntFailure>>,
    /// The generated note part images, by [`NoteFallback`]: at most
    /// three, however many parts draw them.
    note_fallbacks: [Option<u32>; 3],
}

#[cfg(test)]
thread_local! {
    /// The `admit_file` calls this thread has made: a header is read at
    /// most once per call.
    pub(crate) static ADMITS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

impl Assets {
    pub(crate) fn new(root: &Root) -> Self {
        Self {
            builder: SheetBuilder::new(root),
            files: HashMap::new(),
            bases: HashMap::new(),
            images: HashMap::new(),
            fonts: HashMap::new(),
            fnt_files: HashMap::new(),
            bitmap_fonts: HashMap::new(),
            note_fallbacks: [None; 3],
        }
    }

    /// The generated image of `paint`, added once: its image and frame
    /// are charged before it is added, and a later request charges
    /// nothing. No file is admitted or attempted.
    ///
    /// # Errors
    ///
    /// The image or frame budget, or a variant's texel total.
    pub(crate) fn note_fallback(
        &mut self,
        budget: &mut Budget,
        paint: NoteFallback,
    ) -> Result<u32, ImportLimit> {
        // The cache slot and the straight RGBA: the source's white,
        // yellow and red.
        let (slot, color) = match paint {
            NoteFallback::Tap => (0, [255, 255, 255, 255]),
            NoteFallback::Long => (1, [255, 255, 0, 255]),
            NoteFallback::Mine => (2, [255, 0, 0, 255]),
        };
        if let Some(&Some(i)) = self.note_fallbacks.get(slot) {
            return Ok(i);
        }
        budget.image(1)?;
        let def = SolidImageDef {
            size: FALLBACK_SIZE,
            color,
            filter: Filter::Nearest,
        };
        let i = self.builder.add_solid_image(def).map_err(|e| match e {
            ImageError::TooManyImages { .. } => ImportLimit::Images,
            ImageError::TooManySkinFrames => ImportLimit::Frames,
            _ => ImportLimit::FileTotals,
        })?;
        if let Some(cached) = self.note_fallbacks.get_mut(slot) {
            *cached = Some(i);
        }
        Ok(i)
    }

    /// Admits `def` once: a repeated request returns the first outcome. A
    /// name of the wrong form counts an attempt and is not cached, so each
    /// request counts; a file that fails a check is cached.
    ///
    /// # Errors
    ///
    /// An aggregate limit: the attempts, or a variant's totals.
    pub(crate) fn admit(
        &mut self,
        budget: &mut Budget,
        def: FileDef,
    ) -> Result<Result<u32, Failed>, ImportLimit> {
        let key = (def.path, def.hi);
        if let Some(r) = self.files.get(&key) {
            return Ok(r
                .as_ref()
                .copied()
                .map_err(|&(refused, _)| Failed::File { refused }));
        }
        budget.file_attempt()?;
        let names = std::iter::once(&key.0).chain(key.1.as_ref());
        if names
            .into_iter()
            .any(|n| n.len() > MAX_PATH_BYTES || Root::check_name(n).is_err())
        {
            return Ok(Err(Failed::Name));
        }
        #[cfg(test)]
        ADMITS.with(|n| n.set(n.get() + 1));
        #[cfg(test)]
        super::trace(super::Event::Admit);
        let def = FileDef {
            path: key.0.clone(),
            hi: key.1.clone(),
        };
        let (result, entry) = match self.builder.admit_file(def) {
            Ok(i) => {
                budget.usage.files = budget.usage.files.saturating_add(1);
                (Ok(i), Ok(i))
            }
            Err(AdmitError::Limit(ImageError::TooManyFiles { .. })) => {
                return Err(ImportLimit::FileAttempts);
            }
            Err(AdmitError::Limit(_)) => return Err(ImportLimit::FileTotals),
            Err(AdmitError::File(e)) => {
                let refused = refused(&e);
                (Err(Failed::File { refused }), Err((refused, Some(e))))
            }
        };
        self.files.insert(key, entry);
        Ok(result)
    }

    /// The error a failed file was cached with, taken: a critical user
    /// refuses the import with it.
    pub(crate) fn take_error(&mut self, path: &str) -> Option<ImageError> {
        let key = (path.to_owned(), None);
        match self.files.get_mut(&key) {
            Some(Err((_, e))) => e.take(),
            _ => None,
        }
    }

    pub(crate) fn file_dims(&self, file: u32) -> Option<[u32; 2]> {
        self.builder.file_dims(file)
    }

    /// The model image for `key` cropped to `crop` and its cell size, if
    /// one was added.
    pub(crate) fn image(&self, key: ImageKey, crop: Option<Crop>) -> Option<(u32, [u32; 2])> {
        self.images.get(&(key, crop)).copied()
    }

    /// `key`'s uncropped image and cell, once one of its images was added.
    pub(crate) fn base(&self, key: ImageKey) -> Option<(ImageDef, [u32; 2])> {
        self.bases.get(&key).copied()
    }

    /// Adds image `def` with cells of `cell` under `key`, its single
    /// frame cut to the rows of `crop` when there is one; returns the
    /// image and its cell.
    pub(crate) fn add_image(
        &mut self,
        key: ImageKey,
        crop: Option<Crop>,
        def: ImageDef,
        cell: [u32; 2],
    ) -> Result<(u32, [u32; 2]), ImageError> {
        let (added, added_cell) = match (crop, def.rect) {
            (Some([y, rows]), Some([x, _, w, _])) => (
                ImageDef {
                    rect: Some([x, y, w, rows]),
                    ..def
                },
                [cell[0], rows],
            ),
            _ => (def, cell),
        };
        let i = self.builder.add_image(added)?;
        self.bases.insert(key, (def, cell));
        self.images.insert((key, crop), (i, added_cell));
        Ok((i, added_cell))
    }

    /// Admits font file `path` once.
    ///
    /// # Errors
    ///
    /// The font count or total bytes.
    pub(crate) fn font(&mut self, budget: &mut Budget, path: &str) -> Result<Font, ImportLimit> {
        if let Some(&f) = self.fonts.get(path) {
            return Ok(f);
        }
        let font = match self.builder.admit_font(FontDef {
            path: path.to_owned(),
        }) {
            Ok(i) => {
                budget.usage.fonts = budget.usage.fonts.saturating_add(1);
                Font::File(i)
            }
            Err(FontAdmitError::Limit(_)) => return Err(ImportLimit::Fonts),
            Err(FontAdmitError::File(_)) => Font::Failed,
        };
        self.fonts.insert(path.to_owned(), font);
        Ok(font)
    }

    /// Admits `.fnt` file `name` and its pages once: a repeated request
    /// returns the first outcome. Each attempt, image and frame is charged
    /// to `budget` as the builder reaches it; the builder's own totals
    /// bound the bytes, glyphs and kerning pairs.
    ///
    /// # Errors
    ///
    /// An aggregate limit.
    pub(crate) fn fnt_file(
        &mut self,
        budget: &mut Budget,
        name: &str,
    ) -> Result<Result<u16, FntFailure>, ImportLimit> {
        if let Some(&r) = self.fnt_files.get(name) {
            return Ok(r);
        }
        let files = self.builder.file_count();
        let admitted = self
            .builder
            .admit_fnt(name, &mut |c| budget.fnt_charge(c).map_err(|_| Refused));
        let (glyphs, kernings) = self.builder.fnt_totals();
        budget.usage.glyphs = u32::try_from(glyphs).unwrap_or(u32::MAX);
        budget.usage.kernings = u32::try_from(kernings).unwrap_or(u32::MAX);
        let result = match admitted {
            Ok(i) => {
                budget.usage.fonts = budget.usage.fonts.saturating_add(1);
                // Its pages are files of the sheet.
                let pages = self.builder.file_count().saturating_sub(files);
                budget.usage.files = budget
                    .usage
                    .files
                    .saturating_add(u32::try_from(pages).unwrap_or(u32::MAX));
                Ok(i)
            }
            Err(FntAdmitError::Limit(l)) => {
                return Err(match l {
                    // The limit `Budget::fnt_charge` refuses each charge with.
                    FntLimit::Charge(c) => match c {
                        Charge::FileAttempt => ImportLimit::FileAttempts,
                        Charge::Image => ImportLimit::Images,
                        Charge::Frames(_) => ImportLimit::Frames,
                        Charge::FontBytes(_) => ImportLimit::Fonts,
                        Charge::ImageBytes { .. } => ImportLimit::FileTotals,
                    },
                    FntLimit::Attempts => ImportLimit::FileAttempts,
                    FntLimit::Fonts | FntLimit::FontBytes => ImportLimit::Fonts,
                    FntLimit::ImageBytes | FntLimit::Texels => ImportLimit::FileTotals,
                    FntLimit::Images => ImportLimit::Images,
                    FntLimit::Frames => ImportLimit::Frames,
                    FntLimit::Glyphs => ImportLimit::Glyphs,
                    FntLimit::Kernings => ImportLimit::Kernings,
                });
            }
            Err(FntAdmitError::File(FntFileError::Font(
                FontError::PathRejected { .. }
                | FontError::PathEscapes { .. }
                | FontError::TooLarge { .. },
            ))) => Err(FntFailure::Refused),
            Err(FntAdmitError::File(e)) => {
                Err(e.fnt().map_or(FntFailure::Missing, FntFailure::Fnt))
            }
        };
        self.fnt_files.insert(name.to_owned(), result);
        Ok(result)
    }

    /// The bitmap font made for font record `rec`, or its primary file's
    /// failure.
    pub(crate) fn bitmap_font(&self, rec: u32) -> Option<Result<u32, FntFailure>> {
        self.bitmap_fonts.get(&rec).copied()
    }

    /// Records font record `rec`'s outcome: a bitmap font of admitted
    /// files `primary` and `fallbacks`, or the primary's failure.
    ///
    /// # Errors
    ///
    /// More bitmap fonts than the model holds.
    pub(crate) fn add_bitmap_font(
        &mut self,
        rec: u32,
        files: Result<(u16, &[u16]), FntFailure>,
    ) -> Result<Result<u32, FntFailure>, ImportLimit> {
        let outcome = match files {
            Ok((primary, fallbacks)) => match self.builder.add_bitmap_font(primary, fallbacks) {
                Ok(i) => Ok(i),
                Err(BitmapFontError::UnknownFile) => Err(FntFailure::Missing),
                Err(BitmapFontError::TooManyFonts | BitmapFontError::TooManyFallbacks) => {
                    return Err(ImportLimit::Fonts);
                }
            },
            Err(f) => Err(f),
        };
        self.bitmap_fonts.insert(rec, outcome);
        Ok(outcome)
    }

    pub(crate) fn finish(self) -> Sheet {
        self.builder.finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn choice_rules_in_order() {
        let mut config = Config::default();
        let c = names(&["Dark.png", "dark", "light.png"]);
        assert_eq!(
            choose(&c, Some("bg"), Some("DARK"), &config),
            (Some(0), Rule::Default, true)
        );
        config.files = BTreeMap::from([("bg".to_owned(), "light.png".to_owned())]);
        assert_eq!(
            choose(&c, Some("bg"), Some("dark"), &config),
            (Some(2), Rule::Configured, false)
        );
        config.files = BTreeMap::from([("bg".to_owned(), "Random".to_owned())]);
        assert_eq!(
            choose(&c, Some("bg"), Some("none"), &config),
            (Some(0), Rule::First, false)
        );
        assert_eq!(
            choose(&[], Some("bg"), None, &config),
            (None, Rule::None, false)
        );
    }

    /// A `w`×`h` PNG at `dir/name`.
    fn png(dir: &std::path::Path, name: &str, w: u32, h: u32) {
        let file = std::fs::File::create(dir.join(name)).unwrap();
        let mut enc = png::Encoder::new(file, w, h);
        enc.set_color(png::ColorType::Rgba);
        let mut out = enc.write_header().unwrap();
        out.write_image_data(&vec![255; (w * h * 4) as usize])
            .unwrap();
    }

    #[test]
    fn a_bitmap_page_past_the_image_budget_stops_before_it_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let two = "info face=\"t\" size=8 padding=0,0,0,0\n\
                   common lineHeight=8 base=6 scaleW=4 scaleH=4 pages=2\n\
                   page id=0 file=\"a.png\"\npage id=1 file=\"b.png\"\n\
                   char id=65 x=0 y=0 width=1 height=1 xoffset=0 yoffset=0 xadvance=1 page=1\n";
        std::fs::write(dir.path().join("p.fnt"), two).unwrap();
        png(dir.path(), "a.png", 4, 4);
        png(dir.path(), "b.png", 4, 4);
        let mut assets = Assets::new(&Root::new(dir.path()).unwrap());
        // All but one image allowance spent on images that failed: the
        // builder holds none of them.
        let mut budget = Budget::default();
        budget.usage.images = u32::try_from(skin_model::MAX_IMAGES - 1).unwrap();
        assert_eq!(
            assets.fnt_file(&mut budget, "p.fnt"),
            Err(ImportLimit::Images)
        );
        // The file and both pages were attempted; one page's image and
        // frame were charged.
        assert_eq!(budget.usage.file_attempts, 3);
        assert_eq!(budget.usage.images as usize, skin_model::MAX_IMAGES);
        assert_eq!(budget.usage.frames, 1);
        assert_eq!(budget.usage.fonts, 0);
        // With the allowance, the font is admitted once.
        let mut budget = Budget::default();
        let mut assets = Assets::new(&Root::new(dir.path()).unwrap());
        assert_eq!(assets.fnt_file(&mut budget, "p.fnt"), Ok(Ok(0)));
        assert_eq!(assets.fnt_file(&mut budget, "p.fnt"), Ok(Ok(0)));
        assert_eq!((budget.usage.images, budget.usage.file_attempts), (2, 3));
        assert_eq!((budget.usage.fonts, budget.usage.glyphs), (1, 2));
    }

    #[test]
    fn a_fallback_is_charged_once_and_shared() {
        let dir = tempfile::tempdir().unwrap();
        let mut assets = Assets::new(&Root::new(dir.path()).unwrap());
        // One image allowance left: the first paint takes it.
        let mut budget = Budget::default();
        budget.usage.images = u32::try_from(skin_model::MAX_IMAGES - 1).unwrap();
        assert_eq!(assets.note_fallback(&mut budget, NoteFallback::Tap), Ok(0));
        assert_eq!(budget.usage.frames, 1);
        // A miss at the cap stops; the cached paint is still shared and
        // charges nothing more.
        assert_eq!(
            assets.note_fallback(&mut budget, NoteFallback::Long),
            Err(ImportLimit::Images)
        );
        let charged = budget.usage;
        assert_eq!(assets.note_fallback(&mut budget, NoteFallback::Tap), Ok(0));
        assert_eq!(budget.usage, charged);
        // The frame cap stops a miss the same way.
        let mut budget = Budget::default();
        budget.usage.frames = u32::try_from(skin_model::MAX_SKIN_FRAMES).unwrap();
        assert_eq!(
            assets.note_fallback(&mut budget, NoteFallback::Mine),
            Err(ImportLimit::Frames)
        );
        assert_eq!(assets.note_fallback(&mut budget, NoteFallback::Tap), Ok(0));
        // No file was attempted.
        assert_eq!(budget.usage.file_attempts, 0);
        assert_eq!(assets.finish().image_count(), 1);
    }

    #[test]
    fn a_failed_file_keeps_its_class_on_every_reference() {
        let dir = tempfile::tempdir().unwrap();
        // A WebP file: only the source decodes it.
        std::fs::write(
            dir.path().join("w.webp"),
            b"RIFF\x1a\0\0\0WEBPVP8L\x0d\0\0\0/\0\0\0\x10\x07\x10\x11\x11\x88\x88\xfe\x07\0",
        )
        .unwrap();
        let mut assets = Assets::new(&Root::new(dir.path()).unwrap());
        let mut budget = Budget::default();
        ADMITS.with(|n| n.set(0));
        let mut admit = |assets: &mut Assets, path: &str| {
            let def = FileDef {
                path: path.to_owned(),
                hi: None,
            };
            assets.admit(&mut budget, def).unwrap()
        };
        for (path, refused) in [("w.webp", true), ("lost.png", false)] {
            let failed = Err(Failed::File { refused });
            assert_eq!(admit(&mut assets, path), failed, "{path}");
            assert_eq!(admit(&mut assets, path), failed, "{path}");
            // A critical user takes the error once; later references keep
            // the class.
            assert!(assets.take_error(path).is_some(), "{path}");
            assert_eq!(admit(&mut assets, path), failed, "{path}");
            assert!(assets.take_error(path).is_none(), "{path}");
            assert_eq!(admit(&mut assets, path), failed, "{path}");
        }
        // Each file was read once and none admitted.
        assert_eq!(ADMITS.with(std::cell::Cell::get), 2);
        assert_eq!((budget.usage.file_attempts, budget.usage.files), (2, 0));
    }

    #[test]
    fn paths_substitute_by_prefix() {
        let c = Choices {
            prefixes: vec![
                ("bg/*.png".into(), Some("dark.png".into())),
                ("fx/*".into(), None),
            ],
        };
        assert_eq!(c.substitute("bg/*.png"), Path::File("bg/dark.png".into()));
        assert_eq!(c.substitute("fx/*"), Path::Missing);
        assert_eq!(c.substitute("etc/*.png"), Path::Glob);
        assert_eq!(c.substitute("a.png"), Path::File("a.png".into()));
        assert!(is_movie("x/A.MP4") && !is_movie("a.png"));
    }
}
