//! What an import changed, dropped or ignored, and where.
//!
//! Notes are kept as a byte span while the import runs and located once
//! at the end: the spans are sorted and each definition file is walked
//! forward once, so locating many notes costs one pass per file. Only
//! the first [`MAX_NOTES`] are kept; later ones are counted.

use std::fmt;

use crate::limits::MAX_NOTES;
use crate::tree::{Arena, BOM};

/// A place in one of [`Report::sources`]: its index and a 1-based line
/// and column (in characters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteLoc {
    pub source: u16,
    pub line: u32,
    pub col: u32,
}

/// How a note changed the skin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// An element the source draws was left out.
    Dropped,
    /// The element is kept and drawn differently from the source, by an
    /// owner decision (D1–D11).
    Deviation,
    /// Something the source does not draw either, or a field with no
    /// effect, was left out.
    Ignored,
}

/// A source object kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    Image,
    ImageSet,
    Value,
    FloatValue,
    Text,
    Slider,
    Graph,
    GaugeGraph,
    JudgeGraph,
    BpmGraph,
    HitErrorVisualizer,
    TimingVisualizer,
    TimingDistribution,
    Gauge,
    Note,
    HiddenCover,
    LiftCover,
    Practice,
    Bga,
    Judge,
    PmChara,
    /// A destination with a negative numeric id: a player-drawn image.
    System,
    /// An image whose source is a movie.
    Movie,
}

/// The kind of a property a field names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyKind {
    Integer,
    String,
    Boolean,
}

/// Why a track was not imported (design.md, the S6b track rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackWhy {
    /// Keys at one time with different values.
    EqualTime,
    /// The first key is before 0.
    NegativeStart,
    /// After its last key the source shows the first key.
    FallbackToFirst,
    /// The loop leaves a periodic gap before the first key.
    Gap,
    /// The loop wraps into negative times.
    NegativeLoop,
    /// More than the model's keys, or a key after its last time.
    Limits,
    /// The source never draws it.
    NeverDrawn,
}

/// Why a timer was not imported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerWhy {
    /// A timer id the importer has no model timer for.
    Unsupported(i32),
    /// A timer nothing ever sets: never drawn, or frame 0 forever.
    Unset(i32),
    /// A script-valued timer.
    Script,
    /// A custom timer that is passive, a chain, a script or an alias of an
    /// unsupported id.
    Custom(i32),
    /// A custom timer declared in the built-in range: no effect.
    CustomBuiltin(i32),
    /// A play-critical element with no timer whose output depends on when
    /// the state started (D3).
    Untimed,
    /// A frame cycle the model cannot run (negative or too long).
    Cycle,
}

/// An owner-decided deviation from the source (design.md, the S6b
/// deviation table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviationId {
    D1,
    D2,
    D3,
    D4,
    D5,
    D6,
    D7,
    D8,
    /// A lane approximation (owner decision 12).
    D9(LaneApprox),
    /// The gauge motion's phase, random sequence and flicker curve.
    D10,
    /// HIDDEN+ is never drawn (owner decision 15).
    D11,
}

/// How the lanes, SUDDEN+ and LIFT are drawn differently from the source
/// (owner decision 12, items a–l).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneApprox {
    /// a: notes and long-note bodies stop at the top of the lanes.
    TopCrop,
    /// e: `dst2` is ignored.
    Dst2,
    /// f: `expansionrate` is ignored.
    Expansion,
    /// h: a long note within one note height of its end draws no body
    /// (the source draws one of negative height).
    LnBody,
    /// i: the part of SUDDEN+ above the lanes is never drawn.
    CoverAbove,
    /// j: bar lines are drawn across the lanes at note centres, white and
    /// opaque, until they leave the lanes' bottom.
    BarLine,
    /// k: what the skin draws between the notes and SUDDEN+ or LIFT is
    /// drawn above them.
    CoverOrder,
}

/// What a note is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteKind {
    /// An object kind the importer does not import.
    Unsupported {
        object: Object,
    },
    /// A property id or name the importer has no model value for; no id
    /// for a script or an unknown name.
    UnsupportedProperty {
        kind: PropertyKind,
        id: Option<i32>,
    },
    /// A field value the importer cannot reproduce.
    UnsupportedField {
        field: &'static str,
    },
    /// A condition id neither built in nor declared: never drawn.
    UndefinedOp(i32),
    /// A destination naming no object: the source skips it.
    UndefinedObject,
    Blend(i32),
    Stretch(i32),
    /// An offset the play sets at run time.
    RuntimeOffset(i32),
    /// An alpha offset on animated colour, or of mixed sign.
    OffsetAlpha,
    Track(TrackWhy),
    Timer(TimerWhy),
    Deviation(DeviationId),
    /// A file that is missing or failed to load, or a choice with no
    /// candidate.
    MissingAsset,
    /// A field with no effect, or a value the source does not draw.
    Ignored {
        field: &'static str,
    },
}

