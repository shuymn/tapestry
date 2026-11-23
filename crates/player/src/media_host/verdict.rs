//! How a worker ended, as its watcher can prove it (design.md "Threat
//! Model" T02): the watcher's decisions apart from its system calls, pure
//! so a test can follow every ordering of the job's message, the reads, the
//! terminations and the worker's end.
//!
//! The watcher asks for the job to be ended for one cause, the job's memory
//! message or a read at the line, whichever comes first, and asks again at
//! each read while the worker lives. A memory end is recorded only when a
//! termination it asked for was accepted and the process then ended with
//! that termination's exit code; any other end is the worker's own, its
//! exit code recorded. A worker that ends with one of the watcher's codes
//! though no termination with that code was accepted ended by itself, and
//! is recorded so, the code's ambiguity logged.

/// The exit code of a worker the watcher ended on the job's message.
pub const ENDED_BY_MESSAGE: u32 = 0xc0de_0002;

/// The exit code of a worker the watcher ended at the line.
pub const ENDED_AT_LINE: u32 = 0xc0de_0003;

/// How a worker ended, as its watcher saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The watcher ended it on the job's message that it refused a commit.
    Message,
    /// The watcher ended it when its peak commit reached the line.
    Line,
    /// It ended by itself, with its exit code: a crash, or an exit the
    /// game did not ask for.
    Exited(u32),
}

impl End {
    pub(super) const fn encode(self) -> u64 {
        match self {
            Self::Message => 1 << 32,
            Self::Line => 2 << 32,
            Self::Exited(code) => (3 << 32) | code as u64,
        }
    }

    pub(super) fn decode(v: u64) -> Option<Self> {
        match v >> 32 {
            1 => Some(Self::Message),
            2 => Some(Self::Line),
            3 => Some(Self::Exited(u32::try_from(v & 0xffff_ffff).ok()?)),
            _ => None,
        }
    }
}

/// A cause the watcher ends a worker's job for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The job's memory message.
    Message,
    /// A read at the line.
    Line,
}

impl Cause {
    /// The exit code the watcher ends the worker's job with.
    pub const fn code(self) -> u32 {
        match self {
            Self::Message => ENDED_BY_MESSAGE,
            Self::Line => ENDED_AT_LINE,
        }
    }

    const fn end(self) -> End {
        match self {
            Self::Message => End::Message,
            Self::Line => End::Line,
        }
    }
}

/// What the watcher asked for, and what the system accepted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    requested: Option<Cause>,
    accepted: Option<Cause>,
}

impl Verdict {
    /// The cause asked for, while the worker lives.
    pub const fn requested(&self) -> Option<Cause> {
        self.requested
    }

    /// The job's memory message about the worker: the cause to end its job
    /// for, unless one was asked for before.
    pub fn message(&mut self) -> Option<Cause> {
        self.ask(Cause::Message)
    }

    /// A read found the worker's peak commit `peak`: the cause to end its
    /// job for once the peak is at `line`, unless one was asked for before.
    pub fn read(&mut self, peak: usize, line: usize) -> Option<Cause> {
        if peak < line {
            return None;
        }
        self.ask(Cause::Line)
    }

    fn ask(&mut self, cause: Cause) -> Option<Cause> {
        if self.requested.is_some() {
            return None;
        }
        self.requested = Some(cause);
        Some(cause)
    }

    /// A termination with `cause`'s code was accepted by the system
    /// (`true`) or refused.
    pub fn terminated(&mut self, cause: Cause, accepted: bool) {
        if accepted && self.accepted.is_none() {
            self.accepted = Some(cause);
        }
    }

    /// How the worker ended once its process ended with exit `code`, and
    /// whether that code is one of the watcher's though no termination with
    /// it was accepted.
    pub fn end(&self, code: u32) -> (End, bool) {
        match self.accepted {
            Some(cause) if code == cause.code() => (cause.end(), false),
            _ => (
                End::Exited(code),
                code == ENDED_BY_MESSAGE || code == ENDED_AT_LINE,
            ),
        }
    }
}
