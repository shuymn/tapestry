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
#[path = "../tests/common/mod.rs"]
mod test_common;
#[cfg(test)]
#[path = "../tests/decl_support/mod.rs"]
mod test_decl;

mod atlas;
mod decl;
mod gpu;
pub mod measure;
mod overlay;
mod pace;
mod projection;
mod renderer;
mod simple;
mod skin;
mod skin_atlas;
mod sprite;
mod staging;
pub mod text;

pub use atlas::{ATLAS_SIZE, GLYPH_H, GLYPH_W, glyph_rows, solid_uv};
pub use decl::{
    AttemptText, BundleError, DeclSkin, Installation, LoadingPresentation, SkinBundle, skin_frame,
};
pub use gpu::{AdapterChoice, Gpu};
pub use overlay::{OVERLAY_RUNS, OVERLAY_SPRITES};
pub use projection::{ClipError, ClipTable, MAX_CLIPS, Projection};
pub use renderer::{
    CommitError, FrameTiming, MAX_SPRITES, MemoryWitness, PreparedToken, RenderError, Renderer,
    Target, letterbox,
};
pub use simple::{
    RESULT_CELL_COLUMN, RESULT_GAUGE_ROW, SimpleSkin, layout, result_graphs, result_screen,
    text_screen,
};
pub use skin::{AttemptInput, FrameInput, LoadingFrame, PlaySkin, SkinError};
pub use skin_atlas::{AtlasPages, PageLevels, RESERVED_LAYERS, UploadError, atlas_limits};
pub use sprite::{Blend, FLAG_NEAREST, Instance, Run, Sprite, SpriteBatch};
pub use staging::{Scissor, Staging};
