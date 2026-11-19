//! The device's logic against a simulated controller and host: the host
//! polls each endpoint at the report interval and receives a report whose
//! AVAILABLE is set unless the endpoint is aborting. It accepts a report
//! whose DATA PID is the one it expects and discards one that repeats the
//! last it accepted, as a USB host does with a duplicate, and ACKs both;
//! the controller clears AVAILABLE when the ACK reaches it, so a lost ACK
//! leaves a report the host accepted AVAILABLE. The writers follow the
//! firmware's loops: ask for the next report once a completion ends the
//! last, then again only when kicked or at the time the logic named, commit
//! it at the clock's reading and write it as embassy-rp does (the opposite
//! PID of the register, then AVAILABLE), then wait for its completion. The
//! link's bytes go through [`Link`]; the link's control
//! requests and the bus's events (a reset, the host configuring the device
//! or an interface again, which rewrites the endpoints' buffer controls as
//! embassy-rp does, a suspend) go to the logic at their times, as the
//! firmware's USB handler passes them on, after the host's poll in the same
//! step and before the writers look. The lease's timer runs every 0.5 ms
//! unless a test stalls it; a test may add the watchdog and its feeder.

use std::cell::{Cell, RefCell};

use super::*;
use crate::protocol::{Answer, Malformed};
use crate::revoke::{REARM_AFTER_PROBE, Seen, judge};
use crate::trace::{Check, Trace};
use crate::wrap::SeqUnwrap;

/// The simulation's time step, µs.
const STEP: u64 = 50;
/// The boot's TRNG draw in the simulated device.
const DRAW: u64 = 0x0123_4567_89ab_cdef;

const fn index(ep: Ep) -> usize {
    match ep {
        Ep::Keyboard => 0,
        Ep::Pad => 1,
    }
}

#[derive(Clone, Debug, Default)]
struct SimBuffer {
    control: Buffer,
    data: Vec<u8>,
}

/// The controller and the clock.
struct Sim {
    t: Cell<u64>,
    buffers: RefCell<[SimBuffer; 2]>,
    abort: Cell<[bool; 2]>,
    done: Cell<[bool; 2]>,
    /// Whether the controller acknowledges an abort.
    acknowledges: Cell<bool>,
    /// Whether it acknowledges the abort of the link's IN buffer.
    link_acknowledges: Cell<bool>,
    /// Buffer control writes by the logic.
    writes: Cell<u32>,
    /// What the logic did to the controller and the tasks, in order.
    actions: RefCell<Vec<Action>>,
    /// µs the clock advances after each read.
    tick: Cell<u64>,
    /// Each endpoint's writer was kicked and has not woken since.
    kicked: Cell<[bool; 2]>,
}

impl Sim {
    fn new() -> Self {
        // The register holds DATA1 at configuration, so the first write
        // sends DATA0.
        let start = SimBuffer {
            control: Buffer {
                available: false,
                pid: true,
                length: 0,
            },
            data: Vec::new(),
        };
        Self {
            t: Cell::new(0),
            buffers: RefCell::new([start.clone(), start]),
            abort: Cell::new([false; 2]),
            done: Cell::new([false; 2]),
            acknowledges: Cell::new(true),
            link_acknowledges: Cell::new(true),
            writes: Cell::new(0),
            actions: RefCell::new(Vec::new()),
            tick: Cell::new(0),
            kicked: Cell::new([false; 2]),
        }
    }
}

/// One thing the logic did through [`Hardware`] besides reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    SetBuffer(Ep),
    SetAbort(Ep, bool),
    ClearAbortDone(Ep),
    Kick(Ep),
    KickTrace,
    RetireLinkOutput,
}

impl Sim {
    fn act(&self, action: Action) {
        self.actions.borrow_mut().push(action);
    }
}

fn set(cell: &Cell<[bool; 2]>, ep: Ep, on: bool) {
    let mut v = cell.get();
    v[index(ep)] = on;
    cell.set(v);
}

impl Hardware for Sim {
    fn now(&self) -> u64 {
        let t = self.t.get();
        self.t.set(t + self.tick.get());
        t
    }

    fn buffer(&self, ep: Ep) -> Buffer {
        self.buffers.borrow()[index(ep)].control
    }

    fn set_buffer(&self, ep: Ep, buffer: Buffer) {
        self.act(Action::SetBuffer(ep));
        self.writes.set(self.writes.get() + 1);
        self.buffers.borrow_mut()[index(ep)].control = buffer;
    }

    fn set_abort(&self, ep: Ep, on: bool) {
        self.act(Action::SetAbort(ep, on));
        set(&self.abort, ep, on);
        if on && self.acknowledges.get() {
            set(&self.done, ep, true);
        }
    }

    fn abort_done(&self, ep: Ep) -> bool {
        self.done.get()[index(ep)]
    }

    fn clear_abort_done(&self, ep: Ep) {
        self.act(Action::ClearAbortDone(ep));
        set(&self.done, ep, false);
    }

    fn kick(&self, ep: Ep) {
        self.act(Action::Kick(ep));
        set(&self.kicked, ep, true);
    }

    fn kick_trace(&self) {
        self.act(Action::KickTrace);
    }

    fn retire_link_output(&self) -> bool {
        self.act(Action::RetireLinkOutput);
        self.link_acknowledges.get()
    }
}

/// A report the host accepted.
#[derive(Clone, Debug)]
struct Accepted {
    ep: Ep,
    at: u64,
    data: Vec<u8>,
}

/// What the bus brings besides the link's bytes.
#[derive(Clone, Copy, Debug)]
enum Event {
    /// `SET_CONTROL_LINE_STATE` with DTR set or clear (RTS set).
    Dtr(bool),
    /// `SET_LINE_CODING` at this rate, 8N1.
    Baud(u32),
    /// A bus reset: the endpoints disabled, their buffers left as they
    /// are, and each interface back at alternate setting 0.
    Reset,
    /// The link's reader fails: a disconnect.
    Lost,
    /// `SET_CONFIGURATION 1`: each endpoint enabled and reinitialised.
    Configure,
    /// `SET_CONFIGURATION 0`: each endpoint disabled and reinitialised.
    Unconfigure,
    /// `SET_INTERFACE` on the endpoint's interface: it is reinitialised.
    SetInterface(Ep),
    /// The bus suspends (true: the host stops polling, the device
    /// disarms) or resumes (false: polling goes on).
    Suspend(bool),
}

/// What an endpoint's writer waits for before it asks for the next report
/// (the firmware's `wait_for_send`).
#[derive(Clone, Copy, Debug)]
enum Wait {
    /// Nothing: it asks at its next turn.
    Ask,
    /// A kick, or this time.
    At(u64),
    /// A kick.
    Idle,
}

/// The watchdog and its feeder, as the firmware runs them: the feeder
/// wakes every [`FEED_EVERY_US`] and feeds only when the logic says so
/// ([`State::feed_due`]); the watchdog resets the device [`WATCHDOG_US`]
/// after the last feed.
#[derive(Clone, Copy, Debug)]
struct Watchdog {
    fed: u64,
    /// When the watchdog reset the device.
    reset: Option<u64>,
}

struct World {
    hw: Sim,
    state: Box<State>,
    link: Link,
    /// Bus events due at their times.
    events: Vec<(u64, Event)>,
    /// Whether each endpoint is enabled: the host polls only those.
    enabled: [bool; 2],
    /// Whether the bus is suspended: the host polls nothing.
    suspended: bool,
    /// Whether the lease's own timer runs.
    timer: bool,
    /// Each endpoint's writer waits for a completion.
    writing: [bool; 2],
    /// What each endpoint's writer waits for when it is not writing.
    waiting: [Wait; 2],
    watchdog: Option<Watchdog>,
    /// The host's first poll and its interval.
    phase: u64,
    interval: u64,
    /// The DATA PID the host expects next on each endpoint.
    expect: [bool; 2],
    accepted: Vec<Accepted>,
    discarded: usize,
    /// ACKs to lose on each endpoint, from its next poll.
    lose_ack: [u32; 2],
    records: Vec<Record>,
}

impl World {
    /// A configured device at `rate`, both endpoints identified, polled by
    /// a host at that rate from `phase`.
    fn new(rate: Rate, phase: u64) -> Self {
        Self::with_draw(rate, phase, Some(DRAW))
    }

    /// As [`World::new`], the boot's TRNG draw `draw`.
    fn with_draw(rate: Rate, phase: u64, draw: Option<u64>) -> Self {
        let mut state = Box::new(State::new());
        state.set_boot(
            Mode {
                rate,
                reporting: Reporting::OnChange,
            },
            Cause::PowerOn,
            0,
        );
        state.set_draw(draw);
        let mut w = Self {
            hw: Sim::new(),
            state,
            link: Link::new(),
            events: Vec::new(),
            enabled: [false; 2],
            suspended: false,
            timer: true,
            writing: [false; 2],
            waiting: [Wait::Ask; 2],
            watchdog: None,
            phase,
            interval: u64::from(rate.interval_us()),
            expect: [false; 2],
            accepted: Vec::new(),
            discarded: 0,
            lose_ack: [0; 2],
            records: Vec::new(),
        };
        w.event(Event::Configure);
        w.run_for(2 * w.interval + phase);
        assert!(w.state.identified());
        w
    }

