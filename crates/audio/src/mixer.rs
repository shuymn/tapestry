//! The mixer: preallocated voices summed in `f32`, one sounding instance per
//! sound id, oldest-voice stealing with a raised-cosine fade, volume, the
//! play's fixed gain and a static soft clipper.
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
//! A sound id sounds at most once at a time, as BMS charts are written for:
//! a start stops the voice or release already playing its id at the new
//! start's first frame, without a fade, and takes that voice's slot, counted
//! in [`Mixer::cuts`]; starts of one id at the same frame therefore sound
//! once. The rule keys on the id, not on the stored sound: ids whose files
//! have identical contents share a stored sound but still overlap.
//!
//! A seek (design.md "Audio") starts the output partway into a schedule. A
//! [`SeekPlan`](crate::SeekPlan), made off the audio thread with the same
//! slot rules, holds the voices left sounding at the seek's frame; the
//! mixer parks a copy and, at that frame, puts them in the slots rendering
//! would have put them in, partway into their sounds and fades. No start is
//! put before that frame while they wait.
//!
//! A ring-out (design.md "Mid-Chart Start and Section Repeat") fades the
//! output on its own frames: gain 1 before its fade frame, falling linearly
//! to 0 at its end frame, where every voice stops and from which nothing
//! starts. The envelope multiplies the clipped output, so it is independent
//! of the volume and of how the output is divided into blocks. Stopping
//! everything ([`Mixer::stop_all`], as every play's start does) takes it
//! off.
//!
//! The sum is never gain-ridden: a limiter that lowers the whole mix when a
//! key sound pushes it over full scale would make the background audibly dip
//! and pump back with every hit. Instead the sum takes the play's fixed gain
//! ([`HEADROOM`], or the chart's normalization gain, design.md "Algorithms")
//! and then [`soft_clip`], which is the identity up to [`SOFT_CLIP_KNEE`]
//! and saturates smoothly to full scale above it, sample by sample, with no
//! state, look-ahead or latency. [`Mixer::mix_raw`] is the sum alone, which
//! the loudness analysis measures.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::SoundId;

use crate::CHANNELS;
use crate::bank::{Sound, SoundBank};

/// Voice cap.
pub const MAX_VOICES: usize = 256;
/// Starts that can wait to be applied; more are dropped and counted.
pub const MAX_PENDING_STARTS: usize = 1024;
/// Largest master volume (+24 dB), so no sum of voices overflows `f32`.
pub const MAX_VOLUME: f32 = 16.0;
/// Fade applied to a stolen voice.
pub const STEAL_FADE_MS: u32 = 2;
/// The gain on the sum of a play without a loudness analysis (-6 dB), so a
/// full-scale background and a full-scale key sound together just reach
/// full scale.
pub const HEADROOM: f32 = 0.5;
/// The largest gain on the sum (+6 dB, `10^(6/20)`), the normalization's
/// cap.
pub const MAX_GAIN: f32 = 1.995_262_3;
/// Level below which [`soft_clip`] passes samples unchanged.
pub const SOFT_CLIP_KNEE: f32 = 0.8;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Voice {
    pub(crate) active: bool,
    /// The sound id it was triggered as.
    pub(crate) id: SoundId,
    /// Index into the bank's stored sounds.
    pub(crate) sound: u32,
    /// Output frame of its first sample: before frame 0 for a voice a seek
    /// placed after a stream's first frames.
    pub(crate) start: i64,
    pub(crate) seq: u64,
    /// Fade-out start for a released voice; `i64::MAX` while playing.
    pub(crate) cut: i64,
}

impl Voice {
    /// An unused slot.
    const IDLE: Self = Self {
        active: false,
        id: SoundId(0),
        sound: 0,
        start: 0,
        seq: 0,
        cut: i64::MAX,
    };

    /// The frame after its last sample, or after its fade for a release.
    fn end(&self, sound: &Sound, fade: usize) -> i64 {
        let frames = i64::try_from(sound.frames()).unwrap_or(i64::MAX);
        let fade = i64::try_from(fade).unwrap_or(i64::MAX);
        self.start
            .saturating_add(frames)
            .min(self.cut.saturating_add(fade))
    }

