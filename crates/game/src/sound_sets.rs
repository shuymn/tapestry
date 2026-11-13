//! Sound sets (design.md "Audio", system sounds; "Threat Model", T18): the
//! `[sounds]` settings, the scan for sets, one pick's resolution of each
//! kind's file, the recovery state that keeps a set that crashed a launch
//! from crashing the next, and the decode of one pick into a
//! [`SystemGeneration`].
//!
//! - [`scan`] lists the folders under a root without entering a link or
//!   junction, and keeps each folder whose own marker resolves (`clear` for
//!   a sound set, `select` for a BGM set), within [`ScanLimits`].
//! - [`resolve`] finds each kind's file: its `.wav` name, then `.flac`,
//!   `.ogg` and `.mp3`, first in the picked set and then in `defaultsound\`.
//!   A candidate must stay inside its folder once canonicalized and be a
//!   regular file; one that does not is refused and named, and lookup moves
//!   on.
//! - [`Recovery`] owns `sound-sets.toml` under the exclusive lock
//!   `sound-sets.lock`: a source's identity is committed as the marker
//!   before any of its files is read for decoding and cleared after, and a
//!   marker found at startup quarantines that identity.
//! - [`decode_pick`] decodes a pick source by source under its marker,
//!   charging one [`PickBudget`], and carries a kind with no candidate over
//!   from the previous pick ([`KeptSounds`]).
//!
//! The candidates' names are this module's constants, so they pass the
//! chart sounds' name rules ("Loading Limits") by construction; their
//! containment and file type are checked on disk.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use ::audio::{
    BankLimits, Decoded, FileRefusal, GenId, PickBudget, SYSTEM_KINDS, Sound, SystemGeneration,
    SystemKind,
};
use serde::{Deserialize, Serialize};

use crate::error::IoFailure;
use crate::save::write_atomic;
use crate::settings::{DataRoot, PlayerSettingsError};

/// The decoded bytes one pick may hold (design.md "Memory Budget").
pub const PICK_BYTES: u64 = 128 * 1024 * 1024;

/// The version of `sound-sets.toml` this build reads and writes.
pub const SOUND_SETS_VERSION: i64 = 1;

/// The extensions a kind's file is tried with, in order.
pub const EXTENSIONS: [&str; 4] = ["wav", "flac", "ogg", "mp3"];

/// The keys `[sounds]` takes.
pub(crate) const SOUND_KEYS: [&str; 2] = ["sound_root", "bgm_root"];

/// `[sounds]` of `player.toml`: where the sets are found. Absent, a root is
/// the data root's `sound\` or `bgm\`; empty, no set of that kind is found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SoundSettings {
    /// The sound sets' root, an absolute path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound_root: Option<PathBuf>,
    /// The BGM sets' root, an absolute path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bgm_root: Option<PathBuf>,
}

impl SoundSettings {
    /// Whether nothing is set, so nothing is written.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Checks that each root given is empty or absolute.
    ///
    /// # Errors
    ///
    /// [`PlayerSettingsError::SoundRoot`] for the first root refused.
    pub fn check(&self) -> Result<(), PlayerSettingsError> {
        for (key, root) in [
            ("sound_root", &self.sound_root),
            ("bgm_root", &self.bgm_root),
        ] {
            if let Some(root) = root
                && !root.as_os_str().is_empty()
                && !root.is_absolute()
            {
                return Err(PlayerSettingsError::SoundRoot {
                    key,
                    value: root.display().to_string(),
                });
            }
        }
        Ok(())
    }

    /// The root of `role`'s sets, `None` when it is set empty.
    #[must_use]
    pub fn root(&self, role: SetRole, data: &DataRoot) -> Option<PathBuf> {
        let (given, default) = match role {
            SetRole::Sound => (&self.sound_root, data.sound()),
            SetRole::Bgm => (&self.bgm_root, data.bgm()),
        };
        match given {
            None => Some(default),
            Some(p) if p.as_os_str().is_empty() => None,
            Some(p) => Some(p.clone()),
        }
    }
}

/// Which sets a root holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SetRole {
    /// Sound sets: every kind but `select` and `decide`; marked by `clear`.
    Sound,
    /// BGM sets: `select` and `decide`; marked by `select`.
    Bgm,
}

impl SetRole {
    /// The kind whose file makes a folder a set of this role.
    #[must_use]
    pub const fn marker(self) -> SystemKind {
        match self {
            Self::Sound => SystemKind::Clear,
            Self::Bgm => SystemKind::Select,
        }
    }

