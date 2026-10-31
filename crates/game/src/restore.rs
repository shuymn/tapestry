//! Restoring a practice round's audio at its lead-in's start (design.md
//! "Mid-Chart Start and Section Repeat", "Audio").
//!
//! A round's recording opens [`RESTORE_AHEAD`](crate::RESTORE_AHEAD)
//! before its lead-in, so the play's origin is fixed before anything is
//! planned. The rig then plans, off the audio thread and in the play's
//! setup, a seek for the lead-in's start on the frame line of that origin,
//! and a fallback [`RESTORE_RETRY`](crate::RESTORE_RETRY) later for a
//! missed origin ([`Restorer::plan`]). Both travel with the play's start;
//! the audio thread restarts the play and applies them ([`Restore::apply`]): the
//! first, the fallback only when the first is late, and otherwise an
//! explicit stop, so a refused restore plays the round with the BGM
//! silent rather than from chart time 0.

use ::audio::{AudioEngine, FrameLine, SeekError, SeekPlan, SeekSource};
use bms_model::{BgmEvent, TimeUs};

use crate::practice::RESTORE_RETRY;

/// Why a round's audio cannot be restored; the round is refused, with no
/// start and no record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RestoreError {
    /// The rig has no restorer for the round: its engine's source and the
    /// round's schedule were not set up.
    #[error("the round's audio restore was not prepared")]
    NotPrepared,
    /// The round's schedule is not the one the engine plays.
    #[error("the round's audio schedule is not the engine's")]
    Schedule,
    /// The seek could not be planned.
    #[error("the round's audio restore cannot be planned")]
    Plan(#[source] SeekError),
}

/// What restores a round's audio: the engine's [`SeekSource`] and the
/// round's schedule, which must be the one the engine plays.
#[derive(Clone, Debug)]
pub struct Restorer {
    source: SeekSource,
    schedule: Vec<BgmEvent>,
}

/// A round's restore, planned: the seek for the lead-in's start and its
/// fallback. It travels boxed with the play's start and is freed on the
/// game thread.
#[derive(Clone, Debug)]
pub struct Restore {
    first: SeekPlan,
    fallback: SeekPlan,
    at: TimeUs,
}

impl Restorer {
    /// A restorer for the engine `source` describes, playing `schedule`.
    #[must_use]
    pub const fn new(source: SeekSource, schedule: Vec<BgmEvent>) -> Self {
        Self { source, schedule }
    }

    /// Checks that the round's schedule is the engine's.
    ///
    /// # Errors
    ///
    /// [`RestoreError::Schedule`] when it is not.
    pub fn check(&self) -> Result<(), RestoreError> {
        if self.source.matches(&self.schedule) {
            Ok(())
        } else {
            Err(RestoreError::Schedule)
        }
    }

    /// Plans the restore at chart time `at` on `line`, the frame line of
    /// the play's origin, with its fallback [`RESTORE_RETRY`] later.
    /// Allocates the plans; the work grows with the schedule's starts
    /// before `at` times the voices.
    ///
    /// # Errors
    ///
    /// [`RestoreError::Plan`] when the schedule is not the engine's.
    pub fn plan(&self, line: FrameLine, at: TimeUs) -> Result<Box<Restore>, RestoreError> {
        let plan = |t| SeekPlan::new(&self.source, &self.schedule, line, t);
        let first = plan(at).map_err(RestoreError::Plan)?;
        let fallback =
            plan(TimeUs(at.0.saturating_add(RESTORE_RETRY.0))).map_err(RestoreError::Plan)?;
        Ok(Box::new(Restore {
            first,
            fallback,
            at,
        }))
    }
}

impl Restore {
    /// The chart time the first plan restores at.
    #[must_use]
    pub const fn at(&self) -> TimeUs {
        self.at
    }

    /// The chart time the fallback restores at.
    #[must_use]
    pub const fn fallback_at(&self) -> TimeUs {
        TimeUs(self.at.0.saturating_add(RESTORE_RETRY.0))
    }
}

/// Why the audio thread refused both plans, or the first for a reason
/// other than lateness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreRefusal {
    /// The plan was made for another engine or schedule.
    Source,
    /// The plan's line has another rate or period.
    Line,
    /// The plan's frame was already written.
    Late,
    /// The play was ringing out.
    RingingOut,
}

/// How a round's audio was restored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restored {
    /// At the lead-in's start.
    First,
    /// [`RESTORE_RETRY`] later, the first being late.
    Fallback,
    /// Not at all: the schedule was stopped, so the BGM stays silent while
    /// key sounds still play.
    Refused(RestoreRefusal),
}

impl Restored {
    /// The outcome as a small nonzero code.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::First => 1,
            Self::Fallback => 2,
            Self::Refused(RestoreRefusal::Source) => 3,
            Self::Refused(RestoreRefusal::Line) => 4,
            Self::Refused(RestoreRefusal::Late) => 5,
            Self::Refused(RestoreRefusal::RingingOut) => 6,
        }
    }

    /// The outcome of a [`code`](Self::code); `None` for 0, no restore.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::First,
            2 => Self::Fallback,
            3 => Self::Refused(RestoreRefusal::Source),
            4 => Self::Refused(RestoreRefusal::Line),
            5 => Self::Refused(RestoreRefusal::Late),
            6 => Self::Refused(RestoreRefusal::RingingOut),
            _ => return None,
        })
    }
}

#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
const fn refusal(e: SeekError) -> RestoreRefusal {
    match e {
        SeekError::Source => RestoreRefusal::Source,
        SeekError::Line { .. } => RestoreRefusal::Line,
        SeekError::Late { .. } => RestoreRefusal::Late,
        SeekError::RingingOut => RestoreRefusal::RingingOut,
    }
}

impl Restore {
    /// Applies the restore to `engine` right after its
    /// [`restart`](AudioEngine::restart): the first plan, the fallback when
    /// the first is late, and otherwise [`stop`](AudioEngine::stop), so the
    /// schedule never plays from chart time 0 while key sounds still play.
    /// Allocates nothing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn apply(&self, engine: &mut AudioEngine) -> Restored {
        let outcome = match engine.seek(&self.first) {
            Ok(()) => return Restored::First,
            Err(SeekError::Late { .. }) => match engine.seek(&self.fallback) {
                Ok(()) => return Restored::Fallback,
                Err(e) => Restored::Refused(refusal(e)),
            },
            Err(e) => Restored::Refused(refusal(e)),
        };
        engine.stop();
        outcome
    }
}
