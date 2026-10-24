//! Loopback capture evaluation (design D5, V6), without the device: the
//! capture thread hands every packet (device position, flags, timestamp,
//! left channel) to `CaptureEval`, which
//!
//! 1. after the render stream started, collects contiguous capture, waits
//!    out the settling time and calibrates the pilot on the next second;
//! 2. finds where the render stream's pilot steps in with the calibrated
//!    threshold (`pilot::OnsetFinder`), which aligns stream frames with
//!    capture positions;
//! 3. holds the newest `HOLD_MS` of capture back from the detector, so the
//!    end of the evaluated capture is cut at a capture position derived
//!    from the render stream's stop time (minus a margin), not wherever
//!    the polling happened to be when the stream stopped;
//! 4. maps capture positions to run µs by a least-squares line through the
//!    packet timestamps, leaving out packets flagged TIMESTAMP_ERROR, and
//!    bounds the map's error by its largest residual: V6's coverage is
//!    narrowed by that bound, and left unestablished when the bound
//!    exceeds `MAX_MAP_ERROR_US` or there are too few timestamps.

use crate::pilot::{Calibration, Detector, DetectorReport, OnsetFinder, PacketFlags, calibrate};
use crate::run::{AudioFacts, Episode};
use std::collections::VecDeque;

/// Capture held back from the detector.
pub const HOLD_MS: u64 = 200;
/// Capture this close to the render stream's stop is not evaluated (the
/// stop cuts the stream to silence, which is not a dropout).
pub const STOP_MARGIN_US: i64 = 10_000;
/// Largest timestamp-map error with which V6 coverage is established.
pub const MAX_MAP_ERROR_US: i64 = 20_000;
/// Capture timestamps skipped after the render stream starts.
pub const STARTUP_SKIP_MS: u64 = 100;
/// Fewest usable packet timestamps for a map.
const MIN_ANCHORS: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureFlags {
    pub silent: bool,
    pub discontinuity: bool,
    /// AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR: the packet's timestamp is
    /// uncertain.
    pub timestamp_error: bool,
}

/// Least-squares line through `(position, µs)` anchors, with its largest
/// residual.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinearMap {
    mean_pos: f64,
    mean_us: f64,
    us_per_frame: f64,
    pub max_residual_us: i64,
}

impl LinearMap {
    pub fn fit(anchors: &[(u64, i64)]) -> Option<Self> {
        if anchors.len() < 2 {
            return None;
        }
        let n = anchors.len() as f64;
        let mean_pos = anchors.iter().map(|a| a.0 as f64).sum::<f64>() / n;
        let mean_us = anchors.iter().map(|a| a.1 as f64).sum::<f64>() / n;
        let (mut sxx, mut sxy) = (0.0, 0.0);
        for &(p, t) in anchors {
            let dx = p as f64 - mean_pos;
            sxx += dx * dx;
            sxy += dx * (t as f64 - mean_us);
        }
        if sxx <= 0.0 {
            return None;
        }
        let mut m = Self {
            mean_pos,
            mean_us,
            us_per_frame: sxy / sxx,
            max_residual_us: 0,
        };
        m.max_residual_us = anchors
            .iter()
            .map(|&(p, t)| (t - m.at(p)).abs())
            .max()
            .unwrap_or(0);
        Some(m)
    }

    pub fn at(&self, pos: u64) -> i64 {
        (self.mean_us + (pos as f64 - self.mean_pos) * self.us_per_frame).round() as i64
    }

    /// The capture position at `t_us`.
    pub fn position_at(&self, t_us: i64) -> u64 {
        (self.mean_pos + (t_us as f64 - self.mean_us) / self.us_per_frame)
            .round()
            .max(0.0) as u64
    }
}

enum Phase {
    /// Before the render stream started.
    Waiting,
    /// Contiguous capture since `base`, for settling and calibration.
    Collecting {
        base: u64,
        next: u64,
    },
    Detecting(Box<Detector>),
    Failed(&'static str),
}

/// What the evaluation found, beyond `AudioFacts`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaptureSummary {
    pub calibration_amplitude: Option<f64>,
    /// Capture position of the pilot step, and each episode's start
    /// position (the frame domain the injection check uses).
    pub onset_position: Option<u64>,
    pub episode_positions: Vec<u64>,
    /// Largest residual of the timestamp map, µs.
    pub map_error_us: Option<i64>,
    /// Timestamps used for the map, and those left out as uncertain.
    pub anchors_used: usize,
    pub timestamp_errors: u64,
    /// Capture positions after which nothing was evaluated (the stop cut).
    pub cut_position: Option<u64>,
    /// The cut fell before capture already handed to the detector (the
    /// hold was too short); the evaluated end is then later than the cut.
    pub cut_late: bool,
    pub error: Option<&'static str>,
}

