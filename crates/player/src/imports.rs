//! The seam between a configured skin (`player.toml` `[skins.<kind>]`) and
//! the render thread (design.md "beatoraja Lua Skins", "Screen skins"): a
//! kind's build from its choice to a bundle, the thread a build runs on,
//! and the reports of its outcome.
//!
//! - [`build`] is a pure function of its inputs: the folder, the header,
//!   the configuration named against it, the import, the atlas and the
//!   bundle, with its text producer only when the skin has rasterized
//!   text. It installs nothing. A Lua play skin whose base import reads a
//!   chart fact ends pending ([`KindOutcome::Pending`]), with what each
//!   attempt's [`specialize`] starts from; [`facts`] derives an attempt's
//!   facts.
//! - [`run_job`] runs one job on a spawned `skin-import` thread with owned
//!   inputs. A completed outcome is returned only once the thread has
//!   ended, so every VM of the job is dropped. Cancelling the attempt
//!   cancels the job and waits for its thread's end without a bound;
//!   ending the run cancels it and waits at most [`STOP_WAIT`], and a job
//!   past it is abandoned to the process's exit, its late outcome dropped
//!   on its own thread. [`run_cancellable`] is it with the run's end
//!   only.
//! - [`startup`] imports and selects each configured kind of a launch
//!   path, one kind after another, before the first screen.
//! - The reports: the stdout line of each kind ([`line`]), the log's
//!   summary of an installed skin ([`log_installed`]), and the select
//!   screen's notice of a screen kind other than play ([`notice`]).
//!
//! Skins are imported at startup only, but for a pending play skin,
//! built for each attempt that its kept specialization does not fit
//! ([`PendingSkin`]); startup marks it pending on the render thread.

use std::fmt::Write as _;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use game::{Chain, Preparation, SkinChoice, SkinChoices};
use render::{ClipTable, DefaultFont, OutlineError, OutlineText, SkinBundle};
use skin_beatoraja::{
    Cancel, Config, Definition, Fact, FactDeps, FactId, Header, ImportError, ImportFacts, Imported,
    LuaHeader, LuaOutcome, LuaUsage, Note, NoteLoc, Report, Rule, Usage,
};
use skin_model::{AtlasLimits, FontReader, Layout, Root, ScreenKind, Side};

use crate::log_file;
use crate::render_side::{RenderLink, SELECT_SLICE};
use crate::ui_text::{Label, Message};

pub mod facts;

/// How long a stopped job is waited for before it is abandoned: the
/// background work's stop bound.
pub const STOP_WAIT: Duration = crate::shelf::STOP_TIMEOUT;

/// The refusal of a job that panicked: the panic is caught on its thread
/// and sent as this.
pub const PANICKED: &str = "import failed";
/// The refusal of a job whose thread ended without sending its outcome.
pub const ENDED: &str = "import thread ended";
/// The refusal of a job stopped between its phases.
pub const STOPPED: &str = "the import was stopped";

/// What a build packs for: the renderer's atlas limits and the window's
/// target size, and where the default font of rasterized text comes from.
#[derive(Clone, Copy, Debug)]
pub struct ImportTarget {
    pub limits: AtlasLimits,
    pub size: [u32; 2],
    /// The default font ([`DefaultFont::runtime`] in production), read only
    /// for a skin with rasterized text.
    pub default_font: fn() -> Result<Arc<DefaultFont>, OutlineError>,
}

/// How a kind's build ended.
#[derive(Debug)]
pub enum KindOutcome {
    /// Built: the bundle to select, and the import's report.
    Bundle(Arc<SkinBundle>, Summary),
    /// The kind has no importer yet: its screen stays built in, for the
    /// reason given.
    BuiltIn(String),
    /// Refused, with the located reason: nothing is selected.
    Refused(String),
    /// A Lua play skin whose base import read a fact the base snapshot
    /// has pending: what each attempt's [`specialize`] starts from, and
    /// the base import's report.
    Pending(Arc<PendingPlay>, PendingNote),
}

/// A pending play skin, built for each attempt ([`specialize`]): the
/// header pass's result, the configuration named against its header, and
/// the target startup packed for. Owned, with no Lua state.
#[derive(Debug)]
pub struct PendingPlay {
    header: LuaHeader,
    config: Config,
    target: ImportTarget,
}

/// What a pending base import reports: the fact that stopped it, and the
/// files and the sandbox's caps it used so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingNote {
    pub fact: FactId,
    pub dependencies: Vec<String>,
    pub usage: LuaUsage,
}

/// `fact` by name.
const fn fact_name(fact: FactId) -> &'static str {
    match fact {
        FactId::Autoplay => "autoplay",
        FactId::JudgeClass => "the chart's judgment class",
        FactId::PlayLevel => "the chart's play level",
    }
}

impl PendingNote {
    /// The refusal of a skin that cannot be built before a chart is
    /// chosen, of a kind other than play, which has no specialization.
    pub fn refusal(&self) -> String {
        format!(
            "the skin reads {} as it is built, which is known only once a chart is \
             chosen; such skins are not installed yet",
            fact_name(self.fact)
        )
    }
}

