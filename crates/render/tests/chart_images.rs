//! The chart's own images on WARP (S10-12 design §5.3): a play
//! installation keeping a chart layer publishes them into it; its loading
//! frames draw neither them nor what their fields condition until they are
//! published, then the present ones and the marks their fields select; a
//! republication replaces them; an attempt prepared on the installation
//! draws them in its frames; and a renderer with no such installation
//! publishes nothing. A new choice's invalidation makes them unavailable;
//! an absent table needs no layer; and a staged installation with its own
//! array takes the images published into it at its commit.
//!
//! The skins are fixed 1920×1080 canvases on a 1920×1080 target, so canvas
//! pixels are target pixels ([`decl_support`]).

mod common;
mod decl_support;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{GAUGES, attempt, context, input, install, loading, offscreen, read_back, view};
use decl_support::{BACK_MARK, CHART_BACK, CHART_STAGE, Look, NO_BACK_MARK, bundle};
use render::text::MenuText;
use std::sync::Arc;

use render::{
    ChartOutcome, ChartTarget, ClipTable, LoadingFrame, LoadingOn, RenderError, SkinBundle,
};
use skin_model::chart_image::{ChartDir, ChartImage, ChartTable, Fitted, read};

const BLACK: [u8; 3] = [0; 3];
const RED: [u8; 3] = [255, 0, 0];
const GREEN: [u8; 3] = [0, 255, 0];
const BLUE: [u8; 3] = [0, 0, 255];

/// The RGB at the centre of canvas rect `rect` of a 1920-wide read back.
fn at(pixels: &[u8], rect: [i32; 4]) -> [u8; 3] {
    let [x, y, w, h] = rect.map(|v| u32::try_from(v).unwrap());
    common::rgb_at(pixels, 1_920, (x + w / 2, y + h / 2))
}

fn frame(lifetime: u64) -> LoadingFrame<'static> {
    common::loading_frame(lifetime, TimeUs(0), TimeUs(0))
}

/// A `w`×`h` picture of `rgb`, read and fitted into `which`'s slot of a
/// chart layer of side `side`.
fn picture(which: ChartImage, side: u32, [w, h]: [u32; 2], rgb: [u8; 3]) -> Fitted {
    let d = tempfile::tempdir().unwrap();
    let [r, g, b] = rgb;
    let file = std::fs::File::create(d.path().join("p.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels = [r, g, b, 255].repeat(usize::try_from(w * h).unwrap());
    e.write_header().unwrap().write_image_data(&texels).unwrap();
    let dir = ChartDir::new(d.path()).unwrap();
    let slot = which.slot(side).unwrap().1;
    read(&dir, "p.png", &[], slot, &|| false, &|_| None)
        .image
        .unwrap()
}

/// Red, with the chart's images.
fn chart_look() -> Look {
    Look {
        chart: true,
        ..Look::left([255, 0, 0, 255])
    }
}

fn chart() -> Chart {
    ChartBuilder::new(120_000).build().unwrap()
}

/// Before the images are published, a loading frame draws neither them
/// nor either mark; published, the present ones and the mark their fields
/// select; published again without the back image, its "none" mark.
#[test]
fn loading_frames_draw_the_published_images() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (b, _) = bundle(&r, dir.path(), &chart_look());
    let side = b.side();
    assert!(b.chart_layer().is_some());
    install(&mut r, &b, 1);
    let p = loading(&mut r, &text, &frame(1));
    for rect in [CHART_STAGE, CHART_BACK, BACK_MARK, NO_BACK_MARK] {
        assert_eq!(at(&p, rect), BLACK, "{rect:?}: unavailable");
    }

    let stage = picture(ChartImage::Stage, side, [640, 480], GREEN);
    let back = picture(ChartImage::Back, side, [320, 240], BLUE);
    let table = r
        .publish_chart_images(ChartTarget::Committed, [Some(&stage), Some(&back), None])
        .unwrap();
    assert!(table.present(ChartImage::Stage) && table.present(ChartImage::Back));
    let p = loading(&mut r, &text, &frame(1));
    assert_eq!(at(&p, CHART_STAGE), GREEN);
    assert_eq!(at(&p, CHART_BACK), BLUE);
    assert_eq!(at(&p, BACK_MARK), RED);
    assert_eq!(at(&p, NO_BACK_MARK), BLACK);

    r.publish_chart_images(ChartTarget::Committed, [Some(&stage), None, None])
        .unwrap();
    let p = loading(&mut r, &text, &frame(1));
    assert_eq!(at(&p, CHART_STAGE), GREEN);
    assert_eq!(at(&p, CHART_BACK), BLACK);
    assert_eq!(at(&p, BACK_MARK), BLACK);
    assert_eq!(at(&p, NO_BACK_MARK), RED);

    // A new loading lifetime keeps the installation's images.
    let p = loading(&mut r, &text, &frame(2));
    assert_eq!(at(&p, CHART_STAGE), GREEN);
}

/// An attempt prepared on the installation draws the published images in
/// its play frames, and its fields read them.
#[test]
fn a_prepared_attempt_draws_the_published_images() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart();
    let mut r = offscreen(1_920, 1_080);
    let (b, _) = bundle(&r, dir.path(), &chart_look());
    install(&mut r, &b, 1);
    let side = b.side();
    let back = picture(ChartImage::Back, side, [64, 64], BLUE);
    r.publish_chart_images(ChartTarget::Committed, [None, Some(&back), None])
        .unwrap();
    let ctx = context(&GAUGES, false);
    let a = attempt(&r, &chart, &ctx);
    let token = r.stage(&b, &a, 2).unwrap();
    r.commit(token).unwrap();
    let v = view(0, 0);
    r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    let p = read_back(&r);
    assert_eq!(at(&p, CHART_STAGE), BLACK, "absent");
    assert_eq!(at(&p, CHART_BACK), BLUE);
    assert_eq!(at(&p, BACK_MARK), RED);
    assert_eq!(at(&p, NO_BACK_MARK), BLACK);
}

