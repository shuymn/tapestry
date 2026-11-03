//! The settings changed from the keyboard while playing (design.md "Input"):
//! the master volume and the display offset anytime, and the input offset
//! on the end screen, from the next play (design.md "Offset Changes and the
//! Watermark"). Each has a reserved key ([`Adjust`]) that no key config may
//! bind, a step and a range; [`Tuning`] holds the values and steps them.
//! Hi-speed's keys, F1 and F2 ([`SPEED_KEYS`]), are reserved too, and step
//! hi-speed through the play adjustments (design.md "Play Adjustments").

use crate::session::Settings;

/// One volume step, percent of unity.
pub const VOLUME_STEP_PERCENT: u32 = 5;
/// The largest volume, percent: unity.
pub const MAX_VOLUME_PERCENT: u32 = 100;
/// The input and display offsets lie within ± this, µs.
pub const OFFSET_LIMIT_US: i64 = 1_000_000;
/// One offset step, µs.
pub const OFFSET_STEP_US: i64 = 1_000;
/// F1 and F2, by scan code: slower and faster hi-speed, through the play
/// adjustments' plan rather than [`Tuning`].
pub const SPEED_KEYS: [u32; 2] = [0x3B, 0x3C];

/// A settings key: F3 to F8 on a keyboard, by scan code, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adjust {
    /// F3.
    VolumeDown,
    /// F4.
    VolumeUp,
    /// F5: notes drawn earlier.
    DisplayOffsetDown,
    /// F6: notes drawn later.
    DisplayOffsetUp,
    /// F7, on the end screen only.
    InputOffsetDown,
    /// F8, on the end screen only.
    InputOffsetUp,
}

impl Adjust {
    /// Every settings key, by scan code.
    pub const ALL: [Self; 6] = [
        Self::VolumeDown,
        Self::VolumeUp,
        Self::DisplayOffsetDown,
        Self::DisplayOffsetUp,
        Self::InputOffsetDown,
        Self::InputOffsetUp,
    ];

    /// The keyboard scan code: F3 is 0x3D, through F8 at 0x42.
    #[must_use]
    pub const fn key(self) -> u32 {
        0x3D + self as u32
    }

    /// The settings key with scan code `code`.
    #[must_use]
    pub fn from_key(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.key() == code)
    }

    /// What it changes, for the player.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::VolumeDown | Self::VolumeUp => "volume",
            Self::DisplayOffsetDown | Self::DisplayOffsetUp => "display offset",
            Self::InputOffsetDown | Self::InputOffsetUp => "input offset",
        }
    }

    /// Whether it changes the input offset, which only the end screen
    /// takes.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn is_input_offset(self) -> bool {
        matches!(self, Self::InputOffsetDown | Self::InputOffsetUp)
    }
}

/// The values the settings keys change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuning {
    /// The master volume, percent of unity.
    pub volume_percent: u32,
    /// The display offset, µs.
    pub display_offset_us: i64,
    /// The input offset, µs.
    pub input_offset_us: i64,
}

impl Tuning {
    /// The values in `settings`. The volume is rounded to a whole percent.
    #[must_use]
    pub fn of(settings: &Settings) -> Self {
        Self {
            // `Settings::load` keeps the volume within 0.0 to 1.0.
            volume_percent: (settings.device.master_volume * 100.0).round() as u32,
            display_offset_us: settings.device.display_offset_us,
            input_offset_us: settings.device.input_offset_us,
        }
    }

    /// One step of `adjust`, held within its range.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn adjust(self, adjust: Adjust) -> Self {
        let offset = |us: i64| us.clamp(-OFFSET_LIMIT_US, OFFSET_LIMIT_US);
        let mut t = self;
        match adjust {
            Adjust::VolumeDown => {
                t.volume_percent = t.volume_percent.saturating_sub(VOLUME_STEP_PERCENT);
            }
            Adjust::VolumeUp => {
                t.volume_percent = t
                    .volume_percent
                    .saturating_add(VOLUME_STEP_PERCENT)
                    .min(MAX_VOLUME_PERCENT);
            }
            Adjust::DisplayOffsetDown => {
                t.display_offset_us = offset(t.display_offset_us.saturating_sub(OFFSET_STEP_US));
            }
            Adjust::DisplayOffsetUp => {
                t.display_offset_us = offset(t.display_offset_us.saturating_add(OFFSET_STEP_US));
            }
            Adjust::InputOffsetDown => {
                t.input_offset_us = offset(t.input_offset_us.saturating_sub(OFFSET_STEP_US));
            }
            Adjust::InputOffsetUp => {
                t.input_offset_us = offset(t.input_offset_us.saturating_add(OFFSET_STEP_US));
            }
        }
        t
    }

    /// The master volume as the mixer takes it.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn volume(self) -> f32 {
        self.volume_percent as f32 / 100.0
    }

    /// Writes the values into `settings`. A volume still at the whole
    /// percent `settings` rounds to keeps its exact value (0.754 stays
    /// 0.754 until a volume key moves it).
    pub fn apply_to(self, settings: &mut Settings) {
        if self.volume_percent != Self::of(settings).volume_percent {
            settings.device.master_volume = self.volume();
        }
        settings.device.display_offset_us = self.display_offset_us;
        settings.device.input_offset_us = self.input_offset_us;
    }
}
