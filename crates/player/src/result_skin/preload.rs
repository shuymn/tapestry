//! The result skin's preload (design.md "beatoraja Lua Skins", the result
//! skin's preload; "Load Pipeline", Pauses): the startup probe's atlas
//! packed once in selection, the decision screen or the practice page, on
//! a coordinator task of the speculative class whose thread is in
//! background mode.
//!
//! - **One owner.** The state ([`Preload`]) is the game thread's alone:
//!   it takes the image state for a pack ([`ResultSkin::slot_for_job`])
//!   and restores it when it collects the pack's outcome, which the pack's
//!   thread sends tagged with its job and its probe's mode. The thread
//!   never writes [`ResultSkin`].
//! - **The pause.** The shelf requests, waits for and releases the pause
//!   through the [`PreloadControl`] it shares: a pack stops at its next
//!   checkpoint while the pause holds and no choice claims it, says so
//!   before it drops its pages, and a new job packs from the start after
//!   the release. A stop is collected before another pack begins, so two
//!   packs never hold pages at once.
//! - **One warm-up.** Once a pack is collected, or an attempt runs its
//!   own image stage, the preload ends for the installation.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use game::coord::{Class, Coordinator, Outcome, Runner};
use skin_beatoraja::{Cancel, FactDeps, Imported, LuaImported};

use super::{
    ImageEntry, Images, Laid, Needs, Others, PendingResult, ResultSkin, Slot, StartupProbe,
    Unbuilt, lay_out,
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
    /// outcome is collected.
    Stopping { run: Running, then: Then },
    /// No preload for this installation: none was imported, or it ended.
    #[default]
    Retired,
}

/// What a stopped pack's preload becomes once its outcome is collected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Then {
    /// It waits for the next pack.
    Idle,
    /// It ends: its mode changed, or the installation ends.
    Retired,
}

/// A pack in flight.
pub(super) struct Running {
    id: u64,
    /// The probe it packs from, the preload's again when it stops.
    input: Box<StartupProbe>,
    /// The game thread's stop (a gate that no longer holds, a changed
    /// mode, the installation's end).
    stop: Cancel,
    /// Its outcome, once sent.
    done: Receiver<PackOut>,
    /// The holdings it took.
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

/// How a pack ended, with the holdings it took (the image entry it
/// packed, or what it left of them), `None` when it took none.
enum PackRan {
    /// Packed: the facts its probe read and the cells its layout needs.
    Built(Box<(Option<Images>, FactDeps, Needs)>),
    /// Its build would make a third live atlas.
    Deferred(Option<Images>),
    /// Stopped, its partial pages dropped on its thread.
    Cancelled(Option<Images>),
    /// Refused, with the reason.
    Refused(String, Option<Images>),
    /// Its thread could not enter background mode: no speculative pack
    /// runs again.
    NoBackground,
}

/// The preload's control, shared by the game thread, the shelf
/// ([`crate::shelf::Shelf`]'s pause) and the pack's thread. Only flags
/// cross threads; the state stays the game thread's.
#[derive(Debug, Default)]
pub struct PreloadControl {
    /// The shelf's pause holds.
    paused: AtomicBool,
    /// A choice to play or a Start claims the pack: the pause does not
    /// stop it.
    claimed: AtomicBool,
    /// The run ends: the pack stops.
    closing: AtomicBool,
    /// A pack's thread could not enter background mode.
    refused: AtomicBool,
    /// The job whose outcome the game thread still wants: one whose
    /// mode changed drops its pages on its own thread.
    wanted: AtomicU64,
    live: Mutex<Live>,
    changed: Condvar,
}

/// The pack's thread as the shelf's wait sees it.
#[derive(Clone, Copy, Debug, Default)]
struct Live {
    /// The job whose thread runs.
    job: Option<u64>,
    /// It has left its build: stopped at a checkpoint, or its build ended.
    left: bool,
}

impl PreloadControl {
    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The shelf's pause is requested (`true`) or released.
    pub fn set_paused(&self, on: bool) {
        self.paused.store(on, Ordering::SeqCst);
    }

    /// Whether the pause holds with no claim exempting the pack.
    fn held(&self) -> bool {
        self.paused.load(Ordering::SeqCst) && !self.claimed.load(Ordering::SeqCst)
    }

    /// Whether a running pack stops: the run ends, or the pause holds
    /// unclaimed.
    fn stops(&self) -> bool {
        self.closing.load(Ordering::SeqCst) || self.held()
    }

