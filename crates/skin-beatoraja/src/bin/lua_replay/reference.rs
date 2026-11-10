//! The reference: the configured pass's own VM, its skin table read as
//! the source's loader reads it, each destination installed as the source
//! installs it, and each installed object of a modelled kind prepared
//! every frame by its own prepare path, its host answering from the
//! recorded frame inputs (design.md "beatoraja Lua Skins", the reference
//! executor; s7-06e-design.md §3.2-§3.3; s7-06e-1-r1-tables.md, B).
//!
//! Modelled kinds: system images, plain and grouped images, image sets
//! and numbers. Every other kind is listed with every callable it holds,
//! its nested parts' included.

use std::collections::BTreeMap;

use mlua::{Function, Table, Value};
use skin_beatoraja::lua::replay::{Called, PropValue, ReferenceVm, SourceFact};
use skin_beatoraja::lua::runtime::{to_bool, to_int};

use crate::inputs::{FrameInputs, Host};
use crate::observed::{
    Cause, Key, Listed, Observed, Outcome, Point, Raw, RefFrame, TimerOut, intern,
};

/// How an image or a number chooses what it shows.
enum Sel {
    /// Always 0: a system or plain image.
    Zero,
    /// A function's value.
    Callable(Function, Key),
    /// An image-index property (a group's or an image set's `ref`).
    Index(i32),
    /// An integer property (a number's or an image set's resolved `value`,
    /// or a number's `ref`).
    Int(PropRef),
    /// A name the factory does not know: a script the source runs.
    Script,
}

/// An integer property, by id or by name.
enum PropRef {
    Id(i32),
    Name(String),
}

enum Kind {
    Image,
    Number,
}

enum Cond {
    /// A numeric boolean property.
    Prop(i32),
    /// A named one (`!` negating it).
    Named(String),
    /// A function.
    Func(Function, Key),
}

/// Whether the source's loader would build a valid object.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Validity {
    Valid,
    Invalid,
    Unknown,
    Movie,
}

struct Obj {
    dest: usize,
    kind: Kind,
    sel: Sel,
    /// An image set's entries, each its source's fact.
    entries: Vec<Validity>,
    conds: Vec<Cond>,
    timer: Option<(Function, Key)>,
    options: Vec<i32>,
    validity: Validity,
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

/// A value as the source serializer's `tojstring()` gives a string field
/// (`LuaSkinLoader.java:107`): an integral number without a fraction.
fn id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => s.to_str().ok().map(|s| s.to_owned()),
        Value::Integer(n) => Some(n.to_string()),
        Value::Number(f) if f.fract() == 0.0 => Some(format!("{f:.0}")),
        Value::Number(f) => Some(format!("{f}")),
        _ => None,
    }
}

/// LuaJ's `isnumber()` and its value: a number, or a string that reads as
/// one.
fn lua_number(v: &Value) -> Option<f64> {
    match v {
        #[expect(clippy::cast_precision_loss, reason = "LuaJ holds a double")]
        Value::Integer(n) => Some(*n as f64),
        Value::Number(f) => Some(*f),
        Value::String(s) => s.to_str().ok()?.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Java's `(int)` of a double: truncated toward zero, saturated, NaN 0.
fn d2i(f: f64) -> i32 {
    if f.is_nan() {
        0
    } else {
        #[expect(clippy::cast_possible_truncation, reason = "Java's saturating d2i")]
        let n = f.trunc().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
        n
    }
}

/// The source serializer's `toint()` of an `int` field (`LuaSkinLoader.
/// java:103-104`, `:144-146`): a number or a numeric string truncated as
/// Java's `(int)`, anything else 0; `None` for an absent field, which the
/// definition's default then gives.
fn toint(v: &Value) -> Option<i32> {
    match v {
        Value::Nil => None,
        Value::Integer(n) => Some(i32::try_from(*n).unwrap_or_else(|_| {
            #[expect(clippy::cast_precision_loss, reason = "LuaJ holds a double")]
            let f = *n as f64;
            d2i(f)
        })),
        other => Some(lua_number(other).map_or(0, d2i)),
    }
}

fn string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => s.to_str().ok().map(|s| s.to_owned()),
        _ => None,
    }
}

