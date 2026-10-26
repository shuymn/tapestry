//! After construction and `open_recording`, batches allocate nothing:
//! packets, pushes, publications, device changes, arrivals, overflow and
//! focus rejection, and a scripted connect and reconnect.
//!
//! A counting global allocator counts allocations made on threads that opted
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.
//! It runs in the dev and release profiles with the same bound, and the
//! tracked stretch starts with a positive control (design.md "Scope of No
//! Allocation").

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::TimeUs;
use eval::ControlId;
use input::{
    Action, DeviceId, DeviceKind, InputAdapter, Packet, ScriptItem, ScriptedInput, SourceConfig,
    SourceSet,
};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: delegates to `System`; only adds a counter, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn batches_do_not_allocate() {
    const QUEUE: usize = 256;
    let controls = [ControlId(0x1e), ControlId(0x1f), ControlId(0x20)];
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 4,
        max_sources: 2,
        max_held: 2,
        queue_capacity: QUEUE,
        max_arrivals: 1,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    for d in 0..3 {
        set.connect(DeviceId(d), DeviceKind::Keyboard);
        set.set_recorded(DeviceId(d), controls.to_vec());
    }
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();
    let key = |device, control, value| Packet {
        device: DeviceId(device),
        control,
        value,
        background: false,
    };

    TRACK.with(|t| t.set(true));
    // The count works: an allocation that escapes is counted in any profile.
    drop(std::hint::black_box(vec![1u8; 8]));
    assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 1);
    let mut popped = 0;
    for t in 1..=1_000_i64 {
        set.begin_batch(TimeUs(t * 10));
        let c = controls[(t % 3) as usize];
        set.packet(&key(0, c, 1));
        set.packet(&key(0, c, 0));
        // Holds every control at once on device 1: beyond `max_held`.
        set.packet(&key(1, c, 1));
        if t == 500 {
            set.connect(DeviceId(10), DeviceKind::Controller);
            set.connect(DeviceId(11), DeviceKind::Controller);
            set.disconnect(DeviceId(2));
        }
        set.end_batch(TimeUs(t * 10 + 1));
        // Device 1's queue is never drained, so it overflows.
        while readers[0].pop().is_some() {
            popped += 1;
        }
    }
    // A scripted connect, disconnect and reconnect while recording is open;
    // the script is built outside the counted section.
    TRACK.with(|t| t.set(false));
    let item = |at, action| ScriptItem {
        at: TimeUs(at),
        action,
    };
    let device = DeviceId(20);
    let mut script = ScriptedInput::new(vec![
        item(15_000, Action::Connect(device, DeviceKind::Controller)),
        item(15_100, Action::Disconnect(device)),
        item(15_200, Action::Connect(device, DeviceKind::Controller)),
    ])
    .with_recorded(device, vec![ControlId(0x101), ControlId(0x102)]);
    TRACK.with(|t| t.set(true));
    for now in [15_000, 15_100, 15_200] {
        script.poll(TimeUs(now), &mut set);
    }
    assert!(script.is_done());
    set.begin_batch(TimeUs(20_000));
    set.packet(&Packet {
        background: true,
        ..key(0, controls[0], 1)
    });
    set.end_batch(TimeUs(20_001));
    TRACK.with(|t| t.set(false));

    assert_eq!(popped, 2_000);
    assert!(readers[1].overflowed());
    assert!(set.overflowed());
    assert_eq!(set.arrivals().len(), 1);
    assert_eq!(set.arrivals_dropped(), 3);
    assert!(set.gate().is_tripped());
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0);
}