/// A pending play skin built for an attempt's facts: the bundle, the
/// import's report, and the facts it read, which say whether it fits
/// another attempt ([`FactDeps::holds`]).
#[derive(Debug)]
pub struct Specialization {
    pub bundle: Arc<SkinBundle>,
    pub summary: Summary,
    pub facts: FactDeps,
}

/// A pending play skin as the game thread keeps it through the run: what
/// each specialization starts from, and the one specialization kept,
/// reused while its facts hold.
#[derive(Debug)]
pub struct PendingSkin {
    play: Arc<PendingPlay>,
    cached: Option<Specialization>,
}

/// The play skin of an attempt at a pending play skin
/// ([`PendingSkin::attempt`]).
#[derive(Debug)]
pub enum ForAttempt {
    /// The specialization to prepare the attempt with.
    Ready(Arc<SkinBundle>),
    /// The attempt's quit, the window's close or a thread's failure came
    /// during the specialization: the attempt is cancelled.
    Cancelled,
    /// The skin cannot be built for the attempt, with the reason.
    Refused(String),
}

/// How a specialization's job ran, for its reports.
#[derive(Debug)]
pub enum Specialized {
    /// The kept specialization fits the attempt's facts: no job ran.
    Reused,
    /// A job ran, ending as the attempt's skin says.
    Ran,
}

impl ForAttempt {
    /// Prints and logs the attempt's skin, as it came `how`:
    /// `skin: play: specialized`, `skin: play: reused` or `skin: play:
    /// refused for the chart: <reason>`; nothing when cancelled.
    pub fn report(&self, how: &Specialized) {
        let line = match (self, how) {
            (Self::Ready(_), Specialized::Reused) => "skin: play: reused".to_owned(),
            (Self::Ready(_), Specialized::Ran) => "skin: play: specialized".to_owned(),
            (Self::Refused(why), _) => format!("skin: play: refused for the chart: {why}"),
            (Self::Cancelled, _) => return,
        };
        println!("{line}");
        log::info!("{line}");
    }
}

/// Logs how the render thread's preparation of an attempt with a
/// specialization ended (`answer`: staged and uploaded, refused,
/// cancelled, or the screen's failure) and the time `took` from the
/// request to its end.
pub fn log_staged(took: Duration, answer: &Result<Preparation, crate::render_side::ScreenError>) {
    let ms = took.as_millis();
    match answer {
        Ok(Preparation::Ready) => log::info!("skin: play: staged with the attempt in {ms} ms"),
        Ok(Preparation::Refused(why)) => {
            log::info!("skin: play: its staging was refused in {ms} ms: {why}");
        }
        Ok(Preparation::Cancelled) => {
            log::info!("skin: play: its staging was cancelled after {ms} ms");
        }
        Err(e) => log::info!("skin: play: its staging failed after {ms} ms: {}", Chain(e)),
    }
}

impl PendingSkin {
    /// `play`, no specialization kept yet.
    pub const fn new(play: Arc<PendingPlay>) -> Self {
        Self { play, cached: None }
    }

    /// The skin of an attempt with `facts` (design.md "beatoraja Lua
    /// Skins", import facts):
    ///
    /// - The specialization kept is reused while it holds for `facts`
    ///   ([`FactDeps::holds`]): no job and no Lua.
    /// - Else the one kept is dropped first, so at most the committed
    ///   bundle and one candidate exist, `wait` shows the wait, and
    ///   [`specialize`] runs on a job ([`run_job`]): `quit` (the attempt's
    ///   quit key) cancels it and waits for its thread's end, and
    ///   `end_run` (a close, a thread's failure) cancels it and waits at
    ///   most [`STOP_WAIT`]. Its specialization is kept, whatever signal
    ///   came, since it fits its facts.
    /// - Once either signal came the attempt is cancelled; a refusal,
    ///   a panic, a silent end or a thread that could not start refuses
    ///   it, with the reason.
    pub fn attempt(
        &mut self,
        facts: ImportFacts,
        wait: impl FnOnce(),
        mut quit: impl FnMut() -> bool,
        mut end_run: impl FnMut() -> bool,
    ) -> (ForAttempt, Specialized) {
        if let Some(kept) = self.cached.as_ref().filter(|c| c.facts.holds(&facts)) {
            return (
                ForAttempt::Ready(Arc::clone(&kept.bundle)),
                Specialized::Reused,
            );
        }
        drop(self.cached.take());
        wait();
        let signaled = std::cell::Cell::new(false);
        let play = Arc::clone(&self.play);
        let from = Instant::now();
        let ran = run_job(
            move |cancel: &Cancel| specialize(&play, &facts, cancel),
            || {
                let seen = quit();
                signaled.set(signaled.get() || seen);
                seen
            },
            || {
                let seen = end_run();
                signaled.set(signaled.get() || seen);
                seen
            },
        );
        let took = from.elapsed();
        let skin = match ran {
            Ran::Done(Ok(s)) => {
                let bundle = Arc::clone(&s.bundle);
                let atlas = Atlas {
                    texels: bundle.atlas_texels(),
                    side: bundle.side(),
                };
                log_specialized(&s.summary, &s.facts, took, atlas, log::logger());
                self.cached = Some(s);
                ForAttempt::Ready(bundle)
            }
            Ran::Done(Err(why)) | Ran::Refused(why) => ForAttempt::Refused(format!(
                "the play skin cannot be built for this chart: {why}"
            )),
            Ran::Abandoned => ForAttempt::Cancelled,
        };
        let skin = if signaled.get() {
            ForAttempt::Cancelled
        } else {
            skin
        };
        (skin, Specialized::Ran)
    }
}

