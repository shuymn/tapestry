//! A loaded skin: its elements in draw order, its regions, its `when`
//! timers, its images, its lanes and its text, and the per-frame build
//! into a [`QuadBatch`].

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{Chart, LANE_COUNT, TimeUs};
use eval::{GaugeState, NoteFacts};
use rules::GaugeValue;

use crate::atlas::uv::{Cycle, UvTable};
use crate::atlas::{Atlas, AtlasError, AtlasLimits, Variant};
use crate::canvas::{Canvas, Fit, MAX_CANVAS, f32_of};
use crate::decision::schema::timer as decision_timer;
use crate::decision::{DecisionError, DecisionFrame, DecisionScreen};
use crate::expr::State;
use crate::expr::{CompileError, Program, SkinOps};
use crate::image::Sheet;
use crate::layout::{Controls, Layout, Region, RegionDef};
use crate::place::{
    AngleFollow, Bind, Bounds, Fill, Follow, Native, Place, Placement, Regions, union,
};
use crate::play::loading::{Loading, LoadingFrame, LoadingGauge, Need};
use crate::play::schema::timer::BUILTIN;
use crate::play::schema::{Grade, field};
use crate::play::timers::elapsed_us;
use crate::play::{BuildError, Continuation, Play, PlayContext, PrepareError, SkinFrame};
use crate::present::{OutTy, Present, PresentDef, PresentError};
use crate::quad::{Blend, ClipId, Filter, QuadBatch, RunKey};
use crate::result::schema::timer as result_timer;
use crate::result::view::{ResultLabels, ResultView};
use crate::result::{ResultError, ResultFrame, ResultScreen, ResultTimers};
use crate::schema::Ty;
use crate::screen::ScreenKind;
use crate::select::schema::timer as selection_timer;
use crate::select::{Selection, SelectionError, SelectionFrame, SelectionTimers};
use crate::selection::{SelectionLabels, SelectionSettings, SelectionView};
use crate::track::{MAX_KEY_US, Track};
use crate::widget::bar::{BarGroupError, BarScratch, MAX_BAR_GROUPS};
use crate::widget::cover_paint::{
    CoverImage, CoverMotion, CoverPaint, CoverPaintDef, CoverPaintError, CoverTrim, PaintImage,
    SliderRate, TrimOutcome,
};
use crate::widget::digits::{Align, Digits, MAX_DIGIT_OFFSETS, Number, NumberDef, NumberError};
use crate::widget::gauge::{Gauge, GaugeDef, GaugeDraw, GaugeError, Shown};
use crate::widget::gauge_graph::{GaugeGraph, GaugeGraphDef, GaugeGraphDraw};
use crate::widget::glyphs;
use crate::widget::glyphs::{BitmapLines, GlyphLine};
use crate::widget::image_choice::{
    ChoicePart, ImageChoice, ImageChoiceDef, ImageChoiceError, MAX_IMAGE_CHOICE_REFS,
};
use crate::widget::judge::{
    ComboStrip, GradeImage, Judge, JudgeDef, JudgeDraw, JudgeError, JudgePart, JudgeSlot,
};
use crate::widget::judge_graph::{GraphDraw, JudgeGraph, JudgeGraphDef};
use crate::widget::lanes::LaneView;
use crate::widget::lanes::{Draw, LANE_QUADS, LANE_RESERVED, Lanes, LanesDef, LanesError, Side};
use crate::widget::ratio_bar::{
    self, Hit, RatioBar, RatioBarDef, RatioBarError, RatioForm, RatioSource, SliderCommand,
    SliderDirection,
};
use crate::widget::song_list::{
    ImagePartDef, LevelDef, ListDraw, MAX_PLACEMENTS, Part, PartDef, PlacementDef, RowTextDef,
    SongList, SongListDef, SongListError,
};
use crate::widget::text::clean;
use crate::widget::text::{
    self, ChartText, FontRef, MAX_TEXT_ELEMENTS, MAX_TEXT_EM_PX, Overflow, Text, TextDef,
    TextError, TextRequest, TextSlots, TextTable, TextTableError, TextValue,
};
use crate::widget::timing_graph::{TimingGraph, TimingGraphDef, TimingGraphDraw};
use crate::widget::{Brush, Frame, ImageRef, Starts, sample};

/// The most elements one skin may hold.
pub const MAX_ELEMENTS: usize = 16_384;
/// The most `when` timers one skin may hold; with the built-in timers,
/// every slot fits a `u16`.
pub const MAX_WHEN_TIMERS: usize = 1_024;
/// The most track keys across one skin's elements.
pub const MAX_SKIN_KEYS: usize = 65_536;
/// The largest magnitude of a key's x, y, w or h in a skin, and of the
/// lanes' coordinates, in reference pixels. Placement sums and products
/// of a few such values stay far inside `i64` and exact in `f64`.
pub const MAX_COORD: i32 = 1 << 20;
/// The most quads one frame's widgets other than the lanes may push,
/// summed over their elements' worst cases.
#[expect(clippy::cast_possible_truncation, reason = "16 384")]
pub const MAX_WIDGET_QUADS: u32 = MAX_ELEMENTS as u32;

/// What an element draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The sheet's image at this index, at the frame its cycle shows.
    Image(u32),
    /// The lanes widget, on the skin's [`LanesDef`]. Its track gives only
    /// color and alpha.
    Lanes,
    /// The number at this index of [`WidgetDefs::numbers`]. Its track
    /// places cell 0 as an image's places its rect.
    Number(u32),
    /// The gauge at this index of [`WidgetDefs::gauges`]. Its track places
    /// the whole gauge as an image's places its rect.
    Gauge(u32),
    /// The judge at this index of [`WidgetDefs::judges`]. Its track gives
    /// only color and alpha; the parts place themselves.
    Judge(u32),
    /// The text at this index of [`WidgetDefs::texts`]. Its track's rect
    /// is the line's box, whose height is the em.
    Text(u32),
    /// The image choice table at this index of
    /// [`WidgetDefs::image_choices`]: the entry its selector picks, on the
    /// entry's track or else the element's, at the frame the entry's own
    /// cycle shows.
    ImageChoice(u32),
    /// The cover paint at this index of [`WidgetDefs::cover_paints`]: its
    /// image or its choice's entry, on its track (or the entry's), moved
    /// by its motion, the covers it follows and its slider, and gated and
    /// clipped by its trim line.
    CoverPaint(u32),
    /// The selection skin's song list ([`SelectionDefs::song_list`]). Its
    /// track gives only color and alpha; its placements place themselves.
    SongList,
    /// The ratio bar at this index of [`SelectionDefs::ratio_bars`]: its
    /// image cut to, or moved by, its ratio from the element's rect.
    RatioBar(u32),
    /// The judge graph at this index of [`ResultDefs::judge_graphs`]: the
    /// view's timelines over the element's rect.
    JudgeGraph(u32),
    /// The gauge graph at this index of [`ResultDefs::gauge_graphs`]: the
    /// view's emphasized gauge over the element's rect.
    GaugeGraph(u32),
    /// The timing distribution at this index of
    /// [`ResultDefs::timing_graphs`]: the view's deviations over the
    /// element's rect.
    TimingGraph(u32),
}

/// What only a result skin has: the widgets it draws and the timing it
/// declares.
#[derive(Clone, Debug, Default)]
pub struct ResultDefs {
    pub judge_graphs: Vec<JudgeGraphDef>,
    pub timing: crate::result::ResultTiming,
    pub gauge_graphs: Vec<GaugeGraphDef>,
    pub timing_graphs: Vec<TimingGraphDef>,
}

/// What only a decision skin has: the timing it declares.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecisionDefs {
    pub timing: crate::decision::DecisionTiming,
}

/// The widgets only a selection skin draws.
#[derive(Clone, Debug, Default)]
pub struct SelectionDefs {
    /// The song list [`Source::SongList`] draws.
    pub song_list: Option<SongListDef>,
    pub ratio_bars: Vec<RatioBarDef>,
}

/// The widgets elements draw by index.
#[derive(Clone, Debug, Default)]
pub struct WidgetDefs {
    pub numbers: Vec<NumberDef>,
    pub gauges: Vec<GaugeDef>,
    pub judges: Vec<JudgeDef>,
    pub texts: Vec<TextDef>,
    pub image_choices: Vec<ImageChoiceDef>,
    pub cover_paints: Vec<CoverPaintDef>,
}

/// The timer an element's track or an image's cycle runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerRef {
    /// A built-in timer's slot (`play::timer::*`, `lane_timer`).
    Builtin(u16),
    /// The skin's `when` timer at this index.
    When(u16),
    /// The play presentation program's timer callback at this index: an
    /// element's timer only.
    Output(u16),
}

/// One element as a front end builds it. Every element names its timer;
/// a front end gives an element without one `screen`.
#[derive(Clone, Debug)]
pub struct ElementDef {
    pub source: Source,
    /// Corner-origin: a key's `(x, y)` is its rect's top-left corner.
    pub track: Track,
    pub timer: TimerRef,
    /// Drawn only while this is true; compiled against the play schema.
    pub visible: Option<Program>,
    pub blend: Blend,
    pub clip: Option<ClipId>,
    pub place: Place,
}

/// Everything a skin is loaded from.
#[derive(Clone, Debug)]
pub struct SkinDef {
    pub fit: Fit,
    /// Per region, by [`Region::index`].
    pub regions: [RegionDef; Region::COUNT],
    pub whens: Vec<Program>,
    pub sheet: Sheet,
    /// The lanes, when an element draws [`Source::Lanes`].
    pub lanes: Option<LanesDef>,
    /// In draw order.
    pub elements: Vec<ElementDef>,
    /// The widgets [`Source::Number`], [`Source::Gauge`] and
    /// [`Source::Judge`] index.
    pub widgets: WidgetDefs,
    /// The last grade, in [`Grade::ALL`]'s order, whose press starts a
    /// lane's `bomb` timer; `None` starts none.
    pub bomb_up_to: Option<Grade>,
    /// How long the skin declares Closing (design.md "Play Lifecycle",
    /// Closing), positive; `None` declares nothing, and the player's
    /// floor applies either way.
    pub close: Option<TimeUs>,
    /// How long the play skin declares its loading presentation lasts at
    /// least (design.md "Play Lifecycle", the loading state), 0 or more;
    /// `None` declares nothing.
    pub loadend: Option<TimeUs>,
    /// The play presentation program, whose outputs the expressions read
    /// as `lua.<k>` and `lua.<k>.ok`; `None` has none.
    pub present: Option<PresentDef>,
}

/// Why [`Skin::new`] refused a skin; `element` and `when` are indices.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("the skin has {n} elements, more than {MAX_ELEMENTS}")]
    TooManyElements { n: usize },
    #[error("the skin has {n} `when` timers, more than {MAX_WHEN_TIMERS}")]
    TooManyWhenTimers { n: usize },
    #[error("the skin's tracks have {n} keys, more than {MAX_SKIN_KEYS}")]
    TooManyKeys { n: usize },
    #[error("`when` timer {when}'s condition is not a bool")]
    WhenNotBool { when: usize },
    #[error("element {element}'s visibility is not a bool")]
    VisibleNotBool { element: usize },
    #[error("element {element}'s timer does not exist")]
    UnknownTimer { element: usize },
    #[error("element {element}'s track on a program timer loops or ends at or past 4 h")]
    ProgramTimerTrack { element: usize },
    #[error("element {element}'s image does not exist")]
    UnknownImage { element: usize },
    #[error("image {image}'s cycle timer does not exist")]
    UnknownImageTimer { image: usize },
    #[error("element {element} is outside `screen` in a fixed-canvas skin")]
    RegionInFixed { element: usize },
    #[error("element {element} fills the canvas outside `screen`")]
    FillOutsideScreen { element: usize },
    #[error("element {element}'s keys leave ±{MAX_COORD}")]
    Coordinates { element: usize },
    #[error(
        "element {element} follows the covers, which only an image, a number, a gauge, a text or a \
         cover paint may, and a moving cover's paint not the lift nor HIDDEN+'s paint HIDDEN+"
    )]
    UnsupportedFollow { element: usize },
    #[error("element {element}'s widget track moves, sizes or rotates it")]
    WidgetGeometry { element: usize },
    #[error("the skin draws the lanes twice")]
    LanesTwice,
    #[error("the lanes are outside `playfield` (`screen` in a fixed-canvas skin)")]
    LanesRegion,
    #[error("element {element} is bound to lanes outside its region")]
    Unbound { element: usize },
    #[error("the skin's lanes")]
    Lanes(#[source] LanesError),
    #[error("the skin's expressions")]
    Ops(#[source] CompileError),
    #[error("a widget table has {n} widgets, more than {MAX_ELEMENTS}")]
    TooManyWidgets { n: usize },
    #[error("the skin has {n} text elements, more than {MAX_TEXT_ELEMENTS}")]
    TooManyTexts { n: usize },
    #[error("element {element}'s widget does not exist")]
    UnknownWidget { element: usize },
    #[error("the widgets can push {n} quads a frame, more than {MAX_WIDGET_QUADS}")]
    TooManyWidgetQuads { n: u64 },
    #[error("the skin's presentation program")]
    Present(#[source] PresentError),
    #[error("a presentation program outside a play skin")]
    PresentKind,
    #[error(
        "an expression reads a presentation output the program lacks, at another type, or in a          `when` timer"
    )]
    OutputRead,
    #[error(
        "cover paint {paint}'s slider reads a presentation output that is no float, or one another slider reads"
    )]
    SliderOutput { paint: usize },
    #[error("judge element {element} is outside `judge` (`screen` in a fixed-canvas skin)")]
    JudgeRegion { element: usize },
    #[error("element {element}'s number")]
    Number {
        element: usize,
        #[source]
        error: NumberError,
    },
    #[error("element {element}'s gauge")]
    Gauge {
        element: usize,
        #[source]
        error: GaugeError,
    },
    #[error("element {element}'s judge")]
    Judge {
        element: usize,
        #[source]
        error: JudgeError,
    },
    #[error("element {element}'s text")]
    Text {
        element: usize,
        #[source]
        error: TextError,
    },
    #[error("the image choice tables have {n} entries, more than {MAX_IMAGE_CHOICE_REFS}")]
    TooManyImageChoiceRefs { n: usize },
    #[error("the glyph strips have {n} digit offsets, more than {MAX_DIGIT_OFFSETS}")]
    TooManyDigitOffsets { n: usize },
    #[error("image choice table {table}")]
    ImageChoice {
        table: usize,
        #[source]
        error: ImageChoiceError,
    },
    #[error("element {element} chooses its image and names a native size of its own")]
    ChoiceNative { element: usize },
    #[error("cover paint {paint}")]
    CoverPaint {
        paint: usize,
        #[source]
        error: CoverPaintError,
    },
    #[error("element {element}'s trimmed cover paint has a negative width or height")]
    CoverGate { element: usize },
    #[error("element {element} draws what a {kind} skin cannot")]
    SourceKind { element: usize, kind: ScreenKind },
    #[error("the song list")]
    SongList(#[source] SongListError),
    #[error("judge graph {graph}'s image does not exist")]
    JudgeGraphImage { graph: usize },
    #[error("graph element {element} names a native size: its graph sizes itself")]
    GraphNative { element: usize },
    #[error("gauge graph {graph}'s image does not exist")]
    GaugeGraphImage { graph: usize },
    #[error("timing distribution {graph}'s image does not exist")]
    TimingGraphImage { graph: usize },
    #[error("ratio bar {bar}")]
    RatioBar {
        bar: usize,
        #[source]
        error: RatioBarError,
    },
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Image(ImageRef),
    Lanes,
    Number(usize),
    Gauge(usize),
    Judge(usize),
    /// The text definition's index, and its layout.
    Text(usize, Text),
    /// The image choice table's index.
    ImageChoice(usize),
    /// The cover paint's index.
    CoverPaint(usize),
    SongList,
    /// The ratio bar's index, and its hit slot when it is a changeable
    /// slider.
    RatioBar(usize, Option<usize>),
    /// The judge graph's index.
    JudgeGraph(usize),
    /// The gauge graph's index.
    GaugeGraph(usize),
    /// The timing distribution's index.
    TimingGraph(usize),
}

#[derive(Clone, Debug)]
struct Element {
    kind: Kind,
    track: Track,
    /// The timer's slot: built-in, or `BUILTIN` plus the `when` index.
    slot: usize,
    visible: Option<Program>,
    key: RunKey,
    region: Region,
    /// The pivot's fractions of the rect.
    pivot: [f32; 2],
    bind: Bind,
    fill: Fill,
    native: Option<Native>,
    follow: Follow,
    angle: AngleFollow,
    /// What its visibility and its number's value or image selector need
    /// to be read while loading.
    loading: Need,
}

/// The runtime state a frame draws from: a play's, or the loading
/// runtime's.
struct Runtime<'a> {
    starts: Starts<'a>,
    state: State<'a>,
    text: &'a TextTable,
    bitmap: &'a [Option<GlyphLine>],
}

/// What a frame's drawing reads of its frame.
struct Scene<'a> {
    /// The frame's clock, which timers age on.
    now: TimeUs,
    /// The display clock, which the notes and bar lines follow.
    display: TimeUs,
    lanes: LaneView,
    /// Whether evaluation is complete.
    complete: bool,
    /// Whether HIDDEN+ is on, which its paint shows only while.
    hidden_enabled: bool,
    /// The turntable angle a turntable follower adds
    /// ([`AngleFollow::Turntable`]).
    turntable: i32,
    phase: Phase<'a>,
}

