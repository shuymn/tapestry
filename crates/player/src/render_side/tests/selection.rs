//! The select screen's updates on the render thread (design.md "Rendering
//! and Skins", Screen skins): the slot's bound and lifetimes, the skin's
//! state per lifetime across a screen built again, a runtime refusal for
//! the rest of the run, and, on WARP through a real screen, the skin drawn
//! with the player's labels.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bms_model::TimeUs;
use game::Preparation;
use skin_model::{ScreenKind, SelectionView};

use super::{Installers, SelectionSeen, bundle, wait_until};
use crate::menu_screen::MenuPage;
use crate::selection_skin::{SelectionEvents, SelectionUpdate, StripText};

/// Update `revision` of lifetime `lifetime`, entered at `lifetime` s, of
/// `view`.
fn update_of(lifetime: u64, revision: u64, view: SelectionView) -> SelectionUpdate {
    let entered = TimeUs(i64::try_from(lifetime).unwrap() * 1_000_000);
    SelectionUpdate {
        lifetime,
        revision,
        list_revision: 1,
        page: MenuPage::default(),
        view,
        labels: crate::select::labels(),
        strip: StripText::default(),
        events: SelectionEvents {
            entered,
            changed: entered,
            ..SelectionEvents::default()
        },
    }
}

fn update(lifetime: u64, revision: u64) -> SelectionUpdate {
    update_of(lifetime, revision, SelectionView::default())
}

/// How long a fence on the render thread is waited for.
const FENCE: Duration = Duration::from_secs(5);

fn seen(screens: &Installers) -> Vec<SelectionSeen> {
    screens.selections.lock().unwrap().clone()
}

/// The screen's device is lost on its next selection frame, and the
/// screen is built again, as the flow does.
fn rebuild(screens: &Installers, link: &super::super::RenderLink) {
    screens.fail.store(true, Ordering::Release);
    assert!(wait_until(|| link.failed()));
    screens.fail.store(false, Ordering::Release);
    link.prepare().unwrap();
}

/// Grants the render thread `n` frames and waits until it holds in the
/// frame wait after them.
fn grant(screens: &Installers, n: u32) {
    let before = screens.waits.load(Ordering::Acquire);
    screens.permits.store(n, Ordering::Release);
    assert!(wait_until(|| {
        screens.waits.load(Ordering::Acquire) >= before + n
            && screens.held.load(Ordering::Acquire)
            && screens.permits.load(Ordering::Acquire) == 0
    }));
}

/// However many updates the game thread makes while the render thread is
/// held, one waits in the slot with one signal, and the render thread
/// takes only the newest; an update taken before selection was left is
/// never drawn after the render thread's frame wait, and one made after
/// it is dropped.
#[test]
fn the_slot_holds_the_newest_and_a_left_lifetime_is_never_drawn() {
    let screens = Installers::default();
    let link = screens.spawn_bare();
    link.enter_selection(1);
    link.show_selection(update(1, 1));
    assert!(wait_until(|| seen(&screens)
        .iter()
        .any(|s| s.revision == 1)));
    // Held in its frame wait.
    screens.pace.store(true, Ordering::Release);
    assert!(wait_until(|| screens.held.load(Ordering::Acquire)));
    for r in 2..=101 {
        link.show_selection(update(1, r));
    }
    // One frame of what it held, then the newest taken and drawn.
    grant(&screens, 2);
    let revisions: Vec<u64> = seen(&screens).iter().map(|s| s.revision).collect();
    assert_eq!(revisions.last(), Some(&101), "{revisions:?}");
    assert!(
        revisions.iter().all(|r| *r == 1 || *r == 101),
        "{revisions:?}"
    );
    // Taken, then held in the wait before its frame: selection is left.
    link.show_selection(update(1, 102));
    grant(&screens, 1);
    link.leave_selection();
    // Made after the end: dropped at the slot.
    link.show_selection(update(1, 103));
    screens.pace.store(false, Ordering::Release);
    assert!(link.fence(FENCE));
    let drawn: Vec<u64> = seen(&screens).iter().map(|s| s.revision).collect();
    assert!(!drawn.contains(&102), "{drawn:?}");
    assert!(!drawn.contains(&103), "{drawn:?}");
    link.exit();
}

/// An update taken while the launch screen shows, whose lifetime ends
/// during the frame wait in which the launch screen's time runs out, is
/// never built: the launch screen hands over to selection, and the
/// lifetime is checked after that, right before the build.
#[test]
fn a_lifetime_left_as_the_launch_screen_hands_over_is_never_drawn() {
    let screens = Installers::default();
    screens.splash.store(true, Ordering::Release);
    let started = std::time::Instant::now();
    let link = screens.spawn_bare();
    link.enter_selection(1);
    link.show_selection(update(1, 1));
    // Taken while the launch screen shows; then held in the frame wait.
    assert!(link.fence(FENCE));
    screens.pace.store(true, Ordering::Release);
    assert!(wait_until(|| screens.held.load(Ordering::Acquire)));
    assert!(started.elapsed() < crate::menu_screen::SPLASH);
    assert!(seen(&screens).is_empty());
    // The launch screen's time runs out during the wait; selection is
    // left; the frame granted.
    std::thread::sleep(
        crate::menu_screen::SPLASH.saturating_sub(started.elapsed()) + Duration::from_millis(300),
    );
    link.leave_selection();
    screens.permits.store(1, Ordering::Release);
    assert!(link.fence(FENCE));
    assert!(seen(&screens).is_empty(), "{:?}", seen(&screens));
    link.exit();
}

