//! Mixer scheduling, voice stealing and queue-overflow accounting (design
//! D2 audio, D4).
//!
//! Sound commands carry an absolute output sample; a voice starts at that
//! exact sample whatever the block size. Voices are preallocated (cap 256);
//! when all are busy the oldest is stolen and counted. A stolen voice is
//! not cut off: it moves to a preallocated release slot and fades out over
//! `fade_frames` (raised cosine), because an abrupt cut is a broadband
//! click, and several stolen at one sample by a chord add up to a step that
//! reaches the 19 kHz pilot band (tests/pilot.rs). Only when
//! every release slot is busy is the most-faded release cut, counted in
//! `hard_cuts`. `mix` does not allocate.

/// Voice cap (design D2).
pub const MAX_VOICES: usize = 256;
/// Fade applied to a stolen voice.
pub const STEAL_FADE_MS: u32 = 2;

/// Mono i16 key sounds, generated before play.
#[derive(Clone, Debug, Default)]
pub struct SoundBank {
    pub sounds: Vec<Vec<i16>>,
}

/// Maps the judgment timeline (µs) to output sample positions.
#[derive(Clone, Copy, Debug)]
pub struct SampleClock {
    /// A µs time and the output sample that plays at it.
    pub anchor_us: i64,
    pub anchor_sample: u64,
    pub rate_hz: u32,
}

impl SampleClock {
    /// Output sample for `t_us` (floor), saturating at 0.
    pub fn sample_at(&self, t_us: i64) -> u64 {
        let d = (i128::from(t_us) - i128::from(self.anchor_us)) * i128::from(self.rate_hz);
        let s = i128::from(self.anchor_sample) + d.div_euclid(1_000_000);
        s.max(0) as u64
    }
}

/// Target sample for a sound triggered by an event at `event_us`: the event
/// time plus a fixed scheduling delay (resampler delay plus one period), but
/// never before `earliest`, the first sample not yet handed to the device.
pub fn schedule(event_us: i64, clock: &SampleClock, delay_samples: u64, earliest: u64) -> u64 {
    (clock.sample_at(event_us) + delay_samples).max(earliest)
}

#[derive(Clone, Copy, Debug, Default)]
struct Voice {
    active: bool,
    sound: u16,
    start: u64,
    seq: u64,
    gain: f32,
    /// Fade-out start for a released voice; `u64::MAX` while playing.
    cut: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Started { voice: usize },
    Stole { voice: usize },
}

#[derive(Clone, Debug)]
pub struct Mixer {
    voices: Vec<Voice>,
    releases: Vec<Voice>,
    /// Gain over the fade, from just below 1 down to 0.
    fade: Vec<f32>,
    /// First output sample not yet mixed.
    mixed_to: u64,
    seq: u64,
    pub steals: u64,
    pub hard_cuts: u64,
}

impl Mixer {
    /// `fade_frames` is the stolen-voice fade length (`STEAL_FADE_MS` at the
    /// device rate); 0 cuts stolen voices off.
    pub fn new(cap: usize, fade_frames: usize) -> Self {
        let fade = (1..=fade_frames)
            .map(|k| {
                let x = k as f64 / fade_frames as f64;
                (0.5 * (1.0 + (std::f64::consts::PI * x).cos())) as f32
            })
            .collect();
        Self {
            voices: vec![Voice::default(); cap],
            releases: vec![Voice::default(); if fade_frames > 0 { cap } else { 0 }],
            fade,
            mixed_to: 0,
            seq: 0,
            steals: 0,
            hard_cuts: 0,
        }
    }

    pub fn active(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }

    /// Starts `sound` at output sample `start`, stealing the oldest voice
    /// (earliest start, then earliest trigger) when all are busy.
    pub fn trigger(&mut self, sound: u16, start: u64, gain: f32) -> Trigger {
        self.seq += 1;
        let voice = Voice {
            active: true,
            sound,
            start,
            seq: self.seq,
            gain,
            cut: u64::MAX,
        };
        if let Some(i) = self.voices.iter().position(|v| !v.active) {
            self.voices[i] = voice;
            return Trigger::Started { voice: i };
        }
        let i = (0..self.voices.len())
            .min_by_key(|&i| (self.voices[i].start, self.voices[i].seq))
            .expect("mixer has voices");
        let victim = std::mem::replace(&mut self.voices[i], voice);
        self.steals += 1;
        self.release(victim, start.max(self.mixed_to));
        Trigger::Stole { voice: i }
    }