/// An import's report as the log shows it: the definition files read, the
/// file choices by name, the counters and the notes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// The definition files read, the entry first.
    pub sources: Vec<String>,
    /// Each file choice decided: its name, the candidate taken and how.
    pub files: Vec<ChosenFile>,
    pub usage: Usage,
    pub notes: Vec<Note>,
    /// Notes the import counted only.
    pub suppressed: u32,
}

/// A file choice's outcome, by the header's names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChosenFile {
    /// The choice's name, or `#<index>` when it has none.
    pub choice: String,
    /// The candidate taken; `None` when its files are missing.
    pub candidate: Option<String>,
    pub rule: Rule,
}

impl Summary {
    /// `report`, its file choices read against `header`, the header it
    /// was imported with.
    fn of(report: Report, header: &Header) -> Self {
        let files = report
            .files
            .iter()
            .map(|f| {
                let spec = header.files.get(usize::from(f.choice));
                ChosenFile {
                    choice: spec
                        .and_then(|s| s.name.clone())
                        .unwrap_or_else(|| format!("#{}", f.choice)),
                    candidate: f
                        .candidate
                        .and_then(|c| spec.and_then(|s| s.candidates.get(usize::from(c)).cloned())),
                    rule: f.rule,
                }
            })
            .collect();
        Self {
            sources: report.sources,
            files,
            usage: report.usage,
            notes: report.notes,
            suppressed: report.suppressed,
        }
    }

    /// Where `at` lies: `<file>:<line>:<col>`.
    fn locate(&self, at: NoteLoc) -> String {
        let file = self
            .sources
            .get(usize::from(at.source))
            .map_or("?", String::as_str);
        format!("{file}:{}:{}", at.line, at.col)
    }
}

/// The settings' name of `kind`, as its `[skins.<kind>]` table has it.
pub const fn name(kind: ScreenKind) -> &'static str {
    match kind {
        ScreenKind::Play => "play",
        ScreenKind::Selection => "selection",
        ScreenKind::Decision => "decision",
        ScreenKind::Result => "result",
    }
}

/// Builds kind `kind`'s skin from `choice` for `target`, with autoplay as
/// `autoplay`, asking `cancel` between its phases, in its Lua passes and
/// in its atlas. A pure function of its inputs: it installs nothing.
///
/// 1. The folder: `choice.root`, the user's own absolute choice, must be
///    an existing folder; it is not checked as a name.
/// 2. The header: `choice.entry`, Lua when it ends in `.luaskin` or `.lua`,
///    else JSON, read under the folder's name and containment rules, as
///    every file the skin names is. A skin of another kind refuses.
/// 3. The configuration: each option and file choice by name, against the
///    header; an unknown name refuses.
/// 4. The import, Lua against the base facts; a kind with no importer
///    keeps its built-in screen, and a Lua play skin reading a chart fact
///    ends pending, built for each attempt ([`specialize`]).
/// 5. The pack ([`pack`]).
pub fn build(
    kind: ScreenKind,
    choice: &SkinChoice,
    autoplay: bool,
    target: &ImportTarget,
    cancel: &Cancel,
) -> KindOutcome {
    match built(kind, choice, autoplay, target, cancel) {
        Ok(Built::Bundle(b, summary)) => KindOutcome::Bundle(b, summary),
        Ok(Built::NoImporter(why)) => KindOutcome::BuiltIn(why),
        Ok(Built::Pending(p, note)) => KindOutcome::Pending(p, note),
        Err(why) => KindOutcome::Refused(why),
    }
}

/// What [`built`] made.
enum Built {
    Bundle(Arc<SkinBundle>, Summary),
    NoImporter(String),
    Pending(Arc<PendingPlay>, PendingNote),
}

/// The header pass's result, by format.
enum Pass {
    Json(Definition, Header),
    Lua(LuaHeader),
}

impl Pass {
    fn header(&self) -> &Header {
        match self {
            Self::Json(_, h) => h,
            Self::Lua(h) => h.header(),
        }
    }
}

/// [`STOPPED`] once `cancel` is cancelled.
fn stopped(cancel: &Cancel) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err(STOPPED.to_owned())
    } else {
        Ok(())
    }
}

