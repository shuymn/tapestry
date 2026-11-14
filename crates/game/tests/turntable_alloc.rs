//! The turntable's per-frame work allocates nothing (design.md "Scope of
//! No Allocation"; "Autoplay", Turntable): autoplay's presses updated over
//! taps, long notes and charge ends, with repeated times, a step back and
//! resets, read with the devices' directions, and the clock's ticks. This
//! file holds a single test, so the harness's other threads are not
//! tracked.

mod counting;

use bms_model::TimeUs;
use game::{AutoplayScratch, ScratchInput, ScratchNote, ScratchNoteKind, TurntableClock};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

const MS: i64 = 1_000;

#[test]
fn turntable_frames_allocate_nothing() {
    let mut notes = Vec::new();
    for i in 0..50 {
        let at = 1_000 + i * 400;
        let charge = i % 2 == 0;
        notes.push(ScratchNote {
            time: TimeUs(at * MS),
            kind: ScratchNoteKind::Tap,
        });
        notes.push(ScratchNote {
            time: TimeUs((at + 100) * MS),
            kind: ScratchNoteKind::LongStart {
                end: TimeUs((at + 250) * MS),
                charge,
            },
        });
        notes.push(ScratchNote {
            time: TimeUs((at + 250) * MS),
            kind: ScratchNoteKind::LongEnd { charge },
        });
    }
    let negative = ScratchInput {
        negative: true,
        ..ScratchInput::NONE
    };
    let mut auto = AutoplayScratch::NEW;
    let mut clock = TurntableClock::default();
    clock.adopt(1);

    counting::track(true);
    counting::self_check();
    let before = counting::calls();
    let mut turned = 0_u32;
    for round in 0..3_i64 {
        auto.reset();
        for ms in (0..21_000).step_by(7) {
            let scratch = if ms % 300 < 50 {
                negative
            } else {
                ScratchInput::NONE
            };
            auto.update(&notes, TimeUs(ms * MS));
            // The same chart time again changes nothing.
            auto.update(&notes, TimeUs(ms * MS));
            let pressed = auto.pressed();
            let [positive, negative] = pressed;
            turned += u32::from(positive || negative);
            let now = TimeUs((ms + 1_000 + round * 30_000) * MS);
            clock.tick(now, scratch.direction(now, pressed));
        }
        // A step back scans the notes again.
        auto.update(&notes, TimeUs(5_000 * MS));
        auto.update(&notes, TimeUs(1_100 * MS));
        std::hint::black_box(auto.pressed());
    }
    let calls = counting::calls() - before;
    counting::track(false);
    assert!(turned > 0, "autoplay pressed nothing");
    std::hint::black_box(clock.angle());
    assert_eq!(calls, 0);
}
