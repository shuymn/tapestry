//! Settings files (design.md "Settings Files"): `device.toml` for this PC and
//! its devices, `player.toml` for the person playing, both in one data root
//! with `presets/` and `records/`. The data root is a parameter; the player
//! resolves the real one.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ::audio::AudioEngine;
use bms_model::{Lane, TimeUs};
use eval::{
    AxisBinding, AxisMode, Binding, ControlId, DeviceProfile, ScratchSettings, SourceProfile,
};
use input::DeviceKind;
use input::hid::{AxisCap, HidProduct, is_axis, wrap_range};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::adjust_keys::{AdjustKeyError, AdjustmentKeys};
use crate::error::IoFailure;
use crate::hispeed::{Cover, Covers, HiSpeedSettings, PlayOptions, PlayOptionsError};
use crate::policy::FailureAction;
use crate::presets::UnknownPreset;
use crate::queue::{DEFAULT_QUEUED_PERIODS, QUEUED_PERIODS};
use crate::save::write_atomic;
use crate::tuning::{Adjust, OFFSET_LIMIT_US, SPEED_KEYS};

/// The directory holding every settings file, the presets and the records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataRoot(PathBuf);

impl DataRoot {
    /// The data root at `path`.
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self(path)
    }

    /// The root itself.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// `device.toml`.
    #[must_use]
    pub fn device_settings(&self) -> PathBuf {
        self.0.join("device.toml")
    }

    /// `player.toml`.
    #[must_use]
    pub fn player_settings(&self) -> PathBuf {
        self.0.join("player.toml")
    }

    /// `presets/`, the user preset files.
    #[must_use]
    pub fn presets(&self) -> PathBuf {
        self.0.join("presets")
    }

    /// `records/`.
    #[must_use]
    pub fn records(&self) -> PathBuf {
        self.0.join("records")
    }
}

/// Why a settings file could not be read or written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    /// The file system refused.
    #[error("the settings file cannot be used")]
    Io(#[source] IoFailure),
    /// The file is not TOML of the settings' shape.
    #[error("the settings file cannot be read")]
    Parse(#[source] toml::de::Error),
    /// `device.toml` reads, but a value cannot be used.
    #[error("the settings cannot be used: device.toml")]
    InvalidDevice(#[source] DeviceSettingsError),
    /// `player.toml` reads, but a value cannot be used.
    #[error("the settings cannot be used: player.toml")]
    InvalidPlayer(#[source] PlayerSettingsError),
    /// `device.toml` is not of [`DEVICE_SETTINGS_VERSION`]: `found` is its
    /// `version`, `None` when it has none. Nothing is converted.
    #[error(
        "device.toml has version {}; this tapestry reads only version {DEVICE_SETTINGS_VERSION} \
         (START/SELECT bindings). Move device.toml aside; tapestry then starts from defaults, \
         and `tapestry keys` writes a new one.",
        found.map_or_else(|| "none".to_owned(), |v| v.to_string())
    )]
    DeviceVersion {
        /// The file's `version`, if it has one.
        found: Option<i64>,
    },
}

impl From<io::Error> for SettingsError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.into())
    }
}

/// Why [`DeviceSettings::validate`] refused a value.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeviceSettingsError {
    /// The keyboard key config cannot be used.
    #[error("keyboard key config")]
    KeyboardKeys(#[source] KeyError),
    /// The controller key config cannot be used.
    #[error("controller key config")]
    ControllerKeys(#[source] KeyError),
    /// The controller key config binds lanes but names no controller.
    #[error(
        "controller key config: it does not say which controller it belongs to; \
         run `tapestry keys` again"
    )]
    ControllerUnnamed,
    /// An offset outside ±[`OFFSET_LIMIT_US`].
    #[error("{name} {value} is outside -{OFFSET_LIMIT_US} to {OFFSET_LIMIT_US}")]
    Offset {
        /// The setting.
        name: &'static str,
        /// Its value, µs.
        value: i64,
    },
    /// A master volume outside 0.0 to 1.0, as the bits of the `f32`
    /// ([`f32::to_bits`]), so the error stays `Eq`.
    #[error("master_volume {} is outside 0.0 to 1.0", f32::from_bits(*.bits))]
    MasterVolume {
        /// The volume's bits.
        bits: u32,
    },
    /// Queued periods outside [`QUEUED_PERIODS`].
    #[error(
        "queued_periods {0} is outside {start} to {end}",
        start = QUEUED_PERIODS.start(),
        end = QUEUED_PERIODS.end()
    )]
    QueuedPeriods(u32),
    /// `[scratch] adjust_counts_per_step` of 0: a step needs movement.
    #[error("[scratch] adjust_counts_per_step 0 is not positive")]
    AdjustCountsPerStep,
}

