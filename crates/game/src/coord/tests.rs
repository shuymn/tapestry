//! The coordinator's permits, scopes, jobs and commit (C1-C3), one test per
//! reachable cell of the job lifecycle table (the PR's "Job lifecycle").
//! Threads meet at channels. The only clock is a watchdog: a wait that
//! never ends gives up after it, so a broken guard fails instead of
//! hanging.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use super::{
    Caps, Class, Coordinator, Ctx, FOREGROUND_PERMITS, Generation, JobKey, JobKind, JobPanic,
    Outcome, Scope, Subscriber, Taken, catch,
};

/// How long a wait lasts before it gives up: a safety net, not a timing
/// bound.
static WATCHDOG: std::sync::LazyLock<Duration> = std::sync::LazyLock::new(|| {
    // `TAPESTRY_TEST_WATCHDOG` (seconds) shortens it for mutation runs.
    let secs = std::env::var("TAPESTRY_TEST_WATCHDOG").ok();
    Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(600))
});

const CAPS: Caps = Caps {
    foreground: FOREGROUND_PERMITS,
    speculative: 1,
};

/// Caps with one permit of each bounded class.
const ONE_EACH: Caps = Caps {
    foreground: 1,
    speculative: 1,
};

const NEVER: &(dyn Fn() -> bool + Sync) = &|| false;
const NOTHING: &(dyn Fn(Class) + Sync) = &|_| {};

fn key(name: &str) -> JobKey {
    JobKey {
        kind: JobKind::Loudness,
        path: PathBuf::from(name),
    }
}

/// A stop that says stop only once the watchdog has run out.
fn watchdog() -> impl Fn() -> bool + Sync {
    let begun = Instant::now();
    move || begun.elapsed() > *WATCHDOG
}

/// A one-shot event between threads.
struct Event(Mutex<Receiver<()>>, Sender<()>);

impl Event {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self(Mutex::new(rx), tx)
    }

    fn fire(&self) {
        let _ = self.1.send(());
    }

    fn wait(&self, what: &str) {
        let got = self.0.lock().unwrap().recv_timeout(*WATCHDOG);
        assert!(got.is_ok(), "{what} never happened");
    }
}

/// The classes a run's hook was called with, in order.
#[derive(Default)]
struct Hook(Mutex<Vec<Class>>);

impl Hook {
    fn call(&self, class: Class) {
        self.0.lock().unwrap().push(class);
    }

    fn calls(&self) -> Vec<Class> {
        self.0.lock().unwrap().clone()
    }
}

fn held(coord: &Coordinator) -> (usize, usize) {
    (
        coord.held(Class::Speculative),
        coord.held(Class::Foreground),
    )
}

/// Runs the checkpoint until it says stop, yielding between.
fn until_cancelled(ctx: &Ctx<'_>) {
    while !ctx.checkpoint() {
        thread::yield_now();
    }
}

/// The speculative cap is `min(setting, 2, cores − 2)`: none on two cores
/// or fewer, none with the setting 0; the foreground keeps 4.
#[test]
fn the_caps_follow_the_machine() {
    let speculative = |setting, cores| Caps::for_machine(setting, cores).speculative;
    assert_eq!(speculative(1, 1), 0);
    assert_eq!(speculative(2, 2), 0);
    assert_eq!(speculative(0, 16), 0);
    assert_eq!(speculative(1, 3), 1);
    assert_eq!(speculative(2, 3), 1);
    assert_eq!(speculative(2, 4), 2);
    assert_eq!(speculative(5, 16), 2);
    assert_eq!(speculative(1, 16), 1);
    assert_eq!(speculative(usize::MAX, usize::MAX), 2);
    assert_eq!(Caps::for_machine(1, 0).foreground, FOREGROUND_PERMITS);
    assert!(Caps::this_machine().speculative <= 2);
}

