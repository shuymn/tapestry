//! Searching and sorting self-made chart files scanned from a temporary
//! folder.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use library::{ChartEntry, Library, Opened, ScanOptions, Search, Sort};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("search")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let root = dir.path().join("songs");
        fs::create_dir_all(&root).unwrap();
        Self { dir, root }
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("library.db")
    }

    /// Writes a chart whose header holds `lines`, each `NAME value`.
    fn chart(&self, relative: &str, lines: &[&str]) -> PathBuf {
        let mut text = String::from("#PLAYER 1\n");
        for line in lines {
            text.push('#');
            text.push_str(line);
            text.push('\n');
        }
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    fn scanned(&self) -> Library {
        let (lib, _) = Library::open(&self.db()).unwrap();
        scan(&lib, std::slice::from_ref(&self.root));
        lib
    }
}

fn scan(lib: &Library, roots: &[PathBuf]) {
    lib.scan(roots, ScanOptions::default())
        .unwrap()
        .finish()
        .unwrap();
}

fn search_limited(
    lib: &Library,
    text: &str,
    sort: Sort,
    by_folder: bool,
    limit: usize,
) -> Vec<ChartEntry> {
    lib.search(&Search {
        text,
        sort,
        by_folder,
        limit,
    })
    .unwrap()
}

fn search(lib: &Library, text: &str, sort: Sort, by_folder: bool) -> Vec<PathBuf> {
    search_limited(lib, text, sort, by_folder, usize::MAX)
        .into_iter()
        .map(|c| c.path)
        .collect()
}

/// The titles `text` finds, sorted.
fn found(lib: &Library, text: &str) -> Vec<String> {
    let mut titles: Vec<String> = search_limited(lib, text, Sort::Title, false, usize::MAX)
        .into_iter()
        .map(|c| c.header.title.unwrap_or_default())
        .collect();
    titles.sort();
    titles
}

fn titles(list: &[&str]) -> Vec<String> {
    let mut titles: Vec<String> = list.iter().map(|&t| t.to_owned()).collect();
    titles.sort();
    titles
}

#[test]
fn search_folds_width_and_kana() {
    let fx = Fixture::new();
    fx.chart("a.bms", &["TITLE ＢＥＡＴ　テスト"]);
    fx.chart("b.bms", &["TITLE ﾃｽﾄ ｶﾞｰﾙ"]);
    fx.chart("c.bms", &["TITLE てすと", "ARTIST Ｍａｋｅｒ"]);
    fx.chart("d.bms", &["TITLE Other"]);
    let lib = fx.scanned();

    let all_test = titles(&["ＢＥＡＴ　テスト", "ﾃｽﾄ ｶﾞｰﾙ", "てすと"]);
    for query in ["テスト", "てすと", "ﾃｽﾄ", "テす"] {
        assert_eq!(found(&lib, query), all_test, "query {query}");
    }
    // No transliteration.
    assert_eq!(found(&lib, "tesuto"), Vec::<String>::new());
    assert_eq!(found(&lib, "beat"), titles(&["ＢＥＡＴ　テスト"]));
    assert_eq!(found(&lib, "Beat テ"), titles(&["ＢＥＡＴ　テスト"]));
    // Half-width voiced kana composes under NFKC.
    assert_eq!(found(&lib, "がーる"), titles(&["ﾃｽﾄ ｶﾞｰﾙ"]));
    assert_eq!(found(&lib, "ガー"), titles(&["ﾃｽﾄ ｶﾞｰﾙ"]));
    // The artist is searched as well.
    assert_eq!(found(&lib, "MAKER"), titles(&["てすと"]));
    assert_eq!(found(&lib, "ma"), titles(&["てすと"]));
}

