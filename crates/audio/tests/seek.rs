//! A seek against playing from the start (design.md "Audio"): from the seek's
//! frame on, the output is the same sample for sample, over generated banks
//! and schedules, banks at every practice speed, and the edges by hand.

mod common;

use std::sync::{Arc, OnceLock};

use audio::{
    AudioEngine, BankLimits, RecordingOutput, SimConfig, SimDevice, Sound, SoundBank, Speed,
    load_bank_at,
};
use bms_model::{BgmEvent, SoundId, TimeUs};
use proptest::prelude::*;
use rtrb::RingBuffer;

/// A low rate keeps the generated cases cheap; nothing in a seek depends
/// on the rate.
const RATE: u32 = 8_000;
const PERIOD: u32 = 80;
/// Steps before the play starts, so the clock is anchored at the seek.
const WARMUP: u32 = 3;

/// A play and where to start it.
#[derive(Clone, Debug)]
struct Case {
    bank: Arc<SoundBank>,
    bgm: Vec<BgmEvent>,
    voices: usize,
    origin: TimeUs,
    offset: TimeUs,
    t: TimeUs,
}

/// The output of `case` from the start or, with `seek`, from its `t`, until
/// every sound has ended; the output frame of `t`; and the first frame
/// written after the restart.
fn play(case: &Case, seek: bool) -> (Vec<f32>, u64, u64) {
    let dev = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let longest = (0..case.bank.stored_count() as u32)
        .filter_map(|i| case.bank.stored(i))
        .map(Sound::frames)
        .max()
        .unwrap_or(0);
    let last = case.bgm.last().map_or(0, |e| e.time.0).max(case.t.0);
    let end_us = case.origin.0 + last + (longest as i64 * 1_000_000 / i64::from(RATE)) + 50_000;
    let frames = (end_us * i64::from(RATE) / 1_000_000) as usize + 4 * PERIOD as usize;
    let mut out = RecordingOutput::new(dev, frames + 4 * PERIOD as usize);
    let mut e =
        AudioEngine::new(case.bank.clone(), &case.bgm, RATE, PERIOD, case.voices, 0).unwrap();
    e.set_output_offset(case.offset);
    let (_tx, mut rx) = RingBuffer::new(1);
    for _ in 0..WARMUP {
        e.step(&mut out, &mut rx).unwrap();
    }
    let restarted = e.written();
    e.restart(case.origin);
    if seek {
        e.seek(case.t);
    }
    while out.samples().len() < frames * 2 {
        e.step(&mut out, &mut rx).unwrap();
    }
    assert_eq!(out.dropped(), 0);
    assert_eq!(e.clock().rebases(), 0, "the mapping held");
    let at = TimeUs(case.origin.0 + case.t.0 - case.offset.0);
    (
        out.samples().to_vec(),
        e.clock().frame_at(at).unwrap(),
        restarted,
    )
}

/// The first frame from `from` on where `a` and `b` differ.
fn first_difference(a: &[f32], b: &[f32], from: usize) -> Option<usize> {
    let n = a.len().min(b.len());
    (2 * from..n)
        .find(|&i| a[i].to_bits() != b[i].to_bits())
        .map(|i| i / 2)
}

/// Checks the seek against the play from the start; returns the output
/// frame of `t` and the full play's output.
fn check(case: &Case) -> Result<(u64, Vec<f32>), String> {
    let (full, at, _) = play(case, false);
    let (sought, at_seek, restarted) = play(case, true);
    if at != at_seek {
        return Err(format!("t maps to frame {at} and {at_seek}"));
    }
    let from = at as usize;
    if let Some(f) = first_difference(&full, &sought, from) {
        return Err(format!(
            "frame {f} ({} after t's frame {from}): {:?} from the start, {:?} sought",
            f - from,
            &full[2 * f..2 * f + 2],
            &sought[2 * f..2 * f + 2],
        ));
    }
    // Before the play, the warmup played the schedule at origin 0.
    let silent = 2 * restarted as usize..2 * from;
    if let Some(i) = silent.into_iter().find(|&i| sought[i] != 0.0) {
        return Err(format!("frame {} sounds before t's frame {from}", i / 2));
    }
    Ok((at, full))
}

fn sound(frames: usize, stereo: bool, first: i16, step: i16) -> Sound {
    let n = if stereo { 2 * frames } else { frames };
    let samples = common::ramp(n, first, step);
    if stereo {
        Sound::stereo(samples)
    } else {
        Sound::mono(samples)
    }
}

fn events(times_ms: &[(i64, u16)]) -> Vec<BgmEvent> {
    times_ms
        .iter()
        .map(|&(ms, s)| BgmEvent {
            time: TimeUs(ms * 1_000),
            sound: SoundId(s),
        })
        .collect()
}

