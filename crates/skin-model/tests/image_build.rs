//! Images in a play: each element shows its image's frame at the image's
//! own cycle timer, with the image's filter and the frame's layer, and a
//! uv table of another sheet draws nothing.

mod image_support;
mod play_support;

use bms_model::{Chart, ChartBuilder, TimeUs};
use image::{Rgba, RgbaImage};
use image_support::{dir, file, save, whole};
use play_support::{
    Harness, MS, bool_program, def, element, ev, full, judged, lane, tally, uvs, white_sheet,
};
use rules::Judgment;
use skin_model::{
    CycleDef, ElementDef, Filter, ImageDef, LoadError, MAX_KEY_US, Sheet, Skin, Source, TimerRef,
    timer,
};
use tempfile::TempDir;

fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(3_000 * MS))
        .build()
        .unwrap()
}

/// Four 2×2 frames side by side, each its own colour.
fn strip(d: &TempDir) {
    let img = RgbaImage::from_fn(8, 2, |x, _| Rgba([(x / 2) as u8 * 60, 0, 0, 255]));
    save(d.path(), "strip.png", &img);
}

fn cycled(period_us: i64, timer: TimerRef) -> ImageDef {
    ImageDef {
        grid: [4, 1],
        cycle: Some(CycleDef {
            period: TimeUs(period_us),
            timer,
        }),
        ..whole(0)
    }
}

fn shows(image: u32, timer: TimerRef) -> ElementDef {
    ElementDef {
        source: Source::Image(image),
        ..element(timer, 0)
    }
}

/// The frame each quad shows: the first frame with its uv.
fn frames(h: &Harness) -> Vec<usize> {
    h.batch
        .quads()
        .iter()
        .map(|q| h.uvs.frames().iter().position(|f| f.uv == q.uv).unwrap())
        .collect()
}

/// Images: a 400 ms cycle on `judge`, the same on `when` 0, and one frame
/// sampled nearest; each drawn by an element on `ready`.
fn skin(d: &TempDir) -> Skin {
    strip(d);
    let sheet = Sheet::new(
        d.path(),
        vec![file("strip.png")],
        vec![
            cycled(400 * MS, TimerRef::Builtin(timer::JUDGE)),
            cycled(400 * MS, TimerRef::When(0)),
            ImageDef {
                filter: Filter::Nearest,
                rect: Some([0, 0, 2, 1]),
                ..whole(0)
            },
        ],
    )
    .unwrap();
    let ready = TimerRef::Builtin(timer::READY);
    Skin::new(def(
        vec![bool_program("combo >= 2")],
        sheet,
        vec![shows(0, ready), shows(1, ready), shows(2, ready)],
    ))
    .unwrap()
}

#[test]
fn a_cycle_runs_on_its_images_timer() {
    let d = dir();
    let mut h = Harness::new(skin(&d), &chart(), &full(1_000));
    // Not started: frame 0, and the elements on `ready` are drawn.
    h.frame(500, &[]);
    assert_eq!(frames(&h), [0, 0, 8]);
    let first = [ev(1_000, judged(1, 0, Judgment::Perfect, Some(0), None, 1))];
    let start = 1_000 * MS;
    for (offset, want) in [
        (0, 0),
        (99_999, 0),
        (100_000, 1),
        (399_999, 3),
        (400_000, 0),
        (650_000, 2),
    ] {
        let effects: &[_] = if offset == 0 { &first } else { &[] };
        h.frame_us(start + offset, effects);
        assert_eq!(frames(&h)[0], want, "{offset} µs after the judgment");
        assert_eq!(frames(&h)[1], 0, "the `when` cycle is off");
    }
    // A judgment restarts it at 0.
    h.frame(
        3_000,
        &[ev(3_000, judged(1, 1, Judgment::Perfect, Some(0), None, 2))],
    );
    assert_eq!(frames(&h)[0], 0);
    h.frame(3_150, &[]);
    assert_eq!(frames(&h)[0], 1);
    // A judgment stamped after the frame shows as just started.
    h.frame(
        3_300,
        &[ev(3_350, judged(1, 1, Judgment::Great, Some(0), None, 2))],
    );
    assert_eq!(frames(&h)[0], 0);
    // The `when` cycle starts on its condition's rising edge.
    h.tally = tally(2);
    h.frame(5_000, &[]);
    assert_eq!(frames(&h)[1], 0);
    h.frame(5_300, &[]);
    assert_eq!(frames(&h)[1], 3);
    h.tally = tally(0);
    h.frame(5_400, &[]);
    assert_eq!(frames(&h)[1], 0);
    assert_eq!(h.batch.quads().len(), 3);
}

