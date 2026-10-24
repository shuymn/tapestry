//! Synthetic input and the stand-in judgment (design D2 "Input/judgment
//! stand-in", D4).
//!
//! Every note in play gets exactly one synthetic press at its time plus a
//! seeded jitter of up to ±20 ms (V8: one input record per note). The
//! logic thread judges each press against the earliest unjudged note of its
//! lane within the window: trivial, but it walks the chart the way a real
//! judgment would.
//! `Play::advance_to` runs the same presses and judgments without threads;
//! the threaded run must reach the same judgment count.
//!
//! Times are µs since play start; the chart loops every `CHART_LEN_US`.

use crate::chart::{CHART_LEN_US, Chart, LANES};
use crate::rng::SplitMix64;

/// Largest press jitter around the note time.
pub const PRESS_JITTER_US: i64 = 20_000;
/// A press judges a note at most this far from it.
pub const JUDGE_WINDOW_US: i64 = 100_000;
/// Seed of the press jitter.
pub const PRESS_SEED: u64 = 0x5EED_0000_1A9D;

/// One synthetic press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Press {
    /// When the input thread emits it (µs since play start).
    pub t_us: i64,
    pub lane: u8,
}

/// A judged press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Judgment {
    /// Index of the judged note among the notes in play.
    pub note: u32,
    /// Press time minus note time.
    pub delta_us: i64,
    /// Note time (µs since play start).
    pub note_us: i64,
    pub lane: u8,
    pub sound: u16,
}

/// A note in play: the chart note of loop `k` at `k × CHART_LEN_US` later.
#[derive(Clone, Copy, Debug)]
struct PlayNote {
    t_us: i64,
    sound: u16,
}

/// The notes of `play_us` of looped play, the presses for them (sorted by
/// time), and the judgment state. Everything is allocated in `new`; pressing
/// and judging never allocate.
#[derive(Clone, Debug)]
pub struct Play {
    presses: Vec<Press>,
    next_press: usize,
    judge: Judge,
}

impl Play {
    pub fn new(chart: &Chart, play_us: i64) -> Self {
        let mut notes = Vec::new();
        let mut lanes: Vec<Vec<u32>> = vec![Vec::new(); usize::from(LANES)];
        let mut presses = Vec::new();
        let mut rng = SplitMix64::new(PRESS_SEED);
        let mut base = 0;
        while base < play_us {
            for n in &chart.notes {
                let t = base + n.time_us;
                if t >= play_us {
                    break;
                }
                let index = notes.len() as u32;
                notes.push(PlayNote {
                    t_us: t,
                    sound: n.sound,
                });
                lanes[usize::from(n.lane)].push(index);
                let jitter = rng.below(2 * PRESS_JITTER_US as u64 + 1) as i64 - PRESS_JITTER_US;
                presses.push(Press {
                    t_us: t + jitter,
                    lane: n.lane,
                });
            }
            base += CHART_LEN_US;
        }
        presses.sort_by_key(|p| p.t_us);
        let judged = vec![false; notes.len()];
        Self {
            presses,
            next_press: 0,
            judge: Judge {
                notes,
                cursor: vec![0; lanes.len()],
                lanes,
                judged,
                count: 0,
            },
        }
    }

    /// Notes in play (= presses).
    pub fn notes_total(&self) -> u32 {
        self.presses.len() as u32
    }

    pub fn presses(&self) -> &[Press] {
        &self.presses
    }

    /// The presses due at or before `t_us`, in order, not yet emitted.
    pub fn take_due(&mut self, t_us: i64) -> &[Press] {
        let from = self.next_press;
        while self
            .presses
            .get(self.next_press)
            .is_some_and(|p| p.t_us <= t_us)
        {
            self.next_press += 1;
        }
        &self.presses[from..self.next_press]
    }

    pub fn judge_mut(&mut self) -> &mut Judge {
        &mut self.judge
    }

    /// Splits the play into the press list (input thread) and the judge
    /// (logic thread).
    pub fn split(self) -> (Vec<Press>, Judge) {
        (self.presses, self.judge)
    }

