//! The shelf of presets (design.md "Standard Presets", "User Presets",
//! "Tuning Without Restart"): the built-ins, which live in the code and are
//! never read from disk or written to it, and the user presets, the TOML
//! files in `presets/`, which alone are read, validated and polled.
//!
//! A check lists `presets/` again and reads each file whose modification
//! time changed since the last check; a file that fails to load keeps the
//! preset it last loaded, with its error. The chosen preset is the one plays
//! use, so a failed reload of it leaves the previous one in use.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bms_model::TimeUs;
use rules::{BUILT_IN_PRESETS, RulePreset, built_in};
use rules_toml::{PresetFileError, check_user_preset_name, read_preset_file};

/// How often the shelf checks the user preset files while a select or result
/// screen shows.
pub const POLL_INTERVAL: TimeUs = TimeUs(250_000);

/// A preset name that names neither a built-in nor a user preset.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "`{0}` is neither a built-in preset ({built_ins}) nor a user preset in the presets folder",
    built_ins = BUILT_IN_PRESETS.join(", ")
)]
pub struct UnknownPreset(pub String);

/// Why the shelf could not be opened with the chosen preset.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ShelfError {
    /// The chosen name is neither a built-in nor a user preset.
    #[error(transparent)]
    Unknown(UnknownPreset),
    /// The chosen user preset's file could not be used.
    #[error(transparent)]
    Preset(PresetFileError),
    /// The presets folder could not be listed for a chosen user preset.
    #[error("the presets folder {} cannot be listed: {kind}", .path.display())]
    Io {
        /// The folder.
        path: PathBuf,
        /// The kind of failure.
        kind: io::ErrorKind,
    },
}

/// What a check found for the chosen user preset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresetChange {
    /// Its file changed and loaded to other rules, or loaded again after a
    /// failure: the chosen preset is the loaded one.
    Reloaded,
    /// Its file changed, or went away, and does not load: the previous
    /// preset stays chosen.
    Failed(PresetFileError),
}

/// One preset the shelf lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// Its name: a built-in's, or a user preset file's stem.
    pub name: &'a str,
    /// Whether it is a built-in.
    pub built_in: bool,
    /// The preset, unless its file never loaded.
    pub preset: Option<&'a RulePreset>,
    /// Why its file did not load at the last check, if it did not.
    pub error: Option<&'a PresetFileError>,
}

/// A user preset file as the last check left it.
#[derive(Clone, Debug)]
struct UserPreset {
    /// The modification time the last read was for.
    modified: Option<SystemTime>,
    /// What the file last loaded to.
    preset: Option<RulePreset>,
    /// Why the last read failed.
    error: Option<PresetFileError>,
}

/// The built-in presets and the user preset files, with the chosen one.
#[derive(Clone, Debug)]
pub struct Shelf {
    dir: PathBuf,
    built_ins: Vec<RulePreset>,
    /// The user preset files by stem, refused names included, so a refused
    /// file is listed with its error.
    users: BTreeMap<String, UserPreset>,
    chosen: String,
    /// Whether the chosen preset is a user preset, not a built-in: a file
    /// named like a built-in is listed under the built-in's name too.
    user_chosen: bool,
    /// The chosen preset: plays take it at their start.
    current: RulePreset,
    next_check: Option<TimeUs>,
}

impl Shelf {
    /// Lists the built-ins and the user presets in `dir`, choosing `chosen`.
    /// A built-in is chosen without reading any file; a folder that cannot
    /// be listed then lists only the built-ins.
    ///
    /// # Errors
    ///
    /// [`ShelfError::Unknown`] when `chosen` is neither a built-in nor a
    /// user preset file in `dir`; [`ShelfError::Preset`] when the chosen
    /// user preset's file does not load; [`ShelfError::Io`] when the folder
    /// cannot be listed for a chosen user preset.
    pub fn open(dir: PathBuf, chosen: &str) -> Result<Self, ShelfError> {
        let unknown = || ShelfError::Unknown(UnknownPreset(chosen.to_owned()));
        let built_in_chosen = built_in(chosen);
        if built_in_chosen.is_none() {
            check_user_preset_name(chosen).map_err(|_| unknown())?;
        }
        let user_chosen = built_in_chosen.is_none().then_some(chosen);
        let mut users = BTreeMap::new();
        match list(&dir) {
            Ok(files) => read(&mut users, &files, &dir, user_chosen),
            Err(kind) if built_in_chosen.is_none() => {
                return Err(ShelfError::Io { path: dir, kind });
            }
            Err(_) => {}
        }
        let current = match built_in_chosen {
            Some(p) => p,
            None => {
                let user: &UserPreset = users.get(chosen).ok_or_else(unknown)?;
                match (&user.preset, &user.error) {
                    (_, Some(e)) => return Err(ShelfError::Preset(e.clone())),
                    (Some(p), None) => p.clone(),
                    (None, None) => return Err(unknown()),
                }
            }
        };
        Ok(Self {
            dir,
            built_ins: BUILT_IN_PRESETS
                .iter()
                .filter_map(|n| built_in(n))
                .collect(),
            users,
            chosen: chosen.to_owned(),
            user_chosen: user_chosen.is_some(),
            current,
            next_check: None,
        })
    }

