//! Pointer input on a selection skin through the game's threads (design.md
//! "Input", **Pointer input**): the test thread is the window, feeding its
//! capture and the menu queue as the window does, and the game thread,
//! running the select screen's looks; the render thread is a real one over
//! stub screens that build the real model of
//! [`pointer_fixture`](crate::selection_skin::pointer_fixture), and answers
//! each batch and drag on what it last built. Holding the render thread
//! ([`hold`], [`grant`]) leaves its frames on the update it took before.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bms_model::TimeUs;
use game::AppRig as _;
use skin_model::{Button, RowRef, RowRefKind, ScreenKind, SelectionPlace};

use super::{Selection, panel_key, panel_keys, settle, tree};
use crate::controls::Controls;
use crate::play::PlayerRig;
use crate::pointer::{Built, Capture, Dispatched, PointerAct, PointerRequest, PointerShared};
use crate::render_side::tests::{Installers, wait_until};
use crate::select::{MenuInput, MenuKey, MenuQueue};

use Dispatched::{Drag, Input, Notch, Press};
use PointerAct::{Key, MoveTo, Nothing, Rows};

/// The window's side: its capture of the mouse, the pointer's shared state
/// and the menu queue it admits presses and notches into.
struct Window {
    capture: Capture,
    shared: Arc<PointerShared>,
    menu: Arc<MenuQueue>,
}

impl Window {
    /// The pointer moved to `at`.
    fn to(&mut self, c: &Controls, at: [i32; 2]) {
        let word = self.shared.word();
        self.capture.moved(&self.shared, at, word, c.scene());
    }

    /// `button` pressed at `at`, and held.
    fn down(&mut self, c: &Controls, at: [i32; 2], button: Button) {
        self.to(c, at);
        let word = self.shared.word();
        let menu = &self.menu;
        self.capture
            .button(&self.shared, button, true, word, c.scene(), |i| {
                menu.push_stamped(i, None)
            });
    }

    /// `button` released.
    fn up(&mut self, c: &Controls, button: Button) {
        let word = self.shared.word();
        let menu = &self.menu;
        self.capture
            .button(&self.shared, button, false, word, c.scene(), |i| {
                menu.push_stamped(i, None)
            });
    }

    /// `button` pressed at `at` and released.
    fn click(&mut self, c: &Controls, at: [i32; 2], button: Button) {
        self.down(c, at, button);
        self.up(c, button);
    }

    /// The wheel turned `notches` toward the user (a row down each).
    #[expect(clippy::cast_precision_loss, reason = "a few notches")]
    fn wheel(&mut self, c: &Controls, notches: i32) {
        let word = self.shared.word();
        let menu = &self.menu;
        self.capture
            .wheel(&self.shared, -(notches as f32), word, c.scene(), |i| {
                menu.push_stamped(i, None)
            });
    }

    /// `key` typed.
    fn key(&self, key: MenuKey) {
        self.menu.push(MenuInput::Key(key));
    }
}

// The fixture's points (y-down target pixels; window pixels are the
// target's), each inside one target only.

/// Slot `i`'s point, right of every element drawn over the rows.
const fn slot(i: i32) -> [i32; 2] {
    [1_700, 140 + 100 * i]
}

/// Row `row`'s point while the build that answers has the cursor on
/// `cursor`: the center slot (3) holds the cursor's row.
fn row(cursor: usize, row: usize) -> [i32; 2] {
    let i = i32::try_from(3 + row).unwrap() - i32::try_from(cursor).unwrap();
    assert!((0..7).contains(&i), "row {row} is not drawn");
    slot(i)
}

/// Nothing is drawn here.
const EMPTY: [i32; 2] = [1_500, 1_000];
/// Event 12, the order.
const ORDER: [i32; 2] = [150, 225];
/// Event 15, play.
const PLAY: [i32; 2] = [150, 325];
/// Event 315, practice.
const PRACTICE: [i32; 2] = [150, 425];
/// Search text 30's input box: right-aligned at x 900, 400 wide, so its
/// box runs from 500; this is `x − w/2`.
const SEARCH: [i32; 2] = [700, 60];
/// Event 13 over the list slider's travel, at v = 0.42.
const MODE_ON_SLIDER: [i32; 2] = [520, 920];
/// Event 13 over slot 4.
const MODE_ON_ROW: [i32; 2] = [1_320, 530];
/// Event 331 over slot 4.
const COVER_ON_ROW: [i32; 2] = [1_420, 530];
/// Event 340 over slot 4.
const JUDGE_ON_ROW: [i32; 2] = [1_520, 530];
/// Inside the hover image's `mouseRect`.
const HOVER: [i32; 2] = [150, 1_010];
/// The hover image's region.
const HOVER_RECT: [f32; 4] = [100.0, 1_000.0, 179.0, 37.0];

/// The list slider's travel at `v`‰.
const fn list_slider(v: i32) -> [i32; 2] {
    [100 + v, 920]
}

/// Slider 17's travel (over slot 5) at `v`‰.
const fn volume_at(v: i32) -> [i32; 2] {
    [1_050 + v / 5, 630]
}

/// The master gain the rig plays at: slider 17 shows it.
const GAIN: f32 = 0.45;

/// The settings a selection skin shows, which slider 17 needs to draw; the
/// gain it shows is the rig's ([`GAIN`]).
fn shown() -> skin_model::SelectionSettings {
    skin_model::SelectionSettings {
        display_offset_ms: 0,
        master_volume: skin_model::MasterVolume::of_gain(GAIN),
        speed: skin_model::SpeedView::Classic,
        order: skin_model::SelectionOrder::Title,
        gauge: None,
        covers_on: [false; 3],
        play_to_end: false,
        assignment: None,
    }
}

