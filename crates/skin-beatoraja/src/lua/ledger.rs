//! A candidate's accounting and its sticky fatal state.
//!
//! One ledger serves one pass; its [`Counts`] carry over to the next pass
//! of the same candidate. The first fault (a cap, a cancellation, a
//! confinement refusal, a loader failure) is kept and every later charge,
//! instruction step and protected call raises it again, so a Lua
//! protected call cannot catch it or reset a budget.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mlua::Lua;

use super::LuaWhy;
use super::limits::{
    INSTRUCTION_STEP, MAX_HOST_CALLS, MAX_INSTRUCTIONS, MAX_LUA_TIME, MAX_NATIVE_WORK,
};
use crate::error::{ImportError, Loc};
use crate::lua::LuaLimit;

/// A cooperative cancellation request, shared with the thread that may
/// cancel: the import checks it between instruction steps, at every host
/// call, before and after each source read and compilation, and every
/// [`CHECK_EVERY`](super::convert::CHECK_EVERY) converted values. A
/// blocked read of the file system is not interrupted.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A request not yet made.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation; an import that sees it refuses with
    /// [`LuaWhy::Cancelled`].
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What a candidate has used, across its passes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LuaUsage {
    /// Instructions, counted in steps of
    /// [`INSTRUCTION_STEP`](super::limits::INSTRUCTION_STEP).
    pub instructions: u64,
    pub host_calls: u32,
    pub native_work: u64,
    /// `require` and `dofile` invocations.
    pub loads: u32,
    /// Source bytes read, a repeated read counted again.
    pub input_bytes: u64,
    /// The `require` invocations among `loads`.
    pub requires: u32,
    /// The `dofile` invocations among `loads`.
    pub dofiles: u32,
    /// Protected calls (`pcall` and `xpcall`).
    pub protected_calls: u32,
    /// `setmetatable` calls.
    pub metatables: u32,
    /// The most Lua memory a VM of the candidate held, in bytes, as
    /// sampled at each instruction step and when the entry returns: a
    /// library function's own peak between two samples is not seen.
    pub peak_memory: u64,
    /// The analysis's tree nodes
    /// ([`MAX_AST_NODES`](super::limits::MAX_AST_NODES)).
    pub ast_nodes: u64,
    /// Closure instances and values captured
    /// ([`MAX_CAPTURES`](super::limits::MAX_CAPTURES)).
    pub captures: u64,
    /// Expression nodes the lowering expanded
    /// ([`MAX_EXPANDED_NODES`](super::limits::MAX_EXPANDED_NODES)).
    pub expanded: u64,
}

/// The counts a pass starts from: the earlier passes' use.
pub(crate) type Counts = LuaUsage;

/// The error a fault raises in Lua: a marker, the fault itself stays in
/// the ledger.
#[derive(Debug)]
pub(crate) struct Fatal;

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the import was refused")
    }
}

impl std::error::Error for Fatal {}

/// One pass's accounting.
pub(crate) struct Ledger {
    counts: Cell<Counts>,
    fault: RefCell<Option<ImportError>>,
    faulted: Cell<bool>,
    cancel: Cancel,
    /// The time the candidate's earlier passes ran, and when this one
    /// started, for [`MAX_LUA_TIME`].
    spent: Duration,
    started: Instant,
    /// The time this pass ran Lua, once its execution ended.
    stopped: Cell<Option<Duration>>,
}

impl Ledger {
    pub(crate) fn new(counts: Counts, spent: Duration, cancel: Cancel) -> Self {
        Self {
            counts: Cell::new(counts),
            fault: RefCell::new(None),
            faulted: Cell::new(false),
            cancel,
            spent,
            started: Instant::now(),
            stopped: Cell::new(None),
        }
    }

    /// Ends this pass's Lua time: the conversion and the teardown after
    /// it are not Lua's.
    pub(crate) fn stop_clock(&self) {
        if self.stopped.get().is_none() {
            self.stopped.set(Some(self.started.elapsed()));
        }
    }

    /// The time the candidate's passes have run, this one's so far
    /// included.
    pub(crate) fn spent(&self) -> Duration {
        let this = self.stopped.get().unwrap_or_else(|| self.started.elapsed());
        self.spent.saturating_add(this)
    }

    pub(crate) fn counts(&self) -> Counts {
        self.counts.get()
    }

    pub(crate) fn update(&self, f: impl FnOnce(&mut Counts)) {
        let mut c = self.counts.get();
        f(&mut c);
        self.counts.set(c);
    }

    /// Whether a fault was recorded.
    pub(crate) fn faulted(&self) -> bool {
        self.faulted.get()
    }

    /// The recorded fault, taken.
    pub(crate) fn take_fault(&self) -> Option<ImportError> {
        self.fault.borrow_mut().take()
    }

    /// Records `e` unless a fault is already recorded, and returns the
    /// error that raises it in Lua.
    pub(crate) fn fail(&self, e: ImportError) -> mlua::Error {
        if !self.faulted.replace(true) {
            *self.fault.borrow_mut() = Some(e);
        }
        mlua::Error::external(Fatal)
    }

