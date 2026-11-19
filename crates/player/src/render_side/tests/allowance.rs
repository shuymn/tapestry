//! The frame interval's heavy operation on the real render loop (design.md
//! "Load Pipeline", the allowance; "Memory Budget", READY's cutoff), on a
//! fake screen whose heavy operations take injected durations and report
//! through the retirement API's methods (`heavy_ops`, `due_heavy`,
//! `destroy_due`, `filled`, `pending_completion`, `grant_ready`), its
//! frames paced by the shared frame gate ([`super::gate::Gate`]). The
//! fake notes each operation it ran with the frames it presented before;
//! the loop's boundary record (`RenderLink::record_heavy`) adds each
//! boundary operation's regime.
//!
//! 2a makes no skin array at a boundary: where a case of the contract
//! names a creation falling due, a due filled entry's destruction (a
//! boundary heavy operation in 2a) stands for it, and the test says so.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Mutex, PoisonError};

use skin_model::AtlasId;
use skin_model::bga::{BgaKey, BgaTable};

use super::gate::Gate;
use super::*;
use crate::bga::{Answer, BgaMsg};
use crate::render_side::{CutoffPhase, HeavyKind, Regime};

/// Hang detection only.
const BOUND: Duration = Duration::from_secs(20);

/// The clock's pace between two heavy operations.
const PACE: Duration = Duration::from_millis(16);

/// A heavy operation the fake ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    /// A filled entry's destruction, by the entry's number.
    Destroy(u32),
    BgaBegin,
    /// The array in use dropped (a drop, or an absent publication's).
    BgaDrop,
    Regrowth,
}

/// One operation and the frames presented before it began.
#[derive(Clone, Copy, Debug)]
struct Ran {
    op: Op,
    start: Instant,
    end: Instant,
    shown: u64,
}

/// What a frame was drawn as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Page,
    Selection,
    Splash,
}

#[derive(Default)]
struct State {
    /// The filled entries, oldest first: their numbers and how long each
    /// destruction takes.
    due: VecDeque<(u32, Duration)>,
    heavy: u64,
    last_end: Option<Instant>,
    ran: Vec<Ran>,
    /// The frames presented.
    shown: u64,
    /// Each frame drawn: its kind, when, presented.
    frames: Vec<(Kind, Instant, bool)>,
    bga_arrays: u64,
    bga_live: Option<u64>,
    bga_table: Option<Arc<BgaTable>>,
    /// Each regrowth made a new generation.
    pool_generation: u64,
    /// Result-kind changes (each destruction counts one).
    result_changes: u64,
}

/// The fake's state, shared with the test.
#[derive(Default)]
struct Fake {
    gate: Gate,
    state: Mutex<State>,
    /// Frames are drawn but not presented (`Retry`).
    unpresented: AtomicBool,
    /// The window is minimized: frames are paced by the clock.
    minimized: AtomicBool,
    /// The filled entries' completion is not seen: none is due.
    held: AtomicBool,
    /// The next frame fails as a lost device.
    lose: AtomicBool,
    /// The next completion poll fails.
    poll_error: AtomicBool,
    /// A regrowth has a larger pool to make (a selection entry's); the
    /// screen's build asks for one too, which makes nothing otherwise.
    regrows: AtomicBool,
    /// Run on the render thread at the next heavy operation's end.
    on_end: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// A result atlas the filled entries hold, named by the result's
    /// report while one is filled.
    result_id: Mutex<Option<AtlasId>>,
    /// The oldest entry's CPU part, handed once its completion is seen.
    cpu: Mutex<Option<render::CpuParts>>,
    /// The entries' completion is seen but their destruction is not due
    /// (their arrays kept): only their CPU parts can go.
    keep: AtomicBool,
    /// CPU parts taken for the worker, put back, and dropped with their
    /// arrays on the render thread.
    taken: AtomicU64,
    returned: AtomicU64,
    dropped_with_array: AtomicU64,
    /// The last per-pass permission to destroy in a staging's call.
    destroy_allowed: AtomicBool,
}

impl Fake {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `op` for `took`, counted as a heavy operation.
    fn run(&self, op: Op, took: Duration) {
        let (start, shown) = (Instant::now(), self.state().shown);
        if !took.is_zero() {
            std::thread::sleep(took);
        }
        let end = Instant::now();
        {
            let mut s = self.state();
            s.heavy += 1;
            s.last_end = Some(end);
            s.ran.push(Ran {
                op,
                start,
                end,
                shown,
            });
        }
        let hook = self
            .on_end
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// One frame drawn: `Shown` when presented, else `Invisible` while
    /// minimized (as the renderer ends a minimized frame) and `Retry`
    /// otherwise.
    fn frame(&self, kind: Kind) -> FrameTiming {
        let presented = !self.unpresented.load(Ordering::SeqCst);
        let mut s = self.state();
        s.frames.push((kind, Instant::now(), presented));
        if presented {
            s.shown += 1;
        }
        let outcome = if presented {
            render::FrameOutcome::Shown
        } else if self.minimized.load(Ordering::SeqCst) {
            render::FrameOutcome::Invisible
        } else {
            render::FrameOutcome::Retry
        };
        crate::render_side::timing_of(outcome)
    }
}

struct Timed(Arc<Fake>);

impl Screen for Timed {
    fn load(
        &mut self,
        _chart: &Chart,
        _section: Option<&eval::Section>,
    ) -> Result<(), RenderError> {
        Ok(())
    }
    fn wait(&self) -> bool {
        self.0.gate.wait()
    }
    fn paced(&self) -> bool {
        self.0.gate.pacing()
    }
    fn grant_ready(&self) -> bool {
        self.0.gate.take_ready()
    }
    fn minimized(&self) -> bool {
        self.0.minimized.load(Ordering::SeqCst)
    }
    fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
        Ok(())
    }
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        Ok(self.0.frame(Kind::Page))
    }
    fn menu(
        &mut self,
        _page: &crate::menu_screen::MenuPage,
        _changed: bool,
    ) -> Result<FrameTiming, RenderError> {
        if self.0.lose.swap(false, Ordering::SeqCst) {
            return Err(RenderError::DeviceLost);
        }
        Ok(self.0.frame(Kind::Page))
    }
    fn selection(
        &mut self,
        _update: &crate::selection_skin::SelectionUpdate,
        _logic: Option<&mut crate::selection_skin::SelectionLogic>,
        _changed: bool,
        _now: TimeUs,
        _app: skin_model::AppReadouts,
    ) -> Result<crate::selection_skin::SelectionDrawn, RenderError> {
        Ok(crate::selection_skin::SelectionDrawn {
            timing: self.0.frame(Kind::Selection),
            refused: None,
            skinned: false,
        })
    }
    fn splash(&mut self, _elapsed: Duration) -> Option<Result<render::FrameOutcome, RenderError>> {
        if self.0.lose.swap(false, Ordering::SeqCst) {
            return Some(Err(RenderError::DeviceLost));
        }
        Some(Ok(self.0.frame(Kind::Splash).outcome))
    }
    fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
        Ok(true)
    }
    fn poll_completion(&mut self) -> Result<(), RenderError> {
        if self.0.poll_error.swap(false, Ordering::SeqCst) {
            return Err(RenderError::DeviceLost);
        }
        Ok(())
    }
    fn filled(&self) -> u32 {
        u32::try_from(self.0.state().due.len()).unwrap()
    }
    fn due_heavy(&self) -> bool {
        !self.0.held.load(Ordering::SeqCst)
            && !self.0.keep.load(Ordering::SeqCst)
            && !self.0.state().due.is_empty()
    }
    fn set_destroy_allowed(&mut self, allowed: bool) {
        self.0.destroy_allowed.store(allowed, Ordering::SeqCst);
    }
    fn take_handoff(&mut self) -> Option<render::Handoff> {
        if self.0.held.load(Ordering::SeqCst) || self.0.state().due.is_empty() {
            return None;
        }
        let cpu = self
            .0
            .cpu
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()?;
        self.0.taken.fetch_add(1, Ordering::SeqCst);
        Some(render::Handoff {
            entry: 0,
            id: *self
                .0
                .result_id
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            kind: ScreenKind::Result,
            bytes: 1,
            cpu,
        })
    }
    fn return_handoff(&mut self, _entry: usize, cpu: render::CpuParts) -> bool {
        self.0.returned.fetch_add(1, Ordering::SeqCst);
        *self.0.cpu.lock().unwrap_or_else(PoisonError::into_inner) = Some(cpu);
        true
    }
    fn pending_completion(&self) -> bool {
        self.0.held.load(Ordering::SeqCst) && !self.0.state().due.is_empty()
    }
    fn destroy_due(&mut self, heavy: bool) -> render::Destroyed {
        if !heavy || self.0.held.load(Ordering::SeqCst) || self.0.keep.load(Ordering::SeqCst) {
            return render::Destroyed::default();
        }
        let next = self.0.state().due.pop_front();
        if let Some((n, took)) = next {
            if self
                .0
                .cpu
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .is_some()
            {
                self.0.dropped_with_array.fetch_add(1, Ordering::SeqCst);
            }
            self.0.run(Op::Destroy(n), took);
            self.0.state().result_changes += 1;
        }
        render::Destroyed::default()
    }
    fn result_changes(&self) -> u64 {
        self.0.state().result_changes
    }
    fn retained_atlases(&self, kind: ScreenKind) -> [Option<AtlasId>; render::RETAINED_ATLASES] {
        let mut ids = [None; render::RETAINED_ATLASES];
        if kind == ScreenKind::Result && !self.0.state().due.is_empty() {
            ids[3] = *self
                .0
                .result_id
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
        }
        ids
    }
    fn heavy_ops(&self) -> render::HeavyOps {
        let s = self.0.state();
        render::HeavyOps {
            count: s.heavy,
            last_end: s.last_end,
        }
    }
    fn regrow_pool(&mut self) {
        if self.0.regrows.load(Ordering::SeqCst) {
            self.0.run(Op::Regrowth, Duration::ZERO);
            self.0.state().pool_generation += 1;
        }
    }
    fn bga_in_use(&self) -> bool {
        self.0.state().bga_live.is_some()
    }
    fn begin_bga(&mut self, _pages: u32) -> Result<BgaKey, render::BgaFault> {
        self.0.run(Op::BgaBegin, Duration::ZERO);
        let mut s = self.0.state();
        s.bga_arrays += 1;
        s.bga_live = Some(s.bga_arrays);
        s.bga_table = None;
        Ok(BgaKey {
            renderer: 1,
            array: s.bga_arrays,
        })
    }
    fn publish_bga(
        &mut self,
        key: Option<BgaKey>,
        entries: Vec<skin_model::bga::BgaEntry>,
    ) -> Result<Arc<BgaTable>, render::BgaFault> {
        let table = BgaTable::new(entries);
        let table = match key {
            Some(k) if self.0.state().bga_live == Some(k.array) => Arc::new(table.keyed(k)),
            Some(_) => return Err(render::BgaFault::Stale),
            None => {
                if self.bga_in_use() {
                    self.0.run(Op::BgaDrop, Duration::ZERO);
                }
                self.0.state().bga_live = None;
                Arc::new(table)
            }
        };
        self.0.state().bga_table = Some(Arc::clone(&table));
        Ok(table)
    }
    fn published_bga(&self, key: BgaKey) -> Option<Arc<BgaTable>> {
        let s = self.0.state();
        s.bga_table
            .as_ref()
            .filter(|t| t.key() == Some(key) && s.bga_live == Some(key.array))
            .cloned()
    }
    fn drop_bga(&mut self) {
        if self.bga_in_use() {
            self.0.run(Op::BgaDrop, Duration::ZERO);
        }
        let mut s = self.0.state();
        s.bga_live = None;
        s.bga_table = None;
    }
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        None
    }
}