/// A play's frame or a loading one.
#[derive(Clone, Copy)]
enum Phase<'a> {
    /// A play: its notes, its latest judgment and the snapshot's shown
    /// gauge.
    Play {
        play: &'a Play,
        /// The frame's note facts, which the lanes draw each note's state
        /// from.
        notes: &'a NoteFacts,
        gauges: &'a [GaugeState],
        shown: usize,
    },
    /// The loading runtime: no chart, the gauge from the frame's
    /// descriptor, if any, and the chosen chart's data the frame gives
    /// ([`LoadingFrame::chosen`](crate::play::loading::LoadingFrame)).
    Loading {
        gauge: Option<LoadingGauge>,
        chosen: u8,
    },
    /// A selection frame: its rows, ratios and slider hits.
    Selection { sel: &'a Selection },
    /// A result frame: the view's state.
    Result { res: &'a ResultScreen },
    /// A decision frame: its timers and text alone.
    Decision,
}

/// A loaded skin.
#[derive(Clone, Debug)]
pub struct Skin {
    elements: Box<[Element]>,
    whens: Box<[Program]>,
    /// What each `when` condition needs to be read while loading.
    whens_need: Box<[Need]>,
    /// The verified presentation program, play only.
    present: Option<Present>,
    ops: SkinOps,
    sheet: Sheet,
    /// The sheet's identity and frame count, kept so a build never reads
    /// the sheet.
    sheet_id: u64,
    frame_count: usize,
    regions: Regions,
    lanes: Option<Lanes>,
    numbers: Box<[Number]>,
    gauges: Box<[Gauge]>,
    judges: Box<[Judge]>,
    texts: Box<[TextDef]>,
    image_choices: Box<[ImageChoice]>,
    cover_paints: Box<[CoverPaint]>,
    /// The text elements: the length of the table
    /// [`attach_text`](Skin::attach_text) takes.
    text_slots: u16,
    /// The quads one frame's widget elements can push, the lanes aside.
    widget_quads: u32,
    bomb_up_to: Option<Grade>,
    close: Option<TimeUs>,
    loadend: Option<TimeUs>,
    /// The screen the skin is made for.
    kind: ScreenKind,
    song_list: Option<SongList>,
    ratio_bars: Box<[RatioBar]>,
    judge_graphs: Box<[JudgeGraph]>,
    /// A result skin's declared timing; the default for another kind.
    result_timing: crate::result::ResultTiming,
    /// A decision skin's declared timing; the default for another kind.
    decision_timing: crate::decision::DecisionTiming,
    gauge_graphs: Box<[GaugeGraph]>,
    timing_graphs: Box<[TimingGraph]>,
    /// The slider elements that take a press.
    sliders: usize,
    /// The callbacks whose outputs a selection or result state carries
    /// after its fields ([`crate::output`]): the screen's runtime writes
    /// them (docs/stage3.md S7-07).
    outputs: usize,
    /// This skin's identity, which a selection, result or decision state
    /// it makes carries: a clone keeps it, another load takes the next.
    id: u64,
}

/// The next skin's identity.
static NEXT_SKIN: AtomicU64 = AtomicU64::new(0);

/// A table index from a definition's `u32`, past every table when it
/// does not fit.
fn index(i: u32) -> usize {
    usize::try_from(i).unwrap_or(usize::MAX)
}

/// The slot `timer` reads with `whens` `when` timers: a built-in slot, or
/// `BUILTIN` plus the `when` index.
fn slot_of(timer: TimerRef, whens: usize) -> Option<usize> {
    screen_slot_of(timer, whens, usize::from(BUILTIN))
}

/// [`slot_of`] over a screen's `builtin` built-in timers.
fn screen_slot_of(timer: TimerRef, whens: usize, builtin: usize) -> Option<usize> {
    match timer {
        TimerRef::Builtin(s) if usize::from(s) < builtin => Some(usize::from(s)),
        TimerRef::When(i) if usize::from(i) < whens => Some(builtin + usize::from(i)),
        TimerRef::Builtin(_) | TimerRef::When(_) | TimerRef::Output(_) => None,
    }
}

/// The slot an element's `timer` reads with `whens` `when` timers after
/// `builtin` built-in ones and the program `present`: [`screen_slot_of`],
/// or, for [`TimerRef::Output`] (V8), the timer of a timer callback after
/// every `when` timer. Every track sampled on a program timer, the
/// element's, an image choice's entry's and a cover paint's gate, must not
/// depend on how far past [`crate::MAX_ELAPSED_MS`] it is, since its
/// elapsed time is capped there ([`End::settles_by_cap`];
/// [`LoadError::ProgramTimerTrack`]).
fn element_slot_of(
    (e, element): (&ElementDef, usize),
    def: &SkinDef,
    (whens, builtin): (usize, usize),
    present: Option<&PresentDef>,
) -> Result<usize, LoadError> {
    let TimerRef::Output(k) = e.timer else {
        return screen_slot_of(e.timer, whens, builtin).ok_or(LoadError::UnknownTimer { element });
    };
    let cbs = present.map_or(&[][..], |p| &p.callbacks);
    let k = usize::from(k);
    if cbs.get(k).map(|c| c.conv) != Some(crate::present::Conv::Timer) {
        return Err(LoadError::UnknownTimer { element });
    }
    let rank = cbs
        .iter()
        .take(k)
        .filter(|c| c.conv == crate::present::Conv::Timer)
        .count();
    let entries = choice_of(def, e.source)
        .into_iter()
        .flat_map(|c| c.choices.iter().flatten().filter_map(|p| p.track.as_ref()));
    let gate = match e.source {
        Source::CoverPaint(p) => def
            .widgets
            .cover_paints
            .get(index(p))
            .and_then(|c| c.gate_track.as_ref()),
        _ => None,
    };
    if !std::iter::once(&e.track)
        .chain(entries)
        .chain(gate)
        .all(Track::settles_by_cap)
    {
        return Err(LoadError::ProgramTimerTrack { element });
    }
    Ok(builtin + whens + rank)
}

/// `deg` degrees as radians in `[0, 2π)`, reduced first so large key
/// values keep their precision.
pub(crate) fn radians(deg: f32) -> f32 {
    let r = f32_of(f64::from(deg).rem_euclid(360.0).to_radians());
    if r < std::f32::consts::TAU { r } else { 0.0 }
}

/// The image choice table `source` selects from: an image choice's, or a
/// choice-backed cover paint's.
fn choice_of(def: &SkinDef, source: Source) -> Option<&ImageChoiceDef> {
    let table = match source {
        Source::ImageChoice(i) => i,
        Source::CoverPaint(p) => match def.widgets.cover_paints.get(index(p))?.image {
            CoverImage::Choice(i) => i,
            CoverImage::Image(_) => return None,
        },
        _ => return None,
    };
    def.widgets.image_choices.get(index(table))
}

/// Whether a key of `t` leaves ±[`MAX_COORD`].
fn track_outside(t: &Track) -> bool {
    t.rects()
        .any(|r| r.iter().any(|v| !(-MAX_COORD..=MAX_COORD).contains(v)))
}

/// Whether a key of `t` leaves ±[`MAX_COORD`] on y when moved `up` up or
/// `down` down.
fn track_moved_outside(t: &Track, up: i64, down: i64) -> bool {
    let max = i64::from(MAX_COORD);
    t.rects().any(|[_, y, ..]| {
        let y = i64::from(y);
        y - up < -max || y + down > max
    })
}

/// Whether a key of `t` leaves ±[`MAX_COORD`] on x when moved `left` left
/// or `right` right.
fn track_moved_outside_x(t: &Track, left: i64, right: i64) -> bool {
    let max = i64::from(MAX_COORD);
    t.rects().any(|[x, ..]| {
        let x = i64::from(x);
        x - left < -max || x + right > max
    })
}

/// The places checks of [`Skin::new`], in element order.
fn check_places(def: &SkinDef) -> Result<(), LoadError> {
    let fixed = def.fit == Fit::Fixed;
    let (home, judge_home) = if fixed {
        (Region::Screen, Region::Screen)
    } else {
        (Region::Playfield, Region::Judge)
    };
    for (element, e) in def.elements.iter().enumerate() {
        let p = e.place;
        if fixed && p.region != Region::Screen {
            return Err(LoadError::RegionInFixed { element });
        }
        if p.fill != Fill::None && p.region != Region::Screen {
            return Err(LoadError::FillOutsideScreen { element });
        }
        if track_outside(&e.track) {
            return Err(LoadError::Coordinates { element });
        }
        let widget = matches!(
            e.source,
            Source::Lanes | Source::Judge(_) | Source::SongList
        );
        if widget && !e.track.is_still() {
            return Err(LoadError::WidgetGeometry { element });
        }
        if let Source::Judge(i) = e.source {
            let judge = |error| LoadError::Judge { element, error };
            if p.region != judge_home {
                return Err(LoadError::JudgeRegion { element });
            }
            if p.bind != Bind::None {
                return Err(judge(JudgeError::Bound));
            }
            let def = def.widgets.judges.get(index(i));
            if let Some(slot) = def.and_then(JudgeDef::mistimed_attachment) {
                return Err(judge(JudgeError::AttachmentTiming { slot }));
            }
            if let Some((part, _)) = def
                .into_iter()
                .flat_map(JudgeDef::tracks)
                .find(|(_, t)| track_outside(t))
            {
                return Err(judge(JudgeError::Coordinates { part }));
            }
        }
        if p.native.is_some() && choice_of(def, e.source).is_some() {
            return Err(LoadError::ChoiceNative { element });
        }
        if let Source::Text(_) = e.source {
            let text = |error| LoadError::Text { element, error };
            if p.bind != Bind::None {
                return Err(text(TextError::Bound));
            }
            if p.fill != Fill::None {
                return Err(text(TextError::Fill));
            }
            if e.track.rects().any(|[_, _, w, h]| h <= 0 || w < 0) {
                return Err(text(TextError::Size));
            }
        }
    }
    let mut widgets = def.elements.iter().filter(|e| e.source == Source::Lanes);
    let lanes_in = widgets.next().map(|e| e.place.region);
    if widgets.next().is_some() {
        return Err(LoadError::LanesTwice);
    }
    if lanes_in.is_some_and(|r| r != home) {
        return Err(LoadError::LanesRegion);
    }
    if let Some(element) = def
        .elements
        .iter()
        .position(|e| e.place.bind != Bind::None && lanes_in != Some(e.place.region))
    {
        return Err(LoadError::Unbound { element });
    }
    Ok(())
}

/// The follow checks of [`Skin::new`], in element order, with `reach` the
/// lanes' length rounded up (0 without lanes): only an image, a number, a
/// gauge, a text or a cover paint follows the covers, a moving cover's
/// paint never following the lift and HIDDEN+'s never HIDDEN+, and every key of a follower or a moving
/// paint, its gate's among them, and of a followed grade of a judge an
/// element draws, its attachment's among them, keeps its y within ±[`MAX_COORD`] when moved `reach`
/// either way; a slider paint's keys keep it, their x or y, moved by its
/// range toward its direction as well, and a linked trim line stays
/// within it raised by `reach`.
fn check_follows(def: &SkinDef, reach: i64) -> Result<(), LoadError> {
    let moved_out = |t: &Track| track_moved_outside(t, reach, reach);
    for (element, e) in def.elements.iter().enumerate() {
        let follow = e.place.follow;
        // The turntable turns an image or a number; a text accepts it and
        // stays unrotated.
        if e.place.angle != AngleFollow::None
            && !matches!(
                e.source,
                Source::Image(_) | Source::ImageChoice(_) | Source::Number(_) | Source::Text(_)
            )
        {
            return Err(LoadError::UnsupportedFollow { element });
        }
        let paint = match e.source {
            Source::CoverPaint(p) => def.widgets.cover_paints.get(index(p)),
            _ => None,
        };
        let slider = paint.and_then(|p| p.slider);
        // The slider's reach toward each side: up, down, left, right.
        let [s_up, s_down, s_left, s_right] = slider.map_or([0; 4], |s| {
            let r = i64::from(s.range);
            match s.direction {
                SliderDirection::Up => [r, 0, 0, 0],
                SliderDirection::Down => [0, r, 0, 0],
                SliderDirection::Left => [0, 0, r, 0],
                SliderDirection::Right => [0, 0, 0, r],
            }
        });
        let motion = paint.map_or(CoverMotion::None, |p| p.motion);
        // A moving paint already rises with the lift, and HIDDEN+'s with
        // HIDDEN+ as well.
        if motion != CoverMotion::None
            && (follow.lift || (motion == CoverMotion::Hidden && follow.hidden))
        {
            return Err(LoadError::UnsupportedFollow { element });
        }
        if follow != Follow::NONE || slider.is_some() || motion != CoverMotion::None {
            if matches!(e.source, Source::Lanes | Source::Judge(_)) {
                return Err(LoadError::UnsupportedFollow { element });
            }
            let moves = follow != Follow::NONE || motion != CoverMotion::None;
            let follows = if moves { reach } else { 0 };
            let (up, down) = (follows + s_up, follows + s_down);
            let out = |t: &Track| {
                track_moved_outside(t, up, down) || track_moved_outside_x(t, s_left, s_right)
            };
            if out(&e.track)
                || choice_of(def, e.source)
                    .into_iter()
                    .flat_map(ImageChoiceDef::tracks)
                    .chain(paint.and_then(|p| p.gate_track.as_ref()))
                    .any(out)
            {
                return Err(LoadError::Coordinates { element });
            }
        }
        if let Some(CoverTrim::Lift(y)) = paint.map(|p| p.trim)
            && i64::from(y) - reach < -i64::from(MAX_COORD)
        {
            return Err(LoadError::Coordinates { element });
        }
        if let Source::Judge(i) = e.source
            && let Some(part) = def.widgets.judges.get(index(i)).and_then(|j| {
                JudgeSlot::ALL
                    .into_iter()
                    .zip(&j.grades)
                    .find_map(|(s, g)| {
                        let g = g.as_ref().filter(|g| g.follow != Follow::NONE)?;
                        if moved_out(&g.track) {
                            Some(JudgePart::Grade(s))
                        } else if g.attachment.as_ref().is_some_and(moved_out) {
                            Some(JudgePart::Attachment(s))
                        } else {
                            None
                        }
                    })
            })
        {
            let error = JudgeError::Coordinates { part };
            return Err(LoadError::Judge { element, error });
        }
    }
    Ok(())
}

/// The resource checks of [`Skin::new`] for one element's widget, whose
/// index is valid: every image exists, glyph images hold whole sets, and
/// a text's font exists. `frames` gives an image's frame count, `None`
/// for a missing image; the sheet has `fonts` font files and `bitmaps`
/// bitmap fonts.
fn check_widget_images(
    element: usize,
    source: Source,
    w: &WidgetDefs,
    frames: &impl Fn(u32) -> Option<u16>,
    [fonts, bitmaps]: [usize; 2],
) -> Result<(), LoadError> {
    match source {
        Source::Number(i) => {
            if let Some(n) = w.numbers.get(index(i)) {
                n.digits
                    .check_image(frames(n.digits.image))
                    .map_err(|error| LoadError::Number { element, error })?;
            }
        }
        Source::Gauge(i) => {
            if let Some((look, part, below, _)) = w
                .gauges
                .get(index(i))
                .into_iter()
                .flat_map(GaugeDef::images)
                .find(|&(.., image)| frames(image).is_none())
            {
                let error = GaugeError::UnknownImage { look, part, below };
                return Err(LoadError::Gauge { element, error });
            }
        }
        Source::Judge(i) => {
            let Some(j) = w.judges.get(index(i)) else {
                return Ok(());
            };
            let judge = |error| LoadError::Judge { element, error };
            for (slot, g) in JudgeSlot::ALL.into_iter().zip(&j.grades) {
                if g.as_ref().is_some_and(|g| frames(g.image).is_none()) {
                    return Err(judge(JudgeError::UnknownImage { slot }));
                }
            }
            for (slot, c) in JudgeSlot::ALL.into_iter().zip(&j.combos) {
                if let Some(c) = c {
                    c.digits
                        .check_image(frames(c.digits.image))
                        .map_err(|error| judge(JudgeError::Combo { slot, error }))?;
                }
            }
        }
        Source::Text(i) => {
            let known = match w.texts.get(index(i)).map(|t| t.font) {
                Some(FontRef::File(f)) => index(f) < fonts,
                Some(FontRef::Bitmap(f)) => index(f) < bitmaps,
                Some(FontRef::Default) | None => true,
            };
            if !known {
                let error = TextError::UnknownFont;
                return Err(LoadError::Text { element, error });
            }
        }
        Source::Image(_)
        | Source::Lanes
        | Source::ImageChoice(_)
        | Source::CoverPaint(_)
        | Source::SongList
        | Source::RatioBar(_)
        | Source::JudgeGraph(_)
        | Source::GaugeGraph(_)
        | Source::TimingGraph(_) => {}
    }
    Ok(())
}

/// The widget parameter checks of [`Skin::new`] for one element's
/// widget, whose index is valid.
fn check_widget_params(element: usize, source: Source, w: &WidgetDefs) -> Result<(), LoadError> {
    match source {
        Source::Number(i) => {
            if let Some(n) = w.numbers.get(index(i)) {
                n.digits
                    .check()
                    .map_err(|error| LoadError::Number { element, error })?;
            }
        }
        Source::Gauge(i) => {
            if let Some(g) = w.gauges.get(index(i)) {
                g.check()
                    .map_err(|error| LoadError::Gauge { element, error })?;
            }
        }
        Source::Judge(i) => {
            let Some(j) = w.judges.get(index(i)) else {
                return Ok(());
            };
            let judge = |error| LoadError::Judge { element, error };
            if j.grades.iter().all(Option::is_none) {
                return Err(judge(JudgeError::Empty));
            }
            for (slot, c) in JudgeSlot::ALL.into_iter().zip(&j.combos) {
                if let Some(c) = c {
                    c.digits
                        .check()
                        .map_err(|error| judge(JudgeError::Combo { slot, error }))?;
                }
            }
        }
        Source::Text(i) => {
            if let Some(TextValue::Literal(l) | TextValue::FieldOr { empty: l, .. }) =
                w.texts.get(index(i)).map(|t| &t.value)
                && !text::literal_ok(l)
            {
                let error = TextError::Literal;
                return Err(LoadError::Text { element, error });
            }
        }
        Source::Image(_)
        | Source::Lanes
        | Source::ImageChoice(_)
        | Source::CoverPaint(_)
        | Source::SongList
        | Source::RatioBar(_)
        | Source::JudgeGraph(_)
        | Source::GaugeGraph(_)
        | Source::TimingGraph(_) => {}
    }
    Ok(())
}

/// The widget quads one frame of `source`'s element can push at worst:
/// 0 for an image, a cover paint or the lanes, which `quad_capacity`
/// counts apart; for a
/// text, 1, or with a bitmap font one per character: a literal's
/// characters, or [`MAX_TEXT_BYTES`] for a field; for a judge graph, its
/// background's and one per channel of every bin; for a gauge graph or a
/// timing distribution, its pieces at worst.
fn worst(source: Source, w: &WidgetDefs, sel: &SelectionDefs, res: &ResultDefs) -> u32 {
    match source {
        Source::Image(_)
        | Source::Lanes
        | Source::ImageChoice(_)
        | Source::CoverPaint(_)
        | Source::RatioBar(_) => 0,
        Source::SongList => sel.song_list.as_ref().map_or(0, SongListDef::quads),
        Source::Text(i) => w.texts.get(index(i)).map_or(0, TextDef::quads),
        Source::Number(i) => w
            .numbers
            .get(index(i))
            .map_or(0, |n| u32::from(n.digits.cells)),
        Source::Gauge(i) => w.gauges.get(index(i)).map_or(0, GaugeDef::quads),
        Source::Judge(i) => w.judges.get(index(i)).map_or(0, JudgeDef::quads),
        Source::JudgeGraph(i) => res
            .judge_graphs
            .get(index(i))
            .map_or(0, JudgeGraphDef::quads),
        Source::GaugeGraph(i) => res
            .gauge_graphs
            .get(index(i))
            .map_or(0, GaugeGraphDef::quads),
        Source::TimingGraph(i) => res
            .timing_graphs
            .get(index(i))
            .map_or(0, TimingGraphDef::quads),
    }
}

/// The regions' boxes from the elements' keys and the lanes' rect.
fn regions(def: &SkinDef, lanes: Option<&Lanes>, sel: &SelectionDefs) -> Regions {
    let mut out = Regions {
        fit: def.fit,
        defs: def.regions,
        fixed: [None; Region::COUNT],
        authored: [None; Region::COUNT],
        lanes: [None; LANE_COUNT],
        span: None,
        lanes_in: None,
    };
    for e in &def.elements {
        let r = e.place.region.index();
        let b: Option<Bounds> = match e.source {
            Source::Lanes => {
                out.lanes_in = Some(e.place.region);
                let rect = lanes.map(Lanes::rect);
                if let Some(rect) = rect {
                    out.span = Some(union(out.span, rect));
                }
                rect
            }
            Source::Image(_)
            | Source::Gauge(_)
            | Source::Number(_)
            | Source::Judge(_)
            | Source::Text(_)
            | Source::ImageChoice(_)
            | Source::CoverPaint(_)
            | Source::SongList
            | Source::RatioBar(_)
            | Source::JudgeGraph(_)
            | Source::GaugeGraph(_)
            | Source::TimingGraph(_) => {
                let b = match e.source {
                    Source::SongList => sel.song_list.as_ref().and_then(|l| {
                        l.placements
                            .iter()
                            .flat_map(|p| p.iter())
                            .filter_map(|p| p.part.track.bounds())
                            .reduce(|a, b| union(Some(a), b))
                    }),
                    Source::Number(i) => def
                        .widgets
                        .numbers
                        .get(index(i))
                        .and_then(|n| n.digits.bounds(&e.track)),
                    Source::Judge(i) => def.widgets.judges.get(index(i)).and_then(JudgeDef::bounds),
                    // Every present entry's track, so a change of entry
                    // moves no region; a paint's authored keys, neither
                    // its following, motion, slider, gate and trim not
                    // counted.
                    Source::ImageChoice(_) | Source::CoverPaint(_) => {
                        match choice_of(def, e.source) {
                            Some(c) => choice_bounds(c, &e.track),
                            None => e.track.bounds(),
                        }
                    }
                    // A text's box only: spilled ink does not count.
                    Source::Image(_)
                    | Source::Gauge(_)
                    | Source::Text(_)
                    | Source::Lanes
                    | Source::RatioBar(_)
                    | Source::JudgeGraph(_)
                    | Source::GaugeGraph(_)
                    | Source::TimingGraph(_) => e.track.bounds(),
                };
                if let Some(b) = b {
                    let slot = match e.place.bind {
                        Bind::None => out.fixed.get_mut(r),
                        Bind::Lane(l) => out.lanes.get_mut(l.index()),
                        Bind::Lanes => Some(&mut out.span),
                    };
                    if let Some(slot) = slot {
                        *slot = Some(union(*slot, b));
                    }
                }
                b
            }
        };
        if let (Some(b), Some(slot)) = (b, out.authored.get_mut(r)) {
            *slot = Some(union(*slot, b));
        }
    }
    out
}

/// The union of the bounds of every present entry's effective track:
/// its own, or `track`, the element's.
fn choice_bounds(c: &ImageChoiceDef, track: &Track) -> Option<Bounds> {
    let own = c.uses_element_track().then(|| track.bounds()).flatten();
    c.tracks()
        .filter_map(Track::bounds)
        .fold(own, |acc, b| Some(union(acc, b)))
}

/// The table checks of [`Skin::new`], every table in order, referenced or
/// not: each selector is an int, each table has an entry, and each present
/// entry's span lies inside an existing image, its cycle's period and
/// timer are valid and its track's keys lie within ±[`MAX_COORD`].
fn check_image_choices(
    w: &WidgetDefs,
    frames: &impl Fn(u32) -> Option<u16>,
    whens: usize,
    builtin: usize,
) -> Result<(), LoadError> {
    for (table, d) in w.image_choices.iter().enumerate() {
        let fail = |error| Err(LoadError::ImageChoice { table, error });
        if d.value.ty() != Ty::Int {
            return fail(ImageChoiceError::NotInt);
        }
        if d.choices.is_empty() {
            return fail(ImageChoiceError::Empty);
        }
        for (choice, c) in d.choices.iter().enumerate() {
            let Some(c) = c else {
                continue;
            };
            let Some(n) = frames(c.span.image) else {
                return fail(ImageChoiceError::UnknownImage { choice });
            };
            let end = u32::from(c.span.first) + u32::from(c.span.frames);
            if c.span.frames == 0 || end > u32::from(n) {
                return fail(ImageChoiceError::Span { choice });
            }
            if let Some(cycle) = c.cycle {
                if !(1..=MAX_KEY_US).contains(&cycle.period.0) {
                    return fail(ImageChoiceError::Cycle { choice });
                }
                if screen_slot_of(cycle.timer, whens, builtin).is_none() {
                    return fail(ImageChoiceError::UnknownTimer { choice });
                }
            }
            if c.track.as_ref().is_some_and(track_outside) {
                return fail(ImageChoiceError::Coordinates { choice });
            }
        }
    }
    Ok(())
}

/// The paint checks of [`Skin::new`], every definition in order,
/// referenced or not: its image or choice table exists, its slider's range
/// is at most [`MAX_COORD`] and comes without a motion, a trim or a gate
/// track, a gate track comes with a trim, its keys lie within
/// ±[`MAX_COORD`] with no negative width or height, and a trim base lies
/// within ±[`MAX_COORD`].
fn check_cover_paints(w: &WidgetDefs, images: usize) -> Result<(), LoadError> {
    let max = i64::from(MAX_COORD);
    for (paint, p) in w.cover_paints.iter().enumerate() {
        let image = match p.image {
            CoverImage::Image(i) if index(i) >= images => Some(CoverPaintError::UnknownImage),
            CoverImage::Choice(i) if index(i) >= w.image_choices.len() => {
                Some(CoverPaintError::UnknownChoice)
            }
            CoverImage::Image(_) | CoverImage::Choice(_) => None,
        };
        let range = p.slider.is_some_and(|s| i64::from(s.range) > max);
        let slider_moves = p.slider.is_some()
            && (p.motion != CoverMotion::None
                || p.trim != CoverTrim::None
                || p.gate_track.is_some());
        let gate = p.gate_track.as_ref();
        let trim = match p.trim {
            CoverTrim::None => 0,
            CoverTrim::Fixed(y) | CoverTrim::Lift(y) => i64::from(y),
        };
        let error = image
            .or(range.then_some(CoverPaintError::Range))
            .or(slider_moves.then_some(CoverPaintError::SliderMotion))
            .or((gate.is_some() && p.trim == CoverTrim::None)
                .then_some(CoverPaintError::GateWithoutTrim))
            .or(gate
                .is_some_and(track_outside)
                .then_some(CoverPaintError::GateCoordinates))
            .or(gate
                .is_some_and(|t| t.rects().any(|[.., w, h]| w < 0 || h < 0))
                .then_some(CoverPaintError::GateSize))
            .or((trim.abs() > max).then_some(CoverPaintError::TrimRange));
        if let Some(error) = error {
            return Err(LoadError::CoverPaint { paint, error });
        }
    }
    Ok(())
}

/// The gate checks of [`Skin::new`], in element order: a trimmed paint
/// without a gate track is gated by its effective paint tracks, the
/// element's and its choice's entries', none of which has a key of
/// negative width or height.
fn check_cover_gates(def: &SkinDef) -> Result<(), LoadError> {
    let negative = |t: &Track| t.rects().any(|[.., w, h]| w < 0 || h < 0);
    for (element, e) in def.elements.iter().enumerate() {
        let Source::CoverPaint(p) = e.source else {
            continue;
        };
        let Some(p) = def.widgets.cover_paints.get(index(p)) else {
            continue;
        };
        if p.trim == CoverTrim::None || p.gate_track.is_some() {
            continue;
        }
        if negative(&e.track)
            || choice_of(def, e.source)
                .into_iter()
                .flat_map(ImageChoiceDef::tracks)
                .any(negative)
        {
            return Err(LoadError::CoverGate { element });
        }
    }
    Ok(())
}

/// The bar group checks of [`Skin::new`], each over every group in order,
/// with `whens` `when` timers, `region` the lanes element's and `reach`
/// the lanes' length rounded up: every timer exists, every visibility is
/// a bool, every key lies within ±[`MAX_COORD`], every group sits in
/// `region` without a fill, and a follower's keys keep their y within it
/// moved `reach` either way.
fn check_bar_groups(
    l: &LanesDef,
    whens: usize,
    region: Option<Region>,
    reach: i64,
) -> Result<(), LoadError> {
    let fail = |group, error| Err(LoadError::Lanes(LanesError::BarGroup { group, error }));
    let groups = &l.bar_groups;
    if let Some(g) = groups
        .iter()
        .position(|g| slot_of(g.timer, whens).is_none())
    {
        return fail(g, BarGroupError::UnknownTimer);
    }
    if let Some(g) = groups
        .iter()
        .position(|g| g.visible.as_ref().is_some_and(|v| v.ty() != Ty::Bool))
    {
        return fail(g, BarGroupError::VisibleNotBool);
    }
    if let Some(g) = groups.iter().position(|g| track_outside(&g.track)) {
        return fail(g, BarGroupError::Coordinates);
    }
    for (g, b) in groups.iter().enumerate() {
        if Some(b.place.region) != region {
            return fail(g, BarGroupError::Region);
        }
        if b.place.fill != Fill::None {
            return fail(g, BarGroupError::Fill);
        }
    }
    if let Some(g) = groups
        .iter()
        .position(|g| g.place.follow != Follow::NONE && track_moved_outside(&g.track, reach, reach))
    {
        return fail(g, BarGroupError::Coordinates);
    }
    Ok(())
}

/// Whether `source` is a text showing a selection screen's string.
fn selection_text(def: &SkinDef, source: Source) -> bool {
    match source {
        Source::Text(i) => def
            .widgets
            .texts
            .get(index(i))
            .is_some_and(|t| matches!(t.value, TextValue::Selection(_))),
        _ => false,
    }
}

/// The kind checks of [`Skin::new`], [`Skin::new_selection`],
/// [`Skin::new_result`] and [`Skin::new_decision`], first: a play skin
/// draws no song list, ratio bar, graph or selection text; a selection
/// skin no lanes, gauge, judge, cover paint or graph; a result skin none
/// of these but its graphs; a decision skin only images, image choices
/// whose selector is a constant, and chart text.
fn check_kind(def: &SkinDef, kind: ScreenKind) -> Result<(), LoadError> {
    for (element, e) in def.elements.iter().enumerate() {
        let ok = match kind {
            ScreenKind::Decision => match e.source {
                Source::Image(_) => true,
                Source::ImageChoice(i) => def
                    .widgets
                    .image_choices
                    .get(index(i))
                    .is_none_or(|c| c.value.constant().is_some()),
                Source::Text(_) => !selection_text(def, e.source),
                Source::Lanes
                | Source::Number(_)
                | Source::Gauge(_)
                | Source::Judge(_)
                | Source::CoverPaint(_)
                | Source::SongList
                | Source::RatioBar(_)
                | Source::JudgeGraph(_)
                | Source::GaugeGraph(_)
                | Source::TimingGraph(_) => false,
            },
            ScreenKind::Selection => !matches!(
                e.source,
                Source::Lanes
                    | Source::Gauge(_)
                    | Source::Judge(_)
                    | Source::CoverPaint(_)
                    | Source::JudgeGraph(_)
                    | Source::GaugeGraph(_)
                    | Source::TimingGraph(_)
            ),
            ScreenKind::Result => {
                !selection_text(def, e.source)
                    && !matches!(
                        e.source,
                        Source::Lanes
                            | Source::Gauge(_)
                            | Source::Judge(_)
                            | Source::CoverPaint(_)
                            | Source::SongList
                            | Source::RatioBar(_)
                    )
            }
            ScreenKind::Play => {
                !selection_text(def, e.source)
                    && !matches!(
                        e.source,
                        Source::SongList
                            | Source::RatioBar(_)
                            | Source::JudgeGraph(_)
                            | Source::GaugeGraph(_)
                            | Source::TimingGraph(_)
                    )
            }
        };
        if !ok {
            return Err(LoadError::SourceKind { element, kind });
        }
    }
    Ok(())
}

/// The selection widget checks of [`Skin::new_selection`]: the song list's
/// placements, center, parts' timers, visibilities, keys, images, fonts and
/// strips, and each ratio bar's image and range. `frames` gives an
/// image's frame count; the timers are `builtin` built-ins and `whens`.
fn check_selection_widgets(
    sel: &SelectionDefs,
    frames: &impl Fn(u32) -> Option<u16>,
    whens: usize,
    builtin: usize,
    sheet: &Sheet,
) -> Result<(), LoadError> {
    if let Some(l) = &sel.song_list {
        let fail = |e| Err(LoadError::SongList(e));
        let n = l.placements.len();
        if n == 0 {
            return fail(SongListError::NoPlacements);
        }
        if n > MAX_PLACEMENTS {
            return fail(SongListError::TooManyPlacements { n });
        }
        if usize::from(l.center) >= n {
            return fail(SongListError::Center { center: l.center });
        }
        for p in l.parts() {
            if screen_slot_of(p.timer, whens, builtin).is_none() {
                return fail(SongListError::UnknownTimer);
            }
            if p.visible.as_ref().is_some_and(|v| v.ty() != Ty::Bool) {
                return fail(SongListError::VisibleNotBool);
            }
            if track_outside(&p.track) {
                return fail(SongListError::Coordinates);
            }
        }
        if l.images().any(|i| frames(i).is_none()) {
            return fail(SongListError::UnknownImage);
        }
        let [fonts, bitmaps] = [sheet.font_count(), sheet.bitmap_font_count()];
        for t in l.text.iter().flatten() {
            let known = match t.font {
                FontRef::File(f) => index(f) < fonts,
                FontRef::Bitmap(f) => index(f) < bitmaps,
                FontRef::Default => true,
            };
            if !known {
                return fail(SongListError::UnknownFont);
            }
            if t.part.track.rects().any(|[_, _, w, h]| h <= 0 || w < 0) {
                return fail(SongListError::TextSize);
            }
        }
        for v in l.level.iter().flatten() {
            let level = |e| LoadError::SongList(SongListError::Level(e));
            v.digits
                .check_image(frames(v.digits.image))
                .map_err(level)?;
            v.digits.check().map_err(level)?;
        }
    }
    for (bar, b) in sel.ratio_bars.iter().enumerate() {
        let error = if frames(b.image).is_none() {
            Some(RatioBarError::UnknownImage)
        } else if matches!(b.form, RatioForm::Slider { range, .. } if range > MAX_COORD.unsigned_abs())
        {
            Some(RatioBarError::Range)
        } else {
            None
        };
        if let Some(error) = error {
            return Err(LoadError::RatioBar { bar, error });
        }
    }
    Ok(())
}

/// The song list `l` as a build draws it, its images from `image` and its
/// timers' slots from `slot`.
fn song_list_of(
    l: SongListDef,
    image: &impl Fn(u32) -> ImageRef,
    slot: &impl Fn(TimerRef) -> usize,
) -> SongList {
    let part = |p: PartDef| {
        let s = slot(p.timer);
        (p, s)
    };
    let picture = |v: ImagePartDef| {
        let (p, s) = part(v.part);
        Part::new(p, s, image(v.image))
    };
    SongList {
        placements: l
            .placements
            .into_iter()
            .map(|arms| {
                arms.map(|a: PlacementDef| {
                    let (p, s) = part(a.part);
                    Part::new(p, s, a.images.map(|i| i.map(image)))
                })
            })
            .collect(),
        center: usize::from(l.center),
        text: l.text.map(|t| {
            t.map(|t: RowTextDef| {
                let (p, s) = part(t.part);
                let bitmap = match t.font {
                    FontRef::Bitmap(f) => Some(f),
                    FontRef::Default | FontRef::File(_) => None,
                };
                let text = Text {
                    slot: 0,
                    align: t.align,
                    overflow: t.overflow,
                    bitmap,
                };
                Part::new(p, s, (text, t.font))
            })
        }),
        level: l.level.map(|v| {
            v.map(|v: LevelDef| {
                let (p, s) = part(v.part);
                Part::new(p, s, Digits::new(v.digits, image))
            })
        }),
        lamp: l.lamp.map(|v| v.map(picture)),
        label: l.label.map(|v| v.map(picture)),
    }
}

/// The lines a selection skin rasterizes for a view: its text elements',
/// by text slot, and its song list's rows', by row slot
/// ([`Skin::selection_text_requests`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelectionRequests {
    pub texts: Vec<TextRequest>,
    pub rows: Vec<TextRequest>,
}

