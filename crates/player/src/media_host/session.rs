//! The game's media session (design.md "Threat Model" T02): the worker,
//! launched when a screen first needs a video and kept for the run, and the
//! supervision that turns a video black when the worker cannot be trusted
//! to show it. Standard technique: an asynchronous child supervisor, as a
//! browser's process host runs its IPC: commands go through a queue to a
//! writer thread ([`super::writer`]), replies come from a reader thread,
//! and a session thread of its own judges both on a monotonic clock, while
//! the game thread only hands requests over and reads each video's state.
//!
//! The rules, each judged on the session's clock:
//!
//! - **Transport** (L1): a hand-off from the game that finds the session's
//!   queue full or gone loses the videos it concerns; a command the writer
//!   has been writing for [`Deadlines::transport`], a writer queue full or
//!   gone, a reply that does not decode or one past the reply queue (the
//!   replies' end), fails the whole worker. Beats do not excuse a stuck
//!   write.
//! - **Acknowledgements** (L2): an open, a play or a seek not answered
//!   within [`Deadlines::answer`] of its own command's write, a close within
//!   [`Deadlines::close`], loses that player's video; an answer that
//!   arrives later, by the reader's stamp, counts as none. The worker,
//!   marked, is terminated once no player of it is live. A result moves a
//!   player only when it echoes the command the player waits on
//!   ([`super::movie::answer`]), and a player goes when the worker's
//!   tracker frees its id.
//! - **Seeks**: the render side writes the newest run it wants sought into
//!   its video's want word ([`Movie::want`]) and wakes the session
//!   ([`Waker`]), so a seek starts without waiting for the tick. A wake
//!   unparks the session's thread, apart from its request queue, which
//!   holds opens alone: a wake never crowds out an open, and one that comes
//!   while the session looks ends its next sleep at once. A live player has at most one
//!   seek in flight, for the newest run wanted, from the clip's start;
//!   wants made meanwhile are coalesced and the newest sought once it is
//!   answered. An end the worker reports for the newest run whose seek was
//!   answered becomes the video's ended run ([`Movie::ended`]).
//! - **Progress** (L3): a decoding player whose progress word has not risen
//!   for [`Deadlines::progress`] is lost, a seek in flight or answered
//!   included: a seek keeps the window it found. Waiting for a free slot
//!   excuses it only while every slot holds a frame the game has not freed, by the
//!   game's own count of the slots it freed; from the first look that finds
//!   a slot free the window runs again. An end excuses it only as an end
//!   for good: a looping play never ends.
//! - **Heartbeat** (L4): no beat for [`Deadlines::heartbeat`] fails the
//!   whole worker.
//!
//! A failure of the whole worker loses every one of its players first, each
//! by one atomic step that writes nothing else, then has its job ended for
//! a hang ([`Worker::terminate_hung`]) and reaps it: the files it had open
//! get their strikes, but for a file the session was closing because its
//! peer was lost ([`Loss::Peer`]): a worker the game's termination ended,
//! by the watcher's proof, does not strike it; one that ended on its own
//! does, whatever its exit code. The next video a screen
//! needs launches a new worker. The launch runs on a thread of its own, so a launch held up
//! (another launch staging its copies) holds neither the game nor the
//! session.
//!
//! The session's end, the game's handle dropped, loses every video it holds
//! and every open its queue still holds, none of them launched.
//!
//! Each video's [`Movie`] is shared with the game: the session moves it
//! from pending to absent (refused at its probe), live (its ring set up)
//! or lost; lost is for good, whatever comes later. The screen that asked
//! for it keeps its own preparation deadline (L5, [`Movie::expire`]). Its
//! state is one atomic word ([`super::movie::Cell`]); the session logs the
//! losses after its look, through a thread of its own, so no deadline, no
//! frame and no loss waits on the log's file.

use std::os::windows::io::{AsRawHandle as _, OwnedHandle};
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::AtomicI64;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::Thread;
use std::time::{Duration, Instant};

use skin_model::media::control::{Command, Matrix, Range, Reply};
use skin_model::media::{self, Geometry, Reader, State, Window};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{
    FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows::Win32::System::Threading::SetEvent;

use super::launch::{LaunchError, Setup};
use super::strikes::Strikes;
use super::{Kind, OpenError, Taken, Worker};

/// The session's deadlines (design.md "Threat Model" T02).
#[derive(Clone, Copy, Debug)]
pub struct Deadlines {
    /// An open's or a play's answer, from its write.
    pub answer: Duration,
    /// A close's answer, from its write.
    pub close: Duration,
    /// A command write under way.
    pub transport: Duration,
    /// The worker's control loop's beats.
    pub heartbeat: Duration,
    /// A decoding player's progress.
    pub progress: Duration,
    /// How often the session looks, at most.
    pub tick: Duration,
}

impl Deadlines {
    /// The game's.
    pub const GAME: Self = Self {
        answer: Duration::from_secs(2),
        close: Duration::from_secs(1),
        transport: Duration::from_secs(2),
        heartbeat: Duration::from_secs(2),
        progress: Duration::from_secs(2),
        tick: Duration::from_millis(100),
    };
}

/// A screen's preparation deadline (L5): how long it waits for its videos'
/// first frames from its preparation's start.
pub const PREPARATION: Duration = Duration::from_secs(3);

/// The most players the session holds in a worker at once: the worker's.
const PLAYERS: u8 = 8;

/// The requests the session's queue holds: opens alone.
const REQUESTS: usize = 32;

use super::movie::{Cell, Outcome, Stage, Then};
pub use super::movie::{Loss, Status};

/// A video a screen asked for, shared between the game, the render side
/// and the session. Its state is a [`Cell`]: each thread changes it by one
/// atomic step and writes nothing else on the way (the session logs the
/// losses later, off the game's and the render side's paths).
pub struct Movie {
    cell: Cell,
    live: OnceLock<Live>,
    /// Slots the render side freed.
    freed: AtomicU64,
    /// The newest run the render side wants sought, 0 for none.
    want: AtomicU64,
    /// The newest run whose end the worker reported, 0 for none.
    ended: AtomicU64,
    /// The media time the session's seeks start from (tests; the game's
    /// are 0, the clip's start).
    #[cfg(test)]
    seek_at: AtomicI64,
}

impl Movie {
    /// A pending video of the screen of `generation`.
    pub fn new(generation: u64) -> Arc<Self> {
        Arc::new(Self {
            cell: Cell::new(generation),
            live: OnceLock::new(),
            freed: AtomicU64::new(0),
            want: AtomicU64::new(0),
            ended: AtomicU64::new(0),
            #[cfg(test)]
            seek_at: AtomicI64::new(0),
        })
    }

    /// The render side wants run `run`, which its reader started, sought:
    /// the session seeks the newest run wanted once no seek of the video is
    /// in flight. It writes one word and allocates nothing; the caller then
    /// wakes the session ([`Waker::wake`]).
    pub fn want(&self, run: u64) {
        self.want.store(run, Ordering::Release);
    }

    /// The newest run whose end the worker reported, 0 for none: the
    /// worker published every frame of that run before reporting it.
    pub fn ended(&self) -> u64 {
        self.ended.load(Ordering::Acquire)
    }

    /// Has the session's seeks start at `at` µs instead of the clip's
    /// start (tests).
    #[cfg(test)]
    pub fn seek_from(&self, at: i64) {
        self.seek_at.store(at, Ordering::Release);
    }

    /// Where the session's seeks start: the clip's start.
    fn seek_at(&self) -> i64 {
        #[cfg(test)]
        return self.seek_at.load(Ordering::Acquire);
        #[cfg(not(test))]
        0
    }

    /// Where it stands.
    pub fn status(&self) -> Status {
        self.cell.status()
    }

    /// The generation of the screen that asked for it.
    pub const fn generation(&self) -> u64 {
        self.cell.generation()
    }

    /// Its ring and probe, once live.
    pub fn live(&self) -> Option<&Live> {
        self.live.get()
    }

    /// Loses it for `why`, unless it is lost already: whether this did.
    pub fn lose(&self, why: Loss) -> bool {
        self.cell.lose(why)
    }

    /// Why it was lost first.
    pub fn loss(&self) -> Option<Loss> {
        self.cell.loss()
    }

    /// The screen's preparation deadline has passed (L5): unless its first
    /// frame was staged, it is absent or lost already, it is lost. Called
    /// by the game thread, without waiting.
    pub fn expire(&self) {
        self.cell.expire();
    }

    /// Whether it reached a terminal outcome for readiness: its first frame
    /// staged, absent, or lost.
    pub fn settled(&self) -> bool {
        self.cell.settled()
    }

    /// The render side staged a frame of run 1.
    pub fn staged_first(&self) {
        self.cell.staged_first();
    }

    /// A live video of the screen of `generation`, of `width` × `height`
    /// in BT.709 limited range, its ring mapped in this process with no
    /// worker: the test writes its frames (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn live_here(generation: u64, width: u32, height: u32) -> Arc<Self> {
        let geometry = Geometry::new(width, height).unwrap();
        let ring = Ring::new(geometry).unwrap();
        let window = Window {
            first: 0,
            last: i64::MAX,
        };
        let reader = Reader::new(ring.words(), geometry, window).unwrap();
        let movie = Self::new(generation);
        let live = Live {
            ring,
            reader: Mutex::new(Some(reader)),
            width,
            height,
            duration: i64::MAX,
            matrix: Matrix::Bt709,
            range: Range::Limited,
        };
        assert!(movie.live.set(live).is_ok());
        assert!(movie.cell.go_live());
        movie
    }

    /// The slots the render side's reader has freed over the ring's life
    /// (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn freed_total(&self) -> u64 {
        self.freed.load(Ordering::Acquire)
    }

    /// The render side's reader has freed `total` slots over the ring's
    /// life ([`Reader::freed`]): the count the session's progress rule
    /// reads. A rise signals the worker's event.
    pub fn freed(&self, total: u64) {
        let before = self.freed.swap(total, Ordering::AcqRel);
        if total != before
            && let Some(live) = self.live()
        {
            live.ring.signal();
        }
    }
}