/// Why `player.toml` was refused: [`PlayerSettings::validate`], or a
/// `visible_time_ms` found by [`load_settings`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlayerSettingsError {
    /// The file sets `visible_time_ms`, which no longer sets hi-speed.
    #[error(
        "visible_time_ms is no longer read; hi-speed is set by [hispeed] green_target \
         (Floating) or classic_hispeed_x100 (Classic); remove the line"
    )]
    VisibleTimeRemoved,
    /// A play option outside its range ([`PlayOptions::new`]), by its
    /// table and key.
    #[error("[{section}] {setting} {value} is outside {expected}")]
    PlayOption {
        /// The table: `hispeed` or `covers`.
        section: &'static str,
        /// The key.
        setting: &'static str,
        /// The value refused.
        value: u32,
        /// What the key takes.
        expected: &'static str,
    },
    /// A preset value that can name neither a built-in nor a user preset.
    #[error(transparent)]
    Preset(UnknownPreset),
    /// A `[skins.<kind>]` value that cannot name a skin, by its table and
    /// key ([`SkinChoice::check`]).
    #[error("[skins.{kind}] {key} '{value}' {why}")]
    Skin {
        /// The kind's table: `play`, `selection`, `decision` or `result`.
        kind: &'static str,
        /// The key: `root` or `entry`.
        key: &'static str,
        /// The value refused.
        value: String,
        /// Why.
        why: &'static str,
    },
    /// A key `[skins]` or a `[skins.<kind>]` table does not take.
    #[error(
        "[{table}] has no key {key}; [skins] takes play, selection, decision and result, \
         and each of them root, entry, options and files"
    )]
    SkinKey {
        /// The table: `skins` or `skins.<kind>`.
        table: String,
        /// The key.
        key: String,
    },
}

/// A control bound to a lane, in a settings file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneKey {
    /// The control: a keyboard scan code, or a controller's button or axis
    /// ([`input::hid`]). A controller's axis plays its lane through the
    /// scratch conversion of [`DeviceSettings::scratch`].
    pub control: u32,
    /// The lane: 0 is the scratch, 1-7 the keys.
    pub lane: u8,
}

/// One kind of device's key config: lanes, system controls and the play
/// adjustments' roles (design.md "Play Adjustments", bindings). The
/// default binds nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    /// The lane bindings.
    pub lanes: Vec<LaneKey>,
    /// The control that retries, if bound.
    pub retry: Option<u32>,
    /// The control that quits, if bound.
    pub quit: Option<u32>,
    /// START, if bound: a keyboard key or a controller's button.
    pub start: Option<u32>,
    /// SELECT, if bound, likewise.
    pub select: Option<u32>,
    /// The auxiliary negative key, if bound, likewise.
    pub aux_negative: Option<u32>,
    /// The auxiliary positive key, if bound, likewise.
    pub aux_positive: Option<u32>,
    /// The scratch's positive digital direction: one of the scratch lane's
    /// own buttons or keys.
    pub scratch_positive: Option<u32>,
    /// The scratch's negative digital direction, likewise.
    pub scratch_negative: Option<u32>,
}

/// What a control is bound to in a key config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A lane: 0 is the scratch, 1-7 the keys.
    Lane(u8),
    /// Retry.
    Retry,
    /// Quit.
    Quit,
    /// START.
    Start,
    /// SELECT.
    Select,
    /// The auxiliary negative key.
    AuxNegative,
    /// The auxiliary positive key.
    AuxPositive,
    /// The scratch's positive digital direction.
    ScratchPositive,
    /// The scratch's negative digital direction.
    ScratchNegative,
}

impl Role {
    /// Whether it is a play adjustments' role rather than a lane, retry or
    /// quit.
    #[must_use]
    pub const fn is_adjustment(self) -> bool {
        !matches!(self, Self::Lane(_) | Self::Retry | Self::Quit)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lane(0) => f.write_str("scratch"),
            Self::Lane(n) => write!(f, "key {n}"),
            Self::Retry => f.write_str("retry"),
            Self::Quit => f.write_str("quit"),
            Self::Start => f.write_str("START"),
            Self::Select => f.write_str("SELECT"),
            Self::AuxNegative => f.write_str("the auxiliary negative key"),
            Self::AuxPositive => f.write_str("the auxiliary positive key"),
            Self::ScratchPositive => f.write_str("the scratch's positive direction"),
            Self::ScratchNegative => f.write_str("the scratch's negative direction"),
        }
    }
}

/// Why a key config cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// One control bound to two roles.
    #[error("control {control:#x} is bound to both {first} and {second}")]
    Overlap {
        /// The control.
        control: u32,
        /// The role it was bound to first.
        first: Role,
        /// The role it was bound to again.
        second: Role,
    },
    /// A keyboard key the game reserves for something else.
    #[error("key {control:#x} is reserved for {reserved_for}")]
    Reserved {
        /// The key's scan code.
        control: u32,
        /// What the game uses it for.
        reserved_for: &'static str,
    },
    /// A lane the game does not have.
    #[error("lane {0} does not exist (lanes are 0 to 7)")]
    NoSuchLane(u8),
    /// Retry or quit bound to something other than a keyboard key: stage 1
    /// reads them from the window's keyboard messages only.
    #[error("{0} takes keyboard keys only")]
    NotAKeyboardKey(Role),
    /// A play adjustments' binding that cannot be used
    /// ([`AdjustmentKeys::check`]).
    #[error(transparent)]
    Adjustment(AdjustKeyError),
}

