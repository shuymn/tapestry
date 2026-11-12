//! The decision screen on the render thread (design.md "Play Lifecycle",
//! the decision screen): the labels' preparation for the chart under the
//! cursor, driven at frame boundaries on WARP; the skin drawn from the
//! pinned slot, through a screen built again, an attempt's preparation and
//! a later mode; the takeover for the active lifetime only; and the kind
//! disabled for the run on a refusal. The game's side (its pin, its
//! lifetime) is played by the test through the label book.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bms_model::{ChartBuilder, TimeSource, TimeUs};
use game::Preparation;
use render::{FrameInput, FrameTiming, LabelBook, LabelState, RenderError, SkinBundle, lock};
use skin_model::{
    ChartLabels, DecisionTiming, RowKey, RowKind, RowScore, RowView, ScreenKind, SelectedChart,
    SelectionView,
};
use tempfile::TempDir;

use super::super::{Extent, MakeScreen, RenderLink, Screen};
use super::{one_line, preset_attempt, spawn_empty, wait_until};
use crate::decision_side::{
    Boundary, DecisionDrawn, DecisionSide, Rebuilt, ShownDecision, Want, decision_build,
    decision_frame_of, labels_revision, update_want,
};
use crate::menu_screen::MenuScreen;
use crate::render_outline::{Value, decision_bundle_in, decision_folder, default_font, text, ttf};
use crate::selection_skin::{SelectionEvents, SelectionUpdate, StripText};

const TARGET: [u32; 2] = [1_920, 1_080];
/// The title's box, y down: 18 pixels tall.
const TITLE: [i32; 4] = [100, 100, 1_200, 18];

/// A decision skin with the title (ref 10) and the artist (ref 16) in a
/// generated Latin face, at em 18.
fn skin_dir() -> TempDir {
    decision_folder(
        &[(
            "ttf.ttf",
            crate::font_support::build(&crate::font_support::latin("Skin")),
        )],
        &[ttf()],
        &[
            text("f", 18, Value::Ref(10), TITLE),
            text("f", 18, Value::Ref(16), [100, 600, 1_200, 18]),
        ],
    )
}

/// The labels of a chart titled `title`, its artist `AB`.
fn labels(title: &str) -> ChartLabels {
    ChartLabels {
        title: title.to_owned(),
        artist: "AB".to_owned(),
        ..ChartLabels::default()
    }
}

/// The selection view with the cursor on `chart`, titled `title`.
fn view_on(chart: &str, title: &str) -> SelectionView {
    let l = labels(title);
    SelectionView {
        rows: vec![RowView {
            key: RowKey::Chart(chart.into()),
            kind: RowKind::Chart,
            title: l.title.clone(),
            subtitle: String::new(),
            artist: l.artist.clone(),
            level: String::new(),
            level_value: 0,
            difficulty: None,
            score: RowScore::Unplayed,
            features: None,
            random: false,
        }],
        cursor: Some(0),
        selected: Some(SelectedChart {
            labels: l,
            summary: None,
            random: false,
            max_ex: None,
            score: RowScore::Unplayed,
            judge_class: None,
        }),
        ..SelectionView::default()
    }
}

/// The preparation asked for `chart` titled `title`.
fn want(chart: &str, title: &str) -> Option<Want> {
    let mut w = None;
    update_want(&mut w, &view_on(chart, title));
    w
}

/// Whether a slot of `book` is ready with `chart` titled `title`.
fn ready(book: &LabelBook, chart: &str, title: &str) -> bool {
    let revision = labels_revision(&labels(title));
    (0..render::LABEL_SLOTS).any(|i| {
        matches!(book.state(i), Some(LabelState::Ready { key, .. })
            if key.chart == Path::new(chart) && key.revision == revision)
    })
}

/// The pinned slot of `book`, its generation and its timing.
fn pinned(book: &LabelBook) -> Option<(usize, u64, DecisionTiming)> {
    (0..render::LABEL_SLOTS).find_map(|i| match book.state(i) {
        Some(LabelState::Pinned {
            generation, timing, ..
        }) => Some((i, *generation, *timing)),
        _ => None,
    })
}

/// One frame as a [`Recorder`] drew it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Drew {
    /// A decision skin's frame: its device, its frame time and its
    /// `screen` timer's start.
    Skin {
        device: u64,
        now: TimeUs,
        screen: Option<TimeUs>,
    },
    /// A page: its device and its first line.
    Page { device: u64, line: String },
}

/// What the test sets and reads of the [`Recorder`]s a render thread
/// builds, their devices numbered from 1.
#[derive(Default)]
struct Seen {
    builds: AtomicU64,
    /// Frames fail as a lost surface while set.
    fail: AtomicBool,
    /// The next decision frame is refused by the renderer.
    refuse_frame: AtomicBool,
    /// The next settling of the labels fails as a poll fails.
    fail_settle: AtomicBool,
    /// A filling slot's settling is refused, as a refused write is, while
    /// set.
    refuse_settle: AtomicBool,
    /// The decision kind's restore is refused on a healthy device while
    /// set.
    refuse_restore: AtomicBool,
    /// The next pinned labels' restore fails as a lost device.
    fail_label_restore: AtomicBool,
    /// This many chart loads fail as a lost surface.
    lose_loads: AtomicU32,
    /// The decision installation's removals.
    removed: AtomicU32,
    /// The next skin frame's lit pixels in the title's box are noted.
    probe: AtomicBool,
    lit: Mutex<Vec<(u64, usize)>>,
    drew: Mutex<Vec<Drew>>,
}

impl Seen {
    fn skins(&self, from: usize) -> usize {
        self.drew
            .lock()
            .unwrap()
            .iter()
            .skip(from)
            .filter(|d| matches!(d, Drew::Skin { .. }))
            .count()
    }

    fn pages(&self, from: usize, line: &str) -> usize {
        self.drew
            .lock()
            .unwrap()
            .iter()
            .skip(from)
            .filter(|d| matches!(d, Drew::Page { line: l, .. } if l == line))
            .count()
    }

    fn len(&self) -> usize {
        self.drew.lock().unwrap().len()
    }
}

/// A real menu screen on WARP that notes each frame ([`Drew`]) and fails
/// or refuses what [`Seen`] says.
struct Recorder {
    menu: MenuScreen,
    device: u64,
    seen: Arc<Seen>,
}