/// The play frame of a committed attempt, read back.
fn play_frame(r: &mut render::Renderer) -> Vec<u8> {
    let v = view(0, 0);
    r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    read_back(r)
}

/// An attempt staged before the images are published, or before they are
/// published again, takes the images valid at its commit: never the copy
/// it made when it was staged.
#[test]
fn a_staged_attempt_takes_the_images_valid_at_its_commit() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart();
    let mut r = offscreen(1_920, 1_080);
    let (b, _) = bundle(&r, dir.path(), &chart_look());
    install(&mut r, &b, 1);
    let side = b.side();
    let stage = picture(ChartImage::Stage, side, [64, 64], GREEN);
    let back = picture(ChartImage::Back, side, [64, 64], BLUE);
    let ctx = context(&GAUGES, false);

    // Staged, then published, then committed.
    let token = r.stage(&b, &attempt(&r, &chart, &ctx), 2).unwrap();
    r.publish_chart_images(ChartTarget::Committed, [Some(&stage), None, None])
        .unwrap();
    r.commit(token).unwrap();
    let p = play_frame(&mut r);
    assert_eq!(at(&p, CHART_STAGE), GREEN);
    assert_eq!(at(&p, CHART_BACK), BLACK);
    assert_eq!(at(&p, NO_BACK_MARK), RED);

    // Staged on those images, published again, then committed.
    let token = r.stage(&b, &attempt(&r, &chart, &ctx), 3).unwrap();
    r.publish_chart_images(ChartTarget::Committed, [None, Some(&back), None])
        .unwrap();
    r.commit(token).unwrap();
    let p = play_frame(&mut r);
    assert_eq!(at(&p, CHART_STAGE), BLACK, "the copy staged is not drawn");
    assert_eq!(at(&p, CHART_BACK), BLUE);
    assert_eq!(at(&p, BACK_MARK), RED);
    assert_eq!(at(&p, NO_BACK_MARK), BLACK);
}

