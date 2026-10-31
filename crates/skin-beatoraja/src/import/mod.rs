//! The mapping (S6b-1): a resolved definition's images, numbers and text
//! to a [`skin_model::SkinDef`], with a [`Report`] of what was changed,
//! dropped or ignored.
//!
//! [`import`] resolves the definition again with the configured options,
//! walks the destinations in source order, maps each element (its role,
//! property, conditions, timers, track, assets and widget), finishes the
//! sheet, builds the skin and locates the report. Every table it grows is
//! charged first ([`budget`]); a play-critical element it cannot
//! reproduce refuses the import, anything else is dropped with a note
//! ([`policy`]).

mod asset;
mod budget;
mod geom;
mod object;
mod ops;
mod policy;
mod property;
mod report;
mod timer;
mod track;
mod widget;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bms_model::TimeUs;
use skin_model::{
    Anchor, Blend, CycleDef, Ease, ElementDef, Filter, Fit, KeyDef, LoadError, MAX_GEOMETRY, PLAY,
    Place, Program, Region, RegionDef, Scope, Skin, SkinDef, Source, TimerRef, Track, Ty,
    WidgetDefs, compile, timer as slot,
};

pub use budget::ImportLimit;
pub use policy::CriticalPart;
pub use report::{
    DeviationId, FileChoice, Note, NoteKind, NoteLoc, Object, PropertyKind, Report, Rule, Severity,
    TimerWhy, TrackWhy, Usage,
};

use self::asset::{Assets, Choices};
use self::budget::Budget;
use self::geom::{Offset, Q, Rect};
use self::object::Objects;
use self::ops::Cond;
use self::policy::{Outcome, Role};
use self::report::{Notes, Span};
use self::timer::{Custom, Timer};
use self::track::{Accepted, SrcKey, Stretch};
use crate::error::{ImportError, Loc};
use crate::header::Header;
use crate::resolve::{Definition, Resolved, Tables, resolve};
use crate::tree::{Kind, Scalar};

/// What the user configured for one skin: the selected option items, the
/// file chosen for each file choice, and the offsets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// The selected option items' numbers.
    pub options: BTreeSet<i32>,
    /// File choice name to the chosen candidate's name.
    pub files: BTreeMap<String, String>,
    pub offsets: BTreeMap<i32, OffsetValue>,
}

/// A user offset: reference pixels (the source at a 1920×1080 window),
/// degrees, and a signed alpha delta in 1/255 units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OffsetValue {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub r: i32,
    pub a: i32,
}

impl Config {
    /// The source's defaults (`SkinConfiguration.java:260-290` at
    /// `0.8.9`): each option's item named by its default, else its first
    /// item; no file chosen; no offsets.
    #[must_use]
    pub fn defaults(header: &Header) -> Self {
        let options = header
            .options
            .iter()
            .filter_map(|o| {
                let by_def = o
                    .def
                    .as_ref()
                    .and_then(|d| o.items.iter().find(|i| i.name.as_ref() == Some(d)));
                by_def.or(o.items.first()).map(|i| i.op)
            })
            .collect();
        Self {
            options,
            ..Self::default()
        }
    }
}

/// An imported skin and its report.
#[derive(Debug)]
pub struct Imported {
    pub skin: Skin,
    pub report: Report,
}

/// Every region anchored top left; a fixed-canvas skin uses only
/// `screen`.
const REGIONS: [RegionDef; Region::COUNT] = [RegionDef {
    anchor: Anchor::TopLeft,
    optional: false,
}; Region::COUNT];

/// The runtime offsets the play sets (`SkinProperty.java:943-947`).
const RUNTIME_OFFSETS: std::ops::RangeInclusive<i32> = 1..=5;
/// The all-elements offset, set by the player's settings (S11).
const OFFSET_ALL: i32 = 10;
/// The highest offset id (`SkinProperty.java:972`).
const OFFSET_MAX: i32 = 199;