/// `f` on a rig of the game's threads with `sel` on its select screen,
/// the render thread drawing [`pointer_fixture`] (slider 17 drawn with
/// `volume`) and taking the pointer, entered with `panels`' level if given,
/// the settings shown and its first update drawn; the master gain at
/// [`GAIN`]; the library's scan paused, so the list changes only as the
/// test changes it.
///
/// [`pointer_fixture`]: crate::selection_skin::pointer_fixture
fn on_pointer<R>(
    mut sel: Selection,
    volume: bool,
    panels: Option<input::PanelLevel>,
    f: impl FnOnce(&mut PlayerRig<'_>, &mut Window, &Installers) -> R,
) -> R {
    let dir = crate::selection_skin::pointer_fixture(volume);
    let limits = skin_model::AtlasLimits {
        max_side: 4_096,
        max_layers: 16,
        reserved_layers: render::RESERVED_LAYERS,
    };
    let (skin, _) = crate::selection_skin::fixture_scripted(limits, dir.path(), "s.json");
    let screens = Installers::default();
    let render = screens.spawn_bare();
    assert_eq!(
        render
            .select(ScreenKind::Selection, Some(skin))
            .wait()
            .unwrap(),
        game::Preparation::Ready
    );
    sel.started(None, Some((3, 7)));
    sel.pointer.log = Some(Vec::new());
    let shared = Arc::clone(&sel.pointer.shared);
    render.pointer(Arc::clone(&shared));
    let mut window = Window {
        capture: Capture::default(),
        shared,
        menu: Arc::clone(&sel.menu),
    };
    let settings = game::Settings {
        device: game::DeviceSettings {
            master_volume: GAIN,
            ..game::DeviceSettings::default()
        },
        player: game::PlayerSettings::default(),
    };
    let (out, _) = crate::tests::on_rig_with(settings, render, |rig, _| {
        rig.adjust = None;
        if let Some(level) = panels {
            rig.panels = Some(level);
        }
        rig.selection = Some(sel);
        rig.entered_selection();
        assert_eq!(rig.controls.tuning().volume(), GAIN);
        sel_mut(rig).feed.settings = Some(shown());
        sel_mut(rig).dirty = true;
        quiet(rig);
        drawn(rig, &screens);
        log(rig);
        f(rig, &mut window, &screens)
    });
    out
}

fn sel_of<'r>(rig: &'r PlayerRig<'_>) -> &'r Selection {
    rig.selection.as_ref().unwrap()
}

fn sel_mut<'r>(rig: &'r mut PlayerRig<'_>) -> &'r mut Selection {
    rig.selection.as_mut().unwrap()
}

/// The library's scan paused, and a pass that ended meanwhile read: the
/// list stays as it is.
fn quiet(rig: &mut PlayerRig<'_>) {
    let sel = sel_mut(rig);
    let _ = sel.shelf.wait_paused(
        Duration::from_millis(1),
        || false,
        Duration::from_secs(3_600),
        || {},
    );
    for _ in 0..2 {
        if sel.shelf.poll(Instant::now()) {
            sel.refresh();
        }
    }
}

/// One look, then the render thread's frame after what it sent.
fn drawn(rig: &mut PlayerRig<'_>, screens: &Installers) {
    assert!(rig.look().is_none());
    assert!(rig.render.fence(Duration::from_secs(60)));
    frames(screens, 1);
}

/// Waits for `n` more selection frames.
fn frames(screens: &Installers, n: usize) {
    let seen = screens.selections.lock().unwrap().len();
    assert!(wait_until(
        || screens.selections.lock().unwrap().len() >= seen + n
    ));
}

/// Looks until every queued input was dispatched and no batch is in
/// flight; the pick, if one was made.
fn take_all(rig: &mut PlayerRig<'_>) -> Option<game::Pick> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(pick) = rig.look() {
            return Some(pick);
        }
        let s = sel_of(rig);
        if s.pointer.flight.is_none() && s.menu.queued() == 0 {
            return None;
        }
        assert!(Instant::now() < deadline, "a batch was never answered");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// [`take_all`] with nothing picked, then the frame after it.
fn act(rig: &mut PlayerRig<'_>, screens: &Installers) {
    assert_eq!(take_all(rig), None);
    drawn(rig, screens);
}

/// Two more frames, so the render thread took the latest movement, then
/// [`act`].
fn dragged(rig: &mut PlayerRig<'_>, screens: &Installers) {
    frames(screens, 2);
    act(rig, screens);
}

/// What the passes dispatched since the last call, with the cursor after
/// each.
fn log(rig: &mut PlayerRig<'_>) -> Vec<(Dispatched, usize)> {
    std::mem::take(sel_mut(rig).pointer.log.as_mut().unwrap())
}

fn cursor(rig: &PlayerRig<'_>) -> usize {
    sel_of(rig).screen.cursor()
}

fn place(rig: &PlayerRig<'_>) -> SelectionPlace {
    sel_of(rig).screen.view(0, 0, false).place
}

/// The render thread held in its frame wait, on the update it took.
fn hold(screens: &Installers) {
    screens.pace.store(true, Ordering::Release);
    assert!(wait_until(|| screens.held.load(Ordering::Acquire)));
}

/// Grants the held render thread `n` frames and waits until it holds in
/// the frame wait after them.
fn grant(screens: &Installers, n: u32) {
    let before = screens.waits.load(Ordering::Acquire);
    screens.permits.store(n, Ordering::Release);
    assert!(wait_until(|| {
        screens.waits.load(Ordering::Acquire) >= before + n
            && screens.held.load(Ordering::Acquire)
            && screens.permits.load(Ordering::Acquire) == 0
    }));
}

fn release(screens: &Installers) {
    screens.pace.store(false, Ordering::Release);
}

/// The rects the last frame built.
fn last_rects(screens: &Installers) -> Vec<[f32; 4]> {
    screens.built.lock().unwrap().last().unwrap().2.clone()
}

/// The fixture as built: each slot holding a row draws its bar at its
/// rect, and every target is where the points above take it.
#[test]
fn the_pointer_fixture_draws_where_its_points_are() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = tree(dir.path(), &[&["a.bms", "b.bms", "c.bms"]]);
    on_pointer(sel, true, None, |_, _, screens| {
        let rects = last_rects(screens);
        let has = |r: [f32; 4]| rects.contains(&r);
        // The cursor on row 0: slots 3 to 6 hold rows 0 to 3.
        for i in 3..7 {
            assert!(
                has([1_000.0, 100.0 + 100.0 * i as f32, 800.0, 80.0]),
                "{rects:?}"
            );
        }
        assert!(!has([1_000.0, 200.0, 800.0, 80.0]));
        for r in [
            [100.0, 200.0, 100.0, 50.0],
            [100.0, 300.0, 100.0, 50.0],
            [100.0, 400.0, 100.0, 50.0],
            [500.0, 905.0, 40.0, 30.0],
            [1_300.0, 510.0, 60.0, 40.0],
            [1_400.0, 510.0, 60.0, 40.0],
            [1_500.0, 510.0, 60.0, 40.0],
            // The list slider at 0 and slider 17 at 0.45 of 200.
            [100.0, 900.0, 20.0, 40.0],
            [1_140.0, 610.0, 20.0, 40.0],
        ] {
            assert!(has(r), "{r:?} {rects:?}");
        }
        // No pointer: the hover image is not drawn.
        assert!(!has(HOVER_RECT));
    });
}

