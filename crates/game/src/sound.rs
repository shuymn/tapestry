//! What the logic thread hands to audio: key sounds through a queue to the
//! audio thread's [`AudioEngine`].

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ::audio::{AudioEngine, AudioOutput, OutputError, SoundCmd};
use bms_model::TimeUs;
use eval::{JudgeEvent, JudgeEventKind};

use crate::logic::EventSink;

/// Sends each key sound (`PlaySound`) to the audio command queue, with the
/// receive time of the press behind it as the command's origin (stage1.md
/// D11). Other events are not audio's concern.
///
/// The queue is sized from the play's event bound ([`Logic::max_events`]),
/// so it cannot fill within a play; if it ever did, the sound would be
/// dropped and counted rather than block the logic thread.
///
/// [`Logic::max_events`]: crate::Logic::max_events
#[derive(Debug)]
pub struct AudioSink {
    cmds: rtrb::Producer<SoundCmd>,
    dropped: Arc<AtomicU64>,
    /// Where the play's time 0 lies on the output's timeline.
    shift: TimeUs,
}

impl AudioSink {
    /// Whether the audio side's end of the queue has been dropped.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        self.cmds.is_abandoned()
    }
}

impl EventSink for AudioSink {
    fn receive(&mut self, ev: &JudgeEvent) {
        let JudgeEventKind::PlaySound { sound, .. } = ev.kind else {
            return;
        };
        // A key sound always comes from a press; the event time stands in
        // only if an origin is ever missing.
        let origin = ev.origin.unwrap_or(ev.time);
        let cmd = SoundCmd {
            sound,
            origin: TimeUs(origin.0.saturating_add(self.shift.0)),
        };
        if self.cmds.push(cmd).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The audio thread's end of the command queue.
#[derive(Debug)]
pub struct AudioQueue {
    cmds: rtrb::Consumer<SoundCmd>,
    dropped: Arc<AtomicU64>,
}

impl AudioQueue {
    /// The commands, for [`AudioEngine::step`].
    pub fn commands(&mut self) -> &mut rtrb::Consumer<SoundCmd> {
        &mut self.cmds
    }

    /// Whether no key sound is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }

    /// Key sounds waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cmds.slots()
    }

    /// Sounds dropped because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// The link between logic and audio: a command queue holding `capacity`
/// sounds, for a play whose time is the output's.
#[must_use]
pub fn audio_link(capacity: usize) -> (AudioSink, AudioQueue) {
    audio_link_at(capacity, TimeUs(0))
}

/// [`audio_link`] for a play whose time 0 lies at `origin` on the output's
/// timeline: each command's origin is moved onto that timeline.
#[must_use]
pub fn audio_link_at(capacity: usize, origin: TimeUs) -> (AudioSink, AudioQueue) {
    let (producer, consumer) = rtrb::RingBuffer::new(capacity);
    let dropped = Arc::new(AtomicU64::new(0));
    (
        AudioSink {
            cmds: producer,
            dropped: Arc::clone(&dropped),
            shift: origin,
        },
        AudioQueue {
            cmds: consumer,
            dropped,
        },
    )
}

/// The runner's audio side: an engine playing into a simulated or recording
/// output, which the runner wakes whenever the output's next buffer event is
/// due. `O` is [`::audio::SimDevice`] or [`::audio::RecordingOutput`] around one.
#[derive(Debug)]
pub struct RunnerAudio<O> {
    engine: AudioEngine,
    output: O,
}

impl<O: AudioOutput + Wakes> RunnerAudio<O> {
    /// `engine` playing into `output`; the engine's chart time 0 is the
    /// runner clock's 0.
    #[must_use]
    pub fn new(engine: AudioEngine, output: O) -> Self {
        Self { engine, output }
    }

    /// The engine.
    #[must_use]
    pub fn engine(&self) -> &AudioEngine {
        &self.engine
    }

    /// The engine, to restart or adjust it between plays.
    pub(crate) fn engine_mut(&mut self) -> &mut AudioEngine {
        &mut self.engine
    }

    /// The output.
    #[must_use]
    pub fn output(&self) -> &O {
        &self.output
    }

    /// Runs every engine step whose buffer event is due before `now` (with
    /// `through`, at `now` too), in time order.
    ///
    /// # Errors
    ///
    /// The output's failure.
    pub(crate) fn step(
        &mut self,
        now: TimeUs,
        through: bool,
        queue: &mut AudioQueue,
    ) -> Result<(), OutputError> {
        loop {
            let at = self.output.next_wake();
            if at > now || (at == now && !through) {
                return Ok(());
            }
            self.wake(queue)?;
        }
    }

    /// When the output's next buffer event is due.
    pub(crate) fn next_wake(&self) -> TimeUs {
        self.output.next_wake()
    }

    /// Runs one engine step, for the output's next buffer event.
    ///
    /// # Errors
    ///
    /// The output's failure.
    pub(crate) fn wake(&mut self, queue: &mut AudioQueue) -> Result<(), OutputError> {
        self.engine
            .step(&mut self.output, queue.commands())
            .map(|_| ())
    }

    /// Stops the play's audio (an abort): voices, pending starts and the
    /// remaining BGM stop, and queued key sounds are discarded.
    pub(crate) fn cancel(&mut self, queue: &mut AudioQueue) {
        self.engine.stop();
        while queue.commands().pop().is_ok() {}
    }
}

/// An output whose next buffer event time is known in advance, so a
/// threadless runner can wake it at the right moment.
pub trait Wakes {
    /// When the next buffer event happens.
    fn next_wake(&self) -> TimeUs;
}

impl Wakes for ::audio::SimDevice {
    fn next_wake(&self) -> TimeUs {
        ::audio::SimDevice::next_wake(self)
    }
}

impl<O: AudioOutput + Wakes> Wakes for ::audio::RecordingOutput<O> {
    fn next_wake(&self) -> TimeUs {
        self.inner().next_wake()
    }
}

#[cfg(test)]
mod tests {
    use bms_model::{Lane, SoundId};

    use super::*;

    fn event(kind: JudgeEventKind, origin: Option<TimeUs>) -> JudgeEvent {
        JudgeEvent {
            seq: 0,
            time: TimeUs(5_000),
            origin,
            kind,
        }
    }

    #[test]
    fn only_key_sounds_are_sent_and_a_full_queue_counts_drops() {
        let (mut sink, mut queue) = audio_link(1);
        let play = JudgeEventKind::PlaySound {
            lane: Lane::SCRATCH,
            sound: SoundId(7),
        };
        sink.receive(&event(JudgeEventKind::SongEnd, None));
        sink.receive(&event(play, Some(TimeUs(4_321))));
        sink.receive(&event(play, Some(TimeUs(4_322))));
        assert_eq!(queue.dropped(), 1);
        assert_eq!(
            queue.commands().pop(),
            Ok(SoundCmd {
                sound: SoundId(7),
                origin: TimeUs(4_321)
            })
        );
        assert!(queue.commands().pop().is_err());
        // Without an origin, the event time stands in.
        sink.receive(&event(play, None));
        assert_eq!(queue.commands().pop().unwrap().origin, TimeUs(5_000));
    }

    #[test]
    fn a_play_away_from_the_output_origin_moves_each_origin_onto_it() {
        // The play's time 0 is at 2.5 s on the output's timeline.
        let (mut sink, mut queue) = audio_link_at(2, TimeUs(2_500_000));
        let play = JudgeEventKind::PlaySound {
            lane: Lane::SCRATCH,
            sound: SoundId(3),
        };
        sink.receive(&event(play, Some(TimeUs(-4_321))));
        assert_eq!(
            queue.commands().pop(),
            Ok(SoundCmd {
                sound: SoundId(3),
                origin: TimeUs(2_495_679)
            })
        );
    }
}
