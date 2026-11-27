//! The result skin's specialization (design.md "beatoraja Lua Skins",
//! installation and specialization; the S10-09g design's D10, D11 and
//! D13): its images prepared once in Preparing from a probe snapshot, and
//! on the result (P) and on each view switch only its Lua layout run again
//! over the skin's resource cache, rebound to the prepared atlas, or
//! packed with a union when the layout needs a cell the atlas lacks.
//!
//! Every outcome is published to the player's result slot on the CPU
//! ([`Published`]); nothing here installs anything on the GPU (S10-09i
//! does). Not in-play code: it runs between plays and on the result.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use eval::Tally;
use render::SkinBundle;
use rules::PerJudgment;
use skin_beatoraja::{
    Cancel, Config, Fact, FactDeps, FactId, ImportFacts, JudgeClass, LuaHeader, LuaImported,
    LuaOutcome,
};
use skin_model::{
    Atlas, AtlasId, BestFigures, BestRate, BestView, FileSource, ImageFacts, KeptLayers, LampKey,
    Layout, ResourceCache, ResultChart, ResultFields, ResultNumber, ResultRate, ResultScore,
    ResultStatus, ResultView, Root, ScreenKind, Sheet, TimingStats, Variant,
};

use crate::chart_images::{ChartImages, Sources};

use crate::imports::{self, ImportTarget, Job, Ran};
use crate::screens::{ScriptGen, ScriptPermit, ScriptSlot, Scripts};

#[cfg(feature = "dev-tools")]
pub(crate) mod gaps;
mod preload;

#[cfg(all(test, feature = "dev-tools"))]
pub(crate) use preload::tests as preload_tests;
use preload::{Adopted, Event, Preload};
pub use preload::{PackCharge, PreloadClaim, PreloadControl};

/// A result skin whose base import read a fact the base snapshot has
/// pending: what each image stage and layout starts from, and the skin
/// folder's resource cache, which every later import reads its image and
/// font files through (D13).
pub struct PendingResult {
    header: LuaHeader,
    config: Config,
    target: ImportTarget,
    cache: Arc<ResourceCache>,
    /// The source its imports read through; its clones share its counts.
    #[cfg(all(test, feature = "dev-tools"))]
    source: FileSource,
}

impl std::fmt::Debug for PendingResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingResult")
            .field("cache", &self.cache)
            .finish_non_exhaustive()
    }
}

impl PendingResult {
    /// `header`, its imports reading through a new cache of `root`.
    pub fn new(mut header: LuaHeader, config: Config, target: ImportTarget, root: &Root) -> Self {
        let cache = ResourceCache::new(root);
        let source = FileSource::cached(Arc::clone(&cache));
        header.read_through(source.clone());
        Self {
            header,
            config,
            target,
            cache,
            #[cfg(all(test, feature = "dev-tools"))]
            source,
        }
    }

    /// What its imports and packs read from the disk (dev-tools).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn source_counts(&self) -> skin_model::SourceCounts {
        self.source.counts()
    }

    /// The startup probe's import (design.md "beatoraja Lua Skins", the
    /// result skin's preload), on the job's thread: the probe's result
    /// values with every attempt input pending, over a chart with no
    /// facts, which no read reaches. With `permit` it is imported as a
    /// shown layout is, its runtime kept, then dropped here with the
    /// permit before the outcome returns; without one it is imported
    /// plain and keeps no VM.
    pub(crate) fn import_probe(
        &self,
        permit: Option<ScriptPermit>,
        cancel: &Cancel,
    ) -> ProbeOutcome {
        let facts = ImportFacts::result_probe(probe_fields(ResultChart::default()));
        let kept = permit.is_some();
        let outcome = if kept {
            self.header
                .import_runtime_reading(&self.config, &facts, cancel)
        } else {
            self.header.import_reading(&self.config, &facts, cancel)
        };
        match outcome {
            Ok(LuaOutcome::Imported(mut imported)) => {
                drop(imported.runtime.take());
                drop(permit);
                if !reads_result(&imported.facts) {
                    return ProbeOutcome::Bundle;
                }
                let deps = imported.facts.clone();
                ProbeOutcome::Probe(Box::new(StartupProbe {
                    imported,
                    deps,
                    kept,
                }))
            }
            Ok(LuaOutcome::Pending(p)) => ProbeOutcome::Stopped(p.pending),
            Err(refused) => ProbeOutcome::Refused(imports::chain(&refused.0)),
        }
    }
}

/// What the startup probe's import gave ([`PendingResult::import_probe`]).
#[derive(Debug)]
pub enum ProbeOutcome {
    /// It read a fact pending in its snapshot, an attempt input: the skin
    /// takes its image stage at each attempt.
    Stopped(FactId),
    /// Refused, with the reason: the skin takes its image stage at each
    /// attempt.
    Refused(String),
    /// It read no fact of the viewed result: a bundle for every view, not
    /// preloaded.
    Bundle,
    /// It read the viewed result: the preload's input.
    Probe(Box<StartupProbe>),
}

/// A completed startup probe that read the viewed result: its import,
/// its runtime dropped, which the preload's pack lays out from with no
/// further Lua; the facts it read; and the mode it actually took, kept
/// only when a permit was taken and the kept import completed.
#[derive(Debug)]
pub struct StartupProbe {
    imported: LuaImported,
    deps: FactDeps,
    kept: bool,
}

impl StartupProbe {
    /// Whether it was imported kept.
    pub const fn kept(&self) -> bool {
        self.kept
    }

    /// Its import (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub const fn imported(&self) -> &LuaImported {
        &self.imported
    }
}

/// A prepared atlas and the sheet it was packed from: the cells a union
/// starts from (D13, "The atlas step").
#[derive(Debug)]
pub struct ImageEntry {
    atlas: Atlas,
    packed: Sheet,
    /// Whether the import it was laid out from had a permit, its runtime
    /// kept (the transition probe's `dev: probe-miss`).
    #[cfg(feature = "dev-tools")]
    import_permit: bool,
    /// Records the thread its last reference drops on (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    _dropped: preload::tests::DropWitness,
}

#[cfg(feature = "dev-tools")]
thread_local! {
    /// Whether the import [`lay_out`] runs on, on this job's thread, had a
    /// permit: set by [`layout`] (the transition probe's `dev: probe-miss`).
    static IMPORT_PERMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl ImageEntry {
    /// The atlas's shared pages.
    pub fn pages(&self) -> &Arc<skin_model::PackedPages> {
        self.atlas.shared_pages()
    }
}

/// What the result slot holds: the built-in result, or a bundle with the
/// prepared atlas it is bound to.
#[derive(Clone, Debug)]
pub enum Published {
    BuiltIn,
    /// A bundle, its prepared atlas, and its runtime's generation when its
    /// callbacks run one (s7-07-design §3.7.1).
    Skin(Arc<SkinBundle>, Option<Arc<ImageEntry>>, Option<ScriptGen>),
}

/// A view a layout is asked for: its fields, or `None` for an unavailable
/// view (D7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewFacts(pub Option<ResultFields>);

/// A layout's specialization: its bundle, the facts it read and the
/// prepared atlas it is bound to.
#[derive(Debug)]
struct Specialized {
    bundle: Arc<SkinBundle>,
    facts: FactDeps,
    entry: Option<Arc<ImageEntry>>,
    /// Its runtime's generation, once offered; reused only while live.
    script: Option<ScriptGen>,
    /// The chart's images derived for its chart slot by its layout job,
    /// when that slot differs from the result half's, with the id of the
    /// sources they came from: it lives no longer than its atlas.
    derived: Option<(u64, Arc<ChartImages>)>,
}

/// What a layout job derives the chart's images from: the attempt's
/// sources, and the result half's chart layer side, which needs no
/// derivation.
#[derive(Clone, Debug)]
struct ChartFrom {
    sources: Arc<Sources>,
    side: Option<u32>,
}

/// The attempt's images the result draws: its sources, and its
/// result half, handed over at the attempt's commit.
#[derive(Debug)]
struct HeldImages {
    sources: Arc<Sources>,
    half: Option<Arc<ChartImages>>,
    /// The chart layer side the half is derived for.
    side: Option<u32>,
}

/// The chart's facts a result view carries but its images'
/// availability, which only the attempt's facts stage resolves: no
/// result import, probe or view sees them before
/// ([`PendingChart::resolved`]).
#[derive(Clone, Copy, Debug)]
pub struct PendingChart(ResultChart);

impl PendingChart {
    /// The facts with `stored`, when given, as the chart's difficulty: the
    /// one the library stores for a chart chosen on selection, declared or
    /// inferred, when it is 1 to 5, in place of the attempt's own
    /// `#DIFFICULTY` (design.md "Play Lifecycle", the chart's labels).
    #[must_use]
    pub fn with_difficulty(mut self, stored: Option<u8>) -> Self {
        if let Some(d) = stored {
            self.0.difficulty = Some(d).filter(|d| (1..=5).contains(d));
        }
        self
    }

    /// The chart's facts with its images' resolved availability.
    #[must_use]
    pub const fn resolved(self, images: ImageFacts) -> ResultChart {
        ResultChart { images, ..self.0 }
    }
}

/// Why a layout gave no specialization.
#[derive(Debug)]
enum Unbuilt {
    /// Refused, with the reason: the built-in result for its view.
    Refused(String),
    /// Its build would make a third live atlas (Q3): it waits.
    Deferred,
}

impl From<String> for Unbuilt {
    fn from(why: String) -> Self {
        Self::Refused(why)
    }
}

/// What a layout job gives back: its request, the view it ran for, its
/// specialization or why not, and the image entry after it (the same, a
/// new one, or none when it dropped an unshown one).
struct LayoutOut {
    request: u64,
    view: ViewFacts,
    laid: Laid,
    /// The runtime its import kept, offered when it lands.
    script: ScriptSlot,
}

/// The attempt's result skin, as its image stage left it.
#[derive(Clone, Debug)]
enum ForAttempt {
    /// A bundle for every view, with the prepared atlas it is bound to
    /// and its runtime's generation: a fixed skin, or one that read no
    /// fact of the viewed result.
    Every(Arc<SkinBundle>, Option<Arc<ImageEntry>>, Option<ScriptGen>),
    /// The built-in result.
    BuiltIn,
    /// Laid out per view.
    Pending,
}

/// The kind's skin, as startup left it.
#[derive(Debug)]
pub enum ResultStart {
    /// Imported with no fact pending: one bundle for every chart, with its
    /// runtime's generation, retained for the run.
    Fixed(Arc<SkinBundle>, Option<ScriptGen>),
    /// Built per attempt and view.
    Pending(Arc<PendingResult>),
}

/// What an attempt's result uploads after its commit.
#[derive(Clone, Debug)]
pub enum Upload {
    /// An installation of this bundle.
    Bundle(Arc<SkinBundle>),
    /// A pages-only installation of a prepared atlas.
    Pages(Arc<skin_model::PackedPages>),
}

impl Upload {
    /// The atlas it uploads.
    pub fn atlas(&self) -> AtlasId {
        match self {
            Self::Bundle(b) => b.atlas_pages().id(),
            Self::Pages(p) => p.id(),
        }
    }
}

/// How an image stage ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Staged {
    /// Ready: the attempt's result skin is decided.
    Ready,
    /// The attempt's quit, a close or a thread's failure came: the
    /// attempt is cancelled.
    Cancelled,
}

