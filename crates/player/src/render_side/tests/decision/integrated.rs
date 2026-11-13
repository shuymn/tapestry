//! The decision skin inside the whole run (the 09f design's C14): the
//! full configured set (play, selection, decision and result skins,
//! self-made and kind-correct) built and installed by production's
//! startup, then `game::App::run` on the player's rig, choices made with
//! the select screen's keys and every frame drawn on WARP by a real menu
//! screen that notes what it drew. A frame gate on the render thread
//! times the frames a test reads (the fade marker's pixels, the refused
//! build); everything else is observed as drawn, never classified by hand.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bms_model::{TimeSource as _, TimeUs};
use game::Preparation;
use input::QpcClock;
use render::{FrameTiming, LabelBook, LabelState, RenderError, SkinBundle, lock};
use skin_model::{BuildError, ScreenKind};
use tempfile::TempDir;

use crate::controls::Controls;
use crate::menu_screen::{MenuPage, MenuScreen};
use crate::render_side::tests::wait_until;
use crate::render_side::{EndPage, MakeScreen, Screen};
use crate::select::{MenuInput, MenuKey, MenuQueue};

const MS: i64 = 1_000;

/// The fade marker's box, y down: a white image on timer 2 whose alpha
/// rises from 128 at the fade-out's start to 255 at its end.
const MARK: [i32; 4] = [800, 500, 64, 64];

/// The pixel read inside [`MARK`].
const MARK_AT: (u32, u32) = (832, 532);

/// How long a wait for the render thread or the run lasts at most.
const BOUND: Duration = Duration::from_secs(20);

/// What one frame drew.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Frame {
    /// A select screen's frame, drawn by its skin or as its page.
    Selection { skinned: bool },
    /// A decision skin's frame: its `screen` timer's start (the choice)
    /// and, when probed, the red of the fade marker's pixel.
    Decision {
        screen: Option<TimeUs>,
        mark: Option<u8>,
    },
    /// A menu page (the built-in decision presentation, the practice
    /// page): its first line.
    Page(String),
    /// A play skin's loading frame.
    Loading,
    /// A play frame.
    Play,
    /// A result skin's frame: the skin's own quads.
    Result { quads: usize },
    /// The built-in result page.
    End,
}

/// A frame and when it was drawn: a decision frame's own time, else the
/// clock as the screen drew it.
#[derive(Clone, Debug)]
struct Drawn {
    at: TimeUs,
    frame: Frame,
}

/// The frame gate's state: while `pace`, each frame wait takes one of
/// `permits`, holding (`held`) until there is one.
#[derive(Default)]
struct GateState {
    pace: bool,
    permits: u32,
    waits: u64,
    held: bool,
}

/// The render thread's frames held and let through one at a time.
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits on `changed` until `done`, within [`BOUND`].
    #[track_caller]
    fn until(&self, what: &str, done: impl Fn(&GateState) -> bool) {
        let s = self.lock();
        let (_s, waited) = self
            .changed
            .wait_timeout_while(s, BOUND, |s| !done(s))
            .unwrap_or_else(PoisonError::into_inner);
        assert!(!waited.timed_out(), "the render thread never {what}");
    }

    /// Frames held from the next frame wait on.
    fn hold(&self) {
        self.lock().pace = true;
        self.until("held a frame", |s| s.held);
    }

    /// One frame, then held again at the next wait.
    fn step(&self) {
        let before = {
            let mut s = self.lock();
            s.permits = 1;
            s.waits
        };
        self.changed.notify_all();
        self.until("drew the frame let through", |s| {
            s.waits > before && s.held && s.permits == 0
        });
    }

    /// Frames go free again.
    fn release(&self) {
        self.lock().pace = false;
        self.changed.notify_all();
    }

    /// The render thread's frame wait.
    fn wait(&self) {
        let mut s = self.lock();
        s.waits += 1;
        while s.pace && s.permits == 0 {
            s.held = true;
            self.changed.notify_all();
            s = self.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
        if s.pace {
            s.permits -= 1;
        }
        s.held = false;
        drop(s);
        self.changed.notify_all();
    }

    fn pacing(&self) -> bool {
        self.lock().pace
    }
}