fn built(
    kind: ScreenKind,
    choice: &SkinChoice,
    autoplay: bool,
    target: &ImportTarget,
    cancel: &Cancel,
) -> Result<Built, String> {
    let stopped = || stopped(cancel);
    let text = |e: &ImportError| Chain(e).to_string();
    // 1. The folder.
    Root::new(&choice.root).map_err(|e| {
        format!(
            "the skin folder {} cannot be used: {}",
            choice.root.display(),
            Chain(&e)
        )
    })?;
    stopped()?;
    // 2. The header.
    let root = choice.root.as_path();
    let lua = {
        let entry = choice.entry.to_ascii_lowercase();
        entry.ends_with(".luaskin") || entry.ends_with(".lua")
    };
    let pass = if lua {
        Pass::Lua(
            skin_beatoraja::read_lua_header(root, &choice.entry, cancel).map_err(|e| text(&e))?,
        )
    } else {
        let (def, header) =
            skin_beatoraja::read_header(root, &choice.entry).map_err(|e| text(&e))?;
        Pass::Json(def, header)
    };
    let found = pass.header().kind;
    if found != kind {
        return Err(text(&ImportError::Slot {
            at: None,
            slot: kind,
            found,
        }));
    }
    stopped()?;
    // 3. The configuration.
    let config = config(pass.header(), choice)?;
    // 4. The import.
    let header = pass.header().clone();
    let imported: Result<Imported, ImportError> = match pass {
        Pass::Json(mut def, _) => skin_beatoraja::import(&mut def, &header, &config, kind),
        Pass::Lua(h) => {
            let facts = ImportFacts::base(kind, Fact::Known(autoplay));
            match h.import(&config, &facts, cancel) {
                Ok(LuaOutcome::Imported(i)) => Ok(i.imported),
                Ok(LuaOutcome::Pending(p)) => {
                    let note = PendingNote {
                        fact: p.pending,
                        dependencies: p.dependencies,
                        usage: p.usage,
                    };
                    // Only play's configured pass runs: another kind
                    // has no importer yet, so it never gets here.
                    if kind != ScreenKind::Play {
                        return Err(note.refusal());
                    }
                    let pending = PendingPlay {
                        header: h,
                        config,
                        target: *target,
                    };
                    return Ok(Built::Pending(Arc::new(pending), note));
                }
                Err(e) => Err(e),
            }
        }
    };
    let imported = match imported {
        Ok(i) => i,
        Err(e @ ImportError::NoImporter { .. }) => return Ok(Built::NoImporter(text(&e))),
        Err(e) => return Err(text(&e)),
    };
    stopped()?;
    // 5. The pack.
    let (bundle, summary) = pack(imported, &header, target, cancel)?;
    Ok(Built::Bundle(bundle, summary))
}

/// Builds `pending` for an attempt's `facts`, asking `cancel` between its
/// phases, in its Lua pass and in its atlas: the configured pass from the
/// header pass's state ([`LuaHeader::import`]), then the pack for the
/// target startup took ([`pack`]). A pure function of its inputs, the
/// skin's files included: it installs nothing.
///
/// # Errors
///
/// The located reason the import or the pack refuses, and a read of a
/// fact `facts` leaves pending.
pub fn specialize(
    pending: &PendingPlay,
    facts: &ImportFacts,
    cancel: &Cancel,
) -> Result<Specialization, String> {
    let imported = match pending.header.import(&pending.config, facts, cancel) {
        Ok(LuaOutcome::Imported(i)) => i,
        Ok(LuaOutcome::Pending(p)) => {
            return Err(format!(
                "the skin reads {}, which the attempt's facts leave pending",
                fact_name(p.pending)
            ));
        }
        Err(e) => return Err(Chain(&e).to_string()),
    };
    stopped(cancel)?;
    let header = pending.header.header();
    let (bundle, summary) = pack(imported.imported, header, &pending.target, cancel)?;
    Ok(Specialization {
        bundle,
        summary,
        facts: imported.facts,
    })
}

/// The pack of `imported`, imported with `header`: the variant for
/// `target`, the atlas within its limits, and the bundle, with its text
/// producer only when the skin has rasterized text; `cancel` asked
/// between its phases and in the atlas.
fn pack(
    imported: Imported,
    header: &Header,
    target: &ImportTarget,
    cancel: &Cancel,
) -> Result<(Arc<SkinBundle>, Summary), String> {
    let stopped = || stopped(cancel);
    let summary = Summary::of(imported.report, header);
    let skin = imported.skin;
    let variant = skin.variant(target.size, &Layout::AUTHOR);
    let atlas = skin
        .atlas(variant, target.limits, &|| cancel.is_cancelled())
        .map_err(|e| format!("the atlas cannot be built: {}", Chain(&e)))?;
    stopped()?;
    let producer = if skin.has_outline_text() {
        let mut reader = FontReader::new(skin.sheet());
        let mut fonts = Vec::new();
        for i in skin.outline_fonts() {
            let bytes = reader
                .read(i)
                .map_err(|e| format!("a font cannot be read: {}", Chain(&e)))?;
            fonts.push((i, bytes));
            stopped()?;
        }
        let default = (target.default_font)()
            .map_err(|e| format!("the default font cannot be used: {}", Chain(&e)))?;
        let producer = OutlineText::new(&skin, fonts, &default)
            .map_err(|e| format!("the skin's text cannot be set: {}", Chain(&e)))?;
        Some(Arc::new(producer))
    } else {
        None
    };
    let scratch = imported.scratch.unwrap_or(Side::Left);
    let bundle = SkinBundle::new(Arc::new(skin), atlas, ClipTable::default(), scratch)
        .map_err(|e| format!("the skin cannot be bundled: {}", Chain(&e)))?;
    let bundle = match producer {
        Some(p) => bundle
            .with_text(p)
            .map_err(|e| format!("the skin cannot be bundled: {}", Chain(&e)))?,
        None => bundle,
    };
    Ok((Arc::new(bundle), summary))
}

