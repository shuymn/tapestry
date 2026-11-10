//! One frame's state: the drained effects through the reducer, the fields
//! from the snapshot, every built-in timer's start, then the `when` timers.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use bms_model::{BEAT, DOMAIN, Lane, TimeUs};
use eval::Tally;
use rules::GaugeValue;

use super::schema::{
    COVER_PERMILLE, DIFFICULTY_MAX, GAUGE_PERMILLE, Grade, JUDGE_CLASS_MAX, LaneTimer, MAX_COUNT,
    NATIVE_TARGET_MAX, PROGRESS_FULL, RANK_TWENTY_SEVENTHS, RATE_BASIS_POINTS, READOUT_MAX,
    READOUT_MAX_U32, field, lane_timer, timer,
};
use super::{
    BuildError, ChartMetadata, CoverSettings, DurationRatio, FrameReadouts, NativeValues, Play,
    RoundedDuration, SkinFrame,
};
use crate::chart_image::ChartImage;
use crate::expr::{Program, State, TIMER_OFF};
use crate::present::{Clock, Io, Present};
use crate::selection::Ratio;
use crate::widget::gauge::GaugeLook;
use crate::widget::lanes::LaneView;

/// A full gauge's value per ‰ of the gauge field.
const GAUGE_UNIT: i64 = GaugeValue::FULL.0 / GAUGE_PERMILLE as i64;

/// A readout already held within `i32` as a field value.
fn field_of(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(0)
}

// The fields a play fills: the 51 before the variants, three per
// variant, then the native block.
const _: () =
    assert!(field::DURATION_VARIANTS as usize + 3 * 8 == field::NATIVE_HISPEED_X100 as usize);

/// A count as a field value, clamped to the field's range.
fn count(v: u32) -> i32 {
    i32::try_from(v).map_or(MAX_COUNT, |v| v.min(MAX_COUNT))
}

/// The targets `tally` has judged: its five grades, less the empty POORs
/// counted among its POORs when `empty_poor_counted` (the evaluator then
/// adds each to both counts, `eval/src/evaluator.rs:1179-1183`). `None`
/// when it counts more empty POORs than POORs.
fn judged_targets(tally: &Tally, empty_poor_counted: bool) -> Option<u64> {
    let c = &tally.counts;
    let poor = if empty_poor_counted {
        c.poor.checked_sub(tally.empty_poor)?
    } else {
        c.poor
    };
    // Five u32 sum to at most 5 · (2^32 − 1), well inside u64.
    Some(
        u64::from(c.perfect)
            + u64::from(c.great)
            + u64::from(c.good)
            + u64::from(c.bad)
            + u64::from(poor),
    )
}

/// The source's 7-key point of `t` over `targets`
/// (`ScoreDataProperty.java:76-79` at `0.8.9`), with Java's intermediate
/// widths: 0 with no target, else
/// `(int)((150000L·PG + (int)(100000·GR) + (int)(20000·GD)) / N)
/// + (int)(50000L·M / N)`, the last `+` wrapping, `M` the maximum combo.
/// The `long` sum is below 2^50 and `50000 · M` below 2^47, so neither
/// overflows `i64`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "Java's `(int)` of a `long`: its low 32 bits"
)]
fn point(t: &Tally, targets: u32) -> i32 {
    // The source's `int` counts; a count past `i32` never comes from a
    // chart within the parser's caps.
    let int = |n: u32| i32::try_from(n).unwrap_or(i32::MAX);
    let n = i64::from(int(targets));
    if n <= 0 {
        return 0;
    }
    let c = &t.counts;
    // `(long)150000 * PG + 100000 * GR + 20000 * GD`: the GREAT and GOOD
    // products are `int`s, wrapping, then widened.
    let sum = 150_000 * i64::from(int(c.perfect))
        + i64::from(100_000_i32.wrapping_mul(int(c.great)))
        + i64::from(20_000_i32.wrapping_mul(int(c.good)));
    // Each `(int)` keeps the low 32 bits of a quotient truncated toward 0.
    let low = |v: i64| v as i32;
    low(sum / n).wrapping_add(low(50_000 * i64::from(int(t.max_combo)) / n))
}

/// The play state's score data (`ScoreDataProperty` at `0.8.9`) as a
/// frame reads it: the point, the EX score and the two score rates, the
/// fields `fill_fields` writes for one tally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settled {
    pub point: i32,
    pub ex_score: i32,
    pub rate_current_bp: i32,
    pub rate_whole_bp: i32,
}

/// The score data a frame over `t` reads, the attempt having `targets`
/// targets of which `t` judged `judged`: the field fill's own formulas.
fn scores(t: &Tally, targets: u32, judged: u64) -> Settled {
    Settled {
        point: point(t, targets),
        ex_score: count(t.ex_score),
        rate_current_bp: rate_bp(t.ex_score, judged),
        rate_whole_bp: rate_bp(t.ex_score, u64::from(targets)),
    }
}

/// The play state's score data after an attempt whose final tally is `t`
/// over `targets` (the source updates it at a judgment only,
/// `BMSPlayer.java:1038` at `0.8.9`, so the last judgment's stays): what
/// a frame over `t` reads. `None` for the zero tally, which no judgment
/// changed, and for a tally [`Skin::build`](crate::Skin::build) refuses
/// (more counted empty POORs than POORs), which no frame showed: the play
/// state's earlier data stays, the point included.
#[must_use]
pub fn settled(t: &Tally, targets: u32, empty_poor_counted: bool) -> Option<Settled> {
    if *t == Tally::default() {
        return None;
    }
    Some(scores(t, targets, judged_targets(t, empty_poor_counted)?))
}

