//! Sprites and the per-frame batch a skin fills.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bytemuck::{Pod, Zeroable};

/// One instanced quad, in layout pixels (1920×1080, origin at the top
/// left).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct Sprite {
    /// Left, top, width, height.
    pub rect: [f32; 4],
    /// Atlas coordinates: left, top, right, bottom.
    pub uv: [f32; 4],
    /// Color multiplied with the atlas sample, RGBA.
    pub color: [f32; 4],
}

/// How a sprite is blended over what is under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blend {
    /// Ordinary alpha blending. Drawn first, in push order.
    Alpha,
    /// Added on top (beams, bombs). Drawn after every alpha sprite.
    Additive,
}

/// The sprites of one frame, at most `capacity` of them across both blends.
///
/// Both lists are allocated once, at full capacity, so pushing never
/// allocates. A push beyond the capacity is dropped and counted.
#[derive(Debug)]
pub struct SpriteBatch {
    alpha: Vec<Sprite>,
    additive: Vec<Sprite>,
    capacity: usize,
    dropped: u32,
}

impl SpriteBatch {
    /// An empty batch holding up to `capacity` sprites.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            alpha: Vec::with_capacity(capacity),
            additive: Vec::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    /// Empties the batch and its drop count for the next frame.
    pub fn clear(&mut self) {
        self.alpha.clear();
        self.additive.clear();
        self.dropped = 0;
    }

    /// Adds `sprite`, or counts it as dropped and returns `false` when the
    /// batch is full.
    pub fn push(&mut self, blend: Blend, sprite: Sprite) -> bool {
        if self.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        match blend {
            Blend::Alpha => self.alpha.push(sprite),
            Blend::Additive => self.additive.push(sprite),
        }
        true
    }

    /// Whether another push would be dropped.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.len() >= self.capacity
    }

    /// Sprites held, across both blends.
    #[must_use]
    pub fn len(&self) -> usize {
        self.alpha.len() + self.additive.len()
    }

    /// Whether the batch holds no sprite.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most sprites the batch holds.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Copies the batch into `out` in drawing order, every alpha sprite
    /// before every additive one, after clearing it. This is the per-frame
    /// instance staging for the GPU buffer; when `out` has room for
    /// [`capacity`](Self::capacity) sprites it never allocates.
    pub fn stage(&self, out: &mut Vec<Sprite>) {
        out.clear();
        out.extend_from_slice(&self.alpha);
        out.extend_from_slice(&self.additive);
    }

    /// Pushes refused since the last [`clear`](Self::clear). An overflow
    /// indicator: a skin may stop at its first refused push, so this is a
    /// lower bound on what the frame left out, never a count of it.
    #[must_use]
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }

    /// The alpha-blended sprites, in push order.
    #[must_use]
    pub fn alpha(&self) -> &[Sprite] {
        &self.alpha
    }

    /// The additive sprites, in push order.
    #[must_use]
    pub fn additive(&self) -> &[Sprite] {
        &self.additive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sprite(x: f32) -> Sprite {
        Sprite {
            rect: [x, 2.0, 3.0, 4.0],
            ..Sprite::default()
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
        assert!(!batch.push(Blend::Alpha, sprite(11.0)));
        assert_eq!(batch.len(), 3);
        assert_eq!(batch.dropped(), 2);
        assert_eq!(batch.alpha(), [sprite(1.0), sprite(7.0)]);
        assert_eq!(batch.additive(), [sprite(5.0)]);
        let mut staged = Vec::with_capacity(3);
        batch.stage(&mut staged);
        assert_eq!(staged, [sprite(1.0), sprite(7.0), sprite(5.0)]);
        batch.clear();
        assert!(batch.is_empty());
        assert_eq!(batch.dropped(), 0);
        assert!(batch.push(Blend::Additive, sprite(13.0)));
    }
}
