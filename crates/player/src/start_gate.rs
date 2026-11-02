//! The start gate's player side (design.md "Play Lifecycle", the start
//! gate): the real threads' part of the handshake the portable lifecycle
//! (`game::live`) drives through [`PlayerRig`](crate::play::PlayerRig).
//!
//! - **Input thread**: the adjustment tap goes to it once
//!   ([`PlayerGate::install`]); a gated opening asks it to open only where
//!   START and SELECT are verifiably released, and a rearm hands it the
//!   readers and buffers reserved with the play's setup, through a bounded
//!   queue, to seal the cancelled opening and open the next at one
//!   boundary ([`input_side::Rearm`]).
//! - **Audio thread**: the play starts with its transport held for the
//!   candidate's generation ([`game::TransportGate`]); a cancellation is
//!   acknowledged ([`game::AudioCounters::dropped`]) before the next
//!   candidate's rearm, which carries a round's restore planned again in a
//!   spare box ([`game::Restorer::replan`]).
//!
//! Production constructs no gate until S10-04b: its plays keep opening
//! their first READY as the play. Development runs install one.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![cfg_attr(
    not(feature = "dev-tools"),
    expect(
        dead_code,
        reason = "production installs no start gate until S10-04b turns it on; development runs do"
    )
)]

use std::time::{Duration, Instant};

use audio::OutputError;
use bms_model::TimeUs;
use eval::InputStart;
use game::{AudioControl, HistoryRecorder, InputWaitError, Opening, Rearm as LiveRearm};
use input::{
    AdjustmentBindings, AdjustmentReceiver, GatePhase, GateState, HoldLatch, SourceReader,
    StartGate, TapError, adjustment_queue,
};
use replay::{CancelReason, MAX_CANCELLED_EVENTS, MAX_OPENING_ARRIVALS};

use crate::input_side::{self, InputLink, RearmOutcome};

/// How long a cancellation's acknowledgement is waited for: far beyond a
/// period, which is when the audio thread takes the cancel.
const DROP_TIMEOUT: Duration = game::SWITCH_TIMEOUT;

/// The start gate of the player's plays.
pub struct PlayerGate {
    gate: StartGate,
    latch: HoldLatch,
    /// The tap's queue's other end, kept so the queue stays open; the
    /// controls' reducer reads it from S10-04b.
    _adjust: AdjustmentReceiver,
    /// The candidate armed last.
    armed: u32,
    /// Whether the attempt's first candidate was handed to audio.
    started: bool,
    /// The rearm's readers and buffers, reserved at the first opening and
    /// reused by every rearm of the run.
    rearm: Option<Box<input_side::Rearm>>,
    /// The input side's held-snapshot storage.
    snapshot: usize,
    /// A round's restore box the next rearm plans in.
    spare: Option<Box<game::Restore>>,
    /// The selection's loading lifetime is paused, not ended, until a
    /// candidate opens: a hold at the opening resumes it.
    loading: bool,
    /// A cancellation the audio thread never acknowledged: its output
    /// counts as failed, so no candidate opens after it.
    unacknowledged: bool,
}

impl PlayerGate {
    /// Sends the input thread a tap of `bindings` and keeps its start gate
    /// and hold latch.
    ///
    /// # Errors
    ///
    /// What the tap refuses of `bindings`.
    pub fn install(input: &InputLink, bindings: AdjustmentBindings) -> Result<Self, TapError> {
        let (mut tap, receiver) = adjustment_queue(bindings)?;
        tap.set_active(true);
        let gate = tap.start_gate();
        let latch = tap.latch();
        input.tap(tap);
        Ok(Self {
            gate,
            latch,
            _adjust: receiver,
            armed: 0,
            started: false,
            rearm: None,
            snapshot: 0,
            spare: None,
            loading: false,
            unacknowledged: false,
        })
    }

    /// Candidate `generation` opened with `opening`: the rearm's buffers
    /// are reserved once, for the input side's snapshot storage.
    pub fn opened(&mut self, generation: u32, opening: &Opening<SourceReader>) {
        self.armed = generation;
        self.started = false;
        let snapshot = opening.start.held.capacity();
        if self
            .rearm
            .as_ref()
            .is_some_and(|r| r.start.held.capacity() >= snapshot)
        {
            self.snapshot = snapshot;
            return;
        }
        self.snapshot = snapshot;
        self.rearm = Some(Box::new(input_side::Rearm {
            readers: Vec::new(),
            lead_in: TimeUs(0),
            generation: 0,
            start: InputStart {
                opened_at: TimeUs(0),
                held: Vec::with_capacity(snapshot),
            },
            arrivals: Vec::with_capacity(MAX_OPENING_ARRIVALS),
            leftover: Vec::with_capacity(MAX_CANCELLED_EVENTS),
            room: 0,
            outcome: RearmOutcome::Pending,
        }));
    }