/// The configuration `choice` names against `header`: each named option's
/// item and each named file choice's candidate, the rest the header's
/// defaults ([`Config::defaults`]); no offsets.
///
/// # Errors
///
/// The first name the header does not have, named.
fn config(header: &Header, choice: &SkinChoice) -> Result<Config, String> {
    Config::named(
        header,
        choice.options.iter().map(|(o, i)| (o.as_str(), i.as_str())),
        choice.files.iter().map(|(f, c)| (f.as_str(), c.as_str())),
    )
}

/// How a job on its own thread ended, as [`run_job`] returns it.
#[derive(Debug)]
pub enum Ran<T> {
    /// The job's outcome, its thread ended.
    Done(T),
    /// No outcome, its thread ended: it panicked ([`PANICKED`]), ended
    /// without a message ([`ENDED`]), or could not start.
    Refused(String),
    /// The run ended, and the job's thread had not ended within
    /// [`STOP_WAIT`]: it is left to the process's exit, and its outcome,
    /// if it comes, is dropped there. Only while the run ends.
    Abandoned,
}

/// [`run_job`] with the run's end only: `stop` is the window's close.
pub fn run_cancellable<T: Send + 'static>(
    job: impl FnOnce(&Cancel) -> T + Send + 'static,
    stop: impl FnMut() -> bool,
) -> Ran<T> {
    run_job(job, || false, stop)
}

/// Runs `job` on a spawned `skin-import` thread with what it owns, the
/// calling thread waiting in [`SELECT_SLICE`] slices.
///
/// - Completion means the thread has ended: once the outcome is in, the
///   wait goes on until the thread has finished, then joins it.
/// - Once `cancel_attempt` turns true (the attempt's quit), the job is
///   cancelled and the wait goes on, without a bound, until its thread
///   ends; it never abandons the job. `end_run` is still asked meanwhile.
/// - Once `end_run` turns true (the window closing, a thread's failure),
///   before or after `cancel_attempt`, the job is cancelled and the thread
///   waited for at most [`STOP_WAIT`] from then, an outcome already
///   received included; past it, [`Ran::Abandoned`].
/// - A panic in `job` is caught on its thread and sent as
///   [`PANICKED`]; a thread that ends without sending is [`ENDED`]; a
///   thread that cannot start refuses at once.
///
/// [`STOP_WAIT`] bounds the job's body, up to the thread's
/// `is_finished`; the join after it is not bounded. A thread whose
/// thread-local destructors block would hold it, which no job here has
/// (waiting on the native thread's end needs platform calls).
///
/// Under dev-tools, each job started counts in [`jobs_started`] and each
/// join in [`jobs_joined`]: a job that could not start counts in
/// neither, and an abandoned one only in the first.
pub fn run_job<T: Send + 'static>(
    job: impl FnOnce(&Cancel) -> T + Send + 'static,
    mut cancel_attempt: impl FnMut() -> bool,
    mut end_run: impl FnMut() -> bool,
) -> Ran<T> {
    let cancel = Cancel::new();
    let (tx, rx) = mpsc::sync_channel::<Result<T, &'static str>>(1);
    let theirs = cancel.clone();
    let body = move || {
        #[cfg(test)]
        let _worker = tests::WorkerEnd;
        let sent = match panic::catch_unwind(AssertUnwindSafe(|| job(&theirs))) {
            Ok(outcome) => Ok(outcome),
            Err(payload) => {
                // Dropped before the send: a payload whose drop panics
                // ends the thread without a message.
                drop(payload);
                Err(PANICKED)
            }
        };
        // Past an abandonment nobody receives: the outcome is dropped
        // here, on this thread.
        let _ = tx.send(sent);
        #[cfg(test)]
        tests::exit_gate();
    };
    let handle = match spawn(body) {
        Ok(h) => h,
        Err(e) => return Ran::Refused(format!("the import thread cannot start: {e}")),
    };
    #[cfg(feature = "dev-tools")]
    JOBS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut deadline: Option<Instant> = None;
    let mut outcome: Option<Ran<T>> = None;
    let mut attempt_cancelled = false;
    loop {
        if deadline.is_none() && end_run() {
            cancel.cancel();
            deadline = Some(Instant::now() + STOP_WAIT);
        }
        if deadline.is_none() && !attempt_cancelled && cancel_attempt() {
            cancel.cancel();
            attempt_cancelled = true;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            // Detached: the handle and the receiver go, so a late outcome
            // is dropped on its own thread.
            return Ran::Abandoned;
        }
        match outcome {
            None => match rx.recv_timeout(SELECT_SLICE) {
                Ok(Ok(done)) => outcome = Some(Ran::Done(done)),
                Ok(Err(why)) => outcome = Some(Ran::Refused(why.to_owned())),
                Err(RecvTimeoutError::Disconnected) => {
                    outcome = Some(Ran::Refused(ENDED.to_owned()));
                }
                Err(RecvTimeoutError::Timeout) => {}
            },
            Some(done) if handle.is_finished() => {
                let _ = handle.join();
                #[cfg(feature = "dev-tools")]
                JOINED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return done;
            }
            Some(_) => thread::sleep(SELECT_SLICE),
        }
    }
}

