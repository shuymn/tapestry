//! A loaded skin: its elements in draw order, its regions, its `when`
//! timers, its images and its lanes, and the per-frame build into a
//! [`QuadBatch`].

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Chart, LANE_COUNT, TimeUs};

use crate::atlas::uv::{Cycle, UvTable};
use crate::atlas::{Atlas, AtlasError, AtlasLimits, Variant};
use crate::canvas::{Canvas, Fit, f32_of};
use crate::expr::{CompileError, Program, SkinOps};
use crate::image::Sheet;
use crate::layout::{Controls, Layout, Region, RegionDef};
use crate::place::{Bind, Bounds, Fill, Place, Placement, Regions, union};
use crate::play::schema::timer::BUILTIN;
use crate::play::timers::elapsed_us;
use crate::play::{Play, PlayContext, PrepareError, SkinFrame};
use crate::quad::{Blend, ClipId, Filter, Quad, QuadBatch, RunKey};
use crate::schema::Ty;
use crate::track::Track;
use crate::widget::ImageRef;
use crate::widget::lanes::{Draw, LANE_QUADS, LANE_RESERVED, Lanes, LanesDef, LanesError, Side};

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

/// What an element draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The sheet's image at this index, at the frame its cycle shows.
    Image(u32),
    /// The lanes widget, on the skin's [`LanesDef`]. Its track gives only
    /// color and alpha.
    Lanes,
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
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Image(ImageRef),
    Lanes,
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
fn radians(deg: f32) -> f32 {
    let r = f32_of(f64::from(deg).rem_euclid(360.0).to_radians());
    if r < std::f32::consts::TAU { r } else { 0.0 }
}

