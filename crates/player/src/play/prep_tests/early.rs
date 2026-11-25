//! The pending play skin's preparation during the decision screen on the
//! player's rig (design.md "Screen Transitions and Loading"; Threat Model
//! T09): begun at the decided play's read, cancelled at the reveal without
//! a wait, consumed by Preparing, refused through the gate, and joined at
//! the settle sites. Real jobs on self-made skins, held by the jobs' test
//! gates.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use game::AppRig as _;

use super::*;
use crate::play::early::{Committed, EarlyGate, EarlyPrep};

/// A host whose headroom fits any growth.
struct Fits;

impl crate::priority::Machine for Fits {
    fn cores(&self) -> usize {
        1
    }

    fn memory(&self) -> Result<u64, String> {
        Err("not read here (test)".to_owned())
    }

    fn headroom(&self) -> Result<u64, String> {
        Ok(u64::MAX)
    }
}

static FITS: Fits = Fits;

/// The decided play's early preparation, as the decision's entry makes
/// it, on `rig` with `skin` pending and a host that fits: its gate.
fn decided(rig: &mut PlayerRig<'_>, skin: crate::imports::PendingSkin) -> Arc<EarlyGate> {
    rig.pending = Some(skin);
    rig.machine = &FITS;
    let early = EarlyPrep::new(EarlyGate::new(rig.render.early_refusal()), 1);
    let gate = Arc::clone(early.gate());
    rig.early = Some(early);
    gate
}

/// The gate's state, by name.
fn state(gate: &EarlyGate) -> &'static str {
    gate.state_name()
}

/// B1 (player): a decided play's specialization begins at its chart's
/// read, before any load, on a job of its own, and ends specialized.
#[test]
fn early_specialization_starts_at_the_read() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        let gate = decided(rig, skin);
        let from = imports::jobs_counted();
        rig.chart_read(&l);
        assert_eq!(jobs_since(from).0, 1, "begun at the read");
        until("the specialization ended", || {
            state(&gate) == "ended specialized"
        });
        rig.left_chart().unwrap();
        assert_eq!(jobs_since(from), (1, 1));
    });
}

/// B6: an import still running at the play's deadline step is cancelled
/// there and not waited for; Preparing begins afresh with a job of its
/// own.
// Covers: T09
#[test]
fn an_import_in_flight_at_the_reveal_is_cancelled() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(12);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        let gate = decided(rig, skin);
        let from = imports::jobs_counted();
        let base = vm_hooks::live();
        rig.chart_read(&l);
        imports::executing(base);
        assert_eq!(rig.deadline_step(), game::StepOutcome::Stepped);
        assert_eq!(state(&gate), "cancelled at the reveal");
        assert_eq!(jobs_since(from), (1, 0), "the reveal did not wait for it");
        let _attempt = begin(rig, &l);
        assert_eq!(stage(rig), "skin", "Preparing imports afresh");
        assert_eq!(jobs_since(from), (2, 1), "the early job joined first");
        rig.discard_attempt();
        assert_eq!(jobs_since(from), (2, 2));
    });
}

/// B6b: the same at a load wait's reveal ([`crate::controls::Controls::look`]).
#[test]
fn an_import_in_flight_at_a_load_wait_reveal_is_cancelled() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(12);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        let gate = decided(rig, skin);
        let from = imports::jobs_counted();
        let base = vm_hooks::live();
        rig.chart_read(&l);
        imports::executing(base);
        let reveals = Arc::new(AtomicUsize::new(0));
        watch_past(rig, &reveals);
        assert_eq!(rig.controls.look(), Some(game::Verdict::Step));
        let d = rig.controls.unwatch_decision().unwrap();
        assert!(d.stepped());
        assert_eq!(reveals.load(Ordering::SeqCst), 1);
        assert_eq!(state(&gate), "cancelled at the reveal");
        assert_eq!(jobs_since(from), (1, 0), "the reveal did not wait for it");
        let _attempt = begin(rig, &l);
        assert_eq!(stage(rig), "skin", "Preparing imports afresh");
        rig.discard_attempt();
        assert_eq!(jobs_since(from), (2, 2));
    });
}