/// The three score fields a play frame over `t` gives, `[ex_score,
/// rate.current_bp, rate.whole_bp]`, by `fill_fields`' own formulas, the
/// attempt having `targets` targets (dev-tools: the replay harness's model
/// of the source's score data, s7-06e-3-design.md §2a), the zero tally's
/// included. `None` for a tally [`Skin::build`](crate::Skin::build)
/// refuses (more counted empty POORs than POORs).
#[cfg(feature = "dev-tools")]
#[must_use]
pub fn score_fields(t: &Tally, targets: u32, empty_poor_counted: bool) -> Option<[i32; 3]> {
    let s = scores(t, targets, judged_targets(t, empty_poor_counted)?);
    Some([s.ex_score, s.rate_current_bp, s.rate_whole_bp])
}

/// The score rate of `ex` over `targets`, in basis points:
/// `⌊10 000 · ex / (2 · targets)⌋`, 10 000 with no target. Not capped at
/// 100 %: a custom EX weighting may pass it; only the field's `i32` range
/// bounds it. `10 000 · ex` is below 2^46 and `2 · targets` below 2^36, so
/// neither overflows `u64`.
fn rate_bp(ex: u32, targets: u64) -> i32 {
    let bp = u64::from(RATE_BASIS_POINTS.unsigned_abs());
    let Some(den) = targets.checked_mul(2).filter(|&d| d > 0) else {
        return RATE_BASIS_POINTS;
    };
    let rate = bp.saturating_mul(u64::from(ex)) / den;
    i32::try_from(rate).unwrap_or(i32::MAX)
}

/// 0 as a ratio.
const RATIO_ZERO: Ratio = Ratio {
    numerator: 0,
    denominator: 1,
};
/// 1 as a ratio.
const RATIO_ONE: Ratio = Ratio {
    numerator: 1,
    denominator: 1,
};

/// The number of score fields from `field::BEST_EX`.
pub(crate) const SCORE_FIELDS: usize = field::COUNT - field::BEST_EX as usize;

/// `x` paced linearly to `p` of `n` targets, `⌊x · p / n⌋`; 0 at `n = 0`.
/// The product is taken in `u128`, so nothing overflows.
fn paced(x: u32, p: u64, n: u64) -> u64 {
    let Some(q) = (u128::from(x) * u128::from(p)).checked_div(u128::from(n)) else {
        return 0;
    };
    u64::try_from(q).unwrap_or(u64::MAX)
}

/// `⌊scale · x / 2n⌋` as a field value, 0 at `n = 0`: an exact rate whose
/// empty denominator gives 0, never [`rate_bp`]'s 100 %. At most
/// `i32::MAX`.
fn exact_rate(x: u64, n: u64, scale: u32) -> i32 {
    let den = u128::from(n) * 2;
    let q = (u128::from(scale) * u128::from(x))
        .checked_div(den)
        .unwrap_or(0);
    i32::try_from(q).unwrap_or(i32::MAX)
}

/// `ex − paced` as a field value, clamped to ±[`MAX_COUNT`].
fn difference(ex: u32, paced: u64) -> i32 {
    let d = i128::from(ex) - i128::from(paced);
    let max = i128::from(MAX_COUNT);
    i32::try_from(d.clamp(-max, max)).unwrap_or(0)
}

/// `x / 2n` as an exact ratio; 0 at `n = 0`.
fn over_chart(x: u64, n: u64) -> Ratio {
    Ratio::new(x, n.saturating_mul(2)).unwrap_or(RATIO_ZERO)
}

/// Whether `27 · ex ≥ 2 · i · d`: a rate of `ex` over `2d` at least
/// `i/27`, exactly.
fn at_least(ex: u32, i: u32, d: u64) -> bool {
    27 * u128::from(ex) >= 2 * u128::from(i) * u128::from(d)
}

/// What one frame's score figures read: the EX score, the judged targets
/// `p`, the attempt's targets `n`, whether the attempt has been updated
/// (its first judgment, the empty POOR included), the play's best and
/// target, and whether its chart declares `#RANDOM`.
#[derive(Clone, Copy)]
pub(crate) struct ScoreInputs {
    pub(crate) ex: u32,
    pub(crate) judged: u64,
    pub(crate) targets: u64,
    pub(crate) updated: bool,
    pub(crate) best: u32,
    pub(crate) target: u32,
    pub(crate) random: bool,
}

/// The score fields from `field::BEST_EX`, in slot order.
pub(crate) fn score_figures(s: ScoreInputs) -> [i32; SCORE_FIELDS] {
    let ScoreInputs {
        ex,
        judged: p,
        targets: n,
        updated,
        best,
        target,
        random,
    } = s;
    let live = updated && n > 0;
    // The rate over the judged targets, 1 while none is judged, in each
    // band from its bound up to the one above (none above AAA).
    let current_at = |r: u32| p == 0 || at_least(ex, r, p);
    let above = std::iter::once(None).chain(RANK_TWENTY_SEVENTHS.map(Some));
    let current = RANK_TWENTY_SEVENTHS
        .into_iter()
        .zip(above)
        .map(|(lo, hi)| i32::from(live && current_at(lo) && !hi.is_some_and(current_at)));
    let whole = RANK_TWENTY_SEVENTHS
        .into_iter()
        .map(|lo| i32::from(live && at_least(ex, lo, n)));
    let bp = RATE_BASIS_POINTS.unsigned_abs();
    let head = [
        count(best),
        count(target),
        difference(ex, paced(best, p, n)),
        difference(ex, paced(target, p, n)),
        exact_rate(u64::from(best), n, bp),
        exact_rate(u64::from(target), n, bp),
        i32::from(!random),
        if random {
            0
        } else {
            exact_rate(u64::from(best), n, 1_000_000)
        },
        i32::from(updated && ex >= best),
        i32::from(updated && ex >= target),
    ];
    let mut out = [0; SCORE_FIELDS];
    for (slot, v) in out
        .iter_mut()
        .zip(head.into_iter().chain(current).chain(whole))
    {
        *slot = v;
    }
    out
}

