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
use crate::observed::{Key, Observed, Outcome, Point, Raw};
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

/// Production's records, by occurrence.
struct Cx<'a> {
    by_key: BTreeMap<(usize, &'static str, u32), &'a Occurrence>,
    run: &'a ProductionRun,
}

impl<'a> Cx<'a> {
    fn rec(&self, k: &Key) -> Option<&'a Occurrence> {
        self.by_key.get(&(k.dest, k.field, k.callable)).copied()
    }

    /// Why production did not run an occurrence the source ran: its
    /// record, its destination's, or its callable's; `None` when it is
    /// compiled or nothing explains it.
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
                _ => self
                    .run
                    .recorded
                    .unused
                    .get(&key.callable)
                    .map(|u| format!("unused {u}")),
            },
        }
    }
}

/// A timer callback's trace: on, its start, its elapsed ms and the start
/// installed.
type TimerTrace = (bool, Option<i64>, Option<i64>, Option<bms_model::TimeUs>);

/// One frame's production events, by callback.
#[derive(Default)]
struct ByK {
    ran: BTreeMap<(u16, u8), (Result<Val, Fail>, Converted)>,
    order: Vec<u16>,
    timer: BTreeMap<u16, TimerTrace>,
    skipped: BTreeMap<u16, Skip>,
    sampled: BTreeMap<usize, Vec<i64>>,
}

