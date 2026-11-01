//! Bitmap text: a `.fnt` font's line laid out as the source's runtime
//! lays out one run without tight bounds, its missing-glyph chain, and
//! the quads a build draws from it, with uvs found in the atlas each
//! frame.

mod lanes_support;
mod play_support;
mod widget_support;

use std::fs;
use std::path::Path;

use image::{ImageFormat, Rgba, RgbaImage};
use skin_model::{
    Align, AtlasLimits, ChartText, Fit, FontRef, LoadError, MAX_TEXT_BYTES, MAX_TEXT_ELEMENTS,
    MAX_WIDGET_QUADS, Overflow, Place, Quad, Root, Sheet, SheetBuilder, Skin, SkinDef, Source,
    TextDef, TextField, TextValue, Variant, WidgetDefs,
};

use play_support::{Harness, REGIONS};
use widget_support::{boolean, digits, element, number};

const CHART: ChartText<'static> = ChartText {
    title: "AB",
    subtitle: "",
    artist: "",
    subartist: "",
    genre: "",
};

/// A glyph: its code, its rectangle's left and size, and its x offset,
/// raw y offset and advance.
type Ch = (i64, [u32; 3], [i32; 3]);

/// A one-page `.fnt` file `name` of size 10 with `padding` and `base`,
/// its page `name.png` 64×16.
fn fnt(dir: &Path, name: &str, base: i32, padding: [i32; 4], chars: &[Ch], kernings: &[[i64; 3]]) {
    let [t, r, b, l] = padding;
    let mut s = format!(
        "info face=\"t\" size=10 padding={t},{r},{b},{l}\n\
         common lineHeight=12 base={base} scaleW=64 scaleH=16 pages=1\n\
         page id=0 file=\"{name}.png\"\nchars count={}\n",
        chars.len()
    );
    for &(id, [x, w, h], [xoff, yoff, adv]) in chars {
        s.push_str(&format!(
            "char id={id} x={x} y=0 width={w} height={h} xoffset={xoff} yoffset={yoff} \
             xadvance={adv} page=0 chnl=15\n"
        ));
    }
    if !kernings.is_empty() {
        s.push_str(&format!("kernings count={}\n", kernings.len()));
        for [a, b, n] in kernings {
            s.push_str(&format!("kerning first={a} second={b} amount={n}\n"));
        }
    }
    fs::write(dir.join(name), s).unwrap();
    RgbaImage::from_pixel(64, 16, Rgba([255, 255, 255, 255]))
        .save_with_format(dir.join(format!("{name}.png")), ImageFormat::Png)
        .unwrap();
}

/// `M`, 9 tall: with base 10 and no padding the ascent is 1.
fn m() -> Ch {
    (77, [60, 2, 9], [0, 0, 2])
}

/// A sheet of the bitmap fonts `chains`, each a primary file and its
/// fallbacks, all in `dir`.
fn sheet(dir: &Path, chains: &[&[&str]]) -> Sheet {
    let mut b = SheetBuilder::new(&Root::new(dir).unwrap());
    let mut files = Vec::new();
    for chain in chains {
        let ids: Vec<u16> = chain
            .iter()
            .map(|name| {
                if let Some(i) = files.iter().position(|f| f == name) {
                    u16::try_from(i).unwrap()
                } else {
                    files.push(*name);
                    b.admit_fnt(name, &mut |_| Ok(())).unwrap()
                }
            })
            .collect();
        b.add_bitmap_font(ids[0], &ids[1..]).unwrap();
    }
    b.finish()
}

fn text(value: TextValue, font: u32, align: Align, overflow: Overflow) -> TextDef {
    TextDef {
        value,
        font: FontRef::Bitmap(font),
        align,
        overflow,
    }
}

fn literal(s: &str) -> TextValue {
    TextValue::Literal(s.to_owned())
}

fn load(sheet: Sheet, texts: Vec<(TextDef, [i32; 4])>) -> Result<Skin, LoadError> {
    let elements = (0_u32..)
        .zip(&texts)
        .map(|(i, &(_, r))| element(Source::Text(i), r, Place::SCREEN))
        .collect();
    Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: REGIONS,
        whens: Vec::new(),
        sheet,
        lanes: None,
        elements,
        widgets: WidgetDefs {
            texts: texts.into_iter().map(|(t, _)| t).collect(),
            ..WidgetDefs::default()
        },
        bomb_up_to: Some(skin_model::Grade::Bad),
    })
}

