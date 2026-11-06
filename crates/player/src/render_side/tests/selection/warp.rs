//! The selection skin on a real screen, on WARP through the render thread
//! (dev-tools: the screen reports its selection frames and text).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use game::Preparation;
use input::QpcClock;
use skin_model::{RowKey, RowKind, RowScore, RowView, ScreenKind, SelectionPlace, SelectionView};

use super::super::{MakeScreen, measure, view, wait_until};
use super::update_of;
use crate::menu_screen::MenuScreen;
use crate::render_side::{FrameSink, RenderLink, Screen, spawn};

/// What a real screen's frames show: the selection frame's quads and the
/// lines the skin's text asked for.
#[derive(Clone, Default)]
struct Frames(Arc<Mutex<Vec<Frame>>>);

/// One frame: its selection quads, the lines asked for, its device,
/// whether it was presented, and for the first frames how many pixels
/// differ from the top-left one (what it drew over the clear colour).
type Frame = (Option<usize>, Vec<String>, u64, bool, Option<usize>);

/// The frames whose pixels [`Frames`] reads back.
const READ_BACK: usize = 3;

impl FrameSink for Frames {
    fn frame(&mut self, screen: &dyn Screen, drawn: &crate::measure::Drawn<'_>) {
        let quads = screen.selection_quads().map(|q| q.len());
        let (lines, _) = screen.selection_requests();
        let mut frames = self.0.lock().unwrap();
        let lit = (frames.len() < READ_BACK)
            .then(|| screen.read_back())
            .flatten()
            .map(|(_, _, px)| {
                let (pixels, _) = px.as_chunks::<4>();
                let corner = pixels[0];
                pixels.iter().filter(|p| **p != corner).count()
            });
        frames.push((quads, lines, screen.device(), drawn.timing.presented, lit));
    }
}

/// The pacing of a [`Lossy`] screen: while `pace` is set, each frame wait
/// takes one of `permits`, holding until there is one.
#[derive(Default)]
struct Gate {
    pace: AtomicBool,
    permits: AtomicU32,
    /// The waits begun.
    waits: AtomicU32,
    /// Set while a wait holds for want of a permit.
    held: AtomicBool,
}

impl Gate {
    /// Holds the render thread in its next frame wait.
    fn hold(&self) {
        self.pace.store(true, Ordering::Release);
        self.check(wait_until(|| self.held.load(Ordering::Acquire)));
    }

    /// Grants the held render thread one frame and waits until it holds
    /// in the frame wait after it.
    fn step(&self) {
        let before = self.waits.load(Ordering::Acquire);
        self.permits.store(1, Ordering::Release);
        self.check(wait_until(|| {
            self.waits.load(Ordering::Acquire) > before
                && self.held.load(Ordering::Acquire)
                && self.permits.load(Ordering::Acquire) == 0
        }));
    }

    /// Releases the render thread before failing, so that it can exit.
    fn check(&self, held: bool) {
        if !held {
            self.pace.store(false, Ordering::Release);
        }
        assert!(held, "the render thread did not hold in its frame wait");
    }
}

/// A [`MenuScreen`] whose next selection frame fails as a lost device
/// while `lose` is set (a real device loss: the screen is built again on a
/// new device), and whose frames `gate` paces.
struct Lossy {
    menu: MenuScreen,
    lose: Arc<AtomicBool>,
    gate: Arc<Gate>,
}

