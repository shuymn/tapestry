//! The clock of a looping play (design.md "Rendering and Skins", video
//! frames): each pass of the clip follows the last on the stream's own
//! clock. The offset of a pass is kept in the stream's time base: each
//! pass lasts from the clip's first usable frame (the first the decoder
//! gives with a time, which may come after the stream's start) to its last
//! frame's end (its stamp plus that frame's own duration), so the next
//! pass's first frame follows the last frame's end with no gap, a stamp is
//! rounded to µs once, from the exact tick count, and passes add no
//! rounding.

/// `ticks` of a time base of `num` / `den` seconds, in µs, rounded half
/// away from zero, as FFmpeg's `av_rescale_q` rounds; saturated at the
/// ends of `i64`.
pub fn rescale_us(ticks: i64, num: i32, den: i32) -> i64 {
    let b = i128::from(num) * 1_000_000;
    let c = i128::from(den);
    if c <= 0 {
        return 0;
    }
    let a = i128::from(ticks);
    let half = c / 2;
    let r = if a >= 0 {
        (a * b + half) / c
    } else {
        -((-a * b + half) / c)
    };
    i64::try_from(r).unwrap_or(if r < 0 { i64::MIN } else { i64::MAX })
}

/// A looping play's clock, in the stream's ticks.
#[derive(Clone, Debug)]
pub struct LoopClock {
    /// The stream's start: a frame's ticks are counted from it.
    start: i64,
    /// The clip's first usable frame's ticks, once known: where each pass
    /// begins.
    first: Option<i64>,
    /// The ticks every pass before this one lasted together.
    offset: i64,
    /// A frame's interval at the stream's average frame rate, if known.
    interval: Option<i64>,
    /// The last frame of the pass: its ticks and its own duration, if
    /// known.
    last: Option<(i64, Option<i64>)>,
    /// The ticks between the pass's last two frames.
    delta: Option<i64>,
}

impl LoopClock {
    /// The clock of a stream starting at `start` ticks whose frames, at its
    /// average rate, last `interval` ticks (if known).
    pub const fn new(start: i64, interval: Option<i64>) -> Self {
        Self {
            start,
            first: None,
            offset: 0,
            interval,
            last: None,
            delta: None,
        }
    }

    /// The ticks, since the play began, of a frame at `pts` lasting
    /// `duration` (if known), which it records as the pass's latest;
    /// `None` when that does not fit.
    pub fn ticks(&mut self, pts: i64, duration: Option<i64>) -> Option<i64> {
        if let Some((last, _)) = self.last {
            self.delta = pts.checked_sub(last).filter(|d| *d > 0).or(self.delta);
        }
        self.last = Some((pts, duration.filter(|d| *d > 0)));
        pts.checked_sub(self.start)?.checked_add(self.offset)
    }

    /// A seek: the next frame decoded follows none of the frames before it,
    /// so neither the jump nor an interval measured before the seek is
    /// taken as a frame's interval; frames after it measure their own.
    pub const fn sever(&mut self) {
        self.last = None;
        self.delta = None;
    }

    /// Whether a frame lasting `duration` (0 or less when unknown) has a
    /// length other than the one-tick fallback: its own, the stream's
    /// average frame interval, or the interval measured to it from the
    /// frame before it.
    pub const fn measures(&self, duration: i64) -> bool {
        duration > 0 || self.interval.is_some() || self.delta.is_some()
    }

    /// The ticks a frame lasting `duration` (0 or less when unknown) lasts:
    /// its own duration, else the stream's average frame interval, else the
    /// interval between the last two frames, else one tick.
    pub fn length(&self, duration: i64) -> i64 {
        Some(duration)
            .filter(|d| *d > 0)
            .or(self.interval)
            .or(self.delta)
            .unwrap_or(1)
    }

    /// How long a frame at `ticks` lasting `duration` ([`Self::length`])
    /// lasts in µs, at a time base of `num`/`den`: its end's stamp minus
    /// its own, each rounded from ticks as stamps are, so a constant-rate
    /// clip's durations tile its stamps; at least 1 µs and at most
    /// [`skin_model::media::MAX_FRAME_US`].
    pub fn frame_us(&self, ticks: i64, duration: i64, num: i32, den: i32) -> i64 {
        let end = ticks.saturating_add(self.length(duration));
        rescale_us(end, num, den)
            .saturating_sub(rescale_us(ticks, num, den))
            .clamp(1, skin_model::media::MAX_FRAME_US)
    }

    /// The ticks every pass before this one lasted together.
    pub const fn pass_start(&self) -> i64 {
        self.offset
    }

    /// The clip's first usable frame's ticks, once known.
    pub const fn first(&self) -> Option<i64> {
        self.first
    }

