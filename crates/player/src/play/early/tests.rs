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
fn started(ran: &AtomicUsize) -> impl FnOnce(&Cancel) -> Result<(), String> + '_ {
    move |_| {
        ran.fetch_add(1, Ordering::SeqCst);
        Ok(())
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
    let mut cancel = Cancel::new();
    assert_eq!(
        gate.start(|c| {
            cancel = c.clone();
            Ok(())
        }),
        Start::Running
    );
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

/// B9c3: the early staging refused before the reveal, published by the
/// render thread for the early request sent, turns the ended
/// specialization into the refusal, the commit answering it and revealing
/// nothing; another request's refusal is not this gate's. After the reveal
/// it changes nothing (the loading state's route).
#[test]
fn a_staging_refusal_before_the_reveal_refuses() {
    let cell: RenderRefusal = Arc::default();
    let gate = EarlyGate::<u32>::new(Some(Arc::clone(&cell)));
    assert!(gate.end(EarlyOutcome::Specialized(1)));
    gate.step(|s| (*s == 1).then_some(5));
    assert_eq!(gate.request(), Some(5), "the early request sent once");
    gate.step(|_| Some(9));
    assert_eq!(gate.request(), Some(5));
    *cell.lock().unwrap() = Some((4, refusal("other")));
    assert_eq!(gate.refused(), None);
    *cell.lock().unwrap() = Some((5, refusal("no room")));
    assert_eq!(gate.refused(), Some(refusal("no room")));
    assert_eq!(gate.state_name(), "ended refused");
    assert_eq!(gate.commit_reveal(), Committed::Refused(refusal("no room")));
    assert_eq!(gate.state_name(), "ended refused", "nothing revealed");

    // Published after the last step: the commit reads it first.
    let gate = EarlyGate::<u32>::new(Some(Arc::clone(&cell)));
    assert!(gate.end(EarlyOutcome::Specialized(1)));
    gate.step(|_| Some(6));
    *cell.lock().unwrap() = Some((6, refusal("restage refused")));
    assert_eq!(
        gate.commit_reveal(),
        Committed::Refused(refusal("restage refused"))
    );

    let gate = EarlyGate::<u32>::new(Some(Arc::clone(&cell)));
    assert!(gate.end(EarlyOutcome::Specialized(1)));
    gate.step(|_| Some(7));
    assert_eq!(gate.commit_reveal(), Committed::Revealed);
    *cell.lock().unwrap() = Some((7, refusal("after")));
    assert_eq!(gate.refused(), None, "after the reveal: the loading route");
    assert_eq!(gate.state_name(), "revealed specialized");
}

/// B6c (gate): what ended before the commit is revealed with it, and taken
/// by Preparing once, with no step after the completion.
#[test]
fn an_ended_specialization_is_taken_after_the_reveal() {
    let gate = EarlyGate::<u32>::new(None);
    assert_eq!(gate.start(|_| Ok(())), Start::Running);
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
    assert_eq!(gate.start(|_| Err("no thread".to_owned())), Start::Ended);
    let r = gate.refused().expect("refused");
    assert!(r.reason.ends_with("no thread"), "{}", r.reason);
}

/// A reveal word on one thread's terms: armed until [`reveal`](Self::reveal)
/// or a refusal; a refusal after the reveal loses. `reveal_at_refuse`
/// reveals just before the next refusal (a reveal between a gate's read of
/// the word and its refusal). The render side's word is the same, on an
/// atomic.
#[derive(Default)]
struct Word {
    revealed: std::sync::atomic::AtomicBool,
    refused: std::sync::atomic::AtomicBool,
    reveal_at_refuse: std::sync::atomic::AtomicBool,
    refusals: AtomicUsize,
    /// The lifetime's cancel, made with the word.
    cancel: Cancel,
}

impl Word {
    /// The render thread's reveal: won unless refused, the import's
    /// cancel cancelled.
    fn reveal(&self) -> bool {
        if self.refused.load(Ordering::SeqCst) {
            return false;
        }
        self.revealed.store(true, Ordering::SeqCst);
        self.cancel.cancel();
        true
    }
}

impl RevealWord for Word {
    fn refuse(&self) -> bool {
        self.refusals.fetch_add(1, Ordering::SeqCst);
        if self.reveal_at_refuse.swap(false, Ordering::SeqCst) {
            self.reveal();
        }
        if self.revealed.load(Ordering::SeqCst) {
            return false;
        }
        self.refused.store(true, Ordering::SeqCst);
        true
    }

    fn revealed(&self) -> bool {
        self.revealed.load(Ordering::SeqCst)
    }

    fn cancel(&self) -> Cancel {
        self.cancel.clone()
    }
}

/// A gate following a fresh word.
fn ordered() -> (Arc<EarlyGate<u32>>, Arc<Word>) {
    let gate = EarlyGate::<u32>::new(None);
    let word = Arc::new(Word::default());
    gate.set_order(Arc::clone(&word) as RevealOrder);
    (gate, word)
}

/// The decision's end: a gate ending refused withdraws the armed reveal
/// (a job's refusal, a start that cannot spawn), so the render thread
/// reveals nothing past it; a specialization and a completion after the
/// commit withdraw nothing.
#[test]
fn a_refusal_withdraws_the_armed_reveal() {
    let (gate, word) = ordered();
    assert_eq!(gate.start(|_| Ok(())), Start::Running);
    assert!(gate.complete(EarlyOutcome::Refused(refusal("refused"))));
    assert_eq!(word.refusals.load(Ordering::SeqCst), 1, "a job's refusal");
    assert!(!word.reveal(), "the render thread reveals nothing");
    assert_eq!(gate.commit_reveal(), Committed::Refused(refusal("refused")));
    let (gate, word) = ordered();
    assert_eq!(gate.start(|_| Err("no thread".to_owned())), Start::Ended);
    assert_eq!(
        word.refusals.load(Ordering::SeqCst),
        1,
        "a start that cannot spawn"
    );
    let (gate, word) = ordered();
    assert_eq!(gate.start(|_| Ok(())), Start::Running);
    gate.complete(EarlyOutcome::Specialized(3));
    assert_eq!(word.refusals.load(Ordering::SeqCst), 0, "a specialization");
    let (gate, word) = ordered();
    assert_eq!(gate.start(|_| Ok(())), Start::Running);
    assert_eq!(gate.commit_reveal(), Committed::Revealed);
    gate.complete(EarlyOutcome::Refused(refusal("late")));
    assert_eq!(
        word.refusals.load(Ordering::SeqCst),
        0,
        "a completion after the commit"
    );
}

/// The render thread's reveal freezes the gate (design.md "Screen
/// Transitions and Loading", reveal and adoption): an import running at it
/// is cancelled there, and its completion afterwards, before the game
/// thread's commit, is dropped, not adopted; the commit then answers the
/// reveal and Preparing imports afresh.
#[test]
fn an_import_ending_after_the_render_threads_reveal_is_dropped() {
    let (gate, word) = ordered();
    let mut cancel = Cancel::new();
    assert_eq!(
        gate.start(|c| {
            cancel = c.clone();
            Ok(())
        }),
        Start::Running
    );
    assert!(word.reveal());
    assert!(
        cancel.is_cancelled(),
        "cancelled at the reveal, not at the commit"
    );
    assert!(!gate.complete(EarlyOutcome::Specialized(7)), "dropped");
    assert_eq!(gate.state_name(), "cancelled at the reveal");
    assert_eq!(gate.commit_reveal(), Committed::Revealed);
    assert_eq!(gate.take_specialized(), None);
    let mut sent = false;
    gate.step(|_| {
        sent = true;
        Some(1)
    });
    assert!(!sent, "no early request after the reveal");
}

/// The render thread's reveal: what ended before it stays adoptable, the
/// game thread's commit consuming that state, not deciding again.
#[test]
fn an_import_ended_before_the_render_threads_reveal_is_adopted() {
    let (gate, word) = ordered();
    assert_eq!(gate.start(|_| Ok(())), Start::Running);
    assert!(gate.complete(EarlyOutcome::Specialized(3)));
    assert!(word.reveal());
    assert_eq!(gate.state_name(), "revealed specialized");
    assert_eq!(gate.commit_reveal(), Committed::Revealed);
    assert_eq!(gate.take_specialized(), Some(3));
}

/// The render thread's reveal: a refusal published after it is dropped,
/// whether the reveal came before the gate read the word or between that
/// read and the refusal's withdrawal; the commit never refuses a reveal
/// already on screen (the loading state's route takes Preparing's own).
#[test]
fn a_refusal_after_the_render_threads_reveal_is_dropped() {
    let (gate, word) = ordered();
    assert_eq!(gate.start(|_| Ok(())), Start::Running);
    assert!(word.reveal());
    assert!(!gate.complete(EarlyOutcome::Refused(refusal("late"))));
    assert_eq!(gate.refused(), None);
    assert_eq!(gate.commit_reveal(), Committed::Revealed);

    let (gate, word) = ordered();
    let mut cancel = Cancel::new();
    assert_eq!(
        gate.start(|c| {
            cancel = c.clone();
            Ok(())
        }),
        Start::Running
    );
    word.reveal_at_refuse.store(true, Ordering::SeqCst);
    assert!(!gate.complete(EarlyOutcome::Refused(refusal("lost"))));
    assert!(cancel.is_cancelled());
    assert_eq!(gate.state_name(), "cancelled at the reveal");
    assert_eq!(gate.commit_reveal(), Committed::Revealed);
}

/// The render thread's reveal: no early start begins after it.
#[test]
fn nothing_begins_after_the_render_threads_reveal() {
    let (gate, word) = ordered();
    assert!(word.reveal());
    let ran = AtomicUsize::new(0);
    assert_eq!(gate.start(started(&ran)), Start::Late);
    assert_eq!(ran.load(Ordering::SeqCst), 0, "no job spawned");
    assert_eq!(gate.state_name(), "revealed");
}

/// The render thread's reveal: a job spawned as it comes, between the
/// gate's read of the word and the job's state, is cancelled by it.
#[cfg(feature = "dev-tools")]
#[test]
fn a_job_spawned_as_the_render_thread_reveals_is_cancelled() {
    let (gate, word) = ordered();
    let mut cancel = Cancel::new();
    let revealing = Arc::clone(&word);
    gate.after_spawn(move || {
        revealing.reveal();
    });
    assert_eq!(
        gate.start(|c| {
            cancel = c.clone();
            Ok(())
        }),
        Start::Running
    );
    assert!(cancel.is_cancelled());
    assert_eq!(gate.state_name(), "cancelled at the reveal");
    assert!(!gate.complete(EarlyOutcome::Specialized(1)));
}

/// The decision's end: the job's cancel is given to the word before its
/// thread is spawned, so a reveal while the spawning thread is still
/// inside the start (its thread made, its state not yet set) cancels the
/// job before the worker's first check.
#[test]
fn a_reveal_while_the_spawner_is_blocked_cancels_the_job() {
    let (gate, word) = ordered();
    let revealing = Arc::clone(&word);
    assert_eq!(
        gate.start(|c| {
            let worker = c.clone();
            // The worker, spawned; its spawner blocked here.
            let first_check = std::thread::spawn(move || {
                revealing.reveal();
                worker.is_cancelled()
            });
            assert!(first_check.join().unwrap(), "the worker ran on");
            Ok(())
        }),
        Start::Running
    );
    assert_eq!(gate.state_name(), "cancelled at the reveal");
}

/// The reveal word: a reveal after the gate read the word idle and before
/// it spawns the job leaves nothing to register: the job is given the
/// lifetime's cancel, which the reveal cancelled, so it starts cancelled,
/// and the gate is cancelled at the reveal.
#[cfg(feature = "dev-tools")]
#[test]
fn a_reveal_before_the_spawn_starts_the_job_cancelled() {
    let (gate, word) = ordered();
    let revealing = Arc::clone(&word);
    gate.before_spawn(move || {
        assert!(revealing.reveal());
    });
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    assert_eq!(
        gate.start(|c| {
            cancelled.store(c.is_cancelled(), Ordering::SeqCst);
            Ok(())
        }),
        Start::Running
    );
    assert!(
        cancelled.load(Ordering::SeqCst),
        "the job began uncancelled"
    );
    assert_eq!(gate.state_name(), "cancelled at the reveal");
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
