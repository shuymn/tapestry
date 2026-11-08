//! The result screen's presentation (design.md "Play Lifecycle", a result
//! skin; the S10-09i design's D1, D4, D5 and D7): what the game thread
//! publishes for the render thread to draw, and the state the two share.
//!
//! A presentation is a view payload, made from one view snapshot (the
//! bundle or the built-in result, the view and its labels, the strip's
//! lines that depend on the view and the view's built-in page), with an
//! overlay merged on its own (the lifetime's start, the fade-out's start
//! and the strip's lines that do not depend on the view). The presenter
//! numbers each with its own revision, and the render thread takes the
//! newest of the current result lifetime through a slot, prepares a new
//! payload apart from what it draws, and promotes it at one commit point
//! before a frame's build. A barrier set when a leaving command is
//! accepted keeps the view presented then on screen: under it only the
//! overlay is merged.
//!
//! Not in-play code: it runs on the result screen and between plays.

use std::sync::Arc;

use bms_model::TimeUs;
use game::{PlayReport, ResultScreen, ViewStatus};
use render::SkinBundle;
use skin_model::{AtlasId, ResultChart, ResultLabels, ResultStatus, ResultView, Unavailable};

use crate::end_screen;
use crate::render_side::EndPage;
use crate::result_skin::Published;

/// The strip's lines at most: one per fact the skin cannot show (D5).
pub const STRIP_LINES: usize = render::RESULT_STRIP_SPRITES;

/// The columns a strip line holds before it is cut.
const STRIP_COLUMNS: usize = 76;

/// A view snapshot's identity: the presenter's number for it.
pub type ViewKey = u64;

/// Why the built-in result shows in place of the skin (D7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// This view's layout or staging was refused.
    View,
    /// The attempt's upload was refused or withdrawn, or its image stage
    /// decided the built-in result.
    Attempt,
    /// The skin was refused on a healthy device: for the rest of the run.
    Run,
}

/// What a view snapshot shows, as one unit (D1).
#[derive(Debug)]
pub struct ViewPayload {
    /// The snapshot it was made from.
    pub key: ViewKey,
    /// The layout that draws it; `None` for the built-in result.
    pub skin: Option<Arc<SkinBundle>>,
    /// Why the built-in result shows, when it does for a skin.
    pub why: Option<(Scope, String)>,
    /// The view the skin is updated with.
    pub view: ResultView,
    /// The chart's strings it shows.
    pub labels: ResultLabels,
    /// The strip's lines that depend on the view.
    pub lines: Vec<String>,
    /// The view's built-in page: its view rows draw when the built-in
    /// result shows.
    pub page: EndPage,
}

/// What merges into a presentation whatever view it shows (D1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overlay {
    /// The lifetime's start on the QPC clock: the `screen` timer.
    pub screen: TimeUs,
    /// The fade-out's start on the QPC clock (timer 2), once a leaving
    /// command was accepted.
    pub fadeout: Option<TimeUs>,
    /// The strip's lines that do not depend on the view.
    pub lines: Vec<String>,
}

/// One presentation of a result lifetime.
#[derive(Debug)]
pub struct ResultPresentation {
    /// The result lifetime it belongs to.
    pub lifetime: u64,
    /// Its revision in the lifetime: a newer one replaces an older one.
    pub revision: u64,
    /// The renderer request its staging takes, from the link's sequence.
    pub request: u64,
    pub payload: Arc<ViewPayload>,
    pub overlay: Overlay,
}

/// The newest presentation of the current result lifetime, until the
/// render thread takes it, and whether a signal for it waits.
#[derive(Debug, Default)]
pub struct ResultSlot {
    lifetime: u64,
    newest: Option<Box<ResultPresentation>>,
    signaled: bool,
}

impl ResultSlot {
    /// Puts `p` in the slot, replacing the one waiting, when its lifetime
    /// is current: whether a signal must be sent (none waits).
    pub fn offer(&mut self, p: ResultPresentation) -> bool {
        if p.lifetime != self.lifetime || self.lifetime == 0 {
            return false;
        }
        drop(self.newest.replace(Box::new(p)));
        !std::mem::replace(&mut self.signaled, true)
    }

    /// Takes the waiting presentation of the current lifetime.
    pub fn take(&mut self) -> Option<Box<ResultPresentation>> {
        self.signaled = false;
        self.newest.take().filter(|p| p.lifetime == self.lifetime)
    }

