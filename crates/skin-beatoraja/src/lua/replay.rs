//! The reference executor's side of the sandbox (dev-tools; design.md
//! "beatoraja Lua Skins", the reference executor): what `lua_replay`
//! needs of an import that production does not keep.
//!
//! - [`LuaHeader::import_recorded`](super::LuaHeader::import_recorded):
//!   the production import, and what it decided for each consumer
//!   occurrence (destination, field, callable, disposition, program
//!   index), why each callable it read was left, and the destination
//!   ledger.
//! - [`LuaHeader::import_reference`](super::LuaHeader::import_reference):
//!   the same import with the configured pass's VM kept as a
//!   [`ReferenceVm`], whose host answers a play skin's reads from frame
//!   inputs the harness gives it, through the same property mapping the
//!   importer lowers reads with.
//!
//! Nothing here runs in an attempt: it exists only with `dev-tools`.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use mlua::{Function, Lua, MultiValue, Value};
use skin_model::{PLAY, Program, Scope, ScreenKind, State, TIMER_OFF, Ty, compile};

use super::callback::{Analysis, Unused, callable_nodes};
use super::runtime::Kept;
use super::{Disposition, HOST_APIS, LuaWhy, Pass};
use crate::import::ledger::DestinationEntry;
use crate::import::ops::{self, Cond};
use crate::import::timer::{Timer, builtin};
use crate::import::{IndexProp, IntProp, index_in, int_in, judge_count};
use crate::tree::Arena;

/// Where the configured pass keeps the returned skin table for the
/// reference, in the VM's registry.
pub(super) const SKIN_KEY: &str = "tapestry.replay.skin";

/// What production decided for one consumer occurrence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occurrence {
    /// The converted table's node holding the function.
    pub node: u32,
    /// The destination it was consumed for, by its index in the
    /// destination list.
    pub destination: Option<usize>,
    /// The field holding it (`value`, `draw`, `op`, `timer`, …).
    pub field: &'static str,
    /// The callable conversion gave the function.
    pub callable: u32,
    /// `lowered`, `compiled`, `replaced`, `runtime`, `refused`.
    pub disposition: &'static str,
    /// Why it did not lower or compile.
    pub reason: Option<String>,
    /// Its program callback, when compiled.
    pub program: Option<u16>,
    /// Its definition.
    pub file: String,
    pub line: u32,
}

/// What a production import recorded besides its outcome.
#[derive(Clone, Debug, Default)]
pub struct Recorded {
    /// Every consumer occurrence, in the order the import met them; a
    /// later record of one node supersedes an earlier one.
    pub occurrences: Vec<Occurrence>,
    /// Each callable read but left unconsumed, with why
    /// (`dropped`, `folded`), by callable.
    pub unused: BTreeMap<u32, &'static str>,
    /// What the import made of each destination.
    pub destinations: Vec<DestinationEntry>,
    /// The declared option items, each by its op, whether selected.
    pub items: BTreeMap<i32, bool>,
}

thread_local! {
    /// The next import keeps its VM for the reference.
    static WANT: Cell<bool> = const { Cell::new(false) };
    /// The next import records its occurrences.
    static RECORDING: Cell<bool> = const { Cell::new(false) };
    static KEPT: RefCell<Option<Kept>> = const { RefCell::new(None) };
    static LAST: RefCell<Recorded> = RefCell::new(Recorded::default());
}

/// Whether this thread's import keeps its VM for the reference.
pub(super) fn wanted() -> bool {
    WANT.with(Cell::get)
}

/// Keeps `kept`, the reference's VM.
pub(super) fn stash(kept: Kept) {
    KEPT.with(|k| *k.borrow_mut() = Some(kept));
}

