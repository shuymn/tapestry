//! A selection skin on a WARP menu screen at set times (dev-tools): the
//! strip's search line, composition and caret and its notice, nothing
//! over the skin otherwise, a preset step included; a frame's app work
//! allocating nothing; and the cost of a cursor move's text.

use std::time::{Duration, Instant};

use bms_model::TimeUs;
use render::text::Fonts;
use skin_model::{RowKey, RowKind, RowScore, RowView, ScreenKind, SelectionPlace, SelectionView};

use super::{MenuScreen, Sent, searching, warp};
use crate::render_side::Screen;
use crate::select::SelectScreen;
use crate::selection_skin::{SelectionLogic, SelectionUpdate, SkinFeed};

/// The application's readouts a frame is given: 59 presents at
/// 2026-10-12 21:47:58, 1 h 2 min 5 s after launch.
const APP: skin_model::AppReadouts = skin_model::AppReadouts {
    fps: 59,
    uptime_us: 3_725_999_000,
    local: skin_model::LocalTime {
        year: 2_026,
        month: 10,
        day: 12,
        hour: 21,
        minute: 47,
        second: 58,
    },
};

/// The readouts of frame `k`: each part moving with it.
fn app(k: i64) -> skin_model::AppReadouts {
    let k = u8::try_from(k.rem_euclid(60)).unwrap();
    skin_model::AppReadouts {
        fps: u16::from(k) * 17,
        uptime_us: APP.uptime_us + i64::from(k) * 16_667,
        local: skin_model::LocalTime {
            month: 1 + k % 12,
            minute: k,
            second: 59 - k,
            ..APP.local
        },
    }
}

/// A WARP screen with the fixture selection skin committed, its skin
/// state, and what it sent the window.
fn skinned() -> (tempfile::TempDir, MenuScreen, SelectionLogic, Sent) {
    skinned_on(warp())
}

/// [`skinned`] on `r`.
fn skinned_on(r: render::Renderer) -> (tempfile::TempDir, MenuScreen, SelectionLogic, Sent) {
    skinned_with(r, crate::selection_skin::fixture(), "s.json")
}

/// [`skinned_on`] with the skin `entry` of `dir`.
fn skinned_with(
    mut r: render::Renderer,
    dir: tempfile::TempDir,
    entry: &str,
) -> (tempfile::TempDir, MenuScreen, SelectionLogic, Sent) {
    let b = crate::selection_skin::fixture_bundle(&r, dir.path(), entry);
    let token = r.install(ScreenKind::Selection, &b, 1).unwrap();
    r.commit(token).unwrap();
    let sent = Sent::default();
    let screen =
        MenuScreen::new(r, Some(Fonts::runtime().unwrap())).with_sink(Box::new(sent.clone()));
    (dir, screen, SelectionLogic::new(&b, 1), sent)
}

/// The update of `screen` at `now` through a feed for the fixture's list.
fn update(feed: &mut SkinFeed, screen: &SelectScreen, now: i64) -> SelectionUpdate {
    feed.window = Some((3, 7));
    let page = screen.page("");
    feed.update(screen, page, TimeUs(now))
}

/// The quads of the selection frame of `u` drawn at `now`.
fn drawn(
    screen: &mut MenuScreen,
    logic: &mut SelectionLogic,
    u: &SelectionUpdate,
    changed: bool,
    now: i64,
) -> usize {
    let d = screen
        .selection(u, Some(logic), changed, TimeUs(now), APP)
        .unwrap();
    assert_eq!(d.refused, None);
    screen.renderer.selection_quads().unwrap().len()
}

