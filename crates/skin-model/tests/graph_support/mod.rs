//! Result graphs for the graph tests: a sheet of generated images (a
//! white solid and the four chip grids), a result skin of judge graphs,
//! its atlas, and a compositor that rasterizes a build's quads at chosen
//! canvas points as the renderer draws them.

#![allow(dead_code)]

use bms_model::TimeUs;
use skin_model::{
    Atlas, AtlasLimits, Blend, ElementDef, Filter, JudgeGraphDef, JudgeGraphKind, KeyDef, Layout,
    MIP_LEVELS, Place, Quad, QuadBatch, ResultDefs, ResultFrame, ResultScreen, ResultTimers,
    ResultView, Root, SheetBuilder, Skin, SkinDef, SolidImageDef, Source, Timelines, TimerRef,
    Track, Variant, result_timer,
};

/// The generated images: a white solid, then the chip grids lit 4 × 4,
/// 5 wide (no x gap), 5 tall (no y gap), and 5 × 5.
pub const SOLID: u32 = 0;
pub const GRID: u32 = 1;
pub const GRID_NO_GAP_X: u32 = 2;
pub const GRID_NO_GAP_Y: u32 = 3;
pub const GRID_NO_GAPS: u32 = 4;
const GRIDS: [[u8; 2]; 4] = [[4, 4], [5, 4], [4, 5], [5, 5]];

/// Room for the 5 × 500 grids with their gutters.
pub const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 1_024,
    max_layers: 8,
    reserved_layers: 1,
};

/// The opaque grey backdrop the compositor draws over.
pub const BACKDROP: f32 = 128.0;

/// A judge graph of `kind` on grid `chips`, with the background when
/// `back`, revealed over `delay_ms`.
pub fn graph(kind: JudgeGraphKind, chips: u32, back: bool, delay_ms: i64) -> JudgeGraphDef {
    JudgeGraphDef {
        kind,
        delay: TimeUs(delay_ms * 1_000),
        reverse: false,
        back,
        chips,
        solid: SOLID,
        fit: skin_model::GraphFit::Stretch,
    }
}

/// A still track over `[x, y, w, h]` at alpha `alpha`.
pub fn track(r: [i32; 4], alpha: i32) -> Track {
    let mut props = [None; 9];
    for (p, v) in props.iter_mut().zip(r) {
        *p = Some(v);
    }
    props[4] = Some(alpha);
    Track::new(
        &[KeyDef {
            at: TimeUs(0),
            props,
        }],
        skin_model::Ease::Linear,
        skin_model::End::Hold,
    )
    .unwrap()
}

/// An element drawing judge graph `i` over `r` at `alpha` on `timer`.
pub fn element(i: u32, r: [i32; 4], alpha: i32, timer: u16) -> ElementDef {
    source_element(Source::JudgeGraph(i), r, alpha, timer)
}

/// An element drawing `source` over `r` at `alpha` on `timer`.
pub fn source_element(source: Source, r: [i32; 4], alpha: i32, timer: u16) -> ElementDef {
    ElementDef {
        source,
        track: track(r, alpha),
        timer: TimerRef::Builtin(timer),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    }
}

/// An element drawing judge graph `i` over `r` on the screen timer,
/// turned `angle` degrees about `pivot`, with no native size.
pub fn placed_element(i: u32, r: [i32; 4], angle: i32, pivot: skin_model::Anchor) -> ElementDef {
    ElementDef {
        place: Place {
            pivot,
            ..Place::SCREEN
        },
        ..moving_element(Source::JudgeGraph(i), &[(0, r, angle)])
    }
}

/// An element on the screen timer.
pub fn screen_element(i: u32, r: [i32; 4], alpha: i32) -> ElementDef {
    element(i, r, alpha, result_timer::SCREEN)
}

/// The sheet of the generated images, in [`SOLID`]..[`GRID_NO_GAPS`]
/// order.
pub fn sheet_builder(dir: &std::path::Path) -> SheetBuilder {
    let mut b = SheetBuilder::new(&Root::new(dir).unwrap());
    let solid = b
        .add_solid_image(SolidImageDef {
            size: [8, 8],
            color: [255; 4],
            filter: Filter::Nearest,
            grid: None,
        })
        .unwrap();
    assert_eq!(solid, SOLID);
    for (i, g) in (GRID..).zip(GRIDS) {
        let at = b
            .add_solid_image(SolidImageDef {
                size: [5, 500],
                color: [255; 4],
                filter: Filter::NearestBase,
                grid: Some(g),
            })
            .unwrap();
        assert_eq!(at, i);
    }
    b
}

