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
