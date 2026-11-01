//! The render thread (design.md "Overall Architecture", "Rendering and Skins"):
//! it owns the screen, readies it for each play on request and says when it is
//! ready, draws each play from the logic step's effects and play view at the
//! play's chart time, and draws the end screen between plays and the select
//! screen ([`crate::menu_screen`]) on song selection. The end screen's page
//! goes through a slot a newer page replaces ([`RenderLink::show_end`]), so
//! however far the render thread falls behind, one page waits for it and
//! it holds one, the graphs of each shared with the result screen's cache;
//! leaving the end screen empties the slot ([`RenderLink::clear_end`]), and
//! a signal whose page went with it draws nothing.
//! With selection the
//! chart is not known at the start: each chosen chart is loaded on request
//! ([`RenderMsg::Load`]) and stays until the next. From a chart's choice
//! to its countdown the play screen is drawn in its loading state, through
//! the chart's load and the play's preparation, whose answers come between
//! its frames: the skin's loading frame ([`Screen::loading`]) with the
//! load's lines over it. The lines go through a slot as the end screen's
//! page does, where newer lines replace those not yet taken and at most
//! one signal waits in the channel ([`LoadingSink`]), so a slow render
//! thread never falls behind on the load's progress. A window target
//! paces frames on the swap chain's waitable object; an offscreen one at about
//! 60 Hz at most, each frame waiting for the GPU to finish the one before.
//!
//! A frame that fails (the surface was lost) drops the screen and tells
//! the game thread, which ends the play; the next play's preparation builds
//! the screen again, and if that fails the run ends with the error.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bms_model::{Chart, TimeSource, TimeUs};
use eval::{JudgeEvent, Section};
use game::{Chain, RenderSide, ResultGraphs};
use input::QpcClock;
use render::measure::{FrameStatistics, Presentation};
use render::{FrameInput, FrameTiming, RenderError};

use crate::measure::{Drawn, Figures, Measure, PlayMeasure};
use crate::menu_screen::{MenuPage, SPLASH};
use crate::tuning::LiveTuning;

/// What draws: the renderer, or a double in tests.
pub trait Screen: Send {
    /// Prepares `chart` and resets the play state; with a practice round's
    /// `section`, only its targets are drawn.
    ///
    /// # Errors
    ///
    /// Why it cannot draw the chart.
    fn load(&mut self, chart: &Chart, section: Option<&Section>) -> Result<(), RenderError>;
    /// Waits until a frame may start; `false` on a timeout. Returns at
    /// once without a frame latency waitable object.
    fn wait(&self) -> bool;
    /// Whether [`wait`](Self::wait) paces frames.
    fn paced(&self) -> bool;
    /// Builds one play frame from `f`: the app's own work, which calls
    /// nothing in wgpu. [`draw`](Self::draw) follows.
    fn build(&mut self, f: &FrameInput<'_>);
    /// Draws the play frame [`build`](Self::build) built;
    /// [`FrameTiming::presented`] is `false` when it was not presented (the
    /// swap chain image timed out or the window is occluded).
    ///
    /// # Errors
    ///
    /// Why the frame failed: the screen must be built again.
    fn draw(&mut self) -> Result<FrameTiming, RenderError>;
    /// The swap chain's latest frame statistics, raw; `None` without a
    /// swap chain or when the call failed.
    fn statistics(&self) -> Option<FrameStatistics> {
        None
    }
    /// How the swap chain presents; `None` without one.
    fn presentation(&self) -> Option<Presentation> {
        None
    }
    /// Draws lines of text, as [`draw`](Self::draw).
    ///
    /// # Errors
    ///
    /// As [`draw`](Self::draw).
    fn text(&mut self, lines: &[&str]) -> Result<bool, RenderError>;
    /// Draws the end screen's `page`, its lines and its graphs, as
    /// [`draw`](Self::draw). A screen that draws no graphs draws the lines
    /// as [`text`](Self::text).
    ///
    /// # Errors
    ///
    /// As [`draw`](Self::draw).
    fn end(&mut self, page: &EndPage) -> Result<bool, RenderError> {
        let lines: Vec<&str> = page.lines.iter().map(String::as_str).collect();
        self.text(&lines)
    }
    /// Draws the select screen's `page`, laid out again when `changed`
    /// (or when this screen has not laid it out yet). A screen that
    /// draws no menus draws nothing.
    ///
    /// # Errors
    ///
    /// As [`draw`](Self::draw), and why the page's text cannot be prepared.
    fn menu(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        let _ = (page, changed);
        Ok(FrameTiming {
            cpu: Duration::ZERO,
            sprites: 0,
            dropped: 0,
            presented: true,
            present_count: None,
        })
    }
    /// Draws the play screen while a chart loads before the countdown:
    /// the skin's loading frame with `page`, the load's lines, over it,
    /// laid out again when `changed` (design.md "Play Lifecycle"). The
    /// default draws them as a menu page.
    ///
    /// # Errors
    ///
    /// As [`menu`](Self::menu).
    fn loading(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        self.menu(page, changed)
    }
    /// Draws the launch screen `elapsed` after the render thread started,
    /// as [`text`](Self::text), until the game sends what to draw; `None`
    /// draws nothing instead (no launch screen).
    ///
    /// # Errors
    ///
    /// As [`menu`](Self::menu).
    fn splash(&mut self, elapsed: Duration) -> Option<Result<bool, RenderError>> {
        let _ = elapsed;
        None
    }
    /// The last frame's pixels (RGBA8) and size, for an offscreen target.
    #[cfg(feature = "dev-tools")]
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)>;
}

/// Builds the screen on the render thread, at startup and after a failure.
pub type MakeScreen = Box<dyn FnMut() -> Result<Box<dyn Screen>, RenderError> + Send>;

/// Why the screen cannot draw a play.
#[derive(Debug, thiserror::Error)]
pub enum ScreenError {
    /// The renderer could not be built or cannot draw the chart.
    #[error(transparent)]
    Render(#[from] RenderError),
    /// The render thread could not be spawned.
    #[error("the render thread cannot start")]
    Spawn(#[source] std::io::Error),
    /// The render thread ended before it answered a preparation.
    #[error("the render thread ended")]
    Ended,
    /// The render thread ended before its screen was built.
    #[error("the render thread ended while starting")]
    EndedWhileStarting,
    /// The render thread did not answer a chart's load in time.
    #[error("the render thread did not answer")]
    NoAnswer,
}

/// The end screen: its lines and the view's graphs, if it has them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndPage {
    /// The lines, the settings lines last.
    pub lines: Vec<String>,
    /// The viewed summary's graphs, shared with the result screen.
    pub graphs: Option<Arc<ResultGraphs>>,
}

/// What the game thread tells the render thread.
pub enum RenderMsg {
    /// Ready the screen for a play (build it again if it failed) and
    /// answer when ready.
    Prepare(mpsc::SyncSender<Result<(), RenderError>>),
    /// Another chart: prepare it, building the screen again if it failed,
    /// and answer when ready. Every play draws it until the next.
    Load {
        /// The chart.
        chart: Arc<Chart>,
        /// The section a practice round draws only the targets of.
        section: Option<Section>,
        /// Where the answer goes.
        reply: mpsc::SyncSender<Result<(), RenderError>>,
    },
    /// Draw the select screen's page instead of a play.
    Menu(Box<MenuPage>),
    /// Draw the play screen's loading state of this loading lifetime with
    /// the lines in the slot ([`LoadingSink`]), kept through a chart's load
    /// and a preparation until something else is drawn; nothing when the
    /// lifetime has ended ([`RenderLink::clear_loading`]).
    Loading(u64),
    /// Loading has ended: stop drawing the loading state, and answer once
    /// no loading frame can be drawn any more ([`RenderLink::end_loading`]).
    EndLoading(mpsc::SyncSender<()>),
    /// A play starts: chart time 0 at the measurement's `origin` on the
    /// clock.
    Play {
        /// The logic step's effects and play view.
        side: RenderSide,
        /// The gauge to show.
        gauge: usize,
        /// What its measurement needs.
        measure: Box<PlayMeasure>,
    },
    /// The play ended at `end` on the clock: answer with its measured
    /// figures, and count nothing more until the next play.
    Finish {
        /// When it ended.
        end: TimeUs,
        /// Where the figures go.
        reply: mpsc::SyncSender<Option<Figures>>,
    },
    /// Draw the end screen instead of a play: the newest page in the slot
    /// ([`RenderLink::show_end`]), or the one drawn last when an earlier
    /// signal already took this one's page or a newer one; nothing once
    /// the pages shown were cleared ([`RenderLink::clear_end`]).
    End,
    /// The run ends.
    Exit,
}

/// The inputs of every play's frames.
#[derive(Clone, Debug)]
pub struct View {
    /// Hi-speed (the visible time) and the display offset, read at every
    /// frame.
    pub tuning: Arc<LiveTuning>,
    /// The effect queue's capacity: the most effects one frame drains.
    pub effects: usize,
}

/// How long the game thread waits for a chart's load: the render thread
/// answers between frames, after the skin's uploads.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the end of the render thread is waited for: past it, the thread
/// is left to the process's exit.
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the game thread waits for a play's figures. The render thread
/// answers between frames, so this bounds only a stalled render thread: a
/// slow frame, such as a software adapter drawing and the dev-tools frame
/// dump encoding an image, can take seconds, and the figures are still
/// wanted after it.
const FINISH_TIMEOUT: Duration = Duration::from_secs(60);

/// Something that looks at frames as they are drawn, after the measurement
/// (design.md "Latency Figures"): the dev-tools frame dump. Called after every
/// frame, presented or not.
pub trait FrameSink: Send {
    /// Called after each frame.
    fn frame(&mut self, screen: &dyn Screen, drawn: &Drawn<'_>);
}

/// No frame sink.
pub struct NoFrames;

impl FrameSink for NoFrames {
    fn frame(&mut self, _screen: &dyn Screen, _drawn: &Drawn<'_>) {}
}

#[derive(Debug)]
struct Shared {
    failed: AtomicBool,
    /// When the current play's first frame was drawn, on the clock.
    first_frame: AtomicI64,
    /// The end screen's slot: its newest page, and the pages left with the
    /// end screen.
    end_page: Mutex<EndSlot>,
    /// The number of the end page last presented, after the frame sink
    /// saw it (dev-tools).
    #[cfg(feature = "dev-tools")]
    end_drawn: AtomicU64,
    /// The loading state's slot.
    loading: Mutex<LoadingSlot>,
}

/// The loading state's lifetime, its newest page until the render thread
/// takes it or the lifetime ends, and whether a [`RenderMsg::Loading`] of
/// this lifetime waits in the channel. Each clear starts a new lifetime: a
/// sink, a signal or a frame of an older one does nothing.
#[derive(Debug, Default)]
struct LoadingSlot {
    lifetime: u64,
    page: Option<Box<MenuPage>>,
    signaled: bool,
}

/// The end screen's newest page with its number, until the render thread
/// takes it or the end screen is left, and the number of the last page
/// shown when it was last left: no page up to it is drawn again.
#[derive(Debug, Default)]
struct EndSlot {
    page: Option<(u64, EndPage)>,
    cleared: u64,
}

impl Shared {
    fn slot(&self) -> std::sync::MutexGuard<'_, EndSlot> {
        self.end_page.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts page `n` into the slot, returning what it held.
    fn put_end(&self, n: u64, page: EndPage) -> Option<(u64, EndPage)> {
        self.slot().page.replace((n, page))
    }

