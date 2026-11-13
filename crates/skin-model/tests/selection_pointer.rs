//! Pointer input on a selection skin (docs/stage3.md S10-09e): what a press
//! hits among the targets the last build drew, topmost first, a song list
//! trying its slots in its clickable order, the source's click argument by
//! button and click type, and an element drawn only while the pointer is
//! inside its hover rect.

mod selection_support;

use selection_support::{
    Fx, boolean, chart, element, folder, list, list_element, load, placement, ratio, rect, view,
};
use skin_model::{
    ActionDef, Button, ClickType, ElementDef, HoverRect, LoadError, PointerDef, PointerError,
    PointerHit, PressHit, RatioBarDef, RatioForm, RatioSource, RowKind, RowRef, RowRefKind,
    RowView, SearchBoxes, SelectionDefs, SelectionText, SliderCommand, SliderDirection,
    SongListDef, SongListError, Source, TextDef, TextValue, WidgetDefs,
};

/// The revision every numbered view here carries.
const REVISION: u64 = 7;

/// A view of `rows`, each numbered by its place, the cursor on the first,
/// at [`REVISION`].
fn numbered(rows: Vec<RowView>) -> skin_model::SelectionView {
    let rows = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| RowView {
            index: Some(u32::try_from(i).unwrap()),
            ..r
        })
        .collect();
    let mut v = view(rows, Some(0));
    v.list_revision = REVISION;
    v
}

/// The identity of chart row `index` at [`REVISION`].
fn chart_ref(index: u32) -> RowRef {
    RowRef {
        list_revision: REVISION,
        index,
        kind: RowRefKind::Chart,
    }
}

/// An action entry for element `element`.
fn action(element: usize, event: u16, click: ClickType) -> PointerDef {
    PointerDef {
        element: u32::try_from(element).unwrap(),
        action: Some(ActionDef { event, click }),
        hover: None,
        search: false,
        region: None,
    }
}

/// A changeable list-position slider to the right over 100.
fn slider() -> RatioBarDef {
    RatioBarDef {
        image: 4,
        form: RatioForm::Slider {
            direction: SliderDirection::Right,
            range: 100,
            changeable: true,
        },
        source: RatioSource::ListPosition,
    }
}

/// A skin of `elements` with `sel`, its view `v` built at 0 ms.
fn built(elements: Vec<ElementDef>, sel: SelectionDefs, v: &skin_model::SelectionView) -> Fx {
    let mut fx = Fx::new(load(elements, WidgetDefs::default(), sel).unwrap());
    fx.view(v);
    fx.frame(0);
    fx
}

/// The press at `p` with `button` on `fx`'s last build.
fn hit(fx: &Fx, p: [f32; 2], button: Button) -> Option<PointerHit> {
    fx.skin.hit_at(&fx.sel, p, fx.target, button).hit
}

