//! The mixer: preallocated voices summed in `f32`, oldest-voice stealing with
//! a raised-cosine fade, volume, a fixed headroom gain and a static soft
//! clipper.
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
//!
//! The sum is never gain-ridden: a limiter that lowers the whole mix when a
//! key sound pushes it over full scale would make the background audibly dip
//! and pump back with every hit. Instead the sum takes a fixed [`HEADROOM`]
//! gain and then [`soft_clip`], which is the identity up to [`SOFT_CLIP_KNEE`]
//! and saturates smoothly to full scale above it, sample by sample, with no
//! state, look-ahead or latency.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fmt;

use bms_model::SoundId;

use crate::CHANNELS;
use crate::bank::SoundBank;

/// Voice cap.
pub const MAX_VOICES: usize = 256;
/// Starts that can wait to be applied; more are dropped and counted.
pub const MAX_PENDING_STARTS: usize = 1024;
/// Largest master volume (+24 dB), so no sum of voices overflows `f32`.
pub const MAX_VOLUME: f32 = 16.0;
/// Fade applied to a stolen voice.
pub const STEAL_FADE_MS: u32 = 2;
/// Fixed gain on the sum (-6 dB), so a full-scale background and a
/// full-scale key sound together just reach full scale.
pub const HEADROOM: f32 = 0.5;
/// Level below which [`soft_clip`] passes samples unchanged.
pub const SOFT_CLIP_KNEE: f32 = 0.8;

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

/// A volume outside `0.0..=MAX_VOLUME` (NaN included).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InvalidVolume(pub f32);

impl fmt::Display for InvalidVolume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "volume {} is outside 0 to {MAX_VOLUME}", self.0)
    }
}

impl std::error::Error for InvalidVolume {}

