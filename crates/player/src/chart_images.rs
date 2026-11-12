//! The chart's own images (S10-12 design §5): the stage file and back
//! image an attempt's labels name, read from the chart's folder under
//! T08's rules, fitted into each installation's chart layer on one owned
//! worker, and published by the render thread.
//!
//! - **Sources** ([`Sources`]): each generation (a choice, a practice
//!   Start, a retry's re-resolution) decodes each named image once, fitted
//!   to the largest chart slot any installation can have, level 0 only. A
//!   later generation re-resolves both names and keeps a source whose
//!   observation is unchanged (design.md "Load Pipeline", Freshness): only
//!   a changed or new file is decoded.
//! - **Halves**: every installation's fit is derived from the sources
//!   ([`chart_image::derive`]): the play half for the play installation,
//!   the result half for the result's. A half is kept per image while its
//!   source is the same read, and a fit one slot would derive as another's
//!   is shared.
//! - **Availability** ([`ImageFacts`]) is the worker's: unresolved from a
//!   generation's start until its sources are adopted, then whether each
//!   source is an image. A failed job resolves both absent.
//! - **The worker** ([`ChartImageWorker`]) runs at most one job, never
//!   beginning one while another's thread has not ended; a job of a chart
//!   left is cancelled through the choice's scope and kept until its
//!   thread ends. A panic, or a thread that cannot start, makes the
//!   images it had not finished absent and never ends the run.
//! - **The play target** is the play installation the attempt draws
//!   with: the committed one, or a pending skin's specialization staged
//!   with an array of its own. The render thread answers what each
//!   publication left ([`Published`]); an installation with no chart layer
//!   is given the images absent, so a skin's "no image" presentation shows.
//!   The result half is handed to the result at the attempt's commit.
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
use skin_model::ImageFacts;
use skin_model::chart_image::{
    self, Absent, ChartDir, ChartImage, ChartTable, FileIdentity, Fitted, Read,
};

/// Holds every read of the images until let go, or until its cancel
/// answers (tests).
#[cfg(test)]
pub static HOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub use crate::chart_currency::{ChartNow, PublicationKey, PublishedTo};

// A name the library keeps is one the chart-image reader accepts.
const _: () = assert!(library::IMAGE_NAME_BYTES == skin_model::MAX_PATH_BYTES);

