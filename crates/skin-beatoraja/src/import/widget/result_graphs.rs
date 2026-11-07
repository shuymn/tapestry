//! A result skin's `gaugegraph` and `timingdistributiongraph`
//! (`skin/json/JsonSkin.java:244-261`, `:323-337`,
//! `skin/json/JsonSkinObjectLoader.java:457-472`, `:500-505`,
//! `result/SkinGaugeGraphObject.java:63-111`,
//! `skin/SkinTimingDistributionGraph.java:36-56` at `0.8.9`), as the
//! model's gauge graph and timing distribution, each drawn on a white
//! system image.
//!
//! Its destination keeps its track, timer, visibility, blend, colour,
//! alpha and angle. A distribution's stretch fits the whole graph as the
//! source fits its texture of `gx` × scale texels; a gauge graph's texture
//! is its own rect (`SkinGaugeGraphObject.java:155-169`), which no
//! stretch changes. A `filter` is noted and ignored: the graphs' quads are
//! solid, with no texel to soften.
//!
//! A colour is an optional `#`, then exactly 6 or 8 hex digits
//! (`rrggbb`, alpha ff, or `rrggbbaa`). Anything else draws opaque red, the
//! distribution's own error colour (`SkinTimingVisualizer.java:185-191`),
//! with a note; a bad gauge graph colour fails the source's skin instead.

use skin_model::{GaugeGraphColors, GaugeGraphDef, GaugeLook, Rgba8, Source, TimingGraphDef};

use super::super::policy::Role;
use super::super::report::{NoteKind, Severity, Span};
use super::super::{Cx, Dst, Opts};
use super::judge_graph::fit;
use crate::error::ImportError;

/// The most columns a distribution draws (owner decision H4): past it the
/// graph drops with a note.
pub(crate) const MAX_TIMING_COLUMNS: i32 = 4_096;

/// The source's error colour: opaque red.
const RED: Rgba8 = Rgba8([0xff, 0, 0, 0xff]);

/// `s` as a colour: an optional `#`, then exactly 6 or 8 hex digits;
/// `None` for anything else.
pub(crate) fn colour(s: &str) -> Option<Rgba8> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| {
        hex.get(i..i + 2)
            .and_then(|h| u8::from_str_radix(h, 16).ok())
    };
    let a = if hex.len() == 8 { byte(6)? } else { 0xff };
    Some(Rgba8([byte(0)?, byte(2)?, byte(4)?, a]))
}

/// The gauge graph's named colours (`JsonSkin.java:247-260`): each look's
/// back, then each look's line, then the border line and the border back.
const GAUGE_NAMES: [(&str, &str); 14] = [
    ("assistClearBGColor", "440044"),
    ("assistAndEasyFailBGColor", "004444"),
    ("grooveFailBGColor", "004400"),
    ("grooveClearAndHardBGColor", "440000"),
    ("exHardBGColor", "444400"),
    ("hazardBGColor", "444444"),
    ("assistClearLineColor", "ff00ff"),
    ("assistAndEasyFailLineColor", "00ffff"),
    ("grooveFailLineColor", "00ff00"),
    ("grooveClearAndHardLineColor", "ff0000"),
    ("exHardLineColor", "ffff00"),
    ("hazardLineColor", "cccccc"),
    ("borderlineColor", "ff0000"),
    ("borderColor", "440000"),
];

/// The distribution's colours (`JsonSkin.java:327-334`): the bars, the
/// average and ±σ lines, then the areas PERFECT to POOR, as
/// [`TimingGraphDef::colors`] orders them.
const TIMING_NAMES: [(&str, &str); 8] = [
    ("graphColor", "00FF00FF"),
    ("averageColor", "FFFFFFFF"),
    ("devColor", "FFFFFFFF"),
    ("PGColor", "000088FF"),
    ("GRColor", "008800FF"),
    ("GDColor", "888800FF"),
    ("BDColor", "880000FF"),
    ("PRColor", "000000FF"),
];

/// The looks' colours from the 14 named ones as the source assigns them
/// (`SkinGaugeGraphObject.java:91-111`): the first three looks share the
/// border line and border back; the last three draw their own line and
/// back on both sides of the border.
fn named_gauge(c: &[Rgba8; 14]) -> [GaugeGraphColors; GaugeLook::COUNT] {
    let [
        b0,
        b1,
        b2,
        b3,
        b4,
        b5,
        l0,
        l1,
        l2,
        l3,
        l4,
        l5,
        border_line,
        border_back,
    ] = *c;
    let lower = |back, line| GaugeGraphColors {
        border_line,
        border_back,
        line,
        back,
    };
    let upper = |back, line| GaugeGraphColors {
        border_line: line,
        border_back: back,
        line,
        back,
    };
    [
        lower(b0, l0),
        lower(b1, l1),
        lower(b2, l2),
        upper(b3, l3),
        upper(b4, l4),
        upper(b5, l5),
    ]
}

