//! Generated key sounds (design D2): band-limited to 16 kHz so the 19 kHz
//! pilot band stays clear.
//!
//! Each sound is a few decaying partials between 80 Hz and 16 kHz (half of
//! the sounds have one in 15–16 kHz, the band edge) with a raised-cosine
//! attack and release, so neither end is a step: a step is broadband and
//! would reach the pilot band.

use crate::mixer::SoundBank;
use crate::rng::SplitMix64;
use std::f64::consts::{PI, TAU};

pub const MAX_PARTIAL_HZ: f64 = 16_000.0;
pub const SOUND_COUNT: usize = 64;
const ATTACK_S: f64 = 0.002;
const RELEASE_S: f64 = 0.005;
/// Peak level of each sound (−6 dBFS).
const PEAK: f64 = 0.5;

fn raised_cosine(x: f64) -> f64 {
    0.5 * (1.0 - (PI * x.clamp(0.0, 1.0)).cos())
}

/// One key sound of `len` frames at `rate_hz`.
pub fn key_sound(seed: u64, rate_hz: u32, len: usize) -> Vec<i16> {
    let mut rng = SplitMix64::new(seed);
    let rate = f64::from(rate_hz);
    let mut partials: Vec<(f64, f64, f64)> = (0..6)
        .map(|_| {
            let f = 80.0 + rng.unit_f64() * (MAX_PARTIAL_HZ - 80.0);
            (f, 0.3 + 0.7 * rng.unit_f64(), rng.unit_f64() * TAU)
        })
        .collect();
    if rng.below(2) == 0 {
        partials[0].0 = 15_000.0 + rng.unit_f64() * 1_000.0;
    }
    let decay_s = 0.2 + 0.8 * rng.unit_f64();
    let (attack, release) = (ATTACK_S * rate, RELEASE_S * rate);
    let raw: Vec<f64> = (0..len)
        .map(|n| {
            let t = n as f64 / rate;
            let env = raised_cosine(n as f64 / attack)
                * raised_cosine((len - n) as f64 / release)
                * (-t / decay_s).exp();
            env * partials
                .iter()
                .map(|&(f, a, p)| a * (TAU * f * t + p).sin())
                .sum::<f64>()
        })
        .collect();
    let peak = raw.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    raw.iter()
        .map(|x| (x / peak * PEAK * 32767.0).round() as i16)
        .collect()
}

/// The 64 key sounds, 1–3 s each.
pub fn key_bank(seed: u64, rate_hz: u32) -> SoundBank {
    let mut rng = SplitMix64::new(seed);
    SoundBank {
        sounds: (0..SOUND_COUNT)
            .map(|_| {
                let len = (f64::from(rate_hz) * (1.0 + 2.0 * rng.unit_f64())) as usize;
                key_sound(rng.next_u64(), rate_hz, len)
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sounds_start_and_end_without_steps() {
        let s = key_sound(3, 48_000, 48_000);
        assert_eq!((s[0], s[s.len() - 1]), (0, 0));
        let peak = s.iter().map(|x| x.unsigned_abs()).max().unwrap();
        assert!((16_000..=16_384).contains(&peak), "{peak}");
    }
}