    /// Leaves the end screen with page `upto` shown last: the slot is
    /// emptied, returning what it held, and no page up to `upto` is valid.
    fn clear_end(&self, upto: u64) -> Option<(u64, EndPage)> {
        let mut slot = self.slot();
        slot.cleared = slot.cleared.max(upto);
        slot.page.take()
    }

    /// Takes the waiting page, if any.
    fn take_end(&self) -> Option<(u64, EndPage)> {
        self.slot().page.take()
    }

    /// The number of the last page cleared.
    fn end_cleared(&self) -> u64 {
        self.slot().cleared
    }

    fn loading_slot(&self) -> std::sync::MutexGuard<'_, LoadingSlot> {
        self.loading.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the waiting lines of loading lifetime `lifetime`, if it is
    /// still the current one: the signal that came for them is taken with
    /// them. A signal of an ended lifetime takes nothing.
    fn take_loading(&self, lifetime: u64) -> Option<Box<MenuPage>> {
        let mut slot = self.loading_slot();
        if slot.lifetime != lifetime {
            return None;
        }
        slot.signaled = false;
        slot.page.take()
    }

    /// Whether loading lifetime `lifetime` is still the current one.
    fn loading_current(&self, lifetime: u64) -> bool {
        self.loading_slot().lifetime == lifetime
    }

    /// Ends the current loading lifetime: its waiting lines are dropped,
    /// and its sinks, signals and frames do nothing from here.
    fn end_lifetime(&self) {
        let mut slot = self.loading_slot();
        slot.lifetime = slot.lifetime.wrapping_add(1);
        slot.signaled = false;
        drop(slot.page.take());
    }
}

/// Where the loading state is shown from, on any thread (the game thread
/// and the decode pool): the newest page replaces one the render thread
/// has not taken yet, and a signal is sent only when none waits, so at most
/// one page and one signal wait whatever the render thread's pace. A sink
/// belongs to the loading lifetime current when it was made, and shows
/// nothing once that lifetime has ended.
#[derive(Clone)]
pub struct LoadingSink {
    tx: Sender<RenderMsg>,
    shared: Arc<Shared>,
    lifetime: u64,
}

impl LoadingSink {
    /// Shows `page` as the loading state, if the sink's lifetime is still
    /// the current one.
    pub fn show(&self, page: MenuPage) {
        let mut slot = self.shared.loading_slot();
        if slot.lifetime != self.lifetime {
            return;
        }
        drop(slot.page.replace(Box::new(page)));
        if !slot.signaled {
            slot.signaled = true;
            // A thread that already ended ignores it.
            let _ = self.tx.send(RenderMsg::Loading(self.lifetime));
        }
    }
}

/// How the render thread ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderEnd {
    /// It ended and was joined.
    Joined,
    /// It ended by a panic.
    Panicked,
    /// It did not end in time and was left to the process's exit.
    Stuck,
}

/// The game thread's end of the render thread.
pub struct RenderLink {
    tx: Sender<RenderMsg>,
    shared: Arc<Shared>,
    handle: JoinHandle<()>,
    /// Disconnects when the thread ends, however it ends.
    ended: Receiver<()>,
    /// How long a preparation or a chart's load is waited for.
    answer_timeout: Duration,
    /// How long the end of the thread is waited for.
    exit_timeout: Duration,
    /// How long a play's figures are waited for.
    finish_timeout: Duration,
    /// The number of the last end page shown.
    end_pages: AtomicU64,
}

impl RenderLink {
    /// Sends a message; a thread that already ended ignores it.
    pub fn send(&self, msg: RenderMsg) {
        let _ = self.tx.send(msg);
    }

    /// Shows the end screen's `page`: it replaces a page the render thread
    /// has not taken yet, which is dropped here, so at most one waits
    /// whatever the render thread's pace.
    pub fn show_end(&self, page: EndPage) {
        let n = self.end_pages.fetch_add(1, Ordering::Relaxed) + 1;
        drop(self.shared.put_end(n, page));
        self.send(RenderMsg::End);
    }

    /// The end screen is left (a retry, selection, the practice page): a
    /// page still waiting is dropped, and no page shown so far is drawn
    /// again, by a signal still pending or by a frame of the page drawn
    /// last.
    pub fn clear_end(&self) {
        let upto = self.end_pages.load(Ordering::Relaxed);
        drop(self.shared.clear_end(upto));
    }

    /// Waits at most `timeout` until the end page shown last has been
    /// presented, and the frame sink has seen it (dev-tools: a script's
    /// `drawn` step). Whether it was.
    #[cfg(feature = "dev-tools")]
    pub fn wait_end_drawn(&self, timeout: Duration) -> bool {
        let shown = self.end_pages.load(Ordering::Relaxed);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.shared.end_drawn.load(Ordering::Acquire) >= shown {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Readies the screen for a play and waits until it is.
    ///
    /// # Errors
    ///
    /// Why the screen cannot be built or cannot draw the chart.
    pub fn prepare(&self) -> Result<(), ScreenError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Prepare(tx));
        self.answer(&rx)
    }

    /// Prepares `chart` for every play from now on, and waits until it is.
    ///
    /// # Errors
    ///
    /// As [`PendingLoad::wait`].
    #[cfg(test)]
    pub fn load(&self, chart: Arc<Chart>, section: Option<Section>) -> Result<(), ScreenError> {
        self.begin_load(chart, section).wait(self)
    }

    /// Starts preparing `chart` for every play from now on; the game
    /// thread goes on with the rest of the load and waits for the answer
    /// later ([`PendingLoad::wait`]). A pending load dropped unanswered
    /// leaves the screen with the chart, which the next load replaces.
    pub fn begin_load(&self, chart: Arc<Chart>, section: Option<Section>) -> PendingLoad {
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Load {
            chart,
            section,
            reply,
        });
        PendingLoad(rx)
    }

    /// Where the current loading lifetime's lines are shown from, on any
    /// thread.
    pub fn loading(&self) -> LoadingSink {
        LoadingSink {
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
            lifetime: self.shared.loading_slot().lifetime,
        }
    }

    /// Loading ended (selection, the practice page): its lifetime ends, so
    /// lines still waiting are dropped, a signal still pending takes
    /// nothing, and the render thread draws no further loading frame of it,
    /// checked after each frame's wait. The next lines shown begin a new
    /// lifetime's.
    pub fn clear_loading(&self) {
        self.shared.end_lifetime();
    }

    /// Loading ended before recording opens: [`clear_loading`](Self::clear_loading),
    /// then waits, at most [`LOAD_TIMEOUT`], until the render thread has
    /// answered that it draws no loading frame any more, so none is drawn
    /// after recording opens.
    ///
    /// # Errors
    ///
    /// [`ScreenError::NoAnswer`] after [`LOAD_TIMEOUT`], and
    /// [`ScreenError::Ended`] when the render thread is gone: recording
    /// must not open.
    pub fn end_loading(&self) -> Result<(), ScreenError> {
        self.clear_loading();
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::EndLoading(reply));
        match rx.recv_timeout(self.answer_timeout) {
            Ok(()) => Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ScreenError::NoAnswer),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ScreenError::Ended),
        }
    }

    /// The answer to a preparation or a load, waited for at most
    /// [`LOAD_TIMEOUT`]; a late answer goes nowhere.
    fn answer(&self, rx: &Receiver<Result<(), RenderError>>) -> Result<(), ScreenError> {
        match rx.recv_timeout(self.answer_timeout) {
            Ok(ready) => Ok(ready?),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ScreenError::NoAnswer),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ScreenError::Ended),
        }
    }

    /// Whether a frame failed since the last preparation.
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }

    /// The figures of the play that ended at `end`; `None` when the render
    /// thread did not answer or had no play to measure.
    pub fn finish(&self, end: TimeUs) -> Option<Figures> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Finish { end, reply });
        rx.recv_timeout(self.finish_timeout).ok().flatten()
    }

    /// When the current play's first frame was drawn.
    pub fn first_frame(&self) -> Option<TimeUs> {
        let t = self.shared.first_frame.load(Ordering::Acquire);
        (t != i64::MIN).then_some(TimeUs(t))
    }

    /// Ends the thread and waits for it at most [`EXIT_TIMEOUT`]; when it
    /// is [`RenderEnd::Stuck`], the window it draws into must stay open.
    pub fn exit(self) -> RenderEnd {
        self.send(RenderMsg::Exit);
        match self.ended.recv_timeout(self.exit_timeout) {
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Detached: a thread stuck in the screen's work must not
                // hold up the exit. The caller keeps the window open.
                eprintln!("render: the render thread did not end");
                RenderEnd::Stuck
            }
            _ => match self.handle.join() {
                Ok(()) => RenderEnd::Joined,
                Err(_) => RenderEnd::Panicked,
            },
        }
    }

    /// Sets how long a preparation, a chart's load or a play's figures
    /// and the end of the thread are waited for, in place of
    /// [`LOAD_TIMEOUT`], [`FINISH_TIMEOUT`] and [`EXIT_TIMEOUT`] (tests,
    /// and `--dev-render-timeout`).
    #[cfg(any(test, feature = "dev-tools"))]
    pub fn set_timeouts(&mut self, answer: Duration, exit: Duration) {
        self.answer_timeout = answer;
        self.finish_timeout = answer;
        self.exit_timeout = exit;
    }
}

/// A chart's load the render thread was asked for ([`RenderLink::begin_load`]).
pub struct PendingLoad(Receiver<Result<(), RenderError>>);

impl PendingLoad {
    /// Waits for the answer, at most [`LOAD_TIMEOUT`] from here.
    ///
    /// # Errors
    ///
    /// Why the screen cannot be built or cannot draw the chart, and
    /// [`ScreenError::NoAnswer`] after [`LOAD_TIMEOUT`]; a late answer goes
    /// nowhere.
    pub fn wait(self, link: &RenderLink) -> Result<(), ScreenError> {
        link.answer(&self.0)
    }
}

