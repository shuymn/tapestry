//! A skin's per-frame build allocates nothing: effects through the
//! reducer, the snapshot into the fields, every timer, the `when` timers,
//! each image's frame and its place in the atlas, the canvas and the
//! regions' placement as the target, the layout and the scratch side
//! change, the lanes, number, gauge, judge and text widgets, and the
//! elements into the batch.
//!
//! A counting global allocator counts allocations made on threads that opt
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.
//! Profiles and the positive control: design.md "Scope of No Allocation".

mod image_support;
mod lanes_support;
mod play_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{Chart, ChartBuilder, Lane, NoteKind, SoundId, TimeUs};
use eval::{GaugeState, JudgeEvent, JudgeEventKind, Section, Tally};
use image_support::{dir, file, save, solid, whole};
use play_support::{bool_program, state, tally};
use rules::{GaugeValue, Judgment};
use skin_model::{
    Adjust, Align, Anchor, AtlasLimits, Attach, Attempt, Bind, Blend, ClipId, ComboPart, CycleDef,
    DigitsDef, Ease, ElementDef, End, Fill, Filter, Fit, GaugeDef, GaugeDesc, GaugeLook,
    GaugeParts, GlyphSet, GradePart, ImageDef, JudgeDef, JudgeSlot, KeyDef, LaneParts, LaneTimer,
    LaneView, Layout as SkinLayout, Motion, NumberDef, Pad, Place, Play, PlayContext, QuadBatch,
    Region, RegionDef, Sheet, Side, Skin, SkinDef, SkinFrame, Source, TimerRef, Track, UvTable,
    Variant, lane_timer, timer,
};
use skin_model::{
    ChartText, FontRef, FrameUv, Overflow, TextDef, TextField, TextImage, TextTable, TextValue,
};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: delegates to `System`; only adds a counter, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const MS: i64 = 1_000;
const NOTES: i64 = 2_000;
/// The most effects one frame drains; the rest wait for the next frame.
const DRAIN: usize = 64;

fn lane(n: i64) -> Lane {
    Lane::new(u8::try_from(n.rem_euclid(8)).unwrap()).unwrap()
}

/// Notes every 50 ms over the 8 lanes (400 ms apart in a lane), every 5th
/// a 200 ms long note (on every lane in turn), and a mine 25 ms after every
/// 5th, four lanes over.
fn chart() -> Chart {
    let mut b = ChartBuilder::new(150_000).sound("a.wav");
    for i in 0..NOTES {
        let t = TimeUs(i * 50 * MS);
        b = if i % 5 == 1 {
            b.long(lane(i), t, TimeUs(t.0 + 200 * MS))
        } else {
            b.tap(lane(i), t)
        };
        if i % 5 == 0 {
            b = b.mine(lane(i + 4), TimeUs(t.0 + 25 * MS), 10);
        }
    }
    b.build().unwrap()
}

fn ev(time: i64, kind: JudgeEventKind) -> JudgeEvent {
    JudgeEvent {
        seq: 0,
        time: TimeUs(time),
        origin: None,
        kind,
    }
}

/// Every kind of effect for every target of `section` (every note without
/// one) and every mine inside it, with both gauges reported after each
/// judgment, sorted by time, ending with `SongEnd`. A `clean` play judges
/// every target GOOD or better, so its combo reaches the target count;
/// otherwise BADs, POORs, BAD heads and empty POORs break it. The combos
/// are counted in time order, an empty POOR breaking the combo.
fn effects(chart: &Chart, section: Option<&Section>, clean: bool) -> Vec<JudgeEvent> {
    let grades = Judgment::ALL;
    let inside = |t: TimeUs| section.is_none_or(|s| s.start <= t && t < s.end);
    let mut out = Vec::new();
    let mut gauge = 50_i64;
    for (l, notes) in chart.lanes.iter().enumerate() {
        let lane = Lane::new(u8::try_from(l).unwrap()).unwrap();
        for (n, note) in notes.iter().enumerate() {
            let t = note.time.0;
            let judged = |judgment, deviation, release| JudgeEventKind::Judged {
                lane,
                note: n,
                judgment,
                deviation,
                release,
                combo: 0,
            };
            match note.kind {
                NoteKind::Mine { .. } => {
                    if inside(note.time) {
                        out.push(ev(t, JudgeEventKind::MineHit { lane, note: n }));
                    }
                    continue;
                }
                _ if !(inside(note.time) && section.is_none_or(|s| note.end() < s.end)) => {
                    continue;
                }
                NoteKind::Tap => {
                    out.push(ev(
                        t,
                        JudgeEventKind::PlaySound {
                            lane,
                            sound: SoundId(0),
                        },
                    ));
                    let k = usize::try_from(t / (50 * MS)).unwrap();
                    let judgment = grades[k % if clean { 3 } else { 5 }];
                    let deviation = (judgment != Judgment::Poor).then_some(TimeUs(0));
                    out.push(ev(t, judged(judgment, deviation, None)));
                    if judgment == Judgment::Poor {
                        out.push(ev(t + 10 * MS, JudgeEventKind::EmptyPoor { lane }));
                    }
                }
                NoteKind::Long { end, .. } => {
                    if !clean && (t / (200 * MS)) % 3 == 0 {
                        let head = judged(Judgment::Bad, Some(TimeUs(-20 * MS)), None);
                        out.push(ev(t - 20 * MS, head));
                        out.push(ev(
                            t + 100 * MS,
                            JudgeEventKind::LnHoldEnded { lane, note: n },
                        ));
                    } else {
                        out.push(ev(
                            t,
                            JudgeEventKind::LnHeld {
                                lane,
                                note: n,
                                judgment: Judgment::Great,
                                deviation: TimeUs(0),
                            },
                        ));
                        out.push(ev(end.0, JudgeEventKind::LnHoldEnded { lane, note: n }));
                        let done = judged(Judgment::Perfect, Some(TimeUs(0)), Some(TimeUs(0)));
                        out.push(ev(end.0, done));
                    }
                }
            }
            // The gauges climb to the maximum and fall back.
            gauge = if gauge >= 100 { 60 } else { gauge + 4 };
            for g in 0..2 {
                let value = GaugeValue(gauge.min(100) * GaugeValue::PERCENT);
                out.push(ev(
                    t,
                    JudgeEventKind::Gauge {
                        gauge: g,
                        value,
                        reference: value,
                    },
                ));
            }
        }
    }
    out.sort_by_key(|e| e.time);
    let mut running = 0;
    for e in &mut out {
        match &mut e.kind {
            JudgeEventKind::Judged {
                judgment, combo, ..
            } => {
                running = if *judgment >= Judgment::Bad {
                    0
                } else {
                    running + 1
                };
                *combo = running;
            }
            JudgeEventKind::EmptyPoor { .. } => running = 0,
            _ => {}
        }
    }
    let last = out.last().map_or(0, |e| e.time.0);
    out.push(ev(last + 500 * MS, JudgeEventKind::SongEnd));
    out
}

