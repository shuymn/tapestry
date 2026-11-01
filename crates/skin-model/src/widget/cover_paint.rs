//! Cover paint: what a skin paints for SUDDEN+, LIFT and HIDDEN+, apart
//! from the logical coverage the lanes scroll under.
//!
//! The coverage is the frame's [`LaneView`] and the geometry resolved from
//! it once per frame; paint never changes it. A paint element is an
//! ordinary element with its own track, timer, visibility, blend, clip and
//! placement, showing an image or an image choice's entry, moved by the
//! cover it paints ([`CoverMotion`]), by the covers it follows
//! ([`Follow`](crate::Follow)) and, as a slider, down by its range times
//! the effective SUDDEN+ share of the whole lane. A trim line
//! ([`CoverTrim`]) gates and clips it against its unstretched, unrotated
//! destination. An invisible, transparent, missing or timer-off paint
//! draws nothing and leaves the coverage and the notes as they are;
//! nothing replaces it.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::ImageRef;
use super::lanes::{LaneFrameGeometry, LaneView};
use crate::canvas::f32_of;
use crate::place::{Affine, LaneMap};
use crate::quad::ClipRect;
use crate::track::Track;

/// What a cover paint shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverImage {
    /// The sheet's image at this index, at the frame its cycle shows.
    Image(u32),
    /// The image choice table at this index of
    /// [`WidgetDefs::image_choices`](crate::WidgetDefs::image_choices):
    /// the entry its selector picks, on the entry's track and native size.
    Choice(u32),
}

/// The cover a paint moves with, from the frame's lane geometry, y down:
/// none, up by the lift's raise `L`, or up by the lift's raise and the
/// hidden cover's own share `L + K`. Without lanes neither moves it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CoverMotion {
    #[default]
    None,
    /// LIFT's paint: up by `L`.
    Lift,
    /// HIDDEN+'s paint: up by `L + K`, drawn only while HIDDEN+ is on
    /// ([`CoverSettings::hidden_enabled`](crate::CoverSettings)), at any
    /// amount.
    Hidden,
}

/// Where a paint is cut off below, in reference pixels, y down: nothing of
/// it is drawn under the line. Each base is at most
/// [`MAX_COORD`](crate::MAX_COORD) in magnitude.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CoverTrim {
    /// No trim line.
    #[default]
    None,
    /// A fixed line at this y.
    Fixed(i32),
    /// A line at this y with the lift at 0 ‰, rising with the lift by `L`
    /// as the judgment line does.
    Lift(i32),
}

/// A cover's paint, which an element draws by index
/// ([`Source::CoverPaint`](crate::Source::CoverPaint)).
///
/// With a trim line, the destination the trim is tested against (the gate)
/// is `gate_track` sampled on the referencing element's timer, or else the
/// selected paint track's rect, moved as the paint moves: a gate whose
/// upper edge is at or below the line draws nothing; one the line crosses
/// clips the paint to the gate's part above the line, unrotated; one wholly
/// above the line draws the paint whole, even where a native size or a
/// stretch makes the paint reach past the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverPaintDef {
    pub image: CoverImage,
    /// A slider's range in reference pixels, at most
    /// [`MAX_COORD`](crate::MAX_COORD): the paint moves down by it times
    /// the effective SUDDEN+ share of the whole lane,
    /// `R · cover · (1000 − lift) / 10⁶`, also for a range of 0. `None`
    /// adds no motion of its own. Only a paint without motion, trim and
    /// gate track has one.
    pub slider_range: Option<u32>,
    pub motion: CoverMotion,
    pub trim: CoverTrim,
    /// The unstretched destination the trim is tested against, when it is
    /// not the paint track; only its rects are read, and only a trimmed
    /// paint has one. A gate track that does not draw at the element's time
    /// draws nothing.
    pub gate_track: Option<Track>,
}

/// Why a cover paint definition was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CoverPaintError {
    #[error("its image does not exist")]
    UnknownImage,
    #[error("its image choice table does not exist")]
    UnknownChoice,
    #[error("its slider range is above {}", crate::MAX_COORD)]
    Range,
    #[error("it has a slider and a motion, a trim or a gate track")]
    SliderMotion,
    #[error("it has a gate track but no trim")]
    GateWithoutTrim,
    #[error("its gate track's keys leave ±{}", crate::MAX_COORD)]
    GateCoordinates,
    #[error("its gate track has a negative width or height")]
    GateSize,
    #[error("its trim line is beyond ±{}", crate::MAX_COORD)]
    TrimRange,
}

