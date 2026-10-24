//! Offline pilot-tone continuity detector (design D5).
//!
//! The audio thread adds a 19 kHz pilot at −30 dBFS to the left channel
//! after the resampler (key sounds are band-limited to 16 kHz). The loopback
//! capture of the same endpoint is demodulated here:
//!
//! 1. Mix down by the pilot frequency (exact period table, no drift).
//! 2. Low-pass the complex baseband (4th-order Butterworth at 1 kHz), which
//!    removes key-sound content at least 3 kHz away and the mix image.
//! 3. Average coherently over a 1 ms window advanced every 0.25 ms (75%
//!    overlap); amplitude = 2|mean|, phase = arg(mean).
//!
//! A window is bad if its amplitude is below 60% of the countdown
//! calibration, or if its phase differs from the previous non-overlapping
//! window (4 hops back) by more than `PHASE_STEP_LIMIT_RAD` while both have
//! full amplitude. Bad windows coalesce into one episode, which closes only
//! after `CLOSE_GOOD_WINDOWS` good windows in a row (the filter's recovery
//! after a gap); G5 counts episodes. A gap of 1 ms or longer silences at
//! least 0.75 ms of some window, so its amplitude falls far below 60%;
//! shorter gaps are a stated limit.
//!
//! Capture continuity is not inferred from the samples: packets carry their
//! device position and flags, and a position jump, overlap or discontinuity
//! flag is an unverifiable span (V6). The trailing partial window is
//! checked at the end, and the report states the evaluated span: it starts
//! after the 3 ms warm-up and ends 3 ms before the capture ends (the
//! filter's settling), and V6 requires it to cover the play window, so the
//! capture must start at least 3 ms before play and run at least 3 ms past
//! it.

/// Pilot frequency.
pub const PILOT_HZ: u32 = 19_000;
/// −30 dBFS.
pub const PILOT_AMPLITUDE: f64 = 0.031_622_776_601_683_79;
/// A window is bad below this share of the calibrated amplitude.
pub const AMPLITUDE_RATIO: f64 = 0.6;
pub const PHASE_STEP_LIMIT_RAD: f64 = 0.35;
const LPF_HZ: f64 = 1_000.0;
/// Windows skipped at the start while the filter settles (3 ms).
const WARMUP_WINDOWS: u64 = 12;
/// Capture after the evaluated span's end, in windows (3 ms): the
/// low-pass's response and one window must settle after a gap before it is
/// certain to show, so the last 3 ms of a capture are not evaluated and the
/// capture must run at least that long past play end (V6).
const SETTLE_WINDOWS: u64 = 12;
/// Good windows in a row that close an episode (2 ms).
pub const CLOSE_GOOD_WINDOWS: u64 = 8;

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Samples per pilot period table: the pilot repeats exactly after this many
/// samples (441 at 44.1 kHz, 48 at 48 kHz, 96 at 96 kHz).
pub fn table_len(rate_hz: u32) -> usize {
    (u64::from(rate_hz) / gcd(u64::from(rate_hz), u64::from(PILOT_HZ))) as usize
}

fn phase_of(n: usize, rate_hz: u32) -> f64 {
    // Exact integer phase index keeps the table drift-free.
    let k = (n as u64 * u64::from(PILOT_HZ)) % u64::from(rate_hz);
    std::f64::consts::TAU * k as f64 / f64::from(rate_hz)
}

/// Pilot generator for the audio thread: preallocated table, no allocation
/// per sample.
#[derive(Clone, Debug)]
pub struct PilotOscillator {
    table: Vec<f32>,
    idx: usize,
}

impl PilotOscillator {
    pub fn new(rate_hz: u32) -> Self {
        let table = (0..table_len(rate_hz))
            .map(|n| (PILOT_AMPLITUDE * phase_of(n, rate_hz).sin()) as f32)
            .collect();
        Self { table, idx: 0 }
    }

    pub fn next_sample(&mut self) -> f32 {
        let x = self.table[self.idx];
        self.idx = (self.idx + 1) % self.table.len();
        x
    }
}

