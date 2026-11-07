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

/// The builds a section counts are this thread's tests' alone: the tests
/// that count run one at a time.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