/// A restored installation's array is made again, zeroed: its images are
/// unavailable until they are published into it, on this renderer and on
/// another one the installation is restored into.
#[test]
fn a_restore_makes_the_images_unavailable_until_republished() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (b, _) = bundle(&r, dir.path(), &chart_look());
    install(&mut r, &b, 1);
    let stage = picture(ChartImage::Stage, b.side(), [64, 64], GREEN);
    r.publish_chart_images(ChartTarget::Committed, [Some(&stage), None, None])
        .unwrap();
    let installation = r.installation(skin_model::ScreenKind::Play).unwrap();
    assert!(installation.loading().chart_images().is_some());

    r.restore(skin_model::ScreenKind::Play, &installation)
        .unwrap();
    assert_eq!(installation.loading().chart_images(), None);
    let p = loading(&mut r, &text, &frame(1));
    for rect in [CHART_STAGE, BACK_MARK, NO_BACK_MARK] {
        assert_eq!(
            at(&p, rect),
            BLACK,
            "{rect:?}: unavailable after the restore"
        );
    }
    r.publish_chart_images(ChartTarget::Committed, [Some(&stage), None, None])
        .unwrap();
    let p = loading(&mut r, &text, &frame(1));
    assert_eq!(at(&p, CHART_STAGE), GREEN);

    // Another renderer the installation is restored into.
    let mut other = offscreen(1_920, 1_080);
    let text = MenuText::new(&other);
    other
        .restore(skin_model::ScreenKind::Play, &installation)
        .unwrap();
    assert_eq!(installation.loading().chart_images(), None);
    let p = loading(&mut other, &text, &frame(1));
    assert_eq!(at(&p, CHART_STAGE), BLACK);
    assert_eq!(at(&p, NO_BACK_MARK), BLACK);
}

/// Two renderers sharing one installation, as a recovery restores it into
/// another: whatever the first publishes afterwards into the shared
/// loading state, the second's frames read the images unavailable, since
/// a table draws only against its own renderer's array.
#[test]
fn another_renderers_publication_never_draws_here() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart();
    let mut r1 = offscreen(1_920, 1_080);
    let (b, _) = bundle(&r1, dir.path(), &chart_look());
    install(&mut r1, &b, 1);
    let stage = picture(ChartImage::Stage, b.side(), [64, 64], GREEN);
    r1.publish_chart_images(ChartTarget::Committed, [Some(&stage), None, None])
        .unwrap();
    let installation = r1.installation(skin_model::ScreenKind::Play).unwrap();

    let mut r2 = offscreen(1_920, 1_080);
    let text2 = MenuText::new(&r2);
    r2.restore(skin_model::ScreenKind::Play, &installation)
        .unwrap();
    // The first publishes again, and commits an attempt, after the restore.
    r1.publish_chart_images(ChartTarget::Committed, [Some(&stage), None, None])
        .unwrap();
    let ctx = context(&GAUGES, false);
    let token = r1.stage(&b, &attempt(&r1, &chart, &ctx), 2).unwrap();
    r1.commit(token).unwrap();
    assert!(
        installation.loading().chart_images().is_some(),
        "the shared copy is R1's"
    );
    let p = loading(&mut r2, &text2, &frame(1));
    for rect in [CHART_STAGE, BACK_MARK, NO_BACK_MARK] {
        assert_eq!(at(&p, rect), BLACK, "{rect:?}: R1's table in R2's frame");
    }
    // R1 still draws its own.
    let text1 = MenuText::new(&r1);
    let p = loading(&mut r1, &text1, &frame(1));
    assert_eq!(at(&p, CHART_STAGE), GREEN);
}

/// With no play installation keeping a chart layer, nothing is published.
#[test]
fn no_chart_layer_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let none = r.publish_chart_images(ChartTarget::Committed, [None, None, None]);
    assert!(matches!(none, Err(RenderError::NoChartLayer)), "{none:?}");
    let (b, _) = bundle(&r, dir.path(), &Look::left([255, 0, 0, 255]));
    assert_eq!(b.chart_layer(), None);
    install(&mut r, &b, 1);
    let none = r.publish_chart_images(ChartTarget::Committed, [None, None, None]);
    assert!(matches!(none, Err(RenderError::NoChartLayer)), "{none:?}");
    let _ = ChartTable::ABSENT;
}

