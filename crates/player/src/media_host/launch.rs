//! The worker's launch (design.md "Threat Model" T02), in the order the
//! sandbox needs:
//!
//! 1. a job: its committed memory capped, the worker ended when the job
//!    closes and on an unhandled exception, one process, its UI
//!    restricted, and a completion port for its messages ([`watch::port`]);
//! 2. the worker's AppContainer profile, with no capability, one for each
//!    installation directory, named for the directory opened, not its path
//!    ([`Installation`]), and an ACE granting it read and execute on that
//!    same directory, inherited by the files directly in it (the executable
//!    and its DLLs);
//! 3. a desktop of its own, granted to the AppContainer, in the session's
//!    alternate window station (`CreateWindowStationW` with no name),
//!    which other launches share and the launch leaves as it is;
//! 4. the creation attributes: the control pipe's two ends as the only
//!    handles it inherits, no child process, the mitigation policies (no
//!    dynamic code, no win32k system calls, strict handle checks, images
//!    only from local files without a low label and from System32 first,
//!    no extension points) and the AppContainer;
//! 5. the process created suspended, detached and below normal priority,
//!    with an environment the launch builds ([`environment`]), then put in
//!    the job;
//! 6. the check before it runs: it is still suspended and in the job, the
//!    job's limits and UI restrictions read back exactly as set, the
//!    queried mitigation policies hold, its token is the AppContainer's
//!    with no capability at low integrity, and it holds exactly the pipe's
//!    ends; then its watcher starts ([`watch`]), and it resumes and must
//!    answer with its hello within the handshake deadline.
//!
//! A step that fails, or a check that finds anything else, terminates the
//! process, if one was made, and fails the launch: never a weaker sandbox.
//! The desktop is made as browsers' sandboxes make an alternate desktop for
//! their sandboxed processes: the process's window station is switched to
//! the alternate one while the desktop is created in it, under a lock, and
//! switched back.
//!
//! A test's setup ([`Setup`], only under `cfg(test)`) removes restrictions,
//! changes what the check expects, makes a step fail, or passes the worker
//! arguments; a build of the game holds the recipe alone. A setup without
//! the win32k lockdown, whose worker connects to the window station when it
//! loads USER32, grants the AppContainer the station too, until the worker
//! drops.

use std::ffi::c_void;
use std::fmt::Write as _;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use skin_model::media::control::{self, Reply};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_TIMEOUT, GENERIC_ALL, GetLastError, HANDLE,
    HANDLE_FLAG_INHERIT, HANDLE_FLAGS, HLOCAL, LocalFree, SetHandleInformation, WAIT_ABANDONED,
    WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetSecurityInfo, SE_FILE_OBJECT,
    SE_OBJECT_TYPE, SE_WINDOW_OBJECT, SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName, GetAppContainerFolderPath,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_FLAGS, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, EqualSid, FreeSid, GetAce, GetAclInformation, GetSidSubAuthority,
    GetSidSubAuthorityCount, GetTokenInformation, NO_INHERITANCE, NO_PROPAGATE_INHERIT_ACE,
    OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES,
    TOKEN_APPCONTAINER_INFORMATION, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TOKEN_USER, TokenAppContainerSid, TokenCapabilities, TokenIntegrityLevel,
    TokenIsAppContainer, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_ID_INFO,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo,
    GetFileInformationByHandleEx, READ_CONTROL, WRITE_DAC,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_UILIMIT,
    JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CloseWindowStation, CreateDesktopW, CreateWindowStationW, DESKTOP_CONTROL_FLAGS,
    GetProcessWindowStation, GetUserObjectInformationW, HDESK, HWINSTA, SetProcessWindowStation,
    UOI_NAME,
};
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::System::SystemServices::SECURITY_MANDATORY_LOW_RID;
use windows::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateMutexW,
    CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetExitCodeProcess, GetProcessMitigationPolicy,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, PROCESS_MITIGATION_POLICY,
    ProcessChildProcessPolicy, ProcessDynamicCodePolicy, ProcessExtensionPointDisablePolicy,
    ProcessImageLoadPolicy, ProcessStrictHandleCheckPolicy, ProcessSystemCallDisablePolicy,
    ReleaseMutex, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, SuspendThread,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::Win32::System::WindowsProgramming::PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;
use windows::core::{PCWSTR, PWSTR, w};

use super::watch::{self, Watch};
use super::{Hello, Owned, Worker, handles};

/// The worker's executable, in the directory the game launches it from.
pub const EXECUTABLE: &str = "tapestry-media.exe";

/// The most replies the game holds unread from one worker; one more fails
/// the worker. Every reply answers a command, and the game reads them each
/// frame: eight players' replies to a few commands each stay far below it.
const REPLIES: usize = 64;

/// The job's cap on the worker's committed memory.
pub(super) const MEMORY_CAP: usize = 512 << 20;

/// How long the worker may take to answer its launch (a design parameter).
const HANDSHAKE: Duration = Duration::from_secs(2);

/// The exit code a failed launch terminates the worker with.
pub const REFUSED: u32 = 0xc0de_0001;

/// The most log lines the game takes from one worker; past it the worker's
/// log is dropped, with one line saying so.
const MAX_LOG_LINES: usize = 1_000;

// The creation-time mitigation policies (`PROCESS_CREATION_MITIGATION_
// POLICY_*_ALWAYS_ON`, winbase.h).
const STRICT_HANDLES_ON: u64 = 1 << 24;
const WIN32K_OFF: u64 = 1 << 28;
const EXTENSION_POINTS_OFF: u64 = 1 << 32;
const DYNAMIC_CODE_OFF: u64 = 1 << 36;
const REMOTE_IMAGES_OFF: u64 = 1 << 52;
const LOW_LABEL_IMAGES_OFF: u64 = 1 << 56;
const SYSTEM32_IMAGES_FIRST: u64 = 1 << 60;

/// The restrictions a launch applies, or its check expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Restrictions(u16);

impl Restrictions {
    /// The AppContainer, with no capability.
    pub const APP_CONTAINER: Self = Self(1);
    /// The job: the memory cap, the end on its closing and on an unhandled
    /// exception, and the UI restrictions.
    pub const JOB: Self = Self(1 << 1);
    /// The job's limit of one process.
    pub const ONE_PROCESS: Self = Self(1 << 2);
    /// No child process.
    pub const NO_CHILD: Self = Self(1 << 3);
    /// No dynamic code.
    pub const NO_DYNAMIC_CODE: Self = Self(1 << 4);
    /// No win32k system call.
    pub const NO_WIN32K: Self = Self(1 << 5);
    /// Strict handle checks.
    pub const STRICT_HANDLES: Self = Self(1 << 6);
    /// Images only from local files without a low label, System32 first.
    pub const IMAGE_LOAD: Self = Self(1 << 7);
    /// No extension point.
    pub const NO_EXTENSION_POINTS: Self = Self(1 << 8);
    /// A desktop of its own, off the interactive window station.
    pub const OWN_DESKTOP: Self = Self(1 << 9);
    /// Only the control pipe's ends inherited.
    pub const HANDLE_LIST: Self = Self(1 << 10);
    /// Every restriction: the recipe.
    pub const RECIPE: Self = Self((1 << 11) - 1);

    /// Whether every restriction of `r` is in `self`.
    pub const fn has(self, r: Self) -> bool {
        self.0 & r.0 == r.0
    }
}

#[cfg(test)]
impl Restrictions {
    /// No restriction.
    pub const NONE: Self = Self(0);

    /// `self` without `r`.
    pub const fn without(self, r: Self) -> Self {
        Self(self.0 & !r.0)
    }

    /// `self` with `r`.
    pub const fn with(self, r: Self) -> Self {
        Self(self.0 | r.0)
    }
}

/// A step of the launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Job,
    Profile,
    Grant,
    Desktop,
    Pipes,
    Attributes,
    Create,
    Assign,
    Check,
    Watch,
    Resume,
    Handshake,
}

#[cfg(test)]
impl Step {
    /// Every step, in order.
    pub const ALL: [Self; 12] = [
        Self::Job,
        Self::Profile,
        Self::Grant,
        Self::Desktop,
        Self::Pipes,
        Self::Attributes,
        Self::Create,
        Self::Assign,
        Self::Check,
        Self::Watch,
        Self::Resume,
        Self::Handshake,
    ];
}

/// A failed launch.
#[derive(Debug)]
pub struct LaunchError {
    /// The step that failed.
    pub step: Step,
    /// The system's error, or 0 when the check found a difference.
    pub code: u32,
    /// What failed or differed.
    pub what: String,
    /// What became of the process, when the launch made one.
    pub process: Made,
}

