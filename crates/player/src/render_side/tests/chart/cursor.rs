//! The cursor's chart images on the worker, written through the render
//! thread into a selection chart-image pool (design.md "Rendering and
//! Skins", chart images; "Load Pipeline", Freshness): a rested row is read
//! into a slot no pin holds and its entry shown at once on a return; a
//! move drops a deferred request and cancels a running read, whose output
//! is never adopted; a check after each use withdraws a changed entry and
//! asks for its read again, its result counting only for the version it
//! checked; a new pool generation drops the resident entries and reads the
//! cursor's row again; leaving selection answers its pool write before
//! play's job; and nothing begins after the run's end.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use game::coord::{Caps, Coordinator, JobKind, Spawner};
use library::Name;

use super::{Rig, pool_of, rig_with};
use crate::chart_currency::CursorChart;
use crate::chart_images::{
    CHECK_HELD, CHECK_HOLD, ChartImageWorker, CursorId, EntryState, HELD_HARD, HOLD, HOLD_HARD,
    PANIC, SlotState, Want,
};
use crate::render_side::tests::wait_until;

const CAPS: Caps = Caps {
    foreground: 4,
    speculative: 1,
};

/// A `w`×`h` PNG at `dir/name`.
fn png(dir: &Path, name: &str, [w, h]: [u32; 2]) {
    let file = std::fs::File::create(dir.join(name)).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels = [10, 20, 30, 255].repeat(usize::try_from(w * h).unwrap());
    e.write_header().unwrap().write_image_data(&texels).unwrap();
}

/// A chart folder with a `stage` stage file `s.png` and a `banner` banner
/// `n.png`.
fn chart(stage: [u32; 2], banner: [u32; 2]) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    png(d.path(), "s.png", stage);
    png(d.path(), "n.png", banner);
    d
}

/// The names [`chart`] writes.
fn named() -> [Name; 2] {
    [
        Name::Named("s.png".to_owned()),
        Name::Named("n.png".to_owned()),
    ]
}

/// The row of `dir` naming `names`.
fn row(dir: &Path, names: [Name; 2]) -> CursorId {
    CursorId {
        dir: dir.to_path_buf(),
        names,
    }
}

/// A worker of a render thread drawing a selection whose screen has a
/// chart-image pool, and the run's coordinator.
struct Cursor {
    r: Rig,
    w: ChartImageWorker,
    coord: Coordinator,
}

/// [`cursor_of`] with a pool of 8 slots.
fn cursor() -> Cursor {
    cursor_of(8)
}

/// A worker of a render thread on selection whose screen has a pool of
/// `slots` slots: its writes are applied at the select screen's frame
/// boundaries.
fn cursor_of(slots: u16) -> Cursor {
    let r = rig_with(slots);
    r.link.enter_selection(1);
    r.link
        .show_selection(crate::render_side::tests::selection_update(1, 1));
    assert!(wait_until(|| r.link.pool_now().0.is_some()));
    let w = ChartImageWorker::new(r.link.chart_generation());
    Cursor {
        r,
        w,
        coord: Coordinator::new(CAPS),
    }
}

impl Cursor {
    /// The cursor moves to the row of `dir` naming `names`, and an update
    /// is sent: the value it carries.
    fn on(&self, dir: &Path, names: [Name; 2]) -> CursorChart {
        self.w.cursor_changed(&self.coord, Some(row(dir, names)));
        self.w.update_chart(false)
    }

    /// The cursor rests on the row it is on, `dir` naming `names`.
    fn rest(&self, dir: &Path, names: [Name; 2]) {
        self.w
            .rest(self.w.cursor_request(dir.to_path_buf(), names, &self.coord));
    }

    /// [`on`](Self::on), then [`rest`](Self::rest).
    fn rest_on(&self, dir: &Path, names: [Name; 2]) {
        let names_again = names.clone();
        self.on(dir, names);
        self.rest(dir, names_again);
    }

    /// The select screen's step of the worker.
    fn step(&self) {
        self.step_on(Spawner::SYSTEM);
    }

    fn step_on(&self, spawner: Spawner) {
        self.w
            .cursor_step(&self.coord, spawner, &self.r.link.chart_publisher());
    }