/// P2: a left press on a row moves the cursor to it by its identity, on
/// the drawn cursor's row acts as Enter, and a slot with no row is no
/// target. With the render thread a frame behind (the cursor moved down
/// after the frame it holds), the old cursor row moves the cursor back
/// and plays nothing; an off-centre chart or folder is moved to, not
/// played or opened; once drawn at the centre, the chart plays.
#[test]
fn a_left_press_moves_to_its_row_and_enters_the_drawn_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, roots) = tree(dir.path(), &[&["f/s/x.bms", "a.bms", "b.bms", "c.bms"]]);
    on_pointer(sel, true, None, |rig, w, screens| {
        let c = rig.controls;
        // Top: all charts, folder f, charts a, b and c.
        let kinds: Vec<_> = (0..5).map(|i| sel_of(rig).screen.row_kind_at(i)).collect();
        assert_eq!(
            kinds,
            [
                RowRefKind::All,
                RowRefKind::Folder,
                RowRefKind::Chart,
                RowRefKind::Chart,
                RowRefKind::Chart
            ]
            .map(Some)
        );
        w.key(MenuKey::Down);
        w.key(MenuKey::Down);
        act(rig, screens);
        assert_eq!(cursor(rig), 2);
        log(rig);
        // The render thread holds the frame with the cursor on a; Down
        // moves it to b, and the press on a's slot is answered on that
        // frame.
        hold(screens);
        w.key(MenuKey::Down);
        w.click(c, row(2, 2), Button::Left);
        assert!(rig.look().is_none());
        assert_eq!(cursor(rig), 3);
        grant(screens, 1);
        assert_eq!(take_all(rig), None);
        assert_eq!(log(rig), [(Input, 3), (Press(MoveTo(2)), 2)]);
        release(screens);
        drawn(rig, screens);
        // An off-centre chart: moved to.
        w.click(c, row(2, 4), Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(4)), 4)]);
        // An off-centre folder: moved to, not opened.
        w.click(c, row(4, 1), Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(1)), 1)]);
        assert_eq!(place(rig), SelectionPlace::Top);
        // A slot that drew no row is no target.
        w.click(c, slot(1), Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Nothing), 1)]);
        // Chart c, then once drawn at the centre, played.
        w.click(c, row(1, 4), Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(4)), 4)]);
        w.click(c, row(4, 4), Button::Left);
        let pick = take_all(rig);
        assert_eq!(pick, Some(game::Pick::Play(roots[0].join("c.bms"))));
        assert_eq!(log(rig), [(Press(Key(MenuKey::Enter)), 4)]);
    });
}

/// The listed rows' titles as the page shows them, the cursor's with `>`.
fn titles(rig: &PlayerRig<'_>) -> Vec<String> {
    super::list(sel_of(rig))
}

/// P2 and P3: a row or list slider command made on a list read before the
/// current one does nothing, whichever button, and a row command whose
/// index or kind is not the current list's does nothing. Two levels down,
/// the folders Alpha and Gamma and the charts beta, delta and epsilon are
/// listed, the cursor on Gamma; with the render thread holding that frame,
/// Alpha is renamed Zeta and the list read again: Gamma, Zeta and the same
/// charts, the cursor kept on Gamma, now first. From the held frame, a
/// left press on Gamma's slot, a left press on delta's, a right press on
/// Gamma's and the list slider at 0.75 do nothing (by index alone they
/// would move to Zeta and delta, go up a level and move to delta). Then
/// commands at the current revision: index 5 (the count) and a chart's
/// index named a folder do nothing, left or right; a valid one moves. On
/// the new frame, the list slider at 0.75 moves to row 3.
#[test]
fn a_command_on_another_list_or_row_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, roots) = tree(
        dir.path(),
        &[&[
            "one/two/Alpha/s/a.bms",
            "one/two/Gamma/s/g.bms",
            "one/two/beta/beta.bms",
            "one/two/delta/delta.bms",
            "one/two/epsilon/epsilon.bms",
        ]],
    );
    on_pointer(sel, true, None, |rig, w, screens| {
        let c = rig.controls;
        w.key(MenuKey::Down);
        w.key(MenuKey::Enter);
        act(rig, screens);
        w.key(MenuKey::Enter);
        act(rig, screens);
        w.key(MenuKey::Down);
        act(rig, screens);
        let two = place(rig);
        assert_eq!(
            two,
            SelectionPlace::Folder(vec!["one".to_owned(), "two".to_owned()])
        );
        assert_eq!(
            titles(rig),
            ["▶ Alpha  (1)", ">▶ Gamma  (1)", "beta", "delta", "epsilon"]
        );
        log(rig);
        hold(screens);
        let dir = roots[0].join("one/two");
        std::fs::rename(dir.join("Alpha"), dir.join("Zeta")).unwrap();
        {
            let s = sel_mut(rig);
            s.shelf.release();
            settle(s);
        }
        quiet(rig);
        assert_eq!(
            titles(rig),
            [">▶ Gamma  (1)", "▶ Zeta  (1)", "beta", "delta", "epsilon"]
        );
        assert_eq!(cursor(rig), 0);
        // The new list's update waits while the frame of the old one is
        // held; the presses are answered on the old one.
        w.click(c, row(1, 1), Button::Left);
        w.click(c, row(1, 3), Button::Left);
        w.click(c, row(1, 1), Button::Right);
        w.click(c, list_slider(750), Button::Left);
        assert!(rig.look().is_none());
        grant(screens, 1);
        assert_eq!(take_all(rig), None);
        assert_eq!(log(rig), [(Press(Nothing), 0); 4]);
        assert_eq!(place(rig), two);
        // Commands made at the current revision, as the render thread
        // would answer them.
        let s = sel_mut(rig);
        let word = s.pointer.shared.word();
        let built = Built {
            lifetime: s.feed.lifetime,
            revision: 0,
            list_revision: s.feed.list_revision,
            cursor: Some(0),
            skinned: true,
        };
        let commands = [
            (5, RowRefKind::Chart, Button::Left),
            (5, RowRefKind::Chart, Button::Right),
            (2, RowRefKind::Folder, Button::Left),
            (2, RowRefKind::Folder, Button::Right),
            // Valid: Zeta.
            (1, RowRefKind::Folder, Button::Left),
        ];
        let mut requests = commands
            .iter()
            .map(|&(_, _, button)| PointerRequest::Press {
                word,
                button,
                at: [0, 0],
            });
        s.pointer.flight = s.pointer.shared.submit(built.lifetime, || requests.next());
        assert!(s.pointer.flight.is_some());
        let mut hits = commands.iter().map(|&(index, kind, button)| {
            Some(skin_model::PointerHit::Row {
                row: RowRef {
                    list_revision: built.list_revision,
                    index,
                    kind,
                },
                button,
            })
        });
        assert!(
            s.pointer
                .shared
                .answer(Some(built), |_| hits.next().unwrap())
        );
        assert_eq!(take_all(rig), None);
        assert_eq!(
            log(rig),
            [
                (Press(Nothing), 0),
                (Press(Nothing), 0),
                (Press(Nothing), 0),
                (Press(Nothing), 0),
                (Press(MoveTo(1)), 1),
            ]
        );
        assert_eq!(place(rig), two);
        // The new list drawn: the list slider at 0.75 of five rows.
        release(screens);
        drawn(rig, screens);
        w.click(c, list_slider(750), Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(3)), 3)]);
    });
}

