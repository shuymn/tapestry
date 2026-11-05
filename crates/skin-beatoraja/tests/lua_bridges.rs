//! The bridges over state the model already holds (docs/stage3.md §3,
//! S7-06f), through the real import and frames: the judgment-count values
//! and conditions, `main_state.judge`, and an image set's value. Every
//! fixture is a small self-made skin.

mod common;

use common::play::{Drawn, Loader, context, round};
use skin_beatoraja::lua::{Disposition, Unsupported};
use skin_beatoraja::{Cancel, Config, ImportError, LuaImported, LuaWhy, NoteKind, read_lua_header};
use skin_model::{GaugeDesc, GaugeLook, LoadingGauge, PlayContext, Quad};
use tempfile::TempDir;

/// A skin folder whose `s.luaskin` runs `body` with `ms` the host module,
/// `skin.source` `a` and images `i0` to `i8`, each a cell of its own.
fn folder(body: &str) -> TempDir {
    let d = tempfile::tempdir().unwrap();
    let entry = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{}}\n\
         for k = 0, 8 do\n\
           skin.image[k + 1] = {{ id = \"i\" .. k, src = \"a\", x = k * 4, y = 0, w = 4, h = 4 }}\n\
         end\n\
         skin.destination = {{}}\n\
         {body}\n\
         return skin\n"
    );
    common::write(d.path(), "s.luaskin", entry.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    d
}

fn import(d: &TempDir) -> Result<LuaImported, ImportError> {
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel)?;
    common::lua_play(&h, &Config::default(), &cancel)
}

/// `body`'s skin in a play of `notes` notes, its images read.
fn drawn(body: &str, notes: u32) -> Drawn {
    let d = folder(body);
    let i = import(&d).unwrap();
    Drawn::new(i.imported.skin, notes)
}

/// `body`'s skin while loading, its images read.
fn loader(body: &str) -> Loader {
    let d = folder(body);
    Loader::new(import(&d).unwrap().imported.skin)
}

/// A tally of `bad` BADs, `poor` scored POORs, split early and late, and
/// `empty` empty POORs not counted as POORs.
fn counts(bad: u32, poor: [u32; 2], empty: u32) -> eval::Tally {
    let mut t = common::play::tally(0, 0);
    t.counts.bad = bad;
    t.early.bad = bad;
    t.counts.poor = poor[0] + poor[1];
    t.early.poor = poor[0];
    t.late.poor = poor[1];
    t.empty_poor = empty;
    t.empty_poor_early = empty;
    t
}

/// Images drawn `10 + k` wide while `expr == k`, for each `k` to 12, so a
/// frame's width reads `expr`'s value.
fn reading(expr: &str) -> String {
    (0..=12)
        .map(|k| {
            format!(
                "table.insert(skin.destination, {{ id = \"i0\",\n\
                   draw = function() return {expr} == {k} end,\n\
                   dst = {{ {{ x = 0, y = 0, w = {w}, h = 10 }} }} }})\n",
                w = 10 + k
            )
        })
        .collect()
}

/// What `reading` reads in `d`'s frame.
fn read(d: &mut Drawn) -> Vec<i32> {
    d.rects(1_000).iter().map(|r| r[2] - 10).collect()
}

/// `expr`'s value over `tally`, in a play that counts empty POORs when
/// `counted`.
fn value_of(expr: &str, tally: eval::Tally, counted: bool) -> Vec<i32> {
    let mut d = drawn(&reading(expr), 20);
    if counted {
        let ctx = PlayContext {
            empty_poor_counted: true,
            ..context()
        };
        d.play = d.skin.prepare(&d.chart, &ctx).unwrap();
    }
    d.tally = tally;
    read(&mut d)
}

/// A number of `prop` drawn, as its digits: value 0 shows one.
fn digits(prop: &str, tally: eval::Tally) -> usize {
    let mut d = drawn(
        &format!(
            "skin.value = {{ {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8, {prop} }} }}\n\
             table.insert(skin.destination, {{ id = \"v\", dst = {{ {{ x = 0, y = 100, w = 6, h = 6 }} }} }})"
        ),
        20,
    );
    d.tally = tally;
    d.frame(1_000, &[]).len()
}

/// 425 is BAD and the scored POORs, 427 those and the empty POORs, by
/// `ref`, by `value` and read in a callback.
#[test]
fn the_combo_break_counts_sum_the_scored_judgments() {
    let tally = || counts(3, [4, 5], 90);
    assert_eq!(digits("ref = 425", tally()), 2, "12");
    assert_eq!(digits("value = 425", tally()), 2, "12");
    assert_eq!(digits("ref = 427", tally()), 3, "102");
    assert_eq!(value_of("ms.number(425)", tally(), false), [12]);
    assert_eq!(
        value_of("ms.number(427)", counts(1, [1, 1], 2), false),
        [5],
        "each term once"
    );
    assert_eq!(digits("ref = 425", counts(0, [0, 0], 5)), 1, "0");
}

