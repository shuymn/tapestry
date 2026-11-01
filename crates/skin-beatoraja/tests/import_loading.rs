//! Imported skins through the loading runtime: untimed destinations and
//! image cycles run on `screen` from the loading state's origin, and the
//! gauge-type conditions, image index 40 and the gauge's empty frame
//! follow the loading frame's gauge descriptor (stage3.md R2, R3, D3, D4).

mod common;

use common::import_body;
use common::play::{Loader, round};
use skin_beatoraja::{DeviationId, NoteKind};
use skin_model::{GaugeDesc, GaugeLook, LoadingGauge, Quad, UvTable, timer};

/// A 10×10 still `i` and a cycled `c` (two 32×64 frames over 100 ms).
const IMAGES: &str = r#""image": [
    {"id": "i", "src": "a", "w": 64, "h": 64},
    {"id": "c", "src": "a", "w": 64, "h": 64, "divx": 2, "cycle": 100}
]"#;

/// Each quad's frame: its index in the uv table.
fn frames(uvs: &UvTable, quads: &[Quad]) -> Vec<usize> {
    quads
        .iter()
        .map(|q| {
            uvs.frames()
                .iter()
                .position(|f| f.uv == q.uv && f.layer == q.layer)
                .expect("a frame of the sheet")
        })
        .collect()
}

/// The rect of the quad `w` wide, if one drew.
fn of_width(rects: &[[i32; 4]], w: i32) -> Option<[i32; 4]> {
    rects.iter().copied().find(|r| r[2] == w)
}

/// Untimed destinations and image cycles draw while loading, from the
/// loading state's origin, while those on an explicit `ready` (40) stay
/// absent; a play runs them again from `ready`'s start, the countdown's
/// restart.
#[test]
fn untimed_elements_run_from_the_loading_origin() {
    // Told apart by width: 10 untimed, 11 on ready, 12 moving untimed,
    // 13 an untimed cycle, 14 a cycle on ready.
    let body = format!(
        r#"{IMAGES}, "destination": [
            {{"id": "i", "dst": [{{"x": 0, "y": 0, "w": 10, "h": 10}}]}},
            {{"id": "i", "timer": 40, "dst": [{{"x": 0, "y": 0, "w": 11, "h": 10}}]}},
            {{"id": "i", "dst": [{{"time": 0, "x": 0, "y": 0, "w": 12, "h": 10}},
                                 {{"time": 1000, "x": 1000}}]}},
            {{"id": "c", "dst": [{{"x": 0, "y": 0, "w": 13, "h": 10}}]}},
            {{"id": "c", "timer": 40, "dst": [{{"x": 0, "y": 0, "w": 14, "h": 10}}]}}
        ]"#
    );
    let mut l = import_body(&body).loading();
    let rects = l.rects(500);
    assert!(of_width(&rects, 10).is_some());
    assert_eq!(of_width(&rects, 11), None);
    assert_eq!(of_width(&rects, 14), None);
    assert_eq!(of_width(&rects, 12).map(|r| r[0]), Some(500));
    // The cycle advances on the loading clock: frame 0, then 1 50 ms on.
    let cycle = |l: &mut Loader, ms: i64| {
        let quads = l.frame(ms);
        let at: Vec<_> = quads
            .iter()
            .filter(|q| round(q.rect)[2] == 13)
            .copied()
            .collect();
        frames(&l.uvs, &at)
    };
    let (first, later) = (cycle(&mut l, 0), cycle(&mut l, 50));
    assert_eq!(first.len(), 1);
    assert_ne!(first, later);
    assert_eq!(l.loading.timer(timer::SCREEN), Some(bms_model::TimeUs(0)));

    // In the play the same element runs from `ready`'s start.
    let mut d = import_body(&body).drawn();
    d.frame(0, &[]);
    let ready = d.play.timer(timer::READY).expect("ready has started").0 / 1_000;
    assert_eq!(d.play.timer(timer::SCREEN), d.play.timer(timer::READY));
    let rects = d.rects(ready + 500);
    assert_eq!(of_width(&rects, 12).map(|r| r[0]), Some(500));
}

