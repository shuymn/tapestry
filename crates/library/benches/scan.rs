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
const INDEXED: usize = 10_000;
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

/// A header-only chart: a title of Latin, kana and a number, one of 50
/// artists, and a level of 1 to 12.
fn header_text(i: usize) -> String {
    const KANA: [&str; 5] = ["ソング", "メロディ", "リズム", "ひかり", "そら"];
    format!(
        "#PLAYER 1\n#TITLE Chart {i} {}\n#ARTIST Artist {}\n#GENRE Bench\n#PLAYLEVEL {}\n",
        KANA[i % KANA.len()],
        i % 50,
        i % 12 + 1,
    )
}

fn bench_search(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("temporary folder");
    let root = dir.path().join("songs");
    for i in 0..INDEXED {
        let folder = root.join(format!("{:03}", i / PER_FOLDER));
        fs::create_dir_all(&folder).expect("create the folder");
        fs::write(folder.join(format!("{i:05}.bms")), header_text(i)).expect("write a chart");
    }
    let (lib, _) = Library::open(&dir.path().join("library.db")).expect("the database opens");
    let read = lib
        .scan(&[root], ScanOptions::default())
        .expect("the scan starts")
        .finish()
        .expect("the scan ends")
        .read;
    assert_eq!(read, u64::try_from(INDEXED).expect("fits"));
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

    let mut group = c.benchmark_group("search_10000");
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