/// What the test sets and reads of the screens a render thread builds.
#[derive(Default)]
struct Seen {
    gate: Gate,
    /// The next decision frame is refused by the renderer.
    refuse: AtomicBool,
    /// The refused decision frame's time.
    refused_at: Mutex<Option<TimeUs>>,
    /// The next decision frame's fade marker is read back.
    probe: AtomicBool,
    drawn: Mutex<Vec<Drawn>>,
    more: Condvar,
}

impl Seen {
    fn push(&self, at: TimeUs, frame: Frame) {
        self.drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Drawn { at, frame });
        self.more.notify_all();
    }

    fn frames(&self) -> Vec<Drawn> {
        self.drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits until the frames drawn so far satisfy `done`, within
    /// [`BOUND`]; the frames then.
    #[track_caller]
    fn until(&self, what: &str, done: impl Fn(&[Drawn]) -> bool) -> Vec<Drawn> {
        let d = self.drawn.lock().unwrap_or_else(PoisonError::into_inner);
        let (d, waited) = self
            .more
            .wait_timeout_while(d, BOUND, |d| !done(d))
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            !waited.timed_out(),
            "never {what}: last frames {:?}",
            d.iter().rev().take(6).collect::<Vec<_>>()
        );
        d.clone()
    }

    fn refused_at(&self) -> Option<TimeUs> {
        *self
            .refused_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A real menu screen on WARP that notes each frame it draws ([`Frame`])
/// and lets [`Seen`]'s gate time its frames.
struct Noted {
    menu: MenuScreen,
    seen: Arc<Seen>,
    clock: QpcClock,
    /// The decision frame built last: its time and `screen` timer.
    decision: Option<(TimeUs, Option<TimeUs>)>,
}

impl Noted {
    fn note(&self, frame: Frame) {
        self.seen.push(self.clock.now(), frame);
    }
}

impl Screen for Noted {
    fn load(
        &mut self,
        chart: &bms_model::Chart,
        section: Option<&eval::Section>,
    ) -> Result<(), RenderError> {
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
    fn publish_chart_images(
        &mut self,
        target: render::ChartTarget,
        images: [Option<&skin_model::chart_image::Fitted>; 2],
    ) -> Result<skin_model::chart_image::ChartTable, RenderError> {
        self.menu.publish_chart_images(target, images)
    }
    fn publish_chart_absent(
        &mut self,
        target: render::ChartTarget,
    ) -> Result<skin_model::chart_image::ChartTable, RenderError> {
        self.menu.publish_chart_absent(target)
    }
    fn invalidate_chart_images(&mut self) {
        self.menu.invalidate_chart_images();
    }
    fn chart_key(&self) -> Option<skin_model::chart_image::ChartKey> {
        self.menu.chart_key()
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
        self.menu.restore(kind, installation)
    }
    fn remove(&mut self, kind: ScreenKind) {
        self.menu.remove(kind);
    }
    fn install_pages(
        &mut self,
        kind: ScreenKind,
        pages: &Arc<skin_model::PackedPages>,
        request: u64,
    ) -> Result<Preparation, RenderError> {
        self.menu.install_pages(kind, pages, request)
    }
    fn restore_pages(
        &mut self,
        kind: ScreenKind,
        pages: &Arc<skin_model::PackedPages>,
    ) -> Result<(), RenderError> {
        self.menu.restore_pages(kind, pages)
    }
    fn disable_result(&mut self) {
        self.menu.disable_result();
    }
    fn drain_retired(&mut self) -> Option<skin_model::AtlasId> {
        self.menu.drain_retired()
    }
    fn installed_pages(&self, kind: ScreenKind) -> Option<Arc<skin_model::PackedPages>> {
        self.menu.installed_pages(kind)
    }
    fn retained_atlases(&self, kind: ScreenKind) -> [Option<skin_model::AtlasId>; 3] {
        self.menu.retained_atlases(kind)
    }
    fn prepare_result(
        &mut self,
        skin: &Arc<SkinBundle>,
        state: &mut skin_model::ResultScreen,
        view: &skin_model::ResultView,
        labels: &skin_model::ResultLabels,
    ) -> Result<Result<(), String>, RenderError> {
        self.menu.prepare_result(skin, state, view, labels)
    }
    fn install_result_text(&mut self) -> bool {
        self.menu.install_result_text()
    }
    fn prepare_result_strip(&mut self, which: usize, lines: &[&str]) -> Result<(), RenderError> {
        self.menu.prepare_result_strip(which, lines)
    }
    fn use_result_strip(&mut self, which: usize) {
        self.menu.use_result_strip(which);
    }
    fn build_result(
        &mut self,
        state: &mut skin_model::ResultScreen,
        frame: &skin_model::ResultFrame<'_>,
    ) -> Result<(), BuildError> {
        self.menu.build_result(state, frame)
    }
    fn submit_result(&mut self) -> Result<FrameTiming, RenderError> {
        let t = self.menu.submit_result()?;
        let quads = self
            .menu
            .result_quads()
            .map_or(0, |q| q.iter().filter(|q| q.layer != 0).count());
        self.note(Frame::Result { quads });
        Ok(t)
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
    ) -> Result<(), BuildError> {
        let refuse = self.seen.refuse.swap(false, Ordering::AcqRel);
        if refuse {
            self.menu.decision_fault(false);
        }
        let built = self.menu.build_decision(dec, labels, frame);
        if refuse && built.is_err() {
            *self
                .seen
                .refused_at
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(frame.now);
        }
        self.decision = Some((frame.now, frame.timers.screen));
        built
    }
    fn submit_decision(&mut self) -> Result<FrameTiming, RenderError> {
        let t = self.menu.submit_decision()?;
        let mark = if self.seen.probe.swap(false, Ordering::AcqRel) {
            let (w, _, pixels) = self.menu.read_back().ok_or(RenderError::DeviceLost)?;
            Some(crate::render_common::rgb_at(&pixels, w, MARK_AT)[0])
        } else {
            None
        };
        let (at, screen) = self.decision.take().unwrap_or((self.clock.now(), None));
        self.seen.push(at, Frame::Decision { screen, mark });
        Ok(t)
    }
    fn decision_fault(&mut self, settle: bool) {
        self.menu.decision_fault(settle);
    }
    fn result_quads(&self) -> Option<Vec<skin_model::Quad>> {
        self.menu.result_quads()
    }
    fn result_strip(&self) -> usize {
        Screen::result_strip(&self.menu)
    }
    fn refuse_next_result(&mut self, text: bool) {
        self.menu.refuse_next_result(text);
    }
    fn wait(&self) -> bool {
        self.seen.gate.wait();
        !self.menu.paced() || self.menu.wait()
    }
    fn paced(&self) -> bool {
        self.seen.gate.pacing() || self.menu.paced()
    }
    fn rearm(&mut self) {
        self.menu.rearm();
    }
    fn carry(&self) -> Option<skin_model::Carry<'_>> {
        self.menu.carry()
    }
    fn build(&mut self, f: &render::FrameInput<'_>) -> Result<(), BuildError> {
        self.menu.build(f)
    }
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        let t = self.menu.draw()?;
        self.note(Frame::Play);
        Ok(t)
    }
    fn text(&mut self, lines: &[&str]) -> Result<bool, RenderError> {
        self.menu.text(lines)
    }
    fn end(&mut self, page: &EndPage) -> Result<bool, RenderError> {
        let p = self.menu.end(page)?;
        self.note(Frame::End);
        Ok(p)
    }
    fn splash(&mut self, elapsed: Duration) -> Option<Result<bool, RenderError>> {
        self.menu.splash(elapsed)
    }
    fn loading(
        &mut self,
        page: &MenuPage,
        update: Option<&crate::loading_state::LoadingUpdate>,
        changed: bool,
        frame: &render::LoadingFrame<'_>,
    ) -> Result<Result<FrameTiming, BuildError>, RenderError> {
        let t = self.menu.loading(page, update, changed, frame)?;
        if t.is_ok() {
            self.note(Frame::Loading);
        }
        Ok(t)
    }
    fn pending_loading(
        &mut self,
        page: &MenuPage,
        changed: bool,
    ) -> Result<FrameTiming, RenderError> {
        let t = self.menu.pending_loading(page, changed)?;
        self.note(Frame::Loading);
        Ok(t)
    }
    fn menu(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        let t = self.menu.menu(page, changed)?;
        let line = page
            .lines
            .first()
            .map_or_else(String::new, |l| l.text.clone());
        self.note(Frame::Page(line));
        Ok(t)
    }
    fn selection(
        &mut self,
        update: &crate::selection_skin::SelectionUpdate,
        logic: Option<&mut crate::selection_skin::SelectionLogic>,
        changed: bool,
        now: TimeUs,
        app: skin_model::AppReadouts,
    ) -> Result<crate::selection_skin::SelectionDrawn, RenderError> {
        let skinned = logic.is_some();
        let drawn = self.menu.selection(update, logic, changed, now, app)?;
        self.note(Frame::Selection {
            skinned: skinned && drawn.refused.is_none(),
        });
        Ok(drawn)
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
    fn minimized(&self) -> bool {
        self.menu.minimized()
    }
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        self.menu.read_back()
    }
}

