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
    /// not positive, or a window or LN-end edge scaled beyond 2^32 µs).
    InvalidRank,
    /// The chart declares a long-note mode the preset cannot judge.
    UnsupportedLnMode(LnMode),
    /// The chart has no note to judge, so the gauge has no denominator.
    NoJudgeableNotes,
    /// A note time or long-note end lies outside `±2^32` µs, the chart time
    /// domain.
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
    /// The section is unusable: `field` names what is wrong (`range` for an
    /// end before the start, `lead_in` for a negative lead-in or a lead-in
    /// start outside `i64`, `initial_gauges` for a count other than the
    /// preset's gauges or a value outside a gauge's bounds or at 0 for a
    /// survival gauge).
    InvalidSection {
        /// What is wrong.
        field: &'static str,
    },
    /// Batch evaluation could not queue an input of the log.
    Input(PushError),
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPreset(_) => f.write_str("invalid preset"),
            Self::InvalidRank => f.write_str("the chart's rank gives no usable judgment window"),
            Self::UnsupportedLnMode(mode) => {
                write!(f, "the chart's long-note mode {mode:?} is not supported")
            }
            Self::NoJudgeableNotes => f.write_str("the chart has no notes to play"),
            Self::TimeOverflow => f.write_str("a note is outside the time range"),
            Self::DuplicateBinding { source, control } => {
                write!(f, "source {} binds control {} twice", source.0, control.0)
            }
            Self::TooManyBindings => f.write_str("the device profile binds too many controls"),
            Self::InvalidSection { field } => write!(f, "the section's {field} is invalid"),
            // `PushError` is a plain value, not an `Error`, so it is printed
            // here and is no source.
            Self::Input(e) => write!(f, "the input log cannot be evaluated: {e:?}"),
        }
    }
}

impl std::error::Error for EvalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPreset(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use rules::RuleError;

    use super::EvalError;

    #[test]
    fn invalid_preset_has_the_rule_error_as_source() {
        let rule = RuleError::NoGauges;
        let e = EvalError::InvalidPreset(rule.clone());
        assert_eq!(e.to_string(), "invalid preset");
        let source = e.source().expect("the rule error is the source");
        assert_eq!(source.downcast_ref::<RuleError>(), Some(&rule));
        assert!(
            source.source().is_none(),
            "the chain ends at the rule error"
        );
        assert!(!e.to_string().contains(&source.to_string()));
    }

    #[test]
    fn other_errors_have_no_source() {
        assert!(EvalError::InvalidRank.source().is_none());
        let e = EvalError::Input(crate::PushError::Full);
        assert_eq!(e.to_string(), "the input log cannot be evaluated: Full");
        assert!(e.source().is_none());
    }
}
