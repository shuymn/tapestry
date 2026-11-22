//! The result skin's preload on the player's rig (design.md "beatoraja Lua
//! Skins", the result skin's preload; "State Model", Preparing): a choice
//! claims the pack, the attempt's image stage adopts it, and the stage's
//! teardown hands it back or joins it. Each pack is held at its build's
//! checkpoint by the preload's test gate, never by timing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use game::AppRig as _;
use game::Rig as _;

use super::result_flow::{chart, on_warp, readied, uploaded};
use super::{begin, imports, stage, until};
use crate::play::PlayerRig;
use crate::result_skin::preload_tests::{Opened, REACHED, Seam, gate, long, preloaded_with};
use crate::result_skin::{PreloadControl, builds};

/// The rig's result skin preloaded from a self-made skin reading a result
/// fact, its preload pausing with the selection's shelf, and its pack
/// begun at a selection look and held at its checkpoint by the gate at
/// `level`. Its control, and the skin's folder.
fn held_on_rig(
    rig: &mut PlayerRig<'_>,
    seam: &Arc<Seam>,
    level: u8,
) -> (Arc<PreloadControl>, tempfile::TempDir) {
    let d = crate::result_skin::tests::runtime_drawn_skin();
    rig.result_skin = preloaded_with(d.path(), seam, Some(rig.render.scripts().clone()));
    let control = rig.result_skin.preload_control().unwrap();
    rig.selection
        .as_mut()
        .unwrap()
        .shelf
        .set_preload(Arc::clone(&control));
    gate(level);
    rig.poll_preload();
    reached();
    (control, d)
}

/// Waits until the held pack reached its gate.
fn reached() {
    let began = Instant::now();
    while !REACHED.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(began.elapsed() < long(), "the pack's checkpoint never came");
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

/// A choice to play on `rig`, as selection's look makes it: the claim
/// first, then the choice's pause.
fn choose(rig: &mut PlayerRig<'_>, loaded: &game::Loaded) {
    let sel = rig.selection.as_mut().unwrap();
    let claim = sel.shelf.claim_preload();
    sel.shelf.choose(loaded.path());
    rig.preload_claim = claim;
}

/// `h`'s preparation polled until its result skin's stage waits for the
/// adopted pack.
fn at_adopted(rig: &mut PlayerRig<'_>, h: &mut game::PrepHandle) {
    until("the adopted stage", || {
        let until = Instant::now() + Duration::from_millis(10);
        let _ = rig.poll_preparation(h, until);
        stage(rig) == "image"
    });
}

/// A choice to play while the pack runs: the attempt adopts it, runs no
/// import job of its own, and the pack's pages are the ones its own
/// ticket uploads after its PrepareAttempt, once; the atlas is built once
/// in the session.
#[test]
fn a_chosen_attempt_uploads_the_adopted_packs_pages() {
    let _serial = imports::serial();
    let _g = Opened;
    let (_dir, l) = chart(None);
    on_warp(l.chart(), true, |rig, _| {
        let seam = Arc::new(Seam::default());
        let b0 = builds();
        let (_control, _skin) = held_on_rig(rig, &seam, 2);
        choose(rig, &l);
        let (u0, jobs) = (crate::dev::result_uploads(), imports::jobs_counted());
        let mut h = begin(rig, &l);
        at_adopted(rig, &mut h);
        gate(0);
        readied(rig, &mut h);
        assert!(rig.pages_ticket.is_some(), "its own ticket");
        assert_eq!(crate::dev::result_uploads() - u0, 1, "uploaded once");
        assert_eq!(builds() - b0, 1, "built once");
        assert_eq!(imports::jobs_counted(), jobs, "no import job");
        let pages = rig.result_skin.image().unwrap().pages().id();
        assert_eq!(uploaded(rig).0, pages, "the pack's pages");
        rig.commit_attempt().unwrap();
        assert_eq!(rig.render.result_status().installed, Some(pages));
    });
}

/// The preparation's discard while its stage holds an adopted pack at a
/// gate: it returns without waiting for the pack's thread, the claim
/// ends, and the pack is the preload's again, so the choice's pause, still
/// held, stops it; after the release it packs.
#[test]
fn a_discard_hands_the_adopted_pack_back_without_waiting() {
    let _serial = imports::serial();
    let _g = Opened;
    let (_dir, l) = chart(None);
    on_warp(l.chart(), true, |rig, _| {
        let seam = Arc::new(Seam::default());
        let (control, _skin) = held_on_rig(rig, &seam, 2);
        choose(rig, &l);
        let mut h = begin(rig, &l);
        at_adopted(rig, &mut h);
        let (_, job) = rig.result_skin.preload_state();
        assert!(job.is_some());
        assert_eq!(rig.drop_prep(), Some(crate::imports::Settled::Ended));
        assert!(!control.claimed(), "the claim ended");
        assert_eq!(rig.result_skin.preload_state(), (None, job), "handed back");
        gate(0);
        until("the pack's collection", || {
            rig.poll_preload();
            rig.result_skin.preload_state().1.is_none()
        });
        assert_eq!(rig.result_skin.preload_state(), (Some(true), None));
        rig.selection.as_mut().unwrap().shelf.release();
        until("the pack after the release", || {
            rig.poll_preload();
            rig.result_skin.preload_state() == (None, None)
        });
        assert!(rig.result_skin.image().is_some());
    });
}

/// The run's end while the preparation's stage holds an adopted pack:
/// the pack is cancelled and its thread has ended once the end returns.
#[test]
fn the_runs_end_joins_the_adopted_pack() {
    let _serial = imports::serial();
    let _g = Opened;
    let (_dir, l) = chart(None);
    on_warp(l.chart(), true, |rig, _| {
        let seam = Arc::new(Seam::default());
        let (control, _skin) = held_on_rig(rig, &seam, 1);
        choose(rig, &l);
        let mut h = begin(rig, &l);
        at_adopted(rig, &mut h);
        rig.end_prep();
        assert!(control.wait_ended(Duration::ZERO), "its thread ended");
    });
}

/// The chosen chart left before Preparing (Back, a refused load, a quit
/// during the load, each through the chart's leaving): the claim ends,
/// and the pack goes on speculative.
#[test]
fn leaving_the_chart_ends_the_claim() {
    let _serial = imports::serial();
    let _g = Opened;
    let (_dir, l) = chart(None);
    on_warp(l.chart(), true, |rig, _| {
        let seam = Arc::new(Seam::default());
        let (control, _skin) = held_on_rig(rig, &seam, 2);
        choose(rig, &l);
        assert!(control.claimed());
        rig.left_chart().unwrap();
        assert!(!control.claimed(), "the claim ended");
        assert!(rig.preload_claim.is_none());
        rig.selection.as_mut().unwrap().shelf.release();
        gate(0);
        until("the pack's collection", || {
            rig.poll_preload();
            rig.result_skin.preload_state().1.is_none()
        });
        assert!(seam.left.lock().unwrap().is_empty(), "never foreground");
        assert!(rig.result_skin.image().is_some());
    });
}