#[derive(Clone, Copy, Debug)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn lowpass(rate_hz: f64, f0: f64, q: f64) -> Self {
        let w0 = std::f64::consts::TAU * f0 / rate_hz;
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 - c) / 2.0 / a0,
            b1: (1.0 - c) / a0,
            b2: (1.0 - c) / 2.0 / a0,
            a1: -2.0 * c / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// One demodulated window.
#[derive(Clone, Copy, Debug)]
pub struct WindowStat {
    pub index: u64,
    pub start_sample: u64,
    pub amplitude: f64,
    pub phase: f64,
}

/// Streaming demodulator producing a `WindowStat` per hop.
#[derive(Clone, Debug)]
struct Demod {
    cos: Vec<f64>,
    sin: Vec<f64>,
    idx: usize,
    lpf: [[Biquad; 2]; 2],
    hop: usize,
    win: usize,
    fill: usize,
    acc: (f64, f64),
    blocks: [(f64, f64); 4],
    blocks_done: u64,
    /// The last `win` filtered baseband samples, for the trailing window.
    ring: Vec<(f64, f64)>,
    fed: u64,
}

impl Demod {
    fn new(rate_hz: u32) -> Self {
        let n = table_len(rate_hz);
        let rate = f64::from(rate_hz);
        // 4th-order Butterworth as two biquads.
        let section = |q| Biquad::lowpass(rate, LPF_HZ, q);
        let pair = [
            section(0.541_196_100_146_197),
            section(1.306_562_964_876_376_7),
        ];
        let win = ((rate_hz as usize + 500) / 1000) / 4 * 4;
        Self {
            cos: (0..n).map(|k| phase_of(k, rate_hz).cos()).collect(),
            sin: (0..n).map(|k| -phase_of(k, rate_hz).sin()).collect(),
            idx: 0,
            lpf: [pair, pair],
            hop: win / 4,
            win,
            fill: 0,
            acc: (0.0, 0.0),
            blocks: [(0.0, 0.0); 4],
            blocks_done: 0,
            ring: vec![(0.0, 0.0); win],
            fed: 0,
        }
    }

    fn stat(&self, index: u64, start_sample: u64, (si, sq): (f64, f64)) -> WindowStat {
        WindowStat {
            index,
            start_sample,
            amplitude: 2.0 * (si * si + sq * sq).sqrt() / self.win as f64,
            phase: sq.atan2(si),
        }
    }

    fn push(&mut self, x: f32) -> Option<WindowStat> {
        let x = f64::from(x);
        let mut i = x * self.cos[self.idx];
        let mut q = x * self.sin[self.idx];
        self.idx = (self.idx + 1) % self.cos.len();
        for s in &mut self.lpf[0] {
            i = s.run(i);
        }
        for s in &mut self.lpf[1] {
            q = s.run(q);
        }
        self.ring[(self.fed % self.win as u64) as usize] = (i, q);
        self.fed += 1;
        self.acc.0 += i;
        self.acc.1 += q;
        self.fill += 1;
        if self.fill < self.hop {
            return None;
        }
        self.blocks[(self.blocks_done % 4) as usize] = self.acc;
        self.blocks_done += 1;
        self.acc = (0.0, 0.0);
        self.fill = 0;
        if self.blocks_done < 4 {
            return None;
        }
        let sum = self
            .blocks
            .iter()
            .fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
        let index = self.blocks_done - 4;
        Some(self.stat(index, index * self.hop as u64, sum))
    }

