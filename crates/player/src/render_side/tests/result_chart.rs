//! The chart's own images on the result screen through the real render
//! thread: a view's images are written into the
//! array its staging uses and keyed to that staging's installation, so
//! its first frame draws them; a set the array already holds is not
//! written again; a staging that never commits takes its table with it,
//! and the view presented keeps drawing its own; a screen built again
//! writes them again; a refused write draws them absent without failing
//! the screen; and a disabled result writes none.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bms_model::TimeUs;
use game::Preparation;
use skin_model::chart_image::ChartTable;
use skin_model::{AtlasLimits, ImageFacts, Quad};

use super::result::{Drawn, Rig, installed, lua_skin_scripted, payload, show, skin_frame};
use super::wait_until;
use crate::chart_images::{ChartImages, Want, derive_set, read_sources};
use crate::result_present::{Overlay, ViewPayload};

/// The stage file's destination (model rect, y down) and a point in it.
const STAGE_DST: [f32; 4] = [1_000.0, 580.0, 400.0, 300.0];
const STAGE_AT: [usize; 2] = [1_200, 730];
/// The back image's destination and a point in it.
const BACK_DST: [f32; 4] = [1_500.0, 680.0, 300.0, 200.0];
const BACK_AT: [usize; 2] = [1_650, 780];
/// The mark drawn under option 191 (the chart has a stage file).
const MARK_DST: [f32; 4] = [1_700.0, 172.0, 8.0, 8.0];

const RED: [u8; 3] = [200, 10, 10];
const GREEN: [u8; 3] = [10, 200, 10];
const BLUE: [u8; 3] = [10, 10, 200];

/// A fixed Lua result skin drawing the stage file (−100), the back image
/// (−101) and a mark under 191, with `more` among its destinations.
fn chart_skin(
    more: &str,
) -> (
    tempfile::TempDir,
    Arc<render::SkinBundle>,
    crate::screens::ScriptSlot,
) {
    let (d, b, s) = lua_skin_scripted(
        "local calls = 0
",
        &format!(
            "{{ id = -100, dst = {{ {{ x = 1000, y = 200, w = 400, h = 300 }} }} }},\n\
             {{ id = -101, dst = {{ {{ x = 1500, y = 200, w = 300, h = 200 }} }} }},\n\
             {{ id = \"dot\", op = {{ 191 }}, dst = {{ {{ x = 1700, y = 900, w = 8, h = 8 }} }} }},\n\
             {more}"
        ),
    );
    assert!(b.chart_layer().is_some(), "the skin keeps a chart layer");
    (d, b, s)
}

/// Two bundles of `bundle`'s skin over one new atlas: a view of the
/// second staged while the first is installed is a layout on the
/// installed array.
fn pair(bundle: &Arc<render::SkinBundle>) -> [Arc<render::SkinBundle>; 2] {
    let skin = bundle.skin();
    let variant = bundle.atlas_pages().variant();
    let limits = AtlasLimits {
        max_side: 4_096,
        max_layers: 16,
        reserved_layers: render::RESERVED_LAYERS,
    };
    let atlas = skin.atlas(variant, limits, &|| false).unwrap();
    let second = atlas
        .rebind_keeping(skin.sheet(), variant, skin.kept_layers())
        .unwrap();
    let of = |a| {
        Arc::new(
            render::SkinBundle::new(
                Arc::clone(skin),
                a,
                render::ClipTable::default(),
                skin_model::Side::Left,
            )
            .unwrap(),
        )
    };
    [of(atlas), of(second)]
}

/// A PNG of `w`×`h` texels of `rgb` at `path`.
fn png(path: &std::path::Path, [w, h]: [u32; 2], [r, g, b]: [u8; 3]) {
    let file = std::fs::File::create(path).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels = [r, g, b, 255].repeat(usize::try_from(w * h).unwrap());
    e.write_header().unwrap().write_image_data(&texels).unwrap();
}

