//! Startup's skins on a render thread of [`Installers`]: the kinds in
//! order, one candidate at a time, an unanswered installation, healthy
//! refusals, a play skin refused on each launch path, the other kinds'
//! notices, and the window's close at each point.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use game::{SkinChoice, SkinChoices};
use skin_beatoraja::{Cancel, Usage};
use skin_model::ScreenKind;

use super::{choice, critical_skin, options_skin, serial, wait_until};
use crate::imports::{
    Build, ImportTarget, KindOutcome, Launch, STOP_WAIT, Started, StartupEnd, Summary, build,
    startup,
};
use crate::render_side::tests::{Installers, bundle};

/// What a build of each kind did, in order, beside the jobs started.
#[derive(Clone, Default)]
struct Builds {
    started: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<String>>>,
}

impl Builds {
    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn note(&self, event: String) {
        self.events.lock().unwrap().push(event);
    }

    /// `make` as startup's build, each call counted and noted.
    fn of(
        &self,
        make: impl Fn(ScreenKind, &SkinChoice, bool, &ImportTarget, &Cancel) -> KindOutcome
        + Send
        + Sync
        + 'static,
    ) -> Build {
        let me = self.clone();
        Arc::new(move |kind, choice, autoplay, target, cancel| {
            me.started.fetch_add(1, Ordering::SeqCst);
            me.note(format!("build {kind:?}"));
            make(kind, choice, autoplay, target, cancel)
        })
    }
}

/// A built bundle, as any installable skin's.
fn bundled(_: ScreenKind, _: &SkinChoice, _: bool, _: &ImportTarget, _: &Cancel) -> KindOutcome {
    KindOutcome::Bundle(
        bundle(),
        Summary {
            sources: vec!["s.json".to_owned()],
            files: Vec::new(),
            usage: Usage::default(),
            notes: Vec::new(),
            suppressed: 0,
            chart_layer: None,
        },
        crate::screens::ScriptSlot::default(),
    )
}

/// Choices of the kinds named, each the same folder's `s.json`.
fn skins(kinds: &[ScreenKind]) -> (tempfile::TempDir, SkinChoices) {
    let dir = options_skin();
    let c = || Some(choice(dir.path(), "s.json"));
    let mut s = SkinChoices::default();
    for kind in kinds {
        match kind {
            ScreenKind::Play => s.play = c(),
            ScreenKind::Selection => s.selection = c(),
            ScreenKind::Decision => s.decision = c(),
            ScreenKind::Result => s.result = c(),
        }
    }
    (dir, s)
}

/// The installer log's lines naming `what`.
fn logged(screens: &Installers, what: &str) -> Vec<String> {
    screens
        .log()
        .into_iter()
        .filter(|l| l.contains(what))
        .collect()
}

/// A stop the test turns on.
fn stop_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[track_caller]
fn failed(r: Result<Started, StartupEnd>) -> String {
    match r {
        Err(StartupEnd::Failed(e)) => format!("{e:#}"),
        other => panic!("{other:?}"),
    }
}

/// While play's installation is held, no later kind's build starts: the
/// count stays 1 until the release, and the result kind's build follows
/// the play skin's answer.
#[test]
fn a_kind_is_built_only_after_the_previous_selection_settles() {
    let _serial = serial();
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let (_dir, choices) = skins(&[ScreenKind::Play, ScreenKind::Result]);
    let builds = Builds::default();
    let build = builds.of(bundled);
    screens.hold_install.store(true, Ordering::SeqCst);
    let midway = std::thread::scope(|s| {
        let watcher = s.spawn(|| {
            assert!(wait_until(|| builds.started() == 1));
            // The play job has ended and its installation is held.
            std::thread::sleep(Duration::from_millis(300));
            let midway = builds.started();
            builds.note("released".to_owned());
            screens.hold_install.store(false, Ordering::SeqCst);
            midway
        });
        let started = startup(&link, &choices, Launch::Selection, false, &build, || false);
        assert_eq!(started.unwrap(), Started::default());
        watcher.join().unwrap()
    });
    assert_eq!(midway, 1);
    assert_eq!(builds.events(), ["build Play", "released", "build Result"]);
    // A fixed result skin is installed as the result kind's after play's
    // (the S10-09i design's D2).
    assert_eq!(
        logged(&screens, "install"),
        ["1 install 1", "1 install Result 2"],
        "{:?}",
        screens.log()
    );
    link.exit();
}

