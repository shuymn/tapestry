//! The preload pack's measurement marks on a real coordinator, shelf and
//! pack thread, timed on a clock the test holds (E17, E18, E19, E20): the
//! admission, a waiter's stop, the stop's time and job, the gaps between
//! checkpoints, the stopping checkpoint's disposal, a hold inside its
//! bracket, a retarget's wait, the witnesses, and the step's first stops.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::{Duration, Instant};

use game::coord::Class;

use super::{
    AFTER_SEEN, AFTER_STOP_STORE, Opened, REACHED, Seam, advance, clock_now, collected,
    coordinator, gate, held_clock, long, look, pass, preloaded, shelf, until,
};
use crate::result_skin::builds;
use crate::result_skin::gaps::{self, HoldAt};
use crate::result_skin::tests::runtime_drawn_skin;
use crate::shelf::{PAUSE_SLICE, Paused};

/// The marks of `tag` since index `from`: (time, job, value) of a paired
/// mark, (time, 0, value) of a plain one.
fn marks(from: usize, tag: &str) -> Vec<(u64, i64, i64)> {
    crate::dev::loop_probe::tests::marks()
        .into_iter()
        .skip(from)
        .filter(|m| m.1 == tag)
        .map(|(t, _, v)| (t, v >> 40, v & ((1 << 40) - 1)))
        .collect()
}

/// Every mark since `from`, in order: (time, tag, raw value).
fn all(from: usize) -> Vec<(u64, &'static str, i64)> {
    crate::dev::loop_probe::tests::marks()
        .into_iter()
        .skip(from)
        .collect()
}

fn start() -> usize {
    let _ = held_clock();
    crate::dev::loop_probe::tests::marks().len()
}

/// A `co:*` mark's task and class.
fn co(v: i64) -> (i64, i64) {
    (v >> 24 & 0xFFFF_FFFF, v >> 20 & 0xF)
}

/// The job id of the pack that began since `from`.
fn begun(from: usize) -> i64 {
    let b = marks(from, "pre:begin");
    assert_eq!(b.len(), 1, "{b:?}");
    b[0].2
}

/// Waits until a mark of `tag` came since `from`.
fn marked(from: usize, tag: &str) {
    until(tag, || !marks(from, tag).is_empty());
}

/// The admission: `pre:admit` once after it and before the first
/// progress, its permit's `co:take` the job's task, which `pre:task`
/// maps; one build and the whole-image decode among its kinds.
#[test]
fn an_admitted_pack_marks_its_admission_and_its_work() {
    let _s = crate::imports::tests::serial();
    let from = start();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    crate::dev::loop_probe::observe_permits(&coord);
    let k0 = skin_model::dev::packings();
    look(&mut rs, &coord, true);
    collected(&mut rs, &coord);
    assert_eq!(skin_model::dev::packings() - k0, 1, "E17: one packing");
    let id = begun(from);
    let task = marks(from, "pre:task");
    assert_eq!(task.len(), 1);
    assert_eq!(task[0].1, id);
    let task = task[0].2;
    let order: Vec<&str> = all(from)
        .iter()
        .map(|m| m.1)
        .filter(|t| matches!(*t, "pre:admit" | "pre:progress" | "co:take"))
        .collect();
    let admit = order.iter().position(|t| *t == "pre:admit").unwrap();
    let progress = order.iter().position(|t| *t == "pre:progress").unwrap();
    assert!(admit < progress, "{order:?}");
    assert_eq!(
        marks(from, "pre:admit"),
        [(marks(from, "pre:admit")[0].0, id, 2)]
    );
    let takes: Vec<(i64, i64)> = marks(from, "co:take")
        .iter()
        .map(|m| co(m.1 << 40 | m.2))
        .collect();
    assert!(takes.contains(&(task, 2)), "{takes:?} for task {task}");
    // After the admission: the job's own take comes first.
    let seen = all(from);
    let job_take = seen
        .iter()
        .position(|m| m.1 == "co:take" && co(m.2).0 == task)
        .unwrap();
    let admit_at = seen.iter().position(|m| m.1 == "pre:admit").unwrap();
    assert!(job_take < admit_at, "pre:admit before its take");
    assert_eq!(marks(from, "pre:build").len(), 1, "one build");
    let visits = marks(from, "pre:visits");
    assert_eq!(visits.len(), 1);
    let kinds = visits[0].2 >> gaps::KIND_SHIFT;
    assert_ne!(
        kinds & 1 << skin_model::dev::kind::WHOLE_DECODE,
        0,
        "{kinds:b}"
    );
    assert_ne!(visits[0].2 & 1 << 16, 0, "StepStart passed");
}

