//! A play's attempt as the screen prepares it (design.md "Play Lifecycle"):
//! what the attempt is fixed with, read from the chart and the play preset
//! once the preset has been refreshed for it, and how its preparation
//! ended. Everything here is ordinary game data: the player translates it
//! into whatever a skin takes, so the game depends on no skin crate.

use bms_model::{Chart, NoteKind, TimeUs};
use eval::Section;
use rules::{GaugeRule, LnMode, RulePreset};

use crate::prepare::ChartLabels;

/// How a play's preparation on the screen ended ([`crate::Rig::prepare_attempt`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preparation {
    /// The attempt is prepared: recording may open.
    Ready,
    /// The player left while it was prepared: the quit key returns to
    /// where the chart was chosen, a close ends the run. Recording never
    /// opens.
    Cancelled,
    /// The screen cannot draw this attempt, and the device is still
    /// healthy: the attempt is refused with the reason, and the screen
    /// stays usable for the menus.
    Refused(PreparationRefusal),
}

/// Why the screen refused to prepare an attempt: an owned diagnostic, with
/// where it was found when that is known (a skin file and its position),
/// for selection, the practice page or the console.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{}", match .location { Some(at) => format!("{at}: {}", .reason), None => .reason.clone() })]
pub struct PreparationRefusal {
    /// Where the problem was found, if known.
    pub location: Option<String>,
    /// What it is.
    pub reason: String,
}

/// What an attempt plays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptKind {
    /// The whole chart, its countdown `lead_in` long.
    Full {
        /// From recording's opening to chart time 0.
        lead_in: TimeUs,
    },
    /// A practice round of this section, in the chart's real time.
    Section(Section),
}

/// What one attempt is fixed with, made afresh for every play, the first,
/// each retry and each practice round, after the play preset was
/// refreshed for it ([`crate::Loaded::attempt`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// The play's number in the session, from 1.
    pub number: u32,
    /// The play preset's name and version: the preset this attempt is
    /// evaluated under, whatever a result screen views.
    pub preset: (String, u32),
    /// The play preset's gauges, in its order.
    pub gauges: Vec<GaugeRule>,
    /// Whether an empty POOR breaks the combo.
    pub empty_poor_breaks_combo: bool,
    /// Whether an empty POOR is counted as a POOR.
    pub empty_poor_counted: bool,
    /// The long-note mode for a chart that declares none.
    pub ln_default: LnMode,
    /// The whole chart or a section.
    pub kind: AttemptKind,
    /// The end of the attempt's notes on its play clock: the latest end of
    /// a note it evaluates (a long note's at its end; mines and background
    /// sounds do not count), or its start when it has none. This is the
    /// endpoint a skin's `last_note` timer samples from, kept apart from
    /// any remaining-time readout.
    pub last_note_end: TimeUs,
    /// The chart's labels, from the ingestion that read the chart.
    pub labels: ChartLabels,
}

impl Attempt {
    /// The attempt numbered `number` of `chart` under `preset`: the whole
    /// chart with a `lead_in` countdown, or the round of `section`.
    #[must_use]
    pub fn new(
        number: u32,
        chart: &Chart,
        preset: &RulePreset,
        lead_in: TimeUs,
        section: Option<&Section>,
        labels: &ChartLabels,
    ) -> Self {
        let notes = chart
            .lanes
            .iter()
            .flatten()
            .filter(|n| !matches!(n.kind, NoteKind::Mine { .. }));
        let (kind, last_note_end) = match section {
            Some(s) => {
                let last = notes.filter(|n| s.is_target(n)).map(|n| n.end()).max();
                (AttemptKind::Section(s.clone()), last.unwrap_or(s.start))
            }
            None => {
                let last = notes.map(|n| n.end()).max();
                (AttemptKind::Full { lead_in }, last.unwrap_or(TimeUs(0)))
            }
        };
        Self {
            number,
            preset: (preset.id.clone(), preset.version),
            gauges: preset.gauges.clone(),
            empty_poor_breaks_combo: preset.empty_poor.breaks_combo,
            empty_poor_counted: preset.empty_poor.counted,
            ln_default: preset.ln_mode.default,
            kind,
            last_note_end,
            labels: labels.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use bms_model::{ChartBuilder, Lane, TimeUs};
    use eval::Section;

    use super::{Attempt, AttemptKind};
    use crate::prepare::ChartLabels;

    const MS: i64 = 1_000;

    /// The endpoint is the latest end of the attempt's own notes: a long
    /// note's end counts, a mine after it does not, and a round counts only
    /// its targets.
    #[test]
    fn the_last_note_end_is_the_attempts_latest_note_end() {
        let lane = |i| Lane::new(i).unwrap();
        let chart = ChartBuilder::new(150_000)
            .tap(lane(1), TimeUs(1_000 * MS))
            .long(lane(2), TimeUs(2_000 * MS), TimeUs(3_500 * MS))
            .tap(lane(3), TimeUs(5_000 * MS))
            .mine(lane(4), TimeUs(9_000 * MS), 1)
            .build()
            .unwrap();
        let preset = rules::tapestry();
        let labels = ChartLabels::default();
        let full = Attempt::new(1, &chart, &preset, TimeUs(1_000 * MS), None, &labels);
        assert_eq!(full.last_note_end, TimeUs(5_000 * MS));
        assert_eq!(
            full.kind,
            AttemptKind::Full {
                lead_in: TimeUs(1_000 * MS)
            }
        );
        let section = Section {
            start: TimeUs(500 * MS),
            end: TimeUs(4_000 * MS),
            lead_in: TimeUs(2_000 * MS),
            initial_gauges: preset.gauges.iter().map(|g| g.start).collect(),
        };
        let round = Attempt::new(
            2,
            &chart,
            &preset,
            TimeUs(1_000 * MS),
            Some(&section),
            &labels,
        );
        assert_eq!(round.last_note_end, TimeUs(3_500 * MS));
        // A section without targets ends where it starts.
        let empty = Section {
            start: TimeUs(6_000 * MS),
            end: TimeUs(8_000 * MS),
            ..section
        };
        let none = Attempt::new(
            3,
            &chart,
            &preset,
            TimeUs(1_000 * MS),
            Some(&empty),
            &labels,
        );
        assert_eq!(none.last_note_end, TimeUs(6_000 * MS));
    }
}
