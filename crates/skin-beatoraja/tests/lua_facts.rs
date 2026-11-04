//! Import facts through the public entries: [`read_lua_header`], then
//! [`LuaHeader::import`] against a snapshot. Each fixture is a small
//! self-made play skin that reads `main_state.option` while it builds its
//! definition.

mod common;

use common::play::Drawn;
use skin_beatoraja::lua::HOST_APIS;
use skin_beatoraja::{
    Cancel, ChartRank, Config, Fact, FactDeps, FactId, FactRead, FactValue, ImportError,
    ImportFacts, JudgeClass, LuaHeader, LuaImported, LuaOutcome, LuaPending, LuaWhy,
    read_lua_header,
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

/// A play snapshot.
fn play(autoplay: Fact<bool>, chart_rank: Fact<Option<ChartRank>>) -> ImportFacts {
    ImportFacts {
        scene: ScreenKind::Play,
        autoplay,
        chart_rank,
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

fn deps(reads: &[FactRead]) -> FactDeps {
    FactDeps {
        scene: ScreenKind::Play,
        reads: reads.to_vec(),
    }
}

const OPTION: u16 = 0;

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
fn the_judgment_class_follows_the_source_s_bands() {
    use ChartRank::{DefExRankMilli as Defex, Rank};
    use JudgeClass::{Easy, Hard, Normal, VeryEasy, VeryHard};
    // Each class option sets its own bit of x.
    let d = folder(
        "for k = 0, 4 do if ms.option(180 + k) then x = x + (1 << k) end end\n\
         x = x * 10",
    );
    let cases = [
        (None, Some(Normal)),
        (Some(Rank(0)), Some(VeryHard)),
        (Some(Rank(1)), Some(Hard)),
        (Some(Rank(2)), Some(Normal)),
        (Some(Rank(3)), Some(Easy)),
        (Some(Rank(4)), Some(VeryEasy)),
        (Some(Rank(5)), None),
        (Some(Rank(-1)), None),
        (Some(Defex(9_999)), None),
        (Some(Defex(10_000)), Some(VeryHard)),
        (Some(Defex(34_999)), Some(VeryHard)),
        (Some(Defex(35_000)), Some(Hard)),
        (Some(Defex(59_999)), Some(Hard)),
        (Some(Defex(60_000)), Some(Normal)),
        (Some(Defex(84_999)), Some(Normal)),
        (Some(Defex(85_000)), Some(Easy)),
        (Some(Defex(109_999)), Some(Easy)),
        (Some(Defex(110_000)), Some(VeryEasy)),
        (Some(Defex(200_000)), Some(VeryEasy)),
    ];
    for (rank, want) in cases {
        let facts = play(Fact::Unavailable, Fact::Known(rank));
        let i = imported(import(&d, &facts));
        assert_eq!(i.facts, deps(&[class(want)]), "{rank:?}");
        assert!(i.facts.holds(&facts));
        let bit = want.map_or(0, |c| 1 << (c as i32));
        assert_eq!(x(i), bit * 10, "{rank:?}");
    }
}

#[test]
fn course_mode_is_a_constant_and_records_nothing() {
    let d = folder("assert(ms.option(290) == false and ms.option(-290) == true)");
    let i = imported(import(&d, &ImportFacts::unavailable(ScreenKind::Play)));
    assert_eq!(i.facts, deps(&[]));
}

#[test]
fn an_unavailable_fact_refuses_at_the_read() {
    let d = folder("local a = 1\nx = ms.option(33) and 1 or 0");
    let facts = play(Fact::Unavailable, Fact::Known(Some(ChartRank::Rank(2))));
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

/// The ways a skin can call `call` under protection: `pcall` of the
/// host function itself and of a closure, and `xpcall` with a handler
/// that returns the error, each on the body's second line. That no skin
/// code runs after the call is shown with a witness only the crate's own
/// tests have (`lua::tests`).
fn protected(call: &str) -> [String; 3] {
    [
        format!("local a = 1\npcall(ms.option, {call})"),
        format!("local a = 1\npcall(function() return ms.option({call}) end)"),
        format!(
            "local a = 1\nxpcall(function() return ms.option({call}) end, function(e) return e end)"
        ),
    ]
}

#[test]
fn protected_calls_do_not_catch_a_fact_refusal_or_a_pending_read() {
    let line = BODY_LINE + 1;
    let at = |why| (why, "s.luaskin".to_owned(), line);
    let known = play(Fact::Known(true), Fact::Known(Some(ChartRank::Rank(2))));
    for body in protected("33") {
        let facts = play(Fact::Unavailable, Fact::Unavailable);
        assert_eq!(
            located(import(&folder(&body), &facts)),
            at(unavailable(33)),
            "{body}"
        );
    }
    for body in protected("33.0") {
        assert_eq!(
            located(import(&folder(&body), &known)),
            at(LuaWhy::FactArgument),
            "{body}"
        );
    }
    for body in protected("182") {
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
    let known = play(Fact::Known(true), Fact::Known(Some(ChartRank::Rank(2))));
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
    let known = play(Fact::Known(true), Fact::Known(Some(ChartRank::Rank(2))));
    for arg in ["2147483648", "-2147483649"] {
        let d = folder(&format!("local a = 1\nx = ms.option({arg}) and 1 or 0"));
        assert_eq!(
            located(import(&d, &known)),
            (LuaWhy::FactArgument, "s.luaskin".to_owned(), BODY_LINE + 1),
            "{arg}"
        );
        for body in protected(arg) {
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
    let known = play(Fact::Known(true), Fact::Known(Some(ChartRank::Rank(2))));
    assert_eq!(imported(import(&d, &known)).facts, deps(&[]));
}

#[test]
fn another_scene_refuses_before_the_configured_pass() {
    // A play skin imported into the result slot refuses on its header:
    // the configured pass, which would fail, never runs.
    let d = folder("error(\"the configured pass ran\")");
    let mut facts = play(Fact::Known(true), Fact::Known(Some(ChartRank::Rank(2))));
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
    let rank = |r| Fact::Known(Some(ChartRank::Rank(r)));
    let d = folder("x = ms.option(33) and 1 or 0");
    let i = imported(import(&d, &play(Fact::Known(true), Fact::Unavailable)));
    assert!(i.facts.holds(&play(Fact::Known(true), Fact::Unavailable)));
    assert!(!i.facts.holds(&play(Fact::Known(false), Fact::Unavailable)));
    assert!(!i.facts.holds(&play(Fact::Unavailable, Fact::Unavailable)));
    assert!(!i.facts.holds(&play(Fact::Pending, Fact::Unavailable)));
    let d = folder("x = ms.option(182) and 1 or 0");
    let i = imported(import(&d, &play(Fact::Unavailable, rank(2))));
    assert!(i.facts.holds(&play(Fact::Unavailable, rank(2))));
    assert!(!i.facts.holds(&play(Fact::Unavailable, rank(1))));
    assert!(!i.facts.holds(&play(Fact::Unavailable, Fact::Unavailable)));
}

#[test]
fn a_change_to_an_unread_fact_still_holds() {
    let d = folder("x = ms.option(33) and 1 or 0");
    let i = imported(import(&d, &play(Fact::Known(true), Fact::Unavailable)));
    let ranks = [
        Fact::Unavailable,
        Fact::Pending,
        Fact::Known(Some(ChartRank::Rank(0))),
        Fact::Known(Some(ChartRank::Rank(9))),
        Fact::Known(Some(ChartRank::DefExRankMilli(120_000))),
    ];
    for rank in ranks {
        assert!(i.facts.holds(&play(Fact::Known(true), rank)), "{rank:?}");
    }
    // Two ranks in one class share the import.
    let d = folder("x = ms.option(182) and 1 or 0");
    let i = imported(import(
        &d,
        &play(Fact::Unavailable, Fact::Known(Some(ChartRank::Rank(2)))),
    ));
    let defex = Fact::Known(Some(ChartRank::DefExRankMilli(70_000)));
    assert!(i.facts.holds(&play(Fact::Unavailable, defex)));
    assert!(i.facts.holds(&play(Fact::Known(false), defex)));
}

#[test]
fn another_scene_never_holds() {
    let on = play(Fact::Known(true), Fact::Known(Some(ChartRank::Rank(2))));
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
    let facts = play(Fact::Known(true), Fact::Known(Some(ChartRank::Rank(2))));
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
        &play(Fact::Known(true), Fact::Known(Some(ChartRank::Rank(2)))),
    ));
    assert_eq!(
        i.facts,
        deps(&[autoplay(true), class(Some(JudgeClass::Normal))])
    );
    assert_eq!(x(i), 1);
}

#[test]
fn a_chart_with_no_rank_header_is_judged_and_classed_as_rank_2() {
    // Every shipped preset reads no rank from a chart without `#RANK` or
    // `#DEFEXRANK` and judges it with its absent window, which is its
    // `#RANK 2` window: the class of `#RANK 2` is the class it plays.
    for name in rules::BUILT_IN_PRESETS {
        let p = rules::built_in(name).unwrap();
        let read = p.headers.read(&[]).rank;
        assert_eq!(read, None, "{name}");
        let absent = p.window_policy.prepare(&p.tables, read);
        let rank_2 = p
            .window_policy
            .prepare(&p.tables, Some(rules::JudgeRank::Rank(2)));
        assert!(absent.is_some(), "{name}");
        assert_eq!(absent, rank_2, "{name}");
    }
    let d = folder("x = ms.option(182) and 1 or 0");
    let facts = play(Fact::Unavailable, Fact::Known(None));
    let i = imported(import(&d, &facts));
    assert_eq!(i.facts, deps(&[class(Some(JudgeClass::Normal))]));
    assert_eq!(x(i), 1);
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
    let known = play(Fact::Known(true), Fact::Known(Some(ChartRank::Rank(2))));
    assert!(matches!(
        h.timed(&config, &known, &Cancel::new()),
        Ok(LuaTimed::Imported(_))
    ));
}
