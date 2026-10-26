//! The audio thread's loop and its control (D8): the engine steps once
//! per buffer event of its output for the whole run, and the game thread
//! starts, cancels and seals each play through a preallocated SPSC queue.
//! Nothing on the audio thread allocates or frees after construction: a
//! play's command queue, retired at the next play, goes back to the game
//! thread through a second preallocated queue and is dropped there.
//!
//! Before the first play the engine is stopped, so it writes silence and no
//! BGM sounds during setup. Between plays the game thread may change the
//! periods the output keeps queued; the audio thread applies the change at
//! its next step and answers with what the output keeps. It also counts each
//! play's underruns from its chart time 0 on, the steady state after the
//! countdown. A sealed play (evaluation complete, no more key
//! sounds) is reported done only once the audio thread has taken every
//! command sent before the seal and every voice and BGM event has ended:
//! the end of the audio tail (D13).

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ::audio::{AudioEngine, AudioOutput, InvalidVolume, MAX_VOLUME, OutputError, ScheduledKind};
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

/// Why the audio thread did not take a play switch or a queue change.
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

/// What the audio thread measured for a play (D11), sent to whoever
/// shows the figures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioProbe {
    /// The play it belongs to ([`AudioControl::generation`]).
    pub generation: u64,
    /// What was measured.
    pub kind: Probed,
}

/// One audio measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probed {
    /// A key sound was scheduled: the mix clock time of its first sample
    /// minus the receive time of the press behind it (press → sound
    /// scheduled; the output after the engine is not included).
    KeyScheduled {
        /// The latency.
        latency: TimeUs,
    },
}

/// The audio thread's counters, readable from any thread.
#[derive(Clone, Debug)]
pub struct AudioCounters(Arc<Shared>);

impl AudioCounters {
    /// Underruns since the engine started: wakes that found the buffer
    /// empty after audio had been written.
    #[must_use]
    pub fn underruns(&self) -> u64 {
        self.0.underruns.load(Ordering::Acquire)
    }

    /// Underruns of play `generation` from its chart time 0 until it was
    /// cancelled or its sealed audio ended; 0 for any other play.
    #[must_use]
    pub fn steady_underruns(&self, generation: u64) -> u64 {
        let packed = self.0.steady.load(Ordering::Acquire);
        if packed >> 32 == generation & u64::from(u32::MAX) {
            packed & u64::from(u32::MAX)
        } else {
            0
        }
    }

    /// Probe samples the full probe queue refused since the engine
    /// started.
    #[must_use]
    pub fn probes_dropped(&self) -> u64 {
        self.0.probes_dropped.load(Ordering::Acquire)
    }
}

#[derive(Debug, Default)]
struct Shared {
    /// A volume to apply: its `f32` bits with bit 32 set; 0 when none is
    /// waiting. Only the newest counts, so volume keys never fill the
    /// control queue.
    volume: AtomicU64,
    /// The generation whose sealed audio has ended.
    done: AtomicU64,
    /// The engine's underruns so far.
    underruns: AtomicU64,
    /// The current play's underruns from its chart time 0: the play's
    /// generation in the high 32 bits, the count in the low 32.
    steady: AtomicU64,
    /// The latest queue change asked for: its number in the high 32 bits,
    /// the periods in the low 32.
    queue_request: AtomicU64,
    /// The answer to a queue change: its number in the high 32 bits, the
    /// periods the output now keeps in the low 32 (0: it cannot say).
    queue_answer: AtomicU64,
    /// Probe samples dropped on a full probe queue.
    probes_dropped: AtomicU64,
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
    /// The number of the latest queue change asked for.
    queue_change: u32,
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
    /// Where key sound latencies go, if anywhere; a full queue drops them.
    probe: Option<rtrb::Producer<AudioProbe>>,
    /// The number of the latest queue change applied.
    queue_change: u32,
    /// While the play's underruns are counted: its chart time 0.
    steady_from: Option<TimeUs>,
    /// The play's underruns counted so far.
    steady: u32,
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
            queue_change: 0,
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
            probe: None,
            queue_change: 0,
            steady_from: None,
            steady: 0,
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
        let deadline = Instant::now() + SWITCH_TIMEOUT;
        loop {
            if self.in_flight
                && let Ok(retired) = self.retired.pop()
            {
                drop(retired);
                self.in_flight = false;
            }
            // A worker that is gone is never ready, whether or not it took
            // the last switch.
            if self.retired.is_abandoned() {
                return Err(AudioWaitError::Gone);
            }
            if !self.in_flight {
                return Ok(());
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

    /// Keeps `periods` periods queued on the output from the audio thread's
    /// next step on, and waits for its answer: the periods the output now
    /// keeps, `None` when it cannot say. Call it between plays, before
    /// recording opens, so a change never lands inside a play. An output
    /// that already failed is not asked: `Ok(None)`, and the next play ends
    /// on the failure as any would.
    ///
    /// # Errors
    ///
    /// [`AudioWaitError::Gone`] once the worker is gone, and
    /// [`AudioWaitError::Timeout`] after [`SWITCH_TIMEOUT`].
    pub fn set_queued_periods(&mut self, periods: u32) -> Result<Option<u32>, AudioWaitError> {
        self.request_queued_periods(periods);
        let deadline = Instant::now() + SWITCH_TIMEOUT;
        loop {
            if let Some(answer) = self.queued_periods() {
                return Ok(answer);
            }
            if self.ctl.is_abandoned() {
                return Err(AudioWaitError::Gone);
            }
            if self.shared.failure.get().is_some() {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                return Err(AudioWaitError::Timeout);
            }
            std::thread::sleep(Duration::from_micros(500));
        }
    }

    /// Asks for `periods` periods queued from the audio thread's next step
    /// on, without waiting ([`set_queued_periods`](Self::set_queued_periods)
    /// waits).
    pub fn request_queued_periods(&mut self, periods: u32) {
        self.queue_change = self.queue_change.wrapping_add(1);
        let request = u64::from(self.queue_change) << 32 | u64::from(periods);
        self.shared.queue_request.store(request, Ordering::Release);
    }

    /// The answer to the latest queue change, once the audio thread gave
    /// it: `Some(None)` when the output cannot say what it keeps.
    #[must_use]
    pub fn queued_periods(&self) -> Option<Option<u32>> {
        let answer = self.shared.queue_answer.load(Ordering::Acquire);
        ((answer >> 32) as u32 == self.queue_change)
            .then(|| Some(answer as u32).filter(|&p| p != 0))
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

    /// Sets the master volume from the next engine step on, whatever the
    /// play's state; a newer volume replaces one not yet taken.
    ///
    /// # Errors
    ///
    /// [`InvalidVolume`] for a volume outside `0.0..=MAX_VOLUME`, which is
    /// not sent.
    pub fn set_volume(&self, volume: f32) -> Result<(), InvalidVolume> {
        if !(0.0..=MAX_VOLUME).contains(&volume) {
            return Err(InvalidVolume(volume));
        }
        let pending = u64::from(volume.to_bits()) | 1 << 32;
        self.shared.volume.store(pending, Ordering::Release);
        Ok(())
    }

    /// The current play's number: 0 before the first play, then one more
    /// for each [`play`](Self::play).
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The audio thread's counters.
    #[must_use]
    pub fn counters(&self) -> AudioCounters {
        AudioCounters(Arc::clone(&self.shared))
    }

    /// Ends the worker's loop.
    pub fn exit(&mut self) {
        self.send(AudioCtl::Exit);
    }
}

impl AudioWorker {
    /// Takes every control message; returns `false` once told to exit.
    fn control(&mut self) -> bool {
        let volume = self.shared.volume.swap(0, Ordering::Acquire);
        if volume != 0 {
            // Checked by `set_volume`.
            let _ = self.engine.set_volume(f32::from_bits(volume as u32));
        }
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
                    self.steady_from = Some(origin);
                    self.steady = 0;
                    self.publish_steady();
                }
                AudioCtl::Cancel => {
                    self.steady_from = None;
                    self.engine.stop();
                    while self.queue.commands().pop().is_ok() {}
                }
                AudioCtl::Seal => self.sealed = true,
                AudioCtl::Exit => self.exited = true,
            }
        }
        !self.exited
    }