pub struct CaptureEval {
    rate_hz: u32,
    settle_frames: usize,
    cal_frames: usize,
    hold_frames: usize,
    phase: Phase,
    pre: Vec<f32>,
    /// Held packets: (position, flags, length) and their samples.
    held: VecDeque<(u64, PacketFlags, usize)>,
    held_samples: Vec<f32>,
    /// First capture position not yet handed to the detector.
    released_to: u64,
    anchors: Vec<(u64, i64)>,
    /// First capture position after the render stream started.
    first_started: Option<u64>,
    startup_frames: u64,
    summary: CaptureSummary,
}

impl CaptureEval {
    /// `settle_us` of capture after the render stream starts is skipped,
    /// then `cal_us` calibrates; `max_anchors` bounds the timestamp log.
    pub fn new(rate_hz: u32, settle_us: i64, cal_us: i64, max_anchors: usize) -> Self {
        let frames = |us: i64| (i64::from(rate_hz) * us / 1_000_000) as usize;
        let hold_frames = (u64::from(rate_hz) * HOLD_MS / 1000) as usize;
        let (settle_frames, cal_frames) = (frames(settle_us), frames(cal_us));
        Self {
            rate_hz,
            settle_frames,
            cal_frames,
            hold_frames,
            phase: Phase::Waiting,
            pre: Vec::with_capacity(settle_frames + cal_frames + rate_hz as usize),
            held: VecDeque::with_capacity(4_096),
            held_samples: Vec::with_capacity(hold_frames + rate_hz as usize),
            released_to: 0,
            anchors: Vec::with_capacity(max_anchors),
            first_started: None,
            startup_frames: u64::from(rate_hz) * STARTUP_SKIP_MS / 1000,
            summary: CaptureSummary::default(),
        }
    }

    /// One capture packet at device position `pos` stamped `t_us`.
    /// `started` is whether the render stream has started.
    pub fn push(
        &mut self,
        pos: u64,
        flags: CaptureFlags,
        t_us: i64,
        samples: &[f32],
        started: bool,
    ) {
        // Timestamps from before the render stream started, and from the
        // first `STARTUP_SKIP_MS` of capture after it, are left out: the
        // first packets of a loopback stream arrive in a burst with
        // timestamps synthesized 10 ms apart (13 ms early on the RME).
        if started && self.first_started.is_none() {
            self.first_started = Some(pos);
        }
        let settled = self
            .first_started
            .is_some_and(|f| pos >= f + self.startup_frames);
        if flags.timestamp_error {
            self.summary.timestamp_errors += 1;
        } else if settled && self.anchors.len() < self.anchors.capacity() {
            self.anchors.push((pos, t_us));
        }
        let pf = PacketFlags {
            silent: flags.silent,
            discontinuity: flags.discontinuity,
        };
        let phase = std::mem::replace(&mut self.phase, Phase::Waiting);
        self.phase = match phase {
            Phase::Waiting if !started => Phase::Waiting,
            Phase::Waiting => {
                self.pre.clear();
                self.collect(pos, pos, pf, samples)
            }
            Phase::Collecting { base, next } => {
                if pos != next || flags.discontinuity {
                    // Contiguous capture only: restart.
                    self.pre.clear();
                    self.collect(pos, pos, pf, samples)
                } else {
                    self.collect(base, pos, pf, samples)
                }
            }
            Phase::Detecting(d) => {
                self.hold(pos, pf, samples);
                Phase::Detecting(d)
            }
            f @ Phase::Failed(_) => f,
        };
        self.release(self.hold_frames);
    }