/// Records the occurrences of `an` over `arena`, when recording.
pub(super) fn record(arena: &Arena, an: &Analysis) {
    if !RECORDING.with(Cell::get) {
        return;
    }
    let callable: HashMap<u32, u32> = callable_nodes(arena).collect();
    let occurrences = an
        .consumed
        .borrow()
        .iter()
        .map(|c| {
            let at = arena.loc(c.node);
            Occurrence {
                node: c.node,
                destination: c.element.as_ref().map(|e| e.index),
                field: c.field,
                callable: callable.get(&c.node).copied().unwrap_or(u32::MAX),
                disposition: label(&c.disposition),
                reason: c.why.as_ref().map(super::survey::unsupported),
                program: c.program,
                file: at.file,
                line: at.line,
            }
        })
        .collect();
    let unused = an
        .unused
        .borrow()
        .iter()
        .map(|(&c, u)| {
            (
                c,
                match u {
                    Unused::Dropped => "dropped",
                    Unused::FoldedFalse => "folded",
                },
            )
        })
        .collect();
    LAST.with(|l| {
        let mut l = l.borrow_mut();
        l.occurrences = occurrences;
        l.unused = unused;
    });
}

/// Records the declared option items, when recording.
pub(crate) fn record_items(items: &BTreeMap<i32, bool>) {
    if RECORDING.with(Cell::get) {
        LAST.with(|l| l.borrow_mut().items.clone_from(items));
    }
}

fn label(d: &Disposition) -> &'static str {
    match d {
        Disposition::Lowered => "lowered",
        Disposition::Compiled => "compiled",
        Disposition::Replaced(_) => "replaced",
        Disposition::Runtime => "runtime",
        Disposition::Refused => "refused",
        Disposition::ClassifiedOnly => "classified only",
        Disposition::Captured => "captured",
        Disposition::Inert => "inert",
        Disposition::NeverAnalysed => "never analysed",
    }
}

/// Runs `f`, an import, recording its occurrences.
pub(super) fn recording<T>(f: impl FnOnce() -> T) -> (T, Recorded) {
    RECORDING.with(|r| r.set(true));
    LAST.with(|l| *l.borrow_mut() = Recorded::default());
    let (out, destinations) = crate::import::ledger::recording(f);
    RECORDING.with(|r| r.set(false));
    let mut rec = LAST.with(RefCell::take);
    rec.destinations = destinations;
    (out, rec)
}

/// Runs `f`, an import, keeping its configured pass's VM.
pub(super) fn keeping<T>(f: impl FnOnce() -> T) -> (T, Option<Kept>) {
    WANT.with(|w| w.set(true));
    KEPT.with(|k| k.borrow_mut().take());
    let out = f();
    WANT.with(|w| w.set(false));
    (out, KEPT.with(|k| k.borrow_mut().take()))
}

/// What one reference invocation came to.
#[derive(Clone, Debug, PartialEq)]
pub enum Called {
    /// It returned: its first result.
    Value(Value),
    /// It raised a Lua error.
    Error,
    /// A read whose availability guard is false.
    Unavailable,
    /// A read the host cannot answer, or a host capability it does not
    /// have, by API name.
    Unanswered(&'static str),
    /// Past the harness's budget.
    Budget,
}

/// The frame inputs the play host answers from.
#[derive(Default)]
pub(crate) struct Frame {
    fields: Vec<i32>,
    timers: Vec<i32>,
    state_us: i64,
    perturb: BTreeMap<i32, i32>,
    /// Compiled reads by source text: `None` when it does not compile.
    cache: HashMap<String, Option<Program>>,
}

impl Frame {
    fn eval(&mut self, src: &str, ty: Ty) -> Option<i32> {
        let p = self
            .cache
            .entry(src.to_owned())
            .or_insert_with(|| compile(src, &Scope::new(&PLAY), ty).ok())
            .as_ref()?;
        Some(p.eval(&State {
            fields: &self.fields,
            timers: &self.timers,
        }))
    }