/// What became of the process a failed launch made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Made {
    /// The launch failed before it made one.
    Nothing,
    /// The launch terminated it, and it ended: its exit code, when Windows
    /// gave it.
    Ended(Option<u32>),
    /// The launch terminated it, and it had not ended when the launch
    /// stopped waiting, 5 s on.
    Running,
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the media worker's launch failed at {:?}: {} (error {})",
            self.step, self.what, self.code
        )?;
        match self.process {
            Made::Nothing => Ok(()),
            Made::Ended(Some(exit)) => write!(f, "; it was ended (exit code {exit:#x})"),
            Made::Ended(None) => write!(f, "; it was ended"),
            Made::Running => write!(f, "; it had not ended 5 s after it was terminated"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// A step's failure: the system's error and what failed.
type Failure = (u32, String);

/// What a launch applies and checks: the recipe, or a test's variation.
#[derive(Clone, Debug)]
pub struct Setup {
    applied: Restrictions,
    expected: Restrictions,
    #[cfg(test)]
    fail_at: Option<Step>,
    #[cfg(test)]
    args: Vec<String>,
    #[cfg(test)]
    handshake: Duration,
    #[cfg(test)]
    watching: watch::Variation,
}

impl Setup {
    /// The recipe: every restriction applied and checked.
    pub const fn recipe() -> Self {
        Self {
            applied: Restrictions::RECIPE,
            expected: Restrictions::RECIPE,
            #[cfg(test)]
            fail_at: None,
            #[cfg(test)]
            args: Vec::new(),
            #[cfg(test)]
            handshake: HANDSHAKE,
            #[cfg(test)]
            watching: watch::Variation {
                no_message: false,
                no_accounting: false,
                discard_message: false,
            },
        }
    }

    /// Whether `step` is to fail as if its call had.
    #[cfg(test)]
    fn fails(&self, step: Step) -> bool {
        self.fail_at == Some(step)
    }

    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "a test's setup makes steps fail; the recipe never does"
    )]
    const fn fails(&self, _: Step) -> bool {
        false
    }

    /// The worker's arguments.
    #[cfg(test)]
    fn args(&self) -> &[String] {
        &self.args
    }

    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "a test's setup passes the worker arguments; the recipe passes none"
    )]
    const fn args(&self) -> &[String] {
        &[]
    }

    /// How long the worker may take to answer its launch.
    #[cfg(test)]
    const fn handshake(&self) -> Duration {
        self.handshake
    }

    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "a test's setup may wait longer; the recipe waits `HANDSHAKE`"
    )]
    const fn handshake(&self) -> Duration {
        HANDSHAKE
    }

    /// What the test changes in the job's watcher.
    #[cfg(test)]
    const fn watching(&self) -> watch::Variation {
        self.watching
    }

    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "a test's setup may change the watcher; the recipe does not"
    )]
    fn watching(&self) -> watch::Variation {
        watch::Variation::default()
    }
}

#[cfg(test)]
impl Setup {
    /// Without `r`, applied or checked.
    pub const fn without(mut self, r: Restrictions) -> Self {
        self.applied = self.applied.without(r);
        self.expected = self.expected.without(r);
        self
    }

    /// Applying exactly `r`.
    pub const fn applying(mut self, r: Restrictions) -> Self {
        self.applied = r;
        self
    }

    /// Checking exactly `r`.
    pub const fn expecting(mut self, r: Restrictions) -> Self {
        self.expected = r;
        self
    }

    /// With `step` failing as if its call had.
    pub const fn failing_at(mut self, step: Step) -> Self {
        self.fail_at = Some(step);
        self
    }

    /// Passing the worker `args`.
    pub fn args_of(mut self, args: &[&str]) -> Self {
        self.args = args.iter().map(|&a| a.to_owned()).collect();
        self
    }

    /// Waiting up to `limit` for the worker's hello.
    pub const fn waiting(mut self, limit: Duration) -> Self {
        self.handshake = limit;
        self
    }

    /// With the job's watcher changed as `variation` says.
    pub const fn watching_as(mut self, variation: watch::Variation) -> Self {
        self.watching = variation;
        self
    }
}

/// The calling thread's last error.
fn last_error() -> u32 {
    // SAFETY: a plain query.
    unsafe { GetLastError() }.0
}

/// The system's error a failed call returned.
fn code_of(e: &windows::core::Error) -> u32 {
    let code = e.code().0.cast_unsigned();
    // A Win32 error wrapped as an HRESULT.
    if code & 0xffff_0000 == 0x8007_0000 {
        code & 0xffff
    } else {
        code
    }
}

/// `what` failed with `e`.
fn failed(what: &str) -> impl FnOnce(windows::core::Error) -> Failure + '_ {
    move |e| (code_of(&e), what.to_owned())
}

/// `path`, nul-terminated.
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// Runs `step` unless the setup makes it fail.
fn run<T>(
    setup: &Setup,
    step: Step,
    f: impl FnOnce() -> Result<T, Failure>,
) -> Result<T, (Step, Failure)> {
    if setup.fails(step) {
        return Err((step, (0, "made to fail".to_owned())));
    }
    f().map_err(|failure| (step, failure))
}

/// `(step, failure)` before a process was made.
fn early((step, (code, what)): (Step, Failure)) -> LaunchError {
    LaunchError {
        step,
        code,
        what,
        process: Made::Nothing,
    }
}

/// Launches the worker in `dir` under `setup`.
pub fn launch(dir: &Path, setup: &Setup) -> Result<Worker, LaunchError> {
    let applied = setup.applied;
    let (job, port) = if applied.has(Restrictions::JOB) {
        let (job, port) = run(setup, Step::Job, || job(applied)).map_err(early)?;
        (Some(job), Some(port))
    } else {
        (None, None)
    };
    // The installation stays open, so where it is, until the launch ends.
    let profiled = if applied.has(Restrictions::APP_CONTAINER) {
        Some(
            run(setup, Step::Profile, || {
                let installation = Installation::open(dir)?;
                let sid = profile(&installation.profile_name()?)?;
                Ok((installation, sid))
            })
            .map_err(early)?,
        )
    } else {
        None
    };
    if let Some((installation, sid)) = &profiled {
        run(setup, Step::Grant, || installation.grant(sid)).map_err(early)?;
    }
    let sid = profiled.as_ref().map(|(_, sid)| sid);
    let desktop = if applied.has(Restrictions::OWN_DESKTOP) {
        Some(
            run(setup, Step::Desktop, || {
                let desktop = Desktop::new(sid)?;
                #[cfg(test)]
                let desktop = desktop.for_setup(sid, applied)?;
                Ok(desktop)
            })
            .map_err(early)?,
        )
    } else {
        None
    };
    let pipes = run(setup, Step::Pipes, Pipes::new).map_err(early)?;
    let child = create(dir, setup, sid, &pipes, desktop.as_ref()).map_err(early)?;
    let pipe_ends = [pipes.worker_in.0, pipes.worker_out.0].map(value);
    let Pipes {
        commands,
        replies,
        worker_in,
        worker_out,
    } = pipes;
    // The worker holds its own copies of its ends now.
    drop((worker_in, worker_out));
    let checked = (|| {
        if let Some(job) = &job {
            run(setup, Step::Assign, || {
                // SAFETY: the job and the process this launch made.
                unsafe { AssignProcessToJobObject(job.0, child.process.0) }
                    .map_err(failed("assigning it to its job"))
            })?;
        }
        let table = run(setup, Step::Check, || {
            check(&child, job.as_ref(), sid, pipe_ends, setup.expected)
        })?;
        // Watched from before it runs: a worker whose memory reaches the
        // cap at once is ended as one that reaches it later is.
        let watch = match (&job, port) {
            (Some(job), Some(port)) => Some(run(setup, Step::Watch, || {
                Watch::start(job, port, child.process.0, child.pid, setup.watching())
                    .map_err(|code| (code, "its job's watcher".to_owned()))
            })?),
            _ => None,
        };
        // Suspended once, by its creation.
        run(setup, Step::Resume, || child.resume(1))?;
        Ok((table, watch))
    })();
    let (table, watch) = match checked {
        Ok(checked) => checked,
        Err(e) => return Err(child.refuse(e)),
    };
    #[cfg(test)]
    let log = std::sync::Arc::new(Mutex::new(String::new()));
    let (tx, rx) = mpsc::sync_channel(REPLIES);
    let overflowed = Arc::new(AtomicBool::new(false));
    let reading = {
        #[cfg(test)]
        let log = std::sync::Arc::clone(&log);
        let overflowed = Arc::clone(&overflowed);
        std::thread::Builder::new()
            .name("media worker replies".to_owned())
            .spawn(move || {
                read_replies(
                    replies,
                    &tx,
                    &overflowed,
                    #[cfg(test)]
                    &log,
                );
            })
    };
    if let Err(e) = reading {
        let code = e.raw_os_error().map_or(0, i32::cast_unsigned);
        return Err(child.refuse((Step::Handshake, (code, "its replies' thread".to_owned()))));
    }
    let hello = match run(setup, Step::Handshake, || handshake(&rx, setup.handshake())) {
        Ok(hello) => hello,
        Err(e) => return Err(child.refuse(e)),
    };
    let (process, pid) = child.release();
    // SAFETY: the game's end of the pipe to the worker, owned by the file
    // from here.
    let commands = unsafe { std::fs::File::from_raw_handle(commands.release().0) };
    #[cfg(not(test))]
    let _ = table;
    Ok(Worker {
        commands: Some(commands),
        replies: rx,
        overflowed,
        hello,
        process,
        pid,
        job,
        watch,
        open_files: std::cell::RefCell::new(std::collections::BTreeMap::new()),
        _desktop: desktop,
        #[cfg(test)]
        log,
        #[cfg(test)]
        suspended: table,
        #[cfg(test)]
        pipe_ends,
    })
}

