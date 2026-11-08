//! The coordinator's permits, scopes, jobs and commit (C1-C3).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{
    Caps, Class, Coordinator, FOREGROUND_PERMITS, Generation, JobKey, JobKind, JobPanic, Outcome,
    Scope, Subscriber, Taken, catch,
};

/// How long a test waits for what must happen however late a loaded
/// machine runs it.
const WATCHDOG: Duration = Duration::from_secs(600);

const CAPS: Caps = Caps {
    foreground: FOREGROUND_PERMITS,
    speculative: 1,
};

fn key(name: &str) -> JobKey {
    JobKey {
        kind: JobKind::Loudness,
        path: PathBuf::from(name),
    }
}

fn until(what: &str, done: impl Fn() -> bool) {
    let begun = Instant::now();
    while !done() {
        assert!(begun.elapsed() < WATCHDOG, "{what} never happened");
        thread::sleep(Duration::from_millis(1));
    }
}

const NEVER: &(dyn Fn() -> bool + Sync) = &|| false;
const NOTHING: &(dyn Fn() + Sync) = &|| {};

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
    let held = coord.try_lease(Class::Speculative).unwrap();
    assert!(coord.lease(Class::Speculative, &|| true).is_none());
    let waiting = {
        let coord = coord.clone();
        thread::spawn(move || coord.lease(Class::Speculative, NEVER).map(|l| l.class()))
    };
    thread::sleep(Duration::from_millis(20));
    assert_eq!(coord.held(Class::Speculative), 1);
    drop(held);
    assert_eq!(waiting.join().unwrap(), Some(Class::Speculative));
    assert_eq!(coord.held(Class::Speculative), 0);
}

/// A job gives its permit back across a wait and takes it again after.
#[test]
fn a_job_holds_no_permit_while_it_waits() {
    let coord = Coordinator::new(CAPS);
    let outcome = coord
        .task::<usize>(Class::Speculative)
        .run(NEVER, NOTHING, |ctx| {
            assert_eq!(coord.held(Class::Speculative), 1);
            let during = ctx.idle(|| {
                let other = coord.try_lease(Class::Speculative);
                (coord.held(Class::Speculative), other.is_some())
            });
            assert_eq!(during, Some((1, true)));
            assert_eq!(ctx.class(), Some(Class::Speculative));
            coord.held(Class::Speculative)
        });
    assert_eq!(outcome, Outcome::Done(1));
    assert_eq!(coord.held(Class::Speculative), 0);

    // Maintenance too: released across the wait.
    let outcome = coord
        .task::<usize>(Class::Maintenance)
        .run(NEVER, NOTHING, |ctx| {
            ctx.idle(|| coord.held(Class::Maintenance)).unwrap_or(9)
        });
    assert_eq!(outcome, Outcome::Done(0));
}

/// A job left with no current subscriber is cancelled at its next
/// checkpoint and gives its permit back; its subscriber's take is stale.
#[test]
fn a_cancelled_job_gives_its_permit_back() {
    let coord = Coordinator::new(CAPS);
    let (handle, runner) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Cursor),
    );
    let runner = runner.unwrap();
    let running = thread::spawn(move || {
        runner.run(NEVER, NOTHING, |ctx| {
            while !ctx.checkpoint() {
                thread::sleep(Duration::from_millis(1));
            }
            7
        })
    });
    until("the job's permit", || coord.held(Class::Speculative) == 1);
    assert_eq!(coord.take(&handle), Taken::Pending);
    coord.advance(Scope::Cursor);
    assert_eq!(running.join().unwrap(), Outcome::Cancelled);
    assert_eq!(coord.held(Class::Speculative), 0);
    assert_eq!(coord.take(&handle), Taken::Stale);
}

