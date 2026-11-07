//! The composition of a result graph's texture layers (design.md
//! "Rendering and Skins", result widgets): a source graph paints layers
//! into one texture and draws the finished texture once under its
//! destination's colour. A widget draws each region of such a texture as
//! one quad tinted with that region's layers composed here, by ordinary
//! straight-alpha over, so the renderer's premultiplied tint applies the
//! destination's alpha once per pixel.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

/// A straight (not premultiplied) RGBA colour, 8 bits a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba8(pub [u8; 4]);

impl Rgba8 {
    /// The colour `0xrrggbb` with alpha `a`.
    #[must_use]
    pub const fn rgb(rgb: u32, a: u8) -> Self {
        let [_, r, g, b] = rgb.to_be_bytes();
        Self([r, g, b, a])
    }

    /// The colour as straight RGBA in `0..=1`.
    #[must_use]
    pub fn unit(self) -> [f32; 4] {
        self.0.map(|c| f32::from(c) / 255.0)
    }
}

/// `src` over `dst`, both straight RGBA in `0..=1`, as straight RGBA:
/// `α = αs + αd(1 − αs)`, `C = (Cs·αs + Cd·αd(1 − αs)) / α`; transparent
/// black when both are transparent.
#[must_use]
pub(crate) fn over(src: [f32; 4], dst: [f32; 4]) -> [f32; 4] {
    let [sr, sg, sb, sa] = src;
    let [dr, dg, db, da] = dst;
    let under = da * (1.0 - sa);
    let a = sa + under;
    if a <= 0.0 {
        return [0.0; 4];
    }
    let c = |s: f32, d: f32| (s * sa + d * under) / a;
    [c(sr, dr), c(sg, dg), c(sb, db), a]
}

#[cfg(test)]
mod tests {
    use super::{Rgba8, over};

    /// `got` is `want` within 0.01 per channel, in 8-bit units.
    #[track_caller]
    fn near(got: [f32; 4], want: [f32; 4]) {
        for (g, w) in got.into_iter().zip(want) {
            assert!(
                (g * 255.0 - w).abs() < 0.01,
                "{got:?} × 255, expected {want:?}"
            );
        }
    }

    /// One translucent layer over another composes by straight-alpha
    /// over: `440000aa` over `004400aa` is (51, 17, 0, 226.7) in 8-bit
    /// units, where an integer blend that darkens the RGB gives
    /// `2d1700e2`; a transparent source leaves the destination; an opaque
    /// one replaces it; nothing over nothing stays transparent.
    #[test]
    fn layers_compose_by_straight_alpha_over() {
        let a = Rgba8::rgb(0x44_0000, 0xaa).unit();
        let b = Rgba8::rgb(0x00_4400, 0xaa).unit();
        near(over(a, b), [51.0, 17.0, 0.0, 226.667]);
        let clear = Rgba8::rgb(0x12_3456, 0).unit();
        near(over(clear, b), [0.0, 68.0, 0.0, 170.0]);
        let opaque = Rgba8::rgb(0x1f_1f1f, 0xff).unit();
        near(over(opaque, b), [31.0, 31.0, 31.0, 255.0]);
        near(over(clear, [0.0; 4]), [0.0; 4]);
        // A translucent colour over a cleared texel keeps its own RGB.
        near(
            over(Rgba8::rgb(0x00_00ff, 0x80).unit(), [0.0; 4]),
            [0.0, 0.0, 255.0, 128.0],
        );
    }
}