/// A handle's value, as the handle table and the control protocol give it.
fn value(h: HANDLE) -> u64 {
    h.0.expose_provenance() as u64
}

impl Owned {
    /// The handle, no longer closed by this value.
    fn release(self) -> HANDLE {
        let h = self.0;
        std::mem::forget(self);
        h
    }
}

/// The job's limits under `r`: the memory cap, the end on the job's
/// closing and on an unhandled exception, one process under
/// [`Restrictions::ONE_PROCESS`], and no breakaway.
fn job_flags(r: Restrictions) -> JOB_OBJECT_LIMIT {
    let mut flags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    if r.has(Restrictions::ONE_PROCESS) {
        flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    }
    flags
}

/// The job's UI restrictions.
const JOB_UI: JOB_OBJECT_UILIMIT = JOB_OBJECT_UILIMIT(
    JOB_OBJECT_UILIMIT_DESKTOP.0
        | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS.0
        | JOB_OBJECT_UILIMIT_EXITWINDOWS.0
        | JOB_OBJECT_UILIMIT_GLOBALATOMS.0
        | JOB_OBJECT_UILIMIT_HANDLES.0
        | JOB_OBJECT_UILIMIT_READCLIPBOARD.0
        | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS.0
        | JOB_OBJECT_UILIMIT_WRITECLIPBOARD.0,
);

/// Step 1: the job, and the completion port its messages come to.
fn job(applied: Restrictions) -> Result<(Owned, Owned), Failure> {
    // SAFETY: an unnamed job, owned by the value returned.
    let job = Owned(unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(failed("a job"))?);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = job_flags(applied);
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.ProcessMemoryLimit = MEMORY_CAP;
    // SAFETY: the job just made, and a struct of the size given.
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            u32::try_from(size_of_val(&limits)).unwrap_or(0),
        )
    }
    .map_err(failed("the job's limits"))?;
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_UI,
    };
    // SAFETY: as above.
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectBasicUIRestrictions,
            (&raw const ui).cast(),
            u32::try_from(size_of_val(&ui)).unwrap_or(0),
        )
    }
    .map_err(failed("the job's UI restrictions"))?;
    let port = watch::port(&job).map_err(failed("the job's completion port"))?;
    Ok((job, port))
}

/// The AppContainer profile's SID, freed when dropped.
struct Sid(PSID);

impl Drop for Sid {
    fn drop(&mut self) {
        // SAFETY: a SID the profile's calls allocated, which `FreeSid`
        // frees.
        let _ = unsafe { FreeSid(self.0) };
    }
}

/// How many times the profile is asked for while another process makes it.
const PROFILE_ATTEMPTS: u32 = 20;

/// The installation directory, open: the worker's AppContainer profile is
/// named for the directory this handle opened, and the grant goes to that
/// directory alone. The directory cannot be renamed or deleted while it is
/// open.
pub struct Installation {
    /// The directory, open to read its DACL and its ID.
    dir: std::fs::File,
    /// The path it was opened by.
    path: std::path::PathBuf,
}

impl Installation {
    /// Opens the directory `dir`.
    ///
    /// # Errors
    ///
    /// When the directory cannot be opened.
    pub fn open(dir: &Path) -> Result<Self, Failure> {
        Ok(Self {
            dir: open_directory(dir, READ_CONTROL.0 | FILE_READ_ATTRIBUTES.0)?,
            path: dir.to_path_buf(),
        })
    }

    /// The directory's handle.
    fn handle(&self) -> HANDLE {
        HANDLE(self.dir.as_raw_handle())
    }

    /// The name of the worker's AppContainer profile for this installation:
    /// `tapestry.media.` and the first 16 bytes of the SHA-256 of the
    /// directory's volume serial number and 128-bit file ID, in hex. A
    /// directory keeps its ID when it is renamed and shares it with no other
    /// on its volume, so each installation's worker runs as an AppContainer
    /// of its own: another directory put where one was, or one whose path
    /// differs from another's in case alone, gets a profile of its own, and
    /// the grant one installation's directory gets reaches no other
    /// installation's worker. Windows keeps a profile until it is deleted:
    /// an installation removed leaves its profile, which grants nothing
    /// outside that installation's own directory, and the game deletes
    /// none, since it cannot tell whether that installation still runs.
    ///
    /// # Errors
    ///
    /// When the system does not give the directory's ID.
    pub fn profile_name(&self) -> Result<String, Failure> {
        let (volume, file) = id(self.handle())?;
        let mut digest = Sha256::new();
        digest.update(volume.to_le_bytes());
        digest.update(file);
        let hash = digest.finalize();
        let mut name = "tapestry.media.".to_owned();
        for byte in hash.iter().take(16) {
            let _ = write!(name, "{byte:02x}");
        }
        Ok(name)
    }

    /// Step 2: grants the AppContainer read and execute on the directory
    /// and the files directly in it, unless an equal ACE is there already.
    /// Windows opens no directory again from a handle (`ReOpenFile` refuses
    /// one), so the directory is opened again by its path, with the right to
    /// change its DACL, and granted only when that is the directory this
    /// value holds: the same volume and file ID. While this value holds it,
    /// its path names no other.
    fn grant(&self, sid: &Sid) -> Result<(), Failure> {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the directory's handle, opened to read its DACL; the
        // descriptor returned holds the DACL and is freed by `Local`.
        let read = unsafe {
            GetSecurityInfo(
                self.handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&raw mut dacl),
                None,
                Some(&raw mut descriptor),
            )
        };
        let _descriptor = Local(descriptor.0);
        if read.is_err() {
            return Err((read.0, "the directory's DACL".to_owned()));
        }
        if granted(dacl, sid) {
            return Ok(());
        }
        let entry = explicit(sid.0, DIRECTORY_ACCESS, DIRECTORY_INHERITANCE);
        let mut new: *mut ACL = std::ptr::null_mut();
        // SAFETY: one entry naming the SID, merged into the directory's
        // DACL; the ACL returned is freed by `Local`.
        let merged = unsafe { SetEntriesInAclW(Some(&[entry]), Some(dacl), &raw mut new) };
        let _new = Local(new.cast());
        if merged.is_err() {
            return Err((merged.0, "the directory's new DACL".to_owned()));
        }
        let opened = open_directory(&self.path, (READ_CONTROL | WRITE_DAC).0)?;
        let writable = HANDLE(opened.as_raw_handle());
        if id(writable)? != id(self.handle())? {
            return Err((
                0,
                "the directory, to grant it: its path names another".to_owned(),
            ));
        }
        // SAFETY: the directory opened again, with the right to change its
        // DACL; the DACL is applied to it and inherited by the files in it.
        let set = unsafe {
            SetSecurityInfo(
                writable,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(new),
                None,
            )
        };
        if set.is_err() {
            return Err((set.0, "granting the directory".to_owned()));
        }
        Ok(())
    }
}

