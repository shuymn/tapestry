//! A play slider moved by a Lua `value` (docs/stage3.md §3 "Slider"; the
//! source's `skin/SkinSlider.java:104-120`,
//! `skin/lua/SkinLuaAccessor.java:665-673` at `0.8.9`): the value compiles
//! into the presentation program, converted by `tofloat()`, and moves the
//! paint by its range times the value, unclamped, in any of the four
//! directions; nil and booleans 0, a non-finite value at 0, an unavailable
//! one not drawn, one past the bound not drawn. Its base role is
//! decorative, never SUDDEN+'s paint; a critical read promotes the whole
//! element. Every fixture is a small self-made skin.

mod common;

use common::play::{Drawn, readouts_for};
use skin_beatoraja::lua::{Disposition, Unsupported};
use skin_beatoraja::{
    Cancel, Config, DeviationId, ImportError, LuaImported, LuaWhy, NoteKind, read_lua_header,
};
use skin_model::{FrameReadouts, LaneView, Quad};
use tempfile::TempDir;

/// The slider's destination: x 100, top 516 in reference pixels (the
/// skin's y 500, 64 tall, reflected), 64×64.
const X: f32 = 100.0;
const TOP: f32 = 516.0;
/// The slider's range, 100 reference pixels in a 1 080-tall skin.
const RANGE: f32 = 100.0;

/// The play's lanes in Lua, as `common::lanes` builds them in JSON: part
/// images of `a.png`, eight lanes from y 200, 600 tall, and their notes'
/// destination.
fn lanes() -> (String, String, String) {
    let eight = |n: &str| format!("{{ {} }}", vec![format!("\"{n}\""); 8].join(", "));
    let images = "{ id = \"nt\", src = \"a\", x = 0, y = 0, w = 8, h = 20 },\n\
        { id = \"ls\", src = \"a\", x = 8, y = 0, w = 8, h = 20 },\n\
        { id = \"le\", src = \"a\", x = 16, y = 0, w = 8, h = 20 },\n\
        { id = \"lb\", src = \"a\", x = 24, y = 0, w = 8, h = 20 },\n\
        { id = \"la\", src = \"a\", x = 32, y = 0, w = 8, h = 20 },\n\
        { id = \"mi\", src = \"a\", x = 40, y = 0, w = 8, h = 20 },\n\
        { id = \"cover\", src = \"a\", x = 0, y = 24, w = 16, h = 16 },"
        .to_owned();
    let mut rects: Vec<String> = (0..7)
        .map(|i| format!("{{ x = {}, y = 200, w = 50, h = 600 }}", 100 + 50 * i))
        .collect();
    rects.push("{ x = 20, y = 200, w = 80, h = 600 }".to_owned());
    let note = format!(
        "skin.note = {{ id = \"notes\", note = {n}, lnstart = {s}, lnend = {e}, lnbody = {b},\n\
           lnactive = {a}, hcnstart = {s}, hcnend = {e}, hcnbody = {b}, hcnactive = {a},\n\
           hcndamage = {b}, hcnreactive = {a}, mine = {m}, dst = {{ {r} }} }}",
        n = eight("nt"),
        s = eight("ls"),
        e = eight("le"),
        b = eight("lb"),
        a = eight("la"),
        m = eight("mi"),
        r = rects.join(", "),
    );
    let dst = "{ id = \"notes\", dst = { { x = 0, y = 0, w = 0, h = 0 } } },".to_owned();
    (images, note, dst)
}

/// A play skin whose slider `s` (64×64 of `a.png`, angle `angle`, range
/// 100, `fields`) is moved by `value = function() <body> end` and drawn
/// at x 100, y 500 with `dst` fields; `lanes` adds the play's lanes and
/// their notes, drawn before it.
fn skin(angle: i32, body: &str, fields: &str, dst: &str, with_lanes: bool) -> String {
    let (images, note, notes) = if with_lanes {
        lanes()
    } else {
        (String::new(), String::new(), String::new())
    };
    format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }}, {{ id = \"t\", path = \"t.png\" }},\n\
           {{ id = \"zz\", path = \"zz.png\" }} }}\n\
         skin.image = {{ {images} }}\n\
         {note}\n\
         skin.slider = {{ {{ id = \"s\", src = \"a\", x = 0, y = 0, w = 64, h = 64, angle = {angle}, range = 100,\n\
           {fields} value = function() {body} end }} }}\n\
         skin.destination = {{ {notes}\n\
           {{ id = \"s\", {dst} dst = {{ {{ x = 100, y = 500, w = 64, h = 64 }} }} }} }}\n\
         return skin\n"
    )
}