/// A chart's images, a 64×48 stage file of `stage` and a 32×24 back
/// image of `back`, read into sources and fitted for chart layers of
/// side `side`.
fn images(side: u32, stage: [u8; 3], back: [u8; 3]) -> (tempfile::TempDir, Arc<ChartImages>) {
    let d = tempfile::tempdir().unwrap();
    png(&d.path().join("s.png"), [64, 48], stage);
    png(&d.path().join("b.png"), [32, 24], back);
    let want = Want {
        dir: d.path().to_owned(),
        names: ["s.png".to_owned(), "b.png".to_owned()],
        play: None,
        result: None,
    };
    let sources = read_sources(&want, None, &|| false);
    let set = derive_set(&sources, side, &|| false).unwrap();
    assert!(set.fitted().iter().all(Option::is_some), "{set:?}");
    (d, Arc::new(set))
}

/// [`payload`] with the chart's images `chart`, both available.
fn viewed(
    key: u64,
    skin: &Arc<render::SkinBundle>,
    ex: u32,
    lines: &[&str],
    chart: &Arc<ChartImages>,
) -> Arc<ViewPayload> {
    let p = payload(key, Some(skin), ex, lines);
    let mut view = p.view.clone();
    view.chart.images = ImageFacts {
        stage: true,
        back: true,
    };
    Arc::new(ViewPayload {
        chart: Some(Arc::clone(chart)),
        view,
        ..(*p).clone()
    })
}

/// Whether `q` holds a quad on `rect`, rounded.
fn has(q: &[Quad], rect: [f32; 4], layer: Option<u16>) -> bool {
    q.iter()
        .any(|q| q.rect.map(f32::round) == rect && layer.is_none_or(|l| q.layer == l))
}

/// Whether a frame drew both images from chart layer `layer`.
fn draws_images(q: &[Quad], layer: u16) -> bool {
    has(q, STAGE_DST, Some(layer)) && has(q, BACK_DST, Some(layer))
}

/// Whether a frame drew either image's quad.
fn draws_any_image(q: &[Quad]) -> bool {
    has(q, STAGE_DST, None) || has(q, BACK_DST, None)
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3)
}

/// Each result frame drawn from now on is read back at the two images'
/// points.
fn sample(rig: &Rig) {
    *rig.faults.sample.lock().unwrap() = vec![STAGE_AT, BACK_AT];
}

/// Forgets what was drawn and sampled, under the lock the render thread
/// records both under.
fn reset(rig: &Rig) {
    let mut d = rig.drawn.lock().unwrap();
    d.clear();
    rig.faults.samples.lock().unwrap().clear();
}

/// The pixels sampled in the frame drawn at `index`.
fn sampled(rig: &Rig, index: usize) -> Vec<[u8; 3]> {
    rig.faults
        .samples
        .lock()
        .unwrap()
        .iter()
        .find(|(i, _)| *i == index)
        .map(|(_, p)| p.clone())
        .unwrap_or_else(|| panic!("frame {index} was not sampled"))
}

/// The first skin frame drawn with `lines` strip lines: its index and its
/// quads.
fn first_with(rig: &Rig, lines: usize) -> (usize, Vec<Quad>) {
    assert!(
        wait_until(|| rig
            .drawn()
            .iter()
            .any(|d| skin_frame(d).is_some_and(|(_, n)| n == lines))),
        "no frame with {lines} lines: {:?}",
        rig.drawn()
    );
    rig.drawn()
        .into_iter()
        .enumerate()
        .find_map(|(i, d)| match d {
            Drawn::Skin(q, n) if n == lines => Some((i, q)),
            _ => None,
        })
        .unwrap()
}

/// The renderer's result chart writes that wrote pixels.
fn writes(rig: &Rig) -> u64 {
    rig.faults.chart_writes.load(Ordering::Acquire)
}

/// The tables the writes answered.
fn tables(rig: &Rig) -> Vec<ChartTable> {
    rig.faults.chart_tables.lock().unwrap().clone()
}

