//! One source table per attempt: later openings reuse its queues, readers
//! and snapshot storage on a new timeline, a removed device keeps its
//! source id and reports absent, and a reconnected one waits for the next
//! attempt.

use bms_model::TimeUs;
use eval::{ControlId, InputKind, InputStart, PhysicalInputEvent, Published, SourceId};
use input::{DeviceId, DeviceKind, Packet, ReopenError, SourceConfig, SourceReader, SourceSet};

const A: ControlId = ControlId(0x1e);
const B: ControlId = ControlId(0x1f);

fn set_with(devices: &[u64]) -> SourceSet {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 4,
        max_sources: 4,
        max_held: 4,
        queue_capacity: 16,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    for &d in devices {
        set.connect(DeviceId(d), DeviceKind::Keyboard);
        set.set_recorded(DeviceId(d), vec![A, B]);
    }
    set.end_batch(TimeUs(1));
    set
}

fn batch(set: &mut SourceSet, at: i64, f: impl FnOnce(&mut SourceSet)) {
    set.begin_batch(TimeUs(at));
    f(set);
    set.end_batch(TimeUs(at + 1));
}

fn press(set: &mut SourceSet, device: u64, control: ControlId, value: i32) {
    set.packet(&Packet {
        device: DeviceId(device),
        control,
        value,
        background: false,
    });
}

fn drain(readers: &mut [SourceReader]) -> Vec<PhysicalInputEvent> {
    readers
        .iter_mut()
        .flat_map(|r| std::iter::from_fn(move || r.pop()))
        .collect()
}

fn control(at: i64, source: u16, control: ControlId, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(at),
        source: SourceId(source),
        kind: InputKind::Control { control, value },
    }
}

#[test]
fn a_reopened_opening_starts_its_queues_over_on_a_new_timeline() {
    let mut set = set_with(&[1, 2]);
    let first = set.open_attempt(TimeUs(100), TimeUs(50)).clone();
    assert_eq!(first.opened_at, TimeUs(50));
    let mut readers = set.take_readers();
    batch(&mut set, 120, |s| press(s, 1, A, 1));
    assert_eq!(drain(&mut readers), [control(70, 0, A, 1)]);
    assert_eq!(readers[0].published(), Published::open(TimeUs(70)));

    set.return_readers(readers);
    let second = set.reopen(TimeUs(200), TimeUs(150)).unwrap().clone();
    assert_eq!(
        second,
        InputStart {
            opened_at: TimeUs(50),
            held: vec![(SourceId(0), A, 1)],
        },
        "the snapshot holds what is held at the new opening"
    );
    let mut readers = set.take_readers();
    assert_eq!(readers.len(), 2);
    assert_eq!(readers[0].published(), Published::open(TimeUs(i64::MIN)));
    batch(&mut set, 210, |s| press(s, 2, B, 1));
    assert_eq!(drain(&mut readers), [control(60, 1, B, 1)]);
    assert_eq!(readers[1].published(), Published::open(TimeUs(60)));
}

#[test]
fn a_removed_source_keeps_its_id_and_a_reconnected_one_waits_for_the_next_attempt() {
    let mut set = set_with(&[1, 2]);
    set.open_attempt(TimeUs(0), TimeUs(0));
    let mut readers = set.take_readers();
    batch(&mut set, 10, |s| s.disconnect(DeviceId(1)));
    batch(&mut set, 20, |s| {
        s.connect(DeviceId(1), DeviceKind::Keyboard);
        s.set_recorded(DeviceId(1), vec![A, B]);
    });
    assert_eq!(set.arrivals().len(), 1);
    drain(&mut readers);
    set.return_readers(readers);
    set.reopen(TimeUs(30), TimeUs(25)).unwrap();
    let mut readers = set.take_readers();
    assert_eq!(set.arrivals(), [], "arrivals are per opening");
    // Source 0 reports absent at the opening; source 1 keeps its id.
    assert_eq!(readers[0].published(), Published::closed(TimeUs(5)));
    batch(&mut set, 40, |s| {
        press(s, 1, A, 1);
        press(s, 2, A, 1);
    });
    assert_eq!(
        drain(&mut readers),
        [
            PhysicalInputEvent {
                received: TimeUs(5),
                source: SourceId(0),
                kind: InputKind::Disconnected,
            },
            control(15, 1, A, 1),
        ],
        "the reconnected keyboard is not recorded in this attempt"
    );
    assert_eq!(
        set.participants().collect::<Vec<_>>(),
        [
            (DeviceId(1), DeviceKind::Keyboard),
            (DeviceId(2), DeviceKind::Keyboard)
        ]
    );
    // The next attempt takes it in.
    let start = set.open_attempt(TimeUs(50), TimeUs(50)).clone();
    assert_eq!(start.held, [(SourceId(0), A, 1), (SourceId(1), A, 1)]);
    let mut readers = set.take_readers();
    batch(&mut set, 60, |s| press(s, 1, B, 1));
    assert_eq!(drain(&mut readers), [control(10, 1, B, 1)]);
}

#[test]
fn a_source_removed_while_focus_was_away_is_absent_from_the_next_opening() {
    let mut set = set_with(&[1, 2]);
    set.open_attempt(TimeUs(0), TimeUs(0));
    let readers = set.take_readers();
    set.background();
    batch(&mut set, 10, |s| s.disconnect(DeviceId(1)));
    // Nothing is recorded or closed once the gate has tripped.
    assert_eq!(readers[0].published(), Published::open(TimeUs(i64::MIN)));
    set.gate_mut().reset();
    set.return_readers(readers);
    set.reopen(TimeUs(30), TimeUs(25)).unwrap();
    let mut readers = set.take_readers();
    assert_eq!(readers[0].published(), Published::closed(TimeUs(5)));
    assert_eq!(readers[1].published(), Published::open(TimeUs(i64::MIN)));
    assert_eq!(
        readers[0].pop().map(|e| e.kind),
        Some(InputKind::Disconnected)
    );
}

#[test]
fn reopening_needs_the_readers_back_drained_and_a_later_origin() {
    let mut set = set_with(&[1, 2]);
    assert_eq!(
        set.reopen(TimeUs(0), TimeUs(0)).err(),
        Some(ReopenError::NotRecording)
    );
    set.open_attempt(TimeUs(10), TimeUs(10));
    let mut readers = set.take_readers();
    assert_eq!(
        set.reopen(TimeUs(20), TimeUs(20)).err(),
        Some(ReopenError::ReadersOut)
    );
    batch(&mut set, 30, |s| press(s, 2, A, 1));
    readers.swap(0, 1);
    set.return_readers(readers);
    assert_eq!(
        set.reopen(TimeUs(40), TimeUs(40)).err(),
        Some(ReopenError::Readers)
    );
    let mut readers = set.take_readers();
    readers.swap(0, 1);
    set.return_readers(readers);
    assert_eq!(
        set.reopen(TimeUs(40), TimeUs(40)).err(),
        Some(ReopenError::Undrained(SourceId(1)))
    );
    assert_eq!(
        set.reopen(TimeUs(40), TimeUs(10)).err(),
        Some(ReopenError::Origin),
        "an origin is never reused"
    );
    let mut readers = set.take_readers();
    assert_eq!(
        drain(&mut readers),
        [control(20, 1, A, 1)],
        "nothing was lost"
    );
    set.return_readers(readers);
    assert!(set.reopen(TimeUs(40), TimeUs(40)).is_ok());
}
