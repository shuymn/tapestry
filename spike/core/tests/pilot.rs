//! P1 test (g): offline pilot detector.

use spike_core::pilot::{
    Detector, DetectorReport, PILOT_AMPLITUDE, PILOT_HZ, PacketFlags, PilotOscillator,
    SampleEpisode, SampleSpan, calibrate,
};
use spike_core::rng::SplitMix64;
use std::f64::consts::TAU;
use std::sync::OnceLock;

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
    assert_eq!(det.finish().episodes, Vec::<SampleEpisode>::new());
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
        assert_eq!(
            det.finish().episodes,
            Vec::<SampleEpisode>::new(),
            "rate {rate}"
        );
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
                    let eps = det.finish().episodes;
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
    assert_eq!(det.finish().episodes.len(), 1);
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
    assert_eq!(a.episodes.len(), 3);
    assert_eq!(a, chunked.finish());
}

/// ±100 ppm triangle (20 s period) stepped every 500 ms, as the
/// drift-correction schedule is.
fn ratio_at(n: usize, rate: f64) -> f64 {
    let t = ((n as f64 / rate) / 0.5).floor() * 0.5 % 20.0;
    let tri = if t < 10.0 {
        t / 5.0 - 1.0
    } else {
        3.0 - t / 5.0
    };
    1.0 + 100e-6 * tri
}

const COUNTDOWN_S: usize = 2;

/// Real-shaped output: the chart's notes trigger generated key sounds
/// through the core mixer (cap 256, steals with `fade` frames), the left
/// channel is resampled with stepped ratio changes (linear interpolation),
/// and the pilot is added after. Returns the samples and the steal count.
fn real_shaped(rate: u32, fade: usize) -> (Vec<f32>, u64) {
    use spike_core::chart::{CHART_LEN_US, Chart, DEFAULT_SEED};
    use spike_core::mixer::{MAX_VOICES, Mixer};
    use spike_core::sounds::key_bank;
    let r = rate as usize;
    let bank = key_bank(11, rate);
    let chart = Chart::generate(DEFAULT_SEED);
    let lead = (COUNTDOWN_S * r) as u64;
    let total = lead as usize + (CHART_LEN_US as usize / 1_000_000 + 2) * r;
    let mut mixer = Mixer::new(MAX_VOICES, fade);
    let mut left: Vec<f32> = Vec::with_capacity(total + 2);
    let mut block = vec![0.0f32; 480 * 2];
    let mut next = 0;
    let mut pos = 0u64;
    while (pos as usize) < total + 2 {
        // Triggers are sent one block ahead of the mix, as the logic thread
        // does.
        while next < chart.notes.len() {
            let n = chart.notes[next];
            let at = lead + (n.time_us as u64 * u64::from(rate)) / 1_000_000;
            if at >= pos + 960 {
                break;
            }
            mixer.trigger(n.sound, at, 0.1);
            next += 1;
        }
        block.fill(0.0);
        mixer.mix(&bank, pos, &mut block);
        left.extend(block.iter().step_by(2));
        pos += 480;
    }
    let mut osc = PilotOscillator::new(rate);
    let mut x = 0.0f64;
    let out: Vec<f32> = (0..total)
        .map(|n| {
            let i = x as usize;
            let f = (x - i as f64) as f32;
            let y = left[i] * (1.0 - f) + left[i + 1] * f;
            x += ratio_at(n, f64::from(rate));
            y + osc.next_sample()
        })
        .collect();
    (out, mixer.steals)
}

fn real_shaped_48k() -> &'static (Vec<f32>, u64) {
    static AUDIO: OnceLock<(Vec<f32>, u64)> = OnceLock::new();
    AUDIO.get_or_init(|| real_shaped(48_000, 96))
}

fn detect(rate: u32, s: &[f32]) -> DetectorReport {
    let cal = calibrate(&s[..COUNTDOWN_S * rate as usize], rate).expect("calibrates");
    let mut det = Detector::new(rate, cal);
    for c in s.chunks(480) {
        det.push(c);
    }
    det.finish()
}

