//! The header policy: how a preset reads a chart's `#RANK`, `#DEFEXRANK`
//! and `#TOTAL` declarations into one rank and one `#TOTAL` during
//! preparation (design.md "Evaluation Stages").

use bms_model::{HeaderValue, JudgmentHeader, JudgmentHeaderName};

/// The judgment rank a header policy reads from a chart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JudgeRank {
    /// `#RANK`, as written.
    Rank(i32),
    /// `#DEFEXRANK`, in thousandths of a percent.
    DefExRankMilli(i64),
}

/// What a header policy reads from a chart's declarations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadHeaders {
    /// The rank that counts, if any.
    pub rank: Option<JudgeRank>,
    /// The `#TOTAL` that counts, in thousandths, if any; the preset's
    /// default applies otherwise.
    pub total_milli: Option<i64>,
}

/// How the judgment-header declarations are read. Every variant walks the
/// declarations in file order, and a declaration it accepts replaces the
/// earlier one of its slot, so a declaration it refuses keeps the earlier
/// one. Each variant says whether `#RANK` and `#DEFEXRANK` share a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderPolicy {
    /// The tapestry reading, which loader version 6 applied itself, with
    /// `#RANK` and `#DEFEXRANK` sharing one slot:
    /// - `#RANK`: an integer token that fits `i32`, whatever its value;
    /// - `#DEFEXRANK`: any number of at least 1, in thousandths rounded down;
    /// - `#TOTAL`: any number not negative, in thousandths rounded down; a
    ///   `#TOTAL` that counts but is 0 there leaves the preset's default.
    Decimals,
    /// The reference decoder's rules (jbms-parser `BMSDecoder.java`), read
    /// as numbers:
    /// - `#RANK`: an integer token from 0 to 4 (`:702-713`,
    ///   `rank >= 0 && rank < 5`);
    /// - `#DEFEXRANK`: any number above 0 (`:716-727`), an integer token
    ///   or a decimal (`120`, `120.0` and `120.5` alike), in thousandths
    ///   rounded down, so one below 0.001 reads as 0 and is refused. Its
    ///   `Integer.parseInt` is how the reference stores the value, not a
    ///   rule of the header (design.md "What a -style Preset Reproduces").
    ///   A `#DEFEXRANK` counts over any `#RANK`, before or after it: the
    ///   decoder that introduced it ignored `#RANK` once a `#DEFEXRANK` was
    ///   set (jbms-parser a7a0729, `if (model.getJudgerank() >= 10)
    ///   return;`), and beatoraja's chart documentation says it takes
    ///   precedence over `#RANK`; the check went away in jbms-parser
    ///   d0fba6d, when the `>= 10` encoding of a #DEFEXRANK stopped working
    ///   with its new minimum of 1;
    /// - `#TOTAL`: any number above 0 (`:730-743`, `total > 0`), in
    ///   thousandths rounded down.
    DefExRankFirst,
}

/// Thousandths of a positive multiple of `10^-18`, rounded down. Fits `i64`
/// for every value within the decimal bounds (`|v| < 10^30`).
fn milli(v: i128) -> Option<i64> {
    i64::try_from(v.div_euclid(HeaderValue::ONE / 1_000)).ok()
}

/// An integer token's value if it fits `i32`.
fn int32(value: HeaderValue) -> Option<i32> {
    match value {
        HeaderValue::Integer(v) => i32::try_from(v / HeaderValue::ONE).ok(),
        HeaderValue::Decimal(_) | HeaderValue::NotANumber => None,
    }
}

impl HeaderPolicy {
    /// Reads `headers` (a chart's declarations, in file order).
    #[must_use]
    pub fn read(&self, headers: &[JudgmentHeader]) -> ReadHeaders {
        let mut read = ReadHeaders::default();
        // `DefExRankFirst`'s #DEFEXRANK, kept apart from #RANK.
        let mut defexrank = None;
        for &JudgmentHeader { name, value } in headers {
            match (self, name) {
                (Self::Decimals, JudgmentHeaderName::Rank) => {
                    if let Some(r) = int32(value) {
                        read.rank = Some(JudgeRank::Rank(r));
                    }
                }
                (Self::Decimals, JudgmentHeaderName::DefExRank) => {
                    if let Some(m) = value.number().and_then(milli).filter(|&m| m >= 1_000) {
                        read.rank = Some(JudgeRank::DefExRankMilli(m));
                    }
                }
                (Self::Decimals, JudgmentHeaderName::Total) => {
                    if let Some(m) = value.number().filter(|&v| v >= 0).and_then(milli) {
                        read.total_milli = Some(m);
                    }
                }
                (Self::DefExRankFirst, JudgmentHeaderName::Rank) => {
                    if let Some(r) = int32(value).filter(|r| (0..5).contains(r)) {
                        read.rank = Some(JudgeRank::Rank(r));
                    }
                }
                (Self::DefExRankFirst, JudgmentHeaderName::DefExRank) => {
                    if let Some(m) = value.number().and_then(milli).filter(|&m| m > 0) {
                        defexrank = Some(JudgeRank::DefExRankMilli(m));
                    }
                }
                (Self::DefExRankFirst, JudgmentHeaderName::Total) => {
                    if let Some(m) = value.number().filter(|&v| v > 0).and_then(milli) {
                        read.total_milli = Some(m);
                    }
                }
            }
        }
        if *self == Self::Decimals {
            // A #TOTAL that is not positive counts as absent.
            read.total_milli = read.total_milli.filter(|&t| t > 0);
        }
        read.rank = defexrank.or(read.rank);
        read
    }
}

