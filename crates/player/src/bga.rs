//! The chart's BGA pictures at preparation (design.md "Rendering and
//! Skins", BGA; "Load Pipeline"; "Memory Budget"): one owned job of their
//! own kind reads and packs them and hands them to the render thread,
//! which writes them into its BGA array and publishes their table before
//! READY.
//!
//! - **Generations.** A new one at each choice, loading entry and retry
//!   ([`BgaWorker::renew`], [`BgaWorker::attempt`]) and when the chart is
//!   left ([`BgaWorker::forget`]), written at once where the render thread
//!   reads it: a step of another generation changes nothing there, so a
//!   job left running publishes nothing current.
//! - **The job** ([`run`]) holds one foreground permit, is cancelled
//!   through the choice's scope, and runs at most one at a time. Before it
//!   reads it states its transient ([`TERMS`]) to the reservation
//!   ([`reserve`]). It resolves every id by the source's priority order
//!   ([`bga::resolve`]) and reuses the array the last attempt published
//!   when every id resolves to the same unchanged file; otherwise it
//!   probes, packs by the ladder ([`bga::pack`]) and decodes one picture at
//!   a time into its cell. The cells go to the render thread in batches of
//!   at most [`BATCH`] bytes: a batch is written, acknowledged and its bytes
//!   dropped before the picture whose cell would overflow it is decoded, so
//!   a decode and one batch are all the job holds.
//! - **Absence.** A refused reservation or allocation, or a refused array
//!   or write, publishes every picture absent; a picture that cannot be
//!   read is absent alone. Play starts in every case.
//! - **Direct launch:** with no coordinator, the job runs on the game
//!   thread in the preparation.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use game::coord::{
    Class, Coordinator, JobKey, JobKind, Outcome, OwnedJob, Owner, Scope, Settled, Spawner,
};
use render::BgaFault;
use skin_model::bga::{self, BgaBinding, BgaCell, BgaEntry, BgaKey, BgaTable, CellPlace};
use skin_model::chart_image::{Absent, ChartDir, Identity};

use crate::render_side::BgaPublisher;

#[cfg(test)]
pub(crate) mod tests;

/// The transient the job states before it reads ("Memory Budget"): one
/// picture's whole-decode bound, as the chart images', and the cells in
/// hand, at most a whole page's bytes together ([`BATCH`]).
pub const TERMS: Terms = Terms {
    decode: 161 << 20,
    cell: bga::PAGE_SIDE as u64 * bga::PAGE_SIDE as u64 * 4,
};

/// A job's host transient, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terms {
    pub decode: u64,
    pub cell: u64,
}

/// The bytes of cells handed to the render thread in one step: the cell
/// term, a whole page's, so a batch never holds more than the largest cell
/// could.
pub const BATCH: u64 = TERMS.cell;

/// Batches are limited to this many bytes instead of [`BATCH`]; 0 for
/// [`BATCH`] (tests).
#[cfg(test)]
pub static BATCH_LIMIT: AtomicU64 = AtomicU64::new(0);

/// The reservation refuses every terms while set (tests).
#[cfg(test)]
pub static REFUSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The terms stated last (tests).
#[cfg(test)]
pub static STATED: Mutex<Option<Terms>> = Mutex::new(None);

/// The job's allocations are limited to this many bytes each; 0 for no
/// limit (tests: a refused allocation).
#[cfg(test)]
pub static ALLOC_LIMIT: AtomicU64 = AtomicU64::new(0);

/// Holds the job before its first cell until let go or cancelled (tests).
#[cfg(test)]
pub static HOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set while the job is held by [`HOLD`] (tests).
#[cfg(test)]
pub static HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The job fails while set (tests): [`FAIL_SPAWN`], its thread refused;
/// [`FAIL_PANIC`], it panics as it begins.
#[cfg(test)]
pub static FAIL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
#[cfg(test)]
pub const FAIL_SPAWN: u8 = 1;
#[cfg(test)]
pub const FAIL_PANIC: u8 = 2;

/// Each whole file read takes at least this many ms more (tests).
#[cfg(test)]
pub static READ_DELAY_MS: AtomicU64 = AtomicU64::new(0);