    /// Steps until `done`, at most 10 s.
    fn until(&self, done: impl Fn(&Self) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done(self) {
            assert!(Instant::now() < end, "{:?}", self.w);
            self.step();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Steps until the row of `dir` naming `names` has an entry, its read
    /// ended and its write answered, and every check settled; its entry.
    /// The change of the cursor's value its adoption made is taken.
    fn shown(&self, dir: &Path, names: [Name; 2]) -> EntryState {
        let id = row(dir, names);
        self.until(|c| {
            c.w.entry(&id).is_some()
                && c.w.idle()
                && !c.w.write_pending()
                && c.w.checks_pending() == (false, false)
        });
        let _ = self.w.take_changed();
        self.w.entry(&id).unwrap().0
    }

    /// Steps until the worker has settled: no read, write or check.
    fn settled(&self) {
        self.until(|c| {
            c.w.idle() && !c.w.write_pending() && c.w.checks_pending() == (false, false)
        });
    }

    /// The pool writes the screen applied: each one's generation, slot and
    /// which of its stage file and banner were images.
    fn written(&self) -> Vec<(u64, u16, [bool; 2])> {
        self.r.charts.lock().unwrap().pool_writes.clone()
    }

    /// The live pool generation.
    fn generation(&self) -> u64 {
        self.r.link.pool_now().0.unwrap().generation
    }
}

/// Lets go of [`HOLD`], [`HOLD_HARD`] and [`CHECK_HOLD`] when dropped, so a
/// failing test leaves no read or check held for the next.
struct LetGo;

impl Drop for LetGo {
    fn drop(&mut self) {
        HOLD.store(false, Ordering::Release);
        HOLD_HARD.store(false, Ordering::Release);
        CHECK_HOLD.store(false, Ordering::Release);
    }
}

/// A rested row's 640×480 stage file and 300×80 banner are read in one job,
/// each decoded once, and written into a free slot of the live pool in one
/// write; the row's entry is resident there, and the next update names
/// the slot. The back image is never read.
#[test]
fn a_rested_rows_images_are_written_into_a_slot() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let d = chart([640, 480], [300, 80]);
    c.rest_on(d.path(), named());
    let g = c.generation();
    let state = c.shown(d.path(), named());
    assert_eq!(
        state,
        EntryState::Resident {
            generation: g,
            slot: 0
        }
    );
    assert_eq!(c.w.begun(), 1);
    assert_eq!(c.w.counts().0, 2);
    assert_eq!(c.written(), vec![(g, 0, [true, true])]);
    assert_eq!(c.w.writes_sent(), 1);
    assert_eq!(
        c.w.update_chart(false),
        CursorChart::Resident {
            generation: g,
            slot: 0
        }
    );
    assert_eq!(c.w.slot_states()[0], SlotState::Resident { version: 1 });
}

/// A row visited before shows at once while its entry stays: A, B and A
/// again names A's slot in the first update on A, with no read, no write
/// and no rest (AC-S1).
#[test]
fn a_return_to_a_resident_row_shows_it_at_once() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let (a, b) = (chart([100, 100], [10, 10]), chart([200, 100], [20, 10]));
    c.rest_on(a.path(), named());
    let at_a = c.shown(a.path(), named());
    c.rest_on(b.path(), named());
    c.shown(b.path(), named());
    let (begun, writes) = (c.w.begun(), c.written().len());
    let EntryState::Resident { generation, slot } = at_a else {
        panic!("{at_a:?}");
    };
    assert_eq!(
        c.on(a.path(), named()),
        CursorChart::Resident { generation, slot }
    );
    c.settled();
    assert_eq!((c.w.begun(), c.written().len()), (begun, writes));
}