    fn now(&self) -> u64 {
        self.hw.t.get()
    }

    fn poll(&mut self, ep: Ep) {
        let i = index(ep);
        if self.suspended || !self.enabled[i] || self.hw.abort.get()[i] {
            return;
        }
        let mut buffers = self.hw.buffers.borrow_mut();
        let b = &mut buffers[i];
        if !b.control.available {
            return;
        }
        if b.control.pid == self.expect[i] {
            self.expect[i] = !self.expect[i];
            self.accepted.push(Accepted {
                ep,
                at: self.hw.t.get(),
                data: b.data.clone(),
            });
        } else {
            self.discarded += 1;
        }
        // The host ACKs; the report stays AVAILABLE when the ACK is lost.
        if self.lose_ack[i] > 0 {
            self.lose_ack[i] -= 1;
        } else {
            b.control.available = false;
        }
    }

    /// A bus event now, as the firmware's USB handler and reader take it.
    fn event(&mut self, event: Event) {
        match event {
            Event::Dtr(on) => {
                let value = u16::from(on) | 2;
                self.state
                    .cdc_request(&self.hw, SET_CONTROL_LINE_STATE, value, &[]);
            }
            Event::Baud(rate) => {
                let mut coding = [0, 0, 0, 0, 0, 0, 8];
                coding[..4].copy_from_slice(&rate.to_le_bytes());
                self.state
                    .cdc_request(&self.hw, SET_LINE_CODING, 0, &coding);
            }
            Event::Reset => {
                self.enabled = [false; 2];
                self.state.bus_reset(&self.hw);
                for ep in [Ep::Keyboard, Ep::Pad] {
                    self.state.interface_set(&self.hw, ep);
                }
            }
            Event::Lost => self.state.read_failed(&self.hw),
            Event::Configure => {
                self.enabled = [true; 2];
                self.reinitialise(Ep::Keyboard);
                self.reinitialise(Ep::Pad);
                self.state.configured(&self.hw);
            }
            Event::Unconfigure => {
                self.enabled = [false; 2];
                self.reinitialise(Ep::Keyboard);
                self.reinitialise(Ep::Pad);
                self.state.unconfigured(&self.hw);
            }
            Event::SetInterface(ep) => {
                self.reinitialise(ep);
                self.state.interface_set(&self.hw, ep);
            }
            Event::Suspend(on) => {
                self.suspended = on;
                if on {
                    self.state.disarm(&self.hw, Reason::Suspended);
                }
            }
        }
    }

    /// embassy-rp's `endpoint_set_enabled`: the buffer control rewritten
    /// to AVAILABLE clear and PID DATA1, so the next write sends DATA0,
    /// where the host restarts the endpoint's toggle.
    fn reinitialise(&mut self, ep: Ep) {
        let i = index(ep);
        self.hw.buffers.borrow_mut()[i].control = Buffer {
            available: false,
            pid: true,
            length: 0,
        };
        self.expect[i] = false;
    }

    fn write(&mut self, ep: Ep) {
        let i = index(ep);
        if self.writing[i] {
            if !self.state.poll_completion(&self.hw, ep) {
                return;
            }
            self.writing[i] = false;
            self.waiting[i] = Wait::Ask;
        }
        let kicked = self.hw.kicked.get()[i];
        let ask = match self.waiting[i] {
            Wait::Ask => true,
            Wait::At(at) => kicked || self.now() >= at,
            Wait::Idle => kicked,
        };
        if !ask {
            return;
        }
        if !matches!(self.waiting[i], Wait::Ask) {
            // The wait takes the kick that woke it.
            set(&self.hw.kicked, ep, false);
        }
        let kind = match self.state.next_report(&self.hw, ep) {
            Next::Now(kind) => kind,
            Next::At(at) => {
                self.waiting[i] = Wait::At(at);
                return;
            }
            Next::Idle => {
                self.waiting[i] = Wait::Idle;
                return;
            }
        };
        let data = self.commit(ep, kind);
        self.hand(ep, data);
    }

    /// `ep`'s report of `kind`, committed at the clock's reading now.
    fn commit(&mut self, ep: Ep, kind: Kind) -> Vec<u8> {
        let now = self.hw.now();
        self.state
            .commit(&self.hw, ep, kind, now)
            .as_bytes()
            .to_vec()
    }

    /// Writes `data` to `ep`'s buffer as embassy-rp does, and the logic
    /// hears it was handed.
    fn hand(&mut self, ep: Ep, data: Vec<u8>) {
        let i = index(ep);
        {
            let mut buffers = self.hw.buffers.borrow_mut();
            let b = &mut buffers[i];
            assert!(!b.control.available, "written over a pending report");
            b.control = Buffer {
                available: true,
                pid: !b.control.pid,
                length: u16::try_from(data.len()).unwrap(),
            };
            b.data = data;
        }
        self.state.handed(&self.hw, ep);
        self.writing[i] = true;
    }

    fn drain(&mut self) {
        while let Some((_, r)) = self.state.next_record() {
            self.records.push(r);
        }
    }

    fn step(&mut self) {
        let t = self.now();
        if t >= self.phase && (t - self.phase).is_multiple_of(self.interval) {
            self.poll(Ep::Keyboard);
            self.poll(Ep::Pad);
        }
        while let Some(i) = self.events.iter().position(|&(at, _)| at <= t) {
            let (_, event) = self.events.remove(i);
            self.event(event);
        }
        self.write(Ep::Keyboard);
        self.write(Ep::Pad);
        if self.timer && t.is_multiple_of(500) {
            self.state.timer(&self.hw);
        }
        if let Some(dog) = &mut self.watchdog
            && dog.reset.is_none()
        {
            if t.is_multiple_of(FEED_EVERY_US) && self.state.feed_due() {
                dog.fed = t;
            }
            if t - dog.fed >= WATCHDOG_US {
                dog.reset = Some(t);
            }
        }
        self.drain();
        self.hw.t.set(t + STEP);
    }

    fn run_for(&mut self, us: u64) {
        let end = self.now() + us;
        while self.now() < end {
            self.step();
        }
    }

    fn run_until(&mut self, t: u64) {
        while self.now() < t {
            self.step();
        }
    }

    /// Bytes from the link, in one packet, through the receive loop: no
    /// step between the commands they hold, except while `flush` waits,
    /// which it stops doing when its connection ends (the firmware
    /// abandons it). The answers.
    fn receive(&mut self, packet: &[u8]) -> Vec<String> {
        let mut answers = Vec::new();
        let mut at = 0;
        while let Some(outcome) = self.link.next(&mut self.state, &self.hw, packet, &mut at) {
            match outcome {
                Outcome::Answer(a) => answers.push(a.to_string()),
                Outcome::Hello => {
                    answers.push(Answer::Hello(self.state.hello("sim")).to_string());
                }
                Outcome::Flush => {
                    let connection = self.link.connection();
                    let deadline = self.now() + 2 * self.interval + 10_000;
                    while !self.state.flush_poll(&self.hw)
                        && self.now() < deadline
                        && self.state.is_current(connection)
                    {
                        self.step();
                    }
                    self.state.flush_end(&self.hw, connection);
                    self.drain();
                }
                // Held until the device reboots.
                Outcome::Hold => break,
                other => answers.push(format!("{other:?}")),
            }
        }
        answers
    }

    /// One command line: its answer.
    fn send(&mut self, line: &str) -> String {
        let answers = self.receive(format!("{line}\n").as_bytes());
        assert_eq!(answers.len(), 1, "{line}: {answers:?}");
        answers.into_iter().next().unwrap()
    }

    /// The current connection's token.
    fn token(&self) -> Token {
        self.state.token().expect("the boot drew a token")
    }

    /// A device time `us` from now, as the device reports it.
    fn device_in(&self, us: u64) -> u64 {
        self.state.out(self.now() + us)
    }

    /// A lease of `ms` ending at device time `deadline`: its id.
    fn lease_until(&mut self, ms: u32, deadline: u64) -> LeaseId {
        let a = self.send(&format!("lease {} {ms} {deadline}", self.token()));
        match Answer::parse_canonical(&a) {
            Some(Answer::Leased { id, .. }) => id,
            _ => panic!("lease: {a}"),
        }
    }

    /// A lease of 30 s from now: its id.
    fn lease(&mut self) -> LeaseId {
        let deadline = self.device_in(30_000_000);
        self.lease_until(30_000, deadline)
    }

