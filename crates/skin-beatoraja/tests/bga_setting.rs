//! The BGA setting, options 40 (`bgaoff`) and 41 (`bgaon`), answered as off
//! on every screen (docs/stage3.md D33): 40 holds, 41 does not, option 170
//! still reads the chart's BGA flag, and each destination record that
//! names 40 or 41 in its `op` or `draw` carries one located D33 note,
//! whatever else becomes of it.

mod common;

use bms_model::ChartBuilder;
use common::lanes::{NOTES, body, note};
use common::play::Drawn;
use common::{import_dir, skin_with};
use skin_beatoraja::{
    Cancel, Config, DeviationId, Imported, NoteKind, NoteLoc, Severity, TrackWhy, read_lua_header,
};
use skin_model::ChartMetadata;

const D33: NoteKind = NoteKind::Deviation(DeviationId::D33);

/// Line and column (from 1, in characters) of `needle`'s only occurrence
/// in `text`.
fn place(text: &str, needle: &str) -> (u32, u32) {
    assert_eq!(text.matches(needle).count(), 1, "{needle:?} once");
    let at = text.find(needle).unwrap();
    let before = &text[..at];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap().chars().count() + 1;
    (u32::try_from(line).unwrap(), u32::try_from(col).unwrap())
}

/// The places of `imported`'s D33 notes, each a deviation, in order.
fn d33_places(imported: &Imported) -> Vec<(u32, u32)> {
    imported
        .report
        .notes
        .iter()
        .filter(|n| n.kind == D33)
        .map(|n| {
            assert_eq!(n.severity, Severity::Deviation);
            let NoteLoc { line, col, .. } = n.at.expect("located");
            (line, col)
        })
        .collect()
}

/// The places of `needles` in `text`.
fn places(text: &str, needles: &[&str]) -> Vec<(u32, u32)> {
    needles.iter().map(|n| place(text, n)).collect()
}

/// A 1920×1080 play skin with source `a` (64×64) and image `i`, and
/// `rest` after them.
fn play_json(rest: &str) -> String {
    format!(
        "{{\"type\": 0, \"w\": 1920, \"h\": 1080,\n\
         \"source\": [{{\"id\": \"a\", \"path\": \"a.png\"}}],\n\
         \"image\": [{{\"id\": \"i\", \"src\": \"a\", \"w\": 64, \"h\": 64}}],\n\
         {rest}}}"
    )
}