/// A result skin of `elements` drawing the judge graphs `graphs`.
pub fn skin(elements: Vec<ElementDef>, graphs: Vec<JudgeGraphDef>) -> Skin {
    skin_with(
        elements,
        ResultDefs {
            judge_graphs: graphs,
            ..ResultDefs::default()
        },
    )
}

/// A result skin of `elements` drawing the result widgets `widgets`.
pub fn skin_with(elements: Vec<ElementDef>, widgets: ResultDefs) -> Skin {
    load_with(elements, widgets).unwrap()
}

/// [`skin_with`], refused or not.
pub fn load_with(
    elements: Vec<ElementDef>,
    widgets: ResultDefs,
) -> Result<Skin, skin_model::LoadError> {
    load_sheet(elements, widgets, |_| {})
}

/// A result skin of `elements` whose sheet has `more` images added after
/// the generated ones, from index [`GRID_NO_GAPS`] + 1.
pub fn skin_more(elements: Vec<ElementDef>, more: impl FnOnce(&mut SheetBuilder)) -> Skin {
    load_sheet(elements, ResultDefs::default(), more).unwrap()
}

fn load_sheet(
    elements: Vec<ElementDef>,
    widgets: ResultDefs,
    more: impl FnOnce(&mut SheetBuilder),
) -> Result<Skin, skin_model::LoadError> {
    let dir = tempfile::tempdir().unwrap();
    let mut sheet = sheet_builder(dir.path());
    more(&mut sheet);
    let def = SkinDef {
        fit: skin_model::Fit::Fixed,
        regions: [skin_model::RegionDef {
            anchor: skin_model::Anchor::TopLeft,
            optional: false,
        }; skin_model::Region::COUNT],
        whens: Vec::new(),
        sheet: sheet.finish(),
        lanes: None,
        elements,
        widgets: skin_model::WidgetDefs::default(),
        bomb_up_to: None,
        close: None,
        present: None,
    };
    Skin::new_result(def, widgets)
}

/// Timelines of 1 s bins, every note judged.
pub fn timelines(bins: Vec<[u32; 9]>) -> Timelines {
    let notes = bins.iter().map(|b| b.iter().sum()).collect();
    Timelines::new(TimeUs(1_000_000), bins, notes, None).unwrap()
}

/// A ready view whose timelines are `bins`.
pub fn view(bins: Vec<[u32; 9]>) -> ResultView {
    ResultView {
        timelines: Some(timelines(bins)),
        ..ready()
    }
}

/// The result tests' ready view, its timelines replaced by callers.
pub fn ready() -> ResultView {
    use eval::Tally;
    use skin_model::{BestView, LampKey, ResultChart, ResultScore, ResultStatus};
    ResultView {
        status: ResultStatus::Ready,
        chart: ResultChart::default(),
        score: ResultScore {
            tally: Tally::default(),
            gauge_permille: 0,
            clear: LampKey::Normal,
            cleared: true,
            rate: None,
            next_rank: 0,
            duration_us: None,
        },
        best: BestView::Unavailable,
        timing: None,
        timelines: None,
        gauge: None,
        deviations: None,
        timing_areas: None,
    }
}

/// A result skin with its state, atlas and batch, on a 1920 × 1080
/// target (the fixed canvas at scale 1).
pub struct GFx {
    pub skin: Skin,
    pub res: ResultScreen,
    pub atlas: Atlas,
    pub batch: QuadBatch,
    pub layout: Layout,
    pub timers: ResultTimers,
}

impl GFx {
    pub fn new(skin: Skin) -> Self {
        let res = skin.prepare_result();
        let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        let atlas = skin.sheet().atlas(Variant::Lo, LIMITS, &|| false).unwrap();
        Self {
            skin,
            res,
            atlas,
            batch,
            layout: Layout::AUTHOR,
            timers: ResultTimers {
                screen: Some(TimeUs(0)),
                fadeout: None,
            },
        }
    }

    pub fn view(&mut self, view: &ResultView) {
        let labels = skin_model::ResultLabels::default();
        self.skin
            .update_result(&mut self.res, view, &labels)
            .unwrap();
    }