/// A slider, an action and a song list slot over one point: the topmost
/// takes the press, in every order; a drag passes over the action and the
/// list to the slider.
#[test]
fn the_topmost_target_takes_the_press() {
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum T {
        List,
        Action,
        Slider,
    }
    let orders = [
        [T::List, T::Action, T::Slider],
        [T::List, T::Slider, T::Action],
        [T::Action, T::List, T::Slider],
        [T::Action, T::Slider, T::List],
        [T::Slider, T::List, T::Action],
        [T::Slider, T::Action, T::List],
    ];
    let point = [50.0, 25.0];
    // The slider's travel at ratio 0 runs x 10 to 110: 40 along.
    let slide = SliderCommand {
        source: RatioSource::ListPosition,
        value: skin_model::Ratio::new(400_000, skin_model::SLIDER_STEPS).unwrap(),
    };
    let mut v = numbered(vec![chart("a"), chart("b")]);
    v.position = ratio(1, 2);
    for order in orders {
        let elements = order
            .iter()
            .map(|t| match t {
                T::List => list_element(),
                T::Action => element(Source::Image(5), [10, 0, 200, 50]),
                T::Slider => element(Source::RatioBar(0), [10, 0, 20, 50]),
            })
            .collect();
        let at = order.iter().position(|&t| t == T::Action).unwrap();
        let mut def = list(1, 0);
        def.clickable = vec![0];
        let fx = built(
            elements,
            SelectionDefs {
                song_list: Some(def),
                ratio_bars: vec![slider()],
                pointer: vec![action(at, 12, ClickType::Plus)],
            },
            &v,
        );
        let want = match order[2] {
            T::List => PointerHit::Row {
                row: chart_ref(0),
                button: Button::Left,
            },
            T::Action => PointerHit::Action { event: 12, arg: 1 },
            T::Slider => PointerHit::Slider(slide),
        };
        assert_eq!(hit(&fx, point, Button::Left), Some(want), "{order:?}");
        assert_eq!(
            fx.skin.drag_at(&fx.sel, point, fx.target),
            Some(slide),
            "{order:?}"
        );
        // Off the slider's travel but on the others, the next one down.
        let below = [150.0, 25.0];
        let want = match order.iter().rev().find(|&&t| t != T::Slider) {
            Some(T::List) => PointerHit::Row {
                row: chart_ref(0),
                button: Button::Left,
            },
            _ => PointerHit::Action { event: 12, arg: 1 },
        };
        assert_eq!(hit(&fx, below, Button::Left), Some(want), "{order:?}");
        assert_eq!(fx.skin.drag_at(&fx.sel, below, fx.target), None);
        // Another target size than the build's hits nothing.
        assert_eq!(
            fx.skin.hit_at(&fx.sel, point, [1_280, 720], Button::Left),
            PressHit::NONE
        );
        assert_eq!(fx.skin.drag_at(&fx.sel, point, [1_280, 720]), None);
    }
}

/// Two slots drawn over one point: the list tries its clickable slots in
/// their order, the first that holds the point taking it, with the button.
#[test]
fn a_song_list_tries_its_slots_in_clickable_order() {
    let rows = (0..8).map(|i| chart(&format!("c{i}"))).collect();
    let v = numbered(rows);
    for (clickable, slot) in [(vec![7, 3], 7), (vec![3, 7], 3), (vec![0, 3, 7], 3)] {
        let mut def = list(8, 0);
        for i in [3, 7] {
            def.placements[i] = [
                placement(0, [10, 300, 200, 50]),
                placement(1, [10, 300, 200, 50]),
            ];
        }
        def.clickable = clickable.clone();
        let fx = built(
            vec![list_element()],
            SelectionDefs {
                song_list: Some(def),
                ..SelectionDefs::default()
            },
            &v,
        );
        assert_eq!(
            hit(&fx, [100.0, 320.0], Button::Right),
            Some(PointerHit::Row {
                row: chart_ref(slot),
                button: Button::Right,
            }),
            "{clickable:?}"
        );
        // A slot not clickable takes no press.
        assert_eq!(hit(&fx, [100.0, 120.0], Button::Left), None);
    }
}

/// A row's identity: its index and kind at the view's revision; a heading,
/// a row the view does not number and a slot without its bar image draw no
/// target; a slot's edges are inside it.
#[test]
fn a_row_is_hit_only_where_its_bar_drew() {
    let heading = RowView {
        key: skin_model::RowKey::Heading(skin_model::FolderId::new("h".into())),
        kind: RowKind::Heading,
        ..chart("")
    };
    let mut v = numbered(vec![folder("f", 2), heading, chart("c"), chart("d")]);
    v.rows[3].index = None;
    let mut def = list(5, 0);
    def.clickable = vec![0, 1, 2, 3, 4];
    // Slot 4 has no bar image for any row kind.
    def.placements[4] = [
        placement(0, [10, 400, 200, 50]),
        placement(1, [10, 400, 200, 50]),
    ];
    def.placements[4][0].images = [None; 3];
    v.rows.push(RowView {
        index: Some(4),
        ..chart("e")
    });
    let fx = built(
        vec![list_element()],
        SelectionDefs {
            song_list: Some(def),
            ..SelectionDefs::default()
        },
        &v,
    );
    let row = |index, kind| {
        Some(PointerHit::Row {
            row: RowRef {
                list_revision: REVISION,
                index,
                kind,
            },
            button: Button::Left,
        })
    };
    assert_eq!(
        hit(&fx, [10.0, 0.0], Button::Left),
        row(0, RowRefKind::Folder)
    );
    assert_eq!(
        hit(&fx, [210.0, 50.0], Button::Left),
        row(0, RowRefKind::Folder)
    );
    assert_eq!(hit(&fx, [210.5, 50.0], Button::Left), None);
    // The heading draws no bar.
    assert_eq!(hit(&fx, [100.0, 120.0], Button::Left), None);
    assert_eq!(
        hit(&fx, [100.0, 220.0], Button::Left),
        row(2, RowRefKind::Chart)
    );
    // Drawn, but not numbered.
    assert_eq!(hit(&fx, [100.0, 320.0], Button::Left), None);
    // Numbered, but its slot has no bar image.
    assert_eq!(hit(&fx, [100.0, 420.0], Button::Left), None);
}

