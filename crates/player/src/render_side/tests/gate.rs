//! A frame gate for the render thread's frame waits, shared by the
//! integrated decision tests and the frame interval's tests: while it
//! paces, each wait takes one permit, holding until there is one; it can
//! answer that no frame is due (`suppress`, `recover`), and a screen's
//! non-blocking grant check takes a permit ready without parking
//! ([`Gate::take_ready`]).

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::render_side::tests::wait_until;

/// How long a wait for the render thread lasts at most: hang detection
/// only, never part of a claim.
const BOUND: Duration = Duration::from_secs(20);

/// The frame gate's state: while `pace`, each frame wait takes one of
/// `permits`, holding (`held`) until there is one.
#[derive(Default)]
pub(crate) struct GateState {
    pub(crate) pace: bool,
    pub(crate) permits: u32,
    pub(crate) waits: u64,
    pub(crate) held: bool,
    /// Every frame wait answers that no frame is due, after a park of at
    /// most a millisecond: nothing is drawn, and the render thread goes on
    /// taking its messages.
    pub(crate) suppress: bool,
    /// The waits answered so while suppressed.
    pub(crate) refused: u64,
    /// The next frame wait answers that no frame is due, whatever the
    /// permits, which it leaves as they are ([`Gate::recover`]).
    pub(crate) refuse_next: bool,
    /// The permits a grant check took without waiting.
    pub(crate) taken_ready: u64,
}

/// The render thread's frames held and let through one at a time.
#[derive(Default)]
pub(crate) struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl Gate {
    pub(crate) fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits on `changed` until `done`, within [`BOUND`].
    #[track_caller]
    pub(crate) fn until(&self, what: &str, done: impl Fn(&GateState) -> bool) {
        let s = self.lock();
        let (_s, waited) = self
            .changed
            .wait_timeout_while(s, BOUND, |s| !done(s))
            .unwrap_or_else(PoisonError::into_inner);
        assert!(!waited.timed_out(), "the render thread never {what}");
    }

    /// Frames held from the next frame wait on.
    pub(crate) fn hold(&self) {
        self.lock().pace = true;
        self.until("held a frame", |s| s.held);
    }

    /// One frame, then held again at the next wait.
    pub(crate) fn step(&self) {
        let before = {
            let mut s = self.lock();
            s.permits = 1;
            s.waits
        };
        self.changed.notify_all();
        self.until("drew the frame let through", |s| {
            s.waits > before && s.held && s.permits == 0
        });
    }

    /// One frame let through, the gate held, until `done` holds.
    #[track_caller]
    pub(crate) fn step_into(&self, done: impl Fn() -> bool) {
        self.lock().permits = 1;
        self.changed.notify_all();
        assert!(wait_until(done), "the frame let through never got there");
    }

    /// Frames go free again.
    pub(crate) fn release(&self) {
        self.lock().pace = false;
        self.changed.notify_all();
    }

    /// No frame is drawn from now (`true`) while the render thread keeps
    /// taking its messages: every frame wait, a held one woken included,
    /// answers that none is due, and the screen counts as paced, so the
    /// clock's pacing never draws either. Turned on, it waits until one
    /// wait was answered so after the call: the render thread asks for a
    /// wait only without a frame granted, so none granted before survives.
    pub(crate) fn suppress(&self, on: bool) {
        let before = {
            let mut s = self.lock();
            s.suppress = on;
            s.refused
        };
        self.changed.notify_all();
        if on {
            self.until("answered a frame wait with none due", |s| {
                s.refused > before
            });
        }
    }

    /// From suppressed to held, in one step: the render thread parks at
    /// its next frame wait and takes no message until let go, no frame
    /// drawn in between.
    pub(crate) fn block(&self) {
        self.set_blocked();
        self.until("held a frame", |s| s.held);
    }

    /// [`block`](Self::block) without waiting for the park: for a hook on
    /// the render thread itself, which parks at its next frame wait.
    pub(crate) fn set_blocked(&self) {
        {
            let mut s = self.lock();
            s.pace = true;
            s.permits = 0;
            s.suppress = false;
        }
        self.changed.notify_all();
    }

    /// A stall's recovery: the frame wait parked now answers that no frame
    /// is due, as a wait past its bound does, and one permit is left ready
    /// for a grant check or the next wait to take. Waits until the parked
    /// wait was answered.
    pub(crate) fn recover(&self) {
        let before = {
            let mut s = self.lock();
            assert!(s.held, "a frame wait is parked");
            s.refuse_next = true;
            s.permits = 1;
            s.refused
        };
        self.changed.notify_all();
        self.until("answered the parked wait with none due", |s| {
            s.refused > before
        });
    }

    /// A non-blocking grant check: a frame is due when the gate does not
    /// pace, or a permit is ready, which it takes.
    pub(crate) fn take_ready(&self) -> bool {
        let mut s = self.lock();
        if !(s.pace || s.suppress) {
            return true;
        }
        if s.pace && !s.suppress && !s.refuse_next && s.permits > 0 {
            s.permits -= 1;
            s.taken_ready += 1;
            drop(s);
            self.changed.notify_all();
            return true;
        }
        false
    }

    /// The render thread's frame wait: whether a frame is due.
    pub(crate) fn wait(&self) -> bool {
        let mut s = self.lock();
        s.waits += 1;
        while s.pace && s.permits == 0 && !s.suppress && !s.refuse_next {
            s.held = true;
            self.changed.notify_all();
            s = self.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
        if std::mem::take(&mut s.refuse_next) {
            s.held = false;
            s.refused += 1;
            drop(s);
            self.changed.notify_all();
            return false;
        }
        if s.suppress {
            s.held = false;
            // A short park, so the render thread's loop does not spin.
            let (mut s, _) = self
                .changed
                .wait_timeout(s, Duration::from_millis(1))
                .unwrap_or_else(PoisonError::into_inner);
            s.refused += 1;
            drop(s);
            self.changed.notify_all();
            return false;
        }
        if s.pace {
            s.permits -= 1;
        }
        s.held = false;
        drop(s);
        self.changed.notify_all();
        true
    }

    /// Whether the frames are paced by the gate: held or suppressed, the
    /// clock's pacing never draws.
    pub(crate) fn pacing(&self) -> bool {
        let s = self.lock();
        s.pace || s.suppress
    }
}
