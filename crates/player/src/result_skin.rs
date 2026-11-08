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
    Cancel, Config, Fact, FactDeps, ImportFacts, JudgeClass, LuaHeader, LuaOutcome,
};
use skin_model::{
    Atlas, AtlasId, BestFigures, BestRate, BestView, FileSource, LampKey, Layout, ResourceCache,
    ResultChart, ResultFields, ResultNumber, ResultRate, ResultScore, ResultStatus, ResultView,
    Root, ScreenKind, Sheet, TimingStats, Variant,
};

use crate::imports::{self, ImportTarget, Job, Ran};

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
}

/// A prepared atlas and the sheet it was packed from: the cells a union
/// starts from (D13, "The atlas step").
#[derive(Debug)]
pub struct ImageEntry {
    atlas: Atlas,
    packed: Sheet,
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
    Skin(Arc<SkinBundle>, Option<Arc<ImageEntry>>),
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
}

/// The attempt's result skin, as its image stage left it.
#[derive(Clone, Debug)]
enum ForAttempt {
    /// A bundle for every view, with the prepared atlas it is bound to: a
    /// fixed skin, or one that read no fact of the viewed result.
    Every(Arc<SkinBundle>, Option<Arc<ImageEntry>>),
    /// The built-in result.
    BuiltIn,
    /// Laid out per view.
    Pending,
}

/// The kind's skin, as startup left it.
#[derive(Debug)]
pub enum ResultStart {
    /// Imported with no fact pending: one bundle for every chart.
    Fixed(Arc<SkinBundle>),
    /// Built per attempt and view.
    Pending(Arc<PendingResult>),
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
}

impl std::fmt::Debug for ResultSkin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResultSkin")
            .field("generation", &self.generation)
            .field("published", &self.published)
            .finish_non_exhaustive()
    }
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
fn probe_view(chart: ResultChart) -> ResultView {
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

/// The one release decision, once a replacement plan fits (D13): an
/// image entry that is not `base` (the shown one) is dropped, its cells
/// kept; the unshown cached specialization goes too. Returns what is left
/// and whether the atlases live besides it (`others`, with the entry when
/// kept) leave room for one more (the S10-09i design's Q3).
fn release(images: Images, base: Option<&Arc<ImageEntry>>, others: Vec<AtlasId>) -> (Images, bool) {
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
        (Some(c), Some(d)) => c.union(&d.packed).or(Some(c)),
    };
    let mut live = others;
    live.extend(entry.as_ref().map(|e| e.pages().id()));
    live.sort_unstable();
    live.dedup();
    let room = live.len() < 2;
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
);

/// How [`ResultSkin::begin_image_stage`] began.
pub enum ImageStage {
    /// Decided at once.
    Ready(Staged),
    /// Its job runs.
    Running(Box<ImageJob>),
}

/// An image stage's job in flight.
pub struct ImageJob {
    job: imports::Job<ImageOut>,
    slot: Slot,
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
    text: bool,
}

