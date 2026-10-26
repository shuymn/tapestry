//! Settings files (stage1.md D10): `device.toml` for this PC and its
//! devices, `player.toml` for the person playing, both in one data root
//! with `presets/` and `records/`. The data root is a parameter; the player
//! resolves the real one.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bms_model::{Lane, TimeUs};
use eval::{Binding, ControlId, DeviceProfile, SourceProfile};
use input::DeviceKind;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::save::write_atomic;

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
        }
    }
}

impl std::error::Error for SettingsError {}

/// A control bound to a lane, in a settings file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneKey {
    /// The control: a keyboard scan code, or a controller's button.
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

impl KeyConfig {
    /// Every recorded control: the lanes and the system controls.
    #[must_use]
    pub fn recorded(&self) -> Vec<ControlId> {
        self.lanes
            .iter()
            .map(|k| ControlId(k.control))
            .chain(self.retry.map(ControlId))
            .chain(self.quit.map(ControlId))
            .collect()
    }

    /// The lane bindings the evaluator reads; an entry with a lane the
    /// game does not have is left out.
    #[must_use]
    pub fn bindings(&self) -> Vec<Binding> {
        self.lanes
            .iter()
            .filter_map(|k| {
                Lane::new(k.lane).map(|lane| Binding {
                    control: ControlId(k.control),
                    lane,
                })
            })
            .collect()
    }
}

/// `device.toml`: what belongs to this PC and its devices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DeviceSettings {
    /// The key config shared by every keyboard.
    pub keyboard: KeyConfig,
    /// The key config of controllers. Per-device controller bindings
    /// arrive with HID support.
    pub controller: KeyConfig,
    /// The input latency offset, µs.
    pub input_offset_us: i64,
    /// The audio output latency offset, µs.
    pub audio_offset_us: i64,
    /// The display latency offset, µs.
    pub display_offset_us: i64,
    /// The audio device's id; the system default when absent.
    pub audio_device: Option<String>,
    /// The master volume, 0.0 to 1.0 being unity or below.
    pub master_volume: f32,
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

    /// The profile the evaluator reads for sources of the given kinds,
    /// indexed by source id.
    #[must_use]
    pub fn profile(&self, kinds: &[DeviceKind]) -> DeviceProfile {
        DeviceProfile {
            sources: kinds
                .iter()
                .map(|&kind| SourceProfile {
                    input_offset: TimeUs(self.input_offset_us),
                    bindings: self.keys(kind).bindings(),
                })
                .collect(),
        }
    }
}

/// `player.toml`: what belongs to the person playing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PlayerSettings {
    /// The scroll speed, in thousandths.
    pub hi_speed_milli: u32,
    /// The preset file in `presets/`.
    pub preset: String,
    /// The gauge shown, by its index in the preset.
    pub displayed_gauge: usize,
}

impl Default for PlayerSettings {
    fn default() -> Self {
        Self {
            hi_speed_milli: 1_000,
            preset: "custom.toml".to_owned(),
            displayed_gauge: 0,
        }
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
