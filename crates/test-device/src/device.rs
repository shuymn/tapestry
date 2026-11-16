//! The device's logic (docs/test-device.md): held input, the pattern, the
//! lease and its disarm, report revocation, each endpoint's association,
//! the trace, the control link's framing and the revocation trial. The
//! firmware runs it against the RP2350's USB controller through
//! [`Hardware`]; the tests run it against a simulated controller and host.

use crate::boot::Cause;
use crate::faults::Fault;
#[cfg(feature = "debug-faults")]
use crate::faults::Faults;
use crate::lease::{Lease, Reason, Refusal};
use crate::mode::{Mode, Rate, Reporting};
use crate::protocol::{Answer, Change, Command, Control, Error, Hello, Item, MAX_LINE, VERSION};
use crate::report::{KEYBOARD_REPORT, KeyboardReport, PAD_REPORT, PadReport};
use crate::revoke::{self, DISARM_AFTER_C0, K1_AFTER_C0, PROBE_AFTER_K1};
use crate::schedule::{Refused, Schedule};
use crate::trace::{Association, Body, Done, Ep, Ring, TrialRun};

/// Trace records the device buffers.
pub const RING: usize = 1024;
/// Pattern items per endpoint.
pub const ITEMS: usize = 1024;
/// Held changes waiting for their reports, per endpoint.
pub const CHANGES: usize = 32;
/// Reads of `EP_ABORT_DONE` before an abort counts as unacknowledged; the
/// controller acknowledges within a few µs.
pub const ABORT_POLLS: u32 = 100_000;
/// How long the device keeps running after an unacknowledged abort before
/// it reboots, so the link can carry the `A` record.
pub const RECOVERY_US: u64 = 20_000;

/// An IN endpoint's buffer control in the controller's DPRAM.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Buffer {
    /// Handed to the controller; it clears the bit when the host takes the
    /// report.
    pub available: bool,
    /// The DATA PID of the report in the buffer, DATA1 when set. A write
    /// sends the opposite of what the register holds.
    pub pid: bool,
    pub length: u16,
}

/// What the logic needs from the USB controller, the clock and the tasks
/// that wait on it.
pub trait Hardware {
    /// µs since boot.
    fn now(&self) -> u64;
    fn buffer(&self, ep: Ep) -> Buffer;
    /// Writes `ep`'s buffer control; only after the controller acknowledged
    /// an abort of the endpoint.
    fn set_buffer(&self, ep: Ep, buffer: Buffer);
    /// Sets or clears `EP_ABORT` for `ep`.
    fn set_abort(&self, ep: Ep, on: bool);
    /// Whether `EP_ABORT_DONE` is set for `ep`.
    fn abort_done(&self, ep: Ep) -> bool;
    /// Clears `EP_ABORT_DONE` for `ep` (write 1 to clear).
    fn clear_abort_done(&self, ep: Ep);
    /// Wakes `ep`'s writer.
    fn kick(&self, ep: Ep);
    /// Wakes the link's sender: trace records wait.
    fn kick_trace(&self);
}

/// The report length of `ep`.
#[must_use]
pub const fn report_len(ep: Ep) -> usize {
    match ep {
        Ep::Keyboard => KEYBOARD_REPORT,
        Ep::Pad => PAD_REPORT,
    }
}

