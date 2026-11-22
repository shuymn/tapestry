//! Every interleaving of a cooperating worker with the game's reads of a
//! frame ring (design.md "Rendering and Skins", video frames; "Threat
//! Model" T02): the worker's frames, across runs and seeks, are never
//! failed, torn, shown twice or out of order, and never left behind.
//!
//! The check is a small explicit-state model checker over the real
//! [`Reader`] and [`Writer`]. Every word the reader asks for goes through
//! the ring's [`Words`], so before each word that a worker's step can change
//! (the watermark, read before the scan, and each slot's state, read as the
//! scan reaches it) the checker may run any step the worker could take
//! next: take the game's seek, claim any free slot (the worker's own scan
//! for one can pass a slot the game frees an instant later), publish the
//! frame it claimed, or raise the watermark after it, which lands in a
//! shadow word until that step. The words a step cannot change (a published
//! slot's other words and its frame, which only the game frees) need no
//! choice there, and a worker's step between two of the game's frees, or
//! after the poll, is the same as one before the next poll's watermark. The
//! checker enumerates the choices depth first, replaying each prefix from
//! the start, and does not explore a state twice: a state is the poll in
//! progress (the reader's state before it, the due stamp and every word it
//! read so far), the ring, the worker and what the game has sent.
//!
//! It checks, at every poll, that the reader faults nothing and takes the
//! newest frame of its run due and published before its watermark read,
//! whole, after every frame it took before, leaving behind only frames of
//! its run not due yet and every frame above the watermark, published before
//! the poll or during it; and, at the end of the game's script, that once
//! the worker has nothing left to publish the polls drain the ring and show
//! the run's last frame. A loom model would also explore the atomics'
//! weaker orderings, but it needs the ring built on loom's own atomics
//! (`skin-model` has no such build) and its thread-by-thread exploration
//! grows past what this test can afford at four slots and two runs; the
//! orderings are the reader's acquire loads of each slot's state and of the
//! watermark against the worker's release stores, the standard publication
//! pattern, and are not explored here.
//!
//! A control breaks the hand-over of runs: a worker that publishes a frame
//! of an earlier run after one of a later run. The checker must find the
//! sequence fault that frame causes.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use skin_model::media::{
    Fault, Frame, Geometry, Reader, SLOT_HEADER_WORDS, SLOTS, Slot, WATERMARK_WORD, Window, Words,
    Writer,
};

/// The protocol's slot states.
const FREE: u64 = 0;
const WRITING: u64 = 1;
const READY: u64 = 2;

/// A slot header's words, as the layout orders them.
const SEQ: usize = 1;
const RUN: usize = 2;
const STAMP: usize = 3;

/// Frames of two words: twelve bytes, four of them spare.
fn geometry() -> Geometry {
    let g = Geometry::new(4, 2).unwrap();
    assert_eq!((g.frame_bytes(), g.frame_words()), (12, 2));
    g
}

/// Frame `seq`'s bytes, unlike every other frame's.
fn frame(seq: u64) -> Vec<u8> {
    (0..12_u64).map(|i| (seq * 37 + i * 11 + 1) as u8).collect()
}

/// Slot `slot`'s state word.
fn state_word(g: Geometry, slot: usize) -> usize {
    g.frame_start(slot) - SLOT_HEADER_WORDS
}

/// Slot `slot`'s word `i`.
fn slot_word(ring: &[AtomicU64], g: Geometry, slot: usize, i: usize) -> u64 {
    ring[state_word(g, slot) + i].load(Ordering::Relaxed)
}

/// The published frames in the ring: (sequence, run, stamp, slot).
fn ready(ring: &[AtomicU64], g: Geometry) -> Vec<(u64, u64, i64, usize)> {
    (0..SLOTS)
        .filter(|&s| slot_word(ring, g, s, 0) == READY)
        .map(|s| {
            (
                slot_word(ring, g, s, SEQ),
                slot_word(ring, g, s, RUN),
                slot_word(ring, g, s, STAMP).cast_signed(),
                s,
            )
        })
        .collect()
}

/// A run the game starts: its window and the stamps of the frames the
/// worker decodes for it, from the run's position on.
pub struct RunPlan {
    window: Window,
    stamps: &'static [i64],
}

