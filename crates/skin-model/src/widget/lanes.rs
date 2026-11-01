//! The lanes widget: notes, long notes, mines, bar lines, the judgment line
//! and the hidden and lift covers on lanes a skin authors as rects, drawn
//! as the simple skin draws them (crates/render/src/simple.rs) or as the
//! source player draws an imported skin ([`LaneStyle`]).
//!
//! Its work per frame is bounded twice: [`LANE_VISITS`] candidates
//! (notes and bar lines met, drawn or skipped) and [`LANE_QUADS`] quads
//! for them. The judgment line and the two covers are reserved and drawn
//! whatever the notes spent.
//!
//! The lift raises the judgment line by its share of the lane; the lane
//! cover (SUDDEN+) then takes its share of the length left above the
//! lift, and the notes scroll over what both leave. Together they may
//! cover the whole lane, as in the source player. That coverage is
//! logical: the lanes paint no SUDDEN+, which skins paint as elements of
//! their own ([`CoverPaintDef`](crate::CoverPaintDef)).
//!
//! The hidden cover (HIDDEN+) rises from its anchor with the lift and by
//! its own share of the length left above the lift, over the notes: it
//! changes neither the scroll nor what is drawn under it. It is cut off
//! below at its trim line ([`HiddenTrim`]). The lift cover keeps its own
//! height under the raised line. Each is painted where a marker element
//! ([`LanePaint`](crate::LanePaint)) stands in the draw order.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{LANE_COUNT, Lane, Note, NoteKind, Position, TimeUs};

use super::ImageRef;
use super::scroll::Scale;
use crate::atlas::uv::{FrameUv, UvTable};
use crate::canvas::f32_of;
use crate::place::Affine;
use crate::play::reducer::is_target;
use crate::play::{Notes, Play};
use crate::quad::{Filter, Quad, QuadBatch, RunKey};
use crate::skin::MAX_COORD;

/// The quads one frame's bar lines and notes may take.
pub const LANE_QUADS: u32 = 8_192;
/// The candidates one frame may visit: each bar line and note the visible
/// range meets, whether drawn, judged, outside the section or culled.
pub const LANE_VISITS: u32 = 2 * LANE_QUADS;
/// The quads reserved beyond [`LANE_QUADS`]: the judgment line, the
/// hidden cover and the lift cover.
pub const LANE_RESERVED: u32 = 3;

/// Which side of the key lanes the scratch lane is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// The images of the lanes' parts, each a sheet image index. Per-lane
/// parts are indexed by [`Lane::index`]. A part left out is not drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneParts {
    pub note: [u32; LANE_COUNT],
    pub head: [u32; LANE_COUNT],
    pub tail: [u32; LANE_COUNT],
    /// A long note's body while its hold is shown.
    pub body_on: [u32; LANE_COUNT],
    /// A long note's body otherwise.
    pub body_off: [u32; LANE_COUNT],
    pub mine: [u32; LANE_COUNT],
    pub bar: Option<u32>,
    pub line: Option<u32>,
    pub lift: Option<u32>,
    /// The hidden cover (HIDDEN+).
    pub hidden: Option<u32>,
}

/// Where the hidden cover is cut off below: none of it is drawn under
/// its trim line or the lanes' bottom, whichever is higher. Lines are y
/// down in reference pixels, unrounded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HiddenTrim {
    /// No trim line: cut at the lanes' bottom.
    None,
    /// A fixed line at this y.
    Fixed(f32),
    /// A line at this y with the lift at 0 ‰, rising with the lift as
    /// the judgment line does.
    Lift(f32),
}

/// How the lanes place and keep their notes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LaneStyle {
    /// As the simple skin (crates/render/src/simple.rs): notes centred on
    /// their position against `line`; a tap or mine stays until judged,
    /// hit or its time passes; a long note shows both caps, its body
    /// between their centres, until its end's position passes the line.
    Simple,
    /// As the source player draws an imported skin
    /// (`play/LaneRenderer.java:288-727` at `0.8.9`), see [`SourceLanes`].
    Source(SourceLanes),
}

/// The source player's lane geometry, unrounded, in reference pixels (y
/// down).
///
/// A note's lower edge sits `offset_y` above the raised judgment line
/// `bottom − lift` at its judgment time, and every note, cap and mine is
/// `note_h` high. A tap or mine is drawn while its time has not passed,
/// judged or not (`:546-560`, `:589-593`); nothing is drawn below the
/// line. A long note is drawn while its end's time has not passed and its
/// end is ahead of its head, or of the line once the head has passed it
/// (`:552`, `:577`); its body runs from its end cap's lower edge to its
/// head cap's upper edge, and its end cap is drawn only when its release
/// is judged on its own (`:701-727`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceLanes {
    /// Where a note's lower edge sits at its judgment time with no lift
    /// and no offset: the source lane region's lower edge.
    pub bottom: f32,
    /// The source lane region's height: the lift and SUDDEN+ take their
    /// shares of it.
    pub height: f32,
    /// How far above the line a note's lower edge sits (the source's `y`
    /// offset), at least 0.
    pub offset_y: f32,
    /// Every note's height, the source's note size plus its `h` offset.
    pub note_h: f32,
}

