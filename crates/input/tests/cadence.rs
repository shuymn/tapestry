//! The input thread's wake policy, run by `cadence::run` on a virtual clock
//! with stubbed waits: a simulated OS queue, timer and wake event.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use bms_model::{TimeSource, TimeUs};
use eval::ControlId;
use input::cadence::{
    self, Boundary, Cadence, DRAIN_CUT, Driven, HEARTBEAT, INPUT_GAP, Phase, Wait, WaitFailed,
    Waiter, Woken,
};
use input::{
    DeviceId, DeviceKind, FocusLatch, Packet, SourceConfig, SourceReader, SourceSet, WakeReport,
};

const KEYBOARD: DeviceId = DeviceId(1);
const A: ControlId = ControlId(0x1e);

/// The virtual clock, in µs.
#[derive(Clone)]
struct Clock(Arc<AtomicI64>);

impl Clock {
    fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }

    fn set(&self, t: i64) {
        self.0.store(t, Ordering::Relaxed);
    }
}

impl TimeSource for Clock {
    fn now(&self) -> TimeUs {
        TimeUs(self.get())
    }
}

/// A queued message: a key report with its value, or a device arrival.
#[derive(Clone, Copy, Debug)]
enum Msg {
    Key(i32),
    Arrival(u64),
}

/// What reaches the thread from outside besides input.
#[derive(Clone, Copy, Debug)]
enum Signal {
    /// A request: the wake event.
    Request,
    /// The window lost focus: the latch, then the wake event.
    FocusLoss,
    /// Stop: the flag, then the wake event.
    Stop,
}

/// The simulated OS around the loop.
struct Sim {
    clock: Clock,
    stop: Rc<AtomicBool>,
    latch: Option<FocusLatch>,
    /// Messages to come, by time.
    arrivals: VecDeque<(i64, Msg)>,
    /// Messages to come that a drain never sees: only a wait queues them,
    /// as if they arrived just after the drain's last read.
    late: VecDeque<(i64, Msg)>,
    /// When a wait that asks for no input returns for the message queue.
    queue_returns: VecDeque<i64>,
    /// Messages queued, with their arrival time.
    queue: VecDeque<(i64, Msg)>,
    signals: VecDeque<(i64, Signal)>,
    event: bool,
    requests: u32,
    /// How late the timer fires.
    timer_late: i64,
    /// µs each message takes to read.
    read_cost: i64,
    /// Every wait, with the time it began.
    waits: Vec<(i64, Wait)>,
    /// A wait fails once the clock reaches this.
    fail_at: Option<i64>,
}

impl Sim {
    fn deliver(&mut self) {
        let now = self.clock.get();
        while let Some(&(t, m)) = self.arrivals.front() {
            if t > now {
                break;
            }
            self.arrivals.pop_front();
            self.queue.push_back((t, m));
        }
        while let Some(&(t, s)) = self.signals.front() {
            if t > now {
                break;
            }
            self.signals.pop_front();
            match s {
                Signal::Request => self.requests += 1,
                Signal::FocusLoss => {
                    if let Some(l) = &self.latch {
                        l.latch();
                    }
                }
                Signal::Stop => self.stop.store(true, Ordering::Release),
            }
            self.event = true;
        }
    }
}

struct SimWaiter(Rc<RefCell<Sim>>);