#[test]
fn g_real_shaped_mix_with_steals_and_ratio_steps_has_no_episodes() {
    let (s, steals) = real_shaped_48k();
    let peak = s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!(*steals > 10_000, "steals {steals}");
    assert!(peak > 0.3 && peak < 1.0, "peak {peak}");
    let report = detect(48_000, s);
    assert_eq!(report.episodes, vec![]);
    assert_eq!(report.unverifiable, vec![]);
}

/// One steal is safe at realistic levels (a single step stays below the
/// detector's limit up to about 0.2 full scale, see `g_small_steps_do_not_trigger`), but a chord
/// steals several voices at the same sample, and their cuts add up. Eight
/// held voices summing to 0.6 stolen at once by an 8-note chord trip the
/// detector when cut abruptly; the mixer's steal fade keeps it clean.
#[test]
fn g_chord_steal_needs_the_steal_fade() {
    use spike_core::mixer::{Mixer, SoundBank};
    let rate = 48_000u32;
    let r = rate as usize;
    // A held tone-like level with a smooth 10 ms attack (no onset step).
    let held: Vec<i16> = (0..3 * r)
        .map(|n| {
            let a = (n as f64 / (0.01 * f64::from(rate))).min(1.0);
            (0.5 * (1.0 - (std::f64::consts::PI * a).cos()) * 16_000.0) as i16
        })
        .collect();
    let bank = SoundBank {
        sounds: vec![held, vec![0; r]],
    };
    let episodes = |fade: usize| {
        let mut m = Mixer::new(8, fade);
        let mut osc = PilotOscillator::new(rate);
        let mut s = Vec::with_capacity(4 * r);
        let mut block = vec![0.0f32; 960];
        for b in 0..(4 * r / 480) as u64 {
            let pos = b * 480;
            if pos == 480 * 250 {
                for _ in 0..8 {
                    m.trigger(0, pos + 480, 0.15);
                }
            }
            if pos == 480 * 300 {
                // The chord steals all 8 held voices at one sample.
                for _ in 0..8 {
                    m.trigger(1, pos + 480 + 7, 0.15);
                }
            }
            block.fill(0.0);
            m.mix(&bank, pos, &mut block);
            s.extend(block.iter().step_by(2).map(|x| x + osc.next_sample()));
        }
        detect(rate, &s).episodes.len()
    };
    assert!(episodes(0) >= 1);
    assert_eq!(episodes(2 * r / 1000), 0);
}

#[test]
fn g_injected_gaps_on_real_shaped_audio_are_each_one_episode() {
    let rate = 48_000usize;
    let (clean, _) = real_shaped_48k();
    let gap = 2 * rate / 1000;
    // Every 10 s from 5 s after the countdown: alternately replace 2 ms
    // with silence (`--inject-gap-ms`) and insert 2 ms of silence, which
    // delays the rest (a starved device).
    let mut s = Vec::with_capacity(clean.len() + 64 * gap);
    let mut injected = vec![];
    let mut from = 0;
    for (k, at) in (0..)
        .map(|k| (COUNTDOWN_S + 5 + 10 * k) * rate)
        .take_while(|&a| a + rate < clean.len())
        .enumerate()
    {
        s.extend_from_slice(&clean[from..at]);
        injected.push(s.len());
        s.extend(std::iter::repeat_n(0.0, gap));
        from = if k % 2 == 0 { at + gap } else { at };
    }
    s.extend_from_slice(&clean[from..]);
    let report = detect(rate as u32, &s);
    let starts: Vec<u64> = report.episodes.iter().map(|e| e.start_sample).collect();
    assert_eq!(starts.len(), injected.len(), "{starts:?} vs {injected:?}");
    for (e, &at) in report.episodes.iter().zip(&injected) {
        assert!(
            (e.start_sample as i64 - at as i64).abs() <= 48,
            "{e:?} at {at}"
        );
        assert!(e.end_sample as usize <= at + gap + 2 * 48, "{e:?} at {at}");
    }
}