/// Starts `body` on a `skin-import` thread; a test can have the start
/// refused.
fn spawn(body: impl FnOnce() + Send + 'static) -> std::io::Result<thread::JoinHandle<()>> {
    #[cfg(test)]
    if tests::spawn_refused() {
        return Err(std::io::Error::other("refused by the test"));
    }
    #[cfg(test)]
    tests::worker_starting();
    let spawned = thread::Builder::new()
        .name("skin-import".to_owned())
        .spawn(body);
    #[cfg(test)]
    if spawned.is_err() {
        tests::spawn_failed();
    }
    spawned
}

/// What became of a kind at startup, for its line and the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shown {
    /// Its skin is installed.
    Installed,
    /// Its screen stays built in, for the reason.
    BuiltIn(String),
    /// Its skin was refused, for the reason.
    Refused(String),
    /// Its skin is pending: it reads this chart fact as it is built, and
    /// is built for each chart before its play.
    Pending(FactId),
}

/// The stdout line of `kind`: `skin: <kind>: installed`, `skin: <kind>:
/// built-in: <reason>`, `skin: <kind>: refused: <reason>` or `skin:
/// <kind>: pending: <what it reads>`.
pub fn line(kind: ScreenKind, shown: &Shown) -> String {
    let kind = name(kind);
    match shown {
        Shown::Installed => format!("skin: {kind}: installed"),
        Shown::Pending(fact) => format!(
            "skin: {kind}: pending: reads {} as it is built; built for each chart before its play",
            fact_name(*fact)
        ),
        Shown::BuiltIn(why) => format!("skin: {kind}: built-in: {why}"),
        Shown::Refused(why) => format!("skin: {kind}: refused: {why}"),
    }
}

/// The select screen's notice of a refused or built-in kind other than
/// play: one line naming the kind and the reason. Play's refusal is its
/// admission notice instead ([`crate::render_side::SkinState`]).
pub fn notice(kind: ScreenKind, why: &str) -> String {
    let screen = match kind {
        ScreenKind::Play => Label::ScreenPlay,
        ScreenKind::Selection => Label::ScreenSelection,
        ScreenKind::Decision => Label::ScreenDecision,
        ScreenKind::Result => Label::ScreenResult,
    };
    Message::SkinBuiltIn { screen, why }.text()
}

/// An installed bundle's atlas array: its level-0 texels
/// ([`SkinBundle::atlas_texels`]) and its pages' side.
#[derive(Clone, Copy, Debug)]
pub struct Atlas {
    pub texels: u64,
    pub side: u32,
}

/// Logs an installed skin of `kind`: one line with its atlas's texels, page
/// side and layers, and the texels every kind's committed skin is charged
/// together against the
/// shared limit, its counters, then its definition files and file
/// choices, the lists last since a record is cut at the log's line cap;
/// then its notes, located, at most the log's cap, and a count of the rest
/// (design.md "Log File"), into `log` (the run's, [`log::logger`]).
pub fn log_installed(
    kind: ScreenKind,
    summary: &Summary,
    atlas: Atlas,
    committed: u64,
    log: &dyn log::Log,
) {
    let Atlas { texels, side } = atlas;
    let square = u64::from(side).saturating_mul(u64::from(side)).max(1);
    let head = format!(
        "installed; atlas {texels} texels (side {side}, {} layers), committed {committed} \
         of {} texels",
        texels / square,
        render::SHARED_TEXELS,
    );
    log_summary(kind, &head, summary, log);
}

