//! The selection skin (docs/stage3.md R6, S10-09d): the `song list` over
//! the view's rows around the cursor, its parts at their placements'
//! anchors, the `ratio bar`'s exact ratios and its slider's commands, the
//! fields, strings and timers a view and a frame give, and the checks that
//! keep each kind's widgets on its own screen.

mod selection_support;

use bms_model::{ChartBuilder, TimeUs};
use selection_support::{
    Fx, all, boolean, chart, digits, element, features, folder, int, keys, labels, labels_of,
    lamps, levels, list, list_element, load, part, ratio, rect, row_text, skin_def, summary, view,
};
use skin_model::{
    Attempt, BuildError, FrameUv, GaugeLook, ImagePartDef, LampKey, LoadError, LoadState,
    LoadingFrame, NumberDef, OwnLamp, PlayContext, PrepareError, Ratio, RatioBarDef, RatioForm,
    RatioSource, RowLabel, RowScore, ScreenKind, SelectionDefs, SelectionError, SelectionPlace,
    SelectionText, Side, Skin, SliderCommand, SliderDirection, SongListError, Source, TextDef,
    TextImage, TextTable, TextValue, TimerRef, WidgetDefs, selection_timer,
};

/// A view of rows `a`, `b` and `c` with the cursor on `cursor`.
fn three(cursor: usize) -> skin_model::SelectionView {
    view(vec![chart("a"), chart("b"), chart("c")], Some(cursor))
}

fn bars(fx: &Fx) -> Vec<(u32, [f32; 4])> {
    fx.drawn()
        .into_iter()
        .filter_map(|(f, r)| f.map(|(i, _)| (i, r)))
        .collect()
}

fn list_skin(def: skin_model::SongListDef) -> Fx {
    Fx::new(
        load(
            vec![list_element()],
            WidgetDefs::default(),
            SelectionDefs {
                song_list: Some(def),
                ..SelectionDefs::default()
            },
        )
        .unwrap(),
    )
}

/// Slot `i` shows row `cursor + i − center`, the selected placement at the
/// center slot and the unselected elsewhere; a slot past the list's ends
/// draws nothing: the list never wraps (O1).
#[test]
fn slots_show_the_rows_around_the_cursor_without_wrapping() {
    let mut fx = list_skin(list(7, 3));
    let at = |y: f32| [10.0, y, 200.0, 50.0];
    for (cursor, want) in [
        (0, vec![(1, at(300.0)), (0, at(400.0)), (0, at(500.0))]),
        (1, vec![(0, at(200.0)), (1, at(300.0)), (0, at(400.0))]),
        (2, vec![(0, at(100.0)), (0, at(200.0)), (1, at(300.0))]),
    ] {
        fx.view(&three(cursor));
        fx.frame(0);
        assert_eq!(bars(&fx), want, "cursor {cursor}");
    }
    // An empty list draws no slot.
    fx.view(&view(Vec::new(), None));
    fx.frame(0);
    assert!(fx.drawn().is_empty());
}

/// A slot whose placement does not draw, hidden by its visibility or its
/// timer off, draws none of its parts (`select/BarRenderer.java:131`).
#[test]
fn a_slot_without_its_placement_draws_no_part() {
    let mut def = list(3, 1);
    def.lamp = lamps([0, -10, 10, 10]);
    def.placements[0][0].part.visible = Some(boolean("cursor.folder"));
    def.placements[2][0].part.timer = TimerRef::Builtin(selection_timer::CHANGE);
    let mut fx = list_skin(def);
    fx.view(&three(1));
    fx.frame(0);
    // Only the center slot: its bar and its lamp (unplayed).
    assert_eq!(fx.images(), [1, 2]);
}

/// Each part sits at its offset from its placement's sampled bottom-left
/// corner, the placement's height read this frame: a part's top is
/// `bar_h − part_y − part_h` of its y-up values below the bar's top.
#[test]
fn parts_hang_from_their_placements_bottom_left_corner() {
    let mut def = list(2, 0);
    // The center slot grows from 50 to 150 tall over 100 ms; slot 1 is 80
    // tall.
    def.placements[0][1].part.track = keys(&[(0, [10, 300, 200, 50]), (100, [10, 300, 200, 150])]);
    def.placements[1][0].part.track = rect([20, 500, 200, 80]);
    // A lamp 30×10 at x 5, its bottom 10 above the bar's bottom (y-up
    // y = 10): its keys hold `−(10 + 10)`.
    def.lamp = lamps([5, -20, 30, 10]);
    let mut fx = list_skin(def);
    fx.view(&view(vec![chart("a"), chart("b")], Some(0)));
    for (ms, center_h) in [(0, 50.0), (50, 100.0), (100, 150.0)] {
        fx.frame(ms);
        let lamps: Vec<[f32; 4]> = fx
            .drawn()
            .into_iter()
            .filter(|(f, _)| f.is_some_and(|(i, _)| i == 2))
            .map(|(_, r)| r)
            .collect();
        assert_eq!(
            lamps,
            [
                [15.0, 300.0 + center_h - 20.0, 30.0, 10.0],
                [25.0, 500.0 + 80.0 - 20.0, 30.0, 10.0],
            ],
            "at {ms} ms"
        );
    }
}

/// The passes draw every bar, then the row texts, the lamps, the levels and
/// the labels, each over every slot.
#[test]
fn parts_draw_pass_by_pass() {
    let mut def = list(2, 0);
    def.lamp = lamps([0, -10, 10, 10]);
    def.level = levels([0, -20, 10, 10], 1);
    def.label = labels([0, -30, 10, 10]);
    def.text = [Some(row_text([0, -40, 10, 10])), None, None];
    let mut fx = list_skin(def);
    let mut a = chart("a");
    a.level_value = 3;
    a.random = true;
    let mut b = chart("b");
    b.level_value = 4;
    b.features = features(true, false);
    fx.view(&view(vec![a, b], Some(0)));
    let texts = TextTable::new(vec![None; fx.skin.text_slots()]);
    fx.skin
        .attach_selection_text(&mut fx.sel, texts, TextTable::new(vec![line(); 2]))
        .unwrap();
    fx.frame(0);
    let digits = selection_support::DIGITS;
    // A text line is no sheet image: it shows as `TEXT`.
    const TEXT: u32 = u32::MAX;
    let order: Vec<u32> = fx
        .drawn()
        .into_iter()
        .map(|(f, _)| f.map_or(TEXT, |(i, _)| i))
        .collect();
    assert_eq!(
        order,
        [1, 0, TEXT, TEXT, 2, 2, digits, digits, 12, 10],
        "bars, texts, lamps, levels, then labels"
    );
}

/// A rasterized line of one em.
fn line() -> Option<TextImage> {
    TextImage::new(
        FrameUv {
            uv: [0.0, 0.0, 0.5, 0.5],
            layer: 0,
        },
        [0.0, 0.0, 1.0, 1.0],
        1.0,
    )
}

/// A slot whose placement's track has ended and hides draws nothing of its
/// own.
#[test]
fn a_placement_past_its_hiding_end_draws_no_part() {
    let mut def = list(2, 0);
    def.lamp = lamps([0, -10, 10, 10]);
    def.placements[1][0].part.track = selection_support::keys_ending(
        &[(0, [10, 100, 200, 50]), (100, [10, 100, 200, 50])],
        skin_model::End::Hide,
    );
    let mut fx = list_skin(def);
    fx.view(&view(vec![chart("a"), chart("b")], Some(0)));
    fx.frame(50);
    assert_eq!(fx.images(), [1, 0, 2, 2]);
    fx.frame(150);
    assert_eq!(fx.images(), [1, 2]);
}

