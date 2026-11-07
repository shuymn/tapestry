//! The result skin's specialization through its production parts (the
//! importer over the resource cache, `run_job` and the polled job, the
//! atlas's plan, build and rebind, the bundle): design C18-C21, the
//! player's parts of C19.

use std::fs;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use skin_beatoraja::{Config, JudgeClass, read_lua_header};
use skin_model::{AtlasLimits, ResultChart, ResultFields, Root};

use super::{Published, ResultSkin, ResultStart, Staged, ViewFacts, builds, probe_view};
use crate::imports::{ImportTarget, jobs_joined, jobs_started};

/// `dir/name`, a `w`×`h` PNG of one colour.
fn png(dir: &Path, name: &str, w: u32, h: u32) {
    let file = fs::File::create(dir.join(name)).unwrap();
    let mut e = png::Encoder::new(BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let data: Vec<u8> = (0..w * h).flat_map(|_| [9, 9, 9, 255]).collect();
    e.write_header().unwrap().write_image_data(&data).unwrap();
}

/// A result skin `s.luaskin` reading 154 as `nr` while it loads, then
/// running `pre`, with the image list `images` (a Lua expression) of
/// `s.png`, its EX number and a status dot.
fn skin(pre: &str, images: &str) -> tempfile::TempDir {
    skin_reading("local nr = ms.number(154)", pre, images)
}

/// As [`skin`], reading `reads` instead of 154.
fn skin_reading(reads: &str, pre: &str, images: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    png(d.path(), "s.png", 64, 16);
    png(d.path(), "big.png", 300, 20);
    let entry = format!(
        "local skin = {{ type = 7, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         {reads}\n\
         {pre}\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }}, {{ id = \"b\", path = \"big.png\" }} }}\n\
         skin.image = {images}\n\
         skin.value = {{ {{ id = \"ex\", src = \"s\", x = 0, y = 8, w = 40, h = 8, divx = 10, \
                           digit = 4, ref = 71 }} }}\n\
         local d = {{ {{ id = \"ex\", dst = {{ {{ x = 0, y = 100, w = 4, h = 8 }} }} }} }}\n\
         for i, im in ipairs(skin.image) do\n\
           d[#d + 1] = {{ id = im.id, op = {{ 90 }}, dst = {{ {{ x = 0, y = 300 + 20 * i, w = 8, h = 8 }} }} }}\n\
         end\n\
         skin.destination = d\n\
         return skin\n"
    );
    fs::write(d.path().join("s.luaskin"), entry).unwrap();
    d
}

fn target(limits: AtlasLimits) -> ImportTarget {
    ImportTarget {
        limits,
        size: [1_920, 1_080],
        default_font: render::DefaultFont::runtime,
    }
}

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 4_096,
    max_layers: 16,
    reserved_layers: render::RESERVED_LAYERS,
};

/// The skin in `dir`, pending, its imports through its cache.
fn pending(dir: &Path, limits: AtlasLimits) -> Arc<super::PendingResult> {
    let h = read_lua_header(dir, "s.luaskin", &skin_beatoraja::Cancel::new()).unwrap();
    let config = Config::defaults(h.header());
    Arc::new(super::PendingResult::new(
        h,
        config,
        target(limits),
        &Root::new(dir).unwrap(),
    ))
}

fn chart() -> ResultChart {
    ResultChart {
        level: Some(12),
        total: None,
        bpm: None,
        difficulty: None,
        length: None,
        notes: None,
    }
}

/// A ready view whose next-rank difference (154) is `nr`.
fn view(nr: u32) -> ViewFacts {
    let mut v = probe_view(chart());
    v.score.next_rank = nr;
    ViewFacts(Some(ResultFields::of(&v)))
}