/// Starts the render thread; `make` builds the screen on it, and the chart,
/// when there is one yet, is prepared before this returns, so every upload
/// is done before the first countdown (design.md "Display Latency and
/// Stutter"); a chart loaded later is prepared before its countdown too.
///
/// # Errors
///
/// Why the screen could not be built or could not draw the chart.
pub fn spawn(
    mut make: MakeScreen,
    chart: Option<Arc<Chart>>,
    section: Option<Section>,
    clock: QpcClock,
    view: View,
    measure: Measure,
    frames: Box<dyn FrameSink>,
) -> Result<RenderLink, ScreenError> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let shared = Arc::new(Shared {
        failed: AtomicBool::new(false),
        first_frame: AtomicI64::new(i64::MIN),
        end_page: Mutex::new(EndSlot::default()),
        #[cfg(feature = "dev-tools")]
        end_drawn: AtomicU64::new(0),
        loading: Mutex::new(LoadingSlot::default()),
    });
    let thread_shared = Arc::clone(&shared);
    let (ended_tx, ended) = mpsc::sync_channel::<()>(0);
    let handle = std::thread::Builder::new()
        .name("render".into())
        .spawn(move || {
            // Dropped when the thread ends, however it ends.
            let _ended = ended_tx;
            #[cfg(feature = "dev-tools")]
            crate::alloc_count::tag(crate::alloc_count::Tag::Render);
            let screen = match build(&mut make, chart.as_deref(), section.as_ref()) {
                Ok(screen) => {
                    let _ = ready_tx.send(Ok(()));
                    screen
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let mut r = Run {
                screen: Some(screen),
                started: Instant::now(),
                make,
                chart,
                section,
                clock,
                view,
                measure,
                frames,
                shared: thread_shared,
                end_page: (0, EndPage::default()),
                granted: false,
                #[cfg(feature = "dev-tools")]
                menu_stats: crate::dev::MenuStats::default(),
                #[cfg(feature = "dev-tools")]
                loading_stats: crate::dev::MenuStats::default(),
            };
            r.run(&rx);
        })
        .map_err(ScreenError::Spawn)?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(RenderLink {
            tx,
            shared,
            handle,
            ended,
            answer_timeout: LOAD_TIMEOUT,
            exit_timeout: EXIT_TIMEOUT,
            finish_timeout: FINISH_TIMEOUT,
            end_pages: AtomicU64::new(0),
        }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e.into())
        }
        Err(_) => Err(ScreenError::EndedWhileStarting),
    }
}

/// What a preparation or a load leaves drawn: the splash stays until what
/// comes next replaces it, since a hidden window shows on a presented
/// frame and a play opens only once the window is shown, and so does the
/// loading state; anything else stops.
fn after_preparing(mode: Mode) -> Mode {
    match mode {
        Mode::Splash { pending } => Mode::Splash { pending },
        // The loading state stays through the chart's load and the play's
        // preparation.
        loading @ Mode::Menu {
            loading: Some(_), ..
        } => loading,
        _ => Mode::Idle,
    }
}

fn build(
    make: &mut MakeScreen,
    chart: Option<&Chart>,
    section: Option<&Section>,
) -> Result<Box<dyn Screen>, RenderError> {
    let mut s = make()?;
    if let Some(chart) = chart {
        s.load(chart, section)?;
    }
    Ok(s)
}

enum Mode {
    Idle,
    /// The launch screen, until the game sends what to draw; a select
    /// page sent before it ends ([`SPLASH`]) waits in `pending`.
    Splash {
        pending: Option<Box<MenuPage>>,
    },
    Play {
        origin: TimeUs,
        side: RenderSide,
        gauge: usize,
        first: bool,
    },
    /// The end screen: [`Run::end_page`].
    End,
    /// The select screen, or with `loading` the play screen's loading
    /// state of that loading lifetime and its lines; `changed` until the
    /// page is laid out.
    Menu {
        page: Box<MenuPage>,
        changed: bool,
        loading: Option<u64>,
    },
}

struct Run {
    screen: Option<Box<dyn Screen>>,
    /// When the render thread started: the launch screen's time runs from it.
    started: Instant,
    make: MakeScreen,
    chart: Option<Arc<Chart>>,
    /// The section of the chart a practice round draws only the targets of.
    section: Option<Section>,
    clock: QpcClock,
    view: View,
    measure: Measure,
    frames: Box<dyn FrameSink>,
    shared: Arc<Shared>,
    /// The end screen's page last taken from the slot, and its number.
    end_page: (u64, EndPage),
    /// The screen's wait granted a frame that no draw has taken yet: the
    /// end screen was left or nothing was to be drawn. Only a presented
    /// frame gives the grant back (a swap chain's waitable object, one
    /// frame of latency), so the next frame takes it instead of waiting
    /// again, which would never end; until a new screen is built.
    granted: bool,
    /// The select screen's frames (dev-tools).
    #[cfg(feature = "dev-tools")]
    menu_stats: crate::dev::MenuStats,
    /// The loading state's frames (dev-tools).
    #[cfg(feature = "dev-tools")]
    loading_stats: crate::dev::MenuStats,
}