/// Placement visibilities on every slot and destination are charged once
/// per arm, as each slot evaluates one placement once a frame; at the
/// budget's edge the skin still loads and draws.
#[test]
fn placement_visibilities_are_charged_as_they_run() {
    let mut def = list(60, 0);
    let condition = (0..16)
        .map(|_| "cursor.chart")
        .collect::<Vec<_>>()
        .join(" && ");
    let program = boolean(&condition);
    let cost = program.op_count();
    for p in &mut def.placements {
        for arm in p.iter_mut() {
            arm.part.visible = Some(program.clone());
        }
    }
    let n = skin_model::MAX_SKIN_OPS / (120 * cost);
    let skin = load(
        (0..n).map(|_| list_element()).collect(),
        WidgetDefs::default(),
        SelectionDefs {
            song_list: Some(def),
            ..SelectionDefs::default()
        },
    )
    .unwrap();
    assert_eq!(skin.ops().total(), n * 120 * cost);
    let mut fx = Fx::new(skin);
    fx.view(&three(0));
    fx.frame(0);
    assert_eq!(fx.images().len(), 3 * n);
}

/// A frame evaluates each populated slot's placement visibility once,
/// however many parts hang from it.
#[cfg(feature = "dev-tools")]
#[test]
fn a_frame_runs_each_slots_placement_once() {
    let mut def = list(60, 0);
    for p in &mut def.placements {
        for arm in p.iter_mut() {
            arm.part.visible = Some(boolean("cursor.chart && !cursor.folder"));
        }
    }
    def.lamp = lamps([0, -10, 10, 10]);
    def.label = labels([0, -20, 10, 10]);
    let mut fx = list_skin(def);
    fx.view(&three(0));
    let before = skin_model::evaluations();
    fx.frame(0);
    assert_eq!(skin_model::evaluations() - before, 3);
}

/// Several song list elements draw the same rows from one row text table.
#[test]
fn song_list_elements_share_one_row_text_table() {
    let mut def = list(2, 0);
    def.text = [Some(row_text([0, -40, 10, 10])), None, None];
    let mut fx = Fx::new(
        load(
            vec![list_element(), list_element()],
            WidgetDefs::default(),
            SelectionDefs {
                song_list: Some(def),
                ..SelectionDefs::default()
            },
        )
        .unwrap(),
    );
    assert_eq!(fx.skin.song_list_slots(), 2);
    fx.view(&view(vec![chart("a"), chart("b")], Some(0)));
    let req = fx
        .skin
        .selection_text_requests(&fx.sel, fx.target, &fx.layout);
    assert_eq!(req.rows.len(), 2);
    let texts = TextTable::new(vec![None; fx.skin.text_slots()]);
    fx.skin
        .attach_selection_text(&mut fx.sel, texts, TextTable::new(vec![line(); 2]))
        .unwrap();
    fx.frame(0);
    let lines = fx.drawn().into_iter().filter(|(f, _)| f.is_none()).count();
    assert_eq!(lines, 4);
}

/// Row text in a bitmap font is laid out per row slot from its row's string
/// and drawn glyph by glyph; each slot charges a field's glyphs to the
/// quad budget.
#[test]
fn bitmap_row_text_lays_out_each_row() {
    let (_dir, skin) = selection_support::bitmap_list(3, 0);
    // Per slot: the bar, a field's 256 glyphs, a lamp and the labels.
    assert_eq!(skin.quad_capacity(), 3 * (2 + 256 + 3));
    assert!(!skin.has_outline_text());
    let mut fx = Fx::new(skin);
    fx.view(&view(
        vec![chart("A"), chart("AB"), folder("B", 1)],
        Some(0),
    ));
    // A bitmap row asks for no raster line.
    let req = fx
        .skin
        .selection_text_requests(&fx.sel, fx.target, &fx.layout);
    assert!(req.rows.is_empty());
    fx.skin.attach_selection_bitmap_text(&mut fx.sel);
    fx.frame(0);
    let quads = fx.batch.quads();
    // Three bars, then the glyphs of "A" and "AB"; the folder row has no
    // style of its own, so it shows no text.
    assert_eq!(quads.len(), 3 + 1 + 2);
    let glyph_x: Vec<f32> = quads[3..].iter().map(|q| q.rect[0]).collect();
    assert_eq!(glyph_x, [10.0, 10.0, 15.0]);
    let glyph_y: Vec<f32> = quads[3..].iter().map(|q| q.rect[1]).collect();
    assert_eq!(glyph_y[0] + 100.0, glyph_y[1]);
}

/// Room for a text layer beside the sheet's pages.
const TEXT_LIMITS: skin_model::AtlasLimits = skin_model::AtlasLimits {
    max_side: 2_048,
    max_layers: 4,
    reserved_layers: 1,
};

/// A skin whose only outline text is its song list's row text reserves the
/// atlas's text layer and names its row text's font file.
#[test]
fn row_text_reserves_the_text_layer_and_names_its_font() {
    let mut def = list(1, 0);
    def.text = [Some(row_text([0, -20, 100, 10])), None, None];
    let skin = load(
        vec![list_element()],
        WidgetDefs::default(),
        SelectionDefs {
            song_list: Some(def.clone()),
            ..SelectionDefs::default()
        },
    )
    .unwrap();
    let atlas = skin
        .atlas(skin_model::Variant::Lo, TEXT_LIMITS, &|| false)
        .unwrap();
    assert!(atlas.text_layer().is_some());
    assert!(skin.outline_fonts().is_empty());
    let mut sd = skin_def(vec![list_element()], WidgetDefs::default());
    sd.sheet = selection_support::sheet_with_font();
    if let Some(t) = def.text[0].as_mut() {
        t.font = skin_model::FontRef::File(0);
    }
    let skin = Skin::new_selection(
        sd,
        SelectionDefs {
            song_list: Some(def),
            ..SelectionDefs::default()
        },
    )
    .unwrap();
    assert_eq!(skin.outline_fonts(), [0]);
    let atlas = skin
        .atlas(skin_model::Variant::Lo, TEXT_LIMITS, &|| false)
        .unwrap();
    assert!(atlas.text_layer().is_some());
}

/// A chart row draws its bar and text in the chart style, a folder row in
/// the folder style, the "all charts" row in its own; the styles' images and
/// text sit apart.
#[test]
fn each_row_kind_takes_its_style() {
    let mut def = list(3, 0);
    for p in &mut def.placements {
        for arm in p.iter_mut() {
            arm.images = [Some(5), Some(6), Some(7)];
        }
    }
    def.text = [
        Some(row_text([0, -40, 100, 20])),
        Some(row_text([100, -40, 100, 20])),
        Some(row_text([200, -40, 100, 20])),
    ];
    let mut fx = list_skin(def);
    fx.view(&view(vec![chart("a"), folder("f", 2), all(9)], Some(0)));
    let line = line();
    let texts = TextTable::new(vec![None; fx.skin.text_slots()]);
    fx.skin
        .attach_selection_text(&mut fx.sel, texts, TextTable::new(vec![line; 3]))
        .unwrap();
    fx.frame(0);
    let drawn = fx.drawn();
    let images: Vec<u32> = drawn
        .iter()
        .filter_map(|(f, _)| f.map(|(i, _)| i))
        .collect();
    assert_eq!(images, [5, 6, 7]);
    let text_x: Vec<f32> = drawn
        .iter()
        .filter(|(f, _)| f.is_none())
        .map(|(_, r)| r[0])
        .collect();
    assert_eq!(text_x, [10.0, 110.0, 210.0]);
    let req = fx
        .skin
        .selection_text_requests(&fx.sel, fx.target, &fx.layout);
    let rows: Vec<(u16, &str)> = req.rows.iter().map(|r| (r.slot, r.text.as_str())).collect();
    assert_eq!(rows, [(0, "a"), (1, "f"), (2, "ALL")]);
}