    /// The voice `by` frames later.
    fn shifted(self, by: i64) -> Self {
        let cut = if self.cut == i64::MAX {
            i64::MAX
        } else {
            self.cut.saturating_add(by)
        };
        Self {
            start: self.start.saturating_add(by),
            cut,
            ..self
        }
    }
}

/// An output frame as a voice frame (output frames stay far below
/// `i64::MAX`: 6 million years at 48 kHz).
pub(crate) fn signed(frame: u64) -> i64 {
    i64::try_from(frame).unwrap_or(i64::MAX)
}

/// Frames of the steal fade at `rate`.
pub(crate) fn fade_frames(rate: u32) -> usize {
    (u64::from(rate) * u64::from(STEAL_FADE_MS) / 1000) as usize
}

/// What starting voices counted.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Counts {
    steals: u64,
    cuts: u64,
    hard_cuts: u64,
}

/// Voice slots and as many release slots. Which slot a voice takes decides
/// the order the mix adds voices in, so a seek keeps it.
#[derive(Clone, Debug)]
pub(crate) struct Slots {
    pub(crate) voices: Vec<Voice>,
    releases: Vec<Voice>,
}

impl Slots {
    pub(crate) fn new(voices: usize) -> Self {
        Self {
            voices: vec![Voice::IDLE; voices],
            releases: vec![Voice::IDLE; voices],
        }
    }

    pub(crate) fn all(&self) -> impl Iterator<Item = &Voice> {
        self.voices.iter().chain(&self.releases)
    }

    fn all_mut(&mut self) -> impl Iterator<Item = &mut Voice> {
        self.voices.iter_mut().chain(&mut self.releases)
    }

    fn sounding(&self) -> usize {
        self.all().filter(|v| v.active).count()
    }

    fn stop(&mut self) {
        for v in self.all_mut() {
            v.active = false;
        }
    }

    /// Puts a start into a voice slot, at its start frame: the slot of the
    /// voice its id stops, else a free one, else the oldest voice's.
    pub(crate) fn apply(&mut self, voice: Voice, counts: &mut Counts) {
        let mut slot = None;
        for (i, v) in self.voices.iter_mut().enumerate() {
            if v.active && v.id == voice.id {
                v.active = false;
                counts.cuts += 1;
                slot.get_or_insert(i);
            }
        }
        for r in &mut self.releases {
            if r.active && r.id == voice.id {
                r.active = false;
                counts.cuts += 1;
            }
        }
        let slot = slot.or_else(|| self.voices.iter().position(|v| !v.active));
        if let Some(free) = slot.and_then(|i| self.voices.get_mut(i)) {
            *free = voice;
            return;
        }
        let Some(oldest) = self.voices.iter_mut().min_by_key(|v| (v.start, v.seq)) else {
            return;
        };
        let victim = std::mem::replace(oldest, voice);
        counts.steals += 1;
        // A victim starting here too has not sounded: it is dropped.
        if victim.start < voice.start {
            self.release(victim, voice.start, counts);
        }
    }

    /// Moves a stolen voice to a release slot fading out from `cut`.
    fn release(&mut self, victim: Voice, cut: i64, counts: &mut Counts) {
        if let Some(free) = self.releases.iter_mut().find(|r| !r.active) {
            *free = Voice { cut, ..victim };
            return;
        }
        counts.hard_cuts += 1;
        // The release furthest into its fade is the quietest.
        if let Some(quietest) = self.releases.iter_mut().min_by_key(|r| r.cut) {
            *quietest = Voice { cut, ..victim };
        }
    }

    /// Frees what [`Mixer::render`] frees when it mixes up to `to`: every
    /// voice and release started before `to` that has ended by it or whose
    /// sound is missing.
    pub(crate) fn retire(&mut self, bank: &SoundBank, fade: usize, to: i64) {
        for v in self.all_mut().filter(|v| v.active && v.start < to) {
            match bank.stored(v.sound) {
                Some(sound) if v.end(sound, fade) > to => {}
                _ => v.active = false,
            }
        }
    }
}

