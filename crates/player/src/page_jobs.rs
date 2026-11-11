//! The practice page's own jobs (design.md "State Model", the practice
//! page; "Load Pipeline"): the chosen chart's 1× bank and its loudness
//! measurement as owned jobs ([`game::coord::OwnedJob`]), begun for the
//! choice and going on as speculative work on the page.
//!
//! - **Their thread's mode** follows the class they compute in
//!   ([`JobMode`]): it enters Windows background mode at a speculative
//!   class and leaves it at the foreground, each call checked. A refused
//!   entry runs no more of the job and is logged once; a refused exit is
//!   logged once and the job goes on in the foreground at background
//!   priority. A call that unwinds is the job's failure (its coordinator
//!   keeps it).
//! - **They move at their own checkpoints**: the bank's decode at each of
//!   its own thread's files ([`crate::selection::decode_on_permits`]), its
//!   other workers retiring at their next file once the foreground is not
//!   wanted; the measurement before each block.
//! - **Their stops are their coordinator's and their mode's alone**: the
//!   job's cancel and its owner's stop (`Ctx::cancelled`), and a refused
//!   entry. They read none of the run's controls, so a Start's quit, which
//!   ends only the Start, never cancels them.
//!
//!
//! [`PageJobs`] begins them when the practice page opens, advances them on
//! each pass of the page, hands them to a Start, and settles them when the
//! chart is left or the run ends.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bms_model::{Chart, Speed};
use game::coord::{
    Class, Coordinator, Ctx, Failure, JobKey, JobKind, LoadFailure, Observed, Outcome, OwnedJob,
    Owner, Scope, Settled, Spawner, Subscriber,
};
use library::LoudnessKey;

use crate::background::Claim;
use crate::shelf::Shelf;

use crate::priority::ThreadMode;

/// What the page's 1× bank job gives: the bank, or why there is none (a
/// cap, or its cancel).
pub type PageBank = Result<Arc<audio::SoundBank>, audio::BankError>;

/// What the page's measurement job gives: the measurement, none when the
/// chart plays at the headroom, or its cancel.
pub type PageMeasure = Result<Option<audio::Measurement>, crate::loudness::Cancelled>;

/// The bank's account in the witness, from before its job begins to its
/// registration (dev-tools); nothing otherwise.
#[cfg(feature = "dev-tools")]
pub type PageToken = Option<crate::bank_witness::DecodeToken>;
/// The bank's account in the witness (dev-tools only).
#[cfg(not(feature = "dev-tools"))]
pub type PageToken = ();

/// Where the page bank's decode reports its files: nowhere, or a Start's
/// loading state while the Start waits for it ([`PageJobs::show`]).
pub type PageSink = Arc<Mutex<Option<Box<dyn Fn(usize, usize) + Send>>>>;

/// Where a page job calls its test probe (tests).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PageAt {
    /// A decode's worker asks before a file: its index (0 is the job's own
    /// thread).
    File(usize),
    /// The bank's decode asks its cancel (before each file, packet and
    /// chunk).
    Cancel,
    /// That ask said stop.
    Stopped,
    /// The measurement asks before a block.
    Block,
    /// The bank's decode made its bank.
    Decoded,
    /// A Start promoted the page's job of this kind, on the game thread,
    /// before it waits for it.
    Promoted(JobKind),
}

/// A page job's test probe (tests).
#[cfg(test)]
pub type PageProbe = Arc<dyn Fn(PageAt) + Send + Sync>;

/// A page job thread's mode, changed on the job's thread at each change of
/// the class it computes in (its coordinator's class hook).
pub struct JobMode {
    mode: Arc<dyn ThreadMode>,
    /// What the job is, for its log line.
    what: String,
    /// The thread is in background mode.
    background: AtomicBool,
    /// An entry was refused: the job runs no more.
    refused: AtomicBool,
    /// A refused call was logged.
    logged: AtomicBool,
    /// Refused calls, and lines logged for them (tests).
    #[cfg(test)]
    refusals: (
        std::sync::atomic::AtomicUsize,
        std::sync::atomic::AtomicUsize,
    ),
}

impl std::fmt::Debug for JobMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobMode")
            .field("what", &self.what)
            .field("refused", &self.refused())
            .finish_non_exhaustive()
    }
}

