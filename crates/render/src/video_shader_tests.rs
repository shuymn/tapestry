//! Skin movies' planes in the sprite shader (design.md "Rendering and
//! Skins", skin movies; video frames): a frame written through the upload
//! buffer at the texture copy's pitch reaches the planes whole, its odd
//! last column and row included, and a quad with the video bit converts
//! its Y'CbCr by the movie's matrix and range to the R'G'B' an independent
//! conversion gives.

use super::pool_shader_tests::{FORMAT, H, W, atlas_of, draw, image};
use super::{Instance, pipelines};
use crate::video::{FLAG_VIDEO, FLAG_VIDEO_BT601, FLAG_VIDEO_FULL, Video};

/// An odd frame: the chroma planes round up, the luma rows are not a
/// multiple of the copy's 256-byte pitch.
const FW: u32 = 321;
const FH: u32 = 241;

/// A frame of `FW` × `FH` whose planes hold `y`, `u` and `v` everywhere
/// but in each plane's last column and row, which hold `edge`'s, so a
/// plane cut short shows.
fn planes(y: u8, u: u8, v: u8, edge: [u8; 3]) -> [Vec<Vec<u8>>; 3] {
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
    let (cw, ch) = (FW.div_ceil(2), FH.div_ceil(2));
    [
        plane(FW, FH, y, edge[0]),
        plane(cw, ch, u, edge[1]),
        plane(cw, ch, v, edge[2]),
    ]
}

/// Writes `frame` into layer 0 through the upload buffer, row by row at
/// the pitch it is given, and copies it into the planes.
fn upload(gpu: &crate::Gpu, video: &mut Video, frame: &[Vec<Vec<u8>>; 3]) {
    let whole = video.write_with(gpu.queue(), 0, FW, FH, |sink, [luma, chroma]| {
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
    assert!(whole, "the frame fits the planes");
    let mut encoder = gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    video.record(&mut encoder);
    gpu.queue().submit([encoder.finish()]);
}

/// R'G'B' of Y'CbCr `(y, u, v)` by BT.601 or BT.709, limited or full
/// range, rounded to 8 bits: the conversion the movie's colour names.
fn reference(y: u8, u: u8, v: u8, bt601: bool, full: bool) -> [u8; 3] {
    let (y, u, v) = (f64::from(y), f64::from(u), f64::from(v));
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

/// A 40-pixel quad at `x` drawing `uv` of the movie in layer 0, opaque
/// white tint, with the video bit and `colour`'s bits.
fn quad(x: f32, uv: [f32; 4], colour: u32) -> (usize, Instance) {
    (
        0,
        Instance {
            rect: [x, 4.0, 40.0, 40.0],
            uv,
            color: [1.0; 4],
            pivot: [0.0, 0.0],
            angle: 0.0,
            flags: FLAG_VIDEO | colour,
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

#[test]
fn a_movie_frame_converts_by_its_matrix_and_range_to_its_last_texel() {
    let r = crate::test_common::offscreen(64, 64);
    let gpu = r.gpu();
    let (alpha, additive, multiply, bindings) = pipelines(gpu, FORMAT);
    let half = [W as f32 / 2.0, H as f32 / 2.0, 0.0, 0.0];
    gpu.queue()
        .write_buffer(&bindings.projection, 0, bytemuck::cast_slice(&half));
    let atlas = atlas_of(gpu, 256, &[image(256, 0)]);
    let bind = bindings.bind(gpu.device(), &atlas, None);
    let bga = crate::bga::BgaArray::stand_in(gpu, &bindings.bga);
    let mut video = Video::new(gpu, &bindings.video);
    video.reserve(gpu, FW, FH, 2);
    let pipes = [&alpha, &additive, &multiply];

    let (y, u, v) = (180, 90, 200);
    let edge = [60, 170, 70];
    upload(gpu, &mut video, &planes(y, u, v, edge));
    let [_, _, ur, vb] = video.uv(FW, FH);
    // A quad across inner texels only; one sampling the last column's
    // centre (luma and chroma) alone; one the last row's.
    let inner = [0.1 * ur, 0.1 * vb, 0.5 * ur, 0.5 * vb];
    let x = (FW as f32 - 0.5) / FW as f32 * ur;
    let last_column = [x, 0.1 * vb, x, 0.5 * vb];
    let y_last = (FH as f32 - 0.5) / FH as f32 * vb;
    let last_row = [0.1 * ur, y_last, 0.5 * ur, y_last];
    let colours = [
        (0, false, false),
        (FLAG_VIDEO_BT601, true, false),
        (FLAG_VIDEO_FULL, false, true),
        (FLAG_VIDEO_BT601 | FLAG_VIDEO_FULL, true, true),
    ];
    let mut quads = Vec::new();
    for (i, &(bits, _, _)) in colours.iter().enumerate() {
        quads.push(quad(4.0 + 48.0 * i as f32, inner, bits));
    }
    quads.push(quad(4.0 + 48.0 * 4.0, last_column, 0));
    quads.push(quad(4.0 + 48.0 * 5.0, last_row, 0));
    let pixels = draw(gpu, pipes, &bind, &bga.bind, &video.bind, &quads);
    for (i, &(_, bt601, full)) in colours.iter().enumerate() {
        near(
            at(&pixels, 4 + 48 * i + 20, 24),
            reference(y, u, v, bt601, full),
            &format!("bt601 {bt601}, full {full}"),
        );
    }
    near(
        at(&pixels, 4 + 48 * 4 + 20, 24),
        reference(edge[0], edge[1], edge[2], false, false),
        "the last column",
    );
    near(
        at(&pixels, 4 + 48 * 5 + 20, 24),
        reference(edge[0], edge[1], edge[2], false, false),
        "the last row",
    );
}
