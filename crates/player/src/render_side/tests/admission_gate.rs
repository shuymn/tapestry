//! The uploader's writes against the render thread's own admission gate
//! (design.md "Memory Budget", the uploader's admission gate), on the
//! production path: a WARP renderer's `upload_boundary_within` asked
//! through `PassGate`, built as `RenderLoop::upload_boundary` builds it,
//! for each payload kind (a staging's array, the loading text, the
//! attempt's chart images) and each event (the swap chain's grant, the
//! offscreen deadline, READY's cutoff asked, the hold's bound, the
//! payload superseded) arriving immediately before the payload's write k,
//! for k of 0, 1, 2 and never. A frame's event stops write k and every
//! later write of that boundary, the band kept: once it is gone the
//! payload resumes and ends as the control does, with the same writes. A
//! superseded payload ends with write k unwritten and is never published
//! or installed. The control, no event, writes every band and publishes.

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use render::{Renderer, UploadGate};

use super::super::{Ask, FrameDue, GATE_NOW, GateState, LoadingSlot, PassGate, admits, cutoff};

/// The swap chain's grant (one test thread uses it).
static GRANT: AtomicBool = AtomicBool::new(false);

/// An event's write: before which write of the payload, or never.
const NEVER: u64 = u64::MAX;
const AT: [u64; 4] = [0, 1, 2, NEVER];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Grant,
    Deadline,
    Cutoff,
    Hold,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Staging,
    LoadingText,
    Chart,
}

/// How a payload ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// Written whole and published or installed.
    Done,
    /// Ended superseded, nothing published or installed.
    Stale,
}

/// What a run came to: the writes made by the end of the boundary the
/// event arrived in (if it did), every write made, and how it ended.
#[derive(Clone, Copy, Debug)]
struct Outcome {
    at_event: Option<u64>,
    writes: u64,
    end: End,
}

/// The production gate, with the test's event set off immediately before
/// the payload's write `at` (counted from the run's first write in
/// `count`).
struct Inject<'a, G: UploadGate> {
    inner: G,
    count: &'a Cell<u64>,
    at: u64,
    fire: &'a dyn Fn(),
    fired: &'a Cell<bool>,
}

impl<G: UploadGate> UploadGate for Inject<'_, G> {
    fn past(&mut self) -> bool {
        self.inner.past()
    }

    fn current(&mut self, payload: render::Payload) -> bool {
        self.inner.current(payload)
    }

    fn admit_write(&mut self, payload: render::Payload) -> Result<(), render::Refused> {
        if self.count.get() == self.at && !self.fired.get() {
            (self.fire)();
            self.fired.set(true);
        }
        let admitted = self.inner.admit_write(payload);
        if admitted.is_ok() {
            self.count.set(self.count.get() + 1);
        }
        admitted
    }
}

/// The payload of `kind` in hand on a new renderer, with what keeps it.
struct Fixture {
    r: Renderer,
    _dirs: Vec<tempfile::TempDir>,
    set: Option<std::sync::Arc<crate::chart_images::ChartImages>>,
}

