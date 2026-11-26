//! The pending play skin's preparation during the decision screen (design.md
//! "Screen Transitions and Loading", the pending play skin's preparation;
//! Threat Model T09): once a decided play's chart is read, its
//! specialization runs on a `skin-import` thread of its own while the
//! decision screen shows, and Preparing adopts what it built.
//!
//! - **The gate** ([`EarlyGate`], one mutex) is made at the play's decision
//!   entry, before either deadline owner (App's look, the load wait's look)
//!   can reveal, and every transition happens under its lock: the start,
//!   the worker's completion at its catch boundary
//!   ([`imports::spawn_job_completing`]), a staging refused before the
//!   reveal, and the reveal's commit ([`EarlyGate::commit_reveal`]), which
//!   cancels an import still running (Preparing imports afresh) and answers
//!   a refusal published before it instead of revealing.
//! - **The reveal word** ([`RevealWord`]): the render thread reveals the
//!   held loading lifetime at the deadline by itself, so the gate follows
//!   the word that orders that reveal against every refusal. Once it says
//!   revealed, every transition first takes the reveal in as the commit
//!   would have (`EarlyGate::sync`): what ended before stays adoptable,
//!   a running import is cancelled and whatever it ends with later is
//!   dropped, and nothing begins after it.
//! - **Admission** ([`admit`]): until the host memory ledger exists, the
//!   peak growth still to come (the candidate, the bank's remaining decode,
//!   a result skin's pack) against the host's headroom read fresh
//!   ([`crate::priority::Machine::headroom`]); deferred once to the bank's
//!   decode end, then declined (today's begin).
//! - **The owner** ([`EarlyPrep`]): its job, cancelled and joined at the
//!   flow's settle sites, never in the deadline's step.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use game::PreparationRefusal;
use skin_beatoraja::Cancel;

use crate::imports::{self, Job, Ran, Specialization};
use crate::priority::Machine;

/// A refusal of the early staging the render thread publishes for the game
/// thread: the early request it refused and why (`Shared::early_refusal`).
pub type RenderRefusal = Arc<Mutex<Option<(u64, PreparationRefusal)>>>;

/// The reveal word of the decision's held loading lifetime, as the gate
/// orders itself against the render thread's reveal at the deadline
/// ([`crate::render_side::RenderLink::reveal_order`]; design.md "Screen
/// Transitions and Loading", reveal and adoption).
pub trait RevealWord: Send + Sync {
    /// Withdraws the armed reveal (a refusal): false only when the reveal
    /// ran first, so the refusal lost.
    fn refuse(&self) -> bool;
    /// Whether the lifetime was revealed through the word.
    fn revealed(&self) -> bool;
    /// `cancel` is cancelled when the render thread reveals; one that
    /// revealed already is read with [`revealed`](Self::revealed) after.
    fn cancel_at_reveal(&self, cancel: &Cancel);
}

/// The gate's reveal word, shared with the render link.
pub type RevealOrder = Arc<dyn RevealWord>;

/// How the early specialization ended, as the gate keeps it.
#[derive(Debug)]
pub enum EarlyOutcome<S> {
    /// Built: the specialization Preparing adopts.
    Specialized(S),
    /// The skin cannot be built for the chart, or its early staging was
    /// refused: the choice returns with the reason.
    Refused(PreparationRefusal),
    /// Not admitted: Preparing begins as it does without the decision
    /// screen.
    Declined,
}

/// The gate's state (design.md "Screen Transitions and Loading", reveal and
/// adoption).
#[derive(Debug)]
enum State<S> {
    /// Made at the decision entry; nothing begun.
    Idle,
    /// The specialization's job runs; its cancel.
    Running(Cancel),
    /// The job ended, or the start ended at once.
    Ended(EarlyOutcome<S>),
    /// The reveal was committed while the job ran: the job is cancelled,
    /// and its outcome, if it comes, is dropped on the worker.
    CancelledAtReveal,
    /// The reveal was committed on what had ended (none: nothing was
    /// begun).
    Revealed(Option<EarlyOutcome<S>>),
}

/// What [`EarlyGate::commit_reveal`] decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Committed {
    /// The loading state is revealed: the deadline's step goes on.
    Revealed,
    /// A refusal came before the reveal: nothing is revealed, and the
    /// choice returns with it.
    Refused(PreparationRefusal),
}

