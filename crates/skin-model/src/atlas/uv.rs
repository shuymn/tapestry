//! The per-frame lookups: a frame of an image from its cycle, and the
//! frame's place in the atlas. They allocate nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

/// Where one frame lies in the atlas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameUv {
    /// Left, top, right, bottom, in the page's 0..1 coordinates.
    pub uv: [f32; 4],
    /// The texture array's layer, at least the reserved layers.
    pub layer: u16,
}

/// Every frame of a sheet's images in the atlas built for one variant, in
/// frame order: image by image, each image's frames row by row.
#[derive(Clone, Debug)]
pub struct UvTable {
    sheet: u64,
    frames: Box<[FrameUv]>,
}

impl UvTable {
    pub(crate) fn new(sheet: u64, frames: Box<[FrameUv]>) -> Self {
        Self { sheet, frames }
    }

    /// The frame at `index`, if there is one.
    #[must_use]
    pub fn get(&self, index: u32) -> Option<FrameUv> {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.frames.get(i))
            .copied()
    }

    /// The frames, in frame order.
    #[must_use]
    pub fn frames(&self) -> &[FrameUv] {
        &self.frames
    }

    /// The identity of the sheet this table was built from.
    #[must_use]
    pub fn sheet_id(&self) -> u64 {
        self.sheet
    }
}

/// An image's frame cycle: its period and the timer slot it runs on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cycle {
    pub period: i64,
    pub slot: usize,
}

impl Cycle {
    /// The frame of `n` shown `elapsed` µs after the cycle's timer started:
    /// `((elapsed mod period) · n) / period`, 0 with fewer than 2 frames.
    /// With `period` at most `MAX_KEY_US` (below 2^34) and `n` at most
    /// 1 024, the product stays below 2^44.
    pub(crate) fn frame(self, elapsed: i64, n: u16) -> u32 {
        if n < 2 || self.period < 1 {
            return 0;
        }
        let t = elapsed.max(0) % self.period;
        t.checked_mul(i64::from(n))
            .and_then(|p| u32::try_from(p / self.period).ok())
            .unwrap_or(0)
    }
}
