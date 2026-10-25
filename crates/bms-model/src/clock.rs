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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{TimeSource, VirtualClock};
    use crate::TimeUs;

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