/// Builds and packs counted under dev-tools and in tests.
#[cfg(any(test, feature = "dev-tools"))]
static BUILDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The atlas builds made so far (dev-tools).
#[cfg(all(test, feature = "dev-tools"))]
pub fn builds() -> u32 {
    BUILDS.load(std::sync::atomic::Ordering::SeqCst)
}

/// The player's result skin: its kind, the attempt's outcome, the image and
/// layout entries, the published generation and the one job.
#[derive(Default)]
pub struct ResultSkin {
    start: Option<ResultStart>,
    attempt: Option<ForAttempt>,
    /// The prepared atlas (D13).
    image: Option<Arc<ImageEntry>>,
    /// The probe's facts and the cells its layout needs: the image stage
    /// is reused while the facts hold and the image entry holds the cells.
    probe: Option<(FactDeps, Needs)>,
    /// The one cached specialization, whichever path made it: a view's
    /// layout, or a bundle for every view of a skin that reads no fact of
    /// the viewed result. Reused while its reads hold ([`Self::reuse_for_attempt`],
    /// [`Self::reuse_for_view`]); one not shown goes once a replacement
    /// plan fits ([`release`]).
    layout: Option<Specialized>,
    published: Option<Published>,
    generation: u64,
    job: Option<(Job<LayoutOut>, u64, Slot)>,
    waiting: Option<(u64, ViewFacts)>,
    /// A request whose build waits for an atlas to be released (Q3).
    deferred: Option<(u64, ViewFacts)>,
    /// The atlases the renderer still references, as it last reported
    /// them ([`Self::set_retained`]).
    retained: Vec<AtlasId>,
    /// Development builds: the render side's holders of each, as last
    /// reported ([`Self::set_holders`]).
    #[cfg(feature = "dev-tools")]
    holders: Vec<(AtlasId, crate::result_present::Holders)>,
    /// Development builds: the stage a view's build is asked for, as the
    /// no-room line names it ([`Self::set_stage`]).
    #[cfg(feature = "dev-tools")]
    stage: &'static str,
    /// The latest request, and the view it asked for.
    latest: Option<(u64, ViewFacts)>,
    next_request: u64,
    /// The attempt's chart facts and judgment class, once its image stage
    /// ran.
    chart: Option<ResultChart>,
    class: Option<Option<JudgeClass>>,
    /// The cells of an image entry whose pages a deferred build dropped
    /// (D13), a union base until a build packs them.
    cells: Option<Sheet>,
    /// The result runtimes' cell: their generations, permits and eviction.
    scripts: Scripts,
    /// A request whose job waits for a permit (s7-07-design §4 rule 4):
    /// tried again at each poll.
    permit_wait: Option<(u64, ViewFacts)>,
    /// The specialization a newer one replaced in `layout` while its
    /// generation was protected: a request for its view reuses it, its
    /// runtime running on (s7-07-design §3.7.1 M8).
    presented: Option<Specialized>,
    /// The thread a layout job wakes once its outcome is in (the game
    /// thread, [`crate::controls::Controls::game_thread`]): a switch's
    /// layout lands then, not at the end of the game thread's idle wait.
    wake: Option<std::thread::Thread>,
    /// The committed attempt's images: replaced at the next commit,
    /// dropped at selection's and practice's entry.
    images: Option<HeldImages>,
    /// Whether the probe of a skin reading a result fact is packed from
    /// its kept import ([`kept_probe_fits`]), else from a plain one; `None`
    /// until first decided, a plain one meanwhile.
    kept: Option<bool>,
    /// The preload of a skin whose startup probe read the viewed result
    /// (design.md "beatoraja Lua Skins", the result skin's preload),
    /// owned and changed on the game thread only.
    preload: Preload,
    /// The preload's control, shared with the shelf's pause and its
    /// packs' threads; none without a preload.
    control: Option<Arc<PreloadControl>>,
    /// The packs' thread mode in place of the system's (tests).
    preload_mode: Option<Arc<dyn crate::priority::ThreadMode>>,
    /// The packs begun, each one's job.
    preload_jobs: u64,
    /// The preload's disposal thread, reserved before its first pack.
    disposer: Option<preload::Disposer>,
}

impl std::fmt::Debug for ResultSkin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResultSkin")
            .field("generation", &self.generation)
            .field("published", &self.published)
            .finish_non_exhaustive()
    }
}

/// Whether the shared texel limit leaves room for any result atlas
/// beside the selection's and decision's committed charges `menus` (the
/// selection's with its pool's firm layer, whether the pool has it now
/// or not, [`render::Renderer::menu_charges`]): the largest play and result
/// charges, each the per-atlas cap and one VM charge, fit beside them
/// (design.md "beatoraja Lua Skins", the result skin; "Memory
/// Budget"). Only then is a probe packed from its kept import, which
/// can be larger than a plain one.
#[must_use]
pub fn kept_probe_fits(menus: u64) -> bool {
    let most = skin_model::MAX_ATLAS_TEXELS.saturating_add(render::VM_TEXELS);
    menus.saturating_add(most.saturating_mul(2)) <= render::SHARED_TEXELS
}

/// Whether `deps` read a fact of the viewed result beyond the chart's
/// ([`skin_beatoraja::FactId::reads_view`]): the chart's facts are known
/// before READY and decided there.
fn reads_result(deps: &FactDeps) -> bool {
    deps.reads.iter().any(|r| r.fact.reads_view())
}

/// The snapshot of a result view's fields, with the chart's judgment
/// class `class` (options 180-184) once the attempt gives it.
fn result_facts(fields: ResultFields, class: Option<Option<JudgeClass>>) -> ImportFacts {
    ImportFacts {
        result: Fact::Known(fields),
        result_inputs: Fact::Known(()),
        judge_class: class.map_or(Fact::Unavailable, Fact::Known),
        ..ImportFacts::unavailable(ScreenKind::Result)
    }
}

/// D13's probe: `chart`'s facts, and every result value, best value and
/// count shown and 1, every validity true.
pub fn probe_fields(chart: ResultChart) -> ResultFields {
    ResultFields::of(&probe_view(chart))
}

/// The view of [`probe_fields`].
pub(crate) fn probe_view(chart: ResultChart) -> ResultView {
    let one = PerJudgment {
        perfect: 1,
        great: 1,
        good: 1,
        bad: 1,
        poor: 1,
    };
    let tally = Tally {
        counts: one,
        ex_score: 1,
        bp: 1,
        combo: 1,
        max_combo: 1,
        empty_poor: 1,
        early: one,
        late: one,
        empty_poor_early: 1,
        empty_poor_late: 1,
    };
    ResultView {
        status: ResultStatus::Ready,
        chart,
        score: ResultScore {
            tally,
            gauge_permille: 1,
            clear: LampKey::Normal,
            cleared: true,
            rate: Some(ResultRate {
                rate_bp: 1,
                grade: 1,
            }),
            next_rank: 1,
            duration_us: Some(1),
        },
        best: BestView::Known(BestFigures {
            ex: 1,
            ex_diff: 1,
            bp: ResultNumber::Shown(1),
            bp_diff: ResultNumber::Shown(1),
            clear: LampKey::Normal,
            max_combo: ResultNumber::Shown(1),
            max_combo_diff: ResultNumber::Shown(1),
            ex_updated: true,
            bp_updated: true,
            rate: Some(BestRate {
                rate_bp: 1,
                grade: 1,
                updated: true,
            }),
        }),
        timing: Some(TimingStats {
            samples: 1,
            mean_x100: 1,
            sd_x100: 1,
        }),
        timelines: None,
        gauge: None,
        deviations: None,
        timing_areas: None,
        totals: Default::default(),
    }
}

/// What a job holds of the image side while it runs, handed back when it
/// ends: the image entry, the kept cells of entries whose pages were
/// dropped (D13), and the cached specialization when it is not the one
/// shown. What is not shown goes once a replacement plan fits
/// ([`release`]) and stays otherwise.
#[derive(Default)]
struct Images {
    entry: Option<Arc<ImageEntry>>,
    cells: Option<Sheet>,
    layout: Option<Specialized>,
}

/// The atlases a build counts besides its image entry
/// ([`ResultSkin::others`]), and in development builds what the no-room
/// line names of them.
struct Others {
    ids: Vec<AtlasId>,
    #[cfg(feature = "dev-tools")]
    line: RoomLine,
}

/// Development builds: what a build that finds no room says (design.md
/// "Memory Budget", the no-room line): the stage, each atlas counted with
/// its holders, and whether the count is conservative, that is whether
/// leaving out the atlases only retirement entries whose pages left them
/// hold would leave room.
#[cfg(feature = "dev-tools")]
struct RoomLine {
    stage: &'static str,
    /// The render side's holders, as the report last named them.
    holders: Vec<(AtlasId, crate::result_present::Holders)>,
    /// The game's own holdings among the atlases counted.
    game: Vec<AtlasId>,
}

#[cfg(feature = "dev-tools")]
impl RoomLine {
    /// The line for `live`, the atlases counted, `entry` the image entry
    /// kept.
    fn line(&self, live: &[AtlasId], entry: Option<AtlasId>) -> String {
        let held = |id: &AtlasId| {
            self.holders
                .iter()
                .find(|(h, _)| h == id)
                .map(|(_, h)| *h)
                .unwrap_or_default()
        };
        let game = |id: &AtlasId| self.game.contains(id) || entry == Some(*id);
        let named: Vec<String> = live
            .iter()
            .map(|id| {
                let mut by = held(id).names();
                if game(id) {
                    if !by.is_empty() {
                        by.push('+');
                    }
                    by.push_str("game");
                }
                format!("{}:{by}", id.0)
            })
            .collect();
        let conservative = live
            .iter()
            .filter(|id| !(held(id).array_only() && !game(id)))
            .count()
            < 2;
        format!(
            "dev: result build: no room at {}: {}{}",
            self.stage,
            named.join(" "),
            if conservative { " conservative" } else { "" }
        )
    }
}

/// The one release decision, once a replacement plan fits (D13): an
/// image entry that is not `base` (the shown one) is dropped, its cells
/// kept; the unshown cached specialization goes too. `merged` is the kept
/// cells' union with the dropped entry's sheet when the caller computed
/// it before the take ([`dropped_union`]); otherwise it is made here.
/// Returns what is left and whether the atlases live besides it
/// (`others`, with the entry when kept) leave room for one more (the
/// S10-09i design's Q3); in development builds, a build with no room says
/// so.
fn release(
    images: Images,
    base: Option<&Arc<ImageEntry>>,
    others: Others,
    merged: Option<Sheet>,
) -> (Images, bool) {
    let Images {
        entry,
        cells,
        layout,
    } = images;
    let (entry, dropped) = match entry {
        Some(e) if base.is_some_and(|b| Arc::ptr_eq(&e, b)) => (Some(e), None),
        other => (None, other),
    };
    drop(layout);
    let cells = match (cells, dropped) {
        (c, None) => c,
        (None, Some(d)) => Some(d.packed.clone()),
        (Some(c), Some(d)) => merged.or_else(|| c.union(&d.packed)).or(Some(c)),
    };
    let mut live = others.ids;
    live.extend(entry.as_ref().map(|e| e.pages().id()));
    live.sort_unstable();
    live.dedup();
    let room = live.len() < 2;
    #[cfg(feature = "dev-tools")]
    if !room {
        let line = others
            .line
            .line(&live, entry.as_ref().map(|e| e.pages().id()));
        #[cfg(test)]
        tests::room_line(&line);
        println!("{line}");
    }
    (
        Images {
            entry,
            cells,
            layout: None,
        },
        room,
    )
}