/// The static soft clipper: `x` unchanged while `|x|` is at most
/// [`SOFT_CLIP_KNEE`]; above it, `knee + (1 - knee) tanh((|x| - knee) /
/// (1 - knee))` with the sign of `x`, which meets the identity with the
/// same slope and approaches full scale. It never exceeds full scale:
/// `knee + (1 - knee)` is exactly 1 in `f32` and `tanh` is at most 1.
#[must_use]
pub fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= SOFT_CLIP_KNEE {
        return x;
    }
    let span = 1.0 - SOFT_CLIP_KNEE;
    (SOFT_CLIP_KNEE + span * ((a - SOFT_CLIP_KNEE) / span).tanh()).copysign(x)
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
    /// Samples above [`SOFT_CLIP_KNEE`] after the volume and headroom.
    saturated: u64,
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
            saturated: 0,
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

    /// Starts waiting to be applied.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Whether another start can wait to be applied.
    #[must_use]
    pub fn can_start(&self) -> bool {
        self.pending.len() < self.pending.capacity()
    }

    /// Output samples so far whose level, after the volume and
    /// [`HEADROOM`], was above [`SOFT_CLIP_KNEE`] and so saturated.
    #[must_use]
    pub fn saturated(&self) -> u64 {
        self.saturated
    }

    /// Sets the master volume, applied with [`HEADROOM`] before the soft
    /// clipper.
    ///
    /// # Errors
    ///
    /// [`InvalidVolume`] for a volume outside `0.0..=MAX_VOLUME`, which is
    /// not applied.
    pub fn set_volume(&mut self, volume: f32) -> Result<(), InvalidVolume> {
        if !(0.0..=MAX_VOLUME).contains(&volume) {
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
        if let Some(free) = self.voices.iter_mut().find(|v| !v.active) {
            *free = voice;
            return;
        }
        let Some(oldest) = self.voices.iter_mut().min_by_key(|v| (v.start, v.seq)) else {
            return;
        };
        let victim = std::mem::replace(oldest, voice);
        self.steals += 1;
        self.release(victim, voice.start);
    }

    /// Moves a stolen voice to a release slot fading out from `cut`.
    fn release(&mut self, victim: Voice, cut: u64) {
        if let Some(free) = self.releases.iter_mut().find(|r| !r.active) {
            *free = Voice { cut, ..victim };
            return;
        }
        self.hard_cuts += 1;
        // The release furthest into its fade is the quietest.
        if let Some(quietest) = self.releases.iter_mut().min_by_key(|r| r.cut) {
            *quietest = Voice { cut, ..victim };
        }
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
                #[expect(
                    clippy::indexing_slicing,
                    reason = "`s` is below `cut` plus the fade length, so the fade index is in bounds"
                )]
                let g = if s < v.cut {
                    1.0
                } else {
                    fade[(s - v.cut) as usize]
                };
                let (l, r) = sound.frame((s - v.start) as usize);
                let o = (s - block_start) as usize * CHANNELS;
                #[expect(
                    clippy::indexing_slicing,
                    reason = "`s` is below `to`, at most the block's end, so the frame is inside `out`"
                )]
                {
                    out[o] += l * g;
                    out[o + 1] += r * g;
                }
            }
            if src_end <= to {
                v.active = false;
            }
        }
    }

    /// Renders the next `out.len() / 2` frames into interleaved stereo
    /// `out` (overwritten), starting at [`Mixer::mixed_to`]: every voice
    /// summed, with each pending start applied when its frame is reached,
    /// then the volume, [`HEADROOM`] and [`soft_clip`].
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
        let gain = self.volume * HEADROOM;
        for x in out.iter_mut() {
            let y = *x * gain;
            if y.abs() > SOFT_CLIP_KNEE {
                self.saturated += 1;
            }
            *x = soft_clip(y);
        }
        self.mixed_to = block_end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_clip_is_the_identity_up_to_the_knee() {
        let mut checked = 0;
        for k in -8_000..=8_000 {
            let x = k as f32 / 10_000.0;
            assert_eq!(soft_clip(x).to_bits(), x.to_bits(), "{x}");
            checked += 1;
        }
        assert_eq!(checked, 16_001);
        assert_eq!(soft_clip(SOFT_CLIP_KNEE), SOFT_CLIP_KNEE);
        assert_eq!(soft_clip(-SOFT_CLIP_KNEE), -SOFT_CLIP_KNEE);
    }

    #[test]
    fn soft_clip_saturates_smoothly_within_full_scale() {
        // 0.8 + 0.2 tanh(0.1 / 0.2) and 0.8 + 0.2 tanh(0.2 / 0.2), by hand
        // (tanh 0.5 = 0.4621172, tanh 1 = 0.7615942).
        assert!((soft_clip(0.9) - 0.892_423_4).abs() < 1e-6);
        assert!((soft_clip(-1.0) + 0.952_318_8).abs() < 1e-6);
        // Any input level, up to the largest float: within full scale,
        // nondecreasing, and above the knee once the input is.
        let mut prev = soft_clip(SOFT_CLIP_KNEE);
        let mut x = SOFT_CLIP_KNEE;
        let mut reached_full_scale = false;
        while x.is_finite() {
            x = x.next_up() * 1.001;
            for y in [soft_clip(x), -soft_clip(-x)] {
                assert!(y > SOFT_CLIP_KNEE && y <= 1.0, "{x}: {y}");
                assert!(y >= prev, "{x}: {y} < {prev}");
            }
            prev = soft_clip(x);
            reached_full_scale |= prev == 1.0;
        }
        assert!(reached_full_scale);
        assert_eq!(soft_clip(f32::MAX), 1.0);
        assert_eq!(soft_clip(f32::NEG_INFINITY), -1.0);
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
        // 0.25 of full scale, halved by the headroom.
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert_eq!(left, [0.125; 6]);
    }

    #[test]
    fn invalid_volumes_are_refused_and_not_applied() {
        let bank = SoundBank::from_sounds(400, vec![crate::Sound::mono(vec![8_192; 2])]);
        let mut m = Mixer::new(1, 400);
        for v in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -0.5,
            16.01,
            f32::MAX,
        ] {
            assert!(m.set_volume(v).is_err(), "{v}");
        }
        assert_eq!(m.set_volume(0.0), Ok(()));
        assert_eq!(m.set_volume(MAX_VOLUME), Ok(()));
        assert_eq!(m.set_volume(0.5), Ok(()));
        m.trigger(&bank, SoundId(0), 0).unwrap();
        let mut out = [0.0; 4];
        m.render(&bank, &mut out);
        // 0.25 × 0.5 volume × 0.5 headroom.
        assert_eq!(out, [0.0625; 4]);
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
