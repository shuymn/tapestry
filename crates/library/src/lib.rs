//! The library database (design.md "Loading and Song Selection"): one row per
//! chart file under the configured roots, with its size, modification time,
//! file hashes and descriptive header, kept current by a background [`Scan`];
//! and the personal bests derived from the saved records (design.md
//! "Personal Bests"), counted live through a [`BestWriter`] and caught up by
//! the bests phase of a scan pass; and the cached loudness measurements of
//! the charts played (design.md "Algorithms").
//!
//! # Schema version 10
//!
//! [`SCHEMA_VERSION`] is kept in `PRAGMA user_version`.
//!
//! - `roots(id, path UNIQUE)`: the roots of the latest scan.
//! - `charts(id, root, path UNIQUE, size, mtime_ns, sha256, md5, title,
//!   subtitle, artist, subartist, genre, playlevel, difficulty, bpm,
//!   norm_text, title_key, artist_key, folder, mode, random, bga, loader,
//!   taps, longs, mines, length_us, tempo_start, tempo_min, tempo_max,
//!   rank_kind, rank_value, stagefile, banner, image_names, key_taps,
//!   key_longs, scratch_taps, scratch_longs, density_peak, density_end,
//!   density_end_x100, density_average, density_average_x100, gauge_total,
//!   main_bpm)`: the
//!   header values as [`bms_parser::read_scanned`] reads them, `NULL` when
//!   undeclared; the searched fields normalized and joined ([`Search`]); the
//!   normalized title and artist, by which [`Sort::Title`] and [`Sort::Artist`]
//!   order; the folder of the path, by which [`Search::by_folder`] groups; the
//!   mode ([`ChartMode`]), of which only 7-key is ever listed; the flag of
//!   `#RANDOM`; the flag of a top-level BGA image definition
//!   ([`bms_parser::Listing::bga`]); the loader version that wrote the row, and a row of another is
//!   read again; and the seed-0 branch's summary ([`ChartSummary`]), `NULL`
//!   when the loader refuses it, its tempos 16 big-endian bytes; and the
//!   rank the beatoraja-style header policy reads from that branch
//!   ([`ChartEntry::rank`]): `rank_kind` `NULL` when the branch is refused,
//!   0 with no rank header, 1 a `#RANK` as written in `rank_value`, 2 a
//!   `#DEFEXRANK` in thousandths in `rank_value`; and the image names that
//!   branch declares ([`ChartEntry::images`]): `stagefile` and `banner`,
//!   `NULL` when undeclared or overlong, and `image_names` `NULL` when the
//!   branch is refused, else a bitmask of the overlong ones (1 the stage
//!   file, 2 the banner), never shortened; and the values a song list
//!   previews of that branch ([`ChartInfoView`]), all `NULL` when the
//!   branch is refused, `main_bpm` also when the MAIN tempo was
//!   unavailable, and 0 when no note weighs one.
//!   Indexed by `root`; by `(mode, title_key IS NULL, title_key, path)`, the
//!   [`Sort::Title`] order, so an unfiltered list reads only the rows it
//!   returns; and by `(mode, folder)`, by which [`Library::level`] finds a
//!   level's folders.
//! - `profiles(chart_id, sha256, loader, data)`: the stored profile
//!   ([`StoredProfile`], at most [`MAX_PROFILE_BYTES`]) of a chart row's
//!   seed-0 branch, stamped with the file's SHA-256 and the loader version
//!   it was read with; written or deleted in its chart row's upsert, and
//!   deleted with its chart row.
//! - `charts_fts(norm_text)`: an FTS5 trigram index of `charts.norm_text`,
//!   kept in the same transactions as `charts` by triggers.
//! - `charts_sha(sha256, path)`: the index by which a record finds its
//!   chart.
//! - `meta(id = 1, replay_format, canonical, evaluator, loader, bests,
//!   requested, completed)`: the interpretation the bests were derived under
//!   ([`Tuple`]), and the catch-up counters.
//! - `scores(preset_sha256, ln_mode, chart_sha256, plays, clears, best_ex,
//!   best_bp, best_combo, cleared, full_combo, play_sha256, play)`: one
//!   [`Score`] per [`Key`], the best play's [`Breakdown`] in `play`
//!   ([`BREAKDOWN_BYTES`]). Its layout belongs to [`BESTS_VERSION`], not to
//!   the schema version: opening a database of another bests tuple creates
//!   the table again, unread, so a change of its columns scans no chart.
//! - `counted(path, body_sha256, size, mtime_ns)`: each play counted
//!   ([`PlayId`]).
//! - `refused(path, size, mtime_ns, reason, waits_for)`: records refused for
//!   a terminal reason, or waiting for the chart of hash `waits_for`.
//! - `loudness(resolved_hash, sounds, mixer_version, analysis_version, rate,
//!   integrated, true_peak, over_range)`: one loudness measurement
//!   ([`LoudnessRow`]) per [`LoudnessKey`], unlinked from `charts`.
//!
//! A change of [`Search`]'s normalization, or of how
//! [`bms_parser::read_header`] decodes text, changes what the stored text
//! means, so it bumps the version too: a chart whose size and modification
//! time are unchanged is not read again otherwise.
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
//! stored header value is cut to [`HEADER_FIELD_BYTES`] and each stored
//! profile is at most [`MAX_PROFILE_BYTES`], so a batch has a fixed bound
//! in bytes.

