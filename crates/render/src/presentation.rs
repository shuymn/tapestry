//! One frame's presentation (design.md "Play readouts"): the lanes and
//! the readouts both skins draw, made from one presentation snapshot and
//! the chart's tempos by the exact projections
//! ([`game::readouts::project`]). Allocation-free.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::Speed;
use game::adjustment::PresentationSnapshot;
use game::readouts::{Projection, project};
use game::{Ratio, References};
use skin_model::{DurationRatio, FrameReadouts, Gain, LaneView, NativeReadouts};

use crate::cover_settings;

/// The lanes and the readouts of `snapshot` at the chart's own (1×) tempo
/// `current_bpm` at the display head and its `references`, played at
/// `speed`; `None` where unknown. With `same_attempt` false the snapshot
/// belongs to another attempt's presentation: the gain and every value
/// that depends on it (the source hi-speed, the durations and their
/// variants, the native duration) are unavailable, while the covers, the
/// settings and `adjusting` are the snapshot's.
///
/// The lanes carry the gain `H`, or `None` where it is unknown or outside
/// the gain's range, and the effective covers: each configured amount
/// while its cover is on, 0 while it is off.
#[must_use]
pub fn frame(
    snapshot: &PresentationSnapshot,
    current_bpm: Option<Ratio>,
    references: Option<References>,
    speed: Speed,
    same_attempt: bool,
) -> (LaneView, FrameReadouts) {
    let mut s = *snapshot;
    if !same_attempt {
        s.hispeed = None;
    }
    let p: Projection = project(&s, current_bpm, references, speed);
    let covers = s.options.covers();
    let lanes = LaneView {
        gain: p.gain.and_then(|h| Gain::new(h.num(), h.den())),
        cover: covers.sudden.effective(),
        lift: covers.lift.effective(),
        hidden: covers.hidden.effective(),
    };
    let readouts = FrameReadouts {
        bpm_current_x100: p.bpm_current_x100,
        hispeed_x100: p.hispeed_x100,
        duration: p.duration.map(duration),
        covers: cover_settings(&s),
        duration_variants: p.variants.map(|v| v.map(duration)),
        native: NativeReadouts {
            duration: p.native_duration.map(duration),
            target_green: p.target_green,
            floating: p.floating,
        },
    };
    (lanes, readouts)
}

/// `r` ms as the model's exact duration.
fn duration(r: Ratio) -> DurationRatio {
    DurationRatio {
        numerator: u128::from(r.num()),
        denominator: u128::from(r.den()),
    }
}

#[cfg(test)]
mod tests {
    use game::adjustment::PresentationSnapshot;
    use game::{
        Cover, Covers, HiSpeedReference, HiSpeedSettings, PlayOptions, Ratio, References, SpeedMode,
    };
    use skin_model::{FrameReadouts, Gain, LaneView, RoundedDuration, native_values};

    use super::frame;
    use bms_model::Speed;

    fn cover(amount: u16, on: bool) -> Cover {
        Cover { amount, on }
    }

    fn floating(green: u32) -> HiSpeedSettings {
        HiSpeedSettings {
            mode: SpeedMode::Floating,
            reference: HiSpeedReference::Start,
            green_target: green,
            ..HiSpeedSettings::default()
        }
    }

    fn snapshot(h: Option<Ratio>, covers: Covers, adjusting: bool) -> PresentationSnapshot {
        PresentationSnapshot {
            options: PlayOptions::new(floating(300), covers).unwrap(),
            hispeed: h,
            target: None,
            display_offset_us: 0,
            adjusting,
            generation: 0,
            sequence: 1,
            epoch: 0,
        }
    }

    fn bpm(n: u64) -> Option<Ratio> {
        Some(Ratio::integer(n))
    }

    fn rounded(lanes: &LaneView, r: &FrameReadouts) -> Option<RoundedDuration> {
        native_values(lanes, r).unwrap().duration
    }

