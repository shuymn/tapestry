//! The producer: faces checked at installation, materialisation, the
//! fallback, cap heights, the glyph and pixel bounds, the refit, and the
//! checks that keep anything unchecked from the rasterizer.

use skin_model::{FontRef, Skin, TextRequest};
use swash::zeno::{Point, Verb};

use super::{
    DefaultFont, LineBounds, LineFit, LineLayout, MAX_SHAPED_GLYPHS, OutlineError, OutlineText,
    RASTER_CALLS, RASTER_MARGIN, RasterError, TextNote, Window, place_points, rasterize_checked,
};
use crate::font_support::{self, Glyph, Os2, Spec};
use crate::outline_support::{self, Value, default_font, text, ttf};

/// The rasterizer calls on this thread so far.
fn raster_calls() -> u32 {
    RASTER_CALLS.with(std::cell::Cell::get)
}

/// The skin of one title text in `ttf.ttf`, its file `font`.
fn skin_with(font: Vec<u8>) -> (tempfile::TempDir, Skin) {
    let dir = outline_support::folder(
        &[("ttf.ttf", font)],
        &[ttf()],
        &[text("f", 100, Value::Ref(10), [100, 100, 400, 100])],
    );
    let skin = outline_support::import(dir.path()).skin;
    (dir, skin)
}

/// The producer of a title in `spec`'s font, with the default font.
fn producer_of(spec: &Spec) -> Result<OutlineText, OutlineError> {
    let (_dir, skin) = skin_with(font_support::build(spec));
    outline_support::producer(&skin, &default_font())
}

fn req(font: FontRef, s: &str, em: f32) -> TextRequest {
    TextRequest {
        slot: 0,
        text: s.to_owned(),
        font,
        em_px: em,
    }
}

const SKIN: FontRef = FontRef::File(0);
const BOUNDS: LineBounds = LineBounds { w: 4_080, h: 1_024 };

fn fits(t: &OutlineText, r: &TextRequest) -> LineLayout {
    match t.layout_line(r, r.em_px, BOUNDS) {
        LineFit::Fits(l) => l,
        other => panic!("{other:?}"),
    }
}

fn raster(t: &OutlineText, l: &LineLayout) -> Vec<u8> {
    let [w, h] = l.size;
    let mut out = vec![0; (w * h) as usize];
    t.raster_line(l, &mut out).unwrap();
    out
}

fn reason(e: OutlineError) -> String {
    match e {
        OutlineError::Font {
            font: Some(0),
            reason,
        } => reason,
        other => panic!("{other:?}"),
    }
}

#[test]
fn ttcf_count_refused_before_reservation() {
    let mut header = b"ttcf".to_vec();
    header.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
    header.extend_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(header.len(), 12);
    let (_dir, skin) = skin_with(header);
    let e = outline_support::producer(&skin, &default_font()).unwrap_err();
    assert_eq!(
        reason(e),
        "the collection declares no face or more than it holds"
    );
    // No face at all is refused too.
    let mut empty = b"ttcf".to_vec();
    empty.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    let (_dir, skin) = skin_with(empty);
    assert!(outline_support::producer(&skin, &default_font()).is_err());
}

#[test]
fn varc_refused() {
    let mut spec = font_support::latin("Varc");
    spec.extra.push((*b"VARC", vec![0; 8]));
    assert!(reason(producer_of(&spec).unwrap_err()).contains("VARC"));
    // The same face without it installs.
    spec.extra.clear();
    producer_of(&spec).unwrap();
}

#[test]
fn zero_units_per_em_refused() {
    let mut spec = font_support::latin("Zero");
    spec.upem = 0;
    assert!(reason(producer_of(&spec).unwrap_err()).contains("units per em"));
}

#[test]
fn collection_face_zero_only() {
    // Face 1's capitals are twice as tall: only face 0's are drawn.
    let zero = font_support::latin("Zero");
    let mut one = font_support::latin("One");
    for g in &mut one.glyphs {
        if let Some(r) = &mut g.rect {
            r[3] *= 2;
        }
    }
    let (_dir, skin) = skin_with(font_support::collection(&[zero.clone(), one]));
    let t = outline_support::producer(&skin, &default_font()).unwrap();
    let from_collection = fits(&t, &req(SKIN, "H", 100.0));
    let plain = producer_of(&zero).unwrap();
    assert_eq!(from_collection, fits(&plain, &req(SKIN, "H", 100.0)));
    assert_eq!(from_collection.size[1], 70);
}