impl Screen for Recorder {
    fn load(
        &mut self,
        chart: &bms_model::Chart,
        section: Option<&eval::Section>,
    ) -> Result<(), RenderError> {
        if self
            .seen
            .lose_loads
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(RenderError::SurfaceLost);
        }
        self.menu.load(chart, section)
    }
    fn prepare_attempt(
        &mut self,
        a: &render::AttemptInput<'_>,
        skin: Option<&Arc<SkinBundle>>,
        request: u64,
    ) -> Option<Result<Preparation, RenderError>> {
        self.menu.prepare_attempt(a, skin, request)
    }
    fn device(&self) -> u64 {
        self.menu.device()
    }
    fn commit(&mut self, token: render::PreparedToken) -> Result<(), render::CommitError> {
        self.menu.commit(token)
    }
    fn discard(&mut self) {
        self.menu.discard();
    }
    fn install(
        &mut self,
        kind: ScreenKind,
        skin: &Arc<SkinBundle>,
        request: u64,
    ) -> Result<Preparation, RenderError> {
        self.menu.install(kind, skin, request)
    }
    fn installation(&self, kind: ScreenKind) -> Option<render::Installation> {
        self.menu.installation(kind)
    }
    fn restore(
        &mut self,
        kind: ScreenKind,
        installation: &render::Installation,
    ) -> Result<(), RenderError> {
        if kind == ScreenKind::Decision && self.seen.refuse_restore.load(Ordering::Acquire) {
            return Err(RenderError::Skin(render::SkinError {
                reason: "refused for the test".to_owned(),
            }));
        }
        self.menu.restore(kind, installation)
    }
    fn remove(&mut self, kind: ScreenKind) {
        if kind == ScreenKind::Decision {
            self.seen.removed.fetch_add(1, Ordering::AcqRel);
        }
        self.menu.remove(kind);
    }
    fn decision_rows(&self) -> Option<Result<render::RowPlan, RenderError>> {
        self.menu.decision_rows()
    }
    fn write_decision_labels(&mut self, store: &mut render::LabelStore) -> render::FillStep {
        self.menu.write_decision_labels(store)
    }
    fn settle_decision_labels(
        &mut self,
        store: &mut render::LabelStore,
    ) -> Result<render::Settled, RenderError> {
        if self.seen.refuse_settle.load(Ordering::Acquire) && store.filling().is_some() {
            return Err(RenderError::LabelRegion { needed: 1, band: 0 });
        }
        if self.seen.fail_settle.swap(false, Ordering::AcqRel) {
            self.menu.decision_fault(true);
        }
        self.menu.settle_decision_labels(store)
    }
    fn restore_decision_labels(
        &mut self,
        store: &mut render::LabelStore,
        slot: usize,
        staged: render::StagedLabels,
    ) -> Result<(), RenderError> {
        if self.seen.fail_label_restore.swap(false, Ordering::AcqRel) {
            return Err(RenderError::DeviceLost);
        }
        self.menu.restore_decision_labels(store, slot, staged)
    }
    fn watch_completion(&self) -> Arc<AtomicBool> {
        self.menu.watch_completion()
    }
    fn device_lost(&self) -> bool {
        self.menu.device_lost()
    }
    fn build_decision(
        &mut self,
        dec: &mut skin_model::DecisionScreen,
        labels: skin_model::DecisionLabels<'_>,
        frame: &skin_model::DecisionFrame<'_>,
    ) -> Result<(), skin_model::BuildError> {
        if self.seen.refuse_frame.swap(false, Ordering::AcqRel) {
            self.menu.decision_fault(false);
        }
        self.menu.build_decision(dec, labels, frame)?;
        self.seen.drew.lock().unwrap().push(Drew::Skin {
            device: self.device,
            now: frame.now,
            screen: frame.timers.screen,
        });
        Ok(())
    }
    fn submit_decision(&mut self) -> Result<FrameTiming, RenderError> {
        if self.seen.fail.load(Ordering::Acquire) {
            return Err(RenderError::SurfaceLost);
        }
        let timing = self.menu.submit_decision()?;
        if self.seen.probe.swap(false, Ordering::AcqRel)
            && let Some((w, _, pixels)) = self.menu.read_back()
        {
            let [x, y, bw, bh] = TITLE.map(|v| u32::try_from(v).unwrap());
            let lit = (y..y + bh)
                .flat_map(|py| (x..x + bw).map(move |px| (px, py)))
                .filter(|&p| crate::render_common::rgb_at(&pixels, w, p)[0] > 128)
                .count();
            self.seen.lit.lock().unwrap().push((self.device, lit));
        }
        Ok(timing)
    }
    fn wait(&self) -> bool {
        self.menu.wait()
    }
    fn paced(&self) -> bool {
        self.menu.paced()
    }
    fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
        self.menu.build(f)
    }
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        self.menu.draw()
    }
    fn text(&mut self, lines: &[&str]) -> Result<bool, RenderError> {
        self.menu.text(lines)
    }
    fn menu(
        &mut self,
        page: &crate::menu_screen::MenuPage,
        changed: bool,
    ) -> Result<FrameTiming, RenderError> {
        if self.seen.fail.load(Ordering::Acquire) {
            return Err(RenderError::SurfaceLost);
        }
        let line = page
            .lines
            .first()
            .map_or_else(String::new, |l| l.text.clone());
        self.seen.drew.lock().unwrap().push(Drew::Page {
            device: self.device,
            line,
        });
        self.menu.menu(page, changed)
    }
    fn resize(&mut self, width: u32, height: u32) {
        self.menu.resize(width, height);
    }
    fn target(&self) -> [u32; 2] {
        self.menu.target()
    }
    fn atlas_limits(&self) -> Option<skin_model::AtlasLimits> {
        self.menu.atlas_limits()
    }
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        self.menu.read_back()
    }
}

/// A render thread on [`Recorder`]s with the decision skin of `dir`
/// committed, holding WARP's turn.
struct Rig {
    link: RenderLink,
    seen: Arc<Seen>,
    book: Arc<Mutex<LabelBook>>,
    extent: Arc<Extent>,
    _turn: crate::warp_turn::Turn,
}