/// What the game does next.
#[derive(Clone, Copy, Debug)]
pub enum Game {
    /// Polls the ring at this due stamp: a peek and the take of the frame
    /// it finds due.
    Poll(i64),
    /// Starts the plan's next run and sends the worker the seek naming it.
    Start,
}

/// A model: the runs, the game's script, and whether the worker breaks the
/// hand-over of runs.
pub struct Plan {
    runs: &'static [RunPlan],
    script: &'static [Game],
    breach: bool,
}

/// A step the checker can take at a choice point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The reader goes on.
    Proceed,
    /// The worker takes the game's next seek.
    Take,
    /// It claims this slot for its run's next frame.
    Claim(usize),
    /// It claims this slot for the earlier run's frame it held back (the
    /// breach).
    ClaimLate(usize),
    /// It publishes the frame it claimed a slot for.
    Publish,
    /// It raises the watermark to the frame it published.
    Raise,
}

/// The worker's view while it claims: every free slot but `target` looks
/// taken, so the real [`Writer::claim`] picks `target`.
struct ClaimView<'a> {
    ring: &'a [AtomicU64],
    g: Geometry,
    target: usize,
    busy: AtomicU64,
}

impl Words for ClaimView<'_> {
    fn words(&self, start: usize, len: usize) -> Option<&[AtomicU64]> {
        let other =
            len == 1 && (0..SLOTS).any(|s| s != self.target && start == state_word(self.g, s));
        if other {
            return Some(std::slice::from_ref(&self.busy));
        }
        self.ring.words(start, len)
    }
}

/// The worker's view while it publishes: its watermark raise lands in a
/// shadow word, and every word it asks for is logged.
struct PublishView<'a> {
    ring: &'a [AtomicU64],
    shadow: &'a AtomicU64,
    log: RefCell<Vec<(usize, usize)>>,
}

impl Words for PublishView<'_> {
    fn words(&self, start: usize, len: usize) -> Option<&[AtomicU64]> {
        self.log.borrow_mut().push((start, len));
        if (start, len) == (WATERMARK_WORD, 1) {
            return Some(std::slice::from_ref(self.shadow));
        }
        self.ring.words(start, len)
    }
}

/// The worker: the real [`Writer`] and where it is in the plan.
struct WorkerModel {
    writer: Writer,
    /// Its watermark raise, until the step that makes it.
    shadow: AtomicU64,
    /// The plan's run it publishes in.
    run: usize,
    /// That run's next frame.
    next: usize,
    /// The last sequence it published.
    seq: u64,
    /// The slot it claimed and the run and stamp of the frame going in.
    claimed: Option<(Slot, u64, i64)>,
    /// Whether its last publication's watermark raise is still to come.
    raise: bool,
    /// The seeks it has taken.
    taken: usize,
    /// The breach: an earlier run's frame held back, its run and stamp.
    late: Option<(u64, i64)>,
    /// Every frame it published: (sequence, run, stamp).
    published: Vec<(u64, u64, i64)>,
}

impl WorkerModel {
    fn attach(ring: &[AtomicU64]) -> Self {
        Self {
            writer: Writer::attach(ring).unwrap(),
            shadow: AtomicU64::new(0),
            run: 0,
            next: 0,
            seq: 0,
            claimed: None,
            raise: false,
            taken: 0,
            late: None,
            published: Vec::new(),
        }
    }

    fn fingerprint(&self, h: &mut impl Hasher) {
        (
            self.run,
            self.next,
            self.seq,
            self.claimed.as_ref().map(|(s, r, t)| (s.index(), *r, *t)),
            self.raise,
            self.taken,
            self.late,
            self.shadow.load(Ordering::Relaxed),
        )
            .hash(h);
    }

    /// Whether it has frames of its run left to publish.
    fn has_frames(&self, plan: &Plan) -> bool {
        self.next < plan.runs[self.run].stamps.len()
    }