/// A render thread on a [`Timed`] screen.
struct Rig {
    link: super::super::RenderLink,
    fake: Arc<Fake>,
}

impl Rig {
    /// The launch screen's time not over: the first frames are its.
    fn launching() -> Self {
        let fake = Arc::new(Fake::default());
        let f = Arc::clone(&fake);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(Timed(Arc::clone(&f))) as Box<dyn Screen>));
        let link = spawn_empty(make);
        link.record_heavy(64);
        Self { link, fake }
    }

    /// Past the launch screen.
    fn new() -> Self {
        let r = Self::launching();
        r.link.set_launch_elapsed(Some(Duration::from_secs(3_600)));
        r
    }

    /// Drawing a menu page, its frames held at the gate.
    fn page() -> Self {
        let r = Self::new();
        r.link.send(RenderMsg::Menu(Box::default()));
        r.until_frames(|f| f.iter().any(|(k, _, _)| *k == Kind::Page));
        r.fake.gate.hold();
        r
    }

    /// Drawing selection lifetime 1, its frames held at the gate.
    fn selection() -> Self {
        let r = Self::new();
        r.select(1);
        r.until_frames(|f| f.iter().any(|(k, _, _)| *k == Kind::Selection));
        r.fake.gate.hold();
        r
    }

    fn select(&self, lifetime: u64) {
        self.link.enter_selection(lifetime);
        self.link
            .show_selection(super::selection_update(lifetime, 1));
    }

    fn due(&self, n: u32, took: Duration) {
        let mut s = self.fake.state();
        s.due.push_back((n, took));
        // A filled entry of the result kind changes its report.
        s.result_changes += 1;
    }

    /// `n` more frames drawn: the render thread's passes have seen what
    /// the test set before.
    fn passes(&self, n: usize) {
        let f = self.fake.state().frames.len();
        self.until_frames(|frames| frames.len() > f + n);
    }

    fn ran(&self) -> Vec<Ran> {
        self.fake.state().ran.clone()
    }

    fn shown(&self) -> u64 {
        self.fake.state().shown
    }

    #[track_caller]
    fn until_ran(&self, n: usize) -> Vec<Ran> {
        assert!(
            wait_until(|| self.fake.state().ran.len() >= n),
            "never ran {n} operations: {:?}",
            self.ran()
        );
        self.ran()
    }

    #[track_caller]
    fn until_frames(&self, done: impl Fn(&[(Kind, Instant, bool)]) -> bool) {
        assert!(wait_until(|| done(&self.fake.state().frames)));
    }

    /// One frame through the gate, presented or not, and the next pass
    /// held at its frame wait.
    fn step(&self, presented: bool) {
        self.fake.unpresented.store(!presented, Ordering::SeqCst);
        self.fake.gate.step();
        self.fake.unpresented.store(false, Ordering::SeqCst);
    }

    /// Sets the hook run at the next heavy operation's end.
    fn on_end(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .fake
            .on_end
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// A due entry destroyed at the boundary after the next presented
    /// frame: the interval's allowance spent (2a's stand-in for a skin
    /// creation in the interval).
    fn spend(&self, n: u32) -> Ran {
        let before = self.ran().len();
        self.due(n, Duration::ZERO);
        self.step(true);
        let ran = self.until_ran(before + 1);
        assert_eq!(ran[before].op, Op::Destroy(n));
        ran[before]
    }

    fn bga(&self, generation: u64, msg: BgaMsg) {
        assert!(self.link.bga_publisher().send(generation, msg));
    }

    /// A live BGA array of `generation`, its keyed table published.
    fn live_array(&self, generation: u64) -> BgaKey {
        let (reply, rx) = mpsc::sync_channel(1);
        self.bga(generation, BgaMsg::Begin { pages: 4, reply });
        self.step(true);
        let key = rx.recv_timeout(BOUND).unwrap().unwrap();
        let answer = Answer::new(BOUND);
        self.bga(
            generation,
            BgaMsg::Publish {
                key: Some(key),
                entries: Vec::new(),
                reply: Arc::clone(&answer),
            },
        );
        self.step(true);
        assert!(answer.given().unwrap().unwrap().key() == Some(key));
        key
    }

    /// The table a `Reuse` of `generation` naming `key` answers.
    fn reuse(&self, generation: u64, key: BgaKey) -> Option<Arc<BgaTable>> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.bga(generation, BgaMsg::Reuse { key, reply });
        self.step(false);
        rx.recv_timeout(BOUND).unwrap()
    }

    fn generation(&self, g: u64) {
        self.link.bga_generation().store(g, Ordering::SeqCst);
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.fake.gate.release();
        self.fake.gate.suppress(false);
        self.link.send(RenderMsg::Exit);
    }
}

