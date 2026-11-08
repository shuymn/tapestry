//! Tracks: the R2 endpoint table for hold, hide and loop, single-key
//! tracks, eases, carried-forward props, exact wide interpolation, and
//! every load error.

use bms_model::TimeUs;
use proptest::prelude::*;
use skin_model::{
    Ease, End, KeyDef, MAX_GEOMETRY, MAX_KEY_US, MAX_TRACK_KEYS, Prop, Track, TrackError,
};

/// A key at `at` µs setting only x.
fn kx(at: i64, x: i32) -> KeyDef {
    let mut props = [None; 9];
    props[0] = Some(x);
    KeyDef {
        at: TimeUs(at),
        props,
    }
}

/// The R2 table's track: x = 0, 200, 400 at 100, 300, 500 ms.
fn table_track(end: End) -> Track {
    Track::new(
        &[kx(100_000, 0), kx(300_000, 200), kx(500_000, 400)],
        Ease::Linear,
        end,
    )
    .unwrap()
}

fn x_at(track: &Track, t: i64) -> Option<f32> {
    track.sample(t).map(|s| s.rect[0])
}

#[track_caller]
fn assert_near(got: Option<f32>, want: Option<f32>) {
    match (got, want) {
        (Some(g), Some(w)) => assert!((g - w).abs() < 1e-4, "{g} vs {w}"),
        _ => assert_eq!(got, want),
    }
}

const L: i64 = 200_000;

/// A time and x under hold, hide and loop.
type Row = (i64, Option<f32>, Option<f32>, Option<f32>);

#[test]
fn endpoint_table_matches_r2() {
    let hold = table_track(End::Hold);
    let hide = table_track(End::Hide);
    let looped = table_track(End::Loop { from: TimeUs(L) });
    // (t, hold, hide, loop); exact rows (at keys and at L) compare with ==.
    let exact: &[Row] = &[
        (99_999, None, None, None),
        (100_000, Some(0.0), Some(0.0), Some(0.0)),
        (200_000, Some(100.0), Some(100.0), Some(100.0)),
        (300_000, Some(200.0), Some(200.0), Some(200.0)),
        (500_000, Some(400.0), Some(400.0), Some(100.0)),
        (500_001, Some(400.0), None, None),
        (800_000, Some(400.0), None, Some(100.0)),
    ];
    for &(t, h, d, l) in exact {
        assert_eq!(x_at(&hold, t), h, "hold at {t}");
        assert_eq!(x_at(&hide, t), d, "hide at {t}");
        if t != 500_001 {
            assert_eq!(x_at(&looped, t), l, "loop at {t}");
        }
    }
    let near: &[Row] = &[
        (100_001, Some(0.001), Some(0.001), Some(0.001)),
        (199_999, Some(99.999), Some(99.999), Some(99.999)),
        (200_001, Some(100.001), Some(100.001), Some(100.001)),
        (499_999, Some(399.999), Some(399.999), Some(399.999)),
        (500_001, Some(400.0), None, Some(100.001)),
        (799_999, Some(400.0), None, Some(399.999)),
    ];
    for &(t, h, d, l) in near {
        assert_near(x_at(&hold, t), h);
        assert_near(x_at(&hide, t), d);
        assert_near(x_at(&looped, t), l);
    }
}

#[test]
fn single_key_tracks() {
    let key = [kx(100_000, 7)];
    let hold = Track::new(&key, Ease::Linear, End::Hold).unwrap();
    let hide = Track::new(&key, Ease::Linear, End::Hide).unwrap();
    assert_eq!(x_at(&hold, 99_999), None);
    assert_eq!(x_at(&hold, 100_000), Some(7.0));
    assert_eq!(x_at(&hold, 100_001), Some(7.0));
    assert_eq!(x_at(&hide, 99_999), None);
    assert_eq!(x_at(&hide, 100_000), Some(7.0));
    assert_eq!(x_at(&hide, 100_001), None);
    assert_eq!(
        Track::new(
            &key,
            Ease::Linear,
            End::Loop {
                from: TimeUs(100_000)
            }
        ),
        Err(TrackError::LoopOutside {
            from: 100_000,
            last: 100_000
        })
    );
}

