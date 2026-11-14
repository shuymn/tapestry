//! The play's progress a play skin reads (docs/stage3.md R3): a `slider`
//! of type 6 (`musicprogress`, `skin/property/FloatPropertyFactory.java:
//! 128-136`), moved by its range times the progress toward its angle
//! (`skin/SkinSlider.java:116-120`), its range scaled to the reference
//! canvas across or down (`skin/json/JsonSkinObjectLoader.java:398-415`),
//! with a D4 note: the progress ends at the last resolved object, where the
//! source's adds 5 s (`play/BMSPlayer.java:66, 198-199`).

mod common;

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use common::play::{Drawn, MS, context};
use common::{Loaded, import_sized, write};
use skin_beatoraja::{Cancel, Config, DeviationId, NoteKind, Object, Severity, read_lua_header};
use skin_model::{Attempt, PlayContext, Skin};

/// One tap at 2 s, the chart's last object: the progress is half at 1 s.
fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(2_000 * MS))
        .build()
        .unwrap()
}

/// Four type-6 sliders on a 1 280 × 720 skin, one per angle (0 up, 1
/// right, 2 down, 3 left), ranges 200 down and 300 across, each drawn at
/// `x 100, y 300, 20 × 10`: in reference pixels `[150, 615, 30, 15]`,
/// ranges `1080 · 200 / 720 = 300` and `1080 · 300 / 720 = 450`.
const SLIDERS: &str = r#""slider": [
    {"id": "u", "src": "a", "w": 8, "h": 8, "type": 6, "angle": 0, "range": 200},
    {"id": "r", "src": "a", "w": 8, "h": 8, "type": 6, "angle": 1, "range": 300},
    {"id": "d", "src": "a", "w": 8, "h": 8, "type": 6, "angle": 2, "range": 200},
    {"id": "l", "src": "a", "w": 8, "h": 8, "type": 6, "angle": 3, "range": 300}],
  "destination": [
    {"id": "u", "dst": [{"x": 100, "y": 300, "w": 20, "h": 10}]},
    {"id": "r", "dst": [{"x": 100, "y": 300, "w": 20, "h": 10}]},
    {"id": "d", "dst": [{"x": 100, "y": 300, "w": 20, "h": 10}]},
    {"id": "l", "dst": [{"x": 100, "y": 300, "w": 20, "h": 10}]}]"#;

fn sliders() -> Loaded {
    import_sized([1_280, 720], SLIDERS, &[])
}

/// The four sliders' rects with the progress at `p` of the whole.
fn moved(p: f64) -> Vec<[i32; 4]> {
    let (down, across) = ((300.0 * p).round() as i32, (450.0 * p).round() as i32);
    vec![
        [150, 615 - down, 30, 15],
        [150 + across, 615, 30, 15],
        [150, 615 + down, 30, 15],
        [150 - across, 615, 30, 15],
    ]
}

/// Each slider is kept with a D4 note and nothing else.
#[test]
fn a_type_six_slider_is_kept_with_a_d4_note() {
    let l = sliders();
    let kinds: Vec<(Severity, NoteKind)> = l
        .report()
        .notes
        .iter()
        .map(|n| (n.severity, n.kind))
        .collect();
    assert_eq!(
        kinds,
        [(Severity::Deviation, NoteKind::Deviation(DeviationId::D4)); 4]
    );
}

/// The sliders move with the play's progress: nothing in READY and at the
/// start, half their range at 1 s, the whole range at 2 s and after.
#[test]
fn the_sliders_follow_the_progress() {
    // The import's folder lives until the atlas is built.
    let l = sliders();
    let skin = l.ok().skin.clone();
    let mut d = Drawn::over(skin.clone(), &chart());
    let ready = PlayContext {
        attempt: Attempt::Full {
            lead_in: TimeUs(1_000 * MS),
        },
        ..context()
    };
    d.play = skin.prepare(&d.chart, &ready).unwrap();
    for (ms, p) in [
        (-500, 0.0),
        (0, 0.0),
        (400, 0.2),
        (1_000, 0.5),
        (2_000, 1.0),
        (9_000, 1.0),
    ] {
        assert_eq!(d.rects(ms), moved(p), "{ms} ms");
    }
}

/// While loading the progress reads 0: every slider draws where it
/// stands.
#[test]
fn loading_draws_the_sliders_unmoved() {
    let mut l = sliders().loading();
    for ms in [0, 2_000] {
        assert_eq!(l.rects(ms), moved(0.0), "{ms} ms");
    }
}

