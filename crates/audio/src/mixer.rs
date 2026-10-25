//! The mixer: preallocated voices summed in `f32`, oldest-voice stealing with
//! a raised-cosine fade, volume, and a peak limiter without look-ahead.
//!
//! A triggered sound waits as a pending start at an absolute output frame,
//! and rendering applies pending starts in time order as it reaches them, so
//! a sound starts at the same sample, and steals the same voice, whatever
//! the block split and however far ahead it was triggered. A start already
//! rendered moves to the first frame not yet rendered: the head of a sound
//! is never cut. A stolen voice is not cut off either: it moves to a release
//! slot and fades out over [`STEAL_FADE_MS`] from the new sound's start,
//! because an abrupt cut is a broadband click. Only when every release slot
//! is busy is the most-faded release cut, counted in [`Mixer::hard_cuts`].

use std::fmt;

use bms_model::SoundId;

use crate::CHANNELS;
use crate::bank::SoundBank;

/// Voice cap.
pub const MAX_VOICES: usize = 256;
/// Starts that can wait to be applied; more are dropped and counted.
pub const MAX_PENDING_STARTS: usize = 1024;
/// Fade applied to a stolen voice.
pub const STEAL_FADE_MS: u32 = 2;
/// The limiter's ceiling: full scale.
pub const LIMITER_CEILING: f32 = 1.0;
/// Time constant of the limiter's release back to unity gain.
pub const LIMITER_RELEASE_MS: u32 = 50;

#[derive(Clone, Copy, Debug, Default)]
struct Voice {
    active: bool,
    /// Index into the bank's stored sounds.
    sound: u32,
    start: u64,
    seq: u64,
    /// Fade-out start for a released voice; `u64::MAX` while playing.
    cut: u64,
}

/// A volume that is negative, infinite or NaN.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InvalidVolume(pub f32);

impl fmt::Display for InvalidVolume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "volume {} is not a finite, non-negative gain", self.0)
    }
}

impl std::error::Error for InvalidVolume {}

/// A stereo-linked peak limiter without look-ahead.
///
/// The gain drops at once to what keeps the louder channel at the ceiling
/// (so the sample that exceeds it is already limited and nothing is
/// delayed), and returns to 1 exponentially with [`LIMITER_RELEASE_MS`].
/// A peak exactly at the ceiling passes unchanged.
#[derive(Clone, Copy, Debug)]
pub struct Limiter {
    gain: f32,
    release: f32,
    /// Frames whose peak exceeded the ceiling.
    limited: u64,
}

impl Limiter {
    /// A limiter at unity gain for `rate` frames per second.
    #[must_use]
    pub fn new(rate: u32) -> Self {
        let tau = f64::from(rate) * f64::from(LIMITER_RELEASE_MS) / 1000.0;
        Self {
            gain: 1.0,
            release: (1.0 - (-1.0 / tau.max(1.0)).exp()) as f32,
            limited: 0,
        }
    }

    /// Limits one stereo frame.
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        let peak = l.abs().max(r.abs());
        let target = if peak > LIMITER_CEILING {
            self.limited += 1;
            LIMITER_CEILING / peak
        } else {
            1.0
        };
        if target < self.gain {
            self.gain = target;
        } else {
            self.gain += (target - self.gain) * self.release;
        }
        let clamp = |x: f32| (x * self.gain).clamp(-LIMITER_CEILING, LIMITER_CEILING);
        (clamp(l), clamp(r))
    }

    /// The current gain.
    #[must_use]
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Frames whose peak exceeded the ceiling so far.
    #[must_use]
    pub fn limited(&self) -> u64 {
        self.limited
    }
}

/// The mixer. Everything is allocated in [`Mixer::new`]; triggering and
/// rendering allocate nothing.
#[derive(Clone, Debug)]
pub struct Mixer {
    voices: Vec<Voice>,
    releases: Vec<Voice>,
    /// Starts not yet applied, latest first (the next one is last).
    pending: Vec<Voice>,
    /// Gain over the fade, from just below 1 down to 0.
    fade: Vec<f32>,
    /// First output frame not yet rendered.
    mixed_to: u64,
    seq: u64,
    volume: f32,
    limiter: Limiter,
    steals: u64,
    hard_cuts: u64,
    dropped_starts: u64,
}

impl Mixer {
    /// A mixer with `voices` voice slots (and as many release slots) at
    /// `rate`, whose stolen voices fade over [`STEAL_FADE_MS`].
    #[must_use]
    pub fn new(voices: usize, rate: u32) -> Self {
        let fade_frames = (u64::from(rate) * u64::from(STEAL_FADE_MS) / 1000) as usize;
        let fade = (1..=fade_frames)
            .map(|k| {
                let x = k as f64 / fade_frames as f64;
                (0.5 * (1.0 + (std::f64::consts::PI * x).cos())) as f32
            })
            .collect();
        Self {
            voices: vec![Voice::default(); voices],
            releases: vec![Voice::default(); voices],
            pending: Vec::with_capacity(MAX_PENDING_STARTS),
            fade,
            mixed_to: 0,
            seq: 0,
            volume: 1.0,
            limiter: Limiter::new(rate),
            steals: 0,
            hard_cuts: 0,
            dropped_starts: 0,
        }
    }

