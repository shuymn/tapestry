//! Sprites, the per-frame batch a skin fills, and the GPU instances the
//! batch is staged into.
//!
//! The batch keeps one ordered list, so a sprite pushed after another is
//! drawn over it whatever their blends, and splits it into runs wherever the
//! blend or the clip changes; the renderer draws the runs in order, each
//! under its clip's scissor.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bytemuck::{Pod, Zeroable};
use skin_model::{ClipId, Filter};

/// One quad, in canvas pixels (the 1920×1080 layout of the built-in
/// screens, origin at the top left).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sprite {
    /// Left, top, width, height.
    pub rect: [f32; 4],
    /// Atlas coordinates: left, top, right, bottom. On layer 0 they are
    /// the built-in atlas's ([`glyph_rows`](crate::glyph_rows),
    /// [`solid_uv`](crate::solid_uv)), whatever the array's page side:
    /// staging rescales them ([`Instance::of`]).
    pub uv: [f32; 4],
    /// Color multiplied with the atlas sample, straight RGBA: staging
    /// premultiplies it ([`Instance::of`]).
    pub color: [f32; 4],
    /// The point in canvas pixels the rect rotates about.
    pub pivot: [f32; 2],
    /// The clockwise rotation about `pivot`, in radians; applied to the rect
    /// as placed. 0 draws the rect as it is.
    pub angle: f32,
    /// The atlas texture array's layer: 0 is the built-in atlas (the
    /// bitmap font and the solid block), a skin's pages follow it.
    pub layer: u16,
    /// How a skin page is sampled. Layer 0 is always sampled at its
    /// nearest level-0 texel, so the bitmap font stays sharp.
    pub filter: Filter,
}

/// How a sprite is blended over what is under it. Either way the colour
/// is premultiplied, and the target's alpha composites over (source 1,
/// destination 1 − source alpha).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blend {
    /// Ordinary alpha blending: source 1, destination 1 − source alpha.
    Alpha,
    /// Added on top (beams, bombs): source 1, destination 1.
    Additive,
}

/// The sprites `start..end` of a batch, all blended with `blend` under
/// `clip`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// The blend of every sprite in the run.
    pub blend: Blend,
    /// The clip of every sprite in the run, an index into the clip table
    /// installed with the atlas; `None` draws on the whole target.
    pub clip: Option<ClipId>,
    /// The run's first sprite.
    pub start: u32,
    /// One past the run's last sprite.
    pub end: u32,
}

/// The atlas layer and filter bits of [`Instance::flags`]: the layer in
/// the low 16 bits, and this bit set when the atlas is sampled at its
/// nearest texel.
pub const FLAG_NEAREST: u32 = 1 << 16;

/// One sprite as the GPU reads it: 64 bytes, its fields converted one by
/// one from a [`Sprite`] ([`Instance::of`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct Instance {
    /// Left, top, width, height, in canvas pixels.
    pub rect: [f32; 4],
    /// Atlas coordinates: left, top, right, bottom, in the array's page.
    pub uv: [f32; 4],
    /// The sprite's color, premultiplied: RGB times alpha, then alpha.
    pub color: [f32; 4],
    /// The point the rect rotates about, in canvas pixels.
    pub pivot: [f32; 2],
    /// The clockwise rotation about `pivot`, in radians.
    pub angle: f32,
    /// The atlas layer (low 16 bits) and [`FLAG_NEAREST`].
    pub flags: u32,
}

const _: () = assert!(size_of::<Instance>() == 64);