/// While the search box has focus, the strip shows the query and the
/// composition, underlined, and sends the caret's area on its line; the
/// skin's search text shows the committed query; a notice adds its line.
/// The strip stages a band per line and the underline: 1, 2, then 3, and
/// none once focus is gone.
#[test]
fn the_strip_shows_the_search_and_its_composition_over_the_skin() {
    let (_dir, mut screen, mut logic, sent) = skinned();
    let mut feed = SkinFeed::default();
    feed.enter(TimeUs(0));
    let typed = searching("abc", "");
    let plain = drawn(
        &mut screen,
        &mut logic,
        &update(&mut feed, &typed, 0),
        true,
        0,
    );
    assert_eq!(screen.selection_strip_staged(), 1, "the search line's band");
    let composing = searching("abc", "かな");
    let u = update(&mut feed, &composing, 0);
    let with = drawn(&mut screen, &mut logic, &u, true, 0);
    // The underline, one sprite more.
    assert_eq!(with, plain + 1);
    assert_eq!(screen.selection_strip_staged(), 2);
    let (asked, _) = screen.renderer.selection_requests();
    assert!(asked.iter().any(|a| a == "abc"), "{asked:?}");
    let area = sent.0.lock().unwrap().last().copied().flatten().unwrap();
    let [x, y, w, h] = area;
    assert_eq!((y, h), (super::line_top(0), super::LINE_STEP));
    assert!(x > super::super::LEFT && w > 30.0, "{area:?}");
    let mut noticed = searching("abc", "かな");
    noticed.set_notice(Some("refused".to_owned()));
    let u = update(&mut feed, &noticed, 0);
    assert_eq!(drawn(&mut screen, &mut logic, &u, true, 0), with + 1);
    assert_eq!(screen.selection_strip_staged(), 3);
    // Focus gone: no caret, no strip.
    let mut idle = searching("abc", "");
    idle.input(crate::select::MenuInput::Key(crate::select::MenuKey::Tab));
    assert!(!idle.searching());
    let u = update(&mut feed, &idle, 0);
    assert_eq!(drawn(&mut screen, &mut logic, &u, true, 0), plain - 1);
    assert_eq!(screen.selection_strip_staged(), 0);
    assert_eq!(screen.selection_strip(), (Vec::new(), false));
    assert_eq!(sent.0.lock().unwrap().last(), Some(&None));
}

/// A refused frame with the search box focused draws the page prepared
/// with the update, and sends the caret's area on the page's search line,
/// not the strip's.
#[test]
fn a_refused_frame_sends_the_pages_caret() {
    for text in [false, true] {
        let (_dir, mut screen, mut logic, sent) = skinned();
        let mut feed = SkinFeed::default();
        feed.enter(TimeUs(0));
        let u = update(&mut feed, &searching("abc", "かな"), 0);
        drawn(&mut screen, &mut logic, &u, true, 0);
        let strip = sent.0.lock().unwrap().last().copied().flatten().unwrap();
        assert_eq!(strip[1], super::line_top(0));
        let line = u.page.caret.unwrap().line;
        assert_ne!(line, 0);
        screen.renderer.refuse_next_selection(text);
        let d = screen
            .selection(&u, Some(&mut logic), text, TimeUs(0), APP)
            .unwrap();
        assert!(d.refused.is_some(), "{text}");
        let page = sent.0.lock().unwrap().last().copied().flatten().unwrap();
        assert_eq!(page[1], super::line_top(line), "{text}: {page:?}");
        assert!(page[2] > 30.0, "{text}: {page:?}");
    }
}

/// A selection frame submits the skin's frame from the selection
/// installation's own staging, every quad it built, and the strip's
/// text over it: the notice's words change the pixels drawn, its band
/// staying.
#[test]
fn a_selection_frame_draws_the_skins_staging_and_the_strips_text() {
    let (_dir, mut screen, mut logic, _) = skinned();
    let mut feed = SkinFeed::default();
    feed.enter(TimeUs(0));
    let mut s = SelectScreen::new(Vec::new());
    s.set_notice(Some("ベータ".to_owned()));
    let t = 5_000_000;
    let first = update(&mut feed, &s, t);
    let d = screen
        .selection(&first, Some(&mut logic), true, TimeUs(t), APP)
        .unwrap();
    let quads = screen.renderer.selection_quads().unwrap().len();
    assert!(quads > 0);
    assert_eq!(d.timing.sprites, quads);
    let with_first = screen.read_back().unwrap();
    s.set_notice(Some("Alpha".to_owned()));
    let second = update(&mut feed, &s, t);
    screen
        .selection(&second, Some(&mut logic), true, TimeUs(t), APP)
        .unwrap();
    let with_second = screen.read_back().unwrap();
    assert_eq!(screen.renderer.selection_quads().unwrap().len(), quads);
    assert_ne!(with_first.2, with_second.2);
}

