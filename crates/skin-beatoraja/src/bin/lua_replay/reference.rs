//! The reference: the configured pass's own VM, its skin table read as
//! the source's loader reads it, each destination installed as the source
//! installs it, and each installed object of a modelled kind prepared
//! every frame by its own prepare path, its host answering from the
//! recorded frame inputs (design.md "beatoraja Lua Skins", the reference
//! executor; s7-06e-design.md §3.2-§3.3).
//!
//! Modelled kinds: system images, plain and grouped images, image sets
//! and numbers. Every other kind is listed with the callables it holds.

use std::collections::BTreeMap;

use mlua::{Function, Table, Value};
use skin_beatoraja::lua::replay::{Called, ReferenceVm};
use skin_beatoraja::lua::runtime::{to_bool, to_int};

use crate::inputs::FrameInputs;
use crate::observed::{Cause, Key, Listed, Observed, Outcome, Point, Raw, RefFrame, TimerOut};

/// How an image or a number chooses what it shows.
enum Sel {
    /// Always 0: a system or plain image.
    Zero,
    /// A function's value.
    Callable(Function, Key),
    /// An image-index property (a group's or an image set's `ref`).
    Index(i32),
    /// An integer property (a number's or an image set's numeric `value`,
    /// or a number's `ref`).
    Int(i32),
    /// A named property the reference does not resolve.
    Named,
}

enum Kind {
    Image,
    Number,
}

enum Cond {
    /// A numeric boolean property.
    Prop(i32),
    /// A named one.
    Named(String),
    /// A function.
    Func(Function, Key),
}

struct Obj {
    dest: usize,
    kind: Kind,
    sel: Sel,
    conds: Vec<Cond>,
    timer: Option<(Function, Key)>,
}

/// A destination as the source reads it.
struct Dest {
    index: usize,
    table: Table,
}

fn raw(t: &Table, k: &str) -> Value {
    t.raw_get::<Value>(k).unwrap_or(Value::Nil)
}

fn list(v: &Value) -> Vec<Value> {
    let Value::Table(t) = v else {
        return Vec::new();
    };
    (1..=t.raw_len())
        .map(|i| t.raw_get::<Value>(i).unwrap_or(Value::Nil))
        .collect()
}

/// A value as the JSON reader's string `id` reads it.
fn id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => s.to_str().ok().map(|s| s.to_owned()),
        Value::Integer(n) => Some(n.to_string()),
        Value::Number(f) if f.fract() == 0.0 => Some(format!("{f:.0}")),
        _ => None,
    }
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(n) => Some(*n),
        #[expect(clippy::cast_possible_truncation, reason = "integral, checked")]
        Value::Number(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Some(*f as i64),
        _ => None,
    }
}

fn int32(v: &Value) -> Option<i32> {
    int(v).and_then(|n| i32::try_from(n).ok())
}

/// The entry of list `name` whose `id` is `id`.
fn find(skin: &Table, name: &str, id: &str) -> Option<Table> {
    list(&raw(skin, name)).into_iter().find_map(|e| match e {
        Value::Table(t) if id_string(&raw(&t, "id")).as_deref() == Some(id) => Some(t),
        _ => None,
    })
}

/// Whether a `source` entry has id `id`.
fn has_source(skin: &Table, src: &Value) -> bool {
    id_string(src).is_some_and(|s| find(skin, "source", &s).is_some())
}

/// The top-level key of another kind holding an object with id `id`: a
/// list of tables with `id`s, or one table with an `id`.
fn other_kind(skin: &Table, id: &str) -> Option<String> {
    let mut found = None;
    let _ = skin.for_each(|k: Value, v: Value| {
        let Some(name) = id_string(&k) else {
            return Ok(());
        };
        if [
            "image",
            "imageset",
            "value",
            "destination",
            "source",
            "font",
        ]
        .contains(&name.as_str())
        {
            return Ok(());
        }
        let hit = match &v {
            Value::Table(t) if id_string(&raw(t, "id")).as_deref() == Some(id) => true,
            Value::Table(_) => list(&v).iter().any(
                |e| matches!(e, Value::Table(t) if id_string(&raw(t, "id")).as_deref() == Some(id)),
            ),
            _ => false,
        };
        if hit && found.is_none() {
            found = Some(name);
        }
        Ok(())
    });
    found
}

pub struct Reference<'a> {
    vm: &'a ReferenceVm,
    items: &'a BTreeMap<i32, bool>,
    objects: Vec<Obj>,
    observed: Observed,
}

