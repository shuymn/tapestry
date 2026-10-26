//! HID game controllers, portable part: how their buttons and axes are
//! named as controls, and how each input report becomes packets.
//!
//! The Windows side reads a report's pressed buttons and axis values with
//! the HID parser and hands them over as a [`HidReport`]; [`HidDevice`]
//! compares it with the collection's last report and turns each change
//! into a [`Packet`] for the [`SourceSet`], and offers presses and turns to
//! the key config's [`Capture`].
//!
//! Windows exposes each top-level collection of a controller as a device
//! of its own. The collections of one physical controller share one
//! [`DeviceId`], so they are one source owning its buttons and axes alike
//! (stage1.md D6); each collection's controls carry its collection number
//! ([`in_collection`]), so they stay distinct within the source.

use eval::ControlId;

use crate::capture::Capture;
use crate::gate::{DeviceId, Packet};
use crate::sources::{DeviceKind, SourceSet};

/// The HID usage page of buttons.
pub const BUTTON_PAGE: u16 = 0x09;
/// The HID usage page of the generic desktop axes.
pub const DESKTOP_PAGE: u16 = 0x01;
/// The generic desktop usages read as axes: X, Y, Z, Rx, Ry, Rz, Slider,
/// Dial and Wheel. The hat switch (0x39) is not an axis.
pub const AXIS_USAGES: std::ops::RangeInclusive<u16> = 0x30..=0x38;

/// The highest button usage read; higher buttons are ignored.
pub const MAX_BUTTON: u16 = 128;
/// The most axes read per collection; further ones are ignored.
pub const MAX_AXES: usize = 16;
/// The most report ids whose buttons are told apart per collection.
pub const MAX_REPORT_IDS: usize = 16;
/// The longest input report read, in bytes. A longer one is not read, and
/// the recording is marked incomplete ([`SourceSet::input_lost`]).
pub const MAX_REPORT_BYTES: usize = 4096;

/// The control of button `usage` (page 9): its number, from 1, in
/// collection 0. Within a collection buttons are below `0x1_0000` and axes
/// above.
#[must_use]
pub const fn button_control(usage: u16) -> ControlId {
    ControlId(usage as u32)
}

/// The control of the axis with usage `usage` on page `page`, in
/// collection 0: `page << 16 | usage`, so X of the generic desktop page is
/// `0x1_0030`.
#[must_use]
pub const fn axis_control(page: u16, usage: u16) -> ControlId {
    ControlId(((page as u32) << 16) | usage as u32)
}

/// `control` of collection `collection` of a controller: the collection
/// number in the top 8 bits. Collection 0 is the controller's only
/// collection, or one Windows did not number.
#[must_use]
pub const fn in_collection(control: ControlId, collection: u8) -> ControlId {
    ControlId((control.0 & 0x00FF_FFFF) | ((collection as u32) << 24))
}

/// Whether a controller's control is an axis ([`axis_control`]) rather than
/// a button, in any collection.
#[must_use]
pub const fn is_axis(control: ControlId) -> bool {
    control.0 & 0x00FF_FFFF >= 0x1_0000
}

/// A raw value of `bits` bits as the axis reports it: sign-extended when
/// the axis's logical range is signed (its minimum is negative).
#[must_use]
pub const fn axis_value(raw: u32, bits: u16, signed: bool) -> i32 {
    if !signed || bits == 0 || bits >= 32 {
        return raw as i32;
    }
    let shift = 32 - bits as u32;
    ((raw << shift) as i32) >> shift
}