    /// First output frame not yet rendered.
    #[must_use]
    pub fn mixed_to(&self) -> u64 {
        self.mixed_to
    }

    /// Voices playing, release slots and pending starts excluded.
    #[must_use]
    pub fn active(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }

    /// Voices, fading releases and pending starts still to sound.
    #[must_use]
    pub fn sounding(&self) -> usize {
        self.active() + self.releases.iter().filter(|v| v.active).count() + self.pending.len()
    }

    /// Voices stolen so far.
    #[must_use]
    pub fn steals(&self) -> u64 {
        self.steals
    }

    /// Fading releases cut off because every release slot was busy.
    #[must_use]
    pub fn hard_cuts(&self) -> u64 {
        self.hard_cuts
    }

    /// Starts dropped because [`MAX_PENDING_STARTS`] were already waiting.
    #[must_use]
    pub fn dropped_starts(&self) -> u64 {
        self.dropped_starts
    }

    /// The limiter's state.
    #[must_use]
    pub fn limiter(&self) -> &Limiter {
        &self.limiter
    }

    /// Sets the master volume, applied before the limiter.
    ///
    /// # Errors
    ///
    /// [`InvalidVolume`] for a negative, infinite or NaN volume, which is
    /// not applied.
    pub fn set_volume(&mut self, volume: f32) -> Result<(), InvalidVolume> {
        if !(volume.is_finite() && volume >= 0.0) {
            return Err(InvalidVolume(volume));
        }
        self.volume = volume;
        Ok(())
    }

    /// Stops every voice, release and pending start (a retry).
    pub fn stop_all(&mut self) {
        for v in self.voices.iter_mut().chain(self.releases.iter_mut()) {
            v.active = false;
        }
        self.pending.clear();
    }

    /// Schedules sound `id` of `bank` to start at output frame `at`, or at
    /// the first frame not yet rendered if `at` is already rendered, and
    /// returns that frame. When the start is reached and every voice is
    /// busy, the oldest playing voice (earliest start, then earliest
    /// trigger) is stolen and fades out from it. Returns `None` if the bank
    /// holds no sound for `id` or [`MAX_PENDING_STARTS`] are waiting.
    pub fn trigger(&mut self, bank: &SoundBank, id: SoundId, at: u64) -> Option<u64> {
        let sound = bank.slot(id)?;
        let start = at.max(self.mixed_to);
        if self.pending.len() == self.pending.capacity() {
            self.dropped_starts += 1;
            return None;
        }
        self.seq += 1;
        let voice = Voice {
            active: true,
            sound,
            start,
            seq: self.seq,
            cut: u64::MAX,
        };
        let key = (start, self.seq);
        let i = self.pending.partition_point(|p| (p.start, p.seq) > key);
        self.pending.insert(i, voice);
        Some(start)
    }

    /// Puts a pending start into a voice slot, at its start frame.
    fn apply(&mut self, voice: Voice) {
        if let Some(i) = self.voices.iter().position(|v| !v.active) {
            self.voices[i] = voice;
            return;
        }
        let Some(i) =
            (0..self.voices.len()).min_by_key(|&i| (self.voices[i].start, self.voices[i].seq))
        else {
            return;
        };
        let victim = std::mem::replace(&mut self.voices[i], voice);
        self.steals += 1;
        self.release(victim, voice.start);
    }

    /// Moves a stolen voice to a release slot fading out from `cut`.
    fn release(&mut self, victim: Voice, cut: u64) {
        let slot = match self.releases.iter().position(|r| !r.active) {
            Some(k) => k,
            None => {
                self.hard_cuts += 1;
                // The release furthest into its fade is the quietest.
                match (0..self.releases.len()).min_by_key(|&k| self.releases[k].cut) {
                    Some(k) => k,
                    None => return,
                }
            }
        };
        self.releases[slot] = Voice { cut, ..victim };
    }

