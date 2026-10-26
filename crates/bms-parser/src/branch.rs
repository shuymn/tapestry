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

/// One `#IF` of a block: `active` while its current branch is taken,
/// `taken` once any branch of it was.
#[derive(Clone, Copy, Debug)]
struct Condition {
    active: bool,
    taken: bool,
}

#[derive(Clone, Debug)]
enum Frame {
    /// A `#RANDOM` or `#SETRANDOM` block; `value` is `None` exactly when the
    /// block sits in a branch not taken, so nothing was drawn.
    Random { value: Option<u32> },
    /// The `#IF`s open directly inside one block; `parent` is whether that
    /// block is taken. An `#IF` met while another is open stacks on it and
    /// only the last one counts; `#ENDIF` pops one, so the one before
    /// counts again. Never empty. The stack is one nesting level, bounded
    /// by the line limit.
    If {
        parent: bool,
        conditions: Vec<Condition>,
    },
}

impl Frame {
    fn active(&self) -> bool {
        match self {
            Self::Random { value } => value.is_some(),
            Self::If { conditions, .. } => conditions.last().is_some_and(|c| c.active),
        }
    }
}

/// The open blocks and the values drawn so far.
#[derive(Debug)]
pub struct Branches {
    rng: SplitMix64,
    frames: Vec<Frame>,
    max_depth: usize,
    /// An `#IF` stacked on another was warned about; once per chart.
    warned_stacked: bool,
    /// Every `#RANDOM` and `#SETRANDOM` value in a taken branch, in order.
    pub results: Vec<u32>,
}

impl Branches {
    pub const fn new(seed: u64, max_depth: usize) -> Self {
        Self {
            rng: SplitMix64::new(seed),
            frames: Vec::new(),
            max_depth,
            warned_stacked: false,
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
                    let n = positive_integer(value, line, what)?;
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
                self.push(Frame::Random { value }, line)?;
            }
            "IF" => {
                // An #IF met while an #IF of the same #RANDOM is open stacks
                // on it, as #ENDIF is often left out between consecutive
                // branches; its block's activity is the open one's.
                let stacked = match self.frames.last() {
                    Some(Frame::If { parent, .. }) => Some(*parent),
                    _ => None,
                };
                let active = stacked.unwrap_or(active);
                let random = self.frames.iter().rev().find_map(|f| match f {
                    Frame::Random { value, .. } => Some(*value),
                    Frame::If { .. } => None,
                });
                let taken = if active {
                    let k = positive_integer(value, line, "#IF")?;
                    if random.is_none() {
                        warn(warnings, WarningKind::UnmatchedDirective("#IF"));
                    }
                    random.flatten() == Some(k)
                } else {
                    false
                };
                let condition = Condition {
                    active: taken,
                    taken,
                };
                if let Some(Frame::If { conditions, .. }) = self.frames.last_mut() {
                    conditions.push(condition);
                    if !std::mem::replace(&mut self.warned_stacked, true) {
                        warn(warnings, WarningKind::UnclosedIf);
                    }
                } else {
                    self.push(
                        Frame::If {
                            parent: active,
                            conditions: vec![condition],
                        },
                        line,
                    )?;
                }
            }
            "ELSE" => {
                // The innermost open #IF; a #RANDOM left open inside its
                // branch ends here.
                if let Some(i) = self
                    .frames
                    .iter()
                    .rposition(|f| matches!(f, Frame::If { .. }))
                {
                    self.frames.truncate(i + 1);
                    if let Some(Frame::If { parent, conditions }) = self.frames.last_mut()
                        && let Some(c) = conditions.last_mut()
                    {
                        c.active = *parent && !c.taken;
                        c.taken = true;
                    }
                } else {
                    warn(warnings, WarningKind::UnmatchedDirective("#ELSE"));
                }
            }
            "ENDIF" => {
                if let Some(i) = self
                    .frames
                    .iter()
                    .rposition(|f| matches!(f, Frame::If { .. }))
                {
                    // Closes any #RANDOM left open inside the #IF too, and
                    // the #IF stacked below it counts again.
                    self.frames.truncate(i + 1);
                    if let Some(Frame::If { conditions, .. }) = self.frames.last_mut() {
                        conditions.pop();
                        if conditions.is_empty() {
                            self.frames.pop();
                        }
                    }
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
                    // Every frame above the last #RANDOM is an #IF.
                    if i + 1 < self.frames.len() {
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
