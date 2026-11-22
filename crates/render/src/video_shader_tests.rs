//! Skin movies' planes in the sprite shader (design.md "Rendering and
//! Skins", skin movies; video frames): a frame written through the upload
//! buffer at the texture copy's pitch reaches its movie's planes whole, its
//! odd last column and row included, and a quad with the video bit
//! converts its Y'CbCr by the movie's matrix and range to the R'G'B' an
//! independent conversion gives. Each movie's planes are its own: a peer
//! admitted later neither erases its frame nor shows at its edge, and the
//! GPU bytes stay within design.md's "Memory Budget" figure whatever the
//! two movies' shapes.

use super::pool_shader_tests::{FORMAT, H, W, atlas_of, draw, image};
use super::{Instance, pipelines};
use crate::video::{
    FLAG_VIDEO, FLAG_VIDEO_BT601, FLAG_VIDEO_FULL, MAX_MOVIES, VIDEO_UV, Video, movie_bytes,
};

/// An odd frame: the chroma planes round up, the luma rows are not a
/// multiple of the copy's 256-byte pitch.
const FW: u32 = 321;
const FH: u32 = 241;

/// A frame's planes, row by row: Y, U, V.
type Frame = [Vec<Vec<u8>>; 3];

/// A frame of `w` × `h` whose planes hold `y`, `u` and `v` everywhere but
/// in each plane's last column and row, which hold `edge`'s, so a plane
/// cut short shows.
fn planes(w: u32, h: u32, [y, u, v]: [u8; 3], edge: [u8; 3]) -> Frame {
    let plane = |w: u32, h: u32, inner: u8, last: u8| -> Vec<Vec<u8>> {
        (0..h)
            .map(|r| {
                (0..w)
                    .map(|c| {
                        if c + 1 == w || r + 1 == h {
                            last
                        } else {
                            inner
                        }
                    })
                    .collect()
            })
            .collect()
    };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    [
        plane(w, h, y, edge[0]),
        plane(cw, ch, u, edge[1]),
        plane(cw, ch, v, edge[2]),
    ]
}

/// A frame of `w` × `h` of one colour.
fn flat(w: u32, h: u32, yuv: [u8; 3]) -> Frame {
    planes(w, h, yuv, yuv)
}

/// Writes `frame` of `w` × `h` into slot `slot`'s upload buffer, row by
/// row at the pitch it is given; its copy is then in hand.
fn write(video: &mut Video, gpu: &crate::Gpu, slot: u32, w: u32, h: u32, frame: &Frame) {
    let whole = video.write_with(gpu, slot, w, h, |sink, [luma, chroma]| {
        let mut at = 0;
        for (p, plane) in frame.iter().enumerate() {
            let pitch = if p == 0 { luma } else { chroma };
            for row in plane {
                if !sink(at, row) {
                    return false;
                }
                at += pitch;
            }
        }
        true
    });
    assert!(whole, "the frame fits slot {slot}'s planes");
}

/// Records the copies in hand and submits them.
fn record(video: &mut Video, gpu: &crate::Gpu) {
    let mut encoder = gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    video.record(&mut encoder);
    gpu.queue().submit([encoder.finish()]);
}

