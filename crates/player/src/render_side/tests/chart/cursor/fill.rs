//! The fill of the rows near the cursor (design.md "Rendering and Skins",
//! chart images): once the cursor's row is served, its neighbours are read
//! nearest first, each by a speculative job of its own in background mode,
//! one at a time; a neighbour never takes a slot its rest filled, showed or
//! kept; a move stops it at its next checkpoint; it never runs with no
//! speculative permit, and a refused background mode ends it for the run.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use game::coord::{Caps, Class, Coordinator, Spawner};
use library::Name;
use skin_model::chart_image::ChartImage;
use windows::Win32::System::Threading::{THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY};

use super::{Cursor, LetGo, chart, cursor_of, named, row};
use crate::chart_currency::CursorChart;
use crate::chart_images::FS_READS;
use crate::chart_images::{
    EntryState, FillGate, HELD_ROW_HARD, HOLD, HOLD_ROW_HARD, LEFT_BEHIND_BYTES, SlotState, Want,
};
use crate::render_side::tests::wait_until;

impl Cursor {
    /// The cursor rests on the row it is on, `dir` naming `names`, its
    /// fill to read `neighbours`.
    fn rest_with(&self, dir: &Path, names: [Name; 2], neighbours: &[&tempfile::TempDir]) {
        let near = neighbours.iter().map(|d| row(d.path(), named())).collect();
        self.w.rest(
            self.w
                .cursor_request(dir.to_path_buf(), names, near, &self.coord),
        );
    }

    /// Steps until the cursor's request is served, the fill has ended and
    /// the worker settled, taking the changes of the cursor's value.
    fn filled(&self) {
        self.until(|c| {
            let _ = c.w.take_changed();
            !c.w.cursor_waiting() && c.w.idle()
        });
    }

    /// The slot of `d`'s entry, if resident.
    fn slot_of(&self, d: &tempfile::TempDir) -> Option<u16> {
        match self.w.entry(&row(d.path(), named()))?.0 {
            EntryState::Resident { slot, .. } => Some(slot),
            EntryState::Absent => None,
        }
    }
}

/// `n` chart folders, each with its own 64×48 stage file and 30×8 banner.
fn charts(n: usize) -> Vec<tempfile::TempDir> {
    (0..n).map(|_| chart([64, 48], [30, 8])).collect()
}

/// After the cursor's row, the fill reads its neighbours in the order the
/// request gives them, nearest first, each in its own job, one write at a
/// time: the free slots go in that order (AC-S3).
#[test]
fn the_fill_reads_the_neighbours_in_their_order_after_the_cursor() {
    let _serial = crate::imports::tests::serial();
    let c = cursor_of(8);
    let (at, near) = (chart([64, 48], [30, 8]), charts(5));
    let near_refs: Vec<&tempfile::TempDir> = near.iter().collect();
    c.on(at.path(), named());
    c.rest_with(at.path(), named(), &near_refs);
    c.filled();
    assert_eq!(c.slot_of(&at), Some(0), "the cursor's row first");
    for (i, d) in near.iter().enumerate() {
        assert_eq!(c.slot_of(d), Some(u16::try_from(i + 1).unwrap()), "{i}");
    }
    assert_eq!(c.w.begun(), 6);
    let written: Vec<u16> = c.written().iter().map(|&(_, s, _)| s).collect();
    assert_eq!(written, [0, 1, 2, 3, 4, 5]);
    assert_eq!(c.w.writes_sent(), 6);
    // A return to a filled row shows it at once, with no read.
    let g = c.generation();
    assert_eq!(
        c.on(near[2].path(), named()),
        CursorChart::Resident {
            generation: g,
            slot: 3
        }
    );
}