    /// [`Self::fail`] with a Lua refusal located where `lua` runs.
    pub(crate) fn fail_lua(&self, lua: &Lua, why: LuaWhy) -> mlua::Error {
        if self.faulted() {
            return mlua::Error::external(Fatal);
        }
        self.fail(ImportError::Lua {
            at: running(lua),
            why,
        })
    }

    /// Raises a recorded fault, or a cancellation now requested.
    pub(crate) fn check(&self, lua: &Lua) -> mlua::Result<()> {
        if self.faulted() {
            return Err(mlua::Error::external(Fatal));
        }
        if self.cancel.is_cancelled() {
            return Err(self.fail_lua(lua, LuaWhy::Cancelled));
        }
        if self.spent() > MAX_LUA_TIME {
            return Err(self.fail_lua(lua, LuaWhy::Limit(LuaLimit::Time)));
        }
        Ok(())
    }

    /// Whether cancellation was requested, outside Lua.
    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The instruction hook: one step of [`INSTRUCTION_STEP`] charged.
    pub(crate) fn step(&self, lua: &Lua) -> mlua::Result<()> {
        self.check(lua)?;
        let next = self
            .counts()
            .instructions
            .saturating_add(u64::from(INSTRUCTION_STEP));
        if next >= MAX_INSTRUCTIONS {
            return Err(self.fail_lua(lua, LuaWhy::Limit(LuaLimit::Instructions)));
        }
        self.update(|c| c.instructions = next);
        self.memory(lua.used_memory());
        Ok(())
    }

    /// One call into a host function, charged before its work.
    pub(crate) fn host_call(&self, lua: &Lua) -> mlua::Result<()> {
        self.check(lua)?;
        let calls = self.counts().host_calls;
        if calls >= MAX_HOST_CALLS {
            return Err(self.fail_lua(lua, LuaWhy::Limit(LuaLimit::HostCalls)));
        }
        self.update(|c| c.host_calls = calls + 1);
        #[cfg(test)]
        super::tests::host_call_witness();
        Ok(())
    }

    /// `work` units of native library work, charged before the call.
    pub(crate) fn native(&self, lua: &Lua, work: u64) -> mlua::Result<()> {
        let next = self.counts().native_work.saturating_add(work);
        if next > MAX_NATIVE_WORK {
            return Err(self.fail_lua(lua, LuaWhy::Limit(LuaLimit::NativeWork)));
        }
        self.update(|c| c.native_work = next);
        Ok(())
    }

    /// Records the VM's memory in use as a candidate for the peak.
    pub(crate) fn memory(&self, used: usize) {
        let used = u64::try_from(used).unwrap_or(u64::MAX);
        self.update(|c| c.peak_memory = c.peak_memory.max(used));
    }
}

/// How far up the stack [`running`] looks for a Lua function: past the
/// deepest the library's matcher recurses (its 200 levels, each a few
/// frames).
const LOOK_UP: usize = 2_048;

/// Where `lua` runs: the innermost Lua function with a current line, by
/// its module's name. The chunks are named `@` and their skin-relative
/// name; the sandbox's own chunks are named `=` and are skipped.
pub(crate) fn running(lua: &Lua) -> Option<Loc> {
    (0..LOOK_UP)
        .map_while(|level| {
            lua.inspect_stack(level, |d| {
                let line = d.current_line()?;
                let source = d.source().source?;
                let name = source.strip_prefix('@')?.to_owned();
                Some(Loc {
                    file: name,
                    line: u32::try_from(line).unwrap_or(u32::MAX),
                    col: 0,
                })
            })
        })
        .flatten()
        .next()
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    use super::{Cancel, Counts, Ledger, LuaWhy};
    use crate::error::ImportError;
    use crate::lua::LuaLimit;

    #[test]
    fn a_fault_refuses_every_later_charge_and_keeps_the_first() {
        let lua = Lua::new();
        let counts = Counts {
            instructions: 5_000,
            host_calls: 7,
            native_work: 11,
            ..Counts::default()
        };
        let ledger = Ledger::new(counts, std::time::Duration::ZERO, Cancel::new());
        ledger.check(&lua).unwrap();
        let _ = ledger.fail_lua(&lua, LuaWhy::Finalizer);
        let _ = ledger.fail_lua(&lua, LuaWhy::Limit(LuaLimit::Memory));
        // Each later charge is refused at once and counts nothing.
        assert!(ledger.check(&lua).is_err());
        assert!(ledger.host_call(&lua).is_err());
        assert!(ledger.step(&lua).is_err());
        assert_eq!(ledger.counts(), counts);
        // A cancellation after the fault does not replace it.
        let cancel = Cancel::new();
        let ledger2 = Ledger::new(counts, std::time::Duration::ZERO, cancel.clone());
        let _ = ledger2.fail_lua(&lua, LuaWhy::Finalizer);
        cancel.cancel();
        assert!(ledger2.check(&lua).is_err());
        for l in [&ledger, &ledger2] {
            match l.take_fault() {
                Some(ImportError::Lua { why, .. }) => assert_eq!(why, LuaWhy::Finalizer),
                other => panic!("{other:?}"),
            }
        }
    }
}
