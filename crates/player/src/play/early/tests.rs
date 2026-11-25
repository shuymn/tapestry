//! The early gate's transitions and the admission's answers: the gate on
//! plain outcomes, the admission on a headroom double and self-made
//! pending play skins.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::imports::tests as imports;

fn refusal(why: &str) -> PreparationRefusal {
    PreparationRefusal {
        location: None,
        reason: why.to_owned(),
    }
}

/// A job's start that spawns nothing: its cancel, and whether it ran.
fn started(ran: &AtomicUsize) -> impl FnOnce() -> Result<Cancel, String> + '_ {
    move || {
        ran.fetch_add(1, Ordering::SeqCst);
        Ok(Cancel::new())
    }
}

/// B6f (gate): a reveal on the idle gate, before the chart was read,
/// reveals with nothing begun, and a start after it begins nothing.
#[test]
fn a_reveal_on_the_idle_gate_begins_nothing_after_it() {
    let gate = EarlyGate::<u32>::new(None);
    assert_eq!(gate.commit_reveal(), Committed::Revealed);
    assert_eq!(gate.state_name(), "revealed");
    let ran = AtomicUsize::new(0);
    assert_eq!(gate.start(started(&ran)), Start::Late);
    assert_eq!(ran.load(Ordering::SeqCst), 0, "no job spawned");
    assert!(!gate.end(EarlyOutcome::Specialized(1)));
    assert_eq!(gate.take_specialized(), None);
}

/// B6d: a completion after the commit saw the job running is dropped on
/// the worker: nothing is kept for Preparing, which imports afresh, and the
/// running job was cancelled at the commit.
#[test]
fn a_completion_after_the_commit_is_dropped() {
    let gate = EarlyGate::<u32>::new(None);
    let cancel = Cancel::new();
    let theirs = cancel.clone();
    assert_eq!(gate.start(|| Ok(theirs)), Start::Running);
    assert_eq!(gate.commit_reveal(), Committed::Revealed);
    assert!(cancel.is_cancelled(), "cancelled at the reveal");
    assert_eq!(gate.state_name(), "cancelled at the reveal");
    gate.complete(EarlyOutcome::Specialized(7));
    assert_eq!(gate.state_name(), "cancelled at the reveal");
    assert_eq!(gate.take_specialized(), None);
    // A refusal after it is no early refusal either (B9d).
    gate.complete(EarlyOutcome::Refused(refusal("late")));
    assert_eq!(gate.refused(), None);
}

/// B9c3: the early staging refused before the reveal turns the ended
/// specialization into the refusal, the commit answering it and revealing
/// nothing; published by the render thread for the early request sent,
/// the same through the shared cell. After the reveal, neither changes
/// the gate (the loading state's route).
#[test]
fn a_staging_refusal_before_the_reveal_refuses() {
    let gate = EarlyGate::<u32>::new(None);
    assert!(gate.end(EarlyOutcome::Specialized(1)));
    gate.refuse_staging(refusal("no room"));
    assert_eq!(gate.state_name(), "ended refused");
    assert_eq!(gate.refused(), Some(refusal("no room")));
    assert_eq!(gate.commit_reveal(), Committed::Refused(refusal("no room")));
    assert_eq!(gate.state_name(), "ended refused", "nothing revealed");

    let cell: RenderRefusal = Arc::default();
    let gate = EarlyGate::<u32>::new(Some(Arc::clone(&cell)));
    assert!(gate.end(EarlyOutcome::Specialized(1)));
    gate.step(|s| (*s == 1).then_some(5));
    assert_eq!(gate.request(), Some(5), "the early request sent once");
    // Another request's refusal is not this gate's.
    *cell.lock().unwrap() = Some((4, refusal("other")));
    assert_eq!(gate.refused(), None);
    *cell.lock().unwrap() = Some((5, refusal("restage refused")));
    assert_eq!(
        gate.commit_reveal(),
        Committed::Refused(refusal("restage refused"))
    );

    let gate = EarlyGate::<u32>::new(Some(Arc::clone(&cell)));
    assert!(gate.end(EarlyOutcome::Specialized(1)));
    gate.step(|_| Some(6));
    assert_eq!(gate.commit_reveal(), Committed::Revealed);
    gate.refuse_staging(refusal("after"));
    *cell.lock().unwrap() = Some((6, refusal("after")));
    assert_eq!(gate.refused(), None, "after the reveal: the loading route");
    assert_eq!(gate.state_name(), "revealed specialized");
}

/// B6c (gate): what ended before the commit is revealed with it, and taken
/// by Preparing once, with no step after the completion.
#[test]
fn an_ended_specialization_is_taken_after_the_reveal() {
    let gate = EarlyGate::<u32>::new(None);
    assert_eq!(gate.start(|| Ok(Cancel::new())), Start::Running);
    gate.complete(EarlyOutcome::Specialized(3));
    assert_eq!(gate.commit_reveal(), Committed::Revealed);
    assert_eq!(gate.state_name(), "revealed specialized");
    assert_eq!(gate.take_specialized(), Some(3));
    assert_eq!(gate.take_specialized(), None, "taken once");
}

/// A start whose thread cannot begin ends the gate refused at once.
#[test]
fn a_start_that_cannot_spawn_is_a_refusal() {
    let gate = EarlyGate::<u32>::new(None);
    assert_eq!(gate.start(|| Err("no thread".to_owned())), Start::Ended);
    let r = gate.refused().expect("refused");
    assert!(r.reason.ends_with("no thread"), "{}", r.reason);
}

