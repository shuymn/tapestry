//! The render thread's turntable (docs/stage3.md, offset 1; design.md
//! "Rendering and Skins", loading state): the play state's clock
//! ([`game::TurntableClock`]) and autoplay's scratch presses
//! ([`game::AutoplayScratch`]), ticked once per presented frame of the
//! play state, the practice page, loading and play alike.
//!
//! - **Play states**: the game thread names a play state when it creates
//!   it ([`RenderMsg::PlayState`](crate::render_side::RenderMsg)); the
//!   render thread adopts it when its presentation begins (the practice
//!   page, a loading lifetime or an attempt's preparation), so no frame of
//!   the previous one sets its baseline. Another number starts the clock
//!   over; nothing else does.
//! - **Pause and resume**: by number, whatever is presented.
//! - **Frames**: each presented frame draws the angle the clock holds,
//!   then updates autoplay at the frame's chart and state times (a play
//!   frame), reads the direction the devices and autoplay give at the
//!   frame's time, and ticks.
//!
//! Nothing here allocates after an attempt's notes are handed over.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;
use game::{AutoplayScratch, PlayState, ScratchInput, ScratchNote, TurntableClock};

/// The turntable of the render thread.
#[derive(Debug, Default)]
pub(crate) struct Turntable {
    clock: TurntableClock,
    /// The play state the game created, adopted when its presentation
    /// begins.
    pending: Option<PlayState>,
    /// The adopted play state's origin on the shared clock.
    origin: TimeUs,
    autoplay: AutoplayScratch,
    /// The committed attempt's scratch notes, for autoplay; none for a
    /// play by hand.
    notes: Vec<ScratchNote>,
}

impl Turntable {
    /// The game created play state `state`: adopted at its presentation's
    /// beginning ([`begin_presentation`](Self::begin_presentation)).
    pub(crate) const fn created(&mut self, state: PlayState) {
        self.pending = Some(state);
    }

    /// A presentation begins (the practice page, a loading lifetime, an
    /// attempt's preparation): a play state created since is adopted.
    pub(crate) fn begin_presentation(&mut self) {
        if let Some(s) = self.pending.take()
            && self.clock.number() != Some(s.number)
        {
            self.clock.adopt(s.number);
            self.origin = s.origin;
        }
    }

    /// Play state `number`'s pause.
    pub(crate) const fn pause(&mut self, number: u64) {
        self.clock.pause(number);
    }

    /// Play state `number`'s resume.
    pub(crate) const fn resume(&mut self, number: u64) {
        self.clock.resume(number);
    }

    /// The committed attempt's scratch notes: autoplay's presses start
    /// over.
    pub(crate) fn attempt(&mut self, notes: Vec<ScratchNote>) {
        self.notes = notes;
        self.autoplay.reset();
    }

    /// A play starts, a round or a rearmed READY candidate: autoplay's
    /// presses start over, as its judge's do.
    pub(crate) const fn play_starts(&mut self) {
        self.autoplay.reset();
    }

    /// The angle a frame draws.
    pub(crate) fn angle(&self) -> i32 {
        self.clock.angle()
    }

    /// A presented frame of the play state at `at` on the shared clock,
    /// at chart time `chart` when it is a play frame, with the devices'
    /// `scratch`: autoplay updates first, then the clock ticks.
    pub(crate) fn presented(&mut self, at: TimeUs, chart: Option<TimeUs>, scratch: ScratchInput) {
        let auto = match chart {
            Some(t) => {
                let ms = game::state_ms(at, self.origin);
                self.autoplay.update(&self.notes, t, ms);
                self.autoplay.pressed()
            }
            None => [false; 2],
        };
        self.clock.tick(at, scratch.direction(at, auto));
    }
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;
    use game::{PlayState, ScratchInput, ScratchNote, ScratchNoteKind};

    use super::Turntable;

    const MS: i64 = 1_000;

    fn state(number: u64, origin_ms: i64) -> PlayState {
        PlayState {
            number,
            origin: TimeUs(origin_ms * MS),
        }
    }