    /// The role of the set that gives `kind`.
    #[must_use]
    pub const fn of(kind: SystemKind) -> Self {
        match kind {
            SystemKind::Select | SystemKind::Decide => Self::Bgm,
            _ => Self::Sound,
        }
    }

    /// The kinds a set of this role gives.
    fn kinds(self) -> impl Iterator<Item = SystemKind> {
        SystemKind::ALL
            .into_iter()
            .filter(move |&k| Self::of(k) == self)
    }
}

/// The file name of `kind` without its extension, as the source names it.
#[must_use]
pub const fn file_stem(kind: SystemKind) -> &'static str {
    match kind {
        SystemKind::Select => "select",
        SystemKind::Decide => "decide",
        SystemKind::Scratch => "scratch",
        SystemKind::FolderOpen => "f-open",
        SystemKind::FolderClose => "f-close",
        SystemKind::OptionOpen => "o-open",
        SystemKind::OptionClose => "o-close",
        SystemKind::OptionChange => "o-change",
        SystemKind::PlayReady => "playready",
        SystemKind::PlayStop => "playstop",
        SystemKind::Clear => "clear",
        SystemKind::Fail => "fail",
        SystemKind::ResultClose => "resultclose",
    }
}

/// Bounds on a scan. Provisional: no source value or measurement fixes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanLimits {
    /// The deepest folder entered, the root at depth 0.
    pub depth: usize,
    /// Folders listed per root, the root included.
    pub folders: usize,
    /// Entries listed per root.
    pub entries: usize,
}

impl Default for ScanLimits {
    /// Depth 8, 4 096 folders and 65 536 entries.
    fn default() -> Self {
        Self {
            depth: 8,
            folders: 4_096,
            entries: 65_536,
        }
    }
}

/// A scan bound that was reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanCap {
    /// A folder deeper than [`ScanLimits::depth`] was not entered.
    Depth,
    /// [`ScanLimits::folders`] were listed: the scan stopped.
    Folders,
    /// [`ScanLimits::entries`] were listed: the scan stopped.
    Entries,
}

impl fmt::Display for ScanCap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let limits = ScanLimits::default();
        match self {
            Self::Depth => write!(f, "folders deeper than {} were not searched", limits.depth),
            Self::Folders => write!(f, "the search stopped after {} folders", limits.folders),
            Self::Entries => write!(f, "the search stopped after {} entries", limits.entries),
        }
    }
}

/// What a scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scanned {
    /// The sets' canonical folders, sorted.
    pub sets: Vec<PathBuf>,
    /// The caps reached, each once.
    pub caps: Vec<ScanCap>,
}

/// Lists the folders under `root`, at any depth and the root included, and
/// keeps those whose own `role` marker resolves ([`SetRole::marker`],
/// looked up in that folder only). A link or junction is never entered, a
/// linked root included. At a cap the scan stops with what it found
/// ([`ScanCap`]).
#[must_use]
pub fn scan(root: &Path, role: SetRole, limits: &ScanLimits) -> Scanned {
    let mut out = Scanned::default();
    let is_folder = fs::symlink_metadata(root)
        .is_ok_and(|m| m.file_type().is_dir() && !m.file_type().is_symlink());
    let Some(root) = is_folder.then(|| root.canonicalize().ok()).flatten() else {
        return out;
    };
    let cap = |out: &mut Scanned, c| {
        if !out.caps.contains(&c) {
            out.caps.push(c);
        }
    };
    let (mut listed, mut entries) = (0_usize, 0_usize);
    let mut stack = vec![(root, 0_usize)];
    'scan: while let Some((folder, depth)) = stack.pop() {
        if listed == limits.folders {
            cap(&mut out, ScanCap::Folders);
            break;
        }
        listed += 1;
        let Ok(list) = fs::read_dir(&folder) else {
            continue;
        };
        let mut children = Vec::new();
        for entry in list {
            // A folder whose listing the cap cuts is not kept.
            if entries == limits.entries {
                cap(&mut out, ScanCap::Entries);
                break 'scan;
            }
            entries += 1;
            let Ok(entry) = entry else { continue };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && !kind.is_symlink() {
                if depth == limits.depth {
                    cap(&mut out, ScanCap::Depth);
                } else {
                    children.push(entry.path());
                }
            }
        }
        set_if_marked(&folder, role, &mut out);
        children.sort();
        stack.extend(children.into_iter().rev().map(|c| (c, depth + 1)));
    }
    out.sets.sort();
    out
}