/// A preset step the flow takes draws nothing over the skin: the update
/// after it carries no strip text, and its frames at the step's time
/// `t`, `t + 1 000 µs` and `t + 2 999 999 µs` stage no strip sprite and
/// submit no strip text, the chosen preset having changed.
#[test]
fn a_preset_step_draws_nothing_over_the_skin() {
    use bms_model::TimeSource as _;
    use game::AppRig as _;
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = crate::selection::tests::tree(dir.path(), &[&["a.bms"]]);
    let presets = dir.path().join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    let mut shelf = game::Shelf::open(presets, "tapestry").unwrap();
    let screens = crate::render_side::tests::Installers::default();
    let render = screens.spawn_bare();
    let (u, t) = crate::selection::tests::on_selection(sel, &screens, render, |rig| {
        let latest = || screens.updates.lock().unwrap().last().cloned();
        assert!(crate::render_side::tests::wait_until(|| latest().is_some()));
        let revision = latest().unwrap().revision;
        rig.selection
            .as_mut()
            .unwrap()
            .preset_steps
            .push_back(game::PresetStep::Next);
        let step = rig.take_preset_step().unwrap();
        let t = rig.clock.now();
        shelf.step(step);
        rig.presets_changed(Ok(&shelf));
        assert!(rig.choose().is_none());
        assert!(crate::render_side::tests::wait_until(|| latest()
            .unwrap()
            .revision
            > revision));
        (latest().unwrap(), t)
    });
    assert_ne!(shelf.chosen_name(), "tapestry");
    assert_eq!(u.strip, crate::selection_skin::StripText::default());
    let (_dir, mut screen, mut logic, _) = skinned();
    for (k, at) in [t.0, t.0 + 1_000, t.0 + 2_999_999].into_iter().enumerate() {
        drawn(&mut screen, &mut logic, &u, k == 0, at);
        assert_eq!(screen.selection_strip_staged(), 0, "{at}");
        assert_eq!(screen.selection_strip(), (Vec::new(), false), "{at}");
    }
}

/// A preset step the flow takes gives the next update the newly chosen
/// preset's displayed gauge, as an attempt resolves it.
#[test]
fn a_preset_step_gives_the_update_the_new_presets_gauge() {
    use game::AppRig as _;
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = crate::selection::tests::tree(dir.path(), &[&["a.bms"]]);
    let presets = dir.path().join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    let mut shelf = game::Shelf::open(presets, "tapestry").unwrap();
    let screens = crate::render_side::tests::Installers::default();
    let render = screens.spawn_bare();
    let gauge_of = |p: &rules::RulePreset, displayed: usize| {
        let policy = game::DefaultPolicy {
            displayed,
            on_failure: game::FailureAction::default(),
        };
        skin_model::GaugeLook::of(&p.gauges[game::resolve_displayed(&policy, p).0])
    };
    let (first, stepped, displayed) =
        crate::selection::tests::on_selection(sel, &screens, render, |rig| {
            // The second gauge shows: the tapestry preset's survival gauge,
            // the next preset's assist-easy one.
            rig.gauge = 1;
            rig.presets_changed(Ok(&shelf));
            let latest = || screens.updates.lock().unwrap().last().cloned().unwrap();
            assert!(crate::render_side::tests::wait_until(|| latest()
                .settings
                .is_some_and(|s| s.gauge.is_some())));
            let first = latest();
            rig.selection
                .as_mut()
                .unwrap()
                .preset_steps
                .push_back(game::PresetStep::Next);
            let step = rig.take_preset_step().unwrap();
            shelf.step(step);
            rig.presets_changed(Ok(&shelf));
            let revision = first.revision;
            assert!(crate::render_side::tests::wait_until(
                || latest().revision > revision
            ));
            (first, latest(), rig.gauge)
        });
    assert_ne!(shelf.chosen_name(), "tapestry");
    let was = gauge_of(&rules::tapestry(), displayed);
    let now = gauge_of(shelf.chosen(), displayed);
    assert_eq!(first.settings.unwrap().gauge, Some(was));
    assert_eq!(stepped.settings.unwrap().gauge, Some(now));
    assert_ne!(was, now, "the step must change the gauge shown");
}

