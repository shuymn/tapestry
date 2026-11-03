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
}

/// Called as each pass starts.
pub(crate) fn pass_witness() {
    PASSES.with(|n| n.set(n.get() + 1));
}

/// Called at every charged host call.
pub(crate) fn host_call_witness() {
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
fn why<T: std::fmt::Debug>(r: Result<T, ImportError>) -> LuaWhy {
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

#[test]
fn every_pass_drops_its_vm_on_every_outcome() {
    let ok = folder("return { type = 0 }");
    let h = header(ok.path(), &Cancel::new()).unwrap();
    assert_eq!(live(), 0);
    h.import(&Config::defaults(h.header()), &Cancel::new())
        .unwrap();
    assert_eq!(live(), 0);
    let fails = folder("if skin_config then error(\"x\") end\nreturn { type = 0 }");
    let h = header(fails.path(), &Cancel::new()).unwrap();
    h.import(&Config::defaults(h.header()), &Cancel::new())
        .unwrap_err();
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
    match pass.ledger.take_fault() {
        Some(ImportError::Lua { why, .. }) => {
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
    let r = h.import(&Config::defaults(h.header()), &Cancel::new());
    let took = start.elapsed();
    // Without the carried time it would run its own 5 s.
    assert_eq!(why(r), LuaWhy::Limit(LuaLimit::Time));
    assert!(took < Duration::from_secs(3), "{took:?}");
}
