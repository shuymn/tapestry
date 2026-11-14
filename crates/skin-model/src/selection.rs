//! The selection screen's snapshot (docs/stage3.md, the presentation
//! snapshot's Selection row): a bounded window of the list's rows around the
//! cursor, each with its identity, kind, text, level and typed score, the
//! cursor's position as an exact ratio, the place and the query, and what is
//! known of the selected chart. The player fills it; whatever draws the
//! screen reads it, with the player's own strings in [`SelectionLabels`].

use std::path::{Path, PathBuf};

use bms_model::ChartSummary;

use crate::GaugeLook;

/// The most rows a [`SelectionView`] holds.
pub const MAX_VISIBLE_ROWS: usize = 64;

/// What selection shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionView {
    /// At most [`MAX_VISIBLE_ROWS`], in list order.
    pub rows: Vec<RowView>,
    /// The cursor's row, an index into `rows`; `None` when nothing is listed.
    pub cursor: Option<usize>,
    /// The cursor among the selectable rows of the whole list, exactly:
    /// `None` under two.
    pub position: Option<Ratio>,
    /// Where selection is.
    pub place: SelectionPlace,
    /// The search box's text.
    pub query: String,
    /// The chart under the cursor; `None` on any other row.
    pub selected: Option<SelectedChart>,
    /// The list's revision: it changes whenever the list's rows may have
    /// changed, so a [`RowRef`](crate::RowRef) drawn from an earlier list
    /// names no row of this one.
    pub list_revision: u64,
}

/// An exact ratio `numerator / denominator`, the denominator never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ratio {
    pub numerator: u64,
    pub denominator: u64,
}

impl Ratio {
    /// `numerator / denominator`; `None` for a zero denominator.
    #[must_use]
    pub const fn new(numerator: u64, denominator: u64) -> Option<Self> {
        if denominator == 0 {
            None
        } else {
            Some(Self {
                numerator,
                denominator,
            })
        }
    }

    /// `x`'s exact binary value `m / 2^k` in lowest terms, for a finite
    /// `x` from 0 to 1; one whose denominator would pass 2^63, which only
    /// a value below 2^-40 (−240 dB) can need, reads as 0, and anything
    /// else (negative, above 1, not finite) as `None`. Reads the bits
    /// only: no float arithmetic.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    #[must_use]
    pub fn of_f32(x: f32) -> Option<Self> {
        const ZERO: Ratio = Ratio {
            numerator: 0,
            denominator: 1,
        };
        let bits = x.to_bits();
        let biased = (bits >> 23) & 0xff;
        let fraction = u64::from(bits & 0x7f_ffff);
        if bits << 1 == 0 {
            // ±0.
            return Some(ZERO);
        }
        if bits >> 31 != 0 || biased == 0xff {
            return None;
        }
        // x = m / 2^k: a normal value has its implicit bit, a subnormal
        // the smallest exponent.
        let (m, k) = if biased == 0 {
            (fraction, 149)
        } else {
            (fraction | 1 << 23, 150 - i64::from(biased))
        };
        // k < 0 is past 2^24.
        let k = u32::try_from(k).ok()?;
        let twos = m.trailing_zeros().min(k);
        let (m, k) = (m >> twos, k - twos);
        match 1_u64.checked_shl(k) {
            Some(d) if m > d => None,
            Some(d) => Some(Self {
                numerator: m,
                denominator: d,
            }),
            // Below 2^24 / 2^64.
            None => Some(ZERO),
        }
    }
}

/// A volume as a selection skin shows it: the gain that plays,
/// as an exact ratio for the sliders and graphs, and its number, the
/// whole percent of that ratio rounded half up (0.53 shows 53).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeReading {
    /// The gain, exactly ([`Ratio::of_f32`]).
    pub ratio: Ratio,
    /// The whole percent, rounded half up.
    pub number: i32,
}

impl VolumeReading {
    /// The volume of `gain`, a gain from 0 to 1 (`device.toml`'s range);
    /// one outside it reads as 0.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    #[must_use]
    pub fn of_gain(gain: f32) -> Self {
        let Some(ratio) = Ratio::of_f32(gain) else {
            return Self {
                ratio: Ratio {
                    numerator: 0,
                    denominator: 1,
                },
                number: 0,
            };
        };
        // ⌊(200·n + d) / (2·d)⌋ with n ≤ d < 2^64: within `u128`, and at
        // most 100 since the gain is at most 1.
        let n = u128::from(ratio.numerator);
        let d = u128::from(ratio.denominator);
        let number = i32::try_from((200 * n + d) / (2 * d)).unwrap_or(0);
        Self { ratio, number }
    }
}

