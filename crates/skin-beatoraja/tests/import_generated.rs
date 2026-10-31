//! Generated skins over the imported grammar: every imported skin
//! prepares and builds, and every model refusal names a location.

mod common;

use common::play::Drawn;
use common::{font, png, skin};
use proptest::prelude::*;
use skin_beatoraja::{Config, ImportError, import, read_header};

fn key() -> impl Strategy<Value = String> {
    (
        0..300_i32,
        -50..2_000_i32,
        -50..1_100_i32,
        -5..200_i32,
        -5..200_i32,
        prop::option::of(-10..300_i32),
        prop::option::of(-360..360_i32),
    )
        .prop_map(|(t, x, y, w, h, a, angle)| {
            let mut s = format!(r#"{{"time": {t}, "x": {x}, "y": {y}, "w": {w}, "h": {h}"#);
            if let Some(a) = a {
                s += &format!(r#", "a": {a}"#);
            }
            if let Some(r) = angle {
                s += &format!(r#", "angle": {r}"#);
            }
            s + "}"
        })
}

fn destination() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec!["img", "cyc", "ex", "pg", "big", "txt", "fnt", "none"]),
        prop::collection::vec(key(), 1..5),
        prop::sample::select(vec![-1, 0, 50, 100, 500]),
        prop::sample::select(vec!["", r#""timer": 0,"#, r#""timer": 2,"#, r#""timer": 41,"#,
                                  r#""timer": 46,"#, r#""timer": 348,"#, r#""timer": 10000,"#]),
        prop::collection::vec(prop::sample::select(vec![0, 7_777, 241, 232, -232, 202, 900]), 0..3),
        prop::sample::select(vec![0, 2, 3, 4]),
        prop::sample::select(vec![-1, 1, 4, 5, 9]),
        0..12_i32,
        prop::bool::ANY,
    )
        .prop_map(|(id, keys, l, timer, ops, blend, stretch, center, offset)| {
            let ops = ops.iter().map(i32::to_string).collect::<Vec<_>>().join(",");
            let offset = if offset { r#""offset": 40,"# } else { "" };
            format!(
                r#"{{"id": "{id}", {timer} {offset} "loop": {l}, "op": [{ops}], "blend": {blend},
                    "stretch": {stretch}, "center": {center}, "dst": [{}]}}"#,
                keys.join(",")
            )
        })
}

fn skin_json(dsts: &[String]) -> String {
    format!(
        r#"{{"type": 0, "w": 1280, "h": 720,
            "property": [{{"name": "p", "item": [{{"name": "x", "op": 900}}]}}],
            "offset": [{{"name": "o", "id": 40}}],
            "source": [{{"id": "a", "path": "a.png"}}],
            "font": [{{"id": "f", "path": "f.ttf"}}, {{"id": "b", "path": "b.fnt"}}],
            "customTimers": [{{"id": 10000, "timer": 41}}],
            "image": [
                {{"id": "img", "src": "a", "x": 0, "y": 0, "w": 64, "h": 64}},
                {{"id": "cyc", "src": "a", "w": 64, "h": 32, "divx": 2, "timer": 46, "cycle": 50}}
            ],
            "value": [
                {{"id": "ex", "src": "a", "w": 60, "h": 6, "divx": 10, "digit": 4, "ref": 71}},
                {{"id": "pg", "src": "a", "w": 60, "h": 6, "divx": 10, "digit": 3, "ref": 110,
                  "align": 2, "space": 3}},
                {{"id": "big", "src": "a", "w": 64, "h": 2, "divx": 32, "digit": 13, "ref": 420}}
            ],
            "text": [
                {{"id": "txt", "font": "f", "size": 20, "ref": 10, "align": 1}},
                {{"id": "fnt", "font": "b", "size": 20, "constantText": "x\u0007y", "overflow": 2}}
            ],
            "destination": [{}]}}"#,
        dsts.join(",")
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn imports_prepare_and_build(dsts in prop::collection::vec(destination(), 1..8)) {
        let dir = skin(&[("s.json", &skin_json(&dsts))]);
        png(dir.path(), "a.png", 64, 64);
        font(dir.path(), "f.ttf");
        let (mut def, header) = read_header(dir.path(), "s.json").unwrap();
        let mut config = Config::defaults(&header);
        config.offsets.insert(40, skin_beatoraja::OffsetValue { x: 3, w: -5, ..Default::default() });
        match import(&mut def, &header, &config) {
            Ok(imported) => {
                let capacity = imported.skin.quad_capacity() as usize;
                let mut drawn = Drawn::new(imported.skin, 100);
                drawn.attach_lines(1.0, 1.0);
                for t in [0, 30, 100, 250, 600] {
                    let q = drawn.frame(t, &[common::play::judged(t, rules::Judgment::Perfect, 1)]);
                    prop_assert!(q.len() <= capacity);
                }
            }
            Err(ImportError::Model { at, source }) => {
                prop_assert!(at.is_some(), "{source:?}");
            }
            Err(_) => {}
        }
    }
}
