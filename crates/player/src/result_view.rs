//! A result view's projections as a result skin reads them (design.md
//! "Personal Bests", skin projections): `game`'s figures, timing
//! statistics, timelines and best comparison converted into `skin_model`'s
//! result types, with each lamp given its gauge's look under the view's
//! preset. `skin_model` holds no `game` type and `game` no library type,
//! so the conversion is the player's.
// S10-09i installs the result view; until then only the tests read it.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "S10-09i installs the result view")
)]

use game::{BestCompare, BestLamp, Rate, ViewSummary};
use rules::RulePreset;
use skin_model::{
    BestFigures, BestRate, BestView, GaugeLook, LampKey, OwnLamp, ResultNumber, ResultRate,
    ResultScore, RowScore, Timelines, TimingStats,
};

/// The parts of a ready view a [`skin_model::ResultView`] carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultParts {
    pub score: ResultScore,
    pub best: BestView,
    pub timing: Option<TimingStats>,
    pub timelines: Option<Timelines>,
}

/// The parts of `summary`, a view under `preset`. A best not yet read is
/// unavailable.
pub fn result_parts(summary: &ViewSummary, preset: &RulePreset) -> ResultParts {
    let f = &summary.figures;
    let t = &summary.graphs.timelines;
    ResultParts {
        score: ResultScore {
            tally: summary.tally,
            notes: f.notes,
            gauge_permille: f.gauge_permille,
            clear: lamp_key(Some(f.clear), preset),
            cleared: f.cleared,
            rate: f.rate.map(rate),
            next_rank: f.next_rank,
        },
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
        timelines: Timelines::new(t.axis.width, t.used().to_vec(), t.uncovered_from).ok(),
    }
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

    use game::{
        BestCompare, BestFigures, BestLamp, BestRate, Rate, ResultFigures, ResultGraphs,
        SHOWN_GAUGES, TimelineAxis, ViewSummary,
    };
    use replay::verify::Coverage;
    use skin_model::{BestView, LampKey, ResultNumber, ResultRate};

    use super::{lamp_key, result_parts};

    fn summary(best: Option<BestCompare>) -> ViewSummary {
        let mut graphs = ResultGraphs::EMPTY;
        graphs.timelines.axis = TimelineAxis {
            origin: bms_model::TimeUs(0),
            width: bms_model::TimeUs(2_000_000),
            count: 3,
        };
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
        let parts = result_parts(&summary(Some(BestCompare::Known(known))), &p);
        assert_eq!(parts.score.notes, 600);
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
        let BestView::Known(b) = result_parts(&summary(Some(BestCompare::Known(shown))), &p).best
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
        assert_eq!(lines.width(), bms_model::TimeUs(2_000_000));
        assert_eq!(lines.bins().len(), 3);
        assert_eq!(lines.bins()[1][4], 2);
        assert_eq!(lines.uncovered_from(), Some(2));
        for best in [None, Some(BestCompare::Unavailable)] {
            assert_eq!(result_parts(&summary(best), &p).best, BestView::Unavailable);
        }
    }
}