/// One bank per practice speed, loaded as a chart's sounds are: three
/// sources at other rates converted to `RATE` and then to the speed.
fn ladder_banks() -> &'static [Arc<SoundBank>] {
    static BANKS: OnceLock<Vec<Arc<SoundBank>>> = OnceLock::new();
    BANKS.get_or_init(|| {
        let dir = common::tmp();
        let paths = [
            ("a.wav", 11_025, 1, common::ramp(2_000, -9_000, 13)),
            ("b.wav", 22_050, 2, common::ramp(2 * 9_000, 7_000, -5)),
            ("c.wav", 16_000, 1, common::ramp(600, 20_000, -61)),
        ]
        .map(|(name, rate, channels, samples)| {
            Some(common::put(
                dir.path(),
                name,
                &common::wav(rate, channels, &samples),
            ))
        });
        let chart = common::chart_of(&paths);
        Speed::ladder()
            .map(|speed| {
                Arc::new(load_bank_at(&chart, RATE, speed, &BankLimits::default()).unwrap())
            })
            .collect()
    })
}

/// A generated bank of one to five sounds, up to 2 s long.
fn generated_bank() -> impl Strategy<Value = Arc<SoundBank>> {
    prop::collection::vec(
        (1usize..16_000, any::<bool>(), any::<i16>(), -200i16..200),
        1..6,
    )
    .prop_map(|sounds| {
        let sounds = sounds
            .into_iter()
            .map(|(frames, stereo, first, step)| sound(frames, stereo, first, step))
            .collect();
        Arc::new(SoundBank::from_sounds(RATE, sounds))
    })
}

/// Either a generated bank with exact times, or the bank at a ladder speed
/// with times mapped as the loader maps them, `floor(t × 20/k)`.
fn bank_and_speed() -> impl Strategy<Value = (Arc<SoundBank>, i64)> {
    prop_oneof![
        generated_bank().prop_map(|b| (b, 20)),
        (0..ladder_banks().len()).prop_map(|i| {
            let bank = ladder_banks()[i].clone();
            let k = i64::from(bank.speed().twentieths());
            (bank, k)
        }),
    ]
}

