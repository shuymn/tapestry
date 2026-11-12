//! The result skin's chart images (design.md "Rendering and Skins",
//! "Load Pipeline"): which fit a view's bundle draws ([`ResultSkin::chart_for`]),
//! the derivations its layouts make from the attempt's sources and who
//! owns them on each layout outcome, the reuse decisions they bound, and
//! a pending skin whose atlas gives up its chart layer.

use std::fs;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use skin_beatoraja::JudgeClass;
use skin_model::AtlasLimits;
use skin_model::chart_image::ChartImage;

use super::tests::{LIMITS, pending, staged_at, view};
use super::{Published, ResultSkin, ResultStart, ViewFacts, builds};
use crate::chart_images::{ChartImages, ResultImages, Want, derive_set, read_sources};
use crate::imports::tests::{jobs_started, serial};

/// `dir/name`, a `w`×`h` PNG of one colour `rgb`.
fn png(dir: &Path, name: &str, [w, h]: [u32; 2], [r, g, b]: [u8; 3]) {
    let file = fs::File::create(dir.join(name)).unwrap();
    let mut e = png::Encoder::new(BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels = [r, g, b, 255].repeat(usize::try_from(w * h).unwrap());
    e.write_header().unwrap().write_image_data(&texels).unwrap();
}

/// A result skin `s.luaskin` running `reads` then `pre` while it loads,
/// with the image list `images` (a Lua expression) over `s.png` (64×16),
/// `w1.png` (2 200×8) and `w2.png` (4 000×8), each image drawn as a
/// status dot (on option 90), its EX
/// number, and the chart's stage file (−100) and back image (−101).
fn chart_skin(reads: &str, pre: &str, images: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    png(d.path(), "s.png", [64, 16], [9, 9, 9]);
    png(d.path(), "w1.png", [2_200, 8], [9, 9, 9]);
    png(d.path(), "w2.png", [4_000, 8], [9, 9, 9]);
    let entry = format!(
        "local skin = {{ type = 7, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         {reads}\n\
         {pre}\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }}, {{ id = \"w1\", path = \"w1.png\" }}, \
                          {{ id = \"w2\", path = \"w2.png\" }} }}\n\
         skin.image = {images}\n\
         skin.value = {{ {{ id = \"ex\", src = \"s\", x = 0, y = 8, w = 40, h = 8, divx = 10, \
                           digit = 4, ref = 71 }} }}\n\
         local d = {{ {{ id = \"ex\", dst = {{ {{ x = 0, y = 900, w = 4, h = 8 }} }} }}, \
                      {{ id = -100, dst = {{ {{ x = 0, y = 0, w = 640, h = 480 }} }} }}, \
                      {{ id = -101, dst = {{ {{ x = 640, y = 0, w = 512, h = 384 }} }} }} }}\n\
         for i, im in ipairs(skin.image) do\n\
           d[#d + 1] = {{ id = im.id, op = {{ 90 }}, dst ={{ {{ x = 0, y = 500 + 20 * i, w = 8, h = 8 }} }} }}\n\
         end\n\
         skin.destination = d\n\
         return skin\n"
    );
    fs::write(d.path().join("s.luaskin"), entry).unwrap();
    d
}

/// One dot of `s.png`.
const DOT: &str = "{ { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }";

/// A skin reading 154 whose probe (154 = 1) is refused, so the image
/// stage prepares nothing and every view's layout packs its own atlas
/// (side 2 048, the chart layer's least); 154 = 8 is refused.
fn unprepared_skin() -> tempfile::TempDir {
    chart_skin(
        "local nr = ms.number(154)",
        "if nr == 1 then error(\"the probe\") end\nif nr == 8 then error(\"eight\") end",
        DOT,
    )
}

/// A chart folder with a `stage` stage file (`stage.png`) and a 512×384
/// back image (`back.png`).
fn chart_dir(stage: [u32; 2]) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    png(d.path(), "stage.png", stage, [200, 40, 10]);
    png(d.path(), "back.png", [512, 384], [10, 60, 220]);
    d
}

