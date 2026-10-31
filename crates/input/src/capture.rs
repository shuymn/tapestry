//! Key config capture: while recording is closed, the next pressed control
//! of any device is offered to the key config screen. Nothing captured is
//! recorded.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use eval::ControlId;

use crate::gate::Packet;
use crate::hid::{AxisCap, HidProduct, wrapped};
use crate::sources::DeviceKind;

/// Set in a stored capture: something was captured.
const PRESENT: u64 = 1 << 33;
/// Set in a stored capture: the control is a controller's.
const CONTROLLER: u64 = 1 << 32;

/// Set in a stored product: a product is stored.
const PRODUCT: u64 = 1 << 48;
/// Set in a stored product: it has an interface number.
const INTERFACE: u64 = 1 << 49;

/// A product in one word: the product id in bits 0-15, the vendor id in
/// bits 16-31, the interface number in bits 32-39 with [`INTERFACE`], and
/// [`PRODUCT`].
fn pack_product(p: HidProduct) -> u64 {
    let interface = p.interface.map_or(0, |i| (u64::from(i) << 32) | INTERFACE);
    u64::from(p.product_id) | (u64::from(p.vendor_id) << 16) | interface | PRODUCT
}

/// The product [`pack_product`] stored; `None` for 0.
fn unpack_product(stored: u64) -> Option<HidProduct> {
    (stored & PRODUCT != 0).then_some(HidProduct {
        vendor_id: (stored >> 16) as u16,
        product_id: stored as u16,
        interface: (stored & INTERFACE != 0).then_some((stored >> 32) as u8),
    })
}

#[derive(Debug, Default)]
struct Slot {
    armed: AtomicBool,
    /// The captured control in the low 32 bits, with [`PRESENT`] and, for
    /// a controller, [`CONTROLLER`]; 0 while nothing is captured.
    control: AtomicU64,
    /// The captured controller press's product ([`pack_product`]); 0 for
    /// a keyboard key or a controller whose product is unknown.
    product: AtomicU64,
    /// Whether the captured press was an axis turned toward lower values.
    lower: AtomicBool,
    /// How many times the slot was armed.
    armings: AtomicU64,
    /// The axis watched, plus [`PRESENT`]; 0 when none.
    watch: AtomicU64,
    /// Its declared range and field width, from its reports.
    declared_min: AtomicI64,
    declared_max: AtomicI64,
    bits: AtomicU64,
    /// The lowest and highest values it reported while watched.
    lo: AtomicI64,
    hi: AtomicI64,
    /// [`SEEN`], [`FORWARD`] and [`BACKWARD`].
    flags: AtomicU64,
}

/// Set in a watch's flags: the axis reported while watched.
const SEEN: u64 = 1;
/// Set in a watch's flags: it crossed its boundary going forward (highest
/// value to lowest).
const FORWARD: u64 = 1 << 1;
/// Set in a watch's flags: it crossed its boundary going backward.
const BACKWARD: u64 = 1 << 2;

