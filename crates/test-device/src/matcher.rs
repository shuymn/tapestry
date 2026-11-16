//! The comparisons behind the timing checks (docs/test-device.md
//! "Verdicts"): pure functions over what the pattern sent, what the trace
//! and the listener saw and what the player recorded, each failing with
//! the first thing that breaks its rule.

use std::collections::HashMap;

use crate::protocol::Control;

/// An edge someone observed: a control going down or up at a host time
/// (µs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub control: Control,
    pub down: bool,
    pub time: i64,
}

/// What one report changed: a set of edges, in no particular order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub edges: Vec<(Control, bool)>,
}

/// Where each sent edge was observed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Matching {
    /// For sent edge k (numbered across reports in order), the observed
    /// edge's index.
    pub observed: Vec<usize>,
    /// For sent edge k, its report's index.
    pub report: Vec<usize>,
    /// Observed makes of a key already down: typematic repeats, which no
    /// report sent.
    pub repeats: Vec<usize>,
}

/// Why the observed edges are not the sent ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeError {
    /// Sent edge `edge` was never observed where it belonged.
    Missing { edge: usize },
    /// Observed edge `observed` was never sent.
    Extra { observed: usize },
}

/// Matches observed edges to the sent reports (docs/test-device.md
/// "Keyboard edges"): across reports order holds; within a report the
/// edges are a set. With `repeats` (a keyboard), a make of a key already
/// down is a typematic repeat and is set aside; on a controller it is an
/// extra edge.
///
/// # Errors
///
/// The first missing or extra edge.
pub fn match_edges(
    sent: &[Report],
    observed: &[Edge],
    repeats: bool,
) -> Result<Matching, EdgeError> {
    let mut first_edge = Vec::with_capacity(sent.len());
    let mut total = 0;
    for r in sent {
        first_edge.push(total);
        total += r.edges.len();
    }
    let mut m = Matching {
        observed: vec![usize::MAX; total],
        report: sent
            .iter()
            .enumerate()
            .flat_map(|(i, r)| vec![i; r.edges.len()])
            .collect(),
        repeats: Vec::new(),
    };
    let mut down: HashMap<Control, bool> = HashMap::new();
    let mut current = 0;
    let unmatched_in = |m: &Matching, r: usize| {
        let start = first_edge.get(r).copied().unwrap_or(total);
        let len = sent.get(r).map_or(0, |rep| rep.edges.len());
        (start..start + len).find(|&k| m.observed.get(k) == Some(&usize::MAX))
    };
    for (i, e) in observed.iter().enumerate() {
        let is_down = down.get(&e.control).copied().unwrap_or(false);
        if repeats && e.down && is_down {
            m.repeats.push(i);
            continue;
        }
        loop {
            let Some(report) = sent.get(current) else {
                return Err(EdgeError::Extra { observed: i });
            };
            let start = first_edge.get(current).copied().unwrap_or(total);
            let hit = report.edges.iter().enumerate().find(|&(j, &(c, d))| {
                c == e.control && d == e.down && m.observed.get(start + j) == Some(&usize::MAX)
            });
            if let Some((j, _)) = hit {
                if let Some(slot) = m.observed.get_mut(start + j) {
                    *slot = i;
                }
                down.insert(e.control, e.down);
                break;
            }
            match unmatched_in(&m, current) {
                None => current += 1,
                Some(edge) => {
                    let later = sent
                        .iter()
                        .skip(current + 1)
                        .any(|r| r.edges.contains(&(e.control, e.down)));
                    return Err(if later {
                        EdgeError::Missing { edge }
                    } else {
                        EdgeError::Extra { observed: i }
                    });
                }
            }
        }
    }
    match m.observed.iter().position(|&o| o == usize::MAX) {
        Some(edge) => Err(EdgeError::Missing { edge }),
        None => Ok(m),
    }
}

/// U2's necessary condition: the edges of one report share one time.
///
/// # Errors
///
/// The first report whose edges have different times.
pub fn check_batches(m: &Matching, observed: &[Edge]) -> Result<(), usize> {
    let mut time_of: HashMap<usize, i64> = HashMap::new();
    for (&o, &r) in m.observed.iter().zip(&m.report) {
        let t = observed.get(o).map_or(i64::MIN, |e| e.time);
        if *time_of.entry(r).or_insert(t) != t {
            return Err(r);
        }
    }
    Ok(())
}

/// A pair of intervals whose difference broke U1(b)'s bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spacing {
    /// The interval between sent edges `index` and `index + 1`.
    pub index: usize,
    pub recorded: i64,
    pub handed: i64,
}

