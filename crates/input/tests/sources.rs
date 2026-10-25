//! Source queues, the watermark protocol, recording and the focus gate.

use std::thread;

use bms_model::{ChartBuilder, Lane, TimeUs};
use eval::{
    Binding, ControlId, DeviceProfile, InputKind, InputStart, PhysicalInputEvent, Published,
    SourceId, SourceProfile, SourceState, watermark,
};
use input::{
    Action, Admit, Arrival, DeviceId, DeviceKind, InputAdapter, Packet, ScriptItem, ScriptedInput,
    SourceConfig, SourceReader, SourceSet, autoplay,
};

const A: ControlId = ControlId(0x1e);
const B: ControlId = ControlId(0x1f);
const C: ControlId = ControlId(0x20);
/// Not bound: never recorded.
const TEXT: ControlId = ControlId(0x30);

fn config() -> SourceConfig {
    SourceConfig {
        max_devices: 8,
        max_sources: 4,
        max_held: 16,
        queue_capacity: 1024,
        max_arrivals: 2,
        recorded: vec![A, B, C],
    }
}

fn key(device: u64, control: ControlId, value: i32) -> Packet {
    Packet {
        device: DeviceId(device),
        control,
        value,
        background: false,
    }
}

fn ev(at: i64, source: u16, control: ControlId, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(at),
        source: SourceId(source),
        kind: InputKind::Control { control, value },
    }
}

fn drain(reader: &mut SourceReader) -> Vec<PhysicalInputEvent> {
    std::iter::from_fn(|| reader.pop()).collect()
}

/// A set with keyboards `0..n` connected, in that order.
fn keyboards(n: u64) -> SourceSet {
    let mut set = SourceSet::new(config());
    set.begin_batch(TimeUs(0));
    for d in 0..n {
        set.connect(DeviceId(d), DeviceKind::Keyboard);
    }
    set.end_batch(TimeUs(0));
    set
}

/// One batch received at `at` holding `packets`, published at `fresh`.
fn batch(set: &mut SourceSet, at: i64, packets: &[Packet], fresh: i64) {
    assert!(set.begin_batch(TimeUs(at)));
    for p in packets {
        set.packet(p);
    }
    set.end_batch(TimeUs(fresh));
}

#[test]
fn events_published_before_watermark_are_visible() {
    const BATCHES: i64 = 250_000;
    const PER_BATCH: i64 = 4;
    let mut set = SourceSet::new(SourceConfig {
        queue_capacity: 1 << 20,
        ..config()
    });
    set.begin_batch(TimeUs(-1));
    set.connect(DeviceId(0), DeviceKind::Keyboard);
    set.open_recording(TimeUs(-1));
    let mut reader = set.take_readers().pop().unwrap();
    let producer = thread::spawn(move || {
        for t in 0..BATCHES {
            set.begin_batch(TimeUs(t));
            for i in 0..PER_BATCH {
                set.packet(&key(0, [A, B][(i % 2) as usize], (i % 2) as i32));
            }
            // The fresh read after the batch is `t + 1`.
            set.end_batch(TimeUs(t + 1));
        }
        set.overflowed()
    });
    let total = BATCHES * PER_BATCH;
    let mut popped = 0;
    let mut checks = 0u64;
    while popped < total {
        let w = reader.published().received_up_to.0;
        let due = if w < 0 { 0 } else { (w + 1) * PER_BATCH };
        while popped < due {
            let e = reader.pop().unwrap_or_else(|| {
                panic!("event {popped} received at or before {w} is not poppable")
            });
            assert!(e.received.0 <= w);
            popped += 1;
        }
        checks += 1;
    }
    assert!(!producer.join().unwrap());
    assert_eq!(popped, 1_000_000);
    assert!(checks > 0);
    assert!(!reader.overflowed());
}

#[test]
fn same_microsecond_batch_after_publication_is_not_finalized() {
    let mut set = keyboards(1);
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();
    batch(&mut set, 100, &[key(0, A, 1)], 150);
    assert_eq!(readers[0].published(), Published::open(TimeUs(149)));
    // The next batch is received in the microsecond of the fresh read.
    batch(&mut set, 150, &[key(0, A, 0)], 150);
    let published = readers[0].published().received_up_to;
    let stream = drain(&mut readers[0]);
    assert_eq!(stream, [ev(100, 0, A, 1), ev(150, 0, A, 0)]);
    // It keeps its true time, which is above the watermark: not finalized
    // yet, so it is still evaluated.
    assert!(stream[1].received > published, "{published:?}");
    batch(&mut set, 160, &[], 161);
    assert_eq!(readers[0].published(), Published::open(TimeUs(160)));
}