/// A play's decision past its deadline watched by the load's waits on
/// `rig`, with its early preparation; each reveal counted in `reveals`.
fn watch_past(rig: &PlayerRig<'_>, reveals: &Arc<AtomicUsize>) {
    let now = bms_model::TimeSource::now(&rig.clock);
    let d = game::Decision::begin(
        bms_model::TimeUs(now.0 - 60_000_000),
        game::DecisionLifetime(1),
        Some(game::DecisionShown::BuiltIn),
        false,
    );
    watch(rig, d, reveals);
}

/// `d` watched by the load's waits on `rig`, with its early preparation;
/// each reveal counted in `reveals`.
fn watch(rig: &PlayerRig<'_>, d: game::Decision, reveals: &Arc<AtomicUsize>) {
    let counted = Arc::clone(reveals);
    rig.controls.watch_decision(crate::controls::DecisionWatch {
        decision: d,
        origin: game::Origin::Load,
        reveal: Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }),
        failures: rig.failures.clone(),
        screen: None,
        early: rig.early_watch(),
    });
}

/// B6c: an import that completed just before the reveal's commit, its
/// thread held past its send and no step after it, is revealed with the
/// commit and consumed by Preparing: no second import.
#[test]
fn a_completion_before_the_commit_is_kept() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        let gate = decided(rig, skin);
        let from = imports::jobs_counted();
        imports::hold_exit(true);
        rig.chart_read(&l);
        until("the job held after its send", || {
            imports::exit_waiting() == 1
        });
        assert_eq!(rig.deadline_step(), game::StepOutcome::Stepped);
        assert_eq!(state(&gate), "revealed specialized");
        imports::hold_exit(false);
        let mut attempt = begin(rig, &l);
        assert_ne!(stage(rig), "skin", "the early specialization is reused");
        assert_eq!(jobs_since(from), (1, 1), "no second import");
        assert_eq!(polled(rig, &mut attempt).unwrap(), game::Preparation::Ready);
    });
}

/// B6e: a job that panics and one whose thread cannot start both end the
/// gate refused; and the gate runs before the worker can publish: a worker
/// at the gate's lock during the start finds the job running.
#[test]
fn a_panic_and_a_spawn_failure_complete_the_gate() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        let gate = decided(rig, skin);
        imports::panic_next_job();
        rig.chart_read(&l);
        until("the panic refused", || state(&gate) == "ended refused");
        let why = rig.early_refusal().expect("refused").reason;
        assert!(why.contains(crate::imports::PANICKED), "{why}");
        rig.left_chart().unwrap();

        let skin = rig.pending.take().unwrap();
        let gate = decided(rig, skin);
        imports::refuse_spawn();
        rig.chart_read(&l);
        assert_eq!(state(&gate), "ended refused");
        let why = rig.early_refusal().expect("refused").reason;
        assert!(why.contains("cannot start"), "{why}");
        rig.left_chart().unwrap();

        let skin = rig.pending.take().unwrap();
        let gate = decided(rig, skin);
        let held = Arc::clone(&gate);
        gate.after_spawn(move || {
            until("the worker at the gate's lock", || held.completing() == 1);
        });
        rig.chart_read(&l);
        until("the worker's thread ended", || imports::workers() == 0);
        assert_eq!(state(&gate), "ended specialized");
        rig.left_chart().unwrap();
    });
}

/// B6f: a reveal before the chart is read commits on the gate made at the
/// decision's entry, and the read after it begins nothing.
#[test]
fn a_reveal_before_the_read_starts_nothing() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        let gate = decided(rig, skin);
        let from = imports::jobs_counted();
        assert_eq!(rig.deadline_step(), game::StepOutcome::Stepped);
        rig.chart_read(&l);
        assert_eq!(jobs_since(from), (0, 0), "nothing begun");
        assert_eq!(state(&gate), "revealed");
        let _attempt = begin(rig, &l);
        assert_eq!(stage(rig), "skin", "today's begin");
        rig.discard_attempt();
    });
}