impl<'a> Reference<'a> {
    /// Reads `vm`'s skin and resolves its destinations; installation waits
    /// for the first frame ([`Self::run`]).
    pub fn new(vm: &'a ReferenceVm, items: &'a BTreeMap<i32, bool>) -> Self {
        let mut r = Self {
            vm,
            items,
            objects: Vec::new(),
            observed: Observed::default(),
        };
        if let Value::Table(skin) = vm.skin().clone() {
            r.resolve(&skin);
        }
        r
    }

    fn key(&mut self, f: &Function, dest: usize, field: &'static str) -> Key {
        let callable = self.vm.callable(f).unwrap_or(u32::MAX);
        if let std::collections::btree_map::Entry::Vacant(e) = self.observed.sites.entry(callable) {
            let info = f.info();
            e.insert((
                info.source.unwrap_or_default().to_string(),
                u32::try_from(info.line_defined.unwrap_or(0)).unwrap_or(0),
            ));
            self.observed.ends.insert(
                callable,
                u32::try_from(info.last_line_defined.unwrap_or(0)).unwrap_or(0),
            );
        }
        Key {
            dest,
            field,
            callable,
        }
    }

    /// Every callable a destination holds, keyed.
    fn dest_callables(&mut self, d: &Dest) -> Vec<Key> {
        let mut out = Vec::new();
        for v in list(&raw(&d.table, "op")) {
            if let Value::Function(f) = v {
                out.push(self.key(&f, d.index, "op"));
            }
        }
        for field in ["draw", "timer"] {
            if let Value::Function(f) = raw(&d.table, field) {
                out.push(self.key(&f, d.index, field));
            }
        }
        out
    }

    fn resolve(&mut self, skin: &Table) {
        // Custom timers and events holding a function (not run in e-1).
        for name in ["customTimers", "customEvents"] {
            self.observed.custom += list(&raw(skin, name))
                .iter()
                .filter(|e| match e {
                    Value::Table(t) => t
                        .pairs::<Value, Value>()
                        .flatten()
                        .any(|(_, v)| matches!(v, Value::Function(_))),
                    _ => false,
                })
                .count();
        }
        for (index, d) in list(&raw(skin, "destination")).into_iter().enumerate() {
            let Value::Table(table) = d else {
                continue;
            };
            let d = Dest { index, table };
            self.resolve_one(skin, &d);
        }
    }

    fn resolve_one(&mut self, skin: &Table, d: &Dest) {
        let Some(id) = id_string(&raw(&d.table, "id")) else {
            let callables = self.dest_callables(d);
            self.observed.unresolved.push(Listed {
                dest: d.index,
                what: "no id".into(),
                callables,
            });
            return;
        };
        // `JSONSkinLoader.java:316-337`: a negative integer is a system
        // image; else the object loader's lists, image first.
        let system = id.parse::<i32>().is_ok_and(|n| n < 0);
        let (kind, sel, valid) = if system {
            (Kind::Image, Sel::Zero, true)
        } else if let Some(img) = find(skin, "image", &id) {
            let len = int(&raw(&img, "len")).unwrap_or(0);
            let valid = has_source(skin, &raw(&img, "src"));
            let sel = if len > 1 {
                Sel::Index(int32(&raw(&img, "ref")).unwrap_or(0))
            } else {
                Sel::Zero
            };
            (Kind::Image, sel, valid)
        } else if let Some(set) = find(skin, "imageset", &id) {
            let valid = list(&raw(&set, "images")).iter().any(|i| {
                id_string(i)
                    .and_then(|i| find(skin, "image", &i))
                    .is_some_and(|img| has_source(skin, &raw(&img, "src")))
            });
            let sel = match raw(&set, "value") {
                Value::Function(f) => Sel::Callable(f.clone(), self.key(&f, d.index, "value")),
                v @ (Value::Integer(_) | Value::Number(_)) => Sel::Int(int32(&v).unwrap_or(0)),
                Value::String(_) => Sel::Named,
                _ => Sel::Index(int32(&raw(&set, "ref")).unwrap_or(0)),
            };
            (Kind::Image, sel, valid)
        } else if let Some(num) = find(skin, "value", &id) {
            let sel = match raw(&num, "value") {
                Value::Function(f) => Sel::Callable(f.clone(), self.key(&f, d.index, "value")),
                v @ (Value::Integer(_) | Value::Number(_)) => Sel::Int(int32(&v).unwrap_or(0)),
                Value::String(_) => Sel::Named,
                _ => Sel::Int(int32(&raw(&num, "ref")).unwrap_or(0)),
            };
            (Kind::Number, sel, true)
        } else {
            let callables = self.dest_callables(d);
            match other_kind(skin, &id) {
                Some(what) => self.observed.unmodelled.push(Listed {
                    dest: d.index,
                    what,
                    callables,
                }),
                None => self.observed.unresolved.push(Listed {
                    dest: d.index,
                    what: "unresolved id".into(),
                    callables,
                }),
            }
            return;
        };
        // A keyframe is needed (`SkinObject.java:577-579`).
        let valid = valid && !list(&raw(&d.table, "dst")).is_empty();
        // Conditions: numeric properties, then names and functions, then
        // `draw` (`JSONSkinLoader.java:458-462`, `SkinObject.java:282-315`).
        let mut ids = Vec::new();
        let mut rest = Vec::new();
        let mut options = Vec::new();
        for v in list(&raw(&d.table, "op")) {
            match &v {
                Value::Function(f) => {
                    let k = self.key(f, d.index, "op");
                    rest.push(Cond::Func(f.clone(), k));
                }
                Value::String(s) => {
                    rest.push(Cond::Named(
                        s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
                    ));
                }
                _ => match int32(&v) {
                    Some(0) | None => {}
                    Some(n) if ReferenceVm::is_property(n) => {
                        if !ids.contains(&n) {
                            ids.push(n);
                        }
                    }
                    Some(n) => options.push(n),
                },
            }
        }
        let mut conds: Vec<Cond> = ids.into_iter().map(Cond::Prop).collect();
        conds.extend(rest);
        if let Value::Function(f) = raw(&d.table, "draw") {
            let k = self.key(&f, d.index, "draw");
            conds.push(Cond::Func(f, k));
        }
        let timer = match raw(&d.table, "timer") {
            Value::Function(f) => {
                let k = self.key(&f, d.index, "timer");
                Some((f, k))
            }
            _ => None,
        };
        let obj = Obj {
            dest: d.index,
            kind,
            sel,
            conds,
            timer,
        };
        if !valid {
            let callables = obj_keys(&obj);
            self.observed.removed.push(Listed {
                dest: d.index,
                what: "invalid".into(),
                callables,
            });
            return;
        }
        // Options (`Skin.java:214-226`): an absent one reads -1.
        let fails = options.iter().any(|&op| {
            let want = op > 0;
            self.items.get(&op.abs()).copied() != Some(want)
        });
        if fails {
            let callables = obj_keys(&obj);
            self.observed.removed.push(Listed {
                dest: d.index,
                what: "option".into(),
                callables,
            });
            return;
        }
        self.objects.push(obj);
    }

