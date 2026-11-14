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

pub(crate) mod asset;
mod budget;
pub(crate) mod callback;
pub(crate) mod decision;
pub(crate) mod geom;
#[cfg(feature = "dev-tools")]
pub(crate) mod ledger;
mod object;
pub(crate) mod ops;
mod policy;
mod prepare;
mod property;
#[cfg(feature = "dev-tools")]
pub(crate) use property::{IndexProp, IntProp, index_in, int_in, int_named, judge_count};
mod report;
pub(crate) mod result;
pub(crate) mod selection;
pub(crate) mod timer;
mod track;
mod widget;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bms_model::TimeUs;
use skin_model::{
    ActionDef, Anchor, AngleFollow, Blend, CycleDef, DecisionDefs, DecisionTiming, Ease,
    ElementDef, Filter, Fit, Follow, Grade, HoverRect, KeyDef, LanesDef, LanesError, LoadError,
    MAX_GEOMETRY, Native, Place, PlayDefs, PlayEnding, PointerDef, Program, Region, RegionDef,
    ResultDefs, ResultTiming, Scope, ScreenKind, SelectionDefs, Skin, SkinDef, Source, TimerRef,
    Track, Ty, WidgetDefs, compile, timer as slot,
};

pub use budget::ImportLimit;
pub use policy::{CriticalPart, EssentialPart};
pub(crate) use property::{int_by_name, str_by_name};
pub use report::{
    DeviationId, FileChoice, LaneApprox, Note, NoteFallback, NoteKind, NoteLoc, Object, PartWhy,
    PropertyKind, Report, Rule, ScheduleWhy, Severity, TimerWhy, TrackWhy, Usage,
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
use crate::tree::Scalar;

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

    /// The configuration that names options and file choices against
    /// `header`: each named option's item and each named file choice's
    /// candidate, the rest the header's defaults ([`Self::defaults`]); no
    /// offsets.
    ///
    /// # Errors
    ///
    /// The first name the header does not have, named.
    pub fn named<'a>(
        header: &Header,
        options: impl IntoIterator<Item = (&'a str, &'a str)>,
        files: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, String> {
        let mut config = Self::defaults(header);
        for (option, item) in options {
            let Some((i, spec)) = header
                .options
                .iter()
                .enumerate()
                .find(|(_, o)| o.name.as_deref() == Some(option))
            else {
                return Err(format!("option \"{option}\" is not in the skin's header"));
            };
            let Some(chosen) = spec
                .items
                .iter()
                .find(|it| it.name.as_deref() == Some(item))
            else {
                return Err(format!("option \"{option}\" has no item \"{item}\""));
            };
            if let Some(slot) = config.options.get_mut(i) {
                *slot = Some(chosen.op);
            }
        }
        for (file, candidate) in files {
            let Some(spec) = header
                .files
                .iter()
                .find(|f| f.name.as_deref() == Some(file))
            else {
                return Err(format!(
                    "file choice \"{file}\" is not in the skin's header"
                ));
            };
            if !spec.candidates.iter().any(|c| c == candidate) {
                return Err(format!(
                    "file choice \"{file}\" has no candidate \"{candidate}\""
                ));
            }
            config.files.insert(file.to_owned(), candidate.to_owned());
        }
        Ok(config)
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
/// The offset the 1P turntable's angle sets
/// (`play/KeyInputProccessor.java:92-108`).
const OFFSET_TURNTABLE: i32 = 1;
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

/// Imports `def`, whose header `header` was read from it, with `config`
/// into the `slot` kind's slot. A header of another kind than `slot`
/// refuses before anything is resolved or read; selection, decision and
/// result map their ids through their profiles ([`selection`],
/// [`decision`], [`result`]).
///
/// # Errors
///
/// A skin of another kind than `slot`, by its header or by the `type` its
/// resolution names ([`ImportError::Slot`]); what resolution refuses; an importer
/// budget or aggregate asset limit ([`ImportError::Limit`]); geometry
/// outside the model ([`ImportError::Range`]); a play-critical element
/// the importer cannot reproduce ([`ImportError::Critical`],
/// [`ImportError::Asset`]); a selection skin's essential part missing or
/// not reproduced ([`ImportError::Essential`]); a skin the model refuses
/// ([`ImportError::Model`]).
pub fn import(
    def: &mut Definition,
    header: &Header,
    config: &Config,
    slot: ScreenKind,
) -> Result<Imported, ImportError> {
    if header.kind != slot {
        return Err(ImportError::Slot {
            at: None,
            slot,
            found: header.kind,
        });
    }
    let enabled = config.enabled();
    let mut resolved = resolve(def, &enabled)?;
    if resolved.kind != slot {
        return Err(ImportError::Slot {
            at: Some(resolved.def.loc(resolved.kind_node)),
            slot,
            found: resolved.kind,
        });
    }
    let tables = resolved.tables(&enabled)?;
    #[cfg(feature = "dev-tools")]
    let mut ledger = ledger::Recorder::new(object::records(&tables, "destination").len());
    let Resolved { def, size, .. } = resolved;
    let mut files = Vec::new();
    let choices = Choices::new(header, config, &mut files);
    let r = geom::scale(size[0]).ok_or(ImportError::Resolution {
        at: None,
        w: i32::try_from(size[0]).unwrap_or(i32::MAX),
        h: i32::try_from(size[1]).unwrap_or(i32::MAX),
    })?;
    let mut cx = Cx::new(def, header, config, tables, choices, (r, size[1]), slot)?;
    let destinations = object::records(&cx.tables, "destination").to_vec();
    match slot {
        ScreenKind::Play => cx.span_hint = cx.lane_span_hint(&destinations),
        ScreenKind::Selection => cx.selection_tables()?,
        ScreenKind::Decision | ScreenKind::Result => {}
    }
    for (index, rec) in destinations.into_iter().enumerate() {
        #[cfg(feature = "dev-tools")]
        ledger.begin(&mut cx);
        let read = cx.destination(index, rec);
        #[cfg(feature = "dev-tools")]
        ledger.end(&mut cx, index, read.is_err());
        read?;
    }
    #[cfg(feature = "dev-tools")]
    ledger.publish(&mut cx);
    #[cfg(feature = "dev-tools")]
    cx.record_unreached_sources();
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
    /// Its `mouseRect` object, read only where a hover rect is kept
    /// ([`Cx::hover`]).
    mouse: Option<u32>,
    /// Whether it is a nested destination: a song list part's on a
    /// selection skin, which keeps no hover rect.
    nested: bool,
    /// The action of the image or image set it draws, on a selection
    /// skin, set before its element is built.
    action: Option<ActionDef>,
    /// Whether it is failure presentation ([`Cx::failure`]).
    failure: bool,
}

/// A property field as the source's script serializer reads it
/// (`JsonSkinSerializer.java:341-371`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Prop {
    /// Absent, null, or neither a number nor a string.
    Null,
    Id(i32),
    Name(String),
    /// A Lua function, by its node, where the field takes one
    /// ([`Cx::callback_prop`]).
    Callable(u32),
}

#[cfg(test)]
thread_local! {
    /// Each element's compiled visibility this thread imported, in order.
    pub(crate) static VISIBLE: std::cell::RefCell<Vec<Option<Program>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A destination option (`JsonSkinSerializer.java:115-131`).
#[derive(Clone, Debug)]
enum Op {
    Id(i32),
    Name(String),
    /// A Lua function, by its node ([`Cx::callback_prop`]).
    Callable(u32),
}

/// Whether a reading of a destination's keys ([`Cx::read_keys`]) draws
/// their colour: a probe, or a track whose colour is not consumed, does
/// not, and the reading that draws it reports a clamp.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Colour {
    Drawn,
    Unused,
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
    /// Offsets 3, 4 and 5 become [`Follow`]: the element moves with LIFT,
    /// SUDDEN+ and HIDDEN+, and is not drawn while HIDDEN+ is off
    /// (`play/LaneRenderer.java:332-346`).
    follow: bool,
    /// Offset 1 becomes [`AngleFollow::Turntable`]: the element turns
    /// with the turntable (`skin/SkinObject.java:526-543`). A text accepts
    /// it and stays unrotated, as the source draws text
    /// (`skin/SkinTextImage.java:122`).
    turntable: bool,
    /// Blend 4 maps to [`Blend::Multiply`]: an image, an image choice or
    /// a player-drawn image drawn as an element of its own. Elsewhere it
    /// drops as an unsupported blend.
    multiply: bool,
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
    /// A judge's combo: its digits; each key gives `x` and `w` together
    /// or neither ([`track::read`]).
    digits: Option<i32>,
    /// The keys as written were charged before this read.
    charged: bool,
    /// The keys' clip is unused: noted, never refused.
    unclipped: bool,
    /// A condition the element is drawn under besides its own: a value's
    /// availability ([`property::Value::availability`]), joined first.
    guard: Option<&'a str>,
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
    /// The covers it moves with, from offsets 3, 4 and 5.
    follow: Follow,
    /// Whether it turns with the turntable (offset 1): an image, an image
    /// choice or a number only.
    turntable: bool,
    stretch: Stretch,
    stretch_mode: i32,
    untimed: bool,
    /// Whether a condition differs from the source's in an owner-decided
    /// way, or the element's value does: a D4 note when it is retained.
    display_deviation: bool,
    /// On a result skin, whether its visibility tests the clear status,
    /// an essential part ([`result`]).
    tests_status: bool,
    /// Its conditions' sources, in the source's order, as `visible` joins
    /// them: what a value read after them is gated by.
    conds: Vec<String>,
    /// On a selection skin: its hover rect ([`Cx::hover`]), its action
    /// (an image's or an image set's) and whether it is the search box
    /// (text 30's).
    hover: Option<HoverRect>,
    action: Option<ActionDef>,
    search: bool,
}

/// The import's running state.
pub(crate) struct Cx<'d> {
    def: &'d mut Definition,
    header: &'d Header,
    config: &'d Config,
    /// The screen the skin is imported for: its profile reads every id.
    kind: ScreenKind,
    /// The y the source's y-up rects are reflected about: the reference
    /// height for an element, 0 for a song list part, whose keys are
    /// offsets from its placement's bottom-left corner.
    flip: Q,
    /// The selection widgets, on a selection skin.
    sel: selection::State,
    /// The essential parts retained, on a result skin.
    res: result::State,
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
    /// The play presentation program, its callbacks in the order the
    /// import consumes them.
    present: skin_model::present::PresentDef,
    /// The program's slot roots, by instance: their slots and the timer
    /// callbacks compiled for them.
    slot_roots: HashMap<u32, callback::SlotRoot>,
    /// Per callable instance, the play destinations whose `timer` it is.
    timer_sites: HashMap<u32, u32>,
    /// Per callable instance, the `timer`s of play destinations whose
    /// object the source removes, or whose timer is inert: no site, and
    /// no reference ([`Cx::close_slots`]).
    removed_refs: HashMap<u32, usize>,
    /// The play destination being read's installation ([`prepare`]).
    install: prepare::Install,
    /// What its kind's `prepare` checks before its conditions.
    prepare: prepare::Prepare,
    /// Whether a read of it so far is one the source sees as a
    /// pre-judgment sentinel ([`property::Value::pre_judgment`]).
    sentinel_read: bool,
    /// The schedule notes, by callback, held until the slot roots close
    /// ([`Cx::schedule_notes`]).
    schedule: Vec<(u16, Span, NoteKind)>,
    /// Each folder a source pattern lists, by name: its entries' names and
    /// whether the listing skipped one, or `None` when it could not be
    /// read; listed and charged once.
    listings: HashMap<String, Option<(Vec<String>, bool)>>,
    /// While a number's destination is read, its value's expression: a
    /// program draw after it runs only while the value is no sentinel.
    number_value: Option<String>,
    /// Where the last lowering's capture proof was voided by a frame-time
    /// write: its refusal's place.
    interference: std::cell::Cell<Option<crate::lua::callback::Site>>,
    /// The output types of the callbacks a selection or result screen's
    /// runtime runs, by index: `lua.<k>` in the expressions.
    outputs: Vec<skin_model::output::OutTy>,
    /// The current element's runtime value, which the source reads before
    /// its draws and which gates them: a number's draws run only when its
    /// value is not a sentinel (`SkinNumber.java:138-150`), an image set's
    /// only when its value is not negative (`SkinImage.java:127-132`).
    /// Cleared after each element.
    value_gate: Option<String>,
    ranks: [Option<u16>; timer::WHEN_TIMERS],
    elements: Vec<ElementDef>,
    element_spans: Vec<Span>,
    widgets: WidgetDefs,
    /// A result skin's judge graphs or a play skin's notes graphs, by
    /// [`Source::JudgeGraph`] index.
    judge_graphs: Vec<skin_model::JudgeGraphDef>,
    /// A play skin's BPM graphs, by [`Source::BpmGraph`] index.
    bpm_graphs: Vec<skin_model::BpmGraphDef>,
    /// A play skin's timing visualizers, by [`Source::TimingVisualizer`]
    /// index.
    timing_visualizers: Vec<skin_model::TimingVisualizerDef>,
    /// A play skin's hit-error visualizers, by
    /// [`Source::HitErrorVisualizer`] index.
    hit_errors: Vec<skin_model::HitErrorDef>,
    /// A result skin's gauge graphs, by [`Source::GaugeGraph`] index.
    gauge_graphs: Vec<skin_model::GaugeGraphDef>,
    /// A result skin's timing distributions, by [`Source::TimingGraph`]
    /// index.
    timing_graphs: Vec<skin_model::TimingGraphDef>,
    /// The lanes, once the note destination is mapped.
    lanes: Option<widget::lanes::LaneState>,
    /// The lanes' span, read ahead of the walk for the offset-4 rule.
    span_hint: widget::lanes::SpanHint,
    /// The judge records already drawn.
    judges: Vec<u32>,
    /// The destinations keeping an image whose cycle has no timer.
    cycled: BTreeSet<Span>,
    /// Image selectors compiled once, by source; each copy kept is
    /// charged.
    selectors: HashMap<String, Program>,
    /// The Lua scalar `op` values already noted ignored: a destination
    /// read twice (the lanes read ahead, a selected row) notes once.
    ignored_ops: BTreeSet<Span>,
    /// The destination being read, for the conversion report's records.
    #[cfg(feature = "dev-tools")]
    element: Option<crate::lua::survey::Element>,
    /// The nested part being read, by its object path under the
    /// destination's definition (`def.images.2`; empty for the
    /// destination itself), for the replay's occurrence records.
    #[cfg(feature = "dev-tools")]
    part_path: String,
    /// Each `op` entry node's position in its list, for the replay's
    /// occurrence records.
    #[cfg(feature = "dev-tools")]
    op_positions: HashMap<u32, u32>,
    /// A condition of the destination being read folded false, for the
    /// conversion report's ledger.
    #[cfg(feature = "dev-tools")]
    folded: bool,
    /// The notes as the ledger located them, for the report if none follow.
    #[cfg(feature = "dev-tools")]
    located: Option<Vec<Note>>,
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
        (r, height): (Q, u32),
        kind: ScreenKind,
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
        #[cfg(feature = "dev-tools")]
        crate::lua::replay::record_items(&items);
        let mut cx = Self {
            def,
            header,
            config,
            kind,
            flip: Q::int(geom::HEIGHT),
            sel: selection::State::default(),
            res: result::State::default(),
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
            outputs: Vec::new(),
            value_gate: None,
            when_spans: Vec::new(),
            present: skin_model::present::PresentDef::default(),
            slot_roots: HashMap::new(),
            timer_sites: HashMap::new(),
            removed_refs: HashMap::new(),
            install: prepare::Install::default(),
            prepare: prepare::Prepare::Open,
            sentinel_read: false,
            schedule: Vec::new(),
            listings: HashMap::new(),
            number_value: None,
            interference: std::cell::Cell::new(None),
            ranks: [None; timer::WHEN_TIMERS],
            elements: Vec::new(),
            element_spans: Vec::new(),
            widgets: WidgetDefs::default(),
            judge_graphs: Vec::new(),
            bpm_graphs: Vec::new(),
            timing_visualizers: Vec::new(),
            hit_errors: Vec::new(),
            gauge_graphs: Vec::new(),
            timing_graphs: Vec::new(),
            lanes: None,
            span_hint: widget::lanes::SpanHint::NoLanes,
            judges: Vec::new(),
            cycled: BTreeSet::new(),
            selectors: HashMap::new(),
            ignored_ops: BTreeSet::new(),
            #[cfg(feature = "dev-tools")]
            element: None,
            #[cfg(feature = "dev-tools")]
            part_path: String::new(),
            #[cfg(feature = "dev-tools")]
            op_positions: HashMap::new(),
            #[cfg(feature = "dev-tools")]
            folded: false,
            #[cfg(feature = "dev-tools")]
            located: None,
        };
        cx.budget.usage.resolved_digit_offsets =
            u32::try_from(cx.tables.digit_offsets.len()).unwrap_or(u32::MAX);
        // A Lua table's entries under keys other than strings are never
        // read, as the source's Lua loader ignores them
        // (`LuaSkinLoader.java:187-205` at `0.8.9`): noted at each table,
        // in conversion order (a node's id). Only a Lua table is mixed.
        let nodes = u32::try_from(cx.def.arena.nodes.len()).unwrap_or(u32::MAX);
        for node in 0..nodes {
            if !cx.def.arena.is_mixed(node) {
                continue;
            }
            let at = cx.arena_span(node);
            cx.ignored(
                at,
                NoteKind::Ignored {
                    field: "non-string keys",
                },
            );
        }
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

    /// What stops the import when an asset request at `at` stopped: a
    /// limit, or the candidate's cancellation.
    fn stopped(&self, at: Span, stop: asset::Stop) -> ImportError {
        match stop {
            asset::Stop::Limit(l) => self.limit(at, l),
            asset::Stop::Cancelled => Definition::cancellation(Some(self.loc(at))),
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
            // A function has no target the import follows: an unknown
            // custom timer, never consumed.
            let t = t.filter(|&f| self.callable_at(f).is_none());
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
        self.custom.classify();
        Ok(())
    }

    /// A property field as the script serializer reads it.
    fn prop(&mut self, field: Option<u32>, name: &'static str) -> Result<Prop, ImportError> {
        let Some(f) = field else {
            return Ok(Prop::Null);
        };
        Ok(match self.def.arena.scalar(f) {
            Scalar::Long(_) | Scalar::Double(_) | Scalar::Int(_) | Scalar::Float(_) => {
                Prop::Id(self.def.int(f, name)?)
            }
            Scalar::Str { .. } => self.def.string(f, name)?.map_or(Prop::Null, Prop::Name),
            Scalar::Callable => return Err(self.def.callable_error(f, name)),
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
        if !self.def.arena.is_array(f) {
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
        let (id, dst) = self.read_dst_keys(rec, keys)?;
        Ok((
            id,
            Dst {
                nested: true,
                ..dst
            },
        ))
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
        let ops = self.ops(op)?;
        if let Some(op) = op
            && self.def.arena.is_lua_scalar(op)
        {
            let at = self.arena_span(op);
            if self.ignored_ops.insert(at) {
                self.ignored(at, NoteKind::ScalarOp);
            }
        }
        let blend = self.int_or(blend, "blend", 0)?;
        let filter = self.int_or(filter, "filter", 0)?;
        let timer = self.callback_prop(timer, "timer")?;
        let looped = self.int_or(looped, "loop", 0)?;
        let center = self.int_or(center, "center", 0)?;
        let stretch = self.int_or(stretch, "stretch", -1)?;
        let draw = self.callback_prop(draw, "draw")?;
        let failure = self.failure(&timer, &ops);
        let dst = Dst {
            span: self.arena_span(rec),
            blend,
            filter,
            timer,
            looped,
            center,
            offsets: ids,
            stretch,
            ops,
            draw,
            keys,
            mouse: mouse.filter(|&m| self.def.arena.is_object(m)),
            nested: false,
            action: None,
            failure,
        };
        Ok((id, dst))
    }

    /// A destination's `op` items: numbers, names and Lua functions,
    /// `null` items skipped. An `op` of a Lua skin's converted table that
    /// is not a table (a number, string, boolean or function) is no
    /// condition, as the source's Lua loader reads any array field
    /// (`skin/lua/LuaSkinLoader.java:171-186` at `0.8.9`); any other
    /// non-array, a Lua table with string keys or a JSON value (an
    /// included file's too), fails the skin as before: the source reads
    /// such a table's entries, and its JSON loader fails
    /// (`skin/json/JSONSkinLoader.java:109`).
    // Source: SF-001
    fn ops(&mut self, op: Option<u32>) -> Result<Vec<Op>, ImportError> {
        let mut ops = Vec::new();
        if let Some(op) = op {
            if !self.def.arena.is_array(op) {
                if !self.def.arena.is_lua_scalar(op) {
                    return Err(self.def.type_error(op, "op", "an array"));
                }
                return Ok(ops);
            }
            let items: Vec<u32> = self.def.arena.children(op).collect();
            #[cfg(feature = "dev-tools")]
            for (i, &item) in items.iter().enumerate() {
                self.op_positions
                    .insert(item, u32::try_from(i).unwrap_or(u32::MAX));
            }
            for item in items {
                match self.callback_prop(Some(item), "op")? {
                    Prop::Id(n) => ops.push(Op::Id(n)),
                    Prop::Name(s) => ops.push(Op::Name(s)),
                    Prop::Callable(id) => ops.push(Op::Callable(id)),
                    Prop::Null => {}
                }
            }
        }
        Ok(ops)
    }

    /// Whether destination record `rec` is failure presentation
    /// ([`Self::failure`]), read from its `timer`, `op` and `draw` alone,
    /// before anything else of it is read or charged.
    fn record_failure(&mut self, rec: u32) -> Result<bool, ImportError> {
        let [timer, op, draw] = self.def.fields(rec, ["timer", "op", "draw"]);
        let timer = self.callback_prop(timer, "timer")?;
        if !self.failure_timed(&timer) {
            return Ok(false);
        }
        let ops = self.ops(op)?;
        // Read for its errors as any destination's `draw` is; it never
        // folds ([`Self::can_draw`]).
        self.callback_prop(draw, "draw")?;
        Ok(self.failure(&timer, &ops))
    }

    /// Whether a destination of `timer` and `ops` is failure
    /// presentation (owner decision 16): its timer resolves to `failed`
    /// and it can still draw once the configured options are folded
    /// ([`Self::can_draw`]). Reads nothing else, and notes, charges and
    /// compiles nothing.
    fn failure(&self, timer: &Prop, ops: &[Op]) -> bool {
        self.failure_timed(timer) && self.can_draw(ops)
    }

    /// Whether `timer` resolves to `failed`: built-in 3, or a custom
    /// timer whose numeric chain reaches it ([`Custom::fails`]). A null,
    /// negative or script timer does not.
    fn failure_timed(&self, timer: &Prop) -> bool {
        self.kind == ScreenKind::Play
            && matches!(*timer, Prop::Id(n) if n >= 0 && self.custom.fails(n))
    }

    /// Whether a destination of `ops` can still draw once the configured
    /// options are folded, as [`Self::common_with`] folds them: no numeric
    /// op folds false or names nothing ([`Self::op_cond`]). Built-in and
    /// named conditions are not folded, so they can draw, and nor is a
    /// `draw`, which is a built-in condition or nothing ([`draw_cond`]).
    fn can_draw(&self, ops: &[Op]) -> bool {
        !ops.iter().any(|op| match op {
            Op::Id(n) => matches!(
                self.op_cond(*n),
                Some(Cond::Fold(false) | Cond::Undefined(_))
            ),
            Op::Name(_) | Op::Callable(_) => false,
        })
    }

    /// Numeric op `n`'s condition: none for 0, else built-ins first, then
    /// the declared items folded ([`ops::numeric`]).
    fn op_cond(&self, n: i32) -> Option<Cond> {
        (n != 0).then(|| ops::numeric_in(self.kind, n, &self.items))
    }

    /// One destination, in source order.
    fn destination(&mut self, index: usize, rec: u32) -> Result<(), ImportError> {
        let at = self.arena_span(rec);
        self.def.cancelled(|| Some(self.loc(at)))?;
        self.budget.element().map_err(|l| self.limit(at, l))?;
        #[cfg(test)]
        trace(Event::Charge);
        let (id, mut dst) = self.read_dst(index, rec)?;
        #[cfg(feature = "dev-tools")]
        {
            self.element = Some(crate::lua::survey::Element {
                index,
                id: id.clone(),
                kind: None,
            });
        }
        let Some(id) = id else {
            self.ignored(at, NoteKind::UndefinedObject);
            return Ok(());
        };
        // A negative number is a player-drawn image (`JSONSkinLoader.java:
        // 316-339`).
        if let Ok(n) = id.parse::<i32>()
            && n < 0
        {
            #[cfg(feature = "dev-tools")]
            if let Some(e) = &mut self.element {
                e.kind = Some("SystemImage".to_owned());
            }
            self.install(&mut dst, None, false)?;
            self.system(dst, n)?;
            self.installed();
            return Ok(());
        }
        let role = Role::Decorative.or_failure(dst.failure);
        let Some((object, orec)) = self
            .objects
            .lookup(self.def, &self.tables, &id, self.kind)?
        else {
            self.ignored(at, NoteKind::UndefinedObject);
            return Ok(());
        };
        #[cfg(feature = "dev-tools")]
        if let Some(e) = &mut self.element {
            e.kind = Some(format!("{object:?}"));
        }
        match self.kind {
            ScreenKind::Selection => return self.selection_destination(dst, object, orec),
            ScreenKind::Result => return self.result_destination(dst, object, orec),
            ScreenKind::Decision => return self.decision_destination(dst, object, orec),
            ScreenKind::Play => {}
        }
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
        // An id another table names too: the source may make the other
        // object when this one's source is missing, so that is uncertain.
        let shared = self.objects.shared(self.def, &self.tables, &id)?;
        self.install(&mut dst, Some((object, orec)), !shared)?;
        // The source makes no object of these when `src` names no source,
        // and skips the destination (`JSONSkinLoader.java:326-338,
        // 484-492`, `JsonSkinObjectLoader.java:42-72, 394-455`,
        // `JsonPlaySkinObjectLoader.java:183-214`), whatever its role. An
        // id another table names keeps the role's handling.
        if matches!(
            object,
            Object::Image
                | Object::Slider
                | Object::Graph
                | Object::HiddenCover
                | Object::LiftCover
        ) {
            let [src] = self.def.fields(orec, ["src"]);
            let src = self.opt_string(src, "src")?;
            if self.source_rec(src.as_deref())?.is_none()
                && !self.objects.shared(self.def, &self.tables, &id)?
            {
                self.ignored(at, NoteKind::UndefinedSource);
                return Ok(());
            }
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
            // A play graph (`type` 101, 102 or 110 to 115,
            // `SkinGraph.java:95-106`) is a ratio bar of the play's
            // progress, its load's or a score rate; any other type is
            // dropped.
            Object::Graph if self.play_graph(orec)? => self.ratio_bar(dst, orec, true)?,
            Object::JudgeGraph => self.judge_graph(dst, orec)?,
            Object::BpmGraph => self.bpm_graph(dst, orec)?,
            Object::TimingVisualizer => self.timing_visualizer(dst, orec)?,
            Object::HitErrorVisualizer => self.hit_error(dst, orec)?,
            _ => {
                self.drop_el(role, at, NoteKind::Unsupported { object })?;
            }
        }
        self.installed();
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
    /// Destination `dst`'s keys ([`track::read`]), the one reading every
    /// caller uses. Colour is clamped rather than refused, so the skin
    /// looks as its author saw it; when `colour` is drawn, a clamped
    /// channel is reported once, as a `Fallback` note at its first key.
    fn read_keys(
        &mut self,
        dst: &Dst,
        digits: Option<i32>,
        colour: Colour,
    ) -> Result<track::Keys, ImportError> {
        let keys = track::read(self.def, &dst.keys, digits)?;
        if colour == Colour::Drawn
            && let Some(k) = keys.clamped
        {
            let span = self.arena_span(k);
            self.note(Some(span), Severity::Fallback, NoteKind::ColourClamped);
        }
        Ok(keys)
    }

    fn common_with(
        &mut self,
        role: Role,
        dst: &Dst,
        o: Opts<'_>,
    ) -> Result<Option<Common>, ImportError> {
        let text = o.text;
        let at = dst.span;
        // The source draws it only while the mouse is inside its rect
        // (`SkinObject.java:603-607`): only a selection skin's elements
        // keep it; the play, decision and result have no pointer, and a
        // song list part has no hover of its own.
        let hover = match dst.mouse {
            None => None,
            Some(m) if self.kind == ScreenKind::Selection && !dst.nested => {
                Some(self.hover(at, m)?)
            }
            Some(_) => {
                self.drop_el(role, at, NoteKind::UnsupportedField { field: "mouseRect" })?;
                return Ok(None);
            }
        };
        // Keys: charged as written before they are read, then as emitted
        // when more (a neutral track emits its first and last).
        let written = dst.keys.len();
        let fits = if o.charged {
            written <= skin_model::MAX_TRACK_KEYS
        } else {
            self.budget.keys(written).map_err(|l| self.limit(at, l))?
        };
        let drawn = if o.neutral {
            Colour::Unused
        } else {
            Colour::Drawn
        };
        let mut keys = self.read_keys(dst, o.digits, drawn)?;
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
        // The source's order: the numeric ids first, then the names and
        // functions in the list's order, then `draw`
        // (`JsonSkin.java:468-480`, `SkinObject.java:282-315`).
        let ids = dst.ops.iter().filter(|op| matches!(op, Op::Id(_)));
        let others = dst.ops.iter().filter(|op| !matches!(op, Op::Id(_)));
        for op in ids.chain(others) {
            let cond = match op {
                Op::Id(n) => {
                    if !seen.insert(*n) {
                        continue;
                    }
                    let Some(c) = self.op_cond(*n) else {
                        continue;
                    };
                    c
                }
                Op::Name(s) => match ops::named_in(self.kind, s) {
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
                Op::Callable(id) => {
                    let id = *id;
                    if !self.draw_callback(role, at, (id, "op"), &mut conds, &mut deviates)? {
                        return Ok(None);
                    }
                    continue;
                }
            };
            #[cfg(feature = "dev-tools")]
            let folded = matches!(cond, Cond::Fold(false));
            if !self.condition(role, at, cond, &mut conds, &mut deviates)? {
                #[cfg(feature = "dev-tools")]
                if folded {
                    self.folded = true;
                    let callables = dst.ops.iter().filter_map(|op| match op {
                        Op::Callable(id) => Some(*id),
                        _ => None,
                    });
                    let draw = match dst.draw {
                        Prop::Callable(id) => Some(id),
                        _ => None,
                    };
                    for id in callables.chain(draw) {
                        self.unused(id, crate::lua::callback::Unused::FoldedFalse);
                    }
                }
                return Ok(None);
            }
        }
        match &dst.draw {
            Prop::Null => {}
            Prop::Id(n) => {
                if let Some(c) = draw_cond(self.kind, *n) {
                    if !self.condition(role, at, c, &mut conds, &mut deviates)? {
                        return Ok(None);
                    }
                } else {
                    self.ignored(at, NoteKind::Ignored { field: "draw" });
                }
            }
            Prop::Name(s) => {
                let Some(c) = ops::named_in(self.kind, s) else {
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
            Prop::Callable(id) => {
                if !self.draw_callback(role, at, (*id, "draw"), &mut conds, &mut deviates)? {
                    return Ok(None);
                }
            }
        }

        // Timer.
        let (timer, untimed) = match dst.timer {
            Prop::Null => (self.screen(), true),
            Prop::Id(n) if n < 0 => (self.screen(), true),
            Prop::Name(_) => {
                self.drop_el(role, at, NoteKind::Timer(TimerWhy::Script))?;
                return Ok(None);
            }
            Prop::Callable(id) if self.kind == ScreenKind::Play => {
                let last = track.keys.iter().map(|k| k.at).max().unwrap_or(0);
                match self.timer_callback(role, at, (id, track.end, last), &conds)? {
                    Ok(t) => (t, false),
                    Err(why) => {
                        self.drop_el(role, at, why)?;
                        return Ok(None);
                    }
                }
            }
            Prop::Callable(id) => {
                let why = self.unconsumed(role, id, "timer", "timer")?;
                self.drop_el(role, at, why)?;
                return Ok(None);
            }
            Prop::Id(n) => match self.custom.resolve_in(self.kind, n) {
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
        let (mut lift, mut cover, mut hidden) = (false, false, false);
        let mut turntable = false;
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
        // for the ids a cover consumes, listed or added by the source, for
        // 3, 4 and 5 on a follower, and for 3 and 4 on a relative part.
        // Only the play sets them: on another screen the run-time offsets
        // are undefined, ignored with a note.
        let play = self.kind == ScreenKind::Play;
        let reserved = |id: i32| {
            play && (o.allow.contains(&id)
                || (matches!(id, OFFSET_LIFT | OFFSET_LANECOVER) && (o.follow || o.relative))
                || (id == OFFSET_HIDDEN && o.follow)
                || (id == OFFSET_TURNTABLE && (o.turntable || o.text)))
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
                    hidden |= id == OFFSET_HIDDEN;
                }
                turntable |= id == OFFSET_TURNTABLE && o.turntable;
                continue;
            }
            if RUNTIME_OFFSETS.contains(&id) && !play {
                self.ignored(at, NoteKind::RuntimeOffset(id));
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
        // Offset 5 is an alpha offset too: −255 while HIDDEN+ is off, which
        // the model reproduces by not drawing the follower. The source
        // skips every alpha offset at an interpolated colour
        // (`SkinObject.java:489-504`) and clamps each in turn (`:474-481`),
        // so a follower of HIDDEN+ needs a fixed colour and no configured
        // alpha above 0, or it would show where the model hides it.
        if hidden && (!track.fixed_color() || alpha.iter().any(|&a| a > 0)) {
            self.drop_el(role, at, NoteKind::OffsetAlpha)?;
            return Ok(None);
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

        // Blend (`Skin.java:655-665`). A neutral outer destination draws
        // nothing of its own: its parts keep their blend, so its own is
        // ignored. Blend 3 draws additive: the source sets the subtract
        // equation and resets it to add before its quads draw
        // (`:657-662`); the quads queued before it, which its batch
        // flushes under subtract when that changes its blend factors, keep
        // their own blend here.
        // Source: SF-006
        let blend = match dst.blend {
            2..=4 | 9 if o.neutral => {
                self.ignored(at, NoteKind::Ignored { field: "blend" });
                Blend::Alpha
            }
            2 => Blend::Add,
            3 => {
                self.ignored(at, NoteKind::SubtractBlend);
                Blend::Add
            }
            4 | 9 if text => {
                self.ignored(at, NoteKind::Ignored { field: "blend" });
                Blend::Alpha
            }
            4 if o.multiply => Blend::Multiply,
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
        let tests_status = self.kind == ScreenKind::Result && src.contains(result::STATUS_FIELD);
        #[cfg(test)]
        VISIBLE.with(|v| v.borrow_mut().push(visible.clone()));

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
            follow: Follow {
                lift,
                cover,
                hidden,
            },
            turntable,
            stretch: Stretch::of(dst.stretch),
            stretch_mode: dst.stretch,
            untimed,
            track,
            display_deviation: deviates,
            tests_status,
            conds,
            hover,
            action: dst.action,
            search: false,
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
                if role.refuses() {
                    self.drop_el(role, at, NoteKind::UndefinedOp(id))?;
                } else {
                    self.ignored(at, NoteKind::UndefinedOp(id));
                }
                Ok(false)
            }
        }
    }

    /// The schema the skin's expressions read: its screen's.
    fn schema(&self) -> &'static skin_model::Schema {
        callback::schema(self.kind)
    }

    /// The screen's own `screen` timer, which an element without a timer
    /// runs on.
    fn screen(&self) -> TimerRef {
        TimerRef::Builtin(match self.kind {
            ScreenKind::Selection => skin_model::selection_timer::SCREEN,
            ScreenKind::Result => skin_model::result_timer::SCREEN,
            ScreenKind::Decision => skin_model::decision_timer::SCREEN,
            ScreenKind::Play => slot::SCREEN,
        })
    }

    /// The callback outputs the screen's expressions read: play's program
    /// or a selection or result screen's runtime.
    fn outputs(&self) -> &[skin_model::output::OutTy] {
        match self.kind {
            ScreenKind::Play | ScreenKind::Decision => &self.present.signature,
            ScreenKind::Selection | ScreenKind::Result => &self.outputs,
        }
    }

    /// Compiles `src` against the screen's schema and charges its
    /// operations before it is kept.
    fn compile(&mut self, src: &str, want: Ty, at: Span) -> Result<Program, ImportError> {
        #[cfg(test)]
        trace(Event::Compile);
        let scope = Scope::new(self.schema()).outputs(self.outputs());
        let p = compile(src, &scope, want).map_err(|source| ImportError::Expr {
            at: self.loc(at),
            source,
        })?;
        self.budget.ops(&p).map_err(|l| self.limit(at, l))?;
        Ok(p)
    }

    /// `src` compiled against the play schema, or `None` when it does not
    /// compile: a program read or gate past an expression's limits. It is
    /// not charged: its callback charges it at each use
    /// ([`skin_model::present::callback_cost`]).
    fn trial(&self, src: &str, want: Ty) -> Option<Program> {
        compile(src, &Scope::new(self.schema()), want).ok()
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

    /// An image's frame cycle (`SkinSourceImage.java:73-89`), for an
    /// element of `role`: `Ok(None)` for no cycle; `Err(why)` for one the
    /// model cannot run. A cycle without a timer runs on `screen`, which
    /// no timer id maps to.
    fn cycle(
        &mut self,
        (at, role): (Span, Role),
        frames: u32,
        t: &Prop,
        cycle: i32,
    ) -> Result<Result<Option<CycleDef>, NoteKind>, ImportError> {
        if frames <= 1 || cycle == 0 {
            return Ok(Ok(None));
        }
        let period = i64::from(cycle) * 1000;
        if cycle < 0 || period > skin_model::MAX_KEY_US {
            return Ok(Err(NoteKind::Timer(TimerWhy::Cycle)));
        }
        let timer = match *t {
            Prop::Null => self.screen(),
            Prop::Id(n) if n < 0 => self.screen(),
            Prop::Name(_) => return Ok(Err(NoteKind::Timer(TimerWhy::Script))),
            Prop::Callable(id) => return Ok(Err(self.unconsumed(role, id, "timer", "timer")?)),
            Prop::Id(n) => match self.custom.resolve_in(self.kind, n) {
                Timer::Unset => {
                    self.ignored(at, NoteKind::Timer(TimerWhy::Unset(n)));
                    return Ok(Ok(None));
                }
                Timer::Unsupported => {
                    let why = if n > timer::BUILTIN_MAX {
                        TimerWhy::Custom(n)
                    } else {
                        TimerWhy::Unsupported(n)
                    };
                    return Ok(Err(NoteKind::Timer(why)));
                }
                t => self.timer_ref(t, at)?,
            },
        };
        Ok(Ok(Some(CycleDef {
            period: TimeUs(period),
            timer,
        })))
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

    /// `mouseRect` object `m` of the destination at `at` as a hover rect:
    /// its `x`, `y`, `w` and `h` (0 when absent) scaled as the region is
    /// (`skin/Skin.java:183-185`), an offset from the region's
    /// bottom-left corner, y up.
    fn hover(&mut self, at: Span, m: u32) -> Result<HoverRect, ImportError> {
        let [x, y, w, h] = self.def.fields(m, ["x", "y", "w", "h"]);
        let src = [
            self.int_or(x, "x", 0)?,
            self.int_or(y, "y", 0)?,
            self.int_or(w, "w", 0)?,
            self.int_or(h, "h", 0)?,
        ];
        let [x, y, w, h] = Rect::scaled(src, self.r)
            .and_then(|r| geom::coords([r.x.round()?, r.y.round()?, r.w.round()?, r.h.round()?]))
            .ok_or_else(|| self.range(at, "mouseRect"))?;
        Ok(HoverRect { x, y, w, h })
    }

    /// `c`'s pointer region as its element's `track` is keyed: each key's
    /// destination, before a stretch fits an image in it or an alignment
    /// moves text (`skin/SkinObject.java:591-602`, where `region` comes
    /// from the destination, and `623-643`, where only the drawn rect is
    /// stretched). `None` when it is the track itself.
    fn pointer_region(&self, c: &Common, track: &Track) -> Result<Option<Track>, ImportError> {
        let rects = c
            .track
            .keys
            .iter()
            .map(|k| {
                self.key_rect(c, k)?
                    .model_from(self.flip)
                    .ok_or_else(|| self.range(c.span, "dst"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let region = self.model_track(c, &rects)?;
        Ok((region != *track).then_some(region))
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
        if self.kind == ScreenKind::Selection {
            // Only an image or an image choice carries an action, and only
            // text showing the query is the search box.
            let action = c
                .action
                .filter(|_| matches!(source, Source::Image(_) | Source::ImageChoice(_)));
            let search = c.search && matches!(source, Source::Text(_));
            if c.hover.is_some() || action.is_some() || search {
                let region = self.pointer_region(&c, &track)?;
                self.sel.pointer.push(PointerDef {
                    element: u32::try_from(self.elements.len()).unwrap_or(u32::MAX),
                    action,
                    hover: c.hover,
                    search,
                    region,
                });
            }
        }
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
                angle: if c.turntable {
                    AngleFollow::Turntable
                } else {
                    AngleFollow::None
                },
                ..Place::SCREEN
            },
        });
        self.element_spans.push(c.span);
        Ok(())
    }

    /// Notes what a retained element or judge part carries: D3 once when
    /// what it shows depends on the state's start ([`Self::untimed_phase`]),
    /// D4 once when it deviates ([`Common::display_deviation`]), and D9
    /// [`LaneApprox::CoverMotion`] once when it follows the covers.
    fn retained(&mut self, c: &Common) -> Result<(), ImportError> {
        if self.kind == ScreenKind::Result {
            self.result_retained(c.role, c.tests_status);
        }
        self.untimed_phase(c);
        if c.display_deviation {
            self.note(
                Some(c.span),
                Severity::Deviation,
                NoteKind::Deviation(DeviationId::D4),
            );
        }
        self.cover_motion(c)
    }

    /// Notes D3 for retained critical `c` when its presentation phase
    /// depends on the start ([`policy::start_dependent`]): an image it
    /// keeps cycling without a timer, whatever its own timer, or, without
    /// a timer itself, its track (owner decision 7). Each destination is
    /// retained once.
    fn untimed_phase(&mut self, c: &Common) {
        let cycled = self.cycled.contains(&c.span);
        if matches!(c.role, Role::Critical(_))
            && policy::start_dependent(&c.track, c.untimed, cycled)
        {
            self.note(
                Some(c.span),
                Severity::Deviation,
                NoteKind::Deviation(DeviationId::D3),
            );
        }
    }

    /// Notes D9 [`LaneApprox::CoverMotion`] once for a retained element
    /// or grade that follows the covers.
    fn cover_motion(&mut self, c: &Common) -> Result<(), ImportError> {
        if c.follow == Follow::NONE {
            return Ok(());
        }
        let part = match c.role {
            Role::Critical(part) => part,
            Role::Decorative | Role::Essential(_) => CriticalPart::Cover,
        };
        self.lane_approx(c.span, part, LaneApprox::CoverMotion)
    }

    /// The `source` record `src` names: the last of an id wins
    /// (`JSONSkinLoader.java:312-314`). `None` when `src` is absent or no
    /// record has it, which the source treats as no image.
    fn source_rec(&mut self, src: Option<&str>) -> Result<Option<u32>, ImportError> {
        if self.sources.is_none() {
            let recs = object::records(&self.tables, "source").to_vec();
            self.sources = Some(object::index(self.def, &recs, true)?);
        }
        Ok(src.and_then(|s| self.sources.as_ref().and_then(|m| m.get(s)).copied()))
    }

    /// Records what the source's loader would find of each source no
    /// element reached (dev-tools, the reference's facts): a movie by its
    /// extension, else whether its file exists, read through the root as
    /// admission reads it; a glob is unknown.
    #[cfg(feature = "dev-tools")]
    fn record_unreached_sources(&mut self) {
        use crate::lua::replay::{SourceFact, has_source, is_recording, record_source};
        if !is_recording() {
            return;
        }
        for rec in object::records(&self.tables, "source").to_vec() {
            let [id] = self.def.fields(rec, ["id"]);
            let Ok(Some(id)) = self.opt_string(id, "id") else {
                continue;
            };
            if has_source(&id) {
                continue;
            }
            let fact = match self.source_path(rec) {
                Ok(Some(path)) if asset::is_movie(&path) => SourceFact::Movie,
                Ok(Some(path)) => match self.choices.substitute(&path) {
                    asset::Path::File(p) => {
                        let name = asset::resolve(&self.entry_dir, &p);
                        if self.def.root().read(&name, u64::MAX).is_ok() {
                            SourceFact::Loaded
                        } else {
                            SourceFact::Missing
                        }
                    }
                    asset::Path::Missing => SourceFact::Missing,
                    asset::Path::Glob => SourceFact::Unknown,
                },
                _ => SourceFact::Missing,
            };
            record_source(&id, fact);
        }
    }

    /// Source record `rec`'s `path`.
    fn source_path(&mut self, rec: u32) -> Result<Option<String>, ImportError> {
        let [path] = self.def.fields(rec, ["path"]);
        self.opt_string(path, "path")
    }

    /// The top-level integer `name`, `default` when absent.
    fn top_int(&mut self, name: &'static str, default: i32) -> Result<i32, ImportError> {
        let [f] = self.def.fields(self.def.top(), [name]);
        self.int_or(f, name, default)
    }

    /// The grades that give a bomb, from the top-level `judgetimer`
    /// (`JsonSkin.java:20`, default 1): a judgment at or below it, PERFECT
    /// 0 to empty POOR 5 (`play/JudgeManager.java:673`). Its whole domain
    /// maps exactly: below 0 none, above 5 every grade.
    fn bomb_up_to(&mut self) -> Result<Option<Grade>, ImportError> {
        let n = self.top_int("judgetimer", 1)?;
        Ok(usize::try_from(n)
            .ok()
            .map(|i| Grade::ALL.get(i).copied().unwrap_or(Grade::EmptyPoor)))
    }

    /// Closing's declared duration, from the top-level `close` in ms
    /// (`JsonSkin.java:17`, default 0): a positive value in µs, `None`
    /// for 0 or below. It is read whether or not the skin defines a
    /// note, where the source sets it only while loading the note's
    /// destination (`JsonPlaySkinObjectLoader.java:37-40`).
    fn close(&mut self) -> Result<Option<TimeUs>, ImportError> {
        let ms = self.top_int("close", 0)?;
        Ok((ms > 0).then(|| TimeUs(i64::from(ms) * 1_000)))
    }

    /// The loading presentation's declared least length, from the
    /// top-level `loadend` in ms: in µs, a negative value 0, `None` when
    /// absent. Read as every integer field is: a value the reader refuses
    /// refuses the import.
    fn loadend(&mut self) -> Result<Option<TimeUs>, ImportError> {
        let [f] = self.def.fields(self.def.top(), ["loadend"]);
        let ms = f.map(|f| self.def.int(f, "loadend")).transpose()?;
        Ok(ms.map(|ms| TimeUs(i64::from(ms.max(0)) * 1_000)))
    }

    /// A play skin's declared ending (design.md "Play Lifecycle", Audio
    /// tail), from the top-level `finishmargin` and `fadeout` in ms
    /// (`JsonSkin.java:14, 21`, each 0 when absent; a Lua skin's table
    /// converts to the same fields, `LuaSkinLoader.java:90-91`), in µs
    /// over the source's whole `int` range, with no cap: a negative
    /// `finishmargin` is 0, and `fadeout` keeps its sign, since the
    /// source leaves on a negative one before its finish margin
    /// (`BMSPlayer.java:756`). `finishmargin` is read whether or not the
    /// skin defines a note, as `close` is, where the source sets it only
    /// while loading the note's destination
    /// (`JsonPlaySkinObjectLoader.java:37-44`). Each is read as every
    /// integer field is: a value the reader refuses refuses the import.
    fn play_ending(&mut self) -> Result<PlayEnding, ImportError> {
        let finish_margin = self.top_int("finishmargin", 0)?;
        let fadeout = self.top_int("fadeout", 0)?;
        Ok(PlayEnding {
            finish_margin: TimeUs(i64::from(finish_margin.max(0)) * 1_000),
            fadeout: TimeUs(i64::from(fadeout) * 1_000),
        })
    }

    /// A result skin's declared timing (design.md "State Model", Result),
    /// from the top-level `fadeout`, `input` and `scene` in ms
    /// (`JsonSkin.java:14-16`, each 0 when absent; a Lua skin's table
    /// converts to the same fields, `LuaSkinLoader.java:90`), in µs: a
    /// negative value is 0, a `scene` of 0 or below is none, and a
    /// `fadeout` past [`skin_model::MAX_RESULT_OUTRO`] is clamped to it
    /// with a deviation note. Each is read as every integer field is: a
    /// value the reader refuses (`null`, a string that is no number, one
    /// past `i32`) refuses the import.
    fn result_timing(&mut self) -> Result<ResultTiming, ImportError> {
        let us = |ms: i32| TimeUs(i64::from(ms.max(0)) * 1_000);
        let fadeout = self.top_int("fadeout", 0)?;
        let input = self.top_int("input", 0)?;
        let scene = self.top_int("scene", 0)?;
        let mut outro = us(fadeout);
        if outro > skin_model::MAX_RESULT_OUTRO {
            outro = skin_model::MAX_RESULT_OUTRO;
            let [f] = self.def.fields(self.def.top(), ["fadeout"]);
            let at = f.map(|f| self.arena_span(f));
            self.note(
                at,
                Severity::Deviation,
                NoteKind::OutroClamped {
                    declared_ms: fadeout,
                },
            );
        }
        Ok(ResultTiming {
            outro,
            input: us(input),
            scene: (scene > 0).then(|| us(scene)),
        })
    }

    /// A decision skin's declared timing (design.md "Play Lifecycle", the
    /// decision screen), from the top-level `scene`, `fadeout` and `input`
    /// in ms (`JsonSkin.java:14-16`, each 0 when absent; a Lua skin's table
    /// converts to the same fields), in µs: a negative value is 0, and a
    /// `scene` or `fadeout` past [`skin_model::MAX_DECISION_PHASE`] is
    /// clamped to it with a deviation note. Each is read as every integer
    /// field is: a value the reader refuses refuses the import.
    fn decision_timing(&mut self) -> Result<DecisionTiming, ImportError> {
        let fadeout = self.top_int("fadeout", 0)?;
        let input = self.top_int("input", 0)?;
        let scene = self.top_int("scene", 0)?;
        Ok(DecisionTiming {
            scene: self.decision_phase("scene", scene),
            fadeout: self.decision_phase("fadeout", fadeout),
            input: TimeUs(i64::from(input.max(0)) * 1_000),
        })
    }

    /// The top-level `field`'s declared `ms` in µs, 0 for a negative value
    /// and clamped to [`skin_model::MAX_DECISION_PHASE`] with a note.
    fn decision_phase(&mut self, field: &'static str, ms: i32) -> TimeUs {
        let us = TimeUs(i64::from(ms.max(0)) * 1_000);
        if us <= skin_model::MAX_DECISION_PHASE {
            return us;
        }
        let [f] = self.def.fields(self.def.top(), [field]);
        let at = f.map(|f| self.arena_span(f));
        self.note(
            at,
            Severity::Deviation,
            NoteKind::DecisionClamped {
                field,
                declared_ms: ms,
            },
        );
        skin_model::MAX_DECISION_PHASE
    }

    fn finish(mut self, files: Vec<FileChoice>) -> Result<Imported, ImportError> {
        let selection = self.kind == ScreenKind::Selection;
        let result = self.kind == ScreenKind::Result;
        let decision = self.kind == ScreenKind::Decision;
        if selection {
            self.selection_essentials()?;
        }
        let timing = if result {
            self.result_essentials()?;
            self.result_timing()?
        } else {
            ResultTiming::default()
        };
        let decision_timing = if decision {
            self.decision_timing()?
        } else {
            DecisionTiming::default()
        };
        let (bomb_up_to, close, loadend, ending) = if selection || result || decision {
            (None, None, None, PlayEnding::default())
        } else {
            (
                self.bomb_up_to()?,
                self.close()?,
                self.loadend()?,
                self.play_ending()?,
            )
        };
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
            close,
            loadend,
            present: {
                self.close_slots()?;
                (!self.present.callbacks.is_empty()).then(|| std::mem::take(&mut self.present))
            },
        };
        let loaded = if selection {
            let widgets = SelectionDefs {
                song_list: self.sel.list.take(),
                ratio_bars: std::mem::take(&mut self.sel.ratio_bars),
                // Kept: a refusal names its entry's element.
                pointer: self.sel.pointer.clone(),
            };
            Skin::new_selection(def, widgets)
        } else if result {
            {
                let widgets = ResultDefs {
                    judge_graphs: std::mem::take(&mut self.judge_graphs),
                    gauge_graphs: std::mem::take(&mut self.gauge_graphs),
                    timing_graphs: std::mem::take(&mut self.timing_graphs),
                    timing,
                };
                Skin::new_result(def, widgets)
            }
        } else if decision {
            let defs = DecisionDefs {
                timing: decision_timing,
            };
            Skin::new_decision(def, defs)
        } else {
            let widgets = PlayDefs {
                ratio_bars: std::mem::take(&mut self.sel.ratio_bars),
                judge_graphs: std::mem::take(&mut self.judge_graphs),
                bpm_graphs: std::mem::take(&mut self.bpm_graphs),
                timing_visualizers: std::mem::take(&mut self.timing_visualizers),
                hit_errors: std::mem::take(&mut self.hit_errors),
            };
            Skin::new_play(def, widgets).map(|s| s.with_play_ending(ending))
        };
        let skin = loaded
            .map(|s| s.with_outputs(self.outputs.len()))
            .map_err(|source| {
                let at = self.load_loc(&source);
                ImportError::Model { at, source }
            })?;
        #[cfg(feature = "dev-tools")]
        let notes = self
            .located
            .take()
            .filter(|n| n.len() == self.notes.list.len())
            .unwrap_or_else(|| report::locate(&self.def.arena, &self.notes.list));
        #[cfg(not(feature = "dev-tools"))]
        let notes = report::locate(&self.def.arena, &self.notes.list);
        let report = Report {
            sources: self.def.arena.sources(),
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
            | LoadError::Text { element, .. }
            | LoadError::SourceKind { element, .. } => Some(self.element_spans.get(element)?),
            LoadError::SongList(_) => Some(self.sel.span.as_ref()?),
            LoadError::RatioBar { bar, .. } => Some(self.sel.bar_spans.get(bar)?),
            LoadError::Pointer { entry, .. } => {
                let element = self.sel.pointer.get(entry)?.element;
                Some(self.element_spans.get(usize::try_from(element).ok()?)?)
            }
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

/// Numeric `draw` `n`'s condition: a boolean property joining the ops
/// (`JsonSkinObjectLoader.java:734-737`), so a built-in only; anything
/// else, an option item among them, has no effect (`None`) and never
/// folds.
fn draw_cond(kind: ScreenKind, n: i32) -> Option<Cond> {
    ops::is_builtin(i32::try_from(n.unsigned_abs()).unwrap_or(0))
        .then(|| ops::numeric_in(kind, n, &BTreeMap::new()))
}

/// The pivot of `center`: 1..=9 as the table of `SkinObject.java:80-81,
/// 228-233`, the centre for 0 and anything else (design.md "Differences
/// from the source", imported skins).
fn pivot(center: i32) -> Anchor {
    match center {
        1 => Anchor::BottomLeft,
        2 => Anchor::Bottom,
        3 => Anchor::BottomRight,
        4 => Anchor::Left,
        6 => Anchor::Right,
        7 => Anchor::TopLeft,
        8 => Anchor::Top,
        9 => Anchor::TopRight,
        _ => Anchor::Center,
    }
}

#[cfg(test)]
mod tests;
