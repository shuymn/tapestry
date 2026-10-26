//! The audio thread's loop and its control (D8): the engine steps once
//! per buffer event of its output for the whole run, and the game thread
//! starts, cancels and seals each play through a preallocated SPSC queue.
//! Nothing on the audio thread allocates or frees after construction: a
//! play's command queue, retired at the next play, goes back to the game
//! thread through a second preallocated queue and is dropped there.
//!
//! Before the first play the engine is stopped, so it writes silence and no
//! BGM sounds during setup. A sealed play (evaluation complete, no more key
//! sounds) is reported done only once the audio thread has taken every
//! command sent before the seal and every voice and BGM event has ended:
//! the end of the audio tail (D13).

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use ::audio::{AudioEngine, AudioOutput, OutputError};
use bms_model::TimeUs;

use crate::sound::{AudioQueue, audio_link};

/// Control messages a play sends at most (start, cancel or seal, and one
/// to spare), times the plays that may queue before the audio thread wakes.
const CONTROL_CAPACITY: usize = 16;
/// Retired queues waiting for the game thread.
const RETIRED_CAPACITY: usize = 8;

/// What the game thread tells the audio thread.
#[derive(Debug)]
pub enum AudioCtl {
    /// A play starts: chart time 0 at `origin` on the output's timeline,
    /// key sounds from `queue`.
    Play {
        /// Where chart time 0 lies.
        origin: TimeUs,
        /// The play's key sounds.
        queue: AudioQueue,
        /// The play's number.
        generation: u64,
    },
    /// The play was aborted: every sound stops.
    Cancel,
    /// Evaluation completed: no key sound follows.
    Seal,
    /// The run ends.
    Exit,
}

#[derive(Debug, Default)]
struct Shared {
    /// The generation whose sealed audio has ended.
    done: AtomicU64,
    failure: OnceLock<OutputError>,
}

/// The game thread's end.
#[derive(Debug)]
pub struct AudioControl {
    ctl: rtrb::Producer<AudioCtl>,
    retired: rtrb::Consumer<AudioQueue>,
    shared: Arc<Shared>,
    generation: u64,
}

/// The audio thread's end: the engine and the play it is playing.
#[derive(Debug)]
pub struct AudioWorker {
    engine: AudioEngine,
    ctl: rtrb::Consumer<AudioCtl>,
    retired: rtrb::Producer<AudioQueue>,
    shared: Arc<Shared>,
    queue: AudioQueue,
    generation: u64,
    sealed: bool,
    exited: bool,
}

/// The two ends around `engine`, which is stopped until the first play.
#[must_use]
pub fn audio_control(mut engine: AudioEngine) -> (AudioControl, AudioWorker) {
    engine.stop();
    let (ctl_tx, ctl_rx) = rtrb::RingBuffer::new(CONTROL_CAPACITY);
    let (ret_tx, ret_rx) = rtrb::RingBuffer::new(RETIRED_CAPACITY);
    let shared = Arc::new(Shared::default());
    (
        AudioControl {
            ctl: ctl_tx,
            retired: ret_rx,
            shared: Arc::clone(&shared),
            generation: 0,
        },
        AudioWorker {
            engine,
            ctl: ctl_rx,
            retired: ret_tx,
            shared,
            // Before the first play: a queue nothing sends to.
            queue: audio_link(1).1,
            generation: 0,
            sealed: false,
            exited: false,
        },
    )
}

impl AudioControl {
    fn send(&mut self, mut msg: AudioCtl) {
        // The worker drains every message each step, even after a failure,
        // so a full queue empties within a period.
        while let Err(rtrb::PushError::Full(m)) = self.ctl.push(msg) {
            msg = m;
            std::thread::yield_now();
        }
    }

    /// Starts a play; the queues retired by earlier plays are dropped here.
    pub fn play(&mut self, origin: TimeUs, queue: AudioQueue) {
        while self.retired.pop().is_ok() {}
        self.generation += 1;
        let generation = self.generation;
        self.send(AudioCtl::Play {
            origin,
            queue,
            generation,
        });
    }

    /// Stops the play's sounds.
    pub fn cancel(&mut self) {
        self.send(AudioCtl::Cancel);
    }

    /// No key sound follows: [`done`](Self::done) turns true when the play's
    /// audio has ended.
    pub fn seal(&mut self) {
        self.send(AudioCtl::Seal);
    }

    /// Whether the sealed play's audio has ended.
    #[must_use]
    pub fn done(&self) -> bool {
        self.generation != 0 && self.shared.done.load(Ordering::Acquire) == self.generation
    }

    /// The output's failure, once it failed.
    #[must_use]
    pub fn failure(&self) -> Option<OutputError> {
        self.shared.failure.get().copied()
    }

    /// Ends the worker's loop.
    pub fn exit(&mut self) {
        self.send(AudioCtl::Exit);
    }
}

impl AudioWorker {
    /// Takes every control message; returns `false` once told to exit.
    fn control(&mut self) -> bool {
        while let Ok(msg) = self.ctl.pop() {
            match msg {
                AudioCtl::Play {
                    origin,
                    queue,
                    generation,
                } => {
                    self.engine.restart(origin);
                    let old = std::mem::replace(&mut self.queue, queue);
                    // The game thread drains the return queue at every play,
                    // so it has room; if not, the queue is leaked rather
                    // than freed here.
                    if let Err(rtrb::PushError::Full(old)) = self.retired.push(old) {
                        std::mem::forget(old);
                    }
                    self.generation = generation;
                    self.sealed = false;
                }
                AudioCtl::Cancel => {
                    self.engine.stop();
                    while self.queue.commands().pop().is_ok() {}
                }
                AudioCtl::Seal => self.sealed = true,
                AudioCtl::Exit => self.exited = true,
            }
        }
        !self.exited
    }

    /// One wake: control messages, then one engine step into `out`.
    /// Returns `Ok(false)` once told to exit.
    ///
    /// # Errors
    ///
    /// The output's failure, which is also published to the control side.
    pub fn step(&mut self, out: &mut impl AudioOutput) -> Result<bool, OutputError> {
        if !self.control() {
            return Ok(false);
        }
        if let Err(e) = self.engine.step(out, self.queue.commands()) {
            let _ = self.shared.failure.set(e);
            return Err(e);
        }
        let mixer = self.engine.mixer();
        if self.sealed
            && self.queue.is_empty()
            && mixer.pending() == 0
            && mixer.sounding() == 0
            && self.engine.bgm_left() == 0
        {
            self.shared.done.store(self.generation, Ordering::Release);
        }
        Ok(true)
    }

    /// Steps until told to exit. After an output failure it only takes
    /// control messages, every `idle`, until then.
    pub fn run(&mut self, out: &mut impl AudioOutput, idle: std::time::Duration) {
        loop {
            match self.step(out) {
                Ok(true) => {}
                Ok(false) => return,
                Err(_) => {
                    while self.control() {
                        std::thread::sleep(idle);
                    }
                    return;
                }
            }
        }
    }

    /// The engine.
    #[must_use]
    pub const fn engine(&self) -> &AudioEngine {
        &self.engine
    }
}
