//! The chart's timeline: one immutable row per retained source position and
//! STOP end, each with its time, display position and exact original tempo
//! (design.md "Chart Model", timeline identity and tempo).
//!
//! Rows are ordinals: a note's endpoints and each bar line name the row
//! they sit on, and rows are never merged, even when two share both their
//! time and their position. Times are the playback µs at the speed the chart
//! was loaded at; positions and tempos are the chart's own, the same at
//! every speed.

use std::sync::Arc;

use crate::{Position, Speed, TimeUs};

/// Most rows a timeline holds: two per object (its position and a STOP's
/// end) and one per bar line at the loader's default limits, with a spare.
pub const MAX_TIMELINE_POINTS: usize = 1_001_001;

/// A row of a [`ChartTimeline`], by its index in [`ChartTimeline::points`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimelineId(pub u32);

impl TimelineId {
    /// The row's index.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// The rows a note's endpoints sit on: its head and, for a long note, its
/// end; a tap's or a mine's two are one row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NoteAnchors {
    /// The head's row.
    pub head: TimelineId,
    /// The end's row.
    pub end: TimelineId,
}

/// An exact original tempo: a BPM in units of `10^-18`, as the chart wrote
/// it and before any practice speed, from 0.000001 to below `10^12` BPM
/// (design.md "Chart Time").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tempo(u128);

impl Tempo {
    /// Units per BPM.
    pub const UNITS_PER_BPM: u128 = 1_000_000_000_000_000_000;
    /// The slowest tempo, 0.000001 BPM.
    pub const MIN: Self = Self(1_000_000_000_000);
    /// The fastest tempo, `10^12` BPM less one unit.
    pub const MAX: Self = Self(1_000_000_000_000_000_000_000_000_000_000 - 1);

    /// The tempo of `units` × `10^-18` BPM, if it is in range.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn from_units(units: u128) -> Option<Self> {
        if units >= Self::MIN.0 && units <= Self::MAX.0 {
            Some(Self(units))
        } else {
            None
        }
    }

    /// The tempo of `milli` thousandths of a BPM, if it is in range.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn from_milli(milli: i64) -> Option<Self> {
        if milli <= 0 {
            return None;
        }
        // At most 2^63 × 10^15, below 2^113.
        Self::from_units(milli.unsigned_abs() as u128 * 1_000_000_000_000_000)
    }

    /// The tempo in units of `10^-18` BPM.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn units(self) -> u128 {
        self.0
    }

    /// The tempo in BPM as a reduced fraction `numer / denom` of two `u64`,
    /// or `None` when the numerator does not fit: the checked boundary for
    /// arithmetic on `u64` ratios. The denominator always fits, since it
    /// divides `10^18`. A tempo that does not fit has no approximation:
    /// what needs it is unavailable.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn ratio(self) -> Option<(u64, u64)> {
        let g = gcd(self.0, Self::UNITS_PER_BPM);
        let numer = self.0 / g;
        let denom = Self::UNITS_PER_BPM / g;
        if numer > u64::MAX as u128 {
            return None;
        }
        Some((numer as u64, denom as u64))
    }
}

#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
const fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// One row of the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimelinePoint {
    /// The row's time in playback µs: the exact time mapped to the chart's
    /// speed, then floored once.
    pub at: TimeUs,
    /// The row's display position: its exact beat count floored once.
    pub pos: Position,
    /// The original tempo from this row on, a change at its position
    /// applied; a STOP keeps it.
    pub tempo: Tempo,
}

/// The chart's tempo metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TempoMeta {
    /// The tempo before time 0, the `#BPM` header's, kept even when a
    /// change at time 0 replaces it at once.
    pub initial: Tempo,
    /// The slowest of the initial tempo and every row's.
    pub min: Tempo,
    /// The fastest of the initial tempo and every row's.
    pub max: Tempo,
    /// The practice speed the rows' times are at. The tempos are
    /// original: a playback tempo applies this speed once.
    pub speed: Speed,
}

