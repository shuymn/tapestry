//! Every interleaving, to a bound, of the fill's events with the
//! foreground's (design.md "Rendering and Skins", chart images; "Load
//! Pipeline"): a read of an ended fill epoch is never adopted, a neighbour's
//! write never evicts a pinned entry or one out of its order, at most one
//! fill read is left behind, and a choice reads its chart's images beside a
//! stalled fill read, decoded, without waiting for it.
//!
//! The check is a small explicit-state model checker over the real worker,
//! coordinator and render thread. Each fill read stops at two gates
//! ([`FillGate`]): as it begins, before its first checkpoint, and once its
//! result is made, before it returns; a read held at a gate ignores its
//! stop, as one starved of a core or past its last checkpoint does. The
//! render thread is held in a selection frame between events, so a pool
//! write sent waits for the acknowledgement event. The events are the
//! select screen's step, the loading waits' service (which takes what ended
//! and sends nothing; explored only with a read ended, where it differs
//! from a step), a pass of the fill read's gate, the render thread's
//! acknowledgement of the write sent, a move between the cursor's row A and
//! its neighbour N1, a rest, a refresh of the list with the cursor kept, a
//! withdrawal of the cursor's entry, a new pool generation, the choice of a
//! chart C in N1's folder, and the return to selection. After each event the
//! checker waits for the threads to settle: a cursor read (never gated) has
//! ended, and a fill read stands at a gate or has ended. The checker
//! enumerates the events depth first, replaying each prefix from a fresh
//! rig (several replays at once, each on a rig and worker of its own, the
//! gates the worker's), and does not explore a state twice: a state is the
//! phase, the cursor, the worker's read, write, fill and slots (versions
//! named by the row they hold), the rows' entries, the gate a read stands
//! at and what the checker has seen of each read.
//!
//! The bound: from the cursor's row A resident, pinned by the frame drawn
//! and the entries padded to their count bound, with the fill's read of N1
//! at its first gate, [`DEPTH`] events, of which at most [`DISRUPTIONS`]
//! move, refresh, withdraw, change the pool's generation or choose (at most
//! one generation and one choice; a new generation only with no write
//! unanswered, which is the same as its answer first or its answer stale,
//! neither adopted into the new generation). Six events reach every path
//! the fill's review named, the longest a neighbour's write sent, a move
//! onto its row and its answer adopted.
//!
//! It checks after every event that no entry, slot or write sent belongs
//! to a fill read whose epoch a move, refresh, withdrawal, pool generation
//! or choice ended before its write was sent; that no slot holds a version
//! nothing owns; that on selection a neighbour's read or write evicts only
//! the padding, older than every row read here (never A, pinned, nor a row
//! of this rest); that at most one fill read's thread lives, its bytes
//! counted while it is left behind; and at the choice, with every gate
//! shut, that play's image job begins and ends, its stage file decoded.
//!
//! A control breaks the epoch's discard
//! ([`ChartImageWorker::break_epoch`](crate::chart_images::ChartImageWorker::break_epoch)):
//! the checker must find the read it adopts.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use game::coord::Spawner;
use skin_model::chart_image::ChartImage;

use super::{Cursor, LetGo, chart, cursor_of, named, png, row};
use crate::chart_currency::CursorChart;
use crate::chart_images::{
    CursorId, EntryState, FillGate, FillGates, FillProbe, LEFT_BEHIND_BYTES, SlotState, Want,
};

/// The events explored after the start.
const DEPTH: usize = 6;

/// The most events of a path that end the fill epoch or change the
/// cursor: a move, a refresh, a withdrawal, a new pool generation and a
/// choice.
const DISRUPTIONS: u8 = 2;

/// How long a thread may take to settle after an event.
const SETTLE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    Step,
    Service,
    Pass,
    Ack,
    Move,
    Rest,
    Refresh,
    Withdraw,
    Generation,
    Choice,
    Return,
}