/// Up to 40 starts over 2 s, half of them on a 5 ms grid so that starts
/// share frames; ids up to one past the bank (a sound it lacks).
fn schedule() -> impl Strategy<Value = Vec<(i64, u16)>> {
    prop::collection::vec((0i64..2_000_000, any::<bool>(), 0u16..6), 0..40).prop_map(|events| {
        let mut events: Vec<(i64, u16)> = events
            .into_iter()
            .map(|(us, snap, s)| (if snap { us / 5_000 * 5_000 } else { us }, s))
            .collect();
        events.sort_by_key(|&(us, _)| us);
        events
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// From t's frame on, a seek's output is the output of playing from
    /// chart time 0, sample for sample: over overlapping sounds, sounds
    /// longer than the gap to t, t inside a sound, before the first start
    /// and after the last, one to five voices, and every practice speed.
    #[test]
    fn seek_matches_mixing_from_the_start(
        (bank, k) in bank_and_speed(),
        schedule in schedule(),
        voices in 1usize..6,
        origin in 1_000_000i64..1_100_000,
        offset in 0i64..20_000,
        t in -400_000i64..2_400_000,
    ) {
        let ids = bank.stored_count() as u16 + 1;
        let bgm = schedule
            .into_iter()
            .map(|(us, s)| BgmEvent { time: TimeUs(us * 20 / k), sound: SoundId(s % ids) })
            .collect();
        let case = Case {
            bank,
            bgm,
            voices,
            origin: TimeUs(origin),
            offset: TimeUs(offset),
            t: TimeUs(t * 20 / k),
        };
        if let Err(e) = check(&case) {
            prop_assert!(false, "{e}");
        }
    }
}

/// The left channel of `out` at frame `f`.
fn left(out: &[f32], f: u64) -> f32 {
    out[2 * f as usize]
}

#[test]
fn a_long_voice_enters_partway_into_its_sound() {
    // At 8 kHz: a 60 s BGM sound started at 5 s is 3 s
    // (24 000 frames) in at a seek to 8 s.
    let samples: Vec<i16> = (0..60 * RATE as usize)
        .map(|i| (i % 20_000) as i16)
        .collect();
    let case = Case {
        bank: Arc::new(SoundBank::from_sounds(RATE, vec![Sound::mono(samples)])),
        bgm: events(&[(5_000, 0)]),
        voices: 4,
        origin: TimeUs(1_000_000),
        offset: TimeUs(0),
        t: TimeUs(8_000_000),
    };
    let (at, full) = check(&case).unwrap();
    // Sample 24 000 is 4 000 (24 000 mod 20 000), halved by the headroom.
    assert_eq!(left(&full, at), 4_000.0 / 65_536.0);
    assert_eq!(left(&full, at + 1), 4_001.0 / 65_536.0);
}

#[test]
fn a_seek_restores_steals_fades_and_cuts_at_the_edges() {
    let bank = |sounds: Vec<Sound>| Arc::new(SoundBank::from_sounds(RATE, sounds));
    let long = || sound(16_000, false, 1_000, 1);
    let stereo = || sound(4_000, true, -3_000, 3);
    let base = |bank, bgm, voices, t_us| Case {
        bank,
        bgm,
        voices,
        origin: TimeUs(1_000_000),
        offset: TimeUs(3_000),
        t: TimeUs(t_us),
    };
    let cases = [
        // t before the chart and before the first start.
        base(bank(vec![long()]), events(&[(100, 0)]), 2, -300_000),
        base(bank(vec![long()]), events(&[(100, 0)]), 2, 50_000),
        // t after every sound has ended.
        base(bank(vec![stereo()]), events(&[(0, 0)]), 2, 900_000),
        // t on a start's frame: it starts as usual.
        base(
            bank(vec![long(), stereo()]),
            events(&[(0, 0), (200, 1)]),
            2,
            200_000,
        ),
        // One voice: the first start is stolen at 10 ms and fades over 2 ms
        // (16 frames); t lands 1 ms into the fade.
        base(
            bank(vec![long(), long()]),
            events(&[(0, 0), (10, 1)]),
            1,
            11_000,
        ),
        // One voice and one release slot: three starts 1 ms apart, so a
        // fading release is cut for the next; t inside the last fade.
        base(
            bank(vec![long(), long(), long(), long()]),
            events(&[(0, 0), (1, 1), (2, 2), (3, 3)]),
            1,
            3_500,
        ),
        // A start of the same id cuts its earlier voice before t; one at
        // the same frame as another sounds once.
        base(
            bank(vec![long(), stereo()]),
            events(&[(0, 0), (0, 0), (300, 0), (300, 1), (300, 1)]),
            2,
            400_000,
        ),
        // A start of an id the bank lacks is ignored.
        base(bank(vec![long()]), events(&[(0, 1), (100, 0)]), 1, 500_000),
        // Voice pressure: more long sounds than voices, steals before and
        // after t.
        base(
            bank((0..6).map(|_| long()).collect()),
            events(&[
                (0, 0),
                (50, 1),
                (100, 2),
                (150, 3),
                (200, 4),
                (250, 5),
                (900, 0),
            ]),
            3,
            220_000,
        ),
    ];
    for (i, case) in cases.iter().enumerate() {
        let (at, full) = check(case).unwrap_or_else(|e| panic!("case {i}: {e}"));
        assert!(
            full.iter().skip(2 * at as usize).any(|&x| x != 0.0) || i == 2,
            "case {i} sounds after t"
        );
    }
}

/// What a seek costs on the audio thread, where it runs: a 2-minute chart
/// at 48 kHz with 256 voices, a 120 s BGM sound and 50 starts a second over
/// 64 sounds of 0.1 to 2 s (6,000 starts), sought to 119 s. Run with
/// `cargo test --release -p audio --test seek -- --ignored --nocapture`.
#[test]
#[ignore = "timing; run in release"]
fn seek_cost_into_a_dense_two_minute_chart() {
    const DEVICE: u32 = 48_000;
    let mut sounds = vec![sound(120 * DEVICE as usize, true, 0, 1)];
    sounds.extend((0..64).map(|i| sound(4_800 + i * 1_425, false, i as i16, 3)));
    let bank = Arc::new(SoundBank::from_sounds(DEVICE, sounds));
    let mut bgm = events(&[(0, 0)]);
    bgm.extend((0..6_000u32).map(|k| BgmEvent {
        time: TimeUs(i64::from(k) * 20_000),
        sound: SoundId(1 + (k * 7 % 64) as u16),
    }));
    let mut e = AudioEngine::new(bank, &bgm, DEVICE, 480, audio::MAX_VOICES, 0).unwrap();
    let mut times: Vec<std::time::Duration> = (0..21)
        .map(|_| {
            e.restart(TimeUs(0));
            let at = std::time::Instant::now();
            e.seek(TimeUs(119_000_000));
            at.elapsed()
        })
        .collect();
    times.sort();
    assert_eq!(e.bgm_left(), 50);
    println!(
        "seek to 119 s over {} starts, {} voices: median {:?}, worst {:?}",
        bgm.len() - 50,
        audio::MAX_VOICES,
        times[times.len() / 2],
        times[times.len() - 1]
    );
}
