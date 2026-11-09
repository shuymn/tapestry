//! Replacement and discard settled through the attempt-quit path (S10 item
//! 9.4a plan §2.6, H1-H7b): the job is cancelled and waited for to its
//! thread's end without a bound, the run's end still asked meanwhile and
//! bounding the wait once it came; after any settle, the run's end is
//! looked at again before anything new begins.

use game::Preparation;

use super::*;

/// H1: a replacement while the specialization's thread is held after its
/// successful outcome waits for that thread past the stop bound (today's
/// bounded settle would have given it up), joins it, and reuses the kept
/// specialization: no new job, the replacement ready.
#[test]
fn replacing_a_pending_specialization_waits_for_its_thread() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        rig.pending = Some(skin);
        let from = imports::jobs_counted();
        imports::hold_exit(true);
        let _first = begin(rig, &l);
        assert_eq!(stage(rig), "skin");
        until("the job held after its send", || {
            imports::exit_waiting() == 1
        });
        let (mut second, after_release) =
            held_past(|| imports::hold_exit(false), || begin(rig, &l));
        assert!(after_release, "the settle returned before the thread ended");
        assert_eq!(jobs_since(from), (1, 1), "the old job joined, no new one");
        assert_ne!(stage(rig), "skin", "the kept specialization is reused");
        assert_eq!(polled(rig, &mut second).unwrap(), Preparation::Ready);
        assert!(matches!(
            rig.ready_attempt(1, &second),
            Ok(game::Readiness::Ready)
        ));
    });
}

/// H1: only a successful specialization is kept through a replacement. One
/// whose thread ended with its bundle is reused by the replacement, with no
/// job; one replaced while it ran is cancelled, ends unfinished, and is not
/// kept: the replacement with the same facts runs a job again.
#[test]
fn a_kept_specialization_is_reused_only_after_success() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let (built, looping) = (at_level(13), at_level(12));
    on_rig(built.chart(), &Installers::default(), true, |rig| {
        rig.pending = Some(skin);
        let from = imports::jobs_counted();
        let _a = begin(rig, &built);
        until(
            "the specialization's thread ended",
            || matches!(rig.prep.as_ref().map(|p| &p.stage), Some(PrepStage::Skin(j)) if j.finished()),
        );
        let _b = begin(rig, &built);
        assert_ne!(stage(rig), "skin", "a successful one is reused");
        assert_eq!(jobs_since(from), (1, 1));

        let base = vm_hooks::live();
        let _c = begin(rig, &looping);
        assert_eq!(stage(rig), "skin");
        imports::executing(base);
        let _d = begin(rig, &looping);
        assert_eq!(stage(rig), "skin", "an unfinished one is not kept");
        rig.discard_attempt();
        assert_eq!(jobs_since(from), (3, 3));
        assert_eq!(vm_hooks::live(), base);
    });
}

/// H2: a replacement while the result skin's image stage's thread is held
/// waits for it past the stop bound, joins it and hands its images back:
/// the replacement's image stage takes them, and gives the very images
/// back once it ends.
#[test]
fn replacing_a_pending_image_stage_hands_its_images_back() {
    let _serial = imports::serial();
    let (_d, rs) = staged_result("local lv = ms.number(96)");
    let first = Arc::clone(rs.image().unwrap());
    let (l13, l14) = (at_level(13), at_level(14));
    on_rig(l13.chart(), &Installers::default(), false, |rig| {
        rig.result_skin = rs;
        let from = imports::jobs_counted();
        imports::hold_exit(true);
        let _a = begin(rig, &l13);
        assert_eq!(stage(rig), "image");
        until("the job held after its send", || {
            imports::exit_waiting() == 1
        });
        let (mut b, after_release) = held_past(|| imports::hold_exit(false), || begin(rig, &l14));
        assert!(after_release, "the settle returned before the thread ended");
        assert_eq!(stage(rig), "image", "the replacement's image stage runs");
        assert!(
            rig.result_skin.image().is_none(),
            "its job holds the images"
        );
        assert_eq!(polled(rig, &mut b).unwrap(), Preparation::Ready);
        assert!(
            rig.result_skin
                .image()
                .is_some_and(|i| Arc::ptr_eq(i, &first)),
            "the very images came back"
        );
        assert_eq!(jobs_since(from), (2, 2));
    });
}

