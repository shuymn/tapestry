//! The permit observer (E21): every permit taken, moved, revoked and
//! given back, and every job's wait, seen once with its task, in the
//! state lock's order.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use super::{Caps, Class, Coordinator, PermitChange, PermitEvent};

const WAIT: Duration = Duration::from_secs(10);

const ONE_EACH: Caps = Caps {
    foreground: 1,
    speculative: 1,
};

/// The changes seen, and a wake for each.
#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<PermitChange>>,
    changed: Condvar,
}

impl Recorder {
    fn on(coord: &Coordinator) -> Arc<Self> {
        let rec = Arc::new(Self::default());
        let r = Arc::clone(&rec);
        coord.observe_permits(Arc::new(move |c| {
            r.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(c);
            r.changed.notify_all();
        }));
        rec
    }

    fn all(&self) -> Vec<PermitChange> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits until a change matches `want`.
    fn wait_for(&self, what: &str, want: impl Fn(&PermitChange) -> bool) {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let (seen, _) = self
            .changed
            .wait_timeout_while(seen, WAIT, |s| !s.iter().any(&want))
            .unwrap_or_else(PoisonError::into_inner);
        assert!(seen.iter().any(want), "never seen: {what}: {seen:?}");
    }

    fn of(&self, task: u64) -> Vec<(PermitEvent, Class)> {
        self.all()
            .iter()
            .filter(|c| c.task == task)
            .map(|c| (c.event, c.class))
            .collect()
    }
}

/// A one-shot signal between threads.
#[derive(Default)]
struct Signal(Mutex<bool>, Condvar);

impl Signal {
    fn fire(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.1.notify_all();
    }

    fn wait(&self, what: &str) {
        let s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (s, _) = self
            .1
            .wait_timeout_while(s, WAIT, |f| !*f)
            .unwrap_or_else(PoisonError::into_inner);
        assert!(*s, "never signalled: {what}");
    }
}

/// The invariants of a record: each give pairs with an earlier take of
/// its task and class (anonymous ones per class), no task gives twice for
/// one take, the counts stay within 0 and the cap, and every opened wait
/// is closed exactly once.
fn check(seen: &[PermitChange]) {
    let mut held: Vec<(u64, Class)> = Vec::new();
    let mut waits: Vec<u64> = Vec::new();
    for c in seen {
        assert!(
            c.cap.is_none_or(|cap| c.count <= cap),
            "over the cap: {c:?}"
        );
        match c.event {
            PermitEvent::Take => held.push((c.task, c.class)),
            PermitEvent::Free => {
                let at = held
                    .iter()
                    .position(|&h| h == (c.task, c.class))
                    .unwrap_or_else(|| panic!("a give with no take: {c:?} in {seen:?}"));
                held.remove(at);
            }
            PermitEvent::Wait => {
                assert!(!waits.contains(&c.task), "opened twice: {c:?}");
                assert!(c.cap.is_some(), "a wait on a class with no cap: {c:?}");
                waits.push(c.task);
            }
            PermitEvent::Rewait | PermitEvent::Retry => {
                assert!(waits.contains(&c.task), "no open wait: {c:?}");
            }
            PermitEvent::Unwait => {
                let at = waits.iter().position(|&t| t == c.task).unwrap();
                waits.remove(at);
            }
        }
        if c.event == PermitEvent::Take && waits.contains(&c.task) {
            waits.retain(|&t| t != c.task);
        }
    }
    assert!(waits.is_empty(), "waits left open: {waits:?} in {seen:?}");
}

/// A (speculative) is promoted: its move takes fg, then gives spec. B
/// takes spec. C (foreground) waits. A is revoked: one give of fg, A's,
/// and C takes fg. A's lease then drops and gives nothing.
#[test]
fn moves_revocations_and_gives_are_seen_once_with_their_task() {
    use Class::{Foreground as Fg, Speculative as Spec};
    use PermitEvent::{Free, Take, Wait};
    let coord = Coordinator::new(ONE_EACH);
    let rec = Recorder::on(&coord);
    let a = coord.task::<()>(Spec);
    let (a_id, promoter) = (a.id, a.promoter());
    let b = coord.task::<()>(Spec);
    let b_id = b.id;
    let c = coord.task::<()>(Fg);
    let c_id = c.id;
    let (a_moved, a_end, b_in, b_end) = (
        Signal::default(),
        Signal::default(),
        Signal::default(),
        Signal::default(),
    );
    let never = || false;
    thread::scope(|s| {
        s.spawn(|| {
            a.run(&never, &|_| {}, |ctx| {
                promoter.promote();
                assert!(!ctx.checkpoint());
                a_moved.fire();
                a_end.wait("A's end");
            })
        });
        a_moved.wait("A's move");
        s.spawn(|| {
            b.run(&never, &|_| {}, |_| {
                b_in.fire();
                b_end.wait("B's end");
            })
        });
        b_in.wait("B's take");
        let running = s.spawn(|| c.run(&never, &|_| {}, |_| {}));
        rec.wait_for("C's wait", |e| {
            e.task == c_id && e.event == PermitEvent::Wait
        });
        promoter.revoke();
        running.join().unwrap();
        a_end.fire();
        b_end.fire();
    });
    let seen = rec.all();
    check(&seen);
    assert_eq!(
        rec.of(a_id),
        [(Take, Spec), (Take, Fg), (Free, Spec), (Free, Fg)]
    );
    assert_eq!(rec.of(b_id), [(Take, Spec), (Free, Spec)]);
    assert_eq!(rec.of(c_id), [(Wait, Fg), (Take, Fg), (Free, Fg)]);
    // C's take follows A's revocation.
    let at = |task, event, class| {
        seen.iter()
            .position(|e| e.task == task && e.event == event && e.class == class)
            .unwrap()
    };
    assert!(at(a_id, Free, Fg) < at(c_id, Take, Fg));
    // The move takes fg before it gives spec, each count after it.
    let a_events: Vec<(PermitEvent, usize)> = seen
        .iter()
        .filter(|e| e.task == a_id)
        .map(|e| (e.event, e.count))
        .collect();
    assert_eq!(a_events, [(Take, 1), (Take, 1), (Free, 0), (Free, 0)]);
}

