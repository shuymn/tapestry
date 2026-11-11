//! The exact presentation playhead (design.md "Chart Model", "Timeline";
//! "Rendering and Skins"): one prepared curve from the chart's timeline
//! that notes, bar lines and beat detection all sample, so a note reaches
//! the judgment line at exactly its judgment µs.
//!
//! Preparation, at load time, picks one representative row for every
//! stored µs of the timeline. Where eligible note endpoints sit at that
//! µs, the first of them by identity is the representative, and every
//! other row of that µs (BGM, bar lines, BPM changes, STOPs, ineligible
//! notes) yields to its position; elsewhere the µs's last row is. A
//! presentation-valid chart's endpoints at one µs share one position, so
//! the choice among them is no policy. A query between two
//! representatives interpolates exactly in integers; before the first and
//! after the last it extrapolates at the scroll map's initial and final
//! rates, saturating.
//!
//! The curve never decreases. Each representative is a row of its µs's
//! bucket, every row of an earlier bucket precedes every row of a later
//! one, and rows' positions never decrease in identity order, so
//! consecutive representatives' positions never decrease; interpolation
//! stays between them and meets each at its own time, and the rates are
//! not negative. A STOP's start and end rows share their position, so the
//! curve is flat across it unless an endpoint at another position shares
//! one of its µs, whose position that µs then takes.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{
    Chart, ChartTimeline, DOMAIN, LANE_COUNT, Note, NoteKind, Position, TimeUs, TimelineId,
    TimelinePoint,
};

use super::PrepareError;
use super::reducer::is_target;

/// The prepared playhead of one attempt: the chart's shared timeline, one
/// representative row per stored µs, and the extrapolation rates.
/// Immutable once prepared; a READY rearm keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExactPlayhead {
    timeline: ChartTimeline,
    /// One row per distinct stored µs, in time order.
    reps: Box<[TimelineId]>,
    /// Units per µs before the first representative: the scroll map's
    /// initial rate.
    initial_rate: i128,
    /// Units per µs after the last: the scroll map's final segment's.
    terminal_rate: i128,
}

/// A lane's next note endpoint in identity order.
#[derive(Clone, Copy, Default)]
struct Cursor {
    note: usize,
    /// Whether the note's head was passed: a long note's end comes next.
    end: bool,
}

impl ExactPlayhead {
    /// The playhead of an attempt on `chart` over `section` (its start and
    /// end, or the whole chart): the endpoints whose notes the attempt
    /// targets choose their µs's position. The chart's presentation is
    /// checked whole first, whatever the section, so a section never
    /// admits a chart the whole would refuse, and every row must lie in
    /// the scroll map's domain, where queries are clamped. Two passes over
    /// the rows: one counts the stored µs, the other fills that many
    /// representatives, walking the eight lanes' endpoints beside the rows
    /// with a cursor each.
    ///
    /// # Errors
    ///
    /// [`PrepareError::Presentation`] with the chart's reason
    /// (`Chart::validate_presentation`), and
    /// [`PrepareError::TimeOutOfDomain`] for the first row past the
    /// domain (`±DOMAIN` µs), which a chart built in code may hold and a
    /// loaded one never does; both before anything is allocated.
    pub fn prepare(chart: &Chart, section: Option<(TimeUs, TimeUs)>) -> Result<Self, PrepareError> {
        chart
            .validate_presentation()
            .map_err(PrepareError::Presentation)?;
        let timeline = chart.timeline.clone();
        let points = timeline.points();
        // Rows start at 0 and never decrease, so the first past the domain
        // is the first past its end.
        let inside = points.partition_point(|p| p.at.0 <= DOMAIN);
        if let Some(p) = points.get(inside) {
            return Err(PrepareError::TimeOutOfDomain {
                row: row_id(inside),
                at: p.at,
            });
        }
        let buckets = points.chunk_by(|a, b| a.at == b.at);
        let mut reps = Vec::with_capacity(buckets.clone().count());
        let mut cursors = [Cursor::default(); LANE_COUNT];
        let mut first = 0_usize;
        for bucket in buckets {
            let last = first + bucket.len() - 1;
            let mut chosen: Option<usize> = None;
            for (notes, cursor) in chart.lanes.iter().zip(&mut cursors) {
                // A lane's endpoints never decrease in identity (the links
                // are checked), and earlier buckets consumed every one up
                // to their last row, so each endpoint met here is in this
                // bucket.
                while let Some((id, note)) = endpoint(notes, *cursor) {
                    if id > last {
                        break;
                    }
                    if is_target(section, note.time, note.end()) && chosen.is_none_or(|c| id < c) {
                        chosen = Some(id);
                    }
                    *cursor = advance(note, *cursor);
                }
            }
            reps.push(row_id(chosen.unwrap_or(last)));
            first = last + 1;
        }
        let terminal_rate = chart
            .scroll
            .segments()
            .last()
            .map_or(chart.scroll.initial_rate(), |s| s.slope);
        Ok(Self {
            timeline,
            reps: reps.into_boxed_slice(),
            initial_rate: chart.scroll.initial_rate(),
            terminal_rate,
        })
    }