/// What an attempt hands the result at its commit: `dir`'s images
/// decoded into sources, and their result half for chart layers of side
/// `side` when it is one (the worker's `take_result`, from the same
/// production functions).
fn handed(dir: &Path, side: Option<u32>) -> ResultImages {
    let want = Want {
        dir: dir.to_path_buf(),
        names: ["stage.png".to_owned(), "back.png".to_owned()],
        play: None,
        result: side,
    };
    let sources = Arc::new(read_sources(&want, None, &|| false));
    assert!(sources.facts().stage && sources.facts().back);
    let half = side.map(|s| Arc::new(derive_set(&sources, s, &|| false).unwrap()));
    ResultImages {
        sources,
        half,
        side,
    }
}

/// The content size of `set`'s `which`.
fn content(set: &ChartImages, which: ChartImage) -> [u32; 2] {
    set.read(which).image.as_ref().unwrap().content()
}

/// Polls `rs` until its jobs are done, at most 20 s.
fn settled(rs: &mut ResultSkin) {
    let end = Instant::now() + Duration::from_secs(20);
    while !rs.poll() {
        assert!(Instant::now() < end, "the layout did not land");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The published bundle.
fn shown(rs: &ResultSkin) -> Arc<render::SkinBundle> {
    match rs.published().0 {
        Some(Published::Skin(b, _, _)) => Arc::clone(b),
        other => panic!("{other:?}"),
    }
}

/// The cached layout's derivation: the id of the sources it came from,
/// and its images.
fn cached_derivation(rs: &ResultSkin) -> Option<(u64, Arc<ChartImages>)> {
    rs.layout.as_ref().and_then(|l| l.derived.clone())
}

/// Every derivation `rs`'s specializations own.
fn derivations(rs: &ResultSkin) -> usize {
    rs.layout
        .iter()
        .chain(&rs.presented)
        .filter(|l| l.derived.is_some())
        .count()
}

/// A pending result skin in `d` with `limits`, its image stage run at
/// level 12.
fn staged_skin(d: &Path, limits: AtlasLimits) -> ResultSkin {
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(pending(d, limits))));
    staged_at(&mut rs, 12, JudgeClass::Normal);
    rs
}

/// A pending skin with no prepared entry asks the
/// worker for no result side; its view's layout derives the images from
/// the sources at its own chart slot, owned by the cached layout and
/// published with its bundle: `chart_for` gives the stage file and the
/// back image at their native sizes.
#[test]
fn a_layout_with_no_prepared_entry_derives_from_the_sources() {
    let _s = serial();
    let d = unprepared_skin();
    let c = chart_dir([640, 480]);
    let mut rs = staged_skin(d.path(), LIMITS);
    assert!(rs.image().is_none(), "nothing prepared");
    assert_eq!(rs.chart_side(), None, "no result side asked for");
    let held = handed(c.path(), rs.chart_side());
    let sources = Arc::clone(&held.sources);
    rs.take_images(Some(held));
    rs.request(view(5));
    settled(&mut rs);
    let b = shown(&rs);
    assert_eq!(b.side(), 2_048);
    assert!(b.chart_layer().is_some());
    let fit = rs
        .chart_for(&b)
        .expect("the layout's derivation is published");
    assert_eq!(content(&fit, ChartImage::Stage), [640, 480]);
    assert_eq!(content(&fit, ChartImage::Back), [512, 384]);
    let (id, owned) = cached_derivation(&rs).expect("the cached layout owns it");
    assert_eq!(id, sources.id());
    assert!(Arc::ptr_eq(&owned, &fit));
    assert_ne!(fit.id(), sources.id(), "derived, not the sources");
    assert_eq!(rs.image_bytes(), sources.bytes() + fit.bytes());
}

