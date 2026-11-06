//! C25 (importer half): every song list count charged before what it
//! bounds, at its cap and one past it, and two song list destinations
//! sharing one definition while each is charged its own quads and ops.

mod common;

use common::selection::{SET, TEXT_T, TITLE, folder, import_selection, song_list};
use skin_beatoraja::{ImportError, ImportLimit};
use skin_model::{MAX_IMAGE_CHOICE_REFS, MAX_SKIN_KEYS, MAX_TRACK_KEYS, MAX_WIDGET_QUADS};

/// The song list elements' visibilities count to the skin's operation
/// cap: as many elements as fit are accepted, one more refuses.
#[test]
fn song_list_ops_count_to_the_cap() {
    // Sixty slots, each arm and the title under long conditions, so the
    // operations reach their cap before the quads do.
    let ops = "[2, 21, 22, 23, 101, 102, 103, 104, -105]";
    let place: Vec<String> = (0..60).map(|i| at(0, 10 * i, 300, 8)).collect();
    let arms: Vec<String> = place
        .iter()
        .map(|p| format!(r#"{p}], "op": {ops}, "dummy": [0"#))
        .collect();
    let refs: Vec<&str> = arms.iter().map(String::as_str).collect();
    let rest = format!(
        r#""text": [{{"id": "t", "op": {ops}, "dst": [{}]}}]"#,
        at(0, 0, 100, 10)
    );
    let body = |lists: usize| {
        let list = song_list(&refs, &refs, 0, &rest);
        let dsts = vec![r#"{"id": "list", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#; lists];
        format!(
            r#""imageset": [{SET}], "text": [{TEXT_T}], {list}, "destination": [{}]"#,
            dsts.join(",")
        )
    };
    let ops = |lists: usize| {
        import_selection(folder(&body(lists)).path())
            .unwrap()
            .report
            .usage
            .ops as usize
    };
    let (one, two) = (ops(1), ops(2));
    let per = two - one;
    let fit = 1 + (skin_model::MAX_SKIN_OPS - one) / per;
    assert_eq!(ops(fit), one + (fit - 1) * per);
    let (l, _) = limit(import_selection(folder(&body(fit + 1)).path()));
    assert_eq!(l, ImportLimit::Ops);
}

fn at(x: i32, y: i32, w: i32, h: i32) -> String {
    format!(r#"{{"x": {x}, "y": {y}, "w": {w}, "h": {h}}}"#)
}

/// A destination of image `dot` with `n` keys, one a millisecond.
fn filler(n: usize) -> String {
    let keys: Vec<String> = (0..n)
        .map(|t| format!(r#"{{"time": {t}, "x": 0, "y": 0, "w": 8, "h": 8}}"#))
        .collect();
    format!(r#"{{"id": "dot", "dst": [{}]}}"#, keys.join(","))
}

/// A skin of one placement (`on` its `liston` keys), its title, `fillers`
/// destinations before `lists` song list destinations, and `more`
/// members.
fn skin(on: &str, fillers: &[String], lists: usize, rest: &str, more: &str) -> String {
    let list = song_list(&[on], &[&at(0, 900, 300, 40)], 0, rest);
    let mut dsts: Vec<String> = fillers.to_vec();
    dsts.extend(
        (0..lists)
            .map(|_| r#"{"id": "list", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#.to_owned()),
    );
    let more = if more.is_empty() {
        String::new()
    } else {
        format!(", {more}")
    };
    format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}], {list},
           "destination": [{}]{more}"#,
        dsts.join(",")
    )
}

/// The limit an import stopped at, and the line it was located on.
#[track_caller]
fn limit(r: Result<skin_beatoraja::Imported, ImportError>) -> (ImportLimit, Option<u32>) {
    match r {
        Err(ImportError::Limit { limit, at }) => (limit, at.map(|a| a.line)),
        other => panic!("{:?}", other.map(|i| i.report.usage)),
    }
}

/// The song list's keys reach the skin's key cap: accepted at it, refused
/// one past it at the song list.
#[test]
fn song_list_keys_count_to_the_cap() {
    let on = at(0, 900, 300, 40);
    let base = import_selection(folder(&skin(&on, &[], 1, TITLE, "")).path())
        .unwrap()
        .report
        .usage
        .keys as usize;
    let room = MAX_SKIN_KEYS - base;
    let fillers: Vec<String> = (0..room / MAX_TRACK_KEYS)
        .map(|_| filler(MAX_TRACK_KEYS))
        .chain(
            [filler(room % MAX_TRACK_KEYS)]
                .into_iter()
                .filter(|_| !room.is_multiple_of(MAX_TRACK_KEYS)),
        )
        .collect();
    let i = import_selection(folder(&skin(&on, &fillers, 1, TITLE, "")).path()).unwrap();
    assert_eq!(i.report.usage.keys as usize, MAX_SKIN_KEYS);
    // One key more on the placement: refused where the song list is read.
    let two = format!(
        "{}, {}",
        r#"{"time": 0, "x": 0, "y": 900, "w": 300, "h": 40}"#,
        r#"{"time": 1, "x": 0, "y": 900, "w": 300, "h": 40}"#
    );
    let body = skin(&two, &fillers, 1, TITLE, "");
    let (l, line) = limit(import_selection(folder(&body).path()));
    assert_eq!(l, ImportLimit::Keys);
    // Located at the placement, on the `songlist` member's line.
    let json = common::selection::json(&body);
    let songlist = json
        .lines()
        .position(|l| l.contains("\"songlist\""))
        .unwrap();
    assert_eq!(line, u32::try_from(songlist + 1).ok());
}

/// An inactive arm that cannot be reproduced is a hidden copy of the
/// active arm, its keys charged as the model counts them: at the key cap
/// the import and the model both accept the skin.
#[test]
fn a_hidden_placement_s_keys_count_to_the_cap() {
    let on = format!(
        "{}, {}, {}",
        r#"{"time": 0, "x": 0, "y": 900, "w": 300, "h": 40}"#,
        r#"{"time": 1, "x": 0, "y": 900, "w": 300, "h": 40}"#,
        r#"{"time": 2, "x": 0, "y": 900, "w": 300, "h": 40}"#
    );
    let off = r#"{"x": 0, "y": 900, "w": 300, "h": 40}], "stretch": 1, "dummy": [0"#;
    let body = |fillers: &[String]| {
        let list = song_list(&[&on], &[off], 0, TITLE);
        let mut dsts: Vec<String> = fillers.to_vec();
        dsts.push(r#"{"id": "list", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#.to_owned());
        format!(
            r#""imageset": [{SET}], "text": [{TEXT_T}], {list}, "destination": [{}]"#,
            dsts.join(",")
        )
    };
    let base = import_selection(folder(&body(&[])).path())
        .unwrap()
        .report
        .usage
        .keys as usize;
    let room = MAX_SKIN_KEYS - base;
    let fillers: Vec<String> = (0..room / MAX_TRACK_KEYS)
        .map(|_| filler(MAX_TRACK_KEYS))
        .chain(
            [filler(room % MAX_TRACK_KEYS)]
                .into_iter()
                .filter(|_| !room.is_multiple_of(MAX_TRACK_KEYS)),
        )
        .collect();
    let i = import_selection(folder(&body(&fillers)).path()).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(i.report.usage.keys as usize, MAX_SKIN_KEYS);
}

/// Two song list destinations: one definition, so one row text table,
/// each charged its own quads and its parts' visibilities once per slot.
#[test]
fn two_song_list_destinations_are_each_charged() {
    let on = r#"{"x": 0, "y": 900, "w": 300, "h": 40}], "op": [2], "dummy": [0"#;
    let rest = format!(
        r#""text": [{{"id": "t", "op": [-1], "dst": [{}]}}]"#,
        at(0, 0, 100, 10)
    );
    let one = import_selection(folder(&skin(on, &[], 1, &rest, "")).path()).unwrap();
    let two = import_selection(folder(&skin(on, &[], 2, &rest, "")).path()).unwrap();
    let (u1, u2) = (one.report.usage, two.report.usage);
    // A slot pushes its bar, its text, its lamp, no level cells and its
    // three labels: six quads.
    assert_eq!(u1.widget_quads, 6);
    assert_eq!(u2.widget_quads, 12);
    // Each element charges the arm's op and the style's, once per slot.
    let per = u2.ops - u1.ops;
    assert!(per > 0);
    let three = import_selection(folder(&skin(on, &[], 3, &rest, "")).path()).unwrap();
    assert_eq!(three.report.usage.ops - u2.ops, per);
    assert_eq!(three.report.usage.widget_quads, 18);
    assert_eq!((u1.placements, u2.placements), (1, 1));
    assert_eq!(u2.elements, u1.elements + 1);
}

/// The widget quads of song list elements up to the cap, and one past.
#[test]
fn song_list_quads_count_to_the_cap() {
    // A level of two cells: eight quads a slot, one slot.
    let rest = format!(
        r#"{TITLE}, "level": [{{"id": "lv", "dst": [{}]}}]"#,
        at(0, 0, 4, 8)
    );
    let value = format!(
        r#""value": [{{"id": "lv", {}, "digit": 2, "ref": 96}}]"#,
        common::selection::DIGITS
    );
    let on = at(0, 900, 300, 40);
    let n = MAX_WIDGET_QUADS as usize / 8;
    let i = import_selection(folder(&skin(&on, &[], n, &rest, &value)).path()).unwrap();
    assert_eq!(i.report.usage.widget_quads, MAX_WIDGET_QUADS);
    let (l, _) = limit(import_selection(
        folder(&skin(&on, &[], n + 1, &rest, &value)).path(),
    ));
    assert_eq!(l, ImportLimit::WidgetQuads);
}

/// A placement's image set is charged its entries before any is looked
/// up or admitted: past the cap, refused with no image admitted.
#[test]
fn image_set_entries_are_charged_before_they_are_read() {
    let on = at(0, 900, 300, 40);
    let names = vec!["\"b0\""; MAX_IMAGE_CHOICE_REFS + 1].join(",");
    let body = skin(&on, &[], 1, TITLE, "")
        .replace(SET, &format!(r#"{{"id": "set", "images": [{names}]}}"#));
    let (l, _) = limit(import_selection(folder(&body).path()));
    assert_eq!(l, ImportLimit::ChoiceRefs);
    let names = vec!["\"b0\""; MAX_IMAGE_CHOICE_REFS / 2].join(",");
    let body = skin(&on, &[], 1, TITLE, "")
        .replace(SET, &format!(r#"{{"id": "set", "images": [{names}]}}"#));
    let i = import_selection(folder(&body).path()).unwrap();
    // Both arms charged.
    assert_eq!(i.report.usage.choice_refs as usize, MAX_IMAGE_CHOICE_REFS);
}

/// A bitmap row text charges a field's glyphs a slot: song list elements
/// of one slot (its bar, 256 glyphs, its lamp and three labels) and
/// numbers filling the rest reach the widget quad cap exactly, and one
/// cell more refuses.
#[test]
fn bitmap_row_text_quads_count_to_the_cap() {
    let per_list = 2 + u32::try_from(skin_model::MAX_TEXT_BYTES).unwrap() + 3;
    let lists = MAX_WIDGET_QUADS / per_list;
    let rest = MAX_WIDGET_QUADS - lists * per_list;
    let body = |extra: u32| {
        let total = rest + extra;
        let mut cells = vec![11; usize::try_from(total / 11).unwrap()];
        if !total.is_multiple_of(11) {
            cells.push(total % 11);
        }
        let values: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    r#"{{"id": "n{i}", {}, "digit": {c}, "ref": 71}}"#,
                    common::selection::DIGITS
                )
            })
            .collect();
        let fillers: Vec<String> = (0..cells.len())
            .map(|i| format!(r#"{{"id": "n{i}", "dst": [{}]}}"#, at(0, 0, 4, 8)))
            .collect();
        let text = format!(r#""text": [{{"id": "tb", "dst": [{}]}}]"#, at(0, 0, 100, 4));
        let more = format!(
            r#""value": [{}], "font": [{{"id": "b", "path": "b.fnt"}}]"#,
            values.join(",")
        );
        skin(
            &at(0, 900, 300, 40),
            &fillers,
            usize::try_from(lists).unwrap(),
            &text,
            &more,
        )
        .replace(TEXT_T, r#"{"id": "tb", "font": "b", "size": 4, "ref": 10}"#)
    };
    let run = |extra: u32| {
        let d = folder(&body(extra));
        common::big_fnt(d.path(), "b.fnt", 4, 0, 0);
        import_selection(d.path())
    };
    let i = run(0).unwrap();
    assert_eq!(i.report.usage.widget_quads, MAX_WIDGET_QUADS);
    assert_eq!(i.report.usage.fonts, 1);
    let (l, _) = limit(run(1));
    assert_eq!(l, ImportLimit::WidgetQuads);
}
