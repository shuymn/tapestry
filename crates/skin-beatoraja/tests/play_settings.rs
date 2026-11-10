//! The play's settings bridges and empty states (S10-13b): image index
//! 55, the speed policy's hi-speed fix, and number 12, the display
//! offset, from the frame's live presentation, while loading too; image
//! index 75 off; options 84, 280-283, 289 and 400 false; text 1003
//! empty. JSON and Lua alike.

mod common;

use bms_model::TimeUs;
use common::play::{Drawn, Loader, readouts_for, round};
use common::{has, import_body, write};
use skin_beatoraja::lua::Disposition;
use skin_beatoraja::{Cancel, Config, DeviationId, LuaImported, NoteKind, read_lua_header};
use skin_model::{FrameReadouts, LaneView};

/// The readouts of a frame showing no lanes, with hi-speed fix `fix`.
fn fixed(fix: u8) -> FrameReadouts {
    FrameReadouts {
        fix_hispeed: fix,
        ..readouts_for(LaneView::NONE)
    }
}

/// Each quad's frame: its index in the uv table.
fn frames(uvs: &skin_model::UvTable, quads: &[skin_model::Quad]) -> Vec<usize> {
    quads
        .iter()
        .map(|q| {
            uvs.frames()
                .iter()
                .position(|f| f.uv == q.uv && f.layer == q.layer)
                .expect("a frame of the sheet")
        })
        .collect()
}

/// A group of ten 10×10 frames of `a.png` in five runs of two, selected
/// by image index `index`, untimed.
fn group(index: i32) -> common::Loaded {
    import_body(&format!(
        r#""image": [{{"id": "g", "src": "a", "w": 50, "h": 20, "divx": 5, "divy": 2,
                      "len": 5, "ref": {index}}}],
           "destination": [{{"id": "g", "dst": [{{"w": 10, "h": 10}}]}}]"#
    ))
}

/// Whether `l` dropped anything as unsupported.
fn dropped(l: &common::Loaded) -> bool {
    l.report()
        .notes
        .iter()
        .any(|n| matches!(n.kind, NoteKind::UnsupportedProperty { .. }))
}

/// C2: image index 55 selects run `fix` of the frame's hi-speed fix, 0
/// Classic and Floating 1 to 4, on play frames and on loading frames; it
/// is kept, read from the live presentation where the source reads the
/// stored configuration (D4).
#[test]
fn index_55_selects_the_frames_hispeed_fix() {
    let l = group(55);
    assert!(!dropped(&l), "{:?}", l.report().notes);
    assert!(has(l.report(), NoteKind::Deviation(DeviationId::D4)));
    let mut d = l.drawn();
    let mut loader: Loader = group(55).loading();
    for fix in 0..=4 {
        d.readouts = Some(fixed(fix));
        let q = d.frame(1_000, &[]);
        assert_eq!(frames(&d.uvs, &q), [2 * usize::from(fix)], "play {fix}");
        loader.fix_hispeed = fix;
        let q = loader.frame(100);
        assert_eq!(
            frames(&loader.uvs, &q),
            [2 * usize::from(fix)],
            "loading {fix}"
        );
    }
}

/// C4: image index 75, the auto-adjust setting, selects run 0 (off),
/// whatever the frame, with nothing dropped and no deviation.
#[test]
fn index_75_selects_off() {
    let l = group(75);
    assert!(l.report().notes.is_empty(), "{:?}", l.report().notes);
    let mut d = l.drawn();
    for fix in [0, 4] {
        d.readouts = Some(fixed(fix));
        let q = d.frame(1_000, &[]);
        assert_eq!(frames(&d.uvs, &q), [0], "{fix}");
    }
}

/// An EX score of one cell at y = 0, then number `prop` of four cells at
/// y = 50.
fn offset_number(prop: &str) -> common::Loaded {
    import_body(&format!(
        r#""value": [
             {{"id": "ex", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 1, "ref": 71}},
             {{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4, {prop}}}],
           "destination": [
             {{"id": "ex", "dst": [{{"x": 0, "y": 0, "w": 12, "h": 20}}]}},
             {{"id": "v", "dst": [{{"x": 0, "y": 50, "w": 12, "h": 20}}]}}]"#
    ))
}