/// A neighbour's read runs on a speculative permit, never a foreground
/// one, and in background mode: its thread enters it once (AC-S10).
#[test]
fn a_neighbour_is_read_on_a_speculative_permit_in_background_mode() {
    static ENTRIES: Mutex<u32> = Mutex::new(0);
    fn count(priority: THREAD_PRIORITY) -> windows::core::Result<()> {
        if priority == THREAD_MODE_BACKGROUND_BEGIN {
            *ENTRIES.lock().unwrap() += 1;
        }
        Ok(())
    }
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let mut c = cursor_of(8);
    c.w =
        c.w.clone()
            .with_fill_mode(Arc::new(crate::priority::Checked::with(count)));
    *ENTRIES.lock().unwrap() = 0;
    let (at, near) = (chart([64, 48], [30, 8]), charts(1));
    c.rest_on(at.path(), named());
    c.shown(at.path(), named());
    assert_eq!(
        *ENTRIES.lock().unwrap(),
        0,
        "the cursor's read in background mode"
    );
    HOLD.store(true, Ordering::Release);
    c.rest_with(at.path(), named(), &[&near[0]]);
    c.until(|c| c.w.begun() == 2 && c.coord.held(Class::Speculative) == 1);
    assert_eq!(c.coord.held(Class::Foreground), 0);
    HOLD.store(false, Ordering::Release);
    c.filled();
    assert_eq!(c.slot_of(&near[0]), Some(1));
    assert_eq!(*ENTRIES.lock().unwrap(), 1);
}

/// With no speculative permit (speculation off) the fill reads nothing:
/// only the cursor's row is read.
#[test]
fn with_speculation_off_the_fill_reads_nothing() {
    let _serial = crate::imports::tests::serial();
    let mut c = cursor_of(8);
    c.coord = Coordinator::new(Caps {
        foreground: 4,
        speculative: 0,
    });
    let (at, near) = (chart([64, 48], [30, 8]), charts(3));
    let near_refs: Vec<&tempfile::TempDir> = near.iter().collect();
    c.on(at.path(), named());
    c.rest_with(at.path(), named(), &near_refs);
    c.filled();
    assert_eq!(c.slot_of(&at), Some(0));
    assert!(
        near.iter()
            .all(|d| c.w.entry(&row(d.path(), named())).is_none())
    );
    assert_eq!(c.w.begun(), 1);
}

