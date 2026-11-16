//! The chart's BGA on a play skin (design.md "Rendering and Skins", BGA):
//! a `bga` destination becomes a BGA element, its keys unbaked and its
//! track, colour, blend and stretch kept, the cropping stretches
//! included, its filter the BGA's own; selection, result and decision
//! skins do not look the object up.

mod common;

use std::sync::Arc;

use bms_model::{BgaEvent, BgaImage, BgaImageId, ChartBga, TimeUs};
use common::play::{Drawn, chart, round};
use common::{import_body, kinds};
use skin_beatoraja::{NoteKind, Object};
use skin_model::bga::{BgaCell, BgaEntry, BgaKey, BgaTable};
use skin_model::{Blend, Filter, FrameUv, Quad};

const KEY: BgaKey = BgaKey {
    renderer: 1,
    array: 2,
};

/// A 512×256 picture on page 3.
const CELL: BgaCell = BgaCell {
    uv: FrameUv {
        uv: [0.0, 0.0, 0.125, 0.0625],
        layer: 3,
    },
    native: [512, 256],
};

/// The quads of `skin`'s frame at 100 ms over a chart whose base shows
/// picture 0 from 0 ms.
fn drawn(skin: skin_model::Skin) -> Vec<Quad> {
    let mut c = chart(0);
    c.bga = Some(
        ChartBga::new(
            vec![BgaImage {
                id: 1,
                name: "a.png".into(),
            }],
            vec![BgaEvent {
                time: TimeUs(0),
                image: Some(BgaImageId(0)),
            }],
            Vec::new(),
            Vec::new(),
        )
        .unwrap(),
    );
    let mut d = Drawn::over(skin, &c);
    d.play.set_bga_images(Arc::new(
        BgaTable::new(vec![BgaEntry::Picture(CELL)]).keyed(KEY),
    ));
    d.play.set_bga_live(Some(KEY));
    d.frame(100, &[])
}

/// The model rect (y down, 1080 high) of a reference rect (y up).
fn model([x, y, w, h]: [i32; 4]) -> [f32; 4] {
    [x as f32, (1_080 - y - h) as f32, w as f32, h as f32]
}

#[test]
fn bga_destination_imports_with_track_blend_colour_stretch() {
    // 300×300 at (100, 200): stretch 3 crops the 512×256 picture to its
    // middle 256×256; stretch 1 and an unset stretch keep it inside,
    // 300×150 centred; stretch 0 fills.
    let rect = model([100, 200, 300, 300]);
    let [x, y, w, _] = rect;
    for (mode, want, uv) in [
        ("3", rect, [0.03125, 0.0, 0.09375, 0.0625]),
        ("1", [x, y + 75.0, w, 150.0], CELL.uv.uv),
        ("-1", [x, y + 75.0, w, 150.0], CELL.uv.uv),
        ("0", rect, CELL.uv.uv),
    ] {
        let l = import_body(&format!(
            r#""bga": {{"id": "b"}},
            "destination": [{{"id": "b", "blend": 2, "filter": 0, "stretch": {mode},
                "dst": [{{"time": 0, "x": 100, "y": 200, "w": 300, "h": 300, "a": 200, "r": 64, "g": 128, "b": 192}},
                        {{"time": 1000, "x": 100, "y": 200, "w": 300, "h": 300, "a": 200, "r": 64, "g": 128, "b": 192}}]}}]"#
        ));
        let r = l.report();
        assert!(r.notes.is_empty(), "{mode}: {:?}", kinds(r));
        assert_eq!(r.usage.images, 0, "the BGA charges no image");
        let skin = l.ok().skin.clone();
        let quads = drawn(skin);
        let [q] = quads.as_slice() else {
            panic!("{mode}: {quads:?}");
        };
        assert_eq!(round(q.rect), round(want), "mode {mode}");
        assert_eq!(q.uv, uv, "mode {mode}");
        assert_eq!(q.layer, 3);
        // The base's own filter, whatever the destination's.
        assert_eq!(q.filter, Filter::Bga);
        assert_eq!(
            q.color,
            [64.0 / 255.0, 128.0 / 255.0, 192.0 / 255.0, 200.0 / 255.0]
        );
    }
    let l = import_body(
        r#""bga": {"id": "b"},
        "destination": [{"id": "b", "blend": 2, "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}]"#,
    );
    let skin = l.ok().skin.clone();
    let mut d = Drawn::new(skin, 0);
    d.frame(0, &[]);
    assert_eq!(d.batch.runs()[0].key.blend, Blend::Add);
}

/// The object is looked up on play only: elsewhere its id names nothing.
#[test]
fn the_bga_object_is_play_only() {
    let undefined = NoteKind::UndefinedObject;
    let ours = r#"{"id": "b", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}, "#;
    let selection = common::selection::minimal(r#""bga": {"id": "b"},"#).replacen(
        "\"destination\": [",
        &format!("\"destination\": [{ours}"),
        1,
    );
    let s = common::selection::folder(&selection);
    let i = common::selection::import_selection(s.path()).unwrap();
    assert!(i.report.notes.iter().any(|n| n.kind == undefined));
    let d = common::decision::folder(
        r#""bga": {"id": "b"},
        "destination": [{"id": "b", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}]"#,
    );
    let i = common::decision::import_decision(d.path()).unwrap();
    assert!(i.report.notes.iter().any(|n| n.kind == undefined));
    let _ = Object::Bga;
}