/// With one slot, A→B gives B the slot A held once no pin holds it: while
/// the render thread displays A's slot, B's read waits; once it does not,
/// B evicts A, and A→B→A reads A again (AC-S7, AC-S8).
#[test]
fn one_slot_a_to_b_to_a_reads_a_again() {
    let _serial = crate::imports::tests::serial();
    let c = cursor_of(1);
    let (a, b) = (chart([100, 100], [10, 10]), chart([200, 100], [20, 10]));
    c.rest_on(a.path(), named());
    let at_a = c.shown(a.path(), named());
    let EntryState::Resident { generation, slot } = at_a else {
        panic!("{at_a:?}");
    };
    let at = CursorChart::Resident { generation, slot };
    assert_eq!(c.w.update_chart(false), at);
    let mut update = crate::render_side::tests::selection_update(1, 2);
    update.cursor_chart = at;
    c.r.link.show_selection(update);
    assert!(wait_until(
        || c.r.link.pool_now().1 == CursorChart::pack(generation, slot)
    ));
    // The render thread still displays A's slot when B's update is sent.
    assert_eq!(c.on(b.path(), named()), CursorChart::Pending);
    c.rest(b.path(), named());
    for _ in 0..20 {
        c.step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(c.w.begun(), 1, "a read into a displayed slot");
    assert!(c.w.cursor_deferred() && c.w.cursor_waiting());
    assert!(c.w.entry(&row(a.path(), named())).is_some());
    // B's update displayed: A's slot is free of pins.
    c.r.link
        .show_selection(crate::render_side::tests::selection_update(1, 3));
    assert!(wait_until(|| c.r.link.pool_now().1 == 0));
    assert_eq!(c.shown(b.path(), named()), at_a);
    assert!(c.w.entry(&row(a.path(), named())).is_none(), "A kept");
    // Back on A: pending, and read again at its rest.
    assert_eq!(c.on(a.path(), named()), CursorChart::Pending);
    c.rest(a.path(), named());
    assert_eq!(c.shown(a.path(), named()), at_a);
    assert_eq!(c.w.begun(), 3);
    assert!(c.w.entry(&row(b.path(), named())).is_none());
}

/// A request deferred behind a running read is dropped by a move: once
/// that read ended, nothing begins until the next rest, whose row is the
/// one read.
#[test]
fn a_move_drops_a_request_deferred_behind_a_running_job() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor();
    let (a, b, n) = (
        chart([100, 100], [10, 10]),
        chart([200, 100], [20, 10]),
        chart([300, 100], [30, 10]),
    );
    HOLD.store(true, Ordering::Release);
    c.rest_on(a.path(), named());
    c.step();
    assert_eq!(c.w.begun(), 1);
    c.rest(b.path(), named());
    c.step();
    assert!(c.w.cursor_deferred() && c.w.cursor_waiting());
    c.on(n.path(), named());
    assert!(!c.w.cursor_deferred(), "the move kept the deferred request");
    HOLD.store(false, Ordering::Release);
    c.until(|c| c.w.idle());
    for _ in 0..20 {
        c.step();
    }
    assert_eq!(c.w.begun(), 1, "the dropped request began");
    assert_eq!(c.w.entries(), 0);
    c.rest(n.path(), named());
    c.shown(n.path(), named());
    assert_eq!(c.w.begun(), 2);
    assert_eq!(c.written().len(), 1);
}

/// A move during a held read cancels it through the cursor's scope: its
/// thread ends though the read is still held, its slot is freed, nothing
/// of it is written, and the next row's read is.
#[test]
fn a_move_cancels_a_held_read() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor();
    let (a, b) = (chart([100, 100], [10, 10]), chart([200, 100], [20, 10]));
    HOLD.store(true, Ordering::Release);
    c.rest_on(a.path(), named());
    c.step();
    assert_eq!(c.w.begun(), 1);
    c.rest_on(b.path(), named());
    // A's read ends on its cancel, B's begins: still held, for the read is.
    c.until(|c| c.w.begun() == 2);
    assert!(c.w.entry(&row(a.path(), named())).is_none(), "A adopted");
    assert!(c.written().is_empty(), "A written");
    // A's slot was freed: B's read took it.
    assert_eq!(c.w.slot_states()[0], SlotState::Writing { version: 2 });
    HOLD.store(false, Ordering::Release);
    c.shown(b.path(), named());
    assert_eq!(c.w.counts().0, 2, "only B's files decoded");
    assert_eq!(c.written().len(), 1);
}

/// A read that ended before a move, its outcome taken after it, is of an
/// old cursor generation: it is never adopted or written, and its slot is
/// freed (SX6).
#[test]
fn a_read_done_before_a_move_is_never_adopted_after_it() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let (a, b) = (chart([100, 100], [10, 10]), chart([200, 100], [20, 10]));
    c.rest_on(a.path(), named());
    c.step();
    assert_eq!(c.w.begun(), 1);
    // Its thread ends; no step takes its outcome yet.
    assert!(wait_until(|| c.w.job_ended()));
    c.on(b.path(), named());
    c.settled();
    assert_eq!(c.w.entries(), 0, "the old generation's read adopted");
    assert!(c.written().is_empty());
    assert!(c.w.slot_states().iter().all(|s| *s == SlotState::Free));
}