/// The quads of one frame with the bitmap lines laid out.
fn drawn(skin: Skin) -> (Harness, Vec<Quad>) {
    let mut h = widget_support::harness(skin);
    h.skin.attach_bitmap_text(&mut h.play, &CHART);
    h.frame(100, &[]);
    let quads = h.batch.quads().to_vec();
    (h, quads)
}

fn rects(skin: Skin) -> Vec<[f32; 4]> {
    drawn(skin).1.iter().map(|q| q.rect).collect()
}

fn close(a: [f32; 4], b: [f32; 4]) {
    assert!(
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3),
        "{a:?} vs {b:?}"
    );
}

/// One font `f.fnt` of `chars` (with `M`), one text of `s` in `r`.
fn one(
    chars: &[Ch],
    kernings: &[[i64; 3]],
    s: &str,
    def: (Align, Overflow),
    r: [i32; 4],
) -> Vec<[f32; 4]> {
    let dir = tempfile::tempdir().unwrap();
    let mut all = chars.to_vec();
    all.push(m());
    fnt(dir.path(), "f.fnt", 10, [0; 4], &all, kernings);
    let sheet = sheet(dir.path(), &[&["f.fnt"]]);
    rects(load(sheet, vec![(text(literal(s), 0, def.0, def.1), r)]).unwrap())
}

/// `A`: x offset 3, 7 wide, advance 14.
const A: Ch = (65, [0, 7, 8], [3, 1, 14]);

#[test]
fn the_line_is_its_advances_without_tight_bounds() {
    let spill = (Align::Left, Overflow::Spill);
    // Left in a 20-wide box at scale 1: the ink from x 3, the line 14.
    let r = one(&[A], &[], "A", spill, [0, 100, 20, 10]);
    close(r[0], [3.0, 100.0, 7.0, 8.0]);
    // Shrunk into 10: by 10/14, the ink at 3·10/14 and 5 wide. A
    // tight-bounds line (7 wide from 0) would not shrink.
    let r = one(
        &[A],
        &[],
        "A",
        (Align::Left, Overflow::Shrink),
        [0, 100, 10, 10],
    );
    close(r[0], [30.0 / 14.0, 100.0, 5.0, 8.0]);
    // Centre and right place the 14-wide line, not the ink.
    let r = one(
        &[A],
        &[],
        "A",
        (Align::Center, Overflow::Spill),
        [0, 100, 20, 10],
    );
    close(r[0], [6.0, 100.0, 7.0, 8.0]);
    let r = one(
        &[A],
        &[],
        "A",
        (Align::Right, Overflow::Spill),
        [0, 100, 20, 10],
    );
    close(r[0], [9.0, 100.0, 7.0, 8.0]);
}

#[test]
fn kerning_moves_the_pen() {
    let b: Ch = (66, [10, 4, 8], [0, 1, 6]);
    let r = one(
        &[A, b],
        &[[65, 66, -2]],
        "AB",
        (Align::Left, Overflow::Spill),
        [0, 100, 40, 10],
    );
    close(r[1], [12.0, 100.0, 4.0, 8.0]);
}

#[test]
fn padding_moves_only_the_ascent() {
    // `M` 9 tall, padding 2 above and 1 below: cap height 6, ascent 4. A
    // glyph 8 tall at raw offset 2 has its top 2 above the box's top.
    let dir = tempfile::tempdir().unwrap();
    let g: Ch = (65, [0, 7, 8], [3, 2, 14]);
    fnt(dir.path(), "f.fnt", 10, [2, 5, 1, 5], &[g, m()], &[]);
    let s = sheet(dir.path(), &[&["f.fnt"]]);
    let d = (
        text(literal("A"), 0, Align::Left, Overflow::Spill),
        [0, 100, 40, 10],
    );
    close(
        rects(load(s.clone(), vec![d.clone()]).unwrap())[0],
        [3.0, 98.0, 7.0, 8.0],
    );
    // At 2.5 times the size.
    let big = (d.0, [0, 100, 100, 25]);
    close(
        rects(load(s, vec![big]).unwrap())[0],
        [7.5, 95.0, 17.5, 20.0],
    );
}

