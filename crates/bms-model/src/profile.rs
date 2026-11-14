//! A chart's profile for its notes and tempo graphs (stage 3, S10-10): the
//! notes of each second by category and the tempo rows, over the chart's
//! 1× times. The loader builds it before it remaps the times to a practice
//! speed, the only point that holds them (design.md "Practice Speed
//! Changes").

use crate::{Chart, Lane, NoteKind, Tempo, TimeUs, TimelineId, TimelinePoint};

/// The note categories of a second, in [`ChartProfile::seconds`]' order.
pub const NOTE_CATEGORIES: usize = 7;
/// The most bins [`ChartProfile::bins`] holds.
pub const NOTE_BINS: usize = 512;

/// A scratch long note's head.
pub const SCRATCH_LONG_HEAD: usize = 0;
/// A scratch long note's body.
pub const SCRATCH_LONG_BODY: usize = 1;
/// A scratch tap.
pub const SCRATCH_TAP: usize = 2;
/// A key long note's head.
pub const KEY_LONG_HEAD: usize = 3;
/// A key long note's body.
pub const KEY_LONG_BODY: usize = 4;
/// A key tap.
pub const KEY_TAP: usize = 5;
/// A mine, scratch or key.
pub const MINE: usize = 6;

/// [`SpeedRow::scroll`]'s factor 1, in its units of `10^-18`.
pub const SCROLL_ONE: i128 = crate::HeaderValue::ONE;

/// One tempo row of [`ChartProfile::speeds`]: from `ms`, the chart's 1×
/// time in whole ms truncated, the chart scrolls at `tempo` times `scroll`,
/// or stands still for `None` (a STOP).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeedRow {
    /// The row's 1× time in whole ms.
    pub ms: u32,
    /// The original tempo from there, `None` while a STOP holds.
    pub tempo: Option<Tempo>,
    /// The scroll factor (`#SCROLLxx`) in effect, in units of `10^-18`
    /// ([`SCROLL_ONE`]): always 1 in a play's profile, which does not read
    /// scroll changes ([`ChartProfile::of`]).
    pub scroll: i128,
}

/// What a song list's preview reads of a chart beyond its notes and tempo
/// rows (design.md "Loading and Song Selection", Summaries), all at 1×: the
/// loader times these objects for the preview alone and neither plays nor
/// shows them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreviewInputs {
    /// The preview's own timeline, the chart's objects and the preview's
    /// timed together, which keeps the tempo changes and STOPs a play's
    /// timeline drops after its last object; `None` when the chart has neither
    /// a preview object nor a timing tail, or timing them failed, so the play
    /// chart's is used.
    pub timeline: Option<PreviewTimeline>,
    /// The latest hidden note (channels 31–39), BGA event (04) or layer
    /// event (07), which lengthen the seconds the preview counts over.
    pub last_shown: Option<TimeUs>,
}

/// [`PreviewInputs::timeline`]: rows as a [`ChartTimeline`](crate::ChartTimeline)
/// holds them, at 1×.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewTimeline {
    /// The rows in time order, a STOP's end the row after its start.
    pub points: Vec<TimelinePoint>,
    /// The rows where a STOP starts, in order.
    pub stops: Vec<TimelineId>,
    /// The scroll changes (channel `SC` with `#SCROLLxx`): at most one per
    /// row of this timeline, ascending, each the factor in units of
    /// `10^-18` from that row on. Keyed by row, never by time, so two
    /// changes in one µs at distinct positions stay apart (design.md
    /// "Chart Time").
    pub scroll: Vec<(TimelineId, i128)>,
    /// The tempo before time 0.
    pub initial: Tempo,
}

/// A chart's notes by second and its tempo rows, at 1× times.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartProfile {
    seconds: Vec<[u32; NOTE_CATEGORIES]>,
    bins: Vec<[u16; NOTE_CATEGORIES]>,
    bin_seconds: u32,
    speeds: Vec<SpeedRow>,
    length_ms: u32,
}