/// How the hi-speed follows the settings, as a selection skin shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeedView {
    /// A saved multiplier: no travel duration without a chart's tempo.
    Classic,
    /// Solved from the target green number `green` (1 to 6 000) at the
    /// tempo `reference` names.
    Floating {
        reference: SpeedReference,
        green: u32,
    },
}

/// The chart tempo Floating solves from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeedReference {
    Start,
    Maximum,
    Main,
    Minimum,
}

/// The order the list is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionOrder {
    Title,
    Artist,
    Level,
    /// By the file's change, newest first: no source sorter has it.
    Modified,
}

/// The player's settings a selection skin shows, read-only (docs/stage3.md
/// §3, Selection): applied to the state apart from the view
/// ([`Skin::update_selection_settings`](crate::Skin::update_selection_settings)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionSettings {
    /// The display offset in whole ms, its sign the settings': a positive
    /// one draws the chart as if later, as the source's judge timing.
    pub display_offset_ms: i32,
    /// The system volume that plays.
    pub system_volume: VolumeReading,
    /// The key volume that plays.
    pub key_volume: VolumeReading,
    /// The background volume that plays.
    pub background_volume: VolumeReading,
    pub speed: SpeedView,
    pub order: SelectionOrder,
    /// The chosen preset's displayed gauge's look; `None` without a
    /// preset.
    pub gauge: Option<GaugeLook>,
    /// Whether SUDDEN+, LIFT and HIDDEN+ are on.
    pub covers_on: [bool; 3],
    /// Whether a failed play goes on to the end (`player.toml`'s
    /// `on_failure`), rather than closing.
    pub play_to_end: bool,
    /// The chosen preset's press assignment; `None` without a preset.
    pub assignment: Option<rules::Assignment>,
}

/// Where selection is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SelectionPlace {
    /// The "all charts" row, then the library's roots or the one root's level.
    #[default]
    Top,
    /// A folder's level: the directory names of the folders entered from
    /// Top's level to this one, outermost first.
    Folder(Vec<String>),
    /// The all-chart list.
    All,
    /// A query's results, whatever the place under them.
    Results,
}

/// A folder's identity: its path as the library stores it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FolderId(PathBuf);

impl FolderId {
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self(path)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// A row's identity, never its text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    All,
    Folder(FolderId),
    Chart(PathBuf),
    /// The heading of a folder's charts in the all-chart list and results.
    Heading(FolderId),
}

/// What a row is, with what it counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// The "all charts" row; its label is the drawer's own.
    All {
        charts: u32,
    },
    /// A folder row: the listed charts under it.
    Folder {
        charts: u32,
    },
    Chart,
    /// Not selectable.
    Heading,
}

/// One row as it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowView {
    pub key: RowKey,
    pub kind: RowKind,
    /// The row's index among the list's selectable rows, the cursor's
    /// space; `None` for a heading, and where the view does not number
    /// its rows.
    pub index: Option<u32>,
    /// A chart's `#TITLE` as stored, a folder's directory name, a heading's
    /// folder under its root; empty for the "all charts" row and a chart
    /// without a title.
    pub title: String,
    pub subtitle: String,
    pub artist: String,
    /// `#PLAYLEVEL` as stored; empty when there is none.
    pub level: String,
    /// `level` as a number, as a skin draws it: what Java's
    /// `Integer.parseInt` reads, 0 when it reads none; 0 on a row that is
    /// not a chart.
    pub level_value: i32,
    /// `#DIFFICULTY` when it is 1 to 5.
    pub difficulty: Option<u8>,
    /// Under the chosen preset; `Unplayed` on a row that is not a chart.
    pub score: RowScore,
    /// What the chart's summary tells of its notes; `None` without a
    /// summary and on a row that is not a chart.
    pub features: Option<RowFeatures>,
    /// Whether the chart declares `#RANDOM` or `#SETRANDOM`; false on a row
    /// that is not a chart.
    pub random: bool,
}

/// What a chart's summary (the draw with seed 0) tells of its notes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowFeatures {
    pub long_notes: bool,
    pub mines: bool,
}

impl RowFeatures {
    /// The features `summary` shows.
    #[must_use]
    pub const fn of(summary: &ChartSummary) -> Self {
        Self {
            long_notes: summary.longs > 0,
            mines: summary.mines > 0,
        }
    }
}