    fn publish_steady(&self) {
        let packed = (self.generation & u64::from(u32::MAX)) << 32 | u64::from(self.steady);
        self.shared.steady.store(packed, Ordering::Release);
    }

    /// Applies a queue change asked for since the last step, and answers.
    fn change_queue(&mut self, out: &mut impl AudioOutput) {
        let request = self.shared.queue_request.load(Ordering::Acquire);
        let change = (request >> 32) as u32;
        if change == self.queue_change {
            return;
        }
        self.queue_change = change;
        let kept = out.set_queue_periods(request as u32).unwrap_or(0);
        let answer = u64::from(change) << 32 | u64::from(kept);
        self.shared.queue_answer.store(answer, Ordering::Release);
    }

    /// One wake: control messages and a queue change, then one engine step
    /// into `out`. Returns `Ok(false)` once told to exit.
    ///
    /// # Errors
    ///
    /// The output's failure, which is also published to the control side.
    pub fn step(&mut self, out: &mut impl AudioOutput) -> Result<bool, OutputError> {
        if !self.control() {
            return Ok(false);
        }
        self.change_queue(out);
        let report = match self.engine.step(out, self.queue.commands()) {
            Ok(report) => report,
            Err(e) => {
                let _ = self.shared.failure.set(e);
                return Err(e);
            }
        };
        self.shared
            .underruns
            .store(self.engine.underruns(), Ordering::Release);
        if report.underrun
            && let (Some(from), Some(at)) = (self.steady_from, report.at)
            && at >= from
        {
            self.steady = self.steady.saturating_add(1);
            self.publish_steady();
        }
        if let Some(p) = self.probe.as_mut() {
            let generation = self.generation;
            let mut dropped = 0;
            self.engine.drain_scheduled(|s| {
                if let (ScheduledKind::Key { origin }, Some(at)) = (s.kind, s.at) {
                    let latency = TimeUs(at.0.saturating_sub(origin.0));
                    let kind = Probed::KeyScheduled { latency };
                    if p.push(AudioProbe { generation, kind }).is_err() {
                        dropped += 1;
                    }
                }
            });
            if dropped > 0 {
                self.shared
                    .probes_dropped
                    .fetch_add(dropped, Ordering::AcqRel);
            }
        }
        let mixer = self.engine.mixer();
        if self.sealed
            && self.queue.is_empty()
            && mixer.pending() == 0
            && mixer.sounding() == 0
            && self.engine.bgm_left() == 0
        {
            self.steady_from = None;
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

    /// Sends each key sound's press → sound scheduled latency to `probe`.
    /// The engine must keep scheduled starts (its `schedule_log`).
    pub fn set_probe(&mut self, probe: rtrb::Producer<AudioProbe>) {
        self.probe = Some(probe);
    }

    /// The engine.
    #[must_use]
    pub const fn engine(&self) -> &AudioEngine {
        &self.engine
    }
}
