//! What the window and the system tell the game thread: system controls,
//! focus, and quitting. Lock-free, so the game thread reads it each step.
//!
//! Closing the window is a latch that is never cleared, so a close that
//! arrives while the game is between states (during startup, while a play
//! is prepared) still ends the run. The quit key latches it too when a
//! chart was given on the command line; with song selection, it leaves the
//! chart instead ([`Controls::quit_leaves`]): a quit taken once, like retry
//! and suspend, each by the play or result it arrives in.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::Thread;

use bms_model::{TimeSource, TimeUs};
use game::Command;
use input::{FocusLatch, QpcClock, Waker};

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
    /// The quit key leaves the chart rather than latching [`quit`](Self::quit).
    leaves: AtomicBool,
    /// A quit key press waiting to be taken, while it leaves the chart.
    leave: AtomicBool,
    focused: AtomicBool,
    latch: OnceLock<FocusLatch>,
    /// Wakes the input thread on a focus change.
    input: OnceLock<Waker>,
    /// The game thread, unparked by what the window and the system tell
    /// it, so it can wait idly between plays.
    game: OnceLock<Thread>,
    tuning: Arc<LiveTuning>,
    /// Whether the measurement overlay shows (design.md "Latency Figures").
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
            leaves: AtomicBool::new(false),
            leave: AtomicBool::new(false),
            focused: AtomicBool::new(true),
            latch: OnceLock::new(),
            input: OnceLock::new(),
            game: OnceLock::new(),
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

    /// Hands over the input thread's waker, once: every focus change
    /// signals it.
    pub fn set_waker(&self, waker: Waker) {
        let _ = self.input.set(waker);
    }

    /// Names the game thread, once: every command, focus change and
    /// settings key unparks it.
    pub fn set_game(&self, game: Thread) {
        let _ = self.game.set(game);
    }

    /// Unparks the game thread, so it acts on what changed without
    /// waiting out its idle wait.
    pub fn wake_game(&self) {
        if let Some(g) = self.game.get() {
            g.unpark();
        }
    }

    /// A system control or the system's suspend. Quit latches; a suspend is
    /// never replaced by a later retry, so it always aborts the play.
    pub fn command(&self, c: Command) {
        self.set_command(c);
        self.wake_game();
    }

    /// From here on the quit key leaves the chart instead of ending the
    /// run (song selection): it is taken once, and only closing the window
    /// latches [`quitting`](Self::quitting).
    pub fn quit_leaves(&self) {
        self.leaves.store(true, Ordering::Release);
    }

    fn set_command(&self, c: Command) {
        let new = match c {
            Command::Quit if self.leaves.load(Ordering::Acquire) => {
                self.leave.store(true, Ordering::Release);
                return;
            }
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

    /// Takes the pending quit (while it leaves the chart), suspend or retry,
    /// in that order: a quit drops a retry pressed with it and leaves a
    /// suspend for the next take, so a chart the player left is left even
    /// when the system suspends too.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn take(&self) -> Option<Command> {
        if self.leave.swap(false, Ordering::AcqRel) {
            let _ = self
                .command
                .compare_exchange(RETRY, NONE, Ordering::AcqRel, Ordering::Acquire);
            return Some(Command::Quit);
        }
        match self.command.swap(NONE, Ordering::AcqRel) {
            RETRY => Some(Command::Retry),
            SUSPEND => Some(Command::Suspend),
            _ => None,
        }
    }

    /// Whether a quit key press waits to be taken (while it leaves the
    /// chart).
    pub fn leaving(&self) -> bool {
        self.leave.load(Ordering::Acquire)
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
        self.quit.store(true, Ordering::Release);
        self.wake_game();
    }

    /// Whether the player quit or the window is closing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn quitting(&self) -> bool {
        self.quit.load(Ordering::Acquire)
    }

    /// The window gained or lost focus; a loss sets the input side's latch
    /// (design.md "Input"). Then the input thread is woken, so an idle one
    /// sees the change, and the game thread.
    pub fn focus(&self, focused: bool) {
        self.focused.store(focused, Ordering::Release);
        if !focused && let Some(l) = self.latch.get() {
            l.latch();
        }
        if let Some(w) = self.input.get() {
            w.wake();
        }
        self.wake_game();
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

    /// With song selection the quit key leaves the chart: taken once, ahead
    /// of a retry or a suspend pressed with it, and it never
    /// latches quitting; closing the window still does.
    #[test]
    fn the_quit_key_leaves_the_chart_with_selection() {
        let c = Controls::new(QpcClock::start());
        c.quit_leaves();
        c.command(Command::Retry);
        c.command(Command::Quit);
        assert!(!c.quitting());
        assert_eq!(c.take(), Some(Command::Quit));
        assert_eq!(c.take(), None);
        c.command(Command::Suspend);
        c.command(Command::Quit);
        assert!(c.leaving());
        assert_eq!(c.take(), Some(Command::Quit));
        assert!(!c.leaving());
        assert_eq!(c.take(), Some(Command::Suspend));
        assert_eq!(c.take(), None);
        assert!(!c.quitting());
        c.close();
        assert!(c.quitting());
        assert_eq!(c.take(), None);
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