/// U1(b): each interval between consecutive recorded edges is within
/// `tolerance` µs of the interval between their handoffs (device µs,
/// unscaled: the rate error over one interval is far below a µs).
///
/// # Errors
///
/// The first interval beyond the tolerance.
pub fn check_spacing(recorded: &[i64], handoffs: &[i64], tolerance: i64) -> Result<(), Spacing> {
    let gaps = |v: &[i64]| v.windows(2).map(|w| w[1] - w[0]).collect::<Vec<_>>();
    for (index, (r, h)) in gaps(recorded).into_iter().zip(gaps(handoffs)).enumerate() {
        if (r - h).abs() > tolerance {
            return Err(Spacing {
                index,
                recorded: r,
                handed: h,
            });
        }
    }
    Ok(())
}

/// U1(c)'s differential: player stamp − listener stamp for each report,
/// both host µs.
#[must_use]
pub fn differential(player: &[i64], listener: &[i64]) -> Vec<i64> {
    player.iter().zip(listener).map(|(p, l)| p - l).collect()
}

/// The spread of a differential: p50, p99 and the largest magnitude.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spread {
    pub p50: i64,
    pub p99: i64,
    pub max_abs: i64,
}

/// The differential's spread (nearest-rank percentiles).
#[must_use]
pub fn spread(d: &[i64]) -> Spread {
    let mut s = d.to_vec();
    s.sort_unstable();
    let rank = |p: usize| {
        let i = (p * s.len()).div_ceil(100).saturating_sub(1);
        s.get(i).copied().unwrap_or(0)
    };
    Spread {
        p50: rank(50),
        p99: rank(99),
        max_abs: d.iter().map(|x| x.abs()).max().unwrap_or(0),
    }
}

/// U1(c)'s acceptance target, two-sided: |d| ≤ `limit` for every report.
///
/// # Errors
///
/// The first report beyond it and its d.
pub fn check_differential(d: &[i64], limit: i64) -> Result<(), (usize, i64)> {
    match d.iter().position(|x| x.abs() > limit) {
        Some(i) => Err((i, d.get(i).copied().unwrap_or(0))),
        None => Ok(()),
    }
}

/// An interval of host µs, both ends included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub lo: i64,
    pub hi: i64,
}

/// receipt − map(C) for each report: the receipt (host µs) minus the
/// mapped completion interval (from [`crate::clock::Bounds::map`]).
#[must_use]
pub fn since_completion(receipts: &[i64], completions: &[(i64, i64)]) -> Vec<Interval> {
    receipts
        .iter()
        .zip(completions)
        .map(|(&r, &(lo, hi))| Interval {
            lo: r - hi,
            hi: r - lo,
        })
        .collect()
}

/// Why a set of intervals fails a range check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeError {
    /// Interval `index` misses the range.
    Outside { index: usize, interval: Interval },
    /// Interval `index` is as wide as the range or wider, so it cannot
    /// reject a wrong pairing.
    Inconclusive { index: usize, width: i64 },
}

/// Every interval meets [`lo`, `hi`]: F4's latency check (0 to 20 ms).
///
/// # Errors
///
/// The first interval outside.
pub fn check_within(intervals: &[Interval], lo: i64, hi: i64) -> Result<(), RangeError> {
    match intervals.iter().position(|i| i.hi < lo || i.lo > hi) {
        Some(index) => Err(RangeError::Outside {
            index,
            interval: intervals[index],
        }),
        None => Ok(()),
    }
}

