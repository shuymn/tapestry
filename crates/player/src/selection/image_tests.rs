//! The cursor's chart images on the select screen (S10-12 plan D2, SC1,
//! SC4): the request waits for the cursor's rest, a change of the cursor's
//! chart (a move, a list read again with other names or another row under
//! the cursor, an entry) is a new cursor generation, the worker is stepped
//! only in a look that chose nothing, and a request waiting behind a job
//! wakes the screen within a slice.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bms_model::{TimeSource as _, TimeUs};
use game::AppRig as _;
use skin_model::chart_image::ChartImage;

use super::tests::{keys, on_selection, over, settle};
use super::{CursorImages, REST, Selection};
use crate::chart_images::{ChartImageWorker, ChartImages, HELD_HARD, HOLD_HARD};
use crate::render_side::RenderLink;
use crate::render_side::tests::{Installers, wait_until};
use crate::select::{MenuInput, MenuKey};

/// The selection chart layer's side.
const SIDE: u32 = 2_048;

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

/// The native size of `which` in `set`, if it is an image.
fn native(set: &ChartImages, which: ChartImage) -> Option<[u32; 2]> {
    set.read(which).image.as_ref().ok().map(|f| f.native())
}

/// A select screen's selection, a render link to show it on, and the
/// worker its cursor asks.
struct Screen {
    sel: Selection,
    render: RenderLink,
    w: ChartImageWorker,
    _screens: Installers,
}

impl Screen {
    fn new(sel: Selection) -> Self {
        let screens = Installers::default();
        let render = screens.spawn_bare();
        let w = ChartImageWorker::new(render.chart_generation(), render.cursor_generation());
        Self {
            sel,
            render,
            w,
            _screens: screens,
        }
    }

    /// The update sent at `ms` ms, the cursor's chart tracked.
    fn show(&mut self, ms: i64) {
        let Self { sel, render, w, .. } = self;
        sel.dirty = true;
        let images = CursorImages {
            worker: w,
            side: Some(SIDE),
        };
        sel.show(render, images, None, TimeUs(ms * 1_000));
    }

    /// Whether a look at `ms` ms makes the cursor's request; it is handed
    /// to the worker.
    fn rest(&mut self, ms: i64) -> bool {
        let images = CursorImages {
            worker: &self.w,
            side: Some(SIDE),
        };
        let request = self.sel.rest_request(images, TimeUs(ms * 1_000));
        let made = request.is_some();
        if let Some(r) = request {
            self.w.rest(r);
        }
        made
    }

