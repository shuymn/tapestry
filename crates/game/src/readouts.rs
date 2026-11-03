//! The play readouts' exact projections (design.md "Play readouts"): from
//! one [`PresentationSnapshot`] and the tempos, the source projections an
//! imported skin reads (the hi-speed `Hs = H / (1 − l)`, the current
//! duration and the eight duration variants) and the native block of the
//! player's own HUD (the active `H`, the current duration and the target).
//!
//! With `H = hn/hd`, the effective LIFT `l` and SUDDEN+ `c` in ‰ and a
//! playback tempo `B = bn/bd`, a duration is the exact ratio
//! `6·(1000 − c)(1000 − l)·bd·hd / (25·bn·hn)` ms. A value whose rounding
//! passes [`READOUT_MAX`] is unavailable, never malformed; nothing here is
//! floating point and nothing allocates.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::Speed;

use crate::adjustment::{PresentationSnapshot, playback};

use crate::hispeed::{self, Covers, MAX_COVER, Ratio, References, SpeedMode, cancelled};

/// The largest readout value, `i32::MAX − 1`, equal to the skin model's.
pub const READOUT_MAX: i32 = i32::MAX - 1;

const MAX: u128 = READOUT_MAX.unsigned_abs() as u128;

/// One frame's projections; `None` is unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Projection {
    /// The active multiplier `H`, the lanes' gain.
    pub gain: Option<Ratio>,
    /// The playback tempo at the display head in floored hundredths of a
    /// BPM, at most [`READOUT_MAX`].
    pub bpm_current_x100: Option<u32>,
    /// The source hi-speed `⌊100·Hs⌋`, never clamped to 20; available
    /// while a field is left above the lift.
    pub hispeed_x100: Option<u32>,
    /// The source current duration with the effective covers, ms.
    pub duration: Option<Ratio>,
    /// The duration variants, index `2t + k`: `t` the current, MAIN,
    /// minimum and maximum tempo, `k` the configured lane cover (counted
    /// while it is off), then none; each with the effective lift.
    pub variants: [Option<Ratio>; 8],
    /// The native current duration `T`, ms: 0 where no field is left.
    pub native_duration: Option<Ratio>,
    /// The target green number, in Floating only.
    pub target_green: Option<u32>,
    /// Whether Floating is the mode.
    pub floating: bool,
}

/// The projections of `snapshot` at the chart's original tempo `current`
/// at the display head and its original `references`, each played at
/// `speed` (applied once here).
#[must_use]
pub fn project(
    snapshot: &PresentationSnapshot,
    current: Option<Ratio>,
    references: Option<References>,
    speed: Speed,
) -> Projection {
    let covers = snapshot.options.covers();
    let settings = snapshot.options.hispeed();
    let h = snapshot.hispeed;
    let lift = covers.lift.effective();
    let at = |bpm: Option<Ratio>| bpm.and_then(|b| playback(b, speed).ok());
    let current = at(current);
    let tempos = [
        current,
        at(references.map(|r| r.main)),
        at(references.map(|r| r.minimum)),
        at(references.map(|r| r.maximum)),
    ];
    let mut variants = [None; 8];
    for (pair, bpm) in variants.as_chunks_mut::<2>().0.iter_mut().zip(tempos) {
        for (slot, sudden) in pair.iter_mut().zip([covers.sudden.amount, 0]) {
            *slot = variant(sudden, lift, bpm, h);
        }
    }
    let floating = settings.mode == SpeedMode::Floating;
    Projection {
        gain: h,
        bpm_current_x100: current.and_then(bpm_x100),
        hispeed_x100: h.and_then(|h| source_hispeed_x100(h, lift)),
        duration: variant(covers.sudden.effective(), lift, current, h),
        variants,
        native_duration: native_duration(current, h, &covers),
        target_green: floating.then_some(settings.green_target),
        floating,
    }
}

/// The duration `6·(1000 − sudden)(1000 − lift)·bd·hd / (25·bn·hn)` ms at
/// `bpm` and `h`, SUDDEN+ and LIFT in ‰; `None` where no field is left
/// above the lift, the tempo or `H` is unknown or 0, the ratio does not
/// fit, or its ms or green number rounds past [`READOUT_MAX`].
#[must_use]
pub fn variant(sudden: u16, lift: u16, bpm: Option<Ratio>, h: Option<Ratio>) -> Option<Ratio> {
    let (bpm, h) = (bpm?, h?);
    if lift >= MAX_COVER {
        return None;
    }
    let left = |a: u16| u128::from(MAX_COVER.saturating_sub(a));
    let t = cancelled(
        [
            6 * left(sudden) * left(lift),
            u128::from(bpm.den()),
            u128::from(h.den()),
        ],
        [25, u128::from(bpm.num()), u128::from(h.num())],
    )
    .ok()?;
    readable(t).then_some(t)
}

/// `n / d` rounded half up, exactly; `None` when `d` is 0.
#[must_use]
pub const fn round_half_up(n: u128, d: u128) -> Option<u128> {
    if d == 0 {
        return None;
    }
    // Quotient and remainder, so `n` near `u128::MAX` does not overflow.
    let (q, r) = (n / d, n % d);
    Some(if r >= d - r { q + 1 } else { q })
}

/// Whether the ms and the green number of the duration `t` both round
/// within [`READOUT_MAX`].
fn readable(t: Ratio) -> bool {
    let (n, d) = (u128::from(t.num()), u128::from(t.den()));
    // n and d are u64s, so 3n and 5d fit.
    let ms = round_half_up(n, d);
    let green = round_half_up(3 * n, 5 * d);
    ms.is_some_and(|v| v <= MAX) && green.is_some_and(|v| v <= MAX)
}

/// The native duration `T` from [`hispeed::duration_ms`], if readable.
fn native_duration(bpm: Option<Ratio>, h: Option<Ratio>, covers: &Covers) -> Option<Ratio> {
    let t = hispeed::duration_ms(bpm?, h?, covers).ok()?;
    readable(t).then_some(t)
}

/// `⌊100000·hn / (hd·(1000 − lift))⌋`, where a field is left.
fn source_hispeed_x100(h: Ratio, lift: u16) -> Option<u32> {
    let left = u128::from(MAX_COVER.checked_sub(lift).filter(|&l| l > 0)?);
    let n = 100_000 * u128::from(h.num()) / (u128::from(h.den()) * left);
    u32::try_from(n).ok().filter(|&v| u128::from(v) <= MAX)
}

/// `⌊100·bpm⌋`, within [`READOUT_MAX`].
fn bpm_x100(bpm: Ratio) -> Option<u32> {
    let n = 100 * u128::from(bpm.num()) / u128::from(bpm.den());
    u32::try_from(n).ok().filter(|&v| u128::from(v) <= MAX)
}