/// A refused background mode stops the fill and ends it for the run: the
/// neighbour is not read, its slot freed, and a later rest begins no fill
/// job (AC-S10). The refusal is logged once (dev-tools: the test logger).
#[test]
fn a_refused_background_mode_ends_the_fill_for_the_run() {
    fn refuse(priority: THREAD_PRIORITY) -> windows::core::Result<()> {
        if priority == THREAD_MODE_BACKGROUND_BEGIN {
            return Err(windows::Win32::Foundation::E_ACCESSDENIED.into());
        }
        Ok(())
    }
    let _serial = crate::imports::tests::serial();
    let mut c = cursor_of(8);
    c.w =
        c.w.clone()
            .with_fill_mode(Arc::new(crate::priority::Checked::with(refuse)));
    let (at, other, near) = (
        chart([64, 48], [30, 8]),
        chart([64, 48], [30, 8]),
        charts(2),
    );
    let run = || {
        c.on(at.path(), named());
        let reads = FS_READS.load(Ordering::Relaxed);
        c.rest_with(at.path(), named(), &[&near[0]]);
        c.filled();
        assert_eq!(
            FS_READS.load(Ordering::Relaxed) - reads,
            3,
            "the refused read touched the filesystem (the cursor's: its \
             folder and its two images)"
        );
        assert!(c.fill_off());
        assert!(c.w.entry(&row(near[0].path(), named())).is_none());
        assert_eq!(c.w.begun(), 2, "the cursor's read and the refused one");
        assert_eq!(
            c.w.slot_states()
                .iter()
                .filter(|s| **s != SlotState::Free)
                .count(),
            1
        );
        // The next rest reads its cursor's row and nothing more.
        c.on(other.path(), named());
        c.rest_with(other.path(), named(), &[&near[1]]);
        c.filled();
        assert!(c.slot_of(&other).is_some());
        assert!(c.w.entry(&row(near[1].path(), named())).is_none());
        assert_eq!(c.w.begun(), 3);
    };
    #[cfg(feature = "dev-tools")]
    {
        let lines = crate::result_skin::chart_tests::logged(
            "chart images: fill: background mode refused",
            run,
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
    }
    #[cfg(not(feature = "dev-tools"))]
    run();
}

/// A move stops the fill at its next checkpoint: the neighbour's read in
/// flight is cancelled and leaves no entry, its slot freed, and nothing
/// more is read until the next rest, which reads its own neighbours
/// (AC-S6). Leaving selection stops it the same way.
#[test]
fn a_move_stops_the_fill_until_the_next_rest() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor_of(8);
    let (at, to, near) = (
        chart([64, 48], [30, 8]),
        chart([64, 48], [30, 8]),
        charts(3),
    );
    c.rest_on(at.path(), named());
    c.shown(at.path(), named());
    HOLD.store(true, Ordering::Release);
    c.rest_with(at.path(), named(), &[&near[0], &near[1]]);
    c.until(|c| c.w.begun() == 2 && !c.w.idle());
    // The move: the held read sees its cancel, and nothing more begins.
    assert_eq!(c.on(to.path(), named()), CursorChart::Pending);
    c.until(|c| c.w.idle() && !c.w.cursor_waiting());
    HOLD.store(false, Ordering::Release);
    for _ in 0..20 {
        c.step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(c.w.begun(), 2);
    assert!(c.w.entry(&row(near[0].path(), named())).is_none());
    assert!(c.w.entry(&row(near[1].path(), named())).is_none());
    assert_eq!(c.w.fill_state(), (false, false));
    assert_eq!(
        c.w.slot_states()
            .iter()
            .filter(|s| **s != SlotState::Free)
            .count(),
        1,
        "the cancelled read's slot not freed"
    );
    // The next rest: its row, then its own neighbours.
    c.rest_with(to.path(), named(), &[&near[1]]);
    c.filled();
    assert!(c.slot_of(&to).is_some() && c.slot_of(&near[1]).is_some());
    assert!(c.w.entry(&row(near[0].path(), named())).is_none());
    // Leaving selection stops a fill as a move does.
    HOLD.store(true, Ordering::Release);
    let begun = c.w.begun();
    c.rest_with(to.path(), named(), &[&near[2]]);
    c.until(|c| c.w.begun() == begun + 1 && !c.w.idle());
    c.w.leave_selection(&c.coord);
    c.until(|c| c.w.idle());
    HOLD.store(false, Ordering::Release);
    assert!(c.w.entry(&row(near[2].path(), named())).is_none());
    assert_eq!(c.w.fill_state(), (false, false));
}

/// A reduced pool full of this rest's rows stops the fill rather than
/// evict them, and the cursor's row is still admitted at the next rest
/// (AC-S8): with three slots, the cursor and its two nearest neighbours
/// are read and the farther ones are not.
#[test]
fn a_full_pool_stops_the_fill_and_still_admits_the_cursor() {
    let _serial = crate::imports::tests::serial();
    let c = cursor_of(3);
    let (at, near) = (chart([64, 48], [30, 8]), charts(4));
    let near_refs: Vec<&tempfile::TempDir> = near.iter().collect();
    c.on(at.path(), named());
    c.rest_with(at.path(), named(), &near_refs);
    c.filled();
    assert_eq!(
        [c.slot_of(&at), c.slot_of(&near[0]), c.slot_of(&near[1])],
        [Some(0), Some(1), Some(2)]
    );
    assert!(c.w.entry(&row(near[2].path(), named())).is_none());
    assert!(c.w.entry(&row(near[3].path(), named())).is_none());
    assert_eq!(c.w.begun(), 3);
    // The cursor on a row with no entry: admitted, evicting one.
    c.on(near[3].path(), named());
    c.rest(near[3].path(), named());
    c.filled();
    assert!(c.slot_of(&near[3]).is_some(), "the cursor not admitted");
}

/// A resident row of the window is kept for the rest: a farther row never
/// takes its slot (design.md "Rendering and Skins", chart images). With
/// three slots holding the cursor and two rows of its window, a farther
/// neighbour is not read.
#[test]
fn a_resident_row_of_the_window_keeps_its_slot() {
    let _serial = crate::imports::tests::serial();
    let c = cursor_of(3);
    let (a, near) = (chart([64, 48], [30, 8]), charts(3));
    c.on(a.path(), named());
    c.rest_with(a.path(), named(), &[&near[0], &near[1]]);
    c.filled();
    let before = [c.slot_of(&a), c.slot_of(&near[0]), c.slot_of(&near[1])];
    assert!(before.iter().all(Option::is_some));
    // On near[0]: its window is a, near[1] (both resident) and near[2].
    c.on(near[0].path(), named());
    c.rest_with(near[0].path(), named(), &[&a, &near[1], &near[2]]);
    c.filled();
    assert_eq!(
        [c.slot_of(&a), c.slot_of(&near[0]), c.slot_of(&near[1])],
        before,
        "a nearer row evicted"
    );
    assert!(c.w.entry(&row(near[2].path(), named())).is_none());
    assert_eq!(c.w.begun(), 3);
}

/// A list read again with the cursor kept refills from the rows near it
/// now: a new neighbour is read, rows already resident are not read again.
#[test]
fn a_refreshed_list_refills_from_its_rows() {
    let _serial = crate::imports::tests::serial();
    let c = cursor_of(8);
    let (at, near) = (chart([64, 48], [30, 8]), charts(2));
    c.on(at.path(), named());
    c.rest_with(at.path(), named(), &[&near[0]]);
    c.filled();
    assert_eq!(c.w.begun(), 2);
    c.w.refill(vec![
        row(near[0].path(), named()),
        row(near[1].path(), named()),
    ]);
    c.filled();
    assert_eq!(c.slot_of(&near[1]), Some(2));
    assert_eq!(c.w.begun(), 3);
}

/// A new pool generation reads the cursor's row and then its neighbours
/// again, as on a cold start (AC-S14).
#[test]
fn a_new_pool_generation_fills_again() {
    let _serial = crate::imports::tests::serial();
    let c = cursor_of(8);
    let (at, near) = (chart([64, 48], [30, 8]), charts(2));
    let near_refs: Vec<&tempfile::TempDir> = near.iter().collect();
    c.on(at.path(), named());
    c.rest_with(at.path(), named(), &near_refs);
    c.filled();
    let next = super::super::pool_of(4);
    *c.r.pool.lock().unwrap() = Some(next);
    assert!(wait_until(|| c.r.link.pool_now().0 == Some(next)));
    c.step();
    assert!(c.w.take_rearm(), "the cursor's read not asked again");
    assert!(near.iter().all(|d| c.slot_of(d).is_none()));
    c.rest_with(at.path(), named(), &near_refs);
    c.filled();
    let fresh = |d: &tempfile::TempDir| {
        matches!(
            c.w.entry(&row(d.path(), named())).map(|e| e.0),
            Some(EntryState::Resident { generation, .. }) if generation == next.generation
        )
    };
    assert!(fresh(&at) && near.iter().all(fresh));
    // The cursor on a row naming no image, whose absent entry outlives
    // the generation: its neighbours are read again all the same.
    let none = tempfile::tempdir().unwrap();
    c.on(none.path(), [Name::None, Name::None]);
    c.w.rest(c.w.cursor_request(
        none.path().to_path_buf(),
        [Name::None, Name::None],
        near.iter().map(|d| row(d.path(), named())).collect(),
        &c.coord,
    ));
    c.filled();
    let last = super::super::pool_of(4);
    *c.r.pool.lock().unwrap() = Some(last);
    assert!(wait_until(|| c.r.link.pool_now().0 == Some(last)));
    c.step();
    assert!(
        c.w.take_rearm(),
        "an absent cursor row's neighbours not asked again"
    );
    c.w.rest(c.w.cursor_request(
        none.path().to_path_buf(),
        [Name::None, Name::None],
        near.iter().map(|d| row(d.path(), named())).collect(),
        &c.coord,
    ));
    c.filled();
    let fresh = |d: &tempfile::TempDir| {
        matches!(
            c.w.entry(&row(d.path(), named())).map(|e| e.0),
            Some(EntryState::Resident { generation, .. }) if generation == last.generation
        )
    };
    assert!(near.iter().all(fresh));
}

/// A new pool generation stops a fill read in flight, whose output it
/// would drop, so the cursor's read asked again begins without waiting
/// for it; the neighbours follow it (AC-S14).
#[test]
fn a_new_pool_generation_stops_the_fill_read_in_flight() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor_of(8);
    let (at, near) = (chart([64, 48], [30, 8]), charts(2));
    let near_refs: Vec<&tempfile::TempDir> = near.iter().collect();
    c.rest_on(at.path(), named());
    c.shown(at.path(), named());
    HOLD.store(true, Ordering::Release);
    c.rest_with(at.path(), named(), &near_refs);
    c.until(|c| c.w.begun() == 2 && !c.w.idle());
    let next = super::super::pool_of(4);
    *c.r.pool.lock().unwrap() = Some(next);
    assert!(wait_until(|| c.r.link.pool_now().0 == Some(next)));
    // The held read ends although the hold stays set: it saw the stop.
    c.until(|c| c.w.idle());
    assert!(c.w.take_rearm(), "the cursor's read not asked again");
    assert_eq!(c.w.begun(), 2);
    assert!(c.w.entry(&row(near[0].path(), named())).is_none());
    HOLD.store(false, Ordering::Release);
    c.rest_with(at.path(), named(), &near_refs);
    c.filled();
    assert_eq!(c.w.begun(), 5);
    let fresh = |d: &tempfile::TempDir| {
        matches!(
            c.w.entry(&row(d.path(), named())).map(|e| e.0),
            Some(EntryState::Resident { generation, .. }) if generation == next.generation
        )
    };
    assert!(fresh(&at) && near.iter().all(fresh));
}

