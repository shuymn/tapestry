//! The audio thread's signal path (design D2 audio, D3, D5), without the
//! device: key-sound mixer → drift-correction resampler (rubato async sinc,
//! ±100 ppm triangle) → master clamp → 19 kHz pilot on the left channel →
//! optional injected gaps. `render` fills device frames; everything is
//! preallocated in `new`, so rendering never allocates.
//!
//! Two timelines: the mixer's (input of the resampler, where sounds are
//! scheduled) and the device's (output frames, where the pilot and the
//! injected gaps live). Frame `d` of the device timeline carries mixer
//! frame `d - device_minus_mixer()` (within a frame: the ratio deviates by
//! at most 100 ppm, its integral over a 10 s half-period stays below
//! 0.5 ms, and the offset is re-read every period).

use crate::mixer::{MAX_VOICES, Mixer, STEAL_FADE_MS, SoundBank};
use crate::pilot::{PILOT_AMPLITUDE, PilotOscillator};
use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

/// Peak deviation of the drift-correction ratio.
pub const DRIFT_PPM: f64 = 100.0;
/// Triangle period of the ratio schedule.
pub const DRIFT_PERIOD_S: f64 = 20.0;
/// The ratio changes in steps this far apart.
pub const DRIFT_STEP_MS: u64 = 500;
/// Gain of each key-sound voice. Sounds peak at −6 dBFS; with the chart's
/// 100–256 concurrent voices this keeps the sum below the clamp (`clamped`
/// counts any sample that reaches it, since a clipped peak is broadband).
pub const VOICE_GAIN: f32 = 0.05;
/// The key-sound mix is clamped here, leaving room for the pilot.
const MIX_LIMIT: f32 = 1.0 - PILOT_AMPLITUDE as f32;

/// Relative ratio of the schedule at device frame `frame`: a ±100 ppm
/// triangle with a 20 s period, held for 500 ms steps.
pub fn drift_ratio(frame: u64, rate_hz: u32) -> f64 {
    let step_frames = u64::from(rate_hz) * DRIFT_STEP_MS / 1000;
    let t = (frame / step_frames * step_frames) as f64 / f64::from(rate_hz) % DRIFT_PERIOD_S;
    let q = DRIFT_PERIOD_S / 4.0;
    let tri = if t < 2.0 * q {
        t / q - 1.0
    } else {
        3.0 - t / q
    };
    1.0 + DRIFT_PPM * 1e-6 * tri
}

/// Gaps of silence written into the device output after the pilot
/// (`--inject-gap-every-s N --inject-gap-ms M`): `len` frames starting at
/// device frames `first`, `first + every`, … for `count` gaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Injection {
    pub first: u64,
    pub every: u64,
    pub len: u64,
    pub count: u64,
}

impl Injection {
    /// Start frame of gap `k`.
    pub fn start(&self, k: u64) -> u64 {
        self.first + k * self.every
    }

    fn silent(&self, frame: u64) -> bool {
        frame >= self.first && {
            let k = (frame - self.first) / self.every;
            k < self.count && frame - self.start(k) < self.len
        }
    }
}

pub struct Engine {
    rate_hz: u32,
    mixer: Mixer,
    bank: SoundBank,
    resampler: Async<f32>,
    /// Interleaved stereo resampler input (mixer frames).
    mix: Vec<f32>,
    /// Interleaved stereo resampler output and how much of it is used.
    stage: Vec<f32>,
    staged: usize,
    stage_pos: usize,
    pilot: PilotOscillator,
    ratio: f64,
    /// Mixer frames produced, device frames produced.
    mixed: u64,
    out: u64,
    injection: Option<Injection>,
    /// Device frames before this one are silent (pilot included).
    pilot_start: u64,
    /// Mixed samples that reached the clamp.
    pub clamped: u64,
}

