//! The source's built-in system images (a destination id that is a
//! negative number): −110 and −111 draw an opaque black and an opaque
//! white 1×1 image as any image is drawn, decorative or SUDDEN+'s paint;
//! the chart's images (−100 to −102) and undefined ids are dropped.

mod common;

use common::lanes::{cover, with};
use common::play::{Drawn, quad_texel, round};
use common::{Loaded, import_body};
use skin_beatoraja::{
    CriticalPart, ImportError, ImportLimit, NoteKind, Object, Severity, TimerWhy,
};
use skin_model::{AtlasLimits, Blend, MAX_SKIN_FRAMES, MAX_TOTAL_TEXELS, Variant};

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 4_096,
    max_layers: 8,
    reserved_layers: 1,
};

/// A quad's rounded rect, colour, atlas texel and blend.
type Painted = ([i32; 4], [f32; 4], [u8; 4], Blend);

/// Each quad of `l` at `ms` over an empty chart.
fn painted(l: &Loaded, ms: i64) -> Vec<Painted> {
    let skin = l.ok().skin.clone();
    let atlas = skin.atlas(Variant::Lo, LIMITS, &|| false).unwrap();
    let mut d = Drawn::new(skin, 0);
    let quads = d.frame(ms, &[]);
    let runs = d.batch.runs().to_vec();
    let blend = |i: usize| {
        let i = u32::try_from(i).unwrap();
        let run = runs.iter().find(|r| (r.start..r.end).contains(&i));
        run.expect("a run holds every quad").key.blend
    };
    quads
        .iter()
        .enumerate()
        .map(|(i, q)| (round(q.rect), q.color, quad_texel(&atlas, q), blend(i)))
        .collect()
}

const BLACK: [u8; 4] = [0, 0, 0, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

#[test]
fn black_and_white_draw_at_their_destination_with_its_colour_and_blend() {
    let l = import_body(
        r#""destination": [
            {"id": "-110", "dst": [{"x": 10, "y": 20, "w": 300, "h": 40}]},
            {"id": "-111", "blend": 2,
             "dst": [{"x": 0, "y": 0, "w": 1920, "h": 1080, "a": 128, "r": 255, "g": 0, "b": 64}]},
            {"id": "-111", "dst": [{"x": 5, "y": 5, "w": 1, "h": 1}]}]"#,
    );
    let r = l.report();
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    // Two generated images, shared by the white destinations; no file.
    let u = r.usage;
    assert_eq!((u.elements, u.images, u.frames), (3, 2, 2));
    assert_eq!((u.file_attempts, u.files), (0, 0));
    let quads = painted(&l, 0);
    assert_eq!(
        quads,
        [
            // y up: 1080 − 20 − 40.
            ([10, 1_020, 300, 40], [1.0; 4], BLACK, Blend::Alpha),
            (
                [0, 0, 1_920, 1_080],
                [1.0, 0.0, 64.0 / 255.0, 128.0 / 255.0],
                WHITE,
                Blend::Add
            ),
            ([5, 1_074, 1, 1], [1.0; 4], WHITE, Blend::Alpha),
        ]
    );
}

#[test]
fn a_system_image_stretches_as_a_one_texel_image() {
    // Stretch 1 keeps the 1×1 frame's aspect inside the destination;
    // stretch 9 its native size; a trimming stretch is dropped.
    let l = import_body(
        r#""destination": [
            {"id": "-110", "stretch": 1, "dst": [{"x": 0, "y": 0, "w": 100, "h": 50}]},
            {"id": "-110", "stretch": 9, "dst": [{"x": 0, "y": 0, "w": 100, "h": 50}]},
            {"id": "-110", "stretch": 3, "dst": [{"x": 0, "y": 0, "w": 100, "h": 50}]}]"#,
    );
    assert_eq!(common::kinds(l.report()), [NoteKind::Stretch(3)]);
    let rects: Vec<_> = painted(&l, 0).into_iter().map(|q| q.0).collect();
    assert_eq!(rects, [[25, 1_030, 50, 50], [50, 1_055, 1, 1]]);
}

