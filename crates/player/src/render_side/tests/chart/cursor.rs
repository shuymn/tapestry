//! The cursor's chart images on the worker, published through the render
//! thread into a selection installation (S10-12 plan D2, D3): a rested
//! request is read and published; a move drops a deferred request and
//! cancels a running read, whose set is never adopted or published; a
//! return to a chart reads it again, while the last set's own request is
//! published again without a job; a failed job publishes absent; leaving
//! selection retires its publication before play's job; and nothing
//! begins after the run's end.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use game::coord::{Caps, Coordinator, JobKind, Spawner};
use library::Name;
use skin_model::chart_image::{Absent, ChartImage};

use super::{Rig, rig_with};
use crate::chart_images::{ChartImageWorker, ChartImages, HELD_HARD, HOLD, HOLD_HARD, PANIC, Want};
use crate::render_side::tests::wait_until;

const CAPS: Caps = Caps {
    foreground: 4,
    speculative: 1,
};
/// The selection chart layer's side.
const SIDE: u32 = 2_048;

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

/// The native size of `which` in `set`, if it is an image.
fn native(set: &ChartImages, which: ChartImage) -> Option<[u32; 2]> {
    set.read(which).image.as_ref().ok().map(|f| f.native())
}

/// A worker of a render thread whose screen has a selection installation
/// with a chart layer, and the run's coordinator.
struct Cursor {
    r: Rig,
    w: ChartImageWorker,
    coord: Coordinator,
}

fn cursor() -> Cursor {
    let r = rig_with(1);
    assert!(wait_until(|| r.link.chart_now(0).selection.is_some()));
    let w = ChartImageWorker::new(r.link.chart_generation(), r.link.cursor_generation());
    Cursor {
        r,
        w,
        coord: Coordinator::new(CAPS),
    }
}

impl Cursor {
    /// The cursor rests on the chart in `dir` naming `names`.
    fn rest(&self, dir: &Path, names: [Name; 2]) {
        self.w.rest(
            self.w
                .cursor_request(dir.to_path_buf(), names, SIDE, &self.coord),
        );
    }

    /// The select screen's step of the worker.
    fn step(&self) {
        self.step_on(Spawner::SYSTEM);
    }

    fn step_on(&self, spawner: Spawner) {
        self.w
            .cursor_step(&self.coord, spawner, &self.r.link.chart_publisher());
    }

    /// The cursor's request identity changed.
    fn moved(&self) {
        self.w.cursor_changed(&self.coord);
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

    /// Steps until the selection set is current, its job ended and its
    /// publication answered; the set.
    fn shown(&self) -> Arc<ChartImages> {
        self.until(|c| c.w.current_selection().is_some() && c.w.idle() && !c.w.selection_pending());
        self.w.current_selection().unwrap()
    }

    /// The selection writes the screen saw: each set's id and which of
    /// its images were images.
    fn written(&self) -> Vec<(u64, [bool; 3])> {
        self.r.charts.lock().unwrap().selection.clone()
    }
}

/// Lets go of [`HOLD`] and [`HOLD_HARD`] when dropped, so a failing test
/// leaves no read held for the next.
struct LetGo;

impl Drop for LetGo {
    fn drop(&mut self) {
        HOLD.store(false, Ordering::Release);
        HOLD_HARD.store(false, Ordering::Release);
    }
}

/// A rested chart's 640×480 stage file and 300×80 banner are read for the
/// selection's slots in one job, each decoded once, and published into the
/// selection installation; the back image is never read there.
#[test]
fn a_rested_charts_images_are_read_and_published() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let d = chart([640, 480], [300, 80]);
    c.rest(d.path(), named());
    let set = c.shown();
    assert_eq!(native(&set, ChartImage::Stage), Some([640, 480]));
    assert_eq!(native(&set, ChartImage::Banner), Some([300, 80]));
    assert_eq!(set.read(ChartImage::Back).image, Err(Absent::NoHeader));
    assert_eq!(c.w.begun(), 1);
    assert_eq!(c.w.counts().0, 2);
    assert_eq!(c.written(), vec![(set.id(), [true, false, true])]);
    assert_eq!(c.w.selection_publications(), 1);
}

/// A request deferred behind a running read is dropped by a move: once
/// that read ended, nothing begins until the next rest, whose chart is the
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
    c.rest(a.path(), named());
    c.step();
    assert_eq!(c.w.begun(), 1);
    c.rest(b.path(), named());
    c.step();
    assert!(c.w.cursor_deferred() && c.w.cursor_waiting());
    c.moved();
    assert!(!c.w.cursor_deferred(), "the move kept the deferred request");
    HOLD.store(false, Ordering::Release);
    c.until(|c| c.w.idle());
    for _ in 0..20 {
        c.step();
    }
    assert_eq!(c.w.begun(), 1, "the dropped request began");
    assert!(c.w.current_selection().is_none());
    c.rest(n.path(), named());
    let set = c.shown();
    assert_eq!(native(&set, ChartImage::Stage), Some([300, 100]));
    assert_eq!(c.w.begun(), 2);
    assert_eq!(c.written(), vec![(set.id(), [true, false, true])]);
}