/// The memory witness counts the selection screen's lines held in its
/// text layer and the skin's state its owner keeps beside the renderer
/// (design §6): the lines once prepared, none once the installation is
/// removed; the state as it reports itself, only when given.
#[test]
fn the_memory_witness_counts_the_selection_lines_and_state() {
    let (_dir, mut screen, mut logic, _) = skinned();
    let before = screen.renderer.memory_with(Some(&logic.sel));
    assert_eq!(before.selection_text, 0);
    assert_eq!(before.selection_state, logic.sel.resident_bytes());
    let mut feed = SkinFeed::default();
    feed.enter(TimeUs(0));
    let s = SelectScreen::new(Vec::new());
    let mut u = update(&mut feed, &s, 0);
    u.view = rows_at(3, 10);
    drawn(&mut screen, &mut logic, &u, true, 0);
    let without = screen.renderer.memory();
    let with = screen.renderer.memory_with(Some(&logic.sel));
    assert!(with.selection_text > 0, "{with:?}");
    assert_eq!(without.selection_text, with.selection_text);
    assert!(with.selection_state > 0);
    assert_eq!(with.selection_state, logic.sel.resident_bytes());
    assert_eq!(without.selection_state, 0);
    assert_eq!(with.total(), without.total() + with.selection_state);
    screen.renderer.remove(ScreenKind::Selection);
    assert_eq!(screen.renderer.memory().selection_text, 0);
}

/// A selection frame's app work (the readouts and timers, the skin's
/// build and the strip's sprites, all three shown) allocates nothing,
/// the readouts moving every frame: the first build after the update's
/// preparation, each later one, the first after a resize prepared
/// outside the count, and refused, with the fallback page's own build;
/// each is counted apart from the GPU submission that follows it. An
/// allocation put into that work (the probe) is counted. The skin draws
/// the chart's stage file and banner, its table changing between frames
/// (none, the images, absent).
#[test]
fn selection_frame_alloc() {
    let _warp = crate::warp_turn::take();
    let (_dir, mut screen, mut logic, _) = skinned_with(
        warp(),
        crate::selection_skin::chart_fixture(),
        "chart.luaskin",
    );
    let tables = chart_tables(&mut screen, &logic);
    let mut feed = SkinFeed::default();
    feed.enter(TimeUs(0));
    let mut s = searching("abc", "かな");
    s.set_notice(Some("refused".to_owned()));
    // The settings, applied each frame with the settings keys' values.
    let tuning = crate::tuning::LiveTuning::new(game::Tuning {
        system: game::Volume {
            percent: 100,
            intent: false,
        },
        key: game::Volume {
            percent: 50,
            intent: false,
        },
        background: game::Volume {
            percent: 50,
            intent: false,
        },
        display_offset_us: 0,
        input_offset_us: 0,
    });
    feed.settings = crate::selection_skin::settings_of(
        Some(game::PlayerSettings::default().options().unwrap()),
        Some(&rules::tapestry()),
        0,
        game::FailureAction::PlayToEnd,
        s.order_view(),
        &tuning,
    );
    let u = update(&mut feed, &s, 0);
    assert!(u.settings.is_some());
    let counted = |f: &mut dyn FnMut()| {
        let before = crate::alloc_count::this_thread();
        f();
        crate::alloc_count::this_thread() - before
    };
    let build = |screen: &mut MenuScreen, logic: &mut SelectionLogic, k: i64| {
        // F4 and F6 between frames.
        if k % 2 == 0 {
            tuning.adjust(game::Adjust::VolumeUp);
        } else {
            tuning.adjust(game::Adjust::DisplayOffsetUp);
        }
        let n = counted(&mut || {
            logic.apply_settings(u.settings.as_ref(), Some(&tuning));
            logic.sel.set_chart_images(chart_table(&tables, k));
            logic.sel.write_chart_fields();
            screen
                .build_selection_frame(&u, logic, TimeUs(k * 16_667), app(k))
                .unwrap();
        });
        screen.renderer.submit_selection(&[]).unwrap();
        n
    };
    let prepare = |screen: &mut MenuScreen, logic: &mut SelectionLogic, changed: bool| {
        screen
            .prepare_selection(&u, logic, changed)
            .unwrap()
            .unwrap();
    };
    prepare(&mut screen, &mut logic, true);
    assert_eq!(build(&mut screen, &mut logic, 0), 0, "first");
    // The search line, the notice and the underline: every strip sprite.
    assert_eq!(screen.selection_strip_staged(), 3);
    const { assert!(render::STRIP_SPRITES == 3) };
    for k in 1..100 {
        assert_eq!(build(&mut screen, &mut logic, k), 0, "{k}");
        let q = screen.renderer.selection_quads().unwrap();
        assert_eq!(
            has(&q, crate::selection_skin::STAGE_DST, None),
            k % 3 == 1,
            "{k}: the stage file drawn"
        );
    }
    screen.resize(1_280, 720);
    prepare(&mut screen, &mut logic, false);
    assert_eq!(build(&mut screen, &mut logic, 0), 0, "after the resize");
    // Refused: the skin's build, then the page's own.
    screen.renderer.refuse_next_selection(false);
    let mut refused = Ok(());
    let n = counted(&mut || refused = screen.build_selection_frame(&u, &mut logic, TimeUs(0), APP));
    assert!(refused.is_err());
    assert_eq!(n, 0, "refused");
    let mut start = None;
    let n = counted(&mut || start = Some(screen.build_page_frame()));
    assert_eq!(n, 0, "the page");
    let text = screen.text.as_ref().unwrap();
    screen.renderer.submit_menu(start.unwrap(), text).unwrap();
    // The positive control: the same work with an allocation in it.
    screen.alloc_probe = true;
    assert!(build(&mut screen, &mut logic, 0) > 0);
    assert!(
        counted(&mut || {
            screen.build_page_frame();
        }) > 0
    );
}

