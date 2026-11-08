//! The play adjustments' settings and their exact arithmetic (design.md
//! "Play Adjustments"): the speed mode, the reference tempo, the Classic
//! multiplier and the steps, the target green number and the three lane
//! covers, validated together as [`PlayOptions`]; and the multiplier `H`,
//! the travel duration and the green number as exact ratios, with every
//! multiplier, set or solved, clamped to 0.01–20.
//!
//! Units: a cover is in ‰, SUDDEN+ of the lane left above the lift and LIFT
//! and HIDDEN+ of the original lane; one green unit is 1/600 s. With `B`
//! the playback tempo, `v = (1 − l)(1 − c)` from the effective LIFT `l` and
//! SUDDEN+ `c`, the duration is `T = 240000·v / (B·H)` ms, the green number
//! `g = 144000·v / (B·H)`, and Floating solves
//! `H = clamp(144000·v / (B·G), 1/100, 20)`. Only the final pixel geometry
//! is floating point, so nothing here is.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::cmp::Ordering;

use replay::{CoverRecord, HiSpeedRatio, StartTuning, TuningCover, TuningMode, TuningReference};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// An exact non-negative ratio in lowest terms, its denominator positive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ratio {
    num: u64,
    den: u64,
}

impl Ratio {
    /// The smallest multiplier, 1/100.
    pub const MIN_HISPEED: Self = Self { num: 1, den: 100 };
    /// The largest multiplier, 20.
    pub const MAX_HISPEED: Self = Self { num: 20, den: 1 };

    /// `num / den` in lowest terms; `None` when `den` is 0.
    #[must_use]
    pub fn new(num: u64, den: u64) -> Option<Self> {
        Self::reduced(u128::from(num), u128::from(den)).ok()
    }

    /// The whole number `n`.
    #[must_use]
    pub const fn integer(n: u64) -> Self {
        Self { num: n, den: 1 }
    }

    /// The numerator, in lowest terms.
    #[must_use]
    pub const fn num(self) -> u64 {
        self.num
    }

    /// The denominator, in lowest terms; never 0.
    #[must_use]
    pub const fn den(self) -> u64 {
        self.den
    }

    /// The nearest whole number, a half rounded up.
    #[must_use]
    pub fn round_half_up(self) -> u64 {
        let n = (2 * u128::from(self.num) + u128::from(self.den)) / (2 * u128::from(self.den));
        u64::try_from(n).unwrap_or(u64::MAX)
    }

    /// The whole hundredths of `self`, if it is a whole number of them that
    /// fits a `u32`.
    #[must_use]
    pub fn hundredths(self) -> Option<u32> {
        let n = u128::from(self.num) * 100;
        let d = u128::from(self.den);
        if n % d != 0 {
            return None;
        }
        u32::try_from(n / d).ok()
    }

    /// `n / 100`.
    #[must_use]
    pub fn from_hundredths(n: u32) -> Self {
        // A `u32` over 100 always fits.
        Self::reduced(u128::from(n), 100).unwrap_or(Self::MIN_HISPEED)
    }

    /// `num / den` reduced, if `den` is positive and the reduced terms fit.
    fn reduced(num: u128, den: u128) -> Result<Self, Unrepresentable> {
        if den == 0 {
            return Err(Unrepresentable);
        }
        let g = gcd(num, den);
        let (num, den) = (num / g, den / g);
        Ok(Self {
            num: u64::try_from(num).map_err(|_| Unrepresentable)?,
            den: u64::try_from(den).map_err(|_| Unrepresentable)?,
        })
    }

    /// `self × other`, exactly.
    ///
    /// # Errors
    ///
    /// [`Unrepresentable`] when the product's lowest terms do not fit.
    pub fn times(self, other: Self) -> Result<Self, Unrepresentable> {
        Self::reduced(
            u128::from(self.num) * u128::from(other.num),
            u128::from(self.den) * u128::from(other.den),
        )
    }

    /// `self` held within 1/100 to 20.
    #[must_use]
    pub fn clamp_hispeed(self) -> Self {
        self.clamp(Self::MIN_HISPEED, Self::MAX_HISPEED)
    }
}

impl Ord for Ratio {
    fn cmp(&self, other: &Self) -> Ordering {
        // Each product of two `u64`s fits a `u128`.
        (u128::from(self.num) * u128::from(other.den))
            .cmp(&(u128::from(other.num) * u128::from(self.den)))
    }
}

impl PartialOrd for Ratio {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

const fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    if a == 0 { 1 } else { a }
}

/// A result whose exact terms do not fit: the transaction that needed it
/// changes nothing (design.md "Play Adjustments", bounds).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("the exact value does not fit")]
pub struct Unrepresentable;

