//! A play's attempt as the screen prepares it (design.md "Play Lifecycle"):
//! what the attempt is fixed with, read from the chart and the play preset
//! once the preset has been refreshed for it, and how its preparation
//! ended. Everything here is ordinary game data: the player translates it
//! into whatever a skin takes, so the game depends on no skin crate.

use bms_model::{Chart, Speed, TimeUs};
use eval::Section;
use rules::{GaugeRule, LnMode, RulePreset};

use crate::policy::GaugeIndex;
use crate::practice::mapped;
use crate::prepare::ChartLabels;
use crate::tempo::AttemptTempo;

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
pub struct PreparationRefusal {
    /// Where the problem was found, if known.
    pub location: Option<String>,
    /// What it is.
    pub reason: String,
}

impl std::fmt::Display for PreparationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(at) = &self.location {
            write!(f, "{at}: ")?;
        }
        f.write_str(&self.reason)
    }
}

/// What an attempt plays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptKind {
    /// The whole chart, its READY state `lead_in` long.
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
    /// The gauge shown, by its index in `gauges`, as the session policy
    /// resolved it once for the attempt ([`crate::Loaded::displayed`]):
    /// what the loading gauge, the play's frames, the report and what
    /// closes the play all use. [`Attempt::new`] gives the first;
    /// [`crate::Loaded::attempt`] sets the policy's.
    pub displayed: GaugeIndex,
    /// Whether an empty POOR breaks the combo.
    pub empty_poor_breaks_combo: bool,
    /// Whether an empty POOR is counted as a POOR.
    pub empty_poor_counted: bool,
    /// The long-note mode for a chart that declares none.
    pub ln_default: LnMode,
    /// The whole chart or a section.
    pub kind: AttemptKind,
    /// The end of the attempt's notes on its play clock ([`NoteEnd`]):
    /// where a skin's `last_note` timer starts, kept apart from any
    /// remaining-time readout.
    pub last_note_end: TimeUs,
    /// The chart's labels, from the ingestion that read the chart.
    pub labels: ChartLabels,
    /// The tempo provider and reference tempos, from the chart as loaded
    /// at the attempt's speed over its section.
    pub tempo: AttemptTempo,
}

/// What an attempt's notes end at (design.md "Play Lifecycle"; the skin
/// runtime's `last_note_end`), by what it plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteEnd {
    /// The whole chart, played or autoplayed: the last note's time, a long
    /// note's end and a mine included; 0 for a chart without notes.
    Play,
    /// A practice round: the section's end in the chart's 1× time, mapped
    /// exactly to the real time of a chart loaded at `speed`, whatever
    /// notes the chart has.
    Practice {
        /// The section's end, in the chart's 1× time.
        end: TimeUs,
        /// The speed the chart is loaded at.
        speed: Speed,
    },
}

impl NoteEnd {
    /// The end on `chart`'s play clock, as each variant says.
    #[must_use]
    pub fn of(self, chart: &Chart) -> TimeUs {
        match self {
            Self::Play => chart
                .lanes
                .iter()
                .flatten()
                .map(|n| n.end())
                .max()
                .unwrap_or(TimeUs(0)),
            Self::Practice { end, speed } => mapped(end, speed),
        }
    }
}

impl Attempt {
    /// The attempt numbered `number` of `chart` under `preset`: the whole
    /// chart with a `lead_in` READY state, or the round of `section`, its
    /// notes ending at `last_note_end` ([`NoteEnd::of`]), with `chart`'s
    /// tempo provider over the same section ([`AttemptTempo::prepare`]).
    #[must_use]
    pub fn new(
        number: u32,
        preset: &RulePreset,
        lead_in: TimeUs,
        section: Option<&Section>,
        last_note_end: TimeUs,
        labels: &ChartLabels,
        chart: &Chart,
    ) -> Self {
        let kind = match section {
            Some(s) => AttemptKind::Section(s.clone()),
            None => AttemptKind::Full { lead_in },
        };
        Self {
            number,
            preset: (preset.id.clone(), preset.version),
            gauges: preset.gauges.clone(),
            displayed: GaugeIndex(0),
            empty_poor_breaks_combo: preset.empty_poor.breaks_combo,
            empty_poor_counted: preset.empty_poor.counted,
            ln_default: preset.ln_mode.default,
            kind,
            last_note_end,
            labels: labels.clone(),
            tempo: AttemptTempo::prepare(chart, section),
        }
    }
}

#[cfg(test)]
mod tests {
    use bms_model::{BgmEvent, ChartBuilder, Lane, SoundId, Speed, TimeUs};

    use super::NoteEnd;

    const MS: i64 = 1_000;

    /// The whole chart's notes end at the last note's time, a long note's
    /// end and a mine included, whatever background sounds follow; a
    /// round's at its 1× section end mapped to the loaded speed, exactly
    /// at the edge of the time range too.
    #[test]
    fn the_note_end_follows_what_the_attempt_plays() {
        let lane = |i| Lane::new(i).unwrap();
        let mut chart = ChartBuilder::new(150_000)
            .tap(lane(1), TimeUs(1_000 * MS))
            .long(lane(2), TimeUs(2_000 * MS), TimeUs(3_500 * MS))
            .tap(lane(3), TimeUs(5_000 * MS))
            .mine(lane(4), TimeUs(9_000 * MS), 1)
            .build()
            .unwrap();
        assert_eq!(NoteEnd::Play.of(&chart), TimeUs(9_000 * MS));
        chart.bgm.push(BgmEvent {
            time: TimeUs(12_000 * MS),
            sound: SoundId(1),
        });
        assert_eq!(NoteEnd::Play.of(&chart), TimeUs(9_000 * MS));
        // 4 000 ms · 100 / 75.
        let practice = NoteEnd::Practice {
            end: TimeUs(4_000 * MS),
            speed: Speed::from_twentieths(15).unwrap(),
        };
        assert_eq!(practice.of(&chart), TimeUs(5_333_333));
        let practice = NoteEnd::Practice {
            end: TimeUs(4_000 * MS),
            speed: Speed::NORMAL,
        };
        assert_eq!(practice.of(&chart), TimeUs(4_000 * MS));
        // floor(i64::MAX · 100 / 150).
        let practice = NoteEnd::Practice {
            end: TimeUs(i64::MAX),
            speed: Speed::from_twentieths(30).unwrap(),
        };
        assert_eq!(practice.of(&chart), TimeUs(6_148_914_691_236_517_204));
    }

    /// Without notes, the whole chart ends at 0 whatever background
    /// sounds it has, and a round at its section's end still.
    #[test]
    fn the_note_end_without_notes() {
        let mut empty = ChartBuilder::new(150_000).build().unwrap();
        empty.bgm.push(BgmEvent {
            time: TimeUs(7_000 * MS),
            sound: SoundId(1),
        });
        assert_eq!(NoteEnd::Play.of(&empty), TimeUs(0));
        let practice = NoteEnd::Practice {
            end: TimeUs(4_000 * MS),
            speed: Speed::NORMAL,
        };
        assert_eq!(practice.of(&empty), TimeUs(4_000 * MS));
    }
}