// ---- M1: the stall and the grant (V17 (m3), (m4)) ----

/// V17 (m3): the gate answering that no frame is due stalls the loop: the
/// next operation runs at the first boundary 16 ms or more after the last
/// one ended, under the stall's clock, and the first presented frame ends
/// the stall; the presenting loop then refills only by a frame shown,
/// never by time.
// Revert: the stall rule removed (`Admit::ByStall` answered `Admit::No`
// in `Run::admit`) -> B never runs: "never ran 2 operations".
#[test]
fn a_stall_runs_one_operation_by_its_clock_and_a_shown_frame_ends_it() {
    let r = Rig::selection();
    let a = r.spend(1);
    r.due(2, Duration::from_millis(2));
    r.fake.gate.suppress(true);
    let ran = r.until_ran(2);
    let b = ran[1];
    assert_eq!(b.op, Op::Destroy(2));
    assert!(b.start >= a.end + PACE, "B within 16 ms of A's end");
    assert_eq!(b.shown, a.shown, "B ran with no frame shown");
    assert_eq!(r.link.heavy_record()[1].regime, Regime::Stalled);
    // Held: the first frame shown ends the stall and refills.
    r.fake.gate.block();
    r.due(3, Duration::ZERO);
    r.due(4, Duration::ZERO);
    r.step(true);
    let c = r.until_ran(3)[2];
    assert_eq!(c.op, Op::Destroy(3));
    assert!(c.shown > b.shown);
    assert_eq!(r.link.heavy_record()[2].regime, Regime::Presenting);
    // Frames not shown over more than the clock's pace: no refill by time.
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(6));
        r.step(false);
    }
    assert_eq!(r.ran().len(), 3, "an operation refilled by time");
    r.step(true);
    let d = r.until_ran(4)[3];
    assert_eq!(d.op, Op::Destroy(4));
    assert!(d.shown > c.shown);
}

/// The stall of (m3) after its operation B, with C due: the hook at B's
/// end blocks the gate, so the render thread parks at that pass's frame
/// wait; 20 ms later.
fn parked_after_b(r: &Rig) -> Ran {
    let _a = r.spend(1);
    r.due(2, Duration::ZERO);
    r.due(3, Duration::ZERO);
    let f = Arc::clone(&r.fake);
    r.on_end(move || f.gate.set_blocked());
    r.fake.gate.suppress(true);
    let b = r.until_ran(2)[1];
    assert_eq!(b.op, Op::Destroy(2));
    r.fake.gate.until("parked after B", |s| s.held);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(r.ran().len(), 2);
    b
}

/// V17 (m4) (i): a recovered grant. The parked wait answers that no frame
/// is due and leaves one permit ready; the next stalled boundary, 16 ms
/// or more after B, finds it by the non-blocking grant check: that frame
/// draws and ends shown before C starts, and C runs at the next boundary.
// Revert: no grant check at a stalled boundary (`Admit::ByStall if
// self.grant_found() => return` dropped from `Run::heavy_op`) -> C runs
// at that boundary before the recovered frame: "C before the recovered
// frame".
#[test]
fn a_recovered_grant_draws_its_frame_before_the_stall_s_operation() {
    let r = Rig::selection();
    let b = parked_after_b(&r);
    r.fake.gate.recover();
    let c = r.until_ran(3)[2];
    assert_eq!(c.op, Op::Destroy(3));
    assert!(c.shown > b.shown, "C before the recovered frame");
    assert_eq!(r.fake.gate.lock().taken_ready, 1, "the grant check took it");
    let shown = r
        .fake
        .state()
        .frames
        .iter()
        .filter(|(_, t, p)| *p && *t > b.end && *t < c.start)
        .count();
    assert_eq!(shown, 1, "one frame between B and C");
}

/// V17 (m4) (ii): a grant taken by a wait whose pass drew nothing. While
/// the render thread is parked, the selection is left and a page queued,
/// then one frame let through: the grant is kept as the mode turns Idle,
/// the next turn takes the page, and its boundary, 16 ms or more after B
/// ended, runs nothing: the page's first frame draws with the kept grant
/// and ends shown before C starts.
// Revert: the stall not ended where the pacing's wait answers a grant
// (`self.stalled = false` after `self.granted = true` dropped) -> the
// page's boundary is still stalled, finds no grant ready and runs C
// before the page's first frame.
#[test]
fn a_grant_kept_through_idle_ends_the_stall_before_the_operation() {
    let r = Rig::selection();
    let b = parked_after_b(&r);
    r.link.leave_selection();
    r.link.send(RenderMsg::Menu(Box::default()));
    let f = Arc::clone(&r.fake);
    r.fake.gate.step_into(move || f.state().ran.len() >= 3);
    let c = r.ran()[2];
    assert_eq!(c.op, Op::Destroy(3));
    let frames = r.fake.state().frames.clone();
    let page = frames
        .iter()
        .find(|(k, t, _)| *k == Kind::Page && *t > b.end)
        .copied()
        .expect("the page's frame");
    assert!(
        page.2 && page.1 < c.start,
        "C before the page's first frame"
    );
    assert_eq!(r.fake.gate.lock().taken_ready, 0);
}

/// V17 (m4) (iii): frames paced by the clock (the gate not pacing, as
/// offscreen), every one ending `Retry`: the stalled loop still runs an
/// operation at the first boundary 16 ms or more after the last ended,
/// though the grant check answers at once there.
// Revert: the grant check made where no grant paces the frames (`!s.paced()
// ||` dropped from `Run::grant_found`) -> the stall ends at every boundary:
// "never ran 3 operations".
#[test]
fn a_clock_paced_stall_runs_its_operations_by_the_clock() {
    let r = Rig::new();
    r.fake.unpresented.store(true, Ordering::SeqCst);
    r.link.send(RenderMsg::Menu(Box::default()));
    r.until_frames(|f| f.len() > 2);
    let shown = r.shown();
    for n in 1..=3 {
        r.due(n, Duration::ZERO);
    }
    let ran = r.until_ran(3);
    for w in ran.windows(2) {
        assert!(w[1].start >= w[0].end + PACE);
    }
    let rec = r.link.heavy_record();
    assert!(
        rec[1..].iter().all(|h| h.regime == Regime::Stalled),
        "{rec:?}"
    );
    assert_eq!(r.shown(), shown, "a frame shown");
}

// ---- M3: the BGA operation held (V17 (e2), (e3)) ----

/// V17 (e2): with a live BGA array and the interval's allowance spent (a
/// due destruction standing for the skin creation), an absent
/// publication sent in that interval is held: the array's drop at the
/// first boundary after the next frame shown, recorded as a BGA drop,
/// and its answer, the absent table, after the drop.
// Revert: the publication run in its handler (the `Publish { key: None }
// if in_use` arm dropped from `Run::bga`) -> "answered in the spent
// interval". Revert: answered at receipt (the held arm answering at once
// as the handler took it) -> "answered in the spent interval".
#[test]
fn an_absent_publication_drops_the_array_in_use_after_the_next_frame() {
    let r = Rig::page();
    r.generation(5);
    let _key = r.live_array(5);
    let x = r.spend(1);
    let answer = Answer::new(BOUND);
    r.bga(
        5,
        BgaMsg::Publish {
            key: None,
            entries: Vec::new(),
            reply: Arc::clone(&answer),
        },
    );
    r.step(false);
    assert!(answer.given().is_none(), "answered in the spent interval");
    assert!(
        r.fake.state().bga_live.is_some(),
        "dropped in the spent interval"
    );
    r.step(true);
    let ran = r.until_ran(3);
    let drop = ran[2];
    assert_eq!(drop.op, Op::BgaDrop);
    assert!(drop.shown > x.shown);
    assert_eq!(r.link.heavy_record().last().unwrap().kind, HeavyKind::Bga);
    let table = answer.given().expect("answered after the drop").unwrap();
    assert_eq!(table.key(), None);
}

