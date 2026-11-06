//! The selection importer's essential parts (C17), its ratio bars (C9,
//! C10 through the importer) and the play and other kinds beside it (C20).

mod common;

use common::selection::{
    BAR, SET, Sel, TEXT_T, TITLE, chart, folder, import_selection, import_selection_with, minimal,
    ratio, song_list, view,
};
use skin_beatoraja::{EssentialPart, ImportError, NoteKind, PropertyKind};

fn at(x: i32, y: i32, w: i32, h: i32) -> String {
    format!(r#"{{"x": {x}, "y": {y}, "w": {w}, "h": {h}}}"#)
}

/// A skin with option `p` (items 900, the default, and 901) and `list`.
fn with_option(list: &str, texts: &str) -> String {
    format!(
        r#""property": [{{"name": "p", "item": [{{"name": "a", "op": 900}}, {{"name": "b", "op": 901}}]}}],
           "imageset": [{SET}], "text": [{texts}], {list},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]"#
    )
}

/// The essential part an import refused.
#[track_caller]
fn refused(r: Result<skin_beatoraja::Imported, ImportError>) -> EssentialPart {
    match r {
        Err(ImportError::Essential { part, .. }) => part,
        other => panic!("{:?}", other.map(|i| i.report)),
    }
}

#[test]
fn inactive_arms_and_decorative_styles_fail_with_notes() {
    // listoff[center] with an unsupported stretch, liston[1] (not the
    // center) likewise, and a lamp whose image is undefined.
    let bad = r#"{"x": 0, "y": 900, "w": 300, "h": 40}], "stretch": 1, "dummy": [0"#;
    let list = song_list(
        &[&at(0, 900, 300, 40), bad],
        &[bad, &at(0, 800, 300, 40)],
        0,
        &format!(
            r#"{TITLE}, "lamp": [{{"id": "nothing", "dst": [{}]}}]"#,
            at(0, 0, 8, 8)
        ),
    );
    let d = folder(&with_option(&list, TEXT_T));
    let i = import_selection(d.path()).unwrap();
    let stretched = i
        .report
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::Stretch(1))
        .count();
    assert_eq!(stretched, 2);
    assert!(common::has(&i.report, NoteKind::UndefinedObject));
    // Slot 0 draws its selected placement, slot 1 its unselected one.
    let mut s = Sel::new(i.skin);
    s.view(&view(vec![chart("a"), chart("b")], Some(0), None, None));
    assert_eq!(s.images(0).len(), 2);
}

#[test]
fn an_active_arm_that_cannot_be_reproduced_refuses() {
    let bad = r#"{"x": 0, "y": 900, "w": 300, "h": 40}], "stretch": 1, "dummy": [0"#;
    let list = song_list(&[bad], &[&at(0, 900, 300, 40)], 0, TITLE);
    let d = folder(&with_option(&list, TEXT_T));
    assert_eq!(
        refused(import_selection(d.path())),
        EssentialPart::SelectedRow
    );
    // A non-center slot's active arm, its `listoff`.
    let list = song_list(
        &[&at(0, 900, 300, 40), &at(0, 800, 300, 40)],
        &[&at(0, 900, 300, 40), bad],
        0,
        TITLE,
    );
    let d = folder(&with_option(&list, TEXT_T));
    assert_eq!(refused(import_selection(d.path())), EssentialPart::SongList);
}

