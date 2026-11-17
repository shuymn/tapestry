//! The chart's BGA pictures before an attempt's readiness (design.md
//! "Rendering and Skins", BGA): the preparation answers ready only once
//! their table is published; a transient the reservation refuses, or an
//! allocation refused, leaves them absent and the attempt still ready.
//! With no coordinator on the rig, the job runs on the game thread, as for
//! the direct launch.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use game::Preparation;
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
/// waiting for its empty table, which the render thread holds here past
/// the link's bound; a later step reads nothing either. The session's answer is read, never the rig's own
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
            // A later step, as the play's take: with no coordinator the
            // job would run now, so what the attempt asked for is read
            // here if anything is.
            rig.bga.step(None, &|| false);
            assert_eq!(rig.bga.counts(), (0, 0), "{what}: nothing read");
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
        let seen = screens.bga.lock().unwrap();
        let table = seen.current().expect("published before ready");
        assert!(matches!(table.entries(), [BgaEntry::Picture(c)] if c.native == [640, 480]));
    });
}
