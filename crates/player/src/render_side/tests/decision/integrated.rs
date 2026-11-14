//! The decision skin inside the whole run (the 09f design's C14): the
//! full configured set (play, selection, decision and result skins,
//! self-made and kind-correct) built and installed by production's
//! startup, then `game::App::run` on the player's rig, choices made with
//! the select screen's keys and every frame drawn on WARP by a real menu
//! screen that notes each frame that reached the target. The run's clock
//! is the test's: held from before the choice, it reads only the times the
//! test sets, so the choice, every timer and every frame drawn meanwhile
//! fall on exact times whatever the threads' scheduling; a frame gate on
//! the render thread lets the frames a test reads (the fade marker's
//! pixels, the refused build) through one at a time at those times.
//! Everything else is observed as drawn, never classified by hand. Wall
//! clock limits only detect a hang.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bms_model::{TimeSource as _, TimeUs};
use game::Preparation;
use input::QpcClock;
use render::{FrameTiming, LabelBook, LabelState, RenderError, SkinBundle, lock};
use skin_model::{AtlasId, BuildError, ScreenKind};
use tempfile::TempDir;

use crate::controls::Controls;
use crate::measure::{DecisionDrawnAs, DecisionFallback, DecisionRecord};

use super::{Decided, DecidedSink, RigSink};
use crate::menu_screen::{MenuPage, MenuScreen};
use crate::render_side::tests::wait_until;
use crate::render_side::{EndPage, MakeScreen, Screen, TakeoverAcks};
use crate::select::{MenuInput, MenuKey, MenuQueue};

const MS: i64 = 1_000;

/// The fade marker's box, y down: a white image on timer 2 whose alpha
/// rises from 128 at the fade-out's start to 255 at its end.
const MARK: [i32; 4] = [800, 500, 64, 64];

/// The pixel read inside [`MARK`].
const MARK_AT: (u32, u32) = (832, 532);

/// The result fixture's marker, a gray (9, 9, 9) box at x 1,600, y 780
/// up from the bottom, 64 × 64 (Lua destinations): drawn only by the
/// result skin.
const RESULT_MARK: &str = "{ id = \"dot\", dst = { { x = 1600, y = 780, w = 64, h = 64 } } },";

/// The pixel read inside [`RESULT_MARK`], y down.
const RESULT_MARK_AT: (u32, u32) = (1_632, 268);

/// How long a wait for the render thread or the run lasts at most: hang
/// detection only, never part of a claim.
const BOUND: Duration = Duration::from_secs(20);

/// What one frame drew, or the play's commit.
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
    /// A staged installation committed (not a frame).
    Commit,
    /// A result skin's frame: the skin's own quads and, in the first one,
    /// the color of [`RESULT_MARK_AT`].
    Result { quads: usize, mark: Option<[u8; 3]> },
    /// The built-in result page.
    End,
}

/// A frame that reached the target, and when it was drawn: a decision
/// frame's own time, else the clock as the screen drew it; and the
/// decision installation's atlas resident on the GPU then.
#[derive(Clone, Debug)]
struct Drawn {
    at: TimeUs,
    frame: Frame,
    decision: Option<AtlasId>,
}

/// The frame gate's state: while `pace`, each frame wait takes one of
/// `permits`, holding (`held`) until there is one.
#[derive(Default)]
struct GateState {
    pace: bool,
    permits: u32,
    waits: u64,
    held: bool,
    /// Every frame wait answers that no frame is due, after a park of at
    /// most a millisecond: nothing is drawn, and the render thread goes on
    /// taking its messages.
    suppress: bool,
    /// The waits answered so while suppressed.
    refused: u64,
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

    /// One frame let through, the gate held, until `done` holds.
    #[track_caller]
    fn step_into(&self, done: impl Fn() -> bool) {
        self.lock().permits = 1;
        self.changed.notify_all();
        assert!(wait_until(done), "the frame let through never got there");
    }

    /// Frames go free again.
    fn release(&self) {
        self.lock().pace = false;
        self.changed.notify_all();
    }

    /// No frame is drawn from now (`true`) while the render thread keeps
    /// taking its messages: every frame wait, a held one woken included,
    /// answers that none is due, and the screen counts as paced, so the
    /// clock's pacing never draws either. Turned on, it waits until one
    /// wait was answered so after the call: the render thread asks for a
    /// wait only without a frame granted, so none granted before survives.
    fn suppress(&self, on: bool) {
        let before = {
            let mut s = self.lock();
            s.suppress = on;
            s.refused
        };
        self.changed.notify_all();
        if on {
            self.until("answered a frame wait with none due", |s| {
                s.refused > before
            });
        }
    }

    /// From suppressed to held, in one step: the render thread parks at
    /// its next frame wait and takes no message until let go, no frame
    /// drawn in between.
    fn block(&self) {
        {
            let mut s = self.lock();
            s.pace = true;
            s.permits = 0;
            s.suppress = false;
        }
        self.changed.notify_all();
        self.until("held a frame", |s| s.held);
    }