/// The first view, staged on its own resources, has its
/// images written into its own array and keyed to its installation: the
/// very first frame draws both from the chart layer, the stage-slot and
/// back-slot texels sampled, and 191 holds.
#[test]
fn a_first_view_on_its_own_resources_draws_its_images() {
    let (_dir, skin, _) = chart_skin("");
    let layer = skin.chart_layer().unwrap();
    let (_images, set) = images(skin.side(), RED, BLUE);
    let rig = Rig::new();
    sample(&rig);
    let lifetime = rig.link.begin_result();
    show(
        &rig.link,
        lifetime,
        1,
        &viewed(1, &skin, 1_111, &[], &set),
        &Overlay::default(),
    );
    let (i, q) = first_with(&rig, 0);
    assert_eq!(i, 0, "the view's first frame is the first drawn");
    assert!(draws_images(&q, layer), "{q:?}");
    assert!(has(&q, MARK_DST, None), "191 holds");
    let px = sampled(&rig, i);
    assert!(near(px[0], RED) && near(px[1], BLUE), "{px:?}");
    assert_eq!(writes(&rig), 1);
    assert_eq!(rig.link.result_status().installed, Some(skin.atlas_id()));
    rig.exit();
}

/// A play skin at the atlas cap that only the result's committed charge
/// keeps out is admitted by releasing the result's installation (the
/// renderer's own admission); the next result lifetime installs the
/// result skin again and its first view writes and draws its own images,
/// the second chart's.
#[test]
fn a_result_released_by_plays_admission_draws_the_next_charts_images() {
    let (_dir, skin, _) = chart_skin("");
    let layer = skin.chart_layer().unwrap();
    let (_a, red) = images(skin.side(), RED, BLUE);
    let (_b, green) = images(skin.side(), GREEN, BLUE);
    let big = {
        let r = crate::render_common::offscreen(1_920, 1_080);
        crate::render_decl::atlas_cap_bundle(&r, [0, 0, 255, 255])
    };
    let mut rig = Rig::new();
    rig.link
        .set_timeouts(Duration::from_secs(60), Duration::from_secs(10));
    sample(&rig);
    rig.faults.charge_result.store(
        render::SHARED_TEXELS - skin_model::MAX_ATLAS_TEXELS + 256 * 256 * 2,
        Ordering::Release,
    );
    let lifetime = rig.link.begin_result();
    show(
        &rig.link,
        lifetime,
        1,
        &viewed(1, &skin, 1_111, &[], &red),
        &Overlay::default(),
    );
    let (i, q) = first_with(&rig, 0);
    assert!(draws_images(&q, layer));
    assert!(near(sampled(&rig, i)[0], RED));
    assert_eq!(rig.link.result_status().installed, Some(skin.atlas_id()));
    // Play's admission: past the shared limit only with the result's
    // charge, so the result's installation is released.
    assert_eq!(
        rig.link
            .select(skin_model::ScreenKind::Play, Some(Arc::clone(&big)))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    assert!(
        wait_until(|| rig.link.result_status().installed.is_none()),
        "{:?}",
        rig.link.result_status()
    );
    // The next attempt's result: installed again, its images written.
    let written = writes(&rig);
    reset(&rig);
    let lifetime = rig.link.begin_result();
    show(
        &rig.link,
        lifetime,
        1,
        &viewed(2, &skin, 2_222, &[], &green),
        &Overlay::default(),
    );
    let (i, q) = first_with(&rig, 0);
    assert!(draws_images(&q, layer), "{q:?}");
    assert!(near(sampled(&rig, i)[0], GREEN), "{:?}", sampled(&rig, i));
    assert_eq!(writes(&rig), written + 1);
    assert_eq!(rig.link.result_status().installed, Some(skin.atlas_id()));
    rig.exit();
}

/// Pages uploaded alone, then a view of a layout on them, then
/// a view of another layout on the same array with the same set, then the
/// same layout again (installed): the first view writes the set into the
/// pages' array; the next ones write nothing (the array holds it), each
/// keyed to its own staging's installation, so each view's first frame
/// draws the images.
#[test]
fn a_layout_on_the_installed_array_writes_nothing_and_draws_at_once() {
    let (_dir, skin, _) = chart_skin("");
    let [a, b] = pair(&skin);
    let layer = a.chart_layer().unwrap();
    let (_images, set) = images(a.side(), RED, BLUE);
    let rig = Rig::new();
    sample(&rig);
    assert_eq!(
        rig.link
            .upload_result(crate::result_skin::Upload::Pages(Arc::clone(
                a.atlas_pages()
            )))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    let lifetime = rig.link.begin_result();
    show(
        &rig.link,
        lifetime,
        1,
        &viewed(1, &a, 1_111, &[], &set),
        &Overlay::default(),
    );
    let (i, q) = first_with(&rig, 0);
    assert!(draws_images(&q, layer), "{q:?}");
    assert!(near(sampled(&rig, i)[0], RED));
    assert_eq!(writes(&rig), 1, "written into the pages' array");
    for (key, bundle, lines) in [(2, &b, &["B"][..]), (3, &b, &["C", "C"][..])] {
        show(
            &rig.link,
            lifetime,
            key,
            &viewed(key, bundle, 2_222, lines, &set),
            &Overlay::default(),
        );
        let (i, q) = first_with(&rig, lines.len());
        assert!(draws_images(&q, layer), "view {key}'s first frame: {q:?}");
        let px = sampled(&rig, i);
        assert!(near(px[0], RED) && near(px[1], BLUE), "view {key}: {px:?}");
        assert_eq!(writes(&rig), 1, "view {key}: the array holds the set");
    }
    let t = tables(&rig);
    assert_eq!(t.len(), 3);
    assert_ne!(t[1].key(), t[0].key(), "the layout's own installation");
    assert_eq!(t[2].key(), t[1].key(), "the installed one's");
    assert_eq!(rig.link.result_status().installed, Some(a.atlas_id()));
    rig.exit();
}

/// A view staged and written, superseded before its
/// commit, is discarded with its staging and never draws; a view whose
/// staging is refused gets no table, writes nothing, and draws its
/// built-in page.
#[test]
fn a_superseded_or_refused_view_draws_no_table_of_its_own() {
    let (_x_dir, x, _) = chart_skin("");
    let (_y_dir, y, _) = chart_skin("");
    let layer = x.chart_layer().unwrap();
    let (_red, red) = images(x.side(), RED, BLUE);
    let (_green, green) = images(y.side(), GREEN, BLUE);
    let rig = Rig::new();
    sample(&rig);
    let lifetime = rig.link.begin_result();
    show(
        &rig.link,
        lifetime,
        1,
        &viewed(1, &x, 1_111, &[], &red),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    assert_eq!(writes(&rig), 1);
    // B staged and written, held before its commit; then C shown while
    // that frame's wait times out: B is dropped, C prepared.
    rig.gate.hold();
    show(
        &rig.link,
        lifetime,
        2,
        &viewed(2, &y, 2_222, &["B"], &green),
        &Overlay::default(),
    );
    rig.gate.step();
    assert_eq!(writes(&rig), 2, "B's images written at its staging");
    assert!(rig.link.result_status().retained.contains(&y.atlas_id()));
    let discards = rig.faults.discards.load(Ordering::Acquire);
    rig.faults.time_out.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        3,
        &viewed(3, &x, 3_333, &["C", "C"], &red),
        &Overlay::default(),
    );
    rig.gate.step();
    assert!(
        rig.faults.discards.load(Ordering::Acquire) > discards,
        "B's staging discarded"
    );
    rig.gate.release();
    let (i, q) = first_with(&rig, 2);
    assert!(draws_images(&q, layer));
    assert!(near(sampled(&rig, i)[0], RED));
    assert!(
        wait_until(|| !rig.link.result_status().retained.contains(&y.atlas_id())),
        "{:?}",
        rig.link.result_status()
    );
    for (i, d) in rig.drawn().iter().enumerate() {
        let (_, n) = skin_frame(d).expect("the skin draws");
        assert_ne!(n, 1, "B drawn");
        if let Some((_, px)) = rig.faults.samples.lock().unwrap().iter().find(|s| s.0 == i) {
            assert!(!near(px[0], GREEN), "B's images drawn in frame {i}");
        }
    }
    // A staging refused: no table, nothing written, its built-in page.
    let before = (writes(&rig), tables(&rig).len());
    rig.faults.refuse_install.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        4,
        &viewed(4, &y, 4_444, &[], &green),
        &Overlay::default(),
    );
    assert!(rig.until(|d| matches!(d, Drawn::Page(p) if p.iter().any(|l| l == "v4 2"))));
    assert_eq!((writes(&rig), tables(&rig).len()), before);
    assert!(!rig.link.failed());
    // A staging cancelled: the same, its own page.
    rig.faults.cancel_install.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        5,
        &viewed(5, &y, 5_555, &[], &green),
        &Overlay::default(),
    );
    assert!(rig.until(|d| matches!(d, Drawn::Page(p) if p.iter().any(|l| l == "v5 2"))));
    assert_eq!((writes(&rig), tables(&rig).len()), before);
    assert!(!rig.link.failed());
    rig.exit();
}