/// Draws ratio bar `b` with `image` at `v` over `rect`: the frame the
/// image's cycle shows, cut to or moved by `v`.
fn draw_ratio(
    b: &RatioBarDef,
    image: ImageRef,
    v: crate::selection::Ratio,
    rect: [f32; 4],
    brush: &Brush,
    at: &Frame<'_>,
    out: &mut QuadBatch,
) {
    let Some(f) = image.frame(at.starts, at.now, at.uvs) else {
        return;
    };
    let fr = ratio_bar::fraction(v);
    match b.form {
        RatioForm::Bar { vertical } => {
            let (r, f) = ratio_bar::cut(vertical, rect, f, fr);
            brush.push(out, r, f, image.filter);
        }
        RatioForm::Slider {
            direction, range, ..
        } => {
            brush.push(
                out,
                ratio_bar::slide(direction, range, rect, fr),
                f,
                image.filter,
            );
        }
    }
}

/// A magnification as an exact fraction.
#[derive(Clone, Copy)]
struct Ratio(u128, u128);

impl Ratio {
    fn mul(self, o: Self) -> Self {
        Self(self.0 * o.0, self.1 * o.1)
    }

    fn gt(self, o: Self) -> bool {
        self.0 * o.1 > o.0 * self.1
    }

    fn max(self, o: Self) -> Self {
        if o.gt(self) { o } else { self }
    }

    fn min(self, o: Self) -> Self {
        if o.gt(self) { self } else { o }
    }
}

impl Skin {
    /// Loads a skin.
    ///
    /// Checks, in order: the counts (elements, `when` timers, each widget
    /// table, text elements, keys across all tracks, judge parts' and
    /// grades' attachment tracks included); every `when` condition and visibility is a bool, judge
    /// parts' included, and every number's value an int; every timer
    /// exists, judge parts' included; every element's widget, every
    /// element's, widget's and lane part's image exists, glyph images hold
    /// whole sets, every text's font file exists, every image choice
    /// table and cover paint is sound, referenced or not, and every
    /// image's cycle timer exists; the places (regions under a fixed canvas, fills
    /// outside `screen`, key coordinates within ±[`MAX_COORD`], widget
    /// tracks that move, judges in `judge` and unbound with each grade's
    /// attachment timed as the grade, texts unbound,
    /// without a fill and with every key's height positive and width not
    /// negative, one lanes widget in its region, bindings beside it); the
    /// widgets' parameters (a literal's length and characters); the
    /// lanes' definition and geometry; the trimmed paints' gates (no
    /// negative width or height); the followers (only an image, a number,
    /// a gauge, a text or a cover paint, or a judge's grade, follows the
    /// covers, a moving cover's paint SUDDEN+ alone, each key's y, a moving
    /// paint's and its gate's among them, within ±[`MAX_COORD`] moved the
    /// lanes' length either way, a slider paint's moved down by its range
    /// as well, and a linked trim line raised by the lanes' length); the total
    /// operations, each `when` charged once however many elements share
    /// it, then each visibility in element order, then per element its
    /// number's value, its image choice's selector or its judge parts'
    /// visibilities; and the widgets'
    /// quads within [`MAX_WIDGET_QUADS`]. So the per-frame cost is known
    /// here. Then each region's box is computed from its elements' keys.
    ///
    /// The widget checks past the table counts and the key count cover the
    /// definitions an element references; an unreferenced definition is
    /// not checked and never drawn, its missing images and timers falling
    /// back to nothing and off. Image choice tables are the exception:
    /// every table is checked, referenced or not.
    ///
    /// These counts check what was built. A front end (an importer, the
    /// authored format's loader) enforces the same element, widget, `when`
    /// and key limits cumulatively while it builds the input, before it
    /// compiles or allocates each item.
    ///
    /// # Errors
    ///
    /// The first check that fails.
    pub fn new(def: SkinDef) -> Result<Self, LoadError> {
        Self::load(
            def,
            ScreenKind::Play,
            SelectionDefs::default(),
            ResultDefs::default(),
        )
    }

