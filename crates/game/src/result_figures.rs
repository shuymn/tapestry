//! What a result view's skin reads besides the totals (design.md "Play
//! Lifecycle", the result's views): the rates, grade and clear of the view
//! ([`ResultFigures`]), its timing statistics from exact sums
//! ([`TimingStats`]) and its judgments over chart time ([`Timelines`]).
//! Each is made when a view is summarized, from its evaluation, in
//! integers, and is a fixed size whatever the play.

use bms_model::{Chart, NoteKind, TimeUs};
use eval::{Evaluation, JudgeEvent, JudgeEventKind};
use replay::verify::Coverage;
use rules::{GaugeValue, Judgment, RulePreset};

use crate::bests::BestLamp;

/// The width of a timeline bin up to [`MAX_TIMELINE_BINS`] bins: 1 s.
pub const TIMELINE_BIN_US: i64 = 1_000_000;

/// The most bins a [`Timelines`] holds; a longer span widens every bin.
pub const MAX_TIMELINE_BINS: usize = 512;

/// The channels of a timeline bin: PERFECT; then GREAT, GOOD, BAD and POOR
/// early; then the same four late.
pub const TIMELINE_CHANNELS: usize = 9;

/// The farthest sample from its note the timing statistics keep, in µs:
/// the presses whose ms value, truncated toward zero, lies within ±150.
pub const TIMING_RANGE_US: i64 = 150_999;

/// The rank thresholds in 27ths of the largest EX score, F to AAA.
const RANK_LOW: [u128; 8] = [0, 6, 9, 12, 15, 18, 21, 24];

/// What a result's views share: the attempt's judged notes, the timelines'
/// bins and the judged notes in each, all fixed by the chart, whether a
/// rate over the best can be known, and whether the attempt was a section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultBasis {
    /// The judged notes of the attempt: taps and long notes, within a
    /// practice round's section.
    pub notes: u32,
    /// The timelines' bins.
    pub timeline: TimelineAxis,
    /// The judged notes of the attempt in each timeline bin, by their
    /// time; bins past `timeline.count` stay 0.
    pub notes_per_bin: [u32; MAX_TIMELINE_BINS],
    /// Whether the chart declares `#RANDOM` or `#SETRANDOM`: its best may
    /// come from a draw with another note count, so no best rate is known.
    pub random: bool,
    /// Whether the attempt was a practice round's section: its notes are
    /// the section's while a best is the whole chart's, so nothing is
    /// compared with a best.
    pub sectioned: bool,
}

impl ResultBasis {
    /// The basis of an attempt at `chart`, within `section` (its start and
    /// the time just past it) for a practice round. The last note, which
    /// fixes the bins' width, is known only once every lane is read, so
    /// the notes are binned in a second pass.
    #[must_use]
    pub fn of(chart: &Chart, section: Option<(TimeUs, TimeUs)>) -> Self {
        let judged = |note: &&bms_model::Note| {
            !matches!(note.kind, NoteKind::Mine { .. })
                && section.is_none_or(|(start, end)| {
                    start <= note.time && note.time < end && note.end() < end
                })
        };
        let mut notes = 0_u32;
        let mut last = None;
        for note in chart.lanes.iter().flatten().filter(judged) {
            notes = notes.saturating_add(1);
            last = last.max(Some(note.time));
        }
        let origin = section.map_or(TimeUs(0), |(start, _)| start);
        let timeline = TimelineAxis::new(origin, last.unwrap_or(origin));
        let mut notes_per_bin = [0_u32; MAX_TIMELINE_BINS];
        for note in chart.lanes.iter().flatten().filter(judged) {
            if let Some(n) = timeline
                .bin(note.time)
                .and_then(|b| notes_per_bin.get_mut(usize::from(b)))
            {
                *n = n.saturating_add(1);
            }
        }
        Self {
            notes,
            timeline,
            notes_per_bin,
            random: !chart.identity.random_results.is_empty(),
            sectioned: section.is_some(),
        }
    }

    /// The judged notes in each bin in use.
    #[must_use]
    pub fn notes_used(&self) -> &[u32] {
        self.notes_per_bin
            .get(..usize::from(self.timeline.count))
            .unwrap_or(&self.notes_per_bin)
    }
}

/// A rate over the largest EX score.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    /// `⌊10 000 · ex / max_ex⌋`.
    pub rate_bp: u32,
    /// 0 (F) to 7 (AAA): the greatest `k` with `27 · ex ≥ low_k · max_ex`,
    /// `low` being 0, 6, 9, …, 24.
    pub grade: u8,
}