impl JobMode {
    /// The mode of a job doing `what` (its log lines begin with it), through
    /// `mode`; its thread starts in the normal mode.
    pub fn new(mode: Arc<dyn ThreadMode>, what: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            mode,
            what: what.into(),
            background: AtomicBool::new(false),
            refused: AtomicBool::new(false),
            logged: AtomicBool::new(false),
            #[cfg(test)]
            refusals: Default::default(),
        })
    }

    /// The class hook: background mode entered at a speculative class, left
    /// at the foreground. A refused entry marks the job refused; a refused
    /// exit leaves it in the foreground at background priority. Either is
    /// logged once.
    pub fn on_class(&self, class: Class) {
        let background = self.background.load(Ordering::Relaxed);
        match class {
            Class::Foreground if background => match self.mode.leave_background() {
                Ok(()) => self.background.store(false, Ordering::Relaxed),
                Err(e) => self.refusal(&format!(
                    "{}: background mode could not be left ({e}); it goes on at background priority",
                    self.what
                )),
            },
            Class::Speculative | Class::Maintenance if !background && !self.refused() => {
                match self.mode.enter_background() {
                    Ok(()) => self.background.store(true, Ordering::Relaxed),
                    Err(e) => {
                        self.refused.store(true, Ordering::Relaxed);
                        self.refusal(&format!(
                            "{}: background mode refused ({e}); it runs no background work",
                            self.what
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    /// Whether an entry was refused: the job runs no more speculative work.
    pub fn refused(&self) -> bool {
        self.refused.load(Ordering::Relaxed)
    }

    /// Logs a refused call, the first one only.
    fn refusal(&self, line: &str) {
        #[cfg(test)]
        self.refusals.0.fetch_add(1, Ordering::Relaxed);
        if !self.logged.swap(true, Ordering::Relaxed) {
            #[cfg(test)]
            self.refusals.1.fetch_add(1, Ordering::Relaxed);
            crate::loudness::log_line(line, true);
        }
    }

    /// The lines logged for refused calls, and the refused calls (tests).
    #[cfg(test)]
    pub fn refusals(&self) -> (usize, usize) {
        (
            self.refusals.1.load(Ordering::Relaxed),
            self.refusals.0.load(Ordering::Relaxed),
        )
    }
}

/// Begins the 1× bank of `chart` (at `path`) at `rate` as an owned job of
/// base class speculative for `subscriber`, promoted from the start when
/// `promoted`, answered for by `owner`; its thread's mode through `mode`.
///
/// # Errors
///
/// As [`Coordinator::spawn_owned`]: its key runs, or its thread could not
/// start.
pub fn spawn_page_bank(
    coord: &Coordinator,
    spawner: Spawner,
    subscriber: Subscriber,
    (path, chart, rate): (PathBuf, Arc<Chart>, u32),
    (promoted, owner): (bool, Owner),
    (mode, sink, token): (Arc<JobMode>, PageSink, PageToken),
    #[cfg(test)] probe: Option<PageProbe>,
) -> Result<OwnedJob<PageBank>, LoadFailure> {
    let key = JobKey {
        kind: JobKind::Bank,
        path,
        speed: Speed::NORMAL,
    };
    let hook = Arc::clone(&mode);
    let pool = coord.clone();
    coord.spawn_owned(
        spawner,
        key,
        (Class::Speculative, promoted, owner),
        subscriber,
        move |class| hook.on_class(class),
        move |ctx: &Ctx<'_>| {
            #[cfg(test)]
            let at = |p: PageAt| {
                if let Some(probe) = &probe {
                    probe(p);
                }
            };
            #[cfg(test)]
            let at_file = |worker: usize| at(PageAt::File(worker));
            let cancel = || {
                #[cfg(test)]
                at(PageAt::Cancel);
                let stop = ctx.cancelled() || mode.refused();
                #[cfg(test)]
                if stop {
                    at(PageAt::Stopped);
                }
                stop
            };
            let progress = |done: usize, files: usize| {
                if let Some(show) = &*sink.lock().unwrap_or_else(PoisonError::into_inner) {
                    show(done, files);
                }
            };
            let bank = crate::selection::decode_on_permits(
                ctx,
                &pool,
                (&chart, rate, Speed::NORMAL),
                &progress,
                &cancel,
                #[cfg(test)]
                crate::selection::DecodeProbe {
                    load: None,
                    at_file: Some(&at_file),
                },
            )
            .map(Arc::new);
            #[cfg(test)]
            if bank.is_ok() {
                at(PageAt::Decoded);
            }
            // Accounted from before the job began to its bank's
            // registration, before it is published (dev-tools).
            #[cfg(feature = "dev-tools")]
            if let (Some(token), Ok(bank)) = (token, &bank) {
                token.register(bank);
            }
            #[cfg(not(feature = "dev-tools"))]
            let () = token;
            bank
        },
    )
}

/// Begins the measurement of `chart` (at `path`, named `name` in the log)
/// over its 1× `bank` as an owned job of base class speculative for
/// `subscriber`, promoted from the start when `promoted`, answered for by
/// `owner`; its thread's mode through `mode`.
///
/// # Errors
///
/// As [`Coordinator::spawn_owned`]: its key runs, or its thread could not
/// start.
pub fn spawn_page_measure(
    coord: &Coordinator,
    spawner: Spawner,
    subscriber: Subscriber,
    (path, chart, bank, name): (PathBuf, Arc<Chart>, Arc<audio::SoundBank>, String),
    (promoted, owner): (bool, Owner),
    mode: Arc<JobMode>,
    #[cfg(test)] probe: Option<PageProbe>,
) -> Result<OwnedJob<PageMeasure>, LoadFailure> {
    let key = JobKey {
        kind: JobKind::Measure,
        path,
        speed: Speed::NORMAL,
    };
    let hook = Arc::clone(&mode);
    coord.spawn_owned(
        spawner,
        key,
        (Class::Speculative, promoted, owner),
        subscriber,
        move |class| hook.on_class(class),
        move |ctx: &Ctx<'_>| {
            let mut host = PageHost {
                ctx,
                mode: &mode,
                #[cfg(test)]
                probe: probe.as_ref(),
            };
            crate::loudness::measure(&chart, &bank, None, &name, &mut host)
        },
    )
}

/// What the page's measurement lends the analysis, on the job's thread.
struct PageHost<'a, 'c> {
    ctx: &'a Ctx<'c>,
    mode: &'a JobMode,
    #[cfg(test)]
    probe: Option<&'a PageProbe>,
}

impl crate::loudness::Host for PageHost<'_, '_> {
    fn at_normal(&mut self) -> anyhow::Result<(Chart, Arc<audio::SoundBank>)> {
        anyhow::bail!("the page measures its 1× bank only")
    }

    fn measuring(&mut self, _name: &str) {}

    /// Its own block-boundary checkpoint: the permit moves to the class the
    /// job wants (the mode changed with it); it stops at its cancel, its
    /// owner's stop, a class with no permit, or a refused entry.
    fn cancelled(&mut self) -> bool {
        #[cfg(test)]
        if let Some(probe) = self.probe {
            probe(PageAt::Block);
        }
        self.ctx.checkpoint() || self.mode.refused()
    }
}

/// The chart the practice page shows, as its read gave it at 1×.
#[derive(Clone, Debug)]
pub struct PageChart {
    /// The chart's file: the jobs' key.
    pub path: PathBuf,
    /// The chart at 1×: a Start at another speed reads its own.
    pub chart: Arc<Chart>,
    /// Its name in the log.
    pub name: String,
    /// The output's rate, which the bank is decoded at.
    pub rate: u32,
}

/// What a page job's published outcome means for the page.
#[derive(Debug)]
pub enum Taken<T> {
    /// Its value.
    Done(T),
    /// It gave nothing the page keeps: its own cancel, a refused mode, or
    /// a panic the page may redo (not responsible, not the foreground's):
    /// a Start does it again.
    Redo,
    /// A panic the run ends with: the foreground's, or one a Start was
    /// responsible for (design.md "Load Pipeline").
    Fatal(LoadFailure),
}

/// What `observed` of a page job of `kind` means for the page.
pub fn taken<T: Clone>(kind: JobKind, observed: &Observed<T>) -> Taken<T> {
    match &observed.outcome {
        Outcome::Done(v) => Taken::Done(v.clone()),
        Outcome::Cancelled => Taken::Redo,
        Outcome::Panicked(p) if observed.owner == Owner::Foreground || observed.responsible => {
            Taken::Fatal(LoadFailure {
                job: kind,
                cause: Failure::Panicked(p.clone()),
            })
        }
        Outcome::Panicked(p) => {
            crate::loudness::log_line(
                &format!("practice: a page job stopped unexpectedly ({p}); a Start does it again"),
                true,
            );
            Taken::Redo
        }
    }
}

/// The run's end, a close or a thread's failure: the cause that bounds a
/// settle ([`PageJobs::settle`]).
#[derive(Clone, Copy, Debug)]
pub struct Terminal;

/// How long a settle with a terminal cause waits for the jobs, in all
/// (design.md "Play Lifecycle", Who loads).
pub const STOP_WAIT: Duration = crate::shelf::STOP_TIMEOUT;

/// The practice page's jobs for the chosen chart (design.md "State Model",
/// the practice page): its 1× bank and, on a lookup miss, its loudness
/// measurement, begun at background priority as the page opens and
/// advanced on each pass of the page with no input. A Start takes them:
/// at 1× it installs the page's bank, at another speed it measures on it
/// first. They are settled when the chart is left, before another bank is
/// decoded, and at the run's end.
pub struct PageJobs {
    /// The chart, from the read; none when no practice page is open.
    chart: Option<PageChart>,
    /// The 1× bank's job until it is taken.
    bank: Option<OwnedJob<PageBank>>,
    /// The measurement's job until it is taken.
    measure: Option<OwnedJob<PageMeasure>>,
    /// The 1× bank, once published or decoded by a Start, kept for a
    /// Start at 1×.
    kept: Option<Arc<audio::SoundBank>>,
    /// The 1× bank's loudness key, when its lookup missed.
    key: Option<LoudnessKey>,
    /// The 1× bank's loudness key, hit or miss: the sounds the known gain
    /// is of.
    bank_key: Option<LoudnessKey>,
    /// The gain needs no measurement: looked up, taken from the
    /// background, or measured. A failed measurement leaves it unknown.
    gain_known: bool,
    /// The page has tried its one measurement: a failure or an own cancel
    /// is left to the Start, and the page does not measure again.
    measure_tried: bool,
    /// A page job's panic the run ends with, published while it was
    /// settled: kept until the caller takes it.
    fatal: Option<LoadFailure>,
    /// The chart is claimed from the background ([`Shelf::claim`]).
    claimed: bool,
    /// An ordinary refusal of the 1× bank, kept for the Start.
    refusal: Option<audio::BankError>,
    /// The thread mode the jobs change at their class.
    mode: Arc<dyn crate::priority::ThreadMode>,
    /// Where the bank's decode reports its files.
    sink: PageSink,
    /// The settle's clock (tests replace it).
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    /// The jobs' probe (tests).
    #[cfg(test)]
    pub probe: Option<PageProbe>,
}

impl std::fmt::Debug for PageJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageJobs")
            .field("chart", &self.chart.as_ref().map(|c| &c.path))
            .field("bank", &self.bank.is_some())
            .field("measure", &self.measure.is_some())
            .field("kept", &self.kept.is_some())
            .field("gain_known", &self.gain_known)
            .finish_non_exhaustive()
    }
}

impl PageJobs {
    /// No page, its jobs' threads in `mode`.
    pub fn new(mode: Arc<dyn crate::priority::ThreadMode>) -> Self {
        Self {
            chart: None,
            bank: None,
            measure: None,
            kept: None,
            key: None,
            bank_key: None,
            gain_known: false,
            measure_tried: false,
            fatal: None,
            claimed: false,
            refusal: None,
            mode,
            sink: PageSink::default(),
            clock: Arc::new(Instant::now),
            #[cfg(test)]
            probe: None,
        }
    }

    /// The settle's clock (tests).
    #[cfg(test)]
    pub fn set_clock(&mut self, clock: Arc<dyn Fn() -> Instant + Send + Sync>) {
        self.clock = clock;
    }

    /// The chart the page shows, when it is `path`'s.
    pub fn chart_of(&self, path: &Path) -> Option<&PageChart> {
        self.chart.as_ref().filter(|c| c.path == path)
    }

    /// Whether the gain needs no measurement.
    pub const fn gain_known(&self) -> bool {
        self.gain_known
    }

    /// The 1× bank's loudness key, when its lookup missed.
    pub const fn key(&self) -> Option<LoudnessKey> {
        self.key
    }

    /// The 1× bank kept for a Start at 1×.
    pub fn kept(&self) -> Option<&Arc<audio::SoundBank>> {
        self.kept.as_ref()
    }

    /// The page opens for `chart`: its 1× bank begins on `coord` as an
    /// owned job of base class speculative, unpromoted, answered for by
    /// the page (`Owner::Speculative`). A refused thread leaves the decode
    /// to the Start.
    pub fn begin(
        &mut self,
        coord: &Coordinator,
        spawner: Spawner,
        chart: PageChart,
        #[cfg(feature = "dev-tools")] witness: &Arc<crate::bank_witness::BankWitness>,
    ) {
        debug_assert!(self.bank.is_none() && self.measure.is_none() && self.kept.is_none());
        let mode = JobMode::new(Arc::clone(&self.mode), format!("sounds: {}", chart.name));
        match spawn_page_bank(
            coord,
            spawner,
            coord.subscriber(Scope::Choice),
            (chart.path.clone(), Arc::clone(&chart.chart), chart.rate),
            (false, Owner::Speculative),
            (
                mode,
                Arc::clone(&self.sink),
                #[cfg(feature = "dev-tools")]
                Some(witness.decode()),
                #[cfg(not(feature = "dev-tools"))]
                (),
            ),
            #[cfg(test)]
            self.probe.clone(),
        ) {
            Ok(job) => self.bank = Some(job),
            Err(e) => crate::loudness::log_line(
                &format!(
                    "practice: the page cannot decode {} ({e}); a Start does",
                    chart.name
                ),
                true,
            ),
        }
        self.chart = Some(chart);
    }

    /// Advances the jobs with no input (each pass of the page): a published
    /// bank is kept and looked up, and on a miss its measurement begins
    /// unless the background has the chart ([`Shelf::claim`]); a published
    /// measurement is kept on the shelf. A failure the page may redo is
    /// left to the Start.
    ///
    /// # Errors
    ///
    /// A published panic the run ends with ([`Taken::Fatal`]).
    pub fn poll(
        &mut self,
        coord: &Coordinator,
        spawner: Spawner,
        shelf: &mut Shelf,
    ) -> Result<(), LoadFailure> {
        let Some(chart) = self.chart.clone() else {
            return Ok(());
        };
        // Each job's outcome is the one observed here: its settle adds none.
        if let Some(observed) = self.bank.as_ref().and_then(OwnedJob::poll) {
            let job = self.bank.take();
            let _ = self.settle_one(JobKind::Bank, job, &mut || None);
            match taken(JobKind::Bank, &observed) {
                Taken::Done(Ok(bank)) => self.keep_bank(shelf, &chart, bank),
                Taken::Done(Err(audio::BankError::Cancelled)) | Taken::Redo => {}
                Taken::Done(Err(e)) => self.refusal = Some(e),
                Taken::Fatal(f) => return Err(f),
            }
        }
        if let Some(observed) = self.measure.as_ref().and_then(OwnedJob::poll) {
            let job = self.measure.take();
            let _ = self.settle_one(JobKind::Measure, job, &mut || None);
            match taken(JobKind::Measure, &observed) {
                Taken::Done(Ok(m)) => self.keep_measured(shelf, m),
                // Left to the Start, which measures in the foreground.
                Taken::Done(Err(crate::loudness::Cancelled)) | Taken::Redo => {
                    self.measure_tried = true;
                }
                Taken::Fatal(f) => return Err(f),
            }
        }
        self.claim_or_measure(coord, spawner, shelf, &chart);
        Ok(())
    }

    /// The 1× `bank`, published or decoded by a Start: kept, and looked up.
    pub fn keep_bank(&mut self, shelf: &mut Shelf, chart: &PageChart, bank: Arc<audio::SoundBank>) {
        // A bank without a digest is never cached and plays at the
        // headroom: nothing to measure.
        self.key = shelf.look_up_page(&chart.chart, &bank, &chart.name);
        self.bank_key = crate::loudness::key(&chart.chart, &bank);
        self.gain_known = self.key.is_none();
        self.kept = Some(bank);
    }

    /// The 1× bank's loudness key, hit or miss: the sounds the known gain is
    /// of.
    pub const fn bank_key(&self) -> Option<LoudnessKey> {
        self.bank_key
    }

    /// The measurement `m` of the kept key, kept on the shelf. None (it
    /// failed) keeps nothing and leaves the gain unknown, so the next
    /// Start measures again; the page does not.
    pub fn keep_measured(&mut self, shelf: &mut Shelf, m: Option<audio::Measurement>) {
        let measured = m.is_some();
        if let Some(key) = self.key {
            shelf.keep_measured(key, m);
        }
        self.gain_known = measured;
        self.measure_tried = true;
    }

    /// The gain known is of sounds since replaced: nothing of the 1× bank
    /// or its gain is kept, so the Start measures the sounds as they are.
    pub fn forget(&mut self) {
        self.kept = None;
        self.key = None;
        self.bank_key = None;
        self.gain_known = false;
        self.measure_tried = false;
    }

    /// A page job's panic the run ends with, published while it was
    /// settled, taken.
    pub fn take_fatal(&mut self) -> Option<LoadFailure> {
        self.fatal.take()
    }

    /// On a miss with no measurement running: the chart claimed from the
    /// background, then measured by the page unless the background has it
    /// or had it measured already.
    fn claim_or_measure(
        &mut self,
        coord: &Coordinator,
        spawner: Spawner,
        shelf: &mut Shelf,
        chart: &PageChart,
    ) {
        let (Some(key), Some(bank)) = (self.key, self.kept.clone()) else {
            return;
        };
        if self.gain_known || self.measure.is_some() || self.measure_tried {
            return;
        }
        match self.claim(shelf, &chart.path, &key) {
            // A reused result is kept by the claim.
            Claim::Reused(_) | Claim::Running | Claim::Settling => {}
            Claim::Taken | Claim::Free => {
                let mode =
                    JobMode::new(Arc::clone(&self.mode), format!("loudness: {}", chart.name));
                match spawn_page_measure(
                    coord,
                    spawner,
                    coord.subscriber(Scope::Choice),
                    (
                        chart.path.clone(),
                        Arc::clone(&chart.chart),
                        bank,
                        chart.name.clone(),
                    ),
                    (false, Owner::Speculative),
                    mode,
                    #[cfg(test)]
                    self.probe.clone(),
                ) {
                    Ok(job) => self.measure = Some(job),
                    Err(e) => crate::loudness::log_line(
                        &format!(
                            "practice: the page cannot measure {} ({e}); a Start does",
                            chart.name
                        ),
                        true,
                    ),
                }
            }
        }
    }

    /// The gain was found without the page: no Start measures it.
    pub const fn known(&mut self) {
        self.gain_known = true;
    }

    /// Whether the page's measurement runs (tests).
    #[cfg(test)]
    pub const fn measuring(&self) -> bool {
        self.measure.is_some()
    }

    /// Whether the page's bank job runs (tests).
    #[cfg(test)]
    pub const fn decoding(&self) -> bool {
        self.bank.is_some()
    }

    /// Says `at` to the jobs' probe, on the game thread (tests).
    #[cfg(test)]
    pub fn probe(&self, at: PageAt) {
        if let Some(probe) = &self.probe {
            probe(at);
        }
    }

    /// The chart claimed from the background under `key`, the claim
    /// remembered for the leave.
    pub fn claim(&mut self, shelf: &mut Shelf, path: &Path, key: &LoudnessKey) -> Claim {
        let claim = shelf.claim(path, key);
        if matches!(claim, Claim::Taken | Claim::Free) {
            self.claimed = true;
        }
        if matches!(claim, Claim::Reused(_)) {
            self.gain_known = true;
        }
        claim
    }

    /// The bank's job, for a Start to join.
    pub fn take_bank(&mut self) -> Option<OwnedJob<PageBank>> {
        self.bank.take()
    }

    /// A joined bank's job the Start gave up (a quit): the page keeps it.
    pub fn put_bank(&mut self, job: OwnedJob<PageBank>) {
        self.bank = Some(job);
    }

    /// The measurement's job, for a Start to join.
    pub fn take_measure(&mut self) -> Option<OwnedJob<PageMeasure>> {
        self.measure.take()
    }

    /// A joined measurement the Start gave up (a quit): the page keeps it.
    pub fn put_measure(&mut self, job: OwnedJob<PageMeasure>) {
        self.measure = Some(job);
    }

    /// The refusal kept for the Start, taken.
    pub fn take_refusal(&mut self) -> Option<audio::BankError> {
        self.refusal.take()
    }

    /// While `show` is set, the bank's decode reports its files to it.
    pub fn show(&self, show: Option<Box<dyn Fn(usize, usize) + Send>>) {
        *self.sink.lock().unwrap_or_else(PoisonError::into_inner) = show;
    }

    /// Settles `job` of `kind`, if any, as one of [`settle`](Self::settle)'s:
    /// a panic it publishes meanwhile that the run ends with.
    pub fn settle_one<T: Clone>(
        &self,
        kind: JobKind,
        job: Option<OwnedJob<T>>,
        terminal: &mut dyn FnMut() -> Option<Terminal>,
    ) -> Option<LoadFailure> {
        let mut first = None;
        job.and_then(|job| self.settle_job(kind, job, &mut first, terminal))
    }

    /// Settles one job of `kind` against the shared deadline: [`STOP_WAIT`]
    /// from `first`, the first cause's time, once one was seen. `terminal`
    /// is asked at every slice. A panic it published that the run ends
    /// with (a Start was responsible for it) is returned, not dropped.
    fn settle_job<T: Clone>(
        &self,
        kind: JobKind,
        job: OwnedJob<T>,
        first: &mut Option<Instant>,
        terminal: &mut dyn FnMut() -> Option<Terminal>,
    ) -> Option<LoadFailure> {
        let clock = Arc::clone(&self.clock);
        let now = move || clock();
        let bound = first.map_or(STOP_WAIT, |at| {
            STOP_WAIT.saturating_sub(now().saturating_duration_since(at))
        });
        let mut cause = || {
            if first.is_some() {
                return Some(Terminal);
            }
            let c = terminal();
            if c.is_some() {
                *first = Some(now());
            }
            c
        };
        let observed = match job.settle(bound, &now, &mut cause) {
            Settled::Joined { observed, .. } => observed,
            Settled::Abandoned { observed, .. } => {
                crate::loudness::log_line(
                    "practice: a page job did not stop in time and is left",
                    true,
                );
                observed.map(|o| (*o).clone())
            }
        };
        match observed.map(|o| taken(kind, &o)) {
            Some(Taken::Fatal(f)) => Some(f),
            _ => None,
        }
    }

    /// Settles every job of the page against one shared deadline, the
    /// first terminal cause's time plus [`STOP_WAIT`] (with no cause the
    /// threads are waited for), and drops the kept bank unless `keep_bank`
    /// (a Start at 1× installs it). The page's chart stays.
    pub fn settle(&mut self, keep_bank: bool, terminal: &mut dyn FnMut() -> Option<Terminal>) {
        let mut first = None;
        if let Some(job) = self.bank.take()
            && let Some(f) = self.settle_job(JobKind::Bank, job, &mut first, terminal)
        {
            self.fatal.get_or_insert(f);
        }
        if let Some(job) = self.measure.take()
            && let Some(f) = self.settle_job(JobKind::Measure, job, &mut first, terminal)
        {
            self.fatal.get_or_insert(f);
        }
        if !keep_bank {
            self.kept = None;
        }
    }

    /// The chart is left (Back, another choice, the run's end): every job
    /// settled as [`settle`](Self::settle), the bank dropped, the claim
    /// given back to the background, nothing of the page kept but a fatal
    /// panic published meanwhile ([`take_fatal`](Self::take_fatal)).
    pub fn leave(
        &mut self,
        shelf: Option<&mut Shelf>,
        terminal: &mut dyn FnMut() -> Option<Terminal>,
    ) {
        self.settle(false, terminal);
        if let (Some(chart), Some(shelf)) = (&self.chart, shelf)
            && self.claimed
        {
            shelf.unclaim(&chart.path);
        }
        self.chart = None;
        self.key = None;
        self.bank_key = None;
        self.gain_known = false;
        self.measure_tried = false;
        self.claimed = false;
        self.refusal = None;
        self.show(None);
    }
}

#[cfg(test)]
mod tests;
