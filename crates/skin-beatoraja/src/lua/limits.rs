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
/// functions, static access path steps and list entries, each charged
/// before it is added. The
/// surveyed skins' configured passes parse about 120 000.
pub const MAX_AST_NODES: u64 = 1 << 20;
/// The deepest nesting of statements and subexpressions the analysis's
/// parser recurses into, checked before each level: Lua's own parser
/// stops at the same depth (`LUAI_MAXCCALLS`). Every other recursive walk
/// over a tree (the effect walk, and partial evaluation through inlined
/// helpers) stops at it too, fatal: a left-leaning chain the parser
/// reads without recursing is as deep as it is long.
pub const MAX_AST_DEPTH: u32 = 200;
/// The most closure instances, captured values, captured tables and table
/// entries copied along static access paths the analysis keeps from a
/// configured pass before its VM is dropped, each charged before it is
/// kept.
pub const MAX_CAPTURES: u64 = 1 << 16;
/// The most units of work the callbacks' analysis does across a
/// candidate: each expression or statement the effect walk, a partial
/// evaluation or a role scan visits, each local scanned, each inlined
/// call, inline-stack entry and parameter bound, each environment slot
/// copied or merged, each conversion, distribution, attachment and
/// emission visit, each captured value a prototype's key copies, each
/// static access path step a capture follows, and each byte of a key or
/// string hashed, compared, generated or compiled, charged before the
/// work. The effect walk visits instances of one function with the same
/// captured values once, so a function created in a loop costs its body
/// once there.
pub const MAX_ANALYSIS_WORK: u64 = 1 << 24;
/// The most monotonic time a candidate's analysis spends in its phases
/// (closing the captured object graph, the effect walk, admission and
/// each lowering of a callable or role scan outside one), checked at
/// each phase's start and end, at each root the walk visits and at the
/// cancellation cadence of the work charged; the time between phases
/// does not count. A backstop
/// beside [`MAX_ANALYSIS_WORK`] for work a unit understates, as generous
/// as [`MAX_LUA_TIME`].
pub const MAX_ANALYSIS_TIME: std::time::Duration = std::time::Duration::from_secs(5);
/// The most bytes of rewritten chunks a configured pass keeps
/// ([`super::instrument`]): each chunk's source and what the rewrite
/// adds, checked before it is built. Twice the read allowance
/// ([`MAX_INPUT_BYTES`](crate::limits::MAX_INPUT_BYTES)), so a pass can
/// rewrite every byte it may read.
pub const MAX_REWRITTEN_BYTES: u64 = 1 << 26;
/// The most residual nodes the partial evaluation of a candidate's
/// callbacks makes in all ([`super::residual`]), each charged before it
/// is pushed; past it, fatal.
pub const MAX_EXPANDED_NODES: u64 = 1 << 20;

/// A kept VM's memory cap on a screen's runtime (docs/stage3.md S7-07,
/// provisional): lowered from [`MAX_LUA_MEMORY`] when the import keeps
/// the VM, twenty times the largest configured pass the surveyed skins
/// measured.
pub const RUNTIME_MEMORY: usize = 16 << 20;
/// The most instructions a screen's runtime runs in one frame, all its
/// callbacks together (provisional): the import's [`MAX_INSTRUCTIONS`]
/// over 50 frames.
pub const RUNTIME_INSTRUCTIONS: u64 = MAX_INSTRUCTIONS / 50;
/// The most host calls a screen's runtime makes in one frame
/// (provisional): the import's [`MAX_HOST_CALLS`] over 50 frames.
pub const RUNTIME_HOST_CALLS: u32 = MAX_HOST_CALLS / 50;
/// The most monotonic time a screen's runtime runs Lua in one frame
/// (provisional), checked cooperatively at each instruction step and host
/// call: a check, not a deadline.
pub const RUNTIME_FRAME_TIME: std::time::Duration = std::time::Duration::from_millis(2);
/// Consecutive frames past the budget after which a runtime stops for
/// its screen's installation.
pub const RUNTIME_STOP_FRAMES: u32 = 3;
