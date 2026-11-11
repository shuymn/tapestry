//! Owned jobs: one test per row of the lifecycle table (Y1-Y32, Z1-Z2)
//! and its in-flight rows (YI1-YI10). Threads meet at gates and
//! channels; a settle's bound is read on a test clock, never the wall
//! clock. Every test arms a watchdog that aborts the process, naming the
//! test, when it hangs: a join that never ends would otherwise never fail.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::super::{
    Caps, Class, Coordinator, Ctx, Failure, Gate, Generation, JobKey, JobKind, Observed, Outcome,
    Owner, Scope, Spawner, Subscriber, Taken, catch,
};
use super::{Handed, OwnedJob, Promotion, Settled, Waited, WorkerLeases};

/// How long a test may run before its watchdog aborts the process.
static WATCHDOG: std::sync::LazyLock<Duration> = std::sync::LazyLock::new(|| {
    // `TAPESTRY_TEST_WATCHDOG` (seconds) shortens it for mutation runs.
    let secs = std::env::var("TAPESTRY_TEST_WATCHDOG").ok();
    Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(600))
});

/// A long wait for something that must happen: a safety net only, half
/// the watchdog, so a missing event fails its own test by name.
static LONG: std::sync::LazyLock<Duration> = std::sync::LazyLock::new(|| *WATCHDOG / 2);

/// Aborts the process when the test it was armed for has not ended in
/// time: a hung join would never fail the test.
struct Watchdog(Arc<AtomicBool>);

