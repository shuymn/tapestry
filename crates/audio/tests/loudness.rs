//! The loudness analysis (design.md "Algorithms"): the normalization gain's
//! formula and caps, the raw sum it measures (before the volume, the gain
//! and the clipper), whole-chart integration in histogram mode, the
//! over-range ceiling read on the meter's own gating grid, the end of an
//! analysis, and the pins that force a version bump when what a
//! measurement depends on changes.

use std::sync::Arc;

use audio::{
    Analysis, BLOCK_FRAMES, HEADROOM, MAX_GAIN, MAX_VOICES, Measurement, Mixer, Sound, SoundBank,
    Step, normalization_gain,
};
use bms_model::{BgmEvent, SoundId, TimeUs};
use ebur128::{EbuR128, Mode};

/// Runs an analysis of `events` over `bank` to its end, counting its steps.
fn analyze(bank: &Arc<SoundBank>, events: &[BgmEvent]) -> (Measurement, usize) {
    let mut analysis = Analysis::new(Arc::clone(bank)).unwrap();
    let mut events = events.iter().copied();
    let mut steps = 0;
    loop {
        steps += 1;
        if let Step::Done(m) = analysis.step(&mut events).unwrap() {
            return (m, steps);
        }
    }
}

/// `n` frames of a mono sine of `hz` at amplitude `amp` (of 32 767) at
/// `rate`.
fn sine(rate: u32, hz: f64, amp: f64, n: usize) -> Sound {
    Sound::mono(
        (0..n)
            .map(|i| {
                let phase = std::f64::consts::TAU * hz * i as f64 / f64::from(rate);
                (phase.sin() * amp).round() as i16
            })
            .collect(),
    )
}

fn event(us: i64, id: u16) -> BgmEvent {
    BgmEvent {
        time: TimeUs(us),
        sound: SoundId(id),
    }
}

/// What a mixer of its own makes of `events` (all triggered up front, in
/// time order: the pending starts hold them all) over `frames` frames,
/// from `mix_raw` in analysis-sized blocks.
fn raw_mix(bank: &SoundBank, events: &[BgmEvent], frames: u64) -> Vec<f32> {
    let mut m = Mixer::new(MAX_VOICES, bank.rate());
    for e in events {
        let at = u64::try_from(e.time.0).unwrap() * u64::from(bank.rate()) / 1_000_000;
        m.trigger(bank, e.sound, at);
    }
    let mut out = Vec::new();
    while (out.len() / 2) < frames as usize {
        let mut block = vec![0.0; BLOCK_FRAMES * 2];
        m.mix_raw(bank, &mut block);
        out.extend(block);
    }
    out.truncate(frames as usize * 2);
    out
}

/// A reference meter in `mode` over `samples`, fed in one call.
fn meter(rate: u32, mode: Mode, samples: &[f32]) -> EbuR128 {
    let mut m = EbuR128::new(2, rate, mode).unwrap();
    m.add_frames_f32(samples).unwrap();
    m
}

fn peak(m: &EbuR128) -> f64 {
    m.true_peak(0).unwrap().max(m.true_peak(1).unwrap())
}

fn measured(integrated: Option<f64>, true_peak: f64) -> Measurement {
    Measurement {
        integrated,
        true_peak,
        over_range: false,
        frames: 0,
    }
}