/// The holdings a job may release, shared with it while it runs: they
/// stay here until [`release`] takes them once a replacement plan fits,
/// so a job that never starts, fails, or ends before that hands them
/// back untouched, and none is referenced past the release.
type Slot = Arc<Mutex<Option<Images>>>;

/// What an image stage's job gives: the probe's outcome (a bundle for
/// every view, images prepared, or a refusal), the facts it read, the
/// images it took and the cells its layout needs.
type ImageOut = (
    Option<Result<Option<Arc<SkinBundle>>, String>>,
    FactDeps,
    Option<Images>,
    Option<Needs>,
    ScriptSlot,
);

/// How [`ResultSkin::begin_image_stage`] began.
pub enum ImageStage {
    /// Decided at once.
    Ready(Staged),
    /// Its job runs.
    Running(Box<ImageJob>),
    /// It waits for the preload's claimed pack, which it adopted in place
    /// of its own import and build; polled as [`Running`](Self::Running).
    Adopted(Box<ImageJob>),
}

/// An image stage in flight: its own job, or the preload's pack it
/// adopted.
pub struct ImageJob(InFlight);

#[expect(
    clippy::large_enum_variant,
    reason = "an image stage in flight is always held boxed"
)]
enum InFlight {
    Own {
        job: imports::Job<ImageOut>,
        slot: Slot,
    },
    Adopted(Adopted),
    /// It waits for the preload's pack `0`, stopped for the attempt, to be
    /// collected before it takes the image state.
    Draining(u64),
}

/// `slot`'s holdings, a poisoned lock included.
fn lock(slot: &Slot) -> MutexGuard<'_, Option<Images>> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A layout's cells and how they are packed: what an atlas must hold for
/// the layout to bind to it ([`Atlas::rebind`]).
struct Needs {
    sheet: Sheet,
    variant: Variant,
    kept: KeptLayers,
}

impl Needs {
    /// Whether `entry`'s atlas holds every cell, packed as needed.
    fn held_by(&self, entry: &ImageEntry) -> bool {
        entry
            .atlas
            .rebind_keeping(&self.sheet, self.variant, self.kept)
            .is_ok()
    }
}

/// What a layout leaves: its specialization or why not; what is left of
/// the holdings it took from its slot ([`release`]), `None` when it never
/// took them; and, when it is specialized, the cells it needs.
struct Laid {
    result: Result<Specialized, Unbuilt>,
    taken: Option<Images>,
    needs: Option<Needs>,
}

impl Laid {
    fn refused(why: String) -> Self {
        Self {
            result: Err(why.into()),
            taken: None,
            needs: None,
        }
    }
}

/// A layout of `pending` for `view`: the import over the cache, then the
/// atlas step against the slot's image entry (D13). When its atlas does
/// not hold the layout's cells, a union with `base`'s (the shown
/// generation's atlas), the entry's and the kept cells, else the layout's
/// cells alone, is planned first; only once a plan fits are the holdings
/// taken from `slot` and what is not shown released, then, unless the
/// atlases live besides it (`others`, with the entry when kept) and the
/// new one would pass two, the plan packed (the S10-09i design's Q3).
#[expect(clippy::too_many_arguments, reason = "the job's inputs, moved in once")]
fn layout(
    pending: &PendingResult,
    view: ViewFacts,
    class: Option<Option<JudgeClass>>,
    slot: &Slot,
    base: Option<Arc<ImageEntry>>,
    others: Others,
    cancel: &Cancel,
    permit: Option<ScriptPermit>,
    chart: Option<&ChartFrom>,
) -> (Laid, ScriptSlot) {
    #[cfg(all(test, feature = "dev-tools"))]
    tests::layout_starts();
    let Some(fields) = view.0 else {
        return (
            Laid::refused("the view is unavailable".to_owned()),
            ScriptSlot::default(),
        );
    };
    let facts = result_facts(fields, class);
    // A layout that is shown keeps its runtime, under the permit taken
    // for it; the plain probe's, never shown, keeps none.
    let outcome = if permit.is_some() {
        pending
            .header
            .import_runtime_reading(&pending.config, &facts, cancel)
    } else {
        pending
            .header
            .import_reading(&pending.config, &facts, cancel)
    };
    let mut imported = match outcome {
        Ok(LuaOutcome::Imported(i)) => i,
        Ok(LuaOutcome::Pending(p)) => {
            return (
                Laid::refused(format!("the skin reads {:?}, pending here", p.pending)),
                ScriptSlot::default(),
            );
        }
        Err(refused) => {
            return (
                Laid::refused(imports::chain(&refused.0)),
                ScriptSlot::default(),
            );
        }
    };
    #[cfg(feature = "dev-tools")]
    IMPORT_PERMIT.set(permit.is_some());
    let script = ScriptSlot::staged_with(imported.runtime.take(), permit);
    let mut laid = lay_out(pending, imported, slot, base, others, cancel, &|| {
        cancel.is_cancelled()
    });
    // A chart slot the result half does not fit: the images derived from
    // the attempt's sources at its size, here, off the game thread.
    if let (Ok(spec), Some(chart)) = (&mut laid.result, chart)
        && spec.bundle.chart_layer().is_some()
        && Some(spec.bundle.side()) != chart.side
    {
        let derived = crate::chart_images::derive_set(&chart.sources, spec.bundle.side(), &|| {
            cancel.is_cancelled()
        });
        // A derivation that failed is not kept with the layout: the layout
        // is laid out again (and derives again) when it is asked for.
        if let Some(derived) = derived {
            spec.derived = Some((chart.sources.id(), Arc::new(derived)));
        }
    }
    // A layout that gave no specialization keeps no runtime: it goes here,
    // on the job's thread.
    let script = if laid.result.is_ok() {
        script
    } else {
        ScriptSlot::default()
    };
    (laid, script)
}

/// The transition probe's `dev: probe-miss` line, for a layout of `sheet`
/// under `variant` and `kept` that the image entry `e` does not hold: the
/// refusal of the entry's rebind (its pages' variant, kept layers or skin
/// folder; a file not held; a cell not held), the entry's variant, kept
/// layers and folder against the layout's, whether the entry's import and
/// the layout's had a permit, and the layout's cells the entry lacks by
/// name and rectangle: a file rewritten with the same cells is refused as
/// `missing-file` with none of them missing, the refusal comparing the
/// file's version as well.
#[cfg(feature = "dev-tools")]
fn probe_miss(e: &ImageEntry, sheet: &Sheet, variant: Variant, kept: skin_model::KeptLayers) {
    #[cfg(all(test, feature = "dev-tools"))]
    tests::in_probe_miss();
    let pages = e.atlas.shared_pages();
    let refusal = match pages.not_held_why(sheet, variant, kept) {
        None => "none".to_owned(),
        Some(skin_model::NotHeldWhy::Header {
            variant,
            kept,
            root,
        }) => format!("header (variant {variant}, kept {kept}, root {root})"),
        Some(skin_model::NotHeldWhy::File(name)) => format!("missing-file {name:?}"),
        Some(skin_model::NotHeldWhy::Cell(name, rect)) => {
            format!("missing-cell {name:?} {rect:?}")
        }
        Some(skin_model::NotHeldWhy::Sheet) => "sheet".to_owned(),
    };
    let have: std::collections::BTreeSet<_> = e.packed.survey_cells(variant).into_iter().collect();
    let cells = sheet.survey_cells(variant);
    let miss: Vec<_> = cells.iter().filter(|c| !have.contains(*c)).collect();
    println!(
        "dev: probe-miss refusal {refusal}; layout variant {variant:?} kept {kept:?} import-permit {}; entry variant {:?} kept {:?} root {:?} import-permit {}; layout cells {} entry cells {} missing by name and rectangle {} {miss:?}",
        IMPORT_PERMIT.get(),
        pages.variant(),
        pages.kept(),
        pages.root(),
        e.import_permit,
        cells.len(),
        have.len(),
        miss.len()
    );
}

/// [`layout`]'s atlas step and bundle, from its import `imported`; the
/// atlas build stops once `check` turns true (the job's cancel, or a
/// task's checkpoint).
fn lay_out(
    pending: &PendingResult,
    imported: skin_beatoraja::LuaImported,
    slot: &Slot,
    base: Option<Arc<ImageEntry>>,
    others: Others,
    cancel: &Cancel,
    check: &dyn Fn() -> bool,
) -> Laid {
    let deps = imported.facts;
    let mut skin = imported.imported.skin;
    let scratch = imported.imported.scratch;
    let bound = match atlas_step(pending, &mut skin, slot, base, others, cancel, check) {
        Ok(bound) => bound,
        Err(not) => {
            let NotBound { why, lent, built } = *not;
            return Laid {
                result: Err(why),
                taken: built.or(lent),
                needs: None,
            };
        }
    };
    let result = imports::bundle_of(skin, scratch, bound.atlas, &pending.target, cancel)
        .map(|bundle| Specialized {
            bundle,
            facts: deps,
            entry: bound.entry,
            script: None,
            derived: None,
        })
        .map_err(Unbuilt::from);
    Laid {
        result,
        taken: bound.taken,
        needs: Some(bound.needs),
    }
}

/// What the atlas step binds a layout to: its atlas, the image entry
/// that holds it, what is left of the slot's holdings it took (the new
/// entry when it packed one; `None` when it took none), and the cells
/// the layout needs.
struct Bound {
    atlas: Atlas,
    entry: Option<Arc<ImageEntry>>,
    taken: Option<Images>,
    needs: Needs,
}

/// An atlas step that bound nothing: why, what is left of the slot's
/// holdings it took (`lent`: the image state the job was lent, for the
/// game thread to have back), and the entry it packed before the
/// binding failed (`built`: new pages, which only a layout may keep).
struct NotBound {
    why: Unbuilt,
    lent: Option<Images>,
    built: Option<Images>,
}

impl NotBound {
    fn refused(why: String) -> Self {
        Self {
            why: why.into(),
            lent: None,
            built: None,
        }
    }
}

/// What a pack packed ([`pack`]): the new entry (or `None`, the slot's
/// held the layout's cells), the facts the probe read and the cells its
/// layout needs.
type Packed = (Option<Images>, FactDeps, Needs);

/// Why a pack packed nothing, and what is left of the holdings the slot
/// lent ([`pack`]).
type NotPacked = Box<(Unbuilt, Option<Images>)>;

/// The preload's pack of `imported` (design.md "beatoraja Lua Skins", the
/// result skin's preload): [`lay_out`]'s atlas step alone, its stop the
/// coordinator's checkpoint `check`. The bundle and its font reads are
/// left to the attempt's layout, which makes the view's bundle anyway, so
/// nothing after the atlas build is cancellable here. Pages it packed and
/// could not bind drop here, on the pack's thread.
fn pack(
    pending: &PendingResult,
    imported: skin_beatoraja::LuaImported,
    slot: &Slot,
    base: Option<Arc<ImageEntry>>,
    others: Others,
    cancel: &Cancel,
    check: &dyn Fn() -> bool,
) -> Result<Packed, NotPacked> {
    let deps = imported.facts;
    let mut skin = imported.imported.skin;
    match atlas_step(pending, &mut skin, slot, base, others, cancel, check) {
        Ok(bound) => Ok((bound.taken, deps, bound.needs)),
        Err(not) => {
            let NotBound { why, lent, built } = *not;
            drop(built);
            Err(Box::new((why, lent)))
        }
    }
}