    /// The steps it can take next, the reader's going on first, given the
    /// seeks sent and the ring.
    fn options(&self, sent: usize, ring: &[AtomicU64], g: Geometry, plan: &Plan) -> Vec<Step> {
        let mut steps = vec![Step::Proceed];
        if self.raise {
            steps.push(Step::Raise);
            return steps;
        }
        if self.claimed.is_some() {
            steps.push(Step::Publish);
            return steps;
        }
        if self.taken < sent {
            steps.push(Step::Take);
        }
        let free = (0..SLOTS).filter(|&s| slot_word(ring, g, s, 0) == FREE);
        if self.has_frames(plan) {
            steps.extend(free.clone().map(Step::Claim));
        }
        if self.late.is_some() {
            steps.extend(free.map(Step::ClaimLate));
        }
        steps
    }

    /// Takes `step`; `loaded` is how many slots' states the poll in progress
    /// has loaded. Returns whether it published into one of those slots.
    fn apply(
        &mut self,
        step: Step,
        ring: &[AtomicU64],
        g: Geometry,
        plan: &Plan,
        loaded: usize,
    ) -> bool {
        match step {
            Step::Proceed => unreachable!("not a worker's step"),
            Step::Take => {
                let (earlier, next) = (self.run, self.next);
                self.taken += 1;
                self.run = self.taken;
                self.next = 0;
                if plan.breach && self.late.is_none() && next < plan.runs[earlier].stamps.len() {
                    self.late = Some((earlier as u64 + 1, plan.runs[earlier].stamps[next]));
                }
            }
            Step::Claim(target) => {
                let slot = self.claim(ring, g, target);
                let stamp = plan.runs[self.run].stamps[self.next];
                self.next += 1;
                self.claimed = Some((slot, self.run as u64 + 1, stamp));
            }
            Step::ClaimLate(target) => {
                let slot = self.claim(ring, g, target);
                let (run, stamp) = self.late.take().unwrap();
                self.claimed = Some((slot, run, stamp));
            }
            Step::Publish => {
                let (slot, run, stamp) = self.claimed.take().unwrap();
                let index = slot.index();
                let seq = self.seq + 1;
                let view = PublishView {
                    ring,
                    shadow: &self.shadow,
                    log: RefCell::new(Vec::new()),
                };
                self.writer
                    .publish(&view, slot, &frame(seq), seq, run, stamp)
                    .unwrap();
                assert_publication_order(&view.log.borrow(), g, index);
                self.seq = seq;
                self.raise = true;
                self.published.push((seq, run, stamp));
                return index < loaded;
            }
            Step::Raise => {
                ring[WATERMARK_WORD]
                    .fetch_max(self.shadow.load(Ordering::Relaxed), Ordering::Release);
                self.raise = false;
            }
        }
        false
    }

    /// Claims `target`, a free slot, through the real [`Writer::claim`].
    fn claim(&self, ring: &[AtomicU64], g: Geometry, target: usize) -> Slot {
        let view = ClaimView {
            ring,
            g,
            target,
            busy: AtomicU64::new(WRITING),
        };
        let slot = self.writer.claim(&view).expect("the free slot");
        assert_eq!(slot.index(), target);
        assert_eq!(slot_word(ring, g, target, 0), WRITING, "claimed");
        slot
    }

    /// Whether it has taken every seek sent and published everything.
    fn finished(&self, sent: usize, plan: &Plan) -> bool {
        self.taken == sent
            && !self.has_frames(plan)
            && self.claimed.is_none()
            && !self.raise
            && self.late.is_none()
    }
}

/// Asserts the real [`Writer::publish`] wrote slot `slot`'s frame and words
/// first, then marked it ready, then raised the watermark.
#[track_caller]
fn assert_publication_order(log: &[(usize, usize)], g: Geometry, slot: usize) {
    let state = state_word(g, slot);
    let n = log.len();
    assert!(n >= 3, "a publication of {n} requests: {log:?}");
    assert_eq!(
        log[n - 1],
        (WATERMARK_WORD, 1),
        "the watermark rises last: {log:?}"
    );
    assert_eq!(
        log[n - 2],
        (state, 1),
        "the slot is marked ready before it: {log:?}"
    );
    let frame = g.frame_start(slot)..g.frame_start(slot) + g.frame_words();
    for &(start, len) in &log[..n - 2] {
        let field = len == 1 && start > state && start < frame.start;
        let in_frame = frame.contains(&start) && start + len <= frame.end;
        assert!(
            field || in_frame,
            "the worker asked for {start}+{len} before it marked slot {slot} ready: {log:?}"
        );
    }
}