/// P3: the list slider moves the cursor to `round((count − 1)·v)`, ties
/// up, for a press or a drag; with one row it does nothing.
#[test]
fn the_list_slider_moves_to_its_rounded_row() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = tree(dir.path(), &[&["a.bms", "b.bms"]]);
    on_pointer(sel, true, None, |rig, w, screens| {
        let c = rig.controls;
        // Three rows: all charts, a and b.
        assert_eq!(sel_of(rig).screen.selectable(), 3);
        w.click(c, list_slider(250), Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(1)), 1)]);
        w.click(c, list_slider(1_000), Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(2)), 2)]);
        w.key(MenuKey::Home);
        act(rig, screens);
        log(rig);
        // A drag from 0 to 1.
        w.down(c, list_slider(0), Button::Left);
        act(rig, screens);
        w.to(c, list_slider(1_000));
        dragged(rig, screens);
        w.up(c, Button::Left);
        assert_eq!(log(rig), [(Press(MoveTo(0)), 0), (Drag(MoveTo(2)), 2)]);
        // One row: the search for "a".
        w.key(MenuKey::Tab);
        w.menu.push(MenuInput::Text("a".to_owned()));
        w.key(MenuKey::Tab);
        act(rig, screens);
        assert_eq!(sel_of(rig).screen.selectable(), 1);
        log(rig);
        w.click(c, list_slider(500), Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Nothing), 0)]);
    });
}

/// Charts `c1` to `c{n}` at Top, under the "all charts" row.
fn charts(dir: &std::path::Path, n: usize) -> (Selection, Vec<std::path::PathBuf>) {
    let files: Vec<String> = (1..=n).map(|i| format!("c{i}.bms")).collect();
    let files: Vec<&str> = files.iter().map(String::as_str).collect();
    tree(dir, &[&files])
}

/// P4: the wheel moves the cursor a row a notch, toward the user down,
/// with the search box focused too; under a panel it does nothing.
#[test]
fn the_wheel_moves_a_row_a_notch_but_not_under_a_panel() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = charts(dir.path(), 8);
    let (mut set, level) = panel_keys();
    on_pointer(sel, true, Some(level), |rig, w, screens| {
        let c = rig.controls;
        w.wheel(c, 3);
        act(rig, screens);
        assert_eq!(log(rig), [(Notch(Rows(3)), 3)]);
        w.wheel(c, -1);
        act(rig, screens);
        assert_eq!(log(rig), [(Notch(Rows(-1)), 2)]);
        w.key(MenuKey::Tab);
        w.wheel(c, 2);
        act(rig, screens);
        assert_eq!(log(rig), [(Input, 2), (Notch(Rows(2)), 4)]);
        assert!(sel_of(rig).screen.searching());
        w.key(MenuKey::Tab);
        act(rig, screens);
        log(rig);
        // START held: panel 1.
        panel_key(&mut set, 0x12, 1);
        w.wheel(c, 1);
        act(rig, screens);
        assert_eq!(log(rig), [(Notch(Nothing), 4)]);
        panel_key(&mut set, 0x12, 0);
    });
}

/// P5: event 12 steps the order as Right for the left button and Left for
/// the right one (click type 0); event 15 on a folder does nothing, and
/// 315 on a chart opens its practice.
#[test]
fn the_order_play_and_practice_buttons_act() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, roots) = tree(dir.path(), &[&["f/s/x.bms", "a.bms"]]);
    on_pointer(sel, true, None, |rig, w, screens| {
        let c = rig.controls;
        assert_eq!(sel_of(rig).screen.order(), 0);
        w.click(c, ORDER, Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Key(MenuKey::Right)), 0)]);
        assert_eq!(sel_of(rig).screen.order(), 1);
        w.click(c, ORDER, Button::Right);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Key(MenuKey::Left)), 0)]);
        assert_eq!(sel_of(rig).screen.order(), 0);
        // Folder f under the cursor.
        w.key(MenuKey::Down);
        act(rig, screens);
        log(rig);
        assert_eq!(sel_of(rig).screen.row_kind_at(1), Some(RowRefKind::Folder));
        w.click(c, PLAY, Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Nothing), 1)]);
        assert_eq!(place(rig), SelectionPlace::Top);
        // Chart a under the cursor.
        w.key(MenuKey::Down);
        act(rig, screens);
        log(rig);
        w.click(c, PRACTICE, Button::Left);
        assert_eq!(
            take_all(rig),
            Some(game::Pick::Practice(roots[0].join("a.bms")))
        );
        assert_eq!(log(rig), [(Press(Key(MenuKey::Practice)), 2)]);
    });
}

/// P6: an element whose event has no native contract takes the press over
/// the row under it and does nothing; the same row beside it moves the
/// cursor.
#[test]
fn an_event_with_no_contract_takes_the_press() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = charts(dir.path(), 3);
    on_pointer(sel, true, None, |rig, w, screens| {
        let c = rig.controls;
        // Slot 4 holds row 1.
        w.click(c, MODE_ON_ROW, Button::Left);
        w.click(c, JUDGE_ON_ROW, Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Nothing), 0), (Press(Nothing), 0)]);
        w.click(c, [1_700, 530], Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(1)), 1)]);
    });
}

/// P7: before K-b, slider 17 takes a press or a drag over the row under
/// it and changes nothing: the gain stays and the cursor stays off that
/// row; with slider 17 not drawn, the same press moves to the row.
#[test]
fn the_volume_slider_takes_the_press_and_changes_nothing() {
    for volume in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (sel, _) = charts(dir.path(), 3);
        on_pointer(sel, volume, None, |rig, w, screens| {
            let c = rig.controls;
            // Slot 5 holds the third row (2).
            w.click(c, volume_at(500), Button::Left);
            act(rig, screens);
            if !volume {
                assert_eq!(log(rig), [(Press(MoveTo(2)), 2)]);
                return;
            }
            assert_eq!(log(rig), [(Press(Nothing), 0)]);
            // A drag from 1.0 to 0.2.
            w.down(c, volume_at(1_000), Button::Left);
            act(rig, screens);
            w.to(c, volume_at(200));
            dragged(rig, screens);
            w.up(c, Button::Left);
            assert_eq!(log(rig), [(Press(Nothing), 0), (Drag(Nothing), 0)]);
            assert_eq!(rig.controls.tuning().volume(), GAIN);
            assert_eq!(
                sel_of(rig).feed.settings.map(|s| s.master_volume),
                Some(skin_model::MasterVolume::of_gain(GAIN))
            );
        });
    }
}

