//! Writing cells into a page with their edges extruded, and the page's
//! box-filtered mip levels.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use image::RgbaImage;

use super::GUTTER;

/// `c` premultiplied by alpha `a`, rounded to nearest.
fn premultiply(c: u8, a: u8) -> u8 {
    u8::try_from((u16::from(c) * u16::from(a) + 127) / 255).unwrap_or(u8::MAX)
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Writes the `src_rect` (left, top, width, height) of `src` premultiplied
/// into the allocation `alloc` of a page `side` texels wide, its content
/// `GUTTER` texels in. Every texel of the allocation outside the content
/// repeats the nearest content texel, so the whole allocation is filled.
pub(crate) fn blit(
    page: &mut [u8],
    side: u32,
    src: &RgbaImage,
    src_rect: [u32; 4],
    alloc: [u32; 4],
) {
    let [sx, sy, w, h] = src_rect;
    let [ax, ay, aw, ah] = alloc;
    if w == 0 || h == 0 {
        return;
    }
    let src_w = to_usize(src.width());
    let raw = src.as_raw();
    for dy in 0..ah {
        let ry = sy + dy.saturating_sub(GUTTER).min(h - 1);
        let start = (to_usize(ay + dy) * to_usize(side) + to_usize(ax)) * 4;
        let Some(row) = page.get_mut(start..start + to_usize(aw) * 4) else {
            continue;
        };
        for (dx, texel) in (0..aw).zip(row.as_chunks_mut::<4>().0) {
            let rx = sx + dx.saturating_sub(GUTTER).min(w - 1);
            let at = (to_usize(ry) * src_w + to_usize(rx)) * 4;
            let Some(&[r, g, b, a]) = raw.get(at..at + 4) else {
                continue;
            };
            *texel = [premultiply(r, a), premultiply(g, a), premultiply(b, a), a];
        }
    }
}

/// Fills the allocation `alloc` of a page `side` texels wide, gutter
/// included, with straight colour `[r, g, b, a]` premultiplied: a
/// generated cell's every texel, with no source image.
pub(crate) fn blit_solid(page: &mut [u8], side: u32, [r, g, b, a]: [u8; 4], alloc: [u32; 4]) {
    let [ax, ay, aw, ah] = alloc;
    let texel = [premultiply(r, a), premultiply(g, a), premultiply(b, a), a];
    for dy in 0..ah {
        let start = (to_usize(ay + dy) * to_usize(side) + to_usize(ax)) * 4;
        let Some(row) = page.get_mut(start..start + to_usize(aw) * 4) else {
            continue;
        };
        for t in row.as_chunks_mut::<4>().0 {
            *t = texel;
        }
    }
}

/// The 2×2 average of four texels' channel, rounded to nearest.
fn mean(a: u8, b: u8, c: u8, d: u8) -> u8 {
    let sum = u16::from(a) + u16::from(b) + u16::from(c) + u16::from(d);
    u8::try_from((sum + 2) / 4).unwrap_or(u8::MAX)
}

/// Fills `dst`, half of `src`'s `side` per axis, with the 2×2 box average
/// of `src`, per channel.
pub(crate) fn downsample(src: &[u8], side: u32, dst: &mut [u8]) {
    let row = to_usize(side) * 4;
    let half_row = row / 2;
    if row == 0 {
        return;
    }
    for (pair, out) in src
        .chunks_exact(2 * row)
        .zip(dst.chunks_exact_mut(half_row))
    {
        let Some((top, bottom)) = pair.split_at_checked(row) else {
            continue;
        };
        for ((t, b), o) in top
            .as_chunks::<8>()
            .0
            .iter()
            .zip(bottom.as_chunks::<8>().0)
            .zip(out.as_chunks_mut::<4>().0)
        {
            let &[t0, t1, t2, t3, t4, t5, t6, t7] = t;
            let &[b0, b1, b2, b3, b4, b5, b6, b7] = b;
            *o = [
                mean(t0, t4, b0, b4),
                mean(t1, t5, b1, b5),
                mean(t2, t6, b2, b6),
                mean(t3, t7, b3, b7),
            ];
        }
    }
}