/// A new choice's invalidation: the last chart's published images, and
/// what their fields select, are unavailable in the loading frames, and an
/// attempt committed then reads them absent, until a publication.
#[test]
fn an_invalidation_makes_the_images_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (b, _) = bundle(&r, dir.path(), &chart_look());
    install(&mut r, &b, 1);
    let side = b.side();
    let stage = picture(ChartImage::Stage, side, [640, 480], GREEN);
    let back = picture(ChartImage::Back, side, [512, 384], BLUE);
    r.publish_chart_images(ChartTarget::Committed, [Some(&stage), Some(&back), None])
        .unwrap();
    let p = loading(&mut r, &text, &frame(1));
    assert_eq!(at(&p, CHART_BACK), BLUE);

    r.invalidate_chart_images();
    let installation = r.installation(skin_model::ScreenKind::Play).unwrap();
    assert_eq!(installation.loading().chart_images(), None);
    let p = loading(&mut r, &text, &frame(2));
    for rect in [CHART_STAGE, CHART_BACK, BACK_MARK, NO_BACK_MARK] {
        assert_eq!(at(&p, rect), BLACK, "{rect:?}: unavailable");
    }
    let ctx = context(&GAUGES, false);
    let token = r.stage(&b, &attempt(&r, &chart, &ctx), 2).unwrap();
    r.commit(token).unwrap();
    let p = play_frame(&mut r);
    assert_eq!(at(&p, CHART_STAGE), BLACK, "absent in the attempt");
    assert_eq!(at(&p, CHART_BACK), BLACK);
    assert_eq!(at(&p, NO_BACK_MARK), RED);
}

/// A play skin that gave up its chart layer, as a fallback leaves one: its
/// bundle keeps none.
fn layerless(r: &render::Renderer, dir: &std::path::Path, look: &Look) -> Arc<SkinBundle> {
    let mut skin = decl_support::skin(dir, look);
    skin.without_chart_layer();
    let atlas = skin
        .atlas(skin_model::Variant::Lo, r.atlas_limits(), &|| false)
        .unwrap();
    let clips = ClipTable::new(look.clip.into_iter().collect()).unwrap();
    Arc::new(SkinBundle::new(Arc::new(skin), atlas, clips, skin_model::Side::Left).unwrap())
}

/// A terminal absence on an installation with no chart layer: the absent
/// table needs no layer, so the fields read it and "no image" shows in
/// loading and in play, while a write there is still refused.
#[test]
fn an_absent_table_needs_no_chart_layer() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let b = layerless(&r, dir.path(), &chart_look());
    assert_eq!(b.chart_layer(), None);
    install(&mut r, &b, 1);
    let p = loading(&mut r, &text, &frame(1));
    assert_eq!(at(&p, NO_BACK_MARK), BLACK, "unavailable before");

    let table = r.publish_chart_absent(ChartTarget::Committed).unwrap();
    assert!(!table.present(ChartImage::Stage) && !table.present(ChartImage::Back));
    let p = loading(&mut r, &text, &frame(1));
    assert_eq!(at(&p, NO_BACK_MARK), RED);
    assert_eq!(at(&p, BACK_MARK), BLACK);
    assert_eq!(at(&p, CHART_STAGE), BLACK);
    let ctx = context(&GAUGES, false);
    let token = r.stage(&b, &attempt(&r, &chart, &ctx), 2).unwrap();
    r.commit(token).unwrap();
    let p = play_frame(&mut r);
    assert_eq!(at(&p, NO_BACK_MARK), RED, "and in the attempt");

    let back = picture(ChartImage::Back, 2_048, [64, 64], BLUE);
    let e = r
        .publish_chart_images(ChartTarget::Committed, [None, Some(&back), None])
        .unwrap_err();
    assert!(matches!(e, RenderError::NoChartLayer), "{e:?}");
    assert_eq!(e.chart_outcome(), ChartOutcome::NoTarget);
    // With no play installation, there is nothing to make absent.
    let mut bare = offscreen(1_920, 1_080);
    assert!(matches!(
        bare.publish_chart_absent(ChartTarget::Committed),
        Err(RenderError::NoChartLayer)
    ));
}

/// Blue, with the chart's images: another bundle than [`chart_look`]'s,
/// so a staging of it has its own array.
fn other_look() -> Look {
    Look {
        chart: true,
        ..Look::left([0, 0, 255, 255])
    }
}

