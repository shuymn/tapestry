//! Load errors and warnings.

use bms_model::ModelError;

/// Why a chart was refused. Errors that come from a line name it (1-based).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    /// The file is larger than [`Limits::max_file_bytes`](crate::Limits).
    #[error("file of {bytes} bytes is too large")]
    FileTooLarge {
        /// Size of the file.
        bytes: usize,
    },
    /// The text has more lines than [`Limits::max_lines`](crate::Limits).
    #[error("line {line}: too many lines")]
    TooManyLines {
        /// The first line past the limit.
        line: usize,
    },
    /// The chart has more objects than [`Limits::max_objects`](crate::Limits).
    #[error("line {line}: too many objects")]
    TooManyObjects {
        /// The line holding the first object past the limit.
        line: usize,
    },
    /// A measure number is above [`Limits::max_measure`](crate::Limits).
    #[error("line {line}: measure {measure} is out of range")]
    MeasureOutOfRange {
        /// The line.
        line: usize,
        /// The measure number.
        measure: u32,
    },
    /// A value is malformed or out of its range.
    #[error("line {line}: {what}: {reason}")]
    InvalidValue {
        /// The line.
        line: usize,
        /// What the value is, such as `#BPM`.
        what: &'static str,
        /// Why it was refused.
        reason: &'static str,
    },
    /// `#RANDOM` and `#IF` blocks nest deeper than
    /// [`Limits::max_nesting`](crate::Limits).
    #[error("line {line}: #RANDOM and #IF blocks nest too deep")]
    NestingTooDeep {
        /// The line opening the first block past the limit.
        line: usize,
    },
    /// A directive whose meaning the loader cannot honor.
    #[error("line {line}: unsupported {what}")]
    Unsupported {
        /// The line.
        line: usize,
        /// The directive, such as `#LNTYPE 2`.
        what: String,
    },
    /// The resolved chart is longer than
    /// [`Limits::max_duration`](crate::Limits).
    #[error("measure {measure}: chart is longer than the duration limit")]
    DurationExceeded {
        /// The first measure with a time past the limit.
        measure: u16,
    },
    /// Time computation overflowed.
    #[error("measure {measure}: time computation overflowed")]
    TimeOverflow {
        /// The measure being computed.
        measure: u16,
    },
    /// A `#WAV` name is absolute, rooted, drive-prefixed, UNC, or contains
    /// `..`.
    #[error("line {line}: sound path must stay relative to the chart folder")]
    SoundPathRejected {
        /// The line of the `#WAV` definition.
        line: usize,
    },
    /// A `#WAV` file resolves outside the chart folder.
    #[error("line {line}: sound file resolves outside the chart folder")]
    SoundPathEscapes {
        /// The line of the `#WAV` definition.
        line: usize,
    },
    /// The chart folder could not be resolved.
    #[error("chart folder: {kind}")]
    ChartDir {
        /// The I/O error kind.
        kind: std::io::ErrorKind,
    },
    /// The resolved chart broke a model invariant. This is a loader bug.
    #[error("resolved chart is invalid")]
    Model(#[source] ModelError),
}

/// Something the loader skipped or assumed; the chart still loads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// The line it concerns (1-based), if any.
    pub line: Option<usize>,
    /// What happened.
    pub kind: WarningKind,
}

/// The kind of a [`Warning`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarningKind {
    /// The text had bytes invalid in the detected encoding.
    DecodeErrors,
    /// A header the loader does not know; ignored.
    UnknownHeader(String),
    /// A channel the loader does not use; its objects are ignored.
    IgnoredChannel(String),
    /// An object pair that is not a valid id; ignored.
    InvalidObject,
    /// Channel data with an odd number of characters; the last is ignored.
    OddObjectData,
    /// A malformed descriptive value (`#RANK`, `#TOTAL`, ...); ignored.
    IgnoredValue(&'static str),
    /// No `#BPM`; 130 is used.
    MissingBpm,
    /// `#PLAYER` other than 1; the notes decide whether the chart loads.
    PlayerNotOne,
    /// An object refers to an undefined `#WAV`, `#BPMxx` or `#STOPxx`; the
    /// object plays no sound or has no effect.
    UndefinedReference,
    /// A `#STOPxx` of zero; ignored.
    ZeroStop,
    /// No file was found for a `#WAV` name.
    MissingSoundFile,
    /// A long-note start without an end; dropped.
    UnpairedLongNoteStart,
    /// A long-note end without a start; dropped.
    UnpairedLongNoteEnd,
    /// A note at the same time and lane as a note that won; dropped.
    SameTimeNoteDropped,
    /// A note at or inside the span of an earlier long note in its lane;
    /// dropped.
    NoteInsideLongNoteDropped,
    /// The chart has no note to judge.
    NoPlayableNotes,
    /// `#IF` outside any `#RANDOM` block (never taken), or `#ELSE`,
    /// `#ENDIF` or `#ENDRANDOM` without its block (ignored).
    UnmatchedDirective(&'static str),
    /// An `#IF` closed by `#ENDRANDOM` or by the end of the file.
    UnclosedIf,
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use bms_model::ModelError;

    use super::LoadError;

    /// No error in the chain prints the Display of its source.
    fn assert_causes_printed_once(mut e: &(dyn Error + 'static)) {
        while let Some(source) = e.source() {
            let (outer, inner) = (e.to_string(), source.to_string());
            assert!(!outer.contains(&inner), "{outer:?} repeats {inner:?}");
            e = source;
        }
    }

    #[test]
    fn model_error_is_the_source() {
        let e = LoadError::Model(ModelError::Overflow);
        assert_eq!(e.to_string(), "resolved chart is invalid");
        let source = e.source().expect("the model error is the source");
        assert_eq!(
            source.downcast_ref::<ModelError>(),
            Some(&ModelError::Overflow)
        );
        assert!(
            source.source().is_none(),
            "the chain ends at the model error"
        );
        assert_causes_printed_once(&e);
    }

    #[test]
    fn leaf_errors_have_no_source() {
        let e = LoadError::ChartDir {
            kind: std::io::ErrorKind::NotFound,
        };
        assert_eq!(e.to_string(), "chart folder: entity not found");
        assert!(e.source().is_none());
        let e = LoadError::Unsupported {
            line: 3,
            what: "#LNTYPE 2".to_owned(),
        };
        assert_eq!(e.to_string(), "line 3: unsupported #LNTYPE 2");
        assert!(e.source().is_none());
    }
}
