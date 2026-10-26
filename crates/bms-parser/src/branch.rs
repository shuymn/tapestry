//! `#RANDOM` blocks: which lines are taken, and the values drawn.

use crate::{LoadError, Warning, WarningKind};

/// SplitMix64, our own so the stream never changes with a dependency (D13).
#[derive(Clone, Copy, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub const fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `1..=n` as `next() % n + 1`.
    pub fn draw(&mut self, n: u32) -> u32 {
        let r = self.next() % u64::from(n);
        // r < n, so it fits; the +1 cannot overflow because r < u32::MAX.
        u32::try_from(r).map_or(n, |r| r + 1)
    }
}

/// One open `#IF`: `active` while its current branch is read, `taken`
/// once any branch of it was, and `random` the number of open `#RANDOM`
/// blocks when it was met.
#[derive(Clone, Copy, Debug)]
struct Flag {
    active: bool,
    taken: bool,
    random: usize,
}

/// `#RANDOM` and `#IF` as charts are written against (stage1.md PR3; the
/// reference is jbms-parser's `BMSDecoder`): two flat stacks, not nested
/// blocks. `#RANDOM` pushes a value and `#ENDRANDOM` pops one; `#IF k`
/// pushes a flag, active when the latest value is `k`, and `#ENDIF` pops
/// whatever flag is on top. A line is read when the top flag is active or
/// no flag is open. `#ELSE` is our addition: it turns the top flag to the
/// branch no earlier one took. Each line costs O(1) whatever the stacks
/// hold, and the line limit bounds them.
#[derive(Debug)]
pub struct Branches {
    rng: SplitMix64,
    /// The values of the open `#RANDOM` and `#SETRANDOM` blocks.
    randoms: Vec<u32>,
    flags: Vec<Flag>,
    /// An `#IF` stacked on one of the same `#RANDOM` was warned about.
    warned_stacked: bool,
    /// An `#IF` without a `#RANDOM` was warned about.
    warned_orphan: bool,
    /// Every `#RANDOM` and `#SETRANDOM` value, in order.
    pub results: Vec<u32>,
}

impl Branches {
    pub const fn new(seed: u64) -> Self {
        Self {
            rng: SplitMix64::new(seed),
            randoms: Vec::new(),
            flags: Vec::new(),
            warned_stacked: false,
            warned_orphan: false,
            results: Vec::new(),
        }
    }

    /// Whether lines at this point are read.
    pub fn active(&self) -> bool {
        self.flags.last().is_none_or(|f| f.active)
    }

    /// Handles a branch directive, whether or not lines are read here.
    /// Returns `Ok(false)` when `name` is not one.
    pub fn directive(
        &mut self,
        name: &str,
        value: &str,
        line: usize,
        warnings: &mut Vec<Warning>,
    ) -> Result<bool, LoadError> {
        let warn = |warnings: &mut Vec<Warning>, kind| {
            warnings.push(Warning {
                line: Some(line),
                kind,
            });
        };
        match name {
            // Drawn even inside an #IF branch not read, as charts expect.
            "RANDOM" => {
                let n = positive_integer(value, line, "#RANDOM")?;
                let v = self.rng.draw(n);
                self.randoms.push(v);
                self.results.push(v);
            }
            "SETRANDOM" => {
                let v = positive_integer(value, line, "#SETRANDOM")?;
                self.randoms.push(v);
                self.results.push(v);
            }
            "IF" => {
                let k = positive_integer(value, line, "#IF")?;
                match self.randoms.last() {
                    // No value to test: no flag, as charts expect, so the
                    // lines are read and the next #ENDIF pops whatever flag
                    // is on top.
                    None => {
                        if !std::mem::replace(&mut self.warned_orphan, true) {
                            warn(warnings, WarningKind::UnmatchedDirective("#IF"));
                        }
                    }
                    Some(&v) => {
                        let random = self.randoms.len();
                        if self.flags.last().is_some_and(|f| f.random == random)
                            && !std::mem::replace(&mut self.warned_stacked, true)
                        {
                            warn(warnings, WarningKind::UnclosedIf);
                        }
                        self.flags.push(Flag {
                            active: v == k,
                            taken: v == k,
                            random,
                        });
                    }
                }
            }
            "ELSE" => {
                if let Some(f) = self.flags.last_mut() {
                    f.active = !f.taken;
                    f.taken = true;
                } else {
                    warn(warnings, WarningKind::UnmatchedDirective("#ELSE"));
                }
            }
            "ENDIF" => {
                if self.flags.pop().is_none() {
                    warn(warnings, WarningKind::UnmatchedDirective("#ENDIF"));
                }
            }
            "ENDRANDOM" => {
                if self.randoms.pop().is_none() {
                    warn(warnings, WarningKind::UnmatchedDirective("#ENDRANDOM"));
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Ends the file: an `#IF` still open is a warning; an open `#RANDOM`
    /// is normal.
    pub fn finish(self, warnings: &mut Vec<Warning>) -> Vec<u32> {
        if !self.flags.is_empty() {
            warnings.push(Warning {
                line: None,
                kind: WarningKind::UnclosedIf,
            });
        }
        self.results
    }
}

/// A decimal integer in `1..=u32::MAX`, or an error naming `what`.
fn positive_integer(text: &str, line: usize, what: &'static str) -> Result<u32, LoadError> {
    text.parse::<u32>()
        .ok()
        .filter(|&n| n > 0)
        .ok_or(LoadError::InvalidValue {
            line,
            what,
            reason: "not a positive integer",
        })
}

#[cfg(test)]
mod tests {
    use super::SplitMix64;

    #[test]
    fn splitmix64_matches_the_published_stream() {
        // The first outputs for seed 1234567, from the reference
        // implementation by Sebastiano Vigna.
        let mut rng = SplitMix64::new(1_234_567);
        let expected = [
            6_457_827_717_110_365_317,
            3_203_168_211_198_807_973,
            9_817_491_932_198_370_423,
            4_593_380_528_125_082_431,
            16_408_922_859_458_223_821,
        ];
        for e in expected {
            assert_eq!(rng.next(), e);
        }
    }
}