impl Watchdog {
    fn arm(name: &'static str) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&done);
        let begun = Instant::now();
        thread::spawn(move || {
            while !seen.load(Ordering::SeqCst) {
                if begun.elapsed() > *WATCHDOG {
                    eprintln!("watchdog: {name} hung");
                    std::process::abort();
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        Self(done)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

const CAPS: Caps = Caps {
    foreground: 4,
    speculative: 1,
};

fn key(name: &str) -> JobKey {
    JobKey {
        speed: bms_model::Speed::NORMAL,
        kind: JobKind::Bank,
        path: PathBuf::from(name),
    }
}

/// Spawns `work` as an owned job of `name` for the current choice.
fn spawn<T, W>(
    coord: &Coordinator,
    name: &str,
    class: Class,
    promoted: bool,
    owner: Owner,
    work: W,
) -> OwnedJob<T>
where
    T: Clone + Send + Sync + 'static,
    W: FnOnce(&Ctx<'_>) -> T + Send + 'static,
{
    coord
        .spawn_owned(
            Spawner::SYSTEM,
            key(name),
            (class, promoted, owner),
            coord.subscriber(Scope::Choice),
            |_| {},
            work,
        )
        .unwrap()
}

/// Waits for the job's outcome without a stop.
fn outcome<T: Clone>(job: &OwnedJob<T>) -> Observed<T> {
    match job.wait(Duration::from_millis(1), || false) {
        Waited::Done(o) => o,
        Waited::Stopped(_) => unreachable!("no stop"),
    }
}

/// Runs the checkpoint until it says stop.
fn until_cancelled(ctx: &Ctx<'_>) {
    while !ctx.checkpoint() {
        thread::yield_now();
    }
}

fn gate() -> Arc<Gate> {
    Arc::new(Gate::default())
}

fn reach(g: &Gate, what: &str) {
    assert!(g.reached(*LONG), "{what} never happened");
}

/// A clock a test moves by hand, counting its reads.
struct TestClock {
    base: Instant,
    us: AtomicI64,
    reads: AtomicUsize,
}

impl TestClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            base: Instant::now(),
            us: AtomicI64::new(0),
            reads: AtomicUsize::new(0),
        })
    }

    fn now(&self) -> Instant {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.base + Duration::from_micros(self.us.load(Ordering::SeqCst).unsigned_abs())
    }

    fn set(&self, us: i64) {
        self.us.store(us, Ordering::SeqCst);
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    /// Waits until the clock was read `n` more times than `since`.
    fn read_past(&self, since: usize, n: usize) {
        let begun = Instant::now();
        while self.reads() < since + n {
            assert!(begun.elapsed() < *LONG, "the clock was never read");
            thread::yield_now();
        }
    }
}

/// A value whose drop waits at a gate and then may panic: what the job's
/// thread drops after it published (its hook's captures).
struct Unwinding {
    entered: Arc<Gate>,
    hold: Arc<Gate>,
    panic: bool,
}

impl Drop for Unwinding {
    fn drop(&mut self) {
        self.entered.arrive();
        self.hold.pass();
        assert!(!self.panic, "a panic after publication");
    }
}

/// Spawns a job publishing `Done(1)` whose thread then waits at `hold`
/// (state U), and panics there when `panic`.
fn unwinding(coord: &Coordinator, name: &str, hold: &Arc<Gate>, panic: bool) -> OwnedJob<u32> {
    let entered = gate();
    let guard = Unwinding {
        entered: Arc::clone(&entered),
        hold: Arc::clone(hold),
        panic,
    };
    let job = coord
        .spawn_owned(
            Spawner::SYSTEM,
            key(name),
            (Class::Foreground, false, Owner::Foreground),
            coord.subscriber(Scope::Choice),
            move |_| {
                let _ = &guard;
            },
            |_| 1_u32,
        )
        .unwrap();
    reach(&entered, "the thread's unwinding");
    job
}

/// What a settle on its own thread needs: the clock, the terminal cause's
/// switch and the count of looks at it.
struct Settling {
    clock: Arc<TestClock>,
    cause: Arc<AtomicBool>,
    asked: Arc<AtomicUsize>,
}

impl Settling {
    fn new() -> Self {
        Self {
            clock: TestClock::new(),
            cause: Arc::new(AtomicBool::new(false)),
            asked: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Settles `job` on its own thread, bound 1 000 µs; the result comes
    /// on the receiver.
    fn start(&self, job: OwnedJob<u32>) -> Receiver<Settled<u32, &'static str>> {
        let (tx, rx) = mpsc::channel();
        let (clock, cause, asked) = (
            Arc::clone(&self.clock),
            Arc::clone(&self.cause),
            Arc::clone(&self.asked),
        );
        thread::spawn(move || {
            let settled = job.settle(Duration::from_micros(1_000), &|| clock.now(), &mut || {
                asked.fetch_add(1, Ordering::SeqCst);
                cause.load(Ordering::SeqCst).then_some("closed")
            });
            let _ = tx.send(settled);
        });
        rx
    }

    /// Waits until the terminal was asked `n` times.
    fn asked(&self, n: usize) {
        let begun = Instant::now();
        while self.asked.load(Ordering::SeqCst) < n {
            assert!(begun.elapsed() < *LONG, "the settle never asked");
            thread::yield_now();
        }
    }

    /// The cause at 0 µs, the deadline at 1 000 µs: at 999 µs a decision
    /// is taken (a read there and the next one) and the settle goes on;
    /// at 1 000 µs it gives up. The clock stays at each time until the
    /// settle's next read shows the decision there was taken.
    fn abandon_at_the_bound(
        &self,
        rx: &Receiver<Settled<u32, &'static str>>,
    ) -> Settled<u32, &'static str> {
        self.asked(2);
        let r = self.clock.reads();
        self.cause.store(true, Ordering::SeqCst);
        self.clock.read_past(r, 2);
        self.clock.set(999);
        let r = self.clock.reads();
        self.clock.read_past(r, 2);
        assert!(rx.try_recv().is_err(), "abandoned before the bound");
        self.clock.set(1_000);
        rx.recv_timeout(*LONG).unwrap()
    }
}

/// Y1.
#[test]
fn an_owned_job_whose_thread_is_refused_holds_nothing() {
    let _w = Watchdog::arm("an_owned_job_whose_thread_is_refused_holds_nothing");
    let coord = Coordinator::new(CAPS);
    let refused = coord.spawn_owned(
        Spawner::refusing(&[JobKind::Bank]),
        key("a"),
        (Class::Foreground, false, Owner::Foreground),
        coord.subscriber(Scope::Choice),
        |_| {},
        |_| 1_u32,
    );
    let e = refused.unwrap_err();
    assert!(matches!(e.cause, Failure::ThreadStart(_)), "{e}");
    assert_eq!(coord.held(Class::Foreground), 0);
    assert_eq!(coord.held(Class::Speculative), 0);
    // The key is free again.
    let job = spawn(
        &coord,
        "a",
        Class::Foreground,
        false,
        Owner::Foreground,
        |_| 2_u32,
    );
    assert_eq!(outcome(&job).outcome, Outcome::Done(2));
}

/// Y2, two cases: a keyed job of another result type, and an owned one;
/// both refuse with [`Failure::AlreadyRuns`], never a thread failure.
#[test]
fn an_owned_spawn_of_a_running_key_is_refused() {
    let _w = Watchdog::arm("an_owned_spawn_of_a_running_key_is_refused");
    let coord = Coordinator::new(CAPS);
    let (first, runner) =
        coord.begin::<u32>(key("a"), Class::Foreground, coord.subscriber(Scope::Choice));
    let runner = runner.unwrap();
    let refused = coord.spawn_owned(
        Spawner::SYSTEM,
        key("a"),
        (Class::Foreground, false, Owner::Foreground),
        coord.subscriber(Scope::Choice),
        |_| {},
        |_| String::new(),
    );
    let e = refused.unwrap_err();
    assert_eq!(e.cause, Failure::AlreadyRuns);
    assert_eq!(
        e.to_string(),
        "the chart's sound decode cannot start: the same job already runs"
    );
    // The running job is untouched: its own kind still joins it.
    let (joined, none) =
        coord.begin::<u32>(key("a"), Class::Foreground, coord.subscriber(Scope::Choice));
    assert!(none.is_none());
    let _ = runner.run(&|| false, &|_| {}, |_| 3);
    assert_eq!(coord.take(&joined), Taken::Ready(Outcome::Done(3)));
    drop((first, joined));

    let hold = gate();
    let owned = stuck(&coord, "b", &hold);
    let refused = coord.spawn_owned(
        Spawner::SYSTEM,
        key("b"),
        (Class::Foreground, false, Owner::Foreground),
        coord.subscriber(Scope::Choice),
        |_| {},
        |_| 5_u32,
    );
    assert_eq!(refused.unwrap_err().cause, Failure::AlreadyRuns);
    assert!(!owned.finished(), "the held job's thread is running");
    hold.open();
    assert_eq!(outcome(&owned).outcome, Outcome::Done(1));
    while !owned.finished() {
        thread::park_timeout(Duration::from_millis(1));
    }
    // Finished: the settle joins at once, with no terminal cause asked for.
    let begun = Instant::now();
    let settled = owned.settle(Duration::from_secs(3600), &Instant::now, &mut || None::<()>);
    assert!(matches!(
        settled,
        Settled::Joined {
            after_panic: None,
            ..
        }
    ));
    assert!(begun.elapsed() < Duration::from_secs(5));
}

/// Y3.
#[test]
fn a_promoted_spawn_is_admitted_in_the_foreground() {
    let _w = Watchdog::arm("a_promoted_spawn_is_admitted_in_the_foreground");
    let coord = Coordinator::new(CAPS);
    let job = spawn(
        &coord,
        "a",
        Class::Speculative,
        true,
        Owner::Foreground,
        |ctx| ctx.class(),
    );
    assert_eq!(
        outcome(&job).outcome,
        Outcome::Done(Some(Class::Foreground))
    );
}

/// Y4.
#[test]
fn a_queued_owned_job_promoted_takes_a_foreground_permit() {
    let _w = Watchdog::arm("a_queued_owned_job_promoted_takes_a_foreground_permit");
    let coord = Coordinator::new(CAPS);
    let busy = coord.try_lease(Class::Speculative).unwrap();
    let job = spawn(
        &coord,
        "a",
        Class::Speculative,
        false,
        Owner::Speculative,
        |ctx| ctx.class(),
    );
    assert_eq!(job.poll(), None);
    assert_eq!(job.promote_unless_ended(), Promotion::Live);
    let o = outcome(&job);
    assert_eq!(o.outcome, Outcome::Done(Some(Class::Foreground)));
    assert!(o.responsible);
    drop(busy);
}

/// Y5.
#[test]
fn a_demoted_queued_job_with_no_speculative_permit_ends_cancelled() {
    let _w = Watchdog::arm("a_demoted_queued_job_with_no_speculative_permit_ends_cancelled");
    let coord = Coordinator::new(Caps {
        foreground: 1,
        speculative: 0,
    });
    let busy = coord.try_lease(Class::Foreground).unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&ran);
    let job = spawn(
        &coord,
        "a",
        Class::Speculative,
        true,
        Owner::Speculative,
        move |_| {
            seen.store(true, Ordering::SeqCst);
        },
    );
    job.demote();
    assert_eq!(outcome(&job).outcome, Outcome::Cancelled);
    assert!(!ran.load(Ordering::SeqCst));
    drop(busy);
}

/// Y6.
#[test]
fn promotion_and_demotion_move_at_the_owners_checkpoint() {
    let _w = Watchdog::arm("promotion_and_demotion_move_at_the_owners_checkpoint");
    let coord = Coordinator::new(CAPS);
    let (step_tx, step_rx) = mpsc::channel::<()>();
    let (class_tx, class_rx) = mpsc::channel();
    let hook = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::clone(&hook);
    let job = coord
        .spawn_owned(
            Spawner::SYSTEM,
            key("a"),
            (Class::Speculative, false, Owner::Speculative),
            coord.subscriber(Scope::Choice),
            move |c| calls.lock().unwrap().push(c),
            move |ctx| {
                class_tx.send(ctx.class()).unwrap();
                while step_rx.recv().is_ok() {
                    let _ = ctx.checkpoint();
                    class_tx.send(ctx.class()).unwrap();
                }
            },
        )
        .unwrap();
    let next = |f: &dyn Fn()| {
        f();
        step_tx.send(()).unwrap();
        class_rx.recv_timeout(*LONG).unwrap()
    };
    assert_eq!(
        class_rx.recv_timeout(*LONG).unwrap(),
        Some(Class::Speculative)
    );
    let promote = || assert_eq!(job.promote_unless_ended(), Promotion::Live);
    assert_eq!(next(&promote), Some(Class::Foreground));
    assert_eq!(next(&|| job.demote()), Some(Class::Speculative));
    let both = || {
        promote();
        job.demote();
    };
    assert_eq!(next(&both), Some(Class::Speculative));
    drop(step_tx);
    let _ = outcome(&job);
    // Its new thread entered the speculative mode at its admission (Z1),
    // then each move at a checkpoint called the hook once.
    assert_eq!(
        *hook.lock().unwrap(),
        [Class::Speculative, Class::Foreground, Class::Speculative]
    );
}

/// Z1, Z2: an owned job's new thread is in the normal mode. Admitted
/// speculative, it calls the hook once with that class, on its own
/// thread, before its work; promoted at its spawn and admitted in the
/// foreground, it calls none.
#[test]
fn an_owned_jobs_first_speculative_admission_enters_background_mode() {
    let _w = Watchdog::arm("an_owned_jobs_first_speculative_admission_enters_background_mode");
    let coord = Coordinator::new(CAPS);
    for promoted in [false, true] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let hooked = Arc::clone(&calls);
        let seen = Arc::clone(&calls);
        let job = coord
            .spawn_owned(
                Spawner::SYSTEM,
                key(if promoted { "promoted" } else { "speculative" }),
                (Class::Speculative, promoted, Owner::Speculative),
                coord.subscriber(Scope::Choice),
                move |c| hooked.lock().unwrap().push((c, thread::current().id())),
                move |ctx| {
                    (
                        ctx.class(),
                        seen.lock().unwrap().len(),
                        thread::current().id(),
                    )
                },
            )
            .unwrap();
        let Outcome::Done((class, before_work, job_thread)) = outcome(&job).outcome else {
            panic!("not done");
        };
        let calls = calls.lock().unwrap().clone();
        if promoted {
            assert_eq!(class, Some(Class::Foreground));
            assert!(calls.is_empty(), "{calls:?}");
        } else {
            assert_eq!(class, Some(Class::Speculative));
            assert_eq!(calls, [(Class::Speculative, job_thread)]);
            assert_eq!(before_work, 1);
        }
    }
}

/// YI4: while the hook runs on the job's thread for a promotion (held
/// there, the coordinator's lock let go), the job is demoted and promoted
/// again and then demoted: the hook's run neither blocks those requests
/// nor is undone by them, and the next checkpoint follows the last one.
#[test]
fn a_hook_running_while_promoted_and_demoted_follows_the_last_request() {
    let _w = Watchdog::arm("a_hook_running_while_promoted_and_demoted_follows_the_last_request");
    let coord = Coordinator::new(CAPS);
    let in_hook = gate();
    let h = Arc::clone(&in_hook);
    let (step_tx, step_rx) = mpsc::channel::<()>();
    let (class_tx, class_rx) = mpsc::channel();
    let job = coord
        .spawn_owned(
            Spawner::SYSTEM,
            key("a"),
            (Class::Speculative, false, Owner::Speculative),
            coord.subscriber(Scope::Choice),
            move |c| {
                if c == Class::Foreground {
                    h.pass();
                }
            },
            move |ctx| {
                class_tx.send(ctx.class()).unwrap();
                while step_rx.recv().is_ok() {
                    let _ = ctx.checkpoint();
                    class_tx.send(ctx.class()).unwrap();
                }
            },
        )
        .unwrap();
    assert_eq!(
        class_rx.recv_timeout(*LONG).unwrap(),
        Some(Class::Speculative)
    );
    assert_eq!(job.promote_unless_ended(), Promotion::Live);
    step_tx.send(()).unwrap();
    reach(&in_hook, "the hook");
    job.demote();
    assert_eq!(job.promote_unless_ended(), Promotion::Live);
    job.demote();
    in_hook.open();
    assert_eq!(
        class_rx.recv_timeout(*LONG).unwrap(),
        Some(Class::Foreground)
    );
    step_tx.send(()).unwrap();
    assert_eq!(
        class_rx.recv_timeout(*LONG).unwrap(),
        Some(Class::Speculative)
    );
    drop(step_tx);
    let o = outcome(&job);
    assert_eq!((o.outcome, o.responsible), (Outcome::Done(()), true));
}

/// A speculative job that waits at `hold`, then passes a checkpoint and
/// returns 7, or 0 when that checkpoint says stop.
fn checked_after(coord: &Coordinator, name: &str, hold: &Arc<Gate>) -> OwnedJob<u32> {
    let held = Arc::clone(hold);
    spawn(
        coord,
        name,
        Class::Speculative,
        false,
        Owner::Speculative,
        move |ctx| {
            held.pass();
            if ctx.checkpoint() { 0 } else { 7 }
        },
    )
}

/// A foreground job that ignores its stop until `hold` opens, then
/// returns 1.
fn stuck(coord: &Coordinator, name: &str, hold: &Arc<Gate>) -> OwnedJob<u32> {
    let held = Arc::clone(hold);
    spawn(
        coord,
        name,
        Class::Foreground,
        false,
        Owner::Foreground,
        move |_| {
            held.pass();
            1
        },
    )
}

/// Y7: the stopped waiter neither cancels nor stops the job, which passes
/// a checkpoint after the stop and finishes.
#[test]
fn a_stopped_wait_leaves_the_job_running_and_responsible() {
    let _w = Watchdog::arm("a_stopped_wait_leaves_the_job_running_and_responsible");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let job = checked_after(&coord, "a", &hold);
    reach(&hold, "the job's start");
    assert_eq!(job.promote_unless_ended(), Promotion::Live);
    assert_eq!(
        job.wait(Duration::from_millis(1), || true),
        Waited::Stopped(None)
    );
    hold.open();
    let o = outcome(&job);
    assert_eq!(o.outcome, Outcome::Done(7));
    assert!(o.responsible);
}

/// Y8.
#[test]
fn a_promotion_after_a_stopped_wait_is_live_again() {
    let _w = Watchdog::arm("a_promotion_after_a_stopped_wait_is_live_again");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let held = Arc::clone(&hold);
    let job = spawn(
        &coord,
        "a",
        Class::Speculative,
        false,
        Owner::Speculative,
        move |ctx| {
            held.pass();
            let _ = ctx.checkpoint();
            ctx.class()
        },
    );
    reach(&hold, "the job's start");
    assert_eq!(job.promote_unless_ended(), Promotion::Live);
    assert_eq!(
        job.wait(Duration::from_millis(1), || true),
        Waited::Stopped(None)
    );
    assert_eq!(job.promote_unless_ended(), Promotion::Live);
    hold.open();
    let o = outcome(&job);
    assert_eq!(o.outcome, Outcome::Done(Some(Class::Foreground)));
    assert!(o.responsible);
}

/// Y9.
#[test]
fn the_owner_at_publication_is_recorded() {
    let _w = Watchdog::arm("the_owner_at_publication_is_recorded");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let job = stuck(&coord, "a", &hold);
    reach(&hold, "the job's start");
    assert_eq!(job.promote_unless_ended(), Promotion::Live);
    assert_eq!(job.to_owner(Owner::Speculative), Handed::Changed);
    hold.open();
    let o = outcome(&job);
    assert_eq!((o.owner, o.responsible), (Owner::Speculative, true));
}

/// Y10.
#[test]
fn advancing_the_choice_cancels_an_owned_job() {
    let _w = Watchdog::arm("advancing_the_choice_cancels_an_owned_job");
    let coord = Coordinator::new(CAPS);
    let started = gate();
    let s = Arc::clone(&started);
    let job = spawn(
        &coord,
        "a",
        Class::Foreground,
        false,
        Owner::Foreground,
        move |ctx| {
            s.arrive();
            until_cancelled(ctx);
        },
    );
    reach(&started, "the job's start");
    let _ = coord.advance(Scope::Choice);
    assert_eq!(outcome(&job).outcome, Outcome::Cancelled);
}

/// Y11: a cancel requested and not yet seen; and with responsibility
/// taken before it, which stays.
#[test]
fn a_cancel_requested_job_is_not_promoted() {
    let _w = Watchdog::arm("a_cancel_requested_job_is_not_promoted");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let job = checked_after(&coord, "a", &hold);
    reach(&hold, "the job's start");
    let _ = coord.advance(Scope::Choice);
    assert_eq!(job.promote_unless_ended(), Promotion::Ending);
    hold.open();
    let o = outcome(&job);
    assert_eq!((o.outcome, o.responsible), (Outcome::Cancelled, false));

    let hold = gate();
    let job = checked_after(&coord, "b", &hold);
    reach(&hold, "the second job's start");
    assert_eq!(job.promote_unless_ended(), Promotion::Live);
    let _ = coord.advance(Scope::Choice);
    assert_eq!(job.promote_unless_ended(), Promotion::Ending);
    hold.open();
    let o = outcome(&job);
    assert_eq!((o.outcome, o.responsible), (Outcome::Cancelled, true));
}

/// A job that loops until it sees its cancel, then waits at `hold`: its
/// thread held in CO. The gate returned opens once the cancel is seen.
fn observed_cancel(
    coord: &Coordinator,
    name: &str,
    hold: &Arc<Gate>,
) -> (OwnedJob<u32>, Arc<Gate>) {
    let (started, seen) = (gate(), gate());
    let (b, s, h) = (Arc::clone(&started), Arc::clone(&seen), Arc::clone(hold));
    let job = spawn(
        coord,
        name,
        Class::Foreground,
        false,
        Owner::Foreground,
        move |ctx| {
            b.arrive();
            until_cancelled(ctx);
            s.arrive();
            h.pass();
            9_u32
        },
    );
    // Computing before the cancel comes.
    reach(&started, "the job's start");
    (job, seen)
}

/// Y12.
#[test]
fn a_cancel_observed_job_is_not_revived() {
    let _w = Watchdog::arm("a_cancel_observed_job_is_not_revived");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let (job, seen) = observed_cancel(&coord, "a", &hold);
    let _ = coord.advance(Scope::Choice);
    reach(&seen, "the cancel seen");
    // The thread is held in CO.
    assert_eq!(job.promote_unless_ended(), Promotion::Ending);
    assert_eq!(job.poll(), None);
    hold.open();
    let o = outcome(&job);
    assert_eq!((o.outcome, o.responsible), (Outcome::Cancelled, false));
}

/// Y13.
#[test]
fn settling_a_cancelled_job_joins_it() {
    let _w = Watchdog::arm("settling_a_cancelled_job_joins_it");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let (job, seen) = observed_cancel(&coord, "a", &hold);
    let _ = coord.advance(Scope::Choice);
    reach(&seen, "the cancel seen");
    hold.open();
    let settled = job.settle(Duration::ZERO, &Instant::now, &mut || None::<()>);
    let Settled::Joined {
        observed,
        after_panic,
        cause,
    } = settled
    else {
        panic!("not joined: {settled:?}");
    };
    assert_eq!(observed.map(|o| o.outcome), Some(Outcome::Cancelled));
    assert_eq!((after_panic, cause), (None, None));
    assert_eq!(coord.unsettled_drops(), 0);
}

fn in_the_hook(c: Class) {
    assert!(c != Class::Foreground, "in the hook");
}

/// Y14, four cases: a panic in the work; in the hook at admission (the
/// first speculative one, which calls it, Z1); in the hook at a checkpoint
/// after a promotion; and in the hook where the work catches it and
/// returns normally, passing no checkpoint after.
#[test]
fn an_owned_panic_carries_its_owner_and_responsibility() {
    let _w = Watchdog::arm("an_owned_panic_carries_its_owner_and_responsibility");
    let coord = Coordinator::new(CAPS);
    let job = spawn(
        &coord,
        "work",
        Class::Foreground,
        false,
        Owner::Foreground,
        |_| -> u32 { panic!("in the work") },
    );
    let o = outcome(&job);
    assert!(matches!(o.outcome, Outcome::Panicked(ref p) if p.0 == "in the work"));
    assert_eq!((o.owner, o.responsible), (Owner::Foreground, false));

    // The job signals once admitted, so a promotion lands at a checkpoint
    // of its work, not at its admission.
    let hooked = |name: &str, promoted: bool, work: fn(&Ctx<'_>) -> u32| {
        let admitted = gate();
        let a = Arc::clone(&admitted);
        let job = coord
            .spawn_owned(
                Spawner::SYSTEM,
                key(name),
                (Class::Speculative, promoted, Owner::Speculative),
                coord.subscriber(Scope::Choice),
                in_the_hook,
                move |ctx| {
                    a.arrive();
                    work(ctx)
                },
            )
            .unwrap();
        if !promoted {
            reach(&admitted, "the job's admission");
        }
        job
    };
    let at_admission = coord
        .spawn_owned(
            Spawner::SYSTEM,
            key("admission"),
            (Class::Speculative, false, Owner::Speculative),
            coord.subscriber(Scope::Choice),
            |c| assert!(c != Class::Speculative, "in the hook"),
            |_| 1_u32,
        )
        .unwrap();
    let o = outcome(&at_admission);
    assert!(matches!(o.outcome, Outcome::Panicked(ref p) if p.0 == "in the hook"));
    assert_eq!((o.owner, o.responsible), (Owner::Speculative, false));

    let at_checkpoint: fn(&Ctx<'_>) -> u32 = |ctx| {
        while ctx.class() != Some(Class::Foreground) {
            let _ = ctx.checkpoint();
            thread::yield_now();
        }
        1
    };
    let caught: fn(&Ctx<'_>) -> u32 = |ctx| loop {
        if catch(|| ctx.checkpoint()).is_err() {
            return 5;
        }
        thread::yield_now();
    };
    for (name, work) in [("checkpoint", at_checkpoint), ("caught", caught)] {
        let job = hooked(name, false, work);
        assert_eq!(job.promote_unless_ended(), Promotion::Live);
        let o = outcome(&job);
        assert!(
            matches!(o.outcome, Outcome::Panicked(ref p) if p.0 == "in the hook"),
            "{name}: {o:?}"
        );
        assert_eq!(
            (o.owner, o.responsible),
            (Owner::Speculative, true),
            "{name}"
        );
    }
}

/// Y15.
#[test]
fn a_published_job_is_not_promoted() {
    let _w = Watchdog::arm("a_published_job_is_not_promoted");
    let coord = Coordinator::new(CAPS);
    let job = spawn(
        &coord,
        "a",
        Class::Speculative,
        false,
        Owner::Speculative,
        |_| 1_u32,
    );
    let done = outcome(&job);
    assert!(!done.responsible);
    assert_eq!(job.promote_unless_ended(), Promotion::Published(done));
}

/// Y16: a responsible wait stopped, the job panicking later; without and
/// with a second promotion before the panic.
#[test]
fn a_late_panic_after_a_stopped_responsible_wait_stays_responsible() {
    let _w = Watchdog::arm("a_late_panic_after_a_stopped_responsible_wait_stays_responsible");
    let coord = Coordinator::new(CAPS);
    for again in [false, true] {
        let hold = gate();
        let held = Arc::clone(&hold);
        let job = spawn(
            &coord,
            if again { "again" } else { "once" },
            Class::Speculative,
            false,
            Owner::Speculative,
            move |_| -> u32 {
                held.pass();
                panic!("late")
            },
        );
        reach(&hold, "the job's start");
        assert_eq!(job.promote_unless_ended(), Promotion::Live);
        assert_eq!(
            job.wait(Duration::from_millis(1), || true),
            Waited::Stopped(None)
        );
        if again {
            assert_eq!(job.promote_unless_ended(), Promotion::Live);
            job.demote();
        }
        hold.open();
        let o = outcome(&job);
        assert!(matches!(o.outcome, Outcome::Panicked(_)));
        assert_eq!((o.owner, o.responsible), (Owner::Speculative, true));
        assert_eq!(job.poll(), Some(o.clone()));
        assert_eq!(job.promote_unless_ended(), Promotion::Published(o));
    }
}

/// Y17.
#[test]
fn an_outcome_published_before_the_boundary_keeps_its_owner() {
    let _w = Watchdog::arm("an_outcome_published_before_the_boundary_keeps_its_owner");
    let coord = Coordinator::new(CAPS);
    let job = spawn(
        &coord,
        "a",
        Class::Foreground,
        false,
        Owner::Foreground,
        |_| -> u32 { panic!("before the page") },
    );
    let o = outcome(&job);
    assert_eq!(o.owner, Owner::Foreground);
    assert_eq!(job.to_owner(Owner::Speculative), Handed::Published(o));
}

/// Y18.
#[test]
fn settling_stops_and_joins() {
    let _w = Watchdog::arm("settling_stops_and_joins");
    let coord = Coordinator::new(CAPS);
    let started = gate();
    let s = Arc::clone(&started);
    let job = spawn(
        &coord,
        "a",
        Class::Foreground,
        false,
        Owner::Foreground,
        move |ctx| {
            s.arrive();
            until_cancelled(ctx);
        },
    );
    reach(&started, "the job's start");
    let settled = job.settle(Duration::ZERO, &Instant::now, &mut || None::<()>);
    assert!(
        matches!(
            settled,
            Settled::Joined { observed: Some(ref o), after_panic: None, cause: None }
                if o.outcome == Outcome::Cancelled
        ),
        "{settled:?}"
    );
    assert_eq!(coord.held(Class::Foreground), 0);
}

/// Y19: abandoned at the deadline and not a microsecond before, with the
/// cause returned exactly.
#[test]
fn a_terminal_during_a_settle_abandons_at_the_bound() {
    let _w = Watchdog::arm("a_terminal_during_a_settle_abandons_at_the_bound");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let job = stuck(&coord, "a", &hold);
    reach(&hold, "the job's start");
    let settling = Settling::new();
    let rx = settling.start(job);
    let settled = settling.abandon_at_the_bound(&rx);
    assert_eq!(
        settled,
        Settled::Abandoned {
            observed: None,
            cause: "closed"
        }
    );
    assert_eq!(coord.unsettled_drops(), 0);
    hold.open();
}

/// Y20.
#[test]
fn settling_after_publication_waits_for_the_thread() {
    let _w = Watchdog::arm("settling_after_publication_waits_for_the_thread");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let job = unwinding(&coord, "a", &hold, false);
    assert_eq!(job.poll().map(|o| o.outcome), Some(Outcome::Done(1)));
    let settling = Settling::new();
    let rx = settling.start(job);
    // It looked three times and still waits for the thread.
    settling.asked(3);
    assert!(rx.try_recv().is_err(), "settled before the thread ended");
    hold.open();
    let settled = rx.recv_timeout(*LONG).unwrap();
    assert!(
        matches!(
            settled,
            Settled::Joined { observed: Some(ref o), after_panic: None, cause: None }
                if o.outcome == Outcome::Done(1)
        ),
        "{settled:?}"
    );
}

/// Y21: the cause arrives after the settle began looking, while the
/// thread unwinds after publication.
#[test]
fn a_terminal_while_the_thread_unwinds_abandons_at_the_bound() {
    let _w = Watchdog::arm("a_terminal_while_the_thread_unwinds_abandons_at_the_bound");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let job = unwinding(&coord, "a", &hold, false);
    let settling = Settling::new();
    let rx = settling.start(job);
    let settled = settling.abandon_at_the_bound(&rx);
    let Settled::Abandoned { observed, cause } = settled else {
        panic!("not abandoned: {settled:?}");
    };
    assert_eq!(
        (observed.map(|o| o.outcome.clone()), cause),
        (Some(Outcome::Done(1)), "closed")
    );
    hold.open();
}

/// Y22.
#[test]
fn a_panic_after_publication_is_reported() {
    let _w = Watchdog::arm("a_panic_after_publication_is_reported");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let job = unwinding(&coord, "a", &hold, true);
    hold.open();
    let settled = job.settle(Duration::ZERO, &Instant::now, &mut || None::<()>);
    let Settled::Joined {
        observed,
        after_panic,
        cause: None,
    } = settled
    else {
        panic!("not joined: {settled:?}");
    };
    assert_eq!(observed.map(|o| o.outcome), Some(Outcome::Done(1)));
    assert_eq!(
        after_panic.map(|p| p.0),
        Some("a panic after publication".to_owned())
    );
}

/// Y23.
#[test]
fn an_abandoned_job_is_not_joined_on_drop() {
    let _w = Watchdog::arm("an_abandoned_job_is_not_joined_on_drop");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let job = stuck(&coord, "a", &hold);
    reach(&hold, "the job's start");
    let settled = job.settle(Duration::ZERO, &Instant::now, &mut || Some("closed"));
    assert!(
        matches!(
            settled,
            Settled::Abandoned {
                cause: "closed",
                ..
            }
        ),
        "{settled:?}"
    );
    assert_eq!(coord.unsettled_drops(), 0);
    hold.open();
}

/// Y24.
#[test]
fn dropping_an_unsettled_owned_job_detaches_it() {
    let _w = Watchdog::arm("dropping_an_unsettled_owned_job_detaches_it");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let held = Arc::clone(&hold);
    let (tx, rx) = mpsc::channel();
    let job = spawn(
        &coord,
        "a",
        Class::Foreground,
        false,
        Owner::Foreground,
        move |ctx| {
            held.pass();
            let _ = tx.send(ctx.cancelled());
        },
    );
    reach(&hold, "the job's start");
    // Dropped on another thread: a drop that joins would wait on the held
    // job forever, and fails here instead.
    let (dropped_tx, dropped) = mpsc::channel();
    thread::spawn(move || {
        drop(job);
        let _ = dropped_tx.send(());
    });
    assert!(
        dropped.recv_timeout(*LONG).is_ok(),
        "the drop blocked on the job's thread"
    );
    assert_eq!(coord.unsettled_drops(), 1);
    hold.open();
    assert_eq!(rx.recv_timeout(*LONG), Ok(true), "the drop stopped it");
}

/// Y24b, two cases: the terminal unwinds; the clock unwinds.
#[test]
fn an_unwinding_terminal_or_clock_leaves_settle_without_blocking() {
    let _w = Watchdog::arm("an_unwinding_terminal_or_clock_leaves_settle_without_blocking");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let job = stuck(&coord, "a", &hold);
    reach(&hold, "the job's start");
    let unwound = catch(move || {
        job.settle(Duration::ZERO, &Instant::now, &mut || -> Option<()> {
            panic!("terminal")
        })
    });
    assert!(unwound.is_err());
    assert_eq!(coord.unsettled_drops(), 1);
    hold.open();

    let hold = gate();
    let job = stuck(&coord, "b", &hold);
    reach(&hold, "the second job's start");
    let unwound = catch(move || {
        job.settle(
            Duration::ZERO,
            &|| -> Instant { panic!("clock") },
            &mut || Some(()),
        )
    });
    assert!(unwound.is_err());
    assert_eq!(coord.unsettled_drops(), 2);
    hold.open();
}

/// Y25.
#[test]
fn two_speculative_jobs_on_one_permit_both_finish() {
    let _w = Watchdog::arm("two_speculative_jobs_on_one_permit_both_finish");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let held = Arc::clone(&hold);
    let first = spawn(
        &coord,
        "a",
        Class::Speculative,
        false,
        Owner::Speculative,
        move |ctx| {
            held.pass();
            ctx.class()
        },
    );
    reach(&hold, "the first job's start");
    let second = spawn(
        &coord,
        "b",
        Class::Speculative,
        false,
        Owner::Speculative,
        |ctx| ctx.class(),
    );
    assert_eq!(second.poll(), None);
    hold.open();
    assert_eq!(
        outcome(&first).outcome,
        Outcome::Done(Some(Class::Speculative))
    );
    assert_eq!(
        outcome(&second).outcome,
        Outcome::Done(Some(Class::Speculative))
    );
}

/// Y25's other half.
#[test]
fn a_queued_job_promoted_is_admitted_in_the_foreground() {
    let _w = Watchdog::arm("a_queued_job_promoted_is_admitted_in_the_foreground");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let held = Arc::clone(&hold);
    let first = spawn(
        &coord,
        "a",
        Class::Speculative,
        false,
        Owner::Speculative,
        move |_| {
            held.pass();
        },
    );
    reach(&hold, "the first job's start");
    let second = spawn(
        &coord,
        "b",
        Class::Speculative,
        false,
        Owner::Speculative,
        |ctx| ctx.class(),
    );
    assert_eq!(second.promote_unless_ended(), Promotion::Live);
    assert_eq!(
        outcome(&second).outcome,
        Outcome::Done(Some(Class::Foreground))
    );
    hold.open();
    let _ = outcome(&first);
}

/// Y26: the pool's cap less the job's own thread; none at cap 1 (or 0);
/// none once the cap shrinks to the extras already running.
#[test]
fn worker_leases_stop_at_the_pools_cap() {
    let _w = Watchdog::arm("worker_leases_stop_at_the_pools_cap");
    let coord = Coordinator::new(CAPS);
    let c = coord.clone();
    let job = spawn(
        &coord,
        "a",
        Class::Foreground,
        false,
        Owner::Foreground,
        move |ctx| {
            let leases = WorkerLeases::new(&c);
            let none_at_one =
                leases.reserve(ctx, 1, 0).is_none() && leases.reserve(ctx, 0, 0).is_none();
            let a = leases.reserve(ctx, 3, 0);
            let b = leases.reserve(ctx, 3, 1);
            let third = leases.reserve(ctx, 3, 2).is_none();
            let held = c.held(Class::Foreground);
            let shrunk = leases.reserve(ctx, 2, 2).is_none() && leases.reserve(ctx, 2, 1).is_none();
            (none_at_one, a.is_some() && b.is_some(), third, held, shrunk)
        },
    );
    assert_eq!(
        outcome(&job).outcome,
        Outcome::Done((true, true, true, 3, true))
    );
    assert_eq!(coord.held(Class::Foreground), 0);
}

/// Y27.
#[test]
fn a_failed_spawn_returns_its_reservation() {
    let _w = Watchdog::arm("a_failed_spawn_returns_its_reservation");
    let coord = Coordinator::new(CAPS);
    let c = coord.clone();
    let job = spawn(
        &coord,
        "a",
        Class::Foreground,
        false,
        Owner::Foreground,
        move |ctx| {
            let pending = WorkerLeases::new(&c).reserve(ctx, 4, 0);
            let reserved = c.held(Class::Foreground);
            // The worker's start failed: the reservation goes back while the
            // decode goes on.
            drop(pending);
            (reserved, c.held(Class::Foreground))
        },
    );
    assert_eq!(outcome(&job).outcome, Outcome::Done((2, 1)));
}

/// Y28: a working extra keeps its permit through a demotion and a smaller
/// cap, and gives it back when it retires.
#[test]
fn a_working_extra_keeps_its_permit_until_it_retires() {
    let _w = Watchdog::arm("a_working_extra_keeps_its_permit_until_it_retires");
    let coord = Coordinator::new(CAPS);
    let (reserved, demoted) = (gate(), gate());
    let (r, d) = (Arc::clone(&reserved), Arc::clone(&demoted));
    let c = coord.clone();
    let job = spawn(
        &coord,
        "a",
        Class::Speculative,
        true,
        Owner::Speculative,
        move |ctx| {
            let leases = WorkerLeases::new(&c);
            let lease = leases.reserve(ctx, 4, 0).expect("a free permit");
            let retire = gate();
            let mid_file = Arc::clone(&retire);
            thread::scope(|s| {
                let worker = s.spawn(move || {
                    mid_file.pass();
                    lease.retire();
                });
                r.arrive();
                d.pass();
                let none =
                    leases.reserve(ctx, 4, 1).is_none() && leases.reserve(ctx, 1, 1).is_none();
                let during = c.held(Class::Foreground);
                retire.open();
                worker.join().unwrap();
                (none, during, c.held(Class::Foreground))
            })
        },
    );
    reach(&reserved, "the reservation");
    job.demote();
    demoted.open();
    // The job's own permit stays foreground until its next checkpoint,
    // which it does not pass: two held while the extra works, one after.
    assert_eq!(outcome(&job).outcome, Outcome::Done((true, 2, 1)));
}

thread_local! {
    /// The clone of an `Asking` running on this thread: a nested clone
    /// asks nothing more.
    static ASKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Reads the slot an `Asking` result was published in.
type Probe = Arc<std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>>;

/// A result whose clone asks the coordinator and reads its own slot again,
/// as a result type may: a clone run under the registry's or the slot's
/// lock deadlocks.
struct Asking(Coordinator, Probe);

impl Clone for Asking {
    fn clone(&self) -> Self {
        if !ASKING.with(std::cell::Cell::get) {
            ASKING.with(|a| a.set(true));
            let _ = self.0.current(Scope::Choice);
            let _ = self.0.held(Class::Foreground);
            if let Some(probe) = self.1.get() {
                probe();
            }
            ASKING.with(|a| a.set(false));
        }
        Self(self.0.clone(), Arc::clone(&self.1))
    }
}

/// Y29: run on its own thread, so a deadlock fails this test by name. The
/// probes hold weak references, so the fixture lets its payloads go and
/// the owned job is settled at the end.
#[test]
fn a_payload_clone_may_query_the_coordinator() {
    let _w = Watchdog::arm("a_payload_clone_may_query_the_coordinator");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let coord = Coordinator::new(CAPS);
        let (owned_probe, keyed_probe): (Probe, Probe) = Default::default();
        let (c, p) = (coord.clone(), Arc::clone(&owned_probe));
        let job = Arc::new(spawn(
            &coord,
            "a",
            Class::Foreground,
            false,
            Owner::Foreground,
            move |_| Asking(c, p),
        ));
        let again = Arc::downgrade(&job);
        let _ = owned_probe.set(Box::new(move || {
            if let Some(job) = again.upgrade() {
                let _ = job.poll();
            }
        }));
        let _ = outcome(&job);
        let (handle, runner) =
            coord.begin::<Asking>(key("b"), Class::Foreground, coord.subscriber(Scope::Choice));
        let handle = Arc::new(handle);
        let (c, p) = (coord.clone(), Arc::clone(&keyed_probe));
        let _ = runner
            .unwrap()
            .run(&|| false, &|_| {}, move |_| Asking(c, p));
        let (c, h) = (coord.clone(), Arc::downgrade(&handle));
        let _ = keyed_probe.set(Box::new(move || {
            if let Some(h) = h.upgrade() {
                let _ = c.take(&h);
            }
        }));
        thread::scope(|s| {
            for _ in 0..2 {
                s.spawn(|| {
                    for _ in 0..50 {
                        assert!(job.poll().is_some());
                        assert!(matches!(
                            job.promote_unless_ended(),
                            Promotion::Published(_)
                        ));
                        assert!(matches!(
                            job.to_owner(Owner::Speculative),
                            Handed::Published(_)
                        ));
                        assert!(matches!(coord.take(&handle), Taken::Ready(_)));
                    }
                });
            }
        });
        // The job's thread clones the published result on its way out:
        // it holds no reference once it has ended.
        let begun = Instant::now();
        while !job.finished() {
            assert!(begun.elapsed() < *LONG, "the job's thread never ended");
            thread::yield_now();
        }
        let job = Arc::into_inner(job).expect("the probes hold no strong reference");
        let settled = job.settle(*LONG, &Instant::now, &mut || None::<()>);
        assert!(
            matches!(
                settled,
                Settled::Joined {
                    after_panic: None,
                    ..
                }
            ),
            "the job's thread panicked"
        );
        assert!(
            Arc::into_inner(handle).is_some(),
            "the probes hold no strong reference"
        );
        let _ = tx.send(());
    });
    assert!(rx.recv_timeout(*LONG).is_ok(), "a payload clone deadlocked");
}

/// Y30: an `Ending` job's computed result is not published: the final
/// cancel is read in the same critical section as the publication, and
/// nothing brings an owned job back from `Ending`. Each case holds a job
/// whose work returned before its finish, advances the choice, finds it
/// ending, then lets it go:
///
/// - an owned job: it publishes `Cancelled`;
/// - an owned job, and between `Ending` and its finish a same-key,
///   same-type `begin` with a current subscriber, its handle kept: the
///   `begin` does not join it (a job of its own begins), so nothing
///   withdraws its cancel, and it publishes `Cancelled`;
/// - a keyed job: `run` returns what was published, `Cancelled`.
#[test]
fn an_ending_job_never_publishes_done() {
    let _w = Watchdog::arm("an_ending_job_never_publishes_done");
    let coord = Coordinator::new(CAPS);
    let cut = coord.cut(false);
    let job = spawn(
        &coord,
        "a",
        Class::Foreground,
        false,
        Owner::Foreground,
        |_| 1_u32,
    );
    reach(&cut, "the finish");
    let _ = coord.advance(Scope::Choice);
    assert_eq!(job.promote_unless_ended(), Promotion::Ending);
    cut.open();
    assert_eq!(outcome(&job).outcome, Outcome::Cancelled);

    let coord = Coordinator::new(CAPS);
    let cut = coord.cut(false);
    let job = spawn(
        &coord,
        "a",
        Class::Foreground,
        false,
        Owner::Foreground,
        |_| 1_u32,
    );
    reach(&cut, "the owned finish");
    let _ = coord.advance(Scope::Choice);
    assert_eq!(job.promote_unless_ended(), Promotion::Ending);
    let (joiner, own) =
        coord.begin::<u32>(key("a"), Class::Foreground, coord.subscriber(Scope::Choice));
    assert!(own.is_some(), "a begin joined an owned job");
    assert_eq!(job.promote_unless_ended(), Promotion::Ending);
    cut.open();
    assert_eq!(outcome(&job).outcome, Outcome::Cancelled);
    drop((own, joiner));

    let coord = Coordinator::new(CAPS);
    let cut = coord.cut(false);
    let (handle, runner) =
        coord.begin::<u32>(key("b"), Class::Foreground, coord.subscriber(Scope::Choice));
    let runner = runner.unwrap();
    let ran = thread::spawn(move || runner.run(&|| false, &|_| {}, |_| 2));
    reach(&cut, "the keyed finish");
    let _ = coord.advance(Scope::Choice);
    cut.open();
    assert_eq!(ran.join().unwrap(), Outcome::Cancelled);
    // The handle's generation is superseded: what was published is read
    // through the slot directly.
    assert_eq!(
        handle.slot.record().map(|r| r.outcome.clone()),
        Some(Outcome::Cancelled)
    );
}

/// Y32: an owned job's requested cancel is never withdrawn, whatever
/// subscriber turns current afterwards. Held before its admission reads
/// the cancel: spawned for a generation ahead of the current one (so its
/// cancel is requested at once), found `Ending`, then the scope advanced
/// to that generation; let go, it ends `Cancelled`, its work never run.
#[test]
fn an_owned_jobs_requested_cancel_is_never_withdrawn() {
    let _w = Watchdog::arm("an_owned_jobs_requested_cancel_is_never_withdrawn");
    let coord = Coordinator::new(CAPS);
    let cut = coord.cut_admission();
    let ran = Arc::new(AtomicBool::new(false));
    let r = Arc::clone(&ran);
    let ahead = Subscriber {
        scope: Scope::Choice,
        generation: Generation(coord.current(Scope::Choice).0 + 1),
    };
    let job = coord
        .spawn_owned(
            Spawner::SYSTEM,
            key("a"),
            (Class::Foreground, false, Owner::Foreground),
            ahead,
            |_| {},
            move |_| {
                r.store(true, Ordering::SeqCst);
                1_u32
            },
        )
        .unwrap();
    reach(&cut, "the admission");
    assert_eq!(job.promote_unless_ended(), Promotion::Ending);
    assert_eq!(coord.advance(Scope::Choice), ahead.generation);
    assert_eq!(job.promote_unless_ended(), Promotion::Ending);
    cut.open();
    assert_eq!(outcome(&job).outcome, Outcome::Cancelled);
    assert!(!ran.load(Ordering::SeqCst), "the work ran");
}

/// A result whose drop asks the coordinator: a drop run under its lock
/// deadlocks.
#[derive(Clone, Debug)]
struct Discarded(Coordinator, Arc<AtomicBool>);

impl Drop for Discarded {
    fn drop(&mut self) {
        let _ = self.0.current(Scope::Choice);
        let _ = self.0.held(Class::Foreground);
        self.1.store(true, Ordering::SeqCst);
    }
}

/// Y31: the result an `Ending` job discards at its finish drops after the
/// finish lets its lock go, without a panic. Run on its own thread, so a
/// deadlock fails this test by name.
#[test]
fn a_discarded_result_drops_after_the_lock() {
    let _w = Watchdog::arm("a_discarded_result_drops_after_the_lock");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let coord = Coordinator::new(CAPS);
        let cut = coord.cut(false);
        let dropped = Arc::new(AtomicBool::new(false));
        let (c, d) = (coord.clone(), Arc::clone(&dropped));
        let job = spawn(
            &coord,
            "a",
            Class::Foreground,
            false,
            Owner::Foreground,
            move |_| Discarded(c, d),
        );
        reach(&cut, "the finish");
        let _ = coord.advance(Scope::Choice);
        assert!(matches!(job.promote_unless_ended(), Promotion::Ending));
        cut.open();
        assert!(matches!(outcome(&job).outcome, Outcome::Cancelled));
        let settled = job.settle(*LONG, &Instant::now, &mut || None::<()>);
        assert!(
            matches!(
                settled,
                Settled::Joined {
                    after_panic: None,
                    ..
                }
            ),
            "{settled:?}"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "the discarded result never dropped"
        );
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(*LONG).is_ok(),
        "a discarded result's drop deadlocked"
    );
}