/// A white `w`×`h` PNG's bytes.
fn white_png(w: u32, h: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut e = png::Encoder::new(&mut bytes, w, h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let data = vec![255_u8; usize::try_from(w * h * 4).unwrap()];
        e.write_header().unwrap().write_image_data(&data).unwrap();
    }
    bytes
}

/// A decision skin, as JSON `s.json` and as Lua `s.luaskin`, declaring
/// scene / input / fade-out 2,500 / 500 / 500 ms: the chart's title (text
/// 10) in a generated Latin face, and the fade marker at [`MARK`].
fn decision_dir() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(
        d.join("ttf.ttf"),
        crate::font_support::build(&crate::font_support::latin("Skin")),
    )
    .unwrap();
    std::fs::write(d.join("m.png"), white_png(8, 8)).unwrap();
    let [x, y, w, h] = MARK;
    // Source y is up from the canvas's bottom.
    let y = 1_080 - y - h;
    let title_y = 1_080 - 100 - 18;
    let json = format!(
        r#"{{"type": 6, "w": 1920, "h": 1080, "scene": 2500, "input": 500, "fadeout": 500,
            "source": [{{"id": "m", "path": "m.png"}}],
            "font": [{{"id": "f", "path": "ttf.ttf"}}],
            "image": [{{"id": "mark", "src": "m", "x": 0, "y": 0, "w": 8, "h": 8}}],
            "text": [{{"id": "t0", "font": "f", "size": 18, "ref": 10}}],
            "destination": [
              {{"id": "t0", "dst": [{{"x": 100, "y": {title_y}, "w": 1200, "h": 18}}]}},
              {{"id": "mark", "timer": 2, "loop": 500,
                "dst": [{{"time": 0, "x": {x}, "y": {y}, "w": {w}, "h": {h}, "a": 128}},
                        {{"time": 500, "x": {x}, "y": {y}, "w": {w}, "h": {h}, "a": 255}}]}}]}}"#
    );
    std::fs::write(d.join("s.json"), json).unwrap();
    let lua = format!(
        "local skin = {{ type = 6, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         skin.scene = 2500 skin.input = 500 skin.fadeout = 500\n\
         skin.source = {{ {{ id = \"m\", path = \"m.png\" }} }}\n\
         skin.font = {{ {{ id = \"f\", path = \"ttf.ttf\" }} }}\n\
         skin.image = {{ {{ id = \"mark\", src = \"m\", x = 0, y = 0, w = 8, h = 8 }} }}\n\
         skin.text = {{ {{ id = \"t0\", font = \"f\", size = 18, ref = 10 }} }}\n\
         skin.destination = {{\n\
         {{ id = \"t0\", dst = {{ {{ x = 100, y = {title_y}, w = 1200, h = 18 }} }} }},\n\
         {{ id = \"mark\", timer = 2, loop = 500, dst = {{\n\
         {{ time = 0, x = {x}, y = {y}, w = {w}, h = {h}, a = 128 }},\n\
         {{ time = 500, x = {x}, y = {y}, w = {w}, h = {h}, a = 255 }} }} }} }}\n\
         return skin\n"
    );
    std::fs::write(d.join("s.luaskin"), lua).unwrap();
    dir
}

