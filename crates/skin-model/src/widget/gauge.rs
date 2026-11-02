//! The `gauge` widget: the shown gauge as a row of segments, each drawn
//! with the front, back or tip image of the gauge's look, at or below its
//! border, and moved by a motion that is a pure function of the element's
//! timer.
//!
//! With `P` segments, the value `V` and the maximum `M`, `n` segments are
//! lit: 0 when `V ≤ 0`, else `min(P, max(1, ⌊V·P/M⌋))`. Segment `i`
//! (1-based) is below the border when `i·M < border·P`. All of it is
//! exact integer arithmetic on the raw [`GaugeValue`]; floats appear only
//! in the geometry.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;
use eval::GaugeState;
use rules::{GaugeKind, GaugeRule, GaugeValue};

use super::{Brush, Frame, ImageRef};
use crate::canvas::f32_of;
use crate::play::GaugeDesc;

use crate::play::schema::GAUGE_PERMILLE;
use crate::quad::QuadBatch;
use crate::track::MAX_KEY_US;

/// The most segments a gauge may have: one per ‰ of the gauge field. A
/// policy cap; the widget reads the raw gauge value.
#[expect(clippy::cast_possible_truncation, reason = "1 000")]
pub const MAX_SEGMENTS: u16 = GAUGE_PERMILLE as u16;

/// How a gauge looks: a skin draws each with its own images.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GaugeLook {
    AssistEasy,
    Easy,
    Normal,
    Hard,
    ExHard,
    Hazard,
}

impl GaugeLook {
    /// The number of looks.
    pub const COUNT: usize = 6;
    /// Every look, in index order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::AssistEasy,
        Self::Easy,
        Self::Normal,
        Self::Hard,
        Self::ExHard,
        Self::Hazard,
    ];

    /// The look's index into [`GaugeDef::looks`].
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The look's value in the `gauge_kind` enum of `gauge.kind`
    /// ([`GAUGE_KIND_ENUM`](crate::GAUGE_KIND_ENUM)).
    pub(crate) const fn schema_value(self) -> i32 {
        match self {
            Self::AssistEasy => 0,
            Self::Easy => 1,
            Self::Normal => 2,
            Self::Hard => 3,
            Self::ExHard => 4,
            Self::Hazard => 5,
        }
    }

    /// The look of a preset's gauge: by its id when it is one of the
    /// built-in presets' (`assist_easy`, `easy`, `normal`, `hard`,
    /// `ex_hard`, `hazard`), else by its kind (`Normal` for an end
    /// threshold, `Hard` for survival).
    #[must_use]
    pub fn of(rule: &GaugeRule) -> Self {
        match rule.id.as_str() {
            "assist_easy" => Self::AssistEasy,
            "easy" => Self::Easy,
            "normal" => Self::Normal,
            "hard" => Self::Hard,
            "ex_hard" => Self::ExHard,
            "hazard" => Self::Hazard,
            _ => match rule.kind {
                GaugeKind::EndThreshold { .. } => Self::Normal,
                GaugeKind::Survival { .. } => Self::Hard,
            },
        }
    }
}

/// One look's images, each sheet image index, as `[at or above the
/// border, below it]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeParts {
    /// A lit segment.
    pub front: [u32; 2],
    /// An unlit segment.
    pub back: [u32; 2],
    /// The last lit segment.
    pub tip: [u32; 2],
}

/// A part of a look, for errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GaugePart {
    Front,
    Back,
    Tip,
}

/// How the lit end moves, from the element's local time `t`: with
/// `k = ⌊t / step⌋`, the offset `a` is `mix(k)`, `−k` or `k`, each modulo
/// `range + 1`, and segment `i < n` is front when `i + a < n`, else back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Still,
    /// `a = splitmix64(k) mod (range + 1)`.
    Random {
        range: u16,
        step: TimeUs,
    },
    /// The gap shrinks: `a = (−k) mod (range + 1)`.
    Rise {
        range: u16,
        step: TimeUs,
    },
    /// The gap grows: `a = k mod (range + 1)`.
    Fall {
        range: u16,
        step: TimeUs,
    },
    /// Every lit segment front, and the tip over the last with an alpha
    /// rising from 0 at the period's start to 1 at its middle and back.
    Flicker {
        period: TimeUs,
    },
}