impl Waiter for SimWaiter {
    fn wait(&mut self, wait: Wait) -> Result<Woken, WaitFailed> {
        let mut sim = self.0.borrow_mut();
        let now = sim.clock.get();
        sim.waits.push((now, wait));
        loop {
            sim.deliver();
            let now = sim.clock.get();
            if sim.fail_at.is_some_and(|t| now >= t) {
                return Err(WaitFailed);
            }
            if sim.event {
                sim.event = false;
                return Ok(Woken::Event);
            }
            // After the event: a wake for it drains before these arrive.
            while let Some(&(t, m)) = sim.late.front() {
                if t > now {
                    break;
                }
                sim.late.pop_front();
                sim.queue.push_back((t, m));
            }
            if !wait.input && sim.queue_returns.front().is_some_and(|&t| now >= t) {
                sim.queue_returns.pop_front();
                return Ok(Woken::Queue);
            }
            if wait.input && !sim.queue.is_empty() {
                return Ok(Woken::Input);
            }
            if wait.deadline.is_some_and(|d| now >= d.0) {
                return Ok(Woken::Timer);
            }
            let timer = wait.deadline.map(|d| d.0 + sim.timer_late);
            let arrival = [sim.arrivals.front(), sim.late.front()]
                .into_iter()
                .flatten()
                .map(|&(t, _)| t)
                .min()
                .filter(|_| wait.input);
            let signal = sim.signals.front().map(|&(t, _)| t);
            let queue = sim.queue_returns.front().copied().filter(|_| !wait.input);
            let other = arrival.into_iter().chain(signal).chain(queue).min();
            match (timer, other) {
                (Some(t), Some(o)) if o <= t => sim.clock.set(o.max(now)),
                // The deadline check above returns once the clock is there.
                (Some(t), _) => sim.clock.set(t.max(now)),
                (None, Some(o)) => sim.clock.set(o.max(now)),
                (None, None) => panic!("the wait at {now} would never return: {wait:?}"),
            }
        }
    }
}

/// What a wake saw.
#[derive(Clone, Copy, Debug)]
struct Seen {
    at: i64,
    published: i64,
    tripped: bool,
}

/// One run of the loop: its source set (recording open from 0 on the
/// keyboard), the simulation, and what every wake saw.
struct Run {
    sim: Rc<RefCell<Sim>>,
    report: WakeReport,
    seen: Vec<Seen>,
    /// Every key report read, in order: its value, arrival and stamp.
    events: Vec<Read>,
    sources: SourceSet,
    result: Result<(), WaitFailed>,
}

fn set() -> (SourceSet, SourceReader) {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 4,
        max_sources: 2,
        max_held: 4,
        queue_capacity: 8192,
        max_arrivals: 4,
    })
    .unwrap();
    set.connect(KEYBOARD, DeviceKind::Keyboard);
    set.set_recorded(KEYBOARD, vec![A]);
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();
    let reader = readers.pop().unwrap();
    (set, reader)
}

struct Setup {
    driven: Driven,
    arrivals: Vec<(i64, Msg)>,
    late: Vec<(i64, Msg)>,
    queue_returns: Vec<i64>,
    signals: Vec<(i64, Signal)>,
    timer_late: i64,
    read_cost: i64,
    fail_at: Option<i64>,
}

impl Setup {
    fn new(driven: Driven) -> Self {
        Self {
            driven,
            arrivals: Vec::new(),
            late: Vec::new(),
            queue_returns: Vec::new(),
            signals: Vec::new(),
            timer_late: 0,
            read_cost: 0,
            fail_at: None,
        }
    }

    fn stop_at(mut self, t: i64) -> Self {
        self.signals.push((t, Signal::Stop));
        self
    }

