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
//! Commands are written on a thread of the host's own as well ([`writer`]):
//! sending one only queues it, never waiting, so no game thread waits on a
//! worker that stops reading; the writer publishes how long its write under
//! way has waited. The worker's control loop beats at least every 500 ms
//! ([`Worker::since_beat`]); beats never enter the reply queue.
//!
//! Dropping a worker never waits on it: its job is ended and closed after a
//! short grace on a thread of its own, whatever its pipe does, and a `Quit`
//! is queued for the writer meanwhile, best effort.
//!
//! A watcher ends a worker whose memory reaches its job's cap ([`watch`])
//! and records how each worker ended; the game reads that end
//! ([`Worker::end`]), and each file open in a worker that ended without
//! the game asking gets a strike, two of which keep the file from being
//! opened again in the run ([`strikes`], [`Worker::reap`]). The host knows
//! which file each player has open from the replies to its own commands,
//! matched by the id each command carries ([`players`]).

pub mod handles;
pub mod launch;
pub mod movie;
pub mod placement;
pub mod players;
pub mod session;
pub mod stage;
pub mod strikes;
pub mod verdict;
pub mod watch;
pub mod writer;

use std::cell::{Cell, RefCell};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use skin_model::media::control::{self, Command, EVENT_ACCESS, FILE_ACCESS, Reply, SECTION_ACCESS};
use windows::Win32::Foundation::{
    DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
    WAIT_OBJECT_0,
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
use players::Players;
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

/// The exit code of a worker the game ended for a hang: a missed deadline,
/// a stuck command write, a stalled decoder or no heartbeat.
pub const EXIT_HUNG: u32 = 0xC0DE_0004;

/// How long a dropped worker has to end on its own before its job closes.
const QUIT_GRACE_MS: u32 = 500;

/// A running worker. Dropping it ends the worker without waiting on it
/// (the module's doc).
pub struct Worker {
    /// Writes the commands on the control pipe's game end (taken when the
    /// worker drops).
    commands: Option<writer::Writer>,
    /// The replies, each with its arrival.
    replies: Receiver<(std::time::Instant, Reply)>,
    /// The game left more replies unread than the queue holds.
    overflowed: Arc<AtomicBool>,
    /// The reader holds a reply between its stamp and its queuing.
    in_hand: Arc<AtomicBool>,
    /// When the worker's control loop last beat.
    beats: Arc<Beats>,
    hello: Hello,
    process: Owned,
    pid: u32,
    /// The job, whose closing ends the worker (`None` only under a test's
    /// setup without one).
    job: Option<Owned>,
    /// The job's watcher (with the job).
    watch: Option<Watch>,
    /// Where each player id stands, from the replies the game took.
    players: RefCell<Players<FileId>>,
    /// The game's termination for a hang ([`Self::terminate_hung`]) was
    /// accepted, the worker's process not ended when the game looked.
    hung: Cell<bool>,
    /// The id of the next command: never 0, which no command has.
    next_id: u32,
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
    /// Launches the worker installed in `installation`, which holds
    /// `tapestry-media.exe` and its FFmpeg DLLs, under `setup` (the recipe,
    /// [`Setup::recipe`]), from copies of them staged under the game's data
    /// folder `data` ([`stage`]).
    ///
    /// # Errors
    ///
    /// The step that failed, or that the check found differing from the
    /// setup, with the system's error; the process, if one was made, is
    /// terminated before it runs (or, at the handshake, as it runs).
    pub fn launch(
        installation: &std::path::Path,
        data: &std::path::Path,
        setup: &Setup,
    ) -> Result<Self, LaunchError> {
        launch::launch(installation, data, setup)
    }

    /// The worker's hello.
    pub const fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Queues `command` for the writer, never waiting: its id, which the
    /// replies that answer it echo.
    ///
    /// # Errors
    ///
    /// `WouldBlock` when the writer's queue is full (its write under way may
    /// be stuck), `BrokenPipe` when the writer has ended (the worker ended).
    pub fn send(&mut self, command: &Command) -> std::io::Result<u32> {
        let id = self.next_id;
        self.next_id = id.checked_add(1).unwrap_or(1);
        let bytes = control::frame(&command.encode(id))
            .ok_or_else(|| std::io::Error::other("a command longer than a frame"))?;
        let commands = self
            .commands
            .as_ref()
            .ok_or_else(|| std::io::Error::other("the worker is dropping"))?;
        match commands.send(id, bytes) {
            Ok(()) => Ok(id),
            Err(writer::Refused::Full) => Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "the command queue is full",
            )),
            Err(writer::Refused::Ended) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the command writer has ended",
            )),
        }
    }

    /// What the command writer publishes: how long its write under way has
    /// waited, when each command was written, whether it has ended.
    pub fn transport(&self) -> Option<&std::sync::Arc<writer::Transport>> {
        self.commands.as_ref().map(writer::Writer::transport)
    }

    /// Terminates the worker's job for a hang ([`EXIT_HUNG`]), without
    /// waiting and without its watcher, unless its process has ended
    /// already; a termination the system accepts is recorded here, for
    /// [`Worker::reap`]. A process that has ended is the worker's own end,
    /// whatever its code: nothing is terminated or recorded.
    pub fn terminate_hung(&self) {
        if self.exited() {
            return;
        }
        if let Some(job) = &self.job {
            // SAFETY: the worker's job, which this value holds.
            match unsafe { TerminateJobObject(job.0, EXIT_HUNG) } {
                Ok(()) => self.hung.set(true),
                Err(e) => log::warn!("media worker: its job could not be ended for a hang ({e})"),
            }
        }
    }

    /// Whether the worker sent more replies than the game holds unread: its
    /// replies have ended, and it is to be treated as failed.
    pub fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Acquire)
    }

    /// Whether the reader holds a reply it has stamped and not queued yet.
    /// Read after the queue is drained: when it does not, every reply
    /// stamped before the drain began is queued, so a second drain takes
    /// them all.
    pub fn in_hand(&self) -> bool {
        self.in_hand.load(Ordering::SeqCst)
    }

    /// How long since the worker's control loop last beat (since the
    /// launch's handshake, before its first beat).
    pub fn since_beat(&self) -> Duration {
        self.beats.since()
    }

    /// The next reply other than a log line, waiting up to `timeout`.
    ///
    /// # Errors
    ///
    /// `Timeout` when none came in time; `Disconnected` once the replies
    /// have ended (the worker ended or sent one that does not decode).
    pub fn recv(&self, timeout: Duration) -> Result<Reply, RecvTimeoutError> {
        let (_, reply) = self.replies.recv_timeout(timeout)?;
        self.players.borrow_mut().take(&reply);
        Ok(reply)
    }

    /// A reply other than a log line that has come, without waiting.
    pub fn try_recv(&self) -> Option<Reply> {
        match self.take_reply() {
            Taken::Reply(_, reply) => Some(reply),
            Taken::Empty | Taken::Ended => None,
        }
    }

    /// The next reply that has come, with its arrival, without waiting; or
    /// that none has, or that the replies have ended (the worker ended, or
    /// sent one that does not decode or one past the queue).
    pub fn take_reply(&self) -> Taken {
        match self.replies.try_recv() {
            Ok((at, reply)) => {
                self.players.borrow_mut().take(&reply);
                Taken::Reply(at, reply)
            }
            Err(TryRecvError::Empty) => Taken::Empty,
            Err(TryRecvError::Disconnected) => Taken::Ended,
        }
    }

    /// Whether the worker's process has ended, as its handle says now.
    pub fn exited(&self) -> bool {
        // SAFETY: the worker's process handle, which this value holds.
        (unsafe { WaitForSingleObject(self.process.0, 0) }) == WAIT_OBJECT_0
    }

    /// Whether the worker's tracker still holds player id `player`: its
    /// close not answered by its own id, nor its open rejected.
    pub fn holds(&self, player: u8) -> bool {
        !self.players.borrow().free(player)
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

    /// Opens `file` as player `player`, unless that player id is taken or
    /// the file has the strikes that keep it from being opened: hands the
    /// worker its handle and sends `Open`, whose reply comes as any other:
    /// the open's id. The id stays the player's until the game takes the
    /// worker's answer that the player closed, or its rejection of the open
    /// ([`players`]).
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
    ) -> Result<u32, OpenError> {
        if !self.players.get_mut().free(player) {
            return Err(OpenError::Occupied);
        }
        let id = FileId::of(file).map_err(OpenError::System)?;
        if strikes.out(id) {
            return Err(OpenError::Struck);
        }
        let value = self
            .hand_in(HANDLE(file.as_raw_handle()), Kind::File)
            .map_err(OpenError::System)?;
        // The handle is the worker's from here: the player is opening
        // whether or not the command is sent.
        let open = self.send(&Command::Open {
            player,
            file: value,
        });
        let open = match open {
            Ok(open) => open,
            Err(e) => {
                self.players.get_mut().opened(player, 0, id);
                return Err(OpenError::Pipe(e));
            }
        };
        self.players.get_mut().opened(player, open, id);
        Ok(open)
    }

    /// Asks the worker to close player `player`: the close's id. Its file
    /// stays open in the worker until the game takes the worker's answer
    /// to this close, so a worker that ends while closing it gives it a
    /// strike.
    ///
    /// # Errors
    ///
    /// When the pipe is closed: the worker has ended.
    pub fn close(&mut self, player: u8) -> std::io::Result<u32> {
        let close = self.send(&Command::Close { player })?;
        self.players.get_mut().closing(player, close);
        Ok(close)
    }

    /// Player `player`, whose close is sent, was closed for a reason that
    /// is not its file's (its peer's loss): if the game's termination
    /// ([`Self::terminate_hung`]) ends the worker before the close is
    /// answered, its file gets no strike; if the worker ends on its own, it
    /// does.
    pub fn spare(&mut self, player: u8) {
        self.players.get_mut().spare(player);
    }

    /// How the worker ended, once its watcher has seen it end and the game
    /// has every reply the worker sent: the replies the game left unread
    /// are taken here, then each player of the worker fails, passed to
    /// `failed`, and each file open in it gets one strike, however many of
    /// its players had it open, but a spared player's ([`Self::spare`])
    /// when the game's termination ended it: the game's termination for a
    /// hang was accepted ([`Self::terminate_hung`]) and the worker ended
    /// with its code ([`EXIT_HUNG`]). That code alone proves nothing, and a
    /// worker that ends by itself with it after the game looked and before
    /// its termination was accepted is taken for the game's termination.
    /// `None` until then.
    pub fn reap(&mut self, strikes: &mut Strikes, failed: impl FnMut(u8)) -> Option<End> {
        let end = self.end()?;
        loop {
            match self.replies.try_recv() {
                Ok((_, reply)) => self.players.get_mut().take(&reply),
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        let terminated = self.hung.get() && end == End::Exited(EXIT_HUNG);
        let (files, players) = self.players.get_mut().clear(terminated);
        for file in files {
            strikes.strike(file);
        }
        players.into_iter().for_each(failed);
        Some(end)
    }
}

/// What [`Worker::take_reply`] found.
pub enum Taken {
    /// A reply and its arrival.
    Reply(std::time::Instant, Reply),
    /// None has come.
    Empty,
    /// The replies have ended.
    Ended,
}

/// When a worker's control loop last beat, as its replies' thread saw
/// each [`Reply::Beat`] arrive: beats never enter the reply queue.
pub struct Beats {
    origin: std::time::Instant,
    /// The last beat's arrival, µs after `origin`.
    last: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    all: std::sync::Mutex<Vec<Duration>>,
}

impl Beats {
    fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
            last: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            all: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Records a beat's arrival.
    fn beat(&self) {
        let at = self.origin.elapsed();
        self.last.store(
            u64::try_from(at.as_micros()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
        #[cfg(test)]
        self.all
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(at);
    }

    /// How long since the last beat.
    fn since(&self) -> Duration {
        self.origin
            .elapsed()
            .saturating_sub(Duration::from_micros(self.last.load(Ordering::Acquire)))
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

    /// Where player `player` stands, from the replies the game took.
    pub fn player_state(&self, player: u8) -> Option<players::State<FileId>> {
        self.players.borrow().state(player)
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
        self.post_job_message_about(message, self.pid)
    }

    /// Posts the job's `message` about process `pid` to the job's watcher,
    /// as the job posts it.
    ///
    /// # Errors
    ///
    /// When the worker has no watcher, or the post fails.
    pub fn post_job_message_about(&self, message: u32, pid: u32) -> windows::core::Result<()> {
        self.watch
            .as_ref()
            .ok_or_else(|| {
                windows::core::Error::from_hresult(windows::Win32::Foundation::E_UNEXPECTED)
            })?
            .post(message, pid)
    }

    /// Makes the job's watcher read the job's accounting at once.
    ///
    /// # Errors
    ///
    /// When the worker has no watcher, or the post fails.
    pub fn read_now(&self) -> windows::core::Result<()> {
        self.watch
            .as_ref()
            .ok_or_else(|| {
                windows::core::Error::from_hresult(windows::Win32::Foundation::E_UNEXPECTED)
            })?
            .read_now()
    }

    /// The peak commit of the worker's job, as its watcher reads it.
    pub fn job_peak(&self) -> Option<usize> {
        watch::job_peak(self.job.as_ref()?)
    }

    /// What the job's watcher did so far.
    pub fn watch_events(&self) -> Vec<watch::Event> {
        self.watch.as_ref().map(Watch::events).unwrap_or_default()
    }

    /// Each beat's arrival so far, after the launch's handshake began.
    pub fn beats(&self) -> Vec<Duration> {
        self.beats
            .all
            .lock()
            .map(|b| b.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
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
        let Some(commands) = self.commands.as_ref().map(writer::Writer::pipe) else {
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
        // `Quit`, best effort, queued for the writer, which writes it while
        // the pipe takes it and then ends with its queue: its write waits
        // while the pipe is full, until the worker reads or ends.
        if let Some(commands) = self.commands.take()
            && let Some(bytes) = control::frame(&Command::Quit.encode(self.next_id))
        {
            let _ = commands.send(self.next_id, bytes);
        }
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