/// The six score ratios the play's ratio bars draw, in
/// [`ScoreRate::ALL`](crate::ScoreRate::ALL)'s order: each exact, 0 for an empty denominator,
/// and none for the best's two on a `#RANDOM` chart.
pub(crate) fn score_ratios(s: ScoreInputs) -> [Option<Ratio>; 6] {
    let ScoreInputs {
        ex,
        judged: p,
        targets: n,
        updated,
        best,
        target,
        random,
    } = s;
    let judged = match (updated, p) {
        (false, _) => RATIO_ZERO,
        (true, 0) => RATIO_ONE,
        (true, p) => over_chart(u64::from(ex), p),
    };
    let compared = |r: Ratio| (!random).then_some(r);
    [
        Some(over_chart(u64::from(ex), n)),
        Some(judged),
        compared(over_chart(paced(best, p, n), n)),
        compared(over_chart(u64::from(best), n)),
        Some(over_chart(paced(target, p, n), n)),
        Some(over_chart(u64::from(target), n)),
    ]
}

/// The readout fields of one frame, `field::BPM_CURRENT_X100` to
/// `field::DURATION_VALID` and from `field::DURATION_VARIANTS` to
/// `field::CHART_LEVEL_VALID`, and `field::TIME_PROGRESS_PPM`, validated
/// before anything is applied.
#[derive(Clone, Copy)]
struct Readouts {
    /// Per variant in slot order: ms, green and validity.
    variants: [[i32; 3]; 8],
    native: NativeValues,
    bpm_current: i32,
    bpm_min: i32,
    bpm_max: i32,
    bpm_current_valid: i32,
    bpm_range_valid: i32,
    bpm_change: i32,
    bpm_change_valid: i32,
    level: i32,
    level_valid: i32,
    remaining_s: i32,
    elapsed_s: i32,
    progress_ppm: i32,
    covers: Covers,
    hispeed: i32,
    hispeed_valid: i32,
    duration_ms: i32,
    green: i32,
    duration_valid: i32,
}

/// The cover fields of one frame, `field::COVER_PERMILLE` to
/// `field::COVER_ADJUSTING`: the stored settings, a play's and a loading
/// frame's alike.
#[derive(Clone, Copy)]
pub(crate) struct Covers {
    pub(crate) cover: i32,
    pub(crate) lift: i32,
    pub(crate) hidden: i32,
    pub(crate) absolute: i32,
    pub(crate) cover_enabled: i32,
    pub(crate) lift_enabled: i32,
    pub(crate) hidden_enabled: i32,
    pub(crate) adjusting: i32,
}

/// The cover fields of the stored settings `c`, or `None` when a setting
/// is above 1 000 ‰ or disagrees with the effective amount `lanes` paints
/// (stage3.md R4, R6).
pub(crate) fn covers(lanes: &LaneView, c: &CoverSettings) -> Option<Covers> {
    let max = COVER_PERMILLE.unsigned_abs();
    if [c.cover, c.lift, c.hidden]
        .into_iter()
        .any(|v| u32::from(v) > max)
    {
        return None;
    }
    // The lanes cap their amounts at 1 000 (`LaneView`); a cover that is
    // off draws 0 whatever it stores.
    let effective = |on: bool, v: u16| if on { v } else { 0 };
    let full = u16::try_from(max).ok()?;
    if lanes.cover.min(full) != effective(c.cover_enabled, c.cover)
        || lanes.lift.min(full) != effective(c.lift_enabled, c.lift)
        || lanes.hidden.min(full) != effective(c.hidden_enabled, c.hidden)
    {
        return None;
    }
    let (cover, lift) = (i32::from(c.cover), i32::from(c.lift));
    Some(Covers {
        cover,
        lift,
        hidden: i32::from(c.hidden),
        // Both at most 1 000, so the product fits.
        absolute: (COVER_PERMILLE - lift) * cover / COVER_PERMILLE,
        cover_enabled: i32::from(c.cover_enabled),
        lift_enabled: i32::from(c.lift_enabled),
        hidden_enabled: i32::from(c.hidden_enabled),
        adjusting: i32::from(c.adjusting),
    })
}

/// Reads every timer at `now` from `starts` into `ms`: the built-in ones,
/// then each `when` timer, which starts on its condition's rising edge
/// over `fields` and the built-in readings and turns off on its falling
/// edge. A condition `admit` rejects reads as false. A play and the
/// loading runtime read their timers through this one rule.
pub(crate) fn read_timers(
    now: TimeUs,
    fields: &[i32],
    starts: &mut [Option<TimeUs>],
    ms: &mut [i32],
    whens: &[Program],
    admit: impl Fn(usize) -> bool,
) {
    read_screen_timers(
        usize::from(timer::BUILTIN),
        now,
        fields,
        starts,
        ms,
        whens,
        admit,
    );
}