/// Opens the directory `dir` with `access`, sharing read and write, not
/// delete.
fn open_directory(dir: &Path, access: u32) -> Result<std::fs::File, Failure> {
    std::fs::OpenOptions::new()
        .access_mode(access)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(dir)
        .map_err(|e| {
            let code = e.raw_os_error().and_then(|c| u32::try_from(c).ok());
            (code.unwrap_or(0), "the installation directory".to_owned())
        })
}

/// The volume serial number and 128-bit file ID of the file or directory
/// `handle` opened.
fn id(handle: HANDLE) -> Result<(u64, [u8; 16]), Failure> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: an open handle, and a struct of the size given.
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&raw mut info).cast(),
            u32::try_from(size_of_val(&info)).unwrap_or(0),
        )
    }
    .map_err(failed("the installation directory's ID"))?;
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

/// The name of the worker's AppContainer profile for the installation in
/// `dir` ([`Installation::profile_name`]).
///
/// # Errors
///
/// When the directory cannot be opened or gives no ID.
pub fn profile_name(dir: &Path) -> Result<String, Failure> {
    Installation::open(dir)?.profile_name()
}

/// How long a launch waits for another making a profile.
const PROFILES_WAIT_MS: u32 = 30_000;

/// Step 2: the SID of the AppContainer profile `name`. The first launch
/// from an installation makes its profile, which Windows keeps; a later one
/// finds its folder and leaves it alone. Asking `CreateAppContainerProfile`
/// for a profile that exists is not harmless: the call, made while another
/// launch creates its process in the AppContainer, can fail that creation
/// with `ERROR_FILE_NOT_FOUND` (2 of 120 launches from four threads at
/// once). So the user's launches, in every process of the session, make a
/// profile one at a time, each looking for it again once it holds the
/// mutex ([`profiles_mutex`]).
fn profile(name: &str) -> Result<Sid, Failure> {
    let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a nul-terminated name; the SID returned is freed by `Sid`.
    let sid = unsafe { DeriveAppContainerSidFromAppContainerName(PCWSTR(name.as_ptr())) }
        .map(Sid)
        .map_err(failed("the profile's SID"))?;
    if made(&sid)? {
        return Ok(sid);
    }
    let mutex = profiles_mutex()?;
    // SAFETY: the mutex just opened. One a process ended holding is taken
    // all the same: the profile is looked for again under it.
    let waited = unsafe { WaitForSingleObject(mutex.0, PROFILES_WAIT_MS) };
    if waited != WAIT_OBJECT_0 && waited != WAIT_ABANDONED {
        return Err((ERROR_TIMEOUT.0, "the profiles' mutex".to_owned()));
    }
    let making = (|| {
        if !made(&sid)? {
            make_profile(&name)?;
        }
        Ok(())
    })();
    // SAFETY: the mutex this thread holds.
    let _ = unsafe { ReleaseMutex(mutex.0) };
    making.map(|()| sid)
}

/// The mutex the user's launches take to make a profile, in the session's
/// namespace: `Local\tapestry-media-profiles-` and the user's SID.
fn profiles_mutex() -> Result<Owned, Failure> {
    let mut token = HANDLE::default();
    // SAFETY: this process's own token, closed by `Owned`.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }
        .map_err(failed("the game's token"))?;
    let token = Owned(token);
    let buffer = token_information(token.0, TokenUser)?;
    // SAFETY: the buffer holds a `TOKEN_USER`, whose SID lies inside it.
    let user = unsafe { buffer.as_ptr().cast::<TOKEN_USER>().read() };
    let mut text = PWSTR::null();
    // SAFETY: the user's SID; the string returned is freed by `Local`.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut text) }
        .map_err(failed("the user's SID as text"))?;
    let _text = Local(text.0.cast());
    // SAFETY: the nul-terminated string the call returned.
    let sid = unsafe { text.as_wide() };
    let name: Vec<u16> = "Local\\tapestry-media-profiles-"
        .encode_utf16()
        .chain(sid.iter().copied())
        .chain([0])
        .collect();
    // SAFETY: a nul-terminated name; the mutex is closed by `Owned`.
    unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
        .map(Owned)
        .map_err(failed("the profiles' mutex"))
}

/// Whether the profile of `sid` exists: whether Windows gives its folder.
fn made(sid: &Sid) -> Result<bool, Failure> {
    folder(sid).map(|folder| folder.is_some())
}

/// The folder of the profile of `sid`, `None` when the profile does not
/// exist.
fn folder(sid: &Sid) -> Result<Option<Vec<u16>>, Failure> {
    let mut text = PWSTR::null();
    // SAFETY: a SID the profile's calls allocated; the string returned is
    // freed by `Local`.
    unsafe { ConvertSidToStringSidW(sid.0, &raw mut text) }
        .map_err(failed("the profile's SID as text"))?;
    let _text = Local(text.0.cast());
    // SAFETY: a nul-terminated SID string; the path returned is freed here.
    match unsafe { GetAppContainerFolderPath(PCWSTR(text.0)) } {
        Ok(path) => {
            // SAFETY: the nul-terminated path the call allocated, copied
            // before it is freed.
            let copy = unsafe { path.as_wide() }.to_vec();
            // SAFETY: the path the call allocated, unused after this.
            unsafe { CoTaskMemFree(Some(path.0.cast())) };
            Ok(Some(copy))
        }
        Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(None),
        Err(e) => Err(failed("the profile's folder")(e)),
    }
}

/// Makes the AppContainer profile `name` (nul-terminated), or finds it
/// made. Another process making it at the same moment fails the call, so it
/// is asked for again, a little later, a bounded number of times.
fn make_profile(name: &[u16]) -> Result<(), Failure> {
    let mut attempt = 1;
    loop {
        // SAFETY: nul-terminated names and no capability; the SID returned
        // is freed by `Sid`.
        let made = unsafe {
            CreateAppContainerProfile(
                PCWSTR(name.as_ptr()),
                w!("tapestry media"),
                w!("Decodes the videos tapestry plays"),
                None,
            )
        };
        match made {
            Ok(sid) => {
                drop(Sid(sid));
                return Ok(());
            }
            Err(e) if e.code() == ERROR_ALREADY_EXISTS.to_hresult() => return Ok(()),
            Err(e) if attempt == PROFILE_ATTEMPTS => return Err(failed("the profile")(e)),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// The access the AppContainer gets on the worker's directory and files.
const DIRECTORY_ACCESS: u32 = FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0;
/// Inherited by the files directly in the directory, and no further.
const DIRECTORY_INHERITANCE: ACE_FLAGS =
    ACE_FLAGS(OBJECT_INHERIT_ACE.0 | NO_PROPAGATE_INHERIT_ACE.0);

/// Memory `LocalAlloc` made, freed when dropped.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: memory the security calls allocated with `LocalAlloc`.
            let _ = unsafe { LocalFree(Some(HLOCAL(self.0))) };
        }
    }
}

/// Whether `dacl` holds an explicit ACE allowing `sid` the directory's
/// access with its inheritance.
fn granted(dacl: *const ACL, sid: &Sid) -> bool {
    if dacl.is_null() {
        return false;
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: a DACL the security call returned, and a struct of the size
    // given.
    let counted = unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast(),
            u32::try_from(size_of_val(&size)).unwrap_or(0),
            AclSizeInformation,
        )
    };
    if counted.is_err() {
        return false;
    }
    (0..size.AceCount).any(|i| {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: ACE `i` of the DACL, which holds `AceCount` of them.
        if unsafe { GetAce(dacl, i, &raw mut ace) }.is_err() {
            return false;
        }
        // SAFETY: each ACE starts with its header.
        let header = unsafe { ace.cast::<ACE_HEADER>().read_unaligned() };
        if u32::from(header.AceType)
            != windows::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE
            || u32::from(header.AceFlags) != DIRECTORY_INHERITANCE.0
        {
            return false;
        }
        let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
        // SAFETY: an allowed ACE: its mask, then its SID from `SidStart`.
        let (mask, ace_sid) = unsafe {
            (
                (&raw const (*allowed).Mask).read_unaligned(),
                PSID((&raw mut (*allowed).SidStart).cast()),
            )
        };
        // SAFETY: two valid SIDs.
        mask & DIRECTORY_ACCESS == DIRECTORY_ACCESS && unsafe { EqualSid(ace_sid, sid.0) }.is_ok()
    })
}

/// An entry granting `sid` `access` with `inheritance`.
fn explicit(sid: PSID, access: u32, inheritance: ACE_FLAGS) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: access,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: PWSTR(sid.0.cast()),
            ..TRUSTEE_W::default()
        },
    }
}