/// Selection's and practice's entry drop every image the result keeps,
/// a kept layout's derivation included: nothing of the chart's pixels
/// stays (its `Weak` dead), while the layout itself stays reusable, and
/// reused with the next attempt's images it derives them again.
#[test]
fn leaving_the_result_drops_the_layouts_derivations() {
    let _s = serial();
    let d = unprepared_skin();
    let c = chart_dir([640, 480]);
    let mut rs = staged_skin(d.path(), LIMITS);
    rs.take_images(Some(handed(c.path(), rs.chart_side())));
    rs.request(view(5));
    settled(&mut rs);
    let b = shown(&rs);
    let (_, owned) = cached_derivation(&rs).expect("the cached layout owns it");
    let weak = Arc::downgrade(&owned);
    drop(owned);
    rs.drop_images();
    assert_eq!(rs.image_bytes(), 0);
    assert!(cached_derivation(&rs).is_none());
    assert!(rs.chart_for(&b).is_none());
    assert!(weak.upgrade().is_none(), "the derivation's pixels kept");
    // The next attempt's images: the kept layout derives them again.
    let e = chart_dir([320, 240]);
    rs.take_images(Some(handed(e.path(), rs.chart_side())));
    rs.request(view(5));
    settled(&mut rs);
    let fit = rs.chart_for(&shown(&rs)).expect("derived again");
    assert_eq!(content(&fit, ChartImage::Stage), [320, 240]);
}

/// The prepared atlas's side is 2 304, so
/// the result half is the 2 304 fit (its stage file 1 136 wide); a view
/// that rebinds to it draws that half, and a later view whose cell the
/// prepared atlas lacks packs a union of side 4 096 and draws the stage
/// file derived from the sources at that slot, 1 600×1 200, not the half.
#[test]
fn a_growing_union_derives_its_slot_from_the_sources() {
    let _s = serial();
    let d = chart_skin(
        "local nr = ms.number(154)",
        "local W1 = { id = \"w1\", src = \"w1\", x = 0, y = 0, w = 2200, h = 8 }\n\
         local W2 = { id = \"w2\", src = \"w2\", x = 0, y = 0, w = 4000, h = 8 }\n\
         local imgs = { W1 }\n\
         if nr == 2 then imgs = { W1, W2 } end",
        "imgs",
    );
    let c = chart_dir([1_600, 1_200]);
    let mut rs = staged_skin(d.path(), LIMITS);
    assert_eq!(rs.image().unwrap().atlas.side(), 2_304);
    assert_eq!(rs.chart_side(), Some(2_304));
    let held = handed(c.path(), rs.chart_side());
    let sources = Arc::clone(&held.sources);
    let half = Arc::clone(held.half.as_ref().unwrap());
    assert_eq!(content(&half, ChartImage::Stage), [1_136, 852]);
    rs.take_images(Some(held));
    rs.request(view(5));
    settled(&mut rs);
    let rebound = shown(&rs);
    assert_eq!(rebound.side(), 2_304);
    assert!(Arc::ptr_eq(&rs.chart_for(&rebound).unwrap(), &half));
    let b0 = builds();
    rs.request(view(2));
    settled(&mut rs);
    assert_eq!(builds() - b0, 1, "one union");
    let grown = shown(&rs);
    assert_eq!(grown.side(), 4_096);
    let fit = rs.chart_for(&grown).expect("the union's derivation");
    assert_eq!(content(&fit, ChartImage::Stage), [1_600, 1_200]);
    assert_eq!(content(&fit, ChartImage::Back), [512, 384]);
    let (id, owned) = cached_derivation(&rs).unwrap();
    assert_eq!(id, sources.id());
    assert!(Arc::ptr_eq(&owned, &fit));
}

