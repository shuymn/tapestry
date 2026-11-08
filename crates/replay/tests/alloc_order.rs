//! A history is checked whole before anything owned is allocated for it:
//! a full event arena whose last event names an unknown source, or whose
//! openings disagree with the end status, is refused with no allocation
//! sized by the arena.
//!
//! A global allocator records the largest single allocation made on a
//! thread that opts in (a thread-local flag), so the test harness's own
//! threads do not disturb it. This file holds a single test for the same
//! reason.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use bms_model::{ChartIdentity, Speed, TimeUs};
use eval::{ControlId, InputKind, InputStart, PhysicalInputEvent, SourceId, SourceProfile};
use replay::{
    BuildProvenance, CancelReason, CancelledOpening, ChartRecord, DecodeError, DecodeLimits,
    DeviceKind, DeviceRecord, EndStatus, Incomplete, MAX_CANCELLED_EVENTS, PlayRecord, PlayType,
    PresetRecord, ReadyHistory, ReceiveMethod, SourceRecord, Status, TransformRecord, Versions,
    WakePolicy, decode, encode,
};

struct Largest;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn note(size: usize) {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        LARGEST.fetch_max(size, Ordering::Relaxed);
    }
}

// SAFETY: delegates to `System`; only records a size, which never
// allocates.
unsafe impl GlobalAlloc for Largest {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Largest = Largest;

/// The largest single allocation `decode` makes for `bytes`, and its
/// result.
fn largest(bytes: &[u8]) -> (usize, Result<PlayRecord, DecodeError>) {
    LARGEST.store(0, Ordering::Relaxed);
    TRACK.with(|t| t.set(true));
    let result = decode(bytes, &DecodeLimits::default());
    TRACK.with(|t| t.set(false));
    (LARGEST.load(Ordering::Relaxed), result)
}

/// A capture cancelled before playback with one keyboard source and one
/// opening, quit at origin `origin`, holding a full arena of events.
fn capture(origin: i64) -> PlayRecord {
    let events = (0..MAX_CANCELLED_EVENTS)
        .map(|i| PhysicalInputEvent {
            received: TimeUs(i as i64),
            source: SourceId(0),
            kind: InputKind::Control {
                control: ControlId(0x1e),
                value: i32::from(i % 2 == 0),
            },
        })
        .collect();
    PlayRecord {
        // A capture cancelled before playback has no starting tuning.
        start_tuning: None,
        versions: Versions {
            evaluator: 1,
            app: "0.1.0".to_owned(),
            provenance: BuildProvenance::Development,
        },
        chart: ChartRecord {
            identity: ChartIdentity {
                sha256: [0x11; 32],
                md5: [0x22; 16],
                resolved_hash: [0x33; 32],
                played_hash: [0x34; 32],
                loader_version: 1,
                random_results: Vec::new(),
            },
            random_seed: 1,
            transform: TransformRecord {
                id: "mirror".to_owned(),
                version: 2,
            },
        },
        preset: PresetRecord::of(&rules::tapestry()),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![SourceRecord {
                kind: DeviceKind::Keyboard,
                receive: ReceiveMethod::RawInput,
                profile: SourceProfile::default(),
                axis_out_of_range: 0,
            }],
            audio_offset: TimeUs(0),
            display_offset: TimeUs(0),
            wake: WakePolicy::Input {
                heartbeat_us: 1_000,
                input_gap_us: 125,
                drain_cut: 1_024,
            },
        },
        play_type: PlayType::Full,
        speed: Speed::NORMAL,
        start: InputStart::default(),
        events: Vec::new(),
        arrivals: Vec::new(),
        status: Status {
            end: EndStatus::CancelledBeforePlayback,
            finalized: TimeUs(0),
            incomplete: Incomplete::default(),
        },
        history: ReadyHistory {
            openings: vec![CancelledOpening {
                start: InputStart::default(),
                origin: TimeUs(origin),
                closed: TimeUs(MAX_CANCELLED_EVENTS as i64),
                events: 0..MAX_CANCELLED_EVENTS as u32,
                arrivals: 0..0,
                reason: CancelReason::Quit,
            }],
            events,
            arrivals: Vec::new(),
        },
    }
}

#[test]
fn a_history_refused_allocates_nothing_sized_by_its_arena() {
    let arena = MAX_CANCELLED_EVENTS * size_of::<PhysicalInputEvent>();
    let valid = capture(0);
    let (_, decoded) = largest(&encode(&valid));
    assert_eq!(decoded, Ok(valid));

    let mut unknown = capture(0);
    if let Some(last) = unknown.history.events.last_mut() {
        last.source = SourceId(1);
    }
    let (size, decoded) = largest(&encode(&unknown));
    assert_eq!(decoded, Err(DecodeError::UnknownSource(1)));
    assert!(
        size < arena,
        "{size} bytes for a source refused at the last event"
    );

    let (size, decoded) = largest(&encode(&capture(-1)));
    assert_eq!(decoded, Err(DecodeError::InvalidHistory("origin")));
    assert!(
        size < arena,
        "{size} bytes for an origin refused by the status"
    );
}