/// H3: a replacement while the render thread holds the old request's
/// staging discards it: the old staging installs nothing, then or once it
/// is let go, and the replacement is committed with its own skin.
#[test]
fn a_replaced_screen_request_installs_nothing_and_the_new_one_commits() {
    let _serial = imports::serial();
    let r = crate::render_common::offscreen(1_920, 1_080);
    let t = imports::target(&r);
    let dir = imports::class_skin();
    let (p, _) = imports::pending(imports::play(
        &imports::choice(dir.path(), "play.luaskin"),
        &t,
    ));
    let (normal, easy) = (
        loaded("#RANK 2\n", "tapestry"),
        loaded("#RANK 3\n", "tapestry"),
    );
    let screens = Installers::default();
    on_rig(normal.chart(), &screens, true, |rig| {
        rig.pending = Some(crate::imports::PendingSkin::new(p));
        let mut a = begin(rig, &normal);
        screens.hold_next_staging(&rig.render);
        until("the old request staged", || {
            let until = Instant::now() + Duration::from_millis(10);
            assert!(matches!(
                rig.poll_preparation(&mut a, until),
                Ok(game::PrepPoll::Pending)
            ));
            screens.staging().is_some()
        });
        assert_eq!(stage(rig), "screen");
        let old = screens.staging().unwrap();
        let mut b = begin(rig, &easy);
        assert_eq!(polled(rig, &mut b).unwrap(), Preparation::Ready);
        assert!(matches!(
            rig.ready_attempt(1, &b),
            Ok(game::Readiness::Ready)
        ));
        rig.commit_attempt().unwrap();
        let new = screens.play().expect("the replacement installed");
        assert!(!Arc::ptr_eq(&new, &old));
        screens.release_staged();
        assert!(screens.staging().is_none(), "nothing of the old staged");
        assert!(screens.play().is_some_and(|p| Arc::ptr_eq(&p, &new)));
        let log = screens.log();
        let prepares: Vec<usize> = (0..log.len())
            .filter(|&i| log[i].contains(" prepare "))
            .collect();
        assert_eq!(prepares.len(), 2, "{log:?}");
        assert!(
            log[prepares[0]..prepares[1]]
                .iter()
                .any(|l| l.ends_with(" discard")),
            "the old request discarded before the new one: {log:?}"
        );
    });
}

/// H4: the attempt's quit while the specialization runs (the flow then
/// discards the preparation): the discard waits for the job's thread,
/// its VM held past the stop bound, joins it and returns once it ended.
#[test]
fn a_quit_while_waiting_settles_the_running_specialization() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(12);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        rig.pending = Some(skin);
        let from = imports::jobs_counted();
        let base = vm_hooks::live();
        vm_hooks::hold();
        let _h = begin(rig, &l);
        imports::executing(base);
        rig.controls.command(game::Command::Quit);
        let ((), after_release) = held_past(vm_hooks::release, || rig.discard_attempt());
        assert!(
            after_release,
            "the discard returned before the thread ended"
        );
        assert_eq!(jobs_since(from), (1, 1), "joined");
        assert_eq!(vm_hooks::live(), base);
        assert_eq!(stage(rig), "none");
    });
}

/// H4: as [`a_quit_while_waiting_settles_the_running_specialization`] for
/// the result skin's image stage: joined, its images handed back.
#[test]
fn a_quit_while_waiting_settles_the_running_image_stage() {
    let _serial = imports::serial();
    let (_d, rs) = staged_result(LEVEL_RESULT);
    let first = Arc::clone(rs.image().unwrap());
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), false, |rig| {
        rig.result_skin = rs;
        let from = imports::jobs_counted();
        let base = vm_hooks::live();
        vm_hooks::hold();
        let _h = begin(rig, &l);
        assert_eq!(stage(rig), "image");
        imports::executing(base);
        rig.controls.command(game::Command::Quit);
        let ((), after_release) = held_past(vm_hooks::release, || rig.discard_attempt());
        assert!(
            after_release,
            "the discard returned before the thread ended"
        );
        assert_eq!(jobs_since(from), (1, 1), "joined");
        assert!(
            rig.result_skin
                .image()
                .is_some_and(|i| Arc::ptr_eq(i, &first)),
            "its images handed back"
        );
    });
}

