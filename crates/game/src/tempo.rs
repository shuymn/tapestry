//! An attempt's tempo provider (design.md "Play Adjustments", tempo): the
//! chart's timeline as loaded at the attempt's speed, which answers the
//! adjustment reducer's lookup ([`TempoMap`]), and the reference tempos
//! prepared with it outside play.
//!
//! Every tempo here is the chart's original one, before the practice
//! speed: the timeline's times are playback µs at the speed it was loaded
//! at, its tempos are not, and the reducer applies the speed once, to a
//! lookup and to the references alike. A tempo whose exact ratio does not
//! fit ([`Tempo::ratio`]) is unavailable, never rounded. The provider
//! shares the chart's timeline and holds nothing else on the heap; MAIN's
//! histogram lives only while the references are prepared.

pub use bms_model::MAIN_HISTOGRAM_LIMIT;
use bms_model::{Chart, ChartTimeline, MainError, Speed, Tempo, TempoMeta, TimeUs};
use eval::Section;

use crate::adjustment::TempoMap;
use crate::hispeed::{HiSpeedReference, Ratio, References};

/// Why an attempt's reference tempos are unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReferenceError {
    /// The reference's tempo has no exact ratio of two `u64`.
    #[error("the {0:?} reference tempo has no exact ratio")]
    Unavailable(HiSpeedReference),
    /// MAIN's histogram needs more bytes than were available for it, or
    /// could not be allocated; nothing was kept.
    #[error("the main tempo's {needed}-byte histogram was not allocated (limit {limit} bytes)")]
    Histogram {
        /// The bytes it needs: its bins times their size.
        needed: usize,
        /// The bytes available.
        limit: usize,
    },
    /// A note's endpoint names a row the timeline does not hold, which the
    /// loader's link check refuses.
    #[error("a note endpoint names timeline row {0}, which the timeline does not hold")]
    Link(u32),
}

/// One attempt's tempo provider: the timeline of the chart as loaded at
/// the attempt's speed, the section whose targets weigh MAIN, and the
/// reference tempos prepared from them. Made afresh for every attempt
/// ([`crate::Loaded::attempt`]), so a speed change brings the newly loaded
/// timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptTempo {
    timeline: ChartTimeline,
    /// The round's section, its start and end in the chart's real time;
    /// `None` for the whole chart.
    section: Option<(TimeUs, TimeUs)>,
    references: Result<References, ReferenceError>,
}

impl AttemptTempo {
    /// The provider of an attempt on `chart`, as loaded at the attempt's
    /// speed, over `section` or the whole chart, MAIN's histogram given
    /// up to [`MAIN_HISTOGRAM_LIMIT`].
    #[must_use]
    pub fn prepare(chart: &Chart, section: Option<&Section>) -> Self {
        Self::prepare_within(chart, section, MAIN_HISTOGRAM_LIMIT)
    }

    /// [`prepare`](Self::prepare) with `limit` bytes for MAIN's histogram,
    /// which is charged against them before it is allocated and freed
    /// before this returns. The references are prepared outside play:
    ///
    /// - START is the initial tempo, MIN and MAX the timeline's extrema
    ///   with the initial tempo, under a section too.
    /// - MAIN is the tempo weighted by the notes, each endpoint counted at
    ///   its own row's tempo: a tap's and a long note's head, and a long
    ///   note's end when the chart declares CN or HCN; mines count nothing,
    ///   and invisible notes and background sounds are not notes. Under a
    ///   section only its targets count (`Section::is_target`). The
    ///   heaviest tempo wins, a tie going to the one weighted first in row
    ///   order, and without any weight it is the initial tempo.
    ///
    /// A reference whose tempo has no exact ratio makes them all
    /// unavailable ([`ReferenceError`]); the provider's lookup is
    /// unaffected.
    #[must_use]
    pub fn prepare_within(chart: &Chart, section: Option<&Section>, limit: usize) -> Self {
        Self {
            timeline: chart.timeline.clone(),
            section: section.map(|s| (s.start, s.end)),
            references: references(chart, section, limit),
        }
    }

    /// The timeline as loaded at the attempt's speed, shared with the
    /// chart.
    #[must_use]
    pub const fn timeline(&self) -> &ChartTimeline {
        &self.timeline
    }

    /// The tempo metadata: the original initial tempo and extrema, and the
    /// speed the timeline was loaded at.
    #[must_use]
    pub fn meta(&self) -> &TempoMeta {
        self.timeline.meta()
    }

