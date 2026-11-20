//! One process tree (docs/test-device.md "Run Guard"): the runner makes a
//! job object that kills every process in it when its last handle closes
//! and allows no breakaway, puts itself in it before it starts anything,
//! and holds the only handle, so every child is in the job from its
//! creation and ends when the runner ends, however it ends (the technique
//! cargo uses on Windows). Each child inherits only its own standard
//! handles (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`), so no child keeps
//! another's pipe open, and the end of a child's output tells its reader
//! that the child ended. Every child is checked to be in the job
//! (`IsProcessInJob`) before it is used.

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::io::{FromRawHandle as _, IntoRawHandle as _};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, bail};
use test_device::evidence::Process;
use windows::Win32::Foundation::{
    FILETIME, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};
use windows::core::{BOOL, PWSTR};

use super::win::{Owned, wide};

/// The run's job.
pub struct Job {
    handle: Owned,
}

impl Job {
    /// Makes a job that kills its processes when its last handle closes
    /// and allows no breakaway. Nothing is in it yet.
    ///
    /// # Errors
    ///
    /// The job cannot be made or limited.
    pub fn create() -> anyhow::Result<Self> {
        // SAFETY: no attributes, no name.
        let handle = Owned(unsafe { CreateJobObjectW(None, None) }.context("CreateJobObject")?);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // No JOB_OBJECT_LIMIT_BREAKAWAY_OK: a child asking to break away
        // is refused.
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the struct is the class's, of its size.
        unsafe {
            SetInformationJobObject(
                handle.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast::<c_void>(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
            )
        }
        .context("SetInformationJobObject")?;
        Ok(Self { handle })
    }

    /// Makes the job and puts this process in it: from then on, every
    /// process this one starts is in it from its creation, and all of them
    /// end when this process ends. A runner that cannot join refuses to
    /// run.
    ///
    /// # Errors
    ///
    /// The job cannot be made, or this process cannot join it (an
    /// enclosing job that forbids it).
    pub fn join() -> anyhow::Result<Self> {
        let job = Self::create()?;
        // SAFETY: the pseudo-handle of this process.
        unsafe { AssignProcessToJobObject(job.handle.0, GetCurrentProcess()) }
            .context("this process cannot join its job")?;
        Ok(job)
    }

    /// Whether `process` is in this job.
    ///
    /// # Errors
    ///
    /// The query failed.
    pub fn holds(&self, process: HANDLE) -> anyhow::Result<bool> {
        let mut inside = BOOL(0);
        // SAFETY: both handles are open; one BOOL written.
        unsafe { IsProcessInJob(process, Some(self.handle.0), &mut inside) }
            .context("IsProcessInJob")?;
        Ok(inside.as_bool())
    }

    /// Ends every process in this job, and in the jobs nested in it, with
    /// exit code `code`.
    ///
    /// # Errors
    ///
    /// The call failed.
    pub fn terminate(&self, code: u32) -> anyhow::Result<()> {
        // SAFETY: an open job handle.
        unsafe { TerminateJobObject(self.handle.0, code) }.context("TerminateJobObject")
    }
}

/// A pipe: one end for a child, one this process keeps. Neither is
/// inheritable outside [`start`], which marks a child's handles
/// inheritable only while it starts that child.
pub struct Pipe {
    /// The end the child gets.
    pub child: Owned,
    /// The end this process keeps.
    pub own: File,
}

impl Pipe {
    /// A pipe for a child's input (`input`: the child reads) or output.
    ///
    /// # Errors
    ///
    /// The pipe cannot be made.
    pub fn new(input: bool) -> anyhow::Result<Self> {
        let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
        // SAFETY: two handles written; no attributes, so neither inherits.
        unsafe { CreatePipe(&mut read, &mut write, None, 0) }.context("CreatePipe")?;
        let (child, own) = if input { (read, write) } else { (write, read) };
        Ok(Self {
            child: Owned(child),
            // SAFETY: a handle this process owns, given to the File.
            own: unsafe { File::from_raw_handle(own.0) },
        })
    }
}

/// A child's standard handles, each the child's end of a pipe or a file.
pub struct Stdio {
    pub input: Owned,
    pub output: Owned,
    pub error: Owned,
}

impl Stdio {
    /// `file` as a child's standard handle.
    pub fn file(file: File) -> Owned {
        Owned(HANDLE(file.into_raw_handle()))
    }