/// A change of the tempo in effect, from [`ChartTimeline::tempo_events`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TempoEvent {
    /// Its time in playback µs.
    pub at: TimeUs,
    /// The original tempo from then on.
    pub tempo: Tempo,
    /// The row it comes from: the last row at `at`.
    pub id: TimelineId,
}

/// Why a timeline was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineError {
    /// The timeline has no row, or more than [`MAX_TIMELINE_POINTS`].
    PointCount {
        /// Rows given.
        count: usize,
    },
    /// More bar lines than [`MAX_TIMELINE_POINTS`].
    BarCount {
        /// Bar lines given.
        count: usize,
    },
    /// The first row is not at time 0 and position 0.
    Origin,
    /// A row's time comes before the previous row's.
    TimeOrder {
        /// Index of the row.
        index: usize,
    },
    /// A row's position comes before the previous row's.
    PositionOrder {
        /// Index of the row.
        index: usize,
    },
    /// A bar line names a row the timeline does not hold, or one before the
    /// previous bar line's.
    BarRow {
        /// Index of the bar line.
        index: usize,
    },
    /// New times were given for another number of rows.
    RemapLength,
}

impl std::fmt::Display for TimelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::PointCount { count } => write!(
                f,
                "the timeline has {count} rows, not 1 to {MAX_TIMELINE_POINTS}"
            ),
            Self::BarCount { count } => write!(
                f,
                "the timeline has {count} bar lines, more than {MAX_TIMELINE_POINTS}"
            ),
            Self::Origin => f.write_str("the first timeline row is not at time 0 and position 0"),
            Self::TimeOrder { index } => write!(f, "timeline row {index} is out of time order"),
            Self::PositionOrder { index } => {
                write!(f, "timeline row {index} is out of position order")
            }
            Self::BarRow { index } => write!(f, "bar line {index} names no row in order"),
            Self::RemapLength => f.write_str("new row times do not match the rows"),
        }
    }
}

impl std::error::Error for TimelineError {}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TimelineData {
    points: Box<[TimelinePoint]>,
    bar_ids: Box<[TimelineId]>,
    meta: TempoMeta,
}

/// The chart's timeline: its rows, the rows of its bar lines in order (one
/// per [`Chart::bar_lines`](crate::Chart::bar_lines) entry) and its tempo
/// metadata. Immutable: clones share one allocation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChartTimeline(Arc<TimelineData>);

/// Checks the rows' order and range.
fn check_times<I: Iterator<Item = TimeUs>>(mut times: I) -> Result<(), TimelineError> {
    if times.next() != Some(TimeUs(0)) {
        return Err(TimelineError::Origin);
    }
    let mut prev = TimeUs(0);
    for (i, at) in times.enumerate() {
        if at < prev {
            return Err(TimelineError::TimeOrder { index: i + 1 });
        }
        prev = at;
    }
    Ok(())
}

impl ChartTimeline {
    /// The timeline of `points`, with the bar lines on `bar_ids`, the tempo
    /// `initial` before time 0 and the rows' times at `speed`. The extrema
    /// are computed from the initial tempo and the rows. The vectors become
    /// the timeline's storage without a copy when they are at their exact
    /// size.
    ///
    /// # Errors
    ///
    /// A [`TimelineError`]: a count out of range, a first row not at time 0
    /// and position 0, times or positions that decrease, or bar rows out
    /// of range or decreasing. A chart built in code may hold times past
    /// the scroll map's domain, where its positions are clamped; the
    /// loader's never do.
    pub fn new(
        points: Vec<TimelinePoint>,
        bar_ids: Vec<TimelineId>,
        initial: Tempo,
        speed: Speed,
    ) -> Result<Self, TimelineError> {
        if points.is_empty() || points.len() > MAX_TIMELINE_POINTS {
            return Err(TimelineError::PointCount {
                count: points.len(),
            });
        }
        if bar_ids.len() > MAX_TIMELINE_POINTS {
            return Err(TimelineError::BarCount {
                count: bar_ids.len(),
            });
        }
        check_times(points.iter().map(|p| p.at))?;
        if points[0].pos != Position(0) {
            return Err(TimelineError::Origin);
        }
        if let Some(i) = points.windows(2).position(|w| w[1].pos < w[0].pos) {
            return Err(TimelineError::PositionOrder { index: i + 1 });
        }
        let mut prev = TimelineId(0);
        for (index, &id) in bar_ids.iter().enumerate() {
            if id.index() >= points.len() || id < prev {
                return Err(TimelineError::BarRow { index });
            }
            prev = id;
        }
        let (mut min, mut max) = (initial, initial);
        for p in &points {
            min = min.min(p.tempo);
            max = max.max(p.tempo);
        }
        Ok(Self(Arc::new(TimelineData {
            points: points.into_boxed_slice(),
            bar_ids: bar_ids.into_boxed_slice(),
            meta: TempoMeta {
                initial,
                min,
                max,
                speed,
            },
        })))
    }