/// A sum's range is its terms': arithmetic that could leave `i32` over it
/// is refused, where the same factor stays inside over one count.
#[test]
fn a_sums_range_bounds_a_callbacks_arithmetic() {
    let draw = |expr: &str| {
        let dir = folder(&format!(
            "table.insert(skin.destination, {{ id = \"i0\", draw = function() return {expr} > 0 end,\n\
               dst = {{ {{ w = 10, h = 10 }} }} }})"
        ));
        import(&dir).unwrap().callbacks
    };
    assert_eq!(draw("ms.number(113) * 100"), [Disposition::Lowered]);
    for expr in [
        "ms.number(425) * 100",
        "ms.number(427) * 100",
        "ms.judge(4) * 200",
    ] {
        assert_eq!(
            draw(expr),
            [Disposition::Replaced(Unsupported::Range)],
            "{expr}"
        );
    }
}

/// One empty POOR alone under a preset that counts it as a POOR: `ref` 114
/// keeps the preset's count, the rest the scored POORs.
#[test]
fn a_counted_empty_poor_is_no_scored_poor() {
    let empty = || {
        let mut t = counts(0, [0, 0], 1);
        t.counts.poor = 1;
        t
    };
    assert_eq!(value_of("ms.number(114)", empty(), true), [1]);
    assert_eq!(value_of("ms.number(425)", empty(), true), [0]);
    assert_eq!(value_of("ms.number(427)", empty(), true), [1]);
    assert_eq!(value_of("ms.judge(4)", empty(), true), [0]);
    assert_eq!(value_of("ms.judge(5)", empty(), true), [1]);
    let mut d = drawn(
        "table.insert(skin.destination, { id = \"i0\", op = { 2245 }, dst = { { w = 10, h = 10 } } })",
        20,
    );
    let ctx = PlayContext {
        empty_poor_counted: true,
        ..context()
    };
    d.play = d.skin.prepare(&d.chart, &ctx).unwrap();
    d.tally = empty();
    assert!(d.rects(1_000).is_empty(), "2245 false");
}

/// Each condition 2242 to 2245 over its own count, and its negation.
#[test]
fn judgment_conditions_read_their_counts() {
    for (op, set) in [
        (2_242, (|t| t.counts.great = 1) as fn(&mut eval::Tally)),
        (2_243, |t| t.counts.good = 1),
        (2_244, |t| t.counts.bad = 1),
        (2_245, |t| t.early.poor = 1),
        (2_245, |t| t.late.poor = 1),
    ] {
        let mut d = drawn(
            &format!(
                "table.insert(skin.destination, {{ id = \"i0\", op = {{ {op} }}, dst = {{ {{ w = 10, h = 10 }} }} }})\n\
                 table.insert(skin.destination, {{ id = \"i0\", op = {{ -{op} }}, dst = {{ {{ w = 11, h = 10 }} }} }})"
            ),
            20,
        );
        d.tally = counts(0, [0, 0], 0);
        assert_eq!(d.rects(1_000)[0][2], 11, "{op} at 0");
        set(&mut d.tally);
        d.tally.counts.poor = d.tally.early.poor + d.tally.late.poor;
        assert_eq!(d.rects(1_000)[0][2], 10, "{op} above 0");
    }
}

/// Callbacks written as the surveyed skins write them: a `finish` image on
/// the last note's timer that a miss turns on, and a missed line.
#[test]
fn callbacks_lower_the_count_bridges() {
    let dir = folder(
        "table.insert(skin.destination, { id = \"i0\", timer = 143, loop = -1, draw = function()\n\
           local cb = ms.number(425) return cb > 0 end,\n\
           dst = { { time = 0, x = 0, y = 0, w = 10, h = 10 }, { time = 100000 } } })\n\
         local bd, pr = 2244, 2245\n\
         table.insert(skin.destination, { id = \"i1\", draw = function()\n\
           return ms.option(bd) or ms.option(pr) end,\n\
           dst = { { x = 0, y = 0, w = 11, h = 10 } } })",
    );
    let i = import(&dir).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered, Disposition::Lowered]);
    let mut d = Drawn::new(i.imported.skin, 20);
    let ctx = PlayContext {
        last_note_end: bms_model::TimeUs(2_000_000),
        ..context()
    };
    d.play = d.skin.prepare(&d.chart, &ctx).unwrap();
    d.tally = counts(0, [0, 0], 3);
    assert!(d.rects(30_000).is_empty(), "empty POORs alone");
    d.tally = counts(0, [0, 1], 0);
    let shown: Vec<_> = d.rects(30_000).iter().map(|r| r[2]).collect();
    assert_eq!(shown, [10, 11]);
}