/// A chart's best under the chosen preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowScore {
    /// Never played under it, or no preset is chosen.
    Unplayed,
    /// Played, and no play set a best.
    NoBest { plays: u64 },
    Best {
        plays: u64,
        ex: u32,
        bp: u32,
        lamp: OwnLamp,
    },
}

/// A best's lamp under the chosen preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnLamp {
    Failed,
    /// The cleared gauge of the highest lamp rank: its index in the preset's
    /// gauges, that rank, and the gauge's look ([`GaugeLook::of`]), which a
    /// skin's lamp images are chosen by.
    Cleared {
        gauge: u16,
        rank: u16,
        look: GaugeLook,
    },
    FullCombo,
}

/// A chart's descriptive header, each field as stored, empty when absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChartLabels {
    pub title: String,
    pub subtitle: String,
    pub artist: String,
    pub subartist: String,
    pub genre: String,
    pub level: String,
    /// `level` as a number, as [`RowView::level_value`].
    pub level_value: i32,
    /// `#DIFFICULTY` when it is 1 to 5.
    pub difficulty: Option<u8>,
}

/// The player's own strings a skin draws on selection, in the player's
/// language: never chart, folder or query text, and never a literal of the
/// skin's or of this crate's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionLabels {
    /// The "all charts" row's and place's name.
    pub all_charts: String,
    /// The name of a query's results.
    pub search_results: String,
}

/// The chart under the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedChart {
    pub labels: ChartLabels,
    /// The summary of its draw with seed 0; `None` when that draw is refused.
    pub summary: Option<ChartSummary>,
    /// Whether it declares `#RANDOM` or `#SETRANDOM`.
    pub random: bool,
    /// The largest EX score its summary allows under the chosen preset, for
    /// rates: `None` for a chart that draws, whose best may come from a draw
    /// with another note count, and for one without a summary or preset.
    pub max_ex: Option<u64>,
    pub score: RowScore,
    /// Its judgment class from its library entry's rank, 0 the hardest to
    /// 4 the easiest; `None` without one, or for a rank in no class. A
    /// value above 4 is none.
    pub judge_class: Option<u8>,
    /// The counted plays that were clears under the chosen preset: eligible
    /// plays whose own lamp is not FAILED (design.md "Personal Bests"); 0
    /// when unplayed.
    pub clears: u64,
    /// The best's values beyond `score`'s; `None` without a best.
    pub best: Option<SelectedBest>,
}

/// What the bests keep of the selected chart beside its EX, BP and lamp
/// (design.md "Personal Bests").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectedBest {
    /// The best max combo, of any counted play: not necessarily `play`'s.
    pub max_combo: u32,
    /// The one play that set the best EX score.
    pub play: BestPlay,
}

/// The play that set a best EX score, every figure from that one play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BestPlay {
    /// Its evaluation's counts: judgments, their early and late splits,
    /// empty POORs, EX score, BP and max combo; the current combo is 0.
    pub tally: eval::Tally,
    /// The judged notes of the chart as it drew them.
    pub notes: u32,
}

impl BestPlay {
    /// The source's point score of a 7-key play
    /// (`ScoreDataProperty.java:75-78` at `0.8.9`): `⌊(150 000·PERFECT +
    /// 100 000·GREAT + 20 000·GOOD) / n⌋ + ⌊50 000·combo / n⌋`, two floors,
    /// with this play's own max combo where the source takes the best max
    /// combo, so every figure is one play's; 0 with no notes.
    #[must_use]
    pub fn point(&self) -> u64 {
        let n = u64::from(self.notes);
        if n == 0 {
            return 0;
        }
        let c = &self.tally.counts;
        let judged = 150_000 * u64::from(c.perfect)
            + 100_000 * u64::from(c.great)
            + 20_000 * u64::from(c.good);
        judged / n + 50_000 * u64::from(self.tally.max_combo) / n
    }
}

#[cfg(test)]
mod tests {
    use bms_model::{ChartSummary, Tempo, TimeUs};

    use super::{Ratio, RowFeatures};

    /// A summary's long notes and mines show as features, each on its own.
    #[test]
    fn features_read_long_notes_and_mines() {
        let tempo = Tempo::from_units(Tempo::UNITS_PER_BPM).unwrap();
        let summary = |longs, mines| ChartSummary {
            taps: 1,
            longs,
            mines,
            length: TimeUs(1),
            tempo_start: tempo,
            tempo_min: tempo,
            tempo_max: tempo,
        };
        let f = |long_notes, mines| RowFeatures { long_notes, mines };
        assert_eq!(RowFeatures::of(&summary(0, 0)), f(false, false));
        assert_eq!(RowFeatures::of(&summary(1, 0)), f(true, false));
        assert_eq!(RowFeatures::of(&summary(0, 1)), f(false, true));
        assert_eq!(RowFeatures::of(&summary(5, 9)), f(true, true));
    }