/// Fails the default face's materialisation in the skin font's system:
/// its probe line does not touch that face (the skin face has every probe
/// character), so only the materialisation reaches it.
fn fail_fallback(
    fonts: &mut glyphon::cosmic_text::FontSystem,
    id: glyphon::cosmic_text::fontdb::ID,
) -> bool {
    let db = fonts.db();
    let fallback = db.len() == 2
        && db
            .face(id)
            .is_some_and(|f| f.families.iter().any(|(n, _)| n == super::DEFAULT_FAMILY));
    !fallback && super::get_font(fonts, id)
}

#[test]
fn materialised_at_installation() {
    let (_dir, skin) = skin_with(font_support::build(&font_support::cjk("Skin")));
    let fonts = outline_support::read_fonts(&skin);
    let default = default_font();
    OutlineText::with_materialise(&skin, fonts.clone(), &default, super::get_font).unwrap();
    let e = OutlineText::with_materialise(&skin, fonts, &default, fail_fallback).unwrap_err();
    assert!(reason(e).contains("does not load"));
}

#[test]
fn a_missing_glyph_comes_from_the_default_font() {
    // A Latin skin face and a CJK title: the CJK glyphs come from the
    // default face, the same coverage as in the default font's own line,
    // the line lower by the cap heights' difference (0.7 em against 0.6).
    let t = producer_of(&font_support::latin("Skin")).unwrap();
    let title = "あいう";
    let skin = fits(&t, &req(SKIN, title, 100.0));
    let default = fits(&t, &req(FontRef::Default, title, 100.0));
    assert!(skin.glyphs.iter().all(|g| g.face == 1), "{:?}", skin.glyphs);
    assert_eq!(skin.size, default.size);
    assert_eq!(raster(&t, &skin), raster(&t, &default));
    assert!(
        (skin.ink[1] - default.ink[1] - 0.1).abs() < 1e-4,
        "{skin:?} {default:?}"
    );
}

#[test]
fn notdef_is_the_skin_face_s() {
    // Neither face has '€': the skin face's .notdef, 500 × 700 at 100.
    let t = producer_of(&font_support::latin("Skin")).unwrap();
    let l = fits(&t, &req(SKIN, "€", 100.0));
    assert_eq!(l.glyphs.len(), 1);
    assert_eq!((l.glyphs[0].face, l.glyphs[0].glyph), (0, 0));
    assert_eq!(l.size, [50, 70]);
}

/// The ink top of `A` (700 units tall) at em 100 in a face of `spec`.
fn ink_top(spec: &Spec) -> f32 {
    let t = producer_of(spec).unwrap();
    fits(&t, &req(SKIN, "A", 100.0)).ink[1]
}

#[test]
fn cap_height_variants() {
    let mut spec = font_support::latin("Cap");
    // A positive sCapHeight: 720, so A's top is 0.02 em below the box top.
    spec.os2 = Os2::V4 { cap_height: 720 };
    assert!((ink_top(&spec) - 0.02).abs() < 1e-5);
    // Zero, version 1, absent: the face's own H, 700.
    for os2 in [Os2::V4 { cap_height: 0 }, Os2::V1, Os2::Absent] {
        spec.os2 = os2;
        assert!(ink_top(&spec).abs() < 1e-5, "{os2:?}");
    }
    // No H either: the ascender, 800.
    spec.cmap.retain(|(c, _)| *c != 'H');
    assert!((ink_top(&spec) - 0.1).abs() < 1e-5);
}

#[test]
fn glyph_cap_refuses() {
    // `M` substitutes eight `I`s: 128 Ms lay out 1 024 glyphs, 129 more.
    let mut spec = font_support::latin("Many");
    let i = font_support::capital(8);
    spec.multiple = Some((font_support::capital(12), vec![i; 8]));
    let t = producer_of(&spec).unwrap();
    let ok = fits(&t, &req(SKIN, &"M".repeat(128), 4.0));
    assert_eq!(ok.glyphs.len(), MAX_SHAPED_GLYPHS);
    let more = req(SKIN, &"M".repeat(129), 4.0);
    assert!(more.text.len() <= skin_model::MAX_TEXT_BYTES);
    assert_eq!(
        t.layout_line(&more, 4.0, BOUNDS),
        LineFit::Refused(TextNote::TooComplex)
    );
}