/// The skin's state is made afresh at each entry and kept, with every
/// start, across a screen built again within the entry; a refusal on a
/// healthy device draws the page from then on, says why once, and a
/// screen built again later restores no selection skin.
#[test]
fn the_skin_state_follows_the_lifetime_and_a_refusal_is_for_the_run() {
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let skin = bundle();
    assert_eq!(
        link.select(ScreenKind::Selection, Some(Arc::clone(&skin)))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    let last = |screens: &Installers| *seen(screens).last().unwrap();
    link.enter_selection(1);
    link.show_selection(update(1, 1));
    assert!(wait_until(|| seen(&screens)
        .iter()
        .any(|s| s.revision == 1)));
    let first = last(&screens);
    assert!(first.state.is_some() && first.changed);
    // Later frames of the same update lay nothing out again.
    assert!(wait_until(|| !last(&screens).changed));
    // A screen built again within the lifetime: the same state and starts,
    // its text laid out again.
    rebuild(&screens, &link);
    link.show_selection(update(1, 2));
    assert!(wait_until(|| last(&screens).revision == 2));
    let again = *seen(&screens).iter().find(|s| s.revision == 2).unwrap();
    assert_ne!(again.device, first.device);
    assert_eq!(again.state, first.state);
    assert_eq!(again.events, first.events);
    assert!(again.changed);
    // The next entry: a fresh state.
    link.enter_selection(2);
    link.show_selection(update(2, 1));
    assert!(wait_until(|| last(&screens).lifetime == 2));
    let second = last(&screens);
    assert!(second.state.is_some());
    assert_ne!(second.state, first.state);
    // Refused: the page from then on, its reason once.
    assert_eq!(link.take_selection_refusal(), None);
    screens.refuse_selection.store(true, Ordering::Release);
    assert!(wait_until(|| last(&screens).state.is_none()));
    let why = link.take_selection_refusal();
    assert_eq!(why.as_deref(), Some("refused for the test"));
    assert_eq!(link.take_selection_refusal(), None);
    screens.refuse_selection.store(false, Ordering::Release);
    rebuild(&screens, &link);
    link.show_selection(update(2, 2));
    assert!(wait_until(
        || last(&screens).revision == 2 && last(&screens).lifetime == 2
    ));
    assert_eq!(last(&screens).state, None);
    let device = last(&screens).device;
    let restored = screens.log();
    assert!(
        !restored.contains(&format!("{device} restore Selection")),
        "{restored:?}"
    );
    link.exit();
}

/// The render thread takes only a newer update of a lifetime: one older
/// than the update it drew, offered after it, is never drawn.
#[test]
fn an_older_update_is_never_drawn_after_a_newer_one() {
    let screens = Installers::default();
    let link = screens.spawn_bare();
    link.enter_selection(1);
    link.show_selection(update(1, 2));
    assert!(wait_until(|| seen(&screens)
        .iter()
        .any(|s| s.revision == 2)));
    link.show_selection(update(1, 1));
    assert!(link.fence(FENCE));
    let drawn: Vec<u64> = seen(&screens).iter().map(|s| s.revision).collect();
    assert!(drawn.iter().all(|r| *r == 2), "{drawn:?}");
    link.exit();
}

/// Another selection skin installed within a lifetime draws from a
/// fresh state of its own, not the one the skin before it filled.
#[test]
fn a_new_installation_starts_a_fresh_skin_state() {
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let install = |skin| {
        assert_eq!(
            link.select(ScreenKind::Selection, Some(skin))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
    };
    install(bundle());
    link.enter_selection(1);
    link.show_selection(update(1, 1));
    assert!(wait_until(|| seen(&screens)
        .iter()
        .any(|s| s.state.is_some())));
    let first = seen(&screens).iter().find_map(|s| s.state).unwrap();
    install(bundle());
    link.show_selection(update(1, 2));
    assert!(wait_until(|| seen(&screens)
        .iter()
        .any(|s| s.revision == 2)));
    let again = seen(&screens)
        .into_iter()
        .find(|s| s.revision == 2)
        .unwrap();
    assert!(again.state.is_some_and(|n| n != first), "{again:?}");
    link.exit();
}

/// A self-made selection skin of one image on a `when` timer whose
/// condition always holds, moving right 1 px per ms from the timer's
/// start: a frame's quad is at x = the time since the start, in ms.
fn when_bundle() -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    use skin_model::{
        Anchor, AtlasLimits, Blend, ElementDef, End, FileDef, Filter, Fit, ImageDef, KeyDef, Place,
        Region, RegionDef, SELECTION, Scope, SelectionDefs, Sheet, Skin, SkinDef, Source, TimerRef,
        Track, Ty, Variant, compile,
    };
    let dir = tempfile::tempdir().unwrap();
    let file = std::fs::File::create(dir.path().join("0.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 2, 2);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    e.write_header()
        .unwrap()
        .write_image_data(&[255; 16])
        .unwrap();
    let sheet = Sheet::new(
        dir.path(),
        vec![FileDef {
            path: "0.png".to_owned(),
            hi: None,
        }],
        vec![ImageDef {
            file: 0,
            rect: None,
            grid: [1, 1],
            cycle: None,
            filter: Filter::Linear,
        }],
    )
    .unwrap();
    let key = |at: i64, x: i32| {
        let mut props = [None; 9];
        props[..4].copy_from_slice(&[Some(x), Some(0), Some(10), Some(10)]);
        KeyDef {
            at: TimeUs(at),
            props,
        }
    };
    let element = ElementDef {
        source: Source::Image(0),
        track: Track::new(
            &[key(0, 0), key(1_000_000_000, 1_000_000)],
            skin_model::Ease::Linear,
            End::Hold,
        )
        .unwrap(),
        timer: TimerRef::When(0),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    };
    let skin = Skin::new_selection(
        SkinDef {
            fit: Fit::Fixed,
            regions: [RegionDef {
                anchor: Anchor::TopLeft,
                optional: false,
            }; Region::COUNT],
            whens: vec![compile("true", &Scope::new(&SELECTION), Ty::Bool).unwrap()],
            sheet,
            lanes: None,
            elements: vec![element],
            widgets: skin_model::WidgetDefs::default(),
            bomb_up_to: None,
            close: None,
        },
        SelectionDefs::default(),
    )
    .unwrap();
    let limits = AtlasLimits {
        max_side: 256,
        max_layers: 4,
        reserved_layers: 1,
    };
    let atlas = skin.atlas(Variant::Lo, limits, &|| false).unwrap();
    let clips = render::ClipTable::new(Vec::new()).unwrap();
    let bundle =
        render::SkinBundle::new(Arc::new(skin), atlas, clips, skin_model::Side::Right).unwrap();
    (dir, Arc::new(bundle))
}

/// Each frame the skin built of lifetime `lifetime`: its device, and its
/// `when` timer's start in ms read from its quad.
fn when_starts(screens: &Installers, lifetime: u64) -> Vec<(u64, i64)> {
    screens
        .built
        .lock()
        .unwrap()
        .iter()
        .filter(|(seen, _, _)| seen.lifetime == lifetime)
        .map(|(seen, now, rects)| {
            #[expect(clippy::cast_possible_truncation, reason = "whole ms of a test's run")]
            let x = rects[0][0].round() as i64;
            (seen.device, now.0 / 1_000 - x)
        })
        .collect()
}

/// A `when` timer of the selection skin starts at the first frame of each
/// entry, its condition holding throughout, and keeps its start across a
/// screen built again within the entry (design.md "Rendering and Skins",
/// Screen skins; C4).
#[test]
fn a_when_timer_restarts_at_each_entry_and_survives_a_screen_built_again() {
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let (_dir, skin) = when_bundle();
    assert_eq!(
        link.select(ScreenKind::Selection, Some(skin))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    let near = |a: i64, b: i64| (a - b).abs() <= 1;
    link.enter_selection(1);
    link.show_selection(update(1, 1));
    assert!(wait_until(|| when_starts(&screens, 1).len() >= 3));
    let first = when_starts(&screens, 1);
    let (device, start) = first[0];
    assert!(first.iter().all(|&(_, s)| near(s, start)), "{first:?}");
    // The first frame started it: its quad at x = 0.
    let first_now = screens.built.lock().unwrap()[0].1;
    assert!(near(start, first_now.0 / 1_000), "{start} {first_now:?}");
    // A screen built again within the entry: the same start.
    rebuild(&screens, &link);
    link.show_selection(update(1, 2));
    assert!(wait_until(|| when_starts(&screens, 1)
        .iter()
        .any(|&(d, _)| d != device)));
    let again = when_starts(&screens, 1);
    assert!(
        again
            .iter()
            .filter(|&&(d, _)| d != device)
            .all(|&(_, s)| near(s, start)),
        "{again:?}"
    );
    // Another screen drawn, selection left, and the next entry: started
    // again at its first frame.
    link.send(super::super::RenderMsg::Menu(Box::default()));
    std::thread::sleep(Duration::from_millis(50));
    link.leave_selection();
    link.enter_selection(2);
    link.show_selection(update(2, 1));
    assert!(wait_until(|| !when_starts(&screens, 2).is_empty()));
    let built = screens.built.lock().unwrap().clone();
    let (_, entered, _) = built.iter().find(|(s, _, _)| s.lifetime == 2).unwrap();
    let (_, restart) = when_starts(&screens, 2)[0];
    assert!(near(restart, entered.0 / 1_000), "{restart} {entered:?}");
    assert!(restart >= start + 50, "{restart} {start}");
    link.exit();
}

#[cfg(feature = "dev-tools")]
mod warp;