/// The entry of list `name` whose `id` is `id`.
fn find(skin: &Table, name: &str, id: &str) -> Option<Table> {
    list(&raw(skin, name)).into_iter().find_map(|e| match e {
        Value::Table(t) if id_string(&raw(&t, "id")).as_deref() == Some(id) => Some(t),
        _ => None,
    })
}

/// The top-level key of another kind holding an object with id `id`, and
/// that object: a list of tables with `id`s, or one table with an `id`.
fn other_kind(skin: &Table, id: &str) -> Option<(String, Table)> {
    let mut found = None;
    let _ = skin.for_each(|k: Value, v: Value| {
        let Some(name) = id_string(&k) else {
            return Ok(());
        };
        if found.is_some()
            || [
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
            Value::Table(t) if id_string(&raw(t, "id")).as_deref() == Some(id) => Some(t.clone()),
            Value::Table(_) => list(&v).into_iter().find_map(|e| match e {
                Value::Table(t) if id_string(&raw(&t, "id")).as_deref() == Some(id) => Some(t),
                _ => None,
            }),
            _ => None,
        };
        if let Some(t) = hit {
            found = Some((name, t));
        }
        Ok(())
    });
    found
}

pub struct Reference<'a> {
    vm: &'a ReferenceVm,
    items: &'a BTreeMap<i32, bool>,
    sources: &'a BTreeMap<String, SourceFact>,
    objects: Vec<Obj>,
    observed: Observed,
    /// A test's override of one page's point.
    page_point: Option<(usize, i32)>,
}

impl<'a> Reference<'a> {
    /// Reads `vm`'s skin and resolves its destinations against the option
    /// items `items` and the source facts `sources`; installation waits
    /// for the first frame ([`Self::run`]).
    pub fn new(
        vm: &'a ReferenceVm,
        items: &'a BTreeMap<i32, bool>,
        sources: &'a BTreeMap<String, SourceFact>,
    ) -> Self {
        let mut r = Self {
            vm,
            items,
            sources,
            objects: Vec::new(),
            observed: Observed::default(),
            page_point: None,
        };
        if let Value::Table(skin) = vm.skin().clone() {
            r.resolve(&skin);
        }
        r
    }

    fn key(&mut self, f: &Function, dest: usize, field: &'static str, index: u32) -> Key {
        self.key_at(f, dest, (field, index), "")
    }

    /// [`Self::key`] of a listed object's part at object path `path`.
    fn key_at(
        &mut self,
        f: &Function,
        dest: usize,
        (field, index): (&'static str, u32),
        path: &'static str,
    ) -> Key {
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
            index,
            callable,
            path,
        }
    }