/// The stages whose job a settle waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    Skin,
    Image,
}

/// A rig on which `held`'s job loops at one level and builds at the next:
/// `f` given the rig, the looping level's chart, the next one's and the
/// VMs open before.
fn with_running(
    held: Held,
    f: impl FnOnce(&mut PlayerRig<'_>, &game::Loaded, &game::Loaded, usize),
) {
    let (_p, skin) = pending_play(LEVEL_SKIN);
    let (_r, rs) = staged_result(LEVEL_RESULT);
    let (looping, next, pending) = match held {
        Held::Skin => (at_level(12), at_level(13), true),
        Held::Image => (at_level(13), at_level(14), false),
    };
    on_rig(looping.chart(), &Installers::default(), pending, |rig| {
        if pending {
            rig.pending = Some(skin);
        }
        rig.result_skin = rs;
        let base = vm_hooks::live();
        f(rig, &looping, &next, base);
    });
}

/// H5a: a close, an input failure or an audio failure during a
/// replacement's settle, the old job's VM held: the settle gives the job
/// up within the stop bound of the signal, no new handle begins (the
/// replacement is cancelled, no job started), and the rig shows the cause.
/// The abandoned thread is counted once the test let it go: started,
/// never joined. One case per signal and per stage.
#[test]
fn a_close_during_a_replacement_settle_ends_the_run_without_opening() {
    let _serial = imports::serial();
    for held in [Held::Skin, Held::Image] {
        for signal in Signal::ALL {
            let case = format!("{held:?} {signal:?}");
            with_running(held, |rig, looping, next, base| {
                let from = imports::jobs_counted();
                vm_hooks::hold();
                let _a = begin(rig, looping);
                imports::executing(base);
                let give = signal.giver(rig);
                let (mut b, took) = std::thread::scope(|s| {
                    let helper = s.spawn(|| {
                        until("the settle cancelled the job", || vm_hooks::waiting() == 1);
                        let at = Instant::now();
                        give();
                        at
                    });
                    let b = begin(rig, next);
                    let returned = Instant::now();
                    (b, returned - helper.join().unwrap())
                });
                assert!(
                    took >= STOP_WAIT && took < STOP_WAIT + LATE,
                    "{case}: {took:?}"
                );
                assert_eq!(stage(rig), "ended", "{case}");
                assert_eq!(
                    polled(rig, &mut b).unwrap(),
                    Preparation::Cancelled,
                    "{case}"
                );
                assert!(signal.shown(rig), "{case}");
                if held == Held::Image {
                    assert!(rig.result_skin.image().is_none(), "{case}: kept by the job");
                }
                release_workers();
                assert_eq!(jobs_since(from), (1, 0), "{case}: abandoned");
                assert_eq!(vm_hooks::live(), base, "{case}");
            });
        }
    }
}

/// H5b: the same signals with the old job ending at once once cancelled:
/// the settle joins it, and the look at the run's end after it begins
/// nothing: the replacement is cancelled, with no job started.
#[test]
fn a_quick_settle_after_a_close_begins_nothing() {
    let _serial = imports::serial();
    for held in [Held::Skin, Held::Image] {
        for signal in Signal::ALL {
            let case = format!("{held:?} {signal:?}");
            with_running(held, |rig, looping, next, base| {
                let from = imports::jobs_counted();
                let _a = begin(rig, looping);
                imports::executing(base);
                signal.giver(rig)();
                let mut b = begin(rig, next);
                assert_eq!(stage(rig), "ended", "{case}");
                assert_eq!(
                    polled(rig, &mut b).unwrap(),
                    Preparation::Cancelled,
                    "{case}"
                );
                assert_eq!(jobs_since(from), (1, 1), "{case}: joined, none begun");
                assert!(signal.shown(rig), "{case}");
            });
        }
    }
}

/// H6: the replaced job panics (its thread held after the panic's
/// refusal was sent, past the stop bound): the settle joins it, keeps
/// nothing, and the replacement runs its own job and is ready.
#[test]
fn a_panic_in_a_replaced_job_does_not_block_the_new_one() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), true, |rig| {
        rig.pending = Some(skin);
        let from = imports::jobs_counted();
        imports::panic_next_job();
        imports::hold_exit(true);
        let _a = begin(rig, &l);
        until("the panicked job held", || imports::exit_waiting() == 1);
        let (mut b, after_release) = held_past(|| imports::hold_exit(false), || begin(rig, &l));
        assert!(after_release);
        assert_eq!(
            stage(rig),
            "skin",
            "nothing kept: the replacement runs a job"
        );
        assert_eq!(polled(rig, &mut b).unwrap(), Preparation::Ready);
        assert!(matches!(
            rig.ready_attempt(1, &b),
            Ok(game::Readiness::Ready)
        ));
        assert_eq!(jobs_since(from), (2, 2));
    });
}