/// A neighbour read once and found without images keeps its absent entry:
/// a later rest's fill passes it by, as it passes a resident row.
#[test]
fn an_absent_neighbour_is_not_read_again() {
    let _serial = crate::imports::tests::serial();
    let c = cursor_of(8);
    let (at, other) = (chart([64, 48], [30, 8]), chart([64, 48], [30, 8]));
    let gone = tempfile::tempdir().unwrap();
    c.on(at.path(), named());
    c.rest_with(at.path(), named(), &[&gone]);
    c.filled();
    assert_eq!(c.w.begun(), 2);
    assert!(matches!(
        c.w.entry(&row(gone.path(), named())).map(|e| e.0),
        Some(EntryState::Absent)
    ));
    c.on(other.path(), named());
    c.rest_with(other.path(), named(), &[&gone]);
    c.filled();
    assert_eq!(c.w.begun(), 3, "only the cursor's row read");
}

/// A list read again while the cursor's request is deferred replaces the
/// request's neighbours: the fill reads the new list's rows, not the old.
#[test]
fn a_refill_before_the_request_begins_replaces_its_neighbours() {
    let _serial = crate::imports::tests::serial();
    let c = cursor_of(8);
    let (at, near) = (chart([64, 48], [30, 8]), charts(2));
    c.on(at.path(), named());
    c.rest_with(at.path(), named(), &[&near[0]]);
    c.w.refill(vec![row(near[1].path(), named())]);
    c.filled();
    assert_eq!(c.w.begun(), 2);
    assert!(c.slot_of(&near[1]).is_some());
    assert!(c.w.entry(&row(near[0].path(), named())).is_none());
}

