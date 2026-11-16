//! The RP2350's side of the device logic ([`test_device::device`]): the
//! USB controller's buffer controls and aborts, the clock, and the signals
//! that wake the writers and the link's sender.

use embassy_rp::pac;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Instant;
#[cfg(feature = "debug-faults")]
use test_device::boot::Cause;
use test_device::device::{Buffer, Hardware};
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

/// The DPRAM buffer control of `ep`.
fn control(ep: Ep) -> pac::common::Reg<pac::usb_dpram::regs::EpBufferControl, pac::common::RW> {
    pac::USB_DPRAM.ep_in_buffer_control(index(ep))
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
