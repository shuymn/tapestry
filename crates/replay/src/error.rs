//! Why a record could not be decoded.

use std::fmt;

/// A section of the body, in the order they are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// Evaluator, loader and app versions, and the build provenance.
    Versions,
    /// Chart identity and `#RANDOM` seed and results.
    Chart,
    /// The preset record.
    Preset,
    /// Session policy and play type.
    Session,
    /// Sources and offsets.
    Devices,
    /// The held-input snapshot.
    Start,
    /// The physical input events.
    Events,
    /// Devices that arrived during play.
    Arrivals,
    /// End status, finalized watermark and incompleteness.
    Status,
}

/// Why [`decode`](crate::decode) refused a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The file is larger than [`DecodeLimits::max_compressed`](crate::DecodeLimits::max_compressed).
    FileTooLarge {
        /// The file's length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The file does not start with the magic bytes.
    BadMagic,
    /// The format version is newer than this decoder, or unknown.
    UnsupportedVersion(u16),
    /// The body encoding is unknown.
    UnsupportedEncoding(u8),
    /// The declared uncompressed length exceeds
    /// [`DecodeLimits::max_uncompressed`](crate::DecodeLimits::max_uncompressed).
    DeclaredLengthTooLarge {
        /// The declared length.
        declared: u32,
        /// The limit.
        max: u32,
    },
    /// The body is not valid compressed data, or it decompresses to more
    /// than the declared length.
    Decompress,
    /// The body decompresses to fewer bytes than declared.
    LengthMismatch {
        /// The declared length.
        declared: u32,
        /// The decompressed length.
        actual: usize,
    },
    /// The data ended in the middle of a value.
    Truncated,
    /// A section's length exceeds the bytes left in the body.
    SectionLength {
        /// The section.
        section: Section,
        /// Its declared length.
        len: u32,
        /// The bytes left after its length.
        remaining: usize,
    },
    /// A section or the body has bytes after its last value.
    TrailingBytes,
    /// A varint is overlong or exceeds its type.
    BadVarint,
    /// A value is out of its range or an unknown tag.
    InvalidValue(&'static str),
    /// A string is not UTF-8.
    InvalidUtf8,
    /// The record has no source.
    NoSources,
    /// More sources than [`DecodeLimits::max_sources`](crate::DecodeLimits::max_sources).
    TooManySources(u64),
    /// More events than [`DecodeLimits::max_events`](crate::DecodeLimits::max_events).
    TooManyEvents(u64),
    /// A list declares more elements than the bytes left could hold.
    CountBeyondData(u64),
    /// An event or held control names a source the record does not have.
    UnknownSource(u16),
    /// The stored preset hash does not match the stored preset bytes.
    PresetHashMismatch,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileTooLarge { len, max } => {
                write!(f, "the file is {len} bytes, above the limit of {max}")
            }
            Self::BadMagic => f.write_str("not an input log"),
            Self::UnsupportedVersion(n) => write!(f, "unsupported format version {n}"),
            Self::UnsupportedEncoding(n) => write!(f, "unsupported body encoding {n}"),
            Self::DeclaredLengthTooLarge { declared, max } => write!(
                f,
                "the declared body length {declared} is above the limit of {max}"
            ),
            Self::Decompress => {
                f.write_str("the body does not decompress within its declared length")
            }
            Self::LengthMismatch { declared, actual } => write!(
                f,
                "the body decompresses to {actual} bytes, not the declared {declared}"
            ),
            Self::Truncated => f.write_str("the data ends in the middle of a value"),
            Self::SectionLength {
                section,
                len,
                remaining,
            } => write!(
                f,
                "section {section:?} declares {len} bytes but {remaining} remain"
            ),
            Self::TrailingBytes => f.write_str("unexpected bytes after the last value"),
            Self::BadVarint => f.write_str("a varint is overlong or out of range"),
            Self::InvalidValue(what) => write!(f, "invalid {what}"),
            Self::InvalidUtf8 => f.write_str("a string is not UTF-8"),
            Self::NoSources => f.write_str("the record has no source"),
            Self::TooManySources(n) => write!(f, "{n} sources exceed the limit"),
            Self::TooManyEvents(n) => write!(f, "{n} events exceed the limit"),
            Self::CountBeyondData(n) => write!(f, "a list of {n} elements exceeds the data"),
            Self::UnknownSource(s) => write!(f, "unknown source {s}"),
            Self::PresetHashMismatch => f.write_str("the preset hash does not match its bytes"),
        }
    }
}

impl std::error::Error for DecodeError {}