#[derive(Debug)]
struct Inner<S> {
    state: State<S>,
    /// The early request sent for the staging, once sent.
    request: Option<u64>,
    /// When it ended specialized (development builds: the early request's
    /// delayed send).
    #[cfg(feature = "dev-tools")]
    ended_at: Option<Instant>,
}

/// The early preparation's gate: one lock over its state, shared by the
/// game thread's owner, both deadline owners and the job's worker. `S` is
/// the specialization (generic for the gate's own tests).
pub struct EarlyGate<S = Specialization> {
    inner: Mutex<Inner<S>>,
    /// The render thread's refusal of the early staging, read under the
    /// lock before each decision that depends on it.
    render: Option<RenderRefusal>,
    /// The reveal word the gate follows ([`RevealWord`]): a refusal
    /// withdraws the render thread's armed reveal through it, under the
    /// lock, and a reveal through it freezes what the gate keeps.
    order: std::sync::OnceLock<RevealOrder>,
    /// Called at the commit's start, before its lock (tests: a refusal
    /// published between a look's classification and its commit).
    #[cfg(test)]
    before_commit: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Called under the start's lock once the job's thread is spawned and
    /// before its state is set (tests: the worker held at the lock).
    #[cfg(test)]
    after_spawn: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The completions begun, counted before their lock (tests: a worker
    /// waiting at the lock).
    #[cfg(test)]
    completing: std::sync::atomic::AtomicUsize,
}

impl<S> std::fmt::Debug for EarlyGate<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EarlyGate").finish_non_exhaustive()
    }
}

/// How [`EarlyGate::start`] went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// The job runs.
    Running,
    /// It ended at once (its thread could not start: a refusal).
    Ended,
    /// The gate was not idle (the reveal came first): nothing begun.
    Late,
}

