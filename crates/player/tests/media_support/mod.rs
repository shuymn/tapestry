//! What the media worker's tests share (design.md "Threat Model" T02): the
//! game's host of the worker, compiled from its source as the game compiles
//! it, the worker put with its FFmpeg DLLs in a directory of its own, which
//! stands for an installation, launched from the copies the launch stages
//! in a data folder of the test's own, in an AppContainer profile of the
//! test's own, and the results of the worker's `--dev-probe`.

#![allow(
    dead_code,
    unused_imports,
    reason = "each test file uses part of what the tests share"
)]

#[path = "../../src/media_host/mod.rs"]
pub mod media_host;

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use media_host::launch::{
    LaunchError, Made, REFUSED, Restrictions, Setup, Step, change_shared, granted_all, revoked,
};
pub use media_host::{Kind, Worker};
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_GROUPS,
    TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenCapabilities,
    TokenIntegrityLevel, TokenIsAppContainer,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessMitigationPolicy, OpenProcessToken,
    PROCESS_MITIGATION_POLICY, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};

pub const WORKER: &str = env!("CARGO_BIN_EXE_tapestry-media");

/// The workspace root.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The FFmpeg DLLs ffmpeg.toml pins, without `.dll`.
pub fn dlls() -> Vec<String> {
    let text = std::fs::read_to_string(root().join("ffmpeg.toml")).unwrap();
    let pin: toml::Table = toml::from_str(&text).unwrap();
    pin["build"]["dlls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_owned())
        .collect()
}

/// How long a test waits for the worker's hello. While other work fills
/// every core (parallel tests, another build) a launch waits for the CPU
/// and for the other tests' launches, which stage their copies one at a
/// time. Only the handshake's own test keeps the recipe's limit.
pub const PATIENT: Duration = Duration::from_secs(30);

/// The recipe, waiting `PATIENT` for the worker's hello.
pub fn recipe() -> Setup {
    Setup::recipe().waiting(PATIENT)
}

/// The worker and its DLLs, copied into a directory of their own (the
/// installation), with a data folder and an AppContainer profile of their
/// own: a test's launches touch nothing of the user's or another test's.
pub struct Stage {
    dir: tempfile::TempDir,
    data: tempfile::TempDir,
    profile: String,
}

/// Copies the worker and its DLLs into `dir`, an installation of its own.
pub fn install(dir: &Path) {
    let from = Path::new(WORKER).parent().unwrap();
    std::fs::copy(WORKER, dir.join("tapestry-media.exe")).unwrap();
    for dll in dlls() {
        let name = format!("{dll}.dll");
        std::fs::copy(from.join(&name), dir.join(&name)).unwrap();
    }
}

/// Deletes the AppContainer profile `name`, which Windows would otherwise
/// keep; deleting one no launch made fails, and is ignored.
pub fn delete_profile(name: &str) {
    let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a nul-terminated name.
    let _ = unsafe {
        windows::Win32::Security::Isolation::DeleteAppContainerProfile(windows::core::PCWSTR(
            name.as_ptr(),
        ))
    };
}

impl Stage {
    pub fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("media-stage-")
            .tempdir()
            .unwrap();
        install(dir.path());
        let data = tempfile::Builder::new()
            .prefix("media-data-")
            .tempdir()
            .unwrap();
        // Lowercase, as Windows names the profile's folder.
        let name = dir.path().file_name().unwrap().to_string_lossy();
        let profile = format!("tapestry.media.test.{}", name.to_lowercase());
        Self { dir, data, profile }
    }

    /// The installation.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// The data folder the launches stage their copies in.
    pub fn data(&self) -> &Path {
        self.data.path()
    }

    /// The AppContainer profile the stage's workers run in.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The directory of the copies the launches stage: the only one in the
    /// data folder's `media`.
    pub fn copies(&self) -> PathBuf {
        let mut found: Vec<PathBuf> = std::fs::read_dir(self.data().join("media"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        found.pop().unwrap()
    }

    /// Launches the worker under `setup`, in the stage's profile.
    pub fn launch(&self, setup: &Setup) -> Result<Launched, LaunchError> {
        let setup = setup.clone().in_profile(&self.profile);
        Worker::launch(self.dir(), self.data(), &setup).map(Launched::new)
    }
}

impl Drop for Stage {
    /// Deletes the stage's AppContainer profile, which Windows would
    /// otherwise keep.
    fn drop(&mut self) {
        delete_profile(&self.profile);
    }
}

/// A launched worker: dropping it ends the worker, as the game does, and
/// waits until it has ended.
pub struct Launched {
    worker: Option<Worker>,
    /// The worker's process, to wait on and, past the wait, end.
    process: HANDLE,
}

impl Launched {
    fn new(worker: Worker) -> Self {
        let mut process = HANDLE::default();
        // SAFETY: the worker's process handle, duplicated within this
        // process; closed on drop.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                worker.process(),
                GetCurrentProcess(),
                &raw mut process,
                (PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION).0,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
        }
        .unwrap();
        Self {
            worker: Some(worker),
            process,
        }
    }

    /// The worker itself, to drop apart from this value, which still
    /// waits for it to end.
    pub fn take(&mut self) -> Worker {
        self.worker.take().unwrap()
    }

    /// Whether the worker has ended.
    pub fn ended(&self) -> bool {
        // SAFETY: the worker's process, a duplicate this value holds.
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_OBJECT_0 }
    }

    /// Waits up to `limit` for the worker to end: its exit code, once it
    /// has.
    pub fn wait_exit(&self, limit: Duration) -> Option<u32> {
        let ms = u32::try_from(limit.as_millis()).unwrap();
        // SAFETY: as above.
        if unsafe { WaitForSingleObject(self.process, ms) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0;
        // SAFETY: as above.
        unsafe { GetExitCodeProcess(self.process, &raw mut code) }.unwrap();
        Some(code)
    }

    /// Waits until the worker has logged `probe done` `n` times or has
    /// ended: its log.
    pub fn probed(&self, n: usize) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let log = self.log();
            if log.matches("probe done").count() >= n || self.ended() {
                // What it logged before it ended has been read by now.
                std::thread::sleep(Duration::from_millis(50));
                return self.log();
            }
            assert!(
                Instant::now() < deadline,
                "the probes did not end; the worker's log:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Deref for Launched {
    type Target = Worker;

    fn deref(&self) -> &Worker {
        self.worker.as_ref().unwrap()
    }
}

impl DerefMut for Launched {
    fn deref_mut(&mut self) -> &mut Worker {
        self.worker.as_mut().unwrap()
    }
}

impl Drop for Launched {
    fn drop(&mut self) {
        drop(self.worker.take());
        if self.wait_exit(Duration::from_secs(10)).is_none() {
            // SAFETY: the worker's process, a duplicate this value holds.
            let _ = unsafe { TerminateProcess(self.process, 1) };
            let _ = self.wait_exit(Duration::from_secs(10));
        }
        // SAFETY: as above, unused after this.
        let _ = unsafe { CloseHandle(self.process) };
    }
}

/// `process`'s flags of the mitigation `policy`.
pub fn policy(process: HANDLE, policy: PROCESS_MITIGATION_POLICY) -> u32 {
    let mut flags = 0u32;
    // SAFETY: each of these policies is one 32-bit word.
    unsafe {
        GetProcessMitigationPolicy(
            process,
            policy,
            (&raw mut flags).cast(),
            size_of_val(&flags),
        )
    }
    .unwrap();
    flags
}

/// `class` of `process`'s token, in an aligned buffer.
fn token(process: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Vec<u64> {
    let mut token = HANDLE::default();
    // SAFETY: a process handle with query access; the token is owned below.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }.unwrap();
    // SAFETY: the token just opened, closed when this drops.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    let mut buffer = vec![0u64; 256];
    let mut bytes = 0;
    // SAFETY: the buffer holds the size given.
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            class,
            Some(buffer.as_mut_ptr().cast()),
            u32::try_from(buffer.len() * 8).unwrap(),
            &raw mut bytes,
        )
    }
    .unwrap();
    buffer
}

/// Whether `process`'s token is an AppContainer's, its capability count
/// and its integrity level.
pub fn token_facts(process: HANDLE) -> (bool, u32, u32) {
    let container = token(process, TokenIsAppContainer)[0] & 0xffff_ffff != 0;
    let groups = token(process, TokenCapabilities);
    // SAFETY: the buffer holds a `TOKEN_GROUPS`.
    let capabilities = unsafe { (*groups.as_ptr().cast::<TOKEN_GROUPS>()).GroupCount };
    let label = token(process, TokenIntegrityLevel);
    // SAFETY: the buffer holds a `TOKEN_MANDATORY_LABEL` and its SID, whose
    // last sub-authority is the level.
    let level = unsafe {
        let sid = (*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid;
        *GetSidSubAuthority(sid, u32::from(*GetSidSubAuthorityCount(sid)) - 1)
    };
    (container, capabilities, level)
}

/// One probe's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub ok: bool,
    pub code: u64,
    pub detail: String,
}

/// The probe results in a worker's log, by name (the last of a name).
pub fn probes(log: &str) -> BTreeMap<String, Probe> {
    log.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("probe ")?;
            let (name, rest) = rest.split_once(" ok=")?;
            let (ok, rest) = rest.split_once(" code=")?;
            let (code, detail) = rest.split_once(' ').unwrap_or((rest, ""));
            let code = match code.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).ok()?,
                None => code.parse().ok()?,
            };
            Some((
                name.to_owned(),
                Probe {
                    ok: ok == "1",
                    code,
                    detail: detail.to_owned(),
                },
            ))
        })
        .collect()
}