/// The rows a cursor at `at` shows: seven around it of `n`, each a long
/// title of its own.
fn rows_at(at: usize, n: usize) -> SelectionView {
    let kana: Vec<char> = ('あ'..='ん').collect();
    let title = |i: usize| -> String {
        (0..48)
            .map(|k| kana[(i * 7 + k * 3) % kana.len()])
            .collect()
    };
    let start = at.saturating_sub(3);
    let end = (at + 4).min(n);
    let rows = (start..end)
        .map(|i| RowView {
            key: RowKey::Chart(std::path::PathBuf::from(format!("{i}.bms"))),
            kind: RowKind::Chart,
            index: None,
            title: title(i),
            subtitle: String::new(),
            artist: String::new(),
            level: String::new(),
            level_value: 0,
            difficulty: None,
            score: RowScore::Unplayed,
            features: None,
            random: false,
        })
        .collect();
    SelectionView {
        rows,
        cursor: Some(at - start),
        position: None,
        place: SelectionPlace::All,
        query: String::new(),
        selected: None,
        list_revision: 0,
    }
}

/// The text preparation of a cursor move (design §6, C19's gate): each
/// move down a list of long titles prepares the skin's lines again, the
/// rows not cached rasterized. Prints the slowest and the mean move; a
/// measurement, run by hand in release.
#[test]
#[ignore = "a measurement, run by hand"]
fn measure_a_cursor_moves_text_preparation() {
    for adapter in [
        render::AdapterChoice::Warp,
        render::AdapterChoice::HighPerformance,
    ] {
        println!("{adapter:?}");
        measure_on(adapter);
    }
}