#[test]
fn a_fallback_glyph_moves_by_the_representative_offsets() {
    // The primary's `S` sits at raw 3; the fallback's at 5.
    let dir = tempfile::tempdir().unwrap();
    let p_s: Ch = (83, [20, 2, 9], [0, 3, 2]);
    let f_s: Ch = (83, [20, 2, 9], [0, 5, 2]);
    let g: Ch = (0x3042, [0, 7, 8], [0, 2, 9]);
    fnt(dir.path(), "p.fnt", 10, [2, 0, 1, 0], &[p_s, m()], &[]);
    fnt(dir.path(), "f.fnt", 10, [0; 4], &[f_s, g], &[]);
    fnt(dir.path(), "e.fnt", 10, [0; 4], &[p_s, g], &[]);
    let s = sheet(dir.path(), &[&["p.fnt", "f.fnt"], &["p.fnt", "e.fnt"]]);
    let d = |font| {
        (
            text(literal("\u{3042}"), font, Align::Left, Overflow::Spill),
            [0, 100, 40, 10],
        )
    };
    let r = rects(load(s, vec![d(0), d(1)]).unwrap());
    // Ascent 4: 100 − 4 + 2 − (5 − 3); with equal offsets, 98.
    close(r[0], [0.0, 96.0, 7.0, 8.0]);
    close(r[1], [0.0, 98.0, 7.0, 8.0]);
}

#[test]
fn missing_glyphs_follow_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let a: Ch = (65, [0, 1, 8], [0, 1, 10]);
    let tilde: Ch = (0xFF5E, [2, 2, 8], [0, 1, 10]);
    let square: Ch = (0x25A1, [6, 3, 8], [0, 1, 10]);
    let missing: Ch = (0, [10, 4, 8], [0, 1, 10]);
    let b: Ch = (66, [14, 5, 8], [0, 1, 10]);
    let emoji: Ch = (0x1_F600, [20, 6, 8], [0, 1, 10]);
    fnt(
        dir.path(),
        "p.fnt",
        10,
        [0; 4],
        &[a, tilde, missing, m()],
        &[],
    );
    fnt(
        dir.path(),
        "q.fnt",
        10,
        [0; 4],
        &[a, tilde, square, missing, m(), emoji],
        &[],
    );
    fnt(dir.path(), "f.fnt", 10, [0; 4], &[b], &[]);
    let s = sheet(dir.path(), &[&["p.fnt", "f.fnt"], &["q.fnt", "f.fnt"]]);
    let width = |q: &[f32; 4]| q[2];
    let line = |font, s: &str| {
        let d = (
            text(literal(s), font, Align::Left, Overflow::Spill),
            [0, 100, 200, 10],
        );
        (d.0, d.1)
    };
    // The primary, a fallback, the other tilde, and for `Z` the missing
    // glyph (no `□` in either file); a supplementary character without a
    // glyph is skipped.
    let r = rects(load(s.clone(), vec![line(0, "AB\u{301C}Z\u{1F601}A")]).unwrap());
    assert_eq!(
        r.iter().map(width).collect::<Vec<_>>(),
        [1.0, 5.0, 2.0, 4.0, 1.0]
    );
    close(r[4], [40.0, 100.0, 1.0, 8.0]);
    // `□` comes before the missing glyph; the supplementary glyph is the
    // file's own.
    let r = rects(load(s, vec![line(1, "Z\u{1F600}\u{1F601}")]).unwrap());
    assert_eq!(r.iter().map(width).collect::<Vec<_>>(), [3.0, 6.0, 3.0]);
}

#[test]
fn a_substituted_glyph_keeps_no_kerning() {
    let a: Ch = (65, [0, 1, 8], [0, 1, 10]);
    let square: Ch = (0x25A1, [6, 3, 8], [0, 1, 10]);
    let kern = [[65, 90, -4], [0x25A1, 65, -3]];
    let spill = (Align::Left, Overflow::Spill);
    // The kerning before `Z` is looked up by `Z` itself.
    let r = one(&[a, square], &kern, "AZ", spill, [0, 100, 200, 10]);
    close(r[1], [6.0, 100.0, 3.0, 8.0]);
    // After the substituted `□` there is none; after a real one there is.
    let r = one(&[a, square], &kern, "ZA", spill, [0, 100, 200, 10]);
    close(r[1], [10.0, 100.0, 1.0, 8.0]);
    let r = one(&[a, square], &kern, "\u{25A1}A", spill, [0, 100, 200, 10]);
    close(r[1], [7.0, 100.0, 1.0, 8.0]);
}