impl Needs {
    /// Whether `entry`'s atlas holds every cell, packed as needed.
    fn held_by(&self, entry: &ImageEntry) -> bool {
        entry
            .atlas
            .rebind(&self.sheet, self.variant, self.text)
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
#[expect(clippy::needless_pass_by_value, reason = "the job owns its inputs")]
fn layout(
    pending: &PendingResult,
    view: ViewFacts,
    class: Option<Option<JudgeClass>>,
    slot: &Slot,
    base: Option<Arc<ImageEntry>>,
    others: Vec<AtlasId>,
    cancel: &Cancel,
) -> Laid {
    #[cfg(all(test, feature = "dev-tools"))]
    tests::layout_starts();
    let Some(fields) = view.0 else {
        return Laid::refused("the view is unavailable".to_owned());
    };
    let facts = result_facts(fields, class);
    let imported = match pending
        .header
        .import_reading(&pending.config, &facts, cancel)
    {
        Ok(LuaOutcome::Imported(i)) => i,
        Ok(LuaOutcome::Pending(p)) => {
            return Laid::refused(format!("the skin reads {:?}, pending here", p.pending));
        }
        Err(refused) => return Laid::refused(imports::chain(&refused.0)),
    };
    let deps = imported.facts;
    let skin = imported.imported.skin;
    let scratch = imported.imported.scratch;
    let variant = skin.variant(pending.target.size, &Layout::AUTHOR);
    let text = skin.has_outline_text();
    let entry = lock(slot).as_ref().and_then(|i| i.entry.clone());
    let held = entry
        .as_ref()
        .and_then(|e| e.atlas.rebind(skin.sheet(), variant, text).ok());
    let (atlas, entry, taken) = match held {
        Some(atlas) => (atlas, entry, None),
        None => {
            drop(entry);
            let limits = pending.target.limits;
            let mut guard = lock(slot);
            let packed = {
                let (e, c) = guard
                    .as_ref()
                    .map_or((None, None), |i| (i.entry.as_ref(), i.cells.as_ref()));
                // The union's base: the shown atlas's cells, the image
                // entry's and the kept ones, which the build keeps as cells
                // only.
                let earlier = base.is_some() || e.is_some() || c.is_some();
                let mut union = Some(skin.sheet().clone());
                for b in [base.as_ref(), e].into_iter().flatten() {
                    union = union.and_then(|u| u.union(&b.packed));
                }
                if let Some(c) = c {
                    union = union.and_then(|u| u.union(c));
                }
                match union
                    .filter(|_| earlier)
                    .filter(|u| u.plan(variant, limits, text).is_ok())
                {
                    Some(u) => u,
                    None if skin.sheet().plan(variant, limits, text).is_ok() => {
                        skin.sheet().clone()
                    }
                    None => {
                        return Laid::refused(
                            "the layout's images do not fit one atlas".to_owned(),
                        );
                    }
                }
            };
            // The plan fits: the holdings are taken, and what is not shown
            // goes before the build.
            let images = guard.take().unwrap_or_default();
            drop(guard);
            let (images, room) = release(images, base.as_ref(), others);
            if !room {
                return Laid {
                    result: Err(Unbuilt::Deferred),
                    taken: Some(images),
                    needs: None,
                };
            }
            let kept = images.cells;
            drop(images.entry);
            #[cfg(all(test, feature = "dev-tools"))]
            tests::build_gate(cancel);
            #[cfg(any(test, feature = "dev-tools"))]
            BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let built =
                match packed.atlas_reserving(variant, limits, text, &|| cancel.is_cancelled()) {
                    Ok(a) => a,
                    Err(e) => {
                        return Laid {
                            result: Err(format!("the atlas cannot be built: {e}").into()),
                            taken: Some(Images {
                                cells: kept,
                                ..Images::default()
                            }),
                            needs: None,
                        };
                    }
                };
            let new = Arc::new(ImageEntry {
                atlas: built,
                packed,
            });
            let taken = Some(Images {
                entry: Some(Arc::clone(&new)),
                ..Images::default()
            });
            match new.atlas.rebind(skin.sheet(), variant, text) {
                Ok(a) => (a, Some(new), taken),
                Err(e) => {
                    return Laid {
                        result: Err(format!("the layout cannot be bound: {e}").into()),
                        taken,
                        needs: None,
                    };
                }
            }
        }
    };
    let needs = Needs {
        sheet: skin.sheet().clone(),
        variant,
        text,
    };
    let result = imports::bundle_of(skin, scratch, atlas, &pending.target, cancel)
        .map(|bundle| Specialized {
            bundle,
            facts: deps,
            entry,
        })
        .map_err(Unbuilt::from);
    Laid {
        result,
        taken,
        needs: Some(needs),
    }
}

impl ResultSkin {
    /// The kind's skin from startup.
    pub fn new(start: Option<ResultStart>) -> Self {
        let mut s = Self::default();
        s.start = start;
        s
    }

    /// The pending skin, if the kind has one.
    fn pending(&self) -> Option<&Arc<PendingResult>> {
        match &self.start {
            Some(ResultStart::Pending(p)) => Some(p),
            _ => None,
        }
    }

    /// The atlases live besides the image entry: the renderer's retained
    /// set, the latest publication's binding and the layout entry's.
    fn others(&self) -> Vec<AtlasId> {
        let mut v = self.retained.clone();
        if let Some(Published::Skin(b, _)) = &self.published {
            v.push(b.atlas_pages().id());
        }
        if let Some(l) = &self.layout {
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
    #[cfg_attr(
        not(all(test, feature = "dev-tools")),
        expect(dead_code, reason = "S10-09i reports what its renderer retains")
    )]
    pub fn set_retained(&mut self, ids: &[AtlasId]) {
        self.retained = ids.to_vec();
        if self.others().len() < 2 {
            self.retry_deferred();
        }
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

    /// The published generation's prepared atlas.
    fn shown_entry(&self) -> Option<Arc<ImageEntry>> {
        match &self.published {
            Some(Published::Skin(_, e)) => e.clone(),
            _ => None,
        }
    }

    /// What the result slot holds, and its generation.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn published(&self) -> (Option<&Published>, u64) {
        (self.published.as_ref(), self.generation)
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
        self.published = Some(p);
        self.generation += 1;
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
        match self.begin_image_stage(chart, class) {
            ImageStage::Ready(staged) => staged,
            ImageStage::Running(mut job) => loop {
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
    /// ([`ImageJob::wait_until`]) and finished
    /// ([`finish_image_stage`](Self::finish_image_stage)).
    pub fn begin_image_stage(
        &mut self,
        chart: ResultChart,
        class: Option<JudgeClass>,
    ) -> ImageStage {
        self.chart = Some(chart);
        self.class = Some(class);
        let pending = match &self.start {
            None => return ImageStage::Ready(Staged::Ready),
            Some(ResultStart::Fixed(b)) => {
                self.attempt = Some(ForAttempt::Every(Arc::clone(b), None));
                return ImageStage::Ready(Staged::Ready);
            }
            Some(ResultStart::Pending(p)) => Arc::clone(p),
        };
        let facts = result_facts(probe_fields(chart), self.class);
        if let Some(reused) = self.reuse_for_attempt(&facts) {
            self.attempt = Some(reused);
            return ImageStage::Ready(Staged::Ready);
        }
        let slot = self.slot_for_job();
        let theirs = Arc::clone(&slot);
        let base = self.shown_entry();
        let others = self.others();
        let spawned = imports::spawn_job(move |cancel: &Cancel| -> ImageOut {
            let slot = theirs;
            let o = pending
                .header
                .import_reading(&pending.config, &facts, cancel);
            let deps = match &o {
                Ok(LuaOutcome::Imported(i)) => i.facts.clone(),
                Ok(LuaOutcome::Pending(p)) => p.facts.clone(),
                Err(refused) => refused.1.clone(),
            };
            let read = reads_result(&deps);
            match o {
                Ok(LuaOutcome::Imported(i)) if !read => {
                    // A skin that reads no fact of the viewed result:
                    // one bundle for every view, packed as any kind's
                    // once its plan fits and what is not shown is
                    // released; with no room, laid out per view.
                    let skin = i.imported.skin;
                    let variant = skin.variant(pending.target.size, &Layout::AUTHOR);
                    let limits = pending.target.limits;
                    if let Err(e) = skin.sheet().plan(variant, limits, skin.has_outline_text()) {
                        return (
                            Some(Err(format!("the atlas cannot be built: {e}"))),
                            deps,
                            None,
                            None,
                        );
                    }
                    let images = lock(&slot).take().unwrap_or_default();
                    let (images, room) = release(images, base.as_ref(), others);
                    if !room {
                        return (None, deps, Some(images), None);
                    }
                    #[cfg(any(test, feature = "dev-tools"))]
                    BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let r = skin
                        .atlas(variant, pending.target.limits, &|| cancel.is_cancelled())
                        .map_err(|e| format!("the atlas cannot be built: {e}"))
                        .and_then(|a| {
                            imports::bundle_of(skin, i.imported.scratch, a, &pending.target, cancel)
                        });
                    (Some(r.map(Some)), deps, Some(images), None)
                }
                Ok(LuaOutcome::Imported(_)) => {
                    let view = ViewFacts(Some(match facts.result {
                        Fact::Known(f) => f,
                        _ => return (None, deps, None, None),
                    }));
                    let laid = layout(&pending, view, Some(class), &slot, base, others, cancel);
                    // The probe's layout is never shown: its atlas is.
                    (
                        Some(laid.result.map(|_| None).map_err(|e| match e {
                            Unbuilt::Refused(why) => why,
                            Unbuilt::Deferred => "deferred".to_owned(),
                        })),
                        deps,
                        laid.taken,
                        laid.needs,
                    )
                }
                Err(_) if !read => (Some(Err("refused".to_owned())), deps, None, None),
                _ => (None, deps, None, None),
            }
        });
        match spawned {
            Ok(job) => ImageStage::Running(Box::new(ImageJob { job, slot })),
            Err(why) => ImageStage::Ready(self.finish_image_stage(&slot, Ran::Refused(why))),
        }
    }

    /// The image stage's `job` waited for until `until` at most
    /// ([`imports::Job::wait_until`] with the attempt's two signals), and
    /// finished once it ended; `None` at `until` with the job running.
    pub fn poll_image_stage(
        &mut self,
        job: &mut ImageJob,
        until: Option<std::time::Instant>,
        quit: impl FnMut() -> bool,
        end_run: impl FnMut() -> bool,
    ) -> Option<Staged> {
        let ran = job.job.wait_until(until, quit, end_run)?;
        Some(self.finish_image_stage(&job.slot, ran))
    }

    /// The attempt's result skin from its image stage's outcome `ran`, the
    /// job's images handed back through `slot`.
    fn finish_image_stage(&mut self, slot: &Slot, ran: Ran<ImageOut>) -> Staged {
        match ran {
            Ran::Done((outcome, deps, taken, needs)) => {
                self.restore(taken, slot);
                match outcome {
                    // A bundle for every view, cached with what it read.
                    Some(Ok(Some(bundle))) => {
                        self.attempt = Some(ForAttempt::Every(Arc::clone(&bundle), None));
                        self.layout = Some(Specialized {
                            bundle,
                            facts: deps,
                            entry: None,
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
        self.next_request += 1;
        let request = self.next_request;
        self.latest = Some((request, view));
        // A newer request supersedes the deferred one.
        self.deferred = None;
        match self.attempt.clone() {
            None => {}
            Some(ForAttempt::Every(b, entry)) => {
                if !matches!(&self.published, Some(Published::Skin(p, _)) if Arc::ptr_eq(p, &b)) {
                    self.publish(Published::Skin(b, entry));
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
        // The entry goes to the job, which hands it back; the layout entry
        // bound to an unshown one goes with it, dropped only with its pages.
        let slot = self.slot_for_job();
        let theirs = Arc::clone(&slot);
        let base = self.shown_entry();
        let others = self.others();
        let class = self.class;
        match imports::spawn_job(move |cancel: &Cancel| LayoutOut {
            request,
            view,
            laid: layout(&pending, view, class, &theirs, base, others, cancel),
        }) {
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
        {
            return Some(ForAttempt::Every(Arc::clone(&l.bundle), l.entry.clone()));
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
        let l = self.layout.as_ref()?;
        let holds = match view.0 {
            Some(fields) => l.facts.holds(&result_facts(fields, self.class)),
            None => {
                !reads_result(&l.facts) && self.attempt_facts().is_some_and(|f| l.facts.holds(&f))
            }
        };
        holds.then(|| Published::Skin(Arc::clone(&l.bundle), l.entry.clone()))
    }

    /// The image side a job may release, in its slot: the image entry, the
    /// kept cells, and the cached specialization unless it is the one
    /// shown.
    fn slot_for_job(&mut self) -> Slot {
        let shown = matches!(
            (&self.published, &self.layout),
            (Some(Published::Skin(p, _)), Some(l)) if Arc::ptr_eq(p, &l.bundle)
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
        self.job.is_none() && self.waiting.is_none() && self.deferred.is_none()
    }

    fn landed(&mut self, out: LayoutOut, slot: &Slot) {
        // The entry is keyed by cells, not by request: it stays.
        let Laid { result, taken, .. } = out.laid;
        self.restore(taken, slot);
        let latest = self.latest.map(|(r, _)| r);
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
                let p = Published::Skin(Arc::clone(&spec.bundle), spec.entry.clone());
                self.layout = Some(spec);
                self.publish(p);
            }
            Ok(spec) => {
                // Disposed of, unless its facts are the latest view's.
                if let Some((_, view)) = self.latest
                    && view == out.view
                {
                    self.layout = Some(spec);
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
        self.job.is_some() || self.waiting.is_some() || self.deferred.is_some()
    }

    /// Cancels the job, if any, and waits for its thread: with no bound
    /// unless `end_run` says the run ends, then at most
    /// [`imports::STOP_WAIT`] (D11, "Cancellation"). The waiting request
    /// is dropped. The image entry the job hands back stays.
    pub fn settle(&mut self, end_run: impl FnMut() -> bool) {
        #[cfg(feature = "dev-tools")]
        println!("dev: result skin settle");
        self.waiting = None;
        self.deferred = None;
        if let Some((job, _, slot)) = self.job.take() {
            match job.finish(|| true, end_run) {
                Ran::Done(out) => self.restore(out.laid.taken, &slot),
                Ran::Refused(_) => self.restore(None, &slot),
                // Its thread is detached and may still take the slot.
                Ran::Abandoned => {}
            }
        }
    }

    /// Whether a layout job runs.
    pub fn has_job(&self) -> bool {
        self.job.is_some()
    }

    /// The result skin of the next attempt is decided again by its image
    /// stage; what the result slot holds stays.
    pub fn next_attempt(&mut self) {
        self.attempt = None;
        self.latest = None;
    }
}

impl Drop for ResultSkin {
    /// The run's end: a running job is cancelled and waited for at most
    /// [`imports::STOP_WAIT`].
    fn drop(&mut self) {
        if self.has_job() {
            self.settle(|| true);
        }
    }
}

/// The chart's facts a result view carries for `loaded`, each its own
/// projection of the loaded chart: its play level and difficulty; the
/// gauge's TOTAL, whole (number 368); the tempo's lowest and highest in
/// floored hundredths with whether it changes in whole BPMs, as the play
/// readouts take them (numbers 90-91); its length at 1×, to its last
/// note's end or BGM event, whatever section or speed is played (numbers
/// 1163-1164); and its judged notes (number 74), which a
/// view replaces with its own.
pub fn attempt_chart(loaded: &game::Loaded) -> ResultChart {
    let attempt = loaded.attempt();
    let labels = &attempt.labels;
    let difficulty = crate::imports::facts::difficulty_of(&labels.difficulty);
    let tempo = attempt.tempo.meta();
    ResultChart {
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
    }
}

/// The judgment class of the chart at `loaded` (options 180-184), as the
/// reference computes it from the chart's own rank.
pub fn attempt_class(loaded: &game::Loaded) -> Option<JudgeClass> {
    JudgeClass::of_headers(&loaded.chart().meta.judgment_headers)
}

/// The facts of `screen`'s view with `chart`: its fields while it is
/// ready, else unavailable (D7).
pub fn view_facts(screen: &game::ResultScreen, chart: ResultChart) -> ViewFacts {
    let (game::ViewStatus::Ready(summary), Some(preset)) = (
        screen.status(),
        screen.viewed().and_then(|e| e.preset.as_deref()),
    ) else {
        return ViewFacts(None);
    };
    let parts = crate::result_view::result_parts(summary, preset, &screen.basis());
    let view = ResultView {
        status: ResultStatus::Ready,
        chart: ResultChart {
            notes: Some(parts.notes),
            ..chart
        },
        score: parts.score,
        best: parts.best,
        timing: parts.timing,
        timelines: parts.timelines,
        gauge: parts.gauge,
        deviations: Some(parts.deviations),
        timing_areas: Some(parts.timing_areas),
    };
    ViewFacts(Some(ResultFields::of(&view)))
}

#[cfg(all(test, feature = "dev-tools"))]
mod tests;