/// The source's argument by click type and button, each half's boundary
/// tested on both sides, the middle line in the right and the top half.
#[test]
fn the_click_type_and_button_give_the_argument() {
    let v = view(vec![chart("a")], Some(0));
    let fx = |click| {
        built(
            vec![element(Source::Image(5), [100, 100, 40, 20])],
            SelectionDefs {
                pointer: vec![action(0, 13, click)],
                ..SelectionDefs::default()
            },
            &v,
        )
    };
    let arg = |fx: &Fx, p, button| match hit(fx, p, button) {
        Some(PointerHit::Action { event: 13, arg }) => arg,
        other => panic!("{other:?}"),
    };
    let mid = [120.0, 110.0];
    let plus = fx(ClickType::Plus);
    let minus = fx(ClickType::Minus);
    for (button, step) in [
        (Button::Left, 1),
        (Button::Right, -1),
        (Button::Middle, 1),
        (Button::Back, 1),
        (Button::Forward, -1),
    ] {
        assert_eq!(arg(&plus, mid, button), step, "{button:?}");
        assert_eq!(arg(&minus, mid, button), -step, "{button:?}");
    }
    // Halves: x = r.x + w/2 is the right half; one pixel left of it the left.
    let halves = fx(ClickType::Halves);
    for (x, want) in [(119.0, -1), (120.0, 1), (100.0, -1), (140.0, 1)] {
        for button in [Button::Left, Button::Right] {
            assert_eq!(arg(&halves, [x, 110.0], button), want, "x {x}");
        }
    }
    // Top and bottom: the source's y-up `y ≥ r.y + h/2` is y ≤ 110 here,
    // the middle line included; the bottom half gives −1.
    let halves = fx(ClickType::TopBottom);
    for (y, want) in [
        (100.0, 1),
        (110.0, 1),
        (111.0, -1),
        (115.0, -1),
        (120.0, -1),
    ] {
        for button in [Button::Left, Button::Right] {
            assert_eq!(arg(&halves, [120.0, y], button), want, "y {y}");
        }
    }
    // Outside the rect, nothing.
    assert_eq!(hit(&plus, [99.0, 110.0], Button::Left), None);
    assert_eq!(hit(&plus, [120.0, 121.0], Button::Left), None);
}

/// An action whose element did not draw takes no press: hidden by its
/// visibility, or by its hover rect with the pointer outside or absent.
#[test]
fn an_action_that_did_not_draw_takes_no_press() {
    let v = view(vec![folder("f", 1)], Some(0));
    let mut e = element(Source::Image(5), [100, 100, 40, 20]);
    e.visible = Some(boolean("cursor.chart"));
    let fx = built(
        vec![e],
        SelectionDefs {
            pointer: vec![action(0, 12, ClickType::Plus)],
            ..SelectionDefs::default()
        },
        &v,
    );
    assert_eq!(hit(&fx, [120.0, 110.0], Button::Left), None);
    let mut fx = built(
        vec![element(Source::Image(5), [100, 100, 40, 20])],
        SelectionDefs {
            pointer: vec![PointerDef {
                hover: Some(HoverRect {
                    x: 0,
                    y: 0,
                    w: 40,
                    h: 20,
                }),
                ..action(0, 12, ClickType::Plus)
            }],
            ..SelectionDefs::default()
        },
        &v,
    );
    assert!(fx.batch.quads().is_empty());
    assert_eq!(hit(&fx, [120.0, 110.0], Button::Left), None);
    fx.pointer = Some([300.0, 110.0]);
    fx.frame(0);
    assert_eq!(hit(&fx, [120.0, 110.0], Button::Left), None);
    fx.pointer = Some([120.0, 110.0]);
    fx.frame(0);
    assert_eq!(fx.batch.quads().len(), 1);
    assert_eq!(
        hit(&fx, [120.0, 110.0], Button::Left),
        Some(PointerHit::Action { event: 12, arg: 1 })
    );
}