/// [`read_timers`] over a screen's `builtin` built-in timers: the play
/// screen's, or another screen's schema's.
pub(crate) fn read_screen_timers(
    builtin: usize,
    now: TimeUs,
    fields: &[i32],
    starts: &mut [Option<TimeUs>],
    ms: &mut [i32],
    whens: &[Program],
    admit: impl Fn(usize) -> bool,
) {
    for (ms, start) in ms.iter_mut().zip(starts.iter()).take(builtin) {
        *ms = reading(now, *start);
    }
    let (Some((builtin_ms, when_ms)), Some(when_starts)) =
        (ms.split_at_mut_checked(builtin), starts.get_mut(builtin..))
    else {
        return;
    };
    let state = State {
        fields,
        timers: builtin_ms,
    };
    for (i, ((program, start), ms)) in whens
        .iter()
        .zip(when_starts.iter_mut())
        .zip(when_ms.iter_mut())
        .enumerate()
    {
        if !admit(i) || !program.eval_bool(&state) {
            *start = None;
        } else if start.is_none() {
            *start = Some(now);
        }
        *ms = reading(now, *start);
    }
}

/// An optional readout up to the maximum: its value (0 when absent) and
/// whether it is present. `None` past the maximum.
fn readout(v: Option<u32>) -> Option<(i32, i32)> {
    match v {
        None => Some((0, 0)),
        Some(v) if v <= READOUT_MAX_U32 => Some((i32::try_from(v).ok()?, 1)),
        Some(_) => None,
    }
}

/// The play level's fields, a play's and a loading frame's alike: its value
/// and whether it is known, and 0 and false for what a source number widget
/// reserves and never draws, `i32::MIN` and `i32::MAX`
/// (`skin/SkinNumber.java:139`).
pub(crate) const fn level_fields(level: Option<i32>) -> (i32, i32) {
    match level {
        Some(l) if l != i32::MIN && l != i32::MAX => (l, 1),
        _ => (0, 0),
    }
}

/// The number of `chart.*` fields from `field::CHART_DIFFICULTY`.
pub(crate) const CHART_FIELDS: usize = 8;

/// The `chart.*` fields of `meta` from `field::CHART_DIFFICULTY`, in slot
/// order, a play's and a loading frame's alike: each value with its
/// validity, 0 and false for what is unknown or out of its range, the
/// total also for what a source number widget reserves and never draws
/// (`skin/SkinNumber.java:139`).
pub(crate) const fn chart_fields(meta: ChartMetadata) -> [i32; CHART_FIELDS] {
    const fn known(v: Option<i32>, max: i32) -> [i32; 2] {
        match v {
            Some(v) if v >= 0 && v <= max => [v, 1],
            _ => [0, 0],
        }
    }
    const fn small(v: Option<u8>) -> Option<i32> {
        match v {
            Some(v) => Some(v as i32),
            None => None,
        }
    }
    let [difficulty, difficulty_valid] = known(small(meta.difficulty), DIFFICULTY_MAX);
    let [total, total_valid] = known(meta.total, READOUT_MAX);
    let [class, class_valid] = known(small(meta.judge_class), JUDGE_CLASS_MAX);
    let [bga, bga_valid] = match meta.bga {
        Some(b) => [b as i32, 1],
        None => [0, 0],
    };
    [
        difficulty,
        difficulty_valid,
        total,
        total_valid,
        bga,
        bga_valid,
        class,
        class_valid,
    ]
}

/// `d` in ms rounded half up, exactly: `⌊n / d⌋`, plus 1 when the
/// remainder is at least `⌈d / 2⌉` (computed as `d / 2 + d % 2`, so
/// nothing overflows). `None` for a zero denominator or past the maximum.
fn round_duration(d: DurationRatio) -> Option<i32> {
    let (n, den) = (d.numerator, d.denominator);
    let q = n.checked_div(den)?;
    let r = n.checked_rem(den)?;
    let q = if r >= den / 2 + den % 2 { q + 1 } else { q };
    u32::try_from(q)
        .ok()
        .filter(|&q| q <= READOUT_MAX_U32)
        .and_then(|q| i32::try_from(q).ok())
}

/// `3n / (5d)` rounded half up, exactly at the full width of `u128`:
/// with `n = q·d + r`, `3n/d = 3q + m + r′/d` where `m = ⌊3r/d⌋` and
/// `r′ = 3r mod d` come from two additions modulo `d` that never
/// overflow; with `A = 3q + m` and `B = 2A + 5`, the result is
/// `⌊B / 10⌋`, plus 1 when `B` ends in 9 and `r′/d ≥ 1/2` (`B` is odd,
/// so only a 9 can carry). `None` for a zero denominator or when `A` or
/// `B` does not fit, which no producer's value reaches.
pub(crate) fn green_half_up(n: u128, d: u128) -> Option<u128> {
    let q = n.checked_div(d)?;
    let r = n.checked_rem(d)?;
    // `a + b` modulo `d` for `a, b < d`, and whether it wrapped.
    let add = |a: u128, b: u128| {
        if a >= d - b {
            (a - (d - b), 1)
        } else {
            (a + b, 0)
        }
    };
    let (two, m1) = add(r, r);
    let (rest, m2) = add(two, r);
    let a = q.checked_mul(3)?.checked_add(m1 + m2)?;
    let b = a.checked_mul(2)?.checked_add(5)?;
    let carry = b % 10 == 9 && rest >= d - rest;
    Some(b / 10 + u128::from(carry))
}

/// `d` as the readouts round it: whole ms half up and the green number
/// half up from the exact ratio × 3/5; `None` for a zero denominator or
/// either past the maximum.
fn round(d: DurationRatio) -> Option<RoundedDuration> {
    let ms = round_duration(d)?;
    let green = green_half_up(d.numerator, d.denominator)?;
    let green = u32::try_from(green)
        .ok()
        .filter(|&g| g <= READOUT_MAX_U32)?;
    Some(RoundedDuration {
        ms: ms.unsigned_abs(),
        green,
    })
}

