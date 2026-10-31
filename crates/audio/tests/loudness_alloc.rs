//! The loudness analysis's memory is fixed when it starts, whatever the
//! chart's duration (design.md "Algorithms"): analyzing one minute and ten
//! minutes allocates the same bytes, the meter included, all of it in
//! `Analysis::new`, and nothing while stepping. The counting allocator
//! sums every tracked thread, so this file holds a single test.

use std::sync::Arc;

use audio::{Analysis, Sound, SoundBank, Step};
use bms_model::{BgmEvent, SoundId, TimeUs};

mod counting;

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

/// What analyzing `minutes` of the schedule allocated on this thread:
/// bytes and allocations in all, and allocations while stepping, with the
/// gating blocks it metered.
fn analyze(bank: &Arc<SoundBank>, minutes: i64) -> (u64, u64, u64, u64) {
    // A sound every 500 ms, alternating two ids so each plays out: the
    // program is above the gates throughout.
    let mut events = (0..minutes * 120).map(|k| BgmEvent {
        time: TimeUs(k * 500_000),
        sound: SoundId(u16::try_from(k % 2).unwrap()),
    });
    let (bytes, allocs) = (counting::allocated_bytes(), counting::allocations());
    let mut analysis = Analysis::new(Arc::clone(bank)).unwrap();
    let after_new = counting::allocations();
    let mut frames = 0;
    loop {
        match analysis.step(&mut events) {
            Ok(Step::More) => {}
            Ok(Step::Done(m)) => {
                frames = m.frames;
                break;
            }
            Err(_) => break,
        }
    }
    let stepping = counting::allocations() - after_new;
    drop(analysis);
    (
        counting::allocated_bytes() - bytes,
        counting::allocations() - allocs,
        stepping,
        frames / 800,
    )
}

#[test]
fn analysis_memory_is_independent_of_duration() {
    const RATE: u32 = 8_000;
    let tone = |amp: f64| {
        Sound::mono(
            (0..RATE as usize * 3 / 5)
                .map(|i| ((i as f64 * 0.37).sin() * amp) as i16)
                .collect(),
        )
    };
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![tone(12_000.0), tone(9_000.0)],
    ));

    counting::track(true);
    counting::self_check();
    let one = analyze(&bank, 1);
    let ten = analyze(&bank, 10);
    counting::track(false);

    let (bytes, allocs, stepping, gates) = one;
    assert!(bytes > 0 && allocs > 0, "{one:?}");
    assert_eq!(stepping, 0, "{one:?}");
    assert!(gates >= 600, "{one:?}");
    // Ten times the gating blocks (over 5 000), not one more byte.
    assert!(ten.3 > 5_000, "{ten:?}");
    assert_eq!((ten.0, ten.1, ten.2), (bytes, allocs, 0), "{one:?} {ten:?}");
}
