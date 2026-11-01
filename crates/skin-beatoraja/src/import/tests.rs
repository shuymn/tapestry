//! Private-hook witnesses: the order of an import's charges, compiles and
//! admissions, and the location pass's walk.

use std::path::Path;

use super::asset::ADMITS;
use super::report::WALKED;
use super::{Config, Event, Imported, NoteKind, Severity, TRACE, import};
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
        r#"{"id": "i", "timer": 348, "op": [232], "dst": [{"w": 1, "h": 1}]},
           {"id": "i", "timer": 348, "op": [232], "dst": [{"w": 1, "h": 1}]}"#,
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

/// Lanes of image `p` (x 20..450, y 200, 600 tall) with `objects` and the
/// destinations `dsts` after the notes; image `bad` reads `bad.png`.
fn lanes(objects: &str, dsts: &str) -> String {
    let eight = format!("[{}]", [r#""p""#; 8].join(", "));
    let mut rects: Vec<String> = (0..7)
        .map(|i| format!(r#"{{"x": {}, "y": 200, "w": 50, "h": 600}}"#, 100 + 50 * i))
        .collect();
    rects.push(r#"{"x": 20, "y": 200, "w": 80, "h": 600}"#.to_owned());
    let lists = ["note", "lnstart", "lnend", "lnbody", "lnactive", "mine"]
        .into_iter()
        .chain([
            "hcnstart",
            "hcnend",
            "hcnbody",
            "hcnactive",
            "hcndamage",
            "hcnreactive",
        ])
        .map(|f| format!(r#""{f}": {eight}"#))
        .collect::<Vec<_>>()
        .join(", ");
    let sep = if objects.is_empty() { "" } else { ", " };
    format!(
        r#"{{"type": 0, "w": 1920, "h": 1080,
            "source": [{{"id": "a", "path": "a.png"}}, {{"id": "b", "path": "bad.png"}}],
            "image": [{{"id": "p", "src": "a", "w": 8, "h": 20}},
                      {{"id": "cv", "src": "a", "w": 16, "h": 16}},
                      {{"id": "bad", "src": "b", "w": 8, "h": 8}}],
            "note": {{"id": "notes", {lists}, "dst": [{}]}}{sep}{objects},
            "destination": [{{"id": "notes", "dst": [{{"w": 0, "h": 0}}]}}{dsts}]}}"#,
        rects.join(", ")
    )
}

/// A LIFT object of source `src` and its destination.
fn lift(id: &str, src: &str) -> (String, String) {
    (
        format!(r#"{{"id": "{id}", "src": "{src}", "w": 8, "h": 8, "disapearLine": 200}}"#),
        format!(r#", {{"id": "{id}", "dst": [{{"x": 20, "y": 0, "w": 430, "h": 200}}]}}"#),
    )
}

#[test]
fn refused_and_dropped_covers_admit_nothing() {
    let (r, _, base) = run(&lanes("", ""));
    r.unwrap();
    assert_eq!(base, 1);
    // HIDDEN+ of the malformed file outside the subset: nothing read.
    let hidden = lanes(
        r#""hiddenCover": [{"id": "hid", "src": "b", "w": 8, "h": 8}]"#,
        r#", {"id": "hid", "timer": 41, "dst": [{"x": 20, "y": 200, "w": 430, "h": 600}]}"#,
    );
    let (r, _, admits) = run(&hidden);
    let dropped = r.unwrap().report.notes.iter().any(|n| {
        n.severity == Severity::Dropped && n.kind == NoteKind::UnsupportedField { field: "dst" }
    });
    assert!(dropped);
    assert_eq!(admits, base);
    // A second LIFT of the malformed file.
    let (o1, d1) = lift("l1", "a");
    let (o2, d2) = lift("l2", "b");
    let (r, _, admits) = run(&lanes(
        &format!(r#""liftCover": [{o1}, {o2}]"#),
        &format!("{d1}{d2}"),
    ));
    assert!(matches!(
        r,
        Err(ImportError::Critical {
            part: super::CriticalPart::Lift,
            ..
        })
    ));
    assert_eq!(admits, base);
    // An offset-4 image that may reach the lanes, of the malformed file.
    let (r, _, admits) = run(&lanes(
        "",
        r#", {"id": "bad", "offset": 4, "dst": [{"x": 0, "y": 0, "w": 100, "h": 8}]}"#,
    ));
    assert!(matches!(
        r,
        Err(ImportError::Critical {
            part: super::CriticalPart::Cover,
            ..
        })
    ));
    assert_eq!(admits, base);
}

#[test]
fn hidden_plus_of_a_malformed_file_is_admitted_then_dropped() {
    let (r, _, base) = run(&lanes("", ""));
    r.unwrap();
    let hidden = lanes(
        r#""hiddenCover": [{"id": "hid", "src": "b", "w": 8, "h": 8}]"#,
        r#", {"id": "hid", "dst": [{"x": 20, "y": 200, "w": 430, "h": 600}]}"#,
    );
    let (r, _, admits) = run(&hidden);
    let imported = r.unwrap();
    let dropped = imported
        .report
        .notes
        .iter()
        .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::MissingAsset);
    assert!(dropped);
    assert_eq!(admits, base + 1);
}

#[test]
fn a_short_mine_list_admits_nothing() {
    let eight = format!("[{}]", [r#""p""#; 8].join(", "));
    let seven = format!("[{}]", [r#""p""#; 7].join(", "));
    let json = lanes("", "").replace(
        &format!(r#""mine": {eight}"#),
        &format!(r#""mine": {seven}"#),
    );
    let (r, _, admits) = run(&json);
    assert!(matches!(
        r,
        Err(ImportError::Critical {
            part: super::CriticalPart::Lanes,
            why: NoteKind::UnsupportedField { field: "mine" },
            ..
        })
    ));
    assert_eq!(admits, 0);
}

#[test]
fn a_rejected_lane_approximation_refuses() {
    use super::widget::lanes::REJECTED;
    use super::{DeviationId, LaneApprox};
    let (r, ..) = run(&lanes("", ""));
    r.unwrap();
    REJECTED.with(|c| c.set(Some(LaneApprox::TopCrop)));
    let (r, ..) = run(&lanes("", ""));
    REJECTED.with(|c| c.set(None));
    match r {
        Err(ImportError::Critical { part, why, .. }) => assert_eq!(
            (part, why),
            (
                super::CriticalPart::Lanes,
                NoteKind::Deviation(DeviationId::D9(LaneApprox::TopCrop))
            )
        ),
        other => panic!("{:?}", other.map(|i| i.report)),
    }
}

/// The memory fixture's judges at J = 3 (`tests/import_memory.rs` F5):
/// every timed grade key is kept, and the outer and combo tracks keep one.
#[test]
fn judges_retain_every_grade_key() {
    use super::widget::judge::RETAINED;
    let judges: Vec<String> = [(11, "240"), (11, "-230, -231"), (10, "-230, -231")]
        .iter()
        .enumerate()
        .map(|(i, (keys, op))| {
            let track: Vec<String> = (0..*keys)
                .map(|t| {
                    format!(
                        r#"{{"time": {t}, "x": {}, "y": 500, "w": 80, "h": 20}}"#,
                        100 + t
                    )
                })
                .collect();
            format!(
                r#"{{"id": "j{i}", "index": 0,
                    "images": [{{"id": "i", "timer": 46, "op": [{op}], "loop": 0, "dst": [{}]}}],
                    "numbers": [{{"id": "v", "dst": [{{"x": 10, "y": 0, "w": 6, "h": 6}}]}}]}}"#,
                track.join(", ")
            )
        })
        .collect();
    let json = format!(
        r#"{{"type": 0, "w": 1920, "h": 1080,
            "source": [{{"id": "a", "path": "a.png"}}],
            "image": [{A}],
            "value": [{{"id": "v", "src": "a", "w": 60, "h": 6, "divx": 10, "digit": 2}}],
            "judge": [{}],
            "destination": [{}]}}"#,
        judges.join(", "),
        (0..3)
            .map(|i| format!(r#"{{"id": "j{i}", "dst": [{{"w": 0, "h": 0}}]}}"#))
            .collect::<Vec<_>>()
            .join(", ")
    );
    RETAINED.with(|r| r.borrow_mut().clear());
    let (r, ..) = run(&json);
    r.unwrap();
    let kept = RETAINED.with(|r| r.take());
    assert_eq!(kept, [[1, 1, 11], [1, 1, 11], [1, 1, 10]]);
}

/// A value's availability joins its conditions before the one visibility
/// is compiled: a guarded number compiles its visibility once, with or
/// without conditions of its own, never an earlier one replaced.
#[test]
fn a_guard_joins_the_one_visibility() {
    let number = |r: i32, op: &str| {
        format!(
            r#"{{"type": 0, "w": 1920, "h": 1080,
                "source": [{{"id": "a", "path": "a.png"}}],
                "value": [{{"id": "v", "src": "a", "w": 60, "h": 6, "divx": 10, "digit": 3,
                            "ref": {r}}}],
                "destination": [{{"id": "v", {op} "dst": [{{"w": 6, "h": 6}}]}}]}}"#
        )
    };
    let compiles = |json: &str| {
        let (r, trace, _) = run(json);
        let ops = r.unwrap().skin.ops().total();
        let n = trace.iter().filter(|e| **e == Event::Compile).count();
        (n, ops)
    };
    // EX score, unguarded: its value alone; with an op, one visibility.
    let (plain, plain_ops) = compiles(&number(71, ""));
    assert_eq!(plain, 1);
    assert_eq!(compiles(&number(71, r#""op": [271],"#)).0, 2);
    // The duration, guarded: one visibility, with an op joined into it.
    let (guarded, guarded_ops) = compiles(&number(312, ""));
    assert_eq!(guarded, 2);
    let (joined, joined_ops) = compiles(&number(312, r#""op": [271],"#));
    assert_eq!(joined, 2);
    assert!(plain_ops < guarded_ops && guarded_ops < joined_ops);
    // A value that is always known is not guarded.
    assert_eq!(compiles(&number(163, "")).0, 1);
}