impl Rate {
    /// The rate of `ex` over `max_ex`; `None` when `max_ex` is 0.
    #[must_use]
    pub fn of(ex: u32, max_ex: u32) -> Option<Self> {
        if max_ex == 0 {
            return None;
        }
        let (e, m) = (u128::from(ex), u128::from(max_ex));
        let grade = RANK_LOW
            .iter()
            .rposition(|&low| 27 * e >= low * m)
            .unwrap_or(0);
        Some(Self {
            rate_bp: u32::try_from(10_000 * e / m).unwrap_or(u32::MAX),
            grade: u8::try_from(grade).unwrap_or(0),
        })
    }
}

/// The EX score from `ex` to the next grade step of 3/27 over `max_ex`, or
/// to `max_ex` past the last step: the first `i` of 0, 3, …, 24 that `ex`
/// does not meet gives `⌈i · max_ex / 27⌉ − ex`.
#[must_use]
pub fn next_rank(ex: u32, max_ex: u32) -> u32 {
    let (e, m) = (u128::from(ex), u128::from(max_ex));
    let next = (0..27_u128)
        .step_by(3)
        .find(|&i| 27 * e < i * m)
        .map_or(m.saturating_sub(e), |i| {
            (i * m).div_ceil(27).saturating_sub(e)
        });
    u32::try_from(next).unwrap_or(u32::MAX)
}

/// The integers of one view's result a skin reads beside its totals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultFigures {
    /// The judged notes of the attempt ([`ResultBasis::notes`]).
    pub notes: u32,
    /// The largest EX score: the judged notes at the preset's largest EX
    /// weight, at most `u32::MAX` as the evaluator's EX score.
    pub max_ex: u32,
    /// The view's rate; `None` when `max_ex` is 0.
    pub rate: Option<Rate>,
    /// The EX score to the next grade ([`next_rank`]).
    pub next_rank: u32,
    /// The view's lamp, by the bests' rule: FULL COMBO, else the cleared
    /// gauge of the highest lamp rank, else failed.
    pub clear: BestLamp,
    /// Whether the view cleared: its lamp ([`clear`](Self::clear)) is not
    /// failed, so the clear and fail options never disagree with the lamp,
    /// whichever gauge is emphasized.
    pub cleared: bool,
    /// The emphasized gauge's end value in tenths of a percent, rounded
    /// down, 1 for a value strictly between 0 and 0.1 %.
    pub gauge_permille: u16,
    /// The timing statistics; `None` without a sample.
    pub timing: Option<TimingStats>,
    /// The mean absolute duration of the attempt's notes in µs
    /// ([`mean_duration`]); `None` with no note.
    pub duration_us: Option<u32>,
    /// The key lanes' judge areas a timing distribution draws
    /// ([`judge_areas`]).
    pub areas: [[i16; 2]; 5],
}

impl ResultFigures {
    /// The figures of `e`, evaluated on `chart` under `preset`, over the
    /// attempt's `notes`, its gauge at index `emphasized` shown.
    #[must_use]
    pub fn of(
        e: &Evaluation,
        chart: &Chart,
        preset: &RulePreset,
        notes: u32,
        emphasized: Option<usize>,
    ) -> Self {
        let w = preset.score.ex;
        let weight = [w.perfect, w.great, w.good, w.bad, w.poor]
            .into_iter()
            .max()
            .unwrap_or(0);
        let max_ex = u32::try_from(u64::from(notes) * u64::from(weight)).unwrap_or(u32::MAX);
        let ex = e.tally.ex_score;
        let shown = emphasized.and_then(|i| e.gauges.get(i));
        let clear = lamp(e, preset);
        Self {
            notes,
            max_ex,
            rate: Rate::of(ex, max_ex),
            next_rank: next_rank(ex, max_ex),
            clear,
            cleared: clear != BestLamp::Failed,
            gauge_permille: shown.map_or(0, |g| permille(g.value)),
            timing: TimingStats::of(&e.events),
            duration_us: mean_duration(&e.events, notes),
            areas: judge_areas(chart, preset),
        }
    }
}