/// A row's lamp image follows its typed lamp, the gauge's look choosing
/// a cleared one; a hazard clear shows the full combo's.
#[test]
fn lamps_follow_the_typed_lamp() {
    let best = |lamp| RowScore::Best {
        plays: 1,
        ex: 1,
        bp: 0,
        lamp,
    };
    let cleared = |look| {
        best(OwnLamp::Cleared {
            gauge: 0,
            rank: 0,
            look,
        })
    };
    for (score, key) in [
        (RowScore::Unplayed, LampKey::NoPlay),
        (RowScore::NoBest { plays: 2 }, LampKey::NoPlay),
        (best(OwnLamp::Failed), LampKey::Failed),
        (cleared(GaugeLook::AssistEasy), LampKey::AssistEasy),
        (cleared(GaugeLook::Easy), LampKey::Easy),
        (cleared(GaugeLook::Normal), LampKey::Normal),
        (cleared(GaugeLook::Hard), LampKey::Hard),
        (cleared(GaugeLook::ExHard), LampKey::ExHard),
        (cleared(GaugeLook::Hazard), LampKey::FullCombo),
        (best(OwnLamp::FullCombo), LampKey::FullCombo),
    ] {
        assert_eq!(LampKey::of(score), key, "{score:?}");
        let mut def = list(1, 0);
        def.lamp = lamps([0, -10, 10, 10]);
        let mut fx = list_skin(def);
        let mut row = chart("a");
        row.score = score;
        fx.view(&view(vec![row], Some(0)));
        fx.frame(0);
        assert_eq!(
            fx.images(),
            [1, 2 + u32::try_from(key.index()).unwrap()],
            "{score:?}"
        );
    }
    // A missing style draws nothing.
    let mut def = list(1, 0);
    def.lamp = lamps([0, -10, 10, 10]);
    def.lamp[LampKey::Failed.index()] = None;
    let mut fx = list_skin(def);
    let mut row = chart("a");
    row.score = best(OwnLamp::Failed);
    fx.view(&view(vec![row], Some(0)));
    fx.frame(0);
    assert_eq!(fx.images(), [1]);
}

/// A chart row's level draws in its difficulty's style, the style without
/// one taking a chart without `#DIFFICULTY`; a folder row has no level.
#[test]
fn levels_draw_in_their_difficultys_style() {
    let mut def = list(4, 0);
    def.level = levels([0, -10, 10, 10], 2);
    let mut fx = list_skin(def);
    let level = |v, d| {
        let mut r = chart("x");
        r.level_value = v;
        r.difficulty = d;
        r
    };
    fx.view(&view(
        vec![
            level(12, None),
            level(7, Some(1)),
            level(0, Some(5)),
            folder("f", 1),
        ],
        Some(0),
    ));
    fx.frame(0);
    let glyphs: Vec<(u16, f32)> = fx
        .drawn()
        .into_iter()
        .filter_map(|(f, r)| match f {
            Some((i, g)) if i == selection_support::DIGITS => Some((g, r[0])),
            _ => None,
        })
        .collect();
    // Each cell is 10 wide from the style's x, the slot's x 10 added.
    assert_eq!(glyphs, [(1, 10.0), (2, 20.0), (7, 1_010.0), (0, 5_010.0)]);
}

/// Labels draw long notes, mines and random in that order, each from its
/// fact; without a summary only random can draw.
#[test]
fn labels_follow_the_row_features() {
    let mut def = list(4, 0);
    def.label = labels([0, -10, 10, 10]);
    let mut fx = list_skin(def);
    let row = |f, random| {
        let mut r = chart("x");
        r.features = f;
        r.random = random;
        r
    };
    fx.view(&view(
        vec![
            row(None, false),
            row(None, true),
            row(features(false, true), false),
            row(features(true, true), true),
        ],
        Some(0),
    ));
    fx.frame(0);
    let labels: Vec<u32> = fx.images().into_iter().filter(|&i| i >= 10).collect();
    let [long, mines, random] = RowLabel::ALL.map(|l| 10 + u32::try_from(l as usize).unwrap());
    assert_eq!(labels, [random, mines, long, mines, random]);
}

fn bar_skin(bars: Vec<RatioBarDef>) -> Fx {
    let elements = (0..bars.len())
        .map(|i| {
            element(
                Source::RatioBar(u32::try_from(i).unwrap()),
                [100, 100, 40, 20],
            )
        })
        .collect();
    Fx::new(
        load(
            elements,
            WidgetDefs::default(),
            SelectionDefs {
                song_list: None,
                ratio_bars: bars,
            },
        )
        .unwrap(),
    )
}

fn bar(form: RatioForm, source: RatioSource) -> RatioBarDef {
    RatioBarDef {
        image: 3,
        form,
        source,
    }
}

/// A bar cuts its image to exactly the ratio of its width from the left,
/// or of its height from the bottom, the uv cut alike; a slider moves by
/// the ratio of its range; with no ratio it draws nothing.
#[test]
fn ratio_bars_draw_the_exact_ratio() {
    let slider = |direction| RatioForm::Slider {
        direction,
        range: 300,
        changeable: false,
    };
    let mut fx = bar_skin(vec![
        bar(
            RatioForm::Bar { vertical: false },
            RatioSource::ListPosition,
        ),
        bar(RatioForm::Bar { vertical: true }, RatioSource::ListPosition),
        bar(slider(SliderDirection::Up), RatioSource::ListPosition),
        bar(slider(SliderDirection::Right), RatioSource::ListPosition),
        bar(slider(SliderDirection::Down), RatioSource::ListPosition),
        bar(slider(SliderDirection::Left), RatioSource::ListPosition),
    ]);
    let whole = fx.uvs.get(3).unwrap().uv;
    let [u0, v0, u1, v1] = whole;
    for (n, d) in [(0, 3), (1, 3), (3, 3)] {
        let mut v = view(Vec::new(), None);
        v.position = ratio(n, d);
        fx.view(&v);
        fx.frame(0);
        let f = n as f32 / d as f32;
        let quads: Vec<([f32; 4], [f32; 4])> =
            fx.batch.quads().iter().map(|q| (q.rect, q.uv)).collect();
        let r = 300.0 * f;
        assert_eq!(
            quads,
            [
                (
                    [100.0, 100.0, 40.0 * f, 20.0],
                    [u0, v0, u0 + (u1 - u0) * f, v1]
                ),
                (
                    [100.0, 100.0 + 20.0 * (1.0 - f), 40.0, 20.0 * f],
                    [u0, v1 - (v1 - v0) * f, u1, v1]
                ),
                ([100.0, 100.0 - r, 40.0, 20.0], whole),
                ([100.0 + r, 100.0, 40.0, 20.0], whole),
                ([100.0, 100.0 + r, 40.0, 20.0], whole),
                ([100.0 - r, 100.0, 40.0, 20.0], whole),
            ],
            "{n}/{d}"
        );
    }
    let mut v = view(Vec::new(), None);
    v.position = None;
    fx.view(&v);
    fx.frame(0);
    assert!(fx.batch.quads().is_empty());
}