/// A chart naming no image is an absent entry at once, with no job and no
/// slot; so is a row whose images are both missing, read but taking no
/// slot, and an update names it absent.
#[test]
fn an_absent_row_takes_no_slot() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let d = tempfile::tempdir().unwrap();
    c.rest_on(d.path(), [Name::None, Name::None]);
    c.step();
    assert_eq!(c.w.begun(), 0);
    assert_eq!(
        c.shown(d.path(), [Name::None, Name::None]),
        EntryState::Absent
    );
    assert_eq!(c.w.update_chart(false), CursorChart::Absent);
    // Named but missing: read, absent, no slot.
    c.rest_on(d.path(), named());
    assert_eq!(c.shown(d.path(), named()), EntryState::Absent);
    assert_eq!(c.w.begun(), 1);
    assert!(c.written().is_empty());
    assert!(c.w.slot_states().iter().all(|s| *s == SlotState::Free));
    assert_eq!(c.w.update_chart(false), CursorChart::Absent);
}

/// An overlong name resolves to nothing, never to a file its prefix names.
#[test]
fn an_overlong_name_reads_nothing() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let d = chart([640, 480], [300, 80]);
    let names = || [Name::Overlong, Name::Named("n.png".to_owned())];
    c.rest_on(d.path(), names());
    c.shown(d.path(), names());
    assert_eq!(c.w.counts().0, 1);
    assert_eq!(c.written(), vec![(c.generation(), 0, [false, true])]);
}

/// A panicked read and one whose thread cannot start leave no entry and
/// free their slot, and the run goes on: the next rest reads the row.
#[test]
fn a_failed_cursor_job_leaves_no_entry() {
    let _serial = crate::imports::tests::serial();
    let d = chart([640, 480], [300, 80]);
    for refused in [false, true] {
        let c = cursor();
        c.rest_on(d.path(), named());
        if refused {
            c.step_on(Spawner::refusing_owned(&[JobKind::ChartImages]));
            assert!(c.w.idle());
        } else {
            PANIC.store(true, Ordering::Release);
            c.step();
            assert_eq!(c.w.begun(), 1);
        }
        c.settled();
        assert_eq!(c.w.entries(), 0, "refused {refused}");
        assert!(c.w.slot_states().iter().all(|s| *s == SlotState::Free));
        assert!(c.written().is_empty());
        c.rest(d.path(), named());
        assert!(matches!(
            c.shown(d.path(), named()),
            EntryState::Resident { .. }
        ));
    }
}

/// A refused write frees its slot and leaves no entry: the row is read
/// again at a later rest.
#[test]
fn a_refused_write_leaves_no_entry() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let d = chart([640, 480], [300, 80]);
    c.r.charts.lock().unwrap().refuse_writes = true;
    c.rest_on(d.path(), named());
    c.until(|c| c.w.writes_sent() == 1 && !c.w.write_pending());
    assert_eq!(c.w.entries(), 0);
    assert!(c.w.slot_states().iter().all(|s| *s == SlotState::Free));
    c.r.charts.lock().unwrap().refuse_writes = false;
    c.rest(d.path(), named());
    assert!(matches!(
        c.shown(d.path(), named()),
        EntryState::Resident { .. }
    ));
}