/// A part of the lanes, for errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanePart {
    Note,
    Head,
    Tail,
    BodyOn,
    BodyOff,
    Mine,
    Bar,
    Line,
    Lift,
    Hidden,
}

/// The lanes as a skin authors them, in reference pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LanesDef {
    /// How notes are placed and kept.
    pub style: LaneStyle,
    /// `(x, w)` per [`Lane::index`], at 100 % lane width.
    pub lanes: [[i32; 2]; LANE_COUNT],
    /// The side the author drew the scratch lane on.
    pub scratch: Side,
    /// The lanes' top: notes and covers are cut to `[top, bottom]`. In the
    /// simple style, where a note appears `visible_time` before it
    /// reaches the line.
    pub top: i32,
    /// The simple style's judgment line.
    pub line: i32,
    /// The lanes' bottom: where a late note leaves them in the simple
    /// style. The source style cuts at [`SourceLanes::bottom`].
    pub bottom: i32,
    /// Where the lift cover's upper edge sits with the lift at 0 ‰; it is
    /// `lift_h` tall below it and drawn only while lifted.
    pub lift_at: f32,
    pub lift_h: f32,
    /// Where the hidden cover's upper edge sits with the lift and the
    /// hidden cover at 0 ‰; it is `hidden_h` tall below it, rises by both
    /// and is drawn only while the hidden cover is above 0 ‰.
    pub hidden_at: f32,
    pub hidden_h: f32,
    pub hidden_trim: HiddenTrim,
    pub note_h: u16,
    pub mine_h: u16,
    pub bar_h: u16,
    pub line_h: u16,
    pub parts: LaneParts,
}

/// What the lanes show this frame: the hi-speed and the covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneView {
    /// The time a note takes from the uncovered top to the line, at the
    /// chart's initial rate.
    pub visible_time: TimeUs,
    /// The lane cover, in ‰ (at most 1 000) of the length left above the
    /// lift: the lane's length less the lift's.
    pub cover: u16,
    /// The lift, in ‰ (at most 1 000) of the lane's length, not shortened
    /// by either cover: the line moves up by it. The lane's length is
    /// `line − top` in the simple style and the region's height in the
    /// source style.
    pub lift: u16,
    /// The hidden cover, in ‰ (at most 1 000) of the length left above
    /// the lift, as the lane cover; 0 draws none of it. The caller passes
    /// 0 while the player has it off.
    pub hidden: u16,
}

/// Why the lanes were refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LanesError {
    #[error("the skin defines lanes but no lanes element")]
    MissingElement,
    #[error("the skin has a lanes element but no lanes")]
    MissingDef,
    #[error("lane {lane}'s width is below 1")]
    LaneWidth { lane: usize },
    #[error("the lanes leave ±{MAX_COORD}")]
    Span,
    #[error("the lanes need top < line ≤ bottom within ±{MAX_COORD} and heights of at least 1")]
    Heights,
    #[error("the covers' extents leave ±{MAX_COORD}, or a drawn cover has no height")]
    Anchors,
    #[error("a key lane is not on the scratch lane's side")]
    ScratchNotAtSide,
    #[error(
        "the source geometry needs a positive height and note height, an offset of at least 0, \
         and a note at its judgment time within the lanes"
    )]
    Source,
    #[error("a {part:?} image does not exist")]
    UnknownPart { part: LanePart },
}