/// Whether the job's spawn is refused (tests: [`FAIL`]).
fn spawn_refused() -> bool {
    #[cfg(test)]
    {
        FAIL.load(Ordering::SeqCst) == FAIL_SPAWN
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// States `terms` to the reservation the ledger will answer (design.md
/// "Load Pipeline"): granted until the ledger exists.
#[must_use]
pub fn reserve(terms: Terms) -> bool {
    #[cfg(test)]
    {
        *STATED.lock().unwrap_or_else(PoisonError::into_inner) = Some(terms);
        if REFUSE.load(Ordering::SeqCst) {
            return false;
        }
    }
    let _ = terms;
    true
}

/// The allocation limit each picture is held to: one whole decode
/// ([`TERMS`]), what its read, decode and cell hold at once (skin-model
/// `ReadPicture::cell`).
fn limit() -> Option<u64> {
    #[cfg(test)]
    {
        let l = ALLOC_LIMIT.load(Ordering::SeqCst);
        if l > 0 {
            return Some(l);
        }
    }
    Some(TERMS.decode)
}

/// The bytes a batch may hold.
fn batch_limit() -> u64 {
    #[cfg(test)]
    {
        let l = BATCH_LIMIT.load(Ordering::SeqCst);
        if l > 0 {
            return l;
        }
    }
    BATCH
}

#[cfg(test)]
fn hold(cancel: &dyn Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while HOLD.load(Ordering::Acquire) && !cancel() && Instant::now() < end {
        HELD.store(true, Ordering::Release);
        std::thread::sleep(Duration::from_millis(1));
    }
    HELD.store(false, Ordering::Release);
}

/// What an attempt's BGA reads: the chart's folder and each `#BMPxx`
/// name by its id's index (none for a chart without BGA, or for a play
/// that does not show it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Want {
    pub dir: PathBuf,
    pub names: Vec<String>,
}

impl Want {
    /// The BGA of `chart`, read from `path`'s folder when `shown` (the
    /// session's answer, [`game::BgaSession::shown`]); nothing when not.
    #[must_use]
    pub fn of(path: &Path, chart: &bms_model::Chart, shown: bool) -> Self {
        Self {
            dir: crate::chart_images::folder(path),
            names: chart
                .bga
                .as_ref()
                .filter(|_| shown)
                .map_or_else(Vec::new, |b| {
                    b.images().iter().map(|i| i.name.clone()).collect()
                }),
        }
    }
}

/// A step of the publication, which the render thread answers on `reply`
/// ([`crate::render_side::RenderMsg::Bga`]).
pub enum BgaMsg {
    /// Begin an array of `pages` pages ([`render::Renderer::begin_bga`]).
    Begin {
        pages: u32,
        reply: mpsc::SyncSender<Result<BgaKey, BgaFault>>,
    },
    /// Write a batch of cells, each its place and texels
    /// ([`render::Renderer::write_bga_cells`]), then drop them.
    Cells {
        key: BgaKey,
        cells: Vec<(CellPlace, Vec<u8>)>,
        reply: mpsc::SyncSender<Result<(), BgaFault>>,
    },
    /// Publish the table ([`render::Renderer::publish_bga`]).
    Publish {
        key: Option<BgaKey>,
        entries: Vec<BgaEntry>,
        reply: mpsc::SyncSender<Publication>,
    },
    /// The table published for `key`'s array, if it is still the one in
    /// use ([`render::Renderer::published_bga`]).
    Reuse {
        key: BgaKey,
        reply: mpsc::SyncSender<Option<Arc<BgaTable>>>,
    },
    /// Drop the array and its table ([`render::Renderer::drop_bga`]).
    Drop,
}

/// The render thread's answer to [`BgaMsg::Publish`], and when it gave it:
/// an answer taken after its bound passed is one given before it only when
/// `at` says so.
#[derive(Debug)]
pub struct Publication {
    pub table: Result<Arc<BgaTable>, BgaFault>,
    pub at: Instant,
}

/// How a step's answer came.
enum Asked<T> {
    Answer(T),
    /// Not within the link's bound, or the render thread is gone.
    Unanswered,
}

/// Sends the step `make` builds and waits for its answer within the
/// link's bound.
fn ask<T>(
    link: &BgaPublisher,
    generation: u64,
    make: impl FnOnce(mpsc::SyncSender<T>) -> BgaMsg,
) -> Asked<T> {
    let (reply, rx) = mpsc::sync_channel(1);
    if !link.send(generation, make(reply)) {
        return Asked::Unanswered;
    }
    match rx.recv_timeout(link.bound()) {
        Ok(v) => Asked::Answer(v),
        Err(_) => Asked::Unanswered,
    }
}

/// How a job ended.
#[derive(Clone, Debug, PartialEq)]
pub enum End {
    /// Its table published, absent pictures included.
    Published(Arc<BgaTable>),
    /// The last attempt's array reused: nothing read.
    Reused(Arc<BgaTable>),
    /// Cancelled, or its generation gone: nothing current published.
    Stopped,
    /// The render thread did not answer within the link's bound.
    Unanswered,
}

/// What the last publication resolved, and the array it went into: the
/// key a later attempt reuses it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Kept {
    dir: PathBuf,
    bindings: Vec<BgaBinding>,
    key: BgaKey,
}

