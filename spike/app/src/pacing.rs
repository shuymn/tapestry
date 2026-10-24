//! Pure frame scheduling and BGA accounting, testable without a display.
//!
//! Times are in one integer unit (QPC ticks in the render loop, µs in
//! tests); grids are `anchor + k × period`.

use spike_core::run::BgaDue;

/// Latest grid point at or before `t`.
fn floor_grid(t: i64, anchor: i64, period: i64) -> i64 {
    anchor + (t - anchor).div_euclid(period) * period
}

/// Frame-cap step (design D3 open choice 3). `last_slot` is the grid slot
/// the previous frame used; `anchor` is the latest display sync time (or
/// the first frame's time until one is known), so the cap grid follows the
/// display's vblank grid and realigns whenever a newer sync time arrives.
///
/// Returns `(wait_until, slot)`: wait until `wait_until` if it is in the
/// future, then render as `slot`. An elapsed slot is permission to render
/// now; missed slots are not made up (no bursts), and the following frame
/// targets the slot after the one used.
pub fn cap_step(now: i64, anchor: i64, period: i64, last_slot: Option<i64>) -> (Option<i64>, i64) {
    let candidate = match last_slot {
        // The next grid point after the used slot (half a period of slack
        // absorbs a re-anchored grid that moved slightly).
        Some(s) => floor_grid(s + period / 2, anchor, period) + period,
        None => floor_grid(now, anchor, period),
    };
    if candidate <= now {
        (None, floor_grid(now, anchor, period).max(candidate))
    } else {
        (Some(candidate), candidate)
    }
}

/// Cap-grid anchor from a display sync at refresh count `count` and QPC
/// `sync`, keeping the cap's phase: grid points are the vblanks whose
/// refresh count is `phase` modulo `k` (the cap period in refreshes).
/// Re-anchoring to a later sync therefore never flips which vblanks the cap
/// uses (with `k = 2`, a sync on an odd refresh still keeps even slots).
pub fn cap_anchor(sync: i64, count: u32, phase: u32, k: u32, refresh: i64) -> i64 {
    let off = (i64::from(count) - i64::from(phase)).rem_euclid(i64::from(k.max(1)));
    sync - off * refresh
}

/// Whether two display syncs `(qpc, refresh count)` lie on one fixed
/// refresh grid of `period` (within a tenth of a period). With VRR or
/// composition the refresh count advances per present instead, and such
/// syncs must not anchor the cap or the prediction.
pub fn sync_consistent(prev: (i64, u32), cur: (i64, u32), period: i64) -> bool {
    let counts = i64::from(cur.1.wrapping_sub(prev.1));
    counts > 0 && (cur.0 - prev.0 - counts * period).abs() <= period / 10
}

/// Consecutive mutually consistent syncs needed to acquire (or reacquire)
/// the refresh grid.
pub const GRID_ACQUIRE_SAMPLES: u32 = 8;

/// The trusted display-refresh grid. A sync on the trusted grid advances
/// the anchor; a sync off it never moves the grid by itself, however well it
/// agrees with its neighbour. Only `GRID_ACQUIRE_SAMPLES` consecutive
/// mutually consistent syncs (re)establish a grid, so a short locally
/// consistent stretch inside VRR-like timing cannot shift the cap slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridEvent {
    OnGrid,
    Acquired,
    OffGrid,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GridTracker {
    trusted: Option<(i64, u32)>,
    candidate: Option<(i64, u32)>,
    streak: u32,
}

