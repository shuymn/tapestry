//! The job watcher (design.md "Threat Model" T02): it ends a worker whose
//! memory reaches its job's cap. The job refuses a commit past the cap and
//! leaves the worker running, so the game ends it: on the job's message
//! that it refused one, or, since Windows does not guarantee that message,
//! when its own read of the job's accounting, every 100 ms whether a message
//! came or not, finds the worker's peak commit at the line 64 MiB below the
//! cap, above any one allocation the worker lets FFmpeg make. The peak never
//! falls, so a crossing between two reads is not missed; a message acts at
//! once and never moves the schedule. After either nothing in that worker
//! is trusted.
//!
//! The job's messages come through a completion port, the way Windows
//! reports a job's events, associated with the job before the worker is put
//! in it ([`port`]). Each launch makes its own job, so the peak read is that
//! worker's alone. Each read also looks at the worker's process: an end the
//! watcher did not cause is the worker's own, a crash or an exit the game
//! did not ask for. The game never waits on the watcher: it reads the end
//! the watcher recorded ([`Watch::end`]), and dropping the watch stops it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED, PostQueuedCompletionStatus,
};
use windows::Win32::System::JobObjects::{
    JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectAssociateCompletionPortInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::SystemServices::JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, WaitForSingleObject,
};

use super::Owned;

/// The line at which the game ends a worker: 64 MiB below its job's cap.
pub const LINE: usize = super::launch::MEMORY_CAP - (64 << 20);

/// How often the watcher reads the job's accounting.
const PERIOD: Duration = Duration::from_millis(100);

/// The exit code of a worker the watcher ended on the job's message.
pub const ENDED_BY_MESSAGE: u32 = 0xc0de_0002;

/// The exit code of a worker the watcher ended at the line.
pub const ENDED_AT_LINE: u32 = 0xc0de_0003;

/// The completion key the job's messages carry.
const JOB_KEY: usize = 1;

/// The completion key of the packet that stops the watcher.
const STOP_KEY: usize = 2;

/// How a worker ended, as its watcher saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The watcher ended it on the job's message that it refused a commit.
    Message,
    /// The watcher ended it when its peak commit reached the line.
    Line,
    /// It ended by itself, with its exit code: a crash, or an exit the
    /// game did not ask for.
    Exited(u32),
}

impl End {
    const fn encode(self) -> u64 {
        match self {
            Self::Message => 1 << 32,
            Self::Line => 2 << 32,
            Self::Exited(code) => (3 << 32) | code as u64,
        }
    }

    fn decode(v: u64) -> Option<Self> {
        match v >> 32 {
            1 => Some(Self::Message),
            2 => Some(Self::Line),
            3 => Some(Self::Exited(u32::try_from(v & 0xffff_ffff).ok()?)),
            _ => None,
        }
    }
}

/// What a test changes in the watcher: nothing outside a test.
#[derive(Clone, Copy, Debug, Default)]
pub struct Variation {
    /// The job's memory message ends nothing.
    #[cfg(test)]
    pub no_message: bool,
    /// The read of the job's accounting ends nothing.
    #[cfg(test)]
    pub no_accounting: bool,
    /// The job's messages go to a port of their own, where the job's
    /// memory message is logged and discarded, as if Windows had not sent
    /// it: the watcher neither wakes nor reads on it.
    #[cfg(test)]
    pub discard_message: bool,
}

#[cfg(test)]
impl Variation {
    const fn message(self) -> bool {
        !self.no_message
    }

    const fn accounting(self) -> bool {
        !self.no_accounting
    }
}

#[cfg(not(test))]
#[expect(
    clippy::unused_self,
    reason = "a test's variation turns a path off; the game's never does"
)]
impl Variation {
    const fn message(self) -> bool {
        true
    }

    const fn accounting(self) -> bool {
        true
    }
}

