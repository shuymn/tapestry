//! The mix clock: which judgment-timeline time each output frame is mixed at
//! (design.md "Aligning Audio to the Judgment Timeline").
//!
//! At each buffer event the audio thread observes the first frame still
//! queued (frames written minus padding) and the time of the engine pass
//! that reads it: the wake plus one period. Wakes come late by a
//! scheduler-dependent amount; engine passes do not. Over a sliding window
//! of [`MIX_CLOCK_WINDOW`] observations, the earliest residual (observation
//! minus `frame / rate`) is the least-late wake, which bounds the clock's
//! error by that smallest lateness instead of the latest wake's. Every
//! observation re-anchors the window, so the mapping follows the device
//! clock's drift against QPC: a residual that falls steadily (a fast
//! device) is always the newest minimum, and one that rises (a slow device)
//! lags by at most the drift over one window.
//!
//! The window holds only while the frame/time relation does. The caller
//! resets the clock when a wake finds the buffer empty (an underrun), and
//! the clock rebases itself when one observation lies more than half a
//! period below the window's earliest, or [`MIX_CLOCK_REBASE_AFTER`] in a
//! row lie more than half a period above it (a jump such as resume).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;

/// Observations in the sliding window.
pub const MIX_CLOCK_WINDOW: usize = 128;
/// Consecutive observations above the window that rebase it.
pub const MIX_CLOCK_REBASE_AFTER: usize = 8;

/// Frame-to-time mapping of the output stream. Allocates only in
/// [`MixClock::new`].
#[derive(Clone, Debug)]
pub struct MixClock {
    rate: u32,
    /// Half a period, µs: the jump that rebases the window.
    jump_us: i64,
    /// Residuals in µs (time minus `frame / rate`), a ring.
    residuals: Vec<i64>,
    len: usize,
    next: usize,
    /// Consecutive observations above the window's earliest by `jump_us`.
    above: usize,
    /// The window's earliest residual and the time it was taken when the
    /// window first filled after a reset, for the drift estimate.
    drift_anchor: Option<(i64, i64)>,
    /// The latest observation's time and the window's earliest residual.
    latest: Option<(i64, i64)>,
    rebases: u64,
}

impl MixClock {
    /// A clock for a stream at `rate` frames per second whose engine passes
    /// are `period` frames apart. `rate` must be nonzero.
    #[must_use]
    pub fn new(rate: u32, period: u32) -> Self {
        Self {
            rate: rate.max(1),
            jump_us: i64::from(period) * 1_000_000 / i64::from(rate.max(1)) / 2,
            residuals: vec![0; MIX_CLOCK_WINDOW],
            len: 0,
            next: 0,
            above: 0,
            drift_anchor: None,
            latest: None,
            rebases: 0,
        }
    }

    /// Forgets every observation (the frame/time relation may have moved).
    pub fn reset(&mut self) {
        self.len = 0;
        self.next = 0;
        self.above = 0;
        self.drift_anchor = None;
        self.latest = None;
        self.rebases += 1;
    }

    /// Times the window was reset or rebased.
    #[must_use]
    pub fn rebases(&self) -> u64 {
        self.rebases
    }

    fn frame_us(&self, frame: u64) -> i128 {
        i128::from(frame) * 1_000_000 / i128::from(self.rate)
    }

    /// The window's earliest residual as of the latest observation (cached;
    /// `None` after a reset).
    fn anchor(&self) -> Option<i64> {
        self.latest.map(|(_, min)| min)
    }

    fn earliest(&self) -> Option<i64> {
        self.residuals.iter().take(self.len).copied().min()
    }