    /// The selection's loading lifetime paused before an opening.
    pub fn pause_loading(&mut self) {
        self.loading = true;
    }

    /// Whether a loading lifetime is paused before an opening.
    pub fn loading_paused(&self) -> bool {
        self.loading
    }

    /// A candidate opened: whether a paused loading lifetime is to end.
    pub fn take_loading(&mut self) -> bool {
        std::mem::take(&mut self.loading)
    }

    /// The candidate armed last is handed to audio, with a round's
    /// `restore`: the restores earlier rearms replaced are taken back from
    /// the audio thread first, so it never holds one it cannot return, and
    /// a copy of `restore` is kept for this chart's first rearm to plan in.
    /// The play's setup, before recording's in-play work.
    pub fn started(&mut self, control: &mut AudioControl, restore: Option<&game::Restore>) {
        self.started = true;
        while control.take_restore().is_some() {}
        self.spare = restore.map(|r| Box::new(r.clone()));
    }

    /// Whether the attempt's first candidate was handed to audio.
    #[must_use]
    pub const fn is_started(&self) -> bool {
        self.started
    }

    /// The input side's held-snapshot storage.
    #[must_use]
    pub const fn snapshot(&self) -> usize {
        self.snapshot
    }

    /// Whether START or SELECT is held, or may be, as the latch reads: when
    /// to ask the input side again.
    #[must_use]
    pub fn holding(&self) -> bool {
        let hold = self.latch.read();
        hold.held || hold.unverified
    }

    /// Whether a cancellation went unacknowledged by the audio thread
    /// ([`wait_dropped`]): the output's failure, which ends the attempt
    /// before another candidate opens.
    #[must_use]
    pub const fn unacknowledged(&self) -> bool {
        self.unacknowledged
    }

    /// The start gate now.
    #[must_use]
    pub fn state(&self) -> GateState {
        self.gate.state()
    }

