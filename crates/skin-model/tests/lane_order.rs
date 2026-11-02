//! The order notes of different lanes are painted in (`NoteOrder`): lane
//! by lane, or merged by their heads under a timeline order, for both
//! lane styles, over coincident key lanes, within the shared budgets, and
//! the order's load check.
//!
//! Every key lane is coincident at 256, 64 wide, the scratch at 160; each
//! lane's tap image is its own ([`TAPS`]), so a quad names its lane.

mod lanes_support;
mod play_support;

use bms_model::{Chart, ChartBuilder, Position, TimeUs};
use eval::Section;
use lanes_support::{
    BAR, BODY_OFF, BODY_ON, HEAD, LINE, MINE, NOTE, TAIL, lanes_element, lanes_skin, simple,
    skin_with, source,
};
use play_support::{Harness, MS, REGIONS, full, lane};
use skin_model::{
    Attempt, LANE_QUADS, LaneStyle, LanesDef, LanesError, LoadError, NoteOrder, PlayContext,
};

/// 150 BPM, in thousandths.
const BPM: i64 = 150_000;

/// Each lane's tap image, by lane index.
const TAPS: [u32; 8] = [NOTE, HEAD, TAIL, BODY_ON, BODY_OFF, MINE, BAR, LINE];

/// `base` with every key coincident, a distinct tap image per lane, no
/// bar group or line, under `order`.
fn coincident(base: LanesDef, order: NoteOrder) -> LanesDef {
    let mut def = base;
    for lane in def.lanes.iter_mut().skip(1) {
        *lane = [256, 64];
    }
    def.parts.note = TAPS;
    def.parts.line = None;
    def.bar_groups.clear();
    def.note_order = order;
    def
}

/// The lane of each quad drawn, by its image, in draw order.
fn lanes_drawn(h: &Harness) -> Vec<usize> {
    let keys: Vec<([u32; 2], u16)> = TAPS
        .iter()
        .map(|&i| {
            let f = h.uvs.get(i).unwrap();
            ([f.uv[0].to_bits(), f.uv[2].to_bits()], f.layer)
        })
        .collect();
    h.batch
        .quads()
        .iter()
        .map(|q| {
            let key = ([q.uv[0].to_bits(), q.uv[2].to_bits()], q.layer);
            keys.iter().position(|&k| k == key).unwrap()
        })
        .collect()
}

/// The lanes of the notes a frame paints under `order`, enumerated from
/// `chart` outside the build: every note of `notes` (the frame's
/// candidates, all in view) inside `section`, sorted lane by lane by
/// `(lane, index)`, or under a timeline order by the exact integer key
/// `(position, time, rank, index in the lane)`.
fn expected(chart: &Chart, order: NoteOrder, section: Option<(TimeUs, TimeUs)>) -> Vec<usize> {
    let mut cands: Vec<(Position, TimeUs, usize, usize, usize)> = Vec::new();
    for (l, notes) in chart.lanes.iter().enumerate() {
        for (i, n) in notes.iter().enumerate() {
            if section.is_none_or(|(s, e)| s <= n.time && n.time < e) {
                let rank = match order {
                    NoteOrder::LaneMajor => l,
                    NoteOrder::Timeline { lanes } => {
                        lanes.iter().position(|&x| usize::from(x) == l).unwrap()
                    }
                };
                cands.push((n.pos, n.time, rank, i, l));
            }
        }
    }
    match order {
        NoteOrder::LaneMajor => cands.sort_by_key(|&(.., rank, i, _)| (rank, i)),
        NoteOrder::Timeline { .. } => cands.sort_by_key(|&(p, t, rank, i, _)| (p, t, rank, i)),
    }
    cands.into_iter().map(|c| c.4).collect()
}

const ORDERS: [NoteOrder; 3] = [
    NoteOrder::LaneMajor,
    NoteOrder::SOURCE,
    NoteOrder::Timeline {
        lanes: [7, 6, 5, 4, 3, 2, 1, 0],
    },
];

