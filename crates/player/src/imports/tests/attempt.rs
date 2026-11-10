//! A pending play skin's attempts ([`PendingSkin::attempt`]) on self-made
//! Lua skins, prepared on a render thread of [`Installers`]: reuse while
//! the facts read hold, one job for other facts, its thread ended before
//! the render thread is asked, the refusal, and the two signals.

use std::sync::Arc;

use render::SkinBundle;
use skin_beatoraja::ImportFacts;

use super::{choice, class_skin, pending, play, serial, target};
use crate::imports::facts::attempt_facts;
use crate::imports::facts::tests::loaded;
use crate::imports::{ForAttempt, PendingSkin, Specialized};
use crate::render_side::tests::{installed_is, pending_link, specialized};

/// The facts of an attempt at `text` under the tapestry preset.
fn facts(text: &str) -> ImportFacts {
    attempt_facts(&loaded(text, "tapestry"), false)
}

/// The jobs this test started and joined, and the VMs open (dev-tools;
/// zeros without).
fn counters() -> (u64, u64, usize) {
    #[cfg(feature = "dev-tools")]
    return (
        super::jobs_started(),
        super::jobs_joined(),
        skin_beatoraja::vm_hooks::live(),
    );
    #[cfg(not(feature = "dev-tools"))]
    (0, 0, 0)
}

/// An attempt at `text` with no signal: its skin, how it ran, whether
/// the wait was shown, and the counters' advance over it.
fn attempt(skin: &mut PendingSkin, text: &str) -> (ForAttempt, Specialized, bool, (u64, u64)) {
    let (s, j, live) = counters();
    let mut waited = false;
    let (got, how) = skin.attempt(facts(text), || waited = true, || false, || false);
    let (s2, j2, live2) = counters();
    // The job's thread, and its VMs, ended before the attempt's skin is
    // given to the render thread.
    assert_eq!(live2, live, "{text:?}");
    (got, how, waited, (s2 - s, j2 - j))
}

#[track_caller]
fn ready(got: ForAttempt) -> Arc<SkinBundle> {
    match got {
        ForAttempt::Ready(b) => b,
        other => panic!("{other:?}"),
    }
}

/// An attempt's specialization runs as one job whose thread has ended
/// before its bundle reaches the render thread, where it is staged with
/// the attempt and installed by the commit. While the facts it read hold
/// (no rank is the normal class's; a play level the skin never read),
/// the one kept is reused: no job and no wait. Another class runs one
/// job more. A skin that cannot be built for the chart refuses with the
/// reason, its kept specialization dropped first.
#[test]
fn an_attempts_skin_is_reused_while_its_facts_hold() {
    let _serial = serial();
    let r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let dir = class_skin();
    let (p, _) = pending(play(&choice(dir.path(), "play.luaskin"), &t));
    let mut skin = PendingSkin::new(p);
    let (chart, screens, link) = pending_link();
    let one = u64::from(cfg!(feature = "dev-tools"));

    let (got, how, waited, ran) = attempt(&mut skin, "#RANK 2\n");
    assert!(matches!(how, Specialized::Ran) && waited);
    assert_eq!(ran, (one, one));
    let normal = ready(got);
    assert_eq!(
        specialized(&link, &chart, &normal).unwrap(),
        game::Preparation::Ready
    );
    link.commit_attempt().unwrap();
    assert!(installed_is(&screens, &normal));

    for text in ["", "#PLAYLEVEL 9\n"] {
        let (got, how, waited, ran) = attempt(&mut skin, text);
        assert!(matches!(how, Specialized::Reused) && !waited, "{text:?}");
        assert_eq!(ran, (0, 0), "{text:?}");
        assert!(Arc::ptr_eq(&ready(got), &normal), "{text:?}");
    }

    let (got, how, _, ran) = attempt(&mut skin, "#RANK 3\n");
    assert!(matches!(how, Specialized::Ran));
    assert_eq!(ran, (one, one));
    let easy = ready(got);
    assert!(!Arc::ptr_eq(&easy, &normal));

    link.exit();

    // A configured pass loading a module that is missing refuses.
    let dir = super::lua_skin("local l = ms.number(96)\nlocal m = require(\"cfg\")");
    let (p, _) = pending(play(&choice(dir.path(), "play.luaskin"), &t));
    let mut skin = PendingSkin::new(p);
    let (got, _, waited, ran) = attempt(&mut skin, "#PLAYLEVEL 12\n");
    assert!(waited);
    assert_eq!(ran, (one, one));
    let ForAttempt::Refused(why) = got else {
        panic!("{got:?}");
    };
    assert!(
        why.starts_with("the play skin cannot be built for this chart: ") && why.contains("cfg"),
        "{why}"
    );
    // Nothing is kept from a refusal: the same facts run a job again.
    let (_, how, _, _) = attempt(&mut skin, "#PLAYLEVEL 12\n");
    assert!(matches!(how, Specialized::Ran));
}

