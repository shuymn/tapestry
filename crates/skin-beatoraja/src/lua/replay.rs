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
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    /// Its position in its `op` list for an `op` entry, 0 otherwise.
    pub index: u32,
    /// The nested part it belongs to, by object path under the
    /// destination's definition (`def.images.2`); empty for the
    /// destination itself.
    pub path: String,
    /// The callable conversion gave the function.
    pub callable: u32,
    /// `lowered`, `compiled`, `replaced`, `runtime`, `refused`.
    pub disposition: &'static str,
    /// Why it did not lower or compile.
    pub reason: Option<String>,
    /// Its program callback, when compiled.
    pub program: Option<u16>,
    /// What it lowered to, when it lowered: the expression the import
    /// kept for this occurrence (s7-06e-3-design.md §2).
    pub lowered: Option<LoweredRecord>,
    /// Its definition.
    pub file: String,
    pub line: u32,
}

/// A lowered occurrence's expression: its source over the play schema and
/// the availability guards of its reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoweredRecord {
    pub src: String,
    pub guards: Vec<&'static str>,
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
    /// What production's admission found of each source it reached, by
    /// source id: immutable for the import (s7-06e-design.md §3.8).
    pub sources: BTreeMap<String, SourceFact>,
}

/// What production's admission found of a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFact {
    /// Its file loads.
    Loaded,
    /// Its file is missing or does not decode.
    Missing,
    /// A movie (by its extension).
    Movie,
    /// Refused by this importer before it was read (a glob).
    Unknown,
}

/// Whether this thread's import is recording.
pub(crate) fn is_recording() -> bool {
    RECORDING.with(Cell::get)
}

/// Whether a fact of source `src` is recorded.
pub(crate) fn has_source(src: &str) -> bool {
    LAST.with(|l| l.borrow().sources.contains_key(src))
}

