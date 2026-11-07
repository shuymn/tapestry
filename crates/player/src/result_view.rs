//! A result view's projections as a result skin reads them (design.md
//! "Personal Bests", skin projections): `game`'s figures, timing
//! statistics, deviations, judge areas, emphasized gauge, timelines (with
//! the result's notes per bin) and best comparison converted into
//! `skin_model`'s result types, with each lamp and the gauge given its
//! gauge's look under the view's preset. `skin_model` holds no `game` type and `game` no library type,
//! so the conversion is the player's.

use game::{BestCompare, BestLamp, Rate, ResultBasis, ResultGraphs, ViewSummary};
use rules::RulePreset;
use skin_model::{
    BestFigures, BestRate, BestView, DEVIATION_SLOTS, GaugeColumn, GaugeDesc, GaugeHistory,
    GaugeLook, LampKey, OwnLamp, ResultNumber, ResultRate, ResultScore, RowScore, Timelines,
    TimingStats,
};

/// The parts of a ready view a [`skin_model::ResultView`] carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultParts {
    pub score: ResultScore,
    /// The judged notes of the draw played, for the view's
    /// [`skin_model::ResultChart::notes`].
    pub notes: u32,
    pub best: BestView,
    pub timing: Option<TimingStats>,
    pub timelines: Option<Timelines>,
    /// The emphasized gauge's history; `None` without one.
    pub gauge: Option<Box<GaugeHistory>>,
    pub deviations: Box<[u32; DEVIATION_SLOTS]>,
    pub timing_areas: [[i16; 2]; 5],
}

/// The parts of `summary`, a view under `preset`, of the result whose
/// views share `basis` ([`game::ResultScreen::basis`]). A best not yet
/// read is unavailable.
pub fn result_parts(
    summary: &ViewSummary,
    preset: &RulePreset,
    basis: &ResultBasis,
) -> ResultParts {
    let f = &summary.figures;
    let t = &summary.graphs.timelines;
    ResultParts {
        score: ResultScore {
            tally: summary.tally,
            gauge_permille: f.gauge_permille,
            clear: lamp_key(Some(f.clear), preset),
            cleared: f.cleared,
            rate: f.rate.map(rate),
            next_rank: f.next_rank,
        },
        notes: f.notes,
        best: match summary.best {
            None | Some(BestCompare::Unavailable) => BestView::Unavailable,
            Some(BestCompare::Known(b)) => BestView::Known(BestFigures {
                ex: b.ex,
                ex_diff: saturated(b.ex_diff),
                bp: b.bp.map_or(ResultNumber::Hidden, |v| {
                    ResultNumber::Shown(saturated(i64::from(v)))
                }),
                bp_diff: b
                    .bp_diff
                    .map_or(ResultNumber::Hidden, |v| ResultNumber::Shown(saturated(v))),
                clear: lamp_key(b.clear, preset),
                ex_updated: b.ex_updated,
                bp_updated: b.bp_updated,
                rate: b.rate.map(|r| BestRate {
                    rate_bp: r.rate.rate_bp,
                    grade: r.rate.grade,
                    updated: r.updated,
                }),
            }),
        },
        timing: f.timing.map(|s| TimingStats {
            samples: s.samples,
            mean_x100: s.mean_x100,
            sd_x100: s.sd_x100,
        }),
        timelines: Timelines::new(
            t.axis.width,
            t.used().to_vec(),
            basis.notes_used().to_vec(),
            t.uncovered_from,
        )
        .ok(),
        gauge: gauge_history(&summary.graphs, preset),
        deviations: Box::new(summary.graphs.deviations),
        timing_areas: f.areas,
    }
}

/// The emphasized gauge's history in `graphs`, with its rule's maximum,
/// border and look under `preset`; `None` without an emphasized gauge or
/// its rule.
fn gauge_history(graphs: &ResultGraphs, preset: &RulePreset) -> Option<Box<GaugeHistory>> {
    let trace = graphs
        .traces
        .get(usize::from(graphs.emphasized?))?
        .as_ref()?;
    let rule = preset.gauges.get(usize::from(trace.gauge))?;
    let desc = GaugeDesc::from_rule(rule);
    Some(Box::new(GaugeHistory {
        columns: trace.columns.map(|c| GaugeColumn {
            low: c.low,
            high: c.high,
            last: c.last,
        }),
        max: desc.max,
        border: desc.border,
        look: desc.look,
        reached: graphs.reached,
        no_input_from: graphs.no_input_from,
    }))
}