/// A press on a changeable slider's travel, as last built, gives the exact
/// ratio under it, snapped at the ends; off the travel, on a slider that did
/// not draw, one that is not changeable, or after a resize not yet drawn, it
/// gives nothing. The state does not change.
#[test]
fn a_slider_press_gives_a_command_and_changes_nothing() {
    let slider = |direction, changeable| RatioForm::Slider {
        direction,
        range: 300,
        changeable,
    };
    let src = RatioSource::ListPosition;
    let cmd = |n| {
        Some(SliderCommand {
            source: src,
            value: Ratio::new(n, skin_model::SLIDER_STEPS).unwrap(),
        })
    };
    let third = skin_model::SLIDER_STEPS / 3;
    for (direction, start, along) in [
        // The rect at ratio 0 is [100, 100, 40, 20]; its bottom is 120.
        (SliderDirection::Up, [110.0, 120.0], [0.0, -1.0]),
        (SliderDirection::Down, [110.0, 120.0], [0.0, 1.0]),
        (SliderDirection::Right, [100.0, 110.0], [1.0, 0.0]),
        (SliderDirection::Left, [100.0, 110.0], [-1.0, 0.0]),
    ] {
        let mut fx = bar_skin(vec![bar(slider(direction, true), src)]);
        let mut v = view(Vec::new(), None);
        v.position = ratio(1, 2);
        fx.view(&v);
        fx.frame(0);
        let at = |d: f32| [start[0] + along[0] * d, start[1] + along[1] * d];
        let before = format!("{:?}", fx.sel);
        assert_eq!(
            fx.skin.slider_at(&fx.sel, at(0.5), fx.target),
            cmd(0),
            "{direction:?}"
        );
        assert_eq!(
            fx.skin.slider_at(&fx.sel, at(100.0), fx.target),
            cmd(third),
            "{direction:?}"
        );
        assert_eq!(
            fx.skin.slider_at(&fx.sel, at(299.5), fx.target),
            cmd(skin_model::SLIDER_STEPS),
            "{direction:?}"
        );
        assert_eq!(
            fx.skin.slider_at(&fx.sel, at(301.0), fx.target),
            None,
            "{direction:?}"
        );
        assert_eq!(format!("{:?}", fx.sel), before);
        // A resize not yet drawn gives nothing; the next build does.
        assert_eq!(fx.skin.slider_at(&fx.sel, at(100.0), [1_280, 720]), None);
        // With no ratio the slider does not draw, so it takes no press.
        fx.view(&view(Vec::new(), None));
        fx.frame(0);
        assert_eq!(fx.skin.slider_at(&fx.sel, at(100.0), fx.target), None);
    }
    let mut fx = bar_skin(vec![bar(slider(SliderDirection::Right, false), src)]);
    let mut v = view(Vec::new(), None);
    v.position = ratio(1, 2);
    fx.view(&v);
    fx.frame(0);
    assert_eq!(fx.skin.slider_at(&fx.sel, [150.0, 110.0], fx.target), None);
    // A slider hidden by its visibility takes no press.
    let mut skin_fx = {
        let mut e = element(Source::RatioBar(0), [100, 100, 40, 20]);
        e.visible = Some(boolean("cursor.chart"));
        Fx::new(
            load(
                vec![e],
                WidgetDefs::default(),
                SelectionDefs {
                    song_list: None,
                    ratio_bars: vec![bar(slider(SliderDirection::Right, true), src)],
                },
            )
            .unwrap(),
        )
    };
    skin_fx.view(&v);
    skin_fx.frame(0);
    assert_eq!(
        skin_fx
            .skin
            .slider_at(&skin_fx.sel, [150.0, 110.0], skin_fx.target),
        None
    );
    // A slider whose track moves is pressed where the last build put it.
    let mut e = element(Source::RatioBar(0), [100, 100, 40, 20]);
    e.track = keys(&[(0, [100, 100, 40, 20]), (100, [500, 100, 40, 20])]);
    let mut fx = Fx::new(
        load(
            vec![e],
            WidgetDefs::default(),
            SelectionDefs {
                song_list: None,
                ratio_bars: vec![bar(slider(SliderDirection::Right, true), src)],
            },
        )
        .unwrap(),
    );
    fx.view(&v);
    fx.frame(0);
    assert_eq!(
        fx.skin.slider_at(&fx.sel, [200.0, 110.0], fx.target),
        cmd(third)
    );
    fx.frame(100);
    assert_eq!(fx.skin.slider_at(&fx.sel, [200.0, 110.0], fx.target), None);
    assert_eq!(
        fx.skin.slider_at(&fx.sel, [600.0, 110.0], fx.target),
        cmd(third)
    );
}

/// The number shows its field from the view.
fn number_skin(value: &str, visible: &str) -> Fx {
    let mut e = element(Source::Number(0), [0, 0, 10, 10]);
    e.visible = Some(boolean(visible));
    Fx::new(
        load(
            vec![e],
            WidgetDefs {
                numbers: vec![NumberDef {
                    digits: digits(6),
                    value: int(value),
                }],
                ..WidgetDefs::default()
            },
            SelectionDefs::default(),
        )
        .unwrap(),
    )
}

fn shown(fx: &Fx) -> Vec<u16> {
    fx.drawn()
        .into_iter()
        .filter_map(|(f, _)| f.map(|(_, g)| g))
        .collect()
}

/// The rate, rank and next rank come from the best's EX and the largest
/// the summary allows, in exact integers, and none of them, nor the EX-rate
/// bar, draws without both or with a largest of 0.
#[test]
fn rates_and_ranks_need_a_positive_largest() {
    let best = |ex| RowScore::Best {
        plays: 1,
        ex,
        bp: 0,
        lamp: OwnLamp::Failed,
    };
    let mut fx_rate = number_skin("selected.rate_percent", "selected.rate_valid");
    let mut fx_rank = number_skin("selected.rank", "selected.rate_valid");
    let mut fx_next = number_skin("selected.next_rank", "selected.rate_valid");
    let mut fx_max = number_skin("selected.max_ex", "selected.max_ex_valid");
    let mut fx_bar = bar_skin(vec![bar(
        RatioForm::Bar { vertical: false },
        RatioSource::ExRate,
    )]);
    let at = |ex: u32, max: Option<u64>| {
        let mut row = chart("a");
        row.score = best(ex);
        let mut v = view(vec![row], Some(0));
        v.selected.as_mut().unwrap().max_ex = max;
        v
    };
    for fx in [
        &mut fx_rate,
        &mut fx_rank,
        &mut fx_next,
        &mut fx_max,
        &mut fx_bar,
    ] {
        // 27 × 12 = 6 × 54: rank 1 exactly; 11 is just below it.
        fx.view(&at(12, Some(54)));
        fx.frame(0);
    }
    assert_eq!(shown(&fx_rate), [2, 2]);
    assert_eq!(shown(&fx_rank), [1]);
    assert_eq!(shown(&fx_next), [6]);
    assert_eq!(shown(&fx_max), [5, 4]);
    assert_eq!(fx_bar.batch.quads().len(), 1);
    fx_rank.view(&at(11, Some(54)));
    fx_rank.frame(0);
    assert_eq!(shown(&fx_rank), [0]);
    fx_next.view(&at(54, Some(54)));
    fx_next.frame(0);
    assert_eq!(shown(&fx_next), [0]);
    for (ex, max) in [(0, Some(0)), (12, None)] {
        for fx in [&mut fx_rate, &mut fx_rank, &mut fx_next, &mut fx_bar] {
            fx.view(&at(ex, max));
            fx.frame(0);
            assert!(fx.batch.quads().is_empty(), "{ex} / {max:?}");
        }
    }
    // The largest itself still shows when it is 0.
    fx_max.view(&at(0, Some(0)));
    fx_max.frame(0);
    assert_eq!(shown(&fx_max), [0]);
}

