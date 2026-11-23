//! The result skin's preload (design.md "beatoraja Lua Skins", the result
//! skin's preload; "Load Pipeline", Pauses): the startup probe's atlas
//! packed once in selection, the decision screen or the practice page, on
//! a coordinator task of the speculative class whose thread is in
//! background mode.
//!
//! The pack is a cancellable job handle with cooperative cancellation and
//! a deferred-destruction owner off the game thread: the game thread owns
//! the handle ([`Preload`]) and changes it only through one transition
//! table ([`ResultSkin::preload_on`] for the game thread's events, a
//! claim's included, [`ResultSkin::collect_preload`] for the pack's
//! outcome); the pack's thread observes a stop only at its checkpoints,
//! and pages nobody takes are dropped on the pack's thread or on the
//! preload's disposal thread ([`Disposer`], reserved before a pack
//! begins), never on the game thread. It reuses what the player has for
//! each part: the adopted stage's sliced polling with the attempt's quit
//! and run-end checks for every wait in Preparing
//! ([`ResultSkin::wait_pack`]); the page job's checked priority
//! ([`crate::page_jobs::JobMode`], its refusal read after each class hook)
//! for background mode, from the class of its first actual permit
//! ([`game::coord::Runner::on_new_thread`], as an owned job's thread);
//! the coordinator's promotion, revoked at a claim's end
//! ([`game::coord::Promoter::revoke`]), so the next chosen chart's bank
//! has the foreground pool's full width; and the shelf's pause,
//! acknowledgement and shared stop deadline ([`crate::shelf::Shelf`]) for
//! the pause and the run's end.
//!
//! - **One owner.** The state is the game thread's alone: it takes the
//!   image state for a pack ([`ResultSkin::slot_for_job`]) and, when it
//!   collects the pack's outcome, takes back the holdings the slot lent
//!   and keeps the new pages only of a pack still wanted. The thread never
//!   writes [`ResultSkin`].
//! - **The pause.** The shelf requests, waits for and releases the pause
//!   through the [`PreloadControl`] it shares: a pack stops at its next
//!   checkpoint while the pause holds and no choice claims it, says so
//!   before it drops its pages, and a new job packs from the start after
//!   the release. A stop is collected before another pack begins, so two
//!   packs never hold pages at once.
//! - **The claim.** A choice to play or a Start claims the pack
//!   ([`PreloadClaim`]): the pause does not stop it, and once the chosen
//!   chart's bank no longer decodes it is promoted to the foreground. The
//!   attempt's image stage adopts a claimed pack of its mode and waits for
//!   it in place of its own import and build; the claim's end demotes it,
//!   its foreground permit revoked at once, and hands it back to the
//!   preload. A pack that can run only in the foreground begins at the
//!   chosen chart's bank's end.
//! - **One warm-up.** Once a pack is collected, or an attempt runs its
//!   own image stage, the preload ends for the installation.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use game::coord::{Class, Coordinator, Outcome, Promoter, Runner};
use skin_beatoraja::{Cancel, Imported, LuaImported};

#[cfg(feature = "dev-tools")]
use super::gaps;
use super::{
    ImageEntry, Images, NotPacked, Others, Packed, PendingResult, ResultSkin, Slot, StartupProbe,
    Unbuilt, lock, pack,
};
use crate::page_jobs::JobMode;
use crate::priority::ThreadMode;

/// The preload's state, on the game thread.
#[derive(Debug, Default)]
pub(super) enum Preload {
    /// The startup probe, waiting for its pack.
    Idle(Box<StartupProbe>),
    /// Its pack runs.
    Running(Running),
    /// Its pack was told to stop by the game thread; `then` once its
    /// outcome is collected. Until then the image state it took stays
    /// with it: nothing else takes the image state (an attempt's stage
    /// waits for the collection, [`ResultSkin::wait_pack`]).
    Stopping { run: Running, then: Then },
    /// No preload for this installation: none was imported, or it ended.
    #[default]
    Retired,
}

/// How long a wait in Preparing for a pack's outcome looks for it before
/// it asks the attempt's signals again.
const PACK_SLICE: Duration = Duration::from_millis(5);

/// A running pack the attempt's image stage adopted, with the claim that
/// let it: dropped with the stage (the quit key, a discard), the claim
/// ends and the pack is handed back to the preload.
pub(super) struct Adopted {
    id: u64,
    _claim: PreloadClaim,
}

impl Adopted {
    /// Its pack's job.
    pub(super) const fn id(&self) -> u64 {
        self.id
    }
}

/// What a stopped pack's preload becomes once its outcome is collected,
/// in order: a later stop's `then` never lowers an earlier one's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Then {
    /// It waits for the next pack: the holdings the slot lent are
    /// returned, and a pack that completed anyway is kept.
    Idle,
    /// It ends (the attempt runs its own stage, the run disabled the
    /// result skin, the installation ends): the holdings the slot lent are
    /// returned, and new pages are disposed of off the game thread.
    Retired,
    /// It ends because its mode is no longer preferred: nothing of it is
    /// returned, the lent holdings included (they are the other mode's),
    /// and all of it is disposed of off the game thread.
    Dropped,
}

/// The game thread's events, which [`ResultSkin::preload_on`] applies
/// through the preload's transition table (a claim's events through
/// [`PreloadControl::on_claim`], the same table's rows).
#[derive(Clone, Copy, Debug)]
pub(super) enum Event<'a> {
    /// A look of selection, the decision screen or the practice page,
    /// `may_run` the practice page's jobs leaving room for a pack.
    Look { may_run: bool },
    /// A pack begins on the coordinator, as a look said it may and after
    /// the renderer's report was read again.
    Start(&'a Coordinator),
    /// The practice page's gate before it admits a job, or at its entry
    /// before its pause is released: a running pack is stopped unless
    /// `may_run`; nothing is collected or begun.
    Gate { may_run: bool },
    /// The kept probe's rule changed ([`ResultSkin::set_kept_probe`]).
    Invalidate,
    /// The run disabled the result skin: no skin work follows.
    Disable,
    /// The attempt's image stage begins without a pack to adopt.
    Attempt,
    /// The installation or the run ends: a pack is stopped and its thread
    /// waited for until the run's shared stop deadline, set `bound` from
    /// now unless the run's end set it already.
    End(Duration),
}

