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
            first: 100_000,
            last: 100_000
        })
    );
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
    assert!(!carried.has_rotation());
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
    assert!(track.has_rotation());
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
    assert_eq!(
        Track::new(&[kx(0, 0), kx(0, 1)], lin, h),
        Err(TrackError::NotIncreasing { index: 1 })
    );
    assert_eq!(
        Track::new(&[kx(0, 0), kx(10, 1), kx(5, 2)], lin, h),
        Err(TrackError::NotIncreasing { index: 2 })
    );
    assert_eq!(
        Track::new(&[kx(-1, 0)], lin, h),
        Err(TrackError::NegativeStart { at: -1 })
    );
    let three = [kx(100, 0), kx(200, 1), kx(300, 2)];
    for from in [300, 99] {
        assert_eq!(
            Track::new(&three, lin, End::Loop { from: TimeUs(from) }),
            Err(TrackError::LoopOutside {
                from,
                first: 100,
                last: 300
            })
        );
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