    /// Loads a selection skin: the checks of [`new`](Self::new) over the
    /// selection schema and its timers, and its widgets in `widgets`. An
    /// element may draw an image, an image choice, a number, a text, the
    /// song list or a ratio bar; the lanes, a gauge, a judge or a cover
    /// paint is refused ([`LoadError::SourceKind`]). The song list has 1 to
    /// [`MAX_PLACEMENTS`] placements and its center among them, every
    /// part's image, timer, font and strip exists, each visibility is a
    /// bool and each key lies within ±[`MAX_COORD`]; every ratio bar's
    /// image exists and a slider's range is at most [`MAX_COORD`]. Each
    /// song list element is charged its parts' visibilities once per slot,
    /// and its worst quads per slot.
    ///
    /// # Errors
    ///
    /// The first check that fails.
    pub fn new_selection(def: SkinDef, widgets: SelectionDefs) -> Result<Self, LoadError> {
        Self::load(def, ScreenKind::Selection, widgets, ResultDefs::default())
    }

    /// Loads a result skin: the checks of [`new`](Self::new) over the
    /// result schema and its timers, and its widgets in `widgets`. An
    /// element may draw an image, an image choice, a number, a text of the
    /// chart, a judge graph, a gauge graph or a timing distribution; the
    /// lanes, a gauge, a judge, a cover paint, the song list, a ratio bar
    /// or a selection text is refused ([`LoadError::SourceKind`]). Every
    /// graph's images exist, no graph element names a native size (a judge
    /// graph and a distribution fit by their [`GraphFit`], a gauge graph's
    /// texture is its rect), and each graph element is charged its worst
    /// quads.
    ///
    /// # Errors
    ///
    /// The first check that fails.
    pub fn new_result(def: SkinDef, widgets: ResultDefs) -> Result<Self, LoadError> {
        Self::load(def, ScreenKind::Result, SelectionDefs::default(), widgets)
    }

    /// Loads a decision skin: the checks of [`new`](Self::new) over the
    /// decision schema ([`DECISION`](crate::DECISION), no field) and its
    /// timers, with what only a decision skin has in `defs`. An element
    /// may draw an image, an image choice whose selector is a constant,
    /// or a text of the chart; the lanes, a number, a gauge, a judge, a
    /// cover paint, the song list, a ratio bar, a graph, a selection text
    /// or an image choice whose selector is not a constant is refused
    /// ([`LoadError::SourceKind`]).
    ///
    /// # Errors
    ///
    /// The first check that fails.
    pub fn new_decision(def: SkinDef, defs: DecisionDefs) -> Result<Self, LoadError> {
        let mut skin = Self::load(
            def,
            ScreenKind::Decision,
            SelectionDefs::default(),
            ResultDefs::default(),
        )?;
        skin.decision_timing = defs.timing;
        Ok(skin)
    }