/// YI1.
#[test]
fn a_promotion_just_before_publication_makes_it_responsible() {
    let _w = Watchdog::arm("a_promotion_just_before_publication_makes_it_responsible");
    let coord = Coordinator::new(CAPS);
    let cut = coord.cut(false);
    let job = spawn(
        &coord,
        "a",
        Class::Speculative,
        false,
        Owner::Speculative,
        |_| 1_u32,
    );
    reach(&cut, "the finish");
    assert_eq!(job.promote_unless_ended(), Promotion::Live);
    cut.open();
    let o = outcome(&job);
    assert_eq!((o.outcome, o.responsible), (Outcome::Done(1), true));
}

/// YI2: held between the registry removal and the publication, the state
/// lock is taken, so observers wait; let go, they see `Published`.
#[test]
fn observers_are_excluded_between_removal_and_publication() {
    let _w = Watchdog::arm("observers_are_excluded_between_removal_and_publication");
    let coord = Coordinator::new(CAPS);
    let cut = coord.cut(true);
    let job = spawn(
        &coord,
        "a",
        Class::Speculative,
        false,
        Owner::Speculative,
        |_| 1_u32,
    );
    reach(&cut, "the removal");
    assert!(
        !coord.state_free(),
        "the state lock was let go before the publication"
    );
    thread::scope(|s| {
        let promoted = s.spawn(|| job.promote_unless_ended());
        let handed = s.spawn(|| job.to_owner(Owner::Foreground));
        cut.open();
        assert!(matches!(promoted.join().unwrap(), Promotion::Published(_)));
        assert!(matches!(handed.join().unwrap(), Handed::Published(_)));
    });
}

