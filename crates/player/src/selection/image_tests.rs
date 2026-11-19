//! The cursor's chart images on the select screen (S10-12 plan D2, SC1,
//! SC4; design.md "Rendering and Skins", chart images): the request waits
//! for the cursor's rest, a change of the cursor's chart (a move, a list
//! read again with other names or another row under the cursor, an entry)
//! is a new cursor generation, a refreshed list keeping the cursor is a
//! use of its entry, the worker is stepped only in a look that chose
//! nothing, a request waiting behind a job wakes the screen within a
//! slice, and a return shows a resident row at once and reads it again,
//! with no move or rest, once its check finds it changed.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bms_model::{TimeSource as _, TimeUs};
use game::AppRig as _;
use input::QpcClock;

use super::tests::{keys, on_selection_at, over, settle};
use super::{CursorImages, REST, Selection};
use crate::chart_currency::CursorChart;
use crate::chart_images::{ChartImageWorker, EntryState, HELD_HARD, HOLD_HARD};
use crate::render_side::RenderLink;
use crate::render_side::tests::{Installers, pool_layout, selection_update, wait_until};
use crate::select::{MenuInput, MenuKey};

/// A `w`×`h` PNG at `path`.
fn png(path: &Path, [w, h]: [u32; 2]) {
    let file = std::fs::File::create(path).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels = [10, 20, 30, 255].repeat(usize::try_from(w * h).unwrap());
    e.write_header().unwrap().write_image_data(&texels).unwrap();
}

/// Writes the chart `file` under `root` naming `stage` and `banner`.
fn write(root: &Path, file: &str, stage: Option<&str>, banner: Option<&str>) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let stem = path.file_stem().unwrap().to_str().unwrap();
    let mut text = format!("#TITLE {stem}\n#BPM 120\n");
    if let Some(s) = stage {
        text.push_str(&format!("#STAGEFILE {s}\n"));
    }
    if let Some(b) = banner {
        text.push_str(&format!("#BANNER {b}\n"));
    }
    text.push_str("#00138:01\n");
    std::fs::write(&path, text).unwrap();
}

/// A library of one root holding `charts` (file, stage file, banner),
/// and its images: `hidden-a.png` 640×480, `hidden-b.png` 320×240,
/// `hidden-n.png` 300×80 and `hidden-m.png` 200×40 (the scan lists no file
/// named `hidden…`).
pub(crate) fn library(
    dir: &Path,
    charts: &[(&str, Option<&str>, Option<&str>)],
) -> (Selection, PathBuf) {
    let root = dir.join("root0");
    for (file, stage, banner) in charts {
        write(&root, file, *stage, *banner);
    }
    png(&root.join("hidden-a.png"), [640, 480]);
    png(&root.join("hidden-b.png"), [320, 240]);
    png(&root.join("hidden-n.png"), [300, 80]);
    png(&root.join("hidden-m.png"), [200, 40]);
    let (sel, _) = over(dir, vec![root.clone()]);
    (sel, root)
}

/// Starts no scan pass after the one running, for a test that looks on a
/// rig: each pass's end refreshes the list, an update, and passes ending
/// faster than the render thread's frames would leave no frame boundary
/// free for the pool's write (one per boundary, none at a boundary that
/// took a new update).
pub(crate) fn quiet(sel: &mut Selection) {
    sel.shelf.set_rescan_after(Duration::from_secs(3_600));
}

/// Screens with a selection chart-image pool of 8 slots.
fn pooled() -> Installers {
    let screens = Installers::default();
    *screens.pool.lock().unwrap() = Some(pool_layout(8));
    screens
}

/// A select screen's selection, a render link drawing a selection, whose
/// screen has a pool, and the worker its cursor asks.
struct Screen {
    sel: Selection,
    render: RenderLink,
    w: ChartImageWorker,
    _screens: Installers,
}