/// The atlas step's own checkpoint sites, after skin-model's (dev-tools:
/// the codes a gap names).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StepSite {
    /// The step's start, before any planning.
    StepStart = 16,
    /// Before each union of an earlier holding with the layout's sheet.
    Union = 17,
    /// After the last union.
    UnionEnd = 18,
    /// After the planning, before the holdings are taken.
    Take = 19,
}

/// Asks `check` at the atlas step's `site`: whether to stop.
fn ask(check: &dyn Fn() -> bool, site: StepSite) -> bool {
    #[cfg(feature = "dev-tools")]
    return gaps::ask(check, site);
    #[cfg(not(feature = "dev-tools"))]
    {
        let _ = site;
        check()
    }
}

/// The union [`release`] makes of the kept cells `kept` with the sheet of
/// the slot's entry `entry` when it drops it (it is not `base`), asking
/// `check`; `None` when there is none to make.
fn dropped_union(
    kept: Option<&Sheet>,
    entry: Option<&Arc<ImageEntry>>,
    base: Option<&Arc<ImageEntry>>,
    check: &dyn Fn() -> bool,
) -> Result<Option<Sheet>, skin_model::AtlasError> {
    match (kept, entry) {
        (Some(c), Some(d)) if !base.is_some_and(|b| Arc::ptr_eq(d, b)) => {
            c.union_checked(&d.packed, check)
        }
        _ => Ok(None),
    }
}

/// The atlas step of `skin` for [`lay_out`] and [`pack`]: the slot's
/// image entry rebound when it holds the layout's cells, else the plan
/// packed (D13, "The atlas step").
///
/// The step asks `check` at its start, before each union of an earlier
/// holding, inside it and after the last, through the planning
/// (skin-model asks it at each phase and batch of work), inside the
/// release's union of the kept cells with the entry it drops, made from
/// the snapshot, and once more before it takes the holdings; a stop at
/// any of these ends it with the slot untouched, the partial tables
/// dropped here.
///
/// The slot's single owner: from [`ResultSkin::slot_for_job`] until the
/// outcome is collected, only this job's thread locks the slot (the game
/// thread locks it once the outcome is in, and no other job is given it),
/// so the snapshot taken under the lock before the unions and planning is
/// still the slot's at the take. A change that lets another thread lock a
/// running job's slot must check the snapshot again at the take. Nothing
/// that can wait for a permit (a checkpoint's move, an admission) runs
/// under the lock.
#[expect(clippy::needless_pass_by_value, reason = "the job owns its inputs")]
#[expect(clippy::too_many_lines, reason = "one step, its stops in order")]
fn atlas_step(
    pending: &PendingResult,
    skin: &mut skin_model::Skin,
    slot: &Slot,
    base: Option<Arc<ImageEntry>>,
    others: Others,
    cancel: &Cancel,
    check: &dyn Fn() -> bool,
) -> Result<Bound, Box<NotBound>> {
    #[cfg(not(all(test, feature = "dev-tools")))]
    let _ = cancel;
    let stopped = || {
        Box::new(NotBound {
            why: "the atlas cannot be built: cancelled".to_owned().into(),
            lent: None,
            built: None,
        })
    };
    if ask(check, StepSite::StepStart) {
        return Err(stopped());
    }
    let variant = skin.variant(pending.target.size, &Layout::AUTHOR);
    let (kept, _) = imports::plan_layers(skin, variant, pending.target.limits);
    let entry = lock(slot).as_ref().and_then(|i| i.entry.clone());
    let held = entry
        .as_ref()
        .and_then(|e| e.atlas.rebind_keeping(skin.sheet(), variant, kept).ok());
    #[cfg(feature = "dev-tools")]
    {
        crate::dev::loop_probe::mark("rl:had_entry", i64::from(entry.is_some()));
        crate::dev::loop_probe::mark("rl:rebind_held", i64::from(held.is_some()));
        if crate::dev::loop_probe::on()
            && held.is_none()
            && let Some(e) = &entry
        {
            gaps::dev_only(|| probe_miss(e, skin.sheet(), variant, kept));
        }
    }
    let (atlas, entry, taken) = match held {
        Some(atlas) => {
            #[cfg(feature = "dev-tools")]
            {
                let pages = atlas.shared_pages();
                crate::dev::loop_probe::mark(
                    "rl:held_pages",
                    i64::try_from(pages.pages().len()).unwrap_or(-1),
                );
                crate::dev::loop_probe::mark("rl:held_side", i64::from(pages.side()));
                crate::dev::loop_probe::mark(
                    "rl:held_level_bytes",
                    crate::dev::loop_probe::pages_level_bytes(pages),
                );
            }
            (atlas, entry, None)
        }
        None => {
            drop(entry);
            let limits = pending.target.limits;
            // Under the lock, only snapshots: the earlier entry's `Arc`
            // and the kept cells.
            let (earlier_entry, kept_cells) = lock(slot)
                .as_ref()
                .map_or((None, None), |i| (i.entry.clone(), i.cells.clone()));
            // The union's base: the shown atlas's cells, the image entry's
            // and the kept ones, which the build keeps as cells only.
            let earlier = base.is_some() || earlier_entry.is_some() || kept_cells.is_some();
            let own = skin.sheet().clone();
            let mut union = earlier.then(|| own.clone());
            for b in [base.as_ref(), earlier_entry.as_ref()]
                .into_iter()
                .flatten()
            {
                if ask(check, StepSite::Union) {
                    return Err(stopped());
                }
                union = match union.map(|u| u.union_checked(&b.packed, check)) {
                    Some(Err(_)) => return Err(stopped()),
                    Some(Ok(u)) => u,
                    None => None,
                };
            }
            if let Some(c) = &kept_cells {
                if ask(check, StepSite::Union) {
                    return Err(stopped());
                }
                union = match union.map(|u| u.union_checked(c, check)) {
                    Some(Err(_)) => return Err(stopped()),
                    Some(Ok(u)) => u,
                    None => None,
                };
            }
            if earlier && ask(check, StepSite::UnionEnd) {
                return Err(stopped());
            }
            let cancelled =
                |e: &skin_model::AtlasError| matches!(e, skin_model::AtlasError::Cancelled);
            let union_plan = match union
                .as_ref()
                .map(|u| u.plan_atlas(variant, limits, kept, check))
            {
                Some(Ok(p)) => Some(p),
                Some(Err(e)) if cancelled(&e) => return Err(stopped()),
                Some(Err(_)) | None => None,
            };
            let from_union = union_plan.is_some();
            let plan = match union_plan {
                Some(p) => {
                    #[cfg(feature = "dev-tools")]
                    crate::dev::loop_probe::mark("rl:branch_union", 1);
                    p
                }
                None => match own.plan_atlas(variant, limits, kept, check) {
                    Ok(p) => {
                        #[cfg(feature = "dev-tools")]
                        {
                            crate::dev::loop_probe::mark("rl:branch_own", i64::from(earlier));
                            // The production plan's shape: no packing of its
                            // own.
                            let sh = p.shape();
                            crate::dev::loop_probe::mark("rl:own_plan_pages", i64::from(sh.pages));
                            crate::dev::loop_probe::mark("rl:own_plan_side", i64::from(sh.side));
                            crate::dev::loop_probe::mark(
                                "rl:own_plan_layers",
                                i64::from(sh.layers),
                            );
                            crate::dev::loop_probe::mark(
                                "rl:own_plan_texels",
                                i64::try_from(sh.texels).unwrap_or(-1),
                            );
                        }
                        p
                    }
                    Err(e) if cancelled(&e) => return Err(stopped()),
                    Err(_) => {
                        #[cfg(feature = "dev-tools")]
                        crate::dev::loop_probe::mark("rl:branch_refused", 1);
                        return Err(Box::new(NotBound::refused(
                            "the layout's images do not fit one atlas".to_owned(),
                        )));
                    }
                },
            };
            // The release's union of the kept cells with the entry it
            // drops, made here from the snapshot so it asks too.
            let Ok(merged) = dropped_union(
                kept_cells.as_ref(),
                earlier_entry.as_ref(),
                base.as_ref(),
                check,
            ) else {
                return Err(stopped());
            };
            // The snapshot goes before the release, so the release's drop
            // of the earlier entry frees its pages before the build.
            drop((earlier_entry, kept_cells));
            if ask(check, StepSite::Take) {
                return Err(stopped());
            }
            // The plan fits: the holdings are taken, and what is not shown
            // goes before the build.
            let images = lock(slot).take().unwrap_or_default();
            let (images, room) = release(images, base.as_ref(), others, merged);
            #[cfg(all(test, feature = "dev-tools"))]
            tests::released();
            if !room {
                return Err(Box::new(NotBound {
                    why: Unbuilt::Deferred,
                    lent: Some(images),
                    built: None,
                }));
            }
            let kept_cells = images.cells;
            drop(images.entry);
            #[cfg(all(test, feature = "dev-tools"))]
            tests::build_gate(cancel);
            #[cfg(any(test, feature = "dev-tools"))]
            BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            #[cfg(feature = "dev-tools")]
            gaps::build();
            #[cfg(feature = "dev-tools")]
            let p_t = render::probe::Span::start();
            let built = match plan.build(check) {
                Ok(a) => {
                    #[cfg(feature = "dev-tools")]
                    {
                        crate::dev::loop_probe::mark(
                            "rl:build_us",
                            crate::dev::loop_probe::us(p_t.elapsed()),
                        );
                        let pages = a.shared_pages();
                        crate::dev::loop_probe::mark(
                            "rl:packed_pages",
                            i64::try_from(pages.pages().len()).unwrap_or(-1),
                        );
                        crate::dev::loop_probe::mark("rl:packed_side", i64::from(pages.side()));
                        crate::dev::loop_probe::mark(
                            "rl:packed_level_bytes",
                            crate::dev::loop_probe::pages_level_bytes(pages),
                        );
                    }
                    a
                }
                Err(e) => {
                    return Err(Box::new(NotBound {
                        why: format!("the atlas cannot be built: {e}").into(),
                        lent: Some(Images {
                            cells: kept_cells,
                            ..Images::default()
                        }),
                        built: None,
                    }));
                }
            };
            // The plan's borrow ended with the build: the sheet it packed
            // is the entry's.
            let packed = match union {
                Some(u) if from_union => u,
                _ => own,
            };
            let new = Arc::new(ImageEntry {
                atlas: built,
                packed,
                #[cfg(feature = "dev-tools")]
                import_permit: IMPORT_PERMIT.get(),
                #[cfg(all(test, feature = "dev-tools"))]
                _dropped: preload::tests::DropWitness,
            });
            #[cfg(all(test, feature = "dev-tools"))]
            preload::tests::built_gate();
            let taken = Some(Images {
                entry: Some(Arc::clone(&new)),
                ..Images::default()
            });
            match new.atlas.rebind_keeping(skin.sheet(), variant, kept) {
                Ok(a) => (a, Some(new), taken),
                Err(e) => {
                    return Err(Box::new(NotBound {
                        why: format!("the layout cannot be bound: {e}").into(),
                        lent: None,
                        built: taken,
                    }));
                }
            }
        }
    };
    Ok(Bound {
        atlas,
        entry,
        taken,
        needs: Needs {
            sheet: skin.sheet().clone(),
            variant,
            kept,
        },
    })
}

/// Marks a result layout pass's end on the run's record as it drops, on
/// every way out of the job (dev-tools).
#[cfg(feature = "dev-tools")]
struct LayoutEnded;