fn rate(r: Rate) -> ResultRate {
    ResultRate {
        rate_bp: r.rate_bp,
        grade: r.grade,
    }
}

/// `v` within `i32`.
fn saturated(v: i64) -> i32 {
    i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX })
}

/// The lamp key of `lamp` under `preset`: no play without one, a cleared
/// gauge by its look, as a row's lamp is keyed.
pub fn lamp_key(lamp: Option<BestLamp>, preset: &RulePreset) -> LampKey {
    let own = match lamp {
        None => return LampKey::NoPlay,
        Some(BestLamp::FullCombo) => OwnLamp::FullCombo,
        Some(BestLamp::Failed) => OwnLamp::Failed,
        Some(BestLamp::Cleared { gauge }) => match preset.gauges.get(usize::from(gauge)) {
            Some(g) => OwnLamp::Cleared {
                gauge,
                rank: g.lamp,
                look: GaugeLook::of(g),
            },
            None => OwnLamp::Failed,
        },
    };
    LampKey::of(RowScore::Best {
        plays: 0,
        ex: 0,
        bp: 0,
        lamp: own,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bms_model::{ChartBuilder, Lane, TimeUs};
    use eval::{Evaluation, GaugePoint, GaugeResult, JudgeEvent, JudgeEventKind};
    use game::{
        Axis, BestCompare, BestFigures, BestLamp, BestRate, GRAPH_COLUMNS, Rate, ResultBasis,
        ResultFigures, ResultGraphs, SHOWN_GAUGES, TimelineAxis, ViewSummary,
    };
    use replay::verify::Coverage;
    use rules::{GaugeKind, GaugeValue, Judgment, RulePreset};
    use skin_model::{BestView, GaugeColumn, GaugeLook, LampKey, ResultNumber, ResultRate};

    use super::{lamp_key, result_parts};

    /// A basis of three 2 s bins holding 5, 6 and 7 notes.
    fn basis() -> ResultBasis {
        let mut notes_per_bin = [0; game::MAX_TIMELINE_BINS];
        notes_per_bin[..3].copy_from_slice(&[5, 6, 7]);
        ResultBasis {
            notes: 600,
            timeline: TimelineAxis {
                origin: TimeUs(0),
                width: TimeUs(2_000_000),
                count: 3,
            },
            notes_per_bin,
            random: false,
            sectioned: false,
        }
    }

    fn summary(best: Option<BestCompare>) -> ViewSummary {
        let mut graphs = ResultGraphs::EMPTY;
        graphs.timelines.axis = basis().timeline;
        graphs.timelines.bins[1][4] = 2;
        graphs.timelines.uncovered_from = Some(2);
        ViewSummary {
            tally: eval::Tally::default(),
            gauges: [None; SHOWN_GAUGES],
            more_gauges: 0,
            coverage: Coverage::Recorded,
            as_played: true,
            graphs: Arc::new(graphs),
            figures: ResultFigures {
                notes: 600,
                max_ex: 1_200,
                rate: Some(Rate {
                    rate_bp: 8_333,
                    grade: 6,
                }),
                next_rank: 67,
                clear: BestLamp::Cleared { gauge: 1 },
                cleared: true,
                gauge_permille: 1,
                timing: Some(game::TimingStats {
                    samples: 4,
                    mean_x100: -75,
                    sd_x100: 12,
                }),
                areas: [[0; 2]; 5],
            },
            best,
        }
    }

    /// Every lamp keys by its look under the view's preset; no lamp is no
    /// play, and a gauge the preset lacks is failed.
    #[test]
    fn lamps_key_by_their_gauges_look() {
        let p = rules::tapestry();
        assert_eq!(lamp_key(None, &p), LampKey::NoPlay);
        assert_eq!(lamp_key(Some(BestLamp::FullCombo), &p), LampKey::FullCombo);
        assert_eq!(lamp_key(Some(BestLamp::Failed), &p), LampKey::Failed);
        let cleared = |gauge| lamp_key(Some(BestLamp::Cleared { gauge }), &p);
        assert_eq!(cleared(0), LampKey::Normal);
        assert_eq!(cleared(1), LampKey::Hard);
        assert_eq!(cleared(9), LampKey::Failed);
    }

    /// The figures, best, timing and timelines convert value for value;
    /// an empty best's BP and difference are hidden, an unread or
    /// unavailable best is unavailable, and a difference past `i32` stays
    /// at its bound.
    #[test]
    fn a_views_parts_convert_value_for_value() {
        let p = rules::tapestry();
        let known = BestFigures {
            ex: 0,
            ex_diff: i64::from(u32::MAX),
            bp: None,
            bp_diff: None,
            clear: None,
            ex_updated: true,
            bp_updated: true,
            rate: Some(BestRate {
                rate: Rate {
                    rate_bp: 0,
                    grade: 0,
                },
                updated: true,
            }),
        };
        let parts = result_parts(&summary(Some(BestCompare::Known(known))), &p, &basis());
        assert_eq!(parts.notes, 600);
        assert_eq!(parts.score.gauge_permille, 1);
        assert_eq!(parts.score.clear, LampKey::Hard);
        assert!(parts.score.cleared);
        assert_eq!(
            parts.score.rate,
            Some(ResultRate {
                rate_bp: 8_333,
                grade: 6
            })
        );
        assert_eq!(parts.score.next_rank, 67);
        let BestView::Known(b) = parts.best else {
            panic!("{:?}", parts.best);
        };
        assert_eq!(b.ex_diff, i32::MAX);
        assert_eq!(
            (b.bp, b.bp_diff),
            (ResultNumber::Hidden, ResultNumber::Hidden)
        );
        assert_eq!(b.clear, LampKey::NoPlay);
        assert!(b.ex_updated && b.bp_updated && b.rate.unwrap().updated);
        let shown = BestFigures {
            bp: Some(12),
            bp_diff: Some(-3),
            clear: Some(BestLamp::FullCombo),
            ..known
        };
        let BestView::Known(b) =
            result_parts(&summary(Some(BestCompare::Known(shown))), &p, &basis()).best
        else {
            panic!("unavailable");
        };
        assert_eq!(
            (b.bp, b.bp_diff),
            (ResultNumber::Shown(12), ResultNumber::Shown(-3))
        );
        assert_eq!(b.clear, LampKey::FullCombo);
        let t = parts.timing.unwrap();
        assert_eq!((t.samples, t.mean_x100, t.sd_x100), (4, -75, 12));
        let lines = parts.timelines.unwrap();
        assert_eq!(lines.width(), TimeUs(2_000_000));
        assert_eq!(lines.bins().len(), 3);
        assert_eq!(lines.bins()[1][4], 2);
        assert_eq!(lines.notes(), &[5, 6, 7]);
        assert_eq!(lines.uncovered_from(), Some(2));
        for best in [None, Some(BestCompare::Unavailable)] {
            assert_eq!(
                result_parts(&summary(best), &p, &basis()).best,
                BestView::Unavailable
            );
        }
    }

    /// A preset of seven gauges whose emphasized one, the seventh, is
    /// listed in the fifth slot: a survival gauge looking HARD, its
    /// maximum 120 % below the first's 150 %, failing below 30 %. The
    /// others end-threshold gauges at 100 %, clearing at 80 %.
    fn seven_gauges() -> RulePreset {
        let mut p = rules::tapestry();
        let normal = p.gauges[0].clone();
        p.gauges.truncate(1);
        p.gauges[0].max = GaugeValue(15_000_000);
        for i in 1..6 {
            p.gauges.push(rules::GaugeRule {
                id: format!("n{i}"),
                ..normal.clone()
            });
        }
        let mut hard = normal;
        hard.id = "hard".to_owned();
        hard.max = GaugeValue(12_000_000);
        hard.kind = GaugeKind::Survival {
            fails_below: GaugeValue(3_000_000),
        };
        p.gauges.push(hard);
        p
    }

    fn gauge(start: i64, points: &[(i64, i64)]) -> GaugeResult {
        GaugeResult {
            start: GaugeValue(start),
            value: GaugeValue(points.last().map_or(start, |p| p.1)),
            reference: GaugeValue(start),
            failed_at: None,
            cleared: true,
            history: points
                .iter()
                .map(|&(t, v)| GaugePoint {
                    time: TimeUs(t),
                    value: GaugeValue(v),
                    reference: GaugeValue(v),
                })
                .collect(),
        }
    }

    /// C14, through `ResultBasis::of` and `ViewSummary::of`: the
    /// emphasized gauge's trace, the preset's seventh in the fifth slot,
    /// converts with its own rule's maximum (not
    /// the graphs' top), border and look, the covered columns and the
    /// what-if's mark; the deviations, areas and notes per bin carry over
    /// value for value; a view with no gauge has no history.
    #[test]
    fn the_graph_data_converts_value_for_value() {
        let lane = |n| Lane::new(n).unwrap();
        let chart = ChartBuilder::new(150_000)
            .tap(lane(1), TimeUs(1_000_000))
            .tap(lane(1), TimeUs(2_500_000))
            .tap(lane(7), TimeUs(3_000_000))
            .build()
            .unwrap();
        let p = seven_gauges();
        let b = ResultBasis::of(&chart, None);
        let event = JudgeEvent {
            seq: 0,
            time: TimeUs(998_000),
            origin: None,
            kind: JudgeEventKind::Judged {
                lane: lane(1),
                note: 0,
                judgment: Judgment::Perfect,
                deviation: Some(TimeUs(-2_000)),
                release: None,
                combo: 1,
                presses: 1,
            },
        };
        let e = Evaluation {
            events: vec![event],
            tally: eval::Tally::default(),
            gauges: (0..7)
                .map(|i| match i {
                    6 => gauge(
                        10_000_000,
                        &[(1_000_000, 11_000_000), (2_000_000, 4_000_000)],
                    ),
                    _ => gauge(2_000_000, &[(1_000_000, 2_500_000)]),
                })
                .collect(),
            completed: true,
            final_deadline: TimeUs(10_000_000),
        };
        let axis = Axis {
            origin: TimeUs(0),
            end: e.final_deadline,
        };
        let view = |e: &Evaluation, coverage| {
            ViewSummary::of(e, &chart, &p, b, axis, coverage, false, &[], Some("hard"))
        };
        let s = view(&e, Coverage::NoInputAfter(TimeUs(5_000_000)));
        let parts = result_parts(&s, &p, &b);
        let g = parts.gauge.unwrap();
        let trace = s.graphs.traces[4].unwrap();
        assert_eq!((s.graphs.emphasized, trace.gauge), (Some(4), 6));
        assert_ne!(trace.columns, s.graphs.traces[0].unwrap().columns);
        let want = trace.columns.map(|c| GaugeColumn {
            low: c.low,
            high: c.high,
            last: c.last,
        });
        assert_eq!(g.columns, want);
        assert_eq!(g.columns[28].low, GaugeValue(10_000_000));
        assert_eq!(g.columns[28].high, GaugeValue(11_000_000));
        assert_eq!(g.columns[57].last, GaugeValue(4_000_000));
        assert_eq!(s.graphs.top, GaugeValue(15_000_000));
        assert_eq!(
            (g.max, g.border, g.look),
            (
                GaugeValue(12_000_000),
                GaugeValue(3_000_000),
                GaugeLook::Hard
            )
        );
        assert_eq!(
            (g.reached, g.no_input_from),
            (GRAPH_COLUMNS as u16, Some(144))
        );
        let mut deviations = [0; skin_model::DEVIATION_SLOTS];
        deviations[152] = 1;
        assert_eq!(*parts.deviations, deviations);
        assert_eq!(parts.timing_areas, s.figures.areas);
        assert_eq!(parts.timing_areas, game::judge_areas(&chart, &p));
        assert_ne!(parts.timing_areas, [[0; 2]; 5]);
        let lines = parts.timelines.unwrap();
        assert_eq!(lines.notes(), &[0, 1, 1, 1]);
        assert_eq!(lines.bins().len(), 4);
        // Ended midway at 4 s: the columns through 4 s's, no what-if mark.
        let s = view(&e, Coverage::EndedMidway(TimeUs(4_000_000)));
        let g = result_parts(&s, &p, &b).gauge.unwrap();
        assert_eq!((g.reached, g.no_input_from), (116, None));
        // No gauge: no history, whatever else the view has.
        let none = Evaluation {
            gauges: Vec::new(),
            ..e
        };
        let parts = result_parts(&view(&none, Coverage::Recorded), &p, &b);
        assert_eq!(parts.gauge, None);
        assert_eq!(*parts.deviations, deviations);
    }
}
