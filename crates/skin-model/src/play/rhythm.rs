//! The rhythm clock behind the `rhythm` timer: the time since the latest
//! bar line, normalized to 1 000 ms per chart beat at the tempo in force,
//! so it runs through a STOP at the tempo the STOP holds and restarts at
//! each bar line (stage3.md R3, the play timers).
//!
//! For a frame at `now`, with `b` the latest bar row at or before it (row
//! 0 before the first) and `k` the last row at or before it, the value in
//! µs is `⌊(Σ_{i=b}^{k-1} (at_{i+1} − at_i)·tempo_i + (now − at_k)·tempo_k)
//! · s / (1 200 · 10¹⁸)⌋`: times in playback µs, tempos in
//! [`Tempo`] units, `s` the practice speed's twentieths (the rows' times
//! are at that speed, so the speed applies once). A cursor keeps the sum up
//! to `k`, so a frame walks only the rows it passes and allocates nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use bms_model::{ChartTimeline, Tempo, TimeUs, TimelineId, TimelinePoint};

/// How long after the attempt's progress end the completed tail keeps the
/// clock at the tempo: the source's play state runs to its last note plus
/// `TIME_MARGIN` (`play/BMSPlayer.java:66, 199` at `0.8.9`).
pub(crate) const TAIL_US: i64 = 5_000_000;

/// The divisor of the clock's rate: 20 twentieths, 60 s a minute, and
/// [`Tempo::UNITS_PER_BPM`].
const DIVISOR: u128 = 1_200 * Tempo::UNITS_PER_BPM;

/// Where the clock stood at the last frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cursor {
    /// The next bar line to pass, an index into the bar rows.
    bar: usize,
    /// The last row at or before the last frame's `now`.
    row: usize,
    /// `Σ (at_{i+1} − at_i)·tempo_i` from the latest bar row to `row`;
    /// `None` once it overflowed, until the next bar line.
    acc: Option<u128>,
}

/// The rhythm clock of one attempt: its speed and its cursor.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rhythm {
    /// The practice speed's twentieths, which the rows' times are at.
    speed: u128,
    cursor: Option<Cursor>,
}

impl Rhythm {
    /// A clock at the speed of `twentieths`/20, with no cursor yet.
    pub(crate) fn new(twentieths: u32) -> Self {
        Self {
            speed: u128::from(twentieths),
            cursor: None,
        }
    }

    /// Drops the cursor: the next frame finds its place afresh.
    pub(crate) fn clear(&mut self) {
        self.cursor = None;
    }

    /// The clock's value at `now` in µs over `timeline`, `None` before the
    /// first row or where a product overflows. A frame whose `now` is
    /// before the cursor's row (a rearmed READY, whose clock restarts)
    /// finds the latest bar line again by binary search.
    pub(crate) fn value(&mut self, timeline: &ChartTimeline, now: TimeUs) -> Option<i64> {
        let points = timeline.points();
        let bars = timeline.bar_ids();
        let cursor = match self.cursor {
            Some(c) if points.get(c.row).is_some_and(|p| p.at <= now) => c,
            _ => place(points, bars, now)?,
        };
        let cursor = advance(points, bars, cursor, now);
        self.cursor = Some(cursor);
        let row = points.get(cursor.row)?;
        let tail = u128::from(now.0.checked_sub(row.at.0)?.unsigned_abs())
            .checked_mul(row.tempo.units())?;
        let total = cursor.acc?.checked_add(tail)?.checked_mul(self.speed)?;
        i64::try_from(total / DIVISOR).ok()
    }
}

/// The cursor at `now`: the last row at or before it, the latest bar row
/// at or before it (row 0 before the first) and the sum between them.
/// `None` before the first row.
fn place(points: &[TimelinePoint], bars: &[TimelineId], now: TimeUs) -> Option<Cursor> {
    let row = points.partition_point(|p| p.at <= now).checked_sub(1)?;
    // Bar rows never decrease, nor do their times.
    let bar = bars.partition_point(|&id| points.get(id.index()).is_some_and(|p| p.at <= now));
    let base = bar
        .checked_sub(1)
        .and_then(|i| bars.get(i))
        .map_or(0, |id| id.index());
    let mut acc = Some(0_u128);
    for i in base..row {
        acc = acc.and_then(|a| a.checked_add(span(points, i)?));
    }
    Some(Cursor { bar, row, acc })
}

/// `cursor` moved on to the last row at or before `now`, the sum restarting
/// at each bar row it passes.
fn advance(
    points: &[TimelinePoint],
    bars: &[TimelineId],
    mut cursor: Cursor,
    now: TimeUs,
) -> Cursor {
    while points
        .get(cursor.row.saturating_add(1))
        .is_some_and(|p| p.at <= now)
    {
        cursor.acc = cursor
            .acc
            .and_then(|a| a.checked_add(span(points, cursor.row)?));
        cursor.row += 1;
        while bars
            .get(cursor.bar)
            .is_some_and(|id| id.index() <= cursor.row)
        {
            cursor.bar += 1;
            cursor.acc = Some(0);
        }
    }
    cursor
}

/// Row `i`'s share of the sum: `(at_{i+1} − at_i)·tempo_i`; `None` past
/// the last row or on overflow.
fn span(points: &[TimelinePoint], i: usize) -> Option<u128> {
    let (a, b) = (points.get(i)?, points.get(i.checked_add(1)?)?);
    let d = b.at.0.checked_sub(a.at.0)?;
    u128::from(d.unsigned_abs()).checked_mul(a.tempo.units())
}