/// H7: the old specialization settled (successful, kept for its facts),
/// then the replacement's, for other facts, is refused its thread: the
/// replacement ends refused, and nothing of the old survives: its facts
/// again run a job.
#[test]
fn a_refused_replacement_specialization_refuses_and_keeps_nothing_old() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let (l13, l14) = (at_level(13), at_level(14));
    on_rig(l13.chart(), &Installers::default(), true, |rig| {
        rig.pending = Some(skin);
        let from = imports::jobs_counted();
        let _a = begin(rig, &l13);
        until(
            "the specialization's thread ended",
            || matches!(rig.prep.as_ref().map(|p| &p.stage), Some(PrepStage::Skin(j)) if j.finished()),
        );
        imports::refuse_spawn();
        let mut b = begin(rig, &l14);
        assert_eq!(stage(rig), "ended");
        match polled(rig, &mut b).unwrap() {
            Preparation::Refused(why) => {
                assert!(why.reason.contains("cannot start"), "{}", why.reason);
            }
            other => panic!("{other:?}"),
        }
        let _c = begin(rig, &l13);
        assert_eq!(stage(rig), "skin", "the old specialization is gone");
        rig.discard_attempt();
        assert_eq!(jobs_since(from), (2, 2));
    });
}

/// H7b: the old image stage settled (its images handed back), then the
/// replacement's image stage is refused its thread: it falls back, laid
/// out per view with the images, and the attempt is ready and committed.
#[test]
fn a_refused_replacement_image_stage_falls_back_and_continues() {
    let _serial = imports::serial();
    let (_d, rs) = staged_result(LEVEL_RESULT);
    let first = Arc::clone(rs.image().unwrap());
    let (l13, l14) = (at_level(13), at_level(14));
    on_rig(l13.chart(), &Installers::default(), false, |rig| {
        rig.result_skin = rs;
        let base = vm_hooks::live();
        let _a = begin(rig, &l13);
        imports::executing(base);
        imports::refuse_spawn();
        let mut b = begin(rig, &l14);
        assert_eq!(stage(rig), "screen", "the fallback continues");
        assert!(
            rig.result_skin
                .image()
                .is_some_and(|i| Arc::ptr_eq(i, &first)),
            "the images handed back"
        );
        assert_eq!(polled(rig, &mut b).unwrap(), Preparation::Ready);
        assert!(matches!(
            rig.ready_attempt(1, &b),
            Ok(game::Readiness::Ready)
        ));
        rig.commit_attempt().unwrap();
        assert!(
            rig.result_skin.staged_line().starts_with("per view"),
            "{}",
            rig.result_skin.staged_line()
        );
    });
}