#[test]
fn snapshot_boundary_follows_stream_order() {
    let mut set = keyboards(1);
    batch(&mut set, 1_000, &[key(0, A, 1), key(0, TEXT, 1)], 1_000);
    let start = set.open_recording(TimeUs(1_000));
    let mut readers = set.take_readers();
    // Same microsecond as `opened_at`, but after the boundary.
    batch(&mut set, 1_000, &[key(0, B, 1)], 1_001);
    assert_eq!(
        start,
        InputStart {
            opened_at: TimeUs(1_000),
            held: vec![(SourceId(0), A, 1)],
        }
    );
    assert_eq!(drain(&mut readers[0]), [ev(1_000, 0, B, 1)]);
}

#[test]
fn held_state_survives_retry() {
    let mut set = keyboards(2);
    set.open_recording(TimeUs(0));
    // C is released without having been pressed.
    batch(
        &mut set,
        10,
        &[key(1, B, 1), key(0, A, 1), key(0, A, 0), key(0, C, 0)],
        11,
    );
    // Retry: B is still down.
    let start = set.open_recording(TimeUs(20));
    let mut readers = set.take_readers();
    assert_eq!(start.held, [(SourceId(1), B, 1)]);
    batch(&mut set, 30, &[key(1, B, 0)], 31);
    assert_eq!(drain(&mut readers[1]), [ev(30, 1, B, 0)]);
    assert!(drain(&mut readers[0]).is_empty());
    let start = set.open_recording(TimeUs(40));
    assert!(start.held.is_empty());
}

#[test]
fn overflow_is_flagged_never_silent() {
    let mut set = SourceSet::new(SourceConfig {
        queue_capacity: 4,
        ..config()
    });
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(0), DeviceKind::Keyboard);
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();
    let controls = [A, B, C, A, B, C];
    let presses: Vec<Packet> = controls.iter().map(|&c| key(0, c, 1)).collect();
    batch(&mut set, 5, &presses, 6);
    assert!(set.overflowed());
    assert!(readers[0].overflowed());
    assert_eq!(
        drain(&mut readers[0]),
        [
            ev(5, 0, A, 1),
            ev(5, 0, B, 1),
            ev(5, 0, C, 1),
            ev(5, 0, A, 1)
        ]
    );
}

#[test]
fn quiet_source_still_advances_watermark() {
    let mut set = keyboards(2);
    set.open_recording(TimeUs(0));
    let readers = set.take_readers();
    let device = DeviceProfile {
        sources: vec![SourceProfile::default(), SourceProfile::default()],
    };
    for t in [100, 200, 300] {
        batch(&mut set, t, &[key(0, A, 1)], t + 5);
        let published: Vec<Published> = readers.iter().map(SourceReader::published).collect();
        assert_eq!(published[1], Published::open(TimeUs(t + 4)));
        assert_eq!(watermark(&published, &device), Some(TimeUs(t + 4)));
    }
}

#[test]
fn arrival_stays_out_of_stream_and_watermark() {
    let mut set = keyboards(1);
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();
    assert!(set.begin_batch(TimeUs(50)));
    set.connect(DeviceId(7), DeviceKind::Controller);
    set.packet(&key(7, A, 1));
    set.end_batch(TimeUs(60));
    assert_eq!(
        set.arrivals(),
        [Arrival {
            at: TimeUs(50),
            kind: DeviceKind::Controller,
        }]
    );
    assert_eq!(set.participants().count(), 1);
    assert_eq!(readers.len(), 1);
    assert!(drain(&mut readers[0]).is_empty());
    let published = [readers[0].published()];
    assert_eq!(
        watermark(&published, &DeviceProfile::default()),
        Some(TimeUs(59))
    );
    // It takes part from the next play.
    set.open_recording(TimeUs(70));
    assert_eq!(set.participants().count(), 2);
}

#[test]
fn full_arrivals_list_drops_and_counts() {
    let mut set = keyboards(1);
    set.open_recording(TimeUs(0));
    set.begin_batch(TimeUs(5));
    for d in 10..13 {
        set.connect(DeviceId(d), DeviceKind::Keyboard);
    }
    assert_eq!(set.arrivals().len(), 2);
    assert_eq!(set.arrivals_dropped(), 1);
}

