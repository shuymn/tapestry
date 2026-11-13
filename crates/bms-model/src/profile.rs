//! A chart's profile for its notes and tempo graphs (stage 3, S10-10): the
//! notes of each second by category and the tempo rows, over the chart's
//! 1× times. The loader builds it before it remaps the times to a practice
//! speed, the only point that holds them (design.md "Practice Speed
//! Changes").

use crate::{Chart, Lane, NoteKind, Tempo, TimeUs, TimelineId};

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
pub const SCROLL_ONE: i128 = 1_000_000_000_000_000_000;

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
    /// The latest hidden note (channels 31–39), BGA event (04) or layer
    /// event (07), which lengthen the seconds the preview counts over.
    pub last_shown: Option<TimeUs>,
    /// The scroll changes (channel `SC` with `#SCROLLxx`) in time order, one
    /// per time, each the factor in units of `10^-18` from then on.
    pub scroll: Vec<(TimeUs, i128)>,
}

/// A chart's notes by second and its tempo rows, at 1× times.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartProfile {
    seconds: Vec<[u32; NOTE_CATEGORIES]>,
    bins: Vec<[u16; NOTE_CATEGORIES]>,
    bin_seconds: u32,
    speeds: Vec<SpeedRow>,
}

impl ChartProfile {
    /// The profile of `chart`, whose times must be its 1× times and whose
    /// `meta.length` its summary's length. `long_stops` are the rows, in
    /// order, where a STOP of at least 1 ms starts: its own length
    /// truncated to µs, which the rows' floored times cannot give.
    #[must_use]
    pub fn of(chart: &Chart, long_stops: &[TimelineId]) -> Self {
        let seconds = seconds(chart, chart.meta.length);
        let (bins, bin_seconds) = condense(&seconds);
        Self {
            seconds,
            bins,
            bin_seconds,
            speeds: speeds(chart, long_stops, &PreviewInputs::default()),
        }
    }

    /// A song list's preview profile of `chart` ([`of`](Self::of)'s
    /// arguments) with what the loader timed for it (`inputs`), as the
    /// reference player's song information reads a chart: its seconds run
    /// to the later of the length and [`PreviewInputs::last_shown`], and its
    /// tempo rows carry the scroll factor, a scroll change being a point of
    /// its own, the last row at the latest point, scroll change or shown
    /// object (beatoraja `song/SongInformation.java:102`, `:180-210`;
    /// jbms-parser `BMSModel.getLastMilliTime`).
    #[must_use]
    pub fn preview(chart: &Chart, long_stops: &[TimelineId], inputs: &PreviewInputs) -> Self {
        let end = inputs
            .last_shown
            .map_or(chart.meta.length, |t| t.max(chart.meta.length));
        let seconds = seconds(chart, end);
        let (bins, bin_seconds) = condense(&seconds);
        Self {
            seconds,
            bins,
            bin_seconds,
            speeds: speeds(chart, long_stops, inputs),
        }
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
    /// start or a bar line, never a STOP's end) a stopped row where a STOP
    /// of at least 1 ms starts (one of the `long_stops` given to
    /// [`of`](Self::of)) unless already stopped, otherwise a row
    /// where the tempo differs from the current one; and the current tempo
    /// at the last point unless a row is already there.
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

fn speeds(chart: &Chart, long_stops: &[TimelineId], inputs: &PreviewInputs) -> Vec<SpeedRow> {
    let points = chart.timeline.points();
    let mut tempo = chart.timeline.meta().initial;
    let mut current = Some(tempo);
    let mut factor = SCROLL_ONE;
    let mut rows = vec![SpeedRow {
        ms: 0,
        tempo: current,
        scroll: factor,
    }];
    let mut changes = inputs.scroll.iter().peekable();
    let mut last = None;
    // A scroll change before the next point: a point of its own at the
    // current tempo, a STOP being over by then (a later position is a
    // later time than the STOP's end).
    let scroll_point = |rows: &mut Vec<SpeedRow>, current: &mut Option<Tempo>, at, f, tempo| {
        let ms = ms_of(at);
        if *current != Some(tempo) || rows.last().is_some_and(|r| r.scroll != f) {
            *current = Some(tempo);
            rows.push(SpeedRow {
                ms,
                tempo: *current,
                scroll: f,
            });
        }
        ms
    };
    for (i, p) in points.iter().enumerate() {
        // A STOP's end is the row after its start: the same position,
        // later. It is no point of its own.
        let prev = i.checked_sub(1).and_then(|j| points.get(j));
        if prev.is_some_and(|q| q.pos == p.pos && q.at < p.at) {
            continue;
        }
        while let Some(&&(at, f)) = changes.peek()
            && at < p.at
        {
            changes.next();
            factor = f;
            scroll_point(&mut rows, &mut current, at, f, tempo);
        }
        // A change at this point's time applies at it.
        while let Some(&&(at, f)) = changes.peek()
            && at == p.at
        {
            changes.next();
            factor = f;
        }
        tempo = p.tempo;
        let id = TimelineId(u32::try_from(i).unwrap_or(u32::MAX));
        let ms = ms_of(p.at);
        let scrolled = rows.last().is_some_and(|r| r.scroll != factor);
        if long_stops.binary_search(&id).is_ok() {
            if current.is_some() {
                current = None;
                rows.push(SpeedRow {
                    ms,
                    tempo: None,
                    scroll: factor,
                });
            }
        } else if current != Some(p.tempo) || scrolled {
            current = Some(p.tempo);
            rows.push(SpeedRow {
                ms,
                tempo: current,
                scroll: factor,
            });
        }
        last = Some(ms);
    }
    for &(at, f) in changes {
        factor = f;
        last = Some(scroll_point(&mut rows, &mut current, at, f, tempo));
    }
    let shown = inputs.last_shown.map(ms_of);
    let last = match (last, shown) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    if let Some(ms) = last
        && rows.last().is_none_or(|r| r.ms != ms)
    {
        rows.push(SpeedRow {
            ms,
            tempo: current,
            scroll: factor,
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{ChartProfile, PreviewInputs, SCROLL_ONE, SpeedRow};
    use crate::{Chart, ChartBuilder, ChartSummary, Lane, Tempo, TimeUs};

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
            last_shown: Some(TimeUs(4_200_000)),
            scroll: Vec::new(),
        };
        let preview = ChartProfile::preview(&chart, &[], &inputs);
        assert_eq!(preview.seconds().len(), 6);
        assert_eq!(ChartProfile::of(&chart, &[]).seconds().len(), 5);
        assert_eq!(preview.speeds().last(), Some(&row(4_200, 120, SCROLL_ONE)));
        // A shown object before the length changes nothing.
        let early = PreviewInputs {
            last_shown: Some(TimeUs(500_000)),
            scroll: Vec::new(),
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
        let inputs = PreviewInputs {
            last_shown: None,
            scroll: vec![
                (TimeUs(1_000_000), SCROLL_ONE / 2),
                (TimeUs(1_500_000), SCROLL_ONE / 2),
                (TimeUs(2_000_000), 2 * SCROLL_ONE),
                (TimeUs(2_500_000), 0),
            ],
        };
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
        let inputs = PreviewInputs {
            last_shown: None,
            scroll: vec![(TimeUs(5_000_000), 3 * SCROLL_ONE)],
        };
        let speeds = ChartProfile::preview(&four_taps(), &[], &inputs);
        assert_eq!(
            speeds.speeds().last(),
            Some(&row(5_000, 120, 3 * SCROLL_ONE))
        );
    }
}
