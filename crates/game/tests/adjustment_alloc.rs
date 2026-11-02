//! The adjustment reducer and its publication allocate nothing once built
//! (design.md "Scope of No Allocation"): the first and later inputs, every
//! kind of request and solve, repeats, an axis, a READY phase change, a
//! loss and its resynchronization, full covers, a new attempt and each
//! drain's snapshot. This file holds a single test, so the harness's other
//! threads are not tracked.

mod counting;

use bms_model::{Speed, TimeUs};
use game::adjustment::{
    Adjustment, AxisQuantum, ControlId, ControlRoles, Event, Head, HeldSet, Observation, Role,
    TempoMap, presentation_link,
};
use game::{Cover, Covers, HiSpeedSettings, PlayOptions, Ratio, References, SpeedMode};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

/// 150 BPM, 300 from 5 s, without a heap.
struct Map;

impl TempoMap for Map {
    fn bpm_at(&self, head: TimeUs) -> Option<Ratio> {
        Some(Ratio::integer(if head.0 < 5_000_000 { 150 } else { 300 }))
    }
}

#[test]
fn the_reducer_and_its_publication_allocate_nothing() {
    let mut roles = ControlRoles::default();
    let keys = [
        Role::Start,
        Role::Select,
        Role::AuxNegative,
        Role::AuxPositive,
        Role::ScratchPositive,
        Role::ScratchNegative,
        Role::White,
        Role::Black,
        Role::Slower,
        Role::Faster,
    ];
    for (i, role) in keys.into_iter().enumerate() {
        assert!(roles.set(ControlId(i as u8), role));
    }
    let c = |i: u8| ControlId(i);
    let full = Covers {
        sudden: Cover {
            amount: 1000,
            on: true,
        },
        lift: Cover {
            amount: 1000,
            on: true,
        },
        hidden: Cover {
            amount: 1000,
            on: true,
        },
    };
    let options = PlayOptions::new(HiSpeedSettings::default(), Covers::default()).unwrap();
    let classic = PlayOptions::new(
        HiSpeedSettings {
            mode: SpeedMode::Classic,
            ..HiSpeedSettings::default()
        },
        full,
    )
    .unwrap();
    let mut a = Adjustment::new(options, roles, TimeUs(-2_000_000), 0);
    let (mut w, mut r) = presentation_link(&a.snapshot());
    let quantum = AxisQuantum::wrapping(256).unwrap();
    let mut seq = 0;
    let mut o = |ms: i64, event: Event| {
        seq += 1;
        Observation {
            time: TimeUs(ms * 1_000),
            seq,
            event,
        }
    };
    let script = [
        o(0, Event::Press(c(9))),
        o(
            1,
            Event::Prepare {
                references: References::all(Ratio::integer(150)),
                speed: Speed::FASTEST,
            },
        ),
        o(2, Event::Press(c(0))),
        o(2, Event::Press(c(4))),
        o(3, Event::Tick),
        o(60, Event::Tick),
        o(600, Event::Tick),
        o(601, Event::Press(c(5))),
        o(602, Event::Release(c(4))),
        o(602, Event::Release(c(5))),
        o(603, Event::Press(c(1))),
        o(604, Event::Release(c(0))),
        o(605, Event::Press(c(6))),
        o(605, Event::Press(c(7))),
        o(605, Event::Press(c(3))),
        o(
            606,
            Event::Axis {
                control: c(4),
                delta: 50,
                quantum,
            },
        ),
        o(
            607,
            Event::Axis {
                control: c(4),
                delta: -700,
                quantum,
            },
        ),
        o(608, Event::DisplayOffset(5_000)),
        o(
            609,
            Event::Phase {
                head: Head::Clock {
                    at: TimeUs(609_000),
                    chart: TimeUs(6_000_000),
                },
                generation: 1,
            },
        ),
        o(610, Event::Repeat(c(9))),
        o(611, Event::Lost),
        o(612, Event::Resync(HeldSet::default().with(c(1)))),
        o(613, Event::Release(c(1))),
        o(614, Event::Ownership),
        o(615, Event::Tick),
    ];
    // The first drain is counted too.
    counting::track(true);
    counting::self_check();
    let before = counting::calls();
    for chunk in script.chunks(3) {
        a.drain(chunk.iter().copied(), &Map, &mut w);
        let _ = r.read();
    }
    // Full covers in a new attempt, Classic: a cover operation at the end.
    a.begin_attempt(classic, TimeUs(-2_000_000), 0);
    a.drain(script.iter().copied(), &Map, &mut w);
    let s = *r.read();
    let calls = counting::calls() - before;
    counting::track(false);
    assert_eq!(calls, 0, "the reducer allocated or freed {calls} times");
    assert!(s.hispeed.is_some());
    assert_eq!(a.diagnostics().unrepresentable, 0);
}
