//! The saving extension point and local records, and the atomic file writes
//! that settings and records share.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use replay::PlayRecord;

/// Creates a temporary file beside `path` under a name no other writer
/// uses (this process's id and a counter, created exclusively) and writes
/// `bytes` to it.
fn write_temp(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let mut name = path.as_os_str().to_owned();
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        name.push(format!(".{}.{n}.tmp", std::process::id()));
        let tmp = PathBuf::from(name);
        match File::create_new(&tmp) {
            Ok(mut file) => {
                let written = file.write_all(bytes).and_then(|()| file.sync_all());
                return written.map(|()| tmp.clone()).inspect_err(|_| {
                    let _ = fs::remove_file(&tmp);
                });
            }
            // Left by a writer that stopped; take the next name.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
}

/// Writes `bytes` to `path` so a reader sees the old file or the new one,
/// never a part: the bytes go to a temporary file beside it, which is then
/// renamed over `path`.
///
/// # Errors
///
/// The I/O error of the write or the rename; the temporary file is removed
/// when the rename fails.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = write_temp(path, bytes)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
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

/// Numbers tried past the highest record found before saving gives up:
/// that many writers would have to publish in the same moment.
pub const SAVE_ATTEMPTS: u64 = 64;

/// Saves records as `<records>/<chart sha256>/<n>.tprl`, numbering each
/// chart's plays from 1. A record is written to a temporary file and then
/// published under the next free number, created exclusively, so plays
/// saved at the same moment, even by two sessions, never replace or mix
/// with one another.
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
        let tmp = write_temp(&dir.join("record"), &replay::encode(rec))?;
        // A hard link fails when the name exists, so a record is never
        // replaced; a number taken in the meantime moves on to the next.
        let mut published = Err(io::Error::from(io::ErrorKind::AlreadyExists));
        for n in last + 1..=last + SAVE_ATTEMPTS {
            let path = dir.join(format!("{n}.tprl"));
            match fs::hard_link(&tmp, &path) {
                Ok(()) => {
                    published = Ok(path);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    published = Err(e);
                    break;
                }
            }
        }
        let _ = fs::remove_file(&tmp);
        Ok(RecordRef { path: published? })
    }
}
