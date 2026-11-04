//! Rasterized text on WARP through the production path: self-made JSON
//! skins naming generated fonts ([`font_support`]) and distance-field
//! records, imported, their fonts read by a `FontReader`, their producer
//! built with a generated default font, their atlas built with its text
//! layer and bundled, then staged, committed and drawn ([`outline_support`]).
//! A counting decorator around the producer, here in the tests, counts the
//! lines rasterized.
//!
//! The skins are a fixed 1920×1080 canvas on a 1920×1080 target, so canvas
//! pixels are target pixels.

mod common;
mod font_support;
mod outline_support;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{GAUGES, context, input, offscreen, read_back, view};
use outline_support::{FontRec, Value, default_font, distance_field, text, ttf};
use render::{
    AttemptInput, AttemptText, LineBounds, LineFit, LineLayout, OutlineText, Renderer, SkinBundle,
    SkinError, TextResidency, text_upload_bound,
};
use skin_model::{ChartText, TextRequest};

fn chart() -> Chart {
    ChartBuilder::new(120_000).build().unwrap()
}

fn labels<'a>(title: &'a str, artist: &'a str) -> ChartText<'a> {
    ChartText {
        title,
        subtitle: "",
        artist,
        subartist: "",
        genre: "",
    }
}

/// The producer, counting the lines it rasterizes.
#[derive(Debug)]
struct Counting {
    inner: OutlineText,
    rasters: AtomicU32,
}

impl AttemptText for Counting {
    fn layout(&self, req: &TextRequest, em_px: f32, bounds: LineBounds) -> LineFit {
        self.inner.layout(req, em_px, bounds)
    }

    fn raster(&self, layout: &LineLayout, out: &mut [u8]) -> Result<(), SkinError> {
        self.rasters.fetch_add(1, Ordering::SeqCst);
        self.inner.raster(layout, out)
    }

    fn resident(&self) -> TextResidency {
        self.inner.resident()
    }
}

/// The bundle of `dir` with a counting producer.
fn counted(r: &Renderer, dir: &Path) -> (Arc<SkinBundle>, Arc<Counting>) {
    let default = default_font();
    let mut kept = None;
    let b = outline_support::bundle_with(r, dir, |skin| {
        let c = Arc::new(Counting {
            inner: outline_support::producer(skin, &default).unwrap(),
            rasters: AtomicU32::new(0),
        });
        kept = Some(Arc::clone(&c));
        c
    });
    (b, kept.unwrap())
}

/// Stages `b` with an attempt labelled `text` for `request`.
fn stage(
    r: &mut Renderer,
    b: &Arc<SkinBundle>,
    text: &ChartText<'_>,
    request: u64,
) -> render::PreparedToken {
    let chart = chart();
    let ctx = context(&GAUGES, false);
    let a = AttemptInput {
        chart: &chart,
        context: &ctx,
        text,
        target: r.target(),
    };
    r.stage(b, &a, request).unwrap()
}

/// A play frame drawn and read back.
fn frame(r: &mut Renderer) -> Vec<u8> {
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert!(timing.presented);
    read_back(r)
}

/// The lit pixels' bounds `[left, top, right, bottom)` in a 1920-wide
/// read back, and their count.
fn lit(p: &[u8]) -> Option<([u32; 4], usize)> {
    let mut b: Option<[u32; 4]> = None;
    let mut n = 0;
    for (i, px) in p.as_chunks::<4>().0.iter().enumerate() {
        if px[..3].iter().all(|&c| c == 0) {
            continue;
        }
        n += 1;
        let (x, y) = (
            u32::try_from(i % 1_920).unwrap(),
            u32::try_from(i / 1_920).unwrap(),
        );
        b = Some(b.map_or([x, y, x + 1, y + 1], |[l, t, r, bo]| {
            [l.min(x), t.min(y), r.max(x + 1), bo.max(y + 1)]
        }));
    }
    b.map(|b| (b, n))
}

/// A skin titled in `ttf.ttf`, the Latin face, its box `[100, 100, 1000,
/// 100]`: em 100, the box's top the cap height.
fn title_skin() -> tempfile::TempDir {
    outline_support::folder(
        &[("ttf.ttf", font_support::build(&font_support::latin("Skin")))],
        &[ttf()],
        &[text("f", 100, Value::Ref(10), [100, 100, 1_000, 100])],
    )
}

/// An imported font's title draws its glyphs: `A` and `B` from the box's
/// top (the cap height, 0.7 em) down to the baseline, inside the box from
/// its left; another title draws another set of pixels.
#[test]
fn imported_ttf_text_draws() {
    let mut r = offscreen(1_920, 1_080);
    let dir = title_skin();
    let b = outline_support::bundle_in(&r, dir.path(), &default_font());
    let token = stage(&mut r, &b, &labels("AB", ""), 1);
    r.commit(token).unwrap();
    assert!(r.text_report().notes.is_empty());
    let (bounds, n) = lit(&frame(&mut r)).expect("the title drew nothing");
    // A from 5 to 35 (300 units wide), B from 45 + 5 to 76.2: 70 tall.
    let [l, t, right, bottom] = bounds;
    assert!((104..=106).contains(&l), "{bounds:?}");
    assert!(
        (99..=101).contains(&t) && (169..=171).contains(&bottom),
        "{bounds:?}"
    );
    assert!((176..=178).contains(&right), "{bounds:?}");
    assert!(n > 2 * 30 * 70 - 400, "{n}");
    let token = stage(&mut r, &b, &labels("W", ""), 2);
    r.commit(token).unwrap();
    let (other, _) = lit(&frame(&mut r)).unwrap();
    assert_ne!(other, bounds);
}

