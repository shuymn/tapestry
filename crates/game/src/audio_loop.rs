//! The audio thread's loop and its control (design.md "Overall Architecture"):
//! the engine steps once per buffer event of its output for the whole run, and
//! the game thread starts, cancels and seals (or rings out) each play through a
//! preallocated SPSC queue. Nothing on the audio thread allocates or frees after
//! construction: a play's command queue, retired at the next play, goes back to
//! the game thread through a second preallocated queue and is dropped there.
//!
//! Before the first play the engine is stopped, so it writes silence and no BGM
//! sounds during setup. Between plays the game thread may change the periods
//! the output keeps queued; the audio thread applies the change at its next
//! step and answers with what the output keeps. It also counts each play's
//! underruns from its chart time 0 on, the steady state after READY. A
//! sealed play (evaluation complete; key sounds come only from the audio
//! tail's feedback) is reported done once its queue is empty and every voice
//! and BGM event has ended, the report naming the play and the commands the
//! audio thread had taken from its queue by then; the game thread takes it
//! as the end of the audio tail (design.md "Play Lifecycle") only when that
//! is every command its queue accepted, so a report made before a later
//! press's sound was accepted never ends the tail.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ::audio::{
    AudioEngine, AudioOutput, BgmSchedule, Channel, FrameLine, Gain, GenId, InvalidVolume,
    MAX_VOLUME, OutputError, RESERVED_KINDS, SYSTEM_INSTALLS, SYSTEM_KINDS, SYSTEM_RETURNS,
    SYSTEM_STARTS, ScheduledKind, SeekSource, SoundBank, SystemGeneration, SystemInstall,
    SystemKind, SystemSnapshot, SystemStart,
};
use bms_model::TimeUs;

use crate::restore::{Restore, Restored};
use crate::sound::{AudioQueue, audio_link};

/// A play's key sound queue and its restore, retired at the next play and
/// freed on the game thread.
type RetiredPlay = (AudioQueue, Option<Box<Restore>>);

#[cfg(debug_assertions)]
thread_local! {
    /// Whether this thread is taking the worker's control messages.
    static IN_STEP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks this thread taking the worker's control messages while it lives
/// (debug builds).
#[cfg(debug_assertions)]
struct InStep;

#[cfg(debug_assertions)]
impl InStep {
    fn enter() -> Self {
        IN_STEP.set(true);
        Self
    }
}

#[cfg(debug_assertions)]
impl Drop for InStep {
    fn drop(&mut self) {
        IN_STEP.set(false);
    }
}

/// A round's restore while the audio thread owns it: it leaves only by
/// going back to the game thread ([`give`](Self::give)), never freed while
/// the worker takes its control messages, which debug builds assert.
#[derive(Debug)]
struct Kept(Option<Box<Restore>>);

impl Kept {
    fn get(&self) -> Option<&Restore> {
        self.0.as_deref()
    }

    /// The box, to hand back.
    fn give(mut self) -> Option<Box<Restore>> {
        self.0.take()
    }
}

impl Drop for Kept {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        debug_assert!(
            self.0.is_none() || !IN_STEP.get(),
            "a restore freed on the audio thread"
        );
    }
}

/// How many times a read of the frame line is tried before the last read
/// is taken: the audio thread publishes it in three stores, so a read that
/// overlaps them is tried again.
const LINE_READS: usize = 64;

/// Control messages a play sends at most (start, seal or ring-out, cancel,
/// and one to spare), times the plays that may queue before the audio thread wakes.
const CONTROL_CAPACITY: usize = 16;
/// Retired queues waiting for the game thread: one, since a play is
/// started only once the queue the previous start retired has come back.
const RETIRED_CAPACITY: usize = 1;
/// Replaced banks waiting for the game thread: one, since a load waits for
/// its answer before another is sent.
const LOADS_CAPACITY: usize = 1;

/// How long the game thread waits for the audio thread to take a play
/// switch: far beyond a period, which is when it takes one.
pub const SWITCH_TIMEOUT: Duration = Duration::from_secs(2);

/// The system sounds' request sequence issues numbers below this: a draw
/// that would reach it is refused, and every later one is too.
pub const SEQ_LIMIT: u64 = 1 << 63;
/// The number never issued: the stop of a kind whose stop was asked for
/// once the sequence was exhausted, and the empty install's number then.
pub const TERMINAL: u64 = u64::MAX;
/// The start ring's last places, which a start of a kind without a
/// reserved voice never takes: one for each reserved kind
/// ([`RESERVED_KINDS`]). A state starts at most one reserved kind per
/// visit and the audio thread takes the ring at every step, so a burst of
/// one-shots never refuses a reserved kind's start (design.md "Audio",
/// Mixing).
pub const RESERVED_HEADROOM: usize = RESERVED_KINDS.len();

/// Why a system sound's start was not queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SystemStartError {
    /// The start ring is full; counted.
    #[error("the system sounds' start ring is full")]
    Full,
    /// The request sequence is exhausted; counted, except a decision
    /// sound's, which its caller turns into the tone.
    #[error("the system sounds' request sequence is exhausted")]
    Exhausted,
}

/// Why a system generation was not queued for install; the generation
/// comes back with it.
#[derive(Debug, thiserror::Error)]
pub enum SystemInstallError {
    /// The install ring is full.
    #[error("the system sounds' install ring is full")]
    Full(SystemGeneration),
    /// The generations handed over and not taken back fill the return
    /// ring's room ([`SYSTEM_RETURNS`]); taking one back frees a place.
    #[error("the system generations not yet taken back fill the return ring")]
    Unreturned(SystemGeneration),
    /// The request sequence is exhausted (only the empty generation is
    /// installed then, at the terminal number).
    #[error("the system sounds' request sequence is exhausted")]
    Exhausted(SystemGeneration),
    /// It was decoded at another rate than the output's.
    #[error("the system generation is at {rate} Hz but the output runs at {output} Hz")]
    RateMismatch {
        /// The generation.
        generation: SystemGeneration,
        /// Its rate.
        rate: u32,
        /// The output's rate.
        output: u32,
    },
}

/// Where a test hook runs inside the audio thread's snapshot of the system
/// sounds' requests (dev-tools).
#[cfg(feature = "dev-tools")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotPoint {
    /// After the cutoff is read, before either ring.
    AfterCutoff,
    /// Between the start ring's read and the install ring's.
    BetweenRings,
    /// After the whole snapshot, before the requests are taken.
    AfterSnapshot,
}

/// A test hook run at each [`SnapshotPoint`] (dev-tools).
#[cfg(feature = "dev-tools")]
struct SnapshotHook(Box<dyn FnMut(SnapshotPoint) + Send>);

#[cfg(feature = "dev-tools")]
impl std::fmt::Debug for SnapshotHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SnapshotHook")
    }
}

/// Why the audio thread did not take a play switch or a queue change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AudioWaitError {
    /// The worker is gone.
    #[error("the audio thread is gone")]
    Gone,
    /// It did not answer within [`SWITCH_TIMEOUT`].
    #[error("the audio thread did not answer")]
    Timeout,
}

/// Why the audio thread did not take another chart's sounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AudioLoadError {
    /// The audio thread did not take them.
    #[error(transparent)]
    Wait(#[from] AudioWaitError),
    /// An earlier load, which timed out, is still not answered: no other
    /// is sent until it is.
    #[error("the audio thread has not answered the previous load")]
    Busy,
    /// The bank was decoded at another rate than the output's.
    #[error("the sound bank is at {bank} Hz but the output runs at {output} Hz")]
    RateMismatch {
        /// The bank's rate.
        bank: u32,
        /// The output's rate.
        output: u32,
    },
}

/// A bank and schedule the audio thread hands back to be freed on the game
/// thread: the ones a load replaced, or the load's own when the engine
/// refused them.
#[derive(Debug)]
struct Retired {
    // Held only to be dropped where the game thread takes them.
    _bank: Arc<SoundBank>,
    _schedule: BgmSchedule,
    /// The load it answers.
    id: u64,
    /// Whether the engine took the load.
    taken: bool,
    /// What the engine plays after the load, for planning its seeks: one
    /// `Arc` clone, freed with the answer on the game thread.
    source: SeekSource,
}

