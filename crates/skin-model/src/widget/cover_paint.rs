//! Cover paint: what a skin paints for SUDDEN+, LIFT and HIDDEN+, apart
//! from the logical coverage the lanes scroll under.
//!
//! The coverage is the frame's [`LaneView`] and the geometry resolved from
//! it once per frame; paint never changes it. A paint element is an
//! ordinary element with its own track, timer, visibility, blend, clip and
//! placement, showing an image or an image choice's entry, moved by the
//! cover it paints ([`CoverMotion`]), by the covers it follows
//! ([`Follow`](crate::Follow)) and, as a slider ([`Slider`]), by its range
//! times a rate in one direction: the effective SUDDEN+ share of the whole
//! lane, or the attempt's progress, which makes a paint that paints no
//! cover (a progress bar's knob, drawn as the source's slider). A trim line
//! ([`CoverTrim`]) gates and clips it against its unstretched, unrotated
//! destination. An invisible, transparent, missing or timer-off paint
//! draws nothing and leaves the coverage and the notes as they are;
//! nothing replaces it.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::ImageRef;
use super::lanes::{LaneFrameGeometry, LaneView};
use super::ratio_bar::SliderDirection;
use crate::canvas::f32_of;
use crate::place::{Affine, LaneMap};
use crate::play::schema::PROGRESS_FULL;
use crate::quad::ClipRect;
use crate::selection::Ratio;
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
    /// A slider: the paint moves by its range times its rate in its
    /// direction, also for a range of 0. `None` adds no motion of its own.
    /// Only a paint without motion, trim and gate track has one.
    pub slider: Option<Slider>,
    pub motion: CoverMotion,
    pub trim: CoverTrim,
    /// The unstretched destination the trim is tested against, when it is
    /// not the paint track; only its rects are read, and only a trimmed
    /// paint has one. A gate track that does not draw at the element's time
    /// draws nothing.
    pub gate_track: Option<Track>,
}

/// A paint's slider (`skin/SkinSlider.java:116-120` at `0.8.9`): moved by
/// `range` reference pixels times its rate, from 0 to 1, toward
/// `direction`. The range is exact ([`range_within`]): its denominator
/// from 1 to [`MAX_RANGE_DENOMINATOR`], its value at most
/// [`MAX_COORD`](crate::MAX_COORD).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slider {
    pub range: Ratio,
    pub rate: SliderRate,
    pub direction: SliderDirection,
}

impl Slider {
    /// SUDDEN+'s slider: down by `range` whole reference pixels times
    /// SUDDEN+'s share.
    #[must_use]
    pub const fn sudden(range: u32) -> Self {
        Self {
            range: Ratio {
                numerator: range as u64,
                denominator: 1,
            },
            rate: SliderRate::Sudden,
            direction: SliderDirection::Down,
        }
    }
}

/// The largest denominator of a slider's range. With the range at most
/// [`MAX_COORD`](crate::MAX_COORD) (2^20), its numerator is at most 2^28,
/// so its product with an `f32`, 24 significant bits, has at most 52 and
/// is exact in `f64`.
pub const MAX_RANGE_DENOMINATOR: u64 = 256;

/// Whether a slider's range `r` is one a skin may hold: its denominator
/// from 1 to [`MAX_RANGE_DENOMINATOR`] and its value at most
/// [`MAX_COORD`](crate::MAX_COORD). Checked before any reach or motion is
/// computed from it.
pub(crate) fn range_within(r: Ratio) -> bool {
    (1..=MAX_RANGE_DENOMINATOR).contains(&r.denominator)
        && r.numerator <= u64::from(crate::MAX_COORD.unsigned_abs()) * r.denominator
}

/// The rate a slider moves by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliderRate {
    /// The effective SUDDEN+ share of the whole lane,
    /// `cover · (1000 − lift) / 10⁶` of the frame's lane view
    /// (`skin/property/FloatPropertyFactory.java:139-151`).
    Sudden,
    /// The attempt's progress, `time.progress_ppm / 10⁶`
    /// (`skin/property/FloatPropertyFactory.java:128-136`).
    Progress,
    /// Program callback `k`'s float output, `lua.<k>`: the skin's own
    /// `value`, the source's `tofloat()` of it
    /// (`skin/lua/SkinLuaAccessor.java:665-673`), unclamped; a non-finite
    /// value moves it by 0 (the source draws it at an infinity or NaN).
    /// Only a slider reads such an output, one slider each (V9); while its
    /// `ok` is not 1 (an unavailable read, or a frame the program did not
    /// run, such as a loading one) the element is not drawn.
    Program(u16),
}

/// The largest slider displacement, in reference pixels, a value slider is
/// drawn at: past it the element is not drawn that frame (docs/stage3.md
/// D24), where the source draws it, off the window unless the paint's own
/// geometry is of that order (a stretch-8 native size under a ±10⁶
/// destination scale). Within it the placement's scales (unit for imported
/// elements, at least ½ for an adaptive region, about ¼ for the
/// lane-width map) keep every edge finite in `f32`.
pub const MAX_SLIDE: f64 = 1_099_511_627_776.0;