#![forbid(unsafe_code)]
#![deny(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

// The bests tests' fixture, shared with the integration tests, names the
// crate as they do.
#[cfg(test)]
extern crate self as library;

mod bests;
mod db;
mod loudness;
mod preview;
mod scan;
mod search;
mod stored_profile;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::Connection;

pub use bests::{
    BESTS_VERSION, BREAKDOWN_BYTES, Best, Breakdown, Contribution, Ineligible, Ingested, Key,
    LN_MODE, PlayId, Score, Verified, counts_for_best, ineligible,
};
pub use bms_model::ChartSummary;
pub use bms_parser::{ChartMode, Header};
pub use db::Tuple;
pub use loudness::{LoudnessKey, LoudnessRow};
pub use preview::{ChartInfoView, GaugeTotal};
pub use scan::{
    BATCH, BEST_BATCH, BestsReport, HEADER_FIELD_BYTES, PassGuard, PassStart, REBUILD_MEMORY,
    Refusal, Refusals, Scan, ScanError, ScanOptions, ScanReport, ThreadStart, stamp,
};
pub use search::{FolderKey, FolderRow, Level, SEPARATOR, Search, Sort};
pub use stored_profile::{MAX_PROFILE_BYTES, MAX_ROWS, StoredProfile};

/// The schema version this build reads and writes.
pub const SCHEMA_VERSION: i64 = 10;

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
    /// Whether it declares `#RANDOM` or `#SETRANDOM`, in any branch.
    pub random: bool,
    /// Whether it defines a BGA image at the top level
    /// ([`bms_parser::Listing::bga`]).
    pub bga: bool,
    /// Its seed-0 branch's summary; `None` when the loader refuses that
    /// branch.
    pub summary: Option<ChartSummary>,
    /// Its rank as the beatoraja-style header policy reads its seed-0
    /// branch's declarations ([`rules::HeaderPolicy::DefExRankFirst`]):
    /// `Some(None)` with no rank header (read as `#RANK 2`), `None` when
    /// the loader refuses that branch.
    pub rank: Option<Option<rules::JudgeRank>>,
    /// The image names its seed-0 branch declares; `None` when the loader
    /// refuses that branch.
    pub images: Option<ChartNames>,
    /// What a song list previews of its seed-0 branch's notes, densities,
    /// gauge total and MAIN tempo; `None` when the loader refuses that
    /// branch.
    pub info: Option<ChartInfoView>,
}

/// The longest image name the library keeps, in bytes: the chart-image
/// reader's own name limit (`skin_model::image::MAX_PATH_BYTES`), so a
/// stored name is one the reader would resolve and a longer one is never
/// shortened into another file's name (design.md "Loading Limits").
pub const IMAGE_NAME_BYTES: usize = 1_024;