/// The chart facts come from the summary and the labels: notes, tempos in
/// whole BPM, whether they change, the length and the level.
#[test]
fn chart_facts_come_from_the_summary() {
    let mut v = three(0);
    {
        let c = v.selected.as_mut().unwrap();
        c.summary = Some(summary(300, 20, 5, 125, 120, 181));
        c.labels.level_value = 12;
    }
    for (value, want) in [
        ("selected.notes", vec![3, 2, 0]),
        ("selected.bpm_max", vec![1, 8, 1]),
        ("selected.bpm_min", vec![1, 2, 0]),
        ("selected.length_min", vec![2]),
        ("selected.length_sec", vec![5]),
        ("selected.level", vec![1, 2]),
    ] {
        let mut fx = number_skin(value, "selected.summary");
        fx.view(&v);
        fx.frame(0);
        assert_eq!(shown(&fx), want, "{value}");
    }
    let mut fx = number_skin("1", "selected.bpm_change");
    fx.view(&v);
    fx.frame(0);
    assert_eq!(shown(&fx), [1]);
    v.selected.as_mut().unwrap().summary = Some(summary(1, 0, 0, 1, 150, 150));
    fx.view(&v);
    fx.frame(0);
    assert!(shown(&fx).is_empty());
    // Without a summary nothing of it shows.
    v.selected.as_mut().unwrap().summary = None;
    let mut fx = number_skin("selected.notes", "selected.summary");
    fx.view(&v);
    fx.frame(0);
    assert!(shown(&fx).is_empty());
}

/// The source never clamps a rate: an EX above the largest the summary
/// allows shows its rate above 100 %, while its bar draws its whole box
/// (`skin/SkinGraph.java:86-106`, `ScoreDataProperty.java:94-96`).
#[test]
fn a_rate_above_the_largest_is_not_clamped() {
    let mut row = chart("a");
    row.score = RowScore::Best {
        plays: 1,
        ex: 81,
        bp: 0,
        lamp: OwnLamp::Failed,
    };
    let mut v = view(vec![row], Some(0));
    v.selected.as_mut().unwrap().max_ex = Some(54);
    let mut fx = bar_skin(vec![bar(
        RatioForm::Bar { vertical: false },
        RatioSource::ExRate,
    )]);
    fx.view(&v);
    fx.frame(0);
    // The bar draws its whole box once, the frame's whole uv: a deviation
    // from the source, which extends its region past the image.
    let whole = fx.uvs.get(3).unwrap().uv;
    let quads: Vec<([f32; 4], [f32; 4])> =
        fx.batch.quads().iter().map(|q| (q.rect, q.uv)).collect();
    assert_eq!(quads, [([100.0, 100.0, 40.0, 20.0], whole)]);
    let mut fx = bar_skin(vec![bar(
        RatioForm::Bar { vertical: true },
        RatioSource::ExRate,
    )]);
    fx.view(&v);
    fx.frame(0);
    let quads: Vec<([f32; 4], [f32; 4])> =
        fx.batch.quads().iter().map(|q| (q.rect, q.uv)).collect();
    assert_eq!(quads, [([100.0, 100.0, 40.0, 20.0], whole)]);
    let mut fx = number_skin("selected.rate_percent", "selected.rate_valid");
    fx.view(&v);
    fx.frame(0);
    assert_eq!(shown(&fx), [1, 5, 0]);
}

/// A rate past the field's range saturates at its largest, never wrapping
/// or reading 0.
#[test]
fn a_huge_rate_saturates() {
    let rate = |ex: u32| {
        let mut row = chart("a");
        row.score = RowScore::Best {
            plays: 1,
            ex,
            bp: 0,
            lamp: OwnLamp::Failed,
        };
        let mut v = view(vec![row], Some(0));
        v.selected.as_mut().unwrap().max_ex = Some(2);
        let mut e = element(Source::Number(0), [0, 0, 10, 10]);
        e.visible = Some(boolean("selected.rate_valid"));
        let mut fx = Fx::new(
            load(
                vec![e],
                WidgetDefs {
                    numbers: vec![NumberDef {
                        digits: digits(10),
                        value: int("selected.rate_percent"),
                    }],
                    ..WidgetDefs::default()
                },
                SelectionDefs::default(),
            )
            .unwrap(),
        );
        fx.view(&v);
        fx.frame(0);
        shown(&fx)
    };
    // 100 · 42 949 672 / 2 is 2 147 483 600, within the range.
    assert_eq!(rate(42_949_672), [2, 1, 4, 7, 4, 8, 3, 6, 0, 0]);
    // 100 · 42 949 673 / 2 is past it: the largest, 2 147 483 646.
    assert_eq!(rate(42_949_673), [2, 1, 4, 7, 4, 8, 3, 6, 4, 6]);
}

/// Of overlapping sliders, the one drawn last takes the press.
#[test]
fn the_topmost_slider_takes_the_press() {
    let slider = RatioForm::Slider {
        direction: SliderDirection::Right,
        range: 300,
        changeable: true,
    };
    let mut fx = bar_skin(vec![
        bar(slider, RatioSource::ListPosition),
        bar(slider, RatioSource::ExRate),
    ]);
    let mut row = chart("a");
    row.score = RowScore::Best {
        plays: 1,
        ex: 27,
        bp: 0,
        lamp: OwnLamp::Failed,
    };
    let mut v = view(vec![row], Some(0));
    v.selected.as_mut().unwrap().max_ex = Some(54);
    v.position = ratio(1, 2);
    fx.view(&v);
    fx.frame(0);
    let got = fx
        .skin
        .slider_at(&fx.sel, [200.0, 110.0], fx.target)
        .unwrap();
    assert_eq!(got.source, RatioSource::ExRate);
}

