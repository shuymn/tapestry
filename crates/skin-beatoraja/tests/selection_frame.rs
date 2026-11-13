//! Selection skins through the production path: `import` into the
//! selection slot, then `prepare_selection`, `update_selection` and
//! `build_selection`, each frame's quads asserted exactly (docs/stage3.md
//! S10-09d).

mod common;

use std::collections::BTreeMap;

use bms_model::TimeUs;
use common::selection::{
    B0, B1, B5, DIGITS, DOT, LABEL, LAMP, SET, Sel, TEXT_T, TITLE, all_row, best, chart, digit_of,
    features, folder, folder_row, import_selection, json, labels, minimal, rect, song_list,
    summary, view,
};
use skin_model::{OwnLamp, RowScore, SelectionLabels, SelectionPlace};

/// Placement `dst` keys at `x`, `y` (source y up), `w`, `h`.
fn at(x: i32, y: i32, w: i32, h: i32) -> String {
    format!(r#"{{"x": {x}, "y": {y}, "w": {w}, "h": {h}}}"#)
}

/// A skin with `list` (a `"songlist"` member), the image set, the title
/// text, the list drawn once, and `more` members.
fn with_list(list: &str, more: &str) -> String {
    let more = if more.is_empty() {
        String::new()
    } else {
        format!(", {more}")
    };
    format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}], {list},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]{more}"#
    )
}

/// C4: each part draws at its sampled offset rect moved by its
/// placement's sampled bottom-left corner, `bar_h − part_y − part_h`
/// below the bar's top, at three times of a moving placement.
#[test]
fn parts_anchor_at_their_placement_s_bottom_left() {
    // Slot 0, the center, moves from y 500 (h 40) to y 600 (h 80) over a
    // second; slot 1 stands at y 400, h 60. The lamp sits 5 right and 3 up
    // from the bar's bottom-left, 8×16.
    let on0 = format!(
        "{}, {{\"time\": 1000, \"x\": 100, \"y\": 600, \"w\": 300, \"h\": 80}}",
        r#"{"time": 0, "x": 100, "y": 500, "w": 300, "h": 40}"#
    );
    let list = song_list(
        &[&on0, &at(120, 400, 300, 60)],
        &[&at(100, 500, 300, 40), &at(120, 400, 300, 60)],
        0,
        &format!(
            r#"{TITLE}, "lamp": [{{"id": "lamp", "dst": [{}]}}]"#,
            at(5, 3, 8, 16)
        ),
    );
    let d = folder(&with_list(&list, ""));
    let mut s = Sel::of(d.path());
    s.view(&view(vec![chart("a"), chart("b")], Some(0), None, None));
    for (ms, y, h) in [(0, 500, 40), (250, 525, 50), (500, 550, 60)] {
        // The bar's top, y down, is 1080 − (y + h); its bottom 1080 − y.
        let bar0 = rect([100, 1080 - (y + h), 300, h]);
        let lamp0 = rect([105, 1080 - y - (3 + 16), 8, 16]);
        let bar1 = rect([120, 1080 - 460, 300, 60]);
        let lamp1 = rect([125, 1080 - 400 - (3 + 16), 8, 16]);
        assert_eq!(
            s.drawn(ms),
            [(B0, bar0), (B0, bar1), (LAMP, lamp0), (LAMP, lamp1)],
            "{ms} ms"
        );
    }
}