/// R'G'B' of Y'CbCr `(y, u, v)` (8-bit levels, fractional where a
/// bilinear read mixes two) by BT.601 or BT.709, limited or full range,
/// rounded to 8 bits: the conversion the movie's colour names.
fn reference(y: f64, u: f64, v: f64, bt601: bool, full: bool) -> [u8; 3] {
    let (yn, cb, cr) = if full {
        (y / 255.0, (u - 128.0) / 255.0, (v - 128.0) / 255.0)
    } else {
        ((y - 16.0) / 219.0, (u - 128.0) / 224.0, (v - 128.0) / 224.0)
    };
    let (kr, kb) = if bt601 {
        (0.299, 0.114)
    } else {
        (0.2126, 0.0722)
    };
    let r = yn + 2.0 * (1.0 - kr) * cr;
    let b = yn + 2.0 * (1.0 - kb) * cb;
    let g = (yn - kr * r - kb * b) / (1.0 - kr - kb);
    [r, g, b].map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// A 40-pixel quad at `x` drawing `uv` of the movie in slot `slot`,
/// opaque white tint, with the video bit and `colour`'s bits.
fn quad(x: f32, uv: [f32; 4], slot: u32, colour: u32) -> (usize, Instance) {
    (
        0,
        Instance {
            rect: [x, 4.0, 40.0, 40.0],
            uv,
            color: [1.0; 4],
            pivot: [0.0, 0.0],
            angle: 0.0,
            flags: FLAG_VIDEO | colour | slot,
        },
    )
}

/// The RGB at target pixel `(x, y)`.
fn at(pixels: &[u8], x: usize, y: usize) -> [u8; 3] {
    let i = (y * W as usize + x) * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

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

/// The sprite pipelines over an offscreen device, its atlas, BGA and
/// video groups.
struct Rig {
    r: crate::Renderer,
    pipes: (
        wgpu::RenderPipeline,
        wgpu::RenderPipeline,
        wgpu::RenderPipeline,
    ),
    bind: wgpu::BindGroup,
    bga: crate::bga::BgaArray,
    video: Video,
}

impl Rig {
    fn new() -> Self {
        let r = crate::test_common::offscreen(64, 64);
        let gpu = r.gpu();
        let (alpha, additive, multiply, bindings) = pipelines(gpu, FORMAT);
        let half = [W as f32 / 2.0, H as f32 / 2.0, 0.0, 0.0];
        gpu.queue()
            .write_buffer(&bindings.projection, 0, bytemuck::cast_slice(&half));
        let atlas = atlas_of(gpu, 256, &[image(256, 0)]);
        let bind = bindings.bind(gpu.device(), &atlas, None);
        let bga = crate::bga::BgaArray::stand_in(gpu, &bindings.bga);
        let video = Video::new(gpu, &bindings.video);
        Self {
            r,
            pipes: (alpha, additive, multiply),
            bind,
            bga,
            video,
        }
    }

    fn reserve(&mut self, slot: u32, w: u32, h: u32) {
        let gpu = self.r.gpu();
        self.video.reserve(gpu, slot, w, h);
    }

    fn write(&mut self, slot: u32, w: u32, h: u32, frame: &Frame) {
        let gpu = self.r.gpu();
        write(&mut self.video, gpu, slot, w, h, frame);
    }

    /// Records the copies in hand, then draws `quads`: the pixels.
    fn draw(&mut self, quads: &[(usize, Instance)]) -> Vec<u8> {
        let gpu = self.r.gpu();
        record(&mut self.video, gpu);
        let (a, b, c) = &self.pipes;
        draw(
            gpu,
            [a, b, c],
            &self.bind,
            &self.bga.bind,
            &self.video.bind,
            quads,
        )
    }
}

#[test]
fn a_movie_frame_converts_by_its_matrix_and_range_to_its_last_texel() {
    let mut rig = Rig::new();
    rig.reserve(0, FW, FH);
    let (y, u, v) = (180, 90, 200);
    let edge = [60, 170, 70];
    rig.write(0, FW, FH, &planes(FW, FH, [y, u, v], edge));
    // A quad across inner texels only; one sampling the last column's
    // luma centre alone; one the last row's.
    let inner = [0.1, 0.1, 0.5, 0.5];
    let x = (FW as f32 - 0.5) / FW as f32;
    let last_column = [x, 0.1, x, 0.5];
    let y_last = (FH as f32 - 0.5) / FH as f32;
    let last_row = [0.1, y_last, 0.5, y_last];
    let colours = [
        (0, false, false),
        (FLAG_VIDEO_BT601, true, false),
        (FLAG_VIDEO_FULL, false, true),
        (FLAG_VIDEO_BT601 | FLAG_VIDEO_FULL, true, true),
    ];
    let mut quads = Vec::new();
    for (i, &(bits, _, _)) in colours.iter().enumerate() {
        quads.push(quad(4.0 + 48.0 * i as f32, inner, 0, bits));
    }
    quads.push(quad(4.0 + 48.0 * 4.0, last_column, 0, 0));
    quads.push(quad(4.0 + 48.0 * 5.0, last_row, 0, 0));
    let pixels = rig.draw(&quads);
    for (i, &(_, bt601, full)) in colours.iter().enumerate() {
        near(
            at(&pixels, 4 + 48 * i + 20, 24),
            reference(y.into(), u.into(), v.into(), bt601, full),
            &format!("bt601 {bt601}, full {full}"),
        );
    }
    // The last luma column's centre (320.5 of 321) is chroma 160.25 of
    // the 161 the odd side rounds up to: three quarters the last chroma
    // texel, a quarter the one before it.
    let mix = |inner: u8, last: u8| 0.25 * f64::from(inner) + 0.75 * f64::from(last);
    let edge_colour = reference(
        edge[0].into(),
        mix(u, edge[1]),
        mix(v, edge[2]),
        false,
        false,
    );
    near(
        at(&pixels, 4 + 48 * 4 + 20, 24),
        edge_colour,
        "the last column",
    );
    near(
        at(&pixels, 4 + 48 * 5 + 20, 24),
        edge_colour,
        "the last row",
    );
}

/// A movie keeps its last frame and its queued copy across a larger
/// peer's admission: a 640×360 movie's frame, its next one written and
/// in hand, then a 1280×720 peer reserved before the copy is recorded.
/// The next frame and every one after show the smaller movie's newest
/// frame.
#[test]
fn a_movie_keeps_its_frame_across_a_larger_peer_s_admission() {
    let mut rig = Rig::new();
    let first = [81, 90, 240];
    let newest = [145, 54, 34];
    rig.reserve(0, 640, 360);
    rig.write(0, 640, 360, &flat(640, 360, first));
    let shown = |pixels: &[u8]| at(pixels, 24, 24);
    let want = |c: [u8; 3]| reference(c[0].into(), c[1].into(), c[2].into(), false, false);
    let q = [quad(4.0, VIDEO_UV, 0, 0)];
    near(shown(&rig.draw(&q)), want(first), "before the peer");
    rig.write(0, 640, 360, &flat(640, 360, newest));
    rig.reserve(1, 1280, 720);
    near(shown(&rig.draw(&q)), want(newest), "the peer's admission");
    near(shown(&rig.draw(&q)), want(newest), "the frame after");
}

/// A neutral movie next to a larger peer shows no colour at its edges:
/// its last column and row sample its own planes, not a peer's size.
#[test]
fn a_smaller_movie_shows_no_chroma_fringe_beside_a_larger_peer() {
    let mut rig = Rig::new();
    let grey = [126, 128, 128];
    rig.reserve(0, 640, 360);
    rig.reserve(1, 1280, 720);
    rig.write(0, 640, 360, &flat(640, 360, grey));
    rig.write(1, 1280, 720, &flat(1280, 720, [200, 16, 240]));
    let x = 639.5 / 640.0;
    let y = 359.5 / 360.0;
    let pixels = rig.draw(&[
        quad(4.0, [x, 0.1, x, 0.5], 0, 0),
        quad(52.0, [0.1, y, 0.5, y], 0, 0),
        quad(100.0, [x, y, x, y], 0, 0),
    ]);
    let want = reference(grey[0].into(), 128.0, 128.0, false, false);
    near(at(&pixels, 24, 24), want, "the last column");
    near(at(&pixels, 72, 24), want, "the last row");
    near(at(&pixels, 120, 24), want, "the last texel");
}

/// design.md "Memory Budget": the skin movies' plane texels and upload
/// buffers total at most 20 MiB and 512 bytes a screen.
const BUDGET: u64 = 20 * 1024 * 1024 + 512;

/// The movies' GPU bytes past three bytes a pixel (I420's 1.5 in texels
/// and 1.5 in the upload buffer) are their sides' rounding and the copy's
/// row pitch: at most 256 bytes over every luma and chroma row of the
/// tallest movie, and 256 for odd sides. Checked over every width at the
/// tallest heights and every height at the narrowest and pitch-edge
/// widths; with two movies under the pixel total, the bound is the
/// budget.
#[test]
fn the_movie_planes_stay_within_the_memory_budget() {
    use skin_model::media::MAX_SIDE;
    use skin_model::movie::MAX_MOVIE_PIXELS;
    let align = u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let over = 2 * align * u64::from(MAX_SIDE) + align;
    let past = |w: u32, h: u32| {
        let (t, u) = movie_bytes(w, h);
        (t + u).saturating_sub(3 * u64::from(w) * u64::from(h))
    };
    for w in 1..=MAX_SIDE {
        for h in [MAX_SIDE - 1, MAX_SIDE] {
            assert!(past(w, h) <= over, "{w}×{h}");
        }
    }
    for w in [1, 2, 255, 256, 257, 511, 512, 513, MAX_SIDE - 1, MAX_SIDE] {
        for h in 1..=MAX_SIDE {
            assert!(past(w, h) <= over, "{w}×{h}");
        }
    }
    let movies = u64::from(MAX_MOVIES);
    assert!(3 * MAX_MOVIE_PIXELS + movies * over <= BUDGET);
    // The shapes past the old arrays' maximum of each axis, and a
    // narrow movie of the tallest side beside one filling the rest.
    for pair in [
        [(8_192, 256), (256, 8_192)],
        [(1, 8_192), (513, 8_160)],
        [(2_048, 2_048), (1, 1)],
    ] {
        let total: u64 = pair
            .iter()
            .map(|&(w, h)| {
                let (t, u) = movie_bytes(w, h);
                t + u
            })
            .sum();
        assert!(total <= BUDGET, "{pair:?}: {total}");
    }
}

/// The two shapes whose axes' maxima are 8 192 a side reserve the bytes
/// of their own sizes, not two 8 192 × 8 192 layers.
#[test]
fn a_wide_and_a_tall_movie_reserve_their_own_sizes() {
    let mut rig = Rig::new();
    rig.reserve(0, 8_192, 256);
    rig.reserve(1, 256, 8_192);
    let (texels, upload) = rig.video.bytes();
    assert_eq!(texels, 2 * 3 * 1024 * 1024, "the planes' texels");
    assert_eq!(upload, 3 * 1024 * 1024 + 4 * 1024 * 1024, "the uploads");
    assert!(texels + upload <= BUDGET);
    // Each is written whole at its own size.
    rig.write(0, 8_192, 256, &flat(8_192, 256, [100, 128, 128]));
    rig.write(1, 256, 8_192, &flat(256, 8_192, [100, 128, 128]));
    let pixels = rig.draw(&[quad(4.0, VIDEO_UV, 0, 0), quad(52.0, VIDEO_UV, 1, 0)]);
    let want = reference(100.0, 128.0, 128.0, false, false);
    near(at(&pixels, 24, 24), want, "the wide movie");
    near(at(&pixels, 72, 24), want, "the tall movie");
}
