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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{GAUGES, context, input, offscreen, read_back, view};
use outline_support::{FontRec, Value, default_font, distance_field, text, ttf};
use render::text::MenuText;
use render::{
    AttemptInput, AttemptText, FillStep, LabelBook, LabelJob, LabelStore, LabelWorker, LineBounds,
    LineFit, LineLayout, OutlineText, Renderer, Settled, SkinBundle, SkinError, TextResidency,
    text_upload_bound,
};
use skin_model::{ChartText, DecisionStrings, ResultLabels, ScreenKind, TextRequest, TextSlots};

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

/// The producer, counting the lines it lays out and rasterizes.
#[derive(Debug)]
struct Counting {
    inner: OutlineText,
    layouts: AtomicU32,
    rasters: AtomicU32,
}

impl AttemptText for Counting {
    fn layout(&self, req: &TextRequest, em_px: f32, bounds: LineBounds) -> LineFit {
        self.layouts.fetch_add(1, Ordering::SeqCst);
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
            layouts: AtomicU32::new(0),
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
        continuation: skin_model::Continuation::Fresh,
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
    // The scratch holds the A8 line and the rectangle's RGBA levels,
    // never the mask: 4 080 × 1 024 and 4 096 × 1 096 × 4 × 85 / 64.
    assert_eq!(m.text_scratch, 4_177_920 + 23_848_960);
    // The mask counts once in the total, apart from every other term.
    let without = render::MemoryWitness { text_mask: 0, ..m };
    assert_eq!(m.total(), without.total() + render::MAX_TEXT_MASK_BYTES);

    // The decision labels' terms: a published slot's content, a fill's
    // last boundary of writes, a waiting output's levels and the worker's
    // buffers, each in its own term and none in the renderer's.
    let decision = outline_support::decision_folder(
        &[("ttf.ttf", font_support::build(&font_support::latin("Skin")))],
        &[ttf()],
        &[
            text("f", 18, Value::Ref(10), [100, 100, 1_200, 18]),
            text("f", 24, Value::Ref(16), [100, 300, 1_200, 24]),
        ],
    );
    let db = outline_support::decision_bundle_in(&r, decision.path(), &default);
    let token = r.install(ScreenKind::Decision, &db, 2).unwrap();
    r.commit(token).unwrap();
    let before = r.memory();
    let plan = Arc::new(r.decision_rows().unwrap().unwrap());
    let (tx, rx) = mpsc::channel();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = tx.send(());
    });
    let worker = LabelWorker::for_bundle(&db, Some(wake)).unwrap();
    let mut store = LabelStore::new(LabelBook::shared([1_920, 1_080]));
    let job = |generation: u64, title: &str| {
        LabelJob::new(
            generation,
            title.into(),
            1,
            DecisionStrings::new(&ResultLabels {
                title: title.to_owned(),
                artist: "AB".to_owned(),
                ..ResultLabels::default()
            }),
            Arc::clone(&plan),
        )
    };
    let staged = |store: &mut LabelStore, r: &Renderer, generation: u64, title: &str| {
        store.post(&worker, job(generation, title), &mut || {
            r.watch_completion()
        });
        rx.recv_timeout(Duration::from_secs(60)).unwrap();
        worker.take().unwrap().unwrap()
    };
    // Each output is claimed before the next job is posted: a newer post
    // supersedes every older preparation.
    let a = staged(&mut store, &r, 1, "A");
    store.offer(a);
    store.claim().unwrap();
    while r.write_decision_labels(&mut store) == FillStep::Wrote {}
    let _ = r.gpu().device().poll(wgpu::PollType::wait_indefinitely());
    assert!(matches!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Published { .. }
    ));
    let bo = staged(&mut store, &r, 2, "B");
    store.offer(bo);
    store.claim().unwrap();
    assert_eq!(r.write_decision_labels(&mut store), FillStep::Complete);
    // A waiting output beside the fill: staged on this thread, as a
    // rebuild's restore stages, since a post would dispose of the fill.
    store.offer(worker.restage(&db, job(3, "C")).unwrap().unwrap());
    let d = store.memory(Some(&worker));
    assert!(d.labels > 0 && d.staged > 0 && d.upload > 0, "{d:?}");
    assert!(d.staged <= plan.staged_bound(), "{d:?}");
    assert!(d.upload <= render::DECISION_TEXT_WRITE_BUDGET);
    assert!(d.text_mask >= plan.mask_bound(), "{d:?}");
    let after = r.memory().with_decision(d);
    assert_eq!(
        (before.decision_labels, before.decision_staged),
        (0, 0),
        "the renderer's own witness leaves them out"
    );
    assert_eq!(
        (after.text_scratch, after.text_mask, after.text_upload),
        (before.text_scratch, before.text_mask, before.text_upload)
    );
    assert_eq!(after.text_cache, before.text_cache);
    assert_eq!(
        after.total(),
        before.total() + d.labels + d.staged + d.text_mask + d.upload
    );
}