impl Kept {
    /// Whether `bindings` in `dir` observe the same files unchanged.
    fn same(&self, dir: &Path, bindings: &[BgaBinding]) -> bool {
        self.dir == dir
            && self.bindings.len() == bindings.len()
            && self
                .bindings
                .iter()
                .zip(bindings)
                .all(|(a, b)| a.same_as(b))
    }
}

/// The decoded cells not yet handed to the render thread.
#[derive(Default)]
struct Batch {
    cells: Vec<(CellPlace, Vec<u8>)>,
    /// Each cell's entry and the picture's native size.
    shown: Vec<(usize, CellPlace, [u32; 2])>,
    bytes: u64,
}

impl Batch {
    fn push(&mut self, index: usize, place: CellPlace, native: [u32; 2], texels: Vec<u8>) {
        self.bytes += cell_bytes(&place);
        self.cells.push((place, texels));
        self.shown.push((index, place, native));
    }
}

/// A cell's bytes at `place`, its gutter included.
fn cell_bytes(place: &CellPlace) -> u64 {
    let [w, h] = place.padded();
    u64::from(w) * u64::from(h) * 4
}

/// How a batch was handed over.
enum Handed {
    /// Written: its entries now name their cells.
    Written,
    /// Its generation or array is gone.
    Stale,
    Refused(BgaFault),
    Unanswered,
}

/// Hands `batch`'s cells to the render thread in one step, leaving it
/// empty, and enters each written cell in `entries`.
fn hand_over(
    link: &BgaPublisher,
    generation: u64,
    key: BgaKey,
    batch: &mut Batch,
    entries: &mut [BgaEntry],
) -> Handed {
    let cells = std::mem::take(&mut batch.cells);
    let shown = std::mem::take(&mut batch.shown);
    batch.bytes = 0;
    #[cfg(feature = "dev-tools")]
    crate::dev::stage("bga_upload_start");
    let written = ask(link, generation, |reply| BgaMsg::Cells {
        key,
        cells,
        reply,
    });
    #[cfg(feature = "dev-tools")]
    crate::dev::stage("bga_upload_end");
    match written {
        Asked::Answer(Ok(())) => {
            for (i, place, native) in shown {
                if let Some(e) = entries.get_mut(i) {
                    *e = BgaEntry::Picture(BgaCell {
                        uv: place.uv(),
                        native: bga::drawn_size(native),
                    });
                }
            }
            Handed::Written
        }
        Asked::Answer(Err(BgaFault::Stale)) => Handed::Stale,
        Asked::Answer(Err(e)) => Handed::Refused(e),
        Asked::Unanswered => Handed::Unanswered,
    }
}

/// A job's output.
#[derive(Clone, Debug)]
pub struct JobOut {
    generation: u64,
    end: End,
    /// What it published into an array, for reuse.
    kept: Option<Kept>,
    /// The cells it decoded.
    decoded: u64,
}

