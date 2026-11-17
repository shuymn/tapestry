//! The comparator (s7-06e-design.md §3.6-§3.7): every source invocation
//! matched to an agreeing production invocation or attributed through
//! production's record of that very consumer occurrence, every production
//! invocation matched; agreement exact, the first difference of a pair
//! its finding.

use std::collections::{BTreeMap, BTreeSet};

use skin_beatoraja::lua::replay::{LoweredRecord, Occurrence};
use skin_beatoraja::lua::survey::Became;
use skin_model::present::trace::{Converted, Event, Skip};
use skin_model::present::{Fail, Val};
use skin_model::{MAX_KEY_US, PLAY, Program, Scope, State, Ty, compile};

use super::{FrameKind, ProductionRun, last_records};
use crate::isolation::{Entry, same_file};
use crate::observed::{Cause, Key, Observed, Outcome, Point, Raw, RefFrame, TimerOut, intern};
use crate::report::{Finding, Report, Stopped};

/// A number's value as tapestry converts it (design.md "beatoraja Lua
/// Skins", Built): the number truncated toward zero and saturated to
/// `i32` (NaN 0); nil and booleans 0. Restated here, not called, so the
/// comparator checks production's conversion against the rule.
pub(crate) fn toint(r: Raw) -> i32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "`as` truncates toward zero and saturates"
    )]
    match r {
        Raw::Int(n) => {
            i32::try_from(n.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap_or(0)
        }
        Raw::Float(f) => f as i32,
        _ => 0,
    }
}

/// What an intended difference from the source is counted under: a
/// number's value whose LuaJ `toint()` differs from [`toint`].
pub const TOINT_PAST_I32: &str = "toint past i32";

/// Whether the source's `toint()` of the reference's outcome `o` for
/// consumer `field` differs from tapestry's [`toint`]: an intended
/// difference (design.md "Differences from the source", number values
/// past `i32`), counted, never a finding.
fn toint_differs(field: &str, o: Outcome) -> bool {
    matches!(o, Outcome::Value(r) if field == "value" && luaj_toint(r) != toint(r))
}

/// LuaJ's `toint()` of a value (`SkinLuaAccessor.java:640-651`): the
/// double saturated to a `long` (NaN 0), then its low 32 bits; nil and
/// booleans 0. Only counts intended differences ([`toint_differs`]).
fn luaj_toint(r: Raw) -> i32 {
    #[expect(clippy::cast_possible_truncation, reason = "Java's d2l then l2i")]
    match r {
        Raw::Int(n) => (n as f64) as i64 as i32,
        Raw::Float(f) => f as i64 as i32,
        _ => 0,
    }
}

/// LuaJ's `toboolean()`.
fn luaj_bool(r: Raw) -> bool {
    !matches!(r, Raw::Nil | Raw::Bool(false))
}

/// LuaJ's `tolong()`.
fn luaj_long(r: Raw) -> i64 {
    #[expect(clippy::cast_possible_truncation, reason = "Java's d2l")]
    match r {
        Raw::Int(n) => n,
        Raw::Float(f) => f as i64,
        _ => 0,
    }
}

/// Whether two numbers are the same real number (NaN with NaN; −0.0 and
/// 0.0 are equal as IEEE compares them).
fn same_number(a: Raw, b: Raw) -> bool {
    let exact = |i: i64, f: f64| {
        f.is_finite()
            && f.fract() == 0.0
            && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f)
            && {
                #[expect(clippy::cast_possible_truncation, reason = "checked in range")]
                let g = f as i64;
                g == i
            }
    };
    match (a, b) {
        (Raw::Int(x), Raw::Int(y)) => x == y,
        (Raw::Float(x), Raw::Float(y)) => x == y || (x.is_nan() && y.is_nan()),
        (Raw::Int(i), Raw::Float(f)) | (Raw::Float(f), Raw::Int(i)) => exact(i, f),
        _ => false,
    }
}

fn val_raw(v: Val) -> Raw {
    match v {
        Val::Nil => Raw::Nil,
        Val::Bool(b) => Raw::Bool(b),
        Val::Int(n) => Raw::Int(n),
        Val::Float(f) => Raw::Float(f),
    }
}

fn same_raw(a: Raw, b: Raw) -> bool {
    match (a, b) {
        (Raw::Nil, Raw::Nil) => true,
        (Raw::Bool(x), Raw::Bool(y)) => x == y,
        (Raw::Other(_), _) | (_, Raw::Other(_)) => false,
        (x, y) => same_number(x, y),
    }
}

/// What the reference's outcome converts to for `field`'s consumer.
fn converted(field: &str, o: Outcome) -> Option<Converted> {
    Some(match (field, o) {
        ("timer", Outcome::Value(r)) => Converted::Long(Some(luaj_long(r))),
        ("timer", Outcome::Error) => Converted::Long(Some(i64::MIN)),
        ("timer", Outcome::Unavailable) => Converted::Long(None),
        ("value", Outcome::Value(r)) => {
            let n = toint(r);
            Converted::Int(n, n != i32::MIN && n != i32::MAX)
        }
        ("value", Outcome::Error) => Converted::Int(0, true),
        ("value", Outcome::Unavailable) => Converted::Int(0, false),
        (_, Outcome::Value(r)) => Converted::Bool(luaj_bool(r)),
        (_, Outcome::Error | Outcome::Unavailable) => Converted::Bool(false),
        _ => return None,
    })
}

/// The first difference between production's invocation and the
/// reference's, if any: the class, the raw value, then the converted
/// output.
///
/// One raw difference is no finding by itself: the reference's `nil`
/// against production's integer 0. The program gives 0 where a branch of
/// the source returns nothing, and the source converts that `nil` to 0
/// (`toint()`, `toboolean()`). The pair agrees only when the converted
/// output is equal, so a `value` agrees (0 drawn on both sides) and a
/// `draw` does not (`nil` false, 0 true); `nil` against any other integer
/// stays a raw difference.
fn differ(field: &str, prod: (Result<Val, Fail>, Converted), refo: Outcome) -> Option<String> {
    let (result, conv) = prod;
    let class = match (result, refo) {
        (Ok(_), Outcome::Value(_))
        | (Err(Fail::Error), Outcome::Error)
        | (Err(Fail::Unavailable), Outcome::Unavailable) => None,
        (r, o) => Some(format!("class: production {r:?}, reference {o:?}")),
    };
    if class.is_some() {
        return class;
    }
    if let (Ok(v), Outcome::Value(r)) = (result, refo)
        && !same_raw(val_raw(v), r)
        && !matches!((v, r), (Val::Int(0), Raw::Nil))
    {
        return Some(format!("raw: production {v:?}, reference {r:?}"));
    }
    match converted(field, refo) {
        Some(c) if c == conv => None,
        c => Some(format!("converted: production {conv:?}, reference {c:?}")),
    }
}

/// A hint naming the operators of callable `c`'s source where Lua 5.4
/// and LuaJ differ, from its tokens (strings and comments skipped).
fn luaj_hint(obs: &Observed, c: u32) -> String {
    let Some((file, first)) = obs.sites.get(&c) else {
        return String::new();
    };
    let last = obs.ends.get(&c).copied().unwrap_or(*first).max(*first);
    let Ok(text) = std::fs::read_to_string(obs.root.join(file.trim_start_matches('@'))) else {
        return String::new();
    };
    let body: Vec<&str> = text
        .lines()
        .skip(usize::try_from(first.saturating_sub(1)).unwrap_or(0))
        .take(usize::try_from(last - first + 1).unwrap_or(0))
        .collect();
    let ops = crate::hints::operators(&body.join("\n"));
    if ops.is_empty() {
        String::new()
    } else {
        format!(" [luaj-operator: {} at line {first}]", ops.join(" "))
    }
}
/// A lowered occurrence's expression compiled as the import compiled it
/// (s7-06e-3-design.md §2): its guards and its source over the play
/// schema, `Bool` for a `draw` or an `op`, `Int` for a `value`.
struct LoweredExpr {
    guards: Vec<Program>,
    src: Program,
}

fn compile_lowered(field: &str, l: &LoweredRecord) -> Result<LoweredExpr, String> {
    let scope = Scope::new(&PLAY);
    let truthy = field != "value";
    let ty = if truthy { Ty::Bool } else { Ty::Int };
    let guards = l
        .guards
        .iter()
        .map(|g| compile(g, &scope, Ty::Bool).map_err(|e| format!("guard {g}: {e:?}")))
        .collect::<Result<Vec<_>, _>>()?;
    let src = compile(&l.src, &scope, ty).map_err(|e| format!("{}: {e:?}", l.src))?;
    Ok(LoweredExpr { guards, src })
}

/// The first difference between a lowered occurrence's expression on
/// `fields` and `timers` and the reference's invocation: the class (a
/// false guard is unavailable), then the converted value (`toboolean` for
/// a `draw` or an `op`, [`toint`] for a `value`, by the reference
/// occurrence's own consumer `field`, whatever the record says). An
/// invocation that could not complete or raised has no expression
/// counterpart.
fn lowered_differ(
    field: &str,
    e: &LoweredExpr,
    fields: &[i32],
    timers: &[i32],
    refo: Outcome,
) -> Option<String> {
    let state = State { fields, timers };
    let given = e.guards.iter().all(|g| g.eval(&state) != 0);
    match refo {
        Outcome::Unanswered(api) => Some(format!("reference could not complete ({api})")),
        Outcome::Budget => Some("reference could not complete (budget)".into()),
        Outcome::Error => Some("a Lua error where the expression gives a value".into()),
        Outcome::Unavailable if given => {
            Some("class: production value, reference unavailable".into())
        }
        Outcome::Unavailable => None,
        Outcome::Value(_) if !given => {
            Some("class: production unavailable, reference value".into())
        }
        Outcome::Value(r) => {
            let v = e.src.eval(&state);
            let (got, want) = if field != "value" {
                (i32::from(v != 0), i32::from(luaj_bool(r)))
            } else {
                (v, toint(r))
            };
            (got != want).then(|| format!("lowered: production {got}, reference {want} ({r:?})"))
        }
    }
}

/// An occurrence's identity as the comparator joins on it: destination,
/// consumer, index, callable and object path (s7-06e-1-r1-tables.md, A4).
type Join = (usize, &'static str, u32, u32, &'static str);

fn join(k: &Key) -> Join {
    (k.dest, k.field, k.index, k.callable, k.path)
}

/// Production's records, by occurrence.
struct Cx<'a> {
    by_key: BTreeMap<Join, &'a Occurrence>,
    run: &'a ProductionRun,
}

