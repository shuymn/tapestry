//! Tracks: inheritance and colour clamping, then acceptance, each
//! accepted track checked against a reference sampler written from the
//! source's description (`SkinObject.java:347-411, 545-575` at `0.8.9`)
//! at every millisecond.

mod common;

use common::play::round;
use common::{has, import_body};
use skin_beatoraja::{NoteKind, Severity, TrackWhy};

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
    let body = r#""image": [{"id": "i", "src": "a", "w": 64, "h": 64}],
               "destination": [{"id": "i", "dst": [{"w": 1, "h": 1, "a": 300, "r": -5, "g": 128}]}]"#;
    let loaded = import_body(body);
    // One line, a fallback located at the clamped key.
    let notes = &loaded.report().notes;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].kind, NoteKind::ColourClamped);
    assert_eq!(notes[0].severity, Severity::Fallback);
    let at = notes[0].at.unwrap();
    // The key's `{` on the destination's line.
    assert_eq!((at.line, at.col), (3, 52), "{at:?}");
    let q = loaded.quads(0);
    // RGBA: r −5 → 0, g 128, b 255, a 300 → 255.
    let [r, g, b, a] = q[0].color;
    assert_eq!([r, b, a], [0.0, 1.0, 1.0]);
    assert!((g - 128.0 / 255.0).abs() < 1e-6);
    // In range, no line.
    let fine = import_body(&body.replace("300", "255").replace("-5", "0"));
    assert!(fine.report().notes.is_empty());
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

/// A loop point before the first key, as the source samples it
/// (`SkinObject.java:358-374` at `0.8.9`): the time wraps first, then a
/// wrapped time before the first key draws nothing, a gap each period.
/// The bundled result skin's shape, keys at 5 and 10 s with the loop at
/// 0, draws at 5 000 and 15 000 ms and hides at 10 000 and 20 000 ms;
/// every ms agrees with the source's sampler for it and for loops at 0
/// and 50 before keys at 100 and 200, under every acceleration.
#[test]
fn a_loop_before_the_first_key_leaves_a_gap_as_the_source() {
    let late = [(5_000, R), (10_000, [110, 20, 60, 40])];
    let loaded = import_body(&body(&late, 0, 0));
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    let mut drawn = loaded.drawn();
    for (t, shown) in [
        (4_999, false),
        (5_000, true),
        (10_000, false),
        (15_000, true),
        (20_000, false),
    ] {
        assert_eq!(!drawn.frame(t, &[]).is_empty(), shown, "{t} ms");
    }
    agrees(&late, 0, 0);
    let pair = [(100, R), (200, [110, 20, 60, 40])];
    for acc in 0..=3 {
        for l in [0, 50] {
            agrees(&pair, acc, l);
        }
    }
}