/// A face whose `A` to `F` are blocks 7.8 em wide and 3.5 em tall less
/// 0.2 em a letter on: at em 256 two of them fill a 2048 layer, and each
/// letter draws its own height.
fn steps() -> Vec<u8> {
    let mut s = font_support::latin("Steps");
    for n in 0..6_u16 {
        s.glyphs[usize::from(font_support::capital(n))] = font_support::Glyph {
            advance: 7_850,
            rect: Some([0, 0, 7_800, 3_500 - 200 * i16::try_from(n).unwrap()]),
        };
    }
    font_support::build(&s)
}

/// A skin of a title and an artist in [`steps`] at em 256.
fn step_skin() -> tempfile::TempDir {
    outline_support::folder(
        &[("ttf.ttf", steps())],
        &[ttf()],
        &[
            text("f", 256, Value::Ref(10), [0, 0, 100, 256]),
            text("f", 256, Value::Ref(16), [0, 400, 100, 256]),
        ],
    )
}

/// A loading frame of lifetime 1 drawn and read back.
fn loading_frame(r: &mut Renderer, menu: &MenuText) -> Vec<u8> {
    common::loading(r, menu, &common::loading_frame(1, TimeUs(0), TimeUs(0)))
}

/// The loading text of `text` prepared for lifetime 1 at `revision`, every
/// slot.
fn prepare_loading(r: &mut Renderer, revision: u64, text: &ChartText<'_>) {
    r.prepare_loading_text(1, revision, TextSlots::ALL, text)
        .unwrap();
}

/// The attempt staged on the installed resources keeps its lines while
/// the loading text is prepared under pressure: with the installed
/// attempt's lines filling the layer, the loading lines find no room at
/// the full em, evict nothing and are set smaller; the installed attempt
/// still draws untouched, and once committed the staged attempt draws as
/// it does where no loading text was prepared.
#[test]
fn attempt_pin_survives_loading() {
    let dir = step_skin();
    let mut r = offscreen(1_920, 1_080);
    let b = outline_support::bundle_in(&r, dir.path(), &default_font());
    let token = stage(&mut r, &b, &labels("A", "B"), 1);
    r.commit(token).unwrap();
    let installed = frame(&mut r);
    let staged = stage(&mut r, &b, &labels("C", "D"), 2);
    assert!(r.text_report().counts.rounds > 1);

    r.enter_loading(1);
    prepare_loading(&mut r, 1, &labels("E", "F"));
    let report = r.text_report();
    assert!(report.notes.is_empty(), "{:?}", report.notes);
    assert_eq!(report.counts.evictions, 0, "{:?}", report.counts);
    assert!(report.counts.rounds > 1, "{:?}", report.counts);
    assert_eq!(report.counts.placements, 2);
    assert_eq!(frame(&mut r), installed);
    r.commit(staged).unwrap();
    let committed = frame(&mut r);

    let mut q = offscreen(1_920, 1_080);
    let qb = outline_support::bundle_in(&q, dir.path(), &default_font());
    let token = stage(&mut q, &qb, &labels("A", "B"), 1);
    q.commit(token).unwrap();
    let token = stage(&mut q, &qb, &labels("C", "D"), 2);
    q.commit(token).unwrap();
    assert_eq!(committed, frame(&mut q));
}