/// Polls `rs` until its jobs are done, at most 20 s.
fn settled(rs: &mut ResultSkin) {
    let end = Instant::now() + Duration::from_secs(20);
    while !rs.poll() {
        assert!(Instant::now() < end, "the layout did not land");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The published bundle and the pages it shares.
fn shown(rs: &ResultSkin) -> Arc<render::SkinBundle> {
    match rs.published().0 {
        Some(Published::Skin(b, _)) => Arc::clone(b),
        other => panic!("{other:?}"),
    }
}

fn staged(rs: &mut ResultSkin) {
    assert_eq!(
        rs.image_stage(chart(), Some(JudgeClass::Normal), || false, || false),
        Staged::Ready
    );
}

/// The builds and jobs a test counts are its own: these tests run one at a
/// time with the importer's tests, which count jobs too.
fn serial() -> crate::imports::tests::Serial {
    crate::imports::tests::serial()
}

/// C18, C19 (a), (c): the image stage packs the probe's cells once; P and
/// a switch lay out over the cache with no image or font file read, no
/// pixel decoded and no build, their bundles sharing the prepared pages,
/// a view that draws fewer cells (a zero branch) included; the next
/// attempt's image stage reuses it with no job.
#[test]
fn images_once_then_layouts() {
    let _s = serial();
    let d = skin(
        "",
        "(nr == 0 and { { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }) \
         or { { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 }, \
              { id = \"more\", src = \"s\", x = 8, y = 0, w = 8, h = 8 } }",
    );
    let p = pending(d.path(), LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
    let b0 = builds();
    staged(&mut rs);
    assert_eq!(builds() - b0, 1, "the probe's cells packed once");
    let pages = Arc::clone(rs.image().unwrap().pages());
    let read = p.source_counts();
    let g0 = rs.published().1;
    for nr in [5, 7, 0] {
        rs.request(view(nr));
        settled(&mut rs);
        let b = shown(&rs);
        assert!(
            Arc::ptr_eq(b.atlas_pages(), &pages),
            "view {nr}: the prepared pages"
        );
    }
    assert_eq!(rs.published().1 - g0, 3);
    assert_eq!(builds() - b0, 1, "no build on P or a switch");
    assert_eq!(
        p.source_counts(),
        read,
        "no image or font file read, no decode"
    );
    // The next attempt: the probe's facts and the atlas hold, no job.
    rs.next_attempt();
    let jobs = jobs_started();
    staged(&mut rs);
    assert_eq!(jobs_started(), jobs, "no image stage job");
    assert_eq!(builds() - b0, 1);
}

/// C19 (b): an image chosen by `154 % 3`; the probe gives residue 1. A
/// view of residue 2 packs a union once, then residue 0 once more; after
/// that every residue lays out with no build.
#[test]
fn a_missing_cell_packs_a_union_once() {
    let _s = serial();
    let d = skin(
        "",
        "{ { id = \"dot\", src = \"s\", x = 8 * (nr % 3), y = 0, w = 8, h = 8 } }",
    );
    let p = pending(d.path(), LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(p)));
    staged(&mut rs);
    let b0 = builds();
    rs.request(view(2));
    settled(&mut rs);
    assert_eq!(builds() - b0, 1, "residue 2: one union");
    rs.request(view(3));
    settled(&mut rs);
    assert_eq!(builds() - b0, 2, "residue 0: one more");
    for nr in [4, 5, 6, 1, 2, 3] {
        rs.request(view(nr));
        settled(&mut rs);
        assert!(matches!(rs.published().0, Some(Published::Skin(..))));
    }
    assert_eq!(builds() - b0, 2, "every residue held");
}

/// C18: a probe refused before any result fact leaves the built-in result
/// with no layout job; one refused after reading 154 prepares nothing, and
/// P lays out and packs the view's own cells.
#[test]
fn refused_probes() {
    let _s = serial();
    // Refused before 154 is read: the built-in result, with no layout job.
    let d = skin(
        "",
        "{ { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
    );
    let entry = fs::read_to_string(d.path().join("s.luaskin"))
        .unwrap()
        .replace(
            "local nr = ms.number(154)",
            "error(\"refused\")\nlocal nr = ms.number(154)",
        );
    fs::write(d.path().join("s.luaskin"), entry).unwrap();
    let p = pending(d.path(), LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(p)));
    staged(&mut rs);
    assert!(rs.image().is_none(), "nothing prepared");
    let jobs = jobs_started();
    rs.request(view(5));
    assert!(!rs.busy(), "no layout job");
    assert_eq!(jobs_started(), jobs);
    assert!(matches!(rs.published().0, Some(Published::BuiltIn)));

    let d = skin(
        "if nr == 1 then error(\"refused\") end",
        "{ { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
    );
    let p = pending(d.path(), LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(p)));
    staged(&mut rs);
    assert!(rs.image().is_none(), "nothing prepared");
    let b0 = builds();
    rs.request(view(5));
    settled(&mut rs);
    assert!(matches!(rs.published().0, Some(Published::Skin(..))));
    assert_eq!(builds() - b0, 1, "P packs its own cells");
}