    /// The chosen preset's name.
    #[must_use]
    pub fn chosen_name(&self) -> &str {
        &self.chosen
    }

    /// The chosen preset, as plays use it from their start.
    #[must_use]
    pub const fn chosen(&self) -> &RulePreset {
        &self.current
    }

    /// Every preset: the built-ins in their order, then the user preset
    /// files by name.
    #[must_use]
    pub fn entries(&self) -> Vec<Entry<'_>> {
        let built_ins = self.built_ins.iter().map(|p| Entry {
            name: &p.id,
            built_in: true,
            preset: Some(p),
            error: None,
        });
        let users = self.users.iter().map(|(name, u)| Entry {
            name,
            built_in: false,
            preset: u.preset.as_ref(),
            error: u.error.as_ref(),
        });
        built_ins.chain(users).collect()
    }

    /// Checks the user preset files at `now` if [`POLL_INTERVAL`] has passed
    /// since the last poll's check (the first poll checks at once).
    pub fn poll(&mut self, now: TimeUs) -> Option<PresetChange> {
        if self.next_check.is_some_and(|t| now.0 < t.0) {
            return None;
        }
        self.next_check = Some(TimeUs(now.0.saturating_add(POLL_INTERVAL.0)));
        self.check()
    }

    /// Lists the folder again and reads each user preset file whose
    /// modification time changed. Returns what changed for the chosen user
    /// preset; `None` for a built-in, or when its file did not change.
    pub fn check(&mut self) -> Option<PresetChange> {
        let chosen = self.user_chosen.then_some(self.chosen.as_str());
        let before = chosen
            .and_then(|c| self.users.get(c))
            .and_then(|u| u.error.clone());
        match list(&self.dir) {
            Ok(files) => read(&mut self.users, &files, &self.dir, chosen),
            Err(kind) => {
                // Nothing is known about the files: each keeps its state,
                // and the chosen one reports the failure.
                if let Some(u) = chosen.and_then(|c| self.users.get_mut(c)) {
                    u.error = Some(PresetFileError::io(self.dir.clone(), kind));
                    u.modified = None;
                }
            }
        }
        let user = self.users.get(chosen?)?;
        // Loading again after a failure is a change even with the rules
        // unchanged, so the failure shown can be cleared.
        let recovered = before.is_some() && user.error.is_none();
        if let Some(p) = &user.preset
            && (*p != self.current || recovered)
        {
            self.current = p.clone();
            return Some(PresetChange::Reloaded);
        }
        match &user.error {
            Some(e) if before.as_ref() != Some(e) => Some(PresetChange::Failed(e.clone())),
            _ => None,
        }
    }
}

/// A `.toml` file in the presets folder: its stem, path and modification
/// time.
type Listed = (String, PathBuf, Option<SystemTime>);

/// Updates `users` from the folder `dir`'s `files`; `chosen` is the chosen
/// user preset's name, `None` when a built-in is chosen.
fn read(
    users: &mut BTreeMap<String, UserPreset>,
    files: &[Listed],
    dir: &Path,
    chosen: Option<&str>,
) {
    for (name, path, modified) in files {
        let known = users.get(name);
        if modified.is_some() && known.is_some_and(|u| u.modified == *modified) {
            continue;
        }
        let previous = known.and_then(|u| u.preset.clone());
        let user = match read_preset_file(path) {
            Ok((preset, read_at)) => UserPreset {
                modified: read_at,
                preset: Some(preset),
                error: None,
            },
            Err(e) => UserPreset {
                modified: *modified,
                preset: previous,
                error: Some(e),
            },
        };
        users.insert(name.clone(), user);
    }
    // A file gone is dropped, except the chosen one's: its preset stays
    // in use, with the reason it cannot be read.
    users.retain(|name, u| {
        if files.iter().any(|(n, ..)| n == name) {
            return true;
        }
        if chosen != Some(name.as_str()) {
            return false;
        }
        u.error = Some(PresetFileError::io(
            user_preset_path(dir, name),
            io::ErrorKind::NotFound,
        ));
        u.modified = None;
        true
    });
}

/// Where the user preset `name` lives in `dir`.
fn user_preset_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.toml"))
}

/// The `.toml` files in `dir`, each with its stem and modification time; a
/// folder that does not exist holds none.
fn list(dir: &Path) -> Result<Vec<Listed>, io::ErrorKind> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.kind()),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.kind())?;
        let path = entry.path();
        let is_toml = path
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("toml"));
        let Some(stem) = path.file_stem() else {
            continue;
        };
        // The entry's own type and time, never a link's target's: a link is
        // listed, and its read refuses it naming the file.
        let file_type = entry.file_type().ok();
        if !is_toml || file_type.is_some_and(|t| t.is_dir()) {
            continue;
        }
        let modified = fs::symlink_metadata(&path).and_then(|m| m.modified()).ok();
        files.push((stem.to_string_lossy().into_owned(), path, modified));
    }
    Ok(files)
}