/// Reads `want`'s pictures for `generation` and hands them to the render
/// thread through `link`, reusing `prior`'s array when every id resolves to
/// the same unchanged file. Asks `cancel` throughout.
#[must_use]
pub(crate) fn run(
    want: &Want,
    generation: u64,
    prior: Option<&Kept>,
    link: &BgaPublisher,
    cancel: &dyn Fn() -> bool,
) -> JobOut {
    #[cfg(feature = "dev-tools")]
    crate::dev::stage("bga_start");
    #[cfg(test)]
    assert_ne!(
        FAIL.load(Ordering::SeqCst),
        FAIL_PANIC,
        "bga: a failing job"
    );
    let out = read_and_publish(want, generation, prior, link, cancel);
    #[cfg(feature = "dev-tools")]
    crate::dev::stage("bga_end");
    out
}

/// [`run`]'s work.
fn read_and_publish(
    want: &Want,
    generation: u64,
    prior: Option<&Kept>,
    link: &BgaPublisher,
    cancel: &dyn Fn() -> bool,
) -> JobOut {
    let mut out = JobOut {
        generation,
        end: End::Stopped,
        kept: None,
        decoded: 0,
    };
    let n = want.names.len();
    let publish =
        |key: Option<BgaKey>, entries: Vec<BgaEntry>| match ask(link, generation, |reply| {
            BgaMsg::Publish {
                key,
                entries,
                reply,
            }
        }) {
            Asked::Answer(Publication { table: Ok(t), .. }) => End::Published(t),
            Asked::Answer(Publication { table: Err(_), .. }) => End::Stopped,
            Asked::Unanswered => End::Unanswered,
        };
    let absent = || vec![BgaEntry::Absent; n];
    if n == 0 {
        out.end = publish(None, Vec::new());
        return out;
    }
    if !reserve(TERMS) {
        log::warn!("bga: its memory was not granted; the pictures are absent");
        out.end = publish(None, absent());
        return out;
    }
    let dir = match ChartDir::new(&want.dir) {
        Ok(dir) => dir,
        Err(e) => {
            log::warn!("bga: the chart's folder: {e}");
            out.end = publish(None, absent());
            return out;
        }
    };
    let identity: Identity<'_> = &crate::chart_images::file_identity;
    #[cfg(feature = "dev-tools")]
    crate::dev::stage("bga_reads_start");
    #[cfg(feature = "dev-tools")]
    let probing = Instant::now();
    let bindings: Vec<BgaBinding> = want
        .names
        .iter()
        .map(|name| bga::resolve(&dir, name, identity))
        .collect();
    if cancel() {
        return out;
    }
    if let Some(k) = prior.filter(|k| k.same(&want.dir, &bindings)) {
        match ask(link, generation, |reply| BgaMsg::Reuse {
            key: k.key,
            reply,
        }) {
            Asked::Answer(Some(t)) => {
                out.end = End::Reused(t);
                out.kept = Some(k.clone());
                return out;
            }
            // The array is gone: read again.
            Asked::Answer(None) => {}
            Asked::Unanswered => {
                out.end = End::Unanswered;
                return out;
            }
        }
    }
    let mut entries = absent();
    let mut natives: Vec<Option<[u32; 2]>> = vec![None; n];
    for ((b, entry), native) in bindings.iter().zip(&mut entries).zip(&mut natives) {
        if cancel() {
            return out;
        }
        match b {
            BgaBinding::Resolved { video: true, .. } => *entry = BgaEntry::Video,
            BgaBinding::Resolved { path, key, .. } => {
                *native = bga::probe(path, key, identity).ok();
            }
            BgaBinding::Unresolved(_) => {}
        }
    }
    // The headers read: each whole file read below ends the reads again.
    #[cfg(feature = "dev-tools")]
    {
        crate::dev::stage("bga_reads_end");
        crate::dev::stage_busy("bga_reads_busy", probing.elapsed());
    }
    #[cfg(feature = "dev-tools")]
    crate::dev::stage("bga_packing_start");
    let stored: Vec<Option<[u32; 2]>> = natives.iter().map(|s| s.map(bga::stored_size)).collect();
    let packing = bga::pack(&stored, bga::PAGE_BUDGET);
    #[cfg(feature = "dev-tools")]
    crate::dev::stage("bga_packing_end");
    if packing.pages == 0 {
        out.end = publish(None, entries);
        return out;
    }
    let pages = packing.pages;
    let key = match ask(link, generation, |reply| BgaMsg::Begin { pages, reply }) {
        Asked::Answer(Ok(k)) => k,
        Asked::Answer(Err(BgaFault::Stale)) => return out,
        Asked::Answer(Err(e)) => {
            log::warn!("bga: no array ({e}); the pictures are absent");
            out.end = publish(None, absent());
            return out;
        }
        Asked::Unanswered => {
            out.end = End::Unanswered;
            return out;
        }
    };
    #[cfg(test)]
    hold(cancel);
    // How a batch's handing over ends the job, if it does.
    let stop = |handed: Handed| match handed {
        Handed::Written => None,
        Handed::Stale => Some(End::Stopped),
        Handed::Refused(e) => {
            log::warn!("bga: a write was refused ({e}); the pictures are absent");
            Some(publish(None, absent()))
        }
        Handed::Unanswered => Some(End::Unanswered),
    };
    let limit_bytes = batch_limit();
    let mut batch = Batch::default();
    for (i, ((b, native), place)) in bindings
        .iter()
        .zip(&natives)
        .zip(&packing.cells)
        .enumerate()
    {
        let (
            BgaBinding::Resolved {
                path, key: file, ..
            },
            Some(native),
            Some(place),
        ) = (b, *native, *place)
        else {
            continue;
        };
        if cancel() {
            return out;
        }
        // The batch goes before the picture whose cell would overflow it
        // is decoded.
        if !batch.cells.is_empty()
            && batch.bytes + cell_bytes(&place) > limit_bytes
            && let Some(end) = stop(hand_over(link, generation, key, &mut batch, &mut entries))
        {
            out.end = end;
            return out;
        }
        // The reads' stage ends at the last file read whole and the
        // decodes' begins at the first decode; each read's and decode's
        // own time adds to its kind's busy time.
        #[cfg(feature = "dev-tools")]
        let reading = Instant::now();
        #[cfg(test)]
        std::thread::sleep(Duration::from_millis(READ_DELAY_MS.load(Ordering::SeqCst)));
        let read = bga::read_picture(path, file, native, cancel, identity, limit());
        #[cfg(feature = "dev-tools")]
        {
            crate::dev::stage("bga_reads_end");
            crate::dev::stage_busy("bga_reads_busy", reading.elapsed());
        }
        let decoded = read.and_then(|read| {
            #[cfg(feature = "dev-tools")]
            crate::dev::stage("bga_decodes_start");
            #[cfg(feature = "dev-tools")]
            let decoding = Instant::now();
            let cell = read.cell(place.content, cancel, limit());
            #[cfg(feature = "dev-tools")]
            {
                crate::dev::stage("bga_decodes_end");
                crate::dev::stage_busy("bga_decodes_busy", decoding.elapsed());
            }
            cell
        });
        let pixels = match decoded {
            Ok(p) => p,
            Err(Absent::Refused) => {
                log::warn!("bga: a picture's memory was refused; the pictures are absent");
                out.end = publish(None, absent());
                return out;
            }
            Err(Absent::Cancelled) => return out,
            Err(_) => continue,
        };
        out.decoded += 1;
        batch.push(i, place, native, pixels.texels);
    }
    if cancel() {
        return out;
    }
    if !batch.cells.is_empty()
        && let Some(end) = stop(hand_over(link, generation, key, &mut batch, &mut entries))
    {
        out.end = end;
        return out;
    }
    out.end = publish(Some(key), entries);
    if matches!(out.end, End::Published(_)) {
        out.kept = Some(Kept {
            dir: want.dir.clone(),
            bindings,
            key,
        });
    }
    out
}

