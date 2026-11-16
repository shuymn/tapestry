//! The two HID reports: their descriptors, byte layouts and encodings
//! (docs/test-device.md "Reports"). Each layout is checked against its own
//! descriptor by a parser ([`crate::hid`]), so the bytes the firmware
//! writes are the fields Windows reads.

/// The keyboard report's bytes: the eight modifiers, then a bitmap of
/// usages 0 to 127.
pub const KEYBOARD_REPORT: usize = 17;
/// The game pad report's bytes.
pub const PAD_REPORT: usize = 18;

/// The keyboard's report descriptor (no report id).
#[rustfmt::skip]
pub const KEYBOARD_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01,       // Generic Desktop, Keyboard, Application
    0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7,       // Keyboard page, modifiers E0..E7
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
    0x19, 0x00, 0x29, 0x7F,                   // usages 0..127 as a bitmap
    0x95, 0x80, 0x81, 0x02,
    0xC0,
];

/// The game pad's report descriptor (no report id).
#[rustfmt::skip]
pub const PAD_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01,       // Generic Desktop, Game Pad, Application
    0x05, 0x09, 0x19, 0x01, 0x29, 0x10,       // buttons 1..16
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x10, 0x81, 0x02,
    0x05, 0x01, 0x09, 0x30,                   // X: the turntable, 0..255
    0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02,
    0x06, 0x00, 0xFF, 0x09, 0x04,             // vendor: the mode byte
    0x75, 0x08, 0x95, 0x01, 0x81, 0x02,
    0x09, 0x01,                               // vendor: the sequence number
    0x15, 0x00, 0x27, 0xFF, 0xFF, 0x00, 0x00, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02,
    0x09, 0x02, 0x09, 0x03, 0x09, 0x05,       // vendor: R(k), H(k-1), C(k-1) in us
    0x17, 0x00, 0x00, 0x00, 0x80, 0x27, 0xFF, 0xFF, 0xFF, 0x7F,
    0x75, 0x20, 0x95, 0x03, 0x81, 0x02,
    0xC0,
];

/// The vendor usage page of the pad's timing fields.
pub const VENDOR_PAGE: u16 = 0xFF00;

/// Where each pad field starts, in bytes.
pub mod pad_at {
    /// Buttons 1 to 16, little-endian, button 1 in bit 0.
    pub const BUTTONS: usize = 0;
    /// The turntable axis.
    pub const AXIS: usize = 2;
    /// The mode byte ([`crate::mode::Mode::byte`]) with [`super::PREVIOUS_VALID`].
    pub const MODE: usize = 3;
    /// The report's sequence number k, `u16` little-endian.
    pub const SEQ: usize = 4;
    /// R(k): when report k's content was fixed.
    pub const READY: usize = 6;
    /// H(k-1): when report k-1 was handed to the USB controller.
    pub const HANDOFF: usize = 10;
    /// C(k-1): when the host took report k-1.
    pub const COMPLETION: usize = 14;
}

/// One value of a layout: its name, usage page and usage, and its bit
/// offset and size in the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub page: u16,
    pub usage: u16,
    pub bit: u32,
    pub size: u32,
}

const fn bits(byte: usize) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a byte offset inside an 18-byte report"
    )]
    let b = byte as u32;
    b * 8
}

/// The pad's values as [`pad_at`] places them: what [`PadReport::encode`]
/// writes, which its descriptor must declare at the same bits.
pub const PAD_FIELDS: &[Field] = &[
    Field {
        name: "button 1",
        page: 0x09,
        usage: 1,
        bit: bits(pad_at::BUTTONS),
        size: 1,
    },
    Field {
        name: "button 16",
        page: 0x09,
        usage: 16,
        bit: bits(pad_at::BUTTONS) + 15,
        size: 1,
    },
    Field {
        name: "axis",
        page: 0x01,
        usage: 0x30,
        bit: bits(pad_at::AXIS),
        size: 8,
    },
    Field {
        name: "mode",
        page: VENDOR_PAGE,
        usage: 4,
        bit: bits(pad_at::MODE),
        size: 8,
    },
    Field {
        name: "sequence",
        page: VENDOR_PAGE,
        usage: 1,
        bit: bits(pad_at::SEQ),
        size: 16,
    },
    Field {
        name: "ready",
        page: VENDOR_PAGE,
        usage: 2,
        bit: bits(pad_at::READY),
        size: 32,
    },
    Field {
        name: "handoff",
        page: VENDOR_PAGE,
        usage: 3,
        bit: bits(pad_at::HANDOFF),
        size: 32,
    },
    Field {
        name: "completion",
        page: VENDOR_PAGE,
        usage: 5,
        bit: bits(pad_at::COMPLETION),
        size: 32,
    },
];