impl<S> EarlyGate<S> {
    /// An idle gate reading the render thread's early refusal from
    /// `render`.
    pub fn new(render: Option<RenderRefusal>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                state: State::Idle,
                request: None,
                #[cfg(feature = "dev-tools")]
                ended_at: None,
            }),
            render,
            order: std::sync::OnceLock::new(),
            #[cfg(test)]
            before_commit: Mutex::new(None),
            #[cfg(test)]
            after_spawn: Mutex::new(None),
            #[cfg(test)]
            completing: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner<S>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The render thread's refusal of this gate's early request, taken
    /// into the state: before the reveal an ended specialization becomes
    /// the refusal; after it, it is the loading state's route, not the
    /// gate's.
    fn absorb(&self, inner: &mut Inner<S>) {
        let Some(cell) = &self.render else {
            return;
        };
        let mut published = cell.lock().unwrap_or_else(PoisonError::into_inner);
        let ours = matches!(*published, Some((r, _)) if Some(r) == inner.request);
        if !ours {
            return;
        }
        if let State::Ended(EarlyOutcome::Specialized(_)) = inner.state
            && let Some((_, refusal)) = published.take()
        {
            log::info!("early: the staging was refused before the reveal: {refusal}");
            inner.state = State::Ended(EarlyOutcome::Refused(refusal));
        }
    }

    /// The gate follows reveal word `order` (once; a second is ignored).
    pub fn set_order(&self, order: RevealOrder) {
        let _ = self.order.set(order);
    }

    /// A refusal withdraws the armed reveal: false when the reveal ran
    /// first (the refusal lost). True with no word set.
    fn refuse(&self) -> bool {
        self.order.get().is_none_or(|o| o.refuse())
    }

    /// The reveal through the word taken into the state, as the commit
    /// takes it (design.md "Screen Transitions and Loading", reveal and
    /// adoption): eligibility frozen at that instant. A running job is
    /// cancelled and whatever it ends with later is dropped
    /// ([`State::CancelledAtReveal`]); what ended before is revealed with
    /// it; an idle gate reveals with nothing begun. A refusal kept here
    /// won the word, so it is never revealed. Every transition runs it
    /// first, under the lock.
    fn sync(&self, inner: &mut Inner<S>) {
        if !self.order.get().is_some_and(|o| o.revealed()) {
            return;
        }
        inner.state = match std::mem::replace(&mut inner.state, State::Idle) {
            State::Running(cancel) => {
                cancel.cancel();
                log::info!("early: the import was still running at the reveal: cancelled");
                State::CancelledAtReveal
            }
            State::Idle => State::Revealed(None),
            refused @ State::Ended(EarlyOutcome::Refused(_)) => refused,
            State::Ended(o) => State::Revealed(Some(o)),
            done @ (State::CancelledAtReveal | State::Revealed(_)) => done,
        };
    }

    /// Begins the job from [`Idle`](State::Idle) with `spawn`, under the
    /// lock: the job's thread spawned and its cancel set as the state
    /// before the lock is released, so the worker's completion, which
    /// takes the same lock, always finds the job running (or cancelled at
    /// the reveal). A thread that cannot start is a refusal at once.
    ///
    /// Nothing begins once the reveal word says revealed, and a job
    /// spawned as the reveal comes is cancelled by it: its cancel is given
    /// to the word before the word is read again.
    pub fn start(&self, spawn: impl FnOnce() -> Result<Cancel, String>) -> Start {
        let mut inner = self.lock();
        self.sync(&mut inner);
        if !matches!(inner.state, State::Idle) {
            return Start::Late;
        }
        match spawn() {
            Ok(cancel) => {
                #[cfg(test)]
                if let Some(hook) = self
                    .after_spawn
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
                {
                    hook();
                }
                if let Some(o) = self.order.get() {
                    o.cancel_at_reveal(&cancel);
                }
                inner.state = State::Running(cancel);
                self.sync(&mut inner);
                Start::Running
            }
            Err(why) => {
                if !self.refuse() {
                    // The reveal came first: Preparing begins as today.
                    inner.state = State::Revealed(None);
                    return Start::Late;
                }
                inner.state = State::Ended(EarlyOutcome::Refused(refusal(&why)));
                Start::Ended
            }
        }
    }

    /// Ends the gate at once from [`Idle`](State::Idle) with `outcome` (a
    /// kept specialization that holds, a declined admission). Whether it
    /// was idle.
    pub fn end(&self, outcome: EarlyOutcome<S>) -> bool {
        let mut inner = self.lock();
        self.sync(&mut inner);
        if !matches!(inner.state, State::Idle) {
            return false;
        }
        #[cfg(feature = "dev-tools")]
        {
            inner.ended_at = Some(Instant::now());
        }
        inner.state = State::Ended(outcome);
        true
    }

    /// How long ago the gate ended (development builds: the early
    /// request's delayed send).
    #[cfg(feature = "dev-tools")]
    pub fn ended_for(&self) -> Duration {
        self.lock()
            .ended_at
            .map_or(Duration::ZERO, |at| at.elapsed())
    }

    /// The worker's completion with `outcome`: kept only while the job
    /// runs; cancelled at the reveal (or anything else), dropped here. A
    /// refusal withdraws the armed reveal; one the reveal beat is dropped
    /// as a completion after the reveal is. Whether it was kept.
    pub fn complete(&self, outcome: EarlyOutcome<S>) -> bool {
        #[cfg(test)]
        self.completing
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut inner = self.lock();
        self.sync(&mut inner);
        match inner.state {
            State::Running(_) => {
                if matches!(outcome, EarlyOutcome::Refused(_)) && !self.refuse() {
                    // Revealed since the sync: the reveal's freeze.
                    self.sync(&mut inner);
                    drop(inner);
                    drop(outcome);
                    return false;
                }
                #[cfg(feature = "dev-tools")]
                {
                    inner.ended_at = Some(Instant::now());
                }
                inner.state = State::Ended(outcome);
                true
            }
            _ => {
                drop(inner);
                drop(outcome);
                false
            }
        }
    }

    /// The early request sent, if any.
    pub fn request(&self) -> Option<u64> {
        self.lock().request
    }

    /// The serving step (non-blocking): the render thread's refusal read
    /// in, then, before the reveal, an ended specialization's early
    /// request sent once with `send` (its number; none: not sent), under
    /// the lock, so no reveal comes between the state read and the send.
    pub fn step(&self, send: impl FnOnce(&S) -> Option<u64>) {
        let mut inner = self.lock();
        self.sync(&mut inner);
        self.absorb(&mut inner);
        if inner.request.is_some() {
            return;
        }
        if let State::Ended(EarlyOutcome::Specialized(s)) = &inner.state {
            inner.request = send(s);
        }
    }

    /// The deadline's reveal, decided against one state under the lock,
    /// the render thread's refusal read first: a refusal ended before it
    /// answers [`Committed::Refused`] and nothing changes; a running job is
    /// cancelled ([`State::CancelledAtReveal`]); whatever ended is revealed
    /// with it; an idle gate reveals with nothing begun. After the render
    /// thread's reveal it decides nothing again: the state the reveal froze
    /// (`sync`) answers [`Committed::Revealed`].
    pub fn commit_reveal(&self) -> Committed {
        #[cfg(test)]
        if let Some(hook) = self
            .before_commit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            hook();
        }
        let mut inner = self.lock();
        self.sync(&mut inner);
        self.absorb(&mut inner);
        let state = std::mem::replace(&mut inner.state, State::Idle);
        let (next, committed) = match state {
            State::Ended(EarlyOutcome::Refused(r)) => {
                log::info!("early: refused before the reveal; nothing revealed: {r}");
                let answer = Committed::Refused(r.clone());
                (State::Ended(EarlyOutcome::Refused(r)), answer)
            }
            State::Running(cancel) => {
                cancel.cancel();
                log::info!("early: the import was still running at the reveal: cancelled");
                (State::CancelledAtReveal, Committed::Revealed)
            }
            State::Ended(o) => (State::Revealed(Some(o)), Committed::Revealed),
            State::Idle => (State::Revealed(None), Committed::Revealed),
            done @ (State::CancelledAtReveal | State::Revealed(_)) => (done, Committed::Revealed),
        };
        inner.state = next;
        committed
    }

    /// The refusal the flow returns with, while one ended before the
    /// reveal (the render thread's included); none once revealed.
    pub fn refused(&self) -> Option<PreparationRefusal> {
        let mut inner = self.lock();
        self.sync(&mut inner);
        self.absorb(&mut inner);
        match &inner.state {
            State::Ended(EarlyOutcome::Refused(r)) => Some(r.clone()),
            _ => None,
        }
    }

    /// The render thread's refusal of the early request sent, published
    /// after the reveal: the attempt adopting the request would end with
    /// it, so the load's looks and the preparation's polls end with it at
    /// once. None before the reveal ([`refused`](Self::refused) reads that
    /// one), and none after a job cancelled at the reveal, which sent no
    /// request.
    pub fn late_refused(&self) -> Option<PreparationRefusal> {
        let mut inner = self.lock();
        self.sync(&mut inner);
        if !matches!(inner.state, State::Revealed(_)) {
            return None;
        }
        let request = inner.request?;
        let cell = self.render.as_ref()?;
        let published = cell.lock().unwrap_or_else(PoisonError::into_inner);
        published
            .as_ref()
            .filter(|(r, _)| *r == request)
            .map(|(_, refusal)| refusal.clone())
    }

    /// The specialization the gate ended with, taken for Preparing, before
    /// or after the reveal, whether or not anything looked at the gate
    /// after the worker published it.
    pub fn take_specialized(&self) -> Option<S> {
        let mut inner = self.lock();
        self.sync(&mut inner);
        match std::mem::replace(&mut inner.state, State::Idle) {
            State::Ended(EarlyOutcome::Specialized(s)) => {
                inner.state = State::Ended(EarlyOutcome::Declined);
                Some(s)
            }
            State::Revealed(Some(EarlyOutcome::Specialized(s))) => {
                inner.state = State::Revealed(None);
                Some(s)
            }
            other => {
                inner.state = other;
                None
            }
        }
    }

    /// Whether the reveal was committed.
    pub fn revealed(&self) -> bool {
        let mut inner = self.lock();
        self.sync(&mut inner);
        matches!(inner.state, State::Revealed(_) | State::CancelledAtReveal)
    }

    /// The state's name (logs and tests).
    pub fn state_name(&self) -> &'static str {
        let mut inner = self.lock();
        self.sync(&mut inner);
        match &inner.state {
            State::Idle => "idle",
            State::Running(_) => "running",
            State::Ended(EarlyOutcome::Specialized(_)) => "ended specialized",
            State::Ended(EarlyOutcome::Refused(_)) => "ended refused",
            State::Ended(EarlyOutcome::Declined) => "ended declined",
            State::CancelledAtReveal => "cancelled at the reveal",
            State::Revealed(None) => "revealed",
            State::Revealed(Some(EarlyOutcome::Specialized(_))) => "revealed specialized",
            State::Revealed(Some(EarlyOutcome::Refused(_))) => "revealed refused",
            State::Revealed(Some(EarlyOutcome::Declined)) => "revealed declined",
        }
    }

    /// `hook` runs at the next commit's start, before its lock (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn before_commit(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .before_commit
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// The completions begun, counted before their lock (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn completing(&self) -> usize {
        self.completing.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// `hook` runs at the next start, under its lock, once the job's
    /// thread is spawned (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn after_spawn(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .after_spawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }
}

