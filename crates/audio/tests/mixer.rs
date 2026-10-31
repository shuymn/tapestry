//! The mixer: exact start samples under any block split, one sounding
//! instance per sound id, stealing with a fade, late starts that keep the
//! head of the sound, and a mix whose gain does not ride the program.

mod common;

use audio::{BankLimits, Mixer, Sound, SoundBank, load_bank};
use bms_model::SoundId;
use common::{chart_of, put, tmp, wav};

const RATE: u32 = 48_000;

/// The mixer's fixed headroom (-6 dB): a sum of voices `s` below the soft
/// clipper's knee comes out as `H * s`, exactly, since `H` is a power of 2.
const H: f32 = 0.5;

fn left(out: &[f32]) -> Vec<f32> {
    out.iter().step_by(2).copied().collect()
}

fn render(m: &mut Mixer, bank: &SoundBank, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0; frames * 2];
    m.render(bank, &mut out);
    out
}

/// A mono ramp 7, 14, … (300 frames), a stereo sound with distinct
/// channels (200 frames), a 3-frame click (shorter than any block below but
/// 1 and 2), and a loud constant (2000 frames at 30 000) under two ids, 3
/// and 4, whose overlap pushes the sum over full scale.
fn split_bank() -> SoundBank {
    let stereo = (0..200i16).flat_map(|i| [5 * i + 3, -9 * i - 4]).collect();
    let loud = || Sound::mono(vec![30_000; 2_000]);
    SoundBank::from_sounds(
        RATE,
        vec![
            Sound::mono((1..=300).map(|i| i * 7).collect()),
            Sound::stereo(stereo),
            Sound::mono(vec![1_000, -2_000, 3_000]),
            loud(),
            loud(),
        ],
    )
}

/// The solo starts checked below, and three cuts: sounds 0 and 1 started
/// again while they play, and the click a second time at 2399.
const SPLIT_TRIGGERS: [(u16, u64); 9] = [
    (0, 1_000),
    (1, 1_037),
    (2, 2_399),
    (3, 0),
    (4, 500),
    (0, 4_321),
    (0, 1_100),
    (1, 1_200),
    (2, 2_399),
];

/// The split triggers rendered in `block`-frame blocks, each triggered
/// `ahead` frames before its start (all before the first block if `None`),
/// with the saturated sample and cut counts.
fn render_split(block: usize, ahead: Option<u64>, total: usize) -> (Vec<f32>, u64, u64) {
    let bank = split_bank();
    let mut m = Mixer::new(8, RATE);
    let mut pending: Vec<(u16, u64)> = SPLIT_TRIGGERS.to_vec();
    let mut out = vec![0.0f32; total * 2];
    let mut pos = 0;
    while pos < total {
        let n = block.min(total - pos);
        let end = (pos + n) as u64;
        pending.retain(|&(s, at)| {
            if ahead.is_some_and(|a| at >= end + a) {
                return true;
            }
            assert_eq!(m.trigger(&bank, SoundId(s), at), Some(at));
            false
        });
        m.render(&bank, &mut out[pos * 2..(pos + n) * 2]);
        pos += n;
    }
    (out, m.saturated(), m.cuts())
}

