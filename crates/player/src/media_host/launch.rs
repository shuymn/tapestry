//! The worker's launch (design.md "Threat Model" T02), in the order the
//! sandbox needs:
//!
//! 1. a job: its committed memory capped, the worker ended when the job
//!    closes and on an unhandled exception, one process, its UI
//!    restricted, and a completion port for its messages ([`watch::port`]);
//! 2. the worker's AppContainer profile, with no capability, the user's
//!    one `tapestry.media` ([`PROFILE`]), and the worker's copies
//!    ([`stage`](super::stage)): its executable and the build's DLLs,
//!    copied byte for byte into a directory of that build's own under the
//!    game's data folder, granted to the AppContainer, checked against the
//!    installation's files and held until the worker has loaded them;
//!    nothing is granted on the installation directory;
//! 3. a desktop of its own, granted to the AppContainer, in the session's
//!    alternate window station (`CreateWindowStationW` with no name),
//!    which other launches share and the launch leaves as it is;
//! 4. the creation attributes: the control pipe's two ends as the only
//!    handles it inherits, no child process, the mitigation policies (no
//!    dynamic code, no win32k system calls, strict handle checks, images
//!    only from local files without a low label and from System32 first,
//!    no extension points) and the AppContainer;
//! 5. the process created suspended, detached and below normal priority,
//!    from the copies, with an environment the launch builds
//!    ([`environment`]), then put in the job;
//! 6. the check before it runs: it is still suspended and in the job, the
//!    job's limits and UI restrictions read back exactly as set, the
//!    queried mitigation policies hold, its token is the AppContainer's
//!    with no capability at low integrity, and it holds exactly the pipe's
//!    ends; then its watcher starts ([`watch`]), and it resumes and must
//!    answer with its hello within the handshake deadline.
//!
//! A step that fails, or a check that finds anything else, terminates the
//! process, if one was made, and fails the launch: never a weaker sandbox.
//! The launch watches the process end for 5 s: one that has not ended by
//! then, or whose end cannot be asked, stays with the error
//! ([`LaunchError::unended`]), as does one Windows refused to terminate.
//! The desktop is made as browsers' sandboxes make an alternate desktop for
//! their sandboxed processes: the process's window station is switched to
//! the alternate one while the desktop is created in it, under a lock, and
//! switched back.
//!
//! A test's setup ([`Setup`], only under `cfg(test)`) removes restrictions,
//! changes what the check expects, makes a step fail, passes the worker
//! arguments, names its AppContainer profile, or watches a terminated
//! process for another time; a build of the game holds the recipe alone. A
//! setup without the win32k lockdown, whose worker connects to the window
//! station when it loads USER32, grants the AppContainer the station too,
//! until the worker drops.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use skin_model::media::control::{self, Reply};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_REVISION_MISMATCH, ERROR_TIMEOUT,
    GENERIC_ALL, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, HLOCAL, LocalFree,
    SetHandleInformation, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetSecurityInfo, SDDL_REVISION_1, SE_OBJECT_TYPE,
    SE_WINDOW_OBJECT, SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName, GetAppContainerFolderPath,
};
use windows::Win32::Security::{
    ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, EqualSid, FreeSid, GetSidSubAuthority,
    GetSidSubAuthorityCount, GetTokenInformation, NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, TOKEN_APPCONTAINER_INFORMATION, TOKEN_GROUPS,
    TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, TokenAppContainerSid,
    TokenCapabilities, TokenIntegrityLevel, TokenIsAppContainer, TokenUser,
};
use windows::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_AFFINITY,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_UILIMIT, JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
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
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateMutexW, CreateProcessW, DETACHED_PROCESS,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    GetExitCodeProcess, GetProcessMitigationPolicy, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, NORMAL_PRIORITY_CLASS, OpenProcessToken,
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
use super::{Hello, Owned, Worker, handles, placement};

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

/// The worker's AppContainer profile, the user's one for every launch: what
/// it is granted (the copies, its desktop) holds nothing of an
/// installation.
pub const PROFILE: &str = "tapestry.media";

/// How long a launch watches a process it terminated end.
const OBSERVE: Duration = Duration::from_secs(5);

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

