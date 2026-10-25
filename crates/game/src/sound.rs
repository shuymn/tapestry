//! What the logic thread hands to audio: key sounds through a queue to the
//! audio thread's [`AudioEngine`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ::audio::{AudioEngine, AudioOutput, OutputError, SoundCmd, StepReport};
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
}

impl EventSink for AudioSink {
    fn receive(&mut self, ev: &JudgeEvent) {
        let JudgeEventKind::PlaySound { sound, .. } = ev.kind else {
            return;
        };
        let cmd = SoundCmd {
            sound,
            // A key sound always comes from a press; the event time stands
            // in only if an origin is ever missing.
            origin: ev.origin.unwrap_or(ev.time),
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

    /// Sounds dropped because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// The link between logic and audio: a command queue holding `capacity`
/// sounds.
#[must_use]
pub fn audio_link(capacity: usize) -> (AudioSink, AudioQueue) {
    let (producer, consumer) = rtrb::RingBuffer::new(capacity);
    let dropped = Arc::new(AtomicU64::new(0));
    (
        AudioSink {
            cmds: producer,
            dropped: Arc::clone(&dropped),
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

    /// The output.
    #[must_use]
    pub fn output(&self) -> &O {
        &self.output
    }

    /// Runs every engine step whose buffer event is due by `now`.
    ///
    /// # Errors
    ///
    /// The output's failure.
    pub(crate) fn step(
        &mut self,
        now: TimeUs,
        queue: &mut AudioQueue,
    ) -> Result<StepReport, OutputError> {
        let mut last = StepReport::default();
        while self.output.next_wake() <= now {
            last = self.engine.step(&mut self.output, queue.commands())?;
        }
        Ok(last)
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
}