    fn idle(t: &mut Turntable, from: i64, to: i64) {
        for ms in (from..=to).step_by(10) {
            t.presented(TimeUs(ms * MS), None, ScratchInput::NONE);
        }
    }

    /// Degrees drawn, within [0, 360).
    fn degrees(t: &Turntable) -> i64 {
        i64::from(t.angle()).rem_euclid(360 * 6_000) / 6_000
    }

    /// A play state created before frames of the previous one: those
    /// frames tick the previous one, and the new one counts from its
    /// presentation's first frame.
    #[test]
    fn adoption_waits_for_the_presentation() {
        let mut t = Turntable::default();
        t.created(state(1, 0));
        t.begin_presentation();
        idle(&mut t, 0, 600);
        assert_eq!(degrees(&t), 100);
        // A retry: created, then old frames, then its preparation.
        t.created(state(2, 700));
        idle(&mut t, 700, 900);
        t.begin_presentation();
        assert_eq!(degrees(&t), 0);
        // Its first presented frame, 500 ms after creation, adds 0.
        t.presented(TimeUs(1_200 * MS), None, ScratchInput::NONE);
        assert_eq!(degrees(&t), 0);
        idle(&mut t, 1_210, 1_800);
        assert_eq!(degrees(&t), 100);
    }

    /// The practice page's time before Start counts; a practice
    /// round's result pauses the clock and the page resumes it from the
    /// held angle with a fresh baseline, even with no result frame.
    #[test]
    fn practice_rounds_keep_the_phase_paused_across_results() {
        let mut t = Turntable::default();
        t.created(state(1, 0));
        t.begin_presentation();
        // 600 ms on the page.
        idle(&mut t, 0, 600);
        // Start: its loading begins no new play state.
        t.begin_presentation();
        assert_eq!(degrees(&t), 100);
        idle(&mut t, 610, 1_200);
        let held = t.angle();
        // The round goes on to its result: paused, nothing presented for
        // 5 s, resumed on the page.
        t.pause(1);
        t.presented(TimeUs(3_000 * MS), None, ScratchInput::NONE);
        assert_eq!(t.angle(), held);
        t.resume(1);
        t.presented(TimeUs(6_200 * MS), None, ScratchInput::NONE);
        assert_eq!(t.angle(), held, "the first page frame adds 0");
        t.presented(TimeUs(6_210 * MS), None, ScratchInput::NONE);
        assert_eq!(t.angle(), held + 10 * 1_000, "then its own interval");
    }

    #[test]
    fn signals_of_another_number_are_ignored() {
        let mut t = Turntable::default();
        t.created(state(2, 0));
        t.begin_presentation();
        idle(&mut t, 0, 100);
        t.pause(1);
        idle(&mut t, 110, 200);
        assert_eq!(t.angle(), 200 * 1_000);
        t.pause(2);
        t.resume(1);
        idle(&mut t, 210, 300);
        assert_eq!(t.angle(), 200 * 1_000);
    }

    /// Autoplay presses on play frames only: a tap turns Positive and,
    /// with the device's Negative held, Negative wins.
    #[test]
    fn autoplay_joins_the_devices_on_play_frames() {
        let mut t = Turntable::default();
        t.created(state(1, 0));
        t.begin_presentation();
        t.attempt(vec![ScratchNote {
            time: TimeUs(1_000 * MS),
            kind: ScratchNoteKind::Tap,
        }]);
        t.play_starts();
        let play = |t: &mut Turntable, ms: i64, scratch| {
            t.presented(TimeUs(ms * MS), Some(TimeUs(ms * MS)), scratch);
        };
        play(&mut t, 990, ScratchInput::NONE);
        let before = t.angle();
        // 990 → 1 000: the tap crossed at 1 000 turns that tick Positive.
        play(&mut t, 1_000, ScratchInput::NONE);
        assert_eq!(t.angle() - before, 3 * 10 * 1_000);
        let negative = ScratchInput {
            negative: true,
            ..ScratchInput::NONE
        };
        let before = t.angle();
        play(&mut t, 1_010, negative);
        assert_eq!(t.angle() - before, -10 * 1_000);
    }
}
