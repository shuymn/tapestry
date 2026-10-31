//! Skin file names to regular files confined to the skin folder.
//!
//! The checks cover the folder's contents as installed: a name cannot leave
//! the folder by itself, and a link or junction cannot lead out of it. A
//! process that rewrites the folder while the player loads it can race the
//! checks; it already runs as the user and can read those files itself, so
//! that is outside the skin threat model (design.md "Threat Model"). A hard
//! link inside the folder to a file outside it cannot be told apart from a
//! copy.
//!
//! Image and font names may not contain `..` at all. A [`Root`] serves a
//! skin definition's reader: its names may step up with `..` as long as
//! they stay inside the folder, and its reads and listings are bounded by
//! the caller.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use super::{MAX_FILE_BYTES, MAX_PATH_BYTES};

/// Why a name did not lead to a file that may be read.
#[derive(Debug)]
pub(crate) enum PathError {
    Rejected,
    Missing,
    Escapes,
    NotAFile,
    TooLarge,
    Io(io::Error),
}

/// Whether `name` has the form of a relative path, checked before any
/// normalisation: at most [`MAX_PATH_BYTES`], not empty, not rooted (`/`,
/// `\`, which also covers UNC and verbatim `\\?\` names), and no drive
/// or stream colon. Both spellings of the separator count on every
/// platform.
fn is_relative_form(name: &str) -> bool {
    name.len() <= MAX_PATH_BYTES
        && !name.is_empty()
        && !name.starts_with(['/', '\\'])
        && !name.contains(':')
}

/// Whether `name` is a relative path that cannot leave the folder by
/// itself: [`is_relative_form`] and no `..` component.
fn is_confined_name(name: &str) -> bool {
    is_relative_form(name) && name.split(['/', '\\']).all(|part| part != "..")
}

/// `name` with `.` and empty components dropped and each `..` taking off
/// the component before it, joined with `/`. `None` when a `..` has
/// nothing left to take off: the name leads above the folder.
fn normalise(name: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// A length that may be read: at most [`MAX_FILE_BYTES`].
fn checked_len(meta: &fs::Metadata) -> Result<u64, PathError> {
    if !meta.is_file() {
        return Err(PathError::NotAFile);
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(PathError::TooLarge);
    }
    Ok(meta.len())
}

/// Resolves `name` against the canonical skin folder `root` without
/// opening it: the name's form, then the canonical path inside `root`,
/// then a regular file of at most [`MAX_FILE_BYTES`]. The file-type check
/// runs before any open, because opening a FIFO blocks.
pub(crate) fn resolve(root: &Path, name: &str) -> Result<(PathBuf, u64), PathError> {
    if !is_confined_name(name) {
        return Err(PathError::Rejected);
    }
    let joined = root.join(name.replace('\\', "/"));
    let canonical = match joined.canonicalize() {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(PathError::Missing),
        Err(e) => return Err(PathError::Io(e)),
    };
    if !canonical.starts_with(root) {
        return Err(PathError::Escapes);
    }
    let meta = fs::metadata(&canonical).map_err(PathError::Io)?;
    let len = checked_len(&meta)?;
    Ok((canonical, len))
}

/// Opens a path [`resolve`] returned and checks the handle again: still a
/// regular file of at most [`MAX_FILE_BYTES`]. Returns the file and its
/// length.
pub(crate) fn open(canonical: &Path) -> Result<(File, u64), PathError> {
    let file = File::open(canonical).map_err(PathError::Io)?;
    let meta = file.metadata().map_err(PathError::Io)?;
    let len = checked_len(&meta)?;
    Ok((file, len))
}

/// Why a [`Root`] refused a name or could not read or list it.
#[derive(Debug, thiserror::Error)]
pub enum RootError {
    #[error(
        "the name is not a relative path (empty, rooted, with a drive or stream colon, \
         or longer than {MAX_PATH_BYTES} bytes)"
    )]
    Rejected,
    #[error("the name leads above the skin folder")]
    Underflow,
    #[error("no such file or folder")]
    Missing,
    #[error("the name resolves outside the skin folder")]
    Escapes,
    #[error("not a regular file")]
    NotAFile,
    #[error("not a folder")]
    NotAFolder,
    #[error("the file is {len} bytes, more than the {max} allowed")]
    TooLarge { len: u64, max: u64 },
    #[error("the folder holds more than the {max} entries allowed")]
    TooManyEntries { max: usize },
    #[error("cannot open the skin folder: {0}")]
    Folder(#[source] io::Error),
    #[error(transparent)]
    Io(io::Error),
}