impl Screen {
    fn new(sel: Selection) -> Self {
        let screens = pooled();
        let render = screens.spawn_bare();
        // The render thread on selection: it applies pool writes.
        render.enter_selection(1);
        render.show_selection(selection_update(1, 1));
        assert!(wait_until(|| render.pool_now().0.is_some()));
        let w = ChartImageWorker::new(render.chart_generation());
        Self {
            sel,
            render,
            w,
            _screens: screens,
        }
    }

    fn images(&self) -> CursorImages<'_> {
        CursorImages {
            worker: &self.w,
            drawn: true,
        }
    }

    /// The update sent at `ms` ms, the cursor's chart tracked.
    fn show(&mut self, ms: i64) {
        let Self { sel, render, w, .. } = self;
        sel.dirty = true;
        let images = CursorImages {
            worker: w,
            drawn: true,
        };
        sel.show(render, images, None, TimeUs(ms * 1_000));
    }

    /// Whether a look at `ms` ms makes the cursor's request; it is handed
    /// to the worker.
    fn rest(&mut self, ms: i64) -> bool {
        let images = CursorImages {
            worker: &self.w,
            drawn: true,
        };
        let request = self.sel.rest_request(images, TimeUs(ms * 1_000));
        let made = request.is_some();
        if let Some(r) = request {
            self.w.rest(r);
        }
        made
    }

    /// Steps the worker until the cursor's row has an entry and nothing
    /// runs or waits; the entry's state.
    fn read(&self) -> EntryState {
        let end = Instant::now() + Duration::from_secs(10);
        while self.w.cursor_entry().is_none()
            || !self.w.idle()
            || self.w.write_pending()
            || self.w.checks_pending() != (false, false)
        {
            assert!(Instant::now() < end, "{:?}", self.w);
            self.w.cursor_step(
                &self.sel.coord,
                self.sel.spawner,
                &self.render.chart_publisher(),
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = self.images();
        self.w.cursor_entry().unwrap().0
    }

    /// Steps the worker until the cursor's request is served, its fill
    /// has ended and nothing runs or waits, taking the changes of the
    /// cursor's value.
    fn filled(&self) {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let _ = self.w.take_changed();
            if !self.w.cursor_waiting() && self.w.idle() {
                break;
            }
            assert!(Instant::now() < end, "{:?}", self.w);
            self.w.cursor_step(
                &self.sel.coord,
                self.sel.spawner,
                &self.render.chart_publisher(),
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// The cursor's request is made once it rested 150 ms on a chart, not
/// before, and once per rest; the stage file and banner are both read for
/// it, into a slot of the pool.
#[test]
fn the_cursors_request_waits_for_its_rest() {
    let _serial = crate::imports::tests::serial();
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = library(
        dir.path(),
        &[("a.bms", Some("hidden-a.png"), Some("hidden-n.png"))],
    );
    let mut s = Screen::new(sel);
    s.show(0);
    keys(&mut s.sel, &[MenuKey::Down]);
    s.show(10);
    assert!(!s.rest(110), "requested 100 ms after the cursor came");
    assert!(!s.rest(159));
    assert!(s.rest(160));
    assert!(!s.rest(400), "requested twice in one rest");
    assert!(matches!(s.read(), EntryState::Resident { .. }));
    assert_eq!(s.w.counts().0, 2);
    assert_eq!(s.w.begun(), 1);
}

/// A move before the rest makes no request: the rest counts from the move,
/// and its request is the new chart's.
#[test]
fn a_move_before_the_rest_makes_no_request() {
    let _serial = crate::imports::tests::serial();
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = library(
        dir.path(),
        &[
            ("a.bms", Some("hidden-a.png"), None),
            ("b.bms", Some("hidden-b.png"), None),
        ],
    );
    let mut s = Screen::new(sel);
    keys(&mut s.sel, &[MenuKey::Down]);
    s.show(0);
    let generation = s.w.cursor_generation();
    keys(&mut s.sel, &[MenuKey::Down]);
    s.show(140);
    assert!(s.w.cursor_generation() > generation);
    assert!(!s.rest(150), "requested for the chart left");
    assert!(!s.rest(289));
    assert!(s.rest(290));
    assert!(matches!(s.read(), EntryState::Resident { .. }));
    // b's read, then the fill's of a, its neighbour.
    s.filled();
    assert_eq!(s.w.begun(), 2);
    assert_eq!(s.w.counts().0, 2);
}

/// A row that is no chart asks for nothing and is absent; a chart naming
/// no image is absent at once and an absent entry at its rest, with no
/// job.
#[test]
fn a_row_with_no_chart_asks_for_nothing() {
    let _serial = crate::imports::tests::serial();
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = library(dir.path(), &[("a.bms", None, None)]);
    let mut s = Screen::new(sel);
    // The all-charts row.
    s.show(0);
    assert!(s.sel.screen.selected().is_none());
    assert_eq!(s.w.sent_chart(), Some(CursorChart::Absent));
    assert!(!s.rest(1_000));
    keys(&mut s.sel, &[MenuKey::Down]);
    s.show(1_000);
    assert_eq!(s.w.sent_chart(), Some(CursorChart::Absent));
    assert!(s.rest(1_150));
    assert_eq!(s.read(), EntryState::Absent);
    assert_eq!(s.w.begun(), 0);
}

/// The list read again: with the cursor's chart and its names unchanged
/// the cursor generation stays and the refresh is a use, checked once;
/// with its banner renamed on the same path the cursor generation changes,
/// the update is pending, and the new banner is read; with the cursor's
/// chart gone, the row under the cursor changes as a move does.
#[test]
fn a_rescan_changes_the_request_only_with_its_chart() {
    let _serial = crate::imports::tests::serial();
    let dir = tempfile::tempdir().unwrap();
    let (sel, root) = library(
        dir.path(),
        &[
            ("a.bms", Some("hidden-a.png"), Some("hidden-n.png")),
            ("b.bms", None, None),
        ],
    );
    let mut s = Screen::new(sel);
    keys(&mut s.sel, &[MenuKey::Down]);
    s.show(0);
    assert!(s.rest(150));
    assert!(matches!(s.read(), EntryState::Resident { .. }));
    // Its adoption's update: a use, checked.
    s.show(160);
    s.read();
    let (generation, checks) = (s.w.cursor_generation(), s.w.checks_begun());
    // An ordinary update is no use.
    s.show(170);
    s.read();
    assert_eq!(s.w.checks_begun(), checks, "an ordinary update checked");
    // Scanned again, unchanged: the same request, a use.
    settle(&mut s.sel);
    s.show(200);
    assert_eq!(s.w.cursor_generation(), generation);
    assert!(!s.rest(400));
    assert!(matches!(
        s.w.sent_chart(),
        Some(CursorChart::Resident { .. })
    ));
    s.read();
    assert_eq!(s.w.checks_begun(), checks + 1, "the refresh not checked");
    // The banner renamed.
    std::thread::sleep(Duration::from_millis(20));
    write(&root, "a.bms", Some("hidden-a.png"), Some("hidden-m.png"));
    settle(&mut s.sel);
    s.show(500);
    assert!(s.w.cursor_generation() > generation, "the old banner kept");
    assert_eq!(s.w.sent_chart(), Some(CursorChart::Pending));
    assert!(s.rest(650));
    let decodes = s.w.counts().0;
    assert!(matches!(s.read(), EntryState::Resident { .. }));
    assert_eq!(s.w.counts().0, decodes + 2);
    // The chart gone: the cursor is on another row.
    let generation = s.w.cursor_generation();
    std::fs::remove_file(root.join("a.bms")).unwrap();
    settle(&mut s.sel);
    s.show(700);
    assert!(s.w.cursor_generation() > generation);
    assert_ne!(
        s.w.sent_chart()
            .map(|c| matches!(c, CursorChart::Resident { .. })),
        Some(true)
    );
}

/// After the cursor's rest the fill reads the charts near it on the list,
/// nearest first around the ring, each folder's names once (AC-S3): the
/// cursor on the third of six charts, its row takes slot 0 and its
/// neighbours the next slots in the order +1, −1, +2, −2, +3; a chart
/// naming no image is passed over, and a list read again keeping the
/// cursor reads a chart that came near it.
#[test]
fn the_fill_reads_the_charts_near_the_cursor_nearest_first() {
    let _serial = crate::imports::tests::serial();
    let dir = tempfile::tempdir().unwrap();
    let names = [
        ("c0.bms", Some("hidden-a.png"), Some("hidden-n.png")),
        ("c1.bms", Some("hidden-a.png"), Some("hidden-m.png")),
        ("c2.bms", Some("hidden-b.png"), Some("hidden-n.png")),
        ("c3.bms", Some("hidden-b.png"), Some("hidden-m.png")),
        ("c4.bms", Some("hidden-a.png"), None),
        ("c5.bms", None, Some("hidden-n.png")),
        ("c6.bms", None, None),
    ];
    let (sel, root) = library(dir.path(), &names);
    let mut s = Screen::new(sel);
    let id = |i: usize| {
        let (_, stage, banner) = names[i];
        let name =
            |n: Option<&str>| n.map_or(library::Name::None, |n| library::Name::Named(n.to_owned()));
        crate::chart_images::CursorId {
            dir: root.clone(),
            names: [name(stage), name(banner)],
        }
    };
    let slot = |s: &Screen, i: usize| match s.w.entry(&id(i)).map(|e| e.0) {
        Some(EntryState::Resident { slot, .. }) => Some(slot),
        _ => None,
    };
    // The all-charts row, then c0 to c6: the cursor on c2.
    keys(&mut s.sel, &[MenuKey::Down, MenuKey::Down, MenuKey::Down]);
    s.show(0);
    assert_eq!(
        s.sel.screen.selected().map(|c| c.path.clone()),
        Some(root.join("c2.bms"))
    );
    assert!(s.rest(150));
    s.filled();
    // c2, then c3, c1, c4, c0, c5; c6 names no image (the ring's +4 is
    // the all-charts row, past half of eight rows).
    let order: Vec<Option<u16>> = [2, 3, 1, 4, 0, 5].iter().map(|&i| slot(&s, i)).collect();
    assert_eq!(order, (0..6).map(Some).collect::<Vec<_>>());
    assert!(s.w.entry(&id(6)).is_none());
    assert_eq!(s.w.begun(), 6);
    // A chart that came near the cursor, read at the list's refresh.
    write(&root, "c21.bms", Some("hidden-b.png"), None);
    settle(&mut s.sel);
    s.show(400);
    s.filled();
    let new = crate::chart_images::CursorId {
        dir: root.clone(),
        names: [
            library::Name::Named("hidden-b.png".to_owned()),
            library::Name::None,
        ],
    };
    assert!(matches!(
        s.w.entry(&new).map(|e| e.0),
        Some(EntryState::Resident { slot: 6, .. })
    ));
    assert_eq!(s.w.begun(), 7, "a filled row read again");
}

/// Lets go of [`HOLD_HARD`] when dropped.
struct LetGo;

impl Drop for LetGo {
    fn drop(&mut self) {
        HOLD_HARD.store(false, Ordering::Release);
    }
}

/// Pushes `key` on the select screen's queue.
pub(crate) fn press(rig: &mut crate::play::PlayerRig<'_>, key: MenuKey) {
    rig.selection
        .as_ref()
        .unwrap()
        .menu
        .push(MenuInput::Key(key));
}

/// The rest past, with a little to spare.
pub(crate) fn past_rest() {
    std::thread::sleep(Duration::from_micros(
        u64::try_from(REST.0).unwrap() + 30_000,
    ));
}

/// Looks until `done`, at most 10 s.
pub(crate) fn look_until(
    rig: &mut crate::play::PlayerRig<'_>,
    done: impl Fn(&ChartImageWorker) -> bool,
) {
    let end = Instant::now() + Duration::from_secs(10);
    while !done(&rig.chart_images) {
        assert!(Instant::now() < end, "{:?}", rig.chart_images);
        assert!(rig.choose().is_none());
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Whether the worker has settled with the cursor's row resident: no
/// read, write or check runs or waits.
pub(crate) fn resident(w: &ChartImageWorker) -> bool {
    matches!(w.cursor_entry(), Some((EntryState::Resident { .. }, ..)))
        && w.idle()
        && !w.write_pending()
        && w.checks_pending() == (false, false)
}

/// The select screen of two charts with images, on a rig whose selection
/// skin draws them into a pool and whose clock a test may hold.
fn on_charts<R>(f: impl FnOnce(&mut crate::play::PlayerRig<'_>) -> R) -> R {
    let dir = tempfile::tempdir().unwrap();
    let (mut sel, _) = library(
        dir.path(),
        &[
            ("a.bms", Some("hidden-a.png"), Some("hidden-n.png")),
            ("b.bms", Some("hidden-b.png"), None),
        ],
    );
    quiet(&mut sel);
    let screens = pooled();
    let render = screens.spawn_bare();
    on_selection_at(sel, &screens, render, QpcClock::holdable(), |rig| {
        rig.selection_chart = true;
        f(rig)
    })
}

/// A chart chosen in the look whose rest came due: no cursor request is
/// made and no job begins in it; the next look, choosing nothing, makes
/// the request.
#[test]
fn a_choice_in_the_look_of_a_rest_begins_no_cursor_job() {
    let _serial = crate::imports::tests::serial();
    on_charts(|rig| {
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        past_rest();
        press(rig, MenuKey::Enter);
        assert!(rig.choose().is_some(), "nothing chosen");
        assert_eq!(rig.chart_images.begun(), 0, "a cursor job began");
        assert!(!rig.chart_images.cursor_deferred());
        // With no choice, the same rest asks.
        assert!(rig.choose().is_none());
        assert_eq!(rig.chart_images.begun(), 1);
    });
}

/// A request waiting for a cancelled read's thread wakes the select screen
/// within a slice, and begins once that thread ended, with no input.
#[test]
fn a_deferred_request_wakes_the_select_screen_within_a_slice() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    on_charts(|rig| {
        HOLD_HARD.store(true, Ordering::Release);
        // Held: the move's look and the wake read one time, however long
        // the look took.
        let moved = rig.clock.hold();
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        // Before the rest, the wake is the rest.
        let rest = rig.selection_wake().unwrap();
        assert_eq!(rest.0, moved.0 + REST.0, "{rest:?} {moved:?}");
        rig.clock.set(rest);
        assert!(rig.choose().is_none());
        assert_eq!(rig.chart_images.begun(), 1);
        assert!(wait_until(|| HELD_HARD.load(Ordering::Acquire)));
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        rig.clock.set(TimeUs(rest.0 + REST.0));
        assert!(rig.choose().is_none());
        assert!(rig.chart_images.cursor_waiting());
        let wake = rig
            .selection_wake()
            .expect("no wake for the waiting request");
        let after = rig.clock.now();
        let slice = i64::try_from(crate::play::IMAGE_SLICE.as_micros()).unwrap();
        assert!(wake.0 <= after.0 + slice, "{wake:?} {after:?}");
        HOLD_HARD.store(false, Ordering::Release);
        rig.clock.run();
        look_until(rig, |w| w.begun() >= 2);
        look_until(rig, resident);
        // B's stage file alone, then the fill's read of A, its neighbour:
        // A's stage file and banner.
        look_until(rig, |w| w.begun() == 3 && w.idle() && !w.cursor_waiting());
        assert_eq!(rig.chart_images.counts().0, 3);
    });
}

/// Back on selection with the cursor on the same chart, its stage file
/// replaced meanwhile (P-i, AC-S4, AC-S17): the entry's first update names
/// its slot at once, with no read; the check of that use finds the file
/// changed, withdraws the row (the next update pending), and its read is
/// asked again with no move and no new rest, the clock held.
#[test]
fn a_return_shows_the_row_at_once_and_reads_it_again_once_changed() {
    let _serial = crate::imports::tests::serial();
    let dir = tempfile::tempdir().unwrap();
    let (mut sel, root) = library(
        dir.path(),
        &[("a.bms", Some("hidden-a.png"), Some("hidden-n.png"))],
    );
    quiet(&mut sel);
    let screens = pooled();
    let render = screens.spawn_bare();
    on_selection_at(sel, &screens, render, QpcClock::holdable(), |rig| {
        rig.selection_chart = true;
        // Held: the rest is reached by moving the clock, and the entry and
        // the looks after it read the times set, however long they took.
        let moved = rig.clock.hold();
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        rig.clock.set(TimeUs(moved.0 + REST.0));
        look_until(rig, resident);
        let (decodes, begun) = (rig.chart_images.counts().0, rig.chart_images.begun());
        assert_eq!(decodes, 2);
        let Some((EntryState::Resident { generation, slot }, ..)) = rig.chart_images.cursor_entry()
        else {
            panic!("{:?}", rig.chart_images);
        };
        // Away (the departure); the stage file replaced meanwhile.
        rig.left_selection();
        std::thread::sleep(Duration::from_millis(20));
        png(&root.join("hidden-a.png"), [160, 120]);
        let checks = rig.chart_images.checks_begun();
        rig.entered_selection();
        // The entry's update names the slot at once, with no read.
        assert_eq!(
            rig.chart_images.sent_chart(),
            Some(CursorChart::Resident { generation, slot })
        );
        assert_eq!(rig.chart_images.begun(), begun, "read at the entry");
        // The check of the entry's use withdraws it.
        look_until(rig, |w| w.cursor_entry().is_none());
        assert!(rig.chart_images.checks_begun() > checks);
        assert_eq!(rig.chart_images.sent_chart(), Some(CursorChart::Pending));
        // Read again, the clock still held: no new rest.
        look_until(rig, resident);
        assert_eq!(rig.chart_images.begun(), begun + 1);
        assert_eq!(rig.chart_images.counts().0, decodes + 2);
    });
}

/// A still cursor whose entry a check withdraws is read again at the next
/// look, the clock held at its first rest's end: the withdrawal counts
/// the rest as served, with no move, no entry and no new rest (AC-S17).
/// The use is a list read again with the cursor kept.
#[test]
fn a_withdrawn_entry_under_a_still_cursor_is_read_again_at_once() {
    let _serial = crate::imports::tests::serial();
    let dir = tempfile::tempdir().unwrap();
    let (mut sel, root) = library(
        dir.path(),
        &[("a.bms", Some("hidden-a.png"), Some("hidden-n.png"))],
    );
    quiet(&mut sel);
    let screens = pooled();
    let render = screens.spawn_bare();
    on_selection_at(sel, &screens, render, QpcClock::holdable(), |rig| {
        rig.selection_chart = true;
        let moved = rig.clock.hold();
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        rig.clock.set(TimeUs(moved.0 + REST.0));
        look_until(rig, resident);
        let (decodes, begun) = (rig.chart_images.counts().0, rig.chart_images.begun());
        let checks = rig.chart_images.checks_begun();
        // The stage file replaced, then the list read again with the
        // cursor kept: a use of its entry.
        std::thread::sleep(Duration::from_millis(20));
        png(&root.join("hidden-a.png"), [160, 120]);
        let sel = rig.selection.as_mut().unwrap();
        sel.shelf.set_rescan_after(Duration::ZERO);
        settle(sel);
        quiet(sel);
        look_until(rig, |w| w.cursor_entry().is_none());
        assert!(rig.chart_images.checks_begun() > checks);
        // Read again with the clock still held: no new rest.
        look_until(rig, resident);
        assert_eq!(rig.chart_images.begun(), begun + 1);
        assert_eq!(rig.chart_images.counts().0, decodes + 2);
    });
}
