//! One frame's state: the drained effects through the reducer, the fields
//! from the snapshot, every built-in timer's start, then the `when` timers.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use bms_model::{BEAT, DOMAIN, Lane, TimeUs};
use eval::Tally;
use rules::GaugeValue;

use super::schema::{
    COVER_PERMILLE, GAUGE_PERMILLE, Grade, LaneTimer, MAX_COUNT, RATE_BASIS_POINTS,
    READOUT_MAX_U32, lane_timer, timer,
};
use super::{BuildError, CoverSettings, DurationRatio, Play, SkinFrame};
use crate::expr::{Program, State, TIMER_OFF};
use crate::widget::gauge::GaugeLook;
use crate::widget::lanes::LaneView;

/// How long past the notes' end the play keeps playing before it
/// finishes, in ms (`play/BMSPlayer.java:66`).
const FINISH_MARGIN_MS: i64 = 5_000;

/// A full gauge's value per ‰ of the gauge field.
const GAUGE_UNIT: i64 = GaugeValue::FULL.0 / GAUGE_PERMILLE as i64;

/// A count as a field value, clamped to the field's range.
fn count(v: u32) -> i32 {
    i32::try_from(v).map_or(MAX_COUNT, |v| v.min(MAX_COUNT))
}

/// The targets `tally` has judged: its five grades, less the empty POORs
/// counted among its POORs when `empty_poor_counted` (the evaluator then
/// adds each to both counts, `eval/src/evaluator.rs:1179-1183`). `None`
/// when it counts more empty POORs than POORs.
fn judged_targets(tally: &Tally, empty_poor_counted: bool) -> Option<u64> {
    let c = &tally.counts;
    let poor = if empty_poor_counted {
        c.poor.checked_sub(tally.empty_poor)?
    } else {
        c.poor
    };
    // Five u32 sum to at most 5 · (2^32 − 1), well inside u64.
    Some(
        u64::from(c.perfect)
            + u64::from(c.great)
            + u64::from(c.good)
            + u64::from(c.bad)
            + u64::from(poor),
    )
}

/// The score rate of `ex` over `targets`, in basis points:
/// `⌊10 000 · ex / (2 · targets)⌋`, 10 000 with no target. Not capped at
/// 100 %: a custom EX weighting may pass it; only the field's `i32` range
/// bounds it. `10 000 · ex` is below 2^46 and `2 · targets` below 2^36, so
/// neither overflows `u64`.
fn rate_bp(ex: u32, targets: u64) -> i32 {
    let bp = u64::from(RATE_BASIS_POINTS.unsigned_abs());
    let Some(den) = targets.checked_mul(2).filter(|&d| d > 0) else {
        return RATE_BASIS_POINTS;
    };
    let rate = bp.saturating_mul(u64::from(ex)) / den;
    i32::try_from(rate).unwrap_or(i32::MAX)
}

/// The readout fields of one frame, `field::BPM_CURRENT_X100` to
/// `field::DURATION_VALID`, validated before anything is applied.
#[derive(Clone, Copy)]
struct Readouts {
    bpm_current: i32,
    bpm_min: i32,
    bpm_max: i32,
    bpm_current_valid: i32,
    bpm_range_valid: i32,
    remaining_s: i32,
    elapsed_s: i32,
    covers: Covers,
    hispeed: i32,
    hispeed_valid: i32,
    duration_ms: i32,
    green: i32,
    duration_valid: i32,
}

/// The cover fields of one frame, `field::COVER_PERMILLE` to
/// `field::COVER_ADJUSTING`: the stored settings, a play's and a loading
/// frame's alike.
#[derive(Clone, Copy)]
pub(crate) struct Covers {
    pub(crate) cover: i32,
    pub(crate) lift: i32,
    pub(crate) hidden: i32,
    pub(crate) absolute: i32,
    pub(crate) cover_enabled: i32,
    pub(crate) lift_enabled: i32,
    pub(crate) hidden_enabled: i32,
    pub(crate) adjusting: i32,
}

/// The cover fields of the stored settings `c`, or `None` when a setting
/// is above 1 000 ‰ or disagrees with the effective amount `lanes` paints
/// (stage3.md R4, R6).
pub(crate) fn covers(lanes: &LaneView, c: &CoverSettings) -> Option<Covers> {
    let max = COVER_PERMILLE.unsigned_abs();
    if [c.cover, c.lift, c.hidden]
        .into_iter()
        .any(|v| u32::from(v) > max)
    {
        return None;
    }
    // The lanes cap their amounts at 1 000 (`LaneView`); a cover that is
    // off draws 0 whatever it stores.
    let effective = |on: bool, v: u16| if on { v } else { 0 };
    let full = u16::try_from(max).ok()?;
    if lanes.cover.min(full) != effective(c.cover_enabled, c.cover)
        || lanes.lift.min(full) != effective(c.lift_enabled, c.lift)
        || lanes.hidden.min(full) != effective(c.hidden_enabled, c.hidden)
    {
        return None;
    }
    let (cover, lift) = (i32::from(c.cover), i32::from(c.lift));
    Some(Covers {
        cover,
        lift,
        hidden: i32::from(c.hidden),
        // Both at most 1 000, so the product fits.
        absolute: (COVER_PERMILLE - lift) * cover / COVER_PERMILLE,
        cover_enabled: i32::from(c.cover_enabled),
        lift_enabled: i32::from(c.lift_enabled),
        hidden_enabled: i32::from(c.hidden_enabled),
        adjusting: i32::from(c.adjusting),
    })
}