    /// A guarded integer: `Err(())` when the guard is false.
    fn guarded(&mut self, guard: Option<&str>, src: &str) -> Option<Result<i32, ()>> {
        if let Some(g) = guard
            && self.eval(g, Ty::Bool)? == 0
        {
            return Some(Err(()));
        }
        Some(Ok(self.eval(src, Ty::Int)?))
    }
}

/// A play skin's configured pass, kept: its VM, the returned skin table,
/// and each callable by function.
pub struct ReferenceVm {
    functions: Vec<Function>,
    by_ptr: HashMap<usize, u32>,
    skin: Value,
    pass: Arc<Pass>,
    lua: Lua,
}

impl std::fmt::Debug for ReferenceVm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReferenceVm")
            .field("functions", &self.functions.len())
            .finish_non_exhaustive()
    }
}

impl ReferenceVm {
    pub(super) fn new(kept: Kept) -> Self {
        let Kept {
            lua,
            pass,
            functions,
            #[cfg(feature = "dev-tools")]
                open: _,
        } = kept;
        pass.enter_runtime(ScreenKind::Play);
        *pass.replay.borrow_mut() = Some(Frame::default());
        let skin = lua.named_registry_value(SKIN_KEY).unwrap_or(Value::Nil);
        let by_ptr = (0_u32..)
            .zip(&functions)
            .map(|(c, f)| (f.to_pointer().addr(), c))
            .collect();
        Self {
            functions,
            by_ptr,
            skin,
            pass,
            lua,
        }
    }

    /// The VM.
    #[must_use]
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// The table the configured pass returned.
    #[must_use]
    pub fn skin(&self) -> &Value {
        &self.skin
    }

    /// The callable conversion gave function `f`, if it gave one.
    #[must_use]
    pub fn callable(&self, f: &Function) -> Option<u32> {
        self.by_ptr.get(&f.to_pointer().addr()).copied()
    }

    /// Adds `delta` to every later answer of `number(id)`: the mutation
    /// control.
    pub fn perturb(&self, id: i32, delta: i32) {
        if let Some(f) = self.pass.replay.borrow_mut().as_mut() {
            f.perturb.insert(id, delta);
        }
    }

    /// Sets the frame the host answers from: the play schema's fields,
    /// the timers' ms and the state clock.
    pub fn set_frame(&self, fields: &[i32], timers: &[i32], state_us: i64) {
        if let Some(f) = self.pass.replay.borrow_mut().as_mut() {
            f.fields.clear();
            f.fields.extend_from_slice(fields);
            f.timers.clear();
            f.timers.extend_from_slice(timers);
            f.state_us = state_us;
        }
    }

