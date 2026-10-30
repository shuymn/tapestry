//! Why a preset was rejected.

use std::fmt;

use crate::{Judgment, LaneClass};

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
        /// The lane class whose table it is.
        class: LaneClass,
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
        /// The lane class whose table it is.
        class: LaneClass,
        /// The judgment whose edge is out of order.
        judgment: Judgment,
    },
    /// An LN-end edge was narrower than the note window's early edge of the
    /// same judgment and lane class, so a press held as long as its long
    /// note could lose its head's judgment.
    LnEndNarrowerThanNote {
        /// The lane class whose table it is.
        class: LaneClass,
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
    /// A gauge with a `#TOTAL` correction has a change that is a share of
    /// the per-note increment, a combination no preset specifies.
    CorrectedShare {
        /// The gauge.
        id: String,
    },
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName { field } => write!(f, "{field} is empty"),
            Self::WindowsNotAscending {
                class,
                side,
                judgment,
            } => write!(
                f,
                "{}: the {side:?} edge of the {judgment:?} window is negative or not wider than the better judgment's",
                class.name()
            ),
            Self::UnsupportedImplementation { name, version } => write!(
                f,
                "the {name} evaluator version {version} is not supported (this build runs version {})",
                crate::Implementation::STANDARD_VERSION
            ),
            Self::LnEndNotAscending { class, judgment } => write!(
                f,
                "{}: the {judgment:?} LN-end edge is negative or not wider than the better judgment's",
                class.name()
            ),
            Self::LnEndNarrowerThanNote { class, judgment } => write!(
                f,
                "{}: the {judgment:?} LN-end edge is narrower than the {judgment:?} window's early edge",
                class.name()
            ),
            Self::OutOfRange { field } => write!(f, "{field} is out of range"),
            Self::NoGauges => f.write_str("the preset has no gauge"),
            Self::DuplicateGauge { id } => write!(f, "gauge {id:?} is defined twice"),
            Self::GaugeOutOfBounds { id, field } => {
                write!(f, "gauge {id:?}: {field} is out of bounds")
            }
            Self::CorrectedShare { id } => write!(
                f,
                "gauge {id:?}: a #TOTAL correction applies to fixed changes only"
            ),
        }
    }
}

impl std::error::Error for RuleError {}

/// Why [`RulePreset::from_canonical_bytes`](crate::RulePreset::from_canonical_bytes)
/// refused its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonicalError {
    /// The bytes start with another encoding version.
    UnsupportedVersion(u64),
    /// The bytes ended inside a value.
    Truncated,
    /// A varint does not fit in 64 bits.
    VarintOverflow,
    /// A varint ends in a zero byte after its first, which the encoder never
    /// writes.
    NonMinimalVarint,
    /// A count is larger than the bytes left.
    CountBeyondData {
        /// The list or string.
        field: &'static str,
        /// The count read.
        count: u64,
    },
    /// An enum tag or a boolean has no meaning.
    UnknownTag {
        /// The field.
        field: &'static str,
        /// The tag read.
        tag: u64,
    },
    /// The implementation's name is not one this build knows.
    UnknownImplementation {
        /// The name read.
        name: String,
    },
    /// A value does not fit in its field's type.
    ValueTooLarge {
        /// The field.
        field: &'static str,
    },
    /// A string is not UTF-8.
    InvalidUtf8 {
        /// The field.
        field: &'static str,
    },
    /// A fixed-length list has another length.
    WrongLength {
        /// The list.
        field: &'static str,
        /// The length read.
        len: usize,
    },
    /// Bytes follow the preset; the count is how many.
    TrailingBytes(usize),
    /// The decoded preset fails [`RulePreset::validate`](crate::RulePreset::validate).
    Invalid(RuleError),
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => {
                write!(f, "the preset encoding version {v} is not supported")
            }
            Self::Truncated => f.write_str("the preset bytes end inside a value"),
            Self::VarintOverflow => f.write_str("a varint does not fit in 64 bits"),
            Self::NonMinimalVarint => f.write_str("a varint is not in its shortest form"),
            Self::CountBeyondData { field, count } => {
                write!(f, "{field}: the count {count} exceeds the bytes left")
            }
            Self::UnknownTag { field, tag } => write!(f, "{field}: unknown tag {tag}"),
            Self::UnknownImplementation { name } => {
                write!(f, "unknown evaluation implementation {name:?}")
            }
            Self::ValueTooLarge { field } => write!(f, "{field} is too large"),
            Self::InvalidUtf8 { field } => write!(f, "{field} is not UTF-8"),
            Self::WrongLength { field, len } => write!(f, "{field} has {len} entries"),
            Self::TrailingBytes(n) => write!(f, "{n} bytes follow the preset"),
            Self::Invalid(e) => write!(f, "the decoded preset is invalid: {e}"),
        }
    }
}

impl std::error::Error for CanonicalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(e) => Some(e),
            _ => None,
        }
    }
}
