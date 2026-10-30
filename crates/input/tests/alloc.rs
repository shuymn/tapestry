//! After construction and `open_recording`, batches allocate nothing:
//! packets, pushes, publications, device changes, arrivals, overflow and
//! focus rejection, a scripted connect and reconnect, and the wake loop
//! with its cut drains.
//!
//! A counting global allocator counts allocations made on threads that opted
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.
//! Profiles and the positive control: design.md "Scope of No Allocation".

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use bms_model::{TimeSource, TimeUs};
use eval::ControlId;
use input::cadence::{self, Boundary, Cadence, Driven, Wait, WaitFailed, Waiter, Woken};
use input::{
    Action, DeviceId, DeviceKind, InputAdapter, Packet, Phase, ScriptItem, ScriptedInput,
    SourceConfig, SourceSet, WakeReport,
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

/// A virtual clock in µs.
#[derive(Clone)]
struct Clock(Arc<AtomicI64>);

impl TimeSource for Clock {
    fn now(&self) -> TimeUs {
        TimeUs(self.0.load(Ordering::Relaxed))
    }
}

/// Waits that return in turn for input, the event and the timer, each
/// 50 µs later.
struct Stub {
    clock: Clock,
    n: u32,
}

impl Waiter for Stub {
    fn wait(&mut self, wait: Wait) -> Result<Woken, WaitFailed> {
        self.n += 1;
        self.clock.0.fetch_add(50, Ordering::Relaxed);
        Ok(match self.n % 3 {
            0 if wait.input => Woken::Input,
            1 => Woken::Event,
            _ => Woken::Timer,
        })
    }
}

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
    set.gate_mut().reset();

    // The wake loop: every kind of wake, with cut drains, on a virtual
    // clock; its report and state are made outside the counted section.
    let clock = Clock(Arc::new(AtomicI64::new(30_000)));
    let stop = AtomicBool::new(false);
    let mut waiter = Stub {
        clock: clock.clone(),
        n: 0,
    };
    let mut cadence = Cadence::new(Driven::Input, clock.now());
    let mut report = WakeReport::new();
    let mut wakes = 0_u32;
    let mut looped = 0_u64;
    TRACK.with(|t| t.set(true));
    let ran = cadence::run(
        &clock,
        &mut waiter,
        &stop,
        &mut cadence,
        &mut report,
        |now, may_read| {
            wakes += 1;
            // Every tenth wake has more queued than one drain reads.
            let mut queued = if wakes.is_multiple_of(10) { 1_500 } else { 3 };
            let (published, n) = cadence::batch(&clock, &mut set, now, may_read, |set, budget| {
                if queued == 0 {
                    return 0;
                }
                queued -= 1;
                set.packet(&key(0, controls[0], i32::from(budget % 2 == 0)));
                1
            });
            while readers[0].pop().is_some() {
                looped += 1;
            }
            if wakes == 2_000 {
                stop.store(true, Ordering::Release);
            }
            Boundary {
                published,
                read: n,
                phase: if wakes.is_multiple_of(7) {
                    Phase::Idle
                } else {
                    Phase::Open
                },
            }
        },
    );
    TRACK.with(|t| t.set(false));
    assert_eq!(ran, Ok(()));
    assert_eq!(wakes, 2_000);
    assert!(report.cuts >= 100, "{}", report.cuts);
    assert!(looped > 2_000, "{looped}");
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0);
}