/// An installation past the answer's bound: the poll gives no answer, the
/// run ends naming play while the installer is still held, no later
/// build starts, and once released nothing is committed.
#[test]
fn an_unanswered_installation_ends_startup() {
    let _serial = serial();
    let screens = Installers::default();
    let mut link = screens.spawn_bare();
    link.set_timeouts(Duration::from_millis(300), Duration::from_secs(10));
    let (_dir, choices) = skins(&[ScreenKind::Play, ScreenKind::Result]);
    let builds = Builds::default();
    let build = builds.of(bundled);
    screens.hold_install.store(true, Ordering::SeqCst);
    let why = failed(startup(
        &link,
        &choices,
        Launch::Selection,
        false,
        &build,
        || false,
    ));
    // Still held: the render thread is inside the installation.
    assert!(
        why.starts_with("the play skin cannot be installed"),
        "{why}"
    );
    assert_eq!(builds.started(), 1);
    screens.hold_install.store(false, Ordering::SeqCst);
    assert!(wait_until(|| !logged(&screens, "install").is_empty()));
    std::thread::sleep(Duration::from_millis(100));
    assert!(logged(&screens, "commit").is_empty(), "{:?}", screens.log());
    assert_eq!(builds.started(), 1);
    link.exit();
}

/// On the song selection path, a play skin the render thread refuses on
/// a healthy device leaves play's notice, and the next kind's build
/// starts after the refusal.
#[test]
fn a_healthy_refusal_moves_on_to_the_next_kind() {
    let _serial = serial();
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let (_dir, choices) = skins(&[ScreenKind::Play, ScreenKind::Result]);
    let builds = Builds::default();
    let build = builds.of(bundled);
    screens.refuse_install.store(true, Ordering::SeqCst);
    let started = startup(&link, &choices, Launch::Selection, false, &build, || false).unwrap();
    assert_eq!(
        started.play.as_deref(),
        Some("スキンを使えません: the skin cannot be installed")
    );
    assert_eq!(builds.events(), ["build Play", "build Result"]);
    // Each is refused in its turn: the result's refusal is a notice.
    assert_eq!(
        logged(&screens, "refuse"),
        ["1 refuse 1", "1 refuse Result 2"],
        "{:?}",
        screens.log()
    );
    assert!(started.result.is_some());
    link.exit();
}

/// A play skin whose entry is missing admits no chart: play's notice
/// names the reason, and nothing is selected for play, a built-in skin
/// included.
#[test]
fn a_missing_play_skin_admits_no_chart() {
    let _serial = serial();
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let dir = options_skin();
    let choices = SkinChoices {
        play: Some(choice(dir.path(), "missing.json")),
        ..SkinChoices::default()
    };
    let build: Build = Arc::new(build);
    let started = startup(&link, &choices, Launch::Selection, false, &build, || false).unwrap();
    let notice = started.play.clone().unwrap();
    assert!(
        notice.starts_with("スキンを使えません: cannot read missing.json"),
        "{notice}"
    );
    let mut state = crate::render_side::SkinState::Refused(notice.clone());
    assert_eq!(state.ready(), Err(notice));
    assert_eq!(started.notice, None);
    assert!(screens.log().is_empty(), "{:?}", screens.log());
    link.exit();
}

