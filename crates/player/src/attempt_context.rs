//! The attempt a play's screen prepares, as a skin takes it (design.md
//! "Play Lifecycle"): the game's [`Attempt`] translated into owned skin
//! data, sent to the render thread in its message, from which a borrowed
//! [`PlayContext`] is made only for the call that prepares the screen,
//! with the chart's labels as [`ChartText`].

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;
use game::{Attempt, AttemptKind, ChartLabels};
use skin_model::{ChartText, GaugeDesc, PlayContext, Side};

/// The scratch side the simple skin draws, its author's layout: it has no
/// imported side and no player layout.
pub const SIMPLE_SCRATCH: Side = Side::Left;

/// One attempt's skin data, owned, made afresh for every play.
#[derive(Clone, Debug)]
pub struct AttemptContext {
    /// The play preset's gauges, in its order.
    gauges: Vec<GaugeDesc>,
    /// The whole chart or a section.
    kind: AttemptKind,
    empty_poor_breaks_combo: bool,
    empty_poor_counted: bool,
    scratch: Side,
    ln_default: rules::LnMode,
    last_note_end: TimeUs,
    /// The chart's labels, which its prepared text shows.
    labels: ChartLabels,
}

impl AttemptContext {
    /// `attempt` as a skin takes it, with the lanes laid out for `scratch`.
    pub fn new(attempt: Attempt, scratch: Side) -> Self {
        Self {
            gauges: attempt.gauges.iter().map(GaugeDesc::from_rule).collect(),
            kind: attempt.kind,
            empty_poor_breaks_combo: attempt.empty_poor_breaks_combo,
            empty_poor_counted: attempt.empty_poor_counted,
            scratch,
            ln_default: attempt.ln_default,
            last_note_end: attempt.last_note_end,
            labels: attempt.labels,
        }
    }

    /// The chart's five labels, borrowed for one preparation.
    pub fn text(&self) -> ChartText<'_> {
        let l = &self.labels;
        ChartText {
            title: &l.title,
            subtitle: &l.subtitle,
            artist: &l.artist,
            subartist: &l.subartist,
            genre: &l.genre,
        }
    }

    /// The borrowed context a skin prepares with, for one call.
    pub fn context(&self) -> PlayContext<'_> {
        PlayContext {
            gauges: &self.gauges,
            attempt: match &self.kind {
                AttemptKind::Full { lead_in } => skin_model::Attempt::Full { lead_in: *lead_in },
                AttemptKind::Section(section) => skin_model::Attempt::Section(section),
            },
            empty_poor_breaks_combo: self.empty_poor_breaks_combo,
            empty_poor_counted: self.empty_poor_counted,
            scratch: self.scratch,
            ln_default: self.ln_default,
            last_note_end: self.last_note_end,
            // No authoritative BPM range is known yet: `None` hides its
            // readouts rather than inferring them from the scroll.
            bpm_range: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;
    use game::{Attempt, ChartLabels};
    use skin_model::{GaugeDesc, Side};

    use super::AttemptContext;

    /// The context carries the preset's gauges in its order, its flags and
    /// LN default, the section and the note end as the game made them,
    /// the scratch side given, and no BPM range; and the chart's five
    /// labels.
    #[test]
    fn the_context_is_the_attempts_own_data() {
        let mut preset = rules::tapestry();
        preset.gauges.reverse();
        preset.empty_poor.counted = !preset.empty_poor.counted;
        let section = eval::Section {
            start: TimeUs(0),
            end: TimeUs(2_000_000),
            lead_in: TimeUs(1_500_000),
            initial_gauges: Vec::new(),
        };
        let attempt = Attempt::new(
            2,
            &preset,
            TimeUs(2_000_000),
            Some(&section),
            TimeUs(1_000_000),
            &ChartLabels {
                title: "t".to_owned(),
                subtitle: "s".to_owned(),
                artist: "a".to_owned(),
                subartist: "sa".to_owned(),
                genre: "g".to_owned(),
            },
        );
        let owned = AttemptContext::new(attempt, Side::Right);
        let ctx = owned.context();
        let expected: Vec<GaugeDesc> = preset.gauges.iter().map(GaugeDesc::from_rule).collect();
        assert_eq!(ctx.gauges, expected.as_slice());
        assert!(matches!(ctx.attempt, skin_model::Attempt::Section(s) if *s == section));
        assert_eq!(ctx.empty_poor_counted, preset.empty_poor.counted);
        assert_eq!(ctx.empty_poor_breaks_combo, preset.empty_poor.breaks_combo);
        assert_eq!(ctx.ln_default, preset.ln_mode.default);
        assert_eq!(ctx.scratch, Side::Right);
        assert_eq!(ctx.last_note_end, TimeUs(1_000_000));
        assert_eq!(ctx.bpm_range, None);
        let text = owned.text();
        assert_eq!(
            [
                text.title,
                text.subtitle,
                text.artist,
                text.subartist,
                text.genre
            ],
            ["t", "s", "a", "sa", "g"]
        );
    }
}