/// Holds every read of the images until let go, whatever its cancel says
/// (tests: a job that ignores its cancel).
#[cfg(test)]
pub static HOLD_HARD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set while a read is held by [`HOLD_HARD`] (tests).
#[cfg(test)]
pub static HELD_HARD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Makes the next read of the images panic (tests: a job's panic).
#[cfg(test)]
pub static PANIC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    /// Files decoded into sources, and fits derived from them, on this
    /// thread (tests: a job hands its counts back with its output).
    static COUNTS: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

/// Counts a decode (`[1, 0]`) or a derivation (`[0, 1]`) (tests).
#[cfg(test)]
fn count([decodes, derives]: [u64; 2]) {
    COUNTS.with(|c| {
        let (d, f) = c.get();
        c.set((d + decodes, f + derives));
    });
}

/// What a publication left ([`render::ChartOutcome`] when it failed).
pub type Published = Result<ChartTable, render::ChartOutcome>;

/// The next set's id.
static SET_IDS: AtomicU64 = AtomicU64::new(1);

/// The chart's stage file, back image and banner as read, by
/// [`ChartImage::index`],
/// and the set's id, unique in the process: what an array's record names
/// ([`render::Renderer::write_result_chart`]).
#[derive(Debug)]
pub struct ChartImages {
    reads: [Arc<Read>; 3],
    id: u64,
}

/// The sources of a generation: each image decoded once, level 0 only,
/// fitted to the largest chart slot ([`chart_image::read_source`]).
pub type Sources = ChartImages;

impl ChartImages {
    fn of(reads: [Arc<Read>; 3]) -> Self {
        Self {
            reads,
            id: SET_IDS.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// All absent for `why`.
    #[must_use]
    pub fn absent(why: &Absent) -> Self {
        let read = || {
            Arc::new(Read {
                binding: chart_image::Binding::NoHeader,
                image: Err(why.clone()),
            })
        };
        Self::of([read(), read(), read()])
    }

    /// Its id.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// The fitted images, absent ones `None`.
    #[must_use]
    pub fn fitted(&self) -> [Option<&Fitted>; 3] {
        self.reads.each_ref().map(|r| r.image.as_ref().ok())
    }

    /// The read of `which`.
    #[must_use]
    pub fn read(&self, which: ChartImage) -> &Read {
        let [stage, back, banner] = &self.reads;
        match which {
            ChartImage::Stage => stage,
            ChartImage::Back => back,
            ChartImage::Banner => banner,
        }
    }

    /// Whether each image is one: the availability of sources.
    #[must_use]
    pub fn facts(&self) -> ImageFacts {
        ImageFacts {
            stage: self.read(ChartImage::Stage).image.is_ok(),
            back: self.read(ChartImage::Back).image.is_ok(),
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

/// What an attempt asks of the images: the chart's folder and its stage
/// file, back image and banner names as its labels hold them (an empty
/// name reads nothing: play and result name no banner), and the page side of
/// each target's chart layer, the play installation's and the result's;
/// `None` for a target that keeps no chart layer (play's images are then
/// published absent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Want {
    pub dir: PathBuf,
    pub names: [String; 3],
    pub play: Option<u32>,
    pub result: Option<u32>,
}

impl Want {
    /// Whether `other` names the same files: the same sources.
    fn same_files(&self, other: &Self) -> bool {
        self.dir == other.dir && self.names == other.names
    }
}

/// Decodes `want`'s images into sources, re-resolving each name and
/// keeping `prior`'s read of an unchanged file (both observations equal,
/// identities available), asking `cancel` at the reader's checkpoints. One
/// image at a time.
#[must_use]
pub fn read_sources(want: &Want, prior: Option<&Sources>, cancel: &dyn Fn() -> bool) -> Sources {
    let dir = match ChartDir::new(&want.dir) {
        Ok(dir) => dir,
        Err(e) => {
            return ChartImages::absent(&Absent::Unresolved(chart_image::Unresolved::Io(e.kind())));
        }
    };
    #[cfg(test)]
    hold(cancel);
    let identity: chart_image::Identity<'_> = &file_identity;
    let reads = ChartImage::ALL.map(|which| {
        let name = want.names.get(which.index()).map_or("", String::as_str);
        let now = chart_image::resolve(&dir, name, &[], identity);
        if let Some(prior) = prior {
            let kept = prior.read(which);
            if now.same_as(&kept.binding) && reusable(kept) {
                return Arc::clone(&prior.reads[which.index()]);
            }
        }
        match now {
            chart_image::Binding::Resolved { .. } => {
                #[cfg(test)]
                count([1, 0]);
                Arc::new(chart_image::read_source(&dir, name, &[], cancel, identity))
            }
            binding => absent_read(binding),
        }
    });
    ChartImages::of(reads)
}

/// The read of an image `binding` leaves unread: absent for its
/// unresolved name's reason, else as named by no header.
fn absent_read(binding: chart_image::Binding) -> Arc<Read> {
    let image = Err(match &binding {
        chart_image::Binding::Unresolved { why, .. } => Absent::Unresolved(*why),
        _ => Absent::NoHeader,
    });
    Arc::new(Read { binding, image })
}

/// Whether `read` may be carried past the job that made it (kept for a
/// later generation, or with a layout): what it holds is the file's own
/// (an image, or the absence its observation gives), never a job's
/// failure (panicked, could not start) or cancellation. The one test every
/// reuse of a source asks.
fn reusable(read: &Read) -> bool {
    !matches!(read.image, Err(Absent::Cancelled | Absent::Failed))
}

/// Whether `fit`, derived from `source`, may be reused: it is an image,
/// or `source`'s own absence. A fit absent while its source is an image is
/// its derivation's failure (cancelled, failed, or its allocation
/// refused), which a later derivation may not repeat. The one test every
/// reuse of a fit asks.
fn fit_reusable(fit: &Read, source: &Read) -> bool {
    fit.image.is_ok() || (source.image.is_err() && reusable(fit))
}

impl ChartImages {
    /// Whether every fit of this set, derived from `sources`, may be
    /// reused ([`fit_reusable`]).
    #[must_use]
    pub fn reusable_from(&self, sources: &Sources) -> bool {
        ChartImage::ALL
            .into_iter()
            .all(|w| fit_reusable(self.read(w), sources.read(w)))
    }
}

/// A half: the fits of a generation's sources for chart layers of one
/// side, and the sources they were derived from.
#[derive(Debug)]
struct Half {
    side: u32,
    generation: u64,
    from: Arc<Sources>,
    fits: Arc<ChartImages>,
}

/// Derives `sources`' fits for chart layers of side `side`: each image's
/// read is kept from `prior` while its source is the same read and its
/// side the same, shared with `other` when its slot derives as that fit
/// ([`chart_image::Slot::derives_as`]), else derived anew. An absent
/// source stays absent; a side with no slot has the images absent.
fn derive_half(
    sources: &Sources,
    side: u32,
    prior: Option<&Half>,
    other: Option<&ChartImages>,
    cancel: &dyn Fn() -> bool,
) -> ChartImages {
    let reads = ChartImage::ALL.map(|which| {
        let i = which.index();
        let source = &sources.reads[i];
        if let Some(p) = prior
            && p.side == side
            && Arc::ptr_eq(&p.from.reads[i], source)
            && fit_reusable(&p.fits.reads[i], source)
        {
            return Arc::clone(&p.fits.reads[i]);
        }
        let Some((_, slot)) = which.slot(side) else {
            return Arc::new(Read {
                binding: chart_image::Binding::NoHeader,
                image: Err(Absent::NoHeader),
            });
        };
        let Ok(image) = &source.image else {
            return Arc::clone(source);
        };
        if let Some(o) = other
            && fit_reusable(o.read(which), source)
            && o.read(which)
                .image
                .as_ref()
                .is_ok_and(|f| slot.derives_as(f))
            && fitted_from(o.read(which), source)
        {
            return Arc::clone(&o.reads[i]);
        }
        #[cfg(test)]
        count([0, 1]);
        Arc::new(Read {
            binding: source.binding.clone(),
            image: chart_image::derive(image, slot, cancel),
        })
    });
    ChartImages::of(reads)
}

/// `sources`' fits for chart layers of side `side`, each derived anew
/// (a result layout's own chart slot), kept with that layout: `None` when a
/// derivation failed, so no failed fit is ever kept ([`fit_reusable`]).
#[must_use]
pub fn derive_set(sources: &Sources, side: u32, cancel: &dyn Fn() -> bool) -> Option<ChartImages> {
    Some(derive_half(sources, side, None, None, cancel)).filter(|d| d.reusable_from(sources))
}

/// Whether `fit` was derived from `source`: the same observation and
/// native size.
fn fitted_from(fit: &Read, source: &Read) -> bool {
    fit.binding == source.binding
        && match (&fit.image, &source.image) {
            (Ok(a), Ok(b)) => a.native() == b.native(),
            _ => false,
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
    /// One just sent, of `generation` and `request`, its answer to come on
    /// `rx`.
    const fn sent(
        generation: u64,
        request: Option<u64>,
        set: Option<Arc<ChartImages>>,
        rx: mpsc::Receiver<Ack>,
        by_job: bool,
    ) -> Self {
        Self {
            generation,
            request,
            set,
            rx: Some(rx),
            ack: None,
            by_job,
            unsent: false,
            gone: false,
        }
    }

    /// Whether it is the job's own publication of `generation`, its set
    /// not yet taken from the job.
    fn awaits_job(&self, generation: u64) -> bool {
        self.by_job && self.generation == generation && self.set.is_none()
    }

    /// The job that was to send it ended, having sent `sent`, or nothing
    /// (then the set is published again).
    fn job_ended(&mut self, sent: Option<&Arc<ChartImages>>) {
        match sent {
            Some(set) => self.set = Some(Arc::clone(set)),
            None => self.unsent = true,
        }
    }

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

/// What the cursor's request names (design.md "Rendering and Skins",
/// Chart images): the chart's folder, its stage file and banner names as
/// the library keeps them, and the page side of the selection
/// installation's chart layer. A rest compares it with the last selection
/// set's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorId {
    pub dir: PathBuf,
    /// The stage file's, then the banner's.
    pub names: [library::Name; 2],
    pub side: u32,
}

/// The cursor's one image request (S10-12 plan D2, I5): made once the
/// cursor rested on a chart row, moved into the worker
/// ([`ChartImageWorker::rest`]), and moved into the job that reads it or
/// dropped. Its subscriber to the cursor's scope is taken when it is made
/// ([`ChartImageWorker::cursor_request`]), so a request its scope outlived
/// is cancelled from its start.
#[must_use]
#[derive(Debug)]
pub struct CursorRequest {
    id: CursorId,
    cursor_generation: u64,
    subscriber: game::coord::Subscriber,
}

/// The last selection set (I7): what it was read for, for which cursor
/// generation it is current, and the set. Also the only candidates a
/// later request's read keeps reads from (D3).
#[derive(Debug)]
struct SelectionSet {
    id: CursorId,
    cursor_generation: u64,
    set: Arc<ChartImages>,
}

/// Reads `id`'s images for the selection installation's chart slots: the
/// stage file and the banner, each decoded and derived for its slot of
/// `id.side`, the sources dropped; the back image is never read there.
/// Each name is resolved again, and `prior`'s read of an unchanged file is
/// kept (D3). An overlong name resolves to nothing, never to a shortened
/// one. Asks `cancel` throughout.
fn read_selection(
    id: &CursorId,
    prior: Option<&ChartImages>,
    cancel: &dyn Fn() -> bool,
) -> ChartImages {
    let dir = match ChartDir::new(&id.dir) {
        Ok(dir) => dir,
        Err(e) => {
            return ChartImages::absent(&Absent::Unresolved(chart_image::Unresolved::Io(e.kind())));
        }
    };
    #[cfg(test)]
    hold(cancel);
    let identity: chart_image::Identity<'_> = &file_identity;
    let [stage, banner] = &id.names;
    let reads = ChartImage::ALL.map(|which| {
        let name = match which {
            ChartImage::Stage => stage,
            ChartImage::Banner => banner,
            ChartImage::Back => return absent_read(chart_image::Binding::NoHeader),
        };
        let (binding, name) = match name {
            library::Name::None => (chart_image::Binding::NoHeader, ""),
            library::Name::Overlong => (
                chart_image::Binding::Unresolved {
                    name: String::new(),
                    why: chart_image::Unresolved::Overlong,
                },
                "",
            ),
            library::Name::Named(name) => (
                chart_image::resolve(&dir, name, &[], identity),
                name.as_str(),
            ),
        };
        if let Some(prior) = prior {
            let kept = prior.read(which);
            if binding.same_as(&kept.binding) && reusable(kept) {
                return Arc::clone(&prior.reads[which.index()]);
            }
        }
        let chart_image::Binding::Resolved { .. } = binding else {
            return absent_read(binding);
        };
        let Some((_, slot)) = which.slot(id.side) else {
            return absent_read(chart_image::Binding::NoHeader);
        };
        #[cfg(test)]
        count([1, 0]);
        let source = chart_image::read_source(&dir, name, &[], cancel, identity);
        let image = match &source.image {
            Ok(image) => {
                #[cfg(test)]
                count([0, 1]);
                chart_image::derive(image, slot, cancel)
            }
            Err(why) => Err(why.clone()),
        };
        Arc::new(Read {
            binding: source.binding,
            image,
        })
    });
    ChartImages::of(reads)
}

/// Holds a read while [`HOLD`] is set and `cancel` does not answer, and
/// while [`HOLD_HARD`] is set whatever it answers (tests).
#[cfg(test)]
fn hold(cancel: &dyn Fn() -> bool) {
    while HOLD.load(std::sync::atomic::Ordering::Acquire) && !cancel() {
        std::thread::sleep(Duration::from_millis(1));
    }
    while HOLD_HARD.load(std::sync::atomic::Ordering::Acquire) {
        HELD_HARD.store(true, std::sync::atomic::Ordering::Release);
        std::thread::sleep(Duration::from_millis(1));
    }
    HELD_HARD.store(false, std::sync::atomic::Ordering::Release);
}

/// What a job gives: an attempt's job its [`JobOut`], the cursor's job
/// its selection set.
#[derive(Debug)]
enum Output {
    Attempt(JobOut),
    Cursor {
        set: Arc<ChartImages>,
        /// Tests: the files it decoded and the fits it derived.
        #[cfg(test)]
        counts: (u64, u64),
    },
}

/// What a job gives: its sources, and the halves it was asked for.
#[derive(Clone, Debug)]
struct JobOut {
    sources: Arc<Sources>,
    /// Whether `sources` were decoded by this job; otherwise they are the
    /// current ones, which adoption keeps.
    fresh: bool,
    play: Option<Arc<ChartImages>>,
    result: Option<Arc<ChartImages>>,
    /// Tests: the files it decoded and the fits it derived.
    #[cfg(test)]
    counts: (u64, u64),
}

/// A job of the worker, its thread not yet joined, and what it is for.
struct Job {
    target: Target,
    job: OwnedJob<Arc<Output>>,
}

/// What a job reads for.
#[derive(Debug)]
enum Target {
    /// The attempt's want.
    Attempt {
        want: Want,
        generation: u64,
        /// Whether it decodes the sources (they were not current when it
        /// began), and which halves it derives: the play half (which it
        /// publishes) and the result half.
        fresh: bool,
        derive: [bool; 2],
    },
    /// The cursor's request, for its cursor generation (I6).
    Cursor {
        id: CursorId,
        cursor_generation: u64,
    },
}

/// What a job that failed (panicked, or could not start) leaves: the
/// images it was to make absent as failed, and nothing else. Sources it
/// did not decode stay as they are ([`Core::adopt`] keeps them), so a
/// derivation's failure never turns availability resolved before it.
fn failed_out(want: &Want, fresh: bool, [play, result]: [bool; 2]) -> JobOut {
    let absent = |side: Option<u32>, made: bool| {
        side.filter(|_| made)
            .map(|_| Arc::new(ChartImages::absent(&Absent::Failed)))
    };
    JobOut {
        sources: Arc::new(ChartImages::absent(&Absent::Failed)),
        fresh,
        play: absent(want.play, play),
        result: absent(want.result, result),
        #[cfg(test)]
        counts: (0, 0),
    }
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
    /// The job reading `want`'s images, or the cursor's: one at a time.
    job: Option<Job>,
    /// A job of an earlier want or generation, its thread not yet joined.
    obsolete: Option<OwnedJob<Arc<Output>>>,
    /// The selection's cursor generation, which the render thread reads
    /// ([`crate::render_side::RenderLink::cursor_generation`]): a new one
    /// at each change of the cursor's request.
    cursor_generation: Arc<AtomicU64>,
    /// The cursor's request, deferred until nothing holds the worker (I5).
    cursor: Option<CursorRequest>,
    /// The last selection set (I7).
    selection: Option<SelectionSet>,
    /// The selection's publication (I8); [`Core::retiring`] is shared
    /// with play's.
    sel_publication: Option<Publication>,
    /// The starter is the selection's ([`ChartImageWorker::cursor_step`]),
    /// which leaving selection drops.
    starter_cursor: bool,
    /// The sources last decoded, of which files, in which generation:
    /// current only for `want`'s files in the current generation (the
    /// facts are then resolved), else kept as the candidates a
    /// re-resolution reuses.
    sources: Option<(Want, u64, Arc<Sources>)>,
    /// The play half and the result half last derived.
    play: Option<Half>,
    result: Option<Half>,
    /// The sources were drawn by a committed attempt: the next attempt of
    /// the same files re-resolves them first.
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
    /// Since when a job the want needs could not begin for a publication
    /// unanswered: the facts' wait is bounded from there.
    blocked: Option<Instant>,
    /// A job the last step could not begin yet (a job's thread still
    /// alive, or a publication unanswered): the waits' service begins it.
    starter: Option<Starter>,
    /// The run ended ([`ChartImageWorker::end`]): nothing begins again.
    ended: bool,
    /// Jobs begun (tests count jobs by it).
    begun: u64,
    /// Tests: the selection publications sent, by the game thread or a
    /// job.
    #[cfg(test)]
    sel_sent: u64,
    /// Tests: the files its jobs decoded and the fits they derived.
    #[cfg(test)]
    counts: (u64, u64),
    /// Tests: the jobs `end` settled, and those it left to the process.
    #[cfg(test)]
    settled: u64,
    #[cfg(test)]
    abandoned: u64,
}

/// The run's one owner of the chart-image worker (module doc): a handle,
/// cloned for the loading waits' service, to one shared state.
#[derive(Clone)]
pub struct ChartImageWorker {
    core: Arc<Mutex<Core>>,
}

impl Default for ChartImageWorker {
    fn default() -> Self {
        Self::new(Arc::default(), Arc::default())
    }
}

impl std::fmt::Debug for ChartImageWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let c = self.core();
        f.debug_struct("ChartImageWorker")
            .field("want", &c.want)
            .field("generation", &c.generation())
            .field("job", &c.job.as_ref().map(|j| &j.target))
            .field("obsolete", &c.obsolete.is_some())
            .field("cursor", &c.cursor)
            .field(
                "selection",
                &c.selection.as_ref().map(|s| (&s.id, s.cursor_generation)),
            )
            .field("sources", &c.sources.as_ref().map(|(w, g, _)| (w, g)))
            .field("retiring", &c.retiring.is_some())
            .finish_non_exhaustive()
    }
}

/// What the attempt's result takes at its commit
/// ([`ChartImageWorker::take_result`]): the generation's sources, and its
/// result half, if one was asked for.
#[derive(Debug)]
pub struct ResultImages {
    pub sources: Arc<Sources>,
    pub half: Option<Arc<ChartImages>>,
    /// The chart layer side the half is derived for.
    pub side: Option<u32>,
}

impl ChartImageWorker {
    /// A worker writing its generation to `generation` and the cursor
    /// generation to `cursor_generation`, which the render thread reads
    /// ([`crate::render_side::RenderLink::chart_generation`],
    /// [`crate::render_side::RenderLink::cursor_generation`]).
    #[must_use]
    pub fn new(generation: Arc<AtomicU64>, cursor_generation: Arc<AtomicU64>) -> Self {
        Self {
            core: Arc::new(Mutex::new(Core {
                want: None,
                generation,
                job: None,
                obsolete: None,
                cursor_generation,
                cursor: None,
                selection: None,
                sel_publication: None,
                starter_cursor: false,
                sources: None,
                play: None,
                result: None,
                used: false,
                publication: None,
                retiring: None,
                deadline: None,
                blocked: None,
                starter: None,
                ended: false,
                begun: 0,
                #[cfg(test)]
                sel_sent: 0,
                #[cfg(test)]
                counts: (0, 0),
                #[cfg(test)]
                settled: 0,
                #[cfg(test)]
                abandoned: 0,
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

    /// Jobs begun (tests).
    #[cfg(test)]
    #[must_use]
    pub fn begun(&self) -> u64 {
        self.core().begun
    }

    /// A new generation: a choice, a practice Start, a retry's
    /// re-resolution or a chart left. Written at once where the render
    /// thread reads it, so no table or acknowledgment of the last one is
    /// current from here, and no frame drawn after a loading lifetime
    /// begun after this draws one; the facts are unresolved until this
    /// generation's sources. What the last generation read stays, only as
    /// the candidates the next read reuses; its job, if running, is left
    /// to end, and its output is never taken; its publication, if
    /// unanswered, retires.
    pub fn renew(&self) {
        self.core().renew();
    }

    /// Asks for `want`'s images for the attempt being prepared: the one
    /// function that sets what is asked (at the choice, on entering the
    /// attempt's facts stage, and with both sides known). Each part is
    /// compared on its own: a want that changes only a side keeps the
    /// sources, the other half and its publication, and the next job only
    /// derives. A changed play part drops the last publication (retiring
    /// it while unanswered), which is published again for it. The same
    /// files after an attempt drew them (a retry or a practice round) are a
    /// new generation ([`renew`](Self::renew)): they are re-resolved, what
    /// did not change kept, and published again (design C11, P22).
    pub fn attempt(&self, want: Want) {
        let mut c = self.core();
        if c.ended {
            return;
        }
        if c.used && c.sources_current_for(&want) {
            c.renew();
        }
        let same_play = c
            .want
            .as_ref()
            .is_some_and(|w| w.same_files(&want) && w.play == want.play);
        if !same_play {
            c.supersede();
        }
        c.want = Some(want);
    }

    /// The current sources were drawn by an attempt that committed.
    pub fn committed(&self) {
        let mut c = self.core();
        c.used = c.sources_current();
    }

    /// The chart is left: a new generation with nothing asked for. Its
    /// job, if running, was cancelled through the choice's scope and is
    /// kept until its thread ends.
    pub fn forget(&self) {
        let mut c = self.core();
        c.renew();
        c.want = None;
        c.starter = None;
        c.starter_cursor = false;
        if let Some(job) = c.job.take() {
            c.retire(job.job);
        }
    }

    /// Selection is entered: the sources and halves go, so nothing of the
    /// last chart's images stays beside selection's, and so does the last
    /// selection set, so a request after the entry reads its files again
    /// (ST16); its publication, if unanswered, retires.
    pub fn drop_images(&self) {
        let mut c = self.core();
        c.sources = None;
        c.play = None;
        c.result = None;
        c.selection = None;
        c.supersede_selection();
    }

    /// The cursor's request for `dir`'s images `names` (the stage file's,
    /// then the banner's) on a selection chart layer of side `side`: for
    /// the current cursor generation, its subscriber to `coord`'s cursor
    /// scope taken now (D2).
    pub fn cursor_request(
        &self,
        dir: PathBuf,
        names: [library::Name; 2],
        side: u32,
        coord: &Coordinator,
    ) -> CursorRequest {
        let c = self.core();
        CursorRequest {
            id: CursorId { dir, names, side },
            cursor_generation: c.cursor_generation(),
            subscriber: coord.subscriber(Scope::Cursor),
        }
    }

    /// The cursor's request identity changed (SC4, D2): the one function
    /// every change goes through. A new cursor generation, written where
    /// the render thread reads it, so no selection table of the last one
    /// draws from the next frame; the deferred request goes; the cursor's
    /// scope advances, cancelling a running cursor job, whose output is
    /// never adopted (it stays the worker's until its thread ends); the
    /// selection publication is superseded (unanswered, it retires).
    pub fn cursor_changed(&self, coord: &Coordinator) {
        self.core().cursor_changed(coord);
    }

    /// The cursor rested (SC1): `request` is the one deferred, replacing
    /// any; the next [`cursor_step`](Self::cursor_step) or the waits'
    /// service begins it once nothing holds the worker.
    pub fn rest(&self, request: CursorRequest) {
        let mut c = self.core();
        if !c.ended {
            c.cursor = Some(request);
        }
    }

    /// Selection is left (D2, SC5), before the play generation renews:
    /// what [`cursor_changed`](Self::cursor_changed) does, and the
    /// selection's starter and set go, so nothing of selection begins or
    /// is published after it. An unanswered selection publication retires,
    /// and no job begins until it is answered.
    pub fn leave_selection(&self, coord: &Coordinator) {
        let mut c = self.core();
        c.cursor_changed(coord);
        if c.starter_cursor {
            c.starter = None;
            c.starter_cursor = false;
        }
        c.selection = None;
    }

    /// Advances the selection's images without blocking (D2): takes what
    /// ended, begins the deferred request once nothing holds the worker
    /// (or publishes without a job what needs none), and publishes the
    /// current selection set again when its last answer is not current
    /// (a screen built again, SC6). Never begins anything after
    /// [`end`](Self::end).
    pub fn cursor_step(
        &self,
        coord: &Coordinator,
        spawner: Spawner,
        publisher: &crate::render_side::ChartPublisher,
    ) {
        let mut c = self.core();
        c.take_finished();
        if c.ended {
            return;
        }
        c.starter = Some((coord.clone(), spawner, Some(publisher.clone())));
        c.starter_cursor = true;
        c.begin_if_free();
        c.republish_selection(publisher);
    }

    /// Whether a deferred cursor request waits for a job's thread to end
    /// or a publication to be answered: the select screen then looks
    /// again within a slice ([`crate::play::IMAGE_SLICE`]).
    #[must_use]
    pub fn cursor_waiting(&self) -> bool {
        let c = self.core();
        c.cursor.is_some() && c.held_back()
    }

    /// Whether a selection set is kept, current or not (tests).
    #[cfg(test)]
    #[must_use]
    pub fn selection_kept(&self) -> bool {
        self.core().selection.is_some()
    }

    /// The selection set, while current for the cursor generation (tests).
    #[cfg(test)]
    #[must_use]
    pub fn current_selection(&self) -> Option<Arc<ChartImages>> {
        let c = self.core();
        let now = c.cursor_generation();
        c.selection
            .as_ref()
            .filter(|s| s.cursor_generation == now)
            .map(|s| Arc::clone(&s.set))
    }

    /// The cursor generation (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    #[must_use]
    pub fn cursor_generation(&self) -> u64 {
        self.core().cursor_generation()
    }

    /// Whether a publication retires (tests).
    #[cfg(test)]
    #[must_use]
    pub fn retiring(&self) -> bool {
        self.core().retiring.is_some()
    }

    /// Whether the selection publication is unanswered (tests).
    #[cfg(test)]
    #[must_use]
    pub fn selection_pending(&self) -> bool {
        self.core().sel_pending()
    }

    /// Whether a cursor request is deferred (tests).
    #[cfg(test)]
    #[must_use]
    pub fn cursor_deferred(&self) -> bool {
        self.core().cursor.is_some()
    }

    /// The selection publications sent (tests).
    #[cfg(test)]
    #[must_use]
    pub fn selection_publications(&self) -> u64 {
        self.core().sel_sent
    }

    /// The result begins: the play half goes (the play installation is not
    /// drawn); a retry derives it again from the sources, decoding
    /// nothing. Its publication goes once answered.
    pub fn result_began(&self) {
        let mut c = self.core();
        c.play = None;
        c.poll_answers();
        if c.publication.as_ref().is_some_and(|p| !p.pending()) {
            c.publication = None;
        }
    }

    /// The facts of the current generation: `None` until its sources are
    /// adopted, then whether each is an image.
    #[must_use]
    pub fn facts(&self) -> Option<ImageFacts> {
        let c = self.core();
        c.sources_current()
            .then(|| c.sources.as_ref().map(|(_, _, s)| s.facts()))?
    }

    /// How long a job the want needs has been held back by a publication
    /// unanswered, if it is.
    #[must_use]
    pub fn blocked_for(&self) -> Option<Duration> {
        self.core().blocked.map(|at| at.elapsed())
    }

    /// What the attempt's result takes at its commit: the current sources
    /// (kept here as the candidates of the next generation) and the result
    /// half, moved out. `None` before the sources are adopted.
    #[must_use]
    pub fn take_result(&self) -> Option<ResultImages> {
        let mut c = self.core();
        if !c.sources_current() {
            return None;
        }
        let sources = c.sources.as_ref().map(|(_, _, s)| Arc::clone(s))?;
        let side = c.want.as_ref().and_then(|w| w.result);
        let half = if c.result_current() {
            c.result.take().map(|h| h.fits)
        } else {
            None
        };
        Some(ResultImages {
            sources,
            half,
            side,
        })
    }

    /// Advances the worker without blocking: takes what ended, and begins
    /// the job `want` needs once no job's thread is alive and no
    /// publication is unanswered, else leaves it to the loading waits'
    /// [`service`](Self::service). `coord` `None` (the direct launch) reads
    /// at once on this thread, asking `cancel`. With `publish`, the skin in
    /// use's loading frames get what is terminal at once: the job publishes
    /// its play half as soon as it is derived, and a target with no chart
    /// layer, or a job that cannot start, publishes the images absent now.
    pub fn step(
        &self,
        coord: Option<(&Coordinator, Spawner)>,
        cancel: &dyn Fn() -> bool,
        publish: Option<&crate::render_side::ChartPublisher>,
    ) {
        let mut c = self.core();
        c.take_finished();
        if c.ended {
            return;
        }
        let Some(want) = c.want.clone() else {
            return;
        };
        let generation = c.generation();
        let Some((coord, spawner)) = coord else {
            if c.job.is_some() || !c.needs_job() {
                return;
            }
            c.begun += 1;
            let out = c.run(&want, cancel);
            #[cfg(test)]
            {
                c.counts.0 += out.counts.0;
                c.counts.1 += out.counts.1;
            }
            // A read stopped part way is never this generation's.
            if !cancel() {
                c.adopt(&want, generation, out);
            }
            return;
        };
        c.starter = Some((coord.clone(), spawner, publish.cloned()));
        c.starter_cursor = false;
        c.begin_if_free();
    }

    /// The loading waits' service ([`crate::controls::Controls::look`]):
    /// what [`step`](Self::step) left, nonblocking, from whichever thread
    /// waits. Takes a finished job's output and answers, and begins a job
    /// the last step left once nothing holds it back. Nothing when the
    /// game thread holds the worker.
    pub fn service(&self) {
        let Ok(mut c) = self.core.try_lock() else {
            return;
        };
        c.take_finished();
        c.begin_if_free();
    }

    /// The play half `want` asked for, once derived in the current
    /// generation (tests).
    #[cfg(test)]
    #[must_use]
    pub fn current(&self) -> Option<Arc<ChartImages>> {
        self.core().play_current().cloned()
    }

    /// The result half, once derived in the current generation (tests).
    #[cfg(test)]
    #[must_use]
    pub fn current_result(&self) -> Option<Arc<ChartImages>> {
        let c = self.core();
        c.result_current()
            .then(|| c.result.as_ref().map(|h| Arc::clone(&h.fits)))?
    }

    /// The current sources (tests).
    #[cfg(test)]
    #[must_use]
    pub fn sources(&self) -> Option<Arc<Sources>> {
        let c = self.core();
        c.sources_current()
            .then(|| c.sources.as_ref().map(|(_, _, s)| Arc::clone(s)))?
    }

    /// The jobs `end` settled, and those it left to the process (tests).
    #[cfg(test)]
    #[must_use]
    pub fn ended_jobs(&self) -> (u64, u64) {
        let c = self.core();
        (c.settled, c.abandoned)
    }

    /// The files its jobs decoded and the fits they derived (tests).
    #[cfg(test)]
    #[must_use]
    pub fn counts(&self) -> (u64, u64) {
        self.core().counts
    }

    /// The bytes it keeps (tests).
    #[cfg(test)]
    #[must_use]
    pub fn retained(&self) -> u64 {
        self.core().retained_bytes()
    }

    /// Publishes the current play half (or absent, for a target with no
    /// chart layer) for the attempt of `request` once everything asked for
    /// is read, and takes its answer: [`Publish::Answered`] once the
    /// publication for that generation and request has returned and is
    /// still current ([`PublicationKey::current`]); one that is not, or
    /// one its job never sent, is asked again, once no other publication
    /// is unanswered. [`Publish::Expired`] when no current answer came
    /// within the render link's bound since the first publication for the
    /// generation and request.
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
        let play = want.play;
        if c.needs_job() {
            return Publish::Waiting;
        }
        let set = match play {
            Some(_) => match c.play_current() {
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
            if c.retiring.is_none() && !c.sel_pending() {
                let rx = render.publish_chart_images(generation, request, set.clone());
                c.publication = Some(Publication::sent(
                    generation,
                    request,
                    set.clone(),
                    rx,
                    false,
                ));
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

    /// At the run's end: nothing begins from here (no want, step, service
    /// or admission begins a job); what it keeps goes, and every job is
    /// stopped and joined within [`STOP_WAIT`], or left to the process's
    /// exit, owning what it holds (design P17); a panic is logged, never
    /// the run's.
    pub fn end(&self) {
        let mut c = self.core();
        c.ended = true;
        c.want = None;
        c.publication = None;
        c.retiring = None;
        c.starter = None;
        c.starter_cursor = false;
        c.sources = None;
        c.play = None;
        c.result = None;
        c.blocked = None;
        c.cursor = None;
        c.selection = None;
        c.sel_publication = None;
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
                    #[cfg(test)]
                    {
                        c.abandoned += 1;
                    }
                }
            }
            #[cfg(test)]
            {
                c.settled += 1;
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

/// The job's work: `want`'s sources decoded (keeping `prior`'s unchanged
/// reads) unless `prior` is current (`fresh` false), then each half
/// `derive` asks for derived. Asks `cancel` throughout.
fn run_job(
    want: &Want,
    prior: Option<&Arc<Sources>>,
    fresh: bool,
    halves: [Option<&Half>; 2],
    derive: [bool; 2],
    cancel: &dyn Fn() -> bool,
) -> JobOut {
    #[cfg(test)]
    let before = COUNTS.with(std::cell::Cell::get);
    // A job's injected panic (tests), a decoding or a deriving one alike.
    #[cfg(test)]
    assert!(
        !PANIC.swap(false, std::sync::atomic::Ordering::AcqRel),
        "an injected panic of the images' job"
    );
    let sources = match prior {
        Some(p) if !fresh => Arc::clone(p),
        _ => Arc::new(read_sources(want, prior.map(AsRef::as_ref), cancel)),
    };
    let [play_prior, result_prior] = halves;
    let [derive_play, derive_result] = derive;
    let play = want
        .play
        .filter(|_| derive_play)
        .map(|side| Arc::new(derive_half(&sources, side, play_prior, None, cancel)));
    let other = play.as_deref().or(play_prior.map(|h| h.fits.as_ref()));
    let result = want
        .result
        .filter(|_| derive_result)
        .map(|side| Arc::new(derive_half(&sources, side, result_prior, other, cancel)));
    JobOut {
        sources,
        fresh,
        play,
        result,
        #[cfg(test)]
        counts: COUNTS.with(|c| {
            let (d, f) = c.get();
            (d - before.0, f - before.1)
        }),
    }
}

/// The cursor job's work: `id`'s selection set ([`read_selection`]),
/// keeping `prior`'s unchanged reads. Asks `cancel` throughout.
fn run_cursor_job(id: &CursorId, prior: Option<&ChartImages>, cancel: &dyn Fn() -> bool) -> Output {
    #[cfg(test)]
    let before = COUNTS.with(std::cell::Cell::get);
    #[cfg(test)]
    assert!(
        !PANIC.swap(false, std::sync::atomic::Ordering::AcqRel),
        "an injected panic of the images' job"
    );
    let set = Arc::new(read_selection(id, prior, cancel));
    Output::Cursor {
        set,
        #[cfg(test)]
        counts: COUNTS.with(|c| {
            let (d, f) = c.get();
            (d - before.0, f - before.1)
        }),
    }
}

impl Core {
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn cursor_generation(&self) -> u64 {
        self.cursor_generation.load(Ordering::SeqCst)
    }

    /// [`ChartImageWorker::cursor_changed`] under the held worker.
    fn cursor_changed(&mut self, coord: &Coordinator) {
        self.cursor_generation.fetch_add(1, Ordering::SeqCst);
        self.cursor = None;
        coord.advance(Scope::Cursor);
        self.supersede_selection();
    }

    /// The selection publication is no longer the one asked for, by the
    /// rule of [`supersede`](Self::supersede).
    fn supersede_selection(&mut self) {
        let p = self.sel_publication.take();
        self.retire_publication(p);
    }

    /// Whether play's publication is unanswered.
    fn play_pending(&self) -> bool {
        self.publication.as_ref().is_some_and(Publication::pending)
    }

    /// Whether the selection publication is unanswered.
    fn sel_pending(&self) -> bool {
        self.sel_publication
            .as_ref()
            .is_some_and(Publication::pending)
    }

    /// Whether a job may not begin now: a job's thread alive, a
    /// publication retiring, or any target's unanswered (SX20).
    fn held_back(&self) -> bool {
        self.job.is_some()
            || self.obsolete.is_some()
            || self.retiring.is_some()
            || self.play_pending()
            || self.sel_pending()
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
        let p = self.publication.take();
        self.retire_publication(p);
    }

    /// A superseded publication `p`: answered, it goes; unanswered, it
    /// retires unless one already does.
    fn retire_publication(&mut self, p: Option<Publication>) {
        if let Some(mut p) = p {
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
        if let Some(p) = &mut self.sel_publication {
            p.poll();
        }
    }

    fn retire(&mut self, job: OwnedJob<Arc<Output>>) {
        if let Some(old) = self.obsolete.take() {
            // Two ended jobs at once: the older is settled now; its
            // thread has ended or ends promptly once cancelled.
            settle_logged(old);
        }
        self.obsolete = Some(job);
    }

    /// Whether the sources are current for `want`'s files.
    fn sources_current_for(&self, want: &Want) -> bool {
        self.sources
            .as_ref()
            .is_some_and(|(w, g, _)| w.same_files(want) && *g == self.generation())
    }

    fn sources_current(&self) -> bool {
        self.want
            .as_ref()
            .is_some_and(|w| self.sources_current_for(w))
    }

    /// The half `half` when current for side `side` of the current
    /// generation's sources.
    fn half_current<'a>(&'a self, half: Option<&'a Half>, side: Option<u32>) -> Option<&'a Half> {
        let side = side?;
        let sources = self.sources.as_ref().map(|(_, _, s)| s)?;
        half.filter(|h| {
            self.sources_current()
                && h.side == side
                && h.generation == self.generation()
                && Arc::ptr_eq(&h.from, sources)
        })
    }

    fn play_current(&self) -> Option<&Arc<ChartImages>> {
        let side = self.want.as_ref()?.play;
        self.half_current(self.play.as_ref(), side).map(|h| &h.fits)
    }

    fn result_current(&self) -> bool {
        let side = self.want.as_ref().and_then(|w| w.result);
        self.half_current(self.result.as_ref(), side).is_some()
    }

    /// Whether the want needs a job: its sources not current, or a half it
    /// asks for not derived from them.
    fn needs_job(&self) -> bool {
        let Some(want) = &self.want else {
            return false;
        };
        !self.sources_current()
            || (want.play.is_some() && self.play_current().is_none())
            || (want.result.is_some() && !self.result_current())
    }

    /// The prior sources a job re-resolves against, and the halves it
    /// keeps reads of.
    fn priors(&self) -> (Option<Arc<Sources>>, Option<Half>, Option<Half>) {
        let copy = |h: &Half| Half {
            side: h.side,
            generation: h.generation,
            from: Arc::clone(&h.from),
            fits: Arc::clone(&h.fits),
        };
        (
            self.sources.as_ref().map(|(_, _, s)| Arc::clone(s)),
            self.play.as_ref().map(copy),
            self.result.as_ref().map(copy),
        )
    }

    /// What a job for the want does: whether it decodes the sources, and
    /// which halves it derives.
    fn work(&self) -> (bool, [bool; 2]) {
        let want = self.want.as_ref();
        (
            !self.sources_current(),
            [
                want.is_some_and(|w| w.play.is_some()) && self.play_current().is_none(),
                want.is_some_and(|w| w.result.is_some()) && !self.result_current(),
            ],
        )
    }

    /// The job's work on this thread (the direct launch): the sources,
    /// and the halves.
    fn run(&self, want: &Want, cancel: &dyn Fn() -> bool) -> JobOut {
        let (prior, play, result) = self.priors();
        let (fresh, derive) = self.work();
        run_job(
            want,
            prior.as_ref(),
            fresh,
            [play.as_ref(), result.as_ref()],
            derive,
            cancel,
        )
    }

    /// Adopts `out`, a job's output for `want` begun in `generation`: each
    /// part only while it is still the one asked for in the current
    /// generation. The facts resolve with the sources.
    fn adopt(&mut self, want: &Want, generation: u64, out: JobOut) {
        let Some(now) = self.want.clone() else {
            return;
        };
        if generation != self.generation() || !now.same_files(want) {
            return;
        }
        // Sources the job did not decode are the current ones: kept, so
        // only a decode replaces them (a derivation's failure fails only
        // its halves).
        let sources = match (&self.sources, out.fresh) {
            (Some((_, g, s)), false) if *g == generation => Arc::clone(s),
            _ => {
                log_absent(&out.sources);
                let s = Arc::clone(&out.sources);
                self.sources = Some((want.clone(), generation, out.sources));
                s
            }
        };
        let half = |side: Option<u32>, fits: Option<Arc<ChartImages>>, asked: Option<u32>| match (
            side, fits,
        ) {
            (Some(side), Some(fits)) if asked == Some(side) => Some(Half {
                side,
                generation,
                from: Arc::clone(&sources),
                fits,
            }),
            _ => None,
        };
        if let Some(h) = half(want.play, out.play, now.play) {
            self.play = Some(h);
        }
        if let Some(h) = half(want.result, out.result, now.result) {
            self.result = Some(h);
        }
        log::info!(
            "chart images: read; {} bytes retained",
            self.retained_bytes()
        );
    }

    /// Takes what ended: the answers, an obsolete job joined, and a
    /// finished job's output when it is of the current generation and
    /// files (or the current cursor generation) and was not cancelled.
    /// Never blocks, never begins a job.
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
        let outcome = job.job.poll().map(|o| o.outcome);
        settle_logged(job.job);
        match job.target {
            Target::Attempt {
                want,
                generation,
                fresh,
                derive,
            } => self.attempt_finished(&want, generation, (fresh, derive), outcome),
            Target::Cursor {
                id,
                cursor_generation,
            } => self.cursor_finished(id, cursor_generation, outcome),
        }
    }

    /// An attempt's job ended with `outcome`: its output adopted while it
    /// is still the one asked for.
    fn attempt_finished(
        &mut self,
        want: &Want,
        generation: u64,
        (fresh, derive): (bool, [bool; 2]),
        outcome: Option<Outcome<Arc<Output>>>,
    ) {
        let (out, sent) = match outcome {
            Some(Outcome::Done(out)) => match &*out {
                Output::Attempt(out) => (Some(out.clone()), true),
                Output::Cursor { .. } => (None, false),
            },
            Some(Outcome::Panicked(p)) => {
                log::warn!("chart images: the worker stopped unexpectedly: {}", p.0);
                (Some(failed_out(want, fresh, derive)), false)
            }
            // A cancelled read is never terminal: the next step reads again
            // what is still asked for.
            Some(Outcome::Cancelled) | None => (None, false),
        };
        let Some(out) = out else {
            return;
        };
        #[cfg(test)]
        {
            self.counts.0 += out.counts.0;
            self.counts.1 += out.counts.1;
        }
        let current = generation == self.generation()
            && self.want.as_ref().is_some_and(|w| w.same_files(want));
        if current
            && matches!(derive, [true, _])
            && self.want.as_ref().is_some_and(|w| w.play == want.play)
            && let Some(p) = self
                .publication
                .as_mut()
                .filter(|p| p.awaits_job(generation))
        {
            // The job's own publication is of its play half when the job
            // sent it; a panicked job sent nothing, and its failed set is
            // published by the game thread.
            p.job_ended(out.play.as_ref().filter(|_| sent));
        }
        self.adopt(want, generation, out);
    }

    /// The cursor's job for `id` in `cursor_generation` ended with
    /// `outcome` (SX5-SX8): its set, or a panicked job's failed one, is
    /// the selection set while that cursor generation is current, else
    /// dropped (stale); a cancelled job leaves nothing. A failed set,
    /// which the job never sent, is published by the next step.
    fn cursor_finished(
        &mut self,
        id: CursorId,
        cursor_generation: u64,
        outcome: Option<Outcome<Arc<Output>>>,
    ) {
        let (set, sent) = match outcome {
            Some(Outcome::Done(out)) => match &*out {
                Output::Cursor {
                    set,
                    #[cfg(test)]
                    counts,
                } => {
                    #[cfg(test)]
                    {
                        self.counts.0 += counts.0;
                        self.counts.1 += counts.1;
                    }
                    (Arc::clone(set), true)
                }
                Output::Attempt(_) => return,
            },
            Some(Outcome::Panicked(p)) => {
                log::warn!("chart images: the worker stopped unexpectedly: {}", p.0);
                (Arc::new(ChartImages::absent(&Absent::Failed)), false)
            }
            Some(Outcome::Cancelled) | None => return,
        };
        if cursor_generation != self.cursor_generation() {
            return;
        }
        if let Some(p) = self
            .sel_publication
            .as_mut()
            .filter(|p| p.awaits_job(cursor_generation))
        {
            p.job_ended(sent.then_some(&set));
        }
        self.adopt_selection(id, cursor_generation, set);
    }

    /// `set` is the selection set for `id` in `cursor_generation` (I7).
    fn adopt_selection(&mut self, id: CursorId, cursor_generation: u64, set: Arc<ChartImages>) {
        self.selection = Some(SelectionSet {
            id,
            cursor_generation,
            set,
        });
        log::debug!(
            "chart images: read for the cursor; {} bytes retained",
            self.retained_bytes()
        );
    }

    /// Begins the job the last step left, once no job's thread is alive and
    /// no publication is unanswered, so a new set is never read while the
    /// render thread may hold two. What is terminal for the play target
    /// (its current half, or absent with no chart layer) is published for
    /// the skin in use first. With no want, the cursor's deferred request
    /// is begun instead ([`begin_cursor`](Self::begin_cursor)): an
    /// attempt's want comes first.
    fn begin_if_free(&mut self) {
        let Some((coord, spawner, publish)) = self.starter.clone() else {
            return;
        };
        if self.ended {
            self.starter = None;
            return;
        }
        let Some(want) = self.want.clone() else {
            if self.starter_cursor {
                self.begin_cursor(&coord, spawner, publish.as_ref());
            } else {
                self.starter = None;
            }
            return;
        };
        let generation = self.generation();
        let terminal = match want.play {
            None => Some(None),
            Some(_) => self.play_current().map(|s| Some(Arc::clone(s))),
        };
        let mut published = true;
        if let Some(set) = terminal {
            published = publish
                .as_ref()
                .is_none_or(|p| self.publish_now(p, generation, set));
        }
        if !self.needs_job() {
            self.blocked = None;
            if published {
                self.starter = None;
            }
            return;
        }
        if self.job.is_some() || self.obsolete.is_some() {
            return;
        }
        if self.held_back() {
            self.blocked.get_or_insert_with(Instant::now);
            return;
        }
        self.blocked = None;
        // The starter stays until the play half is published (above).
        // The last sources' reads are the candidates: an unchanged file's
        // read is kept, not decoded again.
        let (prior, play_prior, result_prior) = self.priors();
        let (fresh, derive) = self.work();
        let [derives_play, _] = derive;
        self.begun += 1;
        let key = JobKey {
            kind: JobKind::ChartImages,
            path: want.dir.clone(),
            speed: bms_model::Speed::NORMAL,
        };
        let read = want.clone();
        let (sender, rx) = match &publish {
            Some(p) if derives_play => {
                let (reply, rx) = mpsc::sync_channel(1);
                (Some((p.clone(), reply)), Some(rx))
            }
            _ => (None, None),
        };
        let spawned = coord.spawn_owned(
            spawner,
            key,
            (Class::Foreground, false, Owner::Foreground),
            coord.subscriber(Scope::Choice),
            |_| {},
            move |ctx| {
                let out = run_job(
                    &read,
                    prior.as_ref(),
                    fresh,
                    [play_prior.as_ref(), result_prior.as_ref()],
                    derive,
                    &|| ctx.cancelled(),
                );
                if let Some((p, reply)) = sender
                    && !ctx.cancelled()
                {
                    p.publish(generation, None, out.play.clone(), reply);
                }
                Arc::new(Output::Attempt(out))
            },
        );
        match spawned {
            Ok(job) => {
                if let Some(rx) = rx {
                    self.supersede();
                    self.publication = Some(Publication::sent(generation, None, None, rx, true));
                }
                self.job = Some(Job {
                    target: Target::Attempt {
                        want,
                        generation,
                        fresh,
                        derive,
                    },
                    job,
                });
            }
            Err(e) => {
                // A thread that cannot start: the images absent, terminal
                // at once, the run goes on (design C13).
                log::warn!("chart images: {e}");
                let out = failed_out(&want, fresh, derive);
                self.adopt(&want, generation, out);
                // Published at once, or by the waits' service once the
                // retiring one is answered.
                self.begin_if_free();
            }
        }
    }

    /// Begins the cursor's deferred request once nothing holds the worker
    /// (SX1, SX20): a request of an older cursor generation is dropped; the
    /// last selection set's own request is that set again, published
    /// without a job (D3); one naming no image is published absent at once
    /// (SX2); otherwise its job reads it on its own subscriber, keeping
    /// the last selection set's unchanged reads, and publishes what it
    /// read unless cancelled. A job that cannot start leaves the images
    /// absent (SX9).
    fn begin_cursor(
        &mut self,
        coord: &Coordinator,
        spawner: Spawner,
        publish: Option<&crate::render_side::ChartPublisher>,
    ) {
        if self.held_back() {
            return;
        }
        let Some(request) = self.cursor.take() else {
            return;
        };
        let cursor_generation = self.cursor_generation();
        if request.cursor_generation != cursor_generation {
            return;
        }
        let CursorRequest { id, subscriber, .. } = request;
        let publish_now = |c: &mut Self, set: Arc<ChartImages>| {
            if let Some(p) = publish {
                c.publish_selection_now(p, cursor_generation, set);
            }
        };
        if let Some(last) = self
            .selection
            .as_mut()
            .filter(|s| s.id == id && ChartImage::ALL.into_iter().all(|w| reusable(s.set.read(w))))
        {
            last.cursor_generation = cursor_generation;
            let set = Arc::clone(&last.set);
            publish_now(self, set);
            return;
        }
        if id.names.iter().all(|n| *n == library::Name::None) {
            let set = Arc::new(ChartImages::absent(&Absent::NoHeader));
            self.adopt_selection(id, cursor_generation, Arc::clone(&set));
            publish_now(self, set);
            return;
        }
        // The last selection set is the only candidate (D3).
        let prior = self
            .selection
            .as_ref()
            .filter(|s| s.id.side == id.side)
            .map(|s| Arc::clone(&s.set));
        self.begun += 1;
        let key = JobKey {
            kind: JobKind::ChartImages,
            path: id.dir.clone(),
            speed: bms_model::Speed::NORMAL,
        };
        let (sender, rx) = match publish {
            Some(p) => {
                let (reply, rx) = mpsc::sync_channel(1);
                (Some((p.clone(), reply)), Some(rx))
            }
            None => (None, None),
        };
        let read = id.clone();
        let spawned = coord.spawn_owned(
            spawner,
            key,
            (Class::Foreground, false, Owner::Foreground),
            subscriber,
            |_| {},
            move |ctx| {
                let out = run_cursor_job(&read, prior.as_deref(), &|| ctx.cancelled());
                if let Some((p, reply)) = sender
                    && !ctx.cancelled()
                    && let Output::Cursor { set, .. } = &out
                {
                    p.publish_selection(cursor_generation, Some(Arc::clone(set)), reply);
                }
                Arc::new(out)
            },
        );
        match spawned {
            Ok(job) => {
                if let Some(rx) = rx {
                    #[cfg(test)]
                    {
                        self.sel_sent += 1;
                    }
                    self.supersede_selection();
                    self.sel_publication =
                        Some(Publication::sent(cursor_generation, None, None, rx, true));
                }
                self.job = Some(Job {
                    target: Target::Cursor {
                        id,
                        cursor_generation,
                    },
                    job,
                });
            }
            Err(e) => {
                // A thread that cannot start: the images absent, the run
                // goes on.
                log::warn!("chart images: {e}");
                let set = Arc::new(ChartImages::absent(&Absent::Failed));
                self.adopt_selection(id, cursor_generation, Arc::clone(&set));
                publish_now(self, set);
            }
        }
    }

    /// Publishes the selection set again when it is current for the
    /// cursor generation and its last publication's answer is not (a
    /// screen built again, SC6, SX11, SX19), or none was sent: no decode,
    /// no move.
    fn republish_selection(&mut self, publisher: &crate::render_side::ChartPublisher) {
        let cursor_generation = self.cursor_generation();
        let Some(set) = self
            .selection
            .as_ref()
            .filter(|s| s.cursor_generation == cursor_generation)
            .map(|s| Arc::clone(&s.set))
        else {
            return;
        };
        self.publish_selection_now(publisher, cursor_generation, set);
    }

    /// Publishes `set` for `cursor_generation` into the selection
    /// installation, unless that very publication is asked for and its
    /// answer, if any, is still current ([`PublicationKey::current`]: the
    /// cursor generation and the live selection key); `false` when nothing
    /// was sent: no selection installation to publish into (SX18: the last
    /// answer stays not current until a new installation's key, which is
    /// then asked for again), or another publication unanswered.
    fn publish_selection_now(
        &mut self,
        publisher: &crate::render_side::ChartPublisher,
        cursor_generation: u64,
        set: Arc<ChartImages>,
    ) -> bool {
        self.poll_answers();
        // Read after the answers: the render thread mirrors its targets
        // before it answers.
        let now = publisher.now(self.generation());
        let asked = self.sel_publication.as_ref().is_some_and(|p| {
            p.gone
                || (p.generation == cursor_generation
                    && same(p.set.as_ref(), Some(&set))
                    && !p.unsent
                    && p.ack.as_ref().is_none_or(|a| a.key.current(&now, None)))
        });
        if asked || now.selection.is_none() {
            return asked;
        }
        self.supersede_selection();
        if self.retiring.is_some() || self.play_pending() {
            return false;
        }
        #[cfg(test)]
        {
            self.sel_sent += 1;
        }
        let (reply, rx) = mpsc::sync_channel(1);
        publisher.publish_selection(cursor_generation, Some(Arc::clone(&set)), reply);
        self.sel_publication = Some(Publication::sent(
            cursor_generation,
            None,
            Some(set),
            rx,
            false,
        ));
        true
    }

    /// Publishes `set` (absent when `None`) for the skin in use, unless
    /// that very publication is already asked for and its answer, if any,
    /// is not of another generation; `false` when another is unanswered,
    /// so nothing was sent.
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
        if self.retiring.is_some() || self.sel_pending() {
            return false;
        }
        let (reply, rx) = mpsc::sync_channel(1);
        publisher.publish(generation, None, set.clone(), reply);
        self.publication = Some(Publication::sent(generation, None, set, rx, false));
        true
    }

    /// The bytes it keeps: the sources, both halves and, while the render
    /// thread may still hold them, the published and the retiring set,
    /// each counted once.
    fn retained_bytes(&self) -> u64 {
        let mut seen: Vec<&Arc<ChartImages>> = Vec::new();
        let mut total = 0_u64;
        let sets = self
            .sources
            .iter()
            .map(|(_, _, s)| s)
            .chain(self.play.iter().map(|h| &h.fits))
            .chain(self.result.iter().map(|h| &h.fits))
            .chain(self.selection.iter().map(|s| &s.set))
            .chain(
                [&self.publication, &self.sel_publication, &self.retiring]
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p.set.as_ref()),
            );
        for s in sets {
            if !seen.iter().any(|k| Arc::ptr_eq(k, s)) {
                total = total.saturating_add(s.bytes());
                seen.push(s);
            }
        }
        total
    }
}

/// Settles an ended job, logging a panic after its publication.
fn settle_logged(job: OwnedJob<Arc<Output>>) {
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
                    ChartImage::Banner => "banner",
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
            names: ["s.png".to_owned(), "b.png".to_owned(), String::new()],
            play: side,
            result: None,
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
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        png(d.path(), "b.png", [512, 384], [40, 50, 60]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        w.attempt(want(d.path(), Some(SIDE)));
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
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        let ready = |w: &ChartImageWorker| w.current().is_some() && w.idle();
        w.attempt(want(d.path(), Some(SIDE)));
        step_until(&w, &coord, Spawner::SYSTEM, ready);
        let first = w.current().unwrap();
        assert!(matches!(
            first.read(ChartImage::Back).image,
            Err(Absent::Unresolved(_))
        ));
        // Not drawn yet: the same want is the same set.
        w.attempt(want(d.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        assert!(std::sync::Arc::ptr_eq(&w.current().unwrap(), &first));
        // Drawn, then the back image appears: the stage file kept.
        w.committed();
        png(d.path(), "b.png", [512, 384], [40, 50, 60]);
        w.attempt(want(d.path(), Some(SIDE)));
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
        w.attempt(want(d.path(), Some(SIDE)));
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
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        w.attempt(want(d.path(), Some(SIDE)));
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
        w.attempt(want(a.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        assert_eq!(w.begun(), 1);
        // A is left with its read held: B asked for, never begun meanwhile.
        w.forget();
        w.attempt(want(b.path(), Some(SIDE)));
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
        w.attempt(want(d.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        assert_eq!(w.begun(), 1);
        // Left: the choice's scope cancelled; then chosen again.
        coord.advance(Scope::Choice);
        w.forget();
        w.renew();
        w.attempt(want(d.path(), Some(SIDE)));
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

    /// A fit for a larger layer, its file unchanged, is derived again from
    /// the source for a smaller layer whose slot it does not fit: the new
    /// fit is placed there, and nothing is decoded again.
    #[test]
    fn a_fit_for_a_larger_layer_is_derived_again_for_a_smaller_one() {
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [1_600, 1_200], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        let ready = |w: &ChartImageWorker| w.current().is_some() && w.idle();
        w.attempt(want(d.path(), Some(4_096)));
        step_until(&w, &coord, Spawner::SYSTEM, ready);
        let large = w.current().unwrap();
        w.attempt(want(d.path(), Some(SIDE)));
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
    /// on the calling thread; a play target with no chart layer has no
    /// play half, but the sources are decoded and the facts resolved;
    /// a read stopped part way resolves nothing.
    #[test]
    fn the_direct_launch_reads_at_once() {
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let w = ChartImageWorker::default();
        w.attempt(want(d.path(), Some(SIDE)));
        w.step(None, &|| false, None);
        assert!(w.idle());
        let set = w.current().unwrap();
        assert!(set.read(ChartImage::Stage).image.is_ok());
        let none = ChartImageWorker::default();
        none.attempt(want(d.path(), None));
        none.step(None, &|| false, None);
        assert!(none.current().is_none());
        assert_eq!(none.begun(), 1);
        assert_eq!(
            none.facts(),
            Some(skin_model::ImageFacts {
                stage: true,
                back: false
            })
        );
        let stopped = ChartImageWorker::default();
        stopped.attempt(want(d.path(), None));
        stopped.step(None, &|| true, None);
        assert_eq!(stopped.facts(), None);
    }

    /// Steps `w` until its facts resolve and its halves are derived, idle.
    fn settle_all(w: &ChartImageWorker, coord: &Coordinator) {
        step_until(w, coord, Spawner::SYSTEM, |w| {
            w.idle() && w.facts().is_some() && !w.core().needs_job()
        });
    }

    fn both(dir: &Path, play: Option<u32>, result: Option<u32>) -> Want {
        Want {
            play,
            result,
            ..want(dir, None)
        }
    }

    /// A play side of 2 304 and a result side of 4 096, a
    /// 1 600×1 200 stage file and a 512×384 back image: each file decoded
    /// once; the result's stage file at its native size, play's
    /// downscaled into its 1 152 slot; the back image's fit shared, as both
    /// slots derive it at its native size.
    #[test]
    fn sources_decode_once_and_every_half_derives_from_them() {
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [1_600, 1_200], [10, 20, 30]);
        png(d.path(), "b.png", [512, 384], [40, 50, 60]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        w.attempt(both(d.path(), Some(2_304), Some(4_096)));
        settle_all(&w, &coord);
        assert_eq!(w.counts(), (2, 3));
        let play = w.current().unwrap();
        let result = w.current_result().unwrap();
        let content =
            |s: &super::ChartImages, which| s.read(which).image.as_ref().unwrap().content();
        assert_eq!(content(&result, ChartImage::Stage), [1_600, 1_200]);
        assert!(content(&play, ChartImage::Stage)[0] < 1_600);
        assert!(std::ptr::eq(
            play.read(ChartImage::Back),
            result.read(ChartImage::Back)
        ));
        assert_eq!(
            w.facts(),
            Some(skin_model::ImageFacts {
                stage: true,
                back: true
            })
        );
        // Handed over at the commit: the half moves, the sources stay.
        let taken = w.take_result().unwrap();
        assert!(std::sync::Arc::ptr_eq(
            taken.half.as_ref().unwrap(),
            &result
        ));
        assert!(w.current_result().is_none());
        assert!(std::sync::Arc::ptr_eq(
            &taken.sources,
            &w.sources().unwrap()
        ));
    }

    /// The want at the choice (no play side), then the facts
    /// stage's (play 2 304), then both sides: the later jobs decode
    /// nothing; the last derives only the result half (here sharing the
    /// play half's fits), keeping the sources and the play half as the
    /// same sets.
    #[test]
    fn a_want_changing_only_a_side_only_derives() {
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        png(d.path(), "b.png", [512, 384], [40, 50, 60]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        w.attempt(both(d.path(), None, None));
        settle_all(&w, &coord);
        let sources = w.sources().unwrap();
        w.attempt(both(d.path(), Some(2_304), None));
        settle_all(&w, &coord);
        let play = w.current().unwrap();
        let (_, der) = w.counts();
        w.attempt(both(d.path(), Some(2_304), Some(4_096)));
        settle_all(&w, &coord);
        assert_eq!(w.counts().0, 2, "decoded at the choice only");
        assert_eq!(w.counts().1, der, "640×480 fits both slots: shared");
        assert!(std::sync::Arc::ptr_eq(&w.sources().unwrap(), &sources));
        assert!(std::sync::Arc::ptr_eq(&w.current().unwrap(), &play));
        assert!(w.current_result().is_some());
        assert_eq!(w.begun(), 3);
    }

    /// A job whose files are no longer asked for when it ends:
    /// its output is never adopted, and the facts stay unresolved until
    /// the files asked for are read.
    #[test]
    fn a_stale_jobs_output_is_never_adopted() {
        let _serial = crate::imports::tests::serial();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        png(a.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        super::HOLD.store(true, std::sync::atomic::Ordering::Release);
        w.attempt(want(a.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        // Another chart's files while A's read is held.
        w.attempt(want(b.path(), Some(SIDE)));
        super::HOLD.store(false, std::sync::atomic::Ordering::Release);
        settle_all(&w, &coord);
        // B has no files: A's stage file, read, was never adopted for it.
        assert_eq!(
            w.facts(),
            Some(skin_model::ImageFacts {
                stage: false,
                back: false
            }),
            "the job's output adopted for B"
        );
        assert!(w.current().unwrap().read(ChartImage::Stage).image.is_err());
        assert_eq!(w.begun(), 2);
    }

    /// A panicked job and a thread that cannot start
    /// resolve the facts as absent, and the halves asked for absent.
    #[test]
    fn a_failed_job_resolves_the_facts_absent() {
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let none = Some(skin_model::ImageFacts::default());
        let w = ChartImageWorker::default();
        super::PANIC.store(true, std::sync::atomic::Ordering::Release);
        w.attempt(both(d.path(), Some(SIDE), Some(4_096)));
        settle_all(&w, &coord);
        assert_eq!(w.facts(), none);
        assert_eq!(
            w.current_result().unwrap().read(ChartImage::Stage).image,
            Err(Absent::Failed)
        );
        let w = ChartImageWorker::default();
        w.attempt(both(d.path(), Some(SIDE), Some(4_096)));
        w.step(
            Some((&coord, Spawner::refusing_owned(&[JobKind::ChartImages]))),
            &|| false,
            None,
        );
        assert_eq!(w.facts(), none);
        assert!(w.current().is_some() && w.current_result().is_some());
    }

    /// A job that only derives a half, the sources already resolved, and
    /// fails (its thread cannot start, or it panics): the sources stay,
    /// so the availability resolved before it stays; only that half is
    /// failed, and the play half stays current.
    #[test]
    fn a_failed_derivation_keeps_the_resolved_sources() {
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let resolved = Some(skin_model::ImageFacts {
            stage: true,
            back: false,
        });
        for refuse in [false, true] {
            let w = ChartImageWorker::default();
            w.attempt(both(d.path(), Some(SIDE), None));
            settle_all(&w, &coord);
            let (sources, play) = (w.sources().unwrap(), w.current().unwrap());
            assert_eq!(w.facts(), resolved);
            w.attempt(both(d.path(), Some(SIDE), Some(4_096)));
            if refuse {
                w.step(
                    Some((&coord, Spawner::refusing_owned(&[JobKind::ChartImages]))),
                    &|| false,
                    None,
                );
            } else {
                super::PANIC.store(true, std::sync::atomic::Ordering::Release);
                settle_all(&w, &coord);
            }
            assert_eq!(w.facts(), resolved, "refused {refuse}: the facts changed");
            assert!(std::sync::Arc::ptr_eq(&w.sources().unwrap(), &sources));
            assert!(std::sync::Arc::ptr_eq(&w.current().unwrap(), &play));
            assert_eq!(
                w.current_result().unwrap().read(ChartImage::Stage).image,
                Err(Absent::Failed)
            );
            assert!(w.take_result().unwrap().sources.facts().stage);
        }
    }

    /// The run's end with a job that ignores its cancel: `end` gives it up
    /// within its bound (the job owning what it holds, left to the
    /// process), keeps nothing itself, and nothing begins after it.
    #[test]
    fn the_end_leaves_a_job_ignoring_its_cancel() {
        let _serial = crate::imports::tests::serial();
        struct LetGo;
        impl Drop for LetGo {
            fn drop(&mut self) {
                super::HOLD_HARD.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _let_go = LetGo;
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        super::HOLD_HARD.store(true, std::sync::atomic::Ordering::Release);
        w.attempt(want(d.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        assert!(!w.idle());
        let end = Instant::now() + Duration::from_secs(10);
        while !super::HELD_HARD.load(std::sync::atomic::Ordering::Acquire) {
            assert!(Instant::now() < end, "the read never began");
            std::thread::sleep(Duration::from_millis(1));
        }
        coord.advance(Scope::Choice);
        let began = Instant::now();
        w.end();
        let took = began.elapsed();
        assert!(
            took >= super::STOP_WAIT && took < super::STOP_WAIT + Duration::from_secs(1),
            "{took:?}"
        );
        assert_eq!(w.ended_jobs(), (1, 1), "the job left to the process");
        assert!(w.idle());
        assert_eq!(w.retained(), 0);
        let jobs = w.begun();
        w.attempt(want(d.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        w.service();
        assert_eq!(w.begun(), jobs, "a job began after the end");
    }

    /// A result half whose derivation failed, then a new generation of the
    /// same, unchanged files before the result took it (a practice Start):
    /// the failed fit is never carried forward; with a working spawner the
    /// half is an image again (derived afresh, or shared with the play
    /// half's own fit), from the kept sources, decoding nothing.
    #[test]
    fn a_failed_fit_is_derived_again_in_the_next_generation() {
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        w.attempt(both(d.path(), Some(SIDE), None));
        settle_all(&w, &coord);
        let sources = w.sources().unwrap();
        w.attempt(both(d.path(), Some(SIDE), Some(4_096)));
        w.step(
            Some((&coord, Spawner::refusing_owned(&[JobKind::ChartImages]))),
            &|| false,
            None,
        );
        assert_eq!(
            w.current_result().unwrap().read(ChartImage::Stage).image,
            Err(Absent::Failed)
        );
        // Drawn, then renewed before the result took the half.
        w.committed();
        w.renew();
        w.attempt(both(d.path(), Some(SIDE), Some(4_096)));
        let decodes = w.counts().0;
        settle_all(&w, &coord);
        let half = w.current_result().unwrap();
        assert_eq!(
            half.read(ChartImage::Stage)
                .image
                .as_ref()
                .map(skin_model::chart_image::Fitted::content),
            Ok([640, 480]),
            "the failed fit carried forward"
        );
        assert!(std::ptr::eq(
            w.sources().unwrap().read(ChartImage::Stage),
            sources.read(ChartImage::Stage)
        ));
        assert_eq!(w.counts().0, decodes, "decoded again");
    }

    /// Sources a failed job left (its panic) are never kept for the next
    /// generation of the same, unchanged files: they are read again, and
    /// the facts resolve to the file's own; a derivation cancelled part way
    /// is not a set a layout keeps.
    #[test]
    fn failed_sources_and_derivations_are_never_kept() {
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        super::PANIC.store(true, std::sync::atomic::Ordering::Release);
        w.attempt(both(d.path(), None, None));
        settle_all(&w, &coord);
        assert_eq!(w.facts(), Some(skin_model::ImageFacts::default()));
        w.committed();
        w.renew();
        w.attempt(both(d.path(), None, None));
        settle_all(&w, &coord);
        assert_eq!(
            w.facts(),
            Some(skin_model::ImageFacts {
                stage: true,
                back: false
            }),
            "the failed sources kept"
        );
        let sources = w.sources().unwrap();
        assert!(super::derive_set(&sources, 4_096, &|| true).is_none());
        assert!(super::derive_set(&sources, 4_096, &|| false).is_some());
    }

    /// The run's end with a held job cancelled through its
    /// scope: `end` returns within its bound, keeps nothing, and nothing
    /// begins after it, from a step, the waits' service or the direct
    /// launch's read.
    #[test]
    fn nothing_begins_after_the_end() {
        let _serial = crate::imports::tests::serial();
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let w = ChartImageWorker::default();
        w.attempt(want(d.path(), Some(SIDE)));
        settle_all(&w, &coord);
        assert!(w.retained() > 0);
        super::HOLD.store(true, std::sync::atomic::Ordering::Release);
        w.renew();
        w.attempt(want(d.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
        assert!(!w.idle());
        coord.advance(Scope::Choice);
        let began = Instant::now();
        w.end();
        assert!(began.elapsed() < super::STOP_WAIT + Duration::from_secs(1));
        assert_eq!(w.ended_jobs(), (1, 0), "the cancelled job joined");
        assert!(w.idle());
        assert_eq!(w.retained(), 0);
        super::HOLD.store(false, std::sync::atomic::Ordering::Release);
        let jobs = w.begun();
        w.attempt(want(d.path(), Some(SIDE)));
        for _ in 0..10 {
            w.step(Some((&coord, Spawner::SYSTEM)), &|| false, None);
            w.service();
        }
        w.step(None, &|| false, None);
        assert_eq!(w.begun(), jobs, "a job began after the end");
        assert_eq!(w.facts(), None);
    }
}