/// Held changes in order.
#[derive(Debug)]
struct Fifo<const N: usize> {
    buf: [Option<Change>; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Fifo<N> {
    const fn new() -> Self {
        Self {
            buf: [None; N],
            head: 0,
            len: 0,
        }
    }

    fn push_back(&mut self, c: Change) -> bool {
        if self.len == N {
            return false;
        }
        match self.buf.get_mut((self.head + self.len) % N) {
            Some(slot) => {
                *slot = Some(c);
                self.len += 1;
                true
            }
            None => false,
        }
    }

    fn pop_front(&mut self) -> Option<Change> {
        if self.len == 0 {
            return None;
        }
        let c = self.buf.get_mut(self.head).and_then(Option::take);
        self.head = (self.head + 1) % N;
        self.len -= 1;
        c
    }

    const fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn clear(&mut self) {
        *self = Self::new();
    }
}

/// One HID endpoint's state.
#[derive(Debug)]
struct Endpoint {
    assoc: Association,
    /// R of the report being built.
    ready: u64,
    /// Whether the report being built is neutral, and whether it is the
    /// owed neutral report.
    building_neutral: bool,
    building_owed: bool,
    /// Whether the pending report is neutral.
    pending_neutral: bool,
    /// Whether the last report the host took was neutral.
    delivered_neutral: bool,
    /// The last report the host took, carried in the next pad report when
    /// it is that report's predecessor.
    last_done: Option<Done>,
    /// Send the current state once without a change: at configuration,
    /// which identifies the endpoint.
    dirty: bool,
    /// After a disarm: a neutral report the host must take before this
    /// endpoint sends anything else. Only its completion clears it.
    owe_neutral: bool,
    /// The owed neutral report while it is handed.
    neutral_seq: Option<u32>,
    /// Held changes from commands, each sent as its own report.
    changes: Fifo<CHANGES>,
    /// Whether the buffer's LENGTH after this endpoint's first write was its
    /// report's.
    identified: bool,
}

impl Endpoint {
    const fn new() -> Self {
        Self {
            assoc: Association::starting_at(0),
            ready: 0,
            building_neutral: true,
            building_owed: false,
            pending_neutral: true,
            delivered_neutral: true,
            last_done: None,
            dirty: false,
            owe_neutral: false,
            neutral_seq: None,
            changes: Fifo::new(),
            identified: false,
        }
    }
}

/// What a report carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The held state.
    Current,
    /// Everything released, the axis unchanged: the report a disarm owes.
    Neutral,
}

/// What an endpoint's writer does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Send a report of this kind now.
    Now(Kind),
    /// Nothing until this time (µs since boot), or a kick.
    At(u64),
    /// Nothing until a kick.
    Idle,
}

/// What a revocation found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Revocation {
    /// No report was pending.
    Nothing,
    /// The host had taken the pending report.
    Taken,
    /// The pending report was withdrawn before the host took it.
    Revoked,
    /// The controller never acknowledged the abort: nothing was touched.
    Unacknowledged,
}

/// A revocation trial's phase ([`crate::revoke`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// k0, the sentinel alone, goes next.
    Sentinel,
    /// k0 is being written.
    SentinelSent,
    /// k0 handed: waiting for the host to take it.
    AwaitK0 { k0: u32 },
    /// k1 goes at `at`, [`K1_AFTER_C0`] after C(k0) at `c0`.
    Revocable { k0: u32, c0: u64, at: u64 },
    /// k1 is being written.
    RevokedSent { k0: u32, c0: u64 },
    /// k1 handed, the probe armed: the lease expires next.
    Revoking,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Trial {
    ep: Ep,
    number: u32,
    phase: Phase,
}

/// What the firmware does with a command the logic does not finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Send this answer.
    Answer(Answer<'static>),
    /// Answer [`State::hello`].
    Hello,
    /// Store the mode, answer `ok` and reboot.
    Mode(Mode),
    /// Run `flush` ([`State::flush_poll`], then [`State::flush_end`]).
    Flush,
    /// Answer `ok` and reboot into BOOTSEL.
    Bootsel,
    /// Disarmed: answer `ok` and leave the bus for this many ms.
    Detach(u32),
    /// A debug fault ([`State::set_fault`] in a `debug-faults` build).
    Fault { fault: Fault, arg: i64 },
}

/// The control link's line framing: bytes in, lines out. Its state belongs
/// to one session of the link and is cleared when the session ends.
#[derive(Debug)]
pub struct Framer {
    buf: [u8; MAX_LINE],
    len: usize,
    overlong: bool,
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

/// One frame of the link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    /// A line, without its ending.
    Line(&'a [u8]),
    /// A line longer than [`MAX_LINE`], reported at its first byte past it;
    /// the rest of the line is dropped.
    Overlong,
}

impl Framer {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [0; MAX_LINE],
            len: 0,
            overlong: false,
        }
    }

    /// Takes one byte: a frame at its line's end (empty lines give none),
    /// or [`Frame::Overlong`] once per line past [`MAX_LINE`].
    pub fn push(&mut self, b: u8) -> Option<Frame<'_>> {
        if b == b'\n' || b == b'\r' {
            let was_overlong = core::mem::take(&mut self.overlong);
            let n = core::mem::take(&mut self.len);
            if was_overlong || n == 0 {
                return None;
            }
            return Some(Frame::Line(self.buf.get(..n).unwrap_or(&[])));
        }
        if self.overlong {
            return None;
        }
        match self.buf.get_mut(self.len) {
            Some(slot) => {
                *slot = b;
                self.len += 1;
                None
            }
            None => {
                self.overlong = true;
                self.len = 0;
                Some(Frame::Overlong)
            }
        }
    }

    /// Forgets a partial line: the session that sent it ended (DTR
    /// dropped, the port closed, a read error or a disconnect).
    pub fn reset(&mut self) {
        self.len = 0;
        self.overlong = false;
    }
}

