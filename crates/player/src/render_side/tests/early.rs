//! A decided play's early request on the render thread (design.md "Screen
//! Transitions and Loading"): staged through the attempt's handshake with
//! its pages and loading text only, adopted by the attempt or dropped for
//! a fresh staging, its result release following to the attempt, restaged
//! on a screen built again, refused into the shared cell, withdrawn by its
//! end and settled at READY's cutoff; and the loading text prepared for it
//! while the decision screen draws.

use std::sync::atomic::Ordering::SeqCst;

use super::*;
use crate::render_side::{Answered, AttemptSkin, CutoffPhase, Extent, PendingAttempt, RenderLink};

/// A render thread with play's skin pending, a held loading lifetime of
/// chart "T" published (nothing of it drawn) and the built-in decision
/// screen drawn, as at a decided play, with room for 64 loading texts
/// noted: the chart, the installers, the link and the lifetime.
fn held() -> (Arc<Chart>, Installers, RenderLink, u64) {
    let (chart, screens, link) = pending_link();
    screens.texts.lock().unwrap().reserve(64);
    link.begin_loading_held(&chosen("T"), None);
    link.loading().show(one_line("loading"));
    let lifetime = link.loading_lifetime();
    link.show_decision(one_line("page"), None);
    (chart, screens, link, lifetime)
}

/// The early request of `skin` sent: its ticket.
fn early(link: &RenderLink, skin: &Arc<render::SkinBundle>) -> PendingAttempt {
    link.early_sender().begin_early(Arc::clone(skin))
}

/// The ticket's answer, waited for.
#[track_caller]
fn answer(ticket: &mut PendingAttempt) -> Answered {
    ticket
        .wait_until(None, || false)
        .expect("an answer")
        .expect("no failure")
}

/// The attempt's request of `chart` with `skin`, adopting `adopts`.
fn adopting(
    link: &RenderLink,
    chart: &Chart,
    skin: &Arc<render::SkinBundle>,
    adopts: u64,
) -> PendingAttempt {
    link.begin_adopting(
        titled(chart),
        AttemptSkin::Specialized(Arc::clone(skin)),
        Some(adopts),
    )
}

/// Whether the log notes `step` on any device.
fn noted(screens: &Installers, step: &str) -> bool {
    screens
        .log()
        .iter()
        .any(|l| l.split_once(' ').is_some_and(|(_, s)| s == step))
}

/// The steps noted starting with `prefix`, on any device.
fn steps(screens: &Installers, prefix: &str) -> Vec<String> {
    screens
        .log()
        .into_iter()
        .filter(|l| {
            l.split_once(' ')
                .is_some_and(|(_, s)| s.starts_with(prefix))
        })
        .collect()
}

/// The screen lost and built again, and the decision screen shown again,
/// as the flow does.
fn rebuild(link: &RenderLink, screens: &Installers) {
    rebuild_screen(link, screens);
    link.show_decision(one_line("page"), None);
}

/// The loading texts asked so far.
fn texts(screens: &Installers) -> Vec<(u64, Option<u64>)> {
    screens.texts.lock().unwrap().clone()
}

/// B3, B16b: an early request stages the specialization's pages and
/// loading presentation only, through the attempt's handshake: no attempt
/// prepared (no `Play`, no attempt text, no BGA), no manifest; its answer
/// is ready, not adopted. While the built-in decision draws the held
/// lifetime, its loading text is prepared on that staging.
// Revert: the early path calls the full `prepare_attempt` -> "prepare"
// noted. Revert: the built-in arm removed -> no text for the staging.
#[test]
fn an_early_request_stages_pages_and_text_only() {
    let (_chart, screens, link, lifetime) = held();
    let skin = bundle();
    let mut ticket = early(&link, &skin);
    let r = ticket.request();
    let a = answer(&mut ticket);
    assert_eq!((a.preparation, a.adopted), (Preparation::Ready, false));
    assert!(
        noted(&screens, &format!("early {r}")),
        "{:?}",
        screens.log()
    );
    assert_eq!(steps(&screens, "prepare"), Vec::<String>::new());
    assert!(screens.bga_shown.lock().unwrap().is_empty());
    assert!(screens.manifests.lock().unwrap().is_empty());
    assert!(screens.bga.lock().unwrap().begins.is_empty());
    assert!(wait_until(|| texts(&screens).contains(&(lifetime, Some(r)))));
    link.exit();
}

