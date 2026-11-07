//! A result skin's `judgegraph` (`skin/json/JsonSkin.java:263-271`,
//! `skin/json/JsonSkinObjectLoader.java:475-481`,
//! `skin/SkinNoteDistributionGraph.java` at `0.8.9`): types 1 (by
//! judgment) and 2 (early and late) become the model's judge graph,
//! drawn on a generated chip grid and a white image; type 0 (the note
//! distribution) and any other type are dropped with a note.
//!
//! Its destination keeps its track, timer, visibility, blend, colour,
//! alpha and angle; its stretch fits the whole graph as the source fits
//! its texture ([`fit`]). A `filter` is noted and ignored: the graph's
//! quads are solid and its chips' gaps whole texels, so there is nothing
//! to soften. A flag field is on only at 1, as the source reads it.

use bms_model::TimeUs;
use skin_model::{Filter, GraphFit, JudgeGraphDef, JudgeGraphKind, Source};

use super::super::policy::Role;
use super::super::report::{NoteKind, Span};
use super::super::{Cx, Dst, Opts};
use crate::error::ImportError;

/// The model fit of stretch mode `mode` (`skin/StretchType.java`): the
/// source's unknown modes and -1 stretch to the rect.
pub(crate) fn fit(mode: i32) -> GraphFit {
    match mode {
        1 => GraphFit::Inner,
        2 | 3 => GraphFit::Outer { trim: mode == 3 },
        4 | 5 => GraphFit::Width { trim: mode == 5 },
        6 | 7 => GraphFit::Height { trim: mode == 7 },
        8 => GraphFit::NoLarger,
        9 | 10 => GraphFit::Exact { trim: mode == 10 },
        _ => GraphFit::Stretch,
    }
}

/// The lit texels of each 5 × 5 chip cell across and down: 4, or 5 on an
/// axis without a gap.
fn lit(no_gap_x: bool, no_gap_y: bool) -> [u8; 2] {
    [4 + u8::from(no_gap_x), 4 + u8::from(no_gap_y)]
}

impl Cx<'_> {
    /// The judge graph record `rec` at `dst`.
    pub(in super::super) fn judge_graph(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let role = Role::Decorative;
        let [ty, back_off, delay, reverse, no_gap, no_gap_x] = self.def.fields(
            rec,
            [
                "type",
                "backTexOff",
                "delay",
                "orderReverse",
                "noGap",
                "noGapX",
            ],
        );
        let kind = match self.int_or(ty, "type", 0)? {
            1 => JudgeGraphKind::Judgment,
            2 => JudgeGraphKind::EarlyLate,
            _ => return self.drop_el(role, at, NoteKind::UnsupportedField { field: "type" }),
        };
        let back = self.int_or(back_off, "backTexOff", 0)? != 1;
        let delay = TimeUs(i64::from(self.int_or(delay, "delay", 500)?) * 1_000);
        let reverse = self.int_or(reverse, "orderReverse", 0)? == 1;
        let no_gap = self.int_or(no_gap, "noGap", 0)? == 1;
        let no_gap_x = self.int_or(no_gap_x, "noGapX", 0)? == 1;
        let Some(c) = self.common_with(role, &dst, Opts::default())? else {
            return Ok(());
        };
        let fit = fit(dst.stretch);
        let mut def = JudgeGraphDef {
            kind,
            delay,
            reverse,
            back,
            chips: 0,
            solid: 0,
            fit,
        };
        // Its quads at worst, before its images are added; spent if it is
        // later refused.
        self.budget
            .widget_quads(def.quads())
            .map_err(|l| self.limit(at, l))?;
        if dst.filter != 0 {
            self.ignored(at, NoteKind::Ignored { field: "filter" });
        }
        def.chips = self
            .assets
            .chip_grid(&mut self.budget, lit(no_gap_x, no_gap))
            .map_err(|l| self.limit(at, l))?;
        def.solid = self.white(at)?;
        let mut rects = Vec::with_capacity(c.track.keys.len());
        for k in &c.track.keys {
            let r = self.key_rect(&c, k)?;
            rects.push(r.model().ok_or_else(|| self.range(at, "dst"))?);
        }
        let t = self.model_track(&c, &rects)?;
        let index = u32::try_from(self.judge_graphs.len()).unwrap_or(u32::MAX);
        self.judge_graphs.push(def);
        self.push(c, Source::JudgeGraph(index), t)
    }

    /// The opaque white system image, drawn nearest, charged and added
    /// once; a limit is located at `at`.
    fn white(&mut self, at: Span) -> Result<u32, ImportError> {
        let white = [255; 4];
        if let Some(i) = self.assets.system(white, Filter::Nearest) {
            return Ok(i);
        }
        self.budget.image(1).map_err(|l| self.limit(at, l))?;
        self.assets
            .add_system(white, Filter::Nearest)
            .map_err(|l| self.limit(at, l))
    }
}

#[cfg(test)]
mod tests {
    use skin_model::GraphFit;

    use super::{fit, lit};

    /// Every stretch mode maps to its fit; -1, 0 and unknown modes
    /// stretch.
    #[test]
    fn stretch_modes_map_to_fits() {
        let got: Vec<GraphFit> = (-1..=11).map(fit).collect();
        assert_eq!(
            got,
            [
                GraphFit::Stretch,
                GraphFit::Stretch,
                GraphFit::Inner,
                GraphFit::Outer { trim: false },
                GraphFit::Outer { trim: true },
                GraphFit::Width { trim: false },
                GraphFit::Width { trim: true },
                GraphFit::Height { trim: false },
                GraphFit::Height { trim: true },
                GraphFit::NoLarger,
                GraphFit::Exact { trim: false },
                GraphFit::Exact { trim: true },
                GraphFit::Stretch,
            ]
        );
        assert_eq!(lit(false, false), [4, 4]);
        assert_eq!(lit(true, false), [5, 4]);
        assert_eq!(lit(false, true), [4, 5]);
    }
}