/// How the multiplier follows the player's settings; `"classic"` or
/// `"floating"` in `player.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeedMode {
    /// A saved multiplier, edited directly.
    Classic,
    /// The multiplier solved from the target green number.
    #[default]
    Floating,
}

/// The chart tempo a preparation solves from; `"start"`, `"max"`, `"main"`
/// or `"min"` in `player.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HiSpeedReference {
    /// The chart's initial tempo.
    #[default]
    Start,
    /// The highest tempo.
    #[serde(rename = "max")]
    Maximum,
    /// The tempo weighted by its notes.
    Main,
    /// The lowest tempo.
    #[serde(rename = "min")]
    Minimum,
}

/// The prepared reference tempos of a chart, in its own BPM (the practice
/// speed not applied).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct References {
    /// The initial tempo.
    pub start: Ratio,
    /// The highest tempo.
    pub maximum: Ratio,
    /// The tempo weighted by its notes.
    pub main: Ratio,
    /// The lowest tempo.
    pub minimum: Ratio,
}

impl References {
    /// The same tempo for every reference.
    #[must_use]
    pub const fn all(bpm: Ratio) -> Self {
        Self {
            start: bpm,
            maximum: bpm,
            main: bpm,
            minimum: bpm,
        }
    }

    /// The tempo `reference` names.
    #[must_use]
    pub const fn get(&self, reference: HiSpeedReference) -> Ratio {
        match reference {
            HiSpeedReference::Start => self.start,
            HiSpeedReference::Maximum => self.maximum,
            HiSpeedReference::Main => self.main,
            HiSpeedReference::Minimum => self.minimum,
        }
    }
}

/// The speed settings, as integers: multipliers and steps in hundredths;
/// `[hispeed]` in `player.toml`, under these names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HiSpeedSettings {
    /// Classic or Floating.
    pub mode: SpeedMode,
    /// What a Floating preparation solves from.
    pub reference: HiSpeedReference,
    /// Classic's saved multiplier, 1 to 2000 (0.01 to 20).
    pub classic_hispeed_x100: u32,
    /// Classic's step, 1 to 100 (0.01 to 1.00).
    pub classic_step_x100: u32,
    /// Floating's step, 5 to 100 in fives (0.05 to 1.00).
    pub floating_step_x100: u32,
    /// The target green number G, 1 to 6000.
    pub green_target: u32,
}

impl Default for HiSpeedSettings {
    fn default() -> Self {
        Self {
            mode: SpeedMode::Floating,
            reference: HiSpeedReference::Start,
            classic_hispeed_x100: 100,
            classic_step_x100: 25,
            floating_step_x100: 50,
            green_target: 300,
        }
    }
}

/// The smallest and largest target green number.
pub const GREEN_TARGETS: (u32, u32) = (1, 6000);
/// The largest cover amount, ‰.
pub const MAX_COVER: u16 = 1000;

/// One lane cover: its configured amount, kept while it is off, and its
/// switch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cover {
    /// ‰, 0 to 1000.
    pub amount: u16,
    /// Whether it is on.
    pub on: bool,
}

impl Cover {
    /// The amount that counts: the configured one when on, else 0.
    #[must_use]
    pub const fn effective(self) -> u16 {
        if self.on { self.amount } else { 0 }
    }
}

/// Which lane cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverKind {
    /// SUDDEN+, from the top, of the lane left above the lift.
    Sudden,
    /// LIFT, raising the judgment line, of the original lane.
    Lift,
    /// HIDDEN+, above the judgment line.
    Hidden,
}

/// The three lane covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Covers {
    /// SUDDEN+.
    pub sudden: Cover,
    /// LIFT.
    pub lift: Cover,
    /// HIDDEN+.
    pub hidden: Cover,
}

impl Covers {
    /// The cover `kind` names.
    #[must_use]
    pub const fn get(&self, kind: CoverKind) -> Cover {
        match kind {
            CoverKind::Sudden => self.sudden,
            CoverKind::Lift => self.lift,
            CoverKind::Hidden => self.hidden,
        }
    }

    /// The cover `kind` names, to change.
    pub const fn get_mut(&mut self, kind: CoverKind) -> &mut Cover {
        match kind {
            CoverKind::Sudden => &mut self.sudden,
            CoverKind::Lift => &mut self.lift,
            CoverKind::Hidden => &mut self.hidden,
        }
    }

    /// `(1000 − l)` and `(1000 − c)`: the field left by the effective LIFT
    /// and SUDDEN+, each in ‰, so `v` is their product over 10⁶.
    fn field(&self) -> (u128, u128) {
        let left = |a: u16| u128::from(MAX_COVER.saturating_sub(a));
        (left(self.lift.effective()), left(self.sudden.effective()))
    }
}