#[test]
fn empty_glyphs_advance_and_a_negative_line_still_draws() {
    let gap: Ch = (66, [0, 0, 8], [0, 1, 6]);
    let back: Ch = (67, [0, 2, 8], [0, 1, -30]);
    let spill = (Align::Left, Overflow::Shrink);
    let r = one(&[A, gap], &[], "BA", spill, [0, 100, 40, 10]);
    assert_eq!(r.len(), 1);
    close(r[0], [9.0, 100.0, 7.0, 8.0]);
    // A line −30 wide is drawn unsqueezed, placed by its alignment.
    let r = one(
        &[back, (108, [0, 1, 1], [0, 0, 1])],
        &[],
        "C",
        (Align::Right, Overflow::Shrink),
        [0, 100, 40, 10],
    );
    close(r[0], [70.0, 100.0, 2.0, 8.0]);
}

#[test]
fn glyph_uvs_are_found_in_each_atlas() {
    let dir = tempfile::tempdir().unwrap();
    fnt(
        dir.path(),
        "f.fnt",
        10,
        [0; 4],
        &[(65, [13, 7, 8], [0, 1, 14]), m()],
        &[],
    );
    let s = sheet(dir.path(), &[&["f.fnt"]]);
    let skin = load(
        s,
        vec![(
            text(literal("A"), 0, Align::Left, Overflow::Spill),
            [0, 100, 40, 10],
        )],
    )
    .unwrap();
    let (mut h, quads) = drawn(skin);
    for reserved in [1, 3] {
        let limits = AtlasLimits {
            max_side: 256,
            max_layers: 8,
            reserved_layers: reserved,
        };
        let atlas = h.skin.atlas(Variant::Lo, limits).unwrap();
        let side = atlas.side() as f32;
        let uvs = atlas.into_parts().1;
        let page = uvs.frames()[0];
        let px = page.uv[0] * side;
        let py = page.uv[1] * side;
        h.uvs = uvs;
        h.frame(100, &[]);
        let q = h.batch.quads()[0];
        assert_eq!(q.layer, page.layer);
        assert_eq!(
            q.uv,
            [
                (px + 13.0) / side,
                py / side,
                (px + 20.0) / side,
                (py + 8.0) / side
            ]
        );
    }
    assert_eq!(quads.len(), 1);
}

#[test]
fn bitmap_text_is_laid_out_only_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    fnt(dir.path(), "f.fnt", 10, [0; 4], &[A, m()], &[]);
    let s = sheet(dir.path(), &[&["f.fnt"]]);
    let def = (
        text(
            TextValue::Field(TextField::Title),
            0,
            Align::Left,
            Overflow::Spill,
        ),
        [0, 100, 40, 10],
    );
    let skin = load(s.clone(), vec![def.clone()]).unwrap();
    // No raster request for a bitmap slot.
    assert!(
        skin.text_requests(&CHART, [1_920, 1_080], &skin_model::Layout::AUTHOR)
            .is_empty()
    );
    let mut h = widget_support::harness(skin);
    h.frame(100, &[]);
    assert!(h.batch.quads().is_empty());
    h.skin.attach_bitmap_text(&mut h.play, &CHART);
    h.frame(100, &[]);
    // "AB": `B` is missing and the font has no missing glyph.
    assert_eq!(h.batch.quads().len(), 1);
    // A visibility folded to false draws nothing.
    let mut e = element(Source::Text(0), def.1, Place::SCREEN);
    e.visible = Some(boolean("false"));
    let skin = Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: REGIONS,
        whens: Vec::new(),
        sheet: s,
        lanes: None,
        elements: vec![e],
        widgets: WidgetDefs {
            texts: vec![def.0],
            ..WidgetDefs::default()
        },
        bomb_up_to: Some(skin_model::Grade::Bad),
    })
    .unwrap();
    assert!(drawn(skin).1.is_empty());
}

