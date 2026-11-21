//! The player a panel row drives (docs/test-device.md "Host Commands",
//! `run`'s panel rows): its scratch settings written through the player's
//! own settings types, its start in the run's job with its output in
//! files of the run's folder, the foreground the runner lets it take, and
//! its main window on the run's monitor.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

use super::click::{Win, contains};
use super::desktop;
use super::job::{self, Child, Job, Stdio};
use super::owner;

/// The paths a panel row takes as arguments: what the repository cannot
/// hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Player {
    /// The release `tapestry.exe` the row starts.
    pub exe: PathBuf,
    /// The scratch `LOCALAPPDATA`: its `tapestry` folder is the data root.
    pub data: PathBuf,
    /// The library's root.
    pub library: PathBuf,
    /// The select skin's root folder, and its entry file within it.
    pub skin: PathBuf,
    pub skin_entry: String,
}

/// The window's client size the panel rows run at: the reference's.
pub const CLIENT: [i32; 2] = test_device::panel::CLIENT;

/// The device's controller, as `device.toml` names it: the test PID's
/// pad interface.
const VENDOR: i64 = 0x1209;
const PRODUCT: i64 = 0x0001;
const PAD_INTERFACE: i64 = 1;
/// The pad's X axis as a binding's control: usage page 1, usage 0x30.
const X_AXIS: i64 = 0x1_0030;

/// `device.toml` for a panel row: the run's monitor, the volumes at 1.0,
/// the device's pad bound (buttons 1-7 on lanes 1-7, X on the scratch,
/// buttons 9 and 10 on START and SELECT), the library.
pub fn device_table(monitor: &str, library: &Path) -> toml::Table {
    let mut lanes = vec![toml::Value::Table(toml::Table::from_iter([
        ("control".to_owned(), toml::Value::Integer(X_AXIS)),
        ("lane".to_owned(), toml::Value::Integer(0)),
    ]))];
    lanes.extend((1..=7).map(|b| {
        toml::Value::Table(toml::Table::from_iter([
            ("control".to_owned(), toml::Value::Integer(b)),
            ("lane".to_owned(), toml::Value::Integer(b)),
        ]))
    }));
    let controller = toml::Table::from_iter([
        (
            "start".to_owned(),
            toml::Value::Integer(super::run::START.into()),
        ),
        (
            "select".to_owned(),
            toml::Value::Integer(super::run::SELECT.into()),
        ),
        ("lanes".to_owned(), toml::Value::Array(lanes)),
    ]);
    let device = toml::Table::from_iter([
        ("vendor_id".to_owned(), toml::Value::Integer(VENDOR)),
        ("product_id".to_owned(), toml::Value::Integer(PRODUCT)),
        ("interface".to_owned(), toml::Value::Integer(PAD_INTERFACE)),
    ]);
    let library = toml::Table::from_iter([(
        "roots".to_owned(),
        toml::Value::Array(vec![toml::Value::String(slashed(library))]),
    )]);
    toml::Table::from_iter([
        (
            "version".to_owned(),
            toml::Value::Integer(i64::from(game::DEVICE_SETTINGS_VERSION)),
        ),
        (
            "monitor".to_owned(),
            toml::Value::String(monitor.to_owned()),
        ),
        ("system_volume".to_owned(), toml::Value::Float(1.0)),
        ("key_volume".to_owned(), toml::Value::Float(1.0)),
        ("background_volume".to_owned(), toml::Value::Float(1.0)),
        ("controller".to_owned(), toml::Value::Table(controller)),
        ("controller_device".to_owned(), toml::Value::Table(device)),
        ("library".to_owned(), toml::Value::Table(library)),
    ])
}

/// `player.toml` for a panel row: the select skin used for testing.
pub fn player_table(skin: &Path, entry: &str) -> toml::Table {
    let selection = toml::Table::from_iter([
        ("root".to_owned(), toml::Value::String(slashed(skin))),
        ("entry".to_owned(), toml::Value::String(entry.to_owned())),
    ]);
    toml::Table::from_iter([(
        "skins".to_owned(),
        toml::Value::Table(toml::Table::from_iter([(
            "selection".to_owned(),
            toml::Value::Table(selection),
        )])),
    )])
}