/// What the watcher did, for a test to read.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A scheduled read of the job's accounting: when, and the peak commit
    /// it found.
    Read { at: Duration, peak: usize },
    /// The job's memory message: when, and whether the test discarded it.
    Message { at: Duration, discarded: bool },
    /// The worker's end: when, and how.
    Ended { at: Duration, end: End },
}

/// A new completion port.
fn new_port() -> windows::core::Result<Owned> {
    // SAFETY: a new port, owned by the value returned.
    Ok(Owned(unsafe {
        CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1)
    }?))
}

/// A completion port for `job`'s messages, associated with it before a
/// process is put in it (the recipe's step 1).
///
/// # Errors
///
/// When the port cannot be made or associated.
pub(super) fn port(job: &Owned) -> windows::core::Result<Owned> {
    let port = new_port()?;
    let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
        CompletionKey: std::ptr::without_provenance_mut(JOB_KEY),
        CompletionPort: port.0,
    };
    // SAFETY: the job and the port, and a struct of the size given.
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectAssociateCompletionPortInformation,
            (&raw const association).cast(),
            u32::try_from(size_of_val(&association)).unwrap_or(0),
        )
    }?;
    Ok(port)
}

/// The peak commit of any process `job` has held, or `None` when the read
/// fails.
pub(super) fn job_peak(job: &Owned) -> Option<usize> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: a handle to the job, and a struct of the size given.
    unsafe {
        QueryInformationJobObject(
            Some(job.0),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            u32::try_from(size_of_val(&limits)).unwrap_or(0),
            None,
        )
    }
    .ok()?;
    Some(limits.PeakProcessMemoryUsed)
}

/// `h`, duplicated within the game with the same access.
fn duplicate(h: HANDLE) -> windows::core::Result<Owned> {
    let mut copy = HANDLE::default();
    // SAFETY: a handle the game holds, duplicated within the game; the
    // value returned owns the copy.
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            h,
            GetCurrentProcess(),
            &raw mut copy,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    }?;
    Ok(Owned(copy))
}

/// A worker's watcher, a thread of its own, stopped when this drops.
pub struct Watch {
    /// The watcher's port.
    port: Arc<Owned>,
    end: Arc<AtomicU64>,
    #[cfg(test)]
    test: Test,
}

/// What a test reads of a watcher, and the port the job's messages go to
/// while it discards them.
#[cfg(test)]
struct Test {
    threads: Vec<std::thread::JoinHandle<()>>,
    events: Arc<std::sync::Mutex<Vec<Event>>>,
    sink: Option<Arc<Owned>>,
}

impl Watch {
    /// Starts watching the worker `process` (process id `pid`) in `job`,
    /// whose messages come to `port`.
    ///
    /// # Errors
    ///
    /// The system's error when a handle or port cannot be made or a thread
    /// cannot start.
    pub(super) fn start(
        job: &Owned,
        port: Owned,
        process: HANDLE,
        pid: u32,
        variation: Variation,
    ) -> Result<Self, u32> {
        let code = |e: windows::core::Error| e.code().0.cast_unsigned();
        let job = duplicate(job.0).map_err(code)?;
        let process = duplicate(process).map_err(code)?;
        let start = Instant::now();
        #[cfg(test)]
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        #[cfg(test)]
        let (port, sink) = if variation.discard_message {
            (new_port().map_err(code)?, Some(Arc::new(port)))
        } else {
            (port, None)
        };
        let port = Arc::new(port);
        let end = Arc::new(AtomicU64::new(0));
        let thread = {
            let port = Arc::clone(&port);
            let end = Arc::clone(&end);
            #[cfg(test)]
            let events = Arc::clone(&events);
            std::thread::Builder::new()
                .name("media worker watch".to_owned())
                .spawn(move || {
                    watch(
                        &Watched {
                            port: &port,
                            job: &job,
                            process: &process,
                            pid,
                            variation,
                            end: &end,
                            #[cfg(test)]
                            events: &events,
                        },
                        start,
                    );
                })
                .map_err(|e| e.raw_os_error().map_or(0, i32::cast_unsigned))?
        };
        #[cfg(not(test))]
        drop(thread);
        #[cfg(test)]
        let mut threads = vec![thread];
        #[cfg(test)]
        if let Some(sink) = &sink {
            let sink = Arc::clone(sink);
            let events = Arc::clone(&events);
            let discarding = std::thread::Builder::new()
                .name("media worker discard".to_owned())
                .spawn(move || discard(&sink, pid, &events, start));
            match discarding {
                Ok(t) => threads.push(t),
                Err(e) => return Err(e.raw_os_error().map_or(0, i32::cast_unsigned)),
            }
        }
        Ok(Self {
            port,
            end,
            #[cfg(test)]
            test: Test {
                threads,
                events,
                sink,
            },
        })
    }

