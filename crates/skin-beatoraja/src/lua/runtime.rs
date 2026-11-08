//! The bounded runtime of a selection or result skin (docs/stage3.md
//! S7-07; design.md "beatoraja Lua Skins", the runtime): the configured
//! pass's VM, kept, running each frame the callbacks no expression
//! represents, its host modules answering from the screen's state.
//!
//! - **Phase.** Keeping the VM sets its host context's phase to runtime,
//!   irreversibly ([`super::Pass::enter_runtime`]): every loader and
//!   import-only capability then raises the fault marker, however a
//!   callback reached it (a global, an upvalue, a table field, an alias).
//! - **Reads.** `main_state.number` and `option` answer through the
//!   screen profile's bridge over the frame's fields, compiled once per
//!   id; an id the profile omits answers the source's value in the
//!   feature's absence ([`absent`]), else the typed constant, with one note
//!   per id. Numbers come back as doubles, as the source's LuaJ holds
//!   them.
//! - **Frame.** Each callback is called in import order, after its gate
//!   (the conditions before it in its element's `dstdraw`, evaluated on
//!   the same frame's fields); its result is converted as the source's
//!   consumer converts it ([`to_int`], [`to_bool`]) and written to its
//!   output slot. Every slot is written every frame.
//! - **Budgets.** One frame's callbacks share
//!   [`limits::RUNTIME_INSTRUCTIONS`], [`limits::RUNTIME_HOST_CALLS`] and
//!   [`limits::RUNTIME_FRAME_TIME`]; the VM's memory is capped at
//!   [`limits::RUNTIME_MEMORY`]. A call past the budget, and every later
//!   call that frame, gives its constant; after
//!   [`limits::RUNTIME_STOP_FRAMES`] such frames in a row, the memory cap,
//!   or a panic, the runtime stops, every slot its constant from then on.
//! - **Failures.** A callback that raises an ordinary error gives the
//!   source's value (0, false), as the source's accessor does, and is
//!   called again the next frame.
//! - **Notes.** At most [`crate::limits::MAX_NOTES`] notes are kept;
//!   further occurrences are counted.
//! - **Permits.** A screen that bounds its kept VMs gives each runtime a
//!   [`Permit`] from its [`Pool`]; the runtime returns it when dropped,
//!   after its VM is destroyed, on whichever thread drops it.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use mlua::{Function, Lua, Value};
use skin_model::output::OutTy;
use skin_model::{Program, RESULT, SELECTION, Scope, ScreenKind, State, Ty, compile};

use super::ledger::Stop;
use super::{LuaLimit, LuaWhy, Pass, limits};
use crate::error::ImportError;
use crate::import::result::profile as res;
use crate::import::selection::profile as sel;

/// A value hidden from its number: the source's `Integer.MIN_VALUE`.
const HIDDEN: i32 = i32::MIN;

/// What the configured pass kept for a runtime: the VM, its host context
/// and each callable of the converted table, by callable index.
pub(crate) struct Kept {
    pub(crate) lua: Lua,
    pub(crate) pass: Arc<Pass>,
    pub(crate) functions: Vec<Function>,
    /// The witness that this VM is live (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub(crate) open: Option<super::vm_hooks::Open>,
}

impl Kept {
    /// Drops the VM, unused: no callback needed it.
    pub(crate) fn close(self) {
        #[cfg(feature = "dev-tools")]
        super::vm_hooks::closing();
        drop(self);
    }
}

impl std::fmt::Debug for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kept")
            .field("functions", &self.functions.len())
            .finish_non_exhaustive()
    }
}

/// One callback the runtime runs: the callable's index in the converted
/// table, its output's type and its gate.
#[derive(Debug)]
pub(crate) struct Site {
    pub(crate) callable: u32,
    pub(crate) ty: OutTy,
    /// The conditions before it in its element's `dstdraw`, which the
    /// source evaluates first, stopping at the first false.
    pub(crate) gate: Option<Program>,
}

/// What became of a callback this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallStatus {
    /// Returned a value.
    Ok,
    /// Raised an ordinary error: the source's value, 0 or false.
    Error,
    /// Not called: its gate was false.
    Skipped,
    /// Not run, or stopped, past the frame's budget.
    Budget,
    /// The runtime has stopped.
    Stopped,
}