/// `json` imported, with the folder its atlas reads.
fn imported_in(json: &str) -> (tempfile::TempDir, Imported) {
    let d = skin_with(json, &[("a.png", 64, 64)]);
    let i = import_dir(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
    (d, i)
}

fn imported(json: &str) -> Imported {
    imported_in(json).1
}

/// The widths drawn at 0 ms over a chart whose BGA flag is `bga`.
fn widths(json: &str, bga: Option<bool>) -> Vec<i32> {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let meta = ChartMetadata {
        bga,
        ..ChartMetadata::UNKNOWN
    };
    let (_dir, i) = imported_in(json);
    let mut d = Drawn::over_meta(i.skin, &chart, meta);
    let mut w: Vec<i32> = d.rects(0).iter().map(|r| r[2]).collect();
    w.sort_unstable();
    w
}

/// A destination of `i` under `op`, drawn `w` wide, on its own line.
fn under(op: &str, w: i32) -> String {
    format!("\n{{\"id\": \"i\", \"op\": [{op}], \"dst\": [{{\"w\": {w}, \"h\": 10}}]}}")
}

/// T1: 40 holds and 41 does not, by number, sign and name, on charts with
/// and without a BGA, while 170 reads the chart's flag; each destination
/// naming 40 or 41 has one D33 at its record, the one under 170 none.
#[test]
fn options_40_and_41_read_as_bga_off() {
    let ops = [
        ("41, 170", 10),
        ("40", 11),
        ("-40", 12),
        ("-41", 13),
        ("\"bgaon\"", 14),
        ("\"!bgaon\"", 15),
        ("170", 16),
    ];
    let dsts: Vec<String> = ops.iter().map(|&(op, w)| under(op, w)).collect();
    let json = play_json(&format!("\"destination\": [{}]", dsts.join(",")));
    assert_eq!(widths(&json, Some(true)), [11, 13, 15]);
    assert_eq!(widths(&json, Some(false)), [11, 13, 15, 16]);
    let needles: Vec<String> = ops[..6]
        .iter()
        .map(|&(op, _)| format!("{{\"id\": \"i\", \"op\": [{op}]"))
        .collect();
    let needles: Vec<&str> = needles.iter().map(String::as_str).collect();
    assert_eq!(d33_places(&imported(&json)), places(&json, &needles));
}

/// T1c: a destination that names 40 or 41 carries D33 beside whatever
/// else ends it: an earlier false option, an undefined object, an
/// unsupported object, a movie, a source that does not resolve and no
/// keys.
#[test]
fn early_exits_keep_their_d33() {
    let json = String::from(
        "{\"type\": 0, \"w\": 1920, \"h\": 1080,\n\
         \"property\": [{\"name\": \"p\", \"item\": [{\"name\": \"off\", \"op\": 900}, {\"name\": \"on\", \"op\": 901}], \"def\": \"on\"}],\n\
         \"source\": [{\"id\": \"a\", \"path\": \"a.png\"}, {\"id\": \"m\", \"path\": \"m.mp4\"}],\n\
         \"image\": [{\"id\": \"i\", \"src\": \"a\", \"w\": 64, \"h\": 64},\n\
                     {\"id\": \"mv\", \"src\": \"m\", \"w\": 64, \"h\": 64},\n\
                     {\"id\": \"nos\", \"src\": \"none\", \"w\": 8, \"h\": 8}],\n\
         \"bga\": {\"id\": \"b\"},\n\
         \"destination\": [\n\
         {\"id\": \"i\", \"op\": [900, 41], \"dst\": [{\"w\": 8, \"h\": 8}]},\n\
         {\"id\": \"gone\", \"op\": [41], \"dst\": [{\"w\": 8, \"h\": 8}]},\n\
         {\"id\": \"b\", \"op\": [41], \"dst\": [{\"w\": 8, \"h\": 8}]},\n\
         {\"id\": \"mv\", \"op\": [41], \"dst\": [{\"w\": 8, \"h\": 8}]},\n\
         {\"id\": \"nos\", \"op\": [40], \"dst\": [{\"w\": 8, \"h\": 8}]},\n\
         {\"id\": \"i\", \"op\": [-41], \"dst\": []}]}",
    );
    let i = imported(&json);
    let kinds = common::kinds(&i.report);
    assert!(kinds.contains(&NoteKind::UndefinedObject), "{kinds:?}");
    assert!(kinds.contains(&NoteKind::UndefinedSource), "{kinds:?}");
    assert_eq!(
        d33_places(&i),
        places(
            &json,
            &[
                "{\"id\": \"i\", \"op\": [900, 41]",
                "{\"id\": \"gone\"",
                "{\"id\": \"b\", \"op\"",
                "{\"id\": \"mv\", \"op\"",
                "{\"id\": \"nos\", \"op\"",
                "{\"id\": \"i\", \"op\": [-41]",
            ]
        )
    );
}

/// T1d: one D33 per destination record: two undefined destinations in a
/// row and a defined one after them each have their own, and two
/// references in one destination give one.
#[test]
fn each_record_has_one_d33() {
    let json = play_json(
        "\"destination\": [\n\
         {\"id\": \"u1\", \"op\": [41], \"dst\": [{\"w\": 8, \"h\": 8}]},\n\
         {\"id\": \"u2\", \"op\": [41], \"dst\": [{\"w\": 8, \"h\": 8}]},\n\
         {\"id\": \"i\", \"op\": [40], \"dst\": [{\"w\": 8, \"h\": 8}]},\n\
         {\"id\": \"i\", \"op\": [40, \"!bgaon\"], \"draw\": \"bgaoff\", \"dst\": [{\"w\": 9, \"h\": 8}]}]",
    );
    assert_eq!(
        d33_places(&imported(&json)),
        places(
            &json,
            &[
                "{\"id\": \"u1\"",
                "{\"id\": \"u2\"",
                "{\"id\": \"i\", \"op\": [40], ",
                "{\"id\": \"i\", \"op\": [40, ",
            ]
        )
    );
}

/// T1e: a bar line group past the key cap is dropped before its keys are
/// read, and still carries its D33 at its record.
#[test]
fn a_bar_group_past_the_key_cap_keeps_its_d33() {
    let key = r#"{"x": 0, "y": 0, "w": 8, "h": 2}"#;
    let many = vec![key; skin_model::MAX_TRACK_KEYS + 1].join(", ");
    let rec = format!(r#"{{"id": "bar", "op": [41], "dst": [{many}]}}"#);
    let objects = note(&format!(r#""group": [{rec}]"#));
    let body = body("", &objects, &[NOTES]);
    let loaded = common::import_body(&body);
    let i = loaded.ok();
    assert!(common::has(&i.report, NoteKind::Track(TrackWhy::Limits)));
    let json = std::fs::read_to_string(loaded.dir.path().join("s.json")).unwrap();
    assert_eq!(
        d33_places(i),
        places(&json, &[r#"{"id": "bar", "op": [41]"#])
    );
}

/// T1e: a selection song list's placement under 40, read by its slot and
/// again by its placement, has one D33.
#[test]
fn a_song_list_placement_read_twice_has_one_d33() {
    use common::selection::{SET, TEXT_T, TITLE, folder, import_selection, json};
    let on = r#"{"x": 0, "y": 100, "w": 8, "h": 8}], "op": [40, 0"#;
    let list =
        common::selection::song_list(&[on], &[r#"{"x": 0, "y": 100, "w": 8, "h": 8}"#], 0, TITLE);
    let body = format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}], {list},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]"#
    );
    let d = folder(&body);
    let i = import_selection(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
    let text = json(&body);
    assert_eq!(
        d33_places(&i),
        places(
            &text,
            &[r#"{"id": "set", "dst": [{"x": 0, "y": 100, "w": 8, "h": 8}], "op": [40, 0"#]
        )
    );
}

/// T3: a Lua play skin's configured pass reads `main_state.option(40)` as
/// true and `option(41)` as false while it builds its tables.
#[test]
fn the_configured_pass_reads_bga_off() {
    let d = common::skin(&[(
        "s.luaskin",
        "local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = { { id = \"a\", path = \"a.png\" } }\n\
         skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 8, h = 8 } }\n\
         local w = 10\n\
         if ms.option(40) then w = w + 1 end\n\
         if ms.option(41) then w = w + 100 end\n\
         if ms.option(-41) then w = w + 2 end\n\
         skin.destination = { { id = \"i\", dst = { { w = w, h = 10 } } } }\n\
         return skin\n",
    )]);
    common::png(d.path(), "a.png", 64, 64);
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let i = common::lua_play(&h, &Config::default(), &cancel).unwrap();
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let mut p = Drawn::over_meta(i.imported.skin, &chart, ChartMetadata::UNKNOWN);
    let w: Vec<i32> = p.rects(0).iter().map(|r| r[2]).collect();
    assert_eq!(w, [13]);
}

/// T4: on the result screen 40 and −41 draw and 41 and −40 do not, each
/// with one D33 at its record.
#[test]
fn the_result_screen_reads_bga_off() {
    use common::result::{Res, best, folder, graph_json, import_result, json, ready};
    use common::selection::{B0, BAR, LABEL, LAMP};
    let cases = [("40", "lamp"), ("-40", "lab"), ("41", "bar"), ("-41", "b0")];
    let dsts: Vec<String> = cases
        .iter()
        .map(|(op, id)| {
            format!(
                "
{{\"id\": \"{id}\", \"op\": [{op}], \"dst\": [{{\"x\": 0, \"y\": 0, \"w\": 8, \"h\": 8}}]}}"
            )
        })
        .collect();
    let body = graph_json("", &dsts.join(","));
    let d = folder(&body);
    let i = import_result(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
    let text = json(&body);
    let needles: Vec<String> = cases
        .iter()
        .map(|(op, id)| format!("{{\"id\": \"{id}\", \"op\": [{op}]"))
        .collect();
    let needles: Vec<&str> = needles.iter().map(String::as_str).collect();
    assert_eq!(d33_places(&i), places(&text, &needles));
    let mut r = Res::new(i.skin);
    r.view(&ready(10, true, best(skin_model::ResultNumber::Shown(3))));
    let ours = [LAMP, LABEL, BAR, B0];
    let mut drawn: Vec<[u8; 2]> = r
        .drawn(0)
        .into_iter()
        .map(|(t, _)| t)
        .filter(|t| ours.contains(t))
        .collect();
    drawn.sort_unstable();
    let mut want = vec![LAMP, B0];
    want.sort_unstable();
    assert_eq!(drawn, want);
}