fn fixture(kind: Kind) -> Fixture {
    use crate::render_decl::{Look, bundle, solid_bundle};
    let mut r = crate::render_common::offscreen(1_920, 1_080);
    let chart = bms_model::ChartBuilder::new(120_000).build().unwrap();
    let ctx = crate::render_common::context(&crate::render_common::GAUGES, false);
    match kind {
        Kind::Staging => {
            let b = solid_bundle(&r, &Look::left([0, 255, 0, 255]), [1_800, 1_800]);
            let input = crate::render_common::attempt(&r, &chart, &ctx);
            assert_eq!(r.stage_stepped(&b, &input, 1).unwrap(), None);
            // Its array created off the frame: asked again until it is
            // imported, as the render thread asks between frames.
            let asked = std::time::Instant::now();
            while r.creating() {
                assert!(asked.elapsed() < Duration::from_secs(10), "never resident");
                std::thread::sleep(Duration::from_millis(1));
                assert_eq!(r.stage_stepped(&b, &input, 1).unwrap(), None);
            }
            Fixture {
                r,
                _dirs: Vec::new(),
                set: None,
            }
        }
        Kind::LoadingText => {
            use skin_model::{TextField, TextSlots};
            let dir = tempfile::tempdir().unwrap();
            let look = Look {
                loading: true,
                chart_text: true,
                ..Look::left([255, 0, 0, 255])
            };
            let (a, _) = bundle(&r, dir.path(), &look);
            crate::render_common::install(&mut r, &a, 1);
            r.enter_loading(1);
            let all = TextSlots::field(TextField::Title)
                .with(TextSlots::field(TextField::Artist))
                .with(TextSlots::field(TextField::Genre));
            let text = skin_model::ChartText {
                title: "Title of a chart",
                subtitle: "",
                artist: "An artist",
                subartist: "",
                genre: "A genre",
            };
            r.prepare_loading_text(1, 1, all, &text);
            assert_eq!(r.loading_text_in_hand(), Some((1, 1)));
            Fixture {
                r,
                _dirs: vec![dir],
                set: None,
            }
        }
        Kind::Chart => {
            let (dir, b) = super::chart_fixture::charted(&r);
            crate::render_common::install(&mut r, &b, 1);
            let (sets, set) = super::chart_fixture::chart_set(b.side());
            Fixture {
                r,
                _dirs: vec![dir, sets],
                set: Some(set),
            }
        }
    }
}

/// One run: `kind`'s payload stepped by boundaries through the production
/// gate, `event` set off before write `at` and gone again after that
/// boundary, until the payload ends.
fn run(kind: Kind, event: Event, at: u64) -> Outcome {
    let mut f = fixture(kind);
    let start = Instant::now();
    let deadline = start + Duration::from_secs(1);
    GRANT.store(false, Ordering::SeqCst);
    GATE_NOW.with(|c| c.set(Some(start)));
    let word = AtomicU32::new(cutoff::RUNNING);
    let hold = Cell::new(false);
    let generation = AtomicU64::new(1);
    let slot = Mutex::new(LoadingSlot {
        lifetime: 1,
        ..LoadingSlot::default()
    });
    let fire = || match event {
        Event::Grant => GRANT.store(true, Ordering::SeqCst),
        Event::Deadline => GATE_NOW.with(|c| c.set(Some(deadline))),
        Event::Cutoff => {
            word.fetch_or(cutoff::ASKED, Ordering::SeqCst);
        }
        Event::Hold => hold.set(true),
        Event::Stale => match kind {
            Kind::Staging => unreachable!("a staging is its renderer's own"),
            Kind::LoadingText => {
                slot.lock().unwrap().lifetime = 2;
            }
            Kind::Chart => {
                generation.fetch_add(1, Ordering::SeqCst);
            }
        },
    };
    let (count, fired) = (Cell::new(0), Cell::new(false));
    let before = f.r.queue_writes();
    let mut chart_up = f.set.as_ref().map(|set| {
        f.r.begin_chart_upload(render::ChartTarget::Committed, set.fitted())
            .unwrap()
    });
    let mut at_event = None;
    for _ in 0..2_000 {
        let mut granted = false;
        let gate = PassGate {
            frame: if event == Event::Deadline {
                FrameDue::At(deadline)
            } else {
                FrameDue::Grant(render::GrantProbe::Flag(&GRANT))
            },
            cut_off: false,
            cutoff: Some(&word),
            hold: Some(|| hold.get()),
            chart: (kind == Kind::Chart).then_some((&generation, 1)),
            bga: None,
            loading: (kind == Kind::LoadingText).then_some((1, 1)),
            slot: (kind == Kind::LoadingText).then_some(&slot),
            granted: &mut granted,
        };
        let was_fired = fired.get();
        let mut gate = Inject {
            inner: gate,
            count: &count,
            at,
            fire: &fire,
            fired: &fired,
        };
        let chart = match (&mut chart_up, &f.set) {
            (Some(up), Some(set)) => Some((up, set.fitted())),
            _ => None,
        };
        let out =
            f.r.upload_boundary_within(Duration::from_secs(60), &mut gate, chart);
        let writes = f.r.queue_writes() - before;
        assert_eq!(writes, count.get(), "a write the gate did not admit");
        if fired.get() && !was_fired {
            at_event = Some(writes);
            // The event is gone after its boundary: the frame drawn, the
            // cutoff ended, the hold ended, the deadline moved on.
            GRANT.store(false, Ordering::SeqCst);
            GATE_NOW.with(|c| c.set(Some(start)));
            word.store(cutoff::RUNNING, Ordering::SeqCst);
            hold.set(false);
        }
        let end = match kind {
            Kind::Staging => out.staged.map(|e| match e {
                render::StagingEnd::Ready(_) => End::Done,
                render::StagingEnd::Refused(_) => panic!("refused: {e:?}"),
            }),
            Kind::LoadingText => out.text.map(|t| match t.result {
                Ok(_) => End::Done,
                Err(render::TextFault::Install(render::LoadingTextError::Stale)) => End::Stale,
                Err(e) => panic!("{e:?}"),
            }),
            Kind::Chart => out.chart.map(|c| match c {
                Ok(_) => End::Done,
                Err(render::RenderError::ChartChanged) => End::Stale,
                Err(e) => panic!("{e:?}"),
            }),
        };
        if let Some(end) = end {
            GATE_NOW.with(|c| c.set(None));
            return Outcome {
                at_event,
                writes,
                end,
            };
        }
        if f.r.step_in_flight() {
            assert!(f.r.wait_step(Duration::from_secs(10)).unwrap());
        }
    }
    panic!("{kind:?} {event:?} {at}: never ended");
}

