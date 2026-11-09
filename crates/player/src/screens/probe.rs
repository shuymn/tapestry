//! The visits' development instruments (dev-tools; the S7-07d gate):
//! the script switches ([`crate::dev::ScriptSwitches`]) applied around a
//! runtime frame, the frame's sample for the screen-frame probe, and the
//! process-wide counts of runtime frames and forced collections.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use skin_beatoraja::lua::runtime::ScreenRuntime;
use skin_beatoraja::vm_hooks;
use skin_model::ScreenKind;

/// What one visit's runtime frame cost: plain numbers, readable outside
/// `screens`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VisitSample {
    /// Whether a runtime ran this frame.
    pub(crate) ran: bool,
    /// The generation of the runtime that ran.
    pub(crate) generation: u64,
    /// The runtime frame's wall time, the forced collection apart.
    pub(crate) span: Duration,
    /// The forced collection's wall time after the frame (`collect`).
    pub(crate) collect_span: Duration,
    /// Whether a forced collection ran after the frame.
    pub(crate) collected: bool,
    /// The runtime's calls when its collections were counted, after the
    /// frame: where the collection ran.
    pub(crate) collected_at_calls: u64,
    /// Instructions run in the frame.
    pub(crate) instructions: u64,
    /// Host calls made in the frame.
    pub(crate) host_calls: u64,
    /// Bridges compiled in the frame: a first use of an id.
    pub(crate) bridges: usize,
    /// The VM's Lua heap after the frame (and the collection), bytes.
    pub(crate) heap: usize,
    /// The VM's sampled heap high-water so far, bytes.
    pub(crate) peak_heap: u64,
}

thread_local! {
    /// This thread's last visit's sample, until taken.
    static LAST: Cell<VisitSample> = const {
        Cell::new(VisitSample {
            ran: false,
            generation: 0,
            span: Duration::ZERO,
            collect_span: Duration::ZERO,
            collected: false,
            collected_at_calls: 0,
            instructions: 0,
            host_calls: 0,
            bridges: 0,
            heap: 0,
            peak_heap: 0,
        })
    };
}

/// Runtime frames run in the process.
static FRAMES: AtomicU64 = AtomicU64::new(0);
/// Forced collections run in the process.
static COLLECTIONS: AtomicU64 = AtomicU64::new(0);

/// This thread's last visit's sample, taken: a visit with no runtime
/// leaves none (`ran` false).
pub(crate) fn take_visit() -> VisitSample {
    LAST.with(|l| l.replace(VisitSample::default()))
}

/// The runtime frames run in the process so far.
pub(crate) fn runtime_frames() -> u64 {
    FRAMES.load(Ordering::SeqCst)
}

/// The forced collections run in the process so far.
pub(crate) fn collections() -> u64 {
    COLLECTIONS.load(Ordering::SeqCst)
}

/// Whether the switches keep the visits from taking a runtime.
pub(in crate::screens) fn off() -> bool {
    crate::dev::script_switches().off
}

/// Runs `frame` on `rt`, `kind`'s runtime of `generation`, under the
/// switches: with no time budget when that kind's budget switch is on,
/// then one full collection when `collect` is; and notes the sample.
/// Takes no script cell or barrier lock, and allocates nothing.
pub(in crate::screens) fn run(
    rt: &mut ScreenRuntime,
    kind: ScreenKind,
    generation: u64,
    frame: impl FnOnce(&mut ScreenRuntime),
) {
    let s = crate::dev::script_switches();
    let budget = match kind {
        ScreenKind::Selection => s.budget_select,
        ScreenKind::Result => s.budget_result,
        _ => false,
    };
    let before = rt.counters();
    let started = Instant::now();
    if budget {
        vm_hooks::set_frame_time(Some(Duration::ZERO));
    }
    frame(rt);
    if budget {
        vm_hooks::set_frame_time(None);
    }
    FRAMES.fetch_add(1, Ordering::SeqCst);
    let span = started.elapsed();
    let after = rt.counters();
    let collections = rt.collections();
    let collect_span = if s.collect {
        let at = Instant::now();
        rt.collect();
        at.elapsed()
    } else {
        Duration::ZERO
    };
    // The collections the runtime itself counts, so a second one or one
    // run elsewhere shows.
    let ran = rt.collections().saturating_sub(collections);
    COLLECTIONS.fetch_add(ran, Ordering::SeqCst);
    let collected = ran > 0;
    let sample = VisitSample {
        ran: true,
        generation,
        span,
        collect_span,
        collected,
        collected_at_calls: rt.counters().calls,
        instructions: after.instructions.saturating_sub(before.instructions),
        host_calls: after.host_calls.saturating_sub(before.host_calls),
        bridges: after.bridges.saturating_sub(before.bridges),
        heap: rt.heap(),
        peak_heap: rt.peak_heap(),
    };
    LAST.with(|l| l.set(sample));
}