/// Where a seek's voices are.
#[derive(Clone, Copy, Debug)]
enum Seek {
    /// No seek is waiting.
    Idle,
    /// The voices sounding at frame `at` of the seek's own mapping wait in
    /// the parked slots.
    Parked { at: i64 },
    /// They enter at output frame `from`, `by` frames later than parked.
    Placed { from: i64, by: i64 },
}

/// A ring-out's envelope on output frames.
#[derive(Clone, Copy, Debug)]
struct Envelope {
    /// The frame the fade starts at: gain 1 before it.
    fade: i64,
    /// The frame the gain reaches 0 at, where every voice stops.
    end: i64,
    /// Whether rendering has reached `end` and stopped every voice.
    ended: bool,
}

impl Envelope {
    /// The gain of frame `f`: 1 before `fade`, `(end - f) / (end - fade)`
    /// from it, 0 from `end` on.
    fn gain(&self, f: i64) -> f32 {
        if f < self.fade {
            1.0
        } else if f >= self.end {
            0.0
        } else {
            ((self.end - f) as f64 / (self.end - self.fade) as f64) as f32
        }
    }
}

/// A volume outside `0.0..=MAX_VOLUME` (NaN included).
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("volume {0} is outside 0 to {MAX_VOLUME}")]
pub struct InvalidVolume(pub f32);

/// A gain that is not above 0 and at most [`MAX_GAIN`] (NaN included).
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("gain {0} is not above 0 and at most {MAX_GAIN}")]
pub struct InvalidGain(pub f32);

/// A play's fixed gain on the sum: above 0 and at most [`MAX_GAIN`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gain(f32);

impl Gain {
    /// The gain of a play without a loudness analysis.
    pub const HEADROOM: Self = Self(HEADROOM);

    /// `gain`, checked.
    ///
    /// # Errors
    ///
    /// [`InvalidGain`] for a gain not above 0 or above [`MAX_GAIN`], NaN
    /// included.
    pub fn new(gain: f32) -> Result<Self, InvalidGain> {
        if gain > 0.0 && gain <= MAX_GAIN {
            Ok(Self(gain))
        } else {
            Err(InvalidGain(gain))
        }
    }

