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

use std::sync::Arc;

use eval::Tally;
use render::SkinBundle;
use rules::PerJudgment;
use skin_beatoraja::{Cancel, Config, Fact, FactDeps, FactId, ImportFacts, LuaHeader, LuaOutcome};
use skin_model::{
    Atlas, AtlasId, BestFigures, BestRate, BestView, FileSource, LampKey, Layout, ResourceCache,
    ResultChart, ResultFields, ResultNumber, ResultRate, ResultScore, ResultStatus, ResultView,
    Root, ScreenKind, Sheet, TimingStats,
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
    result: Result<Specialized, Unbuilt>,
    entry: Option<Arc<ImageEntry>>,
}

/// The attempt's result skin, as its image stage left it.
#[derive(Clone, Debug)]
enum ForAttempt {
    /// A bundle for every view: a fixed skin, or one that read no result
    /// fact.
    Every(Arc<SkinBundle>),
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
    image: Option<Arc<ImageEntry>>,
    /// The probe's facts, for reusing the image stage.
    probe: Option<FactDeps>,
    layout: Option<Specialized>,
    published: Option<Published>,
    generation: u64,
    job: Option<(Job<LayoutOut>, u64)>,
    waiting: Option<(u64, ViewFacts)>,
    /// A request whose build waits for an atlas to be released (Q3).
    deferred: Option<(u64, ViewFacts)>,
    /// The atlases the renderer still references, as it last reported
    /// them ([`Self::set_retained`]).
    retained: Vec<AtlasId>,
    /// The latest request, and the view it asked for.
    latest: Option<(u64, ViewFacts)>,
    next_request: u64,
}

impl std::fmt::Debug for ResultSkin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResultSkin")
            .field("generation", &self.generation)
            .field("published", &self.published)
            .finish_non_exhaustive()
    }
}

/// Whether `deps` read a result fact.
fn reads_result(deps: &FactDeps) -> bool {
    deps.reads
        .iter()
        .any(|r| matches!(r.fact, FactId::ResultNumber(_) | FactId::ResultOption(_)))
}

