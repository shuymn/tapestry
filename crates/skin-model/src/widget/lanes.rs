//! The lanes widget: notes, long notes, mines, bar lines, the judgment line
//! and the lane and lift covers, drawn as the simple skin draws them
//! (crates/render/src/simple.rs) on lanes a skin authors as rects.
//!
//! Its work per frame is bounded twice: [`LANE_VISITS`] candidates
//! (notes and bar lines met, drawn or skipped) and [`LANE_QUADS`] quads
//! for them. The judgment line and the covers are reserved and always
//! drawn: a dropped cover would show notes it should hide.
//!
//! The lift raises the judgment line by its share of the lane; the cover
//! then takes its share of the length left above the lift. Both are
//! clamped on the edges they paint, so the painted cover always leaves a
//! tenth of the lane uncovered above the judgment line.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{LANE_COUNT, Lane, NoteKind, Position, TimeUs};

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
/// The quads reserved beyond [`LANE_QUADS`]: the judgment line, the lane
/// cover and the lift cover.
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
    pub cover: Option<u32>,
    pub lift: Option<u32>,
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
    Cover,
    Lift,
}

/// The lanes as a skin authors them, in reference pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanesDef {
    /// `(x, w)` per [`Lane::index`], at 100 % lane width.
    pub lanes: [[i32; 2]; LANE_COUNT],
    /// The side the author drew the scratch lane on.
    pub scratch: Side,
    /// Where a note appears `visible_time` before it reaches the line.
    pub top: i32,
    /// The judgment line.
    pub line: i32,
    /// Where a late note leaves the lanes.
    pub bottom: i32,
    /// Where the cover's lower edge sits with the cover at 0 ‰, and the
    /// top the cover is cut to: from `top` down, leaving at least a tenth
    /// of the lane above the line.
    pub cover_at: i32,
    /// Where the lift cover's upper edge sits with the lift at 0 ‰: from
    /// `line` to `bottom`.
    pub lift_at: i32,
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
    /// The lane cover, in ‰ of the length left above the lift: the
    /// original lane length (`line − top`) less the lift's.
    pub cover: u16,
    /// The lift, in ‰ of the original lane length (`line − top`, not
    /// shortened by either cover): the line moves up by it.
    pub lift: u16,
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
    #[error(
        "the covers need top ≤ cover_at, a tenth of the lane below cover_at, and line ≤ lift_at ≤ bottom"
    )]
    Anchors,
    #[error("a key lane is not on the scratch lane's side")]
    ScratchNotAtSide,
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
            (LanePart::Cover, p.cover),
            (LanePart::Lift, p.lift),
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

    /// Checks the geometry: every width at least 1, every lane within
    /// ±[`MAX_COORD`], `top < line ≤ bottom` within it, heights of at
    /// least 1, the cover anchors (`top ≤ cover_at`, at least a tenth of
    /// the lane from `cover_at` to the line, `line ≤ lift_at ≤ bottom`),
    /// and every key lane on the scratch lane's side.
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
        let [cover_at, lift_at] = [self.cover_at, self.lift_at].map(i64::from);
        if !(top <= cover_at && 10 * (line - cover_at) >= line - top && line <= lift_at)
            || lift_at > bottom
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
    pub(crate) cover: Option<ImageRef>,
    pub(crate) lift: Option<ImageRef>,
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
    top: i32,
    line: i32,
    bottom: i32,
    cover_at: i32,
    lift_at: i32,
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
            top: def.top,
            line: def.line,
            bottom: def.bottom,
            cover_at: def.cover_at,
            lift_at: def.lift_at,
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
                cover: p.cover.map(&image),
                lift: p.lift.map(&image),
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

/// The covers' geometry for one frame, in reference pixels.
struct Covers {
    /// The lane's length, from the top to the unlifted line.
    len: f64,
    /// How far the lift raises the line, and the raised line.
    lift_px: f64,
    line_y: f64,
    /// How far the cover's lower edge moves down from its anchor.
    cover_px: f64,
    /// The top of the scroll: where notes appear.
    edge: f64,
}

/// What the lanes draw from this frame.
pub(crate) struct Draw<'a> {
    pub(crate) play: &'a Play,
    /// The frame's clock, which image cycles age on.
    pub(crate) now: TimeUs,
    /// The display clock, which positions follow.
    pub(crate) display: TimeUs,
    pub(crate) view: LaneView,
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