/// B6f through the decided play's entry: the entry makes the gate before
/// its fence, idle, so App's reveal before the chart's read commits on it
/// and the read after it begins nothing.
// Revert: the gate not made at the entry -> no gate for the reveal.
#[test]
fn the_entry_makes_the_gate_the_reveal_commits_on() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        rig.pending = Some(skin);
        rig.machine = &FITS;
        // No adjustment waits on the input thread, which none serves.
        rig.adjust = None;
        let charts = tempfile::tempdir().unwrap();
        let (sel, roots) = crate::selection::tests::tree(charts.path(), &[&["a.bms"]]);
        rig.selection = Some(sel);
        let now = bms_model::TimeSource::now(&rig.clock);
        let d = game::Decision::begin(
            now,
            game::DecisionLifetime(1),
            Some(game::DecisionShown::BuiltIn),
            false,
        );
        let _d = rig.entered_decision(&game::Pick::Play(roots[0].join("a.bms")), d);
        let gate = Arc::clone(rig.early.as_ref().expect("the entry's gate").gate());
        assert_eq!(state(&gate), "idle");
        let from = imports::jobs_counted();
        assert_eq!(rig.deadline_step(), game::StepOutcome::Stepped);
        assert_eq!(state(&gate), "revealed");
        rig.chart_read(&l);
        assert_eq!(jobs_since(from), (0, 0), "nothing begun");
        assert_eq!(state(&gate), "revealed");
        rig.left_chart().unwrap();
    });
}

/// B9d: a refusal the cancelled job publishes after the commit is dropped
/// on its worker: no early refusal, the loading state's route.
#[test]
fn a_refusal_after_the_commit_takes_the_loading_route() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(12);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        let gate = decided(rig, skin);
        let base = vm_hooks::live();
        rig.chart_read(&l);
        imports::executing(base);
        assert_eq!(rig.deadline_step(), game::StepOutcome::Stepped);
        until("the cancelled job's thread ended", || {
            imports::workers() == 0
        });
        assert_eq!(gate.completing(), 1, "its outcome came to the gate");
        assert_eq!(rig.early_refusal(), None);
        assert_eq!(state(&gate), "cancelled at the reveal");
        rig.left_chart().unwrap();
    });
}

/// B12a: the choice left before Preparing (a quit in loading) cancels the
/// job and joins it.
#[test]
fn ending_the_request_cancels_the_job() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(12);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        let gate = decided(rig, skin);
        let from = imports::jobs_counted();
        let base = vm_hooks::live();
        rig.chart_read(&l);
        imports::executing(base);
        let _watchdog = crate::tests::Watchdog::arm("the early job's join");
        rig.left_chart().unwrap();
        // Ended by its cancel, not by running to its instruction limit.
        let why = gate.refused().expect("the job's outcome").reason;
        assert!(why.ends_with("the import was cancelled"), "{why}");
        assert!(rig.early.is_none());
        assert_eq!(jobs_since(from), (1, 1), "cancelled and joined");
        assert_eq!(vm_hooks::live(), base);
    });
}

/// B12c: the early job is joined after the deadline's step, never in it:
/// in `left_chart` for a quit.
#[test]
fn the_join_is_never_in_the_deadline_step() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(12);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        decided(rig, skin);
        let from = imports::jobs_counted();
        let base = vm_hooks::live();
        rig.chart_read(&l);
        imports::executing(base);
        assert_eq!(rig.deadline_step(), game::StepOutcome::Stepped);
        assert_eq!(jobs_since(from), (1, 0), "not joined in the step");
        rig.left_chart().unwrap();
        assert_eq!(jobs_since(from), (1, 1), "joined in left_chart");
    });
}

