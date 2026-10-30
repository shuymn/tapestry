//! The library database (design.md "Loading and Song Selection"): one row per
//! chart file under the configured roots, with its size, modification time,
//! file hashes and descriptive header, kept current by a background [`Scan`].
//!
//! # Schema version 1
//!
//! [`SCHEMA_VERSION`] is kept in `PRAGMA user_version`.
//!
//! - `roots(id, path UNIQUE)`: the roots of the latest scan.
//! - `charts(id, root, path UNIQUE, size, mtime_ns, sha256, md5, title,
//!   subtitle, artist, subartist, genre, playlevel, difficulty, bpm)`: the
//!   header values as [`bms_parser::read_header`] reads them, `NULL` when
//!   undeclared.
//!
//! Everything in the database is derived from the chart files, so a file of
//! any other version is deleted with its `-wal` and `-shm` files and created
//! anew, and the next scan reads every chart; only the version is read from
//! the old file. [`Library::open`] says so in [`Opened::Rebuilt`] for the
//! caller to log, since this crate never logs.
//!
//! The database is in WAL mode. A scan's writer thread owns the one writer
//! connection; [`Library`] holds the reader connection of the thread that
//! queries it. Both come from the [`Library`], the only owner of every
//! connection to its file in this process: a second one of the same file is
//! refused while the first or its scan lives, and one scan runs at a time.
//! A rebuild happens in [`Library::open`] before any connection exists.
//! Keeping other processes away from the file is the caller's part. Each
//! stored header value is cut to [`HEADER_FIELD_BYTES`], so a batch has a
//! fixed bound in bytes.

#![forbid(unsafe_code)]
#![deny(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

mod db;
mod scan;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::Connection;

pub use bms_parser::Header;
pub use scan::{
    BATCH, HEADER_FIELD_BYTES, PARSERS, Scan, ScanError, ScanOptions, ScanReport, ThreadStart,
    Worker,
};

/// The schema version this build reads and writes.
pub const SCHEMA_VERSION: i64 = 1;

/// Why the library database cannot be opened, read or scanned.
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    /// SQLite refused.
    #[error("the library database cannot be used: {}", path.display())]
    Database {
        /// The database file.
        path: PathBuf,
        /// What SQLite said.
        #[source]
        source: rusqlite::Error,
    },
    /// A file of another schema version could not be deleted.
    #[error("the old library database cannot be deleted: {}", path.display())]
    Remove {
        /// The file that stayed.
        path: PathBuf,
        /// What the file system said.
        #[source]
        source: io::Error,
    },
    /// A scan thread could not be started.
    #[error("a library scan thread cannot be started")]
    Spawn(#[source] io::Error),
    /// The folder of the database file cannot be resolved.
    #[error("the library database's folder cannot be found: {}", path.display())]
    Locate {
        /// The database file.
        path: PathBuf,
        /// What the file system said.
        #[source]
        source: io::Error,
    },
    /// Another [`Library`] of this file, or its scan, lives in this process.
    #[error("the library database is already open: {}", path.display())]
    AlreadyOpen {
        /// The database file.
        path: PathBuf,
    },
    /// A scan of this library is still running.
    #[error("a library scan is already running")]
    ScanRunning,
}

/// What [`Library::open`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opened {
    /// There was no database; an empty one was created.
    Created,
    /// The database has this build's schema version.
    Current,
    /// The database had another schema version; it was deleted and created
    /// anew, empty.
    Rebuilt {
        /// The version the old file held.
        old_version: i64,
    },
}

impl fmt::Display for Opened {
    /// The line to log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => write!(f, "library: created schema version {SCHEMA_VERSION}"),
            Self::Current => write!(f, "library: schema version {SCHEMA_VERSION}"),
            Self::Rebuilt { old_version } => write!(
                f,
                "library: schema version {old_version} is not {SCHEMA_VERSION}; \
                 the database was deleted and is rebuilt by a full scan"
            ),
        }
    }
}

/// One indexed chart file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartEntry {
    /// The file.
    pub path: PathBuf,
    /// Its size in bytes when it was read.
    pub size: u64,
    /// Its modification time when it was read, ns since the Unix epoch.
    pub mtime_ns: i64,
    /// SHA-256 of its bytes.
    pub sha256: [u8; 32],
    /// MD5 of its bytes.
    pub md5: [u8; 16],
    /// Its descriptive header.
    pub header: Header,
}

/// The database files a [`Library`] of this process holds, by their
/// canonical path.
static OPEN: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// A database file held by this process: by its [`Library`] and by the
/// lease of its running scan, whichever ends last.
#[derive(Debug)]
struct Owner {
    key: PathBuf,
    scanning: AtomicBool,
}

