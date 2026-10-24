//! Play timeline and heavy-window assignment (design §3.3).
//!
//! Play is `loops` back-to-back plays of the 150 s chart starting at
//! `play_start_us`. Heavy window instances are H1 (40–70 s) and H2
//! (110–140 s) of each loop; with 4 loops there are 8 instances (240 s).

use crate::chart::CHART_LEN_US;
use serde::{Deserialize, Serialize};

pub const HEAVY_SPANS_US: [(i64, i64); 2] = [(40_000_000, 70_000_000), (110_000_000, 140_000_000)];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timeline {
    pub play_start_us: i64,
    pub loops: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub start_us: i64,
    pub end_us: i64,
}

impl Timeline {
    pub fn play_end_us(&self) -> i64 {
        self.play_start_us + i64::from(self.loops) * CHART_LEN_US
    }

    pub fn in_play(&self, t_us: i64) -> bool {
        (self.play_start_us..self.play_end_us()).contains(&t_us)
    }

    pub fn window_count(&self) -> usize {
        self.loops as usize * HEAVY_SPANS_US.len()
    }

    /// Instance `i` (`loop * 2 + h`), half-open.
    pub fn window(&self, i: usize) -> Window {
        let (a, b) = HEAVY_SPANS_US[i % HEAVY_SPANS_US.len()];
        let base = self.play_start_us + (i / HEAVY_SPANS_US.len()) as i64 * CHART_LEN_US;
        Window {
            start_us: base + a,
            end_us: base + b,
        }
    }

    /// Heavy window instance containing `t_us`, if any.
    pub fn window_of(&self, t_us: i64) -> Option<usize> {
        if !self.in_play(t_us) {
            return None;
        }
        let rel = t_us - self.play_start_us;
        let lp = (rel / CHART_LEN_US) as usize;
        let pos = rel % CHART_LEN_US;
        HEAVY_SPANS_US
            .iter()
            .position(|&(a, b)| (a..b).contains(&pos))
            .map(|h| lp * HEAVY_SPANS_US.len() + h)
    }

    pub fn is_heavy(&self, t_us: i64) -> bool {
        self.window_of(t_us).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_instances_of_240_s_and_half_open_edges() {
        let tl = Timeline {
            play_start_us: 5_000_000,
            loops: 4,
        };
        assert_eq!(tl.window_count(), 8);
        let total: i64 = (0..8)
            .map(|i| tl.window(i).end_us - tl.window(i).start_us)
            .sum();
        assert_eq!(total, 240_000_000);
        for i in 0..8 {
            let w = tl.window(i);
            assert_eq!(tl.window_of(w.start_us), Some(i));
            assert_eq!(tl.window_of(w.end_us - 1), Some(i));
            assert_eq!(tl.window_of(w.end_us), None);
            assert_eq!(tl.window_of(w.start_us - 1), None);
        }
        assert_eq!(tl.window_of(tl.play_end_us()), None);
        assert_eq!(
            tl.window(7).start_us,
            5_000_000 + 3 * 150_000_000 + 110_000_000
        );
    }
}