    /// Runs the loop with `phase` deciding each boundary's phase from the
    /// wake's time and the requests seen so far.
    fn run(mut self, mut phase: impl FnMut(i64, u32) -> Phase) -> Run {
        self.arrivals.sort_by_key(|&(t, _)| t);
        self.late.sort_by_key(|&(t, _)| t);
        self.signals.sort_by_key(|&(t, _)| t);
        let clock = Clock(Arc::new(AtomicI64::new(0)));
        let stop = Rc::new(AtomicBool::new(false));
        let (mut sources, reader) = set();
        let sim = Rc::new(RefCell::new(Sim {
            clock: clock.clone(),
            stop: Rc::clone(&stop),
            latch: Some(sources.gate().handle()),
            arrivals: self.arrivals.into(),
            late: self.late.into(),
            queue_returns: self.queue_returns.into(),
            queue: VecDeque::new(),
            signals: self.signals.into(),
            event: false,
            requests: 0,
            timer_late: self.timer_late,
            read_cost: self.read_cost,
            waits: Vec::new(),
            fail_at: self.fail_at,
        }));
        let mut waiter = SimWaiter(Rc::clone(&sim));
        let mut cadence = Cadence::new(self.driven, clock.now());
        assert_eq!(cadence.phase(), Phase::Tail);
        let mut report = WakeReport::new();
        let seen = RefCell::new(Vec::new());
        let events = RefCell::new(Vec::new());
        let reader = RefCell::new(reader);
        // The watermark published at the previous boundary.
        let last_watermark = Cell::new(i64::MIN);
        let driven = self.driven;
        let result = {
            let wake = |now: TimeUs, may_read: bool| {
                let (published, read) = if driven == Driven::Input {
                    let (published, n) =
                        cadence::batch(&clock, &mut sources, now, may_read, |sources, budget| {
                            let mut sim = sim.borrow_mut();
                            sim.deliver();
                            assert!(budget >= 1);
                            let Some((arrived, msg)) = sim.queue.pop_front() else {
                                return 0;
                            };
                            match msg {
                                Msg::Key(value) => {
                                    sources.packet(&Packet {
                                        device: KEYBOARD,
                                        control: A,
                                        value,
                                        background: false,
                                    });
                                    events.borrow_mut().push(Read {
                                        value,
                                        arrived,
                                        stamp: now.0,
                                    });
                                }
                                Msg::Arrival(d) => {
                                    sources.connect(DeviceId(d), DeviceKind::Keyboard)
                                }
                            }
                            let t = sim.clock.get() + sim.read_cost;
                            sim.clock.set(t);
                            1
                        });
                    (published, n)
                } else {
                    sources.begin_batch(now);
                    sources.end_batch(now);
                    (now, None)
                };
                // The watermark rule: what this batch pushed lies above
                // every watermark published before it.
                let mut reader = reader.borrow_mut();
                while let Some(e) = reader.pop() {
                    assert!(
                        e.received.0 > last_watermark.get(),
                        "{e:?} at or below {}",
                        last_watermark.get()
                    );
                }
                last_watermark.set(reader.published().received_up_to.0);
                let at = clock.now().0;
                let requests = sim.borrow().requests;
                seen.borrow_mut().push(Seen {
                    at,
                    published: published.0,
                    tripped: sources.gate().is_tripped(),
                });
                Boundary {
                    published,
                    read,
                    phase: phase(at, requests),
                }
            };
            cadence::run(&clock, &mut waiter, &stop, &mut cadence, &mut report, wake)
        };
        Run {
            sim,
            report,
            seen: seen.into_inner(),
            events: events.into_inner(),
            sources,
            result,
        }
    }
}

impl Run {
    fn waits(&self) -> Vec<(i64, Wait)> {
        self.sim.borrow().waits.clone()
    }

    /// The stamp of every key report read, in order.
    fn stamps(&self) -> Vec<i64> {
        self.events.iter().map(|r| r.stamp).collect()
    }
}

/// A key report as read.
#[derive(Clone, Copy, Debug)]
struct Read {
    value: i32,
    arrived: i64,
    stamp: i64,
}

fn keys(from: i64, every: i64, count: i32) -> Vec<(i64, Msg)> {
    (0..count)
        .map(|i| (from + i64::from(i) * every, Msg::Key(i + 1)))
        .collect()
}

/// While Open, every wait's heartbeat deadline lies at most 1 ms after the
/// last publication, also when the timer fires late, and the heartbeat
/// publishes even with no input.
#[test]
fn heartbeat_deadline_follows_last_publication() {
    for late in [0, 300, 1_700] {
        let mut setup = Setup::new(Driven::Input).stop_at(20_000);
        setup.timer_late = late;
        let run = setup.run(|_, _| Phase::Open);
        assert_eq!(run.result, Ok(()));
        let mut last = 0;
        let mut seen = run.seen.iter().peekable();
        for (at, wait) in run.waits() {
            while let Some(s) = seen.next_if(|s| s.at <= at) {
                last = s.published;
            }
            let deadline = wait.deadline.expect("Open keeps a heartbeat").0;
            assert!(
                deadline <= last + HEARTBEAT.0,
                "late {late}: {deadline} after {last}"
            );
        }
        let beats = run.report.causes.heartbeat;
        assert!(
            beats >= 20_000 / (1_000 + late as u64) - 1,
            "late {late}: {beats}"
        );
        assert_eq!(run.report.lateness.max(), late as u64);
        assert_eq!(run.report.causes.input, 0);
    }
}