#[test]
fn glyph_bounds_refuse() {
    // `X` is 32 em tall: past the line's 1 024 pixels at 100, before any
    // of it reaches the rasterizer.
    let mut spec = font_support::latin("Tall");
    let x = usize::from(font_support::capital(23));
    spec.glyphs[x] = Glyph {
        advance: 700,
        rect: Some([50, 0, 650, 32_000]),
    };
    let t = producer_of(&spec).unwrap();
    let before = raster_calls();
    assert_eq!(
        t.layout_line(&req(SKIN, "AX", 100.0), 100.0, BOUNDS),
        LineFit::Refused(TextNote::TooLarge)
    );
    assert_eq!(raster_calls(), before);
}

#[test]
fn refit_to_bounds() {
    // 256 bytes of W at em 256: far wider than 4 080 pixels, set smaller.
    let t = producer_of(&font_support::latin("Wide")).unwrap();
    let r = req(SKIN, &"W".repeat(256), 256.0);
    let l = fits(&t, &r);
    assert!(
        l.size[0] <= BOUNDS.w && l.size[0] > BOUNDS.w * 9 / 10,
        "{:?}",
        l.size
    );
    assert!(l.raster_em < 256.0);
    raster(&t, &l);
}

#[test]
fn font_errors_and_no_runtime_font() {
    // Bytes that only start like a font.
    let mut bad = vec![0, 1, 0, 0];
    bad.extend_from_slice(&[0xFF; 64]);
    let (_dir, skin) = skin_with(bad);
    assert!(matches!(
        outline_support::producer(&skin, &default_font()),
        Err(OutlineError::Font { font: Some(0), .. })
    ));
    // A font the skin's texts use but that was not read.
    let (_dir, skin) = skin_with(font_support::build(&font_support::latin("L")));
    assert!(matches!(
        OutlineText::new(&skin, Vec::new(), &default_font()),
        Err(OutlineError::Font { font: Some(0), .. })
    ));
    // No runtime font in the folder.
    let empty = tempfile::tempdir().unwrap();
    assert!(matches!(
        DefaultFont::from_dir(empty.path()),
        Err(OutlineError::NoDefaultFont)
    ));
    // A default font without the family.
    let data = font_support::build(&font_support::latin("Other"));
    assert!(matches!(
        DefaultFont::from_data(data, "Yu Gothic UI"),
        Err(OutlineError::Font { font: None, .. })
    ));
}

/// A closed square from (2, 2) to (6, 6).
fn square() -> (Vec<Point>, Vec<Verb>) {
    let p = [[2.0, 2.0], [6.0, 2.0], [6.0, 6.0], [2.0, 6.0]].map(|[x, y]| Point::new(x, y));
    let v = vec![
        Verb::MoveTo,
        Verb::LineTo,
        Verb::LineTo,
        Verb::LineTo,
        Verb::Close,
    ];
    (p.to_vec(), v)
}

#[test]
fn unchecked_points_never_reach_the_rasterizer() {
    let (points, verbs) = square();
    let mut mask = vec![0; 64];
    let before = raster_calls();
    rasterize_checked(&points, &verbs, [8, 8], &mut mask).unwrap();
    assert_eq!(raster_calls(), before + 1);
    assert!(mask.iter().any(|&c| c > 0));
    // A NaN among valid points, infinities, and values at the f32 to i32
    // conversion's edge (zeno multiplies by 256): each refused unrendered.
    let bad = [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        i32::MAX as f32 / 256.0,
        i32::MIN as f32 / 256.0,
        8.0 + RASTER_MARGIN + 1.0,
    ];
    for v in bad {
        for at in 0..points.len() {
            for axis in 0..2 {
                let mut p = points.clone();
                if axis == 0 {
                    p[at].x = v;
                } else {
                    p[at].y = v;
                }
                let mut out = Vec::new();
                assert_eq!(
                    place_points(&p, [0.0, 8.0], Window::image([8, 8]), &mut out),
                    None,
                    "{v} at point {at} axis {axis}"
                );
                assert_eq!(
                    rasterize_checked(&p, &verbs, [8, 8], &mut mask),
                    Err(RasterError::Outline),
                    "{v} at point {at} axis {axis}"
                );
            }
        }
        // A pen that is not finite.
        let mut out = Vec::new();
        assert_eq!(
            place_points(&points, [v, 0.0], Window::image([8, 8]), &mut out),
            None
        );
    }
    // A mask of another size than given, or too large.
    assert_eq!(
        rasterize_checked(&points, &verbs, [8, 9], &mut mask),
        Err(RasterError::Size)
    );
    assert_eq!(
        rasterize_checked(&points, &verbs, [0, 8], &mut []),
        Err(RasterError::Size)
    );
    assert_eq!(raster_calls(), before + 1, "a refused outline was rendered");
}