/// Text showing the query takes a press inside its box as the search box,
/// whatever the button and with nothing typed; another text does not.
#[test]
fn the_search_box_takes_a_press_with_an_empty_query() {
    let text = |value| TextDef {
        value,
        font: skin_model::FontRef::Default,
        align: skin_model::Align::Left,
        overflow: skin_model::Overflow::Spill,
    };
    let skin = load(
        vec![
            element(Source::Text(0), [100, 100, 200, 20]),
            element(Source::Text(1), [100, 200, 200, 20]),
        ],
        WidgetDefs {
            texts: vec![
                text(TextValue::Selection(SelectionText::Query)),
                text(TextValue::Selection(SelectionText::Place)),
            ],
            ..WidgetDefs::default()
        },
        SelectionDefs {
            pointer: vec![PointerDef {
                element: 0,
                action: None,
                hover: None,
                search: true,
                region: None,
            }],
            ..SelectionDefs::default()
        },
    )
    .unwrap();
    let mut fx = Fx::new(skin);
    let v = view(Vec::new(), None);
    assert!(v.query.is_empty());
    fx.view(&v);
    fx.frame(0);
    for button in [Button::Left, Button::Right, Button::Middle] {
        assert_eq!(
            hit(&fx, [100.0, 100.0], button),
            Some(PointerHit::Search { index: 0 })
        );
        assert_eq!(
            hit(&fx, [300.0, 120.0], button),
            Some(PointerHit::Search { index: 0 })
        );
    }
    assert_eq!(hit(&fx, [301.0, 110.0], Button::Left), None);
    assert_eq!(hit(&fx, [150.0, 210.0], Button::Left), None);
}

/// An element with a hover rect draws only while the pointer is inside it:
/// the rect is an offset from the element's bottom-left corner, y up, its
/// edges inside it.
#[test]
fn a_hover_rect_shows_its_element_only_under_the_pointer() {
    // An image over a 179×37 region at (1 000, 300), its rect `h` tall.
    let skin = |h| {
        built(
            vec![element(Source::Image(5), [1_000, 300, 179, 37])],
            SelectionDefs {
                pointer: vec![PointerDef {
                    element: 0,
                    action: None,
                    hover: Some(HoverRect {
                        x: 0,
                        y: 0,
                        w: 179,
                        h,
                    }),
                    search: false,
                    region: None,
                }],
                ..SelectionDefs::default()
            },
            &view(vec![chart("a")], Some(0)),
        )
    };
    let drawn = |fx: &mut Fx, pointer| {
        fx.pointer = pointer;
        fx.frame(0);
        !fx.batch.quads().is_empty()
    };
    let mut whole = skin(37);
    assert!(drawn(&mut whole, Some([1_010.0, 310.0])));
    assert!(!drawn(&mut whole, Some([1_200.0, 310.0])));
    assert!(!drawn(&mut whole, None));
    // The bottom half in y-up space: from 337 − 18 = 319 down to 337.
    let mut half = skin(18);
    assert!(drawn(&mut half, Some([1_010.0, 330.0])));
    assert!(!drawn(&mut half, Some([1_010.0, 305.0])));
    for (y, want) in [(318.0, false), (319.0, true), (337.0, true), (338.0, false)] {
        assert_eq!(drawn(&mut half, Some([1_010.0, y])), want, "y {y}");
    }
    for (x, want) in [
        (999.0, false),
        (1_000.0, true),
        (1_179.0, true),
        (1_180.0, false),
    ] {
        assert_eq!(drawn(&mut half, Some([x, 330.0])), want, "x {x}");
    }
}