/// A device failing during a view's chart write fails the screen: the
/// staging is never committed, and on the screen built again the view is
/// staged again and its first frame draws its images.
#[test]
fn a_device_lost_during_the_chart_write_draws_the_images_on_the_next_screen() {
    let (_x_dir, x, _) = chart_skin("");
    let (_red, red) = images(x.side(), RED, BLUE);
    let (_green, green) = images(x.side(), GREEN, BLUE);
    let rig = Rig::new();
    sample(&rig);
    let lifetime = rig.link.begin_result();
    show(
        &rig.link,
        lifetime,
        1,
        &viewed(1, &x, 1_111, &[], &red),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let written = writes(&rig);
    rig.faults.lose_chart.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        2,
        &viewed(2, &x, 2_222, &["B"], &green),
        &Overlay::default(),
    );
    assert!(wait_until(|| rig.link.failed()));
    assert_eq!(writes(&rig), written, "nothing written by the lost write");
    reset(&rig);
    rig.link.prepare().unwrap();
    let (i, q) = first_with(&rig, 1);
    assert!(draws_images(&q, x.chart_layer().unwrap()), "{q:?}");
    let px = sampled(&rig, i);
    assert!(near(px[0], GREEN) && near(px[1], BLUE), "{px:?}");
    rig.exit();
}