/// A loop point before the first key, as the source samples it
/// (`SkinObject.java:358-374` at `0.8.9`): the time wraps first, then a
/// time before the first key draws nothing. Keys at 5 and 10 s with the
/// loop at 0 (period 10 s): drawn at 5 s and 15 s, at 7.5 s and 17.5 s
/// halfway between the keys, and hidden at 10 s, 12 s and 20 s, where
/// the time wraps to 0 and 2 s.
#[test]
fn a_loop_before_the_first_key_leaves_a_gap_each_period() {
    let keys = [kx(5_000_000, 0), kx(10_000_000, 100)];
    let t = Track::new(&keys, Ease::Linear, End::Loop { from: TimeUs(0) }).unwrap();
    for (ms, x) in [
        (4_999, None),
        (5_000, Some(0.0)),
        (7_500, Some(50.0)),
        (9_999, Some(99.98)),
        (10_000, None),
        (12_000, None),
        (15_000, Some(0.0)),
        (17_500, Some(50.0)),
        (20_000, None),
    ] {
        assert_eq!(
            x_at(&t, ms * 1_000).map(|v| (v * 100.0).round() / 100.0),
            x,
            "{ms} ms"
        );
    }
}

/// A single key after its loop point never draws, as in the source: every
/// time past the loop point wraps before the key. Key at 100 ms, loop at
/// 40 ms (period 60 ms): the key's own time and each following period
/// boundary wrap to 40 ms, and the times just past them to just after it.
#[test]
fn a_single_key_after_its_loop_point_never_draws() {
    let key = [kx(100_000, 7)];
    let t = Track::new(
        &key,
        Ease::Linear,
        End::Loop {
            from: TimeUs(40_000),
        },
    )
    .unwrap();
    for us in [
        40_000, 99_999, 100_000, 100_001, 159_999, 160_000, 160_001, 220_000, 280_000,
    ] {
        assert_eq!(x_at(&t, us), None, "{us} µs");
    }
}

/// Keys sharing a time (`SkinObject.java:545-575` at `0.8.9`): the last
/// of them shows at it, and before it the track moves toward the first.
#[test]
fn keys_at_one_time_show_the_last_and_approach_the_first() {
    let keys = [
        kx(0, 0),
        kx(100_000, 50),
        kx(100_000, 120),
        kx(100_000, 200),
        kx(200_000, 300),
    ];
    for ease in [Ease::Linear, Ease::In, Ease::Out, Ease::Step] {
        let t = Track::new(&keys, ease, End::Hold).unwrap();
        assert_eq!(x_at(&t, 100_000), Some(200.0), "{ease:?}");
        assert!(x_at(&t, 99_999).unwrap() <= 50.0, "{ease:?}");
    }
    let lin = Track::new(&keys, Ease::Linear, End::Hold).unwrap();
    assert_near(x_at(&lin, 150_000), Some(250.0));
    assert_near(x_at(&lin, 99_000), Some(49.5));
    assert_near(x_at(&lin, 101_000), Some(201.0));
    let step = Track::new(&keys, Ease::Step, End::Hold).unwrap();
    assert_eq!(x_at(&step, 99_999), Some(0.0));
    // At 0, the second of two keys shows.
    let zero = Track::new(
        &[kx(0, 0), kx(0, 40), kx(100_000, 140)],
        Ease::Linear,
        End::Hold,
    )
    .unwrap();
    assert_eq!(x_at(&zero, 0), Some(40.0));
    assert_near(x_at(&zero, 50_000), Some(90.0));
    // An end pair under hide: the last at its time, nothing after.
    let end = Track::new(
        &[kx(0, 0), kx(200_000, 100), kx(200_000, 30)],
        Ease::Linear,
        End::Hide,
    )
    .unwrap();
    assert_eq!(x_at(&end, 200_000), Some(30.0));
    assert_eq!(x_at(&end, 200_001), None);
}

