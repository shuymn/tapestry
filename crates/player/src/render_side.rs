//! The render thread (D8, D9): it owns the screen, readies it for each
//! play on request and says when it is ready, draws each play from the
//! logic step's effects and play view at the play's chart time, and draws
//! the end screen between plays. A window target paces frames on the swap
//! chain's waitable object; an offscreen one at about 60 Hz.
//!
//! A frame that fails (the surface was lost) drops the screen and tells
//! the game thread, which ends the play; the next play's preparation builds
//! the screen again, and if that fails the run ends with the error.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use bms_model::{Chart, TimeSource, TimeUs};
use eval::JudgeEvent;
use game::RenderSide;
use input::QpcClock;
use render::measure::FrameStatistics;
use render::{FrameInput, FrameTiming, Renderer};

use crate::measure::{Drawn, Figures, Measure, PlayMeasure};
use crate::tuning::LiveTuning;

/// What draws: the renderer, or a double in tests.
pub trait Screen: Send {
    /// Prepares `chart` and resets the play state.
    ///
    /// # Errors
    ///
    /// Why it cannot draw the chart.
    fn load(&mut self, chart: &Chart) -> Result<(), String>;
    /// Waits until a frame may start; `false` on a timeout. Returns at
    /// once without a frame latency waitable object.
    fn wait(&self) -> bool;
    /// Whether [`wait`](Self::wait) paces frames.
    fn paced(&self) -> bool;
    /// Draws one play frame; [`FrameTiming::presented`] is `false` when it
    /// was not presented (the swap chain image timed out or the window is
    /// occluded).
    ///
    /// # Errors
    ///
    /// Why the frame failed: the screen must be built again.
    fn frame(&mut self, f: &FrameInput<'_>) -> Result<FrameTiming, String>;
    /// The swap chain's latest frame statistics, raw; `None` without a
    /// swap chain or when the call failed.
    fn statistics(&self) -> Option<FrameStatistics> {
        None
    }
    /// Draws lines of text, as [`frame`](Self::frame).
    ///
    /// # Errors
    ///
    /// As [`frame`](Self::frame).
    fn text(&mut self, lines: &[&str]) -> Result<bool, String>;
    /// The last frame's pixels (RGBA8) and size, for an offscreen target.
    #[cfg(feature = "dev-tools")]
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)>;
}

impl Screen for Renderer {
    fn load(&mut self, chart: &Chart) -> Result<(), String> {
        Self::load(self, chart).map_err(|e| e.to_string())
    }

    fn wait(&self) -> bool {
        self.wait_for_frame(100)
    }

    fn paced(&self) -> bool {
        self.has_frame_waitable()
    }

    fn frame(&mut self, f: &FrameInput<'_>) -> Result<FrameTiming, String> {
        Self::frame(self, f).map_err(|e| e.to_string())
    }

    fn statistics(&self) -> Option<FrameStatistics> {
        self.frame_statistics()
    }

    fn text(&mut self, lines: &[&str]) -> Result<bool, String> {
        self.text_frame(lines)
            .map(|t| t.presented)
            .map_err(|e| e.to_string())
    }

    #[cfg(feature = "dev-tools")]
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        let (w, h) = self.size();
        let pixels = Self::read_back(self);
        (!pixels.is_empty()).then_some((w, h, pixels))
    }
}

/// Builds the screen on the render thread, at startup and after a failure.
pub type MakeScreen = Box<dyn FnMut() -> Result<Box<dyn Screen>, String> + Send>;