/// F9: shows and hides the measurement overlay during play (design.md "Latency
/// Figures").
pub const OVERLAY_KEY: u32 = 0x43;

/// F11: chooses the previous play preset on the select screen (design.md
/// "Play Lifecycle").
pub const PREV_PRESET_KEY: u32 = 0x57;

/// F12: chooses the next play preset on the select screen; key config's
/// keep key too, on its own screen.
pub const NEXT_PRESET_KEY: u32 = crate::keys::KEEP_KEY;

/// What the game reserves keyboard key `code` (a scan code) for, if
/// anything: the settings keys, the overlay key, the preset keys and the
/// key config's keep key are never bound.
#[must_use]
pub fn reserved_key(code: u32) -> Option<&'static str> {
    if code == NEXT_PRESET_KEY {
        return Some("keeping a binding in key config and the next play preset");
    }
    if code == PREV_PRESET_KEY {
        return Some("the previous play preset");
    }
    if code == OVERLAY_KEY {
        return Some("the measurement overlay");
    }
    if SPEED_KEYS.contains(&code) {
        return Some("hi-speed");
    }
    Adjust::from_key(code).map(Adjust::name)
}

impl KeyConfig {
    /// Every binding with its role: the lanes, retry and quit, then the
    /// play adjustments' roles, the scratch's directions last.
    pub fn roles(&self) -> impl Iterator<Item = (u32, Role)> + '_ {
        let adjustments = [
            (self.start, Role::Start),
            (self.select, Role::Select),
            (self.aux_negative, Role::AuxNegative),
            (self.aux_positive, Role::AuxPositive),
            (self.scratch_positive, Role::ScratchPositive),
            (self.scratch_negative, Role::ScratchNegative),
        ];
        self.lanes
            .iter()
            .map(|k| (k.control, Role::Lane(k.lane)))
            .chain(self.retry.map(|c| (c, Role::Retry)))
            .chain(self.quit.map(|c| (c, Role::Quit)))
            .chain(
                adjustments
                    .into_iter()
                    .filter_map(|(c, r)| c.map(|c| (c, r))),
            )
    }

    /// Checks the key config of a kind of device: every lane exists, no
    /// control has two roles (keyboard keys by
    /// [`key_identity`](input::raw::key_identity)), a keyboard binds no reserved key, and only a
    /// keyboard binds retry and quit; then the play adjustments' roles
    /// ([`AdjustmentKeys::check`]).
    ///
    /// # Errors
    ///
    /// The first binding that breaks a rule.
    pub fn check(&self, kind: DeviceKind) -> Result<(), KeyError> {
        let mut seen: Vec<(u32, Role)> = Vec::new();
        for (control, role) in self.roles().filter(|(_, r)| !r.is_adjustment()) {
            // Keyboard keys are compared as the window tells them apart,
            // so two names of one key are one key.
            let key = match kind {
                DeviceKind::Keyboard => input::raw::key_identity(ControlId(control)).0,
                DeviceKind::Controller => control,
            };
            if kind != DeviceKind::Keyboard && matches!(role, Role::Retry | Role::Quit) {
                return Err(KeyError::NotAKeyboardKey(role));
            }
            if let Role::Lane(l) = role
                && Lane::new(l).is_none()
            {
                return Err(KeyError::NoSuchLane(l));
            }
            if kind == DeviceKind::Keyboard
                && let Some(reserved_for) = reserved_key(key)
            {
                return Err(KeyError::Reserved {
                    control,
                    reserved_for,
                });
            }
            if let Some(&(_, first)) = seen.iter().find(|(k, _)| *k == key) {
                return Err(KeyError::Overlap {
                    control,
                    first,
                    second: role,
                });
            }
            seen.push((key, role));
        }
        AdjustmentKeys::of(self)
            .check(kind)
            .map_err(KeyError::Adjustment)
    }

    /// The generated keyboard's key config in an autoplay session
    /// (design.md "Autoplay"): the default keyboard lanes, and no retry or
    /// quit binding, which the window's keys keep doing.
    #[must_use]
    pub fn autoplay() -> Self {
        Self {
            retry: None,
            quit: None,
            ..DeviceSettings::default().keyboard
        }
    }

    /// Every recorded control: the lanes, the system controls, START,
    /// SELECT and the auxiliary keys (design.md "Input", roles of
    /// controls). The scratch's directions are its lane controls already.
    #[must_use]
    pub fn recorded(&self) -> Vec<ControlId> {
        self.roles()
            .filter(|(_, r)| !matches!(r, Role::ScratchPositive | Role::ScratchNegative))
            .map(|(c, _)| ControlId(c))
            .collect()
    }

    /// The lane bindings the evaluator reads; an entry with a lane the
    /// game does not have is left out. A controller's axes are left out
    /// too: they are [`axes`](Self::axes).
    #[must_use]
    pub fn bindings(&self, kind: DeviceKind) -> Vec<Binding> {
        self.lanes
            .iter()
            .filter(|k| !axis_of(kind, k.control))
            .filter_map(|k| {
                Lane::new(k.lane).map(|lane| Binding {
                    control: ControlId(k.control),
                    lane,
                })
            })
            .collect()
    }

    /// A controller's axes bound to lanes, each converted by `scratch`
    /// resolved for the axis as the device declares it in `caps`
    /// ([`ScratchConfig::settings`]); none for a keyboard.
    #[must_use]
    pub fn axes(
        &self,
        kind: DeviceKind,
        scratch: &ScratchConfig,
        caps: &[AxisCap],
    ) -> Vec<AxisBinding> {
        self.lanes
            .iter()
            .filter(|k| axis_of(kind, k.control))
            .filter_map(|k| {
                let control = ControlId(k.control);
                Lane::new(k.lane).map(|lane| AxisBinding {
                    control,
                    lane,
                    scratch: scratch.settings(caps.iter().find(|c| c.control == control)),
                })
            })
            .collect()
    }
}

