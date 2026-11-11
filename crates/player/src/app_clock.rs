//! The application clock (design.md "Rendering and Skins", play readouts):
//! the frames presented in the last second, the local date and time, and
//! the uptime on the clock started at launch, sampled once per frame on the
//! render thread into one [`AppReadouts`].

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;
use skin_model::{AppReadouts, LocalTime};
use windows::Win32::Foundation::SYSTEMTIME;

/// The present instants the ring keeps: the most frames one second can
/// count (`skin_model::FPS_MAX`).
pub(crate) const RING: usize = 1_024;
/// The window the frames are counted over, in µs.
const WINDOW_US: i64 = 1_000_000;

const _: () = assert!(RING as i32 == skin_model::FPS_MAX);

/// The latest [`RING`] present instants, oldest overwritten first: made
/// once with the render thread, never allocating after.
pub(crate) struct AppClock {
    /// Present instants (µs on the launch clock), `head` the next written.
    ring: Box<[i64; RING]>,
    head: usize,
    /// How many of `ring` hold an instant, at most [`RING`].
    len: usize,
}

impl AppClock {
    /// No present yet.
    pub(crate) fn new() -> Self {
        Self {
            ring: Box::new([0; RING]),
            head: 0,
            len: 0,
        }
    }

    /// A frame reached the screen at `at`, which is no earlier than the
    /// previous present.
    pub(crate) fn presented(&mut self, at: TimeUs) {
        if let Some(slot) = self.ring.get_mut(self.head) {
            *slot = at.0;
        }
        self.head = (self.head + 1) % RING;
        self.len = (self.len + 1).min(RING);
    }

    /// The presents in the last second, `(now − 1 s, now]`: at most
    /// [`RING`], the newest first until one falls out of the window.
    pub(crate) fn fps(&self, now: TimeUs) -> u16 {
        let from = now.0.saturating_sub(WINDOW_US);
        let mut n: u16 = 0;
        for back in 1..=self.len {
            let i = (self.head + RING - back) % RING;
            let Some(&at) = self.ring.get(i) else { break };
            if at <= from {
                break;
            }
            if at <= now.0 {
                n = n.saturating_add(1);
            }
        }
        n
    }

    /// The frame's readouts at `now` on the launch clock: the presents of
    /// the last second, the local time read now, and `now` itself as the
    /// uptime, the clock reading 0 at launch (`main`).
    pub(crate) fn readouts(&self, now: TimeUs) -> AppReadouts {
        AppReadouts {
            fps: self.fps(now),
            uptime_us: now.0.max(0),
            local: local_now(),
        }
    }
}

/// The local date and time now (`GetLocalTime`), with no heap.
fn local_now() -> LocalTime {
    // SAFETY: `GetLocalTime` takes nothing and returns the time by value.
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    local_time(&t)
}