/// What the game thread tells the audio thread.
#[derive(Debug)]
pub enum AudioCtl {
    /// Another chart: its bank and audio schedule replace the engine's,
    /// and nothing sounds until the next play. Between plays only.
    Load {
        /// The chart's sounds, decoded at the output's rate.
        bank: Arc<SoundBank>,
        /// Its BGM, or the schedule that stands for it (autoplay).
        schedule: BgmSchedule,
        /// The load's number, which its answer carries.
        id: u64,
    },
    /// A play starts: chart time 0 at `origin` on the output's timeline,
    /// key sounds from `queue`, the sum at `gain` for the whole play, and a
    /// practice round's audio restored by `restore` (design.md "Mid-Chart
    /// Start and Section Repeat"): one message, so no pass runs between the
    /// start and the seek.
    Play {
        /// Where chart time 0 lies.
        origin: TimeUs,
        /// The play's key sounds.
        queue: AudioQueue,
        /// The play's number.
        generation: u64,
        /// The play's fixed gain on the sum: [`Gain::HEADROOM`], or the
        /// chart's normalization gain (design.md "Algorithms").
        gain: Gain,
        /// A round's restore, planned on `origin`'s frame line; it goes
        /// back to the game thread with `queue` at the next play.
        restore: Option<Box<Restore>>,
        /// The READY candidate whose transport this play's start waits
        /// for ([`TransportGate`]); `None` starts it unconditionally.
        gate: Option<TransportGate>,
    },
    /// A READY candidate of the current play was cancelled and another
    /// opened: chart time 0 now at `origin`, its transport waiting for
    /// `gate` as the first's did, the play's queue and gain kept. A
    /// round's `restore`, planned again in a box the game thread owns,
    /// takes the place of the play's: the one it replaces goes back for
    /// the next rearm ([`AudioControl::take_restore`]). Nothing is
    /// allocated or freed.
    Rearm {
        /// Where the new candidate's chart time 0 lies.
        origin: TimeUs,
        /// Its gate.
        gate: TransportGate,
        /// A round's restore for it.
        restore: Option<Box<Restore>>,
    },
    /// The play was aborted: every sound stops.
    Cancel,
    /// Evaluation completed: the play's audio is reported done whenever it
    /// has ended, and key sounds come only from the audio tail's feedback.
    Seal,
    /// A practice round's evaluation completed: sealed, and the play rings
    /// out (design.md "Mid-Chart Start and Section Repeat"):
    /// the BGM plays on until `fade_at`, the output fades to silence by
    /// `end_at`, both in the play's chart time, and every sound stops
    /// there. The play is then done as a sealed one is.
    RingOut {
        /// Where the fade starts.
        fade_at: TimeUs,
        /// Where the output is silent and every sound stops.
        end_at: TimeUs,
    },
    /// The run ends.
    Exit,
}

/// A READY candidate's transport authorization (design.md "Play
/// Lifecycle", the start gate): the engine holds the play's transport,
/// and the pass whose block reaches chart time `commit_at` (0, or a
/// round's lead-in start), by that pass's own wake and its actual block,
/// commits `generation` on `gate` before anything of the schedule starts;
/// when the commit finds it cancelled or stale, every sound of the play
/// stops there, so its scheduled chart start and restore never run.
#[derive(Clone, Debug)]
pub struct TransportGate {
    /// The start gate the input thread cancels.
    pub gate: input::StartGate,
    /// The candidate's generation.
    pub generation: u32,
    /// Where its transport commits, in chart time.
    pub commit_at: TimeUs,
}

/// What the audio thread measured for a play (design.md "Latency Figures"),
/// sent to whoever shows the figures.
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

    /// Wakes since the engine started that left a due start for a later
    /// wake, the mixer's pending starts full.
    #[must_use]
    pub fn deferred_wakes(&self) -> u64 {
        self.0.deferred_wakes.load(Ordering::Acquire)
    }

    /// Voices the mixer stole at its voice limit since the engine started.
    #[must_use]
    pub fn steals(&self) -> u64 {
        self.0.steals.load(Ordering::Acquire)
    }

    /// System sound starts refused since the engine started: a full start
    /// ring or an exhausted sequence on the requesting side, a full
    /// one-shot part or a sounding reserved voice, and every start or tone
    /// request after the output failed on the audio thread.
    #[must_use]
    pub fn system_refused(&self) -> u64 {
        self.0.system_refused.load(Ordering::Acquire)
            + self.0.system_refused_requests.load(Ordering::Acquire)
    }

    /// Audio steps whose system requests waited behind an install that
    /// found the retired slot taken.
    #[must_use]
    pub fn system_install_waits(&self) -> u64 {
        self.0.system_install_waits.load(Ordering::Acquire)
    }

    /// Whether the audio thread dropped READY candidate `generation`'s
    /// transport, by a cancel or a commit that found it cancelled: its
    /// cancellation is acknowledged, and no authorization of it is left.
    #[must_use]
    pub fn dropped(&self, generation: u32) -> bool {
        self.0.dropped.load(Ordering::Acquire) == u64::from(generation) + 1
    }

    /// How play `generation`'s audio was restored, once the audio thread
    /// took its start; `None` for a play without a restore, or another play.
    #[must_use]
    pub fn restored(&self, generation: u64) -> Option<Restored> {
        let packed = self.0.restored.load(Ordering::Acquire);
        if packed >> 8 == generation {
            Restored::from_code((packed & 0xff) as u8)
        } else {
            None
        }
    }

    /// Where the chart times of a play whose chart time 0 lies at `origin`
    /// fall on the output, by the audio thread's last step: its mix clock,
    /// output offset and written frames, read together. For planning a
    /// round's restore ([`crate::Restorer::plan`]) once the play's origin is
    /// fixed.
    #[must_use]
    pub fn frame_line(&self, origin: TimeUs) -> FrameLine {
        let s = &*self.0;
        let mut read = (0, 0);
        for _ in 0..LINE_READS {
            let before = s.line_seq.load(Ordering::Acquire);
            read = (
                s.line_shift.load(Ordering::Acquire),
                s.line_written.load(Ordering::Acquire),
            );
            if before.is_multiple_of(2) && s.line_seq.load(Ordering::Acquire) == before {
                break;
            }
            std::thread::yield_now();
        }
        // A read the audio thread kept overlapping is at worst a clock
        // moved since: the seek is then degraded but bounded (design.md
        // "Audio").
        FrameLine {
            rate: s.rate,
            period: s.period,
            block_start: read.1,
            base_us: origin.0.saturating_sub(read.0),
        }
    }
}

#[derive(Debug, Default)]
struct Shared {
    /// A volume to apply per channel, in [`Channel::ALL`] order: its `f32`
    /// bits with bit 32 set; 0 when none is waiting. Only the newest
    /// counts, so volume keys never fill the control queue.
    volumes: [AtomicU64; 3],
    /// The decision tone asked to start: not a control message, so it
    /// never waits behind one.
    tone: AtomicBool,
    /// The decision tones asked for so far (dev-tools).
    #[cfg(feature = "dev-tools")]
    tones: AtomicU64,
    /// The system sounds' request sequence: the next number to issue.
    /// Starts at 1; never passes [`SEQ_LIMIT`].
    system_seq: AtomicU64,
    /// The highest number the game thread has pushed a start or install
    /// under (0: none): the audio thread takes nothing above it.
    system_published: AtomicU64,
    /// Per [`SystemKind`], its stop: raised, never lowered.
    system_stops: [AtomicU64; SYSTEM_KINDS],
    /// The current system generation's id (0: none installed).
    system_installed: AtomicU64,
    /// System starts the audio thread refused so far.
    system_refused: AtomicU64,
    /// System starts the requesting side refused so far.
    system_refused_requests: AtomicU64,
    /// Per [`SystemKind`], the starts queued so far (dev-tools).
    #[cfg(feature = "dev-tools")]
    starts_queued: [AtomicU64; SYSTEM_KINDS],
    /// Per [`SystemKind`], the stops drawn so far, each stop of every
    /// kind counted for each (dev-tools).
    #[cfg(feature = "dev-tools")]
    stops_drawn: [AtomicU64; SYSTEM_KINDS],
    /// The tones the audio thread started so far, as of its latest
    /// control step (dev-tools).
    #[cfg(feature = "dev-tools")]
    tones_started: AtomicU64,
    /// Per kind of [`RESERVED_KINDS`], its reserved voice as of the audio
    /// thread's latest control step: the generation's id shifted left one
    /// bit, with bit 0 set when it loops; 0 when silent (dev-tools).
    #[cfg(feature = "dev-tools")]
    reserved: [AtomicU64; RESERVED_KINDS.len()],
    /// Steps so far whose system requests waited behind a blocked install.
    system_install_waits: AtomicU64,
    /// The latest report that the sealed play's audio has ended: its
    /// generation in the high 32 bits, the key-sound commands the audio
    /// thread had taken from its queue in the low 32.
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
    /// The engine's wakes that deferred a start so far.
    deferred_wakes: AtomicU64,
    /// The mixer's stolen voices so far.
    steals: AtomicU64,
    /// How the latest play's restore went: its generation shifted left 8
    /// bits, with [`Restored`]'s code (0: no restore).
    restored: AtomicU64,
    /// The latest READY candidate whose transport the audio thread
    /// dropped, plus one: its cancellation acknowledged (0: none).
    dropped: AtomicU64,
    /// The frame line's publication count: odd while the audio thread
    /// writes it.
    line_seq: AtomicU64,
    /// A play's origin minus its frame line's base: the output offset plus
    /// the mix clock's anchor.
    line_shift: AtomicI64,
    /// The first frame not yet written, a pass's first.
    line_written: AtomicU64,
    /// The output's rate and pass size, fixed for the engine.
    rate: u32,
    period: u32,
    failure: OnceLock<OutputError>,
    /// The audio thread's end was dropped: the thread ended or is
    /// unwinding.
    gone: AtomicBool,
}

