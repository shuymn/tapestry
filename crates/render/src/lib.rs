//! The play screen (design.md "Rendering and Skins"): wgpu on DX12 with one
//! instanced sprite pipeline per blend over an atlas texture array, whose
//! reserved layer 0 holds an ASCII bitmap font baked in at startup and whose
//! later layers a skin's pages with their mips ([`AtlasPages`]), and the skin
//! boundary [`PlaySkin`] with its one implementation, [`SimpleSkin`]. A
//! frame's sprites are drawn in the order they were pushed, with
//! premultiplied colour, each run under its clip's scissor, projected from the
//! skin's canvas onto the target ([`Projection`]).
//!
//! [`Renderer`] draws into a borderless window's swap chain (FIFO, one frame
//! of latency, the render thread waiting on the waitable object itself) or
//! into an offscreen target that tests read back. Every pipeline is created
//! and every upload finished in [`Renderer::new`], [`Renderer::load`] and
//! [`Renderer::install_atlas`], before READY; a new target size is
//! applied between frames ([`Renderer::resize`]).
//!
//! The app's per-frame work, the skin's build into the preallocated
//! [`SpriteBatch`], the measurement overlay after it from the batch's
//! reserve, and its staging into [`Instance`]s ([`Staging`]), allocates
//! nothing. A skin may refuse a frame with the runtime model's
//! `BuildError`: nothing of it is submitted, and the renderer builds that
//! skin's attempt no more until it is prepared again.
//!
//! Menu screens draw Japanese text with [`text`] and
//! [`Renderer::menu_frame`], which the play screen never calls.
//!
//! [`measure`] holds the pure frame and latency measurement of design.md
//! "Latency Figures", and [`Renderer::frame_statistics`] reads the swap chain's
//! DXGI frame statistics it pairs to frames.

// The integration tests' fixtures, shared with the unit tests, which name
// this crate as they do.
#[cfg(test)]
extern crate self as render;
#[cfg(test)]
mod admission_tests;
#[cfg(test)]
#[path = "../tests/font_support/mod.rs"]
mod font_support;
#[cfg(test)]
#[path = "../tests/outline_support/mod.rs"]
mod outline_support;
#[cfg(test)]
mod result_tests;
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod test_common;
#[cfg(test)]
#[path = "../tests/decl_support/mod.rs"]
mod test_decl;

mod atlas;
mod decision_labels;
mod decl;
mod gpu;
pub mod measure;
mod outline;
mod overlay;
mod pace;
pub mod presentation;
mod projection;
mod renderer;
mod simple;
mod skin;
mod skin_atlas;
mod sprite;
mod staging;
pub mod text;
mod text_cache;
mod text_upload;

pub use atlas::{ATLAS_SIZE, GLYPH_H, GLYPH_W, glyph_rows, solid_uv};
pub use decision_labels::{
    CoolFrom, DECISION_TEXT_WRITE_BUDGET, DecisionMemory, FillProgress, FillStep, Gen, LABEL_SLOTS,
    LabelBook, LabelContent, LabelJob, LabelKey, LabelOutput, LabelRow, LabelScratch, LabelState,
    LabelStore, LabelWorker, Publish, RowPlan, Settled, StagedLabels, Watch, lock, row_height,
    stage_bundle_labels, stage_labels,
};
pub use decl::{
    AttemptText, BundleError, DeclSkin, Installation, LoadingPresentation, SkinBundle, pages_bytes,
    pages_texels, skin_frame,
};
pub use gpu::{AdapterChoice, Gpu};
pub use outline::{
    DefaultFont, LineBounds, LineFit, LineLayout, MAX_DEFAULT_FONT_BYTES, MAX_LINE_H, MAX_LINE_W,
    MAX_SHAPED_GLYPHS, OutlineError, OutlineText, PlacedGlyph, RASTER_MARGIN, RasterError,
    TextNote, TextResidency,
};
pub use overlay::{OVERLAY_RUNS, OVERLAY_SPRITES};
pub use projection::{ClipError, ClipTable, MAX_CLIPS, Projection};
#[cfg(any(test, feature = "dev-tools"))]
pub use renderer::UploadTiming;
#[cfg(feature = "dev-tools")]
pub use renderer::take_build_start;
pub use renderer::{
    Admission, ChartOutcome, ChartTarget, CommitError, FrameTiming, LoadingTextError,
    LoadingTextKey, MAX_SPRITES, MemoryWitness, PreparedToken, RESULT_STRIP_SPRITES, RenderError,
    Renderer, ResultChartTarget, SHARED_TEXELS, STRIP_SPRITES, Target, VM_TEXELS, letterbox,
};
pub use simple::{
    RESULT_CELL_COLUMN, RESULT_GAUGE_ROW, SimpleSkin, layout, result_graphs, result_screen,
    text_screen,
};
pub use skin::{AttemptInput, FrameInput, LoadingFrame, PlaySkin, SkinError, cover_settings};
pub use skin_atlas::{AtlasPages, PageLevels, RESERVED_LAYERS, UploadError, atlas_limits};
pub use sprite::{Blend, FLAG_BASE, FLAG_NEAREST, Instance, Run, Sprite, SpriteBatch};
pub use staging::{Scissor, Staging};
pub use text_cache::{MAX_ALIASES, MAX_ROUNDS, MAX_TEXT_ENTRIES, TextCounts};
pub use text_upload::{MAX_RECT_H, MAX_RECT_W, MAX_TEXT_MASK_BYTES, TextReport, text_upload_bound};
/// The adapter facts [`Gpu::adapter`] gives, for callers without `wgpu`
/// (dev-tools: a scenario's adapter line and admission).
#[cfg(feature = "dev-tools")]
pub use wgpu::{AdapterInfo, DeviceType};