/// A 3-key track at x near `i`, growing wider and flatter; every 5th
/// turns.
fn track(i: u16, ease: Ease, end: End) -> Track {
    let turns = i.is_multiple_of(5);
    let key = |at: i64, x: i32, alpha: i32| {
        let mut props = [None; 9];
        let k = i32::try_from(at / (100 * MS)).unwrap();
        props[0] = Some(x);
        props[1] = Some(x / 2);
        props[2] = Some(64 + 8 * k);
        props[3] = Some(64 - 4 * k);
        props[4] = Some(alpha);
        props[8] = turns.then_some(90 * k - 45);
        KeyDef {
            at: TimeUs(at),
            props,
        }
    };
    let x = i32::from(i);
    Track::new(
        &[
            key(0, x, 0),
            key(100 * MS, x + 10, 255),
            key(500 * MS, x + 40, 128),
        ],
        ease,
        end,
    )
    .unwrap()
}

/// Images drawn by the elements on `when` timers, after one image per
/// built-in timer.
const WHEN_IMAGES: u16 = 5;
/// The first lane part's image, after the elements' images.
const PARTS: u16 = timer::BUILTIN + WHEN_IMAGES;
/// The lane parts' images from [`PARTS`] on: the note (a cycle of 4 frames
/// on `ready`), head, tail, active and inactive body, mine, bar line,
/// judgment line, cover and lift; then the scratch beam's.
const NOTE: u16 = PARTS;
const BODY_ON: u16 = PARTS + 3;
const BEAM: u16 = PARTS + 10;
/// The widgets' images after the beam: the numbers' glyphs (two sets of
/// ten and of twenty-four, cycled; eleven), the combos' (ten, eleven,
/// twenty-four), a grade per judge slot, the gauges' two look sets of
/// six, and the flicker gauge's six.
const DIGITS_10: u16 = BEAM + 1;
const DIGITS_11: u16 = BEAM + 2;
const DIGITS_24: u16 = BEAM + 3;
const COMBO_10: u16 = BEAM + 4;
const COMBO_11: u16 = BEAM + 5;
const COMBO_24: u16 = BEAM + 6;
const GRADES: u16 = BEAM + 7;
const LOOKS: u16 = GRADES + 7;
const FLICKER: u16 = LOOKS + 12;
const IMAGES: u16 = FLICKER + 6;
/// The reserved layers of the atlas.
const RESERVED: u16 = 1;

/// The canvases a stretch cycles through: 16:9 at two sizes, 21:9, 4:3
/// and one past the 4:1 cap.
const TARGETS: [[u32; 2]; 5] = [
    [1_920, 1_080],
    [3_840, 2_160],
    [2_560, 1_080],
    [1_440, 1_080],
    [4_321, 1_080],
];

/// A uv rectangle as a map key.
fn uv_key(uv: [f32; 4]) -> [u32; 4] {
    uv.map(f32::to_bits)
}

/// A lane part's key whatever its crop: the uv's horizontal extent and the
/// layer.
fn part_key(uv: [f32; 4], layer: u16) -> ([u32; 2], u16) {
    ([uv[0].to_bits(), uv[2].to_bits()], layer)
}

/// A skin, its atlas's uv table and, per uv, the image and frame it shows.
struct Loaded {
    skin: Skin,
    uvs: UvTable,
    shows: HashMap<[u32; 4], (u16, u16)>,
    parts: HashMap<([u32; 2], u16), u16>,
}

/// The frames of image `k`.
fn frames_of(k: u16) -> u16 {
    if k == NOTE {
        4
    } else if k == DIGITS_10 {
        20
    } else if k == DIGITS_11 || k == COMBO_11 {
        11
    } else if k == DIGITS_24 {
        48
    } else if k == COMBO_10 {
        10
    } else if k == COMBO_24 {
        24
    } else if k < PARTS && k.is_multiple_of(3) {
        4 + k % 5
    } else {
        1
    }
}

/// One image per built-in timer, then [`WHEN_IMAGES`] more: each a file of
/// its own, every third a cycle of 4 to 8 frames on its own built-in timer
/// or on a `when` timer, the filters alternating; then the lane parts and
/// the beam, each its own width. Built in a folder that lives until the
/// atlas is built.
fn images() -> (tempfile::TempDir, Sheet) {
    let d = dir();
    let mut files = Vec::new();
    let mut images = Vec::new();
    for k in 0..IMAGES {
        let cols = frames_of(k);
        let (w, h) = if k < PARTS {
            (u32::from(12 + k % 5), u32::from(10 + k % 3))
        } else if k <= BEAM {
            (u32::from(k), 16)
        } else {
            (4, u32::from(4 + k % 3))
        };
        let name = format!("{k}.png");
        save(
            d.path(),
            &name,
            &solid(w * u32::from(cols), h, [k as u8, 0, 0, 255]),
        );
        files.push(file(&name));
        let timer = if k == NOTE || k == DIGITS_10 || k == DIGITS_24 {
            TimerRef::Builtin(timer::READY)
        } else if k < timer::BUILTIN && k % 2 == 0 {
            TimerRef::Builtin(k)
        } else {
            TimerRef::When(k % 32)
        };
        images.push(ImageDef {
            grid: [cols, 1],
            cycle: (cols > 1).then_some(CycleDef {
                period: TimeUs(300 * MS),
                timer,
            }),
            filter: if k % 2 == 0 {
                Filter::Linear
            } else {
                Filter::Nearest
            },
            ..whole(u32::from(k))
        });
    }
    let sheet = Sheet::new(d.path(), files, images).unwrap();
    (d, sheet)
}

