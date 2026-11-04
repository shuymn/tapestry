//! The sandbox's failure paths that need a private witness: cancellation
//! at a chosen point, a panic in a host function, the VMs alive, and the
//! instruction hook on a thread the host creates. Each runs through the
//! public entries ([`crate::read_lua_header`], [`LuaHeader::import`])
//! except the last, which no skin can reach.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use super::{Cancel, LuaHeader, LuaLimit, LuaWhy, read_lua_header};
use crate::error::ImportError;
use crate::import::Config;
use skin_model::ScreenKind;

/// The configured pass of `h` with `config` on a play scene whose import
/// facts are all unavailable, which never ends pending.
pub(crate) fn play(
    h: &LuaHeader,
    config: &Config,
    cancel: &Cancel,
) -> Result<super::LuaImported, ImportError> {
    let facts = super::facts::ImportFacts::unavailable(skin_model::ScreenKind::Play);
    match h.import(config, &facts, cancel)? {
        super::LuaOutcome::Imported(i) => Ok(i),
        super::LuaOutcome::Pending(p) => panic!("pending with no pending fact: {p:?}"),
    }
}

/// What a test does at a point of the import.
type Action = Box<dyn Fn()>;

thread_local! {
    /// The host calls left before [`AT_HOST_CALL`]'s action runs.
    static HOST_CALLS: Cell<Option<u32>> = const { Cell::new(None) };
    static AT_HOST_CALL: RefCell<Option<Action>> = RefCell::new(None);
    static AT_CONVERSION: RefCell<Option<Action>> = RefCell::new(None);
    static AT_CONVERTED: RefCell<Option<Action>> = RefCell::new(None);
    /// The VMs alive on this thread.
    static LIVE: Cell<usize> = const { Cell::new(0) };
    /// The passes this thread has run.
    pub(crate) static PASSES: Cell<usize> = const { Cell::new(0) };
    /// Whether a conversion is copying captures, and the host calls made
    /// meanwhile: a skin handler run by the capture would make one.
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
    pub(crate) static CAPTURE_CALLS: Cell<u32> = const { Cell::new(0) };
}

thread_local! {
    static AT_EXPANSION: RefCell<Option<Action>> = RefCell::new(None);
    /// Whether a VM built now gets `continued`, and how often a skin
    /// called it: a witness that is no host function, so the ledger
    /// neither charges nor refuses it.
    static CONTINUED_ON: Cell<bool> = const { Cell::new(false) };
    static CONTINUED: Cell<u32> = const { Cell::new(0) };
}

/// Gives the skin the global `continued` when a test asked for it.
pub(crate) fn install_continued(lua: &mlua::Lua, env: &mlua::Table) -> mlua::Result<()> {
    if CONTINUED_ON.with(Cell::get) {
        let f = lua.create_function(|_, ()| {
            CONTINUED.with(|n| n.set(n.get() + 1));
            Ok(())
        })?;
        env.raw_set("continued", f)?;
    }
    Ok(())
}

thread_local! {
    /// The size-memo slots this thread's lowerings held, summed.
    pub(crate) static MEMO_SLOTS: Cell<usize> = const { Cell::new(0) };
}

/// Called as a lowering ends with `slots` size-memo slots.
pub(crate) fn memo_witness(slots: usize) {
    MEMO_SLOTS.with(|n| n.set(n.get() + slots));
}

/// Called as lowering expands an instance.
pub(crate) fn expansion_witness() {
    if let Some(f) = AT_EXPANSION.with(|a| a.borrow_mut().take()) {
        f();
    }
}

/// Runs `f` when a conversion has next read its whole table.
pub(crate) fn at_converted(f: impl Fn() + 'static) {
    AT_CONVERTED.with(|a| *a.borrow_mut() = Some(Box::new(f)));
}

/// Runs `f` when lowering next expands an instance.
pub(crate) fn at_expansion(f: impl Fn() + 'static) {
    AT_EXPANSION.with(|a| *a.borrow_mut() = Some(Box::new(f)));
}

/// Called as a conversion starts and ends copying captures.
pub(crate) fn capturing(on: bool) {
    CAPTURING.with(|c| c.set(on));
}

/// Called as each pass starts.
pub(crate) fn pass_witness() {
    PASSES.with(|n| n.set(n.get() + 1));
}