impl GridTracker {
    /// Feeds one sync `(qpc, refresh count)`: whether it extended the
    /// trusted grid, (re)acquired one, or lies off it.
    pub fn observe_event(&mut self, sync: (i64, u32), period: i64) -> GridEvent {
        if let Some(t) = self.trusted
            && sync_consistent(t, sync, period)
        {
            self.trusted = Some(sync);
            self.candidate = None;
            self.streak = 0;
            return GridEvent::OnGrid;
        }
        self.streak = match self.candidate {
            Some(c) if sync_consistent(c, sync, period) => self.streak + 1,
            _ => 1,
        };
        self.candidate = Some(sync);
        if self.streak >= GRID_ACQUIRE_SAMPLES {
            self.trusted = Some(sync);
            self.candidate = None;
            self.streak = 0;
            return GridEvent::Acquired;
        }
        GridEvent::OffGrid
    }

    /// The latest trusted sync, if a grid is established.
    pub fn anchor(&self) -> Option<(i64, u32)> {
        self.trusted
    }
}

/// Whether a cap needs pacing at all: a cap at (or above) the refresh rate
/// is what Fifo already delivers, so it adds no timer wait.
pub fn cap_needed(cap_hz: u32, refresh_mhz: u32) -> bool {
    u64::from(cap_hz) * 1000 < u64::from(refresh_mhz)
}

/// Vblanks between a frame's start and its display. UNCALIBRATED: an
/// empirical value from local 59.951 Hz runs (Fifo, maximum frame latency
/// 1, waitable-object wait), where the DXGI display estimates fell on the
/// first vblank after the frame started once the grid used the exact
/// refresh rational. P5 calibrates it against PresentMon at 60 and 240 Hz,
/// with and without a cap, using the run's private trace.
pub const DISPLAY_VBLANKS: i64 = 1;

/// Predicted display time of a frame starting at `now`: the
/// `DISPLAY_VBLANKS`-th vblank of the display grid after `now` when a sync
/// anchor is known, else `now + DISPLAY_VBLANKS × period`.
pub fn predict_display(now: i64, sync_anchor: Option<i64>, period: i64) -> i64 {
    match sync_anchor {
        // A frame starts on the vblank that released it (the waitable
        // wait), give or take wake jitter, so a start up to a quarter period
        // before a vblank counts as that vblank: plain `floor` flips to the
        // previous vblank whenever the wake lands a few µs early.
        Some(a) => floor_grid(now + period / 4, a, period) + DISPLAY_VBLANKS * period,
        None => now + DISPLAY_VBLANKS * period,
    }
}

/// Frames the grid must disagree with the running target before the target
/// follows the grid.
pub const PREDICTION_RESYNC_FRAMES: u32 = 3;

/// Display-time prediction that advances monotonically. With Fifo and
/// latency 1, each present shows on the vblank after the previous one, so
/// when presents keep up the target advances by exactly the refreshes
/// between successive frame starts (a stall advances it by the refreshes
/// it took). The grid prediction anchors the first target and corrects the
/// target only after it has disagreed for `PREDICTION_RESYNC_FRAMES` frames
/// in a row (a re-anchored grid, a lasting phase change, presents dropped by
/// composition), so a single wake that lands on the other side of a vblank
/// cannot move it. A target never moves back (it may hold for a frame while
/// resyncing), so the due BGA frame never skips a frame the display will
/// not skip.
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayPredictor {
    last: Option<(i64, i64)>,
    disagree: u32,
}

impl DisplayPredictor {
    pub fn predict(&mut self, now: i64, sync_anchor: Option<i64>, period: i64) -> i64 {
        let grid = predict_display(now, sync_anchor, period);
        let target = match self.last {
            Some((start, target)) => {
                let steps = ((now - start) as f64 / period as f64).round().max(1.0) as i64;
                let expected = target + steps * period;
                if (grid - expected).abs() < period / 2 {
                    // Same vblank: take the grid's exact phase (the running
                    // target may have started off-grid before an anchor
                    // existed).
                    self.disagree = 0;
                    grid.max(target)
                } else {
                    self.disagree += 1;
                    if self.disagree >= PREDICTION_RESYNC_FRAMES {
                        self.disagree = 0;
                        // Follow the grid, holding the target rather than
                        // moving it back when the running target got ahead
                        // (composition can drop presents at startup).
                        grid.max(target)
                    } else {
                        expected
                    }
                }
            }
            None => grid,
        };
        self.last = Some((now, target));
        target
    }
}

