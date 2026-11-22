//! The result skin's preload on a real coordinator and shelf: each pack
//! is held at its build's checkpoint until the test lets it go, never by
//! timing. A wait for something that never happens fails its test by
//! name after `TAPESTRY_TEST_WATCHDOG` seconds.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use game::coord::{Caps, Class, Coordinator, FOREGROUND_PERMITS};
use skin_model::AtlasId;

use skin_beatoraja::JudgeClass;

use crate::priority::{PriorityError, ThreadMode};
use crate::result_skin::tests::{LIMITS, chart, pending, runtime_drawn_skin};
use crate::result_skin::{ImageStage, ResultSkin, ResultStart, Staged, builds};
use crate::screens::Scripts;
use crate::shelf::{PAUSE_SLICE, Paused, Shelf};

/// The gate a pack's checkpoint waits at: 0 open, 1 held until the pack
/// should stop, 2 held whatever it is told.
static GATE: AtomicU8 = AtomicU8::new(0);
/// Whether a pack reached the armed gate.
pub(crate) static REACHED: AtomicBool = AtomicBool::new(false);
/// The times a pack came to the armed gate.
static ARRIVALS: AtomicU32 = AtomicU32::new(0);
/// The checkpoints the armed gate lets pass while it holds ([`step`]).
static STEPS: AtomicU32 = AtomicU32::new(0);

/// A pack's checkpoint, before the coordinator's: it waits at the armed
/// [`GATE`], `stop` saying whether it should stop, unless a [`step`] lets
/// it pass this once.
pub(super) fn pack_gate(stop: &dyn Fn() -> bool) {
    if GATE.load(SeqCst) == 0 {
        return;
    }
    ARRIVALS.fetch_add(1, SeqCst);
    REACHED.store(true, SeqCst);
    loop {
        match GATE.load(SeqCst) {
            0 => return,
            1 if stop() => return,
            _ if STEPS
                .try_update(SeqCst, SeqCst, |n| n.checked_sub(1))
                .is_ok() =>
            {
                return;
            }
            _ => std::thread::park_timeout(Duration::from_millis(1)),
        }
    }
}

/// Lets the pack held at the armed gate pass one checkpoint (the
/// coordinator's after it included) and waits until it is held at the
/// gate again, so that checkpoint has returned. Whether it came back
/// within [`long`] (a pack that ended its build, or waits in its
/// checkpoint for a permit, does not).
pub(crate) fn step() -> bool {
    let arrived = ARRIVALS.load(SeqCst);
    STEPS.fetch_add(1, SeqCst);
    let began = Instant::now();
    while ARRIVALS.load(SeqCst) == arrived {
        if began.elapsed() > long() {
            return false;
        }
        std::thread::park_timeout(Duration::from_millis(1));
    }
    true
}

/// Arms the gate at `level` (0 opens it).
pub(crate) fn gate(level: u8) {
    if level > 0 && GATE.load(SeqCst) == 0 {
        REACHED.store(false, SeqCst);
        STEPS.store(0, SeqCst);
    }
    GATE.store(level, SeqCst);
}

/// The gate opens when the test ends, however it ends.
pub(crate) struct Opened;

impl Drop for Opened {
    fn drop(&mut self) {
        gate(0);
    }
}

pub(crate) fn long() -> Duration {
    let secs = std::env::var("TAPESTRY_TEST_WATCHDOG").ok();
    Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(600))
}

/// Waits until `done`, failing the test past [`long`].
pub(crate) fn until(what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < long(), "{what} never happened");
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

/// A thread mode that records each entry and exit by thread name and
/// refuses entries when told.
#[derive(Default)]
pub(crate) struct Seam {
    refuse: AtomicBool,
    pub(crate) entered: Mutex<Vec<String>>,
    pub(crate) left: Mutex<Vec<String>>,
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
        let name = std::thread::current().name().unwrap_or("").to_owned();
        self.left.lock().unwrap().push(name);
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
    preloaded_with(dir, seam, None)
}