/// B16c: once READY's cutoff is asked no loading text is prepared, the
/// decision still drawing.
// Revert: the cutoff check removed -> texts asked after the cutoff.
#[test]
fn no_loading_text_once_the_cutoff_is_asked() {
    let (_chart, screens, link, lifetime) = held();
    let mut ticket = early(&link, &bundle());
    let r = ticket.request();
    answer(&mut ticket);
    assert!(wait_until(|| texts(&screens).contains(&(lifetime, Some(r)))));
    link.ask_cutoff();
    assert!(wait_until(|| screens.staging().is_none()));
    let asked = texts(&screens).len();
    let frames = screens.menu_frames.lock().unwrap().len();
    assert!(wait_until(
        || screens.menu_frames.lock().unwrap().len() >= frames + 3
    ));
    assert_eq!(texts(&screens).len(), asked, "{:?}", texts(&screens));
    link.exit();
}

/// B5a: the attempt adopts an early request not yet taken (its room
/// released the result and then waited): the early ticket is answered
/// ready and adopted, never superseded, and the result's release follows
/// to the attempt's request, which stages afresh.
// Revert: promotion after `supersede_attempts` -> the early ticket
// answered "a newer request superseded it".
#[test]
fn the_attempt_adopts_an_untaken_early_request() {
    let (chart, screens, link, _lifetime) = held();
    screens.early_room.store(2, SeqCst);
    let skin = bundle();
    let mut ticket = early(&link, &skin);
    let e = ticket.request();
    assert!(wait_until(|| link.result_status().released == Some(e)));
    assert!(noted(&screens, "release"));
    let mut full = adopting(&link, &chart, &skin, e);
    let f = full.request();
    let a = answer(&mut ticket);
    assert_eq!((a.preparation, a.adopted), (Preparation::Ready, true));
    assert_eq!(answer(&mut full).preparation, Preparation::Ready);
    assert_eq!(link.result_status().released, Some(f));
    assert_eq!(steps(&screens, "early"), Vec::<String>::new());
    assert_eq!(steps(&screens, "adopt"), Vec::<String>::new());
    assert!(!steps(&screens, &format!("prepare {f} ")).is_empty());
    link.exit();
}

/// B5c: the attempt adopts an answered early staging (same lifetime,
/// bundle, device and target): the staging is re-keyed in place, its
/// preparation goes on, it reaches READY and commits.
// Revert: `adopt_staging` replaced by a fresh stage -> no "adopt".
#[test]
fn the_attempt_adopts_an_answered_early_staging() {
    let (chart, screens, link, _lifetime) = held();
    let skin = bundle();
    let mut ticket = early(&link, &skin);
    let e = ticket.request();
    answer(&mut ticket);
    let mut full = adopting(&link, &chart, &skin, e);
    let f = full.request();
    assert_eq!(answer(&mut full).preparation, Preparation::Ready);
    assert!(
        noted(&screens, &format!("adopt {e} {f}")),
        "{:?}",
        screens.log()
    );
    assert_eq!(steps(&screens, "discard"), Vec::<String>::new());
    link.commit_attempt().unwrap();
    assert!(noted(&screens, &format!("commit {f}")));
    assert!(installed_is(&screens, &skin));
    link.exit();
}

