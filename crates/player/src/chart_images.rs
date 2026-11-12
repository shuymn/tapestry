//! The chart's own images on play (S10-12 design §5): the stage file and
//! back image an attempt's labels name, read from the chart's folder under
//! T08's rules and fitted into the play installation's chart layer on one
//! owned worker, then published by the render thread, and settled before
//! the attempt's final readiness.
//!
//! - **The set** ([`ChartImages`]) is the two reads, each with the
//!   observation its pixels came from. A later attempt re-resolves both
//!   names and keeps a read whose observation is unchanged (design.md
//!   "Load Pipeline", Freshness): only a changed or new file is decoded.
//! - **The worker** ([`ChartImageWorker`]) runs at most one job, never
//!   beginning one while another's thread has not ended; a job of a chart
//!   left is cancelled through the choice's scope and kept until its
//!   thread ends. A panic, or a thread that cannot start, makes the
//!   images it had not finished absent and never ends the run.
//! - **The target** is the play installation the attempt draws with: the
//!   committed one, or a pending skin's specialization staged with an
//!   array of its own. The render thread answers what each publication
//!   left ([`Published`]); an installation with no chart layer is given
//!   the images absent, so a skin's "no image" presentation shows.
//! - **Direct launch:** with no coordinator, the images are read
//!   synchronously on the game thread in the attempt's preparation.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use game::coord::{
    Class, Coordinator, JobKey, JobKind, Outcome, OwnedJob, Owner, Scope, Settled, Spawner,
};
use skin_model::chart_image::{
    self, Absent, ChartDir, ChartImage, ChartTable, FileIdentity, Fitted, Read,
};

/// Holds every read of the images until let go, or until its cancel
/// answers (tests).
#[cfg(test)]
pub static HOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub use crate::chart_currency::{ChartNow, PublicationKey, PublishedTo};

/// Makes the next read of the images panic (tests: a job's panic).
#[cfg(test)]
pub static PANIC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What a publication left ([`render::ChartOutcome`] when it failed).
pub type Published = Result<ChartTable, render::ChartOutcome>;

/// The chart's stage file and back image as read, by [`ChartImage::index`].
#[derive(Debug)]
pub struct ChartImages {
    reads: [Arc<Read>; 2],
}

impl ChartImages {
    /// Both absent for `why`.
    #[must_use]
    pub fn absent(why: &Absent) -> Self {
        let read = || {
            Arc::new(Read {
                binding: chart_image::Binding::NoHeader,
                image: Err(why.clone()),
            })
        };
        Self {
            reads: [read(), read()],
        }
    }

    /// The fitted images, absent ones `None`.
    #[must_use]
    pub fn fitted(&self) -> [Option<&Fitted>; 2] {
        self.reads.each_ref().map(|r| r.image.as_ref().ok())
    }

    /// The read of `which`.
    #[must_use]
    pub fn read(&self, which: ChartImage) -> &Read {
        let [stage, back] = &self.reads;
        match which {
            ChartImage::Stage => stage,
            ChartImage::Back => back,
        }
    }

    /// The bytes of its fitted levels, for the memory witness.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.fitted()
            .iter()
            .flatten()
            .flat_map(|f| f.levels().iter())
            .map(|l| u64::try_from(l.len()).unwrap_or(u64::MAX))
            .fold(0, u64::saturating_add)
    }
}

/// What an attempt asks of the images: the chart's folder, its stage file
/// and back image names as its labels hold them, and the page side of the
/// target's chart layer; `None` when the target keeps no chart layer, whose
/// images are published absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Want {
    pub dir: PathBuf,
    pub names: [String; 2],
    pub side: Option<u32>,
}

/// Reads `want`'s images, re-resolving each name and keeping `prior`'s
/// read of an unchanged file (both observations equal, identities
/// available), and asking `cancel` at the reader's checkpoints. One image at
/// a time.
#[must_use]
pub fn read_set(
    want: &Want,
    side: u32,
    prior: Option<&ChartImages>,
    cancel: &dyn Fn() -> bool,
) -> ChartImages {
    let dir = match ChartDir::new(&want.dir) {
        Ok(dir) => dir,
        Err(e) => {
            return ChartImages::absent(&Absent::Unresolved(chart_image::Unresolved::Io(e.kind())));
        }
    };
    #[cfg(test)]
    while HOLD.load(std::sync::atomic::Ordering::Acquire) && !cancel() {
        std::thread::sleep(Duration::from_millis(1));
    }
    #[cfg(test)]
    assert!(
        !PANIC.swap(false, std::sync::atomic::Ordering::AcqRel),
        "an injected panic of the images' read"
    );
    let identity: chart_image::Identity<'_> = &file_identity;
    let mut reads = Vec::with_capacity(2);
    for (which, name) in ChartImage::ALL.into_iter().zip(&want.names) {
        let Some((_, slot)) = which.slot(side) else {
            reads.push(Arc::new(Read {
                binding: chart_image::Binding::NoHeader,
                image: Err(Absent::NoHeader),
            }));
            continue;
        };
        if let Some(prior) = prior {
            let kept = prior.read(which);
            let now = chart_image::resolve(&dir, name, &[], identity);
            // Kept only when unchanged and fitted for a slot it still
            // fits: a read for a larger layer is read again for this one.
            if now.same_as(&kept.binding) && kept.image.as_ref().is_ok_and(|f| slot.holds(f)) {
                reads.push(Arc::clone(&prior.reads[which.index()]));
                continue;
            }
        }
        reads.push(Arc::new(chart_image::read(
            &dir,
            name,
            &[],
            slot,
            cancel,
            identity,
        )));
    }
    let mut it = reads.into_iter();
    match (it.next(), it.next()) {
        (Some(stage), Some(back)) => ChartImages {
            reads: [stage, back],
        },
        _ => ChartImages::absent(&Absent::Failed),
    }
}