/// A gauge: the element's sampled rect split into `segments`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeDef {
    /// 1 to [`MAX_SEGMENTS`].
    pub segments: u16,
    /// Per [`GaugeLook::index`].
    pub looks: [GaugeParts; GaugeLook::COUNT],
    pub motion: Motion,
}

/// Why a gauge was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GaugeError {
    #[error("the segments are not 1 to {MAX_SEGMENTS}")]
    Segments,
    #[error("the {look:?} {part:?} image ({}) does not exist", if *below { "below" } else { "at or above" })]
    UnknownImage {
        look: GaugeLook,
        part: GaugePart,
        below: bool,
    },
    #[error("the motion's step is not 1 µs to {MAX_KEY_US} µs")]
    Step,
    #[error("the flicker's period is not 1 µs to {MAX_KEY_US} µs")]
    Period,
    #[error("the motion's range is above the segments")]
    Range,
}

impl GaugeDef {
    /// Every image, with its look, part and side.
    pub(crate) fn images(&self) -> impl Iterator<Item = (GaugeLook, GaugePart, bool, u32)> + '_ {
        GaugeLook::ALL
            .into_iter()
            .zip(&self.looks)
            .flat_map(|(look, p)| {
                [
                    (GaugePart::Front, p.front),
                    (GaugePart::Back, p.back),
                    (GaugePart::Tip, p.tip),
                ]
                .into_iter()
                .flat_map(move |(part, [above, below])| {
                    [(look, part, false, above), (look, part, true, below)]
                })
            })
    }

    /// The parameters' checks: segments, step, period, range.
    pub(crate) fn check(&self) -> Result<(), GaugeError> {
        if !(1..=MAX_SEGMENTS).contains(&self.segments) {
            return Err(GaugeError::Segments);
        }
        let time = |t: TimeUs| (1..=MAX_KEY_US).contains(&t.0);
        match self.motion {
            Motion::Still => {}
            Motion::Random { range, step }
            | Motion::Rise { range, step }
            | Motion::Fall { range, step } => {
                if !time(step) {
                    return Err(GaugeError::Step);
                }
                if range > self.segments {
                    return Err(GaugeError::Range);
                }
            }
            Motion::Flicker { period } => {
                if !time(period) {
                    return Err(GaugeError::Period);
                }
            }
        }
        Ok(())
    }

    /// The quads one frame can push: every segment, and the flicker's tip.
    pub(crate) fn quads(&self) -> u32 {
        let tip = u32::from(matches!(self.motion, Motion::Flicker { .. }));
        u32::from(self.segments) + tip
    }
}

impl GaugeDesc {
    /// The descriptor of a preset's gauge rule: its start, maximum, look
    /// ([`GaugeLook::of`]) and border (the clear threshold, or the value a
    /// survival gauge fails below).
    #[must_use]
    pub fn from_rule(rule: &GaugeRule) -> Self {
        Self {
            start: rule.start,
            max: rule.max,
            look: GaugeLook::of(rule),
            border: match rule.kind {
                GaugeKind::EndThreshold { clear } => clear,
                GaugeKind::Survival { fails_below } => fails_below,
            },
        }
    }
}

/// splitmix64's output for `k`.
pub(crate) fn mix(k: u64) -> u64 {
    let mut z = k.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The lit segments of `p` for `value` against `max`.
pub(crate) fn lit(value: GaugeValue, max: GaugeValue, p: u16) -> u16 {
    let (v, m, p) = (i128::from(value.0), i128::from(max.0), i128::from(p));
    if v <= 0 || m <= 0 {
        return 0;
    }
    let n = (v * p / m).clamp(1, p);
    u16::try_from(n).unwrap_or(0)
}

impl Motion {
    /// The offset `a` at local time `t` (µs, at least 0).
    pub(crate) fn offset(self, t: i64) -> i64 {
        // The step `k` and the modulus `range + 1`.
        let at = |range: u16, step: TimeUs| (t.max(0) / step.0.max(1), i64::from(range) + 1);
        match self {
            Self::Still | Self::Flicker { .. } => 0,
            Self::Random { range, step } => {
                let (k, r) = at(range, step);
                i64::try_from(mix(k.unsigned_abs()) % r.unsigned_abs()).unwrap_or(0)
            }
            Self::Rise { range, step } => {
                let (k, r) = at(range, step);
                (-k).rem_euclid(r)
            }
            Self::Fall { range, step } => {
                let (k, r) = at(range, step);
                k.rem_euclid(r)
            }
        }
    }
}

/// A look's images as a build reads them, `[at or above, below]`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Look {
    pub(crate) front: [ImageRef; 2],
    pub(crate) back: [ImageRef; 2],
    pub(crate) tip: [ImageRef; 2],
}

