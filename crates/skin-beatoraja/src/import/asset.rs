//! Assets: file choices, source paths, file admission through the
//! model's [`SheetBuilder`] with request and failure caches, images per
//! (object, filter, cut), and fonts.

use std::collections::HashMap;

use skin_model::{
    AdmitError, FileDef, FontAdmitError, FontDef, ImageDef, ImageError, MAX_PATH_BYTES, Root,
    Sheet, SheetBuilder,
};

use super::Config;
use super::budget::{Budget, ImportLimit};
use super::report::{FileChoice, Rule};
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
    /// The file failed a check ([`Assets::take_error`]).
    File,
}

/// A font as a text element uses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Font {
    File(u16),
    Failed,
}

/// An image cache key: the source object, a linear filter, and the cut
/// (columns and rows kept).
pub(crate) type ImageKey = (u32, bool, [u32; 2]);

/// The source rows an image keeps of its single frame: the first row and
/// the count.
pub(crate) type Crop = [u32; 2];

/// The sheet being built, with the importer's caches.
pub(crate) struct Assets {
    builder: SheetBuilder,
    /// Admitted files and cached failures, by name and @2x name.
    files: HashMap<(String, Option<String>), Result<u32, Option<ImageError>>>,
    /// Each key's uncropped image and cell, once one of its images is
    /// added.
    bases: HashMap<ImageKey, (ImageDef, [u32; 2])>,
    images: HashMap<(ImageKey, Option<Crop>), (u32, [u32; 2])>,
    fonts: HashMap<String, Font>,
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
        }
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
            return Ok(r.as_ref().copied().map_err(|_| Failed::File));
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
            Err(AdmitError::File(e)) => (Err(Failed::File), Err(Some(e))),
        };
        self.files.insert(key, entry);
        Ok(result)
    }

    /// The error a failed file was cached with, taken: a critical user
    /// refuses the import with it.
    pub(crate) fn take_error(&mut self, path: &str) -> Option<ImageError> {
        let key = (path.to_owned(), None);
        match self.files.get_mut(&key) {
            Some(Err(e)) => e.take(),
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
