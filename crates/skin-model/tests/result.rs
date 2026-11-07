//! The result kind of the model (docs/stage3.md S10-09g): a result skin
//! loads against the result schema and refuses what only play or
//! selection draws; its state is filled from each result view and built
//! into frames; a value is shown, hidden (read as `i32::MIN`, drawn as
//! nothing) or unavailable (its validity false).

mod result_support;
mod selection_support;

use bms_model::{ChartBuilder, TimeUs};
use result_support::{RFx, best, chart, labels, number, ready, relement, rint, rload};
use selection_support::{DIGITS, frame_of};
use skin_model::{
    Attempt, BestView, BuildError, ChartMetadata, ElementDef, FontRef, ImageChoiceDef, LoadError,
    MAX_TEXT_BYTES, Overflow, PlayContext, PrepareError, ResultDefs, ResultError, ResultFields,
    ResultLabels, ResultNumber, ResultStatus, ResultView, ScreenKind, SelectionText, Side, Skin,
    Source, TextDef, TextField, TextImage, TextTable, TextTableError, TextValue, TimerRef,
    Unavailable, WidgetDefs, result_field as field, result_timer,
};

fn text(value: TextValue) -> TextDef {
    TextDef {
        value,
        font: FontRef::Default,
        align: skin_model::Align::Left,
        overflow: Overflow::Spill,
    }
}

/// A result skin draws images, image choices, numbers and chart text, and
/// refuses the lanes, a gauge, a judge, a cover paint, the song list, a
/// ratio bar and a selection string, each at its element.
#[test]
fn a_result_skin_refuses_what_only_play_or_selection_draws() {
    let widgets = || WidgetDefs {
        numbers: vec![number("ex_score", 4)],
        texts: vec![
            text(TextValue::Field(TextField::Title)),
            text(TextValue::Selection(SelectionText::Query)),
        ],
        ..WidgetDefs::default()
    };
    let ok = vec![
        relement(Source::Image(0), [0, 0, 10, 10], None),
        relement(Source::Number(0), [0, 20, 10, 10], None),
        relement(Source::Text(0), [0, 40, 100, 20], None),
    ];
    let skin = rload(ok.clone(), widgets()).unwrap();
    assert_eq!(skin.kind(), ScreenKind::Result);
    for refused in [
        Source::Lanes,
        Source::Gauge(0),
        Source::Judge(0),
        Source::CoverPaint(0),
        Source::SongList,
        Source::RatioBar(0),
        Source::Text(1),
    ] {
        let mut elements = ok.clone();
        elements.push(relement(refused, [0, 60, 10, 10], None));
        let got = rload(elements, widgets());
        assert!(
            matches!(
                got,
                Err(LoadError::SourceKind {
                    element: 3,
                    kind: ScreenKind::Result
                })
            ),
            "{refused:?}: {got:?}"
        );
    }
    // A play timer past the result's two is not a result timer.
    let mut elements = ok;
    elements.push(ElementDef {
        timer: TimerRef::Builtin(result_timer::BUILTIN),
        ..relement(Source::Image(0), [0, 0, 10, 10], None)
    });
    assert!(matches!(
        rload(elements, widgets()),
        Err(LoadError::UnknownTimer { element: 3 })
    ));
}

