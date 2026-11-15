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
//! the caller. A [`Root`] name is walked from the folder one component at
//! a time, each link judged by its own target before anything below it
//! is looked at (design.md "Threat Model" T08), so whether a name is
//! missing never depends on anything outside the folder.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::collections::VecDeque;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use super::source::FileSource;
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
/// `\`, which also covers UNC and verbatim `\\?\` names), no drive or
/// stream colon and no NUL. Both spellings of the separator count on
/// every platform.
fn is_relative_form(name: &str) -> bool {
    name.len() <= MAX_PATH_BYTES
        && !name.is_empty()
        && !name.starts_with(['/', '\\'])
        && !name.contains([':', '\0'])
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
        "the name is not a relative path (empty, rooted, with a drive or stream colon \
         or a NUL, or longer than {MAX_PATH_BYTES} bytes)"
    )]
    Rejected,
    #[error("the name leads above the skin folder")]
    Underflow,
    #[error("no such file or folder")]
    Missing,
    #[error("the name resolves outside the skin folder")]
    Escapes,
    #[error(
        "a link on the path leads nowhere inside the skin folder (its target is missing, \
         asks for a child under a file or is too long, or more than {MAX_LINK_HOPS} links \
         chain)"
    )]
    Dangling,
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

/// A skin folder, canonical, that names are read and listed against,
/// with the [`FileSource`] its image and font content is read through
/// (the disk unless [`Root::with_source`] gives another). Its own
/// [`read`](Root::read) and [`list`](Root::list), for definitions and
/// scripts, always reach the disk.
#[derive(Clone, Debug)]
pub struct Root {
    dir: PathBuf,
    /// The folder as it was named, made absolute without resolving
    /// links: a link inside may spell its target this way.
    as_given: PathBuf,
    source: FileSource,
}

/// The most links one [`Root`] name's walk expands, a loop included: a
/// provisional engineering limit, not a measured one (an acyclic chain
/// of more is refused too).
pub const MAX_LINK_HOPS: u32 = 32;

/// Where a component a walk has queued came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum From {
    /// The name the caller asked for.
    Name,
    /// A link's target.
    Link,
}

