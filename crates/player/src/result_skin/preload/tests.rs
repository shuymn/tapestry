//! The result skin's preload on a real coordinator and shelf: each pack
//! is held at its build's checkpoint until the test lets it go, never by
//! timing. A wait for something that never happens fails its test by
//! name after `TAPESTRY_TEST_WATCHDOG` seconds.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use game::coord::{Caps, Class, Coordinator, FOREGROUND_PERMITS};
use skin_model::AtlasId;

use crate::priority::{PriorityError, ThreadMode};
use crate::result_skin::tests::{LIMITS, pending, runtime_drawn_skin};
use crate::result_skin::{ResultSkin, ResultStart, builds};
use crate::shelf::{PAUSE_SLICE, Paused, Shelf};

/// The gate a pack's checkpoint waits at: 0 open, 1 held until the pack
/// should stop, 2 held whatever it is told.
static GATE: AtomicU8 = AtomicU8::new(0);
/// Whether a pack reached the armed gate.
static REACHED: AtomicBool = AtomicBool::new(false);

/// A pack's checkpoint, before the coordinator's: it waits at the armed
/// [`GATE`], `stop` saying whether it should stop.
pub(super) fn pack_gate(stop: &dyn Fn() -> bool) {
    if GATE.load(SeqCst) == 0 {
        return;
    }
    REACHED.store(true, SeqCst);
    loop {
        match GATE.load(SeqCst) {
            0 => return,
            1 if stop() => return,
            _ => std::thread::park_timeout(Duration::from_millis(1)),
        }
    }
}

/// Arms the gate at `level` (0 opens it).
fn gate(level: u8) {
    if level > 0 && GATE.load(SeqCst) == 0 {
        REACHED.store(false, SeqCst);
    }
    GATE.store(level, SeqCst);
}

/// The gate opens when the test ends, however it ends.
struct Opened;

impl Drop for Opened {
    fn drop(&mut self) {
        gate(0);
    }
}

fn long() -> Duration {
    let secs = std::env::var("TAPESTRY_TEST_WATCHDOG").ok();
    Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(600))
}

/// Waits until `done`, failing the test past [`long`].
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < long(), "{what} never happened");
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

/// A thread mode that records each entry by thread name and refuses
/// them when told.
#[derive(Default)]
struct Seam {
    refuse: AtomicBool,
    entered: Mutex<Vec<String>>,
}

impl ThreadMode for Seam {
    fn enter_background(&self) -> Result<(), PriorityError> {
        let name = std::thread::current().name().unwrap_or("").to_owned();
        self.entered.lock().unwrap().push(name);
        if self.refuse.load(SeqCst) {
            return Err(PriorityError("refused (test)".to_owned()));
        }
        Ok(())
    }

    fn leave_background(&self) -> Result<(), PriorityError> {
        Ok(())
    }
}

fn coordinator(speculative: usize) -> Coordinator {
    Coordinator::new(Caps {
        foreground: FOREGROUND_PERMITS,
        speculative,
    })
}

