//! Scanning self-made chart files in a temporary folder into the library
//! database, and reopening it.

use std::fs::{self, File};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use library::{HEADER_FIELD_BYTES, Library, Opened, SCHEMA_VERSION, ScanOptions, ScanReport};
use tempfile::TempDir;

fn chart_text(title: &str) -> String {
    format!("#PLAYER 1\n#TITLE {title}\n#ARTIST Maker\n#BPM 150\n#PLAYLEVEL 7\n#00111:0101\n")
}

fn scan(lib: &Library, roots: &[PathBuf]) -> ScanReport {
    lib.scan(roots, ScanOptions::default())
        .unwrap()
        .finish()
        .unwrap()
}

fn titles(lib: &Library) -> Vec<(PathBuf, String)> {
    lib.charts()
        .unwrap()
        .into_iter()
        .map(|c| (c.path, c.header.title.unwrap_or_default()))
        .collect()
}

struct Fixture {
    dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("library")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let root = dir.path().join("songs");
        fs::create_dir_all(&root).unwrap();
        Self { dir, root }
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("library.db")
    }

    fn open(&self) -> (Library, Opened) {
        Library::open(&self.db()).unwrap()
    }

    fn write(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
}

#[test]
fn scan_reads_headers_of_new_files() {
    let fx = Fixture::new();
    let (title, _, _) = encoding_rs::SHIFT_JIS.encode("曲名テスト");
    let mut sjis = b"#PLAYER 1\n#TITLE ".to_vec();
    sjis.extend_from_slice(&title);
    sjis.extend_from_slice(b"\n#GENRE Test\n");
    let a = fx.write("pack/a.bms", chart_text("Alpha").as_bytes());
    let b = fx.write("pack/deeper/b.BME", &sjis);
    let c = fx.write("c.bml", chart_text("Gamma").as_bytes());
    fx.write("pack/readme.txt", b"#TITLE not a chart");
    fx.write("pack/song.wav", b"RIFF");

    let (lib, opened) = fx.open();
    assert_eq!(opened, Opened::Created);
    let report = scan(&lib, std::slice::from_ref(&fx.root));
    assert_eq!(
        report,
        ScanReport {
            found: 3,
            read: 3,
            ..ScanReport::default()
        }
    );
    let mut expected = vec![
        (a.clone(), "Alpha".to_owned()),
        (b, "曲名テスト".to_owned()),
        (c, "Gamma".to_owned()),
    ];
    expected.sort();
    assert_eq!(titles(&lib), expected);

    let entry = lib
        .charts()
        .unwrap()
        .into_iter()
        .find(|e| e.path == a)
        .unwrap();
    let bytes = fs::read(&a).unwrap();
    let identity = bms_parser::load_bms(&bytes, &fx.root, &bms_parser::LoadOptions::default())
        .unwrap()
        .chart
        .identity;
    assert_eq!(entry.sha256, identity.sha256, "the loader's file hash");
    assert_eq!(entry.md5, identity.md5);
    assert_eq!(entry.size, u64::try_from(bytes.len()).unwrap());
    assert_eq!(entry.header.artist.as_deref(), Some("Maker"));
    assert_eq!(entry.header.bpm.as_deref(), Some("150"));
    assert_eq!(entry.header.playlevel.as_deref(), Some("7"));
    assert_eq!(entry.header.genre, None);
}

#[test]
fn rescan_reads_only_changed_files() {
    let fx = Fixture::new();
    let kept = fx.write("kept.bms", chart_text("Kept").as_bytes());
    let resized = fx.write("resized.bms", chart_text("Old").as_bytes());
    let touched = fx.write("touched.bms", chart_text("Same size").as_bytes());
    let (lib, _) = fx.open();
    assert_eq!(scan(&lib, std::slice::from_ref(&fx.root)).read, 3);

    // A new size, and a new time at the same size.
    fs::write(&resized, chart_text("New and longer")).unwrap();
    fs::write(&touched, chart_text("Edit size")).unwrap();
    let later = SystemTime::now() + Duration::from_secs(60);
    File::options()
        .write(true)
        .open(&touched)
        .unwrap()
        .set_modified(later)
        .unwrap();

    let report = scan(&lib, std::slice::from_ref(&fx.root));
    assert_eq!(
        report,
        ScanReport {
            found: 3,
            unchanged: 1,
            read: 2,
            ..ScanReport::default()
        }
    );
    let mut expected = vec![
        (kept, "Kept".to_owned()),
        (resized, "New and longer".to_owned()),
        (touched, "Edit size".to_owned()),
    ];
    expected.sort();
    assert_eq!(titles(&lib), expected);

    let report = scan(&lib, std::slice::from_ref(&fx.root));
    assert_eq!((report.found, report.unchanged, report.read), (3, 3, 0));
}