fn measure_on(adapter: render::AdapterChoice) {
    let target = render::Target::Offscreen {
        width: 1_920,
        height: 1_080,
    };
    let (_dir, mut screen, mut logic, _) =
        skinned_on(render::Renderer::new(target, adapter).unwrap());
    let mut feed = SkinFeed::default();
    feed.enter(TimeUs(0));
    let s = SelectScreen::new(Vec::new());
    let mut u = update(&mut feed, &s, 0);
    let mut times = Vec::new();
    let mut skin_times = Vec::new();
    let mut rasterized = Vec::new();
    for at in 0..120 {
        u.revision += 1;
        u.view = rows_at(at, 120);
        // The fallback page and the strip, laid out with glyphon.
        let begun = Instant::now();
        screen.prepare_page(&u.page, 60.0, 30.0).unwrap();
        screen.prepare_strip(&u.strip, None).unwrap();
        times.push(begun.elapsed());
        // The skin's lines: the rows not cached rasterized and uploaded.
        let skin = std::sync::Arc::clone(logic.bundle.skin());
        skin.update_selection(&mut logic.sel, &u.view, &u.labels)
            .unwrap();
        let begun = Instant::now();
        screen
            .renderer
            .prepare_selection_text(&mut logic.sel)
            .unwrap();
        skin_times.push(begun.elapsed());
        rasterized.push(screen.renderer.selection_requests().1);
    }
    let stats = |t: &[Duration]| {
        let max = t.iter().max().copied().unwrap_or(Duration::ZERO);
        let mean = t.iter().sum::<Duration>() / u32::try_from(t.len()).unwrap();
        (max, mean)
    };
    println!(
        "selection text (max, mean) per move: page and strip {:?}; the skin's lines {:?} \
         (first {:?})",
        stats(&times[1..]),
        stats(&skin_times[1..]),
        skin_times[0],
    );
    println!("rasterized per move {:?}", &rasterized[1..8]);
}

/// A selection frame's build allocates nothing with the skin's runtime
/// present and running each frame before it, outside the counted build
/// (docs/stage3.md S7-07; design.md "Scope of No Allocation"): the
/// skin's callbacks read the chart image options, and it draws the stage
/// file and banner, their table changing between frames.
#[test]
fn selection_runtime_frame_alloc() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let _untimed = crate::selection_skin::untimed();
    let mut r = warp();
    let dir = crate::selection_skin::chart_fixture();
    let (b, script) =
        crate::selection_skin::fixture_scripted(r.atlas_limits(), dir.path(), "chart.luaskin");
    let token = r.install(ScreenKind::Selection, &b, 1).unwrap();
    r.commit(token).unwrap();
    let key = std::sync::Arc::as_ptr(&b).addr();
    let scripts = crate::screens::Scripts::new();
    scripts.commit(ScreenKind::Selection, key, script);
    let mut screen = MenuScreen::new(r, Some(Fonts::runtime().unwrap()));
    let mut logic =
        SelectionLogic::new(&b, 1).with_script(scripts.slot(ScreenKind::Selection, key));
    let mut feed = SkinFeed::default();
    feed.enter(TimeUs(0));
    let u = update(&mut feed, &searching("", ""), 0);
    let tables = chart_tables(&mut screen, &logic);
    screen
        .prepare_selection(&u, &mut logic, true)
        .unwrap()
        .unwrap();
    for k in 0..50 {
        let now = TimeUs(k * 16_667);
        logic.sel.set_chart_images(chart_table(&tables, k));
        crate::screens::selection_visit(
            &mut logic.script,
            logic.bundle.skin(),
            &mut logic.sel,
            now,
            &u.events.timers(),
            &APP,
        )
        .unwrap();
        let before = crate::alloc_count::this_thread();
        screen
            .build_selection_frame(&u, &mut logic, now, APP)
            .unwrap();
        assert_eq!(crate::alloc_count::this_thread() - before, 0, "frame {k}");
        let q = screen.renderer.selection_quads().unwrap();
        // The callbacks read 191 as the frame draws the stage file.
        let drawn = k % 3 == 1;
        assert_eq!(
            has(&q, crate::selection_skin::STAGE_DST, None),
            drawn,
            "{k}"
        );
        assert_eq!(
            has(&q, crate::selection_skin::chart_mark(1, 1), None),
            drawn,
            "{k}: 191 in a callback"
        );
        screen.renderer.submit_selection(&[]).unwrap();
    }
    logic.script.park();
    assert_eq!(
        scripts.parked(ScreenKind::Selection),
        Some((50 * 12, false)),
        "the runtime ran its twelve callbacks each frame"
    );
}