/// A layout landing for an older request publishes nothing.
/// Its derivation is dropped with it when the latest view is another
/// (an unavailable view here, the built-in result), and cached with it
/// when the latest view is its own, which then reuses it with no job.
#[test]
fn an_older_landing_drops_or_caches_its_derivation() {
    let _s = serial();
    let d = unprepared_skin();
    let c = chart_dir([640, 480]);
    // Dropped: view 7's job lands after the unavailable view's request.
    let mut rs = staged_skin(d.path(), LIMITS);
    let held = handed(c.path(), rs.chart_side());
    let sources = Arc::clone(&held.sources);
    rs.take_images(Some(held));
    rs.request(view(7));
    rs.request(ViewFacts(None));
    assert!(matches!(rs.published().0, Some(Published::BuiltIn)));
    let g = rs.published().1;
    settled(&mut rs);
    assert_eq!(rs.published().1, g, "nothing published");
    assert_eq!(derivations(&rs), 0, "the older derivation dropped");
    assert_eq!(rs.image_bytes(), sources.bytes());
    // Cached: view 7 is asked for again while its job runs.
    let mut rs = staged_skin(d.path(), LIMITS);
    let held = handed(c.path(), rs.chart_side());
    let sources = Arc::clone(&held.sources);
    rs.take_images(Some(held));
    let jobs = jobs_started();
    rs.request(view(7));
    rs.request(ViewFacts(None));
    rs.request(view(7));
    settled(&mut rs);
    assert_eq!(jobs_started() - jobs, 1, "the older layout reused");
    let b = shown(&rs);
    let fit = rs.chart_for(&b).expect("its derivation published");
    let (id, owned) = cached_derivation(&rs).unwrap();
    assert_eq!(id, sources.id());
    assert!(Arc::ptr_eq(&owned, &fit));
    assert_eq!(content(&fit, ChartImage::Stage), [640, 480]);
}