/// Where the current generation's pictures stand for READY.
#[derive(Clone, Debug, PartialEq)]
pub enum Poll {
    /// Not published yet.
    Waiting,
    /// Published (or reused) for the current generation.
    Ready(Arc<BgaTable>),
    /// The render thread did not answer: the attempt is refused.
    Expired,
}

/// A job running, of the generation it was begun for.
struct Running {
    generation: u64,
    job: OwnedJob<Arc<JobOut>>,
}

struct Core {
    generation: Arc<AtomicU64>,
    link: BgaPublisher,
    want: Option<Want>,
    /// The current generation's pictures were drawn by an attempt that
    /// committed: the same want again is a retry.
    used: bool,
    job: Option<Running>,
    /// Jobs of an older generation, cancelled, until their threads end.
    obsolete: Vec<OwnedJob<Arc<JobOut>>>,
    /// How the current generation's job ended.
    done: Option<End>,
    /// The current generation's absent table (a want without pictures,
    /// or a job that could not start or panicked), sent from this thread,
    /// its answer not yet taken.
    absent: Option<AbsentAnswer>,
    kept: Option<Kept>,
    ended: bool,
    begun: u64,
    decoded: u64,
}

/// The render thread's answer to an absent table, taken by a later step
/// or poll, never waited for: the game thread sends it while the render
/// thread may be held (a loading frame awaiting its acknowledgement).
struct AbsentAnswer {
    rx: mpsc::Receiver<Publication>,
    /// The link's bound from the send: unanswered past it, an answer given
    /// after it included, however early it is taken.
    by: Instant,
}

