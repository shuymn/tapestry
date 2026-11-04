//! Note parts: ids that name no image draw the source's generated paint;
//! malformed drawn arrays, an unsized unresolved tap and broken images
//! refuse, and undrawn data is not checked;
//! the body aliases, the tail suppression and the exact note heights.

mod common;

use bms_model::{Chart, ChartBuilder, Lane, LnMode, TimeUs};
use common::lanes::{NOTES, body, eight, indexed_ids, note};
use common::play::{Drawn, MS, long_note, quad_texel, round};
use common::{Loaded, import_body};
use skin_beatoraja::{CriticalPart, ImportError, NoteFallback, NoteKind, PartWhy, Severity};
use skin_model::{AtlasLimits, Variant};

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 4_096,
    max_layers: 8,
    reserved_layers: 1,
};
const WHITE: [u8; 4] = [255, 255, 255, 255];
const YELLOW: [u8; 4] = [255, 255, 0, 255];
const RED: [u8; 4] = [255, 0, 0, 255];
const SIZES: &str = r#""size": [20, 20, 20, 20, 20, 20, 20, 20]"#;

/// The default note member with `from` replaced by `to` once, and
/// `extra` fields.
fn note_edit(edits: &[(&str, &str)], extra: &str) -> String {
    let mut n = note(extra);
    for (from, to) in edits {
        assert!(n.contains(from), "{from}");
        n = n.replacen(from, to, 1);
    }
    n
}

/// The lanes with the note member edited.
fn lanes_edit(edits: &[(&str, &str)], extra: &str) -> Loaded {
    import_body(&body("", &note_edit(edits, extra), &[NOTES]))
}

