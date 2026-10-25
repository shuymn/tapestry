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

#[derive(Clone, Copy, Debug)]
enum Frame {
    /// A `#RANDOM` or `#SETRANDOM` block; `value` is `None` when the block
    /// sits in a branch not taken, so nothing was drawn.
    Random { value: Option<u32>, active: bool },
    /// An `#IF` block: `active` while the current branch is taken, `taken`
    /// once any branch of it was, `parent` whether its block is taken.
    If {
        active: bool,
        taken: bool,
        parent: bool,
    },
}

impl Frame {
    const fn active(self) -> bool {
        match self {
            Self::Random { active, .. } | Self::If { active, .. } => active,
        }
    }
}

/// The open blocks and the values drawn so far.
#[derive(Debug)]
pub struct Branches {
    rng: SplitMix64,
    frames: Vec<Frame>,
    max_depth: usize,
    /// Every `#RANDOM` and `#SETRANDOM` value in a taken branch, in order.
    pub results: Vec<u32>,
}

impl Branches {
    pub const fn new(seed: u64, max_depth: usize) -> Self {
        Self {
            rng: SplitMix64::new(seed),
            frames: Vec::new(),
            max_depth,
            results: Vec::new(),
        }
    }

    /// Whether lines at this point are read.
    pub fn active(&self) -> bool {
        self.frames.last().is_none_or(|f| f.active())
    }

    /// Handles a branch directive. Returns `Ok(false)` when `name` is not one.
    pub fn directive(
        &mut self,
        name: &str,
        value: &str,
        line: usize,
        warnings: &mut Vec<Warning>,
    ) -> Result<bool, LoadError> {
        let active = self.active();
        let warn = |warnings: &mut Vec<Warning>, kind| {
            warnings.push(Warning {
                line: Some(line),
                kind,
            });
        };
        match name {
            "RANDOM" | "SETRANDOM" => {
                // A #RANDOM directly inside another ends it: #ENDRANDOM is
                // often left out between consecutive blocks.
                if matches!(self.frames.last(), Some(Frame::Random { .. })) {
                    self.frames.pop();
                }
                let parent = self.active();
                let value = if parent {
                    let what = if name == "RANDOM" {
                        "#RANDOM"
                    } else {
                        "#SETRANDOM"
                    };
                    let n = positive_integer(value).ok_or(LoadError::InvalidValue {
                        line,
                        what,
                        reason: "not a positive integer",
                    })?;
                    let v = if name == "RANDOM" {
                        self.rng.draw(n)
                    } else {
                        n
                    };
                    self.results.push(v);
                    Some(v)
                } else {
                    None
                };
                self.push(
                    Frame::Random {
                        value,
                        active: parent,
                    },
                    line,
                )?;
            }
            "IF" => {
                let random = self.frames.iter().rev().find_map(|f| match f {
                    Frame::Random { value, .. } => Some(*value),
                    Frame::If { .. } => None,
                });
                let taken = if active {
                    let k = positive_integer(value).ok_or(LoadError::InvalidValue {
                        line,
                        what: "#IF",
                        reason: "not a positive integer",
                    })?;
                    if random.is_none() {
                        warn(warnings, WarningKind::UnmatchedDirective("#IF"));
                    }
                    random.flatten() == Some(k)
                } else {
                    false
                };
                self.push(
                    Frame::If {
                        active: taken,
                        taken,
                        parent: active,
                    },
                    line,
                )?;
            }
            "ELSE" => match self.frames.last_mut() {
                Some(Frame::If {
                    active,
                    taken,
                    parent,
                }) => {
                    *active = *parent && !*taken;
                    *taken = true;
                }
                _ => warn(warnings, WarningKind::UnmatchedDirective("#ELSE")),
            },
            "ENDIF" => {
                if let Some(i) = self
                    .frames
                    .iter()
                    .rposition(|f| matches!(f, Frame::If { .. }))
                {
                    // Closes any #RANDOM left open inside the #IF too.
                    self.frames.truncate(i);
                } else {
                    warn(warnings, WarningKind::UnmatchedDirective("#ENDIF"));
                }
            }
            "ENDRANDOM" => {
                if let Some(i) = self
                    .frames
                    .iter()
                    .rposition(|f| matches!(f, Frame::Random { .. }))
                {
                    if self.frames[i..]
                        .iter()
                        .any(|f| matches!(f, Frame::If { .. }))
                    {
                        warn(warnings, WarningKind::UnclosedIf);
                    }
                    self.frames.truncate(i);
                } else {
                    warn(warnings, WarningKind::UnmatchedDirective("#ENDRANDOM"));
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn push(&mut self, frame: Frame, line: usize) -> Result<(), LoadError> {
        if self.frames.len() >= self.max_depth {
            return Err(LoadError::NestingTooDeep { line });
        }
        self.frames.push(frame);
        Ok(())
    }

    /// Ends the file: an `#IF` still open is a warning; an open `#RANDOM`
    /// is normal.
    pub fn finish(self, warnings: &mut Vec<Warning>) -> Vec<u32> {
        if self.frames.iter().any(|f| matches!(f, Frame::If { .. })) {
            warnings.push(Warning {
                line: None,
                kind: WarningKind::UnclosedIf,
            });
        }
        self.results
    }
}

/// A decimal integer in `1..=u32::MAX`.
fn positive_integer(text: &str) -> Option<u32> {
    text.parse::<u32>().ok().filter(|&n| n > 0)
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