/// A pack in flight.
pub(super) struct Running {
    id: u64,
    /// The probe it packs from, the preload's again when it stops.
    input: Box<StartupProbe>,
    /// The game thread's stop (a gate that no longer holds, a changed
    /// mode, the installation's end), latched for this job.
    stop: Cancel,
    /// Its outcome, once sent.
    done: Receiver<PackOut>,
    /// The holdings it was lent.
    slot: Slot,
}

impl Running {
    /// Whether its probe was imported kept (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub(super) const fn kept(&self) -> bool {
        self.input.kept
    }
}

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Running")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// A pack's outcome: its job, the mode its probe was imported in, and
/// how it ended.
struct PackOut {
    id: u64,
    kept: bool,
    ran: PackRan,
}

/// How a pack ended.
#[expect(
    clippy::large_enum_variant,
    reason = "one outcome per pack, sent once and taken apart at once"
)]
enum PackRan {
    /// Packed and still wanted as it ended: the new entry (`None` when the
    /// lent entry held the layout's cells and stays in the slot), the
    /// facts its probe read and the cells its layout needs.
    Built(Box<Packed>),
    /// It packed nothing the preload keeps. `lent` is what is left of the
    /// holdings the slot lent, `None` when it took none; any pages it
    /// built were dropped on its thread.
    Ended { lent: Option<Images>, end: End },
}

/// Why a pack ended with nothing kept.
enum End {
    /// Its build would make a third live atlas.
    Deferred,
    /// Stopped (at a checkpoint or waiting for a permit), or packed after
    /// the game thread stopped wanting it.
    Cancelled,
    /// Refused, with the reason.
    Refused(String),
    /// Its thread could not enter background mode.
    NoBackground,
}

/// What the disposal thread runs: a drop, or a drain and a drop.
type Disposal = Box<dyn FnOnce() + Send>;

/// The preload's disposal thread, a deferred-destruction owner: what the
/// game thread hands it drops there, so pages nobody takes never free on
/// the game thread. It is reserved before the first pack begins, so every
/// disposal after it, the run's end included, has it without starting a
/// thread; it ends once the result skin lets it go and it has dropped
/// everything handed to it.
pub(super) struct Disposer(mpsc::Sender<Disposal>);

impl std::fmt::Debug for Disposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disposer").finish_non_exhaustive()
    }
}

impl Disposer {
    /// Its thread, started.
    fn start() -> std::io::Result<Self> {
        let (sender, handed) = mpsc::channel::<Disposal>();
        std::thread::Builder::new()
            .name("result-dispose".to_owned())
            .spawn(move || {
                for disposal in handed {
                    disposal();
                }
            })?;
        Ok(Self(sender))
    }

    /// Runs `disposal` on its thread. Should the thread be gone (it
    /// panicked), it runs here, logged.
    fn run(&self, disposal: Disposal) {
        if let Err(mpsc::SendError(disposal)) = self.0.send(disposal) {
            log::warn!("skin: result: the preload's disposal thread is gone; freed in place");
            disposal();
        }
    }
}

/// The preload's control, shared by the game thread, the shelf
/// ([`crate::shelf::Shelf`]'s pause) and the pack's thread. Only flags
/// cross threads; the state stays the game thread's.
#[derive(Debug, Default)]
pub struct PreloadControl {
    /// The shelf's pause holds.
    paused: AtomicBool,
    /// The claims live ([`PreloadClaim`]): while any does, the pause does
    /// not stop the pack.
    claims: AtomicUsize,
    /// The claimed pack is wanted in the foreground: its chart's bank no
    /// longer decodes. Cleared with the last claim.
    foreground: AtomicBool,
    /// The current pack's promoter, which a claim promotes and its end
    /// demotes.
    promoter: Mutex<Option<Promoter>>,
    /// The run ends: the pack stops.
    closing: AtomicBool,
    /// The run's stop deadline, shared by every wait at its end (the
    /// shelf's, the result skin's): set by the first.
    deadline: Mutex<Option<Instant>>,
    /// A pack's thread could not enter background mode, at its begin or
    /// at a later class change.
    refused: AtomicBool,
    /// The coordinator has no speculative permit.
    speculation_off: AtomicBool,
    /// The job whose outcome the game thread still wants: one it stopped
    /// for good drops its pages on its own thread.
    wanted: AtomicU64,
    live: Mutex<Live>,
    changed: Condvar,
}

/// The pack's thread as the shelf's wait sees it.
#[derive(Clone, Copy, Debug, Default)]
struct Live {
    /// The job whose thread runs.
    job: Option<u64>,
    /// It has left its build: stopped at a checkpoint or waiting for a
    /// permit, or its build ended.
    left: bool,
}

impl PreloadControl {
    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The shelf's pause is requested (`true`) or released.
    pub fn set_paused(&self, on: bool) {
        #[cfg(feature = "dev-tools")]
        let request = (on && !self.claimed()).then(|| self.stop_request());
        self.paused.store(on, Ordering::SeqCst);
        #[cfg(all(test, feature = "dev-tools"))]
        tests::after_stop_store();
        #[cfg(feature = "dev-tools")]
        if let Some(request) = request {
            mark_stop(request, "pause");
        }
    }

    /// Dev-tools: a stop's request, read before its flag is stored: the
    /// probe's time and the running pack's job, if one runs, so no
    /// observation of the stop can precede its mark and a job that leaves
    /// meanwhile still gets it ([`mark_stop`]).
    #[cfg(feature = "dev-tools")]
    fn stop_request(&self) -> Option<(bms_model::TimeUs, u64)> {
        let at = crate::dev::loop_probe::stamp()?;
        let live = *self.live();
        live.job.filter(|_| !live.left).map(|id| (at, id))
    }

    /// Whether the pause holds with no claim exempting the pack.
    fn held(&self) -> bool {
        self.paused.load(Ordering::SeqCst) && !self.claimed()
    }

    /// Whether a claim lives.
    pub fn claimed(&self) -> bool {
        self.claims.load(Ordering::SeqCst) > 0
    }

