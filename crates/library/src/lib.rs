//! The library database (design.md "Loading and Song Selection"): one row per
//! chart file under the configured roots, with its size, modification time,
//! file hashes and descriptive header, kept current by a background [`Scan`].
//!
//! # Schema version 2
//!
//! [`SCHEMA_VERSION`] is kept in `PRAGMA user_version`.
//!
//! - `roots(id, path UNIQUE)`: the roots of the latest scan.
//! - `charts(id, root, path UNIQUE, size, mtime_ns, sha256, md5, title,
//!   subtitle, artist, subartist, genre, playlevel, difficulty, bpm,
//!   norm_text, title_key, artist_key, folder)`: the header values as
//!   [`bms_parser::read_header`] reads them, `NULL` when undeclared; the
//!   searched fields normalized and joined ([`Search`]); the normalized
//!   title and artist, by which [`Sort::Title`] and [`Sort::Artist`] order;
//!   and the folder of the path, by which [`Search::by_folder`] groups.
//! - `charts_fts(norm_text)`: an FTS5 trigram index of `charts.norm_text`,
//!   kept in the same transactions as `charts` by triggers.
//!
//! A change of [`Search`]'s normalization changes what the stored text
//! means, so it bumps the version too.
//!
//! Everything in the database is derived from the chart files, so a file of
//! any other version is deleted with its `-wal` and `-shm` files and created
//! anew, and the next scan reads every chart; only the version is read from
//! the old file. [`Library::open`] says so in [`Opened::Rebuilt`] for the
//! caller to log, since this crate never logs.
//!
//! The database is in WAL mode. The scan's one thread owns the writer
//! connection; [`Library`] holds the reader connection of the thread that
//! queries it. Both come from the [`Library`], which resolves the file
//! (links followed) and holds an exclusive OS lock on `<file name>.lock`
//! beside it for as long as it or its scan lives: a second [`Library`] of
//! the file, in this process or another, is refused, and one scan runs at a
//! time. Hard links are not supported: another hard-linked name selects
//! another lock. A rebuild happens in [`Library::open`] after the lock is
//! taken and before any connection exists. The lock file is left in place;
//! the database file itself is never locked, since SQLite locks it. Each
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
mod search;

use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::Connection;

pub use bms_parser::Header;
pub use scan::{BATCH, HEADER_FIELD_BYTES, Scan, ScanError, ScanOptions, ScanReport, ThreadStart};
pub use search::{SEPARATOR, Search, Sort};

/// The schema version this build reads and writes.
pub const SCHEMA_VERSION: i64 = 2;

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
    /// The database path cannot be resolved: its folder is missing, or an
    /// entry is there but does not lead to a file, such as a link to a
    /// missing file. Nothing was removed.
    #[error("the library database's path cannot be resolved: {}", path.display())]
    Resolve {
        /// The path given.
        path: PathBuf,
        /// What the file system said.
        #[source]
        source: io::Error,
    },
    /// The lock file beside the database cannot be opened or locked.
    #[error("the library database's lock cannot be taken: {}", path.display())]
    Lock {
        /// The lock file.
        path: PathBuf,
        /// What the file system said.
        #[source]
        source: io::Error,
    },
    /// Another [`Library`] of this file, or its scan, lives in this process
    /// or another.
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

/// The lock beside a database file, held by its [`Library`] and by the lease
/// of its running scan, whichever ends last. The lock ends with the `File`.
#[derive(Debug)]
struct Owner {
    _lock: File,
    scanning: AtomicBool,
}

impl Owner {
    /// Takes the exclusive lock on `<file name>.lock` beside the database,
    /// creating it. The database file itself is never locked: SQLite does
    /// its own locking on it.
    fn take(path: &Path) -> Result<Arc<Self>, LibraryError> {
        let mut name = path.file_name().unwrap_or_default().to_owned();
        name.push(".lock");
        let lock_path = path.with_file_name(name);
        let refused = |source| LibraryError::Lock {
            path: lock_path.clone(),
            source,
        };
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(refused)?;
        match lock.try_lock() {
            Ok(()) => Ok(Arc::new(Self {
                _lock: lock,
                scanning: AtomicBool::new(false),
            })),
            Err(TryLockError::WouldBlock) => Err(LibraryError::AlreadyOpen {
                path: path.to_owned(),
            }),
            Err(TryLockError::Error(e)) => Err(refused(e)),
        }
    }
}