impl ChartProfile {
    /// The profile of `chart`, whose times must be its 1× times and whose
    /// `meta.length` its summary's length. `stops` are the rows, in order,
    /// where a STOP of any positive length starts, which the rows' floored
    /// times cannot tell from a point at the same position.
    #[must_use]
    pub fn of(chart: &Chart, stops: &[TimelineId]) -> Self {
        Self::preview(chart, stops, &PreviewInputs::default())
    }

    /// A song list's preview profile of `chart` ([`of`](Self::of)'s
    /// arguments) with what the loader timed for it (`inputs`), as the
    /// reference player's song information reads a chart (beatoraja
    /// `song/SongInformation.java:102`, `:180-210`; jbms-parser
    /// `BMSModel.getLastMilliTime`): its length runs to the later of the
    /// chart's and [`PreviewInputs::last_shown`], its seconds over it; its
    /// tempo rows follow the preview's own timeline when there is one, and
    /// carry its scroll factor, a scroll change being a row of its own;
    /// the last row is at the latest point or shown object.
    #[must_use]
    pub fn preview(chart: &Chart, stops: &[TimelineId], inputs: &PreviewInputs) -> Self {
        let end = inputs
            .last_shown
            .map_or(chart.meta.length, |t| t.max(chart.meta.length));
        let seconds = seconds(chart, end);
        let (bins, bin_seconds) = condense(&seconds);
        let speeds = match &inputs.timeline {
            Some(t) => speeds(&t.points, t.initial, &t.stops, &t.scroll, inputs.last_shown),
            None => speeds(
                chart.timeline.points(),
                chart.timeline.meta().initial,
                stops,
                &[],
                inputs.last_shown,
            ),
        };
        Self {
            seconds,
            bins,
            bin_seconds,
            speeds,
            length_ms: ms_of(end),
        }
    }

    /// The content's length in whole ms: the chart's
    /// ([`ChartMeta::length`](crate::ChartMeta::length)), or a preview's
    /// with its shown objects, which a tempo graph spans.
    #[must_use]
    pub const fn length_ms(&self) -> u32 {
        self.length_ms
    }

    /// One entry per 1× second, `⌊length / 1 s⌋ + 2` of them: each
    /// category's count of the notes in that second. A long note counts
    /// its body in every second from its head's to its end's (the last
    /// second at most), its head in its head's second in place of the
    /// body there; its end counts nothing.
    #[must_use]
    pub fn seconds(&self) -> &[[u32; NOTE_CATEGORIES]] {
        &self.seconds
    }

    /// At most [`NOTE_BINS`] bins of [`bin_seconds`](Self::bin_seconds)
    /// seconds each, the last one maybe shorter: each the counts of its
    /// fullest second (the largest sum over the categories, the earliest
    /// on a tie), saturating.
    #[must_use]
    pub fn bins(&self) -> &[[u16; NOTE_CATEGORIES]] {
        &self.bins
    }

    /// The seconds per bin: 1 up to [`NOTE_BINS`] seconds, otherwise
    /// `⌈seconds / NOTE_BINS⌉`.
    #[must_use]
    pub const fn bin_seconds(&self) -> u32 {
        self.bin_seconds
    }

    /// The tempo rows, in time order: the initial tempo at 0; at each
    /// point (a note's head or end, a BGM event, a tempo change, a STOP's
    /// start or end, or a bar line; in a preview also a scroll change) the
    /// state there: stopped where a STOP starts (one of the `stops` given
    /// to [`of`](Self::of)), otherwise its tempo and scroll factor, so a
    /// STOP ends at its own end; a row only where the effective speed
    /// (tempo times factor, 0 while stopped) differs from the last row's,
    /// compared exactly; and the state at the last point unless a row is
    /// already there.
    #[must_use]
    pub fn speeds(&self) -> &[SpeedRow] {
        &self.speeds
    }
}

/// The 1× second of `t`, if it is one of `count`.
fn second_of(t: TimeUs, count: usize) -> Option<usize> {
    usize::try_from(t.0.div_euclid(1_000_000))
        .ok()
        .filter(|&s| t.0 >= 0 && s < count)
}