/// A view presented (A, red) and a newer one staged and
/// written (B, green) whose commit does not happen: its generation
/// evicted (`Gone`), its commit refused, or a fade-out begun. Each frame
/// after draws A with its own table (the stage texel red), B is never
/// installed, and its staging is retired.
#[test]
fn a_commit_that_does_not_happen_keeps_the_presented_views_table() {
    for outcome in ["gone", "refused", "fadeout"] {
        let (_x_dir, x, _) = chart_skin("");
        let (_y_dir, y, script) = chart_skin(
            "{ id = \"dot\", draw = function() calls = calls + 1 return calls > 0 end, \
             dst = { { x = 0, y = 700, w = 8, h = 8 } } },",
        );
        let layer = x.chart_layer().unwrap();
        let (_red, red) = images(x.side(), RED, BLUE);
        let (_green, green) = images(y.side(), GREEN, BLUE);
        let rig = Rig::new();
        sample(&rig);
        let lifetime = rig.link.begin_result();
        show(
            &rig.link,
            lifetime,
            1,
            &viewed(1, &x, 1_111, &[], &red),
            &Overlay::default(),
        );
        assert!(rig.until(|d| skin_frame(d).is_some()));
        let mut b = viewed(2, &y, 2_222, &["B"], &green);
        match outcome {
            "gone" => {
                // Neither published, presented nor retained: evicted.
                let g = rig.link.scripts().offer_result(script, false);
                assert!(g.is_some(), "{outcome}: a runtime");
                assert!(!rig.link.scripts().protected(g.unwrap()));
                b = Arc::new(ViewPayload {
                    script: g,
                    ..(*b).clone()
                });
            }
            "refused" => rig.faults.refuse_commit.store(true, Ordering::Release),
            _ => assert_eq!(
                rig.link.pin_result(TimeUs(5), None).unwrap().0,
                Some(1),
                "{outcome}"
            ),
        }
        let written = writes(&rig);
        reset(&rig);
        show(&rig.link, lifetime, 2, &b, &Overlay::default());
        assert!(
            wait_until(|| writes(&rig) == written + 1),
            "{outcome}: B written"
        );
        let after = rig.drawn().len();
        assert!(wait_until(|| rig.drawn().len() >= after + 3));
        assert!(
            wait_until(|| !rig.link.result_status().retained.contains(&y.atlas_id())),
            "{outcome}: B's staging retired: {:?}",
            rig.link.result_status()
        );
        assert_eq!(
            rig.link.result_status().installed,
            Some(x.atlas_id()),
            "{outcome}: B never installed"
        );
        for (i, d) in rig.drawn().iter().enumerate() {
            let Some((q, n)) = skin_frame(d) else {
                panic!("{outcome}: {d:?}");
            };
            assert_eq!(n, 0, "{outcome}: B drawn");
            assert!(draws_images(q, layer), "{outcome}: frame {i}");
            let px = sampled(&rig, i);
            assert!(near(px[0], RED), "{outcome}: frame {i}: {px:?}");
        }
        assert_eq!(rig.link.result_status().shown, Some((1, true)), "{outcome}");
        rig.exit();
    }
}

