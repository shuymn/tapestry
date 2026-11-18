//! The fill of the rows near the cursor (design.md "Rendering and Skins",
//! chart images): once the cursor's row is served, its neighbours are read
//! nearest first, each by a speculative job of its own in background mode,
//! one at a time; a neighbour never takes a slot its rest filled, showed or
//! kept; a move stops it at its next checkpoint; it never runs with no
//! speculative permit, and a refused background mode ends it for the run.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use game::coord::{Caps, Class, Coordinator};
use library::Name;
use windows::Win32::System::Threading::{THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY};

use super::{Cursor, LetGo, chart, cursor_of, named, row};
use crate::chart_currency::CursorChart;
use crate::chart_images::{EntryState, HOLD, SlotState};
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
        c.rest_with(at.path(), named(), &[&near[0]]);
        c.filled();
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

impl Cursor {
    /// Whether a refused background mode ended the fill for the run.
    fn fill_off(&self) -> bool {
        self.w.fill_state().1
    }
}