/// Why a cover paint definition was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CoverPaintError {
    #[error("its image does not exist")]
    UnknownImage,
    #[error("its image choice table does not exist")]
    UnknownChoice,
    #[error(
        "its slider range is above {} or its denominator outside 1 to {MAX_RANGE_DENOMINATOR}",
        crate::MAX_COORD
    )]
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

/// Why a slider's paint is not drawn this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlideStop {
    /// Its program value gave none.
    NoValue,
    /// Its displacement is past [`MAX_SLIDE`].
    Beyond,
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
    pub(crate) slider: Option<Slider>,
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
    /// The slider's displacement `[dx, dy]` this frame in reference
    /// pixels, y down: `R = n / d` times its rate toward its direction
    /// (`skin/SkinSlider.java:116-120` at `0.8.9`), `R` a checked range
    /// ([`range_within`]: `n` at most 2^28, `d` at most 2^8). SUDDEN+'s rate
    /// is `c · (1000 − l) / 10⁶` of the view's effective amounts, each at
    /// most 1 000 ‰ (`skin/property/FloatPropertyFactory.java:139-151`);
    /// the progress's `p / 10⁶` of `progress`, `time.progress_ppm` clamped
    /// to 0..=10⁶. `n` times either rate (below 2^48) and `d · 10⁶` are
    /// exact in `u64` and `f64`; only their division rounds. A program
    /// rate reads its output from `fields` (`base` the slot of output 0):
    /// not drawn without a value (its `ok` not 1), else its `f32` bits, a
    /// non-finite value as 0; `n · value` has at most 24 + 28 significant
    /// bits and is finite, so it is exact in `f64` and only its division
    /// by `d` rounds; past [`MAX_SLIDE`] it is not drawn.
    pub(crate) fn slider_offset(
        &self,
        view: LaneView,
        progress: i32,
        fields: &[i32],
        base: usize,
    ) -> Result<[f64; 2], SlideStop> {
        let Some(s) = self.slider else {
            return Ok([0.0, 0.0]);
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "a checked range's terms are at most 2^28, exact in f64"
        )]
        let (n, den) = (s.range.numerator as f64, s.range.denominator as f64);
        let d = match s.rate {
            SliderRate::Sudden | SliderRate::Progress => {
                let rate = if s.rate == SliderRate::Sudden {
                    let c = u64::from(view.cover.min(1_000));
                    let l = u64::from(view.lift.min(1_000));
                    c * (1_000 - l)
                } else {
                    u64::from(progress.clamp(0, PROGRESS_FULL).unsigned_abs())
                };
                #[expect(clippy::cast_precision_loss, reason = "below 2^20, exact in f64")]
                let rate = rate as f64;
                n * rate / (den * 1_000_000.0)
            }
            SliderRate::Program(k) => {
                let at = base.saturating_add(usize::from(k).saturating_mul(2));
                if fields.get(at.saturating_add(1)) != Some(&1) {
                    return Err(SlideStop::NoValue);
                }
                #[expect(
                    clippy::cast_sign_loss,
                    reason = "the slot holds the f32's bits (`present::exec`)"
                )]
                let v = f32::from_bits(fields.get(at).copied().unwrap_or(0) as u32);
                let v = if v.is_finite() { f64::from(v) } else { 0.0 };
                let d = v * n / den;
                if d.abs() > MAX_SLIDE {
                    return Err(SlideStop::Beyond);
                }
                d
            }
        };
        Ok(match s.direction {
            SliderDirection::Up => [0.0, -d],
            SliderDirection::Right => [d, 0.0],
            SliderDirection::Down => [0.0, d],
            SliderDirection::Left => [-d, 0.0],
        })
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

#[cfg(test)]
mod tests {
    use super::{CoverMotion, CoverPaint, CoverTrim, PaintImage, Slider, SliderRate, range_within};
    use crate::selection::Ratio;
    use crate::widget::lanes::LaneView;
    use crate::widget::ratio_bar::SliderDirection;

    /// A rate's motion rounds once: `n · rate / (d · 10⁶)`, here
    /// 380511.90072836366, where dividing by `d` first rounds twice and
    /// gives 380511.9007283636.
    #[test]
    fn a_rate_s_motion_rounds_once() {
        let range = Ratio {
            numerator: 15_402_619,
            denominator: 22,
        };
        assert!(range_within(range));
        let paint = CoverPaint {
            image: PaintImage::Choice(0),
            slider: Some(Slider {
                range,
                rate: SliderRate::Progress,
                direction: SliderDirection::Down,
            }),
            motion: CoverMotion::None,
            trim: CoverTrim::None,
            gate: None,
        };
        assert_eq!(
            paint.slider_offset(LaneView::NONE, 543_496, &[], 0),
            Ok([0.0, 380_511.900_728_363_66])
        );
    }
}
