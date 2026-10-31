//! Skins with lanes for the layout and lanes tests: the simple skin's
//! geometry, one image per lane part (told apart by uv), and helpers that
//! read the parts back out of a batch.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bms_model::{Lane, TimeUs};
use image::{ImageFormat, Rgba, RgbaImage};
use skin_model::{
    Anchor, Bind, Blend, Ease, ElementDef, End, FileDef, Filter, Fit, ImageDef, KeyDef, LaneParts,
    LanesDef, LoadError, Place, Quad, Region, RegionDef, Sheet, Side, Skin, SkinDef, Source,
    TimerRef, Track, timer,
};

use crate::play_support::{Harness, REGIONS};

/// The images, by index: a white texel for plain elements, then one per
/// lane part, each its own size so none shares a packed frame.
pub const WHITE: u32 = 0;
pub const NOTE: u32 = 1;
pub const HEAD: u32 = 2;
pub const TAIL: u32 = 3;
pub const BODY_ON: u32 = 4;
pub const BODY_OFF: u32 = 5;
pub const MINE: u32 = 6;
pub const BAR: u32 = 7;
pub const LINE: u32 = 8;
pub const COVER: u32 = 9;
pub const LIFT: u32 = 10;
const SIZES: [(u32, u32); 11] = [
    (1, 1),
    (4, 16),
    (5, 16),
    (6, 16),
    (7, 8),
    (8, 8),
    (9, 8),
    (10, 2),
    (11, 6),
    (12, 32),
    (13, 32),
];

/// A folder of the part images under the target's test folder, written
/// once, each atomically.
fn root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("skin-model-lanes");
        std::fs::create_dir_all(&dir).unwrap();
        for (i, &(w, h)) in SIZES.iter().enumerate() {
            let tmp = tempfile::Builder::new()
                .suffix(".png")
                .tempfile_in(&dir)
                .unwrap();
            RgbaImage::from_pixel(w, h, Rgba([255; 4]))
                .save_with_format(tmp.path(), ImageFormat::Png)
                .unwrap();
            tmp.persist(dir.join(format!("{i}.png"))).unwrap();
        }
        dir
    })
}

/// The sheet of [`SIZES`]: image `i` is file `i`, whole, one frame.
pub fn sheet() -> Sheet {
    let n = u32::try_from(SIZES.len()).unwrap();
    Sheet::new(
        root(),
        (0..n)
            .map(|i| FileDef {
                path: format!("{i}.png"),
                hi: None,
            })
            .collect(),
        (0..n)
            .map(|file| ImageDef {
                file,
                rect: None,
                grid: [1, 1],
                cycle: None,
                filter: Filter::Linear,
            })
            .collect(),
    )
    .unwrap()
}

/// The simple skin's lanes: the scratch at 160 (96 wide), the keys 64
/// wide from 256, top 80, line 880, bottom 960; notes 16, mines 8, bar
/// lines 2 and the line 6 tall.
pub fn simple() -> LanesDef {
    let mut lanes = [[160, 96]; 8];
    for (k, lane) in lanes.iter_mut().enumerate().skip(1) {
        *lane = [256 + 64 * (i32::try_from(k).unwrap() - 1), 64];
    }
    LanesDef {
        lanes,
        scratch: Side::Left,
        top: 80,
        line: 880,
        bottom: 960,
        note_h: 16,
        mine_h: 8,
        bar_h: 2,
        line_h: 6,
        parts: LaneParts {
            note: [NOTE; 8],
            head: [HEAD; 8],
            tail: [TAIL; 8],
            body_on: [BODY_ON; 8],
            body_off: [BODY_OFF; 8],
            mine: [MINE; 8],
            bar: Some(BAR),
            line: Some(LINE),
            cover: Some(COVER),
            lift: Some(LIFT),
        },
    }
}

/// A track holding one key from 0.
pub fn still(props: [Option<i32>; 9]) -> Track {
    Track::new(
        &[KeyDef {
            at: TimeUs(0),
            props,
        }],
        Ease::Linear,
        End::Hold,
    )
    .unwrap()
}

/// A rect track `[x, y, w, h]` holding from 0.
pub fn rect([x, y, w, h]: [i32; 4]) -> Track {
    let mut props = [None; 9];
    props[0] = Some(x);
    props[1] = Some(y);
    props[2] = Some(w);
    props[3] = Some(h);
    still(props)
}

/// An element on `ready` showing `image` over `r`, placed by `place`.
pub fn image(image: u32, r: [i32; 4], place: Place) -> ElementDef {
    ElementDef {
        source: Source::Image(image),
        track: rect(r),
        timer: TimerRef::Builtin(timer::READY),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place,
    }
}

/// `place` in `region`.
pub fn in_region(region: Region) -> Place {
    Place {
        region,
        ..Place::SCREEN
    }
}

/// `place` in the playfield, bound by `bind`.
pub fn bound(bind: Bind) -> Place {
    Place {
        region: Region::Playfield,
        bind,
        ..Place::SCREEN
    }
}

/// The lanes widget on `ready`, in the playfield.
pub fn lanes_element() -> ElementDef {
    ElementDef {
        source: Source::Lanes,
        track: still([None; 9]),
        timer: TimerRef::Builtin(timer::READY),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: in_region(Region::Playfield),
    }
}

/// An adapted skin over [`sheet`] with `regions`, `lanes` and `elements`.
pub fn skin_with(
    regions: [RegionDef; Region::COUNT],
    lanes: Option<LanesDef>,
    elements: Vec<ElementDef>,
) -> Result<Skin, LoadError> {
    Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions,
        whens: Vec::new(),
        sheet: sheet(),
        lanes,
        elements,
    })
}

/// A skin of just the simple lanes.
pub fn lanes_skin(def: LanesDef) -> Skin {
    skin_with(REGIONS, Some(def), vec![lanes_element()]).unwrap()
}

/// Every region anchored at `anchor`.
pub fn anchored(anchor: Anchor) -> [RegionDef; Region::COUNT] {
    [RegionDef {
        anchor,
        optional: false,
    }; Region::COUNT]
}

/// What tells a part's quads apart, whatever their crop: the uv's
/// horizontal extent and the layer.
fn part_key(h: &Harness, image: u32) -> ([u32; 2], u16) {
    let f = h.uvs.get(image).unwrap();
    ([f.uv[0].to_bits(), f.uv[2].to_bits()], f.layer)
}

/// The quads of `image`'s part, in draw order.
pub fn quads_of(h: &Harness, image: u32) -> Vec<Quad> {
    let key = part_key(h, image);
    h.batch
        .quads()
        .iter()
        .filter(|q| ([q.uv[0].to_bits(), q.uv[2].to_bits()], q.layer) == key)
        .copied()
        .collect()
}

/// The rects of `image`'s part.
pub fn rects(h: &Harness, image: u32) -> Vec<[f32; 4]> {
    quads_of(h, image).iter().map(|q| q.rect).collect()
}

/// The rects of `image`'s part in the column starting at `x`.
pub fn in_column(h: &Harness, image: u32, x: f32) -> Vec<[f32; 4]> {
    rects(h, image).into_iter().filter(|r| r[0] == x).collect()
}

/// The simple skin's column of `lane`.
pub fn lane_x(lane: Lane) -> f32 {
    match lane.index() {
        0 => 160.0,
        k => 256.0 + 64.0 * (k as f32 - 1.0),
    }
}

/// The rects of `image`'s part in `lane`'s simple column.
pub fn in_lane(h: &Harness, image: u32, lane: Lane) -> Vec<[f32; 4]> {
    in_column(h, image, lane_x(lane))
}