impl Engine {
    /// `chunk` is the resampler's fixed output size (the device period).
    pub fn new(rate_hz: u32, chunk: usize, bank: SoundBank, injection: Option<Injection>) -> Self {
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: None,
            oversampling_factor: 128,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
        };
        let resampler = Async::<f32>::new_sinc(
            1.0,
            1.0 + 2.0 * DRIFT_PPM * 1e-6,
            &params,
            chunk,
            2,
            FixedAsync::Output,
        )
        .expect("resampler parameters are valid");
        let fade = (rate_hz * STEAL_FADE_MS / 1000) as usize;
        Self {
            rate_hz,
            mixer: Mixer::new(MAX_VOICES, fade),
            bank,
            mix: vec![0.0; resampler.input_frames_max() * 2],
            stage: vec![0.0; resampler.output_frames_max() * 2],
            resampler,
            staged: 0,
            stage_pos: 0,
            pilot: PilotOscillator::new(rate_hz),
            ratio: 1.0,
            mixed: 0,
            out: 0,
            injection,
            pilot_start: 0,
            clamped: 0,
        }
    }

    pub fn rate_hz(&self) -> u32 {
        self.rate_hz
    }

    /// Resampler delay in device frames.
    pub fn resampler_delay(&self) -> u64 {
        self.resampler.output_delay() as u64
    }

    /// Device frames produced so far (the next frame `render` writes).
    pub fn device_frames(&self) -> u64 {
        self.out
    }

    /// Device frame minus the mixer frame it carries (includes the
    /// resampler delay and the frames buffered between the stages).
    pub fn device_minus_mixer(&self) -> i64 {
        // Device frames already rendered or staged, against mixer frames
        // consumed, plus the filter delay.
        let produced = self.out + (self.staged - self.stage_pos) as u64;
        produced as i64 - self.mixed as i64 + self.resampler.output_delay() as i64
    }

    /// The first mixer frame not yet mixed: a sound may start no earlier.
    pub fn mixer_frames(&self) -> u64 {
        self.mixed
    }

    /// Keeps the output silent before device frame `frame`: the pilot then
    /// starts with a step at a known frame, long after the audio engine's
    /// own start-up, which aligns the stream's frames with capture
    /// positions (`pilot::OnsetFinder`).
    pub fn set_pilot_start(&mut self, frame: u64) {
        self.pilot_start = frame;
    }

    /// Sets the injected gaps (before the first gap is rendered).
    pub fn set_injection(&mut self, injection: Option<Injection>) {
        self.injection = injection;
    }

    pub fn mixer(&self) -> &Mixer {
        &self.mixer
    }

    /// Starts `sound` so it reaches the device at device frame `device`,
    /// or as soon as possible after it. Returns the device frame it starts
    /// at.
    pub fn trigger_at_device(&mut self, sound: u16, device: u64) -> u64 {
        let offset = self.device_minus_mixer();
        let want = device as i64 - offset;
        let start = want.max(self.mixed as i64) as u64;
        self.mixer.trigger(sound, start, VOICE_GAIN);
        (start as i64 + offset) as u64
    }

    /// Writes `frames` device frames to interleaved `out` with `channels`
    /// channels (left is channel 0, right channel 1, others silent).
    pub fn render(&mut self, out: &mut [f32], channels: usize, frames: usize) {
        let mut done = 0;
        while done < frames {
            if self.stage_pos == self.staged {
                self.produce();
            }
            let n = (frames - done).min(self.staged - self.stage_pos);
            for k in 0..n {
                let s = (self.stage_pos + k) * 2;
                let o = (done + k) * channels;
                out[o] = self.stage[s];
                if channels > 1 {
                    out[o + 1] = self.stage[s + 1];
                }
                for x in &mut out[o + 2.min(channels)..o + channels] {
                    *x = 0.0;
                }
            }
            self.stage_pos += n;
            self.out += n as u64;
            done += n;
        }
    }

    /// One resampler chunk into the stage, with the clamp, the pilot and
    /// the injected gaps applied.
    fn produce(&mut self) {
        let base = self.out;
        let ratio = drift_ratio(base, self.rate_hz);
        if ratio != self.ratio {
            self.resampler
                .set_resample_ratio_relative(ratio, false)
                .expect("ratio within the constructed bounds");
            self.ratio = ratio;
        }
        let need = self.resampler.input_frames_next();
        let mix = &mut self.mix[..need * 2];
        mix.fill(0.0);
        self.mixer.mix(&self.bank, self.mixed, mix);
        self.mixed += need as u64;
        let frames_out = self.resampler.output_frames_next();
        let input = InterleavedSlice::new(&self.mix[..need * 2], 2, need).expect("input size");
        let mut output =
            InterleavedSlice::new_mut(&mut self.stage[..frames_out * 2], 2, frames_out)
                .expect("output size");
        let (_, written) = self
            .resampler
            .process_into_buffer(&input, &mut output, None)
            .expect("resampler buffers are sized");
        for k in 0..written {
            let frame = base + k as u64;
            let (l, r) = (self.stage[2 * k], self.stage[2 * k + 1]);
            let mut clamp = |x: f32| {
                if x.abs() >= MIX_LIMIT {
                    self.clamped += 1;
                }
                x.clamp(-MIX_LIMIT, MIX_LIMIT)
            };
            let (mut l, mut r) = (clamp(l), clamp(r));
            l += self.pilot.next_sample();
            if frame < self.pilot_start || self.injection.is_some_and(|i| i.silent(frame)) {
                (l, r) = (0.0, 0.0);
            }
            self.stage[2 * k] = l;
            self.stage[2 * k + 1] = r;
        }
        self.staged = written;
        self.stage_pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_ratio_is_a_stepped_triangle() {
        let r = 48_000;
        assert_eq!(drift_ratio(0, r), 1.0 - 100e-6);
        // Held within a 500 ms step.
        assert_eq!(drift_ratio(23_999, r), drift_ratio(0, r));
        let at = |s: f64| drift_ratio((s * f64::from(r)) as u64, r);
        assert!((at(5.0) - 1.0).abs() < 1e-12);
        assert!((at(10.0) - (1.0 + 100e-6)).abs() < 1e-12);
        assert!((at(15.0) - 1.0).abs() < 1e-12);
        assert!((at(20.0) - (1.0 - 100e-6)).abs() < 1e-12);
    }

    #[test]
    fn injection_silences_exact_frames() {
        let i = Injection {
            first: 100,
            every: 1_000,
            len: 10,
            count: 2,
        };
        let silent: Vec<u64> = (0..3_000).filter(|&f| i.silent(f)).collect();
        let want: Vec<u64> = (100..110).chain(1_100..1_110).collect();
        assert_eq!(silent, want);
    }

    #[test]
    fn render_in_any_block_size_gives_the_same_samples() {
        let bank = SoundBank {
            sounds: vec![vec![16_000; 4_000]],
        };
        let run = |block: usize| {
            let mut e = Engine::new(48_000, 480, bank.clone(), None);
            e.trigger_at_device(0, 1_000);
            let mut out = vec![0.0f32; 9_600 * 3];
            let mut done = 0;
            while done < 9_600 {
                let n = block.min(9_600 - done);
                e.render(&mut out[done * 3..(done + n) * 3], 3, n);
                done += n;
            }
            out
        };
        let reference = run(480);
        for block in [1, 7, 333, 1_000] {
            assert_eq!(run(block), reference, "block {block}");
        }
        // Third channel silent; the sound arrives at device frame 1000 on
        // the right channel (no pilot there), not before.
        assert!(reference.iter().skip(2).step_by(3).all(|&x| x == 0.0));
        let right: Vec<f32> = reference.iter().skip(1).step_by(3).copied().collect();
        let half = 16_000.0 / 32_768.0 * VOICE_GAIN / 2.0;
        let first = right.iter().position(|x| x.abs() > half).unwrap();
        assert!((999..=1_001).contains(&first), "{first}");
    }
}