#[cfg(feature = "dev-tools")]
impl Drop for LayoutEnded {
    fn drop(&mut self) {
        crate::dev::transitions::game_mark_now("result-layout", 1);
    }
}

impl ResultSkin {
    /// The kind's skin from startup.
    pub fn new(start: Option<ResultStart>) -> Self {
        let mut s = Self::default();
        s.start = start;
        s
    }

    /// The result runtimes' cell, the render thread's ([`Scripts`]): the
    /// generations it gives are that cell's.
    #[must_use]
    pub fn with_scripts(mut self, scripts: Scripts) -> Self {
        self.scripts = scripts;
        self
    }

    /// The pending skin, if the kind has one.
    fn pending(&self) -> Option<&Arc<PendingResult>> {
        match &self.start {
            Some(ResultStart::Pending(p)) => Some(p),
            _ => None,
        }
    }

    /// The atlases a build at `stage` counts: [`others`](Self::others),
    /// with what the no-room line names in development builds.
    fn counted(&self, stage: &'static str) -> Others {
        #[cfg(not(feature = "dev-tools"))]
        let _ = stage;
        Others {
            ids: self.others(),
            #[cfg(feature = "dev-tools")]
            line: RoomLine {
                stage,
                holders: self.holders.clone(),
                game: self.game_holdings(),
            },
        }
    }

    /// Development builds: the game's own holdings among the atlases a
    /// build counts: the latest publication's binding and the layout
    /// entries'.
    #[cfg(feature = "dev-tools")]
    fn game_holdings(&self) -> Vec<AtlasId> {
        let mut v = Vec::new();
        if let Some(Published::Skin(b, _, _)) = &self.published {
            v.push(b.atlas_pages().id());
        }
        for l in self.layout.iter().chain(&self.presented) {
            v.push(l.bundle.atlas_pages().id());
        }
        v
    }

    /// Development builds: the render side's holders of the atlases the
    /// report names ([`crate::result_present::ResultReport::holders`]).
    #[cfg(feature = "dev-tools")]
    pub fn set_holders(&mut self, holders: &[(AtlasId, crate::result_present::Holders)]) {
        self.holders.clear();
        self.holders.extend_from_slice(holders);
    }

    /// Development builds: the stage the next view's build is asked for,
    /// as the no-room line names it.
    #[cfg(feature = "dev-tools")]
    pub const fn set_stage(&mut self, stage: &'static str) {
        self.stage = stage;
    }

    /// The atlases live besides the image entry: the renderer's retained
    /// set, the latest publication's binding and the layout entry's.
    fn others(&self) -> Vec<AtlasId> {
        let mut v = self.retained.clone();
        if let Some(Published::Skin(b, _, _)) = &self.published {
            v.push(b.atlas_pages().id());
        }
        for l in self.layout.iter().chain(&self.presented) {
            v.push(l.bundle.atlas_pages().id());
        }
        v.sort_unstable();
        v.dedup();
        v
    }

    /// The atlases the renderer still references, in any role (installed,
    /// staged or taken and not yet presented, kept by a recovery
    /// description): it replaces the set reported before. Only a later
    /// report without an id releases it here. A deferred build is tried
    /// again once fewer than two atlases are live (the S10-09i design's
    /// Q3).
    pub fn set_retained(&mut self, ids: &[AtlasId]) {
        self.note_retained(ids);
        self.forget_unpresented();
        if self.others().len() < 2 {
            self.retry_deferred();
        }
    }

    /// Whether the kept probe fits, decided from the charges the
    /// renderer last reported ([`kept_probe_fits`]), at startup on the
    /// selection path and before each image stage, when no job runs. A
    /// change of a decision made drops what was packed, bound or imported
    /// the other way: the preload, the probe's record, the prepared
    /// atlas, the kept cells, and the cached layouts and the publication
    /// bound to a prepared atlas (the publication's would be the next
    /// probe's union base), so no later probe or view packs or binds cells
    /// only the other mode keeps. A bundle for every view of a skin that
    /// reads no fact of the viewed result has its own atlas, the same in
    /// either mode, and stays. The first decision drops nothing. What is
    /// dropped goes to the preload's disposal thread once a pack began
    /// (a collected pack's atlas among it), never freed here (design.md
    /// "beatoraja Lua Skins", the result skin's preload).
    pub fn set_kept_probe(&mut self, kept: bool) {
        if self.kept.replace(kept).is_some_and(|was| was != kept) {
            // A running pack is stopped, its holdings quarantined until its
            // outcome is collected and disposed of; nothing of it is kept.
            self.preload_on(Event::Invalidate);
            let dropped = (
                self.probe.take(),
                self.image.take(),
                self.cells.take(),
                self.layout.take_if(|l| l.entry.is_some()),
                self.presented.take_if(|l| l.entry.is_some()),
                self.published
                    .take_if(|p| matches!(p, Published::Skin(_, Some(_), _))),
            );
            self.dispose(dropped);
        }
    }

    /// The startup probe's `outcome` ([`imports::result_probe`]): a probe
    /// that read the viewed result waits for its pack; any other outcome
    /// leaves no preload, the skin taking its image stage at each attempt.
    pub fn set_preload(&mut self, outcome: ProbeOutcome) {
        self.preload = match outcome {
            ProbeOutcome::Probe(p) => {
                log::info!(
                    "skin: result: preloaded: {} probe, {} facts read",
                    if p.kept() { "kept" } else { "plain" },
                    p.deps.reads.len()
                );
                self.control = Some(Arc::default());
                Preload::Idle(p)
            }
            ProbeOutcome::Stopped(fact) => {
                log::info!("skin: result: not preloaded: it reads {fact:?}, an attempt's input");
                Preload::Retired
            }
            ProbeOutcome::Refused(why) => {
                log::info!("skin: result: not preloaded: {why}");
                Preload::Retired
            }
            ProbeOutcome::Bundle => Preload::Retired,
        };
    }

