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
//! suspend kept. A quit key press in the epoch is kept apart with the time
//! it was observed, which the decision screen past its time reads
//! ([`Controls::take_quit_since`]).

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

/// The decision screen as the waits inside a chart's load serve it
/// (design.md "Play Lifecycle", the decision screen): every wait's stop
/// asks [`Controls::look`], which classifies at one point
/// ([`game::Decision::classify`]) and runs the play's deadline step there,
/// on whichever thread asks.
pub struct DecisionWatch {
    /// The decision screen, moved here by the flow for the wait and back
    /// at its end ([`Controls::unwatch_decision`]).
    pub decision: game::Decision,
    /// Where the screen is while the wait runs: a play's load
    /// ([`game::Origin::Load`]), or practice before its chart is read
    /// ([`game::Origin::Read`], so its deadline sets the screen waiting
    /// for the read and never steps inside a rig's wait).
    pub origin: game::Origin,
    /// The reveal word read at the deadline step: its first part, before
    /// the input epoch ends
    /// ([`crate::render_side::RenderLink::reveal_at_step`]).
    pub reveal: Box<dyn Fn() -> crate::render_side::AtStep + Send + Sync>,
    /// The rig's thread failures, as the rig itself reads them.
    pub failures: crate::failures::Failures,
    /// The screen, built again from the wait's looks when it was lost
    /// ([`crate::render_side::RecoveryHandle::look`]); none: not served.
    pub screen: Option<crate::render_side::RecoveryHandle>,
    /// A decided play's early preparation (design.md "Screen Transitions
    /// and Loading"): its gate, whose refusal each look classifies with
    /// and whose reveal the step commits first, and its serving step
    /// (sending the early request); none without one.
    pub early: Option<EarlyWatch>,
}

/// The early preparation as the load's looks serve it
/// ([`DecisionWatch::early`]).
pub struct EarlyWatch {
    /// The gate.
    pub gate: std::sync::Arc<crate::play::early::EarlyGate>,
    /// The serving step, non-blocking ([`crate::play::early::EarlyGate::step`]).
    pub step: Box<dyn Fn(&crate::play::early::EarlyGate) + Send + Sync>,
}

impl std::fmt::Debug for DecisionWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionWatch")
            .field("decision", &self.decision)
            .finish_non_exhaustive()
    }
}

/// The chart images' work a look serves ([`Controls::serve_images`]).
pub struct ImageService(pub Box<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for ImageService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ImageService")
    }
}