/// Reads every timer at `now` from `starts` into `ms`: the built-in ones,
/// then each `when` timer, which starts on its condition's rising edge
/// over `fields` and the built-in readings and turns off on its falling
/// edge. A condition `admit` rejects reads as false. A play and the
/// loading runtime read their timers through this one rule.
pub(crate) fn read_timers(
    now: TimeUs,
    fields: &[i32],
    starts: &mut [Option<TimeUs>],
    ms: &mut [i32],
    whens: &[Program],
    admit: impl Fn(usize) -> bool,
) {
    let builtin = usize::from(timer::BUILTIN);
    for (ms, start) in ms.iter_mut().zip(starts.iter()).take(builtin) {
        *ms = reading(now, *start);
    }
    let (Some((builtin_ms, when_ms)), Some(when_starts)) =
        (ms.split_at_mut_checked(builtin), starts.get_mut(builtin..))
    else {
        return;
    };
    let state = State {
        fields,
        timers: builtin_ms,
    };
    for (i, ((program, start), ms)) in whens
        .iter()
        .zip(when_starts.iter_mut())
        .zip(when_ms.iter_mut())
        .enumerate()
    {
        if !admit(i) || !program.eval_bool(&state) {
            *start = None;
        } else if start.is_none() {
            *start = Some(now);
        }
        *ms = reading(now, *start);
    }
}

/// An optional readout up to the maximum: its value (0 when absent) and
/// whether it is present. `None` past the maximum.
fn readout(v: Option<u32>) -> Option<(i32, i32)> {
    match v {
        None => Some((0, 0)),
        Some(v) if v <= READOUT_MAX_U32 => Some((i32::try_from(v).ok()?, 1)),
        Some(_) => None,
    }
}

/// `d` in ms rounded half up, exactly: `⌊n / d⌋`, plus 1 when the
/// remainder is at least `⌈d / 2⌉` (computed as `d / 2 + d % 2`, so
/// nothing overflows). `None` for a zero denominator or past the maximum.
fn round_duration(d: DurationRatio) -> Option<i32> {
    let (n, den) = (d.numerator, d.denominator);
    let q = n.checked_div(den)?;
    let r = n.checked_rem(den)?;
    let q = if r >= den / 2 + den % 2 { q + 1 } else { q };
    u32::try_from(q)
        .ok()
        .filter(|&q| q <= READOUT_MAX_U32)
        .and_then(|q| i32::try_from(q).ok())
}

/// Whole seconds of `us` µs (at least 0), up or down, bounded by
/// `i32::MAX`.
fn seconds(us: i128, up: bool) -> i32 {
    let us = us.max(0);
    let s = us / 1_000_000 + i128::from(up && us % 1_000_000 != 0);
    i32::try_from(s).unwrap_or(i32::MAX)
}

/// A timer's reading at `now`: elapsed ms since `start`, at least 0 (an
/// effect stamped after `now` reads 0), or [`TIMER_OFF`].
fn reading(now: TimeUs, start: Option<TimeUs>) -> i32 {
    start.map_or(TIMER_OFF, |s| {
        i32::try_from(elapsed_us(now, s) / 1_000).unwrap_or(i32::MAX)
    })
}

/// µs from `start` to `now`, at least 0: an effect stamped after `now`
/// shows as just started. Timers and tracks age by this one rule.
pub(crate) fn elapsed_us(now: TimeUs, start: TimeUs) -> i64 {
    now.0.saturating_sub(start.0).max(0)
}

impl Play {
    /// Refuses a poisoned play: one a build refused, until a fresh
    /// prepare.
    pub(crate) fn check(&self) -> Result<(), BuildError> {
        self.poisoned.map_or(Ok(()), Err)
    }

    /// Refuses the frame with `e` and poisons the play.
    fn refuse(&mut self, e: BuildError) -> Result<(), BuildError> {
        self.poisoned = Some(e);
        Err(e)
    }