    /// The `hello` answer.
    fn hello(&mut self) -> Hello<'static> {
        let line = self.send("hello");
        match Answer::parse_canonical(&line) {
            Some(Answer::Hello(h)) => Hello {
                version: h.version,
                build: "sim",
                mode: h.mode,
                lease: h.lease,
                armed: h.armed,
                reason: h.reason,
                boot: h.boot,
                count: h.count,
                identified: h.identified,
                faults: h.faults,
                quiet: h.quiet,
                session: h.session,
            },
            _ => panic!("hello: {line}"),
        }
    }

    /// The flushed trace.
    fn trace(&mut self) -> Trace {
        assert_eq!(self.receive(b"flush\n"), Vec::<String>::new());
        let mut check = Check::default();
        for &r in &self.records {
            check.feed(r);
        }
        check.finish().expect("the trace")
    }

    /// The listener's changes on `ep`: each accepted report against the
    /// one before it.
    fn seen(&self, ep: Ep) -> Vec<Seen> {
        let mut before = vec![0; report_len(ep)];
        let mut seen = Vec::new();
        for a in self.accepted.iter().filter(|a| a.ep == ep) {
            for (control, down) in crate::revoke::edges(ep, &before, &a.data) {
                seen.push(Seen {
                    at: a.at,
                    control,
                    down,
                });
            }
            before.clone_from(&a.data);
        }
        seen
    }

    /// The listener's changes on `ep` from time `from`.
    fn seen_from(&self, ep: Ep, from: u64) -> Vec<(Control, bool)> {
        self.seen(ep)
            .into_iter()
            .filter(|s| s.at >= from)
            .map(|s| (s.control, s.down))
            .collect()
    }

    /// The pad reports the host accepted from time `from`, decoded.
    fn pad_from(&self, from: u64) -> Vec<(u64, PadReport)> {
        self.accepted
            .iter()
            .filter(|a| a.ep == Ep::Pad && a.at >= from)
            .map(|a| (a.at, PadReport::decode(&a.data).unwrap()))
            .collect()
    }

    /// Checks every accepted pad report against the trace.
    fn check_pad_reports(&self, trace: &Trace) {
        let mut unwrap = SeqUnwrap::default();
        for a in self.accepted.iter().filter(|a| a.ep == Ep::Pad) {
            let report = PadReport::decode(&a.data).unwrap();
            let seq = u32::try_from(unwrap.next(report.seq).unwrap()).unwrap();
            if trace.reports.contains_key(&(Ep::Pad, seq)) {
                assert_eq!(trace.check_pad_report(seq, &report), Ok(()), "{report:?}");
            }
        }
    }
}

const F24: Control = Control::Key(0x73);
const B1: Control = Control::Button(1);
const B2: Control = Control::Button(2);
const B3: Control = Control::Button(3);

/// After a disarm, new input waits until the host has taken the neutral
/// report: one packet that releases, leases again and arms an item
/// pressing the same key still shows the key going up, then down.
#[test]
fn new_input_waits_for_the_neutral_report_a_disarm_owes() {
    let mut w = World::new(Rate::Hz1000, 300);
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.run_for(3_000);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), [(F24, true)]);
    let from = w.now();
    let token = w.token();
    let second = LeaseId { token, n: 2 };
    let packet = format!(
        "release\nlease {token} 1000 {}\nitem 0 k115+\narm {second} 0\n",
        w.device_in(1_000_000)
    );
    let answers = w.receive(packet.as_bytes());
    assert_eq!(answers[0], "ok");
    assert!(
        answers[1].starts_with(&format!("lease {second} ")),
        "{answers:?}"
    );
    assert_eq!(answers[2..], ["ok", "ok"]);
    w.run_for(5_000);
    assert_eq!(
        w.seen_from(Ep::Keyboard, from),
        [(F24, false), (F24, true)],
        "the neutral report first"
    );
}

/// An owed neutral report revoked in its turn, before the host took it,
/// is sent again: it is owed until it is taken.
#[test]
fn an_owed_neutral_report_revoked_in_turn_is_sent_again() {
    let mut w = World::new(Rate::Hz125, 3_000);
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.run_for(10_000);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), [(F24, true)]);
    // Between polls: the neutral report is handed, then revoked by the
    // next disarm before any poll.
    assert_eq!(w.send("release"), "ok");
    w.run_for(1_000);
    assert!(
        w.hw.buffer(Ep::Keyboard).available,
        "the neutral report waits"
    );
    assert_eq!(w.send("release"), "ok");
    w.run_for(20_000);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), [(F24, true), (F24, false)]);
    assert_eq!(w.discarded, 0);
}

/// A pressed report the host accepted whose ACK was lost is still
/// AVAILABLE at the disarm, so it is withdrawn and the host expects either
/// DATA PID: the release still reaches the host, and new input after it.
#[test]
fn a_release_reaches_the_host_after_a_lost_ack() {
    let mut w = World::new(Rate::Hz125, 3_000);
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.lose_ack[index(Ep::Keyboard)] = 1;
    w.run_for(10_000);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), [(F24, true)]);
    assert!(w.hw.buffer(Ep::Keyboard).available, "the ACK was lost");
    assert_eq!(w.send("release"), "ok");
    w.run_for(30_000);
    assert_eq!(
        w.seen_from(Ep::Keyboard, 0),
        [(F24, true), (F24, false)],
        "the release"
    );
    assert_eq!(w.discarded, 1, "the first neutral report repeats the PID");
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 4 1")), "ok");
    w.run_for(10_000);
    assert_eq!(
        w.seen_from(Ep::Keyboard, 0),
        [(F24, true), (F24, false), (Control::Key(4), true)]
    );
}

/// The release property's host-visible part after a lost ACK: the host
/// has the release within two of the endpoint's polls after the cutoff,
/// the second owed neutral report when it dropped the first as a repeat.
#[test]
fn a_lost_ack_releases_within_two_polls() {
    let mut w = World::new(Rate::Hz125, 3_000);
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.lose_ack[index(Ep::Keyboard)] = 1;
    w.run_for(10_000);
    assert!(w.hw.buffer(Ep::Keyboard).available, "the ACK was lost");
    let cutoff = w.now();
    assert_eq!(w.send("release"), "ok");
    w.run_for(4 * w.interval);
    let release = w
        .seen(Ep::Keyboard)
        .into_iter()
        .find(|s| s.control == F24 && !s.down)
        .expect("the release reached the host");
    let polls_after = |t: u64| (t - w.phase) / w.interval;
    let polls = polls_after(release.at) - polls_after(cutoff);
    assert!(
        (1..=2).contains(&polls),
        "the release at the {polls}th poll after the cutoff"
    );
    assert_eq!(w.discarded, 1, "the first neutral report was a repeat");
}

/// A neutral report the USB stack drops when the host configures the
/// device or the keyboard's interface again was never taken: after a
/// release whose first neutral report the host discarded (its toggle was
/// unknown after a lost ACK), the second, still queued, is dropped, and the
/// release still reaches the host before the next press. An unconfiguration
/// also ends the connection and its lease, so its press is held under a
/// lease of the next connection.
#[test]
fn a_reconfiguration_keeps_the_release_owed() {
    for reinit in [
        &[Event::Configure][..],
        &[Event::SetInterface(Ep::Keyboard)],
        &[Event::Unconfigure, Event::Configure],
    ] {
        let mut w = World::new(Rate::Hz125, 3_000);
        let ep = Ep::Keyboard;
        let id = w.lease();
        assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
        w.lose_ack[index(ep)] = 1;
        w.run_for(10_000);
        assert_eq!(w.send("release"), "ok");
        w.run_for(8_000);
        assert_eq!(w.discarded, 1, "{reinit:?}");
        assert!(w.hw.buffer(ep).available, "{reinit:?}");
        for (k, &event) in (1..).zip(reinit) {
            w.events.push((w.now() + 1_000 * k, event));
        }
        if matches!(reinit[0], Event::Unconfigure) {
            // The second neutral report is dropped while the device is
            // unconfigured; the press waits for the next connection.
            w.run_for(2_500);
            assert!(w.events.is_empty());
            w.event(Event::Dtr(true));
        }
        let id = w.lease();
        assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
        w.run_for(20_000);
        assert_eq!(
            w.seen_from(ep, 0),
            [(F24, true), (F24, false), (F24, true)],
            "{reinit:?}"
        );
    }
}

/// A press the host took with its ACK lost, dropped by `SET_INTERFACE`
/// before its retry, may still be held at the host: its release goes ahead
/// of the next input.
#[test]
fn a_press_dropped_by_set_interface_is_released_first() {
    let mut w = World::new(Rate::Hz125, 3_000);
    let ep = Ep::Keyboard;
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.lose_ack[index(ep)] = 1;
    w.run_for(10_000);
    assert!(w.hw.buffer(ep).available, "the ACK was lost");
    w.event(Event::SetInterface(ep));
    assert_eq!(w.send(&format!("key {id} 4 1")), "ok");
    w.run_for(30_000);
    assert_eq!(
        w.seen_from(ep, 0),
        [
            (F24, true),
            (F24, false),
            (Control::Key(4), true),
            (F24, true)
        ]
    );
}

