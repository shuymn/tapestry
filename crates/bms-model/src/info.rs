//! What a song list previews of a chart's notes before it is chosen
//! (design.md "Loading and Song Selection", Summaries): the counts by
//! kind and the integer parts of the densities, from the chart's
//! [`ChartProfile`]. Dividing them into the shown decimals takes floating
//! point, which this crate does not use; the library does it.

use crate::ChartProfile;
use crate::profile::{
    KEY_LONG_HEAD, KEY_TAP, MINE, NOTE_CATEGORIES, SCRATCH_LONG_HEAD, SCRATCH_TAP,
};

/// The widest window [`ChartInfo::end_seconds`] spans.
pub const END_WINDOW_SECONDS: u32 = 5;

/// A chart's note counts and the integer parts of its densities, over its
/// profile's 1× seconds (`len` of them, [`ChartProfile::seconds`]). A
/// second's notes are its taps, long-note heads and long-note bodies,
/// never its mines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChartInfo {
    /// Key taps.
    pub key_taps: u32,
    /// Key long notes, each once.
    pub key_longs: u32,
    /// Scratch taps.
    pub scratch_taps: u32,
    /// Scratch long notes, each once.
    pub scratch_longs: u32,
    /// The notes of the chart, `N`: every tap and long note once, no mine.
    pub notes: u32,
    /// The most notes in any second.
    pub peak: u64,
    /// The notes of the seconds holding at least `N / len / 4` notes (in
    /// integer division) ...
    pub average_notes: u64,
    /// ... and how many such seconds there are.
    pub average_seconds: u32,
    /// The second in which a count from the border start reaches 0, the
    /// count falling by one for each tap, long-note head and mine of a
    /// second in turn; 0 when it never reaches 0.
    pub border_second: u32,
    /// The most notes in a window of [`end_seconds`](Self::end_seconds)
    /// seconds starting at or after `border_second` and ending before the
    /// last second ...
    pub end_notes: u64,
    /// ... and the window's width, `min(5, len − border_second − 1)`, or 0
    /// (with `end_notes` 0) when that is not positive.
    pub end_seconds: u32,
}

/// The notes of `second`: every category but mines.
fn notes_of(second: &[u32; NOTE_CATEGORIES]) -> u64 {
    second
        .iter()
        .enumerate()
        .filter(|&(c, _)| c != MINE)
        .map(|(_, &n)| u64::from(n))
        .sum()
}

/// What counts down the border in `second`: its taps, long-note heads and
/// mines, never its bodies.
fn counted_of(second: &[u32; NOTE_CATEGORIES]) -> u64 {
    [SCRATCH_LONG_HEAD, SCRATCH_TAP, KEY_LONG_HEAD, KEY_TAP, MINE]
        .iter()
        .filter_map(|&c| second.get(c))
        .map(|&n| u64::from(n))
        .sum()
}

/// The sum of category `c` over `seconds`, saturating at `u32::MAX`.
fn total_of(seconds: &[[u32; NOTE_CATEGORIES]], c: usize) -> u32 {
    let sum: u64 = seconds
        .iter()
        .filter_map(|s| s.get(c))
        .map(|&n| u64::from(n))
        .sum();
    u32::try_from(sum).unwrap_or(u32::MAX)
}