/// Whether `control` of a device of `kind` is an axis.
const fn axis_of(kind: DeviceKind, control: u32) -> bool {
    matches!(kind, DeviceKind::Controller) && is_axis(ControlId(control))
}

/// How a controller's axis plays its lane (design.md "Analog Scratch"), in
/// `device.toml`. Whether the axis is relative and how many values a
/// revolution has come from the device's declaration unless set here; the
/// thresholds and release time default to [`ScratchSettings::DEFAULT`],
/// starting points tuned by playing. The resolved settings are what the
/// input log records; [`menu_reversed`](Self::menu_reversed) and the
/// adjustment settings are not among them, since they never reach
/// judgment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ScratchConfig {
    /// Whether the axis reports movement rather than position; from the
    /// device when absent.
    pub relative: Option<bool>,
    /// For an absolute axis, the values in one revolution (it wraps from
    /// its highest back to its lowest), 0 if it does not wrap; from the
    /// device's declared range when absent.
    pub wrap: Option<u32>,
    /// Movement in one direction that presses.
    pub press_threshold: u32,
    /// Movement in the pressed direction that keeps the lane pressed.
    pub hold_threshold: u32,
    /// How long after the axis stops the lane is released, µs.
    pub release_after_us: u32,
    /// Whether the axis scrolls menus down when turned toward lower values
    /// rather than higher ones (design.md "Menu input"). Menus only: play
    /// and the input log never see it.
    pub menu_reversed: bool,
    /// Whether the axis adjusts (design.md "Play Adjustments") the other
    /// way from its movement: turned toward lower values, it raises
    /// SUDDEN+ and speeds up.
    pub adjust_reversed: bool,
    /// For an axis that does not wrap, or is relative, the movement that
    /// makes one adjustment step; positive. Absent, such an axis does not
    /// adjust.
    pub adjust_counts_per_step: Option<u32>,
}

impl Default for ScratchConfig {
    fn default() -> Self {
        let d = ScratchSettings::DEFAULT;
        Self {
            relative: None,
            wrap: None,
            press_threshold: d.press_threshold,
            hold_threshold: d.hold_threshold,
            release_after_us: d.release_after_us,
            menu_reversed: false,
            adjust_reversed: false,
            adjust_counts_per_step: None,
        }
    }
}

impl ScratchConfig {
    /// The settings the evaluator reads for an axis the device declares
    /// as `cap` (`None`: not declared, as for a device not connected): a
    /// value set here, else the declaration, else the default
    /// ([`ScratchSettings::DEFAULT`]: absolute, without wraparound).
    #[must_use]
    pub fn settings(&self, cap: Option<&AxisCap>) -> ScratchSettings {
        let default_wrap = match ScratchSettings::DEFAULT.mode {
            AxisMode::Absolute { wrap } => wrap,
            AxisMode::Relative => 0,
        };
        let relative = self.relative.or(cap.map(|c| c.relative)).unwrap_or(false);
        let declared = cap.filter(|c| !c.relative).map(|c| c.wrap);
        ScratchSettings {
            mode: if relative {
                AxisMode::Relative
            } else {
                AxisMode::Absolute {
                    wrap: self.wrap.or(declared).unwrap_or(default_wrap),
                }
            },
            press_threshold: self.press_threshold,
            hold_threshold: self.hold_threshold,
            release_after_us: self.release_after_us,
        }
    }

    /// The values the conversion of an axis declared as `cap` takes as one
    /// revolution, as [`settings`](Self::settings) resolves it: the range
    /// of a revolution set here ([`wrap_range`]), else the declared range.
    /// `None` when it has none: relative, or not wrapping.
    #[must_use]
    pub fn range(&self, cap: &AxisCap) -> Option<(i32, i32)> {
        if self.relative.unwrap_or(cap.relative) {
            return None;
        }
        match self.wrap {
            Some(wrap) => wrap_range(cap, wrap),
            None => (!cap.relative).then_some((cap.min, cap.max)),
        }
    }
}

