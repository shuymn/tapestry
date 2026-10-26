//! A record made under version 1 of the custom preset stays evaluable: the
//! preset file written then still loads to the same canonical bytes, and
//! evaluating under it gives the judgments it gave then.

mod common;

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, control, device, judgments, lane};
use eval::{EvalInput, InputKind, InputStart, PhysicalInputEvent, SourceId};
use replay::PresetRecord;
use rules::Judgment::{Bad, Perfect, Poor};

/// The custom preset file as version 1 wrote it.
const CUSTOM_V1: &str = include_str!("fixtures/custom-v1.toml");

/// SHA-256 of version 1's canonical bytes, computed by an external
/// `sha256sum` over the bytes version 1 wrote.
const CUSTOM_V1_SHA256: &str = "c3d88a28fa7e369f842d6c7a1623be3b551775d1e5e45b222b4c0dd615e0cae1";

fn key(t: i64, n: u8, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(t),
        source: SourceId(0),
        kind: InputKind::Control {
            control: control(lane(n)),
            value,
        },
    }
}

/// A press at `t` and its release 5 ms later.
fn tap(t: i64, n: u8) -> [PhysicalInputEvent; 2] {
    [key(t, n, 1), key(t + 5 * MS, n, 0)]
}

#[test]
fn v1_record_reevaluates_unchanged() {
    let preset = game::load_preset_toml(CUSTOM_V1).unwrap();
    let record = PresetRecord::of(&preset);
    let hex: String = record.sha256.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, CUSTOM_V1_SHA256);
    assert_eq!(record.id, "custom");

    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_060 * MS))
        .tap(lane(2), TimeUs(2_000 * MS))
        .tap(lane(2), TimeUs(2_500 * MS))
        .tap(lane(3), TimeUs(3_000 * MS))
        .tap(lane(3), TimeUs(3_700 * MS))
        .build()
        .unwrap();
    let presses = [
        // Nearest: the later note at 0 ms, and the earlier one is missed.
        (1_060, 1),
        (2_000, 2),
        // 400 ms before the next note, judged or not: nothing.
        (2_100, 2),
        // Exactly 300 ms ahead: an empty POOR.
        (2_200, 2),
        (2_500, 2),
        // BAD 150 ms early, then a press 100 ms before that judged note:
        // nothing, as only the next unjudged note (800 ms on) counts.
        (2_850, 3),
        (2_900, 3),
        (3_700, 3),
    ];
    let input = EvalInput {
        events: presses.iter().flat_map(|&(t, n)| tap(t * MS, n)).collect(),
        device: device(&[0]),
        start: InputStart::default(),
        finalized: None,
    };
    let e = eval::evaluate(&chart, &input, &preset).unwrap();
    assert_eq!(
        judgments(&e.events),
        [
            (1, 1, Perfect, Some(0)),
            (1, 0, Poor, None),
            (2, 0, Perfect, Some(0)),
            (2, 1, Perfect, Some(0)),
            (3, 0, Bad, Some(-150 * MS)),
            (3, 1, Perfect, Some(0)),
        ]
    );
    let t = e.tally;
    assert_eq!(t.empty_poor, 1);
    assert_eq!((t.counts.perfect, t.counts.bad, t.counts.poor), (4, 1, 1));
    assert_eq!((t.ex_score, t.bp, t.combo, t.max_combo), (8, 2, 1, 2));
    assert!(e.completed);
}
