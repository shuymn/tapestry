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
    MAX_TEXT_BYTES, Overflow, PlayContext, PrepareError, ResultError, ResultLabels, ResultNumber,
    ResultStatus, ResultView, ScreenKind, SelectionText, Side, Skin, Source, TextDef, TextField,
    TextImage, TextTable, TextTableError, TextValue, TimerRef, Unavailable, WidgetDefs,
    result_field as field, result_timer,
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
        (field::RESULT_NOTES, 600),
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
    for slot in field::CHART_LEVEL..=field::CHART_LENGTH_VALID {
        assert_eq!(f(slot), 0, "slot {slot}");
    }
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
    fx.view(&ready(1_234, best(ResultNumber::Shown(1)), 2));
    fx.frame(0);
    assert_eq!(shown(&fx), values);
    for why in [
        Unavailable::NothingRecorded,
        Unavailable::CannotLoad,
        Unavailable::OverBudget,
        Unavailable::Failed,
        Unavailable::CancelledBeforePlayback,
    ] {
        let mut v = ready(1_234, best(ResultNumber::Shown(1)), 2);
        v.status = ResultStatus::Unavailable(why);
        fx.view(&v);
        fx.frame(16);
        assert_eq!(shown(&fx), vec![(200.0, vec![d(1), d(2)])], "{why:?}");
        assert_eq!(fx.res.timing(), None);
        assert!(fx.res.timelines().is_none());
    }
    fx.view(&ready(1_234, best(ResultNumber::Shown(1)), 2));
    fx.frame(33);
    assert_eq!(shown(&fx), values);
    assert!(fx.res.timelines().is_some());
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
    let mut fx = RFx::new(Skin::new_result(def).unwrap());
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