#[test]
fn deleted_files_leave_the_index() {
    let fx = Fixture::new();
    let kept = fx.write("a/kept.bms", chart_text("Kept").as_bytes());
    let gone = fx.write("a/gone.bms", chart_text("Gone").as_bytes());
    fx.write("b/folder.bms", chart_text("Folder").as_bytes());
    let (lib, _) = fx.open();
    assert_eq!(scan(&lib, std::slice::from_ref(&fx.root)).read, 3);

    fs::remove_file(&gone).unwrap();
    fs::remove_dir_all(fx.root.join("b")).unwrap();
    let report = scan(&lib, std::slice::from_ref(&fx.root));
    assert_eq!(
        report,
        ScanReport {
            found: 1,
            unchanged: 1,
            removed: 2,
            ..ScanReport::default()
        }
    );
    assert_eq!(titles(&lib), vec![(kept, "Kept".to_owned())]);
}

#[test]
fn removed_root_leaves_the_index_and_missing_root_stays() {
    let fx = Fixture::new();
    let other = fx.dir.path().join("other");
    fs::create_dir_all(&other).unwrap();
    let in_other = other.join("x.bms");
    fs::write(&in_other, chart_text("Other")).unwrap();
    let in_root = fx.write("y.bms", chart_text("Root").as_bytes());
    let (lib, _) = fx.open();
    let roots = [fx.root.clone(), other.clone()];
    assert_eq!(scan(&lib, &roots).read, 2);

    // A configured root that cannot be listed, as an unplugged drive,
    // keeps its rows.
    let away = fx.dir.path().join("away");
    fs::rename(&other, &away).unwrap();
    let report = scan(&lib, &roots);
    assert_eq!((report.found, report.removed), (1, 0));
    assert_eq!(titles(&lib).len(), 2);

    // A root no longer configured loses its rows.
    fs::rename(&away, &other).unwrap();
    let report = scan(&lib, std::slice::from_ref(&fx.root));
    assert_eq!((report.found, report.removed), (1, 1));
    assert_eq!(titles(&lib), vec![(in_root, "Root".to_owned())]);
}

#[test]
fn long_header_values_are_cut_at_a_character_boundary() {
    let fx = Fixture::new();
    // 3 bytes per character, so the cap falls inside one.
    let title = "あ".repeat(4 * HEADER_FIELD_BYTES);
    let artist = "a".repeat(2 * HEADER_FIELD_BYTES);
    let text = format!("#PLAYER 1\n#TITLE {title}\n#ARTIST {artist}\n#GENRE Short\n");
    fx.write("long.bms", text.as_bytes());
    let (lib, _) = fx.open();
    assert_eq!(scan(&lib, std::slice::from_ref(&fx.root)).read, 1);
    let header = lib.charts().unwrap().remove(0).header;
    let kept = HEADER_FIELD_BYTES / 3;
    assert_eq!(header.title, Some("あ".repeat(kept)));
    assert_eq!(header.artist, Some("a".repeat(HEADER_FIELD_BYTES)));
    assert_eq!(header.genre.as_deref(), Some("Short"));
}

#[test]
fn oversized_file_is_skipped() {
    let fx = Fixture::new();
    let cap = bms_parser::Limits::default().max_file_bytes;
    let mut big = chart_text("Big").into_bytes();
    big.resize(cap + 1, b'\n');
    fx.write("big.bms", &big);
    let mut edge = chart_text("Edge").into_bytes();
    edge.resize(cap, b'\n');
    let edge_path = fx.write("edge.bms", &edge);
    let (lib, _) = fx.open();
    let report = scan(&lib, std::slice::from_ref(&fx.root));
    assert_eq!((report.found, report.read, report.skipped), (2, 1, 1));
    assert_eq!(titles(&lib), vec![(edge_path, "Edge".to_owned())]);
}