#[test]
fn sound_starts_at_exact_sample_for_any_block_split() {
    let (reference, saturated, cuts) = render_split(1, None, 6_000);
    // The two loud sounds overlap from 500 to 2000: 2 × 30 000 / 32 768 ×
    // 0.5 = 0.92 is above the knee (0.8) in both channels of those 1500
    // frames, and only there (the small sounds move it by under 0.06), so
    // the clipper saturated, and nothing left exceeds full scale.
    assert_eq!((saturated, cuts), (3_000, 3));
    assert!(reference.iter().all(|x| x.abs() <= 1.0));
    for block in [2, 7, 64, 441, 480, 1_024, 6_000] {
        for ahead in [None, Some(0), Some(1), Some(150)] {
            let (out, _, cuts) = render_split(block, ahead, 6_000);
            assert!(out == reference, "block {block}, ahead {ahead:?}");
            assert_eq!(cuts, 3, "block {block}, ahead {ahead:?}");
        }
    }

    // Each sound alone, in 480-frame blocks: silent before its start, its
    // first sample exactly at it (sample / 32 768, halved: / 65 536).
    let bank = split_bank();
    let solo = |s: u16, at: u64, frames: usize| {
        let mut m = Mixer::new(8, RATE);
        m.trigger(&bank, SoundId(s), at).unwrap();
        let mut out = Vec::new();
        while out.len() < frames * 2 {
            out.extend(render(&mut m, &bank, 480));
        }
        (out, m.sounding())
    };
    let (out, _) = solo(0, 1_000, 1_440);
    assert!(out[..2 * 1_000].iter().all(|&x| x == 0.0));
    assert_eq!(
        (out[2 * 1_000], out[2 * 1_000 + 1]),
        (7.0 / 65_536.0, 7.0 / 65_536.0)
    );
    assert_eq!(out[2 * 1_299], 2_100.0 / 65_536.0);
    assert_eq!(out[2 * 1_300], 0.0);
    let (out, _) = solo(1, 1_037, 1_440);
    assert_eq!(out[2 * 1_036..2 * 1_037], [0.0, 0.0]);
    assert_eq!(out[2 * 1_037..2 * 1_038], [3.0 / 65_536.0, -4.0 / 65_536.0]);
    assert_eq!(
        out[2 * 1_236..2 * 1_237],
        [998.0 / 65_536.0, -1_795.0 / 65_536.0]
    );
    // The click straddles a block boundary (2399 | 2400) and ends inside
    // the next block, freeing its voice.
    let (out, sounding) = solo(2, 2_399, 2_880);
    assert_eq!(
        left(&out)[2_398..2_403],
        [
            0.0,
            1_000.0 / 65_536.0,
            -2_000.0 / 65_536.0,
            3_000.0 / 65_536.0,
            0.0
        ]
    );
    assert_eq!(sounding, 0);
}

/// Constant mono sounds: 0.5, 0.25 and 0.125 of full scale, 100 frames.
fn steal_bank(rate: u32) -> SoundBank {
    SoundBank::from_sounds(
        rate,
        vec![
            Sound::mono(vec![16_384; 100]),
            Sound::mono(vec![8_192; 100]),
            Sound::mono(vec![4_096; 100]),
        ],
    )
}

/// `got` is the mix of the voice sums `want`, each below the knee.
fn assert_close(got: &[f32], want: &[f32]) {
    assert_eq!(got.len(), want.len());
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert!((g - H * w).abs() < 1e-6, "frame {i}: {g} vs {}", H * w);
    }
}

// At 2 kHz the 2 ms fade is 4 frames, gains 0.5 (1 + cos(πk/4)) for
// k = 1..4: 0.8535534, 0.5, 0.1464466, 0 (by hand).
const F1: f32 = 0.853_553_4;
const F3: f32 = 0.146_446_6;

#[test]
fn oldest_voice_is_stolen_with_fade() {
    let bank = steal_bank(2_000);
    let mut m = Mixer::new(2, 2_000);
    // A at 10, then B at 5, then C at 20, all scheduled before rendering.
    // At 20 both voices are busy and the oldest by start is B (5 < 10),
    // though A was triggered first: B fades from 20.
    assert_eq!(m.trigger(&bank, SoundId(0), 10), Some(10));
    assert_eq!(m.trigger(&bank, SoundId(1), 5), Some(5));
    assert_eq!(m.trigger(&bank, SoundId(2), 20), Some(20));
    let out = left(&render(&mut m, &bank, 40));
    assert_eq!((m.steals(), m.hard_cuts()), (1, 0));
    let mut want = vec![0.0; 40];
    want[5..10].fill(0.25);
    want[10..20].fill(0.75);
    want[20] = 0.5 + 0.25 * F1 + 0.125;
    want[21] = 0.5 + 0.25 * 0.5 + 0.125;
    want[22] = 0.5 + 0.25 * F3 + 0.125;
    want[23..40].fill(0.625);
    assert_close(&out, &want);

    // Equal starts: the earlier trigger (0.5) is the older one and fades.
    let mut m = Mixer::new(2, 2_000);
    m.trigger(&bank, SoundId(0), 0).unwrap();
    m.trigger(&bank, SoundId(1), 0).unwrap();
    m.trigger(&bank, SoundId(2), 1).unwrap();
    let out = left(&render(&mut m, &bank, 3));
    assert_close(&out, &[0.75, 0.5 * F1 + 0.375, 0.5 * 0.5 + 0.375]);
}

