//! Skins with widgets for the number, gauge, judge and widget tests: a
//! sheet whose every frame has its own uv, so a quad tells its image and
//! frame apart, and helpers that build widget definitions and read the
//! glyphs, segments and grades back out of a batch.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bms_model::{Chart, ChartBuilder, TimeUs};
use image::{ImageFormat, Rgba, RgbaImage};
use rules::{GaugeValue, Judgment};
use skin_model::{
    Align, Anchor, Attach, Blend, ComboPart, CycleDef, DigitsDef, ElementDef, FileDef, Filter, Fit,
    Follow, GaugeDef, GaugeDesc, GaugeLook, GaugeParts, GlyphSet, GradePart, ImageDef, JudgeDef,
    JudgeSlot, LanesDef, LoadError, Motion, NumberDef, PLAY, Pad, Place, Program, Quad, Region,
    RegionDef, Scope, Sheet, Skin, SkinDef, Source, TimerRef, Ty, WidgetDefs, compile, timer,
};

use crate::lanes_support::rect;
use crate::play_support::{Harness, MS, REGIONS, ev, full, judged};

/// The images, by index; image 0 is a plain one.
pub const TEN: u32 = 1;
pub const ELEVEN: u32 = 2;
pub const TWENTY_FOUR: u32 = 3;
/// Two sets of ten glyphs cycled over 100 ms on `judge`.
pub const TEN_CYCLED: u32 = 4;
/// 25 frames: no whole set of 24.
pub const TWENTY_FIVE: u32 = 5;
/// The grade images, one per slot from [`GRADE`].
pub const GRADE: u32 = 6;
/// The gauge images from [`GAUGE`]: per look set (normal, then hard),
/// front, back and tip, each at or above then below the border.
pub const GAUGE: u32 = GRADE + 7;
pub const IMAGES: u32 = GAUGE + 12;

/// Each image's frames.
pub fn frames_of(i: u32) -> u16 {
    match i {
        TEN => 10,
        ELEVEN => 11,
        TWENTY_FOUR => 24,
        TEN_CYCLED => 20,
        TWENTY_FIVE => 25,
        _ => 1,
    }
}

/// A folder of the images under the target's test folder, written once,
/// each atomically; each frame 2×2, each image its own height.
fn root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("skin-model-widgets");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..IMAGES {
            let tmp = tempfile::Builder::new()
                .suffix(".png")
                .tempfile_in(&dir)
                .unwrap();
            RgbaImage::from_pixel(2 * u32::from(frames_of(i)), 2 + i % 3, Rgba([255; 4]))
                .save_with_format(tmp.path(), ImageFormat::Png)
                .unwrap();
            tmp.persist(dir.join(format!("{i}.png"))).unwrap();
        }
        dir
    })
}

/// The sheet of the images: image `i` is file `i`, cut into its frames.
pub fn sheet() -> Sheet {
    Sheet::new(
        root(),
        (0..IMAGES)
            .map(|i| FileDef {
                path: format!("{i}.png"),
                hi: None,
            })
            .collect(),
        (0..IMAGES)
            .map(|file| ImageDef {
                file,
                rect: None,
                grid: [frames_of(file), 1],
                cycle: (file == TEN_CYCLED).then_some(CycleDef {
                    period: TimeUs(100 * MS),
                    timer: TimerRef::Builtin(timer::JUDGE),
                }),
                filter: Filter::Linear,
            })
            .collect(),
    )
    .unwrap()
}

/// A skin of `elements` over [`sheet`] with `widgets` under `fit`.
pub fn load_fit(
    fit: Fit,
    regions: [RegionDef; Region::COUNT],
    lanes: Option<LanesDef>,
    elements: Vec<ElementDef>,
    widgets: WidgetDefs,
) -> Result<Skin, LoadError> {
    Skin::new(SkinDef {
        fit,
        regions,
        whens: Vec::new(),
        sheet: sheet(),
        lanes,
        elements,
        widgets,
        bomb_up_to: Some(skin_model::Grade::Bad),
    })
}

/// An adapted skin of `elements` with `widgets`.
pub fn load(elements: Vec<ElementDef>, widgets: WidgetDefs) -> Result<Skin, LoadError> {
    load_fit(Fit::Adapt, REGIONS, None, elements, widgets)
}

/// An element on `ready` drawing `source` over `r`.
pub fn element(source: Source, r: [i32; 4], place: Place) -> ElementDef {
    ElementDef {
        source,
        track: rect(r),
        timer: TimerRef::Builtin(timer::READY),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place,
    }
}

/// A judge element: still, on `ready`, in `judge`.
pub fn judge_element(i: u32) -> ElementDef {
    element(
        Source::Judge(i),
        [0; 4],
        Place {
            region: Region::Judge,
            ..Place::SCREEN
        },
    )
}

