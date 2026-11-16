//! The test input device (docs/test-device.md; design.md "Test Input
//! Device"): what its firmware and the host share, so the bytes one side
//! writes are the bytes the other side reads, and the device's logic
//! ([`device`]), which the firmware runs against the USB controller and
//! the tests run against a simulated controller and host.
//!
//! Without the default `std` feature the crate is `no_std` and allocates
//! nothing: the firmware (`test-device/firmware`, outside the workspace)
//! builds it that way for the RP2350. The host's half needs `std`: the
//! clock bounds ([`clock`]), the matching behind the timing checks
//! ([`matcher`]), the trace check ([`trace::Check`]) and the report
//! descriptor parser ([`hid`]).

#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![forbid(unsafe_code)]
#![deny(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod boot;
#[cfg(feature = "std")]
pub mod clock;
pub mod device;
pub mod faults;
#[cfg(feature = "std")]
pub mod hid;
pub mod keys;
pub mod lease;
#[cfg(feature = "std")]
pub mod matcher;
pub mod mode;
pub mod protocol;
pub mod report;
pub mod revoke;
pub mod schedule;
pub mod trace;
pub mod uf2;
pub mod wrap;

/// The USB vendor id: pid.codes, whose test product ids are for devices
/// that are never distributed.
pub const VID: u16 = 0x1209;
/// The USB product id: pid.codes' test product id.
pub const PID: u16 = 0x0001;
/// The keyboard's interface number.
pub const KEYBOARD_INTERFACE: u8 = 0;
/// The game pad's interface number.
pub const PAD_INTERFACE: u8 = 1;
/// The control link's first interface number (CDC ACM, two interfaces).
pub const LINK_INTERFACE: u8 = 2;