    /// The checks of [`new`](Self::new) for a skin of `kind`, with the
    /// selection widgets `sel` and the result widgets `res`.
    #[expect(clippy::too_many_lines, reason = "one ordered list of checks")]
    fn load(
        mut def: SkinDef,
        kind: ScreenKind,
        sel: SelectionDefs,
        res: ResultDefs,
    ) -> Result<Self, LoadError> {
        let present = def.present.take();
        if present.is_some() && kind != ScreenKind::Play {
            return Err(LoadError::PresentKind);
        }
        let builtin = usize::from(match kind {
            ScreenKind::Selection => selection_timer::BUILTIN,
            ScreenKind::Result => result_timer::BUILTIN,
            ScreenKind::Decision => decision_timer::BUILTIN,
            ScreenKind::Play => BUILTIN,
        });
        let elements = &def.elements;
        let whens = &def.whens;
        let w = &def.widgets;
        if elements.len() > MAX_ELEMENTS {
            return Err(LoadError::TooManyElements { n: elements.len() });
        }
        if let Some(n) = sel
            .song_list
            .as_ref()
            .map(|l| l.placements.len())
            .filter(|&n| n > MAX_PLACEMENTS)
        {
            return Err(LoadError::SongList(SongListError::TooManyPlacements { n }));
        }
        check_kind(&def, kind)?;
        if whens.len() > MAX_WHEN_TIMERS {
            return Err(LoadError::TooManyWhenTimers { n: whens.len() });
        }
        for n in [
            w.numbers.len(),
            w.gauges.len(),
            w.judges.len(),
            w.texts.len(),
            w.image_choices.len(),
            w.cover_paints.len(),
            sel.ratio_bars.len(),
            res.judge_graphs.len(),
            res.gauge_graphs.len(),
            res.timing_graphs.len(),
        ] {
            if n > MAX_ELEMENTS {
                return Err(LoadError::TooManyWidgets { n });
            }
        }
        let groups = def.lanes.as_ref().map_or(0, |l| l.bar_groups.len());
        if groups > MAX_BAR_GROUPS {
            return Err(LoadError::Lanes(LanesError::TooManyBarGroups { n: groups }));
        }
        let n = elements
            .iter()
            .filter(|e| matches!(e.source, Source::Text(_)))
            .count();
        if n > MAX_TEXT_ELEMENTS {
            return Err(LoadError::TooManyTexts { n });
        }
        let refs = w
            .image_choices
            .iter()
            .fold(0_usize, |n, c| n.saturating_add(c.choices.len()));
        if refs > MAX_IMAGE_CHOICE_REFS {
            return Err(LoadError::TooManyImageChoiceRefs { n: refs });
        }
        let digit_offsets = w
            .numbers
            .iter()
            .map(|n| &n.digits)
            .chain(
                w.judges
                    .iter()
                    .flat_map(|j| j.combos.iter().flatten().map(|c| &c.digits)),
            )
            .fold(0_usize, |n, d| n.saturating_add(d.offsets.len()));
        if digit_offsets > MAX_DIGIT_OFFSETS {
            return Err(LoadError::TooManyDigitOffsets { n: digit_offsets });
        }
        let keys = elements
            .iter()
            .map(|e| &e.track)
            .chain(w.judges.iter().flat_map(|j| j.tracks().map(|(_, t)| t)))
            .chain(w.image_choices.iter().flat_map(ImageChoiceDef::tracks))
            .chain(w.cover_paints.iter().filter_map(|p| p.gate_track.as_ref()))
            .chain(
                def.lanes
                    .iter()
                    .flat_map(|l| l.bar_groups.iter().map(|g| &g.track)),
            )
            .chain(
                sel.song_list
                    .iter()
                    .flat_map(SongListDef::parts)
                    .map(|p| &p.track),
            )
            .fold(0_usize, |n, t| n.saturating_add(t.key_count()));
        if keys > MAX_SKIN_KEYS {
            return Err(LoadError::TooManyKeys { n: keys });
        }
        if let Some(when) = whens.iter().position(|w| w.ty() != Ty::Bool) {
            return Err(LoadError::WhenNotBool { when });
        }
        if let Some(element) = elements
            .iter()
            .position(|e| e.visible.as_ref().is_some_and(|v| v.ty() != Ty::Bool))
        {
            return Err(LoadError::VisibleNotBool { element });
        }
        for (element, e) in elements.iter().enumerate() {
            match e.source {
                Source::Number(i) => {
                    if w.numbers
                        .get(index(i))
                        .is_some_and(|n| n.value.ty() != Ty::Int)
                    {
                        let error = NumberError::NotInt;
                        return Err(LoadError::Number { element, error });
                    }
                }
                Source::Judge(i) => {
                    if let Some((part, _)) = w
                        .judges
                        .get(index(i))
                        .into_iter()
                        .flat_map(JudgeDef::visibles)
                        .find(|(_, v)| v.ty() != Ty::Bool)
                    {
                        let error = JudgeError::VisibleNotBool { part };
                        return Err(LoadError::Judge { element, error });
                    }
                }
                Source::Image(_)
                | Source::Lanes
                | Source::Gauge(_)
                | Source::Text(_)
                | Source::ImageChoice(_)
                | Source::CoverPaint(_)
                | Source::SongList
                | Source::RatioBar(_)
                | Source::JudgeGraph(_)
                | Source::GaugeGraph(_)
                | Source::TimingGraph(_) => {}
            }
        }
        let mut slots = Vec::with_capacity(elements.len());
        for (element, e) in elements.iter().enumerate() {
            let slot =
                element_slot_of((e, element), &def, (whens.len(), builtin), present.as_ref())?;
            slots.push(slot);
            if let Source::Judge(i) = e.source
                && let Some((part, _)) = w
                    .judges
                    .get(index(i))
                    .into_iter()
                    .flat_map(JudgeDef::timers)
                    .find(|&(_, t)| screen_slot_of(t, whens.len(), builtin).is_none())
            {
                let error = JudgeError::UnknownTimer { part };
                return Err(LoadError::Judge { element, error });
            }
        }
        let images = def.sheet.images();
        let exists = |i: u32| usize::try_from(i).is_ok_and(|i| i < images.len());
        let frames = |i: u32| images.get(index(i)).map(|m| m.frames);
        if let Some(element) = elements.iter().position(|e| match e.source {
            Source::Image(_) | Source::Lanes => false,
            Source::CoverPaint(p) => w.cover_paints.get(index(p)).is_none(),
            Source::Number(i) => w.numbers.get(index(i)).is_none(),
            Source::Gauge(i) => w.gauges.get(index(i)).is_none(),
            Source::Judge(i) => w.judges.get(index(i)).is_none(),
            Source::Text(i) => w.texts.get(index(i)).is_none(),
            Source::ImageChoice(i) => w.image_choices.get(index(i)).is_none(),
            Source::SongList => sel.song_list.is_none(),
            Source::RatioBar(i) => sel.ratio_bars.get(index(i)).is_none(),
            Source::JudgeGraph(i) => res.judge_graphs.get(index(i)).is_none(),
            Source::GaugeGraph(i) => res.gauge_graphs.get(index(i)).is_none(),
            Source::TimingGraph(i) => res.timing_graphs.get(index(i)).is_none(),
        }) {
            return Err(LoadError::UnknownWidget { element });
        }
        if let Some(element) = elements.iter().position(|e| match e.source {
            Source::Image(i) => !exists(i),
            Source::Lanes
            | Source::Number(_)
            | Source::Gauge(_)
            | Source::Judge(_)
            | Source::Text(_)
            | Source::ImageChoice(_)
            | Source::CoverPaint(_)
            | Source::SongList
            | Source::RatioBar(_)
            | Source::JudgeGraph(_)
            | Source::GaugeGraph(_)
            | Source::TimingGraph(_) => false,
        }) {
            return Err(LoadError::UnknownImage { element });
        }
        if let Some(graph) = res
            .judge_graphs
            .iter()
            .position(|g| !exists(g.chips) || !exists(g.solid))
        {
            return Err(LoadError::JudgeGraphImage { graph });
        }
        if let Some(element) = elements.iter().position(|e| {
            matches!(
                e.source,
                Source::JudgeGraph(_) | Source::GaugeGraph(_) | Source::TimingGraph(_)
            ) && e.place.native.is_some()
        }) {
            return Err(LoadError::GraphNative { element });
        }
        if let Some(graph) = res.gauge_graphs.iter().position(|g| !exists(g.solid)) {
            return Err(LoadError::GaugeGraphImage { graph });
        }
        if let Some(graph) = res.timing_graphs.iter().position(|g| !exists(g.solid)) {
            return Err(LoadError::TimingGraphImage { graph });
        }
        check_selection_widgets(&sel, &frames, whens.len(), builtin, &def.sheet)?;
        for (element, e) in elements.iter().enumerate() {
            let fonts = [def.sheet.font_count(), def.sheet.bitmap_font_count()];
            check_widget_images(element, e.source, w, &frames, fonts)?;
        }
        check_image_choices(w, &frames, whens.len(), builtin)?;
        check_cover_paints(w, images.len())?;
        if let Some((part, _)) = def
            .lanes
            .iter()
            .flat_map(LanesDef::images)
            .find(|&(_, i)| !exists(i))
        {
            return Err(LoadError::Lanes(LanesError::UnknownPart { part }));
        }
        let mut cycles = Vec::with_capacity(images.len());
        for (image, d) in images.iter().enumerate() {
            let cycle = match d.cycle {
                Some(c) => Some(Cycle {
                    period: c.period.0,
                    slot: screen_slot_of(c.timer, whens.len(), builtin)
                        .ok_or(LoadError::UnknownImageTimer { image })?,
                }),
                None => None,
            };
            cycles.push(cycle);
        }
        check_places(&def)?;
        for (element, e) in elements.iter().enumerate() {
            check_widget_params(element, e.source, w)?;
        }
        let widget = elements.iter().any(|e| e.source == Source::Lanes);
        let lanes = match (&def.lanes, widget) {
            (None, false) => None,
            (Some(_), false) => return Err(LoadError::Lanes(LanesError::MissingElement)),
            (None, true) => return Err(LoadError::Lanes(LanesError::MissingDef)),
            (Some(l), true) => {
                l.check().map_err(LoadError::Lanes)?;
                Some(l)
            }
        };
        let lanes_in = elements
            .iter()
            .find(|e| e.source == Source::Lanes)
            .map(|e| e.place.region);
        check_cover_gates(&def)?;
        if let Some(l) = lanes {
            check_bar_groups(l, whens.len(), lanes_in, l.reach())?;
        }
        check_follows(&def, lanes.map_or(0, LanesDef::reach))?;
        let mut ops = SkinOps::default();
        // Each element evaluates its own number's value and its judge's
        // part visibilities, so they are charged per referencing element.
        let per_element = elements.iter().flat_map(|e| {
            let value = match e.source {
                Source::Number(i) => w.numbers.get(index(i)).map(|n| &n.value),
                _ => choice_of(&def, e.source).map(|c| &c.value),
            };
            let judge = match e.source {
                Source::Judge(i) => w.judges.get(index(i)),
                _ => None,
            };
            value
                .into_iter()
                .chain(judge.into_iter().flat_map(|j| j.visibles().map(|(_, v)| v)))
        });
        // The lanes element samples each bar group's visibility once.
        let groups = lanes
            .into_iter()
            .flat_map(|l| l.bar_groups.iter().filter_map(|g| g.visible.as_ref()));
        let signature: &[OutTy] = present.as_ref().map_or(&[], |p| &p.signature);
        let base = match kind {
            ScreenKind::Selection => crate::SELECTION.fields.len(),
            ScreenKind::Result => crate::RESULT.fields.len(),
            ScreenKind::Decision => crate::DECISION.fields.len(),
            ScreenKind::Play => field::COUNT,
        };
        for (i, program) in whens
            .iter()
            .chain(elements.iter().filter_map(|e| e.visible.as_ref()))
            .chain(per_element)
            .chain(groups)
            .enumerate()
        {
            ops.charge(program).map_err(LoadError::Ops)?;
            // A selection or result screen's outputs are its runtime's,
            // which the importer counts after the build ([`Skin::with_outputs`]).
            if matches!(kind, ScreenKind::Play | ScreenKind::Decision) {
                check_outputs(program, base, signature, i < whens.len())?;
            }
        }
        check_slider_outputs(&def.widgets.cover_paints, signature)?;
        // V1 to V5, its per-frame cost charged with the expressions'.
        let present = present
            .map(|p| Present::verify(p, base, &mut ops))
            .transpose()
            .map_err(LoadError::Present)?;
        if let Some(list) = &sel.song_list {
            let lists = elements
                .iter()
                .filter(|e| e.source == Source::SongList)
                .count();
            let slots = list.placements.len();
            let arms = list
                .placements
                .iter()
                .flat_map(|p| p.iter())
                .filter_map(|p| p.part.visible.as_ref());
            let styles: Vec<&Program> = list
                .parts()
                .skip(2 * slots)
                .filter_map(|p| p.visible.as_ref())
                .collect();
            for _ in 0..lists {
                for program in arms.clone() {
                    ops.charge(program).map_err(LoadError::Ops)?;
                }
                for &program in &styles {
                    for _ in 0..slots {
                        ops.charge(program).map_err(LoadError::Ops)?;
                    }
                }
            }
        }
        let widget_quads = elements
            .iter()
            .fold(0_u64, |n, e| n + u64::from(worst(e.source, w, &sel, &res)));
        if widget_quads > u64::from(MAX_WIDGET_QUADS) {
            return Err(LoadError::TooManyWidgetQuads { n: widget_quads });
        }
        let widget_quads = u32::try_from(widget_quads).unwrap_or(MAX_WIDGET_QUADS);
        let image = |i: u32| {
            let m = images.get(index(i));
            ImageRef {
                first: m.map_or(0, |m| m.first),
                frames: m.map_or(0, |m| m.frames),
                cycle: cycles.get(index(i)).copied().flatten(),
                filter: m.map_or(Filter::Linear, |m| m.filter),
            }
        };
        let n_whens = whens.len();
        // A timer only an unreferenced judge names reads off.
        let slot = |t: TimerRef| screen_slot_of(t, n_whens, builtin).unwrap_or(usize::MAX);
        let checked = lanes.is_some();
        let lanes = def
            .lanes
            .take()
            .filter(|_| checked)
            .map(|l| Lanes::new(l, image, slot));
        let regions = regions(&def, lanes.as_ref(), &sel);
        let WidgetDefs {
            numbers,
            gauges,
            judges,
            texts,
            image_choices,
            cover_paints,
        } = def.widgets;
        let cover_paints: Box<[CoverPaint]> = cover_paints
            .into_iter()
            .map(|p| CoverPaint {
                image: match p.image {
                    CoverImage::Image(i) => PaintImage::Image(image(i)),
                    CoverImage::Choice(i) => PaintImage::Choice(index(i)),
                },
                slider: p.slider,
                motion: p.motion,
                trim: p.trim,
                gate: p.gate_track,
            })
            .collect();
        let image_choices: Box<[ImageChoice]> = image_choices
            .into_iter()
            .map(|d| ImageChoice {
                value: d.value,
                past_table: d.past_table,
                parts: d
                    .choices
                    .into_iter()
                    .map(|c| {
                        c.map(|c| {
                            let base = image(c.span.image);
                            ChoicePart {
                                image: ImageRef {
                                    first: base.first.saturating_add(u32::from(c.span.first)),
                                    frames: c.span.frames,
                                    cycle: c.cycle.map(|cycle| Cycle {
                                        period: cycle.period.0,
                                        slot: slot(cycle.timer),
                                    }),
                                    filter: base.filter,
                                },
                                track: c.track,
                                native: c.native,
                            }
                        })
                    })
                    .collect(),
            })
            .collect();
        let numbers: Box<[Number]> = numbers
            .into_iter()
            .map(|n| Number {
                digits: Digits::new(n.digits, image),
                value: n.value,
            })
            .collect();
        let gauges = gauges.iter().map(|g| Gauge::new(g, image)).collect();
        let judges = judges
            .into_iter()
            .map(|j| Judge {
                grades: j.grades.map(|g| {
                    g.map(|g| GradeImage {
                        image: image(g.image),
                        slot: slot(g.timer),
                        track: g.track,
                        pivot: g.pivot.fractions(),
                        blend: g.blend,
                        visible: g.visible,
                        native: g.native,
                        follow: g.follow,
                        attachment: g.attachment,
                    })
                }),
                combos: j.combos.map(|c| {
                    c.map(|c| ComboStrip {
                        digits: Digits::new(c.digits, image),
                        slot: slot(c.timer),
                        track: c.track,
                        attach: c.attach,
                        pivot: c.pivot.fractions(),
                        blend: c.blend,
                        visible: c.visible,
                        native: c.native,
                    })
                }),
                shift: j.shift,
            })
            .collect();
        // Text slots in draw order; at most `MAX_TEXT_ELEMENTS`.
        let mut text_slots = 0_u16;
        let mut sliders = 0_usize;
        let SelectionDefs {
            song_list,
            ratio_bars,
        } = sel;
        let song_list = song_list.map(|l| song_list_of(l, &image, &slot));
        let ratio_bars: Box<[RatioBar]> = ratio_bars
            .into_iter()
            .map(|def| RatioBar {
                def,
                image: image(def.image),
            })
            .collect();
        let judge_graphs: Box<[JudgeGraph]> = res
            .judge_graphs
            .into_iter()
            .map(|def| JudgeGraph {
                def,
                chips: image(def.chips),
                solid: image(def.solid),
            })
            .collect();
        let gauge_graphs: Box<[GaugeGraph]> = res
            .gauge_graphs
            .into_iter()
            .map(|def| GaugeGraph {
                def,
                solid: image(def.solid),
            })
            .collect();
        let timing_graphs: Box<[TimingGraph]> = res
            .timing_graphs
            .into_iter()
            .map(|def| TimingGraph {
                def,
                solid: image(def.solid),
            })
            .collect();
        let elements = def
            .elements
            .into_iter()
            .zip(slots)
            .map(|(e, slot)| Element {
                loading: {
                    let choice = |i: usize| image_choices.get(i).map(|c| &c.value);
                    let value = match e.source {
                        Source::Number(i) => numbers.get(index(i)).map(|n| &n.value),
                        Source::ImageChoice(i) => choice(index(i)),
                        Source::CoverPaint(p) => {
                            match cover_paints.get(index(p)).map(|p| p.image) {
                                Some(PaintImage::Choice(i)) => choice(i),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    // A progress slider reads `time.progress_ppm`, which
                    // loading knows (0) and nothing else needs.
                    Need::of(e.visible.as_ref()).and(Need::of(value))
                },
                kind: match e.source {
                    Source::Image(i) => Kind::Image(image(i)),
                    Source::Lanes => Kind::Lanes,
                    Source::Number(i) => Kind::Number(index(i)),
                    Source::Gauge(i) => Kind::Gauge(index(i)),
                    Source::Judge(i) => Kind::Judge(index(i)),
                    Source::ImageChoice(i) => Kind::ImageChoice(index(i)),
                    Source::CoverPaint(p) => Kind::CoverPaint(index(p)),
                    Source::SongList => Kind::SongList,
                    Source::RatioBar(i) => {
                        let changeable = matches!(
                            ratio_bars.get(index(i)).map(|b| b.def.form),
                            Some(RatioForm::Slider {
                                changeable: true,
                                ..
                            })
                        );
                        let hit = changeable.then(|| {
                            sliders += 1;
                            sliders - 1
                        });
                        Kind::RatioBar(index(i), hit)
                    }
                    Source::JudgeGraph(i) => Kind::JudgeGraph(index(i)),
                    Source::GaugeGraph(i) => Kind::GaugeGraph(index(i)),
                    Source::TimingGraph(i) => Kind::TimingGraph(index(i)),
                    Source::Text(i) => {
                        let def = texts.get(index(i));
                        let text = Text {
                            slot: text_slots,
                            align: def.map_or(Align::Left, |d| d.align),
                            overflow: def.map_or(Overflow::Spill, |d| d.overflow),
                            bitmap: match def.map(|d| d.font) {
                                Some(FontRef::Bitmap(f)) => Some(f),
                                _ => None,
                            },
                        };
                        text_slots = text_slots.saturating_add(1);
                        Kind::Text(index(i), text)
                    }
                },
                track: e.track,
                slot,
                visible: e.visible,
                key: RunKey {
                    blend: e.blend,
                    clip: e.clip,
                },
                region: e.place.region,
                pivot: e.place.pivot.fractions(),
                bind: e.place.bind,
                fill: e.place.fill,
                native: e.place.native,
                follow: e.place.follow,
                angle: e.place.angle,
            })
            .collect();
        let whens_need = def.whens.iter().map(|w| Need::of(Some(w))).collect();
        Ok(Self {
            elements,
            whens: def.whens.into_boxed_slice(),
            whens_need,
            present,
            ops,
            sheet_id: def.sheet.id(),
            frame_count: def.sheet.frame_count(),
            sheet: def.sheet,
            regions,
            lanes,
            numbers,
            gauges,
            judges,
            texts: texts.into_boxed_slice(),
            image_choices,
            cover_paints,
            text_slots,
            widget_quads,
            bomb_up_to: def.bomb_up_to,
            close: def.close,
            loadend: def.loadend,
            kind,
            song_list,
            ratio_bars,
            judge_graphs,
            result_timing: res.timing,
            decision_timing: crate::decision::DecisionTiming::default(),
            gauge_graphs,
            timing_graphs,
            sliders,
            outputs: 0,
            id: NEXT_SKIN.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// How long the skin declares Closing, if it declares a positive
    /// duration ([`SkinDef::close`]).
    #[must_use]
    pub fn close(&self) -> Option<TimeUs> {
        self.close
    }

    /// How long the skin declares its loading presentation lasts at least,
    /// if it declares it ([`SkinDef::loadend`]).
    #[must_use]
    pub fn loadend(&self) -> Option<TimeUs> {
        self.loadend
    }

    /// What a result skin declares for its screen's timing
    /// ([`ResultDefs::timing`]); every part 0 or none for another kind.
    #[must_use]
    pub fn result_timing(&self) -> crate::result::ResultTiming {
        self.result_timing
    }

    /// What a decision skin declares for its screen's timing
    /// ([`DecisionDefs::timing`]); every part 0 for another kind.
    #[must_use]
    pub fn decision_timing(&self) -> crate::decision::DecisionTiming {
        self.decision_timing
    }

    /// The frames of the sheet's images: the length of the uv table
    /// [`build`](Self::build) takes.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// The sheet the elements draw from.
    #[must_use]
    pub fn sheet(&self) -> &Sheet {
        &self.sheet
    }

    /// Builds the sheet's atlas for `variant`, asking `cancel` as it goes,
    /// with a text layer when the skin has rasterized text
    /// ([`has_outline_text`](Self::has_outline_text),
    /// [`Sheet::atlas_reserving`]).
    ///
    /// # Errors
    ///
    /// As [`Sheet::atlas_reserving`].
    pub fn atlas(
        &self,
        variant: Variant,
        limits: AtlasLimits,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Atlas, AtlasError> {
        self.sheet
            .atlas_reserving(variant, limits, self.has_outline_text(), cancel)
    }

    /// Whether some text element or song list row style is rasterized: set
    /// in the default font or a font file, not a bitmap font.
    #[must_use]
    pub fn has_outline_text(&self) -> bool {
        self.text_fonts()
            .any(|f| matches!(f, FontRef::Default | FontRef::File(_)))
    }

    /// The font of every text element and every song list row style.
    fn text_fonts(&self) -> impl Iterator<Item = FontRef> + '_ {
        self.texts.iter().map(|t| t.font).chain(
            self.song_list
                .iter()
                .flat_map(|l| l.text.iter().flatten().map(|t| t.content.1)),
        )
    }

    /// The font files the rasterized text elements and song list row
    /// styles are set in, each once,
    /// in index order: the ones a [`FontReader`](crate::FontReader) reads
    /// for installation.
    #[must_use]
    pub fn outline_fonts(&self) -> Vec<u32> {
        let mut fonts: Vec<u32> = self
            .text_fonts()
            .filter_map(|f| match f {
                FontRef::File(f) => Some(f),
                FontRef::Default | FontRef::Bitmap(_) => None,
            })
            .collect();
        fonts.sort_unstable();
        fonts.dedup();
        fonts
    }

    /// The operations every frame's expressions and presentation program
    /// run (V5 charges the program's).
    #[must_use]
    pub fn ops(&self) -> SkinOps {
        self.ops
    }

    /// The presentation program's callbacks; 0 without one.
    #[must_use]
    pub fn program_callbacks(&self) -> usize {
        self.present.as_ref().map_or(0, Present::len)
    }

    /// The presentation program's structure, written out (dev-tools): its
    /// instructions, constants, reads, gates, slots and signature, which
    /// two imports' programs are equal exactly when.
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn program_structure(&self) -> String {
        format!("{:?}", self.present)
    }

    /// Whether the presentation program's slotted callbacks may run on a
    /// carried state ([`Present::carry_safe`](crate::present::Present::carry_safe));
    /// true without a program (dev-tools).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn program_carry_safe(&self) -> bool {
        self.present
            .as_ref()
            .is_none_or(crate::present::Present::carry_safe)
    }

    /// Per program callback, whether it loads or stores a slot (dev-tools).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn program_slotted(&self) -> Vec<bool> {
        self.present
            .as_ref()
            .map_or_else(Vec::new, |p| p.slotted().to_vec())
    }

    /// The lanes' bar groups: the groups a [`Play`] from
    /// [`prepare`](Self::prepare) holds work for.
    #[must_use]
    pub fn bar_group_count(&self) -> usize {
        self.lanes.as_ref().map_or(0, |l| l.bar_groups.len())
    }

    /// The bytes of one bar group's per-attempt work in a [`Play`]: its
    /// frame record and heap slot.
    #[must_use]
    pub fn bar_group_scratch_bytes() -> usize {
        BarScratch::group_bytes()
    }

    /// The digit offsets the loaded strips hold: every number's and every
    /// judge combo's, a shared definition once and each stored copy apart.
    #[must_use]
    pub fn digit_offset_count(&self) -> usize {
        let numbers = self.numbers.iter().map(|n| n.digits.offsets.len());
        let combos = self
            .judges
            .iter()
            .flat_map(|j| j.combos.iter().flatten().map(|c| c.digits.offsets.len()));
        numbers.chain(combos).sum()
    }

    /// The entries of the image choice tables, holes and repeats included.
    #[must_use]
    pub fn choice_ref_count(&self) -> usize {
        self.image_choices.iter().map(|c| c.parts.len()).sum()
    }

    /// The fixed clips its elements draw under: one past the highest
    /// [`ClipId`] an element names, 0 when none does. A clip table of fewer
    /// rectangles would leave some of them unclipped or undrawn.
    #[must_use]
    pub fn clip_count(&self) -> usize {
        self.elements
            .iter()
            .filter_map(|e| e.key.clip)
            .map(|c| usize::from(c.0) + 1)
            .max()
            .unwrap_or(0)
    }

    /// The quads a frame can push: one per image element (an image choice
    /// element, a cover paint and a ratio bar included, however long its
    /// table), each
    /// widget
    /// element's worst case (a number's cells, a gauge's segments and its
    /// flicker's tip, a judge's largest slot with its combo's cells, a
    /// text's one line or its bitmap glyphs), and the lanes' budget with
    /// its reserved judgment line.
    #[must_use]
    pub fn quad_capacity(&self) -> u32 {
        let images = self
            .elements
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    Kind::Image(_)
                        | Kind::ImageChoice(_)
                        | Kind::CoverPaint(_)
                        | Kind::RatioBar(..)
                )
            })
            .count();
        let lanes = if self.lanes.is_some() {
            LANE_QUADS + LANE_RESERVED
        } else {
            0
        };
        u32::try_from(images)
            .unwrap_or(u32::MAX)
            .saturating_add(self.widget_quads)
            .saturating_add(lanes)
    }

    /// The runs a frame can push: at worst, one per quad (a judge pushes
    /// at most two runs, each of at least one quad).
    #[must_use]
    pub fn run_capacity(&self) -> u32 {
        self.quad_capacity()
    }

    /// The canvas this skin draws on for a `target`-pixel target.
    #[must_use]
    pub fn canvas(&self, target: [u32; 2]) -> Canvas {
        Canvas::new(target, self.regions.fit)
    }

    /// Where every region lands on `canvas` with `layout` and the lanes
    /// laid out for `scratch`. [`build`](Self::build) uses the play's
    /// scratch side; a preview or the adjust mode passes the selected one.
    /// Allocates nothing.
    #[must_use]
    pub fn placement(&self, canvas: &Canvas, layout: &Layout, scratch: Side) -> Placement {
        self.regions
            .place(self.lanes.as_ref(), canvas, layout, scratch)
    }

    /// The controls this skin offers for region `r`; none for a region
    /// without elements, and none at all on a fixed canvas.
    #[must_use]
    pub fn controls(&self, r: Region) -> Controls {
        self.regions.controls(r)
    }

    /// The asset variant for a `target`-pixel target with `layout`: `Hi`
    /// when some image the skin can draw is magnified on its smaller axis,
    /// counting the canvas scale, its region's scale, a cover or stretch
    /// fill, and for lane-bound images and the lane parts a lane width
    /// below 100 % (lane width alone never picks `Hi`). A bar group counts with
    /// the lanes element, under its region and visibility, by its own
    /// binding: an unbound group has no lane width. Hidden regions and
    /// elements and groups whose visibility folds to false do not count;
    /// a cover paint counts by its authored tracks, its gate, trim and
    /// motion not counted.
    /// Exact rationals, no float. For `prepare`, not in play.
    #[must_use]
    pub fn variant(&self, target: [u32; 2], layout: &Layout) -> Variant {
        let (fit, cw, ch) = self.fit(target);
        let (fw, fh) = (cw.mul(Ratio(1, 1_920)), ch.mul(Ratio(1, 1_080)));
        let k = u128::from(self.regions.lane_width(layout).min(100));
        let one = Ratio(1, 1);
        let hi = self.elements.iter().any(|e| {
            let adjust = self.regions.effective(e.region, layout);
            // A hidden region, or a visibility folded to false, never draws;
            // a text has its own pixel size.
            let never = e.visible.as_ref().and_then(Program::constant) == Some(0);
            if !adjust.visible || never || matches!(e.kind, Kind::Text(..)) {
                return false;
            }
            let fill = match e.fill {
                Fill::None => one,
                Fill::Cover => fw.max(fh),
                Fill::Stretch => fw.min(fh),
            };
            let scale = Ratio(u128::from(adjust.scale), 100);
            let placed = fit.mul(scale).mul(fill);
            let lane = |bind: Bind| {
                if bind == Bind::None {
                    one
                } else {
                    Ratio(k, 100)
                }
            };
            if matches!(e.kind, Kind::Lanes) {
                let groups = self.lanes.iter().flat_map(|l| &l.bar_groups);
                return placed.mul(Ratio(k, 100)).gt(one)
                    || groups
                        .filter(|g| g.visible.as_ref().and_then(Program::constant) != Some(0))
                        .any(|g| placed.mul(lane(g.bind)).gt(one));
            }
            placed.mul(lane(e.bind)).gt(one)
        });
        if hi { Variant::Hi } else { Variant::Lo }
    }

    /// For a `target`-pixel target: the canvas scale in target pixels per
    /// reference pixel, and the canvas's width and height in reference
    /// pixels, as exact fractions.
    fn fit(&self, target: [u32; 2]) -> (Ratio, Ratio, Ratio) {
        let [tw, th] = target.map(|v| u128::from(v.max(1)));
        let wide = tw * 9 >= th * 16;
        let [max_w, max_h] = MAX_CANVAS.map(u128::from);
        if self.regions.fit == Fit::Fixed {
            let fit = Ratio(tw, 1_920).min(Ratio(th, 1_080));
            (fit, Ratio(1_920, 1), Ratio(1_080, 1))
        } else if wide {
            let cw = if tw > 4 * th {
                Ratio(max_w, 1)
            } else {
                Ratio(1_080 * tw, th)
            };
            (Ratio(th, 1_080), cw, Ratio(1_080, 1))
        } else {
            let ch = if th > 2 * tw {
                Ratio(max_h, 1)
            } else {
                Ratio(1_920 * th, tw)
            };
            (Ratio(tw, 1_920), Ratio(1_920, 1), ch)
        }
    }

    /// The text elements: the length of the table
    /// [`attach_text`](Self::attach_text) takes.
    #[must_use]
    pub fn text_slots(&self) -> usize {
        usize::from(self.text_slots)
    }

    /// The lines to rasterize for `chart` on a `target`-pixel target with
    /// `layout`, in slot order. For `prepare`, not in play: it allocates.
    ///
    /// A text element whose visibility folds to false gives no request,
    /// nor does one whose string is empty once control characters are
    /// dropped (after the full fields are joined) and it is cut to
    /// [`MAX_TEXT_BYTES`](crate::MAX_TEXT_BYTES) at a character boundary.
    /// The em is the element's largest key height times the canvas scale
    /// and its region's effective scale, at most [`MAX_TEXT_EM_PX`]. An
    /// element in a hidden region still gets a request, since the adjust
    /// mode can show it.
    #[must_use]
    pub fn text_requests(
        &self,
        chart: &ChartText<'_>,
        target: [u32; 2],
        layout: &Layout,
    ) -> Vec<TextRequest> {
        let (fit, ..) = self.fit(target);
        let mut out = Vec::new();
        for e in &self.elements {
            let Kind::Text(i, t) = e.kind else {
                continue;
            };
            if e.visible.as_ref().and_then(Program::constant) == Some(0) {
                continue;
            }
            let Some(def) = self.texts.get(i) else {
                continue;
            };
            if t.bitmap.is_some() {
                continue;
            }
            let s = text::clean(&def.value.resolve(chart));
            if s.is_empty() {
                continue;
            }
            out.push(TextRequest {
                slot: t.slot,
                text: s,
                font: def.font,
                em_px: self.em_px(&e.track, e.region, fit, layout),
            });
        }
        out
    }

    /// A line's em in pixels: `track`'s largest key height times the canvas
    /// scale `fit` and `region`'s effective scale under `layout`, at most
    /// [`MAX_TEXT_EM_PX`].
    fn em_px(&self, track: &Track, region: Region, fit: Ratio, layout: &Layout) -> f32 {
        let h = track.rects().map(|[.., h]| h).max().unwrap_or(0);
        let h = u128::try_from(h).unwrap_or(0);
        let scale = self.regions.effective(region, layout).scale;
        let em = Ratio(h, 1).mul(fit).mul(Ratio(u128::from(scale), 100));
        // Both terms are below 2^60; rounding them to f64 is the intent.
        f32_of(em.0 as f64 / em.1 as f64).min(MAX_TEXT_EM_PX)
    }

    /// Lays out `play`'s bitmap lines for `chart`, one per text slot in a
    /// bitmap font ([`FontRef::Bitmap`]). For `prepare`, not in play: it
    /// allocates. A fresh [`prepare`](Self::prepare) starts with none, so
    /// no bitmap text draws until this is called.
    ///
    /// The string is chosen and cleaned as
    /// [`text_requests`](Self::text_requests) does; an element whose
    /// visibility folds to false, or whose line has a value beyond
    /// ±[`MAX_TEXT_EM`](crate::MAX_TEXT_EM) em, gets no line.
    pub fn attach_bitmap_text(&self, play: &mut Play, chart: &ChartText<'_>) {
        play.set_bitmap(self.bitmap_lines(chart));
    }

    /// The bitmap lines of `chart`, one per text slot, as
    /// [`attach_bitmap_text`](Self::attach_bitmap_text) lays them out.
    fn bitmap_lines(&self, chart: &ChartText<'_>) -> Box<[Option<GlyphLine>]> {
        let mut lines = vec![None; self.text_slots()];
        for e in &self.elements {
            let Kind::Text(i, t) = e.kind else {
                continue;
            };
            let (Some(font), Some(def)) = (t.bitmap, self.texts.get(i)) else {
                continue;
            };
            if e.visible.as_ref().and_then(Program::constant) == Some(0) {
                continue;
            }
            let s = text::clean(&def.value.resolve(chart));
            if let Some(slot) = lines.get_mut(usize::from(t.slot)) {
                *slot = glyphs::layout(&self.sheet, font, &s);
            }
        }
        lines.into_boxed_slice()
    }

    /// Attaches the rasterized lines of `play`'s attempt, one per text
    /// slot; a slot in a bitmap font draws its bitmap line instead. A
    /// fresh [`prepare`](Self::prepare) starts with none, so no text draws
    /// until its table is attached.
    ///
    /// # Errors
    ///
    /// A table whose length is not [`text_slots`](Self::text_slots).
    pub fn attach_text(&self, play: &mut Play, table: TextTable) -> Result<(), TextTableError> {
        let expected = self.text_slots();
        if table.len() != expected {
            return Err(TextTableError::Len {
                expected,
                got: table.len(),
            });
        }
        play.set_text(table);
        Ok(())
    }

    /// A fresh loading state ([`Loading`]) for the play screen before a
    /// chart's READY state, its lanes laid out for `scratch`: no chart, every
    /// timer off and every text slot empty until a build or an install.
    #[must_use]
    pub fn prepare_loading(&self, scratch: Side) -> Loading {
        if self.kind != ScreenKind::Play {
            return Loading::inert(scratch);
        }
        Loading::new(self.whens.len(), self.text_slots(), scratch)
    }

    /// Whether text slot `slot` is one that `which` names (a composite
    /// with its parts), as [`install_loading_text`](Self::install_loading_text)
    /// and [`clear_loading_text`](Self::clear_loading_text) name them.
    #[must_use]
    pub fn names_text_slot(&self, which: TextSlots, slot: u16) -> bool {
        self.named_slots(which).any(|(s, ..)| s == slot)
    }

    /// The text elements `which` names, as `(slot, its value, its
    /// bitmap font)`.
    fn named_slots(
        &self,
        which: TextSlots,
    ) -> impl Iterator<Item = (u16, &TextValue, Option<u32>)> {
        self.elements.iter().filter_map(move |e| match e.kind {
            Kind::Text(i, t) => self
                .texts
                .get(i)
                .filter(|d| which.names(&d.value))
                .map(|d| (t.slot, &d.value, t.bitmap)),
            _ => None,
        })
    }

    /// Installs `table`'s rasterized lines into `loading`'s slots that
    /// `which` names (a composite with its parts), each from the table's
    /// line for that slot, an empty one clearing it; other slots keep
    /// theirs, and no timer changes. The lines show from the next
    /// [`build_loading`](Self::build_loading).
    ///
    /// # Errors
    ///
    /// A table whose length is not [`text_slots`](Self::text_slots);
    /// nothing changes.
    pub fn install_loading_text(
        &self,
        loading: &mut Loading,
        which: TextSlots,
        table: &TextTable,
    ) -> Result<(), TextTableError> {
        let expected = self.text_slots();
        if table.len() != expected {
            return Err(TextTableError::Len {
                expected,
                got: table.len(),
            });
        }
        for (slot, ..) in self.named_slots(which) {
            loading.text_mut().set(slot, table.get(slot).copied());
        }
        Ok(())
    }

    /// Lays out `chart`'s lines in bitmap fonts for the slots `which`
    /// names, as [`attach_bitmap_text`](Self::attach_bitmap_text) does for
    /// a play, to install later
    /// ([`install_loading_bitmap_lines`](Self::install_loading_bitmap_lines));
    /// every other slot has none. Preparation, not a build: it allocates.
    #[must_use]
    pub fn loading_bitmap_lines(&self, which: TextSlots, chart: &ChartText<'_>) -> BitmapLines {
        let mut lines = vec![None; self.text_slots()];
        for (slot, value, font) in self.named_slots(which) {
            let Some(font) = font else {
                continue;
            };
            if let Some(s) = lines.get_mut(usize::from(slot)) {
                *s = glyphs::layout(&self.sheet, font, &text::clean(&value.resolve(chart)));
            }
        }
        BitmapLines::new(lines.into_boxed_slice())
    }

    /// Installs `lines` into `loading`'s bitmap-font slots that `which`
    /// names, each taking its line or, with none, cleared; other slots
    /// keep theirs, and no timer changes. The lines move in, so nothing is
    /// laid out or allocated; the lines they replace are dropped.
    ///
    /// # Errors
    ///
    /// Lines whose length is not [`text_slots`](Self::text_slots);
    /// nothing changes.
    pub fn install_loading_bitmap_lines(
        &self,
        loading: &mut Loading,
        which: TextSlots,
        mut lines: BitmapLines,
    ) -> Result<(), TextTableError> {
        let expected = self.text_slots();
        if lines.len() != expected {
            return Err(TextTableError::Len {
                expected,
                got: lines.len(),
            });
        }
        for (slot, _, font) in self.named_slots(which) {
            if font.is_none() {
                continue;
            }
            if let Some(s) = loading.bitmap_mut().get_mut(usize::from(slot)) {
                *s = lines.take(slot);
            }
        }
        Ok(())
    }

    /// Lays out `chart`'s lines in bitmap fonts for `loading`'s slots that
    /// `which` names and installs them at once
    /// ([`loading_bitmap_lines`](Self::loading_bitmap_lines), then
    /// [`install_loading_bitmap_lines`](Self::install_loading_bitmap_lines)).
    /// For between frames, not in a build: it allocates.
    pub fn install_loading_bitmap_text(
        &self,
        loading: &mut Loading,
        which: TextSlots,
        chart: &ChartText<'_>,
    ) {
        let lines = self.loading_bitmap_lines(which, chart);
        // Laid out by this skin: its length is the skin's slots.
        let _ = self.install_loading_bitmap_lines(loading, which, lines);
    }

    /// Clears `loading`'s slots that `which` names, rasterized and bitmap
    /// lines alike, so they draw nothing from the next
    /// [`build_loading`](Self::build_loading); no timer changes.
    pub fn clear_loading_text(&self, loading: &mut Loading, which: TextSlots) {
        for (slot, ..) in self.named_slots(which) {
            loading.text_mut().set(slot, None);
            if let Some(s) = loading.bitmap_mut().get_mut(usize::from(slot)) {
                *s = None;
            }
        }
    }

    /// The timers the presentation program's timer callbacks give.
    fn program_timers(&self) -> usize {
        self.present.as_ref().map_or(0, Present::timers)
    }

    /// The output slots the presentation program writes, two a callback.
    fn program_slots(&self) -> usize {
        self.present
            .as_ref()
            .map_or(0, |p| crate::output::slots(p.len()))
    }

    /// A fresh [`Play`] for the first attempt of a play state of `chart`:
    /// [`prepare_carried`](Self::prepare_carried) with
    /// [`Continuation::Fresh`].
    ///
    /// # Errors
    ///
    /// As [`prepare_carried`](Self::prepare_carried).
    pub fn prepare(&self, chart: &Chart, ctx: &PlayContext<'_>) -> Result<Play, PrepareError> {
        self.prepare_carried(chart, ctx, Continuation::Fresh)
    }

    /// A [`Play`] for one attempt of `chart`, taking over `carry` from the
    /// earlier attempts of its play state: everything else starts clean.
    ///
    /// # Errors
    ///
    /// A negative lead-in, a READY state start outside the time range, a
    /// section whose initial gauges do not match the context's gauges, or
    /// a carried bank that does not fit the program.
    pub fn prepare_carried(
        &self,
        chart: &Chart,
        ctx: &PlayContext<'_>,
        carry: Continuation<'_>,
    ) -> Result<Play, PrepareError> {
        if self.kind != ScreenKind::Play {
            return Err(PrepareError::Kind);
        }
        Play::new(
            chart,
            ctx,
            carry,
            self.present.as_ref(),
            self.whens.len(),
            self.lanes.is_some(),
            self.bar_group_count(),
            self.bomb_up_to,
        )
    }

    /// Builds one frame: applies the drained effects, reads the snapshot,
    /// starts the timers, places the regions on the canvas of the frame's
    /// target, then appends each drawn element's quads to `out` in draw
    /// order (the caller clears `out`). Allocates nothing.
    ///
    /// `play` must come from this skin's [`prepare`](Self::prepare); one
    /// with another number of timers or bar groups draws nothing. `uvs` must come from an
    /// atlas of this skin's sheet; a table of another sheet, or of another
    /// length, draws nothing. A text element draws its slot's line from
    /// the table attached to `play` ([`attach_text`](Self::attach_text));
    /// a slot without a line, or a table of another length, draws no
    /// text.
    ///
    /// Each element shows its image's frame at the image's cycle timer:
    /// frame 0 while that timer is off, without a cycle, or with one frame.
    /// An image element's quad is its sampled rect moved by the covers it
    /// follows ([`Follow`](crate::Follow)), then with its lane (when
    /// bound), then by its fill or its region; its pivot is the
    /// named point of the sampled rect moved alike, and its angle turns
    /// the placed rect about it.
    ///
    /// Conditions sample once per frame, after the effects. The snapshot
    /// can be newer than a drain the effect buffer split, so a frame may
    /// show a newer combo beside an older judgment. `gauge_max` reads off
    /// while the snapshot's gauge is below its maximum; if a below-maximum
    /// effect is dropped and the gauge later refills, it keeps the earlier
    /// start.
    ///
    /// # Errors
    ///
    /// [`BuildError::InconsistentTally`] when the snapshot counts more
    /// empty POORs as POORs than it has POORs, else
    /// [`BuildError::InvalidReadouts`] when a readout is out of range or
    /// a stored cover setting disagrees with the lanes; then the same
    /// error on every build after that until a fresh
    /// [`prepare`](Self::prepare): nothing is appended and `play` keeps
    /// its state, its effects not applied. The caller stops the attempt
    /// and prepares again, or ends the play with an error; it never builds
    /// on that `play` again (stage3.md S10).
    pub fn build(
        &self,
        play: &mut Play,
        frame: &SkinFrame<'_>,
        uvs: &UvTable,
        out: &mut QuadBatch,
    ) -> Result<(), BuildError> {
        if self.kind != ScreenKind::Play {
            return Err(BuildError::Kind);
        }
        play.check()?;
        if play.slots() != usize::from(BUILTIN) + self.whens.len() + self.program_timers()
            || play.field_count() != field::COUNT + self.program_slots()
            || play.bar_capacity() != self.bar_group_count()
            || uvs.sheet_id() != self.sheet_id
            || uvs.frames().len() != self.frame_count
        {
            return Ok(());
        }
        let canvas = self.canvas(frame.target);
        let placement = self.placement(&canvas, frame.layout, play.scratch());
        play.fill(frame, &self.whens, self.present.as_ref())?;
        // The bar groups' work, lent to the lanes for this frame.
        let mut bars = play.take_bars();
        let shown = &*play;
        let rt = Runtime {
            starts: shown.starts(),
            state: shown.state(),
            text: shown.text(),
            bitmap: shown.bitmap(),
        };
        let scene = Scene {
            turntable: frame.turntable,
            now: frame.now,
            display: TimeUs(frame.now.0.saturating_add(frame.display_offset.0)),
            lanes: frame.lanes,
            complete: frame.complete,
            hidden_enabled: frame.readouts.covers.hidden_enabled,
            phase: Phase::Play {
                play: shown,
                notes: frame.notes,
                gauges: frame.gauges,
                shown: frame.gauge,
            },
        };
        self.draw(&rt, &scene, bars.as_mut(), uvs, &canvas, &placement, out);
        play.restore_bars(bars);
        Ok(())
    }

    /// Builds one loading frame of `loading`, which must come from this
    /// skin's [`prepare_loading`](Self::prepare_loading): fills its fields
    /// and timers from `frame` ([`LoadingFrame`]), places the regions,
    /// then appends each drawn element's quads to `out` in draw order
    /// with a play's own drawing (the caller clears `out`). Allocates
    /// nothing.
    ///
    /// `loading` and `screen` run from the frame's origin and every other
    /// built-in timer is off. An element whose visibility, number value or
    /// image selector reads a field unavailable while loading draws
    /// nothing, nor one reading the gauge's kind while the frame gives no
    /// descriptor; a `when` timer whose condition does is off. The lanes
    /// draw their judgment line alone, the gauge its empty frame from the
    /// descriptor, and a judge nothing. A text slot draws the line
    /// installed in it ([`install_loading_text`](Self::install_loading_text)).
    /// A state with another number of timers, or a uv table of another
    /// sheet or length, draws nothing.
    ///
    /// # Errors
    ///
    /// [`BuildError::InvalidReadouts`] when a stored cover setting is
    /// above 1 000 or disagrees with the lanes; then the same error on
    /// every later build of `loading`, which keeps its state.
    pub fn build_loading(
        &self,
        loading: &mut Loading,
        frame: &LoadingFrame<'_>,
        uvs: &UvTable,
        out: &mut QuadBatch,
    ) -> Result<(), BuildError> {
        if self.kind != ScreenKind::Play {
            return Err(BuildError::Kind);
        }
        if let Some(e) = loading.refused() {
            return Err(e);
        }
        if loading.slots() != usize::from(BUILTIN) + self.whens.len()
            || uvs.sheet_id() != self.sheet_id
            || uvs.frames().len() != self.frame_count
        {
            return Ok(());
        }
        let canvas = self.canvas(frame.target);
        let placement = self.placement(&canvas, frame.layout, loading.scratch());
        loading.fill(frame, &self.whens, &self.whens_need)?;
        let rt = Runtime {
            starts: loading.starts(),
            state: loading.state(),
            text: loading.text(),
            bitmap: loading.bitmap(),
        };
        let scene = Scene {
            turntable: frame.turntable,
            now: frame.now,
            display: frame.now,
            lanes: frame.lanes,
            complete: false,
            hidden_enabled: frame.covers.hidden_enabled,
            phase: Phase::Loading {
                gauge: frame.gauge,
                chosen: frame.chosen(),
            },
        };
        self.draw(&rt, &scene, None, uvs, &canvas, &placement, out);
        Ok(())
    }

    /// The screen this skin is made for.
    #[must_use]
    pub fn kind(&self) -> ScreenKind {
        self.kind
    }

    /// The song list's slots: the rows a selection view places around
    /// its cursor. 0 without a song list.
    #[must_use]
    pub fn song_list_slots(&self) -> usize {
        self.song_list.as_ref().map_or(0, |l| l.placements.len())
    }

    /// The song list's center slot, where the cursor's row shows.
    #[must_use]
    pub fn song_list_center(&self) -> usize {
        self.song_list.as_ref().map_or(0, |l| l.center)
    }

    /// This skin with `n` callback outputs after a selection or result
    /// state's fields ([`crate::output`]): the importer's count of the
    /// callbacks the screen's runtime runs, whose `lua.<k>` names its
    /// expressions read.
    #[must_use]
    pub fn with_outputs(mut self, n: usize) -> Self {
        self.outputs = n;
        self
    }

    /// The callback outputs a selection or result state carries.
    #[must_use]
    pub fn outputs(&self) -> usize {
        self.outputs
    }

    /// A fresh selection state ([`Selection`]): every timer off, every
    /// field 0, every row and text slot empty until a view and a text
    /// table. One a play skin makes is inert: it draws nothing.
    #[must_use]
    pub fn prepare_selection(&self) -> Selection {
        if self.kind != ScreenKind::Selection {
            return Selection::inert(self.id);
        }
        Selection::new(
            self.id,
            true,
            self.whens.len(),
            self.text_slots(),
            self.song_list_slots(),
            self.sliders,
            self.outputs,
        )
    }

    /// Whether `sel` is a state this selection skin made.
    fn owns(&self, sel: &Selection) -> bool {
        self.kind == ScreenKind::Selection
            && sel.live
            && sel.skin == self.id
            && sel.slots() == usize::from(selection_timer::BUILTIN) + self.whens.len()
            && sel.rows.len() == self.song_list_slots()
            && sel.hits.len() == self.sliders
            && sel.text.len() == self.text_slots()
    }

    /// Fills `sel` from `view` and the player's `labels`: the fields, the
    /// strings its text shows, each song list slot's row (slot `i` shows
    /// row `cursor + i − center` of the view, a slot past the view's rows
    /// none) and the ratios. For a view change, not a frame: it may
    /// allocate, at most the strings' growth. The texts stay as attached
    /// until a new table is.
    ///
    /// # Errors
    ///
    /// [`SelectionError::Kind`] when this is not a selection skin or `sel`
    /// is not its state; nothing changes.
    pub fn update_selection(
        &self,
        sel: &mut Selection,
        view: &SelectionView,
        labels: &SelectionLabels,
    ) -> Result<(), SelectionError> {
        if !self.owns(sel) {
            return Err(SelectionError::Kind);
        }
        sel.update(view, labels, self.song_list_center());
        Ok(())
    }

    /// Fills `sel`'s settings' fields and the master volume's ratio from
    /// `settings`, apart from the view, which a later
    /// [`update_selection`](Self::update_selection) leaves as they are.
    /// Writes fields only: no string, no allocation, so a frame may call
    /// it.
    ///
    /// # Errors
    ///
    /// [`SelectionError::Kind`] when this is not a selection skin or `sel`
    /// is not its state; nothing changes.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn update_selection_settings(
        &self,
        sel: &mut Selection,
        settings: &SelectionSettings,
    ) -> Result<(), SelectionError> {
        if !self.owns(sel) {
            return Err(SelectionError::Kind);
        }
        sel.settings(settings);
        Ok(())
    }

