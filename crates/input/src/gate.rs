//! The input side of the focus policy: losing focus aborts the play, and
//! nothing received after that is recorded.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eval::ControlId;

/// A device as the adapter identifies it, for example by its OS handle.
/// It never enters the log; sources get anonymous ids instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(pub u64);

/// One control change as received from a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet {
    /// The device.
    pub device: DeviceId,
    /// The control.
    pub control: ControlId,
    /// Its new value; for a button, nonzero is pressed.
    pub value: i32,
    /// Received while the game was in the background.
    pub background: bool,
}

/// What the gate decided for a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    /// The packet may be recorded.
    Record,
    /// The packet is not recorded: it was received in the background, or
    /// the gate has tripped.
    Reject,
}

/// Sets the latch from another thread, such as the window's focus-loss
/// notification.
#[derive(Clone, Debug)]
pub struct FocusLatch(Arc<AtomicBool>);

impl FocusLatch {
    /// Sets the latch: the gate trips at its next check.
    pub fn latch(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The input thread's gate. The latch is checked before each batch, a
/// background packet is rejected and trips it, and once it has tripped
/// nothing is admitted.
#[derive(Debug)]
pub struct FocusGate {
    latch: Arc<AtomicBool>,
    tripped: bool,
}

impl Default for FocusGate {
    fn default() -> Self {
        Self::new()
    }
}

impl FocusGate {
    /// An open gate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            latch: Arc::new(AtomicBool::new(false)),
            tripped: false,
        }
    }

    /// A handle that sets the latch from another thread.
    #[must_use]
    pub fn handle(&self) -> FocusLatch {
        FocusLatch(Arc::clone(&self.latch))
    }

    /// Sets the latch.
    pub fn latch(&self) {
        self.latch.store(true, Ordering::Release);
    }

    /// The check before a batch: whether the batch may be recorded. Trips
    /// the gate if the latch is set.
    pub fn check(&mut self) -> bool {
        if self.latch.load(Ordering::Acquire) {
            self.tripped = true;
        }
        !self.tripped
    }

    /// Decides one packet. A background packet trips the gate, which
    /// catches focus changing between [`check`](Self::check) and the batch.
    pub fn admit(&mut self, packet: &Packet) -> Admit {
        if packet.background {
            self.latch();
            self.tripped = true;
        }
        if self.tripped {
            Admit::Reject
        } else {
            Admit::Record
        }
    }

    /// Whether the gate has tripped; the play is then aborted.
    #[must_use]
    pub const fn is_tripped(&self) -> bool {
        self.tripped
    }

    /// Opens the gate again for the next play.
    pub fn reset(&mut self) {
        self.latch.store(false, Ordering::Release);
        self.tripped = false;
    }
}

#[cfg(test)]
mod tests {
    use eval::ControlId;

    use super::{Admit, DeviceId, FocusGate, Packet};

    #[test]
    fn latch_from_another_thread_trips_at_the_next_check_until_reset() {
        let mut gate = FocusGate::new();
        let packet = Packet {
            device: DeviceId(2),
            control: ControlId(9),
            value: 1,
            background: false,
        };
        let handle = gate.handle();
        std::thread::spawn(move || handle.latch()).join().unwrap();
        // The latch is read at the check before a batch, not per packet.
        assert_eq!(gate.admit(&packet), Admit::Record);
        assert!(!gate.check());
        assert_eq!(gate.admit(&packet), Admit::Reject);
        gate.reset();
        assert!(gate.check());
        assert_eq!(gate.admit(&packet), Admit::Record);
    }
}