/// The descriptor of a preset's gauge 0 with `look`, its border at 80 %.
fn descriptor(look: GaugeLook) -> Option<LoadingGauge> {
    let desc = GaugeDesc {
        start: rules::GaugeValue(20 * rules::GaugeValue::PERCENT),
        max: rules::GaugeValue::FULL,
        look,
        border: rules::GaugeValue(80 * rules::GaugeValue::PERCENT),
    };
    Some(LoadingGauge::of(0, &desc))
}

const D4: NoteKind = NoteKind::Deviation(DeviationId::D4);

/// The gauge-type conditions, negated or not, follow the descriptor's
/// look while loading, and draw nothing without one.
#[test]
fn gauge_type_conditions_follow_the_loading_descriptor() {
    // Shown under a normal, then a hard look.
    for (op, normal, hard) in [
        ("42", true, false),
        ("-42", false, true),
        ("43", false, true),
        (r#""!gauge_hard""#, true, false),
        (r#""gauge_ex""#, false, false),
        ("-1046", true, true),
    ] {
        let loaded = import_body(&format!(
            r#"{IMAGES}, "destination": [{{"id": "i", "op": [{op}],
                                          "dst": [{{"w": 10, "h": 10}}]}}]"#
        ));
        assert_eq!(common::kinds(loaded.report()), [D4], "{op}");
        let mut l = loaded.loading();
        l.gauge = descriptor(GaugeLook::Normal);
        assert_eq!(l.frame(0).len(), usize::from(normal), "{op}");
        l.gauge = descriptor(GaugeLook::Hard);
        assert_eq!(l.frame(16).len(), usize::from(hard), "{op}");
        l.gauge = None;
        assert_eq!(l.frame(32).len(), 0, "{op}");
    }
}

/// Image index 40 selects the run of the descriptor's look while
/// loading, and nothing without one.
#[test]
fn image_index_40_follows_the_loading_descriptor() {
    // Five runs of two frames, cycled over 300 ms, untimed.
    let loaded = import_body(
        r#""image": [{"id": "g", "src": "a", "w": 50, "h": 20, "divx": 5, "divy": 2,
                      "cycle": 300, "len": 5, "ref": 40}],
           "destination": [{"id": "g", "dst": [{"w": 10, "h": 10}]}]"#,
    );
    assert_eq!(common::kinds(loaded.report()), [D4]);
    let mut l = loaded.loading();
    let shown = |l: &mut Loader, gauge, ms| {
        l.gauge = gauge;
        let quads = l.frame(ms);
        frames(&l.uvs, &quads)
    };
    assert_eq!(shown(&mut l, descriptor(GaugeLook::Normal), 0), [4]);
    assert_eq!(shown(&mut l, descriptor(GaugeLook::Hard), 16), [6]);
    assert_eq!(shown(&mut l, descriptor(GaugeLook::Easy), 32), [2]);
    assert_eq!(shown(&mut l, None, 48), []);
}

/// The gauge draws its empty frame from the descriptor while loading:
/// every segment unlit, those below the border and those at or above it
/// with their own images; without a descriptor nothing.
#[test]
fn the_gauge_draws_its_empty_frame_while_loading() {
    let nodes: Vec<String> = (0..4)
        .map(|k| {
            format!(
                r#"{{"id": "n{k}", "src": "a", "x": {}, "y": 0, "w": 8, "h": 8}}"#,
                8 * k
            )
        })
        .collect();
    let body = format!(
        r#""image": [{}], "gauge": {{"id": "g", "nodes": ["n0", "n1", "n2", "n3"], "parts": 10}},
           "destination": [{{"id": "g", "dst": [{{"x": 100, "y": 500, "w": 100, "h": 10}}]}}]"#,
        nodes.join(", ")
    );
    let mut l = import_body(&body).loading();
    let quads = l.frame(100);
    assert_eq!(quads.len(), 10);
    let uvs = frames(&l.uvs, &quads);
    // Border 80 % of 10 segments: segment i (from 1) is below when
    // i·100 < 80·10.
    let below = uvs.iter().filter(|&&u| u == uvs[0]).count();
    assert_eq!(below, 7);
    assert!(uvs[7..].iter().all(|&u| u == uvs[9] && u != uvs[0]));
    // The frame a play draws of an empty gauge.
    let mut d = import_body(&body).drawn();
    d.gauges[0].value = rules::GaugeValue(0);
    let empty = d.frame(1_000, &[]);
    assert_eq!(frames(&d.uvs, &empty), uvs);
    l.gauge = None;
    assert!(l.frame(116).is_empty());
}
