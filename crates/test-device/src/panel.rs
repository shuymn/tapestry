//! The panel-3 rows' click targets (docs/test-device.md "Verification",
//! P1): one guarded click per volume slider of the select skin used for
//! testing, at a client point of a 1920×1080 window, which both players
//! take alike. The runner clicks these points, and the tests that pin
//! their values read this table.

/// One volume slider's target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// The skin's slider number: 17 system, 18 key, 19 background.
    pub slider: u8,
    /// The client point clicked, in the window's pixels.
    pub x: i32,
    pub y: i32,
    /// The whole percent the click sets, and both players draw.
    pub percent: u8,
}

/// The window's client size the targets are for.
pub const CLIENT: [i32; 2] = [1920, 1080];

/// Where each slider's travel begins and how long it is, in client
/// pixels: x − 212 is a multiple of 44, a quarter of 176, so each value is
/// k / 4, exact in `f32` and `f64`.
pub const TRAVEL_FROM: i32 = 212;
pub const TRAVEL: i32 = 176;

/// The three targets: y is the centre of each slider's 31-pixel row.
pub const TARGETS: [Target; 3] = [
    Target {
        slider: 17,
        x: 256,
        y: 606,
        percent: 25,
    },
    Target {
        slider: 18,
        x: 300,
        y: 657,
        percent: 50,
    },
    Target {
        slider: 19,
        x: 344,
        y: 709,
        percent: 75,
    },
];

/// The reference player's control run (`p1-ref-75`): slider 17 at 75 %,
/// sliders 18 and 19 as [`TARGETS`]. Compared with tapestry's shot at
/// [`TARGETS`], it must differ at slider 17 alone.
pub const REF_75: [Target; 3] = [
    Target {
        slider: 17,
        x: 344,
        y: 606,
        percent: 75,
    },
    TARGETS[1],
    TARGETS[2],
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Each target's x lies a whole quarter along its travel, and that
    /// quarter is its percent.
    #[test]
    fn every_target_is_a_quarter_of_its_travel() {
        for t in TARGETS.into_iter().chain(REF_75) {
            let along = t.x - TRAVEL_FROM;
            assert_eq!(along * 4 % TRAVEL, 0, "{t:?}");
            assert_eq!(along * 100 / TRAVEL, i32::from(t.percent), "{t:?}");
            assert!((0..CLIENT[0]).contains(&t.x) && (0..CLIENT[1]).contains(&t.y));
        }
    }

    /// The control table differs from the targets at slider 17 alone, set
    /// to another whole quarter on the same row.
    #[test]
    fn ref_75_moves_slider_17_alone() {
        let moved: Vec<_> = TARGETS
            .iter()
            .zip(&REF_75)
            .filter(|(t, r)| t != r)
            .collect();
        assert_eq!(moved.len(), 1, "{moved:?}");
        let (t, r) = moved[0];
        assert_eq!((t.slider, r.slider), (17, 17));
        assert_eq!((r.y, r.percent), (t.y, 75));
        assert_ne!(r.x, t.x);
    }
}