/// The same shape on a result skin's type-2 judge graph, turning as it
/// moves: keys at 5 and 10 s, loop 0, angle 10° to 55°. At every ms of
/// three periods the graph is drawn exactly when the source's sampler
/// draws, its region at the source's rect within the rounding of D1. The
/// angle is the source's interpolated value in fractional degrees, where
/// the source truncates it to whole ones (`SkinObject.java:531`), the
/// intended deviation of design.md "Custom Skin Format" (angles
/// interpolate numerically like any value), so the drawn angle is within
/// a degree above the source's whole degrees.
#[test]
fn a_result_graph_loop_before_its_first_key_leaves_the_same_gap() {
    use bms_model::TimeUs;
    use common::result::{Res, folder, import_result, ready};
    use skin_model::{BestView, Filter, ResultView, Timelines};

    let keys: [(i64, [i32; 4], i32); 2] = [
        (5_000, [100, 400, 520, 82], 10),
        (10_000, [300, 300, 400, 120], 55),
    ];
    let dst = keys
        .iter()
        .map(|&(t, [x, y, w, h], angle)| {
            format!(r#"{{"time": {t}, "x": {x}, "y": {y}, "w": {w}, "h": {h}, "angle": {angle}}}"#)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let d = folder(&format!(
        r#""value": [{{"id": "ex", "src": "s", "x": 0, "y": 32, "w": 40, "h": 8, "divx": 10, "digit": 4, "ref": 71}}],
           "judgegraph": [{{"id": "g", "type": 2}}],
           "destination": [
             {{"id": "ex", "dst": [{{"x": 0, "y": 100, "w": 4, "h": 8}}]}},
             {{"id": "dot", "op": [90], "dst": [{{"x": 0, "y": 300, "w": 8, "h": 8}}]}},
             {{"id": "g", "loop": 0, "dst": [{dst}]}}]"#
    ));
    let imported = import_result(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(
        !imported
            .report
            .notes
            .iter()
            .any(|n| matches!(n.kind, NoteKind::Track(_))),
        "{:?}",
        imported.report
    );
    let mut r = Res::new(imported.skin);
    let timelines = Timelines::new(TimeUs(1_000_000), vec![[1; 9]], vec![9], None).unwrap();
    r.view(&ResultView {
        timelines: Some(timelines),
        ..ready(1_000, true, BestView::Unavailable)
    });
    let rects: Vec<Key> = keys.iter().map(|&(t, rect, _)| (t, rect)).collect();
    let angles: Vec<Key> = keys.iter().map(|&(t, _, a)| (t, [a, 0, 0, 0])).collect();
    for t in 0..=30_010 {
        // The graph's background quads (the essentials lie left of x 50).
        let back: Vec<_> = r
            .frame(t)
            .into_iter()
            .filter(|q| q.filter == Filter::Nearest && q.rect[0] >= 50.0)
            .collect();
        let Some(want) = reference(&rects, 0, 0, t) else {
            assert!(back.is_empty(), "{t} ms: {back:?}");
            continue;
        };
        let bounds = back.iter().fold(
            [f32::MAX, f32::MAX, f32::MIN, f32::MIN],
            |[x0, y0, x1, y1], q| {
                let [x, y, w, h] = q.rect;
                [x0.min(x), y0.min(y), x1.max(x + w), y1.max(y + h)]
            },
        );
        let got = round([
            bounds[0],
            bounds[1],
            bounds[2] - bounds[0],
            bounds[3] - bounds[1],
        ]);
        let want = reflect(want);
        assert!(
            want.iter().zip(got).all(|(a, b)| (a - b).abs() <= 1),
            "{t} ms: {want:?} vs {got:?}"
        );
        let source = reference(&angles, 0, 0, t).unwrap()[0];
        for q in &back {
            // The quad turns clockwise on the y-down canvas, the source
            // counterclockwise on its y-up one: the same turn on screen.
            let drawn = (360.0 - q.angle.to_degrees()) % 360.0;
            assert!((drawn - source).abs() < 1e-3, "{t} ms: {drawn} vs {source}");
            assert!((-1e-3..1.0).contains(&(drawn - source.trunc())), "{t} ms");
        }
    }
}

#[test]
fn tracks_the_model_cannot_follow_are_dropped() {
    let x = |x| [x, 0, 10, 10];
    for (keys, l, why) in [
        (vec![(-10, x(0)), (10, x(20))], -1, TrackWhy::NegativeStart),
        (
            vec![(0, x(0)), (100, x(20))],
            150,
            TrackWhy::FallbackToFirst,
        ),
        // Every key at 0 with different values: the first shows after 0.
        (vec![(0, x(0)), (0, x(20))], 0, TrackWhy::FallbackToFirst),
        // A loop point before the earliest the model loops from (4 h).
        (vec![(0, x(0)), (100, x(20))], -14_400_001, TrackWhy::Limits),
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

/// Keys sharing a time, as the source keeps and samples them
/// (`SkinObject.java:240-255, 545-575` at `0.8.9`): the last of them
/// draws at that time, the track before it moves toward the first, and
/// the keys between never draw. Every ms under every acceleration agrees
/// with the reference port, whose values a run of the source's sampler
/// gave: 99 ms → x 49.5, 100 ms → x 200, 101 ms → x 201, 199 ms → x 299.
#[test]
fn keys_at_one_time_draw_as_the_source() {
    let x = |x| [x, 0, 10, 10];
    let jump = [(0, x(0)), (100, x(50)), (100, x(200)), (200, x(300))];
    let three = [
        (0, x(0)),
        (100, x(50)),
        (100, x(120)),
        (100, x(200)),
        (200, x(300)),
    ];
    let at_zero = [(0, x(0)), (0, x(40)), (100, x(140))];
    let end_group = [(0, x(0)), (200, x(100)), (200, x(30))];
    for acc in 0..=3 {
        for l in [-1, 0, 50, 100, 200] {
            agrees(&jump, acc, l);
            agrees(&three, acc, l);
            agrees(&end_group, acc, l);
        }
        for l in [-1, 0, 50, 100] {
            agrees(&at_zero, acc, l);
        }
    }
    // The source's values, y up as authored.
    let loaded = import_body(&body(&jump, 0, 200));
    let mut drawn = loaded.drawn();
    for (t, want) in [(99, 49.5), (100, 200.0), (101, 201.0), (199, 299.0)] {
        let q = drawn.frame(t, &[]);
        assert!(
            (q[0].rect[0] - want).abs() < 1e-3,
            "{t} ms: {:?}",
            q[0].rect
        );
    }
    // Step and ease-in just before the shared time.
    for (acc, want) in [(3, 0.0), (1, 49.005)] {
        let loaded = import_body(&body(&jump, acc, 200));
        let mut drawn = loaded.drawn();
        let q = drawn.frame(99, &[]);
        assert!(
            (q[0].rect[0] - want).abs() < 1e-3,
            "acc {acc}: {:?}",
            q[0].rect
        );
        assert!((drawn.frame(100, &[])[0].rect[0] - 200.0).abs() < 1e-3);
    }
    // The end group under hide: its last key at 200, nothing after.
    let loaded = import_body(&body(&end_group, 0, -1));
    let mut drawn = loaded.drawn();
    assert!((drawn.frame(200, &[])[0].rect[0] - 30.0).abs() < 1e-3);
    assert!(drawn.frame(201, &[]).is_empty());
    // Two keys at 100 under loop 0: never drawn.
    let loaded = import_body(&body(&[(100, x(0)), (100, x(50))], 0, 0));
    assert!(has(loaded.report(), NoteKind::Track(TrackWhy::NeverDrawn)));
    assert_eq!(loaded.ok().report.usage.images, 0);
}

/// A loop point below −1 wraps into negative times, as the source
/// (`SkinObject.java:358-374` at `0.8.9`): `t' = (t − L) mod (kn − L) + L`,
/// nothing drawn where `t'` falls before the first key. Keys at 100 and
/// 200 ms with the loop at −50 draw over [100, 200) and again from 350 ms.
#[test]
fn a_loop_below_minus_one_wraps_as_the_source() {
    let pair = [(100, R), (200, [110, 20, 60, 40])];
    for acc in 0..=3 {
        for l in [-2, -50, -300] {
            agrees(&pair, acc, l);
        }
    }
    let loaded = import_body(&body(&pair, 0, -50));
    let mut drawn = loaded.drawn();
    for (t, shown) in [
        (99, false),
        (100, true),
        (199, true),
        (200, false),
        (349, false),
        (350, true),
        (449, true),
        (450, false),
    ] {
        assert_eq!(!drawn.frame(t, &[]).is_empty(), shown, "{t} ms");
    }
    // The earliest loop point the model keeps: 4 h before 0.
    let loaded = import_body(&body(&pair, 0, -14_400_000));
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    let mut drawn = loaded.drawn();
    assert!(!drawn.frame(150, &[]).is_empty());
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