/// The result entry points refuse a skin of another kind, or stay inert;
/// a result skin's play and selection entry points refuse, and a state
/// another result skin made draws nothing.
#[test]
fn result_entry_points_refuse_other_kinds() {
    let elements = || vec![relement(Source::Image(0), [0, 0, 10, 10], None)];
    let play = Skin::new(selection_support::skin_def(
        elements(),
        WidgetDefs::default(),
    ))
    .unwrap();
    let mut inert = play.prepare_result();
    assert_eq!(
        play.update_result(&mut inert, &ready(10, BestView::Unavailable, 1), &labels()),
        Err(ResultError::Kind)
    );
    let mut fx = RFx::new(rload(elements(), WidgetDefs::default()).unwrap());
    let frame = skin_model::ResultFrame {
        now: TimeUs(0),
        target: fx.target,
        layout: &fx.layout,
        timers: fx.timers,
    };
    assert_eq!(
        play.build_result(&mut inert, &frame, &fx.uvs, &mut fx.batch),
        Err(BuildError::Kind)
    );
    // The result skin refuses a play, a play frame and selection.
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let ctx = PlayContext {
        meta: ChartMetadata::UNKNOWN,
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
        fx.skin.prepare(&chart, &ctx),
        Err(PrepareError::Kind)
    ));
    let mut sel = fx.skin.prepare_selection();
    assert!(
        fx.skin
            .update_selection(
                &mut sel,
                &selection_support::view(Vec::new(), None),
                &selection_support::labels_of()
            )
            .is_err()
    );
    // A state of another result skin, or of a play skin, draws nothing.
    let other = rload(elements(), WidgetDefs::default()).unwrap();
    let mut foreign = other.prepare_result();
    assert_eq!(
        fx.skin
            .update_result(&mut foreign, &ready(1, BestView::Unavailable, 1), &labels()),
        Err(ResultError::Kind)
    );
    fx.skin
        .build_result(&mut foreign, &frame, &fx.uvs, &mut fx.batch)
        .unwrap();
    fx.skin
        .build_result(&mut inert, &frame, &fx.uvs, &mut fx.batch)
        .unwrap();
    assert!(fx.batch.quads().is_empty());
    assert_eq!(
        fx.skin
            .attach_result_text(&mut foreign, TextTable::default()),
        Err(TextTableError::Foreign)
    );
    // Its own state draws.
    fx.frame(0);
    assert_eq!(fx.drawn().len(), 1);
}

/// `update_result` fills every field from the view: the chart's under
/// every status, the result's, the timing's and the best's while ready, a
/// hidden best value as `i32::MIN`.
#[test]
fn a_view_fills_the_result_fields() {
    let mut fx = RFx::new(rload(vec![], WidgetDefs::default()).unwrap());
    fx.view(&ready(1_000, best(ResultNumber::Shown(-3)), 4));
    let f = |slot: u16| fx.res.state().fields[usize::from(slot)];
    for (slot, want) in [
        (field::RESULT_VALID, 1),
        (field::EX_SCORE, 1_000),
        (field::MAX_COMBO, 77),
        (field::RESULT_BP, 20),
        (field::CHART_NOTES, 600),
        (field::CHART_NOTES_VALID, 1),
        (field::COUNT_PERFECT, 400),
        (field::COUNT_GREAT, 2),
        (field::COUNT_POOR, 5),
        (field::COUNT_EMPTY_POOR, 6),
        // GREAT to POOR early, with the early empty POORs: 1 + 1 + 2 + 3 + 4.
        (field::COUNT_FAST, 11),
        // 1 + 2 + 2 + 2 + 2.
        (field::COUNT_SLOW, 9),
        (field::COUNT_EARLY_PERFECT, 10),
        (field::COUNT_EARLY_PERFECT + 1, 390),
        (field::COUNT_LATE_EMPTY_POOR - 1, 4),
        (field::COUNT_LATE_EMPTY_POOR, 2),
        (field::GAUGE, 805),
        (field::RESULT_RATE_VALID, 1),
        (field::RATE_CURRENT_BP, 8_333),
        (field::RESULT_GRADE, 6),
        (field::RESULT_NEXT_RANK, 17),
        // NORMAL's clear type.
        (field::RESULT_CLEAR, 5),
        (field::RESULT_CLEARED, 1),
        (field::TIMING_VALID, 1),
        (field::TIMING_MEAN_X100, -375),
        (field::TIMING_SD_X100, 1_234),
        (field::BEST_KNOWN, 1),
        (field::BEST_EX, 900),
        (field::BEST_EX_DIFF, 100),
        (field::BEST_BP, 23),
        (field::BEST_BP_DIFF, -3),
        // HARD's clear type.
        (field::BEST_CLEAR, 6),
        (field::BEST_EX_UPDATED, 1),
        (field::BEST_BP_UPDATED, 1),
        (field::BEST_RATE_VALID, 1),
        (field::BEST_RATE_BP, 7_500),
        (field::BEST_GRADE, 5),
        (field::BEST_RATE_UPDATED, 1),
        (field::CHART_LEVEL, 12),
        (field::CHART_LEVEL_VALID, 1),
        (field::CHART_TOTAL, 300),
        (field::CHART_TOTAL_VALID, 1),
        (field::CHART_BPM_MIN_X100, 15_000),
        (field::CHART_BPM_MAX_X100, 18_050),
        (field::CHART_BPM_CHANGES, 1),
        (field::CHART_BPM_VALID, 1),
        (field::CHART_DIFFICULTY, 4),
        (field::CHART_LENGTH_MS, 123_456),
        (field::CHART_LENGTH_VALID, 1),
    ] {
        assert_eq!(f(slot), want, "slot {slot}");
    }
    assert_eq!(fx.res.timing().unwrap().samples, 500);
    assert_eq!(fx.res.timelines().unwrap().bins().len(), 4);
    // Hidden: the BP and its difference read `i32::MIN`, the best known.
    fx.view(&ready(1_000, best(ResultNumber::Hidden), 4));
    let f = |slot: u16| fx.res.state().fields[usize::from(slot)];
    assert_eq!(f(field::BEST_KNOWN), 1);
    assert_eq!(f(field::BEST_BP), i32::MIN);
    assert_eq!(f(field::BEST_BP_DIFF), i32::MIN);
    // Unavailable best: every best field is 0 and invalid.
    fx.view(&ready(1_000, BestView::Unavailable, 4));
    let f = |slot: u16| fx.res.state().fields[usize::from(slot)];
    for slot in field::BEST_KNOWN..=field::BEST_RATE_UPDATED {
        assert_eq!(f(slot), 0, "slot {slot}");
    }
    // An unknown chart fact is invalid; a difficulty outside 1 to 5 is 0.
    let mut v = ready(1, BestView::Unavailable, 1);
    v.chart = skin_model::ResultChart {
        difficulty: Some(6),
        ..skin_model::ResultChart::default()
    };
    fx.view(&v);
    let f = |slot: u16| fx.res.state().fields[usize::from(slot)];
    for slot in field::CHART_LEVEL..=field::CHART_NOTES_VALID {
        assert_eq!(f(slot), 0, "slot {slot}");
    }
    assert_eq!(f(field::CHART_NOTES), 0);
}