/// A neighbour's entry never evicts one its rest filled or kept: with the
/// entries at their bound, a later rest that keeps B and reads D evicts an
/// older entry of no slot of its own, never B's.
#[test]
fn a_neighbour_never_evicts_an_entry_its_rest_keeps() {
    let _serial = crate::imports::tests::serial();
    let c = cursor_of(8);
    let near = charts(4);
    let [a, b, cur, d] = [&near[0], &near[1], &near[2], &near[3]];
    c.on(a.path(), named());
    c.rest_with(a.path(), named(), &[b]);
    c.filled();
    assert!(c.slot_of(b).is_some());
    c.w.pad_entries(None, None);
    c.on(cur.path(), named());
    c.rest_with(cur.path(), named(), &[b, d]);
    c.filled();
    assert!(c.slot_of(b).is_some(), "the kept row's entry was evicted");
    assert!(c.slot_of(d).is_some() && c.slot_of(cur).is_some());
    assert!(c.slot_of(a).is_none(), "the cursor's row evicts the oldest");
}

/// A check that withdraws the cursor's entry while a neighbour is read
/// stops that read; no neighbour begins until the cursor's read is asked
/// again, and the request asked again reads the stopped row after the
/// cursor's (AC-S8).
#[test]
fn a_withdrawn_cursor_row_is_read_again_before_its_neighbours() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor_of(8);
    let (at, near) = (chart([640, 480], [300, 80]), charts(2));
    let near_refs: Vec<&tempfile::TempDir> = near.iter().collect();
    c.rest_on(at.path(), named());
    c.shown(at.path(), named());
    c.w.update_chart(false);
    c.settled();
    let checks = c.w.checks_begun();
    HOLD.store(true, Ordering::Release);
    c.rest_with(at.path(), named(), &near_refs);
    c.until(|c| c.w.begun() == 2 && !c.w.idle());
    std::thread::sleep(Duration::from_millis(20));
    super::png(at.path(), "s.png", [320, 240]);
    // A use: a list refreshed with the cursor kept.
    let _ = c.w.update_chart(true);
    c.until(|c| {
        c.w.checks_pending() == (false, false) && c.w.checks_begun() > checks && c.w.idle()
    });
    assert!(c.w.entry(&row(at.path(), named())).is_none());
    for _ in 0..20 {
        c.step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(c.w.begun(), 2, "a neighbour began before the cursor's read");
    assert!(c.w.take_rearm());
    HOLD.store(false, Ordering::Release);
    c.rest_with(at.path(), named(), &near_refs);
    c.until(|c| c.w.entry(&row(at.path(), named())).is_some());
    assert!(c.w.entry(&row(near[0].path(), named())).is_none());
    c.filled();
    assert!(c.slot_of(&near[0]).is_some() && c.slot_of(&near[1]).is_some());
    assert_eq!(c.w.begun(), 5);
}