/// C19 (e): the probe's cells past the target (a cell wider than its
/// page): Preparing prepares nothing; P's smaller cells pack and show.
#[test]
fn an_overflowing_probe_prepares_nothing() {
    let _s = serial();
    let small = AtlasLimits {
        max_side: 256,
        ..LIMITS
    };
    let d = skin(
        "",
        "(nr == 0 and { { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }) \
         or { { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 }, \
              { id = \"wide\", src = \"b\", x = 0, y = 0, w = 300, h = 20 } }",
    );
    let p = pending(d.path(), small);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(p)));
    let b0 = builds();
    staged(&mut rs);
    assert!(rs.image().is_none(), "the probe's cells do not fit");
    assert_eq!(builds() - b0, 0, "nothing built");
    rs.request(view(0));
    settled(&mut rs);
    assert!(matches!(rs.published().0, Some(Published::Skin(..))));
    assert_eq!(builds() - b0, 1);
}

/// C20: a layout job cancelled and its thread joined before the command
/// is served; the published generation stays.
#[test]
fn a_cancelled_layout_is_joined() {
    let _s = serial();
    let d = skin(
        "if nr == 9 then while true do end end",
        "{ { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
    );
    let p = pending(d.path(), LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(p)));
    staged(&mut rs);
    rs.request(view(5));
    settled(&mut rs);
    let g = rs.published().1;
    rs.request(view(9));
    assert!(rs.busy());
    let joined = jobs_joined();
    rs.settle(|| false);
    assert!(!rs.busy());
    assert_eq!(jobs_joined() - joined, 1, "the job's thread joined");
    assert_eq!(rs.published().1, g, "the shown generation stays");
    assert!(rs.image().is_some(), "the entry handed back");
}

/// C21: switches A, B, C while A's job runs: B is replaced by C while it
/// waits, A's layout is disposed of, and C's commits, the generation
/// changing once.
#[test]
fn only_the_latest_switch_commits() {
    let _s = serial();
    let d = skin(
        "if nr == 9 then local n = 0 for i = 1, 3000000 do n = n + i end end",
        "{ { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
    );
    let p = pending(d.path(), LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(p)));
    staged(&mut rs);
    let g = rs.published().1;
    let jobs = jobs_started();
    rs.request(view(9));
    rs.request(view(5));
    rs.request(view(7));
    settled(&mut rs);
    assert_eq!(rs.published().1 - g, 1, "one commit, C's");
    assert_eq!(jobs_started() - jobs, 2, "A's job, then C's; B never ran");
    let c = shown(&rs);
    rs.request(view(7));
    assert!(!rs.busy(), "C's layout is the entry");
    assert!(Arc::ptr_eq(&shown(&rs), &c));
}

/// Q5: the chart's facts projected for a result view: TOTAL (368), the
/// tempo's range and change (90-91), the length (1163-1164) and the
/// judged notes (74), each from the loaded chart.
#[test]
fn the_chart_facts_are_projected() {
    let loaded = crate::imports::facts::tests::loaded(
        "#PLAYLEVEL 7\n#DIFFICULTY 3\n#TOTAL 300.7\n#BPM01 180\n#00208:01\n#00211:01\n",
        "beatoraja-style",
    );
    let c = super::attempt_chart(&loaded);
    assert_eq!((c.level, c.difficulty), (Some(7), Some(3)));
    assert_eq!(c.total, Some(300));
    assert_eq!(
        c.bpm,
        Some(skin_model::ResultBpm {
            min_x100: 12_000,
            max_x100: 18_000,
            changes: true,
        })
    );
    assert_eq!(c.notes, Some(2));
    // The chart's last object, measure 2's note, at 4 s (`#BPM 120`).
    assert_eq!(c.length, Some(bms_model::TimeUs(4_000_000)));
}

/// The chart's length (numbers 1163-1164) is its 1× content end, a
/// background sound's time included: a note at 2 s and a BGM event at
/// measure 10 (`#BPM 120`) give 20 s, whatever section or speed the
/// attempt plays. The BGM event's sound is declared: the loader keeps no
/// background object of an undefined `#WAV`.
#[test]
fn the_length_is_the_charts_content_end_at_1x() {
    let (_dir, mut loaded) =
        crate::imports::facts::tests::loaded_kept("#WAV01 a.wav\n#01001:01\n", "beatoraja-style");
    let length = |l: &game::Loaded| super::attempt_chart(l).length;
    assert_eq!(length(&loaded), Some(bms_model::TimeUs(20_000_000)));
    loaded
        .set_speed(bms_model::Speed::from_twentieths(15).unwrap())
        .unwrap();
    assert_eq!(length(&loaded), Some(bms_model::TimeUs(20_000_000)));
    loaded
        .set_speed(bms_model::Speed::from_twentieths(29).unwrap())
        .unwrap();
    assert_eq!(length(&loaded), Some(bms_model::TimeUs(20_000_000)));
    let gauges = loaded.attempt().gauges.len();
    loaded
        .set_practice(Some(game::Practice {
            start: bms_model::TimeUs(0),
            end: bms_model::TimeUs(3_000_000),
            lead_in: bms_model::TimeUs(1_000_000),
            initial_gauges: vec![rules::GaugeValue(20 * rules::GaugeValue::PERCENT); gauges],
            repeat: true,
        }))
        .unwrap();
    assert_eq!(length(&loaded), Some(bms_model::TimeUs(20_000_000)));
}