/// A bus reset leaves the buffers as they are (embassy-rp only disables
/// the endpoints): a report the host acknowledged just before keeps its
/// completion, and the release the disarm owes, handed while the endpoint
/// is disabled, is dropped by the configuration that follows and sent
/// again before the next press. The reset ends the connection, whose
/// trace goes with it, so the completion is read from the endpoint.
#[test]
fn a_bus_reset_keeps_the_release_owed() {
    let mut w = World::new(Rate::Hz125, 3_000);
    let ep = Ep::Keyboard;
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    let press = w.state.ep_ref(ep).assoc.next_seq();
    // The host takes the press at its poll at 27 ms; the reset comes in
    // the same step, before the writer sees the completion.
    w.events.push((27_000, Event::Reset));
    w.run_until(30_000);
    assert!(
        w.hw.buffer(ep).available,
        "the release handed while disabled"
    );
    let done = w.state.ep_ref(ep).last_done;
    assert!(
        matches!(done, Some(Done { seq, complete, .. }) if seq == press && complete >= 27_000),
        "the press taken before the reset keeps its completion: {done:?}"
    );
    w.event(Event::Configure);
    w.event(Event::Dtr(true));
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.run_for(20_000);
    assert_eq!(w.seen_from(ep, 0), [(F24, true), (F24, false), (F24, true)]);
}

/// An ACK that reaches the controller just before a reconfiguration,
/// whose completion the logic had not yet seen, still counts as dropped
/// (`E`, no `C`): the release goes first, the current state after it, and
/// the release at the disarm still reaches the host.
#[test]
fn an_ack_just_before_a_reconfiguration_counts_as_dropped() {
    let mut w = World::new(Rate::Hz125, 3_000);
    let ep = Ep::Keyboard;
    assert_eq!(w.send("trace on"), "ok");
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.run_until(27_000);
    let seq = w.state.ep_ref(ep).assoc.pending().unwrap();
    // The host takes and ACKs the press at its poll at 27 ms; the
    // configuration comes in the same step, before the writer looks.
    w.events.push((27_000, Event::Configure));
    w.step();
    assert_eq!(w.seen_from(ep, 0), [(F24, true)]);
    let bodies: Vec<Body> = w.records.iter().map(|r| r.body).collect();
    assert!(
        bodies.contains(&Body::Dropped {
            ep,
            seq,
            at: 27_000
        }),
        "{bodies:?}"
    );
    w.run_for(20_000);
    assert_eq!(w.send("release"), "ok");
    w.run_for(20_000);
    assert_eq!(
        w.seen_from(ep, 0),
        [(F24, true), (F24, false), (F24, true), (F24, false)]
    );
    let trace = w.trace();
    assert_eq!(
        trace
            .reports
            .get(&(ep, seq))
            .map(|r| (r.dropped, r.completion)),
        Some((true, None))
    );
}

/// A pad report after its predecessor was revoked agrees with the trace:
/// it carries no H and C, since the revoked report has no completion.
#[test]
fn a_pad_report_after_a_revocation_agrees_with_the_trace() {
    let mut w = World::new(Rate::Hz125, 3_000);
    assert_eq!(w.send("trace on"), "ok");
    let id = w.lease();
    assert_eq!(w.send(&format!("button {id} 1 1")), "ok");
    w.run_for(10_000);
    // Handed between polls, then revoked.
    assert_eq!(w.send(&format!("button {id} 2 1")), "ok");
    w.run_for(1_000);
    assert!(w.hw.buffer(Ep::Pad).available);
    assert_eq!(w.send("release"), "ok");
    w.run_for(20_000);
    let trace = w.trace();
    assert!(
        trace
            .reports
            .values()
            .any(|e| e.revoked.is_some_and(|(available, _)| available)),
        "a report was revoked"
    );
    assert_eq!(w.seen_from(Ep::Pad, 0), [(B1, true), (B1, false)]);
    w.check_pad_reports(&trace);
}

/// A line that is not a command ends the lease and releases what it held:
/// malformed, not UTF-8, or too long.
#[test]
fn a_malformed_line_releases_held_input() {
    let long = format!("{}\n", "a".repeat(MAX_LINE + 5));
    for (bytes, error) in [
        (&b"kye 115 0\n"[..], "err syntax"),
        (&b"key 115 \xff\n"[..], "err syntax"),
        (long.as_bytes(), "err overlong"),
    ] {
        let mut w = World::new(Rate::Hz1000, 300);
        let id = w.lease();
        assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
        w.run_for(3_000);
        assert_eq!(w.receive(bytes), [error], "{bytes:?}");
        w.run_for(3_000);
        assert_eq!(
            w.seen_from(Ep::Keyboard, 0),
            [(F24, true), (F24, false)],
            "{bytes:?}"
        );
        let hello = w.hello();
        assert_eq!(
            (hello.lease, hello.armed, hello.reason),
            (None, false, Reason::Protocol),
            "{bytes:?}"
        );
    }
}

/// Each case of the framing probe, a carriage return before the line feed
/// and an empty line included, is answered exactly once, with its error,
/// and ends the lease; the `ping` after it is answered `pong` with its own
/// number.
#[test]
fn a_non_canonical_line_is_refused_and_the_next_answer_pairs() {
    for case in Malformed::ALL {
        let mut w = World::new(Rate::Hz1000, 300);
        let id = w.lease();
        assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
        w.run_for(3_000);
        let mut packet = case.bytes().to_vec();
        packet.extend_from_slice(b"ping 7\n");
        let answers = w.receive(&packet);
        assert_eq!(answers.len(), 2, "{case:?}: {answers:?}");
        assert_eq!(
            answers[0],
            format!("err {}", case.answer().name()),
            "{case:?}"
        );
        assert!(
            matches!(
                Answer::parse_canonical(&answers[1]),
                Some(Answer::Pong { n: 7, .. })
            ),
            "{case:?}: {answers:?}"
        );
        w.run_for(3_000);
        assert_eq!(
            w.seen_from(Ep::Keyboard, 0),
            [(F24, true), (F24, false)],
            "{case:?}"
        );
        let hello = w.hello();
        assert_eq!((hello.lease, hello.reason), (None, Reason::Protocol));
    }
}

/// A partial line does not survive the link connection that sent it: its
/// rest, arriving in the next connection, is not a command, whether the
/// connection ended with DTR dropped, a bus reset or a failed read.
#[test]
fn a_partial_line_does_not_survive_its_connection() {
    for end in [Event::Dtr(false), Event::Reset, Event::Lost] {
        let mut w = World::new(Rate::Hz1000, 300);
        assert_eq!(w.receive(b"detach "), Vec::<String>::new());
        w.event(end);
        w.event(Event::Dtr(true));
        assert_eq!(w.receive(b"500\n"), ["err syntax"], "{end:?}");
    }
    // Within one connection a line spans packets.
    let mut w = World::new(Rate::Hz1000, 300);
    let line = format!("lease {} 1000 {}\n", w.token(), w.device_in(1_000_000));
    let (a, rest) = line.as_bytes().split_at(3);
    let (b, c) = rest.split_at(10);
    assert_eq!(w.receive(a), Vec::<String>::new());
    assert_eq!(w.receive(b), Vec::<String>::new());
    let answers = w.receive(c);
    assert!(
        answers[0].starts_with(&format!("lease {}:1 ", w.token())),
        "{answers:?}"
    );
}

/// A connection that ends while a command's outcome is awaited takes the
/// rest of its packet with it: the port closed and opened again while
/// `flush` waits on a pending report leaves no `detach` to complete, and
/// the abandoned `flush` sends no `F`.
#[test]
fn a_connection_that_ends_during_a_command_drops_the_rest_of_its_packet() {
    let mut w = World::new(Rate::Hz125, 3_000);
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.run_for(1_000);
    assert!(w.hw.buffer(Ep::Keyboard).available, "flush waits");
    let t = w.now();
    w.events.push((t + 1_000, Event::Dtr(false)));
    w.events.push((t + 2_000, Event::Dtr(true)));
    assert_eq!(w.receive(b"flush\ndetach "), Vec::<String>::new());
    w.run_until(t + 2_050);
    assert!(w.events.is_empty());
    assert!(
        !w.records.iter().any(|r| r.body == Body::Flushed),
        "the abandoned flush sent F: {:?}",
        w.records
    );
    assert_eq!(w.receive(b"500\n"), ["err syntax"]);
}

/// A failed read ends the connection and its lease: what was held is
/// released.
#[test]
fn a_failed_read_ends_the_lease() {
    let mut w = World::new(Rate::Hz1000, 300);
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.run_for(3_000);
    w.event(Event::Lost);
    w.run_for(3_000);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), [(F24, true), (F24, false)]);
    let hello = w.hello();
    assert_eq!((hello.lease, hello.reason), (None, Reason::Closed));
    assert_eq!(hello.session.map(|t| t.count), Some(1));
}