/// A staged play installation with its own array (a pending skin's
/// specialization): images published into it are written there and not
/// into the committed array, held with the staging, and valid from its
/// commit on, though the commit makes a new resource generation; a discard
/// drops them, and a token that is no longer the staging's is refused.
#[test]
fn a_staged_array_takes_its_images_at_its_commit() {
    let dir = tempfile::tempdir().unwrap();
    let other_dir = tempfile::tempdir().unwrap();
    let chart = chart();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (b, _) = bundle(&r, dir.path(), &chart_look());
    install(&mut r, &b, 1);
    let stage = picture(ChartImage::Stage, b.side(), [640, 480], GREEN);
    r.publish_chart_images(ChartTarget::Committed, [Some(&stage), None, None])
        .unwrap();
    let ctx = context(&GAUGES, false);

    let (b2, _) = bundle(&r, other_dir.path(), &other_look());
    assert_ne!(b2.atlas_id(), b.atlas_id());
    let back = picture(ChartImage::Back, b2.side(), [512, 384], RED);
    let outgoing = r.chart_key().unwrap();
    let token = r.stage(&b2, &attempt(&r, &chart, &ctx), 2).unwrap();
    let table = r
        .publish_chart_images(ChartTarget::Staged(token), [None, Some(&back), None])
        .unwrap();
    assert!(table.present(ChartImage::Back) && !table.present(ChartImage::Stage));
    // The committed installation is untouched: its own table and texels.
    let p = loading(&mut r, &text, &frame(1));
    assert_eq!(at(&p, CHART_STAGE), GREEN);
    assert_eq!(at(&p, CHART_BACK), BLACK);
    r.commit(token).unwrap();
    // Keyed for the incoming installation: its presentation, its atlas,
    // the generation the commit made, on this renderer.
    let incoming = r.installation(skin_model::ScreenKind::Play).unwrap();
    let key = incoming.loading().chart_images().unwrap().key().unwrap();
    assert_eq!(Some(key), r.chart_key());
    assert_eq!(key.renderer, outgoing.renderer);
    assert_eq!(key.installation, incoming.loading().id());
    assert_ne!(key.installation, outgoing.installation);
    assert_eq!(key.atlas, b2.atlas_id().0);
    assert_eq!(key.generation, outgoing.generation + 1);
    let p = play_frame(&mut r);
    assert_eq!(at(&p, CHART_BACK), RED, "the staged image after the commit");
    assert_eq!(at(&p, CHART_STAGE), BLACK);
    assert_eq!(at(&p, BACK_MARK), BLUE);
    assert_eq!(at(&p, NO_BACK_MARK), BLACK);

    // Published into a staging that is then discarded: gone with it.
    let (b3, _) = bundle(&r, dir.path(), &chart_look());
    let token = r.stage(&b3, &attempt(&r, &chart, &ctx), 3).unwrap();
    let green = picture(ChartImage::Stage, b3.side(), [64, 64], GREEN);
    r.publish_chart_images(ChartTarget::Staged(token), [Some(&green), None, None])
        .unwrap();
    r.discard();
    let e = r
        .publish_chart_images(ChartTarget::Staged(token), [Some(&green), None, None])
        .unwrap_err();
    assert!(matches!(e, RenderError::ChartChanged), "{e:?}");
    let newer = r.stage(&b3, &attempt(&r, &chart, &ctx), 4).unwrap();
    let e = r
        .publish_chart_images(ChartTarget::Staged(token), [Some(&green), None, None])
        .unwrap_err();
    assert!(
        matches!(e, RenderError::ChartChanged),
        "an older token: {e:?}"
    );
    assert_eq!(e.chart_outcome(), ChartOutcome::Unavailable);
    r.commit(newer).unwrap();
    let p = play_frame(&mut r);
    assert_eq!(
        at(&p, CHART_STAGE),
        BLACK,
        "nothing of the discarded staging"
    );
    assert_eq!(at(&p, NO_BACK_MARK), RED);

    // An absent table staged: "no image" from the commit.
    let (b4, _) = bundle(&r, other_dir.path(), &other_look());
    let token = r.stage(&b4, &attempt(&r, &chart, &ctx), 5).unwrap();
    r.publish_chart_absent(ChartTarget::Staged(token)).unwrap();
    r.commit(token).unwrap();
    let p = play_frame(&mut r);
    assert_eq!(at(&p, NO_BACK_MARK), BLUE);
}