    /// The speed the timeline's times are at.
    #[must_use]
    pub fn speed(&self) -> Speed {
        self.timeline.meta().speed
    }

    /// The round's section, its start and end in the chart's real time;
    /// `None` for the whole chart.
    #[must_use]
    pub const fn section(&self) -> Option<(TimeUs, TimeUs)> {
        self.section
    }

    /// The reference tempos in original BPM, which [`Event::Prepare`]
    /// scales by the speed once, or why they are unavailable.
    ///
    /// [`Event::Prepare`]: crate::adjustment::Event::Prepare
    ///
    /// # Errors
    ///
    /// The [`ReferenceError`] preparation met.
    pub const fn references(&self) -> Result<References, ReferenceError> {
        self.references
    }
}

impl TempoMap for AttemptTempo {
    /// The original tempo at the playback time `head` on the loaded
    /// timeline ([`ChartTimeline::tempo_at`]); `None` when it has no exact
    /// ratio.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn bpm_at(&self, head: TimeUs) -> Option<Ratio> {
        ratio(self.timeline.tempo_at(head))
    }
}

/// `tempo` as an exact ratio, if it has one.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn ratio(tempo: Tempo) -> Option<Ratio> {
    let (num, den) = tempo.ratio()?;
    Ratio::new(num, den)
}

/// The references of `chart` over `section`, MAIN's histogram within
/// `limit` bytes.
fn references(
    chart: &Chart,
    section: Option<&Section>,
    limit: usize,
) -> Result<References, ReferenceError> {
    let meta = chart.timeline.meta();
    let exact = |tempo, reference| ratio(tempo).ok_or(ReferenceError::Unavailable(reference));
    let start = exact(meta.initial, HiSpeedReference::Start)?;
    let maximum = exact(meta.max, HiSpeedReference::Maximum)?;
    let minimum = exact(meta.min, HiSpeedReference::Minimum)?;
    let main = exact(main(chart, section, limit)?, HiSpeedReference::Main)?;
    Ok(References {
        start,
        maximum,
        main,
        minimum,
    })
}

/// The whole chart's MAIN tempo, with the histogram bounded by `limit`
/// bytes: what a loaded chart computes once for its play skins' tempo
/// graphs, whatever section a practice round plays (stage 3, S10-10).
pub(crate) fn whole_main(chart: &Chart, limit: usize) -> Result<Tempo, ReferenceError> {
    main(chart, None, limit)
}

/// MAIN's tempo ([`AttemptTempo::prepare_within`]): the chart's
/// ([`bms_model::main_tempo`]), weighed by the section's targets only.
fn main(chart: &Chart, section: Option<&Section>, limit: usize) -> Result<Tempo, ReferenceError> {
    let keep = |note: &bms_model::Note| section.is_none_or(|s| s.is_target(note));
    bms_model::main_tempo(chart, keep, limit).map_err(|e| match e {
        MainError::Histogram { needed, limit } => ReferenceError::Histogram { needed, limit },
        MainError::Link(row) => ReferenceError::Link(row),
    })
}

#[cfg(test)]
mod tests {
    use bms_model::{Chart, ChartBuilder, ChartTimeline, Lane, LnMode, Tempo, TimeUs};
    use eval::Section;

    use super::{AttemptTempo, ReferenceError};
    use crate::adjustment::TempoMap;
    use crate::hispeed::{HiSpeedReference, Ratio, References};

    const MS: i64 = 1_000;

    fn lane(i: u8) -> Lane {
        Lane::new(i).unwrap()
    }

    fn at(ms: i64) -> TimeUs {
        TimeUs(ms * MS)
    }

    fn bpm(n: u64) -> Ratio {
        Ratio::integer(n)
    }

    fn section(start: i64, end: i64) -> Section {
        Section {
            start: at(start),
            end: at(end),
            lead_in: at(1_000),
            initial_gauges: Vec::new(),
        }
    }

    fn main(chart: &Chart, section: Option<&Section>) -> Ratio {
        AttemptTempo::prepare(chart, section)
            .references()
            .unwrap()
            .main
    }

    /// 150 BPM with two taps, then 300 from 1 s with a tap, three mines
    /// and a long note from 1.5 s to 1.9 s.
    fn weighted() -> Chart {
        ChartBuilder::new(150_000)
            .tap(lane(1), at(100))
            .tap(lane(2), at(200))
            .bpm(at(1_000), 300_000)
            .tap(lane(1), at(1_100))
            .mine(lane(2), at(1_200), 1)
            .mine(lane(2), at(1_300), 1)
            .mine(lane(2), at(1_400), 1)
            .long(lane(3), at(1_500), at(1_900))
            .build()
            .unwrap()
    }