#[test]
fn the_longest_period_at_the_latest_time_stays_in_range() {
    let d = dir();
    strip(&d);
    let sheet = Sheet::new(
        d.path(),
        vec![file("strip.png")],
        vec![cycled(MAX_KEY_US, TimerRef::Builtin(timer::READY))],
    )
    .unwrap();
    let ready = TimerRef::Builtin(timer::READY);
    let skin = Skin::new(def(Vec::new(), sheet, vec![shows(0, ready)])).unwrap();
    let ctx = skin_model::PlayContext {
        attempt: skin_model::Attempt::Full {
            lead_in: TimeUs(i64::MAX),
        },
        ..full(0)
    };
    let mut h = Harness::new(skin, &chart(), &ctx);
    h.frame_us(i64::MIN + 1, &[]);
    assert_eq!(frames(&h), [0]);
    h.frame_us(i64::MAX, &[]);
    // `ready` started at i64::MIN + 1; the elapsed time saturates.
    let t = i128::from(i64::MAX) % i128::from(MAX_KEY_US);
    let want = usize::try_from(t * 4 / i128::from(MAX_KEY_US)).unwrap();
    assert_eq!(frames(&h), [want]);
}

#[test]
fn quads_carry_the_filter_and_the_layer() {
    let d = dir();
    let mut h = Harness::new(skin(&d), &chart(), &full(1_000));
    h.frame(0, &[]);
    let quads = h.batch.quads();
    assert_eq!(
        quads.iter().map(|q| q.filter).collect::<Vec<_>>(),
        [Filter::Linear, Filter::Linear, Filter::Nearest]
    );
    for (q, f) in quads.iter().zip(frames(&h)) {
        let layer = h.uvs.frames()[f].layer;
        assert!(layer >= 1);
        assert_eq!(q.layer, layer);
    }
}

#[test]
fn a_table_of_another_sheet_draws_nothing() {
    let d = dir();
    let mut h = Harness::new(skin(&d), &chart(), &full(1_000));
    h.frame(0, &[]);
    assert_eq!(h.batch.quads().len(), 3);
    let own = h.uvs.clone();
    // Another length.
    let one = Skin::new(def(Vec::new(), white_sheet(), Vec::new())).unwrap();
    h.uvs = uvs(&one);
    assert_eq!(h.uvs.frames().len(), 1);
    h.frame(10, &[]);
    assert!(h.batch.quads().is_empty());
    // The same length from another sheet: the same files loaded again.
    let twin = skin(&d);
    assert_eq!(twin.frame_count(), own.frames().len());
    assert_ne!(twin.sheet().id(), h.skin.sheet().id());
    h.uvs = uvs(&twin);
    assert_eq!(h.uvs.frames(), own.frames());
    h.frame(20, &[]);
    assert!(h.batch.quads().is_empty());
    h.uvs = own;
    h.frame(30, &[]);
    assert_eq!(h.batch.quads().len(), 3);
}

#[test]
fn image_references_are_checked_at_load() {
    let d = dir();
    strip(&d);
    let sheet = |timer| {
        Sheet::new(
            d.path(),
            vec![file("strip.png")],
            vec![whole(0), cycled(1, timer)],
        )
        .unwrap()
    };
    let ready = TimerRef::Builtin(timer::READY);
    for bad in [TimerRef::Builtin(timer::BUILTIN), TimerRef::When(1)] {
        let err = Skin::new(def(vec![bool_program("true")], sheet(bad), Vec::new())).unwrap_err();
        assert!(
            matches!(err, LoadError::UnknownImageTimer { image: 1 }),
            "{err:?}"
        );
    }
    let err = Skin::new(def(
        Vec::new(),
        sheet(ready),
        vec![shows(1, ready), shows(2, ready)],
    ))
    .unwrap_err();
    assert!(
        matches!(err, LoadError::UnknownImage { element: 1 }),
        "{err:?}"
    );
}