/// The BGA pictures' worker: what the game thread asks, the one job, and
/// its end for READY.
#[derive(Clone)]
pub struct BgaWorker {
    core: Arc<Mutex<Core>>,
}

impl std::fmt::Debug for BgaWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let c = self.core();
        f.debug_struct("BgaWorker")
            .field("generation", &c.current())
            .field("want", &c.want)
            .field("job", &c.job.as_ref().map(|j| j.generation))
            .field("done", &c.done)
            .finish_non_exhaustive()
    }
}

/// The job's thread's bound at the run's end.
const STOP_WAIT: Duration = Duration::from_secs(2);

impl BgaWorker {
    /// A worker writing its generation into `generation`, which the render
    /// thread reads, and handing pictures through `link`.
    #[must_use]
    pub fn new(generation: Arc<AtomicU64>, link: BgaPublisher) -> Self {
        Self {
            core: Arc::new(Mutex::new(Core {
                generation,
                link,
                want: None,
                used: false,
                job: None,
                obsolete: Vec::new(),
                done: None,
                absent: None,
                kept: None,
                ended: false,
                begun: 0,
                decoded: 0,
            })),
        }
    }

    fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new generation: a choice or a loading entry. Written at once where
    /// the render thread reads it, so nothing of the last one is published
    /// from here; its job, if running, is left to end, its output never
    /// taken. What the last publication resolved stays, as the key the next
    /// attempt reuses its array by.
    pub fn renew(&self) {
        self.core().renew();
    }

    /// Asks for `want`'s pictures for the attempt being prepared. The same
    /// pictures after an attempt drew them (a retry) are a new generation:
    /// resolved again, the array reused when nothing changed.
    pub fn attempt(&self, want: Want) {
        let mut c = self.core();
        if c.ended {
            return;
        }
        if c.want.as_ref() != Some(&want) || c.used {
            c.renew();
        }
        c.want = Some(want);
    }

    /// The current pictures were drawn by an attempt that committed.
    pub fn committed(&self) {
        let mut c = self.core();
        c.used = c.done.is_some();
    }

    /// The chart is left: a new generation with nothing asked for, and the
    /// array dropped. Its job, if running, was cancelled through the
    /// choice's scope and is kept until its thread ends.
    pub fn forget(&self) {
        let mut c = self.core();
        c.renew();
        c.want = None;
        c.kept = None;
        let g = c.current();
        let _ = c.link.send(g, BgaMsg::Drop);
    }

    /// Selection is entered: nothing of the last chart's pictures stays.
    pub fn drop_images(&self) {
        self.forget();
    }