fn by_k(events: &[Event]) -> ByK {
    let mut b = ByK::default();
    for e in events {
        match *e {
            Event::Ran {
                k,
                call,
                result,
                conv,
            } => {
                if call == 1 {
                    b.order.push(k);
                }
                b.ran.insert((k, call), (result, conv));
            }
            Event::Timer {
                k,
                on,
                start,
                elapsed_ms,
                installed,
            } => {
                b.timer.insert(k, (on, start, elapsed_ms, installed));
            }
            Event::Skipped { k, why } => {
                b.skipped.insert(k, why);
            }
            Event::Sampled { slot, age_us } => b.sampled.entry(slot).or_default().push(age_us),
        }
    }
    b
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
    let mut by_key: BTreeMap<(usize, &'static str, u32), &Occurrence> = BTreeMap::new();
    for o in records.values() {
        if let Some(d) = o.destination {
            by_key.insert((d, o.field, o.callable), o);
        }
    }
    let cx = Cx { by_key, run };
    let rec = |k: &Key| cx.rec(k);
    let mut key_of: BTreeMap<u16, Key> = BTreeMap::new();
    for o in records.values() {
        if let (Some(k), Some(dest)) = (o.program, o.destination) {
            key_of.insert(
                k,
                Key {
                    dest,
                    field: o.field,
                    callable: o.callable,
                },
            );
        }
    }
    let site = |k: &Key| obs.sites.get(&k.callable).cloned().unwrap_or_default();
    for (k, key) in &key_of {
        rep.compiled.push((*k, *key, site(key)));
    }
    let attribute = |key: &Key| cx.attribute(key);
    let mut findings: Vec<Finding> = Vec::new();
    let mut find = |key: Key, frame: usize, what: String| {
        findings.push(Finding {
            key,
            site: site(&key),
            frame,
            what,
        });
    };
    let mut stopped: BTreeMap<u32, (&'static str, usize, Key)> = BTreeMap::new();
    for (i, (pf, rf)) in run.frames.iter().zip(&obs.frames).enumerate() {
        let p = by_k(&pf.events);
        rep.production_runs += p.ran.len();
        let mut used: BTreeSet<(u16, u8)> = BTreeSet::new();
        let mut order: Vec<u16> = Vec::new();
        for pt in &rf.points {
            rep.points += 1;
            match *pt {
                Point::Ran { key, call, outcome } => {
                    let k = rec(&key)
                        .and_then(|o| o.program)
                        .filter(|_| rec(&key).is_some_and(|o| o.disposition == "compiled"));
                    let api = match outcome {
                        Outcome::Unanswered(a) => Some(a),
                        Outcome::Budget => Some("budget"),
                        _ => None,
                    };
                    if let Some(api) = api {
                        if k.is_some() {
                            find(key, i, format!("reference could not complete ({api})"));
                            continue;
                        }
                        let e = stopped.entry(key.callable).or_insert((api, 0, key));
                        e.1 += 1;
                    }
                    let Some(k) = k else {
                        match attribute(&key) {
                            Some(b) => *rep.buckets.entry(b).or_default() += 1,
                            None => find(key, i, "no production record of this occurrence".into()),
                        }
                        continue;
                    };
                    if call == 1 {
                        order.push(k);
                    }
                    rep.matched += 1;
                    let Some(&prod) = p.ran.get(&(k, call)) else {
                        let why = p
                            .skipped
                            .get(&k)
                            .map_or("not traced".into(), |s| format!("{s:?}"));
                        find(
                            key,
                            i,
                            format!("ran in the source, skipped in production ({why})"),
                        );
                        continue;
                    };
                    used.insert((k, call));
                    match differ(key.field, prod, outcome) {
                        None => rep.agreed += 1,
                        Some(d) => find(key, i, d),
                    }
                }
                Point::NotReached { key, why } => {
                    let Some(k) = rec(&key)
                        .filter(|o| o.disposition == "compiled")
                        .and_then(|o| o.program)
                    else {
                        continue;
                    };
                    rep.matched += 1;
                    if p.ran.contains_key(&(k, 1)) {
                        used.insert((k, 1));
                        used.insert((k, 2));
                        find(
                            key,
                            i,
                            format!("invoked in production, not in the source ({why:?})"),
                        );
                    } else if p.skipped.get(&k) == Some(&Skip::SlotsOff) {
                        find(key, i, "slots off on a fresh attempt".into());
                    } else {
                        rep.agreed += 1;
                    }
                }
            }
        }
        // The order among program callbacks.
        let prod_order: Vec<u16> = p
            .order
            .iter()
            .copied()
            .filter(|k| order.contains(k))
            .collect();
        if prod_order != order
            && let Some(key) = order.first().and_then(|k| key_of.get(k))
        {
            find(
                *key,
                i,
                format!("order: production {prod_order:?}, source {order:?}"),
            );
        }
        // Production invocations with no source point.
        for &(k, call) in p.ran.keys() {
            if !used.contains(&(k, call)) {
                let key = key_of.get(&k).copied().unwrap_or(Key {
                    dest: usize::MAX,
                    field: "?",
                    callable: u32::MAX,
                });
                find(
                    key,
                    i,
                    format!("production invocation {k} call {call} with no source point"),
                );
            }
        }
        // Timers: start, elapsed, installed start, sampled age.
        for t in &rf.timers {
            let Some(k) = rec(&t.key)
                .filter(|o| o.disposition == "compiled")
                .and_then(|o| o.program)
            else {
                continue;
            };
            let elapsed = t
                .start
                .map(|s| (pf.state_us / 1_000).saturating_sub(s / 1_000));
            let drawn = t.on && elapsed.is_some_and(|e| e >= 0);
            let cap = i64::from(MAX_ELAPSED_MS);
            let installed = elapsed.filter(|_| drawn).map(|e| {
                bms_model::TimeUs(pf.now.0.saturating_sub(e.min(cap).saturating_mul(1_000)))
            });
            let want = (
                t.on,
                t.start.filter(|_| t.on),
                elapsed.filter(|_| t.on),
                installed,
            );
            match p.timer.get(&k) {
                None => find(t.key, i, "timer not traced in production".into()),
                Some(&got) if got != want => find(
                    t.key,
                    i,
                    format!("timer: production {got:?}, reference {want:?}"),
                ),
                Some(_) => {}
            }
            let slot = run.timer_slots.get(&k).copied();
            let ages = slot
                .and_then(|s| p.sampled.get(&s))
                .cloned()
                .unwrap_or_default();
            match (drawn, elapsed) {
                (true, Some(e)) => {
                    let age = e.min(cap).saturating_mul(1_000);
                    if ages.is_empty() {
                        find(t.key, i, "no sampled age where the source draws".into());
                    } else if ages.iter().any(|&a| a != age) {
                        find(t.key, i, format!("sampled ages {ages:?}, source {age}"));
                    }
                }
                _ => {
                    if !ages.is_empty() {
                        find(
                            t.key,
                            i,
                            format!("sampled {ages:?} where the source draws nothing"),
                        );
                    }
                }
            }
        }
    }
    // A compiled callable on an object the reference did not run.
    for (label, list) in [
        ("unmodelled", &obs.unmodelled),
        ("removed", &obs.removed),
        ("unresolved", &obs.unresolved),
    ] {
        for l in list {
            for key in &l.callables {
                if rec(key).is_some_and(|o| matches!(o.disposition, "compiled" | "lowered")) {
                    find(
                        *key,
                        0,
                        format!(
                            "{} on a {label} object ({})",
                            rec(key).map_or("", |o| o.disposition),
                            l.what
                        ),
                    );
                }
            }
        }
    }
    // Stopped roots and their isolation entries.
    for (callable, (api, count, key)) in stopped {
        let s = obs.sites.get(&callable).cloned().unwrap_or_default();
        let disposition = attribute(&key).unwrap_or_else(|| "compiled".into());
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
/// the root, and each reader it lists carries its evidence.
fn cover(s: &(String, u32), iso: &[Entry], obs: &Observed, cx: &Cx<'_>) -> Result<(), String> {
    let rec = |k: &Key| cx.rec(k);
    let attribute = |k: &Key| cx.attribute(k);
    let Some(e) = iso
        .iter()
        .find(|e| e.line == s.1 && same_file(&s.0, &e.file))
    else {
        return Err("no entry".into());
    };
    for r in &e.readers {
        let keys: Vec<Key> = obs
            .frames
            .iter()
            .flat_map(|f| f.points.iter())
            .map(|p| match p {
                Point::Ran { key, .. } | Point::NotReached { key, .. } => *key,
            })
            .filter(|k| {
                k.field == r.field
                    && obs
                        .sites
                        .get(&k.callable)
                        .is_some_and(|(f, l)| *l == r.line && same_file(f, &r.file))
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if keys.is_empty() {
            return Err(format!("reader {}:{} not scheduled", r.file, r.line));
        }
        for k in keys {
            if rec(&k).is_some_and(|o| matches!(o.disposition, "compiled" | "lowered")) {
                return Err(format!(
                    "reader {}:{} is {}",
                    r.file,
                    r.line,
                    rec(&k).map_or("", |o| o.disposition)
                ));
            }
            let got = attribute(&k).unwrap_or_default();
            let ok = match r.evidence.as_str() {
                "replaced" => got.starts_with("replaced"),
                "dropped" => got == "dropped" || got == "unused dropped",
                _ => false,
            };
            if !ok {
                return Err(format!("reader {}:{} carries {got:?}", r.file, r.line));
            }
        }
    }
    Ok(())
}