/// The capability of an axis with control `control`, from its
/// descriptor: relative, or absolute with the declared logical range
/// `min..=max` in a field of `bits` bits, whose `max − min + 1` values are
/// one revolution. Any signedness and any width: a field unsigned to its
/// full width declares its maximum as a negative number (0xFFFF read as
/// -1 in 16 bits), which is read back unsigned. `None` for an absolute axis
/// whose count of values does not fit in a `u32` (a full 32-bit field) or
/// whose range is empty: such an axis is not read.
///
/// A descriptor can be wrong: a value reported outside the declared range
/// is counted at run time ([`SourceSet::axis_value_out_of_range`]), and key
/// config can widen the range from what it observes ([`widened_wrap`]).
#[must_use]
pub fn axis_cap(
    control: ControlId,
    absolute: bool,
    min: i32,
    max: i32,
    bits: u16,
) -> Option<AxisCap> {
    if !absolute {
        return Some(AxisCap {
            control,
            relative: true,
            wrap: 0,
            min: 0,
            max: 0,
            bits,
        });
    }
    let max = if max < min && (1..32).contains(&bits) {
        i32::try_from(max.cast_unsigned() & ((1_u32 << bits) - 1)).ok()?
    } else {
        max
    };
    let span = i64::from(max) - i64::from(min) + 1;
    let wrap = u32::try_from(span).ok().filter(|&s| s > 0)?;
    Some(AxisCap {
        control,
        relative: false,
        wrap,
        min,
        max,
        bits,
    })
}

/// The revolution of an absolute axis that reported `lo..=hi` while key
/// config captured it, when that lies outside its declared range: the
/// smallest standard span holding both, the full two's-complement range of
/// `k` bits (-2^(k−1)..=2^(k−1) − 1) for a signed field, 0..=2^k − 1 for an
/// unsigned one, with 2^k values; `k` never exceeds the field's width, nor
/// 31 bits (2^32 values do not fit in a `u32`). `None` when the observed
/// values lie inside the declared range, when no such span holds them, or
/// for a relative axis.
#[must_use]
pub fn widened_wrap(cap: &AxisCap, lo: i32, hi: i32) -> Option<u32> {
    if cap.relative || (cap.min <= lo && hi <= cap.max) {
        return None;
    }
    let (lo, hi) = (i64::from(lo.min(cap.min)), i64::from(hi.max(cap.max)));
    let signed = cap.min < 0;
    (1..=cap.bits.min(31)).find_map(|k| {
        let size = 1_i64 << k;
        let (low, high) = if signed {
            (-size / 2, size / 2 - 1)
        } else {
            (0, size - 1)
        };
        (low <= lo && hi <= high)
            .then(|| u32::try_from(size).ok())
            .flatten()
    })
}

/// The collection number Windows gives a controller's top-level collection
/// in its interface path (`&Col02` is 2), or 0 when the path has none.
#[must_use]
pub fn collection_of(path: &[u16]) -> u8 {
    let lower = |c: u16| u8::try_from(c).map_or(0, |b| b.to_ascii_lowercase());
    let marker = b"&col";
    path.windows(marker.len() + 2)
        .find(|w| w.iter().zip(marker).all(|(&c, &m)| lower(c) == m))
        .and_then(|w| {
            let hex = |c: u16| char::from(lower(c)).to_digit(16);
            Some(hex(w[4])? * 16 + hex(w[5])?)
        })
        .and_then(|n| u8::try_from(n).ok())
        .unwrap_or(0)
}

/// What a collection's axis is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxisCap {
    /// Its control ([`axis_control`] in its collection).
    pub control: ControlId,
    /// Whether each report is a movement rather than a position.
    pub relative: bool,
    /// For an absolute axis, the values it reports ([`axis_cap`]), as
    /// one revolution; 0 for a relative one.
    pub wrap: u32,
    /// For an absolute axis, the declared lowest value; 0 for a relative
    /// one.
    pub min: i32,
    /// For an absolute axis, the declared highest value; 0 for a relative
    /// one.
    pub max: i32,
    /// The width of its report field.
    pub bits: u16,
}

const NO_AXIS: AxisCap = AxisCap {
    control: ControlId(0),
    relative: false,
    wrap: 0,
    min: 0,
    max: 0,
    bits: 0,
};

/// The buttons one report id carries: bit `n − 1` is button `n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ButtonGroup {
    /// The report id; 0 for a collection without report ids.
    pub report_id: u8,
    /// The buttons it carries.
    pub buttons: u128,
}

/// What a collection reports, read from its descriptor once.
#[derive(Clone, Copy, Debug)]
pub struct HidLayout<'a> {
    /// Its collection number ([`collection_of`]).
    pub collection: u8,
    /// Its axes, with their controls in the collection, at most
    /// [`MAX_AXES`]; more are ignored.
    pub axes: &'a [AxisCap],
    /// Which buttons each report id carries, at most [`MAX_REPORT_IDS`].
    /// Empty: every report carries every button.
    pub buttons: &'a [ButtonGroup],
}