/// A play option outside its range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum PlayOptionsError {
    /// The value lies outside the setting's range, or off its grid.
    #[error("play option `{setting}` is {value}, outside {expected}")]
    Invalid {
        /// The setting, as `player.toml` will name it.
        setting: &'static str,
        /// The value refused.
        value: u32,
        /// What the setting takes.
        expected: &'static str,
    },
}

/// The play adjustments' settings, validated: the one store that a
/// preparation reads and the reducer updates ([`crate::adjustment`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlayOptions {
    hispeed: HiSpeedSettings,
    covers: Covers,
}

impl PlayOptions {
    /// The options, once each value is within its range.
    ///
    /// # Errors
    ///
    /// The first setting outside its range, by name.
    pub fn new(hispeed: HiSpeedSettings, covers: Covers) -> Result<Self, PlayOptionsError> {
        check_hispeed(&hispeed)?;
        check_covers(&covers)?;
        Ok(Self { hispeed, covers })
    }

    /// The speed settings.
    #[must_use]
    pub const fn hispeed(&self) -> HiSpeedSettings {
        self.hispeed
    }

    /// The covers.
    #[must_use]
    pub const fn covers(&self) -> Covers {
        self.covers
    }

    /// Replaces the speed settings, once they are valid.
    ///
    /// # Errors
    ///
    /// The first setting outside its range; nothing changes then.
    pub fn set_hispeed(&mut self, hispeed: HiSpeedSettings) -> Result<(), PlayOptionsError> {
        check_hispeed(&hispeed)?;
        self.hispeed = hispeed;
        Ok(())
    }

    /// Replaces the covers, once they are valid.
    ///
    /// # Errors
    ///
    /// The first cover outside its range; nothing changes then.
    pub fn set_covers(&mut self, covers: Covers) -> Result<(), PlayOptionsError> {
        check_covers(&covers)?;
        self.covers = covers;
        Ok(())
    }

    /// The reducer's write path, its values already held in range.
    pub(crate) const fn parts_mut(&mut self) -> (&mut HiSpeedSettings, &mut Covers) {
        (&mut self.hispeed, &mut self.covers)
    }

    /// The record's starting tuning (design.md "Input Log Format"): these
    /// options with the multiplier `hispeed` in effect and the cover
    /// `target` the controls adjust, each `None` when there is none.
    #[must_use]
    pub fn start_tuning(&self, hispeed: Option<Ratio>, target: Option<CoverKind>) -> StartTuning {
        let h = self.hispeed;
        let cover = |c: Cover| CoverRecord {
            amount: c.amount,
            on: c.on,
        };
        StartTuning {
            mode: match h.mode {
                SpeedMode::Classic => TuningMode::Classic,
                SpeedMode::Floating => TuningMode::Floating,
            },
            reference: match h.reference {
                HiSpeedReference::Start => TuningReference::Start,
                HiSpeedReference::Maximum => TuningReference::Maximum,
                HiSpeedReference::Main => TuningReference::Main,
                HiSpeedReference::Minimum => TuningReference::Minimum,
            },
            classic_hispeed_x100: h.classic_hispeed_x100,
            classic_step_x100: h.classic_step_x100,
            floating_step_x100: h.floating_step_x100,
            green_target: h.green_target,
            sudden: cover(self.covers.sudden),
            lift: cover(self.covers.lift),
            hidden: cover(self.covers.hidden),
            target: target.map(|k| match k {
                CoverKind::Sudden => TuningCover::Sudden,
                CoverKind::Lift => TuningCover::Lift,
                CoverKind::Hidden => TuningCover::Hidden,
            }),
            hispeed: hispeed.map(|r| HiSpeedRatio {
                num: r.num(),
                den: r.den(),
            }),
        }
    }
}

fn check(
    setting: &'static str,
    value: u32,
    ok: bool,
    expected: &'static str,
) -> Result<(), PlayOptionsError> {
    if ok {
        Ok(())
    } else {
        Err(PlayOptionsError::Invalid {
            setting,
            value,
            expected,
        })
    }
}

fn check_hispeed(h: &HiSpeedSettings) -> Result<(), PlayOptionsError> {
    check(
        "classic_hispeed_x100",
        h.classic_hispeed_x100,
        (1..=2000).contains(&h.classic_hispeed_x100),
        "1 to 2000",
    )?;
    check(
        "classic_step_x100",
        h.classic_step_x100,
        (1..=100).contains(&h.classic_step_x100),
        "1 to 100",
    )?;
    check(
        "floating_step_x100",
        h.floating_step_x100,
        (5..=100).contains(&h.floating_step_x100) && h.floating_step_x100.is_multiple_of(5),
        "5 to 100 in steps of 5",
    )?;
    check(
        "green_target",
        h.green_target,
        (GREEN_TARGETS.0..=GREEN_TARGETS.1).contains(&h.green_target),
        "1 to 6000",
    )
}