/// Input wakes leave the heartbeat where it was: while input arrives every
/// 300 µs the deadlines stay on the 1 ms grid, and once it stops, the next
/// publication comes within 1 ms of the last.
#[test]
fn input_wakes_never_delay_the_heartbeat() {
    let mut setup = Setup::new(Driven::Input).stop_at(10_000);
    setup.arrivals = keys(100, 300, 17);
    let run = setup.run(|_, _| Phase::Open);
    assert!(run.report.causes.input >= 17, "{:?}", run.report.causes);
    // A wait for input waits for the heartbeat alone: on the grid.
    for (at, wait) in run.waits().into_iter().filter(|(_, w)| w.input) {
        assert_eq!(wait.deadline.unwrap().0 % HEARTBEAT.0, 0, "{at}: {wait:?}");
    }
    let published: Vec<i64> = run.seen.iter().map(|s| s.published).collect();
    for pair in published.windows(2) {
        assert!(pair[1] - pair[0] <= HEARTBEAT.0, "{pair:?}");
    }
    assert!(run.report.publications.max() <= HEARTBEAT.0 as u64);
    assert!(published.last().unwrap() >= &9_000);
}

/// Input faster than the bound (every 10 µs) wakes the thread at most once
/// per gap, never spins, and every report is recorded as its own event in
/// order, within one gap of its arrival.
#[test]
fn fast_input_waits_for_the_bound_and_keeps_every_report() {
    const N: i32 = 2_000;
    let mut setup = Setup::new(Driven::Input).stop_at(25_000);
    setup.arrivals = keys(0, 10, N);
    let run = setup.run(|_, _| Phase::Open);
    // Every report, in order, each its own event (the run checked each
    // against the watermark), within one gap of its arrival.
    assert_eq!(run.events.len(), N as usize);
    for (i, r) in run.events.iter().enumerate() {
        assert_eq!(r.value, i as i32 + 1);
        assert!(r.stamp >= r.arrived, "{r:?}");
        assert!(r.stamp - r.arrived <= INPUT_GAP.0, "{r:?}");
    }
    // Wakes for input are at least the gap apart: at most one per gap,
    // each with an early return before it, plus the heartbeats, each of
    // which can add one more early return.
    let input_wakes = run.report.causes.input + run.report.causes.drain;
    assert!(
        input_wakes <= (20_000 / INPUT_GAP.0 + 1) as u64,
        "{input_wakes}"
    );
    assert!(
        run.report.waits <= 2 * input_wakes + 2 * run.report.causes.heartbeat + 4,
        "{} waits",
        run.report.waits
    );
}

/// A backlog larger than the cut is read in batches of at most
/// [`DRAIN_CUT`]: each batch publishes and runs the hook, and the next
/// one takes a new stamp, at or after that publication.
#[test]
fn drain_is_cut_and_restamped() {
    let mut setup = Setup::new(Driven::Input).stop_at(20_000);
    // Queued before the first wait, and read at 1 µs each.
    setup.arrivals = keys(0, 0, 3_000);
    setup.read_cost = 1;
    let run = setup.run(|_, _| Phase::Open);
    assert_eq!(run.report.cuts, 2);
    // Each batch published at its last read and ran the hook there.
    let batches: Vec<(i64, i64)> = run
        .seen
        .iter()
        .take(3)
        .map(|s| (s.at, s.published))
        .collect();
    assert_eq!(batches, [(1_024, 1_024), (2_048, 2_048), (3_000, 3_000)]);
    // The first batch keeps its wake's stamp; each later one takes the
    // stamp of its own wake, the previous batch's publication.
    let stamps = run.stamps();
    assert_eq!(stamps.len(), 3_000);
    let cut = DRAIN_CUT as usize;
    assert!(stamps[..cut].iter().all(|&s| s == 0));
    assert!(stamps[cut..2 * cut].iter().all(|&s| s == 1_024));
    assert!(stamps[2 * cut..].iter().all(|&s| s == 2_048));
    // The rest of a cut drain is read at the gap's end, never by waiting
    // for input.
    assert_eq!(run.report.causes.input, 1);
    assert_eq!(run.report.causes.drain, 2);
    let after_cut: Vec<Wait> = run
        .waits()
        .into_iter()
        .filter(|&(at, _)| (1..3_000).contains(&at))
        .map(|(_, w)| w)
        .collect();
    assert!(after_cut.iter().all(|w| !w.input), "{after_cut:?}");
}