    /// Before a READY state: whether the pack acknowledges the pause, that
    /// is no pack's thread runs its build unclaimed, waiting at most
    /// `slice` for it.
    pub fn wait_acknowledged(&self, slice: Duration) -> bool {
        let quiet = |l: &Live| self.claimed.load(Ordering::SeqCst) || l.job.is_none() || l.left;
        let live = self.live();
        let (live, _) = self
            .changed
            .wait_timeout_while(live, slice, |l| !quiet(l))
            .unwrap_or_else(PoisonError::into_inner);
        quiet(&live)
    }

    /// The run ends: a running pack stops at its next checkpoint.
    pub fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    /// Whether the run's end was signalled ([`close`](Self::close)).
    fn closed(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
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

    /// Job `id` has left its build.
    fn left(&self, id: u64) {
        let mut live = self.live();
        if live.job == Some(id) && !live.left {
            live.left = true;
            drop(live);
            self.changed.notify_all();
        }
    }

    /// Job `id`'s thread ends.
    fn ended(&self, id: u64) {
        let mut live = self.live();
        if live.job == Some(id) {
            *live = Live::default();
            drop(live);
            self.changed.notify_all();
        }
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
    /// The pack on its own thread: background mode entered through the
    /// priority seam first, then the layout's atlas step on `runner`, its
    /// checkpoint the build's stop.
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
        super::IMPORT_PERMIT.set(kept);
        let mode = JobMode::new(mode, "skin: result: preload");
        mode.on_class(Class::Speculative);
        if mode.refused() {
            control.refused.store(true, Ordering::SeqCst);
            drop(runner);
            let _ = done.send(PackOut {
                id,
                kept,
                ran: PackRan::NoBackground,
            });
            return;
        }
        let stops = || stop.is_cancelled() || control.stops();
        let mut laid: Option<Laid> = None;
        let outcome = runner.run(&stops, &|class| mode.on_class(class), |ctx| {
            let check = || {
                #[cfg(all(test, feature = "dev-tools"))]
                tests::pack_gate(&|| stop.is_cancelled() || control.stops());
                let stopped = ctx.checkpoint();
                if stopped {
                    control.left(id);
                }
                stopped
            };
            laid = Some(lay_out(
                &pending, imported, &slot, base, others, &stop, &check,
            ));
        });
        control.left(id);
        let taken = |l: Option<Laid>| l.and_then(|l| l.taken);
        let ran = match (outcome, laid) {
            (Outcome::Done(()), Some(l)) => match l.result {
                Ok(spec) => match l.needs {
                    Some(needs) => PackRan::Built(Box::new((l.taken, spec.facts, needs))),
                    None => PackRan::Refused("no cells".to_owned(), l.taken),
                },
                Err(Unbuilt::Deferred) => PackRan::Deferred(l.taken),
                Err(Unbuilt::Refused(why)) => PackRan::Refused(why, l.taken),
            },
            (Outcome::Done(()), None) => PackRan::Refused("no layout".to_owned(), None),
            (Outcome::Cancelled, l) => PackRan::Cancelled(taken(l)),
            (Outcome::Panicked(p), l) => PackRan::Refused(format!("it panicked: {p}"), taken(l)),
        };
        // A pack whose mode changed meanwhile: its pages go here.
        let ran = match ran {
            PackRan::Built(..) if control.wanted.load(Ordering::SeqCst) != id => {
                drop(ran);
                PackRan::Cancelled(None)
            }
            ran => ran,
        };
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

    /// The preload at a look of selection, the decision screen or the
    /// practice page (design.md "beatoraja Lua Skins", the result skin's
    /// preload): a pack that ended is collected; a running one is stopped
    /// unless `may_run` (the practice page's jobs leave it room,
    /// [`crate::page_jobs::PageJobs::preload_may_run`]). Whether a pack is
    /// to begin now ([`begin_preload`](Self::begin_preload), after a fresh
    /// report): the preload waits, `may_run`, no pause holds unclaimed,
    /// `coord` has a speculative permit, and background mode was never
    /// refused.
    pub fn poll_preload(&mut self, coord: &Coordinator, may_run: bool) -> bool {
        self.collect_preload(false);
        if !may_run {
            self.stop_preload(Then::Idle);
        }
        let Some(control) = &self.control else {
            return false;
        };
        matches!(self.preload, Preload::Idle(_))
            && may_run
            && !control.held()
            && coord.caps().speculative > 0
            && !control.refused.load(Ordering::SeqCst)
    }

    /// Begins the pack on `coord`, after [`poll_preload`](Self::poll_preload)
    /// said so and the renderer's report was read again: on the game
    /// thread the atlases a build counts, then the image state taken for
    /// it, then its thread. A probe imported in the other mode than the
    /// one now preferred ends the preload; while two result atlases are
    /// live it does not build and waits.
    pub fn begin_preload(&mut self, coord: &Coordinator) {
        let Preload::Idle(input) = &self.preload else {
            return;
        };
        if self.kept != Some(input.kept) {
            log::info!("skin: result: not preloaded: its probe's mode is no longer preferred");
            self.preload = Preload::Retired;
            return;
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
        let runner = coord.task::<()>(Class::Speculative);
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

    /// The game thread stops a running pack: its stop is set, and once its
    /// outcome is collected the preload becomes `then` (a pack that
    /// stops for an ending preload stays ending).
    pub(super) fn stop_preload(&mut self, then: Then) {
        self.preload = match std::mem::take(&mut self.preload) {
            Preload::Running(run) => {
                run.stop.cancel();
                Preload::Stopping { run, then }
            }
            Preload::Stopping { run, then: was } => Preload::Stopping {
                run,
                then: if was == Then::Retired { was } else { then },
            },
            Preload::Idle(_) if then == Then::Retired => Preload::Retired,
            other => other,
        };
        if then == Then::Retired
            && let Some(c) = &self.control
        {
            c.wanted.store(0, Ordering::SeqCst);
        }
    }

    /// The pack's outcome collected, waited for when `wait` (its stop
    /// already set), else only when it is in. A pack is taken while its
    /// mode is the one preferred: its image entry restored and its probe's
    /// facts and cells kept, so the attempt reuses it, and the preload
    /// ends. A deferred or stopped pack hands its holdings back and the
    /// preload waits again (or ends, as its stop said); a refused one, or
    /// a thread that ended without an outcome, ends it.
    pub(super) fn collect_preload(&mut self, wait: bool) {
        let got = match &self.preload {
            Preload::Running(r) | Preload::Stopping { run: r, .. } => {
                if wait {
                    Some(r.done.recv().ok())
                } else {
                    match r.done.try_recv() {
                        Ok(out) => Some(Some(out)),
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => Some(None),
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
        let Some(out) = out.filter(|o| o.id == run.id) else {
            log::info!("skin: result: preload {} ended without its outcome", run.id);
            self.restore(None, &run.slot);
            return;
        };
        let current = self.kept == Some(out.kept) && then == Then::Idle;
        let waits = |input| match then {
            Then::Idle => Preload::Idle(input),
            Then::Retired => Preload::Retired,
        };
        self.preload = match out.ran {
            PackRan::Built(built) if current => {
                let (taken, deps, needs) = *built;
                log::info!("skin: result: preload {} packed", run.id);
                self.restore(taken, &run.slot);
                self.probe = Some((deps, needs));
                Preload::Retired
            }
            // Its mode is no longer preferred: nothing of it is restored.
            PackRan::Built(..) => {
                self.restore(None, &run.slot);
                Preload::Retired
            }
            PackRan::Deferred(taken) | PackRan::Cancelled(taken) => {
                self.restore(taken, &run.slot);
                waits(run.input)
            }
            PackRan::NoBackground => {
                self.restore(None, &run.slot);
                waits(run.input)
            }
            PackRan::Refused(why, taken) => {
                log::info!("skin: result: not preloaded: {why}");
                self.restore(taken, &run.slot);
                Preload::Retired
            }
        };
    }

    /// A running pack stopped and its outcome collected, waited for (its
    /// next checkpoint), before anything else takes the image state.
    pub(super) fn settle_preload(&mut self, then: Then) {
        if matches!(self.preload, Preload::Running(_) | Preload::Stopping { .. }) {
            self.stop_preload(then);
            self.collect_preload(true);
        }
    }

    /// The installation ends: a running pack is stopped and its thread
    /// waited for at most `bound`, unless the shelf's end already waited
    /// for it within the run's shared bound.
    pub(super) fn end_preload(&mut self, bound: Duration) {
        if !matches!(self.preload, Preload::Running(_) | Preload::Stopping { .. }) {
            return;
        }
        self.stop_preload(Then::Retired);
        self.preload = Preload::Retired;
        if let Some(c) = &self.control
            && !c.closed()
        {
            c.close();
            let began = Instant::now();
            if !c.wait_ended(bound) {
                log::warn!(
                    "skin: result: the preload did not stop within {} ms; it ends with the process",
                    began.elapsed().as_millis()
                );
            }
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