impl LanesDef {
    /// Every part's image, with its part.
    pub(crate) fn images(&self) -> impl Iterator<Item = (LanePart, u32)> + '_ {
        let p = &self.parts;
        let lanes = [
            (LanePart::Note, &p.note),
            (LanePart::Head, &p.head),
            (LanePart::Tail, &p.tail),
            (LanePart::BodyOn, &p.body_on),
            (LanePart::BodyOff, &p.body_off),
            (LanePart::Mine, &p.mine),
        ];
        let single = [
            (LanePart::Bar, p.bar),
            (LanePart::Line, p.line),
            (LanePart::Lift, p.lift),
            (LanePart::Hidden, p.hidden),
        ];
        lanes
            .into_iter()
            .flat_map(|(part, images)| images.iter().map(move |&i| (part, i)))
            .chain(
                single
                    .into_iter()
                    .filter_map(|(part, i)| i.map(|i| (part, i))),
            )
    }

    /// The lane's length `H`, rounded up to a whole reference pixel: the
    /// farthest a follower moves either way. For checked lanes it is at
    /// most 2^22.
    pub(crate) fn reach(&self) -> i64 {
        match self.style {
            LaneStyle::Simple => i64::from(self.line) - i64::from(self.top),
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a checked source height is positive and at most 2^21"
            )]
            LaneStyle::Source(s) => f64::from(s.height).ceil() as i64,
        }
    }

    /// Checks the geometry: every width at least 1, every lane within
    /// ±[`MAX_COORD`], `top < line ≤ bottom` within it, heights of at
    /// least 1, the covers' extents and the hidden cover's trim line within
    /// it with a height for each cover drawn, every key lane on the scratch lane's side, and for the source
    /// style its geometry ([`LanesError::Source`]).
    pub(crate) fn check(&self) -> Result<(), LanesError> {
        let max = i64::from(MAX_COORD);
        let within = |v: i64| (-max..=max).contains(&v);
        for (lane, &[x, w]) in self.lanes.iter().enumerate() {
            if w < 1 {
                return Err(LanesError::LaneWidth { lane });
            }
            let (x, w) = (i64::from(x), i64::from(w));
            if !(within(x) && within(w) && within(x + w)) {
                return Err(LanesError::Span);
            }
        }
        let [top, line, bottom] = [self.top, self.line, self.bottom].map(i64::from);
        if !(within(top) && within(bottom) && top < line && line <= bottom)
            || [self.note_h, self.mine_h, self.bar_h, self.line_h].contains(&0)
        {
            return Err(LanesError::Heights);
        }
        // The covers' extents within ±MAX_COORD, and a height for each
        // cover drawn.
        #[expect(clippy::cast_precision_loss, reason = "within ±2^21, exact in f64")]
        let near = |v: f64| v.abs() <= max as f64;
        let [lift_at, lift_h, hidden_at, hidden_h] =
            [self.lift_at, self.lift_h, self.hidden_at, self.hidden_h].map(f64::from);
        let sized = |h: f64, drawn: bool| if drawn { h > 0.0 } else { h >= 0.0 };
        let trim = match self.hidden_trim {
            HiddenTrim::None => true,
            HiddenTrim::Fixed(y) | HiddenTrim::Lift(y) => near(f64::from(y)),
        };
        if !(near(lift_at)
            && near(lift_at + lift_h)
            && near(hidden_at)
            && near(hidden_at + hidden_h)
            && trim
            && sized(lift_h, self.parts.lift.is_some())
            && sized(hidden_h, self.parts.hidden.is_some()))
        {
            return Err(LanesError::Anchors);
        }
        let [scratch, keys @ ..] = self
            .lanes
            .map(|[x, w]| [i64::from(x), i64::from(x) + i64::from(w)]);
        let beside = keys.iter().all(|&[x0, x1]| match self.scratch {
            Side::Left => x0 >= scratch[1],
            Side::Right => x1 <= scratch[0],
        });
        if !beside {
            return Err(LanesError::ScratchNotAtSide);
        }
        if let LaneStyle::Source(s) = self.style {
            let [y, h, o, n] = [s.bottom, s.height, s.offset_y, s.note_h].map(f64::from);
            // The note at its judgment time, with no lift, lies within
            // `[top, bottom]` (`bottom` is the source's).
            #[expect(clippy::cast_precision_loss, reason = "within ±2^21, exact in f64")]
            let ok = y.abs() <= max as f64
                && h > 0.0
                && h <= 2.0 * max as f64
                && n > 0.0
                && o >= 0.0
                && y - o - n >= top as f64;
            if !ok {
                return Err(LanesError::Source);
            }
        }
        Ok(())
    }
}

/// The lanes' current geometry: `(x, w)` per lane and of the span, before
/// the region's transform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Geometry {
    pub(crate) lanes: [[f64; 2]; LANE_COUNT],
    pub(crate) span: [f64; 2],
}

impl Geometry {
    /// The geometry of a skin without lanes: unit widths, so a bind map
    /// never divides by zero.
    pub(crate) const NONE: Self = Self {
        lanes: [[0.0, 1.0]; LANE_COUNT],
        span: [0.0, 1.0],
    };
}

/// The parts as a build reads them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Parts {
    pub(crate) note: [ImageRef; LANE_COUNT],
    pub(crate) head: [ImageRef; LANE_COUNT],
    pub(crate) tail: [ImageRef; LANE_COUNT],
    pub(crate) body_on: [ImageRef; LANE_COUNT],
    pub(crate) body_off: [ImageRef; LANE_COUNT],
    pub(crate) mine: [ImageRef; LANE_COUNT],
    pub(crate) bar: Option<ImageRef>,
    pub(crate) line: Option<ImageRef>,
    pub(crate) lift: Option<ImageRef>,
    pub(crate) hidden: Option<ImageRef>,
}

/// Loaded lanes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lanes {
    /// `(x, w)` per lane, as authored.
    lanes: [[i64; 2]; LANE_COUNT],
    /// The span's left and right edges, and the key block's.
    span: [i64; 2],
    keys: [i64; 2],
    scratch: Side,
    style: LaneStyle,
    top: i32,
    line: i32,
    bottom: i32,
    lift_at: f32,
    lift_h: f32,
    hidden_at: f32,
    hidden_h: f32,
    hidden_trim: HiddenTrim,
    note_h: f32,
    mine_h: f32,
    bar_h: f32,
    line_h: f32,
    parts: Parts,
}