impl fmt::Display for NoteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { object } => write!(f, "an unsupported {object:?}"),
            Self::UnsupportedProperty { kind, id: Some(id) } => {
                write!(f, "an unsupported {kind:?} property {id}")
            }
            Self::UnsupportedProperty { kind, id: None } => {
                write!(f, "an unsupported or script {kind:?} property")
            }
            Self::UnsupportedField { field } => write!(f, "an unsupported `{field}`"),
            Self::UndefinedOp(id) => write!(f, "an undefined condition {id}"),
            Self::UndefinedObject => f.write_str("no object of this id"),
            Self::Blend(b) => write!(f, "an unsupported blend {b}"),
            Self::Stretch(s) => write!(f, "an unsupported stretch {s}"),
            Self::RuntimeOffset(id) => write!(f, "a run-time offset {id}"),
            Self::OffsetAlpha => f.write_str("an alpha offset on animated colour or of mixed sign"),
            Self::Track(why) => write!(f, "an unsupported track ({why:?})"),
            Self::Timer(why) => write!(f, "an unsupported timer ({why:?})"),
            Self::Deviation(d) => write!(f, "deviation {d:?}"),
            Self::MissingAsset => f.write_str("a missing or unreadable file"),
            Self::Ignored { field } => write!(f, "`{field}` ignored"),
        }
    }
}

/// One report entry; `at` is `None` for a configuration entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note {
    pub at: Option<NoteLoc>,
    pub severity: Severity,
    pub kind: NoteKind,
}

/// How a file choice was decided (owner decision 2, D8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// The configured candidate.
    Configured,
    /// The default by name or stem, ignoring ASCII case.
    Default,
    /// The first candidate, sorted.
    First,
    /// No candidate: its files are missing.
    None,
}

/// A file choice's outcome. `choice` and `candidate` index the `files`
/// of the [`Header`](crate::Header) passed to [`import`](crate::import)
/// and its candidates: [`Imported`](crate::Imported) does not keep the
/// header, so the caller reads them against that same header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileChoice {
    pub choice: u16,
    pub candidate: Option<u16>,
    pub rule: Rule,
    /// More than one candidate matched the default.
    pub ambiguous: bool,
}

/// The importer's counters as charged. A dropped element's charges stay,
/// so these are what the import used against each budget, not what the
/// skin holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub elements: u32,
    pub keys: u32,
    pub ops: u32,
    pub whens: u32,
    pub images: u32,
    pub frames: u32,
    pub file_attempts: u32,
    pub files: u32,
    pub fonts: u32,
    pub texts: u32,
    pub widget_quads: u32,
    pub notes: u32,
}

/// What an import changed, dropped or ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The definition files read: the entry file, then the includes.
    pub sources: Vec<String>,
    pub files: Vec<FileChoice>,
    pub notes: Vec<Note>,
    /// Notes past [`MAX_NOTES`], counted only.
    pub suppressed: u32,
    pub usage: Usage,
}

/// A byte offset in one definition file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Span {
    pub(crate) file: u16,
    pub(crate) offset: u32,
}

impl Span {
    pub(crate) fn of(arena: &Arena, id: u32) -> Self {
        let n = arena.node(id);
        Self {
            file: n.file,
            offset: n.start,
        }
    }
}

/// The notes of a running import, unlocated.
#[derive(Debug, Default)]
pub(crate) struct Notes {
    pub(crate) list: Vec<(Option<Span>, Severity, NoteKind)>,
    pub(crate) suppressed: u32,
}

impl Notes {
    /// Adds a note, or counts it past [`MAX_NOTES`].
    pub(crate) fn push(&mut self, at: Option<Span>, severity: Severity, kind: NoteKind) {
        if self.list.len() < MAX_NOTES {
            self.list.push((at, severity, kind));
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The bytes the location pass has walked on this thread.
    pub(crate) static WALKED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Locates every note: the spans sorted, each file walked forward once.
pub(crate) fn locate(arena: &Arena, notes: &[(Option<Span>, Severity, NoteKind)]) -> Vec<Note> {
    let mut order: Vec<(Span, u32)> = notes
        .iter()
        .zip(0_u32..)
        .filter_map(|((at, ..), i)| at.map(|s| (s, i)))
        .collect();
    order.sort_unstable();
    let mut locs: Vec<Option<NoteLoc>> = vec![None; notes.len()];
    let mut cursor: Option<(u16, usize, u32, u32)> = None;
    for (span, i) in order {
        let Some(text) = arena.files.get(usize::from(span.file)) else {
            continue;
        };
        let bytes = &text.bytes;
        let (mut pos, mut line, mut col) = match cursor {
            Some((file, pos, line, col)) if file == span.file => (pos, line, col),
            _ => {
                let start = if bytes.starts_with(BOM) { BOM.len() } else { 0 };
                (start, 1, 1)
            }
        };
        let end = usize::try_from(span.offset)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let walked = bytes.get(pos..end).unwrap_or_default();
        #[cfg(test)]
        WALKED.with(|w| w.set(w.get() + walked.len() as u64));
        for &b in walked {
            if b == b'\n' {
                line += 1;
                col = 1;
            } else if b & 0xC0 != 0x80 {
                col += 1;
            }
        }
        pos = pos.max(end);
        cursor = Some((span.file, pos, line, col));
        if let Some(slot) = usize::try_from(i).ok().and_then(|i| locs.get_mut(i)) {
            *slot = Some(NoteLoc {
                source: span.file,
                line,
                col,
            });
        }
    }
    notes
        .iter()
        .zip(locs)
        .map(|(&(_, severity, kind), at)| Note { at, severity, kind })
        .collect()
}
