//! CPU benchmarks of a skin's build (design.md "Measurement").
//!
//! `skin_model_build_dense_frame` is `Skin::build` of a skin of just the
//! lanes, laid out as the simple skin's, on the render benchmark's dense
//! chart and frame (crates/render/benches/skin.rs), so the two compare.
//! The chart is built here; the skin and the frame helpers are the lanes
//! tests'.
//!
//! `skin_model_build_widgets` is `Skin::build` of a skin of widgets only:
//! four 11-cell numbers, a 50-segment flicker gauge, a judgment display
//! with every grade and 4-cell combos, and two lines of text (one
//! squeezed, one spilling), all drawn. The skin helpers are the widget
//! tests'.
//!
//! `skin_model_build_bitmap_text` is `Skin::build` of 64 lines of 256
//! kerned glyphs in a bitmap font, the widget quad cap.

#[path = "../tests/lanes_support/mod.rs"]
mod lanes_support;
#[path = "../tests/play_support/mod.rs"]
mod play_support;
#[path = "../tests/widget_support/mod.rs"]
mod widget_support;

use std::hint::black_box;

use bms_model::{ChartBuilder, TimeUs};
use criterion::{Criterion, criterion_group, criterion_main};
use lanes_support::{lanes_skin, simple};
use play_support::{Harness, MS, full, lane, pct};
use rules::Judgment;
use skin_model::{
    Align, ChartText, DigitsDef, Fit, FontRef, FrameUv, GlyphSet, JudgeSlot, MAX_TEXT_BYTES,
    MAX_TEXT_ELEMENTS, MAX_WIDGET_QUADS, Motion, Overflow, Pad, Place, Root, SheetBuilder, Skin,
    SkinDef, Source, TextDef, TextImage, TextTable, TextValue, WidgetDefs,
};
use widget_support::{
    TEN, combo, digits, element, gauge, harness, judge_all, judge_element, judgment, load, number,
};

/// Notes on every lane every 6 ms for 30 s, with a long note on lane 3
/// every 20th step: a frame shows the whole visible window full.
fn bench_build(c: &mut Criterion) {
    let mut builder = ChartBuilder::new(150_000);
    for step in 0..5_000_i64 {
        let at = TimeUs(1_000 * MS + step * 6 * MS);
        for l in 0..8_u8 {
            builder = match (l, step % 20) {
                (3, 0) => builder.long(lane(l), at, TimeUs(at.0 + 100 * MS)),
                // Lane 3 is inside its long note.
                (3, 1..=16) => builder,
                _ => builder.tap(lane(l), at),
            };
        }
    }
    let chart = builder.build().expect("valid chart");
    let mut h = Harness::new(lanes_skin(simple()), &chart, &full(0));
    h.offset = TimeUs(8 * MS);
    let now = 10_000 * MS;
    h.frame_us(now, &[]);
    assert!(h.batch.quads().len() > 500, "a dense frame");
    assert_eq!(h.batch.dropped(), 0, "the batch has room");
    c.bench_function("skin_model_build_dense_frame", |b| {
        b.iter(|| {
            h.frame_us(black_box(now), &[]);
            black_box(h.batch.quads().len())
        });
    });
}

