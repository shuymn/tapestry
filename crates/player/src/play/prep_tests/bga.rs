//! The chart's BGA pictures before an attempt's readiness (design.md
//! "Rendering and Skins", BGA): the preparation answers ready only once
//! their table is published; a transient the reservation refuses, or an
//! allocation refused, leaves them absent and the attempt still ready.
//! With no coordinator on the rig, the job runs on the game thread, as for
//! the direct launch.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use game::Preparation;
use game::Rig as _;
use skin_model::bga::BgaEntry;

use super::{begin, imports, on_rig, polled, stage};
use crate::bga::{ALLOC_LIMIT, HOLD, REFUSE, STATED, TERMS};
use crate::render_side::tests::Installers;

/// Lets the job go and the reservation and allocations grant when
/// dropped, a failing test's included.
struct LetGo;

impl Drop for LetGo {
    fn drop(&mut self) {
        HOLD.store(false, Ordering::Release);
        REFUSE.store(false, Ordering::SeqCst);
        ALLOC_LIMIT.store(0, Ordering::SeqCst);
    }
}

/// A chart whose base shows a 640×480 picture from its first measure,
/// the picture written beside it.
fn chart_with_bga() -> (tempfile::TempDir, game::Loaded) {
    let (dir, l) =
        crate::imports::facts::tests::loaded_kept("#BMP01 a.png\n#00104:01\n", "tapestry");
    crate::bga::tests::png(dir.path(), "a.png", [640, 480], [200, 0, 0]);
    (dir, l)
}

/// Ready only after the table is published: the job held for 100 ms keeps
/// the preparation from answering, and once it answers the table holds
/// the picture.
#[test]
fn the_preparation_waits_for_the_bga_publication() {
    let _serial = imports::serial();
    let (_dir, l) = chart_with_bga();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        let _let_go = LetGo;
        HOLD.store(true, Ordering::Release);
        let began = Instant::now();
        let ready = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                HOLD.store(false, Ordering::Release);
            });
            let mut h = begin(rig, &l);
            polled(rig, &mut h).unwrap()
        });
        assert_eq!(ready, Preparation::Ready);
        assert!(began.elapsed() >= Duration::from_millis(100));
        assert_eq!(stage(rig), "answered");
        let seen = screens.bga.lock().unwrap();
        let table = seen.current().expect("published before ready");
        assert!(table.key().is_some());
        assert!(matches!(table.entries(), [BgaEntry::Picture(c)] if c.native == [640, 480]));
    });
}

/// The reservation refusing the job's stated transient, and separately an
/// allocation of the decode refused: the pictures are absent (no array,
/// the placeholder), and the attempt is ready.
#[test]
fn bga_transient_the_ledger_refuses_leaves_bga_absent_and_play_starts() {
    let _serial = imports::serial();
    for refusal in ["reservation", "allocation"] {
        let (_dir, l) = chart_with_bga();
        let screens = Installers::default();
        on_rig(l.chart(), &screens, false, |rig| {
            let _let_go = LetGo;
            *STATED.lock().unwrap() = None;
            match refusal {
                "reservation" => REFUSE.store(true, Ordering::SeqCst),
                _ => ALLOC_LIMIT.store(64 << 10, Ordering::SeqCst),
            }
            let mut h = begin(rig, &l);
            assert_eq!(
                polled(rig, &mut h).unwrap(),
                Preparation::Ready,
                "{refusal}"
            );
            assert_eq!(*STATED.lock().unwrap(), Some(TERMS), "{refusal}");
            let seen = screens.bga.lock().unwrap();
            let table = seen.current().expect("published before ready");
            assert_eq!(table.key(), None, "{refusal}: no array");
            assert_eq!(table.entries(), [BgaEntry::Absent], "{refusal}");
            assert_eq!(seen.cells, 0, "{refusal}");
        });
    }
}

