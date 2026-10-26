//! What the window and the system tell the game thread: system controls,
//! focus, and quitting. Lock-free, so the game thread reads it each step.
//!
//! Quitting (the quit key or closing the window) is a latch that is never
//! cleared, so a quit that arrives while the game is between states (during
//! startup, while a play is prepared) still ends the run. Retry and suspend
//! are taken once, each by the play it arrives in.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use bms_model::{TimeSource, TimeUs};
use game::Command;
use input::{FocusLatch, QpcClock};

use crate::tuning::LiveTuning;

const NONE: u8 = 0;
const RETRY: u8 = 1;
const SUSPEND: u8 = 2;

/// The shared state between the window thread, the power notification and
/// the game thread.
#[derive(Debug)]
pub struct Controls {
    clock: QpcClock,
    command: AtomicU8,
    /// When the latest retry was received, on the clock.
    retry_at: AtomicI64,
    quit: AtomicBool,
    focused: AtomicBool,
    latch: OnceLock<FocusLatch>,
    tuning: Arc<LiveTuning>,
    /// Whether the measurement overlay shows (D11).
    overlay: Arc<AtomicBool>,
}

impl Controls {
    /// Controls with the window focused and nothing pending; receipt times
    /// are read from `clock`.
    #[must_use]
    pub fn new(clock: QpcClock) -> Self {
        let defaults = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        Self::with_tuning(
            clock,
            Arc::new(LiveTuning::new(game::Tuning::of(&defaults))),
        )
    }

    /// Controls as [`new`](Self::new), with the settings keys stepping
    /// `tuning`.
    #[must_use]
    pub fn with_tuning(clock: QpcClock, tuning: Arc<LiveTuning>) -> Self {
        Self {
            clock,
            command: AtomicU8::new(NONE),
            retry_at: AtomicI64::new(i64::MIN),
            quit: AtomicBool::new(false),
            focused: AtomicBool::new(true),
            latch: OnceLock::new(),
            tuning,
            overlay: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The clock's time now.
    #[cfg(feature = "dev-tools")]
    pub fn now(&self) -> TimeUs {
        self.clock.now()
    }

    /// The values the settings keys step.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn tuning(&self) -> &Arc<LiveTuning> {
        &self.tuning
    }

    /// Whether the measurement overlay shows, for the render thread.
    pub const fn overlay(&self) -> &Arc<AtomicBool> {
        &self.overlay
    }

    /// Shows the measurement overlay if hidden, hides it if shown.
    pub fn toggle_overlay(&self) {
        self.overlay.fetch_xor(true, Ordering::AcqRel);
    }

    /// Hands over the input side's focus latch, once.
    pub fn set_latch(&self, latch: FocusLatch) {
        let _ = self.latch.set(latch);
    }

    /// A system control or the system's suspend. Quit latches; a suspend is
    /// never replaced by a later retry, so it always aborts the play.
    pub fn command(&self, c: Command) {
        let new = match c {
            Command::Quit => {
                self.quit.store(true, Ordering::Release);
                return;
            }
            Command::Retry => {
                self.retry_at.store(self.clock.now().0, Ordering::Release);
                RETRY
            }
            Command::Suspend => SUSPEND,
        };
        let _ = self
            .command
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (old != SUSPEND).then_some(new)
            });
    }

    /// Takes the pending retry or suspend.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn take(&self) -> Option<Command> {
        match self.command.swap(NONE, Ordering::AcqRel) {
            RETRY => Some(Command::Retry),
            SUSPEND => Some(Command::Suspend),
            _ => None,
        }
    }

    /// When the latest retry was received.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn retry_at(&self) -> Option<TimeUs> {
        let t = self.retry_at.load(Ordering::Acquire);
        (t != i64::MIN).then_some(TimeUs(t))
    }

    /// The window was asked to close, or its event loop ended: the game
    /// quits.
    pub fn close(&self) {
        self.command(Command::Quit);
    }

    /// Whether the player quit or the window is closing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn quitting(&self) -> bool {
        self.quit.load(Ordering::Acquire)
    }

    /// The window gained or lost focus; a loss sets the input side's
    /// latch (D6).
    pub fn focus(&self, focused: bool) {
        self.focused.store(focused, Ordering::Release);
        if !focused && let Some(l) = self.latch.get() {
            l.latch();
        }
    }

    /// Whether the window has focus.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn focused(&self) -> bool {
        self.focused.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use game::Command;
    use input::QpcClock;

    use super::Controls;

    #[test]
    fn suspend_is_never_replaced_and_take_empties() {
        let c = Controls::new(QpcClock::start());
        assert_eq!(c.take(), None);
        c.command(Command::Retry);
        assert_eq!(c.take(), Some(Command::Retry));
        assert_eq!(c.take(), None);
        c.command(Command::Suspend);
        c.command(Command::Retry);
        assert_eq!(c.take(), Some(Command::Suspend));
        assert!(c.retry_at().is_some());
    }

    #[test]
    fn a_close_stays_latched_through_takes() {
        let c = Controls::new(QpcClock::start());
        assert!(!c.quitting());
        c.close();
        c.command(Command::Retry);
        assert_eq!(c.take(), Some(Command::Retry));
        assert_eq!(c.take(), None);
        assert!(c.quitting());
    }
}