/// A rounded duration's fields: ms, green and validity, 0 while absent.
fn duration_fields(d: Option<RoundedDuration>) -> [i32; 3] {
    // Both are rounded within `READOUT_MAX` ([`round`]), so they fit.
    d.map_or([0, 0, 0], |d| [field_of(d.ms), field_of(d.green), 1])
}

/// The native block of a frame: the active `H` from `lanes`' gain in
/// hundredths, the native duration rounded, the target green number and
/// the whites from the stored covers and the lanes' effective lift, each
/// rounded half up exactly. The one rounding of the native block, which
/// the fields and the native HUD read.
///
/// # Errors
///
/// [`BuildError::InvalidReadouts`] for a stored cover above 1 000, a zero
/// duration denominator, a rounded duration past
/// [`READOUT_MAX`](crate::READOUT_MAX), or a target green number outside
/// 1 to 6 000 or given outside Floating.
pub fn native_values(lanes: &LaneView, r: &FrameReadouts) -> Result<NativeValues, BuildError> {
    let max = COVER_PERMILLE.unsigned_abs();
    let c = &r.covers;
    if [c.cover, c.lift, c.hidden]
        .into_iter()
        .any(|v| u32::from(v) > max)
    {
        return Err(BuildError::InvalidReadouts);
    }
    let n = &r.native;
    let target_ok = |g: u32| n.floating && (1..=NATIVE_TARGET_MAX.unsigned_abs()).contains(&g);
    if n.target_green.is_some_and(|g| !target_ok(g)) {
        return Err(BuildError::InvalidReadouts);
    }
    let duration = n
        .duration
        .map(|d| round(d).ok_or(BuildError::InvalidReadouts))
        .transpose()?;

    // A gain is within 1/100 to 20, so its hundredths are 1 to 2 000.
    let hispeed_x100 = lanes.gain.and_then(|g| {
        let (num, den) = (u128::from(g.num()), u128::from(g.den()));
        u32::try_from((200 * num + den) / (2 * den)).ok()
    });
    // Each product is at most 1 000 · 1 000.
    let left = 1_000 - u32::from(lanes.lift.min(1_000));
    let white = |v: u16| u16::try_from(left * u32::from(v) / 1_000).unwrap_or(0);
    Ok(NativeValues {
        hispeed_x100,
        duration,
        target_green: n.target_green,
        sudden_white: white(c.cover),
        lift_white: c.lift,
        hidden_white: white(c.hidden),
        floating: n.floating,
    })
}

/// Whole seconds of `us` µs (at least 0), up or down, bounded by
/// `i32::MAX`.
fn seconds(us: i128, up: bool) -> i32 {
    let us = us.max(0);
    let s = us / 1_000_000 + i128::from(up && us % 1_000_000 != 0);
    i32::try_from(s).unwrap_or(i32::MAX)
}

/// The attempt's progress at `now` from `start` to `end` (µs, `start ≤
/// end`), in parts per million: 0 before `start` (READY, the source's
/// `TIMER_PLAY` off), [`PROGRESS_FULL`] at and after `end` (the source's
/// `min(…, 1)`), and `⌊10⁶ · (now − start) / (end − start)⌋` between
/// them (`skin/property/FloatPropertyFactory.java:128-136`).
fn progress_ppm(now: i128, start: i128, end: i128) -> i32 {
    if now < start {
        return 0;
    }
    if now >= end {
        return PROGRESS_FULL;
    }
    // `start ≤ now < end`: the divisor is positive, the share below 10⁶.
    let full = i128::from(PROGRESS_FULL);
    i32::try_from((now - start) * full / (end - start)).unwrap_or(PROGRESS_FULL)
}

/// A timer's reading at `now`: elapsed ms since `start`, at least 0 (an
/// effect stamped after `now` reads 0), or [`TIMER_OFF`].
fn reading(now: TimeUs, start: Option<TimeUs>) -> i32 {
    start.map_or(TIMER_OFF, |s| {
        i32::try_from(elapsed_us(now, s) / 1_000).unwrap_or(i32::MAX)
    })
}

/// µs from `start` to `now`, at least 0: an effect stamped after `now`
/// shows as just started. Timers and tracks age by this one rule.
pub(crate) fn elapsed_us(now: TimeUs, start: TimeUs) -> i64 {
    now.0.saturating_sub(start.0).max(0)
}

impl Play {
    /// Refuses a poisoned play: one a build refused, until a fresh
    /// prepare.
    pub(crate) fn check(&self) -> Result<(), BuildError> {
        self.poisoned.map_or(Ok(()), Err)
    }

    /// Refuses the frame with `e` and poisons the play.
    fn refuse(&mut self, e: BuildError) -> Result<(), BuildError> {
        self.poisoned = Some(e);
        Err(e)
    }