    /// The gain as a factor.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

/// The static soft clipper: `x` unchanged while `|x|` is at most
/// [`SOFT_CLIP_KNEE`]; above it, `knee + (1 - knee) tanh((|x| - knee) /
/// (1 - knee))` with the sign of `x`, which meets the identity with the
/// same slope and approaches full scale. It never exceeds full scale:
/// `knee + (1 - knee)` is exactly 1 in `f32` and `tanh` is at most 1. A
/// NaN, which no sum of finite samples makes, becomes silence.
#[must_use]
pub fn soft_clip(x: f32) -> f32 {
    if x.is_nan() {
        return 0.0;
    }
    let a = x.abs();
    if a <= SOFT_CLIP_KNEE {
        return x;
    }
    let span = 1.0 - SOFT_CLIP_KNEE;
    (SOFT_CLIP_KNEE + span * ((a - SOFT_CLIP_KNEE) / span).tanh()).copysign(x)
}

/// The mixer. Everything is allocated in [`Mixer::new`]; triggering,
/// seeking and rendering allocate nothing.
#[derive(Clone, Debug)]
pub struct Mixer {
    live: Slots,
    /// A seek's voices until their frame is reached.
    parked: Slots,
    seek: Seek,
    /// Starts not yet applied, latest first (the next one is last).
    pending: Vec<Voice>,
    /// Gain over the fade, from just below 1 down to 0.
    fade: Vec<f32>,
    /// First output frame not yet rendered.
    mixed_to: u64,
    seq: u64,
    volume: f32,
    /// The play's fixed gain on the sum.
    gain: f32,
    /// Samples above [`SOFT_CLIP_KNEE`] after the volume and the gain.
    saturated: u64,
    counts: Counts,
    dropped_starts: u64,
    /// The frame after the last sample (fade included) of every voice that
    /// has played to its end.
    last_end: i64,
    /// A ring-out's envelope, until everything stops.
    envelope: Option<Envelope>,
}

impl Mixer {
    /// A mixer with `voices` voice slots (and as many release slots) at
    /// `rate`, whose stolen voices fade over [`STEAL_FADE_MS`].
    #[must_use]
    pub fn new(voices: usize, rate: u32) -> Self {
        let fade_frames = fade_frames(rate);
        let fade = (1..=fade_frames)
            .map(|k| {
                let x = k as f64 / fade_frames as f64;
                (0.5 * (1.0 + (std::f64::consts::PI * x).cos())) as f32
            })
            .collect();
        Self {
            live: Slots::new(voices),
            parked: Slots::new(voices),
            seek: Seek::Idle,
            pending: Vec::with_capacity(MAX_PENDING_STARTS),
            fade,
            mixed_to: 0,
            seq: 0,
            volume: 1.0,
            gain: HEADROOM,
            saturated: 0,
            counts: Counts::default(),
            dropped_starts: 0,
            last_end: 0,
            envelope: None,
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
        self.live.voices.iter().filter(|v| v.active).count()
    }

    /// Voice slots.
    pub(crate) fn voices(&self) -> usize {
        self.live.voices.len()
    }

    /// Voices, fading releases, pending starts and a seek's waiting voices
    /// still to sound.
    #[must_use]
    pub fn sounding(&self) -> usize {
        // The parked slots are idle without a seek waiting.
        self.live.sounding() + self.pending.len() + self.parked.sounding()
    }

    /// Voices stolen so far because every voice was busy.
    #[must_use]
    pub fn steals(&self) -> u64 {
        self.counts.steals
    }

    /// Voices and releases stopped so far by a start of their own sound id.
    #[must_use]
    pub fn cuts(&self) -> u64 {
        self.counts.cuts
    }

    /// Fading releases cut off because every release slot was busy.
    #[must_use]
    pub fn hard_cuts(&self) -> u64 {
        self.counts.hard_cuts
    }

    /// The output frame after the last sample, fade included, of every
    /// voice that has played to its end: where the sum fell silent once
    /// nothing sounds. Voices stopped otherwise (a cut, a seek, a ring-out's
    /// end) do not move it; the loudness analysis, which reads it, stops
    /// none.
    #[must_use]
    pub fn last_end(&self) -> u64 {
        u64::try_from(self.last_end).unwrap_or(0)
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
        self.pending.len() < MAX_PENDING_STARTS
    }

    /// Output samples so far whose level, after the volume and the gain,
    /// was above [`SOFT_CLIP_KNEE`] and so saturated.
    #[must_use]
    pub fn saturated(&self) -> u64 {
        self.saturated
    }

    /// Sets the master volume, applied with the gain before the soft
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

    /// Sets the play's fixed gain on the sum, applied with the volume
    /// before the soft clipper: [`HEADROOM`] until set.
    ///
    /// # Errors
    ///
    /// [`InvalidGain`] for a gain not above 0 or above [`MAX_GAIN`], which
    /// is not applied.
    pub fn set_gain(&mut self, gain: f32) -> Result<(), InvalidGain> {
        self.gain = Gain::new(gain)?.get();
        Ok(())
    }

    /// The play's fixed gain on the sum.
    #[must_use]
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Stops every voice, release, pending start and seek (a retry), and
    /// takes a ring-out's envelope off: the output is at full gain again.
    pub fn stop_all(&mut self) {
        self.silence();
        self.envelope = None;
    }

    /// Stops every voice, release, pending start and seek.
    fn silence(&mut self) {
        self.live.stop();
        self.parked.stop();
        self.seek = Seek::Idle;
        self.pending.clear();
    }

    /// Rings the output out: from output frame `fade` the gain falls
    /// linearly per frame, from 1 to 0 at frame `end` (a `fade` after `end`
    /// counts as `end`), where every voice stops, and no start is put at or
    /// after `end`. Frames already rendered keep what they had. Replaces an
    /// earlier ring-out; [`stop_all`](Self::stop_all) takes it off.
    pub fn ring_out(&mut self, fade: i64, end: i64) {
        self.envelope = Some(Envelope {
            fade: fade.min(end),
            end,
            ended: false,
        });
    }

    /// Whether a ring-out's end frame has been rendered: everything stopped
    /// there and nothing starts any more.
    #[must_use]
    pub fn rung_out(&self) -> bool {
        self.envelope.is_some_and(|e| e.ended)
    }

    /// Schedules sound `id` of `bank` to start at output frame `at`, or at
    /// the first frame not yet rendered if `at` is already rendered, and
    /// returns that frame. When the start is reached, the voice or release
    /// still playing `id` stops there without a fade; otherwise, when every
    /// voice is busy, the oldest playing voice (earliest start, then
    /// earliest trigger) is stolen and fades out from it, or is dropped if
    /// it starts there too. Returns `None` if the bank holds no sound for
    /// `id`, [`MAX_PENDING_STARTS`] are waiting, or the start falls at or
    /// after a ring-out's end. While a placed seek waits to enter, no start
    /// is put before its frame.
    pub fn trigger(&mut self, bank: &SoundBank, id: SoundId, at: u64) -> Option<u64> {
        let sound = bank.slot(id)?;
        let mut start = at.max(self.mixed_to);
        if let Seek::Placed { from, .. } = self.seek {
            // The seek's voices replace whatever starts before them.
            start = start.max(u64::try_from(from).unwrap_or(0));
        }
        if self.envelope.is_some_and(|e| signed(start) >= e.end) {
            return None;
        }
        if self.pending.len() >= MAX_PENDING_STARTS {
            self.dropped_starts += 1;
            return None;
        }
        self.seq += 1;
        let voice = Voice {
            active: true,
            id,
            sound,
            start: signed(start),
            seq: self.seq,
            cut: i64::MAX,
        };
        let key = (voice.start, self.seq);
        let i = self.pending.partition_point(|p| (p.start, p.seq) > key);
        self.pending.insert(i, voice);
        Some(start)
    }

    /// Stops everything and parks a seek's voices and releases, sounding at
    /// frame `at` of the seek's frame line, with `seq` the last trigger
    /// order they used. Nothing sounds until [`place`](Self::place) puts
    /// `at` on the output. Copies at most the voice slots; slots beyond the
    /// mixer's are ignored (the engine checks the counts first).
    pub(crate) fn park(&mut self, slots: &Slots, seq: u64, at: i64) {
        self.stop_all();
        for (p, s) in self.parked.all_mut().zip(slots.all()) {
            *p = *s;
        }
        self.seq = self.seq.max(seq);
        self.seek = Seek::Parked { at };
    }

    /// Puts the frame a seek parked at output frame `frame`: its voices
    /// enter there, each partway into its sound or fade as far as that
    /// frame is past its start, or at the first frame not yet rendered if
    /// `frame` is already rendered. Until they enter, no start is put
    /// before that frame. Starts pending before it still apply, and what
    /// they start stops there. Does nothing without a parked seek.
    pub(crate) fn place(&mut self, frame: i64) {
        if let Seek::Parked { at } = self.seek {
            self.seek = Seek::Placed {
                from: frame.max(signed(self.mixed_to)),
                by: frame.saturating_sub(at),
            };
        }
    }

    /// Replaces every voice and release with the seek's, moved `by` frames.
    fn enter(&mut self, by: i64) {
        for (v, p) in self.live.all_mut().zip(self.parked.all_mut()) {
            *v = p.shifted(by);
            p.active = false;
        }
        self.seek = Seek::Idle;
    }

    /// Adds every voice and release into `out` (whose first frame is
    /// `block_start`) over frames `from..to`. Voices that end by `to` are
    /// freed.
    fn mix(&mut self, bank: &SoundBank, out: &mut [f32], block_start: i64, from: i64, to: i64) {
        let fade = &self.fade;
        let mut last_end = self.last_end;
        for v in self.live.all_mut().filter(|v| v.active) {
            if v.start >= to {
                continue;
            }
            let Some(sound) = bank.stored(v.sound) else {
                v.active = false;
                continue;
            };
            let first = v.start.max(from);
            let src_end = v.end(sound, fade.len());
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
                last_end = last_end.max(src_end);
            }
        }
        self.last_end = last_end;
    }