    /// Whether a claim lives and wants its pack in the foreground.
    fn foreground_claimed(&self) -> bool {
        self.claimed() && self.foreground.load(Ordering::SeqCst)
    }

    /// Whether no speculative pack can run: speculation is off, or a
    /// pack's thread could not enter background mode. A pack then runs
    /// only as foreground work, once a claim wants it there.
    fn alone(&self) -> bool {
        self.refused.load(Ordering::SeqCst) || self.speculation_off.load(Ordering::SeqCst)
    }

    /// A choice to play or a Start claims the pack (design.md "Load
    /// Pipeline", Pauses): from now until the claim drops, the pause does
    /// not stop it. The claim's holder asks for it before it requests the
    /// pause, so a pack never sees the choice's pause unclaimed.
    pub fn claim(self: &Arc<Self>) -> PreloadClaim {
        self.on_claim(ClaimEvent::Claim);
        // The job it attaches to: the running pack's now, else the one
        // begun under it later (`pre:claim` at its begin).
        #[cfg(feature = "dev-tools")]
        {
            let live = *self.live();
            match live.job.filter(|_| !live.left) {
                Some(id) => mark_job("pre:claim", id, ""),
                None => mark_job(
                    "pre:claim_open",
                    u64::try_from(self.claims.load(Ordering::SeqCst)).unwrap_or(0),
                    "",
                ),
            }
        }
        PreloadClaim(Arc::clone(self))
    }

    fn promoter(&self) -> MutexGuard<'_, Option<Promoter>> {
        self.promoter.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A claim's event, the transition table's Claim, Promote and Drop rows
    /// ([`ResultSkin::preload_on`]): on the game thread, on the shared
    /// flags and the current job's promoter, under the promoter's lock so
    /// a pack's begin ([`set_promoter`](Self::set_promoter)) sees each
    /// whole. The preload's state is not changed: the next look or the
    /// attempt reads the flags.
    fn on_claim(&self, event: ClaimEvent) {
        let current = self.promoter();
        match event {
            ClaimEvent::Claim => {
                self.claims.fetch_add(1, Ordering::SeqCst);
            }
            ClaimEvent::Promote => {
                self.foreground.store(true, Ordering::SeqCst);
                if let Some(p) = current.as_ref() {
                    p.promote();
                }
            }
            ClaimEvent::Drop => {
                if self.claims.fetch_sub(1, Ordering::SeqCst) == 1 {
                    self.foreground.store(false, Ordering::SeqCst);
                    if let Some(p) = current.as_ref() {
                        p.revoke();
                    }
                }
            }
        }
    }

    /// Job `promoter`'s begin: it is the one a claim promotes from now,
    /// promoted at once when a claim lives and wants it in the foreground
    /// already. Whether it was.
    fn set_promoter(&self, promoter: Promoter) -> bool {
        let mut current = self.promoter();
        let promote = self.foreground_claimed();
        if promote {
            promoter.promote();
        }
        *current = Some(promoter);
        promote
    }

    /// Whether a running pack stops: the run ends; the pause holds
    /// unclaimed; or no speculative pack can run and no claim wants it in
    /// the foreground (a claim's end demoted it, or its thread's mode was
    /// refused at a class change).
    fn stops(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
            || self.held()
            || (self.alone() && !self.foreground_claimed())
    }

    /// Before a READY state: whether the pack acknowledges the pause, that
    /// is no pack's thread runs its build unclaimed, waiting at most
    /// `slice` for it.
    pub fn wait_acknowledged(&self, slice: Duration) -> bool {
        let quiet = |l: &Live| self.claimed() || l.job.is_none() || l.left;
        let live = self.live();
        let (live, _) = self
            .changed
            .wait_timeout_while(live, slice, |l| !quiet(l))
            .unwrap_or_else(PoisonError::into_inner);
        quiet(&live)
    }

    /// The run ends: a running pack stops at its next checkpoint, and the
    /// run's stop deadline is `deadline` unless an earlier end set it.
    /// The deadline every wait of the run's end shares.
    pub fn close_by(&self, deadline: Instant) -> Instant {
        #[cfg(feature = "dev-tools")]
        let request = self.stop_request();
        self.closing.store(true, Ordering::SeqCst);
        #[cfg(feature = "dev-tools")]
        mark_stop(request, "close");
        let mut set = self.deadline.lock().unwrap_or_else(PoisonError::into_inner);
        *set.get_or_insert(deadline)
    }

    /// After [`close_by`](Self::close_by): waits until no pack's thread
    /// runs, until the run's stop deadline at most: whether none does.
    pub fn wait_closed(&self) -> bool {
        let deadline = *self.deadline.lock().unwrap_or_else(PoisonError::into_inner);
        let bound = deadline.map_or(Duration::ZERO, |d| {
            d.saturating_duration_since(Instant::now())
        });
        self.wait_ended(bound)
    }

    /// Waits until no pack's thread runs, at most `bound`: whether none
    /// does.
    pub fn wait_ended(&self, bound: Duration) -> bool {
        let live = self.live();
        let (live, _) = self
            .changed
            .wait_timeout_while(live, bound, |l| l.job.is_some())
            .unwrap_or_else(PoisonError::into_inner);
        live.job.is_none()
    }

    /// Job `id`'s thread begins.
    fn began(&self, id: u64) {
        self.wanted.store(id, Ordering::SeqCst);
        *self.live() = Live {
            job: Some(id),
            left: false,
        };
    }

    /// Job `id` has left its build, `stopped` there (at a checkpoint or
    /// waiting for a permit) or ended. Marked once per job (dev-tools:
    /// `pre:ack` for a stop, then the terminal `pre:left`).
    fn left(&self, id: u64, stopped: bool) {
        let mut live = self.live();
        if live.job == Some(id) && !live.left {
            live.left = true;
            drop(live);
            #[cfg(feature = "dev-tools")]
            if stopped {
                mark_job("pre:ack", id, "");
            }
            #[cfg(not(feature = "dev-tools"))]
            let _ = stopped;
            self.changed.notify_all();
        }
    }