/// A renderer that lags: it holds the bundles it took and reports their
/// atlases as retained only when told to.
#[derive(Default)]
struct FakeRenderer {
    held: Vec<Arc<render::SkinBundle>>,
}

impl FakeRenderer {
    /// Takes `bundle`, keeping only it, and reports what it retains.
    fn take(&mut self, rs: &mut ResultSkin, bundle: Arc<render::SkinBundle>) {
        self.held = vec![bundle];
        self.report(rs);
    }

    fn report(&self, rs: &mut ResultSkin) {
        let ids: Vec<_> = self.held.iter().map(|b| b.atlas_pages().id()).collect();
        rs.set_retained(&ids);
    }
}

/// Polls `rs` until no layout job runs, at most 20 s.
fn ran(rs: &mut ResultSkin) {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        rs.poll();
        if !rs.running() {
            return;
        }
        assert!(Instant::now() < end, "the layout did not end");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Every atlas seen, by a weak reference.
#[derive(Default)]
struct Seen(Vec<std::sync::Weak<skin_model::PackedPages>>);

impl Seen {
    fn note(&mut self, rs: &ResultSkin) {
        if let Some(e) = rs.image() {
            self.0.push(Arc::downgrade(e.pages()));
        }
        if let Some(Published::Skin(b, _)) = rs.published().0 {
            self.0.push(Arc::downgrade(b.atlas_pages()));
        }
    }

    /// The distinct atlases still alive.
    fn alive(&self) -> usize {
        let mut ids: Vec<_> = self
            .0
            .iter()
            .filter_map(|w| w.upgrade().map(|p| p.id()))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids.len()
    }
}

/// Q3: with a renderer that lags behind the publications, successive
/// cell-set changes keep at most two atlases alive: the third build waits
/// until the renderer's report drops an id, a built-in publication with a
/// retained atlas counts it, a report repeated after a recovery changes
/// nothing, and the run's end drops every atlas.
#[test]
fn at_most_two_atlases_live_with_a_lagging_renderer() {
    let _s = serial();
    let d = skin(
        "",
        "{ { id = \"dot\", src = \"s\", x = 8 * nr, y = 0, w = 8, h = 8 } }",
    );
    let p = pending(d.path(), LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(p)));
    let mut fake = FakeRenderer::default();
    let mut seen = Seen::default();
    staged(&mut rs);
    seen.note(&rs);
    let step = |rs: &mut ResultSkin, seen: &mut Seen, nr: u32| {
        rs.request(view(nr));
        ran(rs);
        seen.note(rs);
    };
    // P: the probe's atlas is dropped for the union's.
    step(&mut rs, &mut seen, 2);
    assert!(seen.alive() <= 2);
    let b = shown(&rs);
    fake.take(&mut rs, b);
    // A switch: the shown atlas and the new one.
    step(&mut rs, &mut seen, 3);
    assert_eq!(seen.alive(), 2, "the renderer's and the new one");
    // The renderer stalls on the first: a third build waits.
    let g = rs.published().1;
    step(&mut rs, &mut seen, 4);
    assert!(rs.deferred(), "the third build waits");
    assert_eq!(rs.published().1, g, "nothing published meanwhile");
    rs.poll();
    assert!(
        rs.deferred(),
        "a poll with two atlases live keeps it waiting"
    );
    assert_eq!(seen.alive(), 2);
    // The renderer moves on, releasing the first: its report starts the
    // build, before any poll.
    let b = shown(&rs);
    fake.take(&mut rs, b);
    assert!(
        !rs.deferred() && rs.running(),
        "the report retries the build"
    );
    ran(&mut rs);
    seen.note(&rs);
    assert!(!rs.deferred());
    assert_eq!(rs.published().1, g + 1);
    assert!(seen.alive() <= 2, "{}", seen.alive());
    // The renderer takes it; the next view is unavailable: the built-in
    // result is published while the renderer retains that atlas.
    let b = shown(&rs);
    fake.take(&mut rs, b);
    rs.request(ViewFacts(None));
    assert!(matches!(rs.published().0, Some(Published::BuiltIn)));
    step(&mut rs, &mut seen, 5);
    assert!(matches!(rs.published().0, Some(Published::Skin(..))));
    assert!(seen.alive() <= 2, "{}", seen.alive());
    // A recovery reports the same atlas again.
    fake.report(&mut rs);
    rs.poll();
    assert!(seen.alive() <= 2, "{}", seen.alive());
    // The run's end.
    drop(rs);
    drop(fake);
    assert_eq!(seen.alive(), 0, "every atlas dropped");
}

