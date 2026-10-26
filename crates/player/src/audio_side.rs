//! The audio thread (D8): it opens the output on itself (WASAPI objects
//! stay on the thread that made them), waits for the engine built from the
//! bank at the output's rate, then runs `game`'s audio loop until the end
//! of the run.

use std::sync::mpsc::{self, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;

use audio::{AudioEngine, AudioOutput};
use game::{AudioControl, AudioProbe, AudioWorker, audio_control};

/// The output's sizes, sent once it is open.
#[derive(Clone, Copy, Debug)]
pub struct Opened {
    /// Frames per second.
    pub rate: u32,
    /// Frames per engine pass.
    pub period: u32,
    /// Periods the output keeps queued, when it says.
    pub queued_periods: Option<u32>,
    /// The most periods its queue can hold, when it says.
    pub most_queued_periods: Option<u32>,
}

/// The audio thread before its engine arrives.
pub struct AudioThread {
    worker: SyncSender<AudioWorker>,
    handle: JoinHandle<()>,
}

/// The running audio thread and its control.
pub struct AudioLink {
    /// The play control.
    pub control: AudioControl,
    handle: JoinHandle<()>,
}

impl AudioThread {
    /// Hands the engine to the thread, which starts stepping it, stopped
    /// until the first play; each key sound's latency goes to `probe`.
    pub fn start(self, engine: AudioEngine, probe: rtrb::Producer<AudioProbe>) -> AudioLink {
        let (control, mut worker) = audio_control(engine);
        worker.set_probe(probe);
        let _ = self.worker.send(worker);
        AudioLink {
            control,
            handle: self.handle,
        }
    }
}

impl AudioLink {
    /// Ends the thread and waits for it.
    pub fn exit(mut self) {
        self.control.exit();
        let _ = self.handle.join();
    }
}

/// Starts the audio thread: `open` makes the output on it, with the
/// periods it keeps queued and the most it can, if it says. `done` sees
/// the output when the thread ends.
///
/// # Errors
///
/// What `open` returned, or a message if the thread could not start.
pub fn spawn<O, F, D>(open: F, done: D) -> Result<(AudioThread, Opened), String>
where
    O: AudioOutput,
    F: FnOnce() -> Result<(O, Option<u32>, Option<u32>), String> + Send + 'static,
    D: FnOnce(&O) + Send + 'static,
{
    let (worker_tx, worker_rx) = mpsc::sync_channel::<AudioWorker>(1);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let handle = std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            #[cfg(feature = "dev-tools")]
            crate::alloc_count::tag(crate::alloc_count::Tag::Audio);
            let (mut out, queued_periods, most_queued_periods) = match open() {
                Ok(out) => out,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(Opened {
                rate: out.rate(),
                period: out.period(),
                queued_periods,
                most_queued_periods,
            }));
            if let Ok(mut worker) = worker_rx.recv() {
                worker.run(&mut out, Duration::from_millis(10));
            }
            done(&out);
        })
        .map_err(|e| format!("the audio thread cannot start: {e}"))?;
    match ready_rx.recv() {
        Ok(Ok(opened)) => Ok((
            AudioThread {
                worker: worker_tx,
                handle,
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