    /// Replaces the rows' times with `times`, the same rows at `speed`:
    /// positions, tempos and bar rows stay. The rows are rewritten in place
    /// unless a clone shares them.
    ///
    /// # Errors
    ///
    /// [`TimelineError::RemapLength`] for another number of times, and the
    /// time errors of [`new`](Self::new); the timeline is unchanged then.
    pub fn remap(&mut self, times: &[TimeUs], speed: Speed) -> Result<(), TimelineError> {
        if times.len() != self.0.points.len() {
            return Err(TimelineError::RemapLength);
        }
        check_times(times.iter().copied())?;
        let data = Arc::make_mut(&mut self.0);
        for (p, &at) in data.points.iter_mut().zip(times) {
            p.at = at;
        }
        data.meta.speed = speed;
        Ok(())
    }

    /// The rows, in identity order: times and positions never decrease.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn points(&self) -> &[TimelinePoint] {
        &self.0.points
    }

    /// The row `id`, if the timeline holds it.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn point(&self, id: TimelineId) -> Option<&TimelinePoint> {
        self.0.points.get(id.index())
    }

    /// The rows of the chart's bar lines, in bar-line order.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn bar_ids(&self) -> &[TimelineId] {
        &self.0.bar_ids
    }

    /// The tempo metadata.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn meta(&self) -> &TempoMeta {
        &self.0.meta
    }

    /// The original tempo in effect at `at` playback µs: the last row at or
    /// before it, so of rows sharing a µs the last in identity order wins;
    /// before time 0, the initial tempo.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn tempo_at(&self, at: TimeUs) -> Tempo {
        let count = self.0.points.partition_point(|p| p.at <= at);
        count
            .checked_sub(1)
            .and_then(|i| self.0.points.get(i))
            .map_or(self.0.meta.initial, |p| p.tempo)
    }

    /// The changes of the tempo in effect, in time order, starting from the
    /// initial tempo: one per µs at which the tempo after its last row
    /// differs from the tempo before it. Allocates nothing.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn tempo_events(&self) -> TempoEvents<'_> {
        TempoEvents {
            rest: &self.0.points,
            next_id: 0,
            current: self.0.meta.initial,
        }
    }
}

/// The iterator of [`ChartTimeline::tempo_events`].
#[derive(Clone, Debug)]
pub struct TempoEvents<'a> {
    rest: &'a [TimelinePoint],
    next_id: u32,
    current: Tempo,
}