    /// The frame's readout fields, or `None` when a readout is out of
    /// range or a stored cover setting disagrees with the lanes' effective
    /// amount (stage3.md R4, R6).
    fn readouts(&self, frame: &SkinFrame<'_>) -> Option<Readouts> {
        let r = &frame.readouts;
        let covers = covers(&frame.lanes, &r.covers)?;
        let (bpm_current, bpm_current_valid) = readout(r.bpm_current_x100)?;
        let (hispeed, hispeed_valid) = readout(r.hispeed_x100)?;
        let (duration_ms, duration_valid) = match r.duration {
            None => (0, 0),
            Some(d) => (round_duration(d)?, 1),
        };
        // Sequenced as the source: three fifths of the rounded duration
        // (`skin/property/IntegerPropertyFactory.java:292-314`).
        let green = i32::try_from(i64::from(duration_ms) * 3 / 5).ok()?;
        let (bpm_min, bpm_max, bpm_range_valid) = self.bpm_range.map_or(Some((0, 0, 0)), |b| {
            Some((
                i32::try_from(b.min_x100).ok()?,
                i32::try_from(b.max_x100).ok()?,
                1,
            ))
        })?;
        let now = i128::from(frame.now.0);
        let start = i128::from(self.play_at.0);
        let end = i128::from(self.readout_end.0);
        Some(Readouts {
            bpm_current,
            bpm_min,
            bpm_max,
            bpm_current_valid,
            bpm_range_valid,
            remaining_s: seconds(end - now.max(start), true),
            elapsed_s: seconds(now - start, false),
            covers,
            hispeed,
            hispeed_valid,
            duration_ms,
            green,
            duration_valid,
        })
    }

    /// Fills this frame's fields and timers: effects through the reducer,
    /// the snapshot into the fields, the built-in starts, then each `when`
    /// timer from its condition over the built-in timers only.
    ///
    /// A poisoned play ([`Play::check`]), a snapshot with more counted
    /// empty POORs than POORs, or invalid readouts ([`Play::readouts`]),
    /// in that order, is refused before any effect is applied: everything
    /// stays as it was, and the play is poisoned with that error until a
    /// fresh prepare.
    pub(crate) fn fill(
        &mut self,
        frame: &SkinFrame<'_>,
        whens: &[Program],
    ) -> Result<(), BuildError> {
        self.check()?;
        let Some(judged) = judged_targets(frame.tally, self.empty_poor_counted) else {
            return self.refuse(BuildError::InconsistentTally);
        };
        let Some(readouts) = self.readouts(frame) else {
            return self.refuse(BuildError::InvalidReadouts);
        };
        for ev in frame.effects {
            self.reducer.apply(ev);
        }
        self.fill_fields(frame, judged, &readouts);
        self.fill_builtin(frame);
        read_timers(
            frame.now,
            &self.fields,
            &mut self.starts,
            &mut self.ms,
            whens,
            |_| true,
        );
        Ok(())
    }

    /// The fields, in slot order (`field::*`): the snapshot's, `judged`
    /// its judged targets ([`judged_targets`]); the whole-attempt rate
    /// over the prepared target count, and the gauge kind from the shown
    /// gauge's descriptor, then the frame's `readouts`.
    fn fill_fields(&mut self, frame: &SkinFrame<'_>, judged: u64, readouts: &Readouts) {
        let r = readouts;
        let t = frame.tally;
        let gauge = frame.gauges.get(frame.gauge).map_or(0, |g| {
            let permille = (g.value.0 / GAUGE_UNIT).clamp(0, i64::from(GAUGE_PERMILLE));
            i32::try_from(permille).unwrap_or(0)
        });
        // The shown gauge's look, while it has a descriptor and a snapshot
        // (the gauge widget's rule); `normal` otherwise.
        let kind = frame
            .gauges
            .get(frame.gauge)
            .and(self.gauge_desc(frame.gauge))
            .map(|d| d.look);
        let targets = u64::from(self.reducer.targets());
        self.fields = [
            count(t.combo),
            count(t.max_combo),
            count(t.ex_score),
            count(t.bp),
            count(t.counts.perfect),
            count(t.counts.great),
            count(t.counts.good),
            count(t.counts.bad),
            count(t.counts.poor),
            count(t.empty_poor),
            count(self.reducer.targets()),
            gauge,
            // Loaded: a play's frames never load (the loading runtime's do).
            100,
            rate_bp(t.ex_score, judged),
            rate_bp(t.ex_score, targets),
            kind.unwrap_or(GaugeLook::Normal).schema_value(),
            i32::from(kind.is_some()),
            r.bpm_current,
            r.bpm_min,
            r.bpm_max,
            r.bpm_current_valid,
            r.bpm_range_valid,
            r.remaining_s,
            r.elapsed_s,
            r.covers.cover,
            r.covers.lift,
            r.covers.hidden,
            r.covers.absolute,
            r.covers.cover_enabled,
            r.covers.lift_enabled,
            r.covers.hidden_enabled,
            r.covers.adjusting,
            r.hispeed,
            r.hispeed_valid,
            r.duration_ms,
            r.green,
            r.duration_valid,
        ];
    }

