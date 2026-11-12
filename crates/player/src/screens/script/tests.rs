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
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
    let (_dir, scripts, vm) = committed();
    let token = scripts.inactive().unwrap();
    assert!(scripts.retire(Quiet::Inactive(token)));
    assert!(gone(&vm));
}

#[test]
fn an_inactive_token_kept_across_an_opening_destroys_nothing() {
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
    let (_dir, scripts, vm) = committed();
    let other = Scripts::new();
    assert!(!scripts.retire(Quiet::Inactive(other.inactive().unwrap())));
    assert!(!gone(&vm));
}

/// An interval dropped unclosed (the game thread unwinding) stays open:
/// no token is quiet, and the cell's drop leaks the runtime.
#[test]
fn an_interval_dropped_unclosed_keeps_every_runtime() {
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
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
    let _serial = crate::imports::tests::serial();
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
        stops.push(
            super::super::selection_visit(
                &mut visit,
                skin,
                &mut sel,
                at,
                &timers,
                &skin_model::AppReadouts::LAUNCH,
            )
            .unwrap(),
        );
    }
    let reported: Vec<_> = stops.iter().flatten().collect();
    assert_eq!(reported, [&super::ScriptStop::Budget], "{stops:?}");
    drop(visit);
    let mut again = scripts.slot(ScreenKind::Selection, 1);
    let at = bms_model::TimeUs(200_000);
    assert_eq!(
        super::super::selection_visit(
            &mut again,
            skin,
            &mut sel,
            at,
            &timers,
            &skin_model::AppReadouts::LAUNCH
        )
        .unwrap(),
        Some(super::ScriptStop::Budget)
    );
    assert_eq!(
        super::super::selection_visit(
            &mut again,
            skin,
            &mut sel,
            at,
            &timers,
            &skin_model::AppReadouts::LAUNCH
        )
        .unwrap(),
        None
    );
}

/// A result generation of the runtime fixture: the cell's generation and
/// the flag its VM's destruction sets.
fn offered(
    scripts: &Scripts,
    retained: bool,
) -> (tempfile::TempDir, super::ScriptGen, Arc<AtomicBool>) {
    let (dir, slot, vm) = staged();
    let g = scripts.offer_result(slot, retained).expect("a generation");
    (dir, g, vm)
}

fn live(scripts: &Scripts, g: super::ScriptGen) -> bool {
    matches!(scripts.result_state(g), super::GenState::Live { .. })
}

/// s7-07-design §3.7.1 M1, C10: two result generations hold the two
/// permits; a third offer is refused and its VM retired; generations are
/// unique in the cell, and another cell's is gone here.
#[test]
fn two_result_generations_hold_the_permits() {
    let _serial = crate::imports::tests::serial();
    let scripts = Scripts::new();
    let (_a, ga, va) = offered(&scripts, false);
    let (_b, gb, vb) = offered(&scripts, false);
    assert_ne!(ga, gb);
    assert_eq!((scripts.permits_free(), scripts.result_entries()), (0, 2));
    let (_c, third, vc) = staged();
    assert_eq!(scripts.offer_result(third, false), None);
    assert!(gone(&vc) && !gone(&va) && !gone(&vb));
    assert!(!live(&Scripts::new(), ga), "another cell's generation");
    // The run's end: a token retires both.
    assert!(scripts.retire(Quiet::Inactive(scripts.inactive().unwrap())));
    assert!(gone(&va) && gone(&vb));
    assert_eq!(scripts.permits_free(), 2);
}