    /// The window over the last `win` samples when samples after the last
    /// full window would otherwise go unchecked.
    fn trailing(&self) -> Option<WindowStat> {
        let last_end = (self.blocks_done.checked_sub(4)? * self.hop as u64) + self.win as u64;
        if self.fed <= last_end {
            return None;
        }
        let sum = self
            .ring
            .iter()
            .fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
        Some(self.stat(self.blocks_done - 3, self.fed - self.win as u64, sum))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Calibration {
    pub amplitude: f64,
}

/// Calibrates from a countdown capture (pilot only). Fails if the pilot is
/// missing, attenuated or boosted by more than 2× (for example by
/// enhancements), or unsteady (any window more than 10% off the median).
pub fn calibrate(samples: &[f32], rate_hz: u32) -> Option<Calibration> {
    let mut d = Demod::new(rate_hz);
    let mut amps: Vec<f64> = samples
        .iter()
        .filter_map(|&x| d.push(x))
        .filter(|w| w.index >= WARMUP_WINDOWS)
        .map(|w| w.amplitude)
        .collect();
    if amps.len() < 100 {
        return None;
    }
    amps.sort_by(f64::total_cmp);
    let median = amps[amps.len() / 2];
    let steady = amps.iter().all(|a| (a - median).abs() <= 0.1 * median);
    let level_ok = (0.5 * PILOT_AMPLITUDE..=2.0 * PILOT_AMPLITUDE).contains(&median);
    (steady && level_ok).then_some(Calibration { amplitude: median })
}

/// A span of capture device positions (half-open).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleSpan {
    pub start: u64,
    pub end: u64,
}

/// A confirmed discontinuity, in capture device positions (half-open).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleEpisode {
    pub start_sample: u64,
    pub end_sample: u64,
    pub bad_windows: u64,
}

/// Capture packet flags (`AUDCLNT_BUFFERFLAGS_SILENT`,
/// `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`).
#[derive(Clone, Copy, Debug, Default)]
pub struct PacketFlags {
    pub silent: bool,
    pub discontinuity: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectorReport {
    pub episodes: Vec<SampleEpisode>,
    /// Spans whose content is unknown: device-position jumps or overlaps,
    /// and packets flagged as a data discontinuity. Any one invalidates the
    /// run (V6).
    pub unverifiable: Vec<SampleSpan>,
    /// Device positions actually checked (after the filter warm-up). V6
    /// requires it to cover the play window.
    pub evaluated: Option<SampleSpan>,
}

#[derive(Clone, Debug)]
pub struct Detector {
    demod: Demod,
    threshold: f64,
    /// Last four windows: (amplitude ok, phase).
    recent: [(bool, f64); 4],
    open: Option<SampleEpisode>,
    good_run: u64,
    episodes: Vec<SampleEpisode>,
    windows: u64,
    /// Device position of the first captured sample, and of the next one
    /// expected.
    base: Option<u64>,
    next: u64,
    /// Frames skipped by position jumps (device position minus fed count).
    jumped: u64,
    unverifiable: Vec<SampleSpan>,
}

impl Detector {
    pub fn new(rate_hz: u32, cal: Calibration) -> Self {
        Self {
            demod: Demod::new(rate_hz),
            threshold: AMPLITUDE_RATIO * cal.amplitude,
            recent: [(false, 0.0); 4],
            open: None,
            good_run: 0,
            episodes: Vec::new(),
            windows: 0,
            base: None,
            next: 0,
            jumped: 0,
            unverifiable: Vec::new(),
        }
    }

    pub fn window_len(&self) -> usize {
        self.demod.win
    }

    pub fn hop_len(&self) -> usize {
        self.demod.hop
    }

    /// Contiguous samples following the previous ones.
    pub fn push(&mut self, samples: &[f32]) {
        let pos = if self.base.is_some() { self.next } else { 0 };
        self.push_packet(pos, PacketFlags::default(), samples);
    }

    /// One capture packet at device position `pos`. Continuity comes from
    /// the device positions and flags, not from the samples: a jump, an
    /// overlap or a discontinuity flag is an unverifiable span, even where
    /// the periodic pilot would hide it (a 48-frame jump at 48 kHz is one
    /// whole pilot period). Missing frames are not invented: later positions
    /// are offset by the jump. A silent packet is silence.
    pub fn push_packet(&mut self, pos: u64, flags: PacketFlags, samples: &[f32]) {
        if self.base.is_none() {
            self.base = Some(pos);
            self.next = pos;
        }
        let mut samples = samples;
        let mut pos = pos;
        if pos > self.next {
            self.unverifiable.push(SampleSpan {
                start: self.next,
                end: pos,
            });
            self.jumped += pos - self.next;
        } else if pos < self.next {
            let overlap = ((self.next - pos) as usize).min(samples.len());
            self.unverifiable.push(SampleSpan {
                start: pos,
                end: self.next,
            });
            samples = &samples[overlap..];
            pos += overlap as u64;
        }
        if flags.discontinuity {
            self.unverifiable.push(SampleSpan {
                start: pos,
                end: pos + samples.len() as u64,
            });
        }
        for &x in samples {
            self.feed(if flags.silent { 0.0 } else { x });
        }
        self.next = pos + samples.len() as u64;
    }