/// What a press hits is `Copy` and small.
#[test]
fn a_hit_is_small_and_copy() {
    fn copy<T: Copy>() {}
    copy::<PointerHit>();
    copy::<RowRef>();
    assert!(size_of::<PointerHit>() <= 32);
    assert_eq!(size_of::<RowRef>(), 16);
}

/// The load refuses an entry naming no element or one another names, an
/// action off an image, a search box off the query's text, a hover rect
/// past the coordinate range, and a clickable slot past the placements.
#[test]
fn load_refuses_misplaced_pointer_entries() {
    let text = TextDef {
        value: TextValue::Selection(SelectionText::Place),
        font: skin_model::FontRef::Default,
        align: skin_model::Align::Left,
        overflow: skin_model::Overflow::Spill,
    };
    let try_load = |pointer: Vec<PointerDef>| {
        load(
            vec![
                element(Source::Image(5), [0, 0, 10, 10]),
                element(Source::Text(0), [0, 0, 10, 10]),
            ],
            WidgetDefs {
                texts: vec![text.clone()],
                ..WidgetDefs::default()
            },
            SelectionDefs {
                pointer,
                ..SelectionDefs::default()
            },
        )
    };
    let plain = |element| PointerDef {
        element,
        action: None,
        hover: None,
        search: false,
        region: None,
    };
    let refused = |r: Result<_, LoadError>| match r {
        Err(LoadError::Pointer { entry, error }) => Some((entry, error)),
        Err(e) => panic!("{e}"),
        Ok(_) => None,
    };
    assert_eq!(
        refused(try_load(vec![plain(2)])),
        Some((0, PointerError::UnknownElement))
    );
    assert_eq!(
        refused(try_load(vec![plain(0), plain(1), plain(0)])),
        Some((2, PointerError::Duplicate))
    );
    assert_eq!(
        refused(try_load(vec![action(1, 12, ClickType::Plus)])),
        Some((0, PointerError::ActionSource))
    );
    assert_eq!(
        refused(try_load(vec![PointerDef {
            search: true,
            ..plain(1)
        }])),
        Some((0, PointerError::SearchSource))
    );
    assert_eq!(
        refused(try_load(vec![PointerDef {
            search: true,
            ..plain(0)
        }])),
        Some((0, PointerError::SearchSource))
    );
    let far = HoverRect {
        x: skin_model::MAX_COORD + 1,
        y: 0,
        w: 1,
        h: 1,
    };
    assert_eq!(
        refused(try_load(vec![PointerDef {
            hover: Some(far),
            ..plain(0)
        }])),
        Some((0, PointerError::Coordinates))
    );
    // Hover on any element, and an action on an image, load.
    let near = HoverRect {
        x: -5,
        y: 5,
        w: 10,
        h: 10,
    };
    assert!(
        try_load(vec![
            PointerDef {
                hover: Some(near),
                ..plain(1)
            },
            action(0, 12, ClickType::TopBottom),
        ])
        .is_ok()
    );
    // A clickable slot past the placements; a repeated one loads.
    let with = |clickable: Vec<u8>| {
        let mut def: SongListDef = list(3, 0);
        def.clickable = clickable;
        load(
            vec![list_element()],
            WidgetDefs::default(),
            SelectionDefs {
                song_list: Some(def),
                ..SelectionDefs::default()
            },
        )
    };
    assert!(matches!(
        with(vec![0, 3]),
        Err(LoadError::SongList(SongListError::Clickable { slot: 3 }))
    ));
    assert!(with(vec![2, 2, 0]).is_ok());
}

