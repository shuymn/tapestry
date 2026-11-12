//! Pointer input on a selection skin (docs/stage3.md S10-09e): what a press
//! hits among the targets the last build drew, topmost first, a song list
//! trying its slots in its clickable order, the source's click argument by
//! button and click type, and an element drawn only while the pointer is
//! inside its hover rect.

mod selection_support;

use selection_support::{
    Fx, boolean, chart, element, folder, list, list_element, load, placement, ratio, view,
};
use skin_model::{
    ActionDef, Button, ClickType, ElementDef, HoverRect, LoadError, PointerDef, PointerError,
    PointerHit, RatioBarDef, RatioForm, RatioSource, RowKind, RowRef, RowRefKind, RowView,
    SelectionDefs, SelectionText, SliderCommand, SliderDirection, SongListDef, SongListError,
    Source, TextDef, TextValue, WidgetDefs,
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
    fx.skin.hit_at(&fx.sel, p, fx.target, button)
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
            None
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
        assert_eq!(hit(&fx, [100.0, 100.0], button), Some(PointerHit::Search));
        assert_eq!(hit(&fx, [300.0, 120.0], button), Some(PointerHit::Search));
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