/// A waiter: stopped while it waits for its permit, it marks no
/// admission; its wait opens and closes without a take, and it sees the
/// stop in its admission.
#[test]
fn a_waiter_stopped_marks_its_wait_and_no_admission() {
    let _s = crate::imports::tests::serial();
    let from = start();
    let d = runtime_drawn_skin();
    let data = tempfile::tempdir().unwrap();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let mut shelf = shelf(data.path());
    let control = rs.preload_control().unwrap();
    shelf.set_preload(Arc::clone(&control));
    let coord = coordinator(1);
    crate::dev::loop_probe::observe_permits(&coord);
    let held = coord.try_lease(Class::Speculative).unwrap();
    // The job's marks: after the test's own lease.
    let after_lease = crate::dev::loop_probe::tests::marks().len();
    look(&mut rs, &coord, true);
    marked(from, "co:wait");
    shelf.pause();
    assert!(control.wait_ended(long()), "the waiter ended");
    drop(held);
    let id = begun(from);
    let seq: Vec<&str> = all(after_lease)
        .iter()
        .map(|m| m.1)
        .filter(|t| {
            matches!(
                *t,
                "co:wait" | "pre:stop" | "co:unwait" | "pre:seen" | "pre:left" | "co:take"
            )
        })
        .collect();
    assert_eq!(
        seq,
        ["co:wait", "pre:stop", "co:unwait", "pre:seen", "pre:left"]
    );
    assert!(marks(from, "pre:admit").is_empty(), "no admission");
    assert_eq!(marks(from, "pre:seen")[0].1, id);
    assert_eq!(marks(from, "pre:seen")[0].2, i64::from(gaps::ADMIT));
    collected(&mut rs, &coord);
}

/// The time held at the stop's request.
static T0: Mutex<i64> = Mutex::new(0);
/// The marks' index at the test's start.
static FROM: Mutex<usize> = Mutex::new(0);
/// The preload's control, for the cut's wait.
static CONTROL: Mutex<Option<Arc<crate::result_skin::PreloadControl>>> = Mutex::new(None);

/// E20's cut, right after the pause's flag is stored: the clock moves on
/// 1 ms, the pack is let go and sees the stop, then the clock moves on
/// another 1 ms before the mark.
fn cut() {
    let from = *FROM.lock().unwrap();
    advance(1_000);
    gate(1);
    let began = Instant::now();
    while marks(from, "pre:seen").is_empty() {
        assert!(began.elapsed() < long(), "the pack never saw the stop");
        std::thread::park_timeout(Duration::from_millis(1));
    }
    // And it has left its build.
    let control = CONTROL.lock().unwrap().clone().unwrap();
    assert!(control.wait_acknowledged(long()), "the pack never left");
    advance(1_000);
}

/// The stop's time and job: read at the request, before its flag, so
/// the mark is the request's time though the pack saw it 1 ms later and
/// had left by the mark.
#[test]
fn a_stop_is_marked_at_its_request_with_its_job() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let from = start();
    *FROM.lock().unwrap() = from;
    let d = runtime_drawn_skin();
    let data = tempfile::tempdir().unwrap();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let mut shelf = shelf(data.path());
    let control = rs.preload_control().unwrap();
    shelf.set_preload(Arc::clone(&control));
    let coord = coordinator(1);
    gate(2);
    look(&mut rs, &coord, true);
    until("the pack's checkpoint", || REACHED.load(SeqCst));
    let id = begun(from);
    *T0.lock().unwrap() = clock_now();
    *CONTROL.lock().unwrap() = Some(Arc::clone(&control));
    *AFTER_STOP_STORE.lock().unwrap() = Some(cut);
    shelf.pause();
    *AFTER_STOP_STORE.lock().unwrap() = None;
    let t0 = u64::try_from(*T0.lock().unwrap()).unwrap();
    assert_eq!(marks(from, "pre:stop"), [(t0, 0, id)], "at the request");
    let seen = marks(from, "pre:seen");
    assert_eq!(seen.len(), 1);
    assert_eq!((seen[0].0, seen[0].1), (t0 + 1_000, id));
    assert!(control.wait_ended(long()));
    collected(&mut rs, &coord);
}