/// `name`'s result in `found`, or a panic naming the log.
pub fn probe<'a>(found: &'a BTreeMap<String, Probe>, name: &str, log: &str) -> &'a Probe {
    found
        .get(name)
        .unwrap_or_else(|| panic!("no probe {name}; the worker's log:\n{log}"))
}

/// Each processor core's logical processors, as a mask, from the system's
/// logical processor information.
pub fn processor_cores() -> Vec<usize> {
    use windows::Win32::System::SystemInformation::{
        GetLogicalProcessorInformation, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION,
    };
    let mut len = 0;
    // SAFETY: a size query.
    let _ = unsafe { GetLogicalProcessorInformation(None, &raw mut len) };
    let n = len as usize / size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION>();
    let mut info = vec![SYSTEM_LOGICAL_PROCESSOR_INFORMATION::default(); n];
    // SAFETY: a buffer of `len` bytes.
    unsafe { GetLogicalProcessorInformation(Some(info.as_mut_ptr()), &raw mut len) }.unwrap();
    info.iter()
        .filter(|i| i.Relationship == RelationProcessorCore)
        .map(|i| i.ProcessorMask)
        .collect()
}

/// Waits up to `limit` for the process `process` to end: its exit code,
/// once it has.
pub fn exit_of(process: HANDLE, limit: Duration) -> Option<u32> {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

    let ms = u32::try_from(limit.as_millis()).unwrap();
    // SAFETY: a process handle with the right to wait on it.
    if unsafe { WaitForSingleObject(process, ms) } != WAIT_OBJECT_0 {
        return None;
    }
    let mut code = 0;
    // SAFETY: as above, with the right to query it.
    unsafe { GetExitCodeProcess(process, &raw mut code) }.unwrap();
    Some(code)
}