/// One image name of a chart, as the library keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Name {
    /// Undeclared, or declared empty.
    None,
    /// Declared, as written (trimmed), at most [`IMAGE_NAME_BYTES`] long.
    Named(String),
    /// Declared longer than [`IMAGE_NAME_BYTES`]: kept as a fact, never
    /// shortened, so it resolves to nothing.
    Overlong,
}

impl Name {
    /// The name a label declares.
    #[must_use]
    pub fn of(label: &str) -> Self {
        let name = label.trim();
        if name.is_empty() {
            Self::None
        } else if name.len() > IMAGE_NAME_BYTES {
            Self::Overlong
        } else {
            Self::Named(name.to_owned())
        }
    }
}

/// The image names a chart's seed-0 branch declares (`#STAGEFILE`,
/// `#BANNER`), which selection reads its images by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartNames {
    pub stagefile: Name,
    pub banner: Name,
}

impl ChartNames {
    /// The names of `labels`.
    #[must_use]
    pub fn of(labels: &bms_parser::Labels) -> Self {
        Self {
            stagefile: Name::of(&labels.stagefile),
            banner: Name::of(&labels.banner),
        }
    }
}

/// The lock beside a database file, held by its [`Library`] and by the lease
/// of its running scan, whichever ends last. The lock ends with the `File`.
#[derive(Debug)]
struct Owner {
    _lock: File,
    scanning: AtomicBool,
    /// A catch-up was requested and its write failed: the next bests phase
    /// writes it, and forgets it only once the write succeeds.
    catch_up: AtomicBool,
}

impl Owner {
    /// Requests a catch-up through `conn`, durably, or else holds it here.
    fn request(&self, conn: &Connection) -> rusqlite::Result<()> {
        db::request(conn).inspect_err(|_| self.catch_up.store(true, Ordering::Release))
    }
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
                catch_up: AtomicBool::new(false),
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
    bests: BestsState,
}

/// What opening found of the stored bests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BestsState {
    /// They were derived under this build's interpretation.
    Current,
    /// They were derived under another, or the file had none: they were
    /// cleared before anything could show them, and a catch-up rebuilds
    /// them from the records.
    Invalidated {
        /// The interpretation found, `None` when there was none.
        old: Option<Tuple>,
    },
}

impl fmt::Display for BestsState {
    /// The line to log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Current => f.write_str("library: personal bests are current"),
            Self::Invalidated { old: None } => {
                f.write_str("library: personal bests are rebuilt from the records")
            }
            Self::Invalidated { old: Some(old) } => write!(
                f,
                "library: personal bests were derived under {old}, not {}; \
                 they were cleared and are rebuilt from the records",
                Tuple::current()
            ),
        }
    }
}

/// The connection that counts live plays into the bests, beside the reader
/// and the scan's writer (design.md "Personal Bests"). It keeps the lock
/// beside the database while it lives.
#[derive(Debug)]
pub struct BestWriter {
    path: PathBuf,
    // Before `owner`, so the connection closes before the lock is released.
    conn: Connection,
    owner: Arc<Owner>,
}

impl BestWriter {
    fn database(&self, source: rusqlite::Error) -> LibraryError {
        LibraryError::Database {
            path: self.path.clone(),
            source,
        }
    }

    /// Counts a live play, in one transaction: the key's score before and
    /// after, or that the play was counted before.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses; nothing was written.
    pub fn ingest(&mut self, c: &Contribution) -> Result<Ingested, LibraryError> {
        let run = |conn: &mut Connection| {
            let tx = conn.transaction()?;
            let ingested = bests::ingest(&tx, c)?;
            tx.commit()?;
            Ok(ingested)
        };
        run(&mut self.conn).map_err(|e| self.database(e))
    }

    /// The score stored under `key`, if a row exists: one indexed read, for
    /// a result view's best before the play (design.md "Personal Bests",
    /// skin projections).
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses the query.
    pub fn score(&self, key: &Key) -> Result<Option<Score>, LibraryError> {
        bests::score(&self.conn, key).map_err(|e| self.database(e))
    }