    /// The null device, for a standard handle the child does not use.
    ///
    /// # Errors
    ///
    /// It cannot be opened.
    pub fn null() -> anyhow::Result<Owned> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("NUL")
            .context("NUL")?;
        Ok(Self::file(f))
    }
}

/// A child of the runner, in its job.
pub struct Child {
    pub handle: Owned,
    pub process: Process,
}

impl Child {
    /// Waits up to `wait` for the child to end: its exit code, or `None`
    /// while it runs.
    pub fn wait(&self, wait: Duration) -> Option<i32> {
        let ms = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: an open process handle.
        if unsafe { WaitForSingleObject(self.handle.0, ms) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0_u32;
        // SAFETY: one u32 written.
        unsafe { GetExitCodeProcess(self.handle.0, &mut code) }.ok()?;
        Some(code.cast_signed())
    }

    /// Ends the child with exit code `code`, by its handle.
    ///
    /// # Errors
    ///
    /// The call failed.
    pub fn kill(&self, code: u32) -> anyhow::Result<()> {
        // SAFETY: an open process handle.
        unsafe { TerminateProcess(self.handle.0, code) }.context("TerminateProcess")
    }
}

/// Opens the process its id and creation time name, to wait for it and end
/// it: a process the id now names that was created at another time is
/// another process, and refused.
///
/// # Errors
///
/// It cannot be opened, or it is another process.
pub fn open(p: Process) -> anyhow::Result<Child> {
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    // SAFETY: no pointers.
    let h = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
            false,
            p.pid,
        )
    }
    .with_context(|| format!("opening process {}", p.pid))?;
    let handle = Owned(h);
    if created(handle.0)? != p.created {
        bail!("process {} is another process than the one recorded", p.pid);
    }
    Ok(Child { handle, process: p })
}

/// A process's creation time (the system's FILETIME), which with its id
/// names it: an id the system reuses names another process.
///
/// # Errors
///
/// The times cannot be read.
pub fn created(process: HANDLE) -> anyhow::Result<u64> {
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: four FILETIMEs written.
    unsafe { GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u) }
        .context("GetProcessTimes")?;
    Ok(u64::from(c.dwHighDateTime) << 32 | u64::from(c.dwLowDateTime))
}

/// This process, as a record names it.
///
/// # Errors
///
/// Its creation time cannot be read.
pub fn this_process() -> anyhow::Result<Process> {
    Ok(Process {
        pid: std::process::id(),
        // SAFETY: the pseudo-handle of this process.
        created: created(unsafe { GetCurrentProcess() })?,
    })
}

/// A command line argument quoted for `CommandLineToArgvW`.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from('"');
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

/// Starts `exe` with `args` in the runner's `job` ([`start`]) and checks
/// that it is in the job before it is used: a child outside it is killed
/// and refused.
///
/// # Errors
///
/// It cannot be started, or it is not in the job.
pub fn spawn(
    job: &Job,
    exe: &Path,
    args: &[String],
    env: &[(&str, &str)],
    stdio: &Stdio,
) -> anyhow::Result<Child> {
    let child = start(exe, args, env, stdio, None)?;
    if !job.holds(child.handle.0)? {
        let _ = child.kill(1);
        bail!(
            "{} (process {}) is not in the run's job",
            exe.display(),
            child.process.pid
        );
    }
    Ok(child)
}

/// Starts `exe` as [`spawn`] does, in a job of its own nested in the
/// runner's `job` from its creation (`PROC_THREAD_ATTRIBUTE_JOB_LIST`), so
/// that the runner can end the child with everything it starts
/// ([`Job::terminate`]). The child's job and the child.
///
/// # Errors
///
/// The job cannot be made, the child cannot be started, or it is not in
/// both jobs.
pub fn spawn_in_own_job(
    job: &Job,
    exe: &Path,
    args: &[String],
    env: &[(&str, &str)],
    stdio: &Stdio,
) -> anyhow::Result<(Job, Child)> {
    let own = Job::create()?;
    let child = start(exe, args, env, stdio, Some(&own))?;
    if !(job.holds(child.handle.0)? && own.holds(child.handle.0)?) {
        let _ = own.terminate(1);
        bail!(
            "{} (process {}) is not in its own job within the run's",
            exe.display(),
            child.process.pid
        );
    }
    Ok((own, child))
}