/// The fields a result state reads after a view are exactly
/// [`ResultFields::of`] that view, the conversion an import reads at load:
/// for shown, hidden and unavailable values and an unavailable view.
#[test]
fn the_state_reads_the_views_result_fields() {
    let mut fx = RFx::new(rload(vec![], WidgetDefs::default()).unwrap());
    let mut unavailable = ready(7, best(ResultNumber::Shown(-3)), 1);
    unavailable.status = ResultStatus::Unavailable(Unavailable::Failed);
    for view in [
        ready(1_000, best(ResultNumber::Shown(-3)), 4),
        ready(1_000, best(ResultNumber::Hidden), 4),
        ready(1_000, BestView::Unavailable, 4),
        result_support::other(),
        unavailable,
    ] {
        fx.view(&view);
        let fields = ResultFields::of(&view);
        assert_eq!(fx.res.state().fields, &fields.0[..], "{view:?}");
        assert_eq!(fields.state().fields, fx.res.state().fields);
    }
    let hidden = ResultFields::of(&ready(1_000, best(ResultNumber::Hidden), 4));
    assert_eq!(hidden.0[usize::from(field::BEST_BP_DIFF)], i32::MIN);
}

/// C12: a number on the BP difference and an element whose visibility
/// reads it, under a shown, a hidden and an unavailable value. The number
/// carries the best's validity as its guard, as a front end joins it. The
/// number draws −3, nothing, nothing; the element draws, draws (a hidden
/// value reads `i32::MIN`, which is not 0), and does not draw (its guard
/// is false).
#[test]
fn a_hidden_value_reads_min_and_its_number_draws_nothing() {
    let skin = rload(
        vec![
            relement(Source::Number(0), [100, 100, 10, 10], Some("best.known")),
            relement(
                Source::Image(5),
                [100, 200, 10, 10],
                Some("best.known && best.bp_diff != 0"),
            ),
            relement(
                Source::Image(6),
                [100, 300, 10, 10],
                Some("best.known && best.bp_diff == -2147483647 - 1"),
            ),
        ],
        WidgetDefs {
            numbers: vec![number("best.bp_diff", 4)],
            ..WidgetDefs::default()
        },
    )
    .unwrap();
    let mut fx = RFx::new(skin);
    let mut frame = |b: BestView| {
        fx.view(&ready(500, b, 1));
        fx.frame(0);
        (fx.at_y(100.0), fx.at_y(200.0), fx.at_y(300.0))
    };
    // Shown −3: ten glyphs show the magnitude.
    assert_eq!(
        frame(best(ResultNumber::Shown(-3))),
        (vec![(DIGITS, 3)], vec![(5, 0)], vec![])
    );
    assert_eq!(
        frame(best(ResultNumber::Hidden)),
        (vec![], vec![(5, 0)], vec![(6, 0)])
    );
    assert_eq!(frame(BestView::Unavailable), (vec![], vec![], vec![]));
}

