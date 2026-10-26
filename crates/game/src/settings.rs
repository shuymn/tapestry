//! Settings files (stage1.md D10): `device.toml` for this PC and its
//! devices, `player.toml` for the person playing, both in one data root
//! with `presets/` and `records/`. The data root is a parameter; the player
//! resolves the real one.

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
use input::hid::{AxisCap, is_axis};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::save::write_atomic;
use crate::tuning::{Adjust, DEFAULT_VISIBLE_TIME_MS, OFFSET_LIMIT_US, VISIBLE_TIME_MS};

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

    /// The preset file named `name` in `presets/`.
    #[must_use]
    pub fn preset(&self, name: &str) -> PathBuf {
        self.0.join("presets").join(name)
    }

    /// `records/`.
    #[must_use]
    pub fn records(&self) -> PathBuf {
        self.0.join("records")
    }
}

/// Why a settings file could not be read or written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsError {
    /// The file system refused.
    Io {
        /// The kind of failure.
        kind: io::ErrorKind,
        /// The error, as the system reported it.
        message: String,
    },
    /// The file is not TOML of the settings' shape.
    Parse(String),
    /// The file reads, but a value cannot be used: a key config binding
    /// one control to two roles or a reserved key, a lane the game does
    /// not have, or a setting outside its range.
    Invalid(String),
}

impl From<io::Error> for SettingsError {
    fn from(e: io::Error) -> Self {
        Self::Io {
            kind: e.kind(),
            message: e.to_string(),
        }
    }
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message, .. } => write!(f, "the settings file cannot be used: {message}"),
            Self::Parse(e) => write!(f, "the settings file cannot be read: {e}"),
            Self::Invalid(e) => write!(f, "the settings cannot be used: {e}"),
        }
    }
}

impl std::error::Error for SettingsError {}

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

/// One kind of device's key config: lanes and system controls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    /// The lane bindings.
    pub lanes: Vec<LaneKey>,
    /// The control that retries, if bound.
    pub retry: Option<u32>,
    /// The control that quits, if bound.
    pub quit: Option<u32>,
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
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lane(0) => f.write_str("scratch"),
            Self::Lane(n) => write!(f, "key {n}"),
            Self::Retry => f.write_str("retry"),
            Self::Quit => f.write_str("quit"),
        }
    }
}

/// Why a key config cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// One control bound to two roles.
    Overlap {
        /// The control.
        control: u32,
        /// The role it was bound to first.
        first: Role,
        /// The role it was bound to again.
        second: Role,
    },
    /// A keyboard key the game reserves for something else.
    Reserved {
        /// The key's scan code.
        control: u32,
        /// What the game uses it for.
        reserved_for: &'static str,
    },
    /// A lane the game does not have.
    NoSuchLane(u8),
    /// Retry or quit bound to something other than a keyboard key: stage 1
    /// reads them from the window's keyboard messages only.
    NotAKeyboardKey(Role),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overlap {
                control,
                first,
                second,
            } => write!(
                f,
                "control {control:#x} is bound to both {first} and {second}"
            ),
            Self::Reserved {
                control,
                reserved_for,
            } => write!(f, "key {control:#x} is reserved for {reserved_for}"),
            Self::NoSuchLane(l) => write!(f, "lane {l} does not exist (lanes are 0 to 7)"),
            Self::NotAKeyboardKey(r) => write!(f, "{r} takes keyboard keys only"),
        }
    }
}

impl std::error::Error for KeyError {}

/// What the game reserves keyboard key `code` (a scan code) for, if
/// anything: the settings keys and the key config's keep key are never
/// bound.
#[must_use]
pub fn reserved_key(code: u32) -> Option<&'static str> {
    if code == crate::keys::KEEP_KEY {
        return Some("keeping a binding in key config");
    }
    Adjust::from_key(code).map(Adjust::name)
}