/// Logs a pending play skin's specialization for an attempt, as an
/// installed skin's summary ([`log_installed`]) after one line with the
/// facts it read, the job's wall time (from its start to its thread's
/// join: reads, scheduling and waits included) and its atlas.
pub fn log_specialized(
    summary: &Summary,
    facts: &FactDeps,
    job: Duration,
    atlas: Atlas,
    log: &dyn log::Log,
) {
    let Atlas { texels, side } = atlas;
    let square = u64::from(side).saturating_mul(u64::from(side)).max(1);
    let read = if facts.reads.is_empty() {
        "none".to_owned()
    } else {
        facts
            .reads
            .iter()
            .map(|r| format!("{:?}={:?}", r.fact, r.value))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let head = format!(
        "specialized; facts {read}; job {} ms; atlas {texels} texels (side {side}, {} layers)",
        job.as_millis(),
        texels / square,
    );
    log_summary(ScreenKind::Play, &head, summary, log);
}

/// `head`, then `summary`'s counters, definition files and file choices
/// on the same line, then its notes, located, as `kind`'s log summary.
fn log_summary(kind: ScreenKind, head: &str, summary: &Summary, log: &dyn log::Log) {
    let u = &summary.usage;
    let mut head = format!(
        "{head}; elements {} images {} frames {} files {} fonts {} texts {} notes {}; \
         files {}; choices ",
        u.elements,
        u.images,
        u.frames,
        u.files,
        u.fonts,
        u.texts,
        u.notes,
        summary.sources.join(", ")
    );
    if summary.files.is_empty() {
        head.push_str("none");
    }
    for (i, f) in summary.files.iter().enumerate() {
        if i > 0 {
            head.push_str(", ");
        }
        let candidate = f.candidate.as_deref().unwrap_or("none");
        let _ = write!(head, "{} = {candidate} ({:?})", f.choice, f.rule);
    }
    let notes = summary.notes.iter().map(|n| {
        let at =
            n.at.map_or_else(|| "config".to_owned(), |at| summary.locate(at));
        format!("{at}: {:?}: {}", n.severity, n.kind)
    });
    log_file::summary_to(
        &format!("skin: {}", name(kind)),
        &head,
        notes,
        summary.suppressed,
        log,
    );
}

/// The jobs [`run_job`] has started in the process (dev-tools).
#[cfg(feature = "dev-tools")]
static JOBS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The job threads [`run_job`] has joined in the process (dev-tools).
#[cfg(feature = "dev-tools")]
static JOINED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The jobs [`run_job`] has started in the process (dev-tools).
#[cfg(feature = "dev-tools")]
pub fn jobs_started() -> u64 {
    JOBS.load(std::sync::atomic::Ordering::SeqCst)
}

/// The job threads [`run_job`] has joined in the process (dev-tools):
/// each ended thread whose outcome it returned, a panic's and a silent
/// end's included; never an abandoned one.
#[cfg(feature = "dev-tools")]
pub fn jobs_joined() -> u64 {
    JOINED.load(std::sync::atomic::Ordering::SeqCst)
}

/// A launch path, by the screen kinds its startup imports, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launch {
    /// Song selection: play, selection, decision, result.
    Selection,
    /// `tapestry play <chart>`: play, then result; a play skin that cannot
    /// be used fails the run.
    Direct,
}

impl Launch {
    const fn kinds(self) -> &'static [ScreenKind] {
        match self {
            Self::Selection => &[
                ScreenKind::Play,
                ScreenKind::Selection,
                ScreenKind::Decision,
                ScreenKind::Result,
            ],
            Self::Direct => &[ScreenKind::Play, ScreenKind::Result],
        }
    }
}

/// A kind's build as [`startup`] runs it: [`build`] in production.
pub type Build =
    Arc<dyn Fn(ScreenKind, &SkinChoice, bool, &ImportTarget, &Cancel) -> KindOutcome + Send + Sync>;

/// What startup leaves for the first screen.
#[derive(Debug, Default)]
pub struct Started {
    /// Play's admission notice when its skin was refused (song selection):
    /// no chart is admitted, and the built-in skin does not stand in.
    pub play: Option<String>,
    /// The select screen's one notice line for the other kinds refused or
    /// left built in.
    pub notice: Option<String>,
    /// Play's pending skin, marked pending on the render thread: each
    /// attempt is prepared with its specialization ([`PendingSkin`]).
    pub pending: Option<PendingSkin>,
}

impl PartialEq for Started {
    /// The notices equal, and the pending skins the same one, if any.
    fn eq(&self, other: &Self) -> bool {
        self.play == other.play
            && self.notice == other.notice
            && match (&self.pending, &other.pending) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(&a.play, &b.play),
                _ => false,
            }
    }
}

impl Eq for Started {}

/// Why startup ended the run.
#[derive(Debug)]
pub enum StartupEnd {
    /// The window closed: the run ends as at any close.
    Closed,
    /// The run fails with this error.
    Failed(anyhow::Error),
}

