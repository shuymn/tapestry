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
/// string read as a `for` bound), so this bounds how long a check
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
/// leaves three orders of magnitude for slow machines and debug builds
/// and refuses nothing a skin needs. One `skin_lua` fuzz input imports up
/// to three times, so that target's timeout is 25 s.
pub const MAX_LUA_TIME: std::time::Duration = std::time::Duration::from_secs(5);
/// The most calls into the functions the host exposes to skins and to
/// the sandbox's protected calls, each charged before it does any work;
/// the library's own `charge`, `refuse` and `metatable` callbacks are
/// not counted, their work O(1).
pub const MAX_HOST_CALLS: u32 = 100_000;
/// The most bytes the sandbox library's native primitives may produce in
/// all ([`super::library`]): each call's result size is known from its
/// validated arguments and charged before the call. Every loop whose
/// length a skin chooses runs as Lua instead (but `next`'s scan of empty
/// slots, which VM memory bounds), under
/// [`MAX_INSTRUCTIONS`]. The VM's own string operations are single
/// instructions over strings the memory limit bounds, under [`MAX_LUA_TIME`].
pub const MAX_NATIVE_WORK: u64 = 100_000_000;
/// The largest result one native primitive call may produce (a
/// substring, a formatted or joined string, a capture), in
/// bytes; a larger one is refused before the call.
pub const MAX_PRIMITIVE_BYTES: i64 = 1 << 20;
/// The most nodes the analysis's Lua trees hold across a candidate's
/// files ([`super::syntax`]): expressions, statements, declarations,
/// functions and list entries, each charged before it is added. The
/// surveyed skins' configured passes parse about 120 000.
pub const MAX_AST_NODES: u64 = 1 << 20;
/// The deepest nesting of statements and subexpressions the analysis's
/// parser recurses into, checked before each level: Lua's own parser
/// stops at the same depth (`LUAI_MAXCCALLS`).
pub const MAX_AST_DEPTH: u32 = 200;
/// The most closure instances and captured values the analysis copies
/// out of a configured pass before its VM is dropped, each charged before
/// it is kept.
pub const MAX_CAPTURES: u64 = 1 << 16;
/// The most expression nodes the lowering of a candidate's callbacks
/// expands helpers into, each helper call charged its expanded size
/// before it is expanded.
pub const MAX_EXPANDED_NODES: u64 = 1 << 20;