    /// The frame's readout fields, or `None` when a readout is out of
    /// range or a stored cover setting disagrees with the lanes' effective
    /// amount (stage3.md R4, R6).
    fn readouts(&self, frame: &SkinFrame<'_>) -> Option<Readouts> {
        let r = &frame.readouts;
        let covers = covers(&frame.lanes, &r.covers)?;
        let (bpm_current, bpm_current_valid) = readout(r.bpm_current_x100)?;
        let (hispeed, hispeed_valid) = readout(r.hispeed_x100)?;
        let (duration_ms, duration_valid) = match r.duration {
            None => (0, 0),
            Some(d) => (round_duration(d)?, 1),
        };
        // Sequenced as the source: three fifths of the rounded duration
        // (`skin/property/IntegerPropertyFactory.java:292-314`), so an
        // imported skin shows the green number players compare to carry
        // their settings over.
        let green = i32::try_from(i64::from(duration_ms) * 3 / 5).ok()?;
        let mut variants = [[0; 3]; 8];
        for (fields, v) in variants.iter_mut().zip(&r.duration_variants) {
            let rounded = match v {
                None => None,
                Some(d) => Some(round(*d)?),
            };
            *fields = duration_fields(rounded);
        }
        let native = native_values(&frame.lanes, r).ok()?;
        let (bpm_min, bpm_max, bpm_range_valid) = self.bpm_range.map_or(Some((0, 0, 0)), |b| {
            Some((
                i32::try_from(b.min_x100).ok()?,
                i32::try_from(b.max_x100).ok()?,
                1,
            ))
        })?;
        let (level, level_valid) = level_fields(self.play_level);
        let now = i128::from(frame.now.0);
        let start = i128::from(self.play_at.0);
        let end = i128::from(self.readout_end.0);
        Some(Readouts {
            variants,
            native,
            bpm_current,
            bpm_min,
            bpm_max,
            bpm_current_valid,
            bpm_range_valid,
            bpm_change: i32::from(self.bpm_whole_change == Some(true)),
            bpm_change_valid: i32::from(self.bpm_whole_change.is_some()),
            level,
            level_valid,
            remaining_s: seconds(end - now.max(start), true),
            elapsed_s: seconds(now - start, false),
            progress_ppm: progress_ppm(now, start, end),
            covers,
            hispeed,
            hispeed_valid,
            duration_ms,
            green,
            duration_valid,
        })
    }

    /// Fills this frame's fields and timers: effects through the reducer,
    /// the snapshot into the fields, the built-in starts, each `when`
    /// timer from its condition over the built-in timers only, then the
    /// presentation program's outputs ([`Present::run`]).
    ///
    /// A poisoned play ([`Play::check`]), a snapshot with more counted
    /// empty POORs than POORs, or invalid readouts ([`Play::readouts`]),
    /// in that order, is refused before any effect is applied: everything
    /// stays as it was, and the play is poisoned with that error until a
    /// fresh prepare.
    pub(crate) fn fill(
        &mut self,
        frame: &SkinFrame<'_>,
        whens: &[Program],
        present: Option<&Present>,
    ) -> Result<(), BuildError> {
        self.terminal = false;
        self.check()?;
        let Some(judged) = judged_targets(frame.tally, self.empty_poor_counted) else {
            return self.refuse(BuildError::InconsistentTally);
        };
        let Some(readouts) = self.readouts(frame) else {
            return self.refuse(BuildError::InvalidReadouts);
        };
        for ev in frame.effects {
            self.reducer.apply(ev);
        }
        self.take_holds(frame.notes, frame.now);
        // The frame's one sample of the playhead, which the beat and the
        // lanes share.
        let display = frame.now.0.saturating_add(frame.display_offset.0);
        self.display = self.playhead.position_at(TimeUs(display));
        self.fill_fields(frame, judged, &readouts);
        self.fill_builtin(frame);
        read_timers(
            frame.now,
            &self.fields,
            &mut self.starts,
            &mut self.ms,
            whens,
            |_| true,
        );
        // The presentation program, last: its callbacks read this frame's
        // fields and timers and write only its outputs, its timers and its
        // slots.
        let program = usize::from(timer::BUILTIN).saturating_add(whens.len());
        if let Some(p) = present
            && let Some((fields, outs)) = self.fields.split_at_mut_checked(field::COUNT)
            && let Some(timers) = self.starts.get_mut(program..)
        {
            let state = State {
                fields,
                timers: &self.ms,
            };
            p.run(&mut Io {
                state: &state,
                clock: Clock {
                    state_us: frame.state_us,
                    now: frame.now,
                },
                bank: &mut self.bank,
                outs,
                timers,
                slots: self.slots_run,
            });
            self.terminal = self.slots_run && frame.complete;
        }
        Ok(())
    }

    /// The score fields: the play state's cached score data until the
    /// attempt's first judgment (the first frame whose tally is not the
    /// zero tally: every judgment, the empty POOR included, adds a count),
    /// then the attempt's own, `judged` its judged targets
    /// (`ScoreDataProperty.java:64-99`, updated only at a judgment,
    /// `BMSPlayer.java:1038` at `0.8.9`). With no cached data (a play
    /// state with no judgment yet) it is the attempt's own throughout.
    fn score_now(&mut self, t: &Tally, judged: u64) -> Settled {
        if !self.judged && *t != Tally::default() {
            self.judged = true;
        }
        match self.cached {
            Some(c) if !self.judged => c,
            _ => scores(t, self.reducer.targets(), judged),
        }
    }

