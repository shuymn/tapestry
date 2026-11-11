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
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use game::coord::{
    Class, Coordinator, JobKey, JobKind, Outcome, OwnedJob, Owner, Scope, Settled, Spawner,
};
use skin_model::chart_image::{
    self, Absent, ChartDir, ChartImage, ChartKey, ChartTable, FileIdentity, Fitted, Read,
};

/// Holds every read of the images until let go, or until its cancel
/// answers (tests).
#[cfg(test)]
pub static HOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

/// Where a publication went, as the render thread answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishedTo {
    /// The staged array of the attempt of this request.
    Staged(u64),
    /// The committed play installation's chart layer, by its live key
    /// (`None` with no installation or on a lost device).
    Committed(Option<ChartKey>),
}

/// What a published table and its acknowledgment are for (design.md
/// "Rendering and Skins", Chart images): the images' generation, the
/// attempt, and the target the table went into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicationKey {
    /// The generation of the images' resolution
    /// ([`ChartImageWorker::renew`]): a new one at each choice, practice
    /// Start, retry's re-resolution and chart left.
    pub generation: u64,
    /// The attempt: its staging's request, `None` before any.
    pub request: Option<u64>,
    /// Where the table went.
    pub target: PublishedTo,
}

/// What a key is checked against: the current generation and the live
/// targets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChartNow {
    /// The generation the game thread set last.
    pub generation: u64,
    /// The request of the staging the render thread holds, if any.
    pub staged: Option<u64>,
    /// The committed play installation's live chart key.
    pub committed: Option<ChartKey>,
}

impl PublicationKey {
    /// Whether this publication is current at `now` for the attempt of
    /// `request`: the same generation, the same attempt, and its target
    /// still the live one. The one test of currency: the game thread's
    /// barrier and loading publication take an acknowledgment only while
    /// it holds, and the render thread publishes, and keeps a table drawn,
    /// only while it holds.
    #[must_use]
    pub fn current(&self, now: &ChartNow, request: Option<u64>) -> bool {
        self.generation == now.generation
            && self.request == request
            && match self.target {
                PublishedTo::Staged(r) => now.staged == Some(r),
                PublishedTo::Committed(k) => now.committed == k,
            }
    }
}

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
    /// The answer; `Err` when the render thread is gone.
    ack: Option<Result<Ack, ()>>,
}

impl Publication {
    fn poll(&mut self) {
        if let Some(rx) = &self.rx {
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
                    self.ack = Some(Ok(ack));
                    self.rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.ack = Some(Err(()));
                    self.rx = None;
                }
            }
        }
    }
}

/// Where [`ChartImageWorker::publish`] stands for an attempt.
#[derive(Debug, PartialEq)]
pub enum Publish<'a> {
    /// The set is not read yet, or its publication has not returned.
    Waiting,
    /// The publication for the attempt returned, current, with this.
    Answered(&'a Published),
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

/// The run's one owner of the chart-image worker (module doc).
pub struct ChartImageWorker {
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
    /// The bound on the answer of the attempt's publication, from the
    /// first one for its generation and request, kept across publications
    /// asked again for it.
    deadline: Option<(u64, Option<u64>, Instant)>,
    /// Jobs begun (tests count decodes by it).
    pub begun: u64,
}

impl Default for ChartImageWorker {
    fn default() -> Self {
        Self::new(Arc::default())
    }
}

impl std::fmt::Debug for ChartImageWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChartImageWorker")
            .field("want", &self.want)
            .field("generation", &self.generation())
            .field("job", &self.job.as_ref().map(|j| (&j.want, j.generation)))
            .field("obsolete", &self.obsolete.is_some())
            .field("set", &self.set.as_ref().map(|(w, g, _)| (w, g)))
            .finish_non_exhaustive()
    }
}

impl ChartImageWorker {
    /// A worker writing its generation to `generation`, which the render
    /// thread reads ([`crate::render_side::RenderLink::chart_generation`]).
    #[must_use]
    pub fn new(generation: Arc<AtomicU64>) -> Self {
        Self {
            want: None,
            generation,
            job: None,
            obsolete: None,
            set: None,
            used: false,
            publication: None,
            deadline: None,
            begun: 0,
        }
    }

