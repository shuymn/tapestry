//! CPU benchmarks of the library (design.md "Measurement"): reading the header
//! of generated chart text, a full scan of generated chart files in a
//! temporary folder into a new database, and searching and sorting an index
//! of generated header-only charts.

use std::fmt::Write as _;
use std::fs;
use std::hint::black_box;
use std::path::Path;

use bms_parser::{Limits, read_header};
use criterion::{Criterion, criterion_group, criterion_main};
use library::{Library, ScanOptions, Search, Sort};

const MEASURES: usize = 400;
/// Chart files in the scanned folder.
const FILES: usize = 300;
/// Charts in the searched index, in folders of `PER_FOLDER`.
const INDEXED: usize = 50_000;
const PER_FOLDER: usize = 100;
/// Results a search returns, a screenful and more.
const SHOWN: usize = 200;

/// A header and `MEASURES` measures of dense channel lines, which the header
/// reader skips.
fn chart_text(title: &str) -> String {
    let mut text = format!(
        "#PLAYER 1\n#TITLE {title}\n#ARTIST Bench\n#GENRE Bench\n#BPM 150\n#PLAYLEVEL 12\n\
         #WAV01 click.wav\n"
    );
    let taps = "01".repeat(16);
    for m in 1..=MEASURES {
        for channel in [11, 12, 13, 14, 15, 16, 18, 19] {
            writeln!(text, "#{m:03}{channel}:{taps}").expect("write to a string");
        }
    }
    text
}

fn bench_header(c: &mut Criterion) {
    let text = chart_text("Bench");
    let limits = Limits::default();
    let header = read_header(text.as_bytes(), &limits).expect("the header reads");
    assert_eq!(header.title.as_deref(), Some("Bench"));
    c.bench_function("read_header_dense_chart", |b| {
        b.iter(|| read_header(black_box(text.as_bytes()), &limits).expect("the header reads"));
    });
}

fn full_scan(db: &Path, root: &Path) -> u64 {
    let _ = fs::remove_file(db);
    let (lib, _) = Library::open(db).expect("the database opens");
    lib.scan(&[root.to_owned()], ScanOptions::default())
        .expect("the scan starts")
        .finish()
        .expect("the scan ends")
        .read
}

fn bench_scan(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("temporary folder");
    let root = dir.path().join("songs");
    fs::create_dir_all(&root).expect("create the folder");
    for i in 0..FILES {
        fs::write(
            root.join(format!("{i:04}.bms")),
            chart_text(&format!("Chart {i}")),
        )
        .expect("write a chart");
    }
    let db = dir.path().join("library.db");
    assert_eq!(full_scan(&db, &root), u64::try_from(FILES).expect("fits"));
    let mut group = c.benchmark_group("scan");
    group.sample_size(10);
    group.bench_function("full_scan_300_files", |b| {
        b.iter(|| full_scan(&db, &root));
    });
    group.finish();
}

/// The header of the searched chart `i`: a title of Latin, a number and
/// hiragana, one of 50 artists, and a level of 1 to 12. It is already
/// normalized, so the rows can be written without the crate's normalization.
fn header(i: usize) -> (String, String, String) {
    const KANA: [&str; 5] = ["そんぐ", "めろでぃ", "りずむ", "ひかり", "そら"];
    (
        format!("chart {i} {}", KANA[i % KANA.len()]),
        format!("artist {}", i % 50),
        (i % 12 + 1).to_string(),
    )
}

/// Writes the searched charts after the first straight into the database:
/// writing and scanning 50,000 files takes minutes. The first is scanned,
/// and its stored row must equal what is written for the others.
fn index(db: &Path, root: &Path) {
    let folder = |i: usize| root.join(format!("{:03}", i / PER_FOLDER));
    let file = |i: usize| folder(i).join(format!("{i:05}.bms"));
    let (title, artist, level) = header(0);
    fs::create_dir_all(folder(0)).expect("create the folder");
    fs::write(
        file(0),
        format!("#TITLE {title}\n#ARTIST {artist}\n#GENRE bench\n#PLAYLEVEL {level}\n"),
    )
    .expect("write a chart");
    let (lib, _) = Library::open(db).expect("the database opens");
    lib.scan(&[root.to_owned()], ScanOptions::default())
        .expect("the scan starts")
        .finish()
        .expect("the scan ends");
    drop(lib);

    let mut conn = rusqlite::Connection::open(db).expect("the database opens");
    let tx = conn.transaction().expect("a transaction");
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO charts (root, path, size, mtime_ns, sha256, md5, title, artist,
                     genre, playlevel, norm_text, title_key, artist_key, folder)
                 VALUES (1, ?1, 0, ?2, zeroblob(32), zeroblob(16), ?3, ?4, 'bench', ?5,
                     ?3 || char(31, 31) || ?4 || char(31, 31) || 'bench', ?3, ?4, ?6)",
            )
            .expect("the statement");
        for i in 1..INDEXED {
            let (title, artist, level) = header(i);
            let path = file(i);
            let folder = folder(i);
            let time = i64::try_from(i).expect("fits");
            insert
                .execute(rusqlite::params![
                    path.to_str(),
                    time,
                    title,
                    artist,
                    level,
                    folder.to_str()
                ])
                .expect("insert a chart");
        }
        let scanned: (String, String, String, String) = tx
            .query_row(
                "SELECT norm_text, title_key, artist_key, folder FROM charts WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("the scanned row");
        let written: (String, String, String, String) = tx
            .query_row(
                "SELECT ?1 || char(31, 31) || ?2 || char(31, 31) || 'bench', ?1, ?2, ?3",
                rusqlite::params![title, artist, folder(0).to_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("the written form");
        assert_eq!(scanned, written, "written rows match scanned ones");
    }
    tx.commit().expect("commit");
}

fn bench_search(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("temporary folder");
    let db = dir.path().join("library.db");
    index(&db, &dir.path().join("songs"));
    let (lib, _) = Library::open(&db).expect("the database opens");
    assert_eq!(
        lib.charts().expect("the charts").len(),
        INDEXED,
        "every chart is indexed"
    );
    let run = |text, sort, by_folder| {
        lib.search(&Search {
            text,
            sort,
            by_folder,
            limit: SHOWN,
        })
        .expect("the search runs")
    };
    // Each case finds what it is meant to.
    assert_eq!(run("chart 1234 ", Sort::Title, false).len(), 1);
    assert_eq!(run("メロディ", Sort::Title, false).len(), SHOWN);
    assert_eq!(run("ひ", Sort::Title, false).len(), SHOWN);
    assert_eq!(run("zz", Sort::Title, false).len(), 0);

    let mut group = c.benchmark_group("search_50000");
    let cases = [
        ("phrase_one_row", "chart 1234 "),
        ("phrase_fifth_of_rows", "めろでぃ"),
        ("short_fifth_of_rows", "ひ"),
        ("short_no_row", "zz"),
    ];
    for (name, text) in cases {
        group.bench_function(name, |b| {
            b.iter(|| run(black_box(text), Sort::Title, false));
        });
    }
    let sorts = [
        ("sort_title", Sort::Title, false),
        ("sort_artist", Sort::Artist, false),
        ("sort_level", Sort::Level, false),
        ("sort_modified", Sort::Modified, false),
        ("sort_title_by_folder", Sort::Title, true),
    ];
    for (name, sort, by_folder) in sorts {
        group.bench_function(name, |b| {
            b.iter(|| run("", sort, by_folder));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_header, bench_scan, bench_search);
criterion_main!(benches);