    /// Advances the worker without waiting for a job: takes what ended, and
    /// begins the job the current want needs once no job's thread is alive.
    /// A want without pictures has nothing to read: no job begins and no
    /// permit is taken, so the load beside it keeps them all; its empty
    /// table is sent here, as an absent table is for a job that cannot
    /// start, and the answer taken by a later step or poll.
    /// `coord`
    /// `None` (the direct launch) runs the job now on this thread, asking
    /// `cancel`.
    pub fn step(&self, coord: Option<(&Coordinator, Spawner)>, cancel: &dyn Fn() -> bool) {
        let mut c = self.core();
        c.take_finished();
        if c.ended
            || c.done.is_some()
            || c.job.is_some()
            || c.absent.is_some()
            || !c.obsolete.is_empty()
        {
            return;
        }
        let Some(want) = c.want.clone() else {
            return;
        };
        if want.names.is_empty() {
            c.publish_absent(0);
            return;
        }
        let generation = c.current();
        let prior = c.kept.clone();
        let link = c.link.clone();
        c.begun += 1;
        let Some((coord, spawner)) = coord else {
            let shared = Arc::clone(&c.generation);
            let out = run(&want, generation, prior.as_ref(), &link, &|| {
                cancel() || shared.load(Ordering::SeqCst) != generation
            });
            c.adopt(out);
            return;
        };
        let key = JobKey {
            kind: JobKind::BgaImages,
            path: want.dir.clone(),
            speed: bms_model::Speed::NORMAL,
        };
        let shared = Arc::clone(&c.generation);
        let n = want.names.len();
        let spawned = if spawn_refused() {
            Err(game::coord::LoadFailure {
                job: JobKind::BgaImages,
                cause: game::coord::Failure::ThreadStart("refused (tests)".to_owned()),
            })
        } else {
            coord.spawn_owned(
                spawner,
                key,
                (Class::Foreground, false, Owner::Foreground),
                coord.subscriber(Scope::Choice),
                |_| {},
                move |ctx| {
                    Arc::new(run(&want, generation, prior.as_ref(), &link, &|| {
                        ctx.cancelled() || shared.load(Ordering::SeqCst) != generation
                    }))
                },
            )
        };
        match spawned {
            Ok(job) => c.job = Some(Running { generation, job }),
            Err(e) => {
                // A thread that cannot start: the pictures absent, the run
                // goes on.
                log::warn!("bga: {e}");
                c.publish_absent(n);
            }
        }
    }

    /// Where the current generation's pictures stand, after taking what
    /// ended.
    #[must_use]
    pub fn poll(&self) -> Poll {
        let mut c = self.core();
        c.take_finished();
        match &c.done {
            Some(End::Published(t) | End::Reused(t)) => Poll::Ready(Arc::clone(t)),
            Some(End::Unanswered) => Poll::Expired,
            Some(End::Stopped) | None => Poll::Waiting,
        }
    }

    /// Whether no job's thread is alive (tests).
    #[cfg(test)]
    #[must_use]
    pub fn idle(&self) -> bool {
        let c = self.core();
        c.job.is_none() && c.obsolete.is_empty()
    }

    /// The current generation (tests).
    #[cfg(test)]
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.core().current()
    }

    /// Whether the current job's thread ended, its end not yet taken
    /// (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    #[must_use]
    pub fn job_ended(&self) -> bool {
        self.core().job.as_ref().is_some_and(|j| j.job.finished())
    }

    /// Jobs begun and cells decoded (tests).
    #[cfg(test)]
    #[must_use]
    pub fn counts(&self) -> (u64, u64) {
        let c = self.core();
        (c.begun, c.decoded)
    }

    /// At the run's end: nothing begins from here, and every job is
    /// stopped and joined within [`STOP_WAIT`], or left to the process's
    /// exit, owning what it holds.
    pub fn end(&self) {
        let mut c = self.core();
        c.ended = true;
        c.want = None;
        let deadline = Instant::now() + STOP_WAIT;
        let jobs: Vec<_> = c
            .job
            .take()
            .map(|j| j.job)
            .into_iter()
            .chain(std::mem::take(&mut c.obsolete))
            .collect();
        for job in jobs {
            let bound = deadline.saturating_duration_since(Instant::now());
            match job.settle(bound, &Instant::now, &mut || Some(())) {
                Settled::Joined {
                    after_panic: Some(p),
                    ..
                } => log::warn!("bga: the worker stopped unexpectedly: {}", p.0),
                Settled::Joined { .. } => {}
                Settled::Abandoned { .. } => {
                    log::warn!("bga: the worker was left to end with the process");
                }
            }
        }
    }
}