    /// 150 BPM at H 3.2 with no cover: 500 ms, green 300; the source
    /// hi-speed is H itself.
    #[test]
    fn h_3_2_at_150_bpm_is_500_ms_and_green_300() {
        let s = snapshot(Ratio::new(16, 5), Covers::default(), false);
        let refs = References::all(Ratio::integer(150));
        let (lanes, r) = frame(&s, bpm(150), Some(refs), Speed::NORMAL, true);
        assert_eq!(lanes.gain, Gain::new(16, 5));
        assert_eq!((lanes.cover, lanes.lift, lanes.hidden), (0, 0, 0));
        assert_eq!(r.hispeed_x100, Some(320));
        assert_eq!(r.bpm_current_x100, Some(15_000));
        assert_eq!(
            rounded(&lanes, &r),
            Some(RoundedDuration {
                ms: 500,
                green: 300
            })
        );
        let v = native_values(&lanes, &r).unwrap();
        assert_eq!(
            (v.hispeed_x100, v.target_green, v.floating),
            (Some(320), Some(300), true)
        );
        assert!(r.duration_variants.iter().all(Option::is_some));
    }

    /// LIFT 1000 leaves no field: the source hi-speed and every duration
    /// variant are unavailable, the native duration is 0 and the whites
    /// stay (design §7.1).
    #[test]
    fn lift_1000_leaves_no_field() {
        let covers = Covers {
            sudden: cover(200, true),
            lift: cover(1_000, true),
            hidden: cover(300, true),
        };
        let s = snapshot(Ratio::new(1, 100), covers, false);
        let refs = References::all(Ratio::integer(150));
        let (lanes, r) = frame(&s, bpm(150), Some(refs), Speed::NORMAL, true);
        assert_eq!((lanes.cover, lanes.lift, lanes.hidden), (200, 1_000, 300));
        assert_eq!(lanes.gain, Gain::new(1, 100));
        assert_eq!(r.hispeed_x100, None);
        assert_eq!(r.duration, None);
        assert_eq!(r.duration_variants, [None; 8]);
        assert_eq!(
            rounded(&lanes, &r),
            Some(RoundedDuration { ms: 0, green: 0 })
        );
        let v = native_values(&lanes, &r).unwrap();
        assert_eq!(
            (v.hispeed_x100, v.sudden_white, v.lift_white, v.hidden_white),
            (Some(1), 0, 1_000, 0)
        );
    }

    /// Full SUDDEN+ at H 0.01: 0 ms covered, the uncovered variant 160 000
    /// ms (design §7.2).
    #[test]
    fn full_sudden_projects_zero_and_the_uncovered_variant() {
        let covers = Covers {
            sudden: cover(1_000, true),
            ..Covers::default()
        };
        let s = snapshot(Ratio::new(1, 100), covers, false);
        let (lanes, r) = frame(&s, bpm(150), None, Speed::NORMAL, true);
        assert_eq!(lanes.cover, 1_000);
        assert_eq!(r.hispeed_x100, Some(1));
        assert_eq!(
            rounded(&lanes, &r),
            Some(RoundedDuration { ms: 0, green: 0 })
        );
        let [covered, uncovered, rest @ ..] = r.duration_variants;
        assert_eq!(covered.map(|d| d.numerator), Some(0));
        let u = uncovered.unwrap();
        assert_eq!(u.numerator / u.denominator, 160_000);
        assert_eq!(rest, [None; 6], "no references");
    }

    /// A snapshot of another attempt keeps its covers and `adjusting` but
    /// gives no gain and nothing that depends on it.
    #[test]
    fn another_attempts_snapshot_has_no_gain() {
        let covers = Covers {
            sudden: cover(250, true),
            lift: cover(100, false),
            ..Covers::default()
        };
        let s = snapshot(Ratio::new(16, 5), covers, true);
        let refs = References::all(Ratio::integer(150));
        let (lanes, r) = frame(&s, bpm(150), Some(refs), Speed::NORMAL, false);
        assert_eq!(lanes.gain, None);
        assert_eq!((lanes.cover, lanes.lift), (250, 0));
        assert_eq!(r.hispeed_x100, None);
        assert_eq!(r.duration, None);
        assert_eq!(r.duration_variants, [None; 8]);
        assert_eq!(r.native.duration, None);
        assert_eq!(r.bpm_current_x100, Some(15_000));
        assert!(r.covers.adjusting);
        assert_eq!(
            (
                r.covers.cover,
                r.covers.cover_enabled,
                r.covers.lift,
                r.covers.lift_enabled
            ),
            (250, true, 100, false)
        );
        assert_eq!(r.native.target_green, Some(300));
    }
}
