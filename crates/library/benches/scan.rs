//! CPU benchmarks of the library (design.md "Measurement"): reading the header
//! of generated chart text, and a full scan of generated chart files in a
//! temporary folder into a new database.

use std::fmt::Write as _;
use std::fs;
use std::hint::black_box;
use std::path::Path;

use bms_parser::{Limits, read_header};
use criterion::{Criterion, criterion_group, criterion_main};
use library::{Library, ScanOptions};

const MEASURES: usize = 400;
/// Chart files in the scanned folder.
const FILES: usize = 300;

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

criterion_group!(benches, bench_header, bench_scan);
criterion_main!(benches);