/// The digits of the number at y = 50 (1 010 on the canvas, which counts
/// from the top) among `quads`, read through `glyph`.
fn digits_of(
    glyph: &std::collections::HashMap<[u32; 4], char>,
    quads: &[skin_model::Quad],
) -> String {
    let mut cells: Vec<_> = quads
        .iter()
        .filter(|q| round(q.rect)[1] == 1_010)
        .map(|q| (round(q.rect)[0], glyph[&q.uv.map(f32::to_bits)]))
        .collect();
    cells.sort_unstable();
    cells.into_iter().map(|(_, c)| c).collect()
}

/// C3: number 12 is the display offset in whole ms, truncated toward
/// zero as the selection screen shows it, live after an offset key (the
/// next frame of the same play reads the new offset), on loading frames
/// too; it is kept with no note, as the source shows its stored offset.
#[test]
fn number_12_shows_the_display_offset() {
    for prop in [r#""ref": 12"#, r#""value": "notesdisplaytiming""#] {
        let l = offset_number(prop);
        assert!(
            l.report().notes.is_empty(),
            "{prop}: {:?}",
            l.report().notes
        );
        let mut d = l.drawn();
        let glyph = d.ex_glyphs(1_000);
        for (us, want) in [(0, "0"), (1_000, "1"), (1_500, "1"), (15_600, "15")] {
            d.display_offset = TimeUs(us);
            assert_eq!(digits_of(&glyph, &d.frame(1_000, &[])), want, "{prop} {us}");
        }
        let mut loader: Loader = offset_number(prop).loading();
        loader.display_offset = TimeUs(15_600);
        assert_eq!(digits_of(&glyph, &loader.frame(100)), "15", "{prop}");
    }
}

/// An image drawn `w` wide under each op of `ops`.
fn drawn_under(ops: &[(String, i32)]) -> common::Loaded {
    let dsts: Vec<String> = ops
        .iter()
        .map(|(op, w)| format!(r#"{{"id": "i", "op": [{op}], "dst": [{{"w": {w}, "h": 10}}]}}"#))
        .collect();
    import_body(&format!(
        r#""image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
           "destination": [{}]"#,
        dsts.join(", ")
    ))
}

fn widths(rects: &[[i32; 4]]) -> Vec<i32> {
    let mut w: Vec<i32> = rects.iter().map(|r| r[2]).collect();
    w.sort_unstable();
    w
}

/// Options 84 (a replay playing), 280-283 and 289 (the course's stage)
/// and 400 (constant mode), by id and by name.
const CONSTANTS: [(i32, &str); 7] = [
    (84, "replay_playing"),
    (280, "course_stage1"),
    (281, "course_stage2"),
    (282, "course_stage3"),
    (283, "course_stage4"),
    (289, "course_stage_final"),
    (400, "constant"),
];

/// C4: each constant option is false and its negation true, by id and by
/// name, with nothing dropped; while loading too.
#[test]
fn play_constants_fold_false() {
    for (id, name) in CONSTANTS {
        for (on, off) in [
            (format!("{id}"), format!("-{id}")),
            (format!(r#""{name}""#), format!(r#""!{name}""#)),
        ] {
            let ops = [(on.clone(), 10), (off, 11)];
            let l = drawn_under(&ops);
            assert!(!dropped(&l), "{on}: {:?}", l.report().notes);
            assert_eq!(widths(&l.drawn().rects(1_000)), [11], "{on}");
            let mut loader: Loader = drawn_under(&ops).loading();
            assert_eq!(widths(&loader.rects(100)), [11], "{on} loading");
        }
    }
}

/// C4: text 1003, the table's level and name, is empty on play: nothing
/// draws and nothing is dropped, where 1002 stays unsupported.
#[test]
fn text_1003_is_empty() {
    let text = |r: i32| {
        import_body(&format!(
            r#""font": [{{"id": "f", "path": "f.ttf"}}],
               "text": [{{"id": "t", "font": "f", "size": 20, "ref": {r}}}],
               "destination": [{{"id": "t", "dst": [{{"w": 100, "h": 20}}]}}]"#
        ))
    };
    let l = text(1_003);
    assert!(!dropped(&l), "{:?}", l.report().notes);
    assert!(has(l.report(), NoteKind::Ignored { field: "value" }));
    assert!(l.drawn().rects(1_000).is_empty());
    let l = text(1_002);
    assert!(l.report().notes.iter().any(|n| matches!(
        n.kind,
        NoteKind::UnsupportedProperty {
            id: Some(1_002),
            ..
        }
    )));
}

/// A Lua play skin of image `i` under each `draw` callback body of
/// `draws`, destination `k` drawn `10 + k` wide.
fn lua_skin(draws: &[String]) -> (tempfile::TempDir, skin_beatoraja::LuaHeader) {
    let d = tempfile::tempdir().unwrap();
    let dsts: Vec<String> = draws
        .iter()
        .enumerate()
        .map(|(k, draw)| {
            format!(
                "{{ id = \"i\", draw = function() {draw} end, dst = {{ {{ w = {}, h = 10 }} }} }}",
                10 + k
            )
        })
        .collect();
    let lua = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 8, h = 8 }} }}\n\
         skin.destination = {{ {} }}\n\
         return skin\n",
        dsts.join(",\n")
    );
    write(d.path(), "s.luaskin", lua.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    let h = read_lua_header(d.path(), "s.luaskin", &Cancel::new()).unwrap();
    (d, h)
}

