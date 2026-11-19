//! The media worker's host in the game (design.md "Threat Model" T02,
//! docs/stage3.md S15): it launches `tapestry-media.exe` in its sandbox
//! ([`launch`]), checks the suspended process before it runs, carries its
//! control pipe (`skin_model::media::control`) and hands it the handles a
//! video needs, each duplicated into it with the least access its use
//! needs ([`Kind`]).
//!
//! The worker's replies are read on a thread of the host's own: its log
//! lines go to the game's log, and every other reply, decoded and so
//! bounded by the protocol, waits for [`Worker::recv`] in a queue of a
//! fixed capacity. A reply that does not decode, or one past the queue's
//! capacity ([`Worker::overflowed`]), ends the reading, as a worker that
//! dies does: the replies then stop for good.
//!
//! Dropping a worker never waits on it: its job is ended and closed after a
//! short grace on a thread of its own, whatever its pipe does, and a `Quit`
//! is sent meanwhile on another, best effort.
//!
//! A watcher ends a worker whose memory reaches its job's cap ([`watch`])
//! and records how each worker ended; the game reads that end
//! ([`Worker::end`]), and each file open in a worker that ended without
//! the game asking gets a strike, two of which keep the file from being
//! opened again in the run ([`strikes`], [`Worker::reap`]). The host knows
//! which file each player has open from the replies the game takes: an
//! open stays the player's until the worker answers that it closed, or
//! refuses the open before it answers its probe.

pub mod handles;
pub mod launch;
pub mod strikes;
pub mod watch;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use skin_model::media::control::{self, Command, EVENT_ACCESS, FILE_ACCESS, Reply, SECTION_ACCESS};
use windows::Win32::Foundation::{
    DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::JobObjects::TerminateJobObject;
use windows::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcess, WaitForSingleObject};
use windows::core::{PCWSTR, w};

use launch::{LaunchError, Setup};
use strikes::{FileId, Strikes};
use watch::{End, Watch};

/// A kind of handle the game hands the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A video's file, read-only.
    File,
    /// A frame ring's section, mapped for reading and writing.
    Section,
    /// A frame ring's free-slot event, waited on.
    Event,
}

impl Kind {
    /// The access the game hands a handle of this kind with, the one the
    /// worker takes it with.
    pub const fn access(self) -> u32 {
        match self {
            Self::File => FILE_ACCESS,
            Self::Section => SECTION_ACCESS,
            Self::Event => EVENT_ACCESS,
        }
    }
}

/// A frame ring's section of `bytes` bytes and its free-slot event, made
/// for handing to the worker: each with a security descriptor whose DACL
/// grants nothing. The game's own handles have every access by their
/// creation, and the worker gets each with the least access its use needs
/// ([`Kind`]); an object without a descriptor would let whoever holds a
/// handle to it duplicate it with any access, a descriptor that grants
/// nothing lets no one widen one.
///
/// # Errors
///
/// When the descriptor, the section or the event cannot be made.
pub fn ring_objects(bytes: u64) -> windows::core::Result<(OwnedHandle, OwnedHandle)> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a nul-terminated SDDL string; the descriptor returned is
    // freed with `LocalFree` below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:"),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    }?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let high = u32::try_from(bytes >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(bytes & 0xffff_ffff).unwrap_or(u32::MAX);
    // SAFETY: an unnamed section backed by the paging file and an unnamed
    // auto-reset event, each owned by the value returned; the descriptor
    // lives through both calls.
    let made = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            Some(&raw const attributes),
            PAGE_READWRITE,
            high,
            low,
            PCWSTR::null(),
        )
        .map(|h| OwnedHandle::from_raw_handle(h.0))
        .and_then(|section| {
            CreateEventW(Some(&raw const attributes), false, false, PCWSTR::null())
                .map(|h| (section, OwnedHandle::from_raw_handle(h.0)))
        })
    };
    // SAFETY: the descriptor `ConvertStringSecurityDescriptor…` allocated.
    let _ = unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
    made
}

/// A handle the host made, closed when dropped.
#[derive(Debug)]
struct Owned(HANDLE);

// SAFETY: a handle is a value any thread of the process may use; the one
// `Owned` holds is used by its owner alone.
unsafe impl Send for Owned {}
// SAFETY: as above; shared, it is only read.
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: a handle this value made, unused after this.
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