    /// Renders the next `out.len() / 2` frames into interleaved stereo
    /// `out` (overwritten), starting at [`Mixer::mixed_to`]: every voice
    /// summed, with each pending start applied and a placed seek's voices
    /// entered when their frame is reached (the seek first), then the
    /// volume, the gain and [`soft_clip`], then a ring-out's envelope.
    /// Every voice stops at a ring-out's end frame.
    pub fn render(&mut self, bank: &SoundBank, out: &mut [f32]) {
        let (block_start, block_end) = self.sum(bank, out);
        let gain = self.volume * self.gain;
        for x in out.iter_mut() {
            let y = *x * gain;
            if y.abs() > SOFT_CLIP_KNEE {
                self.saturated += 1;
            }
            *x = soft_clip(y);
        }
        if let Some(e) = self.envelope.filter(|e| e.fade < block_end) {
            for (f, frame) in (block_start..).zip(out.as_chunks_mut::<CHANNELS>().0) {
                let g = e.gain(f);
                for x in frame {
                    *x *= g;
                }
            }
        }
    }

    /// The next `out.len() / 2` frames as [`render`](Self::render) sums
    /// them, into interleaved stereo `out` (overwritten): every voice at
    /// unity gain, with pending starts, a placed seek and a ring-out's end
    /// applied, before the volume, the gain, the soft clipper and a
    /// ring-out's fade. The saturation count is not touched. What the
    /// loudness analysis measures; it allocates nothing.
    pub fn mix_raw(&mut self, bank: &SoundBank, out: &mut [f32]) {
        self.sum(bank, out);
    }

