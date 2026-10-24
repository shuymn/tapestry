//! The generated heavy chart (design D2).
//!
//! 8 lanes, 150 s, built in code from a seed (no BMS parsing):
//!
//! | Chart time (s) | Content |
//! | --- | --- |
//! | 0–10 | 30 notes/s |
//! | 10–40 | 80 notes/s, BPM change every 2 s (×0.25–×4), 4 STOPs |
//! | 40–70 | 8-note all-lane chords at 30 Hz (heavy window H1) |
//! | 70–110 | 60 notes/s, LN-heavy |
//! | 110–140 | 8-note all-lane chords at 30 Hz (heavy window H2) |
//! | 140–150 | 30 notes/s tail |

use crate::rng::SplitMix64;
use sha2::{Digest, Sha256};

pub const LANES: u8 = 8;
pub const CHART_LEN_US: i64 = 150_000_000;
pub const BASE_BPM: f64 = 150.0;
pub const SOUND_COUNT: u16 = 64;
pub const DEFAULT_SEED: u64 = 0x5EED_0000_0003;
/// Validity V1 floor.
pub const MIN_NOTES: usize = 10_000;

const SEC: i64 = 1_000_000;
/// BPM multipliers for the changes at 10, 12, …, 38 s: a sweep between ×0.25
/// and ×4 that moves one step (factor 2) at a time. A larger drop would
/// let the slow segment show more notes in the visible time than a heavy
/// window does (at ×4 → ×0.25, about 170 versus 80), so H1/H2 would no
/// longer hold the peak visible notes.
const BPM_SWEEP: [f64; 15] = [
    1.0, 2.0, 4.0, 2.0, 1.0, 0.5, 0.25, 0.5, 1.0, 2.0, 4.0, 2.0, 1.0, 0.5, 0.25,
];
const STOP_STARTS_US: [i64; 4] = [15_500_000, 21_500_000, 27_500_000, 33_500_000];
const STOP_LEN_US: i64 = 250_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note {
    pub time_us: i64,
    pub lane: u8,
    /// End of a long note; `None` for a tap.
    pub end_us: Option<i64>,
    pub sound: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TempoEvent {
    Bpm { time_us: i64, bpm: f64 },
    Stop { time_us: i64, duration_us: i64 },
}

impl TempoEvent {
    pub fn time_us(&self) -> i64 {
        match *self {
            Self::Bpm { time_us, .. } | Self::Stop { time_us, .. } => time_us,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chart {
    /// Sorted by `(time_us, lane)`.
    pub notes: Vec<Note>,
    /// Sorted by time; the first event is the initial BPM at 0.
    pub tempo: Vec<TempoEvent>,
}

impl Chart {
    pub fn generate(seed: u64) -> Self {
        let mut rng = SplitMix64::new(seed);
        let mut tempo = vec![TempoEvent::Bpm {
            time_us: 0,
            bpm: BASE_BPM,
        }];
        let mut notes = Vec::with_capacity(20_000);

        // 0–10 s: lead-in.
        taps(&mut notes, &mut rng, 0, 10 * SEC, 30, &[]);

        // 10–40 s: BPM change every 2 s, STOPs; notes avoid STOP spans.
        for (k, m) in (0i64..).zip(BPM_SWEEP) {
            tempo.push(TempoEvent::Bpm {
                time_us: 10 * SEC + k * 2 * SEC,
                bpm: BASE_BPM * m,
            });
        }
        for &s in &STOP_STARTS_US {
            tempo.push(TempoEvent::Stop {
                time_us: s,
                duration_us: STOP_LEN_US,
            });
        }
        tempo.push(TempoEvent::Bpm {
            time_us: 40 * SEC,
            bpm: BASE_BPM,
        });
        tempo.sort_by_key(TempoEvent::time_us);
        let stops: Vec<(i64, i64)> = STOP_STARTS_US
            .iter()
            .map(|&s| (s, s + STOP_LEN_US))
            .collect();
        taps(&mut notes, &mut rng, 10 * SEC, 40 * SEC, 80, &stops);

        // 40–70 s and 110–140 s: heavy windows.
        chords(&mut notes, &mut rng, 40 * SEC, 70 * SEC);
        long_notes(&mut notes, &mut rng, 70 * SEC, 110 * SEC);
        chords(&mut notes, &mut rng, 110 * SEC, 140 * SEC);
        taps(&mut notes, &mut rng, 140 * SEC, CHART_LEN_US, 30, &[]);

        notes.sort_by_key(|n| (n.time_us, n.lane));
        Self { notes, tempo }
    }

    /// SHA-256 over a fixed little-endian encoding of every note and tempo
    /// event, as lowercase hex.
    pub fn content_hash(&self) -> String {
        let mut h = Sha256::new();
        for n in &self.notes {
            h.update(n.time_us.to_le_bytes());
            h.update([n.lane]);
            h.update(n.end_us.unwrap_or(-1).to_le_bytes());
            h.update(n.sound.to_le_bytes());
        }
        for t in &self.tempo {
            match *t {
                TempoEvent::Bpm { time_us, bpm } => {
                    h.update([0]);
                    h.update(time_us.to_le_bytes());
                    h.update(bpm.to_bits().to_le_bytes());
                }
                TempoEvent::Stop {
                    time_us,
                    duration_us,
                } => {
                    h.update([1]);
                    h.update(time_us.to_le_bytes());
                    h.update(duration_us.to_le_bytes());
                }
            }
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn sound(rng: &mut SplitMix64) -> u16 {
    rng.below(u64::from(SOUND_COUNT)) as u16
}

/// Single taps at `per_s` notes per second on random lanes, skipping `holes`.
fn taps(
    notes: &mut Vec<Note>,
    rng: &mut SplitMix64,
    from: i64,
    to: i64,
    per_s: i64,
    holes: &[(i64, i64)],
) {
    let count = (to - from) * per_s / SEC;
    for i in 0..count {
        let time_us = from + i * SEC / per_s;
        if holes.iter().any(|&(a, b)| (a..b).contains(&time_us)) {
            continue;
        }
        notes.push(Note {
            time_us,
            lane: rng.below(u64::from(LANES)) as u8,
            end_us: None,
            sound: sound(rng),
        });
    }
}

/// All-lane chords at 30 Hz (240 notes/s).
fn chords(notes: &mut Vec<Note>, rng: &mut SplitMix64, from: i64, to: i64) {
    let count = (to - from) * 30 / SEC;
    for i in 0..count {
        let time_us = from + i * SEC / 30;
        for lane in 0..LANES {
            notes.push(Note {
                time_us,
                lane,
                end_us: None,
                sound: sound(rng),
            });
        }
    }
}

/// 60 notes/s, half of them long notes of 60–160 ms; a note goes to a random
/// lane that is free at its time (no overlap on a lane), and no long note
/// reaches past `to`.
fn long_notes(notes: &mut Vec<Note>, rng: &mut SplitMix64, from: i64, to: i64) {
    let mut busy_until = [i64::MIN; LANES as usize];
    let count = (to - from) * 60 / SEC;
    for i in 0..count {
        let time_us = from + i * SEC / 60;
        let free: Vec<u8> = (0..LANES)
            .filter(|&l| busy_until[l as usize] < time_us)
            .collect();
        if free.is_empty() {
            continue;
        }
        let lane = free[rng.below(free.len() as u64) as usize];
        let end_us = (rng.below(2) == 0)
            .then(|| time_us + 60_000 + rng.below(101) as i64 * 1000)
            .filter(|&e| e < to);
        busy_until[lane as usize] = end_us.unwrap_or(time_us);
        notes.push(Note {
            time_us,
            lane,
            end_us,
            sound: sound(rng),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // (j) The scene must hold at least 10,000 notes and hash stably.
    #[test]
    fn chart_has_at_least_10000_notes_and_a_stable_hash() {
        let a = Chart::generate(DEFAULT_SEED);
        assert!(a.notes.len() >= MIN_NOTES, "{} notes", a.notes.len());
        // Planned ~19,800 minus notes skipped for STOP spans and full lanes.
        assert!((19_000..=19_800).contains(&a.notes.len()));
        let b = Chart::generate(DEFAULT_SEED);
        assert_eq!(a.content_hash(), b.content_hash());
        assert_eq!(a.content_hash(), EXPECTED_HASH, "chart content changed");
        assert_ne!(Chart::generate(1).content_hash(), a.content_hash());
    }

    // Pinned so an accidental change to the generator is caught; update
    // deliberately (and record it) when the scene changes.
    const EXPECTED_HASH: &str = "2c29e2cb80c46533beb8feae84f7a123eac949c8a19878328fa1c94d4dc945b5";

    #[test]
    fn chart_structure_matches_the_scene_table() {
        let c = Chart::generate(DEFAULT_SEED);
        let in_span = |a: i64, b: i64| {
            c.notes
                .iter()
                .filter(|n| (a * SEC..b * SEC).contains(&n.time_us))
                .count()
        };
        assert_eq!(in_span(40, 70), 7200);
        assert_eq!(in_span(110, 140), 7200);
        assert_eq!(in_span(0, 10), 300);
        assert!(
            c.notes
                .iter()
                .all(|n| n.lane < LANES && n.sound < SOUND_COUNT)
        );
        assert!(
            c.notes
                .windows(2)
                .all(|w| (w[0].time_us, w[0].lane) < (w[1].time_us, w[1].lane))
        );
        assert_eq!(
            c.tempo
                .iter()
                .filter(|t| matches!(t, TempoEvent::Stop { .. }))
                .count(),
            4
        );
        // No note inside a STOP, and no tap or head on a lane while an LN holds it.
        for &s in &STOP_STARTS_US {
            assert!(
                !c.notes
                    .iter()
                    .any(|n| (s..s + STOP_LEN_US).contains(&n.time_us))
            );
        }
        let mut busy = [i64::MIN; LANES as usize];
        for n in &c.notes {
            assert!(
                busy[n.lane as usize] < n.time_us,
                "overlap on lane {}",
                n.lane
            );
            busy[n.lane as usize] = n.end_us.unwrap_or(i64::MIN);
        }
        assert!(c.notes.iter().filter(|n| n.end_us.is_some()).count() > 800);
    }
}