impl Pen<'_, '_> {
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
        match cut(x, w, y - h / 2.0, y + h / 2.0, self.top, self.bottom) {
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

impl Lanes {
    /// Draws the lanes into `out`: the bar lines, the judgment line, every
    /// lane's notes, the cover, then the lift cover. Allocates nothing.
    pub(crate) fn draw(&self, d: &Draw<'_>, out: &mut QuadBatch) {
        let Some(notes) = d.play.notes() else {
            return;
        };
        let g = self.covers(d.view);
        let (line_y, span_px) = (g.line_y, g.line_y - g.edge);
        let scale = Scale::new(
            d.play.scroll(),
            d.display,
            d.view.visible_time,
            span_px,
            line_y,
        );
        // Coordinates within ±2^20 are exact in `f32`.
        #[expect(clippy::cast_precision_loss, reason = "within ±2^20")]
        let (top, bottom) = (self.top as f32, self.bottom as f32);
        let line = f32_of(line_y);
        let pad = self.note_h.max(self.mine_h).max(self.bar_h) / 2.0;
        let (from, to) = scale.positions(
            f32_of(f64::from(self.bottom) - line_y) + pad,
            f32_of(line_y - f64::from(self.top)) + pad,
        );
        let [sx, sw] = d.geometry.span.map(f32_of);
        let mut pen = Pen {
            d,
            out,
            top,
            bottom,
            visits: 0,
            quads: 0,
            stopped: false,
        };

        if let Some(bar) = self.parts.bar.map(|p| d.shown(&p)) {
            let bars = &notes.bars;
            let start = bars.partition_point(|&p| p < from);
            for &p in bars.iter().skip(start).take_while(|&&p| p <= to) {
                if !(pen.visit() && pen.cap(bar, sx, sw, scale.y(p), self.bar_h)) {
                    break;
                }
            }
        }
        if let Some(part) = self.parts.line.map(|p| d.shown(&p)) {
            let y = line - self.line_h / 2.0;
            pen.push(
                part,
                &Cut {
                    rect: [sx, y, sw, self.line_h],
                    from: None,
                    to: None,
                },
            );
        }
        let ringing_out = d.complete && notes.section.is_some();
        if !ringing_out {
            for lane in Lane::all() {
                if !self.lane(&mut pen, notes, &scale, lane, line, from, to) {
                    break;
                }
            }
        }
        if let Some(part) = self.parts.cover.map(|p| d.shown(&p)) {
            let edge = f32_of(f64::from(self.cover_at) + g.cover_px);
            let (len, cover_at) = (f32_of(g.len), f32_of(f64::from(self.cover_at)));
            if let Some(c) = cut(sx, sw, edge - len, edge, cover_at, bottom) {
                pen.push(part, &c);
            }
        }
        if g.lift_px > 0.0
            && let Some(part) = self.parts.lift.map(|p| d.shown(&p))
        {
            let from = f32_of(f64::from(self.lift_at) - g.lift_px);
            if let Some(c) = cut(sx, sw, from, bottom, top, bottom) {
                pen.push(part, &c);
            }
        }
    }

    /// The covers' geometry for `view`. The lift takes its share of the
    /// lane, the cover its share of the height left above the lift; each is
    /// clamped so its painted edge stays a tenth of the lane above the
    /// raised judgment line, which [`LanesDef::check`]'s anchors allow at
    /// 0 ‰.
    fn covers(&self, view: LaneView) -> Covers {
        // `top < line` within ±2^20, so the length fits and is positive.
        let len = f64::from(self.line - self.top);
        let band = len / 10.0;
        let (line, cover_at) = (f64::from(self.line), f64::from(self.cover_at));
        // Both clamps are at least 0 by the anchors' check; with
        // `top ≤ cover_at`, the lift's keeps it within 900 ‰ of the lane.
        let lift_px = (len * f64::from(view.lift) / 1_000.0).min(line - cover_at - band);
        let line_y = line - lift_px;
        let above = len - lift_px;
        let cover_px = (above * f64::from(view.cover) / 1_000.0).min(line_y - band - cover_at);
        Covers {
            len,
            lift_px,
            line_y,
            cover_px,
            edge: f64::from(self.top) + cover_px,
        }
    }

    /// One lane's notes meeting `[from, to]`; `false` once the frame's
    /// lanes stop.
    #[expect(clippy::too_many_arguments, reason = "one frame's lane pass")]
    fn lane(
        &self,
        pen: &mut Pen<'_, '_>,
        notes: &Notes,
        scale: &Scale,
        lane: Lane,
        line: f32,
        from: Position,
        to: Position,
    ) -> bool {
        let d = pen.d;
        let l = lane.index();
        let (Some(list), Some(&(lo, hi)), Some(&[x, w])) = (
            notes.lanes.get(l),
            notes.range.get(l),
            d.geometry.lanes.get(l),
        ) else {
            return true;
        };
        let (x, w) = (f32_of(x), f32_of(w));
        let p = &self.parts;
        let (Some(note), Some(head_part), Some(tail_part), Some(on), Some(off), Some(mine)) = (
            p.note.get(l),
            p.head.get(l),
            p.tail.get(l),
            p.body_on.get(l),
            p.body_off.get(l),
            p.mine.get(l),
        ) else {
            return true;
        };
        // Each part's frame once for the lane's pass.
        let [note, head_part, tail_part, on, off, mine] =
            [note, head_part, tail_part, on, off, mine].map(|p| d.shown(p));
        let held = d.play.hold_note(lane);
        // Positions never decrease along a lane (`Chart::validate`), so the
        // notes meeting [from, to] are one run.
        let start = list.partition_point(|n| n.end_pos() < from).max(lo);
        for (i, n) in list.iter().enumerate().take(hi).skip(start) {
            if n.pos > to {
                break;
            }
            if !pen.visit() {
                return false;
            }
            if d.play.done(l, i) {
                continue;
            }
            let head = scale.y(n.pos);
            let drawn = match n.kind {
                // A mine leaves at its time, hit or not.
                NoteKind::Mine { .. } if n.time < d.display => true,
                NoteKind::Mine { .. } => pen.cap(mine, x, w, head, self.mine_h),
                _ if !is_target(notes.section, n.time, n.end()) => true,
                NoteKind::Tap => pen.cap(note, x, w, head, self.note_h),
                NoteKind::Long { end_pos, .. } => {
                    // Unless done by a decision before its end, a long
                    // note is drawn until its end passes the line, and once
                    // its head's time has passed the head stays on the line.
                    if end_pos < scale.at_line {
                        continue;
                    }
                    let tail = scale.y(end_pos);
                    let head = head.min(line);
                    let body = if held == Some(i) { on } else { off };
                    let body_ok = match cut(x, w, tail, head, pen.top, pen.bottom) {
                        Some(c) => pen.counted(body, &c),
                        None => true,
                    };
                    body_ok
                        && pen.cap(tail_part, x, w, tail, self.note_h)
                        && pen.cap(head_part, x, w, head, self.note_h)
                }
            };
            if !drawn {
                return false;
            }
        }
        true
    }
}
