//! Debug faults (docs/test-device.md "Debug faults"): the firmware's own
//! mutations for negative rows, compiled into its `debug-faults` feature
//! only. Each clears itself [`LIFETIME_US`] after it is set, so no negative
//! row can leave the board stuck; the boot hang clears itself after its
//! count of boots, and the clock offset (which breaks nothing) lasts until
//! the next reboot.

/// How long a fault stays on.
pub const LIFETIME_US: u64 = 30_000_000;

/// A fault the firmware can be told to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Fault {
    /// Report the handoff as the completion (F-S's negative row).
    CompletionAtWrite,
    /// Skip every other poll in continuous reporting (F2, U5).
    EveryOtherPoll,
    /// Report the other rate's mode byte without changing `bInterval` (F3).
    ModeByteOnly,
    /// Ignore every command line (F5a).
    ParserOff,
    /// Also ignore the 1200-baud line coding (F5a's negative row).
    BaudOff,
    /// Spin in the USB task without yielding, so the watchdog fires (F5b).
    Hang,
    /// Run the watchdog at 8 s instead of 2 s (F5b's negative row).
    SlowWatchdog,
    /// Hang at each of the next `arg` boots before feeding the watchdog
    /// (F5c).
    BootHang,
    /// Keep the held state and the pattern through a disarm (L1).
    IgnoreDisarm,
    /// Enter the suspend handler as if the bus had suspended (L3).
    Suspend,
    /// Keep the pattern armed across a detach (L4).
    KeepArmed,
    /// Never send pattern item `arg` (U1).
    DropItem,
    /// Send each change of an item as a report of its own (U2).
    Split,
    /// Start the reported µs 10 s before 2³² (F4).
    ClockOffset,
    /// Shift each pong's µs by `arg` (signed) µs (F4's negative row).
    PongOffset,
}

const ALL: [Fault; 15] = [
    Fault::CompletionAtWrite,
    Fault::EveryOtherPoll,
    Fault::ModeByteOnly,
    Fault::ParserOff,
    Fault::BaudOff,
    Fault::Hang,
    Fault::SlowWatchdog,
    Fault::BootHang,
    Fault::IgnoreDisarm,
    Fault::Suspend,
    Fault::KeepArmed,
    Fault::DropItem,
    Fault::Split,
    Fault::ClockOffset,
    Fault::PongOffset,
];

impl Fault {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CompletionAtWrite => "c-at-write",
            Self::EveryOtherPoll => "every-other-poll",
            Self::ModeByteOnly => "mode-byte-only",
            Self::ParserOff => "parser-off",
            Self::BaudOff => "baud-off",
            Self::Hang => "hang",
            Self::SlowWatchdog => "watchdog-8s",
            Self::BootHang => "boot-hang",
            Self::IgnoreDisarm => "ignore-disarm",
            Self::Suspend => "suspend",
            Self::KeepArmed => "keep-armed",
            Self::DropItem => "drop-item",
            Self::Split => "split",
            Self::ClockOffset => "clock-offset",
            Self::PongOffset => "pong-offset",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        ALL.into_iter().find(|f| f.name() == s)
    }

    /// Whether the fault takes an argument.
    #[must_use]
    pub const fn takes_arg(self) -> bool {
        matches!(self, Self::BootHang | Self::DropItem | Self::PongOffset)
    }
}

/// The faults that are on, each until its deadline.
#[derive(Clone, Copy, Debug)]
pub struct Faults {
    until: [u64; ALL.len()],
    arg: [i64; ALL.len()],
}

impl Default for Faults {
    fn default() -> Self {
        Self::new()
    }
}

impl Faults {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            until: [0; ALL.len()],
            arg: [0; ALL.len()],
        }
    }

    /// Turns `fault` on at `now` with `arg`.
    pub fn set(&mut self, fault: Fault, arg: i64, now: u64) {
        let i = fault as usize;
        let until = match fault {
            Fault::ClockOffset => u64::MAX,
            _ => now.saturating_add(LIFETIME_US),
        };
        if let (Some(u), Some(a)) = (self.until.get_mut(i), self.arg.get_mut(i)) {
            *u = until;
            *a = arg;
        }
    }

    /// `fault`'s argument while it is on at `now`.
    #[must_use]
    pub fn on(&self, fault: Fault, now: u64) -> Option<i64> {
        let i = fault as usize;
        match (self.until.get(i), self.arg.get(i)) {
            (Some(&until), Some(&arg)) if now < until => Some(arg),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fault_clears_itself_after_its_lifetime() {
        let mut f = Faults::new();
        assert_eq!(f.on(Fault::Hang, 0), None);
        f.set(Fault::DropItem, 37, 1_000);
        assert_eq!(f.on(Fault::DropItem, 1_000), Some(37));
        assert_eq!(f.on(Fault::DropItem, 1_000 + LIFETIME_US - 1), Some(37));
        assert_eq!(f.on(Fault::DropItem, 1_000 + LIFETIME_US), None);
        f.set(Fault::ClockOffset, 0, 5);
        assert_eq!(
            f.on(Fault::ClockOffset, u64::MAX - 1),
            Some(0),
            "until reboot"
        );
    }

    #[test]
    fn every_name_parses_back() {
        for f in ALL {
            assert_eq!(Fault::parse(f.name()), Some(f));
        }
    }
}