/// A new pool generation (a screen built again, a reduction, a
/// replacement) drops the resident entries and the slot map, keeps the
/// absent ones, and reads the cursor's row again with no move, into the
/// new generation; an update before that names it pending (AC-S14).
#[test]
fn a_new_pool_generation_reloads_the_cursor() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let (a, b, none) = (
        chart([100, 100], [10, 10]),
        chart([200, 100], [20, 10]),
        tempfile::tempdir().unwrap(),
    );
    c.rest_on(none.path(), [Name::None, Name::None]);
    c.shown(none.path(), [Name::None, Name::None]);
    c.rest_on(b.path(), named());
    c.shown(b.path(), named());
    c.rest_on(a.path(), named());
    c.shown(a.path(), named());
    let old = c.generation();
    // Another pool, of other capacity.
    let next = pool_of(2);
    *c.r.pool.lock().unwrap() = Some(next);
    assert!(wait_until(|| c.r.link.pool_now().0 == Some(next)));
    c.step();
    assert_eq!(c.w.pool_generation(), next.generation);
    assert!(c.w.take_changed(), "the cursor's resident value kept");
    assert!(c.w.take_rearm(), "the cursor's read not asked again");
    assert_eq!(c.w.update_chart(false), CursorChart::Pending);
    assert!(c.w.entry(&row(b.path(), named())).is_none());
    assert_eq!(
        c.w.entry(&row(none.path(), [Name::None, Name::None]))
            .unwrap()
            .0,
        EntryState::Absent
    );
    assert_eq!(c.w.slot_states(), vec![SlotState::Free; 2]);
    // The select screen asks again (its rest counted served).
    c.rest(a.path(), named());
    assert_eq!(
        c.shown(a.path(), named()),
        EntryState::Resident {
            generation: next.generation,
            slot: 0
        }
    );
    assert!(
        c.written()
            .iter()
            .all(|&(g, ..)| g == old || g == next.generation)
    );
    // No pool: nothing is read; a pool again asks for the row's read.
    *c.r.pool.lock().unwrap() = None;
    assert!(wait_until(|| c.r.link.pool_now().0.is_none()));
    c.step();
    assert!(c.w.entry(&row(a.path(), named())).is_none());
    let begun = c.w.begun();
    c.rest(a.path(), named());
    for _ in 0..10 {
        c.step();
    }
    assert_eq!(c.w.begun(), begun, "read with no pool");
    assert!(!c.w.take_rearm());
    let again = pool_of(1);
    *c.r.pool.lock().unwrap() = Some(again);
    assert!(wait_until(|| c.r.link.pool_now().0 == Some(again)));
    c.step();
    assert!(c.w.take_rearm(), "the row not asked again with a pool");
}

/// A changed file withdraws its entry at the check of its next use: a
/// stage file replaced, a stage file deleted, and a missing banner that
/// appeared each drop the cursor row's entry, free its slot, make the
/// next update pending and ask for the row's read again with no move
/// (AC-S4).
#[test]
fn a_changed_file_withdraws_its_entry() {
    let _serial = crate::imports::tests::serial();
    for change in ["replaced", "deleted", "appeared"] {
        let c = cursor();
        let d = chart([640, 480], [300, 80]);
        if change == "appeared" {
            std::fs::remove_file(d.path().join("n.png")).unwrap();
        }
        c.rest_on(d.path(), named());
        c.shown(d.path(), named());
        // The adoption's use is checked, unchanged.
        c.w.update_chart(false);
        c.settled();
        assert!(!c.w.take_changed() && !c.w.take_rearm(), "{change}");
        let checks = c.w.checks_begun();
        assert!(checks >= 1, "{change}");
        std::thread::sleep(Duration::from_millis(20));
        match change {
            "replaced" => png(d.path(), "s.png", [320, 240]),
            "deleted" => std::fs::remove_file(d.path().join("s.png")).unwrap(),
            _ => png(d.path(), "n.png", [300, 80]),
        }
        // A use: a list refreshed with the cursor kept.
        assert_ne!(c.w.update_chart(true), CursorChart::Pending, "{change}");
        c.until(|c| c.w.checks_pending() == (false, false) && c.w.checks_begun() > checks);
        assert!(c.w.entry(&row(d.path(), named())).is_none(), "{change}");
        assert!(c.w.take_changed() && c.w.take_rearm(), "{change}");
        assert_eq!(c.w.update_chart(false), CursorChart::Pending, "{change}");
        // Read again with no move.
        c.rest(d.path(), named());
        c.shown(d.path(), named());
        assert_eq!(c.w.begun(), 2, "{change}");
    }
}

/// An ordinary update (the same row and entry, no entry or refresh) is no
/// use: it schedules no check.
#[test]
fn an_ordinary_update_schedules_no_check() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let d = chart([640, 480], [300, 80]);
    c.rest_on(d.path(), named());
    c.shown(d.path(), named());
    c.w.update_chart(false);
    c.settled();
    let checks = c.w.checks_begun();
    for _ in 0..5 {
        c.w.update_chart(false);
        c.step();
    }
    c.settled();
    assert_eq!(c.w.checks_begun(), checks);
    c.w.entered();
    c.w.update_chart(false);
    c.settled();
    assert_eq!(c.w.checks_begun(), checks + 1, "an entry is a use");
}