/// A loaded gauge.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Gauge {
    pub(crate) segments: u16,
    pub(crate) looks: [Look; GaugeLook::COUNT],
    pub(crate) motion: Motion,
}

/// The shown gauge as a frame draws it: its look, maximum and border, and
/// its value (0 for its empty frame).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Shown {
    pub(crate) look: GaugeLook,
    pub(crate) max: GaugeValue,
    pub(crate) border: GaugeValue,
    pub(crate) value: GaugeValue,
}

impl Shown {
    /// A play's shown gauge: the snapshot's value under its descriptor;
    /// `None` when either lacks the shown index.
    pub(crate) fn of(
        gauges: &[GaugeState],
        descs: Option<&GaugeDesc>,
        shown: usize,
    ) -> Option<Self> {
        let (g, d) = (gauges.get(shown)?, descs?);
        Some(Self {
            look: d.look,
            max: d.max,
            border: d.border,
            value: g.value,
        })
    }
}

/// What a gauge draws from this frame.
pub(crate) struct GaugeDraw<'a> {
    pub(crate) at: Frame<'a>,
    /// The shown gauge; `None` draws nothing.
    pub(crate) shown: Option<Shown>,
    /// µs since the element's timer started.
    pub(crate) t: i64,
}

impl Gauge {
    pub(crate) fn new(def: &GaugeDef, image: impl Fn(u32) -> ImageRef) -> Self {
        Self {
            segments: def.segments,
            looks: def.looks.map(|p| Look {
                front: p.front.map(&image),
                back: p.back.map(&image),
                tip: p.tip.map(&image),
            }),
            motion: def.motion,
        }
    }

    /// Draws the shown gauge over `rect` through `brush`; nothing without
    /// one. A segment whose
    /// frame is missing is skipped; a refused push stops the gauge.
    pub(crate) fn draw(
        &self,
        d: &GaugeDraw<'_>,
        rect: [f32; 4],
        brush: &Brush,
        out: &mut QuadBatch,
    ) {
        let Some(desc) = d.shown else {
            return;
        };
        let Some(look) = self.looks.get(desc.look.index()) else {
            return;
        };
        let p = self.segments;
        let n = lit(desc.value, desc.max, p);
        let a = self.motion.offset(d.t);
        let flicker = match self.motion {
            Motion::Flicker { period } => Some(period.0.max(1)),
            _ => None,
        };
        let [x, y, w, h] = rect;
        let (x, w) = (f64::from(x), f64::from(w));
        let pf = f64::from(p);
        // The look's six frames at `now`, resolved once for every segment.
        let resolve = |images: &[ImageRef; 2]| {
            images.map(|i| (i.frame(d.at.starts, d.at.now, d.at.uvs), i.filter))
        };
        let (front, back, tip) = (
            resolve(&look.front),
            resolve(&look.back),
            resolve(&look.tip),
        );
        let (m, border) = (i128::from(desc.max.0), i128::from(desc.border.0));
        for i in 1..=p {
            let side = usize::from(i128::from(i) * m < border * i128::from(p));
            let part = if flicker.is_some() {
                if i <= n { &front } else { &back }
            } else if i == n {
                &tip
            } else if i64::from(i) + a < i64::from(n) {
                &front
            } else {
                &back
            };
            let cell = [f32_of(x + w * f64::from(i - 1) / pf), y, f32_of(w / pf), h];
            let Some(&(frame, filter)) = part.get(side) else {
                continue;
            };
            if let Some(f) = frame
                && !brush.push(out, cell, f, filter)
            {
                return;
            }
            if let Some(period) = flicker
                && i == n
                && let Some(&(Some(f), tip_filter)) = tip.get(side)
            {
                #[expect(clippy::cast_precision_loss, reason = "below 2^35, exact in f64")]
                let (phase, period) = ((d.t.max(0) % period) as f64, period as f64);
                let k = 1.0 - (2.0 * phase - period).abs() / period;
                let [r, gg, b, alpha] = brush.color;
                let tip_brush = Brush {
                    color: [r, gg, b, alpha * f32_of(k)],
                    ..*brush
                };
                if !tip_brush.push(out, cell, f, tip_filter) {
                    return;
                }
            }
        }
    }
}
