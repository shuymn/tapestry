//! Pointer input imported on a selection skin (docs/stage3.md S10-09e):
//! an image's or an image set's `act` and `click` become its action, the
//! song list's `clickable` its slots' order, text 30 by `ref` the search
//! box and `mouseRect` a hover rect, each read back through `hit_at` and
//! the frame as the production path draws it.

mod common;

use common::selection::{
    IMAGES, SET, Sel, TEXT_T, TITLE, chart, folder, folder_row, import_selection, song_list, view,
};
use skin_beatoraja::{EssentialPart, ImportError, NoteKind, PropertyKind};
use skin_model::{
    Button, PointerHit, PressHit, RatioSource, RowRef, RowRefKind, RowView, SearchBoxes,
};
use tempfile::TempDir;

const TARGET: [u32; 2] = [1_920, 1_080];

/// A selection skin folder whose `s.json` declares [`IMAGES`] and
/// `images`, then `body`.
fn skin(images: &str, body: &str) -> TempDir {
    let d = folder("");
    let json = format!(
        r#"{{"type": 5, "w": 1920, "h": 1080,
            "source": [{{"id": "s", "path": "s.png"}}],
            "font": [{{"id": "f", "path": "f.ttf"}}],
            "image": [{IMAGES}{images}], {body}}}"#
    );
    common::write(d.path(), "s.json", json.as_bytes());
    d
}