    fn collect(&mut self, base: u64, pos: u64, flags: PacketFlags, samples: &[f32]) -> Phase {
        let need = self.settle_frames + self.cal_frames;
        let take = (need - self.pre.len()).min(samples.len());
        if flags.silent {
            self.pre.extend(std::iter::repeat_n(0.0, take));
        } else {
            self.pre.extend_from_slice(&samples[..take]);
        }
        if self.pre.len() < need {
            return Phase::Collecting {
                base,
                next: pos + samples.len() as u64,
            };
        }
        let Some(cal) = calibrate(&self.pre[self.settle_frames..], self.rate_hz) else {
            self.summary.error = Some("pilot did not calibrate");
            return Phase::Failed("pilot did not calibrate");
        };
        self.summary.calibration_amplitude = Some(cal.amplitude);
        self.summary.onset_position = self.onset(base, cal);
        let mut d = Detector::new(self.rate_hz, cal);
        // The calibration capture itself is not evaluated; the rest of
        // this packet is held like any later packet.
        let rest = &samples[take..];
        self.released_to = pos + take as u64;
        if !rest.is_empty() {
            self.hold(pos + take as u64, flags, rest);
        }
        // An empty push fixes the detector's base position.
        d.push_packet(self.released_to, PacketFlags::default(), &[]);
        Phase::Detecting(Box::new(d))
    }

    fn onset(&self, base: u64, cal: Calibration) -> Option<u64> {
        let mut f = OnsetFinder::new(self.rate_hz, cal);
        f.push_packet(base, PacketFlags::default(), &self.pre);
        f.onset()
    }

    fn hold(&mut self, pos: u64, flags: PacketFlags, samples: &[f32]) {
        self.held.push_back((pos, flags, samples.len()));
        self.held_samples.extend_from_slice(samples);
    }

    /// Hands held packets to the detector until at most `keep` frames are
    /// held.
    fn release(&mut self, keep: usize) {
        let Phase::Detecting(d) = &mut self.phase else {
            return;
        };
        while self.held_samples.len() > keep {
            let Some((pos, flags, len)) = self.held.pop_front() else {
                break;
            };
            d.push_packet(pos, flags, &self.held_samples[..len]);
            self.held_samples.drain(..len);
            self.released_to = pos + len as u64;
        }
    }

    /// Ends the evaluation. `stopped_us` is when the render stream stopped
    /// (run µs); capture from `STOP_MARGIN_US` before it on is dropped.
    pub fn finish(mut self, stopped_us: Option<i64>) -> (AudioFacts, CaptureSummary) {
        let map = (self.anchors.len() >= MIN_ANCHORS)
            .then(|| LinearMap::fit(&self.anchors))
            .flatten();
        self.summary.anchors_used = self.anchors.len();
        self.summary.map_error_us = map.map(|m| m.max_residual_us);
        let cut = match (map, stopped_us) {
            (Some(m), Some(s)) => Some(m.position_at(s - STOP_MARGIN_US)),
            _ => None,
        };
        self.summary.cut_position = cut;
        let mut facts = AudioFacts::default();
        let phase = std::mem::replace(&mut self.phase, Phase::Waiting);
        let Phase::Detecting(mut d) = phase else {
            if let Phase::Failed(e) = phase {
                self.summary.error = Some(e);
            }
            return (facts, self.summary);
        };
        match cut {
            Some(c) if c < self.released_to => self.summary.cut_late = true,
            Some(c) => {
                while let Some((pos, flags, len)) = self.held.pop_front() {
                    if pos >= c {
                        break;
                    }
                    let n = len.min((c - pos) as usize);
                    d.push_packet(pos, flags, &self.held_samples[..n]);
                    self.held_samples.drain(..len);
                }
            }
            // No map or no stop time: everything captured is evaluated.
            None => {
                while let Some((pos, flags, len)) = self.held.pop_front() {
                    d.push_packet(pos, flags, &self.held_samples[..len]);
                    self.held_samples.drain(..len);
                }
            }
        }
        let report: DetectorReport = d.finish();
        facts.pilot_calibrated = true;
        facts.unverifiable_spans = report.unverifiable.len() as u32;
        self.summary.episode_positions = report.episodes.iter().map(|e| e.start_sample).collect();
        let Some(m) = map else {
            self.summary.error = Some("too few capture timestamps to map positions");
            return (facts, self.summary);
        };
        if let Some(span) = report.evaluated
            && m.max_residual_us <= MAX_MAP_ERROR_US
        {
            // Narrowed by the map's error bound, so coverage is never
            // claimed beyond what the timestamps establish.
            facts.capture_from_us = Some(m.at(span.start) + m.max_residual_us);
            facts.capture_to_us = Some(m.at(span.end) - m.max_residual_us);
        }
        facts.episodes = report
            .episodes
            .iter()
            .map(|e| Episode {
                start_us: m.at(e.start_sample),
                end_us: m.at(e.end_sample),
            })
            .collect();
        (facts, self.summary)
    }

