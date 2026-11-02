//! The Lua sandbox's caps (design.md "Threat Model", Lua skins). Each is
//! charged before the work it bounds and counts across one candidate's
//! two passes; the loaders also take the definition bounds of
//! [`crate::limits`]: [`MAX_INCLUDE_DEPTH`](crate::limits::MAX_INCLUDE_DEPTH)
//! loaders active at once, [`MAX_INCLUDE_PARSES`](crate::limits::MAX_INCLUDE_PARSES)
//! load invocations, [`MAX_FILE_BYTES`](crate::limits::MAX_FILE_BYTES) a
//! file and [`MAX_INPUT_BYTES`](crate::limits::MAX_INPUT_BYTES) read in
//! all, and the converted table those of a parsed definition
//! ([`MAX_DEPTH`](crate::limits::MAX_DEPTH),
//! [`MAX_NODES`](crate::limits::MAX_NODES),
//! [`MAX_STRING_BYTES`](crate::limits::MAX_STRING_BYTES),
//! [`MAX_DECODED_BYTES`](crate::limits::MAX_DECODED_BYTES)).

/// The most Lua-managed memory a VM holds at once, compilation included:
/// an allocation past it fails. Only one VM is alive at a time.
pub const MAX_LUA_MEMORY: usize = 64 << 20;
/// The most Lua VM instructions across both passes, counted on every VM
/// thread in steps of [`INSTRUCTION_STEP`]: the step that reaches it
/// stops the run. A VM's last partial step, under one step, is not
/// counted.
pub const MAX_INSTRUCTIONS: u64 = 10_000_000;
/// How many instructions run between two charges of
/// [`MAX_INSTRUCTIONS`].
pub const INSTRUCTION_STEP: u32 = 1_000;
/// The most calls into the functions the host exposes, each charged
/// before it does any work.
pub const MAX_HOST_CALLS: u32 = 100_000;
/// The most work the library's native loops may do, which the
/// instruction count does not see: a pattern match's worst case,
/// `(length + 1)` to the power of its repetitions (one more unanchored)
/// times the pattern's length, and one unit an element `table.move`
/// copies or `table.sort` compares. Charged before the call.
pub const MAX_NATIVE_WORK: u64 = 100_000_000;