/// B2c: a direct launch, with no decision's entry, specializes only in
/// Preparing: its read begins nothing.
#[test]
fn a_direct_launch_specializes_in_preparing() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        rig.pending = Some(skin);
        rig.machine = &FITS;
        let from = imports::jobs_counted();
        rig.chart_read(&l);
        assert!(rig.early.is_none());
        assert_eq!(jobs_since(from), (0, 0));
        let mut attempt = begin(rig, &l);
        assert_eq!(stage(rig), "skin");
        assert_eq!(polled(rig, &mut attempt).unwrap(), game::Preparation::Ready);
        assert_eq!(jobs_since(from), (1, 1));
    });
}

/// B2d: a retry specializes only in Preparing: the decision's early
/// preparation was the first attempt's, taken there.
#[test]
fn a_retry_specializes_in_preparing() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let (first, retry) = (at_level(13), at_level(14));
    on_rig(first.chart(), &Installers::default(), true, |rig| {
        let gate = decided(rig, skin);
        let from = imports::jobs_counted();
        rig.chart_read(&first);
        until("the specialization ended", || {
            state(&gate) == "ended specialized"
        });
        assert_eq!(rig.deadline_step(), game::StepOutcome::Stepped);
        let mut attempt = begin(rig, &first);
        assert_ne!(stage(rig), "skin");
        assert_eq!(polled(rig, &mut attempt).unwrap(), game::Preparation::Ready);
        assert!(rig.early.is_none(), "taken by the first attempt");
        let mut again = begin(rig, &retry);
        assert_eq!(stage(rig), "skin", "the retry's own job");
        assert_eq!(polled(rig, &mut again).unwrap(), game::Preparation::Ready);
        assert_eq!(jobs_since(from), (2, 2));
    });
}

/// B9c: a refusal published before a load wait's look ahead of the
/// deadline latches there; at the deadline the look returns, revealing
/// nothing.
#[test]
fn an_early_refusal_latches_at_a_load_wait_look_before_the_deadline() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        let gate = decided(rig, skin);
        imports::refuse_spawn();
        rig.chart_read(&l);
        assert_eq!(state(&gate), "ended refused");
        let reveals = Arc::new(AtomicUsize::new(0));
        let now = bms_model::TimeSource::now(&rig.clock);
        let d = game::Decision::begin(
            now,
            game::DecisionLifetime(1),
            Some(game::DecisionShown::BuiltIn),
            false,
        );
        let deadline = d.deadline().unwrap();
        watch(rig, d, &reveals);
        assert_eq!(rig.controls.look(), Some(game::Verdict::Continue));
        assert!(
            bms_model::TimeSource::now(&rig.clock).0 < deadline.0,
            "looked before the deadline"
        );
        assert!(rig.controls.with_watched(|d| d.unwrap().latched()));
        until("the deadline", || {
            bms_model::TimeSource::now(&rig.clock).0 >= deadline.0
        });
        assert_eq!(rig.controls.look(), Some(game::Verdict::Return));
        let d = rig.controls.unwatch_decision().unwrap();
        assert!(!d.stepped());
        assert_eq!(reveals.load(Ordering::SeqCst), 0);
        rig.left_chart().unwrap();
    });
}

/// B9c2: a refusal published between a load wait's classification and
/// its commit reveals nothing and leaves the decision unstepped.
#[test]
fn a_refusal_between_classify_and_commit_is_not_revealed() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        let gate = decided(rig, skin);
        let published = Arc::clone(&gate);
        gate.before_commit(move || {
            assert!(published.end(crate::play::early::EarlyOutcome::Refused(
                game::PreparationRefusal {
                    location: None,
                    reason: "refused between".to_owned(),
                }
            )));
        });
        let reveals = Arc::new(AtomicUsize::new(0));
        watch_past(rig, &reveals);
        assert_eq!(rig.controls.look(), Some(game::Verdict::Return));
        let d = rig.controls.unwatch_decision().unwrap();
        assert!(!d.stepped());
        assert_eq!(reveals.load(Ordering::SeqCst), 0, "nothing revealed");
        assert_eq!(
            gate.commit_reveal(),
            Committed::Refused(game::PreparationRefusal {
                location: None,
                reason: "refused between".to_owned(),
            })
        );
        rig.chart_read(&l);
        rig.left_chart().unwrap();
    });
}