/// A distance-field font draws in the default font (D13): no `.fnt` file
/// exists, and the line sits lower by the default face's smaller cap
/// height (0.6 em).
#[test]
fn distance_field_font_draws_default() {
    let mut r = offscreen(1_920, 1_080);
    let dir = outline_support::folder(
        &[],
        &[distance_field()],
        &[text("d", 100, Value::Ref(10), [100, 100, 1_000, 100])],
    );
    assert!(!dir.path().join("d.fnt").exists());
    let b = outline_support::bundle_in(&r, dir.path(), &default_font());
    let token = stage(&mut r, &b, &labels("A", ""), 1);
    r.commit(token).unwrap();
    let ([_, t, _, bottom], _) = lit(&frame(&mut r)).expect("D13 drew nothing");
    // The capital is 0.7 em tall over a 0.6 em cap: from 0.1 em above the
    // box top.
    assert!(
        (89..=91).contains(&t) && (159..=161).contains(&bottom),
        "{t} {bottom}"
    );
}

/// The committed staging's cache comes with it: the next attempt of the
/// same titles rasterizes nothing.
#[test]
fn commit_adopts_cache() {
    let mut r = offscreen(1_920, 1_080);
    let dir = title_skin();
    let (b, c) = counted(&r, dir.path());
    let token = stage(&mut r, &b, &labels("AB", ""), 1);
    r.commit(token).unwrap();
    let rastered = c.rasters.load(Ordering::SeqCst);
    assert_eq!(rastered, 1);
    let token = stage(&mut r, &b, &labels("AB", ""), 2);
    r.commit(token).unwrap();
    assert_eq!(c.rasters.load(Ordering::SeqCst), rastered);
    assert_eq!(r.text_report().counts.hits, 1);
    assert!(lit(&frame(&mut r)).is_some());
}

/// A face whose `A` to `F` are blocks 7.8 em wide and 3.5 em tall: at em
/// 256 a line of one is 1997 × 896, so two fill a 2048 layer.
fn blocks() -> Vec<u8> {
    let mut s = font_support::latin("Blocks");
    for n in 0..6 {
        s.glyphs[usize::from(font_support::capital(n))] = font_support::Glyph {
            advance: 7_850,
            rect: Some([0, 0, 7_800, 3_500]),
        };
    }
    font_support::build(&s)
}

/// A skin of a title and an artist in [`blocks`] at em 256.
fn block_skin() -> tempfile::TempDir {
    outline_support::folder(
        &[("ttf.ttf", blocks())],
        &[FontRec { ..ttf() }],
        &[
            text("f", 256, Value::Ref(10), [0, 0, 100, 256]),
            text("f", 256, Value::Ref(16), [0, 400, 100, 256]),
        ],
    )
}

/// The installed attempt's lines are pinned while another is staged under
/// pressure: the staging of other lines finds no room at the full em,
/// evicts nothing, and is set at a smaller em after failed rounds, its
/// upload holding only its final placement; the installed frame still
/// draws its own lines untouched. After the staging is discarded, its
/// lines are no longer held: the next staging evicts them, and never the
/// installed ones.
#[test]
fn stage_then_commit_pins_attempt_and_discard_releases() {
    let mut r = offscreen(1_920, 1_080);
    let dir = block_skin();
    let b = outline_support::bundle_in(&r, dir.path(), &default_font());
    assert_eq!(b.side(), 2_048);
    let token = stage(&mut r, &b, &labels("A", "B"), 1);
    r.commit(token).unwrap();
    let installed = frame(&mut r);
    assert!(lit(&installed).is_some());
    let full = r.text_report().upload_bytes;

    stage(&mut r, &b, &labels("C", "D"), 2);
    let report = r.text_report();
    assert!(report.notes.is_empty(), "{:?}", report.notes);
    assert_eq!(report.counts.evictions, 0);
    assert!(report.counts.rounds >= 4, "{:?}", report.counts);
    assert_eq!(report.counts.placements, 2);
    // Only the final, small placement was staged for upload.
    assert!(
        report.upload_bytes > 0 && report.upload_bytes < full / 8,
        "{report:?}"
    );
    assert!(report.upload_bytes <= text_upload_bound(2_048));
    // The installed attempt still draws, pixel for pixel.
    assert_eq!(frame(&mut r), installed);

    r.discard();
    stage(&mut r, &b, &labels("E", "F"), 3);
    let report = r.text_report();
    assert!(report.counts.evictions >= 1, "{:?}", report.counts);
    assert_eq!(frame(&mut r), installed);
}

/// The witness's text terms: the bundle counts its skin font exactly, the
/// default font is counted once apart, and the cache, the scratch and the
/// upload each in their own term.
#[test]
fn witness_terms_are_disjoint() {
    let mut r = offscreen(1_920, 1_080);
    let dir = title_skin();
    let default = default_font();
    let b = outline_support::bundle_in(&r, dir.path(), &default);
    let token = stage(&mut r, &b, &labels("AB", ""), 1);
    r.commit(token).unwrap();
    let m = r.memory();
    let font = u64::try_from(font_support::build(&font_support::latin("Skin")).len()).unwrap();
    let resident = b.text_residency().unwrap();
    assert_eq!(resident.skin_fonts, font);
    assert_eq!(m.default_font, default.bytes());
    assert_eq!(m.default_font, resident.default_font);
    assert!(m.committed_cpu >= font);
    assert!(m.text_cache > 0 && m.text_scratch > 0);
    assert_eq!(m.text_upload, r.text_report().upload_bytes);
    assert_eq!(m.staged_text_cache, 0);
    assert_eq!(m.text_mask, render::MAX_TEXT_MASK_BYTES);
    assert_eq!(render::MAX_TEXT_MASK_BYTES, 4_177_920);
}