/// The refusal of a specialization that could not be built for the chart,
/// as Preparing's own says it.
fn refusal(why: &str) -> PreparationRefusal {
    PreparationRefusal {
        location: None,
        reason: format!("the play skin cannot be built for this chart: {why}"),
    }
}

/// A job's outcome as the gate keeps it: a refusal, a panic
/// ([`imports::PANICKED`]) and a silent end all refuse.
fn outcome_of(
    sent: &Result<Result<Specialization, String>, &'static str>,
) -> EarlyOutcome<Specialization> {
    match sent {
        Ok(Ok(s)) => EarlyOutcome::Specialized(s.clone()),
        Ok(Err(why)) => EarlyOutcome::Refused(refusal(why)),
        Err(why) => EarlyOutcome::Refused(refusal(why)),
    }
}

/// The import's measured ceiling, the most a play skin's import allocates
/// (design.md "Memory Budget", the import's measured ceiling).
pub const IMPORT_CEILING: u64 = 153_286_620;
/// One image's whole-decode bound (design.md "Memory Budget").
pub const IMAGE_DECODE_BOUND: u64 = 161 * 1024 * 1024;
/// The candidate's pages at the atlas cap: 256 Mi texels of 4 bytes.
pub const ATLAS_PAGES_BOUND: u64 = 256 * 1024 * 1024 * 4;
/// A result skin's preload's pack, its surveyed bound (design.md "beatoraja
/// Lua Skins", the result skin's preload).
pub const PACK_BOUND: u64 = 1_160_000_000;