    /// Requests a catch-up ([`Library::request_bests_pass`]).
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when the request cannot be written; it is
    /// then held in memory for the next bests phase.
    pub fn request_pass(&self) -> Result<(), LibraryError> {
        self.owner.request(&self.conn).map_err(|e| self.database(e))
    }
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
        let mut reader = db::connect(path).map_err(database)?;
        if opened != Opened::Current {
            db::create(&reader).map_err(database)?;
        }
        let bests = match db::check_tuple(&mut reader).map_err(database)? {
            None => BestsState::Current,
            Some(old) => BestsState::Invalidated { old },
        };
        Ok((
            Self {
                path: path.to_owned(),
                reader,
                owner,
                bests,
            },
            opened,
        ))
    }

    fn database(&self, source: rusqlite::Error) -> LibraryError {
        LibraryError::Database {
            path: self.path.clone(),
            source,
        }
    }

    /// What opening found of the stored bests, for the log.
    #[must_use]
    pub const fn bests_state(&self) -> BestsState {
        self.bests
    }

    /// Requests a catch-up of the bests: the next scan pass counts every
    /// record not yet counted (design.md "Personal Bests"). The player
    /// requests one at each launch, so plays saved by a run without a
    /// library are counted too.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when the request cannot be written; it is
    /// then held in memory for the next bests phase of this library.
    pub fn request_bests_pass(&self) -> Result<(), LibraryError> {
        self.owner
            .request(&self.reader)
            .map_err(|e| self.database(e))
    }

    /// Whether a catch-up is pending: requested and not yet completed, or
    /// held in memory after its write failed.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses the query.
    pub fn bests_pending(&self) -> Result<bool, LibraryError> {
        let (requested, completed) = db::progress(&self.reader).map_err(|e| self.database(e))?;
        Ok(requested > completed || self.owner.catch_up.load(Ordering::Acquire))
    }

    /// Every chart's score under a preset (by its SHA-256) and LN mode, by
    /// chart SHA-256: one query over the key's prefix.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses the query.
    pub fn bests(
        &self,
        preset_sha256: &[u8; 32],
        ln_mode: i64,
    ) -> Result<HashMap<[u8; 32], Score>, LibraryError> {
        bests::scores(&self.reader, preset_sha256, ln_mode).map_err(|e| self.database(e))
    }

    /// The loudness measured under `key`, if one is stored.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses the query.
    pub fn loudness(&self, key: &LoudnessKey) -> Result<Option<LoudnessRow>, LibraryError> {
        loudness::get(&self.reader, key).map_err(|e| self.database(e))
    }

    /// Stores the loudness measured under `key`, in one transaction that
    /// also removes the rows of other mixer and analysis versions. Call it
    /// while no scan pass may be committing: once its pause is
    /// acknowledged, or with none running.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses; nothing was written.
    pub fn store_loudness(
        &mut self,
        key: &LoudnessKey,
        row: &LoudnessRow,
    ) -> Result<(), LibraryError> {
        loudness::put(&mut self.reader, key, row).map_err(|source| LibraryError::Database {
            path: self.path.clone(),
            source,
        })
    }

    /// Opens the connection that counts live plays.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when the connection cannot be opened.
    pub fn best_writer(&self) -> Result<BestWriter, LibraryError> {
        Ok(BestWriter {
            path: self.path.clone(),
            conn: db::connect(&self.path).map_err(|e| self.database(e))?,
            owner: Arc::clone(&self.owner),
        })
    }

    /// The stored profile of `entry`'s chart, for the song list's graphs:
    /// `None` when there is none, when it is not of the file `entry` was
    /// read from (its SHA-256) by this build's loader, or when it does not
    /// decode within the bounds ([`StoredProfile`]).
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses the query.
    pub fn profile(&self, entry: &ChartEntry) -> Result<Option<StoredProfile>, LibraryError> {
        let Some(path) = entry.path.to_str() else {
            return Ok(None);
        };
        db::profile(&self.reader, path, &entry.sha256).map_err(|e| self.database(e))
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

    /// One folder row per stored root that has listed charts, in path order:
    /// what selection lists with several roots, and the keys of the roots'
    /// levels ([`level`](Self::level)).
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses the query.
    pub fn top(&self) -> Result<Vec<FolderRow>, LibraryError> {
        search::top(&self.reader).map_err(|e| self.database(e))
    }

    /// A folder's level (design.md "Play Lifecycle", Selection): its folder
    /// rows by name, then the chart rows, its own charts and its song
    /// folders', in `sort` order, at most `limit` rows together. One read
    /// transaction, so a scan's commit cannot fall between them.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses the query.
    pub fn level(&self, key: &FolderKey, sort: Sort, limit: usize) -> Result<Level, LibraryError> {
        search::level(&self.reader, key, sort, limit, || {}).map_err(|e| self.database(e))
    }

    /// How many indexed charts are of other modes and so listed nowhere, for
    /// the log.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Database`] when SQLite refuses the query.
    pub fn unlisted(&self) -> Result<u64, LibraryError> {
        search::unlisted(&self.reader).map_err(|e| self.database(e))
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
        let roots = roots.iter().map(|root| one_spelling(root)).collect();
        scan::start(writer, roots, options, lease).map_err(LibraryError::Spawn)
    }

    /// The reader connection, for tests that change the database under a
    /// scan.
    #[cfg(test)]
    pub(crate) const fn reader(&self) -> &Connection {
        &self.reader
    }
}

