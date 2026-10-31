//! The database file: its version, its schema, and the statements the scan and
//! the reader run.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bms_parser::Header;
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};

use crate::search::Keys;
use crate::{ChartEntry, LibraryError, SCHEMA_VERSION};

/// How long a connection waits for another one's lock, such as a checkpoint.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The `user_version` of the database at `path`, or `None` when there is no
/// file. Nothing else is read from it.
pub fn stored_version(path: &Path) -> rusqlite::Result<Option<i64>> {
    match fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        _ => {}
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let version = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    conn.close().map_err(|(_, e)| e)?;
    Ok(Some(version))
}

/// The database file and its `-wal` and `-shm` files.
fn files(path: &Path) -> [PathBuf; 3] {
    let with = |suffix: &str| {
        let mut name = OsString::from(path.as_os_str());
        name.push(suffix);
        PathBuf::from(name)
    };
    [path.to_owned(), with("-wal"), with("-shm")]
}

/// Deletes the database file and its `-wal` and `-shm` files, those that
/// exist.
pub fn remove(path: &Path) -> Result<(), LibraryError> {
    for file in files(path) {
        match fs::remove_file(&file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(LibraryError::Remove {
                    path: file,
                    source: e,
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// A connection in WAL mode, creating the file when it is missing.
pub fn connect(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    let mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "journal_mode {mode}, not WAL"
        )));
    }
    Ok(conn)
}

/// Creates schema [`SCHEMA_VERSION`] in an empty database, version included,
/// in one transaction.
///
/// `charts_fts` is an external-content FTS5 index of `charts.norm_text`
/// kept by triggers: every statement that inserts, updates or deletes a
/// chart row, the scan's upserts and removals and the removal of a root's
/// rows alike, changes the index in its own transaction. The trigram
/// tokenizer is case-sensitive, since `norm_text` is already lowercased and
/// a second case folding of its own would make it disagree with `LIKE`;
/// diacritics are kept, as normalization keeps them.
pub fn create(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "BEGIN;
         CREATE TABLE roots (
             id INTEGER PRIMARY KEY,
             path TEXT NOT NULL UNIQUE
         );
         CREATE TABLE charts (
             id INTEGER PRIMARY KEY,
             root INTEGER NOT NULL REFERENCES roots(id),
             path TEXT NOT NULL UNIQUE,
             size INTEGER NOT NULL,
             mtime_ns INTEGER NOT NULL,
             sha256 BLOB NOT NULL,
             md5 BLOB NOT NULL,
             title TEXT,
             subtitle TEXT,
             artist TEXT,
             subartist TEXT,
             genre TEXT,
             playlevel TEXT,
             difficulty TEXT,
             bpm TEXT,
             norm_text TEXT NOT NULL,
             title_key TEXT,
             artist_key TEXT,
             folder TEXT NOT NULL
         );
         CREATE INDEX charts_root ON charts(root);
         CREATE INDEX charts_title ON charts(title_key IS NULL, title_key, path);
         CREATE VIRTUAL TABLE charts_fts USING fts5(
             norm_text,
             content = 'charts',
             content_rowid = 'id',
             tokenize = 'trigram case_sensitive 1'
         );
         CREATE TRIGGER charts_fts_insert AFTER INSERT ON charts BEGIN
             INSERT INTO charts_fts (rowid, norm_text) VALUES (new.id, new.norm_text);
         END;
         CREATE TRIGGER charts_fts_delete AFTER DELETE ON charts BEGIN
             INSERT INTO charts_fts (charts_fts, rowid, norm_text)
                 VALUES ('delete', old.id, old.norm_text);
         END;
         CREATE TRIGGER charts_fts_update AFTER UPDATE ON charts BEGIN
             INSERT INTO charts_fts (charts_fts, rowid, norm_text)
                 VALUES ('delete', old.id, old.norm_text);
             INSERT INTO charts_fts (rowid, norm_text) VALUES (new.id, new.norm_text);
         END;
         CREATE INDEX charts_sha ON charts(sha256, path);
         CREATE TABLE meta (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             replay_format INTEGER NOT NULL,
             canonical INTEGER NOT NULL,
             evaluator INTEGER NOT NULL,
             loader INTEGER NOT NULL,
             bests INTEGER NOT NULL,
             requested INTEGER NOT NULL,
             completed INTEGER NOT NULL
         );
         CREATE TABLE scores (
             preset_sha256 BLOB NOT NULL,
             ln_mode INTEGER NOT NULL,
             chart_sha256 BLOB NOT NULL,
             plays INTEGER NOT NULL,
             best_ex INTEGER,
             best_bp INTEGER,
             cleared BLOB,
             full_combo INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (preset_sha256, ln_mode, chart_sha256)
         ) WITHOUT ROWID;
         CREATE TABLE counted (
             path TEXT NOT NULL,
             body_sha256 BLOB NOT NULL,
             size INTEGER NOT NULL,
             mtime_ns INTEGER NOT NULL,
             PRIMARY KEY (path, body_sha256)
         ) WITHOUT ROWID;
         CREATE TABLE refused (
             path TEXT NOT NULL PRIMARY KEY,
             size INTEGER NOT NULL,
             mtime_ns INTEGER NOT NULL,
             reason INTEGER NOT NULL,
             waits_for BLOB
         ) WITHOUT ROWID;
         CREATE TABLE loudness (
             resolved_hash BLOB NOT NULL CHECK (length(resolved_hash) = 32),
             sounds BLOB NOT NULL CHECK (length(sounds) = 32),
             mixer_version INTEGER NOT NULL,
             analysis_version INTEGER NOT NULL,
             rate INTEGER NOT NULL,
             integrated REAL,
             true_peak REAL NOT NULL,
             over_range INTEGER NOT NULL,
             PRIMARY KEY (resolved_hash, sounds, mixer_version, analysis_version, rate)
         ) WITHOUT ROWID;
         PRAGMA user_version = {SCHEMA_VERSION};
         COMMIT;"
    ))
}