/// C14: a view that goes ready, unavailable, then ready shows the values,
/// then the chart's fields alone, then the values again; each number
/// carries its group's validity as its guard.
#[test]
fn an_unavailable_view_shows_the_chart_alone() {
    let skin = rload(
        vec![
            relement(Source::Number(0), [100, 100, 10, 10], Some("result.valid")),
            relement(
                Source::Number(1),
                [100, 200, 10, 10],
                Some("chart.level_valid"),
            ),
            relement(Source::Number(2), [100, 300, 10, 10], Some("timing.valid")),
            relement(Source::Number(3), [100, 400, 10, 10], Some("best.known")),
            relement(Source::Image(7), [100, 500, 10, 10], Some("result.cleared")),
        ],
        WidgetDefs {
            numbers: vec![
                number("ex_score", 4),
                number("chart.level", 2),
                number("timing.sd_x100", 4),
                number("best.ex", 3),
            ],
            ..WidgetDefs::default()
        },
    )
    .unwrap();
    let mut fx = RFx::new(skin);
    let d = |n: u16| (DIGITS, n);
    let values = vec![
        (100.0, vec![d(1), d(2), d(3), d(4)]),
        (200.0, vec![d(1), d(2)]),
        (300.0, vec![d(1), d(2), d(3), d(4)]),
        (400.0, vec![d(9), d(0), d(0)]),
        (500.0, vec![(7, 0)]),
    ];
    let shown = |fx: &RFx| -> Vec<(f32, Vec<(u32, u16)>)> {
        [100.0, 200.0, 300.0, 400.0, 500.0]
            .into_iter()
            .map(|y| (y, fx.at_y(y)))
            .filter(|(_, q)| !q.is_empty())
            .collect()
    };
    // B differs from A in its score, best, timing and timelines.
    let a = ready(1_234, best(ResultNumber::Shown(1)), 2);
    let b = result_support::other();
    let b_values = vec![
        (100.0, vec![d(5), d(6), d(7), d(8)]),
        (200.0, vec![d(1), d(2)]),
        (300.0, vec![d(4), d(3), d(2), d(1)]),
        (400.0, vec![d(4), d(4), d(4)]),
    ];
    // What B alone gives a fresh state of the same skin.
    let mut fresh = RFx::new(fx.skin.clone());
    fresh.view(&b);
    fresh.frame(33);
    assert_eq!(shown(&fresh), b_values);
    for why in [
        Unavailable::NothingRecorded,
        Unavailable::CannotLoad,
        Unavailable::OverBudget,
        Unavailable::Failed,
        Unavailable::CancelledBeforePlayback,
    ] {
        fx.view(&a);
        fx.frame(0);
        assert_eq!(shown(&fx), values, "{why:?}");
        let mut v = a.clone();
        v.status = ResultStatus::Unavailable(why);
        fx.view(&v);
        fx.frame(16);
        assert_eq!(shown(&fx), vec![(200.0, vec![d(1), d(2)])], "{why:?}");
        assert_eq!(fx.res.timing(), None);
        assert!(fx.res.timelines().is_none());
        // Nothing of A survives B: every field, the timing, the timelines
        // and the frame are B's alone.
        fx.view(&b);
        fx.frame(33);
        assert_eq!(shown(&fx), b_values, "{why:?}");
        assert_eq!(fx.drawn(), fresh.drawn(), "{why:?}");
        assert_eq!(fx.res.state().fields, fresh.res.state().fields, "{why:?}");
        assert_eq!(fx.res.timing(), b.timing);
        assert_eq!(fx.res.timelines(), b.timelines.as_ref());
    }
}