/// What the game has taken so far: the last frame and its run.
#[derive(Clone, Copy, Debug, Default, Hash)]
struct Shown(Option<(u64, i64, u64)>);

/// The events the enumeration must reach, so that a weakened model shows.
#[derive(Debug, Default)]
pub struct Reached {
    pub runs: u64,
    pub states: usize,
    pub polls: u64,
    /// A frame published into a slot whose state the poll had already
    /// loaded.
    pub passed: u64,
    /// A frame left in the ring above the watermark a poll read.
    pub deferred: u64,
    /// A frame of an earlier run freed unread.
    pub unread: u64,
    /// The worker with frames to publish and no free slot.
    pub blocked: u64,
    /// A poll that took a frame.
    pub staged: u64,
}

/// One execution: the ring, the reader's poll in progress, the worker, and
/// the choices made.
struct Exec<'a> {
    plan: &'a Plan,
    g: Geometry,
    ring: &'a [AtomicU64],
    worker: RefCell<WorkerModel>,
    /// The seeks the game has sent.
    sent: Cell<usize>,
    prefix: &'a [(usize, usize)],
    made: RefCell<Vec<(usize, usize)>>,
    visited: &'a RefCell<HashSet<u64>>,
    reached: &'a RefCell<Reached>,
    /// A state seen before: the run goes on without choices to the end of
    /// the poll in progress and stops.
    pruned: Cell<bool>,
    /// The drain after the script: no choices.
    draining: Cell<bool>,
    /// The poll in progress: its state before it, the words it read, how
    /// many slots' states it has loaded, the watermark it read and the
    /// frames in the ring at or below it then.
    poll_key: Cell<u64>,
    trace: RefCell<Vec<u64>>,
    loaded: Cell<usize>,
    watermark: Cell<Option<u64>>,
    below: RefCell<Vec<(u64, u64, i64, usize)>>,
    /// The reader's state before the poll, as text.
    debug: RefCell<String>,
}

/// The reader's view of the ring: the checker's choice points.
struct ReaderView<'e, 'a>(&'e Exec<'a>);

impl Words for ReaderView<'_, '_> {
    fn words(&self, start: usize, len: usize) -> Option<&[AtomicU64]> {
        let e = self.0;
        let watermark = (start, len) == (WATERMARK_WORD, 1);
        let scan = e.loaded.get();
        let state = len == 1 && scan < SLOTS && start == state_word(e.g, scan);
        if watermark || state {
            e.schedule(start, len);
        }
        // The reader loads the slot's state once the worker's steps here are
        // done: a frame published into it now is one the poll sees.
        if state {
            e.loaded.set(scan + 1);
        }
        if watermark {
            let w = e.ring[WATERMARK_WORD].load(Ordering::Relaxed);
            e.watermark.set(Some(w));
            *e.below.borrow_mut() = ready(e.ring, e.g)
                .into_iter()
                .filter(|f| f.0 <= w)
                .collect();
        }
        let words = e.ring.words(start, len);
        let mut trace = e.trace.borrow_mut();
        trace.extend([start as u64, len as u64]);
        trace.extend(
            words
                .into_iter()
                .flatten()
                .map(|w| w.load(Ordering::Relaxed)),
        );
        words
    }
}

