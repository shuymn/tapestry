//! Once built, the engine allocates nothing per step: draining commands,
//! scheduling BGM, seeking, cutting a sound's earlier instance, stealing voices,
//! mixing, soft clipping and writing; nor does
//! the audio thread loop around it.
//!
//! The counting allocator counts only threads that opted in, so the test
//! harness's own threads do not disturb the count. This file holds a single
//! test for the same reason. Profiles and the positive control: design.md
//! "Scope of No Allocation".

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use audio::{
    AudioEngine, AudioOutput, OutputError, RecordingOutput, SHIFT_HISTORY, SimConfig, SimDevice,
    Sound, SoundBank, SoundCmd, Wake, run_audio,
};
use bms_model::{BgmEvent, SoundId, TimeUs};
use rtrb::RingBuffer;

mod counting;

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

/// Sets `stop` during its `left`th wait from now.
struct StopAfter<'a, O> {
    inner: &'a mut O,
    left: u32,
    stop: &'a AtomicBool,
}

impl<O: AudioOutput> AudioOutput for StopAfter<'_, O> {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }

    fn period(&self) -> u32 {
        self.inner.period()
    }

    fn wait(&mut self) -> Wake {
        self.left -= 1;
        if self.left == 0 {
            self.stop.store(true, Ordering::Release);
        }
        self.inner.wait()
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        self.inner.write(f)
    }
}

#[test]
fn mixing_does_not_allocate() {
    const RATE: u32 = 48_000;
    const STEPS: u32 = 3_000;
    // Key sounds 0 to 15 and BGM sound 16.
    const KEYS: u16 = 16;
    let mut sounds: Vec<Sound> = (0..KEYS)
        .map(|_| Sound::mono(vec![20_000; 9_000]))
        .collect();
    sounds.push(Sound::stereo(
        (0..4_000).map(|i| (i % 7_000) as i16 - 3_500).collect(),
    ));
    let bank = Arc::new(SoundBank::from_sounds(RATE, sounds));
    // BGM every 20 ms over the 30 s run, each start cutting the last.
    let bgm: Vec<BgmEvent> = (0..1_500)
        .map(|k| BgmEvent {
            time: TimeUs(k * 20_000),
            sound: SoundId(KEYS),
        })
        .collect();
    let mut e = AudioEngine::new(bank, &bgm, RATE, 480, 8, 64).unwrap();
    // 40 stalls of three missed wakes, two underruns each: more than the
    // simulator's shift history holds.
    let missed_wakes = (0..40u64)
        .flat_map(|k| [50 + 70 * k, 51 + 70 * k, 52 + 70 * k])
        .collect();
    let dev = SimDevice::new(SimConfig {
        missed_wakes,
        drift_ppm: 80,
        jitter_us: 1_500,
        seed: 11,
        ..SimConfig::steady(RATE, 480, TimeUs(0))
    });
    let mut out = RecordingOutput::new(dev, 48_000);
    let (mut tx, mut rx) = RingBuffer::<SoundCmd>::new(256);

    let mut frames = 0u64;
    let mut commands = 0u32;
    counting::track(true);
    counting::self_check();
    // Three key commands a step on two sounds, the third repeating the
    // second at its frame, which cuts it. Sixteen keys of 9000 frames
    // started two a step outnumber the 8 voices, so voices are stolen.
    for step in 0..STEPS {
        for k in 0..3u16 {
            let key = (2 * step as u16 + k.min(1)) % KEYS;
            tx.push(SoundCmd {
                sound: SoundId(key),
                origin: TimeUs(i64::from(step) * 10_000 + i64::from(k)),
            })
            .unwrap();
        }
        let r = e.step(&mut out, &mut rx).unwrap();
        frames += u64::from(r.frames);
        commands += r.commands;
        e.drain_scheduled(|_| {});
    }
    // A seek to chart time 15 s, due 100 ms from now, into the middle of a
    // BGM sound; the play goes on from there.
    let now = out.inner().next_wake();
    e.restart(TimeUs(now.0 - 15_000_000 + 100_000));
    e.seek(TimeUs(15_000_000));
    let sought = e.bgm_left();
    let mut entered = false;
    for _ in 0..50 {
        e.step(&mut out, &mut rx).unwrap();
        entered |= e.mixer().active() > 0;
    }
    let stop = AtomicBool::new(false);
    let mut looped = StopAfter {
        inner: &mut out,
        left: 500,
        stop: &stop,
    };
    let run = run_audio(&mut looped, &mut e, &mut rx, &stop).unwrap();
    counting::track(false);

    assert_eq!(run.steps, 500);
    assert_eq!(commands, 3 * STEPS);
    assert!(frames > u64::from(STEPS) * 480, "{frames}");
    assert!(out.inner().underruns() > SHIFT_HISTORY as u64);
    assert_eq!(e.underruns(), 40);
    assert!(e.mixer().steals() > 1_000, "{}", e.mixer().steals());
    assert!(e.mixer().cuts() >= u64::from(STEPS), "{}", e.mixer().cuts());
    assert!(e.mixer().saturated() > 0);
    // The events before 15 s were passed over, and voices sounded after.
    assert_eq!(sought, 1_500 - 750);
    assert!(entered);
    assert_eq!(counting::allocations(), 0);
}