/// The screen fails with a view presented, and with
/// a newer view staged and written but not committed; on the screen built
/// again, the first frame of each view draws its images (the stage
/// texel): the presented view's written again at the reapply, the
/// prepared view's at its staging again.
#[test]
fn the_images_draw_again_on_a_screen_built_again() {
    // The view presented.
    let (_x_dir, x, _) = chart_skin("");
    let layer = x.chart_layer().unwrap();
    let (_red, red) = images(x.side(), RED, BLUE);
    let rig = Rig::new();
    sample(&rig);
    let lifetime = rig.link.begin_result();
    show(
        &rig.link,
        lifetime,
        1,
        &viewed(1, &x, 1_111, &[], &red),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    rig.faults.lose.store(true, Ordering::Release);
    assert!(wait_until(|| rig.link.failed()));
    reset(&rig);
    rig.link.prepare().unwrap();
    let (i, q) = first_with(&rig, 0);
    assert!(draws_images(&q, layer), "{q:?}");
    let px = sampled(&rig, i);
    assert!(near(px[0], RED) && near(px[1], BLUE), "{px:?}");
    rig.exit();

    // The view prepared: the strip prepared for it fails after its
    // staging wrote its images.
    let (_y_dir, y, _) = chart_skin("");
    let (_green, green) = images(y.side(), GREEN, BLUE);
    let rig = Rig::new();
    sample(&rig);
    let lifetime = rig.link.begin_result();
    show(
        &rig.link,
        lifetime,
        1,
        &viewed(1, &x, 1_111, &[], &red),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    rig.faults.fail_strip.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        2,
        &viewed(2, &y, 2_222, &["B"], &green),
        &Overlay::default(),
    );
    assert!(wait_until(|| rig.link.failed()));
    assert_eq!(writes(&rig), 2, "B written before the failure");
    reset(&rig);
    rig.link.prepare().unwrap();
    let (i, q) = first_with(&rig, 1);
    assert!(draws_images(&q, y.chart_layer().unwrap()), "{q:?}");
    let px = sampled(&rig, i);
    assert!(near(px[0], GREEN) && near(px[1], BLUE), "{px:?}");
    for (i, d) in rig.drawn().iter().enumerate() {
        if let Some((_, 0)) = skin_frame(d) {
            assert!(near(sampled(&rig, i)[0], RED), "A's frame {i}");
        }
    }
    rig.exit();
}

/// A view staged on the installed array whose write is
/// refused draws with an all-absent table (no image quad, the stage point
/// not the images', 191 still holding), the array's record is cleared (the
/// next view of the set it held writes it again), and the screen does not
/// fail.
#[test]
fn a_refused_chart_write_draws_absent_and_keeps_the_screen() {
    let (_dir, x, _) = chart_skin("");
    let layer = x.chart_layer().unwrap();
    let (_red, red) = images(x.side(), RED, BLUE);
    let (_green, green) = images(x.side(), GREEN, BLUE);
    let rig = Rig::new();
    sample(&rig);
    let lifetime = installed(&rig, &x);
    show(
        &rig.link,
        lifetime,
        1,
        &viewed(1, &x, 1_111, &[], &red),
        &Overlay::default(),
    );
    let (i, _) = first_with(&rig, 0);
    assert!(near(sampled(&rig, i)[0], RED));
    assert_eq!(writes(&rig), 1);
    rig.faults.refuse_chart.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        2,
        &viewed(2, &x, 2_222, &["B"], &green),
        &Overlay::default(),
    );
    let (i, q) = first_with(&rig, 1);
    assert!(!draws_any_image(&q), "an all-absent table: {q:?}");
    assert!(has(&q, MARK_DST, None), "191 holds");
    let px = sampled(&rig, i);
    assert!(!near(px[0], RED) && !near(px[0], GREEN), "{px:?}");
    assert_eq!(writes(&rig), 1, "nothing written");
    assert!(!rig.link.failed(), "the screen does not fail");
    assert_eq!(rig.link.result_status().disabled, None);
    assert_eq!(rig.link.result_status().shown, Some((2, true)));
    // The record cleared: the set it held is written again.
    show(
        &rig.link,
        lifetime,
        3,
        &viewed(3, &x, 3_333, &["C", "C"], &red),
        &Overlay::default(),
    );
    let (i, q) = first_with(&rig, 2);
    assert!(draws_images(&q, layer));
    assert!(near(sampled(&rig, i)[0], RED));
    assert_eq!(writes(&rig), 2, "the record was cleared");
    rig.exit();
}