/// §4 rule 2, M5: the eviction before a take retires every parked
/// generation but the presented one, the fixed skin's and the one kept;
/// a view of an evicted generation is not committed, its commit not run.
#[test]
fn the_eviction_spares_the_presented_retained_and_kept() {
    let _serial = crate::imports::tests::serial();
    let scripts = Scripts::new();
    scripts.enter_result(1);
    let (_a, presented, va) = offered(&scripts, false);
    let (_b, other, vb) = offered(&scripts, false);
    scripts.set_published(Some(presented));
    assert_eq!(
        scripts.commit_view(Some(presented), 1, || true),
        super::ViewCommit::Committed
    );
    let permit = scripts.result_permit(&[]).expect("the evicted permit");
    assert!(gone(&vb) && !gone(&va));
    assert!(live(&scripts, presented) && !live(&scripts, other));
    let mut ran = false;
    assert_eq!(
        scripts.commit_view(Some(other), 1, || {
            ran = true;
            true
        }),
        super::ViewCommit::Gone
    );
    assert!(!ran);
    drop(permit);
    // A kept generation survives; the presented mark moves with a commit,
    // so the formerly presented one is evicted after.
    let (_c, kept, vc) = offered(&scripts, false);
    scripts.set_published(None);
    assert_eq!(
        scripts.commit_view(None, 1, || true),
        super::ViewCommit::Committed
    );
    let again = scripts.result_permit(&[Some(kept)]);
    assert!(again.is_some());
    assert!(gone(&va) && !gone(&vc));
    let scripts = Scripts::new();
    let (_d, fixed, vd) = offered(&scripts, true);
    assert!(scripts.result_permit(&[]).is_some());
    assert!(!gone(&vd) && live(&scripts, fixed));
}

/// M9 (plan r2 HIGH): a view discarded (a refusal, the pin, a switch)
/// releases only its reference: a same-generation prepared slot dropped
/// while the presented one runs keeps the generation live and running,
/// its permit held, and the runtime resumes after parking.
#[test]
fn a_discarded_view_of_the_presented_generation_retires_nothing() {
    let _serial = crate::imports::tests::serial();
    let scripts = Scripts::new();
    let (_a, g, vm) = offered(&scripts, false);
    let notes = super::ScriptNotes::default();
    let mut presented = scripts.result_slot(Some(g));
    assert!(presented.result_running(&notes).is_some());
    let prepared = scripts.result_slot(Some(g));
    drop(prepared);
    assert!(!gone(&vm) && live(&scripts, g));
    assert_eq!(scripts.permits_free(), 1);
    assert!(presented.result_running(&notes).is_some(), "still running");
    presented.park();
    assert!(
        scripts
            .result_slot(Some(g))
            .result_running(&notes)
            .is_some()
    );
}

/// A switch that keeps the layout moves the running runtime to the new
/// view's slot; a slot of another generation takes nothing from it.
#[test]
fn a_same_generation_switch_moves_the_running_runtime() {
    let _serial = crate::imports::tests::serial();
    let scripts = Scripts::new();
    let (_a, g, _) = offered(&scripts, false);
    let (_b, h, _) = offered(&scripts, false);
    let notes = super::ScriptNotes::default();
    let mut old = scripts.result_slot(Some(g));
    assert!(old.result_running(&notes).is_some());
    let mut other = scripts.result_slot(Some(h));
    other.transfer_from(&mut old);
    assert!(old.runs() && !other.runs());
    let mut new = scripts.result_slot(Some(g));
    new.transfer_from(&mut old);
    assert!(new.runs() && !old.runs());
}

/// M9: a slot re-reads its generation at each use: once disabled, the
/// runtime it holds is given back and retired, nothing runs, and a later
/// offer is refused.
#[test]
fn a_running_slot_rechecks_its_generation() {
    let _serial = crate::imports::tests::serial();
    let scripts = Scripts::new();
    let (_a, g, vm) = offered(&scripts, false);
    let notes = super::ScriptNotes::default();
    let mut slot = scripts.result_slot(Some(g));
    assert!(slot.result_running(&notes).is_some());
    scripts.disable_result();
    assert!(!gone(&vm), "running: retired as its slot gives it back");
    assert!(slot.result_running(&notes).is_none());
    assert!(gone(&vm));
    assert_eq!(scripts.permits_free(), 2);
    let (_b, late, vl) = staged();
    assert_eq!(scripts.offer_result(late, false), None);
    assert!(gone(&vl));
}