impl Shared {
    /// The output's failure: one the audio thread latched, else its end
    /// gone. The one reading of it, from the game thread's control and
    /// from any thread through an [`OutputProbe`].
    fn failure(&self) -> Option<OutputError> {
        self.failure.get().copied().or_else(|| {
            self.gone
                .load(Ordering::Acquire)
                .then_some(OutputError::DeviceLost)
        })
    }

    /// `kind`'s stop word (dev-tools): the one reading of it, from the
    /// game thread's control and through an [`OutputProbe`].
    #[cfg(feature = "dev-tools")]
    fn system_stop_word(&self, kind: SystemKind) -> u64 {
        self.system_stops
            .get(kind.index())
            .map_or(0, |word| word.load(Ordering::Acquire))
    }
}

/// The output's latched failure, from any thread
/// ([`AudioControl::failure_probe`]).
#[derive(Clone)]
pub struct OutputProbe(Arc<Shared>);

impl OutputProbe {
    /// The output's failure, as [`AudioControl::failure`] reads it.
    #[must_use]
    pub fn failure(&self) -> Option<OutputError> {
        self.0.failure()
    }

    /// How many decision tones were asked for
    /// ([`AudioControl::play_tone`]; dev-tools).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn tone_requests(&self) -> u64 {
        self.0.tones.load(Ordering::Acquire)
    }

    /// How many starts of `kind` were queued
    /// ([`AudioControl::system_start`] answered `Ok`; dev-tools).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn system_starts(&self, kind: SystemKind) -> u64 {
        self.0
            .starts_queued
            .get(kind.index())
            .map_or(0, |n| n.load(Ordering::Acquire))
    }

    /// How many stops reached `kind` ([`AudioControl::system_stop`] of
    /// it, and every [`AudioControl::system_stop_all`]; dev-tools).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn system_stops(&self, kind: SystemKind) -> u64 {
        self.0
            .stops_drawn
            .get(kind.index())
            .map_or(0, |n| n.load(Ordering::Acquire))
    }

    /// `kind`'s stop word ([`AudioControl::system_stop_word`]), readable
    /// after the control is gone (dev-tools).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn system_stop_word(&self, kind: SystemKind) -> u64 {
        self.0.system_stop_word(kind)
    }

    /// How many tones the audio thread started, by a tone request or by a
    /// decision sound its generation has none for, as of its latest control
    /// step (dev-tools).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn tones_started(&self) -> u64 {
        self.0.tones_started.load(Ordering::Acquire)
    }

    /// The sounding voice reserved for `kind` as of the audio thread's
    /// latest control step: its generation and whether it loops; `None`
    /// when silent or when `kind` has no reserved voice (dev-tools).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn reserved(&self, kind: SystemKind) -> Option<(GenId, bool)> {
        let at = RESERVED_KINDS.iter().position(|&k| k == kind)?;
        let word = self.0.reserved.get(at)?.load(Ordering::Acquire);
        (word != 0).then_some((GenId(word >> 1), word & 1 == 1))
    }
}

impl std::fmt::Debug for OutputProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OutputProbe").field(&self.failure()).finish()
    }
}

/// A switch of the engine's sounds ([`AudioControl::switch`]): the bank
/// and schedule it owns until they are sent, the number they were sent
/// under, and when it was made.
#[derive(Debug)]
pub struct Switch {
    unsent: Option<(Arc<SoundBank>, BgmSchedule)>,
    id: Option<u64>,
    since: Instant,
}

impl Switch {
    /// Whether its load is in the control queue (or answered).
    #[must_use]
    pub const fn sent(&self) -> bool {
        self.id.is_some()
    }
}

/// A look at a [`Switch`] ([`AudioControl::poll_switch`]).
#[derive(Debug, PartialEq, Eq)]
pub enum SwitchPoll {
    /// Not answered yet: unsent, or sent and waiting for the audio thread.
    Pending,
    /// Answered, or the audio thread is gone.
    Done(Result<(), AudioLoadError>),
}

/// The game thread's end.
#[derive(Debug)]
pub struct AudioControl {
    ctl: rtrb::Producer<AudioCtl>,
    retired: rtrb::Consumer<RetiredPlay>,
    /// The restores rearms replaced, for planning the next rearm in.
    rearmed: rtrb::Consumer<Box<Restore>>,
    /// What the engine plays, as of its latest load: for planning a round's
    /// restore.
    source: SeekSource,
    /// Whether a start was sent whose retired queue has not come back.
    in_flight: bool,
    /// Banks and schedules a load replaced, freed here.
    loads: rtrb::Consumer<Retired>,
    /// The number of the last load sent.
    load_id: u64,
    /// A load sent and not yet answered, by number: no other is sent
    /// while it is, so at most one answer is ever on its way.
    outstanding: Option<u64>,
    /// The engine's bank rate: a load at another rate is refused here.
    rate: u32,
    shared: Arc<Shared>,
    generation: u64,
    /// The commands the current play's queue accepted
    /// ([`AudioQueue::accepted_count`]); `None` before the first play.
    accepted: Option<Arc<AtomicU64>>,
    /// The number of the latest queue change asked for.
    queue_change: u32,
    /// Pushes of a switch taken as a full queue still to come (dev-tools).
    #[cfg(feature = "dev-tools")]
    full_for: Arc<std::sync::atomic::AtomicU32>,
    /// The game thread's system sound starts, in number order.
    system_starts: rtrb::Producer<SystemStart>,
    /// System generations to install, in number order.
    system_installs: rtrb::Producer<SystemInstall>,
    /// System generations the audio thread is done with, freed here.
    system_returns: rtrb::Consumer<SystemGeneration>,
    /// System generations handed to the audio thread and not taken back:
    /// queued, current, retired or in the return ring.
    system_held: usize,
}