/// A gate a layout waits at between dropping an unshown image entry and
/// its build, while armed: the tests that arm it cancel there.
static GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Whether a layout reached the armed gate.
static REACHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How a layout's job ends once past the gate: 0 on, 1 by a panic, 2 by
/// a panic whose payload's drop panics, which ends its thread without an
/// outcome.
static FAIL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// A payload whose drop panics.
struct Bomb;

impl Drop for Bomb {
    fn drop(&mut self) {
        panic!("the payload's drop");
    }
}

/// [`GATE`], waited at by a layout's job until it is disarmed or the job
/// is cancelled; then [`FAIL`], taken.
pub(super) fn build_gate(cancel: &skin_beatoraja::Cancel) {
    use std::sync::atomic::Ordering::SeqCst;
    if !GATE.load(SeqCst) {
        return;
    }
    REACHED.store(true, SeqCst);
    while GATE.load(SeqCst) && !cancel.is_cancelled() {
        std::thread::sleep(Duration::from_millis(1));
    }
    match FAIL.swap(0, SeqCst) {
        1 => panic!("the layout's job"),
        2 => std::panic::panic_any(Bomb),
        _ => {}
    }
}

/// D11 (C21): a superseded job's failure, its thread panicking or ending
/// without an outcome, publishes nothing over the newer view's reused
/// layout, and that reuse drops the request waiting behind the job.
#[test]
fn a_stale_failure_leaves_the_newer_publication() {
    use std::sync::atomic::Ordering::SeqCst;
    let _s = serial();
    for fail in [1, 2] {
        let d = skin(
            "",
            "{ { id = \"dot\", src = \"s\", x = 8 * (nr % 3), y = 0, w = 8, h = 8 } }",
        );
        let mut rs = ResultSkin::new(Some(ResultStart::Pending(pending(d.path(), LIMITS))));
        staged(&mut rs);
        rs.request(view(2));
        settled(&mut rs);
        let two = shown(&rs);
        // View 3's cell needs a build: its job waits at the gate, then
        // fails; view 4 waits behind it.
        REACHED.store(false, SeqCst);
        FAIL.store(fail, SeqCst);
        GATE.store(true, SeqCst);
        let jobs = jobs_started();
        rs.request(view(3));
        let end = Instant::now() + Duration::from_secs(20);
        while !REACHED.load(SeqCst) {
            assert!(Instant::now() < end, "the layout did not reach its build");
            std::thread::sleep(Duration::from_millis(1));
        }
        rs.request(view(4));
        // View 2 again: its layout is reused at once.
        rs.request(view(2));
        assert!(Arc::ptr_eq(&shown(&rs), &two), "{fail}");
        GATE.store(false, SeqCst);
        settled(&mut rs);
        assert!(Arc::ptr_eq(&shown(&rs), &two), "{fail}");
        assert_eq!(jobs_started() - jobs, 1, "{fail}: no job for view 4");
    }
}

/// D11's cancellation after the drop: a layout cancelled once it has
/// dropped the unshown image entry, before its build, leaves no image
/// entry (the entry's pages gone); the next attempt's image stage builds
/// one again although the probe's facts hold.
#[test]
fn a_cancel_after_the_drop_leaves_no_entry_then_rebuilds() {
    use std::sync::atomic::Ordering::SeqCst;
    let _s = serial();
    let d = skin(
        "",
        "{ { id = \"dot\", src = \"s\", x = 8 * nr, y = 0, w = 8, h = 8 } }",
    );
    let p = pending(d.path(), LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(p)));
    staged(&mut rs);
    let probe = Arc::downgrade(rs.image().unwrap().pages());
    REACHED.store(false, SeqCst);
    GATE.store(true, SeqCst);
    // Nothing is shown: the probe's entry is unshown, and the view's new
    // cell needs a build.
    rs.request(view(2));
    let end = Instant::now() + Duration::from_secs(20);
    while !REACHED.load(SeqCst) {
        assert!(Instant::now() < end, "the layout did not reach its build");
        std::thread::sleep(Duration::from_millis(1));
    }
    rs.settle(|| false);
    GATE.store(false, SeqCst);
    assert!(rs.image().is_none(), "no image entry after the cancel");
    assert!(
        probe.upgrade().is_none(),
        "the dropped entry's pages are gone"
    );
    // The next attempt: the probe's facts hold, but no entry is live.
    rs.next_attempt();
    let jobs = jobs_started();
    staged(&mut rs);
    assert_eq!(jobs_started() - jobs, 1, "the image stage runs again");
    assert!(rs.image().is_some(), "an entry built again");
}

