//! P1 test (g): offline pilot detector.

use spike_core::pilot::{
    Detector, PILOT_AMPLITUDE, PILOT_HZ, PilotOscillator, SampleEpisode, calibrate,
};
use spike_core::rng::SplitMix64;
use std::f64::consts::TAU;

const RATES: [u32; 3] = [44_100, 48_000, 96_000];

fn pilot(n: usize, rate: u32, phase: f64) -> f32 {
    (PILOT_AMPLITUDE * (TAU * f64::from(PILOT_HZ) * n as f64 / f64::from(rate) + phase).sin())
        as f32
}

/// Key-sound stand-in: decaying partials at 50 Hz – 16 kHz retriggered at
/// random with a 2 ms attack, plus a steady 16 kHz tone (the band edge) at
/// −12 dBFS faded in over 10 ms, pitched by a ±100 ppm triangle ratio
/// schedule stepped every 500 ms (as the resampler is). The pilot is added
/// after, at its exact frequency. Onsets are ramps because band-limited key
/// sounds have no steps; see `g_small_steps_do_not_trigger` for clicks.
struct Sounds {
    rng: SplitMix64,
    rate: f64,
    osc: Vec<Osc>,
    edge: Osc,
    n: u64,
    ratio: f64,
}

struct Osc {
    re: f64,
    im: f64,
    step: (f64, f64),
    freq: f64,
    amp: f64,
    peak: f64,
    attack: f64,
    decay: f64,
    left: u64,
}

impl Osc {
    fn set_freq(&mut self, freq: f64, ratio: f64, rate: f64) {
        self.freq = freq;
        let w = TAU * freq * ratio / rate;
        self.step = (w.cos(), w.sin());
    }

    fn next(&mut self) -> f64 {
        let (c, s) = self.step;
        let (re, im) = (self.re * c - self.im * s, self.re * s + self.im * c);
        self.re = re;
        self.im = im;
        if self.amp < self.peak {
            self.amp = (self.amp + self.attack).min(self.peak);
        } else {
            self.amp *= self.decay;
            self.peak = 0.0;
        }
        im * self.amp
    }
}

impl Sounds {
    fn new(rate: u32, seed: u64) -> Self {
        let rate = f64::from(rate);
        let mk = || Osc {
            re: 1.0,
            im: 0.0,
            step: (1.0, 0.0),
            freq: 0.0,
            amp: 0.0,
            peak: 0.0,
            attack: 0.0,
            decay: 1.0,
            left: 0,
        };
        let mut edge = mk();
        edge.set_freq(16_000.0, 1.0, rate);
        edge.peak = 0.25;
        edge.attack = 0.25 / (0.01 * rate);
        Self {
            rng: SplitMix64::new(seed),
            rate,
            osc: (0..12).map(|_| mk()).collect(),
            edge,
            n: 0,
            ratio: 1.0,
        }
    }

    fn next(&mut self) -> f32 {
        let step = (self.rate / 2.0) as u64;
        if self.n.is_multiple_of(step) {
            // Triangle, 20 s period, ±100 ppm.
            let t = (self.n as f64 / self.rate) % 20.0;
            let tri = if t < 10.0 {
                t / 5.0 - 1.0
            } else {
                3.0 - t / 5.0
            };
            self.ratio = 1.0 + 100e-6 * tri;
            for o in self.osc.iter_mut().chain(std::iter::once(&mut self.edge)) {
                let f = o.freq;
                o.set_freq(f, self.ratio, self.rate);
            }
        }
        self.n += 1;
        let mut x = self.edge.next();
        for i in 0..self.osc.len() {
            if self.osc[i].left == 0 {
                let f = 50.0 + self.rng.unit_f64() * 15_950.0;
                let o = &mut self.osc[i];
                o.set_freq(f, self.ratio, self.rate);
                o.amp = 0.0;
                o.peak = 0.04 + 0.03 * self.rng.unit_f64();
                o.attack = o.peak / (0.002 * self.rate);
                o.decay = (-1.0 / (0.3 * self.rate)).exp();
                o.left = (self.rate * (0.05 + 0.45 * self.rng.unit_f64())) as u64;
                let (re, im) = (o.re, o.im);
                let m = (re * re + im * im).sqrt();
                o.re = re / m;
                o.im = im / m;
            }
            self.osc[i].left -= 1;
            x += self.osc[i].next();
        }
        x as f32
    }
}