/// YI3: a promotion first, then a cancel the job sees: it ends cancelled,
/// which is no failure, keeping the responsibility it was given.
#[test]
fn promotion_and_an_observed_cancel_are_ordered_by_the_lock() {
    let _w = Watchdog::arm("promotion_and_an_observed_cancel_are_ordered_by_the_lock");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let job = checked_after(&coord, "a", &hold);
    reach(&hold, "the job's start");
    assert_eq!(job.promote_unless_ended(), Promotion::Live);
    let _ = coord.advance(Scope::Choice);
    hold.open();
    let o = outcome(&job);
    assert_eq!((o.outcome, o.responsible), (Outcome::Cancelled, true));
}

/// YI5.
#[test]
fn a_stop_with_a_published_result_keeps_it() {
    let _w = Watchdog::arm("a_stop_with_a_published_result_keeps_it");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let job = stuck(&coord, "a", &hold);
    let waited = job.wait(Duration::from_millis(1), || {
        hold.open();
        while job.poll().is_none() {
            thread::yield_now();
        }
        true
    });
    assert!(
        matches!(waited, Waited::Stopped(Some(ref o)) if o.outcome == Outcome::Done(1)),
        "{waited:?}"
    );
    assert_eq!(job.poll().map(|o| o.outcome), Some(Outcome::Done(1)));
}

