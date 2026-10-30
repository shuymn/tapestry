//! A ring-out allocates nothing on the audio thread: taking the request,
//! placing it on the output, fading, stopping every voice at its end and
//! refusing later starts (design.md "Mid-Chart Start and Section Repeat",
//! "Scope of No Allocation").
//!
//! The counting allocator sums every thread that opted in, so this file
//! holds a single test.

use std::sync::Arc;

use audio::{AudioEngine, RecordingOutput, SimConfig, SimDevice, Sound, SoundBank, SoundCmd};
use bms_model::{BgmEvent, SoundId, TimeUs};
use rtrb::RingBuffer;

mod counting;

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

#[test]
fn ring_out_allocates_nothing() {
    const RATE: u32 = 48_000;
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![
            Sound::mono(vec![12_000; 5 * RATE as usize]),
            Sound::mono(vec![4_000; 4_800]),
        ],
    ));
    // A long voice, BGM every 50 ms across the fade and past the end.
    let mut bgm = vec![BgmEvent {
        time: TimeUs(0),
        sound: SoundId(0),
    }];
    bgm.extend((1..60).map(|k| BgmEvent {
        time: TimeUs(k * 50_000),
        sound: SoundId(1),
    }));
    let mut e = AudioEngine::new(bank, &bgm, RATE, 480, 8, 64).unwrap();
    let dev = SimDevice::new(SimConfig::steady(RATE, 480, TimeUs(0)));
    let mut out = RecordingOutput::new(dev, 0);
    let (mut tx, mut rx) = RingBuffer::<SoundCmd>::new(16);
    e.restart(TimeUs(20_000));

    counting::track(true);
    counting::self_check();
    let mut rung = false;
    let mut steps = 0;
    while out.inner().next_wake() < TimeUs(3_500_000) {
        if !rung && out.inner().next_wake() >= TimeUs(1_000_000) {
            e.ring_out(TimeUs(1_800_000), TimeUs(2_300_000));
            rung = true;
        }
        // A key sound every step, refused once the ring-out has ended.
        tx.push(SoundCmd {
            sound: SoundId(1),
            origin: out.inner().next_wake(),
        })
        .unwrap();
        e.step(&mut out, &mut rx).unwrap();
        e.drain_scheduled(|_| {});
        steps += 1;
    }
    counting::track(false);

    assert!(steps > 300, "{steps}");
    assert!(e.mixer().rung_out());
    assert_eq!(e.mixer().sounding(), 0);
    assert_eq!(e.bgm_left(), 0);
    assert_eq!(counting::allocations(), 0);
}