    /// Builds the frame at `ms`; every push accepted.
    pub fn frame(&mut self, ms: i64) {
        self.batch.clear();
        let frame = ResultFrame {
            now: TimeUs(ms * 1_000),
            target: [1_920, 1_080],
            layout: &self.layout,
            timers: self.timers,
        };
        self.skin
            .build_result(&mut self.res, &frame, self.atlas.uvs(), &mut self.batch)
            .unwrap();
        assert_eq!(self.batch.dropped(), 0);
    }

    pub fn quads(&self) -> &[Quad] {
        self.batch.quads()
    }

    /// The RGB the frame shows at canvas point `[x, y]` and the quads
    /// covering it, each quad read at the level its own filter picks
    /// ([`composite`]).
    pub fn pixel(&self, [x, y]: [f32; 2]) -> ([f32; 3], u32) {
        composite(self.quads(), &self.atlas, x, y, None)
    }

    /// The quads drawn on a chip grid, in push order.
    pub fn chips(&self) -> Vec<Quad> {
        self.quads()
            .iter()
            .filter(|q| q.filter == Filter::NearestBase)
            .copied()
            .collect()
    }

    /// The quads drawn on the white solid, in push order.
    pub fn back(&self) -> Vec<Quad> {
        self.quads()
            .iter()
            .filter(|q| q.filter == Filter::Nearest)
            .copied()
            .collect()
    }
}

/// The mip level the shader's sampler reads for quad `q` under `filter`:
/// level 0 for [`Filter::NearestBase`]; else the level of detail from both
/// screen-space uv derivatives (`log2` of the larger texel step a canvas
/// pixel takes, the canvas at scale 1), rounded to the nearest level and
/// clamped to the levels, as a nearest mipmap filter picks it.
pub fn level(q: &Quad, side: u32, filter: Filter) -> usize {
    if filter == Filter::NearestBase {
        return 0;
    }
    (lod(q, side).round().max(0.0) as usize).min(MIP_LEVELS - 1)
}

/// The level of detail of quad `q` (no rounding), as [`level`] computes it.
pub fn lod(q: &Quad, side: u32) -> f32 {
    let [l, t, r, b] = q.uv;
    let [.., w, h] = q.rect;
    let s = side as f32;
    ((r - l).abs() * s / w).max((b - t).abs() * s / h).log2()
}

/// The premultiplied texel of `atlas` at uv `[u, v]` on `layer`, nearest
/// at `level`.
pub fn sample(atlas: &Atlas, layer: u16, level: usize, [u, v]: [f32; 2]) -> [f32; 4] {
    let page = usize::from(layer - LIMITS.reserved_layers as u16);
    let side = atlas.side() >> level;
    let px = atlas.pages()[page].level(level).unwrap();
    let tx = ((u * side as f32).floor() as u32).min(side - 1);
    let ty = ((v * side as f32).floor() as u32).min(side - 1);
    let at = ((ty * side + tx) * 4) as usize;
    std::array::from_fn(|c| f32::from(px[at + c]) / 255.0)
}

/// The RGB (0 to 255) the renderer draws at canvas point `(x, y)`: the
/// quads in push order over an opaque grey [`BACKDROP`], each quad
/// covering the point (turned clockwise by its angle about its pivot, as
/// the vertex shader turns its corners) sampled at its uv there (nearest, at the level
/// [`level`] gives for its own filter, or for `force` when given), times
/// its colour premultiplied once, blended premultiplied over (design.md
/// "Rendering and Skins", premultiplied colour); and how many quads cover
/// the point.
pub fn composite(
    quads: &[Quad],
    atlas: &Atlas,
    x: f32,
    y: f32,
    force: Option<Filter>,
) -> ([f32; 3], u32) {
    let mut dst = [BACKDROP / 255.0; 3];
    let mut covered = 0;
    for q in quads {
        covered += u32::from(draw(&mut dst, q, Blend::Alpha, atlas, [x, y], force));
    }
    (dst.map(|c| c * 255.0), covered)
}

