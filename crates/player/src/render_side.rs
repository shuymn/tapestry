//! The render thread (design.md "Overall Architecture", "Rendering and Skins"):
//! it owns the screen, readies it for each play on request and says when it is
//! ready, draws each play from the logic step's effects and play view at the
//! play's chart time, and draws the end screen between plays and the select
//! screen ([`crate::menu_screen`]) on song selection. With selection the
//! chart is not known at the start: each chosen chart is loaded on request
//! ([`RenderMsg::Load`]) and stays until the next. A window target
//! paces frames on the swap chain's waitable object; an offscreen one at about
//! 60 Hz.
//!
//! A frame that fails (the surface was lost) drops the screen and tells
//! the game thread, which ends the play; the next play's preparation builds
//! the screen again, and if that fails the run ends with the error.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use bms_model::{Chart, TimeSource, TimeUs};
use eval::JudgeEvent;
use game::{Chain, RenderSide};
use input::QpcClock;
use render::measure::{FrameStatistics, Presentation};
use render::{FrameInput, FrameTiming, RenderError};

use crate::measure::{Drawn, Figures, Measure, PlayMeasure};
use crate::menu_screen::MenuPage;
use crate::tuning::LiveTuning;

/// What draws: the renderer, or a double in tests.
pub trait Screen: Send {
    /// Prepares `chart` and resets the play state.
    ///
    /// # Errors
    ///
    /// Why it cannot draw the chart.
    fn load(&mut self, chart: &Chart) -> Result<(), RenderError>;
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
        /// Where the answer goes.
        reply: mpsc::SyncSender<Result<(), RenderError>>,
    },
    /// Draw the select screen's page instead of a play.
    Menu(Box<MenuPage>),
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
    /// Draw these lines instead of a play.
    Text(Vec<String>),
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
}

impl RenderLink {
    /// Sends a message; a thread that already ended ignores it.
    pub fn send(&self, msg: RenderMsg) {
        let _ = self.tx.send(msg);
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
    /// Why the screen cannot be built or cannot draw the chart, and
    /// [`ScreenError::NoAnswer`] after [`LOAD_TIMEOUT`]; a late answer goes
    /// nowhere.
    pub fn load(&self, chart: Arc<Chart>) -> Result<(), ScreenError> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Load { chart, reply });
        self.answer(&rx)
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
            let screen = match build(&mut make, chart.as_deref()) {
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
                make,
                chart,
                clock,
                view,
                measure,
                frames,
                shared: thread_shared,
                #[cfg(feature = "dev-tools")]
                menu_stats: crate::dev::MenuStats::default(),
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
        }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e.into())
        }
        Err(_) => Err(ScreenError::EndedWhileStarting),
    }
}

fn build(make: &mut MakeScreen, chart: Option<&Chart>) -> Result<Box<dyn Screen>, RenderError> {
    let mut s = make()?;
    if let Some(chart) = chart {
        s.load(chart)?;
    }
    Ok(s)
}

enum Mode {
    Idle,
    Play {
        origin: TimeUs,
        side: RenderSide,
        gauge: usize,
        first: bool,
    },
    Text(Vec<String>),
    /// The select screen; `changed` until its page is laid out.
    Menu {
        page: Box<MenuPage>,
        changed: bool,
    },
}

struct Run {
    screen: Option<Box<dyn Screen>>,
    make: MakeScreen,
    chart: Option<Arc<Chart>>,
    clock: QpcClock,
    view: View,
    measure: Measure,
    frames: Box<dyn FrameSink>,
    shared: Arc<Shared>,
    /// The select screen's frames (dev-tools).
    #[cfg(feature = "dev-tools")]
    menu_stats: crate::dev::MenuStats,
}