/// A result skin with neither an EX score nor a status, and a play skin
/// there of another kind: each is refused and keeps the built-in result
/// screen, with one notice line, nothing is selected for it, and charts
/// stay admitted.
#[test]
fn a_non_play_kind_keeps_its_built_in_screen_with_a_notice() {
    let _serial = serial();
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let dir = options_skin();
    std::fs::write(
        dir.path().join("r.json"),
        r#"{"type": 7, "w": 1920, "h": 1080}"#,
    )
    .unwrap();
    let build: Build = Arc::new(build);
    for (entry, why) in [
        ("r.json", "has no EX score"),
        (
            "s.json",
            "a 7-key play skin in the single-chart result slot",
        ),
    ] {
        let choices = SkinChoices {
            result: Some(choice(dir.path(), entry)),
            ..SkinChoices::default()
        };
        let started = startup(&link, &choices, Launch::Selection, false, &build, || false).unwrap();
        assert!(started.play.is_none() && started.pending.is_none());
        assert!(started.selection.is_none() && started.result.is_none());
        let notice = started.notice.unwrap();
        assert!(
            notice.starts_with("リザルトのスキンは使えません（内蔵画面）: ")
                && notice.contains(why),
            "{notice}"
        );
    }
    assert!(screens.log().is_empty(), "{:?}", screens.log());
    link.exit();
}

/// On the direct path a play skin refused by any source fails the run at
/// once, naming the reason, and the result kind is never built: the
/// importer's refusal, a panic, a thread ending without its outcome, and
/// the render thread's refusal. An installable one is installed and the
/// result kind built after it.
#[test]
fn a_refused_play_skin_fails_the_direct_path_at_once() {
    struct Bomb;
    impl Drop for Bomb {
        fn drop(&mut self) {
            panic!("the payload's drop panics");
        }
    }
    let _serial = serial();
    let (dir, choices) = skins(&[ScreenKind::Play, ScreenKind::Result]);
    critical_skin(dir.path(), "bad.json", "C:n.png");
    let refused_choices = SkinChoices {
        play: Some(choice(dir.path(), "bad.json")),
        result: choices.result.clone(),
        ..SkinChoices::default()
    };
    type Make = fn(ScreenKind, &SkinChoice, bool, &ImportTarget, &Cancel) -> KindOutcome;
    let importer: Make = build;
    let panics: Make = |_, _, _, _, _| panic!("the build panics");
    let ends: Make = |_, _, _, _, _| std::panic::panic_any(Bomb);
    for (make, choices, refuse, says) in [
        (
            importer,
            &refused_choices,
            false,
            "the EX score number cannot be imported",
        ),
        (panics, &choices, false, "import failed"),
        (ends, &choices, false, "import thread ended"),
        (
            bundled as Make,
            &choices,
            true,
            "the skin cannot be installed",
        ),
    ] {
        let screens = Installers::default();
        let link = screens.spawn_bare();
        screens.refuse_install.store(refuse, Ordering::SeqCst);
        let builds = Builds::default();
        let build = builds.of(make);
        let why = failed(startup(
            &link,
            choices,
            Launch::Direct,
            false,
            &build,
            || false,
        ));
        assert!(why.starts_with("the play skin cannot be used: "), "{why}");
        assert!(why.contains(says), "{why}");
        assert_eq!(builds.events(), ["build Play"]);
        assert!(
            logged(&screens, "install").is_empty(),
            "{:?}",
            screens.log()
        );
        link.exit();
    }
    // The positive control.
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let builds = Builds::default();
    let build = builds.of(bundled);
    let started = startup(&link, &choices, Launch::Direct, false, &build, || false).unwrap();
    assert_eq!(started, Started::default());
    assert_eq!(builds.events(), ["build Play", "build Result"]);
    // Play's and the fixed result skin's are committed.
    assert_eq!(
        logged(&screens, "commit"),
        ["1 commit 1", "1 commit 2"],
        "{:?}",
        screens.log()
    );
    link.exit();
}