/// Another scope's request leaves a job alone; dropping its last current
/// handle cancels it, before it has a permit too.
#[test]
fn dropping_the_last_handle_cancels_the_job() {
    let coord = Coordinator::new(CAPS);
    let (handle, runner) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Cursor),
    );
    coord.advance(Scope::Folder);
    let blocker = coord.try_lease(Class::Speculative).unwrap();
    let runs = AtomicUsize::new(0);
    thread::scope(|s| {
        let running = s.spawn(|| {
            runner.unwrap().run(NEVER, NOTHING, |_| {
                runs.fetch_add(1, Ordering::SeqCst);
                1
            })
        });
        thread::sleep(Duration::from_millis(20));
        drop(handle);
        assert_eq!(running.join().unwrap(), Outcome::Cancelled);
    });
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    drop(blocker);
    assert_eq!(coord.held(Class::Speculative), 0);
}

/// A→B→A: back at A, the subscription joins A's job still running and
/// clears its cancel; one computation serves the new subscriber, and the
/// superseded one's take is stale.
#[test]
fn returning_to_a_chart_joins_its_running_job() {
    let coord = Coordinator::new(CAPS);
    let first = coord.subscriber(Scope::Cursor);
    let (old, runner) = coord.begin::<u32>(key("a"), Class::Speculative, first);
    let (go_tx, go) = mpsc::channel::<()>();
    let go = std::sync::Mutex::new(go);
    let runs = AtomicUsize::new(0);
    thread::scope(|s| {
        let running = s.spawn(|| {
            runner.unwrap().run(NEVER, NOTHING, |ctx| {
                runs.fetch_add(1, Ordering::SeqCst);
                go.lock().unwrap().recv_timeout(WATCHDOG).unwrap();
                if ctx.checkpoint() { 0 } else { 42 }
            })
        });
        until("the job's start", || runs.load(Ordering::SeqCst) == 1);
        // B.
        coord.advance(Scope::Cursor);
        let (b, b_runner) = coord.begin::<u32>(
            key("b"),
            Class::Speculative,
            coord.subscriber(Scope::Cursor),
        );
        drop((b, b_runner));
        // Back to A.
        let back = Subscriber {
            scope: Scope::Cursor,
            generation: coord.advance(Scope::Cursor),
        };
        assert_eq!(back.generation, Generation(2));
        let (new, joined) = coord.begin::<u32>(key("a"), Class::Speculative, back);
        assert!(joined.is_none());
        go_tx.send(()).unwrap();
        assert_eq!(running.join().unwrap(), Outcome::Done(42));
        assert_eq!(coord.take(&new), Taken::Ready(Outcome::Done(42)));
        assert_eq!(coord.take(&old), Taken::Stale);
    });
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    // Ended, the key begins anew.
    let (_, again) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Cursor),
    );
    assert!(again.is_some());
}

/// A result is taken only while its subscriber's generation is current.
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

/// A panic is caught at the job's boundary and returned as it is, to the
/// runner and every subscriber, with the permit given back.
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
    assert_eq!(coord.held(Class::Speculative), 0);

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

/// A runner dropped without running ends its job cancelled for its
/// subscribers.
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

/// Promotion moves a running job's permit to the foreground at its next
/// checkpoint, calls the hook once, and frees the speculative permit.
#[test]
fn promotion_moves_the_permit() {
    let coord = Coordinator::new(CAPS);
    let promoted = AtomicUsize::new(0);
    let on_promote = || {
        promoted.fetch_add(1, Ordering::SeqCst);
    };
    let (handle, runner) = coord.begin::<u32>(
        key("a"),
        Class::Speculative,
        coord.subscriber(Scope::Choice),
    );
    let outcome = runner.unwrap().run(NEVER, &on_promote, |ctx| {
        assert_eq!(ctx.class(), Some(Class::Speculative));
        assert!(coord.try_lease(Class::Speculative).is_none());
        handle.promote();
        assert!(!ctx.checkpoint());
        assert!(!ctx.checkpoint());
        assert!(ctx.promoted());
        assert_eq!(ctx.class(), Some(Class::Foreground));
        assert_eq!(coord.held(Class::Foreground), 1);
        assert_eq!(coord.held(Class::Speculative), 0);
        assert!(coord.try_lease(Class::Speculative).is_some());
        // Across a wait it takes a foreground permit again.
        assert_eq!(ctx.idle(|| coord.held(Class::Foreground)), Some(0));
        assert_eq!(ctx.class(), Some(Class::Foreground));
        1
    });
    assert_eq!(outcome, Outcome::Done(1));
    assert_eq!(promoted.load(Ordering::SeqCst), 1);
    assert_eq!(coord.held(Class::Foreground), 0);
}

