//! What the window and the system tell the game thread: system controls,
//! focus, and closing. Lock-free, so the game thread reads it each step.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use game::Command;
use input::FocusLatch;

/// The shared state between the window (main thread), the power
/// notification and the game thread.
#[derive(Debug, Default)]
pub struct Controls {
    /// The pending command: 0 none, else [`code`] of a [`Command`].
    command: AtomicU8,
    closing: AtomicBool,
    focused: AtomicBool,
    latch: OnceLock<FocusLatch>,
}

const fn code(c: Command) -> u8 {
    match c {
        Command::Retry => 1,
        Command::Quit => 2,
        Command::Suspend => 3,
    }
}

impl Controls {
    /// Controls with the window focused and nothing pending.
    #[must_use]
    pub fn new() -> Self {
        let c = Self::default();
        c.focused.store(true, Ordering::Release);
        c
    }

    /// Hands over the input side's focus latch, once.
    pub fn set_latch(&self, latch: FocusLatch) {
        let _ = self.latch.set(latch);
    }

    /// A system control or the system's suspend. A suspend is never
    /// replaced by a later control, so it always aborts the play.
    pub fn command(&self, c: Command) {
        let new = code(c);
        let _ = self
            .command
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (old != code(Command::Suspend)).then_some(new)
            });
    }

    /// Takes the pending command.
    pub fn take(&self) -> Option<Command> {
        match self.command.swap(0, Ordering::AcqRel) {
            1 => Some(Command::Retry),
            2 => Some(Command::Quit),
            3 => Some(Command::Suspend),
            _ => None,
        }
    }

    /// Drops a pending command, at the start of a play.
    pub fn clear(&self) {
        self.command.store(0, Ordering::Release);
    }

    /// The window was asked to close: the game quits.
    pub fn close(&self) {
        self.closing.store(true, Ordering::Release);
        self.command(Command::Quit);
    }

    /// Whether the window was asked to close.
    pub fn closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// The window gained or lost focus; a loss sets the input side's
    /// latch (D6).
    pub fn focus(&self, focused: bool) {
        self.focused.store(focused, Ordering::Release);
        if !focused && let Some(l) = self.latch.get() {
            l.latch();
        }
    }

    /// Whether the window has focus.
    pub fn focused(&self) -> bool {
        self.focused.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use game::Command;

    use super::Controls;

    #[test]
    fn suspend_is_never_replaced_and_take_empties() {
        let c = Controls::new();
        assert_eq!(c.take(), None);
        c.command(Command::Retry);
        c.command(Command::Quit);
        assert_eq!(c.take(), Some(Command::Quit));
        assert_eq!(c.take(), None);
        c.command(Command::Suspend);
        c.command(Command::Retry);
        assert_eq!(c.take(), Some(Command::Suspend));
        c.command(Command::Retry);
        c.clear();
        assert_eq!(c.take(), None);
    }
}
