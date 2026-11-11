//! The decision screen's state (design.md "Play Lifecycle", the decision
//! screen): from a chart's choice to its deadline, every look and every
//! reply between the choice and the practice page or the loading state's
//! opening is classified here, at one point, in one order (terminal causes
//! first, then latched refusals, then the deadline's route), so no wait can
//! act on a reply before it.
//!
//! The state is pure: the caller supplies the time and its looks (a
//! terminal cause, the escape key) and acts on the [`Verdict`]. The App
//! serves it between its own waits; a rig serves it inside the waits it
//! owns, through the same [`Decision`] lent to it.

use bms_model::TimeUs;

/// The built-in presentation's time from the choice (design.md :753).
pub const DECISION_TIME: TimeUs = TimeUs(1_500_000);

/// Where the decision screen is when it looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The chart's read runs.
    Read,
    /// The play's load (bank, loudness, the screen's chart) runs.
    Load,
    /// The play's load is ready; the screen waits for its deadline.
    Ready,
    /// Practice: the chart is read; its page's jobs run until the deadline.
    Jobs,
    /// Practice past the deadline, the 1× read still running (D11): the
    /// decision presentation stays, and Back and the quit key leave.
    Waiting,
    /// The play's loading state, entered at the deadline, before its
    /// opening.
    Loading,
}

/// What a wait's completed reply was, as far as the decision screen cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// A refusal the player goes back from (the chart, the preset, the
    /// preset shelf, the bank, the unload): held until the deadline.
    Refusal,
    /// A failure the run ends with (the read refused or panicked, a page
    /// job's panic a Start was responsible for).
    Fatal,
}

/// What the caller does after a look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing: go on waiting, or act on the reply.
    Continue,
    /// The deadline: the play's visible loading entry, or the practice
    /// page's entry, now and once.
    Step,
    /// The run ends (a close, a thread's failure, a fatal reply).
    End,
    /// Back to selection with the refusal latched; no loading state, no
    /// page, no preparation.
    Return,
    /// Back to selection by the player, past the deadline while practice
    /// waits for its read (D11).
    Back,
}

impl Verdict {
    /// Whether the decision screen is left: the run ends, or selection
    /// returns. A wait that gets one stops and hands it to its caller.
    #[must_use]
    pub const fn leaves(self) -> bool {
        matches!(self, Self::End | Self::Return | Self::Back)
    }
}

/// How a wait the decision screen serves ended: its operation done, or the
/// screen left by the verdict a look returned (the operation left running,
/// its answer going nowhere). A wait never decides a transition itself; it
/// hands the classified one to its caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served<T> {
    /// The operation answered.
    Done(T),
    /// A look left the decision screen with this verdict.
    Left(Verdict),
}

/// The decision screen of one choice, or none ([`Decision::off`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    /// The deadline on the rig's clock; none when the screen is off.
    deadline: Option<TimeUs>,
    /// The choice was to practice.
    practice: bool,
    /// The deadline's step ran.
    stepped: bool,
    /// A refusal is held to the deadline.
    latched: bool,
    /// Practice past the deadline, waiting for its read.
    waiting: bool,
    /// How the screen was left inside a wait the rig owns, for the caller
    /// to route once the wait returns.
    left: Option<Verdict>,
}

impl Decision {
    /// No decision screen: every look continues.
    #[must_use]
    pub const fn off() -> Self {
        Self {
            deadline: None,
            practice: false,
            stepped: false,
            latched: false,
            waiting: false,
            left: None,
        }
    }

    /// The decision screen of a choice made at `now`, shown for `time`
    /// (none: no screen), to practice with `practice`.
    #[must_use]
    pub fn begin(now: TimeUs, time: Option<TimeUs>, practice: bool) -> Self {
        Self {
            deadline: time.map(|t| TimeUs(now.0.saturating_add(t.0))),
            practice,
            ..Self::off()
        }
    }

    /// Whether a decision screen is shown.
    #[must_use]
    pub const fn on(&self) -> bool {
        self.deadline.is_some()
    }

    /// The deadline, when on.
    #[must_use]
    pub const fn deadline(&self) -> Option<TimeUs> {
        self.deadline
    }

    /// Whether the choice was to practice.
    #[must_use]
    pub const fn practice(&self) -> bool {
        self.practice
    }

    /// Whether the deadline's step ran.
    #[must_use]
    pub const fn stepped(&self) -> bool {
        self.stepped
    }

    /// Whether a refusal is held.
    #[must_use]
    pub const fn latched(&self) -> bool {
        self.latched
    }

    /// Whether practice waits past the deadline for its read.
    #[must_use]
    pub const fn waiting(&self) -> bool {
        self.waiting
    }

    /// Whether the screen is off, or its deadline was stepped or passed.
    #[must_use]
    pub fn past(&self, now: TimeUs) -> bool {
        self.deadline.is_none_or(|d| now.0 >= d.0)
    }

    /// How a wait the rig owns left the screen, if it did.
    #[must_use]
    pub const fn left(&self) -> Option<Verdict> {
        self.left
    }

    /// How a wait the rig owns left the screen, taken.
    pub const fn take_left(&mut self) -> Option<Verdict> {
        self.left.take()
    }

    /// A wait the rig owns ends on `verdict` (not [`Verdict::Continue`]);
    /// the caller routes it when the wait returns.
    pub const fn leave(&mut self, verdict: Verdict) {
        self.left = Some(verdict);
    }