impl Iterator for TempoEvents<'_> {
    type Item = TempoEvent;

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn next(&mut self) -> Option<TempoEvent> {
        while let Some(first) = self.rest.first() {
            let len = self.rest.iter().take_while(|p| p.at == first.at).count();
            let (bucket, rest) = self.rest.split_at_checked(len)?;
            self.rest = rest;
            let last = bucket.last()?;
            // At most `MAX_TIMELINE_POINTS` rows, so this fits.
            let count = u32::try_from(len).ok()?;
            let id = TimelineId(self.next_id + count - 1);
            self.next_id += count;
            if last.tempo != self.current {
                self.current = last.tempo;
                return Some(TempoEvent {
                    at: last.at,
                    tempo: last.tempo,
                    id,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChartTimeline, MAX_TIMELINE_POINTS, Tempo, TempoEvent, TimelineError, TimelineId,
        TimelinePoint,
    };
    use crate::{Position, Speed, TimeUs};

    fn bpm(n: u128) -> Tempo {
        Tempo::from_units(n * Tempo::UNITS_PER_BPM).unwrap()
    }

    fn row(at: i64, pos: i128, tempo: u128) -> TimelinePoint {
        TimelinePoint {
            at: TimeUs(at),
            pos: Position(pos),
            tempo: bpm(tempo),
        }
    }

    #[test]
    fn row_layouts_are_the_charged_sizes() {
        // The loader's construction charge counts 48 bytes a row, 4 a bar
        // row and 8 a mapped time (design.md "Chart Model", "Timeline").
        assert_eq!(size_of::<TimelinePoint>(), 48);
        assert_eq!(size_of::<TimelineId>(), 4);
        assert_eq!(size_of::<TimeUs>(), 8);
        assert_eq!(size_of::<ChartTimeline>(), size_of::<usize>());
    }

    #[test]
    fn tempo_range_and_ratio() {
        assert_eq!(Tempo::from_units(999_999_999_999), None);
        assert_eq!(Tempo::from_units(10_u128.pow(12)), Some(Tempo::MIN));
        assert_eq!(Tempo::from_units(10_u128.pow(30)), None);
        assert_eq!(Tempo::from_milli(0), None);
        assert_eq!(Tempo::from_milli(150_500).unwrap().ratio(), Some((301, 2)));
        assert_eq!(bpm(150).ratio(), Some((150, 1)));
        // 150.000000000000000001 BPM: 150 × 10^18 + 1 over 10^18 does not
        // fit `u64`, so it has no ratio rather than a rounded one.
        let fine = Tempo::from_units(150 * Tempo::UNITS_PER_BPM + 1).unwrap();
        assert_eq!(fine.ratio(), None);
        assert_eq!(Tempo::MIN.ratio(), Some((1, 1_000_000)));
    }

    #[test]
    fn rows_are_checked_before_storage() {
        let new = |points: Vec<TimelinePoint>, bars: Vec<u32>| {
            ChartTimeline::new(
                points,
                bars.into_iter().map(TimelineId).collect(),
                bpm(120),
                Speed::NORMAL,
            )
            .map(|_| ())
        };
        assert_eq!(
            new(vec![], vec![]),
            Err(TimelineError::PointCount { count: 0 })
        );
        assert_eq!(
            new(vec![row(1, 0, 120)], vec![]),
            Err(TimelineError::Origin)
        );
        assert_eq!(
            new(vec![row(0, 1, 120)], vec![]),
            Err(TimelineError::Origin)
        );
        assert_eq!(
            new(vec![row(0, 0, 120), row(5, 5, 120), row(4, 6, 120)], vec![]),
            Err(TimelineError::TimeOrder { index: 2 })
        );
        assert_eq!(
            new(vec![row(0, 0, 120), row(5, 5, 120), row(6, 4, 120)], vec![]),
            Err(TimelineError::PositionOrder { index: 2 })
        );
        let two = || vec![row(0, 0, 120), row(5, 5, 120)];
        assert_eq!(new(two(), vec![0, 1, 1]), Ok(()));
        assert_eq!(
            new(two(), vec![1, 0]),
            Err(TimelineError::BarRow { index: 1 })
        );
        assert_eq!(new(two(), vec![2]), Err(TimelineError::BarRow { index: 0 }));
        // Rows that share their time and position stay distinct.
        assert_eq!(new(vec![row(0, 0, 120), row(0, 0, 120)], vec![]), Ok(()));
        let full = vec![row(0, 0, 120); MAX_TIMELINE_POINTS];
        assert_eq!(new(full.clone(), vec![]), Ok(()));
        let mut over = full;
        over.push(row(0, 0, 120));
        assert_eq!(
            new(over, vec![]),
            Err(TimelineError::PointCount {
                count: MAX_TIMELINE_POINTS + 1
            })
        );
    }

    #[test]
    fn the_initial_tempo_and_extrema_survive_a_change_at_zero() {
        // 150 BPM, 300 at time 0, a STOP at 400 ms keeping 300, 75 later.
        let points = vec![
            row(0, 0, 300),
            row(400_000, 10, 300),
            row(800_000, 10, 300),
            row(900_000, 20, 75),
        ];
        let t = ChartTimeline::new(points, vec![], bpm(150), Speed::NORMAL).unwrap();
        let meta = t.meta();
        assert_eq!(
            (meta.initial, meta.min, meta.max),
            (bpm(150), bpm(75), bpm(300))
        );
        assert_eq!(t.tempo_at(TimeUs(-1)), bpm(150));
        assert_eq!(t.tempo_at(TimeUs(0)), bpm(300));
        // A STOP is no zero tempo.
        assert_eq!(t.tempo_at(TimeUs(600_000)), bpm(300));
        assert_eq!(t.tempo_at(TimeUs(900_000)), bpm(75));
        assert_eq!(t.tempo_at(TimeUs(i64::MAX)), bpm(75));
        let events: Vec<TempoEvent> = t.tempo_events().collect();
        assert_eq!(
            events,
            [
                TempoEvent {
                    at: TimeUs(0),
                    tempo: bpm(300),
                    id: TimelineId(0)
                },
                TempoEvent {
                    at: TimeUs(900_000),
                    tempo: bpm(75),
                    id: TimelineId(3)
                },
            ]
        );
    }

    #[test]
    fn rows_sharing_a_us_coalesce_later_wins_for_tempo() {
        // Two rows at 10 µs: 90 then 180; the later in identity order wins,
        // and a µs whose rows end where they began makes no event.
        let points = vec![
            row(0, 0, 120),
            row(10, 1, 90),
            row(10, 2, 180),
            row(20, 3, 60),
            row(20, 4, 180),
        ];
        let t = ChartTimeline::new(points, vec![], bpm(120), Speed::NORMAL).unwrap();
        assert_eq!(t.tempo_at(TimeUs(10)), bpm(180));
        assert_eq!(t.point(TimelineId(1)).unwrap().tempo, bpm(90));
        assert_eq!(t.point(TimelineId(5)), None);
        let events: Vec<(i64, Tempo, u32)> = t
            .tempo_events()
            .map(|e| (e.at.0, e.tempo, e.id.0))
            .collect();
        assert_eq!(events, [(10, bpm(180), 2)]);
    }

    #[test]
    fn remap_rewrites_times_only_and_in_place() {
        let points = vec![row(0, 0, 150), row(1_000, 5, 150), row(2_001, 9, 300)];
        let mut t =
            ChartTimeline::new(points, vec![TimelineId(1)], bpm(150), Speed::NORMAL).unwrap();
        let before = t.points().as_ptr();
        let half = Speed::SLOWEST;
        assert_eq!(t.remap(&[TimeUs(0)], half), Err(TimelineError::RemapLength));
        assert_eq!(
            t.remap(&[TimeUs(0), TimeUs(5), TimeUs(4)], half),
            Err(TimelineError::TimeOrder { index: 2 })
        );
        assert_eq!(t.meta().speed, Speed::NORMAL);
        t.remap(&[TimeUs(0), TimeUs(2_000), TimeUs(4_002)], half)
            .unwrap();
        assert_eq!(t.points().as_ptr(), before);
        assert_eq!(t.points()[2], row(4_002, 9, 300));
        assert_eq!(t.bar_ids(), [TimelineId(1)]);
        assert_eq!(t.meta().speed, half);
        assert_eq!(t.meta().max, bpm(300));
        // A shared timeline is copied, never changed under its other owner.
        let shared = t.clone();
        t.remap(&[TimeUs(0), TimeUs(1), TimeUs(2)], Speed::NORMAL)
            .unwrap();
        assert_eq!(shared.points()[2].at, TimeUs(4_002));
        assert_ne!(t.points().as_ptr(), shared.points().as_ptr());
    }
}