/// The whole frames' build mark (dev-tools, the S7-07d gate) is set
/// before the selection skin's build begins, where the frame's `cpu`
/// starts counting: the preparation span never holds the build.
#[test]
fn the_build_mark_precedes_the_selection_build() {
    let _serial = crate::imports::tests::serial();
    let mut r = warp();
    let dir = crate::selection_skin::runtime_fixture();
    let (b, script) =
        crate::selection_skin::fixture_scripted(r.atlas_limits(), dir.path(), "rt.luaskin");
    let token = r.install(ScreenKind::Selection, &b, 1).unwrap();
    r.commit(token).unwrap();
    let key = std::sync::Arc::as_ptr(&b).addr();
    let scripts = crate::screens::Scripts::new();
    scripts.commit(ScreenKind::Selection, key, script);
    let mut screen = MenuScreen::new(r, Some(Fonts::runtime().unwrap()));
    let mut logic =
        SelectionLogic::new(&b, 1).with_script(scripts.slot(ScreenKind::Selection, key));
    let mut feed = SkinFeed::default();
    feed.enter(TimeUs(0));
    let u = update(&mut feed, &searching("", ""), 0);
    screen
        .prepare_selection(&u, &mut logic, true)
        .unwrap()
        .unwrap();
    for k in 0..5 {
        let now = TimeUs(k * 16_667);
        let _ = crate::dev::take_build_mark();
        let _ = render::take_build_start();
        screen
            .build_selection_frame(&u, &mut logic, now, APP)
            .unwrap();
        screen.renderer.submit_selection(&[]).unwrap();
        let mark = crate::dev::take_build_mark().expect("the build marked");
        let start = render::take_build_start().expect("the build began");
        assert!(mark <= start, "frame {k}: the build began before its mark");
    }
}

/// The chart fixture's tables a frame is given in turn: none (not
/// current), its images written into the screen's selection array, and
/// absent for that array.
fn chart_tables(
    screen: &mut MenuScreen,
    logic: &SelectionLogic,
) -> [Option<skin_model::chart_image::ChartTable>; 3] {
    let (_images, set) = crate::selection_skin::selection_images(
        logic.bundle.side(),
        [200, 10, 10],
        Some([10, 200, 10]),
    );
    let table = screen
        .write_selection_chart(set.fitted(), set.id())
        .unwrap();
    let key = table.key().unwrap();
    [
        None,
        Some(table),
        Some(skin_model::chart_image::ChartTable::ABSENT.keyed(key)),
    ]
}

/// The table of [`chart_tables`] frame `k` is given.
fn chart_table(
    tables: &[Option<skin_model::chart_image::ChartTable>; 3],
    k: i64,
) -> Option<skin_model::chart_image::ChartTable> {
    tables[usize::try_from(k.rem_euclid(3)).unwrap()]
}

/// Whether `q` holds a quad on `rect`, rounded, from `layer` if given.
fn has(q: &[skin_model::Quad], rect: [f32; 4], layer: Option<u16>) -> bool {
    q.iter()
        .any(|q| q.rect.map(f32::round) == rect && layer.is_none_or(|l| q.layer == l))
}

/// The chart fixture's skin with its runtime on a WARP screen, its chart
/// layer's index, and its state.
fn chart_skinned(
    scripts: &crate::screens::Scripts,
) -> (tempfile::TempDir, MenuScreen, SelectionLogic, u16) {
    let mut r = warp();
    let dir = crate::selection_skin::chart_fixture();
    let (b, script) =
        crate::selection_skin::fixture_scripted(r.atlas_limits(), dir.path(), "chart.luaskin");
    assert!(script.holds(), "the callbacks are the runtime's");
    let layer = b.chart_layer().expect("the skin keeps a chart layer");
    let token = r.install(ScreenKind::Selection, &b, 1).unwrap();
    r.commit(token).unwrap();
    let key = std::sync::Arc::as_ptr(&b).addr();
    scripts.commit(ScreenKind::Selection, key, script);
    let screen = MenuScreen::new(r, Some(Fonts::runtime().unwrap()));
    let logic = SelectionLogic::new(&b, 1).with_script(scripts.slot(ScreenKind::Selection, key));
    (dir, screen, logic, layer)
}

