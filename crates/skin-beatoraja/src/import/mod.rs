//! The mapping: a resolved definition's images, numbers, text and play
//! objects (lanes, SUDDEN+, LIFT, judge and gauge) to a
//! [`skin_model::SkinDef`], with a [`Report`] of what was changed, dropped
//! or ignored.
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
    Anchor, Blend, CycleDef, Ease, ElementDef, Filter, Fit, Follow, Grade, KeyDef, LanesDef,
    LanesError, LoadError, MAX_GEOMETRY, Native, PLAY, Place, Program, Region, RegionDef, Scope,
    Skin, SkinDef, Source, TimerRef, Track, Ty, WidgetDefs, compile, timer as slot,
};

pub use budget::ImportLimit;
pub use policy::CriticalPart;
pub use report::{
    DeviationId, FileChoice, LaneApprox, Note, NoteFallback, NoteKind, NoteLoc, Object, PartWhy,
    PropertyKind, Report, Rule, Severity, TimerWhy, TrackWhy, Usage,
};

use self::asset::{Assets, Choices};
use self::budget::Budget;
use self::geom::{Offset, Q, Rect};
use self::object::Objects;
use self::ops::Cond;
use self::policy::Role;
use self::report::{Notes, Span};
use self::timer::{Custom, Timer};
use self::track::{Accepted, SrcKey, Stretch};
use crate::error::{ImportError, Loc};
use crate::header::Header;
use crate::resolve::{Definition, Resolved, Tables, resolve};
use crate::tree::{Kind, Scalar};

/// What the user configured for one skin: the item selected in each
/// option, the file chosen for each file choice, and the offsets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// Per option of the header, in its order: the selected item's
    /// number, `None` for none. An option past the end selects none.
    pub options: Vec<Option<i32>>,
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
            .map(|o| {
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

    /// Every selected item's number: what the source tree's conditions
    /// see (`JSONSkinLoader.java:254-260`).
    #[must_use]
    pub fn enabled(&self) -> BTreeSet<i32> {
        self.options.iter().flatten().copied().collect()
    }
}

/// An imported skin and its report.
#[derive(Debug)]
pub struct Imported {
    pub skin: Skin,
    pub report: Report,
    /// The side the skin draws the scratch lane on; `None` without lanes.
    /// The caller plays with it until the player has a setting (owner
    /// decision 11).
    pub scratch: Option<skin_model::Side>,
}

/// Every region anchored top left; a fixed-canvas skin uses only
/// `screen`.
const REGIONS: [RegionDef; Region::COUNT] = [RegionDef {
    anchor: Anchor::TopLeft,
    optional: false,
}; Region::COUNT];

/// The runtime offsets the play sets (`SkinProperty.java:943-947`).
const RUNTIME_OFFSETS: std::ops::RangeInclusive<i32> = 1..=5;
/// The offset that moves with LIFT (`SkinProperty.java:944`).
const OFFSET_LIFT: i32 = 3;
/// The offset SUDDEN+ moves (`SkinProperty.java:947`).
const OFFSET_LANECOVER: i32 = 4;
/// The offset HIDDEN+ moves (`SkinProperty.java:948`).
const OFFSET_HIDDEN: i32 = 5;
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
    let enabled = config.enabled();
    let mut resolved = resolve(def, &enabled)?;
    let tables = resolved.tables(&enabled)?;
    let Resolved { def, size, .. } = resolved;
    let mut files = Vec::new();
    let choices = Choices::new(header, config, &mut files);
    let r = geom::scale(size[0]).ok_or(ImportError::Resolution {
        at: None,
        w: i32::try_from(size[0]).unwrap_or(i32::MAX),
        h: i32::try_from(size[1]).unwrap_or(i32::MAX),
    })?;
    let mut cx = Cx::new(def, header, config, tables, choices, r, size[1])?;
    let destinations = object::records(&cx.tables, "destination").to_vec();
    cx.span_hint = cx.lane_span_hint(&destinations);
    cx.custom.judge_region = cx.judge_region(&destinations);
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

/// How [`Cx::common_with`] reads a destination.
#[derive(Clone, Copy, Default)]
struct Opts<'a> {
    /// A `text`: no angle, the blend only noted.
    text: bool,
    /// Drawn unrotated: the keys' angles are cleared before the track is
    /// accepted, and an angle is only noted.
    unrotated: bool,
    /// Run-time offsets the receiving widget consumes itself (the covers).
    allow: &'a [i32],
    /// Offsets 3 and 4 become [`Follow`]: the element moves with LIFT and
    /// SUDDEN+ (`play/LaneRenderer.java:332-333`).
    follow: bool,
    /// A relative element (a judge's combo): offsets 3 and 4 are taken
    /// and move nothing, as the source skips a relative element's own
    /// translation and attaches it to its grade (`skin/SkinObject.java:
    /// 404-428`, `play/SkinJudge.java:122-125`).
    relative: bool,
    /// A play widget's outer destination: the source's constructor key
    /// at 0 joins the keys, every key is neutral (rect 0, white, opaque),
    /// and blend and colour are not consumed.
    neutral: bool,
    /// The offsets are not consumed: noted, never refused.
    no_offsets: bool,
    /// A judge's combo: its digits, which shift each raw key left
    /// ([`track::read`]).
    digits: Option<i32>,
    /// The keys as written were charged before this read.
    charged: bool,
    /// The keys' clip is unused: noted, never refused.
    unclipped: bool,
    /// A condition the element is drawn under besides its own: a value's
    /// availability ([`property::Value::availability`]), joined first.
    guard: Option<&'static str>,
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
    /// The covers it moves with, from offsets 3 and 4.
    follow: Follow,
    stretch: Stretch,
    stretch_mode: i32,
    untimed: bool,
    /// Whether a condition differs from the source's in an owner-decided
    /// way, or the element's value does: a D4 note when it is retained.
    display_deviation: bool,
}