/// Stop is seen within one cut drain under sustained input: the loop ends
/// after the batch it is in, with the rest unread.
#[test]
fn stop_ends_a_sustained_drain_within_one_cut() {
    let mut setup = Setup::new(Driven::Input).stop_at(500);
    setup.arrivals = keys(0, 0, 5_000);
    setup.read_cost = 1;
    let run = setup.run(|_, _| Phase::Open);
    assert_eq!(run.result, Ok(()));
    assert_eq!(run.seen.len(), 1);
    assert_eq!(run.stamps().len(), DRAIN_CUT as usize);
}

/// A batch after a cut in the microsecond of the cut's publication keeps
/// the watermark rule: its events lie above the published watermark.
#[test]
fn same_us_batch_keeps_the_watermark_rule() {
    let mut setup = Setup::new(Driven::Input).stop_at(5_000);
    // A request at 500 finds the queue empty and publishes from 500; a key
    // arrives right after, in the same microsecond, and is read at once.
    setup.signals.push((500, Signal::Request));
    setup.late = keys(500, 0, 1);
    let run = setup.run(|_, _| Phase::Open);
    let wakes: Vec<(i64, i64)> = run
        .seen
        .iter()
        .take(2)
        .map(|s| (s.at, s.published))
        .collect();
    assert_eq!(wakes, [(500, 500), (500, 500)]);
    // The run's own check: the event (500) lay above the watermark
    // published before it (499).
    assert_eq!(run.stamps(), [500]);
}

/// A heartbeat within the gap after a read publishes without reading:
/// input held for the gap is read at its end, not at the heartbeat.
#[test]
fn a_heartbeat_inside_the_gap_publishes_without_reading() {
    let mut setup = Setup::new(Driven::Input).stop_at(3_000);
    setup.arrivals = vec![(990, Msg::Key(1)), (995, Msg::Key(2))];
    let run = setup.run(|_, _| Phase::Open);
    assert_eq!(run.stamps(), [990, 990 + INPUT_GAP.0]);
    assert!(
        run.seen
            .iter()
            .any(|s| s.at == 1_000 && s.published == 1_000),
        "{:?}",
        run.seen
    );
    assert_eq!(run.report.causes.heartbeat, 2);
    // The heartbeat kept the input held: the one early return for it at
    // 995, none after the heartbeat.
    assert_eq!(run.report.rearms, 1);
}

/// A batch told not to read reads nothing and reports no count, which
/// keeps held or left input so; told to read, it reports what it read,
/// 0 included.
#[test]
fn a_batch_that_may_not_read_reports_no_count() {
    let clock = Clock(Arc::new(AtomicI64::new(7)));
    let (mut sources, _reader) = set();
    let mut calls = 0;
    let skipped = cadence::batch(&clock, &mut sources, TimeUs(7), false, |_, _| {
        calls += 1;
        1
    });
    assert_eq!((skipped, calls), ((TimeUs(7), None), 0));
    let empty = cadence::batch(&clock, &mut sources, TimeUs(7), true, |_, _| 0);
    assert_eq!(empty, (TimeUs(7), Some(0)));
}

/// A wake whose hook leaves for Idle still samples the deadline its
/// publication reached: a request at 1,050 after a deadline at 1,000.
#[test]
fn entering_idle_keeps_the_last_lateness_sample() {
    let mut setup = Setup::new(Driven::Input).stop_at(2_000);
    setup.timer_late = 300;
    setup.signals.push((1_050, Signal::Request));
    let run = setup.run(|_, requests| {
        if requests > 0 {
            Phase::Idle
        } else {
            Phase::Tail
        }
    });
    assert_eq!(run.report.causes.heartbeat, 0);
    assert_eq!(run.report.lateness.count(), 1);
    assert_eq!(run.report.lateness.max(), 50);
}