/// F-S's pairing check: every receipt − map(C) interval meets [0, `l`],
/// and is narrower than `l`, so pairing a report with the completion of
/// the one before it (one interval earlier) would miss.
///
/// # Errors
///
/// The first inconclusive interval, else the first one outside.
pub fn check_pairing(intervals: &[Interval], l: i64) -> Result<(), RangeError> {
    if let Some(index) = intervals.iter().position(|i| i.hi - i.lo >= l) {
        let i = intervals[index];
        return Err(RangeError::Inconclusive {
            index,
            width: i.hi - i.lo,
        });
    }
    check_within(intervals, 0, l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Bounds;
    use crate::clock::tests::{DEVICE0, Truth};

    fn tap(control: Control, n: usize) -> Vec<Report> {
        (0..n)
            .flat_map(|_| {
                [
                    Report {
                        edges: vec![(control, true)],
                    },
                    Report {
                        edges: vec![(control, false)],
                    },
                ]
            })
            .collect()
    }

    /// Lanes 1 to 7 in turn, press then release, `n` times each.
    fn lanes(n: usize) -> Vec<Report> {
        (0..n)
            .flat_map(|i| {
                let b = Control::Button(u8::try_from(i % 7).unwrap() + 1);
                [
                    Report {
                        edges: vec![(b, true)],
                    },
                    Report {
                        edges: vec![(b, false)],
                    },
                ]
            })
            .collect()
    }

    /// What a listener sees when every report arrives 125 ms apart.
    fn seen(sent: &[Report]) -> Vec<Edge> {
        sent.iter()
            .zip(0..)
            .flat_map(|(r, i)| {
                r.edges.iter().map(move |&(control, down)| Edge {
                    control,
                    down,
                    time: i * 125_000,
                })
            })
            .collect()
    }

    #[test]
    fn the_sent_edges_match_in_order() {
        let sent = lanes(64);
        let m = match_edges(&sent, &seen(&sent), false).unwrap();
        assert_eq!(m.observed, (0..128).collect::<Vec<_>>());
        assert!(m.repeats.is_empty());
    }

    /// U1(a)'s negative: the device never sent edge 37.
    #[test]
    fn a_dropped_edge_fails_at_that_edge() {
        let sent = lanes(64);
        let mut obs = seen(&sent);
        obs.remove(37);
        assert_eq!(
            match_edges(&sent, &obs, false),
            Err(EdgeError::Missing { edge: 37 })
        );
    }

    /// U1(a)'s negative: a pattern with edges 37 and 38 swapped.
    #[test]
    fn swapped_edges_fail_where_they_swap() {
        let sent = lanes(64);
        let mut obs = seen(&sent);
        obs.swap(37, 38);
        assert_eq!(
            match_edges(&sent, &obs, false),
            Err(EdgeError::Missing { edge: 37 })
        );
    }

    /// U4's negatives: a recorded edge deleted, an extra press inserted.
    #[test]
    fn a_deleted_or_inserted_keyboard_edge_fails() {
        let w = Control::Key(0x1A);
        let sent = tap(w, 32);
        let mut deleted = seen(&sent);
        deleted.remove(20);
        assert_eq!(
            match_edges(&sent, &deleted, true),
            Err(EdgeError::Missing { edge: 20 })
        );
        let mut extra = seen(&sent);
        extra.insert(
            21,
            Edge {
                control: Control::Key(0x08),
                down: true,
                time: 0,
            },
        );
        assert_eq!(
            match_edges(&sent, &extra, true),
            Err(EdgeError::Extra { observed: 21 })
        );
    }

    /// U4r: a long hold's repeats are makes of a key already down, set
    /// aside as repeats; on a controller the same make is an extra edge.
    #[test]
    fn keyboard_repeats_are_classed_apart() {
        let w = Control::Key(0x1A);
        let sent = tap(w, 1);
        let mut obs = seen(&sent);
        let repeat = Edge {
            control: w,
            down: true,
            time: 600_000,
        };
        obs.splice(1..1, [repeat, repeat]);
        let m = match_edges(&sent, &obs, true).unwrap();
        assert_eq!(m.repeats, [1, 2]);
        assert_eq!(m.observed, [0, 3]);
        let b = Control::Button(1);
        let pad: Vec<Edge> = obs.iter().map(|e| Edge { control: b, ..*e }).collect();
        assert_eq!(
            match_edges(&tap(b, 1), &pad, false),
            Err(EdgeError::Extra { observed: 1 })
        );
    }

    /// Within a report the edges are a set: any order matches.
    #[test]
    fn edges_within_a_report_match_in_any_order() {
        let chord: Vec<(Control, bool)> = (1..=7).map(|n| (Control::Button(n), true)).collect();
        let sent = vec![Report {
            edges: chord.clone(),
        }];
        let mut obs = seen(&sent);
        obs.reverse();
        let m = match_edges(&sent, &obs, false).unwrap();
        assert_eq!(m.observed, [6, 5, 4, 3, 2, 1, 0]);
        assert_eq!(check_batches(&m, &obs), Ok(()));
    }

    /// U2's negative: one recorded time inside a 7-edge report changed.
    #[test]
    fn a_report_whose_edges_differ_in_time_fails() {
        let chord = |down| (1..=7).map(|n| (Control::Button(n), down)).collect();
        let sent: Vec<Report> = (0..16)
            .map(|i| Report {
                edges: chord(i % 2 == 0),
            })
            .collect();
        let mut obs = seen(&sent);
        let m = match_edges(&sent, &obs, false).unwrap();
        assert_eq!(check_batches(&m, &obs), Ok(()));
        obs[7 * 5 + 3].time += 1;
        assert_eq!(check_batches(&m, &obs), Err(5));
    }

    #[test]
    fn spacing_is_held_to_its_tolerance() {
        let handed: Vec<i64> = (0..10).map(|i| i * 125_000).collect();
        let mut recorded: Vec<i64> = handed.iter().map(|h| h + 3_000_000).collect();
        assert_eq!(check_spacing(&recorded, &handed, 1_125), Ok(()));
        recorded[4] += 1_126;
        assert_eq!(
            check_spacing(&recorded, &handed, 1_125),
            Err(Spacing {
                index: 3,
                recorded: 126_126,
                handed: 125_000
            })
        );
    }

    /// U1(c)'s negatives: one player event 2 ms late, or 2 ms early.
    #[test]
    fn the_differential_fails_either_way_beyond_1_ms() {
        let listener: Vec<i64> = (0..128).map(|i| i * 125_000).collect();
        let player: Vec<i64> = listener.iter().map(|l| l + 150).collect();
        let d = differential(&player, &listener);
        assert_eq!(check_differential(&d, 1_000), Ok(()));
        assert_eq!(
            spread(&d),
            Spread {
                p50: 150,
                p99: 150,
                max_abs: 150
            }
        );
        for shift in [2_000, -2_000] {
            let mut p = player.clone();
            p[40] += shift;
            let d = differential(&p, &listener);
            assert_eq!(check_differential(&d, 1_000), Err((40, 150 + shift)));
        }
    }

    /// 500 completions over the run, each received 1 ms after it.
    fn fs_run() -> (Truth, Bounds, Vec<u64>, Vec<i64>) {
        let truth = Truth { k: 1 };
        let bounds = Bounds::fit(&truth.pings(1200, |_| (150, 150))).unwrap();
        let completions: Vec<u64> = (0..500)
            .map(|k| DEVICE0 + 1_000_000 + k * 240_000)
            .collect();
        let receipts = completions.iter().map(|&c| truth.host(c) + 1_000).collect();
        (truth, bounds, completions, receipts)
    }

    #[test]
    fn a_completion_paired_with_its_report_lands_in_0_to_l() {
        let (_, bounds, completions, receipts) = fs_run();
        let mapped: Vec<_> = completions.iter().map(|&c| bounds.map(c)).collect();
        let intervals = since_completion(&receipts, &mapped);
        assert_eq!(check_pairing(&intervals, 4_000), Ok(()));
        assert_eq!(check_within(&intervals, 0, 20_000), Ok(()));
    }

    /// F-S's comparator mutation: report k paired with C(k−1), one 125 Hz
    /// interval earlier, moves every interval up by 8 ms.
    #[test]
    fn pairing_with_the_previous_completion_misses() {
        let (_, bounds, completions, receipts) = fs_run();
        let earlier: Vec<_> = completions.iter().map(|&c| bounds.map(c - 8_000)).collect();
        let intervals = since_completion(&receipts, &earlier);
        assert!(matches!(
            check_pairing(&intervals, 4_000),
            Err(RangeError::Outside { index: 0, interval }) if interval.lo > 4_000
        ));
    }

    #[test]
    fn a_mapping_as_wide_as_l_is_inconclusive() {
        let wide = [Interval {
            lo: -1_000,
            hi: 3_000,
        }];
        assert_eq!(
            check_pairing(&wide, 4_000),
            Err(RangeError::Inconclusive {
                index: 0,
                width: 4_000
            })
        );
    }

    /// F4's negative: every pong 50 ms early makes the mapping 50 ms late,
    /// so every interval ends below 0.
    #[test]
    fn pongs_50_ms_early_put_every_interval_below_zero() {
        let (truth, _, completions, receipts) = fs_run();
        let mut pings = truth.pings(1200, |_| (150, 150));
        for p in &mut pings {
            p.device -= 50_000;
        }
        let bounds = Bounds::fit(&pings).unwrap();
        let mapped: Vec<_> = completions.iter().map(|&c| bounds.map(c)).collect();
        let intervals = since_completion(&receipts, &mapped);
        assert!(intervals.iter().all(|i| i.hi < 0));
        assert!(matches!(
            check_within(&intervals, 0, 20_000),
            Err(RangeError::Outside { index: 0, .. })
        ));
    }
}