/// An element's action and hover rect are placed on its region, its
/// destination before a stretch fitted its image in it
/// (`skin/SkinObject.java:591-607, 623-643, 671-700`): a 2:1 image drawn
/// 100×50 in the middle of a 100×100 destination takes a press anywhere
/// in the destination, and its hover rect runs from the destination's
/// corner.
#[test]
fn an_action_and_a_hover_rect_are_placed_on_the_region() {
    let v = view(vec![chart("a")], Some(0));
    // The drawn rect, stretched: [100, 125, 100, 50].
    let fx = built(
        vec![element(Source::Image(5), [100, 125, 100, 50])],
        SelectionDefs {
            pointer: vec![PointerDef {
                region: Some(rect([100, 100, 100, 100])),
                ..action(0, 12, ClickType::TopBottom)
            }],
            ..SelectionDefs::default()
        },
        &v,
    );
    assert_eq!(fx.batch.quads().len(), 1);
    // Above and below the drawn rect, in the destination: the halves of
    // the destination, not of the drawn rect.
    assert_eq!(
        hit(&fx, [150.0, 110.0], Button::Left),
        Some(PointerHit::Action { event: 12, arg: 1 })
    );
    assert_eq!(
        hit(&fx, [150.0, 140.0], Button::Left),
        Some(PointerHit::Action { event: 12, arg: 1 })
    );
    assert_eq!(
        hit(&fx, [150.0, 190.0], Button::Left),
        Some(PointerHit::Action { event: 12, arg: -1 })
    );
    assert_eq!(hit(&fx, [150.0, 201.0], Button::Left), None);
    // A hover rect from the region's corner: a right-aligned line drawn at
    // x 500 in a destination anchored at x 900.
    let mut fx = built(
        vec![element(Source::Image(5), [500, 300, 400, 20])],
        SelectionDefs {
            pointer: vec![PointerDef {
                element: 0,
                action: None,
                hover: Some(HoverRect {
                    x: 0,
                    y: 0,
                    w: 400,
                    h: 20,
                }),
                search: false,
                region: Some(rect([900, 300, 400, 20])),
            }],
            ..SelectionDefs::default()
        },
        &v,
    );
    fx.pointer = Some([1_000.0, 310.0]);
    fx.frame(0);
    assert_eq!(fx.batch.quads().len(), 1);
    fx.pointer = Some([600.0, 310.0]);
    fx.frame(0);
    assert!(fx.batch.quads().is_empty());
}

/// A search text's input box is its region moved left by half or all of
/// its width as it is centred or right-aligned
/// (`skin/SkinText.java:162-169`), its full height whatever its line's.
#[test]
fn a_search_box_is_its_region_moved_by_its_alignment() {
    for (align, left) in [
        (skin_model::Align::Center, 700_i32),
        (skin_model::Align::Right, 500_i32),
    ] {
        // The line's box as an imported fixed-size font keys it: moved by
        // the alignment, 10 tall in a 20-tall destination.
        let skin = load(
            vec![element(Source::Text(0), [left, 100, 400, 10])],
            WidgetDefs {
                texts: vec![TextDef {
                    value: TextValue::Selection(SelectionText::Query),
                    font: skin_model::FontRef::Default,
                    align,
                    overflow: skin_model::Overflow::Spill,
                }],
                ..WidgetDefs::default()
            },
            SelectionDefs {
                pointer: vec![PointerDef {
                    element: 0,
                    action: None,
                    hover: None,
                    search: true,
                    region: Some(rect([900, 100, 400, 20])),
                }],
                ..SelectionDefs::default()
            },
        )
        .unwrap();
        let mut fx = Fx::new(skin);
        fx.view(&view(Vec::new(), None));
        fx.frame(0);
        let x = f32::from(i16::try_from(left).unwrap());
        let search = Some(PointerHit::Search { index: 0 });
        for (p, want) in [
            ([x, 100.0], search),
            ([x + 400.0, 120.0], search),
            ([x + 200.0, 118.0], search),
            ([x - 1.0, 110.0], None),
            ([x + 401.0, 110.0], None),
            ([x + 200.0, 121.0], None),
        ] {
            assert_eq!(hit(&fx, p, Button::Left), want, "{align:?} {p:?}");
        }
    }
}