/// Records what admission found of source `src`, when recording; a later
/// fact of a source keeps an earlier `Loaded`.
pub(crate) fn record_source(src: &str, fact: SourceFact) {
    if RECORDING.with(Cell::get) {
        LAST.with(|l| {
            let mut l = l.borrow_mut();
            let e = l.sources.entry(src.to_owned()).or_insert(fact);
            if *e != SourceFact::Loaded {
                *e = fact;
            }
        });
    }
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
                index: c.index,
                path: c.path.clone(),
                callable: callable.get(&c.node).copied().unwrap_or(u32::MAX),
                disposition: label(&c.disposition),
                reason: c.why.as_ref().map(super::survey::unsupported),
                program: c.program,
                lowered: c.lowered.as_ref().map(|l| LoweredRecord {
                    src: l.src.clone(),
                    guards: l.guards.clone(),
                }),
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
                    Unused::Removed => "removed",
                    Unused::Inert => "inert",
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
    /// `number` ids answered as unanswered whatever the host (a test's
    /// mutation control).
    deny: BTreeSet<i32>,
    /// On a play frame, whether the play state has no score data yet: the
    /// EX score and the score rates then read `Integer.MIN_VALUE`, as the
    /// source's do (s7-06e-3-design.md §2a).
    no_score: bool,
    /// On a frame production builds no play frame for (loading, the
    /// practice page), the fields its host answers, by slot; `None` on a
    /// play frame. A read loading any other field, an output or a timer
    /// operand, its availability guard's included, is unanswered there,
    /// and so is `timer`.
    allowed: Option<Vec<bool>>,
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
        if let Some(allowed) = &self.allowed
            && !p.loads_only(&|slot| allowed.get(usize::from(slot)).copied().unwrap_or(false))
        {
            return None;
        }
        Some(p.eval(&State {
            fields: &self.fields,
            timers: &self.timers,
        }))
    }

    /// An integer property read through its mapped expression `src` under
    /// `guard`, plus `delta` (a mutation control): `Integer.MIN_VALUE`
    /// for a score property (`sentinel`) while the play state has no
    /// score data, as the source's factory answers (s7-06e-3-design.md
    /// §2a). `Err(())` when the guard is false, `None` when unanswered.
    fn property(
        &mut self,
        sentinel: bool,
        guard: Option<&str>,
        src: &str,
        delta: i32,
    ) -> Option<Result<i32, ()>> {
        if sentinel && self.no_score {
            return Some(Ok(i32::MIN));
        }
        Some(self.guarded(guard, src)?.map(|n| n.saturating_add(delta)))
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
        self.set_play(fields, timers, state_us, None);
    }

    /// [`Self::set_frame`] with the play state's score data, as the source
    /// keeps it across its rounds: `Some(None)` before its first score
    /// update (the EX score and the score rates read `Integer.MIN_VALUE`),
    /// `Some(Some([ex_score, rate.current_bp, rate.whole_bp]))` after it,
    /// those three fields answered from it (s7-06e-3-design.md §2a); `None`
    /// answers them from `fields`.
    pub fn set_play(
        &self,
        fields: &[i32],
        timers: &[i32],
        state_us: i64,
        score: Option<Option<[i32; 3]>>,
    ) {
        if let Some(f) = self.pass.replay.borrow_mut().as_mut() {
            f.fields.clear();
            f.fields.extend_from_slice(fields);
            f.timers.clear();
            f.timers.extend_from_slice(timers);
            f.state_us = state_us;
            f.allowed = None;
            f.no_score = matches!(score, Some(None));
            if let Some(Some(s)) = score {
                for (slot, v) in SCORE_SLOTS.into_iter().zip(s) {
                    if let Some(cell) = f.fields.get_mut(usize::from(slot)) {
                        *cell = v;
                    }
                }
            }
        }
    }

    /// Sets a frame production builds no play frame for: the host answers
    /// only reads that load fields `allowed` accepts (by slot) from
    /// `fields`, and the state clock; every timer read is unanswered.
    pub fn set_outside(&self, fields: &[i32], allowed: &[bool], state_us: i64) {
        if let Some(f) = self.pass.replay.borrow_mut().as_mut() {
            f.fields.clear();
            f.fields.extend_from_slice(fields);
            f.timers.clear();
            f.state_us = state_us;
            f.allowed = Some(allowed.to_vec());
            f.no_score = false;
        }
    }

    /// Makes every later `number(id)` unanswered: a test's mutation
    /// control.
    pub fn deny(&self, id: i32) {
        if let Some(f) = self.pass.replay.borrow_mut().as_mut() {
            f.deny.insert(id);
        }
    }

    /// Calls `f` with no arguments under a fresh budget: the runtime's
    /// instruction and host-call caps per call, and
    /// [`REFERENCE_CALL_TIME`] of wall time, so a loaded machine's clock
    /// cannot stop it.
    pub fn call(&self, f: &Function) -> Called {
        super::vm_hooks::set_frame_time(Some(REFERENCE_CALL_TIME));
        self.pass.ledger.begin_frame();
        super::vm_hooks::set_frame_time(None);
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

    /// Integer property `id` (a number's `ref`, or a numeric `value`) on
    /// the frame.
    #[must_use]
    pub fn number_property(&self, id: i32) -> PropValue {
        self.int_prop(int_in(ScreenKind::Play, id), Some(id))
    }

    /// Integer property `name` (a named `value`): the factory's id for
    /// it, else its name; `None` when the factory knows neither, so the
    /// source runs the string as a script.
    #[must_use]
    pub fn named_number_property(&self, name: &str) -> Option<PropValue> {
        let p = crate::import::int_named(ScreenKind::Play, name)?;
        Some(self.int_prop(p.0, p.1))
    }

    fn int_prop(&self, p: IntProp, id: Option<i32>) -> PropValue {
        let IntProp::Shown(v) = p else {
            return match p {
                IntProp::Null => PropValue::Null,
                _ => PropValue::Unsupported,
            };
        };
        let mut g = self.pass.replay.borrow_mut();
        let Some(f) = g.as_mut() else {
            return PropValue::Unsupported;
        };
        let delta = id.and_then(|i| f.perturb.get(&i)).copied().unwrap_or(0);
        match f.property(v.pre_judgment(), v.availability(), v.expr(), delta) {
            Some(Ok(n)) => PropValue::Value(n),
            Some(Err(())) => PropValue::Unavailable,
            None => PropValue::Unsupported,
        }
    }

    /// Image-index property `id` (an image group's or an image set's
    /// `ref`) on the frame: `None` when the mapping has no value for it;
    /// a property the factory does not know selects 0, as the source's
    /// null `ref` does (`SkinImage.java:123-125`).
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

    /// The id of boolean property `name` (leading `!`s ignored), when the
    /// factory names one: its static class is that id's.
    #[must_use]
    pub fn condition_id(name: &str) -> Option<i32> {
        ops::named_id(name.trim_start_matches('!'))
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

/// An integer property's value on a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropValue {
    /// The factory returns null for it.
    Null,
    /// The model has no value for it.
    Unsupported,
    /// Its availability guard is false.
    Unavailable,
    Value(i32),
}

/// The wall time one reference invocation may take.
const REFERENCE_CALL_TIME: std::time::Duration = std::time::Duration::from_secs(10);

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
            if f.deny.contains(&n) {
                return Some(Err(unanswered()));
            }
            match int_in(ScreenKind::Play, n) {
                IntProp::Shown(v) => {
                    let delta = f.perturb.get(&n).copied().unwrap_or(0);
                    match f.property(v.pre_judgment(), v.availability(), v.expr(), delta) {
                        Some(Ok(n)) => Ok(Value::Number(f64::from(n))),
                        Some(Err(())) => Err(unavailable(id)),
                        None => Err(unanswered()),
                    }
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
        "timer" if f.allowed.is_some() => Err(unanswered()),
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

/// The play fields the source's score data gives: the EX score and the
/// two score rates (the eight properties `Value::pre_judgment` names).
const SCORE_SLOTS: [u16; 3] = [
    skin_model::field::EX_SCORE,
    skin_model::field::RATE_CURRENT_BP,
    skin_model::field::RATE_WHOLE_BP,
];

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

#[cfg(test)]
mod tests {
    use skin_model::field;

    use super::Frame;

    /// The eight score properties as the source's score data gives them:
    /// `Integer.MIN_VALUE` before any, else from the carried fields
    /// (s7-06e-3-design.md L10); a direct property and a Lua read alike.
    #[test]
    fn score_properties_follow_the_score_data() {
        use crate::import::{IntProp, int_in};
        let ids = [71, 101, 102, 103, 115, 116, 155, 156];
        let read = |f: &mut Frame, id: i32| -> Option<i32> {
            let IntProp::Shown(v) = int_in(skin_model::ScreenKind::Play, id) else {
                return None;
            };
            f.property(v.pre_judgment(), v.availability(), v.expr(), 0)?
                .ok()
        };
        let mut f = Frame {
            fields: vec![0; field::COUNT],
            no_score: true,
            ..Frame::default()
        };
        for id in ids {
            let IntProp::Shown(v) = int_in(skin_model::ScreenKind::Play, id) else {
                panic!("{id} is mapped");
            };
            assert!(v.pre_judgment(), "{id}");
            assert_eq!(read(&mut f, id), Some(i32::MIN), "{id}");
        }
        f.no_score = false;
        for (slot, v) in super::SCORE_SLOTS.into_iter().zip([6, 10_000, 4_285]) {
            f.fields[usize::from(slot)] = v;
        }
        let got: Vec<Option<i32>> = ids.iter().map(|&id| read(&mut f, id)).collect();
        let want = [6, 6, 100, 0, 42, 85, 42, 85].map(Some);
        assert_eq!(got, want);
    }

    /// Off play, a read's availability guard is checked as its value is:
    /// an allowed value behind a guard on a field the host does not give
    /// is unanswered, where a play frame answers it.
    #[test]
    fn an_outside_guard_is_checked_as_its_value() {
        let mut f = Frame {
            fields: vec![0; field::COUNT],
            ..Frame::default()
        };
        f.fields[usize::from(field::POINT)] = 5;
        f.fields[usize::from(field::CHART_LEVEL_VALID)] = 1;
        let mut allowed = vec![false; field::COUNT];
        allowed[usize::from(field::POINT)] = true;
        f.allowed = Some(allowed);
        assert_eq!(f.guarded(None, "point"), Some(Ok(5)));
        assert_eq!(f.guarded(Some("chart.level_valid"), "point"), None);
        f.allowed = None;
        assert_eq!(f.guarded(Some("chart.level_valid"), "point"), Some(Ok(5)));
    }
}
