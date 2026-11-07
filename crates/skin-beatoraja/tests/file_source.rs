//! A Lua result skin's imports through a [`FileSource`] with a
//! [`ResourceCache`] (design D13, C19 (a), (f), (g), (h) at the import
//! level): what a run reads from the disk, counted at the source, and that
//! a run served from the cache imports what a fresh one does.

#![cfg(feature = "dev-tools")]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::result::{Res, best, ready};
use common::{font, link_dir, png, write};
use skin_beatoraja::{Cancel, Config, Fact, ImportFacts, LuaImported, LuaOutcome, read_lua_header};
use skin_model::{
    CacheBounds, FileSource, FontReader, ResourceCache, ResultFields, ResultNumber, Root,
    ScreenKind, SourceCounts,
};

/// A bitmap font `name` of size 8 over `pages`, its glyphs `A` and `B`
/// `advance` wide.
fn fnt(root: &Path, name: &str, pages: &[&str], advance: u32) {
    let mut s = format!(
        "info face=\"t\" size=8 padding=0,0,0,0\n\
         common lineHeight=8 base=8 scaleW=16 scaleH=16 pages={}\n",
        pages.len()
    );
    for (i, p) in pages.iter().enumerate() {
        s += &format!("page id={i} file=\"{p}\"\n");
    }
    s += "chars count=2\n";
    for (i, code) in [65, 66].iter().enumerate() {
        let page = i % pages.len();
        s += &format!(
            "char id={code} x=0 y=0 width=8 height=8 xoffset=0 yoffset=0 \
             xadvance={advance} page={page}\n"
        );
    }
    write(root, name, s.as_bytes());
}