    /// Result lifetime `lifetime` begins (0 for none).
    pub fn enter(&mut self, lifetime: u64) {
        self.lifetime = lifetime;
        self.signaled = false;
        drop(self.newest.take());
    }

    /// Whether `lifetime` is the current one.
    pub const fn current(&self, lifetime: u64) -> bool {
        lifetime != 0 && self.lifetime == lifetime
    }
}

/// The barrier the game thread sets when a leaving command is accepted
/// (D4), and the view the render thread presents, written at its commit
/// point under the same lock.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResultBarrier {
    /// The lifetime the rest belongs to.
    pub lifetime: u64,
    /// The view presented at the last commit point.
    pub presented: Option<ViewKey>,
    /// The view pinned by the barrier, once set.
    pub pinned: Option<ViewKey>,
    /// The fade-out's start the barrier was set with: set, the barrier
    /// holds, a view pinned or none presented yet.
    pub fadeout: Option<TimeUs>,
    /// The presented view's skin timing while its skin draws: `None` for
    /// a built-in view, before the first commit and once the kind is
    /// disabled (the S10-09i design's D6, D7).
    pub timing: Option<skin_model::ResultTiming>,
}

/// What the render thread shows of the result kind, and the run's
/// register (D7): read by the game thread at every look.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultReport {
    /// Why the result kind is disabled for the run, once it is.
    pub disabled: Option<String>,
    /// The lifetime the rest describes.
    pub lifetime: u64,
    /// The revision presented last.
    pub revision: u64,
    /// The view presented last, and whether its skin draws.
    pub shown: Option<(ViewKey, bool)>,
    /// The view the barrier pinned.
    pub pinned: Option<ViewKey>,
    /// Every atlas the renderer references for the result kind
    /// (installed, staged, retired, kept for recovery), each once.
    pub retained: Vec<AtlasId>,
    /// A view whose staging was refused, with why.
    pub refused: Option<(ViewKey, String)>,
    /// The atlas of the result's installation committed (or pages-only),
    /// kept to restore.
    pub installed: Option<AtlasId>,
}

/// `text` cut to the strip's width.
fn cut(text: &str) -> String {
    if text.chars().count() <= STRIP_COLUMNS {
        return text.to_owned();
    }
    let mut s: String = text.chars().take(STRIP_COLUMNS - 2).collect();
    s.push_str("..");
    s
}

/// The strip's lines that depend on the view of `screen` (D5, in its
/// table's order): the coverage, the best's eligibility, the view's
/// unavailability, the viewed preset when it is not the play's own, and
/// the viewed file's error. Each only while it applies.
pub fn view_lines(report: &PlayReport, screen: &ResultScreen) -> Vec<String> {
    let status = screen.status();
    let viewed = screen.viewed();
    let name = viewed.map_or("", |e| e.name.as_str());
    let mut lines = Vec::new();
    let coverage = end_screen::coverage_line(status);
    if !coverage.is_empty() {
        lines.push(coverage);
    }
    // Ineligible either way: counted without setting a best, or not
    // counted at all.
    if let Some(
        game::BestView::Counted {
            ineligible: Some(why),
            ..
        }
        | game::BestView::NotCounted(why),
    ) = &report.best
    {
        lines.push(format!("BEST NOT UPDATED: {why}"));
    }
    if !matches!(status, ViewStatus::Ready(_)) {
        lines.push(end_screen::view_label(status, name));
    }
    if viewed.is_some_and(|e| e.hash != Some(screen.played())) {
        lines.push(format!("VIEWING {name}"));
    }
    if let Some(e) = screen.file_error() {
        lines.push(format!(
            "FILE DID NOT RELOAD: {}",
            end_screen::preset_error_text(e)
        ));
    }
    lines.iter().map(|l| cut(l)).collect()
}

/// The facts behind the strip's lines that do not depend on the view
/// (D5): what a play's report and its result screen say, kept while the
/// result shows so the offset guidance and a thread's failure can join
/// them later.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlayFacts {
    /// A cancellation before playback, a save failure and a mismatch with
    /// the live stream, in that order, each while it applies.
    before: Vec<String>,
    /// The recommended shift's median, for the offset guidance: `None`
    /// when the play gives none (autoplay, nothing evaluated), `Some(None)`
    /// while too few presses were judged.
    median: Option<Option<i64>>,
    /// The latest two notices and the play's reload error.
    after: Vec<String>,
}

