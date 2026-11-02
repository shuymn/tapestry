//! Errors for building and validating a chart.

use std::fmt;

use crate::{Lane, Position, SoundId, TimeUs, TimelineError, TimelineId};

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
    /// A time left the `i64` range.
    Overflow,
    /// A display position or scroll segment left the map's domain or
    /// `i128`.
    PositionOverflow,
    /// The scroll map's initial rate was zero or negative.
    InitialRateNotPositive,
    /// A scroll segment broke the map's shape: the first must start at time
    /// 0 at position 0, starts increase strictly, slopes are not negative,
    /// and each segment reaches the next anchor without passing it.
    ScrollNotMonotone {
        /// Index of the segment.
        index: usize,
    },
    /// A note's display position came before the end position of the note
    /// before it in its lane, or a long note's end position before its
    /// head's.
    PositionsUnsorted {
        /// Lane of the note.
        lane: Lane,
        /// Index of the note in its lane.
        index: usize,
    },
    /// A note came earlier than the note before it in its lane.
    Unsorted {
        /// Lane of the note.
        lane: Lane,
        /// Index of the note in its lane.
        index: usize,
    },
    /// A long note ended before it started.
    LongEndBeforeStart {
        /// Lane of the note.
        lane: Lane,
        /// Index of the note in its lane.
        index: usize,
    },
    /// A note started before the end of the previous note in its lane.
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
    /// A bar line came before the bar line before it.
    BarLinesUnsorted {
        /// Index of the bar line.
        index: usize,
    },
    /// A note or BGM event named a sound outside the sound table.
    UnknownSound {
        /// The missing sound.
        sound: SoundId,
    },
    /// The chart's timeline was refused.
    Timeline(TimelineError),
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
            Self::Overflow => f.write_str("time overflowed i64"),
            Self::PositionOverflow => f.write_str("display position is out of range"),
            Self::InitialRateNotPositive => f.write_str("initial scroll rate is not positive"),
            Self::ScrollNotMonotone { index } => {
                write!(f, "scroll segment {index} breaks the map's order")
            }
            Self::PositionsUnsorted { lane, index } => {
                write!(
                    f,
                    "lane {} note {index} is displayed before the note before it ends",
                    lane.index()
                )
            }
            Self::Unsorted { lane, index } => {
                write!(
                    f,
                    "lane {} note {index} is earlier than the note before it",
                    lane.index()
                )
            }
            Self::LongEndBeforeStart { lane, index } => {
                write!(
                    f,
                    "lane {} long note {index} ends before it starts",
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
                write!(f, "bar line {index} is before the bar line before it")
            }
            Self::UnknownSound { sound } => {
                write!(f, "sound {} is not in the sound table", sound.0)
            }
            Self::Timeline(e) => write!(f, "the timeline is invalid: {e}"),
        }
    }
}

impl std::error::Error for ModelError {}

/// Which endpoint of a note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndpointPart {
    /// The head: a tap, a mine, or a long note's start.
    Head,
    /// A long note's end.
    End,
}

/// A note endpoint, in chart coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EndpointLocation {
    /// The note's lane.
    pub lane: Lane,
    /// The note's index in its lane.
    pub index: usize,
    /// Which endpoint.
    pub part: EndpointPart,
    /// The timeline row it names.
    pub timeline_id: TimelineId,
}

impl fmt::Display for EndpointLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let part = match self.part {
            EndpointPart::Head => "head",
            EndpointPart::End => "end",
        };
        write!(
            f,
            "lane {} note {} {part} (timeline row {})",
            self.lane.index(),
            self.index,
            self.timeline_id.0
        )
    }
}

/// Why a chart's presentation was refused (design.md "Chart Model",
/// "Timeline"): a broken link between its notes, bar lines and timeline
/// ([`Chart::validate_presentation_links`]), or two note endpoints at one
/// playback µs at different positions ([`Chart::validate_presentation`]).
///
/// [`Chart::validate_presentation_links`]: crate::Chart::validate_presentation_links
/// [`Chart::validate_presentation`]: crate::Chart::validate_presentation
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentationError {
    /// An endpoint names a row the timeline does not hold, or one whose
    /// time or position is not the endpoint's.
    Link {
        /// The endpoint.
        endpoint: EndpointLocation,
    },
    /// A tap's or a mine's head and end name different rows.
    SplitAnchors {
        /// Lane of the note.
        lane: Lane,
        /// Index of the note in its lane.
        index: usize,
    },
    /// A long note's end row comes before its head row.
    ReversedAnchors {
        /// Lane of the note.
        lane: Lane,
        /// Index of the note in its lane.
        index: usize,
    },
    /// A note's head row comes before the end row of the note before it in
    /// its lane.
    LaneOrder {
        /// Lane of the note.
        lane: Lane,
        /// Index of the later note in its lane.
        index: usize,
    },
    /// The timeline names another number of bar lines than the chart has.
    BarCount {
        /// Bar lines in the chart.
        bars: usize,
        /// Bar rows in the timeline.
        rows: usize,
    },
    /// A bar line's row is not at its position.
    BarLink {
        /// Index of the bar line.
        index: usize,
    },
    /// Two note endpoints sit at one playback µs at different positions:
    /// no single playhead meets both.
    EndpointConflict {
        /// Their common playback µs.
        at: TimeUs,
        /// The first endpoint of that µs in identity order.
        first: EndpointLocation,
        /// The first one at another position.
        second: EndpointLocation,
        /// `first`'s display position.
        first_pos: Position,
        /// `second`'s display position.
        second_pos: Position,
    },
}

impl fmt::Display for PresentationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Link { endpoint } => write!(f, "{endpoint} is not at its timeline row"),
            Self::SplitAnchors { lane, index } => write!(
                f,
                "lane {} note {index} names two timeline rows",
                lane.index()
            ),
            Self::ReversedAnchors { lane, index } => write!(
                f,
                "lane {} long note {index} ends on a timeline row before its head's",
                lane.index()
            ),
            Self::LaneOrder { lane, index } => write!(
                f,
                "lane {} note {index} starts on a timeline row before the previous note ends",
                lane.index()
            ),
            Self::BarCount { bars, rows } => {
                write!(f, "{bars} bar lines but {rows} bar rows in the timeline")
            }
            Self::BarLink { index } => write!(f, "bar line {index} is not at its timeline row"),
            Self::EndpointConflict {
                at,
                first,
                second,
                first_pos,
                second_pos,
            } => write!(
                f,
                "{first} and {second} both sound at {} µs but sit at different positions \
                 ({} and {} in 2^-80 beat)",
                at.0, first_pos.0, second_pos.0
            ),
        }
    }
}

impl std::error::Error for PresentationError {}