    /// Calls `f` with no arguments under a fresh budget.
    pub fn call(&self, f: &Function) -> Called {
        self.pass.ledger.begin_frame();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.call::<Value>(())));
        let out = match r {
            Ok(Ok(v)) => Called::Value(v),
            Ok(Err(e)) => self.why(&e),
            Err(_) => Called::Unanswered("panic"),
        };
        self.pass.ledger.clear_fault();
        let _ = self.pass.ledger.take_stop();
        out
    }

    fn why(&self, e: &mlua::Error) -> Called {
        if matches!(e, mlua::Error::MemoryError(_)) {
            return Called::Budget;
        }
        if !self.pass.ledger.faulted() {
            return Called::Error;
        }
        match self.pass.ledger.peek_why() {
            Some(LuaWhy::Unavailable { .. }) => Called::Unavailable,
            Some(LuaWhy::Limit(_) | LuaWhy::Cancelled) => Called::Budget,
            Some(LuaWhy::LiveState { api }) => Called::Unanswered(api_name(api)),
            _ => Called::Unanswered("host"),
        }
    }

    /// Integer property `id` (a number's `ref` or value id) on the frame:
    /// `None` when the mapping has no value for it, `Some(None)` while it
    /// is unavailable.
    #[must_use]
    pub fn number_property(&self, id: i32) -> Option<Option<i32>> {
        let IntProp::Shown(v) = int_in(ScreenKind::Play, id) else {
            return None;
        };
        let mut g = self.pass.replay.borrow_mut();
        let f = g.as_mut()?;
        let delta = f.perturb.get(&id).copied().unwrap_or(0);
        match f.guarded(v.availability(), v.expr())? {
            Ok(n) => Some(Some(n.saturating_add(delta))),
            Err(()) => Some(None),
        }
    }

    /// Image-index property `id` (an image group's or an image set's
    /// `ref`) on the frame: `None` when the mapping has no value for it.
    #[must_use]
    pub fn index_property(&self, id: i32) -> Option<i32> {
        let p = index_in(ScreenKind::Play, id);
        let src = p.selector()?;
        let mut g = self.pass.replay.borrow_mut();
        g.as_mut()?.eval(src, Ty::Int)
    }

    /// Numeric `op` id `op` on the frame: `Some(true)` or `Some(false)`
    /// for a boolean property or a declared option item, `None` when the
    /// model has no value for it. `items` are the declared option items.
    #[must_use]
    pub fn condition(&self, op: i32, items: &BTreeMap<i32, bool>) -> Option<bool> {
        match ops::numeric_in(ScreenKind::Play, op, items) {
            Cond::Fold(b) => Some(b),
            Cond::Expr { src, .. } => {
                let mut g = self.pass.replay.borrow_mut();
                g.as_mut()?.eval(&src, Ty::Bool).map(|v| v != 0)
            }
            Cond::Unsupported(_) => None,
            Cond::Undefined(_) => Some(false),
        }
    }

    /// Boolean property `name` (a `!` negating it) on the frame: `None`
    /// when the model has no value for it.
    #[must_use]
    pub fn named_condition(&self, name: &str) -> Option<bool> {
        match ops::named_in(ScreenKind::Play, name)? {
            Cond::Fold(b) => Some(b),
            Cond::Expr { src, .. } => {
                let mut g = self.pass.replay.borrow_mut();
                g.as_mut()?.eval(&src, Ty::Bool).map(|v| v != 0)
            }
            Cond::Unsupported(_) | Cond::Undefined(_) => None,
        }
    }

    /// Whether numeric op `op` (either sign) names a built-in boolean
    /// property: otherwise it is an option of the destination.
    #[must_use]
    pub fn is_property(op: i32) -> bool {
        i32::try_from(op.unsigned_abs()).is_ok_and(ops::is_builtin)
    }

    /// Whether built-in boolean property `op` (either sign) is static in a
    /// play state, so the source's installation evaluates it once.
    #[must_use]
    pub fn is_static(op: i32) -> bool {
        i32::try_from(op.unsigned_abs()).is_ok_and(|id| STATIC_IN_PLAY.binary_search(&id).is_ok())
    }
}

/// The boolean properties static in a play state (`isStatic` true):
/// `TYPE_STATIC_ALL`, `TYPE_STATIC_WITHOUT_MUSICSELECT`, the song-data,
/// trophy and course-stage properties (`BooleanPropertyFactory.java` at
/// `0.8.9`, its `BooleanType` entries); `TYPE_NO_STATIC` and
/// `TYPE_STATIC_ON_RESULT` are not. Sorted.
const STATIC_IN_PLAY: &[i32] = &[
    40, 41, 50, 51, 118, 119, 121, 125, 126, 127, 128, 129, 130, 131, 150, 151, 152, 153, 154, 155,
    160, 161, 162, 163, 164, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183,
    184, 190, 191, 192, 193, 194, 195, 280, 281, 282, 283, 289, 290, 1008, 1128, 1129, 1130, 1131,
    1160, 1161, 1177,
];

fn api_name(api: u16) -> &'static str {
    HOST_APIS.get(usize::from(api)).map_or("host", |&(_, n)| n)
}

/// The integer argument of a host read, when it is one.
fn arg(args: &MultiValue) -> Option<i64> {
    match args.front() {
        Some(Value::Integer(n)) => Some(*n),
        Some(Value::Number(f)) if f.fract() == 0.0 && f.abs() < 9.0e15 =>
        {
            #[expect(clippy::cast_possible_truncation, reason = "integral and within i64")]
            Some(*f as i64)
        }
        _ => None,
    }
}