impl Lanes {
    /// Lanes from a checked definition, with its parts resolved.
    pub(crate) fn new(def: &LanesDef, image: impl Fn(u32) -> ImageRef) -> Self {
        let lanes = def.lanes.map(|[x, w]| [i64::from(x), i64::from(w)]);
        let edges = |l: &[[i64; 2]]| {
            l.iter().fold([i64::MAX, i64::MIN], |[a, b], &[x, w]| {
                [a.min(x), b.max(x + w)]
            })
        };
        let keys = lanes.get(1..).map_or([0, 0], edges);
        let p = &def.parts;
        Self {
            lanes,
            span: edges(&lanes),
            keys,
            scratch: def.scratch,
            style: def.style,
            top: def.top,
            line: def.line,
            bottom: def.bottom,
            lift_at: def.lift_at,
            lift_h: def.lift_h,
            hidden_at: def.hidden_at,
            hidden_h: def.hidden_h,
            hidden_trim: def.hidden_trim,
            note_h: f32::from(def.note_h),
            mine_h: f32::from(def.mine_h),
            bar_h: f32::from(def.bar_h),
            line_h: f32::from(def.line_h),
            parts: Parts {
                note: p.note.map(&image),
                head: p.head.map(&image),
                tail: p.tail.map(&image),
                body_on: p.body_on.map(&image),
                body_off: p.body_off.map(&image),
                mine: p.mine.map(&image),
                bar: p.bar.map(&image),
                line: p.line.map(&image),
                lift: p.lift.map(&image),
                hidden: p.hidden.map(&image),
            },
        }
    }

    /// The widget's own rect: the span from the top to the bottom, as
    /// `[x0, y0, x1, y1]`.
    pub(crate) fn rect(&self) -> [i64; 4] {
        let [s0, s1] = self.span;
        [s0, i64::from(self.top), s1, i64::from(self.bottom)]
    }

    /// The authored geometry.
    #[expect(clippy::cast_precision_loss, reason = "within ±2^21, exact in f64")]
    pub(crate) fn authored(&self) -> Geometry {
        let [s0, s1] = self.span;
        Geometry {
            lanes: self.lanes.map(|[x, w]| [x as f64, w as f64]),
            span: [s0 as f64, (s1 - s0) as f64],
        }
    }

    /// The geometry with the scratch lane on `scratch` and every lane
    /// scaled to `pct` % about the span's point at `fx_halves` halves of
    /// its width.
    ///
    /// On the other side, the scratch lane is mirrored in the span and the
    /// key block moves whole to the other end, keeping each key's offset
    /// in it. Coordinates are within ±2^20 and `pct` at most 200, so the
    /// integer steps are exact and `f64` rounds only the division by 100.
    pub(crate) fn geometry(&self, scratch: Side, pct: u16, fx_halves: u8) -> Geometry {
        let [s0, s1] = self.span;
        let [k0, k1] = self.keys;
        let mirrored = scratch != self.scratch;
        let mut lanes = self.lanes;
        if mirrored {
            for (i, lane) in lanes.iter_mut().enumerate() {
                let [x, w] = *lane;
                lane[0] = if i == 0 {
                    s0 + s1 - x - w
                } else {
                    s0 + s1 - k1 + (x - k0)
                };
            }
        }
        // Twice the scaling point, an integer.
        let q2 = 2 * s0 + i64::from(fx_halves) * (s1 - s0);
        let k = f64::from(pct);
        // `x' = Q + (x − Q)·k/100`, as `(2x − 2Q)·k / 200 + Q`.
        #[expect(clippy::cast_precision_loss, reason = "within ±2^30, exact in f64")]
        let map = |x: i64| ((2 * x - q2) as f64 * k / 200.0) + q2 as f64 / 2.0;
        #[expect(clippy::cast_precision_loss, reason = "within ±2^21, exact in f64")]
        let width = |w: i64| w as f64 * k / 100.0;
        let lanes = lanes.map(|[x, w]| [map(x), width(w)]);
        let (a, b) = (map(s0), map(s1));
        Geometry {
            lanes,
            span: [a, b - a],
        }
    }
}

/// What the lanes draw from this frame.
pub(crate) struct Draw<'a> {
    pub(crate) play: &'a Play,
    /// The frame's clock, which image cycles age on.
    pub(crate) now: TimeUs,
    /// The display clock, which positions follow.
    pub(crate) display: TimeUs,
    pub(crate) view: LaneView,
    /// The frame's geometry, computed once for every element.
    pub(crate) frame: &'a LaneFrameGeometry,
    /// Whether evaluation is complete: a section attempt rings out
    /// without notes.
    pub(crate) complete: bool,
    pub(crate) geometry: &'a Geometry,
    pub(crate) affine: Affine,
    pub(crate) color: [f32; 4],
    pub(crate) key: RunKey,
    pub(crate) uvs: &'a UvTable,
}