/// [`composite`] of a batch, each run's quads blended by its run's blend:
/// over, added, or multiplied by the straight colour (the premultiplied
/// product divided by its alpha, black at alpha 0; design.md "Rendering
/// and Skins").
pub fn composite_runs(batch: &QuadBatch, atlas: &Atlas, [x, y]: [f32; 2]) -> ([f32; 3], u32) {
    let mut dst = [BACKDROP / 255.0; 3];
    let mut covered = 0;
    for run in batch.runs() {
        for q in &batch.quads()[run.start as usize..run.end as usize] {
            covered += u32::from(draw(&mut dst, q, run.key.blend, atlas, [x, y], None));
        }
    }
    (dst.map(|c| c * 255.0), covered)
}

/// Draws quad `q` with `blend` into `dst` (0 to 1) at canvas point
/// `[x, y]`, as [`composite`] says; whether it covers the point.
fn draw(
    dst: &mut [f32; 3],
    q: &Quad,
    blend: Blend,
    atlas: &Atlas,
    [x, y]: [f32; 2],
    force: Option<Filter>,
) -> bool {
    // The point turned back about the quad's pivot.
    let [cx, cy] = q.pivot;
    let (s, k) = q.angle.sin_cos();
    let (dx, dy) = (x - cx, y - cy);
    let (x, y) = (cx + dx * k + dy * s, cy - dx * s + dy * k);
    let [qx, qy, qw, qh] = q.rect;
    if !(x >= qx && x < qx + qw && y >= qy && y < qy + qh) {
        return false;
    }
    let [l, t, r, b] = q.uv;
    let uv = [l + (r - l) * (x - qx) / qw, t + (b - t) * (y - qy) / qh];
    let k = level(q, atlas.side(), force.unwrap_or(q.filter));
    let texel = sample(atlas, q.layer, k, uv);
    let [cr, cg, cb, ca] = q.color;
    let tint = [cr * ca, cg * ca, cb * ca, ca];
    let src: [f32; 4] = std::array::from_fn(|c| texel[c] * tint[c]);
    let [.., a] = src;
    for (d, s) in dst.iter_mut().zip(src) {
        *d = match blend {
            Blend::Alpha => s + *d * (1.0 - a),
            Blend::Add => s + *d,
            Blend::Multiply if a > 0.0 => *d * s / a,
            Blend::Multiply => 0.0,
        };
    }
    true
}

/// `got` is `want` within `tolerance` (8-bit units) per channel.
#[track_caller]
pub fn near(got: [f32; 3], want: [f32; 3], tolerance: f32) {
    assert!(
        got.iter()
            .zip(want)
            .all(|(g, w)| (g - w).abs() <= tolerance),
        "{got:?}, expected {want:?}"
    );
}

/// Straight colour `rgb` (8-bit) at alpha `a` (0 to 1) over the backdrop.
pub fn over_backdrop(rgb: [f32; 3], a: f32) -> [f32; 3] {
    rgb.map(|c| c * a + BACKDROP * (1.0 - a))
}

/// An element drawing `source` on the screen timer whose track runs
/// linearly through `keys`, each `(ms, [x, y, w, h], angle)` at alpha 255.
pub fn moving_element(source: Source, keys: &[(i64, [i32; 4], i32)]) -> ElementDef {
    let keys: Vec<KeyDef> = keys
        .iter()
        .map(|&(ms, r, angle)| {
            let mut props = [None; 9];
            for (p, v) in props.iter_mut().zip(r) {
                *p = Some(v);
            }
            props[4] = Some(255);
            props[8] = Some(angle);
            KeyDef {
                at: TimeUs(ms * 1_000),
                props,
            }
        })
        .collect();
    ElementDef {
        source,
        track: Track::new(&keys, skin_model::Ease::Linear, skin_model::End::Hold).unwrap(),
        timer: TimerRef::Builtin(result_timer::SCREEN),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    }
}

/// Every quad of `quads` has a positive width and height and lies inside
/// `[x, y, w, h]` (unturned, within `eps`).
#[track_caller]
pub fn inside(quads: &[Quad], [x, y, w, h]: [f32; 4], eps: f32) {
    for q in quads {
        let [qx, qy, qw, qh] = q.rect;
        assert!(qw > 0.0 && qh > 0.0, "empty quad {:?}", q.rect);
        assert!(
            qx >= x - eps && qy >= y - eps && qx + qw <= x + w + eps && qy + qh <= y + h + eps,
            "{:?} outside {:?}",
            q.rect,
            [x, y, w, h]
        );
    }
}
