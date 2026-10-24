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

/// How the render thread was released for a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// The frame-latency waitable object blocked and released the thread at
    /// a vblank. Measured against the trusted grid, such wakes may land
    /// slightly before their vblank (`EARLY_WAKE_TOLERANCE_US`).
    Vblank,
    /// The cap timer released the thread for `slot` (a vblank on the cap
    /// grid). The frame belongs to that vblank or a later one: a timer wake
    /// may land early, and a late one belongs to the vblank it passed.
    CapSlot(i64),
    /// Not released by a vblank (the waitable object was already signaled,
    /// or there is none): the frame belongs to the latest vblank before it.
    Late,
}

/// Early-wake tolerance for `Wake::Vblank`, in µs. Measured: windowed
/// 59.951 Hz runs (out/p-, s-, mon-DISPLAY2, ~1,000 frames each) had
/// waitable wakes up to 249 µs before the trusted grid's vblank (1–2% of
/// frames), and 240.085 Hz runs none before it; this is twice the measured
/// worst case. It is bounded by `period / 8` (`early_wake_tolerance`) so it
/// can never move a frame past more than an eighth of a refresh. Wakes by
/// the cap timer or after an overrun never use it.
pub const EARLY_WAKE_TOLERANCE_US: i64 = 500;

/// `EARLY_WAKE_TOLERANCE_US` in the unit of `period` (`units_per_s` per
/// second), at most an eighth of a period.
pub fn early_wake_tolerance(period: i64, units_per_s: i64) -> i64 {
    (EARLY_WAKE_TOLERANCE_US * units_per_s / 1_000_000).min(period / 8)
}

/// The vblank (on the grid `anchor + k × period`) that released a frame
/// woken at `now`: its identity on the display grid.
pub fn release_vblank(now: i64, wake: Wake, anchor: i64, period: i64, tolerance: i64) -> i64 {
    match wake {
        Wake::Vblank => floor_grid(now + tolerance, anchor, period),
        // The slot rounded to the display grid (the cap grid may be anchored
        // on an earlier sync of the same grid).
        Wake::CapSlot(slot) => {
            floor_grid(now, anchor, period).max(floor_grid(slot + period / 2, anchor, period))
        }
        Wake::Late => floor_grid(now, anchor, period),
    }
}

/// Display-time prediction from anchored vblank identities. A frame shows
/// `DISPLAY_VBLANKS` refreshes after the vblank that released it
/// (`release_vblank`), so successive targets follow the display's own
/// opportunities: a frame woken late at 1.6 periods belongs to the vblank
/// at 1 and shows at 2, never inventing a refresh. Every target lies after
/// `now` (an expired target cannot be chosen), and a target never moves
/// back: two frames released by one vblank (presents the compositor drops,
/// a grid re-anchored to another phase) share the earlier target. Without
/// an anchor the target is `now + DISPLAY_VBLANKS × period`.
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayPredictor {
    tolerance: i64,
    last: Option<i64>,
}

impl DisplayPredictor {
    /// `tolerance` from `early_wake_tolerance`, in the unit of the times.
    pub fn new(tolerance: i64) -> Self {
        Self {
            tolerance,
            last: None,
        }
    }

    pub fn predict(&mut self, now: i64, wake: Wake, sync_anchor: Option<i64>, period: i64) -> i64 {
        let release = match (sync_anchor, wake) {
            (Some(a), _) => release_vblank(now, wake, a, period, self.tolerance),
            (None, Wake::CapSlot(slot)) => now.max(slot),
            (None, _) => now,
        };
        let target = (release + DISPLAY_VBLANKS * period).max(self.last.unwrap_or(i64::MIN));
        debug_assert!(target > now);
        self.last = Some(target);
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
        let mut p = DisplayPredictor::new(early_wake_tolerance(R, 1_000_000));
        assert_eq!(
            p.predict(10_100, Wake::Late, Some(1_000), R),
            1_000 + (2 + DISPLAY_VBLANKS) * R
        );
        let mut p = DisplayPredictor::new(early_wake_tolerance(R, 1_000_000));
        assert_eq!(
            p.predict(10_100, Wake::Vblank, None, R),
            10_100 + DISPLAY_VBLANKS * R
        );
    }

