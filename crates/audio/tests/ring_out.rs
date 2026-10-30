//! A practice round's ring-out (design.md "Mid-Chart Start and Section
//! Repeat"): the gain is 1 before the fade, falls linearly per output frame
//! to 0 at the end, where every voice stops and nothing starts; the output
//! does not depend on the period size; and every play starts at full gain.

use std::sync::Arc;

use audio::{
    AudioEngine, HEADROOM, RecordingOutput, ScheduledKind, SimConfig, SimDevice, Sound, SoundBank,
};
use bms_model::{BgmEvent, SoundId, TimeUs};
use rtrb::RingBuffer;

const RATE: u32 = 48_000;
const MS: i64 = 1_000;
/// Where chart time 0 lies on the device's timeline.
const ORIGIN: i64 = 100 * MS;
/// The ring-out of D15's example, 1 s and 1.5 s after a completion at 1 s.
const FADE_AT: TimeUs = TimeUs(2_000 * MS);
const END_AT: TimeUs = TimeUs(2_500 * MS);
/// The long sound's level, and the level the output holds for it.
const LEVEL: i16 = 8_192;

fn full() -> f32 {
    f32::from(LEVEL) / 32_768.0 * HEADROOM
}

/// Sound 0 lasts 10 s at [`LEVEL`]; sound 1 is 0.1 s at another level.
fn bank() -> Arc<SoundBank> {
    Arc::new(SoundBank::from_sounds(
        RATE,
        vec![
            Sound::mono(vec![LEVEL; 10 * RATE as usize]),
            Sound::mono(vec![2_000; RATE as usize / 10]),
        ],
    ))
}

/// The long sound at 0.5 s; sound 1 during the fade (2.2 s) and after the
/// end (2.6 s).
fn bgm() -> Vec<BgmEvent> {
    vec![
        BgmEvent {
            time: TimeUs(500 * MS),
            sound: SoundId(0),
        },
        BgmEvent {
            time: TimeUs(2_200 * MS),
            sound: SoundId(1),
        },
        BgmEvent {
            time: TimeUs(2_600 * MS),
            sound: SoundId(1),
        },
    ]
}

/// A steady device of `period` frames whose frame `n` plays at
/// `(n + 480) / RATE` s whatever the period, so every period puts a time on
/// the same frame.
fn device(period: u32, queued: u32) -> RecordingOutput<SimDevice> {
    let start = TimeUs((480 - i64::from(period)) * 1_000_000 / i64::from(RATE));
    let dev = SimDevice::new(SimConfig {
        queue_periods: queued,
        ..SimConfig::steady(RATE, period, start)
    });
    RecordingOutput::new(dev, 4 * RATE as usize)
}

/// What an engine played, and the BGM starts it logged by chart time.
struct Played {
    left: Vec<f32>,
    bgm: Vec<TimeUs>,
    engine: AudioEngine,
}

/// Plays [`bgm`] from chart time 0 at [`ORIGIN`] with `period`-frame passes,
/// asking for the ring-out at chart time 1 s when `ring_out`, until 3.5 s.
fn play(period: u32, ring_out: bool) -> Played {
    let mut out = device(period, 2);
    let mut e = AudioEngine::new(bank(), &bgm(), RATE, period, 8, 64).unwrap();
    let (_tx, mut rx) = RingBuffer::new(4);
    e.restart(TimeUs(ORIGIN));
    let mut asked = !ring_out;
    while out.inner().next_wake() < TimeUs(ORIGIN + 3_500 * MS) {
        if !asked && out.inner().next_wake() >= TimeUs(ORIGIN + 1_000 * MS) {
            e.ring_out(FADE_AT, END_AT);
            asked = true;
        }
        e.step(&mut out, &mut rx).unwrap();
    }
    let mut bgm = Vec::new();
    e.drain_scheduled(|s| {
        if let ScheduledKind::Bgm { time } = s.kind {
            bgm.push(time);
        }
    });
    Played {
        left: out.samples().iter().step_by(2).copied().collect(),
        bgm,
        engine: e,
    }
}

/// The output frame chart time `t` falls on: frame `n` plays at
/// `(n + 480) / RATE` s, and chart time 0 at [`ORIGIN`].
fn frame(t: TimeUs) -> usize {
    usize::try_from((ORIGIN + t.0 - 10 * MS) * i64::from(RATE) / 1_000_000).unwrap()
}

