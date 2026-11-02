//! The loading bridge over an imported skin: a render thread on
//! [`Recorder`]s with the self-made JSON skin of
//! [`crate::render_json`] selected, its chart labels in its bitmap font,
//! one colour per letter, read at each loading frame.

use super::*;
use crate::render_json::colour;

const BLACK: [u8; 3] = [0; 3];

/// A render thread on [`Recorder`]s with the imported skin selected, and
/// its folder.
fn imported_recording() -> (super::super::RenderLink, Recording, tempfile::TempDir) {
    let (link, rec) = Recording::new();
    let (dir, skin) = crate::render_json::bundle(&crate::render_common::offscreen(1_920, 1_080));
    let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
    link.load(chart, None).unwrap();
    assert_eq!(link.select(Some(skin)).wait().unwrap(), Preparation::Ready);
    (link, rec, dir)
}

/// The frame's title, artist, genre and full title squares.
fn glyphs(s: &Shot) -> [[u8; 3]; 4] {
    s.glyphs.unwrap()
}

/// Through the render thread and a real menu screen: chart A, by
/// artist D, drawn in bitmap glyphs, its lifetime ended; then chart B,
/// whose library entry has no artist, chosen and its load stalled at 4 of
/// 9 sounds. Every frame of B's lifetime draws its background and its
/// animation where the loading clock puts it, moving on while stalled;
/// its title and full title are empty until B's text installs and B's
/// from then on, never A's; its artist and genre stay empty, never A's
/// artist. Without the staged bitmap lines B's title never draws.
#[test]
fn a_stalled_load_draws_only_its_own_bitmap_text() {
    let (link, rec, _dir) = imported_recording();
    link.begin_loading(&chosen_by("A", Some("D")), None);
    link.loading()
        .report(LoadInput::Stage(Stage::Chart), Some(one_line("A")));
    let a = [colour('A'), colour('D'), BLACK, colour('A')];
    let a_end = rec.until(0, |s| s.glyphs == Some(a));
    link.end_loading().unwrap();

    link.begin_loading(&chosen("B"), None);
    let sink = link.loading();
    sink.report(LoadInput::Stage(Stage::Chart), Some(one_line("B")));
    let four = Stage::Sounds { done: 4, files: 9 };
    sink.report(LoadInput::Stage(four), Some(one_line("B 4/9")));
    let b = colour('B');
    let shown = rec.until(a_end, |s| s.glyphs.is_some_and(|g| g[0] == b));
    let stalled = rec.more(shown, 10);
    assert!(wait_until(|| rec
        .shots()
        .last()
        .is_some_and(|s| s.now.0 - s.load.since.0 > 200_000)));
    let end = rec.more(stalled, 3);
    link.end_loading().unwrap();
    link.exit();

    let shots = rec.shots();
    let lifetime_b = shots[end - 1].lifetime;
    assert_ne!(shots[0].lifetime, lifetime_b);
    let frames: Vec<&Shot> = shots[..end]
        .iter()
        .filter(|s| s.lifetime == lifetime_b)
        .collect();
    for s in &frames {
        let [title, artist, genre, full] = glyphs(s);
        assert!(title == BLACK || title == b, "{s:?}");
        assert_eq!(full, title, "{s:?}");
        assert_eq!((artist, genre), (BLACK, BLACK), "{s:?}");
        let red = s.red.unwrap();
        assert!(red.background && red.animation_now, "{s:?}");
    }
    let titled: Vec<&&Shot> = frames.iter().skip_while(|s| glyphs(s)[0] != b).collect();
    assert!(titled.len() >= 10, "{frames:?}");
    assert!(titled.iter().all(|s| glyphs(s)[0] == b), "{frames:?}");
    assert!(!frames.last().unwrap().red.unwrap().animation_start);
}