/// A Lua play skin whose base import reads a chart fact ends pending:
/// startup marks it pending on the render thread and admits charts, on
/// the song selection path and on the direct path, which goes on to the
/// result kind's build. Nothing is installed for play, the marker
/// refuses an attempt without a specialization, and the line says what
/// the skin reads.
#[test]
fn a_pending_play_skin_is_marked_pending_at_startup() {
    let _serial = serial();
    for (body, fact) in [
        (
            "if ms.option(180) then x = 500 end",
            skin_beatoraja::FactId::JudgeClass,
        ),
        ("x = ms.number(96)", skin_beatoraja::FactId::PlayLevel),
    ] {
        let dir = super::lua_skin(body);
        let (_result_dir, result) = skins(&[ScreenKind::Result]);
        let choices = SkinChoices {
            play: Some(choice(dir.path(), "play.luaskin")),
            ..result
        };
        let builds = Builds::default();
        let build = builds.of(move |kind, c, a, t, cancel| match kind {
            ScreenKind::Play => build(kind, c, a, t, cancel),
            _ => bundled(kind, c, a, t, cancel),
        });
        for launch in [Launch::Selection, Launch::Direct] {
            let screens = Installers::default();
            let link = screens.spawn_bare();
            let started = startup(&link, &choices, launch, false, &build, || false).unwrap();
            assert_eq!(started.play, None, "{launch:?}");
            assert_eq!(started.notice, None, "{launch:?}");
            assert!(started.pending.is_some(), "{launch:?}");
            // Play is pending, so an attempt with the skin in use is
            // refused unasked; only the fixed result skin is installed.
            assert_eq!(
                screens.log(),
                ["1 install Result 2", "1 commit 2"],
                "{launch:?}"
            );
            assert!(started.result.is_some(), "{launch:?}");
            let why = crate::render_side::tests::in_use_attempt(&link);
            assert_eq!(why, "the play skin awaits its specialization", "{launch:?}");
            assert!(logged(&screens, "prepare").is_empty(), "{launch:?}");
            link.exit();
        }
        assert_eq!(
            builds.events(),
            ["build Play", "build Result", "build Play", "build Result"]
        );
        let line = crate::imports::line(ScreenKind::Play, &crate::imports::Shown::Pending(fact));
        assert!(
            line.starts_with("skin: play: pending: reads the chart's ")
                && line.ends_with(" as it is built; built for each chart before its play"),
            "{line}"
        );
    }
}

/// A close during startup ends it within the stop bound, with no further
/// build or selection: while play's job runs Lua, while the result kind's
/// job runs after play's skin was committed, and while play's selection
/// is pending.
#[test]
fn a_close_during_startup_ends_it() {
    let _serial = serial();
    let (_dir, choices) = skins(&[ScreenKind::Play, ScreenKind::Result]);
    let bound = STOP_WAIT + Duration::from_secs(2);

    // The result kind's job, after play's skin was committed.
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let builds = Builds::default();
    let running = Arc::new(AtomicBool::new(false));
    let theirs = Arc::clone(&running);
    let build = builds.of(move |kind, c, a, t, cancel| {
        if kind == ScreenKind::Result {
            theirs.store(true, Ordering::SeqCst);
            let end = Instant::now() + Duration::from_secs(30);
            while !cancel.is_cancelled() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(1));
            }
            return KindOutcome::Refused(crate::imports::STOPPED.to_owned());
        }
        bundled(kind, c, a, t, cancel)
    });
    let stop = stop_flag();
    let r = std::thread::scope(|s| {
        s.spawn(|| {
            assert!(wait_until(|| running.load(Ordering::SeqCst)));
            stop.store(true, Ordering::SeqCst);
        });
        let from = Instant::now();
        let r = startup(&link, &choices, Launch::Selection, false, &build, || {
            stop.load(Ordering::SeqCst)
        });
        (r, from.elapsed())
    });
    assert!(matches!(r.0, Err(StartupEnd::Closed)), "{:?}", r.0);
    assert!(r.1 < bound + Duration::from_secs(1), "{:?}", r.1);
    assert_eq!(builds.events(), ["build Play", "build Result"]);
    assert_eq!(logged(&screens, "commit").len(), 1, "{:?}", screens.log());
    assert_eq!(logged(&screens, "Result").len(), 0, "{:?}", screens.log());
    link.exit();

    // Play's selection pending.
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let builds = Builds::default();
    let build = builds.of(bundled);
    screens.hold_install.store(true, Ordering::SeqCst);
    let stop = stop_flag();
    let r = std::thread::scope(|s| {
        s.spawn(|| {
            // Play's selection reached the render thread and waits there.
            assert!(wait_until(|| screens.install_held.load(Ordering::SeqCst)));
            stop.store(true, Ordering::SeqCst);
        });
        startup(&link, &choices, Launch::Selection, false, &build, || {
            stop.load(Ordering::SeqCst)
        })
    });
    assert!(matches!(r, Err(StartupEnd::Closed)), "{r:?}");
    screens.hold_install.store(false, Ordering::SeqCst);
    assert!(wait_until(|| !logged(&screens, "install").is_empty()));
    std::thread::sleep(Duration::from_millis(100));
    assert!(logged(&screens, "commit").is_empty(), "{:?}", screens.log());
    assert_eq!(builds.events(), ["build Play"]);
    link.exit();
}