/// Each bounded class stops at its cap and frees a permit when a lease is
/// dropped; maintenance is never refused, even with no other permit.
#[test]
fn permits_stop_at_their_cap_and_maintenance_is_never_refused() {
    let coord = Coordinator::new(Caps {
        foreground: 4,
        speculative: 2,
    });
    let fg: Vec<_> = (0..4)
        .filter_map(|_| coord.try_lease(Class::Foreground))
        .collect();
    assert_eq!(fg.len(), 4);
    assert!(coord.try_lease(Class::Foreground).is_none());
    let mut spec: Vec<_> = (0..2)
        .filter_map(|_| coord.try_lease(Class::Speculative))
        .collect();
    assert_eq!(spec.len(), 2);
    assert!(coord.try_lease(Class::Speculative).is_none());
    assert_eq!(coord.held(Class::Speculative), 2);
    spec.pop();
    assert_eq!(coord.held(Class::Speculative), 1);
    assert!(coord.try_lease(Class::Speculative).is_some());
    drop(fg);
    assert_eq!(coord.held(Class::Foreground), 0);

    let none = Coordinator::new(Caps {
        foreground: 0,
        speculative: 0,
    });
    let kept: Vec<_> = (0..100)
        .filter_map(|_| none.try_lease(Class::Maintenance))
        .collect();
    assert_eq!(kept.len(), 100);
    assert_eq!(none.held(Class::Maintenance), 100);
    assert!(none.try_lease(Class::Speculative).is_none());
    assert!(none.lease(Class::Speculative, NEVER).is_none());
    drop(kept);
    assert_eq!(none.held(Class::Maintenance), 0);
}

/// A wait for a permit gives up when asked, and gets one once another is
/// dropped.
#[test]
fn a_wait_for_a_permit_gives_up_or_gets_the_next_free_one() {
    let coord = Coordinator::new(CAPS);
    let held_one = coord.try_lease(Class::Speculative).unwrap();
    assert!(coord.lease(Class::Speculative, &|| true).is_none());
    let asked = Event::new();
    let give_up = watchdog();
    let got = thread::scope(|s| {
        let waiting = s.spawn(|| {
            let look = || {
                asked.fire();
                give_up()
            };
            coord.lease(Class::Speculative, &look).map(|l| l.class())
        });
        asked.wait("the wait's first look");
        drop(held_one);
        waiting.join().unwrap()
    });
    assert_eq!(got, Some(Class::Speculative));
    assert_eq!(coord.held(Class::Speculative), 0);
}

/// Idle: a job gives its permit back across a wait and takes it again
/// after; maintenance too.
#[test]
fn a_job_holds_no_permit_while_it_waits() {
    let coord = Coordinator::new(CAPS);
    let outcome = coord
        .task::<(Option<(usize, bool)>, usize)>(Class::Speculative)
        .run(NEVER, NOTHING, |ctx| {
            let during = ctx.idle(|| {
                let other = coord.try_lease(Class::Speculative);
                (coord.held(Class::Speculative), other.is_some())
            });
            (during, coord.held(Class::Speculative))
        });
    assert_eq!(outcome, Outcome::Done((Some((1, true)), 1)));
    assert_eq!(coord.held(Class::Speculative), 0);

    let outcome = coord
        .task::<Option<usize>>(Class::Maintenance)
        .run(NEVER, NOTHING, |ctx| {
            ctx.idle(|| coord.held(Class::Maintenance))
        });
    assert_eq!(outcome, Outcome::Done(Some(0)));
}

/// Begin, current subscriber, no job of the key: a job begins and its
/// result is taken.
#[test]
fn a_current_subscriber_begins_a_job_and_takes_its_result() {
    let coord = Coordinator::new(CAPS);
    let (handle, runner) =
        coord.begin::<u32>(key("a"), Class::Foreground, coord.subscriber(Scope::Choice));
    assert_eq!(coord.take(&handle), Taken::Pending);
    assert_eq!(runner.unwrap().run(NEVER, NOTHING, |_| 5), Outcome::Done(5));
    assert_eq!(coord.take(&handle), Taken::Ready(Outcome::Done(5)));
    assert_eq!(held(&coord), (0, 0));
}

/// Begin with a stale subscriber: the job is asked to cancel at once and
/// ends cancelled without computing; the take is stale.
#[test]
fn a_stale_subscriber_begins_a_job_already_cancelled() {
    let coord = Coordinator::new(CAPS);
    let stale = coord.subscriber(Scope::Choice);
    coord.advance(Scope::Choice);
    let (handle, runner) = coord.begin::<u32>(key("a"), Class::Speculative, stale);
    let runs = AtomicUsize::new(0);
    let outcome = runner.unwrap().run(NEVER, NOTHING, |_| {
        runs.fetch_add(1, Ordering::SeqCst);
        1
    });
    assert_eq!(outcome, Outcome::Cancelled);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(coord.take(&handle), Taken::Stale);
    assert_eq!(held(&coord), (0, 0));
}