/// The peak growth still to come, by part (design.md "Screen Transitions
/// and Loading", admission): what is resident is in the headroom and never
/// charged again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Growth {
    /// The candidate's: the import's ceiling, one image's decode and its
    /// pages at the atlas cap; none for a kept specialization reused.
    pub candidate: u64,
    /// The bank's remaining decode: its bytes not yet allocated and the raw
    /// files in flight; none once the decode has ended.
    pub bank: u64,
    /// A result skin's preload's: a pack running uncollected, or one that
    /// could begin again while a stopped pack's pages are still disposed of
    /// (the overlap), at its bound.
    pub pack: u64,
}

impl Growth {
    /// The candidate's growth: none when `reused`.
    pub const fn candidate(reused: bool) -> u64 {
        if reused {
            0
        } else {
            IMPORT_CEILING + IMAGE_DECODE_BOUND + ATLAS_PAGES_BOUND
        }
    }

    /// The bank's remaining growth under `limits` while `decoding`: its
    /// whole bank and a file in flight on each decode thread.
    pub fn bank(decoding: bool, limits: &audio::BankLimits) -> u64 {
        if !decoding {
            return 0;
        }
        let threads = u64::try_from(limits.max_decode_threads).unwrap_or(u64::MAX);
        limits
            .max_bank_bytes
            .saturating_add(threads.saturating_mul(limits.max_file_bytes))
    }

    /// A preload's growth from its `charge`.
    pub const fn pack(charge: crate::result_skin::PackCharge) -> u64 {
        if charge.running || (charge.disposing && charge.may_begin) {
            PACK_BOUND
        } else {
            0
        }
    }

    /// All of it.
    pub const fn total(&self) -> u64 {
        self.candidate
            .saturating_add(self.bank)
            .saturating_add(self.pack)
    }
}

/// An admission's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admitted {
    /// It fits: the early start begins.
    Yes,
    /// It does not fit now: asked again once, at the bank's decode end.
    Deferred,
    /// Not admitted (still not fitting at the second asking, past the
    /// reveal, or the headroom unreadable): today's begin, with why.
    Declined(String),
}

/// The early start's admission of `growth` against `headroom`, read fresh
/// (design.md "Screen Transitions and Loading", admission): fitting, it
/// begins; else the first asking defers it, a second (`again`) or one past
/// the reveal (`revealed`) declines it, and an unreadable headroom declines
/// it at once. Each answer is logged.
pub fn admit(
    growth: Growth,
    headroom: Result<u64, String>,
    again: bool,
    revealed: bool,
) -> Admitted {
    let answer = match headroom {
        Err(why) => Admitted::Declined(format!("the host's headroom cannot be read: {why}")),
        Ok(_) if revealed => Admitted::Declined("the reveal came first".to_owned()),
        Ok(room) if growth.total() <= room => Admitted::Yes,
        Ok(room) if again => Admitted::Declined(format!(
            "growth {} B over the headroom {room} B at the decode's end",
            growth.total()
        )),
        Ok(_) => Admitted::Deferred,
    };
    match &answer {
        Admitted::Yes => log::info!("early: admitted: growth {:?}", growth),
        Admitted::Deferred => {
            log::info!("early: deferred to the decode's end: growth {:?}", growth)
        }
        Admitted::Declined(why) => log::info!("early: declined, today's begin: {why}"),
    }
    answer
}