/// The controller a controller key config belongs to: its model and, on a
/// composite device, the function whose controls are bound
/// ([`HidProduct`]), the same in every port and after replugging. It names
/// no single unit and no port, and it never enters the input log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerDevice {
    /// The vendor id.
    pub vendor_id: u16,
    /// The product id.
    pub product_id: u16,
    /// The interface number of the function on a composite device; absent
    /// for a device that is not composite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<u8>,
}

impl From<HidProduct> for ControllerDevice {
    fn from(p: HidProduct) -> Self {
        Self {
            vendor_id: p.vendor_id,
            product_id: p.product_id,
            interface: p.interface,
        }
    }
}

impl From<ControllerDevice> for HidProduct {
    fn from(d: ControllerDevice) -> Self {
        Self {
            vendor_id: d.vendor_id,
            product_id: d.product_id,
            interface: d.interface,
        }
    }
}

/// The `version` a `device.toml` carries; [`load_settings`] refuses any
/// other, and a file without one.
pub const DEVICE_SETTINGS_VERSION: u32 = 2;

/// `device.toml`: what belongs to this PC and its devices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DeviceSettings {
    /// The file's layout, [`DEVICE_SETTINGS_VERSION`].
    pub version: u32,
    /// The key config shared by every keyboard.
    pub keyboard: KeyConfig,
    /// The controller key config: its buttons and axes by HID usage
    /// ([`input::hid`]), of one controller
    /// ([`controller_device`](Self::controller_device)).
    pub controller: KeyConfig,
    /// The controller [`controller`](Self::controller) belongs to, as
    /// `tapestry keys` captured it; set whenever it binds a lane. Only a
    /// connected controller of this model plays; other controllers take no
    /// part.
    pub controller_device: Option<ControllerDevice>,
    /// The input latency offset, µs.
    pub input_offset_us: i64,
    /// The audio output latency offset, µs.
    pub audio_offset_us: i64,
    /// The display latency offset, µs.
    pub display_offset_us: i64,
    /// The audio device's id; the system default when absent.
    pub audio_device: Option<String>,
    /// The monitor to play on, by its display device name (for example
    /// `\.\DISPLAY1`); the system's primary monitor when absent.
    pub monitor: Option<String>,
    /// The master volume, 0.0 (silent) to 1.0 (unity).
    pub master_volume: f32,
    /// How controllers' axes play their lanes.
    pub scratch: ScratchConfig,
    /// Periods the audio output keeps queued ahead of playback, 2 or 3
    /// ([`QUEUED_PERIODS`]). A play with underruns after its READY state sets
    /// it to 3; only the player sets it back.
    pub queued_periods: u32,
    /// The song library, `[library]`.
    pub library: LibrarySettings,
}

/// `[library]` of `device.toml`: where this PC keeps its charts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LibrarySettings {
    /// The folders scanned for charts, with every folder below them, as
    /// written. Strings rather than paths, so every value has a TOML form.
    pub roots: Vec<String>,
}

impl Default for DeviceSettings {
    fn default() -> Self {
        // Scan codes: left shift for the scratch, then Z S X D C F V.
        let codes = [0x2a, 0x2c, 0x1f, 0x2d, 0x20, 0x2e, 0x21, 0x2f];
        Self {
            version: DEVICE_SETTINGS_VERSION,
            keyboard: KeyConfig {
                lanes: (0..)
                    .zip(codes)
                    .map(|(lane, control)| LaneKey { control, lane })
                    .collect(),
                // Backspace retries and Escape quits.
                retry: Some(0x0e),
                quit: Some(0x01),
                // START on E, SELECT on W, the auxiliary keys on Up and
                // Down, and the scratch key as the scratch's positive
                // direction. E is no lane, menu, reserved, retry or quit
                // key, and leaves Q free for a quit key.
                start: Some(0x12),
                select: Some(0x11),
                aux_negative: Some(0xE048),
                aux_positive: Some(0xE050),
                scratch_positive: Some(0x2a),
                scratch_negative: None,
            },
            // Nothing until key config captures it.
            controller: KeyConfig::default(),
            controller_device: None,
            input_offset_us: 0,
            audio_offset_us: 0,
            display_offset_us: 0,
            audio_device: None,
            monitor: None,
            master_volume: 1.0,
            scratch: ScratchConfig::default(),
            queued_periods: DEFAULT_QUEUED_PERIODS,
            library: LibrarySettings::default(),
        }
    }
}

impl DeviceSettings {
    /// The key config for a kind of device.
    #[must_use]
    pub const fn keys(&self, kind: DeviceKind) -> &KeyConfig {
        match kind {
            DeviceKind::Keyboard => &self.keyboard,
            DeviceKind::Controller => &self.controller,
        }
    }