fn check_covers(c: &Covers) -> Result<(), PlayOptionsError> {
    for (setting, cover) in [("sudden", c.sudden), ("lift", c.lift), ("hidden", c.hidden)] {
        check(
            setting,
            u32::from(cover.amount),
            cover.amount <= MAX_COVER,
            "0 to 1000",
        )?;
    }
    Ok(())
}

/// Floating's solve: `H = clamp(144000·v / (B·G), 1/100, 20)` for the
/// playback tempo `bpm` (positive), the target `green` (positive) and the
/// effective covers.
///
/// # Errors
///
/// [`Unrepresentable`] when `bpm` or `green` is 0, or the unclamped result's
/// lowest terms do not fit.
pub fn solve(bpm: Ratio, green: u32, covers: &Covers) -> Result<Ratio, Unrepresentable> {
    let (lift, sudden) = covers.field();
    // 144000 / 10⁶ = 18/125.
    let num = 18 * lift * sudden * u128::from(bpm.den);
    let den = 125 * u128::from(bpm.num) * u128::from(green);
    if den == 0 {
        return Err(Unrepresentable);
    }
    clamped_hispeed(num, den)
}

/// `num / den` held within 0.01–20, compared before reducing. `den`
/// (positive) is below 2⁹⁸ for every caller, so `20·den` fits, and `num` is
/// multiplied only once it is below `20·den`.
fn clamped_hispeed(num: u128, den: u128) -> Result<Ratio, Unrepresentable> {
    if num >= 20 * den {
        return Ok(Ratio::MAX_HISPEED);
    }
    if num * 100 <= den {
        return Ok(Ratio::MIN_HISPEED);
    }
    Ratio::reduced(num, den)
}

/// `h` moved by `hundredths` / 100, the result clamped to 0.01–20, never
/// the request: a step inward always works.
///
/// # Errors
///
/// [`Unrepresentable`] when the result's lowest terms do not fit.
pub fn step_hispeed(h: Ratio, hundredths: i64) -> Result<Ratio, Unrepresentable> {
    // In hundredths over `den`: `h` is `100·num`, the step `by`, and the
    // ends `2000·den` and `den`. Each fits a `u128` (`by` < 2¹²⁷), and the
    // step is compared with its room to the end before it is added.
    let (num, den) = (u128::from(h.num), u128::from(h.den));
    let by = u128::from(hundredths.unsigned_abs()) * den;
    let num = if hundredths >= 0 {
        if by >= (2000 * den).saturating_sub(100 * num) {
            return Ok(Ratio::MAX_HISPEED);
        }
        100 * num + by
    } else {
        if by >= (100 * num).saturating_sub(den) {
            return Ok(Ratio::MIN_HISPEED);
        }
        100 * num - by
    };
    clamped_hispeed(num, 100 * den)
}

/// The nominal travel duration `T = 240000·v / (B·H)` ms: 0 when no field
/// is left (`v = 0`), evaluated before any division.
///
/// # Errors
///
/// [`Unrepresentable`] when `bpm` or `h` is 0, or the lowest terms do not
/// fit.
pub fn duration_ms(bpm: Ratio, h: Ratio, covers: &Covers) -> Result<Ratio, Unrepresentable> {
    if bpm.num == 0 || h.num == 0 {
        return Err(Unrepresentable);
    }
    let (lift, sudden) = covers.field();
    if lift == 0 || sudden == 0 {
        return Ok(Ratio::integer(0));
    }
    // 240000 / 10⁶ = 6/25.
    cancelled(
        [6 * lift * sudden, u128::from(bpm.den), u128::from(h.den)],
        [25, u128::from(bpm.num), u128::from(h.num)],
    )
}

/// The product of `nums` over the product of `dens`, each pair of factors
/// cancelled first, so a result whose lowest terms fit never overflows on
/// the way.
pub(crate) fn cancelled(
    mut nums: [u128; 3],
    mut dens: [u128; 3],
) -> Result<Ratio, Unrepresentable> {
    for n in &mut nums {
        for d in &mut dens {
            let g = gcd(*n, *d);
            *n /= g;
            *d /= g;
        }
    }
    let product = |f: [u128; 3]| {
        f.into_iter()
            .try_fold(1_u128, u128::checked_mul)
            .ok_or(Unrepresentable)
    };
    Ratio::reduced(product(nums)?, product(dens)?)
}

/// The green number `g = 144000·v / (B·H)`, three fifths of
/// [`duration_ms`].
///
/// # Errors
///
/// What [`duration_ms`] reports.
pub fn green(bpm: Ratio, h: Ratio, covers: &Covers) -> Result<Ratio, Unrepresentable> {
    duration_ms(bpm, h, covers)?.times(Ratio { num: 3, den: 5 })
}