impl Exec<'_> {
    /// Runs the worker's steps the choices give, until the reader goes on.
    fn schedule(&self, start: usize, len: usize) {
        loop {
            let steps = self
                .worker
                .borrow()
                .options(self.sent.get(), self.ring, self.g, self.plan);
            {
                let w = self.worker.borrow();
                if w.claimed.is_none()
                    && !w.raise
                    && w.has_frames(self.plan)
                    && steps.iter().all(|s| !matches!(s, Step::Claim(_)))
                {
                    self.reached.borrow_mut().blocked += 1;
                }
            }
            let step = self.choose(&steps, start, len);
            if step == Step::Proceed {
                return;
            }
            let passed = self.worker.borrow_mut().apply(
                step,
                self.ring,
                self.g,
                self.plan,
                self.loaded.get(),
            );
            if passed {
                self.reached.borrow_mut().passed += 1;
            }
        }
    }

    fn choose(&self, steps: &[Step], start: usize, len: usize) -> Step {
        if steps.len() == 1 || self.pruned.get() || self.draining.get() {
            return Step::Proceed;
        }
        let at = self.made.borrow().len();
        let index = if let Some(&(c, n)) = self.prefix.get(at) {
            assert_eq!(n, steps.len(), "the replay diverged at choice {at}");
            c
        } else {
            if !self.visited.borrow_mut().insert(self.key(start, len)) {
                self.pruned.set(true);
                return Step::Proceed;
            }
            0
        };
        self.made.borrow_mut().push((index, steps.len()));
        steps[index]
    }

    /// The state at a choice point.
    fn key(&self, start: usize, len: usize) -> u64 {
        let mut h = DefaultHasher::new();
        self.poll_key.get().hash(&mut h);
        self.trace.borrow().hash(&mut h);
        (start, len, self.loaded.get(), self.watermark.get()).hash(&mut h);
        self.below.borrow().hash(&mut h);
        self.ring[WATERMARK_WORD]
            .load(Ordering::Relaxed)
            .hash(&mut h);
        for s in 0..SLOTS {
            let state = slot_word(self.ring, self.g, s, 0);
            state.hash(&mut h);
            if state == READY {
                for w in &self.ring
                    [state_word(self.g, s) + 1..self.g.frame_start(s) + self.g.frame_words()]
                {
                    w.load(Ordering::Relaxed).hash(&mut h);
                }
            }
        }
        self.worker.borrow().fingerprint(&mut h);
        self.sent.get().hash(&mut h);
        h.finish()
    }

    /// Polls at `due`, step `at` of the script, and checks the poll.
    fn poll(
        &self,
        reader: &mut Reader,
        shown: &mut Shown,
        at: usize,
        due: i64,
    ) -> Result<(), Fault> {
        let mut h = DefaultHasher::new();
        let mut debug = self.debug.borrow_mut();
        debug.clear();
        write!(debug, "{reader:?}").unwrap();
        (at, debug.as_str(), due, *shown).hash(&mut h);
        drop(debug);
        self.poll_key.set(h.finish());
        self.trace.borrow_mut().clear();
        self.loaded.set(0);
        self.watermark.set(None);
        self.below.borrow_mut().clear();
        let run = reader.run();
        let polled = read(reader, &ReaderView(self), due)?;

        let w = self.watermark.get().expect("the poll read the watermark");
        let below = self.below.borrow();
        let due_frames = below.iter().filter(|f| f.1 == run && f.2 <= due);
        let newest = due_frames.max_by_key(|f| f.0).map(|f| (f.0, f.2));
        assert_eq!(
            polled.taken.as_ref().map(|(f, _)| (f.seq, f.stamp)),
            newest,
            "the poll at {due} takes the newest frame of run {run} due and at or below the watermark {w}, of {below:?}"
        );
        let mut reached = self.reached.borrow_mut();
        reached.polls += 1;
        if let Some((f, bytes)) = &polled.taken {
            let f = *f;
            assert!(*bytes == frame(f.seq), "frame {} taken whole", f.seq);
            if let Some((seq, stamp, r)) = shown.0 {
                assert!(f.seq > seq, "{f:?} taken after frame {seq}");
                assert!(
                    r != run || f.stamp > stamp,
                    "{f:?} taken after stamp {stamp} in run {run}"
                );
            }
            shown.0 = Some((f.seq, f.stamp, run));
            reached.staged += 1;
        }
        let left = ready(self.ring, self.g);
        // Every frame published above the watermark the poll read, before
        // the poll or during it, stays queued: the poll frees none of them,
        // whatever else it frees.
        for p in self.worker.borrow().published.iter().filter(|p| p.0 > w) {
            assert!(
                left.iter().any(|l| l.0 == p.0),
                "frame {p:?} above the watermark {w} freed by the poll at {due} in run {run}"
            );
        }
        let mut freed = false;
        for f in below.iter() {
            if left.iter().any(|l| l.0 == f.0) {
                assert!(
                    f.1 == run && f.2 > due,
                    "frame {f:?} left behind by the poll at {due} in run {run}"
                );
            } else {
                freed = true;
                if f.1 < run {
                    reached.unread += 1;
                }
            }
        }
        assert_eq!(polled.freed, freed, "the poll says whether it freed slots");
        if left.iter().any(|l| l.0 > w) {
            reached.deferred += 1;
        }
        Ok(())
    }
}

