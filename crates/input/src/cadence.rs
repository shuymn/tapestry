//! When the input thread wakes (design.md "Input", "Raw Input reception"):
//! the wake policy as a pure state machine, the loop that runs it against
//! a [`Waiter`], and what that loop measured ([`WakeReport`]).
//!
//! The thread is in one of three [`Phase`]s, which its hook reports after
//! every wake:
//!
//! - **Open** (recording) and **Tail** (recording closed, until the audio
//!   tail ends) keep a heartbeat: a deadline at most [`HEARTBEAT`] after
//!   the last publication. Input wakes never move it; only a publication
//!   at or after it does, to the next point of its 1 ms grid.
//! - **Idle** has no timer: the thread sleeps until input, a device change
//!   or the wake event (a request, a focus change, stop).
//!
//! The thread only enters Idle when the hook says so, which it does on
//! request; it starts in Tail.
//!
//! Two bounds hold under any input rate. Input wakes are at least
//! [`INPUT_GAP`] apart: input that arrives within the gap ends the wait,
//! but is not read; the thread then waits for the timer only, never for
//! input, until the gap's end, and reads it there, so input left queued
//! cannot make a wait return at once. Input slower than the gap costs one
//! wait per arrival. A drain reads at most [`DRAIN_CUT`] messages
//! ([`batch`]); a cut drain publishes, runs the hook, and the rest is read
//! at the gap's end under a new stamp, so publication, the hook and stop
//! progress at least once per `DRAIN_CUT` messages.
//!
//! [`Driven::Timer`] (the autoplay thread, which receives no device) waits
//! for the timer only while Open or Tail and for the wake event only while
//! Idle.
//!
//! Nothing here reads a clock or calls the OS: the loop takes a
//! [`TimeSource`] and a [`Waiter`], so it runs on a virtual clock with
//! stubbed waits in tests. Running it allocates nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, Ordering};

use bms_model::{TimeSource, TimeUs};

use crate::sources::SourceSet;
use crate::time::WakeHistogram;

/// The heartbeat's period: while Open or Tail, the deadline lies at most
/// this long after the last publication.
pub const HEARTBEAT: TimeUs = TimeUs(1_000);

/// The shortest time between two wakes caused by input.
pub const INPUT_GAP: TimeUs = TimeUs(125);

/// The most messages one batch reads; the rest is read by the next batch,
/// after a publication.
pub const DRAIN_CUT: u32 = 1_024;

/// What the input thread is doing, as its hook reports after each wake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Recording is open.
    Open,
    /// Recording is closed and the audio tail still plays.
    Tail,
    /// Nothing waits on the thread's publications: no timer.
    Idle,
}

/// What a thread wakes for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driven {
    /// Raw Input: input and device changes wake it, besides the heartbeat
    /// and the wake event.
    Input,
    /// No device: the heartbeat and the wake event only (autoplay).
    Timer,
}

/// One wait: always for the wake event, and for the timer and input as
/// given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wait {
    /// When the timer fires; `None` for no timer.
    pub deadline: Option<TimeUs>,
    /// Whether queued input (Raw Input or a device change), including
    /// input already seen but not yet read, ends the wait.
    pub input: bool,
}

/// Why a wait returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woken {
    /// The wake event was signaled.
    Event,
    /// Input is queued.
    Input,
    /// The timer fired, or its deadline had already passed.
    Timer,
}

/// The wait itself failed: the thread cannot wait any more and ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the input thread's wait failed")]
pub struct WaitFailed;

/// Waits as [`Wait`] says.
pub trait Waiter {
    /// Returns when the wake event is signaled, when input is queued and
    /// `wait.input`, or at `wait.deadline`; at once for a deadline that
    /// has passed.
    ///
    /// # Errors
    ///
    /// [`WaitFailed`] when the wait cannot be made.
    fn wait(&mut self, wait: Wait) -> Result<Woken, WaitFailed>;
}

/// Why a wake is processed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// Input or a device change arrived.
    Input,
    /// The wake event: a request, a focus change or stop.
    Event,
    /// The heartbeat's deadline passed.
    Heartbeat,
    /// The previous drain was cut, and its gap has ended.
    Drain,
}