/// One start at a time: a child's handles are inheritable only while it
/// starts.
static STARTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sets or clears the inherit flag of `handles`.
fn inherit(handles: &[HANDLE], on: bool) -> anyhow::Result<()> {
    let flags = if on {
        HANDLE_FLAG_INHERIT
    } else {
        HANDLE_FLAGS(0)
    };
    for &h in handles {
        // SAFETY: an open handle of this process; only its flag changes.
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, flags) }
            .context("SetHandleInformation")?;
    }
    Ok(())
}

/// Starts `exe` with `args`, inheriting `stdio` and no other handle (its
/// handles are inheritable only while it starts, and the start lists them
/// alone), with `env` added to this process's environment.
///
/// # Errors
///
/// It cannot be started.
pub fn start(
    exe: &Path,
    args: &[String],
    env: &[(&str, &str)],
    stdio: &Stdio,
    job: Option<&Job>,
) -> anyhow::Result<Child> {
    let handles = [stdio.input.0, stdio.output.0, stdio.error.0];
    let _one = STARTING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    inherit(&handles, true)?;
    let jobs = job.map(|j| [j.handle.0]);
    let started = start_listed(exe, args, env, stdio, &handles, jobs.as_ref());
    let cleared = inherit(&handles, false);
    let child = started?;
    cleared?;
    Ok(child)
}

fn start_listed(
    exe: &Path,
    args: &[String],
    env: &[(&str, &str)],
    stdio: &Stdio,
    handles: &[HANDLE; 3],
    jobs: Option<&[HANDLE; 1]>,
) -> anyhow::Result<Child> {
    let count = if jobs.is_some() { 2 } else { 1 };
    let mut size = 0;
    // SAFETY: a size query; it fails with the size written.
    let _ = unsafe { InitializeProcThreadAttributeList(None, count, None, &mut size) };
    let mut buf = vec![0_u64; size.div_ceil(8)];
    let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr().cast());
    // SAFETY: the buffer holds `size` bytes.
    unsafe { InitializeProcThreadAttributeList(Some(list), count, None, &mut size) }
        .context("InitializeProcThreadAttributeList")?;
    // SAFETY: the handle array outlives the list's use below.
    let listed = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(handles.as_ptr().cast()),
            size_of_val(handles),
            None,
            None,
        )
    };
    let listed = listed.and_then(|()| match jobs {
        // SAFETY: the job array outlives the list's use below.
        Some(jobs) => unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(jobs.as_ptr().cast()),
                size_of_val(jobs),
                None,
                None,
            )
        },
        None => Ok(()),
    });
    let result = listed.context("UpdateProcThreadAttribute").and_then(|()| {
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap_or(0);
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = stdio.input.0;
        si.StartupInfo.hStdOutput = stdio.output.0;
        si.StartupInfo.hStdError = stdio.error.0;
        si.lpAttributeList = list;
        let line = std::iter::once(exe.display().to_string())
            .chain(args.iter().cloned())
            .map(|a| quote(&a))
            .collect::<Vec<_>>()
            .join(" ");
        let mut line = wide(&line);
        let block = env_block(env);
        let exe_w = wide(&exe.display().to_string());
        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: every pointer is to a live buffer for the call.
        unsafe {
            CreateProcessW(
                windows::core::PCWSTR(exe_w.as_ptr()),
                Some(PWSTR(line.as_mut_ptr())),
                None,
                None,
                true,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
                Some(block.as_ptr().cast()),
                None,
                &raw const si.StartupInfo,
                &mut pi,
            )
        }
        .with_context(|| format!("starting {}", exe.display()))?;
        let _ = Owned(pi.hThread);
        Ok(Owned(pi.hProcess))
    });
    // SAFETY: the list was initialised above.
    unsafe { DeleteProcThreadAttributeList(list) };
    let handle = result?;
    let process = Process {
        // SAFETY: an open process handle.
        pid: unsafe { windows::Win32::System::Threading::GetProcessId(handle.0) },
        created: created(handle.0)?,
    };
    Ok(Child { handle, process })
}