    /// Prunes static conditions on the first frame (`Skin.java:199-213`):
    /// a false one removes the object, a true one is dropped.
    fn install(&mut self) {
        let objects = std::mem::take(&mut self.objects);
        for mut obj in objects {
            let mut removed = None;
            let mut kept = Vec::new();
            for c in obj.conds.drain(..) {
                match c {
                    Cond::Prop(n) if ReferenceVm::is_static(n) => {
                        match self.vm.condition(n, self.items) {
                            Some(true) => {}
                            Some(false) => removed = removed.or(Some("static condition")),
                            None => removed = removed.or(Some("static condition unknown")),
                        }
                    }
                    other => kept.push(other),
                }
            }
            obj.conds = kept;
            match removed {
                None => self.objects.push(obj),
                Some(why) => {
                    let callables = obj_keys(&obj);
                    let l = Listed {
                        dest: obj.dest,
                        what: why.into(),
                        callables,
                    };
                    if why == "static condition" {
                        self.observed.removed.push(l);
                    } else {
                        self.observed.unresolved.push(l);
                    }
                }
            }
        }
    }

    /// Runs every frame of `frames`.
    pub fn run(mut self, frames: &[FrameInputs]) -> Observed {
        for (i, f) in frames.iter().enumerate() {
            self.vm.set_frame(f.fields(), f.timers(), f.state_us());
            if i == 0 {
                self.install();
            }
            let mut out = RefFrame::default();
            for o in &self.objects {
                prepare(self.vm, self.items, o, &mut out);
            }
            self.observed.frames.push(out);
        }
        self.observed
    }
}

fn obj_keys(o: &Obj) -> Vec<Key> {
    let mut out = Vec::new();
    if let Sel::Callable(_, k) = &o.sel {
        out.push(*k);
    }
    for c in &o.conds {
        if let Cond::Func(_, k) = c {
            out.push(*k);
        }
    }
    if let Some((_, k)) = &o.timer {
        out.push(*k);
    }
    out
}

fn outcome(c: &Called) -> Outcome {
    match c {
        Called::Value(v) => Outcome::Value(raw_of(v)),
        Called::Error => Outcome::Error,
        Called::Unavailable => Outcome::Unavailable,
        Called::Unanswered(api) => Outcome::Unanswered(api),
        Called::Budget => Outcome::Budget,
    }
}

