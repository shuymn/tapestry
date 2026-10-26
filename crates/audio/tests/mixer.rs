//! The mixer: exact start samples under any block split, stealing with a
//! fade, late starts that keep the head of the sound, and a mix whose gain
//! does not ride the program.

use audio::{Mixer, Sound, SoundBank};
use bms_model::SoundId;

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
/// 1 and 2), and a loud constant (2000 frames at 30 000) that two
/// overlapping copies push over full scale.
fn split_bank() -> SoundBank {
    let stereo = (0..200i16).flat_map(|i| [5 * i + 3, -9 * i - 4]).collect();
    SoundBank::from_sounds(
        RATE,
        vec![
            Sound::mono((1..=300).map(|i| i * 7).collect()),
            Sound::stereo(stereo),
            Sound::mono(vec![1_000, -2_000, 3_000]),
            Sound::mono(vec![30_000; 2_000]),
        ],
    )
}

const SPLIT_TRIGGERS: [(u16, u64); 6] = [
    (0, 1_000),
    (1, 1_037),
    (2, 2_399),
    (3, 0),
    (3, 500),
    (0, 4_321),
];

fn render_split(block: usize, total: usize) -> (Vec<f32>, u64) {
    let bank = split_bank();
    let mut m = Mixer::new(8, RATE);
    for (s, at) in SPLIT_TRIGGERS {
        m.trigger(&bank, SoundId(s), at).unwrap();
    }
    let mut out = vec![0.0f32; total * 2];
    let mut pos = 0;
    while pos < total {
        let n = block.min(total - pos);
        m.render(&bank, &mut out[pos * 2..(pos + n) * 2]);
        pos += n;
    }
    (out, m.saturated())
}

#[test]
fn sound_starts_at_exact_sample_for_any_block_split() {
    let (reference, saturated) = render_split(1, 6_000);
    // The two loud copies overlap from 500 to 2000: 2 × 30 000 / 32 768 ×
    // 0.5 = 0.92 is above the knee (0.8) in both channels of those 1500
    // frames, and only there (the small sounds move it by under 0.06), so
    // the clipper saturated, and nothing left exceeds full scale.
    assert_eq!(saturated, 3_000);
    assert!(reference.iter().all(|x| x.abs() <= 1.0));
    for block in [2, 7, 64, 441, 480, 1_024, 6_000] {
        assert!(render_split(block, 6_000).0 == reference, "block {block}");
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
    let bank = SoundBank::from_sounds(
        2_000,
        vec![Sound::stereo(bgm), Sound::mono(vec![29_491; 50])],
    );
    let mut m = Mixer::new(3, 2_000);
    for (s, at) in [(0, 0), (1, 100), (1, 250), (1, 250)] {
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