    /// The preload's probe mode while it waits for its pack, kept or
    /// plain; `None` with no preload (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub const fn preloaded(&self) -> Option<bool> {
        match &self.preload {
            Preload::Idle(p) => Some(p.kept),
            Preload::Running(r) | Preload::Stopping { run: r, .. } => Some(r.kept()),
            Preload::Retired => None,
        }
    }

    /// The pending skin startup left, if any.
    pub fn pending_start(&self) -> Option<Arc<PendingResult>> {
        self.pending().map(Arc::clone)
    }

    /// A fresh report of the atlases the renderer references, read before
    /// a build is decided (a view's request, the image stage's begin;
    /// design.md "Memory Budget", the result's atlas report): no deferred
    /// build is tried again here, since a request that follows supersedes
    /// it.
    pub fn note_retained(&mut self, ids: &[AtlasId]) {
        self.retained.clear();
        self.retained.extend_from_slice(ids);
    }

    /// The deferred request, asked for again unless a job runs.
    fn retry_deferred(&mut self) {
        if self.job.is_none()
            && let Some((request, view)) = self.deferred.take()
            && self.latest.map(|(r, _)| r) == Some(request)
        {
            self.layout_for(request, view);
        }
    }

    /// The committed attempt's images, from its commit on: its
    /// sources and its result half, replacing the last attempt's.
    pub fn take_images(&mut self, images: Option<crate::chart_images::ResultImages>) {
        self.images = images.map(|i| HeldImages {
            sources: i.sources,
            half: i.half,
            side: i.side,
        });
        log::info!(
            "chart images: the result keeps {} bytes",
            self.image_bytes()
        );
    }

    /// The attempt's images go: selection or practice is entered.
    ///
    /// The one exit of every image the result keeps: the sources and the
    /// result half, and each kept layout's derivation (a layout kept for
    /// reuse keeps its bundle and facts, and derives again if it is
    /// reused with other images).
    pub fn drop_images(&mut self) {
        self.images = None;
        for l in self.layout.iter_mut().chain(self.presented.iter_mut()) {
            l.derived = None;
        }
    }

    /// The bytes of the images it keeps: the sources, the result half and
    /// the layouts' derivations, each once (the memory witness).
    #[must_use]
    pub fn image_bytes(&self) -> u64 {
        let mut seen: Vec<&Arc<ChartImages>> = Vec::new();
        let sets = self
            .images
            .iter()
            .flat_map(|h| std::iter::once(&h.sources).chain(h.half.as_ref()))
            .chain(
                self.layout
                    .iter()
                    .chain(&self.presented)
                    .filter_map(|l| l.derived.as_ref().map(|(_, d)| d)),
            );
        let mut total = 0_u64;
        for s in sets {
            if !seen.iter().any(|k| Arc::ptr_eq(k, s)) {
                total = total.saturating_add(s.bytes());
                seen.push(s);
            }
        }
        total
    }

    /// The chart layer side the attempt's result half is derived for:
    /// a bundle for every view's, else the prepared atlas's, when
    /// it keeps a chart layer; `None` otherwise (a layout then derives
    /// its own from the sources).
    #[must_use]
    pub fn chart_side(&self) -> Option<u32> {
        match &self.attempt {
            Some(ForAttempt::Every(b, _, _)) => b.chart_layer().map(|_| b.side()),
            Some(ForAttempt::Pending) => self
                .image
                .as_ref()
                .filter(|e| e.atlas.chart_layer().is_some())
                .map(|e| e.atlas.side()),
            Some(ForAttempt::BuiltIn) | None => None,
        }
    }

    /// The chart's images a payload of `bundle` draws: the result half
    /// when it is fitted for `bundle`'s chart slot, else the derivation
    /// its layout made from this attempt's sources; `None` for a bundle
    /// with no chart layer, or before the attempt's commit. The one
    /// function a payload, a view's reuse and an attempt's reuse take a
    /// fit from.
    #[must_use]
    pub fn chart_for(&self, bundle: &Arc<SkinBundle>) -> Option<Arc<ChartImages>> {
        let held = self.images.as_ref()?;
        bundle.chart_layer()?;
        let side = bundle.side();
        if held.side == Some(side)
            && let Some(h) = &held.half
        {
            return Some(Arc::clone(h));
        }
        self.layout
            .iter()
            .chain(&self.presented)
            .find(|l| Arc::ptr_eq(&l.bundle, bundle))
            .and_then(|l| l.derived.as_ref())
            .filter(|(id, _)| *id == held.sources.id())
            .map(|(_, d)| Arc::clone(d))
    }

    /// Whether `l`'s images are this attempt's: it keeps no chart layer,
    /// the attempt's images are not handed over yet, the result half fits
    /// its slot, or its derivation is of these sources.
    fn chart_ready(&self, l: &Specialized) -> bool {
        let Some(held) = &self.images else {
            return true;
        };
        l.bundle.chart_layer().is_none()
            || held.side == Some(l.bundle.side())
            || l.derived
                .as_ref()
                .is_some_and(|(id, _)| *id == held.sources.id())
    }

    /// What a layout job derives from: the attempt's sources and the
    /// result half's side.
    fn chart_from(&self) -> Option<ChartFrom> {
        self.images.as_ref().map(|h| ChartFrom {
            sources: Arc::clone(&h.sources),
            side: h.side,
        })
    }

    /// The published generation's prepared atlas.
    fn shown_entry(&self) -> Option<Arc<ImageEntry>> {
        match &self.published {
            Some(Published::Skin(_, e, _)) => e.clone(),
            _ => None,
        }
    }

    /// What the result slot holds, and its generation.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn published(&self) -> (Option<&Published>, u64) {
        (self.published.as_ref(), self.generation)
    }

    /// What the result slot holds: the latest request's publication once
    /// [`poll`](Self::poll) says it is published.
    pub const fn shown(&self) -> Option<&Published> {
        self.published.as_ref()
    }

    /// Whether the kind has a skin, fixed or laid out per view.
    pub const fn has_skin(&self) -> bool {
        self.start.is_some()
    }

    /// What the attempt's result uploads after its commit (the S10-09i
    /// design's D2): a fixed skin's or a chart-only specialization's
    /// bundle, or a prepared atlas's pages; nothing for the built-in
    /// result or before an image stage.
    pub fn upload(&self) -> Option<Upload> {
        match &self.attempt {
            Some(ForAttempt::Every(b, _, _)) => Some(Upload::Bundle(Arc::clone(b))),
            Some(ForAttempt::Pending) => self
                .image
                .as_ref()
                .map(|e| Upload::Pages(Arc::clone(e.pages()))),
            Some(ForAttempt::BuiltIn) | None => None,
        }
    }

    /// The prepared atlas, if one is live.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn image(&self) -> Option<&Arc<ImageEntry>> {
        self.image.as_ref()
    }

    fn publish(&mut self, p: Published) {
        #[cfg(feature = "dev-tools")]
        println!(
            "dev: result skin published: {}",
            match &p {
                Published::BuiltIn => "built-in",
                Published::Skin(..) => "skin",
            }
        );
        // The cell's publication protects its runtime from now, and only
        // it, until the next publication or attempt.
        self.scripts.set_published(match &p {
            Published::Skin(_, _, g) => *g,
            Published::BuiltIn => None,
        });
        self.published = Some(p);
        self.generation += 1;
    }

    /// Whether the attempt's image stage chose the built-in result: every
    /// view of this attempt is published as it.
    pub fn attempt_built_in(&self) -> bool {
        matches!(self.attempt, Some(ForAttempt::BuiltIn))
    }

    /// The attempt's result skin as its image stage left it, and whether a
    /// prepared atlas is live (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn staged_line(&self) -> String {
        let attempt = match &self.attempt {
            None => "none",
            Some(ForAttempt::Every(..)) => "every view",
            Some(ForAttempt::BuiltIn) => "built-in",
            Some(ForAttempt::Pending) => "per view",
        };
        let images = if self.image.is_some() {
            "prepared"
        } else {
            "none"
        };
        format!("{attempt}, images {images}")
    }

    /// The image stage of an attempt whose chart gives `chart` and the
    /// judgment class `class` (D11): the probe import and, unless the live
    /// prepared atlas holds its cells, one atlas build, on a job run with
    /// the attempt's two signals. A fixed skin, or a probe whose facts and
    /// atlas still hold, runs no job.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn image_stage(
        &mut self,
        chart: ResultChart,
        class: Option<JudgeClass>,
        mut quit: impl FnMut() -> bool,
        mut end_run: impl FnMut() -> bool,
    ) -> Staged {
        match self.begin_image_stage(chart, class, None) {
            ImageStage::Ready(staged) => staged,
            ImageStage::Running(mut job) | ImageStage::Adopted(mut job) => loop {
                if let Some(staged) = self.poll_image_stage(&mut job, None, &mut quit, &mut end_run)
                {
                    break staged;
                }
            },
        }
    }

    /// [`image_stage`](Self::image_stage)'s start: decided at once when no
    /// job is needed, else its job started and returned at once
    /// ([`ImageStage::Running`]), to be waited for
    /// ([`poll_image_stage`](Self::poll_image_stage)), which finishes it.
    /// With `claim`, the choice's claim on the preload, a running pack of
    /// the mode now preferred is adopted instead ([`ImageStage::Adopted`]):
    /// no import or build of the stage's own (design.md "beatoraja Lua
    /// Skins", the result skin's preload).
    pub fn begin_image_stage(
        &mut self,
        chart: ResultChart,
        class: Option<JudgeClass>,
        claim: Option<PreloadClaim>,
    ) -> ImageStage {
        self.chart = Some(chart);
        self.class = Some(class);
        // A pack that ended is collected first: packed, it is reused below.
        self.collect_preload(std::time::Duration::ZERO);
        if let Some(adopted) = self.adopt_preload(claim) {
            return ImageStage::Adopted(Box::new(ImageJob(InFlight::Adopted(adopted))));
        }
        self.attempt_stage()
    }

    /// The attempt's image stage for the chart and judgment class set,
    /// with no pack to adopt ([`Event::Attempt`]): the stage ends the
    /// preload (design.md "beatoraja Lua Skins", the result skin's
    /// preload). A pack still in flight (an unclaimed one, which the
    /// pause's acknowledgement before Preparing leaves out of its build,
    /// or one whose mode is no longer preferred) is stopped, and the stage
    /// first waits for its outcome to be collected in Preparing's polls
    /// ([`ImageStage::Running`]): nothing else takes the image state
    /// meanwhile. A packed one is reused below.
    fn attempt_stage(&mut self) -> ImageStage {
        self.preload_on(Event::Attempt);
        if let Some(id) = self.pack_in_flight() {
            return ImageStage::Running(Box::new(ImageJob(InFlight::Draining(id))));
        }
        let (Some(chart), Some(class)) = (self.chart, self.class) else {
            return ImageStage::Ready(Staged::Ready);
        };
        #[cfg(feature = "dev-tools")]
        {
            crate::dev::loop_probe::mark(
                "img:begin",
                i64::from(self.probe.is_some()) | (i64::from(self.image.is_some()) << 1),
            );
            if crate::dev::loop_probe::on() {
                println!("dev: probe-chart {chart:?} class {class:?}");
            }
        }
        let pending = match &self.start {
            None => return ImageStage::Ready(Staged::Ready),
            Some(ResultStart::Fixed(b, script)) => {
                self.attempt = Some(ForAttempt::Every(Arc::clone(b), None, *script));
                return ImageStage::Ready(Staged::Ready);
            }
            Some(ResultStart::Pending(p)) => Arc::clone(p),
        };
        let facts = result_facts(probe_fields(chart), self.class);
        #[cfg(feature = "dev-tools")]
        if crate::dev::loop_probe::on()
            && let (Some((d, n)), Some(e)) = (&self.probe, &self.image)
        {
            crate::dev::loop_probe::mark("img:deps_hold", i64::from(d.holds(&facts)));
            crate::dev::loop_probe::mark("img:cells_held", i64::from(n.held_by(e)));
            for r in &d.reads {
                let one = FactDeps {
                    scene: d.scene,
                    reads: vec![*r],
                };
                if !one.holds(&facts) {
                    println!("dev: probe-facts differs {r:?}");
                }
            }
        }
        if let Some(reused) = self.reuse_for_attempt(&facts) {
            #[cfg(feature = "dev-tools")]
            {
                crate::dev::loop_probe::mark(
                    "img:reused",
                    i64::from(matches!(&reused, ForAttempt::Every(..))),
                );
                // Decided at once from the resident probe and atlas.
                crate::dev::loop_probe::mark("img:resident", i64::from(self.image.is_some()));
            }
            self.attempt = Some(reused);
            return ImageStage::Ready(Staged::Ready);
        }
        // A bundle for every view keeps its runtime: the probe's import
        // keeps one under a permit, taken after the eviction; laid out per
        // view, it goes on the job's thread.
        let permit = self.scripts.result_permit(&[]);
        let slot = self.slot_for_job();
        let theirs = Arc::clone(&slot);
        let base = self.shown_entry();
        let others = self.counted("the image stage");
        let kept = self.kept == Some(true);
        // The stage's own import and build (dev-tools: whether it keeps a
        // runtime).
        #[cfg(feature = "dev-tools")]
        crate::dev::loop_probe::mark("img:job", i64::from(permit.is_some()));
        let spawned = imports::spawn_job(move |cancel: &Cancel| -> ImageOut {
            let slot = theirs;
            let o = if permit.is_some() {
                pending
                    .header
                    .import_runtime_reading(&pending.config, &facts, cancel)
            } else {
                pending
                    .header
                    .import_reading(&pending.config, &facts, cancel)
            };
            let deps = match &o {
                Ok(LuaOutcome::Imported(i)) => i.facts.clone(),
                Ok(LuaOutcome::Pending(p)) => p.facts.clone(),
                Err(refused) => refused.1.clone(),
            };
            let read = reads_result(&deps);
            #[cfg(feature = "dev-tools")]
            {
                crate::dev::loop_probe::mark(
                    "img:import_end",
                    i64::try_from(deps.reads.len()).unwrap_or(-1),
                );
                if crate::dev::loop_probe::on() {
                    println!("dev: probe-facts reads_view={read} {deps:?}");
                }
            }
            match o {
                Ok(LuaOutcome::Imported(mut i)) if !read => {
                    let script = ScriptSlot::staged_with(i.runtime.take(), permit);
                    // A skin that reads no fact of the viewed result:
                    // one bundle for every view, packed as any kind's
                    // once its plan fits and what is not shown is
                    // released; with no room, laid out per view.
                    let mut skin = i.imported.skin;
                    let variant = skin.variant(pending.target.size, &Layout::AUTHOR);
                    let limits = pending.target.limits;
                    let (kept, _) = imports::plan_layers(&mut skin, variant, limits);
                    if let Err(e) = skin.sheet().plan_keeping(variant, limits, kept) {
                        return (
                            Some(Err(format!("the atlas cannot be built: {e}"))),
                            deps,
                            None,
                            None,
                            ScriptSlot::default(),
                        );
                    }
                    let images = lock(&slot).take().unwrap_or_default();
                    let (images, room) = release(images, base.as_ref(), others, None);
                    if !room {
                        return (None, deps, Some(images), None, ScriptSlot::default());
                    }
                    #[cfg(any(test, feature = "dev-tools"))]
                    BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let r = skin
                        .atlas(variant, pending.target.limits, &|| cancel.is_cancelled())
                        .map_err(|e| format!("the atlas cannot be built: {e}"))
                        .and_then(|a| {
                            imports::bundle_of(skin, i.imported.scratch, a, &pending.target, cancel)
                        });
                    let script = if r.is_ok() {
                        script
                    } else {
                        ScriptSlot::default()
                    };
                    (Some(r.map(Some)), deps, Some(images), None, script)
                }
                Ok(LuaOutcome::Imported(mut i)) => {
                    // A skin that reads the viewed result: the probe's
                    // layout is never shown, its atlas is. While the kept
                    // probe fits ([`kept_probe_fits`]) it is laid out from
                    // this import, made in the shown layouts' mode when
                    // the permit came, so its atlas holds the cells a
                    // runtime's callback draws; the runtime goes and the
                    // permit is released here, before the pack. Else from
                    // a second, plain import, which keeps no such cells.
                    drop(i.runtime.take());
                    drop(permit);
                    let laid = if kept {
                        lay_out(&pending, i, &slot, base, others, cancel, &|| {
                            cancel.is_cancelled()
                        })
                    } else {
                        drop(i);
                        let view = ViewFacts(Some(match facts.result {
                            Fact::Known(f) => f,
                            _ => return (None, deps, None, None, ScriptSlot::default()),
                        }));
                        layout(
                            &pending,
                            view,
                            Some(class),
                            &slot,
                            base,
                            others,
                            cancel,
                            None,
                            None,
                        )
                        .0
                    };
                    (
                        Some(laid.result.map(|_| None).map_err(|e| match e {
                            Unbuilt::Refused(why) => why,
                            Unbuilt::Deferred => "deferred".to_owned(),
                        })),
                        deps,
                        laid.taken,
                        laid.needs,
                        ScriptSlot::default(),
                    )
                }
                Err(_) if !read => (
                    Some(Err("refused".to_owned())),
                    deps,
                    None,
                    None,
                    ScriptSlot::default(),
                ),
                _ => (None, deps, None, None, ScriptSlot::default()),
            }
        });
        match spawned {
            Ok(job) => ImageStage::Running(Box::new(ImageJob(InFlight::Own { job, slot }))),
            Err(why) => ImageStage::Ready(self.finish_image_stage(&slot, Ran::Refused(why))),
        }
    }

    /// The image stage's `job` waited for until `until` at most
    /// ([`imports::Job::wait_until`] with the attempt's two signals), and
    /// finished once it ended, its images handed back unless it was
    /// abandoned; `None` at `until` with the job running. Once either
    /// signal came the attempt is cancelled.
    pub fn poll_image_stage(
        &mut self,
        job: &mut ImageJob,
        until: Option<std::time::Instant>,
        quit: impl FnMut() -> bool,
        end_run: impl FnMut() -> bool,
    ) -> Option<Staged> {
        let (mut quit, mut end_run) = (quit, end_run);
        loop {
            let id = match &mut job.0 {
                InFlight::Own { job, slot } => {
                    let ran = job.wait_until(until, &mut quit, &mut end_run)?;
                    let staged = self.finish_image_stage(slot, ran);
                    return Some(if job.signaled() {
                        Staged::Cancelled
                    } else {
                        staged
                    });
                }
                InFlight::Adopted(a) => a.id(),
                InFlight::Draining(id) => *id,
            };
            if !self.wait_pack(id, until, &mut quit, &mut end_run)? {
                return Some(Staged::Cancelled);
            }
            // Collected: the stage is decided from what the pack left, its
            // own job begun only when it left nothing.
            match self.attempt_stage() {
                ImageStage::Ready(staged) => return Some(staged),
                ImageStage::Running(next) | ImageStage::Adopted(next) => *job = *next,
            }
        }
    }

    /// An image stage's `job` no attempt waits for any more (its
    /// preparation superseded or discarded), settled as the attempt's quit
    /// settles it: cancelled and waited for to its thread's end without a
    /// bound, while `end_run` (a close, a thread's failure) is still asked;
    /// once it turns true, the job is given up within
    /// [`imports::STOP_WAIT`] of it. A job that ended is finished, so its
    /// images are handed back; an abandoned one keeps them. An adopted
    /// pack is handed back to the preload as its claim drops, its thread
    /// not waited for, unless the run ends: then it is cancelled and
    /// waited for until the run's shared stop deadline. A stage waiting
    /// for a stopped pack's collection leaves it to the preload the same
    /// way.
    pub fn settle_image_stage(
        &mut self,
        job: &mut ImageJob,
        mut end_run: impl FnMut() -> bool,
    ) -> imports::Settled {
        let (job, slot) = match &mut job.0 {
            InFlight::Own { job, slot } => (job, slot),
            InFlight::Adopted(_) | InFlight::Draining(_) => {
                if end_run() {
                    self.end_preload(imports::STOP_WAIT);
                }
                return imports::Settled::Ended;
            }
        };
        // Without a deadline the wait returns only with the job's end.
        let Some(ran) = job.wait_until(None, || true, end_run) else {
            return imports::Settled::Ended;
        };
        let settled = if matches!(ran, Ran::Abandoned) {
            imports::Settled::Abandoned
        } else {
            imports::Settled::Ended
        };
        let _ = self.finish_image_stage(slot, ran);
        settled
    }

    /// The attempt's result skin from its image stage's outcome `ran`, the
    /// job's images handed back through `slot`.
    fn finish_image_stage(&mut self, slot: &Slot, ran: Ran<ImageOut>) -> Staged {
        match ran {
            Ran::Done((outcome, deps, taken, needs, script)) => {
                self.restore(taken, slot);
                match outcome {
                    // A bundle for every view, cached with what it read and
                    // its runtime's generation.
                    Some(Ok(Some(bundle))) => {
                        let script = self.scripts.offer_result(script, false);
                        self.attempt = Some(ForAttempt::Every(Arc::clone(&bundle), None, script));
                        self.cache(Specialized {
                            bundle,
                            facts: deps,
                            entry: None,
                            script,
                            derived: None,
                        });
                        self.probe = None;
                    }
                    // Images prepared: laid out per view.
                    Some(Ok(None)) => {
                        self.attempt = Some(ForAttempt::Pending);
                        self.probe = needs.map(|n| (deps, n));
                    }
                    // Refused with no result fact read.
                    Some(Err(why)) if !reads_result(&deps) => {
                        log::info!("skin: result: built-in for this attempt: {why}");
                        self.attempt = Some(ForAttempt::BuiltIn);
                        self.probe = None;
                    }
                    // Refused after a result fact, or the probe's images do
                    // not fit: nothing prepared, laid out per view.
                    _ => {
                        self.attempt = Some(ForAttempt::Pending);
                        self.probe = None;
                    }
                }
                Staged::Ready
            }
            Ran::Refused(why) => {
                log::info!("skin: result: its image stage failed: {why}");
                self.restore(None, slot);
                self.attempt = Some(ForAttempt::Pending);
                Staged::Ready
            }
            Ran::Abandoned => Staged::Cancelled,
        }
    }

    /// Asks for `view`'s result skin (P's own view, then each switch):
    /// published at once when nothing needs a job, else the job is started,
    /// or the request waits, replacing the one waiting (D11, "Requests").
    /// Returns the request.
    pub fn request(&mut self, view: ViewFacts) -> u64 {
        #[cfg(feature = "dev-tools")]
        if crate::dev::loop_probe::on() {
            match &view.0 {
                Some(f) => println!(
                    "dev: probe-view {}",
                    f.0.iter().map(i32::to_string).collect::<Vec<_>>().join(" ")
                ),
                None => println!("dev: probe-view unavailable"),
            }
        }
        self.next_request += 1;
        let request = self.next_request;
        self.latest = Some((request, view));
        // A newer request supersedes the deferred one.
        self.deferred = None;
        match self.attempt.clone() {
            None => {}
            Some(ForAttempt::Every(b, entry, script)) => {
                if matches!(&self.published, Some(Published::Skin(p, _, _)) if Arc::ptr_eq(p, &b)) {
                    // Published already, maybe in an earlier attempt: no
                    // new publication, but its runtime is this attempt's
                    // publication, which protects it (code r4).
                    self.scripts.set_published(script);
                } else {
                    self.publish(Published::Skin(b, entry, script));
                }
            }
            Some(ForAttempt::BuiltIn) => self.publish(Published::BuiltIn),
            Some(ForAttempt::Pending) => self.layout_for(request, view),
        }
        request
    }

    /// The layout of a pending skin for `view`, request `request`. One
    /// published at once supersedes the request waiting or deferred.
    fn layout_for(&mut self, request: u64, view: ViewFacts) {
        if let Some(p) = self.reuse_for_view(view) {
            self.supersede(p);
            return;
        }
        if view.0.is_none() {
            // An unavailable view with nothing to reuse: the built-in
            // result.
            self.supersede(Published::BuiltIn);
            return;
        }
        if self.job.is_some() {
            self.waiting = Some((request, view));
            return;
        }
        self.start_job(request, view);
    }

    fn start_job(&mut self, request: u64, view: ViewFacts) {
        let Some(pending) = self.pending().map(Arc::clone) else {
            return;
        };
        // Reuse was checked first (`layout_for`); then the eviction, which
        // spares the published layout's runtime (the renderer may still
        // present it) besides the presented and the fixed ones, and the
        // permit, without which the request waits for one. The cached
        // layout holds no later view here (else it was reused), so it is
        // not spared: kept, it could hold a permit no view releases.
        let Some(permit) = self.scripts.result_permit(&[]) else {
            self.permit_wait = Some((request, view));
            return;
        };
        // The entry goes to the job, which hands it back; the layout entry
        // bound to an unshown one goes with it, dropped only with its pages.
        let slot = self.slot_for_job();
        let theirs = Arc::clone(&slot);
        let base = self.shown_entry();
        #[cfg(feature = "dev-tools")]
        let stage = if self.stage.is_empty() {
            "a view's build"
        } else {
            self.stage
        };
        #[cfg(not(feature = "dev-tools"))]
        let stage = "";
        let others = self.counted(stage);
        let class = self.class;
        let chart = self.chart_from();
        // The game thread, woken once the outcome is in.
        let wake = self.wake.clone();
        match imports::spawn_job_with(
            move |cancel: &Cancel| {
                // The layout's pass on the run's record (dev-tools): its
                // start and end, so a measurement knows when its Lua ran.
                #[cfg(feature = "dev-tools")]
                crate::dev::transitions::game_mark_now("result-layout", 0);
                #[cfg(feature = "dev-tools")]
                let _ended = LayoutEnded;
                let (laid, script) = layout(
                    &pending,
                    view,
                    class,
                    &theirs,
                    base,
                    others,
                    cancel,
                    Some(permit),
                    chart.as_ref(),
                );
                LayoutOut {
                    request,
                    view,
                    laid,
                    script,
                }
            },
            move || {
                if let Some(t) = wake {
                    t.unpark();
                }
            },
        ) {
            Ok(job) => self.job = Some((job, request, slot)),
            Err(why) => {
                log::info!("skin: result: its layout cannot start: {why}");
                self.restore(None, &slot);
                self.publish(Published::BuiltIn);
            }
        }
    }

    /// Publishes `p` for the latest request: an older request waiting or
    /// deferred is dropped.
    fn supersede(&mut self, p: Published) {
        self.waiting = None;
        self.deferred = None;
        self.permit_wait = None;
        self.publish(p);
    }

    /// The facts of the attempt's probe: its chart's, every result value 1.
    fn attempt_facts(&self) -> Option<ImportFacts> {
        self.chart
            .map(|c| result_facts(probe_fields(c), self.class))
    }

    /// The one reuse decision for an attempt whose probe facts are `facts`
    /// (D11, "Reuse"), whichever path left the holding: the cached
    /// specialization when it reads no fact of the viewed result and its
    /// reads hold, as one bundle for every view; else the image entry when
    /// the probe's facts hold and it holds every cell the probe's layout
    /// needs, however it was packed since, laid out per view.
    fn reuse_for_attempt(&self, facts: &ImportFacts) -> Option<ForAttempt> {
        if let Some(l) = &self.layout
            && !reads_result(&l.facts)
            && l.facts.holds(facts)
            && self.scripts.result_live(l.script)
        {
            return Some(ForAttempt::Every(
                Arc::clone(&l.bundle),
                l.entry.clone(),
                l.script,
            ));
        }
        if let (Some((deps, needs)), Some(e)) = (&self.probe, &self.image)
            && deps.holds(facts)
            && needs.held_by(e)
        {
            return Some(ForAttempt::Pending);
        }
        None
    }

    /// The one reuse decision for a view: the cached specialization when
    /// its reads hold the view's facts, or, for an unavailable view, when
    /// it reads no fact of the viewed result and its reads hold the
    /// attempt's.
    fn reuse_for_view(&self, view: ViewFacts) -> Option<Published> {
        // The cached one, else the one kept, while it is protected.
        let l = self
            .layout
            .iter()
            .chain(self.presented.iter().filter(|l| self.protected(l.script)))
            .filter(|l| self.scripts.result_live(l.script))
            // A layout whose images are of other sources: laid out again.
            .filter(|l| self.chart_ready(l))
            .find(|l| match view.0 {
                Some(fields) => l.facts.holds(&result_facts(fields, self.class)),
                None => {
                    !reads_result(&l.facts)
                        && self.attempt_facts().is_some_and(|f| l.facts.holds(&f))
                }
            })?;
        Some(Published::Skin(
            Arc::clone(&l.bundle),
            l.entry.clone(),
            l.script,
        ))
    }

    /// Drops the specialization kept for reuse once its generation is no
    /// longer protected ([`Self::protected`]): its atlas counts no more.
    fn forget_unpresented(&mut self) {
        if self
            .presented
            .as_ref()
            .is_some_and(|p| !self.protected(p.script))
        {
            self.presented = None;
        }
    }

    /// Caches `spec` as the layout; the one it replaces is kept for reuse
    /// while its generation is protected.
    fn cache(&mut self, spec: Specialized) {
        if let Some(old) = self.layout.replace(spec)
            && self.protected(old.script)
        {
            self.presented = Some(old);
        } else {
            self.forget_unpresented();
        }
    }

    /// The image side a job may release, in its slot: the image entry, the
    /// kept cells, and the cached specialization unless it is the one
    /// shown.
    fn slot_for_job(&mut self) -> Slot {
        let shown = matches!(
            (&self.published, &self.layout),
            (Some(Published::Skin(p, _, _)), Some(l)) if Arc::ptr_eq(p, &l.bundle)
        );
        Arc::new(Mutex::new(Some(Images {
            entry: self.image.take(),
            cells: self.cells.take(),
            layout: if shown { None } else { self.layout.take() },
        })))
    }

    /// The image side after a job: what it left of the holdings it took,
    /// else the holdings still in its slot, untouched. A specialization
    /// returns unless another replaced it.
    fn restore(&mut self, taken: Option<Images>, slot: &Slot) {
        let Some(images) = taken.or_else(|| lock(slot).take()) else {
            return;
        };
        self.image = images.entry;
        self.cells = images.cells;
        if self.layout.is_none() {
            self.layout = images.layout;
        }
    }

    /// Takes a finished job's outcome, publishes it when its request is
    /// still the latest, and starts the waiting request. Returns whether
    /// the latest request is published.
    pub fn poll(&mut self) -> bool {
        self.forget_unpresented();
        if let Some((job, _, _)) = &mut self.job
            && let Some(ran) = job.poll()
            && let Some((_, request, slot)) = self.job.take()
        {
            match ran {
                Ran::Done(out) => self.landed(out, &slot),
                // A failed thread's request: the built-in result while it
                // is still the latest; the holdings it had not taken stay.
                Ran::Refused(why) => {
                    log::info!("skin: result: its layout failed: {why}");
                    self.restore(None, &slot);
                    if self.latest.map(|(r, _)| r) == Some(request) {
                        self.publish(Published::BuiltIn);
                    }
                }
                Ran::Abandoned => {}
            }
            if let Some((request, view)) = self.waiting.take() {
                self.layout_for(request, view);
            }
        }
        // A deferred build is tried again once fewer than two atlases are
        // live.
        if self.deferred.is_some() && self.job.is_none() && self.others().len() < 2 {
            self.retry_deferred();
        }
        // A permit wait tries the eviction and the take again at each poll,
        // whatever was released, for the latest request only.
        if self.job.is_none()
            && let Some((request, view)) = self.permit_wait.take()
            && self.latest.map(|(r, _)| r) == Some(request)
        {
            self.layout_for(request, view);
        }
        self.job.is_none()
            && self.waiting.is_none()
            && self.deferred.is_none()
            && self.permit_wait.is_none()
    }

    fn landed(&mut self, out: LayoutOut, slot: &Slot) {
        // The entry is keyed by cells, not by request: it stays.
        let Laid { result, taken, .. } = out.laid;
        self.restore(taken, slot);
        let latest = self.latest.map(|(r, _)| r);
        // Its runtime is the cell's from now when the latest request or the
        // cache keeps its layout (retired by a later eviction once neither
        // does); otherwise it is retired now.
        let kept = match &result {
            Ok(_) if latest == Some(out.request) => true,
            Ok(_) => self.latest.is_some_and(|(_, v)| v == out.view),
            Err(_) => false,
        };
        let script = if kept {
            self.scripts.offer_result(out.script, false)
        } else {
            self.scripts.discard(out.script);
            None
        };
        let result = result.map(|spec| Specialized { script, ..spec });
        match result {
            Ok(spec) if latest == Some(out.request) => {
                // The facts the published layout read (dev-tools).
                #[cfg(feature = "dev-tools")]
                println!(
                    "dev: result layout read:{}",
                    spec.facts
                        .reads
                        .iter()
                        .map(|r| format!(" {:?}={:?}", r.fact, r.value))
                        .collect::<String>()
                );
                let p = Published::Skin(Arc::clone(&spec.bundle), spec.entry.clone(), spec.script);
                self.cache(spec);
                self.publish(p);
            }
            Ok(spec) => {
                // Disposed of, unless its facts are the latest view's.
                if let Some((_, view)) = self.latest
                    && view == out.view
                {
                    self.cache(spec);
                }
            }
            Err(Unbuilt::Deferred) if latest == Some(out.request) => {
                self.deferred = Some((out.request, out.view));
            }
            Err(Unbuilt::Refused(why)) if latest == Some(out.request) => {
                log::info!("skin: result: built-in for this view: {why}");
                self.publish(Published::BuiltIn);
            }
            Err(_) => {}
        }
    }

    /// Whether a build waits for an atlas to be released.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn deferred(&self) -> bool {
        self.deferred.is_some()
    }

    /// Whether a job runs or waits.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn busy(&self) -> bool {
        self.job.is_some()
            || self.waiting.is_some()
            || self.deferred.is_some()
            || self.permit_wait.is_some()
    }

    /// Whether a request waits for a permit (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn permit_waiting(&self) -> bool {
        self.permit_wait.is_some()
    }

    /// The result runtimes' cell (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn scripts(&self) -> &Scripts {
        &self.scripts
    }

    /// Cancels the job, if any, and waits for its thread: with no bound
    /// unless `end_run` says the run ends, then at most
    /// [`imports::STOP_WAIT`] (D11, "Cancellation"). The waiting request
    /// is dropped. The image entry the job hands back stays.
    pub fn settle(&mut self, end_run: impl FnMut() -> bool) {
        #[cfg(feature = "dev-tools")]
        println!("dev: result skin settle");
        #[cfg(all(test, feature = "dev-tools"))]
        crate::dev::hold_result_settle();
        self.waiting = None;
        self.deferred = None;
        self.permit_wait = None;
        if let Some((job, _, slot)) = self.job.take() {
            match job.finish(|| true, end_run) {
                Ran::Done(out) => {
                    self.restore(out.laid.taken, &slot);
                    // No request wants it: the cell retires it.
                    self.scripts.discard(out.script);
                }
                Ran::Refused(_) => self.restore(None, &slot),
                // Its thread is detached and may still take the slot.
                Ran::Abandoned => {}
            }
        }
    }

    /// The requests made so far (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub const fn requests(&self) -> u64 {
        self.next_request
    }

    /// Whether a layout job runs.
    pub fn has_job(&self) -> bool {
        self.job.is_some()
    }

    /// Has each layout job wake `thread` once its outcome is in.
    pub fn set_wake(&mut self, thread: Option<std::thread::Thread>) {
        self.wake = thread;
    }

    /// Whether the layout job's outcome was received and not yet taken: its
    /// thread may still be ending, and [`poll`](Self::poll) takes it once
    /// the thread has ended.
    pub fn landing(&mut self) -> bool {
        self.job.as_mut().is_some_and(|(job, _, _)| job.received())
    }

    /// The result skin of the next attempt is decided again by its image
    /// stage; what the result slot holds stays.
    pub fn next_attempt(&mut self) {
        self.attempt = None;
        self.latest = None;
        self.scripts.set_published(None);
    }

    /// Whether runtime generation `g` is protected now, as the cell
    /// decides it for the eviction and the commit point alike
    /// ([`Scripts::protected`]).
    fn protected(&self, g: Option<ScriptGen>) -> bool {
        g.is_some_and(|g| self.scripts.protected(g))
    }
}