impl Instance {
    /// `s` as the GPU draws it: its tint's RGB multiplied by its alpha,
    /// once; its layer and filter packed into the flags; and on layer 0,
    /// the built-in atlas, its uv multiplied by `font_scale`, the built-in
    /// atlas's side over the array's page side (1 when the array is the
    /// built-in atlas alone). The built-in atlas sits at the top left of
    /// layer 0, so its texels keep their coordinates; the sides are powers
    /// of two, so the product is exact.
    #[must_use]
    pub fn of(s: &Sprite, font_scale: f32) -> Self {
        let [r, g, b, a] = s.color;
        let uv = if s.layer == 0 {
            s.uv.map(|v| v * font_scale)
        } else {
            s.uv
        };
        let nearest = match s.filter {
            Filter::Nearest => FLAG_NEAREST,
            Filter::Linear => 0,
        };
        Self {
            rect: s.rect,
            uv,
            color: [r * a, g * a, b * a, a],
            pivot: s.pivot,
            angle: s.angle,
            flags: u32::from(s.layer) | nearest,
        }
    }
}

/// The sprites of one frame, in drawing order, at most `capacity` of them.
///
/// The sprite and run lists are allocated once, at full capacity, so
/// pushing never allocates: a run starts at a sprite, so there are never
/// more runs than sprites. A push beyond the capacity is dropped and
/// counted.
#[derive(Debug)]
pub struct SpriteBatch {
    sprites: Vec<Sprite>,
    runs: Vec<Run>,
    capacity: u32,
    dropped: u32,
}

impl SpriteBatch {
    /// An empty batch holding up to `capacity` sprites (at most
    /// `u32::MAX`).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = u32::try_from(capacity).unwrap_or(u32::MAX);
        let n = usize::try_from(capacity).unwrap_or(usize::MAX);
        Self {
            sprites: Vec::with_capacity(n),
            runs: Vec::with_capacity(n),
            capacity,
            dropped: 0,
        }
    }

    /// Empties the batch and its drop count for the next frame.
    pub fn clear(&mut self) {
        self.sprites.clear();
        self.runs.clear();
        self.dropped = 0;
    }

    /// The number of sprites held: the end of the last run.
    fn held(&self) -> u32 {
        self.runs.last().map_or(0, |r| r.end)
    }

    /// Adds `sprite` unclipped: [`push_clipped`](Self::push_clipped) with
    /// no clip.
    pub fn push(&mut self, blend: Blend, sprite: Sprite) -> bool {
        self.push_clipped(blend, None, sprite)
    }

    /// Adds `sprite` after every sprite already held, extending the last
    /// run when its blend is `blend` and its clip `clip`; or counts it as
    /// dropped and returns `false` when the batch is full.
    pub fn push_clipped(&mut self, blend: Blend, clip: Option<ClipId>, sprite: Sprite) -> bool {
        let held = self.held();
        if held >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        match self.runs.last_mut() {
            Some(run) if run.blend == blend && run.clip == clip => run.end = held + 1,
            _ => self.runs.push(Run {
                blend,
                clip,
                start: held,
                end: held + 1,
            }),
        }
        self.sprites.push(sprite);
        true
    }

    /// Whether another push would be dropped.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.held() >= self.capacity
    }

    /// Sprites held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sprites.len()
    }

    /// Whether the batch holds no sprite.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sprites.is_empty()
    }

    /// The most sprites the batch holds.
    #[must_use]
    pub fn capacity(&self) -> usize {
        usize::try_from(self.capacity).unwrap_or(usize::MAX)
    }

    /// Converts the batch into `out` in drawing order ([`Instance::of`]
    /// with `font_scale`), after clearing it: the per-frame instance
    /// staging for the GPU buffer. It never allocates: it stages at most
    /// `out`'s capacity, and the renderer's staging has room for the
    /// batch's [`capacity`](Self::capacity).
    pub fn stage(&self, out: &mut Vec<Instance>, font_scale: f32) {
        out.clear();
        let room = out.capacity();
        out.extend(
            self.sprites
                .iter()
                .take(room)
                .map(|s| Instance::of(s, font_scale)),
        );
    }

    /// Pushes refused since the last [`clear`](Self::clear). An overflow
    /// indicator: a skin may stop at its first refused push, so this is a
    /// lower bound on what the frame left out, never a count of it.
    #[must_use]
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }

    /// The sprites, in drawing order.
    #[must_use]
    pub fn sprites(&self) -> &[Sprite] {
        &self.sprites
    }

    /// The runs, in drawing order: they cover the sprites contiguously,
    /// and neighbouring runs differ in blend or clip.
    #[must_use]
    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// The sprites blended with `blend`, in drawing order.
    pub fn blended(&self, blend: Blend) -> impl Iterator<Item = &Sprite> {
        self.runs
            .iter()
            .filter(move |r| r.blend == blend)
            .flat_map(|r| {
                let range = usize::try_from(r.start).unwrap_or(usize::MAX)
                    ..usize::try_from(r.end).unwrap_or(usize::MAX);
                self.sprites.get(range).unwrap_or_default()
            })
    }
}