/// The audio thread's end: the engine and the play it is playing.
#[derive(Debug)]
pub struct AudioWorker {
    engine: AudioEngine,
    ctl: rtrb::Consumer<AudioCtl>,
    retired: rtrb::Producer<RetiredPlay>,
    loads: rtrb::Producer<Retired>,
    /// A replaced bank the full return queue did not take.
    unreturned_load: Option<Retired>,
    shared: Arc<Shared>,
    queue: AudioQueue,
    /// The current play's restore, kept until the next play retires it.
    restore: Option<Kept>,
    /// The current READY candidate's transport, until it commits or is
    /// dropped.
    transport: Option<TransportGate>,
    /// The current play's origin.
    origin: TimeUs,
    generation: u64,
    /// The commands taken from the current play's queue: started, refused
    /// or discarded.
    consumed: u64,
    sealed: bool,
    exited: bool,
    /// A retired play the full return queue did not take.
    unreturned: Option<RetiredPlay>,
    /// The restores rearms replaced, back to the game thread for the next.
    rearmed: rtrb::Producer<Box<Restore>>,
    /// A replaced restore the full queue did not take.
    unreturned_restore: Option<Kept>,
    /// What waits aside for that one to go back, out of the control
    /// queue, so the commands behind it are still taken.
    held: Option<Held>,
    /// Where key sound latencies go, if anywhere; a full queue drops them.
    probe: Option<rtrb::Producer<AudioProbe>>,
    /// The number of the latest queue change applied.
    queue_change: u32,
    /// While the play's underruns are counted: its chart time 0.
    steady_from: Option<TimeUs>,
    /// The play's underruns counted so far.
    steady: u32,
    system_starts: rtrb::Consumer<SystemStart>,
    system_installs: rtrb::Consumer<SystemInstall>,
    system_returns: rtrb::Producer<SystemGeneration>,
    /// A test hook inside the system snapshot (dev-tools).
    #[cfg(feature = "dev-tools")]
    snapshot_hook: Option<SnapshotHook>,
}

/// A rearm the audio thread set aside while the restore it would replace
/// had nowhere to go back: applied once that one went back, or, cancelled
/// or superseded first, its own restore going back in its place.
#[derive(Debug)]
enum Held {
    /// A rearm not yet applied.
    Rearm {
        origin: TimeUs,
        gate: TransportGate,
        restore: Kept,
    },
    /// A rearm's restore that never played, to go back.
    Returning(Kept),
}