/// Called at every charged host call.
pub(crate) fn host_call_witness() {
    if CAPTURING.with(Cell::get) {
        CAPTURE_CALLS.with(|n| n.set(n.get() + 1));
    }
    let due = HOST_CALLS.with(|n| match n.get() {
        Some(0) => {
            n.set(None);
            true
        }
        Some(k) => {
            n.set(Some(k - 1));
            false
        }
        None => false,
    });
    if due && let Some(f) = AT_HOST_CALL.with(|a| a.borrow_mut().take()) {
        f();
    }
}

/// Called when a conversion starts.
pub(crate) fn conversion_witness() {
    if let Some(f) = AT_CONVERSION.with(|a| a.borrow_mut().take()) {
        f();
    }
}

/// Called when a conversion has read the whole table.
pub(crate) fn converted_witness() {
    if let Some(f) = AT_CONVERTED.with(|a| a.borrow_mut().take()) {
        f();
    }
}

/// A VM's presence: kept in its app data, so it ends with the VM.
pub(crate) struct Live;

impl Live {
    pub(crate) fn new() -> Self {
        LIVE.with(|n| n.set(n.get() + 1));
        Self
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        LIVE.with(|n| n.set(n.get() - 1));
    }
}

fn live() -> usize {
    LIVE.with(Cell::get)
}

/// Runs `f` at host call `n` (0 the first).
fn at_host_call(n: u32, f: impl Fn() + 'static) {
    HOST_CALLS.with(|c| c.set(Some(n)));
    AT_HOST_CALL.with(|a| *a.borrow_mut() = Some(Box::new(f)));
}

fn folder(entry: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("s.luaskin"), entry).unwrap();
    std::fs::write(d.path().join("p.lua"), "return 1").unwrap();
    d
}

fn header(root: &Path, cancel: &Cancel) -> Result<LuaHeader, ImportError> {
    read_lua_header(root, "s.luaskin", cancel)
}

#[track_caller]
pub(crate) fn why<T: std::fmt::Debug>(r: Result<T, ImportError>) -> LuaWhy {
    match r {
        Err(ImportError::Lua { why, .. }) => why,
        other => panic!("{other:?}"),
    }
}

/// A loop of host calls that only a fault ends.
const ENDLESS: &str = "local n = 0\nwhile true do n = n + 1 pcall(type, n) end";

#[test]
fn cancellation_is_seen_at_the_next_host_call_or_step() {
    for entry in [
        ENDLESS.to_owned(),
        format!("pcall(function() {ENDLESS} end)\nreturn {{}}"),
        // Before a source is read.
        "for i = 1, 100 do dofile(\"p.lua\") end\nwhile true do end".to_owned(),
    ] {
        let d = folder(&entry);
        let cancel = Cancel::new();
        let remote = cancel.clone();
        at_host_call(10, move || remote.cancel());
        assert_eq!(why(header(d.path(), &cancel)), LuaWhy::Cancelled, "{entry}");
        assert_eq!(live(), 0);
    }
}

#[test]
fn cancellation_is_seen_by_the_conversion() {
    let d = folder("return { type = 0 }");
    let cancel = Cancel::new();
    let remote = cancel.clone();
    AT_CONVERSION.with(|a| *a.borrow_mut() = Some(Box::new(move || remote.cancel())));
    assert_eq!(why(header(d.path(), &cancel)), LuaWhy::Cancelled);
    assert_eq!(live(), 0);
}

#[test]
fn a_panic_in_a_host_function_unwinds_and_drops_the_vm() {
    let d = folder(ENDLESS);
    at_host_call(3, || panic!("a host function panicked"));
    let r = std::panic::catch_unwind(|| header(d.path(), &Cancel::new()));
    assert!(r.is_err());
    assert_eq!(live(), 0);
    // A protected call does not catch it.
    let d = folder(&format!("pcall(function() {ENDLESS} end)\nreturn {{}}"));
    at_host_call(3, || panic!("a host function panicked"));
    let r = std::panic::catch_unwind(|| header(d.path(), &Cancel::new()));
    assert!(r.is_err());
    assert_eq!(live(), 0);
}

/// A panic in a host function counts the VM closed in the dev-tools
/// hooks too, so a later caller's count starts from its base.
#[cfg(feature = "dev-tools")]
#[test]
fn a_panic_in_a_host_function_closes_the_vm_in_the_hooks() {
    let base = super::vm_hooks::thread_live();
    let d = folder(ENDLESS);
    at_host_call(3, || panic!("a host function panicked"));
    let r = std::panic::catch_unwind(|| header(d.path(), &Cancel::new()));
    assert!(r.is_err());
    assert_eq!(super::vm_hooks::thread_live(), base);
}