/// What a peek and the take after it did: the frame taken and its bytes,
/// and whether slots were freed.
struct Read {
    taken: Option<(Frame, Vec<u8>)>,
    freed: bool,
}

/// Peeks at `due` and takes the frame due, if any, at the tight pitch into
/// a buffer of the frame's bytes, each written once.
fn read<W: Words + ?Sized>(reader: &mut Reader, ring: &W, due: i64) -> Result<Read, Fault> {
    let peek = reader.peek(ring, due)?;
    let Some(d) = peek.due else {
        return Ok(Read {
            taken: None,
            freed: peek.freed,
        });
    };
    let g = reader.geometry();
    let w = g.width() as usize;
    let mut out = vec![0_u8; g.frame_bytes()];
    let mut written = vec![false; g.frame_bytes()];
    let mut sink = |at: usize, bytes: &[u8]| {
        let end = at + bytes.len();
        for w in &mut written[at..end] {
            assert!(!*w, "a byte written twice");
            *w = true;
        }
        out[at..end].copy_from_slice(bytes);
        true
    };
    let f = reader.take(ring, d, [w, w.div_ceil(2)], &mut sink)?;
    assert_eq!((f.seq, f.stamp), (d.seq(), d.stamp()));
    assert!(
        written.iter().all(|w| *w),
        "every byte of the frame written"
    );
    Ok(Read {
        taken: Some((f, out)),
        freed: true,
    })
}

/// How one execution ended.
enum Ended {
    /// The script ran out, or a state seen before stopped it.
    Done(Vec<(usize, usize)>),
    /// A poll faulted.
    Fault(Fault, Vec<(usize, usize)>),
}

/// Runs `plan` once, the choices taken from `prefix` and, past it, the
/// first of each.
fn execute(
    plan: &Plan,
    prefix: &[(usize, usize)],
    visited: &RefCell<HashSet<u64>>,
    reached: &RefCell<Reached>,
) -> Ended {
    let g = geometry();
    let mem: Vec<AtomicU64> = (0..g.words()).map(|_| AtomicU64::new(0)).collect();
    let ring = &*mem;
    let mut reader = Reader::new(ring, g, plan.runs[0].window).unwrap();
    let e = Exec {
        plan,
        g,
        ring,
        worker: RefCell::new(WorkerModel::attach(ring)),
        sent: Cell::new(0),
        prefix,
        made: RefCell::new(Vec::new()),
        visited,
        reached,
        pruned: Cell::new(false),
        draining: Cell::new(false),
        poll_key: Cell::new(0),
        trace: RefCell::new(Vec::new()),
        loaded: Cell::new(0),
        watermark: Cell::new(None),
        below: RefCell::new(Vec::new()),
        debug: RefCell::new(String::new()),
    };
    let mut shown = Shown::default();
    for (at, game) in plan.script.iter().enumerate() {
        match *game {
            Game::Start => {
                let next = e.sent.get() + 1;
                assert_eq!(reader.start_run(plan.runs[next].window), next as u64 + 1);
                e.sent.set(next);
            }
            Game::Poll(due) => {
                if let Err(f) = e.poll(&mut reader, &mut shown, at, due) {
                    return Ended::Fault(f, e.made.take());
                }
            }
        }
        if e.pruned.get() {
            return Ended::Done(e.made.take());
        }
    }

    // The drain: the worker publishes what it can, the game polls with
    // everything due, until nothing is left.
    e.draining.set(true);
    for _ in 0..64 {
        loop {
            let steps = e.worker.borrow().options(e.sent.get(), ring, g, plan);
            let Some(&step) = steps.iter().find(|s| **s != Step::Proceed) else {
                break;
            };
            e.worker.borrow_mut().apply(step, ring, g, plan, SLOTS);
        }
        if e.worker.borrow().finished(e.sent.get(), plan) && ready(ring, g).is_empty() {
            break;
        }
        if let Err(f) = e.poll(&mut reader, &mut shown, usize::MAX, i64::MAX) {
            return Ended::Fault(f, e.made.take());
        }
    }
    let worker = e.worker.borrow();
    assert!(
        worker.finished(e.sent.get(), plan) && ready(ring, g).is_empty(),
        "the ring did not drain: {:?}",
        ready(ring, g)
    );
    let last = worker
        .published
        .iter()
        .filter(|p| p.1 == reader.run())
        .max_by_key(|p| p.0)
        .map(|p| p.0);
    assert_eq!(
        shown.0.filter(|s| s.2 == reader.run()).map(|s| s.0),
        last,
        "the run's last frame shows once the ring drains"
    );
    drop(worker);
    Ended::Done(e.made.take())
}