    /// Cancels candidate `generation` unless it committed first: `true`
    /// when it committed.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn settle(&self, generation: u32) -> bool {
        let _ = self.gate.cancel();
        self.gate.state()
            == GateState {
                generation,
                phase: GatePhase::Committed,
            }
    }

    /// The transport authorization of the candidate armed last, committing
    /// at chart time `commit_at`.
    #[must_use]
    pub fn transport(&self, commit_at: TimeUs) -> game::TransportGate {
        game::TransportGate {
            gate: self.gate.clone(),
            generation: self.armed,
            commit_at,
        }
    }

    /// Plans a rearmed round's restore at chart time `at` on the frame line
    /// of `origin`, in the spare box (or the one the audio thread gave
    /// back): nothing is allocated.
    ///
    /// # Errors
    ///
    /// What [`game::Restorer::replan`] refuses, or no box to plan in.
    pub fn replan(
        &mut self,
        restorer: &game::Restorer,
        control: &mut AudioControl,
        origin: TimeUs,
        at: TimeUs,
    ) -> Result<(), game::RestoreError> {
        if self.spare.is_none() {
            self.spare = control.take_restore();
        }
        let spare = self
            .spare
            .as_deref_mut()
            .ok_or(game::RestoreError::NotPrepared)?;
        restorer.replan(spare, control.counters().frame_line(origin), at)
    }

    /// The rearmed candidate `generation`'s start, chart time 0 at
    /// `origin`, its transport committing at `commit_at`, a round's restore
    /// with it.
    pub fn restart(
        &mut self,
        control: &mut AudioControl,
        origin: TimeUs,
        generation: u32,
        commit_at: TimeUs,
        round: bool,
    ) {
        self.armed = generation;
        let restore = if round { self.spare.take() } else { None };
        control.rearm(origin, self.transport(commit_at), restore);
    }

    /// Seals the cancelled candidate's opening and opens candidate
    /// `generation` through the input thread ([`InputLink::rearm`]): the
    /// game thread drains `readers` into `history` first, the input thread
    /// takes what came since at its boundary, and the sealed opening is
    /// kept with its closing time and arrivals before the next candidate's
    /// snapshot is copied into `start`. A history that ran out, or buffers
    /// the input side found too small, seals the capture with every event
    /// that fits: nothing opens.
    ///
    /// # Errors
    ///
    /// The input thread's failure.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn rearm(
        &mut self,
        input: &mut InputLink,
        readers: &mut Vec<SourceReader>,
        lead_in: TimeUs,
        generation: u32,
        history: &mut HistoryRecorder,
        start: &mut InputStart,
    ) -> Result<LiveRearm, InputWaitError> {
        let Some(mut rearm) = self.rearm.take() else {
            return Err(InputWaitError::Gone);
        };
        if history.drain(readers).is_err() {
            self.rearm = Some(rearm);
            return Ok(LiveRearm::Held);
        }
        rearm.readers = std::mem::take(readers);
        rearm.lead_in = lead_in;
        rearm.generation = generation;
        rearm.room = history.room();
        let mut rearm = input.rearm(rearm)?;
        *readers = std::mem::take(&mut rearm.readers);
        let outcome = rearm.outcome;
        let answer = match outcome {
            RearmOutcome::Opened { origin, closed } => {
                let kept = history.take(&rearm.leftover).and(history.seal(
                    closed,
                    &rearm.arrivals,
                    CancelReason::Held,
                ));
                if kept.is_err() {
                    history.exhaust();
                }
                start.opened_at = rearm.start.opened_at;
                start.held.clear();
                let room = start.held.capacity();
                start
                    .held
                    .extend(rearm.start.held.iter().take(room).copied());
                self.armed = generation;
                LiveRearm::Opened { origin }
            }
            // What the input side popped is kept as far as the history
            // has room: the cancelled opening goes on, or, refused, its
            // capture is sealed with every event that fits.
            RearmOutcome::Held | RearmOutcome::Pending => {
                if history.take(&rearm.leftover).is_err() {
                    history.exhaust();
                }
                LiveRearm::Held
            }
            RearmOutcome::Refused => {
                let _ = history.take(&rearm.leftover);
                history.exhaust();
                LiveRearm::Held
            }
        };
        self.rearm = Some(rearm);
        Ok(answer)
    }
}

/// The audio output's failure: the worker's, or a gated candidate's
/// cancellation it never acknowledged, which counts as its stall.
pub fn output_failure(control: &AudioControl, gate: Option<&PlayerGate>) -> Option<OutputError> {
    control.failure().or_else(|| {
        gate.is_some_and(PlayerGate::unacknowledged)
            .then_some(OutputError::Stalled)
    })
}

