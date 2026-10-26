//! The audio thread (D8): the engine steps once per buffer event of its
//! output for the whole run, playing each play's sounds on the output's
//! timeline. The game thread starts and stops plays through a channel the
//! thread polls between steps (an empty poll allocates nothing), and reads
//! the thread's state from atomics: whether the current play's audio is
//! silent (the end of its tail, D13) and whether the output failed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use audio::{AudioEngine, AudioOutput, OutputError};
use bms_model::TimeUs;
use game::{AudioQueue, audio_link};

/// What the game thread tells the audio thread.
pub enum AudioMsg {
    /// The engine to run, once, after the bank is loaded at the output's
    /// rate.
    Engine(Box<AudioEngine>),
    /// A play starts: chart time 0 at `origin` on the clock, key sounds
    /// from `queue`.
    Play {
        /// Where chart time 0 lies.
        origin: TimeUs,
        /// The play's key sounds.
        queue: AudioQueue,
        /// The play's number, echoed in the state.
        generation: u64,
    },
    /// The play was aborted: every sound stops.
    Cancel,
    /// The run ends.
    Exit,
}

/// The output's sizes, sent once it is open.
#[derive(Clone, Copy, Debug)]
pub struct Opened {
    /// Frames per second.
    pub rate: u32,
    /// Frames per engine pass.
    pub period: u32,
}

#[derive(Debug, Default)]
struct AudioState {
    /// `generation << 1 | silent` after the latest step.
    play: AtomicU64,
    failure: OnceLock<OutputError>,
}

/// The game thread's end of the audio thread.
pub struct AudioLink {
    tx: Sender<AudioMsg>,
    state: Arc<AudioState>,
    handle: Option<JoinHandle<()>>,
}

impl AudioLink {
    /// Sends a message; a thread that already ended ignores it.
    pub fn send(&self, msg: AudioMsg) {
        let _ = self.tx.send(msg);
    }

    /// Whether play `generation`'s audio has stopped: no key sound queued
    /// or pending, no voice sounding, no BGM event left.
    pub fn silent(&self, generation: u64) -> bool {
        self.state.play.load(Ordering::Acquire) == (generation << 1 | 1)
    }

    /// The output's failure, once it failed.
    pub fn failure(&self) -> Option<OutputError> {
        self.state.failure.get().copied()
    }

    /// Ends the thread and waits for it.
    pub fn exit(mut self) {
        self.send(AudioMsg::Exit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Starts the audio thread: `open` makes the output on it (WASAPI objects
/// stay on the thread that made them), then it waits for the engine and
/// runs. `done` sees the output when the thread ends.
///
/// # Errors
///
/// What `open` returned, or a message if the thread could not start.
pub fn spawn<O, F, D>(open: F, done: D) -> Result<(AudioLink, Opened), String>
where
    O: AudioOutput,
    F: FnOnce() -> Result<O, String> + Send + 'static,
    D: FnOnce(&O) + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let state = Arc::new(AudioState::default());
    let thread_state = Arc::clone(&state);
    let handle = std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let mut out = match open() {
                Ok(out) => out,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(Opened {
                rate: out.rate(),
                period: out.period(),
            }));
            run(&mut out, &rx, &thread_state);
            done(&out);
        })
        .map_err(|e| format!("the audio thread cannot start: {e}"))?;
    match ready_rx.recv() {
        Ok(Ok(opened)) => Ok((
            AudioLink {
                tx,
                state,
                handle: Some(handle),
            },
            opened,
        )),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => Err("the audio thread ended while opening the output".to_owned()),
    }
}

fn run(out: &mut impl AudioOutput, rx: &Receiver<AudioMsg>, state: &AudioState) {
    let mut engine = loop {
        match rx.recv() {
            Ok(AudioMsg::Engine(e)) => break e,
            Ok(AudioMsg::Exit) | Err(_) => return,
            Ok(_) => {}
        }
    };
    // Before the first play: a queue nothing sends to.
    let mut queue = audio_link(1).1;
    let mut generation = 0;
    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                AudioMsg::Play {
                    origin,
                    queue: q,
                    generation: g,
                } => {
                    engine.restart(origin);
                    queue = q;
                    generation = g;
                }
                AudioMsg::Cancel => {
                    engine.stop();
                    while queue.commands().pop().is_ok() {}
                }
                AudioMsg::Exit => return,
                AudioMsg::Engine(_) => {}
            }
        }
        if let Err(e) = engine.step(out, queue.commands()) {
            let _ = state.failure.set(e);
            // The output is gone: wait for the end of the run.
            while !matches!(rx.recv(), Ok(AudioMsg::Exit) | Err(_)) {}
            return;
        }
        let silent = queue.is_empty()
            && engine.mixer().pending() == 0
            && engine.mixer().sounding() == 0
            && engine.bgm_left() == 0;
        state
            .play
            .store(generation << 1 | u64::from(silent), Ordering::Release);
    }
}