/// A part as one frame draws it: its current frame (`None` draws nothing
/// but still spends its quad) and its filter.
#[derive(Clone, Copy)]
struct Shown {
    frame: Option<FrameUv>,
    filter: Filter,
}

impl Draw<'_> {
    /// `part`'s frame this frame.
    fn shown(&self, part: &ImageRef) -> Shown {
        Shown {
            frame: part.frame(self.play, self.now, self.uvs),
            filter: part.filter,
        }
    }
}

/// The quads drawn so far and the budgets they draw on.
struct Pen<'a, 'b> {
    d: &'b Draw<'a>,
    out: &'b mut QuadBatch,
    top: f32,
    bottom: f32,
    visits: u32,
    quads: u32,
    /// Whether a budget ran out or the batch filled: nothing more is
    /// visited.
    stopped: bool,
}

/// A part's rect, cut to `[top, bottom]`, and the share of its height cut
/// from each end.
struct Cut {
    rect: [f32; 4],
    /// The kept part as fractions of the uncut height, when cut.
    from: Option<f32>,
    to: Option<f32>,
}

/// `[y0, y0 + h)` cut to `[top, bottom]`, with `SimpleSkin`'s `clip`.
fn cut(x: f32, w: f32, y0: f32, y1: f32, top: f32, bottom: f32) -> Option<Cut> {
    let (c0, c1) = (y0.max(top), y1.min(bottom));
    if c1 <= c0 {
        return None;
    }
    let h = y1 - y0;
    Some(Cut {
        rect: [x, c0, w, c1 - c0],
        from: (y0 < top).then(|| (c0 - y0) / h),
        to: (y1 > bottom).then(|| (c1 - y0) / h),
    })
}

impl<'a, 'b> Pen<'a, 'b> {
    /// A pen over the frame's lanes, cut to their top and bottom
    /// (coordinates within ±2^20 are exact in `f32`), nothing spent.
    fn new(d: &'b Draw<'a>, out: &'b mut QuadBatch) -> Self {
        Self {
            d,
            out,
            top: f32_of(d.frame.top),
            bottom: f32_of(d.frame.bottom),
            visits: 0,
            quads: 0,
            stopped: false,
        }
    }

    /// Charges one candidate; `false` once the visits are spent.
    fn visit(&mut self) -> bool {
        if self.stopped {
            return false;
        }
        if self.visits == LANE_VISITS {
            self.stop();
            return false;
        }
        self.visits += 1;
        true
    }

    /// Stops the frame's lanes with one drop.
    fn stop(&mut self) {
        self.stopped = true;
        self.out.count_drop();
    }

    /// Pushes a bar line or note quad; `false` once the quads are spent or
    /// the batch is full.
    fn counted(&mut self, part: Shown, cut: &Cut) -> bool {
        if self.stopped {
            return false;
        }
        if self.quads == LANE_QUADS {
            self.stop();
            return false;
        }
        self.quads += 1;
        if !self.push(part, cut) {
            self.stopped = true;
            return false;
        }
        true
    }

    /// A cap of height `h` centred at `y`, cut to the lanes.
    fn cap(&mut self, part: Shown, x: f32, w: f32, y: f32, h: f32) -> bool {
        self.span(part, x, w, y - h / 2.0, y + h / 2.0)
    }

    /// `part` over `[y0, y1)`, cut to the lanes.
    fn span(&mut self, part: Shown, x: f32, w: f32, y0: f32, y1: f32) -> bool {
        match cut(x, w, y0, y1, self.top, self.bottom) {
            Some(c) => self.counted(part, &c),
            None => true,
        }
    }

    /// Pushes `part` over the cut rect, its uv cut alike, transformed;
    /// `false` when the batch refused it.
    fn push(&mut self, part: Shown, cut: &Cut) -> bool {
        let d = self.d;
        let Some(f) = part.frame else {
            return true;
        };
        let [u0, v0, u1, v1] = f.uv;
        let along = |t: f32| v0 + (v1 - v0) * t;
        let uv = [u0, cut.from.map_or(v0, along), u1, cut.to.map_or(v1, along)];
        let rect = d.affine.rect(cut.rect);
        self.out.push(
            d.key,
            Quad {
                rect,
                uv,
                color: d.color,
                pivot: [rect[0], rect[1]],
                angle: 0.0,
                layer: f.layer,
                filter: part.filter,
            },
        )
    }
}

