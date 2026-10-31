//! An imported skin through the whole model path: atlas, uv table,
//! prepare, text requests, a text table, and frames built at several
//! play states.

mod common;

use common::play::{Drawn, judged, round, tally};
use common::{font, hd, import_dir, png, skin};
use rules::Judgment;

/// A 1920×1080 skin: a judge-cycled image, a critical EX score, a
/// count, an image on the A rank timer, and two texts.
fn json() -> String {
    hd(r#"
        "source": [
            {"id": "a", "path": "a.png"},
            {"id": "n", "path": "n.png"}
        ],
        "font": [{"id": "f", "path": "f.ttf"}],
        "image": [
            {"id": "plain", "src": "a", "x": 0, "y": 0, "w": 64, "h": 64},
            {"id": "cyc", "src": "a", "x": 0, "y": 0, "w": 64, "h": 32, "divx": 2,
             "timer": 46, "cycle": 100},
            {"id": "rank", "src": "a", "x": 0, "y": 32, "w": 64, "h": 32}
        ],
        "value": [
            {"id": "ex", "src": "n", "x": 0, "y": 0, "w": 100, "h": 10, "divx": 10,
             "digit": 4, "ref": 71},
            {"id": "pg", "src": "n", "x": 0, "y": 0, "w": 100, "h": 10, "divx": 10,
             "digit": 3, "ref": 110}
        ],
        "text": [
            {"id": "title", "font": "f", "size": 24, "ref": 10},
            {"id": "lit", "font": "f", "size": 24, "constantText": "hello", "align": 1}
        ],
        "destination": [
            {"id": "plain", "dst": [{"x": 10, "y": 20, "w": 64, "h": 64}]},
            {"id": "cyc", "timer": 46, "dst": [{"x": 100, "y": 20, "w": 32, "h": 32}]},
            {"id": "ex", "timer": 41, "dst": [{"x": 200, "y": 500, "w": 10, "h": 20}]},
            {"id": "pg", "timer": 41, "dst": [{"x": 200, "y": 600, "w": 10, "h": 20}]},
            {"id": "rank", "timer": 348, "dst": [{"x": 300, "y": 20, "w": 64, "h": 32}]},
            {"id": "title", "timer": 41, "dst": [{"x": 400, "y": 900, "w": 300, "h": 30}]},
            {"id": "lit", "timer": 41, "dst": [{"x": 400, "y": 800, "w": 300, "h": 30}]}
        ]
    "#)
}

#[test]
fn an_imported_skin_draws_through_the_model() {
    let d = skin(&[("s.json", &json())]);
    png(d.path(), "a.png", 64, 64);
    png(d.path(), "n.png", 100, 10);
    font(d.path(), "f.ttf");
    let imported = import_dir(d.path()).unwrap();
    let r = &imported.report;
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    assert_eq!(r.usage.elements, 7);
    assert_eq!(r.usage.images, 5);
    assert_eq!(r.usage.files, 2);
    assert_eq!(r.usage.fonts, 1);
    assert_eq!(r.usage.texts, 2);
    assert_eq!(r.usage.whens, 1);
    assert_eq!(r.usage.widget_quads, 4 + 3 + 2);
    // Retained: five images, 1 + 2 + 1 + 10 + 10 frames, every element's
    // quads.
    let sheet = imported.skin.sheet();
    assert_eq!(sheet.image_count(), 5);
    assert_eq!(sheet.frame_count(), 24);
    assert_eq!(imported.skin.quad_capacity(), 3 + 4 + 3 + 2);
    assert_eq!(imported.skin.text_slots(), 2);

    let mut drawn = Drawn::new(imported.skin, 100);
    drawn.attach_lines(50.0, 20.0);
    // Playing from 0 (no lead-in): the untimed image, each number's one
    // glyph "0", and the texts; the judge-timed image and the rank image
    // wait.
    let playing = drawn.frame(0, &[]);
    assert_eq!(playing.len(), 1 + 1 + 1 + 2, "{playing:?}");
    assert_eq!(round(playing[0].rect), [10, 1080 - 84, 64, 64]);
    // The EX score's last cell: x + 3 cells of 10 px.
    assert_eq!(round(playing[1].rect), [230, 1080 - 520, 10, 20]);

    // A judgment starts the cycled image; the A rank shows once the EX
    // score reaches 18/27 of the maximum (100 notes: 2 × 100 × 18/27 ≥
    // 134, so 134 is the first).
    drawn.tally = tally(133, 1);
    let below = drawn.frame(1_100, &[judged(1_100, Judgment::Perfect, 1)]);
    assert_eq!(below.len(), 1 + 1 + 3 + 1 + 2, "{below:?}");
    drawn.tally = tally(134, 2);
    let at = drawn.frame(1_200, &[]);
    assert_eq!(at.len(), 1 + 1 + 3 + 1 + 2 + 1, "{at:?}");
    let rank = at.iter().find(|q| round(q.rect)[0] == 300).unwrap();
    assert_eq!(round(rank.rect), [300, 1080 - 52, 64, 32]);
}