/// The full configured set's folders, each a self-made skin of its kind:
/// a Lua play skin, a JSON select screen skin, the decision skin (its
/// `entry`, JSON or Lua) and a Lua result skin with no fact pending.
struct Set {
    play: TempDir,
    selection: TempDir,
    decision: TempDir,
    entry: &'static str,
    result: TempDir,
}

impl Set {
    fn new(entry: &'static str) -> Self {
        Self {
            play: crate::imports::tests::lua_skin(""),
            selection: crate::selection_skin::fixture(),
            decision: decision_dir(),
            entry,
            result: crate::render_side::tests::result::fixed_skin().0,
        }
    }

    fn choices(&self) -> game::SkinChoices {
        use crate::imports::tests::choice;
        game::SkinChoices {
            play: Some(choice(self.play.path(), "play.luaskin")),
            selection: Some(choice(self.selection.path(), "s.json")),
            decision: Some(choice(self.decision.path(), self.entry)),
            result: Some(choice(self.result.path(), "s.luaskin")),
        }
    }
}

/// What a run's helper reaches beside the game thread.
struct Side<'a> {
    seen: &'a Seen,
    menu: &'a MenuQueue,
    book: &'a Mutex<LabelBook>,
    /// The chart under the cursor, as the library lists it.
    chart: PathBuf,
    clock: QpcClock,
}