/// One input report as the HID parser read it.
#[derive(Clone, Copy, Debug)]
pub struct HidReport<'a> {
    /// Its report id; 0 for a collection without report ids.
    pub id: u8,
    /// The pressed button usages, or `None` when this report does not carry
    /// buttons.
    pub buttons: Option<&'a [u16]>,
    /// Each axis's value, in the order of [`HidDevice::axes`]; `None` for an
    /// axis this report does not carry.
    pub axes: &'a [Option<i32>],
}

/// The smallest movement that captures an axis in key config: a sixteenth
/// of a revolution, at least 16 values, net of any back and forth.
#[must_use]
pub fn capture_travel(cap: &AxisCap) -> i64 {
    i64::from((cap.wrap / 16).max(16))
}

/// A collection's last report, to turn the next into packets. Fixed size:
/// it allocates nothing.
#[derive(Clone, Debug)]
pub struct HidDevice {
    device: DeviceId,
    collection: u8,
    caps: [AxisCap; MAX_AXES],
    n_axes: usize,
    groups: [ButtonGroup; MAX_REPORT_IDS],
    n_groups: usize,
    /// Bit `n − 1` is button `n`.
    pressed: u128,
    /// Each absolute axis's last value.
    values: [Option<i32>; MAX_AXES],
    /// Each axis's net movement since the key config last armed its
    /// capture.
    travel: [i64; MAX_AXES],
    /// The capture's arming the travel counts from.
    armed: u64,
}

impl HidDevice {
    /// A collection of `device` laid out as `layout`, no button pressed and
    /// no axis value seen.
    #[must_use]
    pub fn new(device: DeviceId, layout: &HidLayout<'_>) -> Self {
        let mut caps = [NO_AXIS; MAX_AXES];
        let n_axes = layout.axes.len().min(MAX_AXES);
        caps[..n_axes].copy_from_slice(&layout.axes[..n_axes]);
        let mut groups = [ButtonGroup {
            report_id: 0,
            buttons: 0,
        }; MAX_REPORT_IDS];
        let n_groups = layout.buttons.len().min(MAX_REPORT_IDS);
        groups[..n_groups].copy_from_slice(&layout.buttons[..n_groups]);
        Self {
            device,
            collection: layout.collection,
            caps,
            n_axes,
            groups,
            n_groups,
            pressed: 0,
            values: [None; MAX_AXES],
            travel: [0; MAX_AXES],
            armed: 0,
        }
    }

    /// The device, shared by the controller's collections.
    #[must_use]
    pub const fn device(&self) -> DeviceId {
        self.device
    }

    /// Its axes.
    #[must_use]
    pub fn axes(&self) -> &[AxisCap] {
        &self.caps[..self.n_axes]
    }

    /// The buttons report `id` carries.
    fn carried(&self, id: u8) -> u128 {
        if self.n_groups == 0 {
            return !0;
        }
        self.groups[..self.n_groups]
            .iter()
            .filter(|g| g.report_id == id)
            .fold(0, |bits, g| bits | g.buttons)
    }