    /// Every built-in timer's start at this frame.
    fn fill_builtin(&mut self, frame: &SkinFrame<'_>) {
        let now = frame.now;
        let ready = (now >= self.ready_at).then_some(self.ready_at);
        let play = (now >= self.play_at).then_some(self.play_at);
        let shown = frame.gauges.get(frame.gauge);
        let failed = shown.and_then(|g| g.failed_at);
        // The reducer's start, masked during READY and while the
        // snapshot is below the maximum: a dropped below-maximum effect
        // cannot leave it on (`play/BMSPlayer.java:639`).
        let gauge_max = match (
            play,
            self.reducer.gauge_max.get(frame.gauge).copied().flatten(),
            shown,
            self.reducer.gauge_top.get(frame.gauge),
        ) {
            (Some(_), Some(at), Some(g), Some(&top)) if g.value >= top => Some(at),
            _ => None,
        };
        // Sampled on whole ms of the play clock, at the first playing
        // frame past the notes' end and before the finish
        // (`play/BMSPlayer.java:683-697`); once set, only this keeps it.
        let mut last_note = self.timer(timer::LAST_NOTE);
        if last_note.is_none() && frame.playing {
            let at = now.0.div_euclid(1_000);
            let end = self.last_note_end.0.div_euclid(1_000);
            if end < at && at <= end.saturating_add(FINISH_MARGIN_MS) {
                last_note = Some(now);
            }
        }
        let beat = ready.and_then(|_| self.beat_start(now, frame.display_offset));
        let judge = self.reducer.judge;
        let mut set = |slot: u16, start: Option<TimeUs>| {
            if let Some(s) = self.starts.get_mut(usize::from(slot)) {
                *s = start;
            }
        };
        set(timer::READY, ready);
        set(timer::SCREEN, ready);
        set(timer::PLAY, play);
        set(timer::FAILED, failed);
        set(timer::LAST_NOTE, last_note);
        set(timer::FULL_COMBO, self.reducer.full_combo);
        set(timer::GAUGE_MAX, gauge_max);
        set(timer::BEAT, beat);
        set(timer::JUDGE, judge.map(|(_, at)| at));
        for grade in Grade::ALL {
            let at = judge.filter(|&(g, _)| g == grade).map(|(_, at)| at);
            set(grade.timer(), at);
        }
        set(timer::COMBO_UP, self.reducer.combo_up);
        // Loading is the loading runtime's (`loading.rs`).
        set(timer::LOADING, None);
        for lane in Lane::all() {
            let l = lane.index();
            let hold = self.reducer.hold_at(l, now).map(|h| h.start);
            let bomb = self.reducer.bomb.get(l).copied().flatten();
            let mine = self.reducer.mine.get(l).copied().flatten();
            // Press and release stay off until S9.
            set(lane_timer(LaneTimer::Bomb, lane), bomb);
            set(lane_timer(LaneTimer::Hold, lane), hold);
            set(lane_timer(LaneTimer::Mine, lane), mine);
        }
    }

    /// The `beat` timer's start: the latest integer-beat crossing on the
    /// display clock, `now + display_offset` clamped to the scroll map's
    /// domain, moved back to `now`'s clock. A STOP does not restart it.
    /// Off when that start leaves the time range.
    fn beat_start(&mut self, now: TimeUs, offset: TimeUs) -> Option<TimeUs> {
        let disp = now.0.saturating_add(offset.0).clamp(-DOMAIN, DOMAIN);
        let b = self.scroll.position_at(TimeUs(disp)).0.div_euclid(BEAT);
        let crossed = match self.beat {
            Some((cached, at)) if cached == b => at,
            _ => {
                let at = self.crossing(b, disp);
                self.beat = Some((b, at));
                at
            }
        };
        TimeUs(crossed).checked_sub(offset)
    }

    /// The first µs in `[-DOMAIN, disp]` where the display position reaches
    /// beat `b`, for `b` at or before `disp`'s beat. The map never
    /// decreases, so a binary search takes at most 34 probes.
    fn crossing(&self, b: i128, disp: i64) -> i64 {
        // `b` is the floor of a position over BEAT, so `b · BEAT` is at or
        // below that position and fits.
        let target = b.saturating_mul(BEAT);
        let (mut lo, mut hi) = (-DOMAIN, disp);
        while lo < hi {
            // Both are within ±2^32, so the sum fits.
            let mid = (lo + hi).div_euclid(2);
            if self.scroll.position_at(TimeUs(mid)).0 >= target {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        hi
    }
}
