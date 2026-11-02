//! The order notes of different lanes are painted in (`NoteOrder`): lane
//! by lane, or merged by their heads under a timeline order, for both
//! lane styles, over coincident key lanes, within the shared budgets, and
//! the order's load check.
//!
//! Every key lane is coincident at 256, 64 wide, the scratch at 160; each
//! lane's tap image is its own ([`TAPS`]), so a quad names its lane.

mod lanes_support;
mod play_support;

use bms_model::{Chart, ChartBuilder, TimeUs};
use lanes_support::{
    BAR, BODY_OFF, BODY_ON, HEAD, LINE, MINE, NOTE, TAIL, lanes_element, lanes_skin, simple,
    skin_with, source,
};
use play_support::{Harness, MS, REGIONS, full, lane};
use skin_model::{LANE_QUADS, LaneStyle, LanesDef, LanesError, LoadError, NoteOrder};

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

/// The drawn quads as `(lane, y)`, in draw order.
fn drawn(h: &Harness) -> Vec<(usize, f32)> {
    lanes_drawn(h)
        .into_iter()
        .zip(h.batch.quads())
        .map(|(l, q)| (l, q.rect[1]))
        .collect()
}

/// Whether `seq` follows `order`: lane by lane in index order with each
/// lane's notes from the line up, or every note from the line up (a
/// later note is higher, so `y` falls), ties by the lane's rank.
fn ordered(seq: &[(usize, f32)], order: NoteOrder) -> bool {
    match order {
        NoteOrder::LaneMajor => seq
            .windows(2)
            .all(|w| w[0].0 < w[1].0 || (w[0].0 == w[1].0 && w[0].1 > w[1].1)),
        NoteOrder::Timeline { lanes } => {
            let rank = |l: usize| lanes.iter().position(|&x| usize::from(x) == l).unwrap();
            seq.windows(2)
                .all(|w| w[0].1 > w[1].1 || (w[0].1 == w[1].1 && rank(w[0].0) < rank(w[1].0)))
        }
    }
}

/// Taps over every lane: at each of 30 times 50 ms apart from 1 000 ms,
/// three lanes, a different three each time.
fn chords() -> Chart {
    let mut b = ChartBuilder::new(BPM);
    for k in 0..90_u16 {
        let t = 1_000 * MS + i64::from(k / 3) * 50 * MS;
        b = b.tap(lane(u8::try_from((3 * k + k / 24) % 8).unwrap()), TimeUs(t));
    }
    b.build().unwrap()
}

const ORDERS: [NoteOrder; 3] = [
    NoteOrder::LaneMajor,
    NoteOrder::SOURCE,
    NoteOrder::Timeline {
        lanes: [7, 6, 5, 4, 3, 2, 1, 0],
    },
];

#[test]
fn notes_are_painted_in_the_lanes_note_order() {
    // Both styles in every order: the frame's notes, each drawn once, in
    // the order's sequence; the timeline orders paint a later note of an
    // earlier lane after an earlier note of a later one, which lane by
    // lane does not.
    for style in [simple(), source()] {
        let name = match style.style {
            LaneStyle::Simple => "simple",
            LaneStyle::Source(_) => "source",
        };
        let mut seen = Vec::new();
        for order in ORDERS {
            let mut h = Harness::new(
                lanes_skin(coincident(style.clone(), order)),
                &chords(),
                &full(0),
            );
            h.frame(900, &[]);
            let seq = drawn(&h);
            assert!(seq.len() > 20, "{name} {order:?}: {}", seq.len());
            assert!(ordered(&seq, order), "{name} {order:?}: {seq:?}");
            for other in ORDERS.into_iter().filter(|&o| o != order) {
                assert!(!ordered(&seq, other), "{name} {order:?} as {other:?}");
            }
            let mut set = seq.clone();
            set.sort_by(|a, b| a.partial_cmp(b).unwrap());
            seen.push(set);
        }
        // The same notes in every order.
        assert!(seen.windows(2).all(|w| w[0] == w[1]), "{name}");
    }
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
        assert!(ordered(&drawn(&h), order), "{order:?}");
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