#[cfg(any(test, feature = "dev-tools"))]
impl Restrictions {
    /// `self` without `r`.
    pub const fn without(self, r: Self) -> Self {
        Self(self.0 & !r.0)
    }
}

#[cfg(test)]
impl Restrictions {
    /// No restriction.
    pub const NONE: Self = Self(0);

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
    Stage,
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
        Self::Stage,
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
    /// The process, when the launch made one and did not see it end: to
    /// wait on, or terminate again. Dropping it closes the handle alone.
    pub unended: Option<OwnedHandle>,
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
    /// stopped watching (5 s on, as the game launches it).
    Pending,
    /// Windows refused to terminate it (the error), and it had not ended.
    NotTerminated(u32),
    /// Windows did not say whether it ended (the error of the wait).
    Unknown(u32),
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
            Made::Pending => write!(
                f,
                "; it was terminated and had not ended when the launch stopped watching"
            ),
            Made::NotTerminated(code) => {
                write!(
                    f,
                    "; terminating it failed (error {code}), and it had not ended"
                )
            }
            Made::Unknown(code) => write!(f, "; whether it ended is unknown (error {code})"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// A step's failure: the system's error and what failed.
pub(super) type Failure = (u32, String);

/// What a launch applies and checks: the recipe, or a test's variation.
#[derive(Clone, Debug)]
pub struct Setup {
    applied: Restrictions,
    expected: Restrictions,
    #[cfg(test)]
    fail_at: Option<Step>,
    #[cfg(any(test, feature = "dev-tools"))]
    args: Vec<String>,
    #[cfg(test)]
    handshake: Duration,
    #[cfg(test)]
    watching: watch::Variation,
    profile: Option<String>,
    #[cfg(test)]
    observe: Duration,
    #[cfg(test)]
    priority: PROCESS_CREATION_FLAGS,
}

impl Setup {
    /// The recipe: every restriction applied and checked.
    pub const fn recipe() -> Self {
        Self {
            applied: Restrictions::RECIPE,
            expected: Restrictions::RECIPE,
            #[cfg(test)]
            fail_at: None,
            #[cfg(any(test, feature = "dev-tools"))]
            args: Vec::new(),
            #[cfg(test)]
            handshake: HANDSHAKE,
            #[cfg(test)]
            watching: watch::Variation {
                no_message: false,
                no_accounting: false,
                discard_message: false,
                period: None,
                failed_terminations: 0,
                before_terminate: None,
                failed_stop_post: false,
            },
            profile: None,
            #[cfg(test)]
            observe: OBSERVE,
            #[cfg(test)]
            priority: PRIORITY,
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
    #[cfg(any(test, feature = "dev-tools"))]
    fn args(&self) -> &[String] {
        &self.args
    }

    #[cfg(not(any(test, feature = "dev-tools")))]
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

    /// The worker's AppContainer profile.
    #[cfg(test)]
    fn profile(&self) -> &str {
        self.profile.as_deref().unwrap_or(PROFILE)
    }

    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "a test's setup names its own profile; the recipe's is `PROFILE`"
    )]
    const fn profile(&self) -> &str {
        PROFILE
    }

    /// How long a terminated process is watched end.
    #[cfg(test)]
    const fn observe(&self) -> Duration {
        self.observe
    }

    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "a test's setup may watch for another time; the recipe watches `OBSERVE`"
    )]
    const fn observe(&self) -> Duration {
        OBSERVE
    }

    /// The worker's priority class.
    #[cfg(test)]
    const fn priority(&self) -> PROCESS_CREATION_FLAGS {
        self.priority
    }

    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "a test's setup may measure another priority; the recipe's is `PRIORITY`"
    )]
    const fn priority(&self) -> PROCESS_CREATION_FLAGS {
        PRIORITY
    }
}

/// The worker's priority class: normal, the game threads' own. Below
/// normal, other programs filling the CPUs starve it (launches, opens and
/// frames late by seconds); its job's CPUs bound what it takes from the
/// game ([`worker_cpus`]).
const PRIORITY: PROCESS_CREATION_FLAGS = NORMAL_PRIORITY_CLASS;

