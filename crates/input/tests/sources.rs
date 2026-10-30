//! Source queues, the watermark protocol, recording and the focus gate.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

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

/// Connects `device`, recording the bound keys A, B and C from it.
fn plug(set: &mut SourceSet, device: DeviceId, kind: DeviceKind) {
    set.connect(device, kind);
    set.set_recorded(device, vec![A, B, C]);
}

/// A set with keyboards `0..n` connected, in that order.
fn keyboards(n: u64) -> SourceSet {
    let mut set = SourceSet::new(config()).unwrap();
    set.begin_batch(TimeUs(0));
    for d in 0..n {
        plug(&mut set, DeviceId(d), DeviceKind::Keyboard);
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
    // A queue far smaller than the stream: the producer waits for room, so
    // the two threads interleave throughout.
    const QUEUE: i64 = 64;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut set = SourceSet::new(SourceConfig {
        queue_capacity: QUEUE as usize,
        ..config()
    })
    .unwrap();
    set.begin_batch(TimeUs(-1));
    plug(&mut set, DeviceId(0), DeviceKind::Keyboard);
    set.open_recording(TimeUs(-1));
    let mut reader = set.take_readers().pop().unwrap();
    let consumed = Arc::new(AtomicI64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let producer = {
        let (consumed, done) = (Arc::clone(&consumed), Arc::clone(&done));
        thread::spawn(move || {
            for t in 0..BATCHES {
                while (t + 1) * PER_BATCH - consumed.load(Ordering::Acquire) > QUEUE {
                    assert!(Instant::now() < deadline, "producer timed out");
                    thread::yield_now();
                }
                set.begin_batch(TimeUs(t));
                for i in 0..PER_BATCH {
                    set.packet(&key(0, [A, B][(i % 2) as usize], (i % 2) as i32));
                }
                // The fresh read after the batch is `t + 1`.
                set.end_batch(TimeUs(t + 1));
            }
            done.store(true, Ordering::Release);
            set.overflowed()
        })
    };
    let total = BATCHES * PER_BATCH;
    let mut popped = 0;
    // Checks that found a new prefix while the producer was still running.
    let mut intermediate = 0u64;
    while popped < total {
        assert!(Instant::now() < deadline, "consumer timed out");
        let running = !done.load(Ordering::Acquire);
        let w = reader.published().received_up_to.0;
        let due = if w < 0 { 0 } else { (w + 1) * PER_BATCH };
        if running && due > popped {
            intermediate += 1;
        }
        while popped < due {
            let e = reader.pop().unwrap_or_else(|| {
                panic!("event {popped} received at or before {w} is not poppable")
            });
            assert!(e.received.0 <= w);
            popped += 1;
        }
        consumed.store(popped, Ordering::Release);
        thread::yield_now();
    }
    assert!(!producer.join().unwrap());
    assert_eq!(popped, 1_000_000);
    // The producer cannot run more than one queue ahead, so most of the
    // stream is checked prefix by prefix while it runs.
    assert!(intermediate >= 1_000, "{intermediate}");
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
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    plug(&mut set, DeviceId(0), DeviceKind::Keyboard);
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
    plug(&mut set, DeviceId(7), DeviceKind::Controller);
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

/// Each play's chart time 0 lies `origin` into the batch clock: every time
/// the play sees is the batch time minus it, worked out by hand below.
#[test]
fn play_origin_moves_every_recorded_time_onto_the_play() {
    let mut set = keyboards(2);
    // Held before the opening: in the snapshot, which carries no time.
    batch(&mut set, 3_100_000, &[key(1, C, 1)], 3_100_400);
    let start = set.open_play(TimeUs(4_000_000), TimeUs(5_000_000));
    assert_eq!(
        start,
        InputStart {
            opened_at: TimeUs(-1_000_000),
            held: vec![(SourceId(1), C, 1)],
        }
    );
    let mut readers = set.take_readers();
    batch(&mut set, 4_200_000, &[key(0, B, 1)], 4_200_300);
    assert_eq!(drain(&mut readers[0]), [ev(-800_000, 0, B, 1)]);
    // fresh − origin − 1.
    assert_eq!(readers[0].published(), Published::open(TimeUs(-799_701)));
    assert!(set.begin_batch(TimeUs(4_300_000)));
    plug(&mut set, DeviceId(6), DeviceKind::Controller);
    set.disconnect(DeviceId(1));
    set.end_batch(TimeUs(4_300_900));
    assert_eq!(
        set.arrivals(),
        [Arrival {
            at: TimeUs(-700_000),
            kind: DeviceKind::Controller,
        }]
    );
    assert_eq!(
        drain(&mut readers[1]),
        [PhysicalInputEvent {
            received: TimeUs(-700_000),
            source: SourceId(1),
            kind: InputKind::Disconnected,
        }]
    );
    assert_eq!(readers[1].published(), Published::closed(TimeUs(-700_000)));
    assert_eq!(readers[0].published(), Published::open(TimeUs(-699_101)));
    // The next opening without an origin is on the batch clock again; the
    // controller that arrived is source 0, keyboard 0 source 1.
    let start = set.open_recording(TimeUs(4_400_000));
    let mut readers = set.take_readers();
    batch(&mut set, 4_500_000, &[key(0, A, 1)], 4_500_200);
    assert_eq!(start.opened_at, TimeUs(4_400_000));
    assert_eq!(drain(&mut readers[1]), [ev(4_500_000, 1, A, 1)]);
}

#[test]
fn full_arrivals_list_drops_and_counts() {
    let mut set = keyboards(1);
    set.open_recording(TimeUs(0));
    set.begin_batch(TimeUs(5));
    for d in 10..13 {
        plug(&mut set, DeviceId(d), DeviceKind::Keyboard);
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
    let mut set = SourceSet::new(config()).unwrap();
    let mut script = ScriptedInput::new(focus_script()).with_recorded(DeviceId(0), vec![A, B, C]);
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
    let mut set = SourceSet::new(config()).unwrap();
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

/// A replayed controller declares the axes its record binds, so its axis
/// events are recorded again; without them they would not be.
#[test]
fn scripted_controller_declares_its_recorded_axes() {
    let x = ControlId(0x1_0030);
    let profile = SourceProfile {
        axes: vec![eval::AxisBinding {
            control: x,
            lane: Lane::new(0).unwrap(),
            scratch: eval::ScratchSettings::DEFAULT,
        }],
        ..SourceProfile::default()
    };
    let declared = input::declared_axes(&profile);
    assert_eq!(declared.len(), 1);
    assert_eq!(declared[0].control, x);
    let log = [ev(1_000, 0, x, 40), ev(1_100, 0, A, 1)];
    let run = |script: ScriptedInput| {
        let mut script = script;
        let mut set = SourceSet::new(config()).unwrap();
        script.poll(TimeUs(999), &mut set);
        set.open_recording(TimeUs(999));
        let mut readers = set.take_readers();
        script.poll(TimeUs(1_200), &mut set);
        drain(&mut readers[0])
    };
    let kinds = [DeviceKind::Controller];
    let script = ScriptedInput::from_log(&InputStart::default(), &log, &kinds);
    assert_eq!(run(script.clone()), [log[1]]);
    assert_eq!(run(script.with_axes(DeviceId(0), &declared)), log);
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
        axes: Vec::new(),
    };
    let events = autoplay(&chart, &profile).unwrap();
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
fn each_connection_is_numbered_with_recording_open_or_closed() {
    let mut set = keyboards(2);
    assert_eq!(set.connection(DeviceId(0)), Some(0));
    assert_eq!(set.connection(DeviceId(1)), Some(1));
    set.open_recording(TimeUs(10));
    set.begin_batch(TimeUs(20));
    set.disconnect(DeviceId(1));
    set.end_batch(TimeUs(21));
    assert_eq!(set.connection(DeviceId(1)), None);
    set.close_recording();
    // Removed and connected again in one batch: a new connection.
    set.begin_batch(TimeUs(30));
    set.disconnect(DeviceId(0));
    plug(&mut set, DeviceId(0), DeviceKind::Keyboard);
    set.end_batch(TimeUs(31));
    assert_eq!(set.connection(DeviceId(0)), Some(2));
    assert_eq!(set.connection(DeviceId(7)), None);
}

#[test]
fn reconnecting_a_known_device_adds_no_source() {
    let mut set = keyboards(2);
    set.begin_batch(TimeUs(5));
    plug(&mut set, DeviceId(1), DeviceKind::Controller);
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
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    for d in [4, 5, 6] {
        plug(&mut set, DeviceId(d), DeviceKind::Keyboard);
    }
    set.open_recording(TimeUs(1));
    assert!(set.source_limited());
    assert_eq!(set.participants().count(), 2);

    // Three keyboards, a device table of two: the third is not tracked.
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        ..config()
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    for d in [4, 5, 6] {
        plug(&mut set, DeviceId(d), DeviceKind::Keyboard);
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
fn a_device_that_takes_no_part_is_not_a_source() {
    // A controller and three keyboards, three sources: the controller (8)
    // takes no part, so the keyboards fit and the play is not limited.
    let mut set = SourceSet::new(SourceConfig {
        max_sources: 3,
        ..config()
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    plug(&mut set, DeviceId(8), DeviceKind::Controller);
    for d in [4, 5, 6] {
        plug(&mut set, DeviceId(d), DeviceKind::Keyboard);
    }
    assert!(set.set_takes_part(DeviceId(8), false));
    assert!(!set.set_takes_part(DeviceId(9), false));
    set.end_batch(TimeUs(0));
    batch(&mut set, 10, &[key(8, A, 1), key(5, B, 1)], 11);
    let start = set.open_recording(TimeUs(20));
    let mut readers = set.take_readers();
    assert!(!set.source_limited());
    let devices: Vec<_> = set.participants().map(|(d, _)| d.0).collect();
    assert_eq!(devices, [4, 5, 6]);
    // Its held control is not in the snapshot, and its presses are not
    // recorded.
    assert_eq!(start.held, [(SourceId(1), B, 1)]);
    batch(&mut set, 30, &[key(8, C, 1), key(6, C, 1)], 31);
    let recorded: Vec<_> = readers.iter_mut().map(drain).collect();
    assert_eq!(recorded, [vec![], vec![], vec![ev(30, 2, C, 1)]]);
    // Reconnected, it takes part again, first as a controller, and the
    // third keyboard is left out.
    set.disconnect(DeviceId(8));
    plug(&mut set, DeviceId(8), DeviceKind::Controller);
    set.open_recording(TimeUs(40));
    assert!(set.source_limited());
    let devices: Vec<_> = set.participants().map(|(d, _)| d.0).collect();
    assert_eq!(devices, [8, 4, 5]);
}

#[test]
fn controllers_take_part_before_keyboards_at_the_limit() {
    // Three keyboard handles found before the controller, two sources: the
    // controller is source 0, then the first keyboard found.
    let mut set = SourceSet::new(SourceConfig {
        max_sources: 2,
        ..config()
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    for d in [4, 5, 6] {
        plug(&mut set, DeviceId(d), DeviceKind::Keyboard);
    }
    plug(&mut set, DeviceId(7), DeviceKind::Controller);
    set.open_recording(TimeUs(1));
    assert!(set.source_limited());
    let sources: Vec<_> = set.participants().collect();
    assert_eq!(
        sources,
        [
            (DeviceId(7), DeviceKind::Controller),
            (DeviceId(4), DeviceKind::Keyboard)
        ]
    );
}

#[test]
fn a_controller_keeps_its_product_until_it_is_removed() {
    let product = |interface| input::hid::HidProduct {
        vendor_id: 0x1234,
        product_id: 0x5678,
        interface,
    };
    let mut set = SourceSet::new(config()).unwrap();
    set.begin_batch(TimeUs(0));
    assert!(!set.set_product(DeviceId(7), product(Some(1))));
    set.connect(DeviceId(7), DeviceKind::Controller);
    assert!(!set.has_product(DeviceId(7), product(Some(1))));
    assert!(set.set_product(DeviceId(7), product(Some(1))));
    assert!(set.has_product(DeviceId(7), product(Some(1))));
    assert!(!set.has_product(DeviceId(7), product(Some(2))));
    assert!(!set.has_product(DeviceId(7), product(None)));
    set.disconnect(DeviceId(7));
    set.connect(DeviceId(7), DeviceKind::Controller);
    assert!(!set.has_product(DeviceId(7), product(Some(1))));
}

#[test]
fn devices_left_out_by_the_source_limit_are_counted() {
    // Two controllers that take part, one that does not, and three
    // keyboards; three sources: both controllers and the first keyboard
    // take part, two keyboards are left out.
    let mut set = SourceSet::new(SourceConfig {
        max_sources: 3,
        ..config()
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    for d in [4, 5, 6] {
        plug(&mut set, DeviceId(d), DeviceKind::Keyboard);
    }
    for d in [7, 8, 9] {
        plug(&mut set, DeviceId(d), DeviceKind::Controller);
    }
    set.set_takes_part(DeviceId(9), false);
    assert_eq!(set.left_out(), 0, "nothing is open yet");
    set.open_recording(TimeUs(1));
    assert_eq!(set.left_out(), 2);
    assert!(set.source_limited());
    let devices: Vec<_> = set.participants().map(|(d, _)| d.0).collect();
    assert_eq!(devices, [7, 8, 4]);
    set.close_recording();
    assert_eq!(set.left_out(), 0);
}

/// An axis is recorded in a play only if its device declared it when
/// recording opened: an axis a collection connected later adds to the
/// source, and an axis never declared, are not recorded, while a declared
/// one and the buttons are.
#[test]
fn only_axes_declared_at_the_opening_are_recorded() {
    let x = ControlId(0x1_0030);
    let y = ControlId(0x1_0031);
    let z = ControlId(0x1_0032);
    let button = ControlId(3);
    let cap = |control| input::hid::axis_cap(control, true, 0, 255, 8).unwrap();
    let mut set = SourceSet::new(config()).unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(7), DeviceKind::Controller);
    set.set_recorded(DeviceId(7), vec![button, x, y, z]);
    set.add_axes(DeviceId(7), &[cap(x)]);
    set.end_batch(TimeUs(0));
    set.open_recording(TimeUs(10));
    let mut readers = set.take_readers();
    // A collection connected during the play declares y.
    set.begin_batch(TimeUs(20));
    set.add_axes(DeviceId(7), &[cap(y)]);
    for c in [x, y, z, button] {
        set.packet(&key(7, c, 5));
    }
    set.end_batch(TimeUs(21));
    assert_eq!(
        drain(&mut readers[0]),
        [ev(20, 0, x, 5), ev(20, 0, button, 5)]
    );
    // From the next opening y is declared, so it is recorded.
    set.open_recording(TimeUs(30));
    let mut readers = set.take_readers();
    batch(&mut set, 40, &[key(7, y, 6), key(7, z, 6)], 41);
    assert_eq!(drain(&mut readers[0]), [ev(40, 0, y, 6)]);
}

#[test]
fn full_held_state_marks_the_log_incomplete() {
    let mut set = SourceSet::new(SourceConfig {
        max_held: 2,
        ..config()
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    plug(&mut set, DeviceId(3), DeviceKind::Keyboard);
    set.open_recording(TimeUs(1));
    batch(&mut set, 10, &[key(3, A, 1), key(3, B, 2)], 11);
    assert!(!set.overflowed());
    batch(&mut set, 20, &[key(3, C, 3)], 21);
    assert!(set.overflowed());
    // The retry's snapshot lacks C, so its log is incomplete too.
    let start = set.open_recording(TimeUs(30));
    assert_eq!(start.held, [(SourceId(0), A, 1), (SourceId(0), B, 2)]);
    assert!(set.overflowed());
    // A device that arrives and loses state does not take part: no flag.
    set.begin_batch(TimeUs(35));
    plug(&mut set, DeviceId(4), DeviceKind::Keyboard);
    batch(
        &mut set,
        40,
        &[key(4, A, 4), key(4, B, 5), key(4, C, 6)],
        41,
    );
    // Removing the first device clears its loss; the second, lost before
    // this opening, still makes the play incomplete.
    set.begin_batch(TimeUs(50));
    set.disconnect(DeviceId(3));
    plug(&mut set, DeviceId(3), DeviceKind::Keyboard);
    set.open_recording(TimeUs(60));
    assert!(set.overflowed());
    set.begin_batch(TimeUs(70));
    set.disconnect(DeviceId(4));
    let start = set.open_recording(TimeUs(80));
    assert!(start.held.is_empty());
    assert!(!set.overflowed());
}

#[test]
fn arrival_losing_state_during_play_leaves_the_log_complete() {
    let mut set = SourceSet::new(SourceConfig {
        max_held: 1,
        ..config()
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    plug(&mut set, DeviceId(1), DeviceKind::Keyboard);
    set.open_recording(TimeUs(5));
    set.begin_batch(TimeUs(10));
    plug(&mut set, DeviceId(2), DeviceKind::Keyboard);
    batch(&mut set, 20, &[key(2, A, 1), key(2, B, 2)], 21);
    assert!(!set.overflowed());
}

#[test]
fn bindings_are_per_device() {
    const X: ControlId = ControlId(0x101);
    const Y: ControlId = ControlId(0x102);
    let mut set = SourceSet::new(config()).unwrap();
    set.begin_batch(TimeUs(0));
    // Two controllers of different models, each with its own binding.
    set.connect(DeviceId(5), DeviceKind::Controller);
    set.connect(DeviceId(6), DeviceKind::Controller);
    assert!(set.set_recorded(DeviceId(5), vec![X]));
    assert!(set.set_recorded(DeviceId(6), vec![Y]));
    assert!(!set.set_recorded(DeviceId(9), vec![X]));
    // Each holds its own control and the other's.
    batch(
        &mut set,
        10,
        &[key(5, X, 1), key(5, Y, 2), key(6, X, 3), key(6, Y, 4)],
        11,
    );
    let start = set.open_recording(TimeUs(20));
    let mut readers = set.take_readers();
    assert_eq!(start.held, [(SourceId(0), X, 1), (SourceId(1), Y, 4)]);
    batch(
        &mut set,
        30,
        &[key(5, X, 0), key(5, Y, 0), key(6, X, 0), key(6, Y, 0)],
        31,
    );
    assert_eq!(drain(&mut readers[0]), [ev(30, 0, X, 0)]);
    assert_eq!(drain(&mut readers[1]), [ev(30, 1, Y, 0)]);
    // A change applies from the next opening.
    assert!(set.set_recorded(DeviceId(5), vec![Y]));
    batch(&mut set, 40, &[key(5, X, 5)], 41);
    assert_eq!(drain(&mut readers[0]), [ev(40, 0, X, 5)]);
    let start = set.open_recording(TimeUs(50));
    assert!(start.held.is_empty());
}

#[test]
fn scripted_focus_loss_without_later_input_stops_publication() {
    let item = |at: i64, action| ScriptItem {
        at: TimeUs(at),
        action,
    };
    let mut script = ScriptedInput::new(vec![
        item(0, Action::Connect(DeviceId(0), DeviceKind::Keyboard)),
        item(500, Action::FocusLoss),
    ]);
    let mut set = SourceSet::new(config()).unwrap();
    script.poll(TimeUs(0), &mut set);
    set.open_recording(TimeUs(0));
    let readers = set.take_readers();
    script.poll(TimeUs(400), &mut set);
    assert_eq!(readers[0].published(), Published::open(TimeUs(399)));
    for now in [500, 600, 700] {
        script.poll(TimeUs(now), &mut set);
    }
    assert!(set.gate().is_tripped());
    assert_eq!(readers[0].published(), Published::open(TimeUs(399)));
}

#[test]
fn latch_set_outside_any_batch_stops_publication() {
    let mut set = keyboards(1);
    set.open_recording(TimeUs(0));
    let readers = set.take_readers();
    let latch = set.gate().handle();
    set.end_batch(TimeUs(100));
    latch.latch();
    set.end_batch(TimeUs(200));
    assert!(set.gate().is_tripped());
    assert_eq!(readers[0].published(), Published::open(TimeUs(99)));
}

#[test]
fn background_press_is_held_without_a_press_edge() {
    let mut set = keyboards(1);
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();
    set.begin_batch(TimeUs(10));
    let pressed = Packet {
        background: true,
        ..key(0, B, 7)
    };
    assert_eq!(set.packet(&pressed), Admit::Reject);
    set.end_batch(TimeUs(11));
    assert!(drain(&mut readers[0]).is_empty());
    // Focus comes back; the next play starts with B held.
    set.gate_mut().reset();
    let start = set.open_recording(TimeUs(20));
    let mut readers = set.take_readers();
    assert_eq!(start.held, [(SourceId(0), B, 7)]);
    batch(&mut set, 30, &[key(0, B, 0)], 31);
    assert_eq!(drain(&mut readers[0]), [ev(30, 0, B, 0)]);
}

#[test]
fn too_many_sources_are_refused_at_construction() {
    let at_limit = SourceConfig {
        max_sources: input::SOURCE_LIMIT,
        ..config()
    };
    assert!(SourceSet::new(at_limit.clone()).is_ok());
    let over = SourceConfig {
        max_sources: input::SOURCE_LIMIT + 1,
        ..at_limit
    };
    let refused = SourceSet::new(over).err();
    assert_eq!(refused, Some(input::ConfigError::TooManySources(65_537)));
    // A leaf: the message is the whole diagnostic.
    let e = refused.unwrap();
    assert_eq!(e.to_string(), "65537 sources exceed the limit of 65536");
    assert!(std::error::Error::source(&e).is_none());
}

#[test]
fn autoplay_refuses_times_outside_the_range() {
    let lane = Lane::new(1).unwrap();
    let chart = ChartBuilder::new(120_000)
        .tap(lane, TimeUs(1_000_000))
        .build()
        .unwrap();
    let profile = |offset| SourceProfile {
        input_offset: TimeUs(offset),
        bindings: vec![Binding { control: A, lane }],
        axes: Vec::new(),
    };
    // The release is at 1_050_000: the largest offset that fits.
    let fits = i64::MAX - 1_050_000;
    assert_eq!(
        autoplay(&chart, &profile(fits))
            .unwrap()
            .last()
            .unwrap()
            .received,
        TimeUs(i64::MAX)
    );
    assert_eq!(
        autoplay(&chart, &profile(fits + 1)),
        Err(input::TimeOverflow)
    );
    // The hold would end past the range, but the lane's next note comes
    // first, so the release is representable.
    let late = ChartBuilder::new(120_000)
        .tap(lane, TimeUs(i64::MAX - 100))
        .mine(lane, TimeUs(i64::MAX - 50), 10)
        .build()
        .unwrap();
    assert_eq!(
        autoplay(&late, &profile(0)),
        Ok(vec![
            ev(i64::MAX - 100, 0, A, 1),
            ev(i64::MAX - 50, 0, A, 0)
        ])
    );
}

/// A queue capacity set between plays sizes the queues of the next opening:
/// a stream that overflows the old size fits the new one.
#[test]
fn a_set_queue_capacity_sizes_the_next_openings_queues() {
    let mut set = SourceSet::new(SourceConfig {
        queue_capacity: 2,
        ..config()
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    plug(&mut set, DeviceId(0), DeviceKind::Keyboard);
    set.end_batch(TimeUs(0));
    let presses = [key(0, A, 1), key(0, A, 0), key(0, B, 1), key(0, B, 0)];
    set.open_recording(TimeUs(0));
    batch(&mut set, 1, &presses, 2);
    assert!(set.overflowed());
    set.close_recording();
    set.set_queue_capacity(4);
    set.open_recording(TimeUs(10));
    let mut readers = set.take_readers();
    batch(&mut set, 11, &presses, 12);
    assert!(!set.overflowed());
    assert_eq!(drain(&mut readers[0]).len(), 4);
}