#[test]
fn a_sheet_with_bitmap_fonts_takes_no_more_fonts() {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..skin_model::MAX_FONTS)
        .map(|i| format!("f{i}.fnt"))
        .collect();
    for n in &names {
        fnt(dir.path(), n, 10, [0; 4], &[A, m()], &[]);
    }
    fs::write(dir.path().join("o.ttf"), [0, 1, 0, 0, 0]).unwrap();
    let chains: Vec<[&str; 1]> = names.iter().map(|n| [n.as_str()]).collect();
    let chains: Vec<&[&str]> = chains.iter().map(|c| c.as_slice()).collect();
    let s = sheet(dir.path(), &chains);
    let outline = skin_model::FontDef {
        path: "o.ttf".into(),
    };
    assert!(matches!(
        s.clone().with_fonts(vec![outline]),
        Err(skin_model::FontError::BitmapFonts)
    ));
    assert!(s.with_fonts(Vec::new()).is_ok());
}

#[test]
fn bitmap_texts_fill_the_widget_quads() {
    let dir = tempfile::tempdir().unwrap();
    fnt(dir.path(), "f.fnt", 10, [0; 4], &[A, m()], &[]);
    RgbaImage::from_pixel(10, 1, Rgba([255; 4]))
        .save_with_format(dir.path().join("d.png"), ImageFormat::Png)
        .unwrap();
    let mut b = SheetBuilder::new(&Root::new(dir.path()).unwrap());
    let f = b.admit_fnt("f.fnt", &mut |_| Ok(())).unwrap();
    b.add_bitmap_font(f, &[]).unwrap();
    let file = b
        .admit_file(skin_model::FileDef {
            path: "d.png".into(),
            hi: None,
        })
        .unwrap();
    let image = b
        .add_image(skin_model::ImageDef {
            file,
            rect: None,
            grid: [10, 1],
            cycle: None,
            filter: skin_model::Filter::Linear,
        })
        .unwrap();
    let s = b.finish();
    let field = text(
        TextValue::Field(TextField::Title),
        0,
        Align::Left,
        Overflow::Spill,
    );
    let texts = vec![field; MAX_TEXT_ELEMENTS];
    assert_eq!(
        u32::try_from(MAX_TEXT_ELEMENTS * MAX_TEXT_BYTES).unwrap(),
        MAX_WIDGET_QUADS
    );
    let mut elements: Vec<_> = (0_u32..)
        .take(MAX_TEXT_ELEMENTS)
        .map(|i| element(Source::Text(i), [0, 0, 10, 10], Place::SCREEN))
        .collect();
    let def = |elements, numbers| SkinDef {
        fit: Fit::Adapt,
        regions: REGIONS,
        whens: Vec::new(),
        sheet: s.clone(),
        lanes: None,
        elements,
        widgets: WidgetDefs {
            texts: texts.clone(),
            numbers,
            ..WidgetDefs::default()
        },
        bomb_up_to: Some(skin_model::Grade::Bad),
    };
    let skin = Skin::new(def(elements.clone(), Vec::new())).unwrap();
    assert_eq!(skin.quad_capacity(), MAX_WIDGET_QUADS);
    elements.push(element(Source::Number(0), [0, 0, 1, 1], Place::SCREEN));
    let one = number(
        digits(
            image,
            skin_model::GlyphSet::Ten,
            1,
            Align::Left,
            skin_model::Pad::Blank,
        ),
        "1",
    );
    assert!(matches!(
        Skin::new(def(elements, vec![one])),
        Err(LoadError::TooManyWidgetQuads { n }) if n == u64::from(MAX_WIDGET_QUADS) + 1
    ));
    // A literal reserves a quad per character, not per byte.
    let accented = text(literal("AÀA"), 0, Align::Left, Overflow::Spill);
    let skin = load(s.clone(), vec![(accented, [0, 0, 10, 10])]).unwrap();
    assert_eq!(skin.quad_capacity(), 3);
    // An unknown bitmap font.
    let bad = text(literal("A"), 1, Align::Left, Overflow::Spill);
    assert!(matches!(
        load(s, vec![(bad, [0, 0, 10, 10])]),
        Err(LoadError::Text {
            error: skin_model::TextError::UnknownFont,
            ..
        })
    ));
}