/// Why a runtime stopped for its installation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// [`limits::RUNTIME_STOP_FRAMES`] frames past the budget in a row.
    Budget,
    /// The VM's memory cap.
    Memory,
    /// A panic inside a call.
    Panic,
}

/// A note of the runtime: a callback's first error, an omitted id's first
/// read, or the stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeNote {
    /// Callback `site` raised an ordinary error.
    CallbackError { site: usize },
    /// A host read of an id the profile omits: `api` (`number` or
    /// `option`) and the id, answered with `value`.
    Absent {
        api: &'static str,
        id: i32,
        value: i32,
    },
    /// An import-only capability called at runtime.
    ImportOnly,
    /// The runtime stopped.
    Stopped(Stopped),
}

/// The runtime's notes: at most [`crate::limits::MAX_NOTES`], each once;
/// further occurrences counted.
#[derive(Debug, Default)]
pub(crate) struct Notes {
    list: Vec<RuntimeNote>,
    seen: HashSet<RuntimeNote>,
    dropped: u64,
}

impl Notes {
    pub(crate) fn add(&mut self, note: RuntimeNote) {
        if self.seen.contains(&note) {
            return;
        }
        if self.list.len() >= crate::limits::MAX_NOTES {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.seen.insert(note.clone());
        self.list.push(note);
    }
}

impl std::hash::Hash for RuntimeNote {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::CallbackError { site } => site.hash(state),
            Self::Absent { api, id, .. } => (api, id).hash(state),
            Self::ImportOnly => {}
            Self::Stopped(s) => (*s as u8).hash(state),
        }
    }
}

/// A host read's bridge, compiled once per id: its guard and its value.
struct Bridged {
    guard: Option<Program>,
    value: Program,
}

/// The screen a runtime frame reads: its kind, a copy of its fields and
/// timer readings, and the compiled bridges by id.
pub(crate) struct Screen {
    kind: ScreenKind,
    fields: Vec<i32>,
    timers: Vec<i32>,
    numbers: HashMap<i32, Option<Bridged>>,
    options: HashMap<i32, Option<Bridged>>,
}

impl Screen {
    pub(crate) fn new(kind: ScreenKind) -> Self {
        Self {
            kind,
            fields: Vec::new(),
            timers: Vec::new(),
            numbers: HashMap::new(),
            options: HashMap::new(),
        }
    }

    pub(crate) const fn kind(&self) -> ScreenKind {
        self.kind
    }

    fn state(&self) -> State<'_> {
        State {
            fields: &self.fields,
            timers: &self.timers,
        }
    }

    fn compile(&self, src: &str, ty: Ty) -> Option<Program> {
        let schema = match self.kind {
            ScreenKind::Selection => &SELECTION,
            ScreenKind::Result => &RESULT,
            ScreenKind::Play | ScreenKind::Decision => return None,
        };
        compile(src, &Scope::new(schema), ty).ok()
    }

    /// `main_state.number(id)`: the bridge's value, `HIDDEN` while its
    /// guard is false; `None` for an id the profile does not bridge.
    pub(crate) fn number(&mut self, id: i32) -> Option<i32> {
        if !self.numbers.contains_key(&id) {
            let src = match self.kind {
                ScreenKind::Selection => match sel::number(id) {
                    Some(sel::Class::Bridge(v)) => Some((v.availability(), v.expr())),
                    _ => None,
                },
                ScreenKind::Result => match res::number(id) {
                    Some(sel::Class::Bridge(v)) => Some((v.guard, v.expr)),
                    _ => None,
                },
                ScreenKind::Play | ScreenKind::Decision => None,
            };
            let b = src.and_then(|(guard, expr)| {
                Some(Bridged {
                    guard: Some(self.compile(guard, Ty::Bool)?),
                    value: self.compile(expr, Ty::Int)?,
                })
            });
            self.numbers.insert(id, b);
        }
        let b = self.numbers.get(&id)?.as_ref()?;
        let st = self.state();
        Some(if b.guard.as_ref().is_none_or(|g| g.eval_bool(&st)) {
            b.value.eval(&st)
        } else {
            HIDDEN
        })
    }

    /// `main_state.option(id)` for a positive `id`: the bridge's value,
    /// false while its guard is false; `None` for an id the profile does
    /// not bridge.
    pub(crate) fn option(&mut self, id: i32) -> Option<bool> {
        if !self.options.contains_key(&id) {
            let b = match self.kind {
                ScreenKind::Selection => match sel::option(id) {
                    Some(sel::Class::Bridge(sel::SelOption::Expr(src))) => self
                        .compile(src, Ty::Bool)
                        .map(|value| Bridged { guard: None, value }),
                    Some(sel::Class::Bridge(sel::SelOption::Constant(b))) => self
                        .compile(if b { "true" } else { "false" }, Ty::Bool)
                        .map(|value| Bridged { guard: None, value }),
                    _ => None,
                },
                ScreenKind::Result => match res::option(id) {
                    Some(sel::Class::Bridge(res::ResOption::Expr(src))) => {
                        let guard = self.compile(res::option_guard(id), Ty::Bool);
                        self.compile(src, Ty::Bool)
                            .map(|value| Bridged { guard, value })
                    }
                    Some(sel::Class::Bridge(res::ResOption::Constant(b))) => self
                        .compile(if b { "true" } else { "false" }, Ty::Bool)
                        .map(|value| Bridged { guard: None, value }),
                    _ => None,
                },
                ScreenKind::Play | ScreenKind::Decision => None,
            };
            self.options.insert(id, b);
        }
        let b = self.options.get(&id)?.as_ref()?;
        let st = self.state();
        Some(b.guard.as_ref().is_none_or(|g| g.eval_bool(&st)) && b.value.eval_bool(&st))
    }
}