/// The worker's answer to its launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// Its FFmpeg's version string.
    pub version: String,
    /// Its FFmpeg's configure line.
    pub configuration: String,
}

/// A player the game opened: its file, and whether the worker has yet to
/// answer its open.
#[derive(Clone, Copy, Debug)]
struct Opened {
    file: FileId,
    /// The worker has not answered the open with its probe or a refusal:
    /// a failure now is the open's refusal, and the player's id is free
    /// again.
    pending: bool,
}

/// How long a dropped worker has to end on its own before its job closes.
const QUIT_GRACE_MS: u32 = 500;

/// A running worker. Dropping it ends the worker without waiting on it
/// (the module's doc).
pub struct Worker {
    /// Writes the commands, the control pipe's game end (taken when the
    /// worker drops).
    commands: Option<std::fs::File>,
    replies: Receiver<Reply>,
    /// The game left more replies unread than the queue holds.
    overflowed: Arc<AtomicBool>,
    hello: Hello,
    process: Owned,
    pid: u32,
    /// The job, whose closing ends the worker (`None` only under a test's
    /// setup without one).
    job: Option<Owned>,
    /// The job's watcher (with the job).
    watch: Option<Watch>,
    /// The file each player the game opened is reading, until the worker
    /// answers that it closed or refuses its open ([`Opened`]).
    open_files: RefCell<BTreeMap<u8, Opened>>,
    /// The window station and desktop the worker runs on, kept while it
    /// runs.
    _desktop: Option<launch::Desktop>,
    #[cfg(test)]
    log: std::sync::Arc<std::sync::Mutex<String>>,
    #[cfg(test)]
    suspended: Vec<handles::Entry>,
    #[cfg(test)]
    pipe_ends: [u64; 2],
}

impl Worker {
    /// Launches the worker in `dir`, which holds `tapestry-media.exe` and
    /// its FFmpeg DLLs, under `setup` (the recipe, [`Setup::recipe`]).
    ///
    /// # Errors
    ///
    /// The step that failed, or that the check found differing from the
    /// setup, with the system's error; the process, if one was made, is
    /// terminated before it runs (or, at the handshake, as it runs).
    pub fn launch(dir: &std::path::Path, setup: &Setup) -> Result<Self, LaunchError> {
        launch::launch(dir, setup)
    }

    /// The worker's hello.
    pub const fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Sends `command`.
    ///
    /// # Errors
    ///
    /// When the pipe is closed: the worker has ended.
    pub fn send(&mut self, command: &Command) -> std::io::Result<()> {
        let bytes = control::frame(&command.encode())
            .ok_or_else(|| std::io::Error::other("a command longer than a frame"))?;
        let commands = self
            .commands
            .as_mut()
            .ok_or_else(|| std::io::Error::other("the worker is dropping"))?;
        commands.write_all(&bytes)?;
        commands.flush()
    }

