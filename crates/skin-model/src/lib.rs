//! The skin runtime model.
//!
//! A [`Schema`] names the state a skin may read: typed fields with declared
//! ranges, timers and enumerations, each at a fixed slot. Expressions are
//! compiled against it ([`compile`]) into a [`Program`]: a postfix list of
//! pure, total integer operations whose stack depth and length are known at
//! load. Every integer subexpression is proven to stay inside `i32` at
//! compile time, and every read is clamped into its declared range, so
//! evaluating a program cannot overflow, allocates nothing and never panics.
//!
//! A frame's drawing is a [`QuadBatch`]: one ordered list of [`Quad`]s split
//! into runs wherever the blend or the clip changes.
//!
//! A [`Skin`] holds elements in draw order, each a keyframe [`Track`] on a
//! timer, and `when` timers. [`Skin::prepare`] makes one attempt's [`Play`];
//! [`Skin::build`] runs each frame's drained effects through its reducer,
//! reads the play view's snapshot into the [`PLAY`] schema's fields, starts
//! the timers and pushes each drawn element's quad.
//!
//! Expressions use integers only; floats appear only in a quad's geometry,
//! after evaluation. The cast lints below reject `as` casts that may
//! truncate, wrap, or lose the sign; they cover the library and its unit
//! tests, not integration tests.

#![forbid(unsafe_code)]
#![deny(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

mod expr;
mod play;
mod quad;
mod schema;
mod skin;
mod track;

pub use expr::{
    CompileError, ErrorKind, MAX_ELAPSED_MS, MAX_NAME_BYTES, MAX_NESTING, MAX_OPS, MAX_SKIN_OPS,
    MAX_SOURCE_BYTES, MAX_STACK, OptionDef, Program, Scope, SkinOps, Span, State, TIMER_OFF,
    compile,
};
pub use play::schema::{
    GAUGE_PERMILLE, Grade, LaneTimer, MAX_COUNT, PLAY, field, lane_timer, timer,
};
pub use play::{Attempt, GaugeDesc, Play, PlayContext, PrepareError, SkinFrame};
pub use quad::{Blend, ClipId, Quad, QuadBatch, Run, RunKey};
pub use schema::{EnumDef, EnumId, Export, Field, FieldTy, Schema, SchemaError, TimerDef, Ty};
pub use skin::{
    ElementDef, LoadError, MAX_ELEMENTS, MAX_SKIN_KEYS, MAX_WHEN_TIMERS, Skin, Source, TimerRef,
};
pub use track::{
    Ease, End, KeyDef, MAX_GEOMETRY, MAX_KEY_US, MAX_TRACK_KEYS, Prop, Sample, Track, TrackError,
};