/// A staging on its own resources draws its loading text before its
/// commit, and the commit moves those lines into the installed loading
/// lines with the cache they are in: the same resources (the key, which
/// names them, unchanged),
/// nothing to install again, the same frame drawn; loading text prepared
/// after it under pressure evicts none of them.
#[test]
fn staged_loading_lines_move_with_the_commit() {
    let dir = step_skin();
    let mut r = offscreen(1_920, 1_080);
    let menu = MenuText::new(&r);
    let a = outline_support::bundle_in(&r, dir.path(), &default_font());
    common::install(&mut r, &a, 1);
    let b = outline_support::bundle_in(&r, dir.path(), &default_font());
    let staged = stage(&mut r, &b, &labels("C", "D"), 2);
    let on = render::LoadingOn::Staged(staged);
    let staged_frame = |r: &mut Renderer| {
        let f = common::loading_frame(1, TimeUs(0), TimeUs(0));
        let t = r.loading_frame_on(on, &f, &menu).unwrap().unwrap();
        assert!(t.presented);
        read_back(r)
    };
    // The first staged frame enters the lifetime.
    staged_frame(&mut r);
    r.prepare_loading_text_on(on, 1, 1, TextSlots::ALL, &labels("A", "B"))
        .unwrap();
    assert_eq!(r.install_staged_loading_text_on(on), Ok(true));
    let drawn = staged_frame(&mut r);
    assert!(
        lit(&drawn).is_some(),
        "the staged loading lines drew nothing"
    );
    let key = r.loading_text_key_on(on);
    assert!(key.is_some() && key != r.loading_text_key());

    r.commit(staged).unwrap();
    assert_eq!(r.loading_text_key(), key);
    assert_eq!(r.install_staged_loading_text(), Ok(false));
    assert_eq!(loading_frame(&mut r, &menu), drawn);

    // Under pressure: the attempt's and the loading lines fill the layer.
    prepare_loading(&mut r, 2, &labels("E", "F"));
    let report = r.text_report();
    assert_eq!(report.counts.evictions, 0, "{:?}", report.counts);
    assert!(report.counts.rounds > 1, "{:?}", report.counts);
    assert_eq!(loading_frame(&mut r, &menu), drawn);
}

/// The committed bundle restaged for another chart (a reused
/// specialization) draws its staged loading frames from the installed
/// resources, nothing made or uploaded (the resources' number unchanged),
/// with the next lifetime's own loading text: drawn once it is installed,
/// and the very presentation the installed frames draw.
#[test]
fn a_reused_specialization_draws_with_no_upload() {
    let dir = step_skin();
    let mut r = offscreen(1_920, 1_080);
    let menu = MenuText::new(&r);
    let b = outline_support::bundle_in(&r, dir.path(), &default_font());
    let token = stage(&mut r, &b, &labels("A", "B"), 1);
    r.commit(token).unwrap();
    let generation = r.resource_generation();
    let again = stage(&mut r, &b, &labels("C", "D"), 2);
    assert_eq!(r.resource_generation(), generation, "restaged in place");
    let on = render::LoadingOn::Staged(again);
    assert_eq!(r.loading_text_key_on(on), r.loading_text_key());
    let lifetime_2 = common::loading_frame(2, TimeUs(0), TimeUs(0));
    let staged = |r: &mut Renderer| {
        let t = r.loading_frame_on(on, &lifetime_2, &menu).unwrap().unwrap();
        assert!(t.presented);
        read_back(r)
    };
    let before = staged(&mut r);
    r.prepare_loading_text_on(on, 2, 1, TextSlots::ALL, &labels("Beta", ""))
        .unwrap();
    assert_eq!(r.install_staged_loading_text_on(on), Ok(true));
    let drawn = staged(&mut r);
    assert!(lit(&drawn).is_some());
    assert_ne!(drawn, before, "the lifetime's own text drawn");
    assert_eq!(r.resource_generation(), generation);
    assert_eq!(
        common::loading(&mut r, &menu, &lifetime_2),
        drawn,
        "the installed presentation"
    );
}

/// The staged loading text and then the installed loading lines keep
/// their lines while an attempt is staged under pressure: with the
/// loading lines filling the layer, the attempt's lines evict none of
/// them and are set smaller, and the loading frame draws the loading
/// lines as it does where no attempt was staged.
#[test]
fn loading_pins_survive_attempt() {
    let dir = step_skin();
    let mut r = offscreen(1_920, 1_080);
    let menu = MenuText::new(&r);
    let b = outline_support::bundle_in(&r, dir.path(), &default_font());
    common::install(&mut r, &b, 1);
    r.enter_loading(1);
    prepare_loading(&mut r, 1, &labels("A", "B"));
    assert_eq!(r.text_report().counts.rounds, 1);

    // Staged: held by the staged text.
    stage(&mut r, &b, &labels("C", "D"), 2);
    let report = r.text_report();
    assert!(report.notes.is_empty(), "{:?}", report.notes);
    assert_eq!(report.counts.evictions, 0, "{:?}", report.counts);
    assert!(report.counts.rounds > 1, "{:?}", report.counts);
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    let drawn = loading_frame(&mut r, &menu);
    assert!(lit(&drawn).is_some(), "the loading lines drew nothing");

    // Installed: held by the installed lines. The attempt staged before
    // is replaced, so its lines may go, and only they.
    stage(&mut r, &b, &labels("E", "F"), 3);
    let report = r.text_report();
    assert!(report.notes.is_empty(), "{:?}", report.notes);
    assert!(report.counts.rounds > 1, "{:?}", report.counts);
    assert_eq!(loading_frame(&mut r, &menu), drawn);

    let mut q = offscreen(1_920, 1_080);
    let qmenu = MenuText::new(&q);
    let qb = outline_support::bundle_in(&q, dir.path(), &default_font());
    common::install(&mut q, &qb, 1);
    q.enter_loading(1);
    prepare_loading(&mut q, 1, &labels("A", "B"));
    assert_eq!(q.install_staged_loading_text(), Ok(true));
    assert_eq!(drawn, loading_frame(&mut q, &qmenu));
}