/// The early request's sender and its ticket, shared by the looks that
/// serve the gate (design.md "Screen Transitions and Loading", the
/// request): sent once, before the reveal, when the gate ended
/// specialized; ended by its owner whatever the ticket's state.
#[derive(Debug)]
pub struct EarlyLink {
    sender: crate::render_side::EarlySender,
    ticket: Mutex<Option<crate::render_side::PendingAttempt>>,
}

impl EarlyLink {
    /// A link sending through `sender`.
    pub fn new(sender: crate::render_side::EarlySender) -> Arc<Self> {
        Arc::new(Self {
            sender,
            ticket: Mutex::new(None),
        })
    }

    /// Serves `gate` (non-blocking): the render thread's refusal read in,
    /// then the early request sent once for an ended specialization before
    /// the reveal.
    pub fn serve(&self, gate: &EarlyGate) {
        // Read before the step, whose closure runs under the gate's lock.
        #[cfg(feature = "dev-tools")]
        let due = crate::dev::early_send_due(gate.ended_for());
        gate.step(|s| {
            #[cfg(feature = "dev-tools")]
            if !due {
                return None;
            }
            let ticket = self.sender.begin_early(Arc::clone(&s.bundle));
            let request = ticket.request();
            log::info!("early: the staging was asked as request {request}");
            *self.ticket.lock().unwrap_or_else(PoisonError::into_inner) = Some(ticket);
            Some(request)
        });
    }

    /// The early request ended: dropped on the render thread, answered or
    /// not, its ticket with it.
    pub fn end(&self, request: u64) {
        let ticket = self
            .ticket
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.sender.discard(request);
        drop(ticket);
    }

    /// The early request's ticket, taken for the attempt that adopts it.
    pub fn take_ticket(&self) -> Option<crate::render_side::PendingAttempt> {
        self.ticket
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// The early request handed to the attempt's preparation: its number,
/// which the attempt's request adopts, and its ticket, dropped once the
/// attempt's request is sent.
#[derive(Debug)]
pub struct Handoff {
    pub request: u64,
    /// Held until the attempt's request is sent: dropped open, it discards
    /// the early request, which must come after the adoption.
    #[expect(dead_code, reason = "held for its drop's order, never read")]
    pub ticket: Option<crate::render_side::PendingAttempt>,
}

/// The early preparation of one decided play: the gate, the job it began,
/// and the facts and skin it begins from when its admission was deferred.
pub struct EarlyPrep {
    gate: Arc<EarlyGate>,
    /// The early request's link to the render thread; none in tests of
    /// the gate alone.
    link: Option<Arc<EarlyLink>>,
    /// The decision lifetime it prepares for.
    lifetime: u64,
    /// The job, until joined.
    job: Option<Job<Result<Specialization, String>>>,
    /// When the job began.
    from: Instant,
    /// What a deferred start begins with at the bank's decode end.
    deferred: Option<(Arc<imports::PendingPlay>, skin_beatoraja::ImportFacts)>,
}

impl std::fmt::Debug for EarlyPrep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EarlyPrep")
            .field("lifetime", &self.lifetime)
            .field("state", &self.gate.state_name())
            .finish_non_exhaustive()
    }
}

impl EarlyPrep {
    /// Nothing begun yet for `lifetime` on `gate`.
    pub fn new(gate: Arc<EarlyGate>, lifetime: u64) -> Self {
        Self {
            gate,
            link: None,
            lifetime,
            job: None,
            from: Instant::now(),
            deferred: None,
        }
    }

    /// The gate.
    pub const fn gate(&self) -> &Arc<EarlyGate> {
        &self.gate
    }

    /// With the early request sent through `link`.
    #[must_use]
    pub fn with_link(mut self, link: Arc<EarlyLink>) -> Self {
        self.link = Some(link);
        self
    }

    /// The early request's link, if any.
    pub const fn link(&self) -> Option<&Arc<EarlyLink>> {
        self.link.as_ref()
    }

    /// The early request sent, handed to the attempt that adopts it.
    pub fn handoff(&self) -> Option<Handoff> {
        let request = self.gate.request()?;
        Some(Handoff {
            request,
            ticket: self.link.as_ref().and_then(|l| l.take_ticket()),
        })
    }

