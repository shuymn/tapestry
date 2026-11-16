//! The BGA array on the GPU (design.md "Rendering and Skins", BGA): the
//! pictures written to the array draw where a play skin's `bga` element
//! is; the layer's black is transparent, so the base shows through it;
//! and each channel's absence draws as its own: a base with no picture
//! the black placeholder, a layer or poor picture nothing.

mod common;
mod decl_support;

use bms_model::{
    BgaEvent, BgaImage, BgaImageId, Chart, ChartBga, ChartBuilder, PoorSequence, TimeUs,
};
use common::{GAUGES, attempt, context, input, install, read_back, view};
use decl_support::{BGA_RECT, Look, bundle};
use skin_model::bga::{BgaCell, BgaEntry, CellPlace};

const WHITE: [u8; 3] = [255; 3];
const BLACK: [u8; 3] = [0; 3];
const GREEN: [u8; 3] = [0, 255, 0];
const BLUE: [u8; 3] = [0, 0, 255];

/// A picture's side in the array.
const SIDE: u32 = 64;

/// The pixel at `(dx, dy)` of [`BGA_RECT`]'s size, from its top left.
fn at(pixels: &[u8], (dx, dy): (u32, u32)) -> [u8; 3] {
    let [x, y, ..] = BGA_RECT.map(|v| u32::try_from(v).unwrap());
    common::rgb_at(pixels, 1_920, (x + dx, y + dy))
}

/// The left and right halves' middles.
const LEFT: (u32, u32) = (100, 200);
const RIGHT: (u32, u32) = (300, 200);

/// Picture `i`'s cell, `SIDE` square, at `SIDE + 2` texels' stride along
/// page 0's top: its left half `left`, its right half `right` (opaque).
fn write(
    r: &mut render::Renderer,
    key: skin_model::bga::BgaKey,
    i: u32,
    left: [u8; 3],
    right: [u8; 3],
) -> BgaEntry {
    let place = CellPlace {
        page: 0,
        origin: [i * (SIDE + 2), 0],
        content: [SIDE, SIDE],
    };
    let [w, h] = place.padded();
    let texels: Vec<u8> = (0..h)
        .flat_map(|_| {
            (0..w).flat_map(|x| {
                let [r, g, b] = if x <= SIDE / 2 { left } else { right };
                [r, g, b, 255]
            })
        })
        .collect();
    r.write_bga_cells(key, &[(place, texels)]).unwrap();
    BgaEntry::Picture(BgaCell {
        uv: place.uv(),
        native: [SIDE, SIDE],
    })
}

fn image(id: u16) -> BgaImage {
    BgaImage {
        id,
        name: format!("{id}.png"),
    }
}

fn event(ms: i64, image: Option<u16>) -> BgaEvent {
    BgaEvent {
        time: TimeUs(ms * 1_000),
        image: image.map(BgaImageId),
    }
}

fn chart(bga: ChartBga) -> Chart {
    let mut c = ChartBuilder::new(120_000).build().unwrap();
    c.bga = Some(bga);
    c
}

/// A renderer drawing `chart` under a white image with the BGA over it.
fn renderer(dir: &std::path::Path, chart: &Chart) -> render::Renderer {
    let mut r = common::offscreen(1_920, 1_080);
    let look = Look {
        bga: true,
        ..Look::left([255, 255, 255, 255])
    };
    let (b, _) = bundle(&r, dir, &look);
    install(&mut r, &b, 1);
    let ctx = context(&GAUGES, false);
    let a = attempt(&r, chart, &ctx);
    let token = r.stage(&b, &a, 2).unwrap();
    r.commit(token).unwrap();
    r
}

/// The frame at chart time `ms`, judging `effects` in it.
fn frame(r: &mut render::Renderer, ms: i64, effects: &[eval::JudgeEvent]) -> Vec<u8> {
    let v = view(0, 0);
    r.frame(&input(&v, TimeUs(ms * 1_000), TimeUs(0), effects))
        .unwrap();
    read_back(r)
}

#[test]
fn black_layer_lets_the_base_through() {
    let dir = tempfile::tempdir().unwrap();
    let c = chart(
        ChartBga::new(
            vec![image(1), image(2)],
            vec![event(0, Some(0))],
            vec![event(0, Some(1))],
            Vec::new(),
        )
        .unwrap(),
    );
    let mut r = renderer(dir.path(), &c);
    let key = r.begin_bga(1).unwrap();
    // The base all blue; the layer black on its left, green on its right.
    let base = write(&mut r, key, 0, BLUE, BLUE);
    let layer = write(&mut r, key, 1, BLACK, GREEN);
    r.publish_bga(Some(key), vec![base, layer]).unwrap();
    let p = frame(&mut r, 100, &[]);
    assert_eq!(at(&p, LEFT), BLUE, "the layer's black is transparent");
    assert_eq!(at(&p, RIGHT), GREEN);
    // Outside the BGA, the image under it.
    assert_eq!(common::rgb_at(&p, 1_920, (50, 50)), WHITE);
}

#[test]
fn absent_pictures_draw_per_channel() {
    let dir = tempfile::tempdir().unwrap();
    // Base 0 then base 1 from 1 000 ms; layer 2 throughout; a poor
    // sequence of picture 3 at 0 ms.
    let c = chart(
        ChartBga::new(
            (1..=4).map(image).collect(),
            vec![event(0, Some(0)), event(1_000, Some(1))],
            vec![event(0, Some(2))],
            vec![PoorSequence {
                time: TimeUs(0),
                frames: vec![Some(BgaImageId(3))].into_boxed_slice(),
            }],
        )
        .unwrap(),
    );
    let mut r = renderer(dir.path(), &c);
    let key = r.begin_bga(1).unwrap();
    let base = write(&mut r, key, 0, BLUE, BLUE);
    r.publish_bga(
        Some(key),
        vec![base, BgaEntry::Absent, BgaEntry::Absent, BgaEntry::Absent],
    )
    .unwrap();
    // An absent layer draws nothing over a present base.
    let p = frame(&mut r, 500, &[]);
    assert_eq!((at(&p, LEFT), at(&p, RIGHT)), (BLUE, BLUE));
    // An absent base draws the black placeholder.
    let p = frame(&mut r, 1_500, &[]);
    assert_eq!((at(&p, LEFT), at(&p, RIGHT)), (BLACK, BLACK));
    // An absent poor picture draws nothing in the miss window: the image
    // under the BGA shows.
    let poor = eval::JudgeEvent {
        seq: 1,
        time: TimeUs(1_600_000),
        origin: None,
        kind: eval::JudgeEventKind::EmptyPoor {
            lane: common::lane(1),
            deviation: TimeUs(0),
            presses: 0,
        },
    };
    let p = frame(&mut r, 1_600, &[poor]);
    assert_eq!((at(&p, LEFT), at(&p, RIGHT)), (WHITE, WHITE));
    // Published with no array, every picture is absent: the placeholder.
    r.publish_bga(None, vec![BgaEntry::Absent; 4]).unwrap();
    let p = frame(&mut r, 500, &[]);
    assert_eq!((at(&p, LEFT), at(&p, RIGHT)), (BLACK, BLACK));
}
