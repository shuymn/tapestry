//! Key config capture: while recording is closed, the next pressed control
//! of any device is offered to the key config screen. Nothing captured is
//! recorded.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use eval::ControlId;

use crate::gate::Packet;

#[derive(Debug, Default)]
struct Slot {
    armed: AtomicBool,
    /// The captured control plus one; 0 while nothing is captured.
    control: AtomicU64,
}

/// The capture slot shared by the input thread, which offers packets, and
/// the key config screen, which arms it and takes the control.
#[derive(Clone, Debug, Default)]
pub struct Capture(Arc<Slot>);

impl Capture {
    /// A disarmed slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Captures the next press, dropping any earlier capture.
    pub fn arm(&self) {
        self.0.control.store(0, Ordering::Relaxed);
        self.0.armed.store(true, Ordering::Release);
    }

    /// The captured control, if one was pressed since [`arm`](Self::arm);
    /// taking it disarms the slot.
    #[must_use]
    pub fn take(&self) -> Option<ControlId> {
        let stored = self.0.control.swap(0, Ordering::Acquire);
        // `offer` stores a `u32` plus one.
        let control = (stored.checked_sub(1)?) as u32;
        self.0.armed.store(false, Ordering::Release);
        Some(ControlId(control))
    }

    /// Offers a packet from the input thread. The first press while armed
    /// is captured, unless recording is open or the packet was received in
    /// the background; releases are not captured.
    pub fn offer(&self, packet: &Packet, recording_open: bool) {
        if recording_open || packet.background || packet.value == 0 {
            return;
        }
        if self.0.armed.swap(false, Ordering::Acquire) {
            self.0
                .control
                .store(u64::from(packet.control.0) + 1, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use eval::ControlId;

    use super::Capture;
    use crate::gate::{DeviceId, Packet};

    fn press(control: u32) -> Packet {
        Packet {
            device: DeviceId(7),
            control: ControlId(control),
            value: 1,
            background: false,
        }
    }

    #[test]
    fn capture_takes_the_first_press_after_arming() {
        let capture = Capture::new();
        capture.offer(&press(0x10), false);
        assert_eq!(capture.take(), None, "a disarmed slot captures nothing");
        capture.arm();
        capture.offer(&press(0xE11D), false);
        capture.offer(&press(0x2C), false);
        assert_eq!(capture.take(), Some(ControlId(0xE11D)));
        assert_eq!(capture.take(), None);
    }

    #[test]
    fn capture_skips_releases_background_and_open_recording() {
        let capture = Capture::new();
        capture.arm();
        capture.offer(
            &Packet {
                value: 0,
                ..press(0x1E)
            },
            false,
        );
        capture.offer(
            &Packet {
                background: true,
                ..press(0x1F)
            },
            false,
        );
        capture.offer(&press(0x20), true);
        assert_eq!(capture.take(), None);
        capture.offer(&press(0x21), false);
        assert_eq!(capture.take(), Some(ControlId(0x21)));
    }
}
