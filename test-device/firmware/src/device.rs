//! The RP2350's side of the device logic ([`test_device::device`]): the
//! USB controller's buffer controls and aborts, the handoff of a report or
//! a link packet to it, the clock, and the signals that wake the writers
//! and the link's sender.

use core::cell::RefCell;
use core::sync::atomic::{Ordering, compiler_fence};

use embassy_rp::pac;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Instant;
#[cfg(feature = "debug-faults")]
use test_device::boot::Cause;
use test_device::device::{ABORT_POLLS, Buffer, Connection, Hardware, Kind, LinkWrite, State};
use test_device::trace::Ep;

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

/// The IN endpoint index of `ep`: the HID class takes the first free IN
/// endpoint (embassy-rp `src/usb.rs`) and the keyboard is built first, then
/// the pad. Checked at each endpoint's first write by the buffer's LENGTH.
const fn index(ep: Ep) -> usize {
    match ep {
        Ep::Keyboard => 1,
        Ep::Pad => 2,
    }
}

/// The IN endpoint index of the link's data: the CDC class takes the next
/// free IN endpoints after the pad's, its notification endpoint (3) and
/// then its data endpoint.
const LINK_IN: usize = 4;

/// The DPRAM's size, and where embassy-rp starts the endpoints' data
/// buffers in it (`ep_mem_free` in `src/usb.rs`), each 64 bytes.
const DPRAM_SIZE: usize = 4096;
const BUFFERS_FROM: usize = 0x180;
const BUFFER_LEN: usize = 64;

/// The DPRAM buffer control of IN endpoint `index`.
fn in_control(
    index: usize,
) -> pac::common::Reg<pac::usb_dpram::regs::EpBufferControl, pac::common::RW> {
    pac::USB_DPRAM.ep_in_buffer_control(index)
}

/// The DPRAM buffer control of `ep`.
fn control(ep: Ep) -> pac::common::Reg<pac::usb_dpram::regs::EpBufferControl, pac::common::RW> {
    in_control(index(ep))
}

/// Copies `data` to IN endpoint `index`'s buffer and hands it to the
/// controller, as embassy-rp's `EndpointIn::write` does once the buffer is
/// free: the DATA PID toggled from `last_pid` (the buffer control's), then
/// LENGTH and FULL, 12 cycles, then AVAILABLE. The buffer is the one
/// embassy-rp allocated for the endpoint, its address read from the
/// endpoint control register. Only with AVAILABLE clear. False, nothing
/// written, when `data` or that address does not fit a buffer.
fn hand_in(index: usize, last_pid: bool, data: &[u8]) -> bool {
    let Some(control) = index.checked_sub(1).filter(|&n| n < 15) else {
        return false;
    };
    let Ok(len) = u16::try_from(data.len()) else {
        return false;
    };
    let at = usize::from(
        pac::USB_DPRAM
            .ep_in_control(control)
            .read()
            .buffer_address(),
    );
    if data.len() > BUFFER_LEN || at < BUFFERS_FROM || at + BUFFER_LEN > DPRAM_SIZE {
        return false;
    }
    compiler_fence(Ordering::SeqCst);
    // SAFETY: `at..at + data.len()` lies inside the DPRAM (checked above)
    // in the 64-byte buffer embassy-rp allocated for this endpoint alone;
    // its AVAILABLE is clear (the caller's read), so the controller does
    // not read it, and nothing else in the firmware writes it.
    let buffer = unsafe {
        core::slice::from_raw_parts_mut(pac::USB_DPRAM.as_ptr().cast::<u8>().add(at), data.len())
    };
    buffer.copy_from_slice(data);
    compiler_fence(Ordering::SeqCst);
    let pid = !last_pid;
    let c = in_control(index);
    c.write(|w| {
        w.set_pid(0, pid);
        w.set_length(0, len);
        w.set_full(0, true);
    });
    cortex_m::asm::delay(12);
    c.write(|w| {
        w.set_pid(0, pid);
        w.set_length(0, len);
        w.set_full(0, true);
        w.set_available(0, true);
    });
    true
}