/// The keyboard's values as [`KeyboardReport`] places them.
pub const KEYBOARD_FIELDS: &[Field] = &[
    Field {
        name: "left control",
        page: 0x07,
        usage: 0xE0,
        bit: 0,
        size: 1,
    },
    Field {
        name: "right GUI",
        page: 0x07,
        usage: 0xE7,
        bit: 7,
        size: 1,
    },
    Field {
        name: "usage 0",
        page: 0x07,
        usage: 0x00,
        bit: 8,
        size: 1,
    },
    Field {
        name: "usage 0x1A (W)",
        page: 0x07,
        usage: 0x1A,
        bit: 8 + 0x1A,
        size: 1,
    },
    Field {
        name: "usage 0x73 (F24)",
        page: 0x07,
        usage: 0x73,
        bit: 8 + 0x73,
        size: 1,
    },
    Field {
        name: "usage 127",
        page: 0x07,
        usage: 0x7F,
        bit: 8 + 0x7F,
        size: 1,
    },
];

/// Bit 7 of the mode byte: the report carries H and C of the report
/// before it (none before the first report).
pub const PREVIOUS_VALID: u8 = 0x80;

/// A game pad report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PadReport {
    /// Buttons 1 to 16, button 1 in bit 0.
    pub buttons: u16,
    pub axis: u8,
    /// The mode byte, without [`PREVIOUS_VALID`].
    pub mode: u8,
    /// k, the low 16 bits of the pad's trace sequence number.
    pub seq: u16,
    /// R(k), the low 32 bits of the device µs.
    pub ready: u32,
    /// H(k-1) and C(k-1), when report k-1 exists.
    pub previous: Option<(u32, u32)>,
}

impl PadReport {
    /// The report's bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; PAD_REPORT] {
        let mut out = [0_u8; PAD_REPORT];
        put(&mut out, pad_at::BUTTONS, &self.buttons.to_le_bytes());
        put(&mut out, pad_at::AXIS, &[self.axis]);
        let valid = if self.previous.is_some() {
            PREVIOUS_VALID
        } else {
            0
        };
        put(
            &mut out,
            pad_at::MODE,
            &[(self.mode & !PREVIOUS_VALID) | valid],
        );
        put(&mut out, pad_at::SEQ, &self.seq.to_le_bytes());
        put(&mut out, pad_at::READY, &self.ready.to_le_bytes());
        if let Some((handoff, completion)) = self.previous {
            put(&mut out, pad_at::HANDOFF, &handoff.to_le_bytes());
            put(&mut out, pad_at::COMPLETION, &completion.to_le_bytes());
        }
        out
    }

    /// The report in `bytes`, exactly [`PAD_REPORT`] of them (no report
    /// id).
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; PAD_REPORT] = bytes.try_into().ok()?;
        let mode = bytes[pad_at::MODE];
        let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let u32_at = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        Some(Self {
            buttons: u16_at(pad_at::BUTTONS),
            axis: bytes[pad_at::AXIS],
            mode: mode & !PREVIOUS_VALID,
            seq: u16_at(pad_at::SEQ),
            ready: u32_at(pad_at::READY),
            previous: (mode & PREVIOUS_VALID != 0)
                .then(|| (u32_at(pad_at::HANDOFF), u32_at(pad_at::COMPLETION))),
        })
    }

    /// Whether no button is pressed (the axis is a position, not a press).
    #[must_use]
    pub const fn is_neutral(&self) -> bool {
        self.buttons == 0
    }
}

fn put(out: &mut [u8; PAD_REPORT], at: usize, bytes: &[u8]) {
    if let Some(dst) = out.get_mut(at..at + bytes.len()) {
        dst.copy_from_slice(bytes);
    }
}