/// A live video's ring and what its probe said.
pub struct Live {
    pub ring: Ring,
    /// The render side's reader, taken once.
    pub reader: Mutex<Option<Reader>>,
    pub width: u32,
    pub height: u32,
    pub duration: i64,
    pub matrix: Matrix,
    pub range: Range,
}

/// A frame ring as the game makes it: a section of its own, mapped here,
/// and its free-slot event.
pub struct Ring {
    section: OwnedHandle,
    event: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

// SAFETY: the view is touched only through the atomics `words` gives; the
// handles are values any thread may use.
unsafe impl Send for Ring {}
// SAFETY: as above.
unsafe impl Sync for Ring {}

impl Ring {
    /// A ring for frames of `geometry`.
    fn new(geometry: Geometry) -> windows::core::Result<Self> {
        let (section, event) =
            super::ring_objects(u64::try_from(geometry.bytes()).unwrap_or(u64::MAX))?;
        // SAFETY: the section just made, mapped whole.
        let view = unsafe {
            MapViewOfFile(
                HANDLE(section.as_raw_handle()),
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                0,
            )
        };
        if view.Value.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        Ok(Self {
            section,
            event,
            view,
            len: geometry.words(),
        })
    }

    /// The ring's words.
    pub fn words(&self) -> &[std::sync::atomic::AtomicU64] {
        // SAFETY: the view holds `len` words, mapped while this value
        // lives, and every access goes through the atomics.
        unsafe { std::slice::from_raw_parts(self.view.Value.cast(), self.len) }
    }

    /// Tells the worker a slot is free.
    pub fn signal(&self) {
        // SAFETY: the ring's event.
        let _ = unsafe { SetEvent(HANDLE(self.event.as_raw_handle())) };
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: the view this ring mapped, unused after this.
        let _ = unsafe { UnmapViewOfFile(self.view) };
    }
}

/// What a screen asks of the session.
enum Request {
    /// Play `file` as `movie`, looping when asked.
    Open {
        movie: Arc<Movie>,
        file: std::fs::File,
        looping: bool,
    },
    /// Fill the worker's command pipe with `len` bytes written apart from
    /// the writer, so the writer's next command waits (a test's).
    #[cfg(test)]
    Stuff(usize),
}

/// What the session tells itself from its other threads.
enum Inner {
    Launched(Result<Worker, LaunchError>),
}

/// How the worker is launched.
#[derive(Clone)]
pub struct Launching {
    pub installation: PathBuf,
    pub data: PathBuf,
    pub setup: Setup,
}

/// What a test reads of the session.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A worker launched, its process id.
    Launched { pid: u32 },
    /// A video lost, for `why`.
    Lost { why: Loss },
    /// A worker's job terminated for a hang (`why`).
    Terminated { pid: u32, why: Loss },
    /// A worker reaped.
    Reaped { pid: u32 },
    /// A seek for `run` sent, as command `id`.
    Seek { run: u64, id: u32 },
    /// A seek for `run` answered, `landed` or not.
    Sought { run: u64, landed: Option<i64> },
    /// Run `run`'s end taken as the video's ended run.
    Ended { run: u64 },
    /// An open handed to the worker.
    Opened,
    /// A player's close answered.
    Closed,
    /// A beat reached the reply queue.
    QueuedBeat,
}

/// What the session shares with the game's handle.
struct Shared {
    /// The generation of the screen being shown.
    generation: AtomicU64,
    /// The worker's process id, 0 without one.
    pid: AtomicU32,
    /// Videos the game's hand-offs lost (L1), for the session to log.
    queue_lost: AtomicU32,
    /// The game's handle has gone: the session ends at its next turn,
    /// whoever holds a [`Waker`].
    quit: AtomicBool,
    /// The worker's writer's account of its writes (tests).
    #[cfg(test)]
    transport: Mutex<Option<Arc<super::writer::Transport>>>,
    #[cfg(test)]
    events: Mutex<Vec<(Duration, Event)>>,
    #[cfg(test)]
    origin: Instant,
    /// The worker's beats so far ([`Worker::beats`]).
    #[cfg(test)]
    beats: Mutex<Vec<Duration>>,
}

impl Shared {
    #[cfg(test)]
    fn event(&self, event: Event) {
        if let Ok(mut events) = self.events.lock() {
            events.push((self.origin.elapsed(), event));
        }
    }
}

/// Records `event` for a test.
macro_rules! event {
    ($shared:expr, $event:expr) => {
        #[cfg(test)]
        $shared.event($event);
    };
}

/// The game's handle on the session. Dropping it ends the session without
/// waiting: its thread ends its worker as it goes.
pub struct Session {
    tx: SyncSender<Request>,
    shared: Arc<Shared>,
    /// The session's thread, unparked by each request and wake.
    thread: Thread,
}

impl Session {
    /// Starts the session, which launches its worker as `launching` says
    /// when a screen first asks for a video.
    ///
    /// # Errors
    ///
    /// When its thread does not start.
    pub fn start(launching: Launching, deadlines: Deadlines) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(REQUESTS);
        let shared = Arc::new(Shared {
            generation: AtomicU64::new(0),
            pid: AtomicU32::new(0),
            queue_lost: AtomicU32::new(0),
            quit: AtomicBool::new(false),
            #[cfg(test)]
            transport: Mutex::new(None),
            #[cfg(test)]
            events: Mutex::new(Vec::new()),
            #[cfg(test)]
            origin: Instant::now(),
            #[cfg(test)]
            beats: Mutex::new(Vec::new()),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("media session".to_owned())
                .spawn(move || Supervisor::new(launching, deadlines, shared).run(&rx))?
                .thread()
                .clone()
        };
        Ok(Self { tx, shared, thread })
    }

    /// Shows the screen of `generation`: the videos of other generations
    /// are closed. Never waits.
    pub fn enter(&self, generation: u64) {
        // The cancellation of the screen left: ordered with the session's
        // reads after an answer (`movie::confirm`).
        self.shared.generation.store(generation, Ordering::SeqCst);
    }

