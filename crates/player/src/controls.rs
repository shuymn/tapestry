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
//!
//! The decision screen's input epoch (design.md "Play Lifecycle", the
//! decision screen) is kept in the same word as the pending commands, so
//! a command is published wholly before or wholly after the epoch's open
//! and close: while it is closed the quit key and retry are dropped and a
//! suspend kept; past the screen's time, while practice waits for its
//! read, the quit key is let through to leave.

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
/// A quit key press waiting to be taken, while it leaves the chart.
const LEAVE: u8 = 4;
/// The decision screen's input epoch is open: quit and retry are dropped.
const EPOCH: u8 = 8;
/// Past the decision screen's time, practice waiting for its read: the
/// quit key leaves again.
const ESCAPE: u8 = 16;

/// The decision screen as the waits inside a chart's load serve it
/// (design.md "Play Lifecycle", the decision screen): every wait's stop
/// asks [`Controls::serve_decision`], which classifies at one point
/// ([`game::Decision::classify`]) and runs the play's deadline step there,
/// on whichever thread asks.
pub struct DecisionWatch {
    /// The decision screen, lent by the flow for the load.
    pub decision: game::Decision,
    /// The held loading lifetime revealed: the deadline step's first
    /// part, before the input epoch ends.
    pub reveal: Box<dyn Fn() + Send + Sync>,
    /// Whether the input or the audio thread failed.
    pub failed: Box<dyn Fn() -> bool + Send + Sync>,
}

impl std::fmt::Debug for DecisionWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionWatch")
            .field("decision", &self.decision)
            .finish_non_exhaustive()
    }
}

/// The shared state between the window thread, the power notification and
/// the game thread.
#[derive(Debug)]
pub struct Controls {
    clock: QpcClock,
    /// The pending commands and the decision screen's epoch, one word.
    command: AtomicU8,
    /// When the latest retry was received, on the clock.
    retry_at: AtomicI64,
    quit: AtomicBool,
    /// The quit key leaves the chart rather than latching [`quit`](Self::quit).
    leaves: AtomicBool,
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
    /// The decision screen a chart's load serves, while it loads.
    watch: std::sync::Mutex<Option<DecisionWatch>>,
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
            focused: AtomicBool::new(true),
            presentation: input::PresentationFocus::new(),
            window_keys: std::sync::Mutex::new(None),
            latch: OnceLock::new(),
            input: OnceLock::new(),
            game: OnceLock::new(),
            tuning,
            overlay: Arc::new(AtomicBool::new(false)),
            scene: std::sync::atomic::AtomicU64::new(0),
            watch: std::sync::Mutex::new(None),
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

    /// Settings key `a` had `event` in the window, now: tuning takes it by
    /// its hold class ([`LiveTuning::key`]), and the game thread is woken
    /// to show a change and look at the loading state again.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn settings_key(&self, a: game::Adjust, event: crate::tuning::KeyEvent) {
        self.tuning.key(a, event, self.clock.now());
        self.wake_game();
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