/// An unconfiguration ends the connection and its lease (`unconfigured`):
/// what was held is released once the device is configured again.
#[test]
fn an_unconfiguration_ends_the_lease() {
    let mut w = World::new(Rate::Hz1000, 300);
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.run_for(3_000);
    w.event(Event::Unconfigure);
    let hello = w.hello();
    assert_eq!((hello.lease, hello.reason), (None, Reason::Unconfigured));
    assert_eq!(
        w.send(&format!("key {id} 4 1")),
        "err no-lease",
        "the ended lease holds nothing"
    );
    w.event(Event::Configure);
    w.run_for(3_000);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), [(F24, true), (F24, false)]);
}

/// Every way a connection ends turns the trace off and drops the records
/// it had buffered: the next connection gets none of them, and no record
/// until it turns the trace on itself.
#[test]
fn a_connection_end_turns_the_trace_off_and_drops_its_records() {
    for end in [
        Event::Dtr(false),
        Event::Lost,
        Event::Reset,
        Event::Unconfigure,
    ] {
        let mut w = World::new(Rate::Hz125, 3_000);
        assert_eq!(w.send("trace on"), "ok");
        let id = w.lease();
        assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
        // Handed, its `T` buffered and not yet sent.
        w.write(Ep::Keyboard);
        assert!(!w.state.ring.is_empty(), "{end:?}");
        let before = w.records.len();
        w.event(end);
        assert_eq!(w.state.next_record(), None, "{end:?}");
        if matches!(end, Event::Reset | Event::Unconfigure) {
            w.event(Event::Configure);
        }
        w.event(Event::Dtr(true));
        let id = w.lease();
        assert_eq!(w.send(&format!("key {id} 4 1")), "ok");
        w.run_for(40_000);
        assert!(
            w.seen_from(Ep::Keyboard, 0)
                .contains(&(Control::Key(4), true)),
            "{end:?}"
        );
        assert_eq!(w.records.len(), before, "{end:?}: {:?}", w.records);
    }
}

/// Output carries its connection to the handoff: after the connection
/// ends, its answer, its record and its waiting `flush` are no longer
/// current, so the sender writes none of them; the next connection's are.
#[test]
fn output_of_an_ended_connection_is_not_current() {
    let mut w = World::new(Rate::Hz125, 3_000);
    assert_eq!(w.send("trace on"), "ok");
    assert_eq!(w.receive(b"ping 1\n").len(), 1);
    let answer = w.link.connection();
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.write(Ep::Keyboard);
    let (record, _) = w.state.next_record().expect("the press's T");
    assert!(w.state.is_current(answer) && w.state.is_current(record));
    w.event(Event::Dtr(false));
    w.event(Event::Dtr(true));
    assert!(!w.state.is_current(answer), "an answer");
    assert!(!w.state.is_current(record), "a record");
    assert!(
        !w.state.flush_end(&w.hw, answer),
        "a flush of the ended connection"
    );
    assert_eq!(w.state.next_record(), None, "the flush wrote nothing");
    // The next connection's output is current.
    assert_eq!(w.receive(b"ping 2\n").len(), 1);
    let next = w.link.connection();
    assert_ne!(next, answer);
    assert!(w.state.is_current(next));
    assert!(w.state.flush_end(&w.hw, next));
    assert!(matches!(w.state.next_record(), Some((c, r)) if c == next && r.body == Body::Flushed));
}

/// A grant names its connection: a token of a connection that ended is
/// refused `err session`, whichever boot half it has.
#[test]
fn a_grant_of_an_ended_connection_is_refused() {
    let mut w = World::new(Rate::Hz1000, 300);
    let old = w.token();
    assert_eq!(w.hello().session, Some(old));
    w.event(Event::Dtr(false));
    w.event(Event::Dtr(true));
    let token = w.token();
    assert_eq!((token.boot, token.count), (old.boot, old.count + 1));
    let deadline = w.device_in(1_000_000);
    assert_eq!(
        w.send(&format!("lease {old} 1000 {deadline}")),
        "err session"
    );
    assert_eq!(w.hello().lease, None);
    let id = w.lease();
    assert_eq!(id, LeaseId { token, n: 1 });
    assert_eq!(
        w.send(&format!(
            "renew {} {deadline}",
            LeaseId { token: old, n: 1 }
        )),
        "err session"
    );
}

/// A grant and a hold of an earlier boot are refused: the boot's draw
/// differs, so its token is another, and its lease id names no lease.
#[test]
fn an_old_grant_and_its_hold_after_a_reboot_are_refused() {
    let mut before = World::new(Rate::Hz1000, 300);
    let old_token = before.token();
    let old_id = before.lease();
    let mut w = World::with_draw(Rate::Hz1000, 300, Some(!DRAW));
    assert_ne!(w.token(), old_token);
    let deadline = w.device_in(1_000_000);
    assert_eq!(
        w.send(&format!("lease {old_token} 1000 {deadline}")),
        "err session"
    );
    assert_eq!(w.send(&format!("button {old_id} 1 1")), "err no-lease");
    let id = w.lease();
    assert_eq!(id.n, old_id.n, "the same number under another token");
    assert_eq!(w.send(&format!("button {old_id} 1 1")), "err wrong-id");
    w.run_for(3_000);
    assert_eq!(w.seen_from(Ep::Pad, 0), []);
}

/// A boot whose TRNG draw failed has no token: `hello` reads `session -`
/// and every grant is refused `err session`.
#[test]
fn a_boot_without_a_draw_refuses_every_grant() {
    let mut w = World::with_draw(Rate::Hz1000, 300, None);
    assert_eq!(w.state.token(), None);
    assert_eq!(w.hello().session, None);
    let deadline = w.device_in(1_000_000);
    let any = Token { boot: 0, count: 0 };
    assert_eq!(
        w.send(&format!("lease {any} 1000 {deadline}")),
        "err session"
    );
    assert_eq!(w.hello().lease, None);
}

/// A held change acts only under the lease it names: another number or
/// another connection's token is refused `err wrong-id`.
#[test]
fn a_held_change_naming_another_lease_is_refused() {
    let mut w = World::new(Rate::Hz1000, 300);
    let id = w.lease();
    let other_n = LeaseId { n: id.n + 1, ..id };
    let other_token = LeaseId {
        token: Token {
            count: id.token.count + 1,
            ..id.token
        },
        ..id
    };
    for other in [other_n, other_token] {
        assert_eq!(w.send(&format!("button {other} 1 1")), "err wrong-id");
        assert_eq!(w.send(&format!("key {other} 4 1")), "err wrong-id");
        assert_eq!(w.send(&format!("axis {other} 200")), "err wrong-id");
        assert_eq!(w.send(&format!("arm {other} 0")), "err wrong-id");
    }
    w.run_for(3_000);
    assert_eq!(w.seen_from(Ep::Pad, 0), []);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), []);
    assert_eq!(w.send(&format!("button {id} 1 1")), "ok");
    w.run_for(3_000);
    assert_eq!(w.seen_from(Ep::Pad, 0), [(B1, true)]);
}

/// The release property's authority cutoff with the lease's timer
/// stalled: after the deadline a held change is refused, no pressed report
/// is handed, and the owed neutral report goes at the next poll.
#[test]
fn authority_ends_at_the_deadline_with_the_timer_stalled() {
    let mut w = World::new(Rate::Hz125, 3_000);
    w.timer = false;
    assert_eq!(w.now(), 19_000);
    // Granted at 19 ms with its deadline at 30 ms.
    let id = w.lease_until(11, w.state.out(30_000));
    assert_eq!(w.send(&format!("button {id} 1 1")), "ok");
    w.run_until(31_000);
    assert_eq!(w.send(&format!("button {id} 2 1")), "err no-lease");
    w.run_until(60_000);
    assert_eq!(
        w.seen(Ep::Pad)
            .iter()
            .map(|s| (s.at, s.control, s.down))
            .collect::<Vec<_>>(),
        [(27_000, B1, true), (35_000, B1, false)],
        "the release at the first poll after the deadline"
    );
    assert!(w.pad_from(30_000).iter().all(|(_, r)| r.buttons == 0));
    assert_eq!(w.hello().reason, Reason::Expired);
}

/// A renewal that reaches the device after the lease's deadline, before
/// the timer or anything else has looked, finds the lease ended.
#[test]
fn a_renewal_after_the_deadline_finds_the_lease_ended_before_the_timer_runs() {
    let mut w = World::new(Rate::Hz125, 3_000);
    w.timer = false;
    let id = w.lease_until(11, w.state.out(30_000));
    w.hw.t.set(30_000);
    assert_eq!(
        w.send(&format!("renew {id} {}", w.state.out(40_000))),
        "err no-lease"
    );
    let hello = w.hello();
    assert_eq!((hello.lease, hello.reason), (None, Reason::Expired));
}