/// V17 (e2), the stand-in: an absent publication with no array in use is
/// answered in the turn that takes it, the allowance spent, and runs no
/// heavy operation.
// Revert: every absent publication held (`if in_use` dropped from the
// held arm of `Run::bga`) -> the publication is put in hand and answered
// only at the pass's boundary.
#[test]
fn an_absent_publication_with_no_array_is_answered_in_its_turn() {
    let r = Rig::page();
    r.generation(5);
    let _x = r.spend(1);
    let held = r.link.bga_ops_held();
    let answer = Answer::new(BOUND);
    r.bga(
        5,
        BgaMsg::Publish {
            key: None,
            entries: Vec::new(),
            reply: Arc::clone(&answer),
        },
    );
    r.step(false);
    assert!(answer.given().is_some(), "not answered");
    assert_eq!(
        r.link.bga_ops_held(),
        held,
        "put in hand, not answered in its turn"
    );
    assert_eq!(r.ran().len(), 1, "a heavy operation ran");
}

/// Which BGA operation (e3) takes in hand.
#[derive(Clone, Copy)]
enum Held {
    Begin,
    Absent,
    Drop,
}

/// V17 (e3): a live array, the allowance spent; a current operation taken
/// in hand; the generation advanced with no message; a `Reuse` of the new
/// generation naming the live array answered with its table; a due
/// destruction (standing for a skin creation falling due). At the first
/// boundary after the next frame shown, no BGA operation, the begin's
/// reply or the publication's answer `Stale`, a second `Reuse` still
/// answered with the table, and the destruction run at that boundary.
fn stale_in_hand(held: Held) {
    let r = Rig::page();
    r.generation(5);
    let key = r.live_array(5);
    let x = r.spend(1);
    let (reply, begun) = mpsc::sync_channel(1);
    let answer = Answer::new(BOUND);
    match held {
        Held::Begin => r.bga(5, BgaMsg::Begin { pages: 4, reply }),
        Held::Absent => r.bga(
            5,
            BgaMsg::Publish {
                key: None,
                entries: Vec::new(),
                reply: Arc::clone(&answer),
            },
        ),
        Held::Drop => r.bga(5, BgaMsg::Drop),
    }
    r.step(false);
    r.generation(6);
    // The `Reuse` is taken in the pass after the next frame shown, whose
    // boundary is the first to find the operation in hand stale.
    let (reused, table) = mpsc::sync_channel(1);
    r.bga(6, BgaMsg::Reuse { key, reply: reused });
    r.due(2, Duration::ZERO);
    r.step(true);
    assert!(
        table.recv_timeout(BOUND).unwrap().is_some(),
        "the live array's table"
    );
    let ran = r.until_ran(3);
    assert_eq!(ran[2].op, Op::Destroy(2), "{ran:?}");
    assert!(ran[2].shown > x.shown);
    assert!(
        r.link
            .heavy_record()
            .iter()
            .filter(|h| h.start > x.end)
            .all(|h| h.kind == HeavyKind::Destruction)
    );
    match held {
        Held::Begin => assert_eq!(
            begun.recv_timeout(BOUND).unwrap(),
            Err(render::BgaFault::Stale)
        ),
        Held::Absent => assert_eq!(answer.given().unwrap().err(), Some(render::BgaFault::Stale)),
        Held::Drop => {}
    }
    assert!(r.reuse(6, key).is_some(), "the live array dropped");
}

// Revert: the generation checked only at receipt (`current` taken as true
// in `Run::bga_op_boundary`) -> the stale Begin runs at that boundary:
// "Destroy(2)" expected, a BGA begin found. Revert: a stale operation
// counted as the interval's (the allowance spent where `bga_op_boundary`
// answers it stale) -> the destruction waits: "never ran 3 operations".
#[test]
fn a_stale_begin_in_hand_runs_nothing_and_spends_nothing() {
    stale_in_hand(Held::Begin);
}

// Revert: the generation checked only at receipt, as the begin's -> the
// stale publication runs at that boundary: "Destroy(2)" expected, a BGA
// drop found.
#[test]
fn a_stale_absent_publication_in_hand_runs_nothing() {
    stale_in_hand(Held::Absent);
}

// Revert: as the begin's; the stale drop drops the live array.
#[test]
fn a_stale_drop_in_hand_runs_nothing() {
    stale_in_hand(Held::Drop);
}

/// V17 (e3), the fourth run: a current `Drop` in hand, then an absent
/// publication of the previous generation: answered `Stale` before the
/// next boundary, and the `Drop` run at the first boundary after the next
/// frame shown.
// Revert: a message put in hand before its generation is checked (`in_use`
// read without `current` in `Run::bga`) -> the stale publication evicts the
// `Drop`, which never runs: "never ran 3 operations".
#[test]
fn a_stale_publication_leaves_the_drop_in_hand() {
    let r = Rig::page();
    r.generation(5);
    let _key = r.live_array(5);
    let x = r.spend(1);
    r.bga(5, BgaMsg::Drop);
    r.step(false);
    let answer = Answer::new(BOUND);
    r.bga(
        4,
        BgaMsg::Publish {
            key: None,
            entries: Vec::new(),
            reply: Arc::clone(&answer),
        },
    );
    r.step(false);
    assert_eq!(answer.given().unwrap().err(), Some(render::BgaFault::Stale));
    assert!(r.fake.state().bga_live.is_some());
    r.step(true);
    let ran = r.until_ran(3);
    assert_eq!(ran[2].op, Op::BgaDrop);
    assert!(ran[2].shown > x.shown);
}

// ---- V17's other cases ----

/// V17 (d): a burst of messages in one frame interval (BGA begins, pages,
/// a fence-like `Menu`), with two entries due: one heavy operation among
/// them until the next frame shown.
// Revert: the allowance reset at each loop turn (`self.allowance = None` at
// the loop's top) -> a second operation in the interval: "two heavy
// operations in one interval".
#[test]
fn a_burst_of_messages_runs_one_heavy_operation() {
    let r = Rig::page();
    r.generation(5);
    let x = r.spend(1);
    let (reply, _begun) = mpsc::sync_channel(1);
    r.bga(5, BgaMsg::Begin { pages: 4, reply });
    r.due(2, Duration::ZERO);
    r.due(3, Duration::ZERO);
    for _ in 0..4 {
        r.link.send(RenderMsg::Menu(Box::default()));
    }
    for _ in 0..6 {
        r.step(false);
    }
    assert_eq!(r.ran().len(), 1, "two heavy operations in one interval");
    r.step(true);
    let ran = r.until_ran(2);
    assert_eq!(ran[1].op, Op::BgaBegin, "the BGA begin first");
    assert!(ran[1].shown > x.shown);
    for _ in 0..3 {
        r.step(false);
    }
    assert_eq!(r.ran().len(), 2);
}

/// V17 (h): in `Mode::Idle` with two entries due, the second destroyed at
/// least 16 ms after the first ended, the loop awake with no message; with
/// nothing due and nothing to look at, it blocks in `recv`.
// Revert: the allowance reset at each loop turn -> "the second within
// 16 ms". Revert: an Idle wait bounded with nothing due (`idle_wait`
// answering `Some(CANCEL_POLL)` with no work) -> "Idle woke with nothing
// due".
#[test]
fn idle_destroys_one_entry_per_clock_pace_and_then_blocks() {
    let r = Rig::new();
    r.select(1);
    r.until_frames(|f| f.iter().any(|(k, _, _)| *k == Kind::Selection));
    r.link.leave_selection();
    // Idle: no frame drawn any more.
    let drained = || r.link.shared.takeover_acks.drained.load(Ordering::SeqCst);
    assert!(wait_until(|| {
        let n = r.fake.state().frames.len();
        std::thread::sleep(Duration::from_millis(40));
        r.fake.state().frames.len() == n
    }));
    r.due(1, Duration::from_millis(1));
    r.due(2, Duration::from_millis(1));
    // A message wakes the blocked Idle loop once.
    r.link.send(RenderMsg::Turntable {
        number: 0,
        pause: false,
    });
    let ran = r.until_ran(2);
    assert!(
        ran[1].start >= ran[0].end + PACE,
        "the second within 16 ms: {ran:?} {:?}",
        r.link.heavy_record()
    );
    let rec = r.link.heavy_record();
    assert!(rec.iter().all(|h| h.regime == Regime::Clock), "{rec:?}");
    std::thread::sleep(Duration::from_millis(30));
    let before = drained();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(drained(), before, "Idle woke with nothing due");
}

