//! The worker's command writer (design.md "Threat Model" T02): a thread of
//! the host's own that alone writes the control pipe, so that no other
//! thread ever waits on a worker that stops reading its commands. Commands
//! reach it through a queue of a fixed capacity, which the sender never
//! waits on: a full queue or a writer that has ended is the sender's to
//! judge. The writer publishes when the write under way began and when each
//! command was written whole, so the supervisor can tell a stuck write from
//! a slow answer and time each answer from its own command's write.

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

/// The commands the queue holds.
pub const QUEUE: usize = 16;

/// The commands whose write times are kept: twice the queue, so a command
/// queued in one look of the supervisor's is still there at the next, which
/// keeps it ([`Transport::written`]).
const TIMES: usize = 2 * QUEUE;

/// A command's write time, as the writer publishes it.
#[derive(Debug, Default)]
struct Written {
    /// The command's id, 0 while its time is being stored.
    id: AtomicU32,
    /// When its write ended, µs after the origin.
    at: AtomicU64,
}

/// What the writer publishes of its writing.
#[derive(Debug)]
pub struct Transport {
    origin: Instant,
    /// When the write under way began, µs after `origin` plus one; 0 while
    /// none is under way.
    writing: AtomicU64,
    /// Each command written whole, in the slot of its id modulo [`TIMES`].
    written: [Written; TIMES],
    /// The newest command written whole.
    newest: AtomicU32,
    /// The writer has ended: a write failed (the worker ended) or the queue
    /// closed.
    ended: AtomicBool,
}

impl Transport {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            writing: AtomicU64::new(0),
            written: std::array::from_fn(|_| Written::default()),
            newest: AtomicU32::new(0),
            ended: AtomicBool::new(false),
        }
    }

    fn since_origin(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    /// How long the write under way has been waiting, `None` when no write
    /// is under way.
    pub fn stuck_for(&self) -> Option<Duration> {
        let began = self.writing.load(Ordering::Acquire).checked_sub(1)?;
        Some(Duration::from_micros(
            self.since_origin().saturating_sub(began),
        ))
    }

    /// When command `id` itself was written whole: `Ok(None)` until it has
    /// been, `Err(())` when its time is no longer kept (a later command took
    /// its slot), which a supervisor that keeps each time at its next look
    /// never meets.
    pub fn written(&self, id: u32) -> Result<Option<Instant>, ()> {
        let slot = self.slot(id);
        // The writer clears the id, stores the time, then the id: a time
        // read between two reads of `id` is that command's.
        if slot.id.load(Ordering::Acquire) == id {
            let at = slot.at.load(Ordering::Acquire);
            if slot.id.load(Ordering::Acquire) == id {
                return Ok(Some(self.origin + Duration::from_micros(at)));
            }
        }
        // Not in its slot: not written yet, unless a newer command was and
        // took the slot. Ids rise but wrap past 0, which no command has.
        let newest = self.newest.load(Ordering::Acquire);
        let behind = newest.wrapping_sub(id);
        if newest != 0 && behind < u32::MAX / 2 && behind as usize >= TIMES {
            return Err(());
        }
        Ok(None)
    }

    fn slot(&self, id: u32) -> &Written {
        let i = id as usize % TIMES;
        #[expect(
            clippy::indexing_slicing,
            reason = "an index modulo the array's length"
        )]
        &self.written[i]
    }

    /// Whether the writer has ended.
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
}

/// Why a command was not queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The queue is full: the writer is behind, its write perhaps stuck.
    Full,
    /// The writer has ended.
    Ended,
}

/// The sending end of a worker's command writer.
pub struct Writer {
    tx: SyncSender<(u32, Vec<u8>)>,
    transport: Arc<Transport>,
    /// The pipe the writer writes, shared so a test can write it too.
    #[cfg(test)]
    pipe: Arc<std::fs::File>,
}

impl Writer {
    /// Starts the writer of `pipe`, the game's end of the control pipe.
    ///
    /// # Errors
    ///
    /// When the thread does not start.
    pub fn start(pipe: std::fs::File) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let transport = Arc::new(Transport::new());
        let pipe = Arc::new(pipe);
        {
            let transport = Arc::clone(&transport);
            let pipe = Arc::clone(&pipe);
            std::thread::Builder::new()
                .name("media worker commands".to_owned())
                .spawn(move || write(&pipe, &rx, &transport))?;
        }
        Ok(Self {
            tx,
            transport,
            #[cfg(test)]
            pipe,
        })
    }

    /// Queues command `id`'s framed `bytes`, never waiting.
    ///
    /// # Errors
    ///
    /// When the queue is full or the writer has ended.
    pub fn send(&self, id: u32, bytes: Vec<u8>) -> Result<(), Refused> {
        if self.transport.ended() {
            return Err(Refused::Ended);
        }
        self.tx.try_send((id, bytes)).map_err(|e| match e {
            TrySendError::Full(_) => Refused::Full,
            TrySendError::Disconnected(_) => Refused::Ended,
        })
    }

    /// What the writer publishes.
    pub fn transport(&self) -> &Arc<Transport> {
        &self.transport
    }

    /// The pipe the writer writes.
    #[cfg(test)]
    pub fn pipe(&self) -> &std::fs::File {
        &self.pipe
    }
}

/// The writer's loop: each queued command written whole, until the queue
/// closes or a write fails.
fn write(mut pipe: &std::fs::File, rx: &Receiver<(u32, Vec<u8>)>, transport: &Transport) {
    while let Ok((id, bytes)) = rx.recv() {
        transport.writing.store(
            transport.since_origin().saturating_add(1),
            Ordering::Release,
        );
        let wrote = pipe.write_all(&bytes).and_then(|()| pipe.flush());
        transport.writing.store(0, Ordering::Release);
        if wrote.is_err() {
            break;
        }
        let slot = transport.slot(id);
        slot.id.store(0, Ordering::Release);
        slot.at.store(transport.since_origin(), Ordering::Release);
        slot.id.store(id, Ordering::Release);
        transport.newest.store(id, Ordering::Release);
    }
    transport.ended.store(true, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Waits up to a few seconds for command `id`'s write time.
    fn written(transport: &Transport, id: u32) -> Instant {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(Some(at)) = transport.written(id) {
                return at;
            }
            assert!(Instant::now() < deadline, "command {id} never written");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // Covers: T02
    #[test]
    fn each_command_keeps_its_own_write_time() {
        let dir = tempfile::tempdir().unwrap();
        let sink = std::fs::File::create(dir.path().join("pipe")).unwrap();
        let writer = Writer::start(sink).unwrap();
        let transport = Arc::clone(writer.transport());
        writer.send(1, vec![1; 8]).unwrap();
        let first = written(&transport, 1);
        // Not yet written: no time, rather than an older command's.
        assert_eq!(transport.written(2), Ok(None));
        std::thread::sleep(Duration::from_millis(50));
        writer.send(2, vec![2; 8]).unwrap();
        let second = written(&transport, 2);
        assert!(second >= first + Duration::from_millis(50));
        // The first command's time stays its own after the second's write.
        assert_eq!(transport.written(1), Ok(Some(first)));
        // Past the times kept, a command's time is gone, not another's.
        for id in 3..=u32::try_from(TIMES).unwrap() + 2 {
            writer.send(id, vec![0; 8]).unwrap();
            written(&transport, id);
        }
        assert_eq!(transport.written(1), Err(()));
    }
}