/// YI8: the thread has ended before the settle begins, so the one look
/// at the terminal is the one after the join, and its cause is kept.
#[test]
fn a_cause_arriving_at_the_join_is_kept() {
    let _w = Watchdog::arm("a_cause_arriving_at_the_join_is_kept");
    let coord = Coordinator::new(CAPS);
    let job = spawn(
        &coord,
        "a",
        Class::Foreground,
        false,
        Owner::Foreground,
        |_| 1_u32,
    );
    let _ = outcome(&job);
    let begun = Instant::now();
    while !job.finished() {
        assert!(begun.elapsed() < *LONG, "the job's thread never ended");
        thread::yield_now();
    }
    let mut looks = 0;
    let settled = job.settle(Duration::ZERO, &Instant::now, &mut || {
        looks += 1;
        Some("late")
    });
    assert!(
        matches!(
            settled,
            Settled::Joined {
                cause: Some("late"),
                ..
            }
        ),
        "{settled:?}"
    );
    assert_eq!(looks, 1, "the cause came from the look after the join");
}

/// YI9: a replacement begun while the old job settles in CO; the old
/// finish leaves the replacement's key.
#[test]
fn an_old_finish_leaves_the_replacements_key() {
    let _w = Watchdog::arm("an_old_finish_leaves_the_replacements_key");
    let coord = Coordinator::new(CAPS);
    let hold = gate();
    let (old, seen) = observed_cancel(&coord, "a", &hold);
    let _ = coord.advance(Scope::Choice);
    reach(&seen, "the cancel seen");
    let next = gate();
    let replacement = stuck(&coord, "a", &next);
    hold.open();
    assert_eq!(outcome(&old).outcome, Outcome::Cancelled);
    let again = coord.spawn_owned(
        Spawner::SYSTEM,
        key("a"),
        (Class::Foreground, false, Owner::Foreground),
        coord.subscriber(Scope::Choice),
        |_| {},
        |_| 2_u32,
    );
    assert!(
        again.is_err(),
        "the old finish removed the replacement's key"
    );
    next.open();
    assert_eq!(outcome(&replacement).outcome, Outcome::Done(1));
}