/// `judge(n)` by index, 0 for an index with no count.
#[test]
fn judge_reads_the_counts_by_index() {
    let mut t = counts(4, [2, 3], 6);
    t.counts.perfect = 1;
    t.counts.great = 2;
    t.counts.good = 3;
    for (n, want) in [
        (0, 1),
        (1, 2),
        (2, 3),
        (3, 4),
        (4, 5),
        (5, 6),
        (6, 0),
        (-1, 0),
    ] {
        assert_eq!(
            value_of(&format!("ms.judge({n})"), t.clone(), false),
            [want],
            "{n}"
        );
    }
    let left =
        "ms.number(106) - (ms.judge(0) + ms.judge(1) + ms.judge(2) + ms.judge(3) + ms.judge(4))";
    assert_eq!(value_of(left, t, false), [5], "the shape of notes left");
    // An argument not a known integer: no coercion.
    for arg in ["1.5", "\"1\"", "ms.number(110)"] {
        let i = import(&folder(&format!(
            "table.insert(skin.destination, {{ id = \"i0\", draw = function() return ms.judge({arg}) > 0 end,\n\
               dst = {{ {{ w = 10, h = 10 }} }} }})"
        )))
        .unwrap();
        assert!(
            matches!(i.callbacks[..], [Disposition::Replaced(_)]),
            "{arg}: {:?}",
            i.callbacks
        );
    }
}

/// An image set of `value` drawn at x 0, and each of its images apart, to
/// tell entries by their uv.
fn image_set(value: &str, more: &str) -> String {
    let images: Vec<String> = (0..9).map(|k| format!("\"i{k}\"")).collect();
    let mut s = format!(
        "skin.imageset = {{ {{ id = \"s\", ref = 40, images = {{ {} }}, value = {value} }} }}\n\
         table.insert(skin.destination, {{ id = \"s\", {more} dst = {{ {{ x = 0, y = 0, w = 10, h = 10 }} }} }})\n",
        images.join(", ")
    );
    for k in 0..9 {
        s.push_str(&format!(
            "table.insert(skin.destination, {{ id = \"i{k}\", dst = {{ {{ x = {x}, y = 0, w = 4, h = 4 }} }} }})\n",
            x = 1_000 + 10 * k
        ));
    }
    s
}

/// The entry the set's quad shows among `quads`, if it drew.
fn entry(quads: &[Quad]) -> Option<i32> {
    let set = quads.iter().find(|q| round(q.rect)[2] == 10)?;
    quads
        .iter()
        .find(|q| round(q.rect)[2] == 4 && q.uv == set.uv && q.layer == set.layer)
        .map(|q| (round(q.rect)[0] - 1_000) / 10)
}

/// A bare `gauge_type()` chooses by the shown gauge's look, with its D4
/// note.
#[test]
fn an_image_set_chooses_by_gauge_type() {
    let d = folder(&image_set("function() return ms.gauge_type() end", ""));
    let i = import(&d).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered]);
    assert!(
        i.imported
            .report
            .notes
            .iter()
            .any(|n| matches!(n.kind, NoteKind::Deviation(skin_beatoraja::DeviationId::D4)))
    );
    for (k, look) in GaugeLook::ALL.into_iter().enumerate() {
        let mut p = Drawn::with_look(import(&d).unwrap().imported.skin, 10, look);
        assert_eq!(
            entry(&p.frame(1_000, &[])),
            Some(i32::try_from(k).unwrap()),
            "{look:?}"
        );
    }
}

/// A numeric callback chooses by its value, under its read's guard, and a
/// past-the-end value entry 0 and a negative none, as the source's image.
#[test]
fn an_image_set_chooses_by_its_value() {
    let mut p = drawn(
        &image_set(
            "function() if ms.number(110) > 0 then return 1 else return 0 end end",
            "",
        ),
        10,
    );
    assert_eq!(entry(&p.frame(1_000, &[])), Some(0));
    p.tally.counts.perfect = 1;
    assert_eq!(entry(&p.frame(1_000, &[])), Some(1));

    let dir = folder(&image_set("function() return ms.number(96) end", ""));
    let chart = common::play::chart(10);
    for (level, want) in [
        (None, None),
        (Some(3), Some(3)),
        (Some(40), Some(0)),
        (Some(-1), None),
    ] {
        let skin = import(&dir).unwrap().imported.skin;
        let mut p = Drawn::over_level(skin, &chart, level);
        assert_eq!(entry(&p.frame(1_000, &[])), want, "{level:?}");
    }
}

