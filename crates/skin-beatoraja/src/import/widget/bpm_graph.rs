//! A play skin's `bpmgraph` (`skin/json/JsonSkin.java:273-283`,
//! `skin/json/JsonSkinObjectLoader.java:482-486`,
//! `skin/SkinBPMGraph.java:56-83` at `0.8.9`), as the model's BPM graph
//! drawn on a white system image.
//!
//! As the source reads them: `delay` in ms, not positive showing the graph
//! at once; `lineWidth` not positive is 2. A `lineWidth` past
//! [`MAX_BPM_LINE_WIDTH`] drops the graph with a note: the source draws
//! it, but its `int` stroke arithmetic wraps for the largest widths and the
//! model bounds its preparation by this width. A colour keeps its hex
//! digits, wherever they are, up to the first six, always opaque
//! (`normalizeColorString`, `:230-233`); none left keeps the default. One
//! to five left, or a JSON `null`, fails the source's skin; here the
//! default is kept with a note.
//!
//! Its destination keeps its track, timer, visibility, blend, colour,
//! alpha and angle. A stretch other than the default drops the graph with
//! a note: the source draws its first frame's texture through the stretch
//! with a negative height (`:105-107`), which the model does not
//! reproduce. A filter changes nothing, as the strokes are solid.

use bms_model::TimeUs;
use skin_model::{BpmColors, BpmGraphDef, MAX_BPM_LINE_WIDTH, Rgba8, Source, bpm_columns};

use super::super::policy::Role;
use super::super::report::{NoteKind, Severity};
use super::super::{Cx, Dst};
use crate::error::ImportError;

/// The colour fields, in [`colors`]'s order: main, min, max, other, stop,
/// transition.
const NAMES: [&str; 6] = [
    "mainBPMColor",
    "minBPMColor",
    "maxBPMColor",
    "otherBPMColor",
    "stopLineColor",
    "transitionLineColor",
];

/// What a BPM graph colour string gives: `Ok(None)` keeps the default (no
/// hex digit), `Err(())` is the source's failure (one to five digits).
pub(crate) fn colour(s: &str) -> Result<Option<Rgba8>, ()> {
    let hex: Vec<u8> = s
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(6)
        .filter_map(|c| c.to_digit(16))
        .filter_map(|d| u8::try_from(d).ok())
        .collect();
    match hex.as_slice() {
        [] => Ok(None),
        &[r1, r0, g1, g0, b1, b0] => Ok(Some(Rgba8([
            r1 << 4 | r0,
            g1 << 4 | g0,
            b1 << 4 | b0,
            0xff,
        ]))),
        _ => Err(()),
    }
}

/// The colours in `NAMES`' order as the model's.
fn colors([main, min, max, other, stop, transition]: [Rgba8; 6]) -> BpmColors {
    BpmColors {
        main,
        min,
        max,
        other,
        stop,
        transition,
    }
}

impl Cx<'_> {
    /// The BPM graph record `rec` at `dst`.
    pub(in super::super) fn bpm_graph(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let role = Role::Decorative.or_failure(dst.failure);
        let [delay, line_width] = self.def.fields(rec, ["delay", "lineWidth"]);
        let delay = TimeUs(i64::from(self.int_or(delay, "delay", 0)?.max(0)) * 1_000);
        let line_width = self.int_or(line_width, "lineWidth", 2)?;
        let line_width = if line_width > 0 { line_width } else { 2 };
        let Some(line_width) = u32::try_from(line_width)
            .ok()
            .filter(|&w| w <= MAX_BPM_LINE_WIDTH)
        else {
            return self.drop_el(role, at, NoteKind::UnsupportedField { field: "lineWidth" });
        };
        if !matches!(dst.stretch, -1 | 0) {
            return self.drop_el(role, at, NoteKind::Stretch(dst.stretch));
        }
        let d = BpmColors::DEFAULT;
        let mut c = [d.main, d.min, d.max, d.other, d.stop, d.transition];
        let fields = self.def.fields(rec, NAMES);
        for ((slot, field), name) in c.iter_mut().zip(fields).zip(NAMES) {
            let Some(s) = self.opt_string(field, name)? else {
                // An explicit JSON `null` fails the source's skin.
                if field.is_some() {
                    self.note(
                        Some(at),
                        Severity::Fallback,
                        NoteKind::InvalidColour { field: name },
                    );
                }
                continue;
            };
            match colour(&s) {
                Ok(Some(v)) => *slot = v,
                Ok(None) => {}
                Err(()) => self.note(
                    Some(at),
                    Severity::Fallback,
                    NoteKind::InvalidColour { field: name },
                ),
            }
        }
        let Some(common) = self.common_with(role, &dst, super::judge_graph::follows())? else {
            return Ok(());
        };
        let t = self.graph_track(&common)?;
        // Its quads at worst: its first key's columns, as the model sizes
        // its pixmap.
        let first = match common.track.keys.first() {
            Some(k) => self.key_rect(&common, k)?.model(),
            None => None,
        };
        let cols = first.map_or(0, |[_, _, w, _]| bpm_columns(w));
        self.budget
            .widget_quads(BpmGraphDef::quads(cols))
            .map_err(|l| self.limit(at, l))?;
        let def = BpmGraphDef {
            delay,
            line_width,
            colors: colors(c),
            solid: self.graph_white(at)?,
        };
        let index = u32::try_from(self.bpm_graphs.len()).unwrap_or(u32::MAX);
        self.bpm_graphs.push(def);
        self.push(common, Source::BpmGraph(index), t)
    }
}

#[cfg(test)]
mod tests {
    use skin_model::Rgba8;

    use super::colour;

    /// Every hex digit counts wherever it is, up to the first six, always
    /// opaque; none keeps the default; one to five is the source's
    /// failure.
    #[test]
    fn a_colour_keeps_its_first_six_hex_digits() {
        let rgb = |v: u32| Ok(Some(Rgba8::rgb(v, 0xff)));
        assert_eq!(colour("00ff00"), rgb(0x00_ff00));
        assert_eq!(colour("#12ab34"), rgb(0x12_ab34));
        assert_eq!(colour("ff000080"), rgb(0xff_0000));
        assert_eq!(colour("0x00ff00"), rgb(0x00_0ff0));
        assert_eq!(colour("12 34 56 78"), rgb(0x12_3456));
        assert_eq!(colour(""), Ok(None));
        assert_eq!(colour("#"), Ok(None));
        assert_eq!(colour("xyz"), Ok(None));
        assert_eq!(colour("ff00"), Err(()));
        assert_eq!(colour("#12345"), Err(()));
    }
}