/// B10: an early staging refused after the reveal ends the attempt at
/// once with its reason: the attempt's request is never sent, nothing
/// staged afresh.
// Revert: mapped to `Cancelled` -> the preparation answers cancelled.
#[test]
fn a_staging_refusal_after_the_reveal_returns_at_once() {
    use std::sync::atomic::Ordering::SeqCst;

    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    let screens = Installers::default();
    on_rig(l.chart(), &screens, true, |rig| {
        let gate = decided(rig, skin);
        let link = crate::play::early::EarlyLink::new(rig.render.early_sender());
        rig.early = rig.early.take().map(|e| e.with_link(link));
        rig.render
            .show_decision(crate::render_side::tests::one_line("page"), None);
        // The early staging waits for its room.
        screens.early_room.store(1, SeqCst);
        rig.chart_read(&l);
        until("the specialization ended", || {
            state(&gate) == "ended specialized"
        });
        rig.step_early();
        let request = gate.request().expect("the early request sent");
        assert_eq!(rig.deadline_step(), game::StepOutcome::Stepped);
        // Refused after the reveal's commit.
        screens.early_room.store(3, SeqCst);
        until("the early staging refused", || {
            rig.render.early_refused(request).is_some()
        });
        let mut handle = begin(rig, &l);
        let answer = polled(rig, &mut handle).unwrap();
        assert!(
            matches!(answer, game::Preparation::Refused(_)),
            "{answer:?}"
        );
        assert!(
            !screens.log().iter().any(|s| s.contains(" prepare ")),
            "{:?}",
            screens.log()
        );
        rig.discard_attempt();
        rig.left_chart().unwrap();
    });
}

/// An early staging refused after a load wait's reveal, the chart's load
/// still waited for: the load's next look returns with the reason, which
/// App's route reads, before the bank's load ends and with no attempt
/// begun.
// Revert: the late refusal not read by the look -> the look goes on.
#[test]
fn a_staging_refusal_after_a_load_wait_reveal_ends_the_load() {
    use std::sync::atomic::Ordering::SeqCst;

    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    let screens = Installers::default();
    on_rig(l.chart(), &screens, true, |rig| {
        let gate = decided(rig, skin);
        let link = crate::play::early::EarlyLink::new(rig.render.early_sender());
        rig.early = rig.early.take().map(|e| e.with_link(link));
        rig.render
            .show_decision(crate::render_side::tests::one_line("page"), None);
        screens.early_room.store(1, SeqCst);
        rig.chart_read(&l);
        until("the specialization ended", || {
            state(&gate) == "ended specialized"
        });
        rig.step_early();
        let request = gate.request().expect("the early request sent");
        let reveals = Arc::new(AtomicUsize::new(0));
        watch_past(rig, &reveals);
        assert_eq!(rig.controls.look(), Some(game::Verdict::Step));
        assert_eq!(reveals.load(Ordering::SeqCst), 1);
        assert_eq!(rig.controls.look(), Some(game::Verdict::Continue));
        assert_eq!(rig.early_refusal(), None);
        // Refused while the load's looks go on.
        screens.early_room.store(3, SeqCst);
        until("the early staging refused", || {
            rig.render.early_refused(request).is_some()
        });
        assert_eq!(rig.controls.look(), Some(game::Verdict::Return));
        let d = rig.controls.unwatch_decision().unwrap();
        assert!(d.stepped());
        assert_eq!(rig.early_refusal(), rig.render.early_refused(request));
        assert!(rig.prep.is_none(), "no attempt begun");
        rig.left_chart().unwrap();
    });
}

