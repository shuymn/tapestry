//! A skin's atlas array, canvas and clips drawn on WARP and read back, and
//! one renderer resized across aspects, minimized and restored with its
//! play state kept.
//!
//! An adapted canvas (`Fit::Adapt`) on 1920×1080, 1920×1200 or 2560×1080
//! is the target's own size at scale 1, and the fixed canvas on 1920×1080
//! is the layout itself, so canvas pixels are target pixels unless a test
//! says otherwise. Expected values are worked out in each test.

mod common;

use std::f32::consts::FRAC_PI_2;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, input, lane, note_center_in, offscreen, view, warp, white_rows_in};
use eval::{JudgeEvent, JudgeEventKind, Section};
use render::{
    AtlasPages, Blend, ClipTable, FrameInput, Gpu, MAX_SPRITES, PageLevels, PlaySkin, RenderError,
    Renderer, SkinError, Sprite, SpriteBatch, UploadError, solid_uv,
};
use rules::Judgment;
use skin_model::{Canvas, ClipId, Filter, Fit};

type Placed = (Blend, Option<ClipId>, Sprite);

/// A skin whose frame is `layout` of its canvas's size.
struct Scene {
    fit: Fit,
    layout: fn([f32; 2]) -> Vec<Placed>,
    capacity: u32,
}

impl Scene {
    fn new(fit: Fit, layout: fn([f32; 2]) -> Vec<Placed>) -> Self {
        Self {
            fit,
            layout,
            capacity: MAX_SPRITES as u32,
        }
    }
}

impl PlaySkin for Scene {
    fn prepare(&mut self, _: &Gpu, _: &Chart, _: Option<&Section>) -> Result<(), SkinError> {
        Ok(())
    }

    fn build(&mut self, f: &FrameInput<'_>, out: &mut SpriteBatch) {
        for (blend, clip, sprite) in (self.layout)(self.canvas(f.target).size()) {
            out.push_clipped(blend, clip, sprite);
        }
    }

    fn capacity(&self) -> u32 {
        self.capacity
    }

    fn canvas(&self, target: [u32; 2]) -> Canvas {
        Canvas::new(target, self.fit)
    }
}

/// One frame of the renderer's skin, presented, read back.
fn frame(r: &mut Renderer) -> Vec<u8> {
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert!(timing.presented);
    assert_eq!(timing.dropped, 0);
    r.read_back()
}