impl Run {
    fn run(&mut self, rx: &Receiver<RenderMsg>) {
        let mut mode = Mode::Idle;
        let mut effects: Vec<JudgeEvent> = Vec::with_capacity(self.view.effects);
        loop {
            let idle = matches!(mode, Mode::Idle) || self.screen.is_none();
            let msg = if idle {
                rx.recv().map_err(|_| TryRecvError::Disconnected)
            } else {
                rx.try_recv()
            };
            // The select screen is left: its frames are reported.
            #[cfg(feature = "dev-tools")]
            if msg.is_ok() && !matches!(msg, Ok(RenderMsg::Menu(_))) {
                self.menu_stats.report();
            }
            match msg {
                Ok(RenderMsg::Load { chart, reply }) => {
                    self.chart = Some(chart);
                    let ready = self.prepare();
                    // A chart the screen cannot draw is refused, and the
                    // screen it dropped is built again without it, for the
                    // select screen; if that fails too, the failure is
                    // reported and the game builds it through a preparation.
                    if ready.is_err() {
                        self.chart = None;
                        if self.prepare().is_err() {
                            self.shared.failed.store(true, Ordering::Release);
                        }
                    }
                    let _ = reply.send(ready);
                    mode = Mode::Idle;
                    continue;
                }
                Ok(RenderMsg::Menu(page)) => {
                    mode = Mode::Menu {
                        page,
                        changed: true,
                    };
                }
                Ok(RenderMsg::Prepare(reply)) => {
                    let ready = self.prepare();
                    let _ = reply.send(ready);
                    mode = Mode::Idle;
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
                Ok(RenderMsg::Text(lines)) => mode = Mode::Text(lines),
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
                if !screen.wait() {
                    continue;
                }
            } else {
                std::thread::sleep(Duration::from_millis(16));
            }
            let drawn = match &mut mode {
                Mode::Idle => continue,
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
                Mode::Menu { page, changed } => {
                    // A screen built again after a failure lays the page
                    // out again.
                    #[cfg(feature = "dev-tools")]
                    let [_, _, _, before] = crate::alloc_count::counts();
                    let drawn = screen.menu(page, *changed).map(|t| (t, None));
                    #[cfg(feature = "dev-tools")]
                    if let Ok((t, _)) = &drawn {
                        let [_, _, _, after] = crate::alloc_count::counts();
                        self.menu_stats.frame(t.cpu, after - before, *changed);
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
                Mode::Text(lines) => {
                    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
                    screen.text(&refs).map(|presented| {
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
            };
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
        let ready = match (&mut self.screen, chart) {
            (Some(s), Some(chart)) => s.load(chart),
            (Some(_), None) => Ok(()),
            (None, _) => build(&mut self.make, chart).map(|s| self.screen = Some(s)),
        };
        if ready.is_ok() {
            self.shared.failed.store(false, Ordering::Release);
        } else {
            self.screen = None;
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
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
        fn load(&mut self, _chart: &Chart) -> Result<(), RenderError> {
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
        fn load(&mut self, _chart: &Chart) -> Result<(), RenderError> {
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

    /// A screen that cannot draw any chart; it notes its menu frames by the
    /// number of the build that made it.
    struct NoCharts {
        id: u32,
        seen: Arc<std::sync::Mutex<Vec<u32>>>,
    }

    impl Screen for NoCharts {
        fn load(&mut self, _chart: &Chart) -> Result<(), RenderError> {
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
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        assert!(matches!(
            link.load(chart),
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
        fn load(&mut self, _chart: &Chart) -> Result<(), RenderError> {
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
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.set_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        assert!(matches!(link.load(chart), Err(ScreenError::NoAnswer)));
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
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.set_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let started = Instant::now();
        assert!(matches!(link.load(chart), Err(ScreenError::NoAnswer)));
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
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        assert!(!link.failed());
        link.send(RenderMsg::Text(vec!["A".to_owned()]));
        assert!(wait_until(|| link.failed()), "the failure was not reported");
        // More text without a screen builds nothing and stays failed.
        link.send(RenderMsg::Text(vec!["A".to_owned()]));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(link.failed());
        // The next preparation builds the screen again.
        assert!(link.prepare().is_ok());
        assert!(!link.failed());
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        link.send(RenderMsg::Text(vec!["B".to_owned()]));
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
        fn load(&mut self, _chart: &Chart) -> Result<(), RenderError> {
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
        fn load(&mut self, _chart: &Chart) -> Result<(), RenderError> {
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
        fn load(&mut self, _chart: &Chart) -> Result<(), RenderError> {
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
        fn load(&mut self, _chart: &Chart) -> Result<(), RenderError> {
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
}