/// Whether `e` says an entry is not there: none at that name, or a
/// component on the way is not a folder.
fn absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// An absolute path split lexically into its anchor (a drive `C:`, a
/// UNC share `\\server\share`, or `/`) and its components, `.` and empty
/// ones dropped and `..` kept: what a `..` steps up from depends on the
/// links before it, which only a walk sees. `None` for a form a link's
/// target may not take: a device, a volume GUID, a drive-relative or a
/// driveless rooted path. A verbatim drive or UNC prefix reads as the
/// plain one.
fn anchored(path: &str) -> Option<(String, Vec<String>)> {
    let sep = |c: char| c == '/' || c == '\\';
    let (anchor, rest) = if cfg!(windows) {
        let plain = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else if let Some(v) = path.strip_prefix(r"\\?\") {
            v.to_owned()
        } else if path.starts_with(r"\\.\") || path.starts_with(r"\??\") {
            return None;
        } else {
            path.to_owned()
        };
        let bytes = plain.as_bytes();
        if bytes.len() >= 3
            && bytes.first().is_some_and(u8::is_ascii_alphabetic)
            && bytes.get(1) == Some(&b':')
            && bytes.get(2).is_some_and(|&b| b == b'/' || b == b'\\')
        {
            (plain.get(..2)?.to_owned(), plain.get(3..)?.to_owned())
        } else {
            let unc = plain.strip_prefix(r"\\").or(plain.strip_prefix("//"))?;
            let mut parts = unc.splitn(3, sep);
            let server = parts.next().filter(|s| !s.is_empty())?;
            let share = parts.next().filter(|s| !s.is_empty())?;
            (
                format!(r"\\{server}\{share}"),
                parts.next().unwrap_or("").to_owned(),
            )
        }
    } else {
        (String::from("/"), path.strip_prefix('/')?.to_owned())
    };
    let parts = rest
        .split(sep)
        .filter(|p| !p.is_empty() && *p != ".")
        .map(str::to_owned)
        .collect();
    Some((anchor, parts))
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
        let as_given = std::path::absolute(dir).map_err(RootError::Folder)?;
        let dir = dir.canonicalize().map_err(RootError::Folder)?;
        if !dir.is_dir() {
            return Err(RootError::NotAFolder);
        }
        Ok(Self {
            dir,
            as_given,
            source: FileSource::disk(),
        })
    }

    /// The same folder, its image and font content read through `source`.
    #[must_use]
    pub fn with_source(&self, source: FileSource) -> Self {
        Self {
            dir: self.dir.clone(),
            as_given: self.as_given.clone(),
            source,
        }
    }

    /// The source its image and font content is read through.
    #[must_use]
    pub fn source(&self) -> &FileSource {
        &self.source
    }

    /// The canonical folder.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Checks the form of `name` as [`Root::read`] and [`Root::list`] do
    /// first, before it is normalised or touches the file system: at most
    /// [`MAX_PATH_BYTES`], not empty, not rooted, drive, UNC or verbatim,
    /// and no `:` or NUL. For a name whose parts are checked before it is split.
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

    /// `name` normalised as [`Root::read`] and [`Root::list`] take it,
    /// before anything touches the file system: the form checked as by
    /// [`Root::check_name`], so a rooted or drive name cannot become
    /// relative by normalising; then `.` and empty components dropped and
    /// each `..` taking off the component before it, joined with `/`.
    ///
    /// # Errors
    ///
    /// [`RootError::Rejected`] when the form is wrong, and
    /// [`RootError::Underflow`] when a `..` leads above the folder.
    pub fn normalise(name: &str) -> Result<String, RootError> {
        Self::check_name(name)?;
        normalise(name).ok_or(RootError::Underflow)
    }

    /// The canonical path `name` leads to inside the folder, and the
    /// normalised name ([`Root::normalise`]): the name walked from the
    /// folder ([`Root::walk`]), then the path it reached canonicalised,
    /// which must still lie inside the folder.
    fn locate(&self, name: &str) -> Result<(PathBuf, String), RootError> {
        let normal = Self::normalise(name)?;
        let reached = self.walk(&normal)?;
        // The walk saw every entry; one gone now was removed meanwhile.
        let canonical = match reached.canonicalize() {
            Ok(c) => c,
            Err(e) if absent(&e) => return Err(RootError::Missing),
            Err(e) => return Err(RootError::Io(e)),
        };
        if !canonical.starts_with(&self.dir) {
            return Err(RootError::Escapes);
        }
        Ok((canonical, normal))
    }

    /// Walks the normalised name `normal` from the folder one component
    /// at a time, and returns the path it reaches: inside the folder, with
    /// no link on it. Each link met is judged by its own target, read and
    /// never followed, before anything below it is looked at: a target
    /// outside the folder ([`Root::inside`]) is [`RootError::Escapes`]
    /// whatever lies beyond it, and an inward one is walked again from the
    /// folder. A target's `..` steps up from the folder the walk has
    /// reached, which holds no link, so the components before it are
    /// walked first; one above the folder is [`RootError::Escapes`]. An
    /// absent entry is [`RootError::Missing`] when it is the name's own,
    /// and [`RootError::Dangling`] when a link's target asked for it; a
    /// file with components left is a path through a file,
    /// decided the same way by the next component's provenance. More than
    /// [`MAX_LINK_HOPS`] links in one walk, one counter for the whole of
    /// it, are [`RootError::Dangling`]: any loop ends so.
    fn walk(&self, normal: &str) -> Result<PathBuf, RootError> {
        let mut queue: VecDeque<(String, From)> = normal
            .split('/')
            .filter(|p| !p.is_empty())
            .map(|p| (p.to_owned(), From::Name))
            .collect();
        let mut at = self.dir.clone();
        let mut hops = 0;
        while let Some((part, from)) = queue.pop_front() {
            if part == ".." {
                if at == self.dir {
                    return Err(RootError::Escapes);
                }
                at.pop();
                continue;
            }
            let next = at.join(&part);
            let meta = match fs::symlink_metadata(&next) {
                Ok(m) => m,
                Err(e) if absent(&e) => {
                    return Err(match from {
                        From::Name => RootError::Missing,
                        From::Link => RootError::Dangling,
                    });
                }
                Err(e) => return Err(RootError::Io(e)),
            };
            if meta.file_type().is_symlink() {
                hops += 1;
                if hops > MAX_LINK_HOPS {
                    return Err(RootError::Dangling);
                }
                let target = fs::read_link(&next).map_err(RootError::Io)?;
                let rest = self.inside(&at, &target)?;
                for p in rest.into_iter().rev() {
                    queue.push_front((p, From::Link));
                }
                at.clone_from(&self.dir);
            } else if meta.is_dir() {
                at = next;
            } else if let Some(&(_, after)) = queue.front() {
                return Err(match after {
                    From::Name => RootError::Missing,
                    From::Link => RootError::Dangling,
                });
            } else {
                at = next;
            }
        }
        Ok(at)
    }

    /// The components below the folder that the link target `target`,
    /// read in the folder `at`, names, its `..` kept for the walk,
    /// compared lexically and exactly with the folder's canonical path and
    /// the path it was opened by:
    /// [`RootError::Escapes`] when it names neither or takes a form a
    /// target may not ([`anchored`]), [`RootError::Dangling`] when it is
    /// longer than [`MAX_PATH_BYTES`]. Nothing is looked up.
    fn inside(&self, at: &Path, target: &Path) -> Result<Vec<String>, RootError> {
        let target = target.to_str().ok_or(RootError::Escapes)?;
        if target.len() > MAX_PATH_BYTES {
            return Err(RootError::Dangling);
        }
        let relative = if cfg!(windows) {
            let b = target.as_bytes();
            !(target.starts_with(['/', '\\']) || b.get(1) == Some(&b':'))
        } else {
            !target.starts_with('/')
        };
        let full = if relative {
            let base = at.to_str().ok_or(RootError::Escapes)?;
            format!("{base}/{target}")
        } else {
            target.to_owned()
        };
        let (anchor, parts) = anchored(&full).ok_or(RootError::Escapes)?;
        for root in [&self.dir, &self.as_given] {
            let Some((root_anchor, root_parts)) = root.to_str().and_then(anchored) else {
                continue;
            };
            if anchor == root_anchor && parts.starts_with(&root_parts) {
                return Ok(parts.get(root_parts.len()..).unwrap_or(&[]).to_vec());
            }
        }
        Err(RootError::Escapes)
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
        let (canonical, normal) = if dir.is_empty() {
            (self.dir.clone(), String::new())
        } else {
            self.locate(dir)?
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
            let mut path = entry.path();
            // A link is kept only when its walk stays inside: one that
            // leads out, nowhere, or out and back in is skipped. A kept one
            // is described by the path its walk reached, never by
            // following the link again.
            if entry.file_type().map_err(RootError::Io)?.is_symlink() {
                let named = if normal.is_empty() {
                    name.clone()
                } else {
                    format!("{normal}/{name}")
                };
                let Ok(reached) = self.walk(&named) else {
                    continue;
                };
                path = reached;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
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
            "a\0.png",
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
            "a\0b",
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
