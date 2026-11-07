//! The result skin's specialization through its production parts (the
//! importer over the resource cache, `run_job` and the polled job, the
//! atlas's plan, build and rebind, the bundle): design C18-C21, the
//! player's parts of C19.

use std::fs;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use skin_beatoraja::{Config, read_lua_header};
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
    let d = tempfile::tempdir().unwrap();
    png(d.path(), "s.png", 64, 16);
    png(d.path(), "big.png", 300, 20);
    let entry = format!(
        "local skin = {{ type = 7, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         local nr = ms.number(154)\n\
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
    assert_eq!(rs.image_stage(chart(), || false, || false), Staged::Ready);
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
    // The last note, in measure 2, ends past the first measure's 2 s.
    assert!(c.length.is_some_and(|l| l.0 >= 2_000_000), "{:?}", c.length);
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
    // The renderer moves on, releasing the first: the build starts.
    let b = shown(&rs);
    fake.take(&mut rs, b);
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
