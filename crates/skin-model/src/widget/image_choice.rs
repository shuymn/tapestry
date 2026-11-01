//! Expression-selected images: an element showing one entry of a table
//! of image spans, chosen each frame by an integer program.
//!
//! The selector is evaluated once per frame against the filled play
//! state: a negative value draws nothing, a value below the table's
//! length draws that entry, and any other value draws entry 0. A hole
//! draws nothing; nothing searches for another entry, wraps or clamps.
//! Each entry animates its own frames on its own cycle and timer, so
//! switching away and back never restarts a clock, and may replace the
//! element's track, sampled on the element's timer, and its native size.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use crate::expr::{Program, State};
use crate::image::CycleDef;
use crate::place::Native;
use crate::track::Track;

use super::ImageRef;

/// The most entries, holes and repeats included, across one skin's image
/// choice tables; each table holds at most this many.
pub const MAX_IMAGE_CHOICE_REFS: usize = 65_536;

/// A run of an image's frames: `frames` frames from `first`, in the
/// image's row-major order, which may cross a grid row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageSpan {
    /// An index into the sheet's images.
    pub image: u32,
    pub first: u16,
    /// At least 1; `first + frames` at most the image's frame count.
    pub frames: u16,
}

/// One entry of an image choice table.
#[derive(Clone, Debug)]
pub struct ImageChoicePart {
    pub span: ImageSpan,
    /// The span's whole animation; `None` shows its first frame. The
    /// image's own cycle is not used.
    pub cycle: Option<CycleDef>,
    /// Replaces the element's whole track, sampled on the element's
    /// timer; `None` keeps the element's.
    pub track: Option<Track>,
    /// The size each quad keeps in target pixels while this entry shows
    /// (the element's [`Place::native`](crate::Place::native) stays
    /// `None`).
    pub native: Option<Native>,
}

/// A table of images an element chooses from by `value`.
#[derive(Clone, Debug)]
pub struct ImageChoiceDef {
    /// An integer program against the play schema.
    pub value: Program,
    /// 1 to [`MAX_IMAGE_CHOICE_REFS`] entries; `None` is a hole.
    pub choices: Vec<Option<ImageChoicePart>>,
}

/// Why an image choice table was refused; `choice` is an entry index.
#[derive(Debug, thiserror::Error)]
pub enum ImageChoiceError {
    #[error("the selector is not an int")]
    NotInt,
    #[error("the table has no entries")]
    Empty,
    #[error("entry {choice}'s image does not exist")]
    UnknownImage { choice: usize },
    #[error("entry {choice}'s frames are empty or outside its image")]
    Span { choice: usize },
    #[error("entry {choice}'s cycle period is outside 1 µs to the longest key time")]
    Cycle { choice: usize },
    #[error("entry {choice}'s cycle timer does not exist")]
    UnknownTimer { choice: usize },
    #[error("entry {choice}'s track's keys leave the model's coordinates")]
    Coordinates { choice: usize },
}

impl ImageChoiceDef {
    /// The present entries' override tracks.
    pub(crate) fn tracks(&self) -> impl Iterator<Item = &Track> + '_ {
        self.choices
            .iter()
            .flatten()
            .filter_map(|c| c.track.as_ref())
    }

    /// Whether a present entry samples the element's own track.
    pub(crate) fn uses_element_track(&self) -> bool {
        self.choices.iter().flatten().any(|c| c.track.is_none())
    }
}

/// A loaded entry: its frames in the uv table, its own track and native
/// size.
#[derive(Clone, Debug)]
pub(crate) struct ChoicePart {
    pub(crate) image: ImageRef,
    pub(crate) track: Option<Track>,
    pub(crate) native: Option<Native>,
}

/// A loaded table.
#[derive(Clone, Debug)]
pub(crate) struct ImageChoice {
    pub(crate) value: Program,
    pub(crate) parts: Box<[Option<ChoicePart>]>,
}

impl ImageChoice {
    /// The entry `state` selects: none for a negative value or a hole,
    /// entry 0 for a value past the table.
    pub(crate) fn select(&self, state: &State<'_>) -> Option<&ChoicePart> {
        let v = usize::try_from(self.value.eval(state)).ok()?;
        self.parts.get(v).or_else(|| self.parts.first())?.as_ref()
    }
}