    /// The point score floors its two terms apart, as the source's: notes
    /// 7, PERFECT 4, GREAT 2, BAD 1, combo 4 give 114 285 + 28 571, where
    /// one floor over the sum gives 142 857; no notes give 0.
    #[test]
    fn the_point_score_floors_its_two_terms_apart() {
        let mut tally = eval::Tally::default();
        tally.counts.perfect = 4;
        tally.counts.great = 2;
        tally.counts.bad = 1;
        tally.max_combo = 4;
        let play = |notes| super::BestPlay { tally, notes };
        assert_eq!(play(7).point(), 142_856);
        assert_eq!(play(0).point(), 0);
        // GOOD weighs 20 000: one GOOD of one note, its combo 1.
        let mut good = eval::Tally::default();
        good.counts.good = 1;
        good.max_combo = 1;
        assert_eq!(
            super::BestPlay {
                tally: good,
                notes: 1
            }
            .point(),
            70_000
        );
    }

    #[test]
    fn a_ratio_has_a_denominator() {
        assert_eq!(Ratio::new(1, 0), None);
        assert_eq!(
            Ratio::new(2, 3),
            Some(Ratio {
                numerator: 2,
                denominator: 3
            })
        );
    }

    fn ratio(numerator: u64, denominator: u64) -> Ratio {
        Ratio {
            numerator,
            denominator,
        }
    }

    /// A gain's ratio is its `f32`'s exact binary value in lowest terms,
    /// with the bits read only; below 2^-40 it reads 0, and outside 0 to
    /// 1 it is none.
    #[test]
    fn a_gain_reads_as_its_exact_binary_value() {
        for (x, want) in [
            (0.0_f32, Some(ratio(0, 1))),
            (-0.0, Some(ratio(0, 1))),
            (1.0, Some(ratio(1, 1))),
            (0.5, Some(ratio(1, 2))),
            // 0x3f4f5c29, 0x3f43d07d, 0x3ee66666 and 0x3c23d70a.
            (0.81, Some(ratio(13_589_545, 16_777_216))),
            (0.7649, Some(ratio(12_832_893, 16_777_216))),
            (0.45, Some(ratio(7_549_747, 16_777_216))),
            (0.01, Some(ratio(5_368_709, 536_870_912))),
            (2.0_f32.powi(-40), Some(ratio(1, 1 << 40))),
            // 2^-40 · (1 + 2^-23): the denominator 2^63.
            (
                f32::from_bits(2.0_f32.powi(-40).to_bits() + 1),
                Some(ratio((1 << 23) + 1, 1 << 63)),
            ),
            // Below 2^-40, exact while lowest terms fit: 1.5 · 2^-41.
            (2.0_f32.powi(-41) * 1.5, Some(ratio(3, 1 << 42))),
            // 2^-41 · (1 + 2^-23) would need 2^64.
            (
                f32::from_bits(2.0_f32.powi(-41).to_bits() + 1),
                Some(ratio(0, 1)),
            ),
            (f32::from_bits(1), Some(ratio(0, 1))),
            (1.000_000_1, None),
            (2.0, None),
            (-0.5, None),
            (f32::NAN, None),
            (f32::INFINITY, None),
        ] {
            assert_eq!(Ratio::of_f32(x), want, "{x:e}");
        }
    }

    /// The volume's number is the whole percent of the exact gain,
    /// rounded half up: the `f32` nearest 0.53 is 0.529 999 97, 53 %.
    #[test]
    fn the_volume_number_is_the_whole_percent_rounded_half_up() {
        for (gain, number) in [
            (0.0_f32, 0),
            (0.01, 1),
            (0.005, 0),
            (0.015, 1),
            (0.07, 7),
            (0.125, 13),
            (0.375, 38),
            (0.5, 50),
            (0.53, 53),
            (0.59, 59),
            (0.7649, 76),
            (0.995, 100),
            (1.0, 100),
        ] {
            let v = super::VolumeReading::of_gain(gain);
            assert_eq!(v.number, number, "{gain}");
            assert_eq!(Some(v.ratio), Ratio::of_f32(gain), "{gain}");
        }
        assert_eq!(super::VolumeReading::of_gain(1.5).number, 0);
    }
}