/// The one spelling of a configured root, from which every stored path derives
/// (design.md "Loading and Song Selection").
fn one_spelling(root: &Path) -> PathBuf {
    let mut spelled = PathBuf::new();
    for component in root.components() {
        match component {
            Component::Prefix(prefix) => {
                if let Prefix::UNC(server, share) = prefix.kind() {
                    // The prefix keeps the separators it was written with, and
                    // a share root always holds its charts under its own
                    // separator.
                    let mut unc = OsString::from(r"\\");
                    unc.push(server);
                    unc.push(r"\");
                    unc.push(share);
                    unc.push(r"\");
                    spelled.push(unc);
                } else {
                    spelled.push(prefix.as_os_str());
                }
            }
            other => spelled.push(other),
        }
    }
    spelled
}

/// Only Windows has these spellings.
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// The spellings of a drive and a share, each to its one spelling.
    #[test]
    fn a_root_has_one_spelling() {
        let table = [
            (r"D:\Music", r"D:\Music"),
            (r"D:\Music\", r"D:\Music"),
            ("D:/Music", r"D:\Music"),
            (r"D:/Music\", r"D:\Music"),
            (r"D:\a\\b\.\c", r"D:\a\b\c"),
            (r"D:\", r"D:\"),
            ("D:/", r"D:\"),
            (r"\\server\share\dir", r"\\server\share\dir"),
            (r"\\server\share\dir\", r"\\server\share\dir"),
            ("//server/share/dir", r"\\server\share\dir"),
            ("//server/share/dir/", r"\\server\share\dir"),
            (r"\\server/share\dir", r"\\server\share\dir"),
            (r"\\server\share", r"\\server\share\"),
            (r"\\server\share\", r"\\server\share\"),
            ("//server/share", r"\\server\share\"),
            ("//server/share/", r"\\server\share\"),
            // A verbatim prefix takes its text literally.
            (r"\\?\D:\Music", r"\\?\D:\Music"),
            (r"\\?\UNC\server\share\dir", r"\\?\UNC\server\share\dir"),
        ];
        for (spelled, stored) in table {
            assert_eq!(
                one_spelling(Path::new(spelled)).to_str(),
                Some(stored),
                "{spelled}"
            );
        }
    }

    /// A chart directly in a share root has the root's own text as its folder,
    /// which is what a level and a count read.
    #[test]
    fn a_share_roots_spelling_is_its_charts_folder() {
        let root = one_spelling(Path::new("//server/share"));
        let chart = root.join("a.bms");
        assert_eq!(
            chart.parent().and_then(Path::to_str),
            root.to_str(),
            "{chart:?}"
        );
    }
}
