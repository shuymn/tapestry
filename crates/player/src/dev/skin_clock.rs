//! The select skin's frozen clock (`--dev-freeze-skin-clock`): what the
//! skin's animated elements read, fixed for the whole run, the standard
//! technique for deterministic UI screenshots (a fixed virtual clock). A
//! test device's panel rows start the player with it, so their shots of the
//! select screen are the same frame in every run and are compared with a
//! pinned reference over the whole screen (docs/test-device.md
//! "Verification", P1).
//!
//! The skin reads time only from its frame: the frame's instant, its
//! timers' starts and the application's readouts. Frozen, every frame is
//! drawn at [`NOW`], each timer that is on started [`AGE_US`] before it
//! (each animation shows the same phase in every frame, every one-shot
//! transition finished), and the readouts are fixed ([`APP`]). Which timers
//! are on stays the screen's: a panel opens and closes as the input decides.
//! Input, audio, settings and the panels' logic keep the real clock.

use std::sync::atomic::{AtomicBool, Ordering};

use bms_model::TimeUs;
use skin_model::{AppReadouts, LocalTime, SelectionTimers};

/// The instant every frozen frame is drawn at.
pub const NOW: TimeUs = TimeUs(600_000_000);
/// How long before [`NOW`] every timer that is on started.
pub const AGE_US: i64 = 30_000_000;
/// The readouts every frozen frame shows: 60 presents, the uptime at
/// [`NOW`], 2001-02-03 04:05:06.
pub const APP: AppReadouts = AppReadouts {
    fps: 60,
    uptime_us: NOW.0,
    local: LocalTime {
        year: 2001,
        month: 2,
        day: 3,
        hour: 4,
        minute: 5,
        second: 6,
    },
};

static FROZEN: AtomicBool = AtomicBool::new(false);

/// Freezes the skin's clock for the rest of the run.
pub fn freeze() {
    FROZEN.store(true, Ordering::Release);
}

/// What the skin reads of a selection frame at `now` with `timers` and
/// `app`: these, or the frozen clock's. Allocates nothing.
#[must_use]
pub fn apply(
    now: TimeUs,
    timers: SelectionTimers,
    app: AppReadouts,
) -> (TimeUs, SelectionTimers, AppReadouts) {
    if FROZEN.load(Ordering::Acquire) {
        frozen(timers)
    } else {
        (now, timers, app)
    }
}

/// The frozen clock's frame of `timers`: each timer on started [`AGE_US`]
/// before [`NOW`], each one off stays off.
const fn frozen(timers: SelectionTimers) -> (TimeUs, SelectionTimers, AppReadouts) {
    const fn at(t: Option<TimeUs>) -> Option<TimeUs> {
        match t {
            Some(_) => Some(TimeUs(NOW.0 - AGE_US)),
            None => None,
        }
    }
    let [a, b, c] = timers.panels;
    (
        NOW,
        SelectionTimers {
            screen: at(timers.screen),
            change: at(timers.change),
            panels: [at(a), at(b), at(c)],
        },
        APP,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frozen, two frames of different instants, timers and readouts read
    /// the same instant, the same readouts, and each timer that is on at
    /// the same age, while which timers are on is the frame's own: an
    /// open panel stays open, a closed one closed.
    #[test]
    fn a_frozen_frame_keeps_which_timers_are_on_and_fixes_the_rest() {
        let started = |p: [Option<i64>; 3]| SelectionTimers {
            screen: Some(TimeUs(1_000)),
            change: Some(TimeUs(2_000)),
            panels: p.map(|t| t.map(TimeUs)),
        };
        let app = AppReadouts {
            fps: 144,
            uptime_us: 7_777,
            ..APP
        };
        let (now, t, a) = frozen(started([None, None, Some(5_000)]));
        assert_eq!(now, NOW);
        assert_eq!(a, APP);
        let aged = Some(TimeUs(NOW.0 - AGE_US));
        assert_eq!(
            t,
            SelectionTimers {
                screen: aged,
                change: aged,
                panels: [None, None, aged],
            }
        );
        let (now2, t2, _) = frozen(started([Some(9_000), None, None]));
        assert_eq!(now2, NOW);
        assert_eq!(t2.panels, [aged, None, None]);
        // Not frozen: the frame's own; frozen: the frozen frame's.
        let real = started([None, Some(3), None]);
        assert_eq!(apply(TimeUs(123), real, app), (TimeUs(123), real, app));
        freeze();
        let got = apply(TimeUs(123), real, app);
        // The process's other tests draw on the real clock (nextest runs
        // each test in a process of its own).
        FROZEN.store(false, Ordering::Release);
        assert_eq!(got, frozen(real));
        assert_ne!(got.0, TimeUs(123));
    }
}