/// Taps from 1 100 to 1 450 ms, all in view at 1 000 ms, with the keys'
/// collisions: lane 1's and lane 2's at one µs, lane 1's one position
/// later; lane 4's a µs after the chord of lanes 0, 3 and 7 at 1 300 ms,
/// at its position; lane 5 empty; lane 0's first and lane 6's last
/// outside the section of [`SECTION`].
fn collisions() -> Chart {
    let at = |ms: i64| TimeUs(ms * MS);
    let mut chart = ChartBuilder::new(BPM)
        .tap(lane(0), at(1_100))
        .tap(lane(0), at(1_300))
        .tap(lane(0), at(1_350))
        .tap(lane(1), at(1_250))
        .tap(lane(2), at(1_250))
        .tap(lane(2), at(1_350))
        .tap(lane(3), at(1_300))
        .tap(lane(4), TimeUs(1_300 * MS + 1))
        .tap(lane(6), at(1_150))
        .tap(lane(6), at(1_350))
        .tap(lane(6), at(1_450))
        .tap(lane(7), at(1_300))
        .tap(lane(7), at(1_350))
        .build()
        .unwrap();
    let pos = |chart: &Chart, l: usize, i: usize| chart.lanes[l][i].pos;
    chart.lanes[1][0].pos = Position(pos(&chart, 2, 0).0 + 1);
    chart.lanes[4][0].pos = pos(&chart, 3, 0);
    chart
}

/// The section the oracle's sectioned frames play: from 1 200 to
/// 1 400 ms.
const SECTION: (i64, i64) = (1_200, 1_400);

#[test]
fn notes_are_painted_in_the_order_s_exact_sequence() {
    // Both styles in every order, the whole chart and a section: every
    // quad's image, so its lane, in the oracle's sequence, no note left
    // out or added.
    let section = Section {
        start: TimeUs(SECTION.0 * MS),
        end: TimeUs(SECTION.1 * MS),
        lead_in: TimeUs(500 * MS),
        initial_gauges: vec![rules::GaugeValue::FULL; 2],
    };
    let sectioned = PlayContext {
        attempt: Attempt::Section(&section),
        ..full(0)
    };
    let chart = collisions();
    for style in [simple(), source()] {
        let name = match style.style {
            LaneStyle::Simple => "simple",
            LaneStyle::Source(_) => "source",
        };
        for order in ORDERS {
            for (ctx, bounds) in [
                (full(0), None),
                (sectioned, Some((section.start, section.end))),
            ] {
                let mut h =
                    Harness::new(lanes_skin(coincident(style.clone(), order)), &chart, &ctx);
                h.frame(1_000, &[]);
                let want = expected(&chart, order, bounds);
                assert_eq!(
                    lanes_drawn(&h),
                    want,
                    "{name} {order:?} section {}",
                    bounds.is_some()
                );
            }
        }
    }
    // The collisions decide: under the source's order lane 2 before lane
    // 1 (an earlier position at one µs), lane 4 after the chord (a later
    // µs at one position).
    let want = expected(&chart, NoteOrder::SOURCE, None);
    assert_eq!(want, [0, 6, 2, 1, 3, 7, 0, 4, 2, 6, 7, 0, 6]);
}

#[test]
fn a_chord_follows_the_order_s_lane_ranks() {
    // One tap on every lane at 1 000 ms: the ranks, whatever the lanes'
    // x order.
    let chord = (0..8_u8)
        .fold(ChartBuilder::new(BPM), |b, l| {
            b.tap(lane(l), TimeUs(1_000 * MS))
        })
        .build()
        .unwrap();
    for (order, want) in [
        (NoteOrder::LaneMajor, [0, 1, 2, 3, 4, 5, 6, 7]),
        (NoteOrder::SOURCE, [1, 2, 3, 4, 5, 6, 7, 0]),
        (
            NoteOrder::Timeline {
                lanes: [7, 6, 5, 4, 3, 2, 1, 0],
            },
            [7, 6, 5, 4, 3, 2, 1, 0],
        ),
    ] {
        for style in [simple(), source()] {
            let mut h = Harness::new(lanes_skin(coincident(style, order)), &chord, &full(0));
            h.frame(900, &[]);
            assert_eq!(lanes_drawn(&h), want, "{order:?}");
        }
    }
}

#[test]
fn a_long_note_s_parts_stay_together_at_its_head() {
    // A long note on lane 2 from 1 000 to 1 300 ms and a tap on lane 1 at
    // 1 100 ms: body, tail and head, then the tap, under a timeline
    // order; lane by lane, the tap first.
    let chart = ChartBuilder::new(BPM)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
        .tap(lane(1), TimeUs(1_100 * MS))
        .build()
        .unwrap();
    let mut def = coincident(simple(), NoteOrder::SOURCE);
    def.parts.note = [TAIL; 8];
    for (order, want) in [
        (NoteOrder::SOURCE, [BODY_OFF, TAIL, HEAD, TAIL]),
        (NoteOrder::LaneMajor, [TAIL, BODY_OFF, TAIL, HEAD]),
    ] {
        def.note_order = order;
        let mut h = Harness::new(lanes_skin(def.clone()), &chart, &full(0));
        h.frame(900, &[]);
        let images: Vec<u32> = h
            .batch
            .quads()
            .iter()
            .map(|q| {
                [BODY_OFF, TAIL, HEAD]
                    .into_iter()
                    .find(|&i| {
                        let f = h.uvs.get(i).unwrap();
                        [f.uv[0], f.uv[2]] == [q.uv[0], q.uv[2]] && f.layer == q.layer
                    })
                    .unwrap()
            })
            .collect();
        assert_eq!(images, want, "{order:?}");
    }
}