/// The regions: anchors spread over the nine, `top` and `bottom` optional,
/// the playfield centred.
fn regions() -> [RegionDef; Region::COUNT] {
    let mut out = [RegionDef {
        anchor: Anchor::TopLeft,
        optional: false,
    }; Region::COUNT];
    for (i, r) in Region::ALL.into_iter().enumerate() {
        out[r.index()] = RegionDef {
            anchor: Anchor::ALL[(i * 4) % 9],
            optional: matches!(r, Region::Top | Region::Bottom),
        };
    }
    out[Region::Playfield.index()].anchor = Anchor::Center;
    out
}

/// The simple lanes over the part images.
fn lanes() -> skin_model::LanesDef {
    let part = |j: u16| u32::from(PARTS + j);
    skin_model::LanesDef {
        parts: LaneParts {
            note: [part(0); 8],
            head: [part(1); 8],
            tail: [part(2); 8],
            body_on: [part(3); 8],
            body_off: [part(4); 8],
            mine: [part(5); 8],
            bar: Some(part(6)),
            line: Some(part(7)),
            cover: Some(part(8)),
            lift: Some(part(9)),
        },
        ..lanes_support::simple()
    }
}

/// An element showing `image` over the whole screen with `fill`.
fn background(image: u16, fill: Fill) -> ElementDef {
    let place = Place {
        fill,
        ..Place::SCREEN
    };
    lanes_support::image(u32::from(image), [0, 0, 1_920, 1_080], place)
}

/// An element on `timer` drawing `source` over `track` in `place`.
fn widget(source: Source, track: Track, timer: TimerRef, place: Place) -> ElementDef {
    ElementDef {
        source,
        track,
        timer,
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place,
    }
}

/// The widgets, their elements appended to `elements`:
/// - numbers in the three glyph layouts, every alignment and padding,
///   the cycled sets, bound to the span and a lane or not, turning, with
///   values going negative, overflowing and reaching `i32::MIN`;
/// - a gauge in each motion, the flicker's with its own images;
/// - two judges with every slot and a combo of each layout, one shifting
///   the grade and one not, their parts animated, additive or hidden.
fn widgets(elements: &mut Vec<ElementDef>) -> skin_model::WidgetDefs {
    let int = |src: &str| {
        skin_model::compile(
            src,
            &skin_model::Scope::new(&skin_model::PLAY),
            skin_model::Ty::Int,
        )
        .unwrap()
    };
    let strip = |image: u16, set, cells, align, pad, space| DigitsDef {
        image: u32::from(image),
        set,
        cells,
        align,
        pad,
        space,
    };
    let numbers = vec![
        (
            strip(DIGITS_10, GlyphSet::Ten, 4, Align::Right, Pad::Blank, 2),
            "combo * 100",
        ),
        (
            strip(DIGITS_11, GlyphSet::Eleven, 6, Align::Left, Pad::Glyph, -3),
            "ex_score - 50",
        ),
        (
            strip(
                DIGITS_24,
                GlyphSet::TwentyFour,
                5,
                Align::Center,
                Pad::Blank,
                0,
            ),
            "combo - 10",
        ),
        (
            strip(
                DIGITS_24,
                GlyphSet::TwentyFour,
                11,
                Align::Right,
                Pad::Zero,
                1,
            ),
            "if combo % 2 == 0 then -2147483647 - 1 else combo",
        ),
        (
            strip(DIGITS_10, GlyphSet::Ten, 3, Align::Center, Pad::Zero, 4),
            "bp - 3",
        ),
    ];
    let places = [
        Place {
            region: Region::Playfield,
            bind: Bind::Lanes,
            pivot: Anchor::Center,
            fill: Fill::None,
        },
        Place {
            region: Region::Left,
            pivot: Anchor::BottomRight,
            ..Place::SCREEN
        },
        Place {
            region: Region::Top,
            pivot: Anchor::Top,
            ..Place::SCREEN
        },
        Place::SCREEN,
        Place {
            region: Region::Playfield,
            bind: Bind::Lane(Lane::new(3).unwrap()),
            ..Place::SCREEN
        },
    ];
    for (i, place) in (0_u16..).zip(places) {
        elements.push(widget(
            Source::Number(u32::from(i)),
            track(300 + 7 * i, Ease::Linear, End::Hold),
            TimerRef::Builtin(timer::READY),
            place,
        ));
    }
    let numbers = numbers
        .into_iter()
        .map(|(digits, src)| NumberDef {
            digits,
            value: int(src),
        })
        .collect();
    let look = |first: u16| {
        let at = |k: u16| [u32::from(first + 2 * k), u32::from(first + 2 * k + 1)];
        GaugeParts {
            front: at(0),
            back: at(1),
            tip: at(2),
        }
    };
    let looks = |hard: GaugeParts| {
        let mut out = [look(LOOKS); GaugeLook::COUNT];
        out[GaugeLook::Hard.index()] = hard;
        out
    };
    let step = TimeUs(70 * MS);
    let gauges = vec![
        GaugeDef {
            segments: 50,
            looks: looks(look(LOOKS + 6)),
            motion: Motion::Still,
        },
        GaugeDef {
            segments: 40,
            looks: looks(look(LOOKS + 6)),
            motion: Motion::Random { range: 5, step },
        },
        GaugeDef {
            segments: 30,
            looks: looks(look(LOOKS + 6)),
            motion: Motion::Rise { range: 3, step },
        },
        GaugeDef {
            segments: 20,
            looks: looks(look(LOOKS + 6)),
            motion: Motion::Fall { range: 2, step },
        },
        GaugeDef {
            segments: 50,
            looks: [look(FLICKER); GaugeLook::COUNT],
            motion: Motion::Flicker {
                period: TimeUs(330 * MS),
            },
        },
    ];
    let regions = [
        Region::Bottom,
        Region::Right,
        Region::Screen,
        Region::Left,
        Region::Top,
    ];
    for (i, region) in (0_u16..).zip(regions) {
        elements.push(widget(
            Source::Gauge(u32::from(i)),
            track(400 + 11 * i, Ease::Out, End::Hold),
            TimerRef::Builtin(timer::PLAY),
            Place {
                region,
                pivot: Anchor::ALL[usize::from(i)],
                ..Place::SCREEN
            },
        ));
    }
    let combos = [
        strip(COMBO_10, GlyphSet::Ten, 4, Align::Center, Pad::Blank, 0),
        strip(COMBO_11, GlyphSet::Eleven, 5, Align::Right, Pad::Glyph, 2),
        strip(
            COMBO_24,
            GlyphSet::TwentyFour,
            11,
            Align::Left,
            Pad::Zero,
            -2,
        ),
    ];
    let judge = |shift: bool| {
        let mut j = JudgeDef {
            grades: Default::default(),
            combos: Default::default(),
            shift,
        };
        for s in JudgeSlot::ALL {
            let k = u16::try_from(s.index()).unwrap();
            let grade_key = |at: i64, y: i32| {
                let mut props = [None; 9];
                props[0] = Some(800);
                props[1] = Some(y);
                props[2] = Some(200);
                props[3] = Some(80);
                props[8] = Some(i32::from(k) * 10);
                KeyDef {
                    at: TimeUs(at),
                    props,
                }
            };
            j.grades[s.index()] = Some(GradePart {
                image: u32::from(GRADES + k),
                track: Track::new(
                    &[grade_key(0, 520), grade_key(200 * MS, 500)],
                    Ease::In,
                    End::Hold,
                )
                .unwrap(),
                timer: TimerRef::Builtin(timer::JUDGE),
                pivot: Anchor::ALL[s.index()],
                blend: if k % 2 == 0 { Blend::Alpha } else { Blend::Add },
                visible: (k == 3).then(|| bool_program("bp < 100000")),
            });
            j.combos[s.index()] = Some(ComboPart {
                digits: combos[s.index() % 3],
                track: track(10 + k, Ease::Linear, End::Hold),
                timer: if k == 4 {
                    TimerRef::Builtin(timer::COMBO_UP)
                } else {
                    TimerRef::Builtin(timer::JUDGE)
                },
                attach: if k % 2 == 0 {
                    Attach::Top
                } else {
                    Attach::Bottom
                },
                pivot: Anchor::Center,
                blend: Blend::Alpha,
                visible: (k == 2).then(|| bool_program("combo % 3 != 0")),
            });
        }
        j
    };
    let still = lanes_support::still([None; 9]);
    for i in 0..2 {
        elements.push(widget(
            Source::Judge(i),
            still.clone(),
            TimerRef::Builtin(timer::READY),
            Place {
                region: Region::Judge,
                ..Place::SCREEN
            },
        ));
    }
    // The title, centred and squeezed while its box is narrower than the
    // line, and a literal, right-aligned and spilling.
    let grow = |at: i64, w: i32| {
        let mut props = [None; 9];
        props[..4].copy_from_slice(&[Some(700), Some(20), Some(w), Some(TEXT_H)]);
        KeyDef {
            at: TimeUs(at),
            props,
        }
    };
    let keys = [grow(0, 20), grow(400 * MS, 200)];
    let squeeze = Track::new(&keys, Ease::Linear, End::Loop { from: TimeUs(0) }).unwrap();
    let ready = TimerRef::Builtin(timer::READY);
    elements.push(widget(Source::Text(0), squeeze, ready, Place::SCREEN));
    let top = Place {
        region: Region::Top,
        ..Place::SCREEN
    };
    let fixed = lanes_support::rect([900, 20, 300, TEXT_H]);
    elements.push(widget(Source::Text(1), fixed, ready, top));
    let text = |value, align, overflow| TextDef {
        value,
        font: FontRef::Default,
        align,
        overflow,
    };
    skin_model::WidgetDefs {
        numbers,
        gauges,
        judges: vec![judge(true), judge(false)],
        texts: vec![
            text(
                TextValue::Field(TextField::Title),
                Align::Center,
                Overflow::Shrink,
            ),
            text(
                TextValue::Literal("MAX".into()),
                Align::Right,
                Overflow::Spill,
            ),
        ],
    }
}

