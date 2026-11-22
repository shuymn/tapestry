//! The game's media session (design.md "Threat Model" T02): the real
//! worker, launched by the session under the recipe from a stage of the
//! test's own, supervised by the session's rules on its own clock, while
//! the test stands in for the game thread (its hand-offs and its screen's
//! preparation deadline) and for the render side (a reader polling each
//! ring as due, which counts the slots it frees). The worker's dev-tools
//! serving options make it fail in each way the rules judge: another
//! protocol at the hello, a command reader that stops while the control
//! loop beats, a control loop that never beats, a player frozen before its
//! play's answer, while it decodes or while it waits for a free slot, a slow
//! converter; a whole worker is starved by suspending its process.
//!
//! Each loss is timed against the deadline it follows: the session judges
//! every 100 ms, so a loss comes within 0.3 s past its deadline; every
//! wait is bounded.

#![cfg(all(windows, feature = "dev-tools"))]

mod media_support;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use media_support::media_host::EXIT_HUNG;
use media_support::media_host::launch::Launches;
use media_support::media_host::session::{
    Deadlines, Event, Launching, Loss, Movie, PREPARATION, Session, Status,
};
use media_support::{Made, Stage, Step, WORKER, read_tight, recipe};
use skin_model::media::{self, Reader, State};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME,
    PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

#[link(name = "ntdll")]
unsafe extern "system" {
    /// Suspends every thread of `process` (ntdll; the debuggers' and the
    /// process tools' suspend).
    fn NtSuspendProcess(process: HANDLE) -> i32;
}

/// The longest a game-thread hand-off may take.
const HAND_OFF: Duration = Duration::from_millis(1);

/// How far past its deadline a loss may come: the session's tick and its
/// judgement.
const SLACK: Duration = Duration::from_millis(300);

/// How long a test waits for a worker's launch: the recipe's patient wait
/// for the hello, and a launch's staging.
const LAUNCH: Duration = Duration::from_secs(40);

/// The clip every test plays: 2 s of 320 × 240 at 30 frames a second.
const CLIP: &str = "mpeg1video mpeg 320x240 30 60 15";

/// A session over a stage of the test's own, and the clip it plays.
struct Rig {
    // The session drops first, its worker before the stage's profile.
    session: Session,
    clip: PathBuf,
    _clips: tempfile::TempDir,
    stage: Stage,
}

impl Rig {
    /// A session whose worker serves the control pipe with `options`
    /// (`--dev-serve`), at the game's deadlines.
    fn serving(options: &[&str]) -> Self {
        let mut args = vec!["--dev-serve"];
        args.extend_from_slice(options);
        Self::with_args(&args)
    }

    /// A session whose worker is passed `args`.
    fn with_args(args: &[&str]) -> Self {
        let stage = Stage::new();
        let clips = tempfile::Builder::new()
            .prefix("media-session-")
            .tempdir()
            .unwrap();
        let clip = clips.path().join("clip.mpg");
        let out = Command::new(WORKER)
            .arg("--dev-gen-clip")
            .arg(&clip)
            .args(CLIP.split(' '))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let launching = Launching {
            installation: stage.dir().to_owned(),
            data: stage.data().to_owned(),
            setup: recipe().in_profile(stage.profile()).args_of(args),
        };
        let session = Session::start(launching, Deadlines::GAME).unwrap();
        Self {
            session,
            clip,
            _clips: clips,
            stage,
        }
    }

    /// Asks for the clip as `movie`, as the game thread does: how long the
    /// hand-off took.
    fn open(&self, movie: &Arc<Movie>, looping: bool) -> Duration {
        self.open_as(movie, "clip.mpg", looping)
    }

    /// Asks for a copy of the clip named `name` as `movie`: a file of its
    /// own, whose strikes are its own.
    fn open_as(&self, movie: &Arc<Movie>, name: &str, looping: bool) -> Duration {
        let path = self.clip.with_file_name(name);
        if !path.exists() {
            std::fs::copy(&self.clip, &path).unwrap();
        }
        let file = std::fs::File::open(&path).unwrap();
        let started = Instant::now();
        self.session.open(movie, file, looping);
        started.elapsed()
    }

    /// A clip of `spec` (`--dev-gen-clip`'s codec, container, size, rate,
    /// frames and group) named `name` beside the rig's own.
    fn clip_of(&self, name: &str, spec: &str) -> PathBuf {
        let path = self.clip.with_file_name(name);
        let out = Command::new(WORKER)
            .arg("--dev-gen-clip")
            .arg(&path)
            .args(spec.split(' '))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        path
    }