/// A `SYSTEMTIME` as the skin reads it: month 1 to 12 and the 24-hour
/// hour as the system gives them; the weekday and milliseconds dropped;
/// each part bounded by its field's range (a leap second reads 59).
pub(crate) fn local_time(t: &SYSTEMTIME) -> LocalTime {
    let byte = |v: u16, lo: u8, hi: u8| u8::try_from(v).unwrap_or(hi).clamp(lo, hi);
    LocalTime {
        year: t
            .wYear
            .min(u16::try_from(skin_model::YEAR_MAX).unwrap_or(u16::MAX)),
        month: byte(t.wMonth, 1, 12),
        day: byte(t.wDay, 1, 31),
        hour: byte(t.wHour, 0, 23),
        minute: byte(t.wMinute, 0, 59),
        second: byte(t.wSecond, 0, 59),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock with `n` presents `step` µs apart, the last at `last`.
    fn presents(clock: &mut AppClock, n: i64, step: i64, last: i64) {
        for k in (0..n).rev() {
            clock.presented(TimeUs(last - k * step));
        }
    }

    /// C7: number 20 is the presents of the last second, at most 1 024.
    #[test]
    fn fps_counts_the_presents_of_the_last_second() {
        let t = 10_000_000;
        // 60 presents 16 667 µs apart: the first at t − 983 335, inside.
        let mut c = AppClock::new();
        presents(&mut c, 60, 16_667, t);
        assert_eq!(c.fps(TimeUs(t)), 60);
        // 30 presents 33 333 µs apart.
        let mut c = AppClock::new();
        presents(&mut c, 30, 33_333, t);
        assert_eq!(c.fps(TimeUs(t)), 30);
        // None.
        assert_eq!(AppClock::new().fps(TimeUs(t)), 0);
        // 60 presents, then 1.5 s later 30 more: only the last 30 count.
        let mut c = AppClock::new();
        presents(&mut c, 60, 16_667, t);
        presents(&mut c, 30, 33_333, t + 1_500_000);
        assert_eq!(c.fps(TimeUs(t + 1_500_000)), 30);
        // 2 000 presents within 1 s: the ring's 1 024.
        let mut c = AppClock::new();
        presents(&mut c, 2_000, 499, t);
        assert_eq!(c.fps(TimeUs(t)), 1_024);
        // The window's edges: a present exactly 1 s before `now` is out,
        // one after `now` not yet counted.
        let mut c = AppClock::new();
        c.presented(TimeUs(t - 1_000_000));
        c.presented(TimeUs(t - 999_999));
        c.presented(TimeUs(t));
        assert_eq!(c.fps(TimeUs(t)), 2);
        assert_eq!(c.fps(TimeUs(t - 1)), 2);
        assert_eq!(c.fps(TimeUs(t - 2)), 2);
        assert_eq!(c.fps(TimeUs(t + 999_999)), 1);
    }

    /// C8: numbers 21-26 are the local year, month 1-12, day, 24-hour
    /// hour, minute and second.
    #[test]
    fn local_time_reads_the_system_time() {
        let t = SYSTEMTIME {
            wYear: 2_026,
            wMonth: 11,
            wDayOfWeek: 3,
            wDay: 11,
            wHour: 23,
            wMinute: 59,
            wSecond: 58,
            wMilliseconds: 999,
        };
        assert_eq!(
            local_time(&t),
            LocalTime {
                year: 2_026,
                month: 11,
                day: 11,
                hour: 23,
                minute: 59,
                second: 58,
            }
        );
        // A leap second reads 59; the clock reads the system now.
        assert_eq!(local_time(&SYSTEMTIME { wSecond: 60, ..t }).second, 59);
        let now = local_now();
        assert!(
            (1..=12).contains(&now.month) && now.year >= 2_026,
            "{now:?}"
        );
    }

    /// C9: the readouts' uptime is the launch clock's reading.
    #[test]
    fn the_uptime_is_the_clock_s_reading() {
        let c = AppClock::new();
        assert_eq!(
            c.readouts(TimeUs(2_160_000_000_000)).uptime_us,
            2_160_000_000_000
        );
        assert_eq!(c.readouts(TimeUs(-5)).uptime_us, 0);
    }

    /// C16: recording presents and reading the frame's readouts allocate
    /// nothing, over a full ring and its wrap; with a positive control.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn app_clock_alloc() {
        use crate::alloc_count::this_thread;
        let mut c = AppClock::new();
        let mut seen = 0_u32;
        let before = this_thread();
        for k in 0..3_000_i64 {
            let now = TimeUs(k * 997);
            c.presented(now);
            let r = c.readouts(now);
            seen = seen.max(u32::from(r.fps));
        }
        let after = this_thread();
        assert_eq!(after - before, 0, "allocations");
        // 997 µs apart: 1 004 presents in (now − 1 s, now].
        assert_eq!(seen, 1_004);
        // The positive control: an allocation on this thread is counted.
        let v = std::hint::black_box(vec![0_u8; 16]);
        assert!(this_thread() > after);
        drop(v);
    }
}
