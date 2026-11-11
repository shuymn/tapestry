//! Timers: the built-in namespace 0..=2999, custom timers above
//! it, and the rank timers as `when` conditions.

use std::collections::{BTreeMap, BTreeSet};

use skin_model::{ScreenKind, TimerRef, timer};

/// The built-in timer namespace (`TimerManager.java:20, 59-64` at
/// `0.8.9`): ids at or below it are the player's.
pub(crate) const BUILTIN_MAX: i32 = 2_999;

/// The built-in timer a survival gauge's failure starts
/// (`SkinProperty.java`, `TIMER_FAILED`).
const FAILED: i32 = 3;

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

/// Built-in timer `id`.
pub(crate) fn builtin(id: i32) -> Timer {
    let slot = match id {
        // Proven unset: nothing sets timer 0 (`TimerPropertyFactory.java:
        // 6-10`, `JsonSkinSerializer.java:109`).
        0 => return Timer::Unset,
        // Keys 8 and 9's key-on and key-off (`SkinProperty.java`): this
        // 7-key mode has no such lanes, so they are never set and never
        // alias another lane.
        108 | 109 | 128 | 129 => return Timer::Unset,
        40 => timer::READY,
        41 => timer::PLAY,
        3 => timer::FAILED,
        143 => timer::LAST_NOTE,
        48 => timer::FULL_COMBO,
        44 => timer::GAUGE_MAX,
        // The rhythm clock (`play/RhythmTimerProcessor.java:60-71`).
        140 => timer::RHYTHM,
        // Every judgment starts both, whichever widget draws it, where the
        // source sets them only for a judge region its judge widget makes
        // (`JudgeManager.java:129, 675-690`).
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
///
/// It waits for the round's own judgment (a count of any kind, the empty
/// POOR's included): the EX score reads the play state's carried score
/// data until then (design.md "beatoraja Lua Skins", the program), and the
/// source's rank timers are switched only at a judgment
/// (`BMSPlayer.java:1040-1042` at `0.8.9`), so a carried EX score turning
/// them on over another section's targets would be a rank state the source
/// never enters (s7-06s-design.md §2.4). Once the round has a judgment the
/// EX score is its own, so the guard changes nothing there.
pub(crate) fn rank_source(k: usize) -> Option<String> {
    let &(_, i) = RANKS.get(k)?;
    Some(format!(
        "(count.perfect > 0 || count.great > 0 || count.good > 0 || count.bad > 0 \
         || count.poor > 0 || count.empty_poor > 0) \
         && notes > 0 && 27 * ex_score >= {} * notes",
        2 * i
    ))
}

/// The declared custom timers: id to the timer it reads, or `None` when
/// it is anything else (passive, a script).
#[derive(Debug, Default)]
pub(crate) struct Custom {
    pub(crate) aliases: BTreeMap<i32, Option<i32>>,
    /// The custom ids whose numeric chain reaches `failed`
    /// ([`Self::classify`]).
    failing: BTreeSet<i32>,
}

impl Custom {
    /// Classifies the custom timers once they are declared: each one
    /// whose chain of numeric targets reaches built-in 3 (`failed`) fails
    /// with it. A custom timer reads its target's time
    /// (`skin/CustomTimer.java:43-47`, `TimerManager.java:59-65`,
    /// `skin/Skin.java:752-758`), a custom one among them, updated each
    /// frame (`skin/Skin.java:783-785`), so a chain follows the failure; a
    /// cycle, a passive or script timer, or an undeclared id never does.
    /// Each id is walked once: the work is linear in the declared timers.
    /// Classification only: [`Self::resolve`] still maps aliases of
    /// built-ins alone.
    pub(crate) fn classify(&mut self) {
        let mut done: BTreeMap<i32, bool> = BTreeMap::new();
        let mut path = Vec::new();
        let mut on_path = BTreeSet::new();
        for &start in self.aliases.keys() {
            let mut id = start;
            // Follow the chain until a known outcome, a built-in, a dead
            // end or a repeat on this path.
            let fails = loop {
                if let Some(&f) = done.get(&id) {
                    break f;
                }
                if !on_path.insert(id) {
                    break false;
                }
                path.push(id);
                match self.aliases.get(&id).copied().flatten() {
                    Some(t) if t <= BUILTIN_MAX => break t == FAILED,
                    Some(t) => id = t,
                    None => break false,
                }
            };
            for id in path.drain(..) {
                done.insert(id, fails);
            }
            on_path.clear();
        }
        self.failing = done
            .into_iter()
            .filter(|&(_, f)| f)
            .map(|(id, _)| id)
            .collect();
    }

    /// Whether timer `id` (non-negative) resolves to `failed`: built-in 3,
    /// or a custom timer whose chain reaches it ([`Self::classify`]).
    pub(crate) fn fails(&self, id: i32) -> bool {
        if id <= BUILTIN_MAX {
            id == FAILED
        } else {
            self.failing.contains(&id)
        }
    }

    /// Timer `id` (non-negative) on a `kind` screen, resolved through the
    /// custom timers: play's ([`Self::resolve`]), or the selection,
    /// decision or result profile's ([`super::selection::profile::timer`],
    /// [`super::decision::profile::timer`],
    /// [`super::result::profile::timer`]), where any other
    /// built-in, play's among them, is undefined and 0 is never set.
    pub(crate) fn resolve_in(&self, kind: ScreenKind, id: i32) -> Timer {
        let builtin = |id: i32| match kind {
            ScreenKind::Result => super::result::profile::timer(id),
            ScreenKind::Decision => super::decision::profile::timer(id),
            _ if id == 0 => Timer::Unset,
            _ => super::selection::profile::timer(id).map_or(Timer::Unsupported, Timer::Builtin),
        };
        if kind == ScreenKind::Play {
            return self.resolve(id);
        }
        if id <= BUILTIN_MAX {
            return builtin(id);
        }
        match self.aliases.get(&id) {
            Some(&Some(target)) if (0..=BUILTIN_MAX).contains(&target) => builtin(target),
            _ => Timer::Unsupported,
        }
    }

    /// Timer `id` (non-negative) resolved through the custom timers. An
    /// alias of a timer never set is never set.
    pub(crate) fn resolve(&self, id: i32) -> Timer {
        if id <= BUILTIN_MAX {
            return builtin(id);
        }
        match self.aliases.get(&id) {
            Some(&Some(target)) if (0..=BUILTIN_MAX).contains(&target) => builtin(target),
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
                assert_eq!(builtin(first + i32::from(lane)), Timer::Builtin(want));
            }
        }
        for id in [108, 109, 128, 129] {
            assert_eq!(builtin(id), Timer::Unset, "{id}");
        }
        assert_eq!(builtin(58), Timer::Unsupported);
        assert_eq!(builtin(2), Timer::Unsupported);
        assert_eq!(builtin(140), Timer::Builtin(timer::RHYTHM));
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
            ..Custom::default()
        };
        assert_eq!(c.resolve(10_000), Timer::Builtin(timer::PLAY));
        assert_eq!(c.resolve(10_001), Timer::Unsupported);
        assert_eq!(c.resolve(10_005), Timer::Unset);
        assert_eq!(c.resolve(10_006), Timer::Builtin(timer::JUDGE));
        assert_eq!(c.resolve(446), Timer::Builtin(timer::JUDGE));
        assert_eq!(c.resolve(10_002), Timer::Unsupported);
        assert_eq!(c.resolve(10_003), Timer::Unsupported);
        assert_eq!(c.resolve(10_004), Timer::Unsupported);
    }

    #[test]
    fn failure_follows_numeric_chains_only() {
        let mut c = Custom {
            aliases: BTreeMap::from([
                (10_000, Some(3)),
                (10_001, Some(10_000)),
                (10_002, Some(10_001)),
                // A cycle, one that enters it, a dead end and another
                // built-in.
                (10_003, Some(10_004)),
                (10_004, Some(10_003)),
                (10_005, Some(10_003)),
                (10_006, None),
                (10_007, Some(10_006)),
                (10_008, Some(41)),
                (10_009, Some(10_010)),
            ]),
            ..Custom::default()
        };
        c.classify();
        for id in [3, 10_000, 10_001, 10_002] {
            assert!(c.fails(id), "{id}");
        }
        for id in [
            0, 2, 41, 10_003, 10_004, 10_005, 10_006, 10_007, 10_008, 10_009, 10_010,
        ] {
            assert!(!c.fails(id), "{id}");
        }
        // Classification only: a chain still resolves to no model timer.
        assert_eq!(c.resolve(10_000), Timer::Builtin(timer::FAILED));
        assert_eq!(c.resolve(10_001), Timer::Unsupported);
    }

    #[test]
    fn a_long_chain_is_walked_once() {
        // 100 000 links to `failed`, declared in an order that meets each
        // link before its target: each id is classified once.
        let n = 100_000;
        let mut aliases: BTreeMap<i32, Option<i32>> =
            (0..n).map(|i| (10_000 + i, Some(10_001 + i))).collect();
        aliases.insert(10_000 + n, Some(3));
        let mut c = Custom {
            aliases,
            ..Custom::default()
        };
        c.classify();
        assert!(c.fails(10_000) && c.fails(10_000 + n));
    }
}