/// The text elements' key height.
const TEXT_H: i32 = 40;
/// Where the text lines lie: outside every page's 0..1, so no image frame
/// shares them.
const TITLE_UV: [f32; 4] = [2.0, 2.0, 3.0, 3.0];
const LITERAL_UV: [f32; 4] = [4.0, 4.0, 5.0, 5.0];
/// The lines' ink, in em: 2.5 wide and 1.25 tall, so the title (100 at an
/// em of 40) is squeezed in boxes narrower than 100.
const TEXT_INK: [f32; 4] = [0.0, 0.125, 2.5, 1.25];

/// The table every attempt attaches, untracked, after its prepare.
fn text_table() -> TextTable {
    let line = |uv| TextImage::new(FrameUv { uv, layer: 0 }, TEXT_INK, 2.5);
    TextTable::new(vec![line(TITLE_UV), line(LITERAL_UV)])
}

/// 2 000 elements, 30 on every built-in timer and 350 over 32 `when`s,
/// with every ease and end, both blends, clips and visibility programs,
/// spread over the 7 regions with the 9 pivots, turning and resizing, the
/// playfield's bound to lanes or the span; a background on `ready`, cover
/// and stretch backgrounds, the lanes and a beam bound to the scratch;
/// each element shows its timer's image.
fn skin() -> Loaded {
    let conditions = [
        "combo >= 1",
        "combo >= 10",
        "gauge >= 800",
        "on(judge.perfect)",
        "elapsed(judge) < 200",
        "on(bomb.3) && !on(hold.3)",
        "on(gauge_max)",
        "elapsed(beat) < 100",
    ];
    let whens: Vec<_> = (0..32)
        .map(|i| bool_program(conditions[i % conditions.len()]))
        .collect();
    let visible = [
        bool_program("combo % 2 == 0"),
        bool_program("bp < 1000"),
        bool_program("on(play)"),
    ];
    let eases = [Ease::Linear, Ease::In, Ease::Out, Ease::Step];
    let builtin = 30 * timer::BUILTIN;
    let mut elements = Vec::new();
    for i in 0_u16..2_000 {
        // A built-in timer's elements show its image.
        let (timer, image) = if i < builtin {
            let slot = i % timer::BUILTIN;
            (TimerRef::Builtin(slot), slot)
        } else {
            (TimerRef::When(i % 32), timer::BUILTIN + i % WHEN_IMAGES)
        };
        let end = match i % 3 {
            0 => End::Hold,
            1 => End::Hide,
            _ => End::Loop {
                from: TimeUs(100 * MS),
            },
        };
        let region = Region::ALL[usize::from(i % 7)];
        let bind = match (region, i % 3) {
            (Region::Playfield, 0) => Bind::Lane(lane(i64::from(i / 7))),
            (Region::Playfield, 1) => Bind::Lanes,
            _ => Bind::None,
        };
        elements.push(ElementDef {
            source: Source::Image(u32::from(image)),
            track: track(i, eases[usize::from(i % 4)], end),
            timer,
            visible: (i % 7 == 0).then(|| visible[usize::from(i % 3)].clone()),
            blend: if (i / 3) % 2 == 0 {
                Blend::Alpha
            } else {
                Blend::Add
            },
            clip: (i % 11 == 0).then_some(ClipId(i % 4)),
            place: Place {
                region,
                pivot: Anchor::ALL[usize::from(i % 9)],
                bind,
                fill: Fill::None,
            },
        });
    }
    // A background on `ready`, drawn on every frame after it.
    elements.push(ElementDef {
        source: Source::Image(u32::from(timer::READY)),
        track: track(0, Ease::Linear, End::Hold),
        timer: TimerRef::Builtin(timer::READY),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    });
    elements.push(background(timer::READY, Fill::Cover));
    elements.push(background(timer::READY, Fill::Stretch));
    elements.push(lanes_support::lanes_element());
    let beam = lanes_support::bound(Bind::Lane(Lane::SCRATCH));
    elements.push(ElementDef {
        blend: Blend::Add,
        ..lanes_support::image(u32::from(BEAM), [152, 824, 112, 112], beam)
    });
    let widgets = widgets(&mut elements);
    let (_dir, sheet) = images();
    let skin = Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: regions(),
        whens,
        sheet,
        lanes: Some(lanes()),
        elements,
        widgets,
    })
    .unwrap();
    // A page of 256 holds only some of the frames.
    let limits = AtlasLimits {
        max_side: 256,
        max_layers: 16,
        reserved_layers: u32::from(RESERVED),
    };
    let atlas = skin.atlas(Variant::Lo, limits).unwrap();
    assert!(atlas.pages().len() >= 2, "{} pages", atlas.pages().len());
    let (_, uvs) = atlas.into_parts();
    // Frames run image by image: image k's frames follow image k - 1's.
    let mut shows = HashMap::new();
    let mut parts = HashMap::new();
    let mut next = 0;
    for k in 0..IMAGES {
        for f in 0..frames_of(k) {
            let frame = uvs.get(next).unwrap();
            assert!(shows.insert(uv_key(frame.uv), (k, f)).is_none());
            if (PARTS..=BEAM).contains(&k) {
                let old = parts.insert(part_key(frame.uv, frame.layer), k);
                // A cycle's frames may share a column.
                assert!(old.is_none_or(|o| o == k), "{k} and {old:?}");
            }
            next += 1;
        }
    }
    assert_eq!(usize::try_from(next).unwrap(), uvs.frames().len());
    Loaded {
        skin,
        uvs,
        shows,
        parts,
    }
}