fn set_if_marked(folder: &Path, role: SetRole, out: &mut Scanned) {
    if locate(folder, role.marker(), &mut Vec::new()).is_some() {
        out.sets.push(folder.to_owned());
    }
}

/// Why a candidate file was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// It leads out of its folder once links are resolved.
    Escapes,
    /// It is not a regular file (a folder, a FIFO, a device).
    NotAFile,
    /// It could not be resolved (a dangling link).
    Unresolved,
    /// Its decode refused it.
    File(FileRefusal),
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Escapes => f.write_str("it leads out of its folder"),
            Self::NotAFile => f.write_str("it is not a regular file"),
            Self::Unresolved => f.write_str("it cannot be resolved"),
            Self::File(r) => r.fmt(f),
        }
    }
}

/// A file refused, to be named once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// The candidate's path.
    pub path: PathBuf,
    /// Why.
    pub why: Why,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "system sound {} was refused: {}",
            self.path.display(),
            self.why
        )
    }
}

/// `kind`'s file in the canonical `folder`: the first of its names with
/// [`EXTENSIONS`] that exists, stays inside the folder and is a regular
/// file. Each one that exists but fails is refused into `refused` and the
/// next is tried. Nothing is opened: a FIFO is refused by its type.
fn locate(folder: &Path, kind: SystemKind, refused: &mut Vec<Refused>) -> Option<PathBuf> {
    for ext in EXTENSIONS {
        let joined = folder.join(format!("{}.{ext}", file_stem(kind)));
        if fs::symlink_metadata(&joined).is_err() {
            continue;
        }
        let mut refuse = |why| {
            refused.push(Refused {
                path: joined.clone(),
                why,
            });
        };
        let Ok(canonical) = joined.canonicalize() else {
            refuse(Why::Unresolved);
            continue;
        };
        if !canonical.starts_with(folder) {
            refuse(Why::Escapes);
            continue;
        }
        if !fs::metadata(&canonical).is_ok_and(|m| m.is_file()) {
            refuse(Why::NotAFile);
            continue;
        }
        return Some(canonical);
    }
    None
}

/// One file of a source's identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileStamp {
    /// Its name in the source's folder.
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
    /// Its modification time, ns since the Unix epoch.
    pub mtime_ns: i64,
}

/// A source's identity (design.md T18): its canonical folder with the name,
/// size and modification time of each of its candidate files that exist,
/// sorted by name. A change of any of them makes another identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentity {
    /// The canonical folder.
    pub path: PathBuf,
    /// Its candidate files that exist.
    pub files: Vec<FileStamp>,
}

impl SourceIdentity {
    /// The identity of the canonical `folder` over the candidates of
    /// `kinds`.
    fn of(folder: &Path, kinds: impl Iterator<Item = SystemKind>) -> Self {
        let mut files = Vec::new();
        for kind in kinds {
            for ext in EXTENSIONS {
                let name = format!("{}.{ext}", file_stem(kind));
                let path = folder.join(&name);
                let Ok(meta) = fs::metadata(&path).or_else(|_| fs::symlink_metadata(&path)) else {
                    continue;
                };
                let mtime_ns =
                    meta.modified()
                        .ok()
                        .map_or(0, |t| match t.duration_since(UNIX_EPOCH) {
                            Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
                            Err(e) => {
                                i64::try_from(e.duration().as_nanos()).map_or(i64::MIN, |n| -n)
                            }
                        });
                files.push(FileStamp {
                    name,
                    size: meta.len(),
                    mtime_ns,
                });
            }
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            path: folder.to_owned(),
            files,
        }
    }
}

impl fmt::Display for SourceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.path.display().fmt(f)
    }
}

/// Where a kind's file comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// The picked sound set.
    SoundSet,
    /// The picked BGM set.
    BgmSet,
    /// `defaultsound\`.
    Defaults,
}

impl Source {
    /// Every source, in the order a pick decodes them.
    pub const ALL: [Self; 3] = [Self::SoundSet, Self::BgmSet, Self::Defaults];

    const fn index(self) -> usize {
        self as usize
    }
}