    /// The game thread, once named: what a job wakes when its outcome is
    /// in, as [`wake_game`](Self::wake_game) does.
    pub fn game_thread(&self) -> Option<Thread> {
        self.game.get().cloned()
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
            Command::Quit if self.leaves.load(Ordering::Acquire) => LEAVE,
            Command::Quit => {
                self.quit.store(true, Ordering::Release);
                return;
            }
            Command::Retry => RETRY,
            Command::Suspend => SUSPEND,
        };
        // In the decision screen's epoch only a suspend is kept, and the
        // quit key once its escape is let through.
        let set = self
            .command
            .try_update(Ordering::AcqRel, Ordering::Acquire, |w| {
                let dropped = w & EPOCH != 0
                    && match bit {
                        LEAVE => w & ESCAPE == 0,
                        RETRY => true,
                        _ => false,
                    };
                (!dropped).then_some(w | bit)
            })
            .is_ok();
        if set && bit == RETRY {
            self.retry_at.store(self.clock.now().0, Ordering::Release);
        }
    }

    /// Takes the pending quit (while it leaves the chart), suspend or retry,
    /// in that order, one per call: a quit drops a retry pressed with it and
    /// leaves a suspend for the next take, so a chart the player left is
    /// left even when the system suspends too.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn take(&self) -> Option<Command> {
        self.take_with(RETRY)
    }

    /// [`take`](Self::take) during Closing (design.md "Play Lifecycle",
    /// Closing), where a quit does nothing: the pending quit is taken
    /// without dropping a pending retry, which the next take returns.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn take_closing(&self) -> Option<Command> {
        self.take_with(NONE)
    }

    /// The pending quit (taking `drops` with it), else the pending
    /// suspend, else the pending retry, taken in one step.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn take_with(&self, drops: u8) -> Option<Command> {
        let order = [
            (LEAVE, LEAVE | drops, Command::Quit),
            (SUSPEND, SUSPEND, Command::Suspend),
            (RETRY, RETRY, Command::Retry),
        ];
        let mut taken = None;
        let _ = self
            .command
            .try_update(Ordering::AcqRel, Ordering::Acquire, |w| {
                let (_, clear, c) = order.iter().find(|(bit, _, _)| w & bit != 0)?;
                taken = Some(*c);
                Some(w & !clear)
            });
        taken
    }

    /// Whether a quit key press waits to be taken (while it leaves the
    /// chart).
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn leaving(&self) -> bool {
        self.command.load(Ordering::Acquire) & LEAVE != 0
    }

    /// The decision screen's choice: its input epoch begins, the ingress
    /// word closed in the same tuning step (design.md "Play Lifecycle",
    /// the decision screen). A quit or retry pending is dropped; a suspend
    /// is kept.
    pub fn close_epoch(&self) {
        self.tuning.close_ingress(|| {
            let _ = self
                .command
                .try_update(Ordering::AcqRel, Ordering::Acquire, |w| {
                    Some((w | EPOCH) & !(LEAVE | RETRY | ESCAPE))
                });
        });
        self.wake_game();
    }

    /// The decision screen is left (its deadline, the practice page, a
    /// return to selection): the epoch ends, the ingress word opened in
    /// the same tuning step. A quit or retry pending is dropped; a suspend
    /// is kept and taken now. Nothing outside the epoch.
    pub fn open_epoch(&self) {
        self.tuning.open_ingress(|| {
            let _ = self
                .command
                .try_update(Ordering::AcqRel, Ordering::Acquire, |w| {
                    Some(w & !(EPOCH | LEAVE | RETRY | ESCAPE))
                });
        });
        self.wake_game();
    }

    /// Past the decision screen's time while practice waits for its read
    /// (design.md "Play Lifecycle", the practice page): the quit key and
    /// Back leave from here. Nothing outside the epoch.
    pub fn let_escape(&self) {
        let _ = self
            .command
            .try_update(Ordering::AcqRel, Ordering::Acquire, |w| {
                (w & EPOCH != 0).then_some(w | ESCAPE)
            });
    }

    /// The quit key pressed since the escape was let through, taken.
    pub fn take_leave(&self) -> bool {
        self.command
            .try_update(Ordering::AcqRel, Ordering::Acquire, |w| {
                (w & LEAVE != 0).then_some(w & !LEAVE)
            })
            .is_ok()
    }

    /// Whether a wait inside a chart's load left the decision screen
    /// ([`serve_decision`](Self::serve_decision)).
    pub fn decision_left(&self) -> bool {
        self.watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|w| w.decision.left().is_some())
    }

    /// Whether the decision screen's epoch lets the quit key and Back
    /// through ([`let_escape`](Self::let_escape)).
    pub fn escaping(&self) -> bool {
        self.command.load(Ordering::Acquire) & ESCAPE != 0
    }

    /// The waits of a chart's load serve `watch` from now on, until
    /// [`unwatch_decision`](Self::unwatch_decision).
    pub fn watch_decision(&self, watch: DecisionWatch) {
        *self
            .watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(watch);
    }

    /// The load's waits serve no decision screen any more: the one they
    /// served, as they left it.
    pub fn unwatch_decision(&self) -> Option<game::Decision> {
        self.watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .map(|w| w.decision)
    }

    /// One look of the decision screen from inside a chart's load
    /// ([`game::Origin::Load`]): a close or a thread's failure ends it
    /// (the wait stops, and the flow finds why); the deadline runs the
    /// play's step here, once, and the wait goes on. Whether the wait is
    /// to stop. Nothing without a decision screen watched.
    pub fn serve_decision(&self) -> bool {
        let mut watch = self
            .watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(w) = watch.as_mut() else {
            return false;
        };
        if w.decision.left().is_some() {
            return true;
        }
        let terminal = self.quitting() || (w.failed)();
        let now = self.clock.now();
        match w
            .decision
            .classify(game::Origin::Load, now, terminal, false, None)
        {
            game::Verdict::Continue => false,
            game::Verdict::Step => {
                // The play's deadline step: the lifetime shown, the
                // loading state's tuning epoch, the input epoch's end, and
                // a new scene. Each a lock-held update or a queued message.
                (w.reveal)();
                self.tuning.enter_loading(now);
                self.open_epoch();
                self.next_scene();
                false
            }
            left => {
                w.decision.leave(left);
                true
            }
        }
    }

    /// Whether tuning observations act now: the ingress word is open.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn ingress_open(&self) -> bool {
        self.tuning.activity().ingress().is_open()
    }

    /// The ingress word now, which an observation is stamped with.
    pub fn ingress(&self) -> input::Ingress {
        self.tuning.activity().ingress()
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
    /// (design.md "Input"), and tuning takes the change
    /// ([`LiveTuning::focus`]). Then the input thread is woken, so an idle
    /// one sees the change, and the game thread.
    pub fn focus(&self, focused: bool) {
        self.next_scene();
        self.tuning.focus(focused, self.clock.now());
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

    /// The decision screen's epoch (E5, E10, N5): a quit and a retry
    /// pending at the choice are dropped by it, and each pressed during it
    /// is dropped without moving the retry's time; a suspend is kept
    /// through it and taken after. The epoch's end drops a quit or retry
    /// again; after it the quit key leaves (the positive control).
    #[test]
    fn the_decision_epoch_drops_quit_and_retry_and_keeps_suspend() {
        let c = Controls::new(QpcClock::start());
        c.quit_leaves();
        c.command(Command::Quit);
        c.command(Command::Retry);
        let retried = c.retry_at();
        c.close_epoch();
        assert!(!c.ingress_open());
        assert_eq!(c.take(), None);
        c.command(Command::Quit);
        c.command(Command::Retry);
        c.command(Command::Suspend);
        assert!(!c.leaving());
        assert_eq!(c.retry_at(), retried);
        c.open_epoch();
        assert!(c.ingress_open());
        assert_eq!(c.take(), Some(Command::Suspend));
        assert_eq!(c.take(), None);
        c.command(Command::Quit);
        assert_eq!(c.take(), Some(Command::Quit));
        assert!(!c.quitting());
    }

    /// Past the decision screen's time, while practice waits for its read
    /// (W3): the quit key, dropped before (the positive control), leaves
    /// once the escape is let through, taken once; outside the epoch the
    /// escape is not let through.
    #[test]
    fn the_escape_lets_the_quit_key_through_in_the_epoch_only() {
        let c = Controls::new(QpcClock::start());
        c.quit_leaves();
        c.let_escape();
        assert!(!c.escaping());
        c.close_epoch();
        c.command(Command::Quit);
        assert!(!c.take_leave());
        c.let_escape();
        assert!(c.escaping());
        c.command(Command::Quit);
        assert!(c.take_leave());
        assert!(!c.take_leave());
        c.open_epoch();
        assert!(!c.escaping());
    }

    /// The decision screen watched by a chart's load (E1, E6, E7 at the
    /// player's waits): before the deadline a look goes on and reveals
    /// nothing; at it the play's step runs once, the ingress opened, the
    /// lifetime revealed and the scene moved, and the wait goes on; a
    /// thread's failure, or a close, then stops every look, the screen
    /// left to end the run.
    #[test]
    fn a_load_wait_serves_the_deadline_once_and_stops_at_a_failure() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        for close in [false, true] {
            let c = Controls::new(QpcClock::start());
            let now = bms_model::TimeSource::now(&c.clock);
            let reveals = Arc::new(AtomicUsize::new(0));
            let failed = Arc::new(AtomicBool::new(false));
            let watch = |decision| {
                let (reveals, failed) = (Arc::clone(&reveals), Arc::clone(&failed));
                super::DecisionWatch {
                    decision,
                    reveal: Box::new(move || {
                        reveals.fetch_add(1, Ordering::SeqCst);
                    }),
                    failed: Box::new(move || failed.load(Ordering::SeqCst)),
                }
            };
            c.close_epoch();
            c.watch_decision(watch(game::Decision::begin(
                now,
                Some(game::DECISION_TIME),
                false,
            )));
            assert!(!c.serve_decision());
            assert_eq!(reveals.load(Ordering::SeqCst), 0);
            assert!(!c.ingress_open());
            let served = c.unwatch_decision().unwrap();
            assert!(!served.stepped());
            // The same screen, its deadline due.
            let due = bms_model::TimeUs(now.0 - game::DECISION_TIME.0);
            c.watch_decision(watch(game::Decision::begin(
                due,
                Some(game::DECISION_TIME),
                false,
            )));
            let scene = c.scene();
            assert!(!c.serve_decision());
            assert!(!c.serve_decision());
            assert_eq!(reveals.load(Ordering::SeqCst), 1);
            assert!(c.ingress_open());
            assert_eq!(c.scene(), scene + 1);
            assert!(!c.decision_left());
            if close {
                c.close();
            } else {
                failed.store(true, Ordering::SeqCst);
            }
            assert!(c.serve_decision());
            assert!(c.decision_left());
            let left = c.unwatch_decision().unwrap();
            assert!(left.stepped());
            assert_eq!(left.left(), Some(game::Verdict::End), "close {close}");
            // Nothing watched: no look stops anything.
            assert!(!c.serve_decision());
        }
    }
}
