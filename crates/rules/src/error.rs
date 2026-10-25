//! Why a preset was rejected.

use std::fmt;

use crate::Judgment;

/// A side of a judgment window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Before the note.
    Early,
    /// After the note.
    Late,
}

/// Why [`RulePreset::validate`](crate::RulePreset::validate) rejected a
/// preset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleError {
    /// A name was empty.
    EmptyName {
        /// The field.
        field: &'static str,
    },
    /// A window edge was negative, or not wider than the better judgment's
    /// edge on the same side.
    WindowsNotAscending {
        /// The side.
        side: Side,
        /// The judgment whose edge is out of order.
        judgment: Judgment,
    },
    /// A value that must be positive (or, for times, not negative) was not.
    OutOfRange {
        /// The field.
        field: &'static str,
    },
    /// The preset has no gauge.
    NoGauges,
    /// Two gauges share a name.
    DuplicateGauge {
        /// The shared name.
        id: String,
    },
    /// A gauge value or factor is outside its bounds.
    GaugeOutOfBounds {
        /// The gauge.
        id: String,
        /// The field.
        field: &'static str,
    },
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName { field } => write!(f, "{field} is empty"),
            Self::WindowsNotAscending { side, judgment } => write!(
                f,
                "the {side:?} edge of the {judgment:?} window is negative or not wider than the better judgment's"
            ),
            Self::OutOfRange { field } => write!(f, "{field} is out of range"),
            Self::NoGauges => f.write_str("the preset has no gauge"),
            Self::DuplicateGauge { id } => write!(f, "gauge {id:?} is defined twice"),
            Self::GaugeOutOfBounds { id, field } => {
                write!(f, "gauge {id:?}: {field} is out of bounds")
            }
        }
    }
}

impl std::error::Error for RuleError {}