    /// Checks what [`Settings::load`] cannot leave to the types: both key
    /// configs ([`KeyConfig::check`], the play adjustments' roles
    /// included), a controller key config binding anything only with its
    /// [`controller_device`](Self::controller_device), the input and
    /// display offsets within ±[`OFFSET_LIMIT_US`], the master volume from
    /// 0.0 to 1.0, the queued periods within [`QUEUED_PERIODS`], and a
    /// positive `[scratch] adjust_counts_per_step`.
    ///
    /// # Errors
    ///
    /// The first value that cannot be used, for the player.
    pub fn validate(&self) -> Result<(), DeviceSettingsError> {
        self.keyboard
            .check(DeviceKind::Keyboard)
            .map_err(DeviceSettingsError::KeyboardKeys)?;
        self.controller
            .check(DeviceKind::Controller)
            .map_err(DeviceSettingsError::ControllerKeys)?;
        if self.controller.roles().next().is_some() && self.controller_device.is_none() {
            return Err(DeviceSettingsError::ControllerUnnamed);
        }
        for (name, value) in [
            ("input_offset_us", self.input_offset_us),
            ("display_offset_us", self.display_offset_us),
        ] {
            if !(-OFFSET_LIMIT_US..=OFFSET_LIMIT_US).contains(&value) {
                return Err(DeviceSettingsError::Offset { name, value });
            }
        }
        if !(0.0..=1.0).contains(&self.master_volume) {
            return Err(DeviceSettingsError::MasterVolume {
                bits: self.master_volume.to_bits(),
            });
        }
        if !QUEUED_PERIODS.contains(&self.queued_periods) {
            return Err(DeviceSettingsError::QueuedPeriods(self.queued_periods));
        }
        if self.scratch.adjust_counts_per_step == Some(0) {
            return Err(DeviceSettingsError::AdjustCountsPerStep);
        }
        Ok(())
    }

    /// Gives `engine` the master volume and the audio output offset. An
    /// out-of-range volume is not applied; the engine keeps unity.
    pub fn apply_to(&self, engine: &mut AudioEngine) {
        let _ = engine.set_volume(self.master_volume);
        engine.set_output_offset(TimeUs(self.audio_offset_us));
    }

    /// The profile the evaluator reads for sources of the given kinds,
    /// indexed by source id, whose axes are as declared in `axes` (by
    /// source id; a source past its end declares none).
    #[must_use]
    pub fn profile(&self, kinds: &[DeviceKind], axes: &[Vec<AxisCap>]) -> DeviceProfile {
        DeviceProfile {
            sources: kinds
                .iter()
                .enumerate()
                .map(|(i, &kind)| SourceProfile {
                    input_offset: TimeUs(self.input_offset_us),
                    bindings: self.keys(kind).bindings(kind),
                    axes: self.keys(kind).axes(
                        kind,
                        &self.scratch,
                        axes.get(i).map_or(&[], Vec::as_slice),
                    ),
                })
                .collect(),
        }
    }
}

/// `player.toml`: what belongs to the person playing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PlayerSettings {
    /// The preset plays use, by name: a built-in's, or a user preset's in
    /// `presets/` (design.md "Standard Presets", "User Presets");
    /// `beatoraja-style` by default.
    pub preset: String,
    /// The gauge shown, by its index in the preset.
    pub displayed_gauge: usize,
    /// What the shown survival gauge's failure does: `"close"` (the
    /// play enters Closing, then shows its result) or `"play-to-end"`
    /// (design.md "Play Lifecycle", Closing). Declared before the tables,
    /// so it is written before them.
    pub on_failure: FailureAction,
    /// The language of the player's own screen text, `"ja"` by default
    /// (design.md "Settings Files"). Read once, at startup; declared
    /// before the tables, so it is written before them.
    pub language: Language,
    /// The speed settings, `[hispeed]` (design.md "Play Adjustments").
    pub hispeed: HiSpeedSettings,
    /// The lane covers, `[covers]`.
    pub covers: CoverSettings,
    /// The skin chosen for each screen kind, `[skins.<kind>]`; a kind
    /// without a table draws its built-in screen. Read once, at startup
    /// (design.md "Settings Files").
    #[serde(skip_serializing_if = "SkinChoices::is_empty")]
    pub skins: SkinChoices,
}

impl Default for PlayerSettings {
    fn default() -> Self {
        Self {
            preset: "beatoraja-style".to_owned(),
            displayed_gauge: 0,
            on_failure: FailureAction::Close,
            language: Language::Ja,
            hispeed: HiSpeedSettings::default(),
            covers: CoverSettings::default(),
            skins: SkinChoices::default(),
        }
    }
}

/// The language the player's own screen text is shown in (`language` in
/// `player.toml`); chart text and other data are shown as stored. A value
/// naming no language refuses the file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    /// Japanese, the base every other language falls back to.
    #[default]
    #[serde(rename = "ja")]
    Ja,
}

/// `[skins]` of `player.toml`: the skin chosen for each screen kind, by
/// the settings' own names of the kinds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkinChoices {
    /// `[skins.play]`: the play screen's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play: Option<SkinChoice>,
    /// `[skins.selection]`: the select screen's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<SkinChoice>,
    /// `[skins.decision]`: the decision screen's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<SkinChoice>,
    /// `[skins.result]`: the result screen's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<SkinChoice>,
}