/// Asserts that the process the failed launch `e` made ends with
/// `REFUSED`: as the launch saw, or, when it stopped watching first, within
/// a minute on the handle it kept. One that does not is terminated before
/// the assertion fails, so it outlives no test.
pub fn ends_refused(e: &LaunchError) {
    use windows::Win32::System::Threading::TerminateProcess;

    match e.process {
        Made::Ended(code) => assert_eq!((code, e.unended.is_none()), (Some(REFUSED), true), "{e}"),
        Made::Pending => {
            let kept = e
                .unended
                .as_ref()
                .unwrap_or_else(|| panic!("no process kept: {e}"));
            let process = HANDLE(kept.as_raw_handle());
            let code = exit_of(process, Duration::from_secs(60));
            if code.is_none() {
                // SAFETY: the process the launch kept.
                let _ = unsafe { TerminateProcess(process, 1) };
            }
            assert_eq!(code, Some(REFUSED), "it did not end: {e}");
        }
        other => panic!("{other:?}: {e}"),
    }
}

/// Peeks at `due` in the frame ring `reader` reads and takes the frame
/// due, if any, at the tight pitch (rows of the luma and chroma widths)
/// into `out`, which it sizes to a frame's bytes: the frame taken, and
/// whether slots were freed (a take always frees its frame's).
///
/// # Errors
///
/// The fault that failed the player, as `peek` and `take` return it.
pub fn read_tight(
    reader: &mut skin_model::media::Reader,
    words: &[std::sync::atomic::AtomicU64],
    due: i64,
    out: &mut Vec<u8>,
) -> Result<(Option<skin_model::media::Frame>, bool), skin_model::media::Fault> {
    let peek = reader.peek(words, due)?;
    let Some(d) = peek.due else {
        return Ok((None, peek.freed));
    };
    let g = reader.geometry();
    out.resize(g.frame_bytes(), 0);
    let w = g.width() as usize;
    let mut sink = |at: usize, bytes: &[u8]| {
        out.get_mut(at..at + bytes.len())
            .map(|d| d.copy_from_slice(bytes))
            .is_some()
    };
    let f = reader.take(words, d, [w, w.div_ceil(2)], &mut sink)?;
    Ok((Some(f), true))
}
