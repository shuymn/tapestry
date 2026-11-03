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
/// counted: the two passes' VMs leave at most 198 instructions uncounted
/// in all, the loaders sharing their VM's counter.
pub const MAX_INSTRUCTIONS: u64 = 10_000_000;
/// How many instructions run between two charges of
/// [`MAX_INSTRUCTIONS`], and between two checks of cancellation and
/// [`MAX_LUA_TIME`]. One VM instruction can cost up to the size of VM
/// memory (a concatenation, a comparison of long strings, a numeral
/// string read in a bitwise operation), so this bounds how long a check
/// can wait. At 100, with the time check, the `lua` bench's execution
/// and conversion took under 2 % longer than at 1 000, and its run to the
/// instruction cap 16 % longer.
pub const INSTRUCTION_STEP: u32 = 100;
/// The most monotonic time a candidate's two passes may run Lua in all,
/// checked with cancellation at every instruction step and host call:
/// one figure bounding the CPU a candidate can use where an
/// instruction's cost is not counted (the VM's string operations, each
/// bounded by VM memory). Only the passes' own time counts, not the time
/// between them, while a header waits for its configuration. The
/// surveyed skins execute in about 1.5 ms and a run to
/// [`MAX_INSTRUCTIONS`] takes about 38 ms (the `lua` bench), so 5 s
/// leaves three orders of magnitude for slow machines and debug builds,
/// refuses nothing a skin needs, and ends a candidate within the fuzz
/// targets' 10 s timeout.
pub const MAX_LUA_TIME: std::time::Duration = std::time::Duration::from_secs(5);
/// The most calls into the functions the host exposes, each charged
/// before it does any work.
pub const MAX_HOST_CALLS: u32 = 100_000;
/// The most bytes the sandbox library's native primitives may produce in
/// all ([`super::library`]): each call's result size is known from its
/// validated arguments and charged before the call. Every loop whose
/// length a skin chooses runs as Lua instead, under
/// [`MAX_INSTRUCTIONS`]. The VM's own string operations are single
/// instructions over strings the memory limit bounds, under [`MAX_LUA_TIME`].
pub const MAX_NATIVE_WORK: u64 = 100_000_000;
/// The largest result one native primitive call may produce (a
/// substring, a repetition, a formatted or joined string, a capture), in
/// bytes; a larger one is refused before the call.
pub const MAX_PRIMITIVE_BYTES: i64 = 1 << 20;
