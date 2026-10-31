//! One frame's state: the drained effects through the reducer, the fields
//! from the snapshot, every built-in timer's start, then the `when` timers.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use bms_model::{BEAT, DOMAIN, Lane, TimeUs};
use rules::GaugeValue;

use super::schema::{GAUGE_PERMILLE, Grade, LaneTimer, MAX_COUNT, lane_timer, timer};
use super::{Play, SkinFrame};
use crate::expr::{Program, State, TIMER_OFF};

/// How long past the notes' end the play keeps playing before it
/// finishes, in ms (`play/BMSPlayer.java:66`).
const FINISH_MARGIN_MS: i64 = 5_000;

/// A full gauge's value per ‰ of the gauge field.
const GAUGE_UNIT: i64 = GaugeValue::FULL.0 / GAUGE_PERMILLE as i64;

/// A count as a field value, clamped to the field's range.
fn count(v: u32) -> i32 {
    i32::try_from(v).map_or(MAX_COUNT, |v| v.min(MAX_COUNT))
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
    /// Fills this frame's fields and timers: effects through the reducer,
    /// the snapshot into the fields, the built-in starts, then each `when`
    /// timer from its condition over the built-in timers only.
    pub(crate) fn fill(&mut self, frame: &SkinFrame<'_>, whens: &[Program]) {
        for ev in frame.effects {
            self.reducer.apply(ev);
        }
        self.fill_fields(frame);
        self.fill_builtin(frame);
        let now = frame.now;
        let builtin = usize::from(timer::BUILTIN);
        for (ms, start) in self.ms.iter_mut().zip(&self.starts).take(builtin) {
            *ms = reading(now, *start);
        }
        let (Some((builtin_ms, when_ms)), Some(when_starts)) = (
            self.ms.split_at_mut_checked(builtin),
            self.starts.get_mut(builtin..),
        ) else {
            return;
        };
        let state = State {
            fields: &self.fields,
            timers: builtin_ms,
        };
        for ((program, start), ms) in whens
            .iter()
            .zip(when_starts.iter_mut())
            .zip(when_ms.iter_mut())
        {
            if !program.eval_bool(&state) {
                *start = None;
            } else if start.is_none() {
                *start = Some(now);
            }
            *ms = reading(now, *start);
        }
    }

    /// The fields, in slot order (`field::*`), from the snapshot alone.
    fn fill_fields(&mut self, frame: &SkinFrame<'_>) {
        let t = frame.tally;
        let gauge = frame.gauges.get(frame.gauge).map_or(0, |g| {
            let permille = (g.value.0 / GAUGE_UNIT).clamp(0, i64::from(GAUGE_PERMILLE));
            i32::try_from(permille).unwrap_or(0)
        });
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
        ];
    }

    /// Every built-in timer's start at this frame.
    fn fill_builtin(&mut self, frame: &SkinFrame<'_>) {
        let now = frame.now;
        let ready = (now >= self.ready_at).then_some(self.ready_at);
        let play = (now >= self.play_at).then_some(self.play_at);
        let shown = frame.gauges.get(frame.gauge);
        let failed = shown.and_then(|g| g.failed_at);
        // The reducer's start, masked during the countdown and while the
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