/// What a stretch saw.
struct Witness {
    split_drains: u32,
    small_drops: u32,
    other_drops: u32,
    /// Per built-in slot: whether it was on at a frame.
    on: [bool; timer::BUILTIN as usize],
    /// Per built-in slot: whether an element on it reached the batch.
    drawn: [bool; timer::BUILTIN as usize],
    when_started: bool,
    empty_frames_after_ready: u32,
    /// Whether a quad showed a frame other than an image's first.
    later_frame: bool,
    /// Whether a quad lay on a layer after the first skin page.
    later_layer: bool,
    /// Whether the lanes drew a note, and an active body.
    notes: bool,
    active_body: bool,
    /// Whether a quad turned.
    turned: bool,
    /// The beam's x per target, at the author's layout.
    beam: [Option<f32>; TARGETS.len()],
    /// The widgets': a combo drew and the shifting judge's grade lay apart
    /// from the other's; the full-gauge PERFECT showed; a sign glyph drew;
    /// the flicker's tip was pushed; each shown gauge drew its own look.
    widgets: Widgets,
}

#[derive(Clone, Copy, Default)]
struct Widgets {
    combo: bool,
    shifted: bool,
    at_max: bool,
    sign: bool,
    overlay: bool,
    looks: [bool; 2],
    /// The text quads drawn, and whether the title drew squeezed.
    text_quads: u32,
    text_squeezed: bool,
}

impl Widgets {
    fn merge(&mut self, o: Self) {
        self.combo |= o.combo;
        self.shifted |= o.shifted;
        self.at_max |= o.at_max;
        self.sign |= o.sign;
        self.overlay |= o.overlay;
        for (a, b) in self.looks.iter_mut().zip(o.looks) {
            *a |= b;
        }
        self.text_quads += o.text_quads;
        self.text_squeezed |= o.text_squeezed;
    }

    /// What one frame's quads show, `shown` the gauge index it showed.
    fn see(&mut self, loaded: &Loaded, quads: &[skin_model::Quad], shown: usize) {
        let mut grades = [None; 2];
        let mut n_grades = 0;
        let mut combo = false;
        let mut flicker = 0;
        for q in quads {
            if q.uv == TITLE_UV || q.uv == LITERAL_UV {
                self.text_quads += 1;
                // Squeezed: narrower for its height than the ink.
                let [.., iw, ih] = TEXT_INK;
                self.text_squeezed |= q.uv == TITLE_UV && q.rect[2] * ih < q.rect[3] * iw * 0.99;
            }
            let Some(&(image, frame)) = loaded.shows.get(&uv_key(q.uv)) else {
                continue;
            };
            match image {
                COMBO_10 | COMBO_11 | COMBO_24 => combo = true,
                DIGITS_24 if frame % 12 == 11 => self.sign = true,
                FLICKER..IMAGES => flicker += 1,
                LOOKS..FLICKER => {
                    let set = usize::from((image - LOOKS) / 6);
                    if set == shown {
                        self.looks[set] = true;
                    }
                }
                GRADES..LOOKS => {
                    self.at_max |= image == GRADES + 6;
                    if let Some(g) = grades.get_mut(n_grades) {
                        *g = Some(q.rect[0]);
                    }
                    n_grades += 1;
                }
                _ => {}
            }
        }
        self.combo |= combo;
        if let [Some(a), Some(b)] = grades
            && combo
            && n_grades == 2
        {
            self.shifted |= a != b;
        }
        self.overlay |= flicker == 51;
    }
}

impl Witness {
    fn new() -> Self {
        Self {
            widgets: Widgets::default(),
            split_drains: 0,
            small_drops: 0,
            other_drops: 0,
            on: [false; timer::BUILTIN as usize],
            drawn: [false; timer::BUILTIN as usize],
            when_started: false,
            empty_frames_after_ready: 0,
            later_frame: false,
            later_layer: false,
            notes: false,
            active_body: false,
            turned: false,
            beam: [None; TARGETS.len()],
        }
    }
}

