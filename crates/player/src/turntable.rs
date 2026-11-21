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
//!   then updates autoplay at the frame's chart time (a play frame),
//!   reads the direction the devices and autoplay give at the frame's
//!   time, and ticks.
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
    autoplay: AutoplayScratch,
    /// The committed attempt's scratch notes, for autoplay; none for a
    /// play by hand.
    notes: Vec<ScratchNote>,
    /// A resume the game sent, applied when the next presentation begins
    /// (the practice page, or a practice retry's next round at its
    /// attempt's preparation), so no frame drawn before it (an old play
    /// frame after a withdrawn result) takes the fresh baseline.
    resume: Option<u64>,
}

impl Turntable {
    /// The game created play state `state`: adopted at its presentation's
    /// beginning ([`begin_presentation`](Self::begin_presentation)).
    pub(crate) const fn created(&mut self, state: PlayState) {
        self.pending = Some(state);
    }

    /// A presentation begins (the practice page, a loading lifetime, an
    /// attempt's preparation): a play state created since is adopted, and
    /// a resume the game sent applies now, the presentation's first frame
    /// its baseline (a resume of a play state replaced meanwhile is
    /// ignored).
    pub(crate) fn begin_presentation(&mut self) {
        if let Some(s) = self.pending.take()
            && self.clock.number() != Some(s.number)
        {
            self.clock.adopt(s.number);
        }
        if let Some(n) = self.resume.take() {
            self.clock.resume(n);
        }
    }

    /// Play state `number`'s pause.
    pub(crate) const fn pause(&mut self, number: u64) {
        self.clock.pause(number);
    }

    /// Play state `number`'s resume, applied when the next presentation
    /// begins ([`begin_presentation`](Self::begin_presentation)); another
    /// number's is ignored and leaves a queued one.
    pub(crate) fn resume(&mut self, number: u64) {
        if self.clock.number() == Some(number) {
            self.resume = Some(number);
        }
    }

    /// The practice page shows: its play state's presentation begins, a
    /// resume the game sent applying with it.
    pub(crate) fn practice_page(&mut self) {
        self.begin_presentation();
    }

    /// The committed attempt's scratch notes: autoplay's presses start
    /// over.
    pub(crate) fn attempt(&mut self, notes: Vec<ScratchNote>) {
        self.notes = notes;
        self.autoplay.reset();
    }

    /// A play starts, a round or a rearmed READY candidate: autoplay's
    /// presses start over.
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
        // Autoplay presses from chart time 0: READY's frames (before it)
        // tick but press nothing.
        let auto = match chart {
            Some(t) if t.0 >= 0 => {
                self.autoplay.update(&self.notes, t);
                self.autoplay.pressed()
            }
            _ => [false; 2],
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
        // The resume comes before the page shows: old frames drawn
        // between (a withdrawn result's play frames) neither tick nor
        // take the baseline.
        t.resume(1);
        t.presented(
            TimeUs(5_000 * MS),
            Some(TimeUs(9_000 * MS)),
            ScratchInput::NONE,
        );
        t.presented(
            TimeUs(5_500 * MS),
            Some(TimeUs(9_500 * MS)),
            ScratchInput::NONE,
        );
        assert_eq!(t.angle(), held, "old frames before the page");
        t.practice_page();
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
        t.practice_page();
        idle(&mut t, 210, 300);
        assert_eq!(t.angle(), 200 * 1_000);
        // A resume of another number after its own leaves its own queued.
        t.resume(2);
        t.resume(1);
        t.practice_page();
        t.presented(TimeUs(400 * MS), None, ScratchInput::NONE);
        t.presented(TimeUs(410 * MS), None, ScratchInput::NONE);
        assert_eq!(t.angle(), 210 * 1_000, "resumed");
    }