#[test]
fn other_schema_version_is_rebuilt() {
    let fx = Fixture::new();
    let chart = fx.write("a.bms", chart_text("Alpha").as_bytes());
    let beside = |suffix: &str| {
        let mut name = fx.db().into_os_string();
        name.push(suffix);
        PathBuf::from(name)
    };
    let saved = |suffix: &str| fx.dir.path().join(format!("saved{suffix}"));
    // An old database whose WAL still holds its last transaction: kept
    // beside a new file, that WAL would bring the old tables back.
    {
        let old = rusqlite::Connection::open(fx.db()).unwrap();
        old.pragma_update(None, "journal_mode", "WAL").unwrap();
        old.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        old.execute_batch(
            "CREATE TABLE charts (path TEXT, title TEXT);
             INSERT INTO charts VALUES ('stale.bms', 'Stale');
             PRAGMA user_version = 3;",
        )
        .unwrap();
        for suffix in ["-wal", "-shm"] {
            fs::copy(beside(suffix), saved(suffix)).unwrap();
        }
    }
    for suffix in ["-wal", "-shm"] {
        fs::copy(saved(suffix), beside(suffix)).unwrap();
    }
    let stale_wal = fs::read(beside("-wal")).unwrap();
    assert!(!stale_wal.is_empty());

    let (lib, opened) = fx.open();
    assert_eq!(opened, Opened::Rebuilt { old_version: 3 });
    assert!(
        opened
            .to_string()
            .contains(&format!("schema version 3 is not {SCHEMA_VERSION}"))
    );
    assert_ne!(fs::read(beside("-wal")).unwrap_or_default(), stale_wal);
    assert_eq!(lib.charts().unwrap(), Vec::new());
    let report = scan(&lib, std::slice::from_ref(&fx.root));
    assert_eq!((report.found, report.read), (1, 1), "a full scan");
    assert_eq!(titles(&lib), vec![(chart, "Alpha".to_owned())]);
    drop(lib);

    // The rebuilt file has this build's version and opens as it is.
    let version: i64 = rusqlite::Connection::open(fx.db())
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let (lib, opened) = fx.open();
    assert_eq!(opened, Opened::Current);
    assert_eq!(titles(&lib).len(), 1);
}

/// A library of schema 4 holds titles as the loader then decoded them, a
/// mostly ASCII Shift_JIS title as windows-1252; opening it under this
/// version rebuilds it, so the next scan reads the unchanged chart again.
#[test]
fn schema_4_reads_shift_jis_titles_again() {
    let fx = Fixture::new();
    let text = chart_text("迎撃-NIGHT SKY PATROL-");
    let (bytes, _, unmappable) = encoding_rs::SHIFT_JIS.encode(&text);
    assert!(!unmappable);
    let chart = fx.write("a.bms", &bytes);
    {
        let (lib, _) = fx.open();
        scan(&lib, std::slice::from_ref(&fx.root));
    }
    let (stored, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(&bytes);
    let stored_title = stored
        .lines()
        .find_map(|l| l.strip_prefix("#TITLE "))
        .unwrap()
        .to_owned();
    {
        let old = rusqlite::Connection::open(fx.db()).unwrap();
        old.execute("UPDATE charts SET title = ?1", [&stored_title])
            .unwrap();
        old.pragma_update(None, "user_version", 4).unwrap();
    }

    let (lib, opened) = fx.open();
    assert_eq!(opened, Opened::Rebuilt { old_version: 4 });
    let report = scan(&lib, std::slice::from_ref(&fx.root));
    assert_eq!((report.found, report.read), (1, 1), "a full scan");
    assert_eq!(
        titles(&lib),
        vec![(chart, "迎撃-NIGHT SKY PATROL-".to_owned())]
    );
}

/// A loudness row is found by its whole key, and by nothing less: a change
/// of any part misses. Storing a row of other versions removes every row of
/// the old ones, and a row survives reopening and rescanning.
#[test]
fn loudness_round_trips_by_its_whole_key() {
    use library::{LoudnessKey, LoudnessRow};
    let fx = Fixture::new();
    fx.write("a.bms", chart_text("Alpha").as_bytes());
    let (mut lib, _) = fx.open();
    let key = LoudnessKey {
        resolved_hash: [1; 32],
        sounds: [2; 32],
        mixer_version: 1,
        analysis_version: 1,
        rate: 48_000,
    };
    let row = LoudnessRow {
        integrated: Some(-14.25),
        true_peak: 1.5,
        over_range: false,
    };
    assert_eq!(lib.loudness(&key).unwrap(), None);
    lib.store_loudness(&key, &row).unwrap();
    assert_eq!(lib.loudness(&key).unwrap(), Some(row));
    let others = [
        LoudnessKey {
            resolved_hash: [9; 32],
            ..key
        },
        LoudnessKey {
            sounds: [9; 32],
            ..key
        },
        LoudnessKey {
            mixer_version: 2,
            ..key
        },
        LoudnessKey {
            analysis_version: 2,
            ..key
        },
        LoudnessKey {
            rate: 44_100,
            ..key
        },
    ];
    for other in &others {
        assert_eq!(lib.loudness(other).unwrap(), None, "{other:?}");
    }
    // Silence (no integrated loudness) and an over-range row round-trip;
    // a store replaces the row of its key.
    let silent = LoudnessRow {
        integrated: None,
        true_peak: 0.0,
        over_range: false,
    };
    let over = LoudnessKey {
        sounds: [3; 32],
        ..key
    };
    let loud = LoudnessRow {
        integrated: Some(29.5),
        true_peak: 40.0,
        over_range: true,
    };
    lib.store_loudness(&over, &loud).unwrap();
    lib.store_loudness(&key, &silent).unwrap();
    assert_eq!(lib.loudness(&key).unwrap(), Some(silent));
    assert_eq!(lib.loudness(&over).unwrap(), Some(loud));
    // Kept across a scan and a reopening.
    scan(&lib, std::slice::from_ref(&fx.root));
    drop(lib);
    let (mut lib, opened) = fx.open();
    assert_eq!(opened, Opened::Current);
    assert_eq!(lib.loudness(&key).unwrap(), Some(silent));
    // Another mixer version's row prunes both of version 1.
    let newer = LoudnessKey {
        mixer_version: 2,
        ..key
    };
    lib.store_loudness(&newer, &row).unwrap();
    assert_eq!(lib.loudness(&newer).unwrap(), Some(row));
    assert_eq!(lib.loudness(&key).unwrap(), None);
    assert_eq!(lib.loudness(&over).unwrap(), None);
}

#[test]
fn thread_start_hook_runs_first_on_the_scan_thread() {
    use std::sync::{Arc, Mutex};
    let fx = Fixture::new();
    fx.write("a.bms", chart_text("Alpha").as_bytes());
    let (lib, _) = fx.open();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let options = ScanOptions {
        on_thread_start: Some(Arc::new(move || {
            let name = std::thread::current().name().map(str::to_owned);
            record.lock().unwrap().push(name);
        })),
        ..ScanOptions::default()
    };
    lib.scan(std::slice::from_ref(&fx.root), options)
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), [Some("library-scan".to_owned())]);
}