/// A drain the wake event started and the cut stopped waits for the gap
/// like any read; a request within the gap publishes without reading.
#[test]
fn an_event_cut_drain_waits_for_the_gap() {
    let mut setup = Setup::new(Driven::Input).stop_at(900);
    setup.arrivals = keys(10, 0, 2_500);
    setup.signals.push((10, Signal::Request));
    setup.signals.push((50, Signal::Request));
    let run = setup.run(|_, _| Phase::Open);
    let stamps = run.stamps();
    let cut = DRAIN_CUT as usize;
    assert_eq!(stamps.len(), 2_500);
    assert!(stamps[..cut].iter().all(|&s| s == 10));
    assert!(stamps[cut..2 * cut].iter().all(|&s| s == 10 + INPUT_GAP.0));
    assert!(stamps[2 * cut..].iter().all(|&s| s == 10 + 2 * INPUT_GAP.0));
    assert!(run.seen.iter().any(|s| s.at == 50), "{:?}", run.seen);
    assert_eq!(run.report.causes.event, 2);
}

/// A wait for the timer alone that returns for the message queue is no
/// failure: the loop waits again, and held input is read at the gap's end.
#[test]
fn a_queue_return_during_a_timer_only_wait_is_not_a_failure() {
    // The OS result for the queue maps to a queue return, not a failure,
    // when no input was asked for.
    assert_eq!(cadence::classify(1, 1, false), Ok(Woken::Queue));
    assert_eq!(cadence::classify(2, 2, false), Ok(Woken::Queue));
    assert_eq!(cadence::classify(2, 2, true), Ok(Woken::Input));
    assert_eq!(cadence::classify(1, 2, false), Ok(Woken::Timer));
    assert_eq!(cadence::classify(0, 2, false), Ok(Woken::Event));
    assert_eq!(cadence::classify(u32::MAX, 2, false), Err(WaitFailed));
    assert_eq!(cadence::classify(3, 2, true), Err(WaitFailed));
    let mut setup = Setup::new(Driven::Input).stop_at(900);
    setup.arrivals = vec![(100, Msg::Key(1)), (110, Msg::Key(2))];
    setup.queue_returns = vec![150, 160];
    let run = setup.run(|_, _| Phase::Open);
    assert_eq!(run.result, Ok(()));
    assert_eq!(run.stamps(), [100, 100 + INPUT_GAP.0]);
    assert!(run.report.rearms >= 3, "{}", run.report.rearms);
}

/// Lateness is sampled whenever a publication reaches the heartbeat's
/// deadline, whatever woke the thread: here an input wake 50 µs after it,
/// before the late timer fired.
#[test]
fn lateness_is_sampled_whatever_reaches_the_deadline() {
    let mut setup = Setup::new(Driven::Input).stop_at(1_200);
    setup.timer_late = 300;
    setup.arrivals = keys(1_050, 0, 1);
    let run = setup.run(|_, _| Phase::Open);
    assert_eq!(run.report.causes.heartbeat, 0);
    assert_eq!(run.report.causes.input, 1);
    assert_eq!(run.report.lateness.count(), 1);
    assert_eq!(run.report.lateness.max(), 50);
}

/// Arrivals during a drain are read in it; one right after it waits for
/// the gap's end, then is read without the timer.
#[test]
fn arrivals_during_and_right_after_a_drain() {
    let mut setup = Setup::new(Driven::Input).stop_at(900);
    setup.arrivals = keys(100, 1, 20);
    setup.arrivals.push((121, Msg::Key(21)));
    setup.read_cost = 1;
    let run = setup.run(|_, _| Phase::Open);
    // The first wake at 100 read the 20 arriving while it drained.
    assert_eq!(run.seen[0].at, 120);
    // The one at 121 waited for the gap's end at 225.
    assert_eq!(run.seen[1].at, 100 + INPUT_GAP.0 + 1);
    assert_eq!(run.stamps().len(), 21);
    assert_eq!(run.report.causes.heartbeat, 0);
}