/// The held unchanged result (AC-S17): A's check is held after its last
/// observation; the file changes while the cursor is on B; the cursor
/// returns to A, a use made after the check began; the check's unchanged
/// result is released. It validates only the use it covered: a follow-up
/// check runs, withdraws A, and A's read is asked again with no move.
/// Run for a resident entry, an absent one whose image is created, across
/// a departure and a re-entry, and for a refreshed list keeping the
/// cursor on A.
#[test]
fn a_use_during_a_held_unchanged_check_is_checked_again() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    for case in ["resident", "absent", "re-entry", "refresh"] {
        let c = cursor();
        let a = chart([640, 480], [300, 80]);
        let b = chart([200, 100], [20, 10]);
        if case == "absent" {
            std::fs::remove_file(a.path().join("s.png")).unwrap();
            std::fs::remove_file(a.path().join("n.png")).unwrap();
        }
        c.rest_on(a.path(), named());
        c.shown(a.path(), named());
        let checks = c.w.checks_begun();
        // A's use, its check held after its observation.
        CHECK_HOLD.store(true, Ordering::Release);
        c.w.update_chart(false);
        c.until(|_| CHECK_HELD.load(Ordering::Acquire));
        // The file changes; meanwhile another use of A.
        std::thread::sleep(Duration::from_millis(20));
        png(a.path(), "s.png", [160, 120]);
        match case {
            "refresh" => {
                c.w.update_chart(true);
            }
            "re-entry" => {
                c.w.leave_selection(&c.coord);
                c.w.entered();
                c.on(a.path(), named());
            }
            _ => {
                c.on(b.path(), named());
                c.on(a.path(), named());
            }
        }
        CHECK_HOLD.store(false, Ordering::Release);
        c.until(|c| c.w.checks_pending() == (false, false) && c.w.checks_begun() >= checks + 2);
        assert!(c.w.entry(&row(a.path(), named())).is_none(), "{case}");
        assert!(c.w.take_rearm(), "{case}");
        assert_eq!(c.w.update_chart(false), CursorChart::Pending, "{case}");
        assert_eq!(c.w.checks_begun(), checks + 2, "{case}");
    }
}

/// A check's result counts only for the version it checked: an entry read
/// again while its old version's check was held keeps the new read, and
/// the old mismatch drops nothing (AC-S15).
#[test]
fn a_check_counts_only_for_its_version() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor();
    let a = chart([640, 480], [300, 80]);
    c.rest_on(a.path(), named());
    c.shown(a.path(), named());
    let old = c.w.entry(&row(a.path(), named())).unwrap().1;
    CHECK_HOLD.store(true, Ordering::Release);
    c.w.update_chart(true);
    c.until(|_| CHECK_HELD.load(Ordering::Acquire));
    // Changed, then a new pool generation: the row is read again as a new
    // version while the old check is held.
    std::thread::sleep(Duration::from_millis(20));
    png(a.path(), "s.png", [160, 120]);
    let next = pool_of(4);
    *c.r.pool.lock().unwrap() = Some(next);
    assert!(wait_until(|| c.r.link.pool_now().0 == Some(next)));
    c.step();
    c.rest(a.path(), named());
    c.until(|c| c.w.entry(&row(a.path(), named())).is_some() && !c.w.write_pending());
    let new = c.w.entry(&row(a.path(), named())).unwrap().1;
    assert_ne!(new, old);
    CHECK_HOLD.store(false, Ordering::Release);
    c.until(|c| !c.w.checks_pending().0);
    assert_eq!(
        c.w.entry(&row(a.path(), named())).map(|e| e.1),
        Some(new),
        "the old version's check dropped the new read"
    );
}

