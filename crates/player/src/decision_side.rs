//! The decision screen's labels on the render thread (design.md "Play
//! Lifecycle", the decision screen; "Rendering and Skins", the text
//! layer): the chart under the selection cursor asked as one coalesced
//! preparation, the frame boundary's work driving the label slots of the
//! committed decision installation, their pinned content rebuilt on a
//! screen built again, and a skinned decision frame's build.
//!
//! The boundary's work and a rebuild allocate; a decision frame's build
//! allocates nothing ([`decision_build`]).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bms_model::TimeUs;
use render::{
    Gen, LABEL_SLOTS, LabelBook, LabelJob, LabelKey, LabelState, LabelStore, LabelWorker,
    RenderError, RowPlan, SkinBundle, lock,
};
use skin_model::{
    BuildError, DecisionFrame, DecisionScreen, DecisionStrings, DecisionTimers, DecisionTiming,
    Layout, ResultLabels, RowKey, SelectionView,
};

use crate::render_side::Screen;

/// What a decision screen draws, as the render thread is told at the
/// choice ([`crate::render_side::RenderMsg::Decision`]): its lifetime, the
/// choice on the clock, the skin's timing and the label slot it pinned;
/// `None` for the slot draws the built-in presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShownDecision {
    /// The decision lifetime ([`game::DecisionLifetime`]).
    pub lifetime: u64,
    /// The choice, on the clock of the game's deadline.
    pub origin: TimeUs,
    /// The skin's declared timing.
    pub timing: DecisionTiming,
    /// The pinned slot and its generation.
    pub slot: Option<(usize, Gen)>,
}

/// The chart a decision preparation is asked for: the cursor's chart, its
/// labels' revision and their strings.
#[derive(Debug)]
pub struct Want {
    pub chart: PathBuf,
    pub revision: u64,
    pub strings: DecisionStrings,
}