/// The gain is the smallest of the loudness term, the +6 dB cap and the
/// −1 dBTP peak term; a measurement without integrated loudness plays at
/// the headroom; over the +30 LUFS ceiling the loudness is taken as the
/// ceiling, so the gain does not jump there.
#[test]
fn gains_follow_the_formula_and_caps() {
    let close = |g: f32, want: f64| (f64::from(g) - want).abs() < 5e-4;
    // The design's examples: the loudness term, then the cap.
    let g = normalization_gain(&measured(Some(-12.0), 1.6));
    assert!(close(g, 0.501), "{g}");
    let g = normalization_gain(&measured(Some(-30.0), 0.3));
    assert!(close(g, 1.995), "{g}");
    assert_eq!(g, MAX_GAIN);
    // The peak term: −18 LUFS would take unity, a peak of 2 holds it to
    // 10^(−1/20) / 2.
    let g = normalization_gain(&measured(Some(-18.0), 2.0));
    assert!(close(g, 0.445_625), "{g}");
    // Silence, or a program too short to gate.
    assert_eq!(normalization_gain(&measured(None, 0.4)), HEADROOM);
    assert_eq!(normalization_gain(&measured(None, 0.0)), HEADROOM);
    // A peak of 0 leaves the other terms.
    let g = normalization_gain(&measured(Some(-24.0), 0.0));
    assert!(close(g, 1.995), "{g}");
    // Just below the ceiling and over it: the gains differ by no more than
    // the loudness does.
    let below = normalization_gain(&measured(Some(29.99), 0.01));
    let over = normalization_gain(&Measurement {
        over_range: true,
        ..measured(Some(29.99), 0.01)
    });
    let db = 20.0 * (f64::from(below) / f64::from(over)).log10();
    assert!((0.0..=0.01 + 1e-4).contains(&db), "{db}");
    // Over range, even with the integrated loudness missing.
    let over = normalization_gain(&Measurement {
        over_range: true,
        ..measured(None, 0.01)
    });
    assert!(close(over, 10_f64.powf(-48.0 / 20.0)), "{over}");
}

/// Two full-scale sounds starting on the same frame sum to twice full
/// scale, and the analysis measures that sum: before the headroom and the
/// clipper the peak could not pass full scale.
#[test]
fn overlapping_full_scale_sounds_measure_above_full_scale() {
    const RATE: u32 = 48_000;
    let tone = || sine(RATE, 997.0, 32_767.0, RATE as usize);
    let bank = Arc::new(SoundBank::from_sounds(RATE, vec![tone(), tone()]));
    let (m, _) = analyze(&bank, &[event(10_000, 0), event(10_000, 1)]);
    assert!(m.true_peak >= 1.9, "{}", m.true_peak);
    assert!(m.integrated.is_some());
}

/// The master volume and the play's gain do not reach `mix_raw`: its
/// samples are the same bits at any volume and gain, and the analysis
/// measures exactly what a meter in its mode makes of that raw sum.
#[test]
fn analysis_ignores_master_volume() {
    const RATE: u32 = 8_000;
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![
            sine(RATE, 440.0, 20_000.0, 2 * RATE as usize),
            sine(RATE, 660.0, 30_000.0, RATE as usize),
        ],
    ));
    let events = [event(0, 0), event(500_000, 1), event(1_500_000, 1)];
    let mixes: Vec<Vec<f32>> = [(1.0, HEADROOM), (0.25, 1.9), (16.0, 0.01)]
        .into_iter()
        .map(|(volume, gain)| {
            let mut m = Mixer::new(MAX_VOICES, RATE);
            m.set_volume(volume).unwrap();
            m.set_gain(gain).unwrap();
            for e in &events {
                m.trigger(&bank, e.sound, u64::try_from(e.time.0).unwrap() * 8 / 1_000);
            }
            let mut out = vec![0.0; 8 * BLOCK_FRAMES * 2];
            for block in out.chunks_mut(BLOCK_FRAMES * 2) {
                m.mix_raw(&bank, block);
            }
            assert_eq!(m.saturated(), 0);
            out
        })
        .collect();
    let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(&mixes[0]), bits(&mixes[1]));
    assert_eq!(bits(&mixes[0]), bits(&mixes[2]));
    // The sum passes full scale: a mix after the gain and the clipper
    // could not.
    assert!(mixes[0].iter().any(|x| x.abs() > 1.0));

    let (m, _) = analyze(&bank, &events);
    let mut raw = raw_mix(&bank, &events, m.frames);
    let reference = meter(RATE, Mode::I | Mode::TRUE_PEAK | Mode::HISTOGRAM, &raw);
    assert_eq!(m.integrated, Some(reference.loudness_global().unwrap()));
    // The peak once the interpolator is flushed.
    raw.extend([0.0; 2 * 12]);
    let reference = meter(RATE, Mode::I | Mode::TRUE_PEAK | Mode::HISTOGRAM, &raw);
    assert_eq!(m.true_peak, peak(&reference));
    assert!(!m.over_range);
}