/// P8: inside a folder, a right press on a row that is not the cursor's,
/// or a middle press on a row, goes up a place without moving to it; at
/// the root it does nothing; a right press no target takes goes up, a
/// middle one does nothing; a right press on event 12 steps the order
/// back.
#[test]
fn another_button_on_a_row_or_a_right_press_goes_back() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = tree(
        dir.path(),
        &[&["f/s1/a.bms", "f/s2/b.bms", "f/s3/c.bms", "g.bms"]],
    );
    on_pointer(sel, true, None, |rig, w, screens| {
        let c = rig.controls;
        let f = SelectionPlace::Folder(vec!["f".to_owned()]);
        let into_f = |rig: &mut PlayerRig<'_>, w: &mut Window| {
            w.key(MenuKey::Enter);
            act(rig, screens);
            assert_eq!(place(rig), f);
            assert_eq!(cursor(rig), 0);
            log(rig);
        };
        // Top: all charts, f and g.
        w.key(MenuKey::Down);
        act(rig, screens);
        into_f(rig, w);
        // Up a place: f's row under the cursor once the list is read.
        let top = (SelectionPlace::Top, 1);
        let at = |rig: &PlayerRig<'_>| (place(rig), cursor(rig));
        w.click(c, row(0, 2), Button::Right);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Key(MenuKey::Escape)), 0)]);
        assert_eq!(at(rig), top);
        into_f(rig, w);
        w.click(c, row(0, 1), Button::Middle);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Key(MenuKey::Escape)), 0)]);
        assert_eq!(at(rig), top);
        // At the root: nothing.
        w.click(c, row(1, 2), Button::Right);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Key(MenuKey::Escape)), 1)]);
        assert_eq!(at(rig), top);
        into_f(rig, w);
        w.click(c, EMPTY, Button::Right);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Key(MenuKey::Escape)), 0)]);
        assert_eq!(at(rig), top);
        into_f(rig, w);
        w.click(c, EMPTY, Button::Middle);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Nothing), 0)]);
        assert_eq!(place(rig), f);
        w.click(c, ORDER, Button::Right);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Key(MenuKey::Left)), 0)]);
        assert_eq!(sel_of(rig).screen.order(), 3);
    });
}

/// P9: a press of any button in search text 30's input box, its alignment
/// placing the box and the query empty, focuses the box; a press elsewhere
/// while focused takes the focus first and then hits, a row included.
/// While the IME composes, a press or a notch changes nothing; once the
/// composition ends, the same press moves the cursor.
#[test]
fn the_search_text_takes_focus_and_a_press_outside_takes_it_away() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = tree(dir.path(), &[&["ab1.bms", "ab2.bms", "ab3.bms", "x.bms"]]);
    on_pointer(sel, true, None, |rig, w, screens| {
        let c = rig.controls;
        let searching = |rig: &PlayerRig<'_>| sel_of(rig).screen.searching();
        w.click(c, SEARCH, Button::Right);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Key(MenuKey::Tab)), 0)]);
        assert!(searching(rig));
        w.key(MenuKey::Tab);
        act(rig, screens);
        assert!(!searching(rig));
        log(rig);
        w.click(c, SEARCH, Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Key(MenuKey::Tab)), 0)]);
        assert!(searching(rig));
        // A press in the box while it has focus: taken, nothing more.
        w.click(c, SEARCH, Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Nothing), 0)]);
        assert!(searching(rig));
        w.click(c, EMPTY, Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Nothing), 0)]);
        assert!(!searching(rig));
        w.key(MenuKey::Tab);
        act(rig, screens);
        log(rig);
        w.click(c, row(0, 2), Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(2)), 2)]);
        assert!(!searching(rig));
        // Composing "かな" after "ab".
        w.key(MenuKey::Tab);
        w.menu.push(MenuInput::Text("ab".to_owned()));
        w.menu.push(MenuInput::Preedit("かな".to_owned()));
        act(rig, screens);
        log(rig);
        assert!(sel_of(rig).screen.composing());
        // The results: ab1, ab2 and ab3; an off-centre row of them.
        assert_eq!(sel_of(rig).screen.selectable(), 3);
        let at = cursor(rig);
        let to = if at == 0 { 2 } else { 0 };
        w.click(c, SEARCH, Button::Left);
        w.click(c, row(at, to), Button::Left);
        w.wheel(c, 2);
        act(rig, screens);
        assert_eq!(
            log(rig),
            [
                (Press(Nothing), at),
                (Press(Nothing), at),
                (Notch(Nothing), at)
            ]
        );
        assert!(searching(rig));
        assert!(sel_of(rig).screen.composing());
        assert_eq!(sel_of(rig).screen.query(), "ab");
        // The composition ends: the press takes the focus and moves.
        w.menu.push(MenuInput::Preedit(String::new()));
        act(rig, screens);
        log(rig);
        w.click(c, row(at, to), Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(to)), to)]);
        assert!(!searching(rig));
    });
}

/// P13: under panel 3 only the order button acts: a press on a row, on
/// event 15, on empty space (right) or in search text 30, a notch and a
/// drag on the list slider do nothing, the search box focused before the
/// panel opened keeping its focus; event 331 and slider 17 take the press
/// over the row under them and change nothing; the hover image still
/// draws under the pointer.
#[test]
fn under_a_panel_only_the_order_button_acts() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = charts(dir.path(), 4);
    let (mut set, level) = panel_keys();
    on_pointer(sel, true, Some(level), |rig, w, screens| {
        let c = rig.controls;
        let searching = |rig: &PlayerRig<'_>| sel_of(rig).screen.searching();
        // The box focused, then SELECT held: panel 3.
        w.key(MenuKey::Tab);
        act(rig, screens);
        log(rig);
        panel_key(&mut set, 0x11, 1);
        w.click(c, EMPTY, Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Nothing), 0)]);
        assert!(searching(rig));
        w.key(MenuKey::Tab);
        act(rig, screens);
        assert!(!searching(rig));
        log(rig);
        w.click(c, row(0, 2), Button::Left);
        w.click(c, PLAY, Button::Left);
        w.click(c, EMPTY, Button::Right);
        w.click(c, SEARCH, Button::Left);
        w.wheel(c, 1);
        act(rig, screens);
        assert_eq!(
            log(rig),
            [
                (Press(Nothing), 0),
                (Press(Nothing), 0),
                (Press(Nothing), 0),
                (Press(Nothing), 0),
                (Notch(Nothing), 0),
            ]
        );
        assert!(!searching(rig));
        // A drag along the list slider.
        w.down(c, list_slider(0), Button::Left);
        act(rig, screens);
        w.to(c, list_slider(1_000));
        dragged(rig, screens);
        w.up(c, Button::Left);
        assert_eq!(log(rig), [(Press(Nothing), 0), (Drag(Nothing), 0)]);
        // The order steps.
        w.click(c, ORDER, Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Key(MenuKey::Right)), 0)]);
        assert_eq!(sel_of(rig).screen.order(), 1);
        // 331 over row 1: taken, no toggle, no move.
        w.click(c, COVER_ON_ROW, Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Nothing), 0)]);
        assert_eq!(
            sel_of(rig).feed.settings.map(|s| s.covers_on),
            Some([false; 3])
        );
        // Slider 17 over row 2 dragged to 0.3.
        w.down(c, volume_at(500), Button::Left);
        act(rig, screens);
        w.to(c, volume_at(300));
        dragged(rig, screens);
        w.up(c, Button::Left);
        assert_eq!(log(rig), [(Press(Nothing), 0), (Drag(Nothing), 0)]);
        assert_eq!(rig.controls.tuning().volume(), GAIN);
        // The hover image draws under the pointer, and not away from it.
        w.to(c, HOVER);
        frames(screens, 2);
        assert!(last_rects(screens).contains(&HOVER_RECT));
        w.to(c, EMPTY);
        frames(screens, 2);
        assert!(!last_rects(screens).contains(&HOVER_RECT));
        panel_key(&mut set, 0x11, 0);
    });
}