#[test]
fn the_probe_and_rendering_share_the_checked_helper() {
    let before = raster_calls();
    let t = producer_of(&font_support::latin("Probe")).unwrap();
    assert!(raster_calls() > before, "the probes rasterized nothing");
    // A space has no outline: no ink, nothing rasterized for it.
    let l = fits(&t, &req(SKIN, "A B", 100.0));
    let calls = raster_calls();
    let out = raster(&t, &l);
    assert_eq!(raster_calls(), calls + 2);
    assert!(out.iter().filter(|&&c| c == 255).count() > 1_000);
}

#[test]
fn a_variable_face_is_drawn_where_it_is_shaped() {
    // wght from 100 to 900, default 700: cosmic-text shapes at 400, half
    // way to the minimum, where H's top is raised by 150 of its 300 units.
    // With no OS/2 cap height, the cap height is that H's top: 850.
    let mut spec = font_support::latin("Variable");
    spec.os2 = Os2::V1;
    spec.variation = Some(font_support::Variation {
        min: 100,
        default: 700,
        max: 900,
        glyph: font_support::capital(7),
        rise: 300,
    });
    let t = producer_of(&spec).unwrap();
    let h = fits(&t, &req(SKIN, "H", 100.0));
    assert_eq!(h.size[1], 85, "H at wght 400: {h:?}");
    // The box top is that cap height, so H starts at it.
    assert!(h.ink[1].abs() < 1e-5, "{h:?}");
    let out = raster(&t, &h);
    let w = h.size[0] as usize;
    // Its last column is partly covered.
    assert!(
        out[..w - 1].iter().all(|&c| c > 200),
        "the raised top is drawn"
    );
    // A, not varied, sits 0.15 em below that box top.
    let a = fits(&t, &req(SKIN, "A", 100.0));
    assert_eq!(a.size[1], 70);
    assert!((a.ink[1] - 0.15).abs() < 1e-5, "{a:?}");
}

#[test]
fn a_constant_mvar_cap_stays_at_the_box_top() {
    fn words(xs: &[u16]) -> Vec<u8> {
        xs.iter().copied().flat_map(u16::to_be_bytes).collect()
    }
    let mut spec = font_support::latin("Mvar");
    let mut fvar = words(&[1, 0, 16, 2, 1, 20, 0, 8]);
    fvar.extend_from_slice(b"wght");
    for value in [100_i32, 700, 900] {
        fvar.extend_from_slice(&(value << 16).to_be_bytes());
    }
    fvar.extend(words(&[0, 256]));
    let n = u16::try_from(spec.glyphs.len()).unwrap();
    let start = 20 + 4 * (u32::from(n) + 1);
    let mut gvar = words(&[1, 0, 1, 0]);
    gvar.extend_from_slice(&start.to_be_bytes());
    gvar.extend(words(&[n, 1]));
    gvar.extend_from_slice(&start.to_be_bytes());
    gvar.resize(usize::try_from(start).unwrap(), 0);
    let mvar = words(&[
        1, 0, 0, 8, 1, 20, 0x6370, 0x6874, 1, 0, 1, 0, 16, 2, 0, 26, 0, 38, 1, 1, 0xc000, 0xc000,
        0, 2, 1, 1, 0, 0, 200, 1, 1, 1, 0, 0,
    ]);
    spec.extra.extend([
        (*b"fvar", fvar),
        (*b"gvar", gvar.clone()),
        (*b"MVAR", mvar.clone()),
    ]);
    let t = producer_of(&spec).unwrap();
    let a = fits(&t, &req(SKIN, "A", 100.0));
    assert_eq!(a.size, [30, 70]);
    assert!(a.ink[1].abs() < 1e-5, "{:?}", a.ink);
    // The selected delta raised to 300 at the axis's minimum: at wght
    // 400, half way there, the cap height is 850, so A starts 0.15 em
    // below the box top.
    let mut raised = mvar;
    raised[66..68].copy_from_slice(&300_i16.to_be_bytes());
    let mvar_at = spec.extra.iter().position(|(t, _)| t == b"MVAR").unwrap();
    spec.extra[mvar_at].1 = raised;
    let t = producer_of(&spec).unwrap();
    let a = fits(&t, &req(SKIN, "A", 100.0));
    assert_eq!(a.size, [30, 70]);
    assert!((a.ink[1] - 0.15).abs() < 1e-5, "{:?}", a.ink);
}
