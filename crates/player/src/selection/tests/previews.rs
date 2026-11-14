//! The cursor's preview graphs through the game's threads (design.md
//! "Loading and Song Selection", Summaries): the game thread reads the
//! stored profile of the chart under the cursor, and a scan pass that
//! writes a missing profile or changes the chart reaches the published
//! update with no cursor move. The render thread is a real one over stub
//! screens that record each update they draw.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Selection, keys, on_selection, over};
use crate::play::PlayerRig;
use crate::render_side::tests::{Installers, wait_until};
use crate::select::MenuKey;

/// Writes `body` as `a.bms` under `root`.
fn write(root: &std::path::Path, body: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(root).unwrap();
    let path = root.join("a.bms");
    std::fs::write(&path, body).unwrap();
    path
}

/// One key tap at 120 BPM on a 7-key lane; [`THREE`] three.
const ONE: &str = "#TITLE a\n#BPM 120\n#00118:01\n";
const THREE: &str = "#TITLE a\n#BPM 120\n#00118:01\n#00119:0101\n";

/// The library's profile rows under `dir`.
fn profiles(dir: &std::path::Path) -> i64 {
    let db = rusqlite::Connection::open(dir.join("data").join("library.db")).unwrap();
    db.busy_timeout(Duration::from_secs(30)).unwrap();
    db.query_row("SELECT count(*) FROM profiles", [], |r| r.get(0))
        .unwrap()
}

/// A selection over `a.bms` written with `body`, its cursor on the chart.
fn on_chart(dir: &std::path::Path, body: &str) -> (Selection, std::path::PathBuf) {
    let root = dir.join("root");
    let path = write(&root, body);
    let (mut sel, _) = over(dir, vec![root]);
    for _ in 0..3 {
        if sel.screen.selected().is_some() {
            break;
        }
        keys(&mut sel, &[MenuKey::Down]);
    }
    assert_eq!(
        sel.screen.selected().map(|e| e.path.clone()),
        Some(path.clone())
    );
    (sel, path)
}

/// Looks until two scan passes have completed after now, then once more,
/// and returns the update the render thread drew last, once it drew the
/// update published last.
fn after_passes(
    rig: &mut PlayerRig<'_>,
    screens: &Installers,
) -> crate::selection_skin::SelectionUpdate {
    let target = rig.selection.as_ref().unwrap().shelf.passes() + 2;
    let begun = Instant::now();
    while rig.selection.as_ref().unwrap().shelf.passes() < target {
        assert!(begun.elapsed() < Duration::from_secs(600), "no pass");
        let _ = rig.look();
        std::thread::sleep(Duration::from_millis(2));
    }
    drawn(rig, screens)
}

/// The update published last as the render thread drew it, the page sent
/// again so a look publishes one.
fn drawn(rig: &mut PlayerRig<'_>, screens: &Installers) -> crate::selection_skin::SelectionUpdate {
    rig.selection.as_mut().unwrap().dirty = true;
    let _ = rig.look();
    let (lifetime, revision) = rig.selection.as_ref().unwrap().feed.published();
    assert!(wait_until(|| {
        screens
            .updates
            .lock()
            .unwrap()
            .last()
            .is_some_and(|u| (u.lifetime, u.revision) == (lifetime, revision))
    }));
    screens.updates.lock().unwrap().last().unwrap().clone()
}

/// The cursor's chart key: its path and SHA-256.
fn key(rig: &PlayerRig<'_>) -> (std::path::PathBuf, [u8; 32]) {
    let e = rig.selection.as_ref().unwrap().screen.selected().unwrap();
    (e.path.clone(), e.sha256)
}

/// A chart whose profile row is gone when selection opens shows no graphs
/// and caches the absence; once a pass writes the profile again (the file's
/// time changed, its bytes and so its key kept), the list's refresh reads
/// it again and the published update carries the graphs, with no key
/// pressed.
#[test]
fn a_missing_profile_is_retried_on_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, path) = on_chart(dir.path(), THREE);
    assert_eq!(profiles(dir.path()), 1);
    let db = rusqlite::Connection::open(dir.path().join("data").join("library.db")).unwrap();
    db.busy_timeout(Duration::from_secs(30)).unwrap();
    db.execute("DELETE FROM profiles", []).unwrap();
    drop(db);
    assert_eq!(profiles(dir.path()), 0);
    let screens = Installers::default();
    on_selection(sel, &screens, screens.spawn_bare(), |rig| {
        let u = drawn(rig, &screens);
        let selected = u.view.selected.as_ref().unwrap();
        assert!(selected.info.is_some());
        assert!(selected.graphs.is_none());
        assert_eq!(rig.selection.as_ref().unwrap().graphs.state(), Some(false));
        let before = key(rig);
        let cursor = rig.selection.as_ref().unwrap().screen.cursor();
        // The same bytes at another time: the next pass reads the file
        // again and writes its profile.
        let later = std::time::SystemTime::now() + Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let u = after_passes(rig, &screens);
        assert_eq!(profiles(dir.path()), 1);
        let graphs = u.view.selected.as_ref().unwrap().graphs.as_ref();
        let graphs = graphs.expect("the published update carries the graphs");
        assert_eq!(
            graphs
                .bins
                .iter()
                .flatten()
                .map(|&n| u32::from(n))
                .sum::<u32>(),
            3
        );
        let sel = rig.selection.as_ref().unwrap();
        assert_eq!(sel.graphs.state(), Some(true));
        assert_eq!(sel.screen.cursor(), cursor);
        assert_eq!(key(rig), before, "the key is unchanged");
    });
}