/// What a processed wake did: the clock read its last publication was
/// made from, whether its drain was cut, and the phase its hook reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boundary {
    /// The time of the wake's last publication.
    pub published: TimeUs,
    /// Whether the drain stopped at [`DRAIN_CUT`], so input may be left.
    pub cut: bool,
    /// The phase after the hook.
    pub phase: Phase,
}

/// The wake policy's state.
#[derive(Clone, Copy, Debug)]
pub struct Cadence {
    driven: Driven,
    phase: Phase,
    /// The heartbeat's deadline, while Open or Tail.
    heartbeat: TimeUs,
    /// Until when input is not read.
    quiet_until: TimeUs,
    /// The last drain was cut: input is left queued.
    left: bool,
    /// Input arrived before `quiet_until` and waits for it.
    held: bool,
}

impl Cadence {
    /// A thread that starts in [`Phase::Tail`] at `now`.
    #[must_use]
    pub const fn new(driven: Driven, now: TimeUs) -> Self {
        Self {
            driven,
            phase: Phase::Tail,
            heartbeat: TimeUs(now.0.saturating_add(HEARTBEAT.0)),
            quiet_until: TimeUs(i64::MIN),
            left: false,
            held: false,
        }
    }

    /// The current phase.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// The heartbeat's deadline; `None` while Idle.
    #[must_use]
    pub fn heartbeat(&self) -> Option<TimeUs> {
        (self.phase != Phase::Idle).then_some(self.heartbeat)
    }

    /// The next wait, at `now`.
    #[must_use]
    pub fn wait(&self, now: TimeUs) -> Wait {
        let beat = self.heartbeat();
        let sooner = |at: TimeUs| Some(beat.map_or(at, |b| b.min(at)));
        match self.driven {
            Driven::Timer => Wait {
                deadline: beat,
                input: false,
            },
            // Input is queued: it is read at the gap's end, never waited
            // for, which would return at once.
            Driven::Input if self.left || self.held => Wait {
                deadline: sooner(self.quiet_until.max(now)),
                input: false,
            },
            Driven::Input => Wait {
                deadline: beat,
                input: true,
            },
        }
    }

    /// Why the wait that returned `woken` at `now` is processed; `None`
    /// when there is nothing to do but wait again (input that arrived
    /// within the gap, or a timer that returned before a deadline).
    pub fn woke(&mut self, woken: Woken, now: TimeUs) -> Option<Cause> {
        match woken {
            Woken::Event => Some(Cause::Event),
            Woken::Input if now < self.quiet_until => {
                self.held = true;
                None
            }
            Woken::Input => Some(Cause::Input),
            Woken::Timer if self.phase != Phase::Idle && now >= self.heartbeat => {
                Some(Cause::Heartbeat)
            }
            Woken::Timer if self.left && now >= self.quiet_until => Some(Cause::Drain),
            Woken::Timer if self.held && now >= self.quiet_until => Some(Cause::Input),
            Woken::Timer => None,
        }
    }

    /// After a wake for `cause` at `woke_at` ended at `boundary`.
    pub fn processed(&mut self, cause: Cause, woke_at: TimeUs, boundary: Boundary) {
        let was_idle = self.phase == Phase::Idle;
        self.phase = boundary.phase;
        if boundary.phase != Phase::Idle {
            let published = boundary.published;
            if was_idle {
                self.heartbeat = TimeUs(published.0.saturating_add(HEARTBEAT.0));
            } else if self.heartbeat <= published {
                // Passed: the next point of the grid after the publication,
                // so a late wake skips beats rather than making them up.
                let steps = (published.0 - self.heartbeat.0) / HEARTBEAT.0 + 1;
                self.heartbeat = TimeUs(
                    self.heartbeat
                        .0
                        .saturating_add(steps.saturating_mul(HEARTBEAT.0)),
                );
            }
        }
        if matches!(cause, Cause::Input | Cause::Drain) {
            self.quiet_until = TimeUs(woke_at.0.saturating_add(INPUT_GAP.0));
        }
        self.left = boundary.cut && self.driven == Driven::Input;
        // Every wake drains: what was held is read.
        self.held = false;
    }
}