/// A strip of `cells` over `image` with `set`, `align` and `pad`, space 2.
pub fn digits(image: u32, set: GlyphSet, cells: u8, align: Align, pad: Pad) -> DigitsDef {
    DigitsDef {
        image,
        set,
        cells,
        align,
        pad,
        space: 2,
    }
}

pub fn int(src: &str) -> Program {
    compile(src, &Scope::new(&PLAY), Ty::Int).unwrap()
}

pub fn boolean(src: &str) -> Program {
    compile(src, &Scope::new(&PLAY), Ty::Bool).unwrap()
}

pub fn number(d: DigitsDef, src: &str) -> NumberDef {
    NumberDef {
        digits: d,
        value: int(src),
    }
}

/// Every look the same: front, back and tip of [`GAUGE`]'s first set.
pub fn parts(set: u32) -> GaugeParts {
    let at = |k: u32| [GAUGE + 6 * set + 2 * k, GAUGE + 6 * set + 2 * k + 1];
    GaugeParts {
        front: at(0),
        back: at(1),
        tip: at(2),
    }
}

/// A gauge of `segments` with `motion`: the hard looks drawn with the
/// second image set, the rest with the first.
pub fn gauge(segments: u16, motion: Motion) -> GaugeDef {
    let mut looks = [parts(0); GaugeLook::COUNT];
    looks[GaugeLook::Hard.index()] = parts(1);
    GaugeDef {
        segments,
        looks,
        motion,
    }
}

/// A grade part on `judge` showing `image` over `r`, holding.
pub fn grade(image: u32, r: [i32; 4]) -> GradePart {
    GradePart {
        image,
        track: rect(r),
        timer: TimerRef::Builtin(timer::JUDGE),
        pivot: Anchor::TopLeft,
        blend: Blend::Alpha,
        visible: None,
        native: None,
        follow: Follow::None,
    }
}

/// A combo part on `judge` over `d` at the relative rect `r`, attached to
/// the grade's top.
pub fn combo(d: DigitsDef, r: [i32; 4]) -> ComboPart {
    ComboPart {
        digits: d,
        track: rect(r),
        timer: TimerRef::Builtin(timer::JUDGE),
        attach: Attach::Top,
        pivot: Anchor::TopLeft,
        blend: Blend::Alpha,
        visible: None,
        native: None,
    }
}

/// A judge with no parts.
pub fn empty_judge() -> JudgeDef {
    JudgeDef {
        grades: Default::default(),
        combos: Default::default(),
        shift: true,
    }
}

/// A judge with every slot's grade image (its own), at `r`.
pub fn judge_all(r: [i32; 4]) -> JudgeDef {
    let mut j = empty_judge();
    for s in JudgeSlot::ALL {
        j.grades[s.index()] = Some(grade(GRADE + u32::try_from(s.index()).unwrap(), r));
    }
    j
}

/// A chart without notes.
pub fn chart() -> Chart {
    ChartBuilder::new(150_000).build().unwrap()
}

/// A harness over `skin` for a full attempt without a lead-in.
pub fn harness(skin: Skin) -> Harness {
    Harness::new(skin, &chart(), &full(0))
}

/// A `Judged` of `judgment` at `ms` with the event combo `combo`.
pub fn judgment(ms: i64, judgment: Judgment, combo: u32) -> eval::JudgeEvent {
    ev(ms, judged(1, 0, judgment, Some(0), None, combo))
}

/// The image and frame of a quad, by its uv.
pub fn shown(h: &Harness, q: &Quad) -> Option<(u32, u16)> {
    let mut first = 0;
    for i in 0..IMAGES {
        for f in 0..frames_of(i) {
            if h.uvs
                .get(first + u32::from(f))
                .is_some_and(|u| u.uv == q.uv && u.layer == q.layer)
            {
                return Some((i, f));
            }
        }
        first += u32::from(frames_of(i));
    }
    None
}

/// The batch's quads with their image and frame.
pub fn drawn(h: &Harness) -> Vec<(u32, u16, Quad)> {
    h.batch
        .quads()
        .iter()
        .map(|q| {
            let (i, f) = shown(h, q).expect("a known frame");
            (i, f, *q)
        })
        .collect()
}

/// The glyphs drawn (frames) and their x, in draw order.
pub fn glyphs(h: &Harness) -> Vec<(u16, f32)> {
    drawn(h)
        .into_iter()
        .map(|(_, f, q)| (f, q.rect[0]))
        .collect()
}

/// A gauge descriptor with `look` and `border` in %.
pub fn desc(look: GaugeLook, border: i64) -> GaugeDesc {
    GaugeDesc {
        start: GaugeValue(20 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
        look,
        border: GaugeValue(border * GaugeValue::PERCENT),
    }
}

/// A balanced sum of `leaves` (a power of two) reads of `bp`: `2·leaves −
/// 1` operations, nested only `log2(leaves)` deep.
pub fn sum(leaves: usize) -> String {
    if leaves <= 1 {
        "bp".to_owned()
    } else {
        format!("({} + {})", sum(leaves / 2), sum(leaves / 2))
    }
}