/// The fade-out timer is off until its start, and a `when` timer runs over
/// the result's fields.
#[test]
fn the_result_timers_follow_the_frame() {
    let mut def = selection_support::skin_def(
        vec![
            ElementDef {
                timer: TimerRef::Builtin(result_timer::FADEOUT),
                ..relement(Source::Image(1), [0, 100, 10, 10], None)
            },
            ElementDef {
                timer: TimerRef::When(0),
                ..relement(Source::Image(2), [0, 200, 10, 10], None)
            },
        ],
        WidgetDefs::default(),
    );
    def.whens = vec![result_support::rbool("result.cleared")];
    let mut fx = RFx::new(Skin::new_result(def, ResultDefs::default()).unwrap());
    fx.view(&ready(1, BestView::Unavailable, 1));
    fx.frame(0);
    assert_eq!((fx.at_y(100.0), fx.at_y(200.0)), (vec![], vec![(2, 0)]));
    fx.timers.fadeout = Some(TimeUs(5_000));
    fx.frame(10);
    assert_eq!(fx.at_y(100.0), vec![(1, 0)]);
    let mut v = ready(1, BestView::Unavailable, 1);
    v.score.cleared = false;
    fx.view(&v);
    fx.frame(20);
    assert_eq!(fx.at_y(200.0), vec![]);
}

/// The text requests show the chart's strings of the last view, each cut
/// to `MAX_TEXT_BYTES`; the state keeps at most twice that a string, and
/// a table of another length is refused.
#[test]
fn result_text_is_the_charts_and_bounded() {
    let mut fx = RFx::new(
        rload(
            vec![
                relement(Source::Text(0), [0, 0, 300, 20], None),
                relement(Source::Text(1), [0, 50, 300, 20], None),
            ],
            WidgetDefs {
                texts: vec![
                    text(TextValue::Field(TextField::FullTitle)),
                    text(TextValue::Field(TextField::Artist)),
                ],
                ..WidgetDefs::default()
            },
        )
        .unwrap(),
    );
    fx.view(&ready(1, BestView::Unavailable, 1));
    let requests = fx.skin.result_text_requests(&fx.res, fx.target, &fx.layout);
    let texts: Vec<&str> = requests.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(texts, ["Song [X]", "Someone"]);
    let long = "é".repeat(4 * MAX_TEXT_BYTES);
    let big = ResultLabels {
        title: long.clone(),
        subtitle: long.clone(),
        artist: long.clone(),
        subartist: long.clone(),
        genre: long,
    };
    let bound = fx.res.resident_bytes() + 5 * 2 * u64::try_from(MAX_TEXT_BYTES).unwrap();
    fx.skin
        .update_result(&mut fx.res, &ready(1, BestView::Unavailable, 1), &big)
        .unwrap();
    assert!(fx.res.resident_bytes() <= bound);
    // Strings growing view after view, from a fresh state, keep at most
    // twice `MAX_TEXT_BYTES` each, however often they grow.
    let mut res = fx.skin.prepare_result();
    let bound = res.resident_bytes() + 5 * 2 * u64::try_from(MAX_TEXT_BYTES).unwrap();
    let mut no_bins = ready(1, BestView::Unavailable, 1);
    no_bins.timelines = None;
    for n in (1..=3 * MAX_TEXT_BYTES).step_by(37) {
        let s = "x".repeat(n);
        let grown = ResultLabels {
            title: s.clone(),
            subtitle: s.clone(),
            artist: s.clone(),
            subartist: s.clone(),
            genre: s,
        };
        fx.skin.update_result(&mut res, &no_bins, &grown).unwrap();
        assert!(res.resident_bytes() <= bound, "{n}");
    }
    for r in fx.skin.result_text_requests(&fx.res, fx.target, &fx.layout) {
        assert!(r.text.len() <= MAX_TEXT_BYTES && !r.text.is_empty());
    }
    assert_eq!(
        fx.skin
            .attach_result_text(&mut fx.res, TextTable::new(vec![None])),
        Err(TextTableError::Len {
            expected: 2,
            got: 1
        })
    );
    let line = TextImage::new(
        skin_model::FrameUv {
            uv: [0.0, 0.0, 0.5, 0.5],
            layer: 0,
        },
        [0.0, 0.0, 1.0, 1.0],
        1.0,
    );
    fx.skin
        .attach_result_text(&mut fx.res, TextTable::new(vec![line, None]))
        .unwrap();
    fx.frame(0);
    let lines = fx
        .batch
        .quads()
        .iter()
        .filter(|q| frame_of(&fx.uvs, q).is_none())
        .count();
    assert_eq!(lines, 1);
}