/// A keyboard report: the modifiers byte, then the usage bitmap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardReport {
    bytes: [u8; KEYBOARD_REPORT],
}

impl KeyboardReport {
    /// Nothing pressed.
    pub const NEUTRAL: Self = Self {
        bytes: [0; KEYBOARD_REPORT],
    };

    /// The byte and bit of `usage`: a modifier (0xE0 to 0xE7) in byte 0, a
    /// usage below 128 in the bitmap; any other usage has none.
    const fn place(usage: u8) -> Option<(usize, u8)> {
        match usage {
            0xE0..=0xE7 => Some((0, 1 << (usage - 0xE0))),
            0..=0x7F => Some((1 + (usage / 8) as usize, 1 << (usage % 8))),
            _ => None,
        }
    }

    /// Presses or releases `usage`; false for a usage the report cannot
    /// carry.
    pub fn set(&mut self, usage: u8, down: bool) -> bool {
        let Some((byte, bit)) = Self::place(usage) else {
            return false;
        };
        let Some(b) = self.bytes.get_mut(byte) else {
            return false;
        };
        if down {
            *b |= bit;
        } else {
            *b &= !bit;
        }
        true
    }

    /// Whether `usage` is pressed.
    #[must_use]
    pub fn is_down(&self, usage: u8) -> bool {
        Self::place(usage)
            .and_then(|(byte, bit)| self.bytes.get(byte).map(|b| b & bit != 0))
            .unwrap_or(false)
    }

    /// Whether nothing is pressed.
    #[must_use]
    pub fn is_neutral(&self) -> bool {
        self.bytes.iter().all(|&b| b == 0)
    }

    /// The report's bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; KEYBOARD_REPORT] {
        &self.bytes
    }

    /// The report in `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            bytes: bytes.try_into().ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid;

    /// Each layout's fields lie where its own descriptor puts them, and the
    /// descriptor's bits fill the report exactly (docs/test-device.md, F0).
    #[test]
    fn each_layout_matches_its_descriptor() {
        assert_eq!(
            hid::check_layout(PAD_DESCRIPTOR, PAD_FIELDS, PAD_REPORT),
            Ok(())
        );
        assert_eq!(
            hid::check_layout(KEYBOARD_DESCRIPTOR, KEYBOARD_FIELDS, KEYBOARD_REPORT),
            Ok(())
        );
    }

    /// A descriptor whose sequence number is 8 bits wide no longer matches
    /// the layout, and the check names that field first.
    #[test]
    fn a_changed_field_width_fails_at_that_field() {
        let mut copy = PAD_DESCRIPTOR.to_vec();
        // The report size before the sequence number's Input item.
        let at = copy
            .windows(4)
            .position(|w| w == [0x75, 0x10, 0x95, 0x01])
            .unwrap();
        copy[at + 1] = 0x08;
        let err = hid::check_layout(&copy, PAD_FIELDS, PAD_REPORT).unwrap_err();
        assert_eq!(err.field, "sequence", "{err:?}");
    }

    #[test]
    fn a_pad_report_round_trips() {
        let r = PadReport {
            buttons: 0x8001,
            axis: 200,
            mode: 0x11,
            seq: 65_535,
            ready: 0xDEAD_BEEF,
            previous: Some((7, 9)),
        };
        assert_eq!(PadReport::decode(&r.encode()), Some(r));
        let first = PadReport {
            previous: None,
            ..r
        };
        let bytes = first.encode();
        assert_eq!(bytes[pad_at::MODE] & PREVIOUS_VALID, 0);
        assert_eq!(PadReport::decode(&bytes), Some(first));
        assert_eq!(PadReport::decode(&bytes[..17]), None);
    }

    #[test]
    fn keyboard_usages_set_their_bits() {
        let mut k = KeyboardReport::default();
        assert!(k.is_neutral());
        assert!(k.set(0x1A, true));
        assert!(k.set(0xE1, true));
        assert!(!k.set(0x80, true), "beyond the bitmap");
        assert_eq!(k.bytes()[0], 0x02);
        assert_eq!(k.bytes()[1 + 3], 1 << 2);
        assert!(k.is_down(0x1A));
        assert!(k.set(0x1A, false));
        assert!(k.set(0xE1, false));
        assert!(k.is_neutral());
    }
}