    /// The current generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// A new generation: a choice, a practice Start, a retry's
    /// re-resolution or a chart left. Written at once where the render
    /// thread reads it, so no table or acknowledgment of the last one is
    /// current from here, and no frame drawn after a loading lifetime
    /// begun after this draws one. What the last generation read stays,
    /// only as the candidates the next read reuses; its job, if running,
    /// is left to end, and its set is never taken
    /// ([`take_finished`](Self::take_finished)).
    pub fn renew(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.publication = None;
        self.used = false;
    }

    /// Asks for `want`'s images for the attempt being prepared. A new want
    /// drops the last publication, which is published again for it. The
    /// same want after an attempt drew the set (a retry or a practice
    /// round) is a new generation ([`renew`](Self::renew)): its files are
    /// re-resolved, what did not change kept, and published again
    /// (design C11, P22).
    pub fn want(&mut self, want: Want) {
        if self.used && self.current_for(&want).is_some() {
            self.renew();
        }
        if self.want.as_ref() != Some(&want) {
            self.publication = None;
        }
        self.want = Some(want);
    }

    /// The current set was drawn by an attempt that committed.
    pub fn committed(&mut self) {
        self.used = self.current().is_some();
    }

    /// The chart is left: a new generation with nothing asked for. Its
    /// job, if running, was cancelled through the choice's scope and is
    /// kept until its thread ends.
    pub fn forget(&mut self) {
        self.renew();
        self.want = None;
        if let Some(job) = self.job.take() {
            self.retire(job.job);
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

    /// Takes what ended: joins an obsolete job, and takes a finished job's
    /// set when it is of the current generation and want and was not
    /// cancelled. Never blocks, never begins a job.
    fn take_finished(&mut self) {
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
        let set = match job.job.poll().map(|o| o.outcome) {
            Some(Outcome::Done(set)) => Some(set),
            Some(Outcome::Panicked(p)) => {
                log::warn!("chart images: the worker stopped unexpectedly: {}", p.0);
                Some(Arc::new(ChartImages::absent(&Absent::Failed)))
            }
            // A cancelled read is never a terminal set: the next step
            // reads again what is still asked for.
            Some(Outcome::Cancelled) | None => None,
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
                .filter(|p| p.generation == job.generation && p.set.is_none())
            {
                // The job's own publication: of this set.
                p.set = Some(Arc::clone(&set));
            }
            self.set = Some((job.want, job.generation, set));
            log::info!(
                "chart images: read; {} bytes retained",
                self.retained_bytes()
            );
        }
    }

    /// Advances the worker without blocking: joins ended jobs, takes a
    /// finished job's set, and begins the job `want` needs once no job's
    /// thread is alive. `coord` `None` (the direct launch) reads at once on
    /// this thread, asking `cancel`. With `publish`, the job publishes its
    /// set into the committed installation as soon as it is read, for the
    /// loading frames (the skin in use's), whatever the game thread waits
    /// on meanwhile.
    pub fn step(
        &mut self,
        coord: Option<(&Coordinator, Spawner)>,
        cancel: &dyn Fn() -> bool,
        publish: Option<&crate::render_side::ChartPublisher>,
    ) {
        self.take_finished();
        let Some(want) = self.want.clone() else {
            return;
        };
        let Some(side) = want.side else {
            return;
        };
        if self.current().is_some() || self.job.is_some() || self.obsolete.is_some() {
            return;
        }
        let generation = self.generation();
        // The last set's reads are the candidates: an unchanged file's read
        // that fits the slot is kept, not decoded again.
        let prior = self.set.as_ref().map(|(_, _, s)| Arc::clone(s));
        self.begun += 1;
        let Some((coord, spawner)) = coord else {
            let set = Arc::new(read_set(&want, side, prior.as_deref(), cancel));
            log_absent(&set);
            self.set = Some((want, generation, set));
            log::info!(
                "chart images: read; {} bytes retained",
                self.retained_bytes()
            );
            return;
        };
        let key = JobKey {
            kind: JobKind::ChartImages,
            path: want.dir.clone(),
            speed: bms_model::Speed::NORMAL,
        };
        let read = want.clone();
        let (sender, rx) = match publish {
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
                    });
                }
                self.job = Some(Job {
                    want,
                    generation,
                    job,
                });
            }
            Err(e) => {
                // A thread that cannot start: the images absent, the run
                // goes on (design C13).
                log::warn!("chart images: {e}");
                let set = Arc::new(ChartImages::absent(&Absent::Failed));
                self.set = Some((want, generation, set));
            }
        }
    }

    /// The set `want` asked for, once read in the current generation.
    #[must_use]
    pub fn current(&self) -> Option<&Arc<ChartImages>> {
        self.current_for(self.want.as_ref()?)
    }

    fn current_for(&self, want: &Want) -> Option<&Arc<ChartImages>> {
        self.set
            .as_ref()
            .filter(|(w, g, _)| w == want && *g == self.generation())
            .map(|(_, _, s)| s)
    }

    /// Publishes the current set (or absent, for a target with no chart
    /// layer) for the attempt of `request` once it is read, and takes its
    /// answer: [`Publish::Answered`] once the publication for that
    /// generation and request has returned and is still current
    /// ([`PublicationKey::current`]); one that is not is asked again.
    /// [`Publish::Expired`] when no current answer came within the render
    /// link's bound since the first publication for the generation and
    /// request.
    pub fn publish(
        &mut self,
        render: &crate::render_side::RenderLink,
        request: Option<u64>,
    ) -> Publish<'_> {
        let Some(want) = self.want.as_ref() else {
            return Publish::Waiting;
        };
        let set = match want.side {
            Some(_) => match self.current() {
                Some(s) => Some(Arc::clone(s)),
                None => return Publish::Waiting,
            },
            None => None,
        };
        let generation = self.generation();
        if let Some(p) = &mut self.publication {
            p.poll();
        }
        // Read after the answer: the render thread mirrors its targets
        // before it answers.
        let now = render.chart_now(generation);
        let asked = self.publication.as_ref().is_some_and(|p| {
            p.generation == generation
                && p.request == request
                && match (&p.set, &set) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    (None, None) => true,
                    _ => false,
                }
                && match &p.ack {
                    None | Some(Err(())) => true,
                    Some(Ok(a)) => a.key.current(&now, request),
                }
        });
        if !asked {
            self.publication = Some(Publication {
                generation,
                request,
                set: set.clone(),
                rx: Some(render.publish_chart_images(generation, request, set)),
                ack: None,
            });
        }
        let deadline = match self.deadline {
            Some((g, r, at)) if g == generation && r == request => at,
            _ => {
                let at = Instant::now() + render.answer_bound();
                self.deadline = Some((generation, request, at));
                at
            }
        };
        let Some(p) = self.publication.as_mut() else {
            return Publish::Waiting;
        };
        p.poll();
        match &p.ack {
            Some(Ok(a)) if a.key.current(&render.chart_now(generation), request) => {
                Publish::Answered(&a.answer)
            }
            Some(Err(())) => Publish::Answered(&GONE),
            _ if Instant::now() >= deadline => Publish::Expired,
            _ => Publish::Waiting,
        }
    }

    /// Whether no job's thread is alive.
    #[must_use]
    pub fn idle(&self) -> bool {
        self.job.is_none() && self.obsolete.is_none()
    }

    /// The bytes of the sets it keeps: the last read and, while the render
    /// thread may still hold it, the last published.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        let set = self.set.as_ref().map_or(0, |(_, _, s)| s.bytes());
        let published = self
            .publication
            .as_ref()
            .and_then(|p| p.set.as_ref())
            .filter(|p| self.set.as_ref().is_none_or(|(_, _, s)| !Arc::ptr_eq(p, s)))
            .map_or(0, |s| s.bytes());
        set.saturating_add(published)
    }

    /// At the run's end: every job is stopped and joined within
    /// [`STOP_WAIT`], or left to the process's exit (design P17); a panic
    /// is logged, never the run's.
    pub fn end(&mut self) {
        self.publication = None;
        let deadline = Instant::now() + STOP_WAIT;
        for job in [self.job.take().map(|j| j.job), self.obsolete.take()]
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
        w: &mut ChartImageWorker,
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
        let mut w = ChartImageWorker::default();
        w.want(want(d.path(), Some(SIDE)));
        step_until(&mut w, &coord, Spawner::SYSTEM, |w| {
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
        assert_eq!(w.begun, 1);
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
        let mut w = ChartImageWorker::default();
        let ready = |w: &ChartImageWorker| w.current().is_some() && w.idle();
        w.want(want(d.path(), Some(SIDE)));
        step_until(&mut w, &coord, Spawner::SYSTEM, ready);
        let first = std::sync::Arc::clone(w.current().unwrap());
        assert!(matches!(
            first.read(ChartImage::Back).image,
            Err(Absent::Unresolved(_))
        ));
        // Not drawn yet: the same want is the same set.
        w.want(want(d.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        assert!(std::sync::Arc::ptr_eq(w.current().unwrap(), &first));
        // Drawn, then the back image appears: the stage file kept.
        w.committed();
        png(d.path(), "b.png", [512, 384], [40, 50, 60]);
        w.want(want(d.path(), Some(SIDE)));
        assert!(w.current().is_none(), "unavailable while re-resolved");
        step_until(&mut w, &coord, Spawner::SYSTEM, ready);
        let second = std::sync::Arc::clone(w.current().unwrap());
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
        step_until(&mut w, &coord, Spawner::SYSTEM, ready);
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
        assert_eq!(w.begun, 3);
    }

    /// A thread that cannot start leaves both images absent as failed,
    /// never a pending wait and never the run's failure.
    #[test]
    fn a_refused_thread_leaves_the_images_absent() {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let mut w = ChartImageWorker::default();
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
        let mut w = ChartImageWorker::default();
        super::HOLD.store(true, std::sync::atomic::Ordering::Release);
        w.want(want(a.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        assert_eq!(w.begun, 1);
        // A is left with its read held: B asked for, never begun meanwhile.
        w.forget();
        w.want(want(b.path(), Some(SIDE)));
        for _ in 0..20 {
            w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(w.begun, 1, "B began beside A's thread");
        assert!(!w.idle());
        super::HOLD.store(false, std::sync::atomic::Ordering::Release);
        step_until(&mut w, &coord, Spawner::SYSTEM, |w| {
            w.current().is_some() && w.idle()
        });
        let set = w.current().unwrap();
        assert_eq!(
            set.read(ChartImage::Stage).image.as_ref().unwrap().native(),
            [320, 240]
        );
        assert_eq!(w.begun, 2);
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
        let mut w = ChartImageWorker::default();
        super::HOLD.store(true, std::sync::atomic::Ordering::Release);
        w.renew();
        w.want(want(d.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        assert_eq!(w.begun, 1);
        // Left: the choice's scope cancelled; then chosen again.
        coord.advance(Scope::Choice);
        w.forget();
        w.renew();
        w.want(want(d.path(), Some(SIDE)));
        super::HOLD.store(false, std::sync::atomic::Ordering::Release);
        step_until(&mut w, &coord, Spawner::SYSTEM, |w| {
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
        assert_eq!(w.begun, 2);
    }

    /// A read fitted for a larger layer, its file unchanged, is read again
    /// for a smaller layer whose slot it does not fit: the new read is
    /// placed there.
    #[test]
    fn a_read_for_a_larger_layer_is_read_again_for_a_smaller_one() {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [1_600, 1_200], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let mut w = ChartImageWorker::default();
        let ready = |w: &ChartImageWorker| w.current().is_some() && w.idle();
        w.want(want(d.path(), Some(4_096)));
        step_until(&mut w, &coord, Spawner::SYSTEM, ready);
        let large = std::sync::Arc::clone(w.current().unwrap());
        w.want(want(d.path(), Some(SIDE)));
        step_until(&mut w, &coord, Spawner::SYSTEM, ready);
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
        let mut w = ChartImageWorker::default();
        w.want(want(d.path(), Some(SIDE)));
        w.step(None, &|| false, None);
        assert!(w.idle());
        let set = w.current().unwrap();
        assert!(set.read(ChartImage::Stage).image.is_ok());
        let mut none = ChartImageWorker::default();
        none.want(want(d.path(), None));
        none.step(None, &|| false, None);
        assert!(none.current().is_none());
        assert_eq!(none.begun, 0);
    }
}