#[test]
fn exhausted_release_slots_cut_the_most_faded() {
    // One voice and one release slot. A (0.5) starts at 0; B (0.25) steals
    // it at 1, and A fades from there; C (0.125) steals B at 2, and B's
    // release needs the slot A is fading in, so A is cut at 2. Everything
    // A played before its cut stays.
    let bank = steal_bank(2_000);
    let mut m = Mixer::new(1, 2_000);
    m.trigger(&bank, SoundId(0), 0).unwrap();
    m.trigger(&bank, SoundId(1), 1).unwrap();
    m.trigger(&bank, SoundId(2), 2).unwrap();
    let out = left(&render(&mut m, &bank, 8));
    assert_eq!((m.steals(), m.hard_cuts()), (2, 1));
    assert_close(
        &out,
        &[
            0.5,
            0.5 * F1 + 0.25,
            0.25 * F1 + 0.125,
            0.25 * 0.5 + 0.125,
            0.25 * F3 + 0.125,
            0.125,
            0.125,
            0.125,
        ],
    );
}

#[test]
fn a_retrigger_stops_the_earlier_instance_at_the_new_start() {
    // A ramp 7, 14, … (300 frames) at 10 and again at 110: the output is
    // the first instance up to frame 109 and the second from 110, sample
    // for sample, with no fade and no release slot used.
    let bank = SoundBank::from_sounds(RATE, vec![Sound::mono((1..=300).map(|i| i * 7).collect())]);
    let mut m = Mixer::new(4, RATE);
    m.trigger(&bank, SoundId(0), 10).unwrap();
    m.trigger(&bank, SoundId(0), 110).unwrap();
    let mut out = left(&render(&mut m, &bank, 200));
    assert_eq!(m.sounding(), 1);
    out.extend(left(&render(&mut m, &bank, 300)));
    let mut want = vec![0.0f32; 500];
    for (f, w) in want.iter_mut().enumerate() {
        let k = match f {
            10..110 => f - 9,
            110..410 => f - 109,
            _ => continue,
        };
        *w = (k * 7) as f32 / 65_536.0;
    }
    assert_eq!(out, want);
    assert_eq!((m.cuts(), m.steals(), m.hard_cuts()), (1, 0, 0));
    assert_eq!(m.sounding(), 0);
}

#[test]
fn starts_of_one_id_at_the_same_frame_sound_once() {
    let bank = steal_bank(2_000);
    let mut m = Mixer::new(4, 2_000);
    for _ in 0..3 {
        m.trigger(&bank, SoundId(1), 5).unwrap();
    }
    let out = left(&render(&mut m, &bank, 50));
    assert_eq!(m.sounding(), 1);
    let mut want = vec![0.0; 50];
    want[5..].fill(0.25 * H);
    assert_eq!(out, want);
    assert_eq!((m.cuts(), m.steals()), (2, 0));
}

