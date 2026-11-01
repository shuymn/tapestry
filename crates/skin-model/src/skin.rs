//! A loaded skin: its elements in draw order, its regions, its `when`
//! timers, its images, its lanes and its text, and the per-frame build
//! into a [`QuadBatch`].

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Chart, LANE_COUNT, TimeUs};

use crate::atlas::uv::{Cycle, UvTable};
use crate::atlas::{Atlas, AtlasError, AtlasLimits, Variant};
use crate::canvas::{Canvas, Fit, MAX_CANVAS, f32_of};
use crate::expr::{CompileError, Program, SkinOps};
use crate::image::Sheet;
use crate::layout::{Controls, Layout, Region, RegionDef};
use crate::place::{Bind, Bounds, Fill, Follow, Native, Place, Placement, Regions, union};
use crate::play::schema::Grade;
use crate::play::schema::timer::BUILTIN;
use crate::play::{BuildError, Play, PlayContext, PrepareError, SkinFrame};
use crate::quad::{Blend, ClipId, Filter, QuadBatch, RunKey};
use crate::schema::Ty;
use crate::track::{MAX_KEY_US, Track};
use crate::widget::digits::{Align, Digits, Number, NumberDef, NumberError};
use crate::widget::gauge::{Gauge, GaugeDef, GaugeDraw, GaugeError};
use crate::widget::glyphs;
use crate::widget::image_choice::{
    ChoicePart, ImageChoice, ImageChoiceDef, ImageChoiceError, MAX_IMAGE_CHOICE_REFS,
};
use crate::widget::judge::{
    ComboStrip, GradeImage, Judge, JudgeDef, JudgeDraw, JudgeError, JudgePart, JudgeSlot,
};
use crate::widget::lanes::{Draw, LANE_QUADS, LANE_RESERVED, Lanes, LanesDef, LanesError, Side};
use crate::widget::text::{
    self, ChartText, FontRef, MAX_TEXT_ELEMENTS, MAX_TEXT_EM_PX, Overflow, Text, TextDef,
    TextError, TextRequest, TextTable, TextTableError, TextValue,
};
use crate::widget::{Brush, Frame, ImageRef, sample};

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
}

/// The widgets elements draw by index.
#[derive(Clone, Debug, Default)]
pub struct WidgetDefs {
    pub numbers: Vec<NumberDef>,
    pub gauges: Vec<GaugeDef>,
    pub judges: Vec<JudgeDef>,
    pub texts: Vec<TextDef>,
    pub image_choices: Vec<ImageChoiceDef>,
}

/// The timer an element's track or an image's cycle runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerRef {
    /// A built-in timer's slot (`play::timer::*`, `lane_timer`).
    Builtin(u16),
    /// The skin's `when` timer at this index.
    When(u16),
}

/// One element as a front end builds it. Every element names its timer;
/// a front end gives an element without one `ready`.
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
        "element {element} follows the covers, which only an image, a number, a gauge or a text may"
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
    #[error("image choice table {table}")]
    ImageChoice {
        table: usize,
        #[source]
        error: ImageChoiceError,
    },
    #[error("element {element} chooses its image and names a native size of its own")]
    ChoiceNative { element: usize },
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
}

/// A loaded skin.
#[derive(Clone, Debug)]
pub struct Skin {
    elements: Box<[Element]>,
    whens: Box<[Program]>,
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
    /// The text elements: the length of the table
    /// [`attach_text`](Skin::attach_text) takes.
    text_slots: u16,
    /// The quads one frame's widget elements can push, the lanes aside.
    widget_quads: u32,
    bomb_up_to: Option<Grade>,
}

/// A table index from a definition's `u32`, past every table when it
/// does not fit.
fn index(i: u32) -> usize {
    usize::try_from(i).unwrap_or(usize::MAX)
}