/// A chart left while a cursor read still runs (a choice refused before
/// its departure): its slot is freed at once and nothing of the read is
/// written, so with one slot the next rest's row is read into it.
#[test]
fn a_chart_left_during_a_cursor_read_frees_its_slot() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor_of(1);
    let (a, b) = (chart([100, 100], [10, 10]), chart([200, 100], [20, 10]));
    HOLD_HARD.store(true, Ordering::Release);
    c.rest_on(a.path(), named());
    c.step();
    // In its read, which ignores its cancel.
    assert!(wait_until(|| HELD_HARD.load(Ordering::Acquire)));
    assert_eq!(c.w.slot_states(), vec![SlotState::Writing { version: 1 }]);
    // The choice, refused before its departure: the chart is left.
    c.w.leave_selection(&c.coord);
    c.w.forget();
    assert_eq!(c.w.slot_states(), vec![SlotState::Free], "the slot kept");
    HOLD_HARD.store(false, Ordering::Release);
    // Back on selection: B's rest is read into the slot.
    c.w.entered();
    c.rest_on(b.path(), named());
    assert!(matches!(
        c.shown(b.path(), named()),
        EntryState::Resident { slot: 0, .. }
    ));
    assert!(c.w.entry(&row(a.path(), named())).is_none(), "A adopted");
    assert_eq!(c.written().len(), 1);
}

/// A check never waits behind a held read or an unanswered write: A's use
/// is checked while B's read is held (r2 MEDIUM 5).
#[test]
fn a_check_does_not_wait_behind_a_held_read() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor();
    let (a, b) = (chart([640, 480], [300, 80]), chart([200, 100], [20, 10]));
    c.rest_on(a.path(), named());
    c.shown(a.path(), named());
    c.settled();
    let checks = c.w.checks_begun();
    HOLD_HARD.store(true, Ordering::Release);
    c.rest_on(b.path(), named());
    c.step();
    assert!(wait_until(|| HELD_HARD.load(Ordering::Acquire)));
    // A use of A, as an entry onto it would make, while B's read is held.
    c.w.cursor_changed(&c.coord, Some(row(a.path(), named())));
    c.w.entered();
    c.w.update_chart(false);
    c.until(|c| c.w.checks_begun() > checks && !c.w.checks_pending().0);
    assert!(!c.w.idle(), "the held read ended");
    HOLD_HARD.store(false, Ordering::Release);
    c.settled();
}

/// Selection left while a read is held and another request waits: the
/// request goes and the read is cancelled; play's job begins only once
/// that read's thread ended, and no cursor read is written or begins
/// after it. A write sent before the departure is answered, not kept, and
/// play's job waits for that answer.
#[test]
fn leaving_selection_ends_its_reads_before_plays_job() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor();
    let (a, b) = (chart([100, 100], [10, 10]), chart([200, 100], [20, 10]));
    HOLD_HARD.store(true, Ordering::Release);
    c.rest_on(a.path(), named());
    c.step();
    // In its read, which ignores its cancel.
    assert!(wait_until(|| HELD_HARD.load(Ordering::Acquire)));
    c.rest(b.path(), named());
    c.step();
    assert!(c.w.cursor_deferred());
    assert_eq!(c.w.begun(), 1);
    // The choice.
    c.w.leave_selection(&c.coord);
    c.r.link.leave_selection();
    assert!(!c.w.cursor_deferred(), "the deferred request kept");
    c.w.renew();
    c.w.attempt(Want {
        dir: b.path().to_path_buf(),
        names: ["s.png".to_owned(), String::new(), String::new()],
        play: Some(2_048),
        result: None,
    });
    let publisher = c.r.link.chart_publisher();
    let play_step = || {
        c.w.step(
            Some((&c.coord, Spawner::SYSTEM)),
            &|| false,
            Some(&publisher),
        );
        c.w.service();
    };
    for _ in 0..20 {
        play_step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(c.w.begun(), 1, "play's job began beside the cursor's");
    HOLD_HARD.store(false, Ordering::Release);
    let end = Instant::now() + Duration::from_secs(10);
    while c.w.current().is_none() || !c.w.idle() {
        assert!(Instant::now() < end, "{:?}", c.w);
        play_step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(c.w.begun(), 2, "a cursor job began after the choice");
    assert_eq!(c.w.entries(), 0, "a stale read was adopted");
    assert!(c.written().is_empty(), "the cancelled read was written");
    assert!(!c.w.write_pending());
}

/// A pool write the render thread holds when selection is left is
/// answered stale, never kept, and the worker frees its slot; play's job
/// waits for that answer.
#[test]
fn a_write_held_when_selection_is_left_is_answered() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let d = chart([640, 480], [300, 80]);
    // The render thread stalls in a selection frame: the write is taken
    // after it, and held for the next boundary.
    c.r.hold.store(true, Ordering::Release);
    assert!(wait_until(|| c.r.charts.lock().unwrap().stalled));
    c.rest_on(d.path(), named());
    c.until(|c| c.w.writes_sent() == 1);
    c.w.leave_selection(&c.coord);
    c.r.link.leave_selection();
    assert!(c.w.write_pending(), "the sent write dropped unanswered");
    c.r.hold.store(false, Ordering::Release);
    let end = Instant::now() + Duration::from_secs(10);
    while c.w.write_pending() {
        assert!(Instant::now() < end, "never answered");
        c.w.service();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(c.written().is_empty(), "written after selection was left");
    assert_eq!(c.w.entries(), 0);
    assert!(c.w.slot_states().iter().all(|s| *s == SlotState::Free));
}

/// A write queued before a move (AC-S5): the boundary that takes the
/// move's update applies no write, so the write lands after a frame of
/// that update, into a slot the update does not name; its row's entry is
/// adopted all the same, and a return to it shows it at once.
#[test]
fn a_write_queued_before_a_move_lands_after_the_moves_frame() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let (a, b) = (chart([640, 480], [300, 80]), chart([200, 100], [20, 10]));
    // The render thread stalls in a selection frame: the write and the
    // move's update are both taken at the boundary after it.
    c.r.hold.store(true, Ordering::Release);
    assert!(wait_until(|| c.r.charts.lock().unwrap().stalled));
    c.rest_on(a.path(), named());
    c.until(|c| c.w.writes_sent() == 1);
    let before = c.r.charts.lock().unwrap().selection_frames;
    let moved = c.on(b.path(), named());
    assert_eq!(moved, CursorChart::Pending);
    let mut update = crate::render_side::tests::selection_update(1, 2);
    update.cursor_chart = moved;
    c.r.link.show_selection(update);
    c.r.hold.store(false, Ordering::Release);
    c.until(|c| !c.w.write_pending());
    let charts = c.r.charts.lock().unwrap();
    // The stalled frame, then the move's frame, then the write.
    assert!(
        charts
            .write_frames
            .first()
            .is_some_and(|&f| f >= before + 2),
        "written at the boundary that took the move: {before} {:?}",
        charts.write_frames
    );
    drop(charts);
    let Some((EntryState::Resident { generation, slot }, ..)) = c.w.entry(&row(a.path(), named()))
    else {
        panic!("{:?}", c.w);
    };
    assert_eq!(
        c.on(a.path(), named()),
        CursorChart::Resident { generation, slot }
    );
}