/// P14: on the built-in page (the skin refused at runtime), a left press
/// where a row was drawn, a right press and notches do nothing.
#[test]
fn the_built_in_page_takes_no_pointer_input() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = charts(dir.path(), 2);
    on_pointer(sel, true, None, |rig, w, screens| {
        let c = rig.controls;
        screens.refuse_selection.store(true, Ordering::Release);
        // The skin's state removed: the page draws.
        assert!(wait_until(|| {
            screens
                .selections
                .lock()
                .unwrap()
                .last()
                .is_some_and(|s| s.state.is_none())
        }));
        drawn(rig, screens);
        log(rig);
        w.click(c, row(0, 1), Button::Left);
        w.click(c, row(0, 1), Button::Right);
        w.click(c, EMPTY, Button::Right);
        w.wheel(c, 2);
        act(rig, screens);
        assert_eq!(
            log(rig),
            [
                (Press(Nothing), 0),
                (Press(Nothing), 0),
                (Press(Nothing), 0),
                (Notch(Nothing), 0),
            ]
        );
        assert_eq!(place(rig), SelectionPlace::Top);
    });
}

/// P15: moving a held button drags the topmost slider under the latest
/// point, once a frame: from empty space onto the list slider at 0.5 it
/// moves to row 1; over a row it drags nothing; a press on event 13 lying
/// over the list slider, held still for 10 frames, drags nothing, and
/// then moved to 1.0 moves to row 2; released, a move drags nothing.
#[test]
fn a_drag_moves_only_a_slider_under_its_latest_point() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = charts(dir.path(), 2);
    on_pointer(sel, true, None, |rig, w, screens| {
        let c = rig.controls;
        w.down(c, EMPTY, Button::Left);
        act(rig, screens);
        w.to(c, list_slider(500));
        dragged(rig, screens);
        w.up(c, Button::Left);
        assert_eq!(log(rig), [(Press(Nothing), 0), (Drag(MoveTo(1)), 1)]);
        // Over row 2 (drawn with the cursor on row 1).
        let [x, y] = row(1, 2);
        w.down(c, [x, y], Button::Left);
        act(rig, screens);
        w.to(c, [x - 100, y + 10]);
        w.to(c, [x - 200, y]);
        dragged(rig, screens);
        w.up(c, Button::Left);
        assert_eq!(log(rig), [(Press(MoveTo(2)), 2)]);
        w.key(MenuKey::Home);
        act(rig, screens);
        log(rig);
        // Event 13 over the list slider, held still.
        w.down(c, MODE_ON_SLIDER, Button::Left);
        act(rig, screens);
        frames(screens, 10);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Nothing), 0)]);
        w.to(c, list_slider(1_000));
        dragged(rig, screens);
        assert_eq!(log(rig), [(Drag(MoveTo(2)), 2)]);
        // Released, then moved.
        w.up(c, Button::Left);
        w.to(c, list_slider(250));
        dragged(rig, screens);
        assert_eq!(log(rig), []);
        assert_eq!(cursor(rig), 2);
    });
}

/// P16: what was observed in one selection lifetime acts in no other.
/// Lifetime 7 → 8, the render thread holding 7's frame: a left press on
/// row 3 and two notches observed under 7 reach the queue after the
/// switch and are hit-tested on 8's frame: nothing; the press observed
/// under 8 moves to row 3. Lifetime 8 → 9: a press answered on 8's frame
/// and left untaken is never dispatched; a press observed under 8 does
/// nothing in 9, answered on 8's frame or on 9's. Lifetime 9 → 10 with the list slider held
/// and no pointer event between: moving it drags nothing, a fresh press
/// there moves to row 4. A hold across a focus loss, or across a new
/// scene alone, drags nothing. While selection is not in front nothing is
/// queued.
#[test]
fn an_input_of_another_lifetime_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sel, roots) = charts(dir.path(), 4);
    sel.feed.lifetime = 6;
    on_pointer(sel, true, None, |rig, w, screens| {
        let c = rig.controls;
        let chart = roots[0].join("c1.bms");
        // Selection left for loading and entered again.
        let again = |rig: &mut PlayerRig<'_>| {
            rig.entered_loading(&chart);
            rig.entered_selection();
            quiet(rig);
        };
        let lifetime = |rig: &PlayerRig<'_>| sel_of(rig).feed.lifetime;
        let built_in = |l: u64| {
            screens
                .built
                .lock()
                .unwrap()
                .iter()
                .any(|(s, _, _)| s.lifetime == l)
        };
        assert_eq!(lifetime(rig), 7);
        let w7 = w.shared.word();
        assert_eq!(w7, 7 << 1 | 1);
        // Observed under 7, taken in 8.
        hold(screens);
        again(rig);
        assert_eq!(lifetime(rig), 8);
        w.menu.push_stamped(
            MenuInput::Pointer {
                button: Button::Left,
                at: row(0, 3),
                word: w7,
            },
            None,
        );
        w.menu.push_stamped(
            MenuInput::Wheel {
                notches: 2,
                word: w7,
            },
            None,
        );
        release(screens);
        assert!(wait_until(|| built_in(8)));
        act(rig, screens);
        assert_eq!(log(rig), [(Press(Nothing), 0), (Notch(Nothing), 0)]);
        w.click(c, row(0, 3), Button::Left);
        act(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(3)), 3)]);
        w.key(MenuKey::Home);
        act(rig, screens);
        log(rig);
        // Answered on 8's frame, taken in 9: abandoned.
        let w8 = w.shared.word();
        w.click(c, row(0, 3), Button::Left);
        assert!(rig.look().is_none());
        assert!(sel_of(rig).pointer.flight.is_some());
        frames(screens, 2);
        again(rig);
        assert_eq!(lifetime(rig), 9);
        act(rig, screens);
        assert_eq!(log(rig), []);
        assert_eq!(cursor(rig), 0);
        // A press observed under 8 answered on 8's frame and dispatched in
        // 9, then one observed under 8 answered on 9's frame, as the
        // render thread would answer them.
        hold(screens);
        for answered in [8, 9] {
            let s = sel_mut(rig);
            let list_revision = s.feed.list_revision;
            let mut request = Some(PointerRequest::Press {
                word: w8,
                button: Button::Left,
                at: row(0, 3),
            });
            s.pointer.flight = s.pointer.shared.submit(9, || request.take());
            let built = Built {
                lifetime: answered,
                revision: 0,
                list_revision,
                cursor: Some(0),
                skinned: true,
            };
            assert!(s.pointer.shared.answer(Some(built), |_| {
                Some(skin_model::PointerHit::Row {
                    row: RowRef {
                        list_revision,
                        index: 3,
                        kind: RowRefKind::Chart,
                    },
                    button: Button::Left,
                })
            }));
            assert_eq!(take_all(rig), None);
            assert_eq!(log(rig), [(Press(Nothing), 0)], "answered on {answered}");
        }
        release(screens);
        drawn(rig, screens);
        // A hold of 9, moved in 10 with no pointer event between.
        w.down(c, list_slider(0), Button::Left);
        act(rig, screens);
        w.to(c, list_slider(250));
        dragged(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(0)), 0), (Drag(MoveTo(1)), 1)]);
        w.key(MenuKey::Home);
        act(rig, screens);
        log(rig);
        again(rig);
        assert_eq!(lifetime(rig), 10);
        drawn(rig, screens);
        w.to(c, list_slider(1_000));
        dragged(rig, screens);
        assert_eq!(log(rig), []);
        w.down(c, list_slider(1_000), Button::Left);
        act(rig, screens);
        w.up(c, Button::Left);
        assert_eq!(log(rig), [(Press(MoveTo(4)), 4)]);
        w.key(MenuKey::Home);
        act(rig, screens);
        log(rig);
        // Across a focus loss, as the window sees one.
        w.down(c, list_slider(0), Button::Left);
        act(rig, screens);
        w.capture.focus_lost(&w.shared);
        c.focus(false);
        c.focus(true);
        w.to(c, list_slider(1_000));
        dragged(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(0)), 0)]);
        // Across a new scene alone.
        w.down(c, list_slider(0), Button::Left);
        act(rig, screens);
        c.next_scene();
        w.to(c, list_slider(1_000));
        dragged(rig, screens);
        assert_eq!(log(rig), [(Press(MoveTo(0)), 0)]);
        // Selection not in front: nothing queued.
        rig.entered_loading(&chart);
        w.click(c, row(0, 1), Button::Left);
        w.wheel(c, 1);
        assert_eq!(w.menu.queued(), 0);
        assert_eq!(w.shared.dropped(), 0);
    });
}

