//! A play skin's `timingvisualizer` and `hiterrorvisualizer`
//! (`skin/json/JsonSkin.java:285-321`,
//! `skin/json/JsonSkinObjectLoader.java:488-499`,
//! `skin/SkinTimingVisualizer.java:56-74`,
//! `skin/SkinHitErrorVisualizer.java:56-82` at `0.8.9`), as the model's
//! visualizers drawn on a white system image.
//!
//! As the source reads them: `lineWidth` clamped to 1 to 4, `windowLength`
//! to 1 to 100; a flag on only at 1; `emaMode` 1 a line, 2 a triangle, 3
//! both and anything else none (the source keeps an average it never
//! draws). A `judgeWidthMillis` outside 1 to
//! [`MAX_JUDGE_WIDTH`], a `width` outside 1 to [`MAX_COORD`] (the timing
//! visualizer's) or 1 to [`MAX_HIT_ERROR_WIDTH`] texels (the hit-error
//! visualizer's pixmap) drops the visualizer with a note; the source takes
//! them, drawing every line at the centre or its edges at 0 ms or below,
//! and failing on a pixmap it cannot make. An `alpha` that is not finite
//! becomes the finite weight that steps the average alike ([`finite`]).
//!
//! A colour is the source's `colorStringValidation`: hex digits only, at
//! least six, else opaque red; then `Color.valueOf`: the first six digits,
//! and an alpha only from exactly eight. The empty POOR band's colour is
//! read without that check (`Color.valueOf` alone: an optional `#`, each
//! pair a signed hex number clamped to 0 to 255), and is clear with
//! `transparent` 1. A POOR colour `Color.valueOf` refuses fails the
//! source's skin; here it is red with a note, as a checked colour that
//! turns red is.
//!
//! Its destination keeps its track, timer, visibility, blend, colour,
//! alpha and angle. A stretch other than the default and a `filter` are
//! noted and ignored: the model places the band, lines and pixmap in the
//! destination's rect and draws them solid.

use skin_model::{
    EmaMode, HitErrorDef, MAX_COORD, MAX_HIT_ERROR_WIDTH, MAX_HIT_ERROR_WINDOW, MAX_JUDGE_WIDTH,
    Rgba8, Source, TimingVisualizerDef,
};

use super::super::policy::Role;
use super::super::report::{NoteKind, Severity, Span};
use super::super::{Cx, Dst, Opts};
use crate::error::ImportError;

/// The source's error colour: opaque red.
const RED: Rgba8 = Rgba8([0xff, 0, 0, 0xff]);
/// `Color.CLEAR`.
const CLEAR: Rgba8 = Rgba8([0; 4]);

/// The timing visualizer's checked colours (`JsonSkin.java:312-318`): the
/// line, the centre, then PERFECT to BAD.
const TIMING_NAMES: [(&str, &str); 6] = [
    ("lineColor", "00FF00FF"),
    ("centerColor", "FFFFFFFF"),
    ("PGColor", "000088FF"),
    ("GRColor", "008800FF"),
    ("GDColor", "888800FF"),
    ("BDColor", "880000FF"),
];
/// The timing visualizer's POOR colour.
const TIMING_POOR: &str = "000000FF";

/// The hit-error visualizer's checked colours (`JsonSkin.java:293-300`):
/// the line, the centre, the average, then PERFECT to BAD.
const HIT_NAMES: [(&str, &str); 7] = [
    ("lineColor", "99CCFF80"),
    ("centerColor", "FFFFFFFF"),
    ("emaColor", "FF0000FF"),
    ("PGColor", "99CCFF80"),
    ("GRColor", "F2CB3080"),
    ("GDColor", "14CC8f80"),
    ("BDColor", "FF1AB380"),
];
/// The hit-error visualizer's POOR colour.
const HIT_POOR: &str = "CC292980";

