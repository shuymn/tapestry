//! The `ratio bar` widget (docs/stage3.md R6, S10-09d): an image cut to an
//! exact ratio of its axis, or moved along a slider's travel by it. A
//! slider never changes state: a press on it gives the ratio under the
//! pointer as a command for the screen to carry out
//! ([`Skin::drag_at`](crate::Skin::drag_at), [`Skin::hit_at`](crate::Skin::hit_at)).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::ImageRef;
use crate::atlas::uv::FrameUv;
use crate::canvas::f32_of;
use crate::selection::Ratio;

/// What a ratio bar shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RatioSource {
    /// The cursor among the list's selectable rows
    /// ([`SelectionView::position`](crate::SelectionView::position)).
    ListPosition,
    /// The selected chart's best EX score over the largest its summary
    /// allows; none without both or with a largest of 0.
    ExRate,
    /// The master gain that plays, exactly
    /// ([`MasterVolume::ratio`](crate::MasterVolume::ratio)); none before
    /// the settings.
    MasterVolume,
    /// A play's score rate: a play skin's source, and only a play skin's,
    /// as the two below.
    Score(ScoreRate),
    /// The load's progress, `load.percent / 100`: live while the play
    /// screen loads, 1 in a play's frames.
    LoadProgress,
    /// The attempt's progress, `time.progress_ppm / 10⁶`: 0 while loading
    /// and in READY, 1 from the attempt's end.
    SongProgress,
}

impl RatioSource {
    /// Whether it is a play's source, which only a play skin draws.
    #[must_use]
    pub const fn play(self) -> bool {
        matches!(
            self,
            Self::Score(_) | Self::LoadProgress | Self::SongProgress
        )
    }

    /// The play field it reads and the field's full scale, for the two
    /// progress sources; `None` for any other.
    #[must_use]
    pub fn field(self) -> Option<(u16, u64)> {
        use crate::play::schema::field;
        match self {
            Self::LoadProgress => Some((field::LOAD_PERCENT, 100)),
            Self::SongProgress => Some((
                field::TIME_PROGRESS_PPM,
                u64::from(crate::play::schema::PROGRESS_FULL.unsigned_abs()),
            )),
            Self::ListPosition | Self::ExRate | Self::MasterVolume | Self::Score(_) => None,
        }
    }
}

/// A play's score rate, exact, with `ex` the EX score, `p` the judged
/// targets, `n` the attempt's targets, B and T the play's best and target
/// ([`ScoreBasis`](crate::ScoreBasis)). An empty denominator gives 0;
/// a rate past 1 draws at 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScoreRate {
    /// `ex / 2n`.
    Current,
    /// `ex / 2p` once the attempt has a judgment (1 while `p` is 0), 0
    /// before.
    Judged,
    /// The best paced linearly to the judged targets, `⌊B · p / n⌋ / 2n`;
    /// none for a chart that declares `#RANDOM`.
    PacedBest,
    /// `B / 2n`; none for a chart that declares `#RANDOM`.
    Best,
    /// The target paced alike, `⌊T · p / n⌋ / 2n`.
    PacedTarget,
    /// `T / 2n`.
    Target,
}

impl ScoreRate {
    /// Every rate, in index order.
    pub const ALL: [Self; 6] = [
        Self::Current,
        Self::Judged,
        Self::PacedBest,
        Self::Best,
        Self::PacedTarget,
        Self::Target,
    ];

    /// Its index in [`ALL`](Self::ALL).
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Which way a slider moves as its ratio grows, on the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SliderDirection {
    Up,
    Right,
    Down,
    Left,
}

/// How a ratio bar draws its ratio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RatioForm {
    /// The image cut to the ratio of its width from the left, or with
    /// `vertical` of its height from the bottom, the uv cut alike
    /// (`skin/SkinGraph.java:98-106`).
    Bar { vertical: bool },
    /// The image moved `range` reference pixels times the ratio
    /// (`skin/SkinSlider.java:117-119`); a press on its travel gives a
    /// command when `changeable`.
    Slider {
        direction: SliderDirection,
        range: u32,
        changeable: bool,
    },
}

/// A ratio bar as a front end builds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RatioBarDef {
    /// The sheet image.
    pub image: u32,
    pub form: RatioForm,
    pub source: RatioSource,
}

/// A loaded ratio bar: its definition and its image.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RatioBar {
    pub(crate) def: RatioBarDef,
    pub(crate) image: ImageRef,
}