/// V17 (j): minimized, frames end invisible and each refills: one entry
/// destroyed per frame; a frame that ends `Retry` refills nothing however
/// long the next shown frame takes.
// Revert: a `Retry` frame refilling (the refill reading `drawn.is_ok()`
// for `outcome.counts()`) -> "refilled by a Retry frame".
#[test]
fn invisible_frames_refill_and_retry_frames_do_not() {
    let r = Rig::page();
    let x = r.spend(1);
    r.due(2, Duration::ZERO);
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(10));
        r.step(false);
    }
    assert_eq!(r.ran().len(), 1, "refilled by a Retry frame");
    r.step(true);
    let y = r.until_ran(2)[1];
    assert!(y.shown > x.shown);
    // Minimized: paced by the clock, every frame invisible.
    r.fake.minimized.store(true, Ordering::SeqCst);
    r.fake.unpresented.store(true, Ordering::SeqCst);
    r.fake.gate.release();
    for n in 3..=5 {
        r.due(n, Duration::ZERO);
    }
    let _ = r.until_ran(5);
    let rec = r.link.heavy_record();
    let tail = &rec[rec.len() - 3..];
    for w in tail.windows(2) {
        assert!(w[1].refills > w[0].refills, "{rec:?}");
    }
}

/// V17 (j) on the launch screen: minimized, its frames end invisible and
/// each refills, while the window stays swap-paced (so a frame refilling
/// nothing would not stall either): one entry destroyed per frame.
// Revert: the launch screen's frame reduced to presented or not
// (`timing_of(shown_or_retry(outcome == FrameOutcome::Shown))` in
// `Mode::Splash`'s arm) -> "never ran 3 operations".
#[test]
fn a_minimized_launch_screen_s_invisible_frames_refill() {
    let r = Rig::launching();
    r.until_frames(|f| f.iter().any(|(k, _, _)| *k == Kind::Splash));
    r.fake.minimized.store(true, Ordering::SeqCst);
    // Every pass from here reads the window minimized and never waits at
    // the gate, which now paces: the screen counts as swap-paced. Its
    // frames go unpresented only then, so none stalled the loop before.
    r.passes(2);
    r.fake.gate.set_blocked();
    r.passes(1);
    r.fake.unpresented.store(true, Ordering::SeqCst);
    r.passes(1);
    for n in 1..=3 {
        r.due(n, Duration::ZERO);
    }
    let _ = r.until_ran(3);
    let rec = r.link.heavy_record();
    let tail = &rec[rec.len() - 3..];
    for w in tail.windows(2) {
        assert!(w[1].refills > w[0].refills, "{rec:?}");
    }
    assert!(
        r.fake
            .state()
            .frames
            .iter()
            .all(|(k, _, _)| *k == Kind::Splash),
        "left the launch screen"
    );
}

/// V17 (k): several completed entries: one per interval, first in, first
/// out (2a makes no creation at a boundary for them to wait behind).
// Revert: two destructions in one interval (`destroy_due(true)` called
// twice in `Run::heavy_op`) -> "two in one interval".
#[test]
fn completed_entries_go_one_per_interval_oldest_first() {
    let r = Rig::page();
    for n in 1..=4 {
        r.due(n, Duration::ZERO);
    }
    let mut ran = Vec::new();
    for _ in 0..8 {
        r.step(true);
        ran = r.ran();
        if ran.len() == 4 {
            break;
        }
    }
    let ops: Vec<Op> = ran.iter().map(|r| r.op).collect();
    assert_eq!(ops, [1, 2, 3, 4].map(Op::Destroy), "oldest first");
    for w in ran.windows(2) {
        assert!(w[1].shown > w[0].shown, "two in one interval");
    }
}

/// V17 (n): changes of regime. An operation at a presenting boundary, then
/// the mode turned Idle (the selection left, as `PrepareAttempt` turns it
/// Idle): the next one at the first Idle boundary 16 ms or more after the
/// first ended; an operation in Idle, then a page whose first grant comes
/// 30 ms later: nothing before that page's first frame ends shown.
// Revert: the allowance freed on entering a presenting mode (`allowance =
// None` in the `Menu` arm) -> "ran before the page's first frame".
#[test]
fn the_allowance_keeps_its_state_across_a_change_of_regime() {
    let r = Rig::selection();
    let a = r.spend(1);
    r.due(2, Duration::ZERO);
    r.link.leave_selection();
    // The wait answered, the selection found left: Idle, no frame drawn.
    let f = Arc::clone(&r.fake);
    r.fake.gate.step_into(move || f.state().ran.len() >= 2);
    let b = r.ran()[1];
    assert!(b.start >= a.end + PACE);
    assert_eq!(r.link.heavy_record()[1].regime, Regime::Clock);
    // Idle: a page drawn, its first grant 30 ms later.
    r.due(3, Duration::ZERO);
    r.link.send(RenderMsg::Menu(Box::default()));
    r.fake.gate.until("parked at the page's wait", |s| s.held);
    std::thread::sleep(Duration::from_millis(30));
    if r.ran().len() < 3 {
        r.step(true);
    }
    let c = r.until_ran(3)[2];
    let rec = r.link.heavy_record()[2];
    if rec.regime == Regime::Clock {
        // The Idle loop ran it by its clock before the page was taken.
        assert!(c.start >= b.end + PACE);
    } else {
        // The page's first frame draws with the grant the Idle pass
        // kept; the operation only after it.
        assert!(c.shown > b.shown, "ran before the page's first frame");
        assert!(
            r.fake
                .state()
                .frames
                .iter()
                .any(|(k, t, p)| *k == Kind::Page && *p && *t > b.end && *t < c.start),
            "ran before the page's first frame"
        );
    }
}

/// V17 (o): a selection entry's regrowth with a BGA `Drop` in hand (sent as
/// the chart is left, before the entry): the regrowth at that pass's
/// boundary before the first selection frame, the `Drop` after the next
/// frame shown.
// Revert: the regrowth run in the `SelectionChanged` turn (`take_selection`
// calling `regrow_pool` at once) -> the `Drop` in the regrowth's interval:
// "the Drop in the regrowth's interval".
#[test]
fn a_selection_entry_regrows_first_and_the_drop_waits() {
    let r = Rig::page();
    r.generation(5);
    let _key = r.live_array(5);
    let _x = r.spend(1);
    r.bga(5, BgaMsg::Drop);
    r.step(false);
    r.fake.regrows.store(true, Ordering::SeqCst);
    r.select(1);
    r.step(true);
    let ran = r.until_ran(3);
    let regrow = ran[2];
    assert_eq!(regrow.op, Op::Regrowth, "{ran:?}");
    let frames = r.fake.state().frames.clone();
    assert!(
        frames
            .iter()
            .all(|(k, t, _)| *k != Kind::Selection || *t > regrow.end),
        "a selection frame before the regrowth"
    );
    r.step(true);
    let drop = r.until_ran(4)[3];
    assert_eq!(drop.op, Op::BgaDrop);
    assert!(
        drop.shown > regrow.shown,
        "the Drop in the regrowth's interval"
    );
}