/// The key lanes' judge areas of `preset` for `chart`: its windows as the
/// evaluator prepares them for the chart's rank, PERFECT, GREAT, GOOD and
/// BAD as `[−late, early]` and the empty POOR range as `[−behind, ahead]`,
/// in ms truncated toward 0, early positive, each within `i16`. All 0 when
/// the windows cannot be prepared, which no evaluated view has.
#[must_use]
pub fn judge_areas(chart: &Chart, preset: &RulePreset) -> [[i16; 2]; 5] {
    let headers = preset.headers.read(&chart.meta.judgment_headers);
    let Some(tables) = preset.window_policy.prepare(&preset.tables, headers.rank) else {
        return [[0; 2]; 5];
    };
    let ms = |t: TimeUs| {
        let v = (t.0 / 1_000).clamp(i64::from(i16::MIN), i64::from(i16::MAX));
        i16::try_from(v).unwrap_or(0)
    };
    let k = &tables.keys;
    let area = |late: TimeUs, early: TimeUs| [ms(TimeUs(late.0.saturating_neg())), ms(early)];
    let w = &k.windows;
    [
        area(w.perfect.late, w.perfect.early),
        area(w.great.late, w.great.early),
        area(w.good.late, w.good.early),
        area(w.bad.late, w.bad.early),
        area(k.empty_poor.behind, k.empty_poor.ahead),
    ]
}

/// `v` in tenths of a percent, rounded down, at least 1 above 0.
fn permille(v: GaugeValue) -> u16 {
    let tenth = GaugeValue::PERCENT / 10;
    let p = v.0.max(0) / tenth;
    let p = if p == 0 && v.0 > 0 { 1 } else { p };
    u16::try_from(p).unwrap_or(u16::MAX)
}

/// The lamp of `e` under `preset`, as a best's is drawn: a FULL COMBO needs
/// a completed evaluation, no judgment the preset counts as breaking the
/// combo and, when an empty POOR breaks it, no empty POOR.
fn lamp(e: &Evaluation, preset: &RulePreset) -> BestLamp {
    let broken = e.events.iter().any(|ev| {
        matches!(ev.kind, JudgeEventKind::Judged { judgment, .. }
            if *preset.score.breaks_combo.get(judgment))
    });
    let empty_poor = preset.empty_poor.breaks_combo && e.tally.empty_poor > 0;
    if e.completed && !broken && !empty_poor {
        return BestLamp::FullCombo;
    }
    preset
        .gauges
        .iter()
        .zip(&e.gauges)
        .enumerate()
        .filter(|(_, (_, g))| g.cleared)
        .max_by_key(|(_, (rule, _))| rule.lamp)
        .and_then(|(i, _)| u16::try_from(i).ok())
        .map_or(BestLamp::Failed, |gauge| BestLamp::Cleared { gauge })
}

/// The timing statistics of one view (design.md "Play Lifecycle"): each
/// press judged on its note within [`TIMING_RANGE_US`] is a sample, early
/// positive; a long note gives one, which a release that finished it
/// makes the farther of its head's press and its release from its time,
/// the release on a tie, as the tally's side ([`side_deviation`]), and
/// otherwise its head's. Misses and empty POORs are none. The sums are
/// exact; only the outputs are truncated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimingStats {
    /// The samples, at least one.
    pub samples: u32,
    /// The mean in hundredths of a ms, truncated toward 0.
    pub mean_x100: i32,
    /// The population standard deviation in hundredths of a ms, rounded
    /// down.
    pub sd_x100: u32,
}

impl TimingStats {
    /// The statistics of `events`; `None` without a sample. One pass, no
    /// allocation.
    #[must_use]
    pub fn of(events: &[JudgeEvent]) -> Option<Self> {
        let (mut n, mut sum, mut squares) = (0_u32, 0_i64, 0_i128);
        for s in events.iter().filter_map(timing_sample) {
            n = n.saturating_add(1);
            sum = sum.saturating_add(s);
            squares = squares.saturating_add(i128::from(s) * i128::from(s));
        }
        if n == 0 {
            return None;
        }
        let count = i128::from(n);
        let mean_x100 = i128::from(sum) / (10 * count);
        let variance = (count * squares - i128::from(sum) * i128::from(sum)).max(0);
        let sd_x100 = variance.unsigned_abs().isqrt() / (10 * count.unsigned_abs());
        Some(Self {
            samples: n,
            mean_x100: i32::try_from(mean_x100).unwrap_or(0),
            sd_x100: u32::try_from(sd_x100).unwrap_or(u32::MAX),
        })
    }
}