/// Why a ratio bar was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RatioBarError {
    #[error("the ratio bar's image does not exist")]
    UnknownImage,
    #[error("the slider's range is past the coordinate range")]
    Range,
}

/// What a press on a slider asks for: its source set to `value`, in 0 to
/// 1. The screen carries it out; the skin changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SliderCommand {
    pub source: RatioSource,
    pub value: Ratio,
}

/// The denominator of a slider command's value: a millionth of the travel.
pub const SLIDER_STEPS: u64 = 1_000_000;

/// `r` as a fraction, divided once and never clamped: the source's graph
/// draws its rate as it is (`skin/SkinGraph.java:86-106`,
/// `skin/property/FloatPropertyFactory.java:58-61`).
#[expect(
    clippy::cast_precision_loss,
    reason = "a ratio's terms below 2^53 are exact; a fraction is the intent"
)]
pub(crate) fn fraction(r: Ratio) -> f32 {
    let f = r.numerator as f64 / r.denominator as f64;
    f32_of(f)
}

/// The rect and uv of a bar showing `f` of `[x, y, w, h]` with frame
/// `frame`, `f` drawn at most 1: a ratio above 1 draws the whole box. The
/// source extends its region past the image for one
/// (`skin/SkinGraph.java:100-105`), which an atlas cannot give; valid data
/// never gets there, since a best EX never passes the largest.
pub(crate) fn cut(
    vertical: bool,
    [x, y, w, h]: [f32; 4],
    frame: FrameUv,
    f: f32,
) -> ([f32; 4], FrameUv) {
    let [u0, v0, u1, v1] = frame.uv;
    let f = f.min(1.0);
    let (rect, uv) = if vertical {
        (
            [x, y + h * (1.0 - f), w, h * f],
            [u0, v1 - (v1 - v0) * f, u1, v1],
        )
    } else {
        ([x, y, w * f, h], [u0, v0, u0 + (u1 - u0) * f, v1])
    };
    (rect, FrameUv { uv, ..frame })
}

/// A slider's rect `[x, y, w, h]` moved by `f` of `range`.
pub(crate) fn slide(
    direction: SliderDirection,
    range: u32,
    [x, y, w, h]: [f32; 4],
    f: f32,
) -> [f32; 4] {
    #[expect(clippy::cast_precision_loss, reason = "a range within ±2^20 is exact")]
    let d = range as f32 * f;
    match direction {
        SliderDirection::Up => [x, y - d, w, h],
        SliderDirection::Right => [x + d, y, w, h],
        SliderDirection::Down => [x, y + d, w, h],
        SliderDirection::Left => [x - d, y, w, h],
    }
}

/// A slider's travel as drawn in the last build, in target pixels: the
/// band a press reads, as the source's (`skin/SkinSlider.java:124-178`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hit {
    pub(crate) source: RatioSource,
    pub(crate) direction: SliderDirection,
    /// The rect at ratio 0, placed: left, top, width, height.
    pub(crate) rect: [f32; 4],
    /// The travel's length in target pixels.
    pub(crate) range: f32,
    /// A target pixel per reference pixel: the ends snap within one
    /// reference pixel.
    pub(crate) scale: f32,
    /// The target size of that build.
    pub(crate) target: [u32; 2],
}

impl Hit {
    /// The command a press at `[px, py]` gives, when it lies on the travel.
    pub(crate) fn at(&self, [px, py]: [f32; 2]) -> Option<SliderCommand> {
        let [x, y, w, h] = self.rect;
        let b = y + h;
        let r = self.range;
        let (inside, along) = match self.direction {
            SliderDirection::Up => (x <= px && px <= x + w && b - r <= py && py <= b, b - py),
            SliderDirection::Down => (x <= px && px <= x + w && b <= py && py <= b + r, py - b),
            SliderDirection::Right => (x <= px && px <= x + r && y <= py && py <= b, px - x),
            SliderDirection::Left => (x - r <= px && px <= x && y <= py && py <= b, x - px),
        };
        if !inside || r <= 0.0 {
            return None;
        }
        let steps = if along < self.scale {
            0
        } else if r - along < self.scale {
            SLIDER_STEPS
        } else {
            let v = (f64::from(along) / f64::from(r)).clamp(0.0, 1.0);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "rounded in [0, 10^6]"
            )]
            let s = (v * SLIDER_STEPS as f64).round() as u64;
            s
        };
        Some(SliderCommand {
            source: self.source,
            value: Ratio::new(steps, SLIDER_STEPS)?,
        })
    }
}