#[test]
fn a_system_image_goes_through_the_destination_rules() {
    // Conditions, timers and tracks apply as to any image: an undefined
    // op hides it, an unsupported timer drops it, a looped track keys it.
    let l = import_body(
        r#""destination": [
            {"id": "-111", "op": [7777], "dst": [{"w": 1, "h": 1}]},
            {"id": "-111", "timer": 2, "dst": [{"w": 1, "h": 1}]},
            {"id": "-110", "dst": [{"time": 0, "w": 10, "h": 10, "a": 0},
                                   {"time": 100, "w": 10, "h": 10, "a": 255}]}]"#,
    );
    let r = l.report();
    assert!(
        r.notes.iter().any(|n| n.severity == Severity::Dropped
            && n.kind == NoteKind::Timer(TimerWhy::Unsupported(2))),
        "{:?}",
        r.notes
    );
    // Neither white one is drawn, so only black is charged.
    assert_eq!(r.usage.images, 1);
    let alpha = |ms| -> Vec<f32> { painted(&l, ms).iter().map(|q| q.1[3]).collect() };
    assert_eq!(alpha(50), [127.5 / 255.0]);
    // Its loop point is 0: at 150 ms it is at 50 again.
    assert_eq!(alpha(150), [127.5 / 255.0]);
}

#[test]
fn an_offset_four_system_image_paints_sudden_plus() {
    // Black SUDDEN+ paint over the default lanes, moved by the cover.
    let lid = cover("").replace(r#""id": "cover""#, r#""id": "-110""#);
    let l = with("", &[&lid]);
    assert_eq!(l.report().usage.file_attempts, 1, "a.png alone");
    let skin = l.ok().skin.clone();
    let atlas = skin.atlas(Variant::Lo, LIMITS, &|| false).unwrap();
    let mut d = Drawn::new(skin, 0);
    d.view.cover = 500;
    let quads = d.frame(1_000, &[]);
    let shown: Vec<_> = quads
        .iter()
        .map(|q| (round(q.rect), quad_texel(&atlas, q)))
        .collect();
    assert_eq!(shown, [([20, -20, 430, 600], BLACK)]);
    // Stretch 5 fits a 1×1 frame to the destination's width, or keeps
    // the destination where that frame would be taller.
    for (h, shown) in [(600, [20, 65, 430, 430]), (300, [20, 280, 430, 300])] {
        let lid = lid
            .replace(r#""offset": 4"#, r#""offset": 4, "stretch": 5"#)
            .replace(r#""h": 600"#, &format!(r#""h": {h}"#));
        let l = with("", &[&lid]);
        let mut d = Drawn::new(l.ok().skin.clone(), 0);
        d.view.cover = 500;
        assert_eq!(d.shown(1_000), [shown], "{h}");
    }
    // Play-critical: moving it untimed refuses the skin (D3).
    let moving = lid.replace(
        r#""dst": [{"x": 20, "y": 800, "w": 430, "h": 600}]"#,
        r#""dst": [{"time": 0, "x": 20, "y": 800, "w": 430, "h": 600},
                   {"time": 500, "x": 20, "y": 700, "w": 430, "h": 600}]"#,
    );
    match with("", &[&moving]).err() {
        ImportError::Critical { part, why, .. } => {
            assert_eq!(
                (part, why),
                (CriticalPart::Cover, NoteKind::Timer(TimerWhy::Untimed))
            );
        }
        e => panic!("{e:?}"),
    }
}

/// A stretch-5 SUDDEN+ paint of black over `[x, y, w, h]` (source
/// coordinates, y up): its quads' rects at cover 0.
fn trimmed_paint(x: i32, y: i32, w: i32, h: i32) -> Vec<[i32; 4]> {
    let lid = format!(
        r#"{{"id": "-110", "offset": 4, "stretch": 5,
            "dst": [{{"x": {x}, "y": {y}, "w": {w}, "h": {h}}}]}}"#
    );
    let l = with("", &[&lid]);
    Drawn::new(l.ok().skin.clone(), 0).rects(0)
}

#[test]
fn stretch_five_takes_the_sources_branch_for_any_sign() {
    // The source trims when `h < w · ih / iw` (`skin/StretchType.java:
    // 65-68, 140-150`): keeping the destination, else fitting a frame
    // `w` high about the destination's centre. A model rect's y is
    // 1080 − y − h.
    let cases = [
        // w −100 < h: fitted −100 high about y 825, the centre.
        ((300, 800, -100, 50), [300, 305, -100, -100]),
        // w 0 < h: fitted 0 high.
        ((300, 800, 0, 50), [300, 255, 0, 0]),
        // h −50 < w 100: trimmed, the destination kept.
        ((200, 800, 100, -50), [200, 330, 100, -50]),
        // h −50 < w 0: trimmed.
        ((300, 800, 0, -50), [300, 330, 0, -50]),
        // w 100 > h 50: trimmed; w 100 < h 300: fitted 100 high.
        ((200, 800, 100, 50), [200, 230, 100, 50]),
        ((200, 800, 100, 300), [200, 80, 100, 100]),
    ];
    for ((x, y, w, h), want) in cases {
        assert_eq!(trimmed_paint(x, y, w, h), [want], "{x} {y} {w} {h}");
    }
}