    /// Job `id`'s thread ends (dev-tools: `pre:left`).
    fn ended(&self, id: u64) {
        let mut live = self.live();
        if live.job == Some(id) {
            *live = Live::default();
            drop(live);
            #[cfg(feature = "dev-tools")]
            crate::dev::loop_probe::mark("pre:left", i64::try_from(id).unwrap_or(-1));
            self.changed.notify_all();
        }
    }
}

/// Dev-tools: the preload's mark `tag` of job `id` (the transition probe,
/// [`crate::dev::loop_probe`]), and its `dev:` line with `what` when the
/// probe is on.
#[cfg(feature = "dev-tools")]
fn mark_job(tag: &'static str, id: u64, what: &str) {
    crate::dev::loop_probe::mark(tag, i64::try_from(id).unwrap_or(-1));
    if crate::dev::loop_probe::on() {
        if what.is_empty() {
            println!("dev: {tag} {id}");
        } else {
            println!("dev: {tag} {id} {what}");
        }
    }
}

/// Dev-tools: `pre:stop` of a stop's `request` ([`PreloadControl::stop_request`]),
/// kept at the request's time after its flag was stored.
#[cfg(feature = "dev-tools")]
fn mark_stop(request: Option<(bms_model::TimeUs, u64)>, why: &str) {
    let Some((at, id)) = request else {
        return;
    };
    crate::dev::loop_probe::mark_at("pre:stop", at, i64::try_from(id).unwrap_or(-1));
    if crate::dev::loop_probe::on() {
        println!("dev: pre:stop {id} {why}");
    }
}

/// Ends job `id`'s thread in the control as the thread lets it go, its
/// outcome sent or dropped (a panic included).
struct Ended<'a>(&'a PreloadControl, u64);

impl Drop for Ended<'_> {
    fn drop(&mut self) {
        self.0.ended(self.1);
    }
}

/// A claim's events ([`PreloadControl::on_claim`]).
#[derive(Clone, Copy, Debug)]
enum ClaimEvent {
    /// A choice to play or a Start claims the pack.
    Claim,
    /// The chosen chart's bank no longer decodes, or the attempt adopts.
    Promote,
    /// A claim ends.
    Drop,
}

/// A choice's claim on the result skin's preload pack (design.md "State
/// Model", the decision screen and the practice page): held from a choice
/// to play or a Start by its load, and moved into the attempt's
/// preparation, so every end of the choice drops it. While it lives the
/// pause does not stop the pack; dropped, the pack is demoted, the
/// foreground permit it holds revoked at once, and handed back to the
/// preload (with speculation off its next checkpoint then cancels it).
pub struct PreloadClaim(Arc<PreloadControl>);

impl std::fmt::Debug for PreloadClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreloadClaim").finish_non_exhaustive()
    }
}

impl PreloadClaim {
    /// The chosen chart's bank no longer decodes (or the attempt adopts
    /// the pack): the claimed pack, and any pack begun under the claim
    /// from now, is promoted to the foreground at its next checkpoint. A
    /// pack that can run only in the foreground begins from now on.
    pub fn promote(&self) {
        self.0.on_claim(ClaimEvent::Promote);
    }
}

impl Drop for PreloadClaim {
    fn drop(&mut self) {
        self.0.on_claim(ClaimEvent::Drop);
    }
}

/// What a pack's thread is given.
struct PackJob {
    id: u64,
    kept: bool,
    pending: Arc<PendingResult>,
    imported: LuaImported,
    slot: Slot,
    base: Option<Arc<ImageEntry>>,
    others: Others,
    stop: Cancel,
    control: Arc<PreloadControl>,
    mode: Arc<dyn ThreadMode>,
    done: mpsc::Sender<PackOut>,
}