/// Scope advance: a running job left with no current subscriber sees its
/// cancel at the next checkpoint and ends cancelled though its work
/// returns a value, its permit given back; another scope's advance leaves
/// it alone.
#[test]
fn an_advanced_scope_cancels_its_running_job() {
    let coord = Coordinator::new(CAPS);
    let (handle, runner) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Cursor),
    );
    let (started, other) = (Event::new(), Event::new());
    let stop = watchdog();
    let (before, outcome) = thread::scope(|s| {
        let running = s.spawn(|| {
            runner.unwrap().run(&stop, NOTHING, |ctx| {
                started.fire();
                other.wait("the other scope's advance");
                let after_other = ctx.checkpoint();
                until_cancelled(ctx);
                if after_other { 0 } else { 7 }
            })
        });
        started.wait("the job's start");
        let before = (held(&coord), coord.take(&handle));
        coord.advance(Scope::Folder);
        other.fire();
        coord.advance(Scope::Cursor);
        (before, running.join().unwrap())
    });
    assert_eq!(before, ((1, 0), Taken::Pending));
    assert_eq!(outcome, Outcome::Cancelled);
    assert_eq!(held(&coord), (0, 0));
    assert_eq!(coord.take(&handle), Taken::Stale);
}

/// Handle drop: the last current handle's drop cancels a job still waiting
/// for its permit, which then never computes.
#[test]
fn dropping_the_last_handle_cancels_a_waiting_job() {
    let coord = Coordinator::new(CAPS);
    let (handle, runner) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Cursor),
    );
    let blocker = coord.try_lease(Class::Speculative).unwrap();
    let runs = AtomicUsize::new(0);
    let asked = Event::new();
    let give_up = watchdog();
    let outcome = thread::scope(|s| {
        let running = s.spawn(|| {
            let stop = || {
                asked.fire();
                give_up()
            };
            runner.unwrap().run(&stop, NOTHING, |_| {
                runs.fetch_add(1, Ordering::SeqCst);
                1
            })
        });
        asked.wait("the admission's first look");
        drop(handle);
        running.join().unwrap()
    });
    drop(blocker);
    assert_eq!(outcome, Outcome::Cancelled);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(held(&coord), (0, 0));
}

/// Scope advance while the job waits for admission: it is cancelled
/// there and never computes.
#[test]
fn an_advanced_scope_cancels_a_waiting_job() {
    let coord = Coordinator::new(CAPS);
    let (handle, runner) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Choice),
    );
    let blocker = coord.try_lease(Class::Speculative).unwrap();
    let runs = AtomicUsize::new(0);
    let asked = Event::new();
    let give_up = watchdog();
    let outcome = thread::scope(|s| {
        let running = s.spawn(|| {
            let stop = || {
                asked.fire();
                give_up()
            };
            runner.unwrap().run(&stop, NOTHING, |_| {
                runs.fetch_add(1, Ordering::SeqCst);
                1
            })
        });
        asked.wait("the admission's first look");
        coord.advance(Scope::Choice);
        running.join().unwrap()
    });
    drop(blocker);
    assert_eq!(outcome, Outcome::Cancelled);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(coord.take(&handle), Taken::Stale);
    assert_eq!(held(&coord), (0, 0));
}

/// Handle drop with an old snapshot: a stale handle dropped after a
/// current subscriber joined does not cancel the job; the decision is
/// made on the live generations.
#[test]
fn a_stale_handles_drop_leaves_a_job_with_a_current_subscriber() {
    let coord = Coordinator::new(CAPS);
    let (old, runner) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Cursor),
    );
    coord.advance(Scope::Cursor);
    let (new, joined) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Cursor),
    );
    assert!(joined.is_none());
    // A stale join beside the current subscriber changes nothing either.
    let (late, joined) = coord.begin::<u32>(key("a"), Class::Speculative, old.subscriber());
    assert!(joined.is_none());
    drop(old);
    drop(late);
    let outcome = runner
        .unwrap()
        .run(NEVER, NOTHING, |ctx| if ctx.checkpoint() { 0 } else { 3 });
    assert_eq!(outcome, Outcome::Done(3));
    assert_eq!(coord.take(&new), Taken::Ready(Outcome::Done(3)));
}