/// Five minutes loud, then five minutes 20 dB quieter: histogram mode
/// integrates the whole chart as a meter keeping every gating block does
/// (within its bin resolution), so the loud half dominates; a meter whose
/// history were truncated to bound memory would measure the quiet tail.
#[test]
fn histogram_mode_keeps_whole_chart_loudness() {
    const RATE: u32 = 8_000;
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![
            sine(RATE, 440.0, 20_000.0, RATE as usize),
            sine(RATE, 440.0, 2_000.0, RATE as usize),
        ],
    ));
    let events: Vec<BgmEvent> = (0..600_i64)
        .map(|s| event(s * 1_000_000, u16::from(s >= 300)))
        .collect();
    let (m, _) = analyze(&bank, &events);
    let l = m.integrated.unwrap();
    let raw = raw_mix(&bank, &events, m.frames);
    let whole = meter(RATE, Mode::I, &raw).loudness_global().unwrap();
    let half = raw.len() / 2;
    let quiet = meter(RATE, Mode::I, &raw[half..])
        .loudness_global()
        .unwrap();
    assert!((l - whole).abs() < 0.1, "{l} against {whole}");
    assert!(l > quiet + 10.0, "{l} against the quiet half's {quiet}");
}

/// EBU Tech 3341 case 1: a stereo 1 kHz sine at −23 dBFS measures −23 LUFS
/// within 0.1 LU. A change of the decoder, mixer or meter that moves it
/// fails here and bumps `MIXER_VERSION` or `ANALYSIS_VERSION`.
#[test]
fn analysis_version_pins_the_reference_tone() {
    const RATE: u32 = 48_000;
    let amp = 32_768.0 * 10_f64.powf(-23.0 / 20.0);
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![sine(RATE, 1_000.0, amp, 20 * RATE as usize)],
    ));
    let (m, _) = analyze(&bank, &[event(0, 0)]);
    let l = m.integrated.unwrap();
    assert!((l + 23.0).abs() < 0.1, "{l}");
    assert_eq!((audio::MIXER_VERSION, audio::ANALYSIS_VERSION), (1, 1));
}

