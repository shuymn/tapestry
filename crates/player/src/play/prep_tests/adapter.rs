//! The per-stage adapter matrix (S10 item 9.4a plan §2.7): one case per
//! stage and event, each with its own oracle. A cell whose outcome is the
//! flow's (the run's end and its cause) runs the chart's first attempt
//! through `game::live::run_chart` on a rig whose input link an input
//! thread serves; the others drive the rig's preparation directly.

use std::sync::atomic::Ordering;

use game::{Leave, LiveError, Preparation};

use super::*;
use crate::render_side::ScreenError;

/// How the flow ended a run.
type Ended = Result<Leave, LiveError<ScreenError>>;

/// The stages of an attempt's preparation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Skin,
    Image,
    Screen,
}

/// As [`on_rig`], with an input thread serving the rig's input link, so
/// the flow's handshakes with it are answered: what an attempt's
/// preparation through `game::live` needs.
fn on_flow_rig<R>(
    chart: &bms_model::Chart,
    screens: &Installers,
    pending: bool,
    f: impl FnOnce(&mut PlayerRig<'_>) -> R,
) -> R {
    use input::{DeviceId, DeviceKind, SourceConfig, SourceSet};
    let render = render_for(chart, screens, pending);
    crate::tests::on_rig_hooked(
        settings(),
        render,
        input::QpcClock::start(),
        |rig, _, end| {
            let crate::tests::InputEnd {
                mut hook,
                waker,
                clock,
            } = end;
            let mut sources = SourceSet::new(SourceConfig {
                max_devices: 2,
                max_sources: 2,
                max_held: 16,
                queue_capacity: 64,
                max_arrivals: 2,
            })
            .unwrap();
            sources.begin_batch(bms_model::TimeUs(0));
            sources.connect(DeviceId(1), DeviceKind::Keyboard);
            sources.end_batch(bms_model::TimeUs(0));
            let thread = input::ClockedThread::spawn(clock, sources, &waker, move |s, now| {
                hook.wake(s, now)
            })
            .unwrap();
            let out = {
                let _let_go = LetGo;
                f(rig)
            };
            thread.stop().unwrap();
            out
        },
    )
    .0
}

/// The chart's first attempt at `level` run through the flow on a rig
/// with the pending play skin `skin`, `arm` called on the rig first: how
/// the run ended, the preparations begun and the render log.
fn run_flow(
    level: u32,
    skin: crate::imports::PendingSkin,
    arm: impl FnOnce(&mut PlayerRig<'_>),
) -> (Ended, u64, Vec<String>) {
    let mut l = at_level(level);
    let chart = l.chart().clone();
    let screens = Installers::default();
    let (ended, preps) = on_flow_rig(&chart, &screens, true, |rig| {
        rig.pending = Some(skin);
        arm(rig);
        (game::live::run_chart(&mut l, rig), rig.preps)
    });
    (ended, preps, screens.log())
}

/// What a signalled flow cell gave.
struct Signalled {
    ended: Ended,
    /// From the signal to the flow's return.
    took: Duration,
    /// The jobs started and joined, counted once every job thread ended.
    jobs: (u64, u64),
    log: Vec<String>,
}

/// The chart's first attempt through the flow with `signal` given while
/// `stage` runs: the specialization looping at level 12, or the image
/// stage looping at level 13 (a fixed play skin), each with its VM held
/// so its thread does not end by itself; or the render thread holding
/// the attempt's staging unanswered (level 14, nothing looping). Every
/// held thread is let go once the flow returned, and the jobs counted
/// once every job thread ended.
fn signalled_in(stage: Stage, signal: Signal) -> Signalled {
    let (_p, skin) = pending_play(LEVEL_SKIN);
    let (_r, rs) = staged_result(LEVEL_RESULT);
    let (level, pending) = match stage {
        Stage::Skin => (12, true),
        Stage::Image => (13, false),
        Stage::Screen => (14, true),
    };
    let mut l = at_level(level);
    let chart = l.chart().clone();
    let screens = Installers::default();
    let (ended, took, jobs) = on_flow_rig(&chart, &screens, pending, |rig| {
        if pending {
            rig.pending = Some(skin);
        }
        rig.result_skin = rs;
        let from = imports::jobs_counted();
        let base = vm_hooks::live();
        if stage == Stage::Screen {
            screens.hold_next_staging(&rig.render);
        } else {
            vm_hooks::hold();
        }
        let give = signal.giver(rig);
        let (ended, took) = std::thread::scope(|s| {
            let helper = s.spawn(|| {
                if stage == Stage::Screen {
                    until("the attempt staged", || screens.staging().is_some());
                } else {
                    imports::executing(base);
                }
                let at = Instant::now();
                give();
                at
            });
            let ended = game::live::run_chart(&mut l, rig);
            let returned = Instant::now();
            (ended, returned - helper.join().unwrap())
        });
        screens.release_staged();
        release_workers();
        (ended, took, jobs_since(from))
    });
    Signalled {
        ended,
        took,
        jobs,
        log: screens.log(),
    }
}

/// A held job's cell: the flow gave the job up within the stop bound of
/// the signal, and the job, counted once its thread ended after the
/// test let it go, was started and never joined.
#[track_caller]
fn abandoned(s: &Signalled) {
    assert!(
        s.took >= STOP_WAIT && s.took < STOP_WAIT + LATE,
        "{:?}",
        s.took
    );
    assert_eq!(s.jobs, (1, 0), "abandoned, never joined");
}

/// A Screen cell: the staged request was discarded and nothing committed.
#[track_caller]
fn discarded(s: &Signalled) {
    let staged = s
        .log
        .iter()
        .position(|l| l.contains(" prepare "))
        .expect("the attempt staged");
    assert!(
        s.log[staged..].iter().any(|l| l.ends_with(" discard")),
        "{:?}",
        s.log
    );
    assert!(!s.log.iter().any(|l| l.contains(" commit ")), "{:?}", s.log);
    assert_eq!(s.jobs, (2, 2), "the skin's and the image stage's, joined");
}

// Skin.

/// Skin × quit: cancelled once the job's thread ended, joined; a
/// specialization that ended successfully is kept (the next attempt with
/// its facts reuses it), an unfinished one is not.
#[test]
fn the_skin_stage_on_a_quit_cancels_and_keeps_only_a_success() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let (built, looping) = (at_level(13), at_level(12));
    on_rig(built.chart(), &Installers::default(), true, |rig| {
        rig.pending = Some(skin);
        let from = imports::jobs_counted();
        for (l, kept) in [(&built, true), (&looping, false)] {
            let base = vm_hooks::live();
            let mut h = begin(rig, l);
            if kept {
                until(
                    "the specialization's thread ended",
                    || matches!(rig.prep.as_ref().map(|p| &p.stage), Some(PrepStage::Skin(j)) if j.finished()),
                );
            } else {
                imports::executing(base);
            }
            rig.controls.command(game::Command::Quit);
            assert_eq!(polled(rig, &mut h).unwrap(), Preparation::Cancelled);
            while rig.take().is_some() {}
            assert!(!rig.controls.leaving());
            let _next = begin(rig, l);
            assert_eq!(stage(rig) != "skin", kept, "kept {kept}");
            rig.discard_attempt();
        }
        // The two quits' jobs and the unfinished one's next attempt.
        assert_eq!(jobs_since(from), (3, 3));
    });
}

/// Skin × close: cancelled within the stop bound, the job abandoned; the
/// run ends as the window closes.
#[test]
fn the_skin_stage_ends_the_run_on_a_close() {
    let _serial = imports::serial();
    let s = signalled_in(Stage::Skin, Signal::Close);
    assert!(matches!(s.ended, Ok(Leave::Quit)), "{:?}", s.ended);
    abandoned(&s);
}

/// Skin × input failure: the run ends with the input thread's failure.
#[test]
fn the_skin_stage_ends_the_run_on_an_input_failure() {
    let _serial = imports::serial();
    let s = signalled_in(Stage::Skin, Signal::Input);
    assert!(matches!(s.ended, Err(LiveError::Input(_))), "{:?}", s.ended);
    abandoned(&s);
}

/// Skin × audio failure: the run ends with the output's failure.
#[test]
fn the_skin_stage_ends_the_run_on_an_audio_failure() {
    let _serial = imports::serial();
    let s = signalled_in(Stage::Skin, Signal::Audio);
    assert!(
        matches!(s.ended, Err(LiveError::Output(_))),
        "{:?}",
        s.ended
    );
    abandoned(&s);
}

/// Skin × panic in the job: the attempt is refused with the reason, and
/// nothing is committed.
#[test]
fn the_skin_stage_refuses_on_a_panic() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let (ended, preps, log) = run_flow(13, skin, |_| imports::panic_next_job());
    match ended {
        Err(LiveError::Preparation(why)) => {
            assert!(
                why.reason.contains(crate::imports::PANICKED),
                "{}",
                why.reason
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(preps, 1);
    assert!(!log.iter().any(|l| l.contains(" commit ")), "{log:?}");
}

/// Skin × the job's thread refused its start: the attempt is refused
/// with the reason, and nothing is committed.
#[test]
fn the_skin_stage_refuses_when_its_thread_cannot_start() {
    let _serial = imports::serial();
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let (ended, preps, log) = run_flow(13, skin, |_| imports::refuse_spawn());
    match ended {
        Err(LiveError::Preparation(why)) => {
            assert!(why.reason.contains("cannot start"), "{}", why.reason);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(preps, 1);
    assert!(!log.iter().any(|l| l.contains(" commit ")), "{log:?}");
}

/// The precedence cases: a Skin refusal (its thread's start refused)
/// together with a failure of the input thread, or of the audio output,
/// that comes as the job's thread starts, after the flow's first looks at
/// the threads. The run ends with the failure, never the refusal, which
/// was reached: one preparation was begun.
fn refusal_with(signal: Signal) -> LiveError<ScreenError> {
    let (_dir, skin) = pending_play(LEVEL_SKIN);
    let (ended, preps, _) = run_flow(13, skin, |rig| {
        let input = rig.input.failer();
        let audio = Arc::clone(&rig.audio_lost);
        imports::on_next_spawn(move || match signal {
            Signal::Input => input(),
            Signal::Audio => audio.store(true, Ordering::Release),
            Signal::Close => unreachable!("not a precedence case"),
        });
        imports::refuse_spawn();
    });
    assert_eq!(
        preps, 1,
        "{signal:?}: the preparation was begun and refused"
    );
    match ended {
        Err(e) => e,
        Ok(leave) => panic!("{signal:?}: {leave:?}"),
    }
}

#[test]
fn a_skin_refusal_with_an_input_failure_ends_with_the_input() {
    let _serial = imports::serial();
    let e = refusal_with(Signal::Input);
    assert!(matches!(e, LiveError::Input(_)), "{e:?}");
}

#[test]
fn a_skin_refusal_with_an_audio_failure_ends_with_the_output() {
    let _serial = imports::serial();
    let e = refusal_with(Signal::Audio);
    assert!(matches!(e, LiveError::Output(_)), "{e:?}");
}

// Image.

/// Image × quit: cancelled once the job's thread ended, joined, its images
/// handed back.
#[test]
fn the_image_stage_on_a_quit_cancels_and_hands_its_images_back() {
    let _serial = imports::serial();
    let (_d, rs) = staged_result(LEVEL_RESULT);
    let first = Arc::clone(rs.image().unwrap());
    let l = at_level(13);
    on_rig(l.chart(), &Installers::default(), false, |rig| {
        rig.result_skin = rs;
        let from = imports::jobs_counted();
        let base = vm_hooks::live();
        let mut h = begin(rig, &l);
        assert_eq!(stage(rig), "image");
        imports::executing(base);
        rig.controls.command(game::Command::Quit);
        assert_eq!(polled(rig, &mut h).unwrap(), Preparation::Cancelled);
        assert_eq!(jobs_since(from), (1, 1));
        assert!(
            rig.result_skin
                .image()
                .is_some_and(|i| Arc::ptr_eq(i, &first)),
            "its images handed back"
        );
    });
}

/// Image × close: cancelled within the stop bound, the job abandoned; the
/// run ends as the window closes.
#[test]
fn the_image_stage_ends_the_run_on_a_close() {
    let _serial = imports::serial();
    let s = signalled_in(Stage::Image, Signal::Close);
    assert!(matches!(s.ended, Ok(Leave::Quit)), "{:?}", s.ended);
    abandoned(&s);
}

/// Image × input failure: the run ends with the input thread's failure.
#[test]
fn the_image_stage_ends_the_run_on_an_input_failure() {
    let _serial = imports::serial();
    let s = signalled_in(Stage::Image, Signal::Input);
    assert!(matches!(s.ended, Err(LiveError::Input(_))), "{:?}", s.ended);
    abandoned(&s);
}

/// Image × audio failure: the run ends with the output's failure.
#[test]
fn the_image_stage_ends_the_run_on_an_audio_failure() {
    let _serial = imports::serial();
    let s = signalled_in(Stage::Image, Signal::Audio);
    assert!(
        matches!(s.ended, Err(LiveError::Output(_))),
        "{:?}",
        s.ended
    );
    abandoned(&s);
}

/// The image stage at level 14 with `arm` called first falls back: laid
/// out per view with its images, ready, and committed.
fn falls_back(arm: impl FnOnce()) {
    let (_d, rs) = staged_result(LEVEL_RESULT);
    let first = Arc::clone(rs.image().unwrap());
    let l = at_level(14);
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        rig.result_skin = rs;
        arm();
        let mut h = begin(rig, &l);
        assert_eq!(polled(rig, &mut h).unwrap(), Preparation::Ready);
        assert!(matches!(
            rig.ready_attempt(1, &h),
            Ok(game::Readiness::Ready)
        ));
        rig.commit_attempt().unwrap();
        assert!(
            rig.result_skin.staged_line().starts_with("per view"),
            "{}",
            rig.result_skin.staged_line()
        );
        assert!(
            rig.result_skin
                .image()
                .is_some_and(|i| Arc::ptr_eq(i, &first)),
            "its images handed back"
        );
    });
    assert!(
        screens.log().iter().any(|l| l.contains(" commit ")),
        "{:?}",
        screens.log()
    );
}

/// Image × panic in the job: the fallback, committed.
#[test]
fn the_image_stage_falls_back_on_a_panic() {
    let _serial = imports::serial();
    falls_back(imports::panic_next_job);
}

/// Image × the job's thread refused its start: the fallback, committed.
#[test]
fn the_image_stage_falls_back_when_its_thread_cannot_start() {
    let _serial = imports::serial();
    falls_back(imports::refuse_spawn);
}

// Screen.

/// Screen × quit: cancelled, its request discarded, nothing committed.
#[test]
fn the_screen_stage_on_a_quit_cancels_and_discards_its_request() {
    let l = at_level(14);
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        screens.hold_preparations(true);
        let mut h = begin(rig, &l);
        assert_eq!(stage(rig), "screen");
        let until = Instant::now() + Duration::from_millis(10);
        assert!(matches!(
            rig.poll_preparation(&mut h, until),
            Ok(game::PrepPoll::Pending)
        ));
        rig.controls.command(game::Command::Quit);
        assert_eq!(polled(rig, &mut h).unwrap(), Preparation::Cancelled);
        screens.hold_preparations(false);
        assert!(rig.commit_attempt().is_err(), "nothing to commit");
    });
}

/// Screen × close: the run ends as the window closes, the request
/// discarded.
#[test]
fn the_screen_stage_ends_the_run_on_a_close() {
    let _serial = imports::serial();
    let s = signalled_in(Stage::Screen, Signal::Close);
    assert!(matches!(s.ended, Ok(Leave::Quit)), "{:?}", s.ended);
    discarded(&s);
}

/// Screen × input failure: the run ends with the input thread's failure.
#[test]
fn the_screen_stage_ends_the_run_on_an_input_failure() {
    let _serial = imports::serial();
    let s = signalled_in(Stage::Screen, Signal::Input);
    assert!(matches!(s.ended, Err(LiveError::Input(_))), "{:?}", s.ended);
    discarded(&s);
}

/// Screen × audio failure: the run ends with the output's failure.
#[test]
fn the_screen_stage_ends_the_run_on_an_audio_failure() {
    let _serial = imports::serial();
    let s = signalled_in(Stage::Screen, Signal::Audio);
    assert!(
        matches!(s.ended, Err(LiveError::Output(_))),
        "{:?}",
        s.ended
    );
    discarded(&s);
}

/// Screen × panic in the render thread's work: the thread is gone, which
/// is fatal: `ScreenError::Ended`, and no screen is built again.
#[test]
fn the_screen_stage_fails_when_the_render_thread_panics() {
    let l = at_level(14);
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        screens.panic_prepare.store(true, Ordering::Release);
        let mut h = begin(rig, &l);
        let polled = polled(rig, &mut h);
        assert!(matches!(polled, Err(ScreenError::Ended)), "{polled:?}");
    });
    assert_eq!(screens.builds.load(Ordering::SeqCst), 1, "never rebuilt");
}

// Screen × the thread's start refused: n/a, the render thread is the
// rig's own, started with it; a request starts no thread.
