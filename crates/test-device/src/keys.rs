//! The pinned usage-to-scan-code table (docs/test-device.md "Keyboard
//! edges"): the make code Windows reports for each keyboard usage a
//! pattern may press, so the listener's edges map back to pattern items.
//! F1 reads each pattern usage alone on hardware once and compares it with
//! this table.

/// A set 1 make code, `extended` for the codes Windows reports with the
/// E0 prefix (`RI_KEY_E0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScanCode {
    pub code: u8,
    pub extended: bool,
}

const fn plain(code: u8) -> Option<ScanCode> {
    Some(ScanCode {
        code,
        extended: false,
    })
}

const fn e0(code: u8) -> Option<ScanCode> {
    Some(ScanCode {
        code,
        extended: true,
    })
}

/// Letters A to Z (usages 0x04 to 0x1D).
const LETTERS: [u8; 26] = [
    0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19,
    0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
];

/// F13 to F24 (usages 0x68 to 0x73): F24 is not next to F23.
const F13_TO_F24: [u8; 12] = [
    0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x76,
];

/// The scan code of keyboard `usage`, or `None` for a usage the table
/// does not pin (no pattern uses it).
#[must_use]
pub fn scan_code(usage: u8) -> Option<ScanCode> {
    match usage {
        0x04..=0x1D => LETTERS
            .get(usize::from(usage - 0x04))
            .and_then(|&c| plain(c)),
        // 1 to 9, then 0.
        0x1E..=0x26 => plain(usage - 0x1E + 0x02),
        0x27 => plain(0x0B),
        0x28 => plain(0x1C), // Enter
        0x29 => plain(0x01), // Escape
        0x2A => plain(0x0E), // Backspace
        0x2B => plain(0x0F), // Tab
        0x2C => plain(0x39), // Space
        // F1 to F10, then F11 and F12.
        0x3A..=0x43 => plain(usage - 0x3A + 0x3B),
        0x44 => plain(0x57),
        0x45 => plain(0x58),
        0x4F => e0(0x4D), // Right
        0x50 => e0(0x4B), // Left
        0x51 => e0(0x50), // Down
        0x52 => e0(0x48), // Up
        0x68..=0x73 => F13_TO_F24
            .get(usize::from(usage - 0x68))
            .and_then(|&c| plain(c)),
        0xE0 => plain(0x1D), // Left Control
        0xE1 => plain(0x2A), // Left Shift
        0xE2 => plain(0x38), // Left Alt
        0xE4 => e0(0x1D),    // Right Control
        0xE5 => plain(0x36), // Right Shift
        0xE6 => e0(0x38),    // Right Alt
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn code(code: u8) -> Option<ScanCode> {
        plain(code)
    }

    #[test]
    fn known_usages_have_their_make_codes() {
        // START and SELECT's defaults, the lease rows' keys and the
        // revocation trial's keys.
        assert_eq!(scan_code(0x08), code(0x12), "E");
        assert_eq!(scan_code(0x1A), code(0x11), "W");
        assert_eq!(scan_code(0x04), code(0x1E), "A");
        assert_eq!(scan_code(0x1D), code(0x2C), "Z");
        assert_eq!(scan_code(0x1E), code(0x02), "1");
        assert_eq!(scan_code(0x27), code(0x0B), "0");
        assert_eq!(scan_code(0x3A), code(0x3B), "F1");
        assert_eq!(scan_code(0x45), code(0x58), "F12");
        assert_eq!(scan_code(0x68), code(0x64), "F13");
        assert_eq!(scan_code(0x71), code(0x6D), "F22");
        assert_eq!(scan_code(0x72), code(0x6E), "F23");
        assert_eq!(scan_code(0x73), code(0x76), "F24");
        assert_eq!(scan_code(0x50), e0(0x4B), "Left");
        assert_eq!(scan_code(0x80), None);
        assert_eq!(scan_code(0xE3), None, "the Windows key is not pinned");
    }

    /// No two usages share a scan code, so every edge maps back to one
    /// usage.
    #[test]
    fn every_pinned_code_names_one_usage() {
        let pinned: Vec<ScanCode> = (0..=u8::MAX).filter_map(scan_code).collect();
        let unique: BTreeSet<ScanCode> = pinned.iter().copied().collect();
        assert_eq!(unique.len(), pinned.len());
        // Letters, digits, five editing keys, F1-F24, the arrows and six
        // modifiers.
        assert_eq!(pinned.len(), 26 + 10 + 5 + 24 + 4 + 6);
    }
}