/// Imports `def`, whose header `header` was read from it, with `config`.
///
/// # Errors
///
/// What resolution refuses; an importer budget or aggregate asset limit
/// ([`ImportError::Limit`]); geometry outside the model
/// ([`ImportError::Range`]); a play-critical element the importer cannot
/// reproduce ([`ImportError::Critical`], [`ImportError::Asset`]); a skin
/// the model refuses ([`ImportError::Model`]).
pub fn import(
    def: &mut Definition,
    header: &Header,
    config: &Config,
) -> Result<Imported, ImportError> {
    let mut resolved = resolve(def, &config.options)?;
    let tables = resolved.tables(&config.options)?;
    let Resolved { def, size, .. } = resolved;
    let mut files = Vec::new();
    let choices = Choices::new(header, config, &mut files);
    let r = geom::scale(size[0]).ok_or(ImportError::Resolution {
        at: None,
        w: i32::try_from(size[0]).unwrap_or(i32::MAX),
        h: i32::try_from(size[1]).unwrap_or(i32::MAX),
    })?;
    let mut cx = Cx::new(def, header, config, tables, choices, r)?;
    let destinations = object::records(&cx.tables, "destination").to_vec();
    for (index, rec) in destinations.into_iter().enumerate() {
        cx.destination(index, rec)?;
    }
    cx.finish(files)
}

/// A destination's fields.
struct Dst {
    span: Span,
    blend: i32,
    filter: i32,
    timer: Prop,
    looped: i32,
    center: i32,
    offsets: Vec<i32>,
    stretch: i32,
    ops: Vec<Op>,
    draw: Prop,
    keys: Vec<u32>,
    mouse: bool,
}

/// A property field as the source's script serializer reads it
/// (`JsonSkinSerializer.java:341-371`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Prop {
    /// Absent, null, or neither a number nor a string.
    Null,
    Id(i32),
    Name(String),
}

/// A destination option (`JsonSkinSerializer.java:115-131`).
#[derive(Clone, Debug)]
enum Op {
    Id(i32),
    Name(String),
}

/// What the common part of an element decided.
struct Common {
    role: Role,
    span: Span,
    track: Accepted,
    ease: Ease,
    timer: TimerRef,
    visible: Option<Program>,
    blend: Blend,
    filter: Filter,
    pivot: Anchor,
    offset: Offset,
    angle: i64,
    stretch: Stretch,
    stretch_mode: i32,
    untimed: bool,
}

/// The import's running state.
pub(crate) struct Cx<'d> {
    def: &'d mut Definition,
    header: &'d Header,
    config: &'d Config,
    tables: Tables,
    /// The scale from the skin's pixels to reference pixels.
    r: Q,
    entry_dir: String,
    budget: Budget,
    notes: Notes,
    objects: Objects,
    sources: Option<HashMap<String, u32>>,
    fonts: Option<HashMap<String, u32>>,
    images: Option<HashMap<String, u32>>,
    custom: Custom,
    assets: Assets,
    choices: Choices,
    /// Every declared option item's number.
    declared: BTreeSet<i32>,
    whens: Vec<Program>,
    when_spans: Vec<Span>,
    ranks: [Option<u16>; 3],
    elements: Vec<ElementDef>,
    element_spans: Vec<Span>,
    widgets: WidgetDefs,
}

/// A witness of the order of an import's work.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Charge,
    Compile,
    Admit,
}