impl Core {
    fn current(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn renew(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.done = None;
        self.absent = None;
        self.used = false;
        if let Some(j) = self.job.take() {
            self.obsolete.push(j.job);
        }
    }

    /// Sends the current generation's table of `n` pictures absent (none:
    /// the empty table), never waiting: its answer is taken by
    /// [`take_finished`](Self::take_finished), unanswered past the link's
    /// bound.
    fn publish_absent(&mut self, n: usize) {
        let (reply, rx) = mpsc::sync_channel(1);
        let message = BgaMsg::Publish {
            key: None,
            entries: vec![BgaEntry::Absent; n],
            reply,
        };
        if self.link.send(self.current(), message) {
            self.absent = Some(AbsentAnswer {
                rx,
                by: Instant::now() + self.link.bound(),
            });
        } else {
            self.done = Some(End::Unanswered);
        }
        self.kept = None;
    }

    /// Takes the absent table's answer once it came, or once the link's
    /// bound passed without it. An answer given within the bound counts
    /// however late it is taken (other loading work may delay the step);
    /// one given after it is unanswered.
    fn take_absent(&mut self) {
        let Some(absent) = &self.absent else {
            return;
        };
        let answer = match absent.rx.try_recv() {
            Ok(a) if a.at >= absent.by => End::Unanswered,
            Ok(Publication { table: Ok(t), .. }) => End::Published(t),
            Ok(Publication { table: Err(_), .. }) => End::Stopped,
            Err(mpsc::TryRecvError::Empty) if Instant::now() < absent.by => return,
            Err(_) => End::Unanswered,
        };
        self.done = Some(answer);
        self.absent = None;
    }

    /// Takes the jobs that ended: an older generation's are settled and
    /// dropped; the current one's output adopted.
    fn take_finished(&mut self) {
        self.take_absent();
        let (ended, live): (Vec<_>, Vec<_>) = std::mem::take(&mut self.obsolete)
            .into_iter()
            .partition(OwnedJob::finished);
        self.obsolete = live;
        for job in ended {
            settle_logged(job);
        }
        let Some(running) = self.job.take() else {
            return;
        };
        if !running.job.finished() {
            self.job = Some(running);
            return;
        }
        let outcome = running.job.poll().map(|o| o.outcome);
        settle_logged(running.job);
        if running.generation != self.current() {
            return;
        }
        match outcome {
            Some(Outcome::Done(out)) => self.adopt((*out).clone()),
            Some(Outcome::Panicked(p)) => {
                log::warn!("bga: the worker stopped unexpectedly: {}", p.0);
                let n = self.want.as_ref().map_or(0, |w| w.names.len());
                self.publish_absent(n);
            }
            // Cancelled: the next step begins it again while it is still
            // asked for.
            Some(Outcome::Cancelled) | None => {}
        }
    }

    /// The current generation's job ended with `out`.
    fn adopt(&mut self, out: JobOut) {
        self.decoded += out.decoded;
        if out.generation != self.current() || out.end == End::Stopped {
            return;
        }
        match out.end {
            End::Published(_) | End::Reused(_) => self.kept = out.kept,
            End::Unanswered | End::Stopped => {}
        }
        self.done = Some(out.end);
    }
}

fn settle_logged(job: OwnedJob<Arc<JobOut>>) {
    if let Settled::Joined {
        after_panic: Some(p),
        ..
    } = job.settle(STOP_WAIT, &Instant::now, &mut || Some(()))
    {
        log::warn!("bga: the worker stopped unexpectedly: {}", p.0);
    }
}
