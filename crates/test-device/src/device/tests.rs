//! The device's logic against a simulated controller and host: the host
//! polls each endpoint at the report interval and receives a report whose
//! AVAILABLE is set unless the endpoint is aborting. It accepts a report
//! whose DATA PID is the one it expects and discards one that repeats the
//! last it accepted, as a USB host does with a duplicate, and ACKs both;
//! the controller clears AVAILABLE when the ACK reaches it, so a lost ACK
//! leaves a report the host accepted AVAILABLE. The writers follow the
//! firmware's loops: wait for the next report, write it as embassy-rp does
//! (the opposite PID of the register, then AVAILABLE), then wait for its
//! completion. The link's bytes go through [`Link`]; the link's control
//! requests and the bus's events (a reset, the host configuring the device
//! or an interface again, which rewrites the endpoints' buffer controls as
//! embassy-rp does) go to the logic at their times, as the firmware's USB
//! handler passes them on, after the host's poll in the same step and
//! before the writers look.

use std::cell::{Cell, RefCell};

use super::*;
use crate::protocol::Answer;
use crate::revoke::{REARM_AFTER_PROBE, Seen, judge};
use crate::trace::{Check, Record, Trace};
use crate::wrap::SeqUnwrap;

/// The simulation's time step, µs.
const STEP: u64 = 50;

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
    /// Buffer control writes by the logic.
    writes: Cell<u32>,
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
            writes: Cell::new(0),
        }
    }
}

fn set(cell: &Cell<[bool; 2]>, ep: Ep, on: bool) {
    let mut v = cell.get();
    v[index(ep)] = on;
    cell.set(v);
}

impl Hardware for Sim {
    fn now(&self) -> u64 {
        self.t.get()
    }

    fn buffer(&self, ep: Ep) -> Buffer {
        self.buffers.borrow()[index(ep)].control
    }

    fn set_buffer(&self, ep: Ep, buffer: Buffer) {
        self.writes.set(self.writes.get() + 1);
        self.buffers.borrow_mut()[index(ep)].control = buffer;
    }

    fn set_abort(&self, ep: Ep, on: bool) {
        set(&self.abort, ep, on);
        if on && self.acknowledges.get() {
            set(&self.done, ep, true);
        }
    }

    fn abort_done(&self, ep: Ep) -> bool {
        self.done.get()[index(ep)]
    }

    fn clear_abort_done(&self, ep: Ep) {
        set(&self.done, ep, false);
    }

    fn kick(&self, _ep: Ep) {}

    fn kick_trace(&self) {}
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
}