/// Gaps: 30 ms inside a checkpoint is not the job's work; 10 ms between
/// two is its longest stretch. The positive control has none.
#[test]
fn a_gap_is_the_work_between_checkpoints() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    for timed in [true, false] {
        let from = start();
        let d = runtime_drawn_skin();
        let seam = Arc::new(Seam::default());
        let mut rs = preloaded(d.path(), &seam);
        let coord = coordinator(1);
        crate::dev::loop_probe::observe_permits(&coord);
        let once = Arc::new(AtomicBool::new(timed));
        let o = Arc::clone(&once);
        skin_model::dev::set_work_hook(Some(Arc::new(move || {
            if o.swap(false, SeqCst) {
                advance(10_000);
            }
        })));
        gate(2);
        look(&mut rs, &coord, true);
        until("the pack's checkpoint", || REACHED.load(SeqCst));
        if timed {
            advance(30_000);
        }
        gate(0);
        collected(&mut rs, &coord);
        skin_model::dev::set_work_hook(None);
        let id = begun(from);
        let gap = marks(from, "pre:gap");
        assert_eq!(gap.len(), 1);
        assert_eq!(gap[0].1, id);
        assert_eq!(gap[0].2, if timed { 10_000 } else { 0 }, "timed {timed}");
        assert!(marks(from, "pre:gap_over").is_empty());
        assert!(marks(from, "co:wait").is_empty(), "no wait");
    }
}

/// The stopping checkpoint ends the job's work: time after it is the
/// stop's disposal, not a gap.
#[test]
fn the_stopping_checkpoint_ends_the_gap() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let from = start();
    let d = runtime_drawn_skin();
    let data = tempfile::tempdir().unwrap();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let mut shelf = shelf(data.path());
    let control = rs.preload_control().unwrap();
    shelf.set_preload(Arc::clone(&control));
    let coord = coordinator(1);
    fn dispose() {
        advance(100_000);
    }
    *AFTER_SEEN.lock().unwrap() = Some(dispose);
    gate(1);
    look(&mut rs, &coord, true);
    until("the pack's checkpoint", || REACHED.load(SeqCst));
    shelf.pause();
    assert!(control.wait_ended(long()));
    *AFTER_SEEN.lock().unwrap() = None;
    let id = begun(from);
    assert_eq!(marks(from, "pre:gap")[0].2, 0);
    assert_eq!(
        marks(from, "pre:dispose"),
        [(marks(from, "pre:dispose")[0].0, id, 100_000)]
    );
    collected(&mut rs, &coord);
}

/// A hold runs inside its checkpoint's bracket: 200 ms held is no gap.
#[test]
fn a_hold_is_inside_its_bracket() {
    let _s = crate::imports::tests::serial();
    let from = start();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    gaps::hold(HoldAt::parse("PlanRects:1:after").unwrap());
    look(&mut rs, &coord, true);
    until("the hold", || gaps::held().is_some());
    advance(200_000);
    gaps::release();
    collected(&mut rs, &coord);
    let id = begun(from);
    assert_eq!(marks(from, "pre:gap")[0].2, 0);
    assert!(marks(from, "pre:gap_over").is_empty());
    assert_eq!(
        marks(from, "pre:hold"),
        [(marks(from, "pre:hold")[0].0, id, 7)]
    );
    assert_eq!(marks(from, "pre:hold_released").len(), 1);
}