/// An early staging refused after the reveal while the attempt's
/// preparation waits in the result skin's image stage (its script loops
/// until cancelled) ends it at that poll with the reason: the image
/// stage settled, its images handed back, and the attempt's request never
/// sent.
// Revert: the polls' check removed -> the preparation goes on waiting.
#[test]
fn a_staging_refusal_ends_a_waiting_preparation() {
    use std::sync::atomic::Ordering::SeqCst;

    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let (_r, rs) = staged_result(LEVEL_RESULT);
    let first = Arc::clone(rs.image().unwrap());
    let l = at_level(13);
    let screens = Installers::default();
    on_rig(l.chart(), &screens, true, |rig| {
        rig.result_skin = rs;
        let gate = decided(rig, skin);
        let link = crate::play::early::EarlyLink::new(rig.render.early_sender());
        rig.early = rig.early.take().map(|e| e.with_link(link));
        rig.render
            .show_decision(crate::render_side::tests::one_line("page"), None);
        screens.early_room.store(1, SeqCst);
        rig.chart_read(&l);
        until("the specialization ended", || {
            state(&gate) == "ended specialized"
        });
        rig.step_early();
        let request = gate.request().expect("the early request sent");
        assert_eq!(rig.deadline_step(), game::StepOutcome::Stepped);
        let mut handle = begin(rig, &l);
        assert_eq!(stage(rig), "image");
        let slice = || Instant::now() + Duration::from_millis(10);
        assert!(matches!(
            rig.poll_preparation(&mut handle, slice()),
            Ok(game::PrepPoll::Pending)
        ));
        screens.early_room.store(3, SeqCst);
        until("the early staging refused", || {
            rig.render.early_refused(request).is_some()
        });
        let refused = rig.render.early_refused(request).unwrap();
        assert!(matches!(
            rig.poll_preparation(&mut handle, slice()),
            Ok(game::PrepPoll::Done(game::Preparation::Refused(r))) if r == refused
        ));
        assert!(rig.early_handoff.is_none(), "the hand-off dropped");
        assert!(
            rig.result_skin
                .image()
                .is_some_and(|i| Arc::ptr_eq(i, &first)),
            "its images handed back"
        );
        assert!(
            !screens.log().iter().any(|s| s.contains(" prepare ")),
            "{:?}",
            screens.log()
        );
        rig.discard_attempt();
        rig.left_chart().unwrap();
    });
}

/// B5e (player): an attempt adopting an early request whose staging
/// released the result begins no result pages upload (the result's step
/// reconciles the release); without the release its pages ticket begins.
// Revert: `begin_pages_ticket` ignores the release -> a ticket begun.
#[test]
fn a_released_result_begins_no_pages_upload() {
    use std::sync::atomic::Ordering::SeqCst;

    use crate::result_skin::{ResultSkin, ResultStart};

    let _serial = imports::serial();
    for release in [false, true] {
        let (_dir, skin) = pending_play(LEVEL_SKIN);
        let l = at_level(13);
        let screens = Installers::default();
        let result = super::result_flow::lua_result(
            "local nr = ms.number(154)",
            &format!("d[#d + 1] = {}", super::result_flow::MARK_DST),
        );
        let p =
            crate::result_skin::tests::pending(result.path(), crate::result_skin::tests::LIMITS);
        on_rig(l.chart(), &screens, true, |rig| {
            rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))))
                .with_scripts(rig.render.scripts().clone());
            let gate = decided(rig, skin);
            let link = crate::play::early::EarlyLink::new(rig.render.early_sender());
            rig.early = rig.early.take().map(|e| e.with_link(link));
            rig.render
                .show_decision(crate::render_side::tests::one_line("page"), None);
            screens
                .early_room
                .store(if release { 2 } else { 0 }, SeqCst);
            rig.chart_read(&l);
            until("the specialization ended", || {
                state(&gate) == "ended specialized"
            });
            rig.step_early();
            let e = gate.request().expect("the early request sent");
            if release {
                until("the result released", || {
                    rig.render.result_status().released == Some(e)
                });
            }
            assert_eq!(rig.deadline_step(), game::StepOutcome::Stepped);
            let mut handle = begin(rig, &l);
            assert_eq!(polled(rig, &mut handle).unwrap(), game::Preparation::Ready);
            assert_eq!(rig.pages_ticket.is_some(), !release, "release {release}");
            if release {
                assert_ne!(rig.render.result_status().released, Some(e), "followed");
            }
            rig.discard_attempt();
            rig.left_chart().unwrap();
        });
    }
}