/// A pass that reads the chart under the cursor changed (another file at
/// the same path) moves its numbers and graphs with it, the key changed and
/// no key pressed.
#[test]
fn a_stationary_cursor_follows_a_changed_file() {
    let dir = tempfile::tempdir().unwrap();
    let (sel, path) = on_chart(dir.path(), ONE);
    let screens = Installers::default();
    on_selection(sel, &screens, screens.spawn_bare(), |rig| {
        let u = drawn(rig, &screens);
        let selected = u.view.selected.as_ref().unwrap();
        assert_eq!(selected.info.map(|i| i.key_taps), Some(1));
        let first = selected.graphs.clone().expect("graphs");
        assert_eq!(
            first
                .bins
                .iter()
                .flatten()
                .map(|&n| u32::from(n))
                .sum::<u32>(),
            1
        );
        let before = key(rig);
        let cursor = rig.selection.as_ref().unwrap().screen.cursor();
        write(path.parent().unwrap(), THREE);
        let u = after_passes(rig, &screens);
        let selected = u.view.selected.as_ref().unwrap();
        assert_eq!(selected.info.map(|i| i.key_taps), Some(3));
        let graphs = selected.graphs.as_ref().expect("graphs");
        assert_eq!(
            graphs
                .bins
                .iter()
                .flatten()
                .map(|&n| u32::from(n))
                .sum::<u32>(),
            3
        );
        let sel = rig.selection.as_ref().unwrap();
        assert_eq!(sel.screen.cursor(), cursor);
        let after = key(rig);
        assert_eq!(after.0, before.0);
        assert_ne!(after.1, before.1, "the key changed");
    });
}

/// Chart C as scanned: 150.5 BPM with ten taps in measure 0, then 200
/// (channel 03) from measure 1 with three: the published graphs carry the
/// exact MAIN 150.5 and the rows 150.5 and 200 through the library's
/// store, while 92 shows the whole 150.
#[test]
fn the_scanned_graphs_keep_the_exact_main() {
    let dir = tempfile::tempdir().unwrap();
    let c = "#TITLE a\n#BPM 150.5\n#00018:01010101010101010101\n#00103:C8\n#00119:010101\n";
    let (sel, _) = on_chart(dir.path(), c);
    let screens = Installers::default();
    on_selection(sel, &screens, screens.spawn_bare(), |rig| {
        let u = drawn(rig, &screens);
        let selected = u.view.selected.as_ref().unwrap();
        assert_eq!(selected.info.and_then(|i| i.main_bpm), Some(150));
        let graphs = selected.graphs.as_ref().expect("graphs");
        let bpm = |units: u128| bms_model::Tempo::from_units(units).unwrap();
        let exact =
            bpm(150 * bms_model::Tempo::UNITS_PER_BPM + bms_model::Tempo::UNITS_PER_BPM / 2);
        assert_eq!(graphs.main, Some(Some(exact)));
        let tempos: Vec<_> = graphs
            .rows
            .as_ref()
            .unwrap()
            .iter()
            .map(|r| r.tempo)
            .collect();
        let fast = bpm(200 * bms_model::Tempo::UNITS_PER_BPM);
        assert_eq!(tempos, [Some(exact), Some(fast), Some(fast)]);
    });
}

/// Selection left by a choice, through the production transition to the
/// decision screen, which takes the one departure every exit takes: the
/// game thread's graph cache, the selection slot and the render side's held
/// update all let the chart's graphs go, so nothing keeps them past the
/// departure. The stub screens' record of drawn updates, the test's own
/// owner, is emptied first.
#[test]
fn leaving_selection_retires_the_graphs() {
    use game::AppRig as _;
    let dir = tempfile::tempdir().unwrap();
    let (sel, path) = on_chart(dir.path(), THREE);
    let screens = Installers::default();
    on_selection(sel, &screens, screens.spawn_bare(), |rig| {
        let u = drawn(rig, &screens);
        let graphs = u.view.selected.as_ref().unwrap().graphs.as_ref();
        let a = Arc::downgrade(graphs.expect("graphs"));
        drop(u);
        screens.updates.lock().unwrap().clear();
        assert_eq!(rig.selection.as_ref().unwrap().graphs.state(), Some(true));
        assert!(a.upgrade().is_some());
        let _ = rig.entered_decision(&game::Pick::Play(path), game::Decision::off());
        assert!(
            wait_until(|| a.upgrade().is_none()),
            "the graphs retired on leaving: {} owners left",
            a.strong_count()
        );
    });
}