/// libGDX's `Color.valueOf(s)`: one leading `#` dropped, then the first
/// three pairs, each `Integer.valueOf(pair, 16)` clamped to 0 to 255, and
/// the fourth pair as alpha when exactly eight characters are left, else
/// opaque; `None` where it throws.
pub(crate) fn value_of(s: &str) -> Option<Rgba8> {
    let hex: Vec<char> = s.strip_prefix('#').unwrap_or(s).chars().collect();
    let pair = |i: usize| {
        let p: String = hex.get(i..i + 2)?.iter().collect();
        let v = i32::from_str_radix(&p, 16).ok()?;
        u8::try_from(v.clamp(0, 255)).ok()
    };
    let a = if hex.len() == 8 { pair(6)? } else { 0xff };
    Some(Rgba8([pair(0)?, pair(2)?, pair(4)?, a]))
}

/// The source's checked colour, `Color.valueOf(colorStringValidation(s))`:
/// `None` where the check gives red.
pub(crate) fn checked(s: &str) -> Option<Rgba8> {
    (s.len() >= 6 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| value_of(s))
        .flatten()
}

/// The EMA mode of `emaMode` (`SkinHitErrorVisualizer.java:165-187`).
fn ema_mode(mode: i32) -> EmaMode {
    match mode {
        1 => EmaMode::Line,
        2 => EmaMode::Triangle,
        3 => EmaMode::Both,
        _ => EmaMode::Off,
    }
}

/// A weight the model takes that steps the average as `alpha` steps the
/// source's (`ema += (long)(alpha · d)` over a whole `d`): NaN steps 0, as
/// 0 does; an infinity saturates every non-zero step, as the largest finite
/// weight of its sign does.
fn finite(alpha: f32) -> f32 {
    if alpha.is_nan() {
        0.0
    } else {
        alpha.clamp(f32::MIN, f32::MAX)
    }
}

/// `v` as a count within `range`, `None` outside it.
fn within(v: i32, range: std::ops::RangeInclusive<u32>) -> Option<u32> {
    u32::try_from(v).ok().filter(|v| range.contains(v))
}

