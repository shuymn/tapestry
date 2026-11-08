//! The readout projections allocate nothing (design.md "Scope of No
//! Allocation"): every projection and variant, full covers, an unknown
//! hi-speed and unavailable references. This file holds a single test, so
//! the harness's other threads are not tracked.

mod counting;

use bms_model::Speed;
use game::adjustment::PresentationSnapshot;
use game::readouts::{project, variant};
use game::{Cover, Covers, HiSpeedSettings, PlayOptions, Ratio, References, SpeedMode};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

#[test]
fn the_projections_allocate_nothing() {
    let cover = |amount: u16, on: bool| Cover { amount, on };
    let cases = [
        Covers::default(),
        Covers {
            sudden: cover(250, false),
            lift: cover(300, true),
            hidden: cover(300, true),
        },
        Covers {
            sudden: cover(1000, true),
            lift: cover(0, false),
            hidden: cover(1000, true),
        },
        Covers {
            sudden: cover(1000, true),
            lift: cover(1000, true),
            hidden: cover(1000, true),
        },
    ];
    let snapshots: Vec<PresentationSnapshot> = cases
        .iter()
        .flat_map(|&c| {
            [SpeedMode::Floating, SpeedMode::Classic].map(|mode| PresentationSnapshot {
                options: PlayOptions::new(
                    HiSpeedSettings {
                        mode,
                        ..HiSpeedSettings::default()
                    },
                    c,
                )
                .unwrap(),
                hispeed: Some(Ratio::new(16, 5).unwrap()),
                target: None,
                display_offset_us: 0,
                adjusting: true,
                generation: 0,
                sequence: 1,
                epoch: 0,
                scratch: game::ScratchInput::NONE,
            })
        })
        .collect();
    let slow = Ratio::new(1, 1000).unwrap();
    let refs = References {
        start: Ratio::integer(150),
        main: Ratio::integer(125),
        minimum: slow,
        maximum: Ratio::integer(240),
    };
    let tempos = [Some(Ratio::integer(150)), Some(slow), None];
    counting::track(true);
    counting::self_check();
    let before = counting::calls();
    let mut available = 0_usize;
    for s in &snapshots {
        for h in [s.hispeed, Some(Ratio::new(1, 100).unwrap()), None] {
            let s = PresentationSnapshot { hispeed: h, ..*s };
            for current in tempos {
                for references in [Some(refs), None] {
                    for speed in [Speed::NORMAL, Speed::SLOWEST, Speed::FASTEST] {
                        let p = project(&s, current, references, speed);
                        available += p.variants.iter().flatten().count();
                    }
                }
            }
        }
    }
    let v = variant(250, 300, Some(Ratio::integer(150)), Some(Ratio::integer(2)));
    let calls = counting::calls() - before;
    counting::track(false);
    assert_eq!(calls, 0, "the projections allocated or freed {calls} times");
    assert!(available > 0 && v.is_some());
}