fn slashed(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Writes `table` as the settings file `T` at `path`: read back through the
/// file's own refusals and types first (from `scratch`), so the file is
/// what the player itself writes, and a table the player would refuse is
/// refused before anything is written at `path`.
///
/// # Errors
///
/// The player's refusal, or a file that cannot be written.
pub fn write_settings<T: game::SettingsFile + serde::Serialize>(
    table: &toml::Table,
    scratch: &Path,
    path: &Path,
) -> anyhow::Result<()> {
    std::fs::write(scratch, toml::to_string(table)?)
        .with_context(|| scratch.display().to_string())?;
    let settings: T = game::load_settings(scratch).with_context(|| {
        format!(
            "the player refuses the settings written to {}",
            scratch.display()
        )
    })?;
    game::save_settings(path, &settings).with_context(|| path.display().to_string())
}

/// The run folder's copies of the settings before the player starts and
/// after it ends.
pub const DEVICE_BEFORE: &str = "device-before.toml";
pub const PLAYER_BEFORE: &str = "player-before.toml";
pub const DEVICE_AFTER: &str = "device-after.toml";
pub const PLAYER_AFTER: &str = "player-after.toml";
/// The player's standard output and error, in the run's folder.
pub const STDOUT: &str = "player-stdout.txt";
pub const STDERR: &str = "player-stderr.txt";

impl Player {
    fn root(&self) -> game::DataRoot {
        game::DataRoot::new(self.data.join("tapestry"))
    }

    /// Writes the scratch `device.toml` and `player.toml` for the run's
    /// `monitor` into the data root, and copies each into the run's folder
    /// `dir` (`device-before.toml`, `player-before.toml`). The data root
    /// must not hold either file yet: the run starts from its own settings.
    ///
    /// # Errors
    ///
    /// A file is already there, the player refuses a table, or a file
    /// cannot be written.
    pub fn prepare(&self, monitor: &str, dir: &Path) -> anyhow::Result<()> {
        let root = self.root();
        let files = [
            (root.device_settings(), DEVICE_BEFORE),
            (root.player_settings(), PLAYER_BEFORE),
        ];
        for (path, _) in &files {
            if path.exists() {
                bail!(
                    "{} exists: a run starts from a fresh data root",
                    path.display()
                );
            }
        }
        let scratch = dir.join("settings-scratch.toml");
        write_settings::<game::DeviceSettings>(
            &device_table(monitor, &self.library),
            &scratch,
            &files[0].0,
        )?;
        write_settings::<game::PlayerSettings>(
            &player_table(&self.skin, &self.skin_entry),
            &scratch,
            &files[1].0,
        )?;
        let _ = std::fs::remove_file(&scratch);
        for (path, name) in &files {
            copy_new(path, &dir.join(name))?;
        }
        Ok(())
    }

    /// Copies the data root's `device.toml` and `player.toml`, as the
    /// player left them, into `dir` (`device-after.toml`,
    /// `player-after.toml`).
    ///
    /// # Errors
    ///
    /// A file is missing or cannot be copied.
    pub fn collect(&self, dir: &Path) -> anyhow::Result<()> {
        let root = self.root();
        copy_new(&root.device_settings(), &dir.join(DEVICE_AFTER))?;
        copy_new(&root.player_settings(), &dir.join(PLAYER_AFTER))
    }

    /// Starts the player in the run's `job` with the scratch data root and
    /// a 1920×1080 window, its output in files of the run's folder `dir`
    /// (never pipes, so it meets no broken pipe when the runner ends).
    ///
    /// # Errors
    ///
    /// The output files exist, or the player cannot start or is not in the
    /// job.
    pub fn spawn(&self, job: &Job, dir: &Path) -> anyhow::Result<Child> {
        let args = [
            "--dev-window".to_owned(),
            format!("{}x{}", CLIENT[0], CLIENT[1]),
        ];
        self.spawn_with(job, dir, &args)
    }

    /// [`Self::spawn`] with `args`.
    fn spawn_with(&self, job: &Job, dir: &Path, args: &[String]) -> anyhow::Result<Child> {
        let stdio = Stdio {
            input: Stdio::null()?,
            output: Stdio::file(owner::create(&dir.join(STDOUT))?),
            error: Stdio::file(owner::create(&dir.join(STDERR))?),
        };
        let data = self.data.to_string_lossy();
        job::spawn(job, &self.exe, args, &[("LOCALAPPDATA", &data)], &stdio)
    }
}

/// The player a run starts: a private copy of its executable and the files
/// it starts beside it, each hashed through a handle that denies writes
/// and deletion from the copy until the run ends, so the file launched is
/// the file hashed.
#[derive(Debug)]
pub struct Verified {
    /// The copy of the executable.
    pub exe: PathBuf,
    /// Its sha256, read through its held handle.
    pub sha256: String,
    /// Every copied file's name and sha256, the executable's first.
    pub files: Vec<(String, String)>,
    /// The handles that keep the copies unchanged while they are held.
    _locks: Vec<std::fs::File>,
}

impl Verified {
    /// `p` with the copy as its executable: the player the run starts.
    #[must_use]
    pub fn player(&self, p: &Player) -> Player {
        Player {
            exe: self.exe.clone(),
            ..p.clone()
        }
    }

    /// How many copies are held.
    #[cfg(test)]
    pub const fn held(&self) -> usize {
        self._locks.len()
    }
}

/// The media worker, copied with the player's executable when it lies
/// beside it, as its DLLs are.
const WORKER: &str = "tapestry-media.exe";

/// Copies the player's executable `exe`, and the media worker and DLLs
/// beside it, into the new folder `into`, each a new file; then opens each
/// copy for reading with only reads shared (no writer, no deletion or
/// rename while it is held) and hashes it through that handle, which must
/// read the bytes copied.
///
/// # Errors
///
/// `into` exists, a file cannot be read or written, or a copy reads other
/// bytes than were written.
pub fn private_copy(exe: &Path, into: &Path) -> anyhow::Result<Verified> {
    use std::io::Read as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
    std::fs::create_dir(into)
        .with_context(|| format!("the player's private folder {}", into.display()))?;
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
    let main = name(exe).with_context(|| format!("{} names no file", exe.display()))?;
    let beside = exe.parent().context("the player's folder")?;
    let mut names = vec![main];
    let mut others: Vec<String> = std::fs::read_dir(beside)
        .with_context(|| beside.display().to_string())?
        .filter_map(|e| name(&e.ok()?.path()))
        .filter(|n| n == WORKER || n.to_ascii_lowercase().ends_with(".dll"))
        .collect();
    others.sort();
    names.extend(others);
    let mut files = Vec::new();
    let mut locks = Vec::new();
    for n in &names {
        let bytes =
            std::fs::read(beside.join(n)).with_context(|| beside.join(n).display().to_string())?;
        let copy = into.join(n);
        owner::write_new(&copy, &bytes)?;
        let mut held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&copy)
            .with_context(|| format!("holding {}", copy.display()))?;
        let mut read = Vec::new();
        held.read_to_end(&mut read)
            .with_context(|| copy.display().to_string())?;
        if read != bytes {
            bail!("{} reads other bytes than were copied", copy.display());
        }
        files.push((
            n.clone(),
            test_device::contract::hex(&<sha2::Sha256 as sha2::Digest>::digest(&read)),
        ));
        locks.push(held);
    }
    Ok(Verified {
        exe: into.join(&names[0]),
        sha256: files[0].1.clone(),
        files,
        _locks: locks,
    })
}