#[test]
fn title_styles_follow_their_chains_or_refuse() {
    let place = at(0, 900, 300, 40);
    let texts = format!(r#"{TEXT_T}, {{"id": "m", "font": "missing", "ref": 10}}"#);
    let entry = |id: &str| format!(r#"{{"id": "{id}", "dst": [{}]}}"#, at(0, 0, 100, 10));
    let none = r#"{"id": "none", "dst": []}"#;
    // text[4] defined with a missing font while text[0] is fine: refused
    // for folders, where the source would drop it and use text[0].
    let styles = format!(
        r#""text": [{}, {none}, {none}, {none}, {}]"#,
        entry("t"),
        entry("m")
    );
    let list = song_list(&[&place], &[&place], 0, &styles);
    let d = folder(&with_option(&list, &texts));
    assert_eq!(refused(import_selection(d.path())), EssentialPart::RowTitle);
    // No text[4] and no text[0], text[2] fine: refused for folders.
    let styles = format!(r#""text": [{none}, {none}, {}]"#, entry("t"));
    let list = song_list(&[&place], &[&place], 0, &styles);
    let d = folder(&with_option(&list, &texts));
    assert_eq!(refused(import_selection(d.path())), EssentialPart::RowTitle);
}

#[test]
fn the_selected_row_must_draw_after_folding() {
    let on = r#"{"x": 0, "y": 900, "w": 300, "h": 40}], "op": [901], "dummy": [0"#;
    let list = song_list(&[on], &[&at(0, 900, 300, 40)], 0, TITLE);
    let d = folder(&with_option(&list, TEXT_T));
    assert_eq!(
        refused(import_selection(d.path())),
        EssentialPart::SelectedRow
    );
    // The option item that folds it true.
    import_selection_with(d.path(), |c| c.options = vec![Some(901)]).unwrap();
}

/// A bar image cut 10 texels wider than its 800-wide file: the selected
/// row draws it, the file's columns stretched over the bar, with a D22
/// note, rather than refusing the row.
#[test]
fn a_bar_cut_past_its_file_s_edge_is_stretched() {
    let body = format!(
        r#""source": [{{"id": "s", "path": "s.png"}}, {{"id": "w", "path": "w.png"}}],
           "image": [{{"id": "wide", "src": "w", "w": 810, "h": 60}}],
           "imageset": [{{"id": "set", "images": ["wide"]}}], "text": [{TEXT_T}],
           {},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]"#,
        song_list(&[&at(0, 500, 810, 60)], &[&at(0, 500, 810, 60)], 0, TITLE)
    );
    let d = folder(&body);
    common::png(d.path(), "w.png", 800, 300);
    let i = import_selection(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(common::has(
        &i.report,
        NoteKind::Deviation(skin_beatoraja::DeviationId::D22)
    ));
    let mut s = Sel::new(i.skin);
    s.view(&view(vec![chart("a")], Some(0), None, None));
    let rects: Vec<[f32; 4]> = s.frame(0).iter().map(|q| q.rect).collect();
    assert_eq!(rects, [[0.0, 520.0, 810.0, 60.0]]);
}

/// The selected row's image file missing: refused as the selected row,
/// naming the missing file, not as a play-critical image.
#[test]
fn a_missing_bar_image_refuses_the_selected_row() {
    let d = folder(&minimal(""));
    std::fs::remove_file(d.path().join("s.png")).unwrap();
    match import_selection(d.path()) {
        Err(ImportError::Essential {
            part: EssentialPart::SelectedRow,
            why: Some(NoteKind::MissingAsset),
            at: Some(_),
        }) => {}
        other => panic!("{:?}", other.map(|i| i.report)),
    }
}

#[test]
fn a_skin_without_a_drawn_list_refuses() {
    let d = folder(&format!(r#""imageset": [{SET}], "text": [{TEXT_T}]"#));
    assert_eq!(refused(import_selection(d.path())), EssentialPart::SongList);
    // A list no destination names.
    let list = song_list(&[&at(0, 900, 300, 40)], &[&at(0, 900, 300, 40)], 0, TITLE);
    let d = folder(&format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}], {list}"#
    ));
    assert_eq!(refused(import_selection(d.path())), EssentialPart::SongList);
    // A center past the placements.
    let list = song_list(&[&at(0, 900, 300, 40)], &[&at(0, 900, 300, 40)], 1, TITLE);
    let d = folder(&with_option(&list, TEXT_T));
    assert_eq!(
        refused(import_selection(d.path())),
        EssentialPart::SelectedRow
    );
}

/// C9 and C10 through the importer: the EX-rate graph cuts its image to
/// the ratio, the position slider moves by it and gives the ratio under
/// the pointer; the master volume slider is unavailable, a folder
/// distribution graph omitted, each with its note.
#[test]
fn ratio_bars_draw_their_ratios() {
    let d = folder(&minimal(
        r#""graph": [{"id": "g", "src": "s", "x": 40, "y": 0, "w": 8, "h": 8, "type": 147, "angle": 0},
                     {"id": "gv", "src": "s", "x": 40, "y": 0, "w": 8, "h": 8, "type": 147},
                     {"id": "dist", "src": "s", "x": 40, "y": 0, "w": 8, "h": 8, "type": -1}],
           "slider": [{"id": "pos", "src": "s", "x": 40, "y": 0, "w": 8, "h": 8, "type": 1, "angle": 1, "range": 200},
                      {"id": "vol", "src": "s", "x": 40, "y": 0, "w": 8, "h": 8, "type": 17}],
           "destination": [{"id": "list", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]},
               {"id": "g", "dst": [{"x": 0, "y": 0, "w": 90, "h": 10}]},
               {"id": "gv", "dst": [{"x": 300, "y": 0, "w": 10, "h": 90}]},
               {"id": "dist", "dst": [{"x": 0, "y": 0, "w": 90, "h": 10}]},
               {"id": "pos", "dst": [{"x": 0, "y": 100, "w": 10, "h": 10}]},
               {"id": "vol", "dst": [{"x": 0, "y": 200, "w": 10, "h": 10}]}]"#,
    ));
    let i = import_selection(d.path()).unwrap();
    assert!(common::has(
        &i.report,
        NoteKind::Unavailable {
            kind: PropertyKind::Slider,
            id: 17
        }
    ));
    assert!(common::has(
        &i.report,
        NoteKind::UnsupportedProperty {
            kind: PropertyKind::Graph,
            id: Some(-1)
        }
    ));
    let mut s = Sel::new(i.skin);
    let mut row = chart("a");
    row.score = common::selection::best(50, 0, 1);
    let mut v = view(vec![row, chart("b"), chart("c")], Some(0), Some(150), None);
    v.position = ratio(1, 2);
    s.view(&v);
    let bars: Vec<[f32; 4]> = s
        .drawn(0)
        .into_iter()
        // A cut graph's middle texel lies off the whole image's middle.
        .filter(|(t, _)| t[1] < 8 && (40..48).contains(&t[0]))
        .map(|(_, r)| r)
        .collect();
    // 90 × 50/150 wide from the left; with no `angle` a graph is vertical
    // (`JsonSkin.java:236`), 90 × 50/150 high from the bottom; the slider
    // 200 × 1/2 to the right.
    assert_eq!(
        bars,
        [
            [0.0, 1070.0, 30.0, 10.0],
            [300.0, 1050.0, 10.0, 30.0],
            [100.0, 970.0, 10.0, 10.0]
        ]
    );
    let _ = BAR;
    let hit = s
        .skin
        .slider_at(&s.sel, [150.0, 975.0], [1_920, 1_080])
        .unwrap();
    assert_eq!(hit.source, skin_model::RatioSource::ListPosition);
    // 150 of the 200 travel from the slider's place: 3/4.
    assert_eq!(4 * hit.value.numerator, 3 * hit.value.denominator);
}