/// A result skin whose startup probe read the viewed result, imported
/// kept, waiting for its pack, its packs' thread mode `seam`.
fn preloaded(dir: &std::path::Path, seam: &Arc<Seam>) -> ResultSkin {
    let p = pending(dir, LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
    rs.set_kept_probe(true);
    let outcome = crate::imports::result_probe(&p, rs.scripts(), true, || false).unwrap();
    rs.set_preload(outcome);
    rs.set_preload_mode(Arc::clone(seam) as Arc<dyn ThreadMode>);
    assert_eq!(rs.preload_state(), (Some(true), None), "the probe waits");
    rs
}

/// One look: the pack collected, and begun when it may.
fn look(rs: &mut ResultSkin, coord: &Coordinator, may_run: bool) {
    if rs.poll_preload(coord, may_run) {
        rs.begin_preload(coord);
    }
}

/// Looks until the preload holds no pack, collected, beginning none.
fn collected(rs: &mut ResultSkin, coord: &Coordinator) {
    until("the pack's collection", || {
        let _ = rs.poll_preload(coord, true);
        rs.preload_state().1.is_none()
    });
}

/// A shelf with no charts, its scan and background measurement quiet.
fn shelf(dir: &std::path::Path) -> Shelf {
    let (library, _) = crate::shelf::open(&game::DataRoot::new(dir.join("data"))).unwrap();
    Shelf::with_options(
        library,
        Vec::new(),
        Duration::from_secs(60),
        library::ScanOptions::default(),
    )
}

/// The pack runs as a speculative task on its own thread, which entered
/// background mode through the priority seam before it; it packs the
/// startup probe once and the attempt then finds the image prepared.
#[test]
fn the_pack_runs_speculative_in_background_mode_and_packs_once() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    let before = builds();
    gate(1);
    look(&mut rs, &coord, true);
    until("the pack's checkpoint", || REACHED.load(SeqCst));
    assert_eq!(coord.held(Class::Speculative), 1);
    assert_eq!(coord.held(Class::Foreground), 0);
    assert_eq!(*seam.entered.lock().unwrap(), ["result-preload"]);
    gate(0);
    collected(&mut rs, &coord);
    assert_eq!(rs.preload_state(), (None, None), "the warm-up ended");
    assert!(rs.image().is_some(), "the packed atlas is the image entry");
    assert_eq!(builds(), before + 1);
    // No pack again in the installation.
    look(&mut rs, &coord, true);
    assert_eq!(rs.preload_state(), (None, None));
    assert_eq!(builds(), before + 1);
}

/// The pause: the shelf's wait before READY returns only once the pack
/// has left its build, and the release begins a new job that packs
/// from the start.
#[test]
fn a_pause_is_acknowledged_by_the_pack_and_a_release_packs_again() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let data = tempfile::tempdir().unwrap();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let mut shelf = shelf(data.path());
    shelf.set_preload(rs.preload_control().unwrap());
    let coord = coordinator(1);
    let before = builds();
    // Held whatever it is told: the wait cannot be acknowledged.
    gate(2);
    look(&mut rs, &coord, true);
    until("the pack's checkpoint", || REACHED.load(SeqCst));
    let (_, Some(first)) = rs.preload_state() else {
        panic!("no pack runs");
    };
    shelf.pause();
    let mut asked = 0;
    let paused = shelf.wait_paused(
        PAUSE_SLICE,
        || {
            asked += 1;
            asked == 3
        },
        Duration::MAX,
        || {},
    );
    assert_eq!(paused, Paused::Cancelled, "the pack has not acknowledged");
    // Let go, it sees the pause at its checkpoint.
    gate(1);
    let began = Instant::now();
    let paused = shelf.wait_paused(
        PAUSE_SLICE,
        || began.elapsed() > long(),
        Duration::MAX,
        || {},
    );
    assert_ne!(paused, Paused::Cancelled);
    // Collected, it waits; nothing begins while the pause holds.
    collected(&mut rs, &coord);
    assert_eq!(rs.preload_state(), (Some(true), None));
    look(&mut rs, &coord, true);
    assert_eq!(rs.preload_state(), (Some(true), None));
    assert_eq!(builds(), before + 1);
    // The release: a new job, from the start.
    shelf.release();
    gate(0);
    look(&mut rs, &coord, true);
    let (_, Some(second)) = rs.preload_state() else {
        panic!("no pack after the release");
    };
    assert_ne!(first, second);
    collected(&mut rs, &coord);
    assert_eq!(rs.preload_state(), (None, None));
    assert!(rs.image().is_some());
    assert_eq!(builds(), before + 2);
}