#[cfg(test)]
mod tests {
    use bms_model::HeaderValue::{self, Decimal, Integer, NotANumber};
    use bms_model::JudgmentHeader;
    use bms_model::JudgmentHeaderName::{self, DefExRank, Rank, Total};

    use super::HeaderPolicy::{Decimals, DefExRankFirst};
    use super::{JudgeRank, ReadHeaders};

    const ONE: i128 = HeaderValue::ONE;

    fn read(
        policy: super::HeaderPolicy,
        headers: &[(JudgmentHeaderName, HeaderValue)],
    ) -> ReadHeaders {
        let headers: Vec<_> = headers
            .iter()
            .map(|&(name, value)| JudgmentHeader { name, value })
            .collect();
        policy.read(&headers)
    }

    fn rank(
        policy: super::HeaderPolicy,
        headers: &[(JudgmentHeaderName, HeaderValue)],
    ) -> Option<JudgeRank> {
        read(policy, headers).rank
    }

    fn total(
        policy: super::HeaderPolicy,
        headers: &[(JudgmentHeaderName, HeaderValue)],
    ) -> Option<i64> {
        read(policy, headers).total_milli
    }

    /// The readings of loader version 6, each as its loader test stated it.
    #[test]
    fn decimals_read_as_loader_version_6_did() {
        let r3 = (Rank, HeaderValue::integer(3));
        for (headers, expected) in [
            (vec![r3], Some(JudgeRank::Rank(3))),
            // A #RANK must be a 32-bit integer token, of any value.
            (
                vec![(Rank, HeaderValue::integer(7))],
                Some(JudgeRank::Rank(7)),
            ),
            (
                vec![(Rank, HeaderValue::integer(-1))],
                Some(JudgeRank::Rank(-1)),
            ),
            (vec![r3, (Rank, Decimal(2 * ONE))], Some(JudgeRank::Rank(3))),
            (
                vec![r3, (Rank, Integer(i128::from(i32::MAX) * ONE + ONE))],
                Some(JudgeRank::Rank(3)),
            ),
            (vec![r3, (Rank, NotANumber)], Some(JudgeRank::Rank(3))),
            // A #DEFEXRANK below 1 is ignored, so an earlier #RANK stays.
            (vec![r3, (DefExRank, Integer(0))], Some(JudgeRank::Rank(3))),
            (
                vec![r3, (DefExRank, HeaderValue::decimal_milli(999))],
                Some(JudgeRank::Rank(3)),
            ),
            (
                vec![r3, (DefExRank, Decimal(ONE - 1))],
                Some(JudgeRank::Rank(3)),
            ),
            (
                vec![r3, (DefExRank, HeaderValue::integer(-2))],
                Some(JudgeRank::Rank(3)),
            ),
            (vec![r3, (DefExRank, NotANumber)], Some(JudgeRank::Rank(3))),
            (
                vec![(DefExRank, Integer(0)), (Rank, HeaderValue::integer(2))],
                Some(JudgeRank::Rank(2)),
            ),
            (vec![(DefExRank, Integer(0))], None),
            (
                vec![
                    (DefExRank, HeaderValue::integer(120)),
                    (DefExRank, Integer(0)),
                    (DefExRank, HeaderValue::decimal_milli(500)),
                ],
                Some(JudgeRank::DefExRankMilli(120_000)),
            ),
            // 1 and above replace it, in thousandths rounded down.
            (
                vec![r3, (DefExRank, HeaderValue::integer(1))],
                Some(JudgeRank::DefExRankMilli(1_000)),
            ),
            (
                vec![(DefExRank, Decimal(100 * ONE + ONE / 2_000_000))],
                Some(JudgeRank::DefExRankMilli(100_000)),
            ),
            // An integer token and a decimal read alike.
            (
                vec![(DefExRank, HeaderValue::decimal_milli(120_000))],
                Some(JudgeRank::DefExRankMilli(120_000)),
            ),
            (
                vec![(DefExRank, HeaderValue::decimal_milli(120_500))],
                Some(JudgeRank::DefExRankMilli(120_500)),
            ),
        ] {
            assert_eq!(rank(Decimals, &headers), expected, "{headers:?}");
        }
        for (headers, expected) in [
            (
                vec![(
                    Total,
                    Decimal(312 * ONE + 345_678_900 * ONE / 1_000_000_000),
                )],
                Some(312_345),
            ),
            (vec![(Total, HeaderValue::integer(250))], Some(250_000)),
            // A later readable one replaces an earlier one, 0 included, and
            // a #TOTAL that is not positive counts as absent.
            (
                vec![(Total, HeaderValue::integer(200)), (Total, Integer(0))],
                None,
            ),
            (
                vec![
                    (Total, HeaderValue::integer(200)),
                    (Total, Decimal(ONE / 10_000)),
                ],
                None,
            ),
            // A negative one or one that is not a number is ignored.
            (
                vec![
                    (Total, HeaderValue::integer(200)),
                    (Total, HeaderValue::integer(-5)),
                ],
                Some(200_000),
            ),
            (
                vec![(Total, HeaderValue::integer(200)), (Total, NotANumber)],
                Some(200_000),
            ),
            (vec![], None),
        ] {
            assert_eq!(total(Decimals, &headers), expected, "{headers:?}");
        }
    }