    /// MAIN counts taps and long-note heads, never mines: two at 150 and
    /// two at 300 tie, and the tie goes to the tempo weighted first. A
    /// chart that declares CN or HCN counts long-note ends too, LN or no
    /// declaration not.
    #[test]
    fn main_weighs_taps_heads_and_declared_ends() {
        let mut chart = weighted();
        for (mode, expected) in [
            (None, 150),
            (Some(LnMode::Ln), 150),
            (Some(LnMode::Cn), 300),
            (Some(LnMode::Hcn), 300),
        ] {
            chart.meta.ln_mode = mode;
            assert_eq!(main(&chart, None), bpm(expected), "{mode:?}");
        }
    }

    /// A tie goes to the tempo weighted first, not to the tempo whose rows
    /// come first: 150 from 0, unweighted until 2 s, and 300 weighted at
    /// 1.1 s.
    #[test]
    fn a_tie_goes_to_the_earliest_weighted_occurrence() {
        let chart = ChartBuilder::new(150_000)
            .bpm(at(1_000), 300_000)
            .tap(lane(1), at(1_100))
            .bpm(at(2_000), 150_000)
            .tap(lane(1), at(2_100))
            .build()
            .unwrap();
        assert_eq!(main(&chart, None), bpm(300));
    }

    /// Without any weight MAIN is the initial tempo, even where a change
    /// at time 0 replaces it at once.
    #[test]
    fn no_weight_is_the_initial_tempo() {
        let chart = ChartBuilder::new(150_000)
            .bpm(at(0), 300_000)
            .mine(lane(1), at(100), 1)
            .build()
            .unwrap();
        let refs = AttemptTempo::prepare(&chart, None).references().unwrap();
        assert_eq!(
            refs,
            References {
                start: bpm(150),
                maximum: bpm(300),
                main: bpm(150),
                minimum: bpm(150),
            }
        );
    }

    /// A practice round weighs MAIN by its own targets, a long note only
    /// when it lies wholly inside; START, MIN and MAX stay the whole
    /// chart's, and the section is kept with the provider.
    #[test]
    fn a_section_weighs_main_by_its_targets_only() {
        let chart = weighted();
        // The 300 BPM part: a tap and the long note.
        let part = section(1_000, 2_000);
        let tempo = AttemptTempo::prepare(&chart, Some(&part));
        assert_eq!(tempo.section(), Some((at(1_000), at(2_000))));
        assert_eq!(
            tempo.references(),
            Ok(References {
                start: bpm(150),
                maximum: bpm(300),
                main: bpm(300),
                minimum: bpm(150),
            })
        );
        // Ending at 1.6 s, the long note is not a target: 150 has two
        // taps, 300 one.
        let cut = section(0, 1_600);
        assert_eq!(main(&chart, Some(&cut)), bpm(150));
        // Nothing in it: the initial tempo.
        assert_eq!(main(&chart, Some(&section(3_000, 4_000))), bpm(150));
    }

    /// The histogram is charged at its bins' bytes (one 32-byte bin per
    /// run of rows sharing a tempo) before it is allocated: a byte short
    /// refuses MAIN, and the references with it.
    #[test]
    fn the_histogram_is_charged_before_it_is_allocated() {
        // Rows at 150, then 300, then 150 again: three runs.
        let chart = ChartBuilder::new(150_000)
            .tap(lane(1), at(100))
            .bpm(at(1_000), 300_000)
            .tap(lane(1), at(1_100))
            .bpm(at(2_000), 150_000)
            .tap(lane(1), at(2_100))
            .build()
            .unwrap();
        let needed = 3 * 32;
        let refused = AttemptTempo::prepare_within(&chart, None, needed - 1);
        assert_eq!(
            refused.references(),
            Err(ReferenceError::Histogram {
                needed,
                limit: needed - 1
            })
        );
        let fits = AttemptTempo::prepare_within(&chart, None, needed);
        assert_eq!(fits.references().unwrap().main, bpm(150));
    }