/// B5d: an early staging made for another bundle, or before a resize, is
/// not adopted: it is dropped and the attempt stages afresh.
// Revert: the target check removed -> the resized staging adopted.
#[test]
fn a_stale_early_staging_is_not_adopted() {
    // Another bundle.
    let (chart, screens, link, _lifetime) = held();
    let mut ticket = early(&link, &bundle());
    let e = ticket.request();
    answer(&mut ticket);
    let other = bundle();
    let mut full = adopting(&link, &chart, &other, e);
    let f = full.request();
    assert_eq!(answer(&mut full).preparation, Preparation::Ready);
    assert_eq!(steps(&screens, "adopt"), Vec::<String>::new());
    assert!(noted(&screens, "discard"));
    assert!(!steps(&screens, &format!("prepare {f} ")).is_empty());
    link.exit();

    // A resize since its staging.
    let (chart, screens, link, _lifetime) = held();
    let extent = Arc::new(Extent::default());
    extent.publish(1_920, 1_080);
    link.follow(&extent);
    let skin = bundle();
    let mut ticket = early(&link, &skin);
    let e = ticket.request();
    answer(&mut ticket);
    extent.publish(1_280, 720);
    let passes = screens.slice_boundaries.load(SeqCst);
    assert!(wait_until(
        || screens.slice_boundaries.load(SeqCst) >= passes + 3
    ));
    let mut full = adopting(&link, &chart, &skin, e);
    let f = full.request();
    let a = answer(&mut full);
    assert_eq!(a.preparation, Preparation::Ready);
    assert_eq!(a.screen.target, [1_280, 720], "resized before the attempt");
    assert_eq!(steps(&screens, "adopt"), Vec::<String>::new());
    assert!(!steps(&screens, &format!("prepare {f} ")).is_empty());
    link.exit();
}

/// B5e: the result's release an early staging made follows to the
/// attempt's request, whether the attempt adopts the staging or stages
/// afresh (another bundle).
// Revert: `released` re-keyed only on promotion -> the fresh staging's
// release stays the early request's.
#[test]
fn the_release_follows_the_promotion() {
    for adopt in [true, false] {
        let (chart, screens, link, _lifetime) = held();
        screens.early_room.store(2, SeqCst);
        let skin = bundle();
        let mut ticket = early(&link, &skin);
        let e = ticket.request();
        assert!(wait_until(|| link.result_status().released == Some(e)));
        screens.early_room.store(0, SeqCst);
        answer(&mut ticket);
        let attempt_skin = if adopt { Arc::clone(&skin) } else { bundle() };
        let mut full = adopting(&link, &chart, &attempt_skin, e);
        let f = full.request();
        assert_eq!(answer(&mut full).preparation, Preparation::Ready);
        assert_eq!(link.result_status().released, Some(f), "adopt {adopt}");
        assert_eq!(
            noted(&screens, &format!("adopt {e} {f}")),
            adopt,
            "{:?}",
            screens.log()
        );
        link.exit();
    }
}

/// B5f: an early token never commits: with only the early staging
/// answered, the commit finds nothing prepared.
// Revert: `commit` accepts the early staging -> "commit" noted.
#[test]
fn an_early_token_does_not_commit() {
    let (_chart, screens, link, _lifetime) = held();
    let skin = bundle();
    let mut ticket = early(&link, &skin);
    answer(&mut ticket);
    assert!(link.commit_attempt().is_err());
    assert_eq!(steps(&screens, "commit"), Vec::<String>::new());
    assert!(screens.play().is_none());
    link.exit();
}

/// B11a, B11d: a device lost before the early answer answers the ticket
/// nothing (no error, no refusal): the record stays, and the staging is
/// restaged on the screen built again once its room fits, then answered.
// Revert: the restage removed -> no "2 early".
#[test]
fn a_lost_device_before_the_answer_restages() {
    let (_chart, screens, link, _lifetime) = held();
    screens.early_room.store(1, SeqCst);
    let mut ticket = early(&link, &bundle());
    let e = ticket.request();
    rebuild(&link, &screens);
    assert!(ticket.try_answer().is_none(), "the ticket left open");
    assert_eq!(link.early_refused(e), None);
    screens.early_room.store(0, SeqCst);
    let a = answer(&mut ticket);
    assert_eq!(a.preparation, Preparation::Ready);
    assert_eq!(a.screen.device, 2);
    assert_eq!(steps(&screens, "early"), vec![format!("2 early {e}")]);
    link.exit();
}

/// B11b: a device lost after the early answer restages it on the screen
/// built again, where the attempt adopts it.
// Revert: the restage only for unanswered requests -> no "2 early".
#[test]
fn a_lost_device_after_the_answer_restages() {
    let (chart, screens, link, _lifetime) = held();
    let skin = bundle();
    let mut ticket = early(&link, &skin);
    let e = ticket.request();
    answer(&mut ticket);
    rebuild(&link, &screens);
    assert!(
        wait_until(|| steps(&screens, "early").contains(&format!("2 early {e}"))),
        "{:?}",
        screens.log()
    );
    let mut full = adopting(&link, &chart, &skin, e);
    let f = full.request();
    let a = answer(&mut full);
    assert_eq!((a.preparation, a.screen.device), (Preparation::Ready, 2));
    assert!(
        noted(&screens, &format!("adopt {e} {f}")),
        "{:?}",
        screens.log()
    );
    link.exit();
}