/// Join with a stale subscriber: a job asked to cancel stays asked; it is
/// not kept alive by a subscriber that is not current.
#[test]
fn a_stale_join_does_not_withdraw_a_cancel() {
    let coord = Coordinator::new(CAPS);
    let first = coord.subscriber(Scope::Cursor);
    let (_old, runner) = coord.begin::<u32>(key("a"), Class::Speculative, first);
    coord.advance(Scope::Cursor);
    let (late, joined) = coord.begin::<u32>(key("a"), Class::Speculative, first);
    assert!(joined.is_none());
    let runs = AtomicUsize::new(0);
    let outcome = runner.unwrap().run(NEVER, NOTHING, |_| {
        runs.fetch_add(1, Ordering::SeqCst);
        1
    });
    assert_eq!(outcome, Outcome::Cancelled);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(coord.take(&late), Taken::Stale);
}

/// A→B→A before the job saw its cancel: back at A, the subscription joins
/// the running job and withdraws the cancel; one computation serves the
/// new subscriber, and the superseded one's take is stale.
#[test]
fn returning_before_the_cancel_is_seen_joins_the_running_job() {
    let coord = Coordinator::new(CAPS);
    let (old, runner) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Cursor),
    );
    let (started, go) = (Event::new(), Event::new());
    let runs = AtomicUsize::new(0);
    let (joined, back, outcome) = thread::scope(|s| {
        let running = s.spawn(|| {
            runner.unwrap().run(NEVER, NOTHING, |ctx| {
                runs.fetch_add(1, Ordering::SeqCst);
                started.fire();
                go.wait("the return to A");
                if ctx.checkpoint() { 0 } else { 42 }
            })
        });
        started.wait("the job's start");
        // B.
        coord.advance(Scope::Cursor);
        drop(coord.begin::<u32>(
            key("b"),
            Class::Speculative,
            coord.subscriber(Scope::Cursor),
        ));
        // Back to A.
        let back = Subscriber {
            scope: Scope::Cursor,
            generation: coord.advance(Scope::Cursor),
        };
        let (new, runner) = coord.begin::<u32>(key("a"), Class::Speculative, back);
        go.fire();
        (runner.is_none(), new, running.join().unwrap())
    });
    assert!(joined);
    assert_eq!(back.subscriber().generation, Generation(2));
    assert_eq!(outcome, Outcome::Done(42));
    assert_eq!(coord.take(&back), Taken::Ready(Outcome::Done(42)));
    assert_eq!(coord.take(&old), Taken::Stale);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let (_, again) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Cursor),
    );
    assert!(again.is_some());
}

/// A→B→A after the job saw its cancel: the cancel is terminal. The return
/// to A begins a fresh job; the old one ends cancelled although its
/// cleanup returns a value, and only the fresh one's result reaches A.
#[test]
fn a_seen_cancel_is_terminal_and_a_return_begins_afresh() {
    let coord = Coordinator::new(CAPS);
    let (_old, runner) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Cursor),
    );
    let (started, seen, go) = (Event::new(), Event::new(), Event::new());
    let stop = watchdog();
    let (old_outcome, new, fresh) = thread::scope(|s| {
        let running = s.spawn(|| {
            runner.unwrap().run(&stop, NOTHING, |ctx| {
                started.fire();
                until_cancelled(ctx);
                seen.fire();
                go.wait("the return to A");
                // A partial value from its cleanup.
                99
            })
        });
        started.wait("the job's start");
        coord.advance(Scope::Cursor);
        seen.wait("the cancel seen");
        let (new, fresh) =
            coord.begin::<u32>(key("a"), Class::Foreground, coord.subscriber(Scope::Cursor));
        go.fire();
        (running.join().unwrap(), new, fresh)
    });
    assert_eq!(old_outcome, Outcome::Cancelled);
    let fresh = fresh.expect("a fresh job");
    assert_eq!(coord.take(&new), Taken::Pending);
    assert_eq!(fresh.run(NEVER, NOTHING, |_| 5), Outcome::Done(5));
    assert_eq!(coord.take(&new), Taken::Ready(Outcome::Done(5)));
    assert_eq!(held(&coord), (0, 0));
}