    /// Every function under `t` (to a few levels, each table once), keyed
    /// by the consumer that holds it: `value`, `draw`, `timer`, an `op`
    /// entry with its index, or `other`, at the object path `path` of
    /// table keys: the callables of an object the reference does not run,
    /// its nested parts' included.
    fn nested(
        &mut self,
        t: &Table,
        dest: usize,
        (path, parent): (&str, &str),
        out: &mut Vec<Key>,
        stack: &mut Vec<usize>,
    ) {
        // A table on the way down is a cycle; one met again elsewhere is
        // another occurrence, with its own path.
        let at = t.to_pointer().addr();
        if path.matches('.').count() > 6 || stack.contains(&at) {
            return;
        }
        stack.push(at);
        let mut found: Vec<(String, Value)> = Vec::new();
        let _ = t.for_each(|k: Value, v: Value| {
            found.push((id_string(&k).unwrap_or_default(), v));
            Ok(())
        });
        found.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, v) in found {
            match v {
                Value::Function(f) => {
                    let consumer = if parent == "op" {
                        ("op", name.parse::<u32>().map_or(0, |i| i.saturating_sub(1)))
                    } else {
                        (
                            match name.as_str() {
                                "value" => "value",
                                "draw" => "draw",
                                "timer" => "timer",
                                _ => "other",
                            },
                            0,
                        )
                    };
                    let k = self.key_at(&f, dest, consumer, intern(path));
                    if !out.contains(&k) {
                        out.push(k);
                    }
                }
                Value::Table(t) => {
                    // An `op` list is a consumer of the object holding
                    // it, its entries at that object's path.
                    let sub = if name == "op" {
                        path.to_owned()
                    } else if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{path}.{name}")
                    };
                    self.nested(&t, dest, (&sub, &name), out, stack);
                }
                _ => {}
            }
        }
        stack.pop();
    }

    /// Every callable of destination `d` (path empty) and of its
    /// definition `def` (path `def`).
    fn all_callables(&mut self, d: usize, dest: &Table, def: Option<&Table>) -> Vec<Key> {
        let mut out = Vec::new();
        let mut stack = Vec::new();
        self.nested(dest, d, ("", ""), &mut out, &mut stack);
        if let Some(def) = def {
            self.nested(def, d, ("def", "def"), &mut out, &mut stack);
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
            self.resolve_one(skin, index, &table);
        }
    }

    /// The fact of source `src`: valid when it loads, invalid when it is
    /// missing or undefined, a movie, or unknown when production never
    /// reached it (s7-06e-design.md §3.8).
    fn source(&self, skin: &Table, src: &Value) -> Validity {
        let Some(id) = id_string(src) else {
            return Validity::Invalid;
        };
        if find(skin, "source", &id).is_none() {
            return Validity::Invalid;
        }
        match self.sources.get(&id) {
            Some(SourceFact::Loaded) => Validity::Valid,
            Some(SourceFact::Missing) => Validity::Invalid,
            Some(SourceFact::Movie) => Validity::Movie,
            Some(SourceFact::Unknown) | None => Validity::Unknown,
        }
    }

    /// A number's or an image set's `value` as its serializer resolves
    /// it (`LuaSkinLoader.java:155-167`): a function, an id, a name the
    /// factory knows, or a script; an id or name the factory returns null
    /// for falls back to `fallback` (`JsonSkinObjectLoader.java:85`,
    /// `:119-124`).
    fn value_sel(&mut self, v: &Value, index: usize, fallback: Sel) -> Sel {
        match v {
            Value::Function(f) => {
                Sel::Callable(f.clone(), self.key_at(f, index, ("value", 0), "def"))
            }
            // A number, or a string that reads as one, is an id by its
            // `toint()`; another string a name; anything else null.
            v if lua_number(v).is_some() => match toint(v) {
                Some(id) if self.vm.number_property(id) != PropValue::Null => {
                    Sel::Int(PropRef::Id(id))
                }
                _ => fallback,
            },
            Value::String(_) => {
                let name = string(v).unwrap_or_default();
                match self.vm.named_number_property(&name) {
                    None => Sel::Script,
                    Some(PropValue::Null) => fallback,
                    Some(_) => Sel::Int(PropRef::Name(name)),
                }
            }
            _ => fallback,
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one destination, as the loader reads it"
    )]
    fn resolve_one(&mut self, skin: &Table, index: usize, dest: &Table) {
        let Some(id) = id_string(&raw(dest, "id")) else {
            let callables = self.all_callables(index, dest, None);
            self.observed.unresolved.push(Listed {
                dest: index,
                what: "no id".into(),
                callables,
            });
            return;
        };
        // `JSONSkinLoader.java:316-337`: a negative integer is a system
        // image; else the object loader's lists, image first
        // (`JsonSkinObjectLoader.java:42-124`).
        let system = id.parse::<i32>().is_ok_and(|n| n < 0);
        let mut entries = Vec::new();
        let (kind, sel, validity) = if system {
            (Kind::Image, Sel::Zero, Validity::Valid)
        } else if let Some(img) = find(skin, "image", &id) {
            let len = i64::from(toint(&raw(&img, "len")).unwrap_or(0));
            let mut validity = self.source(skin, &raw(&img, "src"));
            // A movie source is the movie constructor, before `len` or the
            // grid (`JsonSkinObjectLoader.java:46-49`). A texture group
            // splits its `divx·divy` cells into `len` arrays (`:50-59`):
            // fewer cells than groups leave every array empty, so no source
            // validates.
            let cells = i64::from(toint(&raw(&img, "divx")).filter(|&d| d > 0).unwrap_or(1))
                * i64::from(toint(&raw(&img, "divy")).filter(|&d| d > 0).unwrap_or(1));
            if validity != Validity::Movie && len > 1 && cells / len == 0 {
                validity = Validity::Invalid;
            }
            // A plain image ignores `ref` and `value`; a group reads its
            // image-index `ref` (`JsonSkinObjectLoader.java:49-64`).
            let sel = if len > 1 && validity != Validity::Movie {
                Sel::Index(toint(&raw(&img, "ref")).unwrap_or(0))
            } else {
                Sel::Zero
            };
            (Kind::Image, sel, validity)
        } else if let Some(set) = find(skin, "imageset", &id) {
            let mut validity = Validity::Invalid;
            for i in list(&raw(&set, "images")) {
                let v = id_string(&i)
                    .and_then(|i| find(skin, "image", &i))
                    .map_or(Validity::Invalid, |img| {
                        self.source(skin, &raw(&img, "src"))
                    });
                entries.push(v);
                validity = match (validity, v) {
                    (Validity::Movie, _) | (_, Validity::Movie) => Validity::Movie,
                    (Validity::Valid, _) | (_, Validity::Valid) => Validity::Valid,
                    (Validity::Unknown, _) | (_, Validity::Unknown) => Validity::Unknown,
                    _ => Validity::Invalid,
                };
            }
            let by_ref = Sel::Index(toint(&raw(&set, "ref")).unwrap_or(0));
            let sel = self.value_sel(&raw(&set, "value"), index, by_ref);
            (Kind::Image, sel, validity)
        } else if let Some(num) = find(skin, "value", &id) {
            let by_ref = Sel::Int(PropRef::Id(toint(&raw(&num, "ref")).unwrap_or(0)));
            let sel = self.value_sel(&raw(&num, "value"), index, by_ref);
            (Kind::Number, sel, Validity::Valid)
        } else {
            let other = other_kind(skin, &id);
            let callables = self.all_callables(index, dest, other.as_ref().map(|(_, t)| t));
            let (list, what) = match other {
                Some((kind, _)) => (&mut self.observed.unmodelled, kind),
                None => (&mut self.observed.unresolved, "unresolved id".into()),
            };
            list.push(Listed {
                dest: index,
                what,
                callables,
            });
            return;
        };
        // A keyframe is needed (`SkinObject.java:577-579`).
        let validity = if list(&raw(dest, "dst")).is_empty() {
            Validity::Invalid
        } else {
            validity
        };
        // Conditions: numeric properties, then names and functions, then
        // `draw` (`JSONSkinLoader.java:458-462`, `SkinObject.java:282-315`).
        let mut ids = Vec::new();
        let mut rest = Vec::new();
        let mut options = Vec::new();
        // Each `op` entry by its serializer (`LuaSkinLoader.java:144-150`):
        // a number, or a string that reads as one, is an id by its
        // `toint()`; a function or another string a property; anything
        // else a null property, no condition.
        for (i, v) in list(&raw(dest, "op")).into_iter().enumerate() {
            match &v {
                Value::Function(f) => {
                    let k = self.key(f, index, "op", u32::try_from(i).unwrap_or(u32::MAX));
                    rest.push(Cond::Func(f.clone(), k));
                }
                v if lua_number(v).is_some() => match toint(v) {
                    Some(0) | None => {}
                    Some(n) if ReferenceVm::is_property(n) => {
                        if !ids.contains(&n) {
                            ids.push(n);
                        }
                    }
                    Some(n) => options.push(n),
                },
                Value::String(_) => rest.push(Cond::Named(string(&v).unwrap_or_default())),
                _ => {}
            }
        }
        let mut conds: Vec<Cond> = ids.into_iter().map(Cond::Prop).collect();
        conds.extend(rest);
        // `draw` by its serializer: a function, an id or a name
        // (`LuaSkinLoader.java:120-122`, `:155-167`).
        match raw(dest, "draw") {
            Value::Function(f) => {
                let k = self.key(&f, index, "draw", 0);
                conds.push(Cond::Func(f, k));
            }
            // An id (a number, or a string that reads as one) the
            // boolean-property factory does not know gives null, and the
            // source adds no condition (`JSONSkinLoader.java:458-462`):
            // `draw` holds no configured option.
            v if lua_number(&v).is_some() => {
                if let Some(n) = toint(&v).filter(|&n| n != 0 && ReferenceVm::is_property(n)) {
                    conds.push(Cond::Prop(n));
                }
            }
            v @ Value::String(_) => conds.push(Cond::Named(string(&v).unwrap_or_default())),
            _ => {}
        }
        let timer = match raw(dest, "timer") {
            Value::Function(f) => {
                let k = self.key(&f, index, "timer", 0);
                Some((f, k))
            }
            _ => None,
        };
        let obj = Obj {
            dest: index,
            kind,
            sel,
            entries,
            conds,
            timer,
            options,
            validity,
        };
        if obj.validity == Validity::Movie {
            let callables = obj_keys(&obj);
            self.observed.unmodelled.push(Listed {
                dest: index,
                what: "movie".into(),
                callables,
            });
            return;
        }
        self.objects.push(obj);
    }

    /// The source's installation (`Skin.java:199-233`), on the first
    /// frame: an object that does not validate, a false static condition
    /// (numeric, named or negated, each keeping its property's class) or
    /// an option not matching removes it; a true static one is dropped.
    /// An object whose validity the facts leave unknown, or whose static
    /// condition the model cannot evaluate, is unresolved unless another
    /// step removes it.
    fn install(&mut self) {
        let objects = std::mem::take(&mut self.objects);
        for mut obj in objects {
            let mut removed: Option<&'static str> = None;
            let mut unknown: Option<&'static str> = None;
            match obj.validity {
                Validity::Invalid => removed = Some("invalid"),
                Validity::Unknown => unknown = Some("source not reached"),
                Validity::Valid | Validity::Movie => {}
            }
            let mut kept = Vec::new();
            for c in obj.conds.drain(..) {
                let stat = match &c {
                    Cond::Prop(n) if ReferenceVm::is_static(*n) => {
                        Some(self.vm.condition(*n, self.items))
                    }
                    Cond::Named(name)
                        if ReferenceVm::condition_id(name).is_some_and(ReferenceVm::is_static) =>
                    {
                        Some(self.vm.named_condition(name))
                    }
                    _ => None,
                };
                match stat {
                    None => kept.push(c),
                    Some(Some(true)) => {}
                    Some(Some(false)) => removed = removed.or(Some("static condition")),
                    Some(None) => unknown = unknown.or(Some("static condition unknown")),
                }
            }
            obj.conds = kept;
            // Options (`Skin.java:214-226`): an absent one reads -1.
            if obj
                .options
                .iter()
                .any(|&op| self.items.get(&op.abs()).copied() != Some(op > 0))
            {
                removed = removed.or(Some("option"));
            }
            match removed.or(unknown) {
                None => {
                    self.observed.installed.push(obj_keys(&obj));
                    self.objects.push(obj);
                }
                Some(why) => {
                    let callables = obj_keys(&obj);
                    let l = Listed {
                        dest: obj.dest,
                        what: why.into(),
                        callables,
                    };
                    if removed.is_some() {
                        self.observed.removed.push(l);
                    } else {
                        self.observed.unresolved.push(l);
                    }
                }
            }
        }
    }

    /// Runs every frame of `frames`: installation first, on the first play
    /// frame's host, since it reads only the play state's static facts;
    /// then each frame on its own host (s7-06e-2-design.md §5).
    pub fn run(mut self, frames: &[FrameInputs]) -> Observed {
        if let Some(f) = frames.iter().find(|f| f.is_play()).or(frames.first()) {
            self.host(f);
            self.install();
        }
        for f in frames {
            self.host(f);
            let mut out = RefFrame {
                outside: !f.is_play(),
                ..RefFrame::default()
            };
            for o in &self.objects {
                let from = out.points.len();
                prepare(self.vm, self.items, o, &mut out);
                out.objects.push(from..out.points.len());
            }
            self.observed.frames.push(out);
        }
        self.observed
    }

    /// Sets the VM's host to frame `f`'s.
    fn host(&self, f: &FrameInputs) {
        match f.host() {
            Host::Play => {
                self.vm
                    .set_play(f.fields(), f.timers(), f.state_us(), Some(f.score()));
            }
            Host::Outside { allowed, page } => {
                let mut fields = f.fields().to_vec();
                if let (Some((at, point)), Some(p)) = (self.page_point, *page)
                    && at == p
                    && let Some(cell) = fields.get_mut(usize::from(skin_model::field::POINT))
                {
                    *cell = point;
                }
                self.vm.set_outside(&fields, allowed, f.state_us());
            }
        }
    }

    /// Answers the point as `point` on the practice page of ordinal `page`
    /// (a test's mutation control, as `--perturb` is for play frames).
    #[cfg(test)]
    pub fn page_point(mut self, page: usize, point: i32) -> Self {
        self.page_point = Some((page, point));
        self
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

/// An integer property's value as the source's prepare reads it: a null
/// one is `Integer.MIN_VALUE` for a number and 0 for an image
/// (`SkinNumber.java:130-138`, `SkinImage.java:123-125`).
fn prop(vm: &ReferenceVm, p: &PropRef, kind: &Kind) -> Result<i32, Cause> {
    let v = match p {
        PropRef::Id(id) => vm.number_property(*id),
        PropRef::Name(n) => vm
            .named_number_property(n)
            .unwrap_or(PropValue::Unsupported),
    };
    match (v, kind) {
        (PropValue::Value(n), _) => Ok(n),
        (PropValue::Null, Kind::Number) => Ok(i32::MIN),
        (PropValue::Null, Kind::Image) => Ok(0),
        (PropValue::Unavailable, _) => Err(Cause::Unavailable),
        (PropValue::Unsupported, _) => Err(Cause::Inconclusive),
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
        Sel::Int(p) => prop(vm, p, &o.kind),
        Sel::Script => Err(Cause::Inconclusive),
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
            Cond::Prop(n) => vm.condition(*n, items).ok_or(Cause::Inconclusive),
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
        // 5. The chosen source (`SkinImage.java:133-139`): past the end
        // selects 0; a hole hides the object after all this.
        let shown = if o.entries.is_empty() {
            Some(true)
        } else {
            let i = usize::try_from(sel)
                .ok()
                .filter(|&i| i < o.entries.len())
                .unwrap_or(0);
            match o.entries.get(i) {
                Some(Validity::Valid) => Some(true),
                Some(Validity::Unknown | Validity::Movie) => None,
                _ => Some(false),
            }
        };
        out.timers.push(TimerOut {
            key: *k,
            on,
            start,
            shown,
        });
    }
}
