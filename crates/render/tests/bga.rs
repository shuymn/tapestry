//! The BGA array on the GPU (design.md "Rendering and Skins", BGA): the
//! pictures written to the array draw where a play skin's `bga` element
//! is; the layer's black is transparent, so the base shows through it;
//! and each channel's absence draws as its own: a base with no picture
//! the black placeholder, a layer or poor picture nothing. A video id
//! draws its chart movie slot's frame, opaque on the layer too, and a lost
//! chart video draws every destination opaque black.

mod common;
mod decl_support;

use bms_model::{
    BgaEvent, BgaImage, BgaImageId, Chart, ChartBga, ChartBuilder, PoorSequence, TimeUs,
};
use common::{GAUGES, attempt, context, input, install, read_back, view};
use decl_support::{BGA_RECT, BGA_RECT_2, Look, bundle};
use skin_model::bga::{BgaCell, BgaEntry, CellPlace, ChartVideos};
use skin_model::movie::{MovieShow, MovieShows};

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

/// A time slice of none: one step at a boundary, so the tests see each
/// step (the slice's own test is a unit test on a test clock).
const ONE_STEP: std::time::Duration = std::time::Duration::ZERO;

/// Hands `cells` to `r` for the array `key` names and steps its frame
/// boundaries, a step at each, until the batch ends; the boundaries it
/// took.
fn upload(
    r: &mut render::Renderer,
    key: skin_model::bga::BgaKey,
    cells: Vec<(CellPlace, Vec<u8>)>,
) -> Result<u32, render::BgaFault> {
    r.stage_bga_cells(key, cells)?;
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    for steps in 1.. {
        match r.bga_boundary_within(ONE_STEP) {
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
    renderer_of(dir, chart, false)
}

/// As [`renderer`], with a second BGA destination when `second`
/// ([`Look::bga_second`]).
fn renderer_of(dir: &std::path::Path, chart: &Chart, second: bool) -> render::Renderer {
    let mut r = common::offscreen(1_920, 1_080);
    let look = Look {
        bga: true,
        bga_second: second,
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

/// A batch is written at frame boundaries, at most `UPLOAD_WRITE`
/// bytes at each (design.md "Memory Budget"): a 2 046² picture, whose
/// cell's 16 MiB are four budgets, ends no sooner than its fourth boundary
/// and draws whole, its last rows included; four cells of a budget each
/// are written at most one a boundary, each dropped once written.
#[test]
fn a_batch_is_written_a_budget_at_a_boundary() {
    assert_eq!(render::UPLOAD_WRITE, 4 << 20);
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
    assert_eq!(
        r.bga_boundary_within(ONE_STEP),
        render::BgaStep::Idle,
        "nothing in hand"
    );
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
    // a boundary writes one cell or, while the last step is in flight,
    // nothing, and each cell's bytes are dropped once written.
    let quarter = u64::try_from(1_024 * 1_024 * 4).unwrap();
    let cells = (0..4)
        .map(|i| {
            let place = CellPlace {
                page: 0,
                origin: [i % 2 * 1_024, i / 2 * 1_024],
                content: [1_022, 1_022],
            };
            (place, vec![255; 1_024 * 1_024 * 4])
        })
        .collect();
    r.stage_bga_cells(key, cells).unwrap();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut left = 4 * quarter;
    while r.bga_boundary_within(ONE_STEP) == render::BgaStep::Pending {
        let now = r.bga_upload_bytes();
        assert!(
            now == left || now + quarter == left,
            "{left} to {now}: one cell a boundary at most"
        );
        left = now;
        assert!(std::time::Instant::now() < end, "the batch never ended");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(left, 0, "every cell written");
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
        r.bga_boundary_within(ONE_STEP),
        render::BgaStep::Pending,
        "a budget written"
    );
    let again = r.begin_bga(1).unwrap();
    assert_eq!(
        r.bga_boundary_within(ONE_STEP),
        render::BgaStep::Done(Err(render::BgaFault::Stale))
    );
    assert_eq!(r.bga_boundary_within(ONE_STEP), render::BgaStep::Idle);
    assert_eq!(r.bga_upload_bytes(), 0);
    let off = CellPlace { page: 1, ..place };
    let (_, texels) = large_cell();
    assert_eq!(
        r.stage_bga_cells(again, vec![(off, texels)]),
        Err(render::BgaFault::Cell)
    );
    assert_eq!(r.bga_boundary_within(ONE_STEP), render::BgaStep::Idle);
}

/// Writes a 64×64 frame into movie slot `slot`'s planes, its left half of
/// Y'CbCr `left` and its right half of `right` (limited range).
fn movie(r: &mut render::Renderer, slot: u32, left: [u8; 3], right: [u8; 3]) -> MovieShow {
    r.video_reserve(slot, 64, 64);
    let whole = r.video_write_with(slot, 64, 64, |sink, [luma, chroma]| {
        let mut at = 0;
        for (plane, rows, width, pitch) in
            [(0, 64, 64, luma), (1, 32, 32, chroma), (2, 32, 32, chroma)]
        {
            let row: Vec<u8> = (0..width)
                .map(|x| {
                    if x < width / 2 {
                        left[plane]
                    } else {
                        right[plane]
                    }
                })
                .collect();
            for _ in 0..rows {
                if !sink(at, &row) {
                    return false;
                }
                at += pitch;
            }
        }
        true
    });
    assert!(whole);
    MovieShow::Frame {
        uv: render::VIDEO_UV,
        layer: u16::try_from(slot).unwrap(),
        size: [64, 64],
        bt601: false,
        full: false,
    }
}

/// Black and white, limited range.
const Y_BLACK: [u8; 3] = [16, 128, 128];
const Y_WHITE: [u8; 3] = [235, 128, 128];

/// Asserts `got` is within 2 levels of `want` in each channel.
#[track_caller]
fn near(got: [u8; 3], want: [u8; 3], what: &str) {
    let off = got
        .iter()
        .zip(want)
        .map(|(g, w)| g.abs_diff(w))
        .max()
        .unwrap_or(0);
    assert!(off <= 2, "{what}: {got:?} against {want:?}");
}

/// A layer that shows a video frame is opaque (design.md "Rendering and
/// Skins", BGA): the frame's black covers the base, where a layer
/// picture's black in the same place lets the base through.
// Source: SF-024
#[test]
fn a_layer_video_is_opaque_over_the_base() {
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
    let base = write(&mut r, key, 0, BLUE, BLUE);
    let keyed = write(&mut r, key, 1, BLACK, GREEN);
    r.publish_bga(Some(key), vec![base, BgaEntry::Video])
        .unwrap();
    r.set_chart_videos(ChartVideos::new(&[None, Some(2)]));
    let mut shows = MovieShows::default();
    shows[2] = movie(&mut r, 2, Y_BLACK, Y_WHITE);
    r.set_play_movies(shows);
    let p = frame(&mut r, 100, &[]);
    near(at(&p, LEFT), BLACK, "the video's black covers the base");
    near(at(&p, RIGHT), WHITE, "the video's white");
    r.publish_bga(Some(key), vec![base, keyed]).unwrap();
    let p = frame(&mut r, 200, &[]);
    assert_eq!(at(&p, LEFT), BLUE, "a layer picture's black is keyed");
    assert_eq!(at(&p, RIGHT), GREEN);
}

/// A chart whose names resolve to videos alone publishes its table with
/// no array (design.md "Rendering and Skins", BGA): its base video still
/// draws its slot's frame, which reads back, where a picture of a table
/// with no array is absent.
#[test]
fn a_video_only_table_with_no_array_draws_its_frame() {
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
    r.publish_bga(None, vec![BgaEntry::Video]).unwrap();
    r.set_chart_videos(ChartVideos::new(&[Some(2)]));
    let mut shows = MovieShows::default();
    shows[2] = movie(&mut r, 2, Y_WHITE, Y_BLACK);
    r.set_play_movies(shows);
    let p = frame(&mut r, 100, &[]);
    near(at(&p, LEFT), WHITE, "the video's white half");
    near(at(&p, RIGHT), BLACK, "the video's black half");
    // Nothing to show: the base's placeholder.
    r.set_play_movies(MovieShows::default());
    let p = frame(&mut r, 200, &[]);
    assert_eq!((at(&p, LEFT), at(&p, RIGHT)), (BLACK, BLACK));
}

/// A lost chart video draws every `bga` destination opaque black over its
/// whole rect (design.md "Rendering and Skins", BGA), whatever its colour
/// and blend: the first alpha-blended, the second additive, tinted and at
/// half alpha, each over white beside the other, with a base, a layer and
/// a poor picture of their own colours under the loss.
#[test]
fn a_lost_panel_is_black_over_every_destination() {
    let dir = tempfile::tempdir().unwrap();
    let c = chart(
        ChartBga::new(
            (1..=3).map(image).collect(),
            vec![event(0, Some(0))],
            vec![event(0, Some(1))],
            vec![PoorSequence {
                time: TimeUs(0),
                frames: vec![Some(BgaImageId(2))].into_boxed_slice(),
            }],
        )
        .unwrap(),
    );
    let mut r = renderer_of(dir.path(), &c, true);
    let key = r.begin_bga(1).unwrap();
    let base = write(&mut r, key, 0, BLUE, BLUE);
    let layer = write(&mut r, key, 1, GREEN, GREEN);
    let poor = write(&mut r, key, 2, BLUE, GREEN);
    r.publish_bga(Some(key), vec![base, layer, poor]).unwrap();
    let rects = [BGA_RECT, BGA_RECT_2].map(|q| q.map(|v| u32::try_from(v).unwrap()));
    let corners = |[x, y, w, h]: [u32; 4]| {
        [
            (x + 1, y + 1),
            (x + w - 2, y + 1),
            (x + 1, y + h - 2),
            (x + w - 2, y + h - 2),
            (x + w / 2, y + h / 2),
        ]
    };
    // Shown first: the panel's own colours over the alpha destination,
    // and the additive one's over white.
    let p = frame(&mut r, 100, &[]);
    assert_eq!(at(&p, LEFT), GREEN, "the layer before the loss");
    let mut shows = MovieShows::default();
    shows[3] = MovieShow::Lost;
    r.set_play_movies(shows);
    let miss = eval::JudgeEvent {
        seq: 1,
        time: TimeUs(300_000),
        origin: None,
        kind: eval::JudgeEventKind::EmptyPoor {
            lane: common::lane(1),
            deviation: TimeUs(0),
            presses: 0,
        },
    };
    for (ms, effects) in [(200, &[][..]), (300, &[miss][..])] {
        let p = frame(&mut r, ms, effects);
        for (i, rect) in rects.iter().enumerate() {
            for xy in corners(*rect) {
                assert_eq!(
                    common::rgb_at(&p, 1_920, xy),
                    BLACK,
                    "destination {i} at {xy:?}, {ms} ms"
                );
            }
        }
        // Between and beside them, the image.
        for xy in [(530, 250), (880, 250)] {
            assert_eq!(common::rgb_at(&p, 1_920, xy), WHITE, "{xy:?}, {ms} ms");
        }
    }
}
