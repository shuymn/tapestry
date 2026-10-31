//! The canvas a target shows: its size by aspect, the caps at 4:1 and 1:2
//! with bars beyond, and the fixed 1920×1080 fit. Every expected value is
//! worked out by hand.

use skin_model::{Canvas, Fit};

fn adapt(w: u32, h: u32) -> Canvas {
    Canvas::new([w, h], Fit::Adapt)
}

#[test]
fn size_scale_and_origin_follow_the_aspect() {
    // (target, size, scale): 16:9 and wider are 1080 tall at
    // scale = height / 1080; narrower are 1920 wide at width / 1920.
    let cases: [([u32; 2], [f32; 2], Option<f32>); 10] = [
        ([1920, 1080], [1920.0, 1080.0], Some(1.0)),
        ([3840, 2160], [1920.0, 1080.0], Some(2.0)),
        ([2560, 1440], [1920.0, 1080.0], Some((4.0_f64 / 3.0) as f32)),
        ([2560, 1080], [2560.0, 1080.0], Some(1.0)),
        // 1080 · 3440 / 1440.
        ([3440, 1440], [2580.0, 1080.0], None),
        ([3840, 1080], [3840.0, 1080.0], None),
        // 1920 · 1200 / 1920.
        ([1920, 1200], [1920.0, 1200.0], None),
        // 1920 · 1080 / 1440, scale 1440 / 1920.
        ([1440, 1080], [1920.0, 1440.0], Some(0.75)),
        // 1920 · 1024 / 1280.
        ([1280, 1024], [1920.0, 1536.0], None),
        // 1366 · 9 = 12294 ≥ 768 · 16 = 12288: wide, 1080 · 1366 / 768.
        ([1366, 768], [1920.9375, 1080.0], None),
    ];
    for ([w, h], size, scale) in cases {
        let c = adapt(w, h);
        assert_eq!(c.size(), size, "{w}×{h}");
        assert_eq!(c.origin(), [0.0, 0.0], "{w}×{h}");
        if let Some(s) = scale {
            assert_eq!(c.scale(), s, "{w}×{h}");
        }
        assert_eq!(c.target(), [w, h]);
        assert_eq!(c.fit(), Fit::Adapt);
    }
    // 1919 · 9 < 1080 · 16: just narrower than 16:9, so 1920 wide and a
    // little taller, not a little narrower.
    let c = adapt(1919, 1080);
    assert_eq!(c.size(), [1920.0, (1080.0_f64 * 1920.0 / 1919.0) as f32]);
    assert!(c.size()[1] > 1080.0);
}

#[test]
fn caps_hold_at_their_boundary() {
    // 4:1 exactly: no bar.
    let c = adapt(4320, 1080);
    assert_eq!(
        (c.size(), c.scale(), c.origin()),
        ([4320.0, 1080.0], 1.0, [0.0, 0.0])
    );
    // One pixel wider: capped, a half-pixel bar each side.
    let c = adapt(4321, 1080);
    assert_eq!(
        (c.size(), c.scale(), c.origin()),
        ([4320.0, 1080.0], 1.0, [0.5, 0.0])
    );
    // 1:2 exactly: 3840 tall, no bar.
    let c = adapt(1080, 2160);
    assert_eq!((c.size(), c.origin()), ([1920.0, 3840.0], [0.0, 0.0]));
    // One pixel taller: capped at scale 1080 / 1920 = 0.5625, so 2160
    // target pixels hold 3840 and a half pixel is left above and below.
    let c = adapt(1080, 2161);
    assert_eq!(
        (c.size(), c.scale(), c.origin()),
        ([1920.0, 3840.0], 0.5625, [0.0, 0.5])
    );
    // A zero dimension counts as 1: a 1×1 target, narrower than 16:9.
    for zero in [[0, 0], [0, 1], [1, 0]] {
        let c = Canvas::new(zero, Fit::Adapt);
        let one = adapt(1, 1);
        assert_eq!(
            (c.size(), c.scale(), c.origin()),
            (one.size(), one.scale(), one.origin())
        );
        assert_eq!(c.size(), [1920.0, 1920.0]);
        assert_eq!(c.target(), zero);
    }
}

#[test]
fn fixed_fits_the_1080p_layout_with_bars() {
    // The renderer's letterbox literals: [x, y, w, h] of the viewport,
    // here as origin and scale (w = 1920 · scale).
    let cases = [
        ([1920, 1080], [0.0, 0.0], 1.0),
        ([960, 540], [0.0, 0.0], 0.5),
        // 5:4: scaled to the width (2/3), bars above and below.
        ([1280, 1024], [0.0, 152.0], 1280.0 / 1920.0),
        // 21:9: scaled to the height, bars left and right.
        ([2560, 1080], [320.0, 0.0], 1.0),
    ];
    for (target, origin, scale) in cases {
        let c = Canvas::new(target, Fit::Fixed);
        assert_eq!(c.size(), [1920.0, 1080.0], "{target:?}");
        assert_eq!(c.origin(), origin, "{target:?}");
        assert_eq!(c.scale(), scale, "{target:?}");
    }
    assert_eq!(
        Canvas::new([1280, 1024], Fit::Fixed).scale() * 1080.0,
        720.0
    );
}
