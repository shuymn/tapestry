//! The report rate and reporting mode, which survive the reboot that
//! applies them in a watchdog scratch register (docs/test-device.md
//! "Report rate").

/// The interrupt endpoints' polling interval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rate {
    /// `bInterval` 1.
    #[default]
    Hz1000,
    /// `bInterval` 8.
    Hz125,
}

impl Rate {
    /// The endpoints' `bInterval`, in ms at full speed.
    #[must_use]
    pub const fn interval_ms(self) -> u8 {
        match self {
            Self::Hz1000 => 1,
            Self::Hz125 => 8,
        }
    }

    /// The interval in µs.
    #[must_use]
    pub const fn interval_us(self) -> u32 {
        self.interval_ms() as u32 * 1000
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "1000" => Some(Self::Hz1000),
            "125" => Some(Self::Hz125),
            _ => None,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hz1000 => "1000",
            Self::Hz125 => "125",
        }
    }
}

/// When the device sends reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reporting {
    /// Only when a control changes.
    #[default]
    OnChange,
    /// At every poll, but only inside a lease: outside one the device stays
    /// silent, so it never wakes a player's input thread while idle.
    Continuous,
}

impl Reporting {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "change" => Some(Self::OnChange),
            "continuous" => Some(Self::Continuous),
            _ => None,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::OnChange => "change",
            Self::Continuous => "continuous",
        }
    }
}

/// The mode the firmware runs in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mode {
    pub rate: Rate,
    pub reporting: Reporting,
}

/// The scratch word's tag: a word without it (power-on garbage, another
/// firmware's value) means the default mode.
const TAG: u32 = 0x4D4F_0000;

impl Mode {
    /// The mode byte in each pad report and in `hello`: the interval in ms
    /// in the low nibble, bit 4 for continuous reporting. Bit 7 is the
    /// report's [`crate::report::PREVIOUS_VALID`], never part of the mode.
    #[must_use]
    pub const fn byte(self) -> u8 {
        let continuous = match self.reporting {
            Reporting::OnChange => 0,
            Reporting::Continuous => 0x10,
        };
        self.rate.interval_ms() | continuous
    }

    /// The mode a [`Self::byte`] names.
    #[must_use]
    pub const fn from_byte(b: u8) -> Option<Self> {
        let rate = match b & 0x0F {
            1 => Rate::Hz1000,
            8 => Rate::Hz125,
            _ => return None,
        };
        let reporting = match b & 0x70 {
            0 => Reporting::OnChange,
            0x10 => Reporting::Continuous,
            _ => return None,
        };
        Some(Self { rate, reporting })
    }

    /// `bcdDevice`: revision information only. Windows keys interface
    /// instances on VID, PID and MI, so a mode change makes no new device
    /// instance.
    #[must_use]
    pub const fn bcd_device(self) -> u16 {
        0x0100 | self.byte() as u16
    }

    /// The scratch register's word.
    #[must_use]
    pub const fn scratch(self) -> u32 {
        TAG | self.byte() as u32
    }

    /// The mode a scratch word holds; the default for any other word.
    #[must_use]
    pub const fn from_scratch(word: u32) -> Self {
        if word & 0xFFFF_FF00 != TAG {
            return Self {
                rate: Rate::Hz1000,
                reporting: Reporting::OnChange,
            };
        }
        let b = (word & 0xFF) as u8;
        match Self::from_byte(b) {
            Some(m) => m,
            None => Self {
                rate: Rate::Hz1000,
                reporting: Reporting::OnChange,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_survives_its_byte_and_scratch_word() {
        for rate in [Rate::Hz1000, Rate::Hz125] {
            for reporting in [Reporting::OnChange, Reporting::Continuous] {
                let m = Mode { rate, reporting };
                assert_eq!(Mode::from_byte(m.byte()), Some(m));
                assert_eq!(Mode::from_scratch(m.scratch()), m);
                assert_eq!(m.byte() & crate::report::PREVIOUS_VALID, 0);
                assert_eq!(m.bcd_device() >> 8, 1);
            }
        }
        assert_eq!(Mode::from_scratch(0), Mode::default());
        assert_eq!(Mode::from_scratch(0x4D4F_0003), Mode::default());
        assert_eq!(Rate::Hz125.interval_us(), 8000);
    }
}