/// V17 (o): the regrowth of an entry whose boundary finds the allowance
/// spent (by the `Drop` run at an earlier boundary of that interval, and,
/// in the second run, by a destruction) is not made until the next entry.
// Revert: the regrowth tried again at a later selection boundary (the
// refused `regrow` kept in `regrow_due`) -> "a regrowth before the next
// entry".
fn no_regrowth_after_a_spent_entry(by_drop: bool) {
    let r = Rig::page();
    r.generation(5);
    let _key = r.live_array(5);
    if by_drop {
        r.bga(5, BgaMsg::Drop);
        r.step(true);
        assert_eq!(r.until_ran(2)[1].op, Op::BgaDrop);
    } else {
        let _ = r.spend(1);
    }
    let spent = r.ran().len();
    r.fake.regrows.store(true, Ordering::SeqCst);
    r.select(1);
    r.step(false);
    for _ in 0..4 {
        r.step(true);
    }
    assert_eq!(r.ran().len(), spent, "a regrowth before the next entry");
    assert_eq!(r.fake.state().pool_generation, 0);
    r.select(2);
    r.step(true);
    let ran = r.until_ran(spent + 1);
    assert_eq!(ran[spent].op, Op::Regrowth);
}

#[test]
fn a_spent_entry_boundary_makes_no_regrowth_until_the_next_entry() {
    no_regrowth_after_a_spent_entry(true);
}

#[test]
fn an_entry_spent_by_a_destruction_makes_no_regrowth() {
    no_regrowth_after_a_spent_entry(false);
}

// ---- M4 / V18: READY's cutoff on the fake ----

fn woken(link: &super::super::RenderLink) -> Arc<AtomicU64> {
    let woken = Arc::new(AtomicU64::new(0));
    let w = Arc::clone(&woken);
    link.set_game_wake(Arc::new(move || {
        w.fetch_add(1, Ordering::AcqRel);
    }));
    woken
}

/// V18: with no filled entry, asked while the gate holds the render thread
/// at the frame's pacing under the launch screen: cut off before the gate
/// opens, no frame presented between.
// Revert: the swap at the pacing left out (`ask_cutoff` only marking and
// sending) -> "not cut off while the frame wait held".
#[test]
fn a_cutoff_asked_at_the_launch_screen_s_pacing_is_given_at_once() {
    let r = Rig::launching();
    r.fake.gate.hold();
    let frames = r.fake.state().frames.len();
    r.link.ask_cutoff();
    let start = Instant::now();
    while r.link.cutoff_phase() != CutoffPhase::CutOff && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        r.link.cutoff_phase(),
        CutoffPhase::CutOff,
        "not cut off while the frame wait held"
    );
    assert_eq!(r.fake.state().frames.len(), frames, "a frame between");
}

/// V18: asked while a test gate holds the render thread in a pass before
/// its pacing (`frame_gates[1]`): cut off at that pass's mark, before its
/// pacing, no frame presented between.
// Revert: the asked bit left out (`mark_waiting` not cutting off when
// asked) -> "not cut off before the pass's frame".
#[test]
fn a_cutoff_asked_before_a_pass_s_pacing_is_given_at_its_mark() {
    let r = Rig::page();
    let gate = &r.link.shared.frame_gates[1];
    gate.hold(true);
    r.fake.gate.step_into(|| gate.waiting());
    let frames = r.fake.state().frames.len();
    r.link.ask_cutoff();
    gate.hold(false);
    let start = Instant::now();
    while r.link.cutoff_phase() != CutoffPhase::CutOff && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        r.link.cutoff_phase(),
        CutoffPhase::CutOff,
        "not cut off before the pass's frame"
    );
    assert_eq!(r.fake.state().frames.len(), frames, "a frame between");
}

/// V18: asked in Idle after the end of loading: cut off in the `Cutoff`
/// message's turn, the game woken.
#[test]
fn a_cutoff_asked_in_idle_is_given_in_its_message_s_turn() {
    let r = Rig::new();
    let woken = woken(&r.link);
    r.link.loading().show(one_line("L"));
    r.until_frames(|f| !f.is_empty());
    r.link.end_loading().unwrap();
    r.link.ask_cutoff();
    assert!(wait_until(|| r.link.cutoff_phase() == CutoffPhase::CutOff));
    assert!(wait_until(|| woken.load(Ordering::Acquire) >= 1));
}

/// V18: with a filled entry, READY waits; cut off in the pass that
/// destroys it, after that pass's publication.
// Revert: the cutoff checked before the boundary's publication (the
// `check_cutoff` after the destruction moved before the publication) ->
// "not published before the cutoff".
#[test]
fn a_cutoff_waits_for_the_filled_entry_and_follows_its_publication() {
    let r = Rig::new();
    let id = super::bundle().atlas_pages().id();
    *r.fake.result_id.lock().unwrap() = Some(id);
    r.fake.held.store(true, Ordering::SeqCst);
    r.due(1, Duration::ZERO);
    r.link.send(RenderMsg::Menu(Box::default()));
    r.passes(2);
    assert!(wait_until(|| r.link.result_status().retained.contains(&id)));
    r.link.ask_cutoff();
    r.passes(5);
    assert_ne!(
        r.link.cutoff_phase(),
        CutoffPhase::CutOff,
        "READY with a filled entry"
    );
    r.fake.held.store(false, Ordering::SeqCst);
    assert!(wait_until(|| r.link.cutoff_phase() == CutoffPhase::CutOff));
    assert_eq!(r.ran().len(), 1, "{:?}", r.ran());
    assert!(
        !r.link.result_status().retained.contains(&id),
        "not published before the cutoff"
    );
}

/// V18 (a): a filled entry's completion held while the game waits, the
/// device lost under the launch screen: the phase lost in the drop's call,
/// never cut off, the game woken.
// Revert: a renderer dropped under an asked cutoff without storing lost
// (the store left out of `Run::screen_dropped`) -> "never lost".
#[test]
fn a_device_lost_under_the_launch_screen_loses_the_cutoff() {
    let r = Rig::launching();
    let woken = woken(&r.link);
    r.fake.held.store(true, Ordering::SeqCst);
    r.due(1, Duration::ZERO);
    r.passes(2);
    r.link.ask_cutoff();
    r.passes(2);
    assert_ne!(r.link.cutoff_phase(), CutoffPhase::CutOff);
    r.fake.lose.store(true, Ordering::SeqCst);
    assert!(
        wait_until(|| r.link.cutoff_phase() == CutoffPhase::Lost),
        "never lost"
    );
    assert!(woken.load(Ordering::Acquire) >= 1);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(r.link.cutoff_phase(), CutoffPhase::Lost);
}

/// V18 (a), in Idle: a device error injected at the pass's completion
/// poll drops the screen as a step's does: the phase lost, never cut off.
// Revert: as the launch screen's -> "never lost".
#[test]
fn a_poll_error_in_idle_loses_the_cutoff() {
    let r = Rig::new();
    let woken = woken(&r.link);
    r.link.loading().show(one_line("L"));
    r.until_frames(|f| !f.is_empty());
    // Filled while loading frames draw, and seen by two of them: the test's
    // entry appears at any time, where a renderer's never appears while
    // the render thread waits for a frame under a cutoff word counting
    // none, so the ask could otherwise be given at once.
    r.fake.held.store(true, Ordering::SeqCst);
    r.due(1, Duration::ZERO);
    r.passes(2);
    r.link.end_loading().unwrap();
    r.link.ask_cutoff();
    std::thread::sleep(Duration::from_millis(30));
    assert_ne!(r.link.cutoff_phase(), CutoffPhase::CutOff);
    r.fake.poll_error.store(true, Ordering::SeqCst);
    assert!(
        wait_until(|| r.link.cutoff_phase() == CutoffPhase::Lost),
        "never lost"
    );
    assert!(woken.load(Ordering::Acquire) >= 1);
    assert!(r.link.failed());
}

/// V18 (b): the render thread's end while the game waits for a cutoff
/// held by a filled entry: the game woken and the failure latched.
// Revert: `Unwinding` not waking the game -> "the game not woken".
#[test]
fn the_render_thread_s_end_wakes_the_cutoff_s_wait() {
    let r = Rig::new();
    let woken = woken(&r.link);
    r.fake.held.store(true, Ordering::SeqCst);
    r.due(1, Duration::ZERO);
    r.passes(2);
    r.link.ask_cutoff();
    r.passes(2);
    assert_ne!(r.link.cutoff_phase(), CutoffPhase::CutOff);
    let before = woken.load(Ordering::Acquire);
    r.link.send(RenderMsg::Panic);
    assert!(
        wait_until(|| woken.load(Ordering::Acquire) > before),
        "the game not woken"
    );
    assert!(wait_until(|| r.link.failed()));
    assert_ne!(r.link.cutoff_phase(), CutoffPhase::CutOff);
}