impl Owner {
    /// Takes the file at canonical path `key`, if no other owner holds it.
    fn take(key: PathBuf) -> Option<Arc<Self>> {
        let mut open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
        if open.contains(&key) {
            return None;
        }
        open.push(key.clone());
        Some(Arc::new(Self {
            key,
            scanning: AtomicBool::new(false),
        }))
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        OPEN.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|key| *key != self.key);
    }
}

/// The right to run the one scan of a [`Library`], released when the scan's
/// writer thread ends, however it ends.
#[derive(Debug)]
pub(crate) struct ScanLease(Arc<Owner>);

impl Drop for ScanLease {
    fn drop(&mut self) {
        self.0.scanning.store(false, Ordering::Release);
    }
}

/// The canonical path of database file `path`: its folder, which must exist,
/// resolved, and its name.
fn canonical(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    Ok(dir.canonicalize()?.join(name))
}

/// The library database. It is the only owner of every connection to its
/// file in this process: its own reader connection, and the writer
/// connection of its scan. A second [`Library`] of the same file is refused
/// while either lives, so a rebuild in [`open`](Self::open) never deletes a
/// file under a live connection. Other processes are kept out by the data
/// root's owner, not by this crate.
#[derive(Debug)]
pub struct Library {
    path: PathBuf,
    // Before `owner`, so the connection closes before the file is released.
    reader: Connection,
    owner: Arc<Owner>,
}

impl Library {
    /// Opens the database at `path`, creating it when it is missing, or
    /// deleting and creating it when it holds another schema version. Both
    /// happen before this process holds any connection to the file.
    ///
    /// # Errors
    ///
    /// [`LibraryError::AlreadyOpen`] while another [`Library`] of this file,
    /// or its scan, lives in this process; [`LibraryError::Locate`] when its
    /// folder cannot be resolved; [`LibraryError::Database`] when SQLite
    /// refuses the file; and [`LibraryError::Remove`] when a file of another
    /// version cannot be deleted.
    pub fn open(path: &Path) -> Result<(Self, Opened), LibraryError> {
        let key = canonical(path).map_err(|source| LibraryError::Locate {
            path: path.to_owned(),
            source,
        })?;
        let owner = Owner::take(key).ok_or_else(|| LibraryError::AlreadyOpen {
            path: path.to_owned(),
        })?;
        let database = |source| LibraryError::Database {
            path: path.to_owned(),
            source,
        };
        let opened = match db::stored_version(path).map_err(database)? {
            None => Opened::Created,
            Some(SCHEMA_VERSION) => Opened::Current,
            Some(old_version) => Opened::Rebuilt { old_version },
        };
        if opened != Opened::Current {
            db::remove(path)?;
        }
        let reader = db::connect(path).map_err(database)?;
        if opened != Opened::Current {
            db::create(&reader).map_err(database)?;
        }
        Ok((
            Self {
                path: path.to_owned(),
                reader,
                owner,
            },
            opened,
        ))
    }

    /// Every indexed chart, by path.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses the query.
    pub fn charts(&self) -> Result<Vec<ChartEntry>, LibraryError> {
        db::charts(&self.reader).map_err(|source| LibraryError::Database {
            path: self.path.clone(),
            source,
        })
    }

    /// Starts a scan of `roots` on its own threads, with its own writer
    /// connection (design.md "Coexisting with Other Software": it pauses
    /// during play, and [`ScanOptions::on_thread_start`] can lower its
    /// threads' priority). One scan runs at a time: the next can start once
    /// [`Scan::wait_finished`] holds or the scan is dropped.
    ///
    /// # Errors
    ///
    /// [`LibraryError::ScanRunning`] while another scan of this library
    /// runs, [`LibraryError::Database`] when the writer connection cannot be
    /// opened, and [`LibraryError::Spawn`] when a thread cannot be started.
    pub fn scan(&self, roots: &[PathBuf], options: ScanOptions) -> Result<Scan, LibraryError> {
        if self.owner.scanning.swap(true, Ordering::AcqRel) {
            return Err(LibraryError::ScanRunning);
        }
        let lease = ScanLease(Arc::clone(&self.owner));
        let writer = db::connect(&self.path).map_err(|source| LibraryError::Database {
            path: self.path.clone(),
            source,
        })?;
        scan::start(writer, roots.to_vec(), options, lease).map_err(LibraryError::Spawn)
    }

    /// The reader connection, for tests that change the database under a
    /// scan.
    #[cfg(test)]
    pub(crate) const fn reader(&self) -> &Connection {
        &self.reader
    }
}