    /// The voice sum of the next block into `out` (overwritten), which ends
    /// the block: returns its first frame and the frame after it.
    fn sum(&mut self, bank: &SoundBank, out: &mut [f32]) -> (i64, i64) {
        out.fill(0.0);
        let block_start = signed(self.mixed_to);
        let block_end = block_start + (out.len() / CHANNELS) as i64;
        let mut pos = block_start;
        loop {
            let next = self.pending.last().filter(|p| p.start < block_end).copied();
            let seek = match self.seek {
                Seek::Placed { from, by } if from < block_end => Some((from, by)),
                _ => None,
            };
            // A ring-out's end in this block, not yet reached.
            let end = self
                .envelope
                .filter(|e| !e.ended && e.end < block_end)
                .map(|e| e.end.max(pos));
            let until = next
                .map_or(block_end, |p| p.start)
                .min(seek.map_or(block_end, |(from, _)| from))
                .min(end.unwrap_or(block_end));
            if until > pos {
                self.mix(bank, out, block_start, pos, until);
                pos = until;
            }
            if let Some(end) = end
                && end <= until
            {
                // Everything stops at the envelope's zero, and nothing
                // starts after it.
                self.silence();
                if let Some(e) = &mut self.envelope {
                    e.ended = true;
                }
                continue;
            }
            if let Some((from, by)) = seek
                && from <= until
            {
                self.enter(by);
                continue;
            }
            let Some(voice) = next else { break };
            self.pending.pop();
            self.live.apply(voice, &mut self.counts);
        }
        self.mixed_to = block_end as u64;
        (block_start, block_end)
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
            let y = soft_clip(x);
            for y in [y, -soft_clip(-x)] {
                assert!(y > SOFT_CLIP_KNEE && y <= 1.0, "{x}: {y}");
                assert!(y >= prev, "{x}: {y} < {prev}");
            }
            prev = y;
            reached_full_scale |= y == 1.0;
        }
        assert!(reached_full_scale);
        assert_eq!(soft_clip(f32::MAX), 1.0);
        assert_eq!(soft_clip(f32::NEG_INFINITY), -1.0);
        assert_eq!(soft_clip(f32::NAN), 0.0);
        assert_eq!(soft_clip(-f32::NAN), 0.0);
    }

    #[test]
    fn a_fade_of_zero_frames_still_steals() {
        // Rate 400 Hz: 2 ms is 0.8 frames, floored to no fade at all.
        let sound = || crate::Sound::mono(vec![8_192; 4]);
        let bank = SoundBank::from_sounds(400, vec![sound(), sound()]);
        let mut m = Mixer::new(1, 400);
        m.trigger(&bank, SoundId(0), 0).unwrap();
        m.trigger(&bank, SoundId(1), 2).unwrap();
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