    /// Classifies a look at `now` from `origin`: `terminal` when the run is
    /// ending or a thread failed, `escape` when Back or the quit key is
    /// pressed, `reply` a wait's completed reply. In this order, and
    /// nothing acts before it: a terminal cause or a fatal reply ends the
    /// run; a refusal is latched (and returns at once once the loading
    /// state or the wait for a late read is reached); at the deadline a
    /// latched refusal returns, else the step runs once (practice whose
    /// read still runs waits instead); past it, while practice waits,
    /// the escape goes back.
    pub fn classify(
        &mut self,
        origin: Origin,
        now: TimeUs,
        terminal: bool,
        escape: bool,
        reply: Option<Reply>,
    ) -> Verdict {
        if terminal || reply == Some(Reply::Fatal) {
            return Verdict::End;
        }
        if reply == Some(Reply::Refusal) {
            self.latched = true;
            if matches!(origin, Origin::Loading | Origin::Waiting) || self.deadline.is_none() {
                return Verdict::Return;
            }
        }
        if let Some(deadline) = self.deadline
            && now.0 >= deadline.0
        {
            if self.latched {
                return Verdict::Return;
            }
            if !self.stepped {
                if self.practice && origin == Origin::Read {
                    self.waiting = true;
                } else {
                    self.stepped = true;
                    self.waiting = false;
                    return Verdict::Step;
                }
            }
        }
        if self.waiting && escape {
            return Verdict::Back;
        }
        Verdict::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: i64) -> TimeUs {
        TimeUs(ms * 1_000)
    }

    fn play() -> Decision {
        Decision::begin(at(0), Some(DECISION_TIME), false)
    }

    fn practice() -> Decision {
        Decision::begin(at(0), Some(DECISION_TIME), true)
    }

    /// Off, every look continues; a refusal returns at once.
    #[test]
    fn off_continues_and_returns_a_refusal_at_once() {
        let mut d = Decision::off();
        assert_eq!(
            d.classify(Origin::Read, at(10_000), false, false, None),
            Verdict::Continue
        );
        assert_eq!(
            d.classify(Origin::Load, at(0), false, false, Some(Reply::Refusal)),
            Verdict::Return
        );
    }

    /// The order: a terminal cause before a refusal and the deadline; a
    /// fatal reply before the deadline (E3c); a refusal with the deadline
    /// due returns without the step (E3b); without the deadline it is held
    /// (the positive control).
    #[test]
    fn terminal_then_refusal_then_the_deadline() {
        let mut d = play();
        assert_eq!(
            d.classify(Origin::Load, at(1_500), true, false, Some(Reply::Refusal)),
            Verdict::End
        );
        let mut d = play();
        assert_eq!(
            d.classify(Origin::Read, at(1_500), false, false, Some(Reply::Fatal)),
            Verdict::End
        );
        assert!(!d.stepped());
        let mut d = play();
        assert_eq!(
            d.classify(Origin::Read, at(1_500), false, false, Some(Reply::Refusal)),
            Verdict::Return
        );
        assert!(!d.stepped(), "no loading state");
        let mut d = play();
        assert_eq!(
            d.classify(Origin::Read, at(1_499), false, false, Some(Reply::Refusal)),
            Verdict::Continue,
            "held to the deadline"
        );
        assert_eq!(
            d.classify(Origin::Ready, at(1_500), false, false, None),
            Verdict::Return
        );
    }

    /// The step runs once at the deadline; after it a refusal returns at
    /// once (E4).
    #[test]
    fn the_step_runs_once_then_refusals_return_at_once() {
        let mut d = play();
        assert_eq!(
            d.classify(Origin::Load, at(1_499), false, false, None),
            Verdict::Continue
        );
        assert_eq!(
            d.classify(Origin::Load, at(1_500), false, false, None),
            Verdict::Step
        );
        assert_eq!(
            d.classify(Origin::Load, at(1_600), false, false, None),
            Verdict::Continue
        );
        assert_eq!(
            d.classify(
                Origin::Loading,
                at(1_700),
                false,
                false,
                Some(Reply::Refusal)
            ),
            Verdict::Return
        );
    }

    /// Practice whose read outlasts the deadline waits (D11): no step;
    /// the escape before the deadline does nothing, past it it goes back;
    /// once read, the next look steps (the page's entry).
    #[test]
    fn a_late_practice_read_waits_and_the_escape_goes_back_past_the_time() {
        let mut d = practice();
        assert_eq!(
            d.classify(Origin::Read, at(1_000), false, true, None),
            Verdict::Continue
        );
        assert_eq!(
            d.classify(Origin::Read, at(1_500), false, false, None),
            Verdict::Continue
        );
        assert!(d.waiting());
        assert_eq!(
            d.classify(Origin::Read, at(1_600), false, true, None),
            Verdict::Back
        );
        let mut d = practice();
        assert_eq!(
            d.classify(Origin::Read, at(1_500), false, false, None),
            Verdict::Continue
        );
        assert_eq!(
            d.classify(Origin::Jobs, at(1_700), false, false, None),
            Verdict::Step
        );
        assert!(!d.waiting());
        assert_eq!(
            d.classify(
                Origin::Waiting,
                at(1_550),
                false,
                false,
                Some(Reply::Refusal)
            ),
            Verdict::Return
        );
    }

    /// A practice read done before the deadline steps at the deadline, not
    /// earlier (W2).
    #[test]
    fn a_read_in_time_steps_at_the_deadline() {
        let mut d = practice();
        assert_eq!(
            d.classify(Origin::Jobs, at(900), false, false, None),
            Verdict::Continue
        );
        assert_eq!(
            d.classify(Origin::Jobs, at(1_500), false, false, None),
            Verdict::Step
        );
    }
}