    /// The early request sent, if any, ended on the render thread.
    pub fn end_request(&self) {
        if let (Some(link), Some(request)) = (&self.link, self.gate.request()) {
            link.end(request);
        }
    }

    /// Begins `play`'s specialization for `facts` on the gate: on a
    /// `skin-import` thread, completing the gate at its catch boundary
    /// (a success, a refusal and a caught panic), the gate's lock held
    /// across the spawn.
    pub fn begin(
        &mut self,
        play: Arc<imports::PendingPlay>,
        facts: skin_beatoraja::ImportFacts,
    ) -> Start {
        self.from = Instant::now();
        let gate = Arc::clone(&self.gate);
        let job = &mut self.job;
        let mut unspawned = None;
        let started = self.gate.start(|| {
            let spawned = imports::spawn_job_completing(
                move |cancel: &Cancel| imports::specialize(&play, &facts, cancel),
                move |sent| {
                    let refused = match sent {
                        Ok(Err(why)) => Some(why.clone()),
                        Err(why) => Some((*why).to_owned()),
                        Ok(Ok(_)) => None,
                    };
                    // A refusal the gate keeps is this attempt's skin
                    // report, as Preparing's would be.
                    if gate.complete(outcome_of(sent))
                        && let Some(why) = refused
                    {
                        imports::ForAttempt::Refused(why).report(&imports::Specialized::Ran);
                    }
                },
                || {},
            )
            .inspect_err(|why| unspawned = Some(why.clone()))?;
            let cancel = spawned.canceller();
            *job = Some(spawned);
            Ok(cancel)
        });
        match started {
            Start::Running => log::info!("early: the specialization began"),
            Start::Ended => {
                log::info!("early: the specialization could not begin");
                // The thread that cannot start is this attempt's refusal,
                // reported as the worker's would be.
                if let Some(why) = unspawned {
                    imports::ForAttempt::Refused(why).report(&imports::Specialized::Ran);
                }
            }
            Start::Late => log::info!("early: the reveal came first; today's begin"),
        }
        started
    }

    /// The admission at the chart's read of `growth` against `machine`'s
    /// headroom read now: admitted, `play`'s specialization for `facts`
    /// begins; deferred, they are kept for [`readmit`](Self::readmit);
    /// declined, the gate ends [`Declined`](EarlyOutcome::Declined).
    pub fn admit_at_read(
        &mut self,
        machine: &dyn Machine,
        growth: Growth,
        play: Arc<imports::PendingPlay>,
        facts: skin_beatoraja::ImportFacts,
    ) -> Admitted {
        let answer = admit(growth, machine.headroom(), false, self.gate.revealed());
        // A script's step armed for the play skin's specialization acts on
        // Preparing's job (development builds): the early start declines.
        #[cfg(feature = "dev-tools")]
        let answer = if crate::dev::play_specializing_armed() {
            log::info!("early: declined, today's begin: a specializing step is armed");
            Admitted::Declined("a specializing step is armed".to_owned())
        } else {
            answer
        };
        match answer {
            Admitted::Yes => {
                self.begin(play, facts);
            }
            Admitted::Deferred => self.deferred = Some((play, facts)),
            Admitted::Declined(_) => {
                self.gate.end(EarlyOutcome::Declined);
            }
        }
        answer
    }

    /// The deferred start asked again, once, at the bank's decode end, with
    /// `growth` (the bank's part now none) against `machine`'s headroom
    /// read now: it begins, or the gate ends declined (today's begin).
    /// `None` without a deferred start.
    pub fn readmit(&mut self, machine: &dyn Machine, growth: Growth) -> Option<Admitted> {
        let (play, facts) = self.deferred.take()?;
        let answer = admit(growth, machine.headroom(), true, self.gate.revealed());
        if answer == Admitted::Yes {
            self.begin(play, facts);
        } else {
            self.gate.end(EarlyOutcome::Declined);
        }
        Some(answer)
    }

    /// Whether a start waits for the bank's decode end.
    pub const fn deferred(&self) -> bool {
        self.deferred.is_some()
    }

    /// The specialization the gate ended with, taken, with the time since
    /// its job began (none for a kept one).
    pub fn take_specialized(&self) -> Option<(Specialization, Duration)> {
        self.gate
            .take_specialized()
            .map(|s| (s, self.from.elapsed()))
    }

    /// Whether a job began and is not joined.
    pub const fn has_job(&self) -> bool {
        self.job.is_some()
    }