/// A play reference's answer to host API `api` with `args`: `None` when
/// the pass is not a reference, so the host answers as it would.
pub(super) fn answer(
    lua: &Lua,
    pass: &Pass,
    api: u16,
    args: &MultiValue,
) -> Option<mlua::Result<Value>> {
    let mut g = pass.replay.borrow_mut();
    let f = g.as_mut()?;
    let name = api_name(api);
    let unanswered = || pass.ledger.fail_lua(lua, LuaWhy::LiveState { api });
    let unavailable = |id: i64| {
        pass.ledger.fail_lua(
            lua,
            LuaWhy::Unavailable {
                api,
                id: i32::try_from(id).unwrap_or(i32::MAX),
            },
        )
    };
    let int = |f: &mut Frame, guard: Option<&str>, src: &str, id: i64, delta: i32| match f
        .guarded(guard, src)
    {
        Some(Ok(n)) => Ok(Value::Number(f64::from(n.saturating_add(delta)))),
        Some(Err(())) => Err(unavailable(id)),
        None => Err(unanswered()),
    };
    Some(match name {
        "number" => {
            let Some(id) = arg(args) else {
                return Some(Err(unanswered()));
            };
            let n = i32::try_from(id).unwrap_or(i32::MAX);
            match int_in(ScreenKind::Play, n) {
                IntProp::Shown(v) => {
                    let delta = f.perturb.get(&n).copied().unwrap_or(0);
                    int(f, v.availability(), v.expr(), id, delta)
                }
                _ => Err(unanswered()),
            }
        }
        "judge" => {
            let Some(n) = arg(args) else {
                return Some(Err(unanswered()));
            };
            match judge_count(n) {
                Some(v) => int(f, v.availability(), v.expr(), n, 0),
                None => Ok(Value::Number(0.0)),
            }
        }
        "gauge_type" => match IndexProp::GaugeKind.selector() {
            Some(src) => int(f, Some("gauge.valid"), src, 0, 0),
            None => Err(unanswered()),
        },
        "option" => {
            let Some(id) = arg(args).and_then(|n| i32::try_from(n).ok()) else {
                return Some(Err(unanswered()));
            };
            match ops::numeric_in(ScreenKind::Play, id, &BTreeMap::new()) {
                Cond::Fold(b) => Ok(Value::Boolean(b)),
                Cond::Undefined(_) => Ok(Value::Boolean(false)),
                Cond::Expr { src, .. } => match f.eval(&src, Ty::Bool) {
                    Some(v) => Ok(Value::Boolean(v != 0)),
                    None => Err(unanswered()),
                },
                Cond::Unsupported(_) => Err(unanswered()),
            }
        }
        "time" => {
            #[expect(clippy::cast_precision_loss, reason = "the source's double")]
            let t = f.state_us as f64;
            Ok(Value::Number(t))
        }
        "timer" => {
            let Some(id) = arg(args).and_then(|n| i32::try_from(n).ok()) else {
                return Some(Err(unanswered()));
            };
            match builtin_timer(id) {
                Some(slot) => {
                    let ms = f.timers.get(slot).copied().unwrap_or(TIMER_OFF);
                    #[expect(clippy::cast_precision_loss, reason = "the source's double")]
                    let v = if ms == TIMER_OFF {
                        i64::MIN as f64
                    } else {
                        f.state_us
                            .saturating_sub(i64::from(ms).saturating_mul(1_000))
                            as f64
                    };
                    Ok(Value::Number(v))
                }
                None if id == 0 => {
                    #[expect(clippy::cast_precision_loss, reason = "the source's double")]
                    let off = i64::MIN as f64;
                    Ok(Value::Number(off))
                }
                None => Err(unanswered()),
            }
        }
        _ => Err(unanswered()),
    })
}

/// The built-in timer slot of timer id `id`, without custom timers.
fn builtin_timer(id: i32) -> Option<usize> {
    if id < 0 {
        return None;
    }
    match builtin(id) {
        Timer::Builtin(slot) => Some(usize::from(slot)),
        _ => None,
    }
}