/// Three bound press/release pairs and an unbound key before focus loss,
/// bound keys after it.
fn focus_script() -> Vec<ScriptItem> {
    let item = |at: i64, action| ScriptItem {
        at: TimeUs(at),
        action,
    };
    let mut items = vec![item(0, Action::Connect(DeviceId(0), DeviceKind::Keyboard))];
    for (i, c) in [A, B, C].into_iter().enumerate() {
        let t = 1_000 + 1_000 * i as i64;
        items.push(item(t, Action::Packet(key(0, c, 1))));
        items.push(item(t + 300, Action::Packet(key(0, c, 0))));
    }
    items.push(item(3_500, Action::Packet(key(0, TEXT, 1))));
    items.push(item(4_000, Action::FocusLoss));
    // Same time as the loss, then later.
    items.push(item(4_000, Action::Packet(key(0, A, 1))));
    items.push(item(4_500, Action::Packet(key(0, A, 0))));
    items.push(item(5_000, Action::Packet(key(0, B, 1))));
    items
}

#[test]
fn bound_keys_after_focus_loss_are_never_recorded() {
    let mut set = SourceSet::new(config());
    let mut script = ScriptedInput::new(focus_script());
    script.poll(TimeUs(0), &mut set);
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();
    let mut published_at_trip = None;
    for now in (100..=6_000).step_by(100) {
        script.poll(TimeUs(now), &mut set);
        if set.gate().is_tripped() && published_at_trip.is_none() {
            published_at_trip = Some(readers[0].published());
        }
    }
    assert!(script.is_done());
    assert!(set.gate().is_tripped());
    let stream = drain(&mut readers[0]);
    assert_eq!(stream.len(), 6, "{stream:?}");
    assert!(stream.iter().all(|e| e.received < TimeUs(4_000)));
    // Nothing is published after the trip.
    assert_eq!(Some(readers[0].published()), published_at_trip);
}

#[test]
fn focus_change_between_check_and_batch_is_rejected() {
    let mut set = keyboards(1);
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();
    // The latch check passes, then the batch holds a background packet.
    assert!(set.begin_batch(TimeUs(100)));
    assert_eq!(set.packet(&key(0, A, 1)), Admit::Record);
    let background = Packet {
        background: true,
        ..key(0, B, 1)
    };
    assert_eq!(set.packet(&background), Admit::Reject);
    assert_eq!(set.packet(&key(0, C, 1)), Admit::Reject);
    set.end_batch(TimeUs(101));
    assert!(set.gate().is_tripped());
    assert_eq!(drain(&mut readers[0]), [ev(100, 0, A, 1)]);
    assert!(!set.begin_batch(TimeUs(200)));
}

#[test]
fn focus_loss_in_the_batch_of_a_disconnect() {
    // The disconnect comes first: it is recorded and closes the source.
    let mut set = keyboards(2);
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();
    set.begin_batch(TimeUs(100));
    set.disconnect(DeviceId(0));
    set.packet(&Packet {
        background: true,
        ..key(1, A, 1)
    });
    set.end_batch(TimeUs(101));
    let stream = drain(&mut readers[0]);
    assert_eq!(stream.len(), 1);
    assert_eq!(stream[0].kind, InputKind::Disconnected);
    assert_eq!(readers[0].published(), Published::closed(TimeUs(100)));
    assert_eq!(readers[1].published().state, SourceState::Open);
    assert!(drain(&mut readers[1]).is_empty());

    // The background packet comes first: the disconnect is not recorded.
    let mut set = keyboards(2);
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();
    set.begin_batch(TimeUs(100));
    set.packet(&Packet {
        background: true,
        ..key(1, A, 1)
    });
    set.disconnect(DeviceId(0));
    set.end_batch(TimeUs(101));
    assert!(drain(&mut readers[0]).is_empty());
    assert_eq!(readers[0].published().state, SourceState::Open);
}

#[test]
fn publication_at_time_zero() {
    let mut set = keyboards(1);
    set.open_recording(TimeUs(0));
    let readers = set.take_readers();
    batch(&mut set, 0, &[key(0, A, 1)], 0);
    assert_eq!(readers[0].published(), Published::open(TimeUs(-1)));
    batch(&mut set, i64::MIN, &[], i64::MIN);
    assert_eq!(readers[0].published(), Published::open(TimeUs(-1)));
}

#[test]
fn scripted_log_first_event_at_opened_at_is_recorded() {
    let start = InputStart {
        opened_at: TimeUs(1_000),
        held: vec![(SourceId(1), B, 1)],
    };
    let log = [
        ev(1_000, 0, A, 1),
        ev(1_000, 1, B, 0),
        ev(1_200, 0, A, 0),
        PhysicalInputEvent {
            received: TimeUs(1_300),
            source: SourceId(1),
            kind: InputKind::Disconnected,
        },
    ];
    let mut script =
        ScriptedInput::from_log(&start, &log, &[DeviceKind::Keyboard, DeviceKind::Keyboard]);
    let mut set = SourceSet::new(config());
    script.poll(TimeUs(999), &mut set);
    assert_eq!(set.open_recording(TimeUs(1_000)), start);
    let mut readers = set.take_readers();
    for now in [1_000, 1_100, 1_200, 1_300, 1_400] {
        script.poll(TimeUs(now), &mut set);
    }
    let mut stream: Vec<PhysicalInputEvent> = readers.iter_mut().flat_map(drain).collect();
    stream.sort_by_key(|e| (e.received, e.source));
    assert_eq!(stream, log);
}