/// The lanes' logical geometry for one frame, in reference pixels, y
/// down: computed once per build from the frame's [`LaneView`], before
/// any element draws, and read by the lanes and by every element that
/// follows the covers ([`Follow`](crate::Follow)). It depends on the
/// amounts only, never on which parts exist or are drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LaneFrameGeometry {
    /// How far the lift raises the line.
    pub(crate) lift_px: f64,
    /// SUDDEN+'s share: how far what follows it moves down.
    pub(crate) cover_px: f64,
    /// How far the hidden cover rises by its own share, above the lift.
    pub(crate) hidden_px: f64,
    /// The raised judgment line.
    pub(crate) line_y: f64,
    /// The scroll's length: from the uncovered top to the line.
    pub(crate) span: f64,
    /// The lanes' top and bottom, where notes and covers are cut.
    pub(crate) top: f64,
    pub(crate) bottom: f64,
    /// The hidden cover's trim line this frame; `None` cuts it at the
    /// bottom only.
    pub(crate) hidden_trim: Option<f64>,
}

impl Lanes {
    /// Draws the lanes into `out`: the bar lines, the judgment line and
    /// every lane's notes. Allocates nothing.
    pub(crate) fn draw(&self, d: &Draw<'_>, out: &mut QuadBatch) {
        let Some(notes) = d.play.notes() else {
            return;
        };
        let g = d.frame;
        let scale = Scale::new(
            d.play.scroll(),
            d.display,
            d.view.visible_time,
            g.span,
            g.line_y,
        );
        let mut pen = Pen::new(d, out);
        let ringing_out = d.complete && notes.section.is_some();
        match self.style {
            LaneStyle::Simple => self.simple(&mut pen, notes, &scale, g.span, ringing_out),
            LaneStyle::Source(s) => self.source(&mut pen, notes, &scale, s, g.span, ringing_out),
        }
    }

    /// Draws the hidden cover into `out`: it keeps its own height, rises by
    /// the lift and its share, and is cut to the lanes and off below at
    /// its trim line; it is on while its amount is, even when a full lift
    /// leaves it no share (`play/LaneRenderer.java:335-345`,
    /// `play/SkinHidden.java:65-90`). Allocates nothing.
    pub(crate) fn draw_hidden(&self, d: &Draw<'_>, out: &mut QuadBatch) {
        let Some(part) = self.parts.hidden.map(|p| d.shown(&p)) else {
            return;
        };
        if d.view.hidden == 0 {
            return;
        }
        let g = d.frame;
        let mut pen = Pen::new(d, out);
        let [sx, sw] = d.geometry.span.map(f32_of);
        let upper = f32_of(f64::from(self.hidden_at) - g.lift_px - g.hidden_px);
        let trim = g
            .hidden_trim
            .map_or(pen.bottom, |y| f32_of(y).min(pen.bottom));
        if let Some(c) = cut(sx, sw, upper, upper + self.hidden_h, pen.top, trim) {
            pen.push(part, &c);
        }
    }

    /// Draws the lift cover into `out`, while lifted: it keeps its own
    /// height, its upper edge rising with the line, cut to the lanes.
    /// Allocates nothing.
    pub(crate) fn draw_lift(&self, d: &Draw<'_>, out: &mut QuadBatch) {
        let g = d.frame;
        let Some(part) = self.parts.lift.map(|p| d.shown(&p)) else {
            return;
        };
        if g.lift_px <= 0.0 {
            return;
        }
        let mut pen = Pen::new(d, out);
        let [sx, sw] = d.geometry.span.map(f32_of);
        let upper = f32_of(f64::from(self.lift_at) - g.lift_px);
        if let Some(c) = cut(sx, sw, upper, upper + self.lift_h, pen.top, pen.bottom) {
            pen.push(part, &c);
        }
    }

    /// The judgment line, centred on the raised line `line`.
    fn line(&self, pen: &mut Pen<'_, '_>, line: f32) {
        let d = pen.d;
        if let Some(part) = self.parts.line.map(|p| d.shown(&p)) {
            let [sx, sw] = d.geometry.span.map(f32_of);
            pen.push(
                part,
                &Cut {
                    rect: [sx, line - self.line_h / 2.0, sw, self.line_h],
                    from: None,
                    to: None,
                },
            );
        }
    }

    /// The bar lines at positions `[from, to]`, centred `shift` below
    /// their position's y.
    fn bars(
        &self,
        pen: &mut Pen<'_, '_>,
        notes: &Notes,
        scale: &Scale,
        [from, to]: [Position; 2],
        shift: f32,
    ) {
        let d = pen.d;
        let [sx, sw] = d.geometry.span.map(f32_of);
        if let Some(bar) = self.parts.bar.map(|p| d.shown(&p)) {
            let bars = &notes.bars;
            let start = bars.partition_point(|&p| p < from);
            for &p in bars.iter().skip(start).take_while(|&&p| p <= to) {
                if !(pen.visit() && pen.cap(bar, sx, sw, scale.y(p) + shift, self.bar_h)) {
                    break;
                }
            }
        }
    }