/// The two ends around `engine`, which is stopped until the first play.
#[must_use]
pub fn audio_control(mut engine: AudioEngine) -> (AudioControl, AudioWorker) {
    engine.stop();
    let (ctl_tx, ctl_rx) = rtrb::RingBuffer::new(CONTROL_CAPACITY);
    let (ret_tx, ret_rx) = rtrb::RingBuffer::new(RETIRED_CAPACITY);
    let (rearm_tx, rearm_rx) = rtrb::RingBuffer::new(RETIRED_CAPACITY);
    let (loads_tx, loads_rx) = rtrb::RingBuffer::new(LOADS_CAPACITY);
    let rate = engine.bank().rate();
    let line = engine.frame_line();
    let source = engine.seek_source();
    let (starts_tx, starts_rx) = rtrb::RingBuffer::new(SYSTEM_STARTS);
    let (installs_tx, installs_rx) = rtrb::RingBuffer::new(SYSTEM_INSTALLS);
    let (returns_tx, returns_rx) = rtrb::RingBuffer::new(SYSTEM_RETURNS);
    let shared = Arc::new(Shared {
        rate: line.rate,
        period: line.period,
        system_seq: AtomicU64::new(1),
        ..Shared::default()
    });
    let control = AudioControl {
        ctl: ctl_tx,
        retired: ret_rx,
        rearmed: rearm_rx,
        source,
        in_flight: false,
        loads: loads_rx,
        load_id: 0,
        outstanding: None,
        #[cfg(feature = "dev-tools")]
        full_for: Arc::default(),
        rate,
        shared: Arc::clone(&shared),
        generation: 0,
        accepted: None,
        queue_change: 0,
        system_starts: starts_tx,
        system_installs: installs_tx,
        system_returns: returns_rx,
        system_held: 0,
    };
    let worker = AudioWorker {
        engine,
        ctl: ctl_rx,
        retired: ret_tx,
        loads: loads_tx,
        unreturned_load: None,
        shared,
        // Before the first play: a queue nothing sends to.
        queue: audio_link(1).1,
        restore: None,
        transport: None,
        origin: TimeUs(0),
        generation: 0,
        consumed: 0,
        sealed: false,
        exited: false,
        unreturned: None,
        rearmed: rearm_tx,
        unreturned_restore: None,
        held: None,
        probe: None,
        queue_change: 0,
        steady_from: None,
        steady: 0,
        system_starts: starts_rx,
        system_installs: installs_rx,
        system_returns: returns_tx,
        #[cfg(feature = "dev-tools")]
        snapshot_hook: None,
    };
    // Before its first step, the line of an engine that has not woken.
    worker.publish_line();
    (control, worker)
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
            // The retired queue and restore are freed here, never on the
            // audio thread.
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

    /// Starts a play at [`Gain::HEADROOM`] ([`play_at`](Self::play_at)).
    ///
    /// # Errors
    ///
    /// As [`ready`](Self::ready); nothing is sent.
    pub fn play(&mut self, origin: TimeUs, queue: AudioQueue) -> Result<(), AudioWaitError> {
        self.play_at(origin, queue, Gain::HEADROOM, None)
    }

    /// Starts a play whose sum takes `gain` from its first frame to its
    /// end: the gain travels with the start, so it never changes inside a
    /// play, unlike the volume. One play switch is in flight at most: the
    /// previous start's retired queue comes back first
    /// ([`ready`](Self::ready)), so the return queue always has room and
    /// the audio thread never frees one.
    ///
    /// A practice round's `restore`, planned on `origin`'s frame line
    /// ([`AudioCounters::frame_line`]), is applied with the start: the
    /// first plan, the fallback when the first is late, and otherwise the
    /// schedule is stopped, so the round plays with its BGM silent rather
    /// than from chart time 0 ([`Restore::apply`]); the outcome is
    /// [`AudioCounters::restored`]. The box comes back with the queue and is
    /// freed here.
    ///
    /// # Errors
    ///
    /// As [`ready`](Self::ready); nothing is sent.
    pub fn play_at(
        &mut self,
        origin: TimeUs,
        queue: AudioQueue,
        gain: Gain,
        restore: Option<Box<Restore>>,
    ) -> Result<(), AudioWaitError> {
        self.start(origin, queue, gain, restore, None)
    }

    /// [`play_at`](Self::play_at) for a READY candidate whose transport
    /// waits for `gate` (design.md "Play Lifecycle", the start gate): the
    /// audio thread commits it in the pass reaching its commit point,
    /// and drops the play's sounds there when it was cancelled.
    ///
    /// # Errors
    ///
    /// As [`ready`](Self::ready); nothing is sent.
    pub fn play_gated(
        &mut self,
        origin: TimeUs,
        queue: AudioQueue,
        gain: Gain,
        restore: Option<Box<Restore>>,
        gate: TransportGate,
    ) -> Result<(), AudioWaitError> {
        self.start(origin, queue, gain, restore, Some(gate))
    }

    fn start(
        &mut self,
        origin: TimeUs,
        queue: AudioQueue,
        gain: Gain,
        restore: Option<Box<Restore>>,
        gate: Option<TransportGate>,
    ) -> Result<(), AudioWaitError> {
        self.ready()?;
        self.in_flight = true;
        self.generation += 1;
        self.accepted = Some(queue.accepted_count());
        let generation = self.generation;
        self.send(AudioCtl::Play {
            origin,
            queue,
            generation,
            gain,
            restore,
            gate,
        });
        Ok(())
    }

    /// The play's cancelled candidate is followed by another, chart time 0
    /// at `origin`, waiting for `gate`, a round's audio restored by
    /// `restore` ([`AudioCtl::Rearm`]). Send it once the cancellation is
    /// acknowledged ([`AudioCounters::dropped`]). The restore it replaces
    /// comes back ([`take_restore`](Self::take_restore)); while one
    /// replaced earlier is neither taken back nor queued, the audio thread
    /// sets the rearm aside until it is, still taking a cancel or an exit
    /// behind it, so none is freed there.
    pub fn rearm(&mut self, origin: TimeUs, gate: TransportGate, restore: Option<Box<Restore>>) {
        self.send(AudioCtl::Rearm {
            origin,
            gate,
            restore,
        });
    }

    /// A round's restore a rearm replaced, once the audio thread handed it
    /// back: the box the next rearm's restore is planned in.
    pub fn take_restore(&mut self) -> Option<Box<Restore>> {
        self.rearmed.pop().ok()
    }

    /// What the engine plays, as of its latest load: for a round's
    /// [`Restorer`](crate::Restorer). One `Arc` clone.
    #[must_use]
    pub fn seek_source(&self) -> SeekSource {
        self.source.clone()
    }

    /// Hands another chart's `bank` and `schedule` to the engine, between
    /// plays, and waits until the audio thread has taken them: what they
    /// replace comes back and is freed here, never on the audio thread.
    /// Nothing sounds until the next [`play`](Self::play). A
    /// [`switch`](Self::switch) polled to its answer or its bound.
    ///
    /// # Errors
    ///
    /// [`AudioLoadError::RateMismatch`] for a bank at another rate than the
    /// output's, sent nowhere; [`AudioWaitError::Gone`] once the worker is
    /// gone, and [`AudioWaitError::Timeout`] after [`SWITCH_TIMEOUT`]. A load
    /// that timed out stays outstanding: the engine may still take it, and
    /// until its answer comes back, which is freed here, no later load is
    /// sent, and a load made meanwhile is refused at once with
    /// [`AudioLoadError::Busy`]. So at most one load is ever queued or
    /// answered, and the audio thread always has room for its answer.
    pub fn load(
        &mut self,
        bank: Arc<SoundBank>,
        schedule: BgmSchedule,
    ) -> Result<(), AudioLoadError> {
        let mut switch = self.switch(bank, schedule)?;
        loop {
            if let SwitchPoll::Done(done) = self.poll_switch(&mut switch) {
                return done;
            }
            // An earlier load still unanswered refuses this one at once.
            if !switch.sent() && self.outstanding.is_some() {
                return Err(AudioLoadError::Busy);
            }
            if let Some(expired) = self.expired(&switch) {
                return Err(expired);
            }
            std::thread::sleep(Duration::from_micros(500));
        }
    }

    /// A switch of the engine's sounds to `bank` and `schedule`, made but
    /// not sent: it owns them until [`poll_switch`](Self::poll_switch) has
    /// put them in the control queue, and its bound ([`SWITCH_TIMEOUT`])
    /// runs from here. Its caller polls it at each of its own slices and
    /// may leave it at any of them.
    ///
    /// # Errors
    ///
    /// [`AudioLoadError::RateMismatch`] for a bank at another rate than the
    /// output's.
    pub fn switch(
        &self,
        bank: Arc<SoundBank>,
        schedule: BgmSchedule,
    ) -> Result<Switch, AudioLoadError> {
        if bank.rate() != self.rate {
            return Err(AudioLoadError::RateMismatch {
                bank: bank.rate(),
                output: self.rate,
            });
        }
        Ok(Switch {
            unsent: Some((bank, schedule)),
            id: None,
            since: Instant::now(),
        })
    }

    /// [`switch`](Self::switch) to an empty bank and schedule at the
    /// output's rate: the unload of [`unload`](Self::unload).
    #[must_use]
    pub fn unload_switch(&self) -> Switch {
        Switch {
            unsent: Some((
                Arc::new(SoundBank::from_sounds(self.rate, Vec::new())),
                BgmSchedule::new(Vec::new()),
            )),
            id: None,
            since: Instant::now(),
        }
    }

    /// One look at `switch`, waiting for nothing. Unsent: the answer of an
    /// earlier switch still outstanding is taken first (freed here, as
    /// [`load`](Self::load) frees it); while one is outstanding nothing is
    /// sent. Then one push of its load: a full control queue keeps the
    /// bank and schedule in the switch, unsent, and assigns nothing, so no
    /// number is ever outstanding that the worker was not given. Sent: its
    /// answer, if it came. [`SwitchPoll::Pending`] until then; a switch
    /// dropped sent stays outstanding, as a load that timed out does, and
    /// the next switch takes its answer.
    pub fn poll_switch(&mut self, switch: &mut Switch) -> SwitchPoll {
        if let Some(id) = switch.id {
            if let Ok(retired) = self.loads.pop() {
                let (answered, taken) = (retired.id, retired.taken);
                if answered == id && taken {
                    self.source = retired.source.clone();
                }
                drop(retired);
                if answered == id {
                    self.outstanding = None;
                    // The engine refuses only another rate, checked at the
                    // switch.
                    return SwitchPoll::Done(if taken {
                        Ok(())
                    } else {
                        Err(AudioLoadError::RateMismatch {
                            bank: self.rate,
                            output: self.rate,
                        })
                    });
                }
            }
            if self.loads.is_abandoned() {
                return SwitchPoll::Done(Err(AudioWaitError::Gone.into()));
            }
            return SwitchPoll::Pending;
        }
        // The answer to a load left unanswered is freed first.
        if self.outstanding.is_some() {
            if let Ok(late) = self.loads.pop() {
                if Some(late.id) == self.outstanding {
                    self.outstanding = None;
                    // The engine plays what that load left it.
                    if late.taken {
                        self.source = late.source.clone();
                    }
                }
                drop(late);
            }
            if self.outstanding.is_some() {
                if self.loads.is_abandoned() {
                    return SwitchPoll::Done(Err(AudioWaitError::Gone.into()));
                }
                return SwitchPoll::Pending;
            }
        }
        if self.ctl.is_abandoned() {
            return SwitchPoll::Done(Err(AudioWaitError::Gone.into()));
        }
        #[cfg(feature = "dev-tools")]
        if self
            .full_for
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
        {
            return SwitchPoll::Pending;
        }
        let Some((bank, schedule)) = switch.unsent.take() else {
            return SwitchPoll::Pending;
        };
        let id = self.load_id.wrapping_add(1);
        match self.ctl.push(AudioCtl::Load { bank, schedule, id }) {
            Ok(()) => {
                self.load_id = id;
                self.outstanding = Some(id);
                switch.id = Some(id);
            }
            Err(rtrb::PushError::Full(AudioCtl::Load { bank, schedule, .. })) => {
                switch.unsent = Some((bank, schedule));
            }
            Err(rtrb::PushError::Full(_)) => {}
        }
        SwitchPoll::Pending
    }

    /// How many more pushes of a switch find the control queue full
    /// (dev-tools): set it, and that many looks keep the load unsent, as
    /// [`poll_switch`](Self::poll_switch) keeps it when the queue is full.
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn forced_full(&self) -> Arc<std::sync::atomic::AtomicU32> {
        Arc::clone(&self.full_for)
    }

    /// Why `switch` ends unanswered once its bound has passed, if it has:
    /// [`AudioWaitError::Timeout`] once sent or while the control queue
    /// stayed full; [`AudioLoadError::Busy`] while an earlier load's answer
    /// kept it from being sent.
    #[must_use]
    pub fn expired(&self, switch: &Switch) -> Option<AudioLoadError> {
        (switch.since.elapsed() >= SWITCH_TIMEOUT).then(|| {
            if switch.id.is_none() && self.outstanding.is_some() {
                AudioLoadError::Busy
            } else {
                AudioWaitError::Timeout.into()
            }
        })
    }

    /// Takes the engine's sounds away: an empty bank and schedule at the
    /// output's rate are loaded as [`load`](Self::load) loads them, so the
    /// bank they replace, and the source the last load answered with, are
    /// freed here once the audio thread has taken them (design.md "Load
    /// Pipeline": no bank is held beside a new decode).
    ///
    /// # Errors
    ///
    /// As [`load`](Self::load).
    pub fn unload(&mut self) -> Result<(), AudioLoadError> {
        let empty = Arc::new(SoundBank::from_sounds(self.rate, Vec::new()));
        self.load(empty, BgmSchedule::new(Vec::new()))
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

    /// Evaluation completed: [`done`](Self::done) turns true when the
    /// play's audio has ended, every key sound its queue accepted
    /// included.
    pub fn seal(&mut self) {
        self.send(AudioCtl::Seal);
    }

    /// Sealed, and the play rings out from `fade_at` to `end_at` in its
    /// chart time ([`AudioCtl::RingOut`]): [`done`](Self::done)
    /// turns true once every sound has stopped, at the latest when the
    /// output reaches `end_at`. The audio thread mixes ahead, so that can
    /// be before the play's time reaches `end_at`.
    pub fn ring_out(&mut self, fade_at: TimeUs, end_at: TimeUs) {
        self.send(AudioCtl::RingOut { fade_at, end_at });
    }

    /// Whether the sealed play's audio has ended: the audio thread's
    /// latest report names this play and every key-sound command its queue
    /// has accepted. A report made before a later command was accepted is
    /// stale and does not count; the game thread, which alone sends them,
    /// reads the accepted count after its last send.
    #[must_use]
    pub fn done(&self) -> bool {
        let Some(accepted) = self.accepted.as_ref().filter(|_| self.generation != 0) else {
            return false;
        };
        let report = self.shared.done.load(Ordering::Acquire);
        let accepted = accepted.load(Ordering::Acquire);
        report == done_report(self.generation, accepted)
    }

    /// The output's failure, once it failed.
    #[must_use]
    pub fn failure(&self) -> Option<OutputError> {
        self.shared.failure()
    }

    /// The output's failure as the audio thread latched it, readable from
    /// any thread.
    #[must_use]
    pub fn failure_probe(&self) -> OutputProbe {
        OutputProbe(Arc::clone(&self.shared))
    }

    /// Sets `channel`'s volume from the next engine step on, whatever the
    /// play's state; a newer volume of that channel replaces one not yet
    /// taken.
    ///
    /// # Errors
    ///
    /// [`InvalidVolume`] for a volume outside `0.0..=MAX_VOLUME`, which is
    /// not sent.
    pub fn set_volume(&self, channel: Channel, volume: f32) -> Result<(), InvalidVolume> {
        if !(0.0..=MAX_VOLUME).contains(&volume) {
            return Err(InvalidVolume(volume));
        }
        let pending = u64::from(volume.to_bits()) | 1 << 32;
        if let Some(word) = self.shared.volumes.get(channel.index()) {
            word.store(pending, Ordering::Release);
        }
        Ok(())
    }

    /// Starts the decision tone from the audio thread's next step on,
    /// whatever the play's state and whatever waits in the control queue:
    /// a UI sound outside every bank, which loading or unloading a bank does
    /// not cut ([`AudioEngine::play_tone`]).
    pub fn play_tone(&self) {
        #[cfg(feature = "dev-tools")]
        self.shared.tones.fetch_add(1, Ordering::AcqRel);
        self.shared.tone.store(true, Ordering::Release);
    }

    /// Draws the next number of the system sounds' request sequence:
    /// `None` once exhausted. A draw issues a number only below
    /// [`SEQ_LIMIT`] and a refused one changes nothing, so the sequence
    /// never wraps and, once exhausted, stays exhausted.
    #[must_use]
    pub fn system_seq(&self) -> Option<u64> {
        self.shared
            .system_seq
            .try_update(Ordering::AcqRel, Ordering::Acquire, |next| {
                (next < SEQ_LIMIT).then(|| next + 1)
            })
            .ok()
    }

    /// Queues a start of `kind` under a new number, `looped` until stopped,
    /// bound to `generation` when given (it plays only from that one), and
    /// publishes the number. Returns the number.
    ///
    /// A kind without a reserved voice leaves the ring's last
    /// [`RESERVED_HEADROOM`] places to the reserved kinds.
    ///
    /// # Errors
    ///
    /// [`SystemStartError::Full`] when the start ring is full, or, for a
    /// kind without a reserved voice, holds no more than
    /// [`RESERVED_HEADROOM`] free places, before any number is drawn; and
    /// [`SystemStartError::Exhausted`] once the sequence is; each is
    /// counted ([`AudioCounters::system_refused`]), except an exhausted
    /// decision sound's.
    pub fn system_start(
        &mut self,
        kind: SystemKind,
        looped: bool,
        generation: Option<GenId>,
    ) -> Result<u64, SystemStartError> {
        let refused = |e: SystemStartError| {
            if !(e == SystemStartError::Exhausted && kind == SystemKind::Decide) {
                self.shared
                    .system_refused_requests
                    .fetch_add(1, Ordering::AcqRel);
            }
            Err(e)
        };
        let room = if kind.is_reserved() {
            0
        } else {
            RESERVED_HEADROOM
        };
        if self.system_starts.slots() <= room {
            return refused(SystemStartError::Full);
        }
        let Some(seq) = self.system_seq() else {
            return refused(SystemStartError::Exhausted);
        };
        let start = SystemStart {
            kind,
            looped,
            generation,
            seq,
        };
        if self.system_starts.push(start).is_err() {
            return refused(SystemStartError::Full);
        }
        self.shared.system_published.store(seq, Ordering::Release);
        #[cfg(feature = "dev-tools")]
        if let Some(n) = self.shared.starts_queued.get(kind.index()) {
            n.fetch_add(1, Ordering::AcqRel);
        }
        Ok(seq)
    }

    /// Stops every voice of `kind` started before now, in every
    /// generation, at the audio thread's next step: the kind's stop word
    /// rises to a new number, or to [`TERMINAL`] once the sequence is
    /// exhausted, so a stop is never refused. Returns the number drawn.
    pub fn system_stop(&self, kind: SystemKind) -> Option<u64> {
        let seq = self.system_seq();
        if let Some(word) = self.shared.system_stops.get(kind.index()) {
            word.fetch_max(seq.unwrap_or(TERMINAL), Ordering::Release);
        }
        #[cfg(feature = "dev-tools")]
        if let Some(n) = self.shared.stops_drawn.get(kind.index()) {
            n.fetch_add(1, Ordering::AcqRel);
        }
        seq
    }

    /// Stops every voice of every kind started before now, in every
    /// generation, at the audio thread's next step, under one number: every
    /// stop word rises to it, or to [`TERMINAL`] once the sequence is
    /// exhausted, so it is never refused. Returns the number drawn.
    pub fn system_stop_all(&self) -> Option<u64> {
        let seq = self.system_seq();
        for word in &self.shared.system_stops {
            word.fetch_max(seq.unwrap_or(TERMINAL), Ordering::Release);
        }
        #[cfg(feature = "dev-tools")]
        for n in &self.shared.stops_drawn {
            n.fetch_add(1, Ordering::AcqRel);
        }
        seq
    }

    /// Whether the audio thread has ended: nothing it was handed is
    /// acknowledged any more.
    #[must_use]
    pub fn gone(&self) -> bool {
        self.shared.gone.load(Ordering::Acquire)
    }

    /// Queues `generation` to become the current system generation under
    /// a new number, and publishes the number: the audio thread applies it
    /// after every start numbered below it. The empty generation is queued
    /// at [`TERMINAL`] once the sequence is exhausted. Returns the number.
    ///
    /// It is admitted only while the generations handed over and not
    /// taken back ([`take_system_return`](Self::take_system_return)) number
    /// fewer than [`SYSTEM_RETURNS`], or, for the empty generation, one
    /// more. The current one never waits in the return ring, so every
    /// generation the audio thread replaces has a return slot, and none is
    /// ever held there without one.
    ///
    /// # Errors
    ///
    /// [`SystemInstallError`] with the generation, unqueued: a full ring,
    /// the generations not taken back at their bound, an exhausted
    /// sequence, or another rate than the output's.
    pub fn install_system(
        &mut self,
        generation: SystemGeneration,
    ) -> Result<u64, SystemInstallError> {
        if !generation.is_empty() && generation.rate() != self.rate {
            let rate = generation.rate();
            return Err(SystemInstallError::RateMismatch {
                generation,
                rate,
                output: self.rate,
            });
        }
        if self.system_installs.is_full() {
            return Err(SystemInstallError::Full(generation));
        }
        let bound = SYSTEM_RETURNS + usize::from(generation.is_empty());
        if self.system_held >= bound {
            return Err(SystemInstallError::Unreturned(generation));
        }
        let seq = match self.system_seq() {
            Some(seq) => seq,
            None if generation.is_empty() => TERMINAL,
            None => return Err(SystemInstallError::Exhausted(generation)),
        };
        if let Err(rtrb::PushError::Full(install)) =
            self.system_installs.push(SystemInstall { generation, seq })
        {
            return Err(SystemInstallError::Full(install.generation));
        }
        self.system_held += 1;
        self.shared.system_published.store(seq, Ordering::Release);
        Ok(seq)
    }

    /// The system generation the audio thread has installed last.
    #[must_use]
    pub fn system_installed(&self) -> Option<GenId> {
        let id = self.shared.system_installed.load(Ordering::Acquire);
        (id != 0).then_some(GenId(id))
    }

    /// A system generation the audio thread is done with, to be dropped
    /// here; taking it frees its place for another install.
    pub fn take_system_return(&mut self) -> Option<SystemGeneration> {
        let generation = self.system_returns.pop().ok()?;
        self.system_held = self.system_held.saturating_sub(1);
        Some(generation)
    }

    /// Sets the next number the request sequence issues (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn seed_system_seq(&self, next: u64) {
        self.shared.system_seq.store(next, Ordering::Release);
    }

    /// The next number the request sequence would issue (dev-tools).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn system_seq_word(&self) -> u64 {
        self.shared.system_seq.load(Ordering::Acquire)
    }

    /// `kind`'s stop word (dev-tools).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn system_stop_word(&self, kind: SystemKind) -> u64 {
        self.shared.system_stop_word(kind)
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

