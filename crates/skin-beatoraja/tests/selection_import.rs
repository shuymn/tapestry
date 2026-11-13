//! The selection importer's essential parts (C17), its ratio bars (C9,
//! C10 through the importer) and the play and other kinds beside it (C20).

mod common;

use common::selection::{
    B0, B1, B5, BAR, IMAGES, SET, Sel, TEXT_T, TITLE, all_row, chart, folder, folder_row,
    import_selection, import_selection_with, minimal, ratio, song_list, view,
};
use skin_beatoraja::{EssentialPart, ImportError, NoteKind, PropertyKind, Severity};

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
/// row draws it, its last ten columns repeating the file's last column,
/// with no note, rather than refusing the row.
#[test]
fn a_bar_cut_past_its_file_s_edge_repeats_its_edge() {
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
    assert!(
        !i.report
            .notes
            .iter()
            .any(|n| matches!(n.kind, NoteKind::Deviation(_))),
        "{:?}",
        i.report
    );
    // The file's last ten columns red, painted before the atlas is built:
    // the bar samples red from column 790 to its right end.
    common::png_with(d.path(), "w.png", 800, 300, |x, _| {
        if x >= 790 {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        }
    });
    let mut s = Sel::new(i.skin);
    s.view(&view(vec![chart("a")], Some(0), None, None));
    let quads = s.frame(0);
    let rects: Vec<[f32; 4]> = quads.iter().map(|q| q.rect).collect();
    assert_eq!(rects, [[0.0, 520.0, 810.0, 60.0]]);
    let q = quads[0];
    let at = |fu| common::play::texel_at(&s.atlas, &q, fu, 0.5)[0];
    assert_eq!((at(0.5), at(0.995), at(791.0 / 810.0)), (0, 255, 255));
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
/// the pointer; a folder distribution graph is omitted with its note; the
/// system volume slider is bridged, with no note, and draws nothing before
/// the settings.
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
    assert!(!common::has(
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
        .drag_at(&s.sel, [150.0, 975.0], [1_920, 1_080])
        .unwrap();
    assert_eq!(hit.source, skin_model::RatioSource::ListPosition);
    // 150 of the 200 travel from the slider's place: 3/4.
    assert_eq!(4 * hit.value.numerator, 3 * hit.value.denominator);
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

/// A negative timer id is no timer (`TimerPropertyFactory.java:7-8`) and
/// a cycle of -1 none: the set's search passes both, so image 0's two
/// frames run on image 1's timer 11 and its 100 ms cycle.
#[test]
fn a_set_s_timing_passes_absent_timers_and_cycles() {
    let images = r#""image": [
        {"id": "m0", "src": "s", "x": 0, "y": 16, "w": 16, "h": 8, "divx": 2, "timer": -1, "cycle": -1},
        {"id": "m1", "src": "s", "x": 16, "y": 16, "w": 8, "h": 8, "timer": 11, "cycle": 100}]"#;
    let list = song_list(&[&at(0, 900, 300, 40)], &[&at(0, 900, 300, 40)], 0, TITLE);
    let body = format!(
        r#""imageset": [{{"id": "set", "images": ["m0", "m1"]}}], "text": [{TEXT_T}], {list},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}], {images}"#
    );
    let d = folder(&body);
    let mut s = Sel::of(d.path());
    s.view(&view(vec![chart("a")], Some(0), None, None));
    // The change at 125 ms: at 150 ms 25 ms into the cycle (frame 0), at
    // 180 ms 55 ms (frame 1).
    s.timers.change = Some(bms_model::TimeUs(125_000));
    assert_eq!(s.images(150), [[4, 20]]);
    assert_eq!(s.images(180), [[12, 20]]);
}

/// The fixtures' sources and images with source `m` added, whose file
/// `m.png` is never written (a file the source cannot load either), its
/// images `x0`, `x1` and `x5`; source `c`, the file [`corrupt`] writes,
/// its images `c1` and `ct` (timer 21, cycle 100); and `more` images.
fn with_missing(more: &str) -> String {
    format!(
        r#""source": [{{"id": "s", "path": "s.png"}}, {{"id": "m", "path": "m.png"}},
                      {{"id": "c", "path": "c.png"}}],
           "image": [{IMAGES},
               {{"id": "x0", "src": "m", "x": 0, "y": 0, "w": 8, "h": 8}},
               {{"id": "x1", "src": "m", "x": 8, "y": 0, "w": 8, "h": 8}},
               {{"id": "x5", "src": "m", "x": 16, "y": 0, "w": 8, "h": 8}},
               {{"id": "c1", "src": "c", "x": 8, "y": 0, "w": 8, "h": 8}},
               {{"id": "ct", "src": "c", "x": 8, "y": 0, "w": 8, "h": 8, "timer": 21, "cycle": 100}}{more}]"#
    )
}

/// `c.png` in `root`: a 64×64 PNG cut short after its header, which
/// admission reads and a whole decode fails, as the source's texture does.
fn corrupt(root: &std::path::Path) {
    common::png_with(root, "c.png", 64, 64, |x, y| {
        [u8::try_from(x).unwrap(), u8::try_from(y).unwrap(), 0, 255]
    });
    let path = root.join("c.png");
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
}

/// A song list of `slots` places 100 apart from y 900 down, center
/// `center`, every slot drawing [`SET`] but slot `odd`, which draws the
/// set `odd_set` (its `images`); its titles, and the list drawn.
fn odd_slot(slots: usize, center: usize, odd: usize, odd_set: &str) -> String {
    let arms: Vec<String> = (0..slots)
        .map(|k| {
            let set = if k == odd { "odd" } else { "set" };
            let y = 900 - 100 * i32::try_from(k).unwrap();
            format!(r#"{{"id": "{set}", "dst": [{}]}}"#, at(0, y, 300, 40))
        })
        .collect();
    let arms = arms.join(",");
    format!(
        r#""imageset": [{SET}, {{"id": "odd", "images": {odd_set}}}], "text": [{TEXT_T}],
           "songlist": {{"id": "list", "center": {center}, "liston": [{arms}], "listoff": [{arms}], {TITLE}}},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]"#
    )
}

/// The quads of the frame at 0 with every row's title given a line,
/// counted by the 40-pixel band each slot's place covers from its top
/// `tops` (y down): a row with its bar 2, one without 1, none 0.
fn per_slot(s: &mut Sel, tops: &[f32]) -> Vec<usize> {
    let line = skin_model::TextImage::new(
        skin_model::FrameUv {
            uv: [0.0, 0.0, 0.5, 0.5],
            layer: 0,
        },
        [0.0, 0.0, 1.0, 1.0],
        1.0,
    );
    let texts = skin_model::TextTable::new(vec![None; s.skin.text_slots()]);
    let rows = skin_model::TextTable::new(vec![line; s.skin.song_list_slots()]);
    s.skin
        .attach_selection_text(&mut s.sel, texts, rows)
        .unwrap();
    let quads = s.frame(0);
    tops.iter()
        .map(|&top| {
            quads
                .iter()
                .filter(|q| (top..top + 40.0).contains(&q.rect[1]))
                .count()
        })
        .collect()
}

/// The dropped notes of kind `kind`.
fn dropped(r: &skin_beatoraja::Report, kind: NoteKind) -> usize {
    r.notes
        .iter()
        .filter(|n| n.severity == Severity::Dropped && n.kind == kind)
        .count()
}

/// A non-center slot whose image 0's file is missing draws nothing, as
/// the source's (`select/BarRenderer.java:250-290`): the skin imports
/// with the hole noted, and the other slots, the center's included, draw
/// their bars and titles.
#[test]
fn a_missing_non_center_bar_image_hides_its_slot() {
    let body = format!(
        "{}, {}",
        with_missing(""),
        odd_slot(4, 1, 3, r#"["x0", "b1"]"#)
    );
    let d = folder(&body);
    let i = import_selection(d.path()).unwrap();
    // Slot 3's two arms, each noted.
    assert_eq!(
        dropped(&i.report, NoteKind::MissingAsset),
        2,
        "{:?}",
        i.report
    );
    let mut s = Sel::new(i.skin);
    let rows = vec![chart("a"), chart("b"), chart("c"), chart("d")];
    s.view(&view(rows, Some(1), None, None));
    // Slots 0 to 2 their bar image 0 at their places; slot 3 (y 600)
    // nothing.
    let bars: Vec<([u8; 2], f32)> = s.drawn(0).into_iter().map(|(t, r)| (t, r[1])).collect();
    assert_eq!(bars, [(B0, 140.0), (B0, 240.0), (B0, 340.0)]);
    // Each drawn slot its bar and title; slot 3 not even its title.
    assert_eq!(
        per_slot(&mut s, &[140.0, 240.0, 340.0, 440.0]),
        [2, 2, 2, 0]
    );
}

/// A row kind's image inside the set whose file is missing, or does not
/// decode, draws that row with its title and no bar, as the source's slot
/// image with no region (`SkinImage.java:133-139`,
/// `select/SkinBar.java:266-274`); a row kind's index (chart 0, folder 1,
/// "all charts" 5) past the set's end takes image 0, as the source's
/// (`SkinImage.java:133-135`), whatever image 1 is.
#[test]
fn a_missing_row_kind_image_draws_its_row_without_a_bar() {
    let place = [
        at(0, 900, 300, 40),
        at(0, 800, 300, 40),
        at(0, 700, 300, 40),
    ];
    let refs: Vec<&str> = place.iter().map(String::as_str).collect();
    let list = song_list(&refs, &refs, 1, TITLE);
    // Rows by slot: the chart, the folder, the "all charts" row; each
    // slot's quads, its bar and title (2) or its title alone (1).
    for (set, bars, holes, quads) in [
        (
            r#"["b0", "x1", "b0", "b0", "b0", "b5"]"#,
            vec![B0, B5],
            1,
            [2, 1, 2],
        ),
        (
            r#"["b0", "b1", "b0", "b0", "b0", "x5"]"#,
            vec![B0, B1],
            1,
            [2, 2, 1],
        ),
        (
            r#"["b0", "c1", "b0", "b0", "b0", "b5"]"#,
            vec![B0, B5],
            1,
            [2, 1, 2],
        ),
        (r#"["b0", "x1"]"#, vec![B0, B0], 1, [2, 1, 2]),
        (r#"["b0", "b1"]"#, vec![B0, B1, B0], 0, [2, 2, 2]),
        (r#"["b0"]"#, vec![B0, B0, B0], 0, [2, 2, 2]),
    ] {
        let body = format!(
            r#"{}, "imageset": [{{"id": "set", "images": {set}}}], "text": [{TEXT_T}], {list},
               "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]"#,
            with_missing("")
        );
        let d = folder(&body);
        corrupt(d.path());
        let i = import_selection(d.path()).unwrap();
        // Each of the three slots' two arms notes its hole.
        assert_eq!(
            dropped(&i.report, NoteKind::MissingAsset),
            6 * holes,
            "{set}"
        );
        let mut s = Sel::new(i.skin);
        s.view(&view(
            vec![chart("c"), folder_row("f", 3), all_row(9)],
            Some(1),
            None,
            None,
        ));
        assert_eq!(s.images(0), bars, "{set}");
        // Every row's title draws, its bar or not.
        assert_eq!(per_slot(&mut s, &[140.0, 240.0, 340.0]), quads, "{set}");
    }
}

/// The set's timing comes from the images that load
/// (`JsonSelectSkinObjectLoader.java:50-63`): the first timer given and
/// the first cycle other than -1, a missing file passed over; a file only
/// this importer refuses still refuses the selected row.
#[test]
fn a_set_s_timing_passes_images_that_do_not_load() {
    let list = song_list(&[&at(0, 900, 300, 40)], &[&at(0, 900, 300, 40)], 0, TITLE);
    let timed = r#",
        {"id": "a0", "src": "s", "x": 0, "y": 16, "w": 16, "h": 8, "divx": 2, "cycle": -1},
        {"id": "c0", "src": "s", "x": 0, "y": 16, "w": 16, "h": 8, "divx": 2, "cycle": 300},
        {"id": "t1", "src": "m", "x": 8, "y": 0, "w": 8, "h": 8, "timer": 21, "cycle": 100},
        {"id": "t5", "src": "s", "x": 16, "y": 16, "w": 8, "h": 8, "timer": 22, "cycle": 200},
        {"id": "cx", "src": "c", "x": -1, "y": 0, "w": 8, "h": 8, "timer": 21, "cycle": 100},
        {"id": "w0", "src": "w", "x": 0, "y": 0, "w": 8, "h": 1}"#;
    let skin = |set: &str| {
        format!(
            r#"{}, "imageset": [{{"id": "set", "images": {set}}}], "text": [{TEXT_T}], {list},
               "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]"#,
            with_missing(timed)
        )
    };
    // Timer 21 (panel 1) from 0 and timer 22 (panel 2) from 1 s.
    let start = |s: &mut Sel| {
        s.timers.panels = [
            Some(bms_model::TimeUs(0)),
            Some(bms_model::TimeUs(1_000_000)),
            None,
        ];
    };
    // Image 1's file missing: image 0's two frames on image 5's timer 22
    // and 200 ms cycle, 50 ms then 120 ms into it (21 and 100 would give
    // frame 1, then 0).
    let d = folder(&skin(r#"["a0", "t1", "a0", "a0", "a0", "t5"]"#));
    let mut s = Sel::of(d.path());
    start(&mut s);
    s.view(&view(vec![chart("a")], Some(0), None, None));
    assert_eq!(s.images(1_050), [[4, 20]]);
    assert_eq!(s.images(1_120), [[12, 20]]);
    // Image 2, never drawn, a file whose whole decode fails, with timer 21
    // and cycle 100: passed over, the same frames; so too with a rectangle
    // this importer refuses, since the source's texture fails before its
    // rectangle is read.
    for two in ["ct", "cx"] {
        let d = folder(&skin(&format!(
            r#"["a0", "a0", "{two}", "a0", "a0", "t5"]"#
        )));
        corrupt(d.path());
        let mut s = Sel::of(d.path());
        start(&mut s);
        s.view(&view(vec![chart("a")], Some(0), None, None));
        assert_eq!(s.images(1_050), [[4, 20]], "{two}");
        assert_eq!(s.images(1_120), [[12, 20]], "{two}");
    }
    // Image 0's cycle 300 with no timer, image 5's timer 22: 100 ms into
    // the cycle frame 0, 200 ms frame 1 (no timer would give 1 at 1.1 s).
    let d = folder(&skin(r#"["c0", "a0", "a0", "a0", "a0", "t5"]"#));
    let mut s = Sel::of(d.path());
    start(&mut s);
    s.view(&view(vec![chart("a")], Some(0), None, None));
    assert_eq!(s.images(1_100), [[4, 20]]);
    assert_eq!(s.images(1_200), [[12, 20]]);
    // Image 0 a file past the decoder's cap, which the source loads, so no
    // hole: on the center slot refused as the selected row, on another as
    // the song list, each naming the cap.
    for (odd, part) in [
        (0, EssentialPart::SelectedRow),
        (1, EssentialPart::SongList),
    ] {
        let body = format!(
            "{}, {}",
            with_missing(timed),
            odd_slot(2, 0, odd, r#"["w0"]"#)
        )
        .replacen(
            r#"{"id": "m", "path": "m.png"}"#,
            r#"{"id": "m", "path": "m.png"}, {"id": "w", "path": "w.png"}"#,
            1,
        );
        let d = folder(&body);
        common::png(d.path(), "w.png", 1_100_000, 1);
        match import_selection(d.path()) {
            Err(ImportError::Essential {
                part: p,
                why: Some(NoteKind::OverCap { .. }),
                ..
            }) => assert_eq!(p, part),
            other => panic!("{:?}", other.map(|i| i.report)),
        }
    }
}