/// The versions bests are derived under (design.md "Personal Bests"): a
/// difference in any of them makes every stored best stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuple {
    /// The replay format version.
    pub replay_format: i64,
    /// The canonical preset encoding's version.
    pub canonical: i64,
    /// The standard evaluator's version.
    pub evaluator: i64,
    /// The chart loader's version.
    pub loader: i64,
    /// The bests rules' version, [`BESTS_VERSION`](crate::BESTS_VERSION).
    pub bests: i64,
}

impl fmt::Display for Tuple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "replay format {}, canonical encoding {}, evaluator {}, loader {}, bests {}",
            self.replay_format, self.canonical, self.evaluator, self.loader, self.bests
        )
    }
}

impl Tuple {
    /// This build's.
    #[must_use]
    pub fn current() -> Self {
        Self {
            replay_format: i64::from(replay::FORMAT_VERSION),
            canonical: i64::try_from(rules::CANONICAL_VERSION).unwrap_or(i64::MAX),
            evaluator: i64::from(rules::Implementation::STANDARD_VERSION),
            loader: i64::from(bms_parser::LOADER_VERSION),
            bests: crate::BESTS_VERSION,
        }
    }
}

/// The stored tuple, `None` without a `meta` row.
pub fn stored_tuple(conn: &Connection) -> rusqlite::Result<Option<Tuple>> {
    conn.query_row(
        "SELECT replay_format, canonical, evaluator, loader, bests FROM meta WHERE id = 1",
        [],
        |r| {
            Ok(Tuple {
                replay_format: r.get(0)?,
                canonical: r.get(1)?,
                evaluator: r.get(2)?,
                loader: r.get(3)?,
                bests: r.get(4)?,
            })
        },
    )
    .optional()
}

/// Makes the stored bests this build's: when the tuple differs or is
/// missing, clears `scores`, `counted` and `refused`, writes the tuple and
/// requests a catch-up, in one transaction. Returns the tuple found when it
/// was not this build's.
pub fn check_tuple(conn: &mut Connection) -> rusqlite::Result<Option<Option<Tuple>>> {
    let tx = conn.transaction()?;
    let stored = stored_tuple(&tx)?;
    let current = Tuple::current();
    if stored == Some(current) {
        return Ok(None);
    }
    tx.execute_batch("DELETE FROM scores; DELETE FROM counted; DELETE FROM refused;")?;
    tx.execute(
        "INSERT INTO meta (id, replay_format, canonical, evaluator, loader, bests, requested,
             completed)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, 1, 0)
         ON CONFLICT (id) DO UPDATE SET replay_format = excluded.replay_format,
             canonical = excluded.canonical, evaluator = excluded.evaluator,
             loader = excluded.loader, bests = excluded.bests,
             requested = requested + 1",
        params![
            current.replay_format,
            current.canonical,
            current.evaluator,
            current.loader,
            current.bests
        ],
    )?;
    tx.commit()?;
    Ok(Some(stored))
}

/// Requests a catch-up: `requested` rises by one, durably.
pub fn request(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("UPDATE meta SET requested = requested + 1 WHERE id = 1", [])
        .map(|_| ())
}