impl ChartInfo {
    /// The info of `profile`, the border starting at `border_start(N)`
    /// for the chart's `N` notes: the caller's reading of the chart's
    /// gauge total, which takes floating point.
    ///
    /// The source analyses a chart only up to 6 hours; the loader's own
    /// 60-minute cap (design.md "Loading Limits") is far below it, so
    /// every loaded chart has its info.
    #[must_use]
    pub fn analyze(profile: &ChartProfile, border_start: impl FnOnce(u32) -> i64) -> Self {
        let seconds = profile.seconds();
        let key_taps = total_of(seconds, KEY_TAP);
        let key_longs = total_of(seconds, KEY_LONG_HEAD);
        let scratch_taps = total_of(seconds, SCRATCH_TAP);
        let scratch_longs = total_of(seconds, SCRATCH_LONG_HEAD);
        let notes = [key_taps, key_longs, scratch_taps, scratch_longs]
            .into_iter()
            .fold(0_u32, u32::saturating_add);

        let len = u64::try_from(seconds.len()).unwrap_or(u64::MAX).max(1);
        let floor = u64::from(notes) / len / 4;
        let (mut peak, mut average_notes, mut average_seconds) = (0_u64, 0_u64, 0_u32);
        for s in seconds {
            let n = notes_of(s);
            peak = peak.max(n);
            if n >= floor {
                average_notes += n;
                average_seconds = average_seconds.saturating_add(1);
            }
        }

        let mut remaining = border_start(notes);
        let mut border_second = 0_usize;
        for (i, s) in seconds.iter().enumerate() {
            let counted = i64::try_from(counted_of(s)).unwrap_or(i64::MAX);
            // The count reaches exactly 0 within this second.
            if remaining >= 1 && remaining <= counted {
                border_second = i;
                break;
            }
            remaining = remaining.saturating_sub(counted);
        }

        let width = seconds
            .len()
            .saturating_sub(border_second)
            .saturating_sub(1)
            .min(END_WINDOW_SECONDS as usize);
        let end_notes = if width == 0 {
            0
        } else {
            // The windows start before `len − width`, so none takes in
            // the last second.
            let last = seconds.len().saturating_sub(1);
            let tail = seconds.get(border_second..last).unwrap_or_default();
            let windows = tail.windows(width);
            windows
                .map(|w| w.iter().map(notes_of).sum::<u64>())
                .max()
                .unwrap_or(0)
        };
        Self {
            key_taps,
            key_longs,
            scratch_taps,
            scratch_longs,
            notes,
            peak,
            average_notes,
            average_seconds,
            border_second: u32::try_from(border_second).unwrap_or(u32::MAX),
            end_notes,
            end_seconds: u32::try_from(width).unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChartInfo;
    use crate::{Chart, ChartBuilder, ChartProfile, ChartSummary, Lane, TimeUs};

    const SECOND: i64 = 1_000_000;

    fn lane(i: u8) -> Lane {
        Lane::new(i).unwrap()
    }

    /// A chart whose second `s` holds `taps[s]` key taps, spread over the
    /// seven keys 1 ms apart, at 150 BPM.
    fn taps(taps: &[u32]) -> ChartBuilder {
        let mut b = ChartBuilder::new(150_000);
        for (s, &n) in taps.iter().enumerate() {
            let s = i64::try_from(s).unwrap();
            for j in 0..i64::from(n) {
                let key = lane(1 + u8::try_from(j % 7).unwrap());
                b = b.tap(key, TimeUs(s * SECOND + (j / 7) * 100));
            }
        }
        b
    }

    fn profile(b: ChartBuilder) -> ChartProfile {
        let mut chart: Chart = b.build().unwrap();
        chart.meta.length = ChartSummary::of(&chart).length;
        ChartProfile::of(&chart, &[])
    }

    /// The info of `b`, asserting its `N` is `n`, the border starting at
    /// `start`.
    fn info(b: ChartBuilder, n: u32, start: i64) -> ChartInfo {
        ChartInfo::analyze(&profile(b), |notes| {
            assert_eq!(notes, n);
            start
        })
    }

    /// Fixture A: seconds 5, 13, 11 and 10 over five seconds, the border
    /// starting at 19 (`#TOTAL 200`): the most is 13, the average 39 over
    /// four seconds, the border reached in second 2 and the end window
    /// the two seconds from there, 21.
    #[test]
    fn a_base_chart() {
        let i = info(taps(&[5, 13, 11, 10]), 39, 19);
        assert_eq!((i.key_taps, i.notes, i.peak), (39, 39, 13));
        assert_eq!((i.average_notes, i.average_seconds), (39, 4));
        assert_eq!(i.border_second, 2);
        assert_eq!((i.end_notes, i.end_seconds), (21, 2));
    }

    /// Fixture G: the average's floor is `N / len / 4` in integer
    /// division, 19 / 5 / 4 = 0, so all five seconds count, the empty
    /// ones too.
    #[test]
    fn the_average_floor_is_an_integer_division() {
        let i = info(taps(&[10, 0, 8, 1]), 19, 0);
        assert_eq!((i.average_notes, i.average_seconds), (19, 5));
    }

    /// Fixture E: mines count toward no density, nor `N`.
    #[test]
    fn mines_are_not_notes() {
        let b = taps(&[4, 2])
            .mine(lane(5), TimeUs(500_000), 1)
            .mine(lane(6), TimeUs(500_000), 1)
            .mine(lane(7), TimeUs(500_000), 1);
        let i = info(b, 6, 0);
        assert_eq!(i.peak, 4);
        assert_eq!((i.average_notes, i.average_seconds), (6, 3));
    }

    /// Mines do count down the border, as taps and heads do: two taps and
    /// three mines in second 0 reach a border of 4 there; without the
    /// mines it would be second 1.
    #[test]
    fn mines_count_down_the_border() {
        let b = taps(&[2, 5])
            .mine(lane(5), TimeUs(500_000), 1)
            .mine(lane(6), TimeUs(500_000), 1)
            .mine(lane(7), TimeUs(500_000), 1);
        assert_eq!(info(b, 7, 4).border_second, 0);
    }

    /// Fixture F: a long note counts in every second it spans, its head in
    /// its own: 3 taps and a long note from 0.5 s to 2.5 s give 4, 1, 1, 0.
    #[test]
    fn long_bodies_count_every_second() {
        let b = taps(&[3]).long(lane(7), TimeUs(500_000), TimeUs(2_500_000));
        let i = info(b, 4, 0);
        assert_eq!((i.key_taps, i.key_longs), (3, 1));
        assert_eq!(i.peak, 4);
        assert_eq!((i.average_notes, i.average_seconds), (6, 4));
    }

    /// Fixture W: the end window moves over every start from the border
    /// up to `len − width` (exclusive): seven seconds of 1 then five of
    /// 10, 13 seconds, the best window the five tens.
    #[test]
    fn the_end_window_moves_up_to_the_last_full_start() {
        let i = info(taps(&[1, 1, 1, 1, 1, 1, 1, 10, 10, 10, 10, 10]), 57, 0);
        assert_eq!(i.border_second, 0);
        assert_eq!((i.end_notes, i.end_seconds), (50, 5));
    }

    /// Fixture T345: a border start of 214 is reached in second 1 (213
    /// notes in second 0), so the window is four seconds from 1, 132; at
    /// 213 it is second 0 and five seconds, 345.
    #[test]
    fn the_border_start_picks_the_window() {
        let seconds = [213, 1, 50, 50, 31];
        let i = info(taps(&seconds), 345, 214);
        assert_eq!(i.border_second, 1);
        assert_eq!((i.end_notes, i.end_seconds), (132, 4));
        let i = info(taps(&seconds), 345, 213);
        assert_eq!(i.border_second, 0);
        assert_eq!((i.end_notes, i.end_seconds), (345, 5));
    }

    /// A border never reached (a start of 0 or below, or above every
    /// counted note) leaves second 0.
    #[test]
    fn an_unreached_border_is_second_zero() {
        for start in [0, -5, 40] {
            assert_eq!(info(taps(&[5, 13, 11, 10]), 39, start).border_second, 0);
        }
    }

    /// The window is never wider than the seconds after the border allow:
    /// reached in second 2 of four, `4 − 2 − 1 = 1`.
    #[test]
    fn the_end_window_narrows_at_the_end() {
        let i = info(taps(&[0, 0, 10]), 10, 1);
        assert_eq!(i.border_second, 2);
        assert_eq!((i.end_notes, i.end_seconds), (10, 1));
    }

    /// Scratch notes and long notes count apart from the keys': fixture B.
    #[test]
    fn counts_by_kind() {
        let mut b = taps(&[20]);
        for k in 0..4 {
            b = b.tap(Lane::SCRATCH, TimeUs(1_000_000 + k * 10_000));
        }
        b = b.long(Lane::SCRATCH, TimeUs(1_100_000), TimeUs(1_200_000));
        for k in 0..3 {
            b = b.long(lane(1 + k), TimeUs(2_000_000), TimeUs(2_100_000));
        }
        b = b
            .mine(lane(5), TimeUs(2_000_000), 1)
            .mine(lane(6), TimeUs(2_000_000), 1);
        let i = info(b, 28, 0);
        assert_eq!(
            (i.key_taps, i.key_longs, i.scratch_taps, i.scratch_longs),
            (20, 3, 4, 1)
        );
    }

    /// A second's notes are counted full width: 70 000 taps in one second.
    #[test]
    fn a_second_is_not_saturated() {
        let i = info(taps(&[70_000]), 70_000, 0);
        assert_eq!(i.peak, 70_000);
    }
}