/// What a pick decodes: at most one set of each role, chosen beforehand,
/// and `defaultsound\` behind them.
#[derive(Clone, Debug)]
pub struct SetPick {
    /// The sound set, if one was chosen.
    pub sound_set: Option<PathBuf>,
    /// The BGM set, if one was chosen.
    pub bgm_set: Option<PathBuf>,
    /// `defaultsound\`.
    pub defaults: PathBuf,
    /// The generation's id.
    pub id: GenId,
    /// The device rate the sounds are decoded at.
    pub rate: u32,
    /// The per-file caps.
    pub limits: BankLimits,
    /// Sources skipped for the run (a decode of theirs panicked).
    pub skipped: Vec<SourceIdentity>,
}

/// A kind's file after resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The file, from `source`.
    Found {
        /// The canonical path.
        path: PathBuf,
        /// Where it comes from.
        source: Source,
    },
    /// Every candidate that exists was refused: the kind is silent.
    Refused,
    /// No candidate exists: the kind keeps its previous sound.
    Missing,
}

/// One pick's resolution.
#[derive(Clone, Debug)]
pub struct Resolution {
    /// Per kind ([`SystemKind::index`]).
    pub kinds: [Resolved; SYSTEM_KINDS],
    /// Per source ([`Source::ALL`]), its identity when it takes part: it
    /// exists and is not excluded.
    pub sources: [Option<SourceIdentity>; 3],
    /// The candidates refused.
    pub refused: Vec<Refused>,
}

/// Resolves each kind of `pick`: its `.wav` name, then `.flac`, `.ogg` and
/// `.mp3`, in the picked set of its role and then in `defaultsound\`. A
/// source that `excluded` names (quarantined or skipped) takes no part, as
/// if absent.
pub fn resolve(pick: &SetPick, excluded: &mut dyn FnMut(&SourceIdentity) -> bool) -> Resolution {
    let mut sources: [Option<SourceIdentity>; 3] = Default::default();
    for (source, folder, kinds) in [
        (
            Source::SoundSet,
            pick.sound_set.as_deref(),
            Some(SetRole::Sound),
        ),
        (Source::BgmSet, pick.bgm_set.as_deref(), Some(SetRole::Bgm)),
        (Source::Defaults, Some(pick.defaults.as_path()), None),
    ] {
        let Some(folder) = folder.and_then(|f| f.canonicalize().ok()) else {
            continue;
        };
        let identity = match kinds {
            Some(role) => SourceIdentity::of(&folder, role.kinds()),
            None => SourceIdentity::of(&folder, SystemKind::ALL.into_iter()),
        };
        if !excluded(&identity)
            && let Some(slot) = sources.get_mut(source.index())
        {
            *slot = Some(identity);
        }
    }
    let mut refused = Vec::new();
    let kinds = SystemKind::ALL.map(|kind| {
        let set = match SetRole::of(kind) {
            SetRole::Sound => Source::SoundSet,
            SetRole::Bgm => Source::BgmSet,
        };
        let before = refused.len();
        for source in [set, Source::Defaults] {
            let Some(Some(identity)) = sources.get(source.index()) else {
                continue;
            };
            if let Some(path) = locate(&identity.path, kind, &mut refused) {
                return Resolved::Found { path, source };
            }
        }
        if refused.len() > before {
            Resolved::Refused
        } else {
            Resolved::Missing
        }
    });
    Resolution {
        kinds,
        sources,
        refused,
    }
}

/// An index below `n` from a hasher with fresh random keys, as the source
/// draws a set at random; 0 when `n` is 0.
#[must_use]
pub fn random_index(n: usize) -> usize {
    let n = u64::try_from(n).unwrap_or(u64::MAX).max(1);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(n);
    usize::try_from(h.finish() % n).unwrap_or(0)
}

/// Chooses one of `sets` (a [`scan`]'s, of `role`) with `index`, leaving
/// out those whose identity `recovery` quarantines or `skipped` holds; the
/// identity is read only for a set whose folder one of them names.
pub fn choose(
    sets: &[PathBuf],
    role: SetRole,
    recovery: &mut Recovery,
    skipped: &[SourceIdentity],
    index: &mut dyn FnMut(usize) -> usize,
) -> Option<PathBuf> {
    let open: Vec<&PathBuf> = sets
        .iter()
        .filter(|set| {
            let named = recovery.names(set) || skipped.iter().any(|s| &s.path == *set);
            !named || {
                let identity = SourceIdentity::of(set, role.kinds());
                recovery.admits(&identity) && !skipped.contains(&identity)
            }
        })
        .collect();
    if open.is_empty() {
        return None;
    }
    let at = index(open.len()).min(open.len() - 1);
    open.get(at).map(|p| (*p).clone())
}

