//! After an attempt's source table and the adjustment tap are made,
//! repeated openings allocate nothing: batches with tapped presses, key
//! repeats and axis movement while recording is open, a full tap queue
//! and its resynchronization, a removal and a reconnection, draining and
//! giving back the readers, and reopening on a new timeline each time.
//!
//! A counting global allocator counts allocations made on threads that
//! opted in (a thread-local flag); this file holds a single test so the
//! harness's own threads do not disturb the count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::TimeUs;
use eval::ControlId;
use input::hid::{AxisCap, HidProduct, axis_control};
use input::{
    AdjustmentBindings, DeviceId, DeviceKind, Packet, SourceConfig, SourceSet, TapControl,
    TapEvent, adjustment_queue,
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

const PRODUCT: HidProduct = HidProduct {
    vendor_id: 1,
    product_id: 2,
    interface: None,
};

#[test]
fn repeated_openings_allocate_nothing() {
    let (a, q) = (ControlId(0x1e), ControlId(0x10));
    let turn = axis_control(0x01, 0x30);
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 4,
        max_sources: 4,
        max_held: 4,
        queue_capacity: 64,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    for d in [1, 2] {
        set.connect(DeviceId(d), DeviceKind::Keyboard);
        set.set_recorded(DeviceId(d), vec![a, q]);
    }
    set.connect(DeviceId(3), DeviceKind::Controller);
    set.set_product(DeviceId(3), PRODUCT);
    set.add_axes(
        DeviceId(3),
        &[AxisCap {
            control: turn,
            relative: false,
            wrap: 256,
            min: 0,
            max: 255,
            bits: 8,
            signed: false,
        }],
    );
    set.set_recorded(DeviceId(3), vec![turn]);
    set.end_batch(TimeUs(1));
    let (mut tap, mut rx) = adjustment_queue(AdjustmentBindings {
        controls: vec![
            (TapControl::Key(q), 0),
            (
                TapControl::Controller {
                    product: PRODUCT,
                    control: turn,
                },
                1,
            ),
        ],
        latched: 1,
        wrap: None,
    })
    .unwrap();
    let latch = tap.latch();
    tap.set_active(true);
    set.set_adjustment(tap);
    set.open_attempt(TimeUs(10), TimeUs(10));
    let mut readers = set.take_readers();
    let packet = |device, control, value| Packet {
        device: DeviceId(device),
        control,
        value,
        background: false,
    };

    TRACK.with(|t| t.set(true));
    // The count works: an allocation that escapes is counted in any profile.
    drop(std::hint::black_box(vec![1u8; 8]));
    assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 1);
    let (mut recorded, mut observed, mut resyncs) = (0, 0, 0);
    let mut at = 100;
    for opening in 0..200_i64 {
        for step in 0..20_u64 {
            at += 10;
            set.begin_batch(TimeUs(at));
            set.packet(&packet(1 + step % 2, q, i32::from(step % 4 < 2)));
            set.packet(&packet(1, a, 1));
            set.packet(&packet(
                3,
                turn,
                ((opening as u64 * 20 + step) * 37 % 256) as i32,
            ));
            if opening == 100 && step == 0 {
                set.disconnect(DeviceId(2));
                set.connect(DeviceId(2), DeviceKind::Keyboard);
            }
            set.end_batch(TimeUs(at + 1));
        }
        recorded += readers
            .iter_mut()
            .map(|r| std::iter::from_fn(|| r.pop()).count())
            .sum::<usize>();
        // The tap is drained every 40th opening, so its queue fills.
        if opening % 40 == 39 {
            while let Some(o) = rx.pop() {
                observed += 1;
                resyncs += usize::from(matches!(o.event, TapEvent::Resync(_)));
            }
        }
        set.return_readers(readers);
        at += 10;
        let start = set.reopen(TimeUs(at), TimeUs(at)).unwrap();
        assert!(start.held.len() <= 3);
        readers = set.take_readers();
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0);
    assert!(recorded > 8_000);
    assert!(observed > 1_000);
    assert!(resyncs > 1, "the tap's queue overflowed and resynchronized");
    assert!(latch.read().presses > 100);
}
