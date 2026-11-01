//! Cover paint: what a skin paints for SUDDEN+, apart from the logical
//! coverage the lanes scroll under.
//!
//! The coverage is the frame's [`LaneView`] and the geometry resolved from
//! it once per frame; paint never changes it. A paint element is an
//! ordinary element with its own track, timer, visibility, blend, clip and
//! placement, showing an image or an image choice's entry, moved by the
//! covers it follows ([`Follow`](crate::Follow)) and, as a slider, down by
//! its range times the effective SUDDEN+ share of the whole lane. An
//! invisible, transparent, missing or timer-off paint draws nothing and
//! leaves the coverage and the notes as they are; nothing replaces it.
//!
//! HIDDEN+ and LIFT keep their parts in the lanes; a marker element
//! ([`LanePaint`]) says where in the draw order each is painted.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::ImageRef;
use super::lanes::LaneView;

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

/// A SUDDEN+ paint, which an element draws by index
/// ([`Source::CoverPaint`](crate::Source::CoverPaint)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverPaintDef {
    pub image: CoverImage,
    /// A slider's range in reference pixels, at most
    /// [`MAX_COORD`](crate::MAX_COORD): the paint moves down by it times
    /// the effective SUDDEN+ share of the whole lane,
    /// `R · cover · (1000 − lift) / 10⁶`, also for a range of 0. `None`
    /// adds no motion of its own.
    pub slider_range: Option<u32>,
}

/// A legacy cover part the lanes paint, which a marker element
/// ([`Source::LanePaint`](crate::Source::LanePaint)) places in the draw
/// order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanePaint {
    /// The hidden cover (HIDDEN+).
    Hidden,
    /// The lift cover.
    Lift,
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
}

/// Why the lane paint markers were refused; `element` is an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LanePaintError {
    #[error("element {element} is not a canonical lane paint marker")]
    Noncanonical { element: usize },
    #[error("element {element} marks the {part:?} cover a second time")]
    Duplicate { part: LanePaint, element: usize },
    #[error("element {element} marks the {part:?} cover, which the lanes do not have")]
    Absent { part: LanePaint, element: usize },
    #[error("no element marks the lanes' {part:?} cover")]
    Missing { part: LanePaint },
}

/// What a loaded paint shows.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PaintImage {
    Image(ImageRef),
    /// The image choice table's index.
    Choice(usize),
}

/// A loaded paint.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CoverPaint {
    pub(crate) image: PaintImage,
    pub(crate) slider: Option<u32>,
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
}