/// What a lock, read and write of the recovery state go through: the disk,
/// or a test's seam.
pub trait RecoveryFs: Send {
    /// Takes the exclusive lock at `path`, creating the file, and holds it
    /// until this value is dropped: `Ok(false)` when another holds it.
    ///
    /// # Errors
    ///
    /// The I/O error that kept it from being tried.
    fn lock(&mut self, path: &Path) -> io::Result<bool>;

    /// The bytes of `path`, `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// The I/O error of the read.
    fn read(&mut self, path: &Path) -> io::Result<Option<Vec<u8>>>;

    /// Writes `bytes` to `path` atomically and flushed.
    ///
    /// # Errors
    ///
    /// The I/O error of the write.
    fn write(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()>;
}

/// The recovery state on disk, with an OS lock held by its file handle.
#[derive(Debug, Default)]
pub struct DiskFs {
    lock: Option<File>,
}

impl RecoveryFs for DiskFs {
    fn lock(&mut self, path: &Path) -> io::Result<bool> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => {
                self.lock = Some(file);
                Ok(true)
            }
            Err(fs::TryLockError::WouldBlock) => Ok(false),
            Err(fs::TryLockError::Error(e)) => Err(e),
        }
    }

    fn read(&mut self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        write_atomic(path, bytes)
    }
}

/// Why the recovery state is unavailable: no set decodes for the run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unavailable {
    /// Another instance holds `sound-sets.lock`.
    #[error(
        "{} is held by another tapestry: sound sets are off for this run",
        .path.display()
    )]
    Locked {
        /// The lock file.
        path: PathBuf,
    },
    /// The lock could not be tried.
    #[error("{} cannot be locked ({}): sound sets are off for this run", .path.display(), .failure.message())]
    Lock {
        /// The lock file.
        path: PathBuf,
        /// What the system said.
        failure: IoFailure,
    },
    /// The state file could not be read.
    #[error(
        "{} cannot be read ({}): sound sets are off for this run; move it aside to use them again",
        .path.display(),
        .failure.message()
    )]
    Read {
        /// The state file.
        path: PathBuf,
        /// What the system said.
        failure: IoFailure,
    },
    /// The state file is not of its shape.
    #[error(
        "{} cannot be parsed ({detail}): sound sets are off for this run; move it aside to use \
         them again",
        .path.display()
    )]
    Parse {
        /// The state file.
        path: PathBuf,
        /// What the parser said.
        detail: String,
    },
    /// The state file is of another version: nothing is converted.
    #[error(
        "{} has version {}; this tapestry reads only version {SOUND_SETS_VERSION}: sound sets \
         are off for this run; move it aside to use them again",
        .path.display(),
        .found.map_or_else(|| "none".to_owned(), |v| v.to_string())
    )]
    Version {
        /// The state file.
        path: PathBuf,
        /// Its `version`, if it has one.
        found: Option<i64>,
    },
    /// A commit of the state file failed.
    #[error("{} cannot be written ({}): sound sets are off for this run", .path.display(), .failure.message())]
    Commit {
        /// The state file.
        path: PathBuf,
        /// What the system said.
        failure: IoFailure,
    },
}

/// `sound-sets.toml`, version 1.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decoding: Option<SourceIdentity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    quarantined: Vec<SourceIdentity>,
}

/// The one owner of `sound-sets.toml` (design.md T18): it holds the
/// exclusive lock for its life, and writes the file only from the state it
/// holds.
pub struct Recovery {
    fs: Box<dyn RecoveryFs>,
    path: PathBuf,
    state: StateFile,
}

impl fmt::Debug for Recovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recovery")
            .field("path", &self.path)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// A recovery state opened at startup.
#[derive(Debug)]
pub struct Opened {
    /// The state, locked.
    pub recovery: Recovery,
    /// The source a marker named, now quarantined, to be named once.
    pub quarantined: Option<SourceIdentity>,
}