/// The source's value of number `id` on a `kind` screen when the feature
/// it reads is absent, where the pinned source defines one (owner
/// decision O8; docs/stage3.md S7-07). `None`: no such value, the typed
/// constant applies.
///
/// - 410-419, a judgment's early or late count, and 420-426, the miss,
///   early and late miss, total early, total late, combo break and poor
///   plus miss counts: 0 with no score (`IntegerPropertyFactory.java:516-519`,
///   `:444-463`, over `MainState.getJudgeCount`, `MainState.java:141-143`).
/// - 271, the rival's EX score: 0 with no rival
///   (`ScoreDataProperty.java:55-58`, `:261`).
/// - 283 and 284, the rival's judgment counts: `Integer.MIN_VALUE` with no
///   rival (`IntegerPropertyFactory.java:521-525`).
/// - 380-389, an IR ranking's EX score, and 390-399, its index:
///   `Integer.MIN_VALUE` with no ranking data
///   (`IntegerPropertyFactory.java:876-905`).
#[must_use]
pub fn absent(kind: ScreenKind, id: i32) -> Option<i32> {
    match (kind, id) {
        (ScreenKind::Selection | ScreenKind::Result, 410..=426) => Some(0),
        (ScreenKind::Selection, 271) => Some(0),
        (ScreenKind::Selection, 283 | 284) => Some(HIDDEN),
        (ScreenKind::Selection | ScreenKind::Result, 380..=399) => Some(HIDDEN),
        _ => None,
    }
}

/// A Lua value as the source's `toint` converts a number's value
/// (`SkinLuaAccessor.java:644-650`, LuaJ 3.0.2): a number by `d2l; l2i`;
/// a numeral string as its number; anything else 0.
///
/// The play presentation program (S7-06d) has the same conversion in its
/// numerics; the second of the two to land keeps one shared function.
#[must_use]
pub fn to_int(v: &Value) -> i32 {
    match v {
        Value::Integer(i) => d2l_l2i(*i as f64),
        Value::Number(n) => d2l_l2i(*n),
        Value::String(s) => s
            .to_str()
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map_or(0, d2l_l2i),
        _ => 0,
    }
}

/// Java's `(int) (long) d`: NaN 0, the long saturating, then its low 32
/// bits.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the source's l2i keeps the low 32 bits"
)]
fn d2l_l2i(d: f64) -> i32 {
    // `as` from f64 to i64 saturates and maps NaN to 0, as Java's d2l.
    (d as i64) as i32
}

/// A Lua value as the source's `toboolean` converts a draw: nil and false
/// are false, everything else true.
#[must_use]
pub const fn to_bool(v: &Value) -> bool {
    !matches!(v, Value::Nil | Value::Boolean(false))
}

/// A bound on the VMs a screen keeps: `n` permits, each held by one VM
/// and returned exactly once, when that VM is destroyed. Clones share the
/// pool.
#[derive(Clone, Debug)]
pub struct Pool {
    inner: Arc<PoolInner>,
}