/// Field `field`'s list `eight(id)` as written in the note member.
fn list(field: &str, id: &str) -> String {
    format!(r#""{field}": {}"#, eight(id))
}

#[track_caller]
fn part_refused(l: Loaded, field: &'static str, why: PartWhy) -> u32 {
    match l.err() {
        ImportError::Critical {
            at,
            part: CriticalPart::Lanes,
            why: NoteKind::NotePart { field: f, why: w },
        } => {
            assert_eq!((f, w), (field, why));
            at.line
        }
        e => panic!("{e:?}"),
    }
}

/// The fallback entries of an import: field, model lane, paint, and
/// whether it is located.
fn fallbacks(l: &Loaded) -> Vec<(&'static str, u8, NoteFallback)> {
    l.report()
        .notes
        .iter()
        .filter_map(|n| match n.kind {
            NoteKind::NotePartFallback { field, lane, paint } => {
                assert_eq!(n.severity, Severity::Fallback);
                assert!(n.at.is_some(), "{n:?}");
                Some((field, lane, paint))
            }
            _ => None,
        })
        .collect()
}

/// The rounded rects at `ms` of `l` over `chart`, each with its atlas
/// colour.
fn painted(l: &Loaded, chart: &Chart, ms: i64) -> Vec<([i32; 4], [u8; 4])> {
    let skin = l.ok().skin.clone();
    let atlas = skin.atlas(Variant::Lo, LIMITS, &|| false).unwrap();
    let mut d = Drawn::over(skin, chart);
    d.frame(ms, &[])
        .iter()
        .map(|q| {
            assert_eq!(q.color, [1.0; 4], "fallback colour is in the texture");
            (round(q.rect), quad_texel(&atlas, q))
        })
        .collect()
}

#[test]
fn every_unresolved_part_draws_shared_generated_paint() {
    // F11's shape: every mapped id distinct and unresolved, sizes
    // explicit, no image record at all.
    let edits = [
        (
            list("note", "nt"),
            format!(r#""note": {}"#, indexed_ids("n")),
        ),
        (
            list("lnstart", "ls"),
            format!(r#""lnstart": {}"#, indexed_ids("s")),
        ),
        (
            list("lnend", "le"),
            format!(r#""lnend": {}"#, indexed_ids("e")),
        ),
        (
            list("lnbody", "lb"),
            format!(r#""lnbody": {}"#, indexed_ids("b")),
        ),
        (
            list("lnactive", "la"),
            format!(r#""lnactive": {}"#, indexed_ids("a")),
        ),
        (
            list("mine", "mi"),
            format!(r#""mine": {}"#, indexed_ids("m")),
        ),
    ];
    let edits: Vec<(&str, &str)> = edits.iter().map(|(a, b)| (&a[..], &b[..])).collect();
    let n = note_edit(&edits, SIZES);
    let json = format!(r#""image": [], {n}, "destination": [{NOTES}]"#);
    let l = import_body(&json);
    let r = l.report();
    assert_eq!(r.usage.note_fallbacks, 48);
    assert_eq!((r.usage.images, r.usage.frames), (3, 3));
    assert_eq!((r.usage.file_attempts, r.usage.files), (0, 0));
    let f = fallbacks(&l);
    assert_eq!(f.len(), 48);
    // Source lane 0 is model lane 1, source lane 7 the scratch, lane 0.
    assert_eq!(f[0], ("note", 1, NoteFallback::Tap));
    assert!(f.contains(&("note", 0, NoteFallback::Tap)));
    for field in ["lnstart", "lnend", "lnbody", "lnactive"] {
        assert_eq!(f.iter().filter(|e| e.0 == field).count(), 8, "{field}");
        assert!(
            f.iter()
                .filter(|e| e.0 == field)
                .all(|e| e.2 == NoteFallback::Long)
        );
    }
    assert!(
        f.iter()
            .filter(|e| e.0 == "mine")
            .all(|e| e.2 == NoteFallback::Mine)
    );
    // Taps white, mines red, a CN's body, tail and head yellow, at the
    // lanes' geometry.
    let tap = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(1_000 * MS))
        .mine(Lane::new(2).unwrap(), TimeUs(1_000 * MS), 10)
        .build()
        .unwrap();
    assert_eq!(
        painted(&l, &tap, 1_000),
        [([100, 860, 50, 20], WHITE), ([150, 860, 50, 20], RED)]
    );
    assert_eq!(
        painted(&l, &long_note(Some(LnMode::Cn), 0), 1_150),
        [
            ([100, 730, 50, 130], YELLOW),
            ([100, 710, 50, 20], YELLOW),
            ([100, 860, 50, 20], YELLOW),
        ]
    );
}

#[test]
fn an_unresolved_tail_keeps_the_mode_tail_rule() {
    let to = format!(r#""lnend": {}"#, indexed_ids("x"));
    let l = lanes_edit(&[(&list("lnend", "le"), &to)], SIZES);
    assert_eq!(l.report().usage.note_fallbacks, 8);
    // LN draws body and head, no tail; CN and HCN draw the yellow tail.
    for (mode, tail) in [
        (None, false),
        (Some(LnMode::Ln), false),
        (Some(LnMode::Cn), true),
        (Some(LnMode::Hcn), true),
    ] {
        let p = painted(&l, &long_note(mode, 0), 1_150);
        let rects: Vec<[i32; 4]> = p.iter().map(|e| e.0).collect();
        let mut want = vec![[100, 730, 50, 130]];
        if tail {
            want.push([100, 710, 50, 20]);
        }
        want.push([100, 860, 50, 20]);
        assert_eq!(rects, want, "{mode:?}");
        assert_eq!(p.iter().any(|e| e.1 == YELLOW), tail, "{mode:?}");
    }
}

#[test]
fn an_unresolved_tap_needs_its_size() {
    let bad = r#""note": ["nt", "nt", "nt", "no", "nt", "nt", "nt", "nt"]"#;
    let edit = [(&list("note", "nt")[..], bad)];
    // No `size`: the source reads the missing image's height.
    let line = part_refused(
        lanes_edit(&edit, ""),
        "note",
        PartWhy::MissingSize { lane: 3 },
    );
    assert!(line > 0);
    // A size for lanes 0 to 2 only still leaves lane 3 without one.
    let short = r#""size": [20, 20, 20]"#;
    part_refused(
        lanes_edit(&edit, short),
        "note",
        PartWhy::MissingSize { lane: 3 },
    );
    // Sized, it draws white on model lane 4.
    let l = lanes_edit(&edit, SIZES);
    assert_eq!(fallbacks(&l), [("note", 4, NoteFallback::Tap)]);
    assert_eq!(l.report().usage.images, 7);
    let tap = ChartBuilder::new(150_000)
        .tap(Lane::new(4).unwrap(), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    assert_eq!(painted(&l, &tap, 1_000), [([250, 860, 50, 20], WHITE)]);
    // An unresolved part other than the tap needs no size.
    let mine = r#""mine": ["mi", "mi", "mi", "mi", "mi", "mi", "mi", "zz"]"#;
    let l = lanes_edit(&[(&list("mine", "mi"), mine)], "");
    assert_eq!(fallbacks(&l), [("mine", 0, NoteFallback::Mine)]);
}

#[test]
fn empty_ids_resolve_like_any_other() {
    let empty = r#""mine": ["", "", "", "", "", "", "", ""]"#;
    let edit = [(&list("mine", "mi")[..], empty)];
    // No image of the empty id: generated paint.
    let l = lanes_edit(&edit, "");
    assert_eq!(fallbacks(&l).len(), 8);
    // With one, its image draws.
    let img = r#"{"id": "", "src": "a", "x": 40, "y": 0, "w": 8, "h": 20}"#;
    let l = import_body(&body(img, &note_edit(&edit, ""), &[NOTES]));
    assert!(fallbacks(&l).is_empty());
    assert_eq!(l.report().usage.note_fallbacks, 0);
}

#[test]
fn a_broken_resolved_image_refuses_without_fallback() {
    // `lost` names a source with no file: refused with the file's error,
    // never replaced with generated paint.
    let lost = r#"{"id": "lost", "src": "zz", "x": 0, "y": 0, "w": 8, "h": 20}"#;
    let mine = r#""mine": ["mi", "mi", "mi", "mi", "mi", "mi", "mi", "lost"]"#;
    let n = note_edit(&[(&list("mine", "mi"), mine)], "");
    let l = import_body(&body(lost, &n, &[NOTES]));
    match l.err() {
        ImportError::Asset { .. } => {}
        e => panic!("{e:?}"),
    }
}

#[test]
fn a_part_whose_source_is_undefined_refuses() {
    // The source fails the whole skin on it
    // (`JsonPlaySkinObjectLoader.java:46-170`); no generated paint.
    let undef = r#"{"id": "undef", "x": 0, "y": 0, "w": 8, "h": 20}"#;
    let mine = r#""mine": ["mi", "mi", "mi", "mi", "mi", "mi", "mi", "undef"]"#;
    let n = note_edit(&[(&list("mine", "mi"), mine)], "");
    match import_body(&body(undef, &n, &[NOTES])).err() {
        ImportError::Critical { part, why, .. } => {
            assert_eq!(
                (part, why),
                (CriticalPart::Lanes, NoteKind::UndefinedSource)
            );
        }
        e => panic!("{e:?}"),
    }
}

/// The refusal's file, line and column, and those of the first `marker`
/// in that file, `skip` bytes into it.
#[track_caller]
fn refusal_and_marker(l: Loaded, marker: &str, skip: usize) -> [(String, u32, u32); 2] {
    let Loaded { dir, result } = l;
    let at = match result {
        Err(ImportError::Critical {
            at,
            why: NoteKind::NotePart { .. },
            ..
        }) => at,
        r => panic!("{:?}", r.err()),
    };
    let text = std::fs::read_to_string(dir.path().join(&at.file)).unwrap();
    let offset = text.find(marker).unwrap() + skip;
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let col = offset - before.rfind('\n').map_or(0, |i| i + 1) + 1;
    let marked = (at.file.clone(), line as u32, col as u32);
    [(at.file, at.line, at.col), marked]
}

#[test]
fn part_refusals_are_located_in_the_note_record() {
    // A missing array: at the note record, not its destination.
    let mines = format!("{}, ", list("mine", "mi"));
    let record = r#""note": {"id": "notes""#;
    let [got, want] = refusal_and_marker(lanes_edit(&[(&mines, "")], ""), record, 8);
    assert_eq!(got, want);
    // A malformed array: at its value.
    let null = lanes_edit(&[(&list("mine", "mi"), r#""mine": null"#)], "");
    let [got, want] = refusal_and_marker(null, r#""mine": null"#, 8);
    assert_eq!(got, want);
}

#[test]
fn malformed_drawn_arrays_refuse() {
    let mines = format!("{}, ", list("mine", "mi"));
    let cases: [(&str, String, &str, PartWhy); 7] = [
        ("mine", mines.clone(), "", PartWhy::Missing),
        ("mine", list("mine", "mi"), r#""mine": null"#, PartWhy::Null),
        (
            "mine",
            list("mine", "mi"),
            r#""mine": "mi""#,
            PartWhy::NotArray,
        ),
        (
            "mine",
            list("mine", "mi"),
            r#""mine": ["mi", "mi", "mi", "mi", "mi", "mi", "mi"]"#,
            PartWhy::Length { len: 7, need: 8 },
        ),
        (
            "note",
            list("note", "nt"),
            r#""note": ["nt", "nt", "nt", "nt", "nt", "nt", "nt", "nt", "nt"]"#,
            PartWhy::Length { len: 9, need: 8 },
        ),
        (
            "note",
            list("note", "nt"),
            r#""note": ["nt", "nt", {}, "nt", "nt", "nt", "nt", "nt"]"#,
            PartWhy::BadEntry { index: 2 },
        ),
        (
            "lnstart",
            list("lnstart", "ls"),
            r#""lnstart": ["ls", "ls", "ls", "ls", "ls", "ls", "ls", null]"#,
            PartWhy::NullEntry { index: 7 },
        ),
    ];
    for (field, from, to, why) in cases {
        part_refused(lanes_edit(&[(&from, to)], ""), field, why);
    }
}

/// Only what the lanes draw is checked: the eight lanes' entries of the
/// six drawn arrays. Entries past the lanes, a `lnend` longer than the
/// other long-note arrays, and the HCN arrays, which only add a note,
/// may be anything.
#[test]
fn undrawn_part_data_is_not_checked() {
    let nine = |id: &str, last: &str| {
        let mut v = vec![format!("\"{id}\""); 8];
        v.push(last.to_owned());
        format!("[{}]", v.join(", "))
    };
    // `note` must have exactly eight; the others any past the lanes.
    for (field, id) in [
        ("lnstart", "ls"),
        ("lnend", "le"),
        ("lnbody", "lb"),
        ("lnactive", "la"),
        ("mine", "mi"),
    ] {
        for last in ["null", "{}", "[]"] {
            let to = format!(r#""{field}": {}"#, nine(id, last));
            lanes_edit(&[(&list(field, id), &to)], "").ok();
        }
    }
    let le = format!(r#""lnend": {}"#, nine("le", r#""le""#));
    lanes_edit(&[(&list("lnend", "le"), &le)], "").ok();
    for to in [
        r#""hcnbody": ["lb"]"#,
        r#""hcnbody": null"#,
        r#""hcnbody": 7"#,
    ] {
        lanes_edit(&[(&list("hcnbody", "lb"), to)], "").ok();
    }
    let hcn = format!(r#""hcnstart": {}, "#, eight("ls"));
    lanes_edit(&[(&hcn, r#""hcnstart": [null], "#)], "").ok();
}

#[test]
fn body_aliases_keep_their_selection() {
    let active = format!(r#""lnbodyActive": {}"#, eight("deco"));
    // With `lnbodyActive`, `lnactive` is not drawn: absent or anything
    // supplied, the lanes load.
    let drop_active = format!(", {}", list("lnactive", "la"));
    let l = lanes_edit(&[(&drop_active, "")], &active);
    assert_eq!(l.report().usage.images, 6);
    for to in [r#""lnactive": null"#, r#""lnactive": ["la", null]"#] {
        lanes_edit(&[(&list("lnactive", "la"), to)], &active).ok();
    }
    // Without it, `lnactive` is drawn and required; an empty or `null`
    // `lnbodyActive` selects the same.
    for extra in ["", r#""lnbodyActive": []"#, r#""lnbodyActive": null"#] {
        part_refused(
            lanes_edit(&[(&drop_active, "")], extra),
            "lnactive",
            PartWhy::Missing,
        );
    }
    // The selected `lnbodyActive` is drawn and checked.
    part_refused(
        lanes_edit(
            &[],
            r#""lnbodyActive": ["d", "d", "d", "d", "d", "d", "d"]"#,
        ),
        "lnbodyActive",
        PartWhy::Length { len: 7, need: 8 },
    );
    // The fallback field names follow the selection.
    let l = lanes_edit(
        &[(&drop_active, "")],
        &format!(r#""lnbodyActive": {}"#, indexed_ids("q")),
    );
    assert!(fallbacks(&l).iter().all(|e| e.0 == "lnbodyActive"));
    assert_eq!(fallbacks(&l).len(), 8);
    // HCN arrays are not drawn: a partial set loads.
    let hcn = format!(r#""hcnbodyActive": {}"#, eight("lb"));
    lanes_edit(&[], &hcn).ok();
}

#[test]
fn sizes_are_exact() {
    let sizes = |s: &str| lanes_edit(&[], &format!(r#""size": [{s}]"#));
    // Equal values however spelled.
    let l = sizes("20, 20.0, 2e1, 200E-1, +20, 20.000, 0.2e2, 20");
    assert_eq!(l.result.as_ref().map(|_| ()).ok(), Some(()));
    // Unequal values load, each lane its own; the fit is checked on the
    // exact value before any conversion: 610.0000001 passes the lanes' top
    // by 10^-7 and refuses, though its `f32` is 610, which fits.
    let l = sizes(
        "20.000001, 20.000001, 20.000001, 20.000001, 20.000001, 20.000001, 20.000001, 20.0000011",
    );
    assert_eq!(l.result.as_ref().map(|_| ()).ok(), Some(()));
    assert_eq!(610.000_000_1_f32, 610.0);
    sizes("20, 20, 20, 610, 20, 20, 20, 20").ok();
    let l = sizes("20, 20, 20, 610.0000001, 20, 20, 20, 20");
    match l.err() {
        ImportError::Critical {
            why: NoteKind::UnsupportedField { field: "offset" },
            ..
        } => {}
        e => panic!("{e:?}"),
    }
    // A used size past the rationals is a range error; an unused one is
    // checked as a number only.
    let l = sizes("1e39, 20, 20, 20, 20, 20, 20, 20");
    assert!(matches!(l.err(), ImportError::Range { field: "size", .. }));
    // Precision past i128 that reduces into it is a size.
    let fine = ["20.00000000000000000000000000000000003125"; 8].join(", ");
    sizes(&fine).ok();
    let coarse = ["20.000000000000000000000000000000000000001"; 8].join(", ");
    assert!(matches!(
        sizes(&coarse).err(),
        ImportError::Range { field: "size", .. }
    ));
    sizes("20, 20, 20, 20, 20, 20, 20, 20, 1e39").ok();
    let l = sizes(r#"20, 20, 20, 20, 20, 20, 20, 20, "x""#);
    assert!(matches!(l.err(), ImportError::Type { field: "size", .. }));
}

#[test]
fn drawn_heights_follow_the_exact_size() {
    // 20.5 everywhere: the note is drawn 20.5 tall.
    let l = lanes_edit(
        &[],
        r#""size": [20.5, 20.5, 20.5, 20.5, 20.5, 20.5, 20.5, 20.5]"#,
    );
    let tap = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let mut d = Drawn::over(l.ok().skin.clone(), &tap);
    let q = d.frame(1_000, &[]);
    assert_eq!(q[0].rect, [100.0, 859.5, 50.0, 20.5]);
}