/// Grants `sid` every access on the window object `object`.
fn grant_object(object: HANDLE, sid: PSID, what: &str) -> Result<(), Failure> {
    change_object(object, explicit(sid, GENERIC_ALL.0, NO_INHERITANCE), what)
}

/// Merges `entry` into the DACL of the window object `object`: the DACL is
/// read, and written whole.
fn change_object(object: HANDLE, entry: EXPLICIT_ACCESS_W, what: &str) -> Result<(), Failure> {
    let kind: SE_OBJECT_TYPE = SE_WINDOW_OBJECT;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a window station or desktop the launch holds; the descriptor
    // returned is freed by `Local`.
    let read = unsafe {
        GetSecurityInfo(
            object,
            kind,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&raw mut dacl),
            None,
            Some(&raw mut descriptor),
        )
    };
    let _descriptor = Local(descriptor.0);
    if read.is_err() {
        return Err((read.0, format!("{what}'s DACL")));
    }
    let mut new: *mut ACL = std::ptr::null_mut();
    // SAFETY: as in `grant`.
    let merged = unsafe { SetEntriesInAclW(Some(&[entry]), Some(dacl), &raw mut new) };
    let _new = Local(new.cast());
    if merged.is_err() {
        return Err((merged.0, format!("{what}'s new DACL")));
    }
    // SAFETY: as above.
    let set = unsafe {
        SetSecurityInfo(
            object,
            kind,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new),
            None,
        )
    };
    if set.is_err() {
        return Err((set.0, format!("setting {what}'s DACL")));
    }
    Ok(())
}

/// The mutex every test process takes to change a window object's DACL
/// that other test processes change too.
#[cfg(test)]
const SHARED_DACL: PCWSTR = w!("Local\\tapestry-media-shared-dacl");

/// How long a change waits for that mutex.
#[cfg(test)]
const SHARED_DACL_WAIT_MS: u32 = 10_000;

/// Merges `entry` into the DACL of the window object `object` under the
/// mutex every test process takes for it: each change reads the DACL and
/// writes it whole, so two at once lose one of them, a grant or a
/// revocation.
#[cfg(test)]
pub fn change_shared(object: HANDLE, entry: EXPLICIT_ACCESS_W, what: &str) -> Result<(), Failure> {
    // SAFETY: a mutex named in the session's namespace, closed by `Owned`.
    let mutex = Owned(
        unsafe { CreateMutexW(None, false, SHARED_DACL) }.map_err(failed("the DACL's mutex"))?,
    );
    // SAFETY: the mutex just opened. One a process ended holding is taken
    // all the same: its change was one call, made or not.
    let waited = unsafe { WaitForSingleObject(mutex.0, SHARED_DACL_WAIT_MS) };
    if waited != WAIT_OBJECT_0 && waited != WAIT_ABANDONED {
        return Err((ERROR_TIMEOUT.0, format!("{what}'s DACL mutex")));
    }
    let changed = change_object(object, entry, what);
    // SAFETY: the mutex this thread holds.
    let _ = unsafe { ReleaseMutex(mutex.0) };
    changed
}

/// An entry revoking every access `sid` is granted.
#[cfg(test)]
pub fn revoked(sid: PSID) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: 0,
        grfAccessMode: windows::Win32::Security::Authorization::REVOKE_ACCESS,
        ..explicit(sid, 0, NO_INHERITANCE)
    }
}

/// An entry granting `sid` every access on a window object.
#[cfg(test)]
pub fn granted_all(sid: PSID) -> EXPLICIT_ACCESS_W {
    explicit(sid, GENERIC_ALL.0, NO_INHERITANCE)
}

/// The process's window station is switched while a desktop is made in
/// another: one launch at a time.
static STATION: Mutex<()> = Mutex::new(());

/// Names each desktop the game makes.
static DESKTOPS: AtomicU32 = AtomicU32::new(0);

/// The window station and desktop the worker runs on, closed when dropped.
pub struct Desktop {
    station: HWINSTA,
    desktop: HDESK,
    /// `station\desktop`, nul-terminated, as the creation names it.
    path: Vec<u16>,
    /// The SID a test's setup granted the window station, revoked when this
    /// drops.
    #[cfg(test)]
    station_grant: Option<Vec<u32>>,
}

// SAFETY: window station and desktop handles are values any thread may
// close; `Desktop` only closes them.
unsafe impl Send for Desktop {}

impl Desktop {
    /// Step 3: a desktop of the worker's own, granted to `sid`, in the window
    /// station `CreateWindowStationW` gives without a name.
    fn new(sid: Option<&Sid>) -> Result<Self, Failure> {
        // SAFETY: a window station named by the system (only administrators
        // may name one), closed by the value returned.
        let station = unsafe { CreateWindowStationW(PCWSTR::null(), 0, GENERIC_ALL.0, None) }
            .map_err(failed("a window station"))?;
        let mut desktop = Self {
            station,
            desktop: HDESK::default(),
            path: Vec::new(),
            #[cfg(test)]
            station_grant: None,
        };
        let mut name = [0u16; 256];
        let mut needed = 0;
        // SAFETY: the window station just made, and a buffer of the size
        // given.
        unsafe {
            GetUserObjectInformationW(
                HANDLE(station.0),
                UOI_NAME,
                Some(name.as_mut_ptr().cast()),
                u32::try_from(size_of_val(&name)).unwrap_or(0),
                Some(&raw mut needed),
            )
        }
        .map_err(failed("the window station's name"))?;
        // The window station is the session's one alternate station: every
        // launch, and other programs' sandboxes, get the same. The launch
        // writes nothing to it, since each write reads its DACL and writes it
        // whole, racing the others, and would leave the AppContainer's ACE
        // behind. Under the recipe the worker never connects to it: win32k
        // is locked.
        let desk_name: Vec<u16> = format!(
            "tapestry-media-{}-{}",
            std::process::id(),
            DESKTOPS.fetch_add(1, Ordering::Relaxed)
        )
        .encode_utf16()
        .chain([0])
        .collect();
        {
            let _switching = STATION.lock().unwrap_or_else(PoisonError::into_inner);
            // SAFETY: the process's window station, which it keeps.
            let original = unsafe { GetProcessWindowStation() }
                .map_err(failed("the process's window station"))?;
            // SAFETY: the window station just made.
            unsafe { SetProcessWindowStation(station) }
                .map_err(failed("switching to the window station"))?;
            // SAFETY: a desktop in the window station just made, closed by
            // the value returned.
            let made = unsafe {
                CreateDesktopW(
                    PCWSTR(desk_name.as_ptr()),
                    PCWSTR::null(),
                    None,
                    DESKTOP_CONTROL_FLAGS(0),
                    GENERIC_ALL.0,
                    None,
                )
            };
            // SAFETY: the process's own window station, switched back.
            let back = unsafe { SetProcessWindowStation(original) };
            desktop.desktop = made.map_err(failed("a desktop"))?;
            back.map_err(failed("switching back the window station"))?;
        }
        if let Some(sid) = sid {
            grant_object(HANDLE(desktop.desktop.0), sid.0, "the desktop")?;
        }
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        desktop.path = name
            .get(..end)
            .unwrap_or_default()
            .iter()
            .copied()
            .chain("\\".encode_utf16())
            .chain(desk_name)
            .collect();
        Ok(desktop)
    }
}

#[cfg(test)]
impl Desktop {
    /// The window station's handle.
    pub fn station(&self) -> HANDLE {
        HANDLE(self.station.0)
    }

    /// The desktop's handle.
    pub fn desktop(&self) -> HANDLE {
        HANDLE(self.desktop.0)
    }

    /// Under a test's setup that removes the win32k lockdown, the worker
    /// loads USER32, which connects it to the window station: `sid` is
    /// granted the station too, and the grant revoked when this drops.
    fn for_setup(mut self, sid: Option<&Sid>, applied: Restrictions) -> Result<Self, Failure> {
        if let Some(sid) = sid
            && !applied.has(Restrictions::NO_WIN32K)
        {
            // SAFETY: a SID the profile's calls allocated.
            let bytes = unsafe { windows::Win32::Security::GetLengthSid(sid.0) };
            let mut copy = vec![0u32; usize::try_from(bytes.div_ceil(4)).unwrap_or(0)];
            // SAFETY: a buffer of at least `bytes` bytes, aligned as a SID.
            unsafe {
                windows::Win32::Security::CopySid(bytes, PSID(copy.as_mut_ptr().cast()), sid.0)
            }
            .map_err(failed("the SID's copy"))?;
            let mut grants = STATION_GRANTS
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some((_, users)) = grants.iter_mut().find(|(s, _)| *s == copy) {
                *users += 1;
            } else {
                change_shared(
                    self.station(),
                    granted_all(PSID(copy.as_mut_ptr().cast())),
                    "the window station",
                )?;
                grants.push((copy.clone(), 1));
            }
            self.station_grant = Some(copy);
        }
        Ok(self)
    }
}