impl<'a> Cx<'a> {
    fn rec(&self, k: &Key) -> Option<&'a Occurrence> {
        self.by_key.get(&join(k)).copied()
    }

    /// What an observer occurrence folded to, when production recorded
    /// one: a `timer` lowered to a constant (design.md "beatoraja Lua
    /// Skins", observers).
    fn observer(&self, k: &Key) -> Option<bool> {
        observer_fold(self.rec(k)?)
    }

    /// Its program callback, when it is compiled.
    fn compiled(&self, k: &Key) -> Option<u16> {
        self.rec(k)
            .filter(|o| o.disposition == "compiled")
            .and_then(|o| o.program)
    }

    /// Why production did not run an occurrence the source ran: its own
    /// record, or its destination's (dropped, folded, ignored); `None`
    /// when it is compiled or nothing explains it. A callable's
    /// classified-only reason explains no occurrence by itself
    /// (s7-06e-design.md §3.7).
    fn attribute(&self, key: &Key) -> Option<String> {
        match self.rec(key) {
            Some(o) if o.disposition == "compiled" => None,
            Some(o) => Some(match &o.reason {
                Some(r) if o.disposition == "replaced" => format!("replaced {r}"),
                _ => o.disposition.to_owned(),
            }),
            None => match self
                .run
                .recorded
                .destinations
                .get(key.dest)
                .map(|e| e.became)
            {
                Some(Became::Dropped) => Some("dropped".into()),
                Some(Became::Folded) => Some("folded".into()),
                Some(Became::Ignored) => Some("ignored".into()),
                _ => None,
            },
        }
    }
}

/// What observer record `o` folded to: a `timer` lowered to `true` or
/// `false`, which only an observer's record is.
fn observer_fold(o: &Occurrence) -> Option<bool> {
    if o.field != "timer" || o.disposition != "lowered" {
        return None;
    }
    match o.lowered.as_ref().map(|l| l.src.as_str()) {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

/// Each frame's `screen` origin on the state clock as docs/stage3.md D3
/// gives it, worked out from the frames themselves: a loading frame's is
/// the first frame of its loading lifetime (its run of loading frames); a
/// play frame's is the first loading frame before the first play frame,
/// else the first play frame less the skin's loading minimum; a page has
/// none (production builds nothing there). Two loading phases with no
/// frame between would read as one lifetime; no script has them.
fn screen_origins(run: &ProductionRun) -> Vec<Option<i64>> {
    let first_play = run
        .frames
        .iter()
        .position(|f| matches!(f.kind, FrameKind::Play { .. }));
    let loading_first = run
        .frames
        .iter()
        .take(first_play.unwrap_or(run.frames.len()))
        .find(|f| matches!(f.kind, FrameKind::Loading))
        .map(|f| f.state_us);
    let play = loading_first.or_else(|| {
        first_play
            .and_then(|i| run.frames.get(i))
            .map(|f| f.state_us - run.loading_min_us)
    });
    let mut lifetime = None;
    let mut out = Vec::with_capacity(run.frames.len());
    for f in &run.frames {
        out.push(match f.kind {
            FrameKind::Loading => Some(*lifetime.get_or_insert(f.state_us)),
            FrameKind::Page => {
                lifetime = None;
                None
            }
            FrameKind::Play { .. } => {
                lifetime = None;
                play
            }
        });
    }
    out
}

/// An observer occurrence's comparison on one frame.
enum ObsCheck {
    Agreed,
    /// Its destination was left by the import before the fold (dropped
    /// or ignored): attributed, not compared.
    Left,
    Found(String),
}

/// The bucket of an observer occurrence whose destination the import
/// left.
const LEFT: &str = "observer on a destination the import left";

/// The first difference of an observer occurrence `occ` from production's
/// fold `b` on a frame at `state_us`: `ages` its destination's own
/// elements' sampled ages there, `origin` the frame's `screen` origin
/// (`None` on a page, where production builds nothing and only the
/// reference is checked), `first` the play state's first frame. The
/// source's observer starts at the first frame its destination is
/// prepared, which for a folded true one is the first; production draws
/// it on `screen`, so each element's age must be the frame's time less
/// the origin, `first − origin` more than the reference's age. `Ok` that
/// difference when the ages were compared.
fn observer_differs(
    b: bool,
    occ: &Occ,
    ages: &[Option<i64>],
    (state_us, origin, first): (i64, Option<i64>, i64),
) -> Result<Option<i64>, String> {
    let outcome = match occ.first {
        Point::NotReached {
            why: why @ (Cause::Inconclusive | Cause::Unavailable),
            ..
        } => return Err(format!("observer: reference could not decide ({why:?})")),
        Point::NotReached { why, .. } if b => {
            return Err(format!(
                "observer: production true, the source does not reach it ({why:?})"
            ));
        }
        Point::NotReached { .. } => return Ok(None),
        Point::Ran { outcome, .. } => outcome,
    };
    match outcome {
        Outcome::Value(_) => {}
        Outcome::Unanswered(a) => {
            return Err(format!("observer: reference could not complete ({a})"));
        }
        Outcome::Budget => return Err("observer: reference could not complete (budget)".into()),
        Outcome::Error | Outcome::Unavailable => {
            return Err(format!("observer: reference {outcome:?}"));
        }
    }
    let Some(t) = occ.summary else {
        return Err("observer: no summary".into());
    };
    if t.on != b {
        return Err(format!("observer: production {b}, reference {}", t.on));
    }
    if !b {
        return Ok(None);
    }
    if t.start != Some(first) {
        return Err(format!(
            "observer start: reference {:?}, expected {first} (the play state's first frame)",
            t.start
        ));
    }
    let Some(origin) = origin else {
        return Ok(None);
    };
    let want = state_us - origin;
    match t.shown {
        None => Err("observer: the selected source's fact is unknown: sampling undecided".into()),
        Some(false) if ages.iter().any(Option::is_some) => Err(format!(
            "observer: sampled {ages:?} where the source draws nothing"
        )),
        Some(false) => Ok(None),
        Some(true) if ages.is_empty() => Err("observer: its destination has no element".into()),
        Some(true) if ages.iter().any(|&a| a != Some(want)) => Err(format!(
            "observer age: production {ages:?}, expected {want} (reference {}, D3 origin {origin})",
            state_us - first
        )),
        Some(true) => Ok(Some(first - origin)),
    }
}

/// A timer callback's trace: on, its start, its elapsed µs and the start
/// installed.
type TimerTrace = (bool, Option<i64>, Option<i64>, Option<bms_model::TimeUs>);

/// One invocation's raw result and its conversion.
type Invocation = (Result<Val, Fail>, Converted);

/// A program callback's event: its invocation or its skip.
#[derive(Clone, Copy)]
enum First {
    Ran(Invocation),
    Skipped(Skip),
}

/// One frame's production events after [`production_shapes`]: one of
/// each, every callback within the program.
#[derive(Default)]
struct Prod {
    first: BTreeMap<u16, First>,
    timer: BTreeMap<u16, TimerTrace>,
    /// Every invocation, in order.
    order: Vec<u16>,
    sampled: BTreeMap<usize, Vec<i64>>,
}

/// A shape finding: production's callback or the source's occurrence it
/// concerns, and what is wrong.
type Misshapen = (Option<u16>, Option<Key>, String);

/// The program callbacks' schema: how many there are and which are timers.
struct Schema<'a> {
    callbacks: u16,
    is_timer: &'a dyn Fn(u16) -> bool,
    timer_slots: &'a BTreeMap<u16, usize>,
    /// Per callback, whether it loads or stores a slot.
    slotted: &'a [bool],
}

/// Production's events of one frame, checked against the program's
/// schema before anything is compared (s7-06e-1-r1-tables.md, A2): every
/// callback within the program; one event (an invocation or a skip)
/// each, a timer callback invoked once like any other; a timer event
/// exactly when a timer callback ran; a program slot sampled only with an
/// installed start.
fn production_shapes(events: &[Event], s: &Schema<'_>) -> (Prod, Vec<Misshapen>) {
    let mut p = Prod::default();
    let mut bad: Vec<Misshapen> = Vec::new();
    let mut kbad = |k: u16, what: String| bad.push((Some(k), None, what));
    for e in events {
        let k = match *e {
            Event::Ran { k, .. } | Event::Skipped { k, .. } | Event::Timer { k, .. } => Some(k),
            Event::Sampled { .. } => None,
        };
        if let Some(k) = k
            && k >= s.callbacks
        {
            kbad(
                k,
                format!("callback {k}: outside the program's {}", s.callbacks),
            );
            continue;
        }
        match *e {
            Event::Ran { k, result, conv } => {
                p.order.push(k);
                if p.first.insert(k, First::Ran((result, conv))).is_some() {
                    kbad(k, format!("callback {k}: a second event"));
                }
            }
            Event::Skipped { k, why } => {
                if p.first.insert(k, First::Skipped(why)).is_some() {
                    kbad(k, format!("callback {k}: a second event"));
                }
            }
            Event::Timer {
                k,
                on,
                start,
                elapsed_us,
                installed,
            } => {
                if !(s.is_timer)(k) {
                    kbad(k, format!("callback {k}: a timer event of a non-timer"));
                } else if p
                    .timer
                    .insert(k, (on, start, elapsed_us, installed))
                    .is_some()
                {
                    kbad(k, format!("timer {k}: a second timer event"));
                }
            }
            Event::Sampled { slot, age_us } => p.sampled.entry(slot).or_default().push(age_us),
        }
    }
    for k in 0..s.callbacks {
        let first = p.first.get(&k).copied();
        if first.is_none() {
            kbad(k, format!("callback {k}: no event"));
        }
        let ran = matches!(first, Some(First::Ran(_)));
        if (s.is_timer)(k) && ran != p.timer.contains_key(&k) {
            kbad(
                k,
                format!(
                    "timer {k}: invoked {ran}, timer event {}",
                    p.timer.contains_key(&k)
                ),
            );
        }
    }
    for (&k, &slot) in s.timer_slots {
        let installed = p.timer.get(&k).is_some_and(|t| t.3.is_some());
        if p.sampled.contains_key(&slot) && !installed {
            kbad(k, format!("timer {k}: sampled with no installed start"));
        }
    }
    // The slots (s7-06e-2-design.md §6.2): a slotted callback is a timer.
    for k in 0..s.callbacks {
        let slotted = s.slotted.get(usize::from(k)).copied().unwrap_or(false);
        if slotted && !(s.is_timer)(k) {
            kbad(k, format!("callback {k}: slotted but not a timer"));
        }
    }
    (p, bad)
}

/// One expected occurrence of an installed object, as the source's frame
/// gave it.
#[derive(Clone, Copy)]
struct Occ {
    key: Key,
    first: Point,
    summary: Option<TimerOut>,
}

/// The source's frame as one catalogue of the installed objects'
/// expected occurrences (s7-06e-1-r1-tables.md, A2): the object ranges an
/// ordered partition of the points, one range per installed object; in
/// each, exactly its consumers in schedule order, each with one point, a
/// timer's too; one summary per invoked timer and none elsewhere. An
/// object whose points break this is reported and left out.
fn catalogue(rf: &RefFrame, installed: &[Vec<Key>]) -> (Vec<Vec<Occ>>, Vec<Misshapen>) {
    let mut bad: Vec<Misshapen> = Vec::new();
    let mut out: Vec<Vec<Occ>> = Vec::new();
    let partition = rf.objects.len() == installed.len()
        && rf.objects.first().is_none_or(|r| r.start == 0)
        && rf.objects.windows(2).all(|w| w[0].end == w[1].start)
        && rf.objects.iter().all(|r| r.start <= r.end)
        && rf.objects.last().map_or(0, |r| r.end) == rf.points.len();
    if !partition {
        bad.push((
            None,
            None,
            "the source's objects do not partition its points".into(),
        ));
        return (out, bad);
    }
    for (range, keys) in rf.objects.iter().zip(installed) {
        let pts = rf.points.get(range.clone()).unwrap_or_default();
        let mut occs: Vec<Occ> = Vec::new();
        let mut at = 0;
        let mut broken = None;
        for &key in keys {
            let first = match pts.get(at) {
                Some(p @ (Point::Ran { key: k, .. } | Point::NotReached { key: k, .. }))
                    if *k == key =>
                {
                    *p
                }
                _ => {
                    broken = Some((key, "an expected occurrence has no point"));
                    break;
                }
            };
            at += 1;
            occs.push(Occ {
                key,
                first,
                summary: None,
            });
        }
        if broken.is_none() && at != pts.len() {
            let key = match pts.get(at) {
                Some(Point::Ran { key, .. } | Point::NotReached { key, .. }) => *key,
                None => keys.first().copied().unwrap_or(UNKNOWN),
            };
            broken = Some((key, "a point the object's consumers do not expect"));
        }
        if let Some((key, what)) = broken {
            bad.push((None, Some(key), what.into()));
            out.push(Vec::new());
        } else {
            out.push(occs);
        }
    }
    // Summaries: one per invoked timer, none elsewhere.
    for t in &rf.timers {
        let slot = out.iter_mut().flatten().find(|o| {
            o.key == t.key && o.key.field == "timer" && matches!(o.first, Point::Ran { .. })
        });
        match slot {
            Some(o) if o.summary.is_none() => o.summary = Some(*t),
            Some(_) => bad.push((None, Some(t.key), "a second timer summary".into())),
            None => bad.push((None, Some(t.key), "a summary of no invoked timer".into())),
        }
    }
    for o in out.iter().flatten() {
        if o.key.field == "timer" && matches!(o.first, Point::Ran { .. }) && o.summary.is_none() {
            bad.push((None, Some(o.key), "an invoked timer with no summary".into()));
        }
    }
    (out, bad)
}

/// Where a frame stands, as a finding prints it.
fn place(kind: FrameKind) -> String {
    match kind {
        FrameKind::Play { round, .. } => format!("round {} {}", round + 1, kind.label()),
        FrameKind::Loading | FrameKind::Page => kind.label().to_owned(),
    }
}

/// A key for findings no occurrence names.
const UNKNOWN: Key = Key {
    dest: usize::MAX,
    field: "?",
    index: 0,
    callable: u32::MAX,
    path: "",
};

/// What a consumer's output cells hold after the source's invocation:
/// (value, given) as `Present::run` writes them.
fn expected_out(field: &str, o: Outcome) -> (i32, i32) {
    match converted(field, o) {
        Some(Converted::Bool(b)) => (i32::from(b), i32::from(o != Outcome::Unavailable)),
        Some(Converted::Int(n, ok)) => (n, i32::from(ok)),
        _ => (0, 0),
    }
}

/// Whether a compiled consumer's outcome lets the one after it run
/// (`Present::run`'s `after`): a value given and no sentinel, a draw or
/// an `op` true.
fn allows(field: &str, o: Outcome) -> bool {
    match converted(field, o) {
        Some(Converted::Int(_, ok)) => ok,
        Some(Converted::Bool(b)) => b && o != Outcome::Unavailable,
        _ => false,
    }
}

/// The skips production may give for a source's `NotReached` of `why`
/// whose object's last compiled consumer before it did not allow it
/// (`blocked`): its `after` is checked before its gate, so `After`; else
/// its gate. A negative selector has no production gate (§7.1): either is
/// accepted, a production invocation there being the finding. `None` for
/// a cause no skip may answer (s7-06e-1-r1-tables.md, A3).
fn allowed(why: Cause, blocked: bool) -> Option<&'static [Skip]> {
    match (why, blocked) {
        (Cause::Inconclusive, _) => None,
        (Cause::Selector, _) => Some(&[Skip::Gate, Skip::After]),
        (_, true) => Some(&[Skip::After]),
        (_, false) => Some(&[Skip::Gate]),
    }
}

