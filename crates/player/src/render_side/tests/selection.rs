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
    std::thread::sleep(Duration::from_millis(100));
    let drawn: Vec<u64> = seen(&screens).iter().map(|s| s.revision).collect();
    assert!(!drawn.contains(&102), "{drawn:?}");
    assert!(!drawn.contains(&103), "{drawn:?}");
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

#[cfg(feature = "dev-tools")]
mod warp;