/// The places checks of [`Skin::new`], in element order.
fn check_places(def: &SkinDef) -> Result<(), LoadError> {
    let fixed = def.fit == Fit::Fixed;
    let max = MAX_COORD;
    for (element, e) in def.elements.iter().enumerate() {
        let p = e.place;
        if fixed && p.region != Region::Screen {
            return Err(LoadError::RegionInFixed { element });
        }
        if p.fill != Fill::None && p.region != Region::Screen {
            return Err(LoadError::FillOutsideScreen { element });
        }
        if e.track
            .rects()
            .any(|r| r.iter().any(|v| !(-max..=max).contains(v)))
        {
            return Err(LoadError::Coordinates { element });
        }
        if e.source == Source::Lanes && !e.track.is_still() {
            return Err(LoadError::WidgetGeometry { element });
        }
    }
    let mut widgets = def.elements.iter().filter(|e| e.source == Source::Lanes);
    let lanes_in = widgets.next().map(|e| e.place.region);
    if widgets.next().is_some() {
        return Err(LoadError::LanesTwice);
    }
    let home = if fixed {
        Region::Screen
    } else {
        Region::Playfield
    };
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
            Source::Image(_) => {
                let b = e.track.bounds();
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
    /// Checks, in order: the counts (elements, `when` timers, keys across
    /// all tracks); every `when` condition and visibility is a bool; every
    /// timer exists; every element's image and every lane part's image
    /// exists, and every image's cycle timer; the places (regions under a
    /// fixed canvas, fills outside `screen`, key coordinates within
    /// ±[`MAX_COORD`], widget tracks that move, one lanes widget in its
    /// region, bindings beside it); the lanes' definition and geometry;
    /// and the total operations, each `when` charged once however many
    /// elements share it, then each visibility in element order. So the
    /// per-frame cost is known here. Then each region's box is computed
    /// from its elements' keys.
    ///
    /// These counts check what was built. A front end (an importer, the
    /// authored format's loader) enforces the same element, `when` and key
    /// limits cumulatively while it builds the input, before it compiles
    /// or allocates each item.
    ///
    /// # Errors
    ///
    /// The first check that fails.
    pub fn new(def: SkinDef) -> Result<Self, LoadError> {
        let elements = &def.elements;
        let whens = &def.whens;
        if elements.len() > MAX_ELEMENTS {
            return Err(LoadError::TooManyElements { n: elements.len() });
        }
        if whens.len() > MAX_WHEN_TIMERS {
            return Err(LoadError::TooManyWhenTimers { n: whens.len() });
        }
        let keys = elements
            .iter()
            .fold(0_usize, |n, e| n.saturating_add(e.track.key_count()));
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
        let mut slots = Vec::with_capacity(elements.len());
        for (element, e) in elements.iter().enumerate() {
            let slot = slot_of(e.timer, whens.len()).ok_or(LoadError::UnknownTimer { element })?;
            slots.push(slot);
        }
        let images = def.sheet.images();
        let exists = |i: u32| usize::try_from(i).is_ok_and(|i| i < images.len());
        if let Some(element) = elements.iter().position(|e| match e.source {
            Source::Image(i) => !exists(i),
            Source::Lanes => false,
        }) {
            return Err(LoadError::UnknownImage { element });
        }
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
        let mut ops = SkinOps::default();
        for program in whens
            .iter()
            .chain(elements.iter().filter_map(|e| e.visible.as_ref()))
        {
            ops.charge(program).map_err(LoadError::Ops)?;
        }
        let image = |i: u32| {
            let i = usize::try_from(i).unwrap_or(usize::MAX);
            let m = images.get(i);
            ImageRef {
                first: m.map_or(0, |m| m.first),
                frames: m.map_or(0, |m| m.frames),
                cycle: cycles.get(i).copied().flatten(),
                filter: m.map_or(Filter::Linear, |m| m.filter),
            }
        };
        let lanes = lanes.map(|l| Lanes::new(l, image));
        let regions = regions(&def, lanes.as_ref());
        let elements = def
            .elements
            .into_iter()
            .zip(slots)
            .map(|(e, slot)| Element {
                kind: match e.source {
                    Source::Image(i) => Kind::Image(image(i)),
                    Source::Lanes => Kind::Lanes,
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

    /// The quads a frame can push: one per image element, and the lanes'
    /// budget with its reserved quads.
    #[must_use]
    pub fn quad_capacity(&self) -> u32 {
        let images = self
            .elements
            .iter()
            .filter(|e| matches!(e.kind, Kind::Image(_)))
            .count();
        let lanes = if self.lanes.is_some() {
            LANE_QUADS + LANE_RESERVED
        } else {
            0
        };
        u32::try_from(images)
            .unwrap_or(u32::MAX)
            .saturating_add(lanes)
    }

    /// The runs a frame can push: at worst, one per quad.
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
    /// below 100 % (lane width alone never picks `Hi`). Hidden regions do
    /// not count. Exact rationals, no float. For `prepare`, not in play.
    #[must_use]
    pub fn variant(&self, target: [u32; 2], layout: &Layout) -> Variant {
        let [tw, th] = target.map(|v| u128::from(v.max(1)));
        let wide = tw * 9 >= th * 16;
        let fixed = self.regions.fit == Fit::Fixed;
        let (fit, cw, ch) = if fixed {
            let fit = Ratio(tw, 1_920).min(Ratio(th, 1_080));
            (fit, Ratio(1_920, 1), Ratio(1_080, 1))
        } else if wide {
            let cw = if tw > 4 * th {
                Ratio(4_320, 1)
            } else {
                Ratio(1_080 * tw, th)
            };
            (Ratio(th, 1_080), cw, Ratio(1_080, 1))
        } else {
            let ch = if th > 2 * tw {
                Ratio(3_840, 1)
            } else {
                Ratio(1_920 * th, tw)
            };
            (Ratio(tw, 1_920), Ratio(1_920, 1), ch)
        };
        let (fw, fh) = (cw.mul(Ratio(1, 1_920)), ch.mul(Ratio(1, 1_080)));
        let k = u128::from(self.regions.lane_width(layout).min(100));
        let one = Ratio(1, 1);
        let hi = self.elements.iter().any(|e| {
            let adjust = self.regions.effective(e.region, layout);
            if !adjust.visible {
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

    /// A fresh [`Play`] for one attempt of `chart`; a retry or the next
    /// practice round calls this again and starts clean.
    ///
    /// # Errors
    ///
    /// A negative lead-in, a countdown start outside the time range, or a
    /// section whose initial gauges do not match the context's gauges.
    pub fn prepare(&self, chart: &Chart, ctx: &PlayContext<'_>) -> Result<Play, PrepareError> {
        Play::new(chart, ctx, self.whens.len(), self.lanes.is_some())
    }

    /// Builds one frame: applies the drained effects, reads the snapshot,
    /// starts the timers, places the regions on the canvas of the frame's
    /// target, then appends each drawn element's quads to `out` in draw
    /// order (the caller clears `out`). Allocates nothing.
    ///
    /// `play` must come from this skin's [`prepare`](Self::prepare); one
    /// with another number of timers draws nothing. `uvs` must come from an
    /// atlas of this skin's sheet; a table of another sheet, or of another
    /// length, draws nothing.
    ///
    /// Each element shows its image's frame at the image's cycle timer:
    /// frame 0 while that timer is off, without a cycle, or with one frame.
    /// An image element's quad is its sampled rect moved with its lane
    /// (when bound), then by its fill or its region; its pivot is the
    /// named point of the sampled rect moved alike, and its angle turns
    /// the placed rect about it.
    ///
    /// Conditions sample once per frame, after the effects. The snapshot
    /// can be newer than a drain the effect buffer split, so a frame may
    /// show a newer combo beside an older judgment. `gauge_max` reads off
    /// while the snapshot's gauge is below its maximum; if a below-maximum
    /// effect is dropped and the gauge later refills, it keeps the earlier
    /// start.
    pub fn build(
        &self,
        play: &mut Play,
        frame: &SkinFrame<'_>,
        uvs: &UvTable,
        out: &mut QuadBatch,
    ) {
        if play.slots() != usize::from(BUILTIN) + self.whens.len()
            || uvs.sheet_id() != self.sheet_id
            || uvs.frames().len() != self.frame_count
        {
            return;
        }
        let canvas = self.canvas(frame.target);
        let placement = self.placement(&canvas, frame.layout, play.scratch());
        play.fill(frame, &self.whens);
        let play = &*play;
        let state = play.state();
        let display = TimeUs(frame.now.0.saturating_add(frame.display_offset.0));
        for e in &self.elements {
            if !placement.region(e.region).visible {
                continue;
            }
            let Some(start) = play.timer_at(e.slot) else {
                continue;
            };
            if e.visible.as_ref().is_some_and(|v| !v.eval_bool(&state)) {
                continue;
            }
            let Some(s) = e.track.sample(elapsed_us(frame.now, start)) else {
                continue;
            };
            let affine = placement.affine(e.region, e.fill);
            match e.kind {
                Kind::Image(image) => {
                    let Some(f) = image.frame(play, frame.now, uvs) else {
                        continue;
                    };
                    let [x, y, w, h] = s.rect;
                    let [fx, fy] = e.pivot;
                    let pivot = [x + fx * w, y + fy * h];
                    let [x, w] = placement.bind(e.bind, x, w);
                    let [px, _] = placement.bind(e.bind, pivot[0], 0.0);
                    out.push(
                        e.key,
                        Quad {
                            rect: affine.rect([x, y, w, h]),
                            uv: f.uv,
                            color: s.color,
                            pivot: affine.point([px, pivot[1]]),
                            angle: radians(s.angle),
                            layer: f.layer,
                            filter: image.filter,
                        },
                    );
                }
                Kind::Lanes => {
                    if let Some(lanes) = &self.lanes {
                        let d = Draw {
                            play,
                            now: frame.now,
                            display,
                            view: frame.lanes,
                            complete: frame.complete,
                            geometry: placement.geometry(),
                            affine,
                            color: s.color,
                            key: e.key,
                            uvs,
                        };
                        lanes.draw(&d, out);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::radians;

    #[test]
    fn angles_reduce_into_one_turn() {
        assert_eq!(radians(0.0), 0.0);
        assert_eq!(radians(360.0), 0.0);
        // Just below a whole turn rounds to 2π in f32; it reads 0.
        assert_eq!(radians(-1e-6), 0.0);
        assert!(radians(359.0) < std::f32::consts::TAU);
    }
}
