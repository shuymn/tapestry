//! The reference canvas a target shows: its size in reference pixels, and
//! the uniform scale and origin that map it onto the target's pixels.
//!
//! The aspect class is decided in integers, so 16:9 lands exactly on
//! 1920×1080; the size is `f32` because an adapted width is rational
//! (1366×768 gives 1920.9375).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

/// How a skin's canvas fits a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// 1080 reference pixels tall and `1080 × aspect` wide; narrower than
    /// 16:9, 1920 wide and taller instead. Capped at [`MAX_CANVAS`], with
    /// bars beyond.
    Adapt,
    /// 1920×1080, fitted whole with bars on the other axis: imported
    /// skins.
    Fixed,
}

/// The largest canvas: 4:1 wide, 1:2 tall.
pub const MAX_CANVAS: [u32; 2] = [4_320, 3_840];

/// The canvas a skin draws on for one target size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Canvas {
    size: [f32; 2],
    scale: f32,
    origin: [f32; 2],
    fit: Fit,
    target: [u32; 2],
}

/// `v` as `f32`, rounded to nearest.
#[expect(
    clippy::cast_possible_truncation,
    reason = "canvas values are below 2^33; rounding to f32 is the intent"
)]
pub(crate) fn f32_of(v: f64) -> f32 {
    v as f32
}

impl Canvas {
    /// The canvas of a `target`-pixel target under `fit`. A zero dimension
    /// counts as 1, so this is total; it allocates nothing.
    #[must_use]
    pub fn new(target: [u32; 2], fit: Fit) -> Self {
        let [tw, th] = target.map(|v| v.max(1));
        let (w, h) = (f64::from(tw), f64::from(th));
        let (size, scale, origin) = match fit {
            Fit::Fixed => {
                // The pillarbox and letterbox of the 1920×1080 layout, in
                // `f32` as the renderer computes it.
                let (w, h) = (tw as f32, th as f32);
                let s = (w / 1_920.0).min(h / 1_080.0);
                let origin = [(w - 1_920.0 * s) / 2.0, (h - 1_080.0 * s) / 2.0];
                ([1_920.0, 1_080.0], s, origin)
            }
            // 16:9 or wider: 1080 tall.
            Fit::Adapt if u64::from(tw) * 9 >= u64::from(th) * 16 => {
                let s = h / 1_080.0;
                if u64::from(tw) > 4 * u64::from(th) {
                    let cw = f64::from(MAX_CANVAS[0]);
                    let x = (w - cw * s) / 2.0;
                    ([f32_of(cw), 1_080.0], f32_of(s), [f32_of(x), 0.0])
                } else {
                    ([f32_of(1_080.0 * w / h), 1_080.0], f32_of(s), [0.0, 0.0])
                }
            }
            // Narrower: 1920 wide.
            Fit::Adapt => {
                let s = w / 1_920.0;
                if u64::from(th) > 2 * u64::from(tw) {
                    let ch = f64::from(MAX_CANVAS[1]);
                    let y = (h - ch * s) / 2.0;
                    ([1_920.0, f32_of(ch)], f32_of(s), [0.0, f32_of(y)])
                } else {
                    ([1_920.0, f32_of(1_920.0 * h / w)], f32_of(s), [0.0, 0.0])
                }
            }
        };
        Self {
            size,
            scale,
            origin,
            fit,
            target,
        }
    }

    /// The size in reference pixels.
    #[must_use]
    pub fn size(&self) -> [f32; 2] {
        self.size
    }

    /// Target pixels per reference pixel.
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Where the canvas's (0, 0) lies on the target, in target pixels: the
    /// width of the bars.
    #[must_use]
    pub fn origin(&self) -> [f32; 2] {
        self.origin
    }

    /// The fit it was made with.
    #[must_use]
    pub fn fit(&self) -> Fit {
        self.fit
    }

    /// The target size it was made for, as given.
    #[must_use]
    pub fn target(&self) -> [u32; 2] {
        self.target
    }
}