/// A pool write is applied only on the select screen (A6): one sent once
/// selection is left is answered stale and writes nothing, and so is one
/// for a generation that is not live.
#[test]
fn a_write_off_selection_or_of_another_generation_is_stale() {
    let c = cursor();
    let g = c.generation();
    let fits = || {
        std::sync::Arc::new(crate::chart_images::ChartImages::absent(
            &skin_model::chart_image::Absent::NoHeader,
        ))
    };
    let answer = |generation| {
        c.r.link
            .write_pool(generation, 0, fits())
            .recv_timeout(Duration::from_secs(10))
            .expect("answered")
    };
    assert_eq!(answer(g + 1), Err(render::PoolWriteError::Stale));
    assert_eq!(answer(g), Ok(()));
    c.r.link.leave_selection();
    assert_eq!(answer(g), Err(render::PoolWriteError::Stale));
    assert_eq!(c.written(), vec![(g, 0, [false, false])]);
}

/// The run's end drops a deferred cursor request, the entries and a
/// running check, and nothing begins after it.
#[test]
fn the_end_drops_a_deferred_cursor_request() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor();
    let (a, b) = (chart([100, 100], [10, 10]), chart([200, 100], [20, 10]));
    HOLD.store(true, Ordering::Release);
    c.rest_on(a.path(), named());
    c.step();
    c.rest(b.path(), named());
    c.step();
    assert!(c.w.cursor_deferred());
    c.w.end();
    assert!(!c.w.cursor_deferred());
    assert!(c.w.idle());
    assert_eq!(c.w.retained(), 0);
    assert_eq!(c.w.checks_pending(), (false, false));
    HOLD.store(false, Ordering::Release);
    let begun = c.w.begun();
    c.rest(a.path(), named());
    for _ in 0..10 {
        c.step();
        c.w.service();
    }
    assert_eq!(c.w.begun(), begun, "a job began after the end");
    assert_eq!(c.w.entries(), 0);
}
