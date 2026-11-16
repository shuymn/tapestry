//! The device's state and what changes it: held input, the pattern, the
//! lease and its disarm, each endpoint's association and the trace
//! (docs/test-device.md).

use embassy_rp::pac;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Instant;
use heapless::Deque;
use test_device::boot::Cause;
use test_device::faults::Fault;
#[cfg(feature = "debug-faults")]
use test_device::faults::Faults;
use test_device::lease::{Lease, Reason};
use test_device::mode::{Mode, Rate, Reporting};
use test_device::protocol::{Change, Control};
use test_device::report::{KEYBOARD_REPORT, KeyboardReport, PAD_REPORT, PadReport};
use test_device::schedule::Schedule;
use test_device::trace::{Association, Body, Done, Ep, Ring};

/// The keyboard's IN endpoint: the HID class takes the first free IN
/// endpoint (embassy-rp `src/usb.rs`), and the keyboard is built first.
/// Checked at its first write by the buffer's LENGTH.
pub const KEYBOARD_EP: usize = 1;
/// The pad's IN endpoint, built second; checked the same way.
pub const PAD_EP: usize = 2;
/// Trace records the firmware buffers.
pub const RING: usize = 1024;
/// Pattern items per endpoint.
pub const ITEMS: usize = 1024;
/// Held changes waiting for their reports, per endpoint.
pub const CHANGES: usize = 32;

/// Wakes an endpoint's writer: a change, an armed pattern, a disarm.
pub static KICK_KEYBOARD: Signal<CriticalSectionRawMutex, ()> = Signal::new();
pub static KICK_PAD: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Wakes the link's sender: trace records wait.
pub static KICK_TRACE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub fn kick(ep: Ep) -> &'static Signal<CriticalSectionRawMutex, ()> {
    match ep {
        Ep::Keyboard => &KICK_KEYBOARD,
        Ep::Pad => &KICK_PAD,
    }
}

/// The µs since boot.
pub fn now() -> u64 {
    Instant::now().as_micros()
}

/// The DPRAM buffer control of IN endpoint `index`.
fn buffer(
    index: usize,
) -> pac::common::Reg<pac::usb_dpram::regs::EpBufferControl, pac::common::RW> {
    pac::USB_DPRAM.ep_in_buffer_control(index)
}

/// Whether IN endpoint `index`'s buffer is still handed to the
/// controller: the controller clears AVAILABLE when the host's IN token
/// takes the report.
pub fn available(index: usize) -> bool {
    buffer(index).read().available(0)
}

/// One HID endpoint's state.
pub struct Endpoint {
    pub assoc: Association,
    /// R of the report being built.
    pub ready: u64,
    /// Whether the pending report is neutral.
    pub pending_neutral: bool,
    /// Whether the last report the host took was neutral.
    pub delivered_neutral: bool,
    /// The last completed report, carried in the next pad report.
    pub last_done: Option<Done>,
    /// Send the current state even without a change: after a disarm, and
    /// once at configuration (which identifies the endpoint).
    pub dirty: bool,
    /// Held changes from commands, each sent as its own report.
    pub changes: Deque<Change, CHANGES>,
    /// Whether the buffer's LENGTH after this endpoint's first write was its
    /// report's.
    pub identified: bool,
}

impl Endpoint {
    const fn new() -> Self {
        Self {
            assoc: Association::starting_at(0),
            ready: 0,
            pending_neutral: true,
            delivered_neutral: true,
            last_done: None,
            dirty: false,
            changes: Deque::new(),
            identified: false,
        }
    }
}

/// What an endpoint's writer does next.
pub enum Next {
    /// Send a report now.
    Now,
    /// Nothing until this time (µs since boot), or a kick.
    At(u64),
    /// Nothing until a kick.
    Idle,
}

pub struct State {
    pub mode: Mode,
    pub lease: Lease,
    pub schedule: Schedule<ITEMS>,
    pub keys: KeyboardReport,
    pub buttons: u16,
    pub axis: u8,
    pub keyboard: Endpoint,
    pub pad: Endpoint,
    pub ring: Ring<RING>,
    pub tracing: bool,
    /// Added to every time the device reports (the clock-offset fault).
    pub offset: u64,
    pub boot: Cause,
    pub count: u8,
    pub hello_answered: bool,
    pub counter_cleared: bool,
    #[cfg(feature = "debug-faults")]
    pub faults: Faults,
}