/// V18 (c): the game left the opening while its cutoff waited on a filled
/// entry; the selection's entry ends the cutoff, so once the entry goes
/// nothing is cut off, and with the worker's gate opened the handoff
/// place's release is read and published in the following pass.
// Revert: the cutoff ended only by `Finish` (`SelectionChanged` left out
// of `Run::ends_cutoff`) -> cut off once the entry goes, the release never
// read: "the id stays in retained".
#[test]
fn the_selection_s_entry_ends_a_cutoff_left_waiting() {
    let r = Rig::new();
    let gate = r.link.worker_gate();
    gate.hold(true);
    let pages = Arc::clone(super::bundle().atlas_pages());
    let id = pages.id();
    r.link.seed_handoff(ScreenKind::Result, pages);
    r.link.send(RenderMsg::Menu(Box::default()));
    assert!(wait_until(|| r.link.result_status().retained.contains(&id)));
    r.fake.held.store(true, Ordering::SeqCst);
    r.due(1, Duration::ZERO);
    r.passes(2);
    r.link.ask_cutoff();
    r.passes(3);
    assert_ne!(r.link.cutoff_phase(), CutoffPhase::CutOff);
    // Left: the selection's entry comes next.
    r.select(1);
    r.until_frames(|f| f.iter().any(|(k, _, _)| *k == Kind::Selection));
    r.fake.held.store(false, Ordering::SeqCst);
    assert!(wait_until(|| r.ran().len() == 1));
    gate.hold(false);
    assert!(
        wait_until(|| !r.link.result_status().retained.contains(&id)),
        "the id stays in retained"
    );
    assert_ne!(r.link.cutoff_phase(), CutoffPhase::CutOff);
}

/// V18 (play passes) and V8b (no destruction in a play pass): an entry
/// falling due while play draws is not destroyed in any play pass, nor
/// after `Finish` before the result's first picture; the next screen's
/// message lets it go.
// Revert: destruction allowed in play (`!matches!(mode, Mode::Play { .. })`
// dropped from `Run::destroy_ok`) -> "destroyed in a play pass".
#[test]
fn play_passes_destroy_nothing() {
    let chart = Arc::new(
        ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(60_000_000))
            .build()
            .unwrap(),
    );
    let fake = Arc::new(Fake::default());
    let f = Arc::clone(&fake);
    let make: MakeScreen = Box::new(move || Ok(Box::new(Timed(Arc::clone(&f))) as Box<dyn Screen>));
    let clock = QpcClock::start();
    let link = spawn(
        make,
        Some(Arc::clone(&chart)),
        None,
        clock,
        view(),
        measure(clock),
        Box::new(NoFrames),
    )
    .unwrap();
    link.set_launch_elapsed(Some(Duration::from_secs(3_600)));
    let r = Rig { link, fake };
    let ready = r.link.prepare_attempt(attempt(&chart, None), || false);
    assert_eq!(ready.unwrap(), Preparation::Ready);
    r.link.commit_attempt().unwrap();
    let (rule, device) = (rules::tapestry(), DeviceProfile::default());
    let (_logic, side) = Logic::<SourceReader>::new(
        &chart,
        &rule,
        &device,
        &InputStart::default(),
        Vec::new(),
        LogicConfig {
            inputs: 0,
            effects: 16,
        },
        Vec::new(),
    )
    .unwrap();
    r.link.send(RenderMsg::Play {
        state_origin: TimeUs(0),
        side,
        gauge: 0,
        measure: play_measure(TimeUs(clock.now().0 + 20_000)),
    });
    r.passes(3);
    r.due(1, Duration::ZERO);
    r.passes(10);
    assert!(r.ran().is_empty(), "destroyed in a play pass");
    let _ = r.link.finish(clock.now());
    r.passes(3);
    assert!(
        r.ran().is_empty(),
        "destroyed before the result's first picture"
    );
    r.link.send(RenderMsg::Menu(Box::default()));
    assert_eq!(r.until_ran(1)[0].op, Op::Destroy(1));
}

// ---- V18: the handoff table full; V18b: the report's owners ----

/// Fills the twelve handoff places with result pages the worker holds
/// (its gate shut), one seed taken per boundary.
fn fill_places(r: &Rig) -> Vec<Arc<skin_model::PackedPages>> {
    let gate = r.link.worker_gate();
    gate.hold(true);
    (0..crate::retire_worker::PLACES)
        .map(|_| {
            let pages = Arc::clone(super::bundle().atlas_pages());
            r.link.seed_handoff(ScreenKind::Result, Arc::clone(&pages));
            assert!(wait_until(|| r
                .link
                .shared
                .handoff_seed
                .lock()
                .unwrap()
                .is_none()));
            pages
        })
        .collect()
}

/// A filled result entry holding `id`, its completion seen, with its CPU
/// part still in it.
fn completed_entry(r: &Rig, id: AtlasId) {
    *r.fake.result_id.lock().unwrap() = Some(id);
    *r.fake.cpu.lock().unwrap() = Some(render::CpuParts::default());
    r.due(1, Duration::ZERO);
}

/// V18, the handoff table full: a completed entry's reference stays in
/// it, its id still reported, until a place frees (then it is handed);
/// its array is kept meanwhile.
// Revert: handoffs taken with no place free (`self.handoffs.has_free() &&`
// dropped from `Run::boundary`) -> "taken with no place free".
#[test]
fn a_full_handoff_table_leaves_the_reference_in_its_entry_until_a_place_frees() {
    let r = Rig::new();
    r.link.send(RenderMsg::Menu(Box::default()));
    r.passes(2);
    let _held = fill_places(&r);
    let id = super::bundle().atlas_pages().id();
    r.fake.keep.store(true, Ordering::SeqCst);
    completed_entry(&r, id);
    r.passes(6);
    assert_eq!(
        r.fake.taken.load(Ordering::SeqCst),
        0,
        "taken with no place free"
    );
    assert_eq!(
        r.fake.returned.load(Ordering::SeqCst),
        0,
        "taken with no place free"
    );
    assert!(r.link.result_status().retained.contains(&id));
    // A place frees: the reference is handed.
    r.link.worker_gate().hold(false);
    assert!(wait_until(|| r.fake.taken.load(Ordering::SeqCst) == 1));
    assert!(
        r.link.result_status().retained.contains(&id),
        "the entry keeps it"
    );
    r.fake.keep.store(false, Ordering::SeqCst);
    assert!(wait_until(|| !r
        .link
        .result_status()
        .retained
        .contains(&id)));
    assert_eq!(r.fake.dropped_with_array.load(Ordering::SeqCst), 0);
}

/// V18, V18b: the handoff table full at the entry's destruction: the
/// reference is dropped with the array on the render thread, and the id,
/// held by no one else, is gone from the report after that pass.
// Revert: as the last one -> "taken with no place free".
#[test]
fn a_full_handoff_table_leaves_the_reference_to_the_entry_s_destruction() {
    let r = Rig::new();
    r.link.send(RenderMsg::Menu(Box::default()));
    r.passes(2);
    let _held = fill_places(&r);
    let id = super::bundle().atlas_pages().id();
    r.fake.held.store(true, Ordering::SeqCst);
    completed_entry(&r, id);
    assert!(wait_until(|| r.link.result_status().retained.contains(&id)));
    r.fake.held.store(false, Ordering::SeqCst);
    assert!(wait_until(|| r.ran().len() == 1));
    r.passes(2);
    assert_eq!(
        r.fake.taken.load(Ordering::SeqCst),
        0,
        "taken with no place free"
    );
    assert_eq!(r.fake.dropped_with_array.load(Ordering::SeqCst), 1);
    assert!(!r.link.result_status().retained.contains(&id));
    r.link.worker_gate().hold(false);
}