impl PackJob {
    /// The pack on its own thread, which begins in the normal mode as
    /// `runner` records it ([`Runner::on_new_thread`]): its admission sets
    /// the thread's mode for the class of its first actual permit through
    /// the priority seam, background mode for a speculative one, whatever
    /// class it was begun in; then, unless that entry was refused, the
    /// atlas step, its checkpoint the build's stop. A refusal of
    /// background mode, at the admission or at a later class change, is
    /// recorded in the shared control at once, so no atlas work follows
    /// the admission's and the stop that the next checkpoint reads holds a
    /// later one (the page job's checked priority).
    fn run(self, runner: Runner<()>) {
        let Self {
            id,
            kept,
            pending,
            imported,
            slot,
            base,
            others,
            stop,
            control,
            mode,
            done,
        } = self;
        let _ended = Ended(&control, id);
        #[cfg(feature = "dev-tools")]
        {
            super::IMPORT_PERMIT.set(kept);
            // The trace's join key: the pack thread's OS id.
            // SAFETY: no arguments; it reads the calling thread's id.
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            crate::dev::loop_probe::mark("pre:tid", gaps::pair(id, i64::from(tid)));
        }
        let mode = JobMode::new(mode, "skin: result: preload");
        let checked = |class| {
            mode.on_class(class);
            if mode.refused() {
                control.refused.store(true, Ordering::SeqCst);
            }
        };
        #[cfg(all(test, feature = "dev-tools"))]
        tests::admission_gate();
        let stops = || stop.is_cancelled() || control.stops();
        let on_class = |class| {
            #[cfg(feature = "dev-tools")]
            if class == Class::Foreground {
                mark_job("pre:class", id, "fg");
            }
            checked(class);
        };
        #[cfg(feature = "dev-tools")]
        let progressed = AtomicBool::new(false);
        let mut packed = None;
        let mut no_background = false;
        let mut admitted = false;
        let outcome = runner.run(&stops, &on_class, |ctx| {
            admitted = true;
            // Admitted: the job's thread holds its permit, before its work.
            #[cfg(feature = "dev-tools")]
            {
                let class = match ctx.class() {
                    Some(Class::Foreground) => 1,
                    Some(Class::Speculative) => 2,
                    Some(Class::Maintenance) => 0,
                    None => 3,
                };
                crate::dev::loop_probe::mark("pre:admit", gaps::pair(id, class));
                gaps::admit(id);
            }
            // Admitted: its thread's mode is set for its permit's class.
            if mode.refused() {
                no_background = true;
                return;
            }
            // Each checkpoint is one bracket (dev-tools): the gate, a hold,
            // the coordinator's checkpoint and the marks lie outside every
            // stretch of the job's work.
            let check = || {
                #[cfg(feature = "dev-tools")]
                let site = gaps::site_now();
                #[cfg(feature = "dev-tools")]
                gaps::bracket_in(site);
                #[cfg(all(test, feature = "dev-tools"))]
                tests::pack_gate(&stops);
                #[cfg(feature = "dev-tools")]
                gaps::engage(id, site, false);
                let stopped = ctx.checkpoint();
                if stopped {
                    #[cfg(feature = "dev-tools")]
                    gaps::stopped_at(site);
                    #[cfg(all(test, feature = "dev-tools"))]
                    tests::after_seen();
                    control.left(id, true);
                    return true;
                }
                #[cfg(feature = "dev-tools")]
                {
                    gaps::engage(id, site, true);
                    // Its first progress: the latency from a release or a
                    // begin to its work.
                    if !progressed.swap(true, Ordering::SeqCst) {
                        mark_job("pre:progress", id, "");
                    }
                    gaps::bracket_out(site);
                }
                false
            };
            packed = Some(pack(&pending, imported, &slot, base, others, &stop, &check));
        });
        let cancelled = matches!(outcome, Outcome::Cancelled);
        // A waiter stopped in its admission saw its stop there.
        #[cfg(feature = "dev-tools")]
        if cancelled
            && !admitted
            && let Some(at) = crate::dev::loop_probe::stamp()
        {
            {
                crate::dev::loop_probe::mark_at(
                    "pre:seen",
                    at,
                    gaps::pair(id, i64::from(gaps::ADMIT)),
                );
            }
        }
        #[cfg(not(feature = "dev-tools"))]
        let _ = admitted;
        control.left(id, cancelled || no_background);
        // Pages it built that nobody takes drop here, on its own thread.
        let lent_of = |p: Option<Result<Packed, NotPacked>>| match p {
            Some(Ok((built, ..))) => {
                drop(built);
                None
            }
            Some(Err(not)) => not.1,
            None => None,
        };
        let wanted = control.wanted.load(Ordering::SeqCst) == id;
        let ran = match (outcome, packed) {
            _ if no_background => PackRan::Ended {
                lent: None,
                end: End::NoBackground,
            },
            (Outcome::Done(()), Some(Ok(built))) if wanted => PackRan::Built(Box::new(built)),
            (Outcome::Done(()), Some(Err(not))) => {
                let (why, lent) = *not;
                PackRan::Ended {
                    lent,
                    end: match why {
                        Unbuilt::Deferred => End::Deferred,
                        Unbuilt::Refused(why) => End::Refused(why),
                    },
                }
            }
            (Outcome::Done(()), None) => PackRan::Ended {
                lent: None,
                end: End::Refused("no layout".to_owned()),
            },
            (Outcome::Done(()) | Outcome::Cancelled, p) => PackRan::Ended {
                lent: lent_of(p),
                end: End::Cancelled,
            },
            (Outcome::Panicked(why), p) => PackRan::Ended {
                lent: lent_of(p),
                end: End::Refused(format!("it panicked: {why}")),
            },
        };
        #[cfg(feature = "dev-tools")]
        mark_job(
            "pre:end",
            id,
            match &ran {
                PackRan::Built(..) => "built",
                PackRan::Ended { end, .. } => match end {
                    End::Deferred => "deferred",
                    End::Cancelled => "cancelled",
                    End::Refused(_) => "refused",
                    End::NoBackground => "no-background",
                },
            },
        );
        #[cfg(feature = "dev-tools")]
        {
            gaps::finish();
            crate::dev::loop_probe::mark("pre:sent", i64::try_from(id).unwrap_or(-1));
        }
        // A receiver gone (the installation's end): the outcome, its pages
        // included, drops here.
        let _ = done.send(PackOut { id, kept, ran });
    }
}

impl StartupProbe {
    /// A copy of its import for one pack, which consumes it: the startup
    /// probe stays for the next.
    fn for_pack(&self) -> LuaImported {
        let i = &self.imported;
        LuaImported {
            imported: Imported {
                skin: i.imported.skin.clone(),
                report: i.imported.report.clone(),
                scratch: i.imported.scratch,
            },
            dependencies: i.dependencies.clone(),
            usage: i.usage,
            callbacks: i.callbacks.clone(),
            facts: i.facts.clone(),
            runtime: None,
        }
    }
}

impl ResultSkin {
    /// The preload's control, for the shelf's pause; none without a
    /// preload.
    pub fn preload_control(&self) -> Option<Arc<PreloadControl>> {
        self.control.clone()
    }