/// A refused layout publishes the built-in result and
/// leaves the cached layout's derivation as it was; a deferred one keeps
/// its request and publishes nothing until the atlas is released; a
/// layout settled after its outcome came drops its derivation with it.
#[test]
fn a_refusal_a_deferral_and_a_cancellation_publish_no_derivation() {
    let _s = serial();
    let d = unprepared_skin();
    let c = chart_dir([640, 480]);
    // Refused (154 = 8).
    let mut rs = staged_skin(d.path(), LIMITS);
    let held = handed(c.path(), rs.chart_side());
    let sources = Arc::clone(&held.sources);
    rs.take_images(Some(held));
    rs.request(view(5));
    settled(&mut rs);
    let five = shown(&rs);
    let (_, d5) = cached_derivation(&rs).unwrap();
    rs.request(view(8));
    settled(&mut rs);
    assert!(matches!(rs.published().0, Some(Published::BuiltIn)));
    assert_eq!(derivations(&rs), 1);
    assert!(Arc::ptr_eq(&rs.layout.as_ref().unwrap().bundle, &five));
    assert!(Arc::ptr_eq(&cached_derivation(&rs).unwrap().1, &d5));
    assert_eq!(rs.image_bytes(), sources.bytes() + d5.bytes());
    // Deferred: two atlases retained, view 5's build waits.
    let mut rs = staged_skin(d.path(), LIMITS);
    let held = handed(c.path(), rs.chart_side());
    let sources = Arc::clone(&held.sources);
    rs.take_images(Some(held));
    rs.set_retained(&[
        skin_model::AtlasId(u64::MAX),
        skin_model::AtlasId(u64::MAX - 1),
    ]);
    rs.request(view(5));
    let end = Instant::now() + Duration::from_secs(20);
    while rs.has_job() {
        rs.poll();
        assert!(Instant::now() < end, "the layout did not end");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(rs.deferred());
    assert_eq!(rs.published().1, 0, "nothing published");
    assert_eq!(derivations(&rs), 0);
    assert_eq!(rs.image_bytes(), sources.bytes());
    rs.set_retained(&[]);
    settled(&mut rs);
    let b = shown(&rs);
    assert_eq!(cached_derivation(&rs).map(|(id, _)| id), Some(sources.id()));
    assert!(rs.chart_for(&b).is_some());
    // Settled once view 5's outcome came, before it is taken.
    let mut rs = staged_skin(d.path(), LIMITS);
    let held = handed(c.path(), rs.chart_side());
    let sources = Arc::clone(&held.sources);
    rs.take_images(Some(held));
    rs.request(view(5));
    let end = Instant::now() + Duration::from_secs(20);
    while !rs.landing() {
        assert!(Instant::now() < end, "the layout did not end");
        std::thread::sleep(Duration::from_millis(2));
    }
    rs.settle(|| false);
    assert!(!rs.busy());
    assert_eq!(rs.published().1, 0, "nothing published");
    assert_eq!(derivations(&rs), 0, "the derivation dropped with the job");
    assert_eq!(rs.image_bytes(), sources.bytes());
}

/// A ready view of a chart at play level `level` whose next-rank
/// difference (154) is `nr`.
fn view_at(level: i32, nr: u32) -> ViewFacts {
    let mut v = super::probe_view(skin_model::ResultChart {
        level: Some(level),
        ..skin_model::ResultChart::default()
    });
    v.score.next_rank = nr;
    ViewFacts(Some(skin_model::ResultFields::of(&v)))
}

/// Attempt A (level 12) prepares nothing (its probe is
/// refused) and view 5's layout packs its own atlas of side 2 048, its
/// images derived from A's sources (a 640×480 stage file). Attempt B
/// (level 13) prepares an atlas of side 2 304 (its probe draws `w1`), so
/// its result half is the 2 304 fit of B's sources (800×600). View 5's
/// cached layout reads no level, so its facts hold for B, but its images
/// are A's: `chart_for` gives its bundle nothing, and B's view 5 is laid
/// out again, on B's prepared atlas, drawing B's half.
#[test]
fn a_cached_layout_of_other_sources_is_laid_out_again() {
    let _s = serial();
    let d = chart_skin(
        "local nr = ms.number(154)",
        "local imgs = { { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }
         if nr == 1 then
           if ms.number(96) ~= 13 then error(\"the probe\") end
           imgs[2] = { id = \"w1\", src = \"w1\", x = 0, y = 0, w = 2200, h = 8 }
         end",
        "imgs",
    );
    let a = chart_dir([640, 480]);
    let b = chart_dir([800, 600]);
    let mut rs = staged_skin(d.path(), LIMITS);
    assert!(rs.image().is_none(), "A prepares nothing");
    rs.take_images(Some(handed(a.path(), rs.chart_side())));
    rs.request(view_at(12, 5));
    settled(&mut rs);
    let first = shown(&rs);
    assert_eq!(first.side(), 2_048);
    let from_a = rs.chart_for(&first).expect("A's derivation");
    assert_eq!(content(&from_a, ChartImage::Stage), [640, 480]);
    rs.next_attempt();
    staged_at(&mut rs, 13, JudgeClass::Normal);
    assert_eq!(rs.chart_side(), Some(2_304));
    let held = handed(b.path(), rs.chart_side());
    let half = Arc::clone(held.half.as_ref().unwrap());
    rs.take_images(Some(held));
    assert!(rs.chart_for(&first).is_none(), "A's derivation is not B's");
    let jobs = jobs_started();
    rs.request(view_at(13, 5));
    settled(&mut rs);
    assert_eq!(jobs_started() - jobs, 1, "laid out again");
    let second = shown(&rs);
    assert!(!Arc::ptr_eq(&second, &first));
    assert_eq!(second.side(), 2_304);
    let fit = rs.chart_for(&second).expect("B's half");
    assert!(Arc::ptr_eq(&fit, &half));
    assert_eq!(content(&fit, ChartImage::Stage), [800, 600]);
}

/// A skin that reads no fact of the viewed result makes a
/// bundle for every view, cached and reused by the next attempt; each
/// attempt asks the worker for its result half at that bundle's side, so
/// the reused bundle draws attempt B's half, not A's.
#[test]
fn a_reused_bundle_for_every_view_draws_the_new_attempts_half() {
    let _s = serial();
    let d = chart_skin("local nr = 0", "", DOT);
    let a = chart_dir([640, 480]);
    let b = chart_dir([800, 600]);
    let mut rs = staged_skin(d.path(), LIMITS);
    assert!(rs.staged_line().starts_with("every view"));
    rs.request(view(5));
    let every = shown(&rs);
    assert_eq!(rs.chart_side(), Some(every.side()));
    let first = handed(a.path(), rs.chart_side());
    let half_a = Arc::clone(first.half.as_ref().unwrap());
    rs.take_images(Some(first));
    assert!(Arc::ptr_eq(&rs.chart_for(&every).unwrap(), &half_a));
    rs.next_attempt();
    let jobs = jobs_started();
    staged_at(&mut rs, 12, JudgeClass::Normal);
    assert_eq!(jobs_started(), jobs, "the cached bundle reused");
    assert_eq!(rs.chart_side(), Some(every.side()));
    let second = handed(b.path(), rs.chart_side());
    let half_b = Arc::clone(second.half.as_ref().unwrap());
    rs.take_images(Some(second));
    rs.request(view(5));
    assert!(Arc::ptr_eq(&shown(&rs), &every));
    let fit = rs.chart_for(&every).unwrap();
    assert!(Arc::ptr_eq(&fit, &half_b));
    assert_eq!(content(&fit, ChartImage::Stage), [800, 600]);
}

/// Atlas limits with room for one page and the reserved layers only: an
/// atlas fits only without its chart layer.
const ONE_LAYER: AtlasLimits = AtlasLimits {
    max_layers: render::RESERVED_LAYERS + 1,
    ..LIMITS
};

/// The test logger: this crate's Info lines, while [`CAPTURING`].
struct Capture;

static CAPTURING: AtomicBool = AtomicBool::new(false);
static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static CAPTURE: Capture = Capture;

impl log::Log for Capture {
    fn enabled(&self, m: &log::Metadata<'_>) -> bool {
        m.level() <= log::Level::Info && CAPTURING.load(Ordering::SeqCst)
    }

    fn log(&self, r: &log::Record<'_>) {
        if self.enabled(r.metadata()) {
            LINES.lock().unwrap().push(r.args().to_string());
        }
    }

    fn flush(&self) {}
}

/// The lines naming `what` that `f` logs, on any thread.
fn logged(what: &str, f: impl FnOnce()) -> Vec<String> {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        log::set_logger(&CAPTURE).expect("no other logger in the tests");
        log::set_max_level(log::LevelFilter::Info);
    });
    LINES.lock().unwrap().clear();
    CAPTURING.store(true, Ordering::SeqCst);
    f();
    CAPTURING.store(false, Ordering::SeqCst);
    std::mem::take(&mut *LINES.lock().unwrap())
        .into_iter()
        .filter(|l| l.contains(what))
        .collect()
}

const GIVEN_UP: &str = "skin: its chart layer is given up";

/// A pending skin whose atlas fits only
/// without the chart layer: view 2's cell is not in the prepared atlas,
/// and its union is packed without the layer, the note logged; the view
/// shows the skin, with no chart layer and so no chart images.
#[test]
fn a_union_with_no_room_for_the_chart_layer_keeps_the_skin() {
    let _s = serial();
    let d = chart_skin(
        "local nr = ms.number(154)",
        "",
        "{ { id = \"dot\", src = \"s\", x = 8 * (nr % 3), y = 0, w = 8, h = 8 } }",
    );
    let c = chart_dir([640, 480]);
    let mut rs = staged_skin(d.path(), ONE_LAYER);
    assert!(rs.image().unwrap().atlas.chart_layer().is_none());
    assert_eq!(rs.chart_side(), None);
    rs.take_images(Some(handed(c.path(), rs.chart_side())));
    let b0 = builds();
    let notes = logged(GIVEN_UP, || {
        rs.request(view(2));
        settled(&mut rs);
    });
    assert_eq!(builds() - b0, 1, "one union");
    assert_eq!(notes.len(), 1, "{notes:?}");
    let b = shown(&rs);
    assert!(b.chart_layer().is_none());
    assert!(rs.chart_for(&b).is_none());
}

/// A skin that reads no fact of the
/// viewed result, whose atlas fits only without the chart layer, makes
/// its bundle for every view without the layer, the note logged, and
/// shows it.
#[test]
fn a_bundle_for_every_view_with_no_room_for_the_chart_layer_keeps_the_skin() {
    let _s = serial();
    let d = chart_skin("local nr = 0", "", DOT);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(pending(d.path(), ONE_LAYER))));
    let notes = logged(GIVEN_UP, || {
        staged_at(&mut rs, 12, JudgeClass::Normal);
    });
    assert!(
        rs.staged_line().starts_with("every view"),
        "{}",
        rs.staged_line()
    );
    assert_eq!(notes.len(), 1, "{notes:?}");
    rs.request(view(5));
    let b = shown(&rs);
    assert!(b.chart_layer().is_none());
    assert_eq!(rs.chart_side(), None);
}