/// Compares `run` with `obs` under the isolation entries `iso`.
#[expect(clippy::too_many_lines, reason = "one pass over every frame")]
pub fn compare(run: &ProductionRun, obs: &Observed, iso: &[Entry]) -> Report {
    let mut rep = Report {
        frames: run.frames.len(),
        callbacks: run.callbacks,
        unmodelled: obs.unmodelled.clone(),
        removed: obs.removed.clone(),
        unresolved: obs.unresolved.clone(),
        custom: obs.custom,
        ..Report::default()
    };
    let records = last_records(&run.recorded);
    let mut by_key: BTreeMap<Join, &Occurrence> = BTreeMap::new();
    let mut key_of: BTreeMap<u16, Key> = BTreeMap::new();
    for o in records.values() {
        if let Some(dest) = o.destination {
            let key = Key {
                dest,
                field: o.field,
                index: o.index,
                callable: o.callable,
                path: intern(&o.path),
            };
            by_key.insert(join(&key), o);
            if let (Some(k), "compiled") = (o.program, o.disposition) {
                key_of.insert(k, key);
            }
        }
    }
    let cx = Cx { by_key, run };
    let site = |k: &Key| obs.sites.get(&k.callable).cloned().unwrap_or_default();
    for (k, key) in &key_of {
        rep.compiled.push((*k, *key, site(key)));
    }
    let key_k = |k: u16| key_of.get(&k).copied().unwrap_or(UNKNOWN);
    let is_timer = |k: u16| key_of.get(&k).is_some_and(|key| key.field == "timer");
    let callbacks = u16::try_from(run.callbacks).unwrap_or(u16::MAX);
    let mut findings: Vec<Finding> = Vec::new();
    let places: Vec<String> = run.frames.iter().map(|f| place(f.kind)).collect();
    let mut find = |key: Key, frame: usize, what: String| {
        findings.push(Finding {
            key,
            site: site(&key),
            frame,
            place: places.get(frame).cloned().unwrap_or_default(),
            what,
        });
    };
    for (i, r) in run.rounds.iter().enumerate() {
        rep.rounds.push(format!(
            "round {}{}",
            i + 1,
            if r.ending_cut {
                ", ending replayed 30 s past the last note of a longer declaration"
            } else {
                ""
            }
        ));
    }
    if run.frames.len() != obs.frames.len() {
        find(
            UNKNOWN,
            0,
            format!(
                "frame count: production {}, reference {}",
                run.frames.len(),
                obs.frames.len()
            ),
        );
    }
    // A compiled callback on an object the reference does not run: one
    // located finding for the run, its frames not reported again; a
    // lowered one is counted until e-3.
    let mut listed_k: BTreeSet<u16> = BTreeSet::new();
    let mut listed_observers: BTreeSet<Join> = BTreeSet::new();
    for (label, list) in [
        ("unmodelled", &obs.unmodelled),
        ("unresolved", &obs.unresolved),
    ] {
        for l in list {
            for key in &l.callables {
                match cx.rec(key).map(|o| o.disposition) {
                    Some("compiled") => {
                        if let Some(k) = cx.compiled(key) {
                            listed_k.insert(k);
                        }
                        find(
                            *key,
                            0,
                            format!("compiled on a {label} object ({})", l.what),
                        );
                    }
                    Some("lowered") if cx.observer(key).is_some() => {
                        listed_observers.insert(join(key));
                        find(
                            *key,
                            0,
                            format!("observer on a {label} object, not compared ({})", l.what),
                        );
                    }
                    Some("lowered") => {
                        *rep.buckets
                            .entry(format!("lowered on a {label} object (e-3)"))
                            .or_default() += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    let mut stopped: BTreeMap<(&'static str, u32), (&'static str, usize, Key)> = BTreeMap::new();
    let mut lowered: BTreeMap<Join, Result<LoweredExpr, String>> = BTreeMap::new();
    let origins = screen_origins(run);
    let first_us = run.frames.first().map_or(0, |f| f.state_us);
    // Each observer occurrence's points, to find one never compared.
    let mut observer_points: BTreeMap<Join, usize> = BTreeMap::new();
    let mut observers_agreed: BTreeMap<(&'static str, bool), usize> = BTreeMap::new();
    let mut observer_offsets: BTreeMap<i64, usize> = BTreeMap::new();
    // An observer occurrence on frame `i` of kind `label`: recognized on
    // both sides, its destination what its fold makes it, then compared
    // ([`observer_differs`]). `None` when it is no observer's.
    let mut observer = |key: Key, occ: &Occ, i: usize, label: &'static str| -> Option<ObsCheck> {
        let held = obs.observers.contains(&key.callable);
        let Some(b) = cx.observer(&key) else {
            return held.then(|| match cx.rec(&key) {
                // No record: the bucket only for a destination the import
                // left before the fold; any other is a record missing.
                None => match run.recorded.destinations.get(key.dest).map(|e| e.became) {
                    Some(Became::Dropped | Became::Ignored) => ObsCheck::Left,
                    Some(became) => ObsCheck::Found(format!(
                        "observer: no production record, its destination {became:?}"
                    )),
                    None => ObsCheck::Found(
                        "observer: no production record, nor its destination's".into(),
                    ),
                },
                Some(o) => ObsCheck::Found(format!(
                    "observer: production record {}, the reference holds an observer",
                    o.disposition
                )),
            });
        };
        if !held {
            return Some(ObsCheck::Found(format!(
                "observer: production folds {b}, the reference holds no observer"
            )));
        }
        let entry = run.recorded.destinations.get(key.dest);
        let elements = match (b, entry.map(|e| e.became)) {
            (false, Some(Became::Folded)) | (true, Some(Became::Retained)) => {
                entry.map(|e| e.elements.clone()).unwrap_or_default()
            }
            (true, Some(Became::Dropped | Became::Ignored)) => return Some(ObsCheck::Left),
            (_, Some(became)) => {
                return Some(ObsCheck::Found(format!(
                    "observer: a {b} fold whose destination became {became:?}"
                )));
            }
            (_, None) => {
                return Some(ObsCheck::Found(format!(
                    "observer: a {b} fold with no destination record"
                )));
            }
        };
        *observer_points.entry(join(&key)).or_default() += 1;
        let pf = run.frames.get(i)?;
        let ages: Vec<Option<i64>> = elements
            .filter_map(|e| u32::try_from(e).ok())
            .map(|e| {
                pf.element_timer_ages
                    .iter()
                    .find(|(el, _)| *el == e)
                    .and_then(|(_, t)| *t)
            })
            .collect();
        let origin = origins.get(i).copied().flatten();
        Some(
            match observer_differs(b, occ, &ages, (pf.state_us, origin, first_us)) {
                Ok(d) => {
                    *observers_agreed.entry((label, b)).or_default() += 1;
                    if let Some(d) = d {
                        *observer_offsets.entry(d).or_default() += 1;
                    }
                    ObsCheck::Agreed
                }
                Err(what) => ObsCheck::Found(what),
            },
        )
    };
    for (i, (pf, rf)) in run.frames.iter().zip(&obs.frames).enumerate() {
        let label = pf.kind.label();
        rep.kinds.entry(label).or_default().0 += 1;
        let agreed_before = rep.agreed;
        let (cat, rbad) = catalogue(rf, &obs.installed);
        for (_, key, what) in rbad {
            find(key.unwrap_or(UNKNOWN), i, what);
        }
        let round = match pf.kind {
            FrameKind::Play { round, .. } => round,
            FrameKind::Loading | FrameKind::Page => {
                if !rf.outside {
                    find(
                        UNKNOWN,
                        i,
                        "frame kind: production outside, reference play".into(),
                    );
                }
                // Production builds no play frame here: no event is
                // expected, and each source invocation is the `outside`
                // bucket's; a stopped slotted root is a finding, any other
                // stopped root needs a reviewed entry (O1).
                // A loading frame builds the skin's loading state, so a
                // sample of a timer outside the program's slots is its own
                // (its built-in and `when` timers); a page builds nothing.
                let program_slot = |slot: usize| run.timer_slots.values().any(|&s| s == slot);
                let loading = matches!(pf.kind, FrameKind::Loading);
                for e in &pf.events {
                    let k = match *e {
                        Event::Ran { k, .. }
                        | Event::Skipped { k, .. }
                        | Event::Timer { k, .. } => key_k(k),
                        Event::Sampled { slot, .. } if loading && !program_slot(slot) => continue,
                        Event::Sampled { .. } => UNKNOWN,
                    };
                    find(k, i, "production event on an outside frame".into());
                }
                let bucket = format!("outside ({label})");
                for occ in cat.iter().flatten() {
                    let key = occ.key;
                    rep.points += 1;
                    match observer(key, occ, i, label) {
                        None => {}
                        Some(ObsCheck::Agreed) => continue,
                        Some(ObsCheck::Left) => {
                            *rep.buckets.entry(LEFT.into()).or_default() += 1;
                            continue;
                        }
                        Some(ObsCheck::Found(what)) => {
                            find(key, i, what);
                            continue;
                        }
                    }
                    let Point::Ran { outcome, .. } = occ.first else {
                        continue;
                    };
                    let api = match outcome {
                        Outcome::Unanswered(a) => Some(a),
                        Outcome::Budget => Some("budget"),
                        _ => None,
                    };
                    let Some(api) = api else {
                        *rep.buckets.entry(bucket.clone()).or_default() += 1;
                        continue;
                    };
                    match cx.compiled(&key) {
                        Some(k) if run.slotted.get(usize::from(k)).copied().unwrap_or(false) => {
                            find(
                                key,
                                i,
                                format!("a slotted root stopped on an outside frame ({api})"),
                            );
                        }
                        _ => {
                            stopped
                                .entry(("outside", key.callable))
                                .or_insert((api, 0, key))
                                .1 += 1;
                        }
                    }
                }
                continue;
            }
        };
        if rf.outside {
            find(
                UNKNOWN,
                i,
                "frame kind: production play, reference outside".into(),
            );
        }
        let schema = Schema {
            callbacks,
            is_timer: &is_timer,
            timer_slots: &run.timer_slots,
            slotted: &run.slotted,
        };
        let (p, pbad) = production_shapes(&pf.events, &schema);
        for (k, key, what) in pbad {
            find(key.unwrap_or_else(|| k.map_or(UNKNOWN, key_k)), i, what);
        }
        rep.production_runs += p.order.len();
        let mut used: BTreeSet<u16> = BTreeSet::new();
        let mut seen: BTreeSet<u16> = BTreeSet::new();
        let mut order: Vec<u16> = Vec::new();
        let mut outs: BTreeMap<u16, (i32, i32)> = BTreeMap::new();
        // Each installed object's occurrences, once, in schedule order;
        // `blocked`: the last compiled consumer before did not allow what
        // follows.
        for occs in &cat {
            let mut blocked = false;
            for occ in occs {
                let key = occ.key;
                let k = cx.compiled(&key);
                let call = match occ.first {
                    Point::Ran { outcome, .. } => Some(outcome),
                    Point::NotReached { .. } => None,
                };
                // Points: the invocation, or the one not reached.
                rep.points += 1;
                match observer(key, occ, i, label) {
                    None => {}
                    Some(ObsCheck::Agreed) => {
                        rep.matched += 1;
                        rep.agreed += 1;
                        continue;
                    }
                    Some(ObsCheck::Left) => {
                        *rep.buckets.entry(LEFT.into()).or_default() += 1;
                        continue;
                    }
                    Some(ObsCheck::Found(what)) => {
                        find(key, i, what);
                        continue;
                    }
                }
                let lowered_rec = cx
                    .rec(&key)
                    .filter(|o| k.is_none() && o.disposition == "lowered");
                let lowered_here = lowered_rec.is_some();
                let api = match call {
                    Some(Outcome::Unanswered(a)) => Some(a),
                    Some(Outcome::Budget) => Some("budget"),
                    _ => None,
                };
                if let Some(api) = api
                    && k.is_none()
                    && !lowered_here
                {
                    stopped
                        .entry(("play", key.callable))
                        .or_insert((api, 0, key))
                        .1 += 1;
                }
                // A lowered occurrence: its expression on this frame's
                // production state against the closure (s7-06e-3-design.md
                // §2); a disagreement is a finding.
                if let Some(o) = lowered_rec {
                    let expr = lowered.entry(join(&key)).or_insert_with(|| {
                        o.lowered
                            .as_ref()
                            .ok_or_else(|| "a lowered occurrence with no expression".to_owned())
                            .and_then(|l| compile_lowered(key.field, l))
                    });
                    if let Some(outcome) = call {
                        rep.matched += 1;
                        if toint_differs(key.field, outcome) {
                            *rep.intended.entry(TOINT_PAST_I32).or_default() += 1;
                        }
                        let e = match expr {
                            Ok(e) => e,
                            Err(why) => {
                                find(key, i, format!("lowered expression: {why}"));
                                continue;
                            }
                        };
                        let Some(d) = lowered_differ(key.field, e, &pf.fields, &pf.timers, outcome)
                        else {
                            rep.agreed += 1;
                            *rep.lowered.entry((label, key.field)).or_default() += 1;
                            continue;
                        };
                        find(key, i, d);
                    }
                    continue;
                }
                let Some(k) = k else {
                    if call.is_some() {
                        match cx.attribute(&key) {
                            Some(b) => *rep.buckets.entry(b).or_default() += 1,
                            None => find(key, i, "no production record of this occurrence".into()),
                        }
                    }
                    continue;
                };
                seen.insert(k);
                rep.matched += 1;
                if let Point::NotReached { why, .. } = occ.first {
                    let was_blocked = blocked;
                    if key.field != "timer" {
                        blocked = true;
                    }
                    outs.insert(k, (0, 0));
                    match (p.first.get(&k), allowed(why, was_blocked)) {
                        (Some(First::Ran(_)), _) => {
                            used.insert(k);
                            find(
                                key,
                                i,
                                format!("invoked in production, not in the source ({why:?})"),
                            );
                        }
                        (None, _) => find(key, i, format!("no skip traced ({why:?})")),
                        (Some(First::Skipped(_)), None) => {
                            find(key, i, format!("reference could not decide ({why:?})"));
                        }
                        (Some(First::Skipped(s)), Some(ok)) if ok.contains(s) => rep.agreed += 1,
                        (Some(First::Skipped(s)), Some(ok)) => find(
                            key,
                            i,
                            format!(
                                "skip cause: production {s:?}, source {why:?} (expects {ok:?})"
                            ),
                        ),
                    }
                    continue;
                }
                if let Some(outcome) = call {
                    order.push(k);
                    if key.field != "timer" {
                        blocked = !allows(key.field, outcome);
                        outs.insert(k, expected_out(key.field, outcome));
                    }
                    match (outcome, p.first.get(&k)) {
                        (Outcome::Unanswered(api), _) => {
                            find(key, i, format!("reference could not complete ({api})"));
                            used.insert(k);
                        }
                        (Outcome::Budget, _) => {
                            find(key, i, "reference could not complete (budget)".into());
                            used.insert(k);
                        }
                        (_, Some(First::Skipped(s))) => find(
                            key,
                            i,
                            format!("ran in the source, skipped in production ({s:?})"),
                        ),
                        (_, None) => {
                            find(key, i, "ran in the source, not traced in production".into());
                        }
                        (_, Some(First::Ran(prod))) => {
                            used.insert(k);
                            if toint_differs(key.field, outcome) {
                                *rep.intended.entry(TOINT_PAST_I32).or_default() += 1;
                            }
                            match differ(key.field, *prod, outcome) {
                                None => rep.agreed += 1,
                                Some(d) => {
                                    let hint = luaj_hint(obs, key.callable);
                                    find(key, i, format!("{d}{hint}"));
                                }
                            }
                        }
                    }
                }
                // A timer's summary: start, elapsed, installed start,
                // sampled age, and its outputs.
                let Some(t) = occ.summary else {
                    continue;
                };
                let elapsed = t.start.map(|s| pf.state_us.saturating_sub(s));
                let installed_ok = t.on && elapsed.is_some_and(|e| e >= 0);
                let installed = elapsed
                    .filter(|_| installed_ok)
                    .map(|e| bms_model::TimeUs(pf.now.0.saturating_sub(e.min(MAX_KEY_US))));
                outs.insert(k, (i32::from(t.on), 1));
                let want = (
                    t.on,
                    t.start.filter(|_| t.on),
                    elapsed.filter(|_| t.on),
                    installed,
                );
                match p.timer.get(&k) {
                    None => find(key, i, "timer not traced in production".into()),
                    Some(&got) if got != want => {
                        find(
                            key,
                            i,
                            format!("timer: production {got:?}, reference {want:?}"),
                        );
                    }
                    Some(_) => {}
                }
                let ages = run
                    .timer_slots
                    .get(&k)
                    .and_then(|s| p.sampled.get(s))
                    .cloned()
                    .unwrap_or_default();
                match (elapsed.filter(|_| installed_ok), t.shown) {
                    (Some(_), None) => find(
                        key,
                        i,
                        "the selected source's fact is unknown: sampling undecided".into(),
                    ),
                    (Some(e), Some(true)) => {
                        let age = e.min(MAX_KEY_US);
                        if ages.is_empty() {
                            find(key, i, "no sampled age where the source draws".into());
                        } else if ages.iter().any(|&a| a != age) {
                            find(key, i, format!("sampled ages {ages:?}, source {age}"));
                        }
                    }
                    _ if !ages.is_empty() => {
                        find(
                            key,
                            i,
                            format!("sampled {ages:?} where the source draws nothing"),
                        );
                    }
                    _ => {}
                }
            }
        }
        // Every compiled callback has a source occurrence this frame, or
        // its object is listed (one finding above).
        for k in 0..callbacks {
            if !seen.contains(&k) && !listed_k.contains(&k) {
                find(
                    key_k(k),
                    i,
                    format!("compiled callback {k} with no source point"),
                );
            }
        }
        // The full order of invocations.
        let prod_order: Vec<u16> = p
            .order
            .iter()
            .copied()
            .filter(|k| seen.contains(k))
            .collect();
        if prod_order != order {
            let key = order.first().map_or(UNKNOWN, |&k| key_k(k));
            find(
                key,
                i,
                format!("order: production {prod_order:?}, source {order:?}"),
            );
        }
        // Production invocations with no source occurrence.
        for &k in &p.order {
            if !used.contains(&k) && !listed_k.contains(&k) {
                find(
                    key_k(k),
                    i,
                    format!("production invocation {k} with no source point"),
                );
            }
        }
        // The output cells expressions read.
        for (&k, &(v, ok)) in &outs {
            let at = usize::from(k) * 2;
            let got = (
                pf.outs.get(at).copied().unwrap_or(i32::MIN),
                pf.outs.get(at + 1).copied().unwrap_or(i32::MIN),
            );
            if got != (v, ok) {
                find(
                    key_k(k),
                    i,
                    format!("output: production {got:?}, source {:?}", (v, ok)),
                );
            }
        }
        let agreed = rep.agreed - agreed_before;
        rep.kinds.entry(label).or_default().1 += agreed;
        if i == 0
            || !matches!(run.frames.get(i - 1).map(|f| f.kind), Some(FrameKind::Play { round: r, .. }) if r == round)
        {
            rep.first_agreed.push(agreed);
        }
    }
    // An observer production folded that no frame compared.
    for (j, o) in &cx.by_key {
        if observer_fold(o).is_some()
            && !listed_observers.contains(j)
            && !observer_points.contains_key(j)
            && matches!(
                run.recorded.destinations.get(j.0).map(|e| e.became),
                Some(Became::Retained | Became::Folded)
            )
        {
            let key = Key {
                dest: j.0,
                field: j.1,
                index: j.2,
                callable: j.3,
                path: j.4,
            };
            find(key, 0, "observer: never compared".into());
        }
    }
    rep.observers = observers_agreed;
    rep.observer_offsets = observer_offsets;
    // Stopped roots and their isolation entries, by the kind of frame
    // they stopped on: an entry covering a root's outside stops leaves its
    // play stops to e-1's rule.
    for ((kind, callable), (api, count, key)) in stopped {
        let s = obs.sites.get(&callable).cloned().unwrap_or_default();
        let disposition = cx.attribute(&key).unwrap_or_else(|| "compiled".into());
        let covered = cover(&s, iso, obs, &cx);
        rep.stopped.push(Stopped {
            kind,
            site: s,
            api,
            count,
            disposition,
            covered,
        });
    }
    rep.findings = findings;
    rep
}

/// Whether an isolation entry covers the root defined at `s`: it names
/// the root; every cell it writes has a reader line or `none`; and each
/// listed reader's occurrences carry its disposition and reason, none of
/// them compiled or lowered (s7-06e-1-r1-tables.md, A).
fn cover(s: &(String, u32), iso: &[Entry], obs: &Observed, cx: &Cx<'_>) -> Result<(), String> {
    let Some(e) = iso
        .iter()
        .find(|e| e.line == s.1 && same_file(&s.0, &e.file))
    else {
        return Err("no entry".into());
    };
    for cell in &e.writes {
        if !e.readers.iter().any(|r| &r.cell == cell) && !e.unread.contains(cell) {
            return Err(format!("cell {cell} has no reviewed readers"));
        }
    }
    for r in &e.readers {
        let keys: BTreeSet<Key> = obs
            .frames
            .iter()
            .flat_map(|f| f.points.iter())
            .map(|p| match p {
                Point::Ran { key, .. } | Point::NotReached { key, .. } => *key,
            })
            .chain(
                obs.unmodelled
                    .iter()
                    .chain(&obs.removed)
                    .chain(&obs.unresolved)
                    .flat_map(|l| l.callables.iter().copied()),
            )
            .filter(|k| {
                k.field == r.field
                    && obs
                        .sites
                        .get(&k.callable)
                        .is_some_and(|(f, l)| *l == r.line && same_file(f, &r.file))
            })
            .collect();
        if keys.is_empty() {
            return Err(format!("reader {}:{} not found", r.file, r.line));
        }
        for k in keys {
            if let Some(o) = cx.rec(&k)
                && matches!(o.disposition, "compiled" | "lowered")
            {
                return Err(format!("reader {}:{} is {}", r.file, r.line, o.disposition));
            }
            let got = cx.attribute(&k).unwrap_or_default();
            let ok = match r.evidence.as_str() {
                "replaced" => got.starts_with(&format!("replaced {}", r.reason)),
                "dropped" => got == "dropped",
                _ => false,
            };
            if !ok {
                return Err(format!("reader {}:{} carries {got:?}", r.file, r.line));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use skin_model::present::trace::{Converted, Event, Skip};

    use super::compare;
    use crate::observed::Point;
    use crate::tests::{TAMPERED, pieces};

    /// The reference's `nil` against production's integer 0 agrees only
    /// when the converted output is equal; `nil` against another integer,
    /// and any other raw mismatch, stays a raw finding.
    #[test]
    fn nil_against_zero_agrees_only_with_the_same_output() {
        use skin_model::present::Val;

        use super::differ;
        use crate::observed::{Outcome, Raw};

        let nil = Outcome::Value(Raw::Nil);
        // A value: `nil` converts to 0, given, as production's 0 does.
        assert_eq!(
            differ("value", (Ok(Val::Int(0)), Converted::Int(0, true)), nil),
            None
        );
        // A draw: `nil` is false where production's 0 is true.
        let d = differ("draw", (Ok(Val::Int(0)), Converted::Bool(true)), nil);
        assert!(
            d.as_deref().is_some_and(|d| d.starts_with("converted:")),
            "{d:?}"
        );
        // A value whose 0 production does not give.
        let d = differ("value", (Ok(Val::Int(0)), Converted::Int(0, false)), nil);
        assert!(
            d.as_deref().is_some_and(|d| d.starts_with("converted:")),
            "{d:?}"
        );
        // `nil` against another integer, and a non-nil mismatch.
        for (v, r) in [(Val::Int(7), Raw::Nil), (Val::Int(7), Raw::Int(9))] {
            let d = differ("value", (Ok(v), Converted::Int(7, true)), Outcome::Value(r));
            assert!(d.as_deref().is_some_and(|d| d.starts_with("raw:")), "{d:?}");
        }
    }

    /// Where the source's `toint()` differs from the rule, counting that
    /// difference lets no wrong production conversion pass: on the frames
    /// whose value is past `i32`, a changed converted value, a changed
    /// valid flag and a changed raw value (still past `i32`) are each
    /// found.
    #[test]
    fn a_wrong_conversion_past_i32_is_found() {
        use skin_model::present::{Fail, Val};

        use super::TOINT_PAST_I32;
        use crate::tests::PAST_I32;

        let (run, obs) = pieces(PAST_I32);
        // Production hides 2e10 (the source's low 32 bits: −1 474 836 480).
        assert!(
            run.frames.iter().flat_map(|f| &f.events).any(|e| matches!(
                e,
                Event::Ran {
                    result: Ok(Val::Float(v)),
                    conv: Converted::Int(i32::MAX, false),
                    ..
                } if *v == 2e10
            )),
            "no value of 2e10 hidden"
        );
        let r = compare(&run, &obs, &[]);
        assert!(r.passes(), "{}", r.text(0, true, true));
        assert!(r.intended.get(TOINT_PAST_I32).is_some_and(|&n| n > 0));
        let found = |f: &dyn Fn(&mut Result<Val, Fail>, &mut Converted)| {
            let (mut run, obs) = pieces(PAST_I32);
            let mut tampered = 0;
            for fr in &mut run.frames {
                for e in &mut fr.events {
                    if let Event::Ran { result, conv, .. } = e
                        && *conv == Converted::Int(i32::MAX, false)
                    {
                        f(result, conv);
                        tampered += 1;
                    }
                }
            }
            assert!(tampered > 0);
            let r = compare(&run, &obs, &[]);
            assert!(!r.findings.is_empty(), "{}", r.text(0, true, true));
        };
        found(&|_, c| *c = Converted::Int(i32::MIN, false));
        found(&|_, c| *c = Converted::Int(i32::MAX, true));
        found(&|r, _| {
            if let Ok(Val::Float(v)) = r {
                *v *= 2.0;
            }
        });
    }

    /// R13 in production's trace: the frame 1 µs after a latch set at a
    /// state clock of …999 µs is sampled 1 µs old, where two truncating
    /// divisions would say 1 000.
    #[test]
    fn a_sub_ms_start_is_sampled_at_1_us() {
        let (body, script) = crate::tests::sub_ms();
        let (run, _) = crate::tests::pieces_over(&body, script);
        let ages: Vec<i64> = run
            .frames
            .iter()
            .flat_map(|f| &f.events)
            .filter_map(|e| match e {
                Event::Sampled { age_us, .. } => Some(*age_us),
                _ => None,
            })
            .collect();
        assert_eq!(ages.first(), Some(&0), "{ages:?}");
        assert_eq!(ages.get(1), Some(&1), "{ages:?}");
    }

    /// The untouched run agrees; each alteration of production's side is
    /// found.
    #[test]
    fn altered_traces_are_found() {
        let (run, obs) = pieces(TAMPERED);
        assert!(compare(&run, &obs, &[]).passes());
        let found = |f: &dyn Fn(&mut super::ProductionRun)| {
            let (mut run, obs) = pieces(TAMPERED);
            f(&mut run);
            let r = compare(&run, &obs, &[]);
            assert!(!r.findings.is_empty(), "{}", r.text(0, true, true));
        };
        // A changed conversion of an agreeing raw value.
        found(&|run| {
            for f in &mut run.frames {
                for e in &mut f.events {
                    if let Event::Ran { conv, .. } = e {
                        *conv = Converted::Long(Some(7));
                    }
                }
            }
        });
        // A changed installed start.
        found(&|run| {
            for f in &mut run.frames {
                for e in &mut f.events {
                    if let Event::Timer {
                        installed: Some(t), ..
                    } = e
                    {
                        t.0 += 1;
                    }
                }
            }
        });
        // A changed sampled age, and a missing one.
        found(&|run| {
            for f in &mut run.frames {
                for e in &mut f.events {
                    if let Event::Sampled { age_us, .. } = e {
                        *age_us += 1;
                    }
                }
            }
        });
        found(&|run| {
            for f in &mut run.frames {
                f.events.retain(|e| !matches!(e, Event::Sampled { .. }));
            }
        });
        // An extra invocation, and a missing one.
        found(&|run| {
            if let Some(f) = run.frames.first_mut()
                && let Some(e) = f
                    .events
                    .iter()
                    .find(|e| matches!(e, Event::Ran { .. }))
                    .copied()
            {
                f.events.push(e);
            }
        });
        found(&|run| {
            for f in &mut run.frames {
                f.events.retain(|e| !matches!(e, Event::Ran { .. }));
            }
        });
    }

    /// R15: an occurrence with no production record is a finding; R17: a
    /// compiled callable on an object the reference does not run is one.
    #[test]
    fn unrecorded_and_unmodelled_occurrences_are_found() {
        let (mut run, obs) = pieces(TAMPERED);
        run.recorded
            .occurrences
            .retain(|o| o.disposition != "lowered");
        let r = compare(&run, &obs, &[]);
        assert!(
            r.findings
                .iter()
                .any(|f| f.what.contains("no production record")),
            "{}",
            r.text(0, true, true)
        );
        let (run, mut obs) = pieces(TAMPERED);
        let key = obs.frames[0].timers[0].key;
        obs.unmodelled.push(crate::observed::Listed {
            dest: key.dest,
            what: "text".into(),
            callables: vec![key],
        });
        let r = compare(&run, &obs, &[]);
        assert!(
            r.findings.iter().any(|f| f.what.contains("unmodelled")),
            "{}",
            r.text(0, true, true)
        );
    }

    /// Completeness (s7-06e-1-r1-tables.md, A): a duplicated invocation,
    /// a deleted or changed skip, reordered invocations, a changed output
    /// cell, a missing frame and a missing source point are each found.
    #[test]
    fn incomplete_or_extra_events_are_found() {
        type Tamper = dyn Fn(&mut super::ProductionRun, &mut crate::observed::Observed);
        let found = |what: &str, f: &Tamper| {
            let (mut run, mut obs) = pieces(TAMPERED);
            f(&mut run, &mut obs);
            let r = compare(&run, &obs, &[]);
            assert!(!r.findings.is_empty(), "{what}: {}", r.text(0, true, true));
        };
        let (run, obs) = pieces(TAMPERED);
        let r = compare(&run, &obs, &[]);
        assert!(r.passes(), "{}", r.text(0, true, true));
        assert!(
            run.frames
                .iter()
                .any(|f| f.events.iter().any(|e| matches!(e, Event::Skipped { .. }))),
            "the fixture skips"
        );
        found("duplicate", &|run, _| {
            for f in &mut run.frames {
                if let Some(e) = f
                    .events
                    .iter()
                    .find(|e| matches!(e, Event::Ran { .. }))
                    .copied()
                {
                    f.events.push(e);
                }
            }
        });
        found("skip deleted", &|run, _| {
            for f in &mut run.frames {
                f.events.retain(|e| !matches!(e, Event::Skipped { .. }));
            }
        });
        found("skip cause changed", &|run, _| {
            for f in &mut run.frames {
                for e in &mut f.events {
                    if let Event::Skipped { why, .. } = e {
                        *why = Skip::After;
                    }
                }
            }
        });
        found("invocations reordered", &|run, _| {
            for f in &mut run.frames {
                let at: Vec<usize> = f
                    .events
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| matches!(e, Event::Ran { .. }))
                    .map(|(i, _)| i)
                    .collect();
                if let [a, b, ..] = at[..] {
                    f.events.swap(a, b);
                }
            }
        });
        found("output changed", &|run, _| {
            for f in &mut run.frames {
                if let Some(v) = f.outs.first_mut() {
                    *v += 1;
                }
            }
        });
        found("frame missing", &|run, _| {
            run.frames.pop();
        });
        found("timer summaries cleared", &|_, obs| {
            for f in &mut obs.frames {
                f.timers.clear();
            }
        });
        found("a timer event for a skipped draw", &|run, _| {
            for f in &mut run.frames {
                let skipped = f.events.iter().find_map(|e| match e {
                    Event::Skipped { k, .. } => Some(*k),
                    _ => None,
                });
                if let Some(k) = skipped {
                    f.events.push(Event::Timer {
                        k,
                        on: false,
                        start: None,
                        elapsed_us: None,
                        installed: None,
                    });
                }
            }
        });
        found("a program slot sampled while off", &|run, _| {
            let slot = run.timer_slots.values().next().copied().unwrap_or(0);
            if let Some(f) = run.frames.first_mut() {
                f.events.push(Event::Sampled { slot, age_us: 0 });
            }
        });
        found("a source point twice", &|_, obs| {
            if let Some(f) = obs.frames.first_mut()
                && let Some(pt) = f
                    .points
                    .iter()
                    .find(|p| matches!(p, crate::observed::Point::NotReached { .. }))
                    .copied()
            {
                f.points.push(pt);
                if let Some(r) = f.objects.last_mut() {
                    r.end = f.points.len();
                }
            }
        });
        found("source point missing", &|_, obs| {
            if let Some(f) = obs.frames.first_mut() {
                f.points
                    .retain(|p| !matches!(p, crate::observed::Point::NotReached { .. }));
            }
        });
    }

    /// A2: a sample of a program timer's slot whose callback was skipped
    /// (no installed start) is an orphan; one with an installed start is
    /// not.
    #[test]
    fn shapes_reject_an_orphan_sample() {
        use super::{Schema, production_shapes};
        let slots: std::collections::BTreeMap<u16, usize> = [(0, 7)].into();
        let is_timer = |k: u16| k == 0;
        let schema = Schema {
            callbacks: 1,
            is_timer: &is_timer,
            timer_slots: &slots,
            slotted: &[],
        };
        let orphan = [
            Event::Skipped {
                k: 0,
                why: Skip::Gate,
            },
            Event::Sampled { slot: 7, age_us: 0 },
        ];
        let (_, bad) = production_shapes(&orphan, &schema);
        assert!(
            bad.iter().any(|b| b.2.contains("no installed start")),
            "{bad:?}"
        );
        let fine = [
            Event::Ran {
                k: 0,
                result: Ok(skin_model::present::Val::Int(5)),
                conv: Converted::Long(Some(5)),
            },
            Event::Timer {
                k: 0,
                on: true,
                start: Some(5),
                elapsed_us: Some(0),
                installed: Some(bms_model::TimeUs(0)),
            },
            Event::Sampled { slot: 7, age_us: 0 },
        ];
        let (_, bad) = production_shapes(&fine, &schema);
        assert!(bad.is_empty(), "{bad:?}");
    }

    /// Inserts `pt` after the point at `at` in a source frame, its object
    /// growing by one.
    fn insert_point(f: &mut crate::observed::RefFrame, at: usize, pt: Point) {
        f.points.insert(at + 1, pt);
        for r in &mut f.objects {
            if r.start > at {
                r.start += 1;
            }
            if r.end > at {
                r.end += 1;
            }
        }
    }

    fn is_timer(p: &Point) -> bool {
        matches!(p, Point::Ran { key, .. } if key.field == "timer")
    }

    /// A2 (r3): each direction's catalogue is checked by itself. Every
    /// mutation of the source's frame and every mutation of production's
    /// events is found, a timer invoked twice on either side included.
    #[test]
    #[expect(clippy::too_many_lines, reason = "one case per mutation")]
    fn catalogue_mutations_are_found() {
        let found =
            |what: &str,
             needle: &str,
             f: &dyn Fn(&mut super::ProductionRun, &mut crate::observed::Observed)| {
                let (mut run, mut obs) = pieces(TAMPERED);
                f(&mut run, &mut obs);
                let r = compare(&run, &obs, &[]);
                assert!(
                    r.findings.iter().any(|x| x.what.contains(needle)),
                    "{what}: {}",
                    r.text(0, true, true)
                );
            };
        let (run, obs) = pieces(TAMPERED);
        assert!(compare(&run, &obs, &[]).passes());
        assert!(
            obs.frames.iter().any(|f| f.points.iter().any(is_timer)),
            "the fixture runs a timer"
        );
        // Source: an installed writer omitted, its points and its range.
        found("omitted writer", "do not partition", &|_, obs| {
            for f in &mut obs.frames {
                let r = f.objects.remove(0);
                let n = r.len();
                f.points.drain(r);
                for o in &mut f.objects {
                    o.start -= n;
                    o.end -= n;
                }
            }
        });
        // Source: overlapping ranges.
        found("overlap", "do not partition", &|_, obs| {
            for f in &mut obs.frames {
                if let [a, b, ..] = &mut f.objects[..] {
                    b.start = a.start;
                }
            }
        });
        // Source and production: a timer invoked twice.
        found("source: a timer point twice", "do not expect", &|_, obs| {
            for f in &mut obs.frames {
                if let Some(at) = f.points.iter().position(is_timer) {
                    let pt = f.points[at];
                    insert_point(f, at, pt);
                }
            }
        });
        found(
            "production: a timer invoked twice",
            "a second event",
            &|run, _| {
                for f in &mut run.frames {
                    let timer = f
                        .events
                        .iter()
                        .find(|e| {
                            matches!(
                                e,
                                Event::Ran {
                                    conv: Converted::Long(_),
                                    ..
                                }
                            )
                        })
                        .copied();
                    f.events.extend(timer);
                }
            },
        );
        // Source and production: a summary of a non-timer.
        found(
            "source non-timer summary",
            "a summary of no invoked timer",
            &|_, obs| {
                for f in &mut obs.frames {
                    let draw = f.points.iter().find_map(|p| match p {
                        Point::Ran { key, .. } if key.field != "timer" => Some(*key),
                        _ => None,
                    });
                    if let (Some(key), Some(t)) = (draw, f.timers.first().copied()) {
                        f.timers.push(crate::observed::TimerOut { key, ..t });
                    }
                }
            },
        );
        found(
            "production non-timer summary",
            "a timer event of a non-timer",
            &|run, _| {
                let draw = (0..u16::try_from(run.callbacks).unwrap_or(0)).find(|&k| {
                    run.recorded
                        .occurrences
                        .iter()
                        .any(|o| o.program == Some(k) && o.field != "timer")
                });
                if let (Some(k), Some(f)) = (draw, run.frames.first_mut()) {
                    f.events.push(Event::Timer {
                        k,
                        on: false,
                        start: None,
                        elapsed_us: None,
                        installed: None,
                    });
                }
            },
        );
        // Production: a skip of a callback outside the program.
        found("out-of-range skip", "outside the program", &|run, _| {
            let k = u16::try_from(run.callbacks).unwrap_or(u16::MAX);
            if let Some(f) = run.frames.first_mut() {
                f.events.push(Event::Skipped { k, why: Skip::Gate });
            }
        });
    }

    /// A4 (r3): isolation evidence is checked per occurrence. A reader
    /// drawn by two destinations is two occurrences; changing the record
    /// of the first alone fails the entry.
    #[test]
    fn isolation_evidence_is_per_occurrence() {
        let body = "local n\n\
             skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
               value = function() if ms.float_number(114) ~= 0 then n = 1 else n = 2 end return n end } }\n\
             local rd = function() return n ~= nil end\n\
             table.insert(skin.destination, { id = \"i\", draw = rd, dst = { { x = 0, y = 0, w = 6, h = 6 } } })\n\
             table.insert(skin.destination, { id = \"i\", draw = rd, dst = { { x = 9, y = 0, w = 6, h = 6 } } })\n\
             table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n";
        let (run, obs) = pieces(body);
        let bare = compare(&run, &obs, &[]);
        let line = bare.stopped.first().map_or(0, |s| s.site.1);
        assert!(line > 0, "{}", bare.text(0, true, true));
        let draws: Vec<(Option<usize>, &str, Option<String>)> = run
            .recorded
            .occurrences
            .iter()
            .filter(|o| o.field == "draw")
            .map(|o| (o.destination, o.disposition, o.reason.clone()))
            .collect();
        assert_eq!(draws.len(), 2, "{draws:?}");
        let iso = crate::isolation::parse(&format!(
            "root s.luaskin {line} writes n\nreader n s.luaskin {} draw replaced Interference\n",
            line + 1
        ))
        .unwrap();
        let ok = compare(&run, &obs, &iso);
        assert!(
            !ok.stopped.is_empty() && ok.stopped.iter().all(|s| s.covered.is_ok()),
            "{draws:?} {}",
            ok.text(0, true, true)
        );
        // The first destination's record changed alone.
        let mut changed = run;
        if let Some(first) = changed
            .recorded
            .occurrences
            .iter_mut()
            .find(|o| o.field == "draw" && o.destination == Some(0))
        {
            first.disposition = "dropped";
            first.reason = None;
        }
        let r = compare(&changed, &obs, &iso);
        assert!(
            r.stopped.iter().any(|s| s.covered.is_err()),
            "{}",
            r.text(0, true, true)
        );
    }

    /// R19: the reference import must equal production's.
    #[test]
    fn a_different_import_is_refused() {
        let (run, _) = pieces(TAMPERED);
        let mut other = run.recorded.clone();
        assert!(run.same_import(&other, &run.structure));
        if let Some(o) = other.occurrences.first_mut() {
            o.program = Some(99);
        }
        assert!(!run.same_import(&other, &run.structure));
        // Programs differing in a constant, a read, a gate or a slot, with
        // the same occurrence records, differ in structure.
        let base = |c: &str, read: &str, op: &str, init: &str| {
            format!(
                "local latch = (function()\n\
                   local state = {init}\n\
                   return function()\n\
                     if state == ms.timer_off_value and ms.number(100) == 200000 then state = ms.time() end\n\
                     return state\n\
                   end\n\
                 end)()\n\
                 table.insert(skin.destination, {{ id = \"i\", timer = latch, loop = -1, dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }})\n\
                 table.insert(skin.destination, {{ id = \"i\", op = {{ {op} }}, draw = function() return ms.number({read}) / {c} >= 0 end, dst = {{ {{ x = 100, y = 0, w = 64, h = 64 }} }} }})\n"
            )
        };
        let (a, _) = pieces(&base("2", "100", "32", "ms.timer_off_value"));
        for (what, body) in [
            ("constant", base("3", "100", "32", "ms.timer_off_value")),
            ("read", base("2", "101", "32", "ms.timer_off_value")),
            ("gate", base("2", "100", "42", "ms.timer_off_value")),
            ("slot", base("2", "100", "32", "-1")),
        ] {
            let (b, _) = pieces(&body);
            assert_eq!(a.recorded.occurrences, b.recorded.occurrences, "{what}");
            assert!(!a.same_import(&b.recorded, &b.structure), "{what}");
        }
    }

    /// s7-06e-2-design.md §6: the rounds' and outside frames' rules,
    /// each broken on one side, are found by their own message.
    #[test]
    fn rounds_and_outside_mutations_are_found() {
        use crate::production::FrameKind;
        use crate::script::Script;
        use crate::tests::{LATCH, dst, pieces_over};
        let body = format!(
            "{LATCH}{}{}",
            dst("-111", 0, "timer = latch, loop = -1,"),
            dst("\"i\"", 100, "timer = latch, loop = -1,")
        );
        let (run, obs) = pieces_over(&body, Script::rounds());
        let r = compare(&run, &obs, &[]);
        assert!(r.passes(), "{}", r.text(0, true, true));
        let found = |what: &str,
                     f: &dyn Fn(
            &mut crate::production::ProductionRun,
            &mut crate::observed::Observed,
        )| {
            let (mut run, mut obs) = pieces_over(&body, Script::rounds());
            f(&mut run, &mut obs);
            let r = compare(&run, &obs, &[]);
            assert!(
                r.findings.iter().any(|x| x.what.contains(what)),
                "{what}: {}",
                r.text(0, true, true)
            );
        };
        // A production event on an outside frame, and a sample on a page
        // (a page builds nothing, so even a built-in timer's is one).
        found("production event on an outside frame", &|run, _| {
            run.frames[0].events.push(Event::Skipped {
                k: 0,
                why: Skip::Gate,
            });
        });
        found("production event on an outside frame", &|run, _| {
            let f = run
                .frames
                .iter_mut()
                .find(|f| matches!(f.kind, FrameKind::Page))
                .unwrap();
            f.events.push(Event::Sampled { slot: 0, age_us: 0 });
        });
        // An outside frame's catalogue broken: a point omitted.
        found("partition", &|_, obs| {
            let f = obs.frames.iter_mut().find(|f| f.outside).unwrap();
            f.points.pop();
        });
        // The frame kinds disagreeing.
        found("frame kind", &|_, obs| {
            obs.frames[0].outside = false;
        });
    }

    /// The observer comparison's rules, each broken on one side, are found
    /// by their own message (design.md "beatoraja Lua Skins", the
    /// reference executor): over a practice session with a true observer
    /// (destination 0), a false one (1) and an untimed image (2).
    #[test]
    #[expect(clippy::too_many_lines, reason = "one case per rule, each a few lines")]
    fn observer_mutations_are_found() {
        use super::LEFT;
        use crate::observed::{Cause, Listed, Observed, Outcome};
        use crate::production::{FrameKind, ProductionRun};
        use crate::report::Finding;
        use crate::script::Script;
        use crate::tests::pieces_over;
        use skin_beatoraja::lua::survey::Became;
        const STEP: i64 = 4_167;
        let body = crate::observer_tests::twins();
        let (run, obs) = pieces_over(&body, Script::rounds());
        let r = compare(&run, &obs, &[]);
        assert!(r.passes(), "{}", r.text(0, true, true));
        // The findings `f` gives whose text holds `what`.
        let found = |what: &str, f: &dyn Fn(&mut ProductionRun, &mut Observed)| -> Vec<Finding> {
            let (mut run, mut obs) = pieces_over(&body, Script::rounds());
            f(&mut run, &mut obs);
            let r = compare(&run, &obs, &[]);
            let hits: Vec<Finding> = r
                .findings
                .iter()
                .filter(|x| x.what.contains(what))
                .cloned()
                .collect();
            assert!(!hits.is_empty(), "{what}: {}", r.text(0, true, true));
            hits
        };
        // Destination `d`'s own elements.
        let elements = |run: &ProductionRun, d: usize| -> Vec<u32> {
            run.recorded.destinations[d]
                .elements
                .clone()
                .map(|e| u32::try_from(e).unwrap())
                .collect()
        };
        // Destination 0's timer ages shifted by STEP on the frames `on` picks.
        let shift = |run: &mut ProductionRun, on: &dyn Fn(usize, &ProductionRun) -> bool| {
            let els = elements(run, 0);
            let pick: Vec<bool> = (0..run.frames.len()).map(|i| on(i, run)).collect();
            for (f, p) in run.frames.iter_mut().zip(pick) {
                if p {
                    for (e, t) in &mut f.element_timer_ages {
                        if els.contains(e) {
                            *t = t.map(|v| v + STEP);
                        }
                    }
                }
            }
        };
        // `edit` on the observer timers of destination `d` from frame
        // `from`, the first `n` of them.
        let timers = |obs: &mut Observed,
                      (d, from, n): (usize, usize, usize),
                      edit: &dyn Fn(&mut crate::observed::TimerOut)| {
            obs.frames
                .iter_mut()
                .skip(from)
                .flat_map(|f| f.timers.iter_mut())
                .filter(|t| t.key.dest == d)
                .take(n)
                .for_each(edit);
        };
        let round_end = |run: &ProductionRun, round: usize| {
            run.frames
                .iter()
                .rposition(|f| matches!(f.kind, FrameKind::Play { round: r, .. } if r == round))
                .unwrap()
        };
        // (a) production's ages a frame later from round 1's READY: the
        // first finding is there.
        let hits = found("observer age", &|run, _| {
            let from = ready_of(run, 0);
            shift(run, &|i, _| i >= from);
        });
        let (run0, _) = pieces_over(&body, Script::rounds());
        assert_eq!(hits.iter().map(|h| h.frame).min(), Some(ready_of(&run0, 0)));
        // (b) on round 2 only.
        found("observer age", &|run, _| {
            let (from, to) = (ready_of(run, 1), round_end(run, 1));
            shift(run, &|i, _| (from..=to).contains(&i));
        });
        // (c) on every frame: a uniform shift of one side.
        found("observer age", &|run, _| shift(run, &|_, _| true));
        // (d) on loading frames only.
        found("observer age", &|run, _| {
            shift(run, &|i, run| {
                matches!(run.frames[i].kind, FrameKind::Loading)
            });
        });
        // (e) the reference's start later from round 2.
        found("observer start", &|run, obs| {
            timers(obs, (0, ready_of(run, 1), usize::MAX), &|t| {
                t.start = t.start.map(|s| s + STEP);
            });
        });
        // (f) the start restarted at round 2's first frame (no carry).
        found("observer start", &|run, obs| {
            let i = ready_of(run, 1);
            let s = run.frames[i].state_us;
            timers(obs, (0, i, usize::MAX), &|t| t.start = Some(s));
        });
        // (g) the false observer on in the reference.
        found("observer: production false, reference true", &|_, obs| {
            timers(obs, (1, 0, usize::MAX), &|t| t.on = true);
        });
        // (h) the start changed on one page frame.
        found("observer start", &|run, obs| {
            let i = run
                .frames
                .iter()
                .position(|f| matches!(f.kind, FrameKind::Page))
                .unwrap();
            timers(obs, (0, i, 1), &|t| t.start = t.start.map(|s| s + STEP));
        });
        // (i) the reference unable to answer on a loading frame: a
        // finding, not a stopped root.
        {
            let (run, mut obs) = pieces_over(&body, Script::rounds());
            let p = obs.frames[0]
                .points
                .iter_mut()
                .find(|p| matches!(p, Point::Ran { key, .. } if key.dest == 0))
                .unwrap();
            let Point::Ran { outcome, .. } = p else {
                unreachable!()
            };
            *outcome = Outcome::Unanswered("x");
            let r = compare(&run, &obs, &[]);
            assert!(
                r.findings
                    .iter()
                    .any(|f| f.frame == 0 && f.what == "observer: reference could not complete (x)"),
                "{}",
                r.text(0, true, true)
            );
            assert!(r.stopped.is_empty(), "{:?}", r.stopped);
        }
        // (i') the reference unable to decide whether the timer is read.
        found(
            "observer: reference could not decide (Inconclusive)",
            &|_, obs| {
                let p = obs.frames[0]
                    .points
                    .iter_mut()
                    .find(|p| matches!(p, Point::Ran { key, .. } if key.dest == 0))
                    .unwrap();
                let Point::Ran { key, .. } = *p else {
                    unreachable!()
                };
                *p = Point::NotReached {
                    key,
                    why: Cause::Inconclusive,
                };
            },
        );
        // (j) the false observer's destination kept.
        found(
            "observer: a false fold whose destination became Retained",
            &|run, _| {
                run.recorded.destinations[1].became = Became::Retained;
            },
        );
        // (k) production's record of the false observer folded to true;
        // alone, its folded destination is the finding; with the
        // destination kept, the fold against the reference's off.
        found(
            "observer: a true fold whose destination became Folded",
            &|run, _| {
                for o in &mut run.recorded.occurrences {
                    if o.destination == Some(1)
                        && let Some(l) = o.lowered.as_mut()
                    {
                        l.src = "true".to_owned();
                    }
                }
            },
        );
        found("observer: production true, reference false", &|run, _| {
            run.recorded.destinations[1].became = Became::Retained;
            for o in &mut run.recorded.occurrences {
                if o.destination == Some(1)
                    && let Some(l) = o.lowered.as_mut()
                {
                    l.src = "true".to_owned();
                }
            }
        });
        // (l) the reference holding no observer.
        found("the reference holds no observer", &|_, obs| {
            obs.observers.clear()
        });
        // (m) element-tied: the observer's own element's timer age not
        // recorded while the untimed image's stays.
        found("observer age", &|run, _| {
            let els = elements(run, 0);
            for f in &mut run.frames {
                f.element_timer_ages.retain(|(e, _)| !els.contains(e));
            }
        });
        // (n) an observer on an object the reference does not model: one
        // located finding, not compared.
        found(
            "observer on a unmodelled object, not compared (x)",
            &|_, obs| {
                let key = obs.frames[0]
                    .timers
                    .iter()
                    .find(|t| t.key.dest == 0)
                    .unwrap()
                    .key;
                obs.unmodelled.push(Listed {
                    dest: 0,
                    what: "x".into(),
                    callables: vec![key],
                });
            },
        );
        // (o) a true observer on a destination the import dropped: the
        // bucket, no finding.
        {
            let (mut run, obs) = pieces_over(&body, Script::rounds());
            run.recorded.destinations[0].became = Became::Dropped;
            let r = compare(&run, &obs, &[]);
            assert!(
                r.buckets.get(LEFT).copied().unwrap_or(0) > 0,
                "{}",
                r.text(0, true, true)
            );
            assert!(
                r.findings.iter().all(|f| !f.what.starts_with("observer")),
                "{}",
                r.text(0, true, true)
            );
        }
        // (p) the true observer's record missing while its destination is
        // kept: a finding, not the bucket; (q) missing with its destination
        // dropped: the bucket, no finding.
        let unrecorded = |run: &mut ProductionRun| {
            run.recorded
                .occurrences
                .retain(|o| !(o.destination == Some(0) && o.field == "timer"));
        };
        found(
            "observer: no production record, its destination Retained",
            &|run, _| {
                unrecorded(run);
            },
        );
        {
            let (mut run, obs) = pieces_over(&body, Script::rounds());
            unrecorded(&mut run);
            run.recorded.destinations[0].became = Became::Dropped;
            let r = compare(&run, &obs, &[]);
            assert!(
                r.buckets.get(LEFT).copied().unwrap_or(0) > 0,
                "{}",
                r.text(0, true, true)
            );
            assert!(
                r.findings.iter().all(|f| !f.what.starts_with("observer")),
                "{}",
                r.text(0, true, true)
            );
        }
    }

    /// The probe of the score tests: Rm's `getDummyNumber` shape, the EX
    /// score behind a guard on property 101, so it reads 0 before a round's
    /// first judgment on both sides (the source's `MIN_VALUE` and the
    /// play's own value for no judgment both fail the guard).
    const SCORE_PROBE: &str = "skin.value = { { id = \"s\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
           value = function() if ms.number(101) < 1 then return 0 else return ms.number(71) end end } }\n\
         table.insert(skin.destination, { id = \"s\", dst = { { x = 0, y = 0, w = 60, h = 6 } } })\n";

    /// The first frame of round `round` (from 0) at READY.
    fn ready_of(run: &crate::production::ProductionRun, round: usize) -> usize {
        run.frames
            .iter()
            .position(|f| {
                matches!(
                    f.kind,
                    super::super::FrameKind::Play {
                        round: r,
                        stage: super::super::Stage::Ready,
                    } if r == round
                )
            })
            .unwrap()
    }

    /// A wrong lowered expression reading the score on a later round's
    /// READY is a finding: the probe agrees on every play frame as it is,
    /// and `max(ex_score, 1)` in its place gives 1 at round 2's READY
    /// where both sides read 0.
    #[test]
    fn a_wrong_score_expression_stays_a_finding() {
        use crate::script::Script;
        use crate::tests::pieces_over;
        let (run, obs) = pieces_over(SCORE_PROBE, Script::rounds());
        let clean = compare(&run, &obs, &[]);
        assert!(
            clean
                .findings
                .iter()
                .all(|f| !f.place.starts_with("round ")),
            "{}",
            clean.text(0, true, true)
        );
        let (mut run, obs) = pieces_over(SCORE_PROBE, Script::rounds());
        let mut n = 0;
        for o in &mut run.recorded.occurrences {
            if let Some(l) = o.lowered.as_mut() {
                l.src = "max(ex_score, 1)".to_owned();
                n += 1;
            }
        }
        assert_eq!(n, 1);
        let r = compare(&run, &obs, &[]);
        assert!(
            r.findings
                .iter()
                .any(|f| f.place == "round 2 ready" && f.what.starts_with("lowered:")),
            "{}",
            r.text(0, true, true)
        );
        assert!(
            r.buckets.keys().all(|b| !b.contains("score")),
            "{}",
            r.text(0, true, true)
        );
    }

    /// The previous round's score fields on a later round's READY (a stale
    /// carry), injected after the record loop's self-check, are a lowered
    /// finding there, never an attribution.
    #[test]
    fn stale_score_fields_on_a_later_round_are_found() {
        use crate::script::Script;
        use crate::tests::pieces_over;
        use skin_model::field;
        let (mut run, obs) = pieces_over(SCORE_PROBE, Script::rounds());
        let i = ready_of(&run, 1);
        let f = &mut run.frames[i];
        for (slot, v) in [
            (field::EX_SCORE, 14),
            (field::RATE_CURRENT_BP, 10_000),
            (field::RATE_WHOLE_BP, 10_000),
        ] {
            f.fields[usize::from(slot)] = v;
        }
        let r = compare(&run, &obs, &[]);
        assert!(
            r.findings
                .iter()
                .any(|f| f.place == "round 2 ready" && f.what.starts_with("lowered:")),
            "{}",
            r.text(0, true, true)
        );
        assert!(
            r.buckets.keys().all(|b| !b.contains("score")),
            "{}",
            r.text(0, true, true)
        );
    }
}