#[derive(Debug)]
struct PoolInner {
    state: Mutex<PoolState>,
    /// The returns so far: a waiting request retries when it changes.
    released: AtomicU64,
}

#[derive(Debug)]
struct PoolState {
    free: usize,
    next: u64,
    /// The ids handed out and not yet returned.
    out: Vec<u64>,
}

impl Pool {
    /// A pool of `n` permits.
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                state: Mutex::new(PoolState {
                    free: n,
                    next: 0,
                    out: Vec::with_capacity(n),
                }),
                released: AtomicU64::new(0),
            }),
        }
    }

    /// A permit, if one is free.
    #[must_use]
    pub fn take(&self) -> Option<Permit> {
        let mut st = self.inner.lock();
        if st.free == 0 {
            return None;
        }
        st.free -= 1;
        let id = st.next;
        st.next = st.next.wrapping_add(1);
        st.out.push(id);
        Some(Permit {
            id,
            pool: Arc::clone(&self.inner),
        })
    }

    /// The permits free now.
    #[must_use]
    pub fn free(&self) -> usize {
        self.inner.lock().free
    }

    /// How many permits have been returned: a request waiting for one
    /// retries when this changes.
    #[must_use]
    pub fn releases(&self) -> u64 {
        self.inner.released.load(Ordering::Acquire)
    }
}

impl PoolInner {
    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        // The state is a count and a list, each update complete before
        // anything can panic: a poisoned lock holds a consistent state.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns permit `id`; a return of an id not outstanding frees
    /// nothing.
    fn give_back(&self, id: u64) -> bool {
        let mut st = self.lock();
        let Some(at) = st.out.iter().position(|o| *o == id) else {
            return false;
        };
        st.out.swap_remove(at);
        st.free += 1;
        drop(st);
        self.released.fetch_add(1, Ordering::AcqRel);
        true
    }
}

/// One of a [`Pool`]'s permits, held by the VM it admits; dropping it
/// returns it.
#[derive(Debug)]
pub struct Permit {
    id: u64,
    pool: Arc<PoolInner>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let _ = self.pool.give_back(self.id);
    }
}

/// The runtime of one selection or result skin.
///
/// Its fields drop in order: the callbacks' functions and the VM before
/// its permit, so a permit returns only once the VM is destroyed.
pub struct ScreenRuntime {
    sites: Vec<(Site, Function)>,
    /// The VM, held so its functions stay callable.
    _lua: Lua,
    pass: Arc<Pass>,
    status: Vec<CallStatus>,
    stopped: Option<Stopped>,
    over_budget: u32,
    calls: u64,
    #[cfg(feature = "dev-tools")]
    _open: Option<super::vm_hooks::Open>,
    permit: Option<Permit>,
}

impl std::fmt::Debug for ScreenRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenRuntime")
            .field("sites", &self.sites.len())
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

impl Drop for ScreenRuntime {
    fn drop(&mut self) {
        #[cfg(feature = "dev-tools")]
        super::vm_hooks::closing();
    }
}

/// What the instruction steps, host calls and calls of a runtime came to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub calls: u64,
    pub instructions: u64,
    pub host_calls: u64,
}

impl ScreenRuntime {
    /// The runtime of `kept` for the callbacks `sites` on a `kind` screen,
    /// its phase entered and its memory cap lowered; `None` when no
    /// callback needs it, the VM dropped.
    pub(crate) fn new(kind: ScreenKind, kept: Kept, sites: Vec<Site>) -> Option<Self> {
        if sites.is_empty() {
            kept.close();
            return None;
        }
        let Kept {
            lua,
            pass,
            functions,
            #[cfg(feature = "dev-tools")]
            open,
        } = kept;
        pass.enter_runtime(kind);
        // A lowered cap only refuses later allocations: the heap the
        // configured pass left stays.
        let _ = lua.set_memory_limit(limits::RUNTIME_MEMORY);
        let sites: Vec<(Site, Function)> = sites
            .into_iter()
            .filter_map(|s| {
                let f = functions.get(s.callable as usize)?.clone();
                Some((s, f))
            })
            .collect();
        let status = vec![CallStatus::Skipped; sites.len()];
        Some(Self {
            sites,
            _lua: lua,
            pass,
            status,
            stopped: None,
            over_budget: 0,
            calls: 0,
            #[cfg(feature = "dev-tools")]
            _open: open,
            permit: None,
        })
    }