/// A skin folder, canonical, that names are read and listed against.
#[derive(Clone, Debug)]
pub struct Root {
    dir: PathBuf,
}

/// The identity of a file a [`Root`] read: equal for every name that
/// reaches the same file through the same canonical path, links and other
/// spellings included. [`FileId::name`] is the name it was read by,
/// normalised.
#[derive(Clone, Debug)]
pub struct FileId {
    canonical: PathBuf,
    name: String,
}

impl FileId {
    /// The name the file was read by, relative to the folder, normalised:
    /// `/` separators, no `.`, `..` or empty components.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl PartialEq for FileId {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }
}

impl Eq for FileId {}

impl Hash for FileId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.canonical.hash(state);
    }
}

/// An entry of a listed folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// A folder rather than a regular file.
    pub folder: bool,
}

/// A listed folder: its regular files and folders inside the skin folder,
/// sorted by name, and how many entries were enumerated to find them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// Every entry seen, the skipped ones included.
    pub enumerated: usize,
}

impl Root {
    /// The skin folder `dir`, canonicalised.
    ///
    /// # Errors
    ///
    /// [`RootError::Folder`] when `dir` cannot be canonicalised, and
    /// [`RootError::NotAFolder`] when it is not a folder.
    pub fn new(dir: &Path) -> Result<Self, RootError> {
        let dir = dir.canonicalize().map_err(RootError::Folder)?;
        if !dir.is_dir() {
            return Err(RootError::NotAFolder);
        }
        Ok(Self { dir })
    }

    /// Checks the form of `name` as [`Root::read`] and [`Root::list`] do
    /// first, before it is normalised or touches the file system: at most
    /// [`MAX_PATH_BYTES`], not empty, not rooted, drive, UNC or verbatim,
    /// and no `:`. For a name whose parts are checked before it is split.
    ///
    /// # Errors
    ///
    /// [`RootError::Rejected`] when the form is wrong.
    pub fn check_name(name: &str) -> Result<(), RootError> {
        if is_relative_form(name) {
            Ok(())
        } else {
            Err(RootError::Rejected)
        }
    }

    /// The canonical path `name` leads to inside the folder, and the
    /// normalised name. The form is checked before normalising, so a
    /// rooted or drive name cannot become relative by it; then `..`
    /// may not step above the folder; then the canonical path, links
    /// followed, must stay inside the folder.
    fn locate(&self, name: &str) -> Result<(PathBuf, String), RootError> {
        Self::check_name(name)?;
        let normal = normalise(name).ok_or(RootError::Underflow)?;
        let canonical = match self.dir.join(&normal).canonicalize() {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(RootError::Missing),
            Err(e) => return Err(RootError::Io(e)),
        };
        if !canonical.starts_with(&self.dir) {
            return Err(RootError::Escapes);
        }
        Ok((canonical, normal))
    }

    /// Reads the regular file `name`, at most `max_bytes` long, and
    /// returns its bytes and identity.
    ///
    /// The name and the canonical path are checked as in
    /// [`Root::list`]; then the file's type and length from its metadata,
    /// before it is opened (opening a FIFO blocks); then the same on the
    /// opened handle. A file that grows past `max_bytes` while it is read
    /// is refused without reading more than one byte past the bound.
    ///
    /// # Errors
    ///
    /// The first check that fails, or [`RootError::Io`].
    pub fn read(&self, name: &str, max_bytes: u64) -> Result<(Vec<u8>, FileId), RootError> {
        let (canonical, normal) = self.locate(name)?;
        if normal.is_empty() {
            return Err(RootError::NotAFile);
        }
        let bounded = |meta: &fs::Metadata| {
            if !meta.is_file() {
                return Err(RootError::NotAFile);
            }
            if meta.len() > max_bytes {
                return Err(RootError::TooLarge {
                    len: meta.len(),
                    max: max_bytes,
                });
            }
            Ok(meta.len())
        };
        bounded(&fs::metadata(&canonical).map_err(RootError::Io)?)?;
        let file = File::open(&canonical).map_err(RootError::Io)?;
        let len = bounded(&file.metadata().map_err(RootError::Io)?)?;
        let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(RootError::Io)?;
        let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if read > max_bytes {
            return Err(RootError::TooLarge {
                len: read,
                max: max_bytes,
            });
        }
        Ok((
            bytes,
            FileId {
                canonical,
                name: normal,
            },
        ))
    }