/// Whether `done` holds within [`SETTLE`].
fn within(done: impl Fn() -> bool) -> bool {
    let end = Instant::now() + SETTLE;
    while !done() {
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

/// The chart folders, shared by every replay: A, N1 (whose folder also
/// holds C's stage file) and N2.
struct World {
    a: tempfile::TempDir,
    n1: tempfile::TempDir,
    n2: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        let n1 = chart([64, 48], [30, 8]);
        png(n1.path(), "c.png", [32, 24]);
        Self {
            a: chart([64, 48], [30, 8]),
            n1,
            n2: chart([64, 48], [30, 8]),
        }
    }

    fn id(dir: &Path) -> CursorId {
        row(dir, named())
    }

    /// The row's name in a state.
    fn label(&self, id: &CursorId) -> &'static str {
        if *id == Self::id(self.a.path()) {
            "A"
        } else if *id == Self::id(self.n1.path()) {
            "N1"
        } else if *id == Self::id(self.n2.path()) {
            "N2"
        } else {
            "?"
        }
    }

    /// The rows near `on`, nearest first.
    fn near(&self, on: &Path) -> Vec<CursorId> {
        if on == self.a.path() {
            vec![Self::id(self.n1.path()), Self::id(self.n2.path())]
        } else {
            vec![Self::id(self.a.path()), Self::id(self.n2.path())]
        }
    }
}

/// One replay: the rig and what the checker has seen.
struct Sim<'w> {
    world: &'w World,
    c: Cursor,
    gates: Arc<FillGates>,
    /// The events of the path that end the epoch or change the cursor.
    disruptions: u8,
    /// The cursor is on A (else on N1), rested in its generation.
    on_a: bool,
    rested: bool,
    /// Between the choice and the return.
    play: bool,
    generations: u8,
    choices: u8,
    /// A's pinned entry's version and where the frame drawn shows it.
    a_version: u64,
    a_shown: Option<(u64, u16)>,
    /// Fill reads whose epoch ended before their write was sent, and
    /// those whose write was sent.
    ended: HashSet<u64>,
    sent: HashSet<u64>,
    /// Writes the render thread answered.
    acked: HashSet<u64>,
    /// Every read's version seen: what is not one is padding.
    seen: HashSet<u64>,
}

type Found = String;

impl<'w> Sim<'w> {
    /// The start: A read, written and shown, its entry pinned by the frame
    /// drawn; the entries padded to their count bound; the render thread
    /// held; the fill's read of N1 at its first gate.
    fn new(world: &'w World, control: bool) -> Self {
        let c = cursor_of(8);
        if control {
            c.w.break_epoch();
        }
        let gates = c.w.fill_gates();
        c.rest_on(world.a.path(), named());
        let EntryState::Resident { generation, slot } = c.shown(world.a.path(), named()) else {
            panic!("A not resident");
        };
        let a_version = c.w.entry(&World::id(world.a.path())).unwrap().1;
        let sim = Self {
            world,
            c,
            gates,
            disruptions: 0,
            on_a: true,
            rested: true,
            play: false,
            generations: 0,
            choices: 0,
            a_version,
            a_shown: Some((generation, slot)),
            ended: HashSet::new(),
            sent: HashSet::new(),
            acked: HashSet::new(),
            seen: HashSet::from([a_version]),
        };
        sim.show_a(1);
        sim.c.w.pad_entries(None, None);
        let _ = sim.c.w.take_evictions();
        sim.gates.shut.store(true, Ordering::Release);
        sim.hold();
        sim.c.w.rest(sim.c.w.cursor_request(
            world.a.path().to_path_buf(),
            named(),
            world.near(world.a.path()),
            &sim.c.coord,
        ));
        sim.c.step();
        sim.settle().unwrap();
        assert_eq!(sim.gates.at.load(Ordering::Acquire), FillGate::Begun as u8);
        sim
    }

    /// The frame drawn on selection lifetime `lifetime` shows A's slot,
    /// pinning it.
    fn show_a(&self, lifetime: u64) {
        let Some((generation, slot)) = self.a_shown else {
            return;
        };
        let mut update = crate::render_side::tests::selection_update(lifetime, 2);
        update.cursor_chart = CursorChart::Resident { generation, slot };
        self.c.r.link.show_selection(update);
        assert!(within(
            || self.c.r.link.pool_now().1 == CursorChart::pack(generation, slot)
        ));
    }

    /// Holds the render thread in a selection frame.
    fn hold(&self) {
        self.c.r.charts.lock().unwrap().stalled = false;
        self.c.r.hold.store(true, Ordering::Release);
        assert!(within(|| self.c.r.charts.lock().unwrap().stalled));
    }