impl Drop for AudioWorker {
    /// The thread's end goes with it, so the control reads the output as
    /// gone, as it would an abandoned queue.
    fn drop(&mut self) {
        self.shared.gone.store(true, Ordering::Release);
    }
}

impl AudioWorker {
    /// Takes every control message; returns `false` once told to exit.
    fn control(&mut self) -> bool {
        #[cfg(debug_assertions)]
        let _in_step = InStep::enter();
        for (word, channel) in self.shared.volumes.iter().zip(Channel::ALL) {
            let volume = word.swap(0, Ordering::Acquire);
            if volume != 0 {
                // Checked by `set_volume`.
                let _ = self
                    .engine
                    .set_volume(channel, f32::from_bits(volume as u32));
            }
        }
        self.system_control();
        if let Some(old) = self.unreturned_restore.take() {
            self.give_back(old);
        }
        // What waits aside goes ahead of every command behind it, once the
        // restore before it went back; else it stays.
        let free = self.unreturned_restore.is_none();
        match self.held.take() {
            None => {}
            Some(Held::Rearm {
                origin,
                gate,
                restore,
            }) => {
                if free {
                    self.rearm(origin, gate, Some(restore));
                } else {
                    self.held = Some(Held::Rearm {
                        origin,
                        gate,
                        restore,
                    });
                }
            }
            Some(Held::Returning(restore)) => {
                if free {
                    self.give_back(restore);
                } else {
                    self.held = Some(Held::Returning(restore));
                }
            }
        }
        if let Some(old) = self.unreturned.take()
            && let Err(rtrb::PushError::Full(old)) = self.retired.push(old)
        {
            self.unreturned = Some(old);
        }
        if let Some(old) = self.unreturned_load.take()
            && let Err(rtrb::PushError::Full(old)) = self.loads.push(old)
        {
            self.unreturned_load = Some(old);
        }
        loop {
            // An answer not yet handed back is never replaced: a load behind
            // it waits in the queue, still owned by it, until the return
            // queue takes the answer.
            if self.unreturned_load.is_some()
                && matches!(self.ctl.peek(), Ok(AudioCtl::Load { .. }))
            {
                break;
            }
            // Nor is a replaced restore not yet handed back: a rearm
            // carrying another is set aside until it is (below), and one
            // more, with that place taken, waits in the queue, so no
            // restore is ever freed on this thread. The game thread takes
            // the returns back at every replan and attempt start, so the
            // place is free whenever a rearm can follow.
            if self.held.is_some()
                && matches!(
                    self.ctl.peek(),
                    Ok(AudioCtl::Rearm {
                        restore: Some(_),
                        ..
                    })
                )
            {
                break;
            }
            let Ok(msg) = self.ctl.pop() else {
                break;
            };
            match msg {
                AudioCtl::Load { bank, schedule, id } => {
                    self.steady_from = None;
                    self.discard_commands();
                    let (taken, (bank, schedule)) = match self.engine.replace(bank, schedule) {
                        Ok(old) => (true, old),
                        Err(refused) => (false, refused),
                    };
                    let retired = Retired {
                        _bank: bank,
                        _schedule: schedule,
                        id,
                        taken,
                        source: self.engine.seek_source(),
                    };
                    // One load is outstanding at a time, so the queue has
                    // room; were it full, the answer would wait here, still
                    // owned, rather than be freed on this thread.
                    if let Err(rtrb::PushError::Full(retired)) = self.loads.push(retired) {
                        self.unreturned_load = Some(retired);
                    }
                }
                AudioCtl::Play {
                    origin,
                    queue,
                    generation,
                    gain,
                    restore,
                    gate,
                } => {
                    // A rearm set aside belonged to the play before.
                    self.release_held(false);
                    // A `Gain` is always in range.
                    let _ = self.engine.set_gain(gain.get());
                    let restore = restore.map(|r| Kept(Some(r)));
                    self.begin(
                        origin,
                        gate,
                        restore.as_ref().and_then(Kept::get),
                        generation,
                    );
                    let old = (
                        std::mem::replace(&mut self.queue, queue),
                        std::mem::replace(&mut self.restore, restore).and_then(Kept::give),
                    );
                    // The return queue has room: the game thread took the
                    // previous start's queue before sending this one. Were
                    // it full, the pair would wait here, still owned,
                    // rather than be freed on this thread.
                    if let Err(rtrb::PushError::Full(old)) = self.retired.push(old) {
                        self.unreturned = Some(old);
                    }
                    self.generation = generation;
                    self.consumed = 0;
                    self.sealed = false;
                    self.steady_from = Some(origin);
                    self.steady = 0;
                    self.publish_steady();
                }
                AudioCtl::Cancel => {
                    self.release_held(true);
                    self.drop_play();
                }
                AudioCtl::Rearm {
                    origin,
                    gate,
                    restore: Some(restore),
                } if self.unreturned_restore.is_some() => {
                    // Nothing is held: a rearm behind one held waits in
                    // the queue (above).
                    debug_assert!(self.held.is_none(), "a held rearm replaced");
                    self.held = Some(Held::Rearm {
                        origin,
                        gate,
                        restore: Kept(Some(restore)),
                    });
                }
                AudioCtl::Rearm {
                    origin,
                    gate,
                    restore,
                } => self.rearm(origin, gate, restore.map(|r| Kept(Some(r)))),
                AudioCtl::Seal => self.sealed = true,
                AudioCtl::RingOut { fade_at, end_at } => {
                    self.engine.ring_out(fade_at, end_at);
                    self.sealed = true;
                }
                AudioCtl::Exit => self.exited = true,
            }
        }
        !self.exited
    }