    /// Frame `queued_from` is mixed at `mixed_at` or earlier. Residuals
    /// beyond the `i64` range saturate.
    pub fn observe(&mut self, queued_from: u64, mixed_at: TimeUs) {
        let wide = i128::from(mixed_at.0) - self.frame_us(queued_from);
        let r = wide.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        if let Some(min) = self.anchor() {
            if r < min.saturating_sub(self.jump_us) {
                self.reset();
            } else if r > min.saturating_add(self.jump_us) {
                self.above += 1;
                if self.above >= MIX_CLOCK_REBASE_AFTER {
                    self.reset();
                }
            } else {
                self.above = 0;
            }
        }
        #[expect(
            clippy::indexing_slicing,
            reason = "`next` is kept below the window length by the modulo below"
        )]
        {
            self.residuals[self.next] = r;
        }
        self.next = (self.next + 1) % self.residuals.len();
        self.len = (self.len + 1).min(self.residuals.len());
        let min = self.earliest().unwrap_or(r);
        if self.drift_anchor.is_none() && self.len == self.residuals.len() {
            self.drift_anchor = Some((mixed_at.0, min));
        }
        self.latest = Some((mixed_at.0, min));
    }

    /// When frame `frame` is mixed, by the least-late observation in the
    /// window; `None` before any observation or past the `i64` range.
    #[must_use]
    pub fn time_at(&self, frame: u64) -> Option<TimeUs> {
        let t = i128::from(self.anchor()?) + self.frame_us(frame);
        i64::try_from(t).ok().map(TimeUs)
    }

    /// The frame mixed at `t` (floor), never before frame 0 and saturating
    /// at `u64::MAX`; `None` before any observation.
    #[must_use]
    pub fn frame_at(&self, t: TimeUs) -> Option<u64> {
        let us = (i128::from(t.0) - i128::from(self.anchor()?)).max(0);
        Some(u64::try_from(us * i128::from(self.rate) / 1_000_000).unwrap_or(u64::MAX))
    }

    /// The device clock's speed against the judgment timeline, in ppm
    /// (positive when the device runs fast), from how the window's earliest
    /// residual moved since the window first filled. `None` until then.
    #[must_use]
    pub fn drift_ppm(&self) -> Option<f64> {
        let (t0, r0) = self.drift_anchor?;
        let (t1, r1) = self.latest?;
        if t1 <= t0 {
            return None;
        }
        Some(-(r1 as f64 - r0 as f64) / (t1 as f64 - t0 as f64) * 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_conversion_floors_and_never_goes_below_frame_zero() {
        // Frame 48 000 mixed at 1 s on a 48 kHz grid (by hand: one frame is
        // 20.833 µs).
        let mut c = MixClock::new(48_000, 480);
        assert_eq!(c.time_at(0), None);
        assert_eq!(c.frame_at(TimeUs(0)), None);
        c.observe(48_000, TimeUs(1_000_000));
        assert_eq!(c.time_at(48_000), Some(TimeUs(1_000_000)));
        assert_eq!(c.time_at(48_001), Some(TimeUs(1_000_020)));
        assert_eq!(c.frame_at(TimeUs(1_000_021)), Some(48_001));
        assert_eq!(c.frame_at(TimeUs(999_979)), Some(47_998));
        assert_eq!(c.frame_at(TimeUs(-5_000_000)), Some(0));
    }

    #[test]
    fn extreme_times_and_frames_saturate_or_give_none() {
        let mut c = MixClock::new(48_000, 480);
        c.observe(0, TimeUs(-1_000));
        assert_eq!(c.frame_at(TimeUs(i64::MIN)), Some(0));
        // (i64::MAX + 1000) µs at 48 kHz is about 4.4e17 frames, in range.
        assert_eq!(
            c.frame_at(TimeUs(i64::MAX)),
            Some(((i128::from(i64::MAX) + 1_000) * 48_000 / 1_000_000) as u64)
        );
        // u64::MAX frames are about 3.8e20 µs, past i64.
        assert_eq!(c.time_at(u64::MAX), None);
        // A frame count past i64 µs saturates the residual instead of
        // wrapping, which reads as an early jump and rebases.
        c.observe(u64::MAX, TimeUs(0));
        assert_eq!(c.rebases(), 1);
        assert_eq!(
            c.frame_at(TimeUs(0)),
            Some(((i128::from(i64::MAX) + 1) * 48_000 / 1_000_000) as u64)
        );
        // At a rate of u32::MAX the frame count itself passes u64.
        let mut c = MixClock::new(u32::MAX, 1);
        c.observe(0, TimeUs(i64::MIN));
        assert_eq!(c.frame_at(TimeUs(i64::MAX)), Some(u64::MAX));
    }

    #[test]
    fn the_least_late_wake_of_the_window_anchors_the_mapping() {
        let mut c = MixClock::new(48_000, 480);
        // Passes every 10 ms; wakes late by 3000, 40, 2000 µs.
        for (k, late) in [3_000, 40, 2_000].into_iter().enumerate() {
            let frame = k as u64 * 480;
            c.observe(frame, TimeUs(k as i64 * 10_000 + late));
        }
        assert_eq!(c.time_at(4_800), Some(TimeUs(100_040)));
    }

    #[test]
    fn an_early_jump_rebases_at_once() {
        let mut c = MixClock::new(48_000, 480);
        c.observe(0, TimeUs(10_000));
        // 5001 µs earlier than the window allows (half a period is 5000).
        c.observe(480, TimeUs(20_000 - 5_001));
        assert_eq!(c.rebases(), 1);
        assert_eq!(c.time_at(480), Some(TimeUs(14_999)));
        // Exactly half a period early stays in the window.
        let mut c = MixClock::new(48_000, 480);
        c.observe(0, TimeUs(10_000));
        c.observe(480, TimeUs(15_000));
        assert_eq!(c.rebases(), 0);
    }

    #[test]
    fn a_late_jump_rebases_after_consecutive_observations_only() {
        let late = |c: &mut MixClock, k: u64| c.observe(k * 480, TimeUs(k as i64 * 10_000 + 5_001));
        let mut c = MixClock::new(48_000, 480);
        c.observe(0, TimeUs(0));
        for k in 1..MIX_CLOCK_REBASE_AFTER as u64 {
            late(&mut c, k);
        }
        assert_eq!(c.rebases(), 0);
        // One on-time observation clears the run.
        c.observe(8 * 480, TimeUs(80_000));
        late(&mut c, 9);
        assert_eq!(c.rebases(), 0);
        for k in 10..8 + MIX_CLOCK_REBASE_AFTER as u64 {
            late(&mut c, k);
        }
        assert_eq!(c.rebases(), 0);
        late(&mut c, 8 + MIX_CLOCK_REBASE_AFTER as u64);
        assert_eq!(c.rebases(), 1);
        assert_eq!(c.time_at(0), Some(TimeUs(5_001)));
    }

    #[test]
    fn drift_estimate_needs_a_full_window_and_follows_the_residual() {
        // A device 100 ppm fast: 480 frames every 9999 µs (by hand:
        // 10 000 / 1.0001 = 9999.0001).
        let mut c = MixClock::new(48_000, 480);
        for k in 0..MIX_CLOCK_WINDOW as u64 - 1 {
            c.observe(k * 480, TimeUs(k as i64 * 9_999));
        }
        assert_eq!(c.drift_ppm(), None);
        for k in MIX_CLOCK_WINDOW as u64 - 1..10_000 {
            c.observe(k * 480, TimeUs(k as i64 * 9_999));
        }
        let ppm = c.drift_ppm().unwrap();
        assert!((ppm - 100.0).abs() < 0.5, "{ppm}");
    }
}
