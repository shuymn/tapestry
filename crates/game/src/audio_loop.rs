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
use std::time::{Duration, Instant};

use ::audio::{AudioEngine, AudioOutput, OutputError};
use bms_model::TimeUs;

use crate::sound::{AudioQueue, audio_link};

/// Control messages a play sends at most (start, cancel or seal, and one
/// to spare), times the plays that may queue before the audio thread wakes.
const CONTROL_CAPACITY: usize = 16;
/// Retired queues waiting for the game thread: one, since a play is
/// started only once the queue the previous start retired has come back.
const RETIRED_CAPACITY: usize = 1;

/// How long the game thread waits for the audio thread to take a play
/// switch: far beyond a period, which is when it takes one.
pub const SWITCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Why the audio thread did not take a play switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioWaitError {
    /// The worker is gone.
    Gone,
    /// It did not answer within [`SWITCH_TIMEOUT`].
    Timeout,
}

impl std::fmt::Display for AudioWaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Gone => "the audio thread is gone",
            Self::Timeout => "the audio thread did not answer",
        })
    }
}

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
    /// Whether a start was sent whose retired queue has not come back.
    in_flight: bool,
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
    /// A retired queue the full return queue did not take.
    unreturned: Option<AudioQueue>,
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
            in_flight: false,
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
            unreturned: None,
        },
    )
}

impl AudioControl {
    fn send(&mut self, mut msg: AudioCtl) {
        // The worker drains every message each step, even after a failure,
        // so a full queue empties within a period.
        while let Err(rtrb::PushError::Full(m)) = self.ctl.push(msg) {
            // A worker that is gone takes nothing more.
            if self.ctl.is_abandoned() {
                return;
            }
            msg = m;
            std::thread::yield_now();
        }
    }

    /// Waits until the audio thread has taken the previous play's switch
    /// and returned its retired queue, which is dropped here. Call it before
    /// recording opens, so [`play`](Self::play) does not wait.
    ///
    /// # Errors
    ///
    /// [`AudioWaitError::Gone`] once the worker is gone, and
    /// [`AudioWaitError::Timeout`] after [`SWITCH_TIMEOUT`].
    pub fn ready(&mut self) -> Result<(), AudioWaitError> {
        if !self.in_flight {
            return Ok(());
        }
        let deadline = Instant::now() + SWITCH_TIMEOUT;
        loop {
            if let Ok(retired) = self.retired.pop() {
                drop(retired);
                self.in_flight = false;
                return Ok(());
            }
            if self.retired.is_abandoned() {
                return Err(AudioWaitError::Gone);
            }
            if Instant::now() >= deadline {
                return Err(AudioWaitError::Timeout);
            }
            std::thread::sleep(Duration::from_micros(500));
        }
    }

    /// Starts a play. One play switch is in flight at most: the previous
    /// start's retired queue comes back first ([`ready`](Self::ready)), so
    /// the return queue always has room and the audio thread never frees
    /// one.
    ///
    /// # Errors
    ///
    /// As [`ready`](Self::ready); nothing is sent.
    pub fn play(&mut self, origin: TimeUs, queue: AudioQueue) -> Result<(), AudioWaitError> {
        self.ready()?;
        self.in_flight = true;
        self.generation += 1;
        let generation = self.generation;
        self.send(AudioCtl::Play {
            origin,
            queue,
            generation,
        });
        Ok(())
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
        let gone = self.ctl.is_abandoned().then_some(OutputError::DeviceLost);
        self.shared.failure.get().copied().or(gone)
    }

    /// Ends the worker's loop.
    pub fn exit(&mut self) {
        self.send(AudioCtl::Exit);
    }
}

impl AudioWorker {
    /// Takes every control message; returns `false` once told to exit.
    fn control(&mut self) -> bool {
        if let Some(old) = self.unreturned.take()
            && let Err(rtrb::PushError::Full(old)) = self.retired.push(old)
        {
            self.unreturned = Some(old);
        }
        while let Ok(msg) = self.ctl.pop() {
            match msg {
                AudioCtl::Play {
                    origin,
                    queue,
                    generation,
                } => {
                    self.engine.restart(origin);
                    let old = std::mem::replace(&mut self.queue, queue);
                    // The return queue has room: the game thread took the
                    // previous start's queue before sending this one. Were
                    // it full, the queue would wait here, still owned,
                    // rather than be freed on this thread.
                    if let Err(rtrb::PushError::Full(old)) = self.retired.push(old) {
                        self.unreturned = Some(old);
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