struct World {
    hw: Sim,
    state: Box<State>,
    link: Link,
    /// Bus events due at their times.
    events: Vec<(u64, Event)>,
    /// Whether each endpoint is enabled: the host polls only those.
    enabled: [bool; 2],
    /// Each endpoint's writer waits for a completion.
    writing: [bool; 2],
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
        let mut state = Box::new(State::new());
        state.set_boot(
            Mode {
                rate,
                reporting: Reporting::OnChange,
            },
            Cause::PowerOn,
            0,
        );
        let mut w = Self {
            hw: Sim::new(),
            state,
            link: Link::new(),
            events: Vec::new(),
            enabled: [false; 2],
            writing: [false; 2],
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
        if !self.enabled[i] || self.hw.abort.get()[i] {
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
            Event::Lost => self.state.link_lost(),
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
        }
        let Next::Now(kind) = self.state.next_report(&self.hw, ep) else {
            return;
        };
        let data = match ep {
            Ep::Keyboard => self.state.keyboard_report(&self.hw, kind).to_vec(),
            Ep::Pad => self.state.pad_report(&self.hw, kind).to_vec(),
        };
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
        while let Some(r) = self.state.next_record() {
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
        if t.is_multiple_of(500) {
            self.state.expire(&self.hw);
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
    /// step between the commands they hold, except while `flush` waits.
    /// The answers.
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
                    let deadline = self.now() + 2 * self.interval + 10_000;
                    while !self.state.flush_poll(&self.hw) && self.now() < deadline {
                        self.step();
                    }
                    self.state.flush_end(&self.hw);
                    self.drain();
                }
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

    /// A lease of 30 s: its id.
    fn lease(&mut self) -> u32 {
        let a = self.send("lease 30000");
        match Answer::parse(&a) {
            Some(Answer::Leased { id, .. }) => id,
            _ => panic!("lease: {a}"),
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

/// After a disarm, new input waits until the host has taken the neutral
/// report: one packet that releases, leases again and arms an item
/// pressing the same key still shows the key going up, then down.
#[test]
fn new_input_waits_for_the_neutral_report_a_disarm_owes() {
    let mut w = World::new(Rate::Hz1000, 300);
    w.lease();
    assert_eq!(w.send("key 115 1"), "ok");
    w.run_for(3_000);
    assert_eq!(w.seen_from(Ep::Keyboard, 0), [(F24, true)]);
    let from = w.now();
    let answers = w.receive(b"release\nlease 1000\nitem 0 k115+\narm 2 0\n");
    assert_eq!(answers[0], "ok");
    assert!(answers[1].starts_with("lease 2 "), "{answers:?}");
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
    w.lease();
    assert_eq!(w.send("key 115 1"), "ok");
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
    w.lease();
    assert_eq!(w.send("key 115 1"), "ok");
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
    w.lease();
    assert_eq!(w.send("key 4 1"), "ok");
    w.run_for(10_000);
    assert_eq!(
        w.seen_from(Ep::Keyboard, 0),
        [(F24, true), (F24, false), (Control::Key(4), true)]
    );
}

/// A neutral report the USB stack drops when the host configures the
/// device or the keyboard's interface again was never taken: after a
/// release whose first neutral report the host discarded (its toggle was
/// unknown after a lost ACK), the second, still queued, is dropped, and the
/// release still reaches the host before the next press.
#[test]
fn a_reconfiguration_keeps_the_release_owed() {
    for reinit in [
        &[Event::Configure][..],
        &[Event::SetInterface(Ep::Keyboard)],
        &[Event::Unconfigure, Event::Configure],
    ] {
        let mut w = World::new(Rate::Hz125, 3_000);
        let ep = Ep::Keyboard;
        w.lease();
        assert_eq!(w.send("key 115 1"), "ok");
        w.lose_ack[index(ep)] = 1;
        w.run_for(10_000);
        assert_eq!(w.send("release"), "ok");
        w.run_for(8_000);
        assert_eq!(w.discarded, 1, "{reinit:?}");
        assert!(w.hw.buffer(ep).available, "{reinit:?}");
        for (k, &event) in (1..).zip(reinit) {
            w.events.push((w.now() + 1_000 * k, event));
        }
        w.lease();
        assert_eq!(w.send("key 115 1"), "ok");
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
    w.lease();
    assert_eq!(w.send("key 115 1"), "ok");
    w.lose_ack[index(ep)] = 1;
    w.run_for(10_000);
    assert!(w.hw.buffer(ep).available, "the ACK was lost");
    w.event(Event::SetInterface(ep));
    assert_eq!(w.send("key 4 1"), "ok");
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
/// again before the next press.
#[test]
fn a_bus_reset_keeps_the_release_owed() {
    let mut w = World::new(Rate::Hz125, 3_000);
    let ep = Ep::Keyboard;
    assert_eq!(w.send("trace on"), "ok");
    w.lease();
    assert_eq!(w.send("key 115 1"), "ok");
    // The host takes the press at its poll at 27 ms; the reset comes in
    // the same step, before the writer sees the completion.
    w.events.push((27_000, Event::Reset));
    w.run_until(30_000);
    assert!(
        w.hw.buffer(ep).available,
        "the release handed while disabled"
    );
    w.event(Event::Configure);
    w.lease();
    assert_eq!(w.send("key 115 1"), "ok");
    w.run_for(20_000);
    assert_eq!(w.seen_from(ep, 0), [(F24, true), (F24, false), (F24, true)]);
    let trace = w.trace();
    let press = trace
        .reports
        .iter()
        .find(|((e, _), r)| *e == ep && r.revoked == Some((false, 17)))
        .map(|(_, r)| r.completion);
    assert!(
        matches!(press, Some(Some(_))),
        "the press taken before the reset keeps its completion: {press:?}"
    );
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
    w.lease();
    assert_eq!(w.send("key 115 1"), "ok");
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
    w.lease();
    assert_eq!(w.send("button 1 1"), "ok");
    w.run_for(10_000);
    // Handed between polls, then revoked.
    assert_eq!(w.send("button 2 1"), "ok");
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
    assert_eq!(
        w.seen_from(Ep::Pad, 0),
        [(Control::Button(1), true), (Control::Button(1), false)]
    );
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
        w.lease();
        assert_eq!(w.send("key 115 1"), "ok");
        w.run_for(3_000);
        assert_eq!(w.receive(bytes), [error], "{bytes:?}");
        w.run_for(3_000);
        assert_eq!(
            w.seen_from(Ep::Keyboard, 0),
            [(F24, true), (F24, false)],
            "{bytes:?}"
        );
        let hello = w.send("hello");
        assert!(
            hello.contains(" lease - armed 0 reason protocol "),
            "{hello}"
        );
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
    assert_eq!(w.receive(b"lea"), Vec::<String>::new());
    assert_eq!(w.receive(b"se 10"), Vec::<String>::new());
    let answers = w.receive(b"00\n");
    assert!(answers[0].starts_with("lease 1 "), "{answers:?}");
}

/// A connection that ends while a command's outcome is awaited takes the
/// rest of its packet with it: the port closed and opened again while
/// `flush` waits on a pending report leaves no `detach` to complete.
#[test]
fn a_connection_that_ends_during_a_command_drops_the_rest_of_its_packet() {
    let mut w = World::new(Rate::Hz125, 3_000);
    w.lease();
    assert_eq!(w.send("key 115 1"), "ok");
    w.run_for(1_000);
    assert!(w.hw.buffer(Ep::Keyboard).available, "flush waits");
    let t = w.now();
    w.events.push((t + 1_000, Event::Dtr(false)));
    w.events.push((t + 2_000, Event::Dtr(true)));
    assert_eq!(w.receive(b"flush\ndetach "), Vec::<String>::new());
    assert!(w.events.is_empty() && w.now() > t + 2_000);
    assert_eq!(w.receive(b"500\n"), ["err syntax"]);
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
    w.lease();
    w.run_for(5_000);
    assert_eq!(w.send("key 115 1"), "ok");
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
    w.lease();
    assert_eq!(w.send(&format!("revoke-test {} 1", ep.name())), "ok");
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
    assert_eq!(
        after,
        [
            (Control::Button(1), true),
            (Control::Button(1), false),
            (Control::Button(2), true)
        ]
    );
}

/// The trial is refused outside 125 Hz on change, without a lease, and
/// while something plays.
#[test]
fn a_trial_needs_its_mode_and_an_idle_lease() {
    let mut w = World::new(Rate::Hz1000, 300);
    w.lease();
    assert_eq!(w.send("revoke-test k 1"), "err mode");
    let mut w = World::new(Rate::Hz125, 3_000);
    assert_eq!(w.send("revoke-test k 1"), "err no-lease");
    w.lease();
    assert_eq!(w.send("key 4 1"), "ok");
    w.run_for(10_000);
    assert_eq!(w.send("revoke-test k 1"), "err playing", "a key held");
    assert_eq!(w.send("revoke-test p 1"), "ok");
    assert_eq!(w.send("item 0 b1+"), "err playing");
}