impl State {
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
            #[cfg(feature = "debug-faults")]
            faults: Faults::new(),
        }
    }

    /// A time as the device reports it.
    pub const fn out(&self, t: u64) -> u64 {
        t.wrapping_add(self.offset)
    }

    /// A reported time as µs since boot.
    pub const fn inward(&self, t: u64) -> u64 {
        t.wrapping_sub(self.offset)
    }

    /// `fault`'s argument while it is on; never in a build without
    /// `debug-faults`.
    #[cfg(feature = "debug-faults")]
    pub fn fault(&self, fault: Fault) -> Option<i64> {
        self.faults.on(fault, now())
    }

    #[cfg(not(feature = "debug-faults"))]
    pub const fn fault(&self, _fault: Fault) -> Option<i64> {
        None
    }

    pub const fn ep(&mut self, ep: Ep) -> &mut Endpoint {
        match ep {
            Ep::Keyboard => &mut self.keyboard,
            Ep::Pad => &mut self.pad,
        }
    }

    pub const fn identified(&self) -> bool {
        self.keyboard.identified && self.pad.identified
    }

    /// Buffers a trace record while tracing.
    pub fn trace(&mut self, body: Body) {
        if self.tracing {
            self.ring.push(body);
            KICK_TRACE.signal(());
        }
    }

    /// The mode byte the pad reports.
    pub fn mode_byte(&self) -> u8 {
        let mut mode = self.mode;
        if self.fault(Fault::ModeByteOnly).is_some() {
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

    /// Takes what `ep` sends next at `t` (µs since boot), applying its
    /// change to the held state: a held change first, then a due pattern
    /// item, then a requested resend, then a continuous report.
    pub fn next_report(&mut self, ep: Ep, t: u64) -> Next {
        if let Some(c) = self.ep(ep).changes.pop_front() {
            self.apply(c);
            return Next::Now;
        }
        while let Some((index, item)) = self.schedule.take_due(ep, t) {
            if self.fault(Fault::DropItem) == Some(i64::from(index)) {
                continue;
            }
            let changes = item.changes();
            let split = self.fault(Fault::Split).is_some();
            for (i, &c) in changes.iter().enumerate() {
                if i == 0 || !split {
                    self.apply(c);
                } else {
                    // Each later change as a report of its own, in order.
                    let _ = self.ep(ep).changes.push_back(c);
                }
            }
            return Next::Now;
        }
        if core::mem::take(&mut self.ep(ep).dirty) {
            return Next::Now;
        }
        if ep == Ep::Pad
            && self.mode.reporting == Reporting::Continuous
            && self.lease.held().is_some()
        {
            return Next::Now;
        }
        match self.schedule.next_due_at(ep) {
            Some(at) => Next::At(at),
            None => Next::Idle,
        }
    }

    /// The pad report to send now, fixing its R.
    pub fn pad_report(&mut self) -> [u8; PAD_REPORT] {
        let ready = self.out(now());
        self.pad.ready = ready;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the report carries the low bits"
        )]
        let report = PadReport {
            buttons: self.buttons,
            axis: self.axis,
            mode: self.mode_byte(),
            seq: self.pad.assoc.next_seq() as u16,
            ready: ready as u32,
            previous: self
                .pad
                .last_done
                .map(|d| (d.handoff as u32, d.complete as u32)),
        };
        report.encode()
    }

    /// The keyboard report to send now, fixing its R.
    pub fn keyboard_report(&mut self) -> [u8; KEYBOARD_REPORT] {
        self.keyboard.ready = self.out(now());
        *self.keys.bytes()
    }

    /// `ep`'s report went to the controller just now.
    pub fn handed(&mut self, ep: Ep, index: usize, neutral: bool, len: usize) {
        let h = self.out(now());
        let at_write = self.fault(Fault::CompletionAtWrite).is_some();
        let e = self.ep(ep);
        let (seq, _) = e.assoc.handed(h);
        e.pending_neutral = neutral;
        if !e.identified {
            e.identified = usize::from(buffer(index).read().length(0)) == len;
        }
        let ready = e.ready;
        self.trace(Body::Handoff {
            ep,
            seq,
            ready,
            handoff: h,
        });
        if at_write {
            // The fault: the handoff taken for the completion.
            self.completed(ep, h);
        }
    }

    fn completed(&mut self, ep: Ep, at: u64) {
        let e = self.ep(ep);
        if let Some(done) = e.assoc.completed(at) {
            e.delivered_neutral = e.pending_neutral;
            e.last_done = Some(done);
            self.trace(Body::Completion {
                ep,
                seq: done.seq,
                at,
            });
        }
    }

    /// Whether `ep` has no report pending, recording its completion when
    /// the controller has just cleared AVAILABLE.
    pub fn poll_completion(&mut self, ep: Ep, index: usize) -> bool {
        if self.ep(ep).assoc.pending().is_none() {
            return true;
        }
        if available(index) {
            return false;
        }
        let at = self.out(now());
        self.completed(ep, at);
        true
    }

    /// Withdraws `ep`'s pending report if the host has not taken it
    /// (docs/test-device.md "Lease and disarm", step 2). Returns whether a
    /// report was revoked.
    fn revoke(&mut self, ep: Ep, index: usize) -> bool {
        let Some(seq) = self.ep(ep).assoc.pending() else {
            return false;
        };
        let usb = pac::USB;
        usb.ep_abort().modify(|w| w.set_ep_in(index, true));
        // EP_ABORT_DONE: the controller will not touch the buffer, so its
        // control register is safe to modify. Bounded: a few µs at most.
        let mut spins = 0_u32;
        while !usb.ep_abort_done().read().ep_in(index) && spins < 100_000 {
            spins += 1;
        }
        let bc = buffer(index).read();
        let taken = !bc.available(0);
        if !taken {
            // Restore the DATA PID of the last report delivered: the next
            // write sends the opposite of what this register holds, which
            // is the toggle the host expects.
            buffer(index).write(|w| {
                w.set_pid(0, !bc.pid(0));
                w.set_length(0, bc.length(0));
            });
            self.ep(ep).assoc.revoked();
        }
        usb.ep_abort().modify(|w| w.set_ep_in(index, false));
        usb.ep_abort_done().write(|w| w.set_ep_in(index, true));
        let at = self.out(now());
        self.trace(Body::Revoked {
            ep,
            seq,
            available: !taken,
            len: bc.length(0),
            at,
        });
        !taken
    }

    /// Ends the lease for `reason`, in the order of docs/test-device.md
    /// "Lease and disarm": drop the pattern and the held input, revoke what
    /// the host has not taken, then send a neutral report wherever the host
    /// last saw something pressed or a report was revoked.
    pub fn disarm(&mut self, reason: Reason) {
        if self.fault(Fault::IgnoreDisarm).is_some() {
            return;
        }
        let bus = matches!(reason, Reason::Detached | Reason::Suspended | Reason::Reset);
        if bus && self.fault(Fault::KeepArmed).is_some() {
            return;
        }
        self.lease.disarm(reason);
        // 1. Disarm.
        self.schedule.clear();
        self.keyboard.changes.clear();
        self.pad.changes.clear();
        self.keys = KeyboardReport::NEUTRAL;
        self.buttons = 0;
        // 2. Revoke, 3. neutral.
        for (ep, index) in [(Ep::Keyboard, KEYBOARD_EP), (Ep::Pad, PAD_EP)] {
            let revoked = self.revoke(ep, index);
            let e = self.ep(ep);
            let pending_pressed = e.assoc.pending().is_some() && !e.pending_neutral;
            if revoked || !e.delivered_neutral || pending_pressed {
                e.dirty = true;
                kick(ep).signal(());
            }
        }
    }
}

/// The boot-hang fault: hang at this boot if the scratch register asks,
/// counting the hang down first (docs/test-device.md "Debug faults").
#[cfg(feature = "debug-faults")]
pub fn boot_hang(watchdog: &mut embassy_rp::watchdog::Watchdog, cause: Cause) {
    const TAG: u32 = 0xB4A6_0000;
    let word = watchdog.get_scratch(2);
    if cause == Cause::PowerOn || word & 0xFFFF_0000 != TAG {
        watchdog.set_scratch(2, 0);
        return;
    }
    let left = word & 0xFFFF;
    if left > 0 {
        watchdog.set_scratch(2, TAG | (left - 1));
        hang();
    }
}

/// Arms the boot-hang fault for the next `boots` boots.
#[cfg(feature = "debug-faults")]
pub fn set_boot_hang(watchdog: &mut embassy_rp::watchdog::Watchdog, boots: u16) {
    watchdog.set_scratch(2, 0xB4A6_0000 | u32::from(boots));
}

/// Spins without yielding, so the watchdog fires.
#[cfg(feature = "debug-faults")]
pub fn hang() -> ! {
    loop {
        core::hint::spin_loop();
    }
}