/// A result skin `s.luaskin` whose font list `fonts` (a Lua expression,
/// which may read `nr`, number 154) and text list `texts` are given.
fn skin(root: &Path, fonts: &str, texts: &str) {
    png(root, "s.png", 40, 16);
    let entry = format!(
        "local skin = {{ type = 7, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         local nr = ms.number(154)\n\
         local function add_x(t) t[#t + 1] = {{ id = \"x\", path = \"../b.fnt\" }} return t end\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
         skin.font = {fonts}\n\
         skin.image = {{ {{ id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 }} }}\n\
         skin.value = {{ {{ id = \"ex\", src = \"s\", x = 0, y = 8, w = 40, h = 8, divx = 10, \
                           digit = 4, ref = 71 }} }}\n\
         skin.text = {texts}\n\
         local d = {{ {{ id = \"ex\", dst = {{ {{ x = 0, y = 100, w = 4, h = 8 }} }} }}, \
                      {{ id = \"dot\", op = {{ 90 }}, dst = {{ {{ x = 0, y = 300, w = 8, h = 8 }} }} }} }}\n\
         for i, t in ipairs(skin.text) do\n\
           d[#d + 1] = {{ id = t.id, dst = {{ {{ x = 0, y = 400 + 20 * i, w = 200, h = 8 }} }} }}\n\
         end\n\
         skin.destination = d\n\
         return skin\n"
    );
    write(root, "s.luaskin", entry.as_bytes());
}

/// A view whose next-rank difference (154) is `nr`.
fn view(nr: u32) -> skin_model::ResultView {
    let mut v = ready(1_000, true, best(ResultNumber::Shown(-3)));
    v.score.next_rank = nr;
    v
}

/// The facts of [`view`]`(nr)`.
fn facts(nr: u32) -> ImportFacts {
    ImportFacts {
        result: Fact::Known(ResultFields::of(&view(nr))),
        ..ImportFacts::unavailable(ScreenKind::Result)
    }
}

/// `root`'s skin imported for `nr` through `source`, or the disk for
/// `None`.
fn run(root: &Path, nr: u32, source: Option<FileSource>) -> LuaImported {
    let cancel = Cancel::new();
    let mut h = read_lua_header(root, "s.luaskin", &cancel).unwrap();
    if let Some(s) = source {
        h.read_through(s);
    }
    match h.import(&Config::defaults(h.header()), &facts(nr), &cancel) {
        Ok(LuaOutcome::Imported(i)) => i,
        other => panic!("{other:?}"),
    }
}

fn cache(root: &Path) -> Arc<ResourceCache> {
    ResourceCache::new(&Root::new(root).unwrap())
}

/// One run for `nr` through a fresh source over `cache`, and what that
/// run read from the disk.
fn cached(root: &Path, cache: &Arc<ResourceCache>, nr: u32) -> (LuaImported, SourceCounts) {
    let source = FileSource::cached(Arc::clone(cache));
    let i = run(root, nr, Some(source.clone()));
    (i, source.counts())
}

/// C19 (a) at the import level: images, a two-page bitmap font and an
/// outline font, imported for one view through the cache, then for
/// another: the first run reads each kind from the disk, the second reads
/// nothing and decodes nothing, and the outline font's bytes, once read
/// for a pack, are served from the cache too.
#[test]
fn a_second_run_reads_no_image_or_font_file() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    for p in ["p0.png", "p1.png"] {
        png(root, p, 16, 16);
    }
    fnt(root, "b.fnt", &["p0.png", "p1.png"], 8);
    font(root, "f.ttf");
    skin(
        root,
        "{ { id = \"b\", path = \"b.fnt\" }, { id = \"f\", path = \"f.ttf\" } }",
        "{ { id = \"tb\", font = \"b\", size = 8, constantText = \"AB\" }, \
           { id = \"tf\", font = \"f\", size = 8, constantText = \"AB\" } }",
    );
    let c = cache(root);
    let (first, n) = cached(root, &c, 5);
    assert!(n.resolutions > 0 && n.headers > 0, "{n:?}");
    assert_eq!(
        (n.fnt_texts, n.page_validations, n.font_magics),
        (1, 2, 1),
        "{n:?}"
    );
    assert_eq!((n.encoded_reads, n.decodes), (2, 2), "{n:?}");
    // A pack's read of the outline font, through the first run's sheet.
    let sheet = first.imported.skin.sheet();
    let fonts = sheet.font_count();
    assert_eq!(fonts, 1, "the outline font is the sheet's");
    let source = FileSource::cached(Arc::clone(&c));
    let mut h = read_lua_header(root, "s.luaskin", &Cancel::new()).unwrap();
    h.read_through(source.clone());
    let (second, n) = {
        let i = match h.import(&Config::defaults(h.header()), &facts(7), &Cancel::new()) {
            Ok(LuaOutcome::Imported(i)) => i,
            other => panic!("{other:?}"),
        };
        (i, source.counts())
    };
    assert_eq!(n, SourceCounts::default(), "the second run read nothing");
    let read_first = FontReader::new(first.imported.skin.sheet())
        .read(0)
        .unwrap();
    let before = source.counts();
    let read_second = FontReader::new(second.imported.skin.sheet())
        .read(0)
        .unwrap();
    assert_eq!(read_first, read_second);
    assert_eq!(source.counts(), before, "the cached bytes, no read");
}

/// C19 (f): two `.fnt` files of other metrics over the same pages. The
/// first run admits one; a view choosing the other reads its text alone.
#[test]
fn a_second_fnt_over_known_pages_reads_only_its_text() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    for p in ["p0.png", "p1.png"] {
        png(root, p, 16, 16);
    }
    fnt(root, "b.fnt", &["p0.png", "p1.png"], 8);
    fnt(root, "c.fnt", &["p0.png", "p1.png"], 6);
    skin(
        root,
        "{ { id = \"b\", path = nr == 0 and \"c.fnt\" or \"b.fnt\" } }",
        "{ { id = \"tb\", font = \"b\", size = 8, constantText = \"AB\" } }",
    );
    let c = cache(root);
    let (_, n) = cached(root, &c, 1);
    assert_eq!((n.fnt_texts, n.page_validations), (1, 2), "{n:?}");
    let (_, n) = cached(root, &c, 0);
    assert_eq!(
        n,
        SourceCounts {
            resolutions: 1,
            fnt_texts: 1,
            ..SourceCounts::default()
        },
        "c.fnt's name and text only"
    );
}

/// C19 (h): a third `.fnt` over a page the cache has no verdict for: that
/// page is read and decoded once, then served; with no room in the cache
/// it is read and decoded on every run.
#[test]
fn a_page_without_a_verdict_is_read_and_decoded() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    for p in ["p0.png", "q0.png"] {
        png(root, p, 16, 16);
    }
    fnt(root, "b.fnt", &["p0.png"], 8);
    fnt(root, "e.fnt", &["q0.png"], 8);
    skin(
        root,
        "{ { id = \"b\", path = nr == 0 and \"e.fnt\" or \"b.fnt\" } }",
        "{ { id = \"tb\", font = \"b\", size = 8, constantText = \"AB\" } }",
    );
    let c = cache(root);
    cached(root, &c, 1);
    let (_, n) = cached(root, &c, 0);
    assert_eq!(
        (n.page_validations, n.encoded_reads, n.decodes, n.headers),
        (1, 1, 1, 1),
        "{n:?}"
    );
    let (_, n) = cached(root, &c, 0);
    assert_eq!(n, SourceCounts::default(), "recorded within the bound");

    let full = ResourceCache::with_bounds(
        &Root::new(root).unwrap(),
        CacheBounds {
            contents: 0,
            ..CacheBounds::DEFAULT
        },
    );
    for _ in 0..2 {
        let (_, n) = cached(root, &full, 0);
        assert_eq!(
            (n.page_validations, n.encoded_reads, n.decodes),
            (1, 1, 1),
            "past the bound, read every time: {n:?}"
        );
    }
}

/// C19 (g): a run served from the cache, reading nothing from the disk,
/// imports what a fresh import of the same view through the disk does:
/// the report with its notes and their places, the usage, the
/// dependencies, the facts, the sheet, and the frame it draws for the
/// view (each quad's place, size and uv: the glyphs' metrics and the
/// images' and fonts' bindings and indices). The views admit fonts in
/// another order, leave one out, and name one file under two names (a
/// folder and an in-root link to it, charged per name), with notes to
/// give (a font refused by its path).
#[test]
fn a_cached_run_imports_what_a_fresh_one_does() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    for p in ["p0.png", "p1.png", "q0.png"] {
        png(root, p, 16, 16);
    }
    fnt(root, "b.fnt", &["p0.png", "p1.png"], 8);
    fnt(root, "c.fnt", &["q0.png"], 6);
    font(root, "f.ttf");
    std::fs::create_dir(root.join("real")).unwrap();
    png(&root.join("real"), "r0.png", 16, 16);
    fnt(&root.join("real"), "r.fnt", &["r0.png"], 5);
    link_dir(&root.join("alias"), &root.join("real"));
    // 1: b, c, f; 2: c, b, f (reordered); 3: b, f (c left out);
    // 4: one file under two names. Every view also names a font above the
    // skin folder, refused by its path.
    skin(
        root,
        "add_x((nr == 2 and { { id = \"b\", path = \"c.fnt\" }, { id = \"c\", path = \"b.fnt\" }, \
                              { id = \"f\", path = \"f.ttf\" } }) \
         or (nr == 3 and { { id = \"b\", path = \"b.fnt\" }, { id = \"f\", path = \"f.ttf\" } }) \
         or (nr == 4 and { { id = \"b\", path = \"real/r.fnt\" }, { id = \"c\", path = \"alias/r.fnt\" }, \
                           { id = \"f\", path = \"f.ttf\" } }) \
         or { { id = \"b\", path = \"b.fnt\" }, { id = \"c\", path = \"c.fnt\" }, \
              { id = \"f\", path = \"f.ttf\" } })",
        "{ { id = \"tb\", font = \"b\", size = 8, constantText = \"ABBA\" }, \
           { id = \"tc\", font = \"c\", size = 8, constantText = \"BA\" }, \
           { id = \"tf\", font = \"f\", size = 8, constantText = \"A\" }, \
           { id = \"tx\", font = \"x\", size = 8, constantText = \"A\" } }",
    );
    let c = cache(root);
    for nr in [1, 2, 3, 4] {
        cached(root, &c, nr);
    }
    let mut noted = false;
    for nr in [1, 2, 3, 4] {
        let fresh = run(root, nr, None);
        let (warm, n) = cached(root, &c, nr);
        assert_eq!(
            n,
            SourceCounts::default(),
            "view {nr}: served from the cache"
        );
        let r = &fresh.imported.report;
        noted |= !r.notes.is_empty();
        assert_eq!(&warm.imported.report, r, "view {nr}: the report");
        assert_eq!(warm.usage, fresh.usage, "view {nr}: the usage");
        assert_eq!(warm.dependencies, fresh.dependencies, "view {nr}");
        assert_eq!(warm.facts, fresh.facts, "view {nr}");
        let (ws, fs) = (warm.imported.skin.sheet(), fresh.imported.skin.sheet());
        assert_eq!(
            (
                ws.frame_count(),
                ws.image_count(),
                ws.font_count(),
                ws.bitmap_font_count()
            ),
            (
                fs.frame_count(),
                fs.image_count(),
                fs.font_count(),
                fs.bitmap_font_count()
            ),
            "view {nr}: the sheet"
        );
        let frame = |i: LuaImported| {
            let mut res = Res::new(i.imported.skin);
            res.view(&view(nr));
            res.skin.attach_result_bitmap_text(&mut res.res);
            res.frame(0)
        };
        let (wq, fq) = (frame(warm), frame(fresh));
        assert!(fq.len() > 4, "view {nr}: glyphs drawn: {}", fq.len());
        assert_eq!(wq, fq, "view {nr}: the frame");
    }
    assert!(noted, "the views give notes");
}