/// A move during a held read cancels it through the cursor's scope: its
/// thread ends though the read is still held, its set is never adopted or
/// published, and the next chart's set is.
#[test]
fn a_move_cancels_a_held_read() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor();
    let (a, b) = (chart([100, 100], [10, 10]), chart([200, 100], [20, 10]));
    HOLD.store(true, Ordering::Release);
    c.rest(a.path(), named());
    c.step();
    assert_eq!(c.w.begun(), 1);
    c.moved();
    c.rest(b.path(), named());
    // A's read ends on its cancel, B's begins: still held, for the read is.
    c.until(|c| c.w.begun() == 2);
    assert!(c.w.current_selection().is_none(), "A's set adopted");
    assert!(c.written().is_empty(), "A's set published");
    HOLD.store(false, Ordering::Release);
    let set = c.shown();
    assert_eq!(native(&set, ChartImage::Stage), Some([200, 100]));
    assert_eq!(c.w.counts().0, 2, "only B's files decoded");
    assert_eq!(c.written(), vec![(set.id(), [true, false, true])]);
}

/// A→B→A reads A again (the last set is the only candidate); a rest on B
/// again after a move away and back, before any other rest, publishes B's
/// set again without a job.
#[test]
fn a_return_to_a_chart_reads_it_again_and_its_own_rest_needs_no_job() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let (a, b) = (chart([100, 100], [10, 10]), chart([200, 100], [20, 10]));
    c.rest(a.path(), named());
    c.shown();
    assert_eq!(c.w.counts().0, 2);
    c.moved();
    c.rest(b.path(), named());
    c.shown();
    assert_eq!(c.w.counts().0, 4);
    c.moved();
    c.rest(a.path(), named());
    let again = c.shown();
    assert_eq!(c.w.counts().0, 6, "A not decoded again");
    assert_eq!(native(&again, ChartImage::Stage), Some([100, 100]));
    c.moved();
    c.rest(b.path(), named());
    let b_set = c.shown();
    let (begun, written) = (c.w.begun(), c.written().len());
    // To another chart and back before its rest.
    c.moved();
    c.moved();
    assert!(c.w.current_selection().is_none());
    c.rest(b.path(), named());
    let same = c.shown();
    assert!(Arc::ptr_eq(&same, &b_set));
    assert_eq!(c.w.begun(), begun, "a job began");
    assert_eq!(c.written().len(), written + 1);
    assert_eq!(c.written().last().unwrap().0, b_set.id());
}

/// The selection installation's key changes (a screen built again): the
/// next step publishes the same set again, decoding nothing and with no
/// move. With no selection installation (a skin refused at runtime) nothing
/// is published again and no job begins, until one is installed.
#[test]
fn a_new_selection_key_publishes_the_set_again() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let d = chart([640, 480], [300, 80]);
    c.rest(d.path(), named());
    let set = c.shown();
    let (begun, counts) = (c.w.begun(), c.w.counts());
    // Frames drawn, so the render thread mirrors the live key at each.
    c.r.link
        .begin_loading(&crate::render_side::tests::chosen("A"), None);
    c.r.link
        .loading()
        .show(crate::render_side::tests::one_line("A"));
    let key = |c: &Cursor| c.r.link.chart_now(0).selection.map(|k| k.installation);
    c.r.selection.store(2, Ordering::SeqCst);
    assert!(wait_until(|| key(&c) == Some(2)));
    c.until(|c| c.written().len() == 2 && !c.w.selection_pending());
    assert_eq!(c.written()[1].0, set.id());
    assert!(Arc::ptr_eq(&c.w.current_selection().unwrap(), &set));
    // No installation: nothing to publish into.
    c.r.selection.store(0, Ordering::SeqCst);
    assert!(wait_until(|| key(&c).is_none()));
    let sent = c.w.selection_publications();
    for _ in 0..20 {
        c.step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        c.w.selection_publications(),
        sent,
        "published with no target"
    );
    // Installed again: published again.
    c.r.selection.store(3, Ordering::SeqCst);
    assert!(wait_until(|| key(&c) == Some(3)));
    c.until(|c| c.written().len() == 3 && !c.w.selection_pending());
    assert_eq!((c.w.begun(), c.w.counts()), (begun, counts));
}