/// The resolved closure of the crates that shape a measurement (the
/// decoder, the resampler and the meter) in `Cargo.lock`, and their
/// manifest lines with the features, hashed: it must match the fingerprint
/// kept beside the versions, so a dependency update that could change a
/// measurement bumps a version first.
#[test]
fn analysis_dependencies_are_pinned() {
    use sha2::{Digest, Sha256};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    // Each package: name, version, checksum and dependencies.
    struct Package {
        name: String,
        version: String,
        checksum: String,
        deps: Vec<String>,
    }
    let value = |line: &str, key: &str| {
        line.strip_prefix(key)
            .and_then(|r| r.strip_prefix(" = \""))
            .and_then(|r| r.strip_suffix('"'))
            .map(str::to_owned)
    };
    let mut packages = Vec::new();
    for block in lock.split("[[package]]").skip(1) {
        let mut p = Package {
            name: String::new(),
            version: String::new(),
            checksum: String::new(),
            deps: Vec::new(),
        };
        let mut in_deps = false;
        for line in block.lines().map(str::trim) {
            if in_deps {
                if line == "]" {
                    in_deps = false;
                } else {
                    p.deps
                        .push(line.trim_matches(|c| c == '"' || c == ',').to_owned());
                }
            } else if line == "dependencies = [" {
                in_deps = true;
            } else if let Some(v) = value(line, "name") {
                p.name = v;
            } else if let Some(v) = value(line, "version") {
                p.version = v;
            } else if let Some(v) = value(line, "checksum") {
                p.checksum = v;
            }
        }
        packages.push(p);
    }
    let find = |dep: &str| {
        let mut parts = dep.split(' ');
        let name = parts.next().unwrap();
        let version = parts.next();
        let found: Vec<usize> = (0..packages.len())
            .filter(|&i| {
                packages[i].name == name && version.is_none_or(|v| packages[i].version == v)
            })
            .collect();
        assert_eq!(found.len(), 1, "{dep}");
        found[0]
    };
    let mut seen = vec![false; packages.len()];
    let mut stack: Vec<usize> = ["symphonia", "rubato", "ebur128"]
        .into_iter()
        .map(find)
        .collect();
    while let Some(i) = stack.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        stack.extend(packages[i].deps.iter().map(|d| find(d)));
    }
    let mut lines: Vec<String> = (0..packages.len())
        .filter(|&i| seen[i])
        .map(|i| {
            let p = &packages[i];
            format!("{} {} {}", p.name, p.version, p.checksum)
        })
        .collect();
    lines.sort();
    for name in ["symphonia = ", "rubato = ", "ebur128 = "] {
        let line = manifest
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no {name} in the workspace manifest"));
        lines.push(line.to_owned());
    }
    let hash: String = Sha256::digest(lines.join("\n"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        hash,
        audio::MEASUREMENT_DEPENDENCIES,
        "the crates that shape a loudness measurement changed: bump \
         MIXER_VERSION or ANALYSIS_VERSION and update the fingerprint\n{}",
        lines.join("\n")
    );
}

/// A loud burst exactly as long as a gating block, placed on the meter's
/// gating grid between two analysis blocks' ends: the gating block that
/// holds it reads at or above the +30 LUFS ceiling, while every window
/// ending on an analysis block's end reads below it. The analysis, which
/// reads where the meter completes each gating block, marks it over range.
#[test]
fn a_window_crossing_the_ceiling_between_blocks_is_over_range() {
    const RATE: u32 = 48_000;
    const HOP: usize = 4_800;
    const IDS: u16 = 64;
    // The burst fills the gating block [4 800, 24 000); analysis blocks
    // end at 20 480 and 24 576.
    let at_us = 100_000;
    let target = 30.06;
    let level = |amp: f64| {
        let bank = SoundBank::from_sounds(
            RATE,
            (0..IDS).map(|_| sine(RATE, 997.0, amp, 4 * HOP)).collect(),
        );
        let events: Vec<BgmEvent> = (0..IDS).map(|k| event(at_us, k)).collect();
        let raw = raw_mix(&bank, &events, 7 * BLOCK_FRAMES as u64);
        let at = |end: usize| {
            meter(RATE, Mode::M, &raw[..end * 2])
                .loudness_momentary()
                .unwrap()
        };
        (at(24_000), at(20_480), at(24_576), bank, events)
    };
    let (probe, ..) = level(1_000.0);
    let amp = 1_000.0 * 10_f64.powf((target - probe) / 20.0);
    let (grid, early, late, bank, events) = level(amp);
    assert!(grid >= 30.0, "{grid}");
    assert!(early < 30.0 && late < 30.0, "{early} {late}");
    let (m, _) = analyze(&Arc::new(bank), &events);
    assert!(m.over_range);
    assert!(f64::from(normalization_gain(&m)) <= 10_f64.powf(-48.0 / 20.0) * 1.000_1);
}