#[test]
fn every_pass_drops_its_vm_on_every_outcome() {
    let ok = folder("return { type = 0 }");
    let h = header(ok.path(), &Cancel::new()).unwrap();
    assert_eq!(live(), 0);
    super::tests::play(&h, &Config::defaults(h.header()), &Cancel::new()).unwrap();
    assert_eq!(live(), 0);
    let fails = folder("if skin_config then error(\"x\") end\nreturn { type = 0 }");
    let h = header(fails.path(), &Cancel::new()).unwrap();
    super::tests::play(&h, &Config::defaults(h.header()), &Cancel::new()).unwrap_err();
    assert_eq!(live(), 0);
    for entry in [
        "while true do end",
        "local s = \"x\"\nfor i = 1, 27 do s = s .. s end",
        "error(\"x\")",
        "return 1",
    ] {
        let d = folder(entry);
        header(d.path(), &Cancel::new()).unwrap_err();
        assert_eq!(live(), 0, "{entry}");
    }
}

#[test]
fn the_instruction_hook_covers_a_thread_the_host_creates() {
    // No skin can create a thread (there is no coroutine library), so this
    // drives the VM the passes build directly.
    let d = folder("return {}");
    let root = skin_model::Root::new(d.path()).unwrap();
    let pass = Rc::new(super::test_pass(root));
    let lua = super::sandbox::vm(&pass).unwrap();
    let f = lua.load("while true do end").into_function().unwrap();
    let thread = lua.create_thread(f).unwrap();
    assert!(thread.resume::<()>(()).is_err());
    match pass.ledger.take_stop() {
        Some(super::ledger::Stop::Fault(ImportError::Lua { why, .. })) => {
            assert_eq!(why, LuaWhy::Limit(LuaLimit::Instructions));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn cancellation_is_seen_by_the_common_reader() {
    // The table is converted whole, then the request comes: the JSON file
    // it includes is not read.
    let d = folder("return { type = 0, property = { include = \"p.json\" } }");
    std::fs::write(d.path().join("p.json"), "[]").unwrap();
    let cancel = Cancel::new();
    let remote = cancel.clone();
    AT_CONVERTED.with(|a| *a.borrow_mut() = Some(Box::new(move || remote.cancel())));
    assert_eq!(why(header(d.path(), &cancel)), LuaWhy::Cancelled);
    assert_eq!(live(), 0);
}

#[test]
fn the_time_cap_spans_both_passes() {
    use std::time::{Duration, Instant};

    use super::limits::MAX_LUA_TIME;
    // A header pass that ran almost all the time leaves the configured
    // pass only the rest.
    let d = folder(
        "if skin_config then\n\
           local s, t = \"x\", \"x\"\n\
           for i = 1, 24 do s, t = s .. s, t .. t end\n\
           while true do if s == t then end end\n\
         end\n\
         return { type = 0 }",
    );
    let mut h = header(d.path(), &Cancel::new()).unwrap();
    h.spent = MAX_LUA_TIME - Duration::from_millis(300);
    let start = Instant::now();
    let r = super::tests::play(&h, &Config::defaults(h.header()), &Cancel::new());
    let took = start.elapsed();
    // Without the carried time it would run its own 5 s.
    assert_eq!(why(r), LuaWhy::Limit(LuaLimit::Time));
    assert!(took < Duration::from_secs(3), "{took:?}");
}

/// Imports a play skin whose configured pass runs `body` against
/// `facts` with the witness `continued` present, and returns the outcome
/// and how often the skin called the witness.
fn continued(
    body: &str,
    facts: &super::facts::ImportFacts,
) -> (Result<super::LuaOutcome, ImportError>, u32) {
    let d = folder(&format!(
        "if not skin_config then return {{ type = 0 }} end\n\
         local ms = require(\"main_state\")\n\
         {body}\n\
         return {{ type = 0 }}"
    ));
    let h = header(d.path(), &Cancel::new()).unwrap();
    CONTINUED_ON.with(|on| on.set(true));
    CONTINUED.with(|n| n.set(0));
    let r = h.import(&Config::defaults(h.header()), facts, &Cancel::new());
    CONTINUED_ON.with(|on| on.set(false));
    (r, CONTINUED.with(Cell::get))
}

#[test]
fn no_skin_code_runs_after_a_fact_stop_inside_a_protected_call() {
    use super::facts::{ChartRank, Fact, ImportFacts};
    let unavailable = ImportFacts::unavailable(ScreenKind::Play);
    let base = ImportFacts::base(ScreenKind::Play, Fact::Known(true));
    let known = ImportFacts {
        chart_rank: Fact::Known(Some(ChartRank::Rank(2))),
        ..base
    };
    // The witness sees a skin go on after an error a protected call
    // catches.
    let (r, n) = continued("pcall(error, \"x\")\ncontinued()", &unavailable);
    assert!(matches!(r, Ok(super::LuaOutcome::Imported(_))), "{r:?}");
    assert_eq!(n, 1);
    for (call, facts) in [("33", unavailable), ("33.0", known), ("182", base)] {
        for wrap in [
            format!("pcall(ms.option, {call})"),
            format!("pcall(function() return ms.option({call}) end)"),
            format!(
                "xpcall(function() return ms.option({call}) end, function(e) continued() return e end)"
            ),
        ] {
            let (r, n) = continued(&format!("{wrap}\ncontinued()"), &facts);
            match (call, &r) {
                ("182", Ok(super::LuaOutcome::Pending(_))) | (_, Err(_)) => {}
                _ => panic!("{wrap}: {r:?}"),
            }
            assert_eq!(n, 0, "{wrap}");
        }
    }
}

/// A Lua skin of type `ty` whose configured pass draws a critical EX
/// score number from a missing file.
fn kind_skin(ty: i32) -> tempfile::TempDir {
    folder(&format!(
        "local skin = {{ type = {ty}, w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         skin.source = {{ {{ id = \"gone\", path = \"gone.png\" }} }}\n\
         skin.value = {{ {{ id = \"ex\", src = \"gone\", w = 10, h = 1, divx = 10, digit = 4, ref = 71 }} }}\n\
         skin.destination = {{ {{ id = \"ex\", dst = {{ {{ w = 1, h = 1 }} }} }} }}\n\
         return skin\n"
    ))
}

/// Both passes of [`kind_skin`]`(ty)` on scene `scene`: the result, the
/// passes run and the asset admissions made.
fn kind_import(ty: i32, scene: ScreenKind) -> (Result<super::LuaOutcome, ImportError>, usize, u32) {
    let d = kind_skin(ty);
    PASSES.with(|n| n.set(0));
    crate::import::asset::ADMITS.with(|n| n.set(0));
    let cancel = Cancel::new();
    let h = header(d.path(), &cancel).unwrap();
    let facts = super::facts::ImportFacts::unavailable(scene);
    let r = h.import(&Config::defaults(h.header()), &facts, &cancel);
    (
        r,
        PASSES.with(Cell::get),
        crate::import::asset::ADMITS.with(Cell::get),
    )
}

#[test]
fn a_lua_skin_in_another_slot_refuses_before_the_configured_pass() {
    use ScreenKind::{Play, Result, Selection};
    for (ty, scene, found) in [(5, Play, Selection), (0, Result, Play)] {
        let (r, passes, admits) = kind_import(ty, scene);
        match r {
            Err(ImportError::Slot {
                at: None,
                slot,
                found: f,
            }) => assert_eq!((slot, f), (scene, found)),
            other => panic!("{ty}: {other:?}"),
        }
        assert_eq!((passes, admits), (1, 0), "{ty}");
    }
    // The control: in its own slot the configured pass runs and the
    // import reaches its critical number's missing file.
    let (r, passes, admits) = kind_import(0, Play);
    assert!(matches!(r, Err(ImportError::Asset { .. })), "{r:?}");
    assert_eq!(passes, 2);
    assert!(admits >= 1);
}

#[test]
fn a_lua_kind_with_no_importer_refuses_before_the_configured_pass() {
    for (ty, kind) in [
        (5, ScreenKind::Selection),
        (6, ScreenKind::Decision),
        (7, ScreenKind::Result),
    ] {
        let (r, passes, admits) = kind_import(ty, kind);
        match r {
            Err(ImportError::NoImporter { kind: k }) => assert_eq!(k, kind),
            other => panic!("{ty}: {other:?}"),
        }
        assert_eq!((passes, admits), (1, 0), "{ty}");
    }
}