impl Run {
    fn run(&mut self, rx: &Receiver<RenderMsg>) {
        let mut mode = Mode::Splash { pending: None };
        let mut effects: Vec<JudgeEvent> = Vec::with_capacity(self.view.effects);
        loop {
            let idle = matches!(mode, Mode::Idle) || self.screen.is_none();
            let msg = if idle {
                rx.recv().map_err(|_| TryRecvError::Disconnected)
            } else {
                rx.try_recv()
            };
            // The select screen is left: its frames are reported; and the
            // loading state's, once it is left.
            #[cfg(feature = "dev-tools")]
            if msg.is_ok() && !matches!(msg, Ok(RenderMsg::Menu(_))) {
                self.menu_stats.report("select");
            }
            #[cfg(feature = "dev-tools")]
            if msg.is_ok()
                && !matches!(
                    msg,
                    Ok(RenderMsg::Loading(_) | RenderMsg::Load { .. } | RenderMsg::Prepare(_))
                )
            {
                self.loading_stats.report("loading");
            }
            match msg {
                Ok(RenderMsg::Load {
                    chart,
                    section,
                    reply,
                }) => {
                    // What the screen draws of the chart (dev-tools): a
                    // section's targets only.
                    #[cfg(feature = "dev-tools")]
                    {
                        let notes = || chart.lanes.iter().flatten();
                        match &section {
                            Some(s) => println!(
                                "dev: render: a section's {} targets of {} notes are drawn",
                                notes().filter(|n| s.is_target(n)).count(),
                                notes().count()
                            ),
                            None => {
                                println!("dev: render: all {} notes are drawn", notes().count())
                            }
                        }
                    }
                    self.chart = Some(chart);
                    self.section = section;
                    let ready = self.prepare();
                    // A chart the screen cannot draw is refused, and the
                    // screen it dropped is built again without it, for the
                    // select screen; if that fails too, the failure is
                    // reported and the game builds it through a preparation.
                    if ready.is_err() {
                        self.chart = None;
                        self.section = None;
                        if self.prepare().is_err() {
                            self.shared.failed.store(true, Ordering::Release);
                        }
                    }
                    let _ = reply.send(ready);
                    mode = after_preparing(mode);
                    continue;
                }
                Ok(RenderMsg::Menu(page)) => {
                    mode = match mode {
                        // The launch screen shows out its time.
                        Mode::Splash { .. } if self.started.elapsed() < SPLASH => Mode::Splash {
                            pending: Some(page),
                        },
                        _ => Mode::Menu {
                            page,
                            changed: true,
                            loading: None,
                        },
                    };
                }
                Ok(RenderMsg::Loading(lifetime)) => {
                    // Lines the slot no longer holds (taken, or their
                    // lifetime ended) change nothing.
                    if let Some(page) = self.shared.take_loading(lifetime) {
                        mode = Mode::Menu {
                            page,
                            changed: true,
                            loading: Some(lifetime),
                        };
                    }
                }
                Ok(RenderMsg::EndLoading(reply)) => {
                    // Between frames: none of the loading state follows.
                    if matches!(
                        mode,
                        Mode::Menu {
                            loading: Some(_),
                            ..
                        }
                    ) {
                        mode = Mode::Idle;
                    }
                    let _ = reply.send(());
                    continue;
                }
                Ok(RenderMsg::Prepare(reply)) => {
                    let ready = self.prepare();
                    let _ = reply.send(ready);
                    mode = after_preparing(mode);
                    continue;
                }
                Ok(RenderMsg::Play {
                    side,
                    gauge,
                    measure,
                }) => {
                    let origin = measure.origin;
                    self.measure.start(*measure);
                    self.shared.first_frame.store(i64::MIN, Ordering::Release);
                    mode = Mode::Play {
                        origin,
                        side,
                        gauge,
                        first: true,
                    };
                }
                // Without a screen (it failed), the failure stays reported
                // and nothing is drawn: the game builds the screen again
                // through a preparation, which can fail.
                // Its page, or a newer one an earlier signal took; whether
                // it may still be drawn is checked before every end frame.
                Ok(RenderMsg::End) => {
                    if let Some(page) = self.shared.take_end() {
                        self.end_page = page;
                    }
                    mode = Mode::End;
                }
                Ok(RenderMsg::Finish { end, reply }) => {
                    // Judgments still queued were never drawn in this play:
                    // N/A, not missing from the figures.
                    if let Mode::Play { side, .. } = &mut mode {
                        effects.clear();
                        side.drain_effects(|e| effects.push(*e));
                        self.measure.never_drawn(&effects);
                        effects.clear();
                    }
                    let _ = reply.send(self.measure.finish(end));
                }
                Ok(RenderMsg::Exit) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => {}
            }
            let Some(screen) = &mut self.screen else {
                continue;
            };
            if screen.paced() {
                if !self.granted && !screen.wait() {
                    continue;
                }
                self.granted = true;
            } else {
                std::thread::sleep(Duration::from_millis(16));
            }
            // The end screen was left (its pages cleared): the page drawn
            // last goes, and nothing is drawn until what comes next, which
            // takes the frame granted.
            if matches!(mode, Mode::End) && self.end_page.0 <= self.shared.end_cleared() {
                self.end_page.1 = EndPage::default();
                mode = Mode::Idle;
                continue;
            }
            // Loading ended during the wait: its frame is not drawn, and
            // nothing is until what comes next, which takes the frame
            // granted.
            if let Mode::Menu {
                loading: Some(lifetime),
                ..
            } = mode
                && !self.shared.loading_current(lifetime)
            {
                mode = Mode::Idle;
                continue;
            }
            let loading_frame = matches!(
                mode,
                Mode::Menu {
                    loading: Some(_),
                    ..
                }
            );
            // The launch screen's time is over: the page waiting replaces it.
            let elapsed = self.started.elapsed();
            if elapsed >= SPLASH
                && let Mode::Splash { pending } = &mut mode
                && let Some(page) = pending.take()
            {
                mode = Mode::Menu {
                    page,
                    changed: true,
                    loading: None,
                };
            }
            let drawn = match &mut mode {
                Mode::Idle => continue,
                Mode::Splash { pending } => {
                    let Some(drawn) = screen.splash(elapsed) else {
                        // No launch screen: the page waiting, if any, at
                        // once; else the frame granted waits for what
                        // comes next.
                        mode = pending.take().map_or(Mode::Idle, |page| Mode::Menu {
                            page,
                            changed: true,
                            loading: None,
                        });
                        continue;
                    };
                    drawn.map(|presented| {
                        let timing = FrameTiming {
                            cpu: Duration::ZERO,
                            sprites: 0,
                            dropped: 0,
                            presented,
                            present_count: None,
                        };
                        (timing, None)
                    })
                }
                Mode::Play {
                    origin,
                    side,
                    gauge,
                    first,
                } => {
                    // The app region's first interval (design.md "Scope of
                    // No Allocation"): this frame's own work up to the
                    // build, before anything calls into wgpu.
                    #[cfg(feature = "dev-tools")]
                    crate::alloc_count::app_region(true);
                    effects.clear();
                    // At most what the buffer holds: the logic step can push
                    // during the drain, and the rest waits for the next
                    // frame.
                    let room = effects.capacity();
                    side.drain_effects_up_to(room, |e| effects.push(*e));
                    let now = TimeUs(self.clock.now().0 - origin.0);
                    // Hi-speed and the display offset change anytime and move
                    // the notes only: they reach the screen here and nowhere
                    // near judgment. Hi-speed changes are not in the v1 log;
                    // they arrive with the separate hi-speed stream and a
                    // format version bump (design.md "Operations unrelated to
                    // judgment go in a separate stream").
                    let t = self.view.tuning.get();
                    let overlay = self.measure.overlay();
                    let shown = overlay.iter().take_while(|l| !l.is_empty()).count();
                    #[expect(
                        clippy::indexing_slicing,
                        reason = "`shown` counts a prefix of `overlay`"
                    )]
                    let f = FrameInput {
                        now,
                        display_offset: TimeUs(t.display_offset_us),
                        visible_time: t.visible_time(),
                        view: side.view(),
                        effects: &effects,
                        gauge: *gauge,
                        overlay: &overlay[..shown],
                    };
                    screen.build(&f);
                    #[cfg(feature = "dev-tools")]
                    crate::alloc_count::app_region(false);
                    let drawn = screen.draw().map(|timing| (timing, Some(now)));
                    // The first frame that reached the screen.
                    if matches!(&drawn, Ok((t, _)) if t.presented) && *first {
                        *first = false;
                        self.shared
                            .first_frame
                            .store(self.clock.now().0, Ordering::Release);
                    }
                    drawn
                }
                Mode::Menu { page, changed, .. } => {
                    // A screen built again after a failure lays the page
                    // out again.
                    #[cfg(feature = "dev-tools")]
                    let [_, _, _, before] = crate::alloc_count::counts();
                    let drawn = if loading_frame {
                        screen.loading(page, *changed)
                    } else {
                        screen.menu(page, *changed)
                    }
                    .map(|t| (t, None));
                    #[cfg(feature = "dev-tools")]
                    if let Ok((t, _)) = &drawn {
                        let [_, _, _, after] = crate::alloc_count::counts();
                        let stats = if loading_frame {
                            &mut self.loading_stats
                        } else {
                            &mut self.menu_stats
                        };
                        stats.frame(t.cpu, after - before, *changed);
                        // What a refusal put on the screen, once drawn.
                        if *changed
                            && let Some(notice) = page.lines.iter().find(|l| {
                                l.style == crate::menu_screen::LineStyle::Notice
                                    && !l.text.is_empty()
                            })
                        {
                            println!("dev: drawn notice: {}", notice.text);
                        }
                    }
                    *changed = false;
                    drawn
                }
                Mode::End => screen.end(&self.end_page.1).map(|presented| {
                    let timing = FrameTiming {
                        cpu: Duration::ZERO,
                        sprites: 0,
                        dropped: 0,
                        presented,
                        present_count: None,
                    };
                    (timing, None)
                }),
            };
            self.granted = false;
            match drawn {
                Ok((timing, play)) => {
                    let d = Drawn {
                        play,
                        // The measured play's: none once its figures are
                        // taken, so no frame after a `Finish` is its.
                        #[cfg(feature = "dev-tools")]
                        generation: self.measure.generation(),
                        #[cfg(feature = "dev-tools")]
                        menu: matches!(mode, Mode::Menu { .. }),
                        #[cfg(feature = "dev-tools")]
                        loading: matches!(
                            mode,
                            Mode::Menu {
                                loading: Some(_),
                                ..
                            }
                        ),
                        timing,
                        statistics: screen.statistics(),
                        presentation: screen.presentation(),
                        effects: if play.is_some() { &effects } else { &[] },
                        at: self.clock.now(),
                    };
                    // The app region's second interval: a play frame's
                    // measurement. The frame sink (the frame dump) stays
                    // outside it.
                    #[cfg(feature = "dev-tools")]
                    crate::alloc_count::app_region(play.is_some());
                    self.measure.frame(&d);
                    #[cfg(feature = "dev-tools")]
                    crate::alloc_count::app_region(false);
                    self.frames.frame(screen.as_ref(), &d);
                    // The end page drawn, for a script waiting on it
                    // (dev-tools).
                    #[cfg(feature = "dev-tools")]
                    if matches!(mode, Mode::End) && timing.presented {
                        self.shared
                            .end_drawn
                            .store(self.end_page.0, Ordering::Release);
                    }
                }
                Err(e) => {
                    // What this frame drew never reached the screen.
                    if matches!(mode, Mode::Play { .. }) {
                        self.measure.failed(&effects);
                    }
                    eprintln!("render: {}", Chain(&e));
                    self.screen = None;
                    self.shared.failed.store(true, Ordering::Release);
                    mode = Mode::Idle;
                }
            }
        }
    }

    /// Resets the screen's play state, building it again after a failure.
    fn prepare(&mut self) -> Result<(), RenderError> {
        let chart = self.chart.as_deref();
        let section = self.section.as_ref();
        let ready = match (&mut self.screen, chart) {
            (Some(s), Some(chart)) => s.load(chart, section),
            (Some(_), None) => Ok(()),
            (None, _) => build(&mut self.make, chart, section).map(|s| self.screen = Some(s)),
        };
        if ready.is_ok() {
            self.shared.failed.store(false, Ordering::Release);
        } else {
            self.screen = None;
            self.granted = false;
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use bms_model::{Chart, ChartBuilder, Lane, TimeSource, TimeUs};
    use eval::{DeviceProfile, InputStart};
    use game::{Logic, LogicConfig};
    use input::{QpcClock, SourceReader};
    use render::{FrameInput, FrameTiming, RenderError, SkinError};

    use super::{MakeScreen, NoFrames, RenderMsg, Screen, ScreenError, View, spawn};
    use crate::measure::{Measure, PlayMeasure};

    fn timing(presented: bool) -> FrameTiming {
        FrameTiming {
            cpu: Duration::from_micros(50),
            sprites: 0,
            dropped: 0,
            presented,
            present_count: None,
        }
    }

    /// A measurement whose queues nothing feeds.
    fn measure(clock: QpcClock) -> Measure {
        let (_steps, steps) = rtrb::RingBuffer::new(1);
        let (_sounds, sounds) = rtrb::RingBuffer::new(1);
        let overlay = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Measure::new(clock, None, None, steps, sounds, overlay)
    }

    fn play_measure(origin: TimeUs) -> Box<PlayMeasure> {
        Box::new(PlayMeasure {
            generation: 1,
            origin,
            countdown_at: origin,
            started: origin,
            page_faults: None,
            load_from: TimeUs(0),
            retry: false,
            audio: None,
            queued_periods: None,
        })
    }

    /// A screen whose third text frame fails, and whose play frames are
    /// presented only from the fourth on, when it notes the clock.
    struct Flaky {
        text_frames: u32,
        play_frames: u32,
        clock: QpcClock,
        presented_at: Arc<AtomicI64>,
    }

    impl Screen for Flaky {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) {}
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            self.play_frames += 1;
            let presented = self.play_frames >= 4;
            if presented {
                let _ = self.presented_at.compare_exchange(
                    i64::MIN,
                    self.clock.now().0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            Ok(timing(presented))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            self.text_frames += 1;
            if self.text_frames == 3 {
                Err(RenderError::SurfaceLost)
            } else {
                Ok(true)
            }
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A menu screen, the `id`th built, whose first screen fails its third
    /// menu frame; every menu frame notes the screen and whether its page
    /// was new.
    struct Menus {
        id: u32,
        frames: u32,
        seen: Arc<std::sync::Mutex<Vec<(u32, bool)>>>,
    }

    impl Screen for Menus {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) {}
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.frames += 1;
            self.seen.lock().unwrap().push((self.id, changed));
            if self.id == 1 && self.frames == 3 {
                Err(RenderError::SurfaceLost)
            } else {
                Ok(timing(true))
            }
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The select screen is drawn without a chart, its page laid out once;
    /// a failed menu frame is reported, and after the preparation that
    /// builds the screen again, the page selection sends again is drawn
    /// there.
    #[test]
    fn a_failed_menu_frame_is_reported_and_the_page_draws_on_a_new_screen() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicU32::new(0));
        let (made, noted) = (Arc::clone(&builds), Arc::clone(&seen));
        let make: MakeScreen = Box::new(move || {
            let id = made.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Box::new(Menus {
                id,
                frames: 0,
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| link.failed()));
        assert!(link.prepare().is_ok());
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| seen.lock().unwrap().contains(&(2, false))));
        link.exit();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(&seen[..3], [(1, true), (1, false), (1, false)]);
        assert_eq!(seen[3], (2, true), "{seen:?}");
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// The loading state is drawn on through a chart's load and a
    /// preparation, answered between its frames, until the play's page
    /// replaces it; after the select page, a load leaves nothing drawn
    /// until what comes next.
    #[test]
    fn the_loading_state_is_drawn_through_the_load_and_the_preparation() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Menus {
                // Not the first build: no frame fails.
                id: 2,
                frames: 0,
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let frames = || seen.lock().unwrap().len();
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.loading().show(crate::menu_screen::MenuPage::default());
        assert!(wait_until(|| frames() >= 2));
        link.load(Arc::clone(&chart), None).unwrap();
        let loaded = frames();
        assert!(
            wait_until(|| frames() >= loaded + 3),
            "no frame after the load"
        );
        link.prepare().unwrap();
        let prepared = frames();
        assert!(
            wait_until(|| frames() >= prepared + 3),
            "no frame after the preparation"
        );
        // The select page and a load: nothing drawn after the answer.
        link.send(RenderMsg::Menu(Box::default()));
        let menu = frames();
        assert!(wait_until(|| frames() >= menu + 2));
        link.load(chart, None).unwrap();
        let after = frames();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(frames(), after);
        link.exit();
    }

    /// A menu screen whose frames stall while `stall` is set; every menu
    /// frame notes its page's first line and whether the page was new.
    struct StalledMenus {
        stall: Arc<AtomicBool>,
        stalled: Arc<AtomicBool>,
        seen: Arc<std::sync::Mutex<Vec<(String, bool)>>>,
    }

    impl Screen for StalledMenus {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) {}
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            let line = page
                .lines
                .first()
                .map_or_else(String::new, |l| l.text.clone());
            self.seen.lock().unwrap().push((line, changed));
            while self.stall.load(Ordering::Acquire) {
                self.stalled.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(1));
            }
            self.stalled.store(false, Ordering::Release);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    fn one_line(text: &str) -> crate::menu_screen::MenuPage {
        crate::menu_screen::MenuPage {
            lines: vec![crate::menu_screen::MenuLine {
                text: text.to_owned(),
                style: crate::menu_screen::LineStyle::Title,
            }],
            caret: None,
        }
    }

    /// A render thread stalled in a loading frame falls behind on nothing:
    /// of 300 pages shown meanwhile, only the newest is drawn, one new page
    /// once it resumes, and the play's preparation is answered at once
    /// after it; a page shown before loading ended is never drawn after.
    #[test]
    fn a_stalled_render_thread_draws_only_the_newest_loading_lines() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stall = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&stall), Arc::clone(&stalled));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(StalledMenus {
                stall: Arc::clone(&held),
                stalled: Arc::clone(&at),
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let sink = link.loading();
        let frames = || seen.lock().unwrap().len();
        sink.show(one_line("0"));
        assert!(wait_until(|| frames() >= 1));
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        let before = frames();
        for n in 1..=300 {
            sink.show(one_line(&n.to_string()));
        }
        stall.store(false, Ordering::Release);
        let released = Instant::now();
        link.prepare().unwrap();
        // Unbacked by a slot, 300 pages a frame each would take seconds.
        assert!(released.elapsed() < Duration::from_secs(1));
        let after: Vec<_> = seen.lock().unwrap()[before..].to_vec();
        let new: Vec<_> = after.iter().filter(|(_, changed)| *changed).collect();
        assert_eq!(new, [&("300".to_owned(), true)], "{after:?}");

        // Loading ends while the thread stalls with a page waiting: the
        // select page follows, and the waiting page is never drawn.
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        sink.show(one_line("stale"));
        link.clear_loading();
        link.send(RenderMsg::Menu(Box::new(one_line("select"))));
        stall.store(false, Ordering::Release);
        assert!(wait_until(|| seen
            .lock()
            .unwrap()
            .iter()
            .any(|(l, _)| l == "select")));
        std::thread::sleep(Duration::from_millis(100));
        link.exit();
        let seen = seen.lock().unwrap().clone();
        assert!(!seen.iter().any(|(l, _)| l == "stale"), "{seen:?}");
    }

    /// A paced screen, as a swap chain's waitable object paces one: `wait`
    /// grants a frame, blocking while `hold` is set, and the frame drawn
    /// next takes the grant; every frame notes whether it was a loading
    /// frame and its page's first line.
    struct PacedLoading {
        hold: Arc<AtomicBool>,
        waiting: Arc<AtomicBool>,
        seen: Arc<std::sync::Mutex<Vec<(bool, String)>>>,
    }

    impl PacedLoading {
        fn note(&self, loading: bool, page: &crate::menu_screen::MenuPage) -> FrameTiming {
            let line = page
                .lines
                .first()
                .map_or_else(String::new, |l| l.text.clone());
            self.seen.lock().unwrap().push((loading, line));
            timing(true)
        }
    }

    impl Screen for PacedLoading {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            while self.hold.load(Ordering::Acquire) {
                self.waiting.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(1));
            }
            self.waiting.store(false, Ordering::Release);
            true
        }
        fn paced(&self) -> bool {
            true
        }
        fn build(&mut self, _f: &FrameInput<'_>) {}
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            Ok(self.note(false, page))
        }
        fn loading(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            Ok(self.note(true, page))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The end of loading before recording opens, while the render thread
    /// waits for its next frame: the answer comes only once it can draw
    /// no loading frame any more, and none is drawn after the end began,
    /// though the wait it was in grants a frame.
    #[test]
    fn no_loading_frame_follows_the_end_of_loading() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hold = Arc::new(AtomicBool::new(false));
        let waiting = Arc::new(AtomicBool::new(false));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&hold), Arc::clone(&waiting));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(PacedLoading {
                hold: Arc::clone(&held),
                waiting: Arc::clone(&at),
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let loading = || seen.lock().unwrap().iter().filter(|(l, _)| *l).count();
        link.loading().show(one_line("L"));
        assert!(wait_until(|| loading() >= 2));
        // The render thread waits for its next frame when loading ends.
        hold.store(true, Ordering::Release);
        assert!(wait_until(|| waiting.load(Ordering::Acquire)));
        let before = loading();
        let release = {
            let hold = Arc::clone(&hold);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                hold.store(false, Ordering::Release);
            })
        };
        link.end_loading().unwrap();
        let answered = loading();
        release.join().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        link.exit();
        assert_eq!(answered, before, "a loading frame before the answer");
        assert_eq!(loading(), before, "a loading frame after the answer");
    }

    /// The end of loading is refused, so recording does not open, when the
    /// render thread stalls in a loading frame past the answer's bound, and
    /// when it is gone.
    #[test]
    fn an_unconfirmed_end_of_loading_is_an_error() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stall = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&stall), Arc::clone(&stalled));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(StalledMenus {
                stall: Arc::clone(&held),
                stalled: Arc::clone(&at),
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.set_timeouts(Duration::from_millis(200), Duration::from_secs(5));
        link.loading().show(one_line("L"));
        assert!(wait_until(|| !seen.lock().unwrap().is_empty()));
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        assert!(matches!(link.end_loading(), Err(ScreenError::NoAnswer)));
        stall.store(false, Ordering::Release);
        // The thread ends; nothing answers any more.
        link.send(RenderMsg::Exit);
        assert!(wait_until(
            || link.ended.try_recv() == Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        assert!(matches!(link.end_loading(), Err(ScreenError::Ended)));
    }

    /// A load cancelled while the render thread stalls with its signal
    /// unread, the select page sent and another chart chosen at once: the
    /// cancelled load's signal takes nothing, and the next load's lines
    /// get a signal of their own, drawn after the select page and kept
    /// through its chart's load.
    #[test]
    fn a_cancelled_loads_signal_leaves_the_next_loads_lines() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stall = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&stall), Arc::clone(&stalled));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(StalledMenus {
                stall: Arc::clone(&held),
                stalled: Arc::clone(&at),
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.send(RenderMsg::Menu(Box::new(one_line("select"))));
        assert!(wait_until(|| !seen.lock().unwrap().is_empty()));
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        // Chart A: its lines and signal, then the quit key cancels it and
        // selection is entered; chart B is chosen at once.
        link.loading().show(one_line("A"));
        link.clear_loading();
        link.send(RenderMsg::Menu(Box::new(one_line("select"))));
        link.loading().show(one_line("B"));
        stall.store(false, Ordering::Release);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(chart, None).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        link.exit();
        let seen = seen.lock().unwrap().clone();
        let lines: Vec<&str> = seen.iter().map(|(l, _)| l.as_str()).collect();
        assert!(!lines.contains(&"A"), "{lines:?}");
        let last_select = lines.iter().rposition(|l| *l == "select").unwrap();
        assert!(
            lines[last_select..].contains(&"B"),
            "B not drawn after the select page: {lines:?}"
        );
        assert_eq!(lines.last(), Some(&"B"), "{lines:?}");
    }

    /// A screen that cannot draw any chart; it notes its menu frames by the
    /// number of the build that made it.
    struct NoCharts {
        id: u32,
        seen: Arc<std::sync::Mutex<Vec<u32>>>,
    }

    impl Screen for NoCharts {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Err(RenderError::Skin(SkinError {
                reason: "no lanes".to_owned(),
            }))
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) {}
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.seen.lock().unwrap().push(self.id);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A screen with a splash page: it counts its splash and menu frames.
    struct Splashes {
        splashes: Arc<AtomicU32>,
        menus: Arc<AtomicU32>,
    }

    impl Screen for Splashes {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) {}
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn splash(&mut self, _elapsed: Duration) -> Option<Result<bool, RenderError>> {
            self.splashes.fetch_add(1, Ordering::SeqCst);
            Some(Ok(true))
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.menus.fetch_add(1, Ordering::SeqCst);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread draws the splash page from its start, before the
    /// game sends anything, and only until the first page replaces it.
    #[test]
    fn the_splash_shows_out_its_time_then_the_first_page() {
        let (splashes, menus) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let (s, m) = (Arc::clone(&splashes), Arc::clone(&menus));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Splashes {
                splashes: Arc::clone(&s),
                menus: Arc::clone(&m),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let spawned = Instant::now();
        assert!(wait_until(|| splashes.load(Ordering::SeqCst) >= 2));
        // The page, ready early, waits for the launch screen's end.
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| menus.load(Ordering::SeqCst) >= 1));
        assert!(spawned.elapsed() >= crate::menu_screen::SPLASH);
        let drawn = splashes.load(Ordering::SeqCst);
        assert!(wait_until(|| menus.load(Ordering::SeqCst) >= 3));
        link.exit();
        assert_eq!(splashes.load(Ordering::SeqCst), drawn);
    }

    /// A page that comes after the launch screen's time replaces it at
    /// once.
    #[test]
    fn a_page_after_the_splash_time_shows_at_once() {
        let (splashes, menus) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let (s, m) = (Arc::clone(&splashes), Arc::clone(&menus));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Splashes {
                splashes: Arc::clone(&s),
                menus: Arc::clone(&m),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        std::thread::sleep(crate::menu_screen::SPLASH + Duration::from_millis(100));
        assert_eq!(menus.load(Ordering::SeqCst), 0);
        let sent = Instant::now();
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| menus.load(Ordering::SeqCst) >= 1));
        assert!(
            sent.elapsed() < Duration::from_millis(500),
            "{:?}",
            sent.elapsed()
        );
        link.exit();
    }

    /// A preparation (a play's, before it opens) leaves the splash drawn,
    /// whether it comes before the splash's first frame or after: the
    /// hidden window shows on a presented frame, and the play waits for
    /// the show.
    #[test]
    fn a_preparation_keeps_the_splash_drawn() {
        for prepare_first in [true, false] {
            let splashes = Arc::new(AtomicU32::new(0));
            let s = Arc::clone(&splashes);
            let make: MakeScreen = Box::new(move || {
                Ok(Box::new(Splashes {
                    splashes: Arc::clone(&s),
                    menus: Arc::new(AtomicU32::new(0)),
                }) as Box<dyn Screen>)
            });
            let clock = QpcClock::start();
            let link = spawn(
                make,
                None,
                None,
                clock,
                view(),
                measure(clock),
                Box::new(NoFrames),
            )
            .unwrap();
            if !prepare_first {
                assert!(wait_until(|| splashes.load(Ordering::SeqCst) >= 1));
            }
            assert!(link.prepare().is_ok());
            let drawn = splashes.load(Ordering::SeqCst);
            assert!(
                wait_until(|| splashes.load(Ordering::SeqCst) >= drawn + 2),
                "prepare first: {prepare_first}"
            );
            link.exit();
        }
    }

    /// A screen that notes the section of every chart it prepares.
    struct Sections(Arc<std::sync::Mutex<Vec<Option<eval::Section>>>>);

    impl Screen for Sections {
        fn load(
            &mut self,
            _chart: &Chart,
            section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            self.0.lock().unwrap().push(section.cloned());
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) {}
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The section a chart is spawned or loaded with reaches the screen,
    /// and every later preparation (a retry's) prepares with it, until a
    /// load without one.
    #[test]
    fn load_carries_the_section_to_the_screen() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(Sections(Arc::clone(&noted))) as Box<dyn Screen>));
        let section = |s: i64| eval::Section {
            start: TimeUs(s),
            end: TimeUs(s + 1_000_000),
            lead_in: TimeUs(2_000_000),
            initial_gauges: Vec::new(),
        };
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let clock = QpcClock::start();
        let link = spawn(
            make,
            Some(Arc::clone(&chart)),
            Some(section(1)),
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.load(Arc::clone(&chart), Some(section(2))).unwrap();
        link.prepare().unwrap();
        link.load(chart, None).unwrap();
        link.prepare().unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                Some(section(1)),
                Some(section(2)),
                Some(section(2)),
                None,
                None
            ]
        );
        link.exit();
    }

    /// A chart the screen cannot draw is refused, and the select screen
    /// still draws afterwards, on a screen built again without the chart:
    /// no failure is left for the game to find.
    #[test]
    fn a_refused_chart_leaves_a_working_select_screen() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicU32::new(0));
        let (made, noted) = (Arc::clone(&builds), Arc::clone(&seen));
        let make: MakeScreen = Box::new(move || {
            let id = made.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Box::new(NoCharts {
                id,
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        assert!(matches!(
            link.load(chart, None),
            Err(ScreenError::Render(RenderError::Skin(_)))
        ));
        assert!(!link.failed());
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| seen.lock().unwrap().contains(&2)));
        assert!(!link.failed());
        link.exit();
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// A screen whose chart preparation blocks until `release` sends.
    struct Blocking {
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Screen for Blocking {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            let _ = self.release.recv();
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) {}
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A play's figures are waited for as long as the bound set says: a
    /// render thread blocked in a chart's load does not answer the play's
    /// end, and the wait gives up at the bound, far below the production
    /// one.
    #[test]
    fn a_blocked_thread_bounds_the_figures_wait() {
        let (release, blocked) = std::sync::mpsc::channel();
        let mut blocked = Some(blocked);
        let make: MakeScreen = Box::new(move || {
            let release = blocked.take().expect("built once");
            Ok(Box::new(Blocking { release }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.set_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        assert!(matches!(link.load(chart, None), Err(ScreenError::NoAnswer)));
        let started = Instant::now();
        assert!(link.finish(clock.now()).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(link.exit(), super::RenderEnd::Stuck);
        release.send(()).unwrap();
    }

    /// A chart's load that blocks the render thread bounds every wait on
    /// it: the load and a preparation give up, and the close that follows
    /// returns, saying the thread did not end, so the window stays open.
    #[test]
    fn a_blocked_load_bounds_the_preparation_and_the_close() {
        let (release, blocked) = std::sync::mpsc::channel();
        let mut blocked = Some(blocked);
        let make: MakeScreen = Box::new(move || {
            let release = blocked.take().expect("built once");
            Ok(Box::new(Blocking { release }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.set_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let started = Instant::now();
        assert!(matches!(link.load(chart, None), Err(ScreenError::NoAnswer)));
        assert!(matches!(link.prepare(), Err(ScreenError::NoAnswer)));
        assert_eq!(link.exit(), super::RenderEnd::Stuck);
        assert!(started.elapsed() < Duration::from_secs(5));
        // The thread ends once its screen does.
        release.send(()).unwrap();
    }

    fn wait_until(f: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn view() -> View {
        View {
            tuning: Arc::new(crate::tuning::LiveTuning::new(game::Tuning {
                visible_time_ms: 600,
                volume_percent: 100,
                display_offset_us: 0,
                input_offset_us: 0,
            })),
            effects: 16,
        }
    }

    /// Builds up to `ok` screens, then fails; counts the builds.
    fn factory(
        ok: u32,
        clock: QpcClock,
        presented_at: &Arc<AtomicI64>,
    ) -> (Arc<AtomicU32>, MakeScreen) {
        let builds = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&builds);
        let presented_at = Arc::clone(presented_at);
        let make = Box::new(move || {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= ok {
                Ok(Box::new(Flaky {
                    text_frames: 0,
                    play_frames: 0,
                    clock,
                    presented_at: Arc::clone(&presented_at),
                }) as Box<dyn Screen>)
            } else {
                Err(RenderError::Skin(SkinError {
                    reason: format!("build {n} failed"),
                }))
            }
        });
        (builds, make)
    }

    /// A render error passes through transparently: its text and its
    /// source are the renderer's. The thread's own failures keep theirs.
    #[test]
    fn screen_errors_keep_their_sources() {
        use std::error::Error as _;

        let skin = SkinError {
            reason: "no lanes".to_owned(),
        };
        let e = ScreenError::from(RenderError::Skin(skin.clone()));
        assert_eq!(e.to_string(), "skin");
        let source = e.source().expect("the skin error is the source");
        assert_eq!(source.downcast_ref::<SkinError>(), Some(&skin));
        assert!(source.source().is_none());
        assert_eq!(game::Chain(&e).to_string(), "skin: no lanes");

        let e = ScreenError::Spawn(std::io::Error::other("no threads"));
        assert_eq!(e.to_string(), "the render thread cannot start");
        let source = e.source().expect("the spawn error is the source");
        assert!(source.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(
            game::Chain(&e).to_string(),
            "the render thread cannot start: no threads"
        );
        assert!(ScreenError::Ended.source().is_none());
        assert!(ScreenError::EndedWhileStarting.source().is_none());
    }

    #[test]
    fn a_failed_frame_is_reported_and_only_a_preparation_builds_again() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let clock = QpcClock::start();
        let presented = Arc::new(AtomicI64::new(i64::MIN));
        let (builds, make) = factory(2, clock, &presented);
        let link = spawn(
            make,
            Some(chart),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        assert!(!link.failed());
        link.show_end(super::EndPage {
            lines: vec!["A".to_owned()],
            graphs: None,
        });
        assert!(wait_until(|| link.failed()), "the failure was not reported");
        // More text without a screen builds nothing and stays failed.
        link.show_end(super::EndPage {
            lines: vec!["A".to_owned()],
            graphs: None,
        });
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(link.failed());
        // The next preparation builds the screen again.
        assert!(link.prepare().is_ok());
        assert!(!link.failed());
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        link.show_end(super::EndPage {
            lines: vec!["B".to_owned()],
            graphs: None,
        });
        assert!(wait_until(|| link.failed()));
        // Building again fails: the preparation reports it.
        let refused = link.prepare();
        assert!(
            matches!(
                &refused,
                Err(ScreenError::Render(RenderError::Skin(SkinError { reason })))
                    if reason == "build 3 failed"
            ),
            "{refused:?}"
        );
        link.exit();
    }

    #[test]
    fn the_first_frame_is_the_first_presented_one() {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let rule = rules::tapestry();
        let device = DeviceProfile::default();
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let clock = QpcClock::start();
        let presented = Arc::new(AtomicI64::new(i64::MIN));
        let (_builds, make) = factory(1, clock, &presented);
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let origin = clock.now();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(origin),
        });
        assert!(wait_until(|| link.first_frame().is_some()));
        let first = link.first_frame().unwrap().0;
        let at = presented.load(Ordering::Acquire);
        assert_ne!(at, i64::MIN);
        assert!(first >= at, "first frame at {first}, presented at {at}");
        link.exit();
    }

    /// A screen that keeps each play frame's visible time and display
    /// offset.
    struct Inputs(Arc<std::sync::Mutex<Vec<(TimeUs, TimeUs)>>>);

    impl Screen for Inputs {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, f: &FrameInput<'_>) {
            self.0
                .lock()
                .unwrap()
                .push((f.visible_time, f.display_offset));
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread reads hi-speed and the display offset at every
    /// frame: each change reaches the frames drawn after it, and no later
    /// frame goes back. The two change one at a time, as two keys do.
    #[test]
    fn render_frames_follow_hi_speed_and_display_offset() {
        use game::Adjust;

        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let tuning = view().tuning;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let screen_seen = Arc::clone(&seen);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(Inputs(Arc::clone(&screen_seen))) as Box<dyn Screen>));
        let clock = QpcClock::start();
        let view = View {
            tuning: Arc::clone(&tuning),
            effects: 16,
        };
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view,
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let origin = clock.now();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(origin),
        });
        let start = (TimeUs(600_000), TimeUs(0));
        let faster = (TimeUs(569_000), TimeUs(0));
        let later = (TimeUs(569_000), TimeUs(1_000));
        assert!(wait_until(|| seen.lock().unwrap().contains(&start)));
        assert!(tuning.adjust(Adjust::HiSpeedUp));
        assert!(wait_until(|| seen.lock().unwrap().contains(&faster)));
        assert!(tuning.adjust(Adjust::DisplayOffsetUp));
        assert!(wait_until(|| seen.lock().unwrap().contains(&later)));
        link.exit();
        // The frames go through the three states in order, each once.
        let mut frames = seen.lock().unwrap().clone();
        frames.dedup();
        assert_eq!(frames, [start, faster, later]);
    }

    /// A screen whose every play frame fails, as a lost surface does.
    struct Lost;

    impl Screen for Lost {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) {}
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Err(RenderError::SurfaceLost)
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A judgment drawn on a play frame that fails never reaches the screen
    /// through it: the render thread counts it N/A in the play's figures.
    #[test]
    fn a_failed_play_frame_makes_its_judgments_na() {
        use eval::{ControlId, InputKind, PhysicalInputEvent, SourceId};
        use game::DeviceSettings;
        use input::DeviceKind;

        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let rule = rules::tapestry();
        let device = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
        let (mut writer, reader) = input::source_queue(16);
        // Z, lane 1's default key, pressed on the note.
        let press = PhysicalInputEvent {
            received: TimeUs(1_000_000),
            source: SourceId(0),
            kind: InputKind::Control {
                control: ControlId(0x2c),
                value: 1,
            },
        };
        writer.push(press).unwrap();
        writer.publish(TimeUs(1_000_001));
        let (mut logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            vec![reader],
            LogicConfig {
                inputs: 16,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        logic.advance_to(TimeUs(1_000_002));
        assert!(
            logic
                .events()
                .iter()
                .any(|e| matches!(e.kind, eval::JudgeEventKind::Judged { .. }))
        );
        let clock = QpcClock::start();
        let make: MakeScreen = Box::new(|| Ok(Box::new(Lost) as Box<dyn Screen>));
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let origin = clock.now();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(origin),
        });
        assert!(wait_until(|| link.failed()));
        let fig = link.finish(clock.now()).expect("the play's figures");
        assert_eq!(fig.press_to_screen_unavailable, 1);
        assert_eq!(fig.press_to_screen, None);
        link.exit();
    }

    /// Play frames the [`Stalling`] screen draws before it stalls.
    const DRAWN_FRAMES: u32 = 2;

    /// A paced screen that lets [`DRAWN_FRAMES`] play frames through and
    /// then times out every wait; it keeps each frame's effect count and
    /// how many of them were judgments.
    struct Stalling {
        waits: Arc<AtomicU32>,
        seen: Arc<std::sync::Mutex<Vec<(usize, usize)>>>,
    }

    impl Screen for Stalling {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            if self.waits.fetch_add(1, Ordering::AcqRel) < DRAWN_FRAMES {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
            false
        }
        fn paced(&self) -> bool {
            true
        }
        fn build(&mut self, f: &FrameInput<'_>) {
            let judged = f
                .effects
                .iter()
                .filter(|e| matches!(e.kind, eval::JudgeEventKind::Judged { .. }))
                .count();
            self.seen.lock().unwrap().push((f.effects.len(), judged));
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// More effects are queued than a frame's buffer holds: each frame
    /// draws at most a buffer's worth, and the rest wait in the queue.
    /// Judgments still queued when the play ends never reached the screen
    /// and are N/A in its figures, as are the drawn ones offscreen (no
    /// present count to pair them with).
    #[test]
    fn frames_draw_a_buffer_of_effects_and_the_rest_end_na() {
        use game::DeviceSettings;
        use input::DeviceKind;

        // The frame buffer the render thread makes for a view of 2 effects.
        let buffer = Vec::<eval::JudgeEvent>::with_capacity(2).capacity();
        let mut builder = ChartBuilder::new(120_000);
        for k in 0..6 {
            builder = builder.tap(Lane::new(1).unwrap(), TimeUs(1_000_000 + k * 200_000));
        }
        let chart = builder.build().unwrap();
        let rule = rules::tapestry();
        let device = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
        let (mut writer, reader) = input::source_queue(32);
        // Every note hit on time.
        for e in input::autoplay(&chart, &device.sources[0]).unwrap() {
            writer.push(e).unwrap();
        }
        writer.publish(TimeUs(3_000_000));
        let (mut logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            vec![reader],
            LogicConfig {
                inputs: 32,
                effects: 64,
            },
            Vec::new(),
        )
        .unwrap();
        // Every effect is queued before the render thread draws a frame.
        logic.advance_to(TimeUs(3_000_001));
        let judged = logic
            .events()
            .iter()
            .filter(|e| matches!(e.kind, eval::JudgeEventKind::Judged { .. }))
            .count();
        assert_eq!(judged, 6);
        // More than the drawn frames' buffers hold.
        assert!(logic.events().len() > DRAWN_FRAMES as usize * buffer);
        let waits = Arc::new(AtomicU32::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let screen = (Arc::clone(&waits), Arc::clone(&seen));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Stalling {
                waits: Arc::clone(&screen.0),
                seen: Arc::clone(&screen.1),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let view = View {
            effects: buffer,
            ..view()
        };
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view,
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        // Past its last frame: the screen has stalled.
        assert!(wait_until(|| waits.load(Ordering::Acquire) > DRAWN_FRAMES));
        let fig = link.finish(clock.now()).expect("the play's figures");
        link.exit();
        let frames = seen.lock().unwrap().clone();
        assert_eq!(frames.len(), DRAWN_FRAMES as usize);
        // Each frame drew a full buffer, never more.
        assert!(frames.iter().all(|&(n, _)| n == buffer), "{frames:?}");
        let drawn: usize = frames.iter().map(|&(_, j)| j).sum();
        assert!(drawn < judged, "{frames:?}");
        // The drawn judgments and the ones still queued at the end.
        assert_eq!(fig.press_to_screen_unavailable, judged as u64);
        assert_eq!(fig.press_to_screen, None);
    }

    /// Something that allocates, kept by `black_box` so no profile elides it.
    #[cfg(feature = "dev-tools")]
    fn allocate() {
        drop(std::hint::black_box(vec![0u8; 8]));
    }

    /// A screen that allocates in every draw and read-back, and in every
    /// build when `in_build`, as wgpu, the readback and a faulty skin would;
    /// it counts its draws and read-backs.
    #[cfg(feature = "dev-tools")]
    struct Allocating {
        in_build: bool,
        draws: Arc<AtomicU32>,
        read_backs: Arc<AtomicU32>,
    }

    #[cfg(feature = "dev-tools")]
    impl Screen for Allocating {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) {
            if self.in_build {
                allocate();
            }
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            allocate();
            self.draws.fetch_add(1, Ordering::AcqRel);
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            self.read_backs.fetch_add(1, Ordering::AcqRel);
            Some((1, 1, vec![0; 4]))
        }
    }

    /// A frame sink that reads every frame back and allocates, as the frame
    /// dump does; it counts its frames.
    #[cfg(feature = "dev-tools")]
    struct Dumping(Arc<AtomicU32>);

    #[cfg(feature = "dev-tools")]
    impl super::FrameSink for Dumping {
        fn frame(&mut self, screen: &dyn Screen, _drawn: &crate::measure::Drawn<'_>) {
            drop(std::hint::black_box(screen.read_back()));
            allocate();
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// A play drawn on [`Allocating`] into [`Dumping`], whose build
    /// allocates when `in_build` and whose measurement when `in_measure`:
    /// its app region count, and how many draws, read-backs and sunk frames
    /// ran.
    #[cfg(feature = "dev-tools")]
    fn app_region_count(in_build: bool, in_measure: bool) -> (Option<u64>, [u32; 3]) {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let counters = [(); 3].map(|()| Arc::new(AtomicU32::new(0)));
        let [draws, read_backs, sunk] = counters.clone();
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Allocating {
                in_build,
                draws: Arc::clone(&draws),
                read_backs: Arc::clone(&read_backs),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut m = measure(clock);
        if in_measure {
            m.allocate_in_frames();
        }
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            m,
            Box::new(Dumping(sunk)),
        )
        .unwrap();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        let [_, _, sunk] = &counters;
        assert!(wait_until(|| sunk.load(Ordering::Acquire) >= 5));
        let fig = link.finish(clock.now()).expect("the play's figures");
        link.exit();
        (
            fig.render_app_allocations,
            counters.map(|c| c.load(Ordering::Acquire)),
        )
    }

    /// What the screen draws, reads back and the frame sink does allocates
    /// outside the app region: the region counts none of it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_app_region_leaves_out_drawing_and_the_frame_sink() {
        let (count, [draws, read_backs, sunk]) = app_region_count(false, false);
        assert_eq!(count, Some(0));
        assert!(draws >= 5 && read_backs >= 5 && sunk >= 5);
    }

    /// An allocation in the frame's build, the region's first interval, is
    /// counted.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_app_region_counts_the_frame_build() {
        let (count, _) = app_region_count(true, false);
        assert!(count.is_some_and(|n| n > 0), "{count:?}");
    }

    /// An allocation in a play frame's measurement, the region's second
    /// interval, is counted; the build allocates nothing here.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_app_region_counts_the_frame_measurement() {
        let (count, _) = app_region_count(false, true);
        assert!(count.is_some_and(|n| n > 0), "{count:?}");
    }

    /// A play's end is a barrier for a scenario's captures: the frame drawn
    /// before the end, which reaches the capture's time, has been written
    /// when the end is answered; no frame after it counts in the play, so
    /// a capture bound to it then is never taken.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_plays_end_settles_its_captures_after_its_last_frame() {
        use crate::dev::scenario::{CaptureState, Captures, ScenarioFrames};
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (taken, late) = (dir.path().join("taken.png"), dir.path().join("late.png"));
        let captures = Captures::of(vec![
            (TimeUs(2_900_000), taken.clone()),
            (TimeUs(0), late.clone()),
        ]);
        let draws = Arc::new(AtomicU32::new(0));
        let drawn = Arc::clone(&draws);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Allocating {
                in_build: false,
                draws: Arc::clone(&drawn),
                read_backs: Arc::new(AtomicU32::new(0)),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(ScenarioFrames::new(captures.clone())),
        )
        .unwrap();
        captures.arm(0);
        captures.bind(1);
        // Chart time 3 s at the play's first frame: past the capture's.
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(TimeUs(clock.now().0 - 3_000_000)),
        });
        assert!(link.finish(clock.now()).is_some());
        assert_eq!(captures.states()[0], CaptureState::Succeeded);
        assert!(std::fs::read(&taken).unwrap().starts_with(b"\x89PNG"));
        // Bound once the play's figures are taken: the frames still drawn
        // count in no play.
        captures.arm(1);
        captures.bind(1);
        // A frame drawn after the next has started was sunk whole.
        let after = draws.load(Ordering::Acquire);
        assert!(wait_until(|| draws.load(Ordering::Acquire) >= after + 2));
        assert_eq!(captures.states()[1], CaptureState::Bound(1));
        assert!(!late.exists());
        link.exit();
    }

    /// What an end frame drew: its first line and its graphs.
    type Drew = (String, Option<Arc<game::ResultGraphs>>);

    /// A screen that notes every end page it draws; while `hold` is set,
    /// an end frame waits for `release` once.
    struct Ends {
        drew: Arc<std::sync::Mutex<Vec<Drew>>>,
        hold: Arc<std::sync::atomic::AtomicBool>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Screen for Ends {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) {}
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            panic!("an end page is drawn with its graphs")
        }
        fn end(&mut self, page: &super::EndPage) -> Result<bool, RenderError> {
            let first = page.lines.first().cloned().unwrap_or_default();
            self.drew.lock().unwrap().push((first, page.graphs.clone()));
            if self.hold.swap(false, Ordering::SeqCst) {
                let _ = self.release.recv_timeout(Duration::from_secs(10));
            }
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.drew.lock().unwrap().push(("<menu>".to_owned(), None));
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread over an [`Ends`] screen: the link, what it drew,
    /// the hold flag and the release.
    type Ended = (
        super::RenderLink,
        Arc<std::sync::Mutex<Vec<Drew>>>,
        Arc<std::sync::atomic::AtomicBool>,
        std::sync::mpsc::Sender<()>,
    );

    /// [`Ended`], started.
    fn ends() -> Ended {
        let drew = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release, released) = std::sync::mpsc::channel();
        let mut released = Some(released);
        let (noted, held) = (Arc::clone(&drew), Arc::clone(&hold));
        let make: MakeScreen = Box::new(move || {
            let release = released.take().expect("built once");
            Ok(Box::new(Ends {
                drew: Arc::clone(&noted),
                hold: Arc::clone(&held),
                release,
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        (link, drew, hold, release)
    }

    fn page(line: &str, graphs: Option<&Arc<game::ResultGraphs>>) -> super::EndPage {
        super::EndPage {
            lines: vec![line.to_owned()],
            graphs: graphs.cloned(),
        }
    }

    fn graphs(reached: u16) -> Arc<game::ResultGraphs> {
        Arc::new(game::ResultGraphs {
            reached,
            ..game::ResultGraphs::EMPTY
        })
    }

    /// The page's graphs reach the screen with its lines; a page with new
    /// lines and the same graphs (a settings change) draws them again; a
    /// page without (a refused view) draws none.
    #[test]
    fn end_pages_reach_the_screen_with_their_graphs() {
        let (link, drew, _, _) = ends();
        let g = graphs(7);
        let last = |line: &str| {
            let drew = Arc::clone(&drew);
            let line = line.to_owned();
            move || drew.lock().unwrap().last().is_some_and(|d| d.0 == line)
        };
        link.show_end(page("A", Some(&g)));
        assert!(wait_until(last("A")));
        link.show_end(page("B", Some(&g)));
        assert!(wait_until(last("B")));
        link.show_end(page("C", None));
        assert!(wait_until(last("C")));
        let drew = drew.lock().unwrap().clone();
        let by = |line: &str| drew.iter().find(|d| d.0 == line).unwrap().1.clone();
        assert!(Arc::ptr_eq(&by("A").unwrap(), &g));
        assert!(Arc::ptr_eq(&by("B").unwrap(), &g));
        assert!(by("C").is_none());
        link.exit();
    }

    /// While the render thread is stuck in a frame, pages sent meanwhile
    /// replace each other in the slot: each replaced one is dropped at
    /// once, so one waits whatever the stall; once the frame ends, the
    /// newest is drawn, and the one drawn before is dropped.
    #[test]
    fn end_pages_coalesce_while_the_render_thread_stalls() {
        let (link, drew, hold, release) = ends();
        let first = graphs(1);
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("first", Some(&first)));
        // Held inside the first page's frame.
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        let sent: Vec<_> = (2..=6).map(graphs).collect();
        for (i, g) in sent.iter().enumerate() {
            link.show_end(page(&format!("page {i}"), Some(g)));
        }
        // The render thread holds the first page (and the screen's note
        // of it); the slot the newest.
        assert_eq!(Arc::strong_count(&first), 3);
        for g in &sent[..4] {
            assert_eq!(Arc::strong_count(g), 1);
        }
        assert_eq!(Arc::strong_count(&sent[4]), 2);
        release.send(()).unwrap();
        assert!(wait_until(|| {
            drew.lock().unwrap().last().is_some_and(|d| d.0 == "page 4")
        }));
        let drew_lines: Vec<String> = drew.lock().unwrap().iter().map(|d| d.0.clone()).collect();
        assert!(
            drew_lines.iter().all(|l| l == "first" || l == "page 4"),
            "{drew_lines:?}"
        );
        // The page drawn before went with the newer one taken: only the
        // screen's note of it is left.
        assert_eq!(Arc::strong_count(&first), 2);
        link.exit();
    }

    /// Left for a retry (a preparation follows) or for a menu screen
    /// (selection, the practice page): once the rig clears the slot, a
    /// page still waiting in it is never drawn, nor the page drawn before,
    /// whether the render thread was stuck in that page's frame or not.
    #[test]
    fn a_left_end_screen_draws_no_stale_page() {
        for (stalled, retry) in [(true, true), (true, false), (false, true), (false, false)] {
            let (link, drew, hold, release) = ends();
            let old = graphs(1);
            hold.store(stalled, Ordering::SeqCst);
            link.show_end(page("old", Some(&old)));
            assert!(wait_until(|| !drew.lock().unwrap().is_empty()));
            let waiting = graphs(2);
            if stalled {
                // A settings change while the frame is stuck: its page waits.
                link.show_end(page("waiting", Some(&waiting)));
                assert_eq!(Arc::strong_count(&waiting), 2);
            }
            // What the rig does on leaving: the slot is emptied first.
            link.clear_end();
            assert_eq!(Arc::strong_count(&waiting), 1);
            if retry {
                if stalled {
                    release.send(()).unwrap();
                }
                assert!(link.prepare().is_ok());
            } else {
                link.send(RenderMsg::Menu(Box::default()));
                if stalled {
                    release.send(()).unwrap();
                }
                assert!(wait_until(|| {
                    drew.lock().unwrap().iter().any(|d| d.0 == "<menu>")
                }));
            }
            let ends = |drew: &[Drew]| drew.iter().filter(|d| d.0 != "<menu>").count();
            let before = ends(&drew.lock().unwrap());
            std::thread::sleep(Duration::from_millis(100));
            let lines: Vec<String> = drew.lock().unwrap().iter().map(|d| d.0.clone()).collect();
            assert!(!lines.iter().any(|l| l == "waiting"), "{lines:?}");
            // No end frame once the screen was left.
            assert_eq!(ends(&drew.lock().unwrap()), before, "{lines:?}");
            if stalled {
                // Only the stuck frame of the old page.
                assert_eq!(before, 1, "{lines:?}");
            }
            if let Some(m) = lines.iter().position(|l| l == "<menu>") {
                assert!(lines[m..].iter().all(|l| l == "<menu>"), "{lines:?}");
            }
            // The render thread let go of the old page too: only the
            // screen's notes of it are left.
            let notes = drew
                .lock()
                .unwrap()
                .iter()
                .filter(|d| d.1.as_ref().is_some_and(|g| Arc::ptr_eq(g, &old)))
                .count();
            if stalled {
                assert_eq!(Arc::strong_count(&old), 1 + notes, "{retry}");
            }
            link.exit();
        }
    }
    /// A script's `drawn` step: the wait ends once the page shown last has
    /// been presented, and gives up at its bound while the render thread
    /// is stuck before drawing it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn waiting_for_the_end_page_ends_when_drawn_or_at_its_bound() {
        let (link, drew, hold, release) = ends();
        link.show_end(page("first", None));
        assert!(link.wait_end_drawn(Duration::from_secs(10)));
        assert!(!drew.lock().unwrap().is_empty());
        // Stuck in the next page's frame: the page after it is not drawn.
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("stuck", None));
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        link.show_end(page("waiting", None));
        let began = Instant::now();
        assert!(!link.wait_end_drawn(Duration::from_millis(200)));
        assert!(began.elapsed() >= Duration::from_millis(200));
        release.send(()).unwrap();
        assert!(link.wait_end_drawn(Duration::from_secs(10)));
        assert_eq!(drew.lock().unwrap().last().unwrap().0, "waiting");
        link.exit();
    }

    /// Two pages coalesced: the first signal took and drew the newer page,
    /// and the second signal is still pending when the end screen is left.
    /// Neither the pending signal nor further frames draw that page again;
    /// left with no message pending, the page drawn last stops too.
    #[test]
    fn a_cleared_page_is_not_drawn_again() {
        let (link, drew, hold, release) = ends();
        let count = |line: &str| drew.lock().unwrap().iter().filter(|d| d.0 == line).count();
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("page 1", None));
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        link.show_end(page("page 2", None));
        link.show_end(page("page 3", None));
        // The next end frame, page 3's for the signal of page 2, holds too.
        hold.store(true, Ordering::SeqCst);
        release.send(()).unwrap();
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        assert_eq!(count("page 3"), 1);
        // Page 3's own signal is pending.
        link.clear_end();
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(count("page 3"), 1, "{:?}", drew.lock().unwrap());
        assert_eq!(count("page 2"), 0);

        // Nothing pending: the page drawn last keeps being drawn until the
        // end screen is left, then stops.
        let (link, drew, _, _) = ends();
        let count = |line: &str| drew.lock().unwrap().iter().filter(|d| d.0 == line).count();
        link.show_end(page("shown", None));
        assert!(wait_until(|| count("shown") >= 3));
        link.clear_end();
        std::thread::sleep(Duration::from_millis(50));
        let after = count("shown");
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(count("shown"), after);
        link.exit();
    }

    /// A retry in the production order: the rig leaves the end screen and
    /// then prepares the screen ([`crate::play::prepare_render`]), while
    /// the render thread is stuck in an end frame with a newer page
    /// waiting; the preparation's reply comes without that page drawn.
    #[test]
    fn a_retry_leaves_the_end_screen_before_preparing() {
        let (link, drew, hold, release) = ends();
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("shown", None));
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        link.show_end(page("waiting", None));
        let mut end_page = Some(super::EndPage::default());
        let resume = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            release.send(()).unwrap();
        });
        assert!(crate::play::prepare_render(&mut end_page, &link).is_ok());
        resume.join().unwrap();
        assert_eq!(end_page, None);
        std::thread::sleep(Duration::from_millis(100));
        let lines: Vec<String> = drew.lock().unwrap().iter().map(|d| d.0.clone()).collect();
        assert_eq!(lines, ["shown"]);
        link.exit();
    }

    /// A screen paced as a swap chain's waitable object with one frame of
    /// latency: a wait takes the one grant, a frame drawn gives it back,
    /// and with none a wait times out (after 5 ms). While `hold` is set,
    /// the wait that takes the grant waits for `release` once.
    struct Paced {
        drew: Arc<std::sync::Mutex<Vec<String>>>,
        grants: AtomicU32,
        hold: Arc<std::sync::atomic::AtomicBool>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Paced {
        fn drawn(&self, what: String) {
            self.drew.lock().unwrap().push(what);
            self.grants.store(1, Ordering::SeqCst);
        }
    }

    impl Screen for Paced {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            let took = self
                .grants
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |g| g.checked_sub(1));
            if took.is_err() {
                std::thread::sleep(Duration::from_millis(5));
                return false;
            }
            if self.hold.swap(false, Ordering::SeqCst) {
                let _ = self.release.recv_timeout(Duration::from_secs(10));
            }
            true
        }
        fn paced(&self) -> bool {
            true
        }
        fn build(&mut self, _f: &FrameInput<'_>) {}
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            self.drawn("<play>".to_owned());
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            panic!("an end page is drawn with its graphs")
        }
        fn end(&mut self, page: &super::EndPage) -> Result<bool, RenderError> {
            self.drawn(page.lines.first().cloned().unwrap_or_default());
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.drawn("<menu>".to_owned());
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread over a [`Paced`] screen, the end page `result`
    /// drawn and the thread held in the wait for its next frame: the link,
    /// what it drew and the release.
    fn held_in_a_wait() -> (
        super::RenderLink,
        Arc<std::sync::Mutex<Vec<String>>>,
        std::sync::mpsc::Sender<()>,
    ) {
        let drew = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release, released) = std::sync::mpsc::channel();
        let mut released = Some(released);
        let (noted, held) = (Arc::clone(&drew), Arc::clone(&hold));
        let make: MakeScreen = Box::new(move || {
            let release = released.take().expect("built once");
            Ok(Box::new(Paced {
                drew: Arc::clone(&noted),
                grants: AtomicU32::new(1),
                hold: Arc::clone(&held),
                release,
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.show_end(page("result", None));
        assert!(drawn(&drew, "result"));
        hold.store(true, Ordering::SeqCst);
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        (link, drew, release)
    }

    /// Whether `drew` noted `what` within the bound.
    fn drawn(drew: &std::sync::Mutex<Vec<String>>, what: &str) -> bool {
        wait_until(|| drew.lock().unwrap().iter().any(|d| d == what))
    }

    /// Selection or the practice page entered while the render thread
    /// waits for a frame of the end screen, in the rig's order: the end
    /// screen is left ([`super::RenderLink::clear_end`]), then the page is
    /// sent. The frame the wait granted goes to the page instead of being
    /// lost with the end page, so on a screen paced by one frame of
    /// latency the page is drawn, and no end frame after it.
    #[test]
    fn a_menu_entered_during_an_end_frame_wait_is_drawn() {
        let (link, drew, release) = held_in_a_wait();
        link.clear_end();
        link.send(RenderMsg::Menu(Box::default()));
        release.send(()).unwrap();
        assert!(drawn(&drew, "<menu>"), "{:?}", drew.lock().unwrap());
        let drew = drew.lock().unwrap().clone();
        let menu = drew.iter().position(|d| d == "<menu>").unwrap();
        assert!(drew[menu..].iter().all(|d| d != "result"), "{drew:?}");
        link.exit();
    }

    /// [`a_menu_entered_during_an_end_frame_wait_is_drawn`] for a retry:
    /// the end screen is left as the screen is prepared
    /// ([`crate::play::prepare_render`]), and the next play's frames are
    /// drawn.
    #[test]
    fn a_retry_during_an_end_frame_wait_draws_its_play() {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let (link, drew, release) = held_in_a_wait();
        let resume = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            release.send(()).unwrap();
        });
        let mut end_page = Some(super::EndPage::default());
        assert!(crate::play::prepare_render(&mut end_page, &link).is_ok());
        resume.join().unwrap();
        let clock = QpcClock::start();
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        assert!(drawn(&drew, "<play>"), "{:?}", drew.lock().unwrap());
        link.exit();
    }
}