/// The row styles of `text`, by height: style `k` is `10 + k` high.
fn styles(indices: &[usize]) -> (String, String) {
    let mut entries = Vec::new();
    let mut texts = Vec::new();
    for k in 0..=indices.iter().copied().max().unwrap_or(0) {
        if indices.contains(&k) {
            entries.push(format!(
                r#"{{"id": "t{k}", "dst": [{}]}}"#,
                at(0, 0, 100, 10 + k as i32)
            ));
            texts.push(format!(r#"{{"id": "t{k}", "font": "f", "ref": 10}}"#));
        } else {
            entries.push(r#"{"id": "none", "dst": []}"#.to_owned());
        }
    }
    (
        format!(r#""text": [{}]"#, entries.join(",")),
        texts.join(","),
    )
}

/// C5: a chart row takes text style 2, else 0; a folder row 4, else 0;
/// the "all charts" row 9, else 4, else 0, and bar image 5; a bar image
/// past the set's end is image 0, as the source's (`SkinImage.java:133-135`).
#[test]
fn row_kinds_take_their_style_chains() {
    let rows = || vec![chart("c"), folder_row("f", 3), all_row(9)];
    for (indices, want) in [
        (&[0][..], [10.0, 10.0, 10.0]),
        (&[0, 2][..], [12.0, 10.0, 10.0]),
        (&[0, 2, 4][..], [12.0, 14.0, 14.0]),
        (&[0, 2, 4, 9][..], [12.0, 14.0, 19.0]),
    ] {
        let (entries, texts) = styles(indices);
        let place = [
            at(0, 900, 300, 40),
            at(0, 800, 300, 40),
            at(0, 700, 300, 40),
        ];
        let refs: Vec<&str> = place.iter().map(String::as_str).collect();
        let list = song_list(&refs, &refs, 1, &entries);
        let body = format!(
            r#""imageset": [{SET}], "text": [{texts}], {list},
               "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]"#
        );
        let d = folder(&body);
        let mut s = Sel::of(d.path());
        s.view(&view(rows(), Some(1), None, None));
        let (_, rows_drawn) = s.requests();
        let ems: Vec<f32> = rows_drawn.iter().map(|r| r.2).collect();
        assert_eq!(ems, want, "{indices:?}");
        // The bars: the chart's image 0, the folder's 1, the "all
        // charts" row's 5.
        assert_eq!(s.images(0), [B0, B1, B5], "{indices:?}");
    }
    // Image sets of 2 and of 1 image: the "all charts" row falls back to
    // image 0, and a folder in a set of 1.
    for (set, want) in [
        (r#"["b0", "b1"]"#, [B0, B1, B0]),
        (r#"["b0"]"#, [B0, B0, B0]),
    ] {
        let place = [
            at(0, 900, 300, 40),
            at(0, 800, 300, 40),
            at(0, 700, 300, 40),
        ];
        let refs: Vec<&str> = place.iter().map(String::as_str).collect();
        let list = song_list(&refs, &refs, 1, TITLE);
        let body = format!(
            r#""imageset": [{{"id": "set", "images": {set}}}], "text": [{TEXT_T}], {list},
               "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]"#
        );
        let d = folder(&body);
        let mut s = Sel::of(d.path());
        s.view(&view(rows(), Some(1), None, None));
        assert_eq!(s.images(0), want, "{set}");
    }
}

/// C13 (importer half): the row text and text 10–16, 30 and 1000 are the
/// view's strings and the given labels', with no literal of the
/// importer's own; a non-empty subtitle joins with one space.
#[test]
fn texts_show_the_view_and_the_labels() {
    let texts: Vec<String> = [10, 11, 12, 13, 16, 30, 1000]
        .iter()
        .map(|r| format!(r#"{{"id": "x{r}", "font": "f", "ref": {r}}}"#))
        .collect();
    let dsts: Vec<String> = [10, 11, 12, 13, 16, 30, 1000]
        .iter()
        .enumerate()
        .map(|(i, r)| {
            format!(
                r#"{{"id": "x{r}", "dst": [{}]}}"#,
                at(0, 20 * i as i32, 300, 10)
            )
        })
        .collect();
    let list = song_list(
        &[&at(0, 900, 300, 40), &at(0, 800, 300, 40)],
        &[&at(0, 900, 300, 40), &at(0, 800, 300, 40)],
        0,
        TITLE,
    );
    let body = format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}, {}], {list},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}, {}]"#,
        texts.join(","),
        dsts.join(",")
    );
    let d = folder(&body);
    let mut s = Sel::of(d.path());
    let mut a = chart("T");
    a.subtitle = "S".to_owned();
    let mut v = view(vec![a, folder_row("F", 2)], Some(0), None, None);
    if let Some(c) = &mut v.selected {
        c.labels.genre = "G".to_owned();
        c.labels.artist = "A".to_owned();
        c.labels.subartist = "B".to_owned();
    }
    v.query = "Q".to_owned();
    v.place = SelectionPlace::Folder(vec!["X".to_owned(), "Y".to_owned()]);
    s.view(&v);
    let (texts, rows) = s.requests();
    let strings: Vec<&str> = texts.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(strings, ["T", "S", "T S", "G", "A B", "Q", "X > Y > "]);
    let rows: Vec<&str> = rows.iter().map(|(_, t, _)| t.as_str()).collect();
    assert_eq!(rows, ["T S", "F"]);
    // A folder under the cursor, the "all charts" row, each place, two
    // label sets.
    for (labels, all, found) in [
        (labels(), "ALL", "FOUND"),
        (
            SelectionLabels {
                all_charts: "every".to_owned(),
                search_results: "hits".to_owned(),
            },
            "every",
            "hits",
        ),
    ] {
        let mut v = view(vec![all_row(4), folder_row("F", 2)], Some(0), None, None);
        v.place = SelectionPlace::All;
        s.skin.update_selection(&mut s.sel, &v, &labels).unwrap();
        let (texts, rows) = s.requests();
        let strings: Vec<&str> = texts.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(strings, [all, all, format!("{all} > ").as_str()]);
        let rows: Vec<&str> = rows.iter().map(|(_, t, _)| t.as_str()).collect();
        assert_eq!(rows, [all, "F"]);
        v.place = SelectionPlace::Results;
        v.cursor = Some(1);
        s.skin.update_selection(&mut s.sel, &v, &labels).unwrap();
        let (texts, _) = s.requests();
        let strings: Vec<&str> = texts.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(strings, ["F", "F", format!("{found} > ").as_str()]);
    }
    v.place = SelectionPlace::Top;
    s.view(&v);
    let (texts, _) = s.requests();
    assert!(texts.iter().all(|(_, t)| t != " > "));
}

/// The numbers of the C24 fixture, by the id each shows: one element a
/// row, `y` its row.
const NUMBERS: [i32; 20] = [
    71, 72, 74, 76, 77, 90, 91, 96, 102, 103, 150, 154, 300, 1163, 1164, 350, 351, 352, 353, 354,
];

/// The options of the C24 fixture, each drawing `dot` on its own row.
const OPTIONS: [i32; 58] = [
    1, 2, 5, 21, 22, 23, 100, 101, 102, 103, 104, 105, 1100, 1101, 1102, 1103, 1104, 160, 161, 162,
    163, 164, 176, 177, 200, 201, 202, 203, 204, 205, 206, 207, 300, 301, 302, 303, 304, 305, 306,
    307, 1160, 1161, 50, 624, 51, 625, 3, 60, 197, 1177, 1002, 1205, -2, -1, -50, -624, -51, -625,
];

/// The C24 fixture: a number per id of [`NUMBERS`] on row `i` (y up
/// `10 · i`), an option image per id of [`OPTIONS`] at x `10 · i`, and the
/// clear type group (370) at x 1000.
fn profile_skin() -> String {
    let values: Vec<String> = NUMBERS
        .iter()
        .map(|id| format!(r#"{{"id": "n{id}", {DIGITS}, "digit": 5, "ref": {id}}}"#))
        .collect();
    let mut dsts: Vec<String> = NUMBERS
        .iter()
        .enumerate()
        .map(|(i, id)| {
            format!(
                r#"{{"id": "n{id}", "dst": [{}]}}"#,
                at(0, 10 * i as i32, 4, 8)
            )
        })
        .collect();
    dsts.extend(OPTIONS.iter().enumerate().map(|(i, op)| {
        format!(
            r#"{{"id": "dot", "op": [{op}], "dst": [{}]}}"#,
            at(10 * i as i32, 1000, 8, 8)
        )
    }));
    dsts.push(format!(
        r#"{{"id": "ct", "dst": [{}]}}"#,
        at(1000, 900, 8, 8)
    ));
    let list = song_list(&[&at(0, 900, 300, 40)], &[&at(0, 900, 300, 40)], 0, TITLE);
    format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}], "value": [{}], {list},
           "image": [{}, {{"id": "ct", "src": "s", "x": 0, "y": 64, "w": 88, "h": 8, "divx": 11, "len": 11, "ref": 370}}],
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}, {}]"#,
        values.join(","),
        common::selection::IMAGES,
        dsts.join(",")
    )
}

/// What a C24 frame shows: each number's digits by its id, the options
/// whose image draws, and the clear type group's frame.
fn profile_frame(s: &mut Sel) -> (BTreeMap<i32, String>, Vec<i32>, Option<u8>) {
    let mut numbers: BTreeMap<i32, String> = BTreeMap::new();
    let mut options = Vec::new();
    let mut clear = None;
    for (t, r) in s.drawn(0) {
        if let Some(d) = digit_of(t) {
            // Row `i` lies at y down 1080 − (10 i + 8).
            let i = ((1072.0 - r[1]) / 10.0) as usize;
            numbers
                .entry(NUMBERS[i])
                .or_default()
                .push(char::from(b'0' + d));
        } else if t == DOT {
            options.push(OPTIONS[(r[0] / 10.0) as usize]);
        } else if t[1] == 68 {
            clear = Some((t[0] - 4) / 8);
        }
    }
    (numbers, options, clear)
}