    /// Steps the worker until the selection set is current and its job
    /// ended; the set.
    fn read(&self) -> std::sync::Arc<ChartImages> {
        let end = Instant::now() + Duration::from_secs(10);
        while self.w.current_selection().is_none() || !self.w.idle() {
            assert!(Instant::now() < end, "{:?}", self.w);
            self.w.cursor_step(
                &self.sel.coord,
                self.sel.spawner,
                &self.render.chart_publisher(),
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        self.w.current_selection().unwrap()
    }
}

/// The cursor's request is made once it rested 150 ms on a chart, not
/// before, and once per rest; the 640×480 stage file and 300×80 banner are
/// read for it.
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
    let set = s.read();
    assert_eq!(native(&set, ChartImage::Stage), Some([640, 480]));
    assert_eq!(native(&set, ChartImage::Banner), Some([300, 80]));
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
    let set = s.read();
    assert_eq!(native(&set, ChartImage::Stage), Some([320, 240]));
    assert_eq!(s.w.begun(), 1);
}

/// A row that is no chart asks for nothing; a chart naming no image is
/// published absent with no job.
#[test]
fn a_row_with_no_chart_asks_for_nothing() {
    let _serial = crate::imports::tests::serial();
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = library(dir.path(), &[("a.bms", None, None)]);
    let mut s = Screen::new(sel);
    // The all-charts row.
    s.show(0);
    assert!(s.sel.screen.selected().is_none());
    assert!(!s.rest(1_000));
    keys(&mut s.sel, &[MenuKey::Down]);
    s.show(1_000);
    assert!(s.rest(1_150));
    let set = s.read();
    assert!(set.fitted().iter().all(Option::is_none));
    assert_eq!(s.w.begun(), 0);
}

/// The list read again: with the cursor's chart and its names unchanged
/// nothing changes; with its banner renamed on the same path the cursor
/// generation changes and the new banner is read; with the cursor's chart
/// gone, the row under the cursor changes as a move does.
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
    assert_eq!(native(&s.read(), ChartImage::Banner), Some([300, 80]));
    let generation = s.w.cursor_generation();
    // Scanned again, unchanged.
    settle(&mut s.sel);
    s.show(200);
    assert_eq!(s.w.cursor_generation(), generation);
    assert!(!s.rest(400));
    assert!(s.w.current_selection().is_some());
    // The banner renamed.
    std::thread::sleep(Duration::from_millis(20));
    write(&root, "a.bms", Some("hidden-a.png"), Some("hidden-m.png"));
    settle(&mut s.sel);
    s.show(500);
    assert!(s.w.cursor_generation() > generation, "the old banner kept");
    assert!(s.w.current_selection().is_none());
    assert!(s.rest(650));
    assert_eq!(native(&s.read(), ChartImage::Banner), Some([200, 40]));
    // The chart gone: the cursor is on another row.
    let generation = s.w.cursor_generation();
    std::fs::remove_file(root.join("a.bms")).unwrap();
    settle(&mut s.sel);
    s.show(700);
    assert!(s.w.cursor_generation() > generation);
    assert!(s.w.current_selection().is_none());
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

/// The select screen of two charts with images, on a rig whose selection
/// skin keeps a chart layer.
fn on_charts<R>(f: impl FnOnce(&mut crate::play::PlayerRig<'_>) -> R) -> R {
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = library(
        dir.path(),
        &[
            ("a.bms", Some("hidden-a.png"), Some("hidden-n.png")),
            ("b.bms", Some("hidden-b.png"), None),
        ],
    );
    let screens = Installers::default();
    let render = screens.spawn_bare();
    on_selection(sel, &screens, render, |rig| {
        rig.selection_chart = Some(SIDE);
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
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        // Before the rest, the wake is the rest.
        let rest = rig.selection_wake().unwrap();
        assert!(rest.0 > rig.clock.now().0 + REST.0 / 2, "{rest:?}");
        past_rest();
        assert!(rig.choose().is_none());
        assert_eq!(rig.chart_images.begun(), 1);
        assert!(wait_until(|| HELD_HARD.load(Ordering::Acquire)));
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        past_rest();
        assert!(rig.choose().is_none());
        assert!(rig.chart_images.cursor_waiting());
        let wake = rig
            .selection_wake()
            .expect("no wake for the waiting request");
        let after = rig.clock.now();
        let slice = i64::try_from(crate::play::IMAGE_SLICE.as_micros()).unwrap();
        assert!(wake.0 <= after.0 + slice, "{wake:?} {after:?}");
        HOLD_HARD.store(false, Ordering::Release);
        look_until(rig, |w| w.begun() == 2);
        look_until(rig, |w| w.current_selection().is_some());
        let set = rig.chart_images.current_selection().unwrap();
        assert_eq!(native(&set, ChartImage::Stage), Some([320, 240]));
    });
}

/// Back on selection with the cursor on the same chart: the cursor
/// generation is new, so the last visit's table is not current; the
/// request comes only after the rest counted from the entry; and the stage
/// file replaced meanwhile and the unchanged banner are both read again.
#[test]
fn a_return_reads_the_chart_again_after_its_rest() {
    let _serial = crate::imports::tests::serial();
    let dir = tempfile::tempdir().unwrap();
    let (sel, root) = library(
        dir.path(),
        &[("a.bms", Some("hidden-a.png"), Some("hidden-n.png"))],
    );
    let screens = Installers::default();
    let render = screens.spawn_bare();
    on_selection(sel, &screens, render, |rig| {
        rig.selection_chart = Some(SIDE);
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        past_rest();
        look_until(rig, |w| w.current_selection().is_some() && w.idle());
        let (decodes, begun) = (rig.chart_images.counts().0, rig.chart_images.begun());
        assert_eq!(decodes, 2);
        let generation = rig.chart_images.cursor_generation();
        // Away (the entry alone is checked here, so the departure's own
        // new cursor generation is left out); the stage file replaced
        // meanwhile.
        std::thread::sleep(Duration::from_millis(20));
        png(&root.join("hidden-a.png"), [160, 120]);
        let entered = rig.clock.now();
        rig.entered_selection();
        assert!(rig.chart_images.cursor_generation() > generation);
        assert!(rig.chart_images.current_selection().is_none());
        assert!(rig.choose().is_none());
        assert_eq!(rig.chart_images.begun(), begun, "requested at the entry");
        let wake = rig.selection_wake().unwrap();
        assert!(wake.0 >= entered.0 + REST.0, "{wake:?} {entered:?}");
        past_rest();
        look_until(rig, |w| w.current_selection().is_some() && w.idle());
        assert_eq!(rig.chart_images.counts().0, decodes + 2);
        let set = rig.chart_images.current_selection().unwrap();
        assert_eq!(native(&set, ChartImage::Stage), Some([160, 120]));
    });
}
