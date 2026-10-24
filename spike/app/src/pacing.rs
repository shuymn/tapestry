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

/// Whether a cap needs pacing at all: a cap at (or above) the refresh rate
/// is what Fifo already delivers, so it adds no timer wait.
pub fn cap_needed(cap_hz: u32, refresh_mhz: u32) -> bool {
    u64::from(cap_hz) * 1000 < u64::from(refresh_mhz)
}

/// Vblanks between a frame's start and its display. With Fifo, maximum
/// frame latency 1 and the waitable-object wait, a frame starts just after
/// a vblank and is shown at the second vblank after it: the first local
/// run (60 Hz) paired every DXGI display estimate one period after a
/// one-vblank prediction.
pub const DISPLAY_VBLANKS: i64 = 2;

/// Predicted display time of a frame starting at `now`: the
/// `DISPLAY_VBLANKS`-th vblank of the display grid after `now` when a sync
/// anchor is known, else `now + DISPLAY_VBLANKS × period`.
pub fn predict_display(now: i64, sync_anchor: Option<i64>, period: i64) -> i64 {
    match sync_anchor {
        Some(a) => floor_grid(now, a, period) + DISPLAY_VBLANKS * period,
        None => now + DISPLAY_VBLANKS * period,
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
    fn cap_at_refresh_needs_no_pacing() {
        assert!(!cap_needed(240, 240_000));
        assert!(!cap_needed(60, 59_940));
        assert!(cap_needed(120, 240_000));
    }

    #[test]
    fn prediction_uses_the_vblank_grid_when_known() {
        // Grid points 1_000 + k·R; the first after 10_100 is k = 3.
        assert_eq!(predict_display(10_100, Some(1_000), R), 1_000 + 4 * R);
        assert_eq!(predict_display(10_100, None, R), 10_100 + 2 * R);
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