/// A kept specialization is reused while its facts hold, whatever became
/// of the skin's files since: a module the header pass loaded and one only
/// the configured pass loads edited, then removed, change nothing; the
/// same bundle is given, and no job or Lua pass runs. Another level, a fact
/// the skin read, builds again and reads the files as they are now.
#[test]
fn a_kept_specialization_is_reused_after_its_files_change() {
    let _serial = serial();
    let r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let dir = super::sources_skin();
    let d = dir.path();
    let (p, _) = pending(play(&choice(d, "play.luaskin"), &t));
    let mut skin = PendingSkin::new(p);
    let one = u64::from(cfg!(feature = "dev-tools"));
    let passes = || {
        #[cfg(feature = "dev-tools")]
        return skin_beatoraja::vm_hooks::passes();
        #[cfg(not(feature = "dev-tools"))]
        0
    };
    let (got, how, _, ran) = attempt(&mut skin, "#PLAYLEVEL 12\n");
    assert!(matches!(how, Specialized::Ran));
    assert_eq!(ran, (one, one));
    let kept = ready(got);

    std::fs::write(d.join("hdr.lua"), "return \"blue.png\"").unwrap();
    std::fs::write(d.join("cfg.lua"), "return \"blue.png\"").unwrap();
    for removed in [false, true] {
        if removed {
            std::fs::remove_file(d.join("hdr.lua")).unwrap();
            std::fs::remove_file(d.join("cfg.lua")).unwrap();
        }
        let before = passes();
        let (got, how, waited, ran) = attempt(&mut skin, "#PLAYLEVEL 12\n");
        assert!(
            matches!(how, Specialized::Reused) && !waited,
            "removed {removed}"
        );
        assert_eq!(ran, (0, 0), "removed {removed}");
        assert_eq!(passes(), before, "removed {removed}");
        assert!(Arc::ptr_eq(&ready(got), &kept), "removed {removed}");
    }

    // Another level builds again, and the configured-only module is gone.
    let (got, how, _, ran) = attempt(&mut skin, "#PLAYLEVEL 13\n");
    assert!(matches!(how, Specialized::Ran));
    assert_eq!(ran, (one, one));
    let ForAttempt::Refused(why) = got else {
        panic!("{got:?}");
    };
    assert!(why.contains("cannot read cfg.lua"), "{why}");
}

