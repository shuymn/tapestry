//! The importer's input bounds (design.md "Threat Model", Skins). Each is
//! checked before the work it guards, and all of them count across one
//! resolution: the entry file and every file it includes, every branch.

/// The largest definition file, in bytes, checked from its metadata before
/// it is read.
pub const MAX_FILE_BYTES: u64 = 16 << 20;
/// The most bytes read across one resolution, the entry file included and
/// a file included twice counted twice. A read is offered the smaller of
/// [`MAX_FILE_BYTES`] and what remains.
pub const MAX_INPUT_BYTES: u64 = 32 << 20;
/// The deepest nesting, counted on one path from the entry file's root
/// value through arrays, objects, branches and included files' values.
pub const MAX_DEPTH: u32 = 64;
/// The most parsed nodes across one resolution, every file and branch and
/// unknown fields included. The arena never holds more.
pub const MAX_NODES: usize = 2 << 20;
/// The most values a resolution expands into its fields' arrays.
pub const MAX_EXPANDED: usize = 2 << 20;
/// The longest string, name or unquoted value, in bytes as written.
pub const MAX_STRING_BYTES: usize = 64 << 10;
/// The most bytes of decoded strings across one resolution, a separate
/// count from the input bytes.
pub const MAX_DECODED_BYTES: u64 = 32 << 20;
/// The longest chain of includes from the entry file.
pub const MAX_INCLUDE_DEPTH: u32 = 16;
/// The most included files read across one resolution.
pub const MAX_INCLUDE_PARSES: u32 = 1_024;
/// The most folder entries enumerated across one resolution, counted
/// before they are filtered or sorted.
pub const MAX_LIST_ENTRIES: usize = 4_096;
/// The most options a header holds.
pub const MAX_OPTIONS: usize = 1_024;
/// The most option items across a header's options.
pub const MAX_OPTION_ITEMS: usize = 16_384;
/// The most file choices a header holds.
pub const MAX_FILE_CHOICES: usize = 1_024;
/// The most offsets a header holds.
pub const MAX_OFFSETS: usize = 1_024;

/// The bounds one definition is read and resolved with: the constants
/// above, lowered only by tests of bounds the input caps make unreachable.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) expanded: usize,
    pub(crate) decoded_bytes: u64,
}

impl Limits {
    pub(crate) const STANDARD: Self = Self {
        expanded: MAX_EXPANDED,
        decoded_bytes: MAX_DECODED_BYTES,
    };
}

/// The most report notes an import keeps; later ones are counted only.
pub const MAX_NOTES: usize = 4_096;