/// An image choice selects by a result field: the clear type.
#[test]
fn an_image_choice_selects_by_a_result_field() {
    let choice = ImageChoiceDef {
        value: rint("result.clear"),
        choices: (0..9)
            .map(|i| {
                Some(skin_model::ImageChoicePart {
                    span: skin_model::ImageSpan {
                        image: i,
                        first: 0,
                        frames: 1,
                    },
                    cycle: None,
                    track: None,
                    native: None,
                })
            })
            .collect(),
        past_table: skin_model::PastTable::Hide,
    };
    let mut fx = RFx::new(
        rload(
            vec![relement(Source::ImageChoice(0), [0, 0, 10, 10], None)],
            WidgetDefs {
                image_choices: vec![choice],
                ..WidgetDefs::default()
            },
        )
        .unwrap(),
    );
    let v: ResultView = ready(1, BestView::Unavailable, 1);
    assert_eq!(v.chart, chart());
    fx.view(&v);
    fx.frame(0);
    // NORMAL's clear type, 5.
    assert_eq!(fx.at_y(0.0), vec![(5, 0)]);
}

/// On result a number draws nothing for `i32::MIN` or `i32::MAX` and draws
/// every value between; a selection number still draws both extremes (play
/// and loading are `number.rs`' and `loading.rs`').
#[test]
fn a_result_number_leaves_out_the_i32_extremes() {
    let srcs = ["2147483647", "2147483646", "-2147483647 - 1", "-2147483647"];
    let elements = || {
        (0..4_i32)
            .map(|i| {
                relement(
                    Source::Number(u32::try_from(i).unwrap()),
                    [100, 100 * (i + 1), 10, 10],
                    None,
                )
            })
            .collect::<Vec<_>>()
    };
    let mut fx = RFx::new(
        rload(
            elements(),
            WidgetDefs {
                numbers: srcs.iter().map(|s| number(s, 10)).collect(),
                ..WidgetDefs::default()
            },
        )
        .unwrap(),
    );
    fx.view(&ready(1, BestView::Unavailable, 1));
    fx.frame(0);
    let d =
        |s: &str| -> Vec<(u32, u16)> { s.bytes().map(|b| (DIGITS, u16::from(b - b'0'))).collect() };
    assert_eq!(fx.at_y(100.0), vec![]);
    assert_eq!(fx.at_y(200.0), d("2147483646"));
    assert_eq!(fx.at_y(300.0), vec![]);
    assert_eq!(fx.at_y(400.0), d("2147483647"));
    // The same numbers on selection.
    let numbers = srcs
        .iter()
        .map(|s| skin_model::NumberDef {
            digits: selection_support::digits(10),
            value: selection_support::int(s),
        })
        .collect();
    let mut sel = selection_support::Fx::new(
        selection_support::load(
            elements()
                .into_iter()
                .map(|e| ElementDef {
                    timer: TimerRef::Builtin(skin_model::selection_timer::SCREEN),
                    visible: None,
                    ..e
                })
                .collect(),
            WidgetDefs {
                numbers,
                ..WidgetDefs::default()
            },
            skin_model::SelectionDefs::default(),
        )
        .unwrap(),
    );
    sel.view(&selection_support::view(Vec::new(), None));
    sel.frame(0);
    let at = |y: f32| -> Vec<(u32, u16)> {
        sel.drawn()
            .into_iter()
            .filter(|(_, r)| r[1] == y)
            .filter_map(|(f, _)| f)
            .collect()
    };
    assert_eq!(at(100.0), d("2147483647"));
    assert_eq!(at(300.0), d("2147483648"));
}