    /// The lines to rasterize for `sel`'s strings on a `target`-pixel
    /// target with `layout`: each text element's, as
    /// [`text_requests`](Self::text_requests) makes them, with the
    /// selection's strings; and each song list row's, in its row style's
    /// font, its em from that style's keys under the song list's region.
    /// A slot in a bitmap font gives none. For a view change, not a frame:
    /// it allocates. A state this skin did not make gives none.
    #[must_use]
    pub fn selection_text_requests(
        &self,
        sel: &Selection,
        target: [u32; 2],
        layout: &Layout,
    ) -> SelectionRequests {
        let mut out = SelectionRequests::default();
        if !self.owns(sel) {
            return out;
        }
        let (fit, ..) = self.fit(target);
        let em = |track: &Track, region: Region| self.em_px(track, region, fit, layout);
        let strings = &sel.strings;
        let chart = strings.chart();
        for e in &self.elements {
            let Kind::Text(i, t) = e.kind else {
                continue;
            };
            let Some(def) = self.texts.get(i) else {
                continue;
            };
            if t.bitmap.is_some() || e.visible.as_ref().and_then(Program::constant) == Some(0) {
                continue;
            }
            let s = clean(
                &def.value
                    .resolve_with(&chart, &strings.query, &strings.place),
            );
            if !s.is_empty() {
                out.texts.push(TextRequest {
                    slot: t.slot,
                    text: s,
                    font: def.font,
                    em_px: em(&e.track, e.region),
                });
            }
        }
        let (Some(list), Some(region)) = (&self.song_list, self.song_list_region()) else {
            return out;
        };
        for (i, (row, s)) in sel.rows.iter().zip(sel.row_strings.iter()).enumerate() {
            let Some(t) = row
                .style
                .and_then(|s| list.text.get(s.index()))
                .and_then(Option::as_ref)
            else {
                continue;
            };
            let (text, font) = t.content;
            if text.bitmap.is_some() {
                continue;
            }
            let s = clean(s);
            if !s.is_empty() {
                out.rows.push(TextRequest {
                    slot: u16::try_from(i).unwrap_or(u16::MAX),
                    text: s,
                    font,
                    em_px: em(&t.track, region),
                });
            }
        }
        out
    }

