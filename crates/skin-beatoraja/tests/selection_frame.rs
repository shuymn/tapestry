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
/// the "all charts" row 9, else 4, else 0, and bar image 5, else 1, else
/// 0.
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
    // image 1, then 0; a folder to 0.
    for (set, want) in [
        (r#"["b0", "b1"]"#, [B0, B1, B1]),
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
    assert!(!label(&mut s, 500));
    s.timers.change = Some(TimeUs(200_000));
    assert!(label(&mut s, 200));
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