/// Take: a result is given only while its subscriber's generation is
/// current.
#[test]
fn a_stale_take_gets_nothing() {
    let coord = Coordinator::new(CAPS);
    let (handle, runner) =
        coord.begin::<u32>(key("a"), Class::Foreground, coord.subscriber(Scope::Choice));
    assert_eq!(runner.unwrap().run(NEVER, NOTHING, |_| 5), Outcome::Done(5));
    assert_eq!(coord.take(&handle), Taken::Ready(Outcome::Done(5)));
    coord.advance(Scope::Choice);
    assert_eq!(coord.take(&handle), Taken::Stale);
}

/// Panic in the work: caught at the job's boundary and returned as it is,
/// to the runner and every subscriber, with the permit given back.
#[test]
fn a_panic_is_returned_without_policy() {
    let coord = Coordinator::new(CAPS);
    let sub = coord.subscriber(Scope::Choice);
    let (handle, runner) = coord.begin::<u32>(key("a"), Class::Speculative, sub);
    let (joined, none) = coord.begin::<u32>(key("a"), Class::Speculative, sub);
    assert!(none.is_none());
    let outcome = runner
        .unwrap()
        .run(NEVER, NOTHING, |_| -> u32 { panic!("a job failed (test)") });
    let panicked = Outcome::Panicked(JobPanic("a job failed (test)".to_owned()));
    assert_eq!(outcome, panicked);
    assert_eq!(coord.take(&handle), Taken::Ready(panicked.clone()));
    assert_eq!(coord.take(&joined), Taken::Ready(panicked));
    assert_eq!(held(&coord), (0, 0));

    let name = String::from("x");
    assert_eq!(
        catch(|| -> () { panic!("{name} failed (test)") }),
        Err(JobPanic("x failed (test)".to_owned()))
    );
    assert_eq!(
        catch(|| -> () { std::panic::panic_any(3_u8) }),
        Err(JobPanic("a panic without a message".to_owned()))
    );
}

/// Panic in the admission or a hook: the owner's stop or the class hook
/// panicking is caught as the work's panic is, with no permit left held.
#[test]
fn a_panic_in_the_admission_or_a_hook_is_caught() {
    let coord = Coordinator::new(CAPS);
    let outcome = coord.task::<u32>(Class::Speculative).run(
        &|| panic!("the stop failed (test)"),
        NOTHING,
        |_| 1,
    );
    assert_eq!(
        outcome,
        Outcome::Panicked(JobPanic("the stop failed (test)".to_owned()))
    );
    assert_eq!(held(&coord), (0, 0));

    let task = coord.task::<u32>(Class::Speculative);
    task.promoter().promote();
    let outcome = task.run(NEVER, &|_| panic!("the hook failed (test)"), |_| 1);
    assert_eq!(
        outcome,
        Outcome::Panicked(JobPanic("the hook failed (test)".to_owned()))
    );
    assert_eq!(held(&coord), (0, 0));
}

/// A runner dropped unrun ends its job cancelled for its subscribers.
#[test]
fn a_runner_dropped_unrun_ends_its_job_cancelled() {
    let coord = Coordinator::new(CAPS);
    let (handle, runner) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Choice),
    );
    drop(runner);
    assert_eq!(coord.take(&handle), Taken::Ready(Outcome::Cancelled));
}

/// Promote while running speculative: the permit moves to the foreground
/// at the next checkpoint and the hook is called once with it; promoting
/// again changes nothing; idle takes a foreground permit again.
#[test]
fn promotion_moves_the_permit() {
    let coord = Coordinator::new(CAPS);
    let hook = Hook::default();
    let (handle, runner) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Choice),
    );
    let mut seen = Vec::new();
    let outcome = runner.unwrap().run(NEVER, &|c| hook.call(c), |ctx| {
        seen.push((ctx.class(), held(&coord)));
        handle.promote();
        assert!(!ctx.checkpoint());
        seen.push((ctx.class(), held(&coord)));
        handle.promote();
        assert!(!ctx.checkpoint());
        let idle = ctx.idle(|| held(&coord));
        seen.push((ctx.class(), idle.unwrap_or((9, 9))));
        1
    });
    assert_eq!(outcome, Outcome::Done(1));
    assert_eq!(
        seen,
        vec![
            (Some(Class::Speculative), (1, 0)),
            (Some(Class::Foreground), (0, 1)),
            (Some(Class::Foreground), (0, 0)),
        ]
    );
    assert_eq!(hook.calls(), vec![Class::Foreground]);
    assert_eq!(held(&coord), (0, 0));
}