    /// Turns one report into packets for `sources`, in order: the buttons
    /// it carries released or pressed since the last report, by number,
    /// then each axis whose position changed or that reported a movement.
    /// Buttons it does not carry keep their state. Button presses are
    /// offered to `capture`, and so is an axis once it has turned
    /// [`capture_travel`] since the capture was armed.
    ///
    /// `background` is the packet's Raw Input mark: a background report
    /// trips the focus gate even when nothing in it changed
    /// ([`SourceSet::background`]), and the physical state still follows
    /// it.
    pub fn report(
        &mut self,
        report: &HidReport<'_>,
        background: bool,
        capture: &Capture,
        sources: &mut SourceSet,
    ) {
        if background {
            sources.background();
        }
        let recording = sources.is_recording();
        let (device, collection) = (self.device, self.collection);
        let packet = |control, value| Packet {
            device,
            control: in_collection(control, collection),
            value,
            background,
        };
        if let Some(buttons) = report.buttons {
            let carried = self.carried(report.id);
            let now = buttons
                .iter()
                .filter(|&&u| (1..=MAX_BUTTON).contains(&u))
                .fold(0_u128, |bits, &u| bits | 1 << (u - 1))
                & carried;
            let mut changed = (now ^ self.pressed) & carried;
            self.pressed = (self.pressed & !carried) | now;
            while changed != 0 {
                let bit = changed.trailing_zeros();
                changed &= changed - 1;
                let usage = u16::try_from(bit + 1).unwrap_or(MAX_BUTTON);
                let p = packet(button_control(usage), i32::from(now >> bit & 1 == 1));
                capture.offer(&p, DeviceKind::Controller, recording);
                sources.packet(&p);
            }
        }
        let armed = capture.armings();
        if armed != self.armed {
            self.armed = armed;
            self.travel = [0; MAX_AXES];
        }
        for (i, value) in report.axes.iter().take(self.n_axes).enumerate() {
            let Some(value) = *value else {
                continue;
            };
            let cap = self.caps[i];
            if !cap.relative && !(cap.min..=cap.max).contains(&value) {
                // A wrong descriptor: recorded as reported, and counted.
                sources.axis_value_out_of_range(device);
            }
            let prev = self.values[i];
            if !cap.relative && capture.watched() == Some(cap.control) {
                capture.observe(&cap, prev, value);
            }
            let moved = if cap.relative {
                i64::from(value)
            } else {
                match self.values[i].replace(value) {
                    None => 0,
                    Some(prev) if prev == value => continue,
                    Some(prev) => wrapped(i64::from(value) - i64::from(prev), cap.wrap),
                }
            };
            if cap.relative && value == 0 {
                continue;
            }
            let p = packet(cap.control, value);
            sources.packet(&p);
            let travel = &mut self.travel[i];
            *travel = travel.saturating_add(moved);
            if travel.abs() >= capture_travel(&cap) {
                *travel = 0;
                let offer = Packet { value: 1, ..p };
                capture.offer(&offer, DeviceKind::Controller, recording);
            }
        }
    }
}

/// A slot of an adapter's collection table: the collection it reads, if
/// any, and that collection's state.
pub trait CollectionSlot {
    /// The handle of the collection it reads; `None` when free.
    fn key(&self) -> Option<u64>;
    /// The collection's state.
    fn state(&self) -> &HidDevice;
    /// Frees the slot and drops its state: a later collection starts
    /// empty.
    fn free(&mut self);
}

/// The collection `key` was removed. Its whole physical device leaves:
/// every slot of it is freed, its cached button and position state
/// dropped, and its source disconnected, releasing everything it held as
/// any disconnect does. The device comes back only through fresh
/// discovery of its collections, as a new device with empty state, like a
/// first connection. Returns `false` when no slot reads `key`.
pub fn remove_collection<S: CollectionSlot>(
    slots: &mut [S],
    key: u64,
    sources: &mut SourceSet,
) -> bool {
    let Some(device) = slots
        .iter()
        .find(|s| s.key() == Some(key))
        .map(|s| s.state().device())
    else {
        return false;
    };
    for s in slots
        .iter_mut()
        .filter(|s| s.key().is_some() && s.state().device() == device)
    {
        s.free();
    }
    sources.disconnect(device);
    true
}

/// Copies `report` into `buf` to be parsed, if it fits; a report longer
/// than `buf` is lost input, and marks the recording incomplete.
pub fn read_report<'a>(
    report: &[u8],
    buf: &'a mut [u8],
    sources: &mut SourceSet,
) -> Option<&'a mut [u8]> {
    let Some(copy) = buf.get_mut(..report.len()) else {
        sources.input_lost();
        return None;
    };
    copy.copy_from_slice(report);
    Some(copy)
}

/// `d` the short way round `wrap` values; unchanged when `wrap` is 0.
pub(crate) fn wrapped(d: i64, wrap: u32) -> i64 {
    if wrap == 0 {
        return d;
    }
    let wrap = i64::from(wrap);
    let d = d.rem_euclid(wrap);
    if d > wrap / 2 { d - wrap } else { d }
}