/// What a loaded paint shows.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PaintImage {
    Image(ImageRef),
    /// The image choice table's index.
    Choice(usize),
}

/// A loaded paint.
#[derive(Clone, Debug)]
pub(crate) struct CoverPaint {
    pub(crate) image: PaintImage,
    pub(crate) slider: Option<u32>,
    pub(crate) motion: CoverMotion,
    pub(crate) trim: CoverTrim,
    pub(crate) gate: Option<Track>,
}

/// How a trimmed paint is drawn this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TrimOutcome {
    /// Not at all.
    Suppressed,
    /// Whole.
    Unclipped,
    /// Clipped to this canvas rect.
    Clipped(ClipRect),
}

impl CoverPaint {
    /// The slider's displacement this frame in reference pixels, y down:
    /// `R · c · (1000 − l) / 10⁶` of the view's effective amounts, each
    /// at most 1 000 ‰ (`skin/property/FloatPropertyFactory.java:139-151`,
    /// `skin/SkinSlider.java:116-120` at `0.8.9`). The numerator is exact
    /// in `u64` and `f64` (below 2^41); only the division rounds.
    pub(crate) fn slider_dy(&self, view: LaneView) -> f64 {
        let Some(r) = self.slider else {
            return 0.0;
        };
        let c = u64::from(view.cover.min(1_000));
        let l = u64::from(view.lift.min(1_000));
        let n = u64::from(r) * c * (1_000 - l);
        #[expect(clippy::cast_precision_loss, reason = "below 2^41, exact in f64")]
        let n = n as f64;
        n / 1_000_000.0
    }

    /// The motion's displacement this frame, y down: 0, `−L` or `−L − K`
    /// of the frame's geometry `g` (`play/LaneRenderer.java:332-345`,
    /// `skin/json/JsonPlaySkinObjectLoader.java:182-210`); 0 without lanes.
    pub(crate) fn motion_dy(&self, g: Option<&LaneFrameGeometry>) -> f64 {
        let Some(g) = g else {
            return 0.0;
        };
        match self.motion {
            CoverMotion::None => 0.0,
            CoverMotion::Lift => -g.lift_px,
            CoverMotion::Hidden => -g.lift_px - g.hidden_px,
        }
    }

    /// The trim line this frame, y down: the fixed base, or the linked one
    /// raised by `L` (`play/SkinHidden.java:60-68`); `None` without one.
    fn trim_y(&self, g: Option<&LaneFrameGeometry>) -> Option<f64> {
        match self.trim {
            CoverTrim::None => None,
            CoverTrim::Fixed(y) => Some(f64::from(y)),
            CoverTrim::Lift(y) => Some(f64::from(y) - g.map_or(0.0, |g| g.lift_px)),
        }
    }

    /// How the paint draws against its gate `[x, y, w, h]` (reference
    /// pixels, sampled, before `dy` moves it): whole without a trim line;
    /// not at all when the gate's upper edge is at or below the line; clipped
    /// to the gate's part above the line, through the lane `map` and the
    /// `affine` and never rotated, when the line crosses the gate, and not
    /// at all when that part has no area on the canvas; whole otherwise
    /// (`play/SkinHidden.java:73-90`, `skin/Skin.java:567`).
    pub(crate) fn outcome(
        &self,
        [x, y, w, h]: [f32; 4],
        dy: f64,
        g: Option<&LaneFrameGeometry>,
        map: LaneMap,
        affine: Affine,
    ) -> TrimOutcome {
        let Some(t) = self.trim_y(g) else {
            return TrimOutcome::Unclipped;
        };
        let top = f64::from(y) + dy;
        if top >= t {
            return TrimOutcome::Suppressed;
        }
        if t >= top + f64::from(h) {
            return TrimOutcome::Unclipped;
        }
        let rect = affine.rect([map.x(x), f32_of(top), map.w(w), f32_of(t - top)]);
        ClipRect::new(rect).map_or(TrimOutcome::Suppressed, TrimOutcome::Clipped)
    }
}
