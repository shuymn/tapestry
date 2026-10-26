//! The render thread (D8, D9): it owns the renderer, draws each play from
//! the logic step's effects and play view at the play's chart time, and
//! draws the end screen between plays. A window target paces frames on the
//! swap chain's waitable object; an offscreen one at about 60 Hz.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use bms_model::{Chart, TimeSource, TimeUs};
use eval::JudgeEvent;
use game::RenderSide;
use input::QpcClock;
use render::{FrameInput, Renderer};

/// What the game thread tells the render thread.
pub enum RenderMsg {
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
    fn frame(&mut self, renderer: &Renderer, play: Option<TimeUs>);
}

/// No frame sink.
pub struct NoFrames;

impl FrameSink for NoFrames {
    fn frame(&mut self, _renderer: &Renderer, _play: Option<TimeUs>) {}
}

/// The game thread's end of the render thread.
pub struct RenderLink {
    tx: Sender<RenderMsg>,
    handle: Option<JoinHandle<()>>,
}

impl RenderLink {
    /// Sends a message; a thread that already ended ignores it.
    pub fn send(&self, msg: RenderMsg) {
        let _ = self.tx.send(msg);
    }

    /// Ends the thread and waits for it.
    pub fn exit(mut self) {
        self.send(RenderMsg::Exit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Starts the render thread: `make` builds the renderer on it, and the
/// chart is prepared before this returns, so every upload is done before
/// the countdown (D9).
///
/// # Errors
///
/// Why the renderer could not be built or could not draw the chart.
pub fn spawn<F>(
    make: F,
    chart: Arc<Chart>,
    clock: QpcClock,
    view: View,
    frames: Box<dyn FrameSink>,
) -> Result<RenderLink, String>
where
    F: FnOnce() -> Result<Renderer, String> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let handle = std::thread::Builder::new()
        .name("render".into())
        .spawn(move || {
            let renderer = make().and_then(|mut r| {
                r.load(&chart).map_err(|e| e.to_string())?;
                Ok(r)
            });
            let mut renderer = match renderer {
                Ok(r) => r,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            run(&mut renderer, &rx, &chart, &clock, view, frames);
        })
        .map_err(|e| format!("the render thread cannot start: {e}"))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(RenderLink {
            tx,
            handle: Some(handle),
        }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => Err("the render thread ended while starting".to_owned()),
    }
}

enum Screen {
    Idle,
    Play {
        origin: TimeUs,
        side: RenderSide,
        gauge: usize,
    },
    Text(Vec<String>),
}

fn run(
    renderer: &mut Renderer,
    rx: &Receiver<RenderMsg>,
    chart: &Chart,
    clock: &QpcClock,
    view: View,
    mut frames: Box<dyn FrameSink>,
) {
    let mut screen = Screen::Idle;
    let mut effects: Vec<JudgeEvent> = Vec::with_capacity(view.effects);
    let mut first_play = true;
    loop {
        let msg = match screen {
            Screen::Idle => rx.recv().map_err(|_| TryRecvError::Disconnected),
            _ => rx.try_recv(),
        };
        match msg {
            Ok(RenderMsg::Play {
                origin,
                side,
                gauge,
            }) => {
                // The skin's play state starts over; the chart was
                // prepared at startup.
                if !first_play && let Err(e) = renderer.load(chart) {
                    eprintln!("render: {e}");
                    return;
                }
                first_play = false;
                screen = Screen::Play {
                    origin,
                    side,
                    gauge,
                };
            }
            Ok(RenderMsg::Text(lines)) => screen = Screen::Text(lines),
            Ok(RenderMsg::Exit) | Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {}
        }
        if renderer.has_frame_waitable() {
            if !renderer.wait_for_frame(100) {
                continue;
            }
        } else {
            std::thread::sleep(Duration::from_millis(16));
        }
        let drawn = match &mut screen {
            Screen::Idle => continue,
            Screen::Play {
                origin,
                side,
                gauge,
            } => {
                effects.clear();
                side.drain_effects(|e| effects.push(*e));
                let now = TimeUs(clock.now().0 - origin.0);
                let f = FrameInput {
                    now,
                    display_offset: view.display_offset,
                    visible_time: view.visible_time,
                    view: side.view(),
                    effects: &effects,
                    gauge: *gauge,
                };
                renderer.frame(&f).map(|_| Some(now))
            }
            Screen::Text(lines) => {
                let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
                renderer.text_frame(&refs).map(|_| None)
            }
        };
        match drawn {
            Ok(play) => frames.frame(renderer, play),
            Err(e) => {
                eprintln!("render: {e}");
                return;
            }
        }
    }
}