/// With every foreground permit taken, a promoted job gives its
/// speculative permit back and waits at its checkpoint for a foreground
/// one; a promotion asked before the start runs on a foreground permit
/// from the beginning.
#[test]
fn promotion_waits_for_a_foreground_permit() {
    let coord = Coordinator::new(Caps {
        foreground: 1,
        speculative: 1,
    });
    let blocker = coord.try_lease(Class::Foreground).unwrap();
    let promoted = AtomicUsize::new(0);
    let on_promote = || {
        promoted.fetch_add(1, Ordering::SeqCst);
    };
    let task = coord.task::<Option<Class>>(Class::Speculative);
    let promoter = task.promoter();
    let asked = AtomicUsize::new(0);
    let (returned, before) = thread::scope(|s| {
        let running = s.spawn(|| {
            task.run(NEVER, &on_promote, |ctx| {
                promoter.promote();
                asked.store(1, Ordering::SeqCst);
                assert!(!ctx.checkpoint());
                ctx.class()
            })
        });
        until("the promotion asked", || asked.load(Ordering::SeqCst) == 1);
        // Not `until`: a failure must still let the job go.
        let begun = Instant::now();
        while coord.held(Class::Speculative) != 0 && begun.elapsed() < WATCHDOG {
            thread::sleep(Duration::from_millis(1));
        }
        let returned = coord.held(Class::Speculative) == 0;
        let before = promoted.load(Ordering::SeqCst);
        drop(blocker);
        assert_eq!(
            running.join().unwrap(),
            Outcome::Done(Some(Class::Foreground))
        );
        (returned, before)
    });
    assert!(returned, "the speculative permit was kept while waiting");
    assert_eq!(before, 0);
    assert_eq!(promoted.load(Ordering::SeqCst), 1);

    let task = coord.task::<Option<Class>>(Class::Speculative);
    task.promoter().promote();
    let outcome = task.run(NEVER, &on_promote, |ctx| {
        assert!(ctx.promoted());
        assert!(!ctx.checkpoint());
        ctx.class()
    });
    assert_eq!(outcome, Outcome::Done(Some(Class::Foreground)));
    assert_eq!(promoted.load(Ordering::SeqCst), 2);
    assert_eq!(coord.held(Class::Speculative), 0);
}

/// A checkpoint on another thread (a decode pool's) applies no promotion:
/// the hook must run on the job's own thread.
#[test]
fn promotion_applies_only_on_the_jobs_thread() {
    let coord = Coordinator::new(CAPS);
    let promoted = AtomicUsize::new(0);
    let on_promote = || {
        promoted.fetch_add(1, Ordering::SeqCst);
    };
    let task = coord.task::<()>(Class::Speculative);
    let promoter = task.promoter();
    let outcome = task.run(NEVER, &on_promote, |ctx| {
        promoter.promote();
        thread::scope(|s| {
            s.spawn(|| assert!(!ctx.checkpoint())).join().unwrap();
        });
        assert!(!ctx.promoted());
        assert_eq!(ctx.class(), Some(Class::Speculative));
        assert!(!ctx.checkpoint());
        assert!(ctx.promoted());
    });
    assert_eq!(outcome, Outcome::Done(()));
    assert_eq!(promoted.load(Ordering::SeqCst), 1);
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