/// A staging's own chart images draw in its loading frames before its
/// commit, keyed for it: the committed installation's images do not, nor
/// do the staging's in the installed frames; the commit moves them with
/// its resources, the installed frames drawing them with no
/// republication. An absent table staged draws the "no image" mark from
/// the staging's frames on.
#[test]
fn staged_chart_images_draw_before_the_commit_and_move_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let other_dir = tempfile::tempdir().unwrap();
    let chart = chart();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (b, _) = bundle(&r, dir.path(), &chart_look());
    install(&mut r, &b, 1);
    let stage = picture(ChartImage::Stage, b.side(), [640, 480], GREEN);
    r.publish_chart_images(ChartTarget::Committed, [Some(&stage), None, None])
        .unwrap();
    let ctx = context(&GAUGES, false);
    let staged = |r: &mut render::Renderer, on: LoadingOn| {
        let t = r.loading_frame_on(on, &frame(1), &text).unwrap().unwrap();
        assert!(t.presented);
        read_back(r)
    };

    let (b2, _) = bundle(&r, other_dir.path(), &other_look());
    let back = picture(ChartImage::Back, b2.side(), [512, 384], RED);
    let token = r.stage(&b2, &attempt(&r, &chart, &ctx), 2).unwrap();
    let on = LoadingOn::Staged(token);
    let p = staged(&mut r, on);
    assert_eq!(
        at(&p, CHART_BACK),
        BLACK,
        "unavailable before its publication"
    );
    assert_eq!(at(&p, CHART_STAGE), BLACK, "never the committed one's");
    r.publish_chart_images(ChartTarget::Staged(token), [None, Some(&back), None])
        .unwrap();
    let p = staged(&mut r, on);
    assert_eq!(at(&p, CHART_BACK), RED);
    assert_eq!(at(&p, CHART_STAGE), BLACK);
    assert_eq!(at(&p, BACK_MARK), BLUE);
    let p = loading(&mut r, &text, &frame(1));
    assert_eq!(
        at(&p, CHART_STAGE),
        GREEN,
        "the installed frames keep theirs"
    );
    assert_eq!(at(&p, CHART_BACK), BLACK);
    r.commit(token).unwrap();
    let p = loading(&mut r, &text, &frame(1));
    assert_eq!(at(&p, CHART_BACK), RED, "moved with the commit");
    assert_eq!(at(&p, CHART_STAGE), BLACK);
    assert_eq!(at(&p, BACK_MARK), BLUE);

    // An absent table staged: "no image" from the staging's frames on.
    let (b3, _) = bundle(&r, dir.path(), &chart_look());
    let token = r.stage(&b3, &attempt(&r, &chart, &ctx), 3).unwrap();
    let on = LoadingOn::Staged(token);
    r.publish_chart_absent(ChartTarget::Staged(token)).unwrap();
    let p = staged(&mut r, on);
    assert_eq!(at(&p, NO_BACK_MARK), RED);
    assert_eq!(at(&p, CHART_BACK), BLACK);
    // A new choice's invalidation: unavailable again in the staged frames.
    r.invalidate_chart_images();
    let p = staged(&mut r, on);
    assert_eq!(at(&p, NO_BACK_MARK), BLACK);
}

/// A chart layer is charged with the atlas: one more layer of its side.
#[test]
fn the_chart_layer_is_charged() {
    let dir = tempfile::tempdir().unwrap();
    let r = offscreen(1_920, 1_080);
    let (b, _) = bundle(&r, dir.path(), &chart_look());
    let side = u64::from(b.side());
    let page = side * side;
    let (_, plain) = {
        let d = tempfile::tempdir().unwrap();
        let (p, _) = bundle(&r, d.path(), &Look::left([255, 0, 0, 255]));
        (d, p)
    };
    // The same pages, the reserved layer, and one more layer.
    assert_eq!(b.side(), 2_048, "a chart layer sets the least side");
    let pages = (plain.atlas_texels() / (u64::from(plain.side()) * u64::from(plain.side()))) - 1;
    assert_eq!(b.atlas_texels(), (pages + 1 + 1) * page);
}