/// A selection state belongs to the skin that made it: another skin with
/// the same tables refuses it, draws nothing of it and reads none of its
/// sliders; a clone of its skin is the same skin.
#[test]
fn a_state_belongs_to_its_skin() {
    let slider = RatioForm::Slider {
        direction: SliderDirection::Right,
        range: 300,
        changeable: true,
    };
    let make = || bar_skin(vec![bar(slider, RatioSource::ListPosition)]);
    let mut a = make();
    let b = make();
    let mut v = view(Vec::new(), None);
    v.position = ratio(1, 2);
    a.view(&v);
    a.frame(0);
    assert!(a.skin.slider_at(&a.sel, [200.0, 110.0], a.target).is_some());
    assert_eq!(b.skin.slider_at(&a.sel, [200.0, 110.0], a.target), None);
    assert_eq!(
        b.skin.update_selection(&mut a.sel, &v, &labels_of()),
        Err(SelectionError::Kind)
    );
    assert_eq!(
        b.skin.attach_selection_text(
            &mut a.sel,
            TextTable::new(Vec::new()),
            TextTable::new(Vec::new())
        ),
        Err(skin_model::TextTableError::Foreign)
    );
    let layout = skin_model::Layout::AUTHOR;
    let frame = skin_model::SelectionFrame {
        now: TimeUs(0),
        target: a.target,
        layout: &layout,
        timers: a.timers,
    };
    let mut out = skin_model::QuadBatch::with_capacity(8, 8);
    b.skin
        .build_selection(&mut a.sel, &frame, &b.uvs, &mut out)
        .unwrap();
    assert!(out.quads().is_empty());
    // A clone keeps its skin's identity.
    let clone = a.skin.clone();
    assert!(clone.slider_at(&a.sel, [200.0, 110.0], a.target).is_some());
}

/// The cursor's row kind and a folder's count read as fields.
#[test]
fn the_cursor_row_reads_as_fields() {
    for (row, chart_on, folder_on, all_on, count) in [
        (chart("a"), 1, 0, 0, vec![0]),
        (folder("f", 34), 0, 1, 0, vec![3, 4]),
        (all(7), 0, 0, 1, vec![7]),
    ] {
        let v = view(vec![row], Some(0));
        for (field, on) in [
            ("cursor.chart", chart_on),
            ("cursor.folder", folder_on),
            ("cursor.all", all_on),
        ] {
            let mut fx = number_skin("1", field);
            fx.view(&v);
            fx.frame(0);
            assert_eq!(shown(&fx).len(), on, "{field}");
        }
        let mut fx = number_skin("cursor.charts", "true");
        fx.view(&v);
        fx.frame(0);
        assert_eq!(shown(&fx), count);
    }
}

fn text_skin(values: Vec<TextValue>) -> Fx {
    let elements = (0..values.len())
        .map(|i| element(Source::Text(u32::try_from(i).unwrap()), [0, 0, 100, 20]))
        .collect();
    let texts = values
        .into_iter()
        .map(|value| TextDef {
            value,
            font: skin_model::FontRef::Default,
            align: skin_model::Align::Left,
            overflow: skin_model::Overflow::Spill,
        })
        .collect();
    Fx::new(
        load(
            elements,
            WidgetDefs {
                texts,
                ..WidgetDefs::default()
            },
            SelectionDefs::default(),
        )
        .unwrap(),
    )
}

/// The text a selection skin shows: the chart's fields on a chart row, a
/// folder's name or the "all charts" label as its title, the query, and the
/// place's names each followed by `" > "`, from the player's labels and the
/// view, with no string of its own.
#[test]
fn texts_show_the_view_and_the_players_labels() {
    use skin_model::TextField::{FullArtist, FullTitle, Genre, Subtitle, Title};
    let fields = [Title, FullTitle, Subtitle, Genre, FullArtist];
    let mut values: Vec<TextValue> = fields.iter().map(|&f| TextValue::Field(f)).collect();
    values.push(TextValue::Selection(SelectionText::Query));
    values.push(TextValue::Selection(SelectionText::Place));
    let mut fx = text_skin(values);
    let shown = |fx: &Fx| -> Vec<(u16, String)> {
        fx.skin
            .selection_text_requests(&fx.sel, fx.target, &fx.layout)
            .texts
            .into_iter()
            .map(|r| (r.slot, r.text))
            .collect()
    };
    let s = |slot: u16, t: &str| (slot, t.to_owned());
    let mut row = chart("T");
    row.subtitle = "S".to_owned();
    let mut v = view(vec![row.clone(), folder("F", 1), all(3)], Some(0));
    {
        let c = v.selected.as_mut().unwrap();
        c.labels.subtitle = "S".to_owned();
        c.labels.artist = "A".to_owned();
        c.labels.subartist = "B".to_owned();
        c.labels.genre = "G".to_owned();
    }
    v.query = "q".to_owned();
    v.place = SelectionPlace::Folder(vec!["X".to_owned(), "Y".to_owned()]);
    fx.view(&v);
    assert_eq!(
        shown(&fx),
        [
            s(0, "T"),
            s(1, "T S"),
            s(2, "S"),
            s(3, "G"),
            s(4, "A B"),
            s(5, "q"),
            s(6, "X > Y > "),
        ]
    );
    // An empty subtitle: the full title is the title alone.
    let mut v2 = view(vec![chart("T")], Some(0));
    v2.place = SelectionPlace::Top;
    fx.view(&v2);
    assert_eq!(shown(&fx), [s(0, "T"), s(1, "T")]);
    // A folder row: its name; the "all charts" row: the label.
    let mut v3 = view(vec![chart("T"), folder("F", 1), all(3)], Some(1));
    v3.place = SelectionPlace::All;
    fx.view(&v3);
    assert_eq!(shown(&fx), [s(0, "F"), s(1, "F"), s(6, "ALL > ")]);
    v3.cursor = Some(2);
    v3.place = SelectionPlace::Results;
    v3.query = "q".to_owned();
    fx.view(&v3);
    assert_eq!(
        shown(&fx),
        [s(0, "ALL"), s(1, "ALL"), s(5, "q"), s(6, "FOUND > ")]
    );
    // Other labels give other text: none is the skin's own.
    let other = skin_model::SelectionLabels {
        all_charts: "every".to_owned(),
        search_results: "hits".to_owned(),
    };
    fx.skin.update_selection(&mut fx.sel, &v3, &other).unwrap();
    assert_eq!(
        shown(&fx),
        [s(0, "every"), s(1, "every"), s(5, "q"), s(6, "hits > ")]
    );
    let _ = labels_of();
}

/// The built-in timers start from the frame's starts; a panel's option
/// reads its timer: on while open, off while closed.
#[test]
fn timers_and_panels_come_from_the_frame() {
    let on_change = {
        let mut e = element(Source::Image(4), [0, 0, 10, 10]);
        e.timer = TimerRef::Builtin(selection_timer::CHANGE);
        e.track = keys(&[(100, [0, 0, 10, 10])]);
        e
    };
    let panel = |n: usize, open: bool| {
        let mut e = element(
            Source::Image(5 + u32::try_from(n).unwrap() * 2 + u32::from(!open)),
            [0, 0, 10, 10],
        );
        let src = format!("{}on(panel{})", if open { "" } else { "!" }, n + 1);
        e.visible = Some(boolean(&src));
        e
    };
    let mut elements = vec![on_change];
    for n in 0..3 {
        elements.push(panel(n, true));
        elements.push(panel(n, false));
    }
    let mut fx = Fx::new(load(elements, WidgetDefs::default(), SelectionDefs::default()).unwrap());
    fx.view(&view(Vec::new(), None));
    fx.frame(0);
    assert_eq!(fx.images(), [6, 8, 10], "every panel closed, no change yet");
    fx.timers.change = Some(TimeUs(50_000));
    fx.frame(100);
    assert_eq!(
        fx.images(),
        [6, 8, 10],
        "the change's track starts at 100 ms"
    );
    fx.frame(150);
    assert_eq!(fx.images(), [4, 6, 8, 10]);
    for n in 0..3 {
        let mut timers = fx.timers;
        timers.panels[n] = Some(TimeUs(0));
        fx.timers = timers;
        fx.frame(150);
        let mut want = vec![4];
        for m in 0..3 {
            let open = fx.timers.panels[m].is_some();
            want.push(5 + u32::try_from(m).unwrap() * 2 + u32::from(!open));
        }
        assert_eq!(fx.images(), want, "panel {n} opened");
    }
}