#[cfg(test)]
thread_local! {
    /// The order of this thread's charges, compiles and admissions.
    pub(crate) static TRACE: std::cell::RefCell<Vec<Event>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Records `e` in [`TRACE`].
#[cfg(test)]
pub(crate) fn trace(e: Event) {
    TRACE.with(|t| t.borrow_mut().push(e));
}

impl<'d> Cx<'d> {
    fn new(
        def: &'d mut Definition,
        header: &'d Header,
        config: &'d Config,
        tables: Tables,
        choices: Choices,
        r: Q,
    ) -> Result<Self, ImportError> {
        let entry_dir = def.entry_dir().to_owned();
        let assets = Assets::new(def.root());
        let declared = header
            .options
            .iter()
            .flat_map(|o| o.items.iter().map(|i| i.op))
            .collect();
        let mut cx = Self {
            def,
            header,
            config,
            tables,
            r,
            entry_dir,
            budget: Budget::default(),
            notes: Notes::default(),
            objects: Objects::default(),
            sources: None,
            fonts: None,
            images: None,
            custom: Custom::default(),
            assets,
            choices,
            declared,
            whens: Vec::new(),
            when_spans: Vec::new(),
            ranks: [None; 3],
            elements: Vec::new(),
            element_spans: Vec::new(),
            widgets: WidgetDefs::default(),
        };
        cx.config_notes();
        cx.custom_timers()?;
        Ok(cx)
    }

    fn arena_span(&self, id: u32) -> Span {
        Span::of(&self.def.arena, id)
    }

    fn loc(&self, at: Span) -> Loc {
        self.def.arena.loc_at(at.file, at.offset)
    }

    fn limit(&self, at: Span, limit: ImportLimit) -> ImportError {
        ImportError::Limit {
            at: Some(self.loc(at)),
            limit,
        }
    }

    fn range(&self, at: Span, field: &'static str) -> ImportError {
        ImportError::Range {
            at: self.loc(at),
            field,
        }
    }

    fn note(&mut self, at: Option<Span>, severity: Severity, kind: NoteKind) {
        self.notes.push(at, severity, kind);
        self.budget.usage.notes = u32::try_from(self.notes.list.len()).unwrap_or(u32::MAX);
    }

    fn ignored(&mut self, at: Span, kind: NoteKind) {
        self.note(Some(at), Severity::Ignored, kind);
    }

    fn drop_el(&mut self, role: Role, at: Span, why: NoteKind) -> Result<Outcome, ImportError> {
        let r = policy::unsupported(&self.def.arena, role, at, why, &mut self.notes);
        self.budget.usage.notes = u32::try_from(self.notes.list.len()).unwrap_or(u32::MAX);
        r
    }

    /// Configuration entries the header does not know.
    fn config_notes(&mut self) {
        let names: BTreeSet<&str> = self
            .header
            .files
            .iter()
            .filter_map(|f| f.name.as_deref())
            .collect();
        let unknown_files = self
            .config
            .files
            .keys()
            .filter(|n| !names.contains(n.as_str()))
            .count();
        // A configured value that is not a candidate, "Random" among them
        // leaves the choice to its default.
        let not_candidates = self
            .header
            .files
            .iter()
            .filter(|f| {
                f.name
                    .as_ref()
                    .and_then(|n| self.config.files.get(n))
                    .is_some_and(|v| !f.candidates.contains(v))
            })
            .count();
        let unknown_files = unknown_files + not_candidates;
        for _ in 0..unknown_files {
            self.note(
                None,
                Severity::Ignored,
                NoteKind::Ignored { field: "files" },
            );
        }
        let ids: BTreeSet<i32> = self.header.offsets.iter().map(|o| o.id).collect();
        let notes: Vec<NoteKind> = self
            .config
            .offsets
            .iter()
            .filter_map(|(&id, v)| {
                if id == OFFSET_ALL && *v != OffsetValue::default() {
                    Some(NoteKind::Ignored { field: "offset 10" })
                } else if !ids.contains(&id) {
                    Some(NoteKind::Ignored { field: "offsets" })
                } else {
                    None
                }
            })
            .collect();
        for kind in notes {
            self.note(None, Severity::Ignored, kind);
        }
    }

    /// The custom timers (`Skin.java:741-743`, `CustomTimer.java:20-58`):
    /// the last of an id wins; one in the built-in range has no effect.
    fn custom_timers(&mut self) -> Result<(), ImportError> {
        let recs = object::records(&self.tables, "customTimers").to_vec();
        for rec in recs {
            let [id, t] = self.def.fields(rec, ["id", "timer"]);
            let id = match id {
                Some(f) => self.def.int(f, "id")?,
                None => 0,
            };
            let target = match self.prop(t, "timer")? {
                Prop::Id(n) if n >= 0 => Some(n),
                _ => None,
            };
            if id <= timer::BUILTIN_MAX {
                let at = self.arena_span(rec);
                self.ignored(at, NoteKind::Timer(TimerWhy::CustomBuiltin(id)));
                continue;
            }
            self.custom.0.insert(id, target);
        }
        Ok(())
    }

    /// A property field as the script serializer reads it.
    fn prop(&mut self, field: Option<u32>, name: &'static str) -> Result<Prop, ImportError> {
        let Some(f) = field else {
            return Ok(Prop::Null);
        };
        Ok(match self.def.arena.scalar(f) {
            Scalar::Long(_) | Scalar::Double(_) => Prop::Id(self.def.int(f, name)?),
            Scalar::Str { .. } => self.def.string(f, name)?.map_or(Prop::Null, Prop::Name),
            Scalar::Null | Scalar::Bool(_) | Scalar::Object | Scalar::Array => Prop::Null,
        })
    }

    fn int_or(
        &mut self,
        field: Option<u32>,
        name: &'static str,
        or: i32,
    ) -> Result<i32, ImportError> {
        field.map_or(Ok(or), |f| self.def.int(f, name))
    }

    fn opt_string(
        &mut self,
        field: Option<u32>,
        name: &'static str,
    ) -> Result<Option<String>, ImportError> {
        field.map_or(Ok(None), |f| self.def.string(f, name))
    }

    /// The `int[]` items of a literal array field.
    fn ints(&mut self, field: Option<u32>, name: &'static str) -> Result<Vec<i32>, ImportError> {
        let Some(f) = field else {
            return Ok(Vec::new());
        };
        if self.def.arena.node(f).kind != Kind::Array {
            return Err(self.def.type_error(f, name, "an array"));
        }
        let items: Vec<u32> = self.def.arena.children(f).collect();
        items.into_iter().map(|i| self.def.int(i, name)).collect()
    }

    fn read_dst(&mut self, index: usize, rec: u32) -> Result<(Option<String>, Dst), ImportError> {
        let [
            id,
            blend,
            filter,
            timer,
            looped,
            center,
            offset,
            offsets,
            stretch,
            op,
            draw,
            mouse,
        ] = self.def.fields(
            rec,
            [
                "id",
                "blend",
                "filter",
                "timer",
                "loop",
                "center",
                "offset",
                "offsets",
                "stretch",
                "op",
                "draw",
                "mouseRect",
            ],
        );
        let id = self.opt_string(id, "id")?;
        let mut ids = self.ints(offsets, "offsets")?;
        ids.push(self.int_or(offset, "offset", 0)?);
        let mut ops = Vec::new();
        if let Some(op) = op {
            if self.def.arena.node(op).kind != Kind::Array {
                return Err(self.def.type_error(op, "op", "an array"));
            }
            let items: Vec<u32> = self.def.arena.children(op).collect();
            for item in items {
                match self.prop(Some(item), "op")? {
                    Prop::Id(n) => ops.push(Op::Id(n)),
                    Prop::Name(s) => ops.push(Op::Name(s)),
                    Prop::Null => {}
                }
            }
        }
        let start = index
            .checked_sub(1)
            .and_then(|i| self.tables.key_ends.get(i))
            .map_or(0, |&e| e as usize);
        let end = self
            .tables
            .key_ends
            .get(index)
            .map_or(start, |&e| e as usize);
        let keys = self
            .tables
            .keys
            .get(start..end)
            .unwrap_or_default()
            .to_vec();
        let dst = Dst {
            span: self.arena_span(rec),
            blend: self.int_or(blend, "blend", 0)?,
            filter: self.int_or(filter, "filter", 0)?,
            timer: self.prop(timer, "timer")?,
            looped: self.int_or(looped, "loop", 0)?,
            center: self.int_or(center, "center", 0)?,
            offsets: ids,
            stretch: self.int_or(stretch, "stretch", -1)?,
            ops,
            draw: self.prop(draw, "draw")?,
            keys,
            mouse: mouse.is_some_and(|m| self.def.arena.node(m).kind == Kind::Object),
        };
        Ok((id, dst))
    }

    /// One destination, in source order.
    fn destination(&mut self, index: usize, rec: u32) -> Result<(), ImportError> {
        let at = self.arena_span(rec);
        self.budget.element().map_err(|l| self.limit(at, l))?;
        #[cfg(test)]
        trace(Event::Charge);
        let (id, dst) = self.read_dst(index, rec)?;
        let Some(id) = id else {
            self.ignored(at, NoteKind::UndefinedObject);
            return Ok(());
        };
        // A negative number is a player-drawn image (`JSONSkinLoader.java:
        // 316-339`).
        if ops::java_int(&id).is_some_and(|n| n < 0) {
            self.drop_el(
                Role::Decorative,
                at,
                NoteKind::Unsupported {
                    object: Object::System,
                },
            )?;
            return Ok(());
        }
        let Some((object, orec)) = self.objects.lookup(self.def, &self.tables, &id)? else {
            self.ignored(at, NoteKind::UndefinedObject);
            return Ok(());
        };
        let part = match object {
            Object::Note => Some(CriticalPart::Lanes),
            Object::Judge => Some(CriticalPart::Judge),
            Object::Gauge => Some(CriticalPart::Gauge),
            Object::HiddenCover => Some(CriticalPart::Cover),
            Object::LiftCover => Some(CriticalPart::Lift),
            _ => None,
        };
        if let Some(part) = part {
            return Err(ImportError::Critical {
                at: self.loc(at),
                part,
                why: NoteKind::NotYet { object },
            });
        }
        match object {
            Object::Image => self.image(dst, orec, None),
            Object::ImageSet => self.image_set(dst, orec),
            Object::Value => self.number(dst, orec),
            Object::Text => self.text(dst, orec),
            _ => {
                self.drop_el(Role::Decorative, at, NoteKind::Unsupported { object })?;
                Ok(())
            }
        }
    }

    /// The common part of an element: track, conditions,
    /// timer, offsets, blend and pivot, each through the role policy.
    /// `None` when the element was dropped or is never drawn.
    fn common(&mut self, role: Role, dst: &Dst) -> Result<Option<Common>, ImportError> {
        let at = dst.span;
        if dst.mouse {
            self.ignored(at, NoteKind::Ignored { field: "mouseRect" });
        }
        // Keys: charged as written, before the track is built.
        let fits = self
            .budget
            .keys(dst.keys.len())
            .map_err(|l| self.limit(at, l))?;
        let keys = track::read(self.def, &dst.keys)?;
        if keys.clip {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "clip" })?;
            return Ok(None);
        }
        if !fits {
            self.drop_el(role, at, NoteKind::Track(TrackWhy::Limits))?;
            return Ok(None);
        }
        let ease = match keys.acc {
            1 => Ease::In,
            2 => Ease::Out,
            3 => Ease::Step,
            _ => Ease::Linear,
        };
        let track = match track::accept(keys.keys, dst.looped) {
            Ok(Some(t)) => t,
            Ok(None) => {
                self.ignored(at, NoteKind::Track(TrackWhy::NeverDrawn));
                return Ok(None);
            }
            Err(why) => {
                self.drop_el(role, at, NoteKind::Track(why))?;
                return Ok(None);
            }
        };

        // Conditions.
        let mut conds: Vec<String> = Vec::new();
        let mut seen = BTreeSet::new();
        for op in &dst.ops {
            let cond = match op {
                Op::Id(0) => continue,
                Op::Id(n) => {
                    if !seen.insert(*n) {
                        continue;
                    }
                    ops::numeric(*n, &self.declared, &self.config.options)
                }
                Op::Name(s) => match ops::named(s) {
                    Some(c) => c,
                    None => {
                        let why = NoteKind::UnsupportedProperty {
                            kind: PropertyKind::Boolean,
                            id: None,
                        };
                        self.drop_el(role, at, why)?;
                        return Ok(None);
                    }
                },
            };
            if !self.condition(role, at, cond, &mut conds)? {
                return Ok(None);
            }
        }
        match &dst.draw {
            Prop::Null => {}
            Prop::Id(n) => {
                if ops::is_builtin(i32::try_from(n.unsigned_abs()).unwrap_or(0)) {
                    let c = ops::numeric(*n, &BTreeSet::new(), &BTreeSet::new());
                    if !self.condition(role, at, c, &mut conds)? {
                        return Ok(None);
                    }
                } else {
                    self.ignored(at, NoteKind::Ignored { field: "draw" });
                }
            }
            Prop::Name(s) => {
                let Some(c) = ops::named(s) else {
                    let why = NoteKind::UnsupportedProperty {
                        kind: PropertyKind::Boolean,
                        id: None,
                    };
                    self.drop_el(role, at, why)?;
                    return Ok(None);
                };
                if !self.condition(role, at, c, &mut conds)? {
                    return Ok(None);
                }
            }
        }

        // Timer.
        let (timer, untimed) = match dst.timer {
            Prop::Null => (TimerRef::Builtin(slot::READY), true),
            Prop::Id(n) if n < 0 => (TimerRef::Builtin(slot::READY), true),
            Prop::Name(_) => {
                self.drop_el(role, at, NoteKind::Timer(TimerWhy::Script))?;
                return Ok(None);
            }
            Prop::Id(n) => match self.custom.resolve(n) {
                Timer::Unset => {
                    self.ignored(at, NoteKind::Timer(TimerWhy::Unset(n)));
                    return Ok(None);
                }
                Timer::Unsupported => {
                    let why = if n > timer::BUILTIN_MAX {
                        TimerWhy::Custom(n)
                    } else {
                        TimerWhy::Unsupported(n)
                    };
                    self.drop_el(role, at, NoteKind::Timer(why))?;
                    return Ok(None);
                }
                t => (self.timer_ref(t, at)?, false),
            },
        };

        // Offsets.
        let mut ids: Vec<i32> = Vec::new();
        for &id in &dst.offsets {
            if (1..=OFFSET_MAX).contains(&id) && !ids.contains(&id) {
                ids.push(id);
            }
        }
        let mut offset = Offset::default();
        let mut alpha: Vec<i64> = Vec::new();
        for id in ids {
            if RUNTIME_OFFSETS.contains(&id) {
                self.drop_el(role, at, NoteKind::RuntimeOffset(id))?;
                return Ok(None);
            }
            if id == OFFSET_ALL {
                continue;
            }
            // A value for an offset the header does not declare is ignored
            // (`config_notes`).
            let declared = self.header.offsets.iter().any(|o| o.id == id);
            let Some(v) = self.config.offsets.get(&id).copied().filter(|_| declared) else {
                continue;
            };
            offset.x += i64::from(v.x);
            offset.y += i64::from(v.y);
            offset.w += i64::from(v.w);
            offset.h += i64::from(v.h);
            offset.r += i64::from(v.r);
            if v.a != 0 {
                alpha.push(i64::from(v.a));
            }
        }
        let mut track = track;
        if !alpha.is_empty() {
            let same_sign = alpha.iter().all(|&a| a > 0) || alpha.iter().all(|&a| a < 0);
            if !track.fixed_color() || !same_sign {
                self.drop_el(role, at, NoteKind::OffsetAlpha)?;
                return Ok(None);
            }
            let sum: i64 = alpha.iter().sum();
            for k in &mut track.keys {
                let [a, ..] = &mut k.color;
                *a = i32::try_from((i64::from(*a) + sum).clamp(0, 255)).unwrap_or(255);
            }
        }

        // Blend (`Skin.java:654-665`).
        let blend = match dst.blend {
            2 => Blend::Add,
            3 => {
                self.ignored(at, NoteKind::Ignored { field: "blend 3" });
                Blend::Add
            }
            4 | 9 => {
                self.drop_el(role, at, NoteKind::Blend(dst.blend))?;
                return Ok(None);
            }
            _ => Blend::Alpha,
        };

        // Visibility, compiled after its charges.
        let visible = if conds.is_empty() {
            None
        } else {
            let src = conds
                .iter()
                .map(|c| format!("({c})"))
                .collect::<Vec<_>>()
                .join(" && ");
            Some(self.compile(&src, Ty::Bool, at)?)
        };

        Ok(Some(Common {
            role,
            span: at,
            ease,
            timer,
            visible,
            blend,
            filter: if dst.filter != 0 {
                Filter::Linear
            } else {
                Filter::Nearest
            },
            pivot: pivot(dst.center),
            offset,
            angle: offset_angle(&offset),
            stretch: Stretch::of(dst.stretch),
            stretch_mode: dst.stretch,
            untimed,
            track,
        }))
    }

    /// Applies one condition: `Ok(false)` when the element is dropped or
    /// never drawn.
    fn condition(
        &mut self,
        role: Role,
        at: Span,
        c: Cond,
        conds: &mut Vec<String>,
    ) -> Result<bool, ImportError> {
        match c {
            Cond::Expr(e) => {
                conds.push(e);
                Ok(true)
            }
            Cond::Fold(true) => Ok(true),
            Cond::Fold(false) => {
                self.ignored(at, NoteKind::Ignored { field: "op" });
                Ok(false)
            }
            Cond::Unsupported(id) => {
                let why = NoteKind::UnsupportedProperty {
                    kind: PropertyKind::Boolean,
                    id: Some(id),
                };
                self.drop_el(role, at, why)?;
                Ok(false)
            }
            Cond::Undefined(id) => {
                match role {
                    Role::Critical(_) => {
                        self.drop_el(role, at, NoteKind::UndefinedOp(id))?;
                    }
                    Role::Decorative => self.ignored(at, NoteKind::UndefinedOp(id)),
                }
                Ok(false)
            }
        }
    }

    /// Compiles `src` against the play schema and charges its operations
    /// before it is kept.
    fn compile(&mut self, src: &str, want: Ty, at: Span) -> Result<Program, ImportError> {
        #[cfg(test)]
        trace(Event::Compile);
        let p = compile(src, &Scope::new(&PLAY), want).map_err(|source| ImportError::Expr {
            at: self.loc(at),
            source,
        })?;
        self.budget.ops(&p).map_err(|l| self.limit(at, l))?;
        Ok(p)
    }

    /// The model reference of a mapped timer, adding a rank's `when`
    /// condition on first use.
    fn timer_ref(&mut self, t: Timer, at: Span) -> Result<TimerRef, ImportError> {
        if let Timer::Rank(k) = t
            && self.ranks.get(k).copied().flatten().is_none()
        {
            self.budget.when().map_err(|l| self.limit(at, l))?;
            #[cfg(test)]
            trace(Event::Charge);
            let src = timer::rank_source(k).unwrap_or_default();
            let p = self.compile(&src, Ty::Bool, at)?;
            let index =
                u16::try_from(self.whens.len()).map_err(|_| self.limit(at, ImportLimit::Whens))?;
            self.whens.push(p);
            self.when_spans.push(at);
            if let Some(slot) = self.ranks.get_mut(k) {
                *slot = Some(index);
            }
        }
        timer::model_ref(t, &self.ranks).ok_or_else(|| self.range(at, "timer"))
    }

    /// An image's frame cycle (`SkinSourceImage.java:73-89`): `Ok(None)`
    /// for no cycle; `Err(why)` for one the model cannot run.
    fn cycle(
        &mut self,
        at: Span,
        frames: u32,
        t: &Prop,
        cycle: i32,
    ) -> Result<Result<(Option<CycleDef>, bool), NoteKind>, ImportError> {
        if frames <= 1 || cycle == 0 {
            return Ok(Ok((None, true)));
        }
        let period = i64::from(cycle) * 1000;
        if cycle < 0 || period > skin_model::MAX_KEY_US {
            return Ok(Err(NoteKind::Timer(TimerWhy::Cycle)));
        }
        let (timer, on_timer) = match *t {
            Prop::Null => (TimerRef::Builtin(slot::READY), false),
            Prop::Id(n) if n < 0 => (TimerRef::Builtin(slot::READY), false),
            Prop::Name(_) => return Ok(Err(NoteKind::Timer(TimerWhy::Script))),
            Prop::Id(n) => match self.custom.resolve(n) {
                Timer::Unset => {
                    self.ignored(at, NoteKind::Timer(TimerWhy::Unset(n)));
                    return Ok(Ok((None, true)));
                }
                Timer::Unsupported => {
                    let why = if n > timer::BUILTIN_MAX {
                        TimerWhy::Custom(n)
                    } else {
                        TimerWhy::Unsupported(n)
                    };
                    return Ok(Err(NoteKind::Timer(why)));
                }
                t => (self.timer_ref(t, at)?, true),
            },
        };
        Ok(Ok((
            Some(CycleDef {
                period: TimeUs(period),
                timer,
            }),
            on_timer,
        )))
    }

    /// The model keys of `c`'s track with each key's rect from `rect`.
    fn model_track(&self, c: &Common, rects: &[[i32; 4]]) -> Result<Track, ImportError> {
        let mut keys = Vec::with_capacity(c.track.keys.len());
        for (k, rect) in c.track.keys.iter().zip(rects) {
            let angle = c.angle_of(k);
            if angle.unsigned_abs() > MAX_GEOMETRY.unsigned_abs() as u64 {
                return Err(self.range(c.span, "angle"));
            }
            let [a, r, g, b] = k.color;
            let [x, y, w, h] = *rect;
            keys.push(KeyDef {
                at: TimeUs(k.at * 1000),
                props: [
                    Some(x),
                    Some(y),
                    Some(w),
                    Some(h),
                    Some(a),
                    Some(r),
                    Some(g),
                    Some(b),
                    Some(i32::try_from(angle).unwrap_or(0)),
                ],
            });
        }
        Track::new(&keys, c.ease, c.track.end).map_err(|_| self.range(c.span, "dst"))
    }

    /// Key `k`'s rect in reference pixels, y up, with the offsets.
    fn key_rect(&self, c: &Common, k: &SrcKey) -> Result<Rect, ImportError> {
        Rect::scaled(k.rect, self.r)
            .and_then(|r| r.offset(c.offset))
            .ok_or_else(|| self.range(c.span, "dst"))
    }

    /// Adds an element and its location.
    fn push(&mut self, c: &Common, source: Source, track: Track) {
        self.elements.push(ElementDef {
            source,
            track,
            timer: c.timer,
            visible: c.visible.clone(),
            blend: c.blend,
            clip: None,
            place: Place {
                pivot: c.pivot,
                ..Place::SCREEN
            },
        });
        self.element_spans.push(c.span);
    }

    /// The source's `source` table, by id: the last of an id wins
    /// (`JSONSkinLoader.java:312-314`).
    fn source_path(&mut self, src: Option<&str>) -> Result<Option<String>, ImportError> {
        if self.sources.is_none() {
            let recs = object::records(&self.tables, "source").to_vec();
            self.sources = Some(object::index(self.def, &recs, true)?);
        }
        let Some(rec) = src.and_then(|s| self.sources.as_ref().and_then(|m| m.get(s)).copied())
        else {
            return Ok(None);
        };
        let [path] = self.def.fields(rec, ["path"]);
        self.opt_string(path, "path")
    }

    fn finish(mut self, files: Vec<FileChoice>) -> Result<Imported, ImportError> {
        let assets = std::mem::replace(&mut self.assets, Assets::new(self.def.root()));
        let sheet = assets.finish();
        let def = SkinDef {
            fit: Fit::Fixed,
            regions: REGIONS,
            whens: std::mem::take(&mut self.whens),
            sheet,
            lanes: None,
            elements: std::mem::take(&mut self.elements),
            widgets: std::mem::take(&mut self.widgets),
        };
        let skin = Skin::new(def).map_err(|source| {
            let at = self.load_loc(&source);
            ImportError::Model { at, source }
        })?;
        let notes = report::locate(&self.def.arena, &self.notes.list);
        let report = Report {
            sources: self
                .def
                .arena
                .files
                .iter()
                .map(|f| f.name.clone())
                .collect(),
            files,
            notes,
            suppressed: self.notes.suppressed,
            usage: self.budget.usage,
        };
        Ok(Imported { skin, report })
    }

    /// Where a model error points: the element or `when` it names.
    fn load_loc(&self, e: &LoadError) -> Option<Loc> {
        let element = match *e {
            LoadError::VisibleNotBool { element }
            | LoadError::UnknownTimer { element }
            | LoadError::UnknownImage { element }
            | LoadError::RegionInFixed { element }
            | LoadError::FillOutsideScreen { element }
            | LoadError::Coordinates { element }
            | LoadError::WidgetGeometry { element }
            | LoadError::Unbound { element }
            | LoadError::UnknownWidget { element }
            | LoadError::JudgeRegion { element }
            | LoadError::Number { element, .. }
            | LoadError::Gauge { element, .. }
            | LoadError::Judge { element, .. }
            | LoadError::Text { element, .. } => Some(self.element_spans.get(element)?),
            LoadError::WhenNotBool { when } => Some(self.when_spans.get(when)?),
            _ => None,
        };
        element.map(|&s| self.loc(s))
    }
}

impl Common {
    /// Key `k`'s model angle: the source's plus the offsets, negated
    /// for the model, which turns clockwise.
    fn angle_of(&self, k: &SrcKey) -> i64 {
        -(i64::from(k.angle) + self.angle)
    }
}

fn offset_angle(o: &Offset) -> i64 {
    o.r
}

/// The pivot of `center` (`SkinObject.java:80-81, 228-233`): honoured
/// for 0..=9, bottom left otherwise.
fn pivot(center: i32) -> Anchor {
    match center {
        0 | 5 => Anchor::Center,
        1 => Anchor::BottomLeft,
        2 => Anchor::Bottom,
        3 => Anchor::BottomRight,
        4 => Anchor::Left,
        6 => Anchor::Right,
        7 => Anchor::TopLeft,
        8 => Anchor::Top,
        9 => Anchor::TopRight,
        _ => Anchor::BottomLeft,
    }
}

#[cfg(test)]
mod tests;