    /// How the worker ended, once the watcher has seen it end.
    pub fn end(&self) -> Option<End> {
        End::decode(self.end.load(Ordering::Acquire))
    }

    /// Asks the watcher to stop, without waiting for it.
    fn stop(&self) {
        // SAFETY: the watcher's port, which this value keeps open. A packet
        // that cannot be posted leaves the watcher to stop at its next read
        // after the worker has ended.
        let _ = unsafe { PostQueuedCompletionStatus(self.port.0, 0, STOP_KEY, None) };
        #[cfg(test)]
        if let Some(sink) = &self.test.sink {
            // SAFETY: as above, the port the test discards messages from.
            let _ = unsafe { PostQueuedCompletionStatus(sink.0, 0, STOP_KEY, None) };
        }
    }
}

#[cfg(test)]
impl Watch {
    /// Posts the job's `message` about process `pid` to the port the job's
    /// messages go to, as the job posts it: the job's completion key, and
    /// the process id where an I/O packet carries its OVERLAPPED.
    pub fn post(&self, message: u32, pid: u32) -> windows::core::Result<()> {
        let port = self.test.sink.as_ref().unwrap_or(&self.port);
        let id: *const OVERLAPPED = std::ptr::without_provenance(pid as usize);
        // SAFETY: the port, which this value keeps open; the OVERLAPPED is
        // never dereferenced, by the watcher or the system.
        unsafe { PostQueuedCompletionStatus(port.0, message, JOB_KEY, Some(id)) }
    }