    #[test]
    fn the_early_wake_tolerance_is_bounded_and_only_for_vblank_wakes() {
        assert_eq!(early_wake_tolerance(16_680, 1_000_000), 500);
        assert_eq!(early_wake_tolerance(R, 1_000_000), 500);
        // A 1 kHz grid: an eighth of a period.
        assert_eq!(early_wake_tolerance(1_000, 1_000_000), 125);
        // QPC units (10 MHz).
        assert_eq!(early_wake_tolerance(41_652, 10_000_000), 5_000);
        let tol = early_wake_tolerance(R, 1_000_000);
        // A waitable wake 250 µs before the vblank at 3R belongs to it.
        assert_eq!(release_vblank(3 * R - 250, Wake::Vblank, 0, R, tol), 3 * R);
        // 600 µs before it is a wake for the vblank at 2R.
        assert_eq!(release_vblank(3 * R - 600, Wake::Vblank, 0, R, tol), 2 * R);
        // A late wake just before a vblank presents before it.
        assert_eq!(release_vblank(3 * R - 30, Wake::Late, 0, R, tol), 2 * R);
        // A cap-timer wake late at 1.8R for the slot at R presents at 2R.
        assert_eq!(release_vblank(R * 18 / 10, Wake::CapSlot(R), 0, R, tol), R);
        // A cap-timer wake 300 µs early still belongs to its slot.
        assert_eq!(
            release_vblank(2 * R - 300, Wake::CapSlot(2 * R), 0, R, tol),
            2 * R
        );
        let mut p = DisplayPredictor::new(tol);
        assert_eq!(p.predict(R * 18 / 10, Wake::CapSlot(R), Some(0), R), 2 * R);
    }

    #[test]
    fn targets_follow_the_display_opportunities() {
        // Starts at 0, 1.6R (released late), 2R, 3R show at R, 2R, 3R, 4R:
        // no invented refresh at 60 Hz.
        let r = 16_680;
        let mut p = DisplayPredictor::new(early_wake_tolerance(r, 1_000_000));
        let starts = [
            (0, Wake::Vblank),
            (r * 16 / 10, Wake::Late),
            (2 * r, Wake::Vblank),
            (3 * r, Wake::Vblank),
        ];
        let targets: Vec<i64> = starts
            .iter()
            .map(|&(t, w)| p.predict(t, w, Some(0), r))
            .collect();
        assert_eq!(targets, [r, 2 * r, 3 * r, 4 * r]);
    }

    #[test]
    fn targets_never_expire_or_move_back() {
        let mut p = DisplayPredictor::new(early_wake_tolerance(R, 1_000_000));
        assert_eq!(p.predict(0, Wake::Vblank, Some(0), R), R);
        // A fractional stall: released late at 2.45R, shown at 3R.
        assert_eq!(p.predict(R * 245 / 100, Wake::Late, Some(0), R), 3 * R);
        assert_eq!(p.predict(3 * R - 100, Wake::Vblank, Some(0), R), 4 * R);
        // A burst within one refresh (presents the compositor drops) holds
        // the target instead of running ahead.
        for k in 1..4 {
            assert_eq!(p.predict(3 * R + k * R / 4, Wake::Late, Some(0), R), 4 * R);
        }
        // A grid re-anchored half a period later: the next target follows
        // it, after the previous one.
        let a = R / 2;
        assert_eq!(p.predict(4 * R, Wake::Vblank, Some(a), R), a + 4 * R);
        // No anchor at first: one period after the start; the anchored
        // target that follows takes the grid phase.
        let mut q = DisplayPredictor::new(early_wake_tolerance(R, 1_000_000));
        assert_eq!(
            q.predict(1_000 + R / 3, Wake::Vblank, None, R),
            1_000 + R / 3 + R
        );
        assert_eq!(
            q.predict(1_000 + R, Wake::Vblank, Some(1_000), R),
            1_000 + 2 * R
        );
    }

    #[derive(Clone, Copy)]
    struct Scenario {
        /// Refresh rate in mHz.
        refresh_mhz: i64,
        frames: i64,
        /// Waitable wakes from `early` µs before to `late` µs after the
        /// vblank.
        early: i64,
        late: i64,
        /// Cap period in refreshes; cap-timer wakes land from 300 µs early
        /// to 1 ms late.
        cap_k: Option<i64>,
        /// Every n-th frame the thread stalls 0.5–1.5 periods after its
        /// present, so the next wake finds the waitable object signaled.
        stall_every: Option<i64>,
        /// Every n-th present is dropped (never displayed).
        drop_every: Option<i64>,
    }

    const JITTER: Scenario = Scenario {
        refresh_mhz: 0,
        frames: 0,
        early: 150,
        late: 150,
        cap_k: None,
        stall_every: None,
        drop_every: None,
    };

    #[derive(Debug, Default)]
    struct Outcome {
        /// Displayed frames whose prediction missed the actual display.
        wrong: usize,
        /// Of those, predicted after the actual display.
        wrong_late: usize,
        stalls: usize,
        /// 60 fps clip frames skipped between displayed frames when chosen
        /// by the prediction, and by the actual display times.
        skips: usize,
        skips_actual: usize,
    }