    /// The system sounds' control step: a snapshot, in this order, of the
    /// tone word (taken), the cutoff (the game thread's publication word),
    /// each ring's readable count and the stop words; then the requests it
    /// admits ([`SystemVoices::apply`](::audio::SystemVoices::apply)), and
    /// the installed generation's id published.
    fn system_control(&mut self) {
        let Self {
            engine,
            shared,
            system_starts,
            system_installs,
            system_returns,
            #[cfg(feature = "dev-tools")]
            snapshot_hook,
            ..
        } = self;
        #[cfg(feature = "dev-tools")]
        let mut hook = |point| {
            if let Some(SnapshotHook(hook)) = snapshot_hook {
                hook(point);
            }
        };
        let tone = shared.tone.swap(false, Ordering::Acquire);
        let cutoff = shared.system_published.load(Ordering::Acquire);
        #[cfg(feature = "dev-tools")]
        hook(SnapshotPoint::AfterCutoff);
        let starts = system_starts.slots();
        #[cfg(feature = "dev-tools")]
        hook(SnapshotPoint::BetweenRings);
        let installs = system_installs.slots();
        let stops = shared
            .system_stops
            .each_ref()
            .map(|word| word.load(Ordering::Acquire));
        #[cfg(feature = "dev-tools")]
        hook(SnapshotPoint::AfterSnapshot);
        let snap = SystemSnapshot {
            tone,
            cutoff,
            starts,
            installs,
            stops,
        };
        let system = engine.system_mut();
        let report = system.apply(&snap, system_starts, system_installs, system_returns);
        if let Some(id) = report.installed {
            shared.system_installed.store(id.0, Ordering::Release);
        }
        shared
            .system_refused
            .store(system.refused(), Ordering::Release);
        shared
            .system_install_waits
            .store(system.install_waits(), Ordering::Release);
        #[cfg(feature = "dev-tools")]
        {
            shared
                .tones_started
                .store(system.tones_started(), Ordering::Release);
            for (word, kind) in shared.reserved.iter().zip(RESERVED_KINDS) {
                let voice = system
                    .reserved_voice(kind)
                    .map_or(0, |(id, looped)| (id.0 << 1) | u64::from(looped));
                word.store(voice, Ordering::Release);
            }
        }
    }