    /// The preload's transition table: every event changes the preload
    /// here, the claim's rows through [`PreloadControl::on_claim`], all on
    /// the game thread. The pack's outcome is collected by
    /// [`collect_preload`]'s table. Retired stays Retired at every event.
    /// Per row: the transition by state; who holds the pages after it
    /// ("lent": the image state the slot lent the pack, which nothing else
    /// takes until the collection, so a stopping pack quarantines it); and
    /// what waits.
    ///
    /// | Event | Transition | Pages | Wait |
    /// |---|---|---|---|
    /// | Look { may_run } | Running, Stopping: collected if ended; Running then stopped, then Idle, when `!may_run`, the pause holds unclaimed, or a foreground-only pack has no foreground claim. Idle stays (the caller starts it, [`poll_preload`]) | as collected; else unchanged | none |
    /// | Start(coord) | Idle, when the look said so: the disposal thread reserved first ([`Disposer`]); then, unless two result atlases are live, the image state taken into the slot, the job's promoter set (promoted when a claim wants the foreground) and its thread spawned: Running. A probe of a mode no longer preferred, or a disposal or pack thread that cannot start: Retired. Other states unchanged | lent to the pack; on a failed spawn, returned | none; the pack's thread sets its mode for the class of its first permit, checked, before any atlas work |
    /// | Claim | unchanged: the pause no longer stops the pack | unchanged | none |
    /// | Promote | unchanged: the current job promoted, in the foreground from its next checkpoint, and a foreground-only pack may start | unchanged | none |
    /// | Drop (the last claim) | unchanged: the current job demoted, its foreground permit revoked at once ([`game::coord::Promoter::revoke`]), its own class taken at its next checkpoint | unchanged | none |
    /// | Gate { may_run } | Running: stopped, then Idle, when `!may_run` | unchanged | none |
    /// | Invalidate | Idle: Retired. Running: stopped, then Dropped. Stopping: then Dropped | lent, quarantined until collected; then disposed of | none |
    /// | Disable, Attempt | Idle: Retired. Running: stopped, then Retired. Stopping: then at least Retired | lent, quarantined until collected; then returned | none here; the attempt's stage waits for the collection in Preparing's slices ([`wait_pack`]) |
    /// | End(bound) | Running, Stopping: stopped, then the receiver and the slot handed to the disposal thread: Retired. Idle: Retired | an outcome queued or sent later, and the lent holdings, drop on the disposal thread or the pack's, never here | the pack's thread, until the run's shared stop deadline |
    ///
    /// Where pages nobody takes go: the pack drops pages it built and
    /// that nobody wants on its own thread; the collection restores only
    /// lent holdings, keeps new pages only of a pack still wanted, and
    /// hands everything else to the disposal thread.
    ///
    /// [`collect_preload`]: Self::collect_preload
    /// [`poll_preload`]: Self::poll_preload
    /// [`wait_pack`]: Self::wait_pack
    pub(super) fn preload_on(&mut self, event: Event<'_>) {
        let in_flight = matches!(self.preload, Preload::Running(_) | Preload::Stopping { .. });
        match event {
            Event::Look { may_run } => {
                self.collect_preload(Duration::ZERO);
                let latch = !may_run || self.control.as_ref().is_some_and(|c| c.stops());
                if latch && matches!(self.preload, Preload::Running(_)) {
                    self.stop_preload(Then::Idle);
                }
            }
            Event::Start(coord) => self.start_pack(coord),
            Event::Gate { may_run } => {
                if !may_run && matches!(self.preload, Preload::Running(_)) {
                    self.stop_preload(Then::Idle);
                }
            }
            Event::Invalidate | Event::Disable | Event::Attempt => {
                let then = if matches!(event, Event::Invalidate) {
                    Then::Dropped
                } else {
                    Then::Retired
                };
                if in_flight {
                    self.stop_preload(then);
                } else {
                    self.preload = Preload::Retired;
                }
            }
            Event::End(bound) => {
                if !in_flight {
                    self.preload = Preload::Retired;
                    return;
                }
                self.stop_preload(Then::Retired);
                // Its receiver, with an outcome already queued or one the
                // pack sends later, and the lent slot go to the disposal
                // thread: nothing of the pack is freed here.
                if let Preload::Stopping { run, .. } = std::mem::take(&mut self.preload) {
                    let Running { done, slot, .. } = run;
                    self.dispose_with(Box::new(move || {
                        while let Ok(out) = done.try_recv() {
                            drop(out);
                        }
                        drop(done);
                        drop(slot);
                    }));
                }
                if let Some(c) = &self.control {
                    let began = Instant::now();
                    c.close_by(began + bound);
                    if !c.wait_closed() {
                        log::warn!(
                            "skin: result: the preload did not stop within {} ms; it ends with the process",
                            began.elapsed().as_millis()
                        );
                    }
                }
            }
        }
    }

    /// The preload at a look of selection, the decision screen or the
    /// practice page (design.md "beatoraja Lua Skins", the result skin's
    /// preload): [`Event::Look`]. Whether a pack is to begin now
    /// ([`begin_preload`](Self::begin_preload), after a fresh report): the
    /// preload waits, `may_run`, no pause holds unclaimed, and either a
    /// speculative pack can run (`coord` has a speculative permit and
    /// background mode was never refused) or a claim wants it in the
    /// foreground (its chart's bank no longer decodes), so a
    /// foreground-only pack never takes a permit the bank's decode draws.
    pub fn poll_preload(&mut self, coord: &Coordinator, may_run: bool) -> bool {
        self.preload_on(Event::Look { may_run });
        let Some(control) = &self.control else {
            return false;
        };
        control
            .speculation_off
            .store(coord.caps().speculative == 0, Ordering::SeqCst);
        matches!(self.preload, Preload::Idle(_))
            && may_run
            && !control.held()
            && (!control.alone() || control.foreground_claimed())
    }

    /// The practice page's gate ([`Event::Gate`]): a running pack is
    /// stopped unless `may_run`, before the page releases its pause or
    /// admits a job.
    pub fn gate_preload(&mut self, may_run: bool) {
        self.preload_on(Event::Gate { may_run });
    }

    /// Begins the pack on `coord` ([`Event::Start`]), after
    /// [`poll_preload`](Self::poll_preload) said so and the renderer's
    /// report was read again.
    pub fn begin_preload(&mut self, coord: &Coordinator) {
        self.preload_on(Event::Start(coord));
    }