    /// Source: jbms-parser `BMSDecoder.java:702-743`; the precedence of
    /// `#DEFEXRANK`, jbms-parser a7a0729.
    #[test]
    fn defexrank_first_reads_the_reference_decoder_rules() {
        let r3 = (Rank, HeaderValue::integer(3));
        for (headers, expected) in [
            (vec![r3], Some(JudgeRank::Rank(3))),
            (
                vec![(Rank, HeaderValue::integer(4))],
                Some(JudgeRank::Rank(4)),
            ),
            (vec![(Rank, Integer(0))], Some(JudgeRank::Rank(0))),
            // Outside 0-4, or not an integer token: the earlier one stays.
            (
                vec![r3, (Rank, HeaderValue::integer(5))],
                Some(JudgeRank::Rank(3)),
            ),
            (
                vec![r3, (Rank, HeaderValue::integer(-1))],
                Some(JudgeRank::Rank(3)),
            ),
            (vec![r3, (Rank, Decimal(2 * ONE))], Some(JudgeRank::Rank(3))),
            (vec![(Rank, HeaderValue::integer(7))], None),
            // #DEFEXRANK: any number above 0, integer token or decimal, in
            // thousandths rounded down.
            (
                vec![r3, (DefExRank, HeaderValue::integer(120))],
                Some(JudgeRank::DefExRankMilli(120_000)),
            ),
            (
                vec![r3, (DefExRank, HeaderValue::decimal_milli(120_000))],
                Some(JudgeRank::DefExRankMilli(120_000)),
            ),
            (
                vec![r3, (DefExRank, HeaderValue::decimal_milli(120_500))],
                Some(JudgeRank::DefExRankMilli(120_500)),
            ),
            (
                vec![r3, (DefExRank, HeaderValue::decimal_milli(500))],
                Some(JudgeRank::DefExRankMilli(500)),
            ),
            (
                vec![(DefExRank, Integer(i128::from(i32::MAX) * ONE + ONE))],
                Some(JudgeRank::DefExRankMilli((i64::from(i32::MAX) + 1) * 1_000)),
            ),
            // Not above 0 in thousandths, or not a number: refused, so the
            // rank or an earlier #DEFEXRANK stays.
            (vec![r3, (DefExRank, Integer(0))], Some(JudgeRank::Rank(3))),
            (
                vec![r3, (DefExRank, HeaderValue::integer(-2))],
                Some(JudgeRank::Rank(3)),
            ),
            (
                vec![r3, (DefExRank, Decimal(ONE / 2_000))],
                Some(JudgeRank::Rank(3)),
            ),
            (vec![r3, (DefExRank, NotANumber)], Some(JudgeRank::Rank(3))),
            (
                vec![
                    (DefExRank, HeaderValue::integer(90)),
                    (DefExRank, Integer(0)),
                ],
                Some(JudgeRank::DefExRankMilli(90_000)),
            ),
            // A #DEFEXRANK counts over any #RANK, before or after it; the
            // later of two #DEFEXRANKs counts.
            (
                vec![(DefExRank, HeaderValue::integer(90)), r3],
                Some(JudgeRank::DefExRankMilli(90_000)),
            ),
            (
                vec![
                    (DefExRank, HeaderValue::integer(90)),
                    r3,
                    (DefExRank, HeaderValue::integer(110)),
                    (Rank, HeaderValue::integer(1)),
                ],
                Some(JudgeRank::DefExRankMilli(110_000)),
            ),
        ] {
            assert_eq!(rank(DefExRankFirst, &headers), expected, "{headers:?}");
        }
        let t200 = (Total, HeaderValue::integer(200));
        for (headers, expected) in [
            (vec![t200], Some(200_000)),
            (
                vec![(Total, HeaderValue::decimal_milli(312_345))],
                Some(312_345),
            ),
            // Not positive, or not a number: the earlier one stays.
            (vec![t200, (Total, Integer(0))], Some(200_000)),
            (vec![t200, (Total, HeaderValue::integer(-5))], Some(200_000)),
            (vec![t200, (Total, NotANumber)], Some(200_000)),
            (vec![(Total, Integer(0))], None),
            // Positive below a thousandth counts, as 0 thousandths.
            (vec![t200, (Total, Decimal(1))], Some(0)),
        ] {
            assert_eq!(total(DefExRankFirst, &headers), expected, "{headers:?}");
        }
    }
}
