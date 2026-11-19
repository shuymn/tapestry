//! The transition probe (#611): marks of the render loop's steps, the
//! game's transitions and the renderer's uploads, drops and first uses
//! (`render::probe`), on the game's clock (the `QpcClock` the process
//! started, the clock of the frames' presents and the scenario's
//! transition record), buffered and printed when the command ends as
//! `dev: probe <µs> <tag> <value>` lines ([`dump`]). Off unless
//! `TAPESTRY_LOOP_PROBE` is set when the process starts ([`init`]).
//!
//! A mark allocates nothing and waits for nothing: its slot is reserved at
//! [`init`], before any play, and taken by one atomic step, so a thread
//! descheduled in a mark holds up no other (a test's held clock is read
//! under its lock; the process's clock is never held). A mark past the
//! room, or one whose slot was taken but not filled when the marks are
//! read, is counted, not kept (the count is printed with the marks). Off,
//! a mark is one atomic load.
//!
//! Two classes of mark. The deciding marks are exact at their events, on
//! the game's clock: `g:choice` at the game's own time of the accepted
//! choice (0) or a practice's Start (1), `prep:begin` and `g:open` read
//! as the preparation and the opening begin, before anything else they
//! do, and `g:ready` at the game's own READY (`ready_state_at`: 0 a play's,
//! 1 a round's), so every end-to-end span a transition is judged by is a
//! subtraction of two of them. Every other mark is diagnostic: it names its
//! event truly (the array, view or upload it is of), and its interval is
//! what is measured, with what it cannot leave out:
//!
//! - `fu:*`, an array's first use: the first frame that encoded a sprite
//!   draw sampling the array uploaded last. `fu:submit_us` is the frame's
//!   submit: `encoder.finish()`, the submit call and the reading of the
//!   submit's return time; `fu:cpu_us` the frame's CPU time as its timing
//!   counts it, the image's acquisition or admission left out;
//!   `fu:gpu_done_us` runs from the submit's return to wgpu's completion
//!   callback, which wgpu runs at the device's next submit or poll, so it
//!   is the latency of that callback, with no bound on the wait for that
//!   call. The callback can run inside another submit or a timed poll, an
//!   upload's included, so its mark can fall among that upload's marks;
//!   marks are the probe's own work, left out of the upload's figures,
//!   though they overlap the GPU's work its wait times.
//! - `tx:*`, a text preparation: each step as timed; `tx:gpu_wait_us`
//!   from just before the wait to its return, zero when every line was
//!   cached and nothing was submitted, and the time to the failure when a
//!   test or a development fault makes the wait fail in its place. Each
//!   includes the thread's descheduling inside it.
//! - `att:*`, an attempt's staging: `att:stage_us` is the whole stage or
//!   preparation call, any upload and wait inside it included.
//! - `rf:*`, the result's first frames: the three frames after its first
//!   view committed, a skin's staging or the built-in view, or, for an
//!   entry with no result lifetime, its built-in page's first three, once
//!   per entry; counted on the render thread. `rf:wait_us` includes the
//!   acquisition's or the admission's wait, which a frame still pending
//!   lengthens.
//! - `dr:*`, a drop: the drop of the handle it names; the memory read after
//!   it can still be held, freed at the device's next maintenance
//!   (`render::probe`).
//! - An upload's summary (`up:*`) is marked once its wait returned, or at
//!   once when it was refused: no wait is added to the refusal, so a
//!   refused upload's queued writes can still be in flight when its
//!   summary is marked.
//!
//! Off, the probe reads no clock and no thread-local of its own: its spans
//! and its own time are zero, and the renderer's after-submit readings are
//! skipped. Its atomics (the frames armed, the first use awaited) are
//! still loaded.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use bms_model::{TimeSource, TimeUs};
use input::QpcClock;

/// The marks one run keeps.
const ROOM: usize = 1 << 20;