    /// Whether a restart that decoded a frame at `pts` landed on the clip's
    /// first usable frame, compared in the stream's ticks: two frames a
    /// few ticks apart can round to one µs. True while that frame is not
    /// known (the landing becomes it).
    pub fn lands_on_first(&self, pts: i64) -> bool {
        self.first.is_none_or(|first| first == pts)
    }

    /// The clip's first usable frame is at `pts`: each pass begins there.
    pub const fn set_first(&mut self, pts: i64) {
        self.first = Some(pts);
    }

    /// The ticks, since the play began, of this pass's first frame: the
    /// clip's first usable frame (the stream's start while it is not
    /// known); `None` when that does not fit.
    pub fn pass_first(&self) -> Option<i64> {
        self.first
            .unwrap_or(self.start)
            .checked_sub(self.start)?
            .checked_add(self.offset)
    }

    /// Ends a pass: the next pass's first frame follows its last frame's
    /// end. `false` when the pass had no frame (a loop of nothing never
    /// spins) or its end does not fit.
    pub fn wrap(&mut self) -> bool {
        let Some((pts, duration)) = self.last.take() else {
            return false;
        };
        let length = self.length(duration.unwrap_or(0));
        self.delta = None;
        let end = pts
            .checked_add(length)
            .and_then(|e| e.checked_sub(self.first.unwrap_or(self.start)));
        match end.and_then(|e| self.offset.checked_add(e)) {
            Some(offset) if offset > self.offset => {
                self.offset = offset;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1/90 000 s, the MPEG time base.
    const TB: (i32, i32) = (1, 90_000);

    /// Plays one pass of frames at `pts`, each lasting `durations`,
    /// returning each frame's stamp in µs.
    fn pass(clock: &mut LoopClock, pts: &[i64], durations: &[Option<i64>]) -> Vec<i64> {
        pts.iter()
            .zip(durations)
            .map(|(&p, &d)| rescale_us(clock.ticks(p, d).unwrap(), TB.0, TB.1))
            .collect()
    }

    // Covers: T02
    #[test]
    fn loop_clock_rescales_once_per_frame() {
        // Five frames of 1500 ticks, the last lasting 4001: a pass of
        // 10 001 ticks, 111 122.2 µs, starting at tick 900.
        let pts: Vec<i64> = (0..5).map(|i| 900 + i * 1500).collect();
        let durations = [Some(1500), Some(1500), Some(1500), Some(1500), Some(4001)];
        let mut clock = LoopClock::new(900, Some(1500));
        let one = rescale_us(10_001, TB.0, TB.1);
        assert_eq!(one, 111_122);
        let mut differs = Vec::new();
        for n in 0..100_i64 {
            let stamps = pass(&mut clock, &pts, &durations);
            // Each pass's first frame starts at n passes' ticks, rounded
            // once.
            let exact = rescale_us(n * 10_001, TB.0, TB.1);
            assert_eq!(stamps[0], exact, "pass {n}");
            if exact != n * one {
                differs.push(n);
            }
            // Inside a pass the frames keep their own spacing.
            assert_eq!(
                stamps[4] - stamps[0],
                rescale_us(n * 10_001 + 6000, TB.0, TB.1) - exact
            );
            assert!(clock.wrap());
        }
        // The passes where rounding each pass would have drifted are there:
        // the assertion above told them apart.
        assert!(differs.contains(&9), "{differs:?}");
        assert!(differs.len() > 50, "{differs:?}");
    }

    /// A frame's duration in µs (design.md "Rendering and Skins", video
    /// frames): its own, else the average period, else the interval
    /// between the last two frames, else one tick, never 0 µs.
    // Covers: T02
    #[test]
    fn a_frame_without_a_duration_falls_back_in_order() {
        let us = |clock: &LoopClock, duration| clock.frame_us(9_000, duration, TB.0, TB.1);
        // Its own duration first: 3003 ticks, 33 366.7 µs, the end's
        // stamp rounded (133 367) minus its own (100 000).
        let mut clock = LoopClock::new(0, Some(1800));
        clock.ticks(0, None);
        clock.ticks(2700, None);
        assert_eq!(us(&clock, 3003), 33_367);
        // None: the average period, 1800 ticks, 20 ms.
        assert_eq!(us(&clock, 0), 20_000);
        // No average period: the last interval, 2700 ticks, 30 ms.
        let mut clock = LoopClock::new(0, None);
        clock.ticks(0, None);
        clock.ticks(2700, None);
        assert_eq!(us(&clock, -1), 30_000);
        // Nothing: one tick, 11.1 µs.
        let clock = LoopClock::new(0, None);
        assert_eq!(us(&clock, 0), 11);
        // One tick of a finer base rounds to no µs: 1 µs, never 0.
        assert_eq!(clock.frame_us(9_000, 0, 1, 10_000_000), 1);
        // A duration past an hour: an hour.
        assert_eq!(
            clock.frame_us(0, 400_000_000, TB.0, TB.1),
            skin_model::media::MAX_FRAME_US
        );
    }

    // Covers: T02
    #[test]
    fn loop_clock_falls_back_to_the_rate_then_the_last_delta() {
        // No frame duration: the average rate's interval.
        let mut clock = LoopClock::new(0, Some(3000));
        pass(&mut clock, &[0, 1000, 2000], &[None, None, None]);
        assert!(clock.wrap());
        assert_eq!(clock.ticks(0, None), Some(5000));
        // Neither: the last inter-frame delta.
        let mut clock = LoopClock::new(0, None);
        pass(&mut clock, &[0, 1000, 2500], &[None, None, None]);
        assert!(clock.wrap());
        assert_eq!(clock.ticks(0, None), Some(4000));
    }

    /// Nothing measured before a seek is a frame's interval after it
    /// (design.md "Rendering and Skins", video frames): with no duration
    /// and no average rate, frames 40 ms apart to 120 ms, then a seek to
    /// the frame at 1 s; that frame measures nothing and lasts one tick,
    /// neither the 880 ms jump nor the 40 ms measured before the seek, and
    /// the frame after it measures its own interval.
    // Covers: T02
    #[test]
    fn a_seek_is_never_a_frame_s_interval() {
        let us = |clock: &mut LoopClock, pts| {
            let ticks = clock.ticks(pts, None).unwrap();
            (clock.frame_us(ticks, 0, TB.0, TB.1), clock.measures(0))
        };
        let mut clock = LoopClock::new(0, None);
        pass(&mut clock, &[0, 3600, 7200, 10_800], &[None; 4]);
        assert!(clock.measures(0));
        clock.sever();
        assert_eq!(us(&mut clock, 90_000), (11, false));
        assert_eq!(us(&mut clock, 94_500), (50_000, true));
        // A duration of its own or the stream's rate is still a length.
        assert!(clock.measures(3600));
        let mut rated = LoopClock::new(0, Some(3600));
        rated.sever();
        assert!(rated.measures(0));
    }

    // Covers: T02
    #[test]
    fn loop_clock_starts_each_pass_at_the_first_usable_frame() {
        // The stream starts at tick 0, its first frame with a time at 3000:
        // three frames of 1500, a pass of 4500 ticks from that frame.
        let mut clock = LoopClock::new(0, Some(1500));
        clock.set_first(3000);
        let one = pass(&mut clock, &[3000, 4500, 6000], &[Some(1500); 3]);
        assert_eq!(one, [33_333, 50_000, 66_667]);
        for n in 1..4_i64 {
            assert!(clock.wrap());
            assert_eq!(clock.pass_first(), Some(3000 + n * 4500));
            let next = pass(&mut clock, &[3000, 4500, 6000], &[Some(1500); 3]);
            // Each pass's first frame follows the last pass's end, which a
            // pass counted from the stream's start would push 3000 ticks
            // later every pass.
            assert_eq!(next[0], rescale_us(3000 + n * 4500, TB.0, TB.1));
        }
    }

    // Covers: T02
    #[test]
    fn a_restart_lands_on_the_first_frame_by_its_ticks() {
        // At 10 MHz, ticks 100 and 104 are both 10 µs: only the ticks tell
        // the first frame from the one after it.
        assert_eq!(
            rescale_us(100, 1, 10_000_000),
            rescale_us(104, 1, 10_000_000)
        );
        let mut clock = LoopClock::new(0, None);
        assert!(clock.lands_on_first(104));
        clock.set_first(100);
        assert!(clock.lands_on_first(100));
        assert!(!clock.lands_on_first(104));
    }

    // Covers: T02
    #[test]
    fn loop_clock_refuses_a_pass_with_no_frame() {
        let mut clock = LoopClock::new(0, Some(1500));
        assert!(!clock.wrap());
        pass(&mut clock, &[0], &[Some(1500)]);
        assert!(clock.wrap());
        assert!(!clock.wrap());
    }

    #[test]
    fn rescale_rounds_half_away_from_zero() {
        assert_eq!(rescale_us(1, 1, 2_000_000), 1);
        assert_eq!(rescale_us(-1, 1, 2_000_000), -1);
        assert_eq!(rescale_us(3, 1, 90_000), 33);
        assert_eq!(rescale_us(i64::MAX, 1_000_000, 1), i64::MAX);
    }
}