impl OverlayFacts {
    /// The facts of `report` and `screen`; an `autoplay` play gives no
    /// offset guidance.
    pub fn of(report: &PlayReport, screen: &ResultScreen, autoplay: bool) -> Self {
        let mut before = Vec::new();
        if report
            .record
            .as_ref()
            .is_some_and(|r| r.status.end == replay::EndStatus::CancelledBeforePlayback)
        {
            before.push("CANCELLED BEFORE PLAYBACK".to_owned());
        }
        if let Some(Err(e)) = &report.saved {
            before.push(format!("NOT SAVED: {}", game::Chain(e)));
        }
        if report.result.as_ref().is_some_and(|s| !s.matches_live) {
            before.push("RE-EVALUATION DIFFERS FROM THE LIVE PLAY".to_owned());
        }
        let median = report
            .result
            .as_ref()
            .filter(|_| !autoplay)
            .map(|s| s.recommended_offset.map(|r| r.median.0));
        let mut after: Vec<String> = screen
            .notices()
            .take(2)
            .map(end_screen::notice_line)
            .collect();
        if let Some(e) = &report.reload_error {
            after.push(format!(
                "THIS PLAY USED THE PREVIOUS {}: {}",
                report.preset.id,
                end_screen::preset_error_text(e)
            ));
        }
        Self {
            before,
            median,
            after,
        }
    }

    /// The strip's lines in D5's order: with `offset`, the input offset
    /// now while the offset keys were used in this lifetime (O6), its
    /// guidance; with `failure`, a thread's failure the next command ends
    /// the run with (O7).
    pub fn lines(&self, offset: Option<i64>, failure: Option<String>) -> Vec<String> {
        let guidance = match (offset, self.median) {
            (Some(now), Some(Some(median))) => Some(format!(
                "INPUT OFFSET {} MS, RECOMMENDED {} MS",
                end_screen::ms(now),
                end_screen::ms(now.saturating_add(median))
            )),
            (Some(now), Some(None)) => Some(format!(
                "INPUT OFFSET {} MS, A RECOMMENDATION NEEDS {} JUDGED PRESSES",
                end_screen::ms(now),
                game::MIN_OFFSET_SAMPLES
            )),
            (Some(now), None) => Some(format!("INPUT OFFSET {} MS", end_screen::ms(now))),
            (None, _) => None,
        };
        self.before
            .iter()
            .cloned()
            .chain(guidance)
            .chain(self.after.iter().cloned())
            .chain(failure)
            .map(|l| cut(&l))
            .collect()
    }
}

/// The rows of the built-in page that describe the view: the preset
/// strip, the view's label, its coverage, the viewed file's error, the
/// counts and timing, and the gauges.
const VIEW_ROWS: [usize; 11] = [1, 2, 3, 4, 8, 9, 10, 11, 13, 14, 15];

/// The rows a reason the skin is not shown may take, the first empty one.
const REASON_ROWS: [usize; 4] = [21, 19, 6, 5];

/// The built-in page drawn for `view`'s payload: its view rows from the
/// payload's page and its graphs, every other row (and the settings lines)
/// from `latest`, the newest page; with `why`, its notice in the first
/// empty row of [`REASON_ROWS`], else over the load times (D1, D7).
pub fn compose_page(view: &EndPage, latest: &EndPage, why: Option<&str>) -> EndPage {
    let mut lines = latest.lines.clone();
    let rows = end_screen::SUMMARY_LINES.max(lines.len());
    lines.resize(rows, String::new());
    for i in VIEW_ROWS {
        if let (Some(to), Some(from)) = (lines.get_mut(i), view.lines.get(i)) {
            to.clone_from(from);
        }
    }
    if let Some(why) = why {
        let row = REASON_ROWS
            .into_iter()
            .find(|&i| lines.get(i).is_some_and(String::is_empty))
            .unwrap_or(23);
        if let Some(line) = lines.get_mut(row) {
            *line = end_screen::fit(
                &format!("RESULT SKIN NOT SHOWN: {why}"),
                end_screen::COLUMNS,
            );
        }
    }
    EndPage {
        lines,
        graphs: view.graphs.clone(),
    }
}

