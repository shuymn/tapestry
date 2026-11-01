//! Note parts: ids that name no image draw the source's generated paint;
//! malformed arrays, an unsized unresolved tap and broken images refuse;
//! the body aliases, the tail suppression and the exact note heights.

mod common;

use bms_model::{Chart, ChartBuilder, Lane, LnMode, TimeUs};
use common::lanes::{NOTES, body, eight, note};
use common::play::{Drawn, MS};
use common::{Loaded, import_body};
use skin_beatoraja::{CriticalPart, ImportError, NoteFallback, NoteKind, PartWhy, Severity};
use skin_model::{Atlas, AtlasLimits, Quad, Variant};

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 4_096,
    max_layers: 8,
    reserved_layers: 1,
};
const WHITE: [u8; 4] = [255, 255, 255, 255];
const YELLOW: [u8; 4] = [255, 255, 0, 255];
const RED: [u8; 4] = [255, 0, 0, 255];
const SIZES: &str = r#""size": [20, 20, 20, 20, 20, 20, 20, 20]"#;

/// Eight distinct ids `{prefix}0` to `{prefix}7`, as a JSON list.
fn ids(prefix: &str) -> String {
    let v: Vec<String> = (0..8).map(|i| format!("\"{prefix}{i}\"")).collect();
    format!("[{}]", v.join(", "))
}

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

/// The atlas texel at the middle of `q`'s uv.
fn texel(atlas: &Atlas, q: &Quad) -> [u8; 4] {
    let side = atlas.side();
    let at = |a: f32, b: f32| ((a + b) / 2.0 * side as f32) as u32;
    let (x, y) = (at(q.uv[0], q.uv[2]), at(q.uv[1], q.uv[3]));
    let level = atlas.pages()[usize::from(q.layer) - 1].level(0).unwrap();
    let i = ((y * side + x) * 4) as usize;
    level[i..i + 4].try_into().unwrap()
}

/// The rounded rects at `ms` of `l` over `chart`, each with its atlas
/// colour.
fn painted(l: &Loaded, chart: &Chart, ms: i64) -> Vec<([i32; 4], [u8; 4])> {
    let skin = l.ok().skin.clone();
    let atlas = skin.atlas(Variant::Lo, LIMITS).unwrap();
    let mut d = Drawn::over(skin, chart);
    d.frame(ms, &[])
        .iter()
        .map(|q| {
            assert_eq!(q.color, [1.0; 4], "fallback colour is in the texture");
            (q.rect.map(|v| v.round() as i32), texel(&atlas, q))
        })
        .collect()
}

/// A long note on model lane 1 from 1 s to 1.3 s in `mode`.
fn long_note(mode: Option<LnMode>) -> Chart {
    let mut chart = ChartBuilder::new(150_000)
        .long(
            Lane::new(1).unwrap(),
            TimeUs(1_000 * MS),
            TimeUs(1_300 * MS),
        )
        .build()
        .unwrap();
    chart.meta.ln_mode = mode;
    chart
}

