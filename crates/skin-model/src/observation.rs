//! What a frame's draw evaluated, per element, and the display list a
//! capture reads back from it (dev-tools).
//!
//! An element's observation is filled as [`Skin::draw`](crate::Skin) passes
//! it, into capacity the batch allocated at its creation, so a play frame
//! allocates nothing. Only a capture turns it into a [`DisplayList`], on the
//! render thread, after the frame.

use bms_model::TimeUs;

use crate::{Blend, ChartText, Filter, QuadBatch, Skin, UvTable};

/// Why an element drew what it drew, at the point [`Skin::draw`](crate::Skin)
/// stopped with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It reached its draw; no quads then means its widget drew nothing.
    Sampled,
    /// Its region is hidden.
    RegionHidden,
    /// A HIDDEN+ paint while HIDDEN+ is off.
    HiddenOff,
    /// Its image choice selected nothing.
    NoChoice,
    /// Its timer is off, its track has not started or has ended, or its
    /// visibility is false.
    NotSampled,
    /// A cover paint's gate track gave no sample.
    GateNotSampled,
    /// A cover paint's trim hid it.
    TrimSuppressed,
}

/// One element's observation in one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElementObservation {
    /// The element's index in the skin.
    pub element: u32,
    /// The first quad it may have pushed.
    pub quad_start: u32,
    /// Where the draw stopped with it.
    pub outcome: Outcome,
    /// A number's evaluated value.
    pub value: Option<i64>,
    /// An image choice's selected entry.
    pub choice: Option<u32>,
    /// The time its track was sampled at.
    pub t: Option<TimeUs>,
}

/// An element in a [`DisplayList`].
#[derive(Clone, Debug, PartialEq)]
pub struct ListedElement {
    /// What the draw evaluated.
    pub observation: ElementObservation,
    /// A text element's string.
    pub text: Option<String>,
}

/// Where a quad's texels come from, by the sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuadSource {
    /// A frame of an image of a file: the file's name and the frame's
    /// rectangle in its 1x pixels.
    File {
        /// The frame's index in the uv table.
        frame: u32,
        /// The file's name, relative to the skin's folder.
        name: String,
        /// Left, top, width, height.
        rect: [u32; 4],
    },
    /// A generated image of one colour.
    Solid {
        /// The frame's index in the uv table.
        frame: u32,
        /// Straight RGBA.
        color: [u8; 4],
    },
    /// Not a frame of the sheet: a text line, or a bitmap font's glyph.
    Other,
}

/// A quad in a [`DisplayList`], in draw order.
#[derive(Clone, Debug, PartialEq)]
pub struct ListedQuad {
    /// The element that pushed it; `None` after the skin's elements, the
    /// overlay's.
    pub element: Option<u32>,
    /// Left, top, width, height on the canvas.
    pub rect: [f32; 4],
    /// The point it rotates about.
    pub pivot: [f32; 2],
    /// Radians, clockwise.
    pub angle: f32,
    /// RGBA.
    pub color: [f32; 4],
    /// Its run's blend.
    pub blend: Blend,
    /// Its run's clip, by index in the skin's clip table.
    pub clip: Option<u16>,
    /// The clip rect its run carries.
    pub clip_rect: Option<[f32; 4]>,
    /// How the atlas is sampled.
    pub filter: Filter,
    /// Its texels.
    pub source: QuadSource,
}

/// What one frame drew, in draw order.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayList {
    /// The skin's elements: a run's ledger of another count is a run error.
    pub element_count: u32,
    /// The elements' observations, in draw order.
    pub elements: Vec<ListedElement>,
    /// The quads, in draw order.
    pub quads: Vec<ListedQuad>,
    /// Quads the batch dropped.
    pub quads_dropped: u32,
    /// Observations the batch had no room for: the list misattributes then,
    /// which a capture must refuse.
    pub observations_dropped: u32,
}

impl Skin {
    /// `batch`, as this skin's [`Skin::build`] left it, as a list. `chart`
    /// gives its text elements' strings. Allocates; only a capture calls it.
    #[must_use]
    pub fn display_list(
        &self,
        batch: &QuadBatch,
        uvs: &UvTable,
        chart: &ChartText<'_>,
    ) -> DisplayList {
        let observations = batch.observations();
        let end = batch.observation_end();
        let mut quads = Vec::with_capacity(batch.quads().len());
        for run in batch.runs() {
            for i in run.start..run.end {
                let Some(q) = usize::try_from(i).ok().and_then(|i| batch.quads().get(i)) else {
                    continue;
                };
                let element = end.filter(|&e| i < e).and_then(|_| {
                    let at = observations.partition_point(|o| o.quad_start <= i);
                    at.checked_sub(1)
                        .and_then(|at| observations.get(at))
                        .map(|o| o.element)
                });
                quads.push(ListedQuad {
                    element,
                    rect: q.rect,
                    pivot: q.pivot,
                    angle: q.angle,
                    color: q.color,
                    blend: run.key.blend,
                    clip: run.key.clip.map(|c| c.0),
                    clip_rect: run.clip_rect.map(crate::ClipRect::rect),
                    filter: q.filter,
                    source: self.quad_source(uvs, q.layer, q.uv),
                });
            }
        }
        DisplayList {
            element_count: u32::try_from(self.element_count()).unwrap_or(u32::MAX),
            elements: observations
                .iter()
                .map(|&observation| ListedElement {
                    observation,
                    text: self.element_text(observation.element, chart),
                })
                .collect(),
            quads,
            quads_dropped: batch.dropped(),
            observations_dropped: batch.observations_dropped(),
        }
    }

    /// The sheet's frame holding the centre of `uv` on `layer`.
    fn quad_source(&self, uvs: &UvTable, layer: u16, uv: [f32; 4]) -> QuadSource {
        let [l, t, r, b] = uv;
        let (x, y) = (f32::midpoint(l, r), f32::midpoint(t, b));
        uvs.frames()
            .iter()
            .position(|f| {
                let [fl, ft, fr, fb] = f.uv;
                f.layer == layer && fl <= x && x <= fr && ft <= y && y <= fb
            })
            .and_then(|i| u32::try_from(i).ok())
            .and_then(|frame| self.sheet().frame_source(frame))
            .unwrap_or(QuadSource::Other)
    }
}