/// Once the result is disabled, a view with
/// images draws its built-in page and writes no chart.
#[test]
fn a_disabled_result_draws_the_page_and_writes_no_chart() {
    let (_dir, x, _) = chart_skin("");
    let (_red, red) = images(x.side(), RED, BLUE);
    let (_green, green) = images(x.side(), GREEN, BLUE);
    let rig = Rig::new();
    let lifetime = installed(&rig, &x);
    show(
        &rig.link,
        lifetime,
        1,
        &viewed(1, &x, 1_111, &[], &red),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    assert_eq!(writes(&rig), 1);
    rig.faults.refuse.store(true, Ordering::Release);
    assert!(wait_until(|| rig.link.result_status().disabled.is_some()));
    let before = (writes(&rig), tables(&rig).len());
    rig.clear();
    show(
        &rig.link,
        lifetime,
        2,
        &viewed(2, &x, 2_222, &[], &green),
        &Overlay::default(),
    );
    assert!(rig.until(|d| matches!(d, Drawn::Page(p) if p.iter().any(|l| l == "v2 2"))));
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        rig.drawn().iter().all(|d| matches!(d, Drawn::Page(_))),
        "the skin drew"
    );
    assert_eq!(
        (writes(&rig), tables(&rig).len()),
        before,
        "a chart written"
    );
    rig.exit();
}