/// The station grants this process's workers hold, by SID, each with how
/// many workers use it: the first grants it and the last revokes it, so a
/// worker dropping never takes the grant from another of its AppContainer.
#[cfg(test)]
static STATION_GRANTS: Mutex<Vec<(Vec<u32>, usize)>> = Mutex::new(Vec::new());

impl Drop for Desktop {
    fn drop(&mut self) {
        #[cfg(test)]
        let failed = self.release_station();
        // SAFETY: the desktop and window station this value made, unused
        // after this.
        unsafe {
            if !self.desktop.is_invalid() {
                let _ = CloseDesktop(self.desktop);
            }
            let _ = CloseWindowStation(self.station);
        }
        // A grant left behind outlives the test: the test fails.
        #[cfg(test)]
        if let Some(message) = failed {
            if std::thread::panicking() {
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
        }
    }
}

#[cfg(test)]
impl Desktop {
    /// Gives up this worker's use of its station grant, revoked when it was
    /// the last to use it: what failed, if the revocation did.
    fn release_station(&mut self) -> Option<String> {
        let mut sid = self.station_grant.take()?;
        let mut grants = STATION_GRANTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let users = grants
            .iter_mut()
            .find(|(s, _)| *s == sid)
            .map(|(_, users)| {
                *users -= 1;
                *users
            });
        grants.retain(|(_, users)| *users > 0);
        if users.is_some_and(|n| n > 0) {
            return None;
        }
        let revoked = revoked(PSID(sid.as_mut_ptr().cast()));
        change_shared(self.station(), revoked, "the window station")
            .err()
            .map(|(code, what)| format!("revoking a test's grant of {what} failed (error {code})"))
    }
}

/// The control pipe: the game's ends and the worker's.
struct Pipes {
    /// The game writes commands here.
    commands: Owned,
    /// The game reads replies here.
    replies: Owned,
    /// The worker's standard input, inheritable.
    worker_in: Owned,
    /// The worker's standard output, inheritable.
    worker_out: Owned,
}

impl Pipes {
    fn new() -> Result<Self, Failure> {
        let inheritable = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let pipe = || -> Result<(Owned, Owned), Failure> {
            let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
            // SAFETY: a new pipe of 64 KiB, whose ends the values returned
            // own.
            unsafe {
                CreatePipe(
                    &raw mut read,
                    &raw mut write,
                    Some(&raw const inheritable),
                    1 << 16,
                )
            }
            .map_err(failed("a pipe"))?;
            Ok((Owned(read), Owned(write)))
        };
        let (worker_in, commands) = pipe()?;
        let (replies, worker_out) = pipe()?;
        for own in [&commands, &replies] {
            // SAFETY: the game's end of a pipe just made, which no child
            // inherits.
            unsafe { SetHandleInformation(own.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
                .map_err(failed("keeping the game's end"))?;
        }
        Ok(Self {
            commands,
            replies,
            worker_in,
            worker_out,
        })
    }
}

/// The policies `applied` sets at creation.
const fn mitigations(applied: Restrictions) -> u64 {
    let mut policies = 0;
    if applied.has(Restrictions::NO_DYNAMIC_CODE) {
        policies |= DYNAMIC_CODE_OFF;
    }
    if applied.has(Restrictions::NO_WIN32K) {
        policies |= WIN32K_OFF;
    }
    if applied.has(Restrictions::STRICT_HANDLES) {
        policies |= STRICT_HANDLES_ON;
    }
    if applied.has(Restrictions::IMAGE_LOAD) {
        policies |= REMOTE_IMAGES_OFF | LOW_LABEL_IMAGES_OFF | SYSTEM32_IMAGES_FIRST;
    }
    if applied.has(Restrictions::NO_EXTENSION_POINTS) {
        policies |= EXTENSION_POINTS_OFF;
    }
    policies
}

/// Appends `arg` to `line` quoted as the C runtime splits a command line.
fn quote(arg: &[u16], line: &mut Vec<u16>) {
    let (space, tab, quote_mark, backslash) = (
        u16::from(b' '),
        u16::from(b'\t'),
        u16::from(b'"'),
        u16::from(b'\\'),
    );
    if !arg.is_empty()
        && !arg
            .iter()
            .any(|&c| c == space || c == tab || c == quote_mark)
    {
        line.extend_from_slice(arg);
        return;
    }
    line.push(quote_mark);
    let mut backslashes = 0;
    for &c in arg {
        if c == backslash {
            backslashes += 1;
        } else {
            if c == quote_mark {
                line.extend(std::iter::repeat_n(backslash, backslashes + 1));
            }
            backslashes = 0;
        }
        line.push(c);
    }
    line.extend(std::iter::repeat_n(backslash, backslashes));
    line.push(quote_mark);
}

/// The process made suspended, terminated when dropped unless released.
struct Child {
    process: Owned,
    thread: Owned,
    pid: u32,
    armed: bool,
}

impl Child {
    /// Resumes its thread, which must have been suspended `times` times: with
    /// more it would stay suspended.
    fn resume(&self, times: u32) -> Result<(), Failure> {
        // SAFETY: the thread this launch made suspended.
        let previous = unsafe { ResumeThread(self.thread.0) };
        if previous == u32::MAX {
            return Err((last_error(), "resuming it".to_owned()));
        }
        if previous != times {
            return Err((
                0,
                format!("its thread was suspended {previous} times, not {times}"),
            ));
        }
        Ok(())
    }

    /// Terminates it for `(step, failure)`, waiting until it has ended.
    fn refuse(mut self, (step, (code, what)): (Step, Failure)) -> LaunchError {
        self.armed = false;
        LaunchError {
            step,
            code,
            what,
            process: terminate(self.process.0),
        }
    }

    /// The process, kept running, and its id.
    fn release(mut self) -> (Owned, u32) {
        self.armed = false;
        let process = std::mem::replace(&mut self.process, Owned(HANDLE::default()));
        (process, self.pid)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.armed {
            let _ = terminate(self.process.0);
        }
    }
}

/// Terminates `process` and waits up to 5 s until it has ended: whether it
/// ended, and with what.
fn terminate(process: HANDLE) -> Made {
    // SAFETY: a process this launch made.
    let _ = unsafe { TerminateProcess(process, REFUSED) };
    // SAFETY: as above.
    if unsafe { WaitForSingleObject(process, 5_000) } != WAIT_OBJECT_0 {
        return Made::Running;
    }
    let mut exit = 0;
    // SAFETY: as above.
    let read = unsafe { GetExitCodeProcess(process, &raw mut exit) };
    Made::Ended(read.is_ok().then_some(exit))
}

/// The attribute list's storage, deleted when dropped.
struct AttributeList {
    _storage: Vec<u64>,
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: a list `InitializeProcThreadAttributeList` set up.
        unsafe { DeleteProcThreadAttributeList(self.list) };
    }
}

/// Steps 4 and 5: the creation attributes and the suspended process.
fn create(
    dir: &Path,
    setup: &Setup,
    sid: Option<&Sid>,
    pipes: &Pipes,
    desktop: Option<&Desktop>,
) -> Result<Child, (Step, Failure)> {
    let applied = setup.applied;
    // Every value an attribute points to lives until the process is made.
    let handle_list = [pipes.worker_in.0, pipes.worker_out.0];
    let child_policy: u32 = PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;
    let policies = mitigations(applied);
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: sid.map_or_else(PSID::default, |s| s.0),
        Capabilities: std::ptr::null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let mut attributes: Vec<(u32, *const c_void, usize)> = Vec::new();
    if applied.has(Restrictions::HANDLE_LIST) {
        attributes.push((
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            handle_list.as_ptr().cast(),
            size_of_val(&handle_list),
        ));
    }
    if applied.has(Restrictions::NO_CHILD) {
        attributes.push((
            PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY,
            (&raw const child_policy).cast(),
            size_of_val(&child_policy),
        ));
    }
    if policies != 0 {
        attributes.push((
            PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
            (&raw const policies).cast(),
            size_of_val(&policies),
        ));
    }
    if sid.is_some() {
        attributes.push((
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            (&raw const capabilities).cast(),
            size_of_val(&capabilities),
        ));
    }
    let list = run(setup, Step::Attributes, || {
        if attributes.is_empty() {
            return Ok(None);
        }
        let count = u32::try_from(attributes.len()).unwrap_or(u32::MAX);
        let mut bytes = 0;
        // SAFETY: asks the list's size, which the call fails to fit.
        let _ = unsafe { InitializeProcThreadAttributeList(None, count, None, &raw mut bytes) };
        let mut storage = vec![0u64; bytes.div_ceil(8)];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
        // SAFETY: `storage` holds `bytes` bytes, aligned for the list.
        unsafe { InitializeProcThreadAttributeList(Some(list), count, None, &raw mut bytes) }
            .map_err(failed("the attribute list"))?;
        let list = AttributeList {
            _storage: storage,
            list,
        };
        for &(attribute, value, size) in &attributes {
            // SAFETY: each value lives until the process is made, with the
            // size its attribute takes.
            unsafe {
                UpdateProcThreadAttribute(
                    list.list,
                    0,
                    attribute as usize,
                    Some(value),
                    size,
                    None,
                    None,
                )
            }
            .map_err(failed("a creation attribute"))?;
        }
        Ok(Some(list))
    })?;
    run(setup, Step::Create, || {
        let exe = dir.join(EXECUTABLE);
        let app = wide(&exe);
        let mut line = Vec::new();
        quote(
            app.split_last().map_or(&[][..], |(_, path)| path),
            &mut line,
        );
        for arg in setup.args() {
            line.push(u16::from(b' '));
            quote(&arg.encode_utf16().collect::<Vec<_>>(), &mut line);
        }
        line.push(0);
        let cwd = wide(dir);
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = u32::try_from(if list.is_some() {
            size_of::<STARTUPINFOEXW>()
        } else {
            size_of::<STARTUPINFOW>()
        })
        .unwrap_or(0);
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = pipes.worker_in.0;
        startup.StartupInfo.hStdOutput = pipes.worker_out.0;
        if let Some(desktop) = desktop {
            startup.StartupInfo.lpDesktop = PWSTR(desktop.path.as_ptr().cast_mut());
        }
        let environment = environment(sid)?;
        let mut flags: PROCESS_CREATION_FLAGS = CREATE_SUSPENDED
            | DETACHED_PROCESS
            | BELOW_NORMAL_PRIORITY_CLASS
            | CREATE_UNICODE_ENVIRONMENT;
        if let Some(list) = &list {
            startup.lpAttributeList = list.list;
            flags |= EXTENDED_STARTUPINFO_PRESENT;
        }
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: nul-terminated paths, a writable command line and an
        // environment block ending in two nuls; the startup information and
        // its attributes live through the call; the handles returned are
        // owned by the `Child`.
        unsafe {
            CreateProcessW(
                PCWSTR(app.as_ptr()),
                Some(PWSTR(line.as_mut_ptr())),
                None,
                None,
                true,
                flags,
                Some(environment.as_ptr().cast()),
                PCWSTR(cwd.as_ptr()),
                &raw const startup.StartupInfo,
                &raw mut info,
            )
        }
        .map_err(failed("creating the process"))?;
        Ok(Child {
            process: Owned(info.hProcess),
            thread: Owned(info.hThread),
            pid: info.dwProcessId,
            armed: true,
        })
    })
}

/// The worker's environment, a block the launch builds. Nothing of the
/// game's environment reaches the worker: its variables name the user's
/// folders and can hold credentials, and the worker needs none of them.
/// The block holds, sorted by name as Windows keeps one:
/// - in an AppContainer (`sid`), `LOCALAPPDATA` and the folder that holds
///   the AppContainer's (`<it>\Packages\<name>\AC`): Windows refuses to
///   create an AppContainer's process whose environment lacks the variable
///   (`ERROR_ENVVAR_NOT_FOUND`), and from it sets the worker's
///   `LOCALAPPDATA` to the AppContainer's folder and its `TEMP` and `TMP`
///   to that folder's `Temp`;
/// - `SystemRoot` and the Windows directory, which Windows expects of every
///   process (system DLLs and the side-by-side loader expand it).
fn environment(sid: Option<&Sid>) -> Result<Vec<u16>, Failure> {
    use std::os::windows::ffi::OsStringExt as _;
    let mut block = Vec::new();
    if let Some(sid) = sid {
        let Some(folder) = folder(sid)? else {
            return Err((0, "the profile's folder is missing".to_owned()));
        };
        let folder = std::path::PathBuf::from(std::ffi::OsString::from_wide(&folder));
        let named = |path: &Path, name: &str| {
            path.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        };
        let packages = folder.parent().and_then(Path::parent);
        let Some(root) = packages
            .filter(|p| named(&folder, "AC") && named(p, "Packages"))
            .and_then(Path::parent)
        else {
            return Err((
                0,
                format!(
                    "the profile's folder is not under Packages: {}",
                    folder.display()
                ),
            ));
        };
        block.extend("LOCALAPPDATA=".encode_utf16());
        block.extend(root.as_os_str().encode_wide());
        block.push(0);
    }
    let mut dir = [0u16; 261];
    // SAFETY: a buffer of the size given.
    let n = usize::try_from(unsafe { GetSystemWindowsDirectoryW(Some(&mut dir)) }).unwrap_or(0);
    let Some(dir) = dir.get(..n).filter(|_| n > 0 && n < 261) else {
        return Err((last_error(), "the Windows directory".to_owned()));
    };
    block.extend("SystemRoot=".encode_utf16());
    block.extend_from_slice(dir);
    block.extend([0, 0]);
    Ok(block)
}

/// Step 6: checks the suspended process against `expected`: the handles
/// it holds.
fn check(
    child: &Child,
    job: Option<&Owned>,
    sid: Option<&Sid>,
    pipe_ends: [u64; 2],
    expected: Restrictions,
) -> Result<Vec<handles::Entry>, Failure> {
    // Still suspended: suspending its thread finds it suspended once, by
    // its creation.
    // SAFETY: the thread this launch made.
    let previous = unsafe { SuspendThread(child.thread.0) };
    if previous == u32::MAX {
        return Err((last_error(), "suspending its thread".to_owned()));
    }
    // Refused while still suspended: the launch terminates it as it is.
    if previous != 1 {
        return Err((
            0,
            format!("its thread was resumed ({previous} suspensions)"),
        ));
    }
    child.resume(2)?;
    if expected.has(Restrictions::JOB) {
        let Some(job) = job else {
            return Err((0, "no job".to_owned()));
        };
        let mut inside = windows::core::BOOL(0);
        // SAFETY: the process and the job this launch made.
        unsafe { IsProcessInJob(child.process.0, Some(job.0), &raw mut inside) }
            .map_err(failed("its job"))?;
        if !inside.as_bool() {
            return Err((0, "not in its job".to_owned()));
        }
        job_holds(job, expected)?;
    }
    policies(child.process.0, expected)?;
    if expected.has(Restrictions::APP_CONTAINER) {
        let Some(sid) = sid else {
            return Err((0, "no AppContainer".to_owned()));
        };
        token(child.process.0, sid)?;
    }
    let table = handles::table(child.process.0)
        .map_err(|s| (s.0.cast_unsigned(), "its handle table".to_owned()))?;
    if expected.has(Restrictions::HANDLE_LIST) {
        let mut held: Vec<u64> = table.iter().map(|e| e.value).collect();
        held.sort_unstable();
        let mut listed = pipe_ends.to_vec();
        listed.sort_unstable();
        if held != listed {
            return Err((
                0,
                format!("it holds handles {held:#x?}, not only the pipe's {listed:#x?}"),
            ));
        }
    }
    Ok(table)
}

/// Checks that `job`'s limits and UI restrictions read back exactly as
/// `expected` sets them: the memory cap, the flags (no breakaway among
/// them) and the process limit.
fn job_holds(job: &Owned, expected: Restrictions) -> Result<(), Failure> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: the job this launch made, and a struct of the size given.
    unsafe {
        QueryInformationJobObject(
            Some(job.0),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            u32::try_from(size_of_val(&limits)).unwrap_or(0),
            None,
        )
    }
    .map_err(failed("the job's limits"))?;
    let basic = limits.BasicLimitInformation;
    let one = expected.has(Restrictions::ONE_PROCESS);
    if basic.LimitFlags != job_flags(expected)
        || limits.ProcessMemoryLimit != MEMORY_CAP
        || (one && basic.ActiveProcessLimit != 1)
    {
        return Err((
            0,
            format!(
                "its job's limits are {:#x}, {} bytes, {} processes",
                basic.LimitFlags.0, limits.ProcessMemoryLimit, basic.ActiveProcessLimit
            ),
        ));
    }
    let mut ui = JOBOBJECT_BASIC_UI_RESTRICTIONS::default();
    // SAFETY: as above.
    unsafe {
        QueryInformationJobObject(
            Some(job.0),
            JobObjectBasicUIRestrictions,
            (&raw mut ui).cast(),
            u32::try_from(size_of_val(&ui)).unwrap_or(0),
            None,
        )
    }
    .map_err(failed("the job's UI restrictions"))?;
    if ui.UIRestrictionsClass != JOB_UI {
        return Err((
            0,
            format!(
                "its job's UI restrictions are {:#x}",
                ui.UIRestrictionsClass.0
            ),
        ));
    }
    Ok(())
}

/// Checks the mitigation policies `expected` sets on `process`.
fn policies(process: HANDLE, expected: Restrictions) -> Result<(), Failure> {
    let wanted: [(Restrictions, PROCESS_MITIGATION_POLICY, u32, &str); 6] = [
        // `ProhibitDynamicCode`.
        (
            Restrictions::NO_DYNAMIC_CODE,
            ProcessDynamicCodePolicy,
            0b1,
            "dynamic code",
        ),
        // `DisallowWin32kSystemCalls`.
        (
            Restrictions::NO_WIN32K,
            ProcessSystemCallDisablePolicy,
            0b1,
            "win32k system calls",
        ),
        // `RaiseExceptionOnInvalidHandleReference`,
        // `HandleExceptionsPermanentlyEnabled`.
        (
            Restrictions::STRICT_HANDLES,
            ProcessStrictHandleCheckPolicy,
            0b11,
            "strict handle checks",
        ),
        // `NoRemoteImages`, `NoLowMandatoryLabelImages`,
        // `PreferSystem32Images`.
        (
            Restrictions::IMAGE_LOAD,
            ProcessImageLoadPolicy,
            0b111,
            "image loads",
        ),
        // `DisableExtensionPoints`.
        (
            Restrictions::NO_EXTENSION_POINTS,
            ProcessExtensionPointDisablePolicy,
            0b1,
            "extension points",
        ),
        // `NoChildProcessCreation`.
        (
            Restrictions::NO_CHILD,
            ProcessChildProcessPolicy,
            0b1,
            "child processes",
        ),
    ];
    for (restriction, policy, bits, what) in wanted {
        if !expected.has(restriction) {
            continue;
        }
        let mut flags = 0u32;
        // SAFETY: each of these policies is one 32-bit word of flags.
        unsafe {
            GetProcessMitigationPolicy(
                process,
                policy,
                (&raw mut flags).cast(),
                size_of_val(&flags),
            )
        }
        .map_err(failed(what))?;
        if flags & bits != bits {
            return Err((0, format!("{what}: its policy's flags are {flags:#x}")));
        }
    }
    Ok(())
}

/// Reads `class` of `token` into an aligned buffer.
fn token_information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u64>, Failure> {
    let mut bytes = 0;
    // SAFETY: asks the information's size.
    let _ = unsafe { GetTokenInformation(token, class, None, 0, &raw mut bytes) };
    let mut buffer = vec![0u64; usize::try_from(bytes).unwrap_or(0).div_ceil(8).max(1)];
    let size = u32::try_from(buffer.len() * 8).unwrap_or(u32::MAX);
    // SAFETY: `buffer` holds `size` bytes, aligned for the information.
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &raw mut bytes,
        )
    }
    .map_err(failed("its token"))?;
    Ok(buffer)
}