impl Side<'_> {
    /// Waits until the chart's decision labels are ready in a slot.
    fn prepared(&self) {
        assert!(
            wait_until(|| {
                let book = lock(self.book);
                (0..render::LABEL_SLOTS).any(|i| {
                    matches!(book.state(i), Some(LabelState::Ready { key, .. })
                        if key.chart == self.chart)
                })
            }),
            "never ready: {:?}",
            lock(self.book)
        );
    }

    /// The select screen's key `key` pressed.
    fn press(&self, key: MenuKey) {
        self.menu.push(MenuInput::Key(key));
    }

    /// Waits until the clock reads `t`.
    fn until_clock(&self, t: TimeUs) {
        while self.clock.now().0 < t.0 {
            std::thread::yield_now();
        }
    }

    /// Waits for the first decision skin frame: the choice's time, which
    /// its `screen` timer starts from.
    fn origin(&self) -> TimeUs {
        let frames = self.seen.until("a decision skin frame", |d| {
            d.iter().any(|d| matches!(d.frame, Frame::Decision { .. }))
        });
        frames
            .iter()
            .find_map(|d| match d.frame {
                Frame::Decision { screen, .. } => screen,
                _ => None,
            })
            .expect("the decision's screen timer runs from the choice")
    }
}

/// Closes the window when dropped, so the run ends even when the helper
/// driving it fails.
struct CloseOnDrop<'a>(&'a Controls);

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// What a run gave: how `App::run` ended, the frames drawn and the plays
/// that ended.
struct Ran {
    ran: Result<(), game::LiveError<crate::render_side::ScreenError>>,
    frames: Vec<Drawn>,
    ended: Vec<game::PlayEnd>,
}