    /// The runtime holding `permit`, returned when it is dropped, after
    /// its VM.
    #[must_use]
    pub fn with_permit(mut self, permit: Permit) -> Self {
        self.permit = Some(permit);
        self
    }

    /// The callbacks it runs: the outputs a state carries for it.
    #[must_use]
    pub fn outputs(&self) -> usize {
        self.sites.len()
    }

    /// Why it stopped, if it did.
    #[must_use]
    pub fn stopped(&self) -> Option<Stopped> {
        self.stopped
    }

    /// Each callback's status in the last frame.
    #[must_use]
    pub fn status(&self) -> &[CallStatus] {
        &self.status
    }

    /// The notes so far, and how many more were dropped past the cap.
    #[must_use]
    pub fn notes(&self) -> (Vec<RuntimeNote>, u64) {
        let n = self.pass.runtime_notes.borrow();
        (n.list.clone(), n.dropped)
    }

    /// The calls, instruction steps and host calls so far.
    #[must_use]
    pub fn counters(&self) -> Counters {
        let c = self.pass.ledger.counts();
        Counters {
            calls: self.calls,
            instructions: c.instructions,
            host_calls: u64::from(c.host_calls),
        }
    }

    /// Runs one frame over a state's `fields` (the schema's, then the
    /// outputs, which it writes) and `timers`.
    pub fn frame(&mut self, fields: &mut [i32], timers: &[i32]) {
        let n = self.sites.len();
        let base = fields.len().saturating_sub(skin_model::output::slots(n));
        // Every slot finalized: its constant until its callback runs.
        for (k, (site, _)) in self.sites.iter().enumerate() {
            write(fields, base, k, constant(site.ty));
        }
        if self.stopped.is_some() {
            self.status.fill(CallStatus::Stopped);
            return;
        }
        {
            let mut screen = self.pass.screen.borrow_mut();
            if let Some(s) = screen.as_mut() {
                s.fields.clear();
                s.fields
                    .extend_from_slice(fields.get(..base).unwrap_or_default());
                s.timers.clear();
                s.timers.extend_from_slice(timers);
            }
        }
        self.pass.ledger.begin_frame();
        let mut out_of_budget = false;
        for k in 0..n {
            let Some((site, f)) = self.sites.get(k) else {
                break;
            };
            let status = if out_of_budget {
                CallStatus::Budget
            } else if site.gate.as_ref().is_some_and(|g| {
                !g.eval_bool(&State {
                    fields: &*fields,
                    timers,
                })
            }) {
                CallStatus::Skipped
            } else {
                self.calls = self.calls.saturating_add(1);
                let r = std::panic::catch_unwind(AssertUnwindSafe(|| f.call::<Value>(())));
                match r {
                    Ok(Ok(v)) => {
                        let out = match site.ty {
                            OutTy::Bool => i32::from(to_bool(&v)),
                            OutTy::Int => to_int(&v),
                        };
                        write(fields, base, k, out);
                        CallStatus::Ok
                    }
                    Ok(Err(e)) => match self.why(&e) {
                        Why::Budget => {
                            out_of_budget = true;
                            CallStatus::Budget
                        }
                        Why::Memory => {
                            self.stop(Stopped::Memory);
                            CallStatus::Stopped
                        }
                        Why::ImportOnly => {
                            self.note(RuntimeNote::ImportOnly);
                            self.note(RuntimeNote::CallbackError { site: k });
                            CallStatus::Error
                        }
                        Why::Error => {
                            self.note(RuntimeNote::CallbackError { site: k });
                            CallStatus::Error
                        }
                    },
                    Err(_) => {
                        self.stop(Stopped::Panic);
                        CallStatus::Stopped
                    }
                }
            };
            // A call's fault is its own: the next call runs.
            self.pass.ledger.clear_fault();
            if let Some(s) = self.status.get_mut(k) {
                *s = status;
            }
            if self.stopped.is_some() {
                break;
            }
        }
        // The frame's stop, if any, is the frame's alone.
        let _ = self.pass.ledger.take_stop();
        if self.stopped.is_some() {
            for (k, (site, _)) in self.sites.iter().enumerate() {
                write(fields, base, k, constant(site.ty));
            }
            self.status.fill(CallStatus::Stopped);
            return;
        }
        if out_of_budget {
            self.over_budget = self.over_budget.saturating_add(1);
            if self.over_budget >= limits::RUNTIME_STOP_FRAMES {
                self.stop(Stopped::Budget);
            }
        } else {
            self.over_budget = 0;
        }
    }