    /// READY's frames tick the clock but press nothing; a scratch tap at
    /// chart time 0 turns Positive from the play's first frame.
    #[test]
    fn ready_frames_tick_but_autoplay_waits_for_the_play() {
        let mut t = Turntable::default();
        t.created(state(1, 0));
        t.begin_presentation();
        t.attempt(vec![ScratchNote {
            time: TimeUs(0),
            kind: ScratchNoteKind::Tap,
        }]);
        t.play_starts();
        // READY at chart −20 and −10 ms, then the play at 0 and 10 ms.
        let mut turned = Vec::new();
        for chart in [-20, -10, 0, 10] {
            turned.push(play_tick(&mut t, chart * MS, 1_000_000));
        }
        assert_eq!(
            turned,
            [0, 10_000, 30_000, 30_000],
            "idle on READY's frames, Positive from chart time 0"
        );
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

    /// The tick a play frame at chart time `chart_ms` adds, the play
    /// state's origin `delta` µs before chart time 0.
    fn play_tick(t: &mut Turntable, chart_us: i64, delta: i64) -> i64 {
        let before = i64::from(t.angle());
        t.presented(
            TimeUs(chart_us + delta),
            Some(TimeUs(chart_us)),
            ScratchInput::NONE,
        );
        (i64::from(t.angle()) - before).rem_euclid(2_160_000)
    }

    /// Presses follow chart time, not the play state's clock: with the
    /// state origin 0 or 999.6 ms before chart 0, a CN end at chart
    /// 1 000 ms turns Negative to chart 1 080 ms, so a frame at 1 080.5 ms
    /// is idle and the LN from 1 082 ms turns Positive at 1 100 ms.
    #[test]
    fn autoplay_presses_by_chart_time_whatever_the_state_origin() {
        for delta in [0, 999_600] {
            let mut t = Turntable::default();
            t.created(state(1, 0));
            t.begin_presentation();
            let note = |ms: i64, kind| ScratchNote {
                time: TimeUs(ms * MS),
                kind,
            };
            t.attempt(vec![
                note(
                    500,
                    ScratchNoteKind::LongStart {
                        end: TimeUs(1_000 * MS),
                        charge: true,
                    },
                ),
                note(1_000, ScratchNoteKind::LongEnd { charge: true }),
                note(
                    1_082,
                    ScratchNoteKind::LongStart {
                        end: TimeUs(5_000 * MS),
                        charge: false,
                    },
                ),
                note(5_000, ScratchNoteKind::LongEnd { charge: false }),
            ]);
            t.play_starts();
            play_tick(&mut t, 1_000_000, delta);
            assert_eq!(play_tick(&mut t, 1_080_500, delta), 80_500, "delta {delta}");
            play_tick(&mut t, 1_090_000, delta);
            assert_eq!(play_tick(&mut t, 1_100_000, delta), 30_000, "delta {delta}");
        }
    }

    /// A play's start clears autoplay's presses, as its judge's init does.
    #[test]
    fn a_play_start_clears_autoplays_presses() {
        let mut t = Turntable::default();
        t.created(state(1, 0));
        t.begin_presentation();
        t.attempt(vec![ScratchNote {
            time: TimeUs(1_000 * MS),
            kind: ScratchNoteKind::Tap,
        }]);
        t.play_starts();
        play_tick(&mut t, 990_000, 0);
        assert_eq!(play_tick(&mut t, 1_000_000, 0), 30_000, "Positive");
        // A rearm or a round: chart time starts over, the press is gone.
        t.play_starts();
        assert_eq!(play_tick(&mut t, 10_000, 1_000_000), 10_000, "idle");
    }

    /// The turntable's per-frame work allocates nothing (design.md "Scope
    /// of No Allocation"): ticks with autoplay crossing taps, long notes and
    /// charge ends, rearms and rounds, a pause and a resume through the
    /// practice page, and a new play state's adoption.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn turntable_frames_alloc() {
        use crate::alloc_count::this_thread;
        let mut t = Turntable::default();
        t.created(state(1, 0));
        t.begin_presentation();
        let mut notes = Vec::new();
        for i in 0..50 {
            let at = 1_000 + i * 400;
            notes.push(ScratchNote {
                time: TimeUs(at * MS),
                kind: ScratchNoteKind::Tap,
            });
            notes.push(ScratchNote {
                time: TimeUs((at + 100) * MS),
                kind: ScratchNoteKind::LongStart {
                    end: TimeUs((at + 250) * MS),
                    charge: i % 2 == 0,
                },
            });
            notes.push(ScratchNote {
                time: TimeUs((at + 250) * MS),
                kind: ScratchNoteKind::LongEnd { charge: i % 2 == 0 },
            });
        }
        t.attempt(notes);
        let next = state(2, 0);
        let negative = ScratchInput {
            negative: true,
            ..ScratchInput::NONE
        };
        // The counter counts: an allocation before the measured interval
        // shows.
        let control = this_thread();
        std::hint::black_box(vec![0_u8; 64]);
        assert!(this_thread() > control, "the counting allocator counts");
        let before = this_thread();
        for round in 0..3 {
            t.play_starts();
            for ms in (-500..21_000).step_by(7) {
                let scratch = if ms % 300 < 50 {
                    negative
                } else {
                    ScratchInput::NONE
                };
                t.presented(TimeUs((ms + 1_000) * MS), Some(TimeUs(ms * MS)), scratch);
            }
            t.pause(1);
            t.presented(TimeUs(30_000 * MS), None, ScratchInput::NONE);
            t.resume(1);
            t.practice_page();
            t.presented(TimeUs((31_000 + round) * MS), None, ScratchInput::NONE);
        }
        t.created(next);
        t.begin_presentation();
        t.presented(TimeUs(40_000 * MS), None, ScratchInput::NONE);
        assert_eq!(this_thread(), before);
    }
}
