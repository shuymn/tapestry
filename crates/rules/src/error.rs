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
    /// The preset names an evaluation implementation or version this build
    /// does not run.
    UnsupportedImplementation {
        /// The implementation's name.
        name: &'static str,
        /// Its version.
        version: u32,
    },
    /// An LN-end edge was negative, or not wider than the better judgment's.
    LnEndNotAscending {
        /// The judgment whose edge is out of order.
        judgment: Judgment,
    },
    /// An LN-end edge was narrower than the note window's early edge of the
    /// same judgment, so a press held as long as its long note could lose
    /// its head's judgment.
    LnEndNarrowerThanNote {
        /// The judgment.
        judgment: Judgment,
    },
    /// A value that must be positive (or, for times, not negative) was not,
    /// or a window edge, before or after a rank scale, passed
    /// [`MAX_WINDOW_EDGE`](crate::MAX_WINDOW_EDGE).
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
            Self::UnsupportedImplementation { name, version } => write!(
                f,
                "the {name} evaluator version {version} is not supported (this build runs version {})",
                crate::Implementation::STANDARD_VERSION
            ),
            Self::LnEndNotAscending { judgment } => write!(
                f,
                "the {judgment:?} LN-end edge is negative or not wider than the better judgment's"
            ),
            Self::LnEndNarrowerThanNote { judgment } => write!(
                f,
                "the {judgment:?} LN-end edge is narrower than the {judgment:?} window's early edge"
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