/// [`preloaded`] on the runtime cell `scripts` when given.
pub(crate) fn preloaded_with(
    dir: &std::path::Path,
    seam: &Arc<Seam>,
    scripts: Option<Scripts>,
) -> ResultSkin {
    let p = pending(dir, LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
    if let Some(scripts) = scripts {
        rs = rs.with_scripts(scripts);
    }
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

/// The preload begun under `seam` on `coord`, its pack held at its
/// checkpoint by the gate at `level`.
fn held_pack(
    dir: &std::path::Path,
    seam: &Arc<Seam>,
    coord: &Coordinator,
    level: u8,
) -> ResultSkin {
    let mut rs = preloaded(dir, seam);
    gate(level);
    look(&mut rs, coord, true);
    until("the pack's checkpoint", || REACHED.load(SeqCst));
    rs
}

/// A choice claims the pack while it is held at its checkpoint, after the
/// pause was requested: the pack is never stopped, the shelf's wait is
/// acknowledged at once, and the same job packs once.
#[test]
fn a_claim_keeps_a_running_pack_through_the_pause() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let data = tempfile::tempdir().unwrap();
    let seam = Arc::new(Seam::default());
    let coord = coordinator(1);
    let before = builds();
    let mut rs = held_pack(d.path(), &seam, &coord, 2);
    let mut shelf = shelf(data.path());
    shelf.set_preload(rs.preload_control().unwrap());
    shelf.pause();
    let claim = shelf.claim_preload().unwrap();
    let paused = shelf.wait_paused(PAUSE_SLICE, || false, Duration::MAX, || {});
    assert_ne!(paused, Paused::Cancelled, "claimed: acknowledged");
    // Let go: it sees the pause claimed and goes on.
    gate(0);
    collected(&mut rs, &coord);
    assert_eq!(rs.preload_state(), (None, None), "packed");
    assert!(rs.image().is_some());
    assert_eq!(builds(), before + 1, "the one job packed");
    drop(claim);
}

/// A claim that comes after the pack's stop was observed: the stopped job
/// is collected and a new one (another id) begins under the pause.
#[test]
fn a_claim_after_the_stop_was_observed_begins_a_new_job() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let data = tempfile::tempdir().unwrap();
    let seam = Arc::new(Seam::default());
    let coord = coordinator(1);
    let before = builds();
    let mut rs = held_pack(d.path(), &seam, &coord, 1);
    let mut shelf = shelf(data.path());
    let control = rs.preload_control().unwrap();
    shelf.set_preload(Arc::clone(&control));
    let (_, Some(first)) = rs.preload_state() else {
        panic!("no pack");
    };
    shelf.pause();
    assert!(control.wait_ended(long()), "stopped at its checkpoint");
    let claim = shelf.claim_preload().unwrap();
    gate(0);
    look(&mut rs, &coord, true);
    let (_, Some(second)) = rs.preload_state() else {
        panic!("no new job under the claim");
    };
    assert_ne!(first, second, "a terminal job is not revived");
    collected(&mut rs, &coord);
    assert_eq!(rs.preload_state(), (None, None));
    assert_eq!(builds(), before + 2);
    drop(claim);
}

/// A claim with the pack's outcome in but not collected: the attempt's
/// stage collects it and finds the image resident, with no job of its
/// own.
#[test]
fn a_claim_with_the_outcome_uncollected_finds_it_resident() {
    let _s = crate::imports::tests::serial();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let coord = coordinator(1);
    let mut rs = preloaded(d.path(), &seam);
    look(&mut rs, &coord, true);
    let control = rs.preload_control().unwrap();
    assert!(control.wait_ended(long()));
    let claim = control.claim();
    let (before, jobs) = (builds(), crate::imports::tests::jobs_counted());
    let stage = rs.begin_image_stage(chart(), Some(JudgeClass::Normal), Some(claim));
    assert!(
        matches!(stage, ImageStage::Ready(Staged::Ready)),
        "resident"
    );
    assert_eq!(builds(), before);
    assert_eq!(crate::imports::tests::jobs_counted(), jobs, "no job begun");
    assert!(rs.image().is_some());
    assert_eq!(rs.preload_state(), (None, None));
}

/// The attempt adopts a claimed pack held mid-build: it stays
/// speculative until promoted at the adoption, the stage waits for it in
/// place of its own import and build, and the atlas is packed once.
#[test]
fn the_attempt_adopts_a_claimed_pack() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let coord = coordinator(1);
    let before = builds();
    let mut rs = held_pack(d.path(), &seam, &coord, 2);
    let claim = rs.preload_control().unwrap().claim();
    assert_eq!(coord.held(Class::Speculative), 1);
    let jobs = crate::imports::tests::jobs_counted();
    let ImageStage::Adopted(mut job) =
        rs.begin_image_stage(chart(), Some(JudgeClass::Normal), Some(claim))
    else {
        panic!("not adopted");
    };
    gate(0);
    let staged = rs.poll_image_stage(&mut job, None, || false, || false);
    assert_eq!(staged, Some(Staged::Ready));
    assert_eq!(*seam.left.lock().unwrap(), ["result-preload"], "foreground");
    assert_eq!(builds(), before + 1, "packed once");
    assert_eq!(crate::imports::tests::jobs_counted(), jobs, "no import job");
    assert!(matches!(
        rs.upload(),
        Some(crate::result_skin::Upload::Pages(_))
    ));
    assert_eq!(rs.preload_state(), (None, None));
}

