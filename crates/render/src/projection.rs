//! Where a frame lands on the target: the canvas projection, and the clip
//! table with each clip's scissor.
//!
//! A skin draws in canvas pixels ([`Canvas`]); the canvas is placed on the
//! target once, as the viewport `origin + size × scale`, and the vertex
//! shader maps canvas pixels onto that viewport. The built-in screens use
//! the fixed 1920×1080 canvas, letterboxed ([`Projection::fixed`]).
//!
//! A clip is a canvas rectangle; its scissor is projected once per target
//! size, each edge rounded half up to a target pixel (D1) and clamped to
//! the target, and a clip that keeps no pixel draws nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use skin_model::{Canvas, ClipId, Fit};

/// How a canvas maps onto a target of `target` pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Projection {
    origin: [f32; 2],
    scale: f32,
    size: [f32; 2],
    target: [u32; 2],
}

impl Projection {
    /// `canvas` on its target ([`Canvas::target`], a zero side counting
    /// as 1).
    #[must_use]
    pub fn new(canvas: &Canvas) -> Self {
        Self {
            origin: canvas.origin(),
            scale: canvas.scale(),
            size: canvas.size(),
            target: canvas.target().map(|v| v.max(1)),
        }
    }

    /// The fixed 1920×1080 canvas letterboxed into a `target`-pixel
    /// target: the built-in screens' projection, and
    /// [`letterbox`](crate::letterbox)'s viewport.
    #[must_use]
    pub fn fixed(target: [u32; 2]) -> Self {
        Self::new(&Canvas::new(target, Fit::Fixed))
    }

    /// The target pixels the canvas covers: `[x, y, width, height]`.
    #[must_use]
    pub fn viewport(&self) -> [f32; 4] {
        let [x, y] = self.origin;
        let [w, h] = self.size;
        [x, y, w * self.scale, h * self.scale]
    }

    /// Half the canvas's size: the vertex shader maps canvas pixel `p` to
    /// clip space as `p / half − 1` across and `1 − p / half` down.
    #[must_use]
    pub fn half(&self) -> [f32; 2] {
        self.size.map(|v| v / 2.0)
    }

    /// The target's size in pixels.
    #[must_use]
    pub fn target(&self) -> [u32; 2] {
        self.target
    }

    /// The target's pixels `[x, y, width, height]` the canvas rectangle
    /// `rect` (`[left, top, width, height]`) covers, for a scissor: each
    /// edge placed as the viewport places it, `origin + edge × scale` in
    /// `f64`, rounded half up (D1) and clamped to the target. `None` when
    /// no pixel is left.
    #[must_use]
    pub fn scissor(&self, rect: [f32; 4]) -> Option<[u32; 4]> {
        let [l, t, w, h] = rect.map(f64::from);
        let [ox, oy] = self.origin.map(f64::from);
        let s = f64::from(self.scale);
        let [tw, th] = self.target;
        let edge = |o: f64, v: f64, max: u32| {
            // Rounded half up, then clamped: a value past either end, an
            // infinity included, lands on it; NaN lands on 0.
            let p = (o + v * s + 0.5).floor().clamp(0.0, f64::from(max));
            p as u32
        };
        let (x0, x1) = (edge(ox, l, tw), edge(ox, l + w, tw));
        let (y0, y1) = (edge(oy, t, th), edge(oy, t + h, th));
        (x0 < x1 && y0 < y1).then_some([x0, y0, x1 - x0, y1 - y0])
    }
}

/// The most clips a table holds: every [`ClipId`].
pub const MAX_CLIPS: usize = 1 << 16;

/// Why a clip table was refused, or a clip id is not in it.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ClipError {
    /// More clips than a [`ClipId`] can name.
    #[error("{count} clips, more than {MAX_CLIPS}")]
    TooMany {
        /// The clips given.
        count: usize,
    },
    /// A clip whose edges are not finite or whose size is negative.
    #[error("clip {index} is not a finite rectangle")]
    Invalid {
        /// Its index.
        index: usize,
    },
    /// A clip id past the table.
    #[error("clip {id} is past the table's {len} clips")]
    Unknown {
        /// The id.
        id: u16,
        /// The table's length.
        len: usize,
    },
}

/// The fixed clips a skin's runs refer to by [`ClipId`], in canvas pixels
/// `[left, top, width, height]`, installed with its atlas. A skin's ids
/// are checked against it before play ([`check`](Self::check)).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClipTable {
    rects: Box<[[f32; 4]]>,
}

impl ClipTable {
    /// The table of `rects`, clip `i` being `rects[i]`.
    ///
    /// # Errors
    ///
    /// [`ClipError::TooMany`] past [`MAX_CLIPS`], and [`ClipError::Invalid`]
    /// for the first rectangle with an edge not finite or a negative size.
    pub fn new(rects: Vec<[f32; 4]>) -> Result<Self, ClipError> {
        if rects.len() > MAX_CLIPS {
            return Err(ClipError::TooMany { count: rects.len() });
        }
        let bad = |&[l, t, w, h]: &[f32; 4]| {
            let finite = [l, t, w, h, l + w, t + h].iter().all(|v| v.is_finite());
            !finite || w < 0.0 || h < 0.0
        };
        if let Some(index) = rects.iter().position(bad) {
            return Err(ClipError::Invalid { index });
        }
        Ok(Self {
            rects: rects.into_boxed_slice(),
        })
    }

