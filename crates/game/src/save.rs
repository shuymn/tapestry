//! The saving extension point and local records, and the atomic file write
//! that settings and records share.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use replay::PlayRecord;

/// Writes `bytes` to `path` so a reader sees the old file or the new one,
/// never a part: the bytes go to a temporary file beside it, which is then
/// renamed over `path`.
///
/// # Errors
///
/// The I/O error of the write or the rename; the temporary file is removed
/// when the rename fails.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes)?;
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

/// Saves records as `<records>/<chart sha256>/<n>.tprl`, numbering each
/// chart's plays from 1, so plays saved within the same second never
/// collide.
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
        let mut last = 0_u64;
        for entry in fs::read_dir(&dir)? {
            let name = entry?.file_name();
            let number = Path::new(&name)
                .file_stem()
                .filter(|_| Path::new(&name).extension().is_some_and(|e| e == "tprl"))
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok());
            last = last.max(number.unwrap_or(0));
        }
        let path = dir.join(format!("{}.tprl", last + 1));
        write_atomic(&path, &replay::encode(rec))?;
        Ok(RecordRef { path })
    }
}