/// `set` installed by production's startup on a render thread of
/// [`Noted`] screens, then `game::App::run` on the player's rig, its input
/// thread serving it, with a library of one chart; the App's play preset
/// `preset` when given. `helper` runs beside the run; the window closes
/// when it returns, which ends the run.
fn run(set: &Set, preset: Option<&str>, helper: impl FnOnce(&Side<'_>) + Send) -> Ran {
    use crate::imports::{Build, Launch, startup};
    let _turn = crate::warp_turn::take();
    // The Lua skins' VMs and the startup's jobs, counted by the importer's
    // tests: one run at a time with them.
    let _serial = crate::imports::tests::serial();
    let clock = QpcClock::start();
    let seen = Arc::new(Seen::default());
    let make: MakeScreen = {
        let seen = Arc::clone(&seen);
        Box::new(move || {
            let r = render::Renderer::new(
                render::Target::Offscreen {
                    width: 1_920,
                    height: 1_080,
                },
                render::AdapterChoice::Warp,
            )?;
            Ok(Box::new(Noted {
                menu: MenuScreen::new(r, Some(render::text::Fonts::runtime()?)),
                seen: Arc::clone(&seen),
                clock,
                decision: None,
            }) as Box<dyn Screen>)
        })
    };
    let link = crate::render_side::tests::spawn_empty_on(make, clock);
    let build: Build = Arc::new(crate::imports::build);
    let started = startup(
        &link,
        &set.choices(),
        Launch::Selection,
        false,
        &build,
        || false,
    )
    .expect("the set installs");
    assert_eq!(
        (started.play.as_deref(), started.notice.as_deref()),
        (None, None),
        "every kind installed"
    );
    assert!(started.pending.is_none(), "the play skin is fixed");
    assert!(started.selection.is_some(), "the select screen's song list");
    assert!(
        matches!(
            started.result,
            Some(crate::result_skin::ResultStart::Fixed(..))
        ),
        "the result skin is fixed"
    );
    let book = link.label_book();
    let charts = tempfile::tempdir().unwrap();
    // A 7-key chart (its invisible note on key 6) with one note to play.
    let root = charts.path().join("root0");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("a.bms"),
        "#TITLE a\n#BPM 120\n#00111:01\n#00138:01\n",
    )
    .unwrap();
    let (mut sel, roots) = crate::selection::tests::over(charts.path(), vec![root]);
    assert!(sel.screen.select_path(&roots[0].join("a.bms")));
    let chart = sel.screen.selected().unwrap().path.clone();
    let menu = Arc::clone(&sel.menu);
    sel.started(started.notice, started.selection);
    let settings = game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings::default(),
    };
    let mut app_settings = settings.clone();
    if let Some(p) = preset {
        p.clone_into(&mut app_settings.player.preset);
    }
    let (ran, _) = crate::tests::on_rig_hooked(settings, link, clock, |rig, _, end| {
        use input::{DeviceId, DeviceKind, SourceConfig, SourceSet};
        let crate::tests::InputEnd {
            mut hook,
            waker,
            clock,
        } = end;
        let mut sources = SourceSet::new(SourceConfig {
            max_devices: 2,
            max_sources: 2,
            max_held: 16,
            queue_capacity: 64,
            max_arrivals: 2,
        })
        .unwrap();
        sources.begin_batch(TimeUs(0));
        sources.connect(DeviceId(1), DeviceKind::Keyboard);
        sources.end_batch(TimeUs(0));
        // The window's focus latches the source set's gate, as at startup.
        rig.controls.set_latch(sources.gate().handle());
        // The keyboard read at every wake with nothing pressed, as the raw
        // input thread reads it: its watermark follows the clock, so the
        // play's evaluation advances to its end.
        let thread = input::ClockedThread::spawn(clock, sources, &waker, move |s, now| {
            s.begin_batch(now);
            s.end_batch(now);
            hook.wake(s, now)
        })
        .unwrap();
        // As the song selection launch hands startup's skins to the rig.
        rig.pending = started.pending;
        rig.play_chart = started.play_chart;
        rig.result_skin = crate::result_skin::ResultSkin::new(started.result)
            .with_scripts(rig.render.scripts().clone());
        rig.result_skin.set_wake(rig.controls.game_thread());
        rig.selection = Some(sel);
        let controls = rig.controls;
        let side = Side {
            seen: &seen,
            menu: &menu,
            book: &book,
            chart,
            clock,
        };
        let mut app = game::App::new(
            game::DataRoot::new(charts.path().join("app")),
            app_settings,
            crate::session_config(replay::ReceiveMethod::Scripted, None),
            Box::new(|s: &game::Settings| game::Parts::unsaved(&s.player)),
            Box::new(game::NoSessionEvents),
        );
        let ran = std::thread::scope(|s| {
            let helper = s.spawn(|| {
                let _close = CloseOnDrop(controls);
                let _let_go = LetGo(&seen.gate);
                helper(&side);
            });
            let ran = app.run(rig);
            if let Err(panic) = helper.join() {
                std::panic::resume_unwind(panic);
            }
            ran
        });
        thread.stop().unwrap();
        let ended = rig.ended_plays.iter().map(|(end, _)| *end).collect();
        (ran, ended)
    });
    let (ran, ended) = ran;
    Ran {
        ran,
        frames: seen.frames(),
        ended,
    }
}

/// Lets the frames go when dropped, so a helper that fails leaves no frame
/// held.
struct LetGo<'a>(&'a Gate);