/// Options 190 to 193 read the selection table set for the frame, the
/// same in the frame's conditions (`op`), in the runtime's callbacks
/// (`option(n)`, `option(-n)`, `not option(m)`)
/// and in its draws of −100 and −102 (D4, SC2, SC3): while no table is
/// current 190 and 192 hold, 191 and 193 do not and neither image draws;
/// with one, 191 and 193 read each image's presence. The fields are
/// written after the view update and before the callbacks: a frame whose
/// view changed reads the table, and a frame whose table changed reads it
/// in its own callbacks, not the last frame's.
#[test]
fn the_selection_image_options_agree_in_frames_callbacks_and_conditions() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let _untimed = crate::selection_skin::untimed();
    let scripts = crate::screens::Scripts::new();
    let (_dir, mut screen, mut logic, layer) = chart_skinned(&scripts);
    let side = logic.bundle.side();
    let (_both, both) =
        crate::selection_skin::selection_images(side, [200, 10, 10], Some([10, 200, 10]));
    let (_stage, stage_only) = crate::selection_skin::selection_images(side, [10, 10, 200], None);
    let mut feed = SkinFeed::default();
    feed.enter(TimeUs(0));
    let mut u = update(&mut feed, &searching("", ""), 0);
    let frame = |screen: &mut MenuScreen,
                 logic: &mut SelectionLogic,
                 u: &SelectionUpdate,
                 changed: bool,
                 table: Option<skin_model::chart_image::ChartTable>,
                 k: i64| {
        logic.sel.set_chart_images(table);
        let d = screen
            .selection(u, Some(logic), changed, TimeUs(k * 16_667), APP)
            .unwrap();
        assert_eq!(d.refused, None);
        screen.renderer.selection_quads().unwrap()
    };
    // What each option of `CHART_OPTIONS` reads with the stage file's and
    // the banner's presence.
    let check = |q: &[skin_model::Quad], stage: bool, banner: bool, what: &str| {
        let reads = [!stage, stage, !banner, banner, !stage, !banner];
        for (i, (n, want)) in crate::selection_skin::CHART_OPTIONS
            .iter()
            .zip(reads)
            .enumerate()
        {
            for (row, how) in ["op", "callback", "callback not"].iter().enumerate() {
                assert_eq!(
                    has(q, crate::selection_skin::chart_mark(row, i), None),
                    want,
                    "{what}: option {n} by {how}"
                );
            }
        }
        assert_eq!(
            has(q, crate::selection_skin::STAGE_DST, Some(layer)),
            stage,
            "{what}: −100"
        );
        assert_eq!(
            has(q, crate::selection_skin::BANNER_DST, Some(layer)),
            banner,
            "{what}: −102"
        );
    };
    let table = screen
        .write_selection_chart(both.fitted(), both.id())
        .unwrap();
    assert_eq!(table.key(), screen.selection_chart_key());
    assert!(table.key().is_some());
    // Not current: the first frame of the view.
    let q = frame(&mut screen, &mut logic, &u, true, None, 0);
    check(&q, false, false, "not current");
    // Current from the next frame: its callbacks read this frame's table.
    let q = frame(&mut screen, &mut logic, &u, false, Some(table), 1);
    check(&q, true, true, "current");
    // A view update (a panel opened) resets the view's fields; the table
    // still reads in that frame's callbacks.
    u.revision += 1;
    let q = frame(&mut screen, &mut logic, &u, true, Some(table), 2);
    check(&q, true, true, "current, the view changed");
    // Not current again (a cursor move), in the same frame.
    let q = frame(&mut screen, &mut logic, &u, false, None, 3);
    check(&q, false, false, "not current again");
    // A set without a banner: 193 does not hold, 192 does.
    let table = screen
        .write_selection_chart(stage_only.fitted(), stage_only.id())
        .unwrap();
    let q = frame(&mut screen, &mut logic, &u, false, Some(table), 4);
    check(&q, true, false, "no banner");
    // An absent table keyed to the array: neither.
    let absent = skin_model::chart_image::ChartTable::ABSENT.keyed(table.key().unwrap());
    let q = frame(&mut screen, &mut logic, &u, false, Some(absent), 5);
    check(&q, false, false, "absent");
}