/// V18b: a device loss with pages handed and a filled entry not yet
/// handed: the handed id stays in the report after the drop's pass (the
/// handoff table is the loop's, not the renderer's), the unhanded entry's
/// id is gone after it, before any rebuild; an id held by two owners is
/// named once.
// Revert: the drop publishing only before `self.screen = None` (the
// publication left out of `Run::screen_dropped`) -> "the dropped entry's id
// still reported". Revert: the report's deduplication removed -> "named
// twice".
#[test]
fn a_device_loss_keeps_the_handed_ids_and_drops_the_renderer_s() {
    let r = Rig::new();
    r.link.send(RenderMsg::Menu(Box::default()));
    r.passes(2);
    let gate = r.link.worker_gate();
    gate.hold(true);
    let handed = Arc::clone(super::bundle().atlas_pages());
    let a = handed.id();
    r.link.seed_handoff(ScreenKind::Result, Arc::clone(&handed));
    // The entry holds the handed atlas's array too: one name.
    *r.fake.result_id.lock().unwrap() = Some(a);
    r.fake.held.store(true, Ordering::SeqCst);
    r.due(1, Duration::ZERO);
    assert!(wait_until(|| r.link.result_status().retained.contains(&a)));
    r.passes(2);
    let named = |id: AtlasId| {
        r.link
            .result_status()
            .retained
            .iter()
            .filter(|x| **x == id)
            .count()
    };
    assert_eq!(named(a), 1, "named twice");
    // Another entry, not handed.
    let b = super::bundle().atlas_pages().id();
    *r.fake.result_id.lock().unwrap() = Some(b);
    r.due(2, Duration::ZERO);
    assert!(wait_until(|| named(b) == 1));
    r.fake.lose.store(true, Ordering::SeqCst);
    assert!(wait_until(|| r.link.failed()));
    assert!(
        wait_until(|| named(b) == 0),
        "the dropped entry's id still reported"
    );
    assert_eq!(named(a), 1, "the handed id gone with the renderer");
    drop(handed);
    gate.hold(false);
    assert!(wait_until(|| named(a) == 0));
}

// ---- V18a: a third atlas waits for the worker's release ----

/// The result's report the game would read, with `committed` (the view
/// committed on the render side, which this fake does not draw) added.
fn report(r: &Rig, committed: AtlasId) -> Vec<AtlasId> {
    let mut ids = r.link.result_status().retained;
    if !ids.contains(&committed) {
        ids.push(committed);
    }
    ids
}

/// Parks the game's look until the report no longer names `gone`, woken
/// by the render thread; the time it took.
fn look_until_gone(r: &Rig, gone: AtlasId) -> Duration {
    let from = Instant::now();
    while r.link.result_status().retained.contains(&gone) {
        std::thread::park_timeout(Duration::from_secs(5));
        assert!(from.elapsed() < BOUND, "never left the report");
    }
    from.elapsed()
}

/// V18a: view 1's atlas retired and handed to the worker (seeded, its
/// gate shut), view 2's committed: view 3's build waits, view 1's id in
/// the report, at most two atlases alive; once the worker drops the pages
/// the pass that reads the release publishes without the id and wakes the
/// game thread, and the build starts at the look it wakes, well before
/// the 5 s park would have ended it. With `entry`, an entry also keeps
/// view 1's array past the worker's release: the build waits for the
/// entry's destruction instead.
fn third_atlas(entry: bool) {
    use crate::result_skin::tests as rt;
    use crate::result_skin::{ResultSkin, ResultStart};
    let _s = crate::imports::tests::serial();
    let r = Rig::new();
    r.link.send(RenderMsg::Menu(Box::default()));
    r.passes(2);
    let game = std::thread::current();
    r.link.set_game_wake(Arc::new(move || game.unpark()));
    let d = rt::skin(
        "",
        "{ { id = \"dot\", src = \"s\", x = 8 * nr, y = 0, w = 8, h = 8 } }",
    );
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(rt::pending(
        d.path(),
        rt::LIMITS,
    ))));
    let mut seen = rt::Seen::default();
    rt::staged(&mut rs);
    rs.request(rt::view(1));
    rt::ran(&mut rs);
    let one = rt::shown(&rs);
    let a1 = one.atlas_pages().id();
    seen.note(&rs);
    rs.request(rt::view(2));
    rt::ran(&mut rs);
    let two = rt::shown(&rs);
    let a2 = two.atlas_pages().id();
    seen.note(&rs);
    assert_ne!(a1, a2);
    // View 1's array retired, completed, its pages handed to the worker.
    let gate = r.link.worker_gate();
    gate.hold(true);
    if entry {
        *r.fake.result_id.lock().unwrap() = Some(a1);
        r.fake.held.store(true, Ordering::SeqCst);
        r.due(1, Duration::ZERO);
    }
    r.link
        .seed_handoff(ScreenKind::Result, Arc::clone(one.atlas_pages()));
    drop(one);
    assert!(wait_until(|| r.link.result_status().retained.contains(&a1)));
    rs.set_retained(&report(&r, a2));
    let g = rs.published().1;
    rs.request(rt::view(3));
    rt::ran(&mut rs);
    assert!(rs.deferred(), "the third build waits");
    assert_eq!(rs.published().1, g);
    seen.note(&rs);
    assert!(seen.alive() <= 2, "{}", seen.alive());
    // The worker drops the pages.
    gate.hold(false);
    if entry {
        let reads = r.link.retire_counts()[3];
        assert!(wait_until(|| r.link.retire_counts()[3] > reads + 2));
        assert!(
            r.link.result_status().retained.contains(&a1),
            "the entry keeps it"
        );
        rs.set_retained(&report(&r, a2));
        assert!(rs.deferred(), "built before the entry's destruction");
        r.fake.held.store(false, Ordering::SeqCst);
    }
    let took = look_until_gone(&r, a1);
    assert!(took < Duration::from_secs(2), "not woken: {took:?}");
    rs.set_retained(&report(&r, a2));
    assert!(!rs.deferred() && rs.has_job(), "the build did not start");
    rt::ran(&mut rs);
    assert_eq!(rs.published().1, g + 1);
    seen.note(&rs);
    assert!(seen.alive() <= 2, "{}", seen.alive());
    drop(two);
}

// Revert: the id dropped at the handoff instead of the release read (the
// handoff table's ids left out of `Run::publish_result_status`) -> the
// report never names view 1's handed atlas (the wait for it fails). Revert: no wake when an id leaves (the
// `game_wake.wake()` after a removal left out) -> "not woken".
#[test]
fn a_third_result_atlas_waits_for_the_worker_s_release() {
    third_atlas(false);
}

// Revert: the screen's entries left out of the report (only the handoff
// table and the record read) -> "the entry keeps it".
#[test]
fn a_third_result_atlas_waits_for_the_entry_s_destruction_past_the_release() {
    third_atlas(true);
}

// ---- V17 (f3) on the fake ----

/// V17 (f3), as far as 2a's boundary goes: in a loading lifetime whose
/// first loading frame is not presented yet, a staging may destroy filled
/// entries in its own call (the permission is set), but a boundary
/// destroys none; the first presented loading frame lets it go.
// Revert: the boundary's destruction not waiting for the lifetime's first
// presented frame (its clause dropped from `Run::destroy_ok`) -> "destroyed
// before the first presented loading frame".
#[test]
fn a_loading_lifetime_destroys_nothing_at_a_boundary_before_its_first_frame() {
    let r = Rig::new();
    r.fake.unpresented.store(true, Ordering::SeqCst);
    r.passes(2);
    let shown = r.shown();
    r.due(1, Duration::ZERO);
    r.link.loading().show(one_line("L"));
    r.passes(6);
    assert!(
        r.ran().is_empty(),
        "destroyed before the first presented loading frame"
    );
    assert!(
        r.fake.destroy_allowed.load(Ordering::SeqCst),
        "a staging may not destroy in the hold"
    );
    r.fake.unpresented.store(false, Ordering::SeqCst);
    let first = r.until_ran(1)[0];
    assert_eq!(first.op, Op::Destroy(1));
    assert!(
        first.shown > shown,
        "destroyed before the first presented loading frame"
    );
}