    /// The job is cancelled (the request ended): it stops at its next
    /// check, without a wait.
    pub fn cancel(&self) {
        if let Some(job) = &self.job {
            job.canceller().cancel();
        }
    }

    /// The job waited for to its thread's end and joined while `end_run`
    /// stays false (it was cancelled, or ended); once `end_run` turns true,
    /// cancelled and given up within [`imports::STOP_WAIT`]. Its outcome
    /// went through the gate; what the job sent is dropped here. Whether it
    /// was abandoned.
    pub fn join(&mut self, end_run: impl FnMut() -> bool) -> bool {
        let Some(job) = self.job.take() else {
            return false;
        };
        matches!(job.finish(|| false, end_run), Ran::Abandoned)
    }
}

impl super::PlayerRig<'_> {
    /// The early preparation as the load's looks serve it, while one
    /// exists.
    pub(crate) fn early_watch(&self) -> Option<crate::controls::EarlyWatch> {
        let early = self.early.as_ref()?;
        let link = early.link().cloned();
        Some(crate::controls::EarlyWatch {
            gate: Arc::clone(early.gate()),
            step: Box::new(move |gate: &EarlyGate| match &link {
                Some(link) => link.serve(gate),
                None => gate.step(|_| None),
            }),
        })
    }

    /// The early preparation served from App's decision looks
    /// (non-blocking).
    pub(crate) fn step_early(&self) {
        if let Some(early) = &self.early {
            match early.link() {
                Some(link) => link.serve(early.gate()),
                None => early.gate().step(|_| None),
            }
        }
    }

    /// The decided play's chart `loaded` is read: on the gate made at the
    /// decision's entry, a kept specialization holding for the attempt's
    /// facts ends it at once; otherwise the specialization's admission
    /// against the host's headroom read now, and its job begun, deferred to
    /// the bank's decode end, or declined (today's begin).
    pub(crate) fn begin_early(&mut self, loaded: &game::Loaded) {
        let (Some(early), Some(pending)) = (self.early.as_mut(), self.pending.as_ref()) else {
            return;
        };
        let facts = crate::imports::facts::attempt_facts(loaded, self.autoplay);
        if let Some(kept) = pending.kept_for(&facts) {
            if early.gate().end(EarlyOutcome::Specialized(kept)) {
                log::info!("early: the kept specialization holds for the chart");
            }
            return;
        }
        let growth = Growth {
            candidate: Growth::candidate(false),
            bank: Growth::bank(true, &audio::BankLimits::default()),
            pack: Growth::pack(self.result_skin.pack_charge()),
        };
        early.admit_at_read(self.machine, growth, pending.play(), facts);
    }

    /// The bank's decode ended: a start deferred at the read admitted
    /// again, once, with the bank's part none.
    pub(crate) fn readmit_early(&mut self) {
        let Some(early) = self.early.as_mut().filter(|e| e.deferred()) else {
            return;
        };
        let growth = Growth {
            candidate: Growth::candidate(false),
            bank: 0,
            pack: Growth::pack(self.result_skin.pack_charge()),
        };
        early.readmit(self.machine, growth);
    }

    /// The early preparation taken by the attempt's preparation (its
    /// settle, after the reveal): the specialization it ended with, if any,
    /// and how long its job ran (none for a kept one), its job joined to
    /// its thread's end (the run's end within the stop bound); and the
    /// early request sent, handed to the attempt's preparation.
    pub(crate) fn take_early(
        &mut self,
    ) -> (Option<(Specialization, Option<Duration>)>, Option<Handoff>) {
        let Some(mut early) = self.early.take() else {
            return (None, None);
        };
        let ran = early.has_job();
        let taken = early.take_specialized();
        // The early request sent goes to the attempt's request, which
        // adopts its staging when it still fits.
        let handoff = early.handoff();
        if early.join(|| self.run_ends()) {
            log::warn!("early: the import's thread was left to the process's exit");
        }
        (taken.map(|(s, took)| (s, ran.then_some(took))), handoff)
    }

    /// The choice's early preparation ended (a refusal, a quit, a close, a
    /// pick left): its job cancelled and joined to its thread's end, or
    /// given up within the stop bound once the run ends.
    pub(crate) fn end_early(&mut self) {
        let Some(mut early) = self.early.take() else {
            return;
        };
        early.cancel();
        early.end_request();
        if early.join(|| self.run_ends()) {
            log::warn!("early: the import's thread was left to the process's exit");
        }
    }
}

#[cfg(test)]
mod tests;