/// G7 due records from what was actually presented. Every BGA frame due
/// in play (`0..due_count`, one per `interval_us` from `play_start_us`) gets
/// a record; it is displayed if some presented frame showed it (repeats
/// count once), otherwise missed — including frames never decoded and
/// those still outstanding at play end.
pub fn bga_due_records(
    play_start_us: i64,
    interval_us: i64,
    due_count: u64,
    shown: impl IntoIterator<Item = u64>,
) -> Vec<BgaDue> {
    let mut seen = vec![false; due_count as usize];
    for i in shown {
        if let Some(s) = seen.get_mut(i as usize) {
            *s = true;
        }
    }
    seen.iter()
        .enumerate()
        .map(|(k, &displayed)| BgaDue {
            due_us: play_start_us + k as i64 * interval_us,
            displayed,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: i64 = 8_333; // 120 Hz cap
    const R: i64 = 4_167; // 240 Hz refresh

    #[test]
    fn late_wake_renders_now_without_halving_the_rate() {
        // The display wait wakes 1 ms after each cap slot.
        let mut last = None;
        let mut slots = vec![];
        let mut now = 0;
        for _ in 0..10 {
            let (wait, slot) = cap_step(now, 0, P, last);
            assert_eq!(wait, None, "a late wake renders at once");
            slots.push(slot);
            last = Some(slot);
            now = slot + P + 1_000;
        }
        // Every frame uses the next slot: 120 fps, not 60.
        assert!(slots.windows(2).all(|w| w[1] - w[0] == P), "{slots:?}");
    }

    #[test]
    fn early_wake_waits_for_the_slot_and_missed_slots_are_not_made_up() {
        let (wait, slot) = cap_step(100, 0, P, Some(0));
        assert_eq!((wait, slot), (Some(P), P));
        // Three periods late: render now on the latest slot, no burst.
        let (wait, slot) = cap_step(3 * P + 50, 0, P, Some(0));
        assert_eq!((wait, slot), (None, 3 * P));
        let (wait, _) = cap_step(3 * P + 60, 0, P, Some(3 * P));
        assert_eq!(wait, Some(4 * P));
    }

    #[test]
    fn grid_realigns_when_display_timing_arrives() {
        // No sync yet: anchored at the first frame (t = 1_000).
        let (_, s0) = cap_step(1_000, 1_000, P, None);
        assert_eq!(s0, 1_000);
        // A vblank at 3_000 becomes the anchor: the next slot is on its grid.
        let (wait, s1) = cap_step(2_000, 3_000, P, Some(s0));
        assert_eq!((wait, s1), (Some(3_000 + P), 3_000 + P));
    }

    #[test]
    fn reanchoring_keeps_the_cap_parity() {
        // 120 Hz cap on 240 Hz: k = 2. The grid started on even refreshes
        // (sync at count 100, QPC 0).
        assert_eq!(cap_anchor(0, 100, 0, 2, R), 0);
        // The next sync is on an odd refresh (count 101, one refresh
        // later): the anchor stays on the even grid.
        let later = cap_anchor(R, 101, 0, 2, R);
        assert_eq!(later, 0);
        let (_, slot) = cap_step(R + 10, later, 2 * R, Some(0));
        assert_eq!(slot, 2 * R);
        assert_eq!(cap_anchor(5 * R, 5, 0, 2, R), 4 * R);
    }

    #[test]
    fn only_syncs_on_the_refresh_grid_anchor() {
        assert!(sync_consistent((0, 10), (3 * R + 100, 13), R));
        // VRR: one count per 33 ms present at a 16.7 ms refresh.
        assert!(!sync_consistent((0, 10), (2 * R, 11), R));
        assert!(!sync_consistent((0, 10), (0, 10), R));
    }

    #[test]
    fn off_grid_pair_does_not_move_the_trusted_grid() {
        let mut g = GridTracker::default();
        // Acquire on the grid 0 + k·R.
        for k in 0..GRID_ACQUIRE_SAMPLES {
            g.observe_event((i64::from(k) * R, k), R);
        }
        assert_eq!(g.anchor(), Some((7 * R, 7)));
        // Off-grid stretch: one count per 2R.
        for k in 8..12u32 {
            assert_eq!(
                g.observe_event((i64::from(k) * 2 * R, k), R),
                GridEvent::OffGrid
            );
        }
        // A locally consistent pair, offset by a third of a refresh from
        // the trusted grid: still not trusted.
        let off = R / 3;
        assert_eq!(g.observe_event((30 * R + off, 30), R), GridEvent::OffGrid);
        assert_eq!(g.observe_event((31 * R + off, 31), R), GridEvent::OffGrid);
        assert_eq!(g.anchor(), Some((7 * R, 7)));
        // Back on the trusted grid: accepted at once.
        assert_eq!(g.observe_event((40 * R, 40), R), GridEvent::OnGrid);
        // A sustained new grid is reacquired.
        for k in 50..50 + GRID_ACQUIRE_SAMPLES {
            g.observe_event((i64::from(k) * R + off, k), R);
        }
        assert_eq!(g.anchor(), Some((57 * R + off, 57)));
    }

    #[test]
    fn cap_at_refresh_needs_no_pacing() {
        assert!(!cap_needed(240, 240_000));
        assert!(!cap_needed(60, 59_940));
        assert!(cap_needed(120, 240_000));
    }

    #[test]
    fn prediction_uses_the_vblank_grid_when_known() {
        // Grid points 1_000 + k·R; the first after 10_100 is k = 3.
        assert_eq!(
            predict_display(10_100, Some(1_000), R),
            1_000 + (2 + DISPLAY_VBLANKS) * R
        );
        assert_eq!(
            predict_display(10_100, None, R),
            10_100 + DISPLAY_VBLANKS * R
        );
    }

    /// Frames released at each vblank of a `num/1000` Hz display, with wake
    /// jitter of up to ±150 µs (as in the 59.951 Hz trace), showing a 60 fps
    /// clip: returns (frames whose prediction missed the actual display,
    /// BGA frames skipped).
    fn simulate(num: i64, frames: i64) -> (usize, usize) {
        let period = 1_000_000_000 / num; // µs (num in mHz)
        let anchor = 0;
        let mut p = DisplayPredictor::default();
        let mut rng = spike_core::rng::SplitMix64::new(9);
        let mut last_index: Option<i64> = None;
        let (mut wrong, mut skips) = (0, 0);
        for k in 1..=frames {
            let vblank = anchor + k * period;
            let jitter = rng.below(301) as i64 - 150;
            let start = vblank + jitter;
            // Shown on the next vblank.
            let actual = vblank + DISPLAY_VBLANKS * period;
            let predicted = p.predict(start, Some(anchor), period);
            if predicted != actual {
                wrong += 1;
            }
            let index = predicted * 60 / 1_000_000;
            if let Some(l) = last_index
                && index > l + 1
            {
                skips += (index - l - 1) as usize;
            }
            last_index = Some(index);
        }
        (wrong, skips)
    }

    #[test]
    fn jitter_around_the_vblank_does_not_skip_bga_frames() {
        // 20 s at 59.951 Hz with a 60 fps clip: only the ~1 frame the rate
        // difference itself skips.
        let (wrong, skips) = simulate(59_951, 1_199);
        assert_eq!(wrong, 0);
        assert!(skips <= 1, "{skips} skips");
        // 240 Hz: four presents per clip frame, none skipped.
        let (wrong, skips) = simulate(240_085, 4_800);
        assert_eq!((wrong, skips), (0, 0));
    }

    #[test]
    fn prediction_follows_presents_and_corrects_after_a_stall() {
        // The first frame has no running target: a wake just before a
        // vblank still counts as that vblank.
        let mut q = DisplayPredictor::default();
        assert_eq!(q.predict(1_000 + R - 30, Some(1_000), R), 1_000 + 2 * R);
        let mut p = DisplayPredictor::default();
        assert_eq!(p.predict(1_000, Some(1_000), R), 1_000 + R);
        assert_eq!(p.predict(1_000 + R - 30, Some(1_000), R), 1_000 + 2 * R);
        // A stall of three refreshes: the target moves by three.
        assert_eq!(p.predict(1_000 + 4 * R + 20, Some(1_000), R), 1_000 + 5 * R);
        // A wake 0.4 period early for the vblank at 6R: two refreshes after
        // the previous start, shown at 7R.
        assert_eq!(
            p.predict(1_000 + 6 * R - 4 * R / 10, Some(1_000), R),
            1_000 + 7 * R
        );
        // A single wake on the far side of a vblank (0.4 period late): one
        // refresh, not two.
        assert_eq!(
            p.predict(1_000 + 6 * R + 4 * R / 10, Some(1_000), R),
            1_000 + 8 * R
        );
        // A lasting phase change of the grid is followed after
        // `PREDICTION_RESYNC_FRAMES` frames, still moving forward.
        let a = 1_000 + R / 2;
        let mut t = 0;
        let last = 8 + i64::from(PREDICTION_RESYNC_FRAMES) - 1;
        for k in 8..=last {
            t = p.predict(1_000 + k * R, Some(a), R);
        }
        assert_eq!(t, predict_display(1_000 + last * R, Some(a), R));
    }

    #[test]
    fn a_target_started_before_the_anchor_takes_the_grid_phase() {
        // No anchor yet: start + one period, a third of a period off the
        // later grid. Once the anchor exists the target snaps to it.
        let mut p = DisplayPredictor::default();
        assert_eq!(p.predict(1_000 + R / 3, None, R), 1_000 + R / 3 + R);
        assert_eq!(p.predict(1_000 + R, Some(1_000), R), 1_000 + 2 * R);
    }

    #[test]
    fn a_running_target_that_got_ahead_resyncs_to_the_grid() {
        // A burst of starts a quarter period apart (presents the compositor
        // drops) pushes the running target ahead; steady frames then follow
        // the grid again after `PREDICTION_RESYNC_FRAMES`, never moving back.
        let mut p = DisplayPredictor::default();
        let mut t = 0;
        let mut prev = i64::MIN;
        for k in 0..4 {
            t = p.predict(1_000 + k * R / 4, Some(1_000), R);
            assert!(t >= prev);
            prev = t;
        }
        for k in 2..20 {
            t = p.predict(1_000 + k * R, Some(1_000), R);
            assert!(t >= prev);
            prev = t;
        }
        assert_eq!(t, predict_display(1_000 + 19 * R, Some(1_000), R));
    }

    #[test]
    fn decoder_dropping_one_frame_per_second_is_98_33_percent() {
        // 600 s at 60 Hz; every 60th frame never decoded, others shown
        // (some twice).
        let n = 36_000u64;
        let shown = (0..n).filter(|k| k % 60 != 59).flat_map(|k| [k, k]);
        let r = bga_due_records(5_000, 16_667, n, shown);
        assert_eq!(r.len() as u64, n);
        let d = r.iter().filter(|b| b.displayed).count();
        assert_eq!(d as u64 * 10_000 / n, 9_833);
        // Outstanding at play end: due but never shown.
        let r = bga_due_records(0, 16_667, 10, 0..8);
        assert!(!r[8].displayed && !r[9].displayed);
        assert_eq!(r[9].due_us, 9 * 16_667);
    }
}