#[cfg(any(test, feature = "dev-tools"))]
impl Setup {
    /// Passing the worker `args`.
    pub fn args_of(mut self, args: &[&str]) -> Self {
        self.args = args.iter().map(|&a| a.to_owned()).collect();
        self
    }

    /// Checking exactly `r`.
    pub const fn expecting(mut self, r: Restrictions) -> Self {
        self.expected = r;
        self
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

    /// With `step` failing as if its call had.
    pub const fn failing_at(mut self, step: Step) -> Self {
        self.fail_at = Some(step);
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

    /// In the AppContainer profile `name`, a test's own, which the test
    /// deletes.
    pub fn in_profile(mut self, name: &str) -> Self {
        self.profile = Some(name.to_owned());
        self
    }

    /// Watching a terminated process end for `limit`.
    pub const fn observing(mut self, limit: Duration) -> Self {
        self.observe = limit;
        self
    }

    /// Launching the worker below normal priority rather than the
    /// recipe's (a measurement's comparison).
    pub const fn at_below_normal_priority(mut self) -> Self {
        self.priority = windows::Win32::System::Threading::BELOW_NORMAL_PRIORITY_CLASS;
        self
    }
}

/// The calling thread's last error.
fn last_error() -> u32 {
    // SAFETY: a plain query.
    unsafe { GetLastError() }.0
}

/// The system's error a failed call returned.
pub(super) fn code_of(e: &windows::core::Error) -> u32 {
    let code = e.code().0.cast_unsigned();
    // A Win32 error wrapped as an HRESULT.
    if code & 0xffff_0000 == 0x8007_0000 {
        code & 0xffff
    } else {
        code
    }
}

/// `what` failed with `e`.
pub(super) fn failed(what: &str) -> impl FnOnce(windows::core::Error) -> Failure + '_ {
    move |e| (code_of(&e), what.to_owned())
}

/// `path`, nul-terminated.
pub(super) fn wide(path: &Path) -> Vec<u16> {
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
        unended: None,
    }
}