/// Promote while running, every foreground permit taken: the job gives its
/// speculative permit back and waits at its checkpoint for a foreground
/// one, holding none; the hook is called only once it has it.
#[test]
fn promotion_waits_for_a_foreground_permit_holding_none() {
    let coord = Coordinator::new(ONE_EACH);
    let blocker = coord.try_lease(Class::Foreground).unwrap();
    let hook = Hook::default();
    let task = coord.task::<Option<Class>>(Class::Speculative);
    let promoter = task.promoter();
    let waiting = Event::new();
    let (stop, give_up) = (watchdog(), watchdog());
    let (spec_free, calls_before, outcome) = thread::scope(|s| {
        let running = s.spawn(|| {
            task.run(&stop, &|c| hook.call(c), |ctx| {
                promoter.promote();
                waiting.fire();
                // Waits here for the foreground permit.
                let _ = ctx.checkpoint();
                ctx.class()
            })
        });
        waiting.wait("the promotion asked");
        // Free only once the job gave it back to wait.
        let spec = coord.lease(Class::Speculative, &give_up);
        let spec_free = spec.is_some();
        let calls_before = hook.calls();
        drop(spec);
        drop(blocker);
        (spec_free, calls_before, running.join().unwrap())
    });
    assert!(spec_free, "the speculative permit was kept while waiting");
    assert!(calls_before.is_empty());
    assert_eq!(outcome, Outcome::Done(Some(Class::Foreground)));
    assert_eq!(hook.calls(), vec![Class::Foreground]);
    assert_eq!(held(&coord), (0, 0));
}

/// Promote while queued for admission (its class full, the foreground
/// free): the admission takes a foreground permit, with the speculative
/// blocker still held and no speculative permit taken.
#[test]
fn promotion_while_queued_admits_in_the_foreground() {
    let coord = Coordinator::new(CAPS);
    let blocker = coord.try_lease(Class::Speculative).unwrap();
    let hook = Hook::default();
    let task = coord.task::<(Option<Class>, (usize, usize))>(Class::Speculative);
    let promoter = task.promoter();
    let asked = Event::new();
    let give_up = watchdog();
    let outcome = thread::scope(|s| {
        let running = s.spawn(|| {
            let stop = || {
                asked.fire();
                give_up()
            };
            task.run(&stop, &|c| hook.call(c), |ctx| (ctx.class(), held(&coord)))
        });
        asked.wait("the admission's first look");
        promoter.promote();
        running.join().unwrap()
    });
    drop(blocker);
    assert_eq!(outcome, Outcome::Done((Some(Class::Foreground), (1, 1))));
    assert_eq!(hook.calls(), vec![Class::Foreground]);
    assert_eq!(held(&coord), (0, 0));
}

/// Demote while running promoted: the permit moves back to the job's own
/// class at the next checkpoint, the hook called with it.
#[test]
fn demotion_moves_the_permit_back() {
    let coord = Coordinator::new(CAPS);
    let hook = Hook::default();
    let task = coord.task::<Vec<(usize, usize)>>(Class::Speculative);
    let promoter = task.promoter();
    let outcome = task.run(NEVER, &|c| hook.call(c), |ctx| {
        promoter.promote();
        let _ = ctx.checkpoint();
        let promoted = held(&coord);
        promoter.demote();
        let _ = ctx.checkpoint();
        vec![promoted, held(&coord)]
    });
    assert_eq!(outcome, Outcome::Done(vec![(0, 1), (1, 0)]));
    assert_eq!(hook.calls(), vec![Class::Foreground, Class::Speculative]);
    assert_eq!(held(&coord), (0, 0));
}