    fn note(&self, note: RuntimeNote) {
        self.pass.runtime_notes.borrow_mut().add(note);
    }

    fn stop(&mut self, why: Stopped) {
        self.stopped = Some(why);
        self.note(RuntimeNote::Stopped(why));
    }

    /// What a call's error was: the frame's budget, the memory cap, an
    /// import-only capability, or an ordinary error.
    fn why(&self, e: &mlua::Error) -> Why {
        if matches!(e, mlua::Error::MemoryError(_)) {
            return Why::Memory;
        }
        // The ledger's fault, raised as its marker: its stop says why.
        match self.pass.ledger.peek_why() {
            Some(LuaWhy::Limit(LuaLimit::Memory)) => Why::Memory,
            Some(LuaWhy::Limit(_)) => Why::Budget,
            Some(LuaWhy::ImportOnly) => Why::ImportOnly,
            _ => Why::Error,
        }
    }
}

enum Why {
    Budget,
    Memory,
    ImportOnly,
    Error,
}

/// A callback's constant: false for a draw, 0 for a value.
const fn constant(_ty: OutTy) -> i32 {
    0
}

/// Writes callback `k`'s value and its `ok` flag (always 1: an output is
/// available whatever became of its call).
fn write(fields: &mut [i32], base: usize, k: usize, v: i32) {
    let at = base.saturating_add(k.saturating_mul(2));
    if let Some(s) = fields.get_mut(at) {
        *s = v;
    }
    if let Some(s) = fields.get_mut(at.saturating_add(1)) {
        *s = 1;
    }
}

/// A fault's reason, peeked, for a stop recorded in the ledger.
pub(crate) fn stop_why(stop: Option<&Stop>) -> Option<LuaWhy> {
    match stop {
        Some(Stop::Fault(ImportError::Lua { why, .. })) => Some(*why),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use mlua::{Lua, Value};

    use super::{Pool, to_bool, to_int};

    /// A permit returns exactly once, by its drop, on any thread; a return
    /// of an id not outstanding frees nothing.
    #[test]
    fn permits_return_once() {
        let pool = Pool::new(2);
        let a = pool.take().unwrap();
        let b = pool.take().unwrap();
        assert!(pool.take().is_none());
        assert_eq!(pool.releases(), 0);
        std::thread::spawn(move || drop(a)).join().unwrap();
        assert_eq!((pool.free(), pool.releases()), (1, 1));
        assert!(!pool.inner.give_back(b.id.wrapping_add(7)));
        let id = b.id;
        drop(b);
        assert!(!pool.inner.give_back(id));
        assert_eq!((pool.free(), pool.releases()), (2, 2));
        let c = pool.take().unwrap();
        assert_ne!(c.id, id);
    }

    /// C12: the source's `toint` (`d2l; l2i`) and `toboolean`.
    #[test]
    fn results_convert_as_the_source_does() {
        let lua = Lua::new();
        let s = |t: &str| Value::String(lua.create_string(t).unwrap());
        for (v, want) in [
            (Value::Number(4.5), 4),
            (Value::Number(-4.5), -4),
            (Value::Number(-0.0), 0),
            (Value::Number(4_294_967_296.0), 0),
            (Value::Number(2_147_483_648.0), i32::MIN),
            (Value::Number(-2_147_483_649.0), i32::MAX),
            (Value::Number(f64::INFINITY), -1),
            (Value::Number(f64::NEG_INFINITY), 0),
            (Value::Number(f64::NAN), 0),
            (Value::Integer(7), 7),
            (Value::Integer(1 << 32), 0),
            (s("12"), 12),
            (s(" 2.5 "), 2),
            (s("x"), 0),
            (Value::Boolean(true), 0),
            (Value::Nil, 0),
        ] {
            assert_eq!(to_int(&v), want, "{v:?}");
        }
        for (v, want) in [
            (Value::Nil, false),
            (Value::Boolean(false), false),
            (Value::Boolean(true), true),
            (Value::Number(0.0), true),
            (Value::Integer(0), true),
            (s(""), true),
        ] {
            assert_eq!(to_bool(&v), want, "{v:?}");
        }
    }
}