    /// What the watcher did so far.
    pub fn events(&self) -> Vec<Event> {
        self.test
            .events
            .lock()
            .map(|e| e.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Stops the watcher and waits until it has, its handles closed.
    pub fn join(mut self) {
        self.stop();
        for thread in std::mem::take(&mut self.test.threads) {
            let _ = thread.join();
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Logs and discards the job's memory messages about process `pid` as they
/// come to `sink`, until the watch stops.
#[cfg(test)]
fn discard(sink: &Owned, pid: u32, events: &std::sync::Mutex<Vec<Event>>, start: Instant) {
    loop {
        let (mut message, mut key) = (0, 0);
        let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
        // SAFETY: the sink port, and locals for what a packet carries.
        let got = unsafe {
            GetQueuedCompletionStatus(
                sink.0,
                &raw mut message,
                &raw mut key,
                &raw mut overlapped,
                u32::MAX,
            )
        };
        match got {
            Ok(()) if key == STOP_KEY => return,
            Ok(())
                if key == JOB_KEY
                    && message == JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT
                    && overlapped.addr() == pid as usize =>
            {
                log::warn!("media worker: the job's memory message discarded (test)");
                if let Ok(mut events) = events.lock() {
                    events.push(Event::Message {
                        at: start.elapsed(),
                        discarded: true,
                    });
                }
            }
            Ok(()) => {}
            Err(_) => return,
        }
    }
}

/// What a watcher watches, and where it records what it saw.
struct Watched<'a> {
    port: &'a Owned,
    job: &'a Owned,
    process: &'a Owned,
    pid: u32,
    variation: Variation,
    end: &'a AtomicU64,
    #[cfg(test)]
    events: &'a std::sync::Mutex<Vec<Event>>,
}

impl Watched<'_> {
    /// Records `end`, unless an end was recorded before.
    fn ended(&self, end: End, start: Instant) {
        let _ = self
            .end
            .compare_exchange(0, end.encode(), Ordering::AcqRel, Ordering::Acquire);
        self.log(Ok(end), start);
    }

    /// Ends the worker with `code`, as `end`.
    fn terminate(&self, code: u32, end: End, start: Instant) {
        // SAFETY: the watcher's own handle to the worker's job.
        if let Err(e) = unsafe { TerminateJobObject(self.job.0, code) } {
            log::warn!("media worker: its job could not be ended ({e})");
        }
        log::warn!("media worker: ended for its memory ({end:?})");
        self.ended(end, start);
    }

    /// The worker's exit code once it has ended.
    fn exited(&self) -> Option<u32> {
        // SAFETY: the watcher's own handle to the worker's process.
        if unsafe { WaitForSingleObject(self.process.0, 0) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0;
        // SAFETY: as above.
        unsafe { GetExitCodeProcess(self.process.0, &raw mut code) }.ok()?;
        Some(code)
    }

    /// Logs a read's peak (`Err(Some)`), the job's memory message
    /// (`Err(None)`) or the worker's end, for a test.
    #[cfg(test)]
    fn log(&self, what: Result<End, Option<usize>>, start: Instant) {
        let at = start.elapsed();
        let event = match what {
            Ok(end) => Event::Ended { at, end },
            Err(Some(peak)) => Event::Read { at, peak },
            Err(None) => Event::Message {
                at,
                discarded: false,
            },
        };
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }

    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "a test reads what the watcher did; the game does not"
    )]
    const fn log(&self, _: Result<End, Option<usize>>, _: Instant) {}
}

/// The watcher's loop, from `start` until the worker ends or the watch is
/// stopped.
fn watch(w: &Watched, start: Instant) {
    let mut next = start + PERIOD;
    loop {
        let now = Instant::now();
        if now >= next {
            // A read late by more than a period skips the reads it missed:
            // the schedule stays on its own grid.
            while next <= now {
                next += PERIOD;
            }
            if let Some(code) = w.exited() {
                w.ended(End::Exited(code), start);
                return;
            }
            if w.variation.accounting() {
                let peak = job_peak(w.job);
                w.log(Err(Some(peak.unwrap_or(0))), start);
                if peak.is_some_and(|p| p >= LINE) {
                    w.terminate(ENDED_AT_LINE, End::Line, start);
                    return;
                }
            }
            continue;
        }
        let wait = u32::try_from((next - now).as_millis()).unwrap_or(u32::MAX);
        let (mut message, mut key) = (0, 0);
        let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
        // SAFETY: the watcher's port, and locals for what a packet carries.
        let got = unsafe {
            GetQueuedCompletionStatus(
                w.port.0,
                &raw mut message,
                &raw mut key,
                &raw mut overlapped,
                wait,
            )
        };
        match got {
            Ok(()) if key == STOP_KEY => return,
            // The job's message carries the process's id where an I/O
            // packet carries its OVERLAPPED.
            Ok(())
                if key == JOB_KEY
                    && message == JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT
                    && overlapped.addr() == w.pid as usize =>
            {
                w.log(Err(None), start);
                if w.variation.message() {
                    w.terminate(ENDED_BY_MESSAGE, End::Message, start);
                    return;
                }
            }
            // Another of the job's messages: the next read sees what it
            // says.
            Ok(()) => {}
            Err(e) => {
                // A port that fails keeps the reads on their schedule.
                if e.code() != windows::core::HRESULT::from_win32(WAIT_TIMEOUT.0) {
                    std::thread::sleep(next.saturating_duration_since(Instant::now()));
                }
            }
        }
    }
}