impl Cx<'_> {
    /// The checked colours of `fields` named and defaulted by `names`; a
    /// colour that does not pass is red with a note.
    fn checked_colours<const N: usize>(
        &mut self,
        at: Span,
        fields: [Option<u32>; N],
        names: [(&'static str, &str); N],
    ) -> Result<[Rgba8; N], ImportError> {
        let mut out = [RED; N];
        for ((slot, field), (name, default)) in out.iter_mut().zip(fields).zip(names) {
            let s = self.opt_string(field, name)?;
            *slot = self.colour_or_red(at, checked(s.as_deref().unwrap_or(default)), name);
        }
        Ok(out)
    }

    /// The POOR band's colour: clear with `transparent` 1, else `field`'s
    /// (`default` when absent) read by `Color.valueOf` alone.
    fn poor_colour(
        &mut self,
        at: Span,
        transparent: bool,
        field: Option<u32>,
        default: &str,
    ) -> Result<Rgba8, ImportError> {
        if transparent {
            return Ok(CLEAR);
        }
        let s = self.opt_string(field, "PRColor")?;
        Ok(self.colour_or_red(at, value_of(s.as_deref().unwrap_or(default)), "PRColor"))
    }

    /// `c`, or red with a note on `name`.
    fn colour_or_red(&mut self, at: Span, c: Option<Rgba8>, name: &'static str) -> Rgba8 {
        c.unwrap_or_else(|| {
            self.note(
                Some(at),
                Severity::Fallback,
                NoteKind::InvalidColour { field: name },
            );
            RED
        })
    }

    /// Notes a stretch other than the default and a filter, which a
    /// visualizer ignores.
    fn visualizer_ignores(&mut self, dst: &Dst) {
        if !matches!(dst.stretch, -1 | 0) {
            self.ignored(dst.span, NoteKind::Ignored { field: "stretch" });
        }
        if dst.filter != 0 {
            self.ignored(dst.span, NoteKind::Ignored { field: "filter" });
        }
    }

    /// The timing visualizer record `rec` at `dst`.
    pub(in super::super) fn timing_visualizer(
        &mut self,
        dst: Dst,
        rec: u32,
    ) -> Result<(), ImportError> {
        let at = dst.span;
        let role = Role::Decorative.or_failure(dst.failure);
        let [width, judge_width, line_width, transparent, decay, poor] = self.def.fields(
            rec,
            [
                "width",
                "judgeWidthMillis",
                "lineWidth",
                "transparent",
                "drawDecay",
                "PRColor",
            ],
        );
        let width = self.int_or(width, "width", 301)?;
        let judge_width = self.int_or(judge_width, "judgeWidthMillis", 150)?;
        let line_width = self.int_or(line_width, "lineWidth", 1)?.clamp(1, 4);
        let transparent = self.int_or(transparent, "transparent", 0)? == 1;
        let decay = self.int_or(decay, "drawDecay", 1)? == 1;
        let Some(judge_width) = within(judge_width, 1..=MAX_JUDGE_WIDTH) else {
            return self.drop_el(
                role,
                at,
                NoteKind::UnsupportedField {
                    field: "judgeWidthMillis",
                },
            );
        };
        let Some(width) = within(width, 1..=MAX_COORD.unsigned_abs()) else {
            return self.drop_el(role, at, NoteKind::UnsupportedField { field: "width" });
        };
        let fields = self.def.fields(rec, TIMING_NAMES.map(|(n, _)| n));
        let [line, center, pg, gr, gd, bd] = self.checked_colours(at, fields, TIMING_NAMES)?;
        let pr = self.poor_colour(at, transparent, poor, TIMING_POOR)?;
        let Some(c) = self.common_with(role, &dst, Opts::default())? else {
            return Ok(());
        };
        let mut def = TimingVisualizerDef {
            judge_width,
            width,
            line_width: line_width.unsigned_abs(),
            line,
            center,
            bands: [pg, gr, gd, bd, pr],
            decay,
            solid: 0,
        };
        self.budget
            .widget_quads(def.quads())
            .map_err(|l| self.limit(at, l))?;
        self.visualizer_ignores(&dst);
        def.solid = self.graph_white(at)?;
        let t = self.graph_track(&c)?;
        let index = u32::try_from(self.timing_visualizers.len()).unwrap_or(u32::MAX);
        self.timing_visualizers.push(def);
        self.push(c, Source::TimingVisualizer(index), t)
    }

    /// The hit-error visualizer record `rec` at `dst`.
    pub(in super::super) fn hit_error(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let role = Role::Decorative.or_failure(dst.failure);
        let [
            width,
            judge_width,
            line_width,
            color_mode,
            lines,
            ema,
            alpha,
            window,
            transparent,
            decay,
            poor,
        ] = self.def.fields(
            rec,
            [
                "width",
                "judgeWidthMillis",
                "lineWidth",
                "colorMode",
                "hiterrorMode",
                "emaMode",
                "alpha",
                "windowLength",
                "transparent",
                "drawDecay",
                "PRColor",
            ],
        );
        let width = self.int_or(width, "width", 301)?;
        let judge_width = self.int_or(judge_width, "judgeWidthMillis", 150)?;
        let line_width = self.int_or(line_width, "lineWidth", 1)?.clamp(1, 4);
        let band_colors = self.int_or(color_mode, "colorMode", 1)? == 1;
        let lines = self.int_or(lines, "hiterrorMode", 1)? == 1;
        let ema_mode = ema_mode(self.int_or(ema, "emaMode", 1)?);
        let alpha = finite(match alpha {
            Some(f) => self.def.float(f, "alpha")?,
            None => 0.1,
        });
        let window = self.int_or(window, "windowLength", 30)?;
        let window = window.clamp(1, i32::try_from(MAX_HIT_ERROR_WINDOW).unwrap_or(1));
        let transparent = self.int_or(transparent, "transparent", 0)? == 1;
        let decay = self.int_or(decay, "drawDecay", 1)? == 1;
        let Some(judge_width) = within(judge_width, 1..=MAX_JUDGE_WIDTH) else {
            return self.drop_el(
                role,
                at,
                NoteKind::UnsupportedField {
                    field: "judgeWidthMillis",
                },
            );
        };
        let Some(width) = within(width, 1..=MAX_HIT_ERROR_WIDTH) else {
            return self.drop_el(role, at, NoteKind::UnsupportedField { field: "width" });
        };
        let fields = self.def.fields(rec, HIT_NAMES.map(|(n, _)| n));
        let [line, center, ema, pg, gr, gd, bd] = self.checked_colours(at, fields, HIT_NAMES)?;
        let pr = self.poor_colour(at, transparent, poor, HIT_POOR)?;
        let Some(c) = self.common_with(role, &dst, Opts::default())? else {
            return Ok(());
        };
        let mut def = HitErrorDef {
            width,
            judge_width,
            line_width: line_width.unsigned_abs(),
            window: window.unsigned_abs(),
            lines,
            band_colors,
            ema_mode,
            alpha,
            line,
            center,
            ema,
            bands: [pg, gr, gd, bd, pr],
            decay,
            solid: 0,
        };
        self.budget
            .widget_quads(def.quads())
            .map_err(|l| self.limit(at, l))?;
        self.visualizer_ignores(&dst);
        def.solid = self.graph_white(at)?;
        let t = self.graph_track(&c)?;
        let index = u32::try_from(self.hit_errors.len()).unwrap_or(u32::MAX);
        self.hit_errors.push(def);
        self.push(c, Source::HitErrorVisualizer(index), t)
    }
}

#[cfg(test)]
mod tests {
    use skin_model::{EmaMode, Rgba8};

    use super::{checked, ema_mode, finite, value_of};

    /// The checked grammar: hex digits only, at least six; the first six
    /// digits, alpha only from exactly eight.
    #[test]
    fn a_checked_colour_is_hex_of_six_or_more() {
        assert_eq!(checked("FF00FF80"), Some(Rgba8([0xff, 0, 0xff, 0x80])));
        assert_eq!(checked("ff00ff"), Some(Rgba8([0xff, 0, 0xff, 0xff])));
        assert_eq!(checked("FF00FF8"), Some(Rgba8([0xff, 0, 0xff, 0xff])));
        assert_eq!(checked("FF00FF80AA"), Some(Rgba8([0xff, 0, 0xff, 0xff])));
        for bad in ["#ff00ff", "ff00f", "", "ff00fg", "+10000", "ff 00ff"] {
            assert_eq!(checked(bad), None, "{bad}");
        }
    }

    /// `Color.valueOf` alone: an optional `#`, signed pairs clamped, alpha
    /// only from exactly eight; `None` where it throws.
    #[test]
    fn value_of_is_libgdx_s_reading() {
        assert_eq!(value_of("#00000080"), Some(Rgba8([0, 0, 0, 0x80])));
        assert_eq!(value_of("CC292980"), Some(Rgba8([0xcc, 0x29, 0x29, 0x80])));
        assert_eq!(value_of("+10000"), Some(Rgba8([1, 0, 0, 0xff])));
        assert_eq!(value_of("-10000"), Some(Rgba8([0, 0, 0, 0xff])));
        assert_eq!(value_of("1234567"), Some(Rgba8([0x12, 0x34, 0x56, 0xff])));
        for bad in ["", "#", "abc", "zz0000", "#12345"] {
            assert_eq!(value_of(bad), None, "{bad}");
        }
    }

    /// A weight that is not finite becomes one the model takes and that
    /// steps alike; a finite one is kept.
    #[test]
    fn a_weight_that_is_not_finite_steps_alike() {
        assert_eq!(finite(0.25).to_bits(), 0.25_f32.to_bits());
        assert_eq!(finite(f32::NAN).to_bits(), 0.0_f32.to_bits());
        assert_eq!(finite(f32::INFINITY).to_bits(), f32::MAX.to_bits());
        assert_eq!(finite(f32::NEG_INFINITY).to_bits(), f32::MIN.to_bits());
    }

    #[test]
    fn ema_modes_draw_only_from_one_to_three() {
        let got: Vec<EmaMode> = (-1..=4).map(ema_mode).collect();
        assert_eq!(
            got,
            [
                EmaMode::Off,
                EmaMode::Off,
                EmaMode::Line,
                EmaMode::Triangle,
                EmaMode::Both,
                EmaMode::Off,
            ]
        );
    }
}
