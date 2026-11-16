//! The device's logic against a simulated controller and host: the host
//! polls each endpoint at the report interval, takes a report whose
//! AVAILABLE is set unless the endpoint is aborting, and discards a report
//! whose DATA PID repeats the last one it accepted, as a USB host does with
//! a duplicate. The writers follow the firmware's loops: wait for the next
//! report, write it as embassy-rp does (the opposite PID of the register,
//! then AVAILABLE), then wait for its completion.

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

struct World {
    hw: Sim,
    state: Box<State>,
    framer: Framer,
    /// Each endpoint's writer waits for a completion.
    writing: [bool; 2],
    /// The host's first poll and its interval.
    phase: u64,
    interval: u64,
    /// The DATA PID the host expects next on each endpoint.
    expect: [bool; 2],
    accepted: Vec<Accepted>,
    discarded: usize,
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
            framer: Framer::new(),
            writing: [false; 2],
            phase,
            interval: u64::from(rate.interval_us()),
            expect: [false; 2],
            accepted: Vec::new(),
            discarded: 0,
            records: Vec::new(),
        };
        w.state.configured(&w.hw);
        w.run_for(2 * w.interval + phase);
        assert!(w.state.identified());
        w
    }

    fn now(&self) -> u64 {
        self.hw.t.get()
    }

    fn poll(&mut self, ep: Ep) {
        let i = index(ep);
        if self.hw.abort.get()[i] {
            return;
        }
        let mut buffers = self.hw.buffers.borrow_mut();
        let b = &mut buffers[i];
        if !b.control.available {
            return;
        }
        b.control.available = false;
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

    /// Bytes from the link, in one packet, with no step between the
    /// commands they hold: the answers.
    fn receive(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut answers = Vec::new();
        for &b in bytes {
            let outcome = match self.framer.push(b) {
                Some(frame) => self.state.frame(&self.hw, frame),
                None => continue,
            };
            match outcome {
                Some(Outcome::Answer(a)) => answers.push(a.to_string()),
                Some(Outcome::Hello) => {
                    answers.push(Answer::Hello(self.state.hello("sim")).to_string());
                }
                Some(Outcome::Flush) => {
                    let deadline = self.now() + 2 * self.interval + 10_000;
                    while !self.state.flush_poll(&self.hw) && self.now() < deadline {
                        self.step();
                    }
                    self.state.flush_end(&self.hw);
                    self.drain();
                }
                Some(other) => answers.push(format!("{other:?}")),
                None => answers.push("(dropped)".into()),
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

/// A partial line does not survive the session that sent it: its rest,
/// arriving in the next session, is not a command.
#[test]
fn a_partial_line_does_not_survive_its_session() {
    for closed in [false, true] {
        let mut w = World::new(Rate::Hz1000, 300);
        assert_eq!(w.receive(b"detach "), Vec::<String>::new());
        let mut framer = std::mem::take(&mut w.framer);
        w.state.session_end(&w.hw, &mut framer, closed);
        w.framer = framer;
        assert_eq!(w.receive(b"500\n"), ["err syntax"], "closed {closed}");
    }
    // Within one session a line spans packets.
    let mut w = World::new(Rate::Hz1000, 300);
    assert_eq!(w.receive(b"lea"), Vec::<String>::new());
    assert_eq!(w.receive(b"se 10"), Vec::<String>::new());
    let answers = w.receive(b"00\n");
    assert!(answers[0].starts_with("lease 1 "), "{answers:?}");
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