/// A close while play's Lua job runs ends startup: the job is cancelled
/// and joined, and no further build or selection follows; with the VM's
/// destruction held past the stop bound, the job is abandoned and the run
/// ends all the same.
#[cfg(feature = "dev-tools")]
#[test]
fn a_close_during_a_lua_job_ends_startup() {
    use skin_beatoraja::vm_hooks;
    let _serial = serial();
    let dir = super::looping();
    let choices = SkinChoices {
        play: Some(choice(dir.path(), "s.luaskin")),
        result: Some(choice(dir.path(), "s.luaskin")),
        ..SkinChoices::default()
    };
    for held in [false, true] {
        let screens = Installers::default();
        let link = screens.spawn_bare();
        let builds = Builds::default();
        let build = builds.of(build);
        let base = vm_hooks::live();
        if held {
            vm_hooks::hold();
        }
        let stop = stop_flag();
        let (r, took) = std::thread::scope(|s| {
            s.spawn(|| {
                super::executing(base);
                stop.store(true, Ordering::SeqCst);
            });
            let from = Instant::now();
            let r = startup(&link, &choices, Launch::Selection, false, &build, || {
                stop.load(Ordering::SeqCst)
            });
            (r, from.elapsed())
        });
        let live = vm_hooks::live();
        vm_hooks::release();
        assert!(matches!(r, Err(StartupEnd::Closed)), "held {held}: {r:?}");
        assert!(
            took < STOP_WAIT + Duration::from_secs(4),
            "held {held}: {took:?}"
        );
        assert_eq!(live, base + usize::from(held), "held {held}");
        assert_eq!(builds.events(), ["build Play"], "held {held}");
        assert!(screens.log().is_empty(), "held {held}: {:?}", screens.log());
        assert!(wait_until(|| vm_hooks::live() == base));
        link.exit();
    }
}

/// A selection skin installed gives the select screen its song list's
/// window and no notice; one the renderer refuses leaves the built-in page
/// with its notice and no window; none configured, neither.
#[test]
fn a_selection_skin_installs_with_its_window_or_leaves_a_notice() {
    let _serial = serial();
    let dir = crate::selection_skin::fixture();
    let choices = SkinChoices {
        selection: Some(choice(dir.path(), "s.json")),
        ..SkinChoices::default()
    };
    let build: Build = Arc::new(build);
    for refuse in [false, true] {
        let screens = Installers::default();
        screens.refuse_install.store(refuse, Ordering::Release);
        let link = screens.spawn_bare();
        let started = startup(&link, &choices, Launch::Selection, false, &build, || false).unwrap();
        if refuse {
            assert_eq!(started.selection, None);
            let notice = started.notice.unwrap();
            assert!(notice.contains("the skin cannot be installed"), "{notice}");
        } else {
            assert_eq!(started.selection, Some((3, 7)));
            assert_eq!(started.notice, None);
        }
        link.exit();
    }
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let none = SkinChoices::default();
    let started = startup(&link, &none, Launch::Selection, false, &build, || false).unwrap();
    assert_eq!(started, Started::default());
    link.exit();
}