impl Screen for Lossy {
    fn load(
        &mut self,
        chart: &bms_model::Chart,
        section: Option<&eval::Section>,
    ) -> Result<(), render::RenderError> {
        self.menu.load(chart, section)
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
        skin: &Arc<render::SkinBundle>,
        request: u64,
    ) -> Result<Preparation, render::RenderError> {
        self.menu.install(kind, skin, request)
    }
    fn installation(&self, kind: ScreenKind) -> Option<render::Installation> {
        self.menu.installation(kind)
    }
    fn restore(
        &mut self,
        kind: ScreenKind,
        installation: &render::Installation,
    ) -> Result<(), render::RenderError> {
        self.menu.restore(kind, installation)
    }
    fn remove(&mut self, kind: ScreenKind) {
        self.menu.remove(kind);
    }
    fn wait(&self) -> bool {
        let gate = &self.gate;
        gate.waits.fetch_add(1, Ordering::AcqRel);
        while gate.pace.load(Ordering::Acquire) {
            if gate
                .permits
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_ok()
            {
                break;
            }
            gate.held.store(true, Ordering::Release);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        gate.held.store(false, Ordering::Release);
        !self.menu.paced() || self.menu.wait()
    }
    fn paced(&self) -> bool {
        self.gate.pace.load(Ordering::Acquire) || self.menu.paced()
    }
    fn build(&mut self, f: &render::FrameInput<'_>) -> Result<(), skin_model::BuildError> {
        self.menu.build(f)
    }
    fn draw(&mut self) -> Result<render::FrameTiming, render::RenderError> {
        self.menu.draw()
    }
    fn text(&mut self, lines: &[&str]) -> Result<bool, render::RenderError> {
        self.menu.text(lines)
    }
    fn menu(
        &mut self,
        page: &crate::menu_screen::MenuPage,
        changed: bool,
    ) -> Result<render::FrameTiming, render::RenderError> {
        self.menu.menu(page, changed)
    }
    fn selection(
        &mut self,
        update: &crate::selection_skin::SelectionUpdate,
        logic: Option<&mut crate::selection_skin::SelectionLogic>,
        changed: bool,
        now: bms_model::TimeUs,
    ) -> Result<crate::selection_skin::SelectionDrawn, render::RenderError> {
        if self.lose.swap(false, Ordering::AcqRel) {
            return Err(render::RenderError::SurfaceLost);
        }
        self.menu.selection(update, logic, changed, now)
    }
    fn selection_requests(&self) -> (Vec<String>, usize) {
        self.menu.selection_requests()
    }
    fn selection_quads(&self) -> Option<Vec<skin_model::Quad>> {
        self.menu.selection_quads()
    }
    fn selection_observations(&self) -> Option<Vec<skin_model::ElementObservation>> {
        self.menu.selection_observations()
    }
    fn selection_views(&self) -> u64 {
        self.menu.selection_views()
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

/// A render thread on a WARP menu screen whose renderer refuses its first
/// selection frame, or its first selection text with `text` (`None`:
/// neither), and its frames.
fn warp_link(fault: Option<bool>) -> (RenderLink, Frames) {
    let (link, frames, _) = lossy_link(fault);
    (link, frames)
}

/// [`warp_link`] on [`Lossy`] screens, with their flag.
fn lossy_link(fault: Option<bool>) -> (RenderLink, Frames, Arc<AtomicBool>) {
    let lose = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&lose);
    let built = Arc::new(AtomicBool::new(false));
    let make: MakeScreen = Box::new(move || {
        let r = render::Renderer::new(
            render::Target::Offscreen {
                width: 1_920,
                height: 1_080,
            },
            render::AdapterChoice::Warp,
        )?;
        let mut s = MenuScreen::new(r, Some(render::text::Fonts::runtime()?));
        // The first screen only: a screen built again draws as it can.
        if let Some(text) = fault.filter(|_| !built.swap(true, Ordering::AcqRel)) {
            s.renderer_mut().refuse_next_selection(text);
        }
        Ok(Box::new(Lossy {
            menu: s,
            lose: Arc::clone(&flag),
            gate: Arc::default(),
        }) as Box<dyn Screen>)
    });
    let frames = Frames::default();
    let clock = QpcClock::start();
    let link = spawn(
        make,
        None,
        None,
        clock,
        view(),
        measure(clock),
        Box::new(frames.clone()),
    )
    .unwrap();
    (link, frames, lose)
}

fn row(key: RowKey, kind: RowKind, title: &str) -> RowView {
    RowView {
        key,
        kind,
        title: title.to_owned(),
        subtitle: String::new(),
        artist: String::new(),
        level: String::new(),
        level_value: 0,
        difficulty: None,
        score: RowScore::Unplayed,
        features: None,
        random: false,
    }
}

/// A view of the all-charts row, a folder and a chart at `place` with
/// `query`, the cursor on the first.
fn listed(place: SelectionPlace, query: &str) -> SelectionView {
    let folder = skin_model::FolderId::new(std::path::PathBuf::from("曲集"));
    SelectionView {
        rows: vec![
            row(RowKey::All, RowKind::All { charts: 2 }, ""),
            row(
                RowKey::Folder(folder),
                RowKind::Folder { charts: 1 },
                "曲集",
            ),
            row(
                RowKey::Chart(std::path::PathBuf::from("a.bms")),
                RowKind::Chart,
                "譜面の名前",
            ),
        ],
        cursor: Some(0),
        position: None,
        place,
        query: query.to_owned(),
        selected: None,
    }
}

/// The fixture skin built for a WARP target.
fn skin() -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    let dir = crate::selection_skin::fixture();
    let limits = crate::render_common::offscreen(1_920, 1_080);
    let skin = crate::selection_skin::fixture_bundle(&limits, dir.path());
    (dir, skin)
}

/// The selection skin installed on a real screen draws each update with
/// the player's labels, folder names and the query as they are: the
/// all-charts row's label, the place's text at All, in the results and in
/// a folder, and the search's text; without the skin the same updates
/// draw the page, and no selection frame.
#[test]
fn the_installed_skin_draws_each_place_with_the_players_labels() {
    let (_dir, skin) = skin();
    let (link, frames) = warp_link(None);
    assert_eq!(
        link.select(ScreenKind::Selection, Some(Arc::clone(&skin)))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    link.enter_selection(1);
    let asked = |lines: &[&str]| {
        wait_until(|| {
            frames.0.lock().unwrap().iter().any(|(quads, asked, ..)| {
                quads.is_some_and(|n| n > 0) && lines.iter().all(|l| asked.iter().any(|a| a == l))
            })
        })
    };
    let places = [
        (
            SelectionPlace::All,
            "",
            vec!["全譜面", "全譜面 > ", "曲集", "譜面の名前"],
        ),
        (SelectionPlace::Results, "abc", vec!["検索結果 > ", "abc"]),
        (
            SelectionPlace::Folder(vec!["曲集".to_owned(), "下の階".to_owned()]),
            "",
            vec!["曲集 > 下の階 > "],
        ),
    ];
    for (revision, (place, query, lines)) in (1..).zip(places) {
        link.show_selection(update_of(1, revision, listed(place, query)));
        assert!(
            asked(&lines),
            "{lines:?}: {:?}",
            frames.0.lock().unwrap().last()
        );
    }
    assert_eq!(link.take_selection_refusal(), None);
    link.exit();

    // No skin installed: the page, and no selection frame.
    let (link, frames) = warp_link(None);
    link.enter_selection(1);
    link.show_selection(update_of(1, 1, listed(SelectionPlace::All, "")));
    assert!(wait_until(|| frames.0.lock().unwrap().len() > 3));
    assert!(frames.0.lock().unwrap().iter().all(|(q, ..)| q.is_none()));
    link.exit();
}

/// An update of `revision` whose page is lines of text no frame drew
/// before.
fn with_page(revision: u64) -> crate::selection_skin::SelectionUpdate {
    use crate::menu_screen::{LineStyle, MenuLine, MenuPage};
    let mut u = update_of(1, revision, listed(SelectionPlace::All, ""));
    u.page = MenuPage {
        lines: ["FALLBACK PAGE", "ＷＷＷＷＷＷＷＷ", "the built-in page"]
            .into_iter()
            .map(|text| MenuLine {
                text: text.to_owned(),
                style: LineStyle::Title,
            })
            .collect(),
        caret: None,
    };
    u
}

/// A selection frame or its text refused on a healthy device presents
/// the page prepared with the update in that very frame, removes the skin
/// from the screen and from what a screen built again restores, and says
/// why once: after a real device loss, the screen built again on another
/// device draws the page, never the skin.
#[test]
fn a_refused_selection_frame_or_text_falls_back_for_the_run() {
    for text in [false, true] {
        let (_dir, skin) = skin();
        let (link, frames, lose) = lossy_link(Some(text));
        assert_eq!(
            link.select(ScreenKind::Selection, Some(skin))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        link.enter_selection(1);
        link.show_selection(with_page(1));
        let why = Mutex::new(None);
        assert!(wait_until(|| {
            let mut why = why.lock().unwrap();
            if why.is_none() {
                *why = link.take_selection_refusal();
            }
            why.is_some()
        }));
        let why = why.into_inner().unwrap().unwrap();
        assert_eq!(why.contains("development fault"), text, "{why}");
        // The refused frame, the first drawn, presented the page: its
        // refusal is said before the frame sink sees it.
        assert!(wait_until(|| !frames.0.lock().unwrap().is_empty()));
        let (_, _, device, presented, lit) = frames.0.lock().unwrap()[0].clone();
        assert!(presented, "{text}");
        assert!(lit.is_some_and(|n| n > 500), "{text}: {lit:?}");
        // From then on the page.
        let drawn = frames.0.lock().unwrap().len();
        assert!(wait_until(|| frames.0.lock().unwrap().len() > drawn + 2));
        assert!(
            frames.0.lock().unwrap()[drawn..]
                .iter()
                .all(|(q, ..)| q.is_none())
        );
        // The device lost: the screen built again on another restores no
        // selection skin, and draws the page.
        lose.store(true, Ordering::Release);
        assert!(wait_until(|| link.failed()));
        link.prepare().unwrap();
        link.show_selection(with_page(2));
        let other = |f: &Frame| f.2 != device;
        assert!(wait_until(|| frames
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|f| other(f))
            .count()
            > 2));
        let after: Vec<Frame> = frames
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|f| other(f))
            .cloned()
            .collect();
        assert!(after.iter().all(|(q, ..)| q.is_none()), "{text}");
        assert!(after.iter().all(|f| f.3), "{text}");
        assert_eq!(link.take_selection_refusal(), None);
        link.exit();
    }
}

/// What a selection frame of the settings fixture showed: numbers 57 and
/// 12 as drawn, the master gain's graph's width, and the views the skin
/// had taken by then.
#[derive(Clone, Debug, PartialEq)]
struct SettingsSeen {
    numbers: Vec<i64>,
    graph: Option<f32>,
    views: u64,
}

/// Each selection frame's [`SettingsSeen`].
#[derive(Clone, Default)]
struct SettingsFrames(Arc<Mutex<Vec<SettingsSeen>>>);

impl FrameSink for SettingsFrames {
    fn frame(&mut self, screen: &dyn Screen, _drawn: &crate::measure::Drawn<'_>) {
        let (Some(observed), Some(quads)) =
            (screen.selection_observations(), screen.selection_quads())
        else {
            return;
        };
        let numbers = observed.iter().filter_map(|o| o.value).collect();
        // The graph is the fixture's last element.
        let graph = observed
            .last()
            .filter(|o| o.outcome == skin_model::Outcome::Sampled)
            .and_then(|o| quads.get(o.quad_start as usize))
            .map(|q| q.rect[2]);
        self.0.lock().unwrap().push(SettingsSeen {
            numbers,
            graph,
            views: screen.selection_views(),
        });
    }
}

/// C13 (player half) and C14: through the player's startup settings, its
/// settings keys' values, the select screen's update and the render
/// thread's frame on WARP, a loaded `master_volume` shows the source's
/// number of the `f32` that plays and its exact ratio, the gain playing
/// unchanged; F4, F6 and F5 on selection each show in the one frame
/// granted after the key, with no view taken.
#[test]
fn the_settings_show_the_gain_that_plays_and_follow_the_keys() {
    use game::{Adjust, AppRig as _};
    let dir = crate::selection_skin::settings_fixture();
    let limits = crate::render_common::offscreen(1_920, 1_080);
    let skin = crate::selection_skin::fixture_bundle(&limits, dir.path());
    // The width of graph 17 for the gain `n / d`, 1 000 wide.
    let width = |n: u64, d: u64| (1_000.0 * n as f64 / d as f64) as f32;
    for (gain, number, ratio) in [
        (0.7649_f32, 76, (12_832_893, 1 << 24)),
        (0.01, 1, (5_368_709, 1 << 29)),
        (0.45, 45, (7_549_747, 1 << 24)),
    ] {
        let frames = SettingsFrames::default();
        let gate = Arc::new(Gate::default());
        let paced = Arc::clone(&gate);
        let make: MakeScreen = Box::new(move || {
            let r = render::Renderer::new(
                render::Target::Offscreen {
                    width: 1_920,
                    height: 1_080,
                },
                render::AdapterChoice::Warp,
            )?;
            Ok(Box::new(Lossy {
                menu: MenuScreen::new(r, Some(render::text::Fonts::runtime()?)),
                lose: Arc::default(),
                gate: Arc::clone(&paced),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(frames.clone()),
        )
        .unwrap();
        assert_eq!(
            link.select(ScreenKind::Selection, Some(Arc::clone(&skin)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        let mut settings = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        settings.device.master_volume = gain;
        let charts = tempfile::tempdir().unwrap();
        let (sel, _) = crate::selection::tests::tree(charts.path(), &[&["a.bms"]]);
        let seen = || frames.0.lock().unwrap().clone();
        let shows = |numbers: [i64; 2], graph: f32| {
            wait_until(|| {
                seen().last().is_some_and(|s| {
                    s.numbers == numbers && s.graph.is_some_and(|g| (g - graph).abs() < 0.01)
                })
            })
        };
        crate::tests::on_rig_with(settings, link, |rig, _| {
            rig.adjust = None;
            rig.selection = Some(sel);
            rig.entered_selection();
            let tuning = Arc::clone(rig.controls.tuning());
            assert!(
                shows([number, 0], width(ratio.0, ratio.1)),
                "{gain}: {:?}",
                seen().last()
            );
            // The audio's gain is the loaded one, exactly.
            assert_eq!(tuning.volume().to_bits(), gain.to_bits());
            if gain != 0.7649 {
                return;
            }
            // Each key pressed while the render thread holds in its frame
            // wait, before it reads the settings, then exactly one frame
            // granted: that frame shows the key.
            gate.hold();
            let views = seen().last().unwrap().views;
            let mut next = |key| {
                let before = seen().len();
                let pressed = tuning.adjust(key);
                gate.step();
                let after = seen();
                (pressed, after.len() - before, after.last().cloned())
            };
            // F4: 76 + 5, the f32 0.81; then F6 and F5: the display offset
            // 1 ms, then 0.
            let steps = [
                (next(Adjust::VolumeUp), [81, 0]),
                (next(Adjust::DisplayOffsetUp), [81, 1]),
                (next(Adjust::DisplayOffsetDown), [81, 0]),
            ];
            gate.pace.store(false, Ordering::Release);
            let graph = width(13_589_545, 1 << 24);
            for ((pressed, drawn, frame), numbers) in steps {
                assert!(pressed, "{numbers:?}");
                assert_eq!(drawn, 1, "{numbers:?}: {frame:?}");
                let frame = frame.unwrap();
                assert_eq!(frame.numbers, numbers, "{frame:?}");
                assert!(
                    frame.graph.is_some_and(|g| (g - graph).abs() < 0.01),
                    "{frame:?}"
                );
                // No view was taken for any of them.
                assert_eq!(frame.views, views, "{frame:?}");
            }
        });
    }
}
