//! Imports 7-key play skin definitions written in the source format's JSON.
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
//! header: the skin's name, author and size, its options, file choices and
//! offsets. [`read_header`] does all three with no option enabled, as the
//! source's own header pass does.
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
mod resolve;
mod tree;

pub use error::{ImportError, InputLimit, Loc, Syntax};
pub use header::{FileSpec, Header, OffsetSpec, OptionItem, OptionSpec, SIZES, read_header};
pub use import::{
    Config, CriticalPart, DeviationId, FileChoice, ImportLimit, Imported, Note, NoteKind, NoteLoc,
    Object, OffsetValue, PropertyKind, Report, Rule, Severity, TimerWhy, TrackWhy, Usage, import,
};
pub use resolve::{Definition, Resolved, resolve};