    /// Adds every voice and release into `out` (whose first frame is
    /// `block_start`) over frames `from..to`. Voices that end by `to` are
    /// freed.
    fn mix(&mut self, bank: &SoundBank, out: &mut [f32], block_start: u64, from: u64, to: u64) {
        let fade = &self.fade;
        for v in self
            .voices
            .iter_mut()
            .chain(self.releases.iter_mut())
            .filter(|v| v.active)
        {
            if v.start >= to {
                continue;
            }
            let Some(sound) = bank.stored(v.sound) else {
                v.active = false;
                continue;
            };
            let first = v.start.max(from);
            let src_end = (v.start + sound.frames()).min(v.cut.saturating_add(fade.len() as u64));
            let last = src_end.min(to);
            for s in first..last {
                let g = if s < v.cut {
                    1.0
                } else {
                    fade[(s - v.cut) as usize]
                };
                let (l, r) = sound.frame((s - v.start) as usize);
                let o = (s - block_start) as usize * CHANNELS;
                out[o] += l * g;
                out[o + 1] += r * g;
            }
            if src_end <= to {
                v.active = false;
            }
        }
    }

    /// Renders the next `out.len() / 2` frames into interleaved stereo
    /// `out` (overwritten), starting at [`Mixer::mixed_to`]: every voice
    /// summed, with each pending start applied when its frame is reached,
    /// then the volume and the limiter.
    pub fn render(&mut self, bank: &SoundBank, out: &mut [f32]) {
        out.fill(0.0);
        let block_start = self.mixed_to;
        let block_end = block_start + (out.len() / CHANNELS) as u64;
        let mut pos = block_start;
        loop {
            let next = self.pending.last().filter(|p| p.start < block_end).copied();
            let until = next.map_or(block_end, |p| p.start);
            if until > pos {
                self.mix(bank, out, block_start, pos, until);
                pos = until;
            }
            let Some(voice) = next else { break };
            self.pending.pop();
            self.apply(voice);
        }
        for [l, r] in out.as_chunks_mut::<CHANNELS>().0 {
            (*l, *r) = self.limiter.process(*l * self.volume, *r * self.volume);
        }
        self.mixed_to = block_end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_passes_full_scale_and_limits_above_it() {
        let mut l = Limiter::new(48_000);
        // Exactly at the ceiling: unchanged, gain stays 1, nothing counted.
        assert_eq!(l.process(1.0, -1.0), (1.0, -1.0));
        assert_eq!(l.process(-1.0, 0.25), (-1.0, 0.25));
        assert_eq!((l.gain(), l.limited()), (1.0, 0));
        // Above it: the same sample is brought to the ceiling (no
        // look-ahead), stereo linked by the louder channel.
        assert_eq!(l.process(2.0, 1.0), (1.0, 0.5));
        assert_eq!((l.gain(), l.limited()), (0.5, 1));
        // Release: a quiet frame afterwards is still attenuated, and the gain
        // recovers toward 1 by the per-frame coefficient
        // 1 - exp(-1 / (48_000 * 0.05)) = 0.000416580... (by hand).
        let (x, _) = l.process(0.5, 0.0);
        let g = 0.5 + 0.5 * 0.000_416_58_f32;
        assert!((x - 0.5 * g).abs() < 1e-6, "{x}");
        assert!(l.gain() < 1.0);
    }

    #[test]
    fn a_fade_of_zero_frames_still_steals() {
        // Rate 400 Hz: 2 ms is 0.8 frames, floored to no fade at all.
        let bank = SoundBank::from_sounds(400, vec![crate::Sound::mono(vec![8_192; 4])]);
        let mut m = Mixer::new(1, 400);
        m.trigger(&bank, SoundId(0), 0).unwrap();
        m.trigger(&bank, SoundId(0), 2).unwrap();
        let mut out = [0.0; 12];
        m.render(&bank, &mut out);
        assert_eq!(m.steals(), 1);
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert_eq!(left, [0.25; 6]);
    }

    #[test]
    fn invalid_volumes_are_refused_and_not_applied() {
        let bank = SoundBank::from_sounds(400, vec![crate::Sound::mono(vec![8_192; 2])]);
        let mut m = Mixer::new(1, 400);
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.5] {
            assert!(m.set_volume(v).is_err(), "{v}");
        }
        assert_eq!(m.set_volume(0.0), Ok(()));
        assert_eq!(m.set_volume(0.5), Ok(()));
        m.trigger(&bank, SoundId(0), 0).unwrap();
        let mut out = [0.0; 4];
        m.render(&bank, &mut out);
        assert_eq!(out, [0.125; 4]);
    }

    #[test]
    fn starts_beyond_the_pending_cap_are_dropped_and_counted() {
        let bank = SoundBank::from_sounds(400, vec![crate::Sound::mono(vec![1; 2])]);
        let mut m = Mixer::new(4, 400);
        for k in 0..MAX_PENDING_STARTS as u64 {
            assert_eq!(m.trigger(&bank, SoundId(0), k), Some(k));
        }
        assert_eq!(m.trigger(&bank, SoundId(0), 0), None);
        assert_eq!(m.dropped_starts(), 1);
        // Rendering frees room again.
        let mut out = vec![0.0; 2 * 2 * MAX_PENDING_STARTS];
        m.render(&bank, &mut out);
        assert!(m.trigger(&bank, SoundId(0), 0).is_some());
    }
}