/// Demote while running promoted with the job's own class full: it gives
/// its foreground permit back and waits for one of its class, holding
/// none.
#[test]
fn demotion_waits_for_its_own_class_holding_none() {
    let coord = Coordinator::new(ONE_EACH);
    let hook = Hook::default();
    let task = coord.task::<Option<Class>>(Class::Speculative);
    let promoter = task.promoter();
    let (promoted, taken) = (Event::new(), Event::new());
    let (stop, give_up) = (watchdog(), watchdog());
    let (fg_free, outcome) = thread::scope(|s| {
        let running = s.spawn(|| {
            task.run(&stop, &|c| hook.call(c), |ctx| {
                promoter.promote();
                let _ = ctx.checkpoint();
                promoted.fire();
                taken.wait("the speculative permit taken");
                promoter.demote();
                // Waits here for a speculative permit.
                let _ = ctx.checkpoint();
                ctx.class()
            })
        });
        promoted.wait("the promotion");
        let blocker = coord.try_lease(Class::Speculative);
        taken.fire();
        // Free only once the job gave it back to wait.
        let fg = coord.lease(Class::Foreground, &give_up);
        let fg_free = fg.is_some();
        drop(fg);
        drop(blocker);
        (fg_free, running.join().unwrap())
    });
    assert!(fg_free, "the foreground permit was kept while waiting");
    assert_eq!(outcome, Outcome::Done(Some(Class::Speculative)));
    assert_eq!(hook.calls(), vec![Class::Foreground, Class::Speculative]);
    assert_eq!(held(&coord), (0, 0));
}

/// Demote while queued after a promotion: the admission is back at the
/// job's own class, and the hook is never called.
#[test]
fn demotion_while_queued_admits_in_its_own_class() {
    let coord = Coordinator::new(ONE_EACH);
    let fg = coord.try_lease(Class::Foreground).unwrap();
    let spec = coord.try_lease(Class::Speculative).unwrap();
    let hook = Hook::default();
    let task = coord.task::<Option<Class>>(Class::Speculative);
    let promoter = task.promoter();
    promoter.promote();
    let asked = Event::new();
    let give_up = watchdog();
    let outcome = thread::scope(|s| {
        let running = s.spawn(|| {
            let stop = || {
                asked.fire();
                give_up()
            };
            task.run(&stop, &|c| hook.call(c), |ctx| ctx.class())
        });
        asked.wait("the admission's first look");
        promoter.demote();
        drop(spec);
        running.join().unwrap()
    });
    drop(fg);
    assert_eq!(outcome, Outcome::Done(Some(Class::Speculative)));
    assert!(hook.calls().is_empty());
    assert_eq!(held(&coord), (0, 0));
}

/// Another thread (a decode pool's) changes no permit: its checkpoint
/// applies no promotion and its idle keeps the permit; the job's own
/// thread applies the promotion.
#[test]
fn only_the_jobs_thread_changes_its_permit() {
    let coord = Coordinator::new(CAPS);
    let hook = Hook::default();
    let task = coord.task::<Vec<(Option<Class>, (usize, usize))>>(Class::Speculative);
    let promoter = task.promoter();
    let outcome = task.run(NEVER, &|c| hook.call(c), |ctx| {
        promoter.promote();
        let other = thread::scope(|s| {
            s.spawn(|| {
                let stop = ctx.checkpoint();
                (stop, ctx.idle(|| held(&coord)))
            })
            .join()
            .unwrap()
        });
        assert_eq!(other, (false, Some((1, 0))));
        let before = (ctx.class(), held(&coord));
        let calls = hook.calls().len();
        let _ = ctx.checkpoint();
        vec![
            before,
            (ctx.class(), (calls, 0)),
            (ctx.class(), held(&coord)),
        ]
    });
    assert_eq!(
        outcome,
        Outcome::Done(vec![
            (Some(Class::Speculative), (1, 0)),
            (Some(Class::Foreground), (0, 0)),
            (Some(Class::Foreground), (0, 1)),
        ])
    );
    assert_eq!(hook.calls(), vec![Class::Foreground]);
}

/// A task's owner stops it through `stop`: while it waits for a permit,
/// or at a checkpoint; with no speculative permit at all, it never runs.
#[test]
fn a_task_stops_when_its_owner_says_so() {
    let coord = Coordinator::new(CAPS);
    let blocker = coord.try_lease(Class::Speculative).unwrap();
    let outcome = coord
        .task::<u32>(Class::Speculative)
        .run(&|| true, NOTHING, |_| 1);
    assert_eq!(outcome, Outcome::Cancelled);
    drop(blocker);
    let outcome = coord
        .task::<u32>(Class::Speculative)
        .run(NEVER, NOTHING, |ctx| u32::from(ctx.checkpoint()));
    assert_eq!(outcome, Outcome::Done(0));

    let off = Coordinator::new(Caps::for_machine(1, 2));
    let outcome = off
        .task::<u32>(Class::Speculative)
        .run(NEVER, NOTHING, |_| 1);
    assert_eq!(outcome, Outcome::Cancelled);
}
