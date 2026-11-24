//! Swappable time source: the real clock in the player, a virtual clock in
//! tests and headless runs.

use std::sync::atomic::{AtomicI64, Ordering};

use crate::TimeUs;

/// A source of the current time on the judgment timeline.
pub trait TimeSource: Send + Sync {
    /// Returns the current time.
    fn now(&self) -> TimeUs;
}

/// A clock that moves only when told to, through [`set`](Self::set) or
/// [`advance`](Self::advance).
#[derive(Debug, Default)]
pub struct VirtualClock {
    now: AtomicI64,
}

impl VirtualClock {
    /// Creates a clock reading `t`.
    #[must_use]
    pub const fn new(t: TimeUs) -> Self {
        Self {
            now: AtomicI64::new(t.0),
        }
    }

    /// Sets the clock to `t`, forwards or backwards.
    pub fn set(&self, t: TimeUs) {
        self.now.store(t.0, Ordering::Release);
    }

    /// Moves the clock by `d`, which may be negative.
    ///
    /// # Panics
    ///
    /// Panics if the new time overflows `i64`. A wrapped clock would jump
    /// across the whole timeline, so overflow is treated as a caller bug.
    pub fn advance(&self, d: TimeUs) {
        let moved = self
            .now
            .try_update(Ordering::AcqRel, Ordering::Acquire, |t| t.checked_add(d.0));
        assert!(moved.is_ok(), "VirtualClock::advance overflowed i64");
    }
}

impl TimeSource for VirtualClock {
    fn now(&self) -> TimeUs {
        TimeUs(self.now.load(Ordering::Acquire))
    }
}

/// An instant on a [`TimeSource`]'s timeline by which something is due.
/// The caller reads the time and passes it in, so a deadline never reads
/// a clock itself and a test drives it with a [`VirtualClock`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Deadline {
    at: TimeUs,
}

impl Deadline {
    /// The deadline at `at`.
    #[must_use]
    pub const fn at_time(at: TimeUs) -> Self {
        Self { at }
    }

    /// The deadline `d` after `now`; the timeline's end when that
    /// overflows.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn after(now: TimeUs, d: TimeUs) -> Self {
        Self {
            at: TimeUs(now.0.saturating_add(d.0)),
        }
    }

    /// When it is due.
    #[must_use]
    pub const fn at(self) -> TimeUs {
        self.at
    }

    /// Whether it has passed at `now`: from its own instant on.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn passed(self, now: TimeUs) -> bool {
        now.0 >= self.at.0
    }

    /// The time left at `now`; none once it has passed.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn left(self, now: TimeUs) -> TimeUs {
        let left = self.at.0.saturating_sub(now.0);
        TimeUs(if left > 0 { left } else { 0 })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{Deadline, TimeSource, VirtualClock};
    use crate::TimeUs;

    /// A deadline passes at its own microsecond, not one before.
    #[test]
    fn a_deadline_passes_at_its_instant() {
        let clock = VirtualClock::new(TimeUs(0));
        let d = Deadline::after(clock.now(), TimeUs(1_041));
        assert_eq!(d.at(), TimeUs(1_041));
        clock.set(TimeUs(1_040));
        assert!(!d.passed(clock.now()));
        clock.set(TimeUs(1_041));
        assert!(d.passed(clock.now()));
        clock.set(TimeUs(1_042));
        assert!(d.passed(clock.now()));
    }

    /// The time left counts down to the instant and stays none past it.
    #[test]
    fn left_saturates_past_the_deadline() {
        let d = Deadline::at_time(TimeUs(4_000));
        assert_eq!(d.left(TimeUs(2_959)), TimeUs(1_041));
        assert_eq!(d.left(TimeUs(4_000)), TimeUs(0));
        assert_eq!(d.left(TimeUs(4_250)), TimeUs(0));
        assert_eq!(
            Deadline::after(TimeUs(i64::MAX - 1), TimeUs(5)).at(),
            TimeUs(i64::MAX)
        );
    }

    #[test]
    fn virtual_clock_moves_only_when_told() {
        let clock = VirtualClock::new(TimeUs(1_000));
        assert_eq!(clock.now(), TimeUs(1_000));
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(clock.now(), TimeUs(1_000));
        clock.advance(TimeUs(250));
        assert_eq!(clock.now(), TimeUs(1_250));
        clock.advance(TimeUs(-50));
        assert_eq!(clock.now(), TimeUs(1_200));
        clock.set(TimeUs(7));
        assert_eq!(clock.now(), TimeUs(7));
    }

    #[test]
    fn virtual_clock_is_shared_as_a_time_source() {
        let clock = Arc::new(VirtualClock::new(TimeUs(0)));
        let source: Arc<dyn TimeSource> = clock.clone();
        clock.advance(TimeUs(5));
        assert_eq!(source.now(), TimeUs(5));
    }

    #[test]
    #[should_panic(expected = "overflowed")]
    fn virtual_clock_advance_panics_on_overflow() {
        let clock = VirtualClock::new(TimeUs(i64::MAX));
        clock.advance(TimeUs(1));
    }
}