fn lua_import(h: &skin_beatoraja::LuaHeader) -> LuaImported {
    common::lua_play(h, &Config::default(), &Cancel::new()).unwrap()
}

/// C5: a Lua play callback's `event_index(55)` lowers and reads C2's value
/// over the five policies.
#[test]
fn a_lua_callback_reads_index_55() {
    let draws: Vec<String> = (0..5)
        .map(|k| format!("return ms.event_index(55) == {k}"))
        .collect();
    let (_d, h) = lua_skin(&draws);
    let i = lua_import(&h);
    assert_eq!(i.callbacks, [Disposition::Lowered; 5]);
    let mut p = Drawn::new(i.imported.skin, 10);
    for fix in 0..=4 {
        p.readouts = Some(fixed(fix));
        assert_eq!(widths(&p.rects(1_000)), [10 + i32::from(fix)], "{fix}");
    }
}

/// C3, C4: a Lua play callback's `number(12)` reads the offset truncated
/// toward zero (−15 600 µs is −15, not a floor's −16) and live; the
/// constant options fold false and their negations true.
#[test]
fn lua_callbacks_read_the_offset_and_the_constants() {
    let draws = [
        "return ms.number(12) == -15",
        "return ms.number(12) == -16",
        "return ms.number(12) == 1",
    ]
    .map(str::to_owned);
    let (_d, h) = lua_skin(&draws);
    let i = lua_import(&h);
    assert_eq!(i.callbacks, [Disposition::Lowered; 3]);
    let mut p = Drawn::new(i.imported.skin, 10);
    p.display_offset = TimeUs(-15_600);
    assert_eq!(widths(&p.rects(1_000)), [10]);
    p.display_offset = TimeUs(1_500);
    assert_eq!(widths(&p.rects(1_100)), [12]);

    let draws: Vec<String> = CONSTANTS
        .iter()
        .flat_map(|(id, _)| {
            [
                format!("return ms.option({id})"),
                format!("return ms.option(-{id})"),
            ]
        })
        .collect();
    let (_d, h) = lua_skin(&draws);
    let i = lua_import(&h);
    assert!(
        i.callbacks.iter().all(|c| *c == Disposition::Lowered),
        "{:?}",
        i.callbacks
    );
    let mut p = Drawn::new(i.imported.skin, 10);
    let want: Vec<i32> = (0..CONSTANTS.len())
        .map(|k| 11 + 2 * i32::try_from(k).unwrap())
        .collect();
    assert_eq!(widths(&p.rects(1_000)), want);
}

/// C4: the configured pass reads the constant options as false on play,
/// as their callbacks do, so a skin choosing its parts by them imports.
#[test]
fn the_configured_pass_reads_the_constants_false() {
    let d = tempfile::tempdir().unwrap();
    let reads: Vec<String> = CONSTANTS
        .iter()
        .map(|(id, _)| format!("ms.option({id}) or not ms.option(-{id})"))
        .collect();
    let lua = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}
         if not skin_config then return skin end
         local ms = require(\"main_state\")
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 8, h = 8 }} }}
         local w = 10
         if {} then w = 20 end
         skin.destination = {{ {{ id = \"i\", dst = {{ {{ w = w, h = 10 }} }} }} }}
         return skin
",
        reads.join(" or ")
    );
    write(d.path(), "s.luaskin", lua.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    let h = read_lua_header(d.path(), "s.luaskin", &Cancel::new()).unwrap();
    let i = lua_import(&h);
    let mut p = Drawn::new(i.imported.skin, 10);
    assert_eq!(widths(&p.rects(1_000)), [10]);
}