/// Whether a press lies outside every search box is read apart from what
/// it hits (`skin/Skin.java:394-397`): an action drawn over the box takes
/// a press inside it, which is still inside; a press off the box is
/// outside whatever it hits.
#[test]
fn a_press_says_whether_it_is_outside_the_search_box() {
    let skin = load(
        vec![
            element(Source::Text(0), [100, 100, 300, 20]),
            element(Source::Image(5), [150, 95, 50, 30]),
        ],
        WidgetDefs {
            texts: vec![TextDef {
                value: TextValue::Selection(SelectionText::Query),
                font: skin_model::FontRef::Default,
                align: skin_model::Align::Left,
                overflow: skin_model::Overflow::Spill,
            }],
            ..WidgetDefs::default()
        },
        SelectionDefs {
            pointer: vec![
                PointerDef {
                    element: 0,
                    action: None,
                    hover: None,
                    search: true,
                    region: None,
                },
                action(1, 12, ClickType::Plus),
            ],
            ..SelectionDefs::default()
        },
    )
    .unwrap();
    let mut fx = Fx::new(skin);
    fx.view(&view(Vec::new(), None));
    fx.frame(0);
    let press = |p| fx.skin.hit_at(&fx.sel, p, fx.target, Button::Left);
    let order = Some(PointerHit::Action { event: 12, arg: 1 });
    assert_eq!(
        press([160.0, 110.0]),
        PressHit {
            hit: order,
            inside_search: SearchBoxes(1)
        }
    );
    assert_eq!(
        press([120.0, 110.0]),
        PressHit {
            hit: Some(PointerHit::Search { index: 0 }),
            inside_search: SearchBoxes(1)
        }
    );
    // On the action, below the box.
    assert_eq!(
        press([160.0, 124.0]),
        PressHit {
            hit: order,
            inside_search: SearchBoxes::NONE
        }
    );
    assert_eq!(
        press([500.0, 110.0]),
        PressHit {
            hit: None,
            inside_search: SearchBoxes::NONE
        }
    );
}

/// Query text over `r`, a search text.
fn query_text() -> TextDef {
    TextDef {
        value: TextValue::Selection(SelectionText::Query),
        font: skin_model::FontRef::Default,
        align: skin_model::Align::Left,
        overflow: skin_model::Overflow::Spill,
    }
}

/// A search entry for element `element`.
fn search(element: u32) -> PointerDef {
    PointerDef {
        element,
        action: None,
        hover: None,
        search: true,
        region: None,
    }
}

/// Each press says which search texts' input boxes, as last sampled, hold
/// it, by their order. A box its condition hides since, or its track's
/// end, takes no press but keeps its last bounds: a press there is still
/// inside it, one off it is not (`skin/SkinObject.java:348-356, 591-602`,
/// `skin/SkinTextInput.java:77-81`).
#[test]
fn a_search_box_keeps_its_last_bounds_while_hidden() {
    let mut by_condition = element(Source::Text(0), [100, 100, 300, 20]);
    by_condition.visible = Some(boolean("cursor.chart"));
    let mut by_time = element(Source::Text(1), [100, 300, 300, 20]);
    by_time.track = selection_support::keys_ending(
        &[(0, [100, 300, 300, 20]), (1_000, [100, 300, 300, 20])],
        skin_model::End::Hide,
    );
    let skin = load(
        vec![by_condition, by_time],
        WidgetDefs {
            texts: vec![query_text(), query_text()],
            ..WidgetDefs::default()
        },
        SelectionDefs {
            pointer: vec![search(0), search(1)],
            ..SelectionDefs::default()
        },
    )
    .unwrap();
    let mut fx = Fx::new(skin);
    let press = |fx: &Fx, p| fx.skin.hit_at(&fx.sel, p, fx.target, Button::Left);
    // Both drawn: each takes a press in its box, inside it alone.
    fx.view(&numbered(vec![chart("a")]));
    fx.frame(0);
    assert_eq!(
        press(&fx, [120.0, 110.0]),
        PressHit {
            hit: Some(PointerHit::Search { index: 0 }),
            inside_search: SearchBoxes(0b01),
        }
    );
    assert_eq!(
        press(&fx, [120.0, 310.0]),
        PressHit {
            hit: Some(PointerHit::Search { index: 1 }),
            inside_search: SearchBoxes(0b10),
        }
    );
    // The first hidden by its condition, the second past its track's end.
    fx.view(&view(Vec::new(), None));
    fx.frame(2_000);
    assert_eq!(
        press(&fx, [120.0, 110.0]),
        PressHit {
            hit: None,
            inside_search: SearchBoxes(0b01),
        },
        "inside the box its condition hid"
    );
    assert_eq!(
        press(&fx, [120.0, 310.0]),
        PressHit {
            hit: None,
            inside_search: SearchBoxes(0b10),
        },
        "inside the box its track's end hid"
    );
    assert_eq!(press(&fx, [500.0, 110.0]), PressHit::NONE);
}