#[test]
fn g_capture_deletion_hidden_by_the_pilot_is_unverifiable() {
    // Deleting exactly one pilot period (48 frames at 48 kHz) leaves the
    // captured pilot seamless; only the device positions reveal it.
    let rate = 48_000;
    let s: Vec<f32> = (0..rate as usize).map(|n| pilot(n, rate, 0.0)).collect();
    let cal = calibrate(&s, rate).unwrap();
    let mut det = Detector::new(rate, cal);
    let mut pos = 1_000_000u64;
    let mut data = s.as_slice();
    for k in 0.. {
        if data.is_empty() {
            break;
        }
        let n = data.len().min(480);
        if k == 50 {
            // The device delivered positions 48 frames further on, and the
            // samples continue seamlessly.
            pos += 48;
        }
        det.push_packet(pos, PacketFlags::default(), &data[..n]);
        pos += n as u64;
        data = &data[n..];
    }
    let report = det.finish();
    assert_eq!(report.episodes, vec![]);
    assert_eq!(
        report.unverifiable,
        vec![SampleSpan {
            start: 1_000_000 + 50 * 480,
            end: 1_000_000 + 50 * 480 + 48
        }]
    );
    // The last 3 ms are not counted as evaluated (filter settling).
    assert_eq!(report.evaluated.unwrap().end, pos - 144);
}

#[test]
fn g_capture_flags_overlap_and_silent_packets() {
    let rate = 48_000;
    let s: Vec<f32> = (0..rate as usize / 2)
        .map(|n| pilot(n, rate, 0.0))
        .collect();
    let cal = calibrate(&s, rate).unwrap();
    let run = |packets: &[(u64, PacketFlags, std::ops::Range<usize>)]| {
        let mut det = Detector::new(rate, cal);
        for (pos, flags, r) in packets {
            det.push_packet(*pos, *flags, &s[r.clone()]);
        }
        det.finish()
    };
    let ok = PacketFlags::default();
    let silent = PacketFlags { silent: true, ..ok };
    let disc = PacketFlags {
        discontinuity: true,
        ..ok
    };
    // A silent packet is a real gap in the output: one episode.
    let r = run(&[
        (0, ok, 0..9_600),
        (9_600, silent, 9_600..10_080),
        (10_080, ok, 10_080..24_000),
    ]);
    assert_eq!(r.episodes.len(), 1);
    assert!(r.unverifiable.is_empty());
    // A discontinuity flag or an overlap is unverifiable.
    let r = run(&[
        (0, ok, 0..9_600),
        (9_600, disc, 9_600..10_080),
        (10_080, ok, 10_080..24_000),
    ]);
    assert_eq!(
        r.unverifiable,
        vec![SampleSpan {
            start: 9_600,
            end: 10_080
        }]
    );
    let r = run(&[(0, ok, 0..9_600), (9_500, ok, 9_500..24_000)]);
    assert_eq!(
        r.unverifiable,
        vec![SampleSpan {
            start: 9_500,
            end: 9_600
        }]
    );
    assert!(r.episodes.is_empty());
}

#[test]
fn g_gap_right_after_warm_up_and_at_the_end_is_detected() {
    let rate = 48_000;
    let cal = calibrate(
        &(0..24_000).map(|n| pilot(n, rate, 0.0)).collect::<Vec<_>>(),
        rate,
    )
    .unwrap();
    // 1 ms gap starting 3.5 ms in, just after the 3 ms warm-up.
    let mut s: Vec<f32> = (0..9_600).map(|n| pilot(n, rate, 0.0)).collect();
    s[168..216].fill(0.0);
    let mut det = Detector::new(rate, cal);
    det.push(&s);
    let r = det.finish();
    assert_eq!(r.episodes.len(), 1, "{r:?}");
    assert_eq!(r.evaluated.unwrap().start, 144);
    // 1 ms gap ending at the last captured sample, not hop-aligned.
    let mut s: Vec<f32> = (0..9_605).map(|n| pilot(n, rate, 0.0)).collect();
    s[9_557..].fill(0.0);
    let mut det = Detector::new(rate, cal);
    det.push(&s);
    let r = det.finish();
    assert_eq!(r.episodes.len(), 1, "{r:?}");
    assert_eq!(r.episodes[0].end_sample, 9_605);
}