/// The part a selector's reads show is the element's role through the
/// whole mapping: a valid one lowers, an unsupported dependency refuses
/// it, where a decorative selector's drops with a note.
#[test]
fn an_image_sets_reads_carry_its_role_through_the_mapping() {
    let critical = "function() return ms.number(101) end";
    let dir = folder(&image_set(critical, ""));
    let i = import(&dir).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered]);
    let mut p = Drawn::new(i.imported.skin, 10);
    p.tally.ex_score = 2;
    assert_eq!(entry(&p.frame(1_000, &[])), Some(2));

    let unsupported = "op = { 289 },";
    assert!(matches!(
        import(&folder(&image_set(critical, unsupported))),
        Err(ImportError::Critical { .. })
    ));
    let mut p = drawn(
        &image_set("function() return ms.number(110) end", unsupported),
        10,
    );
    assert_eq!(entry(&p.frame(1_000, &[])), None, "dropped");
}

/// A value that does not lower follows its role, never `ref`: its reason
/// kept, whether decorative or critical. `gauge_type()` in arithmetic is
/// still a type the expressions do not take.
#[test]
fn an_image_set_value_that_does_not_lower_follows_its_role() {
    for (value, why) in [
        (
            "function() return ms.gauge_type() + 1 end",
            Unsupported::Type,
        ),
        ("function() return \"x\" end", Unsupported::Type),
        (
            "function() return ms.number(426) end",
            Unsupported::Unmapped { api: 1, id: 426 },
        ),
    ] {
        let dir = folder(&image_set(value, ""));
        let i = import(&dir).unwrap();
        assert_eq!(i.callbacks, [Disposition::Replaced(why)], "{value}");
        let mut p = Drawn::new(i.imported.skin, 10);
        assert_eq!(entry(&p.frame(1_000, &[])), None, "no ref: {value}");
    }
    match import(&folder(&image_set(
        "function() return ms.number(101) + ms.number(426) end",
        "",
    ))) {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    field: "value",
                    why: Unsupported::Unmapped { id: 426, .. },
                },
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
}

/// While loading no program reading a count is evaluated, its negation
/// included; a play frame at zero counts draws that negation. The gauge's
/// selector follows the descriptor.
#[test]
fn loading_reads_no_counts_and_the_selector_follows_the_descriptor() {
    let body = "table.insert(skin.destination, { id = \"i0\", op = { -2244 }, dst = { { w = 10, h = 10 } } })\n\
         table.insert(skin.destination, { id = \"i0\", draw = function() return ms.number(425) == 0 end,\n\
           dst = { { w = 11, h = 10 } } })\n\
         skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8, ref = 425 } }\n\
         table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 100, w = 6, h = 6 } } })";
    let mut l = loader(body);
    assert!(l.rects(100).is_empty(), "while loading");
    let mut d = drawn(body, 10);
    d.tally = counts(0, [0, 0], 0);
    let widths: Vec<_> = d.rects(1_000).iter().map(|r| r[2]).collect();
    assert_eq!(widths, [10, 11, 6], "a play frame at zero counts");

    let mut l = loader(&image_set("function() return ms.gauge_type() end", ""));
    assert_eq!(entry(&l.frame(100)), Some(2), "the normal look");
    let desc = GaugeDesc {
        look: GaugeLook::Hazard,
        ..common::play::GAUGES[0]
    };
    l.gauge = Some(LoadingGauge::of(0, &desc));
    assert_eq!(entry(&l.frame(200)), Some(5));
    l.gauge = None;
    assert_eq!(entry(&l.frame(300)), None, "no descriptor");
}

/// The bridges read each frame's snapshot: its counts, then the zero a
/// rearmed candidate's evaluator starts from, then the new attempt's.
#[test]
fn combo_break_follows_each_snapshot() {
    let mut d = drawn(&reading("ms.number(425)"), 20);
    d.tally = counts(1, [1, 1], 0);
    assert_eq!(read(&mut d), [3]);
    d.play.rearm();
    d.tally = counts(0, [0, 0], 0);
    assert_eq!(read(&mut d), [0]);
    d.tally = counts(1, [0, 1], 0);
    assert_eq!(read(&mut d), [2]);
}