impl Rig {
    fn new(dir: &TempDir) -> Self {
        let turn = crate::warp_turn::take();
        let bundle = {
            let probe = crate::render_common::offscreen(TARGET[0], TARGET[1]);
            decision_bundle_in(&probe, dir.path(), &default_font())
        };
        let seen = Arc::new(Seen::default());
        let s = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            let r = render::Renderer::new(
                render::Target::Offscreen {
                    width: TARGET[0],
                    height: TARGET[1],
                },
                render::AdapterChoice::Warp,
            )?;
            let device = s.builds.fetch_add(1, Ordering::AcqRel) + 1;
            Ok(Box::new(Recorder {
                menu: MenuScreen::new(r, Some(render::text::Fonts::runtime()?)),
                device,
                seen: Arc::clone(&s),
            }) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        let extent = Arc::new(Extent::default());
        link.follow(&extent);
        let answer = link.select(ScreenKind::Decision, Some(bundle)).wait();
        assert_eq!(answer.unwrap(), Preparation::Ready);
        let book = link.label_book();
        Self {
            link,
            seen,
            book,
            extent,
            _turn: turn,
        }
    }

    /// Update `revision` of selection lifetime 1, its cursor on `chart`
    /// titled `title`.
    fn cursor(&self, revision: u64, chart: &str, title: &str) {
        self.link.enter_selection(1);
        self.link.show_selection(SelectionUpdate {
            lifetime: 1,
            revision,
            list_revision: 1,
            page: one_line("select"),
            view: view_on(chart, title),
            labels: crate::select::labels(),
            strip: StripText::default(),
            events: SelectionEvents::default(),
            settings: None,
        });
    }

    /// The cursor on `chart` until its labels are ready.
    fn prepared(&self, chart: &str, title: &str) {
        self.cursor(1, chart, title);
        assert!(
            wait_until(|| ready(&lock(&self.book), chart, title)),
            "{chart} never ready: {:?}",
            lock(&self.book)
        );
    }

    /// The game's choice of `chart` titled `title` under `lifetime`: its
    /// ready slot pinned and the skinned screen shown, chosen now.
    fn choose(&self, chart: &str, title: &str, lifetime: u64) -> ShownDecision {
        let revision = labels_revision(&labels(title));
        let (slot, generation, timing) = lock(&self.book)
            .pin(Path::new(chart), revision, lifetime)
            .expect("a ready slot pinned");
        let shown = ShownDecision {
            lifetime,
            origin: self.link.clock.now(),
            timing,
            slot: Some((slot, generation)),
        };
        self.link.leave_selection();
        self.link.show_decision(one_line("page"), Some(shown));
        shown
    }

    /// Waits for `n` skin frames after the `from`th frame.
    fn skin_frames(&self, from: usize, n: usize) {
        assert!(
            wait_until(|| self.seen.skins(from) >= n),
            "{:?}",
            self.seen
                .drew
                .lock()
                .unwrap()
                .iter()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
        );
    }

    /// The pinned skin drawn: `chart`'s labels prepared, chosen under
    /// lifetime 5, three skin frames drawn.
    fn drawing(&self, chart: &str, title: &str) -> ShownDecision {
        self.prepared(chart, title);
        let shown = self.choose(chart, title, 5);
        self.skin_frames(0, 3);
        shown
    }

    /// The screen lost on its next frame and built again, as the flow
    /// does.
    fn rebuild(&self) -> Result<(), crate::render_side::ScreenError> {
        self.seen.fail.store(true, Ordering::Release);
        assert!(wait_until(|| self.link.failed()));
        self.seen.fail.store(false, Ordering::Release);
        self.link.prepare()
    }

    /// The rebuilds the label book has seen.
    fn epoch(&self) -> u64 {
        lock(&self.book).current().1
    }

    /// Another preparation refused while the pinned skin draws: a resize
    /// re-prepares the pinned chart at the new target, its settling
    /// refused.
    fn refuse_another(&self) {
        self.seen.refuse_settle.store(true, Ordering::Release);
        self.extent.publish(1_280, 720);
        assert!(wait_until(|| self.link.take_decision_refusal().is_some()));
    }
}

/// C4: a skinned frame's timers run from the choice (7,000 ms), not from
/// the frame's time: `screen` at the choice throughout, the fade-out from
/// 2,500 ms after it, the input timer past 500 ms.
#[test]
fn decision_frame_timers_at_0_2499_2500_2999() {
    let origin = TimeUs(7_000_000);
    let shown = ShownDecision {
        lifetime: 1,
        origin,
        timing: DecisionTiming {
            scene: TimeUs(2_500_000),
            fadeout: TimeUs(500_000),
            input: TimeUs(500_000),
        },
        slot: Some((0, 1)),
    };
    let at = |ms: i64| TimeUs(origin.0 + ms * 1_000);
    for (ms, fadeout) in [
        (0, None),
        (2_499, None),
        (2_500, Some(at(2_500))),
        (2_999, Some(at(2_500))),
    ] {
        let f = decision_frame_of(&shown, at(ms), TARGET);
        assert_eq!(f.now, at(ms));
        assert_eq!(f.timers.screen, Some(origin), "{ms} ms");
        assert_eq!(f.timers.fadeout, fadeout, "{ms} ms");
        assert_eq!(f.timers.input, (ms > 500).then_some(at(500)), "{ms} ms");
    }
}

thread_local! {
    /// The completions a holding [`Direct`] made on this thread.
    static HELD: std::cell::RefCell<Vec<Arc<AtomicBool>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Fires every completion a holding [`Direct`] made on this thread.
fn fire_held() {
    HELD.with(|h| {
        for f in h.borrow_mut().drain(..) {
            f.store(true, Ordering::Release);
        }
    });
}

/// A screen of the test thread over a real renderer on WARP with the
/// decision skin committed, whose completions the test fires itself
/// ([`fire_held`]) while `hold` is set.
struct Direct {
    menu: MenuScreen,
    hold: bool,
    /// While set, a filling slot's settling finds its writes pending.
    hold_settle: bool,
}

impl Screen for Direct {
    fn load(
        &mut self,
        chart: &bms_model::Chart,
        section: Option<&eval::Section>,
    ) -> Result<(), RenderError> {
        self.menu.load(chart, section)
    }
    fn decision_rows(&self) -> Option<Result<render::RowPlan, RenderError>> {
        self.menu.decision_rows()
    }
    fn write_decision_labels(&mut self, store: &mut render::LabelStore) -> render::FillStep {
        self.menu.write_decision_labels(store)
    }
    fn settle_decision_labels(
        &mut self,
        store: &mut render::LabelStore,
    ) -> Result<render::Settled, RenderError> {
        if self.hold_settle && store.filling().is_some() {
            return Ok(render::Settled::Pending);
        }
        self.menu.settle_decision_labels(store)
    }
    fn restore_decision_labels(
        &mut self,
        store: &mut render::LabelStore,
        slot: usize,
        staged: render::StagedLabels,
    ) -> Result<(), RenderError> {
        self.menu.restore_decision_labels(store, slot, staged)
    }
    fn watch_completion(&self) -> Arc<AtomicBool> {
        if !self.hold {
            return self.menu.watch_completion();
        }
        let flag = Arc::new(AtomicBool::new(false));
        HELD.with(|h| h.borrow_mut().push(Arc::clone(&flag)));
        flag
    }
    fn device_lost(&self) -> bool {
        self.menu.device_lost()
    }
    fn build_decision(
        &mut self,
        dec: &mut skin_model::DecisionScreen,
        labels: skin_model::DecisionLabels<'_>,
        frame: &skin_model::DecisionFrame<'_>,
    ) -> Result<(), skin_model::BuildError> {
        self.menu.build_decision(dec, labels, frame)
    }
    fn submit_decision(&mut self) -> Result<FrameTiming, RenderError> {
        self.menu.submit_decision()
    }
    fn decision_fault(&mut self, settle: bool) {
        self.menu.decision_fault(settle);
    }
    fn selection(
        &mut self,
        update: &SelectionUpdate,
        logic: Option<&mut crate::selection_skin::SelectionLogic>,
        changed: bool,
        now: TimeUs,
        app: skin_model::AppReadouts,
    ) -> Result<crate::selection_skin::SelectionDrawn, RenderError> {
        self.menu.selection(update, logic, changed, now, app)
    }
    fn wait(&self) -> bool {
        self.menu.wait()
    }
    fn paced(&self) -> bool {
        self.menu.paced()
    }
    fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
        self.menu.build(f)
    }
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        self.menu.draw()
    }
    fn text(&mut self, lines: &[&str]) -> Result<bool, RenderError> {
        self.menu.text(lines)
    }
    fn resize(&mut self, width: u32, height: u32) {
        self.menu.resize(width, height);
    }
    fn target(&self) -> [u32; 2] {
        self.menu.target()
    }
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        self.menu.read_back()
    }
}