#[test]
fn autoplay_presses_every_note_and_releases_before_the_next() {
    let lane = |i| Lane::new(i).unwrap();
    let chart = ChartBuilder::new(120_000)
        .tap(lane(1), TimeUs(1_000_000))
        .tap(lane(1), TimeUs(1_020_000))
        .long(lane(2), TimeUs(1_000_000), TimeUs(1_500_000))
        .mine(lane(3), TimeUs(1_100_000), 10)
        .tap(lane(4), TimeUs(1_200_000))
        .build()
        .unwrap();
    let profile = SourceProfile {
        input_offset: TimeUs(5),
        bindings: [(1, A), (2, B), (3, C)]
            .map(|(l, control)| Binding {
                control,
                lane: lane(l),
            })
            .to_vec(),
    };
    let events = autoplay(&chart, &profile);
    assert_eq!(
        events,
        [
            ev(1_000_005, 0, A, 1),
            ev(1_000_005, 0, B, 1),
            ev(1_020_005, 0, A, 0),
            ev(1_020_005, 0, A, 1),
            ev(1_070_005, 0, A, 0),
            ev(1_500_005, 0, B, 0),
        ]
    );
}

#[test]
fn closed_source_keeps_its_final_watermark() {
    let mut set = keyboards(2);
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();
    batch(&mut set, 100, &[key(0, A, 1), key(1, B, 1)], 110);
    set.begin_batch(TimeUs(200));
    set.disconnect(DeviceId(0));
    set.end_batch(TimeUs(210));
    batch(&mut set, 300, &[key(1, B, 0)], 310);
    assert_eq!(readers[0].published(), Published::closed(TimeUs(200)));
    assert_eq!(readers[1].published(), Published::open(TimeUs(309)));
    assert_eq!(
        drain(&mut readers[0]),
        [
            ev(100, 0, A, 1),
            PhysicalInputEvent {
                received: TimeUs(200),
                source: SourceId(0),
                kind: InputKind::Disconnected,
            },
        ]
    );
}

#[test]
fn reconnecting_a_known_device_adds_no_source() {
    let mut set = keyboards(2);
    set.begin_batch(TimeUs(5));
    set.connect(DeviceId(1), DeviceKind::Controller);
    set.open_recording(TimeUs(10));
    assert_eq!(
        set.participants().collect::<Vec<_>>(),
        [
            (DeviceId(0), DeviceKind::Keyboard),
            (DeviceId(1), DeviceKind::Keyboard)
        ]
    );
    assert!(!set.source_limited());
}

#[test]
fn source_and_device_limits_mark_the_log_incomplete() {
    // Three keyboards, two sources: the third does not take part.
    let mut set = SourceSet::new(SourceConfig {
        max_sources: 2,
        ..config()
    });
    set.begin_batch(TimeUs(0));
    for d in [4, 5, 6] {
        set.connect(DeviceId(d), DeviceKind::Keyboard);
    }
    set.open_recording(TimeUs(1));
    assert!(set.source_limited());
    assert_eq!(set.participants().count(), 2);

    // Three keyboards, a device table of two: the third is not tracked.
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        ..config()
    });
    set.begin_batch(TimeUs(0));
    for d in [4, 5, 6] {
        set.connect(DeviceId(d), DeviceKind::Keyboard);
    }
    set.open_recording(TimeUs(1));
    assert!(set.source_limited());
    assert_eq!(set.participants().count(), 2);
    // Once it is removed, every connected device takes part again.
    set.disconnect(DeviceId(6));
    set.open_recording(TimeUs(2));
    assert!(!set.source_limited());
}

#[test]
fn full_held_state_marks_the_log_incomplete() {
    let mut set = SourceSet::new(SourceConfig {
        max_held: 2,
        ..config()
    });
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(3), DeviceKind::Keyboard);
    set.open_recording(TimeUs(1));
    batch(&mut set, 10, &[key(3, A, 1), key(3, B, 2)], 11);
    assert!(!set.overflowed());
    batch(&mut set, 20, &[key(3, C, 3)], 21);
    assert!(set.overflowed());
    let start = set.open_recording(TimeUs(30));
    assert_eq!(start.held, [(SourceId(0), A, 1), (SourceId(0), B, 2)]);
}