/// What the game thread tells the render thread.
pub enum RenderMsg {
    /// Ready the screen for a play (build it again if it failed) and
    /// answer when ready.
    Prepare(mpsc::SyncSender<Result<(), String>>),
    /// A play starts: chart time 0 at `origin` on the clock.
    Play {
        /// Where chart time 0 lies.
        origin: TimeUs,
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

/// How long the game thread waits for a play's figures.
const FINISH_TIMEOUT: Duration = Duration::from_secs(2);

/// Something that looks at frames as they are drawn: the measurement
/// (D11) and the dev-tools frame dump. Called after every frame, presented
/// or not.
pub trait FrameSink: Send {
    /// Called after each frame.
    fn frame(&mut self, screen: &dyn Screen, drawn: &Drawn<'_>);
}

impl FrameSink for Measure {
    fn frame(&mut self, _screen: &dyn Screen, drawn: &Drawn<'_>) {
        Self::frame(self, drawn);
    }
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

/// The game thread's end of the render thread.
pub struct RenderLink {
    tx: Sender<RenderMsg>,
    shared: Arc<Shared>,
    handle: JoinHandle<()>,
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
    pub fn prepare(&self) -> Result<(), String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Prepare(tx));
        rx.recv()
            .unwrap_or_else(|_| Err("the render thread ended".to_owned()))
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
        rx.recv_timeout(FINISH_TIMEOUT).ok().flatten()
    }

    /// When the current play's first frame was drawn.
    pub fn first_frame(&self) -> Option<TimeUs> {
        let t = self.shared.first_frame.load(Ordering::Acquire);
        (t != i64::MIN).then_some(TimeUs(t))
    }

    /// Ends the thread and waits for it.
    pub fn exit(self) {
        self.send(RenderMsg::Exit);
        let _ = self.handle.join();
    }
}

/// Starts the render thread; `make` builds the screen on it, and the chart
/// is prepared before this returns, so every upload is done before the
/// first countdown (D9).
///
/// # Errors
///
/// Why the screen could not be built or could not draw the chart.
pub fn spawn(
    mut make: MakeScreen,
    chart: Arc<Chart>,
    clock: QpcClock,
    view: View,
    measure: Measure,
    frames: Box<dyn FrameSink>,
) -> Result<RenderLink, String> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let shared = Arc::new(Shared {
        failed: AtomicBool::new(false),
        first_frame: AtomicI64::new(i64::MIN),
    });
    let thread_shared = Arc::clone(&shared);
    let handle = std::thread::Builder::new()
        .name("render".into())
        .spawn(move || {
            #[cfg(feature = "dev-tools")]
            crate::alloc_count::tag(crate::alloc_count::Tag::Render);
            let screen = build(&mut make, &chart);
            let ready = screen.as_ref().map(|_| ()).map_err(Clone::clone);
            let _ = ready_tx.send(ready);
            if let Ok(screen) = screen {
                let mut r = Run {
                    screen: Some(screen),
                    make,
                    chart,
                    clock,
                    view,
                    measure,
                    frames,
                    shared: thread_shared,
                };
                r.run(&rx);
            }
        })
        .map_err(|e| format!("the render thread cannot start: {e}"))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(RenderLink { tx, shared, handle }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => Err("the render thread ended while starting".to_owned()),
    }
}

fn build(make: &mut MakeScreen, chart: &Chart) -> Result<Box<dyn Screen>, String> {
    let mut s = make()?;
    s.load(chart)?;
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
}

struct Run {
    screen: Option<Box<dyn Screen>>,
    make: MakeScreen,
    chart: Arc<Chart>,
    clock: QpcClock,
    view: View,
    measure: Measure,
    frames: Box<dyn FrameSink>,
    shared: Arc<Shared>,
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
            match msg {
                Ok(RenderMsg::Prepare(reply)) => {
                    let ready = self.prepare();
                    let _ = reply.send(ready);
                    mode = Mode::Idle;
                    continue;
                }
                Ok(RenderMsg::Play {
                    origin,
                    side,
                    gauge,
                    measure,
                }) => {
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
                    effects.clear();
                    side.drain_effects(|e| effects.push(*e));
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
                    let f = FrameInput {
                        now,
                        display_offset: TimeUs(t.display_offset_us),
                        visible_time: t.visible_time(),
                        view: side.view(),
                        effects: &effects,
                        gauge: *gauge,
                        overlay: &overlay[..shown],
                    };
                    let drawn = screen.frame(&f).map(|timing| (timing, Some(now)));
                    // The first frame that reached the screen.
                    if matches!(&drawn, Ok((t, _)) if t.presented) && *first {
                        *first = false;
                        self.shared
                            .first_frame
                            .store(self.clock.now().0, Ordering::Release);
                    }
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
                        timing,
                        statistics: screen.statistics(),
                        effects: if play.is_some() { &effects } else { &[] },
                        at: self.clock.now(),
                    };
                    FrameSink::frame(&mut self.measure, screen.as_ref(), &d);
                    self.frames.frame(screen.as_ref(), &d);
                }
                Err(e) => {
                    eprintln!("render: {e}");
                    self.screen = None;
                    self.shared.failed.store(true, Ordering::Release);
                    mode = Mode::Idle;
                }
            }
        }
    }

    /// Resets the screen's play state, building it again after a failure.
    fn prepare(&mut self) -> Result<(), String> {
        let ready = match &mut self.screen {
            Some(s) => s.load(&self.chart),
            None => build(&mut self.make, &self.chart).map(|s| self.screen = Some(s)),
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
    use render::{FrameInput, FrameTiming};

    use super::{MakeScreen, NoFrames, RenderMsg, Screen, View, spawn};
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
            load_from: TimeUs(0),
            retry: false,
            underruns: None,
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
        fn load(&mut self, _chart: &Chart) -> Result<(), String> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn frame(&mut self, _f: &FrameInput<'_>) -> Result<FrameTiming, String> {
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
        fn text(&mut self, _lines: &[&str]) -> Result<bool, String> {
            self.text_frames += 1;
            if self.text_frames == 3 {
                Err("surface lost".to_owned())
            } else {
                Ok(true)
            }
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
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
                Err(format!("build {n} failed"))
            }
        });
        (builds, make)
    }

    #[test]
    fn a_failed_frame_is_reported_and_only_a_preparation_builds_again() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let clock = QpcClock::start();
        let presented = Arc::new(AtomicI64::new(i64::MIN));
        let (builds, make) = factory(2, clock, &presented);
        let link = spawn(
            make,
            chart,
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
        assert_eq!(link.prepare(), Ok(()));
        assert!(!link.failed());
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        link.send(RenderMsg::Text(vec!["B".to_owned()]));
        assert!(wait_until(|| link.failed()));
        // Building again fails: the preparation reports it.
        assert_eq!(link.prepare(), Err("build 3 failed".to_owned()));
        link.exit();
    }

    #[test]
    fn the_first_frame_is_the_first_presented_one() {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let rule = rules::custom();
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
            Arc::new(chart.clone()),
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let origin = clock.now();
        link.send(RenderMsg::Play {
            origin,
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
        fn load(&mut self, _chart: &Chart) -> Result<(), String> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn frame(&mut self, f: &FrameInput<'_>) -> Result<FrameTiming, String> {
            self.0
                .lock()
                .unwrap()
                .push((f.visible_time, f.display_offset));
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, String> {
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
            Arc::new(chart.clone()),
            clock,
            view,
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let (rule, device) = (rules::custom(), DeviceProfile::default());
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
            origin,
            side,
            gauge: 0,
            measure: play_measure(origin),
        });
        let start = (TimeUs(600_000), TimeUs(0));
        let faster = (TimeUs(575_000), TimeUs(0));
        let later = (TimeUs(575_000), TimeUs(1_000));
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
}
