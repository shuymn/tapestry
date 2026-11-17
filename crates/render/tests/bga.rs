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
    upload(r, key, vec![(place, texels)]).unwrap();
    BgaEntry::Picture(BgaCell {
        uv: place.uv(),
        native: [SIDE, SIDE],
    })
}

/// Hands `cells` to `r` for the array `key` names and steps its frame
/// boundaries until the batch ends; the boundaries it took.
fn upload(
    r: &mut render::Renderer,
    key: skin_model::bga::BgaKey,
    cells: Vec<(CellPlace, Vec<u8>)>,
) -> Result<u32, render::BgaFault> {
    r.stage_bga_cells(key, cells)?;
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    for steps in 1.. {
        match r.bga_boundary() {
            render::BgaStep::Done(done) => return done.map(|()| steps),
            render::BgaStep::Pending => {
                assert!(std::time::Instant::now() < end, "the batch never ended");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            render::BgaStep::Idle => panic!("no batch in hand"),
        }
    }
    unreachable!()
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

/// A 2 046² picture's cell, its top half blue and its bottom half green,
/// at page 0's top left.
fn large_cell() -> (CellPlace, Vec<u8>) {
    let place = CellPlace {
        page: 0,
        origin: [0, 0],
        content: [2_046, 2_046],
    };
    let [w, h] = place.padded();
    let texels = (0..h)
        .flat_map(|y| {
            let [cr, cg, cb] = if y < h / 2 { BLUE } else { GREEN };
            (0..w).flat_map(move |_| [cr, cg, cb, 255])
        })
        .collect();
    (place, texels)
}

/// A batch is written at frame boundaries, at most `BGA_WRITE_BUDGET`
/// bytes at each (design.md "Memory Budget"): a 2 046² picture, whose
/// cell's 16 MiB are four budgets, ends no sooner than its fourth boundary
/// and draws whole, its last rows included; four cells of a budget each
/// are written one a boundary, each dropped once written.
#[test]
fn a_batch_is_written_a_budget_at_a_boundary() {
    assert_eq!(render::BGA_WRITE_BUDGET, 4 << 20);
    let dir = tempfile::tempdir().unwrap();
    let c = chart(
        ChartBga::new(
            vec![image(1)],
            vec![event(0, Some(0))],
            Vec::new(),
            Vec::new(),
        )
        .unwrap(),
    );
    let mut r = renderer(dir.path(), &c);
    let key = r.begin_bga(1).unwrap();
    let (place, texels) = large_cell();
    assert_eq!(r.bga_boundary(), render::BgaStep::Idle, "nothing in hand");
    let steps = upload(&mut r, key, vec![(place, texels)]).unwrap();
    assert!(steps >= 4, "{steps} boundaries");
    assert_eq!(r.bga_upload_bytes(), 0);
    r.publish_bga(
        Some(key),
        vec![BgaEntry::Picture(BgaCell {
            uv: place.uv(),
            native: [2_046, 2_046],
        })],
    )
    .unwrap();
    let p = frame(&mut r, 100, &[]);
    assert_eq!(at(&p, (200, 10)), BLUE);
    assert_eq!(at(&p, (200, 390)), GREEN, "the last rows written");
    // Four 1 022² pictures, each cell a whole budget (1 024 rows of 4 KiB):
    // one cell written at each boundary, its bytes dropped once written.
    let quarter = 1_024 * 1_024 * 4;
    let cells = (0..4)
        .map(|i| {
            let place = CellPlace {
                page: 0,
                origin: [i % 2 * 1_024, i / 2 * 1_024],
                content: [1_022, 1_022],
            };
            (place, vec![255; quarter])
        })
        .collect();
    r.stage_bga_cells(key, cells).unwrap();
    for left in (1..4).rev() {
        assert_eq!(r.bga_boundary(), render::BgaStep::Pending);
        assert_eq!(
            r.bga_upload_bytes(),
            left * u64::try_from(quarter).unwrap(),
            "one cell a boundary"
        );
    }
    r.cancel_bga_upload();
}

/// A batch whose array is replaced while it is written ends stale at the
/// next boundary, dropped; a cell off its page is refused before anything
/// is taken in hand.
#[test]
fn a_batch_whose_array_is_replaced_ends_stale() {
    let dir = tempfile::tempdir().unwrap();
    let c = chart(
        ChartBga::new(
            vec![image(1)],
            vec![event(0, Some(0))],
            Vec::new(),
            Vec::new(),
        )
        .unwrap(),
    );
    let mut r = renderer(dir.path(), &c);
    let key = r.begin_bga(1).unwrap();
    let (place, texels) = large_cell();
    let bytes = u64::try_from(texels.len()).unwrap();
    r.stage_bga_cells(key, vec![(place, texels)]).unwrap();
    assert_eq!(r.bga_upload_bytes(), bytes);
    assert_eq!(
        r.bga_boundary(),
        render::BgaStep::Pending,
        "a budget written"
    );
    let again = r.begin_bga(1).unwrap();
    assert_eq!(
        r.bga_boundary(),
        render::BgaStep::Done(Err(render::BgaFault::Stale))
    );
    assert_eq!(r.bga_boundary(), render::BgaStep::Idle);
    assert_eq!(r.bga_upload_bytes(), 0);
    let off = CellPlace { page: 1, ..place };
    let (_, texels) = large_cell();
    assert_eq!(
        r.stage_bga_cells(again, vec![(off, texels)]),
        Err(render::BgaFault::Cell)
    );
    assert_eq!(r.bga_boundary(), render::BgaStep::Idle);
}
