//! The values a song list previews of a chart before it is chosen (design.md
//! "Loading and Song Selection", Summaries): its note counts by kind, its
//! densities, its gauge total and its MAIN tempo, each as the reference
//! player's song information shows it (beatoraja `song/SongInformation.java`
//! and `skin/property/IntegerPropertyFactory.java` at 0.8.9).
//!
//! [`ChartInfo`] gives the integer parts; the divisions and the gauge total
//! are done here, once at the scan, exactly, and the shown integers, each
//! rounded down, are stored. The reference computes them in `double`, whose
//! rounding is not reproduced (design.md "Design Principles", "Reproduce a
//! source's rules, not its byproducts").

use bms_model::{ChartInfo, ChartProfile, HeaderValue, JudgmentHeader, Tempo};
use rules::HeaderPolicy;

/// What a song list previews of a chart: each value the integer the
/// reference player's number shows, from the seed-0 branch at 1×.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChartInfoView {
    /// Key taps.
    pub key_taps: i32,
    /// Key long notes.
    pub key_longs: i32,
    /// Scratch taps.
    pub scratch_taps: i32,
    /// Scratch long notes.
    pub scratch_longs: i32,
    /// The most notes in a second ([`ChartInfo::peak`]); its hundredths
    /// are always 0.
    pub density_peak: i32,
    /// The end density's whole part: the most notes a second over the
    /// window after the border ([`ChartInfo::end_notes`]).
    pub density_end: i32,
    /// Its hundredths, `((int)(x · 100)) % 100`.
    pub density_end_hundredths: i32,
    /// The average density's whole part: the mean of the seconds at or
    /// above the floor ([`ChartInfo::average_notes`]).
    pub density_average: i32,
    /// Its hundredths.
    pub density_average_hundredths: i32,
    /// The gauge total ([`GaugeTotal::whole`]), rounded down.
    pub total: i32,
    /// The MAIN tempo in whole BPM truncated, 0 when no note weighs
    /// anything (a chart of mines alone), as the reference's song
    /// information leaves it; `None` when it was unavailable (its histogram
    /// was not allocated).
    pub main_bpm: Option<i32>,
}

/// `v` as a shown integer: saturated to `i32`.
fn shown_int(v: i128) -> i32 {
    i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX })
}

/// The whole part and the hundredths of `notes / seconds`, both rounded
/// down, exactly; 0 over no second.
fn shown_mean(notes: u64, seconds: u32) -> (i32, i32) {
    let (n, s) = (i128::from(notes), i128::from(seconds));
    if s == 0 {
        return (0, 0);
    }
    (shown_int(n / s), shown_int(n * 100 / s % 100))
}

/// The gauge total the reference's song information reads
/// (`song/SongData.java` validates the model with
/// `play/BMSPlayerRule.java:68-80` before building it), exactly: the
/// `#TOTAL` the reference decoder reads, a number above 0
/// ([`HeaderPolicy::DefExRankFirst`]), at the loader's `10^-18`; else the
/// gauge's default `max(260, 7.605 n / (0.01 n + 6.5))` for the chart's
/// `n` notes ([`rules::DefaultTotal::NotesCurve`]). The reference works in
/// `double`, so a value its rounding moves across an integer shows
/// differently (design.md "Differences from the Source").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeTotal {
    /// Above 0.
    numerator: i128,
    /// Above 0.
    denominator: i128,
}

impl GaugeTotal {
    /// The total of a chart with the judgment `headers` and `notes` notes.
    #[must_use]
    pub fn of(headers: &[JudgmentHeader], notes: u32) -> Self {
        if let Some(units) = HeaderPolicy::DefExRankFirst
            .total_header(headers)
            .and_then(HeaderValue::number)
            .filter(|&v| v > 0)
        {
            return Self {
                numerator: units,
                denominator: HeaderValue::ONE,
            };
        }
        // 7.605 n / (0.01 n + 6.5) = 7 605 n / (10 n + 6 500).
        let n = i128::from(notes);
        let (curve, over) = (7_605 * n, 10 * n + 6_500);
        if curve >= 260 * over {
            Self {
                numerator: curve,
                denominator: over,
            }
        } else {
            Self {
                numerator: 260,
                denominator: 1,
            }
        }
    }

    /// Its whole part, rounded down: number 368.
    #[must_use]
    pub fn whole(self) -> i32 {
        shown_int(self.numerator / self.denominator)
    }

    /// The border's start for `notes` notes: `N · (1 − 100 / total)`
    /// truncated toward 0, as the reference starts its count.
    #[must_use]
    pub fn border_start(self, notes: u32) -> i64 {
        // N − a / p, with a = 100 · N · q.
        let n = i128::from(notes);
        let a = 100 * n * self.denominator;
        let floor = a / self.numerator;
        let ceil = floor + i128::from(a % self.numerator != 0);
        // Above 0 it rounds down (N − ⌈a / p⌉), below 0 up (N − ⌊a / p⌋).
        let start = if ceil <= n { n - ceil } else { n - floor };
        i64::try_from(start).unwrap_or(i64::MIN)
    }
}

impl ChartInfoView {
    /// The view of the chart whose seed-0 branch has the preview `profile`,
    /// the judgment `headers` and the MAIN tempo `main` (as
    /// [`bms_parser::Scanned::main`]).
    #[must_use]
    pub fn of(
        profile: &ChartProfile,
        headers: &[JudgmentHeader],
        main: Option<Option<Tempo>>,
    ) -> Self {
        let info = ChartInfo::analyze(profile, |notes| {
            GaugeTotal::of(headers, notes).border_start(notes)
        });
        let count = |n: u32| i32::try_from(n).unwrap_or(i32::MAX);
        let (density_end, density_end_hundredths) = shown_mean(info.end_notes, info.end_seconds);
        let (density_average, density_average_hundredths) =
            shown_mean(info.average_notes, info.average_seconds);
        let one = Tempo::UNITS_PER_BPM;
        Self {
            key_taps: count(info.key_taps),
            key_longs: count(info.key_longs),
            scratch_taps: count(info.scratch_taps),
            scratch_longs: count(info.scratch_longs),
            density_peak: i32::try_from(info.peak).unwrap_or(i32::MAX),
            density_end,
            density_end_hundredths,
            density_average,
            density_average_hundredths,
            total: GaugeTotal::of(headers, info.notes).whole(),
            main_bpm: main
                .map(|m| m.map_or(0, |t| i32::try_from(t.units() / one).unwrap_or(i32::MAX))),
        }
    }
}

#[cfg(test)]
mod tests;
