//! `Skin::variant`: `Hi` exactly when some image the skin can draw is
//! magnified on its smaller axis, counting the canvas scale, its region's
//! scale, the fills and, for lane-bound images and the lanes, a lane width
//! below 100 %. Targets are literals; each expectation is the product
//! worked out in its comment.

mod lanes_support;
mod play_support;

use bms_model::Lane;
use lanes_support::{WHITE, bound, image, in_region, lanes_element, sheet, simple, skin_with};
use play_support::REGIONS;
use skin_model::{
    Adjust, Bind, ElementDef, Fill, Fit, Layout, Place, Region, Skin, SkinDef, Variant,
};

fn scaled(r: Region, scale: u16) -> Layout {
    let mut l = Layout::AUTHOR;
    l.set(
        r,
        Adjust {
            scale,
            ..Adjust::IDENTITY
        },
    )
    .unwrap();
    l
}

fn adapted(elements: Vec<ElementDef>) -> Skin {
    skin_with(REGIONS, None, elements).unwrap()
}

fn fixed(elements: Vec<ElementDef>) -> Skin {
    Skin::new(SkinDef {
        fit: Fit::Fixed,
        regions: REGIONS,
        whens: Vec::new(),
        sheet: sheet(),
        lanes: None,
        elements,
    })
    .unwrap()
}

fn playfield_image() -> ElementDef {
    image(WHITE, [0, 0, 10, 10], in_region(Region::Playfield))
}

#[test]
fn the_canvas_scale_alone() {
    // An image on `screen` at 100 %: the canvas scale decides. 1921×1081
    // is narrower than 16:9 (1921·9 < 1081·16), so 1921/1920 > 1; 3840×1080
    // and 2560×1080 are 1080 tall (1); 1280×720 is 2/3.
    let s = adapted(vec![image(WHITE, [0, 0, 10, 10], Place::SCREEN)]);
    let l = Layout::AUTHOR;
    for (target, want) in [
        ([1_920, 1_080], Variant::Lo),
        ([1_921, 1_081], Variant::Hi),
        ([3_840, 1_080], Variant::Lo),
        ([2_560, 1_080], Variant::Lo),
        ([3_840, 2_160], Variant::Hi),
        ([1_280, 720], Variant::Lo),
        ([1_920, 1_081], Variant::Lo),
        ([1_921, 1_080], Variant::Lo),
    ] {
        assert_eq!(s.variant(target, &l), want, "{target:?}");
    }
    // Fixed: min(w/1920, h/1080).
    let s = fixed(vec![image(WHITE, [0, 0, 10, 10], Place::SCREEN)]);
    assert_eq!(s.variant([3_840, 1_080], &l), Variant::Lo);
    assert_eq!(s.variant([3_840, 2_160], &l), Variant::Hi);
    assert_eq!(s.variant([1_921, 1_081], &l), Variant::Hi);
}

#[test]
fn a_regions_scale_counts() {
    let s = adapted(vec![playfield_image()]);
    // 1 × 120 % > 1; 1 × 100 % is not.
    assert_eq!(
        s.variant([1_920, 1_080], &scaled(Region::Playfield, 120)),
        Variant::Hi
    );
    assert_eq!(
        s.variant([1_920, 1_080], &scaled(Region::Playfield, 101)),
        Variant::Hi
    );
    assert_eq!(s.variant([1_920, 1_080], &Layout::AUTHOR), Variant::Lo);
    // 2 × 50 % = 1 at 4K: the only image-bearing region halved.
    assert_eq!(
        s.variant([3_840, 2_160], &scaled(Region::Playfield, 50)),
        Variant::Lo
    );
    assert_eq!(
        s.variant([3_840, 2_160], &scaled(Region::Playfield, 51)),
        Variant::Hi
    );
    // Another region's scale does not reach it.
    assert_eq!(
        s.variant([1_920, 1_080], &scaled(Region::Top, 200)),
        Variant::Lo
    );
    // A hidden judge does not count; shown, it does.
    let s = adapted(vec![image(WHITE, [0, 0, 10, 10], in_region(Region::Judge))]);
    let mut l = scaled(Region::Judge, 150);
    assert_eq!(s.variant([1_920, 1_080], &l), Variant::Hi);
    l.set(
        Region::Judge,
        Adjust {
            scale: 150,
            visible: false,
            ..Adjust::IDENTITY
        },
    )
    .unwrap();
    assert_eq!(s.variant([1_920, 1_080], &l), Variant::Lo);
}

#[test]
fn fills_count_their_own_scale() {
    // 2560×1080: cover max(4/3, 1) > 1, stretch min(4/3, 1) = 1.
    let fill = |fill| {
        image(
            WHITE,
            [0, 0, 1_920, 1_080],
            Place {
                fill,
                ..Place::SCREEN
            },
        )
    };
    let l = Layout::AUTHOR;
    assert_eq!(
        adapted(vec![fill(Fill::Cover)]).variant([2_560, 1_080], &l),
        Variant::Hi
    );
    assert_eq!(
        adapted(vec![fill(Fill::Stretch)]).variant([2_560, 1_080], &l),
        Variant::Lo
    );
    // 1920×1440 (4:3 → canvas 1920×1440, scale 0.75): cover max(1, 4/3) ×
    // 0.75 = 1.
    assert_eq!(
        adapted(vec![fill(Fill::Cover)]).variant([1_440, 1_080], &l),
        Variant::Lo
    );
    // 2560×1440 (canvas 1920×1080 at 4/3) stretches by 4/3.
    assert_eq!(
        adapted(vec![fill(Fill::Stretch)]).variant([2_560, 1_440], &l),
        Variant::Hi
    );
}

#[test]
fn the_lane_width_narrows_lane_bound_images_only() {
    // The lanes and a beam bound to key 1 at 3840×2160: 2 × 50 % = 1 on x,
    // the smaller axis, so `Lo`; at 51 %, `Hi`.
    let beam = image(
        WHITE,
        [264, 100, 48, 780],
        bound(Bind::Lane(Lane::new(1).unwrap())),
    );
    let s = skin_with(REGIONS, Some(simple()), vec![lanes_element(), beam]).unwrap();
    let width = |pct| {
        let mut l = Layout::AUTHOR;
        l.set_lane_width(pct).unwrap();
        l
    };
    assert_eq!(s.variant([3_840, 2_160], &width(50)), Variant::Lo);
    assert_eq!(s.variant([3_840, 2_160], &width(51)), Variant::Hi);
    assert_eq!(s.variant([3_840, 2_160], &width(100)), Variant::Hi);
    // Widening alone never picks `Hi`: 1 × min(150 %, 100 %).
    assert_eq!(s.variant([1_920, 1_080], &width(150)), Variant::Lo);
    assert_eq!(s.variant([1_920, 1_080], &width(200)), Variant::Lo);
    // An unbound image in the playfield keeps `Hi` at 50 %.
    let s = skin_with(
        REGIONS,
        Some(simple()),
        vec![lanes_element(), playfield_image()],
    )
    .unwrap();
    assert_eq!(s.variant([3_840, 2_160], &width(50)), Variant::Hi);
}