/// The requests hold one line per text element and one per row slot.
#[test]
fn requests_hold_the_texts_and_one_line_per_row() {
    let mut def = list(60, 30);
    def.text = [Some(row_text([0, -20, 100, 20])), None, None];
    let elements = std::iter::once(list_element())
        .chain((0..64).map(|i| element(Source::Text(i), [0, 0, 100, 20])))
        .collect();
    let texts = (0..64)
        .map(|_| TextDef {
            value: TextValue::Literal("x".to_owned()),
            font: skin_model::FontRef::Default,
            align: skin_model::Align::Left,
            overflow: skin_model::Overflow::Spill,
        })
        .collect();
    let skin = load(
        elements,
        WidgetDefs {
            texts,
            ..WidgetDefs::default()
        },
        SelectionDefs {
            song_list: Some(def),
            ..SelectionDefs::default()
        },
    )
    .unwrap();
    let mut fx = Fx::new(skin);
    let rows: Vec<_> = (0..60).map(|i| chart(&format!("r{i}"))).collect();
    fx.view(&view(rows, Some(30)));
    let req = fx
        .skin
        .selection_text_requests(&fx.sel, fx.target, &fx.layout);
    assert_eq!((req.texts.len(), req.rows.len()), (64, 60));
    assert_eq!(req.rows[0].text, "r0");
    assert_eq!(req.rows[59].text, "r59");
}

fn plain_play_def(
    elements: Vec<skin_model::ElementDef>,
    widgets: WidgetDefs,
) -> skin_model::SkinDef {
    skin_def(elements, widgets)
}

/// A play skin draws no song list, ratio bar or selection text; a
/// selection skin no lanes, gauge, judge or cover paint.
#[test]
fn each_kind_draws_its_own_widgets() {
    let kind = |r: Result<Skin, LoadError>, k| match r {
        Err(LoadError::SourceKind { element: 0, kind }) => assert_eq!(kind, k),
        other => panic!("{other:?}"),
    };
    let sel = || SelectionDefs {
        song_list: Some(list(1, 0)),
        ratio_bars: vec![bar(RatioForm::Bar { vertical: false }, RatioSource::ExRate)],
    };
    for source in [Source::SongList, Source::RatioBar(0)] {
        kind(
            Skin::new(plain_play_def(
                vec![element(source, [0, 0, 1, 1])],
                WidgetDefs::default(),
            )),
            ScreenKind::Play,
        );
    }
    let text = WidgetDefs {
        texts: vec![TextDef {
            value: TextValue::Selection(SelectionText::Query),
            font: skin_model::FontRef::Default,
            align: skin_model::Align::Left,
            overflow: skin_model::Overflow::Spill,
        }],
        ..WidgetDefs::default()
    };
    kind(
        Skin::new(plain_play_def(
            vec![element(Source::Text(0), [0, 0, 1, 1])],
            text,
        )),
        ScreenKind::Play,
    );
    for source in [
        Source::Lanes,
        Source::Gauge(0),
        Source::Judge(0),
        Source::CoverPaint(0),
    ] {
        kind(
            load(
                vec![element(source, [0, 0, 1, 1])],
                WidgetDefs::default(),
                sel(),
            ),
            ScreenKind::Selection,
        );
    }
}

/// Each entry point of the other kind is refused or inert and appends
/// nothing; a play timer is not a selection timer.
#[test]
fn the_other_kinds_entry_points_draw_nothing() {
    let skin = load(
        vec![element(Source::Image(0), [0, 0, 10, 10])],
        WidgetDefs::default(),
        SelectionDefs::default(),
    )
    .unwrap();
    assert_eq!(skin.kind(), ScreenKind::Selection);
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let ctx = PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: &[],
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
    };
    assert!(matches!(
        skin.prepare(&chart, &ctx),
        Err(PrepareError::Kind)
    ));
    let uvs = selection_support::uvs(&skin);
    let mut out = skin_model::QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let layout = skin_model::Layout::AUTHOR;
    let mut loading = skin.prepare_loading(Side::Left);
    let lf = LoadingFrame {
        difficulty: None,
        bga: None,
        now: TimeUs(0),
        load: LoadState {
            since: TimeUs(0),
            percent: 0,
        },
        target: [1_920, 1_080],
        layout: &layout,
        turntable: 0,
        lanes: skin_model::LaneView::NONE,
        covers: skin_model::CoverSettings {
            cover: 0,
            lift: 0,
            hidden: 0,
            cover_enabled: false,
            lift_enabled: false,
            hidden_enabled: false,
            adjusting: false,
        },
        gauge: None,
        play_level: None,
        autoplay: false,
    };
    assert_eq!(
        skin.build_loading(&mut loading, &lf, &uvs, &mut out),
        Err(BuildError::Kind)
    );
    assert!(out.quads().is_empty());
    // A selection skin's loading state is inert: no timer, no text slot.
    assert!(loading.state().timers.is_empty());
    assert!(loading.state().fields.iter().all(|&f| f == 0));
    // A play skin's selection state is inert.
    let play = Skin::new(plain_play_def(
        vec![element(Source::Image(0), [0, 0, 10, 10])],
        WidgetDefs::default(),
    ))
    .unwrap();
    let inert = play.prepare_selection();
    assert!(inert.state().timers.is_empty());
    // A play skin attaches nothing to a selection skin's state, nor lays
    // out its bitmap text.
    let mut live = skin.prepare_selection();
    skin.update_selection(&mut live, &three(0), &labels_of())
        .unwrap();
    let before = format!("{live:?}");
    assert_eq!(
        play.attach_selection_text(
            &mut live,
            TextTable::new(Vec::new()),
            TextTable::new(Vec::new())
        ),
        Err(skin_model::TextTableError::Foreign)
    );
    play.attach_selection_bitmap_text(&mut live);
    assert_eq!(format!("{live:?}"), before);
    let mut sel = play.prepare_selection();
    assert_eq!(
        play.update_selection(&mut sel, &view(Vec::new(), None), &labels_of()),
        Err(SelectionError::Kind)
    );
    let frame = skin_model::SelectionFrame {
        now: TimeUs(0),
        target: [1_920, 1_080],
        layout: &layout,
        timers: skin_model::SelectionTimers::default(),
    };
    let play_uvs = selection_support::uvs(&play);
    assert_eq!(
        play.build_selection(&mut sel, &frame, &play_uvs, &mut out),
        Err(BuildError::Kind)
    );
    assert!(out.quads().is_empty());
    // A selection skin refuses the play screen's `ready` timer (slot 0 is
    // `screen` there; past the five built-ins is no timer).
    let mut e = element(Source::Image(0), [0, 0, 10, 10]);
    e.timer = TimerRef::Builtin(selection_timer::BUILTIN);
    assert!(matches!(
        load(vec![e], WidgetDefs::default(), SelectionDefs::default()),
        Err(LoadError::UnknownTimer { element: 0 })
    ));
}