/// An open file's identity: its volume's serial number and its file
/// index (the NTFS file id), `None` when the system cannot give it.
#[must_use]
pub fn file_identity(file: &File) -> Option<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is the open file's for the call's duration, and
    // `info` is a valid out-parameter of the call's type.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &raw mut info) }.ok()?;
    Some(FileIdentity(
        (u128::from(info.dwVolumeSerialNumber) << 64)
            | (u128::from(info.nFileIndexHigh) << 32)
            | u128::from(info.nFileIndexLow),
    ))
}

/// The job's thread's bound at the run's end, and between attempts the
/// slice a wait for the worker parks.
pub const STOP_WAIT: Duration = Duration::from_secs(2);

/// A publication's answer: what it was for, and what it left.
#[derive(Clone, Debug)]
pub struct Ack {
    pub key: PublicationKey,
    pub answer: Published,
}

/// A publication asked for, by the game thread or by the job that read
/// the set, and its answer.
struct Publication {
    /// The generation and attempt it was asked for.
    generation: u64,
    request: Option<u64>,
    /// The set published, `None` for absent (or, for the job's own
    /// publication, until the job's set is taken).
    set: Option<Arc<ChartImages>>,
    /// The answer, until it came.
    rx: Option<mpsc::Receiver<Ack>>,
    /// The answer.
    ack: Option<Ack>,
    /// Sent by the job that read the set, which drops its sender unsent
    /// when it panics or is cancelled.
    by_job: bool,
    /// The job's sender dropped with nothing sent: nothing reached the
    /// render thread, so the set is published again.
    unsent: bool,
    /// The render thread is gone: no answer will come.
    gone: bool,
}

impl Publication {
    /// Takes an answer that came, if any.
    fn poll(&mut self) {
        let Some(rx) = &self.rx else {
            return;
        };
        match rx.try_recv() {
            Ok(ack) => {
                match &ack.answer {
                    Ok(t) => log::info!(
                        "chart images: published (stage file {}, back image {})",
                        t.present(ChartImage::Stage),
                        t.present(ChartImage::Back)
                    ),
                    Err(o) => log::info!("chart images: not published: {o:?}"),
                }
                self.ack = Some(ack);
                self.rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                if self.by_job {
                    self.unsent = true;
                } else {
                    self.gone = true;
                }
            }
        }
    }

    /// Whether it is queued or being published, its answer not taken: the
    /// render thread may still hold its set.
    const fn pending(&self) -> bool {
        self.rx.is_some()
    }
}

/// Where [`ChartImageWorker::publish`] stands for an attempt.
#[derive(Debug, PartialEq)]
pub enum Publish {
    /// The set is not read yet, or its publication has not returned.
    Waiting,
    /// The publication for the attempt returned, current, with this.
    Answered(Published),
    /// No current answer within the bound since the attempt's first
    /// publication: the render thread holds it.
    Expired,
}

/// What an answer from a render thread gone reads as.
const GONE: Published = Err(render::ChartOutcome::Unavailable);

/// A job of the worker, its thread not yet joined.
struct Job {
    want: Want,
    generation: u64,
    job: OwnedJob<Arc<ChartImages>>,
}

/// What a job begun later, from a wait, needs: the coordinator, the
/// spawner, and the publisher its set goes to.
type Starter = (
    Coordinator,
    Spawner,
    Option<crate::render_side::ChartPublisher>,
);