/// A list read again that no longer has the neighbour being read stops
/// that read; the new list's rows are read instead.
#[test]
fn a_refresh_stops_a_neighbour_it_no_longer_lists() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor_of(8);
    let (at, near) = (chart([64, 48], [30, 8]), charts(2));
    c.rest_on(at.path(), named());
    c.shown(at.path(), named());
    HOLD.store(true, Ordering::Release);
    c.rest_with(at.path(), named(), &[&near[0]]);
    c.until(|c| c.w.begun() == 2 && !c.w.idle());
    c.w.refill(vec![row(near[1].path(), named())]);
    c.until(|c| c.w.begun() == 3);
    HOLD.store(false, Ordering::Release);
    c.filled();
    assert!(c.w.entry(&row(near[0].path(), named())).is_none());
    assert!(c.slot_of(&near[1]).is_some());
}

/// A neighbour never takes a slot a pin holds, though an earlier rest
/// filled it: with two slots, the row a frame still displays keeps its
/// slot, and the fill stops.
#[test]
fn a_neighbour_never_takes_a_pinned_slot_of_an_earlier_rest() {
    let _serial = crate::imports::tests::serial();
    let c = cursor_of(2);
    let (a, b, near) = (
        chart([64, 48], [30, 8]),
        chart([64, 48], [30, 8]),
        charts(1),
    );
    c.rest_on(a.path(), named());
    let at_a = c.shown(a.path(), named());
    let EntryState::Resident { generation, slot } = at_a else {
        panic!("{at_a:?}");
    };
    c.r.charts.lock().unwrap().unpresented = true;
    let mut update = crate::render_side::tests::selection_update(1, 2);
    update.cursor_chart = CursorChart::Resident { generation, slot };
    c.r.link.show_selection(update);
    assert!(wait_until(
        || c.r.link.pool_now().1 == CursorChart::pack(generation, slot)
    ));
    c.on(b.path(), named());
    c.rest_with(b.path(), named(), &[&near[0]]);
    c.filled();
    assert!(c.slot_of(&b).is_some());
    assert_eq!(c.w.entry(&row(a.path(), named())).map(|e| e.0), Some(at_a));
    assert!(c.w.entry(&row(near[0].path(), named())).is_none());
}

