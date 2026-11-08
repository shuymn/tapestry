//! The cell's holdings against its events (design.md "beatoraja Lua
//! Skins", Ownership and Quiescence acknowledged): each runtime is an
//! import's, kept by the production import of a self-made skin, and its
//! VM's destruction is observed from the VM's own teardown.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use skin_model::ScreenKind;

use super::{Quiet, ScriptSlot, Scripts};

/// The limits of an import target, as the player's screen gives them.
const LIMITS: skin_model::AtlasLimits = skin_model::AtlasLimits {
    max_side: 4_096,
    max_layers: 16,
    reserved_layers: render::RESERVED_LAYERS,
};

/// The runtime fixture's import: its staged runtime, and a flag its VM's
/// destruction sets.
fn staged() -> (tempfile::TempDir, ScriptSlot, Arc<AtomicBool>) {
    let (dir, _, slot, gone) = staged_bundle();
    (dir, slot, gone)
}

/// [`staged`], with the bundle the import made.
fn staged_bundle() -> (
    tempfile::TempDir,
    Arc<render::SkinBundle>,
    ScriptSlot,
    Arc<AtomicBool>,
) {
    let dir = crate::selection_skin::runtime_fixture();
    let (bundle, slot) = crate::selection_skin::fixture_scripted(LIMITS, dir.path(), "rt.luaskin");
    assert!(slot.holds(), "the fixture's callback runs in the runtime");
    let gone = Arc::new(AtomicBool::new(false));
    slot.on_vm_drop(&gone);
    (dir, bundle, slot, gone)
}

fn gone(flag: &AtomicBool) -> bool {
    flag.load(Ordering::Acquire)
}

/// A cell whose last handle goes with a runtime no token retired leaks
/// it: its VM is not destroyed.
#[test]
fn a_cell_dropped_unretired_leaks_its_runtime() {
    let (_dir, slot, vm) = staged();
    let scripts = Scripts::new();
    scripts.commit(ScreenKind::Selection, 1, slot);
    drop(scripts);
    assert!(!gone(&vm));
}

/// A retirement with a token destroys the parked runtime, and retired is
/// terminal: a visit after it takes nothing.
#[test]
fn a_retired_runtime_is_destroyed_and_never_resumed() {
    let (_dir, slot, vm) = staged();
    let scripts = Scripts::new();
    scripts.commit(ScreenKind::Selection, 1, slot);
    let mut visit = scripts.slot(ScreenKind::Selection, 1);
    assert!(visit.running().is_some());
    visit.park();
    assert!(!gone(&vm), "parking destroys nothing");
    scripts.retire(Quiet::Inactive(scripts.inactive().unwrap()));
    assert!(gone(&vm));
    assert!(visit.running().is_none());
    assert!(scripts.slot(ScreenKind::Selection, 1).running().is_none());
}

/// While a play's interval is open no visit takes the runtime, a
/// retirement on the render thread destroys nothing, and no token says the
/// cell is quiet; the interval's close gives the token that destroys it.
#[test]
fn nothing_is_taken_or_destroyed_while_a_play_runs() {
    let (_dir, slot, vm) = staged();
    let scripts = Scripts::new();
    scripts.commit(ScreenKind::Selection, 1, slot);
    let interval = scripts.open_interval();
    assert!(scripts.slot(ScreenKind::Selection, 1).running().is_none());
    scripts.retire_kind(ScreenKind::Selection);
    assert!(!gone(&vm));
    assert!(scripts.inactive().is_none());
    let closed = interval.close();
    assert!(!gone(&vm));
    scripts.retire(Quiet::Closed(closed));
    assert!(gone(&vm));
}

/// A visit's slot parks its runtime when dropped, and resumes it after:
/// the same VM, its Lua state kept (owner decision O7 (a)).
#[test]
fn a_dropped_visit_parks_and_the_next_resumes() {
    let (_dir, bundle, slot, vm) = staged_bundle();
    let scripts = Scripts::new();
    scripts.commit(ScreenKind::Selection, 1, slot);
    let calls = |s: &mut ScriptSlot| s.running().map(|rt| rt.counters().calls);
    let mut visit = scripts.slot(ScreenKind::Selection, 1);
    let mut sel = bundle.skin().prepare_selection();
    if let Some(rt) = visit.running() {
        let (fields, timers) = sel.runtime_parts();
        rt.frame(fields, timers);
    }
    drop(visit);
    assert!(!gone(&vm));
    let mut again = scripts.slot(ScreenKind::Selection, 1);
    assert_eq!(calls(&mut again), Some(1));
    // Another installation's slot takes nothing.
    assert!(scripts.slot(ScreenKind::Selection, 2).running().is_none());
}

/// A runtime of a generation a later commit superseded is retired when
/// its visit parks it, and the new generation's runtime is the one taken.
#[test]
fn a_superseded_generation_is_retired_as_it_parks() {
    let (_dir, first, old) = staged();
    let (_dir2, second, new) = staged();
    let scripts = Scripts::new();
    scripts.commit(ScreenKind::Selection, 1, first);
    let mut visit = scripts.slot(ScreenKind::Selection, 1);
    assert!(visit.running().is_some());
    scripts.commit(ScreenKind::Selection, 1, second);
    visit.park();
    assert!(gone(&old));
    assert!(!gone(&new));
    assert!(visit.running().is_some());
}

/// The orphan mark completes as its thread unwinds while another thread
/// holds the cell's lock: it neither locks nor waits.
#[test]
fn the_orphan_mark_neither_locks_nor_waits() {
    let scripts = Scripts::new();
    let held = scripts.0.host();
    let guard = scripts.orphan_guard();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let t = std::thread::spawn(move || {
        let _tx = tx;
        let _guard = guard;
        panic!("the render thread fails");
    });
    // The sender drops after the guard, as the thread unwinds.
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    );
    assert!(t.join().is_err());
    assert!(scripts.orphaned());
    drop(held);
}