/// C24: each needed id draws its value from the view, each decorative
/// constant its truthful constant, and each omitted id nothing.
#[test]
fn the_profile_draws_the_view_through_the_production_path() {
    let d = folder(&profile_skin());
    let i = import_selection(d.path()).unwrap();
    let mut s = Sel::new(i.skin);
    // A chart with a best of EX 100 of 150, 5 BP and 7 plays, cleared on a
    // normal gauge; 60 taps and 15 long notes over 125 s at 120 to 180
    // BPM; level 12.
    let mut row = chart("a");
    row.score = best(100, 5, 7);
    row.level_value = 12;
    let one_each = summary(60, 15, 125, 120, 180);
    s.view(&view(vec![row], Some(0), Some(150), Some(one_each)));
    let (numbers, options, clear) = profile_frame(&mut s);
    let want: BTreeMap<i32, String> = [
        (71, "100"),
        (72, "150"),
        (74, "75"),
        (76, "5"),
        (77, "7"),
        (90, "180"),
        (91, "120"),
        (96, "12"),
        // ⌊100 · 100 / 150⌋ and ⌊10 000 · 100 / 150⌋ mod 100.
        (102, "66"),
        (103, "66"),
        (150, "100"),
        // ⌈21 · 150 / 27⌉ − 100.
        (154, "17"),
        // 125 s: 2 min 5 s.
        (1163, "2"),
        (1164, "5"),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_owned()))
    .collect();
    assert_eq!(numbers, want);
    // 27 · 100 ≥ 18 · 150: rank A (202 and 302); the normal clear (103);
    // a 7-key chart whose tempo changes; offline and no rival.
    assert_eq!(
        options,
        [2, 5, 103, 160, 177, 202, 302, 50, 624, -1, -51, -625]
    );
    // The normal gauge's clear: the source's clear type 5.
    assert_eq!(clear, Some(5));
    // A folder row: its charts, and no chart, score or lamp.
    s.view(&view(vec![folder_row("f", 9)], Some(0), None, None));
    let (numbers, options, clear) = profile_frame(&mut s);
    assert_eq!(numbers, BTreeMap::from([(300, "9".to_owned())]));
    assert_eq!(options, [1, 50, 624, -2, -51, -625]);
    assert_eq!(clear, None);
    // An unplayed chart: no clear type; a no-play lamp.
    s.view(&view(vec![chart("u")], Some(0), None, None));
    let (_, options, clear) = profile_frame(&mut s);
    assert_eq!(clear, None);
    assert!(options.contains(&100));
}

/// C11: with `max_ex` 0 no rate, rank or next rank draws; 72 draws 0.
#[test]
fn a_zero_maximum_draws_no_rate() {
    let d = folder(&profile_skin());
    let mut s = Sel::of(d.path());
    let mut row = chart("a");
    row.score = best(0, 0, 1);
    s.view(&view(vec![row], Some(0), Some(0), None));
    let (numbers, options, _) = profile_frame(&mut s);
    assert_eq!(
        numbers,
        BTreeMap::from([
            (71, "0".to_owned()),
            (72, "0".to_owned()),
            (76, "0".to_owned()),
            (77, "1".to_owned()),
            (96, "0".to_owned()),
            (150, "0".to_owned()),
        ])
    );
    assert!(
        !options
            .iter()
            .any(|o| (200..=207).contains(o) || (300..=307).contains(o))
    );
}

/// C22: timers 11 and 21–23 start from the frame; option N holds while
/// panel N is open and −N while it is closed.
#[test]
fn panels_and_the_change_timer_follow_the_frame() {
    let dsts: Vec<String> = [21, -21, 22, -22, 23, -23]
        .iter()
        .enumerate()
        .map(|(i, op)| {
            format!(
                r#"{{"id": "dot", "op": [{op}], "dst": [{}]}}"#,
                at(10 * i as i32, 1000, 8, 8)
            )
        })
        .chain([
            r#"{"id": "lab", "timer": 11, "dst": [{"time": 0, "x": 0, "y": 0, "w": 8, "h": 8}]}"#
                .to_owned(),
            // Drawn from 100 ms after the change.
            r#"{"id": "lamp", "timer": 11, "loop": 100,
                "dst": [{"time": 100, "x": 0, "y": 0, "w": 8, "h": 8}]}"#
                .to_owned(),
        ])
        .collect();
    let d = folder(&minimal(&format!(
        r#""destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}, {}]"#,
        dsts.join(",")
    )));
    let mut s = Sel::of(d.path());
    s.view(&view(vec![chart("a")], Some(0), None, None));
    let shown = |s: &mut Sel, ms| -> Vec<i32> {
        s.drawn(ms)
            .into_iter()
            .filter(|(t, _)| *t == DOT)
            .map(|(_, r)| [21, -21, 22, -22, 23, -23][(r[0] / 10.0) as usize])
            .collect()
    };
    assert_eq!(shown(&mut s, 0), [-21, -22, -23]);
    for n in 0..3 {
        s.timers.panels = [None; 3];
        s.timers.panels[n] = Some(TimeUs(0));
        let mut want: Vec<i32> = [-21, -22, -23].to_vec();
        want[n] = 21 + n as i32;
        assert_eq!(shown(&mut s, 0), want, "panel {}", n + 1);
    }
    // The change-timed label: off before its start, then drawn at its
    // start plus its key.
    let label = |s: &mut Sel, ms| s.images(ms).contains(&LABEL);
    let late = |s: &mut Sel, ms| s.images(ms).contains(&LAMP);
    assert!(!label(&mut s, 500) && !late(&mut s, 500));
    // A change at 200 ms: before it, the timer reads as just started (a
    // start after `now` ages to 0), so the label at key 0 draws and the
    // one at key 100 does not; from its start plus 100 ms it does.
    s.timers.change = Some(TimeUs(200_000));
    assert!(label(&mut s, 150) && !late(&mut s, 150));
    assert!(label(&mut s, 200) && !late(&mut s, 299));
    assert!(late(&mut s, 300));
}

/// C1 (importer half): a play timer and a LIFT offset on a selection skin
/// are undefined there: the timer's element draws nothing with its note,
/// the offset is ignored with its note.
#[test]
fn play_timers_and_offsets_are_undefined_on_selection() {
    let d = folder(&minimal(
        r#""destination": [{"id": "list", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]},
             {"id": "dot", "timer": 41, "dst": [{"x": 0, "y": 0, "w": 8, "h": 8}]},
             {"id": "lab", "offset": 3, "dst": [{"x": 0, "y": 0, "w": 8, "h": 8}]}]"#,
    ));
    let i = import_selection(d.path()).unwrap();
    let kinds = common::kinds(&i.report);
    assert!(kinds.contains(&skin_beatoraja::NoteKind::Timer(
        skin_beatoraja::TimerWhy::Unsupported(41)
    )));
    assert!(kinds.contains(&skin_beatoraja::NoteKind::RuntimeOffset(3)));
    let mut s = Sel::new(i.skin);
    s.view(&view(vec![chart("a")], Some(0), None, None));
    assert_eq!(s.images(0), [B0, LABEL]);
}