impl Drop for LetGo<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// The index of the first frame at or after `from` that `f` matches.
fn first(frames: &[Drawn], from: usize, f: impl Fn(&Frame) -> bool) -> Option<usize> {
    frames
        .iter()
        .skip(from)
        .position(|d| f(&d.frame))
        .map(|i| i + from)
}

/// C14, P5: with the full configured set installed by production's
/// startup (JSON and Lua decision skins in turn), a chart chosen on the
/// skinned select screen draws the decision skin from the choice; its
/// fade marker, on timer 2 from choice + `scene`, is dark in a frame
/// before 2,500 ms and drawn as the fade defines in one between 2,500
/// and 3,000 ms; then the play loads (loading frames), commits and runs
/// to its end (play frames, a completed play), and with the decision
/// skin resident the result draws skinned: result skin frames, never the
/// built-in result page.
#[test]
fn full_set_draws_the_decision_then_plays_to_a_skinned_result() {
    for entry in ["s.json", "s.luaskin"] {
        let set = Set::new(entry);
        let Ran { ran, frames, ended } = run(&set, None, |side| {
            side.prepared();
            side.seen.gate.hold();
            side.seen.probe.store(true, Ordering::Release);
            side.press(MenuKey::Enter);
            // Frames one at a time until the decision skin's first.
            let mut before = None;
            for _ in 0..50 {
                side.seen.gate.step();
                before = side.seen.frames().into_iter().find_map(|d| match d.frame {
                    Frame::Decision { screen, mark } => Some((d.at, screen, mark)),
                    _ => None,
                });
                if before.is_some() {
                    break;
                }
            }
            let (at, origin, mark) = before.expect("a decision skin frame");
            let origin = origin.expect("the screen timer from the choice");
            let early = at.0 - origin.0;
            assert!((0..2_500 * MS).contains(&early), "{entry}: {early} µs");
            assert_eq!(mark, Some(0), "{entry}: the marker before timer 2");
            // The next frame at 2,500 ms from the choice.
            side.until_clock(TimeUs(origin.0 + 2_500 * MS));
            side.seen.probe.store(true, Ordering::Release);
            side.seen.gate.step();
            let inside = side.seen.frames();
            let Some(Drawn {
                at,
                frame: Frame::Decision { mark, .. },
            }) = inside.last()
            else {
                panic!("{entry}: {:?}", inside.last());
            };
            let late = at.0 - origin.0;
            assert!(
                (2_500 * MS..3_000 * MS).contains(&late),
                "{entry}: {late} µs"
            );
            // Alpha 128 at the fade-out's start, rising to 255 at its end.
            let alpha = 128 + 127 * (late - 2_500 * MS) / (500 * MS);
            let mark = i64::from(mark.expect("probed"));
            assert!(
                (mark - alpha).abs() <= 8,
                "{entry}: marker {mark} at {late} µs, alpha {alpha}"
            );
            side.seen.gate.release();
            side.seen.until("a result skin frame", |d| {
                d.iter()
                    .any(|d| matches!(d.frame, Frame::Result { quads } if quads > 0))
            });
        });
        assert!(ran.is_ok(), "{entry}: {ran:?}");
        assert_eq!(ended, [game::PlayEnd::Completed], "{entry}");
        let skinned = first(&frames, 0, |f| {
            matches!(f, Frame::Selection { skinned: true })
        })
        .expect("the select screen's skin drew");
        let decision = first(&frames, skinned, |f| matches!(f, Frame::Decision { .. }))
            .expect("the decision skin drew");
        let loading = first(&frames, decision, |f| *f == Frame::Loading).expect("loading drew");
        let play = first(&frames, loading, |f| *f == Frame::Play).expect("the play drew");
        let result = first(
            &frames,
            play,
            |f| matches!(f, Frame::Result { quads } if *quads > 0),
        )
        .expect("the result skin drew");
        assert!(
            first(&frames, loading, |f| matches!(f, Frame::Decision { .. })).is_none(),
            "{entry}: a decision frame after loading"
        );
        assert!(
            first(&frames, decision, |f| *f == Frame::End).is_none(),
            "{entry}: the built-in result page drew (the skin's first frame {result})"
        );
    }
}