/// Checks `process`'s token: `sid`'s AppContainer, with no capability, at
/// low integrity.
fn token(process: HANDLE, sid: &Sid) -> Result<(), Failure> {
    let mut token = HANDLE::default();
    // SAFETY: the process this launch made; the token is closed by `Owned`.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }
        .map_err(failed("its token"))?;
    let token = Owned(token);
    let is_container = token_information(token.0, TokenIsAppContainer)?;
    if is_container.first().copied().unwrap_or(0) & 0xffff_ffff == 0 {
        return Err((0, "its token is no AppContainer's".to_owned()));
    }
    let container = token_information(token.0, TokenAppContainerSid)?;
    // SAFETY: the buffer holds a `TOKEN_APPCONTAINER_INFORMATION`.
    let container = unsafe {
        container
            .as_ptr()
            .cast::<TOKEN_APPCONTAINER_INFORMATION>()
            .read()
    };
    // SAFETY: two valid SIDs.
    if unsafe { EqualSid(container.TokenAppContainer, sid.0) }.is_err() {
        return Err((0, "its token is another AppContainer's".to_owned()));
    }
    let capabilities = token_information(token.0, TokenCapabilities)?;
    // SAFETY: the buffer holds a `TOKEN_GROUPS`.
    let count =
        unsafe { (&raw const (*capabilities.as_ptr().cast::<TOKEN_GROUPS>()).GroupCount).read() };
    if count != 0 {
        return Err((0, format!("its token holds {count} capabilities")));
    }
    let label = token_information(token.0, TokenIntegrityLevel)?;
    // SAFETY: the buffer holds a `TOKEN_MANDATORY_LABEL`, whose SID it
    // holds too; its last sub-authority is the integrity level.
    let level = unsafe {
        let label = label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>().read();
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        *GetSidSubAuthority(label.Label.Sid, u32::from(count).saturating_sub(1))
    };
    if level != SECURITY_MANDATORY_LOW_RID.cast_unsigned() {
        return Err((0, format!("its token's integrity is {level:#x}")));
    }
    Ok(())
}