impl KeyConfig {
    /// Every binding with its role: the lanes, then retry and quit.
    pub fn roles(&self) -> impl Iterator<Item = (u32, Role)> + '_ {
        self.lanes
            .iter()
            .map(|k| (k.control, Role::Lane(k.lane)))
            .chain(self.retry.map(|c| (c, Role::Retry)))
            .chain(self.quit.map(|c| (c, Role::Quit)))
    }

    /// Checks the key config of a kind of device: every lane exists, no
    /// control has two roles (keyboard keys by
    /// [`key_identity`](input::raw::key_identity)), a keyboard binds no reserved key, and only a
    /// keyboard binds retry and quit.
    ///
    /// # Errors
    ///
    /// The first binding that breaks a rule.
    pub fn check(&self, kind: DeviceKind) -> Result<(), KeyError> {
        let mut seen: Vec<(u32, Role)> = Vec::new();
        for (control, role) in self.roles() {
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
        Ok(())
    }

    /// Every recorded control: the lanes and the system controls.
    #[must_use]
    pub fn recorded(&self) -> Vec<ControlId> {
        self.roles().map(|(c, _)| ControlId(c)).collect()
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
/// input log records.
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
}

/// `device.toml`: what belongs to this PC and its devices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DeviceSettings {
    /// The key config shared by every keyboard.
    pub keyboard: KeyConfig,
    /// The key config shared by every controller: its buttons and axes by
    /// HID usage ([`input::hid`]).
    pub controller: KeyConfig,
    /// The input latency offset, µs.
    pub input_offset_us: i64,
    /// The audio output latency offset, µs.
    pub audio_offset_us: i64,
    /// The display latency offset, µs.
    pub display_offset_us: i64,
    /// The audio device's id; the system default when absent.
    pub audio_device: Option<String>,
    /// The master volume, 0.0 (silent) to 1.0 (unity).
    pub master_volume: f32,
    /// How controllers' axes play their lanes.
    pub scratch: ScratchConfig,
}

impl Default for DeviceSettings {
    fn default() -> Self {
        // Scan codes: left shift for the scratch, then Z S X D C F V.
        let codes = [0x2a, 0x2c, 0x1f, 0x2d, 0x20, 0x2e, 0x21, 0x2f];
        Self {
            keyboard: KeyConfig {
                lanes: (0..)
                    .zip(codes)
                    .map(|(lane, control)| LaneKey { control, lane })
                    .collect(),
                // Backspace retries and Escape quits.
                retry: Some(0x0e),
                quit: Some(0x01),
            },
            controller: KeyConfig {
                lanes: Vec::new(),
                retry: None,
                quit: None,
            },
            input_offset_us: 0,
            audio_offset_us: 0,
            display_offset_us: 0,
            audio_device: None,
            master_volume: 1.0,
            scratch: ScratchConfig::default(),
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
    /// configs ([`KeyConfig::check`]), the input and display offsets within
    /// ±[`OFFSET_LIMIT_US`], and the master volume from 0.0 to 1.0.
    ///
    /// # Errors
    ///
    /// The first value that cannot be used, for the player.
    pub fn validate(&self) -> Result<(), String> {
        self.keyboard
            .check(DeviceKind::Keyboard)
            .map_err(|e| format!("keyboard key config: {e}"))?;
        self.controller
            .check(DeviceKind::Controller)
            .map_err(|e| format!("controller key config: {e}"))?;
        for (name, us) in [
            ("input_offset_us", self.input_offset_us),
            ("display_offset_us", self.display_offset_us),
        ] {
            if !(-OFFSET_LIMIT_US..=OFFSET_LIMIT_US).contains(&us) {
                return Err(format!(
                    "{name} {us} is outside -{OFFSET_LIMIT_US} to {OFFSET_LIMIT_US}"
                ));
            }
        }
        if !(0.0..=1.0).contains(&self.master_volume) {
            return Err(format!(
                "master_volume {} is outside 0.0 to 1.0",
                self.master_volume
            ));
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
    /// Hi-speed as the visible time, ms: how long a note takes from the
    /// top of the lane to the judgment line at the chart's start BPM (D9).
    pub visible_time_ms: u32,
    /// The preset file in `presets/`.
    pub preset: String,
    /// The gauge shown, by its index in the preset.
    pub displayed_gauge: usize,
}

impl Default for PlayerSettings {
    fn default() -> Self {
        Self {
            visible_time_ms: DEFAULT_VISIBLE_TIME_MS,
            preset: "custom.toml".to_owned(),
            displayed_gauge: 0,
        }
    }
}

impl PlayerSettings {
    /// Checks the visible time is within [`VISIBLE_TIME_MS`].
    ///
    /// # Errors
    ///
    /// The value outside its range, for the player.
    pub fn validate(&self) -> Result<(), String> {
        if !VISIBLE_TIME_MS.contains(&self.visible_time_ms) {
            return Err(format!(
                "visible_time_ms {} is outside {} to {}",
                self.visible_time_ms,
                VISIBLE_TIME_MS.start(),
                VISIBLE_TIME_MS.end()
            ));
        }
        Ok(())
    }
}

/// Reads a settings file; a file that does not exist gives the defaults.
///
/// # Errors
///
/// [`SettingsError::Io`] when the file exists but cannot be read, and
/// [`SettingsError::Parse`] for text of another shape, including an
/// unknown key.
pub fn load_settings<T: DeserializeOwned + Default>(path: &Path) -> Result<T, SettingsError> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| SettingsError::Parse(e.to_string())),
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