    /// Lists folder `dir` (`""` is the skin folder itself): its regular
    /// files and folders whose canonical paths stay inside the skin
    /// folder, sorted by name. Entries whose names are not UTF-8, links
    /// out of the folder, dangling links and other file types are
    /// skipped.
    ///
    /// The name is checked as in [`Root::read`] and the folder's canonical
    /// path must stay inside the skin folder before anything is
    /// enumerated. Every enumerated entry counts against `max_entries`
    /// before it is examined, skipped ones included.
    ///
    /// # Errors
    ///
    /// The first check that fails, [`RootError::TooManyEntries`] at entry
    /// `max_entries + 1`, or [`RootError::Io`].
    pub fn list(&self, dir: &str, max_entries: usize) -> Result<Listing, RootError> {
        let canonical = if dir.is_empty() {
            self.dir.clone()
        } else {
            self.locate(dir)?.0
        };
        if !fs::metadata(&canonical).map_err(RootError::Io)?.is_dir() {
            return Err(RootError::NotAFolder);
        }
        let mut entries = Vec::new();
        let mut enumerated = 0;
        for entry in fs::read_dir(&canonical).map_err(RootError::Io)? {
            enumerated += 1;
            if enumerated > max_entries {
                return Err(RootError::TooManyEntries { max: max_entries });
            }
            let entry = entry.map_err(RootError::Io)?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let path = entry.path();
            let linked = entry.file_type().map_err(RootError::Io)?.is_symlink();
            if linked && !path.canonicalize().is_ok_and(|c| c.starts_with(&self.dir)) {
                continue;
            }
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if meta.is_file() || meta.is_dir() {
                entries.push(Entry {
                    name,
                    folder: meta.is_dir(),
                });
            }
        }
        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(Listing {
            entries,
            enumerated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{is_confined_name, is_relative_form, normalise};
    use crate::image::MAX_PATH_BYTES;

    #[test]
    fn names_leaving_the_folder_are_refused() {
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        for bad in [
            "",
            "../x.png",
            "..\\x.png",
            "a/../../x.png",
            "/x.png",
            "\\x.png",
            "\\\\server\\s\\x.png",
            "C:\\x.png",
            "c:x.png",
            "x.png:s",
            long.as_str(),
        ] {
            assert!(!is_confined_name(bad), "{bad}");
        }
        let at_cap = "a".repeat(MAX_PATH_BYTES);
        for good in ["x.png", "a/b.png", "a\\b.png", "..x.png", at_cap.as_str()] {
            assert!(is_confined_name(good), "{good}");
        }
    }

    #[test]
    fn the_form_is_checked_on_every_platform() {
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        for bad in [
            "",
            "/x",
            "\\x",
            "\\\\server\\s\\x",
            "//server/s/x",
            "\\\\?\\C:\\x",
            "//?/C:/x",
            "\\\\?\\UNC\\server\\s",
            "C:\\x",
            "c:x",
            "x:s",
            long.as_str(),
        ] {
            assert!(!is_relative_form(bad), "{bad}");
        }
        for good in ["x", "a/../b", "..\\x", "./x"] {
            assert!(is_relative_form(good), "{good}");
        }
    }

    #[test]
    fn normalising_drops_dots_and_refuses_underflow() {
        assert_eq!(normalise("a/./b//c").as_deref(), Some("a/b/c"));
        assert_eq!(normalise("a\\..\\b").as_deref(), Some("b"));
        assert_eq!(
            normalise("play/../_font/x.ttf").as_deref(),
            Some("_font/x.ttf")
        );
        assert_eq!(normalise("a/..").as_deref(), Some(""));
        assert_eq!(normalise(".."), None);
        assert_eq!(normalise("a/../../x"), None);
        assert_eq!(normalise("../a/x"), None);
    }
}