#[cfg(test)]
mod tests {
    use std::mem::offset_of;

    use super::*;

    fn sprite(x: f32) -> Sprite {
        Sprite {
            rect: [x, 2.0, 3.0, 4.0],
            ..Sprite::default()
        }
    }

    fn run(blend: Blend, clip: Option<u16>, start: u32, end: u32) -> Run {
        Run {
            blend,
            clip: clip.map(ClipId),
            start,
            end,
        }
    }

    #[test]
    fn push_beyond_capacity_is_dropped_and_counted() {
        let mut batch = SpriteBatch::with_capacity(3);
        assert!(batch.push(Blend::Alpha, sprite(1.0)));
        assert!(batch.push(Blend::Additive, sprite(5.0)));
        assert!(!batch.is_full());
        assert!(batch.push(Blend::Alpha, sprite(7.0)));
        assert!(batch.is_full());
        assert!(!batch.push(Blend::Additive, sprite(9.0)));
        assert!(!batch.push_clipped(Blend::Alpha, Some(ClipId(0)), sprite(11.0)));
        assert_eq!(batch.len(), 3);
        assert_eq!(batch.dropped(), 2);
        batch.clear();
        assert!(batch.is_empty());
        assert!(batch.runs().is_empty());
        assert_eq!(batch.dropped(), 0);
        assert!(batch.push(Blend::Additive, sprite(13.0)));
    }

    #[test]
    fn alpha_after_additive_keeps_its_place() {
        // Alpha, additive, then alpha again: three runs in push order, so
        // the last alpha sprite is drawn over the additive one.
        let mut batch = SpriteBatch::with_capacity(8);
        assert!(batch.push(Blend::Alpha, sprite(1.0)));
        assert!(batch.push(Blend::Alpha, sprite(2.0)));
        assert!(batch.push(Blend::Additive, sprite(3.0)));
        assert!(batch.push(Blend::Alpha, sprite(4.0)));
        assert_eq!(
            batch.runs(),
            [
                run(Blend::Alpha, None, 0, 2),
                run(Blend::Additive, None, 2, 3),
                run(Blend::Alpha, None, 3, 4),
            ]
        );
        let xs = |s: &[Instance]| s.iter().map(|i| i.rect[0]).collect::<Vec<_>>();
        let mut staged = Vec::with_capacity(8);
        batch.stage(&mut staged, 1.0);
        assert_eq!(xs(&staged), [1.0, 2.0, 3.0, 4.0]);
        let alpha: Vec<_> = batch.blended(Blend::Alpha).map(|s| s.rect[0]).collect();
        assert_eq!(alpha, [1.0, 2.0, 4.0]);
        let additive: Vec<_> = batch.blended(Blend::Additive).map(|s| s.rect[0]).collect();
        assert_eq!(additive, [3.0]);
    }