/// A renewal is judged at one instant: the old deadline's expiry and the
/// new deadline's bounds are read from the same clock sample. Here the
/// clock advances 2 µs at every read, so a second read would fall past the
/// old deadline, where the new deadline lies within the lease's length, while
/// the sample the expiry check took, before the deadline, puts it past the
/// length.
#[test]
fn a_renewal_is_judged_at_one_instant() {
    let mut w = World::new(Rate::Hz125, 3_000);
    w.timer = false;
    let id = w.lease_until(11, w.state.out(30_000));
    // A debug-faults build reads the clock once more first, for the
    // parser-off fault.
    let before = 2 * u64::from(cfg!(feature = "debug-faults"));
    w.hw.t.set(29_999 - before);
    w.hw.tick.set(2);
    let answer = w.send(&format!("renew {id} {}", w.state.out(30_001 + 11_000)));
    w.hw.tick.set(0);
    assert_eq!(answer, "err bad-length", "judged at two instants");
    assert_eq!(w.hello().lease, Some(id.n), "the lease as it was");
}

/// A report decided before the deadline and committed after it is neutral:
/// the deadline is checked with the clock sample that stamps the report,
/// which the firmware takes in the critical section that hands the report
/// to the controller.
#[test]
fn a_report_built_after_the_deadline_is_neutral() {
    let mut w = World::new(Rate::Hz125, 3_000);
    w.timer = false;
    let id = w.lease_until(11, w.state.out(30_000));
    w.run_until(29_000);
    assert_eq!(w.send(&format!("button {id} 1 1")), "ok");
    assert_eq!(
        w.state.next_report(&w.hw, Ep::Pad),
        Next::Now(Kind::Current)
    );
    w.hw.t.set(30_000);
    let data = w.commit(Ep::Pad, Kind::Current);
    let report = PadReport::decode(&data).unwrap();
    assert_eq!(
        report.buttons, 0,
        "a pressed report built after the deadline"
    );
    w.hand(Ep::Pad, data);
    w.run_until(60_000);
    assert_eq!(w.seen_from(Ep::Pad, 0), []);
    assert_eq!(w.hello().reason, Reason::Expired);
}

/// The guard band at the handoff: a pressed report staged before the
/// deadline and committed with the clock inside [`COMMIT_MARGIN_US`] of it
/// ends the lease there (`expired`) and commits the owed neutral report
/// instead, which the host takes at its next poll; committed one µs
/// earlier, outside the margin, it is pressed and the lease is held.
#[test]
fn a_report_committed_within_the_margin_of_the_deadline_is_the_release() {
    let margin = 30_000 - COMMIT_MARGIN_US;
    for (at, pressed) in [(margin, false), (margin - 1, true)] {
        assert!(
            pressed || at < 30_000,
            "no guard band: the first commit that ends the lease is at the deadline"
        );
        let mut w = World::new(Rate::Hz125, 3_000);
        w.timer = false;
        // Granted at 19 ms with its deadline at 30 ms; polls at 27 and
        // 35 ms. Button 1 is taken at 27 ms, so a release is owed when the
        // lease ends.
        let id = w.lease_until(11, w.state.out(30_000));
        assert_eq!(w.send(&format!("button {id} 1 1")), "ok");
        w.run_until(29_000);
        assert_eq!(w.send(&format!("button {id} 2 1")), "ok");
        assert_eq!(
            w.state.next_report(&w.hw, Ep::Pad),
            Next::Now(Kind::Current),
            "staged before the deadline"
        );
        w.hw.t.set(at);
        let data = w.commit(Ep::Pad, Kind::Current);
        let buttons = PadReport::decode(&data).unwrap().buttons;
        assert_eq!(buttons != 0, pressed, "committed at {at} µs: {buttons:#x}");
        let held = w.state.lease.held();
        w.hand(Ep::Pad, data);
        // Back on the simulation's step, where the host polls.
        w.hw.t.set(30_000);
        w.run_until(60_000);
        let seen: Vec<_> = w
            .seen(Ep::Pad)
            .iter()
            .map(|s| (s.at, s.control, s.down))
            .collect();
        if pressed {
            // Taken at 35 ms, after the deadline: the next ask releases.
            assert_eq!(held, Some(id.n));
            assert_eq!(
                seen,
                [
                    (27_000, B1, true),
                    (35_000, B2, true),
                    (43_000, B1, false),
                    (43_000, B2, false)
                ]
            );
        } else {
            assert_eq!(held, None, "the lease ended at the commit");
            assert_eq!(w.hello().reason, Reason::Expired);
            assert_eq!(
                seen,
                [(27_000, B1, true), (35_000, B1, false)],
                "the owed release at the next poll"
            );
        }
    }
}

/// A pressed report handed before the deadline and taken after it, with
/// the timer stalled and nothing queued: the writer's next ask, at the
/// completion, ends the lease and the release goes at the next poll.
#[test]
fn a_press_taken_after_the_deadline_is_released_at_the_next_poll() {
    let mut w = World::new(Rate::Hz125, 3_000);
    w.timer = false;
    // Granted at 19 ms with its deadline at 30 ms; polls at 27, 35 and
    // 43 ms. Button 1 is handed at 28 ms and taken at 35 ms.
    let id = w.lease_until(11, w.state.out(30_000));
    w.run_until(28_000);
    assert_eq!(w.send(&format!("button {id} 1 1")), "ok");
    w.run_until(60_000);
    assert_eq!(
        w.seen(Ep::Pad)
            .iter()
            .map(|s| (s.at, s.control, s.down))
            .collect::<Vec<_>>(),
        [(35_000, B1, true), (43_000, B1, false)],
        "the release at the poll after the press was taken"
    );
    assert_eq!(w.hello().reason, Reason::Expired);
}

/// A report staged before the host reconfigures its endpoint is committed
/// as the neutral report the reconfiguration owes: the release goes ahead
/// of the staged change, and the current state follows it.
#[test]
fn a_report_staged_before_a_reconfiguration_is_committed_as_the_release() {
    let mut w = World::new(Rate::Hz125, 3_000);
    let id = w.lease();
    assert_eq!(w.send(&format!("button {id} 1 1")), "ok");
    w.run_for(10_000);
    assert_eq!(w.seen_from(Ep::Pad, 0), [(B1, true)]);
    assert_eq!(w.send(&format!("button {id} 2 1")), "ok");
    assert_eq!(
        w.state.next_report(&w.hw, Ep::Pad),
        Next::Now(Kind::Current)
    );
    w.event(Event::SetInterface(Ep::Pad));
    let data = w.commit(Ep::Pad, Kind::Current);
    w.hand(Ep::Pad, data);
    w.run_for(30_000);
    assert_eq!(
        w.seen_from(Ep::Pad, 0),
        [(B1, true), (B1, false), (B1, true), (B2, true)],
        "the release before the staged change"
    );
}

/// The watchdog is fed only on the lease timer's progress: with the timer
/// stalled before the deadline and both endpoints idle after a press the
/// host took, nothing else ends the lease, and the device is reset within
/// the stall's start plus the watchdog's timeout plus the feed period
/// (or the host has the release by then).
#[test]
fn a_stalled_timer_ends_in_a_release_or_a_reset() {
    let mut w = World::new(Rate::Hz1000, 300);
    w.watchdog = Some(Watchdog {
        fed: w.now(),
        reset: None,
    });
    let id = w.lease_until(1_000, w.device_in(500_000));
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.run_for(3_000);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), [(F24, true)]);
    let stall = w.now();
    w.timer = false;
    let bound = stall + WATCHDOG_US + FEED_EVERY_US;
    w.run_until(bound + STEP);
    let reset = w.watchdog.and_then(|dog| dog.reset);
    let release = w
        .seen(Ep::Keyboard)
        .into_iter()
        .find(|s| s.control == F24 && !s.down)
        .map(|s| s.at);
    assert!(
        reset.is_some_and(|t| t <= bound) || release.is_some_and(|t| t <= bound),
        "the press held past {bound} µs: reset {reset:?}, release {release:?}"
    );
}

/// Output of an ended connection may still sit in the link's IN buffer:
/// the connection's end retires it, and the next connection's input runs
/// once that is done. Without the controller's acknowledgement of the
/// retirement the device recovers by rebooting and the next connection's
/// input is held, neither acted on nor answered.
#[test]
fn the_next_connection_waits_until_the_ended_ones_output_is_retired() {
    for acknowledged in [true, false] {
        let mut w = World::new(Rate::Hz1000, 300);
        w.hw.link_acknowledges.set(acknowledged);
        // A's answer, handed to the link's IN buffer.
        assert_eq!(w.receive(b"ping 1\n").len(), 1);
        w.hw.actions.borrow_mut().clear();
        w.event(Event::Dtr(false));
        assert!(
            w.hw.actions.borrow().contains(&Action::RetireLinkOutput),
            "the end retired nothing"
        );
        w.event(Event::Dtr(true));
        let packet = format!("lease {} 1000 {}\n", w.token(), w.device_in(1_000_000));
        let mut at = 0;
        let next = w.link.next(&mut w.state, &w.hw, packet.as_bytes(), &mut at);
        if acknowledged {
            assert!(
                matches!(next, Some(Outcome::Answer(Answer::Leased { .. }))),
                "{next:?}"
            );
        } else {
            assert_eq!(
                next,
                Some(Outcome::Hold),
                "B's command before the retirement"
            );
            assert_eq!((at, w.state.lease.held()), (0, None), "held, not acted on");
            assert!(w.state.recovering());
            let again = w.link.next(&mut w.state, &w.hw, packet.as_bytes(), &mut at);
            assert_eq!(again, Some(Outcome::Hold), "held until the reboot");
        }
    }
}