/// The attempt's quit key while it waits for an adopted pack held at a
/// gate: the wait returns at once, the claim drops with the stage and the
/// pack goes on as the preload's, unclaimed.
#[test]
fn the_quit_key_hands_an_adopted_pack_back_without_waiting() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let coord = coordinator(1);
    let mut rs = held_pack(d.path(), &seam, &coord, 2);
    let control = rs.preload_control().unwrap();
    let (_, job) = rs.preload_state();
    let ImageStage::Adopted(mut stage) =
        rs.begin_image_stage(chart(), Some(JudgeClass::Normal), Some(control.claim()))
    else {
        panic!("not adopted");
    };
    let staged = rs.poll_image_stage(&mut stage, None, || true, || false);
    assert_eq!(staged, Some(Staged::Cancelled));
    drop(stage);
    assert!(!control.claimed(), "the claim dropped");
    assert_eq!(rs.preload_state(), (None, job), "still the preload's");
    gate(0);
    collected(&mut rs, &coord);
    assert!(rs.image().is_some(), "packed for the next attempt");
}

/// The run's end while the stage holds an adopted pack: it is cancelled
/// and its thread has ended once the settle returns.
#[test]
fn the_runs_end_joins_an_adopted_pack() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let coord = coordinator(1);
    let mut rs = held_pack(d.path(), &seam, &coord, 1);
    let control = rs.preload_control().unwrap();
    let ImageStage::Adopted(mut stage) =
        rs.begin_image_stage(chart(), Some(JudgeClass::Normal), Some(control.claim()))
    else {
        panic!("not adopted");
    };
    let _ = rs.settle_image_stage(&mut stage, || true);
    assert!(control.wait_ended(Duration::ZERO), "its thread ended");
    assert_eq!(rs.preload_state(), (None, None));
}

/// A mode change while the claimed pack runs: it is stopped and its
/// outcome discarded, the attempt runs its own stage, and no pack begins
/// again.
#[test]
fn a_mode_change_while_claimed_runs_the_attempts_own_stage() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let coord = coordinator(1);
    let mut rs = held_pack(d.path(), &seam, &coord, 1);
    let claim = rs.preload_control().unwrap().claim();
    rs.set_kept_probe(false);
    assert_eq!(rs.preload_state(), (None, None));
    assert!(rs.image().is_none(), "nothing of the pack restored");
    let stage = rs.begin_image_stage(chart(), Some(JudgeClass::Normal), Some(claim));
    let ImageStage::Running(mut own) = stage else {
        panic!("not its own stage");
    };
    let _ = rs.settle_image_stage(&mut own, || false);
    look(&mut rs, &coord, true);
    assert_eq!(rs.preload_state(), (None, None));
}

/// With speculation off, a claim begins the pack in the foreground with
/// no background entry; the claim's end demotes it and its next
/// checkpoint cancels it, so the preload waits and begins nothing more.
#[test]
fn with_speculation_off_a_claim_runs_the_pack_in_the_foreground() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let coord = coordinator(0);
    let mut rs = preloaded(d.path(), &seam);
    let claim = rs.preload_control().unwrap().claim();
    gate(2);
    look(&mut rs, &coord, true);
    until("the pack's checkpoint", || REACHED.load(SeqCst));
    assert_eq!(coord.held(Class::Foreground), 1);
    assert!(
        seam.entered.lock().unwrap().is_empty(),
        "no background mode"
    );
    drop(claim);
    gate(0);
    collected(&mut rs, &coord);
    assert_eq!(rs.preload_state(), (Some(true), None), "cancelled, waiting");
    assert!(rs.image().is_none());
    look(&mut rs, &coord, true);
    assert_eq!(rs.preload_state(), (Some(true), None), "nothing begins");
}

/// With speculation on, the claim's end leaves the pack speculative: it
/// is never moved to the foreground, and it packs.
#[test]
fn a_claims_end_leaves_the_pack_speculative() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let coord = coordinator(1);
    let mut rs = held_pack(d.path(), &seam, &coord, 2);
    drop(rs.preload_control().unwrap().claim());
    gate(0);
    collected(&mut rs, &coord);
    assert!(seam.left.lock().unwrap().is_empty(), "never foreground");
    assert!(rs.image().is_some());
}

/// After a packed outcome is collected, no pack begins again in the
/// installation, across a claim, a mode change and a deferral.
#[test]
fn no_pack_begins_after_the_warm_up() {
    let _s = crate::imports::tests::serial();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let coord = coordinator(1);
    let mut rs = preloaded(d.path(), &seam);
    let before = builds();
    look(&mut rs, &coord, true);
    collected(&mut rs, &coord);
    let claim = rs.preload_control().unwrap().claim();
    rs.set_kept_probe(false);
    rs.note_retained(&[AtlasId(u64::MAX - 1), AtlasId(u64::MAX)]);
    look(&mut rs, &coord, true);
    rs.note_retained(&[]);
    look(&mut rs, &coord, true);
    assert_eq!(rs.preload_state(), (None, None));
    assert_eq!(builds(), before + 1);
    drop(claim);
}