impl Drop for ResultSkin {
    /// The run's end: a running job is cancelled and waited for at most
    /// [`imports::STOP_WAIT`], and so is the preload's pack.
    fn drop(&mut self) {
        if self.has_job() {
            self.settle(|| true);
        }
        self.end_preload(imports::STOP_WAIT);
    }
}

/// The chart's facts a result view carries for `loaded`, each its own
/// projection of the loaded chart: its play level and difficulty; the
/// gauge's TOTAL, whole (number 368); the tempo's lowest and highest in
/// floored hundredths with whether it changes in whole BPMs, as the play
/// readouts take them (numbers 90-91); its length at 1×, to its last
/// note's end or BGM event, whatever section or speed is played (numbers
/// 1163-1164); its judged notes (number 74), which a view replaces with
/// its own; its judgment class (options 180-184), [`attempt_class`],
/// the same in every view; and the play's target (numbers 151, 157-158,
/// with 108, 128, 153 and options 352-354 against it), the attempt's own
/// `score.target`, which its play skin takes too, the same in every view.
/// A practice round's is 0.
pub fn attempt_chart(loaded: &game::Loaded) -> PendingChart {
    let attempt = loaded.attempt();
    let labels = &attempt.labels;
    let difficulty = crate::imports::facts::difficulty_of(&labels.difficulty);
    let tempo = attempt.tempo.meta();
    PendingChart(ResultChart {
        level: Some(crate::imports::facts::level_of(&labels.playlevel)),
        total: u32::try_from(attempt.song_total).ok(),
        bpm: crate::attempt_context::bpm_range(tempo).map(|r| skin_model::ResultBpm {
            min_x100: r.min_x100,
            max_x100: r.max_x100,
            changes: crate::attempt_context::bpm_whole_change(tempo),
        }),
        difficulty: Some(difficulty).filter(|d| (1..=5).contains(d)),
        length: Some(loaded.chart().meta.length),
        notes: Some(game::ResultBasis::of(loaded.chart(), None).notes),
        judge_class: attempt_class(loaded).map(JudgeClass::index),
        images: ImageFacts::default(),
        target: attempt.score.target,
    })
}

/// The judgment class of the chart at `loaded` (options 180-184), from the
/// chart's own rank, by the rule every screen uses.
pub fn attempt_class(loaded: &game::Loaded) -> Option<JudgeClass> {
    JudgeClass::of_headers(&loaded.chart().meta.judgment_headers)
}

/// The facts of `screen`'s view with `chart`: its fields while it is
/// ready, else unavailable (D7).
pub fn view_facts(screen: &game::ResultScreen, chart: ResultChart) -> ViewFacts {
    let view = crate::result_present::view_of(screen, chart);
    ViewFacts((view.status == ResultStatus::Ready).then(|| ResultFields::of(&view)))
}

#[cfg(all(test, feature = "dev-tools"))]
pub(crate) mod chart_tests;
#[cfg(all(test, feature = "dev-tools"))]
mod step_tests;
#[cfg(all(test, feature = "dev-tools"))]
pub(crate) mod tests;