/// 64 coherent near-full-scale voices sum far past the histogram's
/// ceiling: over range.
#[test]
fn coherent_voices_are_over_range() {
    const RATE: u32 = 8_000;
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        (0..64)
            .map(|_| sine(RATE, 440.0, 30_000.0, RATE as usize))
            .collect(),
    ));
    let events: Vec<BgmEvent> = (0..64).map(|k| event(0, k)).collect();
    let (m, _) = analyze(&bank, &events);
    assert!(m.over_range);
    assert!(m.true_peak > 40.0, "{}", m.true_peak);
}

/// A sound whose last frames, the loudest, end exactly on an analysis
/// block's end: the true peak is read once the interpolator's delay is
/// flushed, so their inter-sample peak counts.
#[test]
fn a_sound_ending_on_a_block_boundary_reports_its_interpolated_peak() {
    const RATE: u32 = 48_000;
    let mut samples = vec![0_i16; 2 * BLOCK_FRAMES];
    let n = samples.len();
    // A burst near the Nyquist rate overshoots between samples.
    for (i, s) in samples[n - 6..].iter_mut().enumerate() {
        *s = if i % 2 == 0 { 16_000 } else { -16_000 };
    }
    let bank = Arc::new(SoundBank::from_sounds(RATE, vec![Sound::mono(samples)]));
    let events = [event(0, 0)];
    let (m, _) = analyze(&bank, &events);
    assert_eq!(m.frames, 2 * BLOCK_FRAMES as u64);
    let mut raw = raw_mix(&bank, &events, m.frames);
    let sample_peak = raw.iter().fold(0.0_f32, |p, x| p.max(x.abs()));
    raw.extend([0.0; 2 * 64]);
    let reference = peak(&meter(RATE, Mode::TRUE_PEAK, &raw));
    assert!(reference > f64::from(sample_peak) * 1.05, "{reference}");
    assert!(
        (m.true_peak - reference).abs() < 1e-9,
        "{} {reference}",
        m.true_peak
    );
}

/// A steady one-second tone: the integrated loudness is that of the frames
/// mixed, read before any padding.
#[test]
fn a_steady_tone_measures_the_frames_mixed() {
    const RATE: u32 = 48_000;
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![sine(RATE, 1_000.0, 8_000.0, RATE as usize)],
    ));
    let events = [event(0, 0)];
    let (m, _) = analyze(&bank, &events);
    // The tone ends inside the twelfth block.
    assert_eq!(m.frames, 12 * BLOCK_FRAMES as u64);
    let raw = raw_mix(&bank, &events, m.frames);
    let reference = meter(RATE, Mode::I | Mode::HISTOGRAM, &raw)
        .loudness_global()
        .unwrap();
    assert_eq!(m.integrated, Some(reference));
}

/// A sound shorter than one gating block (100 ms) completes none: no
/// integrated loudness, so the chart plays at the headroom.
#[test]
fn a_short_sound_has_no_integrated_loudness() {
    const RATE: u32 = 48_000;
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![sine(RATE, 1_000.0, 20_000.0, RATE as usize / 10)],
    ));
    let (m, steps) = analyze(&bank, &[event(0, 0)]);
    assert_eq!(m.integrated, None);
    assert!(m.true_peak > 0.5);
    assert_eq!(normalization_gain(&m), HEADROOM);
    assert_eq!(steps, 3);
}

/// A bank with no sound stored is done at once, metering nothing however
/// long the schedule; an empty schedule over sounds is done after its
/// first step.
#[test]
fn nothing_to_sound_is_done_at_once() {
    let empty = Arc::new(SoundBank::from_sounds(48_000, Vec::new()));
    let far: Vec<BgmEvent> = (0..10).map(|k| event(k * 600_000_000, 0)).collect();
    let (m, steps) = analyze(&empty, &far);
    assert_eq!((m.integrated, m.frames, steps), (None, 0, 1));
    let bank = Arc::new(SoundBank::from_sounds(
        48_000,
        vec![Sound::mono(vec![1; 10])],
    ));
    let (m, steps) = analyze(&bank, &[]);
    assert_eq!((m.integrated, m.frames, steps), (None, 0, 1));
}