#[test]
fn ring_out_envelope_is_on_output_frames() {
    let rung = play(480, true);
    let plain = play(480, false);
    let (fade, end) = (frame(FADE_AT), frame(END_AT));
    // By hand: (2.1 s - 10 ms) and (2.6 s - 10 ms) at 48 kHz.
    assert_eq!((fade, end), (100_320, 124_320));
    assert_eq!(end - fade, 24_000);
    assert!(rung.left.len() > end + 1_000);
    // Gain 1 before the fade: the same output as without a ring-out.
    assert_eq!(rung.left[..fade], plain.left[..fade]);
    assert_eq!(rung.left[fade - 1], full());
    // From the fade the gain falls by 1/24 000 a frame: 1 at the fade, a
    // quarter down 6 000 frames in (before sound 1 at 2.2 s), 1/24 000 on
    // the last frame, 0 at the end.
    let mut checked = 0;
    for f in fade..end {
        let g = ((end - f) as f64 / 24_000.0) as f32;
        assert_eq!(rung.left[f], plain.left[f] * g, "frame {f}");
        checked += 1;
    }
    assert_eq!(checked, 24_000);
    assert_eq!(rung.left[fade], full());
    assert_eq!(rung.left[fade + 6_000], full() * 0.75);
    assert_eq!(rung.left[end - 1], full() * (1.0 / 24_000.0));
    // At the end every voice has stopped: silence after it, while the long
    // sound plays on without the ring-out.
    assert!(rung.left[end..].iter().all(|&x| x == 0.0));
    assert_eq!(plain.left[end], full());
    assert_eq!(rung.engine.mixer().sounding(), 0);
    assert_eq!(rung.engine.bgm_left(), 0);
    // The BGM plays on during the fade (2.2 s); the event after the end
    // (2.6 s) never starts.
    assert_eq!(rung.bgm, [TimeUs(500 * MS), TimeUs(2_200 * MS)]);
    assert_eq!(plain.bgm.len(), 3);
}

#[test]
fn ring_out_is_independent_of_period_size() {
    // 7 ms (336 frames, which does not divide the 24 000-frame fade) and
    // 10 ms passes.
    let seven = play(336, true);
    let ten = play(480, true);
    let n = seven.left.len().min(ten.left.len());
    assert!(n > frame(END_AT) + 1_000);
    let mut differing = 0;
    for f in 0..n {
        if seven.left[f].to_bits() != ten.left[f].to_bits() {
            differing += 1;
        }
    }
    assert_eq!(differing, 0);
    // The fade is there in both.
    assert!(seven.left[frame(FADE_AT) + 1] < full());
    assert_eq!(seven.bgm, ten.bgm);
}

#[test]
fn play_resets_the_ring_out_envelope() {
    for cut_mid_fade in [false, true] {
        let mut out = device(480, 2);
        let mut e = AudioEngine::new(bank(), &bgm(), RATE, 480, 8, 0).unwrap();
        let (_tx, mut rx) = RingBuffer::new(4);
        e.restart(TimeUs(ORIGIN));
        e.ring_out(FADE_AT, END_AT);
        // Into the fade, or past the end.
        let until = if cut_mid_fade { 2_300 } else { 2_700 };
        while out.inner().next_wake() < TimeUs(ORIGIN + until * MS) {
            e.step(&mut out, &mut rx).unwrap();
        }
        let left = |out: &RecordingOutput<SimDevice>, f: usize| out.samples()[2 * f];
        assert!(left(&out, frame(FADE_AT) + 1) < full());
        // The next play: chart time 0 just ahead of what is written, the
        // long sound at its own 0.5 s.
        let written = usize::try_from(e.written()).unwrap();
        let origin = (written as i64 + 2 * 480) * 1_000_000 / i64::from(RATE) + 10 * MS;
        e.restart(TimeUs(origin - 500 * MS));
        let first = written + 2 * 480;
        while out.inner().next_wake() < TimeUs(origin + 200 * MS) {
            e.step(&mut out, &mut rx).unwrap();
        }
        // From the sound's first frame on, full gain on every frame.
        let start = (first..)
            .find(|&f| left(&out, f) != 0.0)
            .expect("the next play sounds");
        assert!(start <= first + 1, "{start} {first}");
        let frames = (start..start + RATE as usize / 10)
            .filter(|&f| left(&out, f) == full())
            .count();
        assert_eq!(frames, RATE as usize / 10, "cut mid-fade: {cut_mid_fade}");
    }
}

/// Steps `e` into `out` until the device's next wake reaches `until` on
/// its timeline.
fn run_to(e: &mut AudioEngine, out: &mut RecordingOutput<SimDevice>, until: i64) {
    let (_tx, mut rx) = RingBuffer::new(4);
    while out.inner().next_wake() < TimeUs(until) {
        e.step(out, &mut rx).unwrap();
    }
}