/// A Lua selection skin whose import completes installs with its song
/// list's window and no notice; one whose configured pass refuses, by an
/// error or by reading a chart fact, leaves the built-in page with a
/// notice naming its located reason.
#[test]
fn a_lua_selection_skin_installs_or_leaves_its_located_reason() {
    let _serial = serial();
    let build: Build = Arc::new(build);
    let dir = crate::selection_skin::lua_fixture();
    let choices = SkinChoices {
        selection: Some(choice(dir.path(), "s.luaskin")),
        ..SkinChoices::default()
    };
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let started = startup(&link, &choices, Launch::Selection, false, &build, || false).unwrap();
    assert_eq!(started.selection, Some((3, 7)));
    assert_eq!(started.notice, None);
    link.exit();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("s.luaskin"),
        "local skin = { type = 5, name = \"t\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         error(\"refused\")\n",
    )
    .unwrap();
    let choices = SkinChoices {
        selection: Some(choice(dir.path(), "s.luaskin")),
        ..SkinChoices::default()
    };
    let screens = Installers::default();
    let link = screens.spawn_bare();
    let started = startup(&link, &choices, Launch::Selection, false, &build, || false).unwrap();
    assert_eq!(started.selection, None);
    let notice = started.notice.unwrap();
    assert!(notice.ends_with("s.luaskin:3:0: a Lua error"), "{notice}");
    link.exit();

    // A chart fact read as the skin is built (the rank's band, the play
    // level) refuses a selection skin with its place: it is never pending,
    // and nothing is installed.
    for read in ["ms.option(180)", "ms.number(96)"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("s.luaskin"),
            format!(
                "local skin = {{ type = 5, name = \"t\", w = 1920, h = 1080 }}\n\
                 if not skin_config then return skin end\n\
                 local ms = require(\"main_state\")\n\
                 skin.fact = {read}\n\
                 return skin\n"
            ),
        )
        .unwrap();
        let choices = SkinChoices {
            selection: Some(choice(dir.path(), "s.luaskin")),
            ..SkinChoices::default()
        };
        let screens = Installers::default();
        let link = screens.spawn_bare();
        let started = startup(&link, &choices, Launch::Selection, false, &build, || false).unwrap();
        assert_eq!(started.selection, None, "{read}");
        assert!(started.pending.is_none(), "{read}");
        let notice = started.notice.unwrap();
        let why = format!(
            "s.luaskin:4:0: main_state.{} reads a fact this scene cannot give",
            &read[3..]
        );
        assert!(notice.ends_with(&why), "{read}: {notice}");
        assert!(screens.log().is_empty(), "{read}: {:?}", screens.log());
        link.exit();
    }
}

/// C8 (S10-09f): a Lua decision skin whose import completes is installed
/// with no notice; one whose configured pass reads a chart fact (the
/// judgment class, the play level), known only at the choice, ends
/// pending and is refused at startup: nothing is installed, and the
/// select screen's notice says why.
#[test]
fn a_lua_decision_skin_installs_or_is_refused_when_pending() {
    let _serial = serial();
    let build: Build = Arc::new(build);
    let entry = |read: &str| {
        format!(
            "local skin = {{ type = 6, name = \"t\", w = 1920, h = 1080 }}\n\
             if not skin_config then return skin end\n\
             local ms = require(\"main_state\")\n\
             skin.fact = {read}\n\
             return skin\n"
        )
    };
    let run = |read: &str| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("d.luaskin"), entry(read)).unwrap();
        let choices = SkinChoices {
            decision: Some(choice(dir.path(), "d.luaskin")),
            ..SkinChoices::default()
        };
        let screens = Installers::default();
        let link = screens.spawn_bare();
        let started = startup(&link, &choices, Launch::Selection, false, &build, || false).unwrap();
        link.exit();
        (started, screens.log())
    };
    let (started, log) = run("ms.option(50)");
    assert_eq!(started.notice, None);
    assert!(started.pending.is_none());
    assert!(log.iter().any(|l| l.contains("commit")), "{log:?}");
    for (read, fact) in [
        ("ms.option(180)", "the chart's judgment class"),
        ("ms.number(96)", "the chart's play level"),
    ] {
        let (started, log) = run(read);
        assert!(started.pending.is_none(), "{read}");
        let notice = started.notice.unwrap();
        assert!(
            notice.starts_with("決定のスキンは使えません（内蔵画面）: ")
                && notice.contains(&format!("the skin reads {fact} as it is built")),
            "{read}: {notice}"
        );
        assert!(log.is_empty(), "{read}: {log:?}");
    }
}
