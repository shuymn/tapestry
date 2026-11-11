//! Private-hook witnesses: the order of an import's charges, compiles and
//! admissions, and the location pass's walk.

use std::path::Path;

use super::asset::ADMITS;
use super::report::WALKED;
use super::{Config, Event, Imported, NoteKind, Object, Severity, TRACE, import};
use crate::error::ImportError;
use skin_model::ScreenKind;

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
    let r = import(
        &mut def,
        &h,
        &Config::defaults(&h),
        skin_model::ScreenKind::Play,
    );
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
fn a_source_with_no_path_refuses_failure_presentation_before_any_file() {
    let json = r#"{"type": 0, "w": 1920, "h": 1080, "source": [{"id": "p"}],
        "image": [{"id": "i", "src": "p", "w": 8, "h": 8}],
        "destination": [{"id": "i", "timer": 3, "dst": [{"w": 1, "h": 1}]}]}"#;
    let (r, _, admits) = run(json);
    match r {
        Err(ImportError::Critical { part, why, .. }) => assert_eq!(
            (part, why),
            (crate::CriticalPart::Failure, NoteKind::MissingAsset)
        ),
        other => panic!("{:?}", other.map(|i| i.report)),
    }
    assert_eq!(admits, 0);
}

#[test]
fn failure_presentation_is_classified_without_work_of_its_own() {
    use Event::{Admit, Charge, Compile};
    // The same destinations on `failed` and on READY: classifying them
    // charges, compiles and admits nothing, and a refusal stops where a
    // decorative element is dropped.
    let dsts = |timer: i32| {
        skin(
            A,
            &format!(
                r#"{{"id": "i", "timer": {timer}, "op": [232], "dst": [{{"w": 1, "h": 1}}]}},
                   {{"id": "i", "timer": {timer}, "op": [7777], "dst": [{{"w": 1, "h": 1}}]}},
                   {{"id": "-111", "timer": {timer}, "dst": [{{"w": 1, "h": 1}}]}}"#
            ),
        )
    };
    let (failed, failed_trace, failed_admits) = run(&dsts(3));
    let (ready, ready_trace, ready_admits) = run(&dsts(40));
    failed.unwrap();
    ready.unwrap();
    assert_eq!(
        failed_trace,
        [Charge, Compile, Charge, Admit, Charge, Charge, Charge]
    );
    assert_eq!((failed_trace, failed_admits), (ready_trace, ready_admits));
    let refused = |timer: i32| {
        let json = skin(
            A,
            &format!(r#"{{"id": "i", "timer": {timer}, "blend": 9, "dst": [{{"w": 1, "h": 1}}]}}"#),
        );
        run(&json)
    };
    let (r, trace, admits) = refused(3);
    assert!(matches!(r, Err(ImportError::Critical { .. })));
    assert_eq!((trace.as_slice(), admits), ([Charge].as_slice(), 0));
    let (r, trace, _) = refused(40);
    r.unwrap();
    assert_eq!(trace, [Charge]);
}

#[test]
fn a_system_image_is_charged_once_and_admits_nothing() {
    use Event::Charge;
    let json = skin(
        A,
        r#"{"id": "-111", "dst": [{"w": 1, "h": 1}]},
           {"id": "-111", "filter": 1, "dst": [{"w": 1, "h": 1}]},
           {"id": "-111", "dst": [{"w": 1, "h": 1}]},
           {"id": "-111", "filter": 1, "dst": [{"w": 1, "h": 1}]},
           {"id": "-100", "dst": [{"w": 1, "h": 1}]}"#,
    );
    let (r, trace, admits) = run(&json);
    let imported = r.unwrap();
    // Each filter's first: the element, then its image; a repeat shares
    // the image; the dropped one charges its element alone.
    assert_eq!(
        trace,
        [Charge, Charge, Charge, Charge, Charge, Charge, Charge]
    );
    assert_eq!(admits, 0);
    let u = imported.report.usage;
    assert_eq!(
        (u.elements, u.images, u.frames, u.file_attempts),
        (5, 2, 2, 0)
    );
}

#[test]
fn a_dropped_element_admits_nothing() {
    let json = skin(
        A,
        r#"{"id": "i", "blend": 9, "dst": [{"w": 1, "h": 1}]},
           {"id": "i", "op": [7777], "dst": [{"w": 1, "h": 1}]},
           {"id": "i", "timer": 2999, "dst": [{"w": 1, "h": 1}]},
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
    // HIDDEN+ of the malformed file trimmed over a destination of
    // negative height: nothing read.
    let hidden = lanes(
        r#""hiddenCover": [{"id": "hid", "src": "b", "w": 8, "h": 8, "disapearLine": 0}]"#,
        r#", {"id": "hid", "dst": [{"x": 20, "y": 200, "w": 430, "h": -600}]}"#,
    );
    let (r, _, admits) = run(&hidden);
    let dropped = r.unwrap().report.notes.iter().any(|n| {
        n.severity == Severity::Dropped && n.kind == NoteKind::UnsupportedField { field: "dst" }
    });
    assert!(dropped);
    assert_eq!(admits, base);
    // LIFT of the malformed file, likewise.
    let (o, d) = lift("l", "b");
    let (r, _, admits) = run(&lanes(
        &format!(r#""liftCover": [{o}]"#),
        &d.replace(r#""h": 200"#, r#""h": -200"#),
    ));
    assert!(matches!(
        r,
        Err(ImportError::Critical {
            part: super::CriticalPart::Lift,
            ..
        })
    ));
    assert_eq!(admits, base);
    // SUDDEN+'s paint of the malformed file in a stretch it refuses.
    let (r, _, admits) = run(&lanes(
        "",
        r#", {"id": "bad", "offset": 4, "stretch": 3, "dst": [{"x": 0, "y": 0, "w": 100, "h": 8}]}"#,
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
fn a_null_part_admits_nothing() {
    let eight = format!("[{}]", [r#""p""#; 8].join(", "));
    let json = lanes("", "").replace(
        &format!(r#""mine": {eight}"#),
        &format!(r#""mine": [{}, null]"#, [r#""p""#; 7].join(", ")),
    );
    let (r, _, admits) = run(&json);
    assert!(matches!(
        r,
        Err(ImportError::Critical {
            part: super::CriticalPart::Lanes,
            why: NoteKind::NotePart {
                field: "mine",
                why: super::PartWhy::NullEntry { index: 7 },
            },
            ..
        })
    ));
    assert_eq!(admits, 0);
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
            why: NoteKind::NotePart {
                field: "mine",
                why: super::PartWhy::Length { len: 7, need: 8 },
            },
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

/// The memory fixture's judges at J = 4 (`tests/import_memory.rs` F5):
/// every timed grade key is kept, and the outer and combo tracks keep one;
/// the grade fitted inside its destination keeps an attachment key per
/// grade key besides.
#[test]
fn judges_retain_every_grade_key() {
    use super::widget::judge::RETAINED;
    let judges: Vec<String> = [
        (11, r#""stretch": 1, "op": [240]"#),
        (11, r#""op": [240]"#),
        (11, r#""op": [-230, -231]"#),
        (10, r#""op": [-230, -231]"#),
    ]
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
                    "images": [{{"id": "i", "timer": 46, {op}, "loop": 0, "dst": [{}]}}],
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
        (0..4)
            .map(|i| format!(r#"{{"id": "j{i}", "dst": [{{"w": 0, "h": 0}}]}}"#))
            .collect::<Vec<_>>()
            .join(", ")
    );
    RETAINED.with(|r| r.borrow_mut().clear());
    let (r, ..) = run(&json);
    assert_eq!(r.unwrap().report.usage.grade_anchor_keys, 11);
    let kept = RETAINED.with(|r| r.take());
    assert_eq!(
        kept,
        [[1, 1, 11, 11], [1, 1, 11, 0], [1, 1, 11, 0], [1, 1, 10, 0]]
    );
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

#[test]
fn image_choices_charge_their_entries_before_their_tables_and_images() {
    use Event::{Admit, Charge, Compile, Refs, Table};
    let json = |objects: &str, dsts: &str| {
        format!(
            r#"{{"type": 0, "w": 1920, "h": 1080,
                "source": [{{"id": "a", "path": "a.png"}}], {objects},
                "destination": [{dsts}]}}"#
        )
    };
    // An image set of two images, drawn twice: its entries, then its
    // selector, its table, each image before its file; the second copy
    // charges its entries and selector again but admits nothing.
    let set = json(
        r#""image": [{"id": "i", "src": "a", "w": 8, "h": 8},
                     {"id": "j", "src": "a", "x": 8, "w": 8, "h": 8}],
           "imageset": [{"id": "s", "images": ["i", "j"], "value": 110}]"#,
        r#"{"id": "s", "dst": [{"w": 1, "h": 1}]}, {"id": "s", "dst": [{"w": 1, "h": 1}]}"#,
    );
    let (r, trace, admits) = run(&set);
    r.unwrap();
    assert_eq!(
        trace,
        [
            Charge, Refs, Compile, Table, Charge, Admit, Charge, //
            Charge, Refs, Charge, Table,
        ]
    );
    assert_eq!(admits, 1);
    // A group: its entries, its selector, its image, then its table.
    let group = json(
        r#""image": [{"id": "g", "src": "a", "w": 16, "h": 8, "divx": 2, "len": 2, "ref": 40}]"#,
        r#"{"id": "g", "dst": [{"w": 1, "h": 1}]}"#,
    );
    let (r, trace, _) = run(&group);
    r.unwrap();
    assert_eq!(trace, [Charge, Refs, Compile, Charge, Admit, Table]);
}

/// A skin of type `ty` whose configured resolution includes `trap.json`
/// (option 901, on by default, which the header pass leaves off) and
/// that draws a critical EX score number from a missing file.
fn trapped(ty: i32) -> String {
    format!(
        r#"{{"type": {ty}, "w": 1920, "h": 1080,
            "property": [
                {{"name": "mode", "item": [{{"name": "on", "op": 901}}], "def": "on"}},
                {{"if": 901, "values": [{{"include": "trap.json"}}]}}
            ],
            "source": [{{"id": "gone", "path": "gone.png"}}],
            "value": [{{"id": "ex", "src": "gone", "w": 10, "h": 1, "divx": 10,
                        "digit": 4, "ref": 71}}],
            "destination": [{{"id": "ex", "dst": [{{"w": 1, "h": 1}}]}}]}}"#
    )
}

/// Imports [`trapped`]`(ty)` into `slot`, `trap.json` present when
/// `trap`: the result and the asset admissions made.
fn into_slot(ty: i32, slot: ScreenKind, trap: bool) -> (Result<Imported, ImportError>, u32) {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("s.json"), trapped(ty)).unwrap();
    if trap {
        std::fs::write(d.path().join("trap.json"), r#"{"name": "trapped"}"#).unwrap();
    }
    ADMITS.with(|n| n.set(0));
    let (mut def, h) = crate::read_header(d.path(), "s.json").unwrap();
    let r = import(&mut def, &h, &Config::defaults(&h), slot);
    (r, ADMITS.with(std::cell::Cell::get))
}

#[test]
fn a_skin_in_another_slot_refuses_before_anything_is_read() {
    use ScreenKind::{Decision, Play, Result, Selection};
    for (ty, slot, found) in [
        (5, Play, Selection),
        (0, Result, Play),
        (6, Play, Decision),
        (0, Decision, Play),
    ] {
        // No trap file: a resolution would refuse on reading it.
        let (r, admits) = into_slot(ty, slot, false);
        match r {
            Err(ImportError::Slot {
                at: None,
                slot: s,
                found: f,
            }) => assert_eq!((s, f), (slot, found)),
            other => panic!("{ty}: {:?}", other.map(|i| i.report)),
        }
        assert_eq!(admits, 0, "{ty}");
    }
    // The control: in its own slot the same skin resolves past the trap
    // and reaches its critical number's missing file.
    let (r, admits) = into_slot(0, Play, true);
    assert!(
        matches!(r, Err(ImportError::Asset { .. })),
        "{:?}",
        r.map(|i| i.report)
    );
    assert!(admits >= 1);
}

/// C1: a decision skin passes the kind gate into its own slot and
/// resolves past the trap; its number, which the decision does not show,
/// is dropped with a note before its file is read.
#[test]
fn a_decision_skin_imports_into_its_own_slot() {
    let (r, admits) = into_slot(6, ScreenKind::Decision, true);
    let i = r.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(i.skin.kind(), ScreenKind::Decision);
    let dropped = i
        .report
        .notes
        .iter()
        .filter(|n| {
            n.severity == Severity::Dropped
                && n.kind
                    == NoteKind::Unsupported {
                        object: Object::Value,
                    }
        })
        .count();
    assert_eq!(dropped, 1, "{:?}", i.report.notes);
    assert_eq!(admits, 0);
}

/// Imports selection skin `body` (beside a 64×64 `a.png` and a font
/// `f.ttf`) into the selection slot, with this thread's witnesses reset.
fn run_selection(body: &str) -> (Result<Imported, ImportError>, Vec<Event>, u32) {
    let d = tempfile::tempdir().unwrap();
    let json = format!(
        r#"{{"type": 5, "w": 1920, "h": 1080,
            "source": [{{"id": "a", "path": "a.png"}}],
            "font": [{{"id": "f", "path": "f.ttf"}}],
            "image": [{{"id": "b", "src": "a", "w": 8, "h": 8}}],
            "text": [{{"id": "t", "font": "f", "ref": 10}}], {body}}}"#
    );
    std::fs::write(d.path().join("s.json"), json).unwrap();
    std::fs::write(d.path().join("f.ttf"), [0, 1, 0, 0, 0]).unwrap();
    png(d.path(), "a.png", 64, 64);
    TRACE.with(|t| t.borrow_mut().clear());
    ADMITS.with(|n| n.set(0));
    let (mut def, h) = crate::read_header(d.path(), "s.json").unwrap();
    let r = import(&mut def, &h, &Config::defaults(&h), ScreenKind::Selection);
    (
        r,
        TRACE.with(|t| t.take()),
        ADMITS.with(std::cell::Cell::get),
    )
}

/// A song list of one placement drawing image set `set` (whose `images`
/// are `images`), titled by `t`, drawn `lists` times.
fn listed(images: &str, lists: usize) -> String {
    let dsts = vec![r#"{"id": "list", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#; lists];
    format!(
        r#""imageset": [{{"id": "set", "images": [{images}]}}],
           "songlist": {{"id": "list", "center": 0,
               "liston": [{{"id": "set", "dst": [{{"x": 0, "y": 900, "w": 300, "h": 40}}]}}],
               "listoff": [{{"id": "set", "dst": [{{"x": 0, "y": 900, "w": 300, "h": 40}}]}}],
               "text": [{{"id": "t", "dst": [{{"x": 0, "y": 0, "w": 100, "h": 10}}]}}]}},
           "destination": [{}]"#,
        dsts.join(",")
    )
}

/// Each song list charge comes before the work it bounds: the placements
/// before any arm is read, an arm's image set entries before any is read
/// or admitted, and each element's quads and visibilities before it is
/// kept. A failing charge leaves its work undone.
#[test]
fn song_list_charges_come_before_their_work() {
    use Event::{Admit, Charge};
    let (r, trace, admits) = run_selection(&listed(r#""b""#, 1));
    r.unwrap();
    // The destination, the placements, the selected arm's set, its image
    // before its file, the unselected arm's set (its image shared), the
    // title's keys again for the folder and "all charts" rows that share
    // it, then the element.
    assert_eq!(
        trace
            .iter()
            .filter(|e| matches!(e, Charge | Admit))
            .copied()
            .collect::<Vec<_>>(),
        [
            Charge, Charge, Charge, Charge, Admit, Charge, Charge, Charge, Charge
        ]
    );
    assert_eq!(admits, 1);
    // An image set past the entry cap: refused at its charge, nothing of
    // it read or admitted, no text compiled.
    let names = vec![r#""b""#; skin_model::MAX_IMAGE_CHOICE_REFS + 1].join(",");
    let (r, trace, admits) = run_selection(&listed(&names, 1));
    assert!(matches!(
        r,
        Err(ImportError::Limit {
            limit: super::ImportLimit::ChoiceRefs,
            ..
        })
    ));
    assert_eq!(admits, 0);
    assert!(!trace.contains(&Admit));
}
