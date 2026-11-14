//! Number 316 beside the SUDDEN+ paint it describes: over the default
//! lanes (a 600 px lane, SUDDEN+'s destination standing on their top, as
//! `import_covers.rs` lays it out), the paint's slide and the number agree
//! with LIFT on or off. Another placement of the paint is the skin's own
//! and is not claimed here.

mod common;

use bms_model::{ChartBuilder, Lane, TimeUs};
use common::lanes::{NOTES, body, cover, note};
use common::play::{Drawn, MS, round};
use common::{Loaded, import_body};
use skin_model::{CoverSettings, FrameReadouts};

/// The EX score, one cell at x 1 500, and 316, four cells from x 1 600;
/// the EX score is the lowest number on the canvas, as `ex_glyphs` needs.
const VALUES: &str = r#""value": [
    {"id": "ex", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 1, "ref": 71},
    {"id": "abs", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4, "ref": 316}]"#;

/// The default lanes, SUDDEN+'s paint, and [`VALUES`].
fn skin() -> Loaded {
    let objects = format!("{}, {VALUES}", note(""));
    let ex = r#"{"id": "ex", "dst": [{"x": 1500, "y": 0, "w": 12, "h": 20}]}"#;
    let abs = r#"{"id": "abs", "dst": [{"x": 1600, "y": 50, "w": 12, "h": 20}]}"#;
    import_body(&body("", &objects, &[NOTES, &cover(""), ex, abs]))
}

/// The paint's slide from where it stands at 0 ‰, in px, and 316's
/// digits, at `ms`.
fn slide_and_number(d: &mut Drawn, ms: i64) -> (i32, String) {
    let glyph = d.ex_glyphs(ms);
    let q = d.frame(ms, &[]);
    let paint: Vec<[i32; 4]> = q
        .iter()
        .map(|q| round(q.rect))
        .filter(|r| r[2] == 430 && r[3] == 600)
        .collect();
    assert_eq!(paint.len(), 1, "{paint:?}");
    let mut cells: Vec<_> = q
        .iter()
        .filter(|q| (1_600..1_648).contains(&round(q.rect)[0]))
        .map(|q| (round(q.rect)[0], glyph[&q.uv.map(f32::to_bits)]))
        .collect();
    cells.sort_unstable();
    // The paint stands at y −320 at 0 ‰.
    (paint[0][1] + 320, cells.into_iter().map(|c| c.1).collect())
}

/// SUDDEN+ 500 ‰ on with a lift of 200 ‰ stored: with LIFT off the paint
/// slides 300 px and 316 reads 500; with LIFT on 240 px and 400; each
/// slide is 316 ‰ of the 600 px lane.
#[test]
fn number_316_agrees_with_the_paint_on_the_default_lanes() {
    let chart = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(5_000 * MS))
        .build()
        .unwrap();
    let mut d = Drawn::over(skin().ok().skin.clone(), &chart);
    for (ms, lift, slide, number) in [(1_000, false, 300, "500"), (1_016, true, 240, "400")] {
        d.view.cover = 500;
        d.view.lift = if lift { 200 } else { 0 };
        d.readouts = Some(FrameReadouts {
            covers: CoverSettings {
                cover: 500,
                lift: 200,
                hidden: 0,
                cover_enabled: true,
                lift_enabled: lift,
                hidden_enabled: false,
                adjusting: false,
            },
            ..FrameReadouts::UNAVAILABLE
        });
        let (s, n) = slide_and_number(&mut d, ms);
        assert_eq!((s, n.as_str()), (slide, number), "LIFT {lift}");
        assert_eq!(s, n.parse::<i32>().unwrap() * 600 / 1_000, "LIFT {lift}");
    }
}