/// The chart's judgment class (options 180-184) reaches the image stage's
/// probe and each layout, read before or after a result value (154): the
/// probe prepares its images, P publishes the skin, and the same view
/// again reuses the layout with no job.
#[test]
fn the_judgment_class_reaches_both_stages() {
    let _s = serial();
    for reads in [
        "local c = ms.option(182)\nlocal nr = ms.number(154)",
        "local nr = ms.number(154)\nlocal c = ms.option(182)",
    ] {
        let d = skin_reading(
            reads,
            "if not c then error(\"class\") end",
            "{ { id = \"a\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
        );
        let mut rs = ResultSkin::new(Some(ResultStart::Pending(pending(d.path(), LIMITS))));
        staged(&mut rs);
        assert_eq!(rs.staged_line(), "per view, images prepared", "{reads}");
        rs.request(view(5));
        settled(&mut rs);
        assert!(
            matches!(rs.published().0, Some(Published::Skin(..))),
            "{reads}"
        );
        let jobs = jobs_started();
        rs.request(view(5));
        assert!(!rs.running(), "{reads}");
        assert_eq!(jobs_started(), jobs, "{reads}");
        assert!(
            matches!(rs.published().0, Some(Published::Skin(..))),
            "{reads}"
        );
    }
}

/// A skin that reads only the chart's facts (number 96, its play level)
/// is decided by the image stage, before READY: one bundle for every
/// view, published at P with no layout job.
#[test]
fn chart_reads_are_decided_before_ready() {
    let _s = serial();
    let d = skin_reading(
        "local lv = ms.number(96)",
        "if lv ~= 12 then error(\"level\") end",
        "{ { id = \"a\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
    );
    let p = pending(d.path(), LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
    staged(&mut rs);
    assert!(
        rs.staged_line().starts_with("every view"),
        "{}",
        rs.staged_line()
    );
    let jobs = jobs_started();
    rs.request(view(5));
    assert!(!rs.running());
    assert_eq!(jobs_started(), jobs);
    let every = shown(&rs);
    // An unchanged retry reuses the bundle: no job, no build, no file
    // read.
    let (b0, read) = (builds(), p.source_counts());
    rs.next_attempt();
    staged(&mut rs);
    assert!(rs.staged_line().starts_with("every view"));
    rs.request(view(7));
    assert_eq!(jobs_started(), jobs);
    assert_eq!(builds(), b0);
    assert_eq!(p.source_counts(), read);
    assert!(Arc::ptr_eq(&shown(&rs), &every));
}

/// The image stage at `level` with judgment class `class`.
fn staged_at(rs: &mut ResultSkin, level: i32, class: JudgeClass) {
    let c = ResultChart {
        level: Some(level),
        ..chart()
    };
    assert_eq!(
        rs.image_stage(c, Some(class), || false, || false),
        Staged::Ready
    );
}

/// The S10-09i design's Q3 for a bundle for every view: a skin that
/// reads the result at level 7 only. Level 7 prepares image entry R0 and
/// publishes its layout; level 8 builds a bundle for every view, R1,
/// published; level 9's build drops R0 and R1 first, so at most two
/// atlases live. With two atlases retained, a level's attempt is laid
/// out per view instead, with no build.
#[test]
fn a_bundle_for_every_view_keeps_two_atlases() {
    let _s = serial();
    let d = skin_reading(
        "local lv = ms.number(96)\nif lv == 7 then local nr = ms.number(154) end",
        "",
        "{ { id = \"a\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
    );
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(pending(d.path(), LIMITS))));
    staged_at(&mut rs, 7, JudgeClass::Normal);
    assert_eq!(rs.staged_line(), "per view, images prepared");
    let r0 = Arc::downgrade(rs.image().unwrap());
    rs.request(view(5));
    settled(&mut rs);
    rs.next_attempt();
    staged_at(&mut rs, 8, JudgeClass::Normal);
    assert!(rs.staged_line().starts_with("every view"));
    rs.request(view(5));
    let r1 = Arc::downgrade(&shown(&rs));
    rs.next_attempt();
    staged_at(&mut rs, 9, JudgeClass::Normal);
    assert!(rs.staged_line().starts_with("every view"));
    assert!(r0.upgrade().is_none(), "R0's pages live with R2");
    rs.request(view(5));
    assert!(r1.upgrade().is_none(), "R1 replaced");
    // Two atlases retained: no room for a bundle, laid out per view.
    rs.set_retained(&[
        skin_model::AtlasId(u64::MAX),
        skin_model::AtlasId(u64::MAX - 1),
    ]);
    rs.next_attempt();
    let b0 = builds();
    staged_at(&mut rs, 10, JudgeClass::Normal);
    assert!(
        rs.staged_line().starts_with("per view"),
        "{}",
        rs.staged_line()
    );
    assert_eq!(builds(), b0);
}