/// B11c: a refused restage is an early refusal: published in the shared
/// cell for the gate, and an attempt adopting the request is answered
/// refused with no staging begun.
// Revert: the tombstone not consulted for `Full` -> the attempt staged.
#[test]
fn a_refused_restage_is_an_early_refusal() {
    let (chart, screens, link, _lifetime) = held();
    let skin = bundle();
    let mut ticket = early(&link, &skin);
    let e = ticket.request();
    answer(&mut ticket);
    screens.refuse_early.store(true, SeqCst);
    rebuild(&link, &screens);
    assert!(wait_until(|| link.early_refused(e).is_some()));
    assert!(noted(&screens, &format!("refuse early {e}")));
    let mut full = adopting(&link, &chart, &skin, e);
    assert!(matches!(
        answer(&mut full).preparation,
        Preparation::Refused(_)
    ));
    assert_eq!(steps(&screens, "prepare"), Vec::<String>::new());
    link.exit();
}

/// B12b: the early request's end withdraws its staging, answered or not:
/// an answered staging is dropped, and one not yet taken never stages.
// Revert: withdrawal by the ticket's drop only -> the staging stays.
#[test]
fn ending_the_request_withdraws_the_staging() {
    // Answered.
    let (_chart, screens, link, _lifetime) = held();
    let sender = link.early_sender();
    let mut ticket = sender.begin_early(bundle());
    let e = ticket.request();
    answer(&mut ticket);
    assert!(screens.staging().is_some());
    sender.discard(e);
    assert!(wait_until(|| screens.staging().is_none()));
    link.exit();

    // Not yet taken: its ticket kept.
    let (_chart, screens, link, _lifetime) = held();
    screens.early_room.store(1, SeqCst);
    let sender = link.early_sender();
    let ticket = sender.begin_early(bundle());
    let e = ticket.request();
    sender.discard(e);
    screens.early_room.store(0, SeqCst);
    let passes = screens.slice_boundaries.load(SeqCst);
    assert!(wait_until(
        || screens.slice_boundaries.load(SeqCst) >= passes + 5
    ));
    assert_eq!(steps(&screens, "early"), Vec::<String>::new());
    drop(ticket);
    link.exit();
}

/// B15: when READY's cutoff is asked, no early staging remains: the
/// answered staging is dropped before the cutoff is given.
// Revert: the settle check skipped -> the staging stays.
#[test]
fn the_cutoff_waits_for_the_early_staging() {
    let (_chart, screens, link, _lifetime) = held();
    let mut ticket = early(&link, &bundle());
    answer(&mut ticket);
    assert!(screens.staging().is_some());
    link.ask_cutoff();
    assert!(wait_until(|| screens.staging().is_none()));
    link.exit();
}

/// READY's cutoff asked just before the frame's wait, the early staging's
/// withdrawal filling a retirement entry: the render thread counts that
/// entry, so it is not cut off until a boundary destroyed it.
// Revert: the filled count read before the settlement -> cut off with the
// entry filled.
#[test]
fn the_cutoff_counts_the_entry_the_withdrawal_fills() {
    let (_chart, screens, link, _lifetime) = held();
    let mut ticket = early(&link, &bundle());
    answer(&mut ticket);
    screens.fill_on_discard.store(true, SeqCst);
    let gate = link.frame_gate(1);
    gate.hold(true);
    assert!(wait_until(|| gate.waiting()));
    link.ask_cutoff();
    gate.hold(false);
    assert!(wait_until(|| screens.staging().is_none()));
    assert_eq!(screens.filled.load(SeqCst), 1);
    let frames = screens.menu_frames.lock().unwrap().len();
    assert!(wait_until(
        || screens.menu_frames.lock().unwrap().len() >= frames + 3
    ));
    assert_ne!(link.cutoff_phase(), CutoffPhase::CutOff);
    screens.filled.store(0, SeqCst);
    assert!(wait_until(|| link.cutoff_phase() == CutoffPhase::CutOff));
    drop(ticket);
    link.exit();
}