/// D13's bounds: past its resolution entries, or its bitmap font or
/// outline font bytes, a cache records nothing more of that kind, so each
/// run reads it again; what it holds stays within the bounds.
#[test]
fn past_a_bound_a_miss_is_read_every_time() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    png(root, "p0.png", 16, 16);
    fnt(root, "b.fnt", &["p0.png"], 8);
    font(root, "f.ttf");
    skin(
        root,
        "{ { id = \"b\", path = \"b.fnt\" }, { id = \"f\", path = \"f.ttf\" } }",
        "{ { id = \"tb\", font = \"b\", size = 8, constantText = \"AB\" }, \
           { id = \"tf\", font = \"f\", size = 8, constantText = \"AB\" } }",
    );
    let r = Root::new(root).unwrap();
    let bounded = |b: CacheBounds| ResourceCache::with_bounds(&r, b);
    let none = bounded(CacheBounds {
        resolutions: 0,
        ..CacheBounds::DEFAULT
    });
    let texts = bounded(CacheBounds {
        fnt_bytes: 0,
        ..CacheBounds::DEFAULT
    });
    let fonts = bounded(CacheBounds {
        font_bytes: 0,
        ..CacheBounds::DEFAULT
    });
    for _ in 0..2 {
        let (_, n) = cached(root, &none, 5);
        assert!(n.resolutions >= 4, "no resolution kept: {n:?}");
        let (_, n) = cached(root, &texts, 5);
        assert_eq!(n.fnt_texts, 1, "no text kept: {n:?}");
        let (i, _) = cached(root, &fonts, 5);
        let source = FileSource::cached(Arc::clone(&fonts));
        let mut h = read_lua_header(root, "s.luaskin", &Cancel::new()).unwrap();
        h.read_through(source.clone());
        drop(i);
        let i = match h.import(&Config::defaults(h.header()), &facts(5), &Cancel::new()) {
            Ok(LuaOutcome::Imported(i)) => i,
            other => panic!("{other:?}"),
        };
        FontReader::new(i.imported.skin.sheet()).read(0).unwrap();
        assert_eq!(source.counts().font_bytes, 1, "no font bytes kept");
    }
    assert_eq!(none.usage().resolutions, 0);
    assert_eq!(texts.usage().fnt_bytes, 0);
    assert_eq!(fonts.usage().font_bytes, 0);
    let all = cache(root);
    cached(root, &all, 5);
    let u = all.usage();
    assert!(
        u.fnt_bytes > 0 && u.resolutions > 0 && u.contents > 0,
        "{u:?}"
    );
}