/// Where a chosen chart goes once its decision screen ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// Played: the loading state.
    Play,
    /// Practised: the practice page, once the chart is read.
    Practice,
    /// Played with a play preset that does not exist: the load is
    /// refused, held to the screen's end, then selection.
    Held,
}

/// R4.1, R4.2 (stage3's list) through the run: the decision skin's build
/// refused by the renderer in a frame let through at 1,000 ms from the
/// choice draws the built-in presentation in that very frame, and its
/// destination (`route`) first draws at 1,500 ms, not before and not at
/// the skin's 3,000 ms; refused at 2,000 ms, past the presentation's
/// time, the destination draws at once, before the skin's 3,000 ms. The
/// built-in page draws in between, and no skin frame after the refusal.
fn refused_build_routes(route: Route) {
    let practice = crate::ui_text::Message::PracticeTitle { name: "a" }.text();
    let destination = |f: &Frame| match route {
        Route::Play => *f == Frame::Loading,
        Route::Practice => *f == Frame::Page(practice.clone()),
        Route::Held => matches!(f, Frame::Selection { .. }),
    };
    for refused_ms in [1_000, 2_000] {
        let set = Set::new("s.json");
        let preset = (route == Route::Held).then_some("missing");
        let mut origin = None;
        let Ran { ran, frames, .. } = run(&set, preset, |side| {
            side.prepared();
            side.press(if route == Route::Practice {
                MenuKey::Practice
            } else {
                MenuKey::Enter
            });
            let o = side.origin();
            // The refused frame is the first one at the time.
            side.seen.gate.hold();
            side.until_clock(TimeUs(o.0 + refused_ms * MS));
            side.seen.refuse.store(true, Ordering::Release);
            side.seen.gate.step();
            side.seen.gate.release();
            let refused = side.seen.refused_at().expect("the build refused");
            side.seen.until("the destination drawn", |d| {
                d.iter()
                    .any(|d| d.at.0 >= refused.0 && destination(&d.frame))
            });
            origin = Some(o);
        });
        let case = format!("{route:?} refused at {refused_ms} ms");
        assert!(ran.is_ok(), "{case}: {ran:?}");
        let origin = origin.expect("the helper ran");
        let at = |ms: i64| origin.0 + ms * MS;
        // The frame the refusal came in drew the built-in page.
        let last = frames
            .iter()
            .rposition(|d| matches!(d.frame, Frame::Decision { .. }))
            .expect("the skin drew");
        let refused = frames.get(last + 1).expect("a frame after the skin's");
        assert!(
            matches!(&refused.frame, Frame::Page(l) if *l != practice),
            "{case}: {refused:?}"
        );
        assert!(
            (at(refused_ms)..at(refused_ms + 500)).contains(&refused.at.0),
            "{case}: refused at {} µs",
            refused.at.0 - origin.0
        );
        let first_decision = first(&frames, 0, |f| matches!(f, Frame::Decision { .. })).unwrap();
        assert!(
            first(&frames, first_decision, destination).is_some_and(|i| i > last),
            "{case}: the destination before the refusal"
        );
        let dest = first(&frames, last + 1, destination).expect("the destination drew");
        assert!(
            frames
                .get(last + 1..dest)
                .unwrap()
                .iter()
                .all(|d| matches!(&d.frame, Frame::Page(l) if *l != practice)),
            "{case}: the built-in page draws until the destination"
        );
        assert!(
            route != Route::Held || !frames.iter().any(|d| d.frame == Frame::Loading),
            "{case}: the refused load's loading state drew"
        );
        let reached = frames.get(dest).unwrap().at.0;
        let earliest = if refused_ms < 1_500 {
            at(1_500)
        } else {
            refused.at.0
        };
        assert!(
            (earliest..at(3_000)).contains(&reached),
            "{case}: reached at {} µs",
            reached - origin.0
        );
    }
}

#[test]
fn refused_build_routes_the_play_path() {
    refused_build_routes(Route::Play);
}

#[test]
fn refused_build_routes_the_practice_path() {
    refused_build_routes(Route::Practice);
}

#[test]
fn refused_build_returns_a_held_refused_load() {
    refused_build_routes(Route::Held);
}