fn seconds(chart: &Chart, end: TimeUs) -> Vec<[u32; NOTE_CATEGORIES]> {
    let whole = usize::try_from(end.0.max(0) / 1_000_000).unwrap_or(0);
    let count = whole.saturating_add(2);
    let mut seconds = vec![[0_u32; NOTE_CATEGORIES]; count];
    for lane in Lane::all() {
        let scratch = lane == Lane::SCRATCH;
        let Some(notes) = chart.lanes.get(lane.index()) else {
            continue;
        };
        for note in notes {
            let Some(head) = second_of(note.time, count) else {
                continue;
            };
            let category = match note.kind {
                NoteKind::Tap if scratch => SCRATCH_TAP,
                NoteKind::Tap => KEY_TAP,
                NoteKind::Mine { .. } => MINE,
                NoteKind::Long { end, .. } => {
                    let (head_cat, body) = if scratch {
                        (SCRATCH_LONG_HEAD, SCRATCH_LONG_BODY)
                    } else {
                        (KEY_LONG_HEAD, KEY_LONG_BODY)
                    };
                    let last = second_of(end, count).unwrap_or(count - 1);
                    for s in seconds.iter_mut().take(last + 1).skip(head) {
                        if let Some(c) = s.get_mut(body) {
                            *c = c.saturating_add(1);
                        }
                    }
                    if let Some(c) = seconds.get_mut(head).and_then(|s| s.get_mut(body)) {
                        *c = c.saturating_sub(1);
                    }
                    head_cat
                }
            };
            if let Some(c) = seconds.get_mut(head).and_then(|s| s.get_mut(category)) {
                *c = c.saturating_add(1);
            }
        }
    }
    seconds
}

/// The bins of `seconds` and their width in seconds.
fn condense(seconds: &[[u32; NOTE_CATEGORIES]]) -> (Vec<[u16; NOTE_CATEGORIES]>, u32) {
    let width = seconds.len().div_ceil(NOTE_BINS).max(1);
    let bins = seconds
        .chunks(width)
        .map(|chunk| {
            let mut fullest: Option<(&[u32; NOTE_CATEGORIES], u64)> = None;
            for s in chunk {
                let sum: u64 = s.iter().map(|&c| u64::from(c)).sum();
                if fullest.is_none_or(|(_, best)| sum > best) {
                    fullest = Some((s, sum));
                }
            }
            fullest.map_or([0; NOTE_CATEGORIES], |(s, _)| {
                s.map(|c| u16::try_from(c).unwrap_or(u16::MAX))
            })
        })
        .collect();
    (bins, u32::try_from(width).unwrap_or(u32::MAX))
}

/// The whole ms of a 1× time, truncated.
fn ms_of(t: TimeUs) -> u32 {
    u32::try_from(t.0.max(0) / 1_000).unwrap_or(u32::MAX)
}

/// `a · b` as 256 bits, high then low.
const fn wide(a: u128, b: u128) -> (u128, u128) {
    const LOW: u128 = u64::MAX as u128;
    let (a1, a0) = (a >> 64, a & LOW);
    let (b1, b0) = (b >> 64, b & LOW);
    let lo = a0 * b0;
    let mid1 = a1 * b0;
    let mid2 = a0 * b1;
    let hi = a1 * b1;
    let (mid, carry) = mid1.overflowing_add(mid2);
    let carry = if carry { 1_u128 << 64 } else { 0 };
    let (lo, c) = lo.overflowing_add(mid << 64);
    let hi = hi + (mid >> 64) + carry + if c { 1 } else { 0 };
    (hi, lo)
}

/// A state's effective speed as a sign and a 256-bit magnitude: the tempo's
/// units times the factor's, 0 while stopped.
fn effective(tempo: Option<Tempo>, scroll: i128) -> (i8, (u128, u128)) {
    match tempo {
        Some(t) if scroll != 0 => (
            if scroll < 0 { -1 } else { 1 },
            wide(t.units(), scroll.unsigned_abs()),
        ),
        _ => (0, (0, 0)),
    }
}