#[test]
fn every_unresolved_part_draws_shared_generated_paint() {
    // F11's shape: every mapped id distinct and unresolved, sizes
    // explicit, no image record at all.
    let edits = [
        (list("note", "nt"), format!(r#""note": {}"#, ids("n"))),
        (list("lnstart", "ls"), format!(r#""lnstart": {}"#, ids("s"))),
        (list("lnend", "le"), format!(r#""lnend": {}"#, ids("e"))),
        (list("lnbody", "lb"), format!(r#""lnbody": {}"#, ids("b"))),
        (
            list("lnactive", "la"),
            format!(r#""lnactive": {}"#, ids("a")),
        ),
        (list("mine", "mi"), format!(r#""mine": {}"#, ids("m"))),
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
        painted(&l, &long_note(Some(LnMode::Cn)), 1_150),
        [
            ([100, 730, 50, 130], YELLOW),
            ([100, 710, 50, 20], YELLOW),
            ([100, 860, 50, 20], YELLOW),
        ]
    );
}

#[test]
fn an_unresolved_tail_keeps_the_mode_tail_rule() {
    let to = format!(r#""lnend": {}"#, ids("x"));
    let l = lanes_edit(&[(&list("lnend", "le"), &to)], SIZES);
    assert_eq!(l.report().usage.note_fallbacks, 8);
    // LN draws body and head, no tail; CN and HCN draw the yellow tail.
    for (mode, tail) in [
        (None, false),
        (Some(LnMode::Ln), false),
        (Some(LnMode::Cn), true),
        (Some(LnMode::Hcn), true),
    ] {
        let p = painted(&l, &long_note(mode), 1_150);
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
    // `lost` names a source with no file: refused as before, never
    // replaced with generated paint.
    let lost = r#"{"id": "lost", "src": "zz", "x": 0, "y": 0, "w": 8, "h": 20}"#;
    let mine = r#""mine": ["mi", "mi", "mi", "mi", "mi", "mi", "mi", "lost"]"#;
    let n = note_edit(&[(&list("mine", "mi"), mine)], "");
    let l = import_body(&body(lost, &n, &[NOTES]));
    match l.err() {
        ImportError::Critical {
            part: CriticalPart::Lanes,
            why,
            ..
        } => assert!(!matches!(why, NoteKind::NotePartFallback { .. }), "{why:?}"),
        e => panic!("{e:?}"),
    }
}

#[test]
fn malformed_selected_arrays_refuse() {
    let mines = format!("{}, ", list("mine", "mi"));
    let cases: [(&str, String, &str, PartWhy); 9] = [
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
        // A trailing entry past the lanes is checked too.
        (
            "mine",
            list("mine", "mi"),
            r#""mine": ["mi", "mi", "mi", "mi", "mi", "mi", "mi", "mi", null]"#,
            PartWhy::NullEntry { index: 8 },
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
        // Every long-note array covers `lnend`'s length.
        (
            "lnstart",
            list("lnend", "le"),
            r#""lnend": ["le", "le", "le", "le", "le", "le", "le", "le", "le"]"#,
            PartWhy::Length { len: 8, need: 9 },
        ),
        (
            "hcnbody",
            list("hcnbody", "lb"),
            r#""hcnbody": ["lb"]"#,
            PartWhy::Length { len: 1, need: 8 },
        ),
    ];
    for (field, from, to, why) in cases {
        part_refused(lanes_edit(&[(&from, to)], ""), field, why);
    }
    // A `lnend` of nine and every selected array of nine loads.
    let nine = |id: &str| format!("[{}]", vec![format!("\"{id}\""); 9].join(", "));
    let mut edits = Vec::new();
    for (field, id) in [
        ("lnstart", "ls"),
        ("lnend", "le"),
        ("lnbody", "lb"),
        ("lnactive", "la"),
        ("hcnstart", "ls"),
        ("hcnend", "le"),
        ("hcnbody", "lb"),
        ("hcnactive", "la"),
        ("hcndamage", "lb"),
        ("hcnreactive", "la"),
    ] {
        edits.push((list(field, id), format!(r#""{field}": {}"#, nine(id))));
    }
    let edits: Vec<(&str, &str)> = edits.iter().map(|(a, b)| (&a[..], &b[..])).collect();
    lanes_edit(&edits, "").ok();
}

#[test]
fn body_aliases_keep_their_selection() {
    let active = format!(r#""lnbodyActive": {}"#, eight("deco"));
    // With `lnbodyActive`, `lnactive` is an unselected alias: absent is
    // fine, supplied it must be well formed.
    let drop_active = format!(", {}", list("lnactive", "la"));
    let l = lanes_edit(&[(&drop_active, "")], &active);
    assert_eq!(l.report().usage.images, 6);
    part_refused(
        lanes_edit(&[(&list("lnactive", "la"), r#""lnactive": null"#)], &active),
        "lnactive",
        PartWhy::Null,
    );
    part_refused(
        lanes_edit(
            &[(&list("lnactive", "la"), r#""lnactive": ["la", null]"#)],
            &active,
        ),
        "lnactive",
        PartWhy::NullEntry { index: 1 },
    );
    // Without it, `lnactive` is selected and required; an empty
    // `lnbodyActive` selects the same.
    part_refused(
        lanes_edit(&[(&drop_active, "")], ""),
        "lnactive",
        PartWhy::Missing,
    );
    part_refused(
        lanes_edit(&[(&drop_active, "")], r#""lnbodyActive": []"#),
        "lnactive",
        PartWhy::Missing,
    );
    part_refused(
        lanes_edit(&[], r#""lnbodyActive": null"#),
        "lnbodyActive",
        PartWhy::Null,
    );
    // The fallback field names follow the selection.
    let l = lanes_edit(
        &[(&drop_active, "")],
        &format!(r#""lnbodyActive": {}"#, ids("q")),
    );
    assert!(fallbacks(&l).iter().all(|e| e.0 == "lnbodyActive"));
    assert_eq!(fallbacks(&l).len(), 8);
    // The HCN branch: with `hcnbodyActive`, its reactive and miss
    // bodies are selected; `hcnactive` is not.
    let hcn = format!(
        r#""hcnbodyActive": {}, "hcnbodyReactive": {}"#,
        eight("lb"),
        eight("la")
    );
    part_refused(lanes_edit(&[], &hcn), "hcnbodyMiss", PartWhy::Missing);
    let full = format!(r#"{hcn}, "hcnbodyMiss": {}"#, eight("lb"));
    let drop_hcn = format!(r#""hcnactive": {}, "#, eight("la"));
    lanes_edit(&[(&drop_hcn, "")], &full).ok();
}

#[test]
fn sizes_are_exact() {
    let sizes = |s: &str| lanes_edit(&[], &format!(r#""size": [{s}]"#));
    // Equal values however spelled.
    let l = sizes("20, 20.0, 2e1, 200E-1, +20, 20.000, 0.2e2, 20");
    assert_eq!(l.result.as_ref().map(|_| ()).ok(), Some(()));
    // Two values one `f32` would merge stay unequal.
    let l = sizes(
        "20.000001, 20.000001, 20.000001, 20.000001, 20.000001, 20.000001, 20.000001, 20.0000011",
    );
    match l.err() {
        ImportError::Critical {
            why: NoteKind::UnsupportedField { field: "size" },
            ..
        } => {}
        e => panic!("{e:?}"),
    }
    // A used size past the rationals is a range error; an unused one is
    // checked as a number only.
    let l = sizes("1e39, 20, 20, 20, 20, 20, 20, 20");
    assert!(matches!(l.err(), ImportError::Range { field: "size", .. }));
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
