//! The comparator (s7-06e-design.md §3.6-§3.7): every source invocation
//! matched to an agreeing production invocation or attributed through
//! production's record of that very consumer occurrence, every production
//! invocation matched; agreement exact, the first difference of a pair
//! its finding.

use std::collections::{BTreeMap, BTreeSet};

use skin_beatoraja::lua::replay::Occurrence;
use skin_beatoraja::lua::survey::Became;
use skin_model::MAX_ELAPSED_MS;
use skin_model::present::trace::{Converted, Event, Skip};
use skin_model::present::{Fail, Val};

use super::{ProductionRun, last_records};
use crate::isolation::{Entry, same_file};
use crate::observed::{Cause, Key, Observed, Outcome, Point, Raw, RefFrame, TimerOut};
use crate::report::{Finding, Report, Stopped};

/// LuaJ's `toint()` of a value (`SkinLuaAccessor.java:640-651`): the
/// double saturated to a `long` (NaN 0), then its low 32 bits; nil and
/// booleans 0.
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
            let n = luaj_toint(r);
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
/// reference's, if any.
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

/// An occurrence's identity as the comparator joins on it.
type Join = (usize, &'static str, u32, u32);

fn join(k: &Key) -> Join {
    (k.dest, k.field, k.index, k.callable)
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

/// A timer callback's trace: on, its start, its elapsed ms and the start
/// installed.
type TimerTrace = (bool, Option<i64>, Option<i64>, Option<bms_model::TimeUs>);

/// One invocation's raw result and its conversion.
type Invocation = (Result<Val, Fail>, Converted);

/// A program callback's call-1 event.
#[derive(Clone, Copy)]
enum First {
    Ran(Invocation),
    Skipped(Skip),
}

/// One frame's production events after [`shapes`]: one of each.
#[derive(Default)]
struct Prod {
    first: BTreeMap<u16, First>,
    second: BTreeMap<u16, Invocation>,
    timer: BTreeMap<u16, TimerTrace>,
    /// Every invocation, in order.
    order: Vec<(u16, u8)>,
    sampled: BTreeMap<usize, Vec<i64>>,
}

/// One frame's reference points after [`shapes`]: each occurrence's
/// call-1 point and call-2 outcome, and each invoked timer's summary.
#[derive(Default)]
struct Refs {
    first: BTreeMap<Join, Point>,
    second: BTreeMap<Join, Outcome>,
    timers: BTreeMap<Join, TimerOut>,
}

/// A shape finding: production's callback or the source's occurrence it
/// concerns, and what is wrong.
type Misshapen = (Option<u16>, Option<Key>, String);

/// The frame's events on both sides, checked for shape before anything is
/// compared (s7-06e-1-r1-tables.md, A2): every finding of a shape that
/// is not exactly once, an orphan or a reuse, and the events kept once.
#[expect(clippy::too_many_lines, reason = "one rule per shape, both sides")]
fn shapes(
    events: &[Event],
    rf: &RefFrame,
    is_timer: &dyn Fn(u16) -> bool,
    timer_slots: &BTreeMap<u16, usize>,
) -> (Prod, Refs, Vec<Misshapen>) {
    let mut p = Prod::default();
    let mut bad: Vec<Misshapen> = Vec::new();
    let mut kbad = |k: u16, what: String| bad.push((Some(k), None, what));
    for e in events {
        match *e {
            Event::Ran {
                k,
                call,
                result,
                conv,
            } => {
                p.order.push((k, call));
                if call == 1 {
                    if p.first.insert(k, First::Ran((result, conv))).is_some() {
                        kbad(k, format!("callback {k}: a second call-1 event"));
                    }
                } else if !is_timer(k) || call != 2 {
                    kbad(k, format!("callback {k}: call {call} of a non-timer"));
                } else if p.second.insert(k, (result, conv)).is_some() {
                    kbad(k, format!("callback {k}: a second call-2 event"));
                }
            }
            Event::Skipped { k, why } => {
                if p.first.insert(k, First::Skipped(why)).is_some() {
                    kbad(k, format!("callback {k}: skipped and another call-1 event"));
                }
            }
            Event::Timer {
                k,
                on,
                start,
                elapsed_ms,
                installed,
            } => {
                if !is_timer(k) {
                    kbad(k, format!("callback {k}: a timer event of a non-timer"));
                } else if p
                    .timer
                    .insert(k, (on, start, elapsed_ms, installed))
                    .is_some()
                {
                    kbad(k, format!("timer {k}: a second timer event"));
                }
            }
            Event::Sampled { slot, age_us } => p.sampled.entry(slot).or_default().push(age_us),
        }
    }
    // Production's orphans: a second call without an on first, a timer
    // event without an invocation, an invoked timer without its event, a
    // program slot sampled without an installed start.
    for (&k, f) in &p.first {
        let on = matches!(f, First::Ran((_, Converted::Long(Some(v)))) if *v != i64::MIN);
        if p.second.contains_key(&k) && !on {
            kbad(k, format!("timer {k}: a second call after no on first"));
        }
        let ran = matches!(f, First::Ran(_));
        if is_timer(k) && ran != p.timer.contains_key(&k) {
            kbad(
                k,
                format!(
                    "timer {k}: invoked {ran}, timer event {}",
                    p.timer.contains_key(&k)
                ),
            );
        }
    }
    for &k in p.timer.keys() {
        if !p.first.contains_key(&k) {
            kbad(k, format!("timer {k}: a timer event with no invocation"));
        }
    }
    for (&k, &slot) in timer_slots {
        let installed = p.timer.get(&k).is_some_and(|t| t.3.is_some());
        if p.sampled.contains_key(&slot) && !installed {
            kbad(k, format!("timer {k}: sampled with no installed start"));
        }
    }
    // The reference's.
    let mut r = Refs::default();
    let mut ran1: BTreeSet<Join> = BTreeSet::new();
    for pt in &rf.points {
        match *pt {
            Point::Ran { key, call: 1, .. } | Point::NotReached { key, .. } => {
                if r.first.insert(join(&key), *pt).is_some() {
                    bad.push((
                        None,
                        Some(key),
                        "the source's occurrence twice in one frame".into(),
                    ));
                }
                if matches!(pt, Point::Ran { .. }) {
                    ran1.insert(join(&key));
                }
            }
            Point::Ran { key, outcome, .. } => {
                if key.field != "timer" || !ran1.contains(&join(&key)) {
                    bad.push((None, Some(key), "a second call with no first".into()));
                } else if r.second.insert(join(&key), outcome).is_some() {
                    bad.push((None, Some(key), "the source's second call twice".into()));
                }
            }
        }
    }
    for t in &rf.timers {
        if !ran1.contains(&join(&t.key)) || r.timers.insert(join(&t.key), *t).is_some() {
            bad.push((
                None,
                Some(t.key),
                "a timer summary not once per invoked timer".into(),
            ));
        }
    }
    for j in &ran1 {
        if j.1 == "timer" && !r.timers.contains_key(j) {
            let key = r.first.get(j).map(|p| match p {
                Point::Ran { key, .. } | Point::NotReached { key, .. } => *key,
            });
            bad.push((None, key, "an invoked timer with no summary".into()));
        }
    }
    (p, r, bad)
}

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
                path: "",
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
    let unknown = Key {
        dest: usize::MAX,
        field: "?",
        index: 0,
        callable: u32::MAX,
        path: "",
    };
    let key_k = |k: u16| key_of.get(&k).copied().unwrap_or(unknown);
    let is_timer = |k: u16| key_of.get(&k).is_some_and(|key| key.field == "timer");
    let mut findings: Vec<Finding> = Vec::new();
    let mut find = |key: Key, frame: usize, what: String| {
        findings.push(Finding {
            key,
            site: site(&key),
            frame,
            what,
        });
    };
    if run.frames.len() != obs.frames.len() {
        find(
            unknown,
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
    let mut stopped: BTreeMap<u32, (&'static str, usize, Key)> = BTreeMap::new();
    let callbacks = u16::try_from(run.callbacks).unwrap_or(u16::MAX);
    for (i, (pf, rf)) in run.frames.iter().zip(&obs.frames).enumerate() {
        let (p, r, bad) = shapes(&pf.events, rf, &is_timer, &run.timer_slots);
        for (k, key, what) in bad {
            find(key.unwrap_or_else(|| k.map_or(unknown, key_k)), i, what);
        }
        rep.production_runs += p.order.len();
        let mut used: BTreeSet<(u16, u8)> = BTreeSet::new();
        let mut seen: BTreeSet<u16> = BTreeSet::new();
        let mut order: Vec<(u16, u8)> = Vec::new();
        let mut outs: BTreeMap<u16, (i32, i32)> = BTreeMap::new();
        // Per object, in schedule order: whether the last compiled
        // consumer before a point did not allow what follows.
        for range in &rf.objects {
            let mut blocked = false;
            for pt in rf.points.get(range.clone()).unwrap_or_default() {
                rep.points += 1;
                match *pt {
                    Point::Ran { key, call, outcome } => {
                        let k = cx.compiled(&key);
                        let api = match outcome {
                            Outcome::Unanswered(a) => Some(a),
                            Outcome::Budget => Some("budget"),
                            _ => None,
                        };
                        if let Some(api) = api
                            && k.is_none()
                        {
                            stopped.entry(key.callable).or_insert((api, 0, key)).1 += 1;
                        }
                        let Some(k) = k else {
                            match cx.attribute(&key) {
                                Some(b) => *rep.buckets.entry(b).or_default() += 1,
                                None => {
                                    find(key, i, "no production record of this occurrence".into());
                                }
                            }
                            continue;
                        };
                        seen.insert(k);
                        order.push((k, call));
                        rep.matched += 1;
                        if call == 1 && key.field != "timer" {
                            blocked = !allows(key.field, outcome);
                            outs.insert(k, expected_out(key.field, outcome));
                        }
                        if let Some(api) = api {
                            find(key, i, format!("reference could not complete ({api})"));
                            used.insert((k, call));
                            continue;
                        }
                        let prod = if call == 1 {
                            match p.first.get(&k) {
                                Some(First::Ran(inv)) => Some(*inv),
                                Some(First::Skipped(s)) => {
                                    find(
                                        key,
                                        i,
                                        format!("ran in the source, skipped in production ({s:?})"),
                                    );
                                    continue;
                                }
                                None => None,
                            }
                        } else {
                            p.second.get(&k).copied()
                        };
                        let Some(prod) = prod else {
                            find(
                                key,
                                i,
                                format!("ran in the source, call {call} not traced in production"),
                            );
                            continue;
                        };
                        used.insert((k, call));
                        match differ(key.field, prod, outcome) {
                            None => rep.agreed += 1,
                            Some(d) => {
                                let hint = luaj_hint(obs, key.callable);
                                find(key, i, format!("{d}{hint}"));
                            }
                        }
                    }
                    Point::NotReached { key, why } => {
                        let Some(k) = cx.compiled(&key) else {
                            continue;
                        };
                        let was_blocked = blocked;
                        if key.field != "timer" {
                            blocked = true;
                        }
                        seen.insert(k);
                        rep.matched += 1;
                        outs.insert(k, (0, 0));
                        match (p.first.get(&k), allowed(why, was_blocked)) {
                            (Some(First::Ran(_)), _) => {
                                used.insert((k, 1));
                                used.insert((k, 2));
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
                            (Some(First::Skipped(s)), Some(ok)) if ok.contains(s) => {
                                rep.agreed += 1
                            }
                            (Some(First::Skipped(s)), Some(ok)) => find(
                                key,
                                i,
                                format!(
                                    "skip cause: production {s:?}, source {why:?} (expects {ok:?})"
                                ),
                            ),
                        }
                    }
                }
            }
        }
        // Every compiled callback has a source point this frame, or its
        // object is listed (one finding above).
        for k in 0..callbacks {
            if !seen.contains(&k) && !listed_k.contains(&k) {
                find(
                    key_k(k),
                    i,
                    format!("compiled callback {k} with no source point"),
                );
            }
        }
        // The full order of invocations: both calls.
        let prod_order: Vec<(u16, u8)> = p
            .order
            .iter()
            .copied()
            .filter(|(k, _)| seen.contains(k))
            .collect();
        if prod_order != order {
            let key = order.first().map_or(unknown, |&(k, _)| key_k(k));
            find(
                key,
                i,
                format!("order: production {prod_order:?}, source {order:?}"),
            );
        }
        // Production invocations with no source point.
        for &(k, call) in &p.order {
            if !used.contains(&(k, call)) && !listed_k.contains(&k) {
                find(
                    key_k(k),
                    i,
                    format!("production invocation {k} call {call} with no source point"),
                );
            }
        }
        // Timers: start, elapsed, installed start, sampled age.
        for t in r.timers.values() {
            let Some(k) = cx.compiled(&t.key) else {
                continue;
            };
            let elapsed = t
                .start
                .map(|s| (pf.state_us / 1_000).saturating_sub(s / 1_000));
            let installed_ok = t.on && elapsed.is_some_and(|e| e >= 0);
            let cap = i64::from(MAX_ELAPSED_MS);
            let installed = elapsed.filter(|_| installed_ok).map(|e| {
                bms_model::TimeUs(pf.now.0.saturating_sub(e.min(cap).saturating_mul(1_000)))
            });
            outs.insert(k, (i32::from(t.on), 1));
            let want = (
                t.on,
                t.start.filter(|_| t.on),
                elapsed.filter(|_| t.on),
                installed,
            );
            match p.timer.get(&k) {
                None => find(t.key, i, "timer not traced in production".into()),
                Some(&got) if got != want => {
                    find(
                        t.key,
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
                (Some(_), None) => {
                    find(
                        t.key,
                        i,
                        "the selected source's fact is unknown: sampling undecided".into(),
                    );
                }
                (Some(e), Some(true)) => {
                    let age = e.min(cap).saturating_mul(1_000);
                    if ages.is_empty() {
                        find(t.key, i, "no sampled age where the source draws".into());
                    } else if ages.iter().any(|&a| a != age) {
                        find(t.key, i, format!("sampled ages {ages:?}, source {age}"));
                    }
                }
                _ if !ages.is_empty() => {
                    find(
                        t.key,
                        i,
                        format!("sampled {ages:?} where the source draws nothing"),
                    );
                }
                _ => {}
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
    }
    // Stopped roots and their isolation entries.
    for (callable, (api, count, key)) in stopped {
        let s = obs.sites.get(&callable).cloned().unwrap_or_default();
        let disposition = cx.attribute(&key).unwrap_or_else(|| "compiled".into());
        let covered = cover(&s, iso, obs, &cx);
        rep.stopped.push(Stopped {
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
    use crate::tests::{TAMPERED, pieces};

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
                && let Some(Event::Ran {
                    k, result, conv, ..
                }) = f
                    .events
                    .iter()
                    .find(|e| matches!(e, Event::Ran { .. }))
                    .copied()
            {
                f.events.push(Event::Ran {
                    k,
                    call: 2,
                    result,
                    conv,
                });
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
    /// a deleted or changed skip, reordered second calls, a changed output
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
        found("second calls reordered", &|run, _| {
            for f in &mut run.frames {
                let at: Vec<usize> = f
                    .events
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| matches!(e, Event::Ran { call: 2, .. }))
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
                        elapsed_ms: None,
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
        use super::shapes;
        let slots: std::collections::BTreeMap<u16, usize> = [(0, 7)].into();
        let is_timer = |k: u16| k == 0;
        let rf = crate::observed::RefFrame::default();
        let orphan = [
            Event::Skipped {
                k: 0,
                why: Skip::Gate,
            },
            Event::Sampled { slot: 7, age_us: 0 },
        ];
        let (_, _, bad) = shapes(&orphan, &rf, &is_timer, &slots);
        assert!(
            bad.iter().any(|b| b.2.contains("no installed start")),
            "{bad:?}"
        );
        let fine = [
            Event::Ran {
                k: 0,
                call: 1,
                result: Ok(skin_model::present::Val::Int(5)),
                conv: Converted::Long(Some(5)),
            },
            Event::Ran {
                k: 0,
                call: 2,
                result: Ok(skin_model::present::Val::Int(5)),
                conv: Converted::Long(Some(5)),
            },
            Event::Timer {
                k: 0,
                on: true,
                start: Some(5),
                elapsed_ms: Some(0),
                installed: Some(bms_model::TimeUs(0)),
            },
            Event::Sampled { slot: 7, age_us: 0 },
        ];
        let (_, _, bad) = shapes(&fine, &rf, &is_timer, &slots);
        assert!(bad.is_empty(), "{bad:?}");
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
}