impl Recovery {
    /// Takes the lock on `sound-sets.lock`, then reads `sound-sets.toml`:
    /// no file is created and committed; a marker found is moved to the
    /// quarantine and committed.
    ///
    /// # Errors
    ///
    /// [`Unavailable`]: the lock is held or cannot be tried, the file
    /// cannot be read or parsed or is of another version (it is left as it
    /// is), or a commit failed.
    pub fn open(root: &DataRoot, mut fs: Box<dyn RecoveryFs>) -> Result<Opened, Unavailable> {
        let lock = root.sound_sets_lock();
        match fs.lock(&lock) {
            Ok(true) => {}
            Ok(false) => return Err(Unavailable::Locked { path: lock }),
            Err(e) => {
                return Err(Unavailable::Lock {
                    path: lock,
                    failure: e.into(),
                });
            }
        }
        let path = root.sound_sets();
        let read = fs.read(&path).map_err(|e| Unavailable::Read {
            path: path.clone(),
            failure: e.into(),
        })?;
        let mut recovery = Self {
            fs,
            path,
            state: StateFile {
                version: SOUND_SETS_VERSION,
                ..StateFile::default()
            },
        };
        let Some(bytes) = read else {
            recovery.commit()?;
            return Ok(Opened {
                recovery,
                quarantined: None,
            });
        };
        recovery.state = recovery.parse(&bytes)?;
        let quarantined = recovery.state.decoding.take();
        if let Some(marker) = &quarantined {
            recovery.state.quarantined.retain(|q| q != marker);
            recovery.state.quarantined.push(marker.clone());
            recovery.commit()?;
        }
        Ok(Opened {
            recovery,
            quarantined,
        })
    }

    fn parse(&self, bytes: &[u8]) -> Result<StateFile, Unavailable> {
        let parse = |detail: String| Unavailable::Parse {
            path: self.path.clone(),
            detail,
        };
        let text = std::str::from_utf8(bytes).map_err(|e| parse(e.to_string()))?;
        let table: toml::Table = toml::from_str(text).map_err(|e| parse(e.to_string()))?;
        let found = match table.get("version") {
            None => None,
            Some(toml::Value::Integer(v)) => Some(*v),
            Some(_) => return Err(parse("version is not an integer".to_owned())),
        };
        if found != Some(SOUND_SETS_VERSION) {
            return Err(Unavailable::Version {
                path: self.path.clone(),
                found,
            });
        }
        toml::from_str(text).map_err(|e| parse(e.to_string()))
    }

    /// The quarantined identities.
    #[must_use]
    pub fn quarantined(&self) -> &[SourceIdentity] {
        &self.state.quarantined
    }

    /// The marker committed now, if any.
    #[must_use]
    pub fn decoding(&self) -> Option<&SourceIdentity> {
        self.state.decoding.as_ref()
    }

    /// Whether a quarantined identity names `folder`.
    #[must_use]
    pub fn names(&self, folder: &Path) -> bool {
        self.state.quarantined.iter().any(|q| q.path == folder)
    }

    /// Whether `identity` may decode: not while it equals a quarantined
    /// one. A quarantined identity of the same folder that differs is
    /// stale: it is dropped, and the next commit writes that.
    pub fn admits(&mut self, identity: &SourceIdentity) -> bool {
        if self.state.quarantined.contains(identity) {
            return false;
        }
        self.state.quarantined.retain(|q| q.path != identity.path);
        true
    }

    /// Commits `identity` as the marker of the source about to decode.
    ///
    /// # Errors
    ///
    /// [`Unavailable::Commit`]: nothing of the source may be read.
    pub fn mark(&mut self, identity: &SourceIdentity) -> Result<(), Unavailable> {
        self.state.decoding = Some(identity.clone());
        self.commit().inspect_err(|_| self.state.decoding = None)
    }

    /// Clears the marker and commits.
    ///
    /// # Errors
    ///
    /// [`Unavailable::Commit`].
    pub fn clear(&mut self) -> Result<(), Unavailable> {
        self.state.decoding = None;
        self.commit()
    }

    fn commit(&mut self) -> Result<(), Unavailable> {
        let failed = |failure: IoFailure| Unavailable::Commit {
            path: self.path.clone(),
            failure,
        };
        let text = toml::to_string(&self.state)
            .map_err(|e| failed(io::Error::new(io::ErrorKind::InvalidData, e).into()))?;
        self.fs
            .write(&self.path, text.as_bytes())
            .map_err(|e| failed(e.into()))
    }
}

/// A sound a later pick may keep: its PCM, shared, and its source.
#[derive(Clone, Debug)]
pub struct Kept {
    /// The sound.
    pub sound: Arc<Sound>,
    /// The identity of the source it was decoded from.
    pub source: SourceIdentity,
}

