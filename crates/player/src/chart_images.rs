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
//! - **The selection** (design.md "Rendering and Skins", chart images):
//!   the cursor's row, once it rested, is read into a slot of the
//!   selection's chart-image pool no pin holds, and written there by the
//!   render thread at a frame boundary, one write in flight. Every row
//!   read is an entry ([`pool`]), resident in its slot or absent, which
//!   each update names for its cursor row, so a row visited before shows
//!   at once while its entry stays; each use of an entry is checked after
//!   it by a job of its own (design.md "Load Pipeline", Freshness). A new
//!   pool generation drops the resident entries.

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

mod pool;

#[cfg(test)]
pub(crate) use pool::{EntryState, SlotState};

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

/// Holds every check of a selection entry after its last observation
/// until let go (tests: a check's result held).
#[cfg(test)]
pub static CHECK_HOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set while a check is held by [`CHECK_HOLD`] (tests).
#[cfg(test)]
pub static CHECK_HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

/// An open file's information from its handle, `None` when the system
/// cannot give it.
#[must_use]
pub fn file_information(
    file: &File,
) -> Option<windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is the open file's for the call's duration, and
    // `info` is a valid out-parameter of the call's type.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &raw mut info) }.ok()?;
    Some(info)
}

/// An open file's identity: its volume's serial number and its file
/// index (the NTFS file id), `None` when the system cannot give it.
#[must_use]
pub fn file_identity(file: &File) -> Option<FileIdentity> {
    let info = file_information(file)?;
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
/// Chart images): the chart's folder and its stage file and banner names
/// as the library keeps them. The selection's entries are keyed by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorId {
    pub dir: PathBuf,
    /// The stage file's, then the banner's.
    pub names: [library::Name; 2],
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

/// The binding of `which`'s name in `id` as observed now: its name
/// resolved again under T08's rules in `dir` (an overlong name to
/// nothing), or the folder's own failure.
fn binding_now(
    dir: &Result<ChartDir, std::io::Error>,
    name: &library::Name,
    identity: chart_image::Identity<'_>,
) -> chart_image::Binding {
    match (dir, name) {
        (_, library::Name::None) => chart_image::Binding::NoHeader,
        (_, library::Name::Overlong) => chart_image::Binding::Unresolved {
            name: String::new(),
            why: chart_image::Unresolved::Overlong,
        },
        (Err(e), library::Name::Named(_)) => chart_image::Binding::Unresolved {
            name: String::new(),
            why: chart_image::Unresolved::Io(e.kind()),
        },
        (Ok(dir), library::Name::Named(name)) => chart_image::resolve(dir, name, &[], identity),
    }
}

/// `source` derived into `cell`: absent when there is no cell or no image.
fn derive_into(source: &Read, cell: Option<chart_image::Slot>, cancel: &dyn Fn() -> bool) -> Read {
    let image = match (&source.image, cell) {
        (Ok(image), Some(cell)) => {
            #[cfg(test)]
            count([0, 1]);
            chart_image::derive(image, cell, cancel)
        }
        (Ok(_), None) => Err(Absent::NoHeader),
        (Err(why), _) => Err(why.clone()),
    };
    Read {
        binding: source.binding.clone(),
        image,
    }
}

/// `sources`' stage file and banner each derived into its pool cell of
/// `cells`, the back image absent, as the cursor's job derives them
/// ([`read_row`]; dev-tools tests): `None` when a derivation failed, so no
/// failed fit is ever kept ([`fit_reusable`]).
#[cfg(all(test, feature = "dev-tools"))]
#[must_use]
pub fn derive_cells(
    sources: &Sources,
    cells: chart_image::ChartCells,
    cancel: &dyn Fn() -> bool,
) -> Option<ChartImages> {
    let reads = ChartImage::ALL.map(|which| {
        let cell = match which {
            ChartImage::Stage => cells.stage,
            ChartImage::Banner => cells.banner,
            ChartImage::Back => return absent_read(chart_image::Binding::NoHeader),
        };
        Arc::new(derive_into(sources.read(which), cell, cancel))
    });
    Some(ChartImages::of(reads)).filter(|d| {
        [ChartImage::Stage, ChartImage::Banner]
            .into_iter()
            .all(|w| {
                fit_reusable(d.read(w), sources.read(w))
                    || matches!(d.read(w).image, Err(Absent::NoHeader))
            })
    })
}

/// Reads row `id`'s images into the pool's `cells`: the stage file and the
/// banner, each name resolved again, decoded, then derived into its cell
/// ([`derive_into`]), one at a time, the source dropped; the back image is
/// never read there. A name with no cell is resolved but not decoded. An
/// overlong name resolves to nothing, never to a shortened one. Asks
/// `cancel` throughout, between the decode and the derivation included.
fn read_row(
    id: &CursorId,
    cells: chart_image::ChartCells,
    cancel: &dyn Fn() -> bool,
) -> ChartImages {
    let dir = ChartDir::new(&id.dir);
    #[cfg(test)]
    hold(cancel);
    let identity: chart_image::Identity<'_> = &file_identity;
    let [stage, banner] = &id.names;
    let reads = ChartImage::ALL.map(|which| {
        let (name, cell) = match which {
            ChartImage::Stage => (stage, cells.stage),
            ChartImage::Banner => (banner, cells.banner),
            ChartImage::Back => return absent_read(chart_image::Binding::NoHeader),
        };
        let binding = binding_now(&dir, name, identity);
        let (Ok(dir), chart_image::Binding::Resolved { .. }, Some(_)) = (&dir, &binding, cell)
        else {
            return absent_read(binding);
        };
        let library::Name::Named(name) = name else {
            return absent_read(binding);
        };
        #[cfg(test)]
        count([1, 0]);
        let source = chart_image::read_source(dir, name, &[], cancel, identity);
        if cancel() {
            return Arc::new(Read {
                binding: source.binding,
                image: Err(Absent::Cancelled),
            });
        }
        Arc::new(derive_into(&source, cell, cancel))
    });
    ChartImages::of(reads)
}

/// Whether row `id`'s names still resolve as `bindings` observed them
/// (design.md "Load Pipeline", Freshness): each resolved again under T08's
/// rules and compared ([`chart_image::Binding::same_as`]), reading no
/// bytes.
fn unchanged(id: &CursorId, bindings: &[chart_image::Binding; 2]) -> bool {
    let dir = ChartDir::new(&id.dir);
    let identity: chart_image::Identity<'_> = &file_identity;
    let same = id
        .names
        .iter()
        .zip(bindings)
        .all(|(name, kept)| binding_now(&dir, name, identity).same_as(kept));
    #[cfg(test)]
    hold_check();
    same
}

/// Holds a check after its last observation while [`CHECK_HOLD`] is set
/// (tests).
#[cfg(test)]
fn hold_check() {
    while CHECK_HOLD.load(std::sync::atomic::Ordering::Acquire) {
        CHECK_HELD.store(true, std::sync::atomic::Ordering::Release);
        std::thread::sleep(Duration::from_millis(1));
    }
    CHECK_HELD.store(false, std::sync::atomic::Ordering::Release);
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
/// its row's images fitted into their pool cells.
#[derive(Debug)]
enum Output {
    Attempt(JobOut),
    Cursor {
        fits: Arc<ChartImages>,
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
    /// The cursor's request, for its cursor generation (I6), read into
    /// slot `slot` of pool generation `generation` as read `version`.
    Cursor {
        id: CursorId,
        cursor_generation: u64,
        generation: u64,
        slot: u16,
        version: u64,
    },
}

/// A row's pool write, from its read's end to its answer: what it is for,
/// and the fits, kept until answered (the render thread may hold them).
#[derive(Debug)]
struct PendingWrite {
    generation: u64,
    slot: u16,
    row: CursorId,
    version: u64,
    fits: Arc<ChartImages>,
    /// The answer, once sent; `None` until the select screen's step sends
    /// it.
    rx: Option<mpsc::Receiver<Result<(), render::PoolWriteError>>>,
}

/// A check of an entry (design.md "Load Pipeline", Freshness): its row,
/// the read and pool generation it checks (none for an absent entry), the
/// last use it validates, and its job.
struct Check {
    row: CursorId,
    version: u64,
    generation: Option<u64>,
    covers: u64,
    job: OwnedJob<bool>,
}

/// The update sent last: its cursor row, the version of that row's entry
/// and the value it carried.
#[derive(Debug)]
struct Sent {
    row: Option<CursorId>,
    version: Option<u64>,
    chart: crate::chart_currency::CursorChart,
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
    /// The selection's cursor generation: a new one at each change of the
    /// cursor's request.
    cursor_generation: u64,
    /// The cursor's request, deferred until nothing holds the worker (I5).
    cursor: Option<CursorRequest>,
    /// The cursor's row, as the next update names it: `None` for a row
    /// that is no chart.
    cursor_row: Option<CursorId>,
    /// The cursor's row whose request was taken in this rest, if any: a
    /// pool generation that drops what it read reads it again.
    asked: Option<CursorId>,
    /// The rows read (design.md "Rendering and Skins", chart images).
    entries: pool::Entries,
    /// The live pool generation (0 for none) and its slots.
    pool_generation: u64,
    slots: Option<pool::Slots>,
    /// Reads numbered: each read's version.
    versions: u64,
    /// The one pool write from a read's end to its answer.
    pending_write: Option<PendingWrite>,
    /// The check running, and the one waiting: a row and its entry's
    /// version.
    check: Option<Check>,
    check_queued: Option<(CursorId, u64)>,
    /// Uses stamped so far.
    use_clock: u64,
    /// The update sent last (one of its slot is pinned).
    sent: Option<Sent>,
    /// The next update is a selection entry's: a use of its row's entry.
    entered: bool,
    /// The cursor's read is to be requested again with no new rest: its
    /// entry was withdrawn, or a new pool generation dropped what it read.
    rearm: bool,
    /// The cursor row's value changed: its entry adopted, withdrawn, or
    /// dropped by a new pool generation; the next look sends an update.
    changed: bool,
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
    /// Tests: the pool writes sent, and the checks begun.
    #[cfg(test)]
    writes_sent: u64,
    #[cfg(test)]
    checks_begun: u64,
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
        Self::new(Arc::default())
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
            .field("cursor_row", &c.cursor_row)
            .field("entries", &c.entries.len())
            .field("pool_generation", &c.pool_generation)
            .field("pending_write", &c.pending_write)
            .field(
                "check",
                &c.check.as_ref().map(|k| (&k.row, k.version, k.covers)),
            )
            .field("check_queued", &c.check_queued)
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
                cursor_generation: 0,
                cursor: None,
                cursor_row: None,
                asked: None,
                entries: pool::Entries::default(),
                pool_generation: 0,
                slots: None,
                versions: 0,
                pending_write: None,
                check: None,
                check_queued: None,
                use_clock: 0,
                sent: None,
                entered: false,
                rearm: false,
                changed: false,
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
                writes_sent: 0,
                #[cfg(test)]
                checks_begun: 0,
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
    /// last chart's images stays beside selection's. The selection's
    /// entries stay, checked on their next use, and so do the pool's slots
    /// while their generation lives (design.md "Rendering and Skins",
    /// chart images).
    pub fn drop_images(&self) {
        let mut c = self.core();
        c.sources = None;
        c.play = None;
        c.result = None;
    }

    /// The cursor's request for `dir`'s images `names` (the stage file's,
    /// then the banner's): for the current cursor generation, its
    /// subscriber to `coord`'s cursor scope taken now (D2).
    pub fn cursor_request(
        &self,
        dir: PathBuf,
        names: [library::Name; 2],
        coord: &Coordinator,
    ) -> CursorRequest {
        let c = self.core();
        CursorRequest {
            id: CursorId { dir, names },
            cursor_generation: c.cursor_generation,
            subscriber: coord.subscriber(Scope::Cursor),
        }
    }

    /// The cursor's row changed to `row` (`None` for a row that is no
    /// chart; SC4, D2): the one function every change goes through. A new
    /// cursor generation; the deferred request goes; the cursor's scope
    /// advances, cancelling a running cursor job, whose output is never
    /// adopted (it stays the worker's until its thread ends); the next
    /// update names `row`.
    pub fn cursor_changed(&self, coord: &Coordinator, row: Option<CursorId>) {
        self.core().cursor_changed(coord, row);
    }

    /// The next update is a selection entry's: a use of its row's entry
    /// (design.md "Load Pipeline", Freshness).
    pub fn entered(&self) {
        self.core().entered = true;
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
    /// selection's starter goes, so nothing of selection begins after it;
    /// a read's write not yet sent goes, its slot freed. A write sent is
    /// answered by the render thread, which keeps none past selection, and
    /// no job begins until it is. The entries and slots stay (design.md
    /// "Rendering and Skins", chart images).
    pub fn leave_selection(&self, coord: &Coordinator) {
        let mut c = self.core();
        c.cursor_changed(coord, None);
        if c.starter_cursor {
            c.starter = None;
            c.starter_cursor = false;
        }
        c.sent = None;
        c.entered = false;
        c.changed = false;
        if c.pending_write.as_ref().is_some_and(|p| p.rx.is_none())
            && let Some(p) = c.pending_write.take()
        {
            c.free_slot(p.generation, p.slot, p.version);
        }
    }

    /// Advances the selection's images without blocking (D2): takes what
    /// ended (the cursor's read, the pool write's answer, the check), reads
    /// the live pool (a new generation drops the resident entries, P-h),
    /// sends a finished read's pool write, begins the deferred request once
    /// nothing holds the worker, and begins the check waiting. Never begins
    /// anything after [`end`](Self::end).
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
        c.sync_pool(publisher);
        c.send_write(publisher);
        c.starter = Some((coord.clone(), spawner, Some(publisher.clone())));
        c.starter_cursor = true;
        c.begin_if_free();
        c.begin_check(coord, spawner);
    }

    /// The value the update about to be sent carries for the cursor's row
    /// (design.md "Rendering and Skins", chart images): resident in its
    /// slot, absent, or pending without an entry. Called only for an
    /// update sent: it pins the slot it names and makes it, and the
    /// entry, the most recently used. A use of the entry (another row or
    /// read than the last update's, a selection entry, or a list
    /// `refreshed` with the cursor kept) is stamped, and a check queued
    /// for it when one is due (design.md "Load Pipeline", Freshness).
    pub fn update_chart(&self, refreshed: bool) -> crate::chart_currency::CursorChart {
        use crate::chart_currency::CursorChart;
        let mut c = self.core();
        let row = c.cursor_row.clone();
        let entered = std::mem::take(&mut c.entered);
        let entry = row
            .as_ref()
            .and_then(|r| c.entries.get(r))
            .map(|e| (e.version, e.state));
        let chart = match (&row, entry) {
            (None, _) => CursorChart::Absent,
            (Some(_), None) => CursorChart::Pending,
            (Some(_), Some((_, pool::EntryState::Absent))) => CursorChart::Absent,
            (Some(_), Some((_, pool::EntryState::Resident { generation, slot }))) => {
                CursorChart::Resident { generation, slot }
            }
        };
        let version = entry.map(|(v, _)| v);
        if let (Some(r), Some(v)) = (&row, version) {
            let same = c
                .sent
                .as_ref()
                .is_some_and(|s| s.row.as_ref() == Some(r) && s.version == Some(v));
            c.entries.touch(r);
            if let CursorChart::Resident { generation, slot } = chart
                && generation == c.pool_generation
                && let Some(slots) = c.slots.as_mut()
            {
                slots.touch(slot);
            }
            if !same || entered || refreshed {
                c.use_clock += 1;
                let stamp = c.use_clock;
                if let Some(e) = c.entries.get_mut(r) {
                    e.last_use = stamp;
                }
                c.queue_check(r);
            }
        }
        c.sent = Some(Sent {
            row,
            version,
            chart,
        });
        chart
    }

    /// Whether the cursor's read is to be requested again with no new
    /// rest, once: its entry was withdrawn by a check, or a new pool
    /// generation dropped what it read.
    pub fn take_rearm(&self) -> bool {
        std::mem::take(&mut self.core().rearm)
    }

    /// Whether the cursor row's value changed since the last update, once:
    /// its entry adopted, withdrawn, or dropped by a new pool generation.
    pub fn take_changed(&self) -> bool {
        std::mem::take(&mut self.core().changed)
    }

    /// Whether the select screen is to look again within a slice
    /// ([`crate::play::IMAGE_SLICE`]): a deferred request waits, a read or
    /// a check runs or waits, a pool write is unanswered, or the cursor's
    /// value changed or its read is to be requested again.
    #[must_use]
    pub fn cursor_waiting(&self) -> bool {
        let c = self.core();
        c.cursor.is_some()
            || c.job
                .as_ref()
                .is_some_and(|j| matches!(j.target, Target::Cursor { .. }))
            || c.pending_write.is_some()
            || c.check.is_some()
            || c.check_queued.is_some()
            || c.rearm
            || c.changed
    }

    /// Whether the job's thread has ended, its outcome not yet taken
    /// (tests).
    #[cfg(test)]
    #[must_use]
    pub fn job_ended(&self) -> bool {
        self.core().job.as_ref().is_some_and(|j| j.job.finished())
    }

    /// The entry of `id`: where its images are, its version, its last use
    /// and the last use validated (tests).
    #[cfg(test)]
    #[must_use]
    pub fn entry(&self, id: &CursorId) -> Option<(pool::EntryState, u64, u64, u64)> {
        self.core()
            .entries
            .get(id)
            .map(|e| (e.state, e.version, e.last_use, e.validated))
    }

    /// The entry of the cursor's row, as [`entry`](Self::entry) (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    #[must_use]
    pub fn cursor_entry(&self) -> Option<(pool::EntryState, u64, u64, u64)> {
        let c = self.core();
        let row = c.cursor_row.as_ref()?;
        c.entries
            .get(row)
            .map(|e| (e.state, e.version, e.last_use, e.validated))
    }

    /// The entries kept (tests).
    #[cfg(test)]
    #[must_use]
    pub fn entries(&self) -> usize {
        self.core().entries.len()
    }

    /// The live pool's slots as the worker keeps them (tests).
    #[cfg(test)]
    #[must_use]
    pub fn slot_states(&self) -> Vec<pool::SlotState> {
        self.core()
            .slots
            .as_ref()
            .map_or_else(Vec::new, pool::Slots::states)
    }

    /// The live pool generation the worker read last, 0 for none (tests).
    #[cfg(test)]
    #[must_use]
    pub fn pool_generation(&self) -> u64 {
        self.core().pool_generation
    }

    /// The value the last update sent carried (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    #[must_use]
    pub fn sent_chart(&self) -> Option<crate::chart_currency::CursorChart> {
        self.core().sent.as_ref().map(|s| s.chart)
    }

    /// The pool writes sent (tests).
    #[cfg(test)]
    #[must_use]
    pub fn writes_sent(&self) -> u64 {
        self.core().writes_sent
    }

    /// The checks begun (tests).
    #[cfg(test)]
    #[must_use]
    pub fn checks_begun(&self) -> u64 {
        self.core().checks_begun
    }

    /// Whether a check runs, and whether one waits (tests).
    #[cfg(test)]
    #[must_use]
    pub fn checks_pending(&self) -> (bool, bool) {
        let c = self.core();
        (c.check.is_some(), c.check_queued.is_some())
    }

    /// Whether a pool write is unanswered or not yet sent (tests).
    #[cfg(test)]
    #[must_use]
    pub fn write_pending(&self) -> bool {
        self.core().pending_write.is_some()
    }

    /// The cursor generation (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    #[must_use]
    pub fn cursor_generation(&self) -> u64 {
        self.core().cursor_generation
    }

    /// Whether a cursor request is deferred (tests).
    #[cfg(test)]
    #[must_use]
    pub fn cursor_deferred(&self) -> bool {
        self.core().cursor.is_some()
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
            if c.retiring.is_none() && c.pending_write.is_none() {
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
        c.cursor_row = None;
        c.asked = None;
        c.entries.clear();
        c.slots = None;
        c.pool_generation = 0;
        c.pending_write = None;
        c.check_queued = None;
        c.sent = None;
        c.rearm = false;
        c.changed = false;
        let deadline = Instant::now() + STOP_WAIT;
        let jobs = [c.job.take().map(|j| j.job), c.obsolete.take()];
        let check = c.check.take().map(|k| k.job);
        let ended = jobs
            .into_iter()
            .flatten()
            .map(|job| end_job(job, deadline))
            .chain(check.map(|job| end_job(job, deadline)));
        #[cfg(test)]
        for abandoned in ended {
            c.settled += 1;
            c.abandoned += u64::from(abandoned);
        }
        #[cfg(not(test))]
        ended.for_each(drop);
    }
}

/// Settles `job` at the run's end within what is left of `deadline`, or
/// leaves it to the process's exit: whether it was left. A panic is
/// logged, never the run's.
fn end_job<T: Clone>(job: OwnedJob<T>, deadline: Instant) -> bool {
    let bound = deadline.saturating_duration_since(Instant::now());
    match job.settle(bound, &Instant::now, &mut || Some(())) {
        Settled::Joined {
            after_panic: Some(p),
            ..
        } => {
            log::warn!("chart images: the worker stopped unexpectedly: {}", p.0);
            false
        }
        Settled::Joined { .. } => false,
        Settled::Abandoned { .. } => {
            log::warn!("chart images: the worker was left to end with the process");
            true
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

/// The cursor job's work: `id`'s images read into the pool's `cells`
/// ([`read_row`]). Asks `cancel` throughout.
fn run_cursor_job(
    id: &CursorId,
    cells: chart_image::ChartCells,
    cancel: &dyn Fn() -> bool,
) -> Output {
    #[cfg(test)]
    let before = COUNTS.with(std::cell::Cell::get);
    #[cfg(test)]
    assert!(
        !PANIC.swap(false, std::sync::atomic::Ordering::AcqRel),
        "an injected panic of the images' job"
    );
    let fits = Arc::new(read_row(id, cells, cancel));
    Output::Cursor {
        fits,
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

    /// [`ChartImageWorker::cursor_changed`] under the held worker.
    fn cursor_changed(&mut self, coord: &Coordinator, row: Option<CursorId>) {
        self.cursor_generation += 1;
        self.cursor = None;
        coord.advance(Scope::Cursor);
        self.cursor_row = row;
        self.asked = None;
        self.rearm = false;
    }

    /// Whether play's publication is unanswered.
    fn play_pending(&self) -> bool {
        self.publication.as_ref().is_some_and(Publication::pending)
    }

    /// Whether a job may not begin now: a job's thread alive, a
    /// publication retiring, play's unanswered, or a pool write not yet
    /// answered (SX20). Never a check.
    fn held_back(&self) -> bool {
        self.job.is_some()
            || self.obsolete.is_some()
            || self.retiring.is_some()
            || self.play_pending()
            || self.pending_write.is_some()
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

    /// Takes the answers that came: the retiring publication's, the last
    /// one's and the pool write's.
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
        self.poll_write();
    }

    /// Frees slot `slot` of pool generation `generation` from read
    /// `version`, while that generation is live.
    fn free_slot(&mut self, generation: u64, slot: u16, version: u64) {
        if generation == self.pool_generation
            && let Some(slots) = self.slots.as_mut()
        {
            slots.free(slot, version);
        }
    }

    /// Inserts `entry` for `row`, freeing the slots of the entries it
    /// evicts and its own if it is not kept; the cursor row's value
    /// changes when it is the cursor's.
    fn adopt_entry(&mut self, row: CursorId, entry: pool::Entry) {
        let (version, state) = (entry.version, entry.state);
        let cursor = self.cursor_row.as_ref() == Some(&row);
        let (kept, gone) = self.entries.insert(row, entry);
        for e in gone {
            if let pool::EntryState::Resident { generation, slot } = e.state {
                self.free_slot(generation, slot, e.version);
            }
        }
        if !kept && let pool::EntryState::Resident { generation, slot } = state {
            self.free_slot(generation, slot, version);
        }
        if cursor {
            self.changed = true;
        }
        log::debug!(
            "chart images: {} entries, {} bytes",
            self.entries.len(),
            self.entries.bytes()
        );
    }

    /// Takes the pool write's answer, if it came: written, the row is
    /// resident in its slot; otherwise (stale, refused, the device lost,
    /// or the render thread gone) its slot is freed and the row has no
    /// entry, so it is read again at a later rest.
    fn poll_write(&mut self) {
        let Some(rx) = self.pending_write.as_ref().and_then(|p| p.rx.as_ref()) else {
            return;
        };
        let answer = match rx.try_recv() {
            Ok(answer) => answer,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(render::PoolWriteError::Stale),
        };
        let Some(p) = self.pending_write.take() else {
            return;
        };
        match answer {
            Ok(()) if p.generation == self.pool_generation => {
                if let Some(slots) = self.slots.as_mut() {
                    slots.set(p.slot, pool::SlotState::Resident { version: p.version });
                }
                let bindings = [
                    p.fits.read(ChartImage::Stage).binding.clone(),
                    p.fits.read(ChartImage::Banner).binding.clone(),
                ];
                let state = pool::EntryState::Resident {
                    generation: p.generation,
                    slot: p.slot,
                };
                self.adopt_entry(p.row, pool::Entry::new(p.version, bindings, state));
            }
            Ok(()) => {}
            Err(e) => {
                log::info!("chart images: the pool write was not applied: {e}");
                self.free_slot(p.generation, p.slot, p.version);
            }
        }
    }

    /// Sends the pool write a finished read left, if any
    /// ([`crate::render_side::ChartPublisher::write_pool`]).
    fn send_write(&mut self, publisher: &crate::render_side::ChartPublisher) {
        let Some(p) = self.pending_write.as_mut().filter(|p| p.rx.is_none()) else {
            return;
        };
        let (reply, rx) = mpsc::sync_channel(1);
        publisher.write_pool(Box::new(crate::render_side::PoolWrite {
            generation: p.generation,
            slot: p.slot,
            fits: Arc::clone(&p.fits),
            reply,
        }));
        p.rx = Some(rx);
        #[cfg(test)]
        {
            self.writes_sent += 1;
        }
    }

    /// Reads the live pool (P-h): when its generation changed or ended,
    /// the resident entries and the slots go, the absent entries stay, a
    /// read's write not yet sent goes, and the cursor's row, its read
    /// asked in this rest and now without an entry, is read again as on a
    /// cold start. Logs the pool's change once.
    fn sync_pool(&mut self, publisher: &crate::render_side::ChartPublisher) {
        let (layout, _) = publisher.pool_now();
        let generation = layout.map_or(0, |l| l.generation);
        if generation == self.pool_generation {
            return;
        }
        match layout {
            Some(l) => log::info!(
                "chart images: pool generation {}, {} slots, cells {:?} and {:?} in {} layers of {}",
                l.generation,
                l.slots,
                l.cells.stage.map(chart_image::Slot::size),
                l.cells.banner.map(chart_image::Slot::size),
                l.layers,
                l.side
            ),
            None => log::info!("chart images: no pool"),
        }
        let resident = self
            .cursor_row
            .as_ref()
            .and_then(|r| self.entries.get(r))
            .is_some_and(|e| matches!(e.state, pool::EntryState::Resident { .. }));
        self.pool_generation = generation;
        self.slots = layout.map(pool::Slots::new);
        self.entries.drop_resident();
        if self.pending_write.as_ref().is_some_and(|p| p.rx.is_none()) {
            self.pending_write = None;
        }
        if resident {
            self.changed = true;
        }
        if generation != 0
            && let Some(row) = &self.cursor_row
            && self.asked.as_ref() == Some(row)
            && self.entries.get(row).is_none()
        {
            self.rearm = true;
        }
    }

    /// Queues a check of `row`'s entry when it is due (a use not yet
    /// validated) and no check running or waiting covers that use.
    fn queue_check(&mut self, row: &CursorId) {
        let Some(e) = self.entries.get(row) else {
            return;
        };
        if e.last_use <= e.validated {
            return;
        }
        let (version, last_use) = (e.version, e.last_use);
        if self
            .check
            .as_ref()
            .is_some_and(|k| &k.row == row && k.version == version && k.covers >= last_use)
        {
            return;
        }
        if self
            .check_queued
            .as_ref()
            .is_some_and(|(r, v)| r == row && *v == version)
        {
            return;
        }
        self.check_queued = Some((row.clone(), version));
    }

    /// Begins the check waiting when none runs: it validates every use of
    /// its entry up to now (`covers`), resolving the names again and
    /// comparing the bindings (design.md "Load Pipeline", Freshness). A
    /// check is never held back by a read or a write.
    fn begin_check(&mut self, coord: &Coordinator, spawner: Spawner) {
        if self.check.is_some() || self.ended {
            return;
        }
        let Some((row, version)) = self.check_queued.take() else {
            return;
        };
        let Some(e) = self.entries.get(&row).filter(|e| e.version == version) else {
            return;
        };
        let covers = e.last_use;
        let generation = match e.state {
            pool::EntryState::Resident { generation, .. } => Some(generation),
            pool::EntryState::Absent => None,
        };
        let (id, bindings) = (row.clone(), e.bindings.clone());
        let key = JobKey {
            kind: JobKind::ChartCheck,
            path: row.dir.clone(),
            speed: bms_model::Speed::NORMAL,
        };
        let spawned = coord.spawn_owned(
            spawner,
            key,
            (Class::Foreground, false, Owner::Foreground),
            coord.subscriber(Scope::Folder),
            |_| {},
            move |_| unchanged(&id, &bindings),
        );
        match spawned {
            Ok(job) => {
                #[cfg(test)]
                {
                    self.checks_begun += 1;
                }
                self.check = Some(Check {
                    row,
                    version,
                    generation,
                    covers,
                    job,
                });
            }
            Err(e) => log::warn!("chart images: {e}"),
        }
    }

    /// Takes the check's outcome once it ended: a result counts only for
    /// the entry's version and, for a resident entry, its pool generation.
    /// Unchanged, the uses up to the one it covered are validated, and a
    /// later use is checked next; a mismatch drops the entry, its slot
    /// freed, and when it is the cursor's row its value changes and its
    /// read is requested again with no move.
    fn check_finished(&mut self) {
        if !self.check.as_ref().is_some_and(|k| k.job.finished()) {
            return;
        }
        let Some(k) = self.check.take() else {
            return;
        };
        let outcome = k.job.poll().map(|o| o.outcome);
        end_job(k.job, Instant::now() + STOP_WAIT);
        let same = match outcome {
            Some(Outcome::Done(same)) => same,
            Some(Outcome::Panicked(p)) => {
                log::warn!("chart images: a check stopped unexpectedly: {}", p.0);
                return;
            }
            Some(Outcome::Cancelled) | None => return,
        };
        let Some(e) = self.entries.get_mut(&k.row) else {
            return;
        };
        let generation = match e.state {
            pool::EntryState::Resident { generation, .. } => Some(generation),
            pool::EntryState::Absent => None,
        };
        if e.version != k.version || generation != k.generation {
            return;
        }
        if same {
            e.validated = e.validated.max(k.covers);
            self.queue_check(&k.row);
            return;
        }
        log::info!(
            "chart images: {} changed since it was read",
            k.row.dir.display()
        );
        if let Some(e) = self.entries.remove(&k.row)
            && let pool::EntryState::Resident { generation, slot } = e.state
        {
            self.free_slot(generation, slot, e.version);
        }
        if self.cursor_row.as_ref() == Some(&k.row) {
            self.changed = true;
            self.rearm = true;
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
        self.check_finished();
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
                generation,
                slot,
                version,
            } => self.cursor_finished(id, cursor_generation, (generation, slot, version), outcome),
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
    /// `outcome` (SX5-SX8), its read admitted to slot `slot` of pool
    /// generation `generation` as read `version`: while that cursor and
    /// pool generation are current, a read with both images absent is an
    /// absent entry, its slot freed, and any other leaves its pool write to
    /// be sent ([`Core::send_write`]); a read stale, cancelled, panicked or
    /// failed part way frees its slot and leaves no entry, so the row is
    /// read again at a later rest.
    fn cursor_finished(
        &mut self,
        id: CursorId,
        cursor_generation: u64,
        (generation, slot, version): (u64, u16, u64),
        outcome: Option<Outcome<Arc<Output>>>,
    ) {
        let fits = match outcome {
            Some(Outcome::Done(out)) => match &*out {
                Output::Cursor {
                    fits,
                    #[cfg(test)]
                    counts,
                } => {
                    #[cfg(test)]
                    {
                        self.counts.0 += counts.0;
                        self.counts.1 += counts.1;
                    }
                    Some(Arc::clone(fits))
                }
                Output::Attempt(_) => None,
            },
            Some(Outcome::Panicked(p)) => {
                log::warn!("chart images: the worker stopped unexpectedly: {}", p.0);
                None
            }
            Some(Outcome::Cancelled) | None => None,
        };
        let current =
            cursor_generation == self.cursor_generation && generation == self.pool_generation;
        let read =
            [ChartImage::Stage, ChartImage::Banner].map(|w| fits.as_ref().map(|f| f.read(w)));
        let whole = read.iter().all(|r| r.is_some_and(reusable));
        let Some(fits) = fits.filter(|_| current && whole) else {
            self.free_slot(generation, slot, version);
            return;
        };
        log_absent(&fits);
        let [stage, _, banner] = fits.fitted();
        if stage.is_none() && banner.is_none() {
            self.free_slot(generation, slot, version);
            let bindings = [
                fits.read(ChartImage::Stage).binding.clone(),
                fits.read(ChartImage::Banner).binding.clone(),
            ];
            self.adopt_entry(
                id,
                pool::Entry::new(version, bindings, pool::EntryState::Absent),
            );
            return;
        }
        self.pending_write = Some(PendingWrite {
            generation,
            slot,
            row: id,
            version,
            fits,
            rx: None,
        });
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
    /// (SX1, SX20; design.md "Rendering and Skins", chart images): a
    /// request of an older cursor generation is dropped, and so is one
    /// whose row has an entry; a chart naming no image is an absent entry
    /// at once, with no job (SX2). With no live pool the request goes, and
    /// the pool's next generation asks for it again. Otherwise the row is
    /// admitted a slot no pin holds (a free one, else the least recently
    /// used, whose entry goes), and its job reads it into the slot's cells
    /// on its own subscriber; with every slot pinned the request waits. A
    /// job that cannot start frees its slot and leaves no entry (SX9).
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
        if request.cursor_generation != self.cursor_generation {
            return;
        }
        self.asked = Some(request.id.clone());
        if self.entries.get(&request.id).is_some() {
            return;
        }
        if request.id.names.iter().all(|n| *n == library::Name::None) {
            self.versions += 1;
            let entry = pool::Entry::new(
                self.versions,
                [
                    chart_image::Binding::NoHeader,
                    chart_image::Binding::NoHeader,
                ],
                pool::EntryState::Absent,
            );
            self.adopt_entry(request.id, entry);
            return;
        }
        let Some((slots, publish)) = self.slots.as_mut().zip(publish) else {
            return;
        };
        let generation = slots.layout().generation;
        // The pins: the slot the newest update sent names, and the one the
        // render thread displayed last, read now.
        let in_pool =
            |chart: Option<(u64, u16)>| chart.filter(|&(g, _)| g == generation).map(|(_, s)| s);
        let sent = self.sent.as_ref().and_then(|s| match s.chart {
            crate::chart_currency::CursorChart::Resident { generation, slot } => {
                Some((generation, slot))
            }
            _ => None,
        });
        let (_, displayed) = publish.pool_now();
        let pins = [
            in_pool(sent),
            in_pool(crate::chart_currency::CursorChart::unpack(displayed)),
        ];
        let Some((slot, evicted)) = slots.admit(pins) else {
            // Every slot pinned or written: the request waits for a frame.
            self.cursor = Some(request);
            return;
        };
        let cells = slots.layout().cells;
        self.versions += 1;
        let version = self.versions;
        slots.set(slot, pool::SlotState::Writing { version });
        if let Some(row) = evicted.and_then(|v| self.entries.by_version(v)).cloned() {
            self.entries.remove(&row);
        }
        let CursorRequest { id, subscriber, .. } = request;
        self.begun += 1;
        let key = JobKey {
            kind: JobKind::ChartImages,
            path: id.dir.clone(),
            speed: bms_model::Speed::NORMAL,
        };
        let read = id.clone();
        let spawned = coord.spawn_owned(
            spawner,
            key,
            (Class::Foreground, false, Owner::Foreground),
            subscriber,
            |_| {},
            move |ctx| Arc::new(run_cursor_job(&read, cells, &|| ctx.cancelled())),
        );
        match spawned {
            Ok(job) => {
                self.job = Some(Job {
                    target: Target::Cursor {
                        id,
                        cursor_generation: self.cursor_generation,
                        generation,
                        slot,
                        version,
                    },
                    job,
                });
            }
            Err(e) => {
                // A thread that cannot start: no entry, the run goes on.
                log::warn!("chart images: {e}");
                self.free_slot(generation, slot, version);
            }
        }
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
        if self.retiring.is_some() || self.pending_write.is_some() {
            return false;
        }
        let (reply, rx) = mpsc::sync_channel(1);
        publisher.publish(generation, None, set.clone(), reply);
        self.publication = Some(Publication::sent(generation, None, set, rx, false));
        true
    }

    /// The bytes it keeps: the sources, both halves and, while the render
    /// thread may still hold them, the published and the retiring set and
    /// the pool write's fits, each counted once. The selection's entries
    /// are bounded apart, by their accounted bytes.
    fn retained_bytes(&self) -> u64 {
        let mut seen: Vec<&Arc<ChartImages>> = Vec::new();
        let mut total = 0_u64;
        let sets = self
            .sources
            .iter()
            .map(|(_, _, s)| s)
            .chain(self.play.iter().map(|h| &h.fits))
            .chain(self.result.iter().map(|h| &h.fits))
            .chain(self.pending_write.iter().map(|p| &p.fits))
            .chain(
                [&self.publication, &self.retiring]
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
