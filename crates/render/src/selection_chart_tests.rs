//! The selection's chart images (S10-12): the cursor's chart's stage file
//! and banner written into the committed selection installation's array,
//! keyed to it, a write of the set the array already holds skipped, a
//! misfit clearing the record, and none without a selection installation.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;

use bms_model::TimeUs;
use render::{ChartTarget, ClipTable, RenderError, Renderer, SkinBundle};
use skin_model::chart_image::{ChartDir, ChartImage, ChartStretch, Fitted, read};
use skin_model::{
    Anchor, Ease, ElementDef, End, Filter, Fit, KeyDef, Layout, Place, Region, RegionDef, Root,
    ScreenKind, Selection, SelectionDefs, SelectionFrame, SelectionTimers, Side, Skin, SkinDef,
    SolidImageDef, Source, TimerRef, Track, Variant, WidgetDefs, selection_timer,
};

use crate::test_common::{offscreen, read_back, rgb_at};

const W: u32 = 1_920;
const H: u32 = 1_080;
const BLACK: [u8; 3] = [0, 0, 0];
const RED: [u8; 3] = [200, 10, 10];
const GREEN: [u8; 3] = [10, 200, 10];
/// Where the stage file and the banner are read back.
const STAGE_AT: (u32, u32) = (200, 300);
const BANNER_AT: (u32, u32) = (1_000, 100);

fn element(source: Source, [x, y, w, h]: [i32; 4]) -> ElementDef {
    let mut props = [None; 9];
    props[..4].copy_from_slice(&[Some(x), Some(y), Some(w), Some(h)]);
    ElementDef {
        source,
        track: Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props,
            }],
            Ease::Linear,
            End::Hold,
        )
        .unwrap(),
        timer: TimerRef::Builtin(selection_timer::SCREEN),
        visible: None,
        blend: skin_model::Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    }
}

/// A selection skin over `dir` drawing the stage file at (0, 0) 400×600
/// and the banner at (800, 0) 400×200, both filling their destinations.
fn chart_skin(dir: &Path) -> Skin {
    let mut b = skin_model::SheetBuilder::new(&Root::new(dir).unwrap());
    b.add_solid_image(SolidImageDef {
        size: [16, 16],
        color: [255, 255, 255, 255],
        filter: Filter::Nearest,
        grid: None,
    })
    .unwrap();
    let chart = |which| Source::ChartImage {
        which,
        filter: Filter::Nearest,
        stretch: ChartStretch::Fill,
    };
    let def = SkinDef {
        fit: Fit::Fixed,
        regions: [RegionDef {
            anchor: Anchor::TopLeft,
            optional: false,
        }; Region::COUNT],
        whens: Vec::new(),
        sheet: b.finish(),
        lanes: None,
        elements: vec![
            element(chart(ChartImage::Stage), [0, 0, 400, 600]),
            element(chart(ChartImage::Banner), [800, 0, 400, 200]),
        ],
        widgets: WidgetDefs::default(),
        bomb_up_to: None,
        close: None,
        loadend: None,
        present: None,
    };
    Skin::new_selection(def, SelectionDefs::default()).unwrap()
}

fn bundle(r: &Renderer, dir: &Path) -> Arc<SkinBundle> {
    let skin = chart_skin(dir);
    let atlas = skin
        .atlas(Variant::Lo, r.atlas_limits(), &|| false)
        .unwrap();
    Arc::new(
        SkinBundle::new(
            Arc::new(skin),
            atlas,
            ClipTable::new(Vec::new()).unwrap(),
            Side::Left,
        )
        .unwrap(),
    )
}