/// Enumerates every interleaving of `plan`: what it reached, or the first
/// fault and the choices that led to it.
pub fn explore(plan: &Plan) -> Result<Reached, (Fault, Vec<(usize, usize)>)> {
    let visited = RefCell::new(HashSet::new());
    let reached = RefCell::new(Reached::default());
    let mut prefix = Vec::new();
    loop {
        let mut made = match execute(plan, &prefix, &visited, &reached) {
            Ended::Fault(f, made) => return Err((f, made)),
            Ended::Done(made) => made,
        };
        reached.borrow_mut().runs += 1;
        loop {
            match made.pop() {
                None => {
                    let mut reached = reached.into_inner();
                    reached.states = visited.borrow().len();
                    return Ok(reached);
                }
                Some((c, n)) if c + 1 < n => {
                    made.push((c + 1, n));
                    break;
                }
                Some(_) => {}
            }
        }
        prefix = made;
    }
}

const fn open(first: i64) -> Window {
    Window {
        first,
        last: i64::MAX,
    }
}

/// Three runs: the video from its start, a seek back behind run 1's frames,
/// a seek forward; every slot, frames above the watermark and frames of
/// earlier runs come into play.
static COOPERATING: Plan = Plan {
    runs: &[
        RunPlan {
            window: open(0),
            stamps: &[0, 100, 200],
        },
        RunPlan {
            window: open(50),
            stamps: &[50, 150],
        },
        RunPlan {
            window: open(300),
            stamps: &[300, 400],
        },
    ],
    script: &[
        Game::Poll(-1),
        Game::Poll(0),
        Game::Poll(150),
        Game::Start,
        Game::Poll(40),
        Game::Poll(100),
        Game::Start,
        Game::Poll(350),
    ],
    breach: false,
};

/// The control: a worker that may publish a frame of run 1 after one of
/// run 2.
static BREACH: Plan = Plan {
    runs: &[
        RunPlan {
            window: open(0),
            stamps: &[0, 100, 200],
        },
        RunPlan {
            window: open(50),
            stamps: &[50, 150],
        },
    ],
    script: &[Game::Poll(0), Game::Start, Game::Poll(40), Game::Poll(100)],
    breach: true,
};

// Covers: T02
#[test]
fn ring_cooperating_writer_plays_in_order_in_every_interleaving() {
    let reached = explore(&COOPERATING).unwrap_or_else(|(f, choices)| {
        panic!("a cooperating worker failed its player: {f}; choices {choices:?}")
    });
    eprintln!("{reached:?}");
    assert!(
        reached.passed > 0
            && reached.deferred > 0
            && reached.unread > 0
            && reached.blocked > 0
            && reached.staged > 0,
        "the enumeration reaches every kind of interleaving: {reached:?}"
    );

    // The control: a worker that breaks the hand-over of runs.
    let (fault, choices) = explore(&BREACH).expect_err("a breach of the hand-over faults");
    assert!(
        matches!(fault, Fault::Sequence { .. }),
        "{fault} after choices {choices:?}"
    );
}