/// D13: an image stage whose probe is refused keeps the layout bound to
/// the unshown image entry: after view 5's layout and an unavailable
/// view, a chart of another judgment class refuses the probe before any
/// result value; back on the first chart, view 5 is reused with no job.
#[test]
fn a_refused_probe_keeps_the_reusable_layout() {
    let _s = serial();
    let d = skin_reading(
        "if ms.option(180) then error(\"class\") end\nlocal nr = ms.number(154)",
        "",
        "{ { id = \"a\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
    );
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(pending(d.path(), LIMITS))));
    staged_at(&mut rs, 12, JudgeClass::Normal);
    rs.request(view(5));
    settled(&mut rs);
    let five = shown(&rs);
    rs.request(ViewFacts(None));
    settled(&mut rs);
    rs.next_attempt();
    staged_at(&mut rs, 12, JudgeClass::VeryHard);
    assert_eq!(rs.staged_line(), "built-in, images prepared");
    rs.next_attempt();
    staged_at(&mut rs, 12, JudgeClass::Normal);
    let jobs = jobs_started();
    rs.request(view(5));
    assert!(!rs.running(), "view 5 reused");
    assert_eq!(jobs_started(), jobs);
    assert!(Arc::ptr_eq(&shown(&rs), &five));
}

/// D13 with the S10-09i design's Q3: a build deferred while two atlases
/// are live drops the unshown image entry's pages but keeps its cells, so
/// the build that follows packs them in its union and the probe's own
/// cell is then rebound with no build.
#[test]
fn a_deferral_keeps_the_unshown_entrys_cells() {
    let _s = serial();
    let d = skin(
        "",
        "{ { id = \"dot\", src = \"s\", x = 8 * (nr % 3), y = 0, w = 8, h = 8 } }",
    );
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(pending(d.path(), LIMITS))));
    staged(&mut rs);
    let probe = Arc::downgrade(rs.image().unwrap());
    // Two atlases the renderer retains: any build defers.
    rs.set_retained(&[
        skin_model::AtlasId(u64::MAX),
        skin_model::AtlasId(u64::MAX - 1),
    ]);
    let b0 = builds();
    // Nothing shown: the probe's entry (cell x = 8) is unshown; view 2's
    // cell (x = 16) needs a build.
    rs.request(view(2));
    ran(&mut rs);
    assert!(rs.deferred());
    assert!(
        rs.image().is_none(),
        "the unshown entry's pages are dropped"
    );
    assert!(probe.upgrade().is_none());
    assert_eq!(builds(), b0);
    rs.set_retained(&[]);
    settled(&mut rs);
    assert_eq!(builds() - b0, 1, "one union build");
    // The probe's cell is in that union: view 1 is rebound with no build.
    rs.request(view(1));
    settled(&mut rs);
    assert!(matches!(rs.published().0, Some(Published::Skin(..))));
    assert_eq!(builds() - b0, 1, "view 1 rebound");
}

/// D13: a layout whose import is refused never evicts the reusable layout
/// bound to the unshown image entry: after view 5's layout, an
/// unavailable view (the built-in result) and view 9's refusal, view 5
/// is reused with no job.
#[test]
fn a_refused_layout_keeps_the_reusable_one() {
    let _s = serial();
    let d = skin(
        "if nr == 9 then error(\"nine\") end",
        "{ { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
    );
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(pending(d.path(), LIMITS))));
    staged(&mut rs);
    rs.request(view(5));
    settled(&mut rs);
    let five = shown(&rs);
    rs.request(ViewFacts(None));
    settled(&mut rs);
    assert!(matches!(rs.published().0, Some(Published::BuiltIn)));
    rs.request(view(9));
    settled(&mut rs);
    assert!(matches!(rs.published().0, Some(Published::BuiltIn)));
    let jobs = jobs_started();
    rs.request(view(5));
    assert!(!rs.running(), "view 5 reused");
    assert_eq!(jobs_started(), jobs);
    assert!(Arc::ptr_eq(&shown(&rs), &five));
}