    /// The number of clips.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rects.len()
    }

    /// Whether the table holds no clip: JSON imports, until they map clips.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// Clip `id`'s rectangle, if the table has it.
    #[must_use]
    pub fn get(&self, id: ClipId) -> Option<[f32; 4]> {
        self.rects.get(usize::from(id.0)).copied()
    }

    /// Checks that every id of `ids` is in the table: a skin's clips,
    /// before it plays.
    ///
    /// # Errors
    ///
    /// [`ClipError::Unknown`] for the first one that is not.
    pub fn check(&self, ids: impl IntoIterator<Item = ClipId>) -> Result<(), ClipError> {
        match ids.into_iter().find(|&id| self.get(id).is_none()) {
            Some(id) => Err(ClipError::Unknown {
                id: id.0,
                len: self.len(),
            }),
            None => Ok(()),
        }
    }

    /// Writes into `out`, which holds one entry per clip, each clip's
    /// scissor under `projection`: nothing allocates.
    pub(crate) fn project(&self, projection: &Projection, out: &mut [Option<[u32; 4]>]) {
        for (rect, scissor) in self.rects.iter().zip(out) {
            *scissor = projection.scissor(*rect);
        }
    }
}

#[cfg(test)]
mod tests {
    use skin_model::{Canvas, ClipId, Fit};

    use super::{ClipError, ClipTable, MAX_CLIPS, Projection};
    use crate::letterbox;

    /// The fixed projection is the letterbox: the same viewport, and the
    /// layout's half, 960×540, which the shader divided by before.
    #[test]
    fn the_fixed_projection_is_the_letterbox() {
        for target in [
            [1920, 1080],
            [960, 540],
            [1280, 1024],
            [2560, 1080],
            [1366, 768],
        ] {
            let p = Projection::fixed(target);
            assert_eq!(p.viewport(), letterbox(target[0], target[1]), "{target:?}");
            assert_eq!(p.half(), [960.0, 540.0]);
            assert_eq!(p.target(), target);
        }
    }

    /// An adapted canvas fills a 21:9 target with no bars: on 2560×1080
    /// it is 2560×1080 at scale 1, wider than the layout; on a 16:10
    /// target of 1920×1200 it is 1920×1200 at scale 1, taller instead.
    #[test]
    fn an_adapted_canvas_fills_the_target() {
        let wide = Projection::new(&Canvas::new([2560, 1080], Fit::Adapt));
        assert_eq!(wide.viewport(), [0.0, 0.0, 2560.0, 1080.0]);
        assert_eq!(wide.half(), [1280.0, 540.0]);
        let tall = Projection::new(&Canvas::new([1920, 1200], Fit::Adapt));
        assert_eq!(tall.viewport(), [0.0, 0.0, 1920.0, 1200.0]);
        assert_eq!(tall.half(), [960.0, 600.0]);
    }

    /// Each edge is placed and rounded half up on its own (D1), then
    /// clamped to the target.
    #[test]
    fn scissors_round_each_edge_half_up_and_clamp() {
        // 960×540: scale 1/2, no bars. Canvas 101..301 is 50.5..150.5,
        // rounded up to 51..151; 100.5..201 (y 201..402) to 101..201.
        let half = Projection::fixed([960, 540]);
        assert_eq!(
            half.scissor([101.0, 201.0, 200.0, 201.0]),
            Some([51, 101, 100, 100])
        );
        // 1280×1024: scale 2/3, 152 rows of bars above. Canvas y 0..3 is
        // 152..154 exactly; x 0..3 is 0..2.
        let boxed = Projection::fixed([1280, 1024]);
        assert_eq!(boxed.scissor([0.0, 0.0, 3.0, 3.0]), Some([0, 152, 2, 2]));
        // Past the target: clamped to it.
        assert_eq!(
            half.scissor([-100.0, 1000.0, 3000.0, 500.0]),
            Some([0, 500, 960, 40])
        );
        // Nothing left: a rectangle below half a pixel, one off the
        // target, and an empty one.
        assert_eq!(half.scissor([10.0, 10.0, 0.5, 10.0]), None);
        assert_eq!(half.scissor([2000.0, 0.0, 10.0, 10.0]), None);
        assert_eq!(half.scissor([10.0, 10.0, 0.0, 0.0]), None);
    }

    #[test]
    fn a_table_keeps_finite_rectangles_and_checks_ids() {
        let table = ClipTable::new(vec![[0.0, 0.0, 10.0, 10.0], [5.0, 5.0, 0.0, 1.0]]).unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(table.get(ClipId(1)), Some([5.0, 5.0, 0.0, 1.0]));
        assert_eq!(table.get(ClipId(2)), None);
        assert_eq!(table.check([ClipId(0), ClipId(1)]), Ok(()));
        assert_eq!(
            table.check([ClipId(1), ClipId(7), ClipId(9)]),
            Err(ClipError::Unknown { id: 7, len: 2 })
        );
        assert_eq!(
            ClipTable::default().check([ClipId(0)]),
            Err(ClipError::Unknown { id: 0, len: 0 })
        );
        for (rects, index) in [
            (vec![[0.0, 0.0, 1.0, 1.0], [0.0, 0.0, -1.0, 1.0]], 1),
            (vec![[f32::NAN, 0.0, 1.0, 1.0]], 0),
            (vec![[0.0, 0.0, 1.0, f32::INFINITY]], 0),
            (vec![[f32::MAX, 0.0, f32::MAX, 1.0]], 0),
        ] {
            assert_eq!(ClipTable::new(rects), Err(ClipError::Invalid { index }));
        }
        assert_eq!(
            ClipTable::new(vec![[0.0; 4]; MAX_CLIPS + 1]),
            Err(ClipError::TooMany {
                count: MAX_CLIPS + 1
            })
        );
        assert_eq!(
            ClipTable::new(vec![[0.0; 4]; MAX_CLIPS]).unwrap().len(),
            MAX_CLIPS
        );
    }
}