    /// Asks for the file at `path` as `movie`, looping.
    fn open_path(&self, movie: &Arc<Movie>, path: &std::path::Path) {
        self.session
            .open(movie, std::fs::File::open(path).unwrap(), true);
    }

    /// Shows the screen of `generation`: how long the hand-off took.
    fn enter(&self, generation: u64) -> Duration {
        let started = Instant::now();
        self.session.enter(generation);
        started.elapsed()
    }

    /// When the session first recorded an event `matching`, waiting up to
    /// `limit`.
    fn event(&self, limit: Duration, matching: impl Fn(&Event) -> bool) -> Option<Duration> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some((at, _)) = self.session.events().into_iter().find(|(_, e)| matching(e)) {
                return Some(at);
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// How many events `matching` the session has recorded.
    fn count(&self, matching: impl Fn(&Event) -> bool) -> usize {
        self.session
            .events()
            .iter()
            .filter(|(_, e)| matching(e))
            .count()
    }

    /// The process of the worker the session launches next or has, once
    /// launched.
    fn worker(&self, limit: Duration) -> Process {
        let deadline = Instant::now() + limit;
        loop {
            let pid = self.session.pid();
            if pid != 0 {
                return Process::open(pid);
            }
            assert!(Instant::now() < deadline, "no worker launched");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// A worker's process, opened by its id.
struct Process {
    pid: u32,
    handle: HANDLE,
}

impl Process {
    fn open(pid: u32) -> Self {
        // SAFETY: a process id; the handle is closed on drop.
        let handle = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SUSPEND_RESUME,
                false,
                pid,
            )
        }
        .unwrap();
        Self { pid, handle }
    }

    /// Its exit code once it has ended, waiting up to `limit`.
    fn exit(&self, limit: Duration) -> Option<u32> {
        let ms = u32::try_from(limit.as_millis()).unwrap();
        // SAFETY: the process this value holds.
        if unsafe { WaitForSingleObject(self.handle, ms) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0;
        // SAFETY: as above.
        unsafe { GetExitCodeProcess(self.handle, &raw mut code) }.unwrap();
        Some(code)
    }

    /// Suspends every thread of it, as a starved worker stands still.
    fn suspend(&self) {
        // SAFETY: the process this value holds, opened to suspend.
        assert_eq!(unsafe { NtSuspendProcess(self.handle) }, 0);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle this value holds, unused after this.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// What the render side's stand-in shares with the test.
#[derive(Default)]
struct Drawn {
    stop: AtomicBool,
    /// It reads nothing of the ring while set.
    hold: AtomicBool,
    /// The frames it took.
    staged: AtomicU64,
    /// When it first freed a slot.
    first_free: Mutex<Option<Instant>>,
}

/// The render side's stand-in for one movie: a reader polling its ring
/// every 4 ms (240 Hz) at the media time since its first poll, taking the
/// frame due, telling the session the slots it freed and the movie its
/// first frame; a fault loses the movie, a lost movie is no more read.
struct Render {
    drawn: Arc<Drawn>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Render {
    /// Draws `movie`, holding from the start when `held`.
    fn start(movie: &Arc<Movie>, held: bool) -> Self {
        let drawn = Arc::new(Drawn::default());
        drawn.hold.store(held, Ordering::Release);
        let thread = {
            let (movie, drawn) = (Arc::clone(movie), Arc::clone(&drawn));
            std::thread::spawn(move || draw(&movie, &drawn))
        };
        Self {
            drawn,
            thread: Some(thread),
        }
    }

    fn staged(&self) -> u64 {
        self.drawn.staged.load(Ordering::Acquire)
    }

    fn release(&self) {
        self.drawn.hold.store(false, Ordering::Release);
    }

    fn first_free(&self) -> Option<Instant> {
        *self.drawn.first_free.lock().unwrap()
    }

    /// Whether it stages new frames over `span`.
    fn advances_over(&self, span: Duration) -> bool {
        let before = self.staged();
        std::thread::sleep(span);
        self.staged() > before
    }
}

impl Drop for Render {
    fn drop(&mut self) {
        self.drawn.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn draw(movie: &Movie, drawn: &Drawn) {
    let mut reader: Option<Reader> = None;
    let mut origin = None;
    let mut frame = Vec::new();
    while !drawn.stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(4));
        if movie.status() == Status::Lost {
            continue;
        }
        let Some(live) = movie.live() else { continue };
        if reader.is_none() {
            reader = live.reader.lock().unwrap().take();
        }
        let Some(r) = reader.as_mut() else { continue };
        if drawn.hold.load(Ordering::Acquire) {
            continue;
        }
        let origin = *origin.get_or_insert_with(Instant::now);
        let due = i64::try_from(origin.elapsed().as_micros()).unwrap();
        match read_tight(r, live.ring.words(), due, &mut frame) {
            Ok((taken, freed)) => {
                if taken.is_some() {
                    drawn.staged.fetch_add(1, Ordering::AcqRel);
                    movie.staged_first();
                }
                if freed {
                    drawn
                        .first_free
                        .lock()
                        .unwrap()
                        .get_or_insert_with(Instant::now);
                    movie.freed(r.freed());
                }
            }
            Err(_) => {
                movie.lose(Loss::Fault);
            }
        }
    }
}

/// When `movie` is lost, waiting up to `limit`.
fn lost(movie: &Movie, limit: Duration) -> Option<Instant> {
    until(limit, || movie.status() == Status::Lost)
}

/// When `done` first holds, checking every millisecond up to `limit`.
fn until(limit: Duration, done: impl Fn() -> bool) -> Option<Instant> {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return Some(Instant::now());
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Waits until `movie` is live and its render side has staged a frame.
fn playing(movie: &Movie, render: &Render) -> Instant {
    until(LAUNCH, || {
        movie.status() == Status::Live && render.staged() > 0
    })
    .unwrap_or_else(|| panic!("never played: {:?}, {:?}", movie.status(), movie.loss()))
}

/// Asserts `at` falls within `SLACK` past `deadline` after `from`.
fn on_time(what: &str, from: Instant, at: Instant, deadline: Duration) {
    let took = at.duration_since(from);
    assert!(
        took >= deadline.saturating_sub(SLACK) && took <= deadline + SLACK,
        "{what}: {took:?}, not {deadline:?} ± {SLACK:?}"
    );
}

/// Asserts a hand-off returned in time.
fn handed_off(what: &str, took: Duration) {
    assert!(took <= HAND_OFF, "{what} took {took:?}");
}

// Covers: T02
#[test]
fn hello_with_another_protocol_is_refused_before_any_handle() {
    let rig = Rig::serving(&["protocol=3"]);
    // The launch itself refuses it at the handshake, and the process ends.
    let Err(e) = rig
        .stage
        .launch(&recipe().args_of(&["--dev-serve", "protocol=3"]))
    else {
        panic!("a worker of another protocol launched");
    };
    // ERROR_REVISION_MISMATCH.
    assert_eq!((e.step, e.code), (Step::Handshake, 1306), "{e}");
    assert!(e.what.contains("protocol 3"), "{e}");
    // The process ends with the refusal's code, on the handle the launch
    // kept when it stopped watching first.
    media_support::ends_refused(&e);
    // Through the session: the video is lost to the launch, and nothing
    // is handed to a worker that never launched.
    let movie = Movie::new(1);
    rig.session.enter(1);
    rig.open(&movie, true);
    assert!(lost(&movie, LAUNCH).is_some(), "{:?}", movie.status());
    assert_eq!(movie.loss(), Some(Loss::Launch));
    assert_eq!(rig.count(|e| matches!(e, Event::Launched { .. })), 0);
    assert_eq!(rig.count(|e| *e == Event::Opened), 0);
}

// Covers: T02
#[test]
fn a_game_with_no_core_to_keep_launches_no_worker() {
    use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessAffinityMask};

    // This test's process kept to one core, both its hardware threads: the
    // worker could take no subset of its cores that leaves the game one.
    let core = media_support::processor_cores()[0];
    // SAFETY: this process's pseudo handle.
    unsafe { SetProcessAffinityMask(GetCurrentProcess(), core) }.unwrap();
    let rig = Rig::serving(&[]);
    // The launch is refused at its first step, before anything is made.
    let Err(e) = rig.stage.launch(&recipe()) else {
        panic!("a worker launched on one core");
    };
    assert_eq!(e.step, Step::Job, "{e}");
    assert!(e.what.contains("no whole core is left for the game"), "{e}");
    assert!(matches!(e.process, Made::Nothing), "{:?}", e.process);
    // Through the session: the video is lost to the launch, the hand-off
    // never waits, and no worker runs.
    let movie = Movie::new(1);
    rig.session.enter(1);
    handed_off("an open", rig.open(&movie, true));
    assert!(lost(&movie, LAUNCH).is_some(), "{:?}", movie.status());
    assert_eq!(movie.loss(), Some(Loss::Launch));
    assert_eq!(rig.count(|e| matches!(e, Event::Launched { .. })), 0);
    assert_eq!(rig.session.pid(), 0);
}

// Covers: T02
#[test]
fn idle_worker_beats_at_production_deadlines() {
    let rig = Rig::serving(&[]);
    rig.session.enter(1);
    let first = Movie::new(1);
    rig.open(&first, true);
    let render = Render::start(&first, false);
    playing(&first, &render);
    // No player for 10 s.
    rig.enter(2);
    assert!(
        rig.event(Duration::from_secs(5), |e| *e == Event::Closed)
            .is_some()
    );
    drop(render);
    std::thread::sleep(Duration::from_secs(10));
    // Then two playing.
    let movies = [Movie::new(2), Movie::new(2)];
    let renders: Vec<_> = movies
        .iter()
        .map(|m| {
            rig.open(m, true);
            Render::start(m, false)
        })
        .collect();
    for (m, r) in movies.iter().zip(&renders) {
        playing(m, r);
    }
    std::thread::sleep(Duration::from_secs(5));
    for m in &movies {
        assert_eq!(m.status(), Status::Live, "{:?}", m.loss());
    }
    assert_eq!(first.loss(), None);
    let beats = rig.session.beats();
    let gaps: Vec<Duration> = beats.windows(2).map(|w| w[1] - w[0]).collect();
    let longest = gaps.iter().max().copied().unwrap_or_default();
    assert!(beats.len() >= 30, "{} beats", beats.len());
    assert!(
        longest <= Duration::from_millis(600),
        "a gap of {longest:?}"
    );
    assert_eq!(rig.count(|e| *e == Event::QueuedBeat), 0);
    assert_eq!(rig.count(|e| matches!(e, Event::Lost { .. })), 0);
}

// Covers: T02
#[test]
fn deaf_worker_is_a_transport_failure_despite_beats() {
    // It reads the first video's open and play, then nothing more.
    let rig = Rig::serving(&["deaf=2"]);
    rig.session.enter(1);
    let first = Movie::new(1);
    handed_off("an open", rig.open(&first, true));
    let render = Render::start(&first, false);
    playing(&first, &render);
    let worker = rig.worker(Duration::ZERO);
    // The pipe full, the writer's next command waits.
    rig.session.stuff(1 << 20);
    std::thread::sleep(Duration::from_millis(300));
    let second = Movie::new(1);
    let asked = Instant::now();
    handed_off("an open", rig.open(&second, true));
    // The game thread's hand-offs stay bounded while the write waits.
    for _ in 0..100 {
        handed_off("an entry", rig.enter(1));
        std::thread::sleep(Duration::from_millis(10));
    }
    let at = lost(&first, Duration::from_secs(5)).expect("no loss within 5 s");
    on_time("the transport's loss", asked, at, Deadlines::GAME.transport);
    assert!(lost(&second, Duration::from_millis(10)).is_some());
    assert_eq!(
        (first.loss(), second.loss()),
        (Some(Loss::Transport), Some(Loss::Transport))
    );
    assert_eq!(worker.exit(Duration::from_secs(5)), Some(EXIT_HUNG));
    // Every loss is stored before the termination is asked.
    let events = rig.session.events();
    let terminated = events
        .iter()
        .position(|(_, e)| {
            *e == Event::Terminated {
                pid: worker.pid,
                why: Loss::Transport,
            }
        })
        .expect("not terminated for the transport");
    let losses: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, (_, e))| {
            *e == Event::Lost {
                why: Loss::Transport,
            }
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(losses.len(), 2, "{events:?}");
    assert!(losses.iter().all(|&i| i < terminated), "{events:?}");
}

// Covers: T02
#[test]
fn missed_ack_on_a_working_transport_loses_one_player() {
    // Player 1 never answers its play.
    let rig = Rig::serving(&["freeze=1:play"]);
    rig.session.enter(1);
    let (frozen, peer) = (Movie::new(1), Movie::new(1));
    rig.open(&frozen, true);
    rig.open(&peer, true);
    let renders = [Render::start(&frozen, false), Render::start(&peer, false)];
    let worker = rig.worker(LAUNCH);
    // Its ring is made just before its play is sent.
    let sent = until(LAUNCH, || frozen.live().is_some()).expect("no play sent");
    let at = lost(&frozen, Duration::from_secs(5)).expect("no loss within 5 s");
    on_time("the play's answer", sent, at, Deadlines::GAME.answer);
    assert_eq!(frozen.loss(), Some(Loss::Answer));
    // The peer goes on, and the worker with it.
    playing(&peer, &renders[1]);
    assert!(renders[1].advances_over(Duration::from_secs(3)));
    assert_eq!(peer.status(), Status::Live);
    assert_eq!(rig.count(|e| matches!(e, Event::Terminated { .. })), 0);
    assert_eq!(worker.exit(Duration::ZERO), None);
    // Once nothing of it is live, it is terminated.
    rig.enter(2);
    assert!(
        rig.event(Duration::from_secs(4), |e| matches!(
            e,
            Event::Terminated {
                why: Loss::Answer,
                ..
            }
        ))
        .is_some(),
        "{:?}",
        rig.session.events()
    );
    assert_eq!(worker.exit(Duration::from_secs(5)), Some(EXIT_HUNG));
}

// Covers: T02
#[test]
fn session_close_deadline_bounds_the_departure() {
    let rig = Rig::serving(&[]);
    rig.session.enter(1);
    let movies = [Movie::new(1), Movie::new(1)];
    let renders: Vec<_> = movies
        .iter()
        .map(|m| {
            rig.open(m, true);
            Render::start(m, false)
        })
        .collect();
    for (m, r) in movies.iter().zip(&renders) {
        playing(m, r);
    }
    let left = Instant::now();
    handed_off("the departure", rig.enter(2));
    let deadline = Deadlines::GAME.close + SLACK;
    let both = until(deadline, || rig.count(|e| *e == Event::Closed) == 2);
    assert!(both.is_some(), "{:?}", rig.session.events());
    assert!(left.elapsed() <= deadline + Duration::from_millis(5));
    assert_eq!(rig.count(|e| matches!(e, Event::Lost { .. })), 0);
}

// Covers: T02
#[test]
fn heartbeat_stall_reaps_the_worker_in_selection() {
    // A worker that answers its hello and never beats.
    let rig = Rig::with_args(&["--dev-deaf"]);
    rig.session.enter(1);
    let movie = Movie::new(1);
    rig.open(&movie, true);
    let worker = rig.worker(LAUNCH);
    let launched = rig
        .event(Duration::ZERO, |e| matches!(e, Event::Launched { .. }))
        .unwrap();
    let terminated = rig
        .event(Duration::from_secs(5), |e| {
            matches!(
                e,
                Event::Terminated {
                    why: Loss::Heartbeat,
                    ..
                }
            )
        })
        .expect("not terminated within 5 s");
    // Its beats are counted from its handshake, before its launch ended.
    assert!(
        terminated - launched <= Deadlines::GAME.heartbeat + SLACK,
        "{:?}",
        terminated - launched
    );
    assert_eq!(movie.loss(), Some(Loss::Heartbeat));
    assert_eq!(worker.exit(Duration::from_secs(5)), Some(EXIT_HUNG));
    assert!(
        rig.event(Duration::from_secs(5), |e| *e
            == Event::Reaped { pid: worker.pid })
            .is_some()
    );
    // The next video the screen needs launches a new worker.
    let next = Movie::new(1);
    rig.open_as(&next, "next.mpg", true);
    let again = rig.worker(LAUNCH);
    assert_ne!(again.pid, worker.pid);
}

// Covers: T02
#[test]
fn decoder_stall_loses_only_its_player() {
    // Player 1 stops once it has published a frame, saying it decodes.
    let rig = Rig::serving(&["freeze=1:decoding"]);
    rig.session.enter(1);
    let (frozen, peer) = (Movie::new(1), Movie::new(1));
    rig.open(&frozen, true);
    rig.open(&peer, true);
    let renders = [Render::start(&frozen, false), Render::start(&peer, false)];
    let live = until(LAUNCH, || frozen.status() != Status::Pending).unwrap();
    let at = lost(&frozen, Duration::from_secs(5)).expect("no loss within 5 s");
    on_time("the decoder's progress", live, at, Deadlines::GAME.progress);
    assert_eq!(frozen.loss(), Some(Loss::Progress));
    playing(&peer, &renders[1]);
    assert!(renders[1].advances_over(Duration::from_secs(3)));
    assert_eq!(peer.status(), Status::Live);
}

// Covers: T02
#[test]
fn a_decoder_with_no_first_frame_is_lost_from_its_play() {
    // Player 1 answers its play and returns no frame, saying it decodes;
    // the launch is held past the progress deadline, so a window running
    // from the open's queuing would lose it at once.
    let rig = Rig::serving(&["freeze=1:start"]);
    let held = Launches::hold().unwrap();
    rig.session.enter(1);
    let movie = Movie::new(1);
    rig.open(&movie, true);
    std::thread::sleep(Deadlines::GAME.progress + Duration::from_millis(500));
    drop(held);
    let live = until(LAUNCH, || movie.status() != Status::Pending).unwrap();
    let at = lost(&movie, Duration::from_secs(5)).expect("no loss within 5 s");
    on_time(
        "the first frame's progress",
        live,
        at,
        Deadlines::GAME.progress,
    );
    assert_eq!(movie.loss(), Some(Loss::Progress));
}

// Covers: T02
#[test]
fn skipped_frames_count_as_progress() {
    // Each frame's conversion takes 1 s: the progress word rises at each
    // decoded frame, a second before the frame is published.
    let rig = Rig::serving(&["slow-convert=1000"]);
    rig.session.enter(1);
    let movie = Movie::new(1);
    rig.open(&movie, true);
    let render = Render::start(&movie, false);
    let live = until(LAUNCH, || movie.live().is_some()).expect("no ring");
    let words = movie.live().unwrap().ring.words();
    // When the progress word and the watermark reached each count.
    let mut decoded = Vec::new();
    let mut published = Vec::new();
    let end = live + Duration::from_secs(8);
    while Instant::now() < end {
        let (progress, _) = media::supervision(words).unwrap();
        let watermark = media::watermark(words).unwrap();
        let now = Instant::now();
        if progress > u64::try_from(decoded.len()).unwrap() {
            decoded.push(now);
        }
        if watermark > u64::try_from(published.len()).unwrap() {
            published.push(now);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(published.len() >= 4, "{} published", published.len());
    for (k, (d, p)) in decoded.iter().zip(&published).enumerate().take(4) {
        let ahead = p.duration_since(*d);
        assert!(
            ahead >= Duration::from_millis(800),
            "frame {k}: counted {ahead:?} before it was published"
        );
    }
    // Sparse publications with the decoder at work are never a stall.
    assert_eq!(movie.status(), Status::Live, "{:?}", movie.loss());
    assert!(render.staged() > 0);
}

// Covers: T02
#[test]
fn waiting_with_a_free_slot_is_not_exempt() {
    // Player 1 stops once it waits for a free slot.
    let rig = Rig::serving(&["freeze=1:waiting"]);
    rig.session.enter(1);
    let (frozen, peer) = (Movie::new(1), Movie::new(1));
    rig.open(&frozen, true);
    rig.open(&peer, true);
    let renders = [Render::start(&frozen, true), Render::start(&peer, false)];
    // Its ring full, it waits: by the game's own count no slot is free.
    until(LAUNCH, || {
        frozen.live().is_some_and(|l| {
            let words = l.ring.words();
            media::supervision(words).is_some_and(|(_, s)| s == Some(State::Waiting))
                && media::watermark(words) == Some(u64::try_from(media::SLOTS).unwrap())
        })
    })
    .expect("its ring never filled");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(frozen.status(), Status::Live, "{:?}", frozen.loss());
    // The game frees a slot: the wait is no longer excused.
    renders[0].release();
    let freed = until(Duration::from_secs(5), || renders[0].first_free().is_some())
        .and_then(|_| renders[0].first_free())
        .expect("nothing freed");
    let at = lost(&frozen, Duration::from_secs(5)).expect("no loss within 5 s");
    on_time(
        "the wait past a free slot",
        freed,
        at,
        Deadlines::GAME.progress,
    );
    assert_eq!(frozen.loss(), Some(Loss::Progress));
    assert!(renders[1].advances_over(Duration::from_secs(2)));
    assert_eq!(peer.status(), Status::Live);
}

// Covers: T02
#[test]
fn backpressure_ends_rearm_the_window() {
    let rig = Rig::serving(&[]);
    rig.session.enter(1);
    let movie = Movie::new(1);
    rig.open(&movie, true);
    let render = Render::start(&movie, true);
    until(LAUNCH, || movie.status() == Status::Live).expect("never live");
    // A ring the game leaves full is no stall.
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(movie.status(), Status::Live, "{:?}", movie.loss());
    // Once the game reads again, the worker goes on.
    render.release();
    assert!(render.advances_over(Duration::from_secs(3)));
    assert_eq!(movie.status(), Status::Live, "{:?}", movie.loss());
}

/// The game thread's preparation of a screen of `movies` from `start`:
/// each frame, every 16 ms, expires the movies at the deadline and commits
/// once every movie has settled. When it committed; each frame's work is
/// a hand-off.
fn prepare(movies: &[Arc<Movie>], start: Instant) -> Instant {
    let deadline = start + PREPARATION;
    loop {
        let frame = Instant::now();
        if frame >= deadline {
            for m in movies {
                m.expire();
            }
        }
        let settled = movies.iter().all(|m| m.settled());
        handed_off("a loading frame", frame.elapsed());
        if settled {
            return frame;
        }
        assert!(frame < deadline + Duration::from_secs(1), "never committed");
        std::thread::sleep(Duration::from_millis(16));
    }
}

// Covers: T02
#[test]
fn held_launch_expires_preparation_and_the_screen_commits() {
    let rig = Rig::serving(&[]);
    // Another launch holds the launches' mutex.
    let held = Launches::hold().unwrap();
    rig.session.enter(1);
    let start = Instant::now();
    let movies = [Movie::new(1), Movie::new(1)];
    for m in &movies {
        handed_off("an open", rig.open(m, true));
    }
    let committed = prepare(&movies, start);
    on_time("the commit", start, committed, PREPARATION);
    for m in &movies {
        assert_eq!(m.loss(), Some(Loss::Preparation));
    }
    // The launch completes later: the worker stays, and nothing of the
    // expired screen is opened.
    drop(held);
    let worker = rig.worker(LAUNCH);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(rig.count(|e| *e == Event::Opened), 0);
    assert_eq!(rig.session.pid(), worker.pid);
    assert_eq!(worker.exit(Duration::ZERO), None);
}

// Covers: T02
#[test]
fn departure_during_a_held_launch_opens_nothing() {
    let rig = Rig::serving(&[]);
    let held = Launches::hold().unwrap();
    rig.session.enter(1);
    let movie = Movie::new(1);
    rig.open(&movie, true);
    std::thread::sleep(Duration::from_millis(200));
    handed_off("the departure", rig.enter(2));
    std::thread::sleep(Duration::from_millis(200));
    drop(held);
    rig.worker(LAUNCH);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(rig.count(|e| *e == Event::Opened), 0);
    assert_ne!(movie.status(), Status::Live);
}

// Covers: T02
#[test]
fn cold_entry_meets_the_preparation_deadline() {
    // A stage of its own: no profile and no copies yet.
    let rig = Rig::serving(&[]);
    rig.session.enter(1);
    let start = Instant::now();
    let movie = Movie::new(1);
    rig.open(&movie, true);
    let render = Render::start(&movie, false);
    let committed = prepare(std::slice::from_ref(&movie), start);
    let took = committed - start;
    assert_eq!(
        movie.status(),
        Status::Live,
        "{:?} after {took:?}",
        movie.loss()
    );
    assert!(render.staged() > 0);
    assert!(took < PREPARATION, "{took:?}");
}

// Covers: T02
#[test]
fn live_movie_without_a_frame_is_lost_at_the_preparation_deadline() {
    let rig = Rig::serving(&[]);
    rig.session.enter(1);
    let movie = Movie::new(1);
    rig.open(&movie, true);
    // Its ring set up and its frames coming, but the render side has
    // staged none when the screen's preparation runs out.
    let render = Render::start(&movie, true);
    until(LAUNCH, || movie.status() == Status::Live).expect("never live");
    let start = Instant::now();
    let committed = prepare(std::slice::from_ref(&movie), start);
    on_time("the commit", start, committed, PREPARATION);
    assert_eq!(movie.loss(), Some(Loss::Preparation));
    assert!(movie.live().is_some());
    drop(render);
}

/// The screen's pixel total is applied at a movie's probe, before its
/// ring is made (design.md "Memory Budget"): with a 2048 × 1536 movie live,
/// a 1280 × 1024 peer passes the 2048 × 2048 total and is refused, lost
/// over its cap with no ring mapped; the control, a 1024 × 1024 peer,
/// meets the total exactly and plays.
// Covers: T02
#[test]
fn a_movie_past_the_total_maps_no_ring() {
    for (peer, refused) in [("1280x1024", true), ("1024x1024", false)] {
        let rig = Rig::serving(&[]);
        rig.session.enter(1);
        let big = Movie::new(1);
        rig.open_path(
            &big,
            &rig.clip_of("big.mpg", "mpeg1video mpeg 2048x1536 30 10 5"),
        );
        until(LAUNCH, || big.status() == Status::Live).expect("the first never live");
        let spec = format!("mpeg1video mpeg {peer} 30 10 5");
        let movie = Movie::new(1);
        rig.open_path(&movie, &rig.clip_of("peer.mpg", &spec));
        until(LAUNCH, || movie.status() != Status::Pending).expect("the peer never judged");
        if refused {
            assert_eq!(movie.loss(), Some(Loss::OverTotal), "{peer}");
            // Its player closed: the session is done with it.
            rig.event(Deadlines::GAME.close + SLACK, |e| *e == Event::Closed)
                .expect("the refused player never closed");
            assert!(movie.live().is_none(), "{peer}: a ring mapped");
        } else {
            assert_eq!(movie.status(), Status::Live, "{peer}");
            assert!(movie.live().is_some());
        }
        assert_eq!(big.status(), Status::Live, "{peer}: the first plays on");
    }
}

// Covers: T02
#[test]
fn cancelled_preparation_closes_and_drops_late_results() {
    let rig = Rig::serving(&[]);
    rig.session.enter(1);
    let movie = Movie::new(1);
    rig.open(&movie, true);
    // Leave as soon as the open is handed to the worker: its probe's
    // answer comes after.
    rig.event(LAUNCH, |e| *e == Event::Opened)
        .expect("never opened");
    let left = Instant::now();
    handed_off("the departure", rig.enter(2));
    let closed = until(Deadlines::GAME.close + SLACK, || {
        rig.count(|e| *e == Event::Closed) == 1
    });
    assert!(
        closed.is_some(),
        "not closed in {:?}: {:?}",
        left.elapsed(),
        rig.session.events()
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(movie.status(), Status::Pending);
    assert!(movie.live().is_none());
}

// Covers: T02
#[test]
fn starved_worker_blacks_its_movie_at_the_host_deadline() {
    let rig = Rig::serving(&[]);
    rig.session.enter(1);
    let deadline = Deadlines::GAME.heartbeat.max(Deadlines::GAME.answer);
    // At entry: suspended as soon as it is launched.
    let entering = Movie::new(1);
    handed_off("an open", rig.open_as(&entering, "entering.mpg", true));
    let worker = rig.worker(LAUNCH);
    worker.suspend();
    let suspended = Instant::now();
    let at = lost(&entering, Duration::from_secs(5)).expect("no loss at entry");
    assert!(at - suspended <= deadline + SLACK, "{:?}", at - suspended);
    assert_eq!(worker.exit(Duration::from_secs(5)), Some(EXIT_HUNG));
    // During playback.
    let playing_movie = Movie::new(1);
    handed_off("an open", rig.open_as(&playing_movie, "playing.mpg", true));
    let render = Render::start(&playing_movie, false);
    playing(&playing_movie, &render);
    let worker = rig.worker(LAUNCH);
    worker.suspend();
    let suspended = Instant::now();
    let at = lost(&playing_movie, Duration::from_secs(5)).expect("no loss in playback");
    assert!(at - suspended <= deadline + SLACK, "{:?}", at - suspended);
    assert_eq!(worker.exit(Duration::from_secs(5)), Some(EXIT_HUNG));
    // At departure.
    let leaving = Movie::new(1);
    handed_off("an open", rig.open_as(&leaving, "leaving.mpg", true));
    let render = Render::start(&leaving, false);
    playing(&leaving, &render);
    let worker = rig.worker(LAUNCH);
    worker.suspend();
    let suspended = Instant::now();
    handed_off("the departure", rig.enter(2));
    assert_eq!(
        worker.exit(deadline + SLACK + Duration::from_secs(1)),
        Some(EXIT_HUNG)
    );
    assert!(suspended.elapsed() <= deadline + SLACK + Duration::from_secs(1));
    assert_eq!(leaving.status(), Status::Lost);
}

// Covers: T02
#[test]
fn quitting_the_game_reaps_the_worker() {
    let rig = Rig::serving(&[]);
    rig.session.enter(1);
    let movie = Movie::new(1);
    rig.open(&movie, true);
    let render = Render::start(&movie, false);
    playing(&movie, &render);
    let worker = rig.worker(Duration::ZERO);
    let Rig { session, .. } = rig;
    drop(session);
    assert!(worker.exit(Duration::from_secs(10)).is_some());
    assert!(lost(&movie, Duration::from_secs(1)).is_some());
    drop(render);
}