/// A negative loop point wraps in `i128`, so no time overflows: keys at
/// 100 and 300 ms with the loop 2 s before 0 (period 2.3 s).
#[test]
fn a_negative_loop_point_wraps_any_time() {
    let from = -2_000_000_i64;
    let keys = [kx(100_000, 0), kx(300_000, 200)];
    let t = Track::new(&keys, Ease::Linear, End::Loop { from: TimeUs(from) }).unwrap();
    let wrapped = |us: i64| {
        let (u, f) = (i128::from(us), i128::from(from));
        i64::try_from(f + (u - f).rem_euclid(300_000 - f)).unwrap()
    };
    // `i64::MAX` wraps to −1 724 193 µs: in the gap, so nothing draws.
    assert_eq!(wrapped(i64::MAX), -1_724_193);
    assert_eq!(x_at(&t, i64::MAX), None);
    // A period before, less the 1 924 193 µs from there to 200 ms.
    assert_eq!(wrapped(i64::MAX - 375_807), 200_000);
    assert_eq!(x_at(&t, i64::MAX - 375_807), Some(100.0));
    assert_eq!(x_at(&t, i64::MIN), None);
    // A period later the track shows what it showed.
    for us in [100_000, 150_000, 299_999] {
        assert_eq!(x_at(&t, us + 2_300_000), x_at(&t, us), "{us}");
        assert_eq!(wrapped(us + 2_300_000), us);
    }
    assert_eq!(x_at(&t, 300_000), None);
    assert_eq!(x_at(&t, 2_399_999), None);
    assert_eq!(x_at(&t, 2_400_000), Some(0.0));
    // The earliest loop point: `t − from` passes no bound either.
    let far = Track::new(
        &keys,
        Ease::Linear,
        End::Loop {
            from: TimeUs(-MAX_KEY_US),
        },
    )
    .unwrap();
    assert_eq!(x_at(&far, 200_000), Some(100.0));
    // `i64::MAX` wraps to −10 326 424 193 µs, before the first key.
    assert_eq!(x_at(&far, i64::MAX), None);
}

#[test]
fn loop_from_the_first_key_and_a_start_at_zero() {
    let looped = table_track(End::Loop {
        from: TimeUs(100_000),
    });
    assert_eq!(x_at(&looped, 500_000), Some(0.0));
    let at_zero = Track::new(&[kx(0, 5), kx(10, 15)], Ease::Linear, End::Hold).unwrap();
    assert_eq!(x_at(&at_zero, 0), Some(5.0));
    assert_eq!(x_at(&at_zero, -1), None);
}

#[test]
fn eases_and_carried_props() {
    let keys = [kx(100_000, 0), kx(300_000, 200)];
    let at = |ease| x_at(&Track::new(&keys, ease, End::Hold).unwrap(), 200_000);
    assert_eq!(at(Ease::In), Some(50.0));
    assert_eq!(at(Ease::Out), Some(150.0));
    assert_eq!(at(Ease::Linear), Some(100.0));
    let step = Track::new(
        &[kx(100_000, 0), kx(300_000, 200), kx(500_000, 400)],
        Ease::Step,
        End::Hold,
    )
    .unwrap();
    assert_eq!(x_at(&step, 299_999), Some(0.0));
    assert_eq!(x_at(&step, 300_000), Some(200.0));

    // The second key leaves x out and sets y: x holds, y interpolates.
    let mut second = KeyDef {
        at: TimeUs(200),
        props: [None; 9],
    };
    second.props[1] = Some(100);
    let mut first = kx(0, 30);
    first.props[1] = Some(0);
    let carried = Track::new(&[first, second], Ease::Linear, End::Hold).unwrap();
    let s = carried.sample(100).unwrap();
    assert_eq!(s.rect[0], 30.0);
    assert_eq!(s.rect[1], 50.0);
    // Key 0's defaults: geometry 0, color and alpha 255 (1.0), angle 0.
    assert_eq!(s.rect[2], 0.0);
    assert_eq!(s.color, [1.0; 4]);
    assert_eq!(s.angle, 0.0);
}

#[test]
fn colors_map_to_unit_range() {
    let mut props = [None; 9];
    props[Prop::Alpha as usize] = Some(0);
    props[Prop::R as usize] = Some(255);
    props[Prop::G as usize] = Some(51);
    props[Prop::Angle as usize] = Some(90);
    let track = Track::new(
        &[KeyDef {
            at: TimeUs(0),
            props,
        }],
        Ease::Linear,
        End::Hold,
    )
    .unwrap();
    let s = track.sample(0).unwrap();
    assert_eq!(s.color, [1.0, 0.2, 1.0, 0.0]);
    assert_eq!(s.angle, 90.0);
}