/// Within one lifetime: a changed artist clears its slot on the frame it
/// arrives, the title kept, and draws its replacement from a later one; a
/// changed title clears its composite, the full title, with it, the
/// artist kept; another imported bundle committed within the lifetime
/// draws only the lifetime's current text. The quit key cancelling the
/// load and another chart chosen at once: its lifetime draws none of the
/// cancelled chart's labels.
#[test]
fn changed_fields_and_a_cancelled_load_draw_only_current_bitmap_text() {
    let (link, rec, _dir) = imported_recording();
    let (_other_dir, other) =
        crate::render_json::bundle(&crate::render_common::offscreen(1_920, 1_080));
    let [a, b, c, d, e] = ['A', 'B', 'C', 'D', 'E'].map(colour);
    link.begin_loading(&chosen_by("A", Some("D")), None);
    let sink = link.loading();
    sink.report(LoadInput::Stage(Stage::Chart), Some(one_line("A")));
    let shown = rec.until(0, |s| s.glyphs == Some([a, d, BLACK, a]));
    let labelled = |title: &str, artist: &str| ChosenChart {
        title: Some(title.to_owned()),
        artist: Some(artist.to_owned()),
        ..ChosenChart::default()
    };
    sink.report(LoadInput::Chart(labelled("A", "E")), None);
    let changed = rec.until(shown, |s| s.text == 2 && glyphs(s)[1] == e);
    sink.report(LoadInput::Chart(labelled("B", "E")), None);
    let retitled = rec.until(changed, |s| s.text == 3 && glyphs(s)[0] == b);
    // Both bundles by address, as frames record them, taken before the
    // selection so no frame drawn meanwhile can stand in for either.
    let bundle = rec.shots()[retitled - 1].bundle;
    let replacement = Some(Arc::as_ptr(&other) as *const () as usize);
    assert_ne!(bundle, replacement);
    assert_eq!(link.select(Some(other)).wait().unwrap(), Preparation::Ready);
    let moved = rec.until(retitled, |s| {
        s.bundle == replacement && s.glyphs == Some([b, e, BLACK, b])
    });
    // Cancelled; chart C, with no artist, chosen at once.
    link.clear_loading();
    link.begin_loading(&chosen("C"), None);
    link.loading()
        .report(LoadInput::Stage(Stage::Chart), Some(one_line("C")));
    let next = rec.until(moved, |s| s.glyphs.is_some_and(|g| g[0] == c));
    let end = rec.more(next, 3);
    link.end_loading().unwrap();
    link.exit();

    let shots = &rec.shots()[..end];
    let lifetime = shots[0].lifetime;
    // Each revision on the first bundle; the other starts its own slots.
    let revision = |n: u64| -> Vec<&Shot> {
        shots
            .iter()
            .filter(|s| s.lifetime == lifetime && s.text == n && s.bundle == bundle)
            .collect()
    };
    let r2 = revision(2);
    assert_eq!(glyphs(r2[0]), [a, BLACK, BLACK, a], "{r2:?}");
    assert!(r2.iter().all(|s| matches!(glyphs(s), [t, x, BLACK, f]
        if t == a && f == a && (x == BLACK || x == e))));
    let r3 = revision(3);
    assert_eq!(glyphs(r3[0]), [BLACK, e, BLACK, BLACK], "{r3:?}");
    assert!(r3.iter().all(|s| matches!(glyphs(s), [t, x, BLACK, f]
        if x == e && f == t && (t == BLACK || t == b))));
    let first_other = shots.iter().position(|s| s.bundle == replacement).unwrap();
    assert!(
        shots[first_other..]
            .iter()
            .filter(|s| s.lifetime == lifetime)
            .all(|s| matches!(glyphs(s), [t, x, BLACK, f]
                if f == t && (t == BLACK || t == b) && (x == BLACK || x == e))),
        "{shots:?}"
    );
    let cancelled: Vec<&Shot> = shots.iter().filter(|s| s.lifetime != lifetime).collect();
    assert!(cancelled.len() >= 3, "{shots:?}");
    assert!(
        cancelled
            .iter()
            .all(|s| matches!(glyphs(s), [t, BLACK, BLACK, f]
        if f == t && (t == BLACK || t == c)))
    );
}