/// P17: presses and notches are dispatched once each, in the order
/// observed among themselves and the keys, whichever thread is held; one
/// past 64 queued is refused and counted; drags take no press room, come
/// once a frame and never before the press that began their hold; one
/// pass takes at most 128 entries.
#[test]
fn the_pointer_keeps_its_order_and_its_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = charts(dir.path(), 9);
    on_pointer(sel, true, None, |rig, w, screens| {
        let c = rig.controls;
        assert_eq!(sel_of(rig).screen.selectable(), 10);
        let cursors = |log: &[(Dispatched, usize)]| log.iter().map(|&(_, c)| c).collect::<Vec<_>>();
        // The render thread held: a press on row 3, Down, Down, a press on
        // row 2 and Up. The second press is answered on the held frame
        // (the cursor on row 0), where row 2 is drawn.
        hold(screens);
        w.click(c, row(0, 3), Button::Left);
        w.key(MenuKey::Down);
        w.key(MenuKey::Down);
        w.click(c, row(0, 2), Button::Left);
        w.key(MenuKey::Up);
        assert!(rig.look().is_none());
        grant(screens, 1);
        assert!(rig.look().is_none());
        grant(screens, 1);
        assert_eq!(take_all(rig), None);
        let l = log(rig);
        assert_eq!(cursors(&l), [3, 4, 5, 2, 1], "{l:?}");
        assert_eq!(cursor(rig), 1);
        release(screens);
        drawn(rig, screens);
        w.key(MenuKey::Home);
        act(rig, screens);
        log(rig);
        // Several batches: 40 presses in flight, Down, 40 more.
        hold(screens);
        for i in 0..40 {
            w.click(c, row(0, if i % 2 == 0 { 3 } else { 2 }), Button::Left);
        }
        w.key(MenuKey::Down);
        assert!(rig.look().is_none());
        assert!(sel_of(rig).pointer.flight.is_some());
        for i in 0..40 {
            w.click(c, row(0, if i % 2 == 0 { 2 } else { 1 }), Button::Left);
        }
        grant(screens, 1);
        assert!(rig.look().is_none());
        grant(screens, 1);
        assert_eq!(take_all(rig), None);
        let want: Vec<(Dispatched, usize)> = (0..40)
            .map(|i| if i % 2 == 0 { 3 } else { 2 })
            .map(|r| (Press(MoveTo(r)), r))
            .chain([(Input, 3)])
            .chain(
                (0..40)
                    .map(|i| if i % 2 == 0 { 2 } else { 1 })
                    .map(|r| (Press(MoveTo(r)), r)),
            )
            .collect();
        assert_eq!(log(rig), want);
        assert_eq!(w.shared.dropped(), 0);
        release(screens);
        drawn(rig, screens);
        w.key(MenuKey::Home);
        act(rig, screens);
        log(rig);
        // The game thread held: 65 presses, the last refused.
        let rows: Vec<usize> = (0..65).map(|i| i % 3 + 1).collect();
        for &r in &rows {
            w.click(c, row(0, r), Button::Left);
        }
        assert_eq!(w.shared.dropped(), 1);
        act(rig, screens);
        let want: Vec<(Dispatched, usize)> =
            rows[..64].iter().map(|&r| (Press(MoveTo(r)), r)).collect();
        assert_eq!(log(rig), want);
        assert_eq!(sel_of(rig).dropped_pointer, 1);
        w.key(MenuKey::Home);
        act(rig, screens);
        log(rig);
        // Drags competing: the list slider held and moved 50 times a frame
        // for 5 frames, 60 middle presses on the order button queued.
        hold(screens);
        w.down(c, list_slider(0), Button::Left);
        let word = w.shared.word();
        for _ in 0..60 {
            assert!(
                w.menu
                    .push_stamped(
                        MenuInput::Pointer {
                            button: Button::Middle,
                            at: ORDER,
                            word,
                        },
                        None,
                    )
                    .is_some()
            );
        }
        for _ in 0..5 {
            for k in 1..=50 {
                w.to(c, list_slider(20 * k));
            }
            assert!(rig.look().is_none());
            grant(screens, 1);
        }
        assert_eq!(take_all(rig), None);
        w.up(c, Button::Left);
        let l = log(rig);
        let steps = l
            .iter()
            .filter(|(d, _)| *d == Press(Key(MenuKey::Right)))
            .count();
        let drags: Vec<usize> = (0..l.len())
            .filter(|&i| matches!(l[i].0, Drag(_)))
            .collect();
        assert_eq!(steps, 60, "{l:?}");
        assert!((1..=5).contains(&drags.len()), "{l:?}");
        assert_eq!(l[0], (Press(MoveTo(0)), 0), "{l:?}");
        assert!(drags[0] > 0);
        assert_eq!(w.shared.dropped(), 1);
        assert_eq!(sel_of(rig).screen.order(), 0);
        release(screens);
        drawn(rig, screens);
        // One pass takes 128 entries: 65 one-letter texts alternating
        // with 64 keys (Down, which the search box ignores).
        w.key(MenuKey::Tab);
        act(rig, screens);
        log(rig);
        let letter = |i: u8| char::from(b'a' + i % 26).to_string();
        for i in 0..65 {
            w.menu.push(MenuInput::Text(letter(i)));
            if i < 64 {
                w.key(MenuKey::Down);
            }
        }
        assert!(rig.look().is_none());
        let first: String = (0..64).map(letter).collect();
        assert_eq!(log(rig).len(), 128);
        assert_eq!(sel_of(rig).screen.query(), first);
        assert!(rig.look().is_none());
        assert_eq!(log(rig), [(Input, cursor(rig))]);
        assert_eq!(sel_of(rig).screen.query(), format!("{first}{}", letter(64)));
        assert_eq!(w.menu.dropped(), 0);
    });
}