/// A scan whose thread is stuck (here at its start hook, as on a stalled
/// file system) is stopped within the bound and left to end on its own:
/// until it does, its lock keeps the database refused to another opening,
/// even with the library dropped; once it ends, the database opens again.
#[test]
fn a_stuck_scan_stops_within_its_bound_and_keeps_the_lock() {
    use std::sync::{Arc, Condvar, Mutex};

    let dir = TempDir::new().unwrap();
    let charts = dir.path().join("charts");
    fs::create_dir_all(&charts).unwrap();
    fs::write(charts.join("a.bms"), chart_text("a")).unwrap();
    let db = dir.path().join("library.db");
    let (lib, _) = Library::open(&db).unwrap();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let held = Arc::clone(&gate);
    let options = ScanOptions {
        on_thread_start: Some(Arc::new(move || {
            let (open, changed) = &*held;
            let mut open = open.lock().unwrap();
            while !*open {
                open = changed.wait(open).unwrap();
            }
        })),
        ..ScanOptions::default()
    };
    let scan = lib.scan(&[charts], options).unwrap();
    let started = std::time::Instant::now();
    assert!(!scan.stop_within(Duration::from_millis(100)));
    assert!(started.elapsed() < Duration::from_secs(5));
    drop(lib);
    assert!(matches!(
        Library::open(&db),
        Err(library::LibraryError::AlreadyOpen { .. })
    ));
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match Library::open(&db) {
            Ok(_) => break,
            Err(library::LibraryError::AlreadyOpen { .. }) => {
                assert!(std::time::Instant::now() < deadline, "the lock stayed");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("{e}"),
        }
    }
}

/// A scan whose work is done but whose thread does not exit (a start hook
/// left a thread-local whose destructor blocks) is stopped within the
/// bound: the stop waits for the work, never for the thread.
#[test]
fn a_stop_waits_for_the_work_not_the_thread() {
    use std::sync::{Arc, Condvar, Mutex};

    type Gate = Arc<(Mutex<bool>, Condvar)>;

    /// Blocks its thread's exit until the gate opens.
    struct Blocks(Gate);

    impl Drop for Blocks {
        fn drop(&mut self) {
            let (open, changed) = &*self.0;
            let mut open = open.lock().unwrap();
            while !*open {
                open = changed.wait(open).unwrap();
            }
        }
    }

    thread_local! {
        static HELD: std::cell::RefCell<Option<Blocks>> = const { std::cell::RefCell::new(None) };
    }

    let dir = TempDir::new().unwrap();
    let charts = dir.path().join("charts");
    fs::create_dir_all(&charts).unwrap();
    fs::write(charts.join("a.bms"), chart_text("a")).unwrap();
    let (lib, _) = Library::open(&dir.path().join("library.db")).unwrap();
    let gate: Gate = Arc::default();
    let held = Arc::clone(&gate);
    let options = ScanOptions {
        on_thread_start: Some(Arc::new(move || {
            HELD.with(|h| *h.borrow_mut() = Some(Blocks(Arc::clone(&held))));
        })),
        ..ScanOptions::default()
    };
    let scan = lib.scan(&[charts], options).unwrap();
    assert!(scan.wait_finished(Duration::from_secs(10)));
    let started = std::time::Instant::now();
    assert!(scan.stop_within(Duration::from_millis(100)));
    assert!(started.elapsed() < Duration::from_secs(5));
    // The thread exits once its thread-local can go.
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
}
