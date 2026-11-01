//! A play's attempt as the screen prepares it (design.md "Play Lifecycle"):
//! what the attempt is fixed with, read from the chart and the play preset
//! once the preset has been refreshed for it, and how its preparation
//! ended. Everything here is ordinary game data: the player translates it
//! into whatever a skin takes, so the game depends on no skin crate.

use bms_model::{Chart, Speed, TimeUs};
use eval::Section;
use rules::{GaugeRule, LnMode, RulePreset};

use crate::practice::mapped_wide;
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
    /// The end of the attempt's notes on its play clock ([`NoteEnd`]):
    /// the endpoint a skin's `last_note` timer samples from, kept apart
    /// from any remaining-time readout.
    pub last_note_end: TimeUs,
    /// The chart's labels, from the ingestion that read the chart.
    pub labels: ChartLabels,
}

/// What an attempt's notes end at (design.md "Play Lifecycle"; the skin
/// runtime's `last_note_end`), by how it plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteEnd {
    /// A normal play: the last note's time, a long note's end and a mine
    /// included; 0 for a chart without notes.
    Play,
    /// Autoplay: the last timeline's, background sounds included; a chart
    /// with background sounds and no notes ends at its last one, and one
    /// with neither at 0.
    Autoplay,
    /// A practice round: the section's end in the chart's 1× time plus
    /// 1 s, mapped to the real time of a chart loaded at `speed`, whatever
    /// notes the chart has; the sum and the mapping are exact, and only
    /// the end saturates.
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
        let notes = || chart.lanes.iter().flatten().map(|n| n.end());
        match self {
            Self::Play => notes().max().unwrap_or(TimeUs(0)),
            Self::Autoplay => notes()
                .chain(chart.bgm.iter().map(|b| b.time))
                .max()
                .unwrap_or(TimeUs(0)),
            Self::Practice { end, speed } => {
                mapped_wide(i128::from(end.0) + i128::from(PRACTICE_PAD.0), speed)
            }
        }
    }
}

/// What a practice round's note end adds to its section's end, in the
/// chart's 1× time.
const PRACTICE_PAD: TimeUs = TimeUs(1_000_000);

impl Attempt {
    /// The attempt numbered `number` of `chart` under `preset`: the whole
    /// chart with a `lead_in` countdown, or the round of `section`, its
    /// notes ending at `last_note_end` ([`NoteEnd::of`]).
    #[must_use]
    pub fn new(
        number: u32,
        preset: &RulePreset,
        lead_in: TimeUs,
        section: Option<&Section>,
        last_note_end: TimeUs,
        labels: &ChartLabels,
    ) -> Self {
        let kind = match section {
            Some(s) => AttemptKind::Section(s.clone()),
            None => AttemptKind::Full { lead_in },
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
    use bms_model::{BgmEvent, ChartBuilder, Lane, SoundId, Speed, TimeUs};

    use super::NoteEnd;

    const MS: i64 = 1_000;

    /// A normal play's notes end at the last note's time, a long note's
    /// end and a mine included; autoplay's at the last timeline's, a
    /// background sound after the notes included; a round's at its 1×
    /// section end plus 1 s, mapped to the loaded speed, exactly at the
    /// edge of the time range too.
    #[test]
    fn the_note_end_follows_how_the_attempt_plays() {
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
        assert_eq!(NoteEnd::Autoplay.of(&chart), TimeUs(12_000 * MS));
        // (4 000 ms + 1 000 ms) · 100 / 75.
        let practice = NoteEnd::Practice {
            end: TimeUs(4_000 * MS),
            speed: Speed::from_twentieths(15).unwrap(),
        };
        assert_eq!(practice.of(&chart), TimeUs(6_666_666));
        let practice = NoteEnd::Practice {
            end: TimeUs(4_000 * MS),
            speed: Speed::NORMAL,
        };
        assert_eq!(practice.of(&chart), TimeUs(5_000 * MS));
        // The pad and the scaling are exact past `TimeUs`'s range:
        // floor((i64::MAX + 1 000 000) · 100 / 150).
        let practice = NoteEnd::Practice {
            end: TimeUs(i64::MAX),
            speed: Speed::from_twentieths(30).unwrap(),
        };
        assert_eq!(practice.of(&chart), TimeUs(6_148_914_691_237_183_871));
    }

    /// Without notes, a normal play ends at 0, autoplay at its last
    /// background sound or 0 without one, and a round at its section's
    /// padded end still.
    #[test]
    fn the_note_end_without_notes() {
        let mut empty = ChartBuilder::new(150_000).build().unwrap();
        assert_eq!(NoteEnd::Play.of(&empty), TimeUs(0));
        assert_eq!(NoteEnd::Autoplay.of(&empty), TimeUs(0));
        let practice = NoteEnd::Practice {
            end: TimeUs(4_000 * MS),
            speed: Speed::NORMAL,
        };
        assert_eq!(practice.of(&empty), TimeUs(5_000 * MS));
        empty.bgm.push(BgmEvent {
            time: TimeUs(7_000 * MS),
            sound: SoundId(1),
        });
        assert_eq!(NoteEnd::Play.of(&empty), TimeUs(0));
        assert_eq!(NoteEnd::Autoplay.of(&empty), TimeUs(7_000 * MS));
        assert_eq!(practice.of(&empty), TimeUs(5_000 * MS));
    }
}