#[test]
fn an_untimed_critical_fade_is_kept_with_one_d3_note() {
    // Only its alpha changes, so the critical paint may run on `screen`
    // (D3): admitted, fading in over 500 ms, with one deviation note.
    let fade = r#"{"id": "-111", "offset": 4,
        "dst": [{"time": 0, "x": 20, "y": 800, "w": 430, "h": 600, "a": 0},
                {"time": 500, "x": 20, "y": 800, "w": 430, "h": 600, "a": 255}]}"#;
    let l = with("", &[fade]);
    assert_eq!(common::d3_notes(&l), 1);
    let mut d = Drawn::new(l.ok().skin.clone(), 0);
    let alpha =
        |d: &mut Drawn, ms| -> Vec<f32> { d.frame(ms, &[]).iter().map(|q| q.color[3]).collect() };
    assert_eq!(alpha(&mut d, 250), [127.5 / 255.0]);
    assert_eq!(alpha(&mut d, 400), [204.0 / 255.0]);
}

#[test]
fn each_filter_charges_one_image_per_colour() {
    let l = import_body(
        r#""destination": [
            {"id": "-110", "dst": [{"w": 1, "h": 1}]},
            {"id": "-110", "filter": 1, "dst": [{"w": 1, "h": 1}]},
            {"id": "-110", "dst": [{"w": 1, "h": 1}]},
            {"id": "-110", "filter": 1, "dst": [{"w": 1, "h": 1}]},
            {"id": "-111", "filter": 1, "dst": [{"w": 1, "h": 1}]}]"#,
    );
    let u = l.report().usage;
    assert_eq!(
        (u.elements, u.images, u.frames, u.file_attempts),
        (5, 3, 3, 0)
    );
    let filters: Vec<_> = l.quads(0).iter().map(|q| q.filter).collect();
    use skin_model::Filter::{Linear, Nearest};
    assert_eq!(filters, [Nearest, Linear, Nearest, Linear, Linear]);
}