impl SkinChoices {
    /// Whether no kind has a skin chosen.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.play.is_none()
            && self.selection.is_none()
            && self.decision.is_none()
            && self.result.is_none()
    }

    /// Each kind's table name and its choice, in startup order: play,
    /// selection, decision, result.
    #[must_use]
    pub const fn by_kind(&self) -> [(&'static str, Option<&SkinChoice>); 4] {
        [
            ("play", self.play.as_ref()),
            ("selection", self.selection.as_ref()),
            ("decision", self.decision.as_ref()),
            ("result", self.result.as_ref()),
        ]
    }
}

/// One `[skins.<kind>]` table: a skin folder, its definition, and the
/// user's options and file choices by name, resolved against the skin's
/// header when it is imported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkinChoice {
    /// The skin folder, an absolute path the user chose: every file the
    /// skin reads stays inside it.
    pub root: PathBuf,
    /// The definition, relative to `root`: a Lua skin when it ends in
    /// `.luaskin` or `.lua`, else a JSON definition.
    pub entry: String,
    /// Option name to the name of the item selected; an option not named
    /// takes its default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    /// File choice name to the candidate chosen; one not named takes its
    /// default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
}

impl SkinChoice {
    /// Checks that `root` is given and that `entry` is a name inside it:
    /// not empty, not rooted, drive, UNC, verbatim or stream (no `:`), no
    /// NUL, and no `..` leading above the folder. Containment, links
    /// included, is checked again when the skin is read.
    ///
    /// # Errors
    ///
    /// [`PlayerSettingsError::Skin`] for the first value refused, under
    /// table `kind`.
    pub fn check(&self, kind: &'static str) -> Result<(), PlayerSettingsError> {
        let refuse = |key, value: &str, why| {
            Err(PlayerSettingsError::Skin {
                kind,
                key,
                value: value.to_owned(),
                why,
            })
        };
        if self.root.as_os_str().is_empty() {
            return refuse("root", "", "is empty");
        }
        let entry = self.entry.as_str();
        if entry.is_empty() {
            return refuse("entry", entry, "is empty");
        }
        if entry.starts_with(['/', '\\']) || entry.contains([':', '\0']) {
            return refuse("entry", entry, "is not a name relative to root");
        }
        let mut depth = 0_usize;
        for part in entry.split(['/', '\\']) {
            match part {
                "" | "." => {}
                ".." => match depth.checked_sub(1) {
                    Some(d) => depth = d,
                    None => return refuse("entry", entry, "leads out of root"),
                },
                _ => depth += 1,
            }
        }
        Ok(())
    }
}

/// `[covers]` of `player.toml`: each lane cover's amount, ‰, and its
/// switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CoverSettings {
    /// SUDDEN+'s amount.
    pub sudden: u16,
    /// Whether SUDDEN+ is on.
    pub sudden_on: bool,
    /// LIFT's amount.
    pub lift: u16,
    /// Whether LIFT is on.
    pub lift_on: bool,
    /// HIDDEN+'s amount.
    pub hidden: u16,
    /// Whether HIDDEN+ is on.
    pub hidden_on: bool,
}

impl Default for CoverSettings {
    fn default() -> Self {
        Self::from(Covers::default())
    }
}

impl From<Covers> for CoverSettings {
    fn from(c: Covers) -> Self {
        Self {
            sudden: c.sudden.amount,
            sudden_on: c.sudden.on,
            lift: c.lift.amount,
            lift_on: c.lift.on,
            hidden: c.hidden.amount,
            hidden_on: c.hidden.on,
        }
    }
}

impl From<CoverSettings> for Covers {
    fn from(c: CoverSettings) -> Self {
        let cover = |amount, on| Cover { amount, on };
        Self {
            sudden: cover(c.sudden, c.sudden_on),
            lift: cover(c.lift, c.lift_on),
            hidden: cover(c.hidden, c.hidden_on),
        }
    }
}

impl PlayerSettings {
    /// The play options `[hispeed]` and `[covers]` set, validated
    /// ([`PlayOptions::new`]); settings [`Settings::load`] gave always have
    /// them.
    ///
    /// # Errors
    ///
    /// [`PlayerSettingsError::PlayOption`]: the first value outside its
    /// range, by table and key.
    pub fn options(&self) -> Result<PlayOptions, PlayerSettingsError> {
        PlayOptions::new(self.hispeed, self.covers.into()).map_err(|e| match e {
            PlayOptionsError::Invalid {
                setting,
                value,
                expected,
            } => PlayerSettingsError::PlayOption {
                section: if matches!(setting, "sudden" | "lift" | "hidden") {
                    "covers"
                } else {
                    "hispeed"
                },
                setting,
                value,
                expected,
            },
        })
    }