    /// The region the first song list element draws in.
    fn song_list_region(&self) -> Option<Region> {
        self.elements
            .iter()
            .find(|e| matches!(e.kind, Kind::SongList))
            .map(|e| e.region)
    }

    /// Attaches the rasterized lines for `sel`'s text elements and song
    /// list rows; a slot in a bitmap font draws its bitmap line instead.
    ///
    /// # Errors
    ///
    /// A table whose length is not the text slots, or the song list's
    /// slots; nothing changes.
    pub fn attach_selection_text(
        &self,
        sel: &mut Selection,
        texts: TextTable,
        rows: TextTable,
    ) -> Result<(), TextTableError> {
        if !self.owns(sel) {
            return Err(TextTableError::Foreign);
        }
        for (expected, got) in [
            (self.text_slots(), texts.len()),
            (self.song_list_slots(), rows.len()),
        ] {
            if got != expected {
                return Err(TextTableError::Len { expected, got });
            }
        }
        sel.text = texts;
        sel.row_text = rows;
        Ok(())
    }

    /// Lays out `sel`'s lines in bitmap fonts: each text element's and
    /// each song list row's in its style's font, chosen and cleaned as
    /// [`selection_text_requests`](Self::selection_text_requests) does.
    /// For a view change, not a frame: it allocates.
    pub fn attach_selection_bitmap_text(&self, sel: &mut Selection) {
        if !self.owns(sel) {
            return;
        }
        let mut lines = vec![None; self.text_slots()];
        {
            let strings = &sel.strings;
            let chart = strings.chart();
            for e in &self.elements {
                let Kind::Text(i, t) = e.kind else {
                    continue;
                };
                let (Some(font), Some(def)) = (t.bitmap, self.texts.get(i)) else {
                    continue;
                };
                if e.visible.as_ref().and_then(Program::constant) == Some(0) {
                    continue;
                }
                let s = clean(
                    &def.value
                        .resolve_with(&chart, &strings.query, &strings.place),
                );
                if let Some(slot) = lines.get_mut(usize::from(t.slot)) {
                    *slot = glyphs::layout(&self.sheet, font, &s);
                }
            }
        }
        sel.bitmap = lines.into_boxed_slice();
        let Some(list) = &self.song_list else {
            return;
        };
        let rows: Vec<_> = sel
            .rows
            .iter()
            .zip(sel.row_strings.iter())
            .map(|(row, s)| {
                let t = row
                    .style
                    .and_then(|s| list.text.get(s.index()))
                    .and_then(Option::as_ref)?;
                let font = t.content.0.bitmap?;
                glyphs::layout(&self.sheet, font, &clean(s))
            })
            .collect();
        sel.row_bitmap = rows.into_boxed_slice();
    }

    /// Prepares the timers of `sel`'s frame at `now` from `timers`: the
    /// built-in timers start from the frame's starts and the `when` timers
    /// over the fields, as the build would. The build of the same frame
    /// (the same `now`) then takes them as they are, so what runs between
    /// the two (the screen's runtime, docs/stage3.md S7-07) reads the
    /// state the build draws. Allocates nothing.
    ///
    /// # Errors
    ///
    /// [`BuildError::Kind`] for a skin that is not a selection skin.
    pub fn prepare_selection_frame(
        &self,
        sel: &mut Selection,
        now: TimeUs,
        timers: &SelectionTimers,
    ) -> Result<(), BuildError> {
        if self.kind != ScreenKind::Selection {
            return Err(BuildError::Kind);
        }
        if self.owns(sel) {
            sel.fill(now, timers, &self.whens);
            sel.prepared = Some(now);
        }
        Ok(())
    }

    /// Builds one selection frame of `sel`, which must come from this
    /// skin's [`prepare_selection`](Self::prepare_selection): starts the
    /// built-in timers from the frame's starts and the `when` timers over
    /// the fields, unless [`prepare_selection_frame`](Self::prepare_selection_frame)
    /// prepared them for this frame's time, places the regions, then appends each drawn element's
    /// quads to `out` in draw order (the caller clears `out`): images,
    /// image choices, numbers, text, the song list's slots and the ratio
    /// bars, recording each changeable slider's travel for
    /// [`slider_at`](Self::slider_at). Allocates nothing.
    ///
    /// A state this skin did not make, or a uv table of another sheet or
    /// length, draws nothing.
    ///
    /// # Errors
    ///
    /// [`BuildError::Kind`] for a skin that is not a selection skin;
    /// nothing is appended.
    pub fn build_selection(
        &self,
        sel: &mut Selection,
        frame: &SelectionFrame<'_>,
        uvs: &UvTable,
        out: &mut QuadBatch,
    ) -> Result<(), BuildError> {
        if self.kind != ScreenKind::Selection {
            return Err(BuildError::Kind);
        }
        if !self.owns(sel) {
            return Ok(());
        }
        for h in &sel.hits {
            h.set(None);
        }
        if uvs.sheet_id() != self.sheet_id || uvs.frames().len() != self.frame_count {
            return Ok(());
        }
        let canvas = self.canvas(frame.target);
        let placement = self.placement(&canvas, frame.layout, Side::Left);
        if sel.prepared.take() != Some(frame.now) {
            sel.fill(frame.now, &frame.timers, &self.whens);
        }
        let sel = &*sel;
        let rt = Runtime {
            starts: sel.starts(),
            state: sel.state(),
            text: &sel.text,
            bitmap: &sel.bitmap,
        };
        let scene = Scene {
            turntable: 0,
            now: frame.now,
            display: frame.now,
            lanes: LaneView::NONE,
            complete: false,
            hidden_enabled: false,
            phase: Phase::Selection { sel },
        };
        self.draw(&rt, &scene, None, uvs, &canvas, &placement, out);
        Ok(())
    }

    /// What a press at `point` (target pixels) on a `target`-pixel target
    /// asks of a changeable slider drawn by the last
    /// [`build_selection`](Self::build_selection) of `sel`: the ratio under
    /// it on the topmost such slider's travel, as the source reads it
    /// (`skin/SkinSlider.java:124-178`), the ends snapped within one
    /// reference pixel. `None` off every travel, for a slider that did not
    /// draw in that build, or when that build was at another target size.
    /// Changes nothing.
    #[must_use]
    pub fn slider_at(
        &self,
        sel: &Selection,
        point: [f32; 2],
        target: [u32; 2],
    ) -> Option<SliderCommand> {
        if !self.owns(sel) {
            return None;
        }
        sel.hits
            .iter()
            .rev()
            .filter_map(Cell::get)
            .filter(|h| h.target == target)
            .find_map(|h| h.at(point))
    }

    /// A fresh result state ([`ResultScreen`]): every timer off, every
    /// field 0 and every validity false, every text slot empty until a
    /// view and a text table. One a skin of another kind makes is inert: it
    /// draws nothing.
    #[must_use]
    pub fn prepare_result(&self) -> ResultScreen {
        if self.kind != ScreenKind::Result {
            return ResultScreen::inert(self.id);
        }
        ResultScreen::new(
            self.id,
            true,
            self.whens.len(),
            self.text_slots(),
            self.outputs,
        )
    }

    /// Whether `res` is a state this result skin made.
    fn owns_result(&self, res: &ResultScreen) -> bool {
        self.kind == ScreenKind::Result
            && res.live
            && res.skin == self.id
            && res.slots() == usize::from(result_timer::BUILTIN) + self.whens.len()
            && res.text.len() == self.text_slots()
    }

    /// Fills `res` from `view` and the chart's `labels`: the fields, the
    /// timing statistics, the timelines, the gauge history, the
    /// deviations, the judge areas and the strings its text shows.
    /// The chart's fields follow the view under every status; the result's,
    /// the timing's and the best's are valid only while the view is ready,
    /// and the best's only while it is known; a hidden best value reads
    /// `i32::MIN`. For a view change, not a frame: it may allocate, at most
    /// the strings' growth and the timelines' copy; the gauge history and
    /// the deviations are copied into storage the state already holds. The texts stay as
    /// attached until a new table is.
    ///
    /// # Errors
    ///
    /// [`ResultError::Kind`] when this is not a result skin or `res` is not
    /// its state; nothing changes.
    pub fn update_result(
        &self,
        res: &mut ResultScreen,
        view: &ResultView,
        labels: &ResultLabels,
    ) -> Result<(), ResultError> {
        if !self.owns_result(res) {
            return Err(ResultError::Kind);
        }
        res.update(view, labels);
        Ok(())
    }

    /// The lines to rasterize for `res`'s strings on a `target`-pixel
    /// target with `layout`: each text element's, as
    /// [`text_requests`](Self::text_requests) makes them, with the chart's
    /// strings of the last view. A slot in a bitmap font gives none. For a
    /// view change, not a frame: it allocates. A state this skin did not
    /// make gives none.
    #[must_use]
    pub fn result_text_requests(
        &self,
        res: &ResultScreen,
        target: [u32; 2],
        layout: &Layout,
    ) -> Vec<TextRequest> {
        if !self.owns_result(res) {
            return Vec::new();
        }
        self.text_requests(&res.strings.chart(), target, layout)
    }

    /// Attaches the rasterized lines for `res`'s text elements; a slot in a
    /// bitmap font draws its bitmap line instead.
    ///
    /// # Errors
    ///
    /// A state this skin did not make, or a table whose length is not the
    /// text slots; nothing changes.
    pub fn attach_result_text(
        &self,
        res: &mut ResultScreen,
        table: TextTable,
    ) -> Result<(), TextTableError> {
        self.attach_table(self.owns_result(res), &mut res.text, table)
    }

    /// Lays out `res`'s lines in bitmap fonts, chosen and cleaned as
    /// [`result_text_requests`](Self::result_text_requests) does. For a
    /// view change, not a frame: it allocates.
    pub fn attach_result_bitmap_text(&self, res: &mut ResultScreen) {
        if !self.owns_result(res) {
            return;
        }
        res.bitmap = self.bitmap_lines(&res.strings.chart());
    }

    /// Prepares the timers of `res`'s frame at `now` from `timers`, as
    /// [`prepare_selection_frame`](Self::prepare_selection_frame) does a
    /// selection's: the build of the same frame (the same `now`) then takes
    /// them as they are, so the screen's runtime between the two reads the
    /// state the build draws (docs/stage3.md S7-07). Allocates nothing.
    ///
    /// # Errors
    ///
    /// [`BuildError::Kind`] for a skin that is not a result skin.
    pub fn prepare_result_frame(
        &self,
        res: &mut ResultScreen,
        now: TimeUs,
        timers: &ResultTimers,
    ) -> Result<(), BuildError> {
        if self.kind != ScreenKind::Result {
            return Err(BuildError::Kind);
        }
        if self.owns_result(res) {
            res.fill(now, timers, &self.whens);
            res.prepared = Some(now);
        }
        Ok(())
    }

    /// Builds one result frame of `res`, which must come from this skin's
    /// [`prepare_result`](Self::prepare_result): starts the built-in timers
    /// from the frame's starts and the `when` timers over the fields,
    /// unless [`prepare_result_frame`](Self::prepare_result_frame) prepared
    /// them for this frame's time,
    /// places the regions, then appends each drawn element's quads to `out`
    /// in draw order (the caller clears `out`): images, image choices,
    /// numbers, text, judge graphs, gauge graphs and timing
    /// distributions. A number draws nothing for `i32::MIN` or
    /// `i32::MAX`. Allocates nothing.
    ///
    /// A state this skin did not make, or a uv table of another sheet or
    /// length, draws nothing.
    ///
    /// # Errors
    ///
    /// [`BuildError::Kind`] for a skin that is not a result skin; nothing
    /// is appended.
    pub fn build_result(
        &self,
        res: &mut ResultScreen,
        frame: &ResultFrame<'_>,
        uvs: &UvTable,
        out: &mut QuadBatch,
    ) -> Result<(), BuildError> {
        if !self.screen_draws(ScreenKind::Result, self.owns_result(res), uvs)? {
            return Ok(());
        }
        if res.prepared.take() != Some(frame.now) {
            res.fill(frame.now, &frame.timers, &self.whens);
        }
        let res = &*res;
        let rt = Runtime {
            starts: res.starts(),
            state: res.state(),
            text: &res.text,
            bitmap: &res.bitmap,
        };
        let at = (frame.now, frame.target, frame.layout);
        self.draw_screen(&rt, Phase::Result { res }, at, uvs, out);
        Ok(())
    }

    /// A fresh decision state ([`DecisionScreen`]): every timer off, every
    /// text slot empty until labels and a text table. One a skin of
    /// another kind makes is inert: it draws nothing.
    #[must_use]
    pub fn prepare_decision(&self) -> DecisionScreen {
        if self.kind != ScreenKind::Decision {
            return DecisionScreen::inert(self.id);
        }
        DecisionScreen::new(self.id, self.whens.len(), self.text_slots())
    }

    /// Whether `dec` is a state this decision skin made.
    fn owns_decision(&self, dec: &DecisionScreen) -> bool {
        self.kind == ScreenKind::Decision
            && dec.skin == self.id
            && dec.slots() == usize::from(decision_timer::BUILTIN) + self.whens.len()
            && dec.text.len() == self.text_slots()
    }

    /// Fills `dec`'s strings from the chosen chart's `labels`, as
    /// [`update_result`](Self::update_result) fills a result's. It may
    /// allocate, at most the strings' growth. The texts stay as attached
    /// until a new table is.
    ///
    /// # Errors
    ///
    /// [`DecisionError::Kind`] when this is not a decision skin or `dec` is
    /// not its state; nothing changes.
    pub fn update_decision(
        &self,
        dec: &mut DecisionScreen,
        labels: &ResultLabels,
    ) -> Result<(), DecisionError> {
        if !self.owns_decision(dec) {
            return Err(DecisionError::Kind);
        }
        dec.strings.set(labels);
        Ok(())
    }

    /// The lines to rasterize for `dec`'s strings on a `target`-pixel
    /// target with `layout`: each text element's, as
    /// [`text_requests`](Self::text_requests) makes them, with the labels
    /// last given. A slot in a bitmap font gives none. It allocates. A
    /// state this skin did not make gives none.
    #[must_use]
    pub fn decision_text_requests(
        &self,
        dec: &DecisionScreen,
        target: [u32; 2],
        layout: &Layout,
    ) -> Vec<TextRequest> {
        if !self.owns_decision(dec) {
            return Vec::new();
        }
        self.text_requests(&dec.strings.chart(), target, layout)
    }

    /// Attaches the rasterized lines for `dec`'s text elements; a slot in
    /// a bitmap font draws its bitmap line instead.
    ///
    /// # Errors
    ///
    /// A state this skin did not make, or a table whose length is not the
    /// text slots; nothing changes.
    pub fn attach_decision_text(
        &self,
        dec: &mut DecisionScreen,
        table: TextTable,
    ) -> Result<(), TextTableError> {
        self.attach_table(self.owns_decision(dec), &mut dec.text, table)
    }

    /// Lays out `dec`'s lines in bitmap fonts, chosen and cleaned as
    /// [`decision_text_requests`](Self::decision_text_requests) does. It
    /// allocates.
    pub fn attach_decision_bitmap_text(&self, dec: &mut DecisionScreen) {
        if !self.owns_decision(dec) {
            return;
        }
        dec.bitmap = self.bitmap_lines(&dec.strings.chart());
    }

    /// Builds one decision frame of `dec`, which must come from this
    /// skin's [`prepare_decision`](Self::prepare_decision): starts the
    /// built-in timers from the frame's starts and the `when` timers,
    /// places the regions, then appends each drawn element's quads to
    /// `out` in draw order (the caller clears `out`): images, image
    /// choices and text. Allocates nothing.
    ///
    /// A state this skin did not make, or a uv table of another sheet or
    /// length, draws nothing.
    ///
    /// # Errors
    ///
    /// [`BuildError::Kind`] for a skin that is not a decision skin;
    /// nothing is appended.
    pub fn build_decision(
        &self,
        dec: &mut DecisionScreen,
        frame: &DecisionFrame<'_>,
        uvs: &UvTable,
        out: &mut QuadBatch,
    ) -> Result<(), BuildError> {
        if !self.screen_draws(ScreenKind::Decision, self.owns_decision(dec), uvs)? {
            return Ok(());
        }
        dec.fill(frame.now, &frame.timers, &self.whens);
        let dec = &*dec;
        let rt = Runtime {
            starts: dec.starts(),
            state: dec.state(),
            text: &dec.text,
            bitmap: &dec.bitmap,
        };
        let at = (frame.now, frame.target, frame.layout);
        self.draw_screen(&rt, Phase::Decision, at, uvs, out);
        Ok(())
    }

    /// Attaches `table` as a screen state's text, the state `owned` by
    /// this skin.
    ///
    /// # Errors
    ///
    /// [`TextTableError::Foreign`] for a state this skin did not make,
    /// then [`TextTableError::Len`] for a table whose length is not the
    /// text slots; nothing changes.
    fn attach_table(
        &self,
        owned: bool,
        text: &mut TextTable,
        table: TextTable,
    ) -> Result<(), TextTableError> {
        if !owned {
            return Err(TextTableError::Foreign);
        }
        let expected = self.text_slots();
        if table.len() != expected {
            return Err(TextTableError::Len {
                expected,
                got: table.len(),
            });
        }
        *text = table;
        Ok(())
    }

    /// Whether a build of a `kind` screen's state draws: not for a state
    /// this skin did not make (`owned`) or a uv table of another sheet or
    /// length.
    ///
    /// # Errors
    ///
    /// [`BuildError::Kind`] for a skin of another kind.
    fn screen_draws(
        &self,
        kind: ScreenKind,
        owned: bool,
        uvs: &UvTable,
    ) -> Result<bool, BuildError> {
        if self.kind != kind {
            return Err(BuildError::Kind);
        }
        Ok(owned && uvs.sheet_id() == self.sheet_id && uvs.frames().len() == self.frame_count)
    }

    /// Draws a filled screen state's frame: `at` is the frame's time,
    /// target size and layout; no lanes, the regions placed for the
    /// left-hand scratch.
    fn draw_screen(
        &self,
        rt: &Runtime<'_>,
        phase: Phase<'_>,
        at: (TimeUs, [u32; 2], &Layout),
        uvs: &UvTable,
        out: &mut QuadBatch,
    ) {
        let (now, target, layout) = at;
        let canvas = self.canvas(target);
        let placement = self.placement(&canvas, layout, Side::Left);
        let scene = Scene {
            turntable: 0,
            now,
            display: now,
            lanes: LaneView::NONE,
            complete: false,
            hidden_enabled: false,
            phase,
        };
        self.draw(rt, &scene, None, uvs, &canvas, &placement, out);
    }