    /// Hands a silent retired system generation back: after the mix, and
    /// at each control step after an output failure.
    fn system_sweep(&mut self) {
        self.engine.system_mut().sweep(&mut self.system_returns);
    }

    /// Runs `hook` at each point of the system snapshot (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn set_snapshot_hook(&mut self, hook: impl FnMut(SnapshotPoint) + Send + 'static) {
        self.snapshot_hook = Some(SnapshotHook(Box::new(hook)));
    }

    /// The rearmed candidate's start, chart time 0 at `origin`, waiting
    /// for `gate`; a round's `restore` replaces the play's, which goes
    /// back to the game thread.
    fn rearm(&mut self, origin: TimeUs, gate: TransportGate, restore: Option<Kept>) {
        self.begin(
            origin,
            Some(gate),
            restore.as_ref().and_then(Kept::get),
            self.generation,
        );
        self.steady_from = Some(origin);
        match restore {
            None => {}
            Some(restore) => match self.restore.replace(restore) {
                None => {}
                Some(old) => self.give_back(old),
            },
        }
    }

    /// Hands `restore` back to the game thread, or keeps it until the
    /// return queue has room; never freed here. Only with no restore
    /// waiting to go back already: every caller took that one first.
    fn give_back(&mut self, restore: Kept) {
        debug_assert!(self.unreturned_restore.is_none(), "a return replaced");
        let Some(boxed) = restore.give() else {
            return;
        };
        match self.rearmed.push(boxed) {
            Ok(()) => {}
            Err(rtrb::PushError::Full(boxed)) => {
                self.unreturned_restore = Some(Kept(Some(boxed)));
            }
        }
    }

    /// A rearm set aside is cancelled, or superseded by a play, before it
    /// was applied: its restore goes back unplayed, and a cancellation
    /// acknowledges its candidate's drop (`acknowledge`). One already on
    /// its way back stays so.
    fn release_held(&mut self, acknowledge: bool) {
        match self.held.take() {
            None => {}
            Some(Held::Rearm { gate, restore, .. }) => {
                if acknowledge {
                    self.shared
                        .dropped
                        .store(u64::from(gate.generation) + 1, Ordering::Release);
                }
                self.held = Some(Held::Returning(restore));
            }
            Some(Held::Returning(restore)) => self.held = Some(Held::Returning(restore)),
        }
    }

    /// A play's or a rearmed candidate's start, chart time 0 at `origin`:
    /// a round's `restore` applied (the first plan, the fallback when it is
    /// late, else silence; never the schedule from 0), the transport held
    /// for `gate`'s commit if there is one, and the restore's outcome
    /// published for play `generation`.
    fn begin(
        &mut self,
        origin: TimeUs,
        gate: Option<TransportGate>,
        restore: Option<&Restore>,
        generation: u64,
    ) {
        self.engine.restart(origin);
        self.origin = origin;
        let restored = restore.map_or(0, |r| r.apply(&mut self.engine).code());
        if let Some(t) = &gate {
            self.engine.hold_transport(t.commit_at);
        }
        self.transport = gate;
        self.shared
            .restored
            .store(generation << 8 | u64::from(restored), Ordering::Release);
    }

    /// Stops every sound of the play and discards its queued key sounds;
    /// a READY candidate's transport still waiting is dropped, and its
    /// cancellation acknowledged.
    fn drop_play(&mut self) {
        self.steady_from = None;
        self.engine.stop();
        self.discard_commands();
        if let Some(t) = self.transport.take() {
            self.shared
                .dropped
                .store(u64::from(t.generation) + 1, Ordering::Release);
        }
    }

    /// Discards the play's queued key-sound commands, counted as taken.
    fn discard_commands(&mut self) {
        while self.queue.commands().pop().is_ok() {
            self.consumed = self.consumed.saturating_add(1);
        }
    }

    /// One engine pass, the READY candidate's transport held: the pass
    /// whose block reaches its commit point, by that pass's own wake and
    /// actual block, commits it before anything of the schedule starts,
    /// or drops the play when the commit finds the candidate cancelled or
    /// stale.
    fn engine_step(
        &mut self,
        out: &mut impl AudioOutput,
    ) -> Result<::audio::StepReport, OutputError> {
        let Self {
            engine,
            queue,
            transport,
            ..
        } = self;
        let mut admit = || {
            transport
                .as_ref()
                .is_some_and(|t| t.gate.commit(t.generation))
        };
        let report = engine.step_admitting(out, queue.commands(), &mut admit)?;
        self.consumed = self.consumed.saturating_add(u64::from(report.commands));
        match report.transport {
            Some(true) => self.transport = None,
            Some(false) => self.drop_play(),
            None => {}
        }
        Ok(report)
    }

    /// Publishes where a play's chart times fall on the output now
    /// ([`AudioCounters::frame_line`]): the two stores sit between two
    /// steps of the publication count, so a reader sees them together.
    fn publish_line(&self) {
        let line = self.engine.frame_line();
        let s = &*self.shared;
        s.line_seq.fetch_add(1, Ordering::AcqRel);
        s.line_shift.store(
            self.origin.0.saturating_sub(line.base_us),
            Ordering::Release,
        );
        s.line_written.store(line.block_start, Ordering::Release);
        s.line_seq.fetch_add(1, Ordering::AcqRel);
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
        let stepped = self.engine_step(out);
        self.system_sweep();
        // A failed write may have mixed, and stolen voices, first.
        self.shared
            .deferred_wakes
            .store(self.engine.deferred_wakes(), Ordering::Release);
        self.shared
            .steals
            .store(self.engine.mixer().steals(), Ordering::Release);
        let report = match stepped {
            Ok(report) => report,
            Err(e) => {
                let _ = self.shared.failure.set(e);
                return Err(e);
            }
        };
        self.publish_line();
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
            self.shared.done.store(
                done_report(self.generation, self.consumed),
                Ordering::Release,
            );
        }
        Ok(true)
    }

    /// Steps until told to exit. After an output failure every system
    /// voice and the tone end, and it only takes control messages, every
    /// `idle`, until then: system stops and installs still apply and a
    /// silent retired generation still goes back, while every system start
    /// is discarded.
    pub fn run(&mut self, out: &mut impl AudioOutput, idle: std::time::Duration) {
        loop {
            match self.step(out) {
                Ok(true) => {}
                Ok(false) => return,
                Err(_) => {
                    self.engine.system_mut().fail();
                    while self.control() {
                        self.system_sweep();
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

/// A report that play `generation`'s audio has ended with `consumed`
/// key-sound commands taken: each in 32 bits, the generation high. The
/// count starts over with each play, and 2^32 key sounds in one play are
/// more than eight months of one every 5 ms, so its 32 bits never wrap
/// onto a stale report.
const fn done_report(generation: u64, consumed: u64) -> u64 {
    (generation & 0xffff_ffff) << 32 | consumed & 0xffff_ffff
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use ::audio::{AudioEngine, SoundBank};
    use bms_model::TimeUs;

    use super::audio_control;

    /// A read of the frame line overlapping a publication that never ends
    /// (the audio thread stopped between its stores) gives up after a
    /// bounded number of tries and takes what it read, rather than spin.
    #[test]
    fn a_frame_line_read_is_bounded() {
        let bank = Arc::new(SoundBank::from_sounds(48_000, Vec::new()));
        let engine = AudioEngine::new(bank, &[], 48_000, 480, 4, 0).unwrap();
        let (control, worker) = audio_control(engine);
        let counters = control.counters();
        let settled = counters.frame_line(TimeUs(1_000));
        assert_eq!(settled.base_us, 1_000);
        assert_eq!((settled.rate, settled.period), (48_000, 480));
        // Mid-publication: odd, with a shift the worker never finished.
        worker.shared.line_seq.fetch_add(1, Ordering::AcqRel);
        worker.shared.line_shift.store(500, Ordering::Release);
        let torn = counters.frame_line(TimeUs(1_000));
        assert_eq!(torn.base_us, 500);
    }
}