/// C6, C11 on result: while a play's interval is open no result slot
/// takes its runtime, and a retirement then destroys nothing until the
/// close's token.
#[test]
fn a_result_runtime_waits_out_the_interval() {
    let _serial = crate::imports::tests::serial();
    let scripts = Scripts::new();
    let (_a, g, vm) = offered(&scripts, false);
    let notes = super::ScriptNotes::default();
    let mut slot = scripts.result_slot(Some(g));
    assert!(slot.result_running(&notes).is_some());
    slot.park();
    let interval = scripts.open_interval();
    assert!(slot.result_running(&notes).is_none());
    scripts.disable_result();
    assert!(!gone(&vm));
    let closed = interval.close();
    assert!(scripts.retire(Quiet::Closed(closed)));
    assert!(gone(&vm));
}

/// O6, M7: a slot taking its runtime gives it the lifetime's notes.
#[test]
fn a_taken_runtime_writes_to_the_lifetimes_notes() {
    let _serial = crate::imports::tests::serial();
    let dir = crate::selection_skin::runtime_fixture();
    let (bundle, slot) =
        crate::selection_skin::fixture_scripted(LIMITS, dir.path(), "stop.luaskin");
    let scripts = Scripts::new();
    let g = scripts.offer_result(slot, false).unwrap();
    let notes = super::ScriptNotes::default();
    let mut s = scripts.result_slot(Some(g));
    let mut sel = bundle.skin().prepare_selection();
    for _ in 0..4 {
        if let Some(rt) = s.result_running(&notes) {
            let (fields, timers) = sel.runtime_parts();
            rt.frame(fields, timers);
        }
    }
    let (kept, _) = notes.notes();
    assert!(
        kept.iter()
            .any(|n| matches!(n, skin_beatoraja::lua::runtime::RuntimeNote::Stopped(_))),
        "{kept:?}"
    );
}

/// Sets this thread's script switches until dropped (dev-tools: the
/// visits this test runs on its own thread).
struct Switches;

impl Switches {
    fn set(s: crate::dev::ScriptSwitches) -> Self {
        crate::dev::set_thread_script_switches(Some(s));
        Self
    }
}

impl Drop for Switches {
    fn drop(&mut self) {
        crate::dev::set_thread_script_switches(None);
    }
}

/// `entry`'s selection runtime committed in a fresh cell, with its
/// bundle.
fn selection_of(entry: &str) -> (tempfile::TempDir, Arc<render::SkinBundle>, Scripts) {
    let dir = crate::selection_skin::runtime_fixture();
    let (bundle, slot) = crate::selection_skin::fixture_scripted(LIMITS, dir.path(), entry);
    assert!(slot.holds(), "{entry}'s callback runs in the runtime");
    let scripts = Scripts::new();
    scripts.commit(ScreenKind::Selection, 1, slot);
    (dir, bundle, scripts)
}

/// `n` selection visits of `scripts`' runtime from `visit`, the stops
/// they reported.
fn visits(
    visit: &mut ScriptSlot,
    bundle: &render::SkinBundle,
    n: i64,
) -> Vec<Option<super::ScriptStop>> {
    let skin = bundle.skin();
    let mut sel = skin.prepare_selection();
    let timers = skin_model::SelectionTimers::default();
    (0..n)
        .map(|f| {
            super::super::selection_visit(
                visit,
                skin,
                &mut sel,
                bms_model::TimeUs(f * 16_000),
                &timers,
                &skin_model::AppReadouts::LAUNCH,
            )
            .unwrap()
        })
        .collect()
}

/// D2's budget switch for a kind stops that kind's runtime after its
/// budget frames, reported once; the other kind's switch leaves it
/// running (the S7-07d gate, P4).
#[test]
fn a_budget_switch_stops_its_kinds_runtime_alone() {
    let _serial = crate::imports::tests::serial();
    let (_dir, bundle, scripts) = selection_of("host.luaskin");
    let mut visit = scripts.slot(ScreenKind::Selection, 1);
    {
        let _s = Switches::set(crate::dev::ScriptSwitches {
            budget_result: true,
            ..Default::default()
        });
        let stops = visits(&mut visit, &bundle, 6);
        assert!(stops.iter().all(Option::is_none), "{stops:?}");
    }
    let _s = Switches::set(crate::dev::ScriptSwitches {
        budget_select: true,
        ..Default::default()
    });
    let stops = visits(&mut visit, &bundle, 6);
    let reported: Vec<_> = stops.iter().flatten().collect();
    assert_eq!(reported, [&super::ScriptStop::Budget], "{stops:?}");
}