#[test]
fn g_no_false_positives_over_600_s_with_sounds_and_ratio_sweep() {
    let rate = 48_000;
    let mut osc = PilotOscillator::new(rate);
    // Countdown: pilot only.
    let countdown: Vec<f32> = (0..rate * 2).map(|_| osc.next_sample()).collect();
    let cal = calibrate(&countdown, rate).expect("calibrates");
    let mut det = Detector::new(rate, cal);
    det.push(&countdown);
    let mut sounds = Sounds::new(rate, 7);
    let mut buf = vec![0.0f32; 480];
    let mut peak = 0.0f32;
    for _ in 0..(600 * rate as usize / buf.len()) {
        for x in &mut buf {
            *x = sounds.next() + osc.next_sample();
            peak = peak.max(x.abs());
        }
        det.push(&buf);
    }
    assert!(
        peak > 0.5,
        "sounds too quiet to be a meaningful test: {peak}"
    );
    assert!(det.windows() > 600 * 4000);
    assert_eq!(det.finish(), Vec::<SampleEpisode>::new());
}

/// Abrupt steps of 0.05 (a stolen voice cut off mid-sound at a typical
/// per-voice level) are broadband clicks but stay far from a bad window.
#[test]
fn g_small_steps_do_not_trigger() {
    for rate in RATES {
        let r = rate as usize;
        let cal = calibrate(
            &(0..r).map(|n| pilot(n, rate, 0.0)).collect::<Vec<_>>(),
            rate,
        )
        .unwrap();
        let mut rng = SplitMix64::new(rate.into());
        let mut level = 0.0f32;
        let s: Vec<f32> = (0..r * 10)
            .map(|n| {
                if n % (r / 10) == r / 20 {
                    level = match (level == 0.0, rng.below(2)) {
                        (true, 0) => 0.05,
                        (true, _) => -0.05,
                        _ => 0.0,
                    };
                }
                pilot(n, rate, 0.0) + level
            })
            .collect();
        let mut det = Detector::new(rate, cal);
        det.push(&s);
        assert_eq!(det.finish(), Vec::<SampleEpisode>::new(), "rate {rate}");
    }
}

#[test]
fn g_detects_1_to_20_ms_gaps_at_every_offset_phase_and_rate_as_one_episode() {
    for rate in RATES {
        let r = rate as usize;
        let lead = r * 15 / 1000;
        let cal_signal: Vec<f32> = (0..r / 2).map(|n| pilot(n, rate, 0.0)).collect();
        let cal = calibrate(&cal_signal, rate).expect("calibrates");
        let probe = Detector::new(rate, cal);
        let (win, hop) = (probe.window_len(), probe.hop_len());
        for gap_ms in 1..=20usize {
            let gap = (gap_ms * r).div_ceil(1000);
            for offset in 0..win {
                for phase in [0.0, 1.1, 2.6, 4.4] {
                    let start = lead + offset;
                    let total = start + gap + lead;
                    let mut s: Vec<f32> = (0..total).map(|n| pilot(n, rate, phase)).collect();
                    s[start..start + gap].fill(0.0);
                    let mut det = Detector::new(rate, cal);
                    det.push(&s);
                    let eps = det.finish();
                    let ctx = format!(
                        "rate {rate} gap {gap_ms} ms offset {offset} phase {phase}: {eps:?}"
                    );
                    assert_eq!(eps.len(), 1, "{ctx}");
                    let e = eps[0];
                    // At the injected time, within one window.
                    let (gs, ge) = (start as i64, (start + gap) as i64);
                    assert!(
                        (e.start_sample as i64 - gs).abs() <= win as i64,
                        "start {ctx}"
                    );
                    assert!(
                        (e.end_sample as i64 - ge).abs() <= (win + hop) as i64,
                        "end {ctx}"
                    );
                }
            }
        }
    }
}

#[test]
fn g_dropped_samples_without_silence_are_caught_by_the_phase_step() {
    let rate = 48_000;
    let cal_signal: Vec<f32> = (0..24_000).map(|n| pilot(n, rate, 0.0)).collect();
    let cal = calibrate(&cal_signal, rate).unwrap();
    // Skip 3 samples (a 1.18 rad pilot phase jump) without any silence.
    let s: Vec<f32> = (0..9_600)
        .map(|n| if n < 4_800 { n } else { n + 3 })
        .map(|n| pilot(n, rate, 0.0))
        .collect();
    let mut det = Detector::new(rate, cal);
    det.push(&s);
    assert_eq!(det.finish().len(), 1);
}

#[test]
fn g_streaming_push_matches_one_shot() {
    let rate = 44_100;
    let cal = calibrate(
        &(0..44_100).map(|n| pilot(n, rate, 0.3)).collect::<Vec<_>>(),
        rate,
    )
    .unwrap();
    let mut s: Vec<f32> = (0..44_100).map(|n| pilot(n, rate, 0.3)).collect();
    for k in 1..4 {
        let a = k * 10_000;
        s[a..a + 100].fill(0.0);
    }
    let mut one = Detector::new(rate, cal);
    one.push(&s);
    let mut chunked = Detector::new(rate, cal);
    for c in s.chunks(441) {
        chunked.push(c);
    }
    let a = one.finish();
    assert_eq!(a.len(), 3);
    assert_eq!(a, chunked.finish());
}