    /// The elements of a build once its state is filled, the lanes drawing
    /// their bar groups with `bars`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the build's own state, split once"
    )]
    fn draw(
        &self,
        rt: &Runtime<'_>,
        scene: &Scene<'_>,
        mut bars: Option<&mut BarScratch>,
        uvs: &UvTable,
        canvas: &Canvas,
        placement: &Placement,
        out: &mut QuadBatch,
    ) {
        let state = &rt.state;
        let now = scene.now;
        // A table of another length draws no text.
        let texts = rt.text.len() == self.text_slots();
        let bitmaps = rt.bitmap.len() == self.text_slots();
        // The lanes' geometry, once for every element, whether or not the
        // lanes draw.
        let lanes = self
            .lanes
            .as_ref()
            .map(|l| l.frame_geometry(scene.lanes, scene.hidden_enabled));
        let play = match scene.phase {
            Phase::Play { play, notes, .. } => Some((play, notes)),
            Phase::Loading { .. }
            | Phase::Selection { .. }
            | Phase::Result { .. }
            | Phase::Decision => None,
        };
        let sel = match scene.phase {
            Phase::Selection { sel } => Some(sel),
            Phase::Play { .. } | Phase::Loading { .. } | Phase::Result { .. } | Phase::Decision => {
                None
            }
        };
        #[cfg(feature = "dev-tools")]
        let mut element = 0_u32;
        for e in &self.elements {
            #[cfg(feature = "dev-tools")]
            {
                out.observe(element);
                element = element.saturating_add(1);
            }
            // While loading, an element reading what loading lacks draws
            // nothing: its condition is false, its value or selector
            // unread.
            if let Phase::Loading { gauge, chosen } = scene.phase
                && !e.loading.met(gauge.is_some(), chosen)
            {
                continue;
            }
            if let (Kind::SongList, Some(sel), Some(list)) = (e.kind, sel, &self.song_list) {
                if !placement.region(e.region).visible {
                    continue;
                }
                let Some((_t, s)) =
                    sample(rt.starts, now, state, e.slot, e.visible.as_ref(), &e.track)
                else {
                    continue;
                };
                let d = ListDraw {
                    starts: rt.starts,
                    now,
                    state,
                    rows: &sel.rows,
                    text: &sel.row_text,
                    bitmap: &sel.row_bitmap,
                    uvs,
                    brush: Brush {
                        key: e.key,
                        affine: placement.affine(e.region, e.fill),
                        map: placement.bind(e.bind),
                        pivot: e.pivot,
                        angle: 0.0,
                        color: s.color,
                        native: None,
                        scale: canvas.scale(),
                    },
                };
                list.draw(&d, out);
                continue;
            }
            if let Kind::Lanes = e.kind {
                let (Some(widget), Some(geometry)) = (&self.lanes, lanes.as_ref()) else {
                    continue;
                };
                if !placement.region(e.region).visible {
                    #[cfg(feature = "dev-tools")]
                    out.observed(crate::observation::Outcome::RegionHidden);
                    continue;
                }
                let Some((_t, s)) =
                    sample(rt.starts, now, state, e.slot, e.visible.as_ref(), &e.track)
                else {
                    #[cfg(feature = "dev-tools")]
                    out.observed(crate::observation::Outcome::NotSampled);
                    continue;
                };
                #[cfg(feature = "dev-tools")]
                out.observed_time(_t);
                let d = Draw {
                    play,
                    starts: rt.starts,
                    state,
                    now,
                    display: scene.display,
                    view: scene.lanes,
                    frame: geometry,
                    complete: scene.complete,
                    geometry: placement.geometry(),
                    placement,
                    affine: placement.affine(e.region, e.fill),
                    color: s.color,
                    key: e.key,
                    uvs,
                    scale: canvas.scale(),
                };
                widget.draw(&d, bars.as_deref_mut(), out);
                continue;
            }
            if !placement.region(e.region).visible {
                #[cfg(feature = "dev-tools")]
                out.observed(crate::observation::Outcome::RegionHidden);
                continue;
            }
            let paint = match e.kind {
                Kind::CoverPaint(i) => self.cover_paints.get(i),
                _ => None,
            };
            // HIDDEN+'s paint, and what follows HIDDEN+, show only while
            // HIDDEN+ is on, at any amount.
            if (paint.is_some_and(|p| p.motion == CoverMotion::Hidden) && !scene.hidden_enabled)
                || e.follow.hides(lanes.as_ref())
            {
                #[cfg(feature = "dev-tools")]
                out.observed(crate::observation::Outcome::HiddenOff);
                continue;
            }
            // An image choice's entry, before its track is sampled.
            let table = match (e.kind, paint.map(|p| p.image)) {
                (Kind::ImageChoice(i), _) | (_, Some(PaintImage::Choice(i))) => Some(i),
                _ => None,
            };
            let choice = match table {
                Some(i) => {
                    let Some(p) = self.image_choices.get(i).and_then(|c| c.select(state)) else {
                        #[cfg(feature = "dev-tools")]
                        out.observed(crate::observation::Outcome::NoChoice);
                        continue;
                    };
                    #[cfg(feature = "dev-tools")]
                    out.observed_choice(self.image_choices.get(i).and_then(|c| c.index_of(p)));
                    Some(p)
                }
                None => None,
            };
            let track = choice.and_then(|p| p.track.as_ref()).unwrap_or(&e.track);
            let Some((t, s)) = sample(rt.starts, now, state, e.slot, e.visible.as_ref(), track)
            else {
                #[cfg(feature = "dev-tools")]
                out.observed(crate::observation::Outcome::NotSampled);
                continue;
            };
            #[cfg(feature = "dev-tools")]
            out.observed_time(t);
            let affine = placement.affine(e.region, e.fill);
            let map = placement.bind(e.bind);
            // The covers' motion, after the track and before the lane map
            // and the region; lanes and judges never follow. A paint also
            // moves with its cover and its slider, the sum kept in `f64`
            // until it is drawn, and its gate, the track's rect or its own,
            // moves alike for its trim.
            let (rect, trim) = match paint {
                Some(p) => {
                    let [x, y, w, h] = s.rect;
                    let g = lanes.as_ref();
                    let progress = state
                        .fields
                        .get(usize::from(field::TIME_PROGRESS_PPM))
                        .copied()
                        .unwrap_or(0);
                    // A value slider's output sits after the play fields.
                    let [dx, slide] =
                        match p.slider_offset(scene.lanes, progress, state.fields, field::COUNT) {
                            Ok(d) => d,
                            Err(_why) => {
                                #[cfg(feature = "dev-tools")]
                                out.observed(match _why {
                                    crate::widget::cover_paint::SlideStop::NoValue => {
                                        crate::observation::Outcome::NoValue
                                    }
                                    crate::widget::cover_paint::SlideStop::Beyond => {
                                        crate::observation::Outcome::SlideBeyond
                                    }
                                });
                                continue;
                            }
                        };
                    let dy = e.follow.dy(g) + p.motion_dy(g) + slide;
                    let gate = match &p.gate {
                        Some(gate) => match gate.sample(t) {
                            Some(gs) => gs.rect,
                            None => {
                                #[cfg(feature = "dev-tools")]
                                out.observed(crate::observation::Outcome::GateNotSampled);
                                continue;
                            }
                        },
                        None => s.rect,
                    };
                    let clip = match p.outcome(gate, dy, g, map, affine) {
                        TrimOutcome::Suppressed => {
                            #[cfg(feature = "dev-tools")]
                            out.observed(crate::observation::Outcome::TrimSuppressed);
                            continue;
                        }
                        TrimOutcome::Unclipped => None,
                        TrimOutcome::Clipped(c) => Some(c),
                    };
                    (
                        [f32_of(f64::from(x) + dx), f32_of(f64::from(y) + dy), w, h],
                        clip,
                    )
                }
                None => (e.follow.translate(s.rect, lanes.as_ref()), None),
            };
            // A text keeps its own angle but never turns with the
            // turntable, as the source draws text unrotated by it.
            let turn = match e.kind {
                Kind::Text(..) => 0.0,
                _ => e.angle.degrees(scene.turntable),
            };
            let brush = Brush {
                key: e.key,
                affine,
                map,
                pivot: e.pivot,
                angle: radians(s.angle + turn),
                color: s.color,
                native: choice.map_or(e.native, |p| p.native),
                scale: canvas.scale(),
            };
            let at = Frame {
                starts: rt.starts,
                now,
                uvs,
            };
            match e.kind {
                Kind::Image(_) | Kind::ImageChoice(_) | Kind::CoverPaint(_) => {
                    let image = match (choice, e.kind, paint.map(|p| p.image)) {
                        (Some(p), ..) => Some(p.image),
                        (None, Kind::Image(image), _)
                        | (None, _, Some(PaintImage::Image(image))) => Some(image),
                        _ => None,
                    };
                    if let Some(image) = image
                        && let Some(f) = image.frame(rt.starts, now, uvs)
                    {
                        brush.push_clipped(out, rect, f, image.filter, trim);
                    }
                }
                Kind::Number(i) => {
                    if let Some(n) = self.numbers.get(i) {
                        let v = n.value.eval(state);
                        #[cfg(feature = "dev-tools")]
                        out.observed_value(i64::from(v));
                        // A number draws nothing for the values a source
                        // number reserves (`SkinNumber.java:135-147`): a
                        // hidden value, a resolved value the source hides,
                        // or a play skin's program value reads one.
                        let reserved = v == i32::MIN || v == i32::MAX;
                        if !reserved {
                            n.digits.draw(v, rect, &brush, &at, out);
                        }
                    }
                }
                Kind::Gauge(i) => {
                    if let Some(g) = self.gauges.get(i) {
                        let shown = match scene.phase {
                            Phase::Play {
                                play,
                                gauges,
                                shown,
                                ..
                            } => Shown::of(gauges, play.gauge_desc(shown), shown),
                            // The empty frame: no value while loading.
                            Phase::Loading { gauge, .. } => gauge.map(|g| Shown {
                                look: g.look,
                                max: g.max,
                                border: g.border,
                                value: GaugeValue(0),
                            }),
                            Phase::Selection { .. } | Phase::Result { .. } | Phase::Decision => {
                                None
                            }
                        };
                        let d = GaugeDraw { at, shown, t };
                        g.draw(&d, rect, &brush, out);
                    }
                }
                Kind::Judge(i) => {
                    // Judgment feedback never draws while loading.
                    if let (Some(j), Some((play, _))) = (self.judges.get(i), play) {
                        let d = JudgeDraw {
                            at,
                            grade: play.judge(),
                            state,
                            affine,
                            map: brush.map,
                            lanes: lanes.as_ref(),
                            color: s.color,
                            clip: e.key.clip,
                            scale: canvas.scale(),
                        };
                        j.draw(&d, out);
                    }
                }
                Kind::Text(_, t) => {
                    if t.bitmap.is_some() {
                        if let Some(Some(line)) =
                            rt.bitmap.get(usize::from(t.slot)).filter(|_| bitmaps)
                        {
                            t.draw_glyphs(line, rect, &brush, uvs, out);
                        }
                    } else if let Some(image) = rt.text.get(t.slot).filter(|_| texts) {
                        t.draw(image, rect, &brush, out);
                    }
                }
                Kind::RatioBar(i, hit) => {
                    if let (Some(b), Some(sel)) = (self.ratio_bars.get(i), sel) {
                        let image = b.image;
                        let b = &b.def;
                        let value = match b.source {
                            RatioSource::ListPosition => sel.position,
                            RatioSource::ExRate => sel.ex_rate,
                            RatioSource::MasterVolume => sel.master_volume,
                        };
                        if let Some(v) = value {
                            draw_ratio(b, image, v, rect, &brush, &at, out);
                            if let (
                                Some(h),
                                RatioForm::Slider {
                                    direction, range, ..
                                },
                            ) = (hit.and_then(|h| sel.hits.get(h)), b.form)
                            {
                                let [ox, oy] = canvas.origin();
                                let sc = canvas.scale();
                                let [x, y, w, h_] = affine.rect(rect);
                                let [sx, sy] = affine.scale;
                                let axis = match direction {
                                    SliderDirection::Up | SliderDirection::Down => sy,
                                    SliderDirection::Right | SliderDirection::Left => sx,
                                } * sc;
                                #[expect(clippy::cast_precision_loss, reason = "within ±2^20")]
                                let range = range as f32 * axis;
                                h.set(Some(Hit {
                                    source: b.source,
                                    direction,
                                    rect: [ox + x * sc, oy + y * sc, w * sc, h_ * sc],
                                    range,
                                    scale: axis,
                                    target: canvas.target(),
                                }));
                            }
                        }
                    }
                }
                Kind::JudgeGraph(i) => {
                    if let (Some(g), Phase::Result { res }) =
                        (self.judge_graphs.get(i), scene.phase)
                    {
                        let d = GraphDraw {
                            at,
                            timelines: res.timelines(),
                            screen: screen_elapsed(rt.starts, now),
                        };
                        g.draw(&d, rect, &brush, out);
                    }
                }
                Kind::GaugeGraph(i) => {
                    if let (Some(g), Phase::Result { res }) =
                        (self.gauge_graphs.get(i), scene.phase)
                    {
                        let d = GaugeGraphDraw {
                            at,
                            gauge: res.gauge(),
                            screen: screen_elapsed(rt.starts, now),
                        };
                        g.draw(&d, rect, &brush, out);
                    }
                }
                Kind::TimingGraph(i) => {
                    if let (Some(g), Phase::Result { res }) =
                        (self.timing_graphs.get(i), scene.phase)
                    {
                        let d = TimingGraphDraw {
                            at,
                            deviations: res.deviations(),
                            timing: res.timing(),
                            areas: res.areas(),
                            ready: res.ready(),
                        };
                        g.draw(&d, rect, &brush, out);
                    }
                }
                Kind::Lanes | Kind::SongList => {}
            }
        }
        #[cfg(feature = "dev-tools")]
        out.end_observations();
    }

    /// The string text element `element` shows for `chart`, if it is one.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn element_text(&self, element: u32, chart: &ChartText<'_>) -> Option<String> {
        let e = self.elements.get(usize::try_from(element).ok()?)?;
        let Kind::Text(i, _) = e.kind else {
            return None;
        };
        let def = self.texts.get(i)?;
        Some(text::clean(&def.value.resolve(chart)))
    }

    /// The skin's number of elements.
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub const fn element_count(&self) -> usize {
        self.elements.len()
    }
}

/// The result screen timer's elapsed µs at `now`, `None` while it is off.
fn screen_elapsed(starts: Starts<'_>, now: TimeUs) -> Option<i64> {
    starts
        .at(usize::from(result_timer::SCREEN))
        .map(|at| elapsed_us(now, at))
}

/// V9: each cover paint whose slider is [`SliderRate::Program`] names a
/// float output of `signature`, and no float output is named by two
/// sliders. A float output no slider names is accepted, as an unread
/// number is.
fn check_slider_outputs(paints: &[CoverPaintDef], signature: &[OutTy]) -> Result<(), LoadError> {
    let mut taken = vec![false; signature.len()];
    for (paint, p) in paints.iter().enumerate() {
        let Some(crate::Slider {
            rate: SliderRate::Program(k),
            ..
        }) = p.slider
        else {
            continue;
        };
        let k = usize::from(k);
        let fresh = signature.get(k) == Some(&OutTy::Float)
            && taken
                .get_mut(k)
                .is_some_and(|t| !std::mem::replace(t, true));
        if !fresh {
            return Err(LoadError::SliderOutput { paint });
        }
    }
    Ok(())
}

/// V6 for `program`: each read past the schema's `base` fields is an output
/// read, resolved by name, of the output of `signature` at that slot, at
/// its type (`lua.<k>` the signature's, `lua.<k>.ok` a boolean); a field
/// read there, or any output read in a `when` timer, is refused.
fn check_outputs(
    program: &Program,
    base: usize,
    signature: &[OutTy],
    when: bool,
) -> Result<(), LoadError> {
    for (slot, out) in program.loads() {
        let Some(at) = usize::from(slot).checked_sub(base) else {
            continue;
        };
        let want = signature.get(at / 2).ok_or(LoadError::OutputRead)?;
        let ok = at % 2 == 1;
        // V8: a timer output is read only as an element's timer.
        let matches = match out {
            _ if *want == OutTy::Timer => false,
            // A float output's value is read only by its slider (V9).
            _ if *want == OutTy::Float && !ok => false,
            Some((ty, o)) => o == ok && (ok || ty == *want),
            None => false,
        };
        if when || !matches {
            return Err(LoadError::OutputRead);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::radians;

    /// Only a text whose visibility does not fold to false is laid out.
    #[test]
    fn hidden_bitmap_text_is_not_laid_out() {
        use crate::{
            Align, Anchor, Attempt, Blend, ChartText, Ease, ElementDef, End, Fit, FontRef, KeyDef,
            Overflow, PLAY, Place, PlayContext, Region, RegionDef, Root, Scope, SheetBuilder, Side,
            Skin, SkinDef, Source, TextDef, TextValue, TimerRef, Track, Ty, WidgetDefs, compile,
            timer,
        };
        use bms_model::{ChartBuilder, TimeUs};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("f.fnt"),
            "info face=\"t\" size=10 padding=0,0,0,0\n\
             common lineHeight=12 base=10 scaleW=8 scaleH=8 pages=1\n\
             page id=0 file=\"f.png\"\n\
             char id=65 x=0 y=0 width=4 height=4 xoffset=0 yoffset=0 xadvance=5 page=0\n",
        )
        .unwrap();
        image::RgbaImage::from_pixel(8, 8, image::Rgba([255; 4]))
            .save_with_format(dir.path().join("f.png"), image::ImageFormat::Png)
            .unwrap();
        let mut b = SheetBuilder::new(&Root::new(dir.path()).unwrap());
        let f = b.admit_fnt("f.fnt", &mut |_| Ok(()), &|| false).unwrap();
        b.add_bitmap_font(f, &[]).unwrap();
        let mut props = [None; 9];
        props[..4].copy_from_slice(&[Some(0), Some(0), Some(100), Some(10)]);
        let track = Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props,
            }],
            Ease::Linear,
            End::Hold,
        )
        .unwrap();
        let element = |i: u32, visible: &str| ElementDef {
            source: Source::Text(i),
            track: track.clone(),
            timer: TimerRef::Builtin(timer::READY),
            visible: Some(compile(visible, &Scope::new(&PLAY), Ty::Bool).unwrap()),
            blend: Blend::Alpha,
            clip: None,
            place: Place::SCREEN,
        };
        let text = TextDef {
            value: TextValue::Literal("A".into()),
            font: FontRef::Bitmap(0),
            align: Align::Left,
            overflow: Overflow::Spill,
        };
        let skin = Skin::new(SkinDef {
            fit: Fit::Adapt,
            regions: [RegionDef {
                anchor: Anchor::TopLeft,
                optional: false,
            }; Region::COUNT],
            whens: Vec::new(),
            sheet: b.finish(),
            lanes: None,
            elements: vec![element(0, "true"), element(1, "false")],
            widgets: WidgetDefs {
                texts: vec![text.clone(), text],
                ..WidgetDefs::default()
            },
            bomb_up_to: None,
            close: None,
            loadend: None,
            present: None,
        })
        .unwrap();
        let chart = ChartBuilder::new(150_000).build().unwrap();
        let ctx = PlayContext {
            meta: crate::ChartMetadata::UNKNOWN,
            autoplay: false,
            gauges: &[],
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: false,
            empty_poor_counted: false,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(0),
            bpm_range: None,
            bpm_whole_change: None,
            play_level: None,
        };
        let mut play = skin.prepare(&chart, &ctx).unwrap();
        skin.attach_bitmap_text(&mut play, &ChartText::default());
        let laid: Vec<bool> = play.bitmap().iter().map(Option::is_some).collect();
        assert_eq!(laid, [true, false]);
    }

    #[test]
    fn angles_reduce_into_one_turn() {
        assert_eq!(radians(0.0), 0.0);
        assert_eq!(radians(360.0), 0.0);
        // Just below a whole turn rounds to 2π in f32; it reads 0.
        assert_eq!(radians(-1e-6), 0.0);
        assert!(radians(359.0) < std::f32::consts::TAU);
    }
}