/// A test-thread preparation: the screen (with the runtime fonts when
/// `fonts`), the side and its book.
fn side_of(dir: &TempDir, fonts: bool) -> (Direct, DecisionSide, Arc<Mutex<LabelBook>>) {
    let mut r = crate::render_common::offscreen(TARGET[0], TARGET[1]);
    let b = decision_bundle_in(&r, dir.path(), &default_font());
    let token = r.install(ScreenKind::Decision, &b, 1).unwrap();
    r.commit(token).unwrap();
    let fonts = fonts.then(|| render::text::Fonts::runtime().unwrap());
    let screen = Direct {
        menu: MenuScreen::new(r, fonts),
        hold: false,
        hold_settle: false,
    };
    let book = LabelBook::shared(TARGET);
    let side = DecisionSide::new(&b, &book, 1, &screen).unwrap();
    (screen, side, book)
}

/// One boundary, which must be quiet.
fn quiet(screen: &mut Direct, side: &mut DecisionSide, want: Option<&Want>) {
    let b = side.boundary(screen, want);
    assert!(matches!(b, Boundary::Quiet), "{b:?}");
}

/// Boundaries until `f` holds of the book, 1 ms apart, at most 60 s.
fn until(
    screen: &mut Direct,
    side: &mut DecisionSide,
    want: Option<&Want>,
    book: &Mutex<LabelBook>,
    f: impl Fn(&LabelBook) -> bool,
) {
    for _ in 0..60_000 {
        quiet(screen, side, want);
        if f(&lock(book)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("never: {:?}", lock(book));
}

/// O1: the cursor back on a chart whose slot is retired and cooling (its
/// last frame not yet completed) posts no job for it; the cooling slot
/// becomes ready again at its flag.
#[test]
fn return_during_cooling_posts_no_duplicate() {
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let (a, b) = (want("a", "A"), want("b", "B"));
    until(&mut screen, &mut side, a.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    lock(&book)
        .pin(Path::new("a"), labels_revision(&labels("A")), 1)
        .unwrap();
    until(&mut screen, &mut side, b.as_ref(), &book, |k| {
        ready(k, "b", "B")
    });
    // A's lifetime ends; its last frame's completion held.
    lock(&book).end(1);
    screen.hold = true;
    let posts = side.posts;
    for _ in 0..3 {
        quiet(&mut screen, &mut side, a.as_ref());
    }
    assert!(
        (0..2).any(|i| matches!(
            lock(&book).state(i),
            Some(LabelState::Cooling {
                from: render::CoolFrom::Retired,
                ..
            })
        )),
        "{:?}",
        lock(&book)
    );
    assert_eq!(side.posts, posts, "a job posted for the cooling chart");
    screen.hold = false;
    fire_held();
    until(&mut screen, &mut side, a.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    assert_eq!(side.posts, posts);
}

/// R0.2: the cursor back on a chart whose slot is ready posts no job.
#[test]
fn cached_key_posts_no_job() {
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let (a, b) = (want("a", "A"), want("b", "B"));
    until(&mut screen, &mut side, a.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    until(&mut screen, &mut side, b.as_ref(), &book, |k| {
        ready(k, "b", "B")
    });
    let posts = side.posts;
    for _ in 0..3 {
        quiet(&mut screen, &mut side, a.as_ref());
    }
    assert_eq!(side.posts, posts);
    assert!(ready(&lock(&book), "a", "A") && ready(&lock(&book), "b", "B"));
}

/// R0.2: ten cursor moves before one boundary make one job, for the last
/// chart.
#[test]
fn cursor_moves_coalesce_to_the_last_chart() {
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let mut w = None;
    for i in 0..10 {
        update_want(&mut w, &view_on(&format!("c{i}"), &format!("T{i}")));
    }
    quiet(&mut screen, &mut side, w.as_ref());
    assert_eq!(side.posts, 1);
    until(&mut screen, &mut side, w.as_ref(), &book, |k| {
        ready(k, "c9", "T9")
    });
    assert_eq!(side.posts, 1);
}

/// O2: a newer request drops the output waiting for a slot (both slots
/// taken: one pinned, one cooling), so the staging alive stays within one
/// generation's bound.
#[test]
fn newer_request_drops_waiting_output_and_cancels_job() {
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let bound = screen.decision_rows().unwrap().unwrap().staged_bound();
    let (a, b, c, d) = (
        want("a", "A"),
        want("b", "B"),
        want("c", "C"),
        want("d", "D"),
    );
    until(&mut screen, &mut side, a.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    until(&mut screen, &mut side, b.as_ref(), &book, |k| {
        ready(k, "b", "B")
    });
    // B's slot retired and cooling, its completion held; A pinned.
    let rb = labels_revision(&labels("B"));
    let ra = labels_revision(&labels("A"));
    lock(&book).pin(Path::new("b"), rb, 2).unwrap();
    lock(&book).pin(Path::new("a"), ra, 3).unwrap();
    screen.hold = true;
    quiet(&mut screen, &mut side, b.as_ref());
    // C's output waits for a slot.
    let mut c_gen = None;
    for _ in 0..60_000 {
        quiet(&mut screen, &mut side, c.as_ref());
        c_gen = side.store().waiting();
        if c_gen.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let c_gen = c_gen.expect("C's output waiting");
    // D asked: C's waiting output is dropped before D's job stages, so
    // the store stages nothing beside the worker.
    side.check_posts = true;
    quiet(&mut screen, &mut side, d.as_ref());
    side.check_posts = false;
    assert_ne!(side.store().waiting(), Some(c_gen), "C's output kept");
    assert_eq!(side.staged_at_post, [0], "C's staging alive at D's post");
    for _ in 0..60_000 {
        quiet(&mut screen, &mut side, d.as_ref());
        assert!(side.memory().staged <= bound);
        if side.store().waiting().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    screen.hold = false;
    fire_held();
}

/// R1.4 (C12): the choice finds no ready slot for another path, another
/// revision of the same path's labels, after a target change, or after a
/// rebuild; the chart's own key pins.
#[test]
fn labels_not_ready_at_the_choice_per_key_part() {
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let a = want("a", "A");
    let revision = labels_revision(&labels("A"));
    until(&mut screen, &mut side, a.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    assert_eq!(lock(&book).pin(Path::new("b"), revision, 1), None, "path");
    assert_eq!(
        lock(&book).pin(Path::new("a"), revision ^ 1, 1),
        None,
        "revision"
    );
    // Target: the next boundary after a resize frees the slot.
    screen.resize(1_280, 720);
    side.stop(&mut || screen.watch_completion());
    quiet(&mut screen, &mut side, None);
    assert_eq!(lock(&book).pin(Path::new("a"), revision, 1), None, "target");
    screen.resize(TARGET[0], TARGET[1]);
    until(&mut screen, &mut side, a.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    // Rebuild: the installation and the device change together.
    assert!(matches!(side.rebuilt(&mut screen), Rebuilt::Restored));
    assert_eq!(
        lock(&book).pin(Path::new("a"), revision, 1),
        None,
        "rebuild"
    );
    until(&mut screen, &mut side, a.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    assert!(lock(&book).pin(Path::new("a"), revision, 1).is_some());
}

/// R0.1 (C12): each key part's own event re-schedules without a cursor
/// move: a scan replacing the chart under the cursor (same labels, another
/// path), a scan updating its labels (same path), a resize, a rebuild;
/// each makes one new job and a ready slot with the new key.
#[test]
fn key_events_reschedule_without_a_cursor_move() {
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let mut w = None;
    update_want(&mut w, &view_on("a", "A"));
    until(&mut screen, &mut side, w.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    let mut posts = side.posts;
    let mut one_more = |side: &DecisionSide, what: &str| {
        assert_eq!(side.posts, posts + 1, "{what}");
        posts = side.posts;
    };
    update_want(&mut w, &view_on("a2", "A"));
    until(&mut screen, &mut side, w.as_ref(), &book, |k| {
        ready(k, "a2", "A")
    });
    one_more(&side, "path");
    update_want(&mut w, &view_on("a2", "A2"));
    until(&mut screen, &mut side, w.as_ref(), &book, |k| {
        ready(k, "a2", "A2")
    });
    one_more(&side, "revision");
    screen.resize(1_280, 720);
    until(&mut screen, &mut side, w.as_ref(), &book, |k| {
        ready(k, "a2", "A2") && k.current().0 == [1_280, 720]
    });
    one_more(&side, "target");
    assert!(matches!(side.rebuilt(&mut screen), Rebuilt::Restored));
    until(&mut screen, &mut side, w.as_ref(), &book, |k| {
        ready(k, "a2", "A2")
    });
    one_more(&side, "rebuild");
}

/// Q9: a region misfit at installation (three labels at em 256) disables
/// the kind for the run with the notice: no slot pins, and the
/// installation is dropped.
#[test]
fn region_misfit_refuses_the_kind() {
    let dir = decision_folder(
        &[(
            "ttf.ttf",
            crate::font_support::build(&crate::font_support::latin("Skin")),
        )],
        &[ttf()],
        &[
            text("f", 256, Value::Ref(10), [0, 0, 100, 256]),
            text("f", 256, Value::Ref(11), [0, 300, 100, 256]),
            text("f", 256, Value::Ref(16), [0, 600, 100, 256]),
        ],
    );
    let rig = Rig::new(&dir);
    let notice = rig.link.take_decision_refusal().expect("the notice");
    assert!(notice.contains("rows"), "{notice}");
    assert_eq!(rig.seen.removed.load(Ordering::Acquire), 1);
    assert!(!lock(&rig.book).healthy());
    rig.link.exit();
}

/// Q9: a preparation refused before any choice (a refused write)
/// disables the kind with the notice, not as one chart not ready: no slot
/// is ready from then on and the installation goes.
#[test]
fn prep_refused_before_choice_disables_kind_with_notice() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.seen.refuse_settle.store(true, Ordering::Release);
    rig.cursor(1, "a", "A");
    assert!(wait_until(|| rig.seen.removed.load(Ordering::Acquire) > 0));
    assert!(rig.link.take_decision_refusal().is_some(), "no notice");
    assert!(!lock(&rig.book).healthy());
    rig.seen.refuse_settle.store(false, Ordering::Release);
    rig.cursor(2, "b", "B");
    assert!(rig.link.fence(Duration::from_secs(60)));
    assert!(rig.link.fence(Duration::from_secs(60)));
    assert!(!ready(&lock(&rig.book), "b", "B"));
    rig.link.exit();
}

/// A poll failure while a preparation settles is the device's: the screen
/// is built again, and nothing is refused.
#[test]
fn poll_error_during_prep_goes_to_recovery() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.seen.fail_settle.store(true, Ordering::Release);
    rig.cursor(1, "a", "A");
    assert!(wait_until(|| rig.link.failed()));
    assert_eq!(rig.link.take_decision_refusal(), None);
    rig.link.prepare().unwrap();
    assert!(wait_until(|| ready(&lock(&rig.book), "a", "A")));
    assert!(lock(&rig.book).healthy());
    rig.link.exit();
}

/// The skin draws from its pinned slot, its frames' `screen` timer at the
/// choice; on a renderer built again by a recovery it draws again from
/// the same generation, rebuilt on the new device (its title drawn), with
/// the same timers and no new presentation; at the lifetime's end the
/// slot is retired, ready again at its flag, and the next choice pins it.
#[test]
fn pin_survives_a_rebuild_and_retires() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    let shown = rig.drawing("a", "A");
    let sent = rig.link.decisions_sent()();
    let epoch = rig.epoch();
    rig.rebuild().unwrap();
    let from = rig.seen.len();
    rig.skin_frames(from, 3);
    assert!(rig.epoch() > epoch, "the book was not reset");
    rig.seen.probe.store(true, Ordering::Release);
    assert!(wait_until(|| !rig.seen.lit.lock().unwrap().is_empty()));
    let (device, lit) = rig.seen.lit.lock().unwrap()[0];
    assert_eq!(device, 2);
    assert!(lit > 50, "the title drawn on the new device: {lit}");
    let skins: Vec<_> = rig
        .seen
        .drew
        .lock()
        .unwrap()
        .iter()
        .filter_map(|d| match d {
            Drew::Skin {
                device,
                now,
                screen,
            } => Some((*device, *now, *screen)),
            Drew::Page { .. } => None,
        })
        .collect();
    assert!(skins.iter().any(|s| s.0 == 2));
    assert!(
        skins
            .iter()
            .all(|s| s.2 == Some(shown.origin) && s.1 >= shown.origin)
    );
    assert_eq!(rig.link.decisions_sent()(), sent, "a second presentation");
    assert_eq!(rig.link.take_decision_takeover(), None);
    let (slot, generation) = shown.slot.unwrap();
    assert_eq!(
        pinned(&lock(&rig.book)).map(|p| (p.0, p.1)),
        Some((slot, generation))
    );
    // The lifetime ends and selection is back: the slot retired, ready
    // again, and the next choice pins the same generation.
    lock(&rig.book).end(5);
    rig.cursor(2, "a", "A");
    assert!(wait_until(|| ready(&lock(&rig.book), "a", "A")));
    let again = lock(&rig.book)
        .pin(Path::new("a"), labels_revision(&labels("A")), 6)
        .unwrap();
    assert_eq!((again.0, again.1), (slot, generation));
    rig.link.exit();
}

/// R4.5: a chart's load whose device is lost builds the renderer again
/// inside the load: the pinned labels are prepared again there, and the
/// skin draws on.
#[test]
fn decision_rebuild_during_load_reprepares_labels() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.drawing("a", "A");
    let epoch = rig.epoch();
    rig.seen.lose_loads.store(1, Ordering::Release);
    let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(chart, None).unwrap();
    assert!(rig.epoch() > epoch, "the labels were not prepared again");
    let from = rig.seen.len();
    rig.skin_frames(from, 3);
    assert_eq!(rig.seen.builds.load(Ordering::Acquire), 2);
    assert_eq!(rig.link.take_decision_takeover(), None);
    rig.link.exit();
}

/// R4.5: the decision kind's restore refused on a healthy device is a
/// takeover of the active lifetime, with the notice; the page draws.
#[test]
fn decision_restore_refused_on_healthy_device_takes_over() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.drawing("a", "A");
    rig.seen.refuse_restore.store(true, Ordering::Release);
    rig.rebuild().unwrap();
    let t = rig.link.take_decision_takeover().expect("a takeover");
    assert_eq!(t.lifetime, game::DecisionLifetime(5));
    assert!(rig.link.take_decision_refusal().is_some(), "no notice");
    let from = rig.seen.len();
    assert!(wait_until(|| rig.seen.pages(from, "page") >= 3));
    assert_eq!(rig.seen.skins(from), 0);
    rig.link.exit();
}

/// R4.5: the pinned labels' restore failing as the device fails is a
/// recovery failure, not a takeover and not a refusal; the next recovery
/// draws the skin.
#[test]
fn decision_rebuild_poll_failure_is_not_a_takeover() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.drawing("a", "A");
    rig.seen.fail_label_restore.store(true, Ordering::Release);
    assert!(rig.rebuild().is_err());
    assert_eq!(rig.link.take_decision_takeover(), None);
    assert_eq!(rig.link.take_decision_refusal(), None);
    rig.link.prepare().unwrap();
    let from = rig.seen.len();
    rig.skin_frames(from, 3);
    rig.link.exit();
}

/// R4.6: another preparation refused while a lifetime is active (the
/// pinned chart's re-preparation after a target change) disables the
/// kind for later choices but is no takeover: the pinned skin draws to
/// the lifetime's end and on until the render leaves the screen (the
/// target back, so the retired slot keeps its content), and the
/// installation goes then.
#[test]
fn other_prep_refused_during_lifetime_keeps_drawing() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.drawing("a", "A");
    rig.refuse_another();
    let from = rig.seen.len();
    rig.skin_frames(from, 3);
    assert_eq!(rig.link.take_decision_takeover(), None);
    assert_eq!(rig.seen.removed.load(Ordering::Acquire), 0);
    rig.extent.publish(TARGET[0], TARGET[1]);
    assert!(wait_until(|| lock(&rig.book).current().0 == TARGET));
    lock(&rig.book).end(5);
    let from = rig.seen.len();
    rig.skin_frames(from, 3);
    assert_eq!(rig.seen.removed.load(Ordering::Acquire), 0);
    assert_eq!(rig.seen.pages(from, "page"), 0, "the skin cut short");
    rig.link.loading().show(one_line("loading"));
    assert!(wait_until(|| rig.seen.removed.load(Ordering::Acquire) == 1));
    rig.link.exit();
}

/// R4.6: after another preparation's refusal cleared the committed entry,
/// a rebuild while the pinned skin draws finds nothing to restore: a
/// takeover.
#[test]
fn rebuild_under_o8_takes_over() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.drawing("a", "A");
    rig.refuse_another();
    assert_eq!(rig.link.take_decision_takeover(), None);
    rig.rebuild().unwrap();
    let t = rig.link.take_decision_takeover().expect("a takeover");
    assert_eq!(t.lifetime, game::DecisionLifetime(5));
    let from = rig.seen.len();
    assert!(wait_until(|| rig.seen.pages(from, "page") >= 3));
    assert_eq!(rig.seen.skins(from), 0);
    rig.link.exit();
}

/// R4.4, O8: the skin's frame refused for a lifetime no longer active
/// produces no takeover, and the run's notice still stands.
#[test]
fn ignored_takeover_keeps_the_run_notice() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.drawing("a", "A");
    // The game ended the lifetime; the render thread still draws it.
    lock(&rig.book).end(5);
    rig.seen.refuse_frame.store(true, Ordering::Release);
    assert!(wait_until(|| rig.seen.removed.load(Ordering::Acquire) == 1));
    assert_eq!(rig.link.take_decision_takeover(), None);
    assert_eq!(rig.link.shared.takeovers_woken.load(Ordering::Acquire), 0);
    assert!(rig.link.take_decision_refusal().is_some(), "the notice");
    rig.link.exit();
}

/// O6: the skin's frame refused for the active lifetime is a takeover,
/// and the game thread is woken for it; the page draws from then on.
#[test]
fn refused_frame_takes_over_and_wakes_the_game() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    let woken = Arc::new(AtomicU32::new(0));
    let w = Arc::clone(&woken);
    rig.link.set_game_wake(Arc::new(move || {
        w.fetch_add(1, Ordering::AcqRel);
    }));
    rig.drawing("a", "A");
    rig.seen.refuse_frame.store(true, Ordering::Release);
    assert!(wait_until(|| woken.load(Ordering::Acquire) == 1));
    let t = rig.link.take_decision_takeover().expect("a takeover");
    assert_eq!(t.lifetime, game::DecisionLifetime(5));
    assert!(rig.link.take_decision_refusal().is_some());
    let from = rig.seen.len();
    assert!(wait_until(|| rig.seen.pages(from, "page") >= 3));
    assert_eq!(rig.seen.skins(from), 0);
    rig.link.exit();
}

/// R6.1: an attempt's preparation keeps the skinned decision screen.
#[test]
fn decision_skin_survives_attempt_preparation() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    rig.drawing("a", "A");
    let attempt = preset_attempt(&chart, &rules::tapestry(), None, "T");
    assert_eq!(
        rig.link.prepare_attempt(attempt, || false).unwrap(),
        Preparation::Ready
    );
    let from = rig.seen.len();
    rig.skin_frames(from, 3);
    rig.link.exit();
}

/// E3: no skinned decision frame follows the loading state's message,
/// nor is the skin drawn again on a screen built again after it.
#[test]
fn stale_decision_frame_never_follows_loading() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.drawing("a", "A");
    rig.link.loading().show(one_line("loading"));
    assert!(rig.link.fence(Duration::from_secs(60)));
    let from = rig.seen.len();
    assert!(wait_until(|| rig.seen.pages(from, "loading") >= 3));
    rig.rebuild().unwrap();
    assert!(rig.link.fence(Duration::from_secs(60)));
    assert_eq!(rig.link.mode(), "idle");
    assert_eq!(rig.seen.skins(from), 0);
    rig.link.exit();
}