/// One batch of a receiver, stamped `stamp`: `next` reads queued messages
/// into `sources`, at most the budget it is given, and returns how many it
/// read (0 when the queue is empty). Reading stops at [`DRAIN_CUT`]
/// messages; then the batch publishes from a fresh clock read. Returns
/// that read and whether the batch was cut.
pub fn batch<C: TimeSource>(
    clock: &C,
    sources: &mut SourceSet,
    stamp: TimeUs,
    mut next: impl FnMut(&mut SourceSet, u32) -> u32,
) -> (TimeUs, bool) {
    sources.begin_batch(stamp);
    let mut left = DRAIN_CUT;
    while left > 0 {
        let read = next(sources, left);
        if read == 0 {
            break;
        }
        left = left.saturating_sub(read);
    }
    let fresh = clock.now();
    sources.end_batch(fresh);
    (fresh, left == 0)
}

/// Processed wakes by cause.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WakeCauses {
    /// Input or a device change.
    pub input: u64,
    /// The wake event.
    pub event: u64,
    /// The heartbeat.
    pub heartbeat: u64,
    /// The rest of a cut drain.
    pub drain: u64,
}

impl WakeCauses {
    /// Every processed wake.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input + self.event + self.heartbeat + self.drain
    }
}

/// What the input thread measured.
#[derive(Clone, Debug, Default)]
pub struct WakeReport {
    /// Every return from a wait: the thread's CPU wakes.
    pub waits: u64,
    /// Waits with nothing to process yet (input within the gap, an early
    /// timer).
    pub rearms: u64,
    /// Processed wakes by cause.
    pub causes: WakeCauses,
    /// Drains cut at [`DRAIN_CUT`].
    pub cuts: u64,
    /// The intervals between consecutive processed wakes.
    pub intervals: WakeHistogram,
    /// How late each heartbeat wake was after its deadline.
    pub lateness: WakeHistogram,
    /// The intervals between consecutive publications while Open or Tail.
    pub publications: WakeHistogram,
    /// Whether the MMCSS "Games" registration succeeded.
    pub mmcss: bool,
}

impl WakeReport {
    /// An empty report; it allocates its histograms.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Runs `cadence` until `stop` is set or a wait fails: waits through
/// `waiter`, reads `clock` at each wake, and calls `wake` with that read
/// for every wake to process. `wake` drains, publishes and runs the hook,
/// and returns its [`Boundary`]. What the loop measures goes into
/// `report`.
///
/// # Errors
///
/// [`WaitFailed`] from the waiter; the loop ends there.
pub fn run<C: TimeSource, W: Waiter>(
    clock: &C,
    waiter: &mut W,
    stop: &AtomicBool,
    cadence: &mut Cadence,
    report: &mut WakeReport,
    mut wake: impl FnMut(TimeUs) -> Boundary,
) -> Result<(), WaitFailed> {
    let mut last_wake: Option<TimeUs> = None;
    let mut last_publication: Option<TimeUs> = None;
    while !stop.load(Ordering::Acquire) {
        let woken = waiter.wait(cadence.wait(clock.now()))?;
        report.waits += 1;
        if stop.load(Ordering::Acquire) {
            break;
        }
        let now = clock.now();
        let Some(cause) = cadence.woke(woken, now) else {
            report.rearms += 1;
            continue;
        };
        match cause {
            Cause::Input => report.causes.input += 1,
            Cause::Event => report.causes.event += 1,
            Cause::Heartbeat => {
                report.causes.heartbeat += 1;
                if let Some(deadline) = cadence.heartbeat() {
                    report.lateness.record(now.0.abs_diff(deadline.0));
                }
            }
            Cause::Drain => report.causes.drain += 1,
        }
        if let Some(last) = last_wake {
            report.intervals.record(now.0.abs_diff(last.0));
        }
        last_wake = Some(now);
        let boundary = wake(now);
        report.cuts += u64::from(boundary.cut);
        cadence.processed(cause, now, boundary);
        if boundary.phase == Phase::Idle {
            last_publication = None;
        } else {
            if let Some(last) = last_publication {
                report
                    .publications
                    .record(boundary.published.0.abs_diff(last.0));
            }
            last_publication = Some(boundary.published);
        }
    }
    Ok(())
}