/// A retarget: a claimed pack promoted while every foreground permit is
/// held gives its speculative permit back and waits for a foreground one;
/// the wait is the time between its marks, and no gap.
#[test]
fn a_retarget_waits_outside_the_gap() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let from = start();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    crate::dev::loop_probe::observe_permits(&coord);
    let control = rs.preload_control().unwrap();
    gate(2);
    look(&mut rs, &coord, true);
    until("the pack's checkpoint", || REACHED.load(SeqCst));
    let fg: Vec<_> = (0..game::coord::FOREGROUND_PERMITS)
        .filter_map(|_| coord.try_lease(Class::Foreground))
        .collect();
    let claim = control.claim();
    claim.promote();
    let _ = pass();
    let waited = marks(from, "co:wait");
    until("the wait for a foreground permit", || {
        marks(from, "co:wait").len() > waited.len()
    });
    let at = clock_now();
    advance(30_000);
    drop(fg);
    gate(0);
    collected(&mut rs, &coord);
    drop(claim);
    let task = marks(from, "pre:task")[0].2;
    let wait: Vec<_> = marks(from, "co:wait")
        .into_iter()
        .filter(|m| co(m.1 << 40 | m.2) == (task, 1))
        .collect();
    let take: Vec<_> = marks(from, "co:take")
        .into_iter()
        .filter(|m| co(m.1 << 40 | m.2) == (task, 1))
        .collect();
    assert_eq!(wait.len(), 1, "{wait:?}");
    assert_eq!(take.len(), 1, "{take:?}");
    assert_eq!(wait[0].0, u64::try_from(at).unwrap());
    assert_eq!(take[0].0 - wait[0].0, 30_000);
    let freed: Vec<_> = marks(from, "co:free")
        .into_iter()
        .filter(|m| co(m.1 << 40 | m.2) == (task, 2))
        .collect();
    assert_eq!(freed.len(), 1, "its speculative permit given back");
    assert_eq!(marks(from, "pre:gap")[0].2, 0, "the wait is no gap");
}

/// The whole-image decode lies between its file's last read and the first
/// mip: 60 ms there is a stretch over the bound named by those sites, the
/// decode among its kinds.
#[test]
fn a_gap_over_the_bound_names_its_sites_and_kinds() {
    let _s = crate::imports::tests::serial();
    let from = start();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    let once = Arc::new(AtomicBool::new(true));
    let o = Arc::clone(&once);
    skin_model::dev::set_decode_hook(Some(Arc::new(move || {
        if o.swap(false, SeqCst) {
            advance(60_000);
        }
    })));
    look(&mut rs, &coord, true);
    collected(&mut rs, &coord);
    skin_model::dev::set_decode_hook(None);
    let id = begun(from);
    let over = marks(from, "pre:gap_over");
    assert_eq!(over.len(), 1, "{over:?}");
    assert_eq!((over[0].1, over[0].2), (id, 60_000));
    let sites = marks(from, "pre:gap_sites")[0].2;
    assert_eq!(
        (sites >> 8, sites & 0xFF),
        (
            i64::from(skin_model::dev::site::READ),
            i64::from(skin_model::dev::site::MIP)
        )
    );
    let kinds = marks(from, "pre:gap_kinds")[0].2;
    assert_ne!(kinds & 1 << skin_model::dev::kind::WHOLE_DECODE, 0);
    assert_eq!(marks(from, "pre:gap_from").len(), 1);
}

/// E18's first stops and E19's own branch, through a hold: held at the
/// step's start or before the plan's first ask, a pause is acknowledged
/// with no build; held at any planning or final ask, the slot is unlocked.
#[test]
fn a_pack_held_at_its_first_asks_stops_with_no_build() {
    let _s = crate::imports::tests::serial();
    for (at, site, packings) in [("StepStart:1:before", 16, 0), ("PlanRects:1:before", 7, 1)] {
        let from = start();
        let d = runtime_drawn_skin();
        let data = tempfile::tempdir().unwrap();
        let seam = Arc::new(Seam::default());
        let mut rs = preloaded(d.path(), &seam);
        let mut shelf = shelf(data.path());
        shelf.set_preload(rs.preload_control().unwrap());
        let coord = coordinator(1);
        let (k0, b0) = (skin_model::dev::packings(), builds());
        gaps::hold(HoldAt::parse(at).unwrap());
        look(&mut rs, &coord, true);
        until("the hold", || gaps::held().is_some());
        shelf.pause();
        gaps::release();
        let began = Instant::now();
        let paused = shelf.wait_paused(
            PAUSE_SLICE,
            || began.elapsed() > long(),
            Duration::MAX,
            || {},
        );
        assert_ne!(paused, Paused::Cancelled, "{at}: acknowledged");
        collected(&mut rs, &coord);
        assert_eq!(skin_model::dev::packings() - k0, packings, "{at}");
        assert_eq!(builds(), b0, "{at}: no build");
        assert!(marks(from, "pre:build").is_empty(), "{at}");
        assert_eq!(marks(from, "pre:seen")[0].2, site, "{at}: seen there");
        let visits = marks(from, "pre:visits")[0].2;
        assert_eq!(
            visits >> gaps::KIND_SHIFT & 1 << skin_model::dev::kind::WHOLE_DECODE,
            0,
            "{at}: no decode"
        );
        shelf.release();
    }

    for at in ["PlanRects:1:after", "PlanEnd:1:after", "Take:1:before"] {
        let d = runtime_drawn_skin();
        let seam = Arc::new(Seam::default());
        let mut rs = preloaded(d.path(), &seam);
        let coord = coordinator(1);
        gaps::hold(HoldAt::parse(at).unwrap());
        look(&mut rs, &coord, true);
        until("the hold", || gaps::held().is_some());
        let slot = rs.pack_slot().unwrap();
        assert!(slot.try_lock().is_ok(), "{at}: the slot is unlocked");
        gaps::release();
        collected(&mut rs, &coord);
    }
}