/// `(requested, completed)`.
pub fn progress(conn: &Connection) -> rusqlite::Result<(i64, i64)> {
    conn.query_row(
        "SELECT requested, completed FROM meta WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

/// A chart file as a parser read it.
#[derive(Debug)]
pub struct Row {
    pub root: i64,
    pub path: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub sha256: [u8; 32],
    pub md5: [u8; 16],
    pub header: Header,
}

/// What a scan starts from.
#[derive(Debug, Default)]
pub struct Prepared {
    /// The roots to walk, with their ids, in the order given.
    pub roots: Vec<(i64, PathBuf)>,
    /// The `(size, mtime_ns)` of every indexed chart under those roots.
    pub known: HashMap<String, (i64, i64)>,
    /// Rows removed because their root is no longer configured.
    pub removed: u64,
}

/// Makes `roots` the table's roots, removing the charts of any other root,
/// and reads what is indexed under them. A root whose path is not Unicode
/// cannot be stored and is left out.
pub fn prepare(conn: &mut Connection, roots: &[PathBuf]) -> rusqlite::Result<Prepared> {
    let tx = conn.transaction()?;
    let mut prepared = Prepared::default();
    for root in roots {
        let Some(text) = root.to_str() else {
            continue;
        };
        tx.execute("INSERT OR IGNORE INTO roots (path) VALUES (?1)", [text])?;
        let id: i64 = tx.query_row("SELECT id FROM roots WHERE path = ?1", [text], |r| r.get(0))?;
        if prepared.roots.iter().all(|&(other, _)| other != id) {
            prepared.roots.push((id, root.clone()));
        }
    }
    let stale: Vec<i64> = {
        let mut stmt = tx.prepare("SELECT id FROM roots")?;
        let ids = stmt.query_map([], |r| r.get(0))?;
        ids.collect::<rusqlite::Result<Vec<i64>>>()?
            .into_iter()
            .filter(|id| prepared.roots.iter().all(|&(kept, _)| kept != *id))
            .collect()
    };
    for id in stale {
        let removed = tx.execute("DELETE FROM charts WHERE root = ?1", [id])?;
        prepared.removed += u64::try_from(removed).unwrap_or(u64::MAX);
        tx.execute("DELETE FROM roots WHERE id = ?1", [id])?;
    }
    {
        let mut stmt = tx.prepare("SELECT path, size, mtime_ns FROM charts")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
        for row in rows {
            let (path, stamp) = row?;
            prepared.known.insert(path, stamp);
        }
    }
    tx.commit()?;
    Ok(prepared)
}

/// Writes `upserts` and deletes the charts at `removals` in one
/// transaction. Returns how many rows the deletions removed.
pub fn commit(
    conn: &mut Connection,
    upserts: &[Row],
    removals: &[String],
) -> rusqlite::Result<u64> {
    let tx = conn.transaction()?;
    let mut removed = 0_u64;
    {
        let mut upsert = tx.prepare(
            "INSERT INTO charts (root, path, size, mtime_ns, sha256, md5, title, subtitle,
                 artist, subartist, genre, playlevel, difficulty, bpm, norm_text, title_key,
                 artist_key, folder)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18)
             ON CONFLICT (path) DO UPDATE SET root = excluded.root, size = excluded.size,
                 mtime_ns = excluded.mtime_ns, sha256 = excluded.sha256, md5 = excluded.md5,
                 title = excluded.title, subtitle = excluded.subtitle,
                 artist = excluded.artist, subartist = excluded.subartist,
                 genre = excluded.genre, playlevel = excluded.playlevel,
                 difficulty = excluded.difficulty, bpm = excluded.bpm,
                 norm_text = excluded.norm_text, title_key = excluded.title_key,
                 artist_key = excluded.artist_key",
        )?;
        for row in upserts {
            let h = &row.header;
            let keys = Keys::of(h);
            upsert.execute(params![
                row.root,
                row.path,
                row.size,
                row.mtime_ns,
                row.sha256,
                row.md5,
                h.title,
                h.subtitle,
                h.artist,
                h.subartist,
                h.genre,
                h.playlevel,
                h.difficulty,
                h.bpm,
                keys.text,
                keys.title,
                keys.artist,
                Path::new(&row.path)
                    .parent()
                    .and_then(Path::to_str)
                    .unwrap_or_default(),
            ])?;
        }
        let mut delete = tx.prepare("DELETE FROM charts WHERE path = ?1")?;
        for path in removals {
            removed += u64::try_from(delete.execute([path])?).unwrap_or(u64::MAX);
        }
    }
    tx.commit()?;
    Ok(removed)
}

/// The columns [`entry`] reads, of `charts` as `c`.
pub const ENTRY_COLUMNS: &str = "c.path, c.size, c.mtime_ns, c.sha256, c.md5, c.title, \
     c.subtitle, c.artist, c.subartist, c.genre, c.playlevel, c.difficulty, c.bpm";

/// A chart from a row of [`ENTRY_COLUMNS`].
pub fn entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChartEntry> {
    let size: i64 = r.get(1)?;
    Ok(ChartEntry {
        path: PathBuf::from(r.get::<_, String>(0)?),
        size: u64::try_from(size).unwrap_or(0),
        mtime_ns: r.get(2)?,
        sha256: r.get(3)?,
        md5: r.get(4)?,
        header: Header {
            title: r.get(5)?,
            subtitle: r.get(6)?,
            artist: r.get(7)?,
            subartist: r.get(8)?,
            genre: r.get(9)?,
            playlevel: r.get(10)?,
            difficulty: r.get(11)?,
            bpm: r.get(12)?,
        },
    })
}

/// Every indexed chart, by path.
pub fn charts(conn: &Connection) -> rusqlite::Result<Vec<ChartEntry>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ENTRY_COLUMNS} FROM charts c ORDER BY c.path"
    ))?;
    let rows = stmt.query_map([], entry)?;
    rows.collect()
}
