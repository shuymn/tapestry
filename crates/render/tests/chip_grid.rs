//! A result judge graph's chips drawn on WARP and read back: the model's
//! own quads over its own atlas (a generated chip grid sampled
//! `NearestBase`), through the real sprite pipeline (design.md "Rendering
//! and Skins", atlas array).
//!
//! The canvas is the fixed 1920 × 1080 layout on a 1920 × 1080 target, so
//! canvas pixels are target pixels.

mod common;

use bms_model::TimeUs;
use common::{input, offscreen, rgb_at, view};
use render::{
    AtlasPages, Blend, ClipTable, FrameInput, Gpu, PageLevels, PlaySkin, Renderer, SkinError,
    Sprite, SpriteBatch,
};
use skin_model::{
    Anchor, BestView, BuildError, Canvas, Ease, ElementDef, End, Filter, Fit, JudgeGraphDef,
    JudgeGraphKind, KeyDef, LampKey, Layout, Place, QuadBatch, Region, RegionDef, ResultChart,
    ResultDefs, ResultFrame, ResultLabels, ResultScore, ResultStatus, ResultTimers, ResultView,
    Root, SheetBuilder, Skin, SkinDef, SolidImageDef, Source, Timelines, TimerRef, Track, Variant,
    WidgetDefs, result_timer,
};

/// A skin whose frame is a fixed list of sprites.
struct Fixed(Vec<Sprite>);

impl PlaySkin for Fixed {
    fn prepare(&mut self, _: &Gpu, _: &render::AttemptInput<'_>) -> Result<(), SkinError> {
        Ok(())
    }

    fn build(&mut self, _: &FrameInput<'_>, out: &mut SpriteBatch) -> Result<(), BuildError> {
        for s in &self.0 {
            out.push(Blend::Alpha, *s);
        }
        Ok(())
    }

    fn capacity(&self) -> u32 {
        64
    }

    fn canvas(&self, target: [u32; 2]) -> Canvas {
        Canvas::new(target, Fit::Fixed)
    }
}

/// A result skin of one type-1 judge graph over `[x, 0, 10, 82]` (A's
/// height) on a white grid lit 4 × 4 of its 5 × 5 cells (image 1).
fn graph_skin(dir: &std::path::Path, x: i32) -> Skin {
    let mut b = SheetBuilder::new(&Root::new(dir).unwrap());
    for (size, grid, filter) in [
        ([8, 8], None, Filter::Nearest),
        ([5, 500], Some([4, 4]), Filter::NearestBase),
    ] {
        b.add_solid_image(SolidImageDef {
            size,
            color: [255; 4],
            filter,
            grid,
        })
        .unwrap();
    }
    let mut props = [None; 9];
    props[..4].copy_from_slice(&[Some(x), Some(0), Some(10), Some(82)]);
    let track = Track::new(
        &[KeyDef {
            at: TimeUs(0),
            props,
        }],
        Ease::Linear,
        End::Hold,
    )
    .unwrap();
    let def = SkinDef {
        fit: Fit::Fixed,
        regions: [RegionDef {
            anchor: Anchor::TopLeft,
            optional: false,
        }; Region::COUNT],
        whens: Vec::new(),
        sheet: b.finish(),
        lanes: None,
        elements: vec![ElementDef {
            source: Source::JudgeGraph(0),
            track,
            timer: TimerRef::Builtin(result_timer::SCREEN),
            visible: None,
            blend: Blend::Alpha,
            clip: None,
            place: Place::SCREEN,
        }],
        widgets: WidgetDefs::default(),
        bomb_up_to: None,
        close: None,
        loadend: None,
        present: None,
    };
    let graph = JudgeGraphDef {
        kind: JudgeGraphKind::Judgment,
        delay: TimeUs(0),
        reverse: false,
        back: false,
        chips: 1,
        solid: 0,
        fit: skin_model::GraphFit::Stretch,
    };
    Skin::new_result(
        def,
        ResultDefs {
            judge_graphs: vec![graph],
            ..ResultDefs::default()
        },
    )
    .unwrap()
}

