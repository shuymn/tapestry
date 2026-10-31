//! Tracks: inheritance and colour clamping, then acceptance, each
//! accepted track checked against a reference sampler written from the
//! source's description (`SkinObject.java:347-411, 545-575` at `0.8.9`)
//! at every millisecond.

mod common;

use common::play::round;
use common::{has, import_body};
use skin_beatoraja::{NoteKind, TrackWhy};

/// A key: time in ms and the rect, y up.
type Key = (i64, [i32; 4]);

/// The source's draw at `t` ms after the timer: the rect drawn, or
/// `None`. Float arithmetic as the source does it.
#[expect(clippy::cast_precision_loss, reason = "the source samples in f32")]
fn reference(keys: &[Key], acc: i32, l: i64, mut t: i64) -> Option<[f32; 4]> {
    let k0 = keys.first()?.0;
    let kn = keys.last()?.0;
    if l == -1 {
        if t > kn {
            t = -1;
        }
    } else if kn > 0 && t > l {
        // Java's `%` keeps the dividend's sign, as Rust's does.
        t = if kn == l { l } else { (t - l) % (kn - l) + l };
    }
    if k0 > t {
        return None;
    }
    let f = |k: &Key| k.1.map(|v| v as f32);
    let (index, rate) = 'rate: {
        if t == kn {
            break 'rate (keys.len() - 1, 0.0_f32);
        }
        let mut time2 = kn;
        for i in (0..keys.len() - 1).rev() {
            let time1 = keys[i].0;
            if time1 <= t && time2 > t {
                let mut rate = (t - time1) as f32 / (time2 - time1) as f32;
                match acc {
                    1 => rate *= rate,
                    2 => rate = 1.0 - (rate - 1.0) * (rate - 1.0),
                    _ => {}
                }
                break 'rate (i, rate);
            }
            time2 = time1;
        }
        (0, 0.0)
    };
    let r1 = f(&keys[index]);
    if rate == 0.0 || acc == 3 {
        return Some(r1);
    }
    let r2 = f(&keys[index + 1]);
    Some([0, 1, 2, 3].map(|i| r1[i] + (r2[i] - r1[i]) * rate))
}

/// The y-down rect of a y-up source rect.
#[expect(clippy::cast_possible_truncation, reason = "test pixels are small")]
fn reflect([x, y, w, h]: [f32; 4]) -> [i32; 4] {
    let top = 1_080.0 - (y + h);
    [
        x.round() as i32,
        top.round() as i32,
        w.round() as i32,
        h.round() as i32,
    ]
}