/// Opposite extreme endpoints across the widest key span: every product
/// leaves `i64` (x² reaches 2^67), so only the `i128` arithmetic is exact.
#[test]
fn wide_interpolation_is_exact() {
    let mut a = [None; 9];
    let mut b = [None; 9];
    for i in 0..4 {
        a[i] = Some(-MAX_GEOMETRY);
        b[i] = Some(MAX_GEOMETRY);
    }
    let keys = [
        KeyDef {
            at: TimeUs(0),
            props: a,
        },
        KeyDef {
            at: TimeUs(MAX_KEY_US),
            props: b,
        },
    ];
    let d = MAX_KEY_US;
    let g = f64::from(MAX_GEOMETRY);
    for ease in [Ease::Linear, Ease::In, Ease::Out, Ease::Step] {
        let track = Track::new(&keys, ease, End::Hold).unwrap();
        for t in [0, 1, d / 3, d / 2, d - 1, d, i64::MAX] {
            let x = (t.min(d) as f64) / (d as f64);
            let f = match ease {
                Ease::Linear => x,
                Ease::In => x * x,
                Ease::Out => x * (2.0 - x),
                Ease::Step => {
                    if t >= d {
                        1.0
                    } else {
                        0.0
                    }
                }
            };
            let want = (-g + 2.0 * g * f) as f32;
            let got = track.sample(t).unwrap().rect[0];
            assert!(
                (got - want).abs() <= 1.0,
                "{ease:?} at {t}: {got} vs {want}"
            );
        }
        assert_eq!(track.sample(0).unwrap().rect[0], -MAX_GEOMETRY as f32);
        assert_eq!(track.sample(d).unwrap().rect[0], MAX_GEOMETRY as f32);
    }
}

#[test]
fn refuses_bad_tracks() {
    let h = End::Hold;
    let lin = Ease::Linear;
    assert_eq!(Track::new(&[], lin, h), Err(TrackError::NoKeys));
    // Keys may share a time; a decrease is refused.
    assert!(Track::new(&[kx(0, 0), kx(0, 1)], lin, h).is_ok());
    assert_eq!(
        Track::new(&[kx(0, 0), kx(10, 1), kx(5, 2)], lin, h),
        Err(TrackError::NotIncreasing { index: 2 })
    );
    assert_eq!(
        Track::new(&[kx(0, 0), kx(10, 1), kx(10, 2), kx(9, 3)], lin, h),
        Err(TrackError::NotIncreasing { index: 3 })
    );
    assert_eq!(
        Track::new(&[kx(-1, 0)], lin, h),
        Err(TrackError::NegativeStart { at: -1 })
    );
    let three = [kx(100, 0), kx(200, 1), kx(300, 2)];
    for from in [300, -MAX_KEY_US - 1] {
        assert_eq!(
            Track::new(&three, lin, End::Loop { from: TimeUs(from) }),
            Err(TrackError::LoopOutside { from, last: 300 })
        );
    }
    // Before the first key, down to 4 h before 0, is a loop point the
    // source keeps.
    for from in [0, -1, -MAX_KEY_US] {
        assert!(Track::new(&three, lin, End::Loop { from: TimeUs(from) }).is_ok());
    }
    let many: Vec<_> = (0..=MAX_TRACK_KEYS as i64).map(|i| kx(i, 0)).collect();
    assert!(Track::new(&many[..MAX_TRACK_KEYS], lin, h).is_ok());
    assert_eq!(
        Track::new(&many, lin, h),
        Err(TrackError::TooManyKeys {
            n: MAX_TRACK_KEYS + 1
        })
    );
    assert!(Track::new(&[kx(MAX_KEY_US, 0)], lin, h).is_ok());
    assert_eq!(
        Track::new(&[kx(0, 0), kx(MAX_KEY_US + 1, 0)], lin, h),
        Err(TrackError::KeyTooLate { index: 1 })
    );
    let mut color = [None; 9];
    color[Prop::G as usize] = Some(256);
    assert_eq!(
        Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props: color
            }],
            lin,
            h
        ),
        Err(TrackError::OutOfRange {
            index: 0,
            prop: Prop::G
        })
    );
    assert_eq!(
        Track::new(&[kx(0, 0), kx(1, MAX_GEOMETRY + 1)], lin, h),
        Err(TrackError::OutOfRange {
            index: 1,
            prop: Prop::X
        })
    );
    assert!(Track::new(&[kx(0, -MAX_GEOMETRY)], lin, h).is_ok());
}

