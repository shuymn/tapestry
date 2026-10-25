//! Why evaluation could not start.

use std::fmt;

use rules::RuleError;

use crate::input::{ControlId, SourceId};

/// Why [`Evaluator::new`](crate::Evaluator::new) refused to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalError {
    /// The preset failed its validation.
    InvalidPreset(RuleError),
    /// The chart's rank gives no usable window scale (a `#DEFEXRANK` that is
    /// not positive, or windows scaled beyond the `i64` range).
    InvalidRank,
    /// A deadline left the `i64` range.
    TimeOverflow,
    /// A source binds one control twice.
    DuplicateBinding {
        /// The source.
        source: SourceId,
        /// The control.
        control: ControlId,
    },
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPreset(e) => write!(f, "invalid preset: {e}"),
            Self::InvalidRank => f.write_str("the chart's rank gives no usable judgment window"),
            Self::TimeOverflow => f.write_str("a deadline is outside the time range"),
            Self::DuplicateBinding { source, control } => {
                write!(f, "source {} binds control {} twice", source.0, control.0)
            }
        }
    }
}

impl std::error::Error for EvalError {}
