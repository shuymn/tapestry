//! A play's attempt as the screen prepares it (design.md "Play Lifecycle"):
//! what the attempt is fixed with, read from the chart and the play preset
//! once the preset has been refreshed for it, and how its preparation
//! ended. Everything here is ordinary game data: the player translates it
//! into whatever a skin takes, so the game depends on no skin crate.

use bms_model::{Chart, JudgmentHeader, NoteKind, Speed, TimeUs};
use eval::Section;
use rules::{GaugeRule, HeaderPolicy, LnMode, RulePreset};

use crate::policy::GaugeIndex;
use crate::practice::mapped;
use crate::prepare::ChartLabels;
use crate::tempo::AttemptTempo;

/// How a play's preparation on the screen ended ([`crate::Rig::poll_preparation`]).
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
    /// The chart's gauge total as the reference player's song information
    /// gives it ([`song_total`]).
    pub song_total: i32,
    /// The chart's judgment-header declarations, as its ingestion kept
    /// them, from which a skin derives the class it shows whatever rank the
    /// preset judges with.
    pub judgment_headers: Vec<JudgmentHeader>,
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
            song_total: song_total(chart),
            judgment_headers: chart.meta.judgment_headers.clone(),
            tempo: AttemptTempo::prepare(chart, section),
        }
    }
}

/// `chart`'s gauge total as the reference player's song information gives
/// it, whatever the play preset (beatoraja `IntegerPropertyFactory.java:
/// 389-394`, `(int) getTotal()`): its `#TOTAL` read as the reference decoder
/// reads it, a number above 0 ([`HeaderPolicy::DefExRankFirst`]), else the
/// default the reference sets for an absent one, `max(260, 7.605 n /
/// (0.01 n + 6.5))` (`play/BMSPlayerRule.java:68-80`). `n` is the whole
/// chart's notes as the reference counts them in its default long-note
/// mode, each tap and each long note once and no mine (jbms-parser
/// `TimeLine.getTotalNotes`). The default is computed in `f64` in the
/// reference's order of operations, so its truncation is the reference's
/// (an exact quotient differs at 4 420 notes: 663 exactly, 662 in `f64`);
/// a `#TOTAL` is truncated exactly, and past `i32` saturates, as the
/// reference's cast does.
#[must_use]
pub fn song_total(chart: &Chart) -> i32 {
    if let Some(milli) = HeaderPolicy::DefExRankFirst
        .read(&chart.meta.judgment_headers)
        .total_milli
    {
        return i32::try_from(milli / 1_000).unwrap_or(i32::MAX);
    }
    let notes = chart
        .lanes
        .iter()
        .flatten()
        .filter(|n| !matches!(n.kind, NoteKind::Mine { .. }))
        .count();
    #[expect(
        clippy::cast_precision_loss,
        reason = "a note count is far below 2^53, where f64 is exact"
    )]
    let n = notes as f64;
    let total = f64::max(260.0, 7.605 * n / (0.01 * n + 6.5));
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the reference truncates (`(int)`); the value is at most 760.5"
    )]
    let total = total as i32;
    total
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

    /// A chart of `taps` taps on key 1, one long note on key 2 and one mine
    /// on key 3, with `headers` declared.
    fn counted(taps: i64, headers: &[bms_model::HeaderValue]) -> bms_model::Chart {
        let lane = |i| Lane::new(i).unwrap();
        let mut b = ChartBuilder::new(150_000)
            .long(lane(2), TimeUs(0), TimeUs(500 * MS))
            .mine(lane(3), TimeUs(0), 1);
        for t in 0..taps {
            b = b.tap(lane(1), TimeUs(t * MS));
        }
        let mut chart = b.build().unwrap();
        chart.meta.judgment_headers = headers
            .iter()
            .map(|&value| bms_model::JudgmentHeader {
                name: bms_model::JudgmentHeaderName::Total,
                value,
            })
            .collect();
        chart
    }

    /// The song total is the `#TOTAL` the reference decoder reads (above 0,
    /// the last such one), truncated, else `(int) max(260, 7.605 n /
    /// (0.01 n + 6.5))` in `f64` over the taps and long notes, each once,
    /// no mine: 260 to 337 notes, the reference's truncation at 4 420 notes
    /// (662, where the exact quotient is 663), whatever the preset.
    #[test]
    fn the_song_total_is_the_references() {
        use bms_model::HeaderValue;
        let total = |taps, headers: &[HeaderValue]| super::song_total(&counted(taps, headers));
        assert_eq!(total(3, &[]), 260);
        // 336 taps and the long note: 337 notes; one more passes 260.
        assert_eq!(total(336, &[]), 260);
        assert_eq!(total(337, &[]), 260);
        // 1 000 notes: 460.909.
        assert_eq!(total(999, &[]), 460);
        assert_eq!(total(4_419, &[]), 662);
        assert_eq!(total(4_418, &[]), 662);
        assert_eq!(total(4_420, &[]), 663);
        assert_eq!(
            total(3, &[HeaderValue::decimal_milli(300_700)]),
            300,
            "truncated"
        );
        assert_eq!(total(3, &[HeaderValue::decimal_milli(500)]), 0);
        for refused in [
            HeaderValue::integer(0),
            HeaderValue::integer(-5),
            HeaderValue::NotANumber,
        ] {
            assert_eq!(
                total(3, &[HeaderValue::integer(200), refused]),
                200,
                "{refused:?} keeps the earlier one"
            );
            assert_eq!(total(3, &[refused]), 260, "{refused:?} alone");
        }
        assert_eq!(
            total(3, &[HeaderValue::Integer(10_i128.pow(29))]),
            i32::MAX,
            "saturates"
        );
    }
}