    fn probe(&self) -> FillProbe {
        self.c.w.fill_probe()
    }

    fn cursor_dir(&self) -> &'w Path {
        if self.on_a {
            self.world.a.path()
        } else {
            self.world.n1.path()
        }
    }

    /// Waits for the threads to settle: a cursor read ended, a fill read
    /// at a gate or ended.
    fn settle(&self) -> Result<(), Found> {
        let end = Instant::now() + SETTLE;
        loop {
            let p = self.probe();
            let at = self.gates.at.load(Ordering::Acquire);
            let cursor_busy = p.job.as_ref().is_some_and(|j| j.epoch.is_none() && !j.done);
            let fill_alive = p.job.as_ref().is_some_and(|j| j.epoch.is_some() && !j.done)
                || p.left_behind == Some(false);
            if !cursor_busy && (!fill_alive || at != 0) {
                return Ok(());
            }
            if Instant::now() >= end {
                return Err(format!("the threads never settled: {p:?}, gate {at}"));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The events possible now.
    fn enabled(&self) -> Vec<Ev> {
        if self.play {
            return vec![Ev::Return];
        }
        let p = self.probe();
        let mut out = vec![Ev::Step];
        // The service differs from a step only in taking a read that
        // ended without sending its write.
        if p.job.as_ref().is_some_and(|j| j.done) {
            out.push(Ev::Service);
        }
        if self.gates.at.load(Ordering::Acquire) != 0 {
            out.push(Ev::Pass);
        }
        if p.write
            .as_ref()
            .is_some_and(|w| w.done && !self.acked.contains(&w.version))
        {
            out.push(Ev::Ack);
        }
        if !self.rested || p.flags[1] {
            out.push(Ev::Rest);
        }
        if self.disruptions >= DISRUPTIONS {
            return out;
        }
        out.push(Ev::Move);
        out.push(Ev::Refresh);
        if self.c.w.entry(&World::id(self.cursor_dir())).is_some() {
            out.push(Ev::Withdraw);
        }
        // A new pool generation with a write sent and unanswered is the
        // same as its acknowledgement first, under the old generation, or
        // its answer stale: neither is adopted into the new one.
        if self.generations == 0 && !out.contains(&Ev::Ack) {
            out.push(Ev::Generation);
        }
        if self.choices == 0 {
            out.push(Ev::Choice);
        }
        out
    }

    /// The fill reads whose result or write an epoch's end discards.
    fn unsent(p: &FillProbe) -> Vec<u64> {
        let job = p
            .job
            .as_ref()
            .filter(|j| j.epoch.is_some())
            .map(|j| j.version);
        let write = p
            .write
            .as_ref()
            .filter(|w| w.epoch.is_some() && !w.done)
            .map(|w| w.version);
        job.into_iter().chain(write).collect()
    }

    fn apply(&mut self, ev: Ev) -> Result<(), Found> {
        let before = self.probe();
        let ends = matches!(
            ev,
            Ev::Move | Ev::Refresh | Ev::Withdraw | Ev::Generation | Ev::Choice
        );
        if ends {
            self.disruptions += 1;
            self.ended.extend(Self::unsent(&before));
        }
        match ev {
            Ev::Step => self.c.step(),
            Ev::Service => self.c.w.service(),
            Ev::Pass => self.pass()?,
            Ev::Ack => {
                let w = before.write.as_ref().map(|w| w.version);
                let calls = self.c.r.charts.lock().unwrap().write_calls;
                self.c.r.hold.store(false, Ordering::Release);
                if !within(|| self.c.r.charts.lock().unwrap().write_calls > calls) {
                    return Err("the write sent was never answered".to_owned());
                }
                self.acked.extend(w);
                self.hold();
            }
            Ev::Move => {
                self.on_a = !self.on_a;
                self.rested = false;
                self.c
                    .w
                    .cursor_changed(&self.c.coord, Some(World::id(self.cursor_dir())));
            }
            Ev::Rest => {
                self.rested = true;
                let _ = self.c.w.take_rearm();
                let dir = self.cursor_dir();
                self.c.w.rest(self.c.w.cursor_request(
                    dir.to_path_buf(),
                    named(),
                    self.world.near(dir),
                    &self.c.coord,
                ));
            }
            Ev::Refresh => self.c.w.refill(self.world.near(self.cursor_dir())),
            Ev::Withdraw => self.c.w.withdraw_cursor_entry(),
            Ev::Generation => {
                self.generations += 1;
                self.a_shown = None;
                let next = super::super::pool_of(8);
                *self.c.r.pool.lock().unwrap() = Some(next);
                self.c.r.hold.store(false, Ordering::Release);
                if !within(|| self.c.r.link.pool_now().0 == Some(next)) {
                    return Err("the new pool never reached the render thread".to_owned());
                }
                self.hold();
            }
            Ev::Choice => self.choose(&before)?,
            Ev::Return => self.back(),
        }
        self.settle()?;
        self.check()
    }

    /// Lets the fill read standing at a gate through it, to its next gate
    /// or its end.
    fn pass(&self) -> Result<(), Found> {
        let at = self.gates.at.load(Ordering::Acquire);
        self.gates.pass.store(1, Ordering::Release);
        let end = Instant::now() + SETTLE;
        while self.gates.pass.load(Ordering::Acquire) != 0 {
            if Instant::now() >= end {
                return Err("a pass was never taken".to_owned());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let ended = |p: &FillProbe| {
            p.job.as_ref().is_none_or(|j| j.epoch.is_none() || j.done)
                && p.left_behind != Some(false)
        };
        while !(at == FillGate::Begun as u8
            && self.gates.at.load(Ordering::Acquire) == FillGate::Read as u8)
            && !ended(&self.probe())
        {
            if Instant::now() >= end {
                return Err("the passed read never reached its next gate".to_owned());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    /// The choice of C, in N1's folder, with every gate shut: play's image
    /// job begins beside any fill read, under its own key, and its stage
    /// file is decoded.
    fn choose(&mut self, before: &FillProbe) -> Result<(), Found> {
        self.choices += 1;
        self.play = true;
        let in_flight = before
            .job
            .as_ref()
            .is_some_and(|j| j.epoch.is_some() && !j.done);
        self.c.w.leave_selection(&self.c.coord);
        self.c.r.link.leave_selection();
        self.c.r.hold.store(false, Ordering::Release);
        if in_flight && self.probe().left_behind.is_none() {
            return Err("the fill read in flight was not left behind".to_owned());
        }
        self.c.w.renew();
        self.c.w.attempt(Want {
            dir: self.world.n1.path().to_path_buf(),
            names: ["c.png".to_owned(), "n.png".to_owned(), String::new()],
            play: Some(2_048),
            result: None,
        });
        let publisher = self.c.r.link.chart_publisher();
        let end = Instant::now() + SETTLE;
        while self.c.w.current().is_none() {
            if Instant::now() >= end {
                return Err(format!("play waited: {:?}", self.c.w));
            }
            self.c.w.step(
                Some((&self.c.coord, Spawner::SYSTEM)),
                &|| false,
                Some(&publisher),
            );
            self.c.w.service();
            std::thread::sleep(Duration::from_millis(1));
        }
        let images = self.c.w.current().unwrap();
        match images.read(ChartImage::Stage).image.as_ref() {
            Ok(stage)
                if stage.native() == [32, 24] && stage.levels()[0][..4] == [10, 20, 30, 255] =>
            {
                Ok(())
            }
            other => Err(format!("the chosen chart's stage file: {other:?}")),
        }
    }

    /// Back to selection on A, its frame showing A while resident.
    fn back(&mut self) {
        self.play = false;
        self.c.w.forget();
        self.c.w.drop_images();
        self.c.r.link.enter_selection(2);
        self.c
            .r
            .link
            .show_selection(crate::render_side::tests::selection_update(2, 1));
        // A's entry of a generation the worker has not yet seen end is not
        // drawn.
        let live = self.c.r.link.pool_now().0.map(|l| l.generation);
        self.a_shown = match self.c.w.entry(&World::id(self.world.a.path())) {
            Some((EntryState::Resident { generation, slot }, ..)) if Some(generation) == live => {
                Some((generation, slot))
            }
            _ => None,
        };
        self.show_a(2);
        self.hold();
        self.on_a = true;
        self.rested = false;
        self.c
            .w
            .cursor_changed(&self.c.coord, Some(World::id(self.world.a.path())));
    }

    /// The checks after each event.
    fn check(&mut self) -> Result<(), Found> {
        let p = self.probe();
        self.seen
            .extend(p.job.iter().chain(&p.write).map(|r| r.version));
        if let Some(w) = p.write.as_ref().filter(|w| w.epoch.is_some() && w.done) {
            if self.ended.contains(&w.version) {
                return Err(format!("a write of an ended epoch was sent: {w:?}"));
            }
            self.sent.insert(w.version);
        }
        let rows = [&self.world.a, &self.world.n1, &self.world.n2].map(|d| World::id(d.path()));
        let mut owned: HashSet<u64> = p.job.iter().chain(&p.write).map(|r| r.version).collect();
        for id in &rows {
            if let Some((state, version, ..)) = self.c.w.entry(id) {
                if self.ended.contains(&version) {
                    return Err(format!(
                        "{}'s read of an ended epoch was adopted",
                        self.world.label(id)
                    ));
                }
                if matches!(state, EntryState::Resident { .. }) {
                    owned.insert(version);
                }
            }
        }
        for s in &p.slots {
            if let SlotState::Writing { version } | SlotState::Resident { version } = s
                && !owned.contains(version)
            {
                return Err(format!(
                    "a slot holds version {version}, which nothing owns"
                ));
            }
        }
        // On selection, where A is pinned by the frame drawn: past the choice
        // no frame draws a slot, and a write answered then evicts as the
        // fill may.
        for (fill, version) in self.c.w.take_evictions() {
            if fill && !self.play && self.seen.contains(&version) {
                let which = if version == self.a_version {
                    "A's pinned entry"
                } else {
                    "an entry of a row read"
                };
                return Err(format!("a neighbour's read or write evicted {which}"));
            }
        }
        let fill_alive = p.job.as_ref().is_some_and(|j| j.epoch.is_some() && !j.done);
        if fill_alive && p.left_behind == Some(false) {
            return Err("two fill reads alive".to_owned());
        }
        let counted = self.c.w.left_behind_bytes();
        if counted
            != if p.left_behind.is_some() {
                LEFT_BEHIND_BYTES
            } else {
                0
            }
        {
            return Err(format!("{counted} bytes counted for {:?}", p.left_behind));
        }
        Ok(())
    }

    /// The state, versions named by what holds them.
    fn signature(&self) -> String {
        let p = self.probe();
        let name = |v: u64| -> String {
            let mut s = String::new();
            if p.job.as_ref().is_some_and(|j| j.version == v) {
                s.push('j');
            }
            if p.write.as_ref().is_some_and(|w| w.version == v) {
                s.push('w');
            }
            if self.ended.contains(&v) {
                s.push('e');
            }
            if self.sent.contains(&v) {
                s.push('s');
            }
            if self.acked.contains(&v) {
                s.push('a');
            }
            for d in [&self.world.a, &self.world.n1, &self.world.n2] {
                let id = World::id(d.path());
                if self.c.w.entry(&id).is_some_and(|e| e.1 == v) {
                    s.push_str(self.world.label(&id));
                }
            }
            s
        };
        let read = |r: &Option<crate::chart_images::Probed>| {
            r.as_ref().map(|r| {
                format!(
                    "{}:{}:{}:{}",
                    self.world.label(&r.row),
                    name(r.version),
                    r.epoch
                        .map(|e| e == p.fill_epoch)
                        .map_or("c", |c| if c { "f" } else { "x" }),
                    r.done
                )
            })
        };
        let entries: Vec<String> = [&self.world.a, &self.world.n1, &self.world.n2]
            .iter()
            .map(|d| {
                let id = World::id(d.path());
                format!("{:?}", self.c.w.entry(&id).map(|e| e.0))
            })
            .collect();
        let slots: Vec<String> = p
            .slots
            .iter()
            .map(|s| match s {
                SlotState::Free => "-".to_owned(),
                SlotState::Writing { version } => format!("W{}", name(*version)),
                SlotState::Resident { version } => format!("R{}", name(*version)),
            })
            .collect();
        format!(
            "{} {} {} {} {} {} | {:?} {:?} {:?} {:?} {:?} {:?} | {:?} | {:?} | gate {}",
            self.play,
            self.on_a,
            self.rested,
            self.generations,
            self.choices,
            self.a_shown.is_some(),
            read(&p.job),
            read(&p.write),
            p.left_behind,
            p.flags,
            p.asked.as_ref().map(|r| self.world.label(r)),
            p.fill_rows
                .iter()
                .map(|r| self.world.label(r))
                .collect::<Vec<_>>(),
            entries,
            slots,
            self.gates.at.load(Ordering::Acquire),
        )
    }

    /// Opens the gates and the render thread and ends the worker.
    fn finish(self) {
        self.gates.shut.store(false, Ordering::Release);
        self.c.r.hold.store(false, Ordering::Release);
        self.c.w.end();
    }
}

/// The exploration's shared state: the states seen, each with the most
/// events left after it when it was explored, the paths waiting, the
/// replays running, and the first failed check.
#[derive(Default)]
struct Search {
    visited: HashMap<String, usize>,
    paths: Vec<Vec<Ev>>,
    running: usize,
    replays: usize,
    found: Option<(Vec<Ev>, Found)>,
}

/// Replays `path` from the start: the state it reaches and the events
/// possible there, or the events that led to a failed check and what
/// failed.
fn replay(
    world: &World,
    control: bool,
    path: &[Ev],
) -> Result<(String, Vec<Ev>), (Vec<Ev>, Found)> {
    let mut sim = Sim::new(world, control);
    for (i, &ev) in path.iter().enumerate() {
        if let Err(e) = sim.apply(ev) {
            sim.finish();
            return Err((path[..=i].to_vec(), e));
        }
    }
    let out = (sim.signature(), sim.enabled());
    sim.finish();
    Ok(out)
}

/// What the exploration found: the states and the replays it took, or the
/// events that led to a failed check and what failed. Replays run on
/// several threads, each with a rig of its own, depth first from the
/// newest path waiting.
fn explore(world: &World, control: bool) -> Result<(usize, usize), (Vec<Ev>, Found)> {
    let search = Mutex::new(Search {
        paths: vec![Vec::new()],
        ..Search::default()
    });
    let wake = Condvar::new();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 8));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let path = {
                        let mut s = search.lock().unwrap();
                        loop {
                            if s.found.is_some() {
                                return;
                            }
                            if let Some(path) = s.paths.pop() {
                                s.running += 1;
                                s.replays += 1;
                                if s.replays.is_multiple_of(500) {
                                    eprintln!(
                                        "{} replays, {} states, {} paths waiting",
                                        s.replays,
                                        s.visited.len(),
                                        s.paths.len()
                                    );
                                }
                                break path;
                            }
                            if s.running == 0 {
                                return;
                            }
                            s = wake.wait(s).unwrap();
                        }
                    };
                    let result = replay(world, control, &path);
                    let mut s = search.lock().unwrap();
                    s.running -= 1;
                    match result {
                        Err(f) => {
                            s.found.get_or_insert(f);
                        }
                        Ok((sig, enabled)) => {
                            let left = DEPTH - path.len();
                            if !s.visited.get(&sig).is_some_and(|&l| l >= left) {
                                s.visited.insert(sig, left);
                                if left > 0 {
                                    for ev in enabled {
                                        let mut next = path.clone();
                                        next.push(ev);
                                        s.paths.push(next);
                                    }
                                }
                            }
                        }
                    }
                    wake.notify_all();
                }
            });
        }
    });
    let s = search.into_inner().unwrap();
    match s.found {
        Some(f) => Err(f),
        None => Ok((s.visited.len(), s.replays)),
    }
}

/// Every interleaving to the bound keeps the fill's rules.
#[test]
fn every_interleaving_of_the_fill_keeps_its_rules() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let world = World::new();
    let begun = Instant::now();
    match explore(&world, false) {
        Ok((states, replays)) => {
            eprintln!(
                "{states} states, {replays} replays in {:?}",
                begun.elapsed()
            );
            assert!(states > 100, "{states} states");
        }
        Err((path, found)) => panic!("{path:?}: {found}"),
    }
}

/// The control: with the epoch's discard broken, the exploration finds a
/// read of an ended epoch adopted.
#[test]
fn the_interleavings_find_a_broken_epoch() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let world = World::new();
    let Err((path, found)) = explore(&world, true) else {
        panic!("the broken epoch was not found");
    };
    assert!(found.contains("ended epoch"), "{path:?}: {found}");
}