/// The slot `timer` reads with `whens` `when` timers: a built-in slot, or
/// `BUILTIN` plus the `when` index.
fn slot_of(timer: TimerRef, whens: usize) -> Option<usize> {
    let builtin = usize::from(BUILTIN);
    match timer {
        TimerRef::Builtin(s) if usize::from(s) < builtin => Some(usize::from(s)),
        TimerRef::When(i) if usize::from(i) < whens => Some(builtin + usize::from(i)),
        TimerRef::Builtin(_) | TimerRef::When(_) => None,
    }
}

/// `deg` degrees as radians in `[0, 2π)`, reduced first so large key
/// values keep their precision.
pub(crate) fn radians(deg: f32) -> f32 {
    let r = f32_of(f64::from(deg).rem_euclid(360.0).to_radians());
    if r < std::f32::consts::TAU { r } else { 0.0 }
}

/// The places checks of [`Skin::new`], in element order.
fn check_places(def: &SkinDef) -> Result<(), LoadError> {
    let fixed = def.fit == Fit::Fixed;
    let max = MAX_COORD;
    let outside = |t: &Track| {
        t.rects()
            .any(|r| r.iter().any(|v| !(-max..=max).contains(v)))
    };
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
        if outside(&e.track) {
            return Err(LoadError::Coordinates { element });
        }
        let widget = matches!(e.source, Source::Lanes | Source::Judge(_));
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
            if let Some((part, _)) = def
                .into_iter()
                .flat_map(JudgeDef::tracks)
                .find(|(_, t)| outside(t))
            {
                return Err(judge(JudgeError::Coordinates { part }));
            }
        }
        if matches!(e.source, Source::ImageChoice(_)) && p.native.is_some() {
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
/// gauge or a text follows the covers, and every key of a follower, and
/// of a followed grade of a judge an element draws, keeps its y within
/// ±[`MAX_COORD`] when moved `reach` either way.
fn check_follows(def: &SkinDef, reach: i64) -> Result<(), LoadError> {
    let max = i64::from(MAX_COORD);
    let moved_out = |t: &Track| {
        t.rects().any(|[_, y, ..]| {
            let y = i64::from(y);
            y - reach < -max || y + reach > max
        })
    };
    for (element, e) in def.elements.iter().enumerate() {
        let follow = e.place.follow;
        if follow != Follow::None {
            if matches!(e.source, Source::Lanes | Source::Judge(_)) {
                return Err(LoadError::UnsupportedFollow { element });
            }
            let overrides = match e.source {
                Source::ImageChoice(i) => def.widgets.image_choices.get(index(i)),
                _ => None,
            };
            if moved_out(&e.track)
                || overrides
                    .into_iter()
                    .flat_map(ImageChoiceDef::tracks)
                    .any(|(_, t)| moved_out(t))
            {
                return Err(LoadError::Coordinates { element });
            }
        }
        if let Source::Judge(i) = e.source
            && let Some(slot) = def.widgets.judges.get(index(i)).and_then(|j| {
                JudgeSlot::ALL
                    .into_iter()
                    .zip(&j.grades)
                    .find_map(|(s, g)| {
                        g.as_ref()
                            .filter(|g| g.follow != Follow::None && moved_out(&g.track))
                            .map(|_| s)
                    })
            })
        {
            let error = JudgeError::Coordinates {
                part: JudgePart::Grade(slot),
            };
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
        Source::Image(_) | Source::Lanes | Source::ImageChoice(_) => {}
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
            if let Some(TextValue::Literal(l)) = w.texts.get(index(i)).map(|t| &t.value)
                && !text::literal_ok(l)
            {
                let error = TextError::Literal;
                return Err(LoadError::Text { element, error });
            }
        }
        Source::Image(_) | Source::Lanes | Source::ImageChoice(_) => {}
    }
    Ok(())
}

/// The widget quads one frame of `source`'s element can push at worst:
/// 0 for an image or the lanes, which `quad_capacity` counts apart; for a
/// text, 1, or with a bitmap font one per character: a literal's
/// characters, or [`MAX_TEXT_BYTES`] for a field.
fn worst(source: Source, w: &WidgetDefs) -> u32 {
    match source {
        Source::Image(_) | Source::Lanes | Source::ImageChoice(_) => 0,
        Source::Text(i) => w.texts.get(index(i)).map_or(0, TextDef::quads),
        Source::Number(i) => w
            .numbers
            .get(index(i))
            .map_or(0, |n| u32::from(n.digits.cells)),
        Source::Gauge(i) => w.gauges.get(index(i)).map_or(0, GaugeDef::quads),
        Source::Judge(i) => w.judges.get(index(i)).map_or(0, JudgeDef::quads),
    }
}

/// The regions' boxes from the elements' keys and the lanes' rect.
fn regions(def: &SkinDef, lanes: Option<&Lanes>) -> Regions {
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
            | Source::ImageChoice(_) => {
                let b = match e.source {
                    Source::Number(i) => def
                        .widgets
                        .numbers
                        .get(index(i))
                        .and_then(|n| n.digits.bounds(&e.track)),
                    Source::Judge(i) => def.widgets.judges.get(index(i)).and_then(JudgeDef::bounds),
                    // Every present entry's track, so a change of entry
                    // moves no region.
                    Source::ImageChoice(i) => def
                        .widgets
                        .image_choices
                        .get(index(i))
                        .and_then(|c| choice_bounds(c, &e.track)),
                    // A text's box only: spilled ink does not count.
                    Source::Image(_) | Source::Gauge(_) | Source::Text(_) | Source::Lanes => {
                        e.track.bounds()
                    }
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
        .filter_map(|(_, t)| t.bounds())
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
) -> Result<(), LoadError> {
    let max = MAX_COORD;
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
                if slot_of(cycle.timer, whens).is_none() {
                    return fail(ImageChoiceError::UnknownTimer { choice });
                }
            }
            let outside = |t: &Track| {
                t.rects()
                    .any(|r| r.iter().any(|v| !(-max..=max).contains(v)))
            };
            if c.track.as_ref().is_some_and(outside) {
                return fail(ImageChoiceError::Coordinates { choice });
            }
        }
    }
    Ok(())
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
    /// table, text elements, keys across all tracks, judge parts'
    /// included); every `when` condition and visibility is a bool, judge
    /// parts' included, and every number's value an int; every timer
    /// exists, judge parts' included; every element's widget, every
    /// element's, widget's and lane part's image exists, glyph images hold
    /// whole sets, every text's font file exists, and every image's cycle
    /// timer exists; the places (regions under a fixed canvas, fills
    /// outside `screen`, key coordinates within ±[`MAX_COORD`], widget
    /// tracks that move, judges in `judge` and unbound, texts unbound,
    /// without a fill and with every key's height positive and width not
    /// negative, one lanes widget in its region, bindings beside it); the
    /// widgets' parameters (a literal's length and characters); the
    /// lanes' definition and geometry; the followers (only an image, a
    /// number, a gauge or a text, or a judge's grade, follows the covers,
    /// each key's y within ±[`MAX_COORD`] moved the lanes' length either
    /// way); the total
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
        let elements = &def.elements;
        let whens = &def.whens;
        let w = &def.widgets;
        if elements.len() > MAX_ELEMENTS {
            return Err(LoadError::TooManyElements { n: elements.len() });
        }
        if whens.len() > MAX_WHEN_TIMERS {
            return Err(LoadError::TooManyWhenTimers { n: whens.len() });
        }
        for n in [
            w.numbers.len(),
            w.gauges.len(),
            w.judges.len(),
            w.texts.len(),
            w.image_choices.len(),
        ] {
            if n > MAX_ELEMENTS {
                return Err(LoadError::TooManyWidgets { n });
            }
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
        let keys = elements
            .iter()
            .map(|e| &e.track)
            .chain(w.judges.iter().flat_map(|j| j.tracks().map(|(_, t)| t)))
            .chain(
                w.image_choices
                    .iter()
                    .flat_map(|c| c.tracks().map(|(_, t)| t)),
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
                | Source::ImageChoice(_) => {}
            }
        }
        let mut slots = Vec::with_capacity(elements.len());
        for (element, e) in elements.iter().enumerate() {
            let slot = slot_of(e.timer, whens.len()).ok_or(LoadError::UnknownTimer { element })?;
            slots.push(slot);
            if let Source::Judge(i) = e.source
                && let Some((part, _)) = w
                    .judges
                    .get(index(i))
                    .into_iter()
                    .flat_map(JudgeDef::timers)
                    .find(|&(_, t)| slot_of(t, whens.len()).is_none())
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
            Source::Number(i) => w.numbers.get(index(i)).is_none(),
            Source::Gauge(i) => w.gauges.get(index(i)).is_none(),
            Source::Judge(i) => w.judges.get(index(i)).is_none(),
            Source::Text(i) => w.texts.get(index(i)).is_none(),
            Source::ImageChoice(i) => w.image_choices.get(index(i)).is_none(),
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
            | Source::ImageChoice(_) => false,
        }) {
            return Err(LoadError::UnknownImage { element });
        }
        for (element, e) in elements.iter().enumerate() {
            let fonts = [def.sheet.font_count(), def.sheet.bitmap_font_count()];
            check_widget_images(element, e.source, w, &frames, fonts)?;
        }
        check_image_choices(w, &frames, whens.len())?;
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
                    slot: slot_of(c.timer, whens.len())
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
        check_follows(&def, lanes.map_or(0, LanesDef::reach))?;
        let mut ops = SkinOps::default();
        // Each element evaluates its own number's value and its judge's
        // part visibilities, so they are charged per referencing element.
        let per_element = elements.iter().flat_map(|e| {
            let number = match e.source {
                Source::Number(i) => w.numbers.get(index(i)).map(|n| &n.value),
                _ => None,
            };
            let judge = match e.source {
                Source::Judge(i) => w.judges.get(index(i)),
                _ => None,
            };
            let choice = match e.source {
                Source::ImageChoice(i) => w.image_choices.get(index(i)).map(|c| &c.value),
                _ => None,
            };
            number
                .into_iter()
                .chain(choice)
                .chain(judge.into_iter().flat_map(|j| j.visibles().map(|(_, v)| v)))
        });
        for program in whens
            .iter()
            .chain(elements.iter().filter_map(|e| e.visible.as_ref()))
            .chain(per_element)
        {
            ops.charge(program).map_err(LoadError::Ops)?;
        }
        let widget_quads = elements
            .iter()
            .fold(0_u64, |n, e| n + u64::from(worst(e.source, w)));
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
        let lanes = lanes.map(|l| Lanes::new(l, image));
        let regions = regions(&def, lanes.as_ref());
        let n_whens = whens.len();
        // A timer only an unreferenced judge names reads off.
        let slot = |t: TimerRef| slot_of(t, n_whens).unwrap_or(usize::MAX);
        let WidgetDefs {
            numbers,
            gauges,
            judges,
            texts,
            image_choices,
        } = def.widgets;
        let image_choices = image_choices
            .into_iter()
            .map(|d| ImageChoice {
                value: d.value,
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
        let numbers = numbers
            .into_iter()
            .map(|n| Number {
                digits: Digits::new(&n.digits, image(n.digits.image)),
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
                    })
                }),
                combos: j.combos.map(|c| {
                    c.map(|c| ComboStrip {
                        digits: Digits::new(&c.digits, image(c.digits.image)),
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
        let elements = def
            .elements
            .into_iter()
            .zip(slots)
            .map(|(e, slot)| Element {
                kind: match e.source {
                    Source::Image(i) => Kind::Image(image(i)),
                    Source::Lanes => Kind::Lanes,
                    Source::Number(i) => Kind::Number(index(i)),
                    Source::Gauge(i) => Kind::Gauge(index(i)),
                    Source::Judge(i) => Kind::Judge(index(i)),
                    Source::ImageChoice(i) => Kind::ImageChoice(index(i)),
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
            })
            .collect();
        Ok(Self {
            elements,
            whens: def.whens.into_boxed_slice(),
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
            text_slots,
            widget_quads,
            bomb_up_to: def.bomb_up_to,
        })
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

    /// Builds the sheet's atlas for `variant` ([`Sheet::atlas`]).
    ///
    /// # Errors
    ///
    /// As [`Sheet::atlas`].
    pub fn atlas(&self, variant: Variant, limits: AtlasLimits) -> Result<Atlas, AtlasError> {
        self.sheet.atlas(variant, limits)
    }

    /// The operations every frame's expressions run.
    #[must_use]
    pub fn ops(&self) -> SkinOps {
        self.ops
    }

    /// The entries of the image choice tables, holes and repeats included.
    #[must_use]
    pub fn choice_ref_count(&self) -> usize {
        self.image_choices.iter().map(|c| c.parts.len()).sum()
    }

    /// The quads a frame can push: one per image element (an image choice
    /// element included, however long its table), each widget
    /// element's worst case (a number's cells, a gauge's segments and its
    /// flicker's tip, a judge's largest slot with its combo's cells, a
    /// text's one line or its bitmap glyphs), and the lanes' budget with
    /// its reserved quads.
    #[must_use]
    pub fn quad_capacity(&self) -> u32 {
        let images = self
            .elements
            .iter()
            .filter(|e| matches!(e.kind, Kind::Image(_) | Kind::ImageChoice(_)))
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
    /// below 100 % (lane width alone never picks `Hi`). Hidden regions and
    /// elements whose visibility folds to false do not count. Exact rationals, no float. For `prepare`, not in play.
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
            let bound = matches!(e.kind, Kind::Lanes) || e.bind != Bind::None;
            let lane = if bound { Ratio(k, 100) } else { one };
            fit.mul(scale).mul(fill).mul(lane).gt(one)
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
            let h = e.track.rects().map(|[.., h]| h).max().unwrap_or(0);
            let h = u128::try_from(h).unwrap_or(0);
            let scale = self.regions.effective(e.region, layout).scale;
            let em = Ratio(h, 1).mul(fit).mul(Ratio(u128::from(scale), 100));
            // Both terms are below 2^60; rounding them to f64 is the intent.
            let em_px = f32_of(em.0 as f64 / em.1 as f64).min(MAX_TEXT_EM_PX);
            out.push(TextRequest {
                slot: t.slot,
                text: s,
                font: def.font,
                em_px,
            });
        }
        out
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
        play.set_bitmap(lines.into_boxed_slice());
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

    /// A fresh [`Play`] for one attempt of `chart`; a retry or the next
    /// practice round calls this again and starts clean.
    ///
    /// # Errors
    ///
    /// A negative lead-in, a countdown start outside the time range, or a
    /// section whose initial gauges do not match the context's gauges.
    pub fn prepare(&self, chart: &Chart, ctx: &PlayContext<'_>) -> Result<Play, PrepareError> {
        Play::new(
            chart,
            ctx,
            self.whens.len(),
            self.lanes.is_some(),
            self.bomb_up_to,
        )
    }

    /// Builds one frame: applies the drained effects, reads the snapshot,
    /// starts the timers, places the regions on the canvas of the frame's
    /// target, then appends each drawn element's quads to `out` in draw
    /// order (the caller clears `out`). Allocates nothing.
    ///
    /// `play` must come from this skin's [`prepare`](Self::prepare); one
    /// with another number of timers draws nothing. `uvs` must come from an
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
        play.check()?;
        if play.slots() != usize::from(BUILTIN) + self.whens.len()
            || uvs.sheet_id() != self.sheet_id
            || uvs.frames().len() != self.frame_count
        {
            return Ok(());
        }
        let canvas = self.canvas(frame.target);
        let placement = self.placement(&canvas, frame.layout, play.scratch());
        play.fill(frame, &self.whens)?;
        let play = &*play;
        let state = play.state();
        // A table of another length draws no text.
        let texts = play.text().len() == self.text_slots();
        let bitmaps = play.bitmap().len() == self.text_slots();
        let display = TimeUs(frame.now.0.saturating_add(frame.display_offset.0));
        // The lanes' geometry, once for every element, whether or not the
        // lanes draw.
        let lanes = self.lanes.as_ref().map(|l| l.frame_geometry(frame.lanes));
        for e in &self.elements {
            if !placement.region(e.region).visible {
                continue;
            }
            // An image choice's entry, before its track is sampled.
            let choice = match e.kind {
                Kind::ImageChoice(i) => {
                    let Some(p) = self.image_choices.get(i).and_then(|c| c.select(&state)) else {
                        continue;
                    };
                    Some(p)
                }
                _ => None,
            };
            let track = choice.and_then(|p| p.track.as_ref()).unwrap_or(&e.track);
            let Some((t, s)) = sample(play, frame.now, &state, e.slot, e.visible.as_ref(), track)
            else {
                continue;
            };
            // The covers' motion, after the track and before the lane map
            // and the region; lanes and judges never follow.
            let rect = e.follow.translate(s.rect, lanes.as_ref());
            let affine = placement.affine(e.region, e.fill);
            let brush = Brush {
                key: e.key,
                affine,
                map: placement.bind(e.bind),
                pivot: e.pivot,
                angle: radians(s.angle),
                color: s.color,
                native: choice.map_or(e.native, |p| p.native),
                scale: canvas.scale(),
            };
            let at = Frame {
                play,
                now: frame.now,
                uvs,
            };
            match e.kind {
                Kind::Image(image) => {
                    if let Some(f) = image.frame(play, frame.now, uvs) {
                        brush.push(out, rect, f, image.filter);
                    }
                }
                Kind::ImageChoice(_) => {
                    if let Some(p) = choice
                        && let Some(f) = p.image.frame(play, frame.now, uvs)
                    {
                        brush.push(out, rect, f, p.image.filter);
                    }
                }
                Kind::Number(i) => {
                    if let Some(n) = self.numbers.get(i) {
                        let v = n.value.eval(&state);
                        n.digits.draw(v, rect, &brush, &at, out);
                    }
                }
                Kind::Gauge(i) => {
                    if let Some(g) = self.gauges.get(i) {
                        let d = GaugeDraw {
                            at,
                            gauges: frame.gauges,
                            shown: frame.gauge,
                            t,
                        };
                        g.draw(&d, rect, &brush, out);
                    }
                }
                Kind::Judge(i) => {
                    if let Some(j) = self.judges.get(i) {
                        let d = JudgeDraw {
                            at,
                            state: &state,
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
                            play.bitmap().get(usize::from(t.slot)).filter(|_| bitmaps)
                        {
                            t.draw_glyphs(line, rect, &brush, uvs, out);
                        }
                    } else if let Some(image) = play.text().get(t.slot).filter(|_| texts) {
                        t.draw(image, rect, &brush, out);
                    }
                }
                Kind::Lanes => {
                    if let (Some(widget), Some(g)) = (&self.lanes, lanes.as_ref()) {
                        let d = Draw {
                            play,
                            now: frame.now,
                            display,
                            view: frame.lanes,
                            frame: g,
                            complete: frame.complete,
                            geometry: placement.geometry(),
                            affine,
                            color: s.color,
                            key: e.key,
                            uvs,
                        };
                        widget.draw(&d, out);
                    }
                }
            }
        }
        Ok(())
    }
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
        let f = b.admit_fnt("f.fnt", &mut |_| Ok(())).unwrap();
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
        })
        .unwrap();
        let chart = ChartBuilder::new(150_000).build().unwrap();
        let ctx = PlayContext {
            gauges: &[],
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: false,
            empty_poor_counted: false,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(0),
            bpm_range: None,
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
