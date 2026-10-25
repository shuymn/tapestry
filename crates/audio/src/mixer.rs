//! The mixer: preallocated voices summed in `f32`, oldest-voice stealing with
//! a raised-cosine fade, volume, and a peak limiter without look-ahead.
//!
//! Voices start at an absolute output frame, so a sound starts at the same
//! sample whatever the block split. A start already rendered moves to the
//! first frame not yet rendered: the head of a sound is never cut. A stolen
//! voice is not cut off either: it moves to a release slot and fades out
//! over [`STEAL_FADE_MS`], because an abrupt cut is a broadband click. Only
//! when every release slot is busy is the most-faded release cut, counted in
//! [`Mixer::hard_cuts`].

use bms_model::SoundId;

use crate::CHANNELS;
use crate::bank::SoundBank;

/// Voice cap.
pub const MAX_VOICES: usize = 256;
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

/// Where a triggered sound starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Started {
    /// The output frame of the sound's first sample.
    pub frame: u64,
    /// The voice slot it took.
    pub voice: usize,
    /// Whether the slot was taken from the oldest playing voice.
    pub stole: bool,
}

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
    /// Gain over the fade, from just below 1 down to 0.
    fade: Vec<f32>,
    /// First output frame not yet rendered.
    mixed_to: u64,
    seq: u64,
    volume: f32,
    limiter: Limiter,
    steals: u64,
    hard_cuts: u64,
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
            fade,
            mixed_to: 0,
            seq: 0,
            volume: 1.0,
            limiter: Limiter::new(rate),
            steals: 0,
            hard_cuts: 0,
        }
    }

    /// First output frame not yet rendered.
    #[must_use]
    pub fn mixed_to(&self) -> u64 {
        self.mixed_to
    }

    /// Voices playing or scheduled, release slots excluded.
    #[must_use]
    pub fn active(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }

    /// Voices and fading releases still to sound.
    #[must_use]
    pub fn sounding(&self) -> usize {
        self.active() + self.releases.iter().filter(|v| v.active).count()
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

    /// The limiter's state.
    #[must_use]
    pub fn limiter(&self) -> &Limiter {
        &self.limiter
    }

    /// Sets the master volume, applied before the limiter.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume;
    }

    /// Stops every voice and release (a retry).
    pub fn stop_all(&mut self) {
        for v in self.voices.iter_mut().chain(self.releases.iter_mut()) {
            v.active = false;
        }
    }

    /// Starts sound `id` of `bank` at output frame `at`, or at the first
    /// frame not yet rendered if `at` is already rendered. When every voice
    /// is busy, the oldest (earliest start, then earliest trigger) is stolen
    /// and fades out from the new sound's start. Returns `None` if the bank
    /// holds no sound for `id` or the mixer has no voice slot.
    pub fn trigger(&mut self, bank: &SoundBank, id: SoundId, at: u64) -> Option<Started> {
        let sound = bank.slot(id)?;
        let start = at.max(self.mixed_to);
        self.seq += 1;
        let voice = Voice {
            active: true,
            sound,
            start,
            seq: self.seq,
            cut: u64::MAX,
        };
        if let Some(i) = self.voices.iter().position(|v| !v.active) {
            self.voices[i] = voice;
            return Some(Started {
                frame: start,
                voice: i,
                stole: false,
            });
        }
        let i =
            (0..self.voices.len()).min_by_key(|&i| (self.voices[i].start, self.voices[i].seq))?;
        let victim = std::mem::replace(&mut self.voices[i], voice);
        self.steals += 1;
        self.release(victim, start);
        Some(Started {
            frame: start,
            voice: i,
            stole: true,
        })
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

    /// Renders the next `out.len() / 2` frames into interleaved stereo
    /// `out` (overwritten), starting at [`Mixer::mixed_to`]: every voice
    /// summed, then the volume and the limiter. Voices that end are freed.
    pub fn render(&mut self, bank: &SoundBank, out: &mut [f32]) {
        out.fill(0.0);
        let frames = (out.len() / CHANNELS) as u64;
        let block_start = self.mixed_to;
        let block_end = block_start + frames;
        let fade = &self.fade;
        for v in self
            .voices
            .iter_mut()
            .chain(self.releases.iter_mut())
            .filter(|v| v.active)
        {
            if v.start >= block_end {
                continue;
            }
            let Some(sound) = bank.stored(v.sound) else {
                v.active = false;
                continue;
            };
            let first = v.start.max(block_start);
            let src_end = (v.start + sound.frames()).min(v.cut.saturating_add(fade.len() as u64));
            let last = src_end.min(block_end);
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
            if src_end <= block_end {
                v.active = false;
            }
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
        let s = m.trigger(&bank, SoundId(0), 2).unwrap();
        assert!(s.stole);
        let mut out = [0.0; 12];
        m.render(&bank, &mut out);
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert_eq!(left, [0.25; 6]);
    }
}