/// D13: once a build's plan fits, the unshown image entry's pages go
/// before the build, the layout bound to them with them.
#[test]
fn a_build_drops_the_layout_bound_to_its_unshown_entry() {
    use std::sync::atomic::Ordering::SeqCst;
    let _s = serial();
    let d = skin(
        "",
        "{ { id = \"dot\", src = \"s\", x = 8 * (nr % 3), y = 0, w = 8, h = 8 } }",
    );
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(pending(d.path(), LIMITS))));
    staged(&mut rs);
    rs.request(view(2));
    settled(&mut rs);
    let two = Arc::downgrade(rs.image().unwrap());
    rs.request(ViewFacts(None));
    settled(&mut rs);
    // View 3's cell needs a build: at it, view 2's entry and its layout
    // are gone.
    REACHED.store(false, SeqCst);
    GATE.store(true, SeqCst);
    rs.request(view(3));
    let end = Instant::now() + Duration::from_secs(20);
    while !REACHED.load(SeqCst) {
        assert!(Instant::now() < end, "the layout did not reach its build");
        std::thread::sleep(Duration::from_millis(1));
    }
    let gone = two.upgrade().is_none();
    GATE.store(false, SeqCst);
    settled(&mut rs);
    assert!(gone, "view 2's pages live through the build");
    assert!(matches!(rs.published().0, Some(Published::Skin(..))));
}

/// A result skin as [`skin`] with one image, reading 154, and a text of
/// the chart's title (text 10) in font `f` of `font`, the files `files`
/// beside it.
fn font_skin(font: &str, files: &[(&str, Vec<u8>)]) -> tempfile::TempDir {
    let d = skin(
        "skin.font = { { id = \"f\", path = \"PATH\" } }\n\
         skin.text = { { id = \"t\", font = \"f\", size = 100, ref = 10 } }"
            .replace("PATH", font)
            .as_str(),
        "{ { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
    );
    let entry = fs::read_to_string(d.path().join("s.luaskin"))
        .unwrap()
        .replace(
            "skin.destination = d\n",
            "d[#d + 1] = { id = \"t\", dst = { { x = 0, y = 600, w = 400, h = 100 } } }\n\
             skin.destination = d\n",
        );
    fs::write(d.path().join("s.luaskin"), entry).unwrap();
    for (name, bytes) in files {
        fs::write(d.path().join(name), bytes).unwrap();
    }
    d
}

/// A bitmap font `f.fnt` of one glyph, `A`, on page `f.png`.
fn fnt() -> Vec<(&'static str, Vec<u8>)> {
    let fnt = "info face=\"solid\" size=100 padding=0,0,0,0\n\
               common lineHeight=100 base=100 scaleW=128 scaleH=128 pages=1\n\
               page id=0 file=\"f.png\"\n\
               char id=65 x=0 y=0 width=100 height=100 xoffset=0 yoffset=0 xadvance=100 \
               page=0 chnl=15\n";
    let dir = tempfile::tempdir().unwrap();
    png(dir.path(), "f.png", 128, 128);
    vec![
        ("f.fnt", fnt.as_bytes().to_vec()),
        ("f.png", fs::read(dir.path().join("f.png")).unwrap()),
    ]
}

/// C19 for fonts: a bitmap font's page and an outline font are prepared
/// by the image stage; P and a switch lay out with no font or image file
/// read and no build, sharing the prepared pages.
#[test]
fn fonts_are_prepared_once() {
    let _s = serial();
    let outline = crate::font_support::build(&crate::font_support::latin("Skin"));
    for (font, files) in [
        ("f.fnt", fnt()),
        ("ttf.ttf", vec![("ttf.ttf", outline.clone())]),
    ] {
        let d = font_skin(font, &files);
        let p = pending(d.path(), LIMITS);
        let mut rs = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
        let b0 = builds();
        staged(&mut rs);
        assert_eq!(rs.staged_line(), "per view, images prepared", "{font}");
        assert_eq!(builds() - b0, 1, "{font}");
        let pages = Arc::clone(rs.image().unwrap().pages());
        let read = p.source_counts();
        // The image stage read the font: the bitmap font's text and page,
        // or the outline font's bytes.
        if font == "f.fnt" {
            assert!(read.fnt_texts > 0 && read.page_validations > 0, "{read:?}");
        } else {
            assert!(read.font_bytes > 0, "{read:?}");
        }
        for nr in [5, 7] {
            rs.request(view(nr));
            settled(&mut rs);
            let b = shown(&rs);
            assert!(Arc::ptr_eq(b.atlas_pages(), &pages), "{font}: view {nr}");
        }
        assert_eq!(builds() - b0, 1, "{font}: no build on P or a switch");
        assert_eq!(p.source_counts(), read, "{font}: no file read, no decode");
    }
}