#[test]
fn a_ring_out_ends_a_seek_still_waiting() {
    // A play sought to 10 s, whose seek waits for its frame, rung out from
    // 1 s to 1.5 s with a BGM event left at 12 s.
    let bgm = vec![
        BgmEvent {
            time: TimeUs(500 * MS),
            sound: SoundId(0),
        },
        BgmEvent {
            time: TimeUs(12_000 * MS),
            sound: SoundId(1),
        },
    ];
    let mut out = device(480, 2);
    let mut e = AudioEngine::new(bank(), &bgm, RATE, 480, 8, 0).unwrap();
    run_to(&mut e, &mut out, 50 * MS);
    e.restart(TimeUs(ORIGIN));
    let plan =
        audio::SeekPlan::new(&e.seek_source(), &bgm, e.frame_line(), TimeUs(10_000 * MS)).unwrap();
    e.seek(&plan).unwrap();
    e.ring_out(TimeUs(1_000 * MS), TimeUs(1_500 * MS));
    // Past the ring-out's end, long before the seek's frame: the play is
    // over, as a sealed tail is, with nothing left to start.
    run_to(&mut e, &mut out, ORIGIN + 1_600 * MS);
    assert!(e.mixer().rung_out());
    assert_eq!(e.mixer().sounding(), 0);
    assert_eq!(e.bgm_left(), 0);
    // The seek's voices and the event at 12 s never sound.
    run_to(&mut e, &mut out, ORIGIN + 13_000 * MS);
    let end = frame(TimeUs(1_500 * MS));
    assert!(out.samples()[2 * end..].iter().all(|&x| x == 0.0));
}

#[test]
fn a_fade_after_the_end_counts_as_the_end() {
    // The fade at 3 s, after the end at 2.5 s: the output stops at 2.5 s,
    // where the long sound is cut and the event at 2.6 s never starts.
    let mut out = device(480, 2);
    let mut e = AudioEngine::new(bank(), &bgm(), RATE, 480, 8, 64).unwrap();
    e.restart(TimeUs(ORIGIN));
    e.ring_out(TimeUs(3_000 * MS), END_AT);
    run_to(&mut e, &mut out, ORIGIN + 3_500 * MS);
    let left: Vec<f32> = out.samples().iter().step_by(2).copied().collect();
    let end = frame(END_AT);
    // Full gain up to the end (sound 1 at 2.2 s adds to it), silence from
    // it on.
    assert_eq!(left[frame(TimeUs(2_100 * MS))], full());
    assert!(left[end - 1] >= full());
    assert!(left[end..].iter().all(|&x| x == 0.0));
    assert_eq!(e.mixer().sounding(), 0);
    assert_eq!(e.bgm_left(), 0);
    let mut starts = Vec::new();
    e.drain_scheduled(|s| starts.push(s.kind));
    assert!(!starts.contains(&ScheduledKind::Bgm {
        time: TimeUs(2_600 * MS)
    }));
}

#[test]
fn a_seek_during_a_ring_out_waits_for_the_next_play() {
    // Rung out from 2 s to 2.5 s, rendered into the fade (2.2 s), then a
    // seek to 2.4 s, inside the long sound: refused, so the fade still
    // ends at 2.5 s and nothing sounds after it.
    let mut out = device(480, 2);
    let mut e = AudioEngine::new(bank(), &bgm(), RATE, 480, 8, 0).unwrap();
    e.restart(TimeUs(ORIGIN));
    e.ring_out(FADE_AT, END_AT);
    run_to(&mut e, &mut out, ORIGIN + 2_200 * MS);
    let plan =
        audio::SeekPlan::new(&e.seek_source(), &bgm(), e.frame_line(), TimeUs(2_400 * MS)).unwrap();
    assert_eq!(e.seek(&plan), Err(audio::SeekError::RingingOut));
    run_to(&mut e, &mut out, ORIGIN + 3_000 * MS);
    assert!(e.mixer().rung_out());
    let end = frame(END_AT);
    assert!(out.samples()[2 * end..].iter().all(|&x| x == 0.0));
    // The next play takes the ring-out off, and a seek with it.
    let origin = out.inner().next_wake().0 + 200 * MS;
    e.restart(TimeUs(origin));
    let plan =
        audio::SeekPlan::new(&e.seek_source(), &bgm(), e.frame_line(), TimeUs(1_000 * MS)).unwrap();
    assert_eq!(e.seek(&plan), Ok(()));
}