/// While Idle, a device change with no input wakes the thread like input,
/// with no timer, and the device is connected.
#[test]
fn device_change_wakes_like_input() {
    let mut setup = Setup::new(Driven::Input).stop_at(8_000);
    setup.arrivals.push((5_000, Msg::Arrival(7)));
    let run = setup.run(|_, _| Phase::Idle);
    assert!(run.seen.iter().any(|s| s.at == 5_000), "{:?}", run.seen);
    assert_eq!(run.report.causes.input, 1);
    assert!(run.sources.devices().any(|(d, _)| d == DeviceId(7)));
    // No timer while Idle: one wait for the change, one after it.
    let waits = run.waits();
    assert_eq!(waits.len(), 3);
    for (at, wait) in &waits[1..] {
        assert_eq!(
            *wait,
            Wait {
                deadline: None,
                input: true
            },
            "{at}"
        );
    }
}

/// While Idle, a focus loss with no input wakes the thread through the
/// wake event, and that wake sees the latch.
#[test]
fn focus_change_wakes_an_idle_thread() {
    let mut setup = Setup::new(Driven::Input).stop_at(8_000);
    setup.signals.push((5_000, Signal::FocusLoss));
    let run = setup.run(|_, _| Phase::Idle);
    let at = run.seen.iter().find(|s| s.tripped).map(|s| s.at);
    assert_eq!(at, Some(5_000), "{:?}", run.seen);
    assert_eq!(run.report.causes.event, 1);
    assert_eq!(run.report.causes.heartbeat, 1);
}

/// The thread starts in Tail and keeps its heartbeat through 50 ms
/// without input; it goes Idle only when the hook says so on a request,
/// and then waits with no timer.
#[test]
fn idle_starts_only_on_request() {
    let mut setup = Setup::new(Driven::Input).stop_at(80_000);
    setup.signals.push((50_000, Signal::Request));
    let run = setup.run(|_, requests| {
        if requests > 0 {
            Phase::Idle
        } else {
            Phase::Tail
        }
    });
    for (at, wait) in run.waits() {
        if at < 50_000 {
            assert!(wait.deadline.is_some(), "{at}");
        } else {
            assert_eq!(wait.deadline, None, "{at}");
        }
    }
    assert_eq!(run.report.causes.heartbeat, 49);
    assert_eq!(run.report.causes.event, 1);
}

/// The autoplay thread never waits for input: timer-driven while Open and
/// Tail, the wake event alone while Idle, where a focus loss still wakes it.
#[test]
fn autoplay_stays_timer_driven_until_idle() {
    let mut setup = Setup::new(Driven::Timer).stop_at(40_000);
    setup.arrivals = keys(0, 50, 400);
    setup.signals.push((20_000, Signal::Request));
    setup.signals.push((30_000, Signal::FocusLoss));
    let run = setup.run(|at, requests| match (at, requests) {
        (_, 1..) => Phase::Idle,
        (..10_000, _) => Phase::Open,
        _ => Phase::Tail,
    });
    for (at, wait) in run.waits() {
        assert!(!wait.input, "{at}");
        assert_eq!(wait.deadline.is_some(), at < 20_000, "{at}");
    }
    assert_eq!(run.report.causes.input, 0);
    assert_eq!(run.report.causes.heartbeat, 19);
    assert_eq!(
        run.seen.iter().find(|s| s.tripped).map(|s| s.at),
        Some(30_000)
    );
    assert!(run.stamps().is_empty());
}

/// A wait that fails ends the loop with the failure, after the wakes
/// before it.
#[test]
fn a_failed_wait_ends_the_loop() {
    let mut setup = Setup::new(Driven::Input).stop_at(50_000);
    setup.fail_at = Some(3_500);
    let run = setup.run(|_, _| Phase::Open);
    assert_eq!(run.result, Err(WaitFailed));
    assert_eq!(run.seen.len(), 3);
}
