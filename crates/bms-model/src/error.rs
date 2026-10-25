//! Errors for building and validating a chart.

use std::fmt;

use crate::{Lane, SoundId, TimeUs};

/// Why a chart or a scroll map was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// A BPM was zero or negative.
    InvalidBpm {
        /// Time of the change (`TimeUs(0)` for the initial BPM).
        at: TimeUs,
        /// The rejected BPM, in millionths.
        bpm_micro: i64,
    },
    /// A STOP had a zero or negative length.
    InvalidStop {
        /// Start of the STOP.
        at: TimeUs,
        /// The rejected length.
        len: TimeUs,
    },
    /// A scroll change was placed before time 0.
    NegativeTime {
        /// Time of the change.
        at: TimeUs,
    },
    /// A BPM change or STOP started strictly inside another STOP.
    ChangeDuringStop {
        /// Time of the change.
        at: TimeUs,
    },
    /// A time or position left the `i64` range.
    Overflow,
    /// A note came earlier than the note before it in its lane.
    Unsorted {
        /// Lane of the note.
        lane: Lane,
        /// Index of the note in its lane.
        index: usize,
    },
    /// A long note did not end after it started.
    LongEndNotAfterStart {
        /// Lane of the note.
        lane: Lane,
        /// Index of the note in its lane.
        index: usize,
    },
    /// A note started at or before the end of the previous note in its lane.
    Overlap {
        /// Lane of the note.
        lane: Lane,
        /// Index of the later note in its lane.
        index: usize,
    },
    /// A BGM event came earlier than the event before it.
    BgmUnsorted {
        /// Index of the event.
        index: usize,
    },
    /// A bar line came earlier than the bar line before it.
    BarLinesUnsorted {
        /// Index of the bar line.
        index: usize,
    },
    /// A note or BGM event named a sound outside the sound table.
    UnknownSound {
        /// The missing sound.
        sound: SoundId,
    },
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::InvalidBpm { at, bpm_micro } => {
                write!(f, "BPM {bpm_micro}/1000000 at {} µs is not positive", at.0)
            }
            Self::InvalidStop { at, len } => {
                write!(f, "STOP at {} µs has non-positive length {}", at.0, len.0)
            }
            Self::NegativeTime { at } => write!(f, "scroll change at negative time {} µs", at.0),
            Self::ChangeDuringStop { at } => {
                write!(f, "scroll change at {} µs falls inside a STOP", at.0)
            }
            Self::Overflow => f.write_str("time or position overflowed i64"),
            Self::Unsorted { lane, index } => {
                write!(
                    f,
                    "lane {} note {index} is earlier than the note before it",
                    lane.index()
                )
            }
            Self::LongEndNotAfterStart { lane, index } => {
                write!(
                    f,
                    "lane {} long note {index} does not end after it starts",
                    lane.index()
                )
            }
            Self::Overlap { lane, index } => {
                write!(
                    f,
                    "lane {} note {index} overlaps the note before it",
                    lane.index()
                )
            }
            Self::BgmUnsorted { index } => {
                write!(f, "BGM event {index} is earlier than the event before it")
            }
            Self::BarLinesUnsorted { index } => {
                write!(f, "bar line {index} is earlier than the bar line before it")
            }
            Self::UnknownSound { sound } => {
                write!(f, "sound {} is not in the sound table", sound.0)
            }
        }
    }
}

impl std::error::Error for ModelError {}
