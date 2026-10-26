//! Load errors and warnings.

use std::fmt;

use bms_model::ModelError;

/// Why a chart was refused. Errors that come from a line name it (1-based).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// The file is larger than [`Limits::max_file_bytes`](crate::Limits).
    FileTooLarge {
        /// Size of the file.
        bytes: usize,
    },
    /// The text has more lines than [`Limits::max_lines`](crate::Limits).
    TooManyLines {
        /// The first line past the limit.
        line: usize,
    },
    /// The chart has more objects than [`Limits::max_objects`](crate::Limits).
    TooManyObjects {
        /// The line holding the first object past the limit.
        line: usize,
    },
    /// A measure number is above [`Limits::max_measure`](crate::Limits).
    MeasureOutOfRange {
        /// The line.
        line: usize,
        /// The measure number.
        measure: u32,
    },
    /// A value is malformed or out of its range.
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
    NestingTooDeep {
        /// The line opening the first block past the limit.
        line: usize,
    },
    /// A directive whose meaning the loader cannot honor.
    Unsupported {
        /// The line.
        line: usize,
        /// The directive, such as `#LNTYPE 2`.
        what: String,
    },
    /// The resolved chart is longer than
    /// [`Limits::max_duration`](crate::Limits).
    DurationExceeded {
        /// The first measure with a time past the limit.
        measure: u16,
    },
    /// The exact time grew past [`Limits::max_time_bits`](crate::Limits).
    TimeOverflow {
        /// The measure being computed.
        measure: u16,
    },
    /// A `#WAV` name is absolute, rooted, drive-prefixed, UNC, or contains
    /// `..`.
    SoundPathRejected {
        /// The line of the `#WAV` definition.
        line: usize,
    },
    /// A `#WAV` file resolves outside the chart folder.
    SoundPathEscapes {
        /// The line of the `#WAV` definition.
        line: usize,
    },
    /// The chart folder could not be resolved.
    ChartDir {
        /// The I/O error kind.
        kind: std::io::ErrorKind,
    },
    /// The resolved chart broke a model invariant. This is a loader bug.
    Model(ModelError),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileTooLarge { bytes } => write!(f, "file of {bytes} bytes is too large"),
            Self::TooManyLines { line } => write!(f, "line {line}: too many lines"),
            Self::TooManyObjects { line } => write!(f, "line {line}: too many objects"),
            Self::MeasureOutOfRange { line, measure } => {
                write!(f, "line {line}: measure {measure} is out of range")
            }
            Self::InvalidValue { line, what, reason } => {
                write!(f, "line {line}: {what}: {reason}")
            }
            Self::NestingTooDeep { line } => {
                write!(f, "line {line}: #RANDOM and #IF blocks nest too deep")
            }
            Self::Unsupported { line, what } => write!(f, "line {line}: unsupported {what}"),
            Self::DurationExceeded { measure } => {
                write!(
                    f,
                    "measure {measure}: chart is longer than the duration limit"
                )
            }
            Self::TimeOverflow { measure } => {
                write!(
                    f,
                    "measure {measure}: exact time is larger than the size limit"
                )
            }
            Self::SoundPathRejected { line } => {
                write!(
                    f,
                    "line {line}: sound path must stay relative to the chart folder"
                )
            }
            Self::SoundPathEscapes { line } => {
                write!(
                    f,
                    "line {line}: sound file resolves outside the chart folder"
                )
            }
            Self::ChartDir { kind } => write!(f, "chart folder: {kind}"),
            Self::Model(e) => write!(f, "resolved chart is invalid: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

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