/// Copies `from` to the new file `to`.
fn copy_new(from: &Path, to: &Path) -> anyhow::Result<()> {
    let bytes = std::fs::read(from).with_context(|| from.display().to_string())?;
    owner::write_new(to, &bytes)
}

/// How long the runner keeps letting the player take the foreground:
/// `AllowSetForegroundWindow` fails (error 87) for a process that has not
/// yet used the window system, as right after it starts.
pub const GRANT_WAIT: Duration = Duration::from_secs(5);
/// How long the player may take to show its window.
pub const WINDOW_WAIT: Duration = Duration::from_secs(30);
/// How often these waits look (and beat).
const LOOK: Duration = Duration::from_millis(5);

/// Lets process `pid` take the foreground, from the runner's process,
/// whose sink is in front: tried at each look for up to [`GRANT_WAIT`].
///
/// # Errors
///
/// It never succeeded: the last error.
pub fn grant(pid: u32, beat: &dyn Fn()) -> anyhow::Result<()> {
    let end = Instant::now() + GRANT_WAIT;
    loop {
        beat();
        // SAFETY: no pointers.
        let r = unsafe { AllowSetForegroundWindow(pid) };
        match r {
            Ok(()) => return Ok(()),
            Err(e) if Instant::now() >= end => {
                return Err(e).with_context(|| {
                    format!("AllowSetForegroundWindow({pid}) within {GRANT_WAIT:?}")
                });
            }
            Err(_) => std::thread::sleep(LOOK),
        }
    }
}

/// Whether a window whose client area is `client` is the run's player
/// window: [`CLIENT`]'s size, wholly inside the run's `monitor`.
pub const fn placed(client: [i32; 4], monitor: [i32; 4]) -> bool {
    client[2] == CLIENT[0] && client[3] == CLIENT[1] && contains(monitor, client)
}