/// The attempt's quit during a looping specialization cancels the attempt
/// once the job's thread has ended, its VM closed; with the VM's
/// destruction held, the quit waits past the stop bound, and the run's
/// end then gives the attempt up within the bound, the job abandoned and
/// never joined.
#[cfg(feature = "dev-tools")]
#[test]
fn a_quit_waits_for_the_job_and_the_runs_end_bounds_it() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use skin_beatoraja::vm_hooks;

    use super::{LATE, STOP_WAIT, executing, lua_skin};
    let serial = serial();
    let r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let dir = lua_skin("local l = ms.number(96)\nwhile true do end");
    let (p, _) = pending(play(&choice(dir.path(), "play.luaskin"), &t));
    let mut skin = PendingSkin::new(p);
    let base = vm_hooks::live();
    for held in [false, true] {
        let (quit, end) = (AtomicBool::new(false), AtomicBool::new(false));
        let (s, j, _) = counters();
        if held {
            vm_hooks::hold();
        }
        // The attempt's thread counts its job into this test's tally.
        let tally = super::tally();
        let (got, took) = std::thread::scope(|scope| {
            let attempt = scope.spawn(|| {
                super::adopt(tally);
                let (got, _) = skin.attempt(
                    facts("#PLAYLEVEL 12\n"),
                    || {},
                    || quit.load(Ordering::SeqCst),
                    || end.load(Ordering::SeqCst),
                );
                (got, Instant::now())
            });
            executing(base);
            quit.store(true, Ordering::SeqCst);
            let ended = if held {
                assert!(super::wait_until(|| vm_hooks::waiting() == 1));
                std::thread::sleep(STOP_WAIT + Duration::from_millis(500));
                assert!(
                    !attempt.is_finished(),
                    "the quit returned before the job ended"
                );
                let ended = Instant::now();
                end.store(true, Ordering::SeqCst);
                ended
            } else {
                Instant::now()
            };
            let (got, at) = attempt.join().unwrap();
            (got, at.duration_since(ended))
        });
        assert!(matches!(got, ForAttempt::Cancelled), "held {held}: {got:?}");
        let (s2, j2, live) = counters();
        if held {
            assert!(took >= STOP_WAIT && took < STOP_WAIT + LATE, "{took:?}");
            assert_eq!((s2 - s, j2 - j), (1, 0));
            assert_eq!(live, base + 1);
            vm_hooks::release();
            assert!(super::wait_until(|| vm_hooks::live() == base));
        } else {
            assert_eq!((s2 - s, j2 - j), (1, 1));
            assert_eq!(live, base);
        }
    }
    drop(serial);
}

/// A job waited for with a deadline ([`crate::imports::Job::wait_until`],
/// what a begun preparation's polls use) returns at each deadline while
/// nothing came and the job runs; once the attempt's quit came it waits
/// past the deadline for the job's thread, which the quit cancelled.
#[test]
fn a_job_wait_returns_at_its_deadline_until_a_quit() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::LATE;
    use crate::imports::{Ran, spawn_job};
    let mut job = spawn_job(|cancel: &crate::imports::Cancel| {
        while !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        // The thread outlives the cancel a while: the quit waits for it.
        std::thread::sleep(Duration::from_millis(200));
        7
    })
    .unwrap();
    let quit = AtomicBool::new(false);
    for _ in 0..3 {
        let asked = Instant::now();
        let polled = job.wait_until(
            Some(asked + Duration::from_millis(50)),
            || quit.load(Ordering::SeqCst),
            || false,
        );
        assert!(polled.is_none());
        let took = asked.elapsed();
        assert!(took >= Duration::from_millis(50) && took < LATE, "{took:?}");
    }
    quit.store(true, Ordering::SeqCst);
    let asked = Instant::now();
    let ran = job.wait_until(Some(asked), || quit.load(Ordering::SeqCst), || false);
    assert!(matches!(ran, Some(Ran::Done(7))), "{:?}", ran.map(|_| ()));
    assert!(
        asked.elapsed() >= Duration::from_millis(200),
        "{:?}",
        asked.elapsed()
    );
}

/// A specialization whose preparation was superseded or discarded is
/// settled, never detached (the S10 item 9 design's 9.2 ownership: the
/// handle owns its subjobs): a job that would run forever is cancelled,
/// its thread joined and its VMs closed within the stop bound.
#[test]
fn a_settled_specialization_is_cancelled_and_joined() {
    let _serial = serial();
    let r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let dir = super::lua_skin("if ms.number(96) == 12 then while true do end end");
    let (p, _) = pending(play(&choice(dir.path(), "play.luaskin"), &t));
    let mut skin = PendingSkin::new(p);
    let (s, j, live) = counters();
    let crate::imports::Begun::Running(mut job) = skin.begin(
        facts(
            "#PLAYLEVEL 12
",
        ),
        || {},
    ) else {
        panic!("a job runs");
    };
    let from = std::time::Instant::now();
    assert_eq!(
        skin.settle(&mut job, || false),
        crate::imports::Settled::Ended
    );
    assert!(
        from.elapsed() < crate::imports::STOP_WAIT,
        "{:?}",
        from.elapsed()
    );
    let (s2, j2, live2) = counters();
    let one = u64::from(cfg!(feature = "dev-tools"));
    assert_eq!((s2 - s, j2 - j), (one, one), "started and joined");
    assert_eq!(live2, live, "its VMs closed");
}