/// Reads the worker's replies from `replies` until the pipe ends, a reply
/// does not decode or the game holds [`REPLIES`] unread: log lines to the
/// game's log, the rest to `tx`. One reply past the queue's capacity marks
/// the worker `overflowed` and ends the reading, which closes the pipe: the
/// worker's next reply fails and ends it. The game's thread never waits on
/// the queue, and takes each reply without allocating.
fn read_replies(
    replies: Owned,
    tx: &SyncSender<Reply>,
    overflowed: &AtomicBool,
    #[cfg(test)] log: &std::sync::Arc<Mutex<String>>,
) {
    use std::io::Read as _;
    // SAFETY: the game's end of the pipe from the worker, owned by the file
    // from here.
    let mut pipe = unsafe { std::fs::File::from_raw_handle(replies.release().0) };
    let mut lines = 0;
    loop {
        let mut prefix = [0; 2];
        if pipe.read_exact(&mut prefix).is_err() {
            return;
        }
        let len = match control::frame_len(prefix) {
            Ok(len) => len,
            Err(e) => {
                log::warn!("media worker: a reply frame the protocol does not have ({e})");
                return;
            }
        };
        let mut message = vec![0; len];
        if pipe.read_exact(&mut message).is_err() {
            return;
        }
        match Reply::decode(&message) {
            Ok(Reply::Log { text }) => {
                lines += 1;
                if lines > MAX_LOG_LINES {
                    continue;
                }
                let text = if lines == MAX_LOG_LINES {
                    "its further log lines are dropped".to_owned()
                } else {
                    text
                };
                log::info!("media worker: {text}");
                #[cfg(test)]
                {
                    let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
                    log.push_str(&text);
                    log.push('\n');
                }
            }
            Ok(reply) => match tx.try_send(reply) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    overflowed.store(true, Ordering::Release);
                    log::warn!("media worker: more than {REPLIES} replies unread; its replies end");
                    return;
                }
                Err(TrySendError::Disconnected(_)) => return,
            },
            Err(e) => {
                log::warn!("media worker: a reply that does not decode ({e})");
                return;
            }
        }
    }
}

/// The handshake: the first reply other than a log line is the hello,
/// within the deadline.
fn handshake(replies: &Receiver<Reply>, limit: Duration) -> Result<Hello, Failure> {
    match replies.recv_timeout(limit) {
        Ok(Reply::Hello {
            version,
            configuration,
        }) => Ok(Hello {
            version,
            configuration,
        }),
        Ok(other) => Err((0, format!("it answered {other:?} before its hello"))),
        Err(RecvTimeoutError::Timeout) => Err((ERROR_TIMEOUT.0, "no hello in time".to_owned())),
        Err(RecvTimeoutError::Disconnected) => Err((0, "it ended before its hello".to_owned())),
    }
}