    fn feed(&mut self, x: f32) {
        if let Some(w) = self.demod.push(x) {
            self.window(w, true);
        }
    }

    fn window(&mut self, w: WindowStat, check_phase: bool) {
        self.windows += 1;
        let amp_ok = w.amplitude >= self.threshold;
        let slot = (w.index % 4) as usize;
        let (prev_ok, prev_phase) = self.recent[slot];
        self.recent[slot] = (amp_ok, w.phase);
        if w.index < WARMUP_WINDOWS {
            return;
        }
        let mut step = w.phase - prev_phase;
        step -= std::f64::consts::TAU * (step / std::f64::consts::TAU).round();
        let phase_bad = check_phase && amp_ok && prev_ok && step.abs() > PHASE_STEP_LIMIT_RAD;
        let start = self.base.unwrap_or(0) + self.jumped + w.start_sample;
        let end = start + self.demod.win as u64;
        if !amp_ok || phase_bad {
            self.good_run = 0;
            match &mut self.open {
                Some(e) => {
                    e.end_sample = end;
                    e.bad_windows += 1;
                }
                None => {
                    self.open = Some(SampleEpisode {
                        start_sample: start,
                        end_sample: end,
                        bad_windows: 1,
                    });
                }
            }
        } else if self.open.is_some() {
            // Close only after the filter has recovered: bad windows
            // separated by a short good run are one episode.
            self.good_run += 1;
            if self.good_run >= CLOSE_GOOD_WINDOWS {
                self.episodes.extend(self.open.take());
                self.good_run = 0;
            }
        }
    }

    pub fn windows(&self) -> u64 {
        self.windows
    }

    /// Checks the trailing partial window, closes any open episode and
    /// returns the report.
    pub fn finish(mut self) -> DetectorReport {
        if let Some(w) = self.demod.trailing() {
            // Overlaps the previous windows, so only its amplitude counts.
            self.window(w, false);
        }
        self.episodes.extend(self.open.take());
        let base = self.base.unwrap_or(0);
        let from = base + WARMUP_WINDOWS * self.demod.hop as u64;
        let to = (base + self.jumped + self.demod.fed)
            .saturating_sub(SETTLE_WINDOWS * self.demod.hop as u64);
        DetectorReport {
            episodes: self.episodes,
            unverifiable: self.unverifiable,
            evaluated: (to > from).then_some(SampleSpan {
                start: from,
                end: to,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_and_hop_lengths() {
        for (rate, win, hop, table) in [
            (44_100, 44, 11, 441),
            (48_000, 48, 12, 48),
            (96_000, 96, 24, 96),
        ] {
            let d = Demod::new(rate);
            assert_eq!((d.win, d.hop, d.cos.len()), (win, hop, table));
        }
    }

    #[test]
    fn trailing_window_covers_the_last_samples() {
        let mut d = Demod::new(48_000);
        for _ in 0..(480 + 5) {
            d.push(0.1);
        }
        let t = d.trailing().expect("unaligned tail");
        assert_eq!(t.start_sample, 485 - 48);
        for _ in 0..7 {
            d.push(0.1);
        }
        // 492 = 41 hops: the last full window ends at the last sample.
        assert!(d.trailing().is_none());
    }

    #[test]
    fn oscillator_is_exact_and_calibrates() {
        let mut o = PilotOscillator::new(48_000);
        let s: Vec<f32> = (0..48_000).map(|_| o.next_sample()).collect();
        let cal = calibrate(&s, 48_000).expect("calibrates");
        assert!((cal.amplitude - PILOT_AMPLITUDE).abs() < 0.02 * PILOT_AMPLITUDE);
        // Silence or a pilot at a tenth of its level does not calibrate.
        assert_eq!(calibrate(&vec![0.0; 48_000], 48_000), None);
        let quiet: Vec<f32> = s.iter().map(|x| x * 0.1).collect();
        assert_eq!(calibrate(&quiet, 48_000), None);
    }
}