/// A session that does not show the BGA (`[bga] mode` off, or `autoplay`
/// in a session that is not autoplay) reads nothing for a chart with BGA:
/// no job begun, nothing decoded, no array begun; and it is ready without
/// waiting for its empty table, sent at once, which the render thread holds here past
/// the link's bound; a later step reads nothing either, and the attempt is
/// prepared to draw the placeholder. The session's answer is read, never the rig's own
/// autoplay, which a scenario's chart changes: an autoplay session whose
/// rig plays by hand still reads the pictures.
#[test]
fn a_session_not_showing_the_bga_reads_nothing_and_is_ready_unwaited() {
    let _serial = imports::serial();
    let session = |mode, autoplay| game::BgaSession {
        settings: game::BgaSettings {
            mode,
            ..game::BgaSettings::default()
        },
        autoplay,
    };
    for (what, s, rig_autoplay) in [
        ("off", session(game::BgaMode::Off, false), false),
        (
            "autoplay, the session by hand",
            session(game::BgaMode::Autoplay, false),
            true,
        ),
    ] {
        let (_dir, l) = chart_with_bga();
        let screens = Installers::default();
        on_rig(l.chart(), &screens, false, |rig| {
            rig.bga_session = s;
            rig.autoplay = rig_autoplay;
            rig.render
                .set_timeouts(Duration::from_secs(1), Duration::from_secs(5));
            rig.render.hold_bga_publications(true);
            let began = Instant::now();
            let mut h = begin(rig, &l);
            let ready = polled(rig, &mut h).unwrap();
            let took = began.elapsed();
            rig.render.hold_bga_publications(false);
            assert_eq!(ready, Preparation::Ready, "{what}");
            assert!(took < Duration::from_secs(1), "{what}: waited {took:?}");
            assert_eq!(stage(rig), "answered", "{what}");
            // Its empty table was sent by READY: once released, the render
            // thread takes it without another step.
            let by = Instant::now() + Duration::from_secs(5);
            while screens.bga.lock().unwrap().published.is_empty() && Instant::now() < by {
                std::thread::park_timeout(Duration::from_millis(1));
            }
            {
                let seen = screens.bga.lock().unwrap();
                assert_eq!(seen.published.len(), 1, "{what}: the empty table sent");
                assert!(seen.published[0].entries().is_empty(), "{what}");
            }
            // A later step, as the play's take: with no coordinator the
            // job would run now, so what the attempt asked for is read
            // here if anything is.
            rig.bga.step(None, &|| false);
            assert_eq!(rig.bga.counts(), (0, 0), "{what}: nothing read");
            assert_eq!(
                *screens.bga_shown.lock().unwrap(),
                [false],
                "{what}: the attempt draws the placeholder"
            );
            let seen = screens.bga.lock().unwrap();
            assert!(seen.begins.is_empty(), "{what}: no array");
            assert_eq!(seen.cells, 0, "{what}");
        });
    }
    let (_dir, l) = chart_with_bga();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        rig.bga_session = session(game::BgaMode::Autoplay, true);
        rig.autoplay = false;
        let mut h = begin(rig, &l);
        assert_eq!(polled(rig, &mut h).unwrap(), Preparation::Ready);
        assert_eq!(rig.bga.counts(), (1, 1), "an autoplay session reads them");
        assert_eq!(*screens.bga_shown.lock().unwrap(), [true], "and draws them");
        let seen = screens.bga.lock().unwrap();
        let table = seen.current().expect("published before ready");
        assert!(matches!(table.entries(), [BgaEntry::Picture(c)] if c.native == [640, 480]));
    });
}

/// With no coordinator (the direct launch), the BGA job runs while the
/// render thread prepares the attempt, so its video manifest reaches the
/// render thread before that preparation answers (design.md "Load
/// Pipeline"): a screen that answers only once a manifest has come, as a
/// play's movie set does, is ready well inside its own deadline, with the
/// chart's video file in the manifest.
#[test]
fn direct_play_waits_for_the_manifest() {
    let _serial = imports::serial();
    let (dir, l) = crate::imports::facts::tests::loaded_kept(
        "#BMP01 a.png\n#BMP02 v.mp4\n#00104:0102\n",
        "tapestry",
    );
    crate::bga::tests::png(dir.path(), "a.png", [64, 64], [200, 0, 0]);
    std::fs::write(dir.path().join("v.mp4"), b"x").unwrap();
    let screens = Installers::default();
    screens.wait_manifest.store(true, Ordering::Release);
    on_rig(l.chart(), &screens, false, |rig| {
        rig.render
            .set_timeouts(Duration::from_secs(2), Duration::from_secs(5));
        let began = Instant::now();
        let mut h = begin(rig, &l);
        let ready = polled(rig, &mut h).unwrap();
        assert_eq!(ready, Preparation::Ready);
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(*screens.manifests.lock().unwrap(), [1], "the video's file");
        let log = screens.log();
        let manifest = log.iter().position(|l| l.ends_with(" manifest"));
        let prepared = log.iter().position(|l| l.contains(" prepare "));
        assert!(manifest.is_some() && prepared.is_some(), "{log:?}");
    });
}

/// Sets the BGA job's per-read delay, and clears it when dropped.
struct ReadDelay;

impl ReadDelay {
    fn set(ms: u64) -> Self {
        crate::bga::READ_DELAY_MS.store(ms, Ordering::SeqCst);
        Self
    }
}

impl Drop for ReadDelay {
    fn drop(&mut self) {
        crate::bga::READ_DELAY_MS.store(0, Ordering::SeqCst);
    }
}

