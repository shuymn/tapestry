//! The play screen (D9): wgpu on DX12 with one instanced sprite pipeline
//! over a texture atlas, an ASCII bitmap font baked into that atlas, and the
//! skin boundary [`PlaySkin`] with its one implementation, [`SimpleSkin`].
//!
//! [`Renderer`] draws into a borderless window's swap chain (FIFO, one frame
//! of latency, the render thread waiting on the waitable object itself) or
//! into an offscreen target that tests read back. Every pipeline and the
//! atlas upload happen in [`Renderer::new`], before the countdown.
//!
//! Per-frame work allocates nothing in this crate's own code. Calls into
//! wgpu run inside a library scope ([`in_library_call`]), so a counting
//! allocator can tell the app's allocations from the library's.

mod atlas;
mod gpu;
mod renderer;
mod simple;
mod skin;
mod sprite;

use std::cell::Cell;

pub use atlas::{ATLAS_SIZE, GLYPH_H, GLYPH_W, glyph_rows};
pub use gpu::{AdapterChoice, Gpu};
pub use renderer::{FrameTiming, RenderError, Renderer, Target, letterbox};
pub use simple::{SimpleSkin, layout};
pub use skin::{FrameInput, PlaySkin, SkinError};
pub use sprite::{Blend, Sprite, SpriteBatch};

thread_local! {
    static LIBRARY_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Whether the calling thread is inside a call into wgpu made by this
/// crate. A counting allocator uses it to leave the library's allocations
/// out of the app count (design.md "Scope of No Allocation"). Reading it
/// never allocates.
#[must_use]
pub fn in_library_call() -> bool {
    LIBRARY_DEPTH.try_with(Cell::get).unwrap_or(0) > 0
}

/// Runs `f`, a call into wgpu, inside the library scope.
fn library<R>(f: impl FnOnce() -> R) -> R {
    LIBRARY_DEPTH.with(|d| d.set(d.get() + 1));
    let r = f();
    LIBRARY_DEPTH.with(|d| d.set(d.get() - 1));
    r
}