/// The import's running state.
pub(crate) struct Cx<'d> {
    def: &'d mut Definition,
    header: &'d Header,
    config: &'d Config,
    tables: Tables,
    /// The scale from the skin's pixels to reference pixels.
    r: Q,
    /// The skin's own height in its pixels, which scales a slider's
    /// vertical range.
    height: u32,
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
    /// What a destination's numeric op reads of each declared option
    /// item: whether it is its option's selection, a later option's entry
    /// replacing an earlier one's for a shared number
    /// (`JSONSkinLoader.java:291-297`).
    items: BTreeMap<i32, bool>,
    whens: Vec<Program>,
    when_spans: Vec<Span>,
    ranks: [Option<u16>; 3],
    elements: Vec<ElementDef>,
    element_spans: Vec<Span>,
    widgets: WidgetDefs,
    /// The lanes, once the note destination is mapped.
    lanes: Option<widget::lanes::LaneState>,
    /// The lanes' span, read ahead of the walk for the offset-4 rule.
    span_hint: widget::lanes::SpanHint,
    /// The judge records already drawn.
    judges: Vec<u32>,
    /// Image selectors compiled once, by source; each copy kept is
    /// charged.
    selectors: HashMap<String, Program>,
}

/// A witness of the order of an import's work.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Charge,
    Compile,
    Admit,
    /// An image choice table's entries charged.
    Refs,
    /// An image choice table allocated.
    Table,
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
        height: u32,
    ) -> Result<Self, ImportError> {
        let entry_dir = def.entry_dir().to_owned();
        let assets = Assets::new(def.root());
        let mut items = BTreeMap::new();
        for (i, o) in header.options.iter().enumerate() {
            let selected = config.options.get(i).copied().flatten();
            for item in &o.items {
                items.insert(item.op, Some(item.op) == selected);
            }
        }
        let mut cx = Self {
            def,
            header,
            config,
            tables,
            r,
            height,
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
            items,
            whens: Vec::new(),
            when_spans: Vec::new(),
            ranks: [None; 3],
            elements: Vec::new(),
            element_spans: Vec::new(),
            widgets: WidgetDefs::default(),
            lanes: None,
            span_hint: widget::lanes::SpanHint::NoLanes,
            judges: Vec::new(),
            selectors: HashMap::new(),
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

    fn drop_el(&mut self, role: Role, at: Span, why: NoteKind) -> Result<(), ImportError> {
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

    /// Whether some destination draws a judge, read ahead of the walk: the
    /// source makes the judge region for it whatever its conditions
    /// (`JsonPlaySkinObjectLoader.java:226, 283-287`). A judge other than
    /// index 0 refuses the import, as does a judge id an image, text,
    /// slider, graph or LIFT shares, and a record this cannot read fails
    /// the walk at or before it, so none needs an answer.
    fn judge_region(&mut self, destinations: &[u32]) -> bool {
        for &rec in destinations {
            let Ok(id) = object::id_of(self.def, rec) else {
                return false;
            };
            let Some(id) = id.filter(|id| !id.parse::<i32>().is_ok_and(|n| n < 0)) else {
                continue;
            };
            match self.objects.lookup(self.def, &self.tables, &id) {
                Ok(Some((Object::Judge, _))) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
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
            self.custom.aliases.insert(id, target);
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
        self.read_dst_keys(rec, keys)
    }

    /// A nested destination (a play object's), its keys resolved under
    /// its `dst` field.
    fn read_nested(&mut self, rec: u32) -> Result<(Option<String>, Dst), ImportError> {
        let keys = match self.def.fields(rec, ["dst"]) {
            [Some(f)] => self.tables.nested.get(&f).cloned().unwrap_or_default(),
            [None] => Vec::new(),
        };
        self.read_dst_keys(rec, keys)
    }

    /// A destination record's fields with `keys` as its `dst`.
    fn read_dst_keys(
        &mut self,
        rec: u32,
        keys: Vec<u32>,
    ) -> Result<(Option<String>, Dst), ImportError> {
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
        if id.parse::<i32>().is_ok_and(|n| n < 0) {
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
        // The source draws the character when its file exists and the
        // notes otherwise: one id cannot be both.
        if object == Object::PmChara
            && self
                .objects
                .names(self.def, &self.tables, &id, Object::Note)?
        {
            return Err(ImportError::Critical {
                at: self.loc(at),
                part: CriticalPart::Lanes,
                why: NoteKind::UnsupportedField { field: "id" },
            });
        }
        // Likewise the source falls through to the judge, and makes its
        // region, when an image's or LIFT's file or a text's font does not
        // load or a slider or graph has no texture
        // (`JsonSkinObjectLoader.java:71, 387-391, 396-416, 420-453, 628`,
        // `JsonPlaySkinObjectLoader.java:31, 200-213, 283`): one id cannot
        // be both.
        if matches!(
            object,
            Object::Image | Object::Text | Object::Slider | Object::Graph | Object::LiftCover
        ) && self
            .objects
            .names(self.def, &self.tables, &id, Object::Judge)?
        {
            return Err(ImportError::Critical {
                at: self.loc(at),
                part: CriticalPart::Judge,
                why: NoteKind::UnsupportedField { field: "id" },
            });
        }
        match object {
            Object::Image => self.image(dst, orec)?,
            Object::ImageSet => self.image_set(dst, orec)?,
            Object::Value => self.number(dst, orec)?,
            Object::Text => self.text(dst, orec)?,
            Object::Note => self.lanes_element(dst, orec)?,
            Object::HiddenCover => self.hidden_cover(dst, orec)?,
            Object::LiftCover => self.lift_cover(dst, orec)?,
            Object::Judge => self.judge(dst, orec)?,
            Object::Gauge => self.gauge(dst, orec)?,
            Object::Slider => self.slider(dst, orec)?,
            _ => {
                self.drop_el(Role::Decorative, at, NoteKind::Unsupported { object })?;
            }
        }
        Ok(())
    }

    /// The common part of an element, read as `o` says: track,
    /// conditions, timer, offsets, blend and pivot, each through the role
    /// policy.
    /// `None` when the element was dropped or is never drawn.
    ///
    /// A `text` consumes neither the angle nor the blend
    /// (`SkinTextFont.java:158-176`): its keys' angles are cleared before
    /// the track is accepted, and an angle or a blend is only noted
    /// (`Opts::text`).
    fn common_with(
        &mut self,
        role: Role,
        dst: &Dst,
        o: Opts<'_>,
    ) -> Result<Option<Common>, ImportError> {
        let text = o.text;
        let at = dst.span;
        // The source draws it only while the mouse is inside its rect
        // (`SkinObject.java:603-607`); the play has no mouse.
        if dst.mouse {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "mouseRect" })?;
            return Ok(None);
        }
        // Keys: charged as written before they are read, then as emitted
        // when more (a neutral track emits its first and last).
        let written = dst.keys.len();
        let fits = if o.charged {
            written <= skin_model::MAX_TRACK_KEYS
        } else {
            self.budget.keys(written).map_err(|l| self.limit(at, l))?
        };
        let mut keys = track::read(self.def, &dst.keys, o.digits)?;
        if o.neutral {
            // The emitted keys, at 0 and at the last time, charged before
            // the constructor's key joins.
            let last = keys.keys.iter().map(|k| k.at).max().unwrap_or(0);
            let emitted = 1 + usize::from(last > 0);
            if emitted > written {
                self.budget
                    .keys(emitted - written)
                    .map_err(|l| self.limit(at, l))?;
            }
            // The constructor's key at 0 (`SkinObject.java:234-254`), then
            // the destination's times; values are not consumed.
            keys.keys.push(track::SrcKey::NEUTRAL);
            for k in &mut keys.keys {
                *k = track::SrcKey {
                    at: k.at,
                    ..track::SrcKey::NEUTRAL
                };
            }
            if keys.clip {
                self.ignored(at, NoteKind::Ignored { field: "clip" });
                keys.clip = false;
            }
        }
        let mut angle = false;
        if text || o.unrotated {
            for k in &mut keys.keys {
                angle |= k.angle != 0;
                k.angle = 0;
            }
        }
        if keys.clip && o.unclipped {
            self.ignored(at, NoteKind::Ignored { field: "clip" });
            keys.clip = false;
        }
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

        // Conditions, after the value's availability.
        let mut conds: Vec<String> = o.guard.into_iter().map(str::to_owned).collect();
        let mut deviates = false;
        let mut seen = BTreeSet::new();
        for op in &dst.ops {
            let cond = match op {
                Op::Id(0) => continue,
                Op::Id(n) => {
                    if !seen.insert(*n) {
                        continue;
                    }
                    ops::numeric(*n, &self.items, self.custom.judge_region)
                }
                Op::Name(s) => match ops::named(s, self.custom.judge_region) {
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
            if !self.condition(role, at, cond, &mut conds, &mut deviates)? {
                return Ok(None);
            }
        }
        match &dst.draw {
            Prop::Null => {}
            Prop::Id(n) => {
                if ops::is_builtin(i32::try_from(n.unsigned_abs()).unwrap_or(0)) {
                    let c = ops::numeric(*n, &BTreeMap::new(), self.custom.judge_region);
                    if !self.condition(role, at, c, &mut conds, &mut deviates)? {
                        return Ok(None);
                    }
                } else {
                    self.ignored(at, NoteKind::Ignored { field: "draw" });
                }
            }
            Prop::Name(s) => {
                let Some(c) = ops::named(s, self.custom.judge_region) else {
                    let why = NoteKind::UnsupportedProperty {
                        kind: PropertyKind::Boolean,
                        id: None,
                    };
                    self.drop_el(role, at, why)?;
                    return Ok(None);
                };
                if !self.condition(role, at, c, &mut conds, &mut deviates)? {
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

        // Offsets: each valid id once, whether listed as `offset` or among
        // `offsets` (`skin/json/JsonSkinLoader.java:473-478`,
        // `skin/SkinObject.java:832-844`).
        let mut ids: Vec<i32> = Vec::new();
        for &id in &dst.offsets {
            if (1..=OFFSET_MAX).contains(&id) && !ids.contains(&id) {
                ids.push(id);
            }
        }
        let mut offset = Offset::default();
        let mut alpha: Vec<i64> = Vec::new();
        let (mut lift, mut cover) = (false, false);
        if o.no_offsets && !ids.is_empty() {
            self.ignored(at, NoteKind::Ignored { field: "offset" });
            ids.clear();
        }
        // A value for an offset the header does not declare is ignored
        // (`config_notes`).
        let value = |cx: &Self, id: i32| {
            let declared = cx.header.offsets.iter().any(|o| o.id == id);
            cx.config.offsets.get(&id).copied().filter(|_| declared)
        };
        // The play sets the y of the offsets the covers move by
        // (`play/LaneRenderer.java:332-345`), so a configured y is
        // overridden; their other components, which the source adds too
        // (`skin/SkinObject.java:404-428`), are not reproduced. That holds
        // for the ids a cover consumes, listed or added by the source, and
        // for 3 and 4 on a follower or a relative part.
        let reserved = |id: i32| {
            o.allow.contains(&id)
                || (matches!(id, OFFSET_LIFT | OFFSET_LANECOVER) && (o.follow || o.relative))
        };
        let moved = o.allow.iter().chain(&ids).copied().find(|&id| {
            reserved(id)
                && value(self, id)
                    .is_some_and(|v| v.x != 0 || v.w != 0 || v.h != 0 || v.r != 0 || v.a != 0)
        });
        if let Some(id) = moved {
            self.drop_el(role, at, NoteKind::RuntimeOffset(id))?;
            return Ok(None);
        }
        for id in ids {
            if o.allow.contains(&id) {
                continue;
            }
            if reserved(id) {
                if o.follow {
                    lift |= id == OFFSET_LIFT;
                    cover |= id == OFFSET_LANECOVER;
                }
                continue;
            }
            if RUNTIME_OFFSETS.contains(&id) {
                self.drop_el(role, at, NoteKind::RuntimeOffset(id))?;
                return Ok(None);
            }
            if id == OFFSET_ALL {
                continue;
            }
            let Some(v) = value(self, id) else {
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
        if o.neutral && !alpha.is_empty() {
            self.ignored(
                at,
                NoteKind::Ignored {
                    field: "offset alpha",
                },
            );
            alpha.clear();
        }
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

        // Blend (`Skin.java:654-665`). A neutral outer destination draws
        // nothing of its own: its parts keep their blend, so its own is
        // ignored.
        let blend = match dst.blend {
            2..=4 | 9 if o.neutral => {
                self.ignored(at, NoteKind::Ignored { field: "blend" });
                Blend::Alpha
            }
            2 => Blend::Add,
            3 => {
                self.ignored(at, NoteKind::Ignored { field: "blend 3" });
                Blend::Add
            }
            4 | 9 if text => {
                self.ignored(at, NoteKind::Ignored { field: "blend" });
                Blend::Alpha
            }
            4 | 9 => {
                self.drop_el(role, at, NoteKind::Blend(dst.blend))?;
                return Ok(None);
            }
            _ => Blend::Alpha,
        };

        // Visibility, compiled after its charges.
        let mut src = String::new();
        for c in &conds {
            if !src.is_empty() {
                src.push_str(" && ");
            }
            src.push('(');
            src.push_str(c);
            src.push(')');
        }
        let visible = if conds.is_empty() {
            None
        } else {
            Some(self.compile(&src, Ty::Bool, at)?)
        };

        if text || o.unrotated {
            if angle || offset.r != 0 {
                self.ignored(at, NoteKind::Ignored { field: "angle" });
            }
            offset.r = 0;
        }
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
            follow: match (lift, cover) {
                (false, false) => Follow::None,
                (true, false) => Follow::Lift,
                (false, true) => Follow::Cover,
                (true, true) => Follow::LiftAndCover,
            },
            stretch: Stretch::of(dst.stretch),
            stretch_mode: dst.stretch,
            untimed,
            track,
            display_deviation: deviates,
        }))
    }

    /// Applies one condition, setting `deviates` for a D4 one: `Ok(false)`
    /// when the element is dropped or never drawn. A repeated expression
    /// adds nothing: the supported conditions are a closed set, so a long
    /// list builds no more; one destination listing more of them than one
    /// source holds is refused at its location (`ops`' tests).
    fn condition(
        &mut self,
        role: Role,
        at: Span,
        c: Cond,
        conds: &mut Vec<String>,
        deviates: &mut bool,
    ) -> Result<bool, ImportError> {
        match c {
            Cond::Expr {
                src,
                display_deviation,
            } => {
                *deviates |= display_deviation;
                if !conds.contains(&src) {
                    conds.push(src);
                }
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
    fn push(&mut self, c: Common, source: Source, track: Track) -> Result<(), ImportError> {
        self.push_native(c, source, track, None)
    }

    /// [`Self::push`] with the native size its quads keep. An element
    /// that follows the covers is noted as D9 [`LaneApprox::CoverMotion`].
    fn push_native(
        &mut self,
        c: Common,
        source: Source,
        track: Track,
        native: Option<Native>,
    ) -> Result<(), ImportError> {
        self.retained(&c)?;
        self.elements.push(ElementDef {
            source,
            track,
            timer: c.timer,
            visible: c.visible,
            blend: c.blend,
            clip: None,
            place: Place {
                pivot: c.pivot,
                native,
                follow: c.follow,
                ..Place::SCREEN
            },
        });
        self.element_spans.push(c.span);
        Ok(())
    }

    /// Notes what a retained element or judge part carries: D4 once when
    /// it deviates ([`Common::display_deviation`]), and D9
    /// [`LaneApprox::CoverMotion`] once when it follows the covers.
    fn retained(&mut self, c: &Common) -> Result<(), ImportError> {
        if c.display_deviation {
            self.note(
                Some(c.span),
                Severity::Deviation,
                NoteKind::Deviation(DeviationId::D4),
            );
        }
        self.cover_motion(c)
    }

    /// Notes D9 [`LaneApprox::CoverMotion`] once for a retained element
    /// or grade that follows the covers.
    fn cover_motion(&mut self, c: &Common) -> Result<(), ImportError> {
        if c.follow == Follow::None {
            return Ok(());
        }
        let part = match c.role {
            Role::Critical(part) => part,
            Role::Decorative => CriticalPart::Cover,
        };
        self.lane_approx(c.span, part, LaneApprox::CoverMotion)
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

    /// The grades that give a bomb, from the top-level `judgetimer`
    /// (`JsonSkin.java:20`, default 1): a judgment at or below it, PERFECT
    /// 0 to empty POOR 5 (`play/JudgeManager.java:673`). Its whole domain
    /// maps exactly: below 0 none, above 5 every grade.
    fn bomb_up_to(&mut self) -> Result<Option<Grade>, ImportError> {
        let [f] = self.def.fields(self.def.top(), ["judgetimer"]);
        let n = self.int_or(f, "judgetimer", 1)?;
        Ok(usize::try_from(n)
            .ok()
            .map(|i| Grade::ALL.get(i).copied().unwrap_or(Grade::EmptyPoor)))
    }

    fn finish(mut self, files: Vec<FileChoice>) -> Result<Imported, ImportError> {
        let bomb_up_to = self.bomb_up_to()?;
        let assets = std::mem::replace(&mut self.assets, Assets::new(self.def.root()));
        let sheet = assets.finish();
        // The lanes' groups move to the skin; the rest is copied.
        let lanes = self.lanes.as_mut().map(|l| {
            let bar_groups = std::mem::take(&mut l.def.bar_groups);
            LanesDef {
                bar_groups,
                ..l.def.clone()
            }
        });
        let def = SkinDef {
            fit: Fit::Fixed,
            regions: REGIONS,
            whens: std::mem::take(&mut self.whens),
            sheet,
            lanes,
            elements: std::mem::take(&mut self.elements),
            widgets: std::mem::take(&mut self.widgets),
            bomb_up_to,
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
        Ok(Imported {
            skin,
            report,
            scratch: self.lanes.as_ref().map(|l| l.def.scratch),
        })
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
            | LoadError::CoverGate { element }
            | LoadError::WidgetGeometry { element }
            | LoadError::Unbound { element }
            | LoadError::UnknownWidget { element }
            | LoadError::JudgeRegion { element }
            | LoadError::Number { element, .. }
            | LoadError::Gauge { element, .. }
            | LoadError::Judge { element, .. }
            | LoadError::Text { element, .. } => Some(self.element_spans.get(element)?),
            LoadError::WhenNotBool { when } => Some(self.when_spans.get(when)?),
            LoadError::Lanes(LanesError::BarGroup { group, .. }) => {
                Some(self.lanes.as_ref()?.bar_spans.get(group)?)
            }
            LoadError::Lanes(_) | LoadError::LanesTwice | LoadError::LanesRegion => {
                Some(&self.lanes.as_ref()?.span)
            }
            _ => None,
        };
        element.map(|&s| self.loc(s))
    }
}

impl Common {
    /// Key `k`'s model angle: the source's plus the offsets, negated
    /// for the model, which turns clockwise.
    fn angle_of(&self, k: &SrcKey) -> i64 {
        -(i64::from(k.angle) + self.offset.r)
    }
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