    /// The chart's timeline the playhead follows, shared with the chart.
    #[must_use]
    pub(crate) fn timeline(&self) -> &ChartTimeline {
        &self.timeline
    }

    /// The representative rows, one per stored µs, in time order.
    #[must_use]
    pub fn representatives(&self) -> &[TimelineId] {
        &self.reps
    }

    /// The bytes it holds of its own: itself and its representatives. The
    /// timeline is shared with the chart.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        size_of_val(&*self.reps)
    }

    /// The pixels-per-position basis: units per µs at the chart's initial
    /// rate (the scroll map's).
    #[must_use]
    pub const fn initial_rate(&self) -> i128 {
        self.initial_rate
    }

    /// The display position at `at` µs, clamped to the scroll map's
    /// domain: a representative's own position at its µs, exact integer
    /// interpolation between two (`Δ/d·e + (Δ mod d)·e/d` with
    /// `d = tb − ta`, `e = t − ta`, `Δ = pb − pa`, no product larger than
    /// `Δ` or `d²`), and the initial or final rate outside them,
    /// saturating. Allocates nothing.
    #[must_use]
    pub fn position_at(&self, at: TimeUs) -> Position {
        let t = at.0.clamp(-DOMAIN, DOMAIN);
        let after = self.reps.partition_point(|&id| self.row(id).at.0 <= t);
        let Some(a) = after.checked_sub(1).map(|i| self.rep(i)) else {
            // Before the first representative (time 0).
            let b = self.rep(0);
            let back = i128::from(b.at.0.saturating_sub(t));
            return Position(
                b.pos
                    .0
                    .saturating_sub(back.saturating_mul(self.initial_rate)),
            );
        };
        if after >= self.reps.len() {
            let ahead = i128::from(t.saturating_sub(a.at.0));
            return Position(
                a.pos
                    .0
                    .saturating_add(ahead.saturating_mul(self.terminal_rate)),
            );
        }
        let b = self.rep(after);
        interpolate(a, b, t)
    }

    /// Representative `i`'s row; an empty row past the end, which
    /// preparation never makes.
    fn rep(&self, i: usize) -> TimelinePoint {
        self.reps.get(i).map_or(EMPTY, |&id| self.row(id))
    }

    fn row(&self, id: TimelineId) -> TimelinePoint {
        self.timeline.point(id).copied().unwrap_or(EMPTY)
    }
}

/// A stand-in row for an id the timeline lacks, which preparation never
/// gives.
const EMPTY: TimelinePoint = TimelinePoint {
    at: TimeUs(0),
    pos: Position(0),
    tempo: bms_model::Tempo::MIN,
};

/// The position at `t` between `a` and `b`, `a.at <= t < b.at`.
fn interpolate(a: TimelinePoint, b: TimelinePoint, t: i64) -> Position {
    // Times are within [0, 2^32] (preparation refuses rows past the
    // domain) and increase strictly between representatives; positions
    // never decrease, and differ by less than 2^127.
    let d = u128::from(b.at.0.abs_diff(a.at.0));
    let e = u128::from(t.abs_diff(a.at.0));
    let delta = b.pos.0.abs_diff(a.pos.0);
    let Some(q) = delta.checked_div(d) else {
        return a.pos;
    };
    let r = delta % d;
    // q·e ≤ Δ, and r·e < d² ≤ 2^64.
    let inc = q.saturating_mul(e).saturating_add(r.saturating_mul(e) / d);
    Position(
        a.pos
            .0
            .saturating_add(i128::try_from(inc).unwrap_or(i128::MAX)),
    )
}

/// The endpoint at `cursor` in `notes`: its row's index and its note.
fn endpoint(notes: &[Note], cursor: Cursor) -> Option<(usize, &Note)> {
    let note = notes.get(cursor.note)?;
    let id = if cursor.end {
        note.anchors.end
    } else {
        note.anchors.head
    };
    Some((id.index(), note))
}

/// The cursor past the endpoint at `cursor` of `note`.
const fn advance(note: &Note, cursor: Cursor) -> Cursor {
    match note.kind {
        NoteKind::Long { .. } if !cursor.end => Cursor {
            note: cursor.note,
            end: true,
        },
        _ => Cursor {
            note: cursor.note + 1,
            end: false,
        },
    }
}

/// The id of row `index`, below the timeline's `u32` cap.
fn row_id(index: usize) -> TimelineId {
    TimelineId(u32::try_from(index).unwrap_or(u32::MAX))
}