fn raw_of(v: &Value) -> Raw {
    match v {
        Value::Nil => Raw::Nil,
        Value::Boolean(b) => Raw::Bool(*b),
        Value::Integer(n) => Raw::Int(*n),
        Value::Number(f) => Raw::Float(*f),
        Value::String(_) => Raw::Other("string"),
        Value::Table(_) => Raw::Other("table"),
        Value::Function(_) => Raw::Other("function"),
        _ => Raw::Other("other"),
    }
}

/// LuaJ's `tolong()` of a returned value (`SkinLuaAccessor.java:736-747`):
/// an integer as it is, a double saturated (NaN 0), anything else 0.
pub fn to_long(v: &Value) -> i64 {
    match v {
        Value::Integer(n) => *n,
        #[expect(clippy::cast_possible_truncation, reason = "Java's saturating d2l")]
        Value::Number(f) => *f as i64,
        _ => 0,
    }
}

/// Every remaining occurrence of `o` from condition `from` on, and its
/// timer, not reached for `why`.
fn not_reached(o: &Obj, from: usize, why: Cause, out: &mut RefFrame) {
    for c in o.conds.iter().skip(from) {
        if let Cond::Func(_, k) = c {
            out.points.push(Point::NotReached { key: *k, why });
        }
    }
    if let Some((_, k)) = &o.timer {
        out.points.push(Point::NotReached { key: *k, why });
    }
}

/// `o`'s prepare path on the frame the VM holds.
fn prepare(vm: &ReferenceVm, items: &BTreeMap<i32, bool>, o: &Obj, out: &mut RefFrame) {
    // 1. The selector or value.
    let sel: Result<i32, Cause> = match &o.sel {
        Sel::Zero => Ok(0),
        Sel::Callable(f, k) => {
            let c = vm.call(f);
            out.points.push(Point::Ran {
                key: *k,
                call: 1,
                outcome: outcome(&c),
            });
            match &c {
                Called::Value(v) => Ok(to_int(v)),
                Called::Error => Ok(0),
                Called::Unavailable => Err(Cause::Unavailable),
                Called::Unanswered(_) | Called::Budget => Err(Cause::Inconclusive),
            }
        }
        Sel::Index(id) => vm.index_property(*id).ok_or(Cause::Inconclusive),
        Sel::Int(id) => match vm.number_property(*id) {
            Some(Some(n)) => Ok(n),
            Some(None) => Err(Cause::Unavailable),
            None => Err(Cause::Inconclusive),
        },
        Sel::Named => Err(Cause::Inconclusive),
    };
    let sel = match sel {
        Ok(v) => v,
        Err(why) => return not_reached(o, 0, why, out),
    };
    // 2. The kind's own gate before the conditions.
    match o.kind {
        Kind::Image if sel < 0 => return not_reached(o, 0, Cause::Selector, out),
        Kind::Number if sel == i32::MIN || sel == i32::MAX => {
            return not_reached(o, 0, Cause::Sentinel, out);
        }
        _ => {}
    }
    // 3. The conditions, the first false stopping.
    for (i, c) in o.conds.iter().enumerate() {
        let holds = match c {
            Cond::Prop(n) => match vm.condition(*n, items) {
                Some(b) => Ok(b),
                None => Err(Cause::Inconclusive),
            },
            Cond::Named(name) => vm.named_condition(name).ok_or(Cause::Inconclusive),
            Cond::Func(f, k) => {
                let r = vm.call(f);
                out.points.push(Point::Ran {
                    key: *k,
                    call: 1,
                    outcome: outcome(&r),
                });
                match &r {
                    Called::Value(v) => Ok(to_bool(v)),
                    Called::Error => Ok(false),
                    Called::Unavailable => Err(Cause::Unavailable),
                    Called::Unanswered(_) | Called::Budget => Err(Cause::Inconclusive),
                }
            }
        };
        match holds {
            Ok(true) => {}
            Ok(false) => return not_reached(o, i + 1, Cause::Condition, out),
            Err(why) => return not_reached(o, i + 1, why, out),
        }
    }
    // 4. The timer: once, and again unless off (`TimerProperty.java:8-22`).
    if let Some((f, k)) = &o.timer {
        let first = vm.call(f);
        out.points.push(Point::Ran {
            key: *k,
            call: 1,
            outcome: outcome(&first),
        });
        let first = match &first {
            Called::Value(v) => Some(to_long(v)),
            Called::Error => Some(i64::MIN),
            _ => None,
        };
        let on = first.is_some_and(|v| v != i64::MIN);
        let start = if on {
            let second = vm.call(f);
            out.points.push(Point::Ran {
                key: *k,
                call: 2,
                outcome: outcome(&second),
            });
            match &second {
                Called::Value(v) => Some(to_long(v)),
                Called::Error => Some(i64::MIN),
                _ => None,
            }
        } else {
            None
        };
        out.timers.push(TimerOut { key: *k, on, start });
    }
}