/// Launches the worker installed in `installation` under `setup`, from
/// copies of its files staged in the game's data folder `data`.
pub fn launch(installation: &Path, data: &Path, setup: &Setup) -> Result<Worker, LaunchError> {
    let applied = setup.applied;
    let (job, port, placement) = if applied.has(Restrictions::JOB) {
        let (job, port, placement) = run(setup, Step::Job, || job(applied)).map_err(early)?;
        (Some(job), Some(port), Some(placement))
    } else {
        (None, None, None)
    };
    let profiled = if applied.has(Restrictions::APP_CONTAINER) {
        Some(run(setup, Step::Profile, || profile(setup.profile())).map_err(early)?)
    } else {
        None
    };
    let sid = profiled.as_ref();
    // The copies are granted to the profile's AppContainer whether or not
    // this launch runs in it: a test's setup without one stages the same.
    let staged = run(setup, Step::Stage, || {
        let derived;
        let container = match sid {
            Some(sid) => sid,
            None => {
                derived = derive(setup.profile())?;
                &derived
            }
        };
        let _one_at_a_time = Launches::hold()?;
        super::stage::stage(installation, data, container.0)
    })
    .map_err(early)?;
    let dir = staged.dir();
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
            check(
                &child,
                job.as_ref(),
                placement.as_ref(),
                sid,
                pipe_ends,
                setup.expected,
                setup.priority(),
            )
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
        Err(e) => return Err(child.refuse(e, setup.observe())),
    };
    #[cfg(test)]
    let log = std::sync::Arc::new(Mutex::new(String::new()));
    let (tx, rx) = mpsc::sync_channel(REPLIES);
    let overflowed = Arc::new(AtomicBool::new(false));
    let in_hand = Arc::new(AtomicBool::new(false));
    let beats = Arc::new(super::Beats::new());
    #[cfg(test)]
    let closes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reading = {
        #[cfg(test)]
        let log = std::sync::Arc::clone(&log);
        #[cfg(test)]
        let closes = Arc::clone(&closes);
        let overflowed = Arc::clone(&overflowed);
        let in_hand = Arc::clone(&in_hand);
        let beats = Arc::clone(&beats);
        std::thread::Builder::new()
            .name("media worker replies".to_owned())
            .spawn(move || {
                let end = read_replies(
                    replies,
                    &tx,
                    &overflowed,
                    &in_hand,
                    &beats,
                    #[cfg(test)]
                    (&log, &closes),
                );
                // The replies' end is published before its line is logged:
                // a log that stalls never delays it.
                drop(tx);
                if let Some(line) = end {
                    log::warn!("media worker: {line}");
                }
            })
    };
    if let Err(e) = reading {
        let code = e.raw_os_error().map_or(0, i32::cast_unsigned);
        return Err(child.refuse(
            (Step::Handshake, (code, "its replies' thread".to_owned())),
            setup.observe(),
        ));
    }
    let hello = match run(setup, Step::Handshake, || handshake(&rx, setup.handshake())) {
        Ok(hello) => hello,
        Err(e) => return Err(child.refuse(e, setup.observe())),
    };
    // SAFETY: the game's end of the pipe to the worker, owned by the file
    // from here.
    let commands = unsafe { std::fs::File::from_raw_handle(commands.release().0) };
    let commands = match super::writer::Writer::start(commands) {
        Ok(writer) => writer,
        Err(e) => {
            let code = e.raw_os_error().map_or(0, i32::cast_unsigned);
            return Err(child.refuse(
                (Step::Handshake, (code, "its commands' thread".to_owned())),
                setup.observe(),
            ));
        }
    };
    // The worker has loaded its copies, which its images now hold.
    drop(staged);
    let (process, pid) = child.release();
    #[cfg(not(test))]
    let _ = table;
    Ok(Worker {
        commands: Some(commands),
        replies: rx,
        overflowed,
        in_hand,
        beats,
        hello,
        process,
        pid,
        job,
        watch,
        players: std::cell::RefCell::new(super::players::Players::default()),
        hung: std::cell::Cell::new(false),
        next_id: 1,
        _desktop: desktop,
        #[cfg(test)]
        log,
        #[cfg(test)]
        closes,
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
    if !many_groups() {
        flags |= JOB_OBJECT_LIMIT_AFFINITY;
    }
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

/// The game's primary processor group and its affinity there: the
/// calling thread's, which a thread the game makes inherits.
fn primary() -> Result<(u16, usize), Failure> {
    let mut affinity = windows::Win32::System::SystemInformation::GROUP_AFFINITY::default();
    // SAFETY: this thread's pseudo handle and an out value.
    unsafe {
        windows::Win32::System::Threading::GetThreadGroupAffinity(
            windows::Win32::System::Threading::GetCurrentThread(),
            &raw mut affinity,
        )
    }
    .ok()
    .map_err(failed("the game's processor group"))?;
    Ok((affinity.Group, affinity.Mask))
}

/// The system's logical processors, from its CPU sets, as this process
/// sees them.
fn cpu_sets() -> Result<Vec<placement::Cpu>, Failure> {
    use windows::Win32::System::SystemInformation::{
        CpuSetInformation, GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
    };
    // SAFETY: this process's pseudo handle.
    let process = Some(unsafe { GetCurrentProcess() });
    let mut len = 0_u32;
    // SAFETY: a query of the size alone, which fails with it set.
    let _ = unsafe { GetSystemCpuSetInformation(None, 0, &raw mut len, process, None) };
    // Eight-byte words, so the entries are aligned.
    let mut words = vec![0_u64; (len as usize).div_ceil(8)];
    // SAFETY: a buffer of at least `len` bytes, the size just given.
    unsafe {
        GetSystemCpuSetInformation(
            Some(words.as_mut_ptr().cast()),
            len,
            &raw mut len,
            process,
            None,
        )
    }
    .ok()
    .map_err(failed("the system's CPU sets"))?;
    let bytes = (len as usize).min(words.len() * 8);
    let base: *const u8 = words.as_ptr().cast();
    let mut cpus = Vec::new();
    let mut at = 0;
    while at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes {
        // SAFETY: an entry the call wrote within the buffer, read whole.
        let entry = unsafe {
            base.add(at)
                .cast::<SYSTEM_CPU_SET_INFORMATION>()
                .read_unaligned()
        };
        if entry.Size == 0 {
            break;
        }
        if entry.Type == CpuSetInformation {
            // SAFETY: an entry of the CPU-set type holds a CPU set.
            let set = unsafe { entry.Anonymous.CpuSet };
            // SAFETY: the flags' byte, every pattern of which is valid.
            let flags = unsafe { set.Anonymous1.AllFlags };
            // Allocated (2) to another process than this one (4).
            let free = flags & 2 == 0 || flags & 4 != 0;
            cpus.push(placement::Cpu {
                id: set.Id,
                group: set.Group,
                index: set.LogicalProcessorIndex,
                core: set.CoreIndex,
                efficiency: set.EfficiencyClass,
                free,
            });
        }
        at += entry.Size as usize;
    }
    Ok(cpus)
}

/// Where the worker runs ([`placement`]): half the whole cores of the
/// game's primary group, its job's bound. A failure when the game may run
/// on fewer than two whole cores, so no subset leaves it one: no worker runs
/// and its videos are lost.
///
/// # Errors
///
/// When the topology cannot be read, or no subset leaves the game a core.
pub fn placement() -> Result<placement::Placement, Failure> {
    let (group, allowed) = primary()?;
    let cpus = cpu_sets()?;
    placement::split(&cpus, group, allowed).ok_or_else(|| {
        (
            0,
            format!("no whole core is left for the game (group {group}, CPUs {allowed:#x})"),
        )
    })
}

/// Whether the system has more than one processor group: the worker's
/// group is then set apart from its CPUs.
fn many_groups() -> bool {
    // SAFETY: a plain query.
    (unsafe { windows::Win32::System::Threading::GetActiveProcessorGroupCount() }) > 1
}

/// Checks that the worker's process runs in `placement`'s group alone, on
/// its CPUs alone.
fn placed(process: HANDLE, placement: &placement::Placement) -> Result<(), Failure> {
    use windows::Win32::System::Threading::{GetProcessAffinityMask, GetProcessGroupAffinity};
    let mut groups = [0_u16; 4];
    let mut count = u16::try_from(groups.len()).unwrap_or(0);
    // SAFETY: the worker's process handle and a buffer of `count` groups.
    let read = unsafe { GetProcessGroupAffinity(process, &raw mut count, groups.as_mut_ptr()) };
    let groups = groups.get(..usize::from(count)).unwrap_or(&[]);
    if !read.as_bool() || groups != [placement.group] {
        return Err((
            0,
            format!("it runs in groups {groups:?}, not {}", placement.group),
        ));
    }
    let (mut mask, mut system) = (0, 0);
    // SAFETY: the worker's process handle and two out values.
    unsafe { GetProcessAffinityMask(process, &raw mut mask, &raw mut system) }
        .map_err(failed("its CPUs"))?;
    if mask != placement.worker {
        return Err((
            0,
            format!("it runs on CPUs {mask:#x}, not {:#x}", placement.worker),
        ));
    }
    Ok(())
}

/// Step 1: where the worker runs ([`placement()`]), its job, and the
/// completion port the job's messages come to.
fn job(applied: Restrictions) -> Result<(Owned, Owned, placement::Placement), Failure> {
    let placement = placement()?;
    // SAFETY: an unnamed job, owned by the value returned.
    let job = Owned(unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(failed("a job"))?);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = job_flags(applied);
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    if !many_groups() {
        limits.BasicLimitInformation.Affinity = placement.worker;
    }
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
    if many_groups() {
        // The worker's group and its CPUs there: the basic limit's
        // affinity names no group.
        let affinity = windows::Win32::System::SystemInformation::GROUP_AFFINITY {
            Mask: placement.worker,
            Group: placement.group,
            Reserved: [0; 3],
        };
        // SAFETY: the job just made, and one group's affinity.
        unsafe {
            SetInformationJobObject(
                job.0,
                windows::Win32::System::JobObjects::JobObjectGroupInformationEx,
                (&raw const affinity).cast(),
                u32::try_from(size_of_val(&affinity)).unwrap_or(0),
            )
        }
        .map_err(failed("the job's processor group"))?;
    }
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
    Ok((job, port, placement))
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

/// How long a launch waits for another making a profile or staging copies.
const LAUNCHES_WAIT_MS: u32 = 30_000;

/// Every access to a mutex (`MUTANT_ALL_ACCESS`, winnt.h).
const MUTEX_ALL_ACCESS: u32 = 0x001f_0001;

/// The SID of the AppContainer `name`, which needs no profile.
fn derive(name: &str) -> Result<Sid, Failure> {
    let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a nul-terminated name; the SID returned is freed by `Sid`.
    unsafe { DeriveAppContainerSidFromAppContainerName(PCWSTR(name.as_ptr())) }
        .map(Sid)
        .map_err(failed("the profile's SID"))
}

/// Step 2: the SID of the AppContainer profile `name`. The first launch
/// makes the profile, which Windows keeps; a later one finds its folder and
/// leaves it alone. Asking `CreateAppContainerProfile` for a profile that
/// exists is not harmless: the call, made while another launch creates its
/// process in the AppContainer, can fail that creation with
/// `ERROR_FILE_NOT_FOUND` (2 of 120 launches from four threads at once). So
/// the user's launches make a profile one at a time ([`Launches`]), each
/// looking for it again once it holds the mutex.
fn profile(name: &str) -> Result<Sid, Failure> {
    let sid = derive(name)?;
    if made(&sid)? {
        return Ok(sid);
    }
    let _one_at_a_time = Launches::hold()?;
    if !made(&sid)? {
        let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
        make_profile(&name)?;
    }
    Ok(sid)
}

/// The user's text SID, from this process's token.
pub fn user_sid() -> Result<String, Failure> {
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
    unsafe { text.to_string() }.map_err(|_| (0, "the user's SID as text".to_owned()))
}

/// The user's launches, in every process and session, one at a time where
/// they make the profile and stage the copies: the mutex
/// `Global\tapestry-media-` and the user's SID, whose DACL grants the user
/// alone. One a process ended holding is taken all the same: what it
/// guards is looked at again under it. Held until dropped.
pub struct Launches(Owned);

impl Launches {
    /// Takes the mutex, waiting up to 30 s.
    ///
    /// # Errors
    ///
    /// When it cannot be made or opened, or another launch holds it longer.
    pub fn hold() -> Result<Self, Failure> {
        let mutex = Self::mutex()?;
        // SAFETY: the mutex just opened.
        let waited = unsafe { WaitForSingleObject(mutex.0, LAUNCHES_WAIT_MS) };
        if waited != WAIT_OBJECT_0 && waited != WAIT_ABANDONED {
            return Err((ERROR_TIMEOUT.0, "the launches' mutex".to_owned()));
        }
        Ok(Self(mutex))
    }

    /// The mutex, made with its DACL or opened.
    fn mutex() -> Result<Owned, Failure> {
        let user = user_sid()?;
        let sddl: Vec<u16> = format!("D:P(A;;0x{MUTEX_ALL_ACCESS:x};;;{user})")
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a nul-terminated SDDL string; the descriptor returned is
        // freed by `Local`.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        }
        .map_err(failed("the launches' mutex's DACL"))?;
        let _descriptor = Local(descriptor.0);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let name: Vec<u16> = format!("Global\\tapestry-media-{user}")
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: a nul-terminated name, and attributes whose descriptor
        // lives through the call; the mutex is closed by `Owned`.
        unsafe { CreateMutexW(Some(&raw const attributes), false, PCWSTR(name.as_ptr())) }
            .map(Owned)
            .map_err(failed("the launches' mutex"))
    }
}

#[cfg(test)]
impl Launches {
    /// The mutex's handle, to read its DACL.
    pub fn handle(&self) -> HANDLE {
        self.0.0
    }
}

impl Drop for Launches {
    fn drop(&mut self) {
        // SAFETY: the mutex this thread holds.
        let _ = unsafe { ReleaseMutex(self.0.0) };
    }
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

/// The access the AppContainer gets on the copies and their directory.
pub(super) const DIRECTORY_ACCESS: u32 = FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0;

/// Memory `LocalAlloc` made, freed when dropped.
pub(super) struct Local(pub(super) *mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: memory the security calls allocated with `LocalAlloc`.
            let _ = unsafe { LocalFree(Some(HLOCAL(self.0))) };
        }
    }
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
    // SAFETY: one entry merged into the object's DACL; the ACL returned is
    // freed by `Local`.
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

    /// Terminates it for `(step, failure)`, watching it end for `observe`:
    /// its handle stays with the error unless it ended.
    fn refuse(mut self, (step, (code, what)): (Step, Failure), observe: Duration) -> LaunchError {
        self.armed = false;
        let process = terminate(self.process.0, observe);
        let unended = (!matches!(process, Made::Ended(_))).then(|| {
            let handle = std::mem::replace(&mut self.process, Owned(HANDLE::default())).release();
            // SAFETY: the process handle this value owned, owned by the
            // value returned from here.
            unsafe { OwnedHandle::from_raw_handle(handle.0) }
        });
        LaunchError {
            step,
            code,
            what,
            process,
            unended,
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
            let _ = terminate(self.process.0, OBSERVE);
        }
    }
}

/// Terminates `process` and watches it end for `observe`: whether it ended,
/// with what, and otherwise whether terminating it or the wait failed.
fn terminate(process: HANDLE, observe: Duration) -> Made {
    // SAFETY: a process this launch made.
    let terminated = unsafe { TerminateProcess(process, REFUSED) };
    let ms = u32::try_from(observe.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: as above.
    let waited = unsafe { WaitForSingleObject(process, ms) };
    if waited == WAIT_OBJECT_0 {
        let mut exit = 0;
        // SAFETY: as above.
        let read = unsafe { GetExitCodeProcess(process, &raw mut exit) };
        return Made::Ended(read.is_ok().then_some(exit));
    }
    if waited != WAIT_TIMEOUT {
        return Made::Unknown(last_error());
    }
    match terminated {
        Ok(()) => Made::Pending,
        Err(e) => Made::NotTerminated(code_of(&e)),
    }
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
        let mut flags: PROCESS_CREATION_FLAGS =
            CREATE_SUSPENDED | DETACHED_PROCESS | setup.priority() | CREATE_UNICODE_ENVIRONMENT;
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
    placement: Option<&placement::Placement>,
    sid: Option<&Sid>,
    pipe_ends: [u64; 2],
    expected: Restrictions,
    priority: PROCESS_CREATION_FLAGS,
) -> Result<Vec<handles::Entry>, Failure> {
    // SAFETY: the process this launch made, with every access.
    let class = unsafe { windows::Win32::System::Threading::GetPriorityClass(child.process.0) };
    if class != priority.0 {
        return Err((
            0,
            format!("its priority class is {class:#x}, not {:#x}", priority.0),
        ));
    }
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
        let Some(placement) = placement else {
            return Err((0, "no placement".to_owned()));
        };
        job_holds(job, expected, placement)?;
        placed(child.process.0, placement)?;
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
fn job_holds(
    job: &Owned,
    expected: Restrictions,
    placement: &placement::Placement,
) -> Result<(), Failure> {
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
    if many_groups() {
        group_holds(job, placement)?;
    }
    let basic = limits.BasicLimitInformation;
    let one = expected.has(Restrictions::ONE_PROCESS);
    if basic.LimitFlags != job_flags(expected)
        || limits.ProcessMemoryLimit != MEMORY_CAP
        || (one && basic.ActiveProcessLimit != 1)
        || (!many_groups() && basic.Affinity != placement.worker)
    {
        return Err((
            0,
            format!(
                "its job's limits are {:#x}, {} bytes, {} processes, CPUs {:#x}",
                basic.LimitFlags.0,
                limits.ProcessMemoryLimit,
                basic.ActiveProcessLimit,
                basic.Affinity
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

/// Checks that `job`'s group affinity reads back as `placement`'s group
/// and CPUs alone (a system of more than one group, where the basic limit's
/// affinity names no group).
fn group_holds(job: &Owned, placement: &placement::Placement) -> Result<(), Failure> {
    use windows::Win32::System::SystemInformation::GROUP_AFFINITY;
    let mut groups = [GROUP_AFFINITY::default(); 4];
    let mut len = 0_u32;
    // SAFETY: the job this launch made, and an array of the size given.
    unsafe {
        QueryInformationJobObject(
            Some(job.0),
            windows::Win32::System::JobObjects::JobObjectGroupInformationEx,
            groups.as_mut_ptr().cast(),
            u32::try_from(size_of_val(&groups)).unwrap_or(0),
            Some(&raw mut len),
        )
    }
    .map_err(failed("the job's processor group"))?;
    let n = len as usize / size_of::<GROUP_AFFINITY>();
    let read: Vec<(u16, usize)> = groups
        .get(..n)
        .unwrap_or(&[])
        .iter()
        .map(|g| (g.Group, g.Mask))
        .collect();
    if read != [(placement.group, placement.worker)] {
        return Err((
            0,
            format!(
                "its job's groups are {read:x?}, not group {} CPUs {:#x}",
                placement.group, placement.worker
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
/// game's log, the rest to `tx`, each stamped with its arrival. One reply
/// past the queue's capacity marks the worker `overflowed` and ends the
/// reading, which closes the pipe: the worker's next reply fails and ends
/// it. The game's thread never waits on the queue, and takes each reply
/// without allocating. `in_hand` is set from before a reply's stamp until
/// it is queued, so the session can tell that every reply stamped before
/// an instant is queued ([`super::Worker::in_hand`]). Returns the line the
/// end of an unexpected end logs, which the caller logs once the sender is
/// dropped. A test's worker also keeps its log lines and counts the close
/// answers queued ([`super::Worker::closes`]).
fn read_replies(
    replies: Owned,
    tx: &SyncSender<(std::time::Instant, Reply)>,
    overflowed: &AtomicBool,
    in_hand: &AtomicBool,
    beats: &super::Beats,
    #[cfg(test)] (log, closes): (
        &std::sync::Arc<Mutex<String>>,
        &std::sync::atomic::AtomicU64,
    ),
) -> Option<String> {
    use std::io::Read as _;
    // SAFETY: the game's end of the pipe from the worker, owned by the file
    // from here.
    let mut pipe = unsafe { std::fs::File::from_raw_handle(replies.release().0) };
    let mut lines = 0;
    loop {
        let mut prefix = [0; 2];
        if pipe.read_exact(&mut prefix).is_err() {
            return None;
        }
        let len = match control::frame_len(prefix) {
            Ok(len) => len,
            Err(e) => return Some(format!("a reply frame the protocol does not have ({e})")),
        };
        let mut message = vec![0; len];
        if pipe.read_exact(&mut message).is_err() {
            return None;
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
            Ok(Reply::Beat) => beats.beat(),
            Ok(reply) => {
                #[cfg(test)]
                let closed = matches!(reply, Reply::Closed { .. });
                in_hand.store(true, Ordering::SeqCst);
                let sent = tx.try_send((std::time::Instant::now(), reply));
                in_hand.store(false, Ordering::SeqCst);
                #[cfg(test)]
                if closed && sent.is_ok() {
                    closes.fetch_add(1, Ordering::SeqCst);
                }
                match sent {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        overflowed.store(true, Ordering::Release);
                        return Some(format!(
                            "more than {REPLIES} replies unread; its replies end"
                        ));
                    }
                    Err(TrySendError::Disconnected(_)) => return None,
                }
            }
            Err(e) => return Some(format!("a reply that does not decode ({e})")),
        }
    }
}

/// The handshake: the first reply other than a log line is the hello,
/// within the deadline, naming the protocol's version the game speaks
/// (`control::PROTOCOL`): a worker of another version is refused before
/// anything is handed to it.
fn handshake(
    replies: &Receiver<(std::time::Instant, Reply)>,
    limit: Duration,
) -> Result<Hello, Failure> {
    match replies.recv_timeout(limit).map(|(_, reply)| reply) {
        Ok(Reply::Hello {
            protocol,
            version,
            configuration,
        }) if protocol == control::PROTOCOL => Ok(Hello {
            version,
            configuration,
        }),
        Ok(Reply::Hello { protocol, .. }) => Err((
            ERROR_REVISION_MISMATCH.0,
            format!("it speaks protocol {protocol}, not {}", control::PROTOCOL),
        )),
        Ok(other) => Err((0, format!("it answered {other:?} before its hello"))),
        Err(RecvTimeoutError::Timeout) => Err((ERROR_TIMEOUT.0, "no hello in time".to_owned())),
        Err(RecvTimeoutError::Disconnected) => Err((0, "it ended before its hello".to_owned())),
    }
}