/// What a close withdraws the decision's armed reveal with
/// ([`Controls::set_reveal_withdraw`]).
struct RevealWithdraw(std::sync::Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for RevealWithdraw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RevealWithdraw")
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
    /// When the pending quit that leaves the chart, the pending suspend and
    /// the pending retry were first received, on the clock: the receipts a
    /// fade phase's skip or cut moves its deadline to ([`take_received`]).
    /// Each is written only while its command's bit is clear, before the
    /// bit is set, and read before a take clears the bit.
    ///
    /// [`take_received`]: Self::take_received
    leave_at: AtomicI64,
    suspend_at: AtomicI64,
    retry_pending_at: AtomicI64,
    /// Held by a sender from its receipt to its bit: commands are
    /// published one at a time, whichever thread sends them (the window's
    /// keys, the power notification, the game thread's own retry), so no
    /// sender's receipt is stored under another's publication.
    senders: std::sync::Mutex<()>,
    /// The next take publishes a quit once it has cleared its bit
    /// (tests).
    #[cfg(test)]
    quit_during_take: AtomicBool,
    /// Runs in the next publication between its receipt and its bit
    /// (tests).
    #[cfg(test)]
    in_publication: std::sync::Mutex<Option<InPublication>>,
    /// When the latest quit key press in the decision screen's epoch was
    /// observed, on the clock; `i64::MIN` for none.
    quit_in_epoch: AtomicI64,
    quit: AtomicBool,
    /// What a close withdraws the decision's armed reveal with
    /// ([`set_reveal_withdraw`](Self::set_reveal_withdraw)), under a lock
    /// a close takes after latching [`quit`](Self::quit).
    reveal_withdraw: std::sync::Mutex<Option<RevealWithdraw>>,
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
    /// What every look serves before its verdict: the chart images' work
    /// a step left (a finished job's set, a job begun later, a terminal
    /// absence published), nonblocking ([`serve_images`](Self::serve_images)).
    images: std::sync::Mutex<Option<ImageService>>,
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
            leave_at: AtomicI64::new(i64::MIN),
            suspend_at: AtomicI64::new(i64::MIN),
            retry_pending_at: AtomicI64::new(i64::MIN),
            senders: std::sync::Mutex::new(()),
            #[cfg(test)]
            quit_during_take: AtomicBool::new(false),
            #[cfg(test)]
            in_publication: std::sync::Mutex::new(None),
            quit_in_epoch: AtomicI64::new(i64::MIN),
            quit: AtomicBool::new(false),
            reveal_withdraw: std::sync::Mutex::new(None),
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
            images: std::sync::Mutex::new(None),
        }
    }

    /// The clock's time now: what a producer observes an input at.
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

    /// The overlay key pressed in the window: it toggles the overlay only
    /// while the ingress word is open, checked and toggled in one tuning
    /// step, so a choice cannot fall between the two.
    pub fn overlay_key(&self) {
        self.tuning.with_ingress(|word| {
            if word.is_open() {
                self.toggle_overlay();
            }
        });
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
        // A command's receipt is read and stored while its bit is clear,
        // before the bit is set, and never while it is set: one pressed
        // again before it is taken joins the pending one, whose first
        // receipt stays, and a take reads the receipt before it clears the
        // bit, so a command and its receipt go together. Senders publish one
        // at a time, so a receipt is only ever stored by the sender that
        // sets its bit next.
        let _sending = self
            .senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = self.clock.now().0;
        let set = loop {
            let w = self.command.load(Ordering::Acquire);
            // In the decision screen's epoch only a suspend is kept; a quit
            // key press is kept apart, with the time it was observed.
            if w & EPOCH != 0 && matches!(bit, LEAVE | RETRY) {
                break false;
            }
            if w & bit != 0 {
                break true;
            }
            self.receipt(bit).store(now, Ordering::Release);
            #[cfg(test)]
            {
                let run = self
                    .in_publication
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(InPublication(run)) = run {
                    run();
                }
            }
            if self
                .command
                .compare_exchange(w, w | bit, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break true;
            }
        };
        if set && bit == RETRY {
            self.retry_at.store(now, Ordering::Release);
        }
        if !set && bit == LEAVE {
            self.quit_in_epoch.store(now, Ordering::Release);
        }
    }

    /// Takes the pending quit (while it leaves the chart), suspend or retry,
    /// in that order, one per call: a quit drops a retry pressed with it and
    /// leaves a suspend for the next take, so a chart the player left is
    /// left even when the system suspends too.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn take(&self) -> Option<Command> {
        self.take_with(RETRY, i64::MAX).map(|(c, _)| c)
    }

    /// [`take`](Self::take) of the commands received by `until`, at it
    /// included, with the command's receipt: the clock's reading when the
    /// window thread or the power notification handed it over, never when
    /// it is taken (design.md "State Model", ResultPreparing: a fade
    /// phase's commands end at its takeover, and what comes after it is
    /// the result's). A command received later stays pending, and a quit
    /// drops only a retry received by `until`.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn take_received(&self, until: TimeUs) -> Option<(Command, TimeUs)> {
        self.take_with(RETRY, until.0)
    }

    /// [`take`](Self::take) during Closing (design.md "Play Lifecycle",
    /// Closing), where a quit does nothing: the pending quit is taken
    /// without dropping a pending retry, which the next take returns.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn take_closing(&self) -> Option<Command> {
        self.take_with(NONE, i64::MAX).map(|(c, _)| c)
    }

    /// The pending quit (taking `drops` with it), else the pending
    /// suspend, else the pending retry, of those received by `until`,
    /// taken in one step with its first receipt, read before its bit is
    /// cleared.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn take_with(&self, drops: u8, until: i64) -> Option<(Command, TimeUs)> {
        let order = [
            (LEAVE, Command::Quit),
            (SUSPEND, Command::Suspend),
            (RETRY, Command::Retry),
        ];
        // Only the taker clears a bit, and a sender writes a receipt only
        // while its bit is clear: a set bit's receipt holds still.
        let received = |w: u8, bit: u8| {
            let at = self.receipt(bit).load(Ordering::Acquire);
            (w & bit != 0 && at <= until).then_some(at)
        };
        loop {
            let w = self.command.load(Ordering::Acquire);
            let (bit, c, at) = order
                .iter()
                .find_map(|&(bit, c)| received(w, bit).map(|at| (bit, c, at)))?;
            let clear = if bit == LEAVE && received(w, drops).is_some() {
                LEAVE | drops
            } else {
                bit
            };
            if self
                .command
                .compare_exchange(w, w & !clear, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                #[cfg(test)]
                if self.quit_during_take.swap(false, Ordering::AcqRel) {
                    self.set_command(Command::Quit);
                }
                return Some((c, TimeUs(at)));
            }
        }
    }

    /// The receipt of the pending command `bit`.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    const fn receipt(&self, bit: u8) -> &AtomicI64 {
        match bit {
            LEAVE => &self.leave_at,
            SUSPEND => &self.suspend_at,
            _ => &self.retry_pending_at,
        }
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
                    Some((w | EPOCH) & !(LEAVE | RETRY))
                });
            self.quit_in_epoch.store(i64::MIN, Ordering::Release);
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
                    Some(w & !(EPOCH | LEAVE | RETRY))
                });
            self.quit_in_epoch.store(i64::MIN, Ordering::Release);
        });
        self.wake_game();
    }

    /// Whether the quit key was pressed in the decision screen's epoch at
    /// or after `deadline`, as the window observed its press, taken: the
    /// decision screen past its time reads it (design.md "Play Lifecycle",
    /// the practice page). A press observed before `deadline` is not, when
    /// ever it is taken.
    pub fn take_quit_since(&self, deadline: TimeUs) -> bool {
        let at = self.quit_in_epoch.swap(i64::MIN, Ordering::AcqRel);
        at != i64::MIN && at >= deadline.0
    }

    /// Whether a wait inside a chart's load left the decision screen
    /// ([`look`](Self::look)).
    pub fn decision_left(&self) -> bool {
        self.watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|w| w.decision.left().is_some())
    }

    /// Every look ([`look`](Self::look)) serves the chart images' work with
    /// `serve` from now on, which must not block.
    pub fn serve_images(&self, serve: ImageService) {
        *self
            .images
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(serve);
    }

    /// The waits of a chart's load serve `watch` from now on, until
    /// [`unwatch_decision`](Self::unwatch_decision).
    pub fn watch_decision(&self, watch: DecisionWatch) {
        *self
            .watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(watch);
    }

    /// `f` on the watched decision as it stands, borrowed under the
    /// watch's lock (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn with_watched<T>(&self, f: impl FnOnce(Option<&game::Decision>) -> T) -> T {
        f(self
            .watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|w| &w.decision))
    }

    /// The load's waits serve no decision screen any more: the one they
    /// served, moved back as they left it.
    pub fn unwatch_decision(&self) -> Option<game::Decision> {
        self.watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .map(|w| w.decision)
    }

    /// One look of the decision screen from inside a wait the rig owns:
    /// the verdict [`game::Decision::classify`] returns from the watch's
    /// origin, with the run's terminal causes (a close, the rig's thread
    /// failures), handed to the waiting caller, which acts on it. The
    /// play's step runs here, once, and the wait goes on; a leaving verdict
    /// is kept in the decision ([`game::Decision::leave`]) for App to route
    /// and returned again by every later look. The escape is not read
    /// here: a press counts by when it was observed, at App's next look.
    /// `None` without a decision screen watched.
    pub fn look(&self) -> Option<game::Verdict> {
        // The chart images' work first, whatever the verdict: it neither
        // blocks nor changes it.
        if let Ok(images) = self.images.try_lock()
            && let Some(serve) = images.as_ref()
        {
            (serve.0)();
        }
        let mut watch = self
            .watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let w = watch.as_mut()?;
        if let Some(left) = w.decision.left() {
            return Some(left);
        }
        let terminal = self.quitting() || w.failures.any();
        // A close or a thread's failure keeps the deadline from revealing:
        // the render thread's armed reveal is withdrawn here too.
        if terminal && !w.decision.practice() {
            self.withdraw_reveal();
        }
        let now = self.clock.now();
        // The decision skin's takeover, as App's look takes it: before the
        // classification, so the deadline then in force is judged.
        if let Some(takeover) = w.screen.as_ref().and_then(|s| s.take_decision_takeover()) {
            let applied = w.decision.apply_takeover(takeover, now);
            // The deadline it moved, the decision's end in force (a run
            // record's mark).
            #[cfg(feature = "dev-tools")]
            if applied {
                crate::dev::transitions::mark_decision_end(&w.decision);
            }
            #[cfg(not(feature = "dev-tools"))]
            let _ = applied;
        }
        let lifetime = w.decision.lifetime();
        // The early preparation served, and a refusal it published
        // classified with: latched here, before the deadline; after the
        // reveal, the render thread's refusal of the early staging the
        // attempt would adopt ends the load at once.
        let refused = w.early.as_ref().and_then(|e| {
            (e.step)(&e.gate);
            (e.gate.refused().is_some() || e.gate.late_refused().is_some())
                .then_some(game::Reply::Refusal)
        });
        let mut seen = w.decision.classify(w.origin, now, terminal, false, refused);
        if seen == game::Verdict::Step {
            if w.decision.practice() {
                w.decision.commit_step();
            } else {
                // The reveal word read first, then committed against the
                // gate's one state: a refusal that won the word shows
                // nothing and returns, and a reveal the render thread has
                // not taken yet steps nothing.
                match (w.reveal)() {
                    crate::render_side::AtStep::Pending => seen = game::Verdict::Continue,
                    crate::render_side::AtStep::Refused => {
                        let committed = w.early.as_ref().map(|e| e.gate.commit_reveal());
                        let why = match committed {
                            Some(crate::play::early::Committed::Refused(r)) => r,
                            _ => crate::selection::refused_before_reveal(),
                        };
                        seen = w.decision.refuse_step(why);
                    }
                    crate::render_side::AtStep::Revealed => {
                        let committed = match &w.early {
                            Some(e) => e.gate.commit_reveal(),
                            None => crate::play::early::Committed::Revealed,
                        };
                        if let crate::play::early::Committed::Refused(why) = committed {
                            seen = w.decision.refuse_step(why);
                        } else {
                            // The play's deadline step: the loading
                            // state's tuning epoch, the input epoch's end,
                            // and a new scene. Each a lock-held update or a
                            // queued message.
                            self.tuning.enter_loading(now);
                            self.open_epoch();
                            self.next_scene();
                            w.decision.commit_step();
                        }
                    }
                }
            }
        }
        if seen.leaves() {
            w.decision.leave(seen);
        }
        // The step or a leave ended the screen's lifetime: its pinned
        // label slot is the render thread's to retire.
        if let (Some(l), None, Some(screen)) = (lifetime, w.decision.lifetime(), &w.screen) {
            screen.end_decision(l.0);
        }
        if !seen.leaves()
            && let Some(screen) = &w.screen
        {
            // A screen lost meanwhile is built again from here; the flow
            // takes the rebuild over, or its failure, at its next look.
            screen.look();
        }
        Some(seen)
    }

    /// The ingress word now, which an observation is stamped with.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn ingress(&self) -> input::Ingress {
        self.tuning.activity().ingress()
    }

    /// When the latest retry was received.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn retry_at(&self) -> Option<TimeUs> {
        let t = self.retry_at.load(Ordering::Acquire);
        (t != i64::MIN).then_some(TimeUs(t))
    }

    /// A close withdraws the decision's armed reveal through `withdraw`
    /// ([`crate::render_side::RenderLink::reveal_withdrawer`]; once, a
    /// second is ignored), installed before the first arm. A close that
    /// takes the lock before the installation latched
    /// [`quit`](Self::quit) before it, which the arm after it reads
    /// (design.md "Screen Transitions and Loading", the reveal word).
    pub fn set_reveal_withdraw(&self, withdraw: std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.reveal_withdraw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert(RevealWithdraw(withdraw));
    }

    /// The decision's armed reveal withdrawn, if one is set.
    fn withdraw_reveal(&self) {
        let withdraw = self
            .reveal_withdraw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|RevealWithdraw(w)| std::sync::Arc::clone(w));
        if let Some(w) = withdraw {
            w();
        }
    }

    /// The window was asked to close, or its event loop ended: the game
    /// quits. A reveal armed for the decision screen is withdrawn first, so
    /// the shutdown's waits draw no loading state past its deadline.
    pub fn close(&self) {
        self.quit.store(true, Ordering::SeqCst);
        self.withdraw_reveal();
        // The first close on the host counter, for a windowed run's record.
        #[cfg(feature = "dev-tools")]
        crate::dev::transitions::close_stamp();
        self.wake_game();
    }

    /// Whether the player quit or the window is closing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn quitting(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
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
        // Stamped as the window observes it: a key queued during the
        // decision screen does not act after it.
        keys.key_stamped(key, event, synthetic, self.ingress());
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