/// R0.4, R6.5: a skinned decision frame's build allocates nothing, after a
/// positive control proving the counter counts.
#[test]
fn decision_frame_alloc() {
    use crate::alloc_count::this_thread;
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let a = want("a", "A");
    until(&mut screen, &mut side, a.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    let (slot, generation, timing) = lock(&book)
        .pin(Path::new("a"), labels_revision(&labels("A")), 1)
        .unwrap();
    let shown = ShownDecision {
        lifetime: 1,
        origin: TimeUs(7_000_000),
        timing,
        slot: Some((slot, generation)),
    };
    let mut dec = side.bundle().skin().prepare_decision();
    let control = this_thread();
    drop(std::hint::black_box(vec![1_u8; 8]));
    assert!(this_thread() > control, "the allocation was not counted");
    let mut frames = [0; 3];
    for (i, n) in (0_i64..).zip(frames.iter_mut()) {
        let now = TimeUs(7_000_000 + i * 1_000_000);
        let before = this_thread();
        let drawn = decision_build(
            &mut screen,
            &mut dec,
            Some(side.store()),
            Some(1),
            &shown,
            now,
        );
        *n = this_thread() - before;
        assert_eq!(drawn, DecisionDrawn::Built);
        screen.submit_decision().unwrap();
    }
    assert_eq!(frames, [0; 3]);
}

/// R0.4, R6.5: the built-in fallback's frame (no slot pinned), a frame
/// whose slot holds another generation and one the skin refuses build
/// nothing and allocate nothing.
#[test]
fn decision_builtin_frame_alloc() {
    use crate::alloc_count::this_thread;
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let a = want("a", "A");
    until(&mut screen, &mut side, a.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    let (slot, generation, timing) = lock(&book)
        .pin(Path::new("a"), labels_revision(&labels("A")), 1)
        .unwrap();
    let skinned = ShownDecision {
        lifetime: 1,
        origin: TimeUs(0),
        timing,
        slot: Some((slot, generation)),
    };
    let builtin = ShownDecision {
        slot: None,
        ..skinned
    };
    let stale = ShownDecision {
        slot: Some((slot, generation + 1)),
        ..skinned
    };
    let mut dec = side.bundle().skin().prepare_decision();
    let control = this_thread();
    drop(std::hint::black_box(vec![1_u8; 8]));
    assert!(this_thread() > control, "the allocation was not counted");
    let mut build = |screen: &mut Direct, shown: &ShownDecision, active: Option<u64>| {
        let before = this_thread();
        let drawn = decision_build(
            screen,
            &mut dec,
            Some(side.store()),
            active,
            shown,
            TimeUs(1),
        );
        (drawn, this_thread() - before)
    };
    assert_eq!(
        build(&mut screen, &builtin, Some(1)),
        (DecisionDrawn::BuiltIn, 0)
    );
    let refused = DecisionDrawn::Refused(skin_model::BuildError::Kind);
    assert_eq!(build(&mut screen, &stale, Some(1)), (refused, 0));
    assert_eq!(build(&mut screen, &stale, None), (DecisionDrawn::Ended, 0));
    screen.decision_fault(false);
    let refused = DecisionDrawn::Refused(skin_model::BuildError::Kind);
    assert_eq!(build(&mut screen, &skinned, Some(1)), (refused, 0));
}

/// R0.4: a selection frame while a decision preparation runs (its job on
/// the worker, then its slot filling) allocates no more than a steady
/// selection frame; the boundary's work is outside the frame.
#[test]
fn selection_frame_during_decision_prep_alloc() {
    use crate::alloc_count::this_thread;
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, true);
    let update = SelectionUpdate {
        lifetime: 1,
        revision: 1,
        list_revision: 1,
        page: one_line("select"),
        view: view_on("a", "A"),
        labels: crate::select::labels(),
        strip: StripText::default(),
        events: SelectionEvents::default(),
        settings: None,
    };
    let frame = |screen: &mut Direct, changed: bool| {
        let before = this_thread();
        screen
            .selection(
                &update,
                None,
                changed,
                TimeUs(1),
                skin_model::AppReadouts::LAUNCH,
            )
            .unwrap();
        this_thread() - before
    };
    frame(&mut screen, true);
    let steady = (0..3).map(|_| frame(&mut screen, false)).max().unwrap();
    let a = want("a", "A");
    let mut during = Vec::new();
    quiet(&mut screen, &mut side, a.as_ref());
    during.push(frame(&mut screen, false));
    for _ in 0..60_000 {
        quiet(&mut screen, &mut side, a.as_ref());
        during.push(frame(&mut screen, false));
        if ready(&lock(&book), "a", "A") {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(ready(&lock(&book), "a", "A"));
    assert!(
        during.iter().all(|&n| n <= steady),
        "steady {steady}, during {during:?}"
    );
}

/// The slot of `book` pinned by `lifetime` at `generation`, its content
/// restored (not restoring).
fn restored_pin(book: &LabelBook, slot: usize, generation: u64, lifetime: u64) -> bool {
    matches!(
        book.state(slot),
        Some(LabelState::Pinned { generation: g, lifetime: l, restoring: false, .. })
            if *g == generation && *l == lifetime
    )
}

/// O3, O4: a rebuild with two pins, one of a lifetime that ended before a
/// boundary retired it and one of the active lifetime: the active pin is
/// the one restored (drawable on the new renderer), and the ended one is
/// freed.
#[test]
fn two_pin_rebuild_restores_the_active_pin() {
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let (a, b) = (want("a", "A"), want("b", "B"));
    until(&mut screen, &mut side, a.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    until(&mut screen, &mut side, b.as_ref(), &book, |k| {
        ready(k, "b", "B")
    });
    let (sa, ..) = lock(&book)
        .pin(Path::new("a"), labels_revision(&labels("A")), 1)
        .unwrap();
    lock(&book).end(1);
    let (sb, gb, _) = lock(&book)
        .pin(Path::new("b"), labels_revision(&labels("B")), 2)
        .unwrap();
    assert!(matches!(side.rebuilt(&mut screen), Rebuilt::Restored));
    let k = lock(&book);
    assert!(
        matches!(k.state(sa), Some(LabelState::Free)),
        "the ended lifetime's pin kept: {k:?}"
    );
    assert!(restored_pin(&k, sb, gb, 2), "{k:?}");
    drop(k);
    assert!(side.store().content(sb, gb).is_some(), "B not restored");
}

/// O1, O2: the cursor back on a ready chart while another chart's job is
/// in flight supersedes that job: it never claims the older ready slot,
/// so both ready charts stay ready.
#[test]
fn return_to_a_cached_chart_supersedes_the_job() {
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let (a, x, b) = (want("a", "A"), want("x", "X"), want("b", "B"));
    until(&mut screen, &mut side, a.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    until(&mut screen, &mut side, x.as_ref(), &book, |k| {
        ready(k, "x", "X")
    });
    let posts = side.posts;
    quiet(&mut screen, &mut side, b.as_ref());
    assert_eq!(side.posts, posts + 1, "B's job posted");
    // Boundaries long enough for B's job, had it gone on, to take a slot.
    for _ in 0..500 {
        quiet(&mut screen, &mut side, a.as_ref());
        if !ready(&lock(&book), "a", "A") {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let k = lock(&book);
    assert!(ready(&k, "a", "A"), "A's slot taken: {k:?}");
    assert!(ready(&k, "x", "X"), "{k:?}");
    assert!(!ready(&k, "b", "B"), "B prepared after the return: {k:?}");
    drop(k);
    assert_eq!(side.posts, posts + 1);
}

/// O2: a newer request while the older one's slot is filling (its writes
/// not settled) cancels the fill: the slot is never published for the
/// old chart, cools as a cancelled one and is freed; the newer one is
/// prepared.
#[test]
fn newer_request_cancels_a_filling_slot() {
    let _turn = crate::warp_turn::take();
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let (c, d) = (want("c", "C"), want("d", "D"));
    screen.hold_settle = true;
    let mut filling = None;
    for _ in 0..60_000 {
        quiet(&mut screen, &mut side, c.as_ref());
        filling = side.store().filling();
        if filling.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let (slot, generation) = filling.expect("C's slot filling");
    quiet(&mut screen, &mut side, d.as_ref());
    assert_ne!(
        side.store().filling(),
        Some((slot, generation)),
        "C's fill kept"
    );
    screen.hold_settle = false;
    for _ in 0..60_000 {
        quiet(&mut screen, &mut side, d.as_ref());
        let k = lock(&book);
        assert!(!ready(&k, "c", "C"), "C published after D was asked: {k:?}");
        if ready(&k, "d", "D") {
            return;
        }
        drop(k);
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("D never ready: {:?}", lock(&book));
}

/// Two label sets of one chart made to share a digest: the second is
/// asked and prepared again (its strings differ), not taken as the first
/// one's ready slot.
#[test]
fn equal_digests_of_other_labels_are_prepared_again() {
    struct Unforce;
    impl Drop for Unforce {
        fn drop(&mut self) {
            crate::decision_side::FORCED_REVISION.set(None);
        }
    }
    let _turn = crate::warp_turn::take();
    crate::decision_side::FORCED_REVISION.set(Some(42));
    let _unforce = Unforce;
    let dir = skin_dir();
    let (mut screen, mut side, book) = side_of(&dir, false);
    let mut w = None;
    update_want(&mut w, &view_on("a", "A"));
    until(&mut screen, &mut side, w.as_ref(), &book, |k| {
        ready(k, "a", "A")
    });
    let first = w.as_ref().unwrap().strings.clone();
    update_want(&mut w, &view_on("a", "B"));
    let second = w.as_ref().unwrap().strings.clone();
    assert_ne!(first, second, "the want kept the first labels");
    let holds = |side: &DecisionSide, strings: &skin_model::DecisionStrings| {
        let k = lock(&book);
        (0..render::LABEL_SLOTS).any(|i| match k.state(i) {
            Some(LabelState::Ready { generation, .. }) => side
                .store()
                .content_of(i, *generation)
                .is_some_and(|c| c.strings() == strings),
            _ => false,
        })
    };
    for _ in 0..60_000 {
        quiet(&mut screen, &mut side, w.as_ref());
        if holds(&side, &second) {
            assert!(!holds(&side, &first), "the first labels still ready");
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("the second labels never prepared: {:?}", lock(&book));
}

/// R4.4, O8: a resized skinned screen whose lifetime ended before the
/// render left it: its retired slot is freed (another target) and its
/// content goes, and the frames draw the page with no refusal, no
/// takeover and no notice.
#[test]
fn ended_resized_decision_draws_the_page_without_refusal() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    let shown = rig.drawing("a", "A");
    let (slot, generation) = shown.slot.unwrap();
    rig.extent.publish(1_280, 720);
    assert!(wait_until(|| lock(&rig.book).current().0 == [1_280, 720]));
    lock(&rig.book).end(5);
    assert!(wait_until(|| {
        lock(&rig.book).state(slot).and_then(LabelState::generation) != Some(generation)
    }));
    let from = rig.seen.len();
    assert!(wait_until(|| rig.seen.pages(from, "page") >= 3));
    assert_eq!(rig.link.take_decision_refusal(), None, "a refusal noted");
    assert_eq!(rig.link.take_decision_takeover(), None);
    assert_eq!(rig.seen.removed.load(Ordering::Acquire), 0);
    assert!(lock(&rig.book).healthy());
    rig.link.exit();
}

/// R4.5, O8: a decision restore refused on a healthy device stands for
/// the run (its notice) when the rest of the rebuild then fails: a
/// chart's load lost again after the refused restore.
#[test]
fn decision_restore_refusal_stands_when_the_rebuild_fails() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.drawing("a", "A");
    rig.seen.refuse_restore.store(true, Ordering::Release);
    rig.seen.lose_loads.store(2, Ordering::Release);
    let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
    assert!(rig.link.load(chart, None).is_err());
    let notice = rig.link.take_decision_refusal().expect("the refusal lost");
    assert!(notice.contains("not restored"), "{notice}");
    rig.link.exit();
}

/// R1.5, R4.6: a choice pinned before another preparation's refusal,
/// its decision message taken after it: the pinned presentation enters
/// on the installation kept for it and draws the skin; no takeover.
#[test]
fn delayed_entry_after_another_refusal_draws_the_pin() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.prepared("a", "A");
    let (slot, generation, timing) = lock(&rig.book)
        .pin(Path::new("a"), labels_revision(&labels("A")), 5)
        .unwrap();
    rig.refuse_another();
    let shown = ShownDecision {
        lifetime: 5,
        origin: rig.link.clock.now(),
        timing,
        slot: Some((slot, generation)),
    };
    let from = rig.seen.len();
    rig.link.leave_selection();
    rig.link.show_decision(one_line("page"), Some(shown));
    rig.skin_frames(from, 3);
    assert_eq!(rig.link.take_decision_takeover(), None);
    rig.link.exit();
}

/// O6, R4.4: a takeover is published under the lock the game ends a
/// lifetime under: held between its lifetime's check and its
/// publication, the book cannot be locked, so the lifetime cannot end
/// between them.
#[test]
fn takeover_published_under_the_lifetime_lock() {
    let dir = skin_dir();
    let rig = Rig::new(&dir);
    rig.drawing("a", "A");
    let gate = &rig.link.shared.takeover_gate;
    gate.hold(true);
    rig.seen.refuse_frame.store(true, Ordering::Release);
    assert!(wait_until(|| gate.waiting()));
    let free = rig.book.try_lock().is_ok();
    gate.hold(false);
    assert!(!free, "the lifetime could end before the publication");
    assert!(wait_until(|| gate.passed() == 1));
    let t = rig.link.take_decision_takeover().expect("a takeover");
    assert_eq!(t.lifetime, game::DecisionLifetime(5));
    rig.link.exit();
}