    /// The fields, in slot order (`field::*`): the snapshot's, `judged`
    /// its judged targets ([`judged_targets`]); the whole-attempt rate
    /// over the prepared target count, and the gauge kind from the shown
    /// gauge's descriptor, then the frame's `readouts`.
    fn fill_fields(&mut self, frame: &SkinFrame<'_>, judged: u64, readouts: &Readouts) {
        let r = readouts;
        let t = frame.tally;
        let score = self.score_now(t, judged);
        // After `score_now`, whose latch is the update the score figures
        // wait for. The figures read the frame's own tally, never the
        // play state's carried score data.
        let inputs = ScoreInputs {
            ex: t.ex_score,
            judged,
            targets: u64::from(self.reducer.targets()),
            updated: self.judged,
            best: self.score.best,
            target: self.score.target,
            random: self.random,
        };
        self.ratios = score_ratios(inputs);
        let gauge = frame.gauges.get(frame.gauge).map_or(0, |g| {
            let permille = (g.value.0 / GAUGE_UNIT).clamp(0, i64::from(GAUGE_PERMILLE));
            i32::try_from(permille).unwrap_or(0)
        });
        // The shown gauge's look, while it has a descriptor and a snapshot
        // (the gauge widget's rule); `normal` otherwise.
        let kind = frame
            .gauges
            .get(frame.gauge)
            .and(self.gauge_desc(frame.gauge))
            .map(|d| d.look);
        let n = &r.native;
        let [native_ms, native_green, native_valid] = duration_fields(n.duration);
        let native: [i32; (field::NATIVE_FLOATING + 1 - field::NATIVE_HISPEED_X100) as usize] = [
            n.hispeed_x100.map_or(0, field_of),
            i32::from(n.hispeed_x100.is_some()),
            native_ms,
            native_green,
            native_valid,
            n.target_green.map_or(0, field_of),
            i32::from(n.target_green.is_some()),
            i32::from(n.sudden_white),
            i32::from(n.lift_white),
            i32::from(n.hidden_white),
            i32::from(n.floating),
        ];
        let head: [i32; field::DURATION_VARIANTS as usize] = [
            count(t.combo),
            count(t.max_combo),
            score.ex_score,
            count(t.bp),
            count(t.counts.perfect),
            count(t.counts.great),
            count(t.counts.good),
            count(t.counts.bad),
            count(t.counts.poor),
            count(t.empty_poor),
            count(self.reducer.targets()),
            gauge,
            // Loaded: a play's frames never load (the loading runtime's do).
            100,
            score.rate_current_bp,
            score.rate_whole_bp,
            kind.unwrap_or(GaugeLook::Normal).schema_value(),
            i32::from(kind.is_some()),
            r.bpm_current,
            r.bpm_min,
            r.bpm_max,
            r.bpm_current_valid,
            r.bpm_range_valid,
            r.remaining_s,
            r.elapsed_s,
            r.covers.cover,
            r.covers.lift,
            r.covers.hidden,
            r.covers.absolute,
            r.covers.cover_enabled,
            r.covers.lift_enabled,
            r.covers.hidden_enabled,
            r.covers.adjusting,
            r.hispeed,
            r.hispeed_valid,
            r.duration_ms,
            r.green,
            r.duration_valid,
            count(t.fast()),
            count(t.slow()),
            count(t.early.perfect),
            count(t.late.perfect),
            count(t.early.great),
            count(t.late.great),
            count(t.early.good),
            count(t.late.good),
            count(t.early.bad),
            count(t.late.bad),
            count(t.early.poor),
            count(t.late.poor),
            count(t.empty_poor_early),
            count(t.empty_poor_late),
        ];
        let fields = head
            .into_iter()
            .chain(r.variants.into_iter().flatten())
            .chain(native)
            .chain([r.bpm_change, r.bpm_change_valid, r.level, r.level_valid])
            .chain([
                i32::from(self.autoplay),
                self.reducer.recent.ms,
                i32::from(self.reducer.recent.imperfect),
            ])
            // Each lane's visual state: its effects' code against the
            // view's latest press.
            .chain(
                Lane::all()
                    .zip(self.reducer.lanes)
                    .map(|(lane, j)| j.read(frame.input.lane(lane))),
            )
            .chain([r.progress_ppm])
            .chain(self.chart_fields)
            .chain([score.point])
            .chain([
                i32::from(self.drawable_chart().present(ChartImage::Stage)),
                i32::from(self.drawable_chart().present(ChartImage::Back)),
            ])
            .chain(score_figures(inputs));
        for (slot, v) in self.fields.iter_mut().zip(fields) {
            *slot = v;
        }
    }

    /// Every built-in timer's start at this frame.
    fn fill_builtin(&mut self, frame: &SkinFrame<'_>) {
        let now = frame.now;
        let ready = (now >= self.ready_at).then_some(self.ready_at);
        let play = (now >= self.play_at).then_some(self.play_at);
        let shown = frame.gauges.get(frame.gauge);
        // From the lifecycle's Closing start, never the evaluator's
        // `failed_at`: a failure that does not close the play (a hidden
        // gauge's, any under play-to-end) starts nothing, and a quit that
        // closes it does.
        let failed = frame.closing;
        // The reducer's start, masked during READY, so skins animate the
        // full gauge from the start of play, and while the snapshot is
        // below the maximum: a dropped below-maximum effect cannot leave
        // it on (`play/BMSPlayer.java:639`).
        let gauge_max = match (
            play,
            self.reducer.gauge_max.get(frame.gauge).copied().flatten(),
            shown,
            self.reducer.gauge_top.get(frame.gauge),
        ) {
            (Some(_), Some(at), Some(g), Some(&top)) if g.value >= top => Some(at),
            _ => None,
        };
        // Started at the notes' end itself by the first playing frame at
        // or past it, however late that frame comes; once set, only this
        // keeps it.
        let mut last_note = self.timer(timer::LAST_NOTE);
        if last_note.is_none() && frame.playing && now >= self.last_note_end {
            last_note = Some(self.last_note_end);
        }
        let beat = ready.and_then(|_| self.beat_start(now, frame.display_offset));
        let judge = self.reducer.judge;
        let mut set = |slot: u16, start: Option<TimeUs>| {
            if let Some(s) = self.starts.get_mut(usize::from(slot)) {
                *s = start;
            }
        };
        set(timer::READY, ready);
        set(timer::SCREEN, ready);
        set(timer::PLAY, play);
        set(timer::FAILED, failed);
        set(timer::LAST_NOTE, last_note);
        set(timer::FULL_COMBO, self.reducer.full_combo);
        set(timer::GAUGE_MAX, gauge_max);
        set(timer::BEAT, beat);
        set(timer::JUDGE, judge.map(|(_, at)| at));
        for grade in Grade::ALL {
            let at = judge.filter(|&(g, _)| g == grade).map(|(_, at)| at);
            set(grade.timer(), at);
        }
        set(timer::COMBO_UP, self.reducer.combo_up);
        // Loading is the loading runtime's (`loading.rs`).
        set(timer::LOADING, None);
        for lane in Lane::all() {
            let l = lane.index();
            let hold = self.holds.get(l).copied().flatten().map(|h| h.start);
            let bomb = self.reducer.bomb.get(l).copied().flatten();
            let mine = self.reducer.mine.get(l).copied().flatten();
            // Key-on and key-off exclude each other, from the view's input
            // in µs, whatever the play phase: a lane held since the opening
            // has no press, so its key-on runs from the READY start.
            let key = frame.input.lane(lane);
            let (press, release) = if key.held {
                (Some(key.pressed.unwrap_or(self.ready_at)), None)
            } else {
                (None, key.released)
            };
            set(lane_timer(LaneTimer::Press, lane), press);
            set(lane_timer(LaneTimer::Release, lane), release);
            set(lane_timer(LaneTimer::Bomb, lane), bomb);
            set(lane_timer(LaneTimer::Hold, lane), hold);
            set(lane_timer(LaneTimer::Mine, lane), mine);
        }
    }