/// Waits, at most [`DROP_TIMEOUT`], until the audio thread acknowledged
/// dropping candidate `generation`'s transport, or `gate` shows it
/// committed: no authorization of it is left when the next opens. A worker
/// that fails shows as the output's failure; one that does not answer in
/// time is marked so on the gate ([`PlayerGate::unacknowledged`]), and
/// counts as failed from then on.
pub fn wait_dropped(control: &AudioControl, gate: &mut PlayerGate, generation: u32) {
    let counters = control.counters();
    let begun = Instant::now();
    loop {
        if counters.dropped(generation)
            || gate.state().phase == GatePhase::Committed
            || control.failure().is_some()
        {
            return;
        }
        if begun.elapsed() >= DROP_TIMEOUT {
            gate.unacknowledged = true;
            return;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use audio::{AudioEngine, Gain, RecordingOutput, SimConfig, SimDevice, Sound, SoundBank};
    use bms_model::{BgmEvent, SoundId, TimeUs};
    use eval::ControlId;
    use game::{AudioWorker, RESTORE_AHEAD, Restorer, audio_control, audio_link_at};
    use input::{AdjustmentBindings, QpcClock, TapControl, Waker};

    use super::{PlayerGate, output_failure, wait_dropped};
    use crate::controls::Controls;
    use crate::input_side::{Hook, InputLink, Mode, link};

    const MS: i64 = 1_000;

    /// A gate installed on an input link with START on Q, and the link's
    /// two ends.
    fn installed() -> (PlayerGate, Hook, InputLink) {
        let controls = Arc::new(Controls::new(QpcClock::start()));
        let (hook, input) = link(controls, Mode::Clock, Waker::new().unwrap());
        let gate = PlayerGate::install(
            &input,
            AdjustmentBindings {
                controls: vec![(TapControl::Key(ControlId(0x10)), 0)],
                latched: 1,
                wrap: None,
            },
        )
        .unwrap();
        (gate, hook, input)
    }

    /// Three practice attempts with one rearm each through the gate, a
    /// stepped audio worker on the other end: each attempt's start takes
    /// back the restore the previous rearm replaced, so every rearm is
    /// taken at once (its candidate's drop acknowledged after it) and the
    /// audio thread never keeps a restore it cannot return.
    #[test]
    fn each_attempt_takes_back_what_the_last_rearm_replaced() {
        let bank = Arc::new(SoundBank::from_sounds(
            48_000,
            vec![Sound::mono(vec![1_000; 4 * 48_000])],
        ));
        let schedule = vec![BgmEvent {
            time: TimeUs(5_000_000),
            sound: SoundId(0),
        }];
        let engine =
            AudioEngine::with_schedule(bank, schedule.clone(), 48_000, 480, 16, 0).unwrap();
        let (mut control, mut worker) = audio_control(engine);
        let mut out =
            RecordingOutput::new(SimDevice::new(SimConfig::steady(48_000, 480, TimeUs(0))), 0);
        let mut steps = |worker: &mut AudioWorker, n: usize| {
            for _ in 0..n {
                worker.step(&mut out).unwrap();
            }
        };
        steps(&mut worker, 3);
        let restorer = Restorer::new(control.seek_source(), schedule);
        let (mut gate, _hook, _input) = installed();
        let at = TimeUs(6_000_000);
        // A round whose lead-in starts 20 ms from the output's position.
        let origin = |worker: &AudioWorker| {
            let now = i64::try_from(worker.engine().written()).unwrap() * 1_000 / 48;
            TimeUs(now + 20 * MS - (at.0 - RESTORE_AHEAD.0))
        };
        let mut generation = 0;
        for attempt in 0..3 {
            let first = origin(&worker);
            let plan = restorer
                .plan(control.counters().frame_line(first), at)
                .unwrap();
            generation += 1;
            gate.gate.arm(generation);
            gate.armed = generation;
            gate.started(&mut control, Some(&plan));
            let (_sink, queue) = audio_link_at(4, first);
            control
                .play_gated(first, queue, Gain::HEADROOM, Some(plan), gate.transport(at))
                .unwrap();
            steps(&mut worker, 2);
            assert!(!gate.settle(generation));
            control.cancel();
            steps(&mut worker, 2);
            assert!(control.counters().dropped(generation));
            let second = origin(&worker);
            generation += 1;
            gate.gate.arm(generation);
            gate.replan(&restorer, &mut control, second, at).unwrap();
            gate.restart(&mut control, second, generation, at, true);
            steps(&mut worker, 2);
            assert!(!gate.settle(generation));
            control.cancel();
            steps(&mut worker, 2);
            assert!(
                control.counters().dropped(generation),
                "attempt {attempt}: the rearm waits on the audio thread"
            );
        }
    }

    /// A cancellation the audio thread never acknowledges (a worker kept
    /// but never stepped): once the wait runs out the output counts as
    /// stalled, which ends the attempt before another candidate opens.
    #[test]
    fn an_unacknowledged_cancellation_fails_the_output() {
        let bank = Arc::new(SoundBank::from_sounds(
            48_000,
            vec![Sound::mono(vec![0; 480])],
        ));
        let engine = AudioEngine::with_schedule(bank, Vec::new(), 48_000, 480, 16, 0).unwrap();
        let (mut control, _worker) = audio_control(engine);
        let (mut gate, _hook, _input) = installed();
        gate.gate.arm(1);
        gate.armed = 1;
        let (_sink, queue) = audio_link_at(4, TimeUs(0));
        control
            .play_gated(
                TimeUs(0),
                queue,
                Gain::HEADROOM,
                None,
                gate.transport(TimeUs(0)),
            )
            .unwrap();
        assert!(!gate.settle(1));
        control.cancel();
        assert_eq!(output_failure(&control, Some(&gate)), None);
        wait_dropped(&control, &mut gate, 1);
        assert_eq!(
            output_failure(&control, Some(&gate)),
            Some(audio::OutputError::Stalled),
            "the next candidate may open without the acknowledgement"
        );
    }
}
