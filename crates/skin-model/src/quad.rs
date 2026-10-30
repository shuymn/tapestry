//! Quads and the per-frame batch a skin fills.
//!
//! The batch keeps one ordered list, so an alpha quad pushed after an
//! additive one is drawn after it, and splits it into runs wherever the
//! blend or the clip changes; a renderer draws the runs in order.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

/// One quad in canvas pixels.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Quad {
    /// Left, top, width, height.
    pub rect: [f32; 4],
    /// Atlas coordinates: left, top, right, bottom.
    pub uv: [f32; 4],
    /// Color multiplied with the atlas sample, RGBA.
    pub color: [f32; 4],
}

/// How a quad is blended over what is under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blend {
    /// Ordinary alpha blending.
    Alpha,
    /// Added on top.
    Add,
}

/// An opaque clip: an index into the clip table of the skin that pushes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipId(pub u16);

/// What a run shares: neighbouring quads with equal keys draw together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunKey {
    pub blend: Blend,
    pub clip: Option<ClipId>,
}

/// The quads `start..end` of a batch, all with `key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub key: RunKey,
    pub start: u32,
    pub end: u32,
}

/// The quads of one frame, in draw order, at most `quads` of them in at
/// most `runs` runs.
///
/// Both lists are allocated once, at full capacity, so pushing never
/// allocates. A push beyond either capacity is dropped and counted.
#[derive(Debug)]
pub struct QuadBatch {
    quads: Vec<Quad>,
    runs: Vec<Run>,
    quad_cap: u32,
    run_cap: u32,
    dropped: u32,
}

/// `n` as a `Vec` capacity, and the cap the batch then enforces (0 on a
/// target whose `usize` cannot hold it).
fn reserve(n: u32) -> (usize, u32) {
    usize::try_from(n).map_or((0, 0), |c| (c, n))
}

impl QuadBatch {
    /// An empty batch holding up to `quads` quads in up to `runs` runs.
    #[must_use]
    pub fn with_capacity(quads: u32, runs: u32) -> Self {
        let (quad_len, quad_cap) = reserve(quads);
        let (run_len, run_cap) = reserve(runs);
        Self {
            quads: Vec::with_capacity(quad_len),
            runs: Vec::with_capacity(run_len),
            quad_cap,
            run_cap,
            dropped: 0,
        }
    }

    /// Empties the batch and its drop count for the next frame, keeping its
    /// capacity.
    pub fn clear(&mut self) {
        self.quads.clear();
        self.runs.clear();
        self.dropped = 0;
    }

    /// The number of quads held: the end of the last run.
    fn len(&self) -> u32 {
        self.runs.last().map_or(0, |r| r.end)
    }

    fn drop_one(&mut self) -> bool {
        self.dropped = self.dropped.saturating_add(1);
        false
    }

    /// Adds `quad` with `key`, extending the last run when its key is
    /// equal. Returns `false` and counts the quad as dropped when the quad
    /// list is full, or when it needs a new run and the run list is full.
    pub fn push(&mut self, key: RunKey, quad: Quad) -> bool {
        let len = self.len();
        if len >= self.quad_cap {
            return self.drop_one();
        }
        match self.runs.last_mut() {
            Some(run) if run.key == key => run.end = len + 1,
            _ => {
                let runs_full = u32::try_from(self.runs.len()).map_or(true, |n| n >= self.run_cap);
                if runs_full {
                    return self.drop_one();
                }
                self.runs.push(Run {
                    key,
                    start: len,
                    end: len + 1,
                });
            }
        }
        self.quads.push(quad);
        true
    }

    /// The quads, in push order.
    #[must_use]
    pub fn quads(&self) -> &[Quad] {
        &self.quads
    }

    /// The runs, in push order; they cover the quads contiguously, and
    /// neighbouring runs have different keys.
    #[must_use]
    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// Quads dropped since the last [`clear`](Self::clear).
    #[must_use]
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Whether the quad list is full, so every push would be dropped.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.len() >= self.quad_cap
    }
}