fn keys_json(keys: &[Key], acc: i32) -> String {
    keys.iter()
        .map(|&(t, [x, y, w, h])| {
            format!(r#"{{"time": {t}, "x": {x}, "y": {y}, "w": {w}, "h": {h}, "acc": {acc}}}"#)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn body(keys: &[Key], acc: i32, l: i64) -> String {
    format!(
        r#""image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
           "destination": [{{"id": "i", "timer": 41, "loop": {l}, "dst": [{}]}}]"#,
        keys_json(keys, acc)
    )
}

/// Imports the track and checks it against [`reference`] at every ms of
/// `[0, 3·max(kn, |L|) + 10]`, within the rounding of D1.
fn agrees(keys: &[Key], acc: i32, l: i64) {
    let loaded = import_body(&body(keys, acc, l));
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    let mut drawn = loaded.drawn();
    let kn = keys.last().unwrap().0;
    for t in 0..=3 * kn.max(l.abs()) + 10 {
        let want = reference(keys, acc, l, t).map(reflect);
        let got = drawn.frame(t, &[]).first().map(|q| round(q.rect));
        match (want, got) {
            (None, None) => {}
            (Some(w), Some(g)) => assert!(
                w.iter().zip(g).all(|(a, b)| (a - b).abs() <= 1),
                "{keys:?} acc {acc} loop {l} at {t}: {w:?} vs {g:?}"
            ),
            _ => panic!("{keys:?} acc {acc} loop {l} at {t}: {want:?} vs {got:?}"),
        }
    }
}

const R: [i32; 4] = [10, 20, 30, 40];

#[test]
fn colour_is_clamped_per_key() {
    let b = r#""image": [{"id": "i", "src": "a", "w": 64, "h": 64}],
               "destination": [{"id": "i", "dst": [{"w": 1, "h": 1, "a": 300, "r": -5, "g": 128}]}]"#;
    let q = import_body(b).quads(0);
    // RGBA: r −5 → 0, g 128, b 255, a 300 → 255.
    let [r, g, b, a] = q[0].color;
    assert_eq!([r, b, a], [0.0, 1.0, 1.0]);
    assert!((g - 128.0 / 255.0).abs() < 1e-6);
}

#[test]
fn single_keys_follow_the_end_table() {
    for l in [-1, -5, 0, 5, 10, 20] {
        agrees(&[(0, R)], 0, l);
    }
    // At 10: hidden after it, held, or never drawn (`Ignored`).
    agrees(&[(10, R)], 0, -1);
    agrees(&[(10, R)], 0, 10);
    agrees(&[(10, R)], 0, 20);
    for l in [-5, 0, 5] {
        let loaded = import_body(&body(&[(10, R)], 0, l));
        assert!(
            has(loaded.report(), NoteKind::Track(TrackWhy::NeverDrawn)),
            "{l}"
        );
        assert_eq!(loaded.ok().report.usage.images, 0);
    }
}

#[test]
fn accepted_tracks_match_the_source() {
    let three = [(0, R), (100, [110, 20, 60, 40]), (200, [60, 50, 30, 10])];
    for acc in 0..=3 {
        for l in [-1, 0, 50, 100, 200] {
            agrees(&three, acc, l);
        }
    }
    let late = [(10, R), (100, [110, 20, 60, 40])];
    for l in [-1, 10, 40, 100] {
        agrees(&late, 0, l);
    }
    // Past the end with the first and last keys equal: held.
    let round_trip = [(0, R), (50, [110, 20, 60, 40]), (100, R)];
    agrees(&round_trip, 0, 150);
}

#[test]
fn tracks_the_model_cannot_follow_are_dropped() {
    let x = |x| [x, 0, 10, 10];
    for (keys, l, why) in [
        (
            vec![(0, x(0)), (10, x(0)), (10, x(100))],
            0,
            TrackWhy::EqualTime,
        ),
        (vec![(-10, x(0)), (10, x(20))], -1, TrackWhy::NegativeStart),
        (vec![(100, x(0)), (200, x(20))], 0, TrackWhy::Gap),
        (vec![(0, x(0)), (100, x(20))], -5, TrackWhy::NegativeLoop),
        (
            vec![(0, x(0)), (100, x(20))],
            150,
            TrackWhy::FallbackToFirst,
        ),
    ] {
        let loaded = import_body(&body(&keys, 0, l));
        assert!(
            has(loaded.report(), NoteKind::Track(why)),
            "{why:?}: {:?}",
            loaded.report()
        );
        assert!(loaded.quads(0).is_empty());
    }
    // Equal-time keys of identical values keep one.
    agrees(&[(0, x(0)), (10, x(5))], 0, 0);
    let dup = body(&[(0, x(0)), (10, x(5)), (10, x(5))], 0, 0);
    assert!(import_body(&dup).report().notes.is_empty());
}

#[test]
fn a_track_holds_at_most_256_keys() {
    for (n, dropped) in [(256, false), (257, true)] {
        let keys: Vec<Key> = (0..n)
            .map(|i| (i, [i32::try_from(i).unwrap(), 0, 1, 1]))
            .collect();
        let loaded = import_body(&body(&keys, 0, 0));
        assert_eq!(
            has(loaded.report(), NoteKind::Track(TrackWhy::Limits)),
            dropped
        );
        assert_eq!(loaded.report().usage.keys, u32::try_from(n).unwrap());
        assert_eq!(loaded.quads(0).len(), usize::from(!dropped));
    }
}