/// The song list's bounds: 1 to 60 placements, the center among them, and
/// its parts' keys and quads charged with the skin's.
#[test]
fn the_song_lists_bounds_are_checked() {
    let with = |def| {
        load(
            vec![list_element()],
            WidgetDefs::default(),
            SelectionDefs {
                song_list: Some(def),
                ..SelectionDefs::default()
            },
        )
    };
    assert!(with(list(60, 59)).is_ok());
    // The counts come first: before the kinds and the keys are read.
    assert!(matches!(
        load(
            vec![element(Source::Lanes, [0, 0, 1, 1])],
            WidgetDefs::default(),
            SelectionDefs {
                song_list: Some(list(61, 0)),
                ..SelectionDefs::default()
            },
        ),
        Err(LoadError::SongList(SongListError::TooManyPlacements {
            n: 61
        }))
    ));
    assert!(matches!(
        load(
            vec![element(Source::Lanes, [0, 0, 1, 1]); skin_model::MAX_ELEMENTS + 1],
            WidgetDefs::default(),
            SelectionDefs::default(),
        ),
        Err(LoadError::TooManyElements { .. })
    ));
    assert!(matches!(
        with(list(61, 0)),
        Err(LoadError::SongList(SongListError::TooManyPlacements {
            n: 61
        }))
    ));
    assert!(matches!(
        with(list(0, 0)),
        Err(LoadError::SongList(SongListError::NoPlacements))
    ));
    assert!(matches!(
        with(list(3, 3)),
        Err(LoadError::SongList(SongListError::Center { center: 3 }))
    ));
    let mut bad = list(1, 0);
    bad.lamp[0] = Some(ImagePartDef {
        part: part([0, 0, 1, 1]),
        image: 999,
    });
    assert!(matches!(
        with(bad),
        Err(LoadError::SongList(SongListError::UnknownImage))
    ));
    // The ratio bar table is bounded as every widget table.
    let bars = |n| {
        load(
            Vec::new(),
            WidgetDefs::default(),
            SelectionDefs {
                song_list: None,
                ratio_bars: vec![bar(RatioForm::Bar { vertical: false }, RatioSource::ExRate); n],
            },
        )
    };
    assert!(bars(skin_model::MAX_ELEMENTS).is_ok());
    assert!(matches!(
        bars(skin_model::MAX_ELEMENTS + 1),
        Err(LoadError::TooManyWidgets { n }) if n == skin_model::MAX_ELEMENTS + 1
    ));
    // Keys: 120 arms of 256 keys and image elements of 256 keys up to the
    // skin's limit exactly; one key past it is refused.
    let many = |n: i32| {
        keys(
            &(0..n)
                .map(|i| (i64::from(i), [0, 0, 1, 1]))
                .collect::<Vec<_>>(),
        )
    };
    let mut big = list(60, 0);
    for p in &mut big.placements {
        for arm in p.iter_mut() {
            arm.part.track = many(256);
        }
    }
    let images = |extra: bool| {
        let mut v: Vec<_> = (0..136)
            .map(|_| {
                let mut e = element(Source::Image(0), [0, 0, 1, 1]);
                e.track = many(256);
                e
            })
            .collect();
        v.push(list_element());
        if extra {
            v.push(element(Source::Image(0), [0, 0, 1, 1]));
        }
        v
    };
    let sel = |l: skin_model::SongListDef| SelectionDefs {
        song_list: Some(l),
        ..SelectionDefs::default()
    };
    // 136 × 256 + 120 × 256 + 1 (the list element) = 65 537 with the list.
    assert!(load(images(false), WidgetDefs::default(), sel(big.clone())).is_err());
    let mut fewer = big.clone();
    fewer.placements[0][0].part.track = many(255);
    assert!(load(images(false), WidgetDefs::default(), sel(fewer)).is_ok());
    assert!(matches!(
        load(images(true), WidgetDefs::default(), sel(big)),
        Err(LoadError::TooManyKeys { n: 65_538 })
    ));
    // Quads: each slot's bar, text, lamp, level cells and labels, per
    // list element.
    let mut q = list(60, 0);
    q.text = [Some(row_text([0, -20, 10, 10])), None, None];
    q.level = levels([0, -10, 10, 10], 11);
    let per = 60 * (2 + 1 + 11 + 3);
    let elements = |n: usize| (0..n).map(|_| list_element()).collect::<Vec<_>>();
    let fit = 16_384 / per;
    assert!(load(elements(fit), WidgetDefs::default(), sel(q.clone())).is_ok());
    assert!(matches!(
        load(elements(fit + 1), WidgetDefs::default(), sel(q)),
        Err(LoadError::TooManyWidgetQuads { .. })
    ));
}

/// Each song list element pays its styles' visibilities once per slot.
#[test]
fn a_song_list_pays_its_visibilities_per_slot() {
    let mut def = list(10, 0);
    let mut lamp = part([0, 0, 1, 1]);
    lamp.visible = Some(boolean("cursor.chart && cursor.folder"));
    let cost = lamp.visible.as_ref().unwrap().op_count();
    def.lamp[0] = Some(ImagePartDef {
        part: lamp,
        image: 2,
    });
    let ops = |n: usize| {
        load(
            (0..n).map(|_| list_element()).collect(),
            WidgetDefs::default(),
            SelectionDefs {
                song_list: Some(def.clone()),
                ..SelectionDefs::default()
            },
        )
        .unwrap()
        .ops()
        .total()
    };
    assert_eq!(ops(1), 10 * cost);
    assert_eq!(ops(2), 20 * cost);
}

/// The build of a frame the preparation filled keeps its timers: a `when`
/// timer reading a runtime output stays as the preparation left it though
/// the runtime writes that output between the two, and the next frame's
/// preparation reads it (docs/stage3.md S7-07; design.md "beatoraja Lua
/// Skins", the frame sequence).
#[test]
fn a_prepared_frame_is_built_without_its_timers_filled_again() {
    let outs = [skin_model::output::OutTy::Bool];
    let when = skin_model::compile(
        "lua.0",
        &skin_model::Scope::new(&skin_model::SELECTION).outputs(&outs),
        skin_model::Ty::Bool,
    )
    .unwrap();
    let mut e = element(Source::Image(4), [0, 0, 10, 10]);
    e.timer = TimerRef::When(0);
    e.track = keys(&[(0, [0, 0, 10, 10])]);
    let mut def = skin_def(vec![e], WidgetDefs::default());
    def.whens = vec![when];
    let skin = Skin::new_selection(def, SelectionDefs::default())
        .unwrap()
        .with_outputs(1);
    let mut fx = Fx::new(skin);
    fx.view(&view(Vec::new(), None));
    let prepare = |fx: &mut Fx, ms: i64| {
        fx.skin
            .prepare_selection_frame(&mut fx.sel, TimeUs(ms * 1_000), &fx.timers)
            .unwrap();
    };
    prepare(&mut fx, 16);
    // The runtime's step: its callback now answers true.
    fx.sel.outputs_mut()[0] = 1;
    fx.frame(16);
    assert_eq!(fx.images(), Vec::<u32>::new(), "the timer as prepared: off");
    prepare(&mut fx, 32);
    fx.frame(32);
    assert_eq!(fx.images(), [4], "the next preparation starts it");
}