    /// The `beat` timer's start: the latest integer-beat crossing of the
    /// exact playhead on the display clock, `now + display_offset` clamped to the scroll map's
    /// domain, moved back to `now`'s clock. A STOP does not restart it.
    /// Off when that start leaves the time range.
    fn beat_start(&mut self, now: TimeUs, offset: TimeUs) -> Option<TimeUs> {
        let disp = now.0.saturating_add(offset.0).clamp(-DOMAIN, DOMAIN);
        // The frame's sample, at `disp` before its clamp, which the
        // playhead clamps alike.
        let b = self.display.0.div_euclid(BEAT);
        let crossed = match self.beat {
            Some((cached, at)) if cached == b => at,
            _ => {
                let at = self.crossing(b, disp);
                self.beat = Some((b, at));
                at
            }
        };
        TimeUs(crossed).checked_sub(offset)
    }

    /// The first µs in `[-DOMAIN, disp]` where the display position reaches
    /// beat `b`, for `b` at or before `disp`'s beat. The playhead never
    /// decreases, so a binary search takes at most 34 probes.
    fn crossing(&self, b: i128, disp: i64) -> i64 {
        // `b` is the floor of a position over BEAT, so `b · BEAT` is at or
        // below that position and fits.
        let target = b.saturating_mul(BEAT);
        let (mut lo, mut hi) = (-DOMAIN, disp);
        while lo < hi {
            // Both are within ±2^32, so the sum fits.
            let mid = (lo + hi).div_euclid(2);
            if self.playhead.position_at(TimeUs(mid)).0 >= target {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        hi
    }
}

#[cfg(test)]
mod tests {
    use super::green_half_up;

    /// `3n / (5d)` rounded half up where `6n + 5d` fits: the reference.
    fn reference(n: u128, d: u128) -> u128 {
        (6 * n + 5 * d) / (10 * d)
    }

    /// The green number rounds half up from the exact ratio at the full
    /// width of `u128`: both terms near its maximum, a remainder exactly
    /// half way and just below it, `d = 1`, and `None` once `2A + 5`
    /// passes the width (`3n/d` above about half of it).
    #[test]
    fn greens_round_half_up_at_full_width() {
        let max = u128::MAX;
        // `n = max − 5 = 10 · (2k + 1)`: `n / 20` ends in exactly .5.
        assert_eq!(
            green_half_up(max - 5, 12),
            Some(17_014_118_346_046_923_173_168_730_371_588_410_573)
        );
        assert_eq!(
            green_half_up(max - 6, 12),
            Some(17_014_118_346_046_923_173_168_730_371_588_410_572)
        );
        assert_eq!(
            green_half_up(max, 12),
            Some(17_014_118_346_046_923_173_168_730_371_588_410_573)
        );
        assert_eq!(green_half_up(max, 6), None);
        assert_eq!(green_half_up(max, max), Some(1));
        // `d = 6t` just below the maximum: `5t / 6t · 3/5` is exactly 1/2.
        let t = max / 6;
        assert_eq!(green_half_up(5 * t, 6 * t), Some(1));
        assert_eq!(green_half_up(5 * t - 1, 6 * t), Some(0));
        // `d = 1`, `u = max / 12 = 5a + 1`: `3u / 5` is `3a + 0.6`, and
        // `3(u − 1) / 5` is `3a`; `3t` leaves no room for `2A + 5`.
        let u = max / 12;
        assert_eq!(
            green_half_up(u, 1),
            Some(17_014_118_346_046_923_173_168_730_371_588_410_573)
        );
        assert_eq!(
            green_half_up(u - 1, 1),
            Some(17_014_118_346_046_923_173_168_730_371_588_410_572)
        );
        assert_eq!(green_half_up(t, 1), None);
        assert_eq!(green_half_up(max, 1), None);
        assert_eq!(green_half_up(1, 0), None);
        for d in 1..=60 {
            for n in 0..=600 {
                assert_eq!(green_half_up(n, d), Some(reference(n, d)), "{n}/{d}");
            }
        }
    }
}