    /// A clip change breaks a run as a blend change does, and the same
    /// clip again extends it.
    #[test]
    fn clips_break_runs() {
        let mut batch = SpriteBatch::with_capacity(8);
        let a = Some(ClipId(0));
        let b = Some(ClipId(1));
        assert!(batch.push_clipped(Blend::Alpha, a, sprite(1.0)));
        assert!(batch.push_clipped(Blend::Alpha, a, sprite(2.0)));
        assert!(batch.push_clipped(Blend::Alpha, b, sprite(3.0)));
        assert!(batch.push(Blend::Alpha, sprite(4.0)));
        assert!(batch.push_clipped(Blend::Additive, None, sprite(5.0)));
        assert!(batch.push_clipped(Blend::Additive, b, sprite(6.0)));
        assert_eq!(
            batch.runs(),
            [
                run(Blend::Alpha, Some(0), 0, 2),
                run(Blend::Alpha, Some(1), 2, 3),
                run(Blend::Alpha, None, 3, 4),
                run(Blend::Additive, None, 4, 5),
                run(Blend::Additive, Some(1), 5, 6),
            ]
        );
    }

    #[test]
    fn staging_stops_at_the_staging_capacity() {
        let mut batch = SpriteBatch::with_capacity(4);
        for x in [1.0, 2.0, 3.0] {
            assert!(batch.push(Blend::Alpha, sprite(x)));
        }
        let mut staged = Vec::with_capacity(2);
        let room = staged.capacity();
        batch.stage(&mut staged, 1.0);
        assert_eq!(staged.len(), room);
        assert_eq!(staged.capacity(), room);
    }

    #[test]
    fn staging_premultiplies_the_tint_once() {
        let s = Sprite {
            rect: [1.0, 2.0, 3.0, 4.0],
            uv: [0.5, 0.25, 0.75, 1.0],
            color: [1.0, 0.5, 0.25, 0.5],
            pivot: [7.0, 8.0],
            angle: 1.5,
            layer: 3,
            filter: Filter::Linear,
        };
        let i = Instance::of(&s, 0.5);
        assert_eq!(i.rect, s.rect);
        // A skin page's uv is its own: not rescaled.
        assert_eq!(i.uv, s.uv);
        assert_eq!(i.color, [0.5, 0.25, 0.125, 0.5]);
        assert_eq!((i.pivot, i.angle), (s.pivot, s.angle));
        assert_eq!(i.flags, 3);
        // Opaque colours are unchanged.
        let opaque = Sprite {
            color: [0.2, 0.4, 0.6, 1.0],
            ..s
        };
        assert_eq!(Instance::of(&opaque, 0.5).color, opaque.color);
    }

    /// The layer and the filter are packed into the flags; layer 0's uv,
    /// the built-in atlas's, is scaled to the array's page side: 'A' at
    /// texel (8, 16) of the 128-texel atlas is at 8/512 of a 512 page.
    #[test]
    fn staging_packs_the_layer_and_rescales_the_built_in_atlas() {
        let glyph = Sprite {
            uv: [8.0 / 128.0, 16.0 / 128.0, 13.0 / 128.0, 23.0 / 128.0],
            ..Sprite::default()
        };
        let i = Instance::of(&glyph, 128.0 / 512.0);
        assert_eq!(
            i.uv,
            [8.0 / 512.0, 16.0 / 512.0, 13.0 / 512.0, 23.0 / 512.0]
        );
        assert_eq!(i.flags, 0);
        assert_eq!(Instance::of(&glyph, 1.0).uv, glyph.uv);
        let pixel_art = Sprite {
            layer: 0x1234,
            filter: Filter::Nearest,
            ..Sprite::default()
        };
        assert_eq!(Instance::of(&pixel_art, 1.0).flags, 0x1_1234);
        assert_eq!(FLAG_NEAREST, 0x1_0000);
    }

    /// The instance layout the vertex buffer describes.
    #[test]
    fn instance_fields_sit_where_the_shader_reads_them() {
        assert_eq!(size_of::<Instance>(), 64);
        assert_eq!(offset_of!(Instance, rect), 0);
        assert_eq!(offset_of!(Instance, uv), 16);
        assert_eq!(offset_of!(Instance, color), 32);
        assert_eq!(offset_of!(Instance, pivot), 48);
        assert_eq!(offset_of!(Instance, angle), 56);
        assert_eq!(offset_of!(Instance, flags), 60);
    }
}
