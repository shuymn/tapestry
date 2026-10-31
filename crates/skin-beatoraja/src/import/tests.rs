//! Private-hook witnesses: the order of an import's charges, compiles and
//! admissions, and the location pass's walk.

use std::path::Path;

use super::asset::ADMITS;
use super::report::WALKED;
use super::{Config, Event, Imported, NoteKind, TRACE, import};
use crate::error::ImportError;

fn png(root: &Path, name: &str, w: u32, h: u32) {
    let file = std::fs::File::create(root.join(name)).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    let mut out = enc.write_header().unwrap();
    out.write_image_data(&vec![255_u8; usize::try_from(w * h * 4).unwrap()])
        .unwrap();
}

/// Imports `json` as `s.json` beside a 64×64 `a.png`, with this thread's
/// witnesses reset first.
fn run(json: &str) -> (Result<Imported, ImportError>, Vec<Event>, u32) {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("s.json"), json).unwrap();
    png(d.path(), "a.png", 64, 64);
    std::fs::write(d.path().join("bad.png"), b"not a png").unwrap();
    TRACE.with(|t| t.borrow_mut().clear());
    ADMITS.with(|n| n.set(0));
    let (mut def, h) = crate::read_header(d.path(), "s.json").unwrap();
    let r = import(&mut def, &h, &Config::defaults(&h));
    (
        r,
        TRACE.with(|t| t.take()),
        ADMITS.with(std::cell::Cell::get),
    )
}

fn skin(images: &str, destinations: &str) -> String {
    format!(
        r#"{{"type": 0, "w": 1920, "h": 1080,
            "source": [{{"id": "a", "path": "a.png"}}, {{"id": "b", "path": "bad.png"}}],
            "image": [{images}],
            "destination": [{destinations}]}}"#
    )
}

const A: &str = r#"{"id": "i", "src": "a", "w": 64, "h": 64}"#;

#[test]
fn each_compile_follows_its_charges_and_each_admission_its_image_charge() {
    use Event::{Admit, Charge, Compile};
    let json = skin(
        A,
        r#"{"id": "i", "timer": 348, "op": [241], "dst": [{"w": 1, "h": 1}]},
           {"id": "i", "timer": 348, "op": [241], "dst": [{"w": 1, "h": 1}]}"#,
    );
    let (r, trace, admits) = run(&json);
    r.unwrap();
    assert_eq!(
        trace,
        [
            // The element, then the rank's `when` before it compiles,
            // then the visibility, then the image before its file.
            Charge, Charge, Compile, Compile, Charge, Admit,
            // The second: the `when` and the image are shared.
            Charge, Compile,
        ]
    );
    assert_eq!(admits, 1);
}

#[test]
fn a_dropped_element_admits_nothing() {
    let json = skin(
        A,
        r#"{"id": "i", "blend": 4, "dst": [{"w": 1, "h": 1}]},
           {"id": "i", "op": [7777], "dst": [{"w": 1, "h": 1}]},
           {"id": "i", "timer": 2, "dst": [{"w": 1, "h": 1}]},
           {"id": "i", "dst": [{"time": 0, "w": 1, "h": 1}, {"time": 0, "w": 2, "h": 1},
                               {"time": 0, "w": 3, "h": 1}]}"#,
    );
    let (r, trace, admits) = run(&json);
    let imported = r.unwrap();
    assert_eq!(imported.report.usage.elements, 4);
    assert_eq!(admits, 0, "{:?}", imported.report.notes);
    assert!(!trace.contains(&Event::Admit));
}

/// 1,000 references to one malformed PNG, through 500 images each drawn
/// with both filters, admit it once; one admission reads its header once
/// (`skin_model`'s builder witness), so the file's header is read once.
#[test]
fn repeated_bad_references_read_one_header() {
    let images: Vec<String> = (0..500)
        .map(|i| format!(r#"{{"id": "{i}", "src": "b", "w": {}, "h": 8}}"#, 1 + i % 8))
        .collect();
    let dsts: Vec<String> = (0..1_000)
        .map(|i| {
            format!(
                r#"{{"id": "{}", "filter": {}, "dst": [{{"w": 1, "h": 1}}]}}"#,
                i / 2,
                i % 2
            )
        })
        .collect();
    let (r, _, admits) = run(&skin(&images.join(","), &dsts.join(",")));
    let imported = r.unwrap();
    let missing = imported
        .report
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::MissingAsset)
        .count();
    assert_eq!(missing, 1_000);
    assert_eq!(admits, 1);
    assert_eq!(imported.report.usage.file_attempts, 1);
}

#[test]
fn the_location_pass_walks_each_file_once() {
    // A large prefix, then 5,000 destinations naming no object, one per
    // line: 4,096 notes kept, the rest only counted.
    let pad = " ".repeat(1 << 20);
    let mut dsts = String::new();
    for i in 0..5_000 {
        if i > 0 {
            dsts.push_str(",\n");
        }
        dsts.push_str(r#"{"id": "none", "dst": [{"w": 1, "h": 1}]}"#);
    }
    let json =
        format!("{{\"type\": 0, \"w\": 1920, \"h\": 1080, {pad}\n\"destination\": [\n{dsts}]}}");
    WALKED.with(|w| w.set(0));
    let (r, ..) = run(&json);
    let imported = r.unwrap();
    let walked = WALKED.with(std::cell::Cell::get);
    assert!(walked <= json.len() as u64, "{walked} > {}", json.len());
    assert_eq!(imported.report.notes.len(), crate::limits::MAX_NOTES);
    assert_eq!(imported.report.suppressed, 5_000 - 4_096);
    // Destination k sits on line 3 + k, at column 1.
    for (k, n) in imported.report.notes.iter().enumerate() {
        let at = n.at.unwrap();
        assert_eq!((at.line, at.col), (3 + u32::try_from(k).unwrap(), 1));
    }
}
