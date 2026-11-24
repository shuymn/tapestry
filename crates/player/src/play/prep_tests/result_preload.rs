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

/// The transition probe's marks in the order they were taken: each of
/// `steps` is there once (with its value where one is given), in that
/// order; none of `absent` is.
fn marked_in_order(steps: &[(&str, Option<i64>)], absent: &[&str]) {
    let marks = crate::dev::loop_probe::tests::marks();
    let tags: Vec<_> = marks.iter().map(|m| (m.1, m.2)).collect();
    let mut at = Vec::new();
    for (tag, value) in steps {
        let found: Vec<_> = (0..tags.len()).filter(|&i| tags[i].0 == *tag).collect();
        assert_eq!(found.len(), 1, "{tag}: {tags:?}");
        if let Some(v) = value {
            assert_eq!(tags[found[0]].1, *v, "{tag}'s value: {tags:?}");
        }
        at.push(found[0]);
    }
    assert!(at.is_sorted(), "{steps:?} out of order: {tags:?}");
    for tag in absent {
        assert!(tags.iter().all(|t| t.0 != *tag), "{tag}: {tags:?}");
    }
}

/// The transition probe's marks of an attempt that adopts the running
/// pack: the pack's begin, the choice's claim of it and the adoption, its
/// move to the foreground, its end and collection, then the stage decided
/// resident, the attempt's preparation sent (`PrepareAttempt`) and only
/// then its pages begun; no import job of the stage's own.
#[test]
fn the_probe_marks_an_adopted_attempts_steps_in_order() {
    let _serial = imports::serial();
    let _g = Opened;
    crate::dev::loop_probe::tests::turn_on();
    let (_dir, l) = chart(None);
    on_warp(l.chart(), true, |rig, _| {
        let seam = Arc::new(Seam::default());
        let (_control, _skin) = held_on_rig(rig, &seam, 2);
        let (_, Some(job)) = rig.result_skin.preload_state() else {
            panic!("no pack runs");
        };
        let id = Some(i64::try_from(job).unwrap());
        choose(rig, &l);
        let mut h = begin(rig, &l);
        at_adopted(rig, &mut h);
        gate(0);
        readied(rig, &mut h);
        marked_in_order(
            &[
                ("pre:begin", id),
                ("pre:claim", id),
                ("img:adopted", id),
                ("pre:class", id),
                ("pre:end", id),
                ("pre:collect", id),
                ("img:resident", Some(1)),
                ("att:send", None),
                ("res:pages_begun", None),
            ],
            &["img:job", "pre:stop"],
        );
    });
}

/// The transition probe's marks of an attempt that finds the pack's atlas
/// resident: the pack begun, ended and collected in selection, a claim of
/// no running pack at the choice, then the stage decided resident, the
/// attempt's preparation sent (`PrepareAttempt`) and only then its pages
/// begun; no adoption and no import job of the stage's own.
#[test]
fn the_probe_marks_a_resident_attempts_steps_in_order() {
    let _serial = imports::serial();
    let _g = Opened;
    crate::dev::loop_probe::tests::turn_on();
    let (_dir, l) = chart(None);
    on_warp(l.chart(), true, |rig, _| {
        let seam = Arc::new(Seam::default());
        let (_control, _skin) = held_on_rig(rig, &seam, 2);
        let (_, Some(job)) = rig.result_skin.preload_state() else {
            panic!("no pack runs");
        };
        let id = Some(i64::try_from(job).unwrap());
        gate(0);
        until("the pack's collection", || {
            rig.poll_preload();
            rig.result_skin.preload_state().1.is_none()
        });
        assert!(rig.result_skin.image().is_some(), "packed");
        choose(rig, &l);
        let mut h = begin(rig, &l);
        readied(rig, &mut h);
        marked_in_order(
            &[
                ("pre:begin", id),
                ("pre:end", id),
                ("pre:collect", id),
                ("pre:claim_open", Some(1)),
                ("img:resident", Some(1)),
                ("att:send", None),
                ("res:pages_begun", None),
            ],
            &["img:job", "img:adopted", "pre:class", "pre:claim"],
        );
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

/// The run disabled the result skin (a view's text refused) before an
/// attempt whose choice claimed the running pack: the attempt's result
/// step drops the claim and stops the pack before the attempt's
/// PrepareAttempt is sent, without waiting for it; its holdings stay
/// with it until a later look collects it, restoring nothing, and the
/// preload ends; the built-in result shows (design.md "beatoraja Lua
/// Skins", the result skin's preload).
#[test]
fn a_run_disabled_result_retires_the_claimed_pack_before_the_attempt() {
    let _serial = imports::serial();
    let _g = Opened;
    crate::dev::loop_probe::tests::turn_on();
    let (dir, l) = chart(None);
    on_warp(l.chart(), true, |rig, _| {
        // The first attempt's view refused: the run disables the kind.
        let d = crate::result_skin::tests::runtime_drawn_skin();
        let p = crate::result_skin::tests::pending(d.path(), crate::result_skin::tests::LIMITS);
        rig.result_skin =
            crate::result_skin::ResultSkin::new(Some(crate::result_skin::ResultStart::Pending(p)))
                .with_scripts(rig.render.scripts().clone());
        super::result_flow::prepared(rig, &l);
        let report = crate::end_screen::tests::report(rules::tapestry());
        let screen = crate::end_screen::tests::screen(dir.path(), &report);
        crate::dev::refuse_next_result_text();
        rig.show_end(&report, &screen);
        until("the kind disabled", || {
            rig.land_result();
            rig.render.result_status().disabled.is_some()
        });
        rig.leave_result();
        // The next choice claims the pack, held until it is told to stop.
        let seam = Arc::new(Seam::default());
        // Held whatever it is told: nothing waits for it.
        let (control, _skin) = held_on_rig(rig, &seam, 2);
        let (_, job) = rig.result_skin.preload_state();
        choose(rig, &l);
        let mut h = begin(rig, &l);
        readied(rig, &mut h);
        assert!(!control.claimed(), "the claim dropped");
        assert!(rig.preload_claim.is_none());
        assert_eq!(rig.result_skin.preload_state(), (None, job), "quarantined");
        assert!(rig.pages_ticket.is_none(), "no pages ticket");
        // Stopped before the second attempt's PrepareAttempt.
        let marks = crate::dev::loop_probe::tests::marks();
        let at = |tag: &str| marks.iter().rposition(|m| m.1 == tag);
        let (stopped, sent) = (at("pre:stop"), at("att:send"));
        assert!(
            stopped.is_some() && stopped < sent,
            "stopped {stopped:?}, sent {sent:?}"
        );
        rig.commit_attempt().unwrap();
        gate(0);
        assert!(control.wait_ended(long()), "the pack's thread ends");
        rig.poll_preload();
        assert_eq!(rig.result_skin.preload_state(), (None, None), "retired");
        assert!(rig.result_skin.image().is_none(), "nothing packed");
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