    /// Whether the worker sent more replies than the game holds unread: its
    /// replies have ended, and it is to be treated as failed.
    pub fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Acquire)
    }

    /// The next reply other than a log line, waiting up to `timeout`.
    ///
    /// # Errors
    ///
    /// `Timeout` when none came in time; `Disconnected` once the replies
    /// have ended (the worker ended or sent one that does not decode).
    pub fn recv(&self, timeout: Duration) -> Result<Reply, RecvTimeoutError> {
        let reply = self.replies.recv_timeout(timeout)?;
        self.track(&reply);
        Ok(reply)
    }

    /// A reply other than a log line that has come, without waiting.
    pub fn try_recv(&self) -> Option<Reply> {
        let reply = self.replies.try_recv().ok()?;
        self.track(&reply);
        Some(reply)
    }

    /// Follows the players' files through `reply`: a player that closed no
    /// longer has its file open, nor does one whose open the worker refused
    /// before answering it; one whose open the worker answered (its probe
    /// or the file's refusal) is no longer pending.
    fn track(&self, reply: &Reply) {
        let (player, gone) = match *reply {
            Reply::Closed { player } => (player, true),
            Reply::Failed { player, .. } => (player, false),
            Reply::Probed { player, .. } | Reply::Refused { player, .. } => {
                if let Some(opened) = self.open_files.borrow_mut().get_mut(&player) {
                    opened.pending = false;
                }
                return;
            }
            _ => return,
        };
        let mut open = self.open_files.borrow_mut();
        if gone || open.get(&player).is_some_and(|o| o.pending) {
            open.remove(&player);
        }
    }

    /// Duplicates `handle`, the game's, into the worker with the access
    /// its `kind` needs: the value a command names it by.
    ///
    /// # Errors
    ///
    /// When the duplication fails.
    pub fn hand_in(&self, handle: HANDLE, kind: Kind) -> windows::core::Result<u64> {
        self.duplicate_in(handle, kind.access())
    }

    /// Duplicates `handle` into the worker with `access`.
    fn duplicate_in(&self, handle: HANDLE, access: u32) -> windows::core::Result<u64> {
        let mut theirs = HANDLE::default();
        // SAFETY: `handle` is open in the game, and the worker's process
        // handle, made by its creation, has every access.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                handle,
                self.process.0,
                &raw mut theirs,
                access,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
        }?;
        Ok(theirs.0.expose_provenance() as u64)
    }

    /// The worker's process id.
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// How the worker ended, once its watcher has seen it end: `None` while
    /// it runs (or without a job, under a test's setup).
    pub fn end(&self) -> Option<End> {
        self.watch.as_ref().and_then(Watch::end)
    }

    /// Opens `file` as player `player`, unless that player is open or the
    /// file has the strikes that keep it from being opened: hands the
    /// worker its handle and sends `Open`, whose reply comes as any other.
    /// The file is the player's until the game takes the worker's answer
    /// that the player closed, or its refusal of the open
    /// ([`Self::recv`]).
    ///
    /// # Errors
    ///
    /// [`OpenError::Occupied`], [`OpenError::Struck`], or the system's
    /// error that kept the file from being handed or the command from
    /// being sent.
    pub fn open(
        &mut self,
        player: u8,
        file: &std::fs::File,
        strikes: &Strikes,
    ) -> Result<(), OpenError> {
        if self.open_files.get_mut().contains_key(&player) {
            return Err(OpenError::Occupied);
        }
        let id = FileId::of(file).map_err(OpenError::System)?;
        if strikes.out(id) {
            return Err(OpenError::Struck);
        }
        let value = self
            .hand_in(HANDLE(file.as_raw_handle()), Kind::File)
            .map_err(OpenError::System)?;
        self.send(&Command::Open {
            player,
            file: value,
        })
        .map_err(OpenError::Pipe)?;
        self.open_files.get_mut().insert(
            player,
            Opened {
                file: id,
                pending: true,
            },
        );
        Ok(())
    }

    /// Asks the worker to close player `player`. Its file stays open in the
    /// worker until the game takes the worker's answer, so a worker that
    /// ends while closing it gives it a strike.
    ///
    /// # Errors
    ///
    /// When the pipe is closed: the worker has ended.
    pub fn close(&mut self, player: u8) -> std::io::Result<()> {
        self.send(&Command::Close { player })
    }

    /// How the worker ended, once its watcher has seen it end: each player
    /// open in it then fails, passed to `failed`, and each file open in it
    /// gets one strike, however many of its players had it open.
    pub fn reap(&mut self, strikes: &mut Strikes, failed: impl FnMut(u8)) -> Option<End> {
        let end = self.end()?;
        let open = std::mem::take(self.open_files.get_mut());
        let files: BTreeSet<FileId> = open.values().map(|o| o.file).collect();
        for file in files {
            strikes.strike(file);
        }
        open.into_keys().for_each(failed);
        Some(end)
    }
}

/// Why a file was not opened.
#[derive(Debug)]
pub enum OpenError {
    /// The player is open: the game has not taken the worker's answer that
    /// it closed.
    Occupied,
    /// It has the strikes that keep it from being opened again in the run.
    Struck,
    /// The system's error reading its identity or handing it in.
    System(windows::core::Error),
    /// The pipe is closed: the worker has ended.
    Pipe(std::io::Error),
}

#[cfg(test)]
impl Worker {
    /// Duplicates `handle` into the worker with `access`, which a test's
    /// control widens.
    pub fn hand_in_with(&self, handle: HANDLE, access: u32) -> windows::core::Result<u64> {
        self.duplicate_in(handle, access)
    }