/// The load refuses an image choice entry keyed otherwise than its
/// element's track on an element with a pointer entry (its region is its
/// own track whichever entry draws), and more search texts than
/// [`skin_model::MAX_SEARCH_BOXES`].
#[test]
fn load_refuses_an_unkeyed_choice_and_too_many_search_texts() {
    let choice = |track| skin_model::ImageChoiceDef {
        value: selection_support::int("0"),
        choices: vec![Some(skin_model::ImageChoicePart {
            span: skin_model::ImageSpan {
                image: 5,
                first: 0,
                frames: 1,
            },
            cycle: None,
            track,
            native: None,
        })],
        past_table: skin_model::PastTable::Hide,
    };
    let with = |track| {
        load(
            vec![element(Source::ImageChoice(0), [0, 0, 10, 10])],
            WidgetDefs {
                image_choices: vec![choice(track)],
                ..WidgetDefs::default()
            },
            SelectionDefs {
                pointer: vec![action(0, 12, ClickType::Plus)],
                ..SelectionDefs::default()
            },
        )
    };
    match with(Some(selection_support::keys(&[
        (0, [0, 0, 20, 10]),
        (100, [0, 0, 20, 10]),
    ]))) {
        Err(LoadError::Pointer { entry: 0, error }) => {
            assert_eq!(error, PointerError::RegionTiming);
        }
        other => panic!("{:?}", other.err()),
    }
    assert!(with(Some(rect([0, 0, 20, 10]))).is_ok());
    // Search texts are text elements: one past the bound is refused as
    // that, so every search text is a bit of the answer.
    let n = skin_model::MAX_SEARCH_BOXES + 1;
    let texts = load(
        (0..n)
            .map(|i| element(Source::Text(u32::try_from(i).unwrap()), [0, 0, 10, 10]))
            .collect(),
        WidgetDefs {
            texts: (0..n).map(|_| query_text()).collect(),
            ..WidgetDefs::default()
        },
        SelectionDefs {
            pointer: (0..n).map(|i| search(u32::try_from(i).unwrap())).collect(),
            ..SelectionDefs::default()
        },
    );
    assert!(
        matches!(texts, Err(LoadError::TooManyTexts { n: 65 })),
        "{:?}",
        texts.err()
    );
}

/// The load refuses a region keyed otherwise than its element's track, or
/// one past the coordinate range.
#[test]
fn load_refuses_a_misplaced_region() {
    let try_load = |region| {
        load(
            vec![element(Source::Image(5), [0, 0, 10, 10])],
            WidgetDefs::default(),
            SelectionDefs {
                pointer: vec![PointerDef {
                    region: Some(region),
                    ..action(0, 12, ClickType::Plus)
                }],
                ..SelectionDefs::default()
            },
        )
    };
    let refused = |r: Result<_, LoadError>| match r {
        Err(LoadError::Pointer { entry, error }) => Some((entry, error)),
        Err(e) => panic!("{e}"),
        Ok(_) => None,
    };
    assert_eq!(
        refused(try_load(selection_support::keys(&[
            (0, [0, 0, 20, 20]),
            (100, [0, 0, 20, 20])
        ]))),
        Some((0, PointerError::RegionTiming))
    );
    assert_eq!(
        refused(try_load(rect([skin_model::MAX_COORD + 1, 0, 20, 20]))),
        Some((0, PointerError::Coordinates))
    );
    assert!(try_load(rect([0, 0, 20, 20])).is_ok());
}