#[test]
fn g_an_episode_closes_only_after_the_signal_has_recovered() {
    // A stuttering dropout (1 ms gap, 1.5 ms of signal, 1 ms gap) is one
    // episode; two gaps 10 ms apart are two.
    let rate = 48_000;
    let cal = calibrate(
        &(0..24_000).map(|n| pilot(n, rate, 0.0)).collect::<Vec<_>>(),
        rate,
    )
    .unwrap();
    let count = |between: usize| {
        let mut s: Vec<f32> = (0..9_600).map(|n| pilot(n, rate, 0.0)).collect();
        s[4_800..4_848].fill(0.0);
        s[4_848 + between..4_896 + between].fill(0.0);
        let mut det = Detector::new(rate, cal);
        det.push(&s);
        det.finish().episodes.len()
    };
    assert_eq!(count(72), 1);
    assert_eq!(count(480), 2);
}

#[test]
fn g_final_gap_is_detected_or_outside_the_evaluated_span() {
    // A loud 14 kHz tone (2 ms attack, phase 11π/8) under the pilot, and the
    // last 1 ms of the capture silenced: the filter has not settled when
    // the capture ends, so the detector must either report the gap or end
    // its evaluated span before it (then V6 sees play as not covered unless
    // the capture runs on past play end).
    let rate = 48_000u32;
    let r = rate as usize;
    let cal = calibrate(
        &(0..r).map(|n| pilot(n, rate, 0.0)).collect::<Vec<_>>(),
        rate,
    )
    .unwrap();
    let total = r / 2;
    let mut s: Vec<f32> = (0..total)
        .map(|n| {
            let t = n as f64 / f64::from(rate);
            let attack = (t / 0.002).min(1.0);
            let tone =
                0.25 * attack * (TAU * 14_000.0 * t + 11.0 * std::f64::consts::PI / 8.0).sin();
            pilot(n, rate, 0.0) + tone as f32
        })
        .collect();
    s[total - 48..].fill(0.0);
    let mut det = Detector::new(rate, cal);
    det.push(&s);
    let report = det.finish();
    let gap_start = (total - 48) as u64;
    let covered_to = report.evaluated.unwrap().end;
    assert!(
        !report.episodes.is_empty() || covered_to <= gap_start,
        "gap missed and counted as covered: {report:?}"
    );
}

#[test]
fn g_gap_ending_at_the_evaluated_span_end_is_detected() {
    // The 3 ms settle margin is enough: a 1 ms gap that ends exactly where
    // the evaluated span ends is reported, at every offset and tone phase.
    let rate = 48_000u32;
    let r = rate as usize;
    let cal = calibrate(
        &(0..r).map(|n| pilot(n, rate, 0.0)).collect::<Vec<_>>(),
        rate,
    )
    .unwrap();
    for phase8 in 0..16 {
        for extra in 0..48 {
            let total = r / 4 + extra;
            let mut s: Vec<f32> = (0..total)
                .map(|n| {
                    let t = n as f64 / f64::from(rate);
                    let attack = (t / 0.002).min(1.0);
                    let ph = f64::from(phase8) * std::f64::consts::PI / 8.0;
                    let tone = 0.25 * attack * (TAU * 14_000.0 * t + ph).sin();
                    pilot(n, rate, 0.0) + tone as f32
                })
                .collect();
            let mut det = Detector::new(rate, cal);
            let end = total - 144;
            s[end - 48..end].fill(0.0);
            det.push(&s);
            let report = det.finish();
            assert_eq!(report.evaluated.unwrap().end, end as u64);
            assert_eq!(
                report.episodes.len(),
                1,
                "phase {phase8}π/8, length {total}"
            );
        }
    }
}