    /// The worker's process handle, with every access.
    pub const fn process(&self) -> HANDLE {
        self.process.0
    }

    /// The job's handle, if the setup made one.
    pub fn job(&self) -> Option<HANDLE> {
        self.job.as_ref().map(|j| j.0)
    }

    /// The window station and desktop the worker runs on, if the setup made
    /// them.
    pub const fn desktop(&self) -> Option<&launch::Desktop> {
        self._desktop.as_ref()
    }

    /// Closes the job, as the game's exit does, keeping the rest: the
    /// watcher stops first, closing its own handle to the job.
    pub fn close_job(&mut self) {
        if let Some(watch) = self.watch.take() {
            watch.join();
        }
        self.job = None;
    }

    /// Posts the job's `message` about the worker to the job's watcher, as
    /// the job posts it.
    ///
    /// # Errors
    ///
    /// When the worker has no watcher, or the post fails.
    pub fn post_job_message(&self, message: u32) -> windows::core::Result<()> {
        self.watch
            .as_ref()
            .ok_or_else(|| {
                windows::core::Error::from_hresult(windows::Win32::Foundation::E_UNEXPECTED)
            })?
            .post(message, self.pid)
    }

    /// The peak commit of the worker's job, as its watcher reads it.
    pub fn job_peak(&self) -> Option<usize> {
        watch::job_peak(self.job.as_ref()?)
    }

    /// What the job's watcher did so far.
    pub fn watch_events(&self) -> Vec<watch::Event> {
        self.watch.as_ref().map(Watch::events).unwrap_or_default()
    }

    /// Every log line the worker sent so far.
    pub fn log(&self) -> String {
        self.log
            .lock()
            .map(|l| l.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// The handles the suspended worker held when it was checked.
    pub fn suspended(&self) -> &[handles::Entry] {
        &self.suspended
    }

    /// The values of the control pipe's ends it was created with.
    pub const fn pipe_ends(&self) -> [u64; 2] {
        self.pipe_ends
    }

    /// A second handle to the game's end of the command pipe, to write
    /// through from another thread.
    pub fn commands_again(&self) -> std::fs::File {
        use std::os::windows::io::AsRawHandle as _;
        let Some(commands) = &self.commands else {
            panic!("the worker is dropping");
        };
        let mut again = HANDLE::default();
        // SAFETY: the game's end of the pipe, duplicated within the game;
        // the file returned owns the duplicate.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                HANDLE(commands.as_raw_handle()),
                GetCurrentProcess(),
                &raw mut again,
                0,
                false,
                windows::Win32::Foundation::DUPLICATE_SAME_ACCESS,
            )
        }
        .unwrap_or_else(|e| panic!("duplicating the command pipe: {e}"));
        // SAFETY: the duplicate just made, owned by the file from here.
        unsafe { std::fs::File::from_raw_handle(again.0) }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // The watcher stops: the worker's end from here on is the game's.
        drop(self.watch.take());
        // `Quit`, best effort, on a thread of its own: the write waits while
        // the pipe is full, until the worker reads or ends. A thread that
        // does not start drops the pipe, which ends the worker's reading as
        // `Quit` would.
        let commands = self.commands.take();
        let _ = std::thread::Builder::new()
            .name("media worker quit".to_owned())
            .spawn(move || {
                if let Some(mut commands) = commands
                    && let Some(bytes) = control::frame(&Command::Quit.encode())
                {
                    let _ = commands.write_all(&bytes);
                }
            });
        // The job is ended and closed after the grace, on another thread,
        // whatever the write does: ended, so that a watcher still closing
        // its own handle to the job keeps no worker alive, and closed, which
        // ends the worker too (`KILL_ON_JOB_CLOSE`). A thread that does not
        // start closes it at once, as its closure drops.
        let job = self.job.take();
        let process = std::mem::replace(&mut self.process, Owned(HANDLE::default()));
        let _ = std::thread::Builder::new()
            .name("media worker end".to_owned())
            .spawn(move || {
                if let Some(job) = &job {
                    // SAFETY: the worker's process handle and its job's,
                    // which this thread holds.
                    unsafe {
                        let _ = WaitForSingleObject(process.0, QUIT_GRACE_MS);
                        let _ = TerminateJobObject(job.0, 0);
                    }
                }
                drop((job, process));
            });
    }
}