/// Every payload kind, every event and every write k: a frame's event
/// stops write k and the boundary's later writes, and the payload then
/// ends as the control does with the same writes; a superseded payload
/// ends with k writes, unpublished; the control writes every band.
// Revert: the gate read before a band's preparation rather than
// immediately before its write (`write_band` writing without
// `admit_write`), or the currency read at the boundary's entry (the
// generation or the slot read once into the gate) -> a write after the
// event: "writes by the event's boundary".
#[test]
fn every_write_passes_the_gate_immediately_before_it() {
    for kind in [Kind::Staging, Kind::LoadingText, Kind::Chart] {
        let control = run(kind, Event::Grant, NEVER);
        assert_eq!(control.end, End::Done, "{kind:?}: the control");
        assert_eq!(control.at_event, None);
        assert!(control.writes > 2, "{kind:?}: {control:?}");
        for event in [
            Event::Grant,
            Event::Deadline,
            Event::Cutoff,
            Event::Hold,
            Event::Stale,
        ] {
            if event == Event::Stale && kind == Kind::Staging {
                continue;
            }
            for at in AT {
                let o = run(kind, event, at);
                let case = format!("{kind:?} {event:?} at {at}: {o:?}");
                if at == NEVER {
                    assert_eq!((o.end, o.writes), (End::Done, control.writes), "{case}");
                    continue;
                }
                assert_eq!(
                    o.at_event,
                    Some(at),
                    "writes by the event's boundary: {case}"
                );
                if event == Event::Stale {
                    assert_eq!((o.end, o.writes), (End::Stale, at), "{case}");
                } else {
                    assert_eq!((o.end, o.writes), (End::Done, control.writes), "{case}");
                }
            }
        }
    }
}

/// The gate's single rule, case by case: each source alone stops a write
/// and a heavy operation, the allowance only the heavy operation.
#[test]
fn each_source_alone_stops_what_it_governs() {
    let none = GateState::default();
    assert!(admits(none, Ask::Heavy) && admits(none, Ask::Write));
    for s in [
        GateState {
            frame_due: true,
            ..none
        },
        GateState {
            cut_off: true,
            ..none
        },
        GateState {
            hold_past: true,
            ..none
        },
    ] {
        assert!(!admits(s, Ask::Heavy), "{s:?}");
        assert!(!admits(s, Ask::Write), "{s:?}");
    }
    let spent = GateState {
        heavy_spent: true,
        ..none
    };
    assert!(!admits(spent, Ask::Heavy));
    assert!(admits(spent, Ask::Write));
}
