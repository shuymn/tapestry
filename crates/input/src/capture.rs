//! Key config capture: while recording is closed, the next pressed control
//! of any device is offered to the key config screen. Nothing captured is
//! recorded.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use eval::ControlId;

use crate::gate::Packet;
use crate::sources::DeviceKind;

/// Set in a stored capture: something was captured.
const PRESENT: u64 = 1 << 33;
/// Set in a stored capture: the control is a controller's.
const CONTROLLER: u64 = 1 << 32;

#[derive(Debug, Default)]
struct Slot {
    armed: AtomicBool,
    /// The captured control in the low 32 bits, with [`PRESENT`] and, for
    /// a controller, [`CONTROLLER`]; 0 while nothing is captured.
    control: AtomicU64,
    /// How many times the slot was armed.
    armings: AtomicU64,
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
        self.0.armings.fetch_add(1, Ordering::Relaxed);
        self.0.control.store(0, Ordering::Relaxed);
        self.0.armed.store(true, Ordering::Release);
    }

    /// How many times the slot was armed: an axis counts its turn toward a
    /// capture from the latest arming.
    #[must_use]
    pub fn armings(&self) -> u64 {
        self.0.armings.load(Ordering::Relaxed)
    }

    /// The captured control and the kind of device it belongs to, if one
    /// was pressed since [`arm`](Self::arm); taking it disarms the slot.
    #[must_use]
    pub fn take(&self) -> Option<(ControlId, DeviceKind)> {
        let stored = self.0.control.swap(0, Ordering::Acquire);
        if stored & PRESENT == 0 {
            return None;
        }
        self.0.armed.store(false, Ordering::Release);
        let kind = if stored & CONTROLLER == 0 {
            DeviceKind::Keyboard
        } else {
            DeviceKind::Controller
        };
        Some((ControlId(stored as u32), kind))
    }

    /// Offers a packet from a device of `kind` on the input thread. The
    /// first press while armed is captured, unless recording is open or the
    /// packet was received in the background; releases are not captured.
    pub fn offer(&self, packet: &Packet, kind: DeviceKind, recording_open: bool) {
        if recording_open || packet.background || packet.value == 0 {
            return;
        }
        if self.0.armed.swap(false, Ordering::Acquire) {
            let kind = match kind {
                DeviceKind::Keyboard => 0,
                DeviceKind::Controller => CONTROLLER,
            };
            let stored = u64::from(packet.control.0) | kind | PRESENT;
            self.0.control.store(stored, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use eval::ControlId;

    use super::Capture;
    use crate::gate::{DeviceId, Packet};
    use crate::sources::DeviceKind;

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
        capture.offer(&press(0x10), DeviceKind::Keyboard, false);
        assert_eq!(capture.take(), None, "a disarmed slot captures nothing");
        capture.arm();
        capture.offer(&press(0xE11D), DeviceKind::Keyboard, false);
        capture.offer(&press(0x2C), DeviceKind::Keyboard, false);
        assert_eq!(
            capture.take(),
            Some((ControlId(0xE11D), DeviceKind::Keyboard))
        );
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
            DeviceKind::Keyboard,
            false,
        );
        capture.offer(
            &Packet {
                background: true,
                ..press(0x1F)
            },
            DeviceKind::Keyboard,
            false,
        );
        capture.offer(&press(0x20), DeviceKind::Keyboard, true);
        assert_eq!(capture.take(), None);
        capture.offer(&press(0x21), DeviceKind::Keyboard, false);
        assert_eq!(
            capture.take(),
            Some((ControlId(0x21), DeviceKind::Keyboard))
        );
    }

    /// The kind comes back with the control, and the extreme controls of
    /// either kind keep their value: 0 is a control, not "nothing".
    #[test]
    fn capture_keeps_the_kind_and_every_control_value() {
        let capture = Capture::new();
        for (control, kind) in [
            (0, DeviceKind::Controller),
            (u32::MAX, DeviceKind::Controller),
            (0, DeviceKind::Keyboard),
            (u32::MAX, DeviceKind::Keyboard),
        ] {
            capture.arm();
            capture.offer(&press(control), kind, false);
            assert_eq!(capture.take(), Some((ControlId(control), kind)));
            assert_eq!(capture.take(), None);
        }
    }
}