/// The right to run the one scan of a [`Library`], released when the scan's
/// thread ends, however it ends. It keeps the lock beside the file
/// while the scan outlives its library.
#[derive(Debug)]
pub(crate) struct ScanLease(Arc<Owner>);

impl Drop for ScanLease {
    fn drop(&mut self) {
        self.0.scanning.store(false, Ordering::Release);
    }
}

/// The database file `path` resolves to: the file itself, links followed,
/// when it exists, or else, when there is no entry at all, its resolved
/// folder and its name. An entry that exists but does not resolve, such as
/// a link to a missing file, is refused, so nothing deletes or replaces it.
/// The check is made on the path that will be created, so a spelling such
/// as a trailing separator cannot hide the entry there.
fn resolve(path: &Path) -> Result<PathBuf, LibraryError> {
    let unresolved = |source| LibraryError::Resolve {
        path: path.to_owned(),
        source,
    };
    match fs::canonicalize(path) {
        Ok(file) => Ok(file),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let name = path.file_name().ok_or_else(|| {
                unresolved(io::Error::new(io::ErrorKind::InvalidInput, "no file name"))
            })?;
            let dir = match path.parent() {
                Some(dir) if !dir.as_os_str().is_empty() => dir,
                _ => Path::new("."),
            };
            let candidate = fs::canonicalize(dir).map_err(unresolved)?.join(name);
            match fs::symlink_metadata(&candidate) {
                Err(gone) if gone.kind() == io::ErrorKind::NotFound => Ok(candidate),
                // An entry is there, yet the path did not resolve to it.
                Ok(_) => Err(unresolved(e)),
                Err(other) => Err(unresolved(other)),
            }
        }
        Err(e) => Err(unresolved(e)),
    }
}

/// The library database. It resolves its file, links followed, and locks
/// `<file name>.lock` beside the resolved file for as long as it or its
/// scan lives, so a second [`Library`] of the file, in this process or
/// another, by a case alias or through a symbolic link, is refused. Hard
/// links are not supported: a second name of the file by a hard link
/// selects another lock. Every connection to the file comes from it, on the
/// resolved path: its own reader connection, and the writer connection of
/// its scan. A rebuild in [`open`](Self::open) therefore never deletes a
/// file under a live connection.
#[derive(Debug)]
pub struct Library {
    path: PathBuf,
    // Before `owner`, so the connection closes before the lock is released.
    reader: Connection,
    owner: Arc<Owner>,
}

impl Library {
    /// Opens the database at `path`, creating it when it is missing, or
    /// deleting and creating it when it holds another schema version. Both
    /// happen after the lock is taken and before any connection exists.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Resolve`] when the path does not resolve (a link to a
    /// missing file included); [`LibraryError::AlreadyOpen`] while another
    /// [`Library`] of this file, or its scan, lives in any process;
    /// [`LibraryError::Lock`] when the
    /// lock file cannot be opened or locked; [`LibraryError::Database`] when
    /// SQLite refuses the file; and [`LibraryError::Remove`] when a file of
    /// another version cannot be deleted.
    pub fn open(path: &Path) -> Result<(Self, Opened), LibraryError> {
        let resolved = resolve(path)?;
        let path = resolved.as_path();
        let owner = Owner::take(path)?;
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

    /// The charts `search` finds, in its order, at most its limit. The query
    /// runs on the reader connection and reads the last batch a running
    /// scan committed; it takes no lock the scan waits for.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses the query.
    pub fn search(&self, search: &Search<'_>) -> Result<Vec<ChartEntry>, LibraryError> {
        search::run(&self.reader, search).map_err(|source| LibraryError::Database {
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