/// P12: the game thread's side of the pointer allocates nothing: 64
/// presses admitted into the menu queue's reserved storage and taken as
/// one batch, answered by `hit_at` on folder and chart rows whose paths
/// are 260 bytes, dispatched, and 50 drags written to the drag slot, one
/// applied; the queue's storage stays as it was reserved.
#[test]
fn pointer_dispatch_alloc() {
    use crate::pointer::{DragCommand, PointerSlot};
    use skin_model::{FolderId, RowKey};

    let counted = |f: &mut dyn FnMut()| {
        let before = crate::alloc_count::this_thread();
        f();
        crate::alloc_count::this_thread() - before
    };
    // The positive control: the counter sees this thread's allocation.
    assert!(
        counted(&mut || {
            std::hint::black_box(Vec::<u8>::with_capacity(16));
        }) > 0
    );
    let dir = tempfile::tempdir().unwrap();
    let (mut sel, _) = tree(
        dir.path(),
        &[&[
            "f0/s/a.bms",
            "f1/s/b.bms",
            "f2/s/c.bms",
            "d.bms",
            "e.bms",
            "g.bms",
            "h.bms",
        ]],
    );
    // Top: all charts (0), folders f0 to f2 (1 to 3), charts d to h (4 to
    // 7); the cursor on f2, drawn at slot 3.
    sel.started(None, Some((3, 7)));
    sel.feed.enter(TimeUs(0));
    let lifetime = sel.feed.lifetime;
    sel.pointer.enter(lifetime);
    assert!(sel.screen.move_to(3));
    let page = sel.screen.page("");
    let mut u = sel.feed.update(&sel.screen, page, TimeUs(0));
    let long = |i: usize| std::path::PathBuf::from(format!("{}{i:03}", "p".repeat(257)));
    for (i, r) in u.view.rows.iter_mut().enumerate() {
        assert_eq!(long(i).as_os_str().len(), 260);
        r.key = match &r.key {
            RowKey::Chart(_) => RowKey::Chart(long(i)),
            RowKey::Folder(_) => RowKey::Folder(FolderId::new(long(i))),
            other => other.clone(),
        };
    }
    let fixture = crate::selection_skin::pointer_fixture(true);
    let limits = skin_model::AtlasLimits {
        max_side: 4_096,
        max_layers: 16,
        reserved_layers: render::RESERVED_LAYERS,
    };
    let (bundle, _) = crate::selection_skin::fixture_scripted(limits, fixture.path(), "s.json");
    let mut logic = crate::selection_skin::SelectionLogic::new(&bundle, 1);
    let skin = Arc::clone(bundle.skin());
    skin.update_selection(&mut logic.sel, &u.view, &u.labels)
        .unwrap();
    let target = [1_920, 1_080];
    let mut quads = skin_model::QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let frame = skin_model::SelectionFrame {
        now: TimeUs(0),
        target,
        layout: &skin_model::Layout::AUTHOR,
        timers: u.events.timers(),
        app: skin_model::AppReadouts::LAUNCH,
        pointer: None,
    };
    skin.build_selection(&mut logic.sel, &frame, bundle.uvs(), &mut quads)
        .unwrap();
    // Slot 1 draws folder f0, slot 5 chart e.
    let row_hit = |at: [i32; 2], index: u32, kind| {
        assert_eq!(
            skin.hit_at(&logic.sel, crate::pointer::point(at), target, Button::Left),
            Some(skin_model::PointerHit::Row {
                row: RowRef {
                    list_revision: u.list_revision,
                    index,
                    kind,
                },
                button: Button::Left,
            })
        );
    };
    row_hit(slot(1), 1, RowRefKind::Folder);
    row_hit(slot(5), 5, RowRefKind::Chart);
    let built = Built {
        lifetime,
        revision: u.revision,
        list_revision: u.list_revision,
        cursor: Some(3),
        skinned: true,
    };
    let shared = Arc::clone(&sel.pointer.shared);
    let menu = Arc::clone(&sel.menu);
    let word = shared.word();
    let mut capture = Capture::default();
    let mut seen = shared.drag_now().sequence;
    // The hold the drags belong to, begun on the list slider and
    // dispatched.
    capture.moved(&shared, list_slider(0), word, 0);
    capture.button(&shared, Button::Left, true, word, 0, |i| {
        menu.push_stamped(i, None)
    });
    assert!(sel.pass(None, TimeUs(0)).is_none());
    shared.serve(Some(built), Some(&logic), target, &mut seen);
    assert!(sel.pass(None, TimeUs(0)).is_none());
    assert!(sel.pointer.flight.is_none());
    let capacity = menu.capacity();
    let round = |sel: &mut Selection, capture: &mut Capture, seen: &mut u64| {
        for k in 0..64 {
            let at = if k % 2 == 0 { slot(1) } else { slot(5) };
            assert!(
                menu.push_stamped(
                    MenuInput::Pointer {
                        button: Button::Left,
                        at,
                        word,
                    },
                    None,
                )
                .is_some()
            );
        }
        assert!(sel.pass(None, TimeUs(0)).is_none());
        assert!(sel.pointer.flight.is_some());
        shared.serve(Some(built), Some(&logic), target, seen);
        for k in 1..=50 {
            capture.moved(&shared, list_slider(20 * k), word, 0);
            shared.serve(Some(built), Some(&logic), target, seen);
        }
        assert!(sel.pass(None, TimeUs(0)).is_none());
    };
    // Once to warm, then counted.
    round(&mut sel, &mut capture, &mut seen);
    let taken = menu.presses_taken();
    let n = counted(&mut || round(&mut sel, &mut capture, &mut seen));
    assert_eq!(n, 0);
    // Every press dispatched (the last on chart e), then the drag to the
    // list slider's end: the last row.
    assert_eq!(menu.presses_taken(), taken + 64);
    assert_eq!((menu.queued(), sel.pointer.flight), (0, None));
    assert_eq!(sel.screen.cursor(), 7);
    assert_eq!(menu.capacity(), capacity);
    assert_eq!(size_of::<RowRef>(), 16);
    assert!(
        size_of::<PointerSlot>() <= 64,
        "{}",
        size_of::<PointerSlot>()
    );
    assert!(
        size_of::<DragCommand>() <= 96,
        "{}",
        size_of::<DragCommand>()
    );
}