/// Imports and selects the skin configured for each kind of `launch`, in
/// order, one kind at a time: each job is joined, and each selection
/// answered, before the next kind's job starts, so at most one candidate
/// exists beside the committed installations. An unconfigured kind sends
/// nothing. Each kind's outcome is one `skin:` line on stdout and in the
/// log, and an installed skin's summary is logged ([`log_installed`]).
///
/// - A play skin that cannot be used refuses every chart on the song
///   selection path ([`Started::play`]); on the direct path it fails the
///   run, whatever refused it, before the result kind's build starts.
/// - Another kind refused, or with no importer yet, keeps its built-in
///   screen, with one notice line ([`Started::notice`]).
/// - A healthy refusal of an installation moves on to the next kind.
///
/// # Errors
///
/// [`StartupEnd::Closed`] once `stop` (the window's close) is seen: no
/// further build, selection or screen follows; a job past [`STOP_WAIT`]
/// is abandoned. [`StartupEnd::Failed`] for the direct path's play
/// refusal, a selection unanswered past its bound (no later kind's build
/// starts), and the render thread's end or failure.
pub fn startup(
    render: &RenderLink,
    skins: &SkinChoices,
    launch: Launch,
    autoplay: bool,
    build: &Build,
    mut stop: impl FnMut() -> bool,
) -> Result<Started, StartupEnd> {
    let mut started = Started::default();
    let mut notices = Vec::new();
    let mut committed = 0_u64;
    for &kind in launch.kinds() {
        let Some(choice) = choice_of(skins, kind) else {
            continue;
        };
        if stop() {
            return Err(StartupEnd::Closed);
        }
        let Some((limits, size)) = render.atlas_limits() else {
            return Err(StartupEnd::Failed(anyhow::anyhow!(
                "the screen packs no skin"
            )));
        };
        let target = ImportTarget {
            limits,
            size,
            default_font: DefaultFont::runtime,
        };
        let job = {
            let build = Arc::clone(build);
            let choice = choice.clone();
            move |cancel: &Cancel| build(kind, &choice, autoplay, &target, cancel)
        };
        let ran = run_cancellable(job, &mut stop);
        let shown = match ran {
            // A job stopped by the close, cancelled or abandoned.
            _ if stop() => return Err(StartupEnd::Closed),
            Ran::Abandoned => return Err(StartupEnd::Closed),
            Ran::Refused(why) | Ran::Done(KindOutcome::Refused(why)) => Shown::Refused(why),
            // Only play's skin can be pending ([`built`]).
            Ran::Done(KindOutcome::Pending(play, note)) => {
                match answered(kind, render.pend(kind), &mut stop)? {
                    None => {
                        started.pending = Some(PendingSkin::new(play));
                        Shown::Pending(note.fact)
                    }
                    Some(why) => Shown::Refused(why),
                }
            }
            Ran::Done(KindOutcome::BuiltIn(why)) => Shown::BuiltIn(why),
            Ran::Done(KindOutcome::Bundle(bundle, summary)) => {
                let (texels, side) = (bundle.atlas_texels(), bundle.side());
                let pending = render.select(kind, Some(bundle));
                match answered(kind, pending, &mut stop)? {
                    None => {
                        committed = committed.saturating_add(texels);
                        let atlas = Atlas { texels, side };
                        log_installed(kind, &summary, atlas, committed, log::logger());
                        Shown::Installed
                    }
                    Some(why) => Shown::Refused(why),
                }
            }
        };
        report(kind, &shown);
        match shown {
            Shown::Installed | Shown::Pending(_) => {}
            Shown::Refused(why) | Shown::BuiltIn(why) if kind == ScreenKind::Play => {
                if launch == Launch::Direct {
                    return Err(StartupEnd::Failed(anyhow::anyhow!(
                        "the play skin cannot be used: {why}"
                    )));
                }
                started.play = Some(Message::SkinUnusable { why: &why }.text());
            }
            Shown::Refused(why) | Shown::BuiltIn(why) => notices.push(notice(kind, &why)),
        }
    }
    if !notices.is_empty() {
        started.notice = Some(notices.join(" / "));
    }
    Ok(started)
}

/// Prints and logs `kind`'s outcome line ([`line`]).
fn report(kind: ScreenKind, shown: &Shown) {
    let l = line(kind, shown);
    println!("{l}");
    log::info!("{l}");
}

/// `kind`'s configured choice.
const fn choice_of(skins: &SkinChoices, kind: ScreenKind) -> Option<&SkinChoice> {
    match kind {
        ScreenKind::Play => skins.play.as_ref(),
        ScreenKind::Selection => skins.selection.as_ref(),
        ScreenKind::Decision => skins.decision.as_ref(),
        ScreenKind::Result => skins.result.as_ref(),
    }
}

/// [`polled`], with a failure reported as `kind`'s refused line once the
/// selection is withdrawn: `None` committed, or the healthy refusal's
/// reason.
///
/// # Errors
///
/// As [`polled`].
fn answered(
    kind: ScreenKind,
    pending: crate::render_side::PendingSelect,
    stop: &mut impl FnMut() -> bool,
) -> Result<Option<String>, StartupEnd> {
    let answer = polled(kind, pending, stop);
    if let Err(StartupEnd::Failed(e)) = &answer {
        report(kind, &Shown::Refused(format!("{e:#}")));
    }
    answer
}

/// Polls `kind`'s selection `pending` (an installation or the pending
/// marker) to its end: `None` committed, or the healthy refusal's reason.
fn polled(
    kind: ScreenKind,
    mut pending: crate::render_side::PendingSelect,
    stop: &mut impl FnMut() -> bool,
) -> Result<Option<String>, StartupEnd> {
    loop {
        if stop() {
            // Withdrawn as it drops: a completion after it installs
            // nothing.
            drop(pending);
            return Err(StartupEnd::Closed);
        }
        match pending.poll(SELECT_SLICE) {
            None => {}
            Some(Ok(Preparation::Ready)) => return Ok(None),
            Some(Ok(Preparation::Refused(why))) => return Ok(Some(why.to_string())),
            Some(Ok(Preparation::Cancelled)) => {
                return Err(StartupEnd::Failed(anyhow::anyhow!(
                    "the {} skin's installation was cancelled",
                    name(kind)
                )));
            }
            Some(Err(e)) => {
                let what = format!("the {} skin cannot be installed", name(kind));
                return Err(StartupEnd::Failed(anyhow::Error::new(e).context(what)));
            }
        }
    }
}

#[cfg(test)]
mod tests;