/// C20: a decision or result skin still has no importer.
#[test]
fn decision_and_result_skins_have_no_importer() {
    for (ty, kind) in [
        (6, skin_model::ScreenKind::Decision),
        (7, skin_model::ScreenKind::Result),
    ] {
        let d = common::skin(&[("s.json", &format!(r#"{{"type": {ty}}}"#))]);
        let (mut def, h) = skin_beatoraja::read_header(d.path(), "s.json").unwrap();
        let r = skin_beatoraja::import(&mut def, &h, &skin_beatoraja::Config::defaults(&h), kind);
        assert!(matches!(r, Err(ImportError::NoImporter { kind: k }) if k == kind));
    }
}

/// An active `listoff` the list does not give: slot 1's unselected arm
/// with only slot 0's `listoff` written refuses the song list, where
/// the inactive `listoff` of the center slot missing is only noted.
#[test]
fn a_missing_active_listoff_refuses() {
    let place = [at(0, 900, 300, 40), at(0, 800, 300, 40)];
    let list = song_list(&[&place[0], &place[1]], &[&place[0]], 0, TITLE);
    let d = folder(&with_option(&list, TEXT_T));
    assert_eq!(refused(import_selection(d.path())), EssentialPart::SongList);
    // Center 1: slot 0's `listoff` is active and given, slot 1's (the
    // center's) inactive and absent.
    let list = song_list(&[&place[0], &place[1]], &[&place[0]], 1, TITLE);
    let d = folder(&with_option(&list, TEXT_T));
    let i = import_selection(d.path()).unwrap();
    assert!(common::has(
        &i.report,
        NoteKind::Ignored { field: "listoff" }
    ));
}

/// A placement's images run on the set's timing, as the source takes it
/// (`JsonSelectSkinObjectLoader.java:47-66`): the first loadable image's
/// cycle and the first timer given, here image 1's timer 11 for image 0's
/// two frames. The same image drawn on its own keeps its own timing,
/// whichever is read first.
#[test]
fn placements_take_their_set_s_timing() {
    let images = r#""image": [
        {"id": "b0", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8},
        {"id": "m0", "src": "s", "x": 0, "y": 16, "w": 16, "h": 8, "divx": 2, "cycle": 100},
        {"id": "m1", "src": "s", "x": 16, "y": 16, "w": 8, "h": 8, "timer": 11}]"#;
    let list = song_list(&[&at(0, 900, 300, 40)], &[&at(0, 900, 300, 40)], 0, TITLE);
    for first in [true, false] {
        let plain = r#"{"id": "m0", "dst": [{"x": 500, "y": 0, "w": 8, "h": 8}]}"#;
        let listed = r#"{"id": "list", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#;
        let dsts = if first {
            format!("{plain}, {listed}")
        } else {
            format!("{listed}, {plain}")
        };
        let body = format!(
            r#""imageset": [{{"id": "set", "images": ["m0", "m1"]}}], "text": [{TEXT_T}], {list},
               "destination": [{dsts}], {images}"#
        );
        let d = folder(&body);
        let mut s = Sel::of(d.path());
        s.view(&view(vec![chart("a")], Some(0), None, None));
        // Two frames over a 100 ms cycle; the change at 125 ms: at 150 ms
        // the placement is 25 ms into its cycle (frame 0), the image on its
        // own 150 ms (frame 1).
        s.timers.change = Some(bms_model::TimeUs(125_000));
        let mut frames: Vec<(f32, [u8; 2])> =
            s.drawn(150).into_iter().map(|(t, r)| (r[0], t)).collect();
        frames.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(frames, [(0.0, [4, 20]), (500.0, [12, 20])], "{first}");
    }
}
