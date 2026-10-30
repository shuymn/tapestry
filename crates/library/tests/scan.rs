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
             PRAGMA user_version = 7;",
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
    assert_eq!(opened, Opened::Rebuilt { old_version: 7 });
    assert!(
        opened
            .to_string()
            .contains(&format!("schema version 7 is not {SCHEMA_VERSION}"))
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
    };
    lib.scan(std::slice::from_ref(&fx.root), options)
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), [Some("library-scan".to_owned())]);
}
