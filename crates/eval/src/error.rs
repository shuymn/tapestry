//! Why evaluation could not start or run.

use std::fmt;

use bms_model::LnMode;
use rules::RuleError;

use crate::evaluator::PushError;
use crate::input::{ControlId, SourceId};

/// Why [`Evaluator::new`](crate::Evaluator::new) refused to start, or
/// [`evaluate`](crate::evaluate) could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalError {
    /// The preset failed its validation.
    InvalidPreset(RuleError),
    /// The chart's rank gives no usable window scale (a `#DEFEXRANK` that is
    /// not positive, or windows scaled beyond the `i64` range).
    InvalidRank,
    /// The chart declares a long-note mode the preset cannot judge.
    UnsupportedLnMode(LnMode),
    /// The chart has no note to judge, so the gauge has no denominator.
    NoJudgeableNotes,
    /// A deadline left the `i64` range.
    TimeOverflow,
    /// A source binds one control twice.
    DuplicateBinding {
        /// The source.
        source: SourceId,
        /// The control.
        control: ControlId,
    },
    /// The profile binds more than its limits
    /// ([`MAX_BINDINGS_PER_SOURCE`](crate::MAX_BINDINGS_PER_SOURCE),
    /// [`MAX_AXES_PER_SOURCE`](crate::MAX_AXES_PER_SOURCE),
    /// [`MAX_BINDINGS`](crate::MAX_BINDINGS), [`MAX_AXES`](crate::MAX_AXES)).
    TooManyBindings,
    /// Batch evaluation could not queue an input of the log.
    Input(PushError),
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPreset(e) => write!(f, "invalid preset: {e}"),
            Self::InvalidRank => f.write_str("the chart's rank gives no usable judgment window"),
            Self::UnsupportedLnMode(mode) => {
                write!(f, "the chart's long-note mode {mode:?} is not supported")
            }
            Self::NoJudgeableNotes => f.write_str("the chart has no notes to play"),
            Self::TimeOverflow => f.write_str("a deadline is outside the time range"),
            Self::DuplicateBinding { source, control } => {
                write!(f, "source {} binds control {} twice", source.0, control.0)
            }
            Self::TooManyBindings => f.write_str("the device profile binds too many controls"),
            Self::Input(e) => write!(f, "the input log cannot be evaluated: {e:?}"),
        }
    }
}

impl std::error::Error for EvalError {}
