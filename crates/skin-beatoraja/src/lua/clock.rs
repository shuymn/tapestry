//! The analysis time budget ([`MAX_ANALYSIS_TIME`]): the monotonic time
//! a candidate's analysis spends inside its phases (design.md "Threat
//! Model", T09).
//!
//! Five phases count: closing the captured object graph, the effect walk,
//! admission, each lowering of a callable, and each role scan outside a
//! lowering (an image set's value). [`AnalysisClock::enter`] and
//! [`AnalysisClock::leave`] bracket each one, so time between phases (the
//! common import's other work) does not count. The clock is checked
//! at every `enter`, at every root of the walk, at the cancellation
//! cadence of [`super::callback::Analysis::charge`], and at every `leave`
//! after the phase's interval is added, so the last phase's time is
//! checked too. Past the cap, the candidate refuses with
//! [`super::LuaLimit::AnalysisTime`], fatal, located at the callable the
//! phase retained, or nowhere when it retained none.

use super::sync::SyncCell;
use std::time::Duration;

use super::Unsupported;
use super::limits::MAX_ANALYSIS_TIME;

/// What a walk past [`MAX_ANALYSIS_TIME`] gives: fatal wherever it is
/// met ([`super::LuaLimit::AnalysisTime`]).
pub(crate) const TOO_LONG: Unsupported = Unsupported::Construct("analysis time past its cap");

/// A phase the clock counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Closing the captured object graph, after the conversion.
    Graph,
    /// The effect walk.
    Walk,
    /// Admission.
    Admit,
    /// One lowering of a callable.
    Lower,
    /// A role scan outside a lowering: an image set's value.
    Scan,
}

/// A point on the monotonic clock: [`std::time::Instant`], or a test's
/// fake time.
#[cfg(not(test))]
type Stamp = std::time::Instant;
#[cfg(test)]
type Stamp = Duration;

#[cfg(not(test))]
fn now() -> Stamp {
    std::time::Instant::now()
}

#[cfg(not(test))]
fn since(s: Stamp) -> Duration {
    s.elapsed()
}

#[cfg(test)]
fn now() -> Stamp {
    fake::NOW.with(std::cell::Cell::get)
}

#[cfg(test)]
fn since(s: Stamp) -> Duration {
    now().saturating_sub(s)
}

/// The time an analysis spent in its phases.
#[derive(Debug, Default)]
pub(crate) struct AnalysisClock {
    /// The time of the phases left.
    spent: SyncCell<Duration>,
    /// When the current phase was entered, if one is.
    since: SyncCell<Option<Stamp>>,
    /// The node of the callable the current phase is at, if any.
    at: SyncCell<Option<u32>>,
}

impl AnalysisClock {
    /// Enters `phase` at the callable at node `at`, if any: checked.
    pub(crate) fn enter(&self, phase: Phase, at: Option<u32>) -> Result<(), Unsupported> {
        self.at.set(at);
        #[cfg(test)]
        fake::entering(phase, false);
        self.since.set(Some(now()));
        #[cfg(test)]
        fake::entering(phase, true);
        #[cfg(not(test))]
        let _ = phase;
        self.check()
    }

    /// Moves the current phase to the root at node `at`: checked.
    pub(crate) fn root(&self, at: u32) -> Result<(), Unsupported> {
        self.at.set(Some(at));
        #[cfg(test)]
        fake::rooting();
        self.check()
    }

    /// Moves the current phase off any callable.
    pub(crate) fn clear(&self) {
        self.at.set(None);
    }

    /// The node of the callable the current phase is at, if any.
    pub(crate) fn retained(&self) -> Option<u32> {
        self.at.get()
    }

    /// Leaves the current phase, its interval added, then checks.
    pub(crate) fn leave(&self) -> Result<(), Unsupported> {
        if let Some(s) = self.since.take() {
            self.spent.set(self.spent.get().saturating_add(since(s)));
        }
        let r = self.check();
        if r.is_ok() {
            self.at.set(None);
        }
        r
    }

    /// [`TOO_LONG`] once the time spent, the current phase's so far
    /// included, is past [`MAX_ANALYSIS_TIME`].
    pub(crate) fn check(&self) -> Result<(), Unsupported> {
        let current = self.since.get().map_or(Duration::ZERO, since);
        if self.spent.get().saturating_add(current) > MAX_ANALYSIS_TIME {
            return Err(TOO_LONG);
        }
        Ok(())
    }

    /// Runs `f` as `phase` at `at`: `f`'s fatal result first, then the
    /// checked leave's.
    pub(crate) fn phase<T>(
        &self,
        phase: Phase,
        at: Option<u32>,
        f: impl FnOnce() -> Result<T, Unsupported>,
    ) -> Result<T, Unsupported> {
        self.enter(phase, at)?;
        let r = f();
        let left = self.leave();
        let r = r?;
        left.map(|()| r)
    }
}

/// The test clock: it stands still until a test advances it.
#[cfg(test)]
pub(crate) mod fake {
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

    use super::Phase;

    type Hook = (Phase, bool, Box<dyn Fn()>);

    thread_local! {
        pub(super) static NOW: Cell<Duration> = const { Cell::new(Duration::ZERO) };
        static AT_PHASE: RefCell<Option<Hook>> = const { RefCell::new(None) };
        static AT_ROOT: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
    }

    /// Advances this thread's clock by `d`.
    pub(crate) fn advance(d: Duration) {
        NOW.with(|n| n.set(n.get().saturating_add(d)));
    }

    /// Runs `f` when `phase` is next entered: inside it (after its start
    /// is taken, before it is checked) when `inside`, else just before.
    pub(crate) fn at_phase(phase: Phase, inside: bool, f: impl Fn() + 'static) {
        AT_PHASE.with(|a| *a.borrow_mut() = Some((phase, inside, Box::new(f))));
    }

    /// Runs `f` when the effect walk next reaches a root, before it is
    /// checked.
    pub(crate) fn at_root(f: impl Fn() + 'static) {
        AT_ROOT.with(|a| *a.borrow_mut() = Some(Box::new(f)));
    }

    pub(super) fn rooting() {
        if let Some(f) = AT_ROOT.with(|a| a.borrow_mut().take()) {
            f();
        }
    }

    pub(super) fn entering(phase: Phase, inside: bool) {
        let due = AT_PHASE.with(|a| {
            let mut a = a.borrow_mut();
            match a.as_ref() {
                Some((p, i, _)) if *p == phase && *i == inside => a.take(),
                _ => None,
            }
        });
        if let Some((_, _, f)) = due {
            f();
        }
    }
}
