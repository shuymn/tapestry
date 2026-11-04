//! Imports skin definitions written in the source format's JSON; maps 7-key
//! play skins.
//!
//! The source format is not strict JSON: it is the lenient dialect of the
//! pinned libGDX 1.9.9 reader, with comments, optional commas, unquoted
//! names and values. [`Definition::read`] reads the entry file through a
//! [`skin_model::Root`] and parses it into one flat arena of nodes with
//! byte spans; strings are validated while parsing and decoded only when a
//! field is read. [`resolve`] expands the `include` and `if`/`value`/
//! `values` wrappers of the fields that take them, for one set of enabled
//! options, reading included files as it reaches them; it can run again on
//! the same definition with another set. [`Resolved::header`] reads the
//! header: the screen kind the skin's `type` names (7-key play, song
//! selection, song decision or single-chart result), the skin's name,
//! author and size, its options, file choices and offsets. [`read_header`]
//! does all three with no option enabled, as the source's own header pass
//! does.
//!
//! [`import`] refuses a skin outside its slot, and a kind with no importer
//! yet, before anything is read. It maps a play skin's images, numbers,
//! text and play objects (the lanes, SUDDEN+, LIFT, the judge and the
//! gauge) onto a [`skin_model::Skin`] for a [`Config`] (the selected
//! options, file choices and offsets), and returns it with a [`Report`]:
//! the definition files read, the file each choice took, a located note
//! for every element dropped, changed by a listed deviation or ignored,
//! and the charged counters ([`Usage`]). A play-critical element it cannot
//! reproduce refuses the import; anything else is dropped with a note.
//!
//! A Lua skin ([`lua`]) runs in a bounded sandbox, once for its header
//! ([`read_lua_header`]) and once for a configuration
//! ([`LuaHeader::import`]); the table it returns is converted into the
//! same arena, its values typed as Lua typed them, and imported through
//! [`import`].
//!
//! Every input bound ([`limits`]) is checked before the work it guards:
//! file sizes from metadata before reading, the include depth and parse
//! count before opening, nodes before they are inserted, decoded strings
//! before they are allocated, folder entries before they are examined.
//!
//! The cast lints below reject `as` casts that may truncate, wrap, or lose
//! the sign.

#![forbid(unsafe_code)]
#![deny(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

mod error;
mod header;
mod import;
mod json;
pub mod limits;
pub mod lua;
mod resolve;
mod tree;

pub use error::{ImportError, InputLimit, Loc, Syntax};
pub use header::{FileSpec, Header, OffsetSpec, OptionItem, OptionSpec, SIZES, read_header};
pub use import::{
    Config, CriticalPart, DeviationId, FileChoice, ImportLimit, Imported, LaneApprox, Note,
    NoteFallback, NoteKind, NoteLoc, Object, OffsetValue, PartWhy, PropertyKind, Report, Rule,
    Severity, TimerWhy, TrackWhy, Usage, import,
};
pub use lua::facts::{
    ChartRank, Fact, FactDeps, FactId, FactRead, FactValue, ImportFacts, JudgeClass,
};
pub use lua::{
    Cancel, LuaHeader, LuaImported, LuaLimit, LuaOutcome, LuaPending, LuaUsage, LuaWhy,
    read_lua_header,
};
pub use resolve::{Definition, Resolved, resolve};
pub use tree::CallableKind;