/// The folder of `lua`, with a 64×64 `a.png` and a 1×1 `t.png`.
fn folder(lua: &str) -> TempDir {
    let d = tempfile::tempdir().unwrap();
    common::write(d.path(), "s.luaskin", lua.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    common::png(d.path(), "t.png", 1, 1);
    d
}

/// An imported skin with the folder its files stay in while it is drawn.
struct Imp(#[allow(dead_code)] TempDir, LuaImported);

impl std::ops::Deref for Imp {
    type Target = LuaImported;
    fn deref(&self) -> &LuaImported {
        &self.1
    }
}

/// `lua` imported from its own folder.
fn load(lua: &str) -> Result<Imp, ImportError> {
    let d = folder(lua);
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel)?;
    let i = common::lua_play(&h, &Config::default(), &cancel)?;
    Ok(Imp(d, i))
}

/// `body` as a down slider without lanes, imported.
fn down(body: &str) -> Imp {
    load(&skin(2, body, "", "", false)).unwrap()
}

/// The slider's one quad at 1 s with `readouts` and the SUDDEN+ amount
/// `cover`, or `None` when nothing is drawn.
fn drawn(i: &LuaImported, cover: u16, readouts: Option<FrameReadouts>) -> Option<Quad> {
    let mut p = Drawn::new(i.imported.skin.clone(), 0);
    p.view = LaneView { cover, ..p.view };
    p.readouts = readouts;
    let q = p.frame(1_000, &[]);
    assert!(q.len() <= 1, "{q:?}");
    q.first().copied()
}

/// The readouts with the current BPM at `bpm`.
fn at_bpm(bpm: u32) -> FrameReadouts {
    FrameReadouts {
        bpm_current_x100: Some(bpm * 100),
        ..readouts_for(LaneView::NONE)
    }
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-3
}

/// The slider's quad origin `[x, y]` at 1 s, with nothing else set.
fn origin(i: &LuaImported) -> [f32; 2] {
    let q = drawn(i, 0, None).expect("drawn");
    [q.rect[0], q.rect[1]]
}

fn compiled(i: &LuaImported) -> bool {
    i.callbacks.contains(&Disposition::Compiled)
}

#[test]
fn a_value_moves_the_paint_by_its_range_in_each_direction() {
    // SUDDEN+'s amount, 250 ‰, as the value: a quarter of the range.
    let body = "return ms.number(14) / 1000";
    for (angle, dx, dy) in [
        (0, 0.0, -25.0),
        (1, 25.0, 0.0),
        (2, 0.0, 25.0),
        (3, -25.0, 0.0),
    ] {
        let i = load(&skin(angle, body, "", "", false)).unwrap();
        assert!(compiled(&i), "{angle}");
        // The SUDDEN+ amount it reads is a cover switch (D4), its only note.
        let notes = &i.imported.report.notes;
        assert!(
            notes
                .iter()
                .all(|n| n.kind == NoteKind::Deviation(DeviationId::D4)),
            "{notes:?}"
        );
        let q = drawn(&i, 250, None).expect("drawn");
        assert!(
            near(q.rect[0], X + dx) && near(q.rect[1], TOP + dy),
            "{angle}: {q:?}"
        );
        // At 0 ‰ the value is 0 and the paint stands at its destination.
        let q = drawn(&i, 0, None).expect("drawn");
        assert!(near(q.rect[0], X) && near(q.rect[1], TOP), "{angle}: {q:?}");
    }
}

#[test]
fn a_negative_value_moves_it_the_other_way_unclamped() {
    // The source's fix-MAIN example: visible area 0.8, BPM 180 over 120.
    let i = down("return 0.8 - 0.8 * ms.number(160) / 120");
    let q = drawn(&i, 0, Some(at_bpm(180))).expect("drawn");
    assert!(near(q.rect[1], TOP - 40.0), "{q:?}");
    // Past 1: twice the range, not clamped to it.
    let i = down("return 2.5");
    assert!(near(origin(&i)[1], TOP + 2.5 * RANGE));
}

#[test]
fn nil_and_booleans_are_zero() {
    for body in [
        "if ms.number(14) > 0 then return 1 end",
        "return false",
        "return true",
    ] {
        let i = down(body);
        assert!(compiled(&i), "{body}");
        assert!(near(origin(&i)[1], TOP), "{body}");
    }
    // The same nil branch taken the other way moves it by the range.
    let i = down("if ms.number(14) > 0 then return 1 end");
    assert!(near(drawn(&i, 500, None).unwrap().rect[1], TOP + RANGE));
}

#[test]
fn a_non_finite_value_draws_at_zero() {
    for body in [
        "return 1 / (ms.number(14) - ms.number(14))",
        "return -1 / (ms.number(14) - ms.number(14))",
        "return (ms.number(14) - ms.number(14)) / (ms.number(14) - ms.number(14))",
        // Finite as a double, an infinity as a float.
        "return 1e300",
    ] {
        let i = down(body);
        assert!(compiled(&i), "{body}");
        assert!(near(origin(&i)[1], TOP), "{body}");
    }
}

#[test]
fn an_unavailable_value_is_not_drawn() {
    let i = down("return ms.number(160) / 100");
    // No tempo yet: the read is unavailable, the slider not drawn.
    assert_eq!(drawn(&i, 0, None), None);
    let q = drawn(&i, 0, Some(at_bpm(50))).expect("drawn");
    assert!(near(q.rect[1], TOP + 0.5 * RANGE), "{q:?}");
}

#[test]
fn a_known_string_is_read_as_the_source_reads_it() {
    for (body, v) in [
        ("return \"0.25\"", 0.25),
        ("return \" 0x10 \"", 16.0),
        ("return \"abc\"", 0.0),
        (
            "if ms.number(14) > 0 then return \"0.5\" end return 0.25",
            0.25,
        ),
    ] {
        let i = down(body);
        assert!(compiled(&i), "{body}");
        assert!(near(origin(&i)[1], TOP + v * RANGE), "{body}");
    }
    let i = down("if ms.number(14) > 0 then return \"0.5\" end return 0.25");
    assert!(near(drawn(&i, 1, None).unwrap().rect[1], TOP + 0.5 * RANGE));
}

/// The output the slider reads: callback 0's float bits.
fn bits(i: &LuaImported) -> u32 {
    let mut p = Drawn::new(i.imported.skin.clone(), 0);
    p.frame(1_000, &[]);
    // The one callback's two outputs are the last fields.
    let fields = p.play.state().fields;
    let [.., v, ok] = fields else {
        panic!("no outputs")
    };
    assert_eq!(*ok, 1);
    #[expect(clippy::cast_sign_loss, reason = "the slot holds the bits")]
    let b = *v as u32;
    b
}

#[test]
fn signed_zero_follows_the_source_s_values() {
    // A numeric −0 is the source's integer zero; a string's keeps its sign;
    // a tiny negative double underflows to −0 as `d2f` does.
    let i = down("local z = -0.0 return z");
    let captured = load(
        &skin(2, "return z", "", "", false)
            .replace("local ms = require", "local z = -0.0\nlocal ms = require"),
    )
    .unwrap();
    assert_eq!(bits(&captured), 0, "captured −0");
    assert_eq!(bits(&i), 0, "a local −0");
    assert_eq!(bits(&down("return \"-0.0\"")), (-0.0_f32).to_bits());
    assert_eq!(bits(&down("return -1e-300")), (-0.0_f32).to_bits());
}

#[test]
fn it_adds_to_the_sudden_follower() {
    let lua = skin(2, "return 0.5", "", "offset = 4,", true);
    let i = load(&lua).unwrap();
    assert!(compiled(&i));
    let mut p = Drawn::new(i.imported.skin.clone(), 0);
    let at = |p: &mut Drawn, cover: u16| {
        p.view = LaneView { cover, ..p.view };
        let q = p.frame(1_000, &[]);
        q.last().unwrap().rect[1]
    };
    let off = at(&mut p, 0);
    assert!(near(off, TOP + 0.5 * RANGE), "{off}");
    let on = at(&mut p, 300);
    // SUDDEN+'s slide at 300 ‰ of the 600-tall lanes, on top of the value.
    assert!(near(on - off, 180.0), "{off} {on}");
}

#[test]
fn past_the_bound_it_is_not_drawn() {
    // 2^40 over the range of 100: just inside and just past the bound.
    let inside = down("return 10995116277.76 * 0.999");
    assert!(drawn(&inside, 0, None).is_some());
    for body in [
        "return 10995116277.76 * 1.001",
        "return 3e38",
        "return -3e38",
    ] {
        assert_eq!(drawn(&down(body), 0, None), None, "{body}");
    }
}

#[test]
fn a_compacted_tall_cut_past_the_bound_is_not_drawn_on_either_axis() {
    // A 1×1 file cut far past its edge keeps a compact one-texel cell at
    // the cut's native size (stretch 9): retained, then moved past the
    // bound, not drawn, whichever axis it moves on.
    for (angle, cut) in [
        (2, "x = 0, y = 1, w = 1, h = 2147483646"),
        (1, "x = 1, y = 0, w = 2147483646, h = 1"),
    ] {
        let lua = skin(
            angle,
            "return ms.number(14) * 1e10",
            "",
            "stretch = 9,",
            false,
        )
        .replace(
            "src = \"a\", x = 0, y = 0, w = 64, h = 64",
            &format!("src = \"t\", {cut}"),
        );
        let i = load(&lua).unwrap();
        assert!(compiled(&i), "{angle}");
        let mut p = Drawn::new(i.imported.skin.clone(), 0);
        p.target = [1, 1];
        assert_eq!(
            p.frame(1_000, &[]).len(),
            1,
            "{angle}: retained and drawn at 0"
        );
        p.view = LaneView { cover: 1, ..p.view };
        // 1e7 × 100: inside the bound, drawn.
        assert_eq!(p.frame(1_000, &[]).len(), 1, "{angle}");
        // 1e10 × 100 × 1 000: past it, not drawn.
        p.view = LaneView {
            cover: 1_000,
            ..p.view
        };
        assert!(p.frame(1_000, &[]).is_empty(), "{angle}");
    }
}

#[test]
fn ignored_fields_beside_a_value_are_noted_and_type_four_is_not_sudden_s() {
    let lua = skin(
        2,
        "return 0.5",
        "type = 4, isRefNum = true, event = 1,",
        "",
        false,
    );
    let i = load(&lua).unwrap();
    for field in ["type", "isRefNum", "event"] {
        assert!(
            common::has(&i.imported.report, NoteKind::Ignored { field }),
            "{field}"
        );
    }
    // The value's position, not SUDDEN+'s share.
    let q = drawn(&i, 300, None).unwrap();
    assert!(near(q.rect[1], TOP + 0.5 * RANGE), "{q:?}");
}

#[test]
fn an_uncompilable_value_drops_the_slider_with_its_cause() {
    let why = Unsupported::Unmapped { api: 6, id: 55 };
    let i = down("return ms.event_index(55)");
    let Disposition::Replaced(got) = i.callbacks[0] else {
        panic!("{:?}", i.callbacks)
    };
    assert!(
        matches!(got, Unsupported::Unmapped { id: 55, .. }),
        "{got:?} {why:?}"
    );
    assert!(common::has(
        &i.imported.report,
        NoteKind::Callback {
            field: "value",
            why: got
        }
    ));
    assert_eq!(drawn(&i, 0, None), None);
    // Over the lanes, a decorative paint still: the skin imports.
    let lua = skin(2, "return ms.event_index(55)", "", "offset = 4,", true);
    assert!(load(&lua).is_ok());
    // A table, and a Lua error, as the residual refuses them.
    for body in ["return {}", "return nil + 1"] {
        let i = down(body);
        assert!(!compiled(&i), "{body}");
        assert!(matches!(i.callbacks[0], Disposition::Replaced(_)), "{body}");
        assert_eq!(drawn(&i, 0, None), None, "{body}");
    }
}

#[test]
fn a_decorative_slider_dropped_after_its_value_compiled_leaves_no_callback() {
    for dst in ["blend = 9,", ""] {
        let mut lua = skin(2, "return 0.5", "", dst, false);
        if dst.is_empty() {
            lua = lua.replace("src = \"a\", x = 0", "src = \"zz\", x = 0");
        }
        let i = load(&lua).unwrap();
        assert!(!compiled(&i), "{dst}");
        assert!(
            Drawn::new(i.imported.skin.clone(), 0)
                .frame(1_000, &[])
                .is_empty()
        );
    }
}

#[test]
fn a_critical_read_promotes_the_whole_slider() {
    // The EX score: compiled, retained, its role critical.
    let i = down("return ms.number(71) / 100");
    assert!(compiled(&i));
    assert!(drawn(&i, 0, None).is_some());
    // With an unsupported blend it refuses, where a decorative one drops.
    let lua = skin(2, "return ms.number(71) / 100", "", "blend = 9,", false);
    assert!(matches!(load(&lua), Err(ImportError::Critical { .. })));
    // An untimed settling track is held at its final pose (D20).
    let lua = skin(2, "return ms.number(71) / 100", "", "", false).replace(
        "dst = { { x = 100, y = 500, w = 64, h = 64 } }",
        "loop = 500, dst = { { time = 0, x = 100, y = 0, w = 64, h = 64 }, { time = 500, y = 500 } }",
    );
    let i = load(&lua).unwrap();
    assert!(common::has(
        &i.imported.report,
        NoteKind::Deviation(DeviationId::D20)
    ));
    // Drawn at its final pose from the start, long before 500 ms.
    let mut p = Drawn::new(i.imported.skin.clone(), 0);
    let q = p.frame(100, &[]);
    assert_eq!(q.len(), 1, "{q:?}");
    assert!(near(q[0].rect[1], TOP), "{q:?}");
    // Not compiled, it refuses; so does an unresolvable callee.
    for body in [
        "return ms.number(71) + ms.event_index(55)",
        "return ms.number(71) + undefined_helper()",
    ] {
        match load(&skin(2, body, "", "", false)) {
            Err(ImportError::Lua {
                why: LuaWhy::Critical { field: "value", .. },
                ..
            }) => {}
            other => panic!("{body}: {:?}", other.map(|i| i.callbacks.clone())),
        }
    }
    let lua = skin(2, "return undefined_helper()", "", "", false);
    assert!(
        matches!(
            load(&lua),
            Err(ImportError::Lua {
                why: LuaWhy::Critical { field: "value", .. },
                ..
            })
        ),
        "an unresolved callee in a decorative value refuses"
    );
}

#[test]
fn as_failure_presentation_an_uncompilable_value_refuses() {
    let lua = skin(2, "return ms.event_index(55)", "", "timer = 3,", false);
    assert!(load(&lua).is_err());
}

#[test]
fn a_value_slider_s_keys_are_charged_once() {
    // One key, as written: the same total as without the slider's value
    // (a progress slider, charged once too).
    let keys = |lua: &str| load(lua).unwrap().imported.report.usage.keys;
    let value = keys(&skin(2, "return 0.5", "", "", false));
    let progress = keys(
        &skin(2, "return 0.5", "", "", false)
            .replace(" value = function() return 0.5 end", " type = 6"),
    );
    assert_eq!(value, 1);
    assert_eq!(value, progress);
}

/// D24: past the bound a value slider is not drawn, even where its own
/// geometry would still span the window, as a mirrored stretch-8 paint of
/// a compacted tall cut under a −10⁶ destination scale does in the source.
#[test]
fn past_the_bound_a_mirrored_stretch_eight_paint_is_not_drawn_d24() {
    for (angle, cut, dst) in [
        (
            2,
            "x = 0, y = 1, w = 1, h = 2147483646",
            "x = 1000000, y = 500, w = -1000000, h = 64",
        ),
        (
            1,
            "x = 1, y = 0, w = 2147483646, h = 1",
            "x = 100, y = 1000000, w = 64, h = -1000000",
        ),
    ] {
        let lua = skin(
            angle,
            "return ms.number(14) * 1e8",
            "",
            "stretch = 8,",
            false,
        )
        .replace(
            "src = \"a\", x = 0, y = 0, w = 64, h = 64",
            &format!("src = \"t\", {cut}"),
        )
        .replace("x = 100, y = 500, w = 64, h = 64", dst);
        let i = load(&lua).unwrap();
        assert!(compiled(&i), "{angle}");
        // Retained and drawn while the value is 0.
        assert!(drawn(&i, 0, None).is_some(), "{angle}");
        // 2e10 × 100: past the bound, not drawn (D24).
        assert_eq!(drawn(&i, 200, None), None, "{angle}");
    }
}