/// The timing sample of `e` in µs, early positive ([`TimingStats`]): a
/// press judged on its note, a long note's by the tally's side when a
/// release finished it, within [`TIMING_RANGE_US`]; `None` for anything
/// else, a miss and an empty POOR among them.
#[must_use]
pub fn timing_sample(e: &JudgeEvent) -> Option<i64> {
    let JudgeEventKind::Judged {
        deviation: Some(head),
        release,
        ..
    } = e.kind
    else {
        return None;
    };
    let d = side_deviation(Some(head), release).unwrap_or(head);
    let s = d.0.saturating_neg();
    (s.unsigned_abs() <= TIMING_RANGE_US.unsigned_abs()).then_some(s)
}

/// What a note judged other than PERFECT to BAD, or left unjudged, adds
/// to [`mean_duration`]'s sum: one second, as the source adds it.
pub const MISSED_DURATION_US: u64 = 1_000_000;

/// The mean absolute duration of the attempt's `notes` in µs, the source's
/// penalized mean (0.8.9 `play/BMSPlayer.java:917-936`, shown as numbers
/// 372 and 373, `skin/property/IntegerPropertyFactory.java:410-420`):
/// each note judged PERFECT to BAD adds the absolute deviation of its
/// press, a long note's by the tally's side as [`timing_sample`] takes it
/// but with no range filter, and every other note, a POOR, a miss or one
/// left unjudged after an abort, adds [`MISSED_DURATION_US`]; the sum is
/// divided by `notes`, rounded down. A long note is one note, as the
/// evaluator judges it once, where the source counts a long note's end
/// apart in some modes. `None` when `notes` is 0. One pass, no
/// allocation, integers only.
#[must_use]
pub fn mean_duration(events: &[JudgeEvent], notes: u32) -> Option<u32> {
    if notes == 0 {
        return None;
    }
    let (mut judged, mut sum) = (0_u32, 0_u64);
    for e in events {
        let JudgeEventKind::Judged {
            judgment,
            deviation,
            release,
            ..
        } = e.kind
        else {
            continue;
        };
        judged = judged.saturating_add(1);
        let pressed = matches!(
            judgment,
            Judgment::Perfect | Judgment::Great | Judgment::Good | Judgment::Bad
        );
        let d = deviation.map(|head| side_deviation(Some(head), release).unwrap_or(head));
        let add = match (pressed, d) {
            (true, Some(d)) => d.0.unsigned_abs(),
            _ => MISSED_DURATION_US,
        };
        sum = sum.saturating_add(add);
    }
    let unjudged = u64::from(notes.saturating_sub(judged));
    sum = sum.saturating_add(unjudged.saturating_mul(MISSED_DURATION_US));
    u32::try_from(sum / u64::from(notes)).ok()
}

/// The bins of a result's timelines over chart time: from `origin`, each
/// `width` wide, `count` of them, the last holding the attempt's last
/// judged note. 1 s bins, or, past [`MAX_TIMELINE_BINS`] of them, the
/// whole second that keeps the count within it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineAxis {
    /// The play's start: chart time 0, or a practice round's section
    /// start.
    pub origin: TimeUs,
    /// Each bin's width.
    pub width: TimeUs,
    /// The bins, 1 to [`MAX_TIMELINE_BINS`].
    pub count: u16,
}

impl TimelineAxis {
    /// The bins from `origin` to `last`, the attempt's last judged note.
    #[must_use]
    pub fn new(origin: TimeUs, last: TimeUs) -> Self {
        let span = last.0.saturating_sub(origin.0).max(0).unsigned_abs();
        let bin = TIMELINE_BIN_US.unsigned_abs();
        let max = MAX_TIMELINE_BINS as u64;
        let width = if span / bin < max {
            bin
        } else {
            span.div_ceil(max - 1).div_ceil(bin) * bin
        };
        let (span, width) = (
            i64::try_from(span).unwrap_or(i64::MAX),
            i64::try_from(width).unwrap_or(i64::MAX),
        );
        Self {
            origin,
            width: TimeUs(width),
            count: u16::try_from(span / width + 1).unwrap_or(u16::MAX),
        }
    }

    /// The bin chart time `t` falls in: `None` before the origin, the last
    /// at or past its end.
    #[must_use]
    pub fn bin(self, t: TimeUs) -> Option<u16> {
        let at = t.0.checked_sub(self.origin.0)?;
        if at < 0 {
            return None;
        }
        let b = (at / self.width.0.max(1)).min(i64::from(self.count) - 1);
        u16::try_from(b).ok()
    }
}

