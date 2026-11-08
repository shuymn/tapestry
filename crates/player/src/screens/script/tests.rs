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

/// The cell with a committed runtime, and the flag its VM's destruction
/// sets.
fn committed() -> (tempfile::TempDir, Scripts, Arc<AtomicBool>) {
    let (dir, slot, vm) = staged();
    let scripts = Scripts::new();
    scripts.commit(ScreenKind::Selection, 1, slot);
    (dir, scripts, vm)
}

/// The retirement-token table (the S7-07b revert table's token rows):
/// each way of holding a token against each order of opening, closing
/// and retiring. A token destroys only when it is the cell's, of the
/// current epoch, with no interval open; otherwise the runtime is retired
/// into the deferred set, and the next good token destroys it.
#[test]
fn an_inactive_token_destroys_before_any_play() {
    let (_dir, scripts, vm) = committed();
    let token = scripts.inactive().unwrap();
    assert!(scripts.retire(Quiet::Inactive(token)));
    assert!(gone(&vm));
}

#[test]
fn an_inactive_token_kept_across_an_opening_destroys_nothing() {
    let (_dir, scripts, vm) = committed();
    let stale = scripts.inactive().unwrap();
    let interval = scripts.open_interval();
    assert!(!scripts.retire(Quiet::Inactive(stale)));
    assert!(!gone(&vm), "the play's interval is open");
    // Retired all the same: the visit after takes nothing.
    let closed = interval.close();
    assert!(scripts.slot(ScreenKind::Selection, 1).running().is_none());
    assert!(!gone(&vm));
    assert!(scripts.retire(Quiet::Closed(closed)));
    assert!(gone(&vm), "the close's token destroys what was deferred");
}

#[test]
fn an_inactive_token_from_before_a_finished_play_destroys_nothing() {
    let (_dir, scripts, vm) = committed();
    let stale = scripts.inactive().unwrap();
    let _closed = scripts.open_interval().close();
    assert!(!scripts.retire(Quiet::Inactive(stale)));
    assert!(!gone(&vm));
    assert!(scripts.retire(Quiet::Inactive(scripts.inactive().unwrap())));
    assert!(gone(&vm));
}

#[test]
fn a_saved_close_token_destroys_nothing_in_the_next_play() {
    let (_dir, scripts, vm) = committed();
    let saved = scripts.open_interval().close();
    let next = scripts.open_interval();
    assert!(!scripts.retire(Quiet::Closed(saved)));
    assert!(!gone(&vm));
    assert!(scripts.retire(Quiet::Closed(next.close())));
    assert!(gone(&vm));
}

#[test]
fn a_close_token_of_one_of_two_open_intervals_destroys_nothing() {
    let (_dir, scripts, vm) = committed();
    let first = scripts.open_interval();
    let second = scripts.open_interval();
    assert!(!scripts.retire(Quiet::Closed(second.close())));
    assert!(!gone(&vm), "the first interval is still open");
    assert!(scripts.slot(ScreenKind::Selection, 1).running().is_none());
    // The first's token is of an older epoch; a fresh one destroys.
    assert!(!scripts.retire(Quiet::Closed(first.close())));
    assert!(!gone(&vm));
    assert!(scripts.retire(Quiet::Inactive(scripts.inactive().unwrap())));
    assert!(gone(&vm));
}

#[test]
fn another_cells_tokens_destroy_nothing() {
    let (_dir, scripts, vm) = committed();
    let other = Scripts::new();
    let interval = scripts.open_interval();
    assert!(!scripts.retire(Quiet::Inactive(other.inactive().unwrap())));
    assert!(!scripts.retire(Quiet::Closed(other.open_interval().close())));
    assert!(!gone(&vm));
    assert!(scripts.retire(Quiet::Closed(interval.close())));
    assert!(gone(&vm));
}

#[test]
fn another_cells_token_destroys_nothing_with_no_interval_open() {
    let (_dir, scripts, vm) = committed();
    let other = Scripts::new();
    assert!(!scripts.retire(Quiet::Inactive(other.inactive().unwrap())));
    assert!(!gone(&vm));
}

/// An interval dropped unclosed (the game thread unwinding) stays open:
/// no token is quiet, and the cell's drop leaks the runtime.
#[test]
fn an_interval_dropped_unclosed_keeps_every_runtime() {
    let (_dir, scripts, vm) = committed();
    let stale = scripts.inactive().unwrap();
    drop(scripts.open_interval());
    assert!(scripts.inactive().is_none());
    assert!(!scripts.retire(Quiet::Inactive(stale)));
    drop(scripts);
    assert!(!gone(&vm));
}

/// An opening asked while a retirement destroys waits for it: the VM is
/// destroyed under the cell's lock, never with an interval open.
#[test]
fn no_interval_opens_while_a_retirement_destroys() {
    let (_dir, slot, _) = staged();
    let scripts = Scripts::new();
    let during = Arc::new(AtomicBool::new(false));
    let opener = Arc::new(std::sync::Mutex::new(None));
    {
        let (scripts, during, opener) = (scripts.clone(), Arc::clone(&during), Arc::clone(&opener));
        slot.on_vm_teardown(move || {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let s = scripts.clone();
            let t = std::thread::spawn(move || {
                let interval = s.open_interval();
                let _ = tx.send(());
                interval
            });
            // The opener had time to open, were it not held back.
            let opened = rx.recv_timeout(Duration::from_millis(200)).is_ok();
            during.store(opened, Ordering::Release);
            *opener.lock().unwrap() = Some((t, rx));
        });
    }
    scripts.commit(ScreenKind::Selection, 1, slot);
    assert!(scripts.retire(Quiet::Inactive(scripts.inactive().unwrap())));
    assert!(
        !during.load(Ordering::Acquire),
        "an interval opened during the destruction"
    );
    let (t, rx) = opener.lock().unwrap().take().unwrap();
    assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
    let _closed = t.join().unwrap().close();
}

/// A slot reports its runtime's stop once; a later screen's slot resuming
/// the stopped runtime reports it again, for its own notice.
#[test]
fn a_stop_is_reported_once_per_screen() {
    let dir = crate::selection_skin::runtime_fixture();
    let (bundle, slot) =
        crate::selection_skin::fixture_scripted(LIMITS, dir.path(), "stop.luaskin");
    let scripts = Scripts::new();
    scripts.commit(ScreenKind::Selection, 1, slot);
    let skin = bundle.skin();
    let mut sel = skin.prepare_selection();
    let timers = skin_model::SelectionTimers::default();
    let mut visit = scripts.slot(ScreenKind::Selection, 1);
    let mut stops = Vec::new();
    for frame in 0..8 {
        let at = bms_model::TimeUs(frame * 16_000);
        stops.push(super::super::selection_visit(&mut visit, skin, &mut sel, at, &timers).unwrap());
    }
    let reported: Vec<_> = stops.iter().flatten().collect();
    assert_eq!(reported, [&super::ScriptStop::Budget], "{stops:?}");
    drop(visit);
    let mut again = scripts.slot(ScreenKind::Selection, 1);
    let at = bms_model::TimeUs(200_000);
    assert_eq!(
        super::super::selection_visit(&mut again, skin, &mut sel, at, &timers).unwrap(),
        Some(super::ScriptStop::Budget)
    );
    assert_eq!(
        super::super::selection_visit(&mut again, skin, &mut sel, at, &timers).unwrap(),
        None
    );
}
