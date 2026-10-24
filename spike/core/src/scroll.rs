//! Scroll function and visible-note lookup (design D2).
//!
//! Scroll position is in beats: the integral of BPM over chart time, held
//! during STOPs. On screen, a note's distance from the judgment line is its
//! beat distance divided by the beats that pass in the constant visible time
//! (300 ms) at the current BPM. Both lookups are binary searches, so per-frame
//! work is proportional to the visible notes, not to the chart size.

use crate::chart::{Chart, TempoEvent};
use std::ops::Range;

pub const VISIBLE_TIME_US: i64 = 300_000;

#[derive(Clone, Copy, Debug)]
struct Segment {
    start_us: i64,
    start_beat: f64,
    /// 0 during a STOP.
    beats_per_us: f64,
    /// BPM that sets the on-screen scale (the last real BPM during a STOP).
    scale_bpm: f64,
}

#[derive(Clone, Debug)]
pub struct ScrollMap {
    segments: Vec<Segment>,
}

impl ScrollMap {
    /// `tempo` must be sorted and start with a BPM event at time 0.
    pub fn new(tempo: &[TempoEvent]) -> Self {
        let mut segments: Vec<Segment> = Vec::with_capacity(tempo.len() * 2);
        let mut bpm = match tempo.first() {
            Some(TempoEvent::Bpm { time_us: 0, bpm }) => *bpm,
            _ => panic!("tempo must start with a BPM event at time 0"),
        };
        // Tempo events are ordered; a STOP resumes at the BPM in effect.
        for ev in tempo {
            match *ev {
                TempoEvent::Bpm { time_us, bpm: b } => {
                    bpm = b;
                    Self::push(&mut segments, time_us, bpm / 60e6, bpm);
                }
                TempoEvent::Stop {
                    time_us,
                    duration_us,
                } => {
                    Self::push(&mut segments, time_us, 0.0, bpm);
                    Self::push(&mut segments, time_us + duration_us, bpm / 60e6, bpm);
                }
            }
        }
        Self { segments }
    }

    fn push(segments: &mut Vec<Segment>, start_us: i64, beats_per_us: f64, scale_bpm: f64) {
        let start_beat = segments.last().map_or(0.0, |s| {
            s.start_beat + (start_us - s.start_us) as f64 * s.beats_per_us
        });
        segments.push(Segment {
            start_us,
            start_beat,
            beats_per_us,
            scale_bpm,
        });
    }

    fn segment(&self, t_us: i64) -> &Segment {
        let i = self.segments.partition_point(|s| s.start_us <= t_us);
        &self.segments[i.saturating_sub(1)]
    }

    pub fn beat_at(&self, t_us: i64) -> f64 {
        let s = self.segment(t_us);
        s.start_beat + (t_us - s.start_us) as f64 * s.beats_per_us
    }

    /// Beats shown between the judgment line and the top of the lane at `t_us`.
    pub fn visible_beats(&self, t_us: i64) -> f64 {
        self.segment(t_us).scale_bpm / 60e6 * VISIBLE_TIME_US as f64
    }
}

/// Note heads in beat order with a binary-searchable visible range.
#[derive(Clone, Debug)]
pub struct NoteTrack {
    map: ScrollMap,
    /// Head beat of each chart note, same order as `Chart::notes`.
    beats: Vec<f64>,
}

impl NoteTrack {
    pub fn new(chart: &Chart) -> Self {
        let map = ScrollMap::new(&chart.tempo);
        let beats = chart.notes.iter().map(|n| map.beat_at(n.time_us)).collect();
        Self { map, beats }
    }

    pub fn map(&self) -> &ScrollMap {
        &self.map
    }

    /// Indices of notes whose head is between the judgment line and the top
    /// of the lane at `now_us`. Held LN bodies whose head has passed are drawn
    /// from the logic thread's held state, not from this range.
    pub fn visible(&self, now_us: i64) -> Range<usize> {
        let lo = self.map.beat_at(now_us);
        let hi = lo + self.map.visible_beats(now_us);
        let a = self.beats.partition_point(|&b| b < lo);
        let b = self.beats.partition_point(|&b| b <= hi);
        a..b
    }

    /// Position of note `i` at `now_us`: 0 at the judgment line, 1 at the top.
    pub fn y(&self, i: usize, now_us: i64) -> f64 {
        (self.beats[i] - self.map.beat_at(now_us)) / self.map.visible_beats(now_us)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::DEFAULT_SEED;

    #[test]
    fn beats_are_continuous_monotone_and_held_during_stops() {
        let c = Chart::generate(DEFAULT_SEED);
        let m = ScrollMap::new(&c.tempo);
        let mut prev = m.beat_at(0);
        for t in (0..150_000_000).step_by(1_000) {
            let b = m.beat_at(t);
            assert!(b >= prev);
            // At most 600 BPM: 0.01 beat per ms.
            assert!(b - prev <= 0.01 + 1e-9, "jump at {t}");
            prev = b;
        }
        for ev in &c.tempo {
            if let TempoEvent::Stop {
                time_us,
                duration_us,
            } = *ev
            {
                assert_eq!(m.beat_at(time_us), m.beat_at(time_us + duration_us));
            }
        }
        // Lead-in at 150 BPM: 25 beats in 10 s.
        assert!((m.beat_at(10_000_000) - 25.0).abs() < 1e-9);
    }

    #[test]
    fn visible_range_matches_a_linear_scan_and_peaks_in_heavy_windows() {
        let c = Chart::generate(DEFAULT_SEED);
        let track = NoteTrack::new(&c);
        let mut peak_light = 0;
        let mut peak_heavy = 0;
        for now in (0..150_000_000).step_by(37_000) {
            let r = track.visible(now);
            let lo = track.map.beat_at(now);
            let hi = lo + track.map.visible_beats(now);
            let scan: Vec<usize> = (0..track.beats.len())
                .filter(|&i| track.beats[i] >= lo && track.beats[i] <= hi)
                .collect();
            assert_eq!(scan, r.clone().collect::<Vec<_>>(), "at {now}");
            for i in r.clone() {
                let y = track.y(i, now);
                assert!((0.0..=1.0 + 1e-9).contains(&y));
            }
            let heavy = (40_000_000..70_000_000).contains(&now)
                || (110_000_000..140_000_000).contains(&now);
            if heavy {
                peak_heavy = peak_heavy.max(r.len());
            } else {
                peak_light = peak_light.max(r.len());
            }
        }
        assert!(peak_heavy > peak_light, "{peak_heavy} vs {peak_light}");
        // 300 ms of 30 Hz chords: 9 or 10 chords of 8 notes.
        assert!(peak_heavy >= 72);
    }
}