/// The worker's state, which the game thread and the loading waits'
/// service share ([`ChartImageWorker::service`]).
struct Core {
    /// What the current attempt asks for.
    want: Option<Want>,
    /// The current generation ([`PublicationKey::generation`]), which the
    /// render thread reads.
    generation: Arc<AtomicU64>,
    /// The job reading `want`'s images.
    job: Option<Job>,
    /// A job of an earlier want or generation, its thread not yet joined.
    obsolete: Option<OwnedJob<Arc<ChartImages>>>,
    /// The images last read, what for, and in which generation: current
    /// only for `want` in the current generation, else kept as the
    /// candidates a re-resolution reuses.
    set: Option<(Want, u64, Arc<ChartImages>)>,
    /// The set was drawn by a committed attempt: the next attempt of the
    /// same want re-resolves its files first.
    used: bool,
    /// The last publication.
    publication: Option<Publication>,
    /// A publication of an earlier generation or attempt, superseded while
    /// unanswered: the render thread may still hold its set. Nothing else
    /// is published, and no job begins, until it is answered, so at most
    /// the current set and this one are alive.
    retiring: Option<Publication>,
    /// The bound on the answer of the attempt's publication, from the
    /// first one for its generation and request, kept across publications
    /// asked again for it.
    deadline: Option<(u64, Option<u64>, Instant)>,
    /// A job the last step could not begin yet (a job's thread still
    /// alive, or a publication unanswered): the waits' service begins it.
    starter: Option<Starter>,
    /// Jobs begun (tests count decodes by it).
    begun: u64,
}

/// The run's one owner of the chart-image worker (module doc): a handle,
/// cloned for the loading waits' service, to one shared state.
#[derive(Clone)]
pub struct ChartImageWorker {
    core: Arc<Mutex<Core>>,
}

impl Default for ChartImageWorker {
    fn default() -> Self {
        Self::new(Arc::default())
    }
}

impl std::fmt::Debug for ChartImageWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let c = self.core();
        f.debug_struct("ChartImageWorker")
            .field("want", &c.want)
            .field("generation", &c.generation())
            .field("job", &c.job.as_ref().map(|j| (&j.want, j.generation)))
            .field("obsolete", &c.obsolete.is_some())
            .field("set", &c.set.as_ref().map(|(w, g, _)| (w, g)))
            .field("retiring", &c.retiring.is_some())
            .finish_non_exhaustive()
    }
}

impl ChartImageWorker {
    /// A worker writing its generation to `generation`, which the render
    /// thread reads ([`crate::render_side::RenderLink::chart_generation`]).
    #[must_use]
    pub fn new(generation: Arc<AtomicU64>) -> Self {
        Self {
            core: Arc::new(Mutex::new(Core {
                want: None,
                generation,
                job: None,
                obsolete: None,
                set: None,
                used: false,
                publication: None,
                retiring: None,
                deadline: None,
                starter: None,
                begun: 0,
            })),
        }
    }

    fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current generation (tests).
    #[cfg(test)]
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.core().generation()
    }

    /// Jobs begun (tests count decodes by it).
    #[cfg(test)]
    #[must_use]
    pub fn begun(&self) -> u64 {
        self.core().begun
    }

    /// A new generation: a choice, a practice Start, a retry's
    /// re-resolution or a chart left. Written at once where the render
    /// thread reads it, so no table or acknowledgment of the last one is
    /// current from here, and no frame drawn after a loading lifetime
    /// begun after this draws one. What the last generation read stays,
    /// only as the candidates the next read reuses; its job, if running,
    /// is left to end, and its set is never taken; its publication, if
    /// unanswered, retires.
    pub fn renew(&self) {
        self.core().renew();
    }

    /// Asks for `want`'s images for the attempt being prepared. A new want
    /// drops the last publication (retiring it while unanswered), which is
    /// published again for it. The same want after an attempt drew the set
    /// (a retry or a practice round) is a new generation
    /// ([`renew`](Self::renew)): its files are re-resolved, what did not
    /// change kept, and published again (design C11, P22).
    pub fn want(&self, want: Want) {
        let mut c = self.core();
        if c.used && c.current_for(&want).is_some() {
            c.renew();
        }
        if c.want.as_ref() != Some(&want) {
            c.supersede();
        }
        c.want = Some(want);
    }

    /// The current set was drawn by an attempt that committed.
    pub fn committed(&self) {
        let mut c = self.core();
        c.used = c.current().is_some();
    }

    /// The chart is left: a new generation with nothing asked for. Its
    /// job, if running, was cancelled through the choice's scope and is
    /// kept until its thread ends.
    pub fn forget(&self) {
        let mut c = self.core();
        c.renew();
        c.want = None;
        c.starter = None;
        if let Some(job) = c.job.take() {
            c.retire(job.job);
        }
    }

    /// Advances the worker without blocking: takes what ended, and begins
    /// the job `want` needs once no job's thread is alive and no
    /// publication is unanswered, else leaves it to the loading waits'
    /// [`service`](Self::service). `coord` `None` (the direct launch) reads
    /// at once on this thread, asking `cancel`. With `publish`, the skin in
    /// use's loading frames get what is terminal at once: the job publishes
    /// its set as soon as it is read, and a target with no chart layer, or a
    /// job that cannot start, publishes the images absent now.
    pub fn step(
        &self,
        coord: Option<(&Coordinator, Spawner)>,
        cancel: &dyn Fn() -> bool,
        publish: Option<&crate::render_side::ChartPublisher>,
    ) {
        let mut c = self.core();
        c.take_finished();
        let Some(want) = c.want.clone() else {
            return;
        };
        let generation = c.generation();
        let Some((coord, spawner)) = coord else {
            if c.current().is_some() || c.job.is_some() {
                return;
            }
            let Some(side) = want.side else {
                return;
            };
            let prior = c.set.as_ref().map(|(_, _, s)| Arc::clone(s));
            c.begun += 1;
            let set = Arc::new(read_set(&want, side, prior.as_deref(), cancel));
            log_absent(&set);
            c.set = Some((want, generation, set));
            log::info!("chart images: read; {} bytes retained", c.retained_bytes());
            return;
        };
        c.starter = Some((coord.clone(), spawner, publish.cloned()));
        c.begin_if_free();
    }

    /// The loading waits' service ([`crate::controls::Controls::look`]):
    /// what [`step`](Self::step) left, nonblocking, from whichever thread
    /// waits. Takes a finished job's set and answers, and begins a job the
    /// last step left once nothing holds it back. Nothing when the game
    /// thread holds the worker.
    pub fn service(&self) {
        let Ok(mut c) = self.core.try_lock() else {
            return;
        };
        c.take_finished();
        c.begin_if_free();
    }

    /// The set `want` asked for, once read in the current generation
    /// (tests).
    #[cfg(test)]
    #[must_use]
    pub fn current(&self) -> Option<Arc<ChartImages>> {
        self.core().current().cloned()
    }

    /// Publishes the current set (or absent, for a target with no chart
    /// layer) for the attempt of `request` once it is read, and takes its
    /// answer: [`Publish::Answered`] once the publication for that
    /// generation and request has returned and is still current
    /// ([`PublicationKey::current`]); one that is not, or one its job never
    /// sent, is asked again, once no other publication is unanswered.
    /// [`Publish::Expired`] when no current answer came within the render
    /// link's bound since the first publication for the generation and
    /// request.
    pub fn publish(
        &self,
        render: &crate::render_side::RenderLink,
        request: Option<u64>,
    ) -> Publish {
        let mut c = self.core();
        c.poll_answers();
        let Some(want) = c.want.as_ref() else {
            return Publish::Waiting;
        };
        let set = match want.side {
            Some(_) => match c.current() {
                Some(s) => Some(Arc::clone(s)),
                None => return Publish::Waiting,
            },
            None => None,
        };
        let generation = c.generation();
        // Read after the answers: the render thread mirrors its targets
        // before it answers.
        let now = render.chart_now(generation);
        let asked = c.publication.as_ref().is_some_and(|p| {
            p.generation == generation
                && p.request == request
                && same(p.set.as_ref(), set.as_ref())
                && !p.unsent
                && p.ack.as_ref().is_none_or(|a| a.key.current(&now, request))
        });
        let deadline = match c.deadline {
            Some((g, r, at)) if g == generation && r == request => at,
            _ => {
                let at = Instant::now() + render.answer_bound();
                c.deadline = Some((generation, request, at));
                at
            }
        };
        if !asked {
            c.supersede();
            if c.retiring.is_none() {
                c.publication = Some(Publication {
                    generation,
                    request,
                    set: set.clone(),
                    rx: Some(render.publish_chart_images(generation, request, set.clone())),
                    ack: None,
                    by_job: false,
                    unsent: false,
                    gone: false,
                });
            }
        }
        let answered = match c.publication.as_mut() {
            Some(p) => {
                p.poll();
                if p.gone {
                    Some(GONE)
                } else {
                    let now = render.chart_now(generation);
                    p.ack
                        .as_ref()
                        .filter(|a| {
                            a.key.current(&now, request) && same(p.set.as_ref(), set.as_ref())
                        })
                        .map(|a| a.answer)
                }
            }
            None => None,
        };
        match answered {
            Some(a) => Publish::Answered(a),
            None if Instant::now() >= deadline => Publish::Expired,
            None => Publish::Waiting,
        }
    }

    /// Whether no job's thread is alive.
    #[must_use]
    pub fn idle(&self) -> bool {
        let c = self.core();
        c.job.is_none() && c.obsolete.is_none()
    }

    /// At the run's end: every job is stopped and joined within
    /// [`STOP_WAIT`], or left to the process's exit (design P17); a panic
    /// is logged, never the run's.
    pub fn end(&self) {
        let mut c = self.core();
        c.publication = None;
        c.retiring = None;
        c.starter = None;
        let deadline = Instant::now() + STOP_WAIT;
        for job in [c.job.take().map(|j| j.job), c.obsolete.take()]
            .into_iter()
            .flatten()
        {
            let bound = deadline.saturating_duration_since(Instant::now());
            match job.settle(bound, &Instant::now, &mut || Some(())) {
                Settled::Joined {
                    after_panic: Some(p),
                    ..
                } => log::warn!("chart images: the worker stopped unexpectedly: {}", p.0),
                Settled::Joined { .. } => {}
                Settled::Abandoned { .. } => {
                    log::warn!("chart images: the worker was left to end with the process");
                }
            }
        }
    }
}