/// The sounds a generation plays, per kind, with their sources: what the
/// next pick keeps for a kind it finds no candidate for. Its empty value
/// holds nothing on the heap.
#[derive(Clone, Debug, Default)]
pub struct KeptSounds {
    kinds: [Option<Kept>; SYSTEM_KINDS],
}

impl KeptSounds {
    /// `kind`'s sound, if it has one.
    #[must_use]
    pub fn get(&self, kind: SystemKind) -> Option<&Kept> {
        self.kinds.get(kind.index())?.as_ref()
    }

    /// Whether no kind has a sound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kinds.iter().all(Option::is_none)
    }

    fn set(&mut self, kind: SystemKind, kept: Kept) {
        if let Some(slot) = self.kinds.get_mut(kind.index()) {
            *slot = Some(kept);
        }
    }
}

/// How a pick decodes one file: [`audio::decode_one`], or a test's seam.
pub trait DecodeFile: Sync {
    /// Decodes `path` as [`audio::decode_one`] does.
    ///
    /// # Errors
    ///
    /// Its [`FileRefusal`].
    fn decode(
        &self,
        path: &Path,
        rate: u32,
        budget: &PickBudget,
        limits: &BankLimits,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> Result<Decoded, FileRefusal>;
}

/// [`audio::decode_one`].
#[derive(Clone, Copy, Debug, Default)]
pub struct OneFile;

impl DecodeFile for OneFile {
    fn decode(
        &self,
        path: &Path,
        rate: u32,
        budget: &PickBudget,
        limits: &BankLimits,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> Result<Decoded, FileRefusal> {
        ::audio::decode_one(path, rate, budget, limits, cancel)
    }
}

/// How a pick's decoding ended.
#[derive(Debug)]
pub enum PickDecode {
    /// The generation, a refused file's kind silent in it.
    Ready(SystemGeneration),
    /// A cancel stopped it.
    Cancelled,
    /// It panicked outside a source's decode.
    Panicked,
}

/// What one pick came to: the decode and the recovery state's outcomes,
/// kept apart.
#[derive(Debug)]
pub struct PickReport {
    /// The decode's outcome.
    pub decode: PickDecode,
    /// The recovery state's: `Err` when a marker could not be committed or
    /// cleared, so no set may decode for the rest of the run.
    pub recovery: Result<(), Unavailable>,
    /// The ready generation's sounds per kind with their sources (empty
    /// unless ready).
    pub kept: KeptSounds,
    /// Files refused, each to be named once.
    pub refused: Vec<Refused>,
    /// Sources whose decode panicked: their kinds are silent, and they are
    /// skipped for the run.
    pub panicked: Vec<SourceIdentity>,
}

/// Per kind, its sound and source in the pick being made.
type Slots = [Option<Kept>; SYSTEM_KINDS];

/// Decodes `pick` into a generation, under `budget`: every kind is resolved
/// first; a kind with no candidate keeps `kept`'s sound, charged before any
/// file is decoded, unless that sound's source is quarantined or skipped;
/// then each source with a file to decode is decoded under its marker,
/// committed before its first byte is read and cleared after, whatever the
/// outcome. Identical bytes at different paths are decoded and charged
/// once. A cancel stops at the next check; a marker that cannot be
/// committed stops the pick before that source is read; one that cannot be
/// cleared stops it after.
pub fn decode_pick(
    pick: &SetPick,
    kept: &KeptSounds,
    budget: &PickBudget,
    recovery: &mut Recovery,
    cancel: &(dyn Fn() -> bool + Sync),
    decoder: &dyn DecodeFile,
) -> PickReport {
    let body = panic::catch_unwind(AssertUnwindSafe(|| {
        pick_body(pick, kept, budget, recovery, cancel, decoder)
    }));
    body.unwrap_or_else(|_| {
        let recovery = match recovery.decoding() {
            Some(_) => recovery.clear(),
            None => Ok(()),
        };
        PickReport {
            decode: PickDecode::Panicked,
            recovery,
            kept: KeptSounds::default(),
            refused: Vec::new(),
            panicked: Vec::new(),
        }
    })
}

fn pick_body(
    pick: &SetPick,
    kept: &KeptSounds,
    budget: &PickBudget,
    recovery: &mut Recovery,
    cancel: &(dyn Fn() -> bool + Sync),
    decoder: &dyn DecodeFile,
) -> PickReport {
    let resolution = resolve(pick, &mut |id| {
        !recovery.admits(id) || pick.skipped.contains(id)
    });
    let mut refused = resolution.refused;
    let mut slots: Slots = Default::default();
    // The kept sounds first, each distinct one charged once.
    let mut charged: Vec<Arc<Sound>> = Vec::new();
    for kind in SystemKind::ALL {
        if !matches!(resolution.kinds.get(kind.index()), Some(Resolved::Missing)) {
            continue;
        }
        let Some(k) = kept.get(kind) else { continue };
        if recovery.quarantined().contains(&k.source) || pick.skipped.contains(&k.source) {
            continue;
        }
        if !charged.iter().any(|c| Arc::ptr_eq(c, &k.sound)) {
            if budget.charge(k.sound.bytes()).is_err() {
                continue;
            }
            charged.push(Arc::clone(&k.sound));
        }
        if let Some(slot) = slots.get_mut(kind.index()) {
            *slot = Some(k.clone());
        }
    }
    let mut report = PickReport {
        decode: PickDecode::Cancelled,
        recovery: Ok(()),
        kept: KeptSounds::default(),
        refused: Vec::new(),
        panicked: Vec::new(),
    };
    let mut digests: HashMap<[u8; 32], Arc<Sound>> = HashMap::new();
    let mut cancelled = false;
    for source in Source::ALL {
        let files: Vec<(SystemKind, &Path)> = SystemKind::ALL
            .into_iter()
            .filter_map(|kind| match resolution.kinds.get(kind.index()) {
                Some(Resolved::Found { path, source: s }) if *s == source => {
                    Some((kind, path.as_path()))
                }
                _ => None,
            })
            .collect();
        let Some(Some(identity)) = resolution.sources.get(source.index()) else {
            continue;
        };
        if files.is_empty() {
            continue;
        }
        if cancel() {
            cancelled = true;
            break;
        }
        if let Err(e) = recovery.mark(identity) {
            report.recovery = Err(e);
            break;
        }
        let decoded = panic::catch_unwind(AssertUnwindSafe(|| {
            decode_source(&files, pick, budget, cancel, decoder, &mut digests)
        }));
        let cleared = recovery.clear();
        match decoded {
            Ok(SourceDecode {
                sounds,
                refused: r,
                cancelled: c,
            }) => {
                refused.extend(r);
                for (kind, sound) in sounds {
                    if let Some(slot) = slots.get_mut(kind.index()) {
                        *slot = Some(Kept {
                            sound,
                            source: identity.clone(),
                        });
                    }
                }
                cancelled = c;
            }
            Err(_) => report.panicked.push(identity.clone()),
        }
        if let Err(e) = cleared {
            report.recovery = Err(e);
            break;
        }
        if cancelled {
            break;
        }
    }
    report.refused = refused;
    if cancelled {
        return report;
    }
    let mut generation = SystemGeneration::new(pick.id, pick.rate);
    for (kind, slot) in SystemKind::ALL.into_iter().zip(slots) {
        if let Some(k) = slot {
            generation = generation.with(kind, Arc::clone(&k.sound));
            report.kept.set(kind, k);
        }
    }
    report.decode = PickDecode::Ready(generation);
    report
}

/// What one source's decode made.
struct SourceDecode {
    sounds: Vec<(SystemKind, Arc<Sound>)>,
    refused: Vec<Refused>,
    cancelled: bool,
}

fn decode_source(
    files: &[(SystemKind, &Path)],
    pick: &SetPick,
    budget: &PickBudget,
    cancel: &(dyn Fn() -> bool + Sync),
    decoder: &dyn DecodeFile,
    digests: &mut HashMap<[u8; 32], Arc<Sound>>,
) -> SourceDecode {
    let mut out = SourceDecode {
        sounds: Vec::new(),
        refused: Vec::new(),
        cancelled: false,
    };
    for &(kind, path) in files {
        match decoder.decode(path, pick.rate, budget, &pick.limits, cancel) {
            Ok(Decoded::Sound { sound, digest }) => {
                let sound = Arc::new(sound);
                digests.insert(digest, Arc::clone(&sound));
                out.sounds.push((kind, sound));
            }
            Ok(Decoded::Same { digest }) => {
                if let Some(sound) = digests.get(&digest) {
                    out.sounds.push((kind, Arc::clone(sound)));
                }
            }
            Err(FileRefusal::Cancelled) => {
                out.cancelled = true;
                break;
            }
            Err(e) => out.refused.push(Refused {
                path: path.to_owned(),
                why: Why::File(e),
            }),
        }
    }
    out
}