#[test]
fn ids_sharing_one_stored_sound_still_overlap() {
    // Two files with the same contents are stored once, but their ids are
    // separate sounds: started together they add up (8 kHz, the lowest
    // rate the bank decodes).
    let dir = tmp();
    let samples = vec![8_192i16; 100];
    let a = put(dir.path(), "a.wav", &wav(8_000, 1, &samples));
    let b = put(dir.path(), "b.wav", &wav(8_000, 1, &samples));
    let bank = load_bank(
        &chart_of(&[Some(a), Some(b)]),
        8_000,
        &BankLimits::default(),
    )
    .unwrap();
    assert_eq!(bank.stored_count(), 1);
    assert_eq!(bank.slot(SoundId(0)), bank.slot(SoundId(1)));
    let mut m = Mixer::new(4, 8_000);
    m.trigger(&bank, SoundId(0), 0).unwrap();
    m.trigger(&bank, SoundId(1), 0).unwrap();
    m.trigger(&bank, SoundId(1), 50).unwrap();
    let out = left(&render(&mut m, &bank, 160));
    let mut want = vec![0.0; 160];
    want[..100].fill(0.5 * H);
    want[100..150].fill(0.25 * H);
    assert_eq!(out, want);
    assert_eq!((m.cuts(), m.steals()), (1, 0));
}

#[test]
fn over_the_cap_a_victim_that_has_not_sounded_is_dropped() {
    // Two voices; A (0.5), B (0.25), C (0.125) and A again, all at 0. C
    // steals A, which has not sounded, so it is dropped, not faded; the
    // second A steals B the same way. One A and one C sound.
    let bank = steal_bank(2_000);
    let mut m = Mixer::new(2, 2_000);
    for s in [0, 1, 2, 0] {
        m.trigger(&bank, SoundId(s), 0).unwrap();
    }
    let out = left(&render(&mut m, &bank, 10));
    assert_eq!(m.sounding(), 2);
    assert_eq!(out, [0.625 * H; 10]);
    assert_eq!((m.steals(), m.cuts(), m.hard_cuts()), (2, 0, 0));
}

#[test]
fn a_retrigger_stops_its_fading_release() {
    // Two voices. A (0.5) and B (0.25) at 0; C (0.125) at 10 steals A,
    // which fades from 10; A again at 11 stops that tail and steals B,
    // which fades from 11.
    let bank = steal_bank(2_000);
    let mut m = Mixer::new(2, 2_000);
    for (s, at) in [(0, 0), (1, 0), (2, 10), (0, 11)] {
        m.trigger(&bank, SoundId(s), at).unwrap();
    }
    let out = left(&render(&mut m, &bank, 40));
    let mut want = vec![0.0; 40];
    want[..10].fill(0.75);
    want[10] = 0.5 * F1 + 0.25 + 0.125;
    want[11] = 0.5 + 0.25 * F1 + 0.125;
    want[12] = 0.5 + 0.25 * 0.5 + 0.125;
    want[13] = 0.5 + 0.25 * F3 + 0.125;
    want[14..].fill(0.625);
    assert_close(&out, &want);
    assert_eq!((m.steals(), m.cuts(), m.hard_cuts()), (2, 1, 0));
}

#[test]
fn retriggers_of_one_id_never_steal() {
    // 300 starts of one long sound, a frame apart: each stops the one
    // before, so one voice of the 256 is ever busy and none is stolen.
    let bank = SoundBank::from_sounds(RATE, vec![Sound::mono(vec![8_192; 1_000])]);
    let mut m = Mixer::new(256, RATE);
    for at in 0..300 {
        m.trigger(&bank, SoundId(0), at).unwrap();
    }
    let out = left(&render(&mut m, &bank, 1_400));
    assert_eq!((m.steals(), m.cuts(), m.hard_cuts()), (0, 299, 0));
    let mut want = vec![0.0; 1_400];
    want[..1_299].fill(0.25 * H);
    assert_eq!(out, want);
}