    /// Moves a stolen voice to a release slot fading out from `cut`.
    fn release(&mut self, victim: Voice, cut: u64) {
        if self.releases.is_empty() {
            return;
        }
        let slot = match self.releases.iter().position(|r| !r.active) {
            Some(k) => k,
            None => {
                self.hard_cuts += 1;
                (0..self.releases.len())
                    .min_by_key(|&k| self.releases[k].cut)
                    .expect("release slots exist")
            }
        };
        self.releases[slot] = Voice { cut, ..victim };
    }

    /// Adds every voice into interleaved stereo `out`, whose first frame is
    /// output sample `block_start`. Voices that end are freed.
    pub fn mix(&mut self, bank: &SoundBank, block_start: u64, out: &mut [f32]) {
        let frames = (out.len() / 2) as u64;
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
            let Some(data) = bank.sounds.get(usize::from(v.sound)) else {
                v.active = false;
                continue;
            };
            let first = v.start.max(block_start);
            let src_end =
                (v.start + data.len() as u64).min(v.cut.saturating_add(fade.len() as u64));
            let last = src_end.min(block_end);
            for s in first..last {
                let g = if s < v.cut {
                    v.gain
                } else {
                    v.gain * fade[(s - v.cut) as usize]
                };
                let x = f32::from(data[(s - v.start) as usize]) / 32768.0 * g;
                let o = ((s - block_start) * 2) as usize;
                out[o] += x;
                out[o + 1] += x;
            }
            if src_end <= block_end {
                v.active = false;
            }
        }
        self.mixed_to = self.mixed_to.max(block_end);
    }
}

/// Queues whose overflow is counted (design D4: never silent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Queue {
    Input,
    SoundCommand,
    BgaPool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OverflowCounters {
    pub input: u64,
    pub sound_command: u64,
    pub bga_pool: u64,
}

impl OverflowCounters {
    /// Records the outcome of a push; a full queue increments its counter.
    pub fn record<T, E>(&mut self, queue: Queue, pushed: Result<T, E>) -> Option<T> {
        match pushed {
            Ok(v) => Some(v),
            Err(_) => {
                match queue {
                    Queue::Input => self.input += 1,
                    Queue::SoundCommand => self.sound_command += 1,
                    Queue::BgaPool => self.bga_pool += 1,
                }
                None
            }
        }
    }