    pub fn anchors(&self) -> &[(u64, i64)] {
        &self.anchors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pilot::PilotOscillator;

    const RATE: u32 = 48_000;
    const PACKET: usize = 480;

    /// A capture of `ms`: silence until the render stream starts at
    /// packet 10, its pilot stepping in 250 ms later, packets stamped
    /// `1_000_000 + 10 ms × k` µs plus `jitter(k)`, with the flags of
    /// `flags(k)`. Returns the evaluation's result for a stream stopped
    /// at `stop_us`.
    fn run(
        ms: usize,
        jitter: impl Fn(usize) -> i64,
        flags: impl Fn(usize) -> CaptureFlags,
        stop_us: i64,
    ) -> (AudioFacts, CaptureSummary) {
        let mut osc = PilotOscillator::new(RATE);
        let mut e = CaptureEval::new(RATE, 500_000, 1_000_000, 100_000);
        let start = 10 * PACKET;
        let pilot_from = start + 12_000;
        let packets = ms * RATE as usize / 1000 / PACKET;
        let mut buf = vec![0.0f32; PACKET];
        for k in 0..packets {
            for (i, x) in buf.iter_mut().enumerate() {
                let f = k * PACKET + i;
                let t_us = 1_000_000 + (f as i64 * 1_000_000 / i64::from(RATE));
                *x = if f >= pilot_from && t_us < stop_us {
                    osc.next_sample()
                } else {
                    0.0
                };
            }
            let t = 1_000_000 + k as i64 * 10_000 + jitter(k);
            e.push((k * PACKET) as u64, flags(k), t, &buf, k >= 10);
        }
        e.finish(Some(stop_us))
    }

    #[test]
    fn coverage_ends_at_the_stop_cut_and_the_stop_is_not_an_episode() {
        // The stream stops at 4.5 s run time (3.5 s into the capture),
        // which goes on 60 ms longer in silence.
        let (facts, s) = run(3_560, |_| 0, |_| CaptureFlags::default(), 4_500_000);
        assert_eq!(facts.episodes, vec![], "{s:?}");
        let to = facts.capture_to_us.unwrap();
        // Cut 10 ms before the stop; the detector's last 3 ms settle.
        assert!((4_486_000..=4_490_000).contains(&to), "{to}");
        assert_eq!(
            s.onset_position
                .map(|p| p.abs_diff(10 * 480 + 12_000) <= 48),
            Some(true)
        );
        assert!(!s.cut_late);
    }

    #[test]
    fn the_startup_burst_is_left_out_of_the_map() {
        // The first three packets after the stream starts are stamped 13 ms
        // early, as the RME's loopback does.
        let (_, s) = run(
            3_560,
            |k| if (10..13).contains(&k) { -13_000 } else { 0 },
            |_| CaptureFlags::default(),
            4_500_000,
        );
        assert_eq!(s.map_error_us, Some(0));
    }

    // Codex P3 finding 1: one +100 ms timestamp marked TIMESTAMP_ERROR
    // must not stretch the map.
    #[test]
    fn uncertain_timestamps_are_left_out_of_the_map() {
        let bad = |k: usize| k == 300;
        let (facts, s) = run(
            3_560,
            |k| if bad(k) { 100_000 } else { 0 },
            |k| CaptureFlags {
                timestamp_error: bad(k),
                ..Default::default()
            },
            4_500_000,
        );
        assert_eq!(s.timestamp_errors, 1);
        assert_eq!(s.map_error_us, Some(0));
        assert!((4_486_000..=4_490_000).contains(&facts.capture_to_us.unwrap()));
    }

    #[test]
    fn an_unflagged_bad_timestamp_widens_the_bound_or_voids_coverage() {
        // ±3 ms jitter narrows coverage by the bound.
        let (facts, s) = run(
            3_560,
            |k| if k % 2 == 0 { 3_000 } else { -3_000 },
            |_| CaptureFlags::default(),
            4_500_000,
        );
        let err = s.map_error_us.unwrap();
        assert!((2_900..=3_100).contains(&err), "{err}");
        assert!(facts.capture_to_us.unwrap() <= 4_490_000 - 2_900);
        // An outlier past the bound: coverage is not established.
        let (facts, s) = run(
            3_560,
            |k| if k == 300 { 100_000 } else { 0 },
            |_| CaptureFlags::default(),
            4_500_000,
        );
        assert!(s.map_error_us.unwrap() > MAX_MAP_ERROR_US);
        assert_eq!((facts.capture_from_us, facts.capture_to_us), (None, None));
    }
}