/// A headroom double: the values each read answers, in order, and the
/// reads made.
#[derive(Default)]
struct Headroom {
    answers: Mutex<VecDeque<Result<u64, String>>>,
    reads: AtomicUsize,
}

impl Headroom {
    fn of(answers: impl IntoIterator<Item = Result<u64, String>>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().collect()),
            reads: AtomicUsize::new(0),
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl Machine for Headroom {
    fn cores(&self) -> usize {
        1
    }

    fn memory(&self) -> Result<u64, String> {
        Err("not read here (test)".to_owned())
    }

    fn headroom(&self) -> Result<u64, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err("no more answers (test)".to_owned()))
    }
}

/// A self-made pending play skin that builds at once, its dir and what a
/// specialization starts from, and an attempt's facts.
fn pending_play() -> (
    tempfile::TempDir,
    Arc<crate::imports::PendingPlay>,
    skin_beatoraja::ImportFacts,
) {
    let r = crate::render_common::offscreen(1_920, 1_080);
    let t = imports::target(&r);
    let dir = imports::lua_skin("local lv = ms.number(96)");
    let (p, _) = imports::pending(imports::play(
        &imports::choice(dir.path(), "play.luaskin"),
        &t,
    ));
    let facts = crate::imports::facts::attempt_facts(
        &crate::imports::facts::tests::loaded("#PLAYLEVEL 3\n", "tapestry"),
        false,
    );
    (dir, p, facts)
}

/// A growth of `total` bytes, all the candidate's.
const fn growth(total: u64) -> Growth {
    Growth {
        candidate: total,
        bank: 0,
        pack: 0,
    }
}

/// The early preparation's job joined, the run not ending.
fn joined(early: &mut EarlyPrep) {
    assert!(!early.join(|| false));
}

/// B13a: a growth over the headroom read at the read defers the start;
/// the bank's decode end reads the headroom afresh and, now fitting,
/// begins it.
#[test]
fn admission_defers_then_admits_at_the_decode_end() {
    let _serial = imports::serial();
    let (_dir, play, facts) = pending_play();
    let machine = Headroom::of([Ok(10), Ok(1_000)]);
    let mut early = EarlyPrep::new(EarlyGate::new(None), 1);
    let at_read = early.admit_at_read(&machine, growth(100), Arc::clone(&play), facts);
    assert_eq!(at_read, Admitted::Deferred);
    assert!(!early.has_job() && early.deferred());
    assert_eq!(early.gate().state_name(), "idle");
    assert_eq!(early.readmit(&machine, growth(100)), Some(Admitted::Yes));
    assert_eq!(machine.reads(), 2, "read fresh at each admission");
    assert!(early.has_job());
    joined(&mut early);
    assert_eq!(early.gate().state_name(), "ended specialized");
    assert_eq!(early.readmit(&machine, growth(100)), None, "asked once");
}

/// B13b: today's begin (the gate declined, no job) when the growth still
/// does not fit at the decode end, when the reveal came first, and at once
/// when the headroom cannot be read.
#[test]
fn admission_declines_to_todays_begin() {
    let _serial = imports::serial();
    let (_dir, play, facts) = pending_play();

    let machine = Headroom::of([Ok(10), Ok(20)]);
    let mut early = EarlyPrep::new(EarlyGate::new(None), 1);
    early.admit_at_read(&machine, growth(100), Arc::clone(&play), facts);
    assert!(matches!(
        early.readmit(&machine, growth(100)),
        Some(Admitted::Declined(_))
    ));
    assert!(!early.has_job());
    assert_eq!(early.gate().state_name(), "ended declined");

    let machine = Headroom::of([Ok(10), Ok(1_000)]);
    let mut early = EarlyPrep::new(EarlyGate::new(None), 1);
    early.admit_at_read(&machine, growth(100), Arc::clone(&play), facts);
    assert_eq!(early.gate().commit_reveal(), Committed::Revealed);
    assert!(matches!(
        early.readmit(&machine, growth(100)),
        Some(Admitted::Declined(_))
    ));
    assert!(!early.has_job(), "past the reveal: today's begin");

    let machine = Headroom::of([Err("unreadable".to_owned())]);
    let mut early = EarlyPrep::new(EarlyGate::new(None), 1);
    let at_read = early.admit_at_read(&machine, growth(0), play, facts);
    assert!(matches!(at_read, Admitted::Declined(_)));
    assert!(!early.has_job() && !early.deferred());
    assert_eq!(early.gate().state_name(), "ended declined");
}

/// The growth's parts: the candidate's bounds (none when reused), the
/// bank's whole bank and a file per decode thread while it decodes, and a
/// pack's bound while one runs or one could begin again while pages are
/// in disposal (B13c's charge).
#[test]
fn the_growth_charges_what_is_still_to_come() {
    assert_eq!(
        Growth::candidate(false),
        153_286_620 + 161 * 1024 * 1024 + 256 * 1024 * 1024 * 4
    );
    assert_eq!(Growth::candidate(true), 0);
    let limits = audio::BankLimits::default();
    assert_eq!(
        Growth::bank(true, &limits),
        512 * 1024 * 1024 + 4 * 128 * 1024 * 1024
    );
    assert_eq!(Growth::bank(false, &limits), 0);
    let charge = |running, disposing, may_begin| crate::result_skin::PackCharge {
        running,
        disposing,
        may_begin,
    };
    assert_eq!(Growth::pack(charge(true, false, false)), PACK_BOUND);
    assert_eq!(Growth::pack(charge(false, true, true)), PACK_BOUND);
    assert_eq!(Growth::pack(charge(false, true, false)), 0);
    assert_eq!(Growth::pack(charge(false, false, true)), 0);
}
