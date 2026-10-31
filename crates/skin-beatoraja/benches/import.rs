//! CPU benchmarks of importing a JSON skin: `read_header` then `import`.
//!
//! `skin_beatoraja_import_typical` is a generated skin of about 2,000
//! images, 64 numbers, 64 texts and 200 conditions over 16 files.
//! `skin_beatoraja_import_element_cap` is a skin at the element cap, each
//! element its own image.

#[path = "../tests/common/mod.rs"]
mod common;

use std::hint::black_box;
use std::path::Path;

use common::{font, hd, join, png, write};
use criterion::{Criterion, criterion_group, criterion_main};
use skin_beatoraja::{Config, import, read_header};

fn typical(root: &Path) {
    for i in 0..16 {
        png(root, &format!("s{i}.png"), 256, 256);
    }
    png(root, "n.png", 120, 20);
    font(root, "f.ttf");
    let sources = join(
        (0..16)
            .map(|i| format!(r#"{{"id": "s{i}", "path": "s{i}.png"}}"#))
            .chain([r#"{"id": "n", "path": "n.png"}"#.to_owned()]),
    );
    let images = join((0..2_000).map(|i| {
        format!(
            r#"{{"id": "i{i}", "src": "s{}", "x": {}, "y": {}, "w": 16, "h": 16}}"#,
            i % 16,
            (i % 16) * 16,
            (i / 16 % 16) * 16
        )
    }));
    let values = join((0..64).map(|i| {
        format!(
            r#"{{"id": "v{i}", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 6,
                "ref": {}}}"#,
            110 + i % 5
        )
    }));
    let texts = join(
        (0..64).map(|i| format!(r#"{{"id": "t{i}", "font": "f", "constantText": "text {i}"}}"#)),
    );
    let dsts = join(
        (0..2_000)
            .map(|i| {
                let op = if i < 200 { r#""op": [241, 232],"# } else { "" };
                format!(
                    r#"{{"id": "i{i}", "timer": 41, {op} "dst": [
                        {{"time": 0, "x": {}, "y": 100, "w": 32, "h": 32}},
                        {{"time": 500, "x": 400, "a": 0}}]}}"#,
                    i % 1_000
                )
            })
            .chain((0..64).map(|i| {
                format!(r#"{{"id": "v{i}", "timer": 41, "dst": [{{"x": 10, "y": {i}, "w": 12, "h": 20}}]}}"#)
            }))
            .chain((0..64).map(|i| {
                format!(r#"{{"id": "t{i}", "dst": [{{"x": 900, "y": {}, "w": 300, "h": 14}}]}}"#, i * 15)
            })),
    );
    write(
        root,
        "s.json",
        hd(&format!(
            r#""source": [{sources}], "font": [{{"id": "f", "path": "f.ttf"}}],
               "image": [{images}], "value": [{values}], "text": [{texts}],
               "destination": [{dsts}]"#
        ))
        .as_bytes(),
    );
}

fn element_cap(root: &Path) {
    png(root, "a.png", 64, 64);
    let images = join((0..16_384).map(|i| {
        format!(
            r#"{{"id": "{i}", "src": "a", "x": {}, "w": 4, "h": 4}}"#,
            i % 60
        )
    }));
    let dsts =
        join((0..16_384).map(|i| format!(r#"{{"id": "{i}", "dst": [{{"w": 4, "h": 4}}]}}"#)));
    write(
        root,
        "s.json",
        hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}], "image": [{images}],
               "destination": [{dsts}]"#
        ))
        .as_bytes(),
    );
}

fn run(root: &Path) -> usize {
    let (mut def, header) = read_header(root, "s.json").unwrap();
    let imported = import(&mut def, &header, &Config::defaults(&header)).unwrap();
    imported.report.usage.elements as usize
}

fn bench_import(c: &mut Criterion) {
    let d = tempfile::tempdir().unwrap();
    typical(d.path());
    assert_eq!(run(d.path()), 2_128);
    c.bench_function("skin_beatoraja_import_typical", |b| {
        b.iter(|| black_box(run(black_box(d.path()))));
    });
    let d = tempfile::tempdir().unwrap();
    element_cap(d.path());
    assert_eq!(run(d.path()), 16_384);
    c.bench_function("skin_beatoraja_import_element_cap", |b| {
        b.iter(|| black_box(run(black_box(d.path()))));
    });
}

criterion_group!(benches, bench_import);
criterion_main!(benches);