/// The view of `screen` a skin is updated with, with `chart`: its parts
/// while it is ready, else the chart's facts under its unavailability.
pub fn view_of(screen: &ResultScreen, chart: ResultChart) -> ResultView {
    let preset = screen.viewed().and_then(|e| e.preset.as_deref());
    let why = match (screen.status(), preset) {
        (ViewStatus::Ready(summary), Some(preset)) => {
            let parts = crate::result_view::result_parts(summary, preset, &screen.basis());
            return ResultView {
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
        }
        (ViewStatus::NothingRecorded, _) => Unavailable::NothingRecorded,
        (ViewStatus::CannotLoad(_), _) | (ViewStatus::Ready(_), None) => Unavailable::CannotLoad,
        (ViewStatus::OverBudget { .. }, _) => Unavailable::OverBudget,
        (ViewStatus::Failed(_), _) => Unavailable::Failed,
    };
    ResultView {
        status: ResultStatus::Unavailable(why),
        ..crate::result_skin::probe_view(chart)
    }
}

/// The chart's strings a result skin shows, from the attempt's labels.
pub fn labels_of(l: &game::ChartLabels) -> ResultLabels {
    ResultLabels {
        title: l.title.clone(),
        subtitle: l.subtitle.clone(),
        artist: l.artist.clone(),
        subartist: l.subartist.clone(),
        genre: l.genre.clone(),
    }
}

/// One view snapshot, as the game thread holds it until its layout lands.
#[derive(Debug)]
pub struct Snapshot {
    pub view: ResultView,
    pub labels: ResultLabels,
    pub lines: Vec<String>,
    pub page: EndPage,
}

/// Where the presenter sends what it publishes: the render link, or a
/// double in tests.
pub trait Publish {
    /// A new result lifetime: its number.
    fn begin(&mut self) -> u64;
    /// The current result lifetime ends.
    fn end(&mut self);
    /// The next request of the renderer's sequence.
    fn request(&mut self) -> u64;
    /// Shows `p`.
    fn show(&mut self, p: ResultPresentation);
    /// Sets the barrier of the current lifetime with the fade-out's start
    /// `at`: the view presented then, and its skin's timing, read under
    /// the same lock; `None`, nothing set, when that view's input time has
    /// not passed from `gate`.
    fn pin(
        &mut self,
        at: TimeUs,
        gate: Option<TimeUs>,
    ) -> Option<(Option<ViewKey>, Option<skin_model::ResultTiming>)>;
    /// The current lifetime's presented view's skin timing.
    fn timing(&self) -> Option<skin_model::ResultTiming>;
}

/// The game thread's side of the result's presentation (D1, D4, D7): it
/// keeps the desired view's snapshot, makes a payload once its layout is
/// published, merges the overlay, and numbers every change.
#[derive(Debug, Default)]
pub struct Presenter {
    /// The current result lifetime, while one is.
    lifetime: Option<u64>,
    revision: u64,
    next_key: ViewKey,
    /// The desired view: its key and snapshot.
    desired: Option<(ViewKey, Snapshot)>,
    /// The payload published last.
    payload: Option<Arc<ViewPayload>>,
    /// The overlay; its fade-out set, the barrier is: no view payload is
    /// published any more.
    overlay: Overlay,
    /// The attempt's fallback (D7), cleared by the next attempt.
    attempt: Option<String>,
    /// A view's fallback: its key and why.
    view: Option<(ViewKey, String)>,
    /// The run's register, as the render thread reported it.
    disabled: Option<String>,
}

impl Presenter {
    /// Whether a result lifetime is current.
    pub const fn current(&self) -> bool {
        self.lifetime.is_some()
    }

    /// A result lifetime begins at `now` on the QPC clock (the result
    /// shown first after its preparation).
    pub fn begin(&mut self, to: &mut impl Publish, now: TimeUs) {
        self.lifetime = Some(to.begin());
        self.revision = 0;
        self.payload = None;
        self.desired = None;
        self.view = None;
        self.overlay = Overlay {
            screen: now,
            fadeout: None,
            lines: Vec::new(),
        };
    }

    /// The result is left: its lifetime ends, and nothing of it is drawn.
    pub fn end(&mut self, to: &mut impl Publish) {
        if self.lifetime.take().is_some() {
            to.end();
        }
        self.payload = None;
        self.desired = None;
    }

    /// The next attempt: the attempt's fallback is forgotten.
    pub fn next_attempt(&mut self) {
        self.attempt = None;
    }

    /// The attempt shows the built-in result for every view, for `why`.
    pub fn attempt_fallback(&mut self, why: String) {
        self.attempt = Some(why);
    }

    /// The attempt's fallback, if any.
    pub fn attempt_fallen(&self) -> Option<&str> {
        self.attempt.as_deref()
    }

    /// A new desired view, `snapshot`: its key. Until its layout lands
    /// ([`land`](Self::land)) the view presented stays.
    pub fn want(&mut self, snapshot: Snapshot) -> ViewKey {
        self.next_key += 1;
        self.desired = Some((self.next_key, snapshot));
        self.next_key
    }

    /// The desired view's layout landed as `shown` (the result slot's
    /// publication for the latest request): its payload is published,
    /// unless the barrier holds or it is the one published.
    pub fn land(&mut self, to: &mut impl Publish, shown: Option<&Published>) {
        if self.overlay.fadeout.is_some() || self.lifetime.is_none() {
            return;
        }
        let Some((key, snapshot)) = &self.desired else {
            return;
        };
        let (skin, why) = match (shown, &self.disabled, &self.attempt, &self.view) {
            (_, Some(why), _, _) => (None, Some((Scope::Run, why.clone()))),
            (_, _, Some(why), _) => (None, Some((Scope::Attempt, why.clone()))),
            (_, _, _, Some((k, why))) if k == key => (None, Some((Scope::View, why.clone()))),
            (Some(Published::Skin(b, _)), _, _, _) => (Some(Arc::clone(b)), None),
            (Some(Published::BuiltIn), _, _, _) => (
                None,
                Some((Scope::View, "the view's layout was refused".to_owned())),
            ),
            (None, _, _, _) => return,
        };
        let same = self.payload.as_ref().is_some_and(|p| {
            p.key == *key
                && match (&p.skin, &skin) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    (None, None) => true,
                    _ => false,
                }
        });
        if same {
            return;
        }
        let payload = Arc::new(ViewPayload {
            key: *key,
            skin,
            why,
            view: snapshot.view.clone(),
            labels: snapshot.labels.clone(),
            lines: snapshot.lines.clone(),
            page: snapshot.page.clone(),
        });
        self.payload = Some(payload);
        self.publish(to);
    }

    /// The overlay's lines are `lines`: published when they changed.
    pub fn lines(&mut self, to: &mut impl Publish, lines: Vec<String>) {
        if self.overlay.lines == lines {
            return;
        }
        self.overlay.lines = lines;
        self.publish(to);
    }

    /// The render thread's status `s`: the run's register, and a view
    /// refused to the built-in result (stale keys change nothing, C17e).
    /// Whether the desired view must be published again.
    pub fn status(&mut self, s: &ResultReport) -> bool {
        let mut changed = false;
        if s.disabled.is_some() && self.disabled.is_none() {
            self.disabled.clone_from(&s.disabled);
            changed = true;
        }
        if let (Some((key, why)), Some((desired, _))) = (&s.refused, &self.desired)
            && key == desired
            && self.view.as_ref().is_none_or(|(k, _)| k != key)
        {
            self.view = Some((*key, why.clone()));
            changed = true;
        }
        changed
    }

    /// The run's register, once set.
    pub fn disabled(&self) -> Option<&str> {
        self.disabled.as_deref()
    }

    /// Sets the barrier at `now` (D4): timer 2 starts, the view presented
    /// then is pinned, and from here no view payload is published; the
    /// overlay with the fade-out follows. Gives the pinned view and its
    /// skin's timing, which the outro plays (D6); `None`, nothing pinned,
    /// when the view on screen's input time has not passed from `gate`.
    pub fn pin(
        &mut self,
        to: &mut impl Publish,
        now: TimeUs,
        gate: Option<TimeUs>,
    ) -> Option<(Option<ViewKey>, Option<skin_model::ResultTiming>)> {
        if self.lifetime.is_none() {
            return Some((None, None));
        }
        let pinned = to.pin(now, gate)?;
        self.overlay.fadeout = Some(now);
        self.publish(to);
        Some(pinned)
    }

    /// The timing of the result skin on screen (the S10-09i design's D6,
    /// D7): the view the render thread presents, while its skin draws;
    /// `None` outside a lifetime, for a built-in view, before the
    /// lifetime's first frame and once the run disabled the kind. A view
    /// published and not yet presented has no say.
    pub fn timing(&self, to: &impl Publish) -> Option<skin_model::ResultTiming> {
        self.lifetime?;
        to.timing()
    }

    /// Publishes the payload published last with the overlay as it is,
    /// under the next revision.
    fn publish(&mut self, to: &mut impl Publish) {
        let (Some(lifetime), Some(payload)) = (self.lifetime, &self.payload) else {
            return;
        };
        self.revision += 1;
        let p = ResultPresentation {
            lifetime,
            revision: self.revision,
            request: to.request(),
            payload: Arc::clone(payload),
            overlay: self.overlay.clone(),
        };
        to.show(p);
    }
}

#[cfg(test)]
mod tests;
