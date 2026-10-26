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
use render::{FrameInput, Renderer};

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
    /// Draws one play frame.
    ///
    /// # Errors
    ///
    /// Why the frame failed: the screen must be built again.
    fn frame(&mut self, f: &FrameInput<'_>) -> Result<(), String>;
    /// Draws lines of text.
    ///
    /// # Errors
    ///
    /// As [`frame`](Self::frame).
    fn text(&mut self, lines: &[&str]) -> Result<(), String>;
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

    fn frame(&mut self, f: &FrameInput<'_>) -> Result<(), String> {
        Self::frame(self, f).map(|_| ()).map_err(|e| e.to_string())
    }

    fn text(&mut self, lines: &[&str]) -> Result<(), String> {
        self.text_frame(lines)
            .map(|_| ())
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
    },
    /// Draw these lines instead of a play.
    Text(Vec<String>),
    /// The run ends.
    Exit,
}

/// The fixed inputs of every play's frames.
#[derive(Clone, Copy, Debug)]
pub struct View {
    /// The display latency offset.
    pub display_offset: TimeUs,
    /// Hi-speed as the visible time.
    pub visible_time: TimeUs,
    /// The effect queue's capacity: the most effects one frame drains.
    pub effects: usize,
}

/// Something that looks at frames as they are drawn: the dev-tools frame
/// dump. `play` is the chart time of a play frame, `None` for the end
/// screen.
pub trait FrameSink: Send {
    /// Called after each frame.
    fn frame(&mut self, screen: &dyn Screen, play: Option<TimeUs>);
}

/// No frame sink.
pub struct NoFrames;

impl FrameSink for NoFrames {
    fn frame(&mut self, _screen: &dyn Screen, _play: Option<TimeUs>) {}
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
    handle: Option<JoinHandle<()>>,
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

    /// When the current play's first frame was drawn.
    pub fn first_frame(&self) -> Option<TimeUs> {
        let t = self.shared.first_frame.load(Ordering::Acquire);
        (t != i64::MIN).then_some(TimeUs(t))
    }

    /// Ends the thread and waits for it.
    pub fn exit(mut self) {
        self.send(RenderMsg::Exit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
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
                    frames,
                    shared: thread_shared,
                };
                r.run(&rx);
            }
        })
        .map_err(|e| format!("the render thread cannot start: {e}"))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(RenderLink {
            tx,
            shared,
            handle: Some(handle),
        }),
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
                }) => {
                    self.shared.first_frame.store(i64::MIN, Ordering::Release);
                    mode = Mode::Play {
                        origin,
                        side,
                        gauge,
                        first: true,
                    };
                }
                Ok(RenderMsg::Text(lines)) => {
                    if self.screen.is_none() {
                        self.screen = build(&mut self.make, &self.chart).ok();
                    }
                    mode = Mode::Text(lines);
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
                    let f = FrameInput {
                        now,
                        display_offset: self.view.display_offset,
                        visible_time: self.view.visible_time,
                        view: side.view(),
                        effects: &effects,
                        gauge: *gauge,
                    };
                    let drawn = screen.frame(&f).map(|()| Some(now));
                    if drawn.is_ok() && *first {
                        *first = false;
                        self.shared
                            .first_frame
                            .store(self.clock.now().0, Ordering::Release);
                    }
                    drawn
                }
                Mode::Text(lines) => {
                    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
                    screen.text(&refs).map(|()| None)
                }
            };
            match drawn {
                Ok(play) => self.frames.frame(screen.as_ref(), play),
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
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use bms_model::{Chart, ChartBuilder, TimeUs};
    use input::QpcClock;
    use render::FrameInput;

    use super::{NoFrames, RenderMsg, Screen, View, spawn};

    /// A screen whose third text frame fails.
    struct Flaky {
        frames: u32,
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
        fn frame(&mut self, _f: &FrameInput<'_>) -> Result<(), String> {
            Ok(())
        }
        fn text(&mut self, _lines: &[&str]) -> Result<(), String> {
            self.frames += 1;
            if self.frames == 3 {
                Err("surface lost".to_owned())
            } else {
                Ok(())
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

    #[test]
    fn a_failed_frame_is_reported_and_the_screen_built_again_or_the_run_ends() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        // Builds: the first at startup, the second after the failure; the
        // third build fails.
        let builds = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&builds);
        let make = Box::new(move || {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= 2 {
                Ok(Box::new(Flaky { frames: 0 }) as Box<dyn Screen>)
            } else {
                Err(format!("build {n} failed"))
            }
        });
        let view = View {
            display_offset: TimeUs(0),
            visible_time: TimeUs(600_000),
            effects: 16,
        };
        let link = spawn(make, chart, QpcClock::start(), view, Box::new(NoFrames)).unwrap();
        assert!(!link.failed());
        link.send(RenderMsg::Text(vec!["A".to_owned()]));
        assert!(wait_until(|| link.failed()), "the failure was not reported");
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        // The next play's preparation builds the screen again.
        assert_eq!(link.prepare(), Ok(()));
        assert!(!link.failed());
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        link.send(RenderMsg::Text(vec!["B".to_owned()]));
        assert!(wait_until(|| link.failed()));
        // Building again fails: the preparation reports it.
        assert_eq!(link.prepare(), Err("build 3 failed".to_owned()));
        link.exit();
    }
}