/// With a coordinator (song selection), a chart chosen while the job of
/// the chart before is still reading (it reads on for 800 ms after its
/// choice is left) has its own job begin as soon as that one ends, while
/// its screen is prepared: the screen, which answers only once a manifest
/// has come, as a play's movie set does, answers well inside the
/// preparation's deadline, with the chosen chart's video file in the
/// manifest (design.md "Load Pipeline", movies at READY).
#[test]
fn a_menu_preparation_starts_its_bga_job_while_its_screen_waits() {
    let _serial = imports::serial();
    let (_dir_a, a) = chart_with_bga();
    let (dir_b, b) = crate::imports::facts::tests::loaded_kept(
        "#BMP01 a.png\n#BMP02 v.mp4\n#00104:0102\n",
        "tapestry",
    );
    crate::bga::tests::png(dir_b.path(), "a.png", [64, 64], [200, 0, 0]);
    std::fs::write(dir_b.path().join("v.mp4"), b"x").unwrap();
    let list = tempfile::tempdir().unwrap();
    let screens = Installers::default();
    screens.wait_manifest.store(true, Ordering::Release);
    on_rig(a.chart(), &screens, false, |rig| {
        let _let_go = LetGo;
        let (sel, _) = crate::selection::tests::tree(list.path(), &[&["b.bms"]]);
        rig.selection = Some(sel);
        rig.render
            .set_timeouts(Duration::from_secs(5), Duration::from_secs(10));
        // Chart A chosen: its job held past its array's begin, then let go
        // into a read that takes 800 ms whatever happens meanwhile.
        HOLD.store(true, Ordering::Release);
        let delay = ReadDelay::set(800);
        rig.chart_images_at_choice(&a);
        super::until("A's job held", || crate::bga::HELD.load(Ordering::Acquire));
        HOLD.store(false, Ordering::Release);
        std::thread::sleep(Duration::from_millis(100));
        drop(delay);
        // Chart B chosen and prepared while A's job still reads.
        rig.bga.renew();
        screens.manifests.lock().unwrap().clear();
        let began = Instant::now();
        let mut h = begin(rig, &b);
        let ready = polled(rig, &mut h).unwrap();
        assert_eq!(ready, Preparation::Ready);
        let took = began.elapsed();
        assert!(took < Duration::from_secs(2), "{took:?}");
        assert_eq!(*screens.manifests.lock().unwrap(), [1], "B's video file");
    });
}

/// Holds each BGA read at its gate, and lets it go when dropped.
struct ReadGate;

impl ReadGate {
    fn set() -> Self {
        crate::bga::READ_GATE.store(true, Ordering::Release);
        Self
    }
}

impl Drop for ReadGate {
    fn drop(&mut self) {
        crate::bga::READ_GATE.store(false, Ordering::Release);
    }
}

/// A quit pending as the screen's wait looks at the BGA job begins no job:
/// with the job of the chart before ended while the chosen chart's screen
/// waits, the wait stops on the quit and the chosen chart's job is not
/// begun.
#[test]
fn a_stopped_screen_wait_begins_no_bga_job() {
    let _serial = imports::serial();
    let (_dir_a, a) = chart_with_bga();
    let (dir_b, b) = crate::imports::facts::tests::loaded_kept(
        "#BMP01 a.png\n#BMP02 v.mp4\n#00104:0102\n",
        "tapestry",
    );
    crate::bga::tests::png(dir_b.path(), "a.png", [64, 64], [200, 0, 0]);
    std::fs::write(dir_b.path().join("v.mp4"), b"x").unwrap();
    let list = tempfile::tempdir().unwrap();
    let screens = Installers::default();
    screens.wait_manifest.store(true, Ordering::Release);
    on_rig(a.chart(), &screens, false, |rig| {
        let _let_go = LetGo;
        let (sel, _) = crate::selection::tests::tree(list.path(), &[&["b.bms"]]);
        rig.selection = Some(sel);
        rig.render
            .set_timeouts(Duration::from_secs(5), Duration::from_secs(10));
        // Chart A's job held inside a read, whatever happens meanwhile;
        // chart B chosen and prepared up to its screen.
        let gate = ReadGate::set();
        rig.chart_images_at_choice(&a);
        super::until("A's read held", || {
            crate::bga::READ_WAITING.load(Ordering::Acquire)
        });
        // A's manifest, sent before its reads, is not B's.
        super::until("A's manifest", || {
            !screens.manifests.lock().unwrap().is_empty()
        });
        screens.manifests.lock().unwrap().clear();
        rig.bga.renew();
        let mut h = begin(rig, &b);
        let began = Instant::now();
        // Its screen's wait looks at the job at least once while A reads.
        let mut waited = false;
        while !waited {
            assert!(
                began.elapsed() < super::watchdog(),
                "never reached its screen"
            );
            waited = stage(rig) == "screen";
            let until = Instant::now() + Duration::from_millis(100);
            let polled = rig.poll_preparation(&mut h, until);
            assert!(matches!(polled, Ok(game::PrepPoll::Pending)), "{polled:?}");
        }
        assert_eq!(stage(rig), "screen");
        assert!(!rig.bga.obsolete_ended(), "A's job still reads");
        // The quit pending, and A's job ended, its end not yet taken.
        rig.controls.command(game::Command::Quit);
        drop(gate);
        super::until("A's job ended", || rig.bga.obsolete_ended());
        let (begun, _) = rig.bga.counts();
        assert_eq!(polled(rig, &mut h).unwrap(), Preparation::Cancelled);
        assert_eq!(rig.bga.counts().0, begun, "no job begun");
    });
}
