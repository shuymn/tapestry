//! Import facts through the public entries: [`read_lua_header`], then
//! [`LuaHeader::import`] against a snapshot. Each fixture is a small
//! self-made play skin that reads `main_state.option` while it builds its
//! definition.

mod common;

use common::play::Drawn;
use skin_beatoraja::lua::HOST_APIS;
use skin_beatoraja::{
    Cancel, Config, Fact, FactDeps, FactId, FactRead, FactValue, ImportError, ImportFacts,
    JudgeClass, LuaHeader, LuaImported, LuaOutcome, LuaPending, LuaWhy, read_lua_header,
};
use skin_model::ScreenKind;
use tempfile::TempDir;

/// The line of a skin's `body` first line ([`folder`]).
const BODY_LINE: u32 = 7;

/// A play skin folder whose configured pass runs `body` with `ms` the
/// host module, then draws image `i` (a 64×64 `a.png`) at `x`, which
/// `body` may set.
fn folder(body: &str) -> TempDir {
    let entry = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }} }}\n\
         local x = 0\n\
         {body}\n\
         skin.destination = {{ {{ id = \"i\", dst = {{ {{ x = x, y = 0, w = 64, h = 64 }} }} }} }}\n\
         return skin\n"
    );
    let d = tempfile::tempdir().unwrap();
    common::write(d.path(), "s.luaskin", entry.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    d
}

fn header(d: &TempDir) -> LuaHeader {
    read_lua_header(d.path(), "s.luaskin", &Cancel::new()).unwrap()
}

/// A play snapshot, the play level unavailable.
fn play(autoplay: Fact<bool>, judge_class: Fact<Option<JudgeClass>>) -> ImportFacts {
    ImportFacts {
        scene: ScreenKind::Play,
        autoplay,
        judge_class,
        play_level: Fact::Unavailable,
    }
}

/// A play snapshot with the play level `level`, the rest unavailable.
fn leveled(level: Fact<i32>) -> ImportFacts {
    ImportFacts {
        play_level: level,
        ..ImportFacts::unavailable(ScreenKind::Play)
    }
}

/// Both passes of `d` with the default configuration against `facts`.
fn import(d: &TempDir, facts: &ImportFacts) -> Result<LuaOutcome, ImportError> {
    let h = header(d);
    h.import(&Config::defaults(h.header()), facts, &Cancel::new())
}

#[track_caller]
fn imported(r: Result<LuaOutcome, ImportError>) -> LuaImported {
    match r {
        Ok(LuaOutcome::Imported(i)) => i,
        other => panic!("{other:?}"),
    }
}

#[track_caller]
fn pending(r: Result<LuaOutcome, ImportError>) -> LuaPending {
    match r {
        Ok(LuaOutcome::Pending(p)) => p,
        other => panic!("{other:?}"),
    }
}

/// The Lua refusal of `r`, its file and line.
#[track_caller]
fn located(r: Result<LuaOutcome, ImportError>) -> (LuaWhy, String, u32) {
    match r {
        Err(ImportError::Lua { why, at: Some(at) }) => (why, at.file, at.line),
        other => panic!("{other:?}"),
    }
}

/// Where image `i` is drawn: its left edge.
fn x(i: LuaImported) -> i32 {
    let rects = Drawn::new(i.imported.skin, 1).rects(1_000);
    assert_eq!(rects.len(), 1, "{rects:?}");
    rects[0][0]
}

fn autoplay(on: bool) -> FactRead {
    FactRead {
        fact: FactId::Autoplay,
        value: FactValue::Bool(on),
    }
}

fn class(c: Option<JudgeClass>) -> FactRead {
    FactRead {
        fact: FactId::JudgeClass,
        value: FactValue::Class(c),
    }
}

fn level(l: i32) -> FactRead {
    FactRead {
        fact: FactId::PlayLevel,
        value: FactValue::Level(l),
    }
}

fn deps(reads: &[FactRead]) -> FactDeps {
    FactDeps {
        scene: ScreenKind::Play,
        reads: reads.to_vec(),
    }
}

const OPTION: u16 = 0;
const NUMBER: u16 = 1;
const TEXT: u16 = 3;

fn unavailable(id: i32) -> LuaWhy {
    LuaWhy::Unavailable { api: OPTION, id }
}

#[test]
fn an_available_fact_is_answered_and_recorded() {
    assert_eq!(HOST_APIS[usize::from(OPTION)], ("main_state", "option"));
    let d = folder("x = ms.option(33) and 100 or 0");
    for (on, want) in [(true, 100), (false, 0)] {
        let facts = play(Fact::Known(on), Fact::Unavailable);
        let i = imported(import(&d, &facts));
        assert_eq!(i.facts, deps(&[autoplay(on)]), "{on}");
        assert!(i.facts.holds(&facts));
        assert_eq!(x(i), want, "{on}");
    }
    // 32 is the fact's other face: the fact's value is recorded.
    let d = folder("x = ms.option(32) and 100 or 0");
    let facts = play(Fact::Known(true), Fact::Unavailable);
    let i = imported(import(&d, &facts));
    assert_eq!(i.facts, deps(&[autoplay(true)]));
    assert!(i.facts.holds(&facts));
    assert_eq!(x(i), 0);
}

#[test]
fn a_negative_id_reads_the_negation_and_records_the_fact() {
    let d = folder("x = ms.option(-33) and 100 or 0");
    for (on, want) in [(true, 0), (false, 100)] {
        let facts = play(Fact::Known(on), Fact::Unavailable);
        let i = imported(import(&d, &facts));
        assert_eq!(i.facts, deps(&[autoplay(on)]), "{on}");
        assert!(i.facts.holds(&facts));
        assert_eq!(x(i), want, "{on}");
    }
}

#[test]
fn the_judgment_class_options_test_the_snapshot_s_class() {
    use JudgeClass::{Easy, Hard, Normal, VeryEasy, VeryHard};
    // Each class option sets its own bit of x.
    let d = folder(
        "for k = 0, 4 do if ms.option(180 + k) then x = x + (1 << k) end end\n\
         x = x * 10",
    );
    for class_now in [
        None,
        Some(VeryHard),
        Some(Hard),
        Some(Normal),
        Some(Easy),
        Some(VeryEasy),
    ] {
        let facts = play(Fact::Unavailable, Fact::Known(class_now));
        let i = imported(import(&d, &facts));
        assert_eq!(i.facts, deps(&[class(class_now)]), "{class_now:?}");
        assert!(i.facts.holds(&facts));
        let bit = class_now.map_or(0, |c| 1 << c.index());
        assert_eq!(x(i), bit * 10, "{class_now:?}");
    }
}

#[test]
fn course_mode_is_a_constant_and_records_nothing() {
    let d = folder("assert(ms.option(290) == false and ms.option(-290) == true)");
    let i = imported(import(&d, &ImportFacts::unavailable(ScreenKind::Play)));
    assert_eq!(i.facts, deps(&[]));
}

#[test]
fn the_table_song_option_is_a_constant_and_records_nothing() {
    let d = folder("assert(ms.option(1008) == false and ms.option(-1008) == true)");
    let i = imported(import(&d, &ImportFacts::unavailable(ScreenKind::Play)));
    assert_eq!(i.facts, deps(&[]));
}

#[test]
fn an_unavailable_fact_refuses_at_the_read() {
    let d = folder("local a = 1\nx = ms.option(33) and 1 or 0");
    let facts = play(Fact::Unavailable, Fact::Known(Some(JudgeClass::Normal)));
    assert_eq!(
        located(import(&d, &facts)),
        (unavailable(33), "s.luaskin".to_owned(), BODY_LINE + 1)
    );
    let d = folder("x = ms.option(181) and 1 or 0");
    let facts = play(Fact::Known(true), Fact::Unavailable);
    assert_eq!(
        located(import(&d, &facts)),
        (unavailable(181), "s.luaskin".to_owned(), BODY_LINE)
    );
}

/// The ways a skin can call `main_state.<api>` with `call` under
/// protection: `pcall` of the host function itself and of a closure, and
/// `xpcall` with a handler that returns the error, each on the body's
/// second line. That no skin code runs after the call is shown with a
/// witness only the crate's own tests have (`lua::tests`).
fn protected(api: &str, call: &str) -> [String; 3] {
    [
        format!("local a = 1\npcall(ms.{api}, {call})"),
        format!("local a = 1\npcall(function() return ms.{api}({call}) end)"),
        format!(
            "local a = 1\nxpcall(function() return ms.{api}({call}) end, function(e) return e end)"
        ),
    ]
}

#[test]
fn protected_calls_do_not_catch_a_fact_refusal_or_a_pending_read() {
    let line = BODY_LINE + 1;
    let at = |why| (why, "s.luaskin".to_owned(), line);
    let known = play(Fact::Known(true), Fact::Known(Some(JudgeClass::Normal)));
    for body in protected("option", "33") {
        let facts = play(Fact::Unavailable, Fact::Unavailable);
        assert_eq!(
            located(import(&folder(&body), &facts)),
            at(unavailable(33)),
            "{body}"
        );
    }
    for body in protected("option", "33.0") {
        assert_eq!(
            located(import(&folder(&body), &known)),
            at(LuaWhy::FactArgument),
            "{body}"
        );
    }
    for body in protected("option", "182") {
        let p = pending(import(
            &folder(&body),
            &ImportFacts::base(ScreenKind::Play, Fact::Known(true)),
        ));
        assert_eq!(
            (p.pending, p.facts),
            (FactId::JudgeClass, deps(&[])),
            "{body}"
        );
    }
}

#[test]
fn an_argument_that_is_not_an_integer_refuses() {
    let known = play(Fact::Known(true), Fact::Known(Some(JudgeClass::Normal)));
    for arg in ["33.0", "2^31", "\"33\"", ""] {
        let d = folder(&format!("local a = 1\nx = ms.option({arg}) and 1 or 0"));
        assert_eq!(
            located(import(&d, &known)),
            (LuaWhy::FactArgument, "s.luaskin".to_owned(), BODY_LINE + 1),
            "{arg}"
        );
    }
}

#[test]
fn an_integer_outside_i32_refuses() {
    let known = play(Fact::Known(true), Fact::Known(Some(JudgeClass::Normal)));
    for arg in ["2147483648", "-2147483649"] {
        let d = folder(&format!("local a = 1\nx = ms.option({arg}) and 1 or 0"));
        assert_eq!(
            located(import(&d, &known)),
            (LuaWhy::FactArgument, "s.luaskin".to_owned(), BODY_LINE + 1),
            "{arg}"
        );
        for body in protected("option", arg) {
            assert_eq!(
                located(import(&folder(&body), &known)),
                (LuaWhy::FactArgument, "s.luaskin".to_owned(), BODY_LINE + 1),
                "{body}"
            );
        }
    }
    // i32::MIN is within range: its magnitude is no built-in.
    let d = folder("assert(ms.option(-2147483648) == false)");
    assert_eq!(imported(import(&d, &known)).facts, deps(&[]));
}

#[test]
fn an_id_the_source_does_not_know_is_false_and_unrecorded() {
    let d = folder("assert(ms.option(99999) == false and ms.option(-99999) == false)");
    let known = play(Fact::Known(true), Fact::Known(Some(JudgeClass::Normal)));
    assert_eq!(imported(import(&d, &known)).facts, deps(&[]));
}

#[test]
fn another_scene_refuses_before_the_configured_pass() {
    // A play skin imported into the result slot refuses on its header:
    // the configured pass, which would fail, never runs.
    let d = folder("error(\"the configured pass ran\")");
    let mut facts = play(Fact::Known(true), Fact::Known(Some(JudgeClass::Normal)));
    facts.scene = ScreenKind::Result;
    match import(&d, &facts) {
        Err(ImportError::Slot {
            at: None,
            slot: ScreenKind::Result,
            found: ScreenKind::Play,
        }) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_changed_or_unavailable_read_fact_no_longer_holds() {
    let rank = |r| Fact::Known(Some(r));
    let d = folder("x = ms.option(33) and 1 or 0");
    let i = imported(import(&d, &play(Fact::Known(true), Fact::Unavailable)));
    assert!(i.facts.holds(&play(Fact::Known(true), Fact::Unavailable)));
    assert!(!i.facts.holds(&play(Fact::Known(false), Fact::Unavailable)));
    assert!(!i.facts.holds(&play(Fact::Unavailable, Fact::Unavailable)));
    assert!(!i.facts.holds(&play(Fact::Pending, Fact::Unavailable)));
    let d = folder("x = ms.option(182) and 1 or 0");
    let i = imported(import(
        &d,
        &play(Fact::Unavailable, rank(JudgeClass::Normal)),
    ));
    assert!(
        i.facts
            .holds(&play(Fact::Unavailable, rank(JudgeClass::Normal)))
    );
    assert!(
        !i.facts
            .holds(&play(Fact::Unavailable, rank(JudgeClass::Hard)))
    );
    assert!(!i.facts.holds(&play(Fact::Unavailable, Fact::Known(None))));
    assert!(!i.facts.holds(&play(Fact::Unavailable, Fact::Unavailable)));
}

#[test]
fn a_change_to_an_unread_fact_still_holds() {
    let d = folder("x = ms.option(33) and 1 or 0");
    let i = imported(import(&d, &play(Fact::Known(true), Fact::Unavailable)));
    let classes = [
        Fact::Unavailable,
        Fact::Pending,
        Fact::Known(None),
        Fact::Known(Some(JudgeClass::VeryHard)),
        Fact::Known(Some(JudgeClass::Easy)),
    ];
    for c in classes {
        assert!(i.facts.holds(&play(Fact::Known(true), c)), "{c:?}");
    }
    // The class read holds whatever the autoplay fact, which it did not
    // read.
    let d = folder("x = ms.option(182) and 1 or 0");
    let normal = Fact::Known(Some(JudgeClass::Normal));
    let i = imported(import(&d, &play(Fact::Unavailable, normal)));
    assert!(i.facts.holds(&play(Fact::Known(false), normal)));
}

#[test]
fn another_scene_never_holds() {
    let on = play(Fact::Known(true), Fact::Known(Some(JudgeClass::Normal)));
    let result = ImportFacts {
        scene: ScreenKind::Result,
        ..on
    };
    for body in ["", "x = ms.option(33) and 1 or 0"] {
        let i = imported(import(&folder(body), &on));
        assert!(i.facts.holds(&on), "{body}");
        assert!(!i.facts.holds(&result), "{body}");
    }
    let deps = FactDeps {
        scene: ScreenKind::Result,
        reads: Vec::new(),
    };
    assert!(!deps.holds(&on));
}

#[test]
fn reads_are_recorded_once_in_first_read_order() {
    let facts = play(Fact::Known(true), Fact::Known(Some(JudgeClass::Normal)));
    let normal = class(Some(JudgeClass::Normal));
    for (ids, want) in [
        ("182, 32, -33, 33, 181, 182", [normal, autoplay(true)]),
        ("-32, 33, 32, -182", [autoplay(true), normal]),
    ] {
        let d = folder(&format!(
            "for _, n in ipairs({{ {ids} }}) do ms.option(n) end"
        ));
        let i = imported(import(&d, &facts));
        assert_eq!(i.facts, deps(&want), "{ids}");
    }
}

#[test]
fn a_base_import_stops_pending_at_a_chart_fact() {
    let d = folder("local on = ms.option(33)\nx = ms.option(182) and 1 or 0");
    let p = pending(import(
        &d,
        &ImportFacts::base(ScreenKind::Play, Fact::Known(true)),
    ));
    assert_eq!(p.pending, FactId::JudgeClass);
    assert_eq!(p.facts, deps(&[autoplay(true)]));
    assert_eq!(p.dependencies, ["s.luaskin"]);
    assert!(
        p.facts
            .holds(&ImportFacts::base(ScreenKind::Play, Fact::Known(true)))
    );
    assert!(
        !p.facts
            .holds(&ImportFacts::base(ScreenKind::Play, Fact::Known(false)))
    );
    // The attempt's final snapshot imports.
    let i = imported(import(
        &d,
        &play(Fact::Known(true), Fact::Known(Some(JudgeClass::Normal))),
    ));
    assert_eq!(
        i.facts,
        deps(&[autoplay(true), class(Some(JudgeClass::Normal))])
    );
    assert_eq!(x(i), 1);
}

/// A chart with no rank header is in the class of `#RANK 2`, the source's
/// default; one in no class answers none of 180 to 184 and records that.
#[test]
fn a_rankless_chart_is_normal_and_a_classless_one_holds_none() {
    assert_eq!(JudgeClass::of_headers(&[]), Some(JudgeClass::Normal));
    let d = folder("x = (ms.option(182) and 1 or 0) + (ms.option(-184) and 10 or 0)");
    let facts = play(Fact::Unavailable, Fact::Known(None));
    let i = imported(import(&d, &facts));
    assert_eq!(i.facts, deps(&[class(None)]));
    assert_eq!(x(i), 10);
}

#[test]
fn the_play_level_is_answered_and_recorded_once() {
    assert_eq!(HOST_APIS[usize::from(NUMBER)], ("main_state", "number"));
    let d = folder("x = ms.number(96) * 10 + ms.number(96)");
    for l in [12, 0, -1] {
        let facts = leveled(Fact::Known(l));
        let i = imported(import(&d, &facts));
        assert_eq!(i.facts, deps(&[level(l)]), "{l}");
        assert!(i.facts.holds(&facts));
        assert_eq!(x(i), l * 11, "{l}");
    }
}

#[test]
fn a_base_import_stops_pending_at_the_play_level() {
    let d = folder("local on = ms.option(33)\nx = ms.number(96)");
    let base = ImportFacts::base(ScreenKind::Play, Fact::Known(true));
    assert_eq!(base.play_level, Fact::Pending);
    let p = pending(import(&d, &base));
    assert_eq!(p.pending, FactId::PlayLevel);
    assert_eq!(p.facts, deps(&[autoplay(true)]));
    // Selection has no chart: its level is unavailable.
    let selection = ImportFacts::base(ScreenKind::Selection, Fact::Known(true));
    assert_eq!(selection.play_level, Fact::Unavailable);
}

// Covers: T09
#[test]
fn protected_calls_do_not_catch_a_play_level_refusal_or_a_pending_read() {
    let line = BODY_LINE + 1;
    let at = |why| (why, "s.luaskin".to_owned(), line);
    let unavailable = LuaWhy::Unavailable {
        api: NUMBER,
        id: 96,
    };
    for body in protected("number", "96") {
        let d = folder(&body);
        let facts = leveled(Fact::Unavailable);
        assert_eq!(located(import(&d, &facts)), at(unavailable), "{body}");
        let p = pending(import(&d, &leveled(Fact::Pending)));
        assert_eq!(
            (p.pending, p.facts),
            (FactId::PlayLevel, deps(&[])),
            "{body}"
        );
    }
    for (api, call, why) in [
        ("number", "96.0", LuaWhy::FactArgument),
        ("number", "97", LuaWhy::LiveState { api: NUMBER }),
        ("text", "\"1003\"", LuaWhy::FactArgument),
        ("text", "12", LuaWhy::LiveState { api: TEXT }),
    ] {
        for body in protected(api, call) {
            let d = folder(&body);
            assert_eq!(
                located(import(&d, &leveled(Fact::Known(12)))),
                at(why),
                "{body}"
            );
        }
    }
}

#[test]
fn a_read_play_level_that_changes_no_longer_holds() {
    let d = folder("x = ms.number(96)");
    let i = imported(import(&d, &leveled(Fact::Known(12))));
    assert!(i.facts.holds(&leveled(Fact::Known(12))));
    assert!(!i.facts.holds(&leveled(Fact::Known(11))));
    assert!(!i.facts.holds(&leveled(Fact::Pending)));
    assert!(!i.facts.holds(&leveled(Fact::Unavailable)));
    // A level nothing read may change.
    let d = folder("x = ms.option(33) and 1 or 0");
    let i = imported(import(&d, &play(Fact::Known(true), Fact::Unavailable)));
    for l in [
        Fact::Known(12),
        Fact::Known(0),
        Fact::Pending,
        Fact::Unavailable,
    ] {
        let facts = ImportFacts {
            play_level: l,
            ..play(Fact::Known(true), Fact::Unavailable)
        };
        assert!(i.facts.holds(&facts), "{l:?}");
    }
}

#[test]
fn mixed_facts_are_recorded_in_first_read_order() {
    let d = folder(
        "local a = ms.option(33)\nlocal l = ms.number(96)\nlocal c = ms.option(182)\n\
         local again = ms.number(96) + (ms.option(33) and 1 or 0)",
    );
    let facts = ImportFacts {
        play_level: Fact::Known(7),
        ..play(Fact::Known(false), Fact::Known(Some(JudgeClass::Normal)))
    };
    let i = imported(import(&d, &facts));
    assert_eq!(
        i.facts,
        deps(&[autoplay(false), level(7), class(Some(JudgeClass::Normal))])
    );
}

#[test]
fn the_play_level_s_argument_must_be_an_integer_within_i32() {
    let facts = leveled(Fact::Known(12));
    for arg in ["96.0", "\"96\"", "2^31", "2147483648", ""] {
        let d = folder(&format!("local a = 1\nx = ms.number({arg})"));
        assert_eq!(
            located(import(&d, &facts)),
            (LuaWhy::FactArgument, "s.luaskin".to_owned(), BODY_LINE + 1),
            "{arg}"
        );
    }
    // Every other id reads play state.
    for arg in ["97", "-96", "71"] {
        let d = folder(&format!("local a = 1\nx = ms.number({arg})"));
        assert_eq!(
            located(import(&d, &facts)),
            (
                LuaWhy::LiveState { api: NUMBER },
                "s.luaskin".to_owned(),
                BODY_LINE + 1
            ),
            "{arg}"
        );
    }
}

#[test]
fn the_table_texts_are_empty_and_recorded_nowhere() {
    assert_eq!(HOST_APIS[usize::from(TEXT)], ("main_state", "text"));
    let d = folder(
        "for id = 1001, 1003 do\n\
           local s = ms.text(id)\n\
           assert(type(s) == \"string\" and s == \"\")\n\
         end",
    );
    let i = imported(import(&d, &ImportFacts::unavailable(ScreenKind::Play)));
    assert_eq!(i.facts, deps(&[]));
    for (arg, why) in [
        ("12", LuaWhy::LiveState { api: TEXT }),
        ("1000", LuaWhy::LiveState { api: TEXT }),
        ("1004", LuaWhy::LiveState { api: TEXT }),
        ("1003.0", LuaWhy::FactArgument),
        ("\"1003\"", LuaWhy::FactArgument),
    ] {
        let d = folder(&format!("local a = 1\nlocal s = ms.text({arg})"));
        assert_eq!(
            located(import(&d, &ImportFacts::unavailable(ScreenKind::Play))),
            (why, "s.luaskin".to_owned(), BODY_LINE + 1),
            "{arg}"
        );
    }
}

#[test]
fn a_part_that_searches_the_table_level_completes() {
    // A part loaded under protection reads the table texts as it is
    // built and searches one: it completes, and the skin imports.
    let d = folder(
        "local ok, name = pcall(dofile, \"customize/table.lua\")\n\
         assert(ok and name == \"\")\n\
         x = 100",
    );
    common::write(
        d.path(),
        "customize/table.lua",
        b"local main_state = require(\"main_state\")\n\
          local name = main_state.text(1003)\n\
          if string.find(main_state.text(1002), \"x\") then error(\"found\") end\n\
          return name\n",
    );
    let i = imported(import(&d, &ImportFacts::unavailable(ScreenKind::Play)));
    assert_eq!(i.facts, deps(&[]));
    assert_eq!(i.dependencies, ["s.luaskin", "customize/table.lua"]);
    assert_eq!(x(i), 100);
}

#[cfg(feature = "dev-tools")]
#[test]
fn the_timed_import_keeps_a_pending_result() {
    use skin_beatoraja::lua::LuaTimed;
    let d = folder("local on = ms.option(33)\nx = ms.option(182) and 1 or 0");
    let h = header(&d);
    let config = Config::defaults(h.header());
    let base = ImportFacts::base(ScreenKind::Play, Fact::Known(true));
    match h.timed(&config, &base, &Cancel::new()) {
        Ok(LuaTimed::Pending(p)) => {
            assert_eq!(p.pending, FactId::JudgeClass);
            assert_eq!(p.facts, deps(&[autoplay(true)]));
            assert_eq!(p.dependencies, ["s.luaskin"]);
        }
        other => panic!("{other:?}"),
    }
    let known = play(Fact::Known(true), Fact::Known(Some(JudgeClass::Normal)));
    assert!(matches!(
        h.timed(&config, &known, &Cancel::new()),
        Ok(LuaTimed::Imported(_))
    ));
}
