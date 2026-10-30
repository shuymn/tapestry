//! An autoplay session's own buffers in the recording reservation (design.md
//! "Autoplay", "Recording Capacity"), measured: the source queue that holds the
//! whole stream, the generated events and the merged schedule are charged as
//! allocated, at their peak, and a chart whose charges take it just over the
//! budget is refused before any of them is allocated.
//!
//! A global allocator sums the bytes allocated on threads that opted in;
//! this file holds a single test. It runs in the dev profile only: it
//! measures buffers that are built and dropped unread, which an optimized
//! build may leave out.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use audio::{AudioEngine, MAX_VOICES};
use bms_model::{
    BgmEvent, Chart, ChartBuilder, HeaderValue, JudgmentHeader, JudgmentHeaderName, SoundId, TimeUs,
};
use common::{MS, custom_settings, lane};
use eval::{EvalError, PhysicalInputEvent};
use game::{
    AutoplayError, ChartIngest, DataRoot, DeviceSettings, IngestError, Ingested, KeyConfig, Loaded,
    LogicError, Parts, RECORDING_MEMORY, Reservation, SessionConfig,
};
use input::{DeviceKind, source_queue};
use replay::{BuildProvenance, ReceiveMethod};

struct Summing;

static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn add(n: usize) {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        BYTES.fetch_add(n, Ordering::Relaxed);
    }
}

// SAFETY: delegates to `System`; only adds to a counter, which never
// allocates.
unsafe impl GlobalAlloc for Summing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        add(layout.size());
        // SAFETY: same contract as the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        add(new_size);
        // SAFETY: same contract as the caller.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Summing = Summing;

/// The bytes `f` allocates, and what it returns.
fn measured<T>(f: impl FnOnce() -> T) -> (usize, T) {
    BYTES.store(0, Ordering::Relaxed);
    TRACK.with(|t| t.set(true));
    let out = f();
    TRACK.with(|t| t.set(false));
    (BYTES.load(Ordering::Relaxed), out)
}

struct Built(Chart);

impl ChartIngest for Built {
    fn max_bytes(&self) -> usize {
        1 << 20
    }

    fn accepts(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "chart")
    }

    fn ingest(&self, _bytes: &[u8], _dir: &Path, _seed: u64) -> Result<Ingested, IngestError> {
        Ok(Ingested {
            chart: self.0.clone(),
            warnings: Vec::new(),
        })
    }
}

const EFFECTS: usize = 4_096;

/// `notes` sounded taps over the eight lanes, 1 ms apart, and `bgm` BGM
/// events at the start.
fn chart(notes: i64, bgm: usize) -> Chart {
    let mut b = ChartBuilder::new(150_000).sound("k.wav");
    for i in 0..notes {
        b = b.tap(
            lane(u8::try_from(i % 8).unwrap()),
            TimeUs(1_000 * MS + i * MS),
        );
    }
    let mut chart = b.build().unwrap();
    for n in chart.lanes.iter_mut().flatten() {
        n.sound = Some(SoundId(0));
    }
    chart.bgm = vec![
        BgmEvent {
            time: TimeUs(0),
            sound: SoundId(0),
        };
        bgm
    ];
    chart
}

fn loaded(dir: &Path, chart: &Chart) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let settings = custom_settings(&root);
    let path = dir.join("play.chart");
    std::fs::write(&path, b"built in code").unwrap();
    let parts = Parts {
        ingest: Box::new(Built(chart.clone())),
        ..Parts::unsaved(&settings.player)
    };
    let config = SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: EFFECTS,
        seed: 5,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: Some(game::AutoKeySounds::Scheduled),
    };
    Loaded::load(root, settings, &path, config, parts).unwrap()
}

/// What an autoplay play of `chart` reserves, counted here on its own: the
/// play's reservation for a window of exactly its events, plus the queue
/// holding them all, the generated events and the merged schedule.
fn expected(chart: &Chart) -> (usize, usize) {
    let device = DeviceSettings {
        keyboard: KeyConfig::autoplay(),
        input_offset_us: 0,
        ..DeviceSettings::default()
    }
    .profile(&[DeviceKind::Keyboard], &[Vec::new()]);
    let notes: usize = chart.lanes.iter().map(Vec::len).sum();
    let events = 2 * notes;
    let schedule = chart.bgm.len() + notes;
    let r = Reservation::of(chart, &rules::custom(), &device, EFFECTS).unwrap();
    let own = 2 * events * size_of::<PhysicalInputEvent>() + schedule * size_of::<BgmEvent>();
    (r.bytes(events) + own, own)
}