/// Commits `ep`'s staged report of `kind` and hands it to the controller in
/// one critical section: the clock sampled, the deadline checked with the
/// guard band and the report built ([`State::commit`]), then the copy and
/// the AVAILABLE write ([`hand_in`]), with nothing run between. False,
/// nothing committed, when the buffer is not free; the caller records the
/// handoff ([`State::handed`]) after a true.
pub fn commit(state: &RefCell<State>, ep: Ep, kind: Kind) -> bool {
    let index = index(ep);
    critical_section::with(|_| {
        let b = in_control(index).read();
        if b.available(0) {
            return false;
        }
        let now = Pico.now();
        let report = state.borrow_mut().commit(&Pico, ep, kind, now);
        hand_in(index, b.pid(0), report.as_bytes())
    })
}

/// One poll of the link's write of `packet` for `connection`: the
/// connection asked first and the IN buffer read in this poll
/// ([`State::link_write`]), the packet handed in it when both allow
/// (`Some(true)`); `Some(false)` when the connection ended (or the packet
/// does not fit), `None` while the buffer still holds the last packet.
pub fn poll_link_write(
    state: &RefCell<State>,
    connection: Connection,
    packet: &[u8],
) -> Option<bool> {
    let b = in_control(LINK_IN).read();
    match state.borrow().link_write(connection, !b.available(0)) {
        LinkWrite::Write => Some(hand_in(LINK_IN, b.pid(0), packet)),
        LinkWrite::Cancel => Some(false),
        LinkWrite::Wait => None,
    }
}

/// The RP2350's USB controller and timer.
pub struct Pico;

impl Hardware for Pico {
    fn now(&self) -> u64 {
        Instant::now().as_micros()
    }

    fn buffer(&self, ep: Ep) -> Buffer {
        let r = control(ep).read();
        Buffer {
            available: r.available(0),
            pid: r.pid(0),
            length: r.length(0),
        }
    }

    fn set_buffer(&self, ep: Ep, buffer: Buffer) {
        control(ep).write(|w| {
            w.set_available(0, buffer.available);
            w.set_pid(0, buffer.pid);
            w.set_length(0, buffer.length);
        });
    }

    fn set_abort(&self, ep: Ep, on: bool) {
        pac::USB.ep_abort().modify(|w| w.set_ep_in(index(ep), on));
    }

    fn abort_done(&self, ep: Ep) -> bool {
        pac::USB.ep_abort_done().read().ep_in(index(ep))
    }

    fn clear_abort_done(&self, ep: Ep) {
        pac::USB
            .ep_abort_done()
            .write(|w| w.set_ep_in(index(ep), true));
    }

    fn kick(&self, ep: Ep) {
        kick(ep).signal(());
    }

    fn kick_trace(&self) {
        KICK_TRACE.signal(());
    }

    /// As a disarm revokes a report (docs/test-device.md "Lease and
    /// Disarm", step 2): with AVAILABLE set, set `EP_ABORT`, wait for
    /// `EP_ABORT_DONE`, clear AVAILABLE with the DATA PID set back to the
    /// last packet sent, then clear `EP_ABORT`. Without the acknowledgement
    /// nothing is touched, and the device recovers by rebooting.
    fn retire_link_output(&self) -> bool {
        let control = in_control(LINK_IN);
        if !control.read().available(0) {
            return true;
        }
        pac::USB.ep_abort().modify(|w| w.set_ep_in(LINK_IN, true));
        let mut polls = 0;
        while !pac::USB.ep_abort_done().read().ep_in(LINK_IN) {
            if polls == ABORT_POLLS {
                return false;
            }
            polls += 1;
        }
        let b = control.read();
        if b.available(0) {
            control.write(|w| {
                w.set_available(0, false);
                w.set_pid(0, !b.pid(0));
                w.set_length(0, b.length(0));
            });
        }
        pac::USB.ep_abort().modify(|w| w.set_ep_in(LINK_IN, false));
        pac::USB
            .ep_abort_done()
            .write(|w| w.set_ep_in(LINK_IN, true));
        true
    }
}

/// The boot-hang fault: hang at this boot if the scratch register asks,
/// counting the hang down first (docs/test-device.md "Debug Faults").
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