/// The lamp of each score: the model's lamp, through the source's lamp
/// index (D7), drawn from the typed own lamp.
#[test]
fn lamps_follow_the_typed_own_lamp() {
    let lamps: Vec<String> = (0..11)
        .map(|k| format!(r#"{{"id": "lamp", "dst": [{}]}}"#, at(10 * k, 0, 8, 8)))
        .collect();
    let list = song_list(
        &[&at(0, 500, 300, 40)],
        &[&at(0, 500, 300, 40)],
        0,
        &format!(r#"{TITLE}, "lamp": [{}]"#, lamps.join(",")),
    );
    let d = folder(&with_list(&list, ""));
    let i = import_selection(d.path()).unwrap();
    // Indices 2, 9 and 10 are never met: noted.
    let ignored = i
        .report
        .notes
        .iter()
        .filter(|n| n.kind == skin_beatoraja::NoteKind::Ignored { field: "lamp" })
        .count();
    assert_eq!(ignored, 3);
    let mut s = Sel::new(i.skin);
    for (score, index) in [
        (RowScore::Unplayed, 0),
        (RowScore::NoBest { plays: 1 }, 0),
        (
            RowScore::Best {
                plays: 1,
                ex: 0,
                bp: 0,
                lamp: OwnLamp::Failed,
            },
            1,
        ),
        (best(1, 0, 1), 5),
        (
            RowScore::Best {
                plays: 1,
                ex: 0,
                bp: 0,
                lamp: OwnLamp::FullCombo,
            },
            8,
        ),
    ] {
        let mut row = chart("a");
        row.score = score;
        s.view(&view(vec![row], Some(0), None, None));
        let lamp: Vec<f32> = s
            .drawn(0)
            .into_iter()
            .filter(|(t, _)| *t == LAMP)
            .map(|(_, r)| r[0] / 10.0)
            .collect();
        assert_eq!(lamp, [index as f32], "{score:?}");
    }
}

/// C8: the labels draw long notes, random and mines from the row, nothing
/// for an unknown fact.
#[test]
fn labels_follow_the_row_s_features() {
    let labels: Vec<String> = (0..3)
        .map(|k| format!(r#"{{"id": "lab", "dst": [{}]}}"#, at(10 * k, 0, 8, 8)))
        .collect();
    let list = song_list(
        &[&at(0, 500, 300, 40)],
        &[&at(0, 500, 300, 40)],
        0,
        &format!(r#"{TITLE}, "label": [{}]"#, labels.join(",")),
    );
    let d = folder(&with_list(&list, ""));
    let mut s = Sel::of(d.path());
    for (f, random, want) in [
        (None, false, vec![]),
        (features(true, false), false, vec![0.0]),
        (features(false, true), false, vec![2.0]),
        (None, true, vec![1.0]),
        (features(true, true), true, vec![0.0, 2.0, 1.0]),
    ] {
        let mut row = chart("a");
        row.features = f;
        row.random = random;
        s.view(&view(vec![row], Some(0), None, None));
        let got: Vec<f32> = s
            .drawn(0)
            .into_iter()
            .filter(|(t, _)| *t == LABEL)
            .map(|(_, r)| r[0] / 10.0)
            .collect();
        assert_eq!(got, want, "{f:?} {random}");
    }
}

/// A level's strip follows the selection loader
/// (`JsonSelectSkinObjectLoader.java:181-196`): a ten-glyph strip pads
/// with blanks whatever its `padding`, and its digit offsets are unread.
#[test]
fn level_strips_follow_the_selection_loader() {
    let list = song_list(
        &[&at(0, 500, 300, 40)],
        &[&at(0, 500, 300, 40)],
        0,
        &format!(
            r#"{TITLE}, "level": [{{"id": "lv", "dst": [{}]}}]"#,
            at(0, 0, 4, 8)
        ),
    );
    let d = folder(&with_list(
        &list,
        &format!(
            r#""value": [{{"id": "lv", {DIGITS}, "digit": 2, "padding": 1, "ref": 96,
                "offset": [{{"x": 1}}, {{"x": 1}}]}}]"#
        ),
    ));
    let i = import_selection(d.path()).unwrap();
    assert_eq!(i.report.usage.digit_offsets, 0);
    let mut s = Sel::new(i.skin);
    let mut row = chart("a");
    row.level_value = 7;
    s.view(&view(vec![row.clone()], Some(0), None, None));
    assert_eq!(s.digits(0), "7");
    // A stretch that keeps the glyph's box and an offset that would change
    // its rows: the offsets unread, the level still draws.
    let level = |value: &str, dst: &str| {
        let list = song_list(
            &[&at(0, 500, 300, 40)],
            &[&at(0, 500, 300, 40)],
            0,
            &format!(r#"{TITLE}, "level": [{{"id": "lv", {dst}}}]"#),
        );
        folder(&with_list(&list, &format!(r#""value": [{value}]"#)))
    };
    let d = level(
        &format!(r#"{{"id": "lv", {DIGITS}, "digit": 1, "ref": 96, "offset": [{{"w": 1}}]}}"#),
        &format!(r#""stretch": 1, "dst": [{}]"#, at(0, 0, 4, 8)),
    );
    let mut s = Sel::of(d.path());
    s.view(&view(vec![row.clone()], Some(0), None, None));
    assert_eq!(s.digits(0), "7");
    // An eleven-glyph strip pads with its eleventh glyph, whatever its
    // `zeropadding`.
    let d = level(
        r#"{"id": "lv", "src": "s", "x": 0, "y": 32, "w": 44, "h": 8, "divx": 11, "digit": 2,
            "zeropadding": 0, "ref": 96}"#,
        &format!(r#""dst": [{}]"#, at(0, 0, 4, 8)),
    );
    let mut s = Sel::of(d.path());
    s.view(&view(vec![row], Some(0), None, None));
    let glyphs: Vec<[u8; 2]> = s.images(0).into_iter().filter(|t| t[1] == 36).collect();
    assert_eq!(glyphs, [[42, 36], [30, 36]]);
}

/// C7: a chart row draws its level in its difficulty's style; the
/// seventh source style is dropped with a note.
#[test]
fn levels_draw_in_their_difficulty_s_style() {
    let levels: Vec<String> = (0..7)
        .map(|k| format!(r#"{{"id": "lv", "dst": [{}]}}"#, at(100 * k, 0, 4, 8)))
        .collect();
    let list = song_list(
        &[&at(0, 500, 300, 40)],
        &[&at(0, 500, 300, 40)],
        0,
        &format!(r#"{TITLE}, "level": [{}]"#, levels.join(",")),
    );
    let d = folder(&with_list(
        &list,
        &format!(r#""value": [{{"id": "lv", {DIGITS}, "digit": 2, "ref": 96}}]"#),
    ));
    let i = import_selection(d.path()).unwrap();
    assert!(common::has(
        &i.report,
        skin_beatoraja::NoteKind::Ignored { field: "level" }
    ));
    let mut s = Sel::new(i.skin);
    for (difficulty, value, style) in [(None, 12, 0), (Some(1), 7, 1), (Some(5), -3, 5)] {
        let mut row = chart("a");
        row.level_value = value;
        row.difficulty = difficulty;
        s.view(&view(vec![row], Some(0), None, None));
        let digits: Vec<(f32, u8)> = s
            .drawn(0)
            .into_iter()
            .filter_map(|(t, r)| digit_of(t).map(|d| (r[0], d)))
            .collect();
        let first = digits.first().map_or(-1.0, |d| (d.0 / 100.0).floor());
        assert_eq!(first, style as f32, "{value}");
    }
    // A folder row draws no level.
    s.view(&view(vec![folder_row("f", 1)], Some(0), None, None));
    assert!(s.images(0).iter().all(|t| digit_of(*t).is_none()));
}

/// C21: placements past 60 are ignored with a note.
#[test]
fn placements_past_sixty_are_ignored() {
    let place: Vec<String> = (0..61).map(|i| at(0, 10 * i, 300, 8)).collect();
    let refs: Vec<&str> = place.iter().map(String::as_str).collect();
    let list = song_list(&refs, &refs, 0, TITLE);
    let d = folder(&with_list(&list, ""));
    let i = import_selection(d.path()).unwrap();
    assert!(common::has(
        &i.report,
        skin_beatoraja::NoteKind::Ignored { field: "liston" }
    ));
    assert_eq!(i.report.usage.placements, 60);
    let rows: Vec<_> = (0..61).map(|k| chart(&format!("{k}"))).collect();
    let mut s = Sel::new(i.skin);
    s.view(&view(rows, Some(0), None, None));
    assert_eq!(s.images(0).len(), 60);
    let _ = json;
}

/// A negative display offset draws number 12 negative, through the
/// production path: with a strip of 24 glyphs (4×8 from row 40, glyph `k`
/// at column `4k`), the negative half's digits after its sign, as play's
/// signed numbers do.
#[test]
fn a_negative_display_offset_draws_number_12_negative() {
    let d = folder(&minimal(
        r#""value": [{"id": "n", "src": "s", "x": 0, "y": 40, "w": 96, "h": 8, "divx": 24, "digit": 5, "ref": 12}],
           "destination": [{"id": "list", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]},
               {"id": "n", "dst": [{"x": 0, "y": 0, "w": 4, "h": 8}]}]"#,
    ));
    let mut s = Sel::of(d.path());
    s.view(&view(vec![chart("a")], Some(0), None, None));
    let glyphs = |s: &mut Sel| -> Vec<u8> {
        s.images(0)
            .into_iter()
            .filter(|t| t[1] == 44)
            .map(|t| (t[0] - 2) / 4)
            .collect()
    };
    // The negative half is glyphs 12 to 23, its sign the last; a positive
    // value takes the first half's "+" (11).
    for (ms, want) in [(-37, vec![23, 15, 19]), (37, vec![11, 3, 7])] {
        let set = settings(
            ms,
            1.0,
            skin_model::SpeedView::Classic,
            skin_model::SelectionOrder::Title,
            None,
            [false; 3],
        );
        s.skin.update_selection_settings(&mut s.sel, &set).unwrap();
        assert_eq!(glyphs(&mut s), want, "{ms} ms");
    }
}

/// The settings' numbers of the settings fixture, one a row.
const SETTING_NUMBERS: [i32; 4] = [12, 57, 312, 313];

/// The image indices of the settings fixture, each on its own column.
const SETTING_INDICES: [i32; 38] = [
    11, 12, 15, 16, 19, 40, 42, 43, 54, 55, 72, 75, 77, 78, 301, 302, 303, 304, 305, 306, 307, 308,
    315, 316, 317, 318, 330, 331, 332, 340, 341, 342, 1, 1, 1, 1, 1, 1,
];

/// The settings fixture: a number per id of [`SETTING_NUMBERS`] on row
/// `i` (y up `10 · i`), an image of eleven entries per id of
/// [`SETTING_INDICES`] at x `10 · i` (its first 32), the master volume's
/// slider (range 100 to the right, at y 600) and graph (100 wide, at
/// y 500).
fn settings_skin() -> String {
    let values: Vec<String> = SETTING_NUMBERS
        .iter()
        .map(|id| format!(r#"{{"id": "n{id}", {DIGITS}, "digit": 5, "ref": {id}}}"#))
        .collect();
    let images: Vec<String> = SETTING_INDICES[..32]
        .iter()
        .map(|id| {
            format!(
                r#"{{"id": "i{id}", "src": "s", "x": 0, "y": 64, "w": 88, "h": 8, "divx": 11, "len": 11, "ref": {id}}}"#
            )
        })
        .collect();
    let mut dsts: Vec<String> = SETTING_NUMBERS
        .iter()
        .enumerate()
        .map(|(i, id)| {
            format!(
                r#"{{"id": "n{id}", "dst": [{}]}}"#,
                at(0, 10 * i as i32, 4, 8)
            )
        })
        .collect();
    dsts.extend(SETTING_INDICES[..32].iter().enumerate().map(|(i, id)| {
        format!(
            r#"{{"id": "i{id}", "dst": [{}]}}"#,
            at(10 * i as i32, 800, 8, 8)
        )
    }));
    dsts.push(format!(
        r#"{{"id": "vs", "dst": [{}]}}"#,
        at(0, 600, 10, 10)
    ));
    dsts.push(format!(
        r#"{{"id": "vg", "dst": [{}]}}"#,
        at(0, 500, 100, 10)
    ));
    let list = song_list(&[&at(0, 900, 300, 40)], &[&at(0, 900, 300, 40)], 0, TITLE);
    format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}], "value": [{}], {list},
           "image": [{}, {}],
           "slider": [{{"id": "vs", "src": "s", "x": 40, "y": 0, "w": 8, "h": 8, "type": 17, "angle": 1, "range": 100}}],
           "graph": [{{"id": "vg", "src": "s", "x": 40, "y": 0, "w": 8, "h": 8, "type": 17, "angle": 0}}],
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}, {}]"#,
        values.join(","),
        common::selection::IMAGES,
        images.join(","),
        dsts.join(",")
    )
}

/// What a settings frame shows: each number's digits by its id, each
/// image index's entry by its id, and the volume slider's and graph's
/// rects.
struct SettingsFrame {
    numbers: BTreeMap<i32, String>,
    indices: BTreeMap<i32, u8>,
    slider: Option<[f32; 4]>,
    graph: Option<[f32; 4]>,
}

fn settings_frame(s: &mut Sel) -> SettingsFrame {
    let mut f = SettingsFrame {
        numbers: BTreeMap::new(),
        indices: BTreeMap::new(),
        slider: None,
        graph: None,
    };
    for (t, r) in s.drawn(0) {
        if let Some(d) = digit_of(t) {
            let i = ((1072.0 - r[1]) / 10.0) as usize;
            f.numbers
                .entry(SETTING_NUMBERS[i])
                .or_default()
                .push(char::from(b'0' + d));
        } else if t[1] == 68 {
            let id = SETTING_INDICES[(r[0] / 10.0) as usize];
            assert!(f.indices.insert(id, (t[0] - 4) / 8).is_none(), "{id}");
        } else if t[1] < 8 && (40..48).contains(&t[0]) {
            // y down: the slider's top at 1080 − 610, the graph's at 570.
            if r[1] == 470.0 {
                f.slider = Some(r);
            } else if r[1] == 570.0 {
                f.graph = Some(r);
            }
        }
    }
    f
}

/// The image indices the settings set: the order, the gauge, the fixed
/// hi-speed, the three covers' switches, the gauge auto shift, the judge
/// algorithm and the hi-speed auto adjustment.
const SETTABLE: [i32; 9] = [12, 40, 55, 330, 331, 332, 78, 340, 342];

/// Settings of `display_offset_ms`, `gain`, `speed`, `order`, `gauge` and
/// `covers_on`, a failed play closing and no preset's assignment.
fn settings(
    display_offset_ms: i32,
    gain: f32,
    speed: skin_model::SpeedView,
    order: skin_model::SelectionOrder,
    gauge: Option<skin_model::GaugeLook>,
    covers_on: [bool; 3],
) -> skin_model::SelectionSettings {
    skin_model::SelectionSettings {
        display_offset_ms,
        master_volume: skin_model::MasterVolume::of_gain(gain),
        speed,
        order,
        gauge,
        covers_on,
        play_to_end: false,
        assignment: None,
    }
}

/// C13: through the production path (`import`, `prepare_selection`,
/// `update_selection` and `update_selection_settings`, `build_selection`)
/// each bridged settings id draws the player's value, each constant its
/// constant and each unavailable id nothing, at two values of each
/// setting; 312, 313 and 55 in Classic and Floating, the duration
/// truncated; the volume's number,
/// slider and graph at the gains 0, 0.45 and 1; and a later view leaves
/// the settings as they were.
#[test]
fn the_settings_draw_the_players_values_through_the_production_path() {
    use skin_model::{GaugeLook, SelectionOrder, SpeedReference, SpeedView};
    let d = folder(&settings_skin());
    let i = import_selection(d.path()).unwrap();
    // Every settings id has a bridge or a constant: none is unavailable.
    let unavailable: Vec<i32> = common::kinds(&i.report)
        .into_iter()
        .filter_map(|k| match k {
            skin_beatoraja::NoteKind::Unavailable { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(unavailable, Vec::<i32>::new());
    let mut s = Sel::new(i.skin);
    s.view(&view(vec![chart("a")], Some(0), None, None));
    // Before any settings: no volume ratio, so neither bar draws.
    let f = settings_frame(&mut s);
    assert_eq!((f.slider, f.graph), (None, None));
    let constants = |f: &SettingsFrame| -> BTreeMap<i32, u8> {
        f.indices
            .iter()
            .filter(|(id, _)| !SETTABLE.contains(*id))
            .map(|(&id, &k)| (id, k))
            .collect()
    };
    // The truthful constants: 7-key and no BGA (2), every other 0.
    let mut want_constants: BTreeMap<i32, u8> = [
        11, 15, 16, 19, 42, 43, 54, 72, 75, 77, 301, 302, 303, 304, 305, 306, 307, 308, 315, 316,
        317, 318, 341,
    ]
    .into_iter()
    .map(|id| (id, 0))
    .collect();
    want_constants.insert(11, 2);
    want_constants.insert(72, 2);

    let a = settings(
        7,
        0.45,
        SpeedView::Classic,
        SelectionOrder::Title,
        Some(GaugeLook::Hard),
        [true, false, true],
    );
    s.skin.update_selection_settings(&mut s.sel, &a).unwrap();
    let f = settings_frame(&mut s);
    assert_eq!(
        f.numbers,
        BTreeMap::from([(12, "7".to_owned()), (57, "45".to_owned())]),
        "Classic: no duration"
    );
    assert_eq!(constants(&f), want_constants);
    let settable = |f: &SettingsFrame| SETTABLE.map(|id| f.indices.get(&id).copied());
    // On a chart row with no preset (340 draws nothing) and a failed play
    // closing (78 is 0).
    assert_eq!(
        settable(&f),
        [
            Some(0),
            Some(3),
            Some(0),
            Some(1),
            Some(0),
            Some(1),
            Some(0),
            None,
            Some(0)
        ]
    );
    // 0.45 as the f32 gain: 7 549 747 / 2^24 of the travel and width.
    let x = (100.0 * f64::from(7_549_747_u32) / f64::from(1_u32 << 24)) as f32;
    assert!((f.slider.unwrap()[0] - x).abs() < 1e-3, "{:?}", f.slider);
    assert!((f.graph.unwrap()[2] - x).abs() < 1e-3, "{:?}", f.graph);

    let b = settings(
        120,
        1.0,
        SpeedView::Floating {
            reference: SpeedReference::Minimum,
            green: 1_200,
        },
        SelectionOrder::Level,
        Some(GaugeLook::AssistEasy),
        [false, true, false],
    );
    s.skin.update_selection_settings(&mut s.sel, &b).unwrap();
    let f = settings_frame(&mut s);
    // ⌊5 · 1200 / 3⌋ ms and the green number.
    assert_eq!(
        f.numbers,
        BTreeMap::from([
            (12, "120".to_owned()),
            (57, "100".to_owned()),
            (312, "2000".to_owned()),
            (313, "1200".to_owned()),
        ])
    );
    assert_eq!(constants(&f), want_constants);
    assert_eq!(
        settable(&f),
        [
            Some(4),
            Some(0),
            Some(4),
            Some(0),
            Some(1),
            Some(0),
            Some(0),
            None,
            Some(1)
        ]
    );
    assert_eq!(f.slider.unwrap()[0], 100.0);
    assert_eq!(f.graph.unwrap()[2], 100.0);
    // A view keeps the settings.
    s.view(&view(vec![chart("b")], Some(0), None, None));
    assert_eq!(
        settings_frame(&mut s).numbers.get(&57).map(String::as_str),
        Some("100")
    );

    // Floating from the start tempo, the modification order (no sorter:
    // nothing), no gauge (nothing), a silent gain.
    let c = settings(
        0,
        0.0,
        SpeedView::Floating {
            reference: SpeedReference::Start,
            green: 300,
        },
        SelectionOrder::Modified,
        None,
        [false; 3],
    );
    s.skin.update_selection_settings(&mut s.sel, &c).unwrap();
    let f = settings_frame(&mut s);
    assert_eq!(
        f.numbers,
        BTreeMap::from([
            (12, "0".to_owned()),
            (57, "0".to_owned()),
            (312, "500".to_owned()),
            (313, "300".to_owned()),
        ])
    );
    assert_eq!(
        settable(&f),
        [
            None,
            None,
            Some(1),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            None,
            Some(1)
        ]
    );
    assert_eq!(f.slider.unwrap()[0], 0.0);

    // The duration truncates: 5 · 301 / 3 is 501.67, drawn 501.
    let d = settings(
        0,
        0.0,
        SpeedView::Floating {
            reference: SpeedReference::Start,
            green: 301,
        },
        SelectionOrder::Modified,
        None,
        [false; 3],
    );
    s.skin.update_selection_settings(&mut s.sel, &d).unwrap();
    let f = settings_frame(&mut s);
    assert_eq!(
        (f.numbers.get(&312), f.numbers.get(&313)),
        (Some(&"501".to_owned()), Some(&"301".to_owned()))
    );
}

/// The judgment class options on selection, in the shapes real skins use
/// (`op` 5 or 2 with one of 180 to 184) and bare: drawn only on a chart row
/// of their class; 180 not on a chart whose class is unknown, nor on a
/// folder row; a negated -181 on every row but a class-1 chart.
#[test]
fn the_judgment_class_options_read_the_selected_charts_class() {
    let ops = ["5, 180", "2, 181", "180", "-181"];
    let dsts: Vec<String> = ops
        .iter()
        .enumerate()
        .map(|(i, op)| {
            format!(
                r#"{{"id": "dot", "op": [{op}], "dst": [{}]}}"#,
                at(10 * i as i32, 1000, 8, 8)
            )
        })
        .collect();
    let d = folder(&minimal(&format!(
        r#""destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}, {}]"#,
        dsts.join(",")
    )));
    let mut s = Sel::of(d.path());
    let shown = |s: &mut Sel| -> Vec<usize> {
        s.drawn(0)
            .into_iter()
            .filter(|(t, _)| *t == DOT)
            .map(|(_, r)| (r[0] / 10.0) as usize)
            .collect()
    };
    for (class, want) in [
        (Some(0), vec![0, 2, 3]),
        (Some(1), vec![1]),
        (None, vec![3]),
    ] {
        let mut v = view(vec![chart("a"), folder_row("f", 3)], Some(0), None, None);
        v.selected.as_mut().unwrap().judge_class = class;
        s.view(&v);
        assert_eq!(shown(&mut s), want, "{class:?}");
    }
    s.view(&view(
        vec![chart("a"), folder_row("f", 3)],
        Some(1),
        None,
        None,
    ));
    assert_eq!(shown(&mut s), [3], "a folder row");
}

/// The settings indices the source reads from its player config and the
/// selected bar's play config, which a folder row has too, the mode
/// filter's (`skin/property/IntegerPropertyFactory.java:1111-1112`,
/// `:1216-1227`, `:1261-1315`; `select/MusicSelector.java:650-669` at
/// `0.8.9`), through the production path: 78 is the failure handling, 341
/// always 0 whatever the gauge, 342 Floating, 340 the preset's assignment
/// (nothing without a preset) and 330 to 332 the switches, on a chart row
/// and a folder row alike.
#[test]
fn the_settings_indices_follow_the_players_settings() {
    use skin_model::{GaugeLook, SelectionOrder, SpeedReference, SpeedView};
    let d = folder(&settings_skin());
    let mut s = Sel::new(import_selection(d.path()).unwrap().skin);
    let chart_row = view(vec![chart("a")], Some(0), None, None);
    let folder_row = view(vec![folder_row("f", 3)], Some(0), None, None);
    let floating = SpeedView::Floating {
        reference: SpeedReference::Maximum,
        green: 300,
    };
    let at = |s: &mut Sel, v, settings: &skin_model::SelectionSettings| {
        s.view(v);
        s.skin
            .update_selection_settings(&mut s.sel, settings)
            .unwrap();
        let f = settings_frame(s);
        [78, 330, 331, 332, 340, 341, 342].map(|id| f.indices.get(&id).copied())
    };
    let base = settings(
        0,
        0.5,
        floating,
        SelectionOrder::Title,
        Some(GaugeLook::Hard),
        [true; 3],
    );
    // A hard gauge (2 if the look were clamped to 0-2), Floating and
    // play-to-end: 78 continue (1), 341 assist easy (0), 342 on (1); the
    // same on a folder row.
    let shift = skin_model::SelectionSettings {
        play_to_end: true,
        assignment: Some(rules::tapestry().assignment),
        ..base
    };
    let all_on = [
        Some(1),
        Some(1),
        Some(1),
        Some(1),
        Some(1),
        Some(0),
        Some(1),
    ];
    assert_eq!(at(&mut s, &chart_row, &shift), all_on);
    assert_eq!(at(&mut s, &folder_row, &shift), all_on);
    // Close: 78 none (0). Classic: 342 off (0). On both rows.
    let close = skin_model::SelectionSettings {
        play_to_end: false,
        speed: SpeedView::Classic,
        ..shift
    };
    for row in [&chart_row, &folder_row] {
        let f = at(&mut s, row, &close);
        assert_eq!((f[0], f[6]), (Some(0), Some(0)));
    }
    // 340 by preset: combo (0) for the two source-style presets,
    // duration (1) for tapestry's, nothing without one, on both rows.
    for (assignment, want) in [
        (Some(rules::lr2_style().assignment), Some(0)),
        (Some(rules::beatoraja_style().assignment), Some(0)),
        (Some(rules::tapestry().assignment), Some(1)),
        (None, None),
    ] {
        let a = skin_model::SelectionSettings {
            assignment,
            ..shift
        };
        assert_eq!(at(&mut s, &chart_row, &a)[4], want, "{assignment:?}");
        assert_eq!(at(&mut s, &folder_row, &a)[4], want, "{assignment:?}");
    }
    // Each cover's switch alone, off then on.
    for (k, id) in [330, 331, 332].into_iter().enumerate() {
        let mut covers_on = [false; 3];
        let off = skin_model::SelectionSettings { covers_on, ..shift };
        let f = at(&mut s, &chart_row, &off);
        assert_eq!(f[1..4], [Some(0); 3], "{id}");
        covers_on[k] = true;
        let on = skin_model::SelectionSettings { covers_on, ..shift };
        let mut want = [Some(0); 3];
        want[k] = Some(1);
        assert_eq!(at(&mut s, &chart_row, &on)[1..4], want, "{id}");
        assert_eq!(at(&mut s, &folder_row, &on)[1..4], want, "{id}");
    }
}

/// The ids of the best play's breakdown, the best max combo and the clear
/// counts, each a number on its own row.
const BEST_NUMBERS: [i32; 40] = [
    75, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 100, 105, 110, 111, 112, 113, 114, 174,
    410, 411, 412, 413, 414, 415, 416, 417, 418, 419, 420, 421, 422, 423, 424, 425, 426, 427,
];

/// A skin with a 6-digit number per id of [`BEST_NUMBERS`] on row `i`.
fn best_numbers_skin() -> String {
    let values: Vec<String> = BEST_NUMBERS
        .iter()
        .map(|id| format!(r#"{{"id": "n{id}", {DIGITS}, "digit": 6, "ref": {id}}}"#))
        .collect();
    let dsts: Vec<String> = BEST_NUMBERS
        .iter()
        .enumerate()
        .map(|(i, id)| {
            format!(
                r#"{{"id": "n{id}", "dst": [{}]}}"#,
                at(0, 10 * i as i32, 4, 8)
            )
        })
        .collect();
    let list = song_list(&[&at(0, 900, 300, 40)], &[&at(0, 900, 300, 40)], 0, TITLE);
    format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}], "value": [{}], {list},
           "image": [{}],
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}, {}]"#,
        values.join(","),
        common::selection::IMAGES,
        dsts.join(",")
    )
}

/// Each drawn number of [`best_numbers_skin`] by its id.
fn best_numbers(s: &mut Sel) -> BTreeMap<i32, String> {
    let mut numbers: BTreeMap<i32, String> = BTreeMap::new();
    for (t, r) in s.drawn(0) {
        if let Some(d) = digit_of(t) {
            let i = ((1072.0 - r[1]) / 10.0) as usize;
            numbers
                .entry(BEST_NUMBERS[i])
                .or_default()
                .push(char::from(b'0' + d));
        }
    }
    numbers
}

/// A view of one chart played 5 times, 2 of them clears; with a best, its
/// play of 7 notes judged PERFECT 4 (1 early), GREAT 2 (early), BAD 1
/// (late) and 2 empty POORs (1 early, 1 late), max combo 4, while the best
/// max combo, another play's, is 9.
fn best_view(with_best: bool) -> skin_model::SelectionView {
    let mut row = chart("a");
    row.score = if with_best {
        RowScore::Best {
            plays: 5,
            ex: 10,
            bp: 1,
            lamp: OwnLamp::Failed,
        }
    } else {
        RowScore::NoBest { plays: 5 }
    };
    let mut v = view(vec![row], Some(0), None, None);
    let c = v.selected.as_mut().unwrap();
    c.clears = if with_best { 2 } else { 0 };
    let per = |perfect, great, bad| rules::PerJudgment {
        perfect,
        great,
        good: 0,
        bad,
        poor: 0,
    };
    c.best = with_best.then(|| skin_model::SelectedBest {
        max_combo: 9,
        play: skin_model::BestPlay {
            tally: eval::Tally {
                counts: per(4, 2, 1),
                ex_score: 10,
                bp: 1,
                max_combo: 4,
                empty_poor: 2,
                early: per(1, 2, 0),
                late: per(3, 0, 1),
                empty_poor_early: 1,
                empty_poor_late: 1,
                ..eval::Tally::default()
            },
            notes: 7,
        },
    });
    v
}

/// The selected chart's best play draws through the production path as
/// the source draws its stored score (`skin/property/
/// IntegerPropertyFactory.java` at `0.8.9`): the best max combo for 75, 105
/// and 174 (`:627-631`), the clears and the plays less them (`:140-152`),
/// the play's counts with their early and late splits (`:444-467`,
/// `:500-519`), their whole percent of its notes, and its point score from
/// its own max combo (`ScoreDataProperty.java:75-78`).
#[test]
fn the_best_play_breakdown_draws() {
    let d = folder(&best_numbers_skin());
    let mut s = Sel::new(import_selection(d.path()).unwrap().skin);
    s.view(&best_view(true));
    let want: BTreeMap<i32, String> = [
        (75, "9"),
        (77, "5"),
        (78, "2"),
        (79, "3"),
        (80, "4"),
        (81, "2"),
        (82, "0"),
        (83, "1"),
        (84, "0"),
        (85, "57"),
        (86, "28"),
        (87, "0"),
        (88, "14"),
        (89, "0"),
        (100, "142856"),
        (105, "9"),
        (110, "4"),
        (111, "2"),
        (112, "0"),
        (113, "1"),
        (114, "0"),
        (174, "9"),
        (410, "1"),
        (411, "3"),
        (412, "2"),
        (413, "0"),
        (414, "0"),
        (415, "0"),
        (416, "0"),
        (417, "1"),
        (418, "0"),
        (419, "0"),
        (420, "2"),
        (421, "1"),
        (422, "1"),
        (423, "3"),
        (424, "2"),
        (425, "1"),
        (426, "2"),
        (427, "3"),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_owned()))
    .collect();
    assert_eq!(best_numbers(&mut s), want);
}

/// Without a best play the source's empty values draw: with a score row
/// (plays, no best) the max combo, clears and the score's counts read 0
/// and the fails the plays, the rates nothing; unplayed, those draw
/// nothing; the point score and the judge counts read 0 either way
/// (`ScoreDataProperty.java:90-91`, `MainState.java:141-144`). A best play
/// of no notes draws its counts and no rate, as the source's hidden rate
/// with no notes (`IntegerPropertyFactory.java:505-511`).
#[test]
fn a_chart_without_a_best_play_shows_the_sources_empty_values() {
    let d = folder(&best_numbers_skin());
    let mut s = Sel::new(import_selection(d.path()).unwrap().skin);
    let zero_counts = [100, 110, 111, 112, 113, 114]
        .into_iter()
        .chain(410..=427)
        .map(|id| (id, "0".to_owned()));
    s.view(&best_view(false));
    let mut want: BTreeMap<i32, String> = zero_counts.clone().collect();
    for (id, v) in [
        (75, "0"),
        (77, "5"),
        (78, "0"),
        (79, "5"),
        (80, "0"),
        (81, "0"),
        (82, "0"),
        (83, "0"),
        (84, "0"),
        (105, "0"),
        (174, "0"),
    ] {
        want.insert(id, v.to_owned());
    }
    assert_eq!(best_numbers(&mut s), want);
    s.view(&view(vec![chart("u")], Some(0), None, None));
    assert_eq!(best_numbers(&mut s), zero_counts.collect());
    // A best play of no notes: its point score 0, its counts drawn, no
    // rate.
    let mut v = best_view(true);
    if let Some(b) = v.selected.as_mut().and_then(|c| c.best.as_mut()) {
        b.play.notes = 0;
    }
    s.view(&v);
    let drawn = best_numbers(&mut s);
    assert_eq!(drawn.get(&100).map(String::as_str), Some("0"));
    assert_eq!(drawn.get(&80).map(String::as_str), Some("4"));
    assert!((85..=89).all(|id| !drawn.contains_key(&id)), "{drawn:?}");
}