    /// The frame's geometry for `view`, each amount at most 1 000 ‰: the
    /// lift takes its share of the lane, the cover its share of what the
    /// lift leaves, and the notes scroll over what both leave, which may
    /// be nothing (`play/LaneRenderer.java:198-213, 322-333`); the hidden
    /// cover takes its share of what the lift leaves, over the notes
    /// (`:335-345`). The source style's lane is its region's height, the
    /// simple style's the length from the top to the line. Edges keep
    /// their fractions; only drawing converts them to `f32`.
    pub(crate) fn frame_geometry(&self, view: LaneView) -> LaneFrameGeometry {
        let (lift, cover, hidden) = (
            view.lift.min(1_000),
            view.cover.min(1_000),
            view.hidden.min(1_000),
        );
        let (len, line, bottom) = match self.style {
            LaneStyle::Simple => (
                f64::from(self.line - self.top),
                f64::from(self.line),
                f64::from(self.bottom),
            ),
            LaneStyle::Source(s) => (
                f64::from(s.height),
                f64::from(s.bottom),
                f64::from(s.bottom),
            ),
        };
        let left = len * f64::from(1_000 - lift) / 1_000.0;
        let span = left * f64::from(1_000 - cover) / 1_000.0;
        let lift_px = len - left;
        LaneFrameGeometry {
            lift_px,
            cover_px: left - span,
            hidden_px: left * f64::from(hidden) / 1_000.0,
            line_y: line - lift_px,
            span,
            top: f64::from(self.top),
            bottom,
            hidden_trim: match self.hidden_trim {
                HiddenTrim::None => None,
                HiddenTrim::Fixed(y) => Some(f64::from(y)),
                HiddenTrim::Lift(y) => Some(f64::from(y) - lift_px),
            },
        }
    }

    /// The simple skin's bar lines, judgment line and notes; with no
    /// scroll left (`span` 0) nothing is scaled and only the line is
    /// drawn, the covers hiding the whole lane.
    fn simple(
        &self,
        pen: &mut Pen<'_, '_>,
        notes: &Notes,
        scale: &Scale,
        span: f64,
        ringing_out: bool,
    ) {
        let line = f32_of(scale.line_y);
        if span <= 0.0 {
            self.line(pen, line);
            return;
        }
        let pad = self.note_h.max(self.mine_h).max(self.bar_h) / 2.0;
        let (from, to) = scale.positions(
            f32_of(f64::from(self.bottom) - scale.line_y) + pad,
            f32_of(scale.line_y - f64::from(self.top)) + pad,
        );
        self.bars(pen, notes, scale, [from, to], 0.0);
        self.line(pen, line);
        if !ringing_out {
            for lane in Lane::all() {
                if !self.simple_lane(pen, notes, scale, lane, [from, to]) {
                    break;
                }
            }
        }
    }

    /// One lane's notes meeting `[from, to]`, as the simple skin draws
    /// them; `false` once the frame's lanes stop.
    fn simple_lane(
        &self,
        pen: &mut Pen<'_, '_>,
        notes: &Notes,
        scale: &Scale,
        lane: Lane,
        [from, to]: [Position; 2],
    ) -> bool {
        let d = pen.d;
        let Some(l) = LaneOf::new(self, d, notes, lane) else {
            return true;
        };
        let (x, w, line) = (l.x, l.w, f32_of(scale.line_y));
        // Positions never decrease along a lane (`Chart::validate`), so the
        // notes meeting [from, to] are one run.
        let start = l.list.partition_point(|n| n.end_pos() < from).max(l.lo);
        for (i, n) in l.list.iter().enumerate().take(l.hi).skip(start) {
            if n.pos > to {
                break;
            }
            if !pen.visit() {
                return false;
            }
            if d.play.done(l.index, i) {
                continue;
            }
            let head = scale.y(n.pos);
            let drawn = match n.kind {
                // A mine leaves at its time, hit or not.
                NoteKind::Mine { .. } if n.time < d.display => true,
                NoteKind::Mine { .. } => pen.cap(l.mine, x, w, head, self.mine_h),
                _ if !is_target(notes.section, n.time, n.end()) => true,
                NoteKind::Tap => pen.cap(l.note, x, w, head, self.note_h),
                NoteKind::Long { end_pos, .. } => {
                    // Judged or not, a long note is drawn until its end
                    // passes the line, and once its head's time has passed
                    // the head stays on the line.
                    if end_pos < scale.at_line {
                        continue;
                    }
                    let tail = scale.y(end_pos);
                    let head = head.min(line);
                    let body = if d.play.body_active(lane, i) {
                        l.on
                    } else {
                        l.off
                    };
                    pen.span(body, x, w, tail, head)
                        && pen.cap(l.tail, x, w, tail, self.note_h)
                        && pen.cap(l.head, x, w, head, self.note_h)
                }
            };
            if !drawn {
                return false;
            }
        }
        true
    }