/// Whether the test thread has looked at a published hold (E25).
static LOOKED: AtomicBool = AtomicBool::new(false);
/// The marks' index at an adoption test's start (E25).
static ADOPT_FROM: Mutex<usize> = Mutex::new(0);

/// Clears E25's hooks, also when an assertion fails.
struct Hooks;

impl Drop for Hooks {
    fn drop(&mut self) {
        *super::AFTER_HOLD_STORE.lock().unwrap() = None;
        *super::AFTER_ADOPT_RELEASE.lock().unwrap() = None;
        *super::IN_MARK_OUTPUT.lock().unwrap() = None;
        *AFTER_SEEN.lock().unwrap() = None;
        LOOKED.store(true, SeqCst);
        gaps::release();
    }
}

/// A hold is marked before it is published: whoever finds the pack held
/// finds its `pre:hold` already marked (E25). The pack waits right after
/// the publication until the test has looked.
#[test]
fn a_hold_is_marked_before_it_is_published() {
    let _s = crate::imports::tests::serial();
    let _h = Hooks;
    let from = start();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    fn looked() {
        until("the test's look at the hold", || LOOKED.load(SeqCst));
    }
    LOOKED.store(false, SeqCst);
    *super::AFTER_HOLD_STORE.lock().unwrap() = Some(looked);
    gaps::hold(HoldAt::parse("PlanRects:1:after").unwrap());
    look(&mut rs, &coord, true);
    until("the hold", || gaps::held().is_some());
    let marked = marks(from, "pre:hold").len();
    LOOKED.store(true, SeqCst);
    gaps::release();
    collected(&mut rs, &coord);
    assert_eq!(marked, 1, "pre:hold not marked when the hold was seen");
}

/// An adoption is marked before the hold it lets go: the held pack's
/// `pre:hold_released` follows `img:adopted` (E25). The game thread waits
/// right after the release until the pack has marked its end.
#[test]
fn an_adoption_is_marked_before_its_hold_is_let_go() {
    let _s = crate::imports::tests::serial();
    let _h = Hooks;
    let from = start();
    *ADOPT_FROM.lock().unwrap() = from;
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    gaps::hold(HoldAt::parse("PlanEnd:1:after").unwrap());
    gaps::release_on_adopt();
    look(&mut rs, &coord, true);
    until("the hold", || gaps::held().is_some());
    let claim = rs.preload_control().unwrap().claim();
    fn released() {
        let from = *ADOPT_FROM.lock().unwrap();
        until("the hold's end", || {
            !marks(from, "pre:hold_released").is_empty()
        });
    }
    *super::AFTER_ADOPT_RELEASE.lock().unwrap() = Some(released);
    let crate::result_skin::ImageStage::Adopted(mut job) = rs.begin_image_stage(
        crate::result_skin::tests::chart(),
        Some(skin_beatoraja::JudgeClass::Normal),
        Some(claim),
    ) else {
        panic!("not adopted");
    };
    *super::AFTER_ADOPT_RELEASE.lock().unwrap() = None;
    let staged = rs.poll_image_stage(&mut job, None, || false, || false);
    assert_eq!(staged, Some(crate::result_skin::Staged::Ready));
    let order: Vec<&str> = all(from)
        .iter()
        .map(|m| m.1)
        .filter(|t| matches!(*t, "img:adopted" | "pre:hold_released"))
        .collect();
    assert_eq!(order, ["img:adopted", "pre:hold_released"]);
}