/// Loading lines set at a smaller em under pressure are found again by
/// the attempt of the same labels through their aliases: in its first
/// round, every request a hit, nothing laid out or rasterized again.
#[test]
fn loading_to_attempt_reuse_after_refit() {
    let dir = step_skin();
    let mut r = offscreen(1_920, 1_080);
    let (b, c) = counted(&r, dir.path());
    let token = stage(&mut r, &b, &labels("A", "B"), 1);
    r.commit(token).unwrap();
    r.enter_loading(1);
    prepare_loading(&mut r, 1, &labels("C", "D"));
    let refit = r.text_report().counts;
    assert!(refit.rounds > 1, "{refit:?}");
    assert_eq!(refit.placements, 2);
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    let (layouts, rasters) = (
        c.layouts.load(Ordering::SeqCst),
        c.rasters.load(Ordering::SeqCst),
    );

    let token = stage(&mut r, &b, &labels("C", "D"), 2);
    let counts = r.text_report().counts;
    assert_eq!(
        (counts.rounds, counts.hits, counts.placements),
        (1, 2, 0),
        "{counts:?}"
    );
    assert_eq!(c.layouts.load(Ordering::SeqCst), layouts);
    assert_eq!(c.rasters.load(Ordering::SeqCst), rasters);
    r.commit(token).unwrap();
    assert!(lit(&frame(&mut r)).is_some());
}

/// A cleared loading slot lets go of its line, and only that slot: with
/// the loading lines filling the layer and the title cleared, an attempt
/// under pressure evicts the title's line and never the artist's, which
/// its slot still draws. Replaced resources start with nothing held: once
/// the installation is restored, the installed text layer's cache and
/// holders hold no bytes.
#[test]
fn cleared_loading_slots_release_their_lines() {
    let dir = step_skin();
    let mut r = offscreen(1_920, 1_080);
    let b = outline_support::bundle_in(&r, dir.path(), &default_font());
    common::install(&mut r, &b, 1);
    r.enter_loading(1);
    prepare_loading(&mut r, 1, &labels("A", "B"));
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    r.clear_loading_text(TextSlots::field(skin_model::TextField::Title));
    stage(&mut r, &b, &labels("C", "D"), 2);
    let counts = r.text_report().counts;
    assert_eq!(counts.evictions, 1, "{counts:?}");
    assert!(r.memory().text_cache > 0);

    let installation = r.installation(skin_model::ScreenKind::Play).unwrap();
    r.restore(skin_model::ScreenKind::Play, &installation)
        .unwrap();
    assert_eq!(r.memory().text_cache, 0);
}

/// A replaced loading slot lets go of its old line, and only that slot:
/// with the loading lines filling the layer, the title's text replaced by
/// another installed, an attempt under pressure evicts the old title's
/// line and never the artist's or the new title's: the loading frame draws
/// as before the attempt, and both lines are hits when asked again,
/// nothing laid out.
#[test]
fn replaced_loading_slots_release_their_lines() {
    let dir = step_skin();
    let mut r = offscreen(1_920, 1_080);
    let menu = MenuText::new(&r);
    let (b, c) = counted(&r, dir.path());
    common::install(&mut r, &b, 1);
    r.enter_loading(1);
    prepare_loading(&mut r, 1, &labels("A", "B"));
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    let title = TextSlots::field(skin_model::TextField::Title);
    r.prepare_loading_text(1, 2, title, &labels("C", "B"))
        .unwrap();
    assert_eq!(r.text_report().counts.evictions, 0);
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    let drawn = loading_frame(&mut r, &menu);
    assert!(lit(&drawn).is_some());

    stage(&mut r, &b, &labels("D", "E"), 2);
    let counts = r.text_report().counts;
    assert_eq!(counts.evictions, 1, "{counts:?}");
    assert_eq!(loading_frame(&mut r, &menu), drawn);
    let layouts = c.layouts.load(Ordering::SeqCst);
    prepare_loading(&mut r, 3, &labels("C", "B"));
    let counts = r.text_report().counts;
    assert_eq!((counts.hits, counts.placements), (2, 0), "{counts:?}");
    assert_eq!(c.layouts.load(Ordering::SeqCst), layouts);
}