#[test]
fn starts_scheduled_ahead_apply_in_time_order() {
    // One voice; one-frame sounds at 0, 100 and 200 never overlap, so none
    // is stolen, whether all three are scheduled before the first block or
    // each just before its own, and whatever the block split.
    let bank = SoundBank::from_sounds(
        2_000,
        vec![
            Sound::mono(vec![16_384]),
            Sound::mono(vec![8_192]),
            Sound::mono(vec![4_096]),
        ],
    );
    let starts = [(0u16, 0u64), (1, 100), (2, 200)];
    let run = |block: usize, ahead: bool| {
        let mut m = Mixer::new(1, 2_000);
        let mut out = Vec::new();
        let mut next = 0;
        while out.len() < 2 * 480 {
            let from = m.mixed_to();
            while next < starts.len() && (ahead || starts[next].1 < from + block as u64) {
                let (s, at) = starts[next];
                assert_eq!(m.trigger(&bank, SoundId(s), at), Some(at));
                next += 1;
            }
            out.extend(render(&mut m, &bank, block));
        }
        assert_eq!((m.steals(), m.hard_cuts()), (0, 0));
        left(&out)[..480].to_vec()
    };
    let mut want = vec![0.0; 480];
    want[0] = 0.5 * H;
    want[100] = 0.25 * H;
    want[200] = 0.125 * H;
    for block in [1, 7, 100, 150, 480] {
        for ahead in [true, false] {
            assert_eq!(run(block, ahead), want, "block {block}, ahead {ahead}");
        }
    }
}

#[test]
fn late_command_never_cuts_the_head() {
    let data: Vec<i16> = (1..=300).map(|i| i * 11).collect();
    let bank = SoundBank::from_sounds(RATE, vec![Sound::mono(data.clone())]);
    // After 480 frames are rendered, frame 480 is the first unwritten one.
    // Targets well before it, one frame before it, and exactly at it all
    // start there; a later target keeps its frame.
    for (target, start) in [(100, 480), (479, 480), (480, 480), (481, 481)] {
        let mut m = Mixer::new(4, RATE);
        render(&mut m, &bank, 480);
        assert_eq!(
            m.trigger(&bank, SoundId(0), target),
            Some(start),
            "target {target}"
        );
        let out = left(&render(&mut m, &bank, 480));
        let at = (start - 480) as usize;
        assert!(out[..at].iter().all(|&x| x == 0.0));
        assert_eq!(out[at], 11.0 / 65_536.0, "target {target}");
        assert_eq!(out[at + 299], 3_300.0 / 65_536.0, "target {target}");
        assert_eq!(out[at + 300], 0.0);
    }
}

#[test]
fn volume_scales_before_the_soft_clipper() {
    // 0.5 of full scale × 0.5 volume × 0.5 headroom.
    let bank = SoundBank::from_sounds(RATE, vec![Sound::mono(vec![16_384; 4])]);
    let mut m = Mixer::new(1, RATE);
    m.set_volume(0.5).unwrap();
    m.trigger(&bank, SoundId(0), 0).unwrap();
    assert_eq!(left(&render(&mut m, &bank, 4)), [0.125; 4]);
}

#[test]
fn background_level_is_unchanged_by_a_loud_key_sound() {
    // A background at 0.5 of full scale (left) and 0.375 (right) for 400
    // frames. One key sound at 0.9 over frames 100 to 150 makes the left
    // sum 1.4, over full scale, but after the headroom (0.7) below the knee,
    // so it simply adds. Two at once over frames 250 to 300 make the sums
    // 2.3 and 2.175, over full scale even after the headroom, and those
    // frames saturate. Either way the background keeps its level right
    // after the key sounds end. A gain-riding limiter, before or after the
    // headroom, would still hold the mix down there while it recovers.
    let bgm = (0..400).flat_map(|_| [16_384i16, 12_288]).collect();
    let key = || Sound::mono(vec![29_491; 50]);
    let bank = SoundBank::from_sounds(2_000, vec![Sound::stereo(bgm), key(), key()]);
    let mut m = Mixer::new(3, 2_000);
    for (s, at) in [(0, 0), (1, 100), (1, 250), (2, 250)] {
        m.trigger(&bank, SoundId(s), at).unwrap();
    }
    let out = render(&mut m, &bank, 400);
    let key = 29_491.0 / 32_768.0;
    let (mut single, mut double) = (0, 0);
    for (i, f) in out.chunks(2).enumerate() {
        if (250..300).contains(&i) {
            double += 1;
            assert!(f[0] > f[1] && f[1] > 0.8 && f[0] <= 1.0, "frame {i}: {f:?}");
            continue;
        }
        let (l, r) = if (100..150).contains(&i) {
            single += 1;
            ((0.5 + key) * H, (0.375 + key) * H)
        } else {
            (0.5 * H, 0.375 * H)
        };
        assert!(
            (f[0] - l).abs() < 1e-6 && (f[1] - r).abs() < 1e-6,
            "frame {i}: {f:?} vs ({l}, {r})"
        );
    }
    assert_eq!((single, double), (50, 50));
    assert_eq!(m.saturated(), 100);
}

