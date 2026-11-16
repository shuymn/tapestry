//! The boot counter (docs/test-device.md "Recovery"): three consecutive
//! watchdog boots send the board to BOOTSEL, so a firmware that hangs at
//! every boot still reaches the drive without the button.

/// Consecutive watchdog boots that reboot into BOOTSEL.
pub const BOOTSEL_AFTER: u8 = 3;
/// Healthy running, with a `hello` answered, that clears the counter.
pub const HEALTHY_US: u64 = 10_000_000;

/// Why the chip booted, from the watchdog's reason register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// Power-on, the RUN pin, or any reset the watchdog did not make.
    PowerOn,
    /// The firmware rebooted itself on purpose (a mode change).
    Forced,
    /// The watchdog timer fired: the firmware hung.
    Watchdog,
}

impl Cause {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::PowerOn => "power-on",
            Self::Forced => "forced",
            Self::Watchdog => "watchdog",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        [Self::PowerOn, Self::Forced, Self::Watchdog]
            .into_iter()
            .find(|c| c.name() == s)
    }
}

/// What the firmware does after reading the counter at boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Run,
    /// Clear the counter and reboot into BOOTSEL.
    Bootsel,
}

/// The counter's tag: a word without it counts as zero.
const TAG: u32 = 0xB007_0000;

/// The scratch word for `count`.
#[must_use]
pub const fn word(count: u8) -> u32 {
    TAG | count as u32
}

/// The count a scratch word holds.
#[must_use]
pub const fn count(word: u32) -> u8 {
    if word & 0xFFFF_FF00 == TAG {
        (word & 0xFF) as u8
    } else {
        0
    }
}

/// The counter after a boot for `cause` with `word` in its register: the
/// new count and what to do.
#[must_use]
pub const fn on_boot(cause: Cause, word: u32) -> (u8, Action) {
    match cause {
        Cause::Watchdog => {
            let n = count(word).saturating_add(1);
            if n >= BOOTSEL_AFTER {
                (0, Action::Bootsel)
            } else {
                (n, Action::Run)
            }
        }
        Cause::PowerOn | Cause::Forced => (0, Action::Run),
    }
}

/// Whether the counter clears: the firmware has run [`HEALTHY_US`] and
/// answered a `hello`.
#[must_use]
pub const fn healthy(uptime_us: u64, hello_answered: bool) -> bool {
    uptime_us >= HEALTHY_US && hello_answered
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boots `causes` in order, clearing the counter after each healthy
    /// run, and returns the actions.
    fn boots(steps: &[(Cause, bool)]) -> Vec<Action> {
        let mut w = 0;
        let mut out = Vec::new();
        for &(cause, healthy_run) in steps {
            let (n, action) = on_boot(cause, w);
            w = word(n);
            if healthy(
                if healthy_run {
                    HEALTHY_US
                } else {
                    HEALTHY_US - 1
                },
                true,
            ) {
                w = word(0);
            }
            out.push(action);
        }
        out
    }

    #[test]
    fn three_watchdog_boots_in_a_row_reach_bootsel() {
        let hang = (Cause::Watchdog, false);
        assert_eq!(
            boots(&[hang, hang, hang]),
            [Action::Run, Action::Run, Action::Bootsel]
        );
    }

    /// Two hangs with a healthy run between them, then one more: the
    /// counter cleared, so no BOOTSEL.
    #[test]
    fn a_healthy_run_clears_the_counter() {
        let hang = (Cause::Watchdog, false);
        let well = (Cause::Watchdog, true);
        assert_eq!(boots(&[hang, well, hang, hang]), [Action::Run; 4]);
    }

    #[test]
    fn other_boots_clear_the_counter() {
        for cause in [Cause::PowerOn, Cause::Forced] {
            assert_eq!(on_boot(cause, word(2)), (0, Action::Run));
        }
        assert_eq!(count(0xDEAD_BEEF), 0);
        assert!(!healthy(HEALTHY_US, false));
    }
}