/// What a stretch's frames cycle through besides the target.
struct Views {
    layouts: Vec<SkinLayout>,
    lanes: [LaneView; 4],
}

impl Views {
    /// The author's layout first, then a moved and scaled playfield, a
    /// hidden judge, a lane width, an inset and a HUD pushed off the
    /// canvas; no covers, a cover, a lift, both.
    fn new() -> Self {
        let adjust = |x, y, scale| Adjust {
            x,
            y,
            scale,
            visible: true,
        };
        let mut moved = SkinLayout::AUTHOR;
        moved.set(Region::Playfield, adjust(100, -40, 120)).unwrap();
        moved.set(Region::Judge, adjust(30, 0, 150)).unwrap();
        let mut hidden = moved;
        hidden
            .set(
                Region::Judge,
                Adjust {
                    visible: false,
                    ..Adjust::IDENTITY
                },
            )
            .unwrap();
        let mut wide = SkinLayout::AUTHOR;
        wide.set_lane_width(150).unwrap();
        wide.set(Region::Playfield, adjust(0, 0, 50)).unwrap();
        let mut inset = SkinLayout::AUTHOR;
        inset.set_inset(600).unwrap();
        inset.set_lane_width(50).unwrap();
        let mut pushed = SkinLayout::AUTHOR;
        pushed.set(Region::Top, adjust(-4_320, 4_320, 200)).unwrap();
        pushed
            .set(Region::Right, adjust(4_320, -4_320, 50))
            .unwrap();
        let view = |cover, lift| LaneView {
            visible_time: TimeUs(600 * MS),
            cover,
            lift,
        };
        Self {
            layouts: vec![SkinLayout::AUTHOR, moved, hidden, wide, inset, pushed],
            lanes: [view(0, 0), view(250, 0), view(0, 100), view(950, 300)],
        }
    }
}

/// One attempt's frames, every 16 ms from `from` to `to`, each draining at
/// most [`DRAIN`] effects; a 1 s stall halfway backs the queue up, fails
/// gauge 1, shows it and changes the display offset. The target changes
/// every 5 frames, the lane view every 23 and the layout every 37; frames
/// after the last effect are complete.
#[allow(clippy::too_many_arguments)]
fn stretch(
    loaded: &Loaded,
    views: &Views,
    play: &mut Play,
    events: &[JudgeEvent],
    tally: &mut Tally,
    gauges: &mut [GaugeState],
    batch: &mut QuadBatch,
    small: &mut QuadBatch,
    from: i64,
    to: i64,
    w: &mut Witness,
) {
    let skin = &loaded.skin;
    let mut next = 0;
    let mut now = from;
    let mut frame_no = 0_usize;
    let mut offset = TimeUs(0);
    let mut stalled = false;
    let midway = from + (to - from) / 2;
    let last = events.last().map_or(0, |e| e.time.0);
    while now <= to {
        if (midway..midway + 16 * MS).contains(&now) {
            now += 1_000 * MS;
            gauges[1].failed_at = Some(TimeUs(now));
            offset = TimeUs(40 * MS);
            stalled = true;
        }
        let pending = events[next..].partition_point(|e| e.time.0 <= now);
        let take = pending.min(DRAIN);
        if pending > take {
            w.split_drains += 1;
        }
        let drain = &events[next..next + take];
        next += take;
        // The snapshot follows the drain.
        for e in drain {
            match e.kind {
                JudgeEventKind::Judged { combo, .. } => {
                    tally.combo = combo;
                    tally.max_combo = tally.max_combo.max(combo);
                }
                JudgeEventKind::Gauge { gauge, value, .. } => {
                    if let Some(g) = gauges.get_mut(gauge) {
                        g.value = value;
                    }
                }
                _ => {}
            }
        }
        // After the stall the shown gauge is mostly gauge 1, every 4th
        // frame gauge 0.
        let shown = usize::from(stalled && !frame_no.is_multiple_of(4));
        let target_no = (frame_no / 5) % TARGETS.len();
        let layout_no = (frame_no / 37) % views.layouts.len();
        let frame = SkinFrame {
            now: TimeUs(now),
            display_offset: offset,
            tally,
            gauges,
            gauge: shown,
            effects: drain,
            complete: now > last,
            target: TARGETS[target_no],
            layout: &views.layouts[layout_no],
            lanes: views.lanes[(frame_no / 23) % views.lanes.len()],
        };
        let use_small = frame_no % 100 == 99;
        let out = if use_small { &mut *small } else { &mut *batch };
        out.clear();
        skin.build(play, &frame, &loaded.uvs, out);
        if use_small {
            w.small_drops += out.dropped();
        } else {
            w.other_drops += out.dropped();
            w.widgets.see(loaded, out.quads(), shown);
            if out.quads().is_empty() {
                w.empty_frames_after_ready += 1;
            }
            for q in out.quads() {
                w.turned |= q.angle != 0.0;
                match loaded.parts.get(&part_key(q.uv, q.layer)) {
                    Some(&NOTE) => w.notes = true,
                    Some(&BODY_ON) => w.active_body = true,
                    Some(&BEAM) if layout_no == 0 => {
                        w.beam[target_no].get_or_insert(q.rect[0]);
                    }
                    _ => {}
                }
                let Some(&(image, frame)) = loaded.shows.get(&uv_key(q.uv)) else {
                    continue;
                };
                if let Some(d) = w.drawn.get_mut(usize::from(image)) {
                    *d = true;
                }
                w.later_frame |= frame > 0;
                w.later_layer |= q.layer > RESERVED;
            }
        }
        for (slot, on) in (0..).zip(w.on.iter_mut()) {
            *on |= play.timer(slot).is_some();
        }
        w.when_started |= play.timer(timer::BUILTIN).is_some();
        frame_no += 1;
        now += 16 * MS;
    }
}