    /// Endpoints count at their own row's tempo, not at the tempo in
    /// effect at their µs: two 150 BPM heads share a µs with a later row
    /// that changes to 300 (a change less than a µs after them), and one
    /// head follows at 300. By row, 150 has two and 300 one; by µs, 300
    /// would have all three.
    #[test]
    fn an_endpoint_counts_at_its_own_row_not_its_us() {
        let mut chart = ChartBuilder::new(150_000)
            .tap(lane(1), at(1_000))
            .tap(lane(2), at(1_000))
            .tap(lane(1), at(2_000))
            .build()
            .unwrap();
        let t = &chart.timeline;
        let (first, last) = (t.points()[1], t.points()[2]);
        let change = bms_model::TimelinePoint {
            pos: bms_model::Position(first.pos.0 + 1),
            tempo: Tempo::from_milli(300_000).unwrap(),
            ..first
        };
        let late = bms_model::TimelinePoint {
            tempo: change.tempo,
            ..last
        };
        let points = vec![t.points()[0], first, change, late];
        chart.timeline =
            ChartTimeline::new(points, Vec::new(), t.meta().initial, t.meta().speed).unwrap();
        let row = |i| bms_model::NoteAnchors {
            head: bms_model::TimelineId(i),
            end: bms_model::TimelineId(i),
        };
        chart.lanes[1][0].anchors = row(1);
        chart.lanes[2][0].anchors = row(1);
        chart.lanes[1][1].anchors = row(3);
        assert_eq!(chart.validate_presentation_links(), Ok(()));
        assert_eq!(chart.timeline.tempo_at(at(1_000)), change.tempo);
        assert_eq!(main(&chart, None), bpm(150));
    }

    /// A declared CN's end counts at its own row's tempo: its head at 150,
    /// a change to 300 under it, its end at 300. With a tap at 150 and two
    /// at 300, 300 has three and 150 two; counted at its head's tempo the
    /// end would make it 150, three to two.
    #[test]
    fn a_long_note_end_counts_at_its_own_tempo() {
        let mut chart = ChartBuilder::new(150_000)
            .tap(lane(1), at(200))
            .long(lane(2), at(500), at(1_500))
            .bpm(at(1_000), 300_000)
            .tap(lane(1), at(2_000))
            .tap(lane(1), at(2_500))
            .build()
            .unwrap();
        for mode in [LnMode::Cn, LnMode::Hcn] {
            chart.meta.ln_mode = Some(mode);
            assert_eq!(main(&chart, None), bpm(300), "{mode:?}");
        }
        chart.meta.ln_mode = None;
        assert_eq!(main(&chart, None), bpm(150));
    }

    /// `chart` with the tempo of row `row` and every later row, or the
    /// initial tempo, replaced by `tempo`.
    fn retimed(chart: &Chart, row: Option<usize>, tempo: Tempo) -> Chart {
        let t = &chart.timeline;
        let mut points = t.points().to_vec();
        let mut initial = t.meta().initial;
        match row {
            Some(i) => points[i..].iter_mut().for_each(|p| p.tempo = tempo),
            None => initial = tempo,
        }
        let mut chart = chart.clone();
        chart.timeline =
            ChartTimeline::new(points, t.bar_ids().to_vec(), initial, t.meta().speed).unwrap();
        chart
    }

    /// A tempo without an exact ratio is unavailable, never rounded: the
    /// references are unavailable, naming the first that is, and the
    /// lookup answers `None` where that tempo is in effect and the exact
    /// tempo elsewhere.
    #[test]
    fn a_tempo_without_a_ratio_is_unavailable() {
        // 150.000000000000000001 BPM.
        let fine = Tempo::from_units(150 * Tempo::UNITS_PER_BPM + 1).unwrap();
        assert_eq!(fine.ratio(), None);
        let chart = ChartBuilder::new(120_000)
            .tap(lane(1), at(100))
            .bpm(at(1_000), 90_000)
            .tap(lane(1), at(1_100))
            .build()
            .unwrap();
        // From the change at 1 s on (row 2): the maximum.
        let late = AttemptTempo::prepare(&retimed(&chart, Some(2), fine), None);
        assert_eq!(
            late.references(),
            Err(ReferenceError::Unavailable(HiSpeedReference::Maximum))
        );
        assert_eq!(late.bpm_at(at(999)), Some(bpm(120)));
        assert_eq!(late.bpm_at(at(1_000)), None);
        // The initial tempo: START, and the lookup before time 0.
        let early = AttemptTempo::prepare(&retimed(&chart, None, fine), None);
        assert_eq!(
            early.references(),
            Err(ReferenceError::Unavailable(HiSpeedReference::Start))
        );
        assert_eq!(early.bpm_at(TimeUs(-1)), None);
        assert_eq!(early.bpm_at(TimeUs(0)), Some(bpm(120)));
    }
}
