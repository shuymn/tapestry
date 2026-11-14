//! A chart's MAIN tempo: the tempo its notes are weighted toward, which a
//! play's tempo references and graphs and a song list's preview share
//! (design.md "Play Adjustments", tempo).

use crate::{Chart, LnMode, MAX_TIMELINE_POINTS, Note, NoteKind, Tempo, TimelineId};

/// One tempo of MAIN's histogram: its weight and its earliest weighted
/// row.
#[derive(Clone, Copy, Debug)]
struct Bin {
    tempo: Tempo,
    weight: u64,
    earliest: TimelineId,
}

/// The bytes MAIN's histogram takes at most: one bin for each run of rows
/// sharing a tempo, and a timeline holds at most [`MAX_TIMELINE_POINTS`]
/// rows.
pub const MAIN_HISTOGRAM_LIMIT: usize = MAX_TIMELINE_POINTS * size_of::<Bin>();

/// Why [`main_tempo`] has no tempo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainError {
    /// The histogram needs more bytes than were available for it, or could
    /// not be allocated; nothing was kept.
    Histogram {
        /// The bytes it needs: its bins times their size.
        needed: usize,
        /// The bytes available.
        limit: usize,
    },
    /// A note's endpoint names a row the timeline does not hold, which the
    /// loader's link check refuses.
    Link(u32),
}

/// MAIN's tempo of `chart`, the notes `keep` accepts weighing it, with the
/// histogram bounded by `limit` bytes, charged before it is allocated and
/// freed before this returns.
///
/// Each endpoint counts at its own row's tempo: a tap's and a long note's
/// head, and a long note's end when the chart declares CN or HCN; mines
/// count nothing, and invisible notes and background sounds are not notes.
/// The heaviest tempo wins, a tie going to the one weighted first in row
/// order; `None` when no note weighs anything (a chart of mines alone),
/// which a play takes as its initial tempo and a song list as no MAIN.
/// The tie rule is tapestry's: the reference player takes the order of
/// its hash map, which is no rule of its own (design.md "Design
/// Principles", rules, not byproducts).
///
/// # Errors
///
/// [`MainError::Histogram`] when the histogram passes `limit` or cannot be
/// allocated; [`MainError::Link`] for an endpoint naming no row.
pub fn main_tempo(
    chart: &Chart,
    keep: impl Fn(&Note) -> bool,
    limit: usize,
) -> Result<Option<Tempo>, MainError> {
    let timeline = &chart.timeline;
    let runs = timeline.points().chunk_by(|a, b| a.tempo == b.tempo);
    // Every distinct tempo of the rows has a bin; the runs bound them.
    let count = runs.clone().count();
    let needed = count.saturating_mul(size_of::<Bin>());
    let refused = MainError::Histogram { needed, limit };
    if needed > limit {
        return Err(refused);
    }
    let mut bins: Vec<Bin> = Vec::new();
    bins.try_reserve_exact(count).map_err(|_| refused)?;
    bins.extend(runs.filter_map(|run| {
        run.first().map(|p| Bin {
            tempo: p.tempo,
            weight: 0,
            earliest: TimelineId(u32::MAX),
        })
    }));
    bins.sort_unstable_by_key(|b| b.tempo);
    bins.dedup_by_key(|b| b.tempo);
    let tails = matches!(chart.meta.ln_mode, Some(LnMode::Cn | LnMode::Hcn));
    for note in chart.lanes.iter().flatten() {
        if !keep(note) {
            continue;
        }
        let (head, end) = match note.kind {
            NoteKind::Tap => (true, false),
            NoteKind::Long { .. } => (true, tails),
            NoteKind::Mine { .. } => (false, false),
        };
        let ids = [
            head.then_some(note.anchors.head),
            end.then_some(note.anchors.end),
        ];
        for id in ids.into_iter().flatten() {
            let tempo = timeline.point(id).ok_or(MainError::Link(id.0))?.tempo;
            // Every row's tempo has its bin.
            let bin = bins
                .binary_search_by_key(&tempo, |b| b.tempo)
                .ok()
                .and_then(|i| bins.get_mut(i))
                .ok_or(MainError::Link(id.0))?;
            bin.weight += 1;
            bin.earliest = bin.earliest.min(id);
        }
    }
    let heaviest = bins
        .iter()
        .filter(|b| b.weight > 0)
        .min_by_key(|b| (std::cmp::Reverse(b.weight), b.earliest));
    Ok(heaviest.map(|b| b.tempo))
}

#[cfg(test)]
mod tests {
    use super::{MAIN_HISTOGRAM_LIMIT, MainError, main_tempo};
    use crate::{Chart, ChartBuilder, Lane, Tempo, TimeUs};

    fn tempo(bpm: i64) -> Tempo {
        Tempo::from_milli(bpm * 1_000).unwrap()
    }

    /// 150 BPM with `slow` taps, then 180 from 10 s with `fast`.
    fn chart(slow: i64, fast: i64) -> Chart {
        let lane = Lane::new(1).unwrap();
        let mut b = ChartBuilder::new(150_000).bpm(TimeUs(10_000_000), 180_000);
        for k in 0..slow {
            b = b.tap(lane, TimeUs(100_000 + k * 100_000));
        }
        for k in 0..fast {
            b = b.tap(lane, TimeUs(10_100_000 + k * 100_000));
        }
        b.build().unwrap()
    }

    fn main(chart: &Chart) -> Tempo {
        main_tempo(chart, |_| true, MAIN_HISTOGRAM_LIMIT)
            .unwrap()
            .unwrap()
    }

    /// Fixture C: the most-noted tempo.
    #[test]
    fn the_most_noted_tempo() {
        assert_eq!(main(&chart(10, 30)), tempo(180));
    }

    /// Fixture C-tie: a tie goes to the tempo weighted first.
    #[test]
    fn a_tie_goes_to_the_earliest() {
        assert_eq!(main(&chart(10, 10)), tempo(150));
    }

    /// Fixture M: mines weigh nothing, so a chart of mines alone has no
    /// MAIN.
    #[test]
    fn mines_alone_give_the_initial_tempo() {
        let lane = Lane::new(1).unwrap();
        let chart = ChartBuilder::new(150_000)
            .bpm(TimeUs(1_000_000), 180_000)
            .mine(lane, TimeUs(1_100_000), 1)
            .mine(lane, TimeUs(1_200_000), 1)
            .mine(lane, TimeUs(1_300_000), 1)
            .build()
            .unwrap();
        assert_eq!(main_tempo(&chart, |_| true, MAIN_HISTOGRAM_LIMIT), Ok(None));
    }

    /// The notes `keep` refuses weigh nothing.
    #[test]
    fn keep_filters_the_notes() {
        let chart = chart(10, 30);
        let early = main_tempo(
            &chart,
            |n| n.time < TimeUs(10_000_000),
            MAIN_HISTOGRAM_LIMIT,
        );
        assert_eq!(early, Ok(Some(tempo(150))));
    }

    /// A histogram past its limit is refused before it is allocated.
    #[test]
    fn a_histogram_past_its_limit_is_refused() {
        assert!(matches!(
            main_tempo(&chart(1, 1), |_| true, 1),
            Err(MainError::Histogram { limit: 1, .. })
        ));
    }
}