/// The model's quads for one bin of `perfect` PERFECTs, and its atlas.
fn quads(skin: &Skin, r: &Renderer, perfect: u32) -> (Vec<Sprite>, skin_model::Atlas) {
    let atlas = skin
        .sheet()
        .atlas(Variant::Lo, r.atlas_limits(), &|| false)
        .unwrap();
    let mut res = skin.prepare_result();
    let v = ResultView {
        status: ResultStatus::Ready,
        chart: ResultChart::default(),
        score: ResultScore {
            clear: LampKey::Normal,
            ..ResultScore::default()
        },
        best: BestView::Unavailable,
        timing: None,
        gauge: None,
        deviations: None,
        timing_areas: None,
        timelines: Some(
            Timelines::new(
                TimeUs(1_000_000),
                vec![[perfect, 0, 0, 0, 0, 0, 0, 0, 0]],
                vec![perfect],
                None,
            )
            .unwrap(),
        ),
    };
    skin.update_result(&mut res, &v, &ResultLabels::default())
        .unwrap();
    let mut out = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let layout = Layout::AUTHOR;
    let frame = ResultFrame {
        now: TimeUs(0),
        target: [1_920, 1_080],
        layout: &layout,
        timers: ResultTimers {
            screen: Some(TimeUs(0)),
            fadeout: None,
        },
        app: skin_model::AppReadouts::LAUNCH,
    };
    skin.build_result(&mut res, &frame, atlas.uvs(), &mut out)
        .unwrap();
    (out.quads().to_vec(), atlas)
}

/// Installs `atlas`'s pages.
fn install(r: &mut Renderer, atlas: &skin_model::Atlas) {
    let levels: Vec<PageLevels<'_>> = atlas
        .pages()
        .iter()
        .map(|p| std::array::from_fn(|k| p.level(k).unwrap()))
        .collect();
    r.install_atlas(
        &AtlasPages {
            side: atlas.side(),
            pages: &levels,
            text_layer: false,
            chart_layer: false,
        },
        ClipTable::new(Vec::new()).unwrap(),
    )
    .unwrap();
}

/// Draws `sprites` over the installed atlas and reads the target back.
fn draw(r: &mut Renderer, sprites: Vec<Sprite>) -> Vec<u8> {
    r.set_skin(Box::new(Fixed(sprites))).unwrap();
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert!(timing.presented);
    r.read_back()
}

/// A 40-cell graph (35 chips, LOD about 1.29) and a 100-cell one (150
/// chips cut at 100, LOD about 2.61) in 82-pixel regions. Down the chips'
/// lit column (x + 5) every pixel is the PERFECT colour `0088ff` or the
/// cleared black of a gap, both present: the grid is read at level 0.
/// The same quads under `Filter::Nearest` read levels 1 and 3, whose box
/// filter blends gap rows: some pixel is neither.
#[test]
fn a_minified_chip_grid_reads_level_zero_on_the_device() {
    let mut r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    for (count, x) in [(35_u32, 0), (150, 100)] {
        let skin = graph_skin(dir.path(), x);
        let (sprites, atlas) = quads(&skin, &r, count);
        assert_eq!(sprites.len(), 1);
        let chip = sprites[0];
        assert_eq!(chip.filter, Filter::NearestBase);
        let [_, top, _, h] = chip.rect;
        let rows = top.ceil() as u32..(top + h).floor() as u32;
        let column = (x + 5) as u32;
        install(&mut r, &atlas);
        for (filter, crisp) in [(Filter::NearestBase, true), (Filter::Nearest, false)] {
            let p = draw(&mut r, vec![Sprite { filter, ..chip }]);
            let (w, _) = r.size();
            let (mut lit, mut gaps, mut blended) = (0, 0, 0);
            for y in rows.clone() {
                let rgb = rgb_at(&p, w, (column, y));
                let is = |want: [u8; 3]| rgb.iter().zip(want).all(|(&g, w)| g.abs_diff(w) <= 2);
                if is([0, 136, 255]) {
                    lit += 1;
                } else if is([0, 0, 0]) {
                    gaps += 1;
                } else {
                    blended += 1;
                }
            }
            if crisp {
                assert_eq!(blended, 0, "{count}: {lit} lit, {gaps} gaps");
                assert!(lit > 0 && gaps > 0, "{count}: {lit} lit, {gaps} gaps");
            } else {
                assert!(blended > 0, "{count}: the nearest level blends no row");
            }
        }
    }
}
