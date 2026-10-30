//! Skin file names to regular files confined to the skin folder.
//!
//! The checks cover the folder's contents as installed: a name cannot leave
//! the folder by itself, and a link or junction cannot lead out of it. A
//! process that rewrites the folder while the player loads it can race the
//! checks; it already runs as the user and can read those files itself, so
//! that is outside the skin threat model (design.md "Threat Model"). A hard
//! link inside the folder to a file outside it cannot be told apart from a
//! copy.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fs::{self, File};
use std::io;
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

/// Whether `name` is a relative path that cannot leave the folder by
/// itself: at most [`MAX_PATH_BYTES`], not empty, not rooted (`/`, `\`,
/// UNC), no drive or stream colon, and no `..` component.
fn is_confined_name(name: &str) -> bool {
    name.len() <= MAX_PATH_BYTES
        && !name.is_empty()
        && !name.starts_with(['/', '\\'])
        && !name.contains(':')
        && name.split(['/', '\\']).all(|part| part != "..")
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

#[cfg(test)]
mod tests {
    use super::is_confined_name;
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
}