/// A `w`×`h` picture of `rgb` fitted for `which`'s slot of a chart layer of
/// side `side`.
fn picture(which: ChartImage, side: u32, [w, h]: [u32; 2], [r, g, b]: [u8; 3]) -> Fitted {
    let d = tempfile::tempdir().unwrap();
    let file = File::create(d.path().join("p.png")).unwrap();
    let mut e = png::Encoder::new(BufWriter::new(file), w, h);
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

/// Builds, submits and reads back a selection frame of `sel`.
fn pixels(r: &mut Renderer, sel: &mut Selection) -> Vec<u8> {
    let layout = Layout::AUTHOR;
    let frame = SelectionFrame {
        now: TimeUs(0),
        target: [W, H],
        layout: &layout,
        timers: SelectionTimers {
            screen: Some(TimeUs(0)),
            ..SelectionTimers::default()
        },
        app: skin_model::AppReadouts::LAUNCH,
        pointer: None,
    };
    r.build_selection(sel, &frame, &[]).unwrap();
    assert!(r.submit_selection(&[]).unwrap().presented);
    read_back(r)
}

/// The colours at the stage file and the banner.
fn frame(r: &mut Renderer, sel: &mut Selection) -> ([u8; 3], [u8; 3]) {
    let px = pixels(r, sel);
    (rgb_at(&px, W, STAGE_AT), rgb_at(&px, W, BANNER_AT))
}

/// The selection's chart images: without a selection installation there
/// is no target; a write into the committed array is keyed to it and draws
/// in its frames; a write of the set the array holds is skipped and one of
/// another set writes; a misfit clears the record, so the same set writes
/// again; an absent publication is keyed to the array and draws nothing;
/// and a restore makes the old tables undrawable.
#[test]
fn selection_chart_images_are_written_into_the_selection_array() {
    let mut r = offscreen(W, H);
    let dir = tempfile::tempdir().unwrap();
    let b = bundle(&r, dir.path());
    assert!(b.chart_layer().is_some());
    let side = b.side();
    let red = picture(ChartImage::Stage, side, [64, 48], RED);
    let banner = picture(ChartImage::Banner, side, [60, 16], GREEN);
    // Higher than the banner's slot.
    let tall = picture(ChartImage::Stage, side, [600, 600], RED);

    // No selection installation: no target, nothing written.
    assert_eq!(r.selection_chart_key(), None);
    assert!(matches!(
        r.write_selection_chart([Some(&red), None, None], 1),
        Err(RenderError::NoChartLayer)
    ));
    assert!(matches!(
        r.publish_chart_absent(ChartTarget::Selection),
        Err(RenderError::NoChartLayer)
    ));
    assert_eq!(r.selection_chart_writes(), 0);

    let token = r.install(ScreenKind::Selection, &b, 1).unwrap();
    r.commit(token).unwrap();
    let key = r.selection_chart_key();
    assert!(key.is_some());
    let t1 = r
        .write_selection_chart([Some(&red), None, Some(&banner)], 1)
        .unwrap();
    assert_eq!(t1.key(), key, "keyed to the selection array");
    assert!(t1.present(ChartImage::Stage) && t1.present(ChartImage::Banner));
    assert_eq!(r.selection_chart_writes(), 1);
    let mut sel = b.skin().prepare_selection();
    assert_eq!(frame(&mut r, &mut sel), (BLACK, BLACK), "none set");
    sel.set_chart_images(Some(t1));
    assert_eq!(frame(&mut r, &mut sel), (RED, GREEN));

    // The set the array holds: skipped; another set writes.
    let again = r
        .write_selection_chart([Some(&red), None, Some(&banner)], 1)
        .unwrap();
    assert_eq!(again, t1);
    assert_eq!(r.selection_chart_writes(), 1, "the array's set");
    let t2 = r
        .write_selection_chart([Some(&red), None, None], 2)
        .unwrap();
    assert_eq!(r.selection_chart_writes(), 2);
    assert!(!t2.present(ChartImage::Banner));

    // A misfit (a stage file's fit higher than the banner's slot): the
    // record cleared, so set 2 writes again.
    let misfit = r
        .write_selection_chart([None, None, Some(&tall)], 3)
        .unwrap_err();
    assert!(matches!(
        misfit,
        RenderError::ChartImage {
            which: ChartImage::Banner
        }
    ));
    assert_eq!(r.selection_chart_writes(), 2);
    r.write_selection_chart([Some(&red), None, None], 2)
        .unwrap();
    assert_eq!(r.selection_chart_writes(), 3, "the record was cleared");

    // Absent: keyed to the array, drawing nothing.
    let absent = r.publish_chart_absent(ChartTarget::Selection).unwrap();
    assert_eq!(absent.key(), key);
    sel.set_chart_images(Some(absent));
    assert_eq!(frame(&mut r, &mut sel), (BLACK, BLACK));

    // A restore: a new array, the old tables undrawable.
    let installation = r.installation(ScreenKind::Selection).unwrap();
    r.restore(ScreenKind::Selection, &installation).unwrap();
    assert_ne!(r.selection_chart_key(), key);
    sel.set_chart_images(Some(t1));
    assert_eq!(frame(&mut r, &mut sel), (BLACK, BLACK));
}
