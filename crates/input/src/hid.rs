//! HID game controllers, portable part: how their buttons and axes are
//! named as controls, and how each input report becomes packets.
//!
//! The Windows side reads a report's pressed buttons and axis values with
//! the HID parser and hands them over as a [`HidReport`]; [`HidDevice`]
//! compares it with the device's last report and turns each change into a
//! [`Packet`] for the [`SourceSet`], and offers presses and turns to the
//! key config's [`Capture`].
//!
//! A controller is one device and so one source, owning its buttons and
//! axes alike (stage1.md D6).

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
/// The most axes read per device; further ones are ignored.
pub const MAX_AXES: usize = 16;

/// The control of button `usage` (page 9): its number, from 1. Buttons are
/// below `0x1_0000`, axes above.
#[must_use]
pub const fn button_control(usage: u16) -> ControlId {
    ControlId(usage as u32)
}

/// The control of the axis with usage `usage` on page `page`:
/// `page << 16 | usage`, so X of the generic desktop page is `0x1_0030`.
#[must_use]
pub const fn axis_control(page: u16, usage: u16) -> ControlId {
    ControlId((page as u32) << 16 | usage as u32)
}

/// Whether a controller's control is an axis ([`axis_control`]) rather than
/// a button.
#[must_use]
pub const fn is_axis(control: ControlId) -> bool {
    control.0 >= 0x1_0000
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

/// What a device's axis is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxisCap {
    /// Its control ([`axis_control`]).
    pub control: ControlId,
    /// Whether each report is a movement rather than a position.
    pub relative: bool,
    /// For an absolute axis, the number of values it reports (logical
    /// maximum − minimum + 1), as one revolution; 0 if unknown.
    pub wrap: u32,
}

const NO_AXIS: AxisCap = AxisCap {
    control: ControlId(0),
    relative: false,
    wrap: 0,
};

/// One input report as the HID parser read it.
#[derive(Clone, Copy, Debug)]
pub struct HidReport<'a> {
    /// The pressed button usages, or `None` when this report does not carry
    /// the buttons (a device with several report ids).
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

/// A HID controller's last report, to turn the next into packets. Fixed
/// size: it allocates nothing.
#[derive(Clone, Debug)]
pub struct HidDevice {
    device: DeviceId,
    caps: [AxisCap; MAX_AXES],
    n_axes: usize,
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
    /// A device with these axes (at most [`MAX_AXES`]; more are ignored),
    /// no button pressed and no axis value seen.
    #[must_use]
    pub fn new(device: DeviceId, axes: &[AxisCap]) -> Self {
        let mut caps = [NO_AXIS; MAX_AXES];
        let n_axes = axes.len().min(MAX_AXES);
        caps[..n_axes].copy_from_slice(&axes[..n_axes]);
        Self {
            device,
            caps,
            n_axes,
            pressed: 0,
            values: [None; MAX_AXES],
            travel: [0; MAX_AXES],
            armed: 0,
        }
    }

    /// The device.
    #[must_use]
    pub const fn device(&self) -> DeviceId {
        self.device
    }

    /// Its axes.
    #[must_use]
    pub fn axes(&self) -> &[AxisCap] {
        &self.caps[..self.n_axes]
    }

    /// Turns one report into packets for `sources`, in order: buttons
    /// released or pressed since the last report, by number, then each axis
    /// whose position changed or that reported a movement. Button presses
    /// are offered to `capture`, and so is an axis once it has turned
    /// [`capture_travel`] since the capture was armed. `background` is the
    /// packet's Raw Input mark.
    pub fn report(
        &mut self,
        report: &HidReport<'_>,
        background: bool,
        capture: &Capture,
        sources: &mut SourceSet,
    ) {
        let recording = sources.is_recording();
        let packet = |control, value| Packet {
            device: self.device,
            control,
            value,
            background,
        };
        if let Some(buttons) = report.buttons {
            let now = buttons
                .iter()
                .filter(|&&u| (1..=MAX_BUTTON).contains(&u))
                .fold(0_u128, |bits, &u| bits | 1 << (u - 1));
            let mut changed = now ^ self.pressed;
            self.pressed = now;
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
                capture.offer(&packet(cap.control, 1), DeviceKind::Controller, recording);
            }
        }
    }
}

/// `d` the short way round `wrap` values; unchanged when `wrap` is 0.
fn wrapped(d: i64, wrap: u32) -> i64 {
    if wrap == 0 {
        return d;
    }
    let wrap = i64::from(wrap);
    let d = d.rem_euclid(wrap);
    if d > wrap / 2 { d - wrap } else { d }
}