/// When no entry may make room for a neighbour's, the fill stops: before
/// its read once the count bound is reached with nothing evictable, and
/// at its adoption when the byte bound leaves it no room, its slot freed
/// and the rows after it not read.
#[test]
fn the_fill_stops_when_no_entry_may_make_room() {
    let _serial = crate::imports::tests::serial();
    let (at, near) = (chart([64, 48], [30, 8]), charts(2));
    let near_refs: Vec<&tempfile::TempDir> = near.iter().collect();
    for leave in [None, Some(16)] {
        let c = cursor_of(8);
        c.rest_on(at.path(), named());
        c.shown(at.path(), named());
        c.w.pad_entries(Some(&row(at.path(), named())), leave);
        c.rest_with(at.path(), named(), &near_refs);
        c.filled();
        let read = u64::from(leave.is_some());
        assert_eq!(c.w.begun(), 1 + read, "{leave:?}");
        assert!(
            near.iter()
                .all(|d| c.w.entry(&row(d.path(), named())).is_none())
        );
        assert_eq!(c.w.fill_state(), (false, false), "{leave:?}");
        assert_eq!(
            c.w.slot_states()
                .iter()
                .filter(|s| **s != SlotState::Free)
                .count(),
            1,
            "{leave:?}: the unkept read's slot not freed"
        );
    }
}