#[test]
fn a_saturated_channel_leaves_the_other_unchanged() {
    // Two sounds at 30 000 on the left and 1 000 on the right: the left
    // sum after the headroom, 0.92, is above the knee and saturates; the
    // right, 2 000 / 65 536, passes exactly. A stereo-linked gain would
    // lower the right too.
    let sound = || Sound::stereo((0..100).flat_map(|_| [30_000i16, 1_000]).collect());
    let bank = SoundBank::from_sounds(2_000, vec![sound(), sound()]);
    let mut m = Mixer::new(2, 2_000);
    m.trigger(&bank, SoundId(0), 0).unwrap();
    m.trigger(&bank, SoundId(1), 0).unwrap();
    let out = render(&mut m, &bank, 100);
    let mut frames = 0;
    for f in out.chunks(2) {
        assert!(f[0] > 0.8 && f[0] < 60_000.0 / 65_536.0, "{f:?}");
        assert_eq!(f[1], 2_000.0 / 65_536.0);
        frames += 1;
    }
    assert_eq!(frames, 100);
    assert_eq!(m.saturated(), 100);
}

/// `render` is `mix_raw` followed by the volume and the play's gain, the
/// soft clipper and a ring-out's envelope, bit for bit, from identical
/// mixers; `mix_raw` leaves the saturation count alone and the sum above
/// full scale.
#[test]
fn render_is_mix_raw_then_gain_and_clipper() {
    let bank = split_bank();
    for (volume, gain) in [(1.0, H), (0.75, 1.3), (2.0, audio::MAX_GAIN)] {
        let mut m = Mixer::new(8, RATE);
        m.set_volume(volume).unwrap();
        m.set_gain(gain).unwrap();
        for (s, at) in SPLIT_TRIGGERS {
            m.trigger(&bank, SoundId(s), at).unwrap();
        }
        // The envelope fades from frame 3 000 to 5 000.
        m.ring_out(3_000, 5_000);
        let mut raw_mixer = m.clone();
        let mut out = Vec::new();
        let mut raw = Vec::new();
        for _ in 0..13 {
            out.extend(render(&mut m, &bank, 480));
            let mut block = vec![0.0; 480 * 2];
            raw_mixer.mix_raw(&bank, &mut block);
            raw.extend(block);
        }
        assert_eq!(raw_mixer.saturated(), 0);
        assert!(m.saturated() > 0, "{volume} {gain}");
        assert!(raw.iter().any(|x| x.abs() > 1.0));
        let expected: Vec<f32> = raw
            .chunks(2)
            .enumerate()
            .flat_map(|(f, frame)| {
                let f = f as i64;
                let e = if f < 3_000 {
                    1.0
                } else if f >= 5_000 {
                    0.0
                } else {
                    ((5_000 - f) as f64 / 2_000.0) as f32
                };
                frame
                    .iter()
                    .map(move |&x| audio::soft_clip(x * (volume * gain)) * e)
            })
            .collect();
        assert_eq!(out.len(), expected.len());
        for (i, (a, b)) in out.iter().zip(&expected).enumerate() {
            assert_eq!(a.to_bits(), b.to_bits(), "sample {i} at {volume} {gain}");
        }
    }
}
