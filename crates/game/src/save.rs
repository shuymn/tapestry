//! The saving extension point and local records, and the atomic file writes
//! that settings and records share.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use replay::PlayRecord;
use tempfile::NamedTempFile;

/// A complete temporary file in `dir`, under a name no other writer uses; it
/// is removed if dropped before it is published.
fn temp_in(dir: &Path, bytes: &[u8]) -> io::Result<NamedTempFile> {
    let mut tmp = NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    Ok(tmp)
}

/// Writes `bytes` to `path` so a reader sees the old file or the new one,
/// never a part: the bytes go to a temporary file beside it, which is then
/// renamed over `path`.
///
/// # Errors
///
/// The I/O error of the write or the rename; the temporary file is removed
/// either way.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    temp_in(path.parent().unwrap_or_else(|| Path::new(".")), bytes)?
        .persist(path)
        .map(|_| ())
        .map_err(|e| e.error)
}

/// Where a saved record went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordRef {
    /// The file written.
    pub path: PathBuf,
}

/// Why a record could not be saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveError {
    /// The file system refused.
    Io {
        /// The kind of failure.
        kind: io::ErrorKind,
        /// The error, as the system reported it.
        message: String,
    },
}

impl From<io::Error> for SaveError {
    fn from(e: io::Error) -> Self {
        Self::Io {
            kind: e.kind(),
            message: e.to_string(),
        }
    }
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message, .. } => write!(f, "the record cannot be saved: {message}"),
        }
    }
}

impl std::error::Error for SaveError {}

/// The saving extension point: receives each finished play's record. A
/// failure is reported, and the play itself is unaffected.
pub trait RecordSink {
    /// Saves one record.
    ///
    /// # Errors
    ///
    /// Why it could not be saved.
    fn save(&mut self, rec: &PlayRecord) -> Result<RecordRef, SaveError>;
}

/// Numbers tried from the first free one before saving gives up: that many
/// writers would have to publish in the same moment.
pub const SAVE_ATTEMPTS: u64 = 64;

/// Saves records as `<records>/<chart sha256>/<n>.tprl`, numbering each
/// chart's plays from 1. A record is written whole to a temporary file and
/// then renamed to the next free number by a rename that refuses to
/// replace, so plays saved at the same moment, even by two sessions, never
/// replace or mix with one another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalRecords {
    dir: PathBuf,
}

impl LocalRecords {
    /// Records under `dir` (the data root's `records`).
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

fn numbers_exhausted() -> io::Error {
    io::Error::other("no record number is left")
}

impl RecordSink for LocalRecords {
    fn save(&mut self, rec: &PlayRecord) -> Result<RecordRef, SaveError> {
        let hex: String = rec
            .chart
            .identity
            .sha256
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let dir = self.dir.join(hex);
        fs::create_dir_all(&dir)?;
        // The highest record file; other names do not count.
        let mut last = 0_u64;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = Path::new(&name);
            let number = name
                .file_stem()
                .filter(|_| name.extension().is_some_and(|e| e == "tprl"))
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok());
            last = last.max(number.unwrap_or(0));
        }
        let first = last.checked_add(1).ok_or_else(numbers_exhausted)?;
        let path = publish(&dir, &replay::encode(rec), first)?;
        Ok(RecordRef { path })
    }
}

/// Writes `bytes` as `<first>.tprl` in `dir`, or the next number free when
/// that one is taken, never replacing a file.
fn publish(dir: &Path, bytes: &[u8], first: u64) -> io::Result<PathBuf> {
    let mut tmp = temp_in(dir, bytes)?;
    let mut n = first;
    for _ in 0..SAVE_ATTEMPTS {
        let path = dir.join(format!("{n}.tprl"));
        match tmp.persist_noclobber(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => tmp = e.file,
            Err(e) => return Err(e.error),
        }
        n = n.checked_add(1).ok_or_else(numbers_exhausted)?;
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Barrier;

    use super::publish;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_taken_number_is_never_replaced() {
        let dir = dir();
        fs::write(dir.path().join("1.tprl"), b"first").unwrap();
        let path = publish(dir.path(), b"second", 1).unwrap();
        assert_eq!(path, dir.path().join("2.tprl"));
        assert_eq!(fs::read(dir.path().join("1.tprl")).unwrap(), b"first");
        assert_eq!(fs::read(&path).unwrap(), b"second");
    }

    #[test]
    fn two_publishers_competing_for_one_number_both_land() {
        let dir = dir();
        let barrier = Barrier::new(2);
        let paths: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = [b"writer a", b"writer b"]
                .into_iter()
                .map(|bytes| {
                    let (dir, barrier) = (dir.path(), &barrier);
                    s.spawn(move || {
                        barrier.wait();
                        (publish(dir, bytes, 1).unwrap(), bytes)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut names: Vec<_> = paths.iter().map(|(p, _)| p.clone()).collect();
        names.sort();
        assert_eq!(
            names,
            [dir.path().join("1.tprl"), dir.path().join("2.tprl")]
        );
        for (path, bytes) in &paths {
            assert_eq!(fs::read(path).unwrap(), *bytes);
        }
    }

    #[test]
    fn numbering_past_the_last_number_is_an_error() {
        let dir = dir();
        fs::write(dir.path().join(format!("{}.tprl", u64::MAX)), b"last").unwrap();
        assert_eq!(
            publish(dir.path(), b"next", u64::MAX).unwrap_err().kind(),
            std::io::ErrorKind::Other
        );
        // Nothing but the existing record is left.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