impl Cx<'_> {
    /// One colour of record field `name`, `default` when absent; red with
    /// a note when it does not parse.
    fn graph_colour(
        &mut self,
        at: Span,
        field: Option<u32>,
        name: &'static str,
        default: &str,
    ) -> Result<Rgba8, ImportError> {
        let s = self.opt_string(field, name)?;
        Ok(self.parsed(at, s.as_deref().unwrap_or(default), name))
    }

    /// `s` as a colour, red with a note on `name` when it does not parse.
    fn parsed(&mut self, at: Span, s: &str, name: &'static str) -> Rgba8 {
        colour(s).unwrap_or_else(|| {
            self.note(
                Some(at),
                Severity::Fallback,
                NoteKind::InvalidColour { field: name },
            );
            RED
        })
    }

    /// The gauge graph record `rec` at `dst`: the 24-entry `color` array
    /// when present (a missing entry `000000`), else the 14 named colours.
    pub(in super::super) fn gauge_graph(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let role = Role::Decorative;
        let [array] = self.def.fields(rec, ["color"]);
        let colors = if let Some(array) = array {
            let entries = self.opt_strings(Some(array), "color")?;
            let mut looks = [[RED; 4]; GaugeLook::COUNT];
            for (i, slot) in looks.iter_mut().flatten().enumerate() {
                // Past the array, the source's black; a null entry, red.
                let s = entries.get(i).map_or(Some("000000"), Option::as_deref);
                *slot = self.parsed(at, s.unwrap_or(""), "color");
            }
            looks.map(|[border_line, border_back, line, back]| GaugeGraphColors {
                border_line,
                border_back,
                line,
                back,
            })
        } else {
            let fields = self.def.fields(rec, GAUGE_NAMES.map(|(n, _)| n));
            let mut c = [RED; 14];
            for ((slot, field), (name, default)) in c.iter_mut().zip(fields).zip(GAUGE_NAMES) {
                *slot = self.graph_colour(at, field, name, default)?;
            }
            named_gauge(&c)
        };
        let Some(c) = self.common_with(role, &dst, Opts::default())? else {
            return Ok(());
        };
        let mut def = GaugeGraphDef { colors, solid: 0 };
        self.budget
            .widget_quads(def.quads())
            .map_err(|l| self.limit(at, l))?;
        if dst.filter != 0 {
            self.ignored(at, NoteKind::Ignored { field: "filter" });
        }
        def.solid = self.graph_white(at)?;
        let t = self.graph_track(&c)?;
        let index = u32::try_from(self.gauge_graphs.len()).unwrap_or(u32::MAX);
        self.gauge_graphs.push(def);
        self.push(c, Source::GaugeGraph(index), t)
    }

    /// The timing distribution record `rec` at `dst`: `width / lineWidth`
    /// columns (`lineWidth` clamped to `1..=width`, `width` at least 1),
    /// dropped with a note past [`MAX_TIMING_COLUMNS`].
    pub(in super::super) fn timing_graph(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let role = Role::Decorative;
        let [width, line_width, average, dev] = self
            .def
            .fields(rec, ["width", "lineWidth", "drawAverage", "drawDev"]);
        let w = self.int_or(width, "width", 301)?.max(1);
        let lw = self.int_or(line_width, "lineWidth", 1)?.clamp(1, w);
        let gx = w / lw;
        if gx > MAX_TIMING_COLUMNS {
            return self.drop_el(role, at, NoteKind::UnsupportedField { field: "width" });
        }
        let average = self.int_or(average, "drawAverage", 1)? == 1;
        let dev = self.int_or(dev, "drawDev", 1)? == 1;
        let fields = self.def.fields(rec, TIMING_NAMES.map(|(n, _)| n));
        let mut colors = [RED; 8];
        for ((slot, field), (name, default)) in colors.iter_mut().zip(fields).zip(TIMING_NAMES) {
            *slot = self.graph_colour(at, field, name, default)?;
        }
        let Some(c) = self.common_with(role, &dst, Opts::default())? else {
            return Ok(());
        };
        let mut def = TimingGraphDef {
            gx: u16::try_from(gx).map_err(|_| self.range(at, "width"))?,
            colors,
            average,
            dev,
            solid: 0,
            fit: fit(dst.stretch),
        };
        self.budget
            .widget_quads(def.quads())
            .map_err(|l| self.limit(at, l))?;
        if dst.filter != 0 {
            self.ignored(at, NoteKind::Ignored { field: "filter" });
        }
        def.solid = self.graph_white(at)?;
        let t = self.graph_track(&c)?;
        let index = u32::try_from(self.timing_graphs.len()).unwrap_or(u32::MAX);
        self.timing_graphs.push(def);
        self.push(c, Source::TimingGraph(index), t)
    }
}

#[cfg(test)]
mod tests {
    use skin_model::Rgba8;

    use super::colour;

    /// An optional `#`, then exactly 6 or 8 hex digits; anything else is
    /// not a colour.
    #[test]
    fn the_colour_grammar_is_plain_hex() {
        assert_eq!(colour("ff00ff"), Some(Rgba8([0xff, 0, 0xff, 0xff])));
        assert_eq!(colour("#ff00ff80"), Some(Rgba8([0xff, 0, 0xff, 0x80])));
        assert_eq!(colour("00FF00FF"), Some(Rgba8([0, 0xff, 0, 0xff])));
        for bad in [
            "ff00",
            "ff00ff8",
            "+10000",
            "-10000",
            "zzzzzz",
            "ff00ffaabb",
            "",
            "#",
            "##ff00ff",
        ] {
            assert_eq!(colour(bad), None, "{bad}");
        }
    }
}
