//! The resolved chart: notes per lane, BGM, sounds and scroll, in integer µs.

use std::path::PathBuf;

use crate::{ModelError, ScrollMap, TimeUs};

/// Number of lanes: the scratch and seven keys.
pub const LANE_COUNT: usize = 8;

/// A lane: 0 is the scratch, 1-7 are the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lane(u8);

impl Lane {
    /// The scratch lane.
    pub const SCRATCH: Self = Self(0);

    /// Returns the lane with `index`, or `None` if it is not below
    /// [`LANE_COUNT`].
    #[must_use]
    pub const fn new(index: u8) -> Option<Self> {
        if (index as usize) < LANE_COUNT {
            Some(Self(index))
        } else {
            None
        }
    }

    /// The lane's index into [`Chart::lanes`].
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Every lane, scratch first.
    pub fn all() -> impl Iterator<Item = Self> {
        (0..LANE_COUNT as u8).map(Self)
    }
}

/// An index into [`Chart::sounds`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SoundId(pub u16);

/// A note in a lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note {
    /// Time of the note (the start, for a long note).
    pub time: TimeUs,
    /// Sound played when the note is hit, if any.
    pub sound: Option<SoundId>,
    /// What kind of note this is.
    pub kind: NoteKind,
}

impl Note {
    /// The last time the note occupies its lane: the end of a long note,
    /// otherwise its time.
    #[must_use]
    pub const fn end(&self) -> TimeUs {
        match self.kind {
            NoteKind::Long { end, .. } => end,
            NoteKind::Tap | NoteKind::Mine { .. } => self.time,
        }
    }
}

/// The kind of a note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteKind {
    /// A note hit once.
    Tap,
    /// A long note held from its time to `end`.
    Long {
        /// Time the long note ends.
        end: TimeUs,
        /// Sound played at the end, if any.
        end_sound: Option<SoundId>,
    },
    /// A mine, to be avoided.
    Mine {
        /// Damage as written in the chart; the rule preset interprets it.
        damage: i32,
    },
}

/// A sound played automatically, not tied to a lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BgmEvent {
    /// Time the sound starts.
    pub time: TimeUs,
    /// The sound.
    pub sound: SoundId,
}

/// An entry of the sound table (`#WAV`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct SoundRef {
    /// The file name as written in the chart.
    pub name: String,
    /// The resolved file, confined to the chart folder, or `None` while
    /// unresolved or when no file was found.
    pub path: Option<PathBuf>,
}

/// The judgment rank a chart declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JudgeRank {
    /// `#RANK`, as written.
    Rank(i32),
    /// `#DEFEXRANK`, in thousandths of a percent.
    DefExRankMilli(i64),
}

/// The long-note judgment mode a chart declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LnMode {
    /// Judged at the start only.
    Ln,
    /// Judged at the start and the end.
    Cn,
    /// Judged at the start and the end, with the gauge changing while held.
    Hcn,
}

/// Header values of a chart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChartMeta {
    /// `#TITLE`.
    pub title: String,
    /// `#RANK` or `#DEFEXRANK`, if declared.
    pub rank: Option<JudgeRank>,
    /// `#TOTAL` in thousandths, if declared.
    pub total_milli: Option<i64>,
    /// The long-note judgment mode, if declared.
    pub ln_mode: Option<LnMode>,
}

/// What identifies the chart a play used. The loader fills it; a chart built
/// in code leaves it at the default (all zero, loader version 0).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ChartIdentity {
    /// SHA-256 of the file bytes.
    pub sha256: [u8; 32],
    /// MD5 of the file bytes.
    pub md5: [u8; 16],
    /// Hash of the resolved objects and the sound names as written.
    pub resolved_hash: [u8; 32],
    /// Version of the loader rules that resolved the chart.
    pub loader_version: u32,
    /// The value chosen by each `#RANDOM`, in order.
    pub random_results: Vec<u32>,
}

/// A fully resolved chart in integer microseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chart {
    /// Notes per lane, indexed by [`Lane::index`], each sorted by time.
    pub lanes: [Vec<Note>; LANE_COUNT],
    /// BGM events sorted by time.
    pub bgm: Vec<BgmEvent>,
    /// The sound table, indexed by [`SoundId`].
    pub sounds: Vec<SoundRef>,
    /// Times of the bar lines, sorted.
    pub bar_lines: Vec<TimeUs>,
    /// Display position over time.
    pub scroll: ScrollMap,
    /// Header values.
    pub meta: ChartMeta,
    /// What identifies the chart.
    pub identity: ChartIdentity,
}

impl Chart {
    /// Checks the invariants consumers rely on.
    ///
    /// # Errors
    ///
    /// Returns the first violation: a lane out of time order, a long note
    /// that does not end after it starts, a note starting at or before the
    /// end of the previous note in its lane, BGM out of time order, or a
    /// sound outside the sound table.
    pub fn validate(&self) -> Result<(), ModelError> {
        for lane in Lane::all() {
            let notes = &self.lanes[lane.index()];
            for (index, note) in notes.iter().enumerate() {
                if matches!(note.kind, NoteKind::Long { end, .. } if end <= note.time) {
                    return Err(ModelError::LongEndNotAfterStart { lane, index });
                }
                if let Some(prev) = index.checked_sub(1).map(|i| notes[i]) {
                    if note.time < prev.time {
                        return Err(ModelError::Unsorted { lane, index });
                    }
                    if note.time <= prev.end() {
                        return Err(ModelError::Overlap { lane, index });
                    }
                }
                let end_sound = match note.kind {
                    NoteKind::Long { end_sound, .. } => end_sound,
                    NoteKind::Tap | NoteKind::Mine { .. } => None,
                };
                for sound in [note.sound, end_sound].into_iter().flatten() {
                    self.check_sound(sound)?;
                }
            }
        }
        for (index, event) in self.bgm.iter().enumerate() {
            if index > 0 && event.time < self.bgm[index - 1].time {
                return Err(ModelError::BgmUnsorted { index });
            }
            self.check_sound(event.sound)?;
        }
        Ok(())
    }

    fn check_sound(&self, sound: SoundId) -> Result<(), ModelError> {
        if usize::from(sound.0) < self.sounds.len() {
            Ok(())
        } else {
            Err(ModelError::UnknownSound { sound })
        }
    }
}