/// One view's judgments over chart time (design.md "Play Lifecycle"). A
/// judgment counts when it was finalized at or before the coverage's end,
/// in its note's bin whatever its finalization time, in the channel of its
/// judgment and side (early: the side [`eval::Tally`] counts it on).
/// Mines and empty POORs are no notes of it. The bins from
/// `uncovered_from` on are past what the input covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timelines {
    /// The bins.
    pub axis: TimelineAxis,
    /// Each bin's channels; bins past `axis.count` stay 0.
    pub bins: [[u32; TIMELINE_CHANNELS]; MAX_TIMELINE_BINS],
    /// The first bin after both the bin holding the coverage's end and
    /// the last bin holding a counted judgment; `None` when every bin is
    /// covered.
    pub uncovered_from: Option<u16>,
}

impl Timelines {
    /// No judgment, every bin covered.
    pub const EMPTY: Self = Self {
        axis: TimelineAxis {
            origin: TimeUs(0),
            width: TimeUs(TIMELINE_BIN_US),
            count: 1,
        },
        bins: [[0; TIMELINE_CHANNELS]; MAX_TIMELINE_BINS],
        uncovered_from: None,
    };

    /// The timelines of `events`, evaluated on `chart`, on `axis`, the
    /// input covering `coverage`. One pass, no allocation.
    pub fn fill(
        &mut self,
        events: &[JudgeEvent],
        chart: &Chart,
        axis: TimelineAxis,
        coverage: Coverage,
    ) {
        *self = Self::EMPTY;
        self.axis = axis;
        let end = match coverage {
            Coverage::Recorded => None,
            Coverage::NoInputAfter(t) | Coverage::EndedMidway(t) => Some(t),
        };
        let mut last: Option<u16> = None;
        for e in events {
            let JudgeEventKind::Judged {
                lane,
                note,
                judgment,
                deviation,
                release,
                ..
            } = e.kind
            else {
                continue;
            };
            if end.is_some_and(|t| e.time > t) {
                continue;
            }
            let Some(at) = chart
                .lanes
                .get(lane.index())
                .and_then(|l| l.get(note))
                .and_then(|n| axis.bin(n.time))
            else {
                continue;
            };
            let Some(slot) = self
                .bins
                .get_mut(usize::from(at))
                .and_then(|b| b.get_mut(channel(judgment, deviation, release)))
            else {
                continue;
            };
            *slot = slot.saturating_add(1);
            last = last.max(Some(at));
        }
        self.uncovered_from = end.and_then(|t| {
            let from = axis.bin(t).max(last).map_or(0, |b| b.saturating_add(1));
            (from < axis.count).then_some(from)
        });
    }

    /// The bins in use.
    #[must_use]
    pub fn used(&self) -> &[[u32; TIMELINE_CHANNELS]] {
        self.bins
            .get(..usize::from(self.axis.count))
            .unwrap_or(&self.bins)
    }
}

/// The channel of a judgment: PERFECT 0; GREAT to POOR 1 to 4 early and 5
/// to 8 late. The side is [`eval::Tally`]'s: a long note's by whichever of
/// its head's press and its release lies farther from its time, the
/// release on a tie; early at a deviation of 0 or less; a miss late.
fn channel(judgment: Judgment, deviation: Option<TimeUs>, release: Option<TimeUs>) -> usize {
    let grade = match judgment {
        Judgment::Perfect => return 0,
        Judgment::Great => 0,
        Judgment::Good => 1,
        Judgment::Bad => 2,
        Judgment::Poor => 3,
    };
    if side_deviation(deviation, release).is_some_and(|d| d.0 <= 0) {
        1 + grade
    } else {
        5 + grade
    }
}

/// The deviation that decides a judgment's side, as [`eval::Tally`] counts
/// it (design.md "Evaluation Stages", accounting): for a long note
/// finished by a release, whichever of its head's press and its release
/// lies farther from its time, the release on a tie; otherwise the
/// press's, `None` for a miss.
fn side_deviation(deviation: Option<TimeUs>, release: Option<TimeUs>) -> Option<TimeUs> {
    match (deviation, release) {
        (Some(head), Some(r)) if head.0.unsigned_abs() > r.0.unsigned_abs() => Some(head),
        (_, Some(r)) => Some(r),
        (head, None) => head,
    }
}

#[cfg(test)]
#[path = "result_figures_tests.rs"]
mod tests;