#[test]
fn search_is_literal_for_punctuation_quotes_and_wildcards() {
    let fx = Fixture::new();
    let all = [
        "a-b",
        r#"say "hi""#,
        "100%",
        "a_b",
        "ＡＢＣあいう",
        "abcアイウ",
        // Decoys that an operator, a wildcard or a dropped quote would find.
        "ab",
        "a b",
        "say hi",
        "1000",
        "axb",
        "aab",
    ];
    for (i, title) in all.iter().enumerate() {
        fx.chart(&format!("{i:02}.bms"), &[&format!("TITLE {title}")]);
    }
    let lib = fx.scanned();
    let cases: [(&str, &[&str]); 9] = [
        ("a-b", &["a-b"]),
        (r#"say "hi""#, &[r#"say "hi""#]),
        ("100%", &["100%"]),
        ("a_b", &["a_b"]),
        ("ＡＢＣあいう", &["ＡＢＣあいう", "abcアイウ"]),
        // Shorter queries go through `LIKE`.
        ("%", &["100%"]),
        ("0%", &["100%"]),
        ("_", &["a_b"]),
        ("\"", &[r#"say "hi""#]),
    ];
    for (query, expected) in cases {
        assert_eq!(found(&lib, query), titles(expected), "query {query}");
    }
    assert_eq!(found(&lib, "").len(), all.len(), "an empty query finds all");
    assert_eq!(found(&lib, "a\u{1F}b"), Vec::<String>::new());
}

#[test]
fn short_query_matches_substring() {
    let fx = Fixture::new();
    fx.chart("a.bms", &["TITLE Alpha", "SUBTITLE x"]);
    fx.chart("b.bms", &["TITLE Beta", "GENRE Lp Rock"]);
    fx.chart("c.bms", &["TITLE ガンマ"]);
    fx.chart("d.bms", &["TITLE Delta", "SUBARTIST ey"]);
    let lib = fx.scanned();
    assert_eq!(found(&lib, "lp"), titles(&["Alpha", "Beta"]));
    assert_eq!(found(&lib, "ンマ"), titles(&["ガンマ"]));
    assert_eq!(found(&lib, "ん"), titles(&["ガンマ"]));
    assert_eq!(found(&lib, "T"), titles(&["Beta", "Delta"]));
    assert_eq!(found(&lib, "ey"), titles(&["Delta"]));
    // No match spans two fields: the title's end and the subtitle.
    assert_eq!(found(&lib, "ax"), Vec::<String>::new());
    assert_eq!(found(&lib, "hax"), Vec::<String>::new());
    assert_eq!(found(&lib, "x"), titles(&["Alpha"]));
}

#[test]
fn sort_by_title_is_normalized_with_untitled_last() {
    let fx = Fixture::new();
    let beta = fx.chart("1.bms", &["TITLE beta"]);
    let alpha_b = fx.chart("3.bms", &["TITLE alpha"]);
    let alpha_a = fx.chart("2.bms", &["TITLE Alpha"]);
    let alpha2 = fx.chart("4.bms", &["TITLE ａｌｐｈａ２"]);
    let untitled = fx.chart("0.bms", &["ARTIST Nobody"]);
    let kana = fx.chart("5.bms", &["TITLE カ"]);
    let hira = fx.chart("6.bms", &["TITLE か"]);
    let lib = fx.scanned();
    assert_eq!(
        search(&lib, "", Sort::Title, false),
        [alpha_a, alpha_b, alpha2, beta, kana, hira, untitled]
    );
}

#[test]
fn sort_by_artist_then_title_with_no_artist_last() {
    let fx = Fixture::new();
    let b = fx.chart("0.bms", &["TITLE One", "ARTIST b"]);
    let a_two = fx.chart("1.bms", &["TITLE Two", "ARTIST Ａ"]);
    let a_one = fx.chart("2.bms", &["TITLE One", "ARTIST a"]);
    let none = fx.chart("3.bms", &["TITLE Zero"]);
    let lib = fx.scanned();
    assert_eq!(
        search(&lib, "", Sort::Artist, false),
        [a_one, a_two, b, none]
    );
}

#[test]
fn sort_by_level_numbers_then_text_then_none() {
    let fx = Fixture::new();
    let twelve = fx.chart("0.bms", &["TITLE a", "PLAYLEVEL 12"]);
    let two = fx.chart("1.bms", &["TITLE b", "PLAYLEVEL 2"]);
    let seven_b = fx.chart("2.bms", &["TITLE b", "PLAYLEVEL 7"]);
    let seven_a = fx.chart("3.bms", &["TITLE a", "PLAYLEVEL 07"]);
    let star = fx.chart("4.bms", &["TITLE a", "PLAYLEVEL ★3"]);
    let question = fx.chart("5.bms", &["TITLE a", "PLAYLEVEL ?"]);
    let none = fx.chart("6.bms", &["TITLE a"]);
    let huge = fx.chart("7.bms", &["TITLE a", "PLAYLEVEL 99999999999999999999999"]);
    let lib = fx.scanned();
    assert_eq!(
        search(&lib, "", Sort::Level, false),
        [two, seven_a, seven_b, twelve, huge, question, star, none]
    );
}

#[test]
fn sort_by_modified_is_newest_first() {
    let fx = Fixture::new();
    let base = SystemTime::now() - Duration::from_secs(3600);
    let at = |relative: &str, secs: u64| {
        let path = fx.chart(relative, &["TITLE t"]);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(base + Duration::from_secs(secs))
            .unwrap();
        path
    };
    let old = at("0.bms", 0);
    let new = at("1.bms", 20);
    let mid_b = at("3.bms", 10);
    let mid_a = at("2.bms", 10);
    let lib = fx.scanned();
    assert_eq!(
        search(&lib, "", Sort::Modified, false),
        [new, mid_a, mid_b, old]
    );
}

#[test]
fn grouping_orders_folders_then_the_sort_key() {
    let fx = Fixture::new();
    let b_a = fx.chart("b/x.bms", &["TITLE A"]);
    let a_c = fx.chart("a/y.bms", &["TITLE C"]);
    let a_b = fx.chart("a/z.bms", &["TITLE B"]);
    let a_sub = fx.chart("a/sub/w.bms", &["TITLE A"]);
    let lib = fx.scanned();
    assert_eq!(
        search(&lib, "", Sort::Title, true),
        [a_b.clone(), a_c.clone(), a_sub.clone(), b_a.clone()]
    );
    assert_eq!(search(&lib, "", Sort::Title, false), [a_sub, b_a, a_b, a_c]);
}

#[test]
fn limit_bounds_the_results() {
    let fx = Fixture::new();
    let paths: Vec<PathBuf> = (0..5)
        .map(|i| fx.chart(&format!("{i}.bms"), &[&format!("TITLE Song {i}")]))
        .collect();
    let lib = fx.scanned();
    let first = |text, limit| -> Vec<PathBuf> {
        search_limited(&lib, text, Sort::Title, false, limit)
            .into_iter()
            .map(|c| c.path)
            .collect()
    };
    for text in ["", "s", "song"] {
        assert_eq!(first(text, 2), paths[..2], "query {text}");
        assert_eq!(first(text, 0), Vec::<PathBuf>::new());
        // The largest limit allocates only for the rows there are.
        assert_eq!(first(text, usize::MAX), paths);
    }
}

#[test]
fn rescan_changes_and_removals_update_the_search() {
    let fx = Fixture::new();
    fx.chart("kept.bms", &["TITLE Kept Song"]);
    let gone = fx.chart("gone.bms", &["TITLE Gone Song"]);
    let changed = fx.chart("changed.bms", &["TITLE Old Title"]);
    let other = fx.dir.path().join("other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("o.bms"), "#TITLE Other Root\n").unwrap();
    let (lib, _) = Library::open(&fx.db()).unwrap();
    scan(&lib, &[fx.root.clone(), other]);
    assert_eq!(found(&lib, "song"), titles(&["Gone Song", "Kept Song"]));
    assert_eq!(found(&lib, "other root"), titles(&["Other Root"]));

    fs::remove_file(&gone).unwrap();
    fs::write(&changed, "#TITLE New Title, longer\n").unwrap();
    scan(&lib, std::slice::from_ref(&fx.root));
    assert_eq!(found(&lib, "song"), titles(&["Kept Song"]));
    assert_eq!(found(&lib, "old title"), Vec::<String>::new());
    assert_eq!(found(&lib, "new title"), titles(&["New Title, longer"]));
    assert_eq!(found(&lib, "other root"), Vec::<String>::new());
    assert_eq!(found(&lib, "ol"), Vec::<String>::new());

    // The index itself holds no entry of a removed or replaced text.
    let raw = rusqlite::Connection::open(fx.db()).unwrap();
    let indexed = |phrase: &str| -> i64 {
        raw.query_row(
            "SELECT count(*) FROM charts_fts WHERE charts_fts MATCH ?1",
            [phrase],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(indexed("\"gone\""), 0);
    assert_eq!(indexed("\"old title\""), 0);
    assert_eq!(indexed("\"other root\""), 0);
    assert_eq!(indexed("\"song\""), 1);
    raw.execute(
        "INSERT INTO charts_fts (charts_fts, rank) VALUES ('integrity-check', 1)",
        [],
    )
    .unwrap();
}

#[test]
fn a_query_reads_committed_rows_without_waiting_for_the_writer() {
    let fx = Fixture::new();
    fx.chart("a.bms", &["TITLE Committed"]);
    let lib = fx.scanned();
    let writer = rusqlite::Connection::open(fx.db()).unwrap();
    writer
        .execute_batch(
            "BEGIN IMMEDIATE;
             INSERT INTO charts (root, path, size, mtime_ns, sha256, md5, title, norm_text,
                 folder)
                 SELECT id, 'pending.bms', 0, 0, zeroblob(32), zeroblob(16), 'Pending', 'pending', ''
                 FROM roots;",
        )
        .unwrap();
    let started = Instant::now();
    assert_eq!(found(&lib, "pending"), Vec::<String>::new());
    assert_eq!(found(&lib, "pe"), Vec::<String>::new());
    assert_eq!(found(&lib, ""), titles(&["Committed"]));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "it did not wait"
    );
    writer.execute_batch("COMMIT").unwrap();
    assert_eq!(found(&lib, "pending"), titles(&["Pending"]));
}

/// Schema 1 as it was created: `charts` without the search columns.
const SCHEMA_1: &str = "
    CREATE TABLE roots (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE);
    CREATE TABLE charts (
        id INTEGER PRIMARY KEY,
        root INTEGER NOT NULL REFERENCES roots(id),
        path TEXT NOT NULL UNIQUE,
        size INTEGER NOT NULL,
        mtime_ns INTEGER NOT NULL,
        sha256 BLOB NOT NULL,
        md5 BLOB NOT NULL,
        title TEXT, subtitle TEXT, artist TEXT, subartist TEXT, genre TEXT,
        playlevel TEXT, difficulty TEXT, bpm TEXT
    );
    CREATE INDEX charts_root ON charts(root);
    PRAGMA user_version = 1;";

#[test]
fn schema_1_is_rebuilt_and_searchable() {
    let fx = Fixture::new();
    fx.chart("a.bms", &["TITLE Alpha Song"]);
    {
        let old = rusqlite::Connection::open(fx.db()).unwrap();
        old.execute_batch(SCHEMA_1).unwrap();
        old.execute(
            "INSERT INTO roots (path) VALUES (?1)",
            [fx.root.to_str().unwrap()],
        )
        .unwrap();
        old.execute(
            "INSERT INTO charts (root, path, size, mtime_ns, sha256, md5, title)
             VALUES (1, 'stale.bms', 0, 0, x'', x'', 'Stale Song')",
            [],
        )
        .unwrap();
    }
    let (lib, opened) = Library::open(&fx.db()).unwrap();
    assert_eq!(opened, Opened::Rebuilt { old_version: 1 });
    assert_eq!(found(&lib, "song"), Vec::<String>::new());
    scan(&lib, std::slice::from_ref(&fx.root));
    assert_eq!(found(&lib, "song"), titles(&["Alpha Song"]));
    assert_eq!(found(&lib, "so"), titles(&["Alpha Song"]));
    let path: &Path = &fx.root.join("a.bms");
    assert_eq!(search(&lib, "alpha", Sort::Title, false), [path]);
}