    /// The render thread's frame wait: whether a frame is due.
    fn wait(&self) -> bool {
        let mut s = self.lock();
        s.waits += 1;
        while s.pace && s.permits == 0 && !s.suppress {
            s.held = true;
            self.changed.notify_all();
            s = self.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
        if s.suppress {
            s.held = false;
            // A short park, so the render thread's loop does not spin.
            let (mut s, _) = self
                .changed
                .wait_timeout(s, Duration::from_millis(1))
                .unwrap_or_else(PoisonError::into_inner);
            s.refused += 1;
            drop(s);
            self.changed.notify_all();
            return false;
        }
        if s.pace {
            s.permits -= 1;
        }
        s.held = false;
        drop(s);
        self.changed.notify_all();
        true
    }

    /// Whether the frames are paced by the gate: held or suppressed, the
    /// clock's pacing never draws.
    fn pacing(&self) -> bool {
        let s = self.lock();
        s.pace || s.suppress
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
    /// A chart's load on the render thread waits here while held.
    load_gate: crate::render_side::TestGate,
    drawn: Mutex<Vec<Drawn>>,
    more: Condvar,
}

impl Seen {
    fn push(&self, drawn: Drawn) {
        self.drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(drawn);
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

/// A real menu screen on WARP that notes each frame that reached the
/// target ([`Frame`]) and lets [`Seen`]'s gate time its frames.
struct Noted {
    menu: MenuScreen,
    seen: Arc<Seen>,
    clock: QpcClock,
    /// The decision frame built last: its time and `screen` timer.
    decision: Option<(TimeUs, Option<TimeUs>)>,
    /// A result skin frame reached the target, its marker read.
    result_read: bool,
}

impl Noted {
    /// The decision installation's atlas resident on the GPU.
    fn resident(&self) -> Option<AtlasId> {
        let [committed, ..] = self.menu.retained_atlases(ScreenKind::Decision);
        committed
    }

    fn note_at(&self, at: TimeUs, frame: Frame) {
        self.seen.push(Drawn {
            at,
            frame,
            decision: self.resident(),
        });
    }

    /// `frame`, when `presented`.
    fn note(&self, presented: bool, frame: Frame) {
        if presented {
            self.note_at(self.clock.now(), frame);
        }
    }
}

impl Screen for Noted {
    fn load(
        &mut self,
        chart: &bms_model::Chart,
        section: Option<&eval::Section>,
    ) -> Result<(), RenderError> {
        self.seen.load_gate.pass();
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
        self.menu.commit(token)?;
        self.note(true, Frame::Commit);
        Ok(())
    }
    fn discard(&mut self) {
        self.menu.discard();
    }
    fn publish_chart_images(
        &mut self,
        target: render::ChartTarget,
        images: [Option<&skin_model::chart_image::Fitted>; 3],
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
        // The first one to reach the target has its marker read back.
        let mark = if t.presented && !self.result_read {
            self.result_read = true;
            let (w, _, pixels) = self.menu.read_back().ok_or(RenderError::DeviceLost)?;
            Some(crate::render_common::rgb_at(&pixels, w, RESULT_MARK_AT))
        } else {
            None
        };
        self.note(t.presented, Frame::Result { quads, mark });
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
        let (at, screen) = self.decision.take().unwrap_or((self.clock.now(), None));
        if !t.presented {
            return Ok(t);
        }
        let mark = if self.seen.probe.swap(false, Ordering::AcqRel) {
            let (w, _, pixels) = self.menu.read_back().ok_or(RenderError::DeviceLost)?;
            Some(crate::render_common::rgb_at(&pixels, w, MARK_AT)[0])
        } else {
            None
        };
        self.note_at(at, Frame::Decision { screen, mark });
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
        if !self.seen.gate.wait() {
            return false;
        }
        !self.menu.paced() || self.menu.wait()
    }
    fn allocate_in_next_decision(&mut self, refuse: bool) {
        self.menu.allocate_in_next_decision(refuse);
    }
    fn paced(&self) -> bool {
        self.seen.gate.pacing() || self.menu.paced()
    }
    fn rearm(&mut self) {
        self.menu.rearm();
    }
    fn build(&mut self, f: &render::FrameInput<'_>) -> Result<(), BuildError> {
        self.menu.build(f)
    }
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        let t = self.menu.draw()?;
        self.note(t.presented, Frame::Play);
        Ok(t)
    }
    fn text(&mut self, lines: &[&str]) -> Result<bool, RenderError> {
        self.menu.text(lines)
    }
    fn end(&mut self, page: &EndPage) -> Result<bool, RenderError> {
        let p = self.menu.end(page)?;
        self.note(p, Frame::End);
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
        if let Ok(t) = &t {
            self.note(t.presented, Frame::Loading);
        }
        Ok(t)
    }
    fn pending_loading(
        &mut self,
        page: &MenuPage,
        changed: bool,
    ) -> Result<FrameTiming, RenderError> {
        let t = self.menu.pending_loading(page, changed)?;
        self.note(t.presented, Frame::Loading);
        Ok(t)
    }
    fn menu(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        let t = self.menu.menu(page, changed)?;
        let line = page
            .lines
            .first()
            .map_or_else(String::new, |l| l.text.clone());
        self.note(t.presented, Frame::Page(line));
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
        let drawn = self.menu.selection(update, logic, changed, now, app)?;
        self.note(
            drawn.timing.presented,
            Frame::Selection {
                skinned: drawn.skinned,
            },
        );
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
/// `entry`, JSON or Lua) and a Lua result skin with no fact pending, its
/// marker at [`RESULT_MARK`].
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
            result: crate::render_side::tests::result::lua_skin_scripted("", RESULT_MARK).0,
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
    /// The game thread's controls: the decision screen its waits watch.
    controls: &'a Controls,
    menu: &'a MenuQueue,
    book: &'a Mutex<LabelBook>,
    /// The chart under the cursor, as the library lists it.
    chart: PathBuf,
    /// The run's clock, which the helper holds and moves on.
    clock: QpcClock,
    /// The game thread's looks for the decision skin's takeover and the
    /// render thread's drained frame loops.
    acks: Arc<TakeoverAcks>,
    /// The decision faults fired so far.
    fired: Box<dyn Fn() -> u64 + Send + Sync>,
    /// The decision labels' work at a frame boundary waits here.
    label_gate: Arc<crate::render_side::TestGate>,
    /// A scenario's choice waits here, its labels found ready.
    ready_gate: Arc<crate::render_side::TestGate>,
    /// The decision labels' readiness asked so far.
    ready_checks: Box<dyn Fn() -> u64 + Send + Sync>,
    /// The decision screens sent so far.
    decisions_sent: Box<dyn Fn() -> u64 + Send + Sync>,
    /// A refused decision frame waits here, its present sampled.
    cleanup_gate: Arc<crate::render_side::TestGate>,
    /// The decision screens the render thread left so far.
    decided: Arc<Decided>,
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

    /// The clock held from now: the time every thread reads until the
    /// helper moves it.
    fn hold_clock(&self) -> TimeUs {
        self.clock.hold()
    }

    /// The chart chosen (`key`) on the held clock, which stays at the
    /// choice's time until the first decision skin frame is drawn, at that
    /// time: the frame (its time, `screen` timer and probe).
    fn choose(&self, key: MenuKey) -> (TimeUs, Option<TimeUs>, Option<u8>) {
        let chosen = self.hold_clock();
        self.press(key);
        let frames = self.seen.until("a decision skin frame", |d| {
            d.iter().any(|d| matches!(d.frame, Frame::Decision { .. }))
        });
        let (at, screen, mark) = frames
            .iter()
            .find_map(|d| match d.frame {
                Frame::Decision { screen, mark } => Some((d.at, screen, mark)),
                _ => None,
            })
            .expect("a decision skin frame");
        assert_eq!(
            at, chosen,
            "the first decision skin frame on the held clock"
        );
        (at, screen, mark)
    }

    /// The takeover of a build refused at `refused_t`, before the
    /// destination is due, processed with the clock held there: the game
    /// thread took it and looked again, so the look that took it and what
    /// that look routed are over; then frames are let through until one
    /// from a frame loop that drained the render thread's messages after
    /// that look has drawn, so it drew whatever the look sent, and each
    /// drew the built-in page. No frame since the refusal (`from`) drew
    /// the destination.
    #[track_caller]
    fn taken_over(
        &self,
        case: &str,
        from: usize,
        refused_t: TimeUs,
        destination: &dyn Fn(&Frame) -> bool,
    ) {
        let early = |what: &str| {
            let frames = self.seen.frames();
            assert!(
                !frames.iter().skip(from).any(|d| destination(&d.frame)),
                "{case}: the destination drawn {what}, the clock held at the refusal: {:?}",
                frames.get(from..).unwrap()
            );
        };
        let start = Instant::now();
        while !self.acks.looked_after() {
            early("before the game looked again");
            assert!(
                start.elapsed() < BOUND,
                "{case}: the game never looked again after taking the takeover: last frames {:?}",
                self.seen.frames().iter().rev().take(6).collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        early("as the game looked again");
        let looked = self.acks.drained();
        self.seen.gate.hold();
        let start = Instant::now();
        loop {
            // The frame held now is drawn by a loop that drained after
            // the look.
            let consumed = self.acks.drained() > looked;
            let (_, drawn) = self.step_until("a frame after the look", |_, d| !d.is_empty());
            assert!(
                drawn
                    .iter()
                    .all(|d| d.at == refused_t && matches!(d.frame, Frame::Page(_))),
                "{case}: the frame after the game's look drew {drawn:?}"
            );
            if consumed {
                break;
            }
            assert!(
                start.elapsed() < BOUND,
                "{case}: the render thread never drained its messages after the look"
            );
        }
        early("after the game's look");
        assert_eq!(self.clock.now(), refused_t, "{case}: the clock held");
        self.seen.gate.release();
    }

    /// Lets frames through one at a time, the gate held, until `done`
    /// holds of the seen state and the frames the last one drew: those
    /// frames, and the index of the first.
    #[track_caller]
    fn step_until(
        &self,
        what: &str,
        done: impl Fn(&Seen, &[Drawn]) -> bool,
    ) -> (usize, Vec<Drawn>) {
        let start = Instant::now();
        loop {
            // Nothing is drawn while the render thread is held.
            let from = self.seen.frames().len();
            self.seen.gate.step();
            let drawn = self.seen.frames().split_off(from);
            if done(self.seen, &drawn) {
                return (from, drawn);
            }
            assert!(
                start.elapsed() < BOUND,
                "never {what}: last frames {drawn:?}"
            );
        }
    }
}

/// Lets the run's clock run when dropped, so a helper that fails leaves
/// the run's threads on a running clock to end on.
struct RunClock(QpcClock);

impl Drop for RunClock {
    fn drop(&mut self) {
        self.0.run();
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
/// that ended; each decision screen the render thread left, as its record
/// says, and each presented decision frame's stamp.
struct Ran {
    ran: Result<(), game::LiveError<crate::render_side::ScreenError>>,
    frames: Vec<Drawn>,
    ended: Vec<game::PlayEnd>,
    records: Vec<DecisionRecord>,
    stamps: Vec<(u64, DecisionDrawnAs)>,
    /// Why the select screen's script failed, if it did.
    failure: Option<String>,
    /// Each saved play's figures and its measure file's text, when the
    /// run saves its plays.
    measured: Vec<(crate::measure::Figures, String)>,
}

/// What a run starts with beyond its set: the decision faults the first
/// decision screen takes, the select screen's script, whether the
/// decision labels' work is held at its gate from the start, whether the
/// plays are saved with their measure files, as `measure play` saves
/// them, and a scenario's captures.
#[derive(Default)]
struct Opts {
    faults: Option<crate::dev::DecisionFaults>,
    script: Option<crate::dev::SelectScript>,
    hold_labels: bool,
    /// A scenario's captures, written by its frame sink, which the run's
    /// choices bind.
    captures: Option<crate::dev::scenario::Captures>,
    saved: bool,
    /// A scenario file's steps (TOML `[[step]]` bodies, its first a
    /// `select` or `practice`), loaded by production's loader in place of
    /// `script`: the select screen runs its first step through the choice.
    scenario: Option<&'static [&'static str]>,
    /// A scenario's choice held at its ready gate from the start.
    hold_ready: bool,
}

/// `set` installed by production's startup on a render thread of
/// [`Noted`] screens, then `game::App::run` on the player's rig, its input
/// thread serving it, with a library of one chart; the App's play preset
/// `preset` when given. `helper` runs beside the run; the window closes
/// when it returns, which ends the run.
fn run(set: &Set, preset: Option<&str>, helper: impl FnOnce(&Side<'_>) + Send) -> Ran {
    run_with(set, preset, Opts::default(), helper)
}

/// [`run`] with `opts`.
fn run_with(
    set: &Set,
    preset: Option<&str>,
    opts: Opts,
    helper: impl FnOnce(&Side<'_>) + Send,
) -> Ran {
    use crate::imports::{Build, Launch, startup};
    let _turn = crate::warp_turn::take();
    // The Lua skins' VMs and the startup's jobs, counted by the importer's
    // tests: one run at a time with them.
    let _serial = crate::imports::tests::serial();
    // Every thread of the run reads this clock; the helper holds it.
    let clock = QpcClock::holdable();
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
                result_read: false,
            }) as Box<dyn Screen>)
        })
    };
    let decided = Arc::new(Decided::default());
    let link = crate::render_side::tests::spawn_on_with(
        make,
        clock,
        Box::new(RigSink {
            decided: DecidedSink(Arc::clone(&decided)),
            frames: opts
                .captures
                .clone()
                .map(crate::dev::scenario::ScenarioFrames::new),
        }),
    );
    let cleanup_gate = link.decision_cleanup_gate();
    if let Some(f) = opts.faults {
        link.set_decision_faults(f);
    }
    let fired = link.decision_faults_fired();
    let (label_gate, ready_gate) = (link.label_gate(), link.ready_gate());
    let (ready_checks, decisions_sent) = (link.ready_checks(), link.decisions_sent());
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
    let acks = link.takeover_acks();
    // Held after the set installed: the labels for the cursor's chart are
    // asked once the select screen shows.
    if opts.hold_labels {
        label_gate.hold(true);
    }
    if opts.hold_ready {
        ready_gate.hold(true);
    }
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
    sel.script = opts.script;
    if let Some(steps) = opts.scenario {
        sel.script = Some(loaded(charts.path(), steps));
    }
    let saved = opts.saved;
    let measured: crate::play::MeasuredPlays = Arc::default();
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
        rig.scenario = opts.captures.map(crate::selection::tests::capture_state);
        if saved {
            rig.measured_plays = Some(Arc::clone(&measured));
        }
        let controls = rig.controls;
        let side = Side {
            seen: &seen,
            controls,
            menu: &menu,
            book: &book,
            chart,
            clock,
            acks,
            fired: Box::new(fired),
            label_gate: Arc::clone(&label_gate),
            ready_gate: Arc::clone(&ready_gate),
            ready_checks: Box::new(ready_checks),
            decisions_sent: Box::new(decisions_sent),
            cleanup_gate: Arc::clone(&cleanup_gate),
            decided: Arc::clone(&decided),
        };
        let root = game::DataRoot::new(charts.path().join("app"));
        let parts = root.clone();
        let mut app = game::App::new(
            root,
            app_settings,
            crate::session_config(replay::ReceiveMethod::Scripted, None),
            Box::new(move |s: &game::Settings| {
                if saved {
                    game::Parts::standard(&parts, &s.player)
                } else {
                    game::Parts::unsaved(&s.player)
                }
            }),
            Box::new(game::NoSessionEvents),
        );
        let ran = std::thread::scope(|s| {
            let helper = s.spawn(|| {
                let _close = CloseOnDrop(controls);
                let _let_go = LetGo(&seen.gate);
                let _labels = LetLabelsGo(&label_gate, &ready_gate);
                let _run = RunClock(clock);
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
        let failure = rig
            .selection
            .as_ref()
            .and_then(|s| s.script.as_ref())
            .and_then(|s| s.failure().map(str::to_owned));
        (ran, ended, failure)
    });
    let (ran, ended, failure) = ran;
    // Read while the run's data folder stands.
    let measured = measured
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(path, fig)| {
            let text = std::fs::read_to_string(crate::measure::measure_path(path))
                .expect("the measure file beside the saved record");
            (fig.clone(), text)
        })
        .collect();
    let records = decided
        .records
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let stamps = decided
        .stamps
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    Ran {
        ran,
        frames: seen.frames(),
        ended,
        records,
        stamps,
        failure,
        measured,
    }
}

/// Lets the label and ready gates go when dropped, so a helper that fails
/// leaves neither held.
struct LetLabelsGo<'a>(
    &'a crate::render_side::TestGate,
    &'a crate::render_side::TestGate,
);

impl Drop for LetLabelsGo<'_> {
    fn drop(&mut self) {
        self.0.hold(false);
        self.1.hold(false);
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
/// fade marker, on timer 2 from choice + `scene`, is dark in the frame
/// drawn at the choice and drawn as the fade defines in frames let through
/// at exactly 2,500 and 2,750 ms on the held clock; then the play loads
/// (loading frames), commits and runs to its end (play frames, a completed
/// play), and the result draws skinned: result skin frames, the first one
/// reaching the target with the skin's marker in its pixels, never the
/// built-in result page. The decision installation's atlas stays resident
/// on the GPU from its first frame through the play's commit to that
/// result frame.
#[test]
fn full_set_draws_the_decision_then_plays_to_a_skinned_result() {
    for entry in ["s.json", "s.luaskin"] {
        let set = Set::new(entry);
        let Ran {
            ran, frames, ended, ..
        } = run(&set, None, |side| {
            side.prepared();
            side.seen.probe.store(true, Ordering::Release);
            let (chosen, origin, mark) = side.choose(MenuKey::Enter);
            assert_eq!(
                origin,
                Some(chosen),
                "{entry}: the screen timer from the choice"
            );
            assert_eq!(mark, Some(0), "{entry}: the marker before timer 2");
            // One frame let through at 2,500 ms from the choice, as timer
            // 2 starts, and one at 2,750 ms, halfway through the fade-out.
            side.seen.gate.hold();
            for ms in [2_500, 2_750] {
                let t = TimeUs(chosen.0 + ms * MS);
                side.clock.set(t);
                side.seen.probe.store(true, Ordering::Release);
                let (_, drawn) = side
                    .step_until(&format!("a decision frame at {ms} ms"), |_, d| {
                        d.iter().any(|d| matches!(d.frame, Frame::Decision { .. }))
                    });
                let Some(Drawn {
                    at,
                    frame: Frame::Decision { mark, .. },
                    ..
                }) = drawn
                    .iter()
                    .find(|d| matches!(d.frame, Frame::Decision { .. }))
                else {
                    unreachable!()
                };
                assert_eq!(*at, t, "{entry}: the frame let through at {ms} ms");
                // Alpha 128 at the fade-out's start, rising to 255 at its
                // end.
                let alpha = 128 + 127 * (ms - 2_500) / 500;
                let mark = i64::from(mark.expect("probed"));
                assert!(
                    (mark - alpha).abs() <= 1,
                    "{entry}: marker {mark} at {ms} ms, alpha {alpha}"
                );
            }
            side.clock.run();
            side.seen.gate.release();
            side.seen
                .until("a skinned result frame reaching the target", |d| {
                    d.iter().any(
                        |d| matches!(d.frame, Frame::Result { quads, mark: Some(_) } if quads > 0),
                    )
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
        let commit = first(&frames, decision, |f| *f == Frame::Commit)
            .filter(|&i| i < play)
            .expect("the play committed before its first frame");
        let result = first(
            &frames,
            play,
            |f| matches!(f, Frame::Result { quads, .. } if *quads > 0),
        )
        .expect("the result skin drew");
        let Frame::Result { mark, .. } = &frames[result].frame else {
            unreachable!()
        };
        assert_eq!(
            *mark,
            Some([9, 9, 9]),
            "{entry}: the result skin's marker in its first frame"
        );
        assert!(
            first(&frames, loading, |f| matches!(f, Frame::Decision { .. })).is_none(),
            "{entry}: a decision frame after loading"
        );
        assert!(
            first(&frames, decision, |f| *f == Frame::End).is_none(),
            "{entry}: the built-in result page drew (the skin's first frame {result})"
        );
        // P5: the decision installation resident from its first frame
        // through the play's commit to the result's first frame.
        let resident = frames[decision]
            .decision
            .expect("the decision installation's atlas at its first frame");
        for (i, d) in frames.iter().enumerate().take(result + 1).skip(decision) {
            assert_eq!(
                d.decision,
                Some(resident),
                "{entry}: the decision installation not resident at {i} (commit {commit}, \
                 result {result}): {d:?}"
            );
        }
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

/// R4.1, R4.2 (stage3's list) through the run, on the held clock: the
/// decision skin's build refused by the renderer in the frame let through
/// at exactly 1,000 ms from the choice draws the built-in presentation in
/// that very frame; with the clock still held there, the game takes the
/// takeover and looks again, and the frames drawn after that are still
/// the built-in page ([`Side::taken_over`]); only then does the clock
/// move, and the destination (`route`) first draws at exactly 1,500 ms.
/// Refused at exactly 2,000 ms, past the presentation's time, the
/// destination draws at once, at the refusal's own time. The clock moves
/// only when the test moves it, so each destination is due at a time the
/// run cannot pass before drawing it, and the skin's own 3,000 ms never
/// comes. The built-in page draws in between, and no skin frame after the
/// refusal.
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
        let case = format!("{route:?} refused at {refused_ms} ms");
        let mut seen = None;
        let Ran { ran, frames, .. } = run(&set, preset, |side| {
            side.prepared();
            let (chosen, ..) = side.choose(if route == Route::Practice {
                MenuKey::Practice
            } else {
                MenuKey::Enter
            });
            // The refused frame is the first one at the time.
            side.seen.gate.hold();
            let refused_t = TimeUs(chosen.0 + refused_ms * MS);
            side.clock.set(refused_t);
            side.seen.refuse.store(true, Ordering::Release);
            let (from, drawn) =
                side.step_until("the build refused", |s, _| s.refused_at().is_some());
            let refused_at = side.seen.refused_at().expect("the build refused");
            assert_eq!(refused_at, refused_t, "{case}: the refusal's time");
            // That frame itself drew the built-in page, before any other.
            assert!(
                matches!(
                    drawn.as_slice(),
                    [Drawn { at, frame: Frame::Page(l), .. }] if *at == refused_t && *l != practice
                ),
                "{case}: the refused frame drew {drawn:?}"
            );
            side.seen.gate.release();
            // At 1,500 ms from the choice, or at once past it: the clock
            // stays there until the destination draws.
            let due = if refused_ms < 1_500 {
                side.taken_over(&case, from, refused_t, &destination);
                TimeUs(chosen.0 + 1_500 * MS)
            } else {
                refused_at
            };
            side.clock.set(due);
            side.seen.until("the destination drawn", |d| {
                d.iter().skip(from).any(|d| destination(&d.frame))
            });
            seen = Some((from, due));
        });
        assert!(ran.is_ok(), "{case}: {ran:?}");
        let (refused, due) = seen.expect("the helper ran");
        assert!(
            first(&frames, refused, |f| matches!(f, Frame::Decision { .. })).is_none(),
            "{case}: a skin frame after the refusal"
        );
        let first_decision = first(&frames, 0, |f| matches!(f, Frame::Decision { .. })).unwrap();
        assert!(
            first(&frames, first_decision, destination).is_some_and(|i| i > refused),
            "{case}: the destination before the refusal"
        );
        let dest = first(&frames, refused, destination).expect("the destination drew");
        assert!(
            frames
                .get(refused..dest)
                .unwrap()
                .iter()
                .all(|d| matches!(&d.frame, Frame::Page(l) if *l != practice)),
            "{case}: the built-in page draws until the destination"
        );
        assert!(
            route != Route::Held || !frames.iter().any(|d| d.frame == Frame::Loading),
            "{case}: the refused load's loading state drew"
        );
        assert_eq!(
            frames.get(dest).unwrap().at,
            due,
            "{case}: the destination's first frame"
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

/// The decision skin frames drawn so far.
fn skin_frames(frames: &[Drawn]) -> usize {
    frames
        .iter()
        .filter(|d| matches!(d.frame, Frame::Decision { .. }))
        .count()
}

impl Side<'_> {
    /// With the gate held, lets frames through one at a time, the clock
    /// moved 50 ms on before each, until `done` holds of the frames drawn
    /// so far: the clock's time then.
    #[track_caller]
    fn frames_until(&self, chosen: TimeUs, what: &str, done: impl Fn(&[Drawn]) -> bool) -> TimeUs {
        self.seen.gate.hold();
        let mut t = chosen;
        for k in 1..=40 {
            if done(&self.seen.frames()) {
                return t;
            }
            t = TimeUs(chosen.0 + k * 50 * MS);
            self.clock.set(t);
            self.step_until(what, |_, d| !d.is_empty());
        }
        panic!(
            "never {what}: {:?}",
            self.seen.frames().iter().rev().take(6).collect::<Vec<_>>()
        );
    }

    /// The decision screens the render thread left so far.
    fn decided_count(&self) -> usize {
        self.decided.records().len()
    }

    /// The decision screen left for the loading state: the clock let run
    /// and the frames let go until the loading state draws.
    fn to_loading(&self) {
        self.clock.run();
        self.seen.gate.release();
        self.seen.until("the loading state", |d| {
            d.iter().any(|d| d.frame == Frame::Loading)
        });
    }
}

/// The one decision screen a run left, and its frames' stamps.
#[track_caller]
fn one_record(ran: &Ran) -> (DecisionRecord, Vec<DecisionDrawnAs>) {
    assert!(ran.ran.is_ok(), "{:?}", ran.ran);
    let [r] = ran.records[..] else {
        panic!("one decision screen: {:?}", ran.records);
    };
    let stamps = ran
        .stamps
        .iter()
        .filter(|(l, _)| *l == r.lifetime)
        .map(|&(_, s)| s)
        .collect();
    (r, stamps)
}

/// A decision skin's screen, its frames let through at times the test
/// sets: its builds, counted as an app region of their own, allocate
/// nothing, and its record says the skin drew, built and presented, with
/// no allocation (design.md "Scope of No Allocation").
#[test]
fn a_decision_skin_screen_allocates_nothing() {
    let set = Set::new("s.luaskin");
    let ran = run(&set, None, |side| {
        side.prepared();
        let (chosen, ..) = side.choose(MenuKey::Enter);
        side.frames_until(chosen, "three decision skin frames", |f| {
            skin_frames(f) >= 3
        });
        side.to_loading();
    });
    let (r, stamps) = one_record(&ran);
    assert!(r.skin_allocates_nothing(), "{r:?}");
    assert!(r.builds >= 3 && r.skin_frames >= 3, "{r:?}");
    assert_eq!(r.presentation(), "skin");
    assert!(
        stamps.iter().all(|s| *s == DecisionDrawnAs::Skin),
        "{stamps:?}"
    );
    assert_eq!(stamps.len() as u64, r.skin_frames);
}

/// The third decision skin build made to allocate inside the skin's own
/// build: the allocation is counted, once, and the fault fired. The play
/// the screen led, saved, carries that record: its decision count is the
/// allocation and its own frames' count holds none of it, in its figures
/// and in its measure file.
#[test]
fn a_later_decision_build_made_to_allocate_is_counted() {
    let set = Set::new("s.luaskin");
    let opts = Opts {
        faults: Some(crate::dev::DecisionFaults::parse("alloc3")),
        saved: true,
        ..Opts::default()
    };
    let ran = run_with(&set, None, opts, |side| {
        side.prepared();
        let (chosen, ..) = side.choose(MenuKey::Enter);
        side.frames_until(chosen, "four decision skin frames", |f| skin_frames(f) >= 4);
        assert_eq!((side.fired)(), 1, "the fault fired");
        side.to_loading();
        side.seen.until("the result skin", |d| {
            d.iter()
                .any(|d| matches!(d.frame, Frame::Result { quads, .. } if quads > 0))
        });
    });
    let (r, _) = one_record(&ran);
    assert_eq!(r.app, 1, "{r:?}");
    assert!(r.builds >= 4, "{r:?}");
    assert_eq!(r.presentation(), "skin");
    assert!(!r.skin_allocates_nothing());
    let [(fig, text)] = &ran.measured[..] else {
        panic!("one saved play: {:?}", ran.measured);
    };
    assert_eq!(
        fig.decision,
        Some(r),
        "the play carries the screen's record"
    );
    assert_eq!(fig.render_app_allocations, Some(0), "{fig:?}");
    for line in ["decision_app = 1", "render_app = 0"] {
        assert!(text.lines().any(|l| l == line), "{line}: {text}");
    }
}

/// The second build made to allocate inside the skin's build, then
/// refused: its allocation counts, exactly once (the refusal's report and
/// takeover outside the region); the screen fell back, the refused frame
/// itself and every later one drawn as the fallback, never as the
/// built-in presentation chosen.
#[test]
fn a_refused_allocating_build_is_counted() {
    let set = Set::new("s.luaskin");
    let opts = Opts {
        faults: Some(crate::dev::DecisionFaults::parse("allocrefuse2")),
        ..Opts::default()
    };
    let ran = run_with(&set, None, opts, |side| {
        side.prepared();
        let (chosen, ..) = side.choose(MenuKey::Enter);
        side.frames_until(chosen, "the refused build's page", |f| {
            f.iter().any(|d| matches!(d.frame, Frame::Page(_)))
        });
        assert_eq!((side.fired)(), 1, "the fault fired");
        side.to_loading();
    });
    let (r, stamps) = one_record(&ran);
    assert_eq!(r.app, 1, "{r:?}");
    assert_eq!(r.builds, 2, "{r:?}");
    assert_eq!(r.fell_back, Some(DecisionFallback::Refused));
    assert_eq!(r.presentation(), "fallback:refused");
    assert_eq!(stamps.first(), Some(&DecisionDrawnAs::Skin), "{stamps:?}");
    assert!(stamps.len() >= 2, "{stamps:?}");
    assert!(
        stamps
            .iter()
            .skip(1)
            .all(|s| *s == DecisionDrawnAs::Fallback),
        "{stamps:?}"
    );
}

/// The second skin frame made to allocate before its submission, outside
/// the counted build: nothing is counted (the region's negative control).
#[test]
fn a_submission_allocation_is_not_counted() {
    let set = Set::new("s.luaskin");
    let opts = Opts {
        faults: Some(crate::dev::DecisionFaults::parse("submitalloc2")),
        ..Opts::default()
    };
    let ran = run_with(&set, None, opts, |side| {
        side.prepared();
        let (chosen, ..) = side.choose(MenuKey::Enter);
        side.frames_until(chosen, "three decision skin frames", |f| {
            skin_frames(f) >= 3
        });
        assert_eq!((side.fired)(), 1, "the fault fired");
        side.to_loading();
    });
    let (r, _) = one_record(&ran);
    assert!(r.skin_allocates_nothing(), "{r:?}");
}

/// A build made to allocate before a lost frame and one after the screen
/// was built again both count, in one record the recovery kept; the frames
/// after it are the skin's again.
#[test]
fn allocations_before_and_after_recovery_both_count() {
    let set = Set::new("s.luaskin");
    let opts = Opts {
        faults: Some(crate::dev::DecisionFaults::parse("alloc2,lost3,alloc5")),
        ..Opts::default()
    };
    let ran = run_with(&set, None, opts, |side| {
        side.prepared();
        let (chosen, ..) = side.choose(MenuKey::Enter);
        side.frames_until(chosen, "skin frames after the recovery", |f| {
            skin_frames(f) >= 5
        });
        assert_eq!((side.fired)(), 3, "every fault fired");
        side.to_loading();
    });
    let (r, stamps) = one_record(&ran);
    assert_eq!(r.app, 2, "{r:?}");
    assert!(r.builds >= 5, "{r:?}");
    assert_eq!(r.presentation(), "skin");
    assert!(
        stamps.iter().all(|s| *s == DecisionDrawnAs::Skin),
        "{stamps:?}"
    );
}

/// The origin of a select step, for a script made of compiled steps.
fn select_origin() -> crate::dev::scenario::Origin {
    crate::dev::scenario::Origin {
        index: 1,
        kind: crate::dev::scenario::StepKind::Select,
    }
}

/// A scenario file of `steps` written in the run's folder `dir` (its
/// library `root0`, an input log of no events beside it), loaded and
/// compiled by production's loader: its script through the first step's
/// choice (Enter, or P), what it waits for before it included. The steps
/// after (the play, the practice's rounds) run on screens the rig does
/// not script.
fn loaded(dir: &std::path::Path, steps: &[&str]) -> crate::dev::SelectScript {
    use crate::dev::SelectStep;
    crate::dev::scenario::tests::write_log(dir);
    let mut text = "version = 8\n[library]\nroots = [\"root0\"]\n".to_owned();
    for s in steps {
        text.push_str("[[step]]\n");
        text.push_str(s);
        text.push('\n');
    }
    let file = dir.join("scenario.toml");
    std::fs::write(&file, text).unwrap();
    let mut script = crate::dev::scenario::load(&file)
        .expect("the scenario loads")
        .script;
    let mut first = Vec::new();
    while let (Some(step), Some(origin)) = (script.front().cloned(), script.origin()) {
        script.done();
        let chose = matches!(step, SelectStep::Enter | SelectStep::PracticeKey(_));
        first.push((step, origin));
        if chose {
            break;
        }
    }
    assert!(
        first.iter().all(|(_, o)| o.index == 1)
            && first.iter().any(|(s, _)| *s == SelectStep::DecisionReady),
        "{first:?}"
    );
    crate::dev::SelectScript::compiled(first)
}

/// A `select` with `decision = "skin"` on the rig's chart, then its play.
const SELECT: &[&str] = &[
    "select = { path = \"root0/a.bms\", decision = \"skin\" }",
    "play = { input = \"auto\", until = \"end\" }",
    "quit = {}",
];

/// A `practice` with `decision = "skin"` on the rig's chart.
const PRACTICE: &[&str] = &[
    "practice = { path = \"root0/a.bms\", from = 1, to = 1, speed = 100, \
     lead_in_ms = 2500, gauge = 20, repeat = true, input = \"log.tprl\", rounds = 1, \
     decision = \"skin\" }",
    "quit = {}",
];

/// A scenario's choice with `decision = "skin"` (its file loaded by
/// production's loader, run by the select screen's script on the rig),
/// its labels' work held at its gate: the script asks the readiness again
/// and again and sends no decision screen; let go, the labels are
/// prepared and the choice pins them, the skin chosen. For a `select`
/// (Enter) and a `practice` (P).
#[test]
fn a_scenario_choice_waits_for_its_labels() {
    for (what, steps) in [("select", SELECT), ("practice", PRACTICE)] {
        let set = Set::new("s.luaskin");
        let opts = Opts {
            scenario: Some(steps),
            hold_labels: true,
            ..Opts::default()
        };
        let ran = run_with(&set, None, opts, |side| {
            assert!(
                wait_until(|| side.label_gate.waiting()),
                "{what}: the labels' work held"
            );
            let from = (side.ready_checks)();
            assert!(
                wait_until(|| (side.ready_checks)() >= from + 3),
                "{what}: asked again"
            );
            assert_eq!(
                (side.decisions_sent)(),
                0,
                "{what}: chosen before its labels"
            );
            side.label_gate.hold(false);
            side.seen.until("a decision skin frame", |d| {
                d.iter().any(|d| matches!(d.frame, Frame::Decision { .. }))
            });
            assert_eq!((side.decisions_sent)(), 1, "{what}");
        });
        assert!(ran.failure.is_none(), "{what}: {:?}", ran.failure);
        let r = ran.records.first().expect("the decision screen left");
        assert!(r.skin, "{what}: {r:?}");
    }
}

/// The labels found ready, then gone before the choice (the book marked
/// unhealthy while the script waits at its ready gate, held before the
/// run starts): the choice pins nothing, and the script fails naming the
/// scenario's step. For a `select` and a `practice`.
#[test]
fn a_pin_lost_after_readiness_fails_its_step() {
    for (what, steps) in [("select", SELECT), ("practice", PRACTICE)] {
        let set = Set::new("s.luaskin");
        let opts = Opts {
            scenario: Some(steps),
            hold_ready: true,
            ..Opts::default()
        };
        let ran = run_with(&set, None, opts, |side| {
            assert!(
                wait_until(|| side.ready_gate.waiting()),
                "{what}: found ready"
            );
            lock(side.book).set_unhealthy();
            side.ready_gate.hold(false);
            assert!(
                wait_until(|| (side.decisions_sent)() == 1),
                "{what}: chosen"
            );
        });
        let failure = ran.failure.expect("the script failed");
        assert!(
            failure.contains("the decision skin's labels were not pinned at the choice"),
            "{what}: {failure}"
        );
        assert!(
            failure.contains(&format!("step 1 ({what})")),
            "{what}: {failure}"
        );
        assert!(
            ran.records.iter().all(|r| !r.skin),
            "{what}: {:?}",
            ran.records
        );
    }
}

/// A choice without `decision = "skin"` does not wait: its labels' work
/// held, it is chosen at once, the built-in presentation drawn, and the
/// script does not fail.
#[test]
fn a_choice_without_the_option_does_not_wait() {
    let set = Set::new("s.luaskin");
    let opts = Opts {
        script: Some(crate::dev::SelectScript::compiled(vec![(
            crate::dev::SelectStep::Enter,
            select_origin(),
        )])),
        hold_labels: true,
        ..Opts::default()
    };
    let ran = run_with(&set, None, opts, |side| {
        assert!(
            wait_until(|| (side.decisions_sent)() == 1),
            "chosen at once"
        );
        side.label_gate.hold(false);
    });
    assert!(ran.failure.is_none(), "{:?}", ran.failure);
    let r = ran.records.first().expect("the decision screen left");
    assert!(!r.skin, "{r:?}");
}

/// Captures of the decision axis at each of `ms`, written under `dir`,
/// none armed.
fn decision_captures(dir: &std::path::Path, ms: &[i64]) -> crate::dev::scenario::Captures {
    let axes: Vec<_> = ms
        .iter()
        .map(|&m| (crate::dev::scenario::Axis::Decision(TimeUs(m * MS)), false))
        .collect();
    crate::dev::scenario::tests::screen_captures(dir, &axes)
}

/// Capture `k`'s state.
fn state_of(c: &crate::dev::scenario::Captures, k: u16) -> crate::dev::scenario::CaptureState {
    c.status(k).expect("a capture").state
}

/// Capture `k` written: its sidecar.
#[track_caller]
fn written(c: &crate::dev::scenario::Captures, dir: &std::path::Path, k: u16) -> String {
    assert!(
        wait_until(|| state_of(c, k) == crate::dev::scenario::CaptureState::Succeeded),
        "{:?}",
        state_of(c, k)
    );
    std::fs::read_to_string(dir.join(format!("{k}.json"))).unwrap()
}

/// A number field of a sidecar.
#[track_caller]
fn field(side: &str, key: &str) -> String {
    let rest = side
        .split(&format!("\"{key}\":"))
        .nth(1)
        .unwrap_or_else(|| panic!("{key} in {side}"));
    rest.split([',', '}']).next().unwrap().to_owned()
}

/// Capture `k` failed, and why.
#[track_caller]
fn failed(c: &crate::dev::scenario::Captures, k: u16) -> String {
    assert!(
        wait_until(|| matches!(
            state_of(c, k),
            crate::dev::scenario::CaptureState::Failed(_)
        )),
        "{:?}",
        state_of(c, k)
    );
    match state_of(c, k) {
        crate::dev::scenario::CaptureState::Failed(why) => why,
        _ => unreachable!(),
    }
}

/// Capture `k` failed already: the render thread drew `next`, so it took
/// the message that left the decision screen, and the screen's departure
/// came before that frame.
#[track_caller]
fn settled_by_now(c: &crate::dev::scenario::Captures, k: u16, next: &str) {
    assert!(
        matches!(
            state_of(c, k),
            crate::dev::scenario::CaptureState::Failed(_)
        ),
        "not failed when {next} drew: {:?}",
        state_of(c, k)
    );
}

/// A decision skin frame at 300 ms from the choice is captured: the frame
/// let through at 100 ms is not taken, the one at 300 ms is, its sidecar
/// naming the skin, the screen's lifetime and the time since the choice.
#[test]
fn a_decision_skin_frame_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let c = decision_captures(dir.path(), &[300]);
    c.arm(0);
    let set = Set::new("s.luaskin");
    let opts = Opts {
        captures: Some(c.clone()),
        ..Opts::default()
    };
    let mut chosen_at = None;
    let ran = run_with(&set, None, opts, |side| {
        side.prepared();
        let (chosen, ..) = side.choose(MenuKey::Enter);
        chosen_at = Some(chosen);
        side.seen.gate.hold();
        side.clock.set(TimeUs(chosen.0 + 100 * MS));
        side.step_until("a frame at 100 ms", |_, d| !d.is_empty());
        assert!(
            matches!(
                state_of(&c, 0),
                crate::dev::scenario::CaptureState::Deciding { .. }
            ),
            "{:?}",
            state_of(&c, 0)
        );
        side.clock.set(TimeUs(chosen.0 + 300 * MS));
        side.step_until("a frame at 300 ms", |_, d| !d.is_empty());
        side.to_loading();
    });
    assert!(ran.ran.is_ok(), "{:?}", ran.ran);
    let side = written(&c, dir.path(), 0);
    let chosen = chosen_at.unwrap();
    assert_eq!(field(&side, "decision_presentation"), "\"skin\"", "{side}");
    assert_eq!(field(&side, "decision_us"), "300000", "{side}");
    assert_eq!(
        field(&side, "decision_origin_us"),
        chosen.0.to_string(),
        "{side}"
    );
    assert_eq!(
        field(&side, "presented_us"),
        (chosen.0 + 300 * MS).to_string(),
        "{side}"
    );
    assert_eq!(field(&side, "screen"), "\"decision\"", "{side}");
    let r = ran.records.first().expect("the screen left");
    assert_eq!(
        field(&side, "decision_lifetime"),
        r.lifetime.to_string(),
        "{side}"
    );
}

/// The built-in presentation's frame is captured the same way: its labels
/// held unprepared, the choice draws the built-in presentation.
#[test]
fn a_built_in_decision_frame_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let c = decision_captures(dir.path(), &[0]);
    c.arm(0);
    let set = Set::new("s.luaskin");
    let opts = Opts {
        captures: Some(c.clone()),
        hold_labels: true,
        script: Some(crate::dev::SelectScript::compiled(vec![(
            crate::dev::SelectStep::Enter,
            select_origin(),
        )])),
        ..Opts::default()
    };
    let ran = run_with(&set, None, opts, |side| {
        assert!(wait_until(|| (side.decisions_sent)() == 1), "chosen");
        // Chosen with no labels ready: the built-in presentation for this
        // screen, whatever is prepared after.
        side.label_gate.hold(false);
        side.seen.until("a decision page", |d| {
            d.iter().any(|d| matches!(d.frame, Frame::Page(_)))
        });
    });
    assert!(ran.ran.is_ok(), "{:?}", ran.ran);
    let side = written(&c, dir.path(), 0);
    assert_eq!(
        field(&side, "decision_presentation"),
        "\"built-in\"",
        "{side}"
    );
}

/// A capture past the decision screen's time fails when the render thread
/// leaves the screen for the loading state, with frames drawn before it:
/// failed once a loading frame is drawn, before the run ends.
#[test]
fn a_decision_capture_past_its_screen_fails() {
    let dir = tempfile::tempdir().unwrap();
    let c = decision_captures(dir.path(), &[600_000]);
    c.arm(0);
    let set = Set::new("s.luaskin");
    let opts = Opts {
        captures: Some(c.clone()),
        ..Opts::default()
    };
    let ran = run_with(&set, None, opts, |side| {
        side.prepared();
        side.choose(MenuKey::Enter);
        side.to_loading();
        settled_by_now(&c, 0, "the loading state");
    });
    assert!(ran.ran.is_ok(), "{:?}", ran.ran);
    let why = failed(&c, 0);
    assert!(
        why.contains("its decision screen ended before a frame at 600000 ms"),
        "{why}"
    );
}

/// The present a capture is judged by is the one sampled right after the
/// frame's draw, before a refusal's report and takeover: the refused frame
/// drawn at T - 1 µs, held after its draw while the clock moves to T, is
/// not taken by a capture at T; the next frame, at T + 1 ms, is, drawn as
/// the fallback.
#[test]
fn the_present_time_is_sampled_before_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let t_ms = 400;
    let c = decision_captures(dir.path(), &[t_ms]);
    c.arm(0);
    let set = Set::new("s.luaskin");
    let opts = Opts {
        captures: Some(c.clone()),
        ..Opts::default()
    };
    let ran = run_with(&set, None, opts, |side| {
        side.prepared();
        let (chosen, ..) = side.choose(MenuKey::Enter);
        let t = TimeUs(chosen.0 + t_ms * MS);
        side.seen.gate.hold();
        side.cleanup_gate.hold(true);
        side.clock.set(TimeUs(t.0 - 1));
        side.seen.refuse.store(true, Ordering::Release);
        // Let one frame through: the refused one, parked after its draw.
        side.seen.gate.step_into(|| side.cleanup_gate.waiting());
        side.clock.set(t);
        side.cleanup_gate.hold(false);
        assert!(wait_until(|| side.seen.gate.lock().held));
        assert!(
            matches!(
                state_of(&c, 0),
                crate::dev::scenario::CaptureState::Deciding { .. }
            ),
            "the refused frame taken: {:?}",
            state_of(&c, 0)
        );
        side.clock.set(TimeUs(t.0 + MS));
        side.step_until("a frame after the refusal", |_, d| !d.is_empty());
        side.to_loading();
    });
    assert!(ran.ran.is_ok(), "{:?}", ran.ran);
    let side = written(&c, dir.path(), 0);
    assert_eq!(
        field(&side, "decision_presentation"),
        "\"fallback\"",
        "{side}"
    );
    assert_eq!(
        field(&side, "decision_us"),
        ((t_ms + 1) * MS).to_string(),
        "{side}"
    );
}

/// A decision screen that presents no frame at all still settles its
/// capture when the render thread leaves it. Drawing is suppressed from
/// before the choice (no frame granted survives), the render thread still
/// taking its messages; the real choice binds the capture and the real
/// departure fails it: (a) at the loading state's reveal, past the
/// deadline; (b) at the return to selection after a refused load.
#[test]
fn zero_presented_frames_fail_at_departure() {
    for refused in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let c = decision_captures(dir.path(), &[0]);
        c.arm(0);
        let set = Set::new("s.luaskin");
        let opts = Opts {
            captures: Some(c.clone()),
            ..Opts::default()
        };
        let preset = refused.then_some("missing");
        let ran = run_with(&set, preset, opts, |side| {
            side.prepared();
            side.seen.gate.suppress(true);
            let chosen = side.hold_clock();
            side.press(MenuKey::Enter);
            assert!(
                wait_until(|| matches!(
                    state_of(&c, 0),
                    crate::dev::scenario::CaptureState::Deciding { .. }
                )),
                "bound at the choice: {:?}",
                state_of(&c, 0)
            );
            side.clock.set(TimeUs(chosen.0 + 4_000 * MS));
            let _ = failed(&c, 0);
            side.clock.run();
            side.seen.gate.suppress(false);
        });
        assert!(ran.ran.is_ok(), "{refused}: {:?}", ran.ran);
        let why = failed(&c, 0);
        assert!(
            why.contains("its decision screen ended"),
            "{refused}: {why}"
        );
        let r = ran.records.first().expect("the screen left");
        assert_eq!(
            (r.skin_frames, r.page_frames, r.ended_frames),
            (0, 0, 0),
            "{refused}: {r:?}"
        );
        assert!(
            ran.stamps.iter().all(|(l, _)| *l != r.lifetime),
            "{refused}: {:?}",
            ran.stamps
        );
    }
}

/// Two choices in quick succession, each with its own capture: the first
/// screen's departure fails only its own capture. The render thread is
/// held from taking any message after the first choice (drawing
/// suppressed, so none of either screen presents); the game returns to
/// selection after the first load is refused, and the second choice binds
/// the second capture while the render thread still draws the first
/// screen. Let go, the first screen's departure fails the first capture
/// and leaves the second pending; the second fails at its own departure.
#[test]
fn a_departure_fails_only_its_own_screen_s_capture() {
    let dir = tempfile::tempdir().unwrap();
    let c = decision_captures(dir.path(), &[0, 0]);
    c.arm(0);
    let set = Set::new("s.luaskin");
    let opts = Opts {
        captures: Some(c.clone()),
        ..Opts::default()
    };
    let deciding = |k: u16| {
        matches!(
            state_of(&c, k),
            crate::dev::scenario::CaptureState::Deciding { .. }
        )
    };
    let ran = run_with(&set, Some("missing"), opts, |side| {
        side.prepared();
        side.seen.gate.suppress(true);
        let chosen = side.hold_clock();
        side.press(MenuKey::Enter);
        assert!(wait_until(|| deciding(0)), "the first bound");
        // The second capture armed once the first is bound.
        c.arm(1);
        // From here the render thread takes no message.
        side.seen.gate.block();
        let first = side.decided_count();
        side.clock.set(TimeUs(chosen.0 + 4_000 * MS));
        // The second choice, once the game is back on selection: Enter is
        // dropped until then, so it is pressed again until a second
        // decision screen is sent.
        let start = Instant::now();
        while (side.decisions_sent)() < 2 {
            assert!(start.elapsed() < BOUND, "never chose again");
            side.press(MenuKey::Enter);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            wait_until(|| deciding(1)),
            "the second bound: {:?}",
            state_of(&c, 1)
        );
        assert_eq!(side.decided_count(), first, "a screen left while held");
        side.seen.gate.suppress(true);
        // Let go: the first screen is left, then the second shown.
        assert!(
            wait_until(|| side.decided_count() > first),
            "the first screen left"
        );
        assert!(
            matches!(
                state_of(&c, 0),
                crate::dev::scenario::CaptureState::Failed(_)
            ),
            "{:?}",
            state_of(&c, 0)
        );
        assert!(
            deciding(1),
            "the second failed by the first departure: {:?}",
            state_of(&c, 1)
        );
        side.clock.set(TimeUs(chosen.0 + 8_000 * MS));
        let _ = failed(&c, 1);
        side.clock.run();
        side.seen.gate.suppress(false);
    });
    assert!(ran.ran.is_ok(), "{:?}", ran.ran);
    // The two screens presented nothing (an Enter pressed again may have
    // made a later choice after them).
    let two = ran.records.get(..2).expect("both screens left");
    for r in two {
        assert_eq!((r.skin_frames, r.page_frames), (0, 0), "{r:?}");
        assert!(
            ran.stamps.iter().all(|(l, _)| *l != r.lifetime),
            "{:?}",
            ran.stamps
        );
    }
}

/// The deadline reached while the game thread waits for the chart's load
/// (the render thread held inside it): the wait's own look steps the
/// screen and reveals the loading state, and the render thread's
/// departure, when it takes that message after the load, fails the
/// capture; before it, the capture stands.
#[test]
fn a_deadline_inside_a_watched_wait_departs() {
    let dir = tempfile::tempdir().unwrap();
    let c = decision_captures(dir.path(), &[600_000]);
    c.arm(0);
    let set = Set::new("s.luaskin");
    let opts = Opts {
        captures: Some(c.clone()),
        ..Opts::default()
    };
    let ran = run_with(&set, None, opts, |side| {
        side.prepared();
        side.seen.load_gate.hold(true);
        let chosen = side.hold_clock();
        side.press(MenuKey::Enter);
        assert!(
            wait_until(|| side.seen.load_gate.waiting()),
            "the chart's load never reached the render thread"
        );
        let left = side.decided_count();
        side.clock.set(TimeUs(chosen.0 + 4_000 * MS));
        assert!(
            wait_until(|| side
                .controls
                .with_watched(|d| d.is_some_and(game::Decision::stepped))),
            "the deadline not stepped inside the load's wait"
        );
        assert!(
            matches!(
                state_of(&c, 0),
                crate::dev::scenario::CaptureState::Deciding { .. }
            ),
            "settled before the render thread left the screen: {:?}",
            state_of(&c, 0)
        );
        assert_eq!(side.decided_count(), left, "left inside the load");
        side.seen.load_gate.hold(false);
        let why = failed(&c, 0);
        assert!(
            why.contains("its decision screen ended before a frame at 600000 ms"),
            "{why}"
        );
        side.clock.run();
    });
    assert!(ran.ran.is_ok(), "{:?}", ran.ran);
    let r = ran.records.first().expect("the screen left");
    assert!(
        ran.stamps.iter().filter(|(l, _)| *l == r.lifetime).count()
            == usize::try_from(r.skin_frames + r.page_frames + r.ended_frames).unwrap(),
        "{r:?} {:?}",
        ran.stamps
    );
}

/// The practice page's entry leaves the decision screen: a capture past
/// its time fails there.
#[test]
fn the_practice_page_leaves_the_decision_screen() {
    let dir = tempfile::tempdir().unwrap();
    let c = decision_captures(dir.path(), &[600_000]);
    c.arm(0);
    let set = Set::new("s.luaskin");
    let opts = Opts {
        captures: Some(c.clone()),
        ..Opts::default()
    };
    let practice = crate::ui_text::Message::PracticeTitle { name: "a" }.text();
    let ran = run_with(&set, None, opts, |side| {
        side.prepared();
        side.choose(MenuKey::Practice);
        side.clock.run();
        side.seen.until("the practice page", |d| {
            d.iter().any(|d| d.frame == Frame::Page(practice.clone()))
        });
        settled_by_now(&c, 0, "the practice page");
    });
    assert!(ran.ran.is_ok(), "{:?}", ran.ran);
    let why = failed(&c, 0);
    assert!(why.contains("its decision screen ended"), "{why}");
}