/// A play skin of 31 images of 1,024 frames and one of `last`, each
/// drawn with both filters, then `extra` destinations.
fn frames_skin(last: &str, extra: &str) -> common::Loaded {
    let mut images: Vec<String> = (0..31)
        .map(|i| {
            format!(r#"{{"id": "{i}", "src": "a", "w": 64, "h": 64, "divx": 32, "divy": 32}}"#)
        })
        .collect();
    images.push(format!(r#"{{"id": "31", "src": "a", {last}}}"#));
    let dsts = common::join((0..64).map(|k| {
        format!(
            r#"{{"id": "{}", "filter": {}, "dst": [{{"w": 1, "h": 1}}]}}"#,
            k / 2,
            k % 2
        )
    }));
    import_body(&format!(
        r#""image": [{}], "destination": [{dsts} {extra}]"#,
        images.join(", ")
    ))
}

#[test]
fn system_images_charge_the_frame_limit() {
    // 31 grids of 1,024 frames and one of 1,023 (31 × 33), each with two
    // filters: two frames short of the limit, which black and white take.
    let last = r#""w": 62, "h": 33, "divx": 31, "divy": 33"#;
    let both = r#", {"id": "-110", "dst": [{"w": 1, "h": 1}]},
                   {"id": "-111", "dst": [{"w": 1, "h": 1}]}"#;
    let l = frames_skin(last, both);
    let u = l.report().usage;
    assert_eq!(
        (u.images, u.frames),
        (66, u32::try_from(MAX_SKIN_FRAMES).unwrap())
    );
    assert_eq!(l.ok().skin.sheet().frame_count(), MAX_SKIN_FRAMES);
    // Either colour again charges nothing; black with the other filter
    // is one frame past it.
    let again = format!("{both}{both}");
    assert_eq!(frames_skin(last, &again).report().usage.images, 66);
    let linear = format!(r#"{both}, {{"id": "-110", "filter": 1, "dst": [{{"w": 1, "h": 1}}]}}"#);
    match frames_skin(last, &linear).result {
        Err(ImportError::Limit { limit, at }) => {
            assert_eq!(limit, ImportLimit::Frames);
            assert!(at.is_some());
        }
        other => panic!("{:?}", other.map(|i| i.report)),
    }
}

/// Fifteen 4096×3072 files and a 4096×`last` one, each drawn, then
/// `extra` destinations.
fn texels_skin(last: i32, extra: &str) -> Result<skin_beatoraja::Imported, ImportError> {
    let sources = common::join((0..16).map(|i| format!(r#"{{"id": "{i}", "path": "{i}.bmp"}}"#)));
    let images =
        common::join((0..16).map(|i| format!(r#"{{"id": "{i}", "src": "{i}", "w": 1, "h": 1}}"#)));
    let dsts =
        common::join((0..16).map(|i| format!(r#"{{"id": "{i}", "dst": [{{"w": 1, "h": 1}}]}}"#)));
    let json = common::hd(&format!(
        r#""source": [{sources}], "image": [{images}], "destination": [{dsts} {extra}]"#
    ));
    let d = common::skin(&[("s.json", &json)]);
    for i in 0..16 {
        let h = if i == 15 { last } else { 3_072 };
        common::write(d.path(), &format!("{i}.bmp"), &common::bmp_header(4_096, h));
    }
    common::import_dir(d.path())
}

#[test]
fn system_images_charge_the_texel_totals() {
    // 15 × 12 Mi + 4096 × 3071 texels leaves 4,096 of the 192 Mi a
    // variant holds: a system image fits.
    let one = ",\n{\"id\": \"-110\", \"dst\": [{\"w\": 1, \"h\": 1}]}";
    let r = texels_skin(3_071, one).unwrap().report;
    assert_eq!((r.usage.images, r.usage.files), (17, 16), "{:?}", r.notes);
    // The files alone fill exactly 192 Mi and are accepted.
    let r = texels_skin(3_072, "").unwrap().report;
    assert_eq!((r.usage.images, r.usage.files), (16, 16), "{:?}", r.notes);
    // A system image after them does not fit: the limit is located at
    // its destination, on the definition's second line.
    match texels_skin(3_072, one) {
        Err(ImportError::Limit {
            limit,
            at: Some(at),
        }) => {
            assert_eq!(limit, ImportLimit::FileTotals);
            assert_eq!((at.file.as_str(), at.line, at.col), ("s.json", 2, 1));
        }
        other => panic!("{:?}", other.map(|i| i.report)),
    }
    assert_eq!(MAX_TOTAL_TEXELS, 16 * (4_096 * 3_072));
}

#[test]
fn other_negative_ids_stay_dropped() {
    let l = import_body(
        r#""destination": [
            {"id": "-100", "dst": [{"w": 10, "h": 10}]},
            {"id": "-101", "dst": [{"w": 10, "h": 10}]},
            {"id": "-102", "dst": [{"w": 10, "h": 10}]},
            {"id": "-5", "dst": [{"w": 10, "h": 10}]},
            {"id": "-2147483648", "dst": [{"w": 10, "h": 10}]}]"#,
    );
    let system = NoteKind::Unsupported {
        object: Object::System,
    };
    let r = l.report();
    assert_eq!(common::kinds(r), [system; 5]);
    assert!(r.notes.iter().all(|n| n.severity == Severity::Dropped));
    assert_eq!(r.usage.images, 0);
    assert!(painted(&l, 0).is_empty());
}

#[test]
fn a_lua_skin_draws_system_images() {
    let d = common::skin(&[(
        "s.luaskin",
        "local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         skin.destination = {\n\
           { id = -111, blend = 2, dst = { { x = 0, y = 0, w = 64, h = 32, a = 64 } } },\n\
           { id = \"-110\", dst = { { x = 64, y = 0, w = 64, h = 32 } } },\n\
           { id = -100, dst = { { x = 0, y = 0, w = 64, h = 32 } } },\n\
         }\n\
         return skin\n",
    )]);
    let imported = common::lua_import(d.path()).unwrap().imported;
    let system = NoteKind::Unsupported {
        object: Object::System,
    };
    assert_eq!(common::kinds(&imported.report), [system]);
    assert_eq!(imported.report.usage.images, 2);
    let l = Loaded {
        dir: d,
        result: Ok(imported),
    };
    assert_eq!(
        painted(&l, 0),
        [
            (
                [0, 1_048, 64, 32],
                [1.0, 1.0, 1.0, 64.0 / 255.0],
                WHITE,
                Blend::Add
            ),
            ([64, 1_048, 64, 32], [1.0; 4], BLACK, Blend::Alpha),
        ]
    );
}
