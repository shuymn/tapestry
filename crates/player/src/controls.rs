//! What the window and the system tell the game thread: system controls,
//! focus, and quitting. Lock-free, so the game thread reads it each step.
//!
//! Closing the window is a latch that is never cleared, so a close that
//! arrives while the game is between states (during startup, while a play
//! is prepared) still ends the run. In the game's runs, with song selection
//! or a chart given on the command line, the quit key is instead a command
//! taken once, like retry and suspend, by the play or result it arrives in
//! ([`Controls::quit_leaves`]): in a play's READY state it leaves the chart,
//! while the chart plays it ends the play on its result, and once the play
//! is complete it skips its tail to the result. Elsewhere (the key config)
//! it latches as a close does.

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
    /// The window's focus as the adjustment tap reads it, with its losses
    /// counted.
    presentation: input::PresentationFocus,
    /// F1 and F2 for the adjustment tap in an autoplay session, which no
    /// keyboard reaches ([`set_window_keys`](Self::set_window_keys)).
    window_keys: std::sync::Mutex<Option<input::WindowKeys>>,
    latch: OnceLock<FocusLatch>,
    /// Wakes the input thread on a focus change.
    input: OnceLock<Waker>,
    /// The game thread, unparked by what the window and the system tell
    /// it, so it can wait idly between plays.
    game: OnceLock<Thread>,
    tuning: Arc<LiveTuning>,
    /// Whether the measurement overlay shows (design.md "Latency Figures").
    overlay: Arc<AtomicBool>,
    /// The scene epoch: a play starting or ending, or focus changing, moves
    /// it, so a key held across it repeats no action.
    scene: std::sync::atomic::AtomicU64,
    /// The input thread's wakes, which a scripted READY's commit waits
    /// past (dev-tools).
    #[cfg(feature = "dev-tools")]
    script_fence: game::ScriptFence,
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
            presentation: input::PresentationFocus::new(),
            window_keys: std::sync::Mutex::new(None),
            latch: OnceLock::new(),
            input: OnceLock::new(),
            game: OnceLock::new(),
            tuning,
            overlay: Arc::new(AtomicBool::new(false)),
            scene: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "dev-tools")]
            script_fence: game::ScriptFence::new(),
        }
    }

    /// The input thread's wakes, which a scripted READY's transport
    /// commits past ([`game::ScriptFence`]; dev-tools).
    #[cfg(feature = "dev-tools")]
    pub const fn script_fence(&self) -> &game::ScriptFence {
        &self.script_fence
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
    /// waiting out its idle wait. The input thread calls it for menu
    /// events.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn wake_game(&self) {
        if let Some(g) = self.game.get() {
            g.unpark();
        }
    }

    /// A system control or the system's suspend. Quit latches; a retry and
    /// a suspend are kept apart, so neither replaces the other: a suspend
    /// still aborts the play, taken before a retry, and a retry pending
    /// with it still ends a Closing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn command(&self, c: Command) {
        self.set_command(c);
        self.wake_game();
    }

    /// From here on the quit key is a command taken once by the play or
    /// result it arrives in, instead of ending the run: only closing the
    /// window latches [`quitting`](Self::quitting).
    pub fn quit_leaves(&self) {
        self.leaves.store(true, Ordering::Release);
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn set_command(&self, c: Command) {
        let bit = match c {
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
        self.command.fetch_or(bit, Ordering::AcqRel);
    }

    /// Takes the pending quit (while it leaves the chart), suspend or retry,
    /// in that order, one per call: a quit drops a retry pressed with it and
    /// leaves a suspend for the next take, so a chart the player left is
    /// left even when the system suspends too.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn take(&self) -> Option<Command> {
        if self.leave.swap(false, Ordering::AcqRel) {
            self.command.fetch_and(!RETRY, Ordering::AcqRel);
            return Some(Command::Quit);
        }
        self.take_slot()
    }

    /// [`take`](Self::take) during Closing (design.md "Play Lifecycle",
    /// Closing), where a quit does nothing: the pending quit is taken
    /// without dropping a pending retry, which the next take returns.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn take_closing(&self) -> Option<Command> {
        if self.leave.swap(false, Ordering::AcqRel) {
            return Some(Command::Quit);
        }
        self.take_slot()
    }

    /// The pending suspend, else the pending retry, taken.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn take_slot(&self) -> Option<Command> {
        for (bit, c) in [(SUSPEND, Command::Suspend), (RETRY, Command::Retry)] {
            if self.command.fetch_and(!bit, Ordering::AcqRel) & bit != 0 {
                return Some(c);
            }
        }
        None
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
        self.next_scene();
        self.focused.store(focused, Ordering::Release);
        self.presentation.set(focused);
        if !focused && let Some(l) = self.latch.get() {
            l.latch();
        }
        if let Some(w) = self.input.get() {
            w.wake();
        }
        self.wake_game();
    }

    /// The scene changed (a play started or ended, focus changed): a held
    /// settings key repeats no action until pressed afresh (the held-input
    /// guard).
    pub fn next_scene(&self) {
        self.scene.fetch_add(1, Ordering::AcqRel);
    }

    /// The scene epoch now.
    pub fn scene(&self) -> u64 {
        self.scene.load(Ordering::Acquire)
    }

    /// The window's presentation focus, for the input thread's source set
    /// ([`input::SourceSet::set_presentation_focus`]).
    #[must_use]
    pub const fn presentation_focus(&self) -> &input::PresentationFocus {
        &self.presentation
    }

    /// From now on the window sends F1 and F2 to `keys`: an autoplay
    /// session's speed keys.
    pub fn set_window_keys(&self, keys: input::WindowKeys) {
        *self
            .window_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(keys);
    }

    /// F1 (`key` 0) or F2 (1) had `event` on the window, `synthetic` when
    /// the window system made it up for a focus change: sent to the
    /// adjustment tap in an autoplay session, nothing otherwise. The input
    /// thread is woken for it.
    pub fn window_key(&self, key: u8, event: input::WindowKeyEvent, synthetic: bool) {
        let mut keys = self
            .window_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(keys) = keys.as_mut() else {
            return;
        };
        keys.key(key, event, synthetic);
        if let Some(w) = self.input.get() {
            w.wake();
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

    /// A retry and a suspend are kept apart whichever is published first:
    /// the suspend is taken first, so it still decides a play's drain, and
    /// the retry after it, so it still ends a Closing; each take empties
    /// what it took.
    #[test]
    fn retry_and_suspend_are_kept_apart_in_either_order() {
        for order in [
            [Command::Suspend, Command::Retry],
            [Command::Retry, Command::Suspend],
        ] {
            let c = Controls::new(QpcClock::start());
            assert_eq!(c.take(), None);
            for command in order {
                c.command(command);
            }
            assert_eq!(c.take(), Some(Command::Suspend), "{order:?}");
            assert_eq!(c.take(), Some(Command::Retry), "{order:?}");
            assert_eq!(c.take(), None, "{order:?}");
            assert!(c.retry_at().is_some());
        }
    }

    /// During Closing the quit key is taken without dropping a retry
    /// pressed with it, in either order, which the next take returns;
    /// outside it, the quit drops the retry.
    #[test]
    fn a_quit_in_closing_keeps_the_retry_pressed_with_it() {
        for order in [
            [Command::Quit, Command::Retry],
            [Command::Retry, Command::Quit],
        ] {
            let c = Controls::new(QpcClock::start());
            c.quit_leaves();
            for command in order {
                c.command(command);
            }
            assert_eq!(c.take_closing(), Some(Command::Quit), "{order:?}");
            assert_eq!(c.take_closing(), Some(Command::Retry), "{order:?}");
            assert_eq!(c.take_closing(), None, "{order:?}");
            for command in order {
                c.command(command);
            }
            assert_eq!(c.take(), Some(Command::Quit), "{order:?}");
            assert_eq!(c.take(), None, "{order:?}");
        }
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