/// A checkpoint at no known site is a site's visit in `pre:visits`, never
/// an unchecked kind's (E25).
#[test]
fn a_visit_at_no_known_site_sets_no_kind() {
    let _s = crate::imports::tests::serial();
    let from = start();
    gaps::admit(9);
    gaps::bracket_in(gaps::NONE);
    gaps::bracket_out(gaps::NONE);
    gaps::finish();
    let v = marks(from, "pre:visits");
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].1, 9);
    assert_eq!(v[0].2 >> gaps::KIND_SHIFT, 0, "a kind's bit: {:b}", v[0].2);
    assert_ne!(v[0].2 & 1 << gaps::NONE, 0, "the visit: {:b}", v[0].2);
}

/// Whether a release was pending when the hold engaged (E25).
static STALE: AtomicBool = AtomicBool::new(true);

/// A release given before a hold engages is spent: the next hold to
/// engage waits for a release of its own (E25), so a script can arm a
/// hold for the next pack before it lets the held one go.
#[test]
fn a_release_before_a_hold_engages_does_not_end_it() {
    let _s = crate::imports::tests::serial();
    let _h = Hooks;
    let from = start();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    fn engaged() {
        STALE.store(gaps::released(), SeqCst);
        until("the test's look at the hold", || LOOKED.load(SeqCst));
    }
    STALE.store(true, SeqCst);
    LOOKED.store(false, SeqCst);
    *super::AFTER_HOLD_STORE.lock().unwrap() = Some(engaged);
    gaps::hold(HoldAt::parse("PlanRects:1:after").unwrap());
    // The release a script gives the hold before this one.
    gaps::release();
    look(&mut rs, &coord, true);
    until("the hold", || gaps::held().is_some());
    LOOKED.store(true, SeqCst);
    assert!(
        !STALE.load(SeqCst),
        "the earlier release still pending at the engagement"
    );
    assert!(marks(from, "pre:hold_released").is_empty());
    gaps::release();
    collected(&mut rs, &coord);
    assert_eq!(marks(from, "pre:hold_released").len(), 1);
}

/// A job's end, its output held 80 ms on the test's clock (E25).
fn slow_end_output(tag: &'static str) {
    if tag == "pre:end" {
        advance(80_000);
    }
}

/// A mark's output is no stretch's work: 80 ms in the `pre:end` line,
/// after the job's last checkpoint, leaves its longest stretch at 0 and
/// no gap over the bound (E25).
#[test]
fn a_marks_output_is_no_stretchs_work() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let _h = Hooks;
    let from = start();
    let d = runtime_drawn_skin();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let coord = coordinator(1);
    *super::IN_MARK_OUTPUT.lock().unwrap() = Some(slow_end_output);
    look(&mut rs, &coord, true);
    collected(&mut rs, &coord);
    let id = begun(from);
    assert_eq!(marks(from, "pre:end").len(), 1, "the job ended");
    assert_eq!(
        marks(from, "pre:gap"),
        [(marks(from, "pre:gap")[0].0, id, 0)]
    );
    assert!(
        marks(from, "pre:gap_over").is_empty(),
        "no gap over the bound"
    );
}

/// After a stop, a mark's output is no disposal: 100 ms of disposal and
/// 80 ms in the `pre:end` line give a disposal of 100 ms (E25).
#[test]
fn a_marks_output_is_no_disposal() {
    let _s = crate::imports::tests::serial();
    let _g = Opened;
    let _h = Hooks;
    let from = start();
    let d = runtime_drawn_skin();
    let data = tempfile::tempdir().unwrap();
    let seam = Arc::new(Seam::default());
    let mut rs = preloaded(d.path(), &seam);
    let mut shelf = shelf(data.path());
    let control = rs.preload_control().unwrap();
    shelf.set_preload(Arc::clone(&control));
    let coord = coordinator(1);
    fn dispose() {
        advance(100_000);
    }
    *AFTER_SEEN.lock().unwrap() = Some(dispose);
    *super::IN_MARK_OUTPUT.lock().unwrap() = Some(slow_end_output);
    gate(1);
    look(&mut rs, &coord, true);
    until("the pack's checkpoint", || REACHED.load(SeqCst));
    shelf.pause();
    assert!(control.wait_ended(long()));
    let id = begun(from);
    assert_eq!(marks(from, "pre:end").len(), 1, "the job ended");
    assert_eq!(
        marks(from, "pre:dispose"),
        [(marks(from, "pre:dispose")[0].0, id, 100_000)]
    );
    collected(&mut rs, &coord);
}