/// Whether two published sets are the same one (both absent included).
fn same(a: Option<&Arc<ChartImages>>, b: Option<&Arc<ChartImages>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

impl Core {
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn renew(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.supersede();
        self.used = false;
    }

    /// The last publication is no longer the one asked for: answered, it
    /// goes; unanswered, it retires. One already retiring stays the one
    /// (a publication is sent only while none retires, so the newer one is
    /// its successor and is dropped with its answer).
    fn supersede(&mut self) {
        if let Some(mut p) = self.publication.take() {
            p.poll();
            if p.pending() && self.retiring.is_none() {
                self.retiring = Some(p);
            }
        }
    }

    /// Takes the answers that came, the retiring one's and the last one's.
    fn poll_answers(&mut self) {
        if let Some(r) = &mut self.retiring {
            r.poll();
            if !r.pending() {
                self.retiring = None;
            }
        }
        if let Some(p) = &mut self.publication {
            p.poll();
        }
    }

    fn retire(&mut self, job: OwnedJob<Arc<ChartImages>>) {
        if let Some(old) = self.obsolete.take() {
            // Two ended jobs at once: the older is settled now; its
            // thread has ended or ends promptly once cancelled.
            settle_logged(old);
        }
        self.obsolete = Some(job);
    }

    fn current(&self) -> Option<&Arc<ChartImages>> {
        self.current_for(self.want.as_ref()?)
    }

    fn current_for(&self, want: &Want) -> Option<&Arc<ChartImages>> {
        self.set
            .as_ref()
            .filter(|(w, g, _)| w == want && *g == self.generation())
            .map(|(_, _, s)| s)
    }

    /// Takes what ended: the answers, an obsolete job joined, and a
    /// finished job's set when it is of the current generation and want
    /// and was not cancelled. Never blocks, never begins a job.
    fn take_finished(&mut self) {
        self.poll_answers();
        if self.obsolete.as_ref().is_some_and(OwnedJob::finished)
            && let Some(old) = self.obsolete.take()
        {
            settle_logged(old);
        }
        let Some(job) = self.job.take() else {
            return;
        };
        if !job.job.finished() {
            self.job = Some(job);
            return;
        }
        let (set, sent) = match job.job.poll().map(|o| o.outcome) {
            Some(Outcome::Done(set)) => (Some(set), true),
            Some(Outcome::Panicked(p)) => {
                log::warn!("chart images: the worker stopped unexpectedly: {}", p.0);
                (Some(Arc::new(ChartImages::absent(&Absent::Failed))), false)
            }
            // A cancelled read is never a terminal set: the next step
            // reads again what is still asked for.
            Some(Outcome::Cancelled) | None => (None, false),
        };
        settle_logged(job.job);
        if let Some(set) = set
            && job.generation == self.generation()
            && self.want.as_ref() == Some(&job.want)
        {
            log_absent(&set);
            if let Some(p) = self
                .publication
                .as_mut()
                .filter(|p| p.by_job && p.generation == job.generation && p.set.is_none())
            {
                // The job's own publication is of this set when the job
                // sent it; a panicked job sent nothing, and its failed set
                // is published by the game thread.
                if sent {
                    p.set = Some(Arc::clone(&set));
                } else {
                    p.unsent = true;
                }
            }
            self.set = Some((job.want, job.generation, set));
            log::info!(
                "chart images: read; {} bytes retained",
                self.retained_bytes()
            );
        }
    }

    /// Begins the job the last step left, once no job's thread is alive and
    /// no publication is unanswered, so a new set is never read while the
    /// render thread may hold two.
    fn begin_if_free(&mut self) {
        let Some((coord, spawner, publish)) = self.starter.clone() else {
            return;
        };
        let Some(want) = self.want.clone() else {
            self.starter = None;
            return;
        };
        let generation = self.generation();
        let Some(side) = want.side else {
            // No chart layer: the images are absent, terminal at once.
            if publish
                .as_ref()
                .is_none_or(|p| self.publish_now(p, generation, None))
            {
                self.starter = None;
            }
            return;
        };
        // A current set: what is left is its publication for the skin in
        // use, unless the job's own is it.
        if let Some(set) = self.current().cloned() {
            if publish
                .as_ref()
                .is_none_or(|p| self.publish_now(p, generation, Some(set)))
            {
                self.starter = None;
            }
            return;
        }
        if self.job.is_some()
            || self.obsolete.is_some()
            || self.retiring.is_some()
            || self.publication.as_ref().is_some_and(Publication::pending)
        {
            return;
        }
        // The starter stays until the set is published (above).
        // The last set's reads are the candidates: an unchanged file's read
        // that fits the slot is kept, not decoded again.
        let prior = self.set.as_ref().map(|(_, _, s)| Arc::clone(s));
        self.begun += 1;
        let key = JobKey {
            kind: JobKind::ChartImages,
            path: want.dir.clone(),
            speed: bms_model::Speed::NORMAL,
        };
        let read = want.clone();
        let (sender, rx) = match &publish {
            Some(p) => {
                let (reply, rx) = mpsc::sync_channel(1);
                (Some((p.clone(), reply)), Some(rx))
            }
            None => (None, None),
        };
        let spawned = coord.spawn_owned(
            spawner,
            key,
            (Class::Foreground, false, Owner::Foreground),
            coord.subscriber(Scope::Choice),
            |_| {},
            move |ctx| {
                let set = Arc::new(read_set(&read, side, prior.as_deref(), &|| ctx.cancelled()));
                if let Some((p, reply)) = sender
                    && !ctx.cancelled()
                {
                    p.publish(generation, None, Some(Arc::clone(&set)), reply);
                }
                set
            },
        );
        match spawned {
            Ok(job) => {
                if let Some(rx) = rx {
                    self.publication = Some(Publication {
                        generation,
                        request: None,
                        set: None,
                        rx: Some(rx),
                        ack: None,
                        by_job: true,
                        unsent: false,
                        gone: false,
                    });
                }
                self.job = Some(Job {
                    want,
                    generation,
                    job,
                });
            }
            Err(e) => {
                // A thread that cannot start: the images absent, terminal
                // at once, the run goes on (design C13).
                log::warn!("chart images: {e}");
                let set = Arc::new(ChartImages::absent(&Absent::Failed));
                self.set = Some((want, generation, set));
                // Published at once, or by the waits' service once the
                // retiring one is answered.
                self.begin_if_free();
            }
        }
    }

    /// Publishes `set` (absent when `None`) for the skin in use, unless
    /// that very publication is already asked for; `false` when another is
    /// unanswered, so nothing was sent.
    fn publish_now(
        &mut self,
        publisher: &crate::render_side::ChartPublisher,
        generation: u64,
        set: Option<Arc<ChartImages>>,
    ) -> bool {
        self.poll_answers();
        let asked = self.publication.as_ref().is_some_and(|p| {
            p.generation == generation
                && p.request.is_none()
                && same(p.set.as_ref(), set.as_ref())
                && !p.unsent
        });
        if asked {
            return true;
        }
        self.supersede();
        if self.retiring.is_some() {
            return false;
        }
        let (reply, rx) = mpsc::sync_channel(1);
        publisher.publish(generation, None, set.clone(), reply);
        self.publication = Some(Publication {
            generation,
            request: None,
            set,
            rx: Some(rx),
            ack: None,
            by_job: false,
            unsent: false,
            gone: false,
        });
        true
    }

    /// The bytes of the sets it keeps: the last read and, while the render
    /// thread may still hold them, the published and the retiring one.
    fn retained_bytes(&self) -> u64 {
        let set = self.set.as_ref().map(|(_, _, s)| s);
        let mut total = set.map_or(0, |s| s.bytes());
        let mut seen: Vec<&Arc<ChartImages>> = set.into_iter().collect();
        for p in [&self.publication, &self.retiring].into_iter().flatten() {
            if let Some(s) = &p.set
                && !seen.iter().any(|k| Arc::ptr_eq(k, s))
            {
                total = total.saturating_add(s.bytes());
                seen.push(s);
            }
        }
        total
    }
}

/// Settles an ended job, logging a panic after its publication.
fn settle_logged(job: OwnedJob<Arc<ChartImages>>) {
    if let Settled::Joined {
        after_panic: Some(p),
        ..
    } = job.settle(STOP_WAIT, &Instant::now, &mut || Some(()))
    {
        log::warn!("chart images: the worker stopped unexpectedly: {}", p.0);
    }
}

/// One load warning per image a chart names and could not be drawn
/// (S10-12 design §4.1).
fn log_absent(set: &ChartImages) {
    for which in ChartImage::ALL {
        match &set.read(which).image {
            Ok(_) | Err(Absent::NoHeader | Absent::Cancelled) => {}
            Err(why) => {
                let name = match which {
                    ChartImage::Stage => "stage file",
                    ChartImage::Back => "back image",
                };
                log::warn!("chart: the {name} is not shown: {why}");
                eprintln!("chart: the {name} is not shown: {why}");
            }
        }
    }
}

/// The chart's folder of `path`.
#[must_use]
pub fn folder(path: &Path) -> PathBuf {
    path.parent().map_or_else(PathBuf::new, Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use game::coord::{Caps, Coordinator, JobKind, Scope, Spawner};
    use skin_model::chart_image::{Absent, ChartImage};

    use super::{ChartImageWorker, Want};

    const CAPS: Caps = Caps {
        foreground: 4,
        speculative: 1,
    };
    const SIDE: u32 = 2_048;

    /// A `w`×`h` PNG of `rgb` at `dir/name`.
    fn png(dir: &Path, name: &str, [w, h]: [u32; 2], [r, g, b]: [u8; 3]) {
        let file = std::fs::File::create(dir.join(name)).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let texels = [r, g, b, 255].repeat(usize::try_from(w * h).unwrap());
        e.write_header().unwrap().write_image_data(&texels).unwrap();
    }

    fn want(dir: &Path, side: Option<u32>) -> Want {
        Want {
            dir: dir.to_path_buf(),
            names: ["s.png".to_owned(), "b.png".to_owned()],
            side,
        }
    }

    /// Steps `w` until `done` or 10 s.
    fn step_until(
        w: &ChartImageWorker,
        coord: &Coordinator,
        spawner: Spawner,
        done: impl Fn(&ChartImageWorker) -> bool,
    ) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done(w) {
            assert!(Instant::now() < end, "{w:?}");
            w.step(Some((coord, spawner)), &|| false, None);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A chart's stage file (640×480) and back image (512×384) are read on
    /// the worker, one job, and its thread is joined once it ended.
    #[test]
    fn the_worker_reads_both_images() {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        png(d.path(), "b.png", [512, 384], [40, 50, 60]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        w.want(want(d.path(), Some(SIDE)));
        step_until(&w, &coord, Spawner::SYSTEM, |w| {
            w.current().is_some() && w.idle()
        });
        let set = w.current().unwrap();
        assert_eq!(
            set.read(ChartImage::Stage).image.as_ref().unwrap().native(),
            [640, 480]
        );
        assert_eq!(
            set.read(ChartImage::Back).image.as_ref().unwrap().native(),
            [512, 384]
        );
        assert_eq!(w.begun(), 1);
        assert!(set.bytes() > 0);
    }

    /// A retry after an attempt drew the set re-resolves both files: an
    /// unchanged one keeps its read (the same pixels, not decoded again),
    /// a rewritten one is read again, and a name missing before and
    /// present now is read.
    #[test]
    fn a_retry_reads_only_what_changed() {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        let ready = |w: &ChartImageWorker| w.current().is_some() && w.idle();
        w.want(want(d.path(), Some(SIDE)));
        step_until(&w, &coord, Spawner::SYSTEM, ready);
        let first = w.current().unwrap();
        assert!(matches!(
            first.read(ChartImage::Back).image,
            Err(Absent::Unresolved(_))
        ));
        // Not drawn yet: the same want is the same set.
        w.want(want(d.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        assert!(std::sync::Arc::ptr_eq(&w.current().unwrap(), &first));
        // Drawn, then the back image appears: the stage file kept.
        w.committed();
        png(d.path(), "b.png", [512, 384], [40, 50, 60]);
        w.want(want(d.path(), Some(SIDE)));
        assert!(w.current().is_none(), "unavailable while re-resolved");
        step_until(&w, &coord, Spawner::SYSTEM, ready);
        let second = w.current().unwrap();
        assert!(std::ptr::eq(
            first.read(ChartImage::Stage),
            second.read(ChartImage::Stage)
        ));
        assert_eq!(
            second
                .read(ChartImage::Back)
                .image
                .as_ref()
                .unwrap()
                .native(),
            [512, 384]
        );
        // Drawn again, the stage file rewritten (another size): read again,
        // the back image kept.
        w.committed();
        std::thread::sleep(Duration::from_millis(20));
        png(d.path(), "s.png", [320, 240], [70, 80, 90]);
        w.want(want(d.path(), Some(SIDE)));
        step_until(&w, &coord, Spawner::SYSTEM, ready);
        let third = w.current().unwrap();
        assert_eq!(
            third
                .read(ChartImage::Stage)
                .image
                .as_ref()
                .unwrap()
                .native(),
            [320, 240]
        );
        assert!(std::ptr::eq(
            second.read(ChartImage::Back),
            third.read(ChartImage::Back)
        ));
        assert_eq!(w.begun(), 3);
    }

    /// A thread that cannot start leaves both images absent as failed,
    /// never a pending wait and never the run's failure.
    #[test]
    fn a_refused_thread_leaves_the_images_absent() {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        w.want(want(d.path(), Some(SIDE)));
        w.step(
            Some((&coord, Spawner::refusing_owned(&[JobKind::ChartImages]))),
            &|| false,
            None,
        );
        assert!(w.idle());
        let set = w.current().unwrap();
        assert_eq!(set.read(ChartImage::Stage).image, Err(Absent::Failed));
        assert_eq!(set.read(ChartImage::Back).image, Err(Absent::Failed));
    }

    /// A chart left while its job's thread is still in a read: the next
    /// chart's job does not begin beside it, and begins once that thread
    /// ended (one worker at a time).
    #[test]
    fn the_next_job_waits_for_the_last_ones_thread() {
        let _serial = crate::imports::tests::serial();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        png(a.path(), "s.png", [640, 480], [10, 20, 30]);
        png(b.path(), "s.png", [320, 240], [70, 80, 90]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        super::HOLD.store(true, std::sync::atomic::Ordering::Release);
        w.want(want(a.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        assert_eq!(w.begun(), 1);
        // A is left with its read held: B asked for, never begun meanwhile.
        w.forget();
        w.want(want(b.path(), Some(SIDE)));
        for _ in 0..20 {
            w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(w.begun(), 1, "B began beside A's thread");
        assert!(!w.idle());
        super::HOLD.store(false, std::sync::atomic::Ordering::Release);
        step_until(&w, &coord, Spawner::SYSTEM, |w| {
            w.current().is_some() && w.idle()
        });
        let set = w.current().unwrap();
        assert_eq!(
            set.read(ChartImage::Stage).image.as_ref().unwrap().native(),
            [320, 240]
        );
        assert_eq!(w.begun(), 2);
        let _ = Scope::Choice;
    }

    /// A choice left while its job's thread is in a read, and the same
    /// chart chosen again (the same folder and names): the cancelled job's
    /// read is never the new choice's set; the images are read again and
    /// drawn.
    #[test]
    fn a_cancelled_job_never_settles_the_next_choice() {
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        super::HOLD.store(true, std::sync::atomic::Ordering::Release);
        w.renew();
        w.want(want(d.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        assert_eq!(w.begun(), 1);
        // Left: the choice's scope cancelled; then chosen again.
        coord.advance(Scope::Choice);
        w.forget();
        w.renew();
        w.want(want(d.path(), Some(SIDE)));
        super::HOLD.store(false, std::sync::atomic::Ordering::Release);
        step_until(&w, &coord, Spawner::SYSTEM, |w| {
            w.current().is_some() && w.idle()
        });
        let set = w.current().unwrap();
        assert_eq!(
            set.read(ChartImage::Stage)
                .image
                .as_ref()
                .map(skin_model::chart_image::Fitted::native),
            Ok([640, 480]),
            "the cancelled read settled the new choice"
        );
        assert_eq!(w.begun(), 2);
    }

    /// A read fitted for a larger layer, its file unchanged, is read again
    /// for a smaller layer whose slot it does not fit: the new read is
    /// placed there.
    #[test]
    fn a_read_for_a_larger_layer_is_read_again_for_a_smaller_one() {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [1_600, 1_200], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        let ready = |w: &ChartImageWorker| w.current().is_some() && w.idle();
        w.want(want(d.path(), Some(4_096)));
        step_until(&w, &coord, Spawner::SYSTEM, ready);
        let large = w.current().unwrap();
        w.want(want(d.path(), Some(SIDE)));
        step_until(&w, &coord, Spawner::SYSTEM, ready);
        let small = w.current().unwrap();
        let fitted = small.read(ChartImage::Stage).image.as_ref().unwrap();
        assert!(
            skin_model::chart_image::ChartUv::of(fitted, ChartImage::Stage, SIDE, 0).is_some(),
            "the larger layer's read was kept: {:?}",
            fitted.padded()
        );
        assert!(!std::ptr::eq(
            large.read(ChartImage::Stage),
            small.read(ChartImage::Stage)
        ));
    }

    /// With no coordinator (the direct launch) the images are read at once
    /// on the calling thread; a target with no chart layer reads none.
    #[test]
    fn the_direct_launch_reads_at_once() {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let w = ChartImageWorker::default();
        w.want(want(d.path(), Some(SIDE)));
        w.step(None, &|| false, None);
        assert!(w.idle());
        let set = w.current().unwrap();
        assert!(set.read(ChartImage::Stage).image.is_ok());
        let none = ChartImageWorker::default();
        none.want(want(d.path(), None));
        none.step(None, &|| false, None);
        assert!(none.current().is_none());
        assert_eq!(none.begun(), 0);
    }
}