/// What a watched axis showed ([`Capture::watch`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watched {
    /// Its declared range and field.
    pub cap: AxisCap,
    /// The lowest value it reported.
    pub lo: i32,
    /// The highest value it reported.
    pub hi: i32,
    /// Whether it crossed its boundary going forward.
    pub forward: bool,
    /// Whether it crossed its boundary going backward.
    pub backward: bool,
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

    /// The product of the controller whose press [`take`](Self::take)
    /// returned last; `None` for a keyboard key or a controller whose
    /// product is unknown. It stays until the next capture.
    #[must_use]
    pub fn product(&self) -> Option<HidProduct> {
        unpack_product(self.0.product.load(Ordering::Acquire))
    }

    /// Whether the press [`take`](Self::take) returned last was an axis
    /// turned toward lower values: its offered value was negative. A
    /// button, a key, or an axis turned toward higher values is not. It
    /// stays until the next capture.
    #[must_use]
    pub fn lower(&self) -> bool {
        self.0.lower.load(Ordering::Acquire)
    }

    /// Watches the absolute axis `control`: the input thread notes the
    /// values it reports and whether it crosses its boundary (from its
    /// highest value to its lowest, or back), so key config learns the
    /// range it really reports. Watching again starts over.
    pub fn watch(&self, control: ControlId) {
        self.0.flags.store(0, Ordering::Relaxed);
        self.0.lo.store(i64::MAX, Ordering::Relaxed);
        self.0.hi.store(i64::MIN, Ordering::Relaxed);
        self.0
            .watch
            .store(u64::from(control.0) | PRESENT, Ordering::Release);
    }

    /// Stops watching.
    pub fn unwatch(&self) {
        self.0.watch.store(0, Ordering::Release);
    }

    /// The axis watched, if any.
    #[must_use]
    pub fn watched(&self) -> Option<ControlId> {
        let w = self.0.watch.load(Ordering::Acquire);
        (w & PRESENT != 0).then_some(ControlId(w as u32))
    }

    /// One report of the watched axis, declared as `cap`, on the input
    /// thread: `value`, after `prev`. A move the short way round its field
    /// (2^bits values) that goes the other way than its raw difference
    /// crossed the boundary.
    pub fn observe(&self, cap: &AxisCap, prev: Option<i32>, value: i32) {
        let slot = &self.0;
        slot.declared_min
            .store(i64::from(cap.min), Ordering::Relaxed);
        slot.declared_max
            .store(i64::from(cap.max), Ordering::Relaxed);
        slot.bits.store(u64::from(cap.bits), Ordering::Relaxed);
        slot.lo.fetch_min(i64::from(value), Ordering::Relaxed);
        slot.hi.fetch_max(i64::from(value), Ordering::Relaxed);
        let mut flags = SEEN;
        if let Some(prev) = prev {
            let raw = i64::from(value) - i64::from(prev);
            let field = if (1..32).contains(&cap.bits) {
                1_u32 << cap.bits
            } else {
                cap.wrap
            };
            let short = wrapped(raw, field);
            if short > 0 && raw < 0 {
                flags |= FORWARD;
            }
            if short < 0 && raw > 0 {
                flags |= BACKWARD;
            }
        }
        slot.flags.fetch_or(flags, Ordering::Release);
    }

    /// What the watched axis has shown so far; `None` before its first
    /// report.
    #[must_use]
    pub fn watch_result(&self) -> Option<Watched> {
        let slot = &self.0;
        let flags = slot.flags.load(Ordering::Acquire);
        let control = self.watched()?;
        if flags & SEEN == 0 {
            return None;
        }
        let narrow = |a: &AtomicI64| i32::try_from(a.load(Ordering::Relaxed)).unwrap_or(0);
        let (min, max) = (narrow(&slot.declared_min), narrow(&slot.declared_max));
        Some(Watched {
            cap: AxisCap {
                control,
                relative: false,
                wrap: u32::try_from(i64::from(max) - i64::from(min) + 1).unwrap_or(0),
                min,
                max,
                bits: u16::try_from(slot.bits.load(Ordering::Relaxed)).unwrap_or(0),
                signed: min < 0,
            },
            lo: narrow(&slot.lo),
            hi: narrow(&slot.hi),
            forward: flags & FORWARD != 0,
            backward: flags & BACKWARD != 0,
        })
    }

    /// Offers a packet from a device of `kind` on the input thread. The
    /// first press while armed is captured, unless recording is open or the
    /// packet was received in the background; releases are not captured.
    pub fn offer(&self, packet: &Packet, kind: DeviceKind, recording_open: bool) {
        self.offer_from(packet, kind, None, recording_open);
    }

    /// [`offer`](Self::offer) for a controller of `product` (`None`:
    /// unknown), which [`product`](Self::product) then returns.
    pub fn offer_controller(
        &self,
        packet: &Packet,
        product: Option<HidProduct>,
        recording_open: bool,
    ) {
        self.offer_from(packet, DeviceKind::Controller, product, recording_open);
    }

    fn offer_from(
        &self,
        packet: &Packet,
        kind: DeviceKind,
        product: Option<HidProduct>,
        recording_open: bool,
    ) {
        if recording_open || packet.background || packet.value == 0 {
            return;
        }
        if self.0.armed.swap(false, Ordering::Acquire) {
            let kind = match kind {
                DeviceKind::Keyboard => 0,
                DeviceKind::Controller => CONTROLLER,
            };
            self.0
                .product
                .store(product.map_or(0, pack_product), Ordering::Relaxed);
            self.0.lower.store(packet.value < 0, Ordering::Relaxed);
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
    use crate::hid::HidProduct;
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

    /// A controller press brings its product, extreme ids included; a
    /// keyboard key or a controller of unknown product clears it.
    #[test]
    fn capture_keeps_the_controller_product() {
        let capture = Capture::new();
        for (vendor_id, product_id, interface) in [
            (0x1A2B, 0x3C4D, Some(0x5E)),
            (0xFFFF, 0, None),
            (0, 0xFFFF, Some(0xFF)),
            (0x6F70, 0x8192, Some(0)),
        ] {
            let product = HidProduct {
                vendor_id,
                product_id,
                interface,
            };
            capture.arm();
            capture.offer_controller(&press(3), Some(product), false);
            assert_eq!(capture.take(), Some((ControlId(3), DeviceKind::Controller)));
            assert_eq!(capture.product(), Some(product));
        }
        capture.arm();
        capture.offer(&press(0x1E), DeviceKind::Keyboard, false);
        assert!(capture.take().is_some());
        assert_eq!(capture.product(), None);
        capture.arm();
        capture.offer_controller(&press(4), None, false);
        assert!(capture.take().is_some());
        assert_eq!(capture.product(), None);
    }
}
