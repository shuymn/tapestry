//! Timers: the built-in namespace 0..=2999, custom timers above
//! it, and the rank timers as `when` conditions.

use std::collections::BTreeMap;

use skin_model::{TimerRef, timer};

/// The built-in timer namespace (`TimerManager.java:20, 59-64` at
/// `0.8.9`): ids at or below it are the player's.
pub(crate) const BUILTIN_MAX: i32 = 2_999;

/// The rank timers, A, AA and AAA (`SkinProperty.java:49-51`), and their
/// thresholds in 27ths of the maximum EX score (`BMSPlayer.java:1040-1042`,
/// `ScoreDataProperty.java:64-110`).
pub(crate) const RANKS: [(i32, i32); 3] = [(348, 18), (349, 21), (350, 24)];

/// The per-lane timer blocks (`LaneProperty.java:46-51`): the first id
/// (offset 0 is the scratch, the model's lane 0) and the model's kind
/// (`press`, `release`, `bomb`, `hold`, in slot order).
const LANE_BLOCKS: [(i32, u16); 4] = [(100, 0), (120, 1), (50, 2), (70, 3)];

/// A timer as the importer maps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Timer {
    /// A model built-in.
    Builtin(u16),
    /// Rank timer `k` of [`RANKS`], a `when` condition.
    Rank(usize),
    /// Proven never set: never drawn as a destination timer, frame 0
    /// forever as a cycle timer.
    Unset,
    /// No model timer.
    Unsupported,
}

/// Built-in timer `id` in a skin that has a judge region (`judge_region`)
/// or not.
pub(crate) fn builtin(id: i32, judge_region: bool) -> Timer {
    let slot = match id {
        // Proven unset: nothing sets timer 0 (`TimerPropertyFactory.java:
        // 6-10`, `JsonSkinSerializer.java:109`).
        0 => return Timer::Unset,
        // Set only for a judge region, which a judge destination makes
        // (`JsonPlaySkinObjectLoader.java:283-287`, `JudgeManager.java:
        // 129, 677-690`).
        46 | 446 if !judge_region => return Timer::Unset,
        40 => timer::READY,
        41 => timer::PLAY,
        3 => timer::FAILED,
        143 => timer::LAST_NOTE,
        48 => timer::FULL_COMBO,
        44 => timer::GAUGE_MAX,
        // 446 is set with 46 on every judgment (`JudgeManager.java:675-690`).
        46 | 446 => timer::JUDGE,
        _ => {
            if let Some(k) = RANKS.iter().position(|&(r, _)| r == id) {
                return Timer::Rank(k);
            }
            let lane = LANE_BLOCKS.iter().find_map(|&(first, kind)| {
                let lane = u16::try_from(id.checked_sub(first)?).ok()?;
                (lane < 8).then(|| timer::LANES + kind * 8 + lane)
            });
            match lane {
                Some(slot) => slot,
                None => return Timer::Unsupported,
            }
        }
    };
    Timer::Builtin(slot)
}

/// The rank `when` condition of [`RANKS`] entry `k`: the EX score at
/// least `i/27` of the maximum. Exact where the source divides in `f32`:
/// a false exact test is at least `g/(54n)` below the threshold (`g =
/// gcd(27, 2i) ≥ 3`) and each rounding is within 2^-25, so the two agree
/// for every target count up to 932,067, past the parser's 500,000-object
/// cap.
pub(crate) fn rank_source(k: usize) -> Option<String> {
    let &(_, i) = RANKS.get(k)?;
    Some(format!("notes > 0 && 27 * ex_score >= {} * notes", 2 * i))
}

/// The declared custom timers: id to the built-in it aliases, or `None`
/// when it is anything else (passive, a chain, a script); and whether the
/// skin has a judge region.
#[derive(Debug, Default)]
pub(crate) struct Custom {
    pub(crate) aliases: BTreeMap<i32, Option<i32>>,
    pub(crate) judge_region: bool,
}

impl Custom {
    /// Timer `id` (non-negative) resolved through the custom timers. An
    /// alias of a timer never set is never set.
    pub(crate) fn resolve(&self, id: i32) -> Timer {
        if id <= BUILTIN_MAX {
            return builtin(id, self.judge_region);
        }
        match self.aliases.get(&id) {
            Some(&Some(target)) if (0..=BUILTIN_MAX).contains(&target) => {
                match builtin(target, self.judge_region) {
                    t @ (Timer::Builtin(_) | Timer::Rank(_) | Timer::Unset) => t,
                    Timer::Unsupported => Timer::Unsupported,
                }
            }
            _ => Timer::Unsupported,
        }
    }
}

/// The model reference of a mapped timer, given the rank `when` indices.
pub(crate) fn model_ref(t: Timer, whens: &[Option<u16>; 3]) -> Option<TimerRef> {
    match t {
        Timer::Builtin(slot) => Some(TimerRef::Builtin(slot)),
        Timer::Rank(k) => whens.get(k).copied().flatten().map(TimerRef::When),
        Timer::Unset | Timer::Unsupported => None,
    }
}

#[cfg(test)]
mod tests {
    use bms_model::Lane;
    use skin_model::{LaneTimer, lane_timer};

    use super::*;

    #[test]
    fn lane_blocks_match_the_model_slots() {
        for (first, kind) in [
            (100, LaneTimer::Press),
            (120, LaneTimer::Release),
            (50, LaneTimer::Bomb),
            (70, LaneTimer::Hold),
        ] {
            for lane in 0..8_u8 {
                let want = lane_timer(kind, Lane::new(lane).unwrap());
                assert_eq!(builtin(first + i32::from(lane), true), Timer::Builtin(want));
            }
        }
        assert_eq!(builtin(58, true), Timer::Unsupported);
        assert_eq!(builtin(2, true), Timer::Unsupported);
        assert_eq!(builtin(140, true), Timer::Unsupported);
    }

    #[test]
    fn custom_timers_alias_supported_built_ins_only() {
        let c = Custom {
            aliases: BTreeMap::from([
                (10_000, Some(41)),
                (10_001, Some(2)),
                (10_002, None),
                (10_003, Some(10_000)),
                (10_005, Some(0)),
                (10_006, Some(46)),
            ]),
            judge_region: false,
        };
        assert_eq!(c.resolve(10_000), Timer::Builtin(timer::PLAY));
        assert_eq!(c.resolve(10_001), Timer::Unsupported);
        assert_eq!(c.resolve(10_005), Timer::Unset);
        assert_eq!(c.resolve(10_006), Timer::Unset);
        assert_eq!(c.resolve(46), Timer::Unset);
        assert_eq!(c.resolve(446), Timer::Unset);
        let c = Custom {
            judge_region: true,
            ..c
        };
        assert_eq!(c.resolve(10_006), Timer::Builtin(timer::JUDGE));
        assert_eq!(c.resolve(446), Timer::Builtin(timer::JUDGE));
        let c = Custom {
            judge_region: false,
            ..c
        };
        assert_eq!(c.resolve(10_002), Timer::Unsupported);
        assert_eq!(c.resolve(10_003), Timer::Unsupported);
        assert_eq!(c.resolve(10_004), Timer::Unsupported);
    }
}
