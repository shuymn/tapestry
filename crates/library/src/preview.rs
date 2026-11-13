//! The values a song list previews of a chart before it is chosen (design.md
//! "Loading and Song Selection", Summaries): its note counts by kind, its
//! densities, its gauge total and its MAIN tempo, each as the reference
//! player's song information shows it (beatoraja `song/SongInformation.java`
//! and `skin/property/IntegerPropertyFactory.java` at 0.8.9).
//!
//! [`ChartInfo`] gives the integer parts; dividing them and reading the
//! gauge total takes floating point, which the chart model does not use, so
//! it is done here, once at the scan, in the reference's `double`
//! operations, and the shown integers are stored.

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
    /// The gauge total ([`gauge_total`]), truncated.
    pub total: i32,
    /// The MAIN tempo in whole BPM truncated, 0 when no note weighs
    /// anything (a chart of mines alone), as the reference's song
    /// information leaves it; `None` when it was unavailable (its histogram
    /// was not allocated).
    pub main_bpm: Option<i32>,
}

/// `x` as Java's `(int)` of a `double` makes it: truncated toward 0,
/// saturating at the ends of `i32`, NaN 0, which Rust's cast is too.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the reference's (int) of a double, which this cast is"
)]
const fn java_int(x: f64) -> i32 {
    x as i32
}

/// `n` as a `double`, exact below 2^53.
#[expect(
    clippy::cast_precision_loss,
    reason = "note counts are far below 2^53, where f64 is exact"
)]
const fn double(n: u64) -> f64 {
    n as f64
}

/// The whole part and the hundredths of `x` as the reference shows them,
/// `(int) x` and `((int)(x · 100)) % 100`.
fn shown(x: f64) -> (i32, i32) {
    (java_int(x), java_int(x * 100.0) % 100)
}

/// The exact decimal text of `v` units of `10^-18` ([`HeaderValue::ONE`]):
/// its integer part, then its 18 fractional digits with trailing zeros
/// trimmed.
fn decimal_text(v: i128) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let magnitude = v.unsigned_abs();
    let one = HeaderValue::ONE.unsigned_abs();
    let whole = magnitude / one;
    let fraction = magnitude % one;
    if fraction == 0 {
        return format!("{sign}{whole}");
    }
    let digits = format!("{fraction:018}");
    format!("{sign}{whole}.{}", digits.trim_end_matches('0'))
}

/// The gauge total the reference's song information reads
/// (`song/SongData.java` validates the model with
/// `play/BMSPlayerRule.java:68-80` before building it): the `#TOTAL` the
/// reference decoder reads, a number above 0
/// ([`HeaderPolicy::DefExRankFirst`]), as the nearest `double` to its
/// exact value, which is what `Double.parseDouble` gives for its text,
/// since Rust's parse rounds correctly too; else `max(260, 7.605 n / (0.01
/// n + 6.5))` in the reference's order, for the chart's `notes`.
///
/// A `#TOTAL` written with more than 18 fractional digits is held to
/// `10^-18` by the loader's canonical form, so its `double` can differ in
/// the last place from the reference's.
#[must_use]
pub fn gauge_total(headers: &[JudgmentHeader], notes: u32) -> f64 {
    let declared = HeaderPolicy::DefExRankFirst
        .total_header(headers)
        .and_then(HeaderValue::number)
        .and_then(|v| decimal_text(v).parse::<f64>().ok());
    declared.unwrap_or_else(|| {
        let n = f64::from(notes);
        f64::max(260.0, 7.605 * n / (0.01 * n + 6.5))
    })
}

/// The border's start for `notes` notes under `total`: `(int)(N · (1 −
/// 100 / total))`, as the reference starts its count.
#[must_use]
pub fn border_start(notes: u32, total: f64) -> i64 {
    i64::from(java_int(f64::from(notes) * (1.0 - 100.0 / total)))
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
        let mut total = 0.0;
        let info = ChartInfo::analyze(profile, |notes| {
            total = gauge_total(headers, notes);
            border_start(notes, total)
        });
        let count = |n: u32| i32::try_from(n).unwrap_or(i32::MAX);
        let mean = |notes: u64, seconds: u32| {
            if seconds == 0 {
                0.0
            } else {
                double(notes) / f64::from(seconds)
            }
        };
        let (density_end, density_end_hundredths) = shown(mean(info.end_notes, info.end_seconds));
        let (density_average, density_average_hundredths) =
            shown(mean(info.average_notes, info.average_seconds));
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
            total: java_int(total),
            main_bpm: main
                .map(|m| m.map_or(0, |t| i32::try_from(t.units() / one).unwrap_or(i32::MAX))),
        }
    }
}

#[cfg(test)]
mod tests;