/// D waits on spec while it is held and is promoted while its thread is
/// held before its retry: the class change is told by the promotion, at
/// once; its retry and take follow. E waits and is stopped: its wait
/// closes with no take. Anonymous leases are task 0; maintenance has no
/// cap.
#[test]
fn a_wait_is_opened_reclassed_and_closed() {
    use Class::{Foreground as Fg, Maintenance, Speculative as Spec};
    use PermitEvent::{Free, Retry, Rewait, Take, Unwait, Wait};
    let coord = Coordinator::new(ONE_EACH);
    let rec = Recorder::on(&coord);
    let held = coord.try_lease(Spec).unwrap();
    let gate = coord.cut_retry();
    let d = coord.task::<()>(Spec);
    let (d_id, promoter) = (d.id, d.promoter());
    let never = || false;
    thread::scope(|s| {
        let running = s.spawn(|| d.run(&never, &|_| {}, |_| {}));
        assert!(gate.reached(WAIT), "D never retried");
        promoter.promote();
        // Told by the promotion, before D's thread tries again.
        assert_eq!(rec.of(d_id), [(Wait, Spec), (Rewait, Fg)]);
        gate.open();
        running.join().unwrap();
    });
    assert_eq!(
        rec.of(d_id),
        [
            (Wait, Spec),
            (Rewait, Fg),
            (Retry, Fg),
            (Take, Fg),
            (Free, Fg)
        ]
    );

    // E: stopped while it waits.
    let e = coord.task::<()>(Spec);
    let e_id = e.id;
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        let running = s.spawn(|| e.run(&|| stop.load(Ordering::SeqCst), &|_| {}, |_| {}));
        rec.wait_for("E's wait", |c| c.task == e_id && c.event == Wait);
        stop.store(true, Ordering::SeqCst);
        running.join().unwrap();
    });
    assert_eq!(rec.of(e_id), [(Wait, Spec), (Unwait, Spec)]);
    drop(held);

    // Anonymous and maintenance leases.
    let before = rec.all().len();
    drop(coord.try_lease(Fg).unwrap());
    drop(coord.try_lease(Maintenance).unwrap());
    let tail: Vec<_> = rec.all()[before..]
        .iter()
        .map(|c| (c.event, c.task, c.class, c.count, c.cap))
        .collect();
    assert_eq!(
        tail,
        [
            (Take, 0, Fg, 1, Some(1)),
            (Free, 0, Fg, 0, Some(1)),
            (Take, 0, Maintenance, 1, None),
            (Free, 0, Maintenance, 0, None),
        ]
    );
    check(&rec.all());
}

/// Two holders in one class: the give is the giver's, not another
/// holder's.
#[test]
fn a_give_is_its_holders_among_two() {
    use Class::Foreground as Fg;
    use PermitEvent::{Free, Take};
    let coord = Coordinator::new(Caps {
        foreground: 2,
        speculative: 1,
    });
    let rec = Recorder::on(&coord);
    let f1 = coord.task::<()>(Fg);
    let f2 = coord.task::<()>(Fg);
    let (f1_id, f2_id) = (f1.id, f2.id);
    assert!(f1_id < f2_id);
    let (f1_in, f2_in, f1_end) = (Signal::default(), Signal::default(), Signal::default());
    let never = || false;
    thread::scope(|s| {
        s.spawn(|| {
            f1.run(&never, &|_| {}, |_| {
                f1_in.fire();
                f1_end.wait("F1's end");
            })
        });
        f1_in.wait("F1's take");
        let two = s.spawn(|| f2.run(&never, &|_| {}, |_| f2_in.fire()));
        f2_in.wait("F2's take");
        two.join().unwrap();
        let last = rec.all().last().copied().unwrap();
        assert_eq!(
            (last.event, last.task, last.class, last.count),
            (Free, f2_id, Fg, 1)
        );
        f1_end.fire();
    });
    assert_eq!(rec.of(f2_id), [(Take, Fg), (Free, Fg)]);
    assert_eq!(rec.of(f1_id), [(Take, Fg), (Free, Fg)]);
    check(&rec.all());
}