    /// Fifo with maximum frame latency 1 on a fixed vblank grid (anchor 0):
    /// the waitable object signals at the previous frame's display, a frame
    /// presents 0.2–0.4 periods after it runs and shows on the first vblank
    /// after its present, and each prediction is checked against that.
    fn simulate(s: Scenario) -> Outcome {
        let period = 1_000_000_000 / s.refresh_mhz; // µs
        let mut p = DisplayPredictor::new(early_wake_tolerance(period, 1_000_000));
        let mut rng = spike_core::rng::SplitMix64::new(9);
        let mut jit = |lo: i64, hi: i64| rng.below((hi - lo + 1) as u64) as i64 + lo;
        let (mut ready, mut signal) = (0, 0);
        let mut last_slot = None;
        let mut prev_target = i64::MIN;
        let mut o = Outcome::default();
        let (mut shown, mut shown_actual) = (vec![], vec![]);
        for f in 1..=s.frames {
            let (mut start, mut wake) = if ready < signal {
                (signal + jit(-s.early, s.late), Wake::Vblank)
            } else {
                (ready, Wake::Late)
            };
            if let Some(k) = s.cap_k {
                let (wait, slot) = cap_step(start, 0, k * period, last_slot);
                last_slot = Some(slot);
                if let Some(w) = wait {
                    start = w + jit(-300, 1_000);
                    wake = Wake::CapSlot(slot);
                }
            }
            let predicted = p.predict(start, wake, Some(0), period);
            assert!(predicted > start, "expired target at frame {f}");
            assert!(predicted >= prev_target, "target moved back at frame {f}");
            prev_target = predicted;
            // The thread cannot run before the waitable object signaled.
            let present = start.max(signal) + jit(period / 5, 2 * period / 5);
            let display = floor_grid(present, 0, period) + period;
            if !s.drop_every.is_some_and(|d| f % d == 0) {
                if predicted != display {
                    o.wrong += 1;
                    o.wrong_late += usize::from(predicted > display);
                }
                shown.push(predicted * 60 / 1_000_000);
                shown_actual.push(display * 60 / 1_000_000);
            }
            signal = display;
            ready = present;
            if s.stall_every.is_some_and(|n| f % n == 0) {
                o.stalls += 1;
                ready += jit(period / 2, 3 * period / 2);
            }
        }
        let skips = |v: &[i64]| -> usize {
            v.windows(2)
                .map(|w| (w[1] - w[0] - 1).max(0) as usize)
                .sum()
        };
        o.skips = skips(&shown);
        o.skips_actual = skips(&shown_actual);
        o
    }

    #[test]
    fn jitter_around_the_vblank_does_not_skip_bga_frames() {
        // 20 s at 59.951 Hz with a 60 fps clip: only the ~1 frame the rate
        // difference itself skips.
        let o = simulate(Scenario {
            refresh_mhz: 59_951,
            frames: 1_199,
            ..JITTER
        });
        assert_eq!(o.wrong, 0);
        assert!(o.skips <= 1, "{o:?}");
        // 240 Hz: four presents per clip frame, none skipped.
        let o = simulate(Scenario {
            refresh_mhz: 240_085,
            frames: 4_800,
            ..JITTER
        });
        assert_eq!((o.wrong, o.skips), (0, 0));
        // The measured early wakes (up to 250 µs) and late ones (up to 1 ms)
        // are still on time.
        let o = simulate(Scenario {
            refresh_mhz: 240_085,
            frames: 4_800,
            early: 250,
            late: 1_000,
            ..JITTER
        });
        assert_eq!((o.wrong, o.skips), (0, 0));
    }

    #[test]
    fn capped_wakes_predict_their_slot() {
        for (refresh_mhz, k) in [(59_951, 2), (240_085, 2), (240_085, 4), (240_085, 8)] {
            let o = simulate(Scenario {
                refresh_mhz,
                frames: 2_400,
                cap_k: Some(k),
                ..JITTER
            });
            assert_eq!(o.wrong, 0, "{refresh_mhz} k={k}: {o:?}");
            assert_eq!(o.skips, o.skips_actual, "{refresh_mhz} k={k}: {o:?}");
        }
    }

    #[test]
    fn stalls_and_dropped_presents_keep_targets_on_the_grid() {
        for refresh_mhz in [59_951, 240_085] {
            for cap_k in [None, Some(2)] {
                // A fractional stall every 50 frames: the late frame may
                // present after the vblank following its wake, so only
                // those frames can be mispredicted, and only early.
                let o = simulate(Scenario {
                    refresh_mhz,
                    frames: 2_400,
                    cap_k,
                    stall_every: Some(50),
                    ..JITTER
                });
                assert!(o.stalls > 0 && o.wrong <= o.stalls, "{o:?}");
                assert_eq!(o.wrong_late, 0, "{o:?}");
                assert!(o.skips <= o.skips_actual + o.wrong, "{o:?}");
                // Every 7th present dropped: the displayed ones are on time.
                let o = simulate(Scenario {
                    refresh_mhz,
                    frames: 2_400,
                    cap_k,
                    drop_every: Some(7),
                    ..JITTER
                });
                assert_eq!(o.wrong, 0, "{o:?}");
                assert_eq!(o.skips, o.skips_actual, "{o:?}");
            }
        }
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