/// A chart naming no image is published absent at once, with no job.
#[test]
fn a_chart_naming_no_image_is_published_absent_without_a_job() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let d = tempfile::tempdir().unwrap();
    c.rest(d.path(), [Name::None, Name::None]);
    c.step();
    assert_eq!(c.w.begun(), 0);
    let set = c.shown();
    assert!(set.fitted().iter().all(Option::is_none));
    assert_eq!(c.written(), vec![(set.id(), [false; 3])]);
}

/// An overlong name resolves to nothing, never to a file its prefix names.
#[test]
fn an_overlong_name_reads_nothing() {
    let _serial = crate::imports::tests::serial();
    let c = cursor();
    let d = chart([640, 480], [300, 80]);
    c.rest(d.path(), [Name::Overlong, Name::Named("n.png".to_owned())]);
    let set = c.shown();
    assert_eq!(
        set.read(ChartImage::Stage).image,
        Err(Absent::Unresolved(
            skin_model::chart_image::Unresolved::Overlong
        ))
    );
    assert_eq!(native(&set, ChartImage::Banner), Some([300, 80]));
    assert_eq!(c.w.counts().0, 1);
}

/// A panicked job and one whose thread cannot start leave the selection's
/// images absent as failed, published so, and the run goes on.
#[test]
fn a_failed_cursor_job_publishes_the_images_absent() {
    let _serial = crate::imports::tests::serial();
    let d = chart([640, 480], [300, 80]);
    for refused in [false, true] {
        let c = cursor();
        c.rest(d.path(), named());
        if refused {
            c.step_on(Spawner::refusing_owned(&[JobKind::ChartImages]));
            assert!(c.w.idle());
        } else {
            PANIC.store(true, Ordering::Release);
        }
        let set = c.shown();
        assert_eq!(
            set.read(ChartImage::Stage).image,
            Err(Absent::Failed),
            "refused {refused}"
        );
        assert_eq!(c.w.begun(), 1);
        assert_eq!(
            c.written(),
            vec![(set.id(), [false; 3])],
            "refused {refused}"
        );
    }
}

/// Selection left while its read is held and another request waits: the
/// request goes, the read is cancelled, and the selection publication
/// retires; play's job begins only once that read's thread ended and the
/// publication was answered, and no cursor job begins after.
#[test]
fn leaving_selection_retires_its_publication_before_plays_job() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor();
    let (a, b) = (chart([100, 100], [10, 10]), chart([200, 100], [20, 10]));
    HOLD_HARD.store(true, Ordering::Release);
    c.rest(a.path(), named());
    c.step();
    // In its read, which ignores its cancel.
    assert!(wait_until(|| HELD_HARD.load(Ordering::Acquire)));
    c.rest(b.path(), named());
    c.step();
    assert!(c.w.selection_pending() && c.w.cursor_deferred());
    assert_eq!(c.w.begun(), 1);
    // The choice.
    c.w.leave_selection(&c.coord);
    assert!(!c.w.cursor_deferred(), "the deferred request kept");
    assert!(c.w.retiring(), "the selection publication not retired");
    assert!(!c.w.selection_pending());
    c.w.renew();
    c.w.attempt(Want {
        dir: b.path().to_path_buf(),
        names: ["s.png".to_owned(), String::new(), String::new()],
        play: Some(SIDE),
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
    assert_eq!(
        c.w.begun(),
        1,
        "play's job began beside the cursor's: {:?}",
        c.w
    );
    assert!(c.w.retiring());
    HOLD_HARD.store(false, Ordering::Release);
    let end = Instant::now() + Duration::from_secs(10);
    while c.w.current().is_none() || !c.w.idle() {
        assert!(Instant::now() < end, "{:?}", c.w);
        play_step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!c.w.retiring());
    assert_eq!(c.w.begun(), 2, "a cursor job began after the choice");
    assert!(c.w.current_selection().is_none());
    assert!(c.written().is_empty(), "the cancelled read was published");
}

/// The run's end drops a deferred cursor request with the rest of the
/// selection's images, and nothing begins after it.
#[test]
fn the_end_drops_a_deferred_cursor_request() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let c = cursor();
    let (a, b) = (chart([100, 100], [10, 10]), chart([200, 100], [20, 10]));
    HOLD.store(true, Ordering::Release);
    c.rest(a.path(), named());
    c.step();
    c.rest(b.path(), named());
    c.step();
    assert!(c.w.cursor_deferred());
    c.w.end();
    assert!(!c.w.cursor_deferred());
    assert!(c.w.idle());
    assert_eq!(c.w.retained(), 0);
    HOLD.store(false, Ordering::Release);
    let begun = c.w.begun();
    c.rest(a.path(), named());
    for _ in 0..10 {
        c.step();
        c.w.service();
    }
    assert_eq!(c.w.begun(), begun, "a job began after the end");
    assert!(c.w.current_selection().is_none());
}