    /// Asks for `file` to be played as `movie`, looping when asked. Never
    /// waits: a full or gone queue loses the video (L1).
    pub fn open(&self, movie: &Arc<Movie>, file: std::fs::File, looping: bool) {
        let request = Request::Open {
            movie: Arc::clone(movie),
            file,
            looping,
        };
        match self.tx.try_send(request) {
            Ok(()) => self.thread.unpark(),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                if movie.lose(Loss::Queue) {
                    self.shared.queue_lost.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// The worker's process id, 0 without one.
    pub fn pid(&self) -> u32 {
        self.shared.pid.load(Ordering::Acquire)
    }

    /// The render side's handle for waking the session when a video wants
    /// a seek.
    pub fn waker(&self) -> Waker {
        Waker {
            session: self.thread.clone(),
        }
    }

    /// When command `id` was written whole, on the clock of
    /// [`Self::events`], once the worker's writer has written it (tests).
    #[cfg(test)]
    pub fn written(&self, id: u32) -> Option<Duration> {
        let transport = self.shared.transport.lock().ok()?.clone()?;
        let at = transport.written(id).ok()??;
        Some(at.saturating_duration_since(self.shared.origin))
    }

    /// The time on the clock of [`Self::events`] (tests).
    #[cfg(test)]
    pub fn now(&self) -> Duration {
        self.shared.origin.elapsed()
    }

    /// The beats of the worker so far, after its launch's handshake began.
    #[cfg(test)]
    pub fn beats(&self) -> Vec<Duration> {
        self.shared
            .beats
            .lock()
            .map(|b| b.clone())
            .unwrap_or_default()
    }

    /// Has the session fill its worker's command pipe with `len` bytes
    /// written apart from the writer, which then waits on its next command
    /// while the worker reads nothing.
    #[cfg(test)]
    pub fn stuff(&self, len: usize) {
        if self.tx.try_send(Request::Stuff(len)).is_ok() {
            self.thread.unpark();
        }
    }

    /// What the session did so far.
    #[cfg(test)]
    pub fn events(&self) -> Vec<(Duration, Event)> {
        self.shared
            .events
            .lock()
            .map(|e| e.clone())
            .unwrap_or_default()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The flag ends the session at its next turn, which the unpark
        // starts at once.
        self.shared.quit.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

/// Wakes the session: its thread, unparked apart from the request queue.
/// Holding one keeps no session alive: dropping the game's handle ends it.
#[derive(Clone)]
pub struct Waker {
    session: Thread,
}

impl Waker {
    /// Asks the session to look now, never waiting and allocating nothing:
    /// it unparks the session's thread, so wakes take no place of the
    /// request queue. Wakes coalesce into the thread's one token: a wake
    /// while the session looks ends its next sleep at once, so a want
    /// stored before the wake is read by a look after it.
    pub fn wake(&self) {
        self.session.unpark();
    }
}

/// A player of the session's.
struct Player {
    id: u8,
    movie: Arc<Movie>,
    file: Option<std::fs::File>,
    looping: bool,
    stage: Stage,
    /// When the command the stage waits on was queued.
    queued: Instant,
    /// When that command was written whole, once the session has seen it
    /// written: each answer's deadline runs from its own command's write.
    written: Option<Instant>,
    /// The progress word's last value and when it rose (or the window last
    /// restarted).
    progress: (u64, Instant),
    /// Its video's loss has been logged.
    noted: bool,
    /// Its probe, answered and held back by the screen's pixel total until
    /// every earlier video of its screen has its ring or is gone
    /// ([`Supervisor::admit_probe`]).
    held: Option<Probe>,
    /// The newest run answered sought or played: run 1, played, until a
    /// seek's answer.
    sought: u64,
}

/// What a probe said of a video, as its ring is made from it.
#[derive(Clone, Copy, Debug)]
struct Probe {
    width: u32,
    height: u32,
    duration: i64,
    matrix: Matrix,
    range: Range,
}

/// The pixel total's verdict on a probed video.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// An earlier video of its screen has no ring yet and is not gone:
    /// asked again at the next look.
    Defer,
    /// It fits with the rings its screen holds: its ring is made.
    Admit,
    /// It passes the total: refused before any ring, it draws nothing.
    Refuse,
}

/// Judges a probed video of `pixels` luma pixels on a screen whose rings
/// made so far hold `charged` luma pixels, `earlier_pending` when an earlier
/// video of the screen, in its skin's declaration order, has no ring yet and
/// is not gone: deferred then, admitted while the total stays within
/// [`skin_model::movie::MAX_MOVIE_PIXELS`], refused past it. A ring once
/// made stays charged for the rest of its screen, lost or not, so no change
/// of a video's state frees pixels whose ring may still be mapped.
pub fn total(earlier_pending: bool, charged: u64, pixels: u64) -> Verdict {
    if earlier_pending {
        Verdict::Defer
    } else if charged.saturating_add(pixels) <= skin_model::movie::MAX_MOVIE_PIXELS {
        Verdict::Admit
    } else {
        Verdict::Refuse
    }
}

impl Player {
    /// Whether it still counts as live for the worker's marked end.
    fn live(&self) -> bool {
        !matches!(self.stage, Stage::Closing(_) | Stage::Idle)
            && self.movie.status() != Status::Lost
    }

    /// The command its stage waits on, and how long its answer may take.
    fn awaited(&self, deadlines: &Deadlines) -> Option<(u32, Duration)> {
        // A probe held back was answered: nothing is awaited of the worker.
        if self.held.is_some() {
            return None;
        }
        match self.stage {
            Stage::Opening(id) | Stage::Playing(id) | Stage::Seeking { id, .. } => {
                Some((id, deadlines.answer))
            }
            Stage::Closing(id) => Some((id, deadlines.close)),
            Stage::Queued | Stage::Live | Stage::Idle => None,
        }
    }

    /// Its stage waits on command `id`, queued now.
    fn wait_on(&mut self, stage: Stage) {
        self.stage = stage;
        self.queued = Instant::now();
        self.written = None;
    }
}

/// The session's log lines, written by a thread of their own: no loss, no
/// deadline and no answer waits on the log's file.
struct Notes {
    tx: SyncSender<String>,
    /// Lines dropped since the last one queued, the queue being full.
    dropped: u32,
}

impl Notes {
    /// The lines the queue holds.
    const LINES: usize = 64;

    fn start() -> Self {
        let (tx, rx) = mpsc::sync_channel::<String>(Self::LINES);
        let spawned = std::thread::Builder::new()
            .name("media session log".to_owned())
            .spawn(move || {
                while let Ok(line) = rx.recv() {
                    log::warn!("media session: {line}");
                }
            });
        if let Err(e) = spawned {
            log::warn!("media session: its log's thread did not start ({e})");
        }
        Self { tx, dropped: 0 }
    }

    /// Queues `line`, never waiting.
    fn note(&mut self, line: String) {
        if self.dropped > 0 {
            let dropped = format!("{} lines dropped", self.dropped);
            if self.tx.try_send(dropped).is_err() {
                self.dropped = self.dropped.saturating_add(1);
                return;
            }
            self.dropped = 0;
        }
        if self.tx.try_send(line).is_err() {
            self.dropped = self.dropped.saturating_add(1);
        }
    }
}

/// The session's thread.
struct Supervisor {
    launching: Launching,
    deadlines: Deadlines,
    shared: Arc<Shared>,
    inner_tx: SyncSender<Inner>,
    inner_rx: Receiver<Inner>,
    worker: Option<Worker>,
    launching_now: bool,
    /// The worker is marked: it is terminated once no player is live.
    marked: bool,
    players: Vec<Player>,
    /// Workers terminated and not yet reaped.
    dying: Vec<Worker>,
    strikes: Strikes,
    notes: Notes,
    /// The game's hand-off losses logged so far.
    queue_noted: u32,
    /// The screen (its generation) whose rings were made last, and the luma
    /// pixels of every ring made for it: the pixel total's charge.
    charged: (u64, u64),
    /// Every ring's creation fails (tests).
    #[cfg(test)]
    ring_fails: bool,
    /// The commands sent with no worker (tests).
    #[cfg(test)]
    sent: Vec<Command>,
    /// Runs at each turn's held points (a test's).
    #[cfg(test)]
    turn: Option<Box<dyn FnMut(Held) + Send>>,
}

/// Where a test holds the session's turn.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    /// The requests are taken; the session has not read its end.
    Taken,
    /// The session has looked and not slept yet.
    Looked,
}

impl Supervisor {
    fn new(launching: Launching, deadlines: Deadlines, shared: Arc<Shared>) -> Self {
        let (inner_tx, inner_rx) = mpsc::sync_channel(4);
        Self {
            launching,
            deadlines,
            shared,
            inner_tx,
            inner_rx,
            worker: None,
            launching_now: false,
            marked: false,
            players: Vec::new(),
            dying: Vec::new(),
            strikes: Strikes::default(),
            notes: Notes::start(),
            queue_noted: 0,
            charged: (0, 0),
            #[cfg(test)]
            ring_fails: false,
            #[cfg(test)]
            sent: Vec::new(),
            #[cfg(test)]
            turn: None,
        }
    }

    fn run(mut self, rx: &Receiver<Request>) {
        loop {
            // The requests waiting before the look, at most a queue's
            // worth, so no sender keeps the session from looking.
            let mut gone = false;
            for _ in 0..REQUESTS {
                match rx.try_recv() {
                    Ok(request) => self.request(request),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        gone = true;
                        break;
                    }
                }
            }
            #[cfg(test)]
            if let Some(turn) = &mut self.turn {
                turn(Held::Taken);
            }
            if gone || self.shared.quit.load(Ordering::Acquire) {
                break;
            }
            self.look();
            self.note_losses();
            #[cfg(test)]
            if let Some(turn) = &mut self.turn {
                turn(Held::Looked);
            }
            // Until a request, a wake or the end unparks the thread, or the
            // tick: one that came since the last sleep ends this one at
            // once, so none is lost.
            std::thread::park_timeout(self.deadlines.tick);
        }
        self.quit(rx);
    }

    /// Takes `request`.
    fn request(&mut self, request: Request) {
        match request {
            Request::Open {
                movie,
                file,
                looping,
            } => self.queue(movie, file, looping),
            #[cfg(test)]
            Request::Stuff(len) => self.stuff(len),
        }
    }

    /// Writes `len` bytes on the worker's command pipe from a thread of its
    /// own, apart from the writer (a test's).
    #[cfg(test)]
    fn stuff(&self, len: usize) {
        use std::io::Write as _;
        if let Some(worker) = &self.worker {
            let mut pipe = worker.commands_again();
            std::thread::spawn(move || {
                let _ = pipe.write_all(&vec![0; len]);
            });
        }
    }

    /// Queues an open until the worker can take it.
    fn queue(&mut self, movie: Arc<Movie>, file: std::fs::File, looping: bool) {
        if movie.status() == Status::Lost {
            return;
        }
        let now = Instant::now();
        self.players.push(Player {
            id: 0,
            movie,
            file: Some(file),
            looping,
            stage: Stage::Queued,
            queued: now,
            written: None,
            progress: (0, now),
            noted: false,
            held: None,
            sought: 1,
        });
    }

    /// One look at everything.
    fn look(&mut self) {
        self.take_launch();
        self.reap();
        let generation = self.shared.generation.load(Ordering::Acquire);
        // Queued players of another screen, or lost, are dropped.
        self.players.retain(|p| {
            !(p.stage == Stage::Queued
                && (p.movie.generation() != generation || p.movie.status() == Status::Lost))
        });
        if self.players.iter().any(|p| p.stage == Stage::Queued)
            && self.worker.is_none()
            && !self.launching_now
        {
            self.launch();
        }
        if self.worker.is_none() {
            return;
        }
        #[cfg(test)]
        if let Some(worker) = &self.worker
            && let Ok(mut beats) = self.shared.beats.lock()
        {
            *beats = worker.beats();
        }
        if let Some(why) = self.worker_failure() {
            self.fail_worker(why);
            return;
        }
        // One order for answers and their deadlines: the deadlines are
        // judged at `now`, after every reply that arrived before `now` is
        // taken, each judged late or in time by its own arrival. A reply
        // the reader holds between its stamp and its queue defers the
        // deadlines to the next look.
        let now = Instant::now();
        self.take_replies();
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        let settled = !worker.in_hand();
        self.take_replies();
        if self.worker.is_none() {
            return;
        }
        self.start_queued();
        self.retry_held();
        if self.worker.is_none() {
            return;
        }
        if let Err(why) = self.seek_wanted(generation) {
            self.fail_worker(why);
            return;
        }
        self.judge(generation, now, settled);
        if self.marked && !self.players.iter().any(Player::live) {
            self.fail_worker(Loss::Answer);
        }
    }

    /// Logs the losses not logged yet: the session's own, the game's and
    /// the render side's, after the look that judged them.
    fn note_losses(&mut self) {
        let queue = self.shared.queue_lost.load(Ordering::Relaxed);
        if queue != self.queue_noted {
            let n = queue.wrapping_sub(self.queue_noted);
            self.queue_noted = queue;
            self.notes.note(format!(
                "{n} videos lost at the hand-off ({:?})",
                Loss::Queue
            ));
        }
        for p in &mut self.players {
            if !p.noted
                && let Some(why) = p.movie.loss()
            {
                p.noted = true;
                self.notes.note(why.line());
            }
        }
    }

    /// Starts the launch on a thread of its own.
    fn launch(&mut self) {
        let launching = self.launching.clone();
        let tx = self.inner_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("media worker launch".to_owned())
            .spawn(move || {
                let launched =
                    Worker::launch(&launching.installation, &launching.data, &launching.setup);
                let _ = tx.send(Inner::Launched(launched));
            });
        match spawned {
            Ok(_) => self.launching_now = true,
            Err(e) => {
                self.lose_queued(Loss::Launch);
                self.notes
                    .note(format!("the launch's thread did not start ({e})"));
            }
        }
    }

    /// Takes a finished launch.
    fn take_launch(&mut self) {
        let Ok(Inner::Launched(launched)) = self.inner_rx.try_recv() else {
            return;
        };
        self.launching_now = false;
        match launched {
            Ok(worker) => {
                self.shared.pid.store(worker.pid(), Ordering::Release);
                event!(self.shared, Event::Launched { pid: worker.pid() });
                #[cfg(test)]
                if let Ok(mut transport) = self.shared.transport.lock() {
                    *transport = worker.transport().cloned();
                }
                self.notes.note(format!("worker {} launched", worker.pid()));
                self.worker = Some(worker);
                self.marked = false;
            }
            Err(e) => {
                self.lose_queued(Loss::Launch);
                self.notes
                    .note(format!("the worker could not be launched ({e})"));
            }
        }
    }

    /// Loses every queued player, then drops them, logged.
    fn lose_queued(&mut self, why: Loss) {
        for p in &self.players {
            if p.stage == Stage::Queued && p.movie.lose(why) {
                event!(self.shared, Event::Lost { why });
            }
        }
        self.note_losses();
        self.players.retain(|p| p.stage != Stage::Queued);
    }

    /// Why the whole worker fails, if it does (L1, L4, its end).
    fn worker_failure(&self) -> Option<Loss> {
        let worker = self.worker.as_ref()?;
        if worker.end().is_some() {
            return Some(Loss::Ended);
        }
        if worker.overflowed() {
            return Some(Loss::Transport);
        }
        match worker.transport() {
            Some(t) if t.ended() => return Some(Loss::Transport),
            Some(t) if t.stuck_for().is_some_and(|d| d > self.deadlines.transport) => {
                return Some(Loss::Transport);
            }
            Some(_) => {}
            None => return Some(Loss::Transport),
        }
        if worker.since_beat() > self.deadlines.heartbeat {
            return Some(Loss::Heartbeat);
        }
        None
    }

    /// Fails the whole worker: every player of it lost first, each by one
    /// atomic step, then its job terminated, to be reaped; the losses are
    /// logged after.
    fn fail_worker(&mut self, why: Loss) {
        for p in &self.players {
            if p.stage != Stage::Queued && p.movie.lose(why) {
                event!(self.shared, Event::Lost { why });
            }
        }
        let pid = self.worker.as_ref().map(Worker::pid);
        if let Some(worker) = self.worker.take() {
            worker.terminate_hung();
            event!(
                self.shared,
                Event::Terminated {
                    pid: worker.pid(),
                    why
                }
            );
            self.shared.pid.store(0, Ordering::Release);
            self.dying.push(worker);
        }
        self.marked = false;
        self.note_losses();
        self.players.retain(|p| p.stage == Stage::Queued);
        if let Some(pid) = pid {
            self.notes.note(format!("worker {pid} fails ({why:?})"));
        }
    }

    /// Reaps the workers that have ended.
    fn reap(&mut self) {
        let strikes = &mut self.strikes;
        self.dying.retain_mut(|w| {
            let reaped = w.reap(strikes, |_| {}).is_some();
            if reaped {
                event!(self.shared, Event::Reaped { pid: w.pid() });
            }
            !reaped
        });
    }

    /// A free player id.
    fn free_id(&self) -> Option<u8> {
        (1..=PLAYERS).find(|id| self.players.iter().all(|p| p.id != *id))
    }

    /// Hands the queued players to the worker.
    fn start_queued(&mut self) {
        let mut failure = None;
        for i in 0..self.players.len() {
            let Some(p) = self.players.get(i) else { break };
            if p.stage != Stage::Queued {
                continue;
            }
            let Some(id) = self.free_id() else { break };
            let (Some(worker), Some(p)) = (self.worker.as_mut(), self.players.get_mut(i)) else {
                break;
            };
            let Some(file) = p.file.take() else { continue };
            match worker.open(id, &file, &self.strikes) {
                Ok(open) => {
                    p.id = id;
                    p.wait_on(Stage::Opening(open));
                    event!(self.shared, Event::Opened);
                }
                Err(OpenError::Struck) => {
                    p.movie.lose(Loss::Struck);
                    p.stage = Stage::Idle;
                }
                Err(OpenError::Pipe(_)) => {
                    // The handle is the worker's; the queue would not take
                    // the command.
                    p.id = id;
                    p.stage = Stage::Idle;
                    p.movie.lose(Loss::Transport);
                    failure = Some(Loss::Transport);
                }
                Err(OpenError::Occupied | OpenError::System(_)) => {
                    p.movie.lose(Loss::Open);
                    p.stage = Stage::Idle;
                }
            }
        }
        // Players that never reached the worker go, logged.
        self.note_losses();
        self.players
            .retain(|p| !(p.stage == Stage::Idle && p.id == 0));
        if let Some(why) = failure {
            self.fail_worker(why);
        }
    }

    /// Takes every reply that has come; the replies' end fails the worker
    /// (L1).
    fn take_replies(&mut self) {
        loop {
            let Some(worker) = self.worker.as_ref() else {
                return;
            };
            match worker.take_reply() {
                Taken::Reply(at, reply) => {
                    if let Err(why) = self.take(at, &reply) {
                        self.fail_worker(why);
                        return;
                    }
                    self.forget_freed();
                }
                Taken::Empty => return,
                Taken::Ended => {
                    let why = if worker.exited() {
                        Loss::Ended
                    } else {
                        Loss::Transport
                    };
                    self.fail_worker(why);
                    return;
                }
            }
        }
    }

    /// When command `id`, which player `i` waits on, was written whole,
    /// kept from the first look that sees it: `None` while it is not. A
    /// time no longer kept is taken as the command's queuing, earlier.
    fn written(&mut self, i: usize, id: u32) -> Option<Instant> {
        let transport = self.worker.as_ref().and_then(Worker::transport).cloned();
        let p = self.players.get_mut(i)?;
        if p.written.is_none() {
            p.written = match transport.map(|t| t.written(id)) {
                Some(Ok(at)) => at,
                Some(Err(())) => Some(p.queued),
                None => None,
            };
        }
        p.written
    }

    /// Takes one reply, which arrived at `at`.
    fn take(&mut self, at: Instant, reply: &Reply) -> Result<(), Loss> {
        #[cfg(test)]
        if matches!(reply, Reply::Beat) {
            self.shared.event(Event::QueuedBeat);
        }
        let (player, outcome) = match *reply {
            Reply::Probed { player, id, .. } => (player, Some(Outcome::Probed(id))),
            Reply::Refused { player, id, .. } => (player, Some(Outcome::Refused(id))),
            Reply::Playing { player, id, .. } => (player, Some(Outcome::Playing(id))),
            Reply::Rejected { player, id, .. } => (player, Some(Outcome::Rejected(id))),
            Reply::Failed { player, .. } => (player, Some(Outcome::Failed)),
            Reply::Sought {
                player,
                id,
                run,
                landed,
                ..
            } => {
                event!(self.shared, Event::Sought { run, landed });
                #[cfg(not(test))]
                let _ = landed;
                (player, Some(Outcome::Sought { id, run }))
            }
            Reply::Ended { player, run } => (player, Some(Outcome::Ended(run))),
            Reply::Closed { player, id, .. } => {
                self.closed(player, id, at);
                return Ok(());
            }
            Reply::Hello { .. } | Reply::Log { .. } | Reply::Beat => return Ok(()),
        };
        let Some(outcome) = outcome else {
            return Ok(());
        };
        let Some(i) = self.players.iter().position(|p| p.id == player) else {
            return Ok(());
        };
        let late = self.late(i, at);
        let generation = self.shared.generation.load(Ordering::Acquire);
        let Some(p) = self.players.get_mut(i) else {
            return Ok(());
        };
        #[cfg(test)]
        let before = p.movie.status();
        let (stage, then) =
            super::movie::answer(p.stage, &p.movie.cell, generation, outcome, late, p.sought);
        // The screen shown, read again after the answer's step.
        let generation = self.shared.generation.load(Ordering::SeqCst);
        let (stage, then) = super::movie::confirm(stage, &p.movie.cell, generation, then);
        if stage == Stage::Live && matches!(p.stage, Stage::Playing(_)) {
            // The progress window starts when playback does; a seek keeps
            // the window it found, its last count and time.
            p.progress = (0, Instant::now());
        }
        if let (Stage::Seeking { run, .. }, Stage::Live) = (p.stage, stage) {
            p.sought = run;
        }
        p.stage = stage;
        #[cfg(test)]
        if before != Status::Lost
            && let Some(why) = p.movie.loss()
        {
            self.shared.event(Event::Lost { why });
        }
        match then {
            Then::Nothing => Ok(()),
            Then::End(run) => {
                p.movie.ended.store(run, Ordering::Release);
                event!(self.shared, Event::Ended { run });
                Ok(())
            }
            Then::Close => self.close(i),
            Then::Mark => {
                self.marked = true;
                Ok(())
            }
            Then::Late => {
                // A missed answer on a working transport: the worker is
                // ended once nothing of it is live.
                self.marked = true;
                self.close(i)
            }
            Then::Play => match *reply {
                Reply::Probed {
                    width,
                    height,
                    duration,
                    matrix,
                    range,
                    ..
                } => self.admit_probe(
                    i,
                    Probe {
                        width,
                        height,
                        duration,
                        matrix,
                        range,
                    },
                ),
                _ => Ok(()),
            },
        }
    }

    /// Whether the answer to the command player `i` waits on, arriving at
    /// `at`, came past its deadline.
    fn late(&mut self, i: usize, at: Instant) -> bool {
        let Some((id, limit)) = self.players.get(i).and_then(|p| p.awaited(&self.deadlines)) else {
            return false;
        };
        self.written(i, id)
            .is_some_and(|written| at.saturating_duration_since(written) > limit)
    }

    /// Close `id` of `player` was answered at `at`: past its deadline, the
    /// video is lost and the worker marked. The player itself goes when the
    /// worker's tracker frees its id ([`Self::forget_freed`]).
    fn closed(&mut self, player: u8, id: u32, at: Instant) {
        let Some(i) = self
            .players
            .iter()
            .position(|p| p.id == player && p.stage == Stage::Closing(id))
        else {
            return;
        };
        if self.late(i, at) {
            self.marked = true;
            if let Some(p) = self.players.get(i)
                && p.movie.lose(Loss::Answer)
            {
                event!(self.shared, Event::Lost { why: Loss::Answer });
            }
        }
    }

    /// Drops the players whose ids the worker's tracker has freed (their
    /// close answered by its own id, or their open rejected): a video of
    /// the screen shown whose player is gone is lost.
    fn forget_freed(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        let generation = self.shared.generation.load(Ordering::Acquire);
        let mut any = false;
        for p in &mut self.players {
            if p.stage == Stage::Queued || worker.holds(p.id) {
                continue;
            }
            any = true;
            if matches!(p.stage, Stage::Closing(_)) {
                event!(self.shared, Event::Closed);
            }
            if p.movie.generation() == generation
                && p.movie.status() != Status::Absent
                && p.movie.lose(Loss::Failed)
            {
                event!(self.shared, Event::Lost { why: Loss::Failed });
            }
            p.stage = Stage::Idle;
            p.id = 0;
        }
        if any {
            self.note_losses();
            self.players
                .retain(|p| !(p.stage == Stage::Idle && p.id == 0));
        }
    }

    /// Applies the screen's pixel total to player `i`'s `probe` before any
    /// ring is made for it ([`total`]): the earlier videos of its screen are
    /// the players queued before it with its generation, in the order the
    /// screen asked for them (its skin's declaration order). Held back while
    /// one of them has no ring yet and is not gone; refused past the total,
    /// lost for [`Loss::OverTotal`] and closed with no ring; else its ring
    /// is made, charged once it exists, and its play sent.
    fn admit_probe(&mut self, i: usize, probe: Probe) -> Result<(), Loss> {
        let Some(p) = self.players.get(i) else {
            return Ok(());
        };
        let generation = p.movie.generation();
        let earlier_pending = self.players.iter().take(i).any(|q| {
            q.movie.generation() == generation
                && q.movie.status() == Status::Pending
                && q.movie.live().is_none()
        });
        if self.charged.0 != generation {
            self.charged = (generation, 0);
        }
        let pixels = u64::from(probe.width) * u64::from(probe.height);
        match total(earlier_pending, self.charged.1, pixels) {
            Verdict::Defer => {
                if let Some(p) = self.players.get_mut(i) {
                    p.held = Some(probe);
                }
                Ok(())
            }
            Verdict::Refuse => {
                if let Some(p) = self.players.get(i)
                    && p.movie.lose(Loss::OverTotal)
                {
                    event!(
                        self.shared,
                        Event::Lost {
                            why: Loss::OverTotal
                        }
                    );
                }
                self.close(i)
            }
            Verdict::Admit => self.play(i, probe),
        }
    }

    /// Asks the pixel total again of every probe held back, in the
    /// screen's order: one of a screen no longer shown, or whose video is
    /// no longer pending, is closed instead. The screen shown is read at
    /// each admission, after the replies were taken, so a screen left
    /// meanwhile gets no ring.
    fn retry_held(&mut self) {
        for i in 0..self.players.len() {
            let generation = self.shared.generation.load(Ordering::Acquire);
            let Some(p) = self.players.get_mut(i) else {
                break;
            };
            let Some(probe) = p.held.take() else {
                continue;
            };
            let done = if p.movie.generation() == generation && p.movie.status() == Status::Pending
            {
                self.admit_probe(i, probe)
            } else {
                self.close(i)
            };
            if let Err(why) = done {
                self.fail_worker(why);
                return;
            }
        }
    }

    /// Sets up player `i`'s ring for its `probe`, charges its pixels to
    /// the screen once the ring exists (a ring whose creation failed spends
    /// nothing), and sends its play.
    fn play(&mut self, i: usize, probe: Probe) -> Result<(), Loss> {
        let Probe {
            width,
            height,
            duration,
            matrix,
            range,
        } = probe;
        let Some(geometry) = Geometry::new(width, height) else {
            if let Some(p) = self.players.get_mut(i) {
                p.movie.lose(Loss::Failed);
            }
            return self.close(i);
        };
        let made = Ring::new(geometry).ok().and_then(|ring| {
            let reader = Reader::new(
                ring.words(),
                geometry,
                Window {
                    first: 0,
                    last: i64::MAX,
                },
            )
            .ok()?;
            Some(Live {
                ring,
                reader: Mutex::new(Some(reader)),
                width,
                height,
                duration,
                matrix,
                range,
            })
        });
        #[cfg(test)]
        let made = made.filter(|_| !self.ring_fails);
        if made.is_some() {
            let pixels = u64::from(width) * u64::from(height);
            self.charged.1 = self.charged.1.saturating_add(pixels);
        }
        let (Some(p), Some(worker)) = (self.players.get_mut(i), self.worker.as_mut()) else {
            return Ok(());
        };
        let Some(live) = made else {
            p.movie.lose(Loss::Failed);
            return self.close(i);
        };
        let handles = worker
            .hand_in(HANDLE(live.ring.section.as_raw_handle()), Kind::Section)
            .and_then(|s| {
                worker
                    .hand_in(HANDLE(live.ring.event.as_raw_handle()), Kind::Event)
                    .map(|e| (s, e))
            });
        let Ok((section, event)) = handles else {
            p.movie.lose(Loss::Failed);
            return self.close(i);
        };
        if p.movie.live.set(live).is_err() {
            p.movie.lose(Loss::Failed);
            return self.close(i);
        }
        let looping = p.looping;
        let id = p.id;
        match worker.send(&Command::Play {
            player: id,
            section,
            event,
            looping,
        }) {
            Ok(play) => {
                p.wait_on(Stage::Playing(play));
                Ok(())
            }
            Err(_) => Err(Loss::Transport),
        }
    }

    /// Sends each live player of the screen shown whose video wants a run
    /// newer than its newest sought, and has no seek in flight, a seek for
    /// that run from the clip's start ([`super::movie::seek_for`]).
    fn seek_wanted(&mut self, generation: u64) -> Result<(), Loss> {
        for i in 0..self.players.len() {
            let Some(p) = self.players.get(i) else { break };
            let want = p.movie.want.load(Ordering::Acquire);
            let Some(run) = super::movie::seek_for(p.stage, p.sought, want) else {
                continue;
            };
            if p.movie.generation() != generation || p.movie.status() != Status::Live {
                continue;
            }
            let seek = Command::Seek {
                player: p.id,
                run,
                at: p.movie.seek_at(),
            };
            let id = self.send(&seek)?;
            if let Some(p) = self.players.get_mut(i) {
                p.wait_on(Stage::Seeking { id, run });
            }
            event!(self.shared, Event::Seek { run, id });
        }
        Ok(())
    }

    /// Queues `command` for the worker: its id. With no worker, a test's
    /// session keeps it instead.
    fn send(&mut self, command: &Command) -> Result<u32, Loss> {
        #[cfg(test)]
        if self.worker.is_none() {
            self.sent.push(command.clone());
            return Ok(u32::try_from(self.sent.len()).unwrap_or(u32::MAX));
        }
        self.worker
            .as_mut()
            .ok_or(Loss::Transport)?
            .send(command)
            .map_err(|_| Loss::Transport)
    }

    /// Sends player `i` its close. A player closed because its peer was
    /// lost has its file spared by a worker the game terminates
    /// ([`Worker::spare`]).
    fn close(&mut self, i: usize) -> Result<(), Loss> {
        let (Some(p), Some(worker)) = (self.players.get_mut(i), self.worker.as_mut()) else {
            return Ok(());
        };
        if matches!(p.stage, Stage::Closing(_)) {
            return Ok(());
        }
        match worker.close(p.id) {
            Ok(close) => {
                if p.movie.loss() == Some(Loss::Peer) {
                    worker.spare(p.id);
                }
                p.wait_on(Stage::Closing(close));
                Ok(())
            }
            Err(_) => Err(Loss::Transport),
        }
    }

    /// Judges every player's deadlines at `now` (L2; only when `settled`,
    /// every reply that arrived before `now` taken) and progress (L3),
    /// closing the ones of other screens and the lost.
    fn judge(&mut self, generation: u64, now: Instant, settled: bool) {
        let mut lost = Vec::new();
        let mut close = Vec::new();
        for i in 0..self.players.len() {
            if !settled {
                break;
            }
            let Some(awaited) = self.players.get(i).and_then(|p| p.awaited(&self.deadlines)) else {
                continue;
            };
            let (id, limit) = awaited;
            if self
                .written(i, id)
                .is_some_and(|at| now.saturating_duration_since(at) > limit)
            {
                lost.push((i, Loss::Answer));
            }
        }
        for (i, p) in self.players.iter_mut().enumerate() {
            if lost.iter().any(|(l, _)| *l == i) {
                continue;
            }
            if matches!(p.stage, Stage::Live | Stage::Seeking { .. })
                && p.movie.generation() == generation
                && p.movie.status() != Status::Lost
                && let Some(why) = progress(p, now, self.deadlines.progress)
            {
                lost.push((i, why));
                continue;
            }
            if super::movie::unwanted(p.stage, &p.movie.cell, generation) {
                close.push(i);
            }
        }
        for (i, why) in lost {
            let Some(p) = self.players.get(i) else {
                continue;
            };
            if p.movie.lose(why) {
                event!(self.shared, Event::Lost { why });
            }
            if why == Loss::Answer {
                // A missed answer on a working transport: the worker is
                // ended once nothing of it is live.
                self.marked = true;
            }
            if !matches!(p.stage, Stage::Closing(_)) {
                close.push(i);
            }
        }
        close.sort_unstable();
        close.dedup();
        for i in close {
            if let Err(why) = self.close(i) {
                self.fail_worker(why);
                return;
            }
        }
    }

    /// Ends the session: every video lost, those of the opens `rx` still
    /// holds included, none launched; the workers dropped (their jobs end
    /// after a grace, without waiting). The game's handle is gone, so no
    /// open comes after the ones `rx` holds.
    fn quit(&mut self, rx: &Receiver<Request>) {
        for p in &self.players {
            p.movie.lose(Loss::Ended);
        }
        while let Ok(request) = rx.try_recv() {
            match request {
                Request::Open { movie, .. } => {
                    movie.lose(Loss::Ended);
                }
                #[cfg(test)]
                Request::Stuff(_) => {}
            }
        }
        self.players.clear();
        self.worker = None;
        self.dying.clear();
        self.shared.pid.store(0, Ordering::Release);
    }
}

/// Judges a live player's progress (L3): why it is lost, if it is.
fn progress(p: &mut Player, now: Instant, limit: Duration) -> Option<Loss> {
    let live = p.movie.live()?;
    let words = live.ring.words();
    let (progress, state) = media::supervision(words)?;
    let seen = Seen {
        progress,
        state,
        published: media::watermark(words).unwrap_or(0),
        freed: p.movie.freed.load(Ordering::Acquire),
    };
    stalled(&mut p.progress, seen, p.looping, now, limit)
}

/// What the session reads of a live player's ring and of the game's frees.
#[derive(Clone, Copy, Debug)]
struct Seen {
    /// The progress word: frames the decoder returned.
    progress: u64,
    state: Option<State>,
    /// The watermark: the newest frame published.
    published: u64,
    /// The slots the game freed.
    freed: u64,
}

/// The progress rule (L3) on `seen`, `last` being the progress word's last
/// value and when it rose or the window last restarted: lost when the word
/// has not risen for more than `limit`, unless every slot holds a frame the
/// game has not freed (published frames past the slots it freed) or a
/// play that is not looping has ended, either of which restarts the
/// window.
fn stalled(
    last: &mut (u64, Instant),
    seen: Seen,
    looping: bool,
    now: Instant,
    limit: Duration,
) -> Option<Loss> {
    if seen.progress != last.0 {
        *last = (seen.progress, now);
        return None;
    }
    let excused = match seen.state {
        Some(State::Waiting) => {
            let slots = u64::try_from(media::SLOTS).unwrap_or(u64::MAX);
            seen.freed.saturating_add(slots) <= seen.published
        }
        Some(State::Ended) => !looping,
        Some(State::Decoding) | None => false,
    };
    if excused {
        last.1 = now;
        return None;
    }
    (now.duration_since(last.1) > limit).then_some(Loss::Progress)
}

/// The deadline of a screen's preparation that began at `start`.
pub fn preparation_deadline(start: Instant) -> Instant {
    start + PREPARATION
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Deadlines::GAME.progress;

    /// The pixel total before any ring: a video alone at the total is
    /// admitted and one pixel past it refused; a second that fits beside
    /// the first's ring is admitted, one that does not is refused; while an
    /// earlier video of the screen has no ring, the verdict waits.
    #[test]
    fn the_pixel_total_is_applied_in_slot_order_before_any_ring() {
        use skin_model::movie::MAX_MOVIE_PIXELS;
        let hd = 1920 * 1080;
        assert_eq!(total(false, 0, MAX_MOVIE_PIXELS), Verdict::Admit);
        assert_eq!(total(false, 0, MAX_MOVIE_PIXELS + 1), Verdict::Refuse);
        assert_eq!(total(false, hd, hd), Verdict::Admit);
        assert_eq!(total(false, 2560 * 1440, 1280 * 720), Verdict::Refuse);
        assert_eq!(total(true, 0, 1), Verdict::Defer);
    }

    /// A session with no worker showing the screen of `generation`.
    fn supervisor(generation: u64) -> Supervisor {
        let shared = Arc::new(Shared {
            generation: AtomicU64::new(generation),
            pid: AtomicU32::new(0),
            queue_lost: AtomicU32::new(0),
            quit: AtomicBool::new(false),
            transport: Mutex::new(None),
            events: Mutex::new(Vec::new()),
            origin: Instant::now(),
            beats: Mutex::new(Vec::new()),
        });
        let launching = Launching {
            installation: PathBuf::new(),
            data: PathBuf::new(),
            setup: Setup::recipe(),
        };
        Supervisor::new(launching, Deadlines::GAME, shared)
    }

    /// A player of `movie` whose open, by `id`, the worker has.
    fn opening(movie: &Arc<Movie>, id: u8) -> Player {
        let now = Instant::now();
        Player {
            id,
            movie: Arc::clone(movie),
            file: None,
            looping: true,
            stage: Stage::Opening(u32::from(id)),
            queued: now,
            written: None,
            progress: (0, now),
            noted: false,
            held: None,
            sought: 1,
        }
    }

    /// A player of `movie`, by `id`, playing run 1 into its ring, its video
    /// live on the screen shown.
    fn live(movie: &Arc<Movie>, id: u8) -> Player {
        assert!(movie.cell.go_live());
        Player {
            stage: Stage::Live,
            ..opening(movie, id)
        }
    }

    /// The seek of `run` for player `player` the session sends.
    fn seek(player: u8, run: u64) -> Command {
        Command::Seek { player, run, at: 0 }
    }

    /// The worker's answer to seek `id` of player 1 for `run`, landing at
    /// `landed`.
    fn sought(id: u32, run: u64, landed: Option<i64>) -> Reply {
        Reply::Sought {
            id,
            player: 1,
            run,
            landed,
            attempts: u8::from(landed.is_some()),
        }
    }

    /// The worker's report that run `run` of player 1 ended.
    fn ended(run: u64) -> Reply {
        Reply::Ended { player: 1, run }
    }

    /// A session showing screen 1 with one live player, 1, of a video
    /// whose run `run` was sought and answered with a landing.
    fn sought_to(run: u64) -> (Supervisor, Arc<Movie>) {
        let mut s = supervisor(1);
        let movie = Movie::new(1);
        s.players = vec![live(&movie, 1)];
        movie.want(run);
        s.seek_wanted(1).unwrap();
        let id = u32::try_from(s.sent.len()).unwrap();
        s.take(Instant::now(), &sought(id, run, Some(40_000)))
            .unwrap();
        assert_eq!(
            (s.players[0].stage, s.players[0].sought),
            (Stage::Live, run)
        );
        (s, movie)
    }

    /// Runs wanted before the session's turn are sought once, for the
    /// newest, from the clip's start; nothing more until a newer want.
    // Covers: T02
    #[test]
    fn a_seek_is_sent_for_the_newest_want() {
        let mut s = supervisor(1);
        let movie = Movie::new(1);
        s.players = vec![live(&movie, 1)];
        // No want: no seek.
        s.seek_wanted(1).unwrap();
        assert!(s.sent.is_empty());
        movie.want(3);
        movie.want(4);
        s.seek_wanted(1).unwrap();
        assert_eq!(s.sent, [seek(1, 4)]);
        assert_eq!(s.players[0].stage, Stage::Seeking { id: 1, run: 4 });
        s.take(Instant::now(), &sought(1, 4, Some(40_000))).unwrap();
        s.seek_wanted(1).unwrap();
        assert_eq!(s.sent, [seek(1, 4)], "run 4 is sought already");
        assert_eq!(movie.loss(), None);
    }

    /// Wants made while a seek is in flight wait for its answer, and then
    /// only the newest is sought.
    // Covers: T02
    #[test]
    fn wants_during_a_seek_are_coalesced() {
        let mut s = supervisor(1);
        let movie = Movie::new(1);
        s.players = vec![live(&movie, 1)];
        movie.want(3);
        s.seek_wanted(1).unwrap();
        assert_eq!(s.sent, [seek(1, 3)]);
        movie.want(4);
        s.seek_wanted(1).unwrap();
        movie.want(5);
        s.seek_wanted(1).unwrap();
        assert_eq!(s.sent, [seek(1, 3)], "one seek in flight");
        s.take(Instant::now(), &sought(1, 3, Some(40_000))).unwrap();
        assert_eq!(s.players[0].sought, 3);
        s.seek_wanted(1).unwrap();
        assert_eq!(s.sent, [seek(1, 3), seek(1, 5)]);
        assert_eq!(s.players[0].stage, Stage::Seeking { id: 2, run: 5 });
        assert_eq!(movie.loss(), None);
    }

    /// An end for a run older than the newest sought (it ended before its
    /// seek arrived) changes nothing; one for the newest is the video's
    /// ended run.
    // Covers: T02
    #[test]
    fn a_stale_end_is_inert() {
        let (mut s, movie) = sought_to(4);
        s.take(Instant::now(), &ended(3)).unwrap();
        assert_eq!(movie.ended(), 0, "a stale end");
        assert_eq!(
            (s.players[0].stage, movie.status()),
            (Stage::Live, Status::Live)
        );
        assert_eq!(movie.loss(), None);
        s.take(Instant::now(), &ended(4)).unwrap();
        assert_eq!(movie.ended(), 4);
        assert_eq!(movie.loss(), None);
    }

    /// An end for a run newer than any sought loses the player.
    // Covers: T02
    #[test]
    fn an_end_for_a_run_not_sought_fails_the_player() {
        let (mut s, movie) = sought_to(4);
        s.take(Instant::now(), &ended(5)).unwrap();
        assert_eq!(movie.loss(), Some(Loss::Answer));
        assert_eq!(movie.ended(), 0);
        // So does one newer than the run in flight.
        let mut s = supervisor(1);
        let movie = Movie::new(1);
        s.players = vec![live(&movie, 1)];
        movie.want(3);
        s.seek_wanted(1).unwrap();
        s.take(Instant::now(), &ended(4)).unwrap();
        assert_eq!(movie.loss(), Some(Loss::Answer));
    }

    /// With one seek in flight an answer to another cannot be stale: it
    /// loses the player, as an answer with no seek in flight does; the
    /// in-flight seek's own answer does not.
    // Covers: T02
    #[test]
    fn a_sought_with_another_id_fails_the_player() {
        for (answer, lost) in [
            (sought(1, 3, Some(40_000)), None),
            (sought(2, 3, Some(40_000)), Some(Loss::Answer)),
            (sought(1, 4, Some(40_000)), Some(Loss::Answer)),
        ] {
            let mut s = supervisor(1);
            let movie = Movie::new(1);
            s.players = vec![live(&movie, 1)];
            movie.want(3);
            s.seek_wanted(1).unwrap();
            s.take(Instant::now(), &answer).unwrap();
            assert_eq!(movie.loss(), lost, "{answer:?}");
        }
        // Nothing in flight.
        let (mut s, movie) = sought_to(3);
        s.take(Instant::now(), &sought(1, 3, Some(40_000))).unwrap();
        assert_eq!(movie.loss(), Some(Loss::Answer));
    }

    /// An empty seek is answered with no landing, then its run's end: the
    /// player is live, its ended run that run, and nothing is lost.
    // Covers: T02
    #[test]
    fn an_empty_seek_is_answered_then_ended() {
        let mut s = supervisor(1);
        let movie = Movie::new(1);
        s.players = vec![live(&movie, 1)];
        movie.want(4);
        s.seek_wanted(1).unwrap();
        s.take(Instant::now(), &sought(1, 4, None)).unwrap();
        s.take(Instant::now(), &ended(4)).unwrap();
        assert_eq!(s.players[0].stage, Stage::Live);
        assert_eq!(s.players[0].sought, 4);
        assert_eq!(movie.ended(), 4);
        assert_eq!((movie.status(), movie.loss()), (Status::Live, None));
    }

    /// A seek not answered within the answer deadline of its write loses
    /// its player for the answer, and marks the worker; at the deadline it
    /// is still waited on.
    // Covers: T02
    #[test]
    fn an_unanswered_seek_loses_its_player() {
        let limit = Deadlines::GAME.answer;
        for (since, lost) in [
            (limit, None),
            (limit + Duration::from_millis(1), Some(Loss::Answer)),
        ] {
            let mut s = supervisor(1);
            let movie = Movie::new(1);
            s.players = vec![live(&movie, 1)];
            movie.want(2);
            s.seek_wanted(1).unwrap();
            let now = Instant::now();
            // The session saw the seek written `since` before now.
            s.players[0].written = Some(now.checked_sub(since).unwrap());
            s.judge(1, now, true);
            assert_eq!(movie.loss(), lost, "{since:?}");
            assert_eq!(s.marked, lost.is_some());
        }
    }

    /// An end for the run in flight, its seek not answered yet, loses the
    /// player: the worker answers a seek before any end of its run. The
    /// ended run stays the last answered run's.
    // Covers: T02
    #[test]
    fn an_end_before_its_seek_is_answered_fails_the_player() {
        let (mut s, movie) = sought_to(4);
        s.take(Instant::now(), &ended(4)).unwrap();
        assert_eq!(movie.ended(), 4);
        movie.want(5);
        s.seek_wanted(1).unwrap();
        assert_eq!(s.players[0].stage, Stage::Seeking { id: 2, run: 5 });
        s.take(Instant::now(), &ended(5)).unwrap();
        assert_eq!(movie.ended(), 4, "run 5's end before its answer");
        assert_eq!(movie.loss(), Some(Loss::Answer));
        // An end of the run answered last, run 4, while 5 is in flight is
        // that run's end, not a loss.
        let (mut s, movie) = sought_to(4);
        movie.want(5);
        s.seek_wanted(1).unwrap();
        s.take(Instant::now(), &ended(4)).unwrap();
        assert_eq!((movie.ended(), movie.loss()), (4, None));
        assert_eq!(s.players[0].stage, Stage::Seeking { id: 2, run: 5 });
    }

    /// The worker's rejection of the seek in flight fails the player; a
    /// rejection of another command does not.
    // Covers: T02
    #[test]
    fn a_rejected_seek_fails_the_player() {
        for (id, lost) in [(2, None), (1, Some(Loss::Failed))] {
            let mut s = supervisor(1);
            let movie = Movie::new(1);
            s.players = vec![live(&movie, 1)];
            movie.want(3);
            s.seek_wanted(1).unwrap();
            let rejected = Reply::Rejected {
                id,
                player: 1,
                code: 87,
            };
            s.take(Instant::now(), &rejected).unwrap();
            assert_eq!(movie.loss(), lost, "a rejection of command {id}");
            let stage = if lost.is_some() {
                Stage::Idle
            } else {
                Stage::Seeking { id: 1, run: 3 }
            };
            assert_eq!(s.players[0].stage, stage);
            assert!(!s.marked);
        }
    }

    /// A seek keeps the progress window it found: a decoder whose word has
    /// not risen since 2.001 s ago is lost for its progress, with its seek
    /// in flight (sent 0.5 s ago) or answered meanwhile.
    // Covers: T02
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_seek_keeps_the_progress_window() {
        for answered in [false, true] {
            let mut s = supervisor(1);
            let movie = Movie::live_here(1, 64, 48);
            let words = movie.live().unwrap().ring.words();
            words[media::PROGRESS_WORD].store(7, Ordering::Release);
            words[media::STATE_WORD].store(State::Decoding.word(), Ordering::Release);
            let now = Instant::now();
            let since = now.checked_sub(MS(2_001)).unwrap();
            s.players = vec![Player {
                stage: Stage::Live,
                progress: (7, since),
                ..opening(&movie, 1)
            }];
            movie.want(2);
            s.seek_wanted(1).unwrap();
            s.players[0].written = Some(now.checked_sub(MS(500)).unwrap());
            if answered {
                s.take(Instant::now(), &sought(1, 2, Some(40_000))).unwrap();
                assert_eq!(s.players[0].stage, Stage::Live);
            }
            s.judge(1, now, true);
            assert_eq!(movie.loss(), Some(Loss::Progress), "answered: {answered}");
        }
    }

    /// A file to hand the session.
    fn file() -> std::fs::File {
        std::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap()
    }

    /// The session `s` on a thread of its own, which starts its turns once
    /// `before` has used the game's handle: the handle, and the thread.
    fn started(
        s: Supervisor,
        before: impl FnOnce(&Session),
    ) -> (Session, std::thread::JoinHandle<()>) {
        let (tx, rx) = mpsc::sync_channel(REQUESTS);
        let (go, gone) = mpsc::channel::<()>();
        let shared = Arc::clone(&s.shared);
        let thread = std::thread::spawn(move || {
            if gone.recv().is_ok() {
                s.run(&rx);
            }
        });
        let session = Session {
            tx,
            shared,
            thread: thread.thread().clone(),
        };
        before(&session);
        go.send(()).unwrap();
        (session, thread)
    }

    /// What lets a held turn go. A turn waits on it without parking its
    /// thread: a channel's wait parks it, and would take the wake a test
    /// makes meanwhile.
    #[derive(Default)]
    struct Gate {
        /// The turns let go and not taken yet, `u32::MAX` for all.
        passes: Mutex<u32>,
        opened: std::sync::Condvar,
    }

    impl Gate {
        /// Lets the next held turn go.
        fn open(&self) {
            let mut passes = self.passes.lock().unwrap();
            *passes = passes.saturating_add(1);
            self.opened.notify_all();
        }

        /// Lets every held turn go from now on.
        fn open_all(&self) {
            *self.passes.lock().unwrap() = u32::MAX;
            self.opened.notify_all();
        }

        /// Waits until a held turn may go, up to 10 s, and takes its pass.
        fn pass(&self) {
            let passes = self.passes.lock().unwrap();
            let (mut passes, _) = self
                .opened
                .wait_timeout_while(passes, Duration::from_secs(10), |n| *n == 0)
                .unwrap();
            if *passes != u32::MAX {
                *passes = passes.saturating_sub(1);
            }
        }
    }

    /// Has `s` report each of its turns' held points `at` to the receiver
    /// returned and wait there until the gate returned lets it go. At
    /// [`Held::Looked`] it first takes any wake left from before, so the
    /// next turn comes only from a wake made while it is held.
    fn hold_at(s: &mut Supervisor, at: Held) -> (Receiver<()>, Arc<Gate>) {
        let (turned, turns) = mpsc::channel();
        let gate = Arc::new(Gate::default());
        let held_by = Arc::clone(&gate);
        s.turn = Some(Box::new(move |held| {
            if held != at {
                return;
            }
            if held == Held::Looked {
                std::thread::park_timeout(Duration::ZERO);
            }
            let _ = turned.send(());
            held_by.pass();
        }));
        (turns, gate)
    }

    /// Wakes take no place of the request queue: however many come, a
    /// queue's worth of opens all find room and nothing is lost at the
    /// hand-off.
    // Covers: T02
    #[test]
    fn wakes_never_crowd_out_an_open() {
        let (tx, rx) = mpsc::sync_channel(REQUESTS);
        let session = Session {
            tx,
            shared: supervisor(1).shared,
            thread: std::thread::current(),
        };
        let waker = session.waker();
        for _ in 0..REQUESTS {
            waker.wake();
        }
        let movies: Vec<_> = (0..REQUESTS)
            .map(|_| {
                let movie = Movie::new(1);
                session.open(&movie, file(), false);
                movie
            })
            .collect();
        let lost = movies.iter().filter(|m| m.loss().is_some()).count();
        let opens = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|r| matches!(r, Request::Open { .. }))
            .count();
        assert_eq!((lost, opens), (0, REQUESTS));
    }

    /// A wake is never lost: one made after the session has looked (read
    /// the wants) and before it sleeps, held there, ends its sleep at once.
    /// The session takes another turn long before its tick (60 s here),
    /// three times over.
    // Covers: T02
    #[test]
    fn a_wake_before_the_sleep_is_not_lost() {
        let mut s = supervisor(1);
        s.deadlines.tick = Duration::from_secs(60);
        let (turns, gate) = hold_at(&mut s, Held::Looked);
        let (session, ended) = started(s, |_| {});
        let waker = session.waker();
        let mut turned = Vec::new();
        for _ in 0..3 {
            turned.push(turns.recv_timeout(Duration::from_secs(10)).is_ok());
            waker.wake();
            gate.open();
        }
        turned.push(turns.recv_timeout(Duration::from_secs(10)).is_ok());
        drop(session);
        gate.open_all();
        ended.join().unwrap();
        assert_eq!(turned, [true; 4], "a wake waited for the tick");
    }

    /// A waker held after the game's handle is dropped keeps no session
    /// alive, and the drop ends it at once, not at its tick (60 s here).
    // Covers: T02
    #[test]
    fn a_retained_waker_keeps_no_session_alive() {
        let mut s = supervisor(1);
        s.deadlines.tick = Duration::from_secs(60);
        let (turns, gate) = hold_at(&mut s, Held::Looked);
        let (session, ended) = started(s, |_| {});
        let waker = session.waker();
        // The session asleep after its first look.
        turns.recv_timeout(Duration::from_secs(10)).unwrap();
        gate.open_all();
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = ended.join();
            let _ = done.send(());
        });
        drop(session);
        let finished = finished.recv_timeout(Duration::from_secs(2)).is_ok();
        drop(waker);
        assert!(finished, "the session outlived the game's handle");
    }

    /// An open the session's queue took before the game's handle went is
    /// lost with the session's end, never left pending: the session is held once it has taken its requests (the first open
    /// among them), one more open is accepted and the handle dropped, then
    /// the session goes on.
    // Covers: T02
    #[test]
    fn an_open_accepted_before_the_end_is_lost_with_it() {
        let mut s = supervisor(1);
        let (held, gate) = hold_at(&mut s, Held::Taken);
        let first = Movie::new(1);
        let (session, ended) = started(s, |session| session.open(&first, file(), false));
        held.recv_timeout(Duration::from_secs(10)).unwrap();
        let late = Movie::new(1);
        session.open(&late, file(), false);
        drop(session);
        gate.open();
        ended.join().unwrap();
        assert_eq!(
            (first.loss(), late.loss()),
            (Some(Loss::Ended), Some(Loss::Ended))
        );
    }

    /// A probe of `width` × `height`.
    const fn probe(width: u32, height: u32) -> Probe {
        Probe {
            width,
            height,
            duration: 1_000_000,
            matrix: Matrix::Bt709,
            range: Range::Limited,
        }
    }

    /// A probe answered while an earlier video of its screen has no ring
    /// is held back, nothing charged; once that video is gone, the next
    /// look judges it alone and charges it.
    #[test]
    fn a_probe_waits_for_the_earlier_videos_of_its_screen() {
        let mut s = supervisor(1);
        let (a, b) = (Movie::new(1), Movie::new(1));
        s.players = vec![opening(&a, 1), opening(&b, 2)];
        s.admit_probe(1, probe(1280, 1024)).unwrap();
        assert!(s.players[1].held.is_some(), "held while slot 0 has no ring");
        assert!(s.players[1].awaited(&Deadlines::GAME).is_none());
        assert_eq!(s.charged.1, 0);
        assert_eq!(b.status(), Status::Pending);
        let _ = a.lose(Loss::Failed);
        s.retry_held();
        assert!(s.players[1].held.is_none());
        assert_eq!(s.charged, (1, 1280 * 1024));
        assert_eq!(b.loss(), None);
    }

    /// A probe held back whose screen is left before it is asked again
    /// gets no ring: the screen shown is read at its admission, not where
    /// the look began. Slot 1 of screen 1 is held behind slot 0; slot 0 is
    /// lost and screen 2 entered; the retry closes slot 1 with nothing
    /// charged and no ring.
    #[test]
    fn a_held_probe_of_a_screen_left_meanwhile_gets_no_ring() {
        let mut s = supervisor(1);
        let (a, b) = (Movie::new(1), Movie::new(1));
        s.players = vec![opening(&a, 1), opening(&b, 2)];
        s.admit_probe(1, probe(1280, 1024)).unwrap();
        assert!(s.players[1].held.is_some());
        let _ = a.lose(Loss::Failed);
        s.shared.generation.store(2, Ordering::Release);
        s.retry_held();
        assert!(s.players[1].held.is_none());
        assert_eq!(s.charged.1, 0, "no ring made for a screen left");
    }

    /// A ring whose creation fails spends nothing of the total: a
    /// 2048 × 1536 video admitted whose ring cannot be made leaves nothing
    /// charged, so a 1280 × 1024 peer of the same screen still fits (the
    /// two together would not) and its ring is charged alone.
    #[test]
    fn a_failed_ring_creation_charges_nothing() {
        let mut s = supervisor(1);
        let (a, b) = (Movie::new(1), Movie::new(1));
        s.players = vec![opening(&a, 1), opening(&b, 2)];
        s.ring_fails = true;
        s.admit_probe(0, probe(2048, 1536)).unwrap();
        assert_eq!(s.charged, (1, 0), "a ring never made is not charged");
        // Its loss, as a session with a worker records it.
        let _ = a.lose(Loss::Failed);
        s.ring_fails = false;
        s.admit_probe(1, probe(1280, 1024)).unwrap();
        assert_eq!(b.loss(), None, "the peer fits");
        assert_eq!(s.charged, (1, 1280 * 1024));
    }

    /// A video's ring stays charged for the rest of its screen, lost or not:
    /// a 2048 × 1536 video admitted then lost (its ring still mapped by the
    /// render side) leaves no room for a 1280 × 1024 peer, which is refused
    /// at its probe and gets no ring. The next screen starts with nothing
    /// charged.
    #[test]
    fn a_lost_video_s_ring_stays_charged_for_its_screen() {
        let mut s = supervisor(1);
        let (a, b) = (Movie::new(1), Movie::new(1));
        s.players = vec![opening(&a, 1), opening(&b, 2)];
        s.admit_probe(0, probe(2048, 1536)).unwrap();
        assert_eq!(s.charged, (1, 2048 * 1536));
        let _ = a.lose(Loss::Fault);
        s.admit_probe(1, probe(1280, 1024)).unwrap();
        assert_eq!(b.loss(), Some(Loss::OverTotal), "the peer refused");
        assert!(b.live().is_none(), "no ring for it");
        assert_eq!(s.charged, (1, 2048 * 1536));
        // The next screen.
        let c = Movie::new(2);
        s.players = vec![opening(&c, 3)];
        s.admit_probe(0, probe(2048, 2048)).unwrap();
        assert_eq!(s.charged, (2, 2048 * 2048));
        assert_eq!(c.loss(), None);
    }

    /// The rule's verdict at each of `steps` (the time since the start, what
    /// is seen then), from a word of 0 seen at the start.
    fn verdicts(steps: &[(Duration, Seen)], looping: bool) -> Vec<Option<Loss>> {
        let start = Instant::now();
        let mut last = (0, start);
        steps
            .iter()
            .map(|(at, seen)| stalled(&mut last, *seen, looping, start + *at, LIMIT))
            .collect()
    }

    fn seen(progress: u64, state: State, published: u64, freed: u64) -> Seen {
        Seen {
            progress,
            state: Some(state),
            published,
            freed,
        }
    }

    const MS: fn(u64) -> Duration = Duration::from_millis;

    // Covers: T02
    #[test]
    fn a_stalled_decoder_is_lost_just_past_the_deadline() {
        // The word frozen while decoding: still there at the deadline, lost
        // just past it, however many frames were published before.
        let frozen = seen(0, State::Decoding, 9, 9);
        assert_eq!(
            verdicts(&[(LIMIT, frozen), (LIMIT + MS(1), frozen)], true),
            [None, Some(Loss::Progress)]
        );
        // A word that rises restarts the window from its rise.
        let rose = seen(1, State::Decoding, 9, 9);
        assert_eq!(
            verdicts(
                &[(MS(1_500), rose), (MS(3_500), rose), (MS(3_501), rose)],
                true
            ),
            [None, None, Some(Loss::Progress)]
        );
        // The watermark rising alone is not progress: frames published
        // while the decoder returns none leave the window running.
        assert_eq!(
            verdicts(
                &[
                    (LIMIT, seen(0, State::Decoding, 10, 10)),
                    (LIMIT + MS(1), seen(0, State::Decoding, 11, 11))
                ],
                true
            ),
            [None, Some(Loss::Progress)]
        );
    }

    // Covers: T02
    #[test]
    fn a_wait_on_the_game_is_excused_across_the_deadline_and_then_rearms() {
        let slots = u64::try_from(media::SLOTS).unwrap();
        // Every slot holds a frame the game has not freed (published 10,
        // freed 10 - SLOTS): excused for as long as it lasts, past the
        // deadline; once the game frees one, the window runs again from the
        // last look that excused it, and the frozen word is lost a deadline
        // after that look.
        let held = seen(0, State::Waiting, 10, 10 - slots);
        let freed = seen(0, State::Waiting, 10, 11 - slots);
        assert_eq!(
            verdicts(
                &[
                    (MS(1_000), held),
                    (MS(5_000), held),
                    (MS(5_900), held),
                    (MS(6_000), freed),
                    (MS(7_900), freed),
                    (MS(7_901), freed),
                ],
                true
            ),
            [None, None, None, None, None, Some(Loss::Progress)]
        );
        // The watermark alone, without the game's count, excuses nothing:
        // a wait whose slots the game has freed is a stall.
        let free = seen(0, State::Waiting, 10, 10);
        assert_eq!(
            verdicts(&[(LIMIT + MS(1), free)], true),
            [Some(Loss::Progress)]
        );
    }

    // Covers: T02
    #[test]
    fn only_an_end_for_good_is_excused() {
        let ended = seen(0, State::Ended, 4, 4);
        assert_eq!(verdicts(&[(MS(10_000), ended)], false), [None]);
        assert_eq!(
            verdicts(&[(LIMIT + MS(1), ended)], true),
            [Some(Loss::Progress)]
        );
    }
}