/// One mark: µs on the recorder's clock, its tag and its value.
pub type Mark = (u64, &'static str, i64);

/// One mark's place: filled by the one mark that took it, its tag set
/// last, so a tag read means the time and value are there.
pub struct Slot {
    t: AtomicU64,
    v: AtomicI64,
    tag: OnceLock<&'static str>,
}

impl Slot {
    const fn new() -> Self {
        Self {
            t: AtomicU64::new(0),
            v: AtomicI64::new(0),
            tag: OnceLock::new(),
        }
    }

    /// Fills the slot: no other mark has it, so nothing waits.
    pub fn fill(&self, t: u64, tag: &'static str, v: i64) {
        self.t.store(t, Ordering::Relaxed);
        self.v.store(v, Ordering::Relaxed);
        let _ = self.tag.set(tag);
    }
}

/// Bounded storage of marks: its slots reserved once when turned on, each
/// taken by one atomic step, so a mark never allocates or waits; the marks
/// past the room are counted.
pub struct Recorder {
    on: AtomicBool,
    clock: OnceLock<QpcClock>,
    slots: OnceLock<Box<[Slot]>>,
    /// The slots taken so far, past the room included.
    taken: AtomicUsize,
    dropped: AtomicU64,
}

impl Recorder {
    pub const fn new() -> Self {
        Self {
            on: AtomicBool::new(false),
            clock: OnceLock::new(),
            slots: OnceLock::new(),
            taken: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Reserves `room` slots, keeps `clock` and turns the recorder on: a
    /// mark's time is `clock`'s. Allocates.
    pub fn turn_on(&self, room: usize, clock: QpcClock) {
        let _ = self
            .slots
            .get_or_init(|| (0..room).map(|_| Slot::new()).collect());
        let _ = self.clock.set(clock);
        self.on.store(true, Ordering::Release);
    }

    pub fn on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// The probe's clock now, in µs from its origin.
    fn now(&self) -> Option<u64> {
        let now = self.clock.get()?.now();
        Some(u64::try_from(now.0).unwrap_or(0))
    }

    /// Takes the next slot, or counts a mark past the room as dropped.
    pub fn take(&self) -> Option<&Slot> {
        let slots = self.slots.get()?;
        // The count goes on past the room, unsaturated: it wraps only after
        // 2^64 marks, out of any run's reach, where a saturating
        // compare-exchange loop would make the marking threads contend.
        let slot = slots.get(self.taken.fetch_add(1, Ordering::Relaxed));
        if slot.is_none() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        slot
    }

    /// Keeps the mark in a slot of its own if there is room, else counts
    /// it as dropped.
    pub fn mark(&self, tag: &'static str, v: i64) {
        if !self.on() {
            return;
        }
        let Some(t) = self.now() else {
            return;
        };
        if let Some(slot) = self.take() {
            slot.fill(t, tag, v);
        }
    }

    /// Keeps a mark at `at`, a time the game read on the recorder's
    /// clock, as [`mark`](Self::mark) keeps one at its own reading.
    pub fn mark_at(&self, tag: &'static str, at: TimeUs, v: i64) {
        if !self.on() {
            return;
        }
        if let Some(slot) = self.take() {
            slot.fill(u64::try_from(at.0).unwrap_or(0), tag, v);
        }
    }

    /// The marks kept, in the order their slots were taken, and the slots
    /// taken but not filled (a mark under way when they were read).
    /// Allocates: read when no mark is due, or in tests.
    pub fn marks(&self) -> (Vec<Mark>, u64) {
        let Some(slots) = self.slots.get() else {
            return (Vec::new(), 0);
        };
        let taken = self.taken.load(Ordering::Acquire).min(slots.len());
        let mut kept = Vec::with_capacity(taken);
        let mut unfilled = 0;
        for s in slots.iter().take(taken) {
            match s.tag.get() {
                Some(tag) => kept.push((
                    s.t.load(Ordering::Relaxed),
                    *tag,
                    s.v.load(Ordering::Relaxed),
                )),
                None => unfilled += 1,
            }
        }
        (kept, unfilled)
    }

    /// The marks past the room.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

static REC: Recorder = Recorder::new();

/// Turns the probe on when `TAPESTRY_LOOP_PROBE` is set: its buffer
/// reserved, its marks timed on `clock`, the game's, and the renderer's
/// marks routed here. Called once, at the start of the process.
pub fn init(clock: QpcClock) {
    if std::env::var_os("TAPESTRY_LOOP_PROBE").is_none() {
        return;
    }
    turn_on(clock);
}

/// [`init`]'s work, whatever the environment.
fn turn_on(clock: QpcClock) {
    REC.turn_on(ROOM, clock);
    render::probe::set_sink(mark);
}

pub fn on() -> bool {
    REC.on()
}

/// Keeps a mark, as the probe's own work ([`render::probe::own`]): left out
/// of every probe figure on this thread.
pub fn mark(tag: &'static str, v: i64) {
    if REC.on() {
        render::probe::own(|| REC.mark(tag, v));
    }
}

/// Keeps a deciding mark at `at`, the game's own time of its event on the
/// game's clock (the probe's).
pub fn mark_at(tag: &'static str, at: TimeUs, v: i64) {
    if REC.on() {
        render::probe::own(|| REC.mark_at(tag, at, v));
    }
}

/// Marks an opening (`g:open`, `kind` 0 a full one, 1 a round's
/// candidate) as it begins, then writes G2's sample (`commit_ready`'s
/// return to here) to `out`: a stalled writer delays the opening, not its
/// mark.
pub fn mark_open(
    kind: i64,
    ready: Option<(std::time::Instant, &str)>,
    out: &mut impl std::io::Write,
) {
    mark("g:open", kind);
    if let Some((at, step)) = ready {
        let _ = writeln!(
            out,
            "dev: probe G2 ready to open: {} us result {step}",
            at.elapsed().as_micros()
        );
    }
}

pub fn us(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

/// Result frames left to probe after a result's first view installed.
pub static RESULT_FRAMES: AtomicU32 = AtomicU32::new(0);

/// Whether the current result frame is one of those probed.
pub static RF_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Every level of every page of `pages`, in bytes (RGBA8, CPU side).
pub fn pages_level_bytes(pages: &skin_model::PackedPages) -> i64 {
    let n: usize = pages
        .pages()
        .iter()
        .flat_map(|p| (0..skin_model::MIP_LEVELS).filter_map(|k| p.level(k)))
        .map(<[u8]>::len)
        .sum();
    i64::try_from(n).unwrap_or(-1)
}

/// Tags render-side uploads with a caller until dropped (the probe on).
pub struct Caller(Option<u8>);

impl Caller {
    pub fn set(c: u8) -> Self {
        Self(on().then(|| render::probe::set_caller(c)))
    }
}

impl Drop for Caller {
    fn drop(&mut self) {
        if let Some(c) = self.0 {
            render::probe::set_caller(c);
        }
    }
}

/// Whether the marks were printed.
static DUMPED: AtomicBool = AtomicBool::new(false);

/// Prints the marks kept, then how many were not kept: past the room, or
/// taken but not filled when read. Once per process: the scenario prints
/// them before its transition record, and every command's end prints them
/// if nothing did.
pub fn dump() {
    if !on() || DUMPED.swap(true, Ordering::AcqRel) {
        return;
    }
    let (marks, unfilled) = REC.marks();
    for (t, tag, v) in marks {
        println!("dev: probe {t} {tag} {v}");
    }
    let past = REC.dropped();
    println!(
        "dev: probe-dropped {} (past the room {past}, unfilled {unfilled})",
        past.saturating_add(unfilled)
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::AtomicU32;

    use super::Recorder;
    use crate::alloc_count::this_thread;

    /// Turns the process's probe on, as `TAPESTRY_LOOP_PROBE` would at
    /// start, on a clock of its own: for a test run in a process of its
    /// own (nextest).
    pub(crate) fn turn_on() {
        super::turn_on(input::QpcClock::start());
    }

    /// [`turn_on`] on `clock`, a rig's game clock.
    pub(crate) fn turn_on_on(clock: input::QpcClock) {
        super::turn_on(clock);
    }

    /// The marks kept so far.
    pub(crate) fn marks() -> Vec<super::Mark> {
        super::REC.marks().0
    }

    /// The probe's clock now, in the marks' µs.
    pub(crate) fn now() -> u64 {
        super::REC.now().unwrap_or(0)
    }

    /// The marks not kept for want of room.
    pub(crate) fn dropped() -> u64 {
        super::REC.dropped()
    }

    /// The probe's in-play hooks, on, allocate nothing: a mark kept, marks
    /// past the room (counted as dropped, not kept), a result frame's
    /// countdown and the in-play switch; with a positive control.
    #[test]
    fn transition_probe_marks_alloc() {
        let rec = Recorder::new();
        rec.turn_on(4, input::QpcClock::start());
        let frames = AtomicU32::new(2);
        let before = this_thread();
        for k in 0..6 {
            rec.mark("loop", k);
        }
        let counted = [
            render::probe::count_down(&frames),
            render::probe::count_down(&frames),
            render::probe::count_down(&frames),
        ];
        render::probe::set_in_play(true);
        render::probe::set_in_play(false);
        let after = this_thread();
        assert_eq!(after - before, 0, "allocations");
        assert_eq!(counted, [true, true, false]);
        let (kept, unfilled) = rec.marks();
        assert_eq!(kept.iter().map(|m| m.2).collect::<Vec<_>>(), [0, 1, 2, 3]);
        assert_eq!((rec.dropped(), unfilled), (2, 0));
        // The positive control: an allocation on this thread is counted.
        let v = std::hint::black_box(vec![0_u8; 16]);
        assert!(this_thread() > after);
        drop(v);
    }

    /// A mark waits for no other: with a slot taken and not filled (its
    /// thread descheduled mid-mark), another thread's marks are kept at
    /// once, and the slot left unfilled is counted when the marks are read.
    #[test]
    fn a_mark_under_way_holds_up_no_other_mark() {
        let rec = std::sync::Arc::new(Recorder::new());
        rec.turn_on(8, input::QpcClock::start());
        assert!(rec.take().is_some(), "a slot taken");
        let (done, finished) = std::sync::mpsc::channel();
        let other = std::sync::Arc::clone(&rec);
        std::thread::spawn(move || {
            for k in 0..3 {
                other.mark("other", k);
            }
            let _ = done.send(());
        });
        assert!(
            finished
                .recv_timeout(std::time::Duration::from_secs(10))
                .is_ok(),
            "a mark waited for the one under way"
        );
        let (kept, unfilled) = rec.marks();
        assert_eq!(
            kept.iter().map(|m| (m.1, m.2)).collect::<Vec<_>>(),
            [("other", 0), ("other", 1), ("other", 2)]
        );
        assert_eq!((rec.dropped(), unfilled), (0, 1), "the mark under way");
    }

    /// Busy for `d` on this thread.
    fn busy(d: std::time::Duration) {
        let t = std::time::Instant::now();
        while t.elapsed() < d {
            std::hint::spin_loop();
        }
    }

    /// A probe figure leaves the probe's own work on its thread out:
    /// nested own work is counted once, and a span's time to another span
    /// leaves out the work between them. Bounded by the wall time around
    /// each figure, not by a guess at the host's speed, so a loaded host
    /// cannot fail it.
    #[test]
    fn a_span_leaves_the_probes_own_work_out() {
        use render::probe::{Span, own, own_time};
        use std::time::{Duration, Instant};
        turn_on();
        let work = Duration::from_millis(20);
        let around = Instant::now();
        let (span, before) = (Span::start(), own_time());
        let inner = Instant::now();
        own(|| own(|| busy(work)));
        let inner = inner.elapsed();
        let counted = own_time() - before;
        let to = Span::start();
        let wall = around.elapsed();
        // Counted once: no more than the wall time of the outer call.
        assert!(
            counted >= work && counted <= inner,
            "own work counted {counted:?} for {work:?} in {inner:?}"
        );
        // The figures leave it out: at most the wall time around them
        // less the work.
        assert!(
            span.until(to) <= wall.saturating_sub(work),
            "{:?}",
            span.until(to)
        );
        let elapsed = span.elapsed();
        assert!(
            elapsed <= around.elapsed().saturating_sub(work),
            "{elapsed:?}"
        );
        // A positive control: work not the probe's is in the figure.
        busy(work);
        assert!(span.elapsed() >= work, "{:?}", span.elapsed());
    }

    /// A frame that would be an array's first use but is skipped before
    /// its submit (minimized, occluded, not admitted) leaves it awaited: the
    /// next frame submitted is its first use, with its upload's caller.
    #[test]
    fn a_skipped_frame_leaves_the_first_use_awaited() {
        turn_on();
        let _caller = super::Caller::set(render::probe::RESULT);
        render::probe::await_first_use(7);
        let result = Some(render::probe::RESULT);
        assert_eq!(render::probe::first_use(7), result, "the frame skipped");
        assert_eq!(render::probe::first_use(7), result, "the frame submitted");
        render::probe::end_first_use(7);
        assert_eq!(render::probe::first_use(7), None, "used");
    }

    /// A mark is the probe's own work, left out of the figures around it.
    /// An opening is marked as it begins, before G2's sample is written:
    /// a writer that stalls 20 ms at each write delays the opening, not
    /// its mark.
    #[test]
    fn the_probe_marks_an_opening_before_its_sample() {
        struct Stalled(Vec<u64>);
        impl std::io::Write for Stalled {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.push(now());
                std::thread::sleep(std::time::Duration::from_millis(20));
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        turn_on();
        let mut out = Stalled(Vec::new());
        super::mark_open(0, Some((std::time::Instant::now(), "x")), &mut out);
        let opened: Vec<u64> = marks()
            .iter()
            .filter(|m| m.1 == "g:open")
            .map(|m| m.0)
            .collect();
        let written = out.0.first().copied().expect("the sample written");
        assert_eq!(opened.len(), 1, "{opened:?}");
        assert!(
            opened.iter().all(|&t| t <= written),
            "the opening marked after its sample: {opened:?} > {written}"
        );
    }

    #[test]
    fn a_mark_is_the_probes_own_work() {
        turn_on();
        let before = render::probe::own_time();
        for k in 0..20_000 {
            super::mark("own", k);
        }
        assert!(render::probe::own_time() > before);
    }

    /// With the probe off a span reads no clock: its figures are zero.
    #[test]
    fn a_span_reads_no_clock_with_the_probe_off() {
        let span = render::probe::Span::start();
        busy(std::time::Duration::from_millis(2));
        assert_eq!(span.elapsed(), std::time::Duration::ZERO);
    }

    /// The frames armed after a drop are not cut by a first use's shorter
    /// count, nor the other way round.
    #[test]
    fn the_frames_armed_after_a_drop_or_a_first_use_keep_the_longer() {
        turn_on();
        render::probe::arm_post(8);
        render::probe::arm_post(3);
        assert_eq!(render::probe::post_left(), 8);
    }
}