/// The revision of a chart's labels: a digest of the five strings a
/// decision skin shows (title, subtitle, artist, subartist, genre), so
/// labels that change under the same path (a scan) are a new revision and
/// equal labels the same one. FNV-1a over each string and a separator.
/// Equal digests of other strings are told apart by the strings
/// themselves before a slot is reused ([`DecisionSide::boundary`]).
#[must_use]
pub fn labels_revision(l: &skin_model::ChartLabels) -> u64 {
    #[cfg(test)]
    if let Some(forced) = FORCED_REVISION.get() {
        return forced;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for s in [&l.title, &l.subtitle, &l.artist, &l.subartist, &l.genre] {
        for b in s.bytes().chain([0xff]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

#[cfg(test)]
thread_local! {
    /// Unit tests: every revision this thread computes, while set (two
    /// label sets made to share a digest).
    pub static FORCED_REVISION: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// The cursor's chart in `view` and its labels' revision; `None` on any
/// other row. Allocates nothing.
fn cursor_chart(view: &SelectionView) -> Option<(&Path, u64)> {
    let row = view.rows.get(view.cursor?)?;
    let RowKey::Chart(path) = &row.key else {
        return None;
    };
    let selected = view.selected.as_ref()?;
    Some((path, labels_revision(&selected.labels)))
}

/// Takes `view`'s cursor into `want`: a new chart, revision or strings
/// replace it, its strings copied; any other row keeps the chart asked
/// last (the choice is made after selection is left).
pub fn update_want(want: &mut Option<Want>, view: &SelectionView) {
    let Some((chart, revision)) = cursor_chart(view) else {
        return;
    };
    let Some(l) = view.selected.as_ref().map(|s| &s.labels) else {
        return;
    };
    let strings = DecisionStrings::new(&ResultLabels {
        title: l.title.clone(),
        subtitle: l.subtitle.clone(),
        artist: l.artist.clone(),
        subartist: l.subartist.clone(),
        genre: l.genre.clone(),
    });
    if want
        .as_ref()
        .is_some_and(|w| w.chart == chart && w.revision == revision && w.strings == strings)
    {
        return;
    }
    *want = Some(Want {
        chart: chart.to_owned(),
        revision,
        strings,
    });
}

/// Whether `key` is `want`'s at `target`.
fn is_want(key: &LabelKey, want: &Want, target: [u32; 2]) -> bool {
    key.chart == want.chart && key.revision == want.revision && key.target == target
}

/// What the slots of `book` hold of `want` at `target`: whether one holds
/// its key with its strings (in `store`), ready, pinned or retired and
/// cooling ([`LabelBook::holds`]), and a ready slot whose key is `want`'s
/// but whose strings are not (equal digests of other labels), to free.
fn held(
    book: &LabelBook,
    store: &LabelStore,
    want: &Want,
    target: [u32; 2],
) -> (bool, Option<(usize, Gen)>) {
    let mut out = (false, None);
    for i in 0..LABEL_SLOTS {
        let (generation, key, ready) = match book.state(i) {
            Some(LabelState::Ready {
                generation, key, ..
            }) => (*generation, key, true),
            Some(
                LabelState::Pinned {
                    generation, key, ..
                }
                | LabelState::Cooling {
                    generation,
                    key,
                    from: render::CoolFrom::Retired,
                    ..
                },
            ) => (*generation, key, false),
            _ => continue,
        };
        if !is_want(key, want, target) {
            continue;
        }
        if store
            .strings(i, generation)
            .is_some_and(|s| *s == want.strings)
        {
            out.0 = true;
        } else if ready {
            out.1 = Some((i, generation));
        }
    }
    out
}

/// What a frame boundary's work came to.
#[derive(Debug)]
pub enum Boundary {
    /// Nothing for the run to act on.
    Quiet,
    /// The kind was refused on a healthy device (the preparation's
    /// producer, a write, a region misfit): disabled for the run.
    Refused(RenderError),
    /// The device or a poll failed: the screen is built again.
    Failed(RenderError),
}

/// What a rebuild of the pinned slot's content came to.
#[derive(Debug)]
pub enum Rebuilt {
    /// Every slot but the pinned one freed; the pinned one, if any,
    /// rebuilt at the new target under its generation.
    Restored,
    /// The kind was refused on a healthy device, a misfit at the new
    /// target included.
    Refused(RenderError),
    /// The device or a poll failed.
    Failed(RenderError),
}

/// The committed decision installation's label preparation: its worker,
/// the render thread's slot contents, its rows at the target and the key
/// last asked of the worker.
pub struct DecisionSide {
    bundle: Arc<SkinBundle>,
    worker: LabelWorker,
    store: LabelStore,
    plan: Option<Arc<RowPlan>>,
    /// The key and strings last asked of the worker, until superseded.
    posted: Option<(LabelKey, DecisionStrings)>,
    /// The next preparation's generation.
    next: Gen,
    /// Refused while a lifetime was active: no request is posted, and it
    /// is dropped once no lifetime is.
    pub retiring: bool,
    /// The preparations posted (tests).
    #[cfg(test)]
    pub posts: u64,
    /// While set, each post waits until the worker holds the new job's
    /// staging and notes the store's staging then in `staged_at_post`
    /// (tests: one generation's staging alive).
    #[cfg(test)]
    pub check_posts: bool,
    #[cfg(test)]
    pub staged_at_post: Vec<u64>,
}

impl std::fmt::Debug for DecisionSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionSide")
            .field("posted", &self.posted)
            .field("next", &self.next)
            .field("retiring", &self.retiring)
            .finish_non_exhaustive()
    }
}

impl DecisionSide {
    /// The preparation of `bundle`'s labels on `book`, its generations
    /// from `next`, its rows asked of `screen`.
    ///
    /// # Errors
    ///
    /// The worker could not be started, or the rows do not fit the region
    /// (a refusal of the kind).
    pub fn new(
        bundle: &Arc<SkinBundle>,
        book: &Arc<Mutex<LabelBook>>,
        next: Gen,
        screen: &dyn Screen,
    ) -> Result<Self, String> {
        let worker = LabelWorker::for_bundle(bundle, None).map_err(|e| e.to_string())?;
        let mut side = Self {
            bundle: Arc::clone(bundle),
            worker,
            store: LabelStore::new(Arc::clone(book)),
            plan: None,
            posted: None,
            next,
            retiring: false,
            #[cfg(test)]
            posts: 0,
            #[cfg(test)]
            check_posts: false,
            #[cfg(test)]
            staged_at_post: Vec::new(),
        };
        side.replan(screen)
            .map_err(|e| game::Chain(&e).to_string())?;
        Ok(side)
    }

    /// Whether it prepares `bundle`'s labels.
    #[must_use]
    pub fn is_for(&self, bundle: &Arc<SkinBundle>) -> bool {
        Arc::ptr_eq(&self.bundle, bundle)
    }

    /// The next generation it would post.
    #[must_use]
    pub const fn next(&self) -> Gen {
        self.next
    }

    /// The slot contents.
    #[must_use]
    pub const fn store(&self) -> &LabelStore {
        &self.store
    }

    /// The bytes the labels hold, the worker's staging included (tests).
    #[cfg(test)]
    #[must_use]
    pub fn memory(&self) -> render::DecisionMemory {
        self.store.memory(Some(&self.worker))
    }

    /// The bundle whose labels it prepares.
    #[must_use]
    pub const fn bundle(&self) -> &Arc<SkinBundle> {
        &self.bundle
    }

    /// Its rows asked again of `screen`, at its target now.
    ///
    /// # Errors
    ///
    /// The rows do not fit the region: a refusal of the kind.
    pub fn replan(&mut self, screen: &dyn Screen) -> Result<(), RenderError> {
        self.plan = None;
        if let Some(plan) = screen.decision_rows() {
            self.plan = Some(Arc::new(plan?));
        }
        Ok(())
    }

    /// Whether preparation work waits for a later boundary: `want` not
    /// asked yet, a job in flight, an output waiting, a slot filling or
    /// cooling.
    #[must_use]
    pub fn busy(&self, target: [u32; 2], want: Option<&Want>) -> bool {
        if self.store.filling().is_some() || self.store.waiting().is_some() {
            return true;
        }
        let book = lock(self.store.book());
        let asked = |w: &Want| {
            self.posted
                .as_ref()
                .is_some_and(|(k, s)| is_want(k, w, target) && *s == w.strings)
        };
        let ready = |w: &Want| held(&book, &self.store, w, target).0;
        let unasked =
            !self.retiring && self.plan.is_some() && want.is_some_and(|w| !ready(w) && !asked(w));
        let cooling =
            (0..LABEL_SLOTS).any(|i| matches!(book.state(i), Some(LabelState::Cooling { .. })));
        let in_flight = want.is_some_and(|w| asked(w) && !ready(w));
        unasked || cooling || in_flight
    }

    /// One frame boundary's work, outside every frame: a target change
    /// taken, the device's health read, ended lifetimes' slots retired,
    /// the worker's output taken, `want` asked (one job, the latest), a
    /// slot claimed, its writes bounded, and the slots settled. Whenever
    /// `want` is no longer the job asked last (another key or strings,
    /// a key a slot holds already included), that job is superseded:
    /// cancelled on the worker, its waiting output dropped and its filling
    /// slot cancelled. A new job is posted only after the store's staging
    /// was released, so at most one generation's staging is alive.
    pub fn boundary(&mut self, screen: &mut dyn Screen, want: Option<&Want>) -> Boundary {
        let book = Arc::clone(self.store.book());
        // A target change: the slots of another target freed, the rows
        // asked again at the new one.
        let target = screen.target();
        if lock(&book).current().0 != target {
            lock(&book).invalidate(target);
            if let Err(e) = self.replan(screen) {
                return Boundary::Refused(e);
            }
        }
        if screen.device_lost() {
            lock(&book).set_unhealthy();
        }
        lock(&book).retire_ended(&mut || screen.watch_completion());
        match self.worker.take() {
            Some(Ok(staged)) => self.store.offer(staged),
            Some(Err((_, e))) => return Boundary::Refused(e),
            None => {}
        }
        if !self.retiring
            && let (Some(want), Some(plan)) = (want, self.plan.clone())
        {
            if self
                .posted
                .as_ref()
                .is_some_and(|(k, s)| !is_want(k, want, target) || *s != want.strings)
            {
                self.worker.cancel();
                self.store.drop_waiting();
                self.store.cancel(&mut || screen.watch_completion());
                self.posted = None;
            }
            let (held, stale) = held(&lock(&book), &self.store, want, target);
            if let Some((slot, generation)) = stale {
                lock(&book).forget(slot, generation);
            }
            if !held && self.posted.is_none() {
                let key = LabelKey {
                    chart: want.chart.clone(),
                    revision: want.revision,
                    target,
                };
                let generation = self.next;
                self.next = self.next.saturating_add(1);
                // The store's staging released before the worker stages.
                self.store.drop_waiting();
                self.store.cancel(&mut || screen.watch_completion());
                self.worker.post(LabelJob {
                    generation,
                    key: key.clone(),
                    strings: want.strings.clone(),
                    plan,
                });
                #[cfg(test)]
                self.check_post();
                self.posted = Some((key, want.strings.clone()));
                #[cfg(test)]
                {
                    self.posts += 1;
                }
            }
        }
        self.store.claim();
        screen.write_decision_labels(&mut self.store);
        let settled = screen.settle_decision_labels(&mut self.store);
        self.store.prune();
        match settled {
            Ok(_) => Boundary::Quiet,
            Err(e) if e.is_refusal() => Boundary::Refused(e),
            Err(e) => Boundary::Failed(e),
        }
    }

    /// Unit tests: waits until the worker holds the job just posted (its
    /// staging, or its output), then notes the store's staging.
    #[cfg(test)]
    fn check_post(&mut self) {
        if !self.check_posts {
            return;
        }
        for _ in 0..60_000 {
            if self.worker.bytes().0 > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.staged_at_post.push(self.store.memory(None).staged);
    }

    /// The screen was built again: the worker quiesced (its job in flight
    /// cancelled and ended), the waiting output and the fill dropped with
    /// the old device's writes, the book reset (every slot free but the
    /// active lifetime's pinned one), the rows asked again at the new
    /// target, and that pinned slot's content rebuilt from its strings at
    /// that target under its generation, on this thread, the worker idle
    /// ([`LabelWorker::restage`]).
    pub fn rebuilt(&mut self, screen: &mut dyn Screen) -> Rebuilt {
        self.worker.quiesce();
        self.store.reset();
        self.posted = None;
        let book = Arc::clone(self.store.book());
        lock(&book).reset();
        let replanned = self.replan(screen);
        let pinned = lock(&book)
            .active_pin()
            .map(|(i, generation, key)| (i, generation, key.clone()));
        if let Err(e) = replanned {
            return Rebuilt::Refused(e);
        }
        let Some((slot, generation, key)) = pinned else {
            return Rebuilt::Restored;
        };
        let (Some(plan), Some(strings)) =
            (&self.plan, self.store.restore_strings(slot, generation))
        else {
            return Rebuilt::Refused(RenderError::LabelRegion { needed: 0, band: 0 });
        };
        let job = LabelJob {
            generation,
            key,
            strings: strings.clone(),
            plan: Arc::clone(plan),
        };
        let staged = match self.worker.restage(&self.bundle, job) {
            Ok(Some(staged)) => staged,
            Ok(None) => return Rebuilt::Restored,
            Err(e) => return Rebuilt::Refused(e),
        };
        match screen.restore_decision_labels(&mut self.store, slot, staged) {
            Ok(()) => Rebuilt::Restored,
            Err(e) if e.is_refusal() => Rebuilt::Refused(e),
            Err(e) => Rebuilt::Failed(e),
        }
    }

    /// Stops preparing: the job in flight and a waiting output dropped.
    pub fn stop(&mut self) {
        self.worker.cancel();
        self.store.drop_waiting();
        self.posted = None;
    }
}

/// What a decision frame's build left to draw.
#[derive(Debug, PartialEq, Eq)]
pub enum DecisionDrawn {
    /// The skin's frame, built, to submit.
    Built,
    /// The built-in presentation: no slot pinned.
    BuiltIn,
    /// The page: the screen's lifetime ended and its slot's content went
    /// with its retirement before the render left the screen. Not a
    /// refusal.
    Ended,
    /// The skin refused its frame, or its slot's content was missing:
    /// nothing of it was staged.
    Refused(BuildError),
}

/// The decision frame at `now` of the screen `shown` on a `target`-pixel
/// target: its timers from the choice, not from the frame's time. Allocates
/// nothing.
#[must_use]
pub fn decision_frame_of(
    shown: &ShownDecision,
    now: TimeUs,
    target: [u32; 2],
) -> DecisionFrame<'static> {
    DecisionFrame {
        now,
        target,
        layout: &Layout::AUTHOR,
        timers: DecisionTimers::at(shown.origin, &shown.timing, now),
    }
}

/// The app's share of a decision frame of `shown` at `now`: `dec` built
/// on `screen` from its pinned slot's content in `store`, borrowed. No
/// slot draws the built-in presentation; a slot whose content is missing
/// is a refusal while `shown`'s lifetime is the `active` one, else the
/// page of a screen that ended. Allocates nothing, refused or not.
pub fn decision_build(
    screen: &mut dyn Screen,
    dec: &mut DecisionScreen,
    store: Option<&LabelStore>,
    active: Option<u64>,
    shown: &ShownDecision,
    now: TimeUs,
) -> DecisionDrawn {
    let Some((slot, generation)) = shown.slot else {
        return DecisionDrawn::BuiltIn;
    };
    let Some(labels) = store.and_then(|s| s.content(slot, generation)) else {
        if active != Some(shown.lifetime) {
            return DecisionDrawn::Ended;
        }
        return DecisionDrawn::Refused(BuildError::Kind);
    };
    let frame = decision_frame_of(shown, now, screen.target());
    match screen.build_decision(dec, labels, &frame) {
        Ok(()) => DecisionDrawn::Built,
        Err(e) => DecisionDrawn::Refused(e),
    }
}