/// A pack's outcome that comes after its probe's mode was dropped is not
/// restored: the image state stays empty and the preload ends.
#[test]
fn a_late_outcome_of_a_dropped_mode_is_not_restored() {
    let _s = crate::imports::tests::serial();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    look(&mut rs, &coord, true);
    let control = rs.preload_control().unwrap();
    assert!(control.wait_ended(long()), "the pack's thread ends");
    // Its outcome is in, not collected; the mode changes first.
    rs.set_kept_probe(false);
    assert_eq!(rs.preload_state(), (None, None));
    assert!(rs.image().is_none(), "nothing of the pack was restored");
    look(&mut rs, &coord, true);
    assert_eq!(rs.preload_state(), (None, None));
}

/// With no speculative permit (speculation off), no pack begins.
#[test]
fn with_speculation_off_no_pack_begins() {
    let _s = crate::imports::tests::serial();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(0);
    let before = builds();
    look(&mut rs, &coord, true);
    assert_eq!(rs.preload_state(), (Some(true), None));
    assert!(seam.entered.lock().unwrap().is_empty());
    assert_eq!(builds(), before);
}

/// Two result atlases live: the pack does not build, the image state
/// stays where it was and the preload waits; once one goes, it packs.
#[test]
fn two_live_result_atlases_keep_the_pack_waiting() {
    let _s = crate::imports::tests::serial();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    let before = builds();
    rs.note_retained(&[AtlasId(u64::MAX - 1), AtlasId(u64::MAX)]);
    look(&mut rs, &coord, true);
    assert_eq!(rs.preload_state(), (Some(true), None));
    assert!(rs.image().is_none());
    assert_eq!(builds(), before);
    rs.note_retained(&[AtlasId(u64::MAX)]);
    look(&mut rs, &coord, true);
    collected(&mut rs, &coord);
    assert_eq!(rs.preload_state(), (None, None));
    assert_eq!(builds(), before + 1);
}

/// While the practice page's jobs want the room, a running pack is
/// stopped and none begins; once they leave it, it packs.
#[test]
fn the_practice_page_keeps_the_pack_stopped() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    let before = builds();
    gate(1);
    look(&mut rs, &coord, true);
    until("the pack's checkpoint", || REACHED.load(SeqCst));
    // A pass of the page whose bank runs: stopped and collected.
    look(&mut rs, &coord, false);
    collected(&mut rs, &coord);
    assert_eq!(rs.preload_state(), (Some(true), None));
    look(&mut rs, &coord, false);
    assert_eq!(rs.preload_state(), (Some(true), None));
    assert_eq!(builds(), before + 1);
    gate(0);
    look(&mut rs, &coord, true);
    collected(&mut rs, &coord);
    assert_eq!(rs.preload_state(), (None, None));
    assert_eq!(builds(), before + 2);
}

/// The installation's end stops a pack held mid-build and waits for its
/// thread, which has ended once the drop returns.
#[test]
fn the_end_joins_a_running_pack() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    gate(1);
    look(&mut rs, &coord, true);
    until("the pack's checkpoint", || REACHED.load(SeqCst));
    let control = rs.preload_control().unwrap();
    drop(rs);
    assert!(control.wait_ended(Duration::ZERO), "its thread ended");
    assert_eq!(coord.held(Class::Speculative), 0);
}

/// A refused background mode: the pack's thread says so and packs
/// nothing, and no speculative pack begins again for the run.
#[test]
fn a_refused_background_mode_begins_no_pack_again() {
    let _s = crate::imports::tests::serial();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    seam.refuse.store(true, SeqCst);
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    let before = builds();
    look(&mut rs, &coord, true);
    collected(&mut rs, &coord);
    assert_eq!(rs.preload_state(), (Some(true), None));
    look(&mut rs, &coord, true);
    look(&mut rs, &coord, true);
    assert_eq!(rs.preload_state(), (Some(true), None));
    assert_eq!(seam.entered.lock().unwrap().len(), 1, "one entry asked");
    assert_eq!(builds(), before);
}