    /// Any input or sound-command overflow invalidates the run (V5); an
    /// exhausted BGA pool only skips frames, counted against G7.
    pub fn invalidates_run(&self) -> bool {
        self.input > 0 || self.sound_command > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bank() -> SoundBank {
        SoundBank {
            sounds: (0..4)
                .map(|k| (1..=300).map(|i| (i * (k + 1)) as i16).collect())
                .collect(),
        }
    }

    fn render(block: usize, triggers: &[(u16, u64)], total: usize) -> Vec<f32> {
        let bank = bank();
        let mut m = Mixer::new(8, 96);
        for &(s, at) in triggers {
            m.trigger(s, at, 1.0);
        }
        let mut out = vec![0.0f32; total * 2];
        let mut pos = 0;
        while pos < total {
            let n = block.min(total - pos);
            m.mix(&bank, pos as u64, &mut out[pos * 2..(pos + n) * 2]);
            pos += n;
        }
        out
    }

    // (h) A scheduled sound starts at its exact sample whatever the block size.
    #[test]
    fn scheduled_sound_starts_at_exact_sample_for_any_block_size() {
        let triggers = [(0u16, 1_000u64), (1, 1_037), (2, 2_399), (3, 0)];
        let reference = render(1, &triggers, 3_000);
        for block in [2, 7, 64, 441, 480, 1024, 3000] {
            assert_eq!(render(block, &triggers, 3_000), reference, "block {block}");
        }
        // Sound 0 alone: silent before 1000, its first sample exactly at 1000.
        let solo = render(480, &[(0, 1_000)], 1_400);
        assert!(solo[..2_000].iter().all(|&x| x == 0.0));
        assert_eq!(solo[2_000], 1.0 / 32768.0);
        assert_eq!(solo[2 * 1_299], 300.0 / 32768.0);
        assert_eq!(solo[2 * 1_300], 0.0);
    }

    // (h) When all voices are busy the oldest one is stolen and counted.
    #[test]
    fn steal_picks_oldest_and_counts() {
        let mut m = Mixer::new(3, 96);
        assert_eq!(m.trigger(0, 30, 1.0), Trigger::Started { voice: 0 });
        assert_eq!(m.trigger(0, 10, 1.0), Trigger::Started { voice: 1 });
        assert_eq!(m.trigger(0, 20, 1.0), Trigger::Started { voice: 2 });
        assert_eq!(m.trigger(1, 40, 1.0), Trigger::Stole { voice: 1 });
        assert_eq!(m.trigger(1, 40, 1.0), Trigger::Stole { voice: 2 });
        // Same start: the earlier trigger is older.
        assert_eq!(m.trigger(1, 50, 1.0), Trigger::Stole { voice: 0 });
        assert_eq!(m.trigger(1, 60, 1.0), Trigger::Stole { voice: 1 });
        assert_eq!(m.steals, 4);
        assert_eq!(m.active(), 3);
    }

    #[test]
    fn stolen_voice_fades_instead_of_cutting_off() {
        let bank = SoundBank {
            sounds: vec![vec![16_000; 10_000], vec![0; 10_000]],
        };
        let run = |fade: usize| {
            let mut m = Mixer::new(1, fade);
            m.trigger(0, 0, 1.0);
            let mut out = vec![0.0f32; 8_000];
            m.mix(&bank, 0, &mut out[..2_000]);
            // Steals the playing voice at sample 1000.
            assert!(matches!(m.trigger(1, 1_000, 1.0), Trigger::Stole { .. }));
            m.mix(&bank, 1_000, &mut out[2_000..]);
            let left: Vec<f32> = out.iter().step_by(2).copied().collect();
            let max_step = left
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0, f32::max);
            (left, max_step)
        };
        let full = 16_000.0 / 32768.0;
        let (left, step) = run(96);
        assert_eq!(left[999], full);
        assert!(left[1_000] > 0.99 * full && left[1_000] < full);
        assert_eq!(left[1_095], 0.0);
        // Raised cosine: steepest step is π/2 of a linear one.
        assert!(
            step <= full * std::f32::consts::FRAC_PI_2 / 96.0 * 1.01,
            "{step}"
        );
        let (left, step) = run(0);
        assert_eq!(left[1_000], 0.0);
        assert_eq!(step, full);
    }

    #[test]
    fn exhausted_release_slots_are_counted_as_hard_cuts() {
        let mut m = Mixer::new(2, 96);
        for k in 0..6 {
            m.trigger(0, k, 1.0);
        }
        // 4 steals into 2 release slots: the last 2 overwrite releases.
        assert_eq!((m.steals, m.hard_cuts), (4, 2));
    }

    #[test]
    fn schedule_converts_exactly_and_never_schedules_in_the_past() {
        let c = SampleClock {
            anchor_us: 1_000_000,
            anchor_sample: 48_000,
            rate_hz: 48_000,
        };
        assert_eq!(c.sample_at(1_000_000), 48_000);
        assert_eq!(c.sample_at(1_000_021), 48_001);
        assert_eq!(c.sample_at(999_979), 47_998);
        assert_eq!(schedule(2_000_000, &c, 480, 0), 96_480);
        assert_eq!(schedule(2_000_000, &c, 480, 100_000), 100_000);
    }

    #[test]
    fn overflow_counts_and_only_input_or_sound_invalidates() {
        let mut o = OverflowCounters::default();
        assert_eq!(o.record::<_, ()>(Queue::Input, Ok(5)), Some(5));
        assert!(!o.invalidates_run());
        assert_eq!(o.record::<(), _>(Queue::BgaPool, Err(())), None);
        assert_eq!(o.bga_pool, 1);
        assert!(!o.invalidates_run());
        o.record::<(), _>(Queue::SoundCommand, Err(()));
        assert!(o.invalidates_run());
        let mut o = OverflowCounters::default();
        o.record::<(), _>(Queue::Input, Err(()));
        assert!(o.invalidates_run());
    }
}