#[test]
fn build_does_not_allocate() {
    let chart = chart();
    let loaded = skin();
    let skin = &loaded.skin;
    let views = Views::new();
    let descs = [
        GaugeDesc {
            start: GaugeValue(50 * GaugeValue::PERCENT),
            max: GaugeValue::FULL,
            look: GaugeLook::Normal,
            border: GaugeValue(80 * GaugeValue::PERCENT),
        },
        GaugeDesc {
            start: GaugeValue::FULL,
            max: GaugeValue::FULL,
            look: GaugeLook::Hard,
            border: GaugeValue::EMPTY,
        },
    ];
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let mut small = QuadBatch::with_capacity(8, 4);
    let lead_in = 1_000 * MS;
    let full = PlayContext {
        gauges: &descs,
        attempt: Attempt::Full {
            lead_in: TimeUs(lead_in),
        },
        empty_poor_breaks_combo: true,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
    };
    let round = Section {
        start: TimeUs(30_000 * MS),
        end: TimeUs(60_000 * MS),
        lead_in: TimeUs(2_000 * MS),
        initial_gauges: vec![GaugeValue(70 * GaugeValue::PERCENT), GaugeValue::FULL],
    };
    let clean_round = Section {
        start: TimeUs(10_000 * MS),
        end: TimeUs(20_000 * MS),
        ..round.clone()
    };
    // (name, context, effects, first frame): the whole chart with breaks,
    // a practice round with breaks after a retry with the scratch on the
    // right, and a round played to a full combo on the left again.
    let attempts = [
        ("play", full, effects(&chart, None, false), -lead_in),
        (
            "round",
            PlayContext {
                attempt: Attempt::Section(&round),
                scratch: Side::Right,
                ..full
            },
            effects(&chart, Some(&round), false),
            28_000 * MS,
        ),
        (
            "clean round",
            PlayContext {
                attempt: Attempt::Section(&clean_round),
                ..full
            },
            effects(&chart, Some(&clean_round), true),
            8_000 * MS,
        ),
    ];
    let mut seen = Witness::new();
    let mut beams = Vec::new();
    for (name, ctx, events, from) in &attempts {
        // Prepared untracked, as between plays, with the text attached.
        let mut play = skin.prepare(&chart, ctx).unwrap();
        skin.attach_text(&mut play, text_table()).unwrap();
        let mut tally = tally(0);
        let initial = match ctx.attempt {
            Attempt::Full { .. } => [descs[0].start, descs[1].start],
            Attempt::Section(s) => [s.initial_gauges[0], s.initial_gauges[1]],
        };
        let mut gauges = initial.map(state);
        let to = events.last().unwrap().time.0 + 1_000 * MS;
        let mut w = Witness::new();

        TRACK.with(|t| t.set(true));
        // The count works: an allocation that escapes is counted in any
        // profile.
        drop(std::hint::black_box(vec![1u8; 8]));
        assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 1, "{name}");
        stretch(
            &loaded,
            &views,
            &mut play,
            events,
            &mut tally,
            &mut gauges,
            &mut batch,
            &mut small,
            *from,
            to,
            &mut w,
        );
        TRACK.with(|t| t.set(false));
        assert_eq!(ALLOCS.load(Ordering::Relaxed), 0, "{name}: allocations");

        assert!(w.split_drains > 0, "{name}: no drain was split");
        assert!(w.small_drops > 0, "{name}: the small batch dropped nothing");
        assert_eq!(w.other_drops, 0, "{name}");
        assert!(w.when_started, "{name}: no `when` timer started");
        assert_eq!(w.empty_frames_after_ready, 0, "{name}");
        assert!(w.later_frame, "{name}: no cycle left its first frame");
        assert!(w.later_layer, "{name}: every quad on the first page");
        assert!(w.notes, "{name}: the lanes drew no note");
        assert!(w.active_body, "{name}: no long note drew active");
        assert!(w.turned, "{name}: no quad turned");
        // The playfield is centred: 21:9 moves the beam 320 right of 16:9.
        let [Some(at_16_9), _, Some(at_21_9), ..] = w.beam else {
            panic!(
                "{name}: the beam was not seen at 16:9 and 21:9: {:?}",
                w.beam
            );
        };
        assert_eq!(at_21_9 - at_16_9, 320.0, "{name}");
        beams.push(at_16_9);
        let fc = usize::from(timer::FULL_COMBO);
        if *name == "clean round" {
            assert!(w.on[fc], "{name}: `full_combo` never started");
            assert!(w.drawn[fc], "{name}: its elements were never drawn");
        } else {
            assert!(!w.on[fc] && !w.drawn[fc], "{name}: a full combo");
        }
        for (a, b) in seen.on.iter_mut().zip(w.on) {
            *a |= b;
        }
        for (a, b) in seen.drawn.iter_mut().zip(w.drawn) {
            *a |= b;
        }
        seen.widgets.merge(w.widgets);
    }
    // The widgets' witnesses, seen inside the counted stretches.
    let ws = seen.widgets;
    assert!(ws.combo, "no combo drew");
    assert!(ws.shifted, "no grade shifted");
    assert!(ws.at_max, "PERFECT at a full gauge never showed");
    assert!(ws.sign, "no sign glyph drew");
    assert!(ws.overlay, "no flicker tip was pushed");
    assert_eq!(ws.looks, [true; 2], "a shown gauge never drew its look");
    assert!(ws.text_quads > 0, "no text drew");
    assert!(ws.text_squeezed, "the title never drew squeezed");
    // The beam follows the scratch: authored at 152 on the left, 600 with
    // the scratch on the right (tests/lanes.rs).
    assert_eq!(beams, [152.0, 600.0, 152.0]);
    // Every fed built-in timer turned on, and an element on it was drawn,
    // in some attempt; lane input is not fed yet.
    for lane in 0..8 {
        for kind in [LaneTimer::Press, LaneTimer::Release] {
            let slot = usize::from(lane_timer(kind, Lane::new(lane).unwrap()));
            assert!(!seen.on[slot] && !seen.drawn[slot], "slot {slot}");
            seen.on[slot] = true;
            seen.drawn[slot] = true;
        }
    }
    let missing = |flags: [bool; timer::BUILTIN as usize]| -> Vec<u16> {
        (0..)
            .zip(flags)
            .filter(|&(_, f)| !f)
            .map(|(s, _)| s)
            .collect()
    };
    assert_eq!(missing(seen.on), [0_u16; 0], "built-in timers never on");
    assert_eq!(
        missing(seen.drawn),
        [0_u16; 0],
        "built-in timers never drawn"
    );
    source_lanes_do_not_allocate(&chart, &full);
    // The next attempt, prepared untracked after the stretches, builds.
    let mut play = skin.prepare(&chart, &full).unwrap();
    skin.attach_text(&mut play, text_table()).unwrap();
    let t = tally(0);
    let gauges = [descs[0].start, descs[1].start].map(state);
    batch.clear();
    let frame = SkinFrame {
        now: TimeUs(5_000 * MS),
        display_offset: TimeUs(0),
        tally: &t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        complete: false,
        target: TARGETS[0],
        layout: &views.layouts[0],
        lanes: views.lanes[0],
    };
    skin.build(&mut play, &frame, &loaded.uvs, &mut batch);
    assert!(!batch.quads().is_empty());
    assert_eq!(batch.dropped(), 0);
    // Both text elements ask for a line; the title is the chart's.
    let chart_text = ChartText {
        title: "Title",
        ..ChartText::default()
    };
    let requests = skin.text_requests(&chart_text, TARGETS[0], &views.layouts[0]);
    let texts: Vec<_> = requests.iter().map(|r| (r.slot, r.text.as_str())).collect();
    assert_eq!(texts, [(0, "Title"), (1, "MAX")]);

    // Bitmap text, laid out untracked, draws its glyphs every frame
    // without allocating, squeezed or not, with a fallback glyph.
    let (_fonts, bitmap) = bitmap_skin();
    let uvs = bitmap
        .atlas(Variant::Lo, LIMITS_SMALL)
        .unwrap()
        .into_parts()
        .1;
    let mut play = bitmap.prepare(&chart, &full).unwrap();
    bitmap.attach_bitmap_text(
        &mut play,
        &ChartText {
            title: "ABAB",
            ..ChartText::default()
        },
    );
    let mut batch = QuadBatch::with_capacity(bitmap.quad_capacity(), bitmap.run_capacity());
    let mut glyphs = 0;
    TRACK.with(|t| t.set(true));
    for ms in (0..2_000).step_by(16) {
        batch.clear();
        let frame = SkinFrame {
            now: TimeUs(ms * MS),
            display_offset: TimeUs(0),
            tally: &t,
            gauges: &gauges,
            gauge: 0,
            effects: &[],
            complete: false,
            target: TARGETS[0],
            layout: &views.layouts[0],
            lanes: views.lanes[0],
        };
        bitmap.build(&mut play, &frame, &uvs, &mut batch);
        glyphs += batch.quads().len();
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "bitmap text: allocations"
    );
    assert!(glyphs > 0, "no glyph drew");
}