/// Bitmap text on result: laid out from the last view's chart strings by
/// `attach_result_bitmap_text`, one glyph quad per character, nothing
/// before it is called, and laid out again for a new view.
#[test]
fn result_bitmap_text_follows_the_view() {
    use image::{ImageFormat, Rgba, RgbaImage};
    use skin_model::{FileDef, Filter, ImageDef, Root, SheetBuilder};
    let dir = tempfile::tempdir().unwrap();
    RgbaImage::from_pixel(2, 2, Rgba([255; 4]))
        .save_with_format(dir.path().join("0.png"), ImageFormat::Png)
        .unwrap();
    std::fs::write(
        dir.path().join("f.fnt"),
        "info face=\"t\" size=10 padding=0,0,0,0\n\
         common lineHeight=12 base=10 scaleW=16 scaleH=16 pages=1\n\
         page id=0 file=\"f.png\"\n\
         char id=65 x=0 y=0 width=4 height=4 xoffset=0 yoffset=0 xadvance=5 page=0\n\
         char id=66 x=4 y=0 width=4 height=4 xoffset=0 yoffset=0 xadvance=5 page=0\n",
    )
    .unwrap();
    RgbaImage::from_pixel(16, 16, Rgba([255; 4]))
        .save_with_format(dir.path().join("f.png"), ImageFormat::Png)
        .unwrap();
    let mut b = SheetBuilder::new(&Root::new(dir.path()).unwrap());
    let file = b
        .admit_file(FileDef {
            path: "0.png".into(),
            hi: None,
        })
        .unwrap();
    b.add_image(ImageDef {
        file,
        rect: None,
        grid: [1, 1],
        cycle: None,
        filter: Filter::Linear,
    })
    .unwrap();
    let f = b.admit_fnt("f.fnt", &mut |_| Ok(()), &|| false).unwrap();
    b.add_bitmap_font(f, &[]).unwrap();
    let mut def = selection_support::skin_def(
        vec![relement(Source::Text(0), [0, 0, 200, 20], None)],
        WidgetDefs {
            texts: vec![TextDef {
                font: FontRef::Bitmap(0),
                ..text(TextValue::Field(TextField::Title))
            }],
            ..WidgetDefs::default()
        },
    );
    def.sheet = b.finish();
    let mut fx = RFx::new(Skin::new_result(def, ResultDefs::default()).unwrap());
    let titled = |title: &str| ResultLabels {
        title: title.to_owned(),
        ..labels()
    };
    let view = ready(1, BestView::Unavailable, 1);
    fx.skin
        .update_result(&mut fx.res, &view, &titled("ABA"))
        .unwrap();
    fx.frame(0);
    assert_eq!(fx.batch.quads().len(), 0);
    fx.skin.attach_result_bitmap_text(&mut fx.res);
    fx.frame(0);
    assert_eq!(fx.batch.quads().len(), 3);
    fx.skin
        .update_result(&mut fx.res, &view, &titled("BB"))
        .unwrap();
    fx.skin.attach_result_bitmap_text(&mut fx.res);
    fx.frame(0);
    assert_eq!(fx.batch.quads().len(), 2);
    // A state of another skin is left as it is.
    let mut foreign = rload(vec![], WidgetDefs::default())
        .unwrap()
        .prepare_result();
    fx.skin.attach_result_bitmap_text(&mut foreign);
}