    /// The Start row: on the game thread the disposal thread reserved, the
    /// atlases a build counts, then the image state taken for it, then its
    /// thread. A probe imported in the other mode than the one now
    /// preferred ends the preload; while two result atlases are live it
    /// does not build and waits.
    fn start_pack(&mut self, coord: &Coordinator) {
        let Preload::Idle(input) = &self.preload else {
            return;
        };
        if self.kept != Some(input.kept) {
            log::info!("skin: result: not preloaded: its probe's mode is no longer preferred");
            self.preload = Preload::Retired;
            return;
        }
        // The disposal thread before anything is lent: every disposal
        // after this, the run's end included, starts no thread.
        if self.disposer.is_none() {
            match Disposer::start() {
                Ok(d) => self.disposer = Some(d),
                Err(e) => {
                    log::info!(
                        "skin: result: not preloaded: its disposal thread cannot start: {e}"
                    );
                    self.preload = Preload::Retired;
                    return;
                }
            }
        }
        let (Some(control), Some(pending)) = (self.control.clone(), self.pending().map(Arc::clone))
        else {
            return;
        };
        // Two result atlases live besides what the build would release
        // ([`super::release`]): no build; it waits.
        let base = self.shown_entry();
        let mut live = self.others();
        live.extend(
            self.image
                .as_ref()
                .filter(|e| base.as_ref().is_some_and(|b| Arc::ptr_eq(e, b)))
                .map(|e| e.pages().id()),
        );
        live.sort_unstable();
        live.dedup();
        if live.len() >= 2 {
            return;
        }
        let Preload::Idle(input) = std::mem::take(&mut self.preload) else {
            return;
        };
        self.preload_jobs += 1;
        let id = self.preload_jobs;
        let others = self.counted("the preload");
        let slot = self.slot_for_job();
        let stop = Cancel::new();
        let (done, received) = mpsc::channel();
        // Of the speculative class whatever begins it, so a claim's end
        // demotes it there (with no speculative permit, its next
        // checkpoint then cancels it); a claim that wants it in the
        // foreground begins it promoted. Its new thread is in the normal
        // mode, as the runner records, until its first permit.
        let runner = coord.task::<()>(Class::Speculative).on_new_thread();
        #[cfg(feature = "dev-tools")]
        let task = runner.task_id();
        let promoted = control.set_promoter(runner.promoter());
        #[cfg(not(feature = "dev-tools"))]
        let _ = promoted;
        let job = PackJob {
            id,
            kept: input.kept,
            pending,
            imported: input.for_pack(),
            slot: Arc::clone(&slot),
            base,
            others,
            stop: stop.clone(),
            control: Arc::clone(&control),
            mode: self
                .preload_mode
                .clone()
                .unwrap_or_else(|| Arc::new(crate::priority::Checked::SYSTEM)),
            done,
        };
        control.began(id);
        match std::thread::Builder::new()
            .name("result-preload".to_owned())
            .spawn(move || job.run(runner))
        {
            Ok(_) => {
                log::info!("skin: result: preload {id} begins");
                #[cfg(feature = "dev-tools")]
                {
                    mark_job("pre:begin", id, if promoted { "fg" } else { "speculative" });
                    crate::dev::loop_probe::mark(
                        "pre:task",
                        gaps::pair(id, i64::try_from(task).unwrap_or(-1)),
                    );
                    // A claim that came before it attaches to it now.
                    if control.claimed() {
                        mark_job("pre:claim", id, "");
                    }
                }
                self.preload = Preload::Running(Running {
                    id,
                    input,
                    stop,
                    done: received,
                    slot,
                });
            }
            Err(e) => {
                control.ended(id);
                log::info!("skin: result: not preloaded: its thread cannot start: {e}");
                self.restore(None, &slot);
                self.preload = Preload::Retired;
            }
        }
    }

    /// The game thread stops a running pack: its stop is latched, and once
    /// its outcome is collected the preload becomes `then` (a later stop
    /// never lowers an earlier one's `then`). A `then` past Idle no longer
    /// wants its outcome, so a pack that packs anyway drops its pages on
    /// its own thread when it can tell.
    fn stop_preload(&mut self, then: Then) {
        self.preload = match std::mem::take(&mut self.preload) {
            Preload::Running(run) => {
                #[cfg(feature = "dev-tools")]
                let request = self.control.as_ref().and_then(|c| c.stop_request());
                run.stop.cancel();
                #[cfg(feature = "dev-tools")]
                mark_stop(
                    request,
                    match then {
                        Then::Idle => "gate",
                        Then::Retired => "retired",
                        Then::Dropped => "dropped",
                    },
                );
                Preload::Stopping { run, then }
            }
            Preload::Stopping { run, then: was } => Preload::Stopping {
                run,
                then: was.max(then),
            },
            other => other,
        };
        if then > Then::Idle
            && let Some(c) = &self.control
        {
            c.wanted.store(0, Ordering::SeqCst);
        }
    }

    /// The pack's outcome collected, waited for at most `wait` (zero at a
    /// look; a slice in Preparing). Its table, by the outcome and the
    /// state's `then` (Idle for a running pack):
    ///
    /// | Outcome | then Idle | then Retired | then Dropped |
    /// |---|---|---|---|
    /// | Built, its mode preferred | new entry restored, probe kept; Retired | new entry disposed of; what the slot still holds returned; Retired | new entry and slot disposed of; Retired |
    /// | Ended { lent } | lent returned; Idle (Retired when refused) | lent returned; Retired | lent and slot disposed of; Retired |
    /// | none (its thread ended without one) | slot returned; Retired | slot returned; Retired | slot disposed of; Retired |
    ///
    /// A Built outcome of a mode no longer preferred is collected as under
    /// Dropped. "Disposed of" is on a disposal thread, never here.
    pub(super) fn collect_preload(&mut self, wait: Duration) {
        let got = match &self.preload {
            Preload::Running(r) | Preload::Stopping { run: r, .. } => {
                if wait.is_zero() {
                    match r.done.try_recv() {
                        Ok(out) => Some(Some(out)),
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => Some(None),
                    }
                } else {
                    match r.done.recv_timeout(wait) {
                        Ok(out) => Some(Some(out)),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => Some(None),
                    }
                }
            }
            Preload::Idle(_) | Preload::Retired => None,
        };
        let Some(out) = got else {
            return;
        };
        let (run, then) = match std::mem::take(&mut self.preload) {
            Preload::Running(run) => (run, Then::Idle),
            Preload::Stopping { run, then } => (run, then),
            other => {
                self.preload = other;
                return;
            }
        };
        #[cfg(feature = "dev-tools")]
        mark_job("pre:collect", run.id, "");
        let Running {
            id, input, slot, ..
        } = run;
        let Some(out) = out.filter(|o| o.id == id) else {
            log::info!("skin: result: preload {id} ended without its outcome");
            self.hand_back(None, &slot, then);
            return;
        };
        let then = if self.kept == Some(out.kept) {
            then
        } else {
            Then::Dropped
        };
        self.preload = match (out.ran, then) {
            (PackRan::Built(built), Then::Idle) => {
                let (new, deps, needs) = *built;
                log::info!("skin: result: preload {id} packed");
                self.restore(new, &slot);
                self.probe = Some((deps, needs));
                Preload::Retired
            }
            (PackRan::Built(built), then) => {
                let (new, _, _) = *built;
                if let Some(new) = new {
                    self.dispose(new);
                }
                self.hand_back(None, &slot, then);
                Preload::Retired
            }
            (PackRan::Ended { lent, end }, then) => {
                self.hand_back(lent, &slot, then);
                match (then, end) {
                    (Then::Idle, End::Refused(why)) => {
                        log::info!("skin: result: not preloaded: {why}");
                        Preload::Retired
                    }
                    (Then::Idle, _) => Preload::Idle(input),
                    _ => Preload::Retired,
                }
            }
        };
    }