/// D2's `off` takes no runtime: it stays parked with no call, and the
/// outputs are their constants (P5's baseline).
#[test]
fn the_off_switch_takes_no_runtime() {
    let _serial = crate::imports::tests::serial();
    let (_dir, bundle, scripts) = selection_of("rt.luaskin");
    let _s = Switches::set(crate::dev::ScriptSwitches {
        off: true,
        ..Default::default()
    });
    let mut visit = scripts.slot(ScreenKind::Selection, 1);
    let skin = bundle.skin();
    let mut sel = skin.prepare_selection();
    let timers = skin_model::SelectionTimers::default();
    for f in 0..3 {
        let at = bms_model::TimeUs(f * 16_000);
        assert_eq!(
            super::super::selection_visit(
                &mut visit,
                skin,
                &mut sel,
                at,
                &timers,
                &skin_model::AppReadouts::LAUNCH
            )
            .unwrap(),
            None
        );
        assert!(sel.outputs_mut().iter().all(|&v| v == 0));
    }
    assert_eq!(scripts.parked(ScreenKind::Selection), Some((0, false)));
    assert!(!super::super::take_visit().ran);
}

/// D2's `collect` runs exactly one full collection after each runtime
/// frame, which the frame's sample notes with the runtime's generation,
/// and none while a play's interval is open, when no visit takes the
/// runtime.
#[test]
fn the_collect_switch_collects_once_after_each_frame() {
    let _serial = crate::imports::tests::serial();
    let (_dir, bundle, scripts) = selection_of("rt.luaskin");
    let _s = Switches::set(crate::dev::ScriptSwitches {
        collect: true,
        ..Default::default()
    });
    let collections = super::super::collections;
    let mut visit = scripts.slot(ScreenKind::Selection, 1);
    let before = collections();
    for n in 1..=4_u64 {
        let _ = visits(&mut visit, &bundle, 1);
        let s = super::super::take_visit();
        assert!(s.ran && s.collected, "{s:?}");
        assert_eq!(collections() - before, n, "one collection a frame");
        let rt = visit.running().unwrap();
        assert_eq!(rt.collections(), n);
        assert_eq!(
            s.collected_at_calls,
            rt.counters().calls,
            "after the frame's calls"
        );
    }
    let generation = visit.generation();
    assert!(generation.is_some());
    // A play's interval: the visit parks, and none takes it while open.
    visit.park();
    let interval = scripts.open_interval();
    let at_open = collections();
    let mut during = scripts.slot(ScreenKind::Selection, 1);
    let _ = visits(&mut during, &bundle, 3);
    assert!(!super::super::take_visit().ran);
    assert_eq!(
        collections(),
        at_open,
        "no collection while the interval is open"
    );
    let _closed = interval.close();
    let _ = visits(&mut during, &bundle, 1);
    let s = super::super::take_visit();
    assert!(s.ran && s.collected);
    assert_eq!(Some(s.generation), generation);
    assert_eq!(collections(), at_open + 1);
}

/// A visit holding its runtime takes no script cell lock for its
/// instruments: its frame, with the switches' work and its sample,
/// completes while another thread holds the cell's lock.
#[test]
fn the_probe_runs_while_another_thread_holds_the_cell() {
    let _serial = crate::imports::tests::serial();
    let (_dir, bundle, scripts) = selection_of("rt.luaskin");
    let mut visit = scripts.slot(ScreenKind::Selection, 1);
    assert!(visit.running().is_some(), "taken before the lock is held");
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let cell = Arc::clone(&scripts.0);
    let holder = std::thread::spawn(move || {
        let _guard = cell.host();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });
    held_rx.recv().unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let prober = std::thread::spawn(move || {
        let _s = Switches::set(crate::dev::ScriptSwitches {
            collect: true,
            ..Default::default()
        });
        let _ = visits(&mut visit, &bundle, 1);
        let s = super::super::take_visit();
        done_tx.send(s.ran).unwrap();
        visit
    });
    let ran = done_rx.recv_timeout(Duration::from_secs(20));
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let visit = prober.join().unwrap();
    assert_eq!(ran, Ok(true), "the probe waited on the cell's lock");
    drop(visit);
}