/// The link's sender asks the connection in the same poll that writes, the
/// connection first: a packet whose connection ended is cancelled even when
/// the IN buffer came free before that poll, and the next connection's
/// packet is written once the buffer is free.
#[test]
fn a_packet_of_an_ended_connection_is_cancelled_when_the_buffer_frees_too() {
    let mut w = World::new(Rate::Hz1000, 300);
    let old = w.state.connection();
    assert_eq!(w.state.link_write(old, false), LinkWrite::Wait);
    assert_eq!(w.state.link_write(old, true), LinkWrite::Write);
    // The end and the buffer coming free, both before the sender's poll.
    w.event(Event::Dtr(false));
    assert_eq!(
        w.state.link_write(old, true),
        LinkWrite::Cancel,
        "an ended connection's packet written"
    );
    assert_eq!(w.state.link_write(old, false), LinkWrite::Cancel);
    w.event(Event::Dtr(true));
    let next = w.state.connection();
    assert_eq!(w.state.link_write(next, true), LinkWrite::Write);
}

/// The connection count never wraps: the boot's last count leaves it with
/// no token, so `hello` reads `session -` and every grant is refused
/// `err session` until a reboot draws again; the ended connection's output
/// stays ended.
#[test]
fn the_last_connection_count_leaves_the_boot_without_a_token() {
    let mut w = World::new(Rate::Hz1000, 300);
    w.state.set_count(u32::MAX);
    let last = w.token();
    let ended = w.state.connection();
    w.event(Event::Dtr(false));
    w.event(Event::Dtr(true));
    assert_eq!(w.state.token(), None, "a count reused");
    assert!(!w.state.is_current(ended));
    assert_eq!(w.hello().session, None);
    let deadline = w.device_in(1_000_000);
    for token in [last, Token { count: 0, ..last }] {
        assert_eq!(
            w.send(&format!("lease {token} 1000 {deadline}")),
            "err session"
        );
    }
}

/// A grant past the connection's last number reboots the device, answering
/// nothing, instead of ending the connection in place: the connection and
/// its token stay current until the reboot leaves the bus, nothing is
/// retired for a next connection, and no grant number is reused.
#[test]
fn a_grant_past_the_last_number_reboots_the_device() {
    let mut w = World::new(Rate::Hz1000, 300);
    w.state.lease.set_last_grant(u32::MAX - 1);
    let id = w.lease();
    assert_eq!(id.n, u32::MAX);
    assert_eq!(w.send("release"), "ok");
    let connection = w.state.connection();
    let deadline = w.device_in(1_000_000);
    w.hw.actions.borrow_mut().clear();
    assert_eq!(
        w.send(&format!("lease {} 1000 {deadline}", id.token)),
        format!("{:?}", Outcome::Reboot),
        "answered instead of rebooting"
    );
    let actions = w.hw.actions.borrow().clone();
    assert!(
        !actions.contains(&Action::RetireLinkOutput),
        "the connection ended in place: {actions:?}"
    );
    assert_eq!(
        w.state.connection(),
        connection,
        "a next connection in place"
    );
    assert!(w.state.is_current(connection));
    assert_eq!(w.token(), id.token);
    assert_eq!(w.hello().lease, None, "a grant number reused");
}

/// A held change that reaches the device after the lease's deadline,
/// before the timer or anything else has looked, is refused.
#[test]
fn a_held_change_after_the_deadline_is_refused_before_the_timer_runs() {
    let mut w = World::new(Rate::Hz125, 3_000);
    w.timer = false;
    let id = w.lease_until(11, w.state.out(30_000));
    w.hw.t.set(30_000);
    assert_eq!(w.send(&format!("button {id} 1 1")), "err no-lease");
    assert_eq!(w.hello().reason, Reason::Expired);
    w.run_for(20_000);
    assert_eq!(w.seen_from(Ep::Pad, 0), []);
}

/// A change queued before the deadline whose report the writer asks for
/// after it is never handed: the owed neutral report goes instead.
#[test]
fn no_pressed_report_is_handed_after_the_deadline_before_the_timer_runs() {
    let mut w = World::new(Rate::Hz125, 3_000);
    w.timer = false;
    // Granted at 19 ms with its deadline at 45 ms; polls at 27, 35, 43,
    // 51 and 59 ms.
    let id = w.lease_until(26, w.state.out(45_000));
    assert_eq!(w.send(&format!("button {id} 1 1")), "ok");
    w.run_until(44_000);
    // Button 2's report is handed now, before the deadline, and taken at
    // 51 ms; button 3's waits for it, and the writer asks for it after the
    // deadline, which ends the lease there. Nothing else runs at the
    // deadline: revoking a report handed before it is the timer's part.
    assert_eq!(w.send(&format!("button {id} 2 1")), "ok");
    assert_eq!(w.send(&format!("button {id} 3 1")), "ok");
    w.run_until(70_000);
    assert!(
        !w.seen(Ep::Pad).iter().any(|s| s.control == B3 && s.down),
        "button 3 was pressed after the deadline: {:?}",
        w.seen(Ep::Pad)
    );
    let after = w.pad_from(45_000);
    assert!(
        after.last().is_some_and(|(_, r)| r.buttons == 0),
        "the owed neutral report last: {after:?}"
    );
    assert_eq!(
        w.seen_from(Ep::Pad, 45_000),
        [(B2, true), (B1, false), (B2, false)],
        "only the report handed before the deadline, then the release"
    );
}

/// A grant delayed past the deadline its host set (a check at t0, `lease
/// <token> 250 <t0 + 250 ms>` arriving after t0 + 250 ms) is refused, so a
/// host that holds only under the id a grant's answer gives holds nothing,
/// and no pressed report is handed.
#[test]
fn a_grant_delayed_past_its_deadline_hands_no_pressed_report() {
    let mut w = World::new(Rate::Hz1000, 300);
    let token = w.token();
    let checked = w.now();
    let deadline = w.state.out(checked + 250_000);
    w.run_until(checked + 251_000);
    let answer = w.send(&format!("lease {token} 250 {deadline}"));
    assert_eq!(answer, "err stale");
    if let Some(Answer::Leased { id, .. }) = Answer::parse_canonical(&answer) {
        assert_eq!(w.send(&format!("button {id} 1 1")), "ok");
    }
    w.run_for(20_000);
    assert_eq!(w.seen_from(Ep::Pad, 0), [], "a pressed report was handed");
}

/// The suspend's release goes first at resume: input queued under a new
/// lease while the bus is suspended follows the owed neutral report.
#[test]
fn a_suspend_releases_at_resume_first() {
    let mut w = World::new(Rate::Hz125, 3_000);
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.run_for(10_000);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), [(F24, true)]);
    w.event(Event::Suspend(true));
    assert_eq!(w.hello().reason, Reason::Suspended);
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 4 1")), "ok");
    let suspended = w.now();
    w.run_for(30_000);
    assert_eq!(w.seen_from(Ep::Keyboard, suspended), [], "nothing polled");
    let resumed = w.now();
    w.event(Event::Suspend(false));
    w.run_for(30_000);
    let first = w
        .accepted
        .iter()
        .find(|a| a.ep == Ep::Keyboard && a.at >= resumed)
        .expect("a report after resume");
    assert!(
        first.data.iter().all(|&b| b == 0),
        "the first report at resume is neutral: {:?}",
        first.data
    );
    assert_eq!(
        w.seen_from(Ep::Keyboard, resumed),
        [(F24, false), (Control::Key(4), true)]
    );
}

/// After an unconfiguration nothing is polled, and the release is the
/// first report the host takes once it configures the device again, ahead
/// of a press of the same button queued in the next connection.
#[test]
fn an_unconfiguration_releases_at_the_next_configuration() {
    let mut w = World::new(Rate::Hz125, 3_000);
    let id = w.lease();
    assert_eq!(w.send(&format!("button {id} 1 1")), "ok");
    w.run_for(10_000);
    assert_eq!(w.seen_from(Ep::Pad, 0), [(B1, true)]);
    w.event(Event::Unconfigure);
    let unconfigured = w.now();
    w.run_for(30_000);
    assert!(w.pad_from(unconfigured).is_empty(), "nothing polled");
    w.event(Event::Dtr(true));
    let id = w.lease();
    assert_eq!(w.send(&format!("button {id} 1 1")), "ok");
    let configured = w.now();
    w.event(Event::Configure);
    w.run_for(30_000);
    let after = w.pad_from(configured);
    assert!(
        after.first().is_some_and(|(_, r)| r.buttons == 0),
        "{after:?}"
    );
    assert_eq!(
        w.seen_from(Ep::Pad, 0),
        [(B1, true), (B1, false), (B1, true)]
    );
}