fn speeds(
    points: &[TimelinePoint],
    initial: Tempo,
    stops: &[TimelineId],
    scroll: &[(TimelineId, i128)],
    last_shown: Option<TimeUs>,
) -> Vec<SpeedRow> {
    let mut state = (Some(initial), SCROLL_ONE);
    let mut rows = vec![SpeedRow {
        ms: 0,
        tempo: state.0,
        scroll: state.1,
    }];
    // A row where the effective speed changes.
    let emit = |rows: &mut Vec<SpeedRow>, ms, state: (Option<Tempo>, i128)| {
        let changed = rows
            .last()
            .is_none_or(|r| effective(r.tempo, r.scroll) != effective(state.0, state.1));
        if changed {
            rows.push(SpeedRow {
                ms,
                tempo: state.0,
                scroll: state.1,
            });
        }
    };
    let mut changes = scroll.iter().peekable();
    let mut last = None;
    for (i, p) in points.iter().enumerate() {
        let id = TimelineId(u32::try_from(i).unwrap_or(u32::MAX));
        // A change at this row applies from it; every change is a row.
        if let Some(&(_, f)) = changes.next_if(|&&(at, _)| at == id) {
            state.1 = f;
        }
        let ms = ms_of(p.at);
        state.0 = if stops.binary_search(&id).is_ok() {
            None
        } else {
            Some(p.tempo)
        };
        emit(&mut rows, ms, state);
        last = Some(ms);
    }
    let shown = last_shown.map(ms_of);
    let last = last.max(shown);
    if let Some(ms) = last
        && rows.last().is_none_or(|r| r.ms != ms)
    {
        rows.push(SpeedRow {
            ms,
            tempo: state.0,
            scroll: state.1,
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{ChartProfile, PreviewInputs, PreviewTimeline, SCROLL_ONE, SpeedRow};
    use crate::{
        Chart, ChartBuilder, ChartSummary, Lane, Tempo, TimeUs, TimelineId, TimelinePoint,
    };

    /// Preview inputs for `chart` with the scroll `changes` as rows of a
    /// timeline of its own: a change at a point's time takes that point's
    /// row (the first there), one between points a row inserted at its time
    /// with the tempo before it, as the loader times an `SC` object.
    fn with_scroll(
        chart: &Chart,
        last_shown: Option<TimeUs>,
        changes: &[(TimeUs, i128)],
    ) -> PreviewInputs {
        let mut points: Vec<TimelinePoint> = chart.timeline.points().to_vec();
        for &(at, _) in changes {
            if points.iter().all(|p| p.at != at) {
                let k = points.partition_point(|p| p.at < at);
                let before = points[k.saturating_sub(1)];
                points.insert(
                    k,
                    TimelinePoint {
                        at,
                        pos: before.pos,
                        tempo: before.tempo,
                    },
                );
            }
        }
        let scroll = changes
            .iter()
            .map(|&(at, f)| {
                let i = points.iter().position(|p| p.at == at).unwrap();
                (TimelineId(u32::try_from(i).unwrap()), f)
            })
            .collect();
        PreviewInputs {
            timeline: Some(PreviewTimeline {
                points,
                stops: Vec::new(),
                scroll,
                initial: chart.timeline.meta().initial,
            }),
            last_shown,
        }
    }

    fn chart(b: ChartBuilder) -> Chart {
        let mut chart = b.build().unwrap();
        chart.meta.length = ChartSummary::of(&chart).length;
        chart
    }

    fn row(ms: u32, bpm: i64, scroll: i128) -> SpeedRow {
        SpeedRow {
            ms,
            tempo: Some(Tempo::from_milli(bpm * 1_000).unwrap()),
            scroll,
        }
    }

    /// Taps at 0, 1, 2 and 3 s at 120 BPM.
    fn four_taps() -> Chart {
        let lane = Lane::new(1).unwrap();
        chart(
            ChartBuilder::new(120_000)
                .tap(lane, TimeUs(0))
                .tap(lane, TimeUs(1_000_000))
                .tap(lane, TimeUs(2_000_000))
                .tap(lane, TimeUs(3_000_000)),
        )
    }

    /// Without inputs the preview is the play profile.
    #[test]
    fn a_preview_without_inputs_is_the_play_profile() {
        let chart = four_taps();
        assert_eq!(
            ChartProfile::preview(&chart, &[], &PreviewInputs::default()),
            ChartProfile::of(&chart, &[])
        );
    }

    /// Fixture H: a hidden note or BGA event at 4.2 s lengthens the seconds
    /// to 6 (`4 + 2`) and ends the rows there; the play profile keeps 5.
    #[test]
    fn a_shown_object_lengthens_the_preview() {
        let chart = four_taps();
        let inputs = PreviewInputs {
            timeline: None,
            last_shown: Some(TimeUs(4_200_000)),
        };
        let preview = ChartProfile::preview(&chart, &[], &inputs);
        assert_eq!(preview.seconds().len(), 6);
        assert_eq!(ChartProfile::of(&chart, &[]).seconds().len(), 5);
        assert_eq!(preview.speeds().last(), Some(&row(4_200, 120, SCROLL_ONE)));
        // A shown object before the length changes nothing.
        let early = PreviewInputs {
            timeline: None,
            last_shown: Some(TimeUs(500_000)),
        };
        assert_eq!(
            ChartProfile::preview(&chart, &[], &early),
            ChartProfile::of(&chart, &[])
        );
    }

    /// Fixture SC: the rows carry the scroll factor, a change at a point's
    /// time applying at it, and one between points a row of its own.
    #[test]
    fn the_rows_carry_the_scroll_factor() {
        let chart = four_taps();
        let inputs = with_scroll(
            &chart,
            None,
            &[
                (TimeUs(1_000_000), SCROLL_ONE / 2),
                (TimeUs(1_500_000), SCROLL_ONE / 2),
                (TimeUs(2_000_000), 2 * SCROLL_ONE),
                (TimeUs(2_500_000), 0),
            ],
        );
        assert_eq!(
            ChartProfile::preview(&chart, &[], &inputs).speeds(),
            [
                row(0, 120, SCROLL_ONE),
                row(1_000, 120, SCROLL_ONE / 2),
                row(2_000, 120, 2 * SCROLL_ONE),
                row(2_500, 120, 0),
                row(3_000, 120, 0),
            ]
        );
        assert_eq!(
            ChartProfile::of(&chart, &[]).speeds(),
            [row(0, 120, SCROLL_ONE), row(3_000, 120, SCROLL_ONE)]
        );
    }

    /// A scroll change after the last point is a row of its own and the
    /// last.
    #[test]
    fn a_late_scroll_change_ends_the_rows() {
        let chart = four_taps();
        let inputs = with_scroll(&chart, None, &[(TimeUs(5_000_000), 3 * SCROLL_ONE)]);
        let speeds = ChartProfile::preview(&chart, &[], &inputs);
        assert_eq!(
            speeds.speeds().last(),
            Some(&row(5_000, 120, 3 * SCROLL_ONE))
        );
    }

    /// Fixture RED: rows follow the effective speed, compared exactly, so
    /// (120 BPM, ×1) and (240 BPM, ×0.5) alternating at 4 096 points give
    /// the initial row and the last one only.
    #[test]
    fn equal_effective_speeds_coalesce() {
        let lane = Lane::new(1).unwrap();
        let mut b = ChartBuilder::new(120_000);
        let mut scroll = Vec::new();
        for k in 0..4_096_i64 {
            let at = TimeUs(1_000_000 + k * 1_000);
            let fast = k % 2 == 0;
            b = b.bpm(at, if fast { 240_000 } else { 120_000 });
            scroll.push((at, if fast { SCROLL_ONE / 2 } else { SCROLL_ONE }));
        }
        let chart = chart(b.tap(lane, TimeUs(6_000_000)));
        let inputs = with_scroll(&chart, None, &scroll);
        assert_eq!(
            ChartProfile::preview(&chart, &[], &inputs).speeds(),
            [row(0, 120, SCROLL_ONE), row(6_000, 120, SCROLL_ONE)]
        );
    }

    /// A scroll of 0 is a speed of 0, as a STOP's: two such states in a
    /// row are one row; a negative factor is its own speed.
    #[test]
    fn a_zero_scroll_is_speed_zero() {
        let chart = four_taps();
        let inputs = with_scroll(
            &chart,
            None,
            &[
                (TimeUs(500_000), 0),
                (TimeUs(1_000_000), 0),
                (TimeUs(1_500_000), -SCROLL_ONE),
            ],
        );
        assert_eq!(
            ChartProfile::preview(&chart, &[], &inputs).speeds(),
            [
                row(0, 120, SCROLL_ONE),
                row(500, 120, 0),
                row(1_500, 120, -SCROLL_ONE),
                row(3_000, 120, -SCROLL_ONE),
            ]
        );
    }

    /// The preview's length is the later of the chart's and the last shown
    /// object, in whole ms, never a later scroll change (fixture HS); the
    /// play profile's is the chart's.
    #[test]
    fn the_preview_length_is_the_content_length() {
        let chart = four_taps();
        let inputs = with_scroll(
            &chart,
            Some(TimeUs(4_200_500)),
            &[(TimeUs(5_000_000), 2 * SCROLL_ONE)],
        );
        let preview = ChartProfile::preview(&chart, &[], &inputs);
        assert_eq!(preview.length_ms(), 4_200);
        assert_eq!(
            preview.speeds().last(),
            Some(&row(5_000, 120, 2 * SCROLL_ONE))
        );
        assert_eq!(ChartProfile::of(&chart, &[]).length_ms(), 3_000);
    }

    fn point(at: i64, bpm: i64) -> TimelinePoint {
        TimelinePoint {
            at: TimeUs(at),
            pos: crate::Position(0),
            tempo: Tempo::from_milli(bpm * 1_000).unwrap(),
        }
    }

    fn preview_rows(
        points: Vec<TimelinePoint>,
        stops: &[u32],
        scroll: &[(u32, i128)],
    ) -> Vec<SpeedRow> {
        let inputs = PreviewInputs {
            timeline: Some(PreviewTimeline {
                points,
                stops: stops.iter().map(|&i| TimelineId(i)).collect(),
                scroll: scroll.iter().map(|&(i, f)| (TimelineId(i), f)).collect(),
                initial: Tempo::from_milli(120_000).unwrap(),
            }),
            last_shown: None,
        };
        ChartProfile::preview(&four_taps(), &[], &inputs)
            .speeds()
            .to_vec()
    }

    /// A scroll change applies at its own row, never at another row of the
    /// same µs: rows 1 (120 BPM) and 2 (240 BPM) are both at 500 ms, and a
    /// factor of 2 from row 2 gives one row (240, ×2), where keying it by
    /// time would give (120, ×2) first.
    #[test]
    fn a_scroll_change_applies_at_its_row_not_its_us() {
        let points = vec![
            point(0, 120),
            point(500_000, 120),
            point(500_000, 240),
            point(1_000_000, 240),
        ];
        assert_eq!(
            preview_rows(points, &[], &[(2, 2 * SCROLL_ONE)]),
            [
                row(0, 120, SCROLL_ONE),
                row(500, 240, 2 * SCROLL_ONE),
                row(1_000, 240, 2 * SCROLL_ONE)
            ]
        );
    }

    /// A scroll change at a STOP's start holds through the STOP: stopped
    /// from 1 s, the factor ½ from there, and at the STOP's end (1.5 s) the
    /// tempo comes back at ½.
    #[test]
    fn a_scroll_change_at_a_stop_start_holds_through_the_stop() {
        let points = vec![
            point(0, 120),
            point(1_000_000, 120),
            point(1_500_000, 120),
            point(2_000_000, 120),
        ];
        let stopped = SpeedRow {
            ms: 1_000,
            tempo: None,
            scroll: SCROLL_ONE / 2,
        };
        assert_eq!(
            preview_rows(points, &[1], &[(1, SCROLL_ONE / 2)]),
            [
                row(0, 120, SCROLL_ONE),
                stopped,
                row(1_500, 120, SCROLL_ONE / 2),
                row(2_000, 120, SCROLL_ONE / 2)
            ]
        );
    }
}
