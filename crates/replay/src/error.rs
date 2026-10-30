//! Why a record could not be decoded.

/// A section of the body, in the order they are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// Evaluator, loader and app versions, and the build provenance.
    Versions,
    /// Chart identity, `#RANDOM` seed and results, and the chart transform.
    Chart,
    /// The preset record.
    Preset,
    /// Session policy, play type and speed.
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
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The file is larger than [`DecodeLimits::max_compressed`](crate::DecodeLimits::max_compressed).
    #[error("the file is {len} bytes, above the limit of {max}")]
    FileTooLarge {
        /// The file's length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The file does not start with the magic bytes.
    #[error("not an input log")]
    BadMagic,
    /// The format version is newer than this decoder, or unknown.
    #[error("unsupported format version {0}")]
    UnsupportedVersion(u16),
    /// The body encoding is unknown.
    #[error("unsupported body encoding {0}")]
    UnsupportedEncoding(u8),
    /// The declared uncompressed length exceeds
    /// [`DecodeLimits::max_uncompressed`](crate::DecodeLimits::max_uncompressed).
    #[error("the declared body length {declared} is above the limit of {max}")]
    DeclaredLengthTooLarge {
        /// The declared length.
        declared: u32,
        /// The limit.
        max: u32,
    },
    /// The body is not valid compressed data, or it decompresses to more
    /// than the declared length.
    #[error("the body does not decompress within its declared length")]
    Decompress,
    /// The body decompresses to fewer bytes than declared.
    #[error("the body decompresses to {actual} bytes, not the declared {declared}")]
    LengthMismatch {
        /// The declared length.
        declared: u32,
        /// The decompressed length.
        actual: usize,
    },
    /// The data ended in the middle of a value.
    #[error("the data ends in the middle of a value")]
    Truncated,
    /// A section's length exceeds the bytes left in the body.
    #[error("section {section:?} declares {len} bytes but {remaining} remain")]
    SectionLength {
        /// The section.
        section: Section,
        /// Its declared length.
        len: u32,
        /// The bytes left after its length.
        remaining: usize,
    },
    /// A section or the body has bytes after its last value.
    #[error("unexpected bytes after the last value")]
    TrailingBytes,
    /// A varint is overlong or exceeds its type.
    #[error("a varint is overlong or out of range")]
    BadVarint,
    /// A value is out of its range or an unknown tag.
    #[error("invalid {0}")]
    InvalidValue(&'static str),
    /// A string is not UTF-8.
    #[error("a string is not UTF-8")]
    InvalidUtf8,
    /// The record has no source.
    #[error("the record has no source")]
    NoSources,
    /// More sources than [`DecodeLimits::max_sources`](crate::DecodeLimits::max_sources).
    #[error("{0} sources exceed the limit")]
    TooManySources(u64),
    /// More events than [`DecodeLimits::max_events`](crate::DecodeLimits::max_events).
    #[error("{0} events exceed the limit")]
    TooManyEvents(u64),
    /// More bindings or axes than the evaluator's limits, per source or in
    /// all.
    #[error("the record binds too many controls")]
    TooManyBindings,
    /// A list declares more elements than the bytes left could hold.
    #[error("a list of {0} elements exceeds the data")]
    CountBeyondData(u64),
    /// An event or held control names a source the record does not have.
    #[error("unknown source {0}")]
    UnknownSource(u16),
    /// The stored preset hash does not match the stored preset bytes.
    #[error("the preset hash does not match its bytes")]
    PresetHashMismatch,
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{DecodeError, Section};

    /// Every variant is a leaf: its message is the whole diagnostic.
    #[test]
    fn decode_errors_are_leaves_with_their_messages() {
        let cases = [
            (
                DecodeError::FileTooLarge { len: 9, max: 8 },
                "the file is 9 bytes, above the limit of 8",
            ),
            (DecodeError::BadMagic, "not an input log"),
            (
                DecodeError::UnsupportedVersion(3),
                "unsupported format version 3",
            ),
            (
                DecodeError::UnsupportedEncoding(4),
                "unsupported body encoding 4",
            ),
            (
                DecodeError::DeclaredLengthTooLarge {
                    declared: 9,
                    max: 8,
                },
                "the declared body length 9 is above the limit of 8",
            ),
            (
                DecodeError::Decompress,
                "the body does not decompress within its declared length",
            ),
            (
                DecodeError::LengthMismatch {
                    declared: 9,
                    actual: 8,
                },
                "the body decompresses to 8 bytes, not the declared 9",
            ),
            (
                DecodeError::Truncated,
                "the data ends in the middle of a value",
            ),
            (
                DecodeError::SectionLength {
                    section: Section::Events,
                    len: 9,
                    remaining: 8,
                },
                "section Events declares 9 bytes but 8 remain",
            ),
            (
                DecodeError::TrailingBytes,
                "unexpected bytes after the last value",
            ),
            (
                DecodeError::BadVarint,
                "a varint is overlong or out of range",
            ),
            (DecodeError::InvalidValue("tag"), "invalid tag"),
            (DecodeError::InvalidUtf8, "a string is not UTF-8"),
            (DecodeError::NoSources, "the record has no source"),
            (DecodeError::TooManySources(5), "5 sources exceed the limit"),
            (DecodeError::TooManyEvents(6), "6 events exceed the limit"),
            (
                DecodeError::TooManyBindings,
                "the record binds too many controls",
            ),
            (
                DecodeError::CountBeyondData(7),
                "a list of 7 elements exceeds the data",
            ),
            (DecodeError::UnknownSource(2), "unknown source 2"),
            (
                DecodeError::PresetHashMismatch,
                "the preset hash does not match its bytes",
            ),
        ];
        for (e, message) in cases {
            assert_eq!(e.to_string(), message);
            assert!(e.source().is_none(), "{e:?} has a source");
        }
    }
}