/// `hello` says whether the build has the debug faults.
#[test]
fn hello_reports_the_fault_capability() {
    let mut w = World::new(Rate::Hz1000, 300);
    assert_eq!(w.hello().faults, cfg!(feature = "debug-faults"));
}

/// `hello`'s quiet counts what the host took: 0 while a release is owed or
/// pending, 1 once the host took the neutral report; a device that never
/// pressed anything is quiet; after a revocation both owed reports must be
/// taken; the axis's position does not count.
#[test]
fn hello_is_quiet_only_once_the_host_took_a_neutral_report() {
    let mut w = World::new(Rate::Hz125, 3_000);
    assert!(w.hello().quiet, "nothing ever pressed");
    let id = w.lease();
    assert!(!w.hello().quiet, "a lease held");
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.run_for(10_000);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), [(F24, true)]);
    // A press taken, then released: owed, then pending, then taken.
    let answers = w.receive(b"release\nhello\n");
    assert!(answers[1].contains(" quiet 0 "), "{answers:?}");
    w.step();
    assert!(w.state.ep_ref(Ep::Keyboard).assoc.pending().is_some());
    assert!(!w.hello().quiet, "the owed report pending");
    w.run_for(w.interval);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), [(F24, true), (F24, false)]);
    assert!(w.hello().quiet, "the neutral report taken");
    // A press handed and revoked before the host took it: two neutral
    // reports owed, and quiet only after both are taken.
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 4 1")), "ok");
    w.run_for(1_000);
    assert!(w.hw.buffer(Ep::Keyboard).available, "the press waits");
    let answers = w.receive(b"release\nhello\n");
    assert!(answers[1].contains(" quiet 0 "), "{answers:?}");
    let mut quiet_at = None;
    for _ in 0..(3 * w.interval / STEP) {
        w.step();
        if quiet_at.is_none() && w.hello().quiet {
            quiet_at = Some(w.now());
        }
    }
    let taken: Vec<u64> = w
        .accepted
        .iter()
        .filter(|a| a.ep == Ep::Keyboard)
        .map(|a| a.at)
        .collect();
    let second = *taken.last().unwrap();
    assert_eq!(
        taken.len(),
        5,
        "the configuration's report, the press, its release and two neutral"
    );
    assert!(
        quiet_at.is_some_and(|t| t > second),
        "quiet at {quiet_at:?}, the second neutral report taken at {second}"
    );
    // The axis keeps its position.
    let id = w.lease();
    assert_eq!(w.send(&format!("axis {id} 200")), "ok");
    w.run_for(10_000);
    assert_eq!(w.send("release"), "ok");
    w.run_for(10_000);
    assert!(w.hello().quiet, "the axis at 200");
}

/// A line coding of 1200 baud asks for BOOTSEL as it arrives, whatever the
/// parser holds; another rate does not, nor 1200 under `baud-off`.
#[test]
fn a_1200_baud_line_coding_asks_for_bootsel() {
    let mut w = World::new(Rate::Hz1000, 300);
    w.event(Event::Baud(115_200));
    assert!(!w.state.bootsel_due());
    assert_eq!(w.receive(b"boot"), Vec::<String>::new());
    w.event(Event::Baud(1200));
    assert!(w.state.bootsel_due());
    #[cfg(feature = "debug-faults")]
    {
        let mut w = World::new(Rate::Hz1000, 300);
        w.state.set_fault(&w.hw, Fault::BaudOff, 0);
        w.event(Event::Baud(1200));
        assert!(!w.state.bootsel_due());
    }
}

/// An abort the controller never acknowledges touches nothing and claims
/// nothing: no buffer write, no revocation, the report still pending; the
/// device records `A`, sends nothing more and reboots.
#[test]
fn an_unacknowledged_abort_touches_nothing_and_recovers() {
    let mut w = World::new(Rate::Hz125, 3_000);
    assert_eq!(w.send("trace on"), "ok");
    let id = w.lease();
    w.run_for(5_000);
    assert_eq!(w.send(&format!("key {id} 115 1")), "ok");
    w.run_for(1_000);
    assert!(w.hw.buffer(Ep::Keyboard).available);
    w.hw.acknowledges.set(false);
    let before = w.records.len();
    assert_eq!(w.send("release"), "ok");
    w.drain();
    assert_eq!(w.hw.writes.get(), 0, "the buffer control was written");
    assert!(w.hw.buffer(Ep::Keyboard).available);
    assert!(w.state.recovering());
    let bodies: Vec<Body> = w.records[before..].iter().map(|r| r.body).collect();
    assert!(
        matches!(
            bodies[..],
            [Body::Unacknowledged {
                ep: Ep::Keyboard,
                ..
            }]
        ),
        "{bodies:?}"
    );
    assert_eq!(w.state.next_report(&w.hw, Ep::Pad), Next::Idle);
    assert!(!w.state.recovery_due(w.now()));
    assert!(w.state.recovery_due(w.now() + RECOVERY_US));
}

/// One revocation trial on `ep` at 125 Hz, judged on the trace flushed
/// when the withheld window ends; when it passes, the host's rearm and a
/// 3-item pattern: the verdict and the listener's changes after the rearm.
fn trial(ep: Ep) -> (Result<(), &'static str>, Vec<(Control, bool)>) {
    let mut w = World::new(Rate::Hz125, 3_000);
    assert_eq!(w.send("trace on"), "ok");
    let id = w.lease();
    assert_eq!(w.send(&format!("revoke-test {id} {} 1", ep.name())), "ok");
    w.run_for(20_000);
    let run = w
        .records
        .iter()
        .find_map(|r| match r.body {
            Body::Trial(run) => Some(run),
            _ => None,
        })
        .expect("the trial's V record");
    assert_eq!((run.ep, run.trial), (ep, 1));
    let rearm = run.probe + REARM_AFTER_PROBE;
    w.run_until(rearm);
    let trace = w.trace();
    if ep == Ep::Pad {
        w.check_pad_reports(&trace);
    }
    let verdict = judge(&run, &trace, &w.seen(ep)).map_err(|f| f.message());
    if verdict.is_err() {
        return (verdict, Vec::new());
    }
    let id = w.lease();
    let items = match ep {
        Ep::Keyboard => ["item 0 k4+", "item 20000 k4-", "item 40000 k5+"],
        Ep::Pad => ["item 0 b1+", "item 20000 b1-", "item 40000 b2+"],
    };
    assert_eq!(w.send("load"), "ok");
    for item in items {
        assert_eq!(w.send(item), "ok");
    }
    let at = w.now();
    assert_eq!(w.send(&format!("arm {id} {at}")), "ok");
    w.run_for(70_000);
    let trace = w.trace();
    if ep == Ep::Pad {
        w.check_pad_reports(&trace);
    }
    (verdict, w.seen_from(ep, rearm))
}

#[test]
fn a_keyboard_revocation_trial_passes() {
    let (verdict, after) = trial(Ep::Keyboard);
    assert_eq!(verdict, Ok(()));
    assert_eq!(
        after,
        [
            (Control::Key(4), true),
            (Control::Key(4), false),
            (Control::Key(5), true)
        ]
    );
}

#[test]
fn a_pad_revocation_trial_passes() {
    let (verdict, after) = trial(Ep::Pad);
    assert_eq!(verdict, Ok(()));
    assert_eq!(after, [(B1, true), (B1, false), (B2, true)]);
}

/// The trial is refused outside 125 Hz on change, without a lease, and
/// while something plays.
#[test]
fn a_trial_needs_its_mode_and_an_idle_lease() {
    let mut w = World::new(Rate::Hz1000, 300);
    let id = w.lease();
    assert_eq!(w.send(&format!("revoke-test {id} k 1")), "err mode");
    let mut w = World::new(Rate::Hz125, 3_000);
    let none = LeaseId {
        token: w.token(),
        n: 1,
    };
    assert_eq!(w.send(&format!("revoke-test {none} k 1")), "err no-lease");
    let id = w.lease();
    assert_eq!(w.send(&format!("key {id} 4 1")), "ok");
    w.run_for(10_000);
    assert_eq!(
        w.send(&format!("revoke-test {id} k 1")),
        "err playing",
        "a key held"
    );
    assert_eq!(w.send(&format!("revoke-test {id} p 1")), "ok");
    assert_eq!(w.send("item 0 b1+"), "err playing");
}
