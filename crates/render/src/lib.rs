//! The play screen (design.md "Rendering and Skins"): wgpu on DX12 with one
//! instanced sprite pipeline over a texture atlas, an ASCII bitmap font baked
//! into that atlas, and the skin boundary [`PlaySkin`] with its one
//! implementation, [`SimpleSkin`].
//!
//! [`Renderer`] draws into a borderless window's swap chain (FIFO, one frame
//! of latency, the render thread waiting on the waitable object itself) or
//! into an offscreen target that tests read back. Every pipeline is created
//! and every upload finished in [`Renderer::new`] and [`Renderer::load`],
//! before the countdown.
//!
//! The app's per-frame work, the skin's build into the preallocated
//! [`SpriteBatch`] and its staging into instances, allocates nothing.
//!
//! Menu screens draw Japanese text with [`text`] and
//! [`Renderer::menu_frame`], which the play screen never calls.
//!
//! [`measure`] holds the pure frame and latency measurement of design.md
//! "Latency Figures", and [`Renderer::frame_statistics`] reads the swap chain's
//! DXGI frame statistics it pairs to frames.

mod atlas;
mod gpu;
pub mod measure;
mod renderer;
mod simple;
mod skin;
mod sprite;
pub mod text;

pub use atlas::{ATLAS_SIZE, GLYPH_H, GLYPH_W, glyph_rows};
pub use gpu::{AdapterChoice, Gpu};
pub use renderer::{
    FrameTiming, MAX_SPRITES, RenderError, Renderer, Target, build_frame, letterbox,
};
pub use simple::{SimpleSkin, layout, text_screen};
pub use skin::{FrameInput, PlaySkin, SkinError};
pub use sprite::{Blend, Sprite, SpriteBatch};