    /// Stores `options` as `[hispeed]` and `[covers]`, to be written.
    pub fn set_options(&mut self, options: &PlayOptions) {
        self.hispeed = options.hispeed();
        self.covers = options.covers().into();
    }

    /// Checks the play options ([`options`](Self::options)), the skin
    /// choices ([`SkinChoice::check`]), and that the preset is a
    /// built-in's name or one a user preset can take. Whether
    /// that user preset exists is checked when it is loaded ([`Shelf`]).
    ///
    /// # Errors
    ///
    /// The value that cannot be used, for the player.
    ///
    /// [`Shelf`]: crate::presets::Shelf
    pub fn validate(&self) -> Result<(), PlayerSettingsError> {
        self.options()?;
        for (kind, choice) in self.skins.by_kind() {
            if let Some(choice) = choice {
                choice.check(kind)?;
            }
        }
        if !rules::BUILT_IN_PRESETS.contains(&self.preset.as_str())
            && rules_toml::check_user_preset_name(&self.preset).is_err()
        {
            return Err(PlayerSettingsError::Preset(UnknownPreset(
                self.preset.clone(),
            )));
        }
        Ok(())
    }
}

/// A settings file's type: what [`load_settings`] reads, and what it
/// refuses before reading it as the type.
pub trait SettingsFile: DeserializeOwned + Default {
    /// Refuses a file of a layout this version does not read, from its
    /// TOML table; nothing is converted.
    ///
    /// # Errors
    ///
    /// The refusal.
    fn refuse(table: &toml::Table) -> Result<(), SettingsError>;
}

impl SettingsFile for DeviceSettings {
    /// Refuses a file without `version = 2` ([`DEVICE_SETTINGS_VERSION`]):
    /// [`SettingsError::DeviceVersion`]. A `version` that is not an integer
    /// is left to the typed parse.
    fn refuse(table: &toml::Table) -> Result<(), SettingsError> {
        let found = match table.get("version") {
            None => None,
            Some(toml::Value::Integer(v)) => Some(*v),
            Some(_) => return Ok(()),
        };
        if found == Some(i64::from(DEVICE_SETTINGS_VERSION)) {
            Ok(())
        } else {
            Err(SettingsError::DeviceVersion { found })
        }
    }
}

impl SettingsFile for PlayerSettings {
    /// Refuses a `visible_time_ms`
    /// ([`PlayerSettingsError::VisibleTimeRemoved`]), and an unknown key
    /// in `[skins]` or a `[skins.<kind>]` table by its table and key
    /// ([`PlayerSettingsError::SkinKey`]). A value of another type is left
    /// to the typed parse.
    fn refuse(table: &toml::Table) -> Result<(), SettingsError> {
        if table.contains_key("visible_time_ms") {
            return Err(SettingsError::InvalidPlayer(
                PlayerSettingsError::VisibleTimeRemoved,
            ));
        }
        let Some(toml::Value::Table(skins)) = table.get("skins") else {
            return Ok(());
        };
        let unknown = |table: String, key: &str| {
            SettingsError::InvalidPlayer(PlayerSettingsError::SkinKey {
                table,
                key: key.to_owned(),
            })
        };
        for (kind, choice) in skins {
            if !SKIN_KINDS.contains(&kind.as_str()) {
                return Err(unknown("skins".to_owned(), kind));
            }
            if let toml::Value::Table(choice) = choice
                && let Some(key) = choice.keys().find(|k| !SKIN_KEYS.contains(&k.as_str()))
            {
                return Err(unknown(format!("skins.{kind}"), key));
            }
        }
        Ok(())
    }
}

/// The kinds `[skins]` takes, by the settings' names.
const SKIN_KINDS: [&str; 4] = ["play", "selection", "decision", "result"];
/// The keys a `[skins.<kind>]` table takes.
const SKIN_KEYS: [&str; 4] = ["root", "entry", "options", "files"];

/// Reads a settings file; a file that does not exist gives the defaults.
/// The text is read as a TOML table first, which
/// [`SettingsFile::refuse`] checks, then as `T`.
///
/// # Errors
///
/// [`SettingsError::Io`] when the file exists but cannot be read,
/// [`SettingsError::Parse`] for text of another shape, including an
/// unknown key and a TOML float where an integer belongs, and what
/// [`SettingsFile::refuse`] refuses.
pub fn load_settings<T: SettingsFile>(path: &Path) -> Result<T, SettingsError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let table: toml::Table = toml::from_str(&text).map_err(SettingsError::Parse)?;
            T::refuse(&table)?;
            toml::from_str(&text).map_err(SettingsError::Parse)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

/// Writes a settings file atomically, creating its directory.
///
/// # Errors
///
/// [`SettingsError::Io`] when the directory or the file cannot be written.
///
/// # Panics
///
/// Never for the settings types here: every field has a TOML form.
pub fn save_settings<T: Serialize>(path: &Path, settings: &T) -> Result<(), SettingsError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = toml::to_string(settings).expect("every settings field has a TOML form");
    write_atomic(path, text.as_bytes())?;
    Ok(())
}