/// The device's state.
#[derive(Debug)]
pub struct State {
    mode: Mode,
    lease: Lease,
    schedule: Schedule<ITEMS>,
    keys: KeyboardReport,
    buttons: u16,
    axis: u8,
    keyboard: Endpoint,
    pad: Endpoint,
    ring: Ring<RING>,
    tracing: bool,
    /// Added to every time the device reports (the clock-offset fault).
    offset: u64,
    boot: Cause,
    count: u8,
    hello_answered: bool,
    counter_cleared: bool,
    trial: Option<Trial>,
    /// When an abort went unacknowledged (µs since boot): nothing is sent
    /// any more and the device reboots [`RECOVERY_US`] later.
    recovery: Option<u64>,
    #[cfg(feature = "debug-faults")]
    faults: Faults,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mode: Mode {
                rate: Rate::Hz1000,
                reporting: Reporting::OnChange,
            },
            lease: Lease::new(),
            schedule: Schedule::new(),
            keys: KeyboardReport::NEUTRAL,
            buttons: 0,
            axis: 0,
            keyboard: Endpoint::new(),
            pad: Endpoint::new(),
            ring: Ring::new(),
            tracing: false,
            offset: 0,
            boot: Cause::PowerOn,
            count: 0,
            hello_answered: false,
            counter_cleared: false,
            trial: None,
            recovery: None,
            #[cfg(feature = "debug-faults")]
            faults: Faults::new(),
        }
    }

    /// The boot's mode, cause and consecutive watchdog boots.
    pub const fn set_boot(&mut self, mode: Mode, cause: Cause, count: u8) {
        self.mode = mode;
        self.boot = cause;
        self.count = count;
    }

    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// A time as the device reports it.
    #[must_use]
    pub const fn out(&self, t: u64) -> u64 {
        t.wrapping_add(self.offset)
    }

    /// A reported time as µs since boot.
    #[must_use]
    pub const fn inward(&self, t: u64) -> u64 {
        t.wrapping_sub(self.offset)
    }

    /// `fault`'s argument while it is on; never without `debug-faults`.
    #[cfg(feature = "debug-faults")]
    pub fn fault(&self, hw: &impl Hardware, fault: Fault) -> Option<i64> {
        self.faults.on(fault, hw.now())
    }

    /// `fault`'s argument while it is on; never without `debug-faults`.
    #[cfg(not(feature = "debug-faults"))]
    pub fn fault(&self, _hw: &impl Hardware, _fault: Fault) -> Option<i64> {
        None
    }

    /// Sets a debug fault that is not handled by the firmware itself.
    #[cfg(feature = "debug-faults")]
    pub fn set_fault(&mut self, hw: &impl Hardware, fault: Fault, arg: i64) {
        let t = hw.now();
        self.faults.set(fault, arg, t);
        match fault {
            Fault::Suspend => self.disarm(hw, Reason::Suspended),
            // The reported µs start 10 s before 2^32.
            Fault::ClockOffset => self.offset = ((1_u64 << 32) - 10_000_000).saturating_sub(t),
            _ => {}
        }
    }

    const fn ep(&mut self, ep: Ep) -> &mut Endpoint {
        match ep {
            Ep::Keyboard => &mut self.keyboard,
            Ep::Pad => &mut self.pad,
        }
    }

    const fn ep_ref(&self, ep: Ep) -> &Endpoint {
        match ep {
            Ep::Keyboard => &self.keyboard,
            Ep::Pad => &self.pad,
        }
    }

    /// Whether both endpoints are identified by their buffers' LENGTH.
    #[must_use]
    pub const fn identified(&self) -> bool {
        self.keyboard.identified && self.pad.identified
    }

    /// Whether an abort went unacknowledged: the device sends nothing more.
    #[must_use]
    pub const fn recovering(&self) -> bool {
        self.recovery.is_some()
    }

    /// Whether the device should reboot now to recover.
    #[must_use]
    pub fn recovery_due(&self, now: u64) -> bool {
        self.recovery
            .is_some_and(|since| now.saturating_sub(since) >= RECOVERY_US)
    }

    /// Whether the boot counter should be cleared now: once, after a
    /// healthy run ([`crate::boot::healthy`]).
    pub fn clear_counter_due(&mut self, now: u64) -> bool {
        if self.counter_cleared || !crate::boot::healthy(now, self.hello_answered) {
            return false;
        }
        self.counter_cleared = true;
        true
    }

    /// Buffers a trace record while tracing.
    fn trace(&mut self, hw: &impl Hardware, body: Body) {
        if self.tracing {
            self.ring.push(body);
            hw.kick_trace();
        }
    }

    /// The oldest buffered trace record.
    pub fn next_record(&mut self) -> Option<crate::trace::Record> {
        self.ring.pop()
    }

    /// The mode byte the pad reports.
    fn mode_byte(&self, hw: &impl Hardware) -> u8 {
        let mut mode = self.mode;
        if self.fault(hw, Fault::ModeByteOnly).is_some() {
            mode.rate = match mode.rate {
                Rate::Hz1000 => Rate::Hz125,
                Rate::Hz125 => Rate::Hz1000,
            };
        }
        mode.byte()
    }

    fn apply(&mut self, c: Change) {
        match c.control {
            Control::Key(usage) => {
                self.keys.set(usage, c.value != 0);
            }
            Control::Button(n) => {
                let bit = 1_u16 << (n.clamp(1, 16) - 1);
                if c.value != 0 {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
            }
            Control::Axis => self.axis = c.value,
        }
    }

    fn held_neutral(&self, ep: Ep) -> bool {
        match ep {
            Ep::Keyboard => self.keys.is_neutral(),
            Ep::Pad => self.buttons == 0,
        }
    }

    /// The bus configured the device: one report on each endpoint, the
    /// current (neutral) state, whose buffer LENGTH identifies it.
    pub fn configured(&mut self, hw: &impl Hardware) {
        for ep in [Ep::Keyboard, Ep::Pad] {
            self.ep(ep).dirty = true;
            hw.kick(ep);
        }
    }

    /// What `ep` sends next, applying its change to the held state. A
    /// neutral report a disarm owes goes before anything else and holds
    /// everything back until the host takes it; then the revocation
    /// trial's report, a held change, a due pattern item, a requested
    /// resend, a continuous report.
    pub fn next_report(&mut self, hw: &impl Hardware, ep: Ep) -> Next {
        if self.recovering() {
            return Next::Idle;
        }
        let t = hw.now();
        let e = self.ep_ref(ep);
        if e.owe_neutral {
            return if e.neutral_seq.is_none() {
                Next::Now(Kind::Neutral)
            } else {
                Next::Idle
            };
        }
        if let Some(next) = self.trial_next(ep, t) {
            return next;
        }
        if let Some(c) = self.ep(ep).changes.pop_front() {
            self.apply(c);
            return Next::Now(Kind::Current);
        }
        while let Some((index, item)) = self.schedule.take_due(ep, t) {
            if self.fault(hw, Fault::DropItem) == Some(i64::from(index)) {
                continue;
            }
            let split = self.fault(hw, Fault::Split).is_some();
            for (i, &c) in item.changes().iter().enumerate() {
                if i == 0 || !split {
                    self.apply(c);
                } else {
                    // Each later change as a report of its own, in order.
                    let _ = self.ep(ep).changes.push_back(c);
                }
            }
            return Next::Now(Kind::Current);
        }
        if core::mem::take(&mut self.ep(ep).dirty) {
            return Next::Now(Kind::Current);
        }
        if ep == Ep::Pad
            && self.mode.reporting == Reporting::Continuous
            && self.lease.held().is_some()
        {
            return Next::Now(Kind::Current);
        }
        match self.schedule.next_due_at(ep) {
            Some(at) => Next::At(at),
            None => Next::Idle,
        }
    }

    /// The trial's next report on `ep`, or `None` when the trial does not
    /// decide it.
    fn trial_next(&mut self, ep: Ep, t: u64) -> Option<Next> {
        let trial = self.trial.filter(|trial| trial.ep == ep)?;
        let c = revoke::controls(ep);
        let (phase, next) = match trial.phase {
            Phase::Sentinel => {
                self.apply(Change {
                    control: c.sentinel,
                    value: 1,
                });
                (Phase::SentinelSent, Next::Now(Kind::Current))
            }
            Phase::Revocable { k0, c0, at } if t >= at => {
                self.apply(Change {
                    control: c.revoked,
                    value: 1,
                });
                (Phase::RevokedSent { k0, c0 }, Next::Now(Kind::Current))
            }
            Phase::Revocable { at, .. } => (trial.phase, Next::At(at)),
            // The probe plays from the schedule, as any armed item.
            Phase::Revoking => return None,
            Phase::SentinelSent | Phase::AwaitK0 { .. } | Phase::RevokedSent { .. } => {
                (trial.phase, Next::Idle)
            }
        };
        self.trial = Some(Trial { phase, ..trial });
        Some(next)
    }

    /// The pad report to send now, fixing its R. It carries report k−1's H
    /// and C only when k−1 is the last report the host took.
    pub fn pad_report(&mut self, hw: &impl Hardware, kind: Kind) -> [u8; PAD_REPORT] {
        let ready = self.out(hw.now());
        let mode = self.mode_byte(hw);
        let buttons = match kind {
            Kind::Current => self.buttons,
            Kind::Neutral => 0,
        };
        let axis = self.axis;
        let e = &mut self.pad;
        e.ready = ready;
        e.building_neutral = buttons == 0;
        e.building_owed = kind == Kind::Neutral;
        let seq = e.assoc.next_seq();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the report carries the low bits"
        )]
        let report = PadReport {
            buttons,
            axis,
            mode,
            seq: seq as u16,
            ready: ready as u32,
            previous: e
                .last_done
                .filter(|d| d.seq == seq.wrapping_sub(1))
                .map(|d| (d.handoff as u32, d.complete as u32)),
        };
        report.encode()
    }

    /// The keyboard report to send now, fixing its R.
    pub fn keyboard_report(&mut self, hw: &impl Hardware, kind: Kind) -> [u8; KEYBOARD_REPORT] {
        let ready = self.out(hw.now());
        let keys = match kind {
            Kind::Current => self.keys,
            Kind::Neutral => KeyboardReport::NEUTRAL,
        };
        let e = &mut self.keyboard;
        e.ready = ready;
        e.building_neutral = keys.is_neutral();
        e.building_owed = kind == Kind::Neutral;
        *keys.bytes()
    }

    /// `ep`'s report went to the controller just now.
    pub fn handed(&mut self, hw: &impl Hardware, ep: Ep) {
        let t = hw.now();
        let h = self.out(t);
        let at_write = self.fault(hw, Fault::CompletionAtWrite).is_some();
        let e = self.ep(ep);
        let (seq, _) = e.assoc.handed(h);
        e.pending_neutral = e.building_neutral;
        if core::mem::take(&mut e.building_owed) {
            e.neutral_seq = Some(seq);
        }
        if !e.identified {
            e.identified = usize::from(hw.buffer(ep).length) == report_len(ep);
        }
        let ready = e.ready;
        self.trace(
            hw,
            Body::Handoff {
                ep,
                seq,
                ready,
                handoff: h,
            },
        );
        self.trial_handed(hw, ep, seq, t);
        if at_write {
            // The fault: the handoff taken for the completion.
            self.completed(hw, ep, h);
        }
    }

    fn trial_handed(&mut self, hw: &impl Hardware, ep: Ep, seq: u32, t: u64) {
        let Some(trial) = self.trial.filter(|trial| trial.ep == ep) else {
            return;
        };
        let phase = match trial.phase {
            Phase::SentinelSent => Phase::AwaitK0 { k0: seq },
            Phase::RevokedSent { k0, c0 } => {
                // The probe, due after the disarm; then the lease expires
                // before the host's next poll.
                let probe = Change {
                    control: revoke::controls(ep).probe,
                    value: 1,
                };
                let due = t.saturating_add(PROBE_AFTER_K1);
                self.schedule.clear();
                if let Some(item) = Item::new(0, &[probe]) {
                    let _ = self.schedule.push(item);
                }
                self.schedule.arm(due);
                if let Some(id) = self.lease.held() {
                    let _ = self.lease.arm(id);
                }
                self.lease.cut(c0.saturating_add(DISARM_AFTER_C0));
                let run = TrialRun {
                    ep,
                    trial: trial.number,
                    k0,
                    k1: seq,
                    probe: self.out(due),
                };
                self.trace(hw, Body::Trial(run));
                Phase::Revoking
            }
            other => other,
        };
        self.trial = Some(Trial { phase, ..trial });
    }

    fn completed(&mut self, hw: &impl Hardware, ep: Ep, at: u64) {
        let e = self.ep(ep);
        let Some(done) = e.assoc.completed(at) else {
            return;
        };
        e.delivered_neutral = e.pending_neutral;
        e.last_done = Some(done);
        if e.neutral_seq == Some(done.seq) {
            e.neutral_seq = None;
            e.owe_neutral = false;
        }
        self.trace(
            hw,
            Body::Completion {
                ep,
                seq: done.seq,
                at,
            },
        );
        if let Some(trial) = self.trial.filter(|trial| trial.ep == ep)
            && let Phase::AwaitK0 { k0 } = trial.phase
            && k0 == done.seq
        {
            let c0 = hw.now();
            self.trial = Some(Trial {
                phase: Phase::Revocable {
                    k0,
                    c0,
                    at: c0.saturating_add(K1_AFTER_C0),
                },
                ..trial
            });
            hw.kick(ep);
        }
    }

    /// Whether `ep` has no report pending, recording its completion when
    /// the controller has just cleared AVAILABLE.
    pub fn poll_completion(&mut self, hw: &impl Hardware, ep: Ep) -> bool {
        if self.ep_ref(ep).assoc.pending().is_none() {
            return true;
        }
        if hw.buffer(ep).available {
            return false;
        }
        let at = self.out(hw.now());
        self.completed(hw, ep, at);
        true
    }

    /// Withdraws `ep`'s pending report if the host has not taken it
    /// (docs/test-device.md "Lease and Disarm", step 2). Without the
    /// controller's acknowledgement nothing is touched or claimed: the
    /// device records `A` and recovers by rebooting.
    fn revoke(&mut self, hw: &impl Hardware, ep: Ep) -> Revocation {
        let Some(seq) = self.ep_ref(ep).assoc.pending() else {
            return Revocation::Nothing;
        };
        hw.set_abort(ep, true);
        let mut polls = 0;
        while !hw.abort_done(ep) {
            if polls == ABORT_POLLS {
                let t = hw.now();
                self.recovery.get_or_insert(t);
                let at = self.out(t);
                self.trace(hw, Body::Unacknowledged { ep, seq, at });
                return Revocation::Unacknowledged;
            }
            polls += 1;
        }
        // Acknowledged: the controller will not touch the buffer.
        let b = hw.buffer(ep);
        if b.available {
            // Withdraw it, restoring the DATA PID of the last report
            // delivered: the next write sends the opposite, the toggle the
            // host expects.
            hw.set_buffer(
                ep,
                Buffer {
                    available: false,
                    pid: !b.pid,
                    length: b.length,
                },
            );
            let e = self.ep(ep);
            e.assoc.revoked();
            if e.neutral_seq == Some(seq) {
                e.neutral_seq = None;
            }
        }
        hw.set_abort(ep, false);
        hw.clear_abort_done(ep);
        let at = self.out(hw.now());
        self.trace(
            hw,
            Body::Revoked {
                ep,
                seq,
                available: b.available,
                len: b.length,
                at,
            },
        );
        if b.available {
            Revocation::Revoked
        } else {
            Revocation::Taken
        }
    }

    /// Ends the lease for `reason`, in the order of docs/test-device.md
    /// "Lease and Disarm": drop the pattern and the held input, revoke what
    /// the host has not taken, then owe a neutral report wherever the host
    /// last saw something pressed or a report was revoked.
    pub fn disarm(&mut self, hw: &impl Hardware, reason: Reason) {
        if self.fault(hw, Fault::IgnoreDisarm).is_some() {
            return;
        }
        let bus = matches!(reason, Reason::Detached | Reason::Suspended | Reason::Reset);
        if bus && self.fault(hw, Fault::KeepArmed).is_some() {
            return;
        }
        self.lease.disarm(reason);
        self.trial = None;
        // 1. Drop the pattern, the held changes and the held state.
        self.schedule.clear();
        self.keyboard.changes.clear();
        self.pad.changes.clear();
        self.keys = KeyboardReport::NEUTRAL;
        self.buttons = 0;
        // 2. Revoke; 3. owe a neutral report.
        for ep in [Ep::Keyboard, Ep::Pad] {
            let revocation = self.revoke(hw, ep);
            let e = self.ep(ep);
            let pending_pressed = e.assoc.pending().is_some() && !e.pending_neutral;
            if revocation == Revocation::Revoked || !e.delivered_neutral || pending_pressed {
                e.owe_neutral = true;
                hw.kick(ep);
            }
        }
    }

    /// The lease's own timer: disarms at the deadline.
    pub fn expire(&mut self, hw: &impl Hardware) {
        if self.lease.expired(hw.now()) {
            self.disarm(hw, Reason::Expired);
        }
    }

    /// One frame from the control link. A frame that is not a command ends
    /// the lease ([`Reason::Protocol`]) and is answered with its error.
    /// `None`: the parser-off fault drops it.
    pub fn frame(&mut self, hw: &impl Hardware, frame: Frame<'_>) -> Option<Outcome> {
        if self.fault(hw, Fault::ParserOff).is_some() {
            return None;
        }
        let parsed = match frame {
            Frame::Line(bytes) => core::str::from_utf8(bytes)
                .ok()
                .and_then(Command::parse)
                .ok_or(Error::Syntax),
            Frame::Overlong => Err(Error::Overlong),
        };
        Some(match parsed {
            Ok(cmd) => self.command(hw, cmd),
            Err(e) => {
                self.disarm(hw, Reason::Protocol);
                Outcome::Answer(Answer::Err(e))
            }
        })
    }

    /// The link's session ended: its partial line is dropped, so no command
    /// spans two sessions. `closed`: the host dropped DTR or closed the
    /// port, which ends the lease; a read error or a disconnect leaves that
    /// to the bus events.
    pub fn session_end(&mut self, hw: &impl Hardware, framer: &mut Framer, closed: bool) {
        framer.reset();
        if closed {
            self.disarm(hw, Reason::Closed);
        }
    }

    /// Whether held input and patterns wait: a pattern or a trial plays.
    const fn busy(&self) -> bool {
        self.schedule.armed() || self.trial.is_some()
    }

    /// Runs `cmd`, or says what the firmware does with it.
    pub fn command(&mut self, hw: &impl Hardware, cmd: Command) -> Outcome {
        let answer = match cmd {
            Command::Hello => return Outcome::Hello,
            Command::Mode(mode) => return Outcome::Mode(mode),
            Command::Flush => return Outcome::Flush,
            Command::Bootsel => return Outcome::Bootsel,
            Command::Fault { fault, arg } => return Outcome::Fault { fault, arg },
            Command::Detach { ms } => {
                self.disarm(hw, Reason::Detached);
                return Outcome::Detach(ms);
            }
            Command::Lease { ms } => {
                let granted = self.lease.grant(hw.now(), ms);
                hw.kick(Ep::Pad);
                match granted {
                    Ok((id, deadline)) => Answer::Leased {
                        id,
                        deadline: self.out(deadline),
                    },
                    Err(r) => Answer::Err(Error::Lease(r)),
                }
            }
            Command::Renew { id } => match self.lease.renew(id, hw.now()) {
                Ok(deadline) => Answer::Leased {
                    id,
                    deadline: self.out(deadline),
                },
                Err(r) => Answer::Err(Error::Lease(r)),
            },
            Command::Set(change) => {
                let ep = change.control.ep();
                if self.lease.held().is_none() {
                    Answer::Err(Error::Lease(Refusal::NoLease))
                } else if self.busy() {
                    Answer::Err(Error::Playing)
                } else if self.ep(ep).changes.push_back(change) {
                    hw.kick(ep);
                    Answer::Ok
                } else {
                    Answer::Err(Error::Pattern)
                }
            }
            Command::Load => {
                if self.busy() {
                    Answer::Err(Error::Playing)
                } else {
                    self.schedule.clear();
                    Answer::Ok
                }
            }
            Command::Item(_) if self.trial.is_some() => Answer::Err(Error::Playing),
            Command::Item(item) => match self.schedule.push(item) {
                Ok(()) => Answer::Ok,
                Err(Refused::Armed) => Answer::Err(Error::Playing),
                Err(Refused::Full | Refused::Order) => Answer::Err(Error::Pattern),
            },
            Command::Arm { id, at } => {
                if !self.identified() {
                    Answer::Err(Error::Unidentified)
                } else if self.trial.is_some() {
                    Answer::Err(Error::Playing)
                } else {
                    match self.lease.arm(id) {
                        Ok(()) => {
                            let start = self.inward(at);
                            self.schedule.arm(start);
                            hw.kick(Ep::Keyboard);
                            hw.kick(Ep::Pad);
                            Answer::Ok
                        }
                        Err(r) => Answer::Err(Error::Lease(r)),
                    }
                }
            }
            Command::Ping(n) => {
                let shift = self.fault(hw, Fault::PongOffset).unwrap_or(0);
                Answer::Pong {
                    n,
                    at: self.out(hw.now()).wrapping_add_signed(shift),
                }
            }
            Command::Trace(on) => {
                if on && !self.identified() {
                    Answer::Err(Error::Unidentified)
                } else {
                    self.tracing = on;
                    Answer::Ok
                }
            }
            Command::Release => {
                self.disarm(hw, Reason::Released);
                Answer::Ok
            }
            Command::RevokeTest { ep, trial } => self.start_trial(hw, ep, trial),
        };
        Outcome::Answer(answer)
    }

    /// Starts revocation trial `number` on `ep` under the held lease, which
    /// the trial ends: 125 Hz on change, nothing armed, held or owed.
    fn start_trial(&mut self, hw: &impl Hardware, ep: Ep, number: u32) -> Answer<'static> {
        let wanted = Mode {
            rate: Rate::Hz125,
            reporting: Reporting::OnChange,
        };
        if !self.identified() {
            return Answer::Err(Error::Unidentified);
        }
        if self.mode != wanted {
            return Answer::Err(Error::Mode);
        }
        if self.lease.held().is_none() {
            return Answer::Err(Error::Lease(Refusal::NoLease));
        }
        let e = self.ep_ref(ep);
        let idle = e.changes.is_empty() && !e.owe_neutral && e.assoc.pending().is_none();
        if self.busy() || self.lease.armed() || !idle || !self.held_neutral(ep) {
            return Answer::Err(Error::Playing);
        }
        self.trial = Some(Trial {
            ep,
            number,
            phase: Phase::Sentinel,
        });
        hw.kick(ep);
        Answer::Ok
    }

    /// The `hello` answer; a `hello` answered counts toward a healthy run.
    pub fn hello<'a>(&mut self, build: &'a str) -> Hello<'a> {
        self.hello_answered = true;
        Hello {
            version: VERSION,
            build,
            mode: self.mode,
            lease: self.lease.held(),
            armed: self.lease.armed(),
            reason: self.lease.reason(),
            boot: self.boot,
            count: self.count,
            identified: self.identified(),
        }
    }

    /// `flush`'s wait: whether no endpoint has a report pending.
    pub fn flush_poll(&mut self, hw: &impl Hardware) -> bool {
        let keyboard = self.poll_completion(hw, Ep::Keyboard);
        let pad = self.poll_completion(hw, Ep::Pad);
        keyboard && pad
    }

    /// `flush`'s end: `P` for each report still pending, after every
    /// buffered record, then `F`.
    pub fn flush_end(&mut self, hw: &impl Hardware) {
        for ep in [Ep::Keyboard, Ep::Pad] {
            if let Some(seq) = self.ep_ref(ep).assoc.pending() {
                self.ring.push(Body::Pending { ep, seq });
            }
        }
        self.ring.push(Body::Flushed);
        hw.kick_trace();
    }
}

#[cfg(test)]
mod tests;