/// A body with one placement and its title, the image sets [`SET`] and
/// `sets`, the texts [`TEXT_T`] and `texts`, the list drawn and then the
/// destinations `dsts`.
fn body(sets: &str, texts: &str, dsts: &str) -> String {
    let list = song_list(
        &[r#"{"x": 100, "y": 500, "w": 300, "h": 40}"#],
        &[r#"{"x": 100, "y": 500, "w": 300, "h": 40}"#],
        0,
        TITLE,
    );
    format!(
        r#""imageset": [{SET}{sets}], "text": [{TEXT_T}{texts}], {list},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}{dsts}]"#
    )
}

/// A destination of `id` whose region's y-down top-left is `(x, top)`,
/// `w`×`h`, with `more` fields.
fn dst(id: &str, [x, top, w, h]: [i32; 4], more: &str) -> String {
    let y = 1_080 - top - h;
    format!(r#", {{"id": "{id}", {more} "dst": [{{"x": {x}, "y": {y}, "w": {w}, "h": {h}}}]}}"#)
}

/// `rows` numbered as a view numbers its selectable rows.
fn numbered(rows: Vec<RowView>) -> Vec<RowView> {
    rows.into_iter()
        .enumerate()
        .map(|(i, r)| RowView {
            index: Some(u32::try_from(i).unwrap()),
            ..r
        })
        .collect()
}

/// `s` drawn once over two charts.
fn drawn(s: &mut Sel) {
    s.view(&view(
        numbered(vec![chart("a"), chart("b")]),
        Some(0),
        None,
        None,
    ));
    s.frame(0);
}

fn hit(s: &Sel, at: [f32; 2], button: Button) -> Option<PointerHit> {
    s.skin.hit_at(&s.sel, at, TARGET, button).hit
}

fn action(event: u16, arg: i8) -> Option<PointerHit> {
    Some(PointerHit::Action { event, arg })
}

/// Each click type gives its argument; every profile event's image is a
/// target, an N event (the cover switch 331 among them) noted nothing and
/// a D one as ignored; an image set and an image group take their action as an
/// image does; an unknown event or a `click` past 3 has no target, each
/// noted.
#[test]
fn act_and_click_become_actions() {
    let images = r#",
        {"id": "a0", "src": "s", "x": 48, "y": 0, "w": 8, "h": 8, "act": 15},
        {"id": "a1", "src": "s", "x": 48, "y": 0, "w": 8, "h": 8, "act": 15, "click": 1},
        {"id": "a2", "src": "s", "x": 48, "y": 0, "w": 8, "h": 8, "act": 315, "click": 2},
        {"id": "a3", "src": "s", "x": 48, "y": 0, "w": 8, "h": 8, "act": 12, "click": 3},
        {"id": "e", "src": "s", "x": 48, "y": 0, "w": 8, "h": 8, "act": 331},
        {"id": "dd", "src": "s", "x": 48, "y": 0, "w": 8, "h": 8, "act": 13},
        {"id": "unk", "src": "s", "x": 48, "y": 0, "w": 8, "h": 8, "act": 999},
        {"id": "bad", "src": "s", "x": 48, "y": 0, "w": 8, "h": 8, "act": 15, "click": 4},
        {"id": "grp", "src": "s", "x": 0, "y": 16, "w": 16, "h": 8, "divx": 2, "len": 2, "ref": 12, "act": 12, "click": 1}"#;
    let sets = r#", {"id": "is", "images": ["dot", "lab"], "ref": 12, "act": 12, "click": 2}"#;
    let dsts = [
        dst("a0", [0, 0, 100, 100], ""),
        dst("a1", [200, 0, 100, 100], ""),
        dst("a2", [400, 0, 100, 100], ""),
        dst("a3", [600, 0, 100, 100], ""),
        dst("e", [800, 0, 100, 100], ""),
        dst("dd", [1_000, 0, 100, 100], ""),
        dst("unk", [1_200, 0, 100, 100], ""),
        dst("bad", [1_400, 0, 100, 100], ""),
        dst("is", [0, 200, 100, 100], ""),
        dst("grp", [200, 200, 100, 100], ""),
    ]
    .concat();
    let d = skin(images, &body(sets, "", &dsts));
    let i = import_selection(d.path()).unwrap();
    let notes: Vec<NoteKind> = i.report.notes.iter().map(|n| n.kind).collect();
    let event = |id| NoteKind::Unavailable {
        kind: PropertyKind::Event,
        id,
    };
    for id in [12, 15, 315, 331] {
        assert!(!notes.contains(&event(id)), "{notes:?}");
    }
    let ignored = |field| NoteKind::Ignored { field };
    // The D event and the unknown one.
    assert_eq!(
        notes.iter().filter(|&&n| n == ignored("act")).count(),
        2,
        "{notes:?}"
    );
    assert_eq!(
        notes.iter().filter(|&&n| n == ignored("click")).count(),
        1,
        "{notes:?}"
    );
    let mut s = Sel::new(i.skin);
    drawn(&mut s);
    // Type 0: the button's step.
    assert_eq!(hit(&s, [50.0, 50.0], Button::Left), action(15, 1));
    assert_eq!(hit(&s, [50.0, 50.0], Button::Right), action(15, -1));
    // Type 1: negated.
    assert_eq!(hit(&s, [250.0, 50.0], Button::Left), action(15, -1));
    assert_eq!(hit(&s, [250.0, 50.0], Button::Right), action(15, 1));
    // Type 2: the halves, the middle on the right.
    assert_eq!(hit(&s, [449.0, 50.0], Button::Left), action(315, -1));
    assert_eq!(hit(&s, [450.0, 50.0], Button::Right), action(315, 1));
    // Type 3: top and bottom.
    assert_eq!(hit(&s, [650.0, 10.0], Button::Left), action(12, 1));
    assert_eq!(hit(&s, [650.0, 90.0], Button::Left), action(12, -1));
    // The cover switch and a D event: targets.
    assert_eq!(hit(&s, [850.0, 50.0], Button::Left), action(331, 1));
    assert_eq!(hit(&s, [1_050.0, 50.0], Button::Left), action(13, 1));
    // No target.
    assert_eq!(hit(&s, [1_250.0, 50.0], Button::Left), None);
    assert_eq!(hit(&s, [1_450.0, 50.0], Button::Left), None);
    // The image set and the group.
    assert_eq!(hit(&s, [10.0, 250.0], Button::Left), action(12, -1));
    assert_eq!(hit(&s, [90.0, 250.0], Button::Left), action(12, 1));
    assert_eq!(hit(&s, [250.0, 250.0], Button::Left), action(12, -1));
}

/// A song list tries its slots in `clickable`'s order: two slots drawn at
/// the same rect take the press as the first listed; an entry naming no
/// slot is dropped with a note.
#[test]
fn clickable_orders_the_slots() {
    let place = r#"{"x": 100, "y": 500, "w": 300, "h": 40}"#;
    let hit_row = |clickable: &str| {
        let list = song_list(
            &[place, place],
            &[place, place],
            0,
            &format!("{TITLE}, \"clickable\": {clickable}"),
        );
        let d = folder(&format!(
            r#""imageset": [{SET}], "text": [{TEXT_T}], {list},
               "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]"#
        ));
        let i = import_selection(d.path()).unwrap();
        let noted = common::has(&i.report, NoteKind::Ignored { field: "clickable" });
        let mut s = Sel::new(i.skin);
        s.view(&view(
            numbered(vec![chart("a"), folder_row("f", 1)]),
            Some(0),
            None,
            None,
        ));
        s.frame(0);
        let hit = hit(&s, [200.0, 560.0], Button::Right);
        (hit, noted)
    };
    let row = |index, kind| {
        Some(PointerHit::Row {
            row: RowRef {
                list_revision: 0,
                index,
                kind,
            },
            button: Button::Right,
        })
    };
    assert_eq!(hit_row("[1, 0]"), (row(1, RowRefKind::Folder), false));
    assert_eq!(hit_row("[0, 1]"), (row(0, RowRefKind::Chart), false));
    // 2 and −1 name no slot of two: dropped, slot 1 kept.
    assert_eq!(hit_row("[2, 1, -1]"), (row(1, RowRefKind::Folder), true));
    // None listed: no row takes a press.
    assert_eq!(hit_row("[]"), (None, false));
}

/// Text is the search box exactly when its `ref` is 30 and it has no
/// `event`: a `value` of 30 has no writer, and an `event` replaces the
/// query's.
#[test]
fn text_30_by_ref_is_the_search_box() {
    let case = |text: &str| {
        let texts = format!(r#", {{"id": "q", "font": "f", "size": 20, {text}}}"#);
        let d = skin("", &body("", &texts, &dst("q", [100, 100, 300, 20], "")));
        let mut s = Sel::of(d.path());
        drawn(&mut s);
        hit(&s, [200.0, 110.0], Button::Left)
    };
    assert_eq!(case(r#""ref": 30"#), Some(PointerHit::Search { index: 0 }));
    assert_eq!(case(r#""value": 30"#), None);
    assert_eq!(case(r#""ref": 30, "event": 1"#), None);
    // Another text is none.
    assert_eq!(case(r#""ref": 10"#), None);
}

/// `mouseRect` on a selection element is a hover rect, an offset from its
/// region's bottom-left corner, y up: the whole region shows the element
/// only while the pointer is inside it, the bottom half only on its lower
/// half; an absent pointer shows neither.
#[test]
fn mouse_rects_gate_selection_elements() {
    let dsts = [
        dst(
            "dot",
            [1_000, 300, 179, 37],
            r#""mouseRect": {"x": 0, "y": 0, "w": 179, "h": 37},"#,
        ),
        dst(
            "lab",
            [1_400, 300, 179, 37],
            r#""mouseRect": {"x": 0, "y": 0, "w": 179, "h": 18},"#,
        ),
    ]
    .concat();
    let d = skin("", &body("", "", &dsts));
    let i = import_selection(d.path()).unwrap();
    assert!(
        !common::has(&i.report, NoteKind::UnsupportedField { field: "mouseRect" }),
        "{:?}",
        i.report.notes
    );
    let mut s = Sel::new(i.skin);
    s.view(&view(vec![chart("a")], Some(0), None, None));
    let mut shows = |pointer| {
        s.pointer = pointer;
        let images = s.images(0);
        (
            images.contains(&common::selection::DOT),
            images.contains(&common::selection::LABEL),
        )
    };
    assert_eq!(shows(Some([1_010.0, 310.0])), (true, false));
    assert_eq!(shows(Some([1_200.0, 310.0])), (false, false));
    assert_eq!(shows(None), (false, false));
    assert_eq!(shows(Some([1_410.0, 330.0])), (false, true));
    assert_eq!(shows(Some([1_410.0, 305.0])), (false, false));
}

/// A song list part keeps no hover rect: the title style's `mouseRect`
/// refuses it, an essential part, with its note, as before pointer input.
#[test]
fn a_song_list_part_keeps_no_mouse_rect() {
    let title = r#""text": [{"id": "t", "mouseRect": {"x": 0, "y": 0, "w": 1, "h": 1}, "dst": [{"x": 2, "y": 3, "w": 100, "h": 10}]}]"#;
    let place = r#"{"x": 100, "y": 500, "w": 300, "h": 40}"#;
    let list = song_list(&[place], &[place], 0, title);
    let d = folder(&format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}], {list},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]"#
    ));
    match import_selection(d.path()) {
        Err(ImportError::Essential {
            part: EssentialPart::RowTitle,
            why: Some(NoteKind::UnsupportedField { field: "mouseRect" }),
            ..
        }) => {}
        other => panic!("{:?}", other.map(|i| i.report)),
    }
}

/// P7: a system volume slider drawn over the third row is imported with
/// no note and takes a press at its centre as a slider.
#[test]
fn a_volume_slider_over_a_row_is_hit_first() {
    let place = |top: i32| {
        format!(
            r#"{{"x": 100, "y": {}, "w": 300, "h": 40}}"#,
            1_080 - top - 40
        )
    };
    let places = [place(100), place(200), place(300)];
    let places: Vec<&str> = places.iter().map(String::as_str).collect();
    let list = song_list(
        &places,
        &places,
        0,
        &format!("{TITLE}, \"clickable\": [0, 1, 2]"),
    );
    let d = folder(&format!(
        r#""slider": [{{"id": "vol", "src": "s", "x": 40, "y": 0, "w": 8, "h": 8, "type": 17, "angle": 1, "range": 200}}],
           "imageset": [{SET}], "text": [{TEXT_T}], {list},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}{}]"#,
        dst("vol", [100, 300, 40, 40], ""),
    ));
    let i = import_selection(d.path()).unwrap();
    assert!(
        !i.report.notes.iter().any(|n| matches!(
            n.kind,
            NoteKind::UnsupportedProperty {
                kind: PropertyKind::Slider,
                ..
            }
        )),
        "{:?}",
        i.report.notes
    );
    let mut s = Sel::new(i.skin);
    s.skin
        .update_selection_settings(
            &mut s.sel,
            &skin_model::SelectionSettings {
                display_offset_ms: 0,
                system_volume: skin_model::VolumeReading::of_gain(0.5),
                speed: skin_model::SpeedView::Classic,
                order: skin_model::SelectionOrder::Title,
                gauge: None,
                covers_on: [false; 3],
                play_to_end: false,
                assignment: None,
            },
        )
        .unwrap();
    s.view(&view(
        numbered(vec![chart("a"), chart("b"), chart("c")]),
        Some(0),
        None,
        None,
    ));
    // The slider's image is drawn over the third row.
    let drawn = s.drawn(0);
    assert!(
        drawn
            .iter()
            .any(|(t, r)| *t == common::selection::BAR && r[1] >= 300.0 && r[1] < 340.0),
        "{drawn:?}"
    );
    match hit(&s, [120.0, 320.0], Button::Left) {
        Some(PointerHit::Slider(c)) => assert_eq!(c.source, RatioSource::SystemVolume),
        other => panic!("{other:?}"),
    }
    // Past its travel (200 to the right), the third row.
    assert_eq!(
        hit(&s, [390.0, 320.0], Button::Left),
        Some(PointerHit::Row {
            row: RowRef {
                list_revision: 0,
                index: 2,
                kind: RowRefKind::Chart
            },
            button: Button::Left,
        })
    );
}

/// A 2:1 image fitted with stretch 1 into a square destination is drawn
/// 100×50 in its middle, but takes a press anywhere in the destination:
/// the source tests a press against `region`, the destination, and
/// stretches only the rect it draws (`skin/SkinObject.java:591-602,
/// 623-643, 671-700`).
#[test]
fn a_stretched_image_takes_a_press_on_its_whole_destination() {
    let images = r#", {"id": "wide", "src": "s", "x": 0, "y": 16, "w": 16, "h": 8, "act": 12}"#;
    let dsts = dst("wide", [600, 400, 100, 100], r#""stretch": 1,"#);
    let d = skin(images, &body("", "", &dsts));
    let mut s = Sel::new(import_selection(d.path()).unwrap().skin);
    s.view(&view(numbered(vec![chart("a")]), Some(0), None, None));
    let quads = s.drawn(0);
    assert!(
        quads.iter().any(|(_, r)| *r == [600.0, 425.0, 100.0, 50.0]),
        "{quads:?}"
    );
    // Above and below the drawn rect, inside the destination.
    assert_eq!(hit(&s, [650.0, 410.0], Button::Left), action(12, 1));
    assert_eq!(hit(&s, [650.0, 490.0], Button::Left), action(12, 1));
    assert_eq!(hit(&s, [650.0, 501.0], Button::Left), None);
}

/// An image set of `extra`'s destination fields, fitted with stretch 1 into
/// a square destination, its first image square and its selected one
/// (ref 11: entry 2) 2:1, drawn: the selected image is drawn 100×50 in the
/// destination's middle.
fn wide_selected(extra: &str) -> Sel {
    let images = r#", {"id": "wide", "src": "s", "x": 0, "y": 16, "w": 16, "h": 8}"#;
    let set = r#", {"id": "is", "images": ["dot", "dot", "wide"], "ref": 11, "act": 12}"#;
    let more = format!(r#""stretch": 1, {extra}"#);
    let d = skin(
        images,
        &body(set, "", &dst("is", [600, 400, 100, 100], &more)),
    );
    let mut s = Sel::new(import_selection(d.path()).unwrap().skin);
    s.view(&view(numbered(vec![chart("a")]), Some(0), None, None));
    s
}

/// Where [`wide_selected`] draws its selected image.
const WIDE_DRAWN: [f32; 4] = [600.0, 425.0, 100.0, 50.0];

/// An image set's region is its destination whichever image is selected:
/// with its 2:1 image drawn 100×50 in the middle of its square destination,
/// a press anywhere in the destination is its action
/// (`skin/SkinObject.java:591-602, 623-643, 671-700`).
#[test]
fn an_image_set_takes_a_press_on_its_destination_whichever_image_draws() {
    let mut s = wide_selected("");
    let quads = s.drawn(0);
    assert!(quads.iter().any(|(_, r)| *r == WIDE_DRAWN), "{quads:?}");
    // Above and below the drawn rect, inside the destination.
    assert_eq!(hit(&s, [650.0, 410.0], Button::Left), action(12, 1));
    assert_eq!(hit(&s, [650.0, 490.0], Button::Left), action(12, 1));
    assert_eq!(hit(&s, [650.0, 501.0], Button::Left), None);
}

/// An image set's `mouseRect` runs from its destination's corner whichever
/// image is selected (`skin/SkinObject.java:603-607`): drawn only while the
/// pointer is in the destination's bottom 10 pixels, not the drawn rect's.
#[test]
fn an_image_set_s_mouse_rect_runs_from_its_destination_whichever_image_draws() {
    let mut s = wide_selected(r#""mouseRect": {"x": 0, "y": 0, "w": 100, "h": 10},"#);
    let mut drawn_at = |pointer: [f32; 2]| {
        s.pointer = Some(pointer);
        s.drawn(0).iter().any(|(_, r)| *r == WIDE_DRAWN)
    };
    assert!(drawn_at([650.0, 495.0]), "in the destination's bottom rows");
    assert!(!drawn_at([650.0, 470.0]), "in the drawn rect's bottom rows");
}

/// A right-aligned text's `mouseRect` runs from its destination's corner,
/// x 900, not from where its line is drawn: the source tests it against
/// `region` (`skin/SkinObject.java:603-607`). The text is the search box,
/// whose input box (x 500 to 900) takes the press only while the pointer
/// is in the hover rect.
#[test]
fn a_right_aligned_text_s_mouse_rect_is_anchored_at_its_destination() {
    let texts = r#", {"id": "q", "font": "f", "size": 20, "ref": 30, "align": 2}"#;
    let dsts = dst(
        "q",
        [900, 100, 400, 20],
        r#""mouseRect": {"x": 0, "y": 0, "w": 400, "h": 20},"#,
    );
    let d = skin("", &body("", texts, &dsts));
    let mut s = Sel::new(import_selection(d.path()).unwrap().skin);
    s.view(&view(numbered(vec![chart("a")]), Some(0), None, None));
    let mut search_at = |pointer: [f32; 2]| {
        s.pointer = Some(pointer);
        s.frame(0);
        hit(&s, [600.0, 110.0], Button::Left)
    };
    assert_eq!(
        search_at([1_000.0, 110.0]),
        Some(PointerHit::Search { index: 0 })
    );
    assert_eq!(
        search_at([1_300.0, 120.0]),
        Some(PointerHit::Search { index: 0 })
    );
    assert_eq!(search_at([600.0, 110.0]), None);
    assert_eq!(search_at([899.0, 110.0]), None);
}

/// A centred and a right-aligned search text's input box: the destination
/// moved left by half or all of its width (`skin/SkinText.java:162-169`).
#[test]
fn an_aligned_search_text_s_input_box() {
    for (align, left) in [(1, 700.0), (2, 500.0)] {
        let texts =
            format!(r#", {{"id": "q", "font": "f", "size": 20, "ref": 30, "align": {align}}}"#);
        let d = skin("", &body("", &texts, &dst("q", [900, 100, 400, 20], "")));
        let mut s = Sel::of(d.path());
        drawn(&mut s);
        let search = Some(PointerHit::Search { index: 0 });
        for (p, want) in [
            ([left, 100.0], search),
            ([left + 400.0, 120.0], search),
            ([left - 1.0, 110.0], None),
            ([left + 401.0, 110.0], None),
            ([left + 200.0, 121.0], None),
        ] {
            assert_eq!(hit(&s, p, Button::Left), want, "align {align} {p:?}");
        }
    }
}

/// An action drawn over the search text inside its input box takes the
/// press, the press still inside the box; a press on a row off the box is
/// outside it (`skin/Skin.java:394-397`).
#[test]
fn a_press_on_an_action_over_the_search_box_is_inside_it() {
    let images = r#", {"id": "order", "src": "s", "x": 48, "y": 0, "w": 8, "h": 8, "act": 12}"#;
    let place = r#"{"x": 100, "y": 500, "w": 300, "h": 40}"#;
    let list = song_list(
        &[place],
        &[place],
        0,
        &format!("{TITLE}, \"clickable\": [0]"),
    );
    let texts = r#", {"id": "q", "font": "f", "size": 20, "ref": 30}"#;
    let dsts = [
        dst("q", [100, 100, 300, 20], ""),
        dst("order", [150, 95, 50, 30], ""),
    ]
    .concat();
    let d = skin(
        images,
        &format!(
            r#""imageset": [{SET}], "text": [{TEXT_T}{texts}], {list},
               "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}{dsts}]"#
        ),
    );
    let mut s = Sel::of(d.path());
    drawn(&mut s);
    let press = |at| s.skin.hit_at(&s.sel, at, TARGET, Button::Left);
    assert_eq!(
        press([160.0, 110.0]),
        PressHit {
            hit: action(12, 1),
            inside_search: SearchBoxes(1),
        }
    );
    assert_eq!(
        press([200.0, 560.0]),
        PressHit {
            hit: Some(PointerHit::Row {
                row: RowRef {
                    list_revision: 0,
                    index: 0,
                    kind: RowRefKind::Chart,
                },
                button: Button::Left,
            }),
            inside_search: SearchBoxes::NONE,
        }
    );
}