const LIMITS_SMALL: AtlasLimits = AtlasLimits {
    max_side: 256,
    max_layers: 4,
    reserved_layers: 1,
};

/// Two bitmap texts of the title: `A` from the primary file, `B` from its
/// fallback; the second squeezed into its box.
fn bitmap_skin() -> (tempfile::TempDir, Skin) {
    let root = dir();
    let fnt = |name: &str, code: u32| {
        let s = format!(
            "info face=\"t\" size=10 padding=0,0,0,0\n\
             common lineHeight=12 base=10 scaleW=16 scaleH=16 pages=1\n\
             page id=0 file=\"{name}.png\"\n\
             char id={code} x=0 y=0 width=6 height=9 xoffset=1 yoffset=1 xadvance=8 page=0\n"
        );
        std::fs::write(root.path().join(format!("{name}.fnt")), s).unwrap();
        save(
            root.path(),
            &format!("{name}.png"),
            &solid(16, 16, [255; 4]),
        );
    };
    fnt("a", 65);
    fnt("b", 66);
    let mut b = skin_model::SheetBuilder::new(&skin_model::Root::new(root.path()).unwrap());
    let a = b.admit_fnt("a.fnt", &mut |_| Ok(())).unwrap();
    let f = b.admit_fnt("b.fnt", &mut |_| Ok(())).unwrap();
    b.add_bitmap_font(a, &[f]).unwrap();
    let text = |overflow| TextDef {
        value: TextValue::Field(TextField::Title),
        font: FontRef::Bitmap(0),
        align: Align::Center,
        overflow,
    };
    let element = |i: u16| ElementDef {
        source: Source::Text(u32::from(i)),
        track: track(i, Ease::Linear, End::Hold),
        timer: TimerRef::Builtin(timer::READY),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    };
    let skin = Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: regions(),
        whens: Vec::new(),
        sheet: b.finish(),
        lanes: None,
        elements: vec![element(0), element(1)],
        widgets: skin_model::WidgetDefs {
            texts: vec![text(Overflow::Spill), text(Overflow::Shrink)],
            ..skin_model::WidgetDefs::default()
        },
    })
    .unwrap();
    (root, skin)
}

/// The source player's lanes (`lanes_support::source`) on `chart` played
/// as CN, so every long note shows its end cap, through frames every 16 ms
/// under covers that leave scroll and covers that leave none: nothing
/// allocated, and notes, end caps and bodies drawn.
fn source_lanes_do_not_allocate(chart: &Chart, ctx: &PlayContext<'_>) {
    let skin = lanes_support::lanes_skin(lanes_support::source());
    let uvs = play_support::uvs(&skin);
    let mut cn = chart.clone();
    cn.meta.ln_mode = Some(bms_model::LnMode::Cn);
    let mut play = skin.prepare(&cn, ctx).unwrap();
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let t = tally(0);
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let layout = SkinLayout::AUTHOR;
    let views = [(0, 0), (500, 200), (1_000, 0), (0, 1_000), (1_000, 1_000)];
    let tail = uvs.get(lanes_support::TAIL).unwrap().uv;
    let (mut quads, mut tails) = (0_usize, 0_usize);
    TRACK.with(|t| t.set(true));
    for (i, now) in (0..12_000 * MS).step_by(16 * MS as usize).enumerate() {
        let (cover, lift) = views[i / 50 % views.len()];
        batch.clear();
        let frame = SkinFrame {
            now: TimeUs(now),
            display_offset: TimeUs(0),
            tally: &t,
            gauges: &gauges,
            gauge: 0,
            effects: &[],
            complete: false,
            target: TARGETS[i % TARGETS.len()],
            layout: &layout,
            lanes: LaneView {
                visible_time: TimeUs(600 * MS),
                cover,
                lift,
            },
        };
        skin.build(&mut play, &frame, &uvs, &mut batch);
        quads += batch.quads().len();
        tails += batch.quads().iter().filter(|q| q.uv == tail).count();
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.swap(0, Ordering::Relaxed),
        0,
        "source lanes: allocations"
    );
    assert!(
        quads > 0 && tails > 0,
        "source lanes drew {quads} quads, {tails} end caps"
    );
}