/// The RGB of pixel (x, y) of a read back of `r`'s target.
fn px(r: &Renderer, pixels: &[u8], x: f32, y: f32) -> [u8; 3] {
    let (w, h) = r.size();
    assert_eq!(pixels.len(), (w * h * 4) as usize);
    let i = ((y as u32 * w + x as u32) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

/// `got` is `want` within `tolerance` per channel.
#[track_caller]
fn near(got: [u8; 3], want: [u8; 3], tolerance: u8) {
    assert!(
        got.iter()
            .zip(want)
            .all(|(&g, w)| g.abs_diff(w) <= tolerance),
        "{got:?}, expected {want:?}"
    );
}

const WHITE: [u8; 3] = [255; 3];
const BLACK: [u8; 3] = [0; 3];
const RED: [u8; 3] = [255, 0, 0];
const BLUE: [u8; 3] = [0, 0, 255];

/// A `side`-texel-square level of RGBA8 texels `f(x)` by column.
fn level(side: u32, f: impl Fn(u32) -> [u8; 4]) -> Vec<u8> {
    (0..side * side).flat_map(|i| f(i % side)).collect()
}

/// The two 256-texel pages the tests upload, every level of each:
/// - page 1 (layer 1): level 0 opaque red, levels 1 to 3 opaque blue;
/// - page 2 (layer 2): level 0 opaque white left of column 128 and opaque
///   black from it, levels 1 to 3 opaque white.
fn page_levels() -> [[Vec<u8>; 4]; 2] {
    let red = |_: u32| [255, 0, 0, 255];
    let blue = |_: u32| [0, 0, 255, 255];
    let white = |_: u32| [255; 4];
    let halves = |x: u32| if x < 128 { [255; 4] } else { [0, 0, 0, 255] };
    [
        [
            level(256, red),
            level(128, blue),
            level(64, blue),
            level(32, blue),
        ],
        [
            level(256, halves),
            level(128, white),
            level(64, white),
            level(32, white),
        ],
    ]
}

fn pages_of(levels: &[[Vec<u8>; 4]; 2]) -> [PageLevels<'_>; 2] {
    levels
        .each_ref()
        .map(|[a, b, c, d]| [&a[..], &b[..], &c[..], &d[..]])
}

/// Installs the two pages with `clips`.
fn install(r: &mut Renderer, clips: Vec<[f32; 4]>) {
    let levels = page_levels();
    let pages = pages_of(&levels);
    r.install_atlas(
        &AtlasPages {
            side: 256,
            pages: &pages,
        },
        ClipTable::new(clips).unwrap(),
    )
    .unwrap();
    assert_eq!(r.atlas_size(), (256, 3));
}

/// The built-in atlas's solid block in `color` over `rect`.
fn solid(rect: [f32; 4], color: [f32; 4]) -> Sprite {
    Sprite {
        rect,
        uv: solid_uv(),
        color,
        ..Sprite::default()
    }
}

/// Atlas `layer`'s `uv` over `rect`, sampled with `filter`.
fn paged(layer: u16, filter: Filter, rect: [f32; 4], uv: [f32; 4]) -> Sprite {
    Sprite {
        rect,
        uv,
        color: [1.0; 4],
        layer,
        filter,
        ..Sprite::default()
    }
}

const WHOLE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// The scene of [`one_scene_at_three_aspects`].
fn aspects_layout([w, h]: [f32; 2]) -> Vec<Placed> {
    let cell = 120.0 / 128.0;
    let wide = [127.0 / 256.0, 0.25, 129.0 / 256.0, 0.75];
    [
        Sprite {
            uv: [cell, cell, 1.0, 1.0],
            ..solid([100.0, 100.0, 2.0, 2.0], [1.0; 4])
        },
        paged(1, Filter::Linear, [100.0, 300.0, 64.0, 64.0], WHOLE),
        paged(1, Filter::Nearest, [200.0, 300.0, 64.0, 64.0], WHOLE),
        paged(1, Filter::Linear, [300.0, 300.0, 512.0, 512.0], WHOLE),
        paged(2, Filter::Linear, [900.0, 300.0, 200.0, 200.0], wide),
        paged(2, Filter::Nearest, [1_200.0, 300.0, 200.0, 200.0], wide),
        solid([w - 300.0, h - 200.0, 200.0, 100.0], [1.0; 4]),
        Sprite {
            pivot: [w - 400.0, 600.0],
            angle: FRAC_PI_2,
            ..solid([w - 400.0, 600.0, 200.0, 40.0], [1.0; 4])
        },
    ]
    .into_iter()
    .map(|s| (Blend::Alpha, None, s))
    .collect()
}

/// One adapted scene drawn by one renderer on a 16:9, a 16:10 and a 21:9
/// target, with the two pages over the built-in atlas. At each aspect the
/// canvas is the target at scale 1, so:
///
/// - The built-in solid block (its texels 120..128) into 2×2 pixels at
///   (100, 100): its uv is scaled by 128 / 256 to texels 120..128 of the
///   256-texel array, 4 texels a pixel, level 2 by the derivatives. Layer
///   0 is read at its nearest level-0 texel: white. Sampled at level 2,
///   which holds nothing for layer 0, it would be black; with its uv not
///   rescaled it would read texels 240..256, also black.
/// - Page 1 whole into 64×64 pixels, smoothed at (100, 300) and nearest at
///   (200, 300): 4 texels a pixel, level 2 under either filter, blue.
///   Into 512×512 at (300, 300), magnified: level 0, red. Sampled at
///   level 0 alone the small ones would be red; levels left out, black.
/// - Page 2's texels 127..129 across, rows 64..192, into 200×200 pixels,
///   smoothed at (900, 300) and nearest at (1200, 300); texel 127 is
///   white, 128 black. The rect's pixel column 75 samples at texel 127 +
///   75.5 × 2 / 200 = 127.755: bilinear, 0.255 of the black texel, so 255
///   × 0.745 = 190; nearest, texel 127, 255. Column 125 samples at
///   128.255: bilinear, 255 × 0.245 = 62.5; nearest, 0. With the filter
///   bit ignored both rects are bilinear.
/// - A white 200×100 rect 300 and 200 pixels in from the canvas's right
///   and bottom edges, its middle at (w − 200, h − 150), and a white bar
///   200×40 from (w − 400, 600) turned a quarter clockwise about its top
///   left, hanging down over x w − 440..w − 400, y 600..800. Under the
///   fixed letterboxed canvas instead, 16:10 shows the layout 1080 tall
///   from row 60, so the rect would be drawn at rows 1060..1160 instead of
///   1000..1100, black at row 1050; 21:9 shows it from column 320, which
///   puts the canvas's x 2260 past the target.
#[test]
fn one_scene_at_three_aspects() {
    let mut r = offscreen(1_920, 1_080);
    install(&mut r, Vec::new());
    r.set_skin(Box::new(Scene::new(Fit::Adapt, aspects_layout)))
        .unwrap();
    let mut checked = 0;
    for (w, h) in [(1_920, 1_080), (1_920, 1_200), (2_560, 1_080)] {
        r.resize(w, h);
        assert_eq!(r.target(), [w, h]);
        let p = frame(&mut r);
        let at = |x: f32, y: f32| px(&r, &p, x, y);
        near(at(100.0, 100.0), WHITE, 2);
        near(at(130.0, 330.0), BLUE, 2);
        near(at(230.0, 330.0), BLUE, 2);
        near(at(550.0, 550.0), RED, 2);
        near(at(975.0, 400.0), [190; 3], 3);
        near(at(1_025.0, 400.0), [62; 3], 3);
        near(at(1_275.0, 400.0), WHITE, 2);
        near(at(1_325.0, 400.0), BLACK, 2);
        let (w, h) = (w as f32, h as f32);
        near(at(w - 200.0, h - 150.0), WHITE, 2);
        near(at(w - 50.0, h - 150.0), BLACK, 2);
        near(at(w - 420.0, 750.0), WHITE, 2);
        near(at(w - 300.0, 620.0), BLACK, 2);
        checked += 1;
    }
    assert_eq!(checked, 3);
}

/// Bands across the whole canvas, 100 rows each, under clips 0 (the left
/// half), 1 (the right half) and 2 (under half a pixel wide: no pixel):
/// - rows 0..100 white under clip 0; 100..200 red under clip 1;
/// - 200..300 green unclipped; 300..400 white under clip 0;
/// - 400..500 blue under clip 2; 500..600 yellow unclipped;
/// - 600..700 grey (0.2) then red at alpha 0.6 added over it, both under
///   clip 1: one clip across a change of blend.
fn clips_layout(_: [f32; 2]) -> Vec<Placed> {
    let band = |row: f32, color| solid([0.0, row, 1_920.0, 100.0], color);
    let (c0, c1, c2) = (Some(ClipId(0)), Some(ClipId(1)), Some(ClipId(2)));
    vec![
        (Blend::Alpha, c0, band(0.0, [1.0; 4])),
        (Blend::Alpha, c1, band(100.0, [1.0, 0.0, 0.0, 1.0])),
        (Blend::Alpha, None, band(200.0, [0.0, 1.0, 0.0, 1.0])),
        (Blend::Alpha, c0, band(300.0, [1.0; 4])),
        (Blend::Alpha, c2, band(400.0, [0.0, 0.0, 1.0, 1.0])),
        (Blend::Alpha, None, band(500.0, [1.0, 1.0, 0.0, 1.0])),
        (Blend::Alpha, c1, band(600.0, [0.2, 0.2, 0.2, 1.0])),
        (Blend::Additive, c1, band(600.0, [1.0, 0.0, 0.0, 0.6])),
    ]
}

const CLIPS: [[f32; 4]; 3] = [
    [0.0, 0.0, 960.0, 1_080.0],
    [960.0, 0.0, 960.0, 1_080.0],
    [10.0, 10.0, 0.4, 5.0],
];

/// Consecutive runs with differing, absent and empty clips, on the fixed
/// canvas at 1920×1080 and then, by the same renderer, at 2560×1080,
/// where the layout is drawn from column 320 and each clip with it: clip 0
/// covers pixels 320..1280, clip 1 1280..2240. Each band is read at canvas
/// x `left` (in clip 0) and `right` (in clip 1): 480 and 1440 at 16:9,
/// 800 and 1800 at 21:9, pixels 1120 and 2120.
///
/// Per band, (left, right): white, black; black, red; green, green;
/// white, black; black, black (the empty clip draws nothing); yellow,
/// yellow; black, (204, 51, 51) (0.2 grey plus 0.6 red). A scissor left
/// set after a clipped run would blacken the right of the unclipped bands
/// after clip 0; the empty clip drawn unclipped would make its band blue.
/// Scissors kept from 16:9 at 21:9 would cover pixels 0..960 and
/// 960..1920, canvas −320..640 and 640..1600, leaving both read columns
/// black there.
#[test]
fn clips_change_between_runs() {
    let mut r = offscreen(1_920, 1_080);
    install(&mut r, CLIPS.to_vec());
    r.set_skin(Box::new(Scene::new(Fit::Fixed, clips_layout)))
        .unwrap();
    let expected: [(f32, [u8; 3], [u8; 3]); 7] = [
        (50.0, WHITE, BLACK),
        (150.0, BLACK, RED),
        (250.0, [0, 255, 0], [0, 255, 0]),
        (350.0, WHITE, BLACK),
        (450.0, BLACK, BLACK),
        (550.0, [255, 255, 0], [255, 255, 0]),
        (650.0, BLACK, [204, 51, 51]),
    ];
    let mut checked = 0;
    for (w, origin, left, right) in [(1_920, 0.0, 480.0, 1_440.0), (2_560, 320.0, 800.0, 1_800.0)] {
        r.resize(w, 1_080);
        let p = frame(&mut r);
        for (row, l, rt) in expected {
            near(px(&r, &p, origin + left, row), l, 2);
            near(px(&r, &p, origin + right, row), rt, 2);
            checked += 1;
        }
    }
    assert_eq!(checked, 14);
}

/// Page 1 smoothed into 64×64 pixels at (100, 300) and at (1100, 300),
/// both under clip 0, the left half of the fixed canvas.
fn refused_layout(_: [f32; 2]) -> Vec<Placed> {
    let c0 = Some(ClipId(0));
    vec![
        (
            Blend::Alpha,
            c0,
            paged(1, Filter::Linear, [100.0, 300.0, 64.0, 64.0], WHOLE),
        ),
        (
            Blend::Alpha,
            c0,
            paged(1, Filter::Linear, [1_100.0, 300.0, 64.0, 64.0], WHOLE),
        ),
    ]
}

/// An atlas the device's limits or its own levels refuse leaves the atlas
/// and clips in use: frames go on drawing page 1's level 2, blue, at
/// (130, 330), and nothing at (1130, 330), outside clip 0. Each refused
/// install offers the whole canvas as clip 0, which would draw the second
/// sprite blue. The atlas replaced by the built-in one alone has no layer
/// 1 and no blue texel: the first sprite would not be blue.
#[test]
fn a_refused_atlas_keeps_the_one_in_use() {
    let mut r = offscreen(1_920, 1_080);
    install(&mut r, vec![CLIPS[0]]);
    r.set_skin(Box::new(Scene::new(Fit::Fixed, refused_layout)))
        .unwrap();
    let check = |r: &mut Renderer| {
        let p = frame(r);
        near(px(r, &p, 130.0, 330.0), BLUE, 2);
        near(px(r, &p, 1_130.0, 330.0), BLACK, 2);
        assert_eq!(r.atlas_size(), (256, 3));
    };
    check(&mut r);
    let max = r.atlas_limits().max_side;
    let whole = || ClipTable::new(vec![[0.0, 0.0, 1_920.0, 1_080.0]]).unwrap();
    let past_side = AtlasPages {
        side: max * 2,
        pages: &[],
    };
    let e = r.install_atlas(&past_side, whole()).unwrap_err();
    assert!(
        matches!(e, RenderError::Atlas(UploadError::Side { side, max: m }) if side == max * 2 && m == max),
        "{e:?}"
    );
    check(&mut r);
    let empty: PageLevels<'_> = [&[]; 4];
    let layers = r.atlas_limits().max_layers as usize;
    let all = vec![empty; layers];
    let past_layers = AtlasPages {
        side: 256,
        pages: &all,
    };
    let e = r.install_atlas(&past_layers, whole()).unwrap_err();
    assert!(
        matches!(e, RenderError::Atlas(UploadError::Layers { .. })),
        "{e:?}"
    );
    check(&mut r);
    let short = AtlasPages {
        side: 256,
        pages: &all[..1],
    };
    let e = r.install_atlas(&short, whole()).unwrap_err();
    assert!(
        matches!(
            e,
            RenderError::Atlas(UploadError::Level {
                page: 0,
                level: 0,
                ..
            })
        ),
        "{e:?}"
    );
    check(&mut r);
}

/// 9,999 transparent sprites, then a white 50×50 one at (100, 100).
fn crowded_layout(_: [f32; 2]) -> Vec<Placed> {
    let clear = solid([0.0, 0.0, 10.0, 10.0], [0.0; 4]);
    let mut out = vec![(Blend::Alpha, None, clear); 9_999];
    out.push((
        Blend::Alpha,
        None,
        solid([100.0, 100.0, 50.0, 50.0], [1.0; 4]),
    ));
    out
}

/// A skin asking for 10,000 sprites gets them all: the last, white at
/// (125, 125), is drawn and none dropped. Sized for `MAX_SPRITES` (8,192)
/// instead, 1,808 would be dropped, the white one among them. One asking
/// for more sprites than the device's largest buffer holds is refused,
/// and the skin in use goes on drawing.
#[test]
fn a_skin_sizes_the_batch() {
    let mut r = offscreen(1_920, 1_080);
    let mut crowded = Scene::new(Fit::Fixed, crowded_layout);
    crowded.capacity = 10_000;
    r.set_skin(Box::new(crowded)).unwrap();
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert_eq!((timing.sprites, timing.dropped), (10_000, 0));
    let p = r.read_back();
    near(px(&r, &p, 125.0, 125.0), WHITE, 2);
    let mut huge = Scene::new(Fit::Fixed, clips_layout);
    huge.capacity = u32::MAX;
    let e = r.set_skin(Box::new(huge)).unwrap_err();
    assert!(
        matches!(
            e,
            RenderError::Capacity {
                sprites: u32::MAX,
                ..
            }
        ),
        "{e:?}"
    );
    let p = frame(&mut r);
    near(px(&r, &p, 125.0, 125.0), WHITE, 2);
}

/// One renderer with the simple skin, notes at 2.0 s in lanes 5 and 7 and
/// the lane 7 one judged, resized between frames shown at 1.8 s: the
/// lane 5 note is 0.4 beats ahead, layout row 613.33, at the layout's
/// middle column of lane 5, 544, and lane 7 (672) stays empty, the
/// judgment kept across every resize. Per target, (lane 5 column, lane 7
/// column, note row):
/// - 1280×1024, layout at 2/3 from row 152: (362, 448, 560.89);
/// - 1920×1080: (544, 672, 613.33);
/// - 2560×1080, from column 320: (864, 992, 613.33);
/// - 1920×1200, from row 60: (544, 672, 673.33);
/// - minimized: built but neither drawn nor presented;
/// - 1920×1200 again, the size before the minimize, then 1280×1024.
#[test]
fn one_renderer_resized_minimized_and_restored() {
    let chart = ChartBuilder::new(120_000)
        .tap(lane(5), TimeUs(2_000 * MS))
        .tap(lane(7), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let mut r = warp(&chart);
    let v = view(1, 2);
    let now = TimeUs(1_800 * MS);
    let judged = JudgeEvent {
        seq: 0,
        time: TimeUs(1_790 * MS),
        origin: Some(TimeUs(1_789 * MS)),
        kind: JudgeEventKind::Judged {
            lane: lane(7),
            note: 0,
            judgment: Judgment::Perfect,
            deviation: Some(TimeUs(-210 * MS)),
            release: None,
            combo: 1,
        },
    };
    assert!(
        r.frame(&input(&v, now, TimeUs(0), &[judged]))
            .unwrap()
            .presented
    );
    let shown = |r: &mut Renderer, (w, h): (u32, u32), (l5, l7, row): (u32, u32, f64)| {
        r.resize(w, h);
        assert!(!r.is_minimized());
        assert_eq!(r.target(), [w, h]);
        let timing = r.frame(&input(&v, now, TimeUs(0), &[])).unwrap();
        assert!(timing.presented);
        let p = r.read_back();
        let got = note_center_in(&p, r.size(), l5);
        assert!(
            (got - row).abs() <= 1.0,
            "{w}×{h}: row {got}, expected {row}"
        );
        assert_eq!(
            white_rows_in(&p, r.size(), l7),
            Vec::<u32>::new(),
            "{w}×{h}"
        );
    };
    shown(&mut r, (1_280, 1_024), (362, 448, 560.89));
    shown(&mut r, (1_920, 1_080), (544, 672, 613.33));
    shown(&mut r, (2_560, 1_080), (864, 992, 613.33));
    shown(&mut r, (1_920, 1_200), (544, 672, 673.33));
    r.resize(0, 0);
    assert!(r.is_minimized());
    assert_eq!(r.target(), [1_920, 1_200]);
    let timing = r.frame(&input(&v, now, TimeUs(0), &[])).unwrap();
    assert!(!timing.presented);
    shown(&mut r, (1_920, 1_200), (544, 672, 673.33));
    shown(&mut r, (1_280, 1_024), (362, 448, 560.89));
}