/// A type-6 slider with an angle outside 0 to 3, a negative range, a
/// value property or a numeric reference is dropped with a note; a range
/// past the coordinates refuses.
#[test]
fn other_type_six_sliders_are_dropped() {
    let one = |fields: &str| {
        import_sized(
            [1_920, 1_080],
            &format!(
                r#""slider": [{{"id": "s", "src": "a", "w": 8, "h": 8, "type": 6{fields}}}],
                   "destination": [{{"id": "s", "dst": [{{"x": 0, "y": 0, "w": 8, "h": 8}}]}}]"#
            ),
            &[],
        )
    };
    let field = |field| NoteKind::UnsupportedField { field };
    for (fields, why) in [
        (r#", "angle": 4, "range": 10"#, field("angle")),
        (r#", "angle": -1, "range": 10"#, field("angle")),
        (r#", "angle": 1, "range": -1"#, field("range")),
        (r#", "angle": 1, "range": 10, "value": 71"#, field("value")),
        (
            r#", "angle": 1, "range": 10, "isRefNum": true"#,
            field("isRefNum"),
        ),
    ] {
        let l = one(fields);
        let dropped: Vec<NoteKind> = l
            .report()
            .notes
            .iter()
            .filter(|n| n.severity == Severity::Dropped)
            .map(|n| n.kind)
            .collect();
        assert_eq!(dropped, [why], "{fields}");
        assert!(l.ok().skin.quad_capacity() <= 1, "{fields}");
    }
    let l = one(r#", "angle": 1, "range": 2000000000"#);
    match l.err() {
        skin_beatoraja::ImportError::Range { field, .. } => assert_eq!(field, "range"),
        e => panic!("{e:?}"),
    }
    // Another type stays unsupported.
    let l = import_sized(
        [1_920, 1_080],
        r#""slider": [{"id": "s", "src": "a", "w": 8, "h": 8, "type": 7, "angle": 1, "range": 10}],
           "destination": [{"id": "s", "dst": [{"x": 0, "y": 0, "w": 8, "h": 8}]}]"#,
        &[],
    );
    assert!(l.report().notes.iter().any(|n| n.kind
        == NoteKind::Unsupported {
            object: Object::Slider
        }));
}

/// A range scales exactly, as every length: range 7 of a 1 600×900 skin
/// is `1080 · 7 / 900 = 8.4` reference pixels, so half the progress moves
/// it 4.2.
#[test]
fn a_progress_range_is_exact_at_900() {
    let l = import_sized(
        [1_600, 900],
        r#""slider": [{"id": "r", "src": "a", "w": 8, "h": 8, "type": 6, "angle": 1, "range": 7}],
           "destination": [{"id": "r", "dst": [{"x": 100, "y": 300, "w": 20, "h": 10}]}]"#,
        &[],
    );
    let mut d = Drawn::over(l.ok().skin.clone(), &chart());
    let mut x = |ms| d.frame(ms, &[]).first().unwrap().rect[0];
    let (still, moved) = (x(0), x(1_000));
    assert_eq!(still, 120.0);
    assert!((moved - still - 4.2).abs() < 1e-4, "{still} {moved}");
}

/// A Lua skin's type-6 slider takes the same path.
#[test]
fn a_lua_skins_slider_follows_the_progress() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "s.luaskin",
        b"local skin = { type = 0, name = \"p\", w = 1280, h = 720 }\n\
          if not skin_config then return skin end\n\
          skin.source = { { id = \"a\", path = \"a.png\" } }\n\
          skin.slider = { { id = \"r\", src = \"a\", x = 0, y = 0, w = 8, h = 8, type = 6,\n\
            angle = 1, range = 300 } }\n\
          skin.destination = { { id = \"r\", dst = { { x = 100, y = 300, w = 20, h = 10 } } } }\n\
          return skin\n",
    );
    common::png(d.path(), "a.png", 64, 64);
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let i = common::lua_play(&h, &Config::default(), &cancel).unwrap();
    let kinds: Vec<NoteKind> = i.imported.report.notes.iter().map(|n| n.kind).collect();
    assert_eq!(kinds, [NoteKind::Deviation(DeviationId::D4)]);
    let skin: Skin = i.imported.skin;
    let mut d = Drawn::over(skin, &chart());
    assert_eq!(d.rects(1_000), [moved(0.5)[1]]);
    assert_eq!(d.rects(2_000), [moved(1.0)[1]]);
}