/// The commit point and the build of a result frame allocate
/// nothing with a chart table installed (the skin's images drawn) and with
/// one absent, alternating; each view's table is written outside the
/// counted spans, as its staging writes it. Unoptimized builds count the
/// standard library's own allocations: CI runs it in release.
#[test]
fn result_chart_frame_alloc() {
    use super::super::{
        HeldView, PreparedView, ResultDrawn, ResultSide, Run, STRIP_PREPARED, Screen as _, Shared,
    };
    use crate::menu_screen::MenuScreen;
    let _warp = crate::warp_turn::take();
    let (_dir, skin, _) = chart_skin("");
    let layer = skin.chart_layer().unwrap();
    let (_images, set) = images(skin.side(), RED, BLUE);
    let r = render::Renderer::new(
        render::Target::Offscreen {
            width: 1_920,
            height: 1_080,
        },
        render::AdapterChoice::Warp,
    )
    .unwrap();
    let mut screen = MenuScreen::new(r, Some(render::text::Fonts::runtime().unwrap()));
    let token = |request, screen: &MenuScreen| render::PreparedToken {
        request,
        device: screen.device(),
        kind: skin_model::ScreenKind::Result,
    };
    assert_eq!(
        screen
            .install(skin_model::ScreenKind::Result, &skin, 1)
            .unwrap(),
        Preparation::Ready
    );
    screen.commit(token(1, &screen)).unwrap();
    let shared = Shared::new();
    let scripts = crate::screens::Scripts::new();
    let mut side = ResultSide::new(1);
    let mut counts = Vec::new();
    for cycle in 0..6_u64 {
        let request = cycle + 2;
        let with = cycle % 2 == 0;
        let p = viewed(
            cycle,
            &skin,
            1_000 + u32::try_from(cycle).unwrap(),
            &[],
            &set,
        );
        assert_eq!(
            screen
                .install(skin_model::ScreenKind::Result, &skin, request)
                .unwrap(),
            Preparation::Ready
        );
        let staged = token(request, &screen);
        let table = if with {
            screen
                .write_result_chart(
                    render::ResultChartTarget::Staged(staged),
                    set.fitted(),
                    set.id(),
                )
                .unwrap()
        } else {
            ChartTable::ABSENT
        };
        let mut state = skin.skin().prepare_result();
        state.set_chart_images(table);
        screen
            .prepare_result(&skin, &mut state, &p.view, &p.labels)
            .unwrap()
            .unwrap();
        screen.prepare_result_strip(STRIP_PREPARED, &[]).unwrap();
        side.prepared = Some(PreparedView {
            held: HeldView {
                revision: cycle + 1,
                payload: Arc::clone(&p),
                overlay: Overlay::default(),
                state: Some(state),
                page: None,
                script: crate::screens::ScriptSlot::default(),
            },
            token: Some(staged),
        });
        let now = TimeUs(1_000_000 + 16_000 * i64::try_from(cycle).unwrap());
        let before = crate::alloc_count::this_thread();
        let fadeout =
            Run::result_commit(&mut screen, Some(&mut side), &shared, &scripts, TimeUs(0));
        let drawn = Run::result_build(
            &mut screen,
            Some(&mut side),
            now,
            fadeout,
            &skin_model::AppReadouts::LAUNCH,
        );
        counts.push(crate::alloc_count::this_thread() - before);
        assert_eq!(drawn, ResultDrawn::Built);
        let quads = screen.result_quads().unwrap();
        assert_eq!(
            draws_images(&quads, layer),
            with,
            "cycle {cycle}: the table drawn"
        );
        screen.submit_result().unwrap();
        side.dropped = [None, None];
        let _ = screen.drain_retired();
    }
    // The control: the count sees an allocation in the same window.
    let before = crate::alloc_count::this_thread();
    let made = std::hint::black_box(vec![0_u8; 64]);
    let control = crate::alloc_count::this_thread() - before;
    drop(made);
    assert!(control > 0, "the count sees allocations");
    assert!(counts.iter().all(|&n| n == 0), "{counts:?}");
}