/// The player's main window, once it shows at [`CLIENT`]'s size inside the
/// run's `monitor`; `alive` says whether the player still runs.
///
/// # Errors
///
/// The player ended first, or no such window showed within
/// [`WINDOW_WAIT`].
pub fn window(
    pid: u32,
    monitor: [i32; 4],
    alive: &dyn Fn() -> bool,
    beat: &dyn Fn(),
) -> anyhow::Result<(Win, [i32; 4])> {
    let end = Instant::now() + WINDOW_WAIT;
    loop {
        beat();
        if let Some(w) = desktop::main_window_of(pid)
            && let Some(client) = desktop::client_rect(w)
            && placed(client, monitor)
        {
            return Ok((w, client));
        }
        if !alive() {
            bail!("the player ended before it showed its window");
        }
        if Instant::now() >= end {
            bail!(
                "the player showed no {}x{} window inside the run's monitor {monitor:?} within {WINDOW_WAIT:?}",
                CLIENT[0],
                CLIENT[1]
            );
        }
        std::thread::sleep(LOOK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The player is launched from a private copy hashed through the
    /// handle held on it: the executable, its media worker and its DLLs are
    /// copied (nothing else beside them), each digest is of its copy, and
    /// while the copies are held none can be written, replaced or deleted;
    /// a source replaced after the copy leaves the copy as hashed. The
    /// player the run starts is the copy. A folder that exists is refused.
    #[test]
    fn the_player_runs_from_a_held_private_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        for (n, b) in [
            ("tapestry.exe", &b"exe"[..]),
            (WORKER, b"worker"),
            ("avcodec-63.dll", b"dll"),
            ("notes.txt", b"other"),
        ] {
            std::fs::write(bin.join(n), b).unwrap();
        }
        let into = tmp.path().join("copy");
        let v = private_copy(&bin.join("tapestry.exe"), &into).unwrap();
        let sha = |b: &[u8]| test_device::contract::hex(&<sha2::Sha256 as sha2::Digest>::digest(b));
        assert_eq!(v.exe, into.join("tapestry.exe"));
        assert_eq!(v.sha256, sha(b"exe"));
        let names: Vec<&str> = v.files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["tapestry.exe", "avcodec-63.dll", WORKER]);
        assert_eq!(v.held(), 3);
        assert!(!into.join("notes.txt").exists());
        // Held: no write, replacement or deletion of the copy.
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&v.exe)
                .is_err()
        );
        assert!(std::fs::remove_file(&v.exe).is_err());
        assert!(std::fs::rename(into.join(WORKER), &v.exe).is_err());
        // The source replaced after the copy: the copy is as hashed.
        std::fs::write(bin.join("tapestry.exe"), b"replaced").unwrap();
        assert_eq!(std::fs::read(&v.exe).unwrap(), b"exe");
        // The run starts the copy.
        assert_eq!(v.player(&player(tmp.path())).exe, v.exe);
        drop(v);
        assert!(private_copy(&bin.join("tapestry.exe"), &into).is_err());
    }

    fn player(dir: &Path) -> Player {
        Player {
            exe: PathBuf::from("tapestry.exe"),
            data: dir.join("data"),
            library: PathBuf::from("C:/lib"),
            skin: PathBuf::from("C:/skins/select"),
            skin_entry: "select/select.luaskin".into(),
        }
    }

    /// The settings a run writes are the player's own: read back through
    /// its types, they hold the run's values, and the file is the type's
    /// own form; a table the player refuses is refused before anything is
    /// written.
    #[test]
    fn a_settings_table_the_player_refuses_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let scratch = tmp.path().join("in.toml");
        let path = tmp.path().join("tapestry").join("device.toml");
        let table = device_table(r"\\.\DISPLAY1", Path::new("C:/lib"));
        write_settings::<game::DeviceSettings>(&table, &scratch, &path).unwrap();
        let back: game::DeviceSettings = game::load_settings(&path).unwrap();
        assert_eq!(back.monitor.as_deref(), Some(r"\\.\DISPLAY1"));
        assert_eq!(back.library.roots, ["C:/lib"]);
        assert_eq!(
            (back.system_volume, back.key_volume, back.background_volume),
            (1.0, 1.0, 1.0)
        );
        assert_eq!(
            (back.controller.start, back.controller.select),
            (Some(9), Some(10))
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            toml::to_string(&back).unwrap()
        );
        let p = tmp.path().join("player.toml");
        write_settings::<game::PlayerSettings>(
            &player_table(Path::new("C:/skins/select"), "select/select.luaskin"),
            &scratch,
            &p,
        )
        .unwrap();
        let back: game::PlayerSettings = game::load_settings(&p).unwrap();
        let s = back.skins.selection.unwrap();
        assert_eq!(s.root, PathBuf::from("C:/skins/select"));
        assert_eq!(s.entry, "select/select.luaskin");
        // What the player refuses is refused, and nothing is written.
        let other = tmp.path().join("other.toml");
        let mut old = table.clone();
        old.insert("version".into(), toml::Value::Integer(2));
        let mut extra = table;
        extra.insert("volume".into(), toml::Value::Float(1.0));
        for bad in [old, extra] {
            assert!(write_settings::<game::DeviceSettings>(&bad, &scratch, &other).is_err());
            assert!(!other.exists(), "{bad}");
        }
        let mut skin = player_table(Path::new("C:/s"), "e");
        skin["skins"]["selection"]
            .as_table_mut()
            .unwrap()
            .insert("size".into(), toml::Value::Integer(1));
        assert!(write_settings::<game::PlayerSettings>(&skin, &scratch, &other).is_err());
        assert!(!other.exists());
    }

    /// A run starts from its own settings: a data root that already holds
    /// a settings file is refused; a fresh one gets both, copied beside the
    /// run.
    #[test]
    fn a_run_s_settings_start_fresh_and_are_kept_beside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run");
        std::fs::create_dir(&dir).unwrap();
        let p = player(tmp.path());
        p.prepare(r"\\.\DISPLAY1", &dir).unwrap();
        for (made, kept) in [
            ("device.toml", DEVICE_BEFORE),
            ("player.toml", PLAYER_BEFORE),
        ] {
            assert_eq!(
                std::fs::read(p.data.join("tapestry").join(made)).unwrap(),
                std::fs::read(dir.join(kept)).unwrap()
            );
        }
        let again = tmp.path().join("again");
        std::fs::create_dir(&again).unwrap();
        let e = p.prepare(r"\\.\DISPLAY1", &again).unwrap_err();
        assert!(format!("{e:#}").contains("fresh data root"), "{e:#}");
    }

    /// A player outside the run's job is ended and refused: the job here is
    /// one this process did not join, so its children are not in it.
    #[test]
    fn the_player_is_in_the_run_job() {
        let tmp = tempfile::tempdir().unwrap();
        let job = Job::create().unwrap();
        let mut p = player(tmp.path());
        p.exe = PathBuf::from(std::env::var("ComSpec").unwrap());
        let Err(e) = p.spawn_with(&job, tmp.path(), &["/c".into(), "exit 0".into()]) else {
            panic!("a player outside the job was taken");
        };
        assert!(
            format!("{e:#}").contains("is not in the run's job"),
            "{e:#}"
        );
    }

    /// The environment variable that makes [`helper`] act.
    const HELPER: &str = "TAPESTRY_LAUNCH_TEST_HELPER";

    /// Not a test of its own: with the variable set to a folder, it joins a
    /// job, starts a child as the run starts the player there (the child
    /// writes a line to each of its outputs and reads the scratch data root
    /// from its environment), waits for it and prints its exit code.
    #[test]
    fn helper() {
        let Ok(dir) = std::env::var(HELPER) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let job = Job::join().unwrap();
        let mut p = player(&dir);
        p.exe = PathBuf::from(std::env::var("ComSpec").unwrap());
        let child = p
            .spawn_with(
                &job,
                &dir,
                &["/c".into(), "echo out %LOCALAPPDATA%& echo err 1>&2".into()],
            )
            .unwrap();
        let code = child.wait(Duration::from_secs(10)).unwrap();
        println!("HELPER {code:?}");
    }

    /// The player's output goes to files of the run's folder, not pipes:
    /// the child's lines are in them, and its environment names the
    /// scratch data root.
    #[test]
    fn the_player_s_output_goes_to_files() {
        let tmp = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "dev::test_device::launch::tests::helper",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER, tmp.path())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("HELPER Some(0)"), "{text}");
        let stdout = std::fs::read_to_string(tmp.path().join(STDOUT)).unwrap();
        let data = tmp.path().join("data");
        assert_eq!(stdout.trim_end(), format!("out {}", data.display()));
        let stderr = std::fs::read_to_string(tmp.path().join(STDERR)).unwrap();
        assert_eq!(stderr.trim_end(), "err");
    }

    #[test]
    fn the_player_window_is_the_reference_size_inside_the_monitor() {
        let monitor = [0, 0, 2560, 1440];
        assert!(placed([0, 0, 1920, 1080], monitor));
        assert!(!placed([0, 0, 1920, 1079], monitor));
        assert!(!placed([700, 400, 1920, 1080], monitor));
        assert!(!placed([0, 0, 2560, 1440], monitor));
    }
}