    /// Drops `what` on the disposal thread ([`Disposer`]), reserved before
    /// any pack began: nothing a pack leaves is freed on the game thread.
    pub(super) fn dispose<T: Send + 'static>(&self, what: T) {
        self.dispose_with(Box::new(move || drop(what)));
    }

    /// Runs `disposal` on the disposal thread; with none (no pack ever
    /// began, so it holds nothing of one) it runs here.
    fn dispose_with(&self, disposal: Disposal) {
        match &self.disposer {
            Some(d) => d.run(disposal),
            None => disposal(),
        }
    }

    /// The holdings the slot lent a collected pack, `lent` (what it left
    /// of what it took) or else what is still in `slot`: returned to the
    /// image state, or under [`Then::Dropped`] disposed of off the game
    /// thread.
    fn hand_back(&mut self, lent: Option<Images>, slot: &Slot, then: Then) {
        if then == Then::Dropped {
            if let Some(images) = lent.or_else(|| lock(slot).take()) {
                self.dispose(images);
            }
        } else {
            self.restore(lent, slot);
        }
    }

    /// The attempt's image stage adopts the running pack when `claim` is
    /// the live claim and the pack's probe was imported in the mode now
    /// preferred (design.md "beatoraja Lua Skins", the result skin's
    /// preload): the claim is promoted and moves into the stage, which
    /// waits for the pack in place of its own import and build. `None`
    /// otherwise (the claim, if any, dropped).
    pub(super) fn adopt_preload(&mut self, claim: Option<PreloadClaim>) -> Option<Adopted> {
        let Preload::Running(run) = &self.preload else {
            return None;
        };
        let claim = claim?;
        if self.kept != Some(run.input.kept) {
            return None;
        }
        claim.promote();
        // The adoption's mark before the hold it releases: the held pack's
        // `pre:hold_released` cannot come first (dev-tools).
        #[cfg(feature = "dev-tools")]
        {
            crate::dev::loop_probe::mark("img:adopted", i64::try_from(run.id).unwrap_or(-1));
            gaps::adopted(run.id);
        }
        log::info!("skin: result: the attempt adopts preload {}", run.id);
        Some(Adopted {
            id: run.id,
            _claim: claim,
        })
    }

    /// The job of the pack in flight, running or stopping.
    pub(super) const fn pack_in_flight(&self) -> Option<u64> {
        match &self.preload {
            Preload::Running(r) | Preload::Stopping { run: r, .. } => Some(r.id),
            Preload::Idle(_) | Preload::Retired => None,
        }
    }

    /// Preparing's wait for pack `id`'s outcome (an adopted pack, or one
    /// the attempt's stage stopped and must collect before it takes the
    /// image state), until `until` at most, in slices that ask the
    /// attempt's signals: `Some(true)` once its outcome is collected,
    /// `Some(false)` when `quit` (the attempt's quit key: nothing waited
    /// for, the pack stays the preload's) or `end_run` ([`Event::End`]
    /// within the run's shared stop deadline) came first, `None` at
    /// `until`. No wait here is unbounded.
    pub(super) fn wait_pack(
        &mut self,
        id: u64,
        until: Option<Instant>,
        mut quit: impl FnMut() -> bool,
        mut end_run: impl FnMut() -> bool,
    ) -> Option<bool> {
        loop {
            if end_run() {
                self.preload_on(Event::End(crate::imports::STOP_WAIT));
                return Some(false);
            }
            if quit() {
                return Some(false);
            }
            let now = Instant::now();
            let slice = until.map_or(PACK_SLICE, |u| {
                PACK_SLICE.min(u.saturating_duration_since(now))
            });
            self.collect_preload(slice.max(Duration::from_micros(1)));
            if self.pack_in_flight() != Some(id) {
                return Some(true);
            }
            if until.is_some_and(|u| Instant::now() >= u) {
                return None;
            }
        }
    }

    /// The run disabled the result skin ([`Event::Disable`]): a running
    /// pack is stopped, not waited for; its outcome is collected at a
    /// later look or the end, and the preload ends.
    pub fn retire_preload(&mut self) {
        self.preload_on(Event::Disable);
    }

    /// The installation ends ([`Event::End`]): a running pack is stopped
    /// and its thread waited for until the run's shared stop deadline,
    /// set `bound` from now unless the run's end set it already.
    pub(super) fn end_preload(&mut self, bound: Duration) {
        self.preload_on(Event::End(bound));
    }

    /// The holdings lent to the pack in flight (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub(super) fn pack_slot(&self) -> Option<Slot> {
        match &self.preload {
            Preload::Running(r) | Preload::Stopping { run: r, .. } => Some(Arc::clone(&r.slot)),
            Preload::Idle(_) | Preload::Retired => None,
        }
    }

    /// The preload's pack thread mode in place of the system's (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn set_preload_mode(&mut self, mode: Arc<dyn ThreadMode>) {
        self.preload_mode = Some(mode);
    }

    /// The preload's state (tests): its probe's mode while it waits, its
    /// job while a pack runs or stops, or neither once it ended.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn preload_state(&self) -> (Option<bool>, Option<u64>) {
        match &self.preload {
            Preload::Idle(p) => (Some(p.kept), None),
            Preload::Running(r) | Preload::Stopping { run: r, .. } => (None, Some(r.id)),
            Preload::Retired => (None, None),
        }
    }
}

#[cfg(all(test, feature = "dev-tools"))]
pub(crate) mod tests;