/// What a test runs in a publication ([`Controls`]' `in_publication`).
#[cfg(test)]
struct InPublication(Box<dyn FnOnce() + Send>);

#[cfg(test)]
impl std::fmt::Debug for InPublication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InPublication")
    }
}

#[cfg(test)]
mod tests {
    use game::Command;
    use input::QpcClock;

    use super::Controls;
    use bms_model::TimeUs;

    /// A take's limit no receipt reaches.
    const ALL: TimeUs = TimeUs(i64::MAX);

    /// The reveal word's close: one after the withdrawal is installed runs
    /// it, once; one before it (the first decision's) leaves the quit latch
    /// the arm after the installation reads.
    #[test]
    fn a_close_reaches_the_reveal_withdrawal_or_the_arm() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        for close_first in [true, false] {
            let c = Controls::new(QpcClock::start());
            let ran = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&ran);
            let withdraw: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            });
            if close_first {
                c.close();
                c.set_reveal_withdraw(withdraw);
                assert!(c.quitting());
            } else {
                c.set_reveal_withdraw(withdraw);
                assert_eq!(ran.load(Ordering::SeqCst), 0, "run before a close");
                c.close();
                assert_eq!(ran.load(Ordering::SeqCst), 1);
            }
        }
    }

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

    /// A close stamps a windowed run's record with the host counter
    /// (development builds).
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_close_stamps_the_run_record() {
        let c = Controls::new(QpcClock::start());
        assert_eq!(crate::dev::transitions::close_qpc(), None);
        let before = render::present_tap::qpc_now();
        c.close();
        let stamped = crate::dev::transitions::close_qpc().expect("stamped");
        assert!(stamped >= before, "{stamped} < {before}");
    }

    /// A quit pressed again before it is taken joins the pending one and
    /// keeps the first press's receipt, the one a fade phase's skip moves
    /// its deadline to.
    #[test]
    fn a_quit_pressed_again_keeps_its_first_receipt() {
        let c = Controls::new(QpcClock::start());
        c.quit_leaves();
        let before = c.now();
        c.command(Command::Quit);
        let first = c.now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let again = c.now();
        c.command(Command::Quit);
        let (taken, at) = c.take_received(ALL).expect("a pending quit");
        assert_eq!(taken, Command::Quit);
        assert!(
            before.0 <= at.0 && at.0 <= first.0,
            "{at:?} not the first press, in {before:?}..={first:?} (again at {again:?})"
        );
        assert_eq!(c.take_received(ALL), None);
    }

    /// A command received after the limit a take is given stays pending,
    /// for the next take; a quit taken by it drops only a retry received
    /// by it too.
    #[test]
    fn a_command_received_after_the_limit_stays_pending() {
        let c = Controls::new(QpcClock::start());
        c.quit_leaves();
        c.command(Command::Quit);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let limit = c.now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        c.command(Command::Retry);
        let (taken, at) = c.take_received(limit).expect("the quit before the limit");
        assert_eq!(taken, Command::Quit);
        assert!(at.0 < limit.0);
        assert_eq!(c.take_received(limit), None, "the retry is past the limit");
        assert_eq!(c.take(), Some(Command::Retry), "kept for the next take");
        c.command(Command::Quit);
        assert_eq!(c.take_received(TimeUs(i64::MIN)), None);
        assert_eq!(c.take(), Some(Command::Quit));
    }

    /// A sender paused between its receipt and its bit holds every other
    /// sender: a quit sent meanwhile is not published and nothing is
    /// taken; once the first is published the second joins it, so the one
    /// command taken has the first receipt and no receipt comes back
    /// twice.
    #[test]
    fn a_sender_between_its_receipt_and_its_bit_holds_the_others() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        type Seen = (
            TimeUs,
            Option<(Command, TimeUs)>,
            std::thread::JoinHandle<()>,
        );
        let c = Arc::new(Controls::new(QpcClock::start()));
        c.quit_leaves();
        let (other, taker) = (Arc::clone(&c), Arc::clone(&c));
        let seen: Arc<Mutex<Option<Seen>>> = Arc::default();
        let into = Arc::clone(&seen);
        *c.in_publication.lock().unwrap() = Some(super::InPublication(Box::new(move || {
            let paused = taker.now();
            let sender = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(5));
                other.command(Command::Quit);
            });
            std::thread::sleep(Duration::from_millis(50));
            *into.lock().unwrap() = Some((paused, taker.take_received(ALL), sender));
        })));
        c.command(Command::Quit);
        let (paused, during, sender) = seen.lock().unwrap().take().unwrap();
        sender.join().unwrap();
        assert_eq!(
            during, None,
            "a quit published or taken while the first was paused"
        );
        let (taken, at) = c.take_received(ALL).expect("the first quit");
        assert_eq!(taken, Command::Quit);
        assert!(at.0 <= paused.0, "{at:?} is not the first quit's receipt");
        assert_eq!(c.take_received(ALL), None, "the second joined the first");
    }

    /// A take returns the receipt of the command it takes: a quit
    /// published once the take has cleared the bit is the next command,
    /// with its own receipt.
    #[test]
    fn a_quit_published_during_a_take_keeps_its_own_receipt() {
        use std::sync::atomic::Ordering;

        let c = Controls::new(QpcClock::start());
        c.quit_leaves();
        c.command(Command::Quit);
        let first = c.now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let between = c.now();
        c.quit_during_take.store(true, Ordering::Release);
        let (_, at) = c.take_received(ALL).expect("the first quit");
        assert!(at.0 <= first.0, "{at:?} is not the first quit's receipt");
        let (taken, later) = c
            .take_received(ALL)
            .expect("the quit published in the take");
        assert_eq!(taken, Command::Quit);
        assert!(
            later.0 >= between.0,
            "{later:?} is not the second quit's receipt"
        );
        assert_eq!(c.take_received(ALL), None);
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
        assert!(!c.ingress().is_open());
        assert_eq!(c.take(), None);
        c.command(Command::Quit);
        c.command(Command::Retry);
        c.command(Command::Suspend);
        assert!(!c.leaving());
        assert_eq!(c.retry_at(), retried);
        c.open_epoch();
        assert!(c.ingress().is_open());
        assert_eq!(c.take(), Some(Command::Suspend));
        assert_eq!(c.take(), None);
        c.command(Command::Quit);
        assert_eq!(c.take(), Some(Command::Quit));
        assert!(!c.quitting());
    }

    /// Past the decision screen's time, while practice waits for its read
    /// (W3): a quit key press is taken by the time it was observed, against
    /// the deadline, whenever it is taken: one observed before the deadline
    /// does not leave, one observed at or after it does, once; neither is
    /// a pending quit while the epoch lasts, and the epoch's end drops one.
    #[test]
    fn a_quit_in_the_epoch_is_taken_by_its_observation_time() {
        let c = Controls::new(QpcClock::start());
        c.quit_leaves();
        c.close_epoch();
        c.command(Command::Quit);
        let after = c.now();
        let deadline = bms_model::TimeUs(after.0 + 1);
        while c.now().0 < deadline.0 {
            std::thread::yield_now();
        }
        assert!(!c.leaving(), "no pending quit in the epoch");
        assert!(!c.take_quit_since(deadline), "observed before the deadline");
        c.command(Command::Quit);
        assert!(c.take_quit_since(deadline));
        assert!(!c.take_quit_since(deadline), "taken once");
        c.command(Command::Quit);
        c.open_epoch();
        assert!(!c.take_quit_since(deadline), "dropped at the epoch's end");
    }

    /// The rig's thread failures for a watcher test, read the way the rig
    /// reads them: an input link and an audio control of their own, and an
    /// adjustment driver's latch. The worker and the hook come back so the
    /// test can end the threads' ends itself.
    fn failures() -> (
        crate::failures::Failures,
        game::AudioWorker,
        crate::input_side::Hook,
        crate::input_side::InputLink,
        game::AdjustDriver,
    ) {
        use std::sync::Arc;
        let bank = Arc::new(audio::SoundBank::from_sounds(
            48_000,
            vec![audio::Sound::mono(vec![0; 480])],
        ));
        let engine =
            audio::AudioEngine::with_schedule(bank, Vec::new(), 48_000, 480, 16, 0).unwrap();
        let (control, worker) = game::audio_control(engine);
        let controls = Arc::new(Controls::new(QpcClock::start()));
        let (hook, link) = crate::input_side::link(
            controls,
            crate::input_side::Mode::Clock,
            input::Waker::new().unwrap(),
        );
        let device = game::DeviceSettings::default();
        let keys = game::KeyConfigs {
            keyboard: device.keyboard.clone(),
            controller: device.controller.clone(),
            controller_device: None,
        };
        let plan = game::AdjustmentConfig::default()
            .plan(&keys, None, None)
            .unwrap();
        let (driver, _tap, _reader) =
            game::AdjustDriver::new(&plan, game::PlayOptions::default(), 0).unwrap();
        let mut failures = crate::failures::Failures::new(
            link.failure_handle(),
            control.failure_probe(),
            Arc::default(),
        );
        failures.install(driver.failure_cell(), Arc::default());
        // The control lives as long as the test needs the probe only.
        drop(control);
        (failures, worker, hook, link, driver)
    }

    /// A pump whose input thread never answers: the driver's next wait
    /// times out while the thread itself is alive.
    struct Silent;

    impl game::InputPump for Silent {
        fn request_fence(&mut self, _n: u64) {}
        fn wake(&mut self) {}
        fn display_offset_us(&self) -> i64 {
            0
        }
        fn park(&mut self) -> Result<(), game::InputWaitError> {
            Err(game::InputWaitError::Timeout)
        }
    }

    /// The decision screen watched by a chart's load (E1, E6, E7 at the
    /// player's waits): before the deadline a look goes on and reveals
    /// nothing; at it the play's step runs once, the ingress opened, the
    /// lifetime revealed and the scene moved, and the wait goes on; then a
    /// close, the audio thread's end dropped (its consumer abandoned), or
    /// an adjustment fence timed out with the input thread alive, each
    /// read through the rig's own failure state, stops every look, the
    /// screen left to end the run. Before the cause (the positive
    /// control) the same look goes on.
    #[test]
    fn a_load_wait_serves_the_deadline_once_and_stops_at_a_failure() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        for cause in ["close", "audio gone", "fence timed out"] {
            let c = Controls::new(QpcClock::start());
            let now = bms_model::TimeSource::now(&c.clock);
            let reveals = Arc::new(AtomicUsize::new(0));
            let (failures, worker, _hook, _link, mut driver) = failures();
            let watch = |decision| {
                let reveals = Arc::clone(&reveals);
                super::DecisionWatch {
                    decision,
                    origin: game::Origin::Load,
                    reveal: Box::new(move || {
                        reveals.fetch_add(1, Ordering::SeqCst);
                        crate::render_side::AtStep::Revealed
                    }),
                    failures: failures.clone(),
                    screen: None,
                    early: None,
                }
            };
            c.close_epoch();
            c.watch_decision(watch(game::Decision::begin(
                now,
                game::DecisionLifetime(1),
                Some(game::DecisionShown::BuiltIn),
                false,
            )));
            assert!(!c.look().is_some_and(game::Verdict::leaves));
            assert_eq!(reveals.load(Ordering::SeqCst), 0);
            assert!(!c.ingress().is_open());
            let served = c.unwatch_decision().unwrap();
            assert!(!served.stepped());
            // The same screen, its deadline due.
            let due = bms_model::TimeUs(now.0 - game::DECISION_TIME.0);
            c.watch_decision(watch(game::Decision::begin(
                due,
                game::DecisionLifetime(2),
                Some(game::DecisionShown::BuiltIn),
                false,
            )));
            let scene = c.scene();
            assert!(!c.look().is_some_and(game::Verdict::leaves));
            assert!(!c.look().is_some_and(game::Verdict::leaves));
            assert_eq!(reveals.load(Ordering::SeqCst), 1);
            assert!(c.ingress().is_open());
            assert_eq!(c.scene(), scene + 1);
            assert!(!c.decision_left(), "{cause}: nothing failed yet");
            match cause {
                "close" => c.close(),
                "audio gone" => drop(worker),
                _ => {
                    assert!(driver.enter_attempt(None, &mut Silent).is_err());
                }
            }
            assert!(c.look().is_some_and(game::Verdict::leaves), "{cause}");
            assert!(c.decision_left());
            let left = c.unwatch_decision().unwrap();
            assert!(left.stepped());
            assert_eq!(left.left(), Some(game::Verdict::End), "{cause}");
            // Nothing watched: no look stops anything.
            assert!(!c.look().is_some_and(game::Verdict::leaves));
        }
    }
}