/// A choice never waits on a fill read (design.md "Load Pipeline"): with
/// a neighbour's read stalled for a core, ignoring its stop, the choice of
/// a chart in that very folder reads its images beside it under a key of
/// its own, decoded, never refused as a job already running; the read is
/// left behind with its slot freed and counted at its bound ("Memory
/// Budget") until its thread ends. Back on selection while it lives, no
/// fill read begins, so at most one is left behind; once it ends, the fill
/// reads on.
#[test]
fn a_choice_never_waits_on_a_fill_read() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor_of(8);
    let (at, near) = (chart([64, 48], [30, 8]), charts(2));
    let near_refs: Vec<&tempfile::TempDir> = near.iter().collect();
    c.rest_on(at.path(), named());
    c.shown(at.path(), named());
    HELD_ROW_HARD.store(false, Ordering::Release);
    HOLD_ROW_HARD.store(true, Ordering::Release);
    c.rest_with(at.path(), named(), &near_refs);
    c.until(|c| c.w.begun() == 2 && !c.w.idle());
    assert!(wait_until(|| HELD_ROW_HARD.load(Ordering::Acquire)));
    let held = |c: &Cursor| {
        c.w.slot_states()
            .iter()
            .filter(|s| **s != SlotState::Free)
            .count()
    };
    assert_eq!(held(&c), 2, "the cursor's slot and the neighbour's");
    // The choice, of a chart in the folder the held read reads.
    c.w.leave_selection(&c.coord);
    c.r.link.leave_selection();
    assert!(c.w.left_behind_alive());
    assert_eq!(c.w.left_behind_bytes(), LEFT_BEHIND_BYTES);
    assert_eq!(held(&c), 1, "the left read's slot not freed");
    c.w.renew();
    c.w.attempt(Want {
        dir: near[0].path().to_path_buf(),
        names: ["s.png".to_owned(), "n.png".to_owned(), String::new()],
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
    let end = Instant::now() + Duration::from_secs(10);
    while c.w.current().is_none() {
        assert!(
            Instant::now() < end,
            "play waited on the fill read: {:?}",
            c.w
        );
        play_step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(HELD_ROW_HARD.load(Ordering::Acquire), "the fill read ended");
    assert_eq!(c.w.begun(), 3, "the cursor's, the neighbour's and play's");
    let images = c.w.current().unwrap();
    let stage = images
        .read(ChartImage::Stage)
        .image
        .as_ref()
        .unwrap_or_else(|e| panic!("the chosen chart's stage file suppressed: {e:?}"));
    assert_eq!(stage.native(), [64, 48]);
    assert_eq!(
        stage.levels()[0][..4],
        [10, 20, 30, 255],
        "the stage file's pixels"
    );
    // Back on selection while the left read lives: the fill waits, no
    // read of it begins.
    c.w.forget();
    c.w.drop_images();
    c.r.link.enter_selection(2);
    c.r.link
        .show_selection(crate::render_side::tests::selection_update(2, 1));
    c.w.entered();
    c.on(at.path(), named());
    c.rest_with(at.path(), named(), &near_refs);
    for _ in 0..20 {
        c.step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(c.w.left_behind_alive());
    assert!(!c.w.cursor_deferred() && c.w.fill_state().0, "{:?}", c.w);
    let begun = c.w.begun();
    for _ in 0..20 {
        c.step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        c.w.begun(),
        begun,
        "a second fill read began beside the left one"
    );
    HOLD_ROW_HARD.store(false, Ordering::Release);
    c.filled();
    assert!(!c.w.left_behind_alive());
    assert_eq!(c.w.left_behind_bytes(), 0);
    assert!(c.slot_of(&near[0]).is_some() && c.slot_of(&near[1]).is_some());
    assert_eq!(c.w.begun(), begun + 2);
}

/// A read left behind at a choice keeps its thread's mode: a refusal of
/// background mode it met is recorded when it settles, and ends the fill
/// for the run as one met by a read taken does (AC-S10).
#[test]
fn a_refusal_met_by_a_read_left_behind_ends_the_fill() {
    fn refuse(priority: THREAD_PRIORITY) -> windows::core::Result<()> {
        if priority == THREAD_MODE_BACKGROUND_BEGIN {
            return Err(windows::Win32::Foundation::E_ACCESSDENIED.into());
        }
        Ok(())
    }
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let mut c = cursor_of(8);
    c.w =
        c.w.clone()
            .with_fill_mode(Arc::new(crate::priority::Checked::with(refuse)));
    let (at, near) = (chart([64, 48], [30, 8]), charts(1));
    c.rest_on(at.path(), named());
    c.shown(at.path(), named());
    let gates = c.w.fill_gates();
    gates.shut.store(true, Ordering::Release);
    c.rest_with(at.path(), named(), &[&near[0]]);
    // The read meets the refusal as its thread takes its class, before it
    // stands at its first gate.
    c.until(|_| gates.at.load(Ordering::Acquire) == FillGate::Begun as u8);
    c.w.leave_selection(&c.coord);
    c.r.link.leave_selection();
    assert!(c.w.left_behind_alive());
    assert!(!c.fill_off());
    gates.shut.store(false, Ordering::Release);
    let end = Instant::now() + Duration::from_secs(10);
    while c.w.left_behind_alive() {
        assert!(Instant::now() < end, "the read left behind never ended");
        c.w.service();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(c.fill_off(), "the refusal of the read left behind was lost");
}

impl Cursor {
    /// Whether a refused background mode ended the fill for the run.
    fn fill_off(&self) -> bool {
        self.w.fill_state().1
    }
}