    /// The source player's bar lines, judgment line and notes
    /// ([`SourceLanes`]). With no scroll left (`span` 0) nothing is
    /// scaled: every tap and mine whose time has not passed is drawn on
    /// the line, as the source then draws every note there, and no long
    /// note or bar line is drawn.
    fn source(
        &self,
        pen: &mut Pen<'_, '_>,
        notes: &Notes,
        scale: &Scale,
        s: SourceLanes,
        span: f64,
        ringing_out: bool,
    ) {
        let range = (span > 0.0).then(|| {
            // A note's lower edge sits `offset_y` above its position's y.
            let pad = s.note_h.max(self.bar_h);
            let (from, to) = scale.positions(
                f32_of(f64::from(s.bottom) - scale.line_y) + s.offset_y + pad,
                f32_of(scale.line_y - f64::from(self.top)) + pad,
            );
            [from, to]
        });
        if let Some(range) = range {
            // At the notes' centres.
            self.bars(pen, notes, scale, range, -s.offset_y - s.note_h / 2.0);
        }
        self.line(pen, f32_of(scale.line_y));
        if !ringing_out {
            for lane in Lane::all() {
                if !self.source_lane(pen, notes, scale, lane, s, range) {
                    break;
                }
            }
        }
    }

    /// One lane's notes as the source player draws them, from those
    /// meeting `range`, or from the display time on without one; `false`
    /// once the frame's lanes stop.
    fn source_lane(
        &self,
        pen: &mut Pen<'_, '_>,
        notes: &Notes,
        scale: &Scale,
        lane: Lane,
        s: SourceLanes,
        range: Option<[Position; 2]>,
    ) -> bool {
        let d = pen.d;
        let Some(l) = LaneOf::new(self, d, notes, lane) else {
            return true;
        };
        let (x, w, oy, nh) = (l.x, l.w, s.offset_y, s.note_h);
        let line = f32_of(scale.line_y);
        let tail_shown = d.play.separate_end_judgment();
        // Times and positions never decrease along a lane
        // (`Chart::validate`).
        let start = match range {
            Some([from, _]) => l.list.partition_point(|n| n.end_pos() < from),
            None => l.list.partition_point(|n| n.time < d.display),
        };
        for (i, n) in l.list.iter().enumerate().take(l.hi).skip(start.max(l.lo)) {
            if range.is_some_and(|[_, to]| n.pos > to) {
                break;
            }
            if !pen.visit() {
                return false;
            }
            if !is_target(notes.section, n.time, n.end()) {
                continue;
            }
            let drawn = match n.kind {
                // Judged or not, until its time passes.
                NoteKind::Tap | NoteKind::Mine { .. } => {
                    if n.time < d.display {
                        continue;
                    }
                    let y = if range.is_some() {
                        scale.y(n.pos)
                    } else {
                        line
                    };
                    let part = if n.kind == NoteKind::Tap {
                        l.note
                    } else {
                        l.mine
                    };
                    pen.span(part, x, w, y - oy - nh, y - oy)
                }
                NoteKind::Long { end, end_pos, .. } => {
                    // Until its end's time passes and while its end is ahead
                    // of its head, which stays on the line once passed.
                    let head_pos = n.pos.max(scale.at_line);
                    if range.is_none() || d.display >= end || end_pos <= head_pos {
                        continue;
                    }
                    let (head, tail) = (scale.y(head_pos) - oy, scale.y(end_pos) - oy);
                    let body = if d.play.body_active(lane, i) {
                        l.on
                    } else {
                        l.off
                    };
                    pen.span(body, x, w, tail, head - nh)
                        && (!tail_shown || pen.span(l.tail, x, w, tail - nh, tail))
                        && pen.span(l.head, x, w, head - nh, head)
                }
            };
            if !drawn {
                return false;
            }
        }
        true
    }
}

/// One lane's notes, geometry and parts for a frame's pass.
struct LaneOf<'n> {
    index: usize,
    list: &'n [Note],
    lo: usize,
    hi: usize,
    x: f32,
    w: f32,
    note: Shown,
    head: Shown,
    tail: Shown,
    on: Shown,
    off: Shown,
    mine: Shown,
}

impl<'n> LaneOf<'n> {
    /// `lane`'s, with each part's frame read once for the pass.
    fn new(lanes: &Lanes, d: &Draw<'_>, notes: &'n Notes, lane: Lane) -> Option<Self> {
        let l = lane.index();
        let (Some(list), Some(&(lo, hi)), Some(&[x, w])) = (
            notes.lanes.get(l),
            notes.range.get(l),
            d.geometry.lanes.get(l),
        ) else {
            return None;
        };
        let p = &lanes.parts;
        let (Some(note), Some(head), Some(tail), Some(on), Some(off), Some(mine)) = (
            p.note.get(l),
            p.head.get(l),
            p.tail.get(l),
            p.body_on.get(l),
            p.body_off.get(l),
            p.mine.get(l),
        ) else {
            return None;
        };
        Some(Self {
            index: l,
            list,
            lo,
            hi,
            x: f32_of(x),
            w: f32_of(w),
            note: d.shown(note),
            head: d.shown(head),
            tail: d.shown(tail),
            on: d.shown(on),
            off: d.shown(off),
            mine: d.shown(mine),
        })
    }
}
