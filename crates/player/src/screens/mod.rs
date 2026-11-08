//! The screens outside play whose skins run Lua at runtime (docs/stage3.md
//! S7-07; design.md "beatoraja Lua Skins", the bounded runtime): a cold
//! module tree, the only place in `player` that names the runtime.
//!
//! - [`script`] holds the runtimes: the cell the render thread and
//!   `RenderLink` share, its host of parked runtimes by kind and
//!   generation, and the opaque [`ScriptSlot`] a screen holds one in.
//!   Every item of it is `pub(in crate::screens)`; what the rest of the
//!   crate may use is exported here.
//! - [`result_visit`] runs one result frame's runtime, the presented
//!   view's, between the frame's commit point and its build.
//! - [`selection_visit`] runs one selection frame's runtime between the
//!   frame's preparation and its build.
//!
//! `cargo xtask check-arch` builds this tree beside a sibling caller to
//! check that privacy keeps the runtime here, and its R6 checks that no
//! other `player` file names `mlua` or `skin_beatoraja::lua` (design.md
//! "beatoraja Lua Skins", Unreachable from play).

mod result_visit;
mod script;
mod selection_visit;

pub(crate) use result_visit::result_visit;
pub(crate) use script::{
    PlayInterval, Quiet, ScriptGen, ScriptNotes, ScriptPermit, ScriptSlot, ScriptStop, Scripts,
    ViewCommit,
};
pub(crate) use selection_visit::selection_visit;