#[test]
fn autoplay_buffers_are_charged_and_admitted_before_they_exist() {
    let dir = tempfile::Builder::new()
        .prefix("autoplay-reserve")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();

    // What the session allocates for itself is what is charged: the
    // events and the schedule in `autoplay` (beyond its plan's probe, which
    // is freed), and the queue the input side makes for each play. Each
    // returns the bytes allocated beyond the charge.
    let unaccounted = |c: &Chart| {
        let session = loaded(dir.path(), c);
        let (plan_bytes, plan) = measured(|| session.plan(&[DeviceKind::Keyboard], &[Vec::new()]));
        assert!(plan.is_ok());
        let (made_bytes, made) = measured(|| session.autoplay().unwrap());
        let (queue_bytes, queue) = measured(|| source_queue(made.events.len()));
        drop(queue);
        let own = made_bytes - plan_bytes + queue_bytes;
        let (_, charged) = expected(c);
        let charge = Reservation {
            fixed: 0,
            per_input: 0,
        }
        .with_autoplay(made.events.len(), made.schedule.len())
        .fixed;
        assert_eq!(charge, charged);
        assert!(own >= charge, "{own} allocated, {charge} charged");
        own - charge
    };
    // Beyond the charges, only small fixed parts (the recorded control
    // list, the queue's shared state, copies of the key config while
    // planning): the same for a chart twice the size, so nothing that
    // grows with the chart (a sort's workspace, a vector grown by
    // doubling, a second copy of the schedule) is left uncharged.
    let (a, b) = (
        unaccounted(&chart(3_000, 40)),
        unaccounted(&chart(6_000, 80)),
    );
    assert_eq!(a, b);
    assert!(a <= 1_024, "{a} bytes unaccounted");
    // The audio engine takes the schedule as it is: building it with the
    // schedule allocates what it does with none.
    let small = chart(3_000, 40);
    let schedule = loaded(dir.path(), &small).autoplay().unwrap().schedule;
    let bank = common::bank(&small);
    let engine = |s: Vec<BgmEvent>| {
        let bank = std::sync::Arc::clone(&bank);
        measured(move || {
            AudioEngine::with_schedule(bank, s, common::RATE, common::PERIOD, MAX_VOICES, 0)
                .unwrap()
        })
        .0
    };
    assert!(!schedule.is_empty());
    assert_eq!(engine(schedule), engine(Vec::new()));

    // A chart whose autoplay charges take it just over the budget (by at
    // most one schedule entry) is refused, and allocates nothing beyond
    // its plan's probe; one BGM event fewer fits. Without any one of the
    // charges it would fit.
    let per_note = {
        let (a, _) = expected(&chart(1_000, 0));
        let (b, _) = expected(&chart(2_000, 0));
        (b - a) / 1_000
    };
    let (base, _) = expected(&chart(1_000, 0));
    let notes = (RECORDING_MEMORY - base) / per_note + 1_000 - 2;
    let (below, own) = expected(&chart(i64::try_from(notes).unwrap(), 0));
    assert!(below <= RECORDING_MEMORY, "{below}");
    let entry = size_of::<BgmEvent>();
    let bgm = (RECORDING_MEMORY - below) / entry + 1;
    let over = chart(i64::try_from(notes).unwrap(), bgm);
    let (total, own_over) = expected(&over);
    assert!(
        total > RECORDING_MEMORY && total - RECORDING_MEMORY <= entry,
        "{total}"
    );
    assert!(own > entry && own_over - own == bgm * entry);
    let session = loaded(dir.path(), &over);
    let (plan_bytes, plan) = measured(|| session.plan(&[DeviceKind::Keyboard], &[Vec::new()]));
    assert!(
        matches!(plan, Err(LogicError::RecordingTooLarge { .. })),
        "{plan:?}"
    );
    let (refused_bytes, refused) = measured(|| session.autoplay());
    assert!(
        matches!(
            refused,
            Err(AutoplayError::Plan(LogicError::RecordingTooLarge { .. }))
        ),
        "{refused:?}"
    );
    assert_eq!(refused_bytes, plan_bytes);
    drop(session);
    let fits = chart(i64::try_from(notes).unwrap(), bgm - 1);
    assert!(expected(&fits).0 <= RECORDING_MEMORY);
    let session = loaded(dir.path(), &fits);
    assert!(session.plan(&[DeviceKind::Keyboard], &[Vec::new()]).is_ok());
    drop(session);

    // The same over-budget chart with a rank the evaluator refuses
    // (`#DEFEXRANK 3000000`, edges past 2^32 µs): the refusal comes from the
    // plan too, before the stream is made, never an admission that skipped
    // the check.
    let mut unranked = over;
    unranked.meta.judgment_headers = vec![JudgmentHeader {
        name: JudgmentHeaderName::DefExRank,
        value: HeaderValue::integer(3_000_000),
    }];
    let session = loaded(dir.path(), &unranked);
    let refusal = LogicError::Eval(EvalError::InvalidRank);
    let (plan_bytes, plan) = measured(|| session.plan(&[DeviceKind::Keyboard], &[Vec::new()]));
    assert_eq!(plan.err(), Some(refusal.clone()));
    let (refused_bytes, refused) = measured(|| session.autoplay());
    assert_eq!(refused.err(), Some(AutoplayError::Plan(refusal)));
    assert_eq!(refused_bytes, plan_bytes);
    let stream = 2 * notes * size_of::<PhysicalInputEvent>();
    assert!(
        refused_bytes < stream,
        "{refused_bytes} of a {stream}-byte stream"
    );
}