/// 4 · 11 cells, 50 segments and the flicker's tip, a grade with a 4-cell
/// combo, and two lines: 44 + 51 + 5 + 2 quads a frame.
fn bench_widgets(c: &mut Criterion) {
    let mut j = judge_all([800, 500, 200, 80]);
    let strip = DigitsDef {
        space: 0,
        ..digits(TEN, GlyphSet::Ten, 4, Align::Right, Pad::Zero)
    };
    for s in JudgeSlot::ALL {
        j.combos[s.index()] = Some(combo(strip, [210, 10, 20, 60]));
    }
    let values = ["combo", "ex_score", "combo * 7", "bp"];
    let mut elements: Vec<_> = (0..4_u32)
        .map(|i| {
            let y = i32::try_from(i).expect("4 numbers") * 40;
            element(Source::Number(i), [100, y, 20, 30], Place::SCREEN)
        })
        .collect();
    elements.push(element(Source::Gauge(0), [0, 200, 500, 20], Place::SCREEN));
    elements.push(judge_element(0));
    // A line 100 wide at an em of 40: squeezed into 60, spilling from 300.
    elements.push(element(Source::Text(0), [600, 0, 60, 40], Place::SCREEN));
    elements.push(element(Source::Text(1), [600, 50, 300, 40], Place::SCREEN));
    let text = |overflow| TextDef {
        value: TextValue::Literal("TEXT".into()),
        font: FontRef::Default,
        align: Align::Center,
        overflow,
    };
    let skin = load(
        elements,
        WidgetDefs {
            numbers: values
                .iter()
                .map(|v| number(digits(TEN, GlyphSet::Ten, 11, Align::Right, Pad::Zero), v))
                .collect(),
            gauges: vec![gauge(
                50,
                Motion::Flicker {
                    period: TimeUs(1_000_000),
                },
            )],
            judges: vec![j],
            texts: vec![text(Overflow::Shrink), text(Overflow::Spill)],
            image_choices: Vec::new(),
            cover_paints: Vec::new(),
        },
    )
    .expect("valid skin");
    let mut h = harness(skin);
    let uv = FrameUv {
        uv: [0.0, 0.0, 1.0, 1.0],
        layer: 1,
    };
    let line = TextImage::new(uv, [0.0, 0.125, 2.5, 1.25], 2.5);
    h.skin
        .attach_text(&mut h.play, TextTable::new(vec![line; 2]))
        .expect("a table per slot");
    h.tally = play_support::tally(1_234);
    h.gauges[0].value = pct(50);
    h.frame(500, &[judgment(500, Judgment::Great, 1_234)]);
    assert_eq!(h.batch.quads().len(), 102, "every widget drawn");
    assert_eq!(h.batch.dropped(), 0, "the batch has room");
    c.bench_function("skin_model_build_widgets", |b| {
        b.iter(|| {
            h.frame(black_box(500), &[]);
            black_box(h.batch.quads().len())
        });
    });
}

/// 64 bitmap texts of 256 kerned glyphs, at the widget quad cap: every
/// glyph's uv found in the atlas each frame.
fn bench_bitmap_text(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("a font folder");
    let fnt = "info face=\"b\" size=10 padding=0,0,0,0\n\
               common lineHeight=12 base=10 scaleW=64 scaleH=16 pages=1\n\
               page id=0 file=\"b.png\"\nchars count=2\n\
               char id=65 x=0 y=0 width=7 height=9 xoffset=1 yoffset=1 xadvance=8 page=0\n\
               char id=66 x=8 y=0 width=7 height=9 xoffset=1 yoffset=1 xadvance=8 page=0\n\
               kernings count=1\nkerning first=65 second=66 amount=-1\n";
    std::fs::write(dir.path().join("b.fnt"), fnt).expect("write the font");
    image::RgbaImage::from_pixel(64, 16, image::Rgba([255; 4]))
        .save_with_format(dir.path().join("b.png"), image::ImageFormat::Png)
        .expect("write the page");
    let mut b = SheetBuilder::new(&Root::new(dir.path()).expect("a root"));
    let f = b.admit_fnt("b.fnt", &mut |_| Ok(())).expect("the font");
    b.add_bitmap_font(f, &[]).expect("a bitmap font");
    let text = TextDef {
        value: TextValue::Literal("AB".repeat(MAX_TEXT_BYTES / 2)),
        font: FontRef::Bitmap(0),
        align: Align::Left,
        overflow: Overflow::Spill,
    };
    let elements = (0..MAX_TEXT_ELEMENTS)
        .map(|i| {
            let y = i32::try_from(i).expect("64 texts") * 16;
            element(
                Source::Text(u32::try_from(i).expect("64 texts")),
                [0, y, 1_000, 16],
                Place::SCREEN,
            )
        })
        .collect();
    let skin = Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: play_support::REGIONS,
        whens: Vec::new(),
        sheet: b.finish(),
        lanes: None,
        elements,
        widgets: WidgetDefs {
            texts: vec![text; MAX_TEXT_ELEMENTS],
            ..WidgetDefs::default()
        },
        bomb_up_to: Some(skin_model::Grade::Bad),
    })
    .expect("valid skin");
    let mut h = harness(skin);
    let chart = ChartText {
        title: "",
        subtitle: "",
        artist: "",
        subartist: "",
        genre: "",
    };
    h.skin.attach_bitmap_text(&mut h.play, &chart);
    h.frame(500, &[]);
    let quads = usize::try_from(MAX_WIDGET_QUADS).expect("a quad count");
    assert_eq!(h.batch.quads().len(), quads, "every glyph drawn");
    assert_eq!(h.batch.dropped(), 0, "the batch has room");
    c.bench_function("skin_model_build_bitmap_text", |b| {
        b.iter(|| {
            h.frame(black_box(500), &[]);
            black_box(h.batch.quads().len())
        });
    });
}

criterion_group!(benches, bench_build, bench_widgets, bench_bitmap_text);
criterion_main!(benches);