    /// Threadless run: emits and judges every press up to `t_us`, calling
    /// `f` for each judgment.
    pub fn advance_to(&mut self, t_us: i64, mut f: impl FnMut(Judgment)) {
        while let Some(&p) = self.presses.get(self.next_press)
            && p.t_us <= t_us
        {
            self.next_press += 1;
            if let Some(j) = self.judge.judge(p) {
                f(j);
            }
        }
    }
}

/// Per lane, a press judges the earliest unjudged note within
/// `JUDGE_WINDOW_US` (the usual rule; nearest-note would leave notes
/// unjudged where a lane's notes are closer than the press jitter, as in
/// the ×4 BPM segments at 12.5 ms).
#[derive(Clone, Debug)]
pub struct Judge {
    notes: Vec<PlayNote>,
    /// Note indexes of each lane, by time.
    lanes: Vec<Vec<u32>>,
    /// Per lane: position of the first note that may still be unjudged.
    cursor: Vec<usize>,
    judged: Vec<bool>,
    count: u32,
}

impl Judge {
    /// Judges `press` against the earliest unjudged note of its lane within
    /// `JUDGE_WINDOW_US`.
    pub fn judge(&mut self, press: Press) -> Option<Judgment> {
        let lane = usize::from(press.lane);
        let list = self.lanes.get(lane)?;
        let mut c = self.cursor[lane];
        // Notes too old to be judged any more, or already judged, are
        // skipped for good.
        while let Some(&i) = list.get(c)
            && (self.judged[i as usize]
                || self.notes[i as usize].t_us < press.t_us - JUDGE_WINDOW_US)
        {
            c += 1;
        }
        self.cursor[lane] = c;
        let i = *list.get(c)?;
        if self.notes[i as usize].t_us > press.t_us + JUDGE_WINDOW_US {
            return None;
        }
        self.cursor[lane] = c + 1;
        self.judged[i as usize] = true;
        self.count += 1;
        let n = self.notes[i as usize];
        Some(Judgment {
            note: i,
            delta_us: press.t_us - n.t_us,
            note_us: n.t_us,
            lane: press.lane,
            sound: n.sound,
        })
    }

    /// Notes judged so far.
    pub fn judged(&self) -> u32 {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::DEFAULT_SEED;

    #[test]
    fn one_press_per_note_within_the_jitter_and_every_note_judged() {
        let chart = Chart::generate(DEFAULT_SEED);
        let mut play = Play::new(&chart, CHART_LEN_US + 10_000_000);
        // A whole loop plus the first 10 s of the next.
        let first_loop = chart.notes.len() as u32;
        let extra = chart
            .notes
            .iter()
            .filter(|n| n.time_us < 10_000_000)
            .count() as u32;
        assert_eq!(play.notes_total(), first_loop + extra);
        let mut n = 0;
        let mut worst = 0;
        play.advance_to(i64::MAX, |j| {
            n += 1;
            worst = worst.max(j.delta_us.abs());
        });
        assert_eq!(n, play.notes_total());
        assert_eq!(play.judge.judged(), n);
        // Where a lane's notes are closer than the jitter a press may judge
        // a neighbour, but never further than the window.
        assert!(worst <= JUDGE_WINDOW_US, "{worst}");
        assert!(play.presses().windows(2).all(|w| w[0].t_us <= w[1].t_us));
    }

    #[test]
    fn take_due_emits_each_press_once_in_order() {
        let chart = Chart::generate(DEFAULT_SEED);
        let mut play = Play::new(&chart, 5_000_000);
        let total = play.notes_total() as usize;
        let mut seen = 0;
        let mut t = -PRESS_JITTER_US;
        while t < 5_000_000 + PRESS_JITTER_US {
            let due = play.take_due(t);
            assert!(due.iter().all(|p| p.t_us <= t));
            seen += due.len();
            t += 1_000;
        }
        assert_eq!(seen, total);
        assert!(play.take_due(i64::MAX).is_empty());
    }

    #[test]
    fn a_press_far_from_every_note_is_not_judged() {
        let chart = Chart::generate(DEFAULT_SEED);
        let (_, mut judge) = Play::new(&chart, 1_000_000).split();
        assert_eq!(
            judge.judge(Press {
                t_us: 50_000_000,
                lane: 0
            }),
            None
        );
        assert_eq!(judge.judge(Press { t_us: 0, lane: 9 }), None);
    }
}