/// This process's environment with `extra` set, as a Unicode block
/// (`CREATE_UNICODE_ENVIRONMENT`).
fn env_block(extra: &[(&str, &str)]) -> Vec<u16> {
    let mut vars: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !extra.iter().any(|(e, _)| e.eq_ignore_ascii_case(k)))
        .collect();
    vars.extend(
        extra
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
    let mut out = Vec::new();
    for (k, v) in vars {
        out.extend(format!("{k}={v}").encode_utf16());
        out.push(0);
    }
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::time::Instant;

    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };

    use super::*;

    /// The environment variable that makes [`helper`] act.
    const HELPER: &str = "TAPESTRY_JOB_TEST_HELPER";

    fn stdio(output: &Pipe) -> Stdio {
        Stdio {
            input: Stdio::null().unwrap(),
            output: Owned(dup(output.child.0)),
            error: Stdio::null().unwrap(),
        }
    }

    fn dup(h: HANDLE) -> HANDLE {
        use windows::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
        let mut out = HANDLE::default();
        // SAFETY: duplicating an open handle within this process.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                h,
                GetCurrentProcess(),
                &mut out,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        }
        .unwrap();
        out
    }

    /// This test binary again, running [`helper`] in `mode`.
    fn me(mode: &str) -> (std::path::PathBuf, Vec<String>, String) {
        (
            std::env::current_exe().unwrap(),
            vec![
                "dev::test_device::job::tests::helper".into(),
                "--exact".into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ],
            mode.to_owned(),
        )
    }

    /// Not a test of its own: the child the tests start. Without the
    /// environment variable it does nothing. `sleep` sleeps; `runner`
    /// joins a job, starts a sleeper in it and prints the sleeper's id;
    /// `breakaway` joins a job and prints whether a child asking to break
    /// away was refused; `outside` prints whether a child of a job it did
    /// not join is refused; `cutter` joins a job, starts a `runner` in a job
    /// of its own, terminates that job with a cut code and prints the exit
    /// codes of the runner and of the sleeper the runner started.
    #[test]
    fn helper() {
        let Ok(mode) = std::env::var(HELPER) else {
            return;
        };
        let out = |s: String| {
            let mut o = std::io::stdout();
            let _ = writeln!(o, "HELPER {s}");
            let _ = o.flush();
        };
        match mode.as_str() {
            "sleep" => std::thread::sleep(Duration::from_secs(30)),
            "runner" => {
                let job = Job::join().unwrap();
                let (exe, args, _) = me("sleep");
                // The null device for every standard handle: a pipe to this
                // runner would end the sleeper by itself once the runner
                // dies, whatever the job does.
                let null = Stdio {
                    input: Stdio::null().unwrap(),
                    output: Stdio::null().unwrap(),
                    error: Stdio::null().unwrap(),
                };
                let child = spawn(&job, &exe, &args, &[(HELPER, "sleep")], &null).unwrap();
                out(child.process.pid.to_string());
                std::thread::sleep(Duration::from_secs(30));
            }
            "cutter" => {
                let job = Job::join().unwrap();
                let (exe, args, _) = me("runner");
                let piped = Pipe::new(false).unwrap();
                let io = Stdio {
                    input: Stdio::null().unwrap(),
                    output: Owned(dup(piped.child.0)),
                    error: Stdio::null().unwrap(),
                };
                let (own, runner) =
                    spawn_in_own_job(&job, &exe, &args, &[(HELPER, "runner")], &io).unwrap();
                drop(io);
                drop(piped.child);
                let pid: u32 = BufReader::new(piped.own)
                    .lines()
                    .find_map(|l| l.ok()?.split_once("HELPER ")?.1.parse().ok())
                    .unwrap();
                let sleeper = open_any(pid);
                own.terminate(CUT).unwrap();
                let runner_code = runner.wait(Duration::from_secs(5));
                let sleeper_code = sleeper.wait(Duration::from_secs(5));
                out(format!("codes {runner_code:?} {sleeper_code:?}"));
            }
            "breakaway" => {
                let _job = Job::join().unwrap();
                let (exe, args, _) = me("sleep");
                let mut cmd = std::process::Command::new(exe);
                std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0100_0000);
                let refused = cmd.args(args).env(HELPER, "sleep").spawn().is_err();
                out(format!("refused {refused}"));
            }
            _ => {}
        }
    }

    /// Starts this binary's helper in `mode` outside any job of ours and
    /// reads its `HELPER` line.
    fn run_helper(mode: &str) -> (std::process::Child, String) {
        let (exe, args, mode) = me(mode);
        let mut child = std::process::Command::new(exe)
            .args(args)
            .env(HELPER, mode)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = lines
            .find_map(|l| Some(l.ok()?.split_once("HELPER ")?.1.to_owned()))
            .expect("the helper's line");
        (child, line)
    }

    /// A runner killed by its handle takes its children with it: the job's
    /// last handle closes with the runner's process.
    #[test]
    fn a_killed_runner_takes_its_children_with_it() {
        let (mut runner, pid) = run_helper("runner");
        let pid: u32 = pid.parse().unwrap();
        // SAFETY: opening the sleeper by the id its runner printed.
        let sleeper = Owned(
            unsafe {
                OpenProcess(
                    PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                    false,
                    pid,
                )
            }
            .unwrap(),
        );
        // SAFETY: an open handle.
        let early = unsafe { WaitForSingleObject(sleeper.0, 0) };
        assert_ne!(early, WAIT_OBJECT_0, "the sleeper ended early");
        runner.kill().unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        // SAFETY: an open handle.
        while unsafe { WaitForSingleObject(sleeper.0, 10) } != WAIT_OBJECT_0 {
            if Instant::now() >= end {
                // Not left running: a copy of this binary would keep it
                // locked for the next build.
                // SAFETY: an open handle with terminate access.
                let _ = unsafe { TerminateProcess(sleeper.0, 1) };
                panic!("the sleeper outlived its runner");
            }
        }
        let _ = runner.wait();
    }

    /// A child the runner starts outside its job is killed and refused.
    #[test]
    fn a_child_outside_the_job_fails_the_run() {
        // A job this process did not join: its children are not in it.
        let job = Job::create().unwrap();
        let (exe, args, _) = me("sleep");
        let p = Pipe::new(false).unwrap();
        let Err(e) = spawn(&job, &exe, &args, &[(HELPER, "sleep")], &stdio(&p)) else {
            panic!("a child outside the job was taken");
        };
        assert!(
            format!("{e:#}").contains("is not in the run's job"),
            "{e:#}"
        );
    }

    /// A cut code: `0x40000000` plus some bits.
    const CUT: u32 = 0x4000_0123;

    /// The process `pid`, to wait for, without its creation time.
    fn open_any(pid: u32) -> Child {
        // SAFETY: opening a process by id.
        let h = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                false,
                pid,
            )
        }
        .unwrap();
        let handle = Owned(h);
        let created = created(handle.0).unwrap();
        Child {
            handle,
            process: Process { pid, created },
        }
    }

    /// A child started in a job of its own ends, with everything it
    /// started in its own nested job, when the runner terminates that job:
    /// each with the cut code.
    #[test]
    fn a_child_s_own_job_ends_its_tree_with_the_cut_code() {
        let (mut helper, line) = run_helper("cutter");
        let code = Some(CUT.cast_signed());
        assert_eq!(line, format!("codes {code:?} {code:?}"));
        let _ = helper.wait();
    }

    /// A child in the job cannot break away from it.
    #[test]
    fn the_job_refuses_breakaway() {
        let (mut helper, line) = run_helper("breakaway");
        assert_eq!(line, "refused true");
        let _ = helper.wait();
    }

    /// A child inherits only its own standard handles: a pipe made for
    /// another child before it started stays out of it, so the other
    /// child's output ends when that child does.
    #[test]
    fn a_child_inherits_only_its_own_handles() {
        // The second child's output pipe, its end inheritable while the
        // first starts (as when another start is under way).
        let second = Pipe::new(false).unwrap();
        inherit(&[second.child.0], true).unwrap();
        let first_out = Pipe::new(false).unwrap();
        let (exe, args, _) = me("sleep");
        let first = start(&exe, &args, &[(HELPER, "sleep")], &stdio(&first_out), None).unwrap();
        // The second child ends at once.
        let cmd = std::path::PathBuf::from(std::env::var("ComSpec").unwrap());
        let quick = start(
            &cmd,
            &["/c".into(), "exit 0".into()],
            &[],
            &stdio(&second),
            None,
        )
        .unwrap();
        drop(second.child);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut own = second.own;
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = std::io::Read::read_to_end(&mut own, &mut sink);
            let _ = tx.send(());
        });
        let ended = rx.recv_timeout(Duration::from_secs(2)).is_ok();
        let _ = first.kill(1);
        let _ = quick.wait(Duration::from_secs(1));
        assert!(
            ended,
            "the second child's output did not end: the first child holds it"
        );
    }

    #[test]
    fn arguments_are_quoted_for_the_command_line() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    }
}