/// Up to 8 strictly increasing keys from `k0 ≥ 0`, with x values.
fn keys() -> impl Strategy<Value = Vec<KeyDef>> {
    (
        0..1_000_000_i64,
        prop::collection::vec((1..500_000_i64, -10_000..10_000_i32), 1..8),
    )
        .prop_map(|(k0, steps)| {
            let mut at = k0;
            steps
                .into_iter()
                .enumerate()
                .map(|(i, (dt, x))| {
                    if i > 0 {
                        at += dt;
                    }
                    kx(at, x)
                })
                .collect()
        })
}

fn ease() -> impl Strategy<Value = Ease> {
    prop_oneof![
        Just(Ease::Linear),
        Just(Ease::In),
        Just(Ease::Out),
        Just(Ease::Step)
    ]
}

proptest! {
    #[test]
    fn hold_and_loop_fold_time(keys in keys(), ease in ease(), t in 0..10_000_000_i64, pick in 0..8_usize) {
        let k0 = keys[0].at.0;
        let kn = keys[keys.len() - 1].at.0;
        let hold = Track::new(&keys, ease, End::Hold).unwrap();
        prop_assert_eq!(hold.sample(t), hold.sample(t.min(kn)));
        // Every key returns its values exactly.
        for k in &keys {
            prop_assert_eq!(x_at(&hold, k.at.0), Some(k.props[0].unwrap() as f32));
        }
        if keys.len() > 1 {
            let from = keys[pick % (keys.len() - 1)].at.0;
            let looped = Track::new(&keys, ease, End::Loop { from: TimeUs(from) }).unwrap();
            if t > from {
                let folded = from + (t - from).rem_euclid(kn - from);
                prop_assert_eq!(looped.sample(t), hold.sample(folded));
            } else {
                prop_assert_eq!(looped.sample(t), hold.sample(t));
            }
            // Every key but the last returns its values exactly; the last
            // shows the loop point.
            for k in &keys[..keys.len() - 1] {
                prop_assert_eq!(x_at(&looped, k.at.0), Some(k.props[0].unwrap() as f32));
            }
            prop_assert_eq!(looped.sample(kn), hold.sample(from));
        }
        prop_assert_eq!(hold.sample(k0 - 1), None);
    }
}

/// Keys whose times may repeat.
fn keys_with_repeats() -> impl Strategy<Value = Vec<KeyDef>> {
    (
        0..1_000_000_i64,
        prop::collection::vec((0..3_i64, -10_000..10_000_i32), 1..8),
    )
        .prop_map(|(k0, steps)| {
            let mut at = k0;
            steps
                .into_iter()
                .map(|(dt, x)| {
                    at += dt * 100_000;
                    kx(at, x)
                })
                .collect()
        })
}

proptest! {
    /// With repeated times and any loop point from 4 h before 0, every
    /// time samples without panicking: the last key of a shared time shows
    /// at it, and a loop shows the held track at the time wrapped in
    /// `i128`.
    #[test]
    fn repeated_times_and_negative_loops(
        keys in keys_with_repeats(),
        ease in ease(),
        t in any::<i64>(),
        back in 0..=MAX_KEY_US,
    ) {
        let kn = keys[keys.len() - 1].at.0;
        let hold = Track::new(&keys, ease, End::Hold).unwrap();
        for (i, k) in keys.iter().enumerate() {
            let last = keys[i + 1..].iter().all(|n| n.at != k.at);
            if last {
                prop_assert_eq!(x_at(&hold, k.at.0), Some(k.props[0].unwrap() as f32));
            }
        }
        let from = (kn - 1).min(-back);
        let looped = Track::new(&keys, ease, End::Loop { from: TimeUs(from) }).unwrap();
        let got = looped.sample(t);
        if t > from {
            let (u, f) = (i128::from(t), i128::from(from));
            let folded = i64::try_from(f + (u - f).rem_euclid(i128::from(kn) - f)).unwrap();
            prop_assert_eq!(got, hold.sample(folded));
        } else {
            prop_assert_eq!(got, hold.sample(t));
        }
    }
}