/// `n` chords of eight taps from 1 000 ms, `step` µs apart.
fn dense(n: i64, step: i64) -> Chart {
    let mut b = ChartBuilder::new(BPM);
    for k in 0..n {
        for l in 0..8 {
            b = b.tap(lane(l), TimeUs(1_000 * MS + k * step));
        }
    }
    b.build().unwrap()
}

#[test]
fn the_quad_budget_keeps_the_order_s_prefix() {
    // 1 100 chords within 400 ms: 8 800 taps, all in view, past the
    // 8 192 quads. A timeline order draws the first 1 024 chords whole,
    // in rank order; lane by lane, every tap of each lane in turn up to
    // the budget;
    // one drop either way.
    let chart = dense(1_100, 360);
    for order in ORDERS {
        let mut h = Harness::new(lanes_skin(coincident(simple(), order)), &chart, &full(0));
        h.frame(900, &[]);
        let lanes = lanes_drawn(&h);
        assert_eq!(lanes.len(), LANE_QUADS as usize, "{order:?}");
        assert_eq!(h.batch.dropped(), 1, "{order:?}");
        let want: Vec<usize> = match order {
            NoteOrder::LaneMajor => (0..8).flat_map(|l| [l; 1_100]).take(8_192).collect(),
            NoteOrder::Timeline { lanes } => {
                (0..1_024).flat_map(|_| lanes.map(usize::from)).collect()
            }
        };
        assert_eq!(lanes, want, "{order:?}");
        // The oracle's sequence cut at the budget.
        assert_eq!(
            lanes,
            expected(&chart, order, None)[..LANE_QUADS as usize],
            "{order:?}"
        );
    }
}

#[test]
fn a_long_note_may_be_cut_between_its_parts() {
    // 8 190 taps then a long note: its body and tail fit, its head is
    // dropped; with one tap more only its body fits; with exactly the
    // budget's taps and nothing after, no drop.
    let run = |taps: i64, long: bool| {
        let mut b = ChartBuilder::new(BPM);
        for k in 0..taps {
            b = b.tap(
                lane(u8::try_from(k % 8).unwrap()),
                TimeUs(1_000 * MS + (k / 8) * 300),
            );
        }
        if long {
            b = b.long(lane(1), TimeUs(1_399 * MS), TimeUs(1_450 * MS));
        }
        let mut def = coincident(simple(), NoteOrder::SOURCE);
        def.parts.note = [NOTE; 8];
        let mut h = Harness::new(lanes_skin(def), &b.build().unwrap(), &full(0));
        h.frame(900, &[]);
        let count = |image: u32| {
            let f = h.uvs.get(image).unwrap();
            h.batch
                .quads()
                .iter()
                .filter(|q| [q.uv[0], q.uv[2]] == [f.uv[0], f.uv[2]] && q.layer == f.layer)
                .count()
        };
        (
            [count(NOTE), count(BODY_OFF), count(TAIL), count(HEAD)],
            h.batch.dropped(),
        )
    };
    assert_eq!(run(8_190, true), ([8_190, 1, 1, 0], 1));
    assert_eq!(run(8_191, true), ([8_191, 1, 0, 0], 1));
    assert_eq!(run(8_192, false), ([8_192, 0, 0, 0], 0));
}

#[test]
fn a_timeline_order_lists_every_lane_once() {
    let err = |lanes: [u8; 8]| {
        let mut def = simple();
        def.note_order = NoteOrder::Timeline { lanes };
        match skin_with(REGIONS, Some(def), vec![lanes_element()]) {
            Ok(_) => None,
            Err(LoadError::Lanes(e)) => Some(e),
            Err(e) => panic!("{e:?}"),
        }
    };
    assert_eq!(err([1, 2, 3, 4, 5, 6, 7, 0]), None);
    assert_eq!(err([7, 6, 5, 4, 3, 2, 1, 0]), None);
    for lanes in [
        [1, 1, 3, 4, 5, 6, 7, 0],
        [1, 2, 3, 4, 5, 6, 7, 8],
        [0, 1, 2, 3, 4, 5, 6, 255],
        [0; 8],
    ] {
        assert_eq!(err(lanes), Some(LanesError::NoteOrder), "{lanes:?}");
    }
}