/// The snapshot of a result view's fields.
fn result_facts(fields: ResultFields) -> ImportFacts {
    ImportFacts {
        result: Fact::Known(fields),
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

/// A layout of `pending` for `view`: the import over the cache, then the
/// atlas step against `entry` (D13). When the atlas does not hold the
/// layout's cells, a union with `base`'s (the shown generation's atlas),
/// else the layout's cells alone, is planned first; only once a plan fits
/// is `entry` dropped when it is not `base`, then, unless the atlases live
/// besides it (`others`, with `entry` when kept) and the new one would
/// pass two, the plan packed (the S10-09i design's Q3).
#[expect(clippy::needless_pass_by_value, reason = "the job owns its inputs")]
fn layout(
    pending: &PendingResult,
    view: ViewFacts,
    entry: Option<Arc<ImageEntry>>,
    base: Option<Arc<ImageEntry>>,
    others: Vec<AtlasId>,
    cancel: &Cancel,
) -> (Result<Specialized, Unbuilt>, Option<Arc<ImageEntry>>) {
    let Some(fields) = view.0 else {
        return (Err("the view is unavailable".to_owned().into()), entry);
    };
    let facts = result_facts(fields);
    let imported = match pending
        .header
        .import_reading(&pending.config, &facts, cancel)
    {
        Ok(LuaOutcome::Imported(i)) => i,
        Ok(LuaOutcome::Pending(p)) => {
            return (
                Err(format!("the skin reads {:?}, pending here", p.pending).into()),
                entry,
            );
        }
        Err(refused) => return (Err(imports::chain(&refused.0).into()), entry),
    };
    let deps = imported.facts;
    let skin = imported.imported.skin;
    let scratch = imported.imported.scratch;
    let variant = skin.variant(pending.target.size, &Layout::AUTHOR);
    let text = skin.has_outline_text();
    let held = entry.as_ref().and_then(|e| {
        e.atlas
            .rebind(skin.sheet(), variant, text)
            .ok()
            .map(|a| (a, Arc::clone(e)))
    });
    let (atlas, entry) = match held {
        Some((atlas, e)) => (atlas, Some(e)),
        None => {
            let limits = pending.target.limits;
            // The union's base: the shown atlas's cells and the image
            // entry's, which the build keeps as cells only.
            let mut union = Some(skin.sheet().clone());
            for b in [&base, &entry].into_iter().flatten() {
                union = union.and_then(|u| u.union(&b.packed));
            }
            let union = union
                .filter(|_| base.is_some() || entry.is_some())
                .filter(|u| u.plan(variant, limits, text).is_ok());
            let packed = match union {
                Some(u) => u,
                None if skin.sheet().plan(variant, limits, text).is_ok() => skin.sheet().clone(),
                None => {
                    return (
                        Err("the layout's images do not fit one atlas".to_owned().into()),
                        entry,
                    );
                }
            };
            // The plan fits: an image entry that is not the shown one is
            // dropped before the build.
            let entry = entry.filter(|e| base.as_ref().is_some_and(|b| Arc::ptr_eq(e, b)));
            let mut live = others;
            live.extend(entry.as_ref().map(|e| e.pages().id()));
            live.sort_unstable();
            live.dedup();
            if live.len() + 1 > 2 {
                return (Err(Unbuilt::Deferred), entry);
            }
            drop(entry);
            #[cfg(any(test, feature = "dev-tools"))]
            BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let built =
                match packed.atlas_reserving(variant, limits, text, &|| cancel.is_cancelled()) {
                    Ok(a) => a,
                    Err(e) => return (Err(format!("the atlas cannot be built: {e}").into()), None),
                };
            let new = Arc::new(ImageEntry {
                atlas: built,
                packed,
            });
            match new.atlas.rebind(skin.sheet(), variant, text) {
                Ok(a) => (a, Some(new)),
                Err(e) => {
                    return (
                        Err(format!("the layout cannot be bound: {e}").into()),
                        Some(new),
                    );
                }
            }
        }
    };
    match imports::bundle_of(skin, scratch, atlas, &pending.target, cancel) {
        Ok(bundle) => (
            Ok(Specialized {
                bundle,
                facts: deps,
                entry: entry.clone(),
            }),
            entry,
        ),
        Err(why) => (Err(why.into()), entry),
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
    /// again (the S10-09i design's Q3).
    #[cfg_attr(
        not(all(test, feature = "dev-tools")),
        expect(dead_code, reason = "S10-09i reports what its renderer retains")
    )]
    pub fn set_retained(&mut self, ids: &[AtlasId]) {
        self.retained = ids.to_vec();
        self.retry_deferred();
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
        self.published = Some(p);
        self.generation += 1;
    }

    /// The image stage of an attempt whose chart gives `chart` (D11): the
    /// probe import and, unless the live prepared atlas holds its cells,
    /// one atlas build, on a job run with the attempt's two signals. A
    /// fixed skin, or a probe whose facts and atlas still hold, runs no
    /// job.
    pub fn image_stage(
        &mut self,
        chart: ResultChart,
        quit: impl FnMut() -> bool,
        end_run: impl FnMut() -> bool,
    ) -> Staged {
        let pending = match &self.start {
            None => return Staged::Ready,
            Some(ResultStart::Fixed(b)) => {
                self.attempt = Some(ForAttempt::Every(Arc::clone(b)));
                return Staged::Ready;
            }
            Some(ResultStart::Pending(p)) => Arc::clone(p),
        };
        let facts = result_facts(probe_fields(chart));
        if let (Some(deps), Some(_)) = (&self.probe, &self.image)
            && deps.holds(&facts)
        {
            self.attempt = Some(ForAttempt::Pending);
            return Staged::Ready;
        }
        let entry = self.image.take();
        let base = self.shown_entry();
        self.drop_layout_bound_to(entry.as_ref());
        let others = self.others();
        let ran = imports::run_job(
            move |cancel: &Cancel| {
                let o = pending
                    .header
                    .import_reading(&pending.config, &facts, cancel);
                let deps = match &o {
                    Ok(LuaOutcome::Imported(i)) => Some(i.facts.clone()),
                    Ok(LuaOutcome::Pending(p)) => Some(p.facts.clone()),
                    Err(refused) => Some(refused.1.clone()),
                };
                let read = deps.as_ref().is_some_and(reads_result);
                match o {
                    Ok(LuaOutcome::Imported(i)) if !read => {
                        // A skin that reads no result fact: one bundle for
                        // every view, packed as any kind's.
                        let skin = i.imported.skin;
                        let variant = skin.variant(pending.target.size, &Layout::AUTHOR);
                        #[cfg(any(test, feature = "dev-tools"))]
                        BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let r = skin
                            .atlas(variant, pending.target.limits, &|| cancel.is_cancelled())
                            .map_err(|e| format!("the atlas cannot be built: {e}"))
                            .and_then(|a| {
                                imports::bundle_of(
                                    skin,
                                    i.imported.scratch,
                                    a,
                                    &pending.target,
                                    cancel,
                                )
                            });
                        (Some(r.map(Some)), deps, entry)
                    }
                    Ok(LuaOutcome::Imported(_)) => {
                        let view = ViewFacts(Some(match facts.result {
                            Fact::Known(f) => f,
                            _ => return (None, deps, entry),
                        }));
                        let (r, entry) = layout(&pending, view, entry, base, others, cancel);
                        // The probe's layout is never shown: its atlas is.
                        (
                            Some(r.map(|_| None).map_err(|e| match e {
                                Unbuilt::Refused(why) => why,
                                Unbuilt::Deferred => "deferred".to_owned(),
                            })),
                            deps,
                            entry,
                        )
                    }
                    Err(_) if !read => (Some(Err("refused".to_owned())), deps, entry),
                    _ => (None, deps, entry),
                }
            },
            quit,
            end_run,
        );
        match ran {
            Ran::Done((outcome, deps, entry)) => {
                self.image = entry;
                match outcome {
                    // A bundle for every view.
                    Some(Ok(Some(bundle))) => {
                        self.attempt = Some(ForAttempt::Every(bundle));
                        self.probe = None;
                    }
                    // Images prepared: laid out per view.
                    Some(Ok(None)) => {
                        self.attempt = Some(ForAttempt::Pending);
                        self.probe = deps;
                    }
                    // Refused with no result fact read.
                    Some(Err(why)) if deps.as_ref().is_some_and(|d| !reads_result(d)) => {
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
        match self.attempt.clone() {
            None => {}
            Some(ForAttempt::Every(b)) => {
                if !matches!(&self.published, Some(Published::Skin(p, _)) if Arc::ptr_eq(p, &b)) {
                    self.publish(Published::Skin(b, None));
                }
            }
            Some(ForAttempt::BuiltIn) => self.publish(Published::BuiltIn),
            Some(ForAttempt::Pending) => self.layout_for(request, view),
        }
        request
    }

    /// The layout of a pending skin for `view`, request `request`.
    fn layout_for(&mut self, request: u64, view: ViewFacts) {
        let Some(fields) = view.0 else {
            // An unavailable view: the current bundle when it read no
            // result fact, else the built-in result.
            let p = match &self.layout {
                Some(l) if !reads_result(&l.facts) => {
                    Published::Skin(Arc::clone(&l.bundle), l.entry.clone())
                }
                _ => Published::BuiltIn,
            };
            self.publish(p);
            return;
        };
        if let Some(l) = &self.layout
            && l.facts.holds(&result_facts(fields))
        {
            let p = Published::Skin(Arc::clone(&l.bundle), l.entry.clone());
            self.publish(p);
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
        // bound to an unshown one goes with it if the job drops it.
        let entry = self.image.take();
        let base = self.shown_entry();
        self.drop_layout_bound_to(entry.as_ref());
        let others = self.others();
        match imports::spawn_job(move |cancel: &Cancel| {
            let (result, entry) = layout(&pending, view, entry, base, others, cancel);
            LayoutOut {
                request,
                view,
                result,
                entry,
            }
        }) {
            Ok(job) => self.job = Some((job, request)),
            Err(why) => {
                log::info!("skin: result: its layout cannot start: {why}");
                self.publish(Published::BuiltIn);
            }
        }
    }

    /// Drops the layout entry when it is bound to `entry` and `entry` is
    /// not shown: a build may drop that entry's pages (D13).
    fn drop_layout_bound_to(&mut self, entry: Option<&Arc<ImageEntry>>) {
        let shown = self.shown_entry();
        if let (Some(e), Some(l)) = (entry, &self.layout)
            && !shown.as_ref().is_some_and(|s| Arc::ptr_eq(s, e))
            && Arc::ptr_eq(l.bundle.atlas_pages(), e.pages())
        {
            self.layout = None;
        }
    }

    /// Takes a finished job's outcome, publishes it when its request is
    /// still the latest, and starts the waiting request. Returns whether
    /// the latest request is published.
    pub fn poll(&mut self) -> bool {
        if let Some((job, _)) = &mut self.job
            && let Some(ran) = job.poll()
        {
            self.job = None;
            match ran {
                Ran::Done(out) => self.landed(out),
                Ran::Refused(why) => {
                    log::info!("skin: result: its layout failed: {why}");
                    if self.waiting.is_none() {
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

    fn landed(&mut self, out: LayoutOut) {
        // The entry is keyed by cells, not by request: it stays.
        self.image = out.entry;
        // A layout entry bound to dropped pages goes with them.
        if let Some(l) = &self.layout
            && l.entry.as_ref().is_some_and(|e| {
                !self.image.as_ref().is_some_and(|i| Arc::ptr_eq(e, i))
                    && !self.shown_entry().is_some_and(|s| Arc::ptr_eq(e, &s))
            })
        {
            self.layout = None;
        }
        let latest = self.latest.map(|(r, _)| r);
        match out.result {
            Ok(spec) if latest == Some(out.request) => {
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

    /// Whether a layout job runs.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn running(&self) -> bool {
        self.job.is_some()
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
        self.waiting = None;
        self.deferred = None;
        if let Some((job, _)) = self.job.take()
            && let Ran::Done(out) = job.finish(|| true, end_run)
        {
            self.image = out.entry;
        }
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
        self.settle(|| true);
    }
}

/// The chart's facts a result view carries for `loaded`, each its own
/// projection of the loaded chart: its play level and difficulty; the
/// gauge's TOTAL, whole (number 368); the tempo's lowest and highest in
/// floored hundredths with whether it changes in whole BPMs, as the play
/// readouts take them (numbers 90-91); its length, to the end of its last
/// note (numbers 1163-1164); and its judged notes (number 74), which a
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
        length: Some(attempt.last_note_end),
        notes: Some(game::ResultBasis::of(loaded.chart(), None).notes),
    }
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
