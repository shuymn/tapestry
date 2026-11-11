//! A skin's per-frame build allocates nothing: effects through the
//! reducer, the snapshot into the fields, every timer, the `when` timers,
//! each image's frame and its place in the atlas, the canvas and the
//! regions' placement as the target, the layout and the scratch side
//! change, the lanes, number, gauge, judge and text widgets, and the
//! elements into the batch, image quads at their native size where they
//! keep one; the score rates and the gauge kind under both empty-POOR
//! policies as the shown gauge switches, a refused inconsistent snapshot,
//! and the readouts per round: known and unknown metrics, READY,
//! a STOP, control changes, an invalid frame and the poisoned play; the
//! rhythm clock across bar lines, a tempo change, a STOP, the tail past
//! its end, a rearm and Closing; and
//! the presentation program's callbacks on every path: the first frame and
//! later ones, a float result, a caught error, a false gate, a false
//! guard, a sentinel suppressing the draws after it, and a poisoned play;
//! and its numerics past ±2^53: integer arithmetic promoted to the
//! double, a division of it, a float's floor kept a float, negation and
//! `abs` at ±2^53, and the state clock read past 2^53 µs and negative.
//!
//! A counting global allocator counts allocations made on threads that opt
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.
//! Profiles and the positive control: design.md "Scope of No Allocation".

mod image_support;
mod lanes_support;
mod play_support;
mod present_support;
mod widget_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{Chart, ChartBuilder, Lane, NoteKind, SoundId, TimeUs};
use eval::{GaugeState, JudgeEvent, JudgeEventKind, NoteFacts, Section, Tally};
use image_support::{dir, file, save, solid, whole};
use play_support::{bool_program, readouts_for, state, tally};
use rules::{GaugeValue, Judgment};
use skin_model::{
    Adjust, Align, Anchor, AtlasLimits, Attach, Attempt, Bind, Blend, BpmRange, BuildError, ClipId,
    ComboPart, CoverImage, CoverMotion, CoverPaintDef, CoverSettings, CoverTrim, CycleDef,
    DigitOffset, DigitsDef, DurationRatio, Ease, ElementDef, End, Fill, Filter, Fit, Follow,
    FrameReadouts, Gain, GaugeDef, GaugeDesc, GaugeLook, GaugeParts, GlyphSet, GradePart, ImageDef,
    JudgeDef, JudgeSlot, KeyDef, LaneParts, LaneView, Layout as SkinLayout, Motion, Native,
    NativeReadouts, NumberDef, Pad, Place, Play, PlayContext, QuadBatch, Region, RegionDef, Sheet,
    Side, Skin, SkinDef, SkinFrame, Source, TimerRef, Track, UvTable, Variant, field, timer,
};
use skin_model::{
    ChartText, FontRef, FrameUv, LoadState, LoadingFrame, LoadingGauge, Overflow, TextDef,
    TextField, TextImage, TextSlots, TextTable, TextValue,
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
/// Every way an element or a grade follows the covers, cycled through.
const FOLLOWS: [Follow; 8] = [
    Follow::NONE,
    Follow::LIFT,
    Follow::COVER,
    Follow::LIFT_AND_COVER,
    Follow::HIDDEN,
    Follow {
        lift: true,
        ..Follow::HIDDEN
    },
    Follow {
        cover: true,
        ..Follow::HIDDEN
    },
    Follow {
        lift: true,
        cover: true,
        hidden: true,
    },
];

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

/// An event caused by an input at `time`: a press, which it counts as
/// the one press of its lane in its µs, or a release, after none.
fn by_input(time: i64, mut kind: JudgeEventKind) -> JudgeEvent {
    match &mut kind {
        JudgeEventKind::Judged {
            release: Some(_), ..
        } => {}
        JudgeEventKind::Judged { presses, .. }
        | JudgeEventKind::LnHeld { presses, .. }
        | JudgeEventKind::EmptyPoor { presses, .. } => *presses = 1,
        _ => {}
    }
    JudgeEvent {
        origin: Some(TimeUs(time)),
        ..ev(time, kind)
    }
}

/// Every kind of effect for every target of `section` (every note without
/// one) and every mine inside it, with both gauges reported after each
/// judgment, sorted by time, ending with `SongEnd`. A `clean` play judges
/// every target GOOD or better, so its combo reaches the target count;
/// otherwise BADs, POORs, BAD heads and empty POORs break it. The combos
/// are counted in time order, an empty POOR breaking the combo. Events a
/// press or a release causes carry it as their origin; a tap's press is
/// 2 ms early or late in turn.
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
                presses: 0,
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
                    out.push(by_input(
                        t,
                        JudgeEventKind::PlaySound {
                            lane,
                            sound: SoundId(0),
                        },
                    ));
                    let k = usize::try_from(t / (50 * MS)).unwrap();
                    let judgment = grades[k % if clean { 3 } else { 5 }];
                    let side = if k.is_multiple_of(2) { -2 } else { 2 };
                    if judgment == Judgment::Poor {
                        out.push(ev(t, judged(judgment, None, None)));
                    } else {
                        let deviation = Some(TimeUs(side * MS));
                        out.push(by_input(t, judged(judgment, deviation, None)));
                    }
                    if judgment == Judgment::Poor {
                        out.push(by_input(
                            t + 10 * MS,
                            JudgeEventKind::EmptyPoor {
                                lane,
                                deviation: TimeUs(10 * MS),
                                presses: 0,
                            },
                        ));
                    }
                }
                NoteKind::Long { end, .. } => {
                    if !clean && (t / (200 * MS)) % 3 == 0 {
                        let head = judged(Judgment::Bad, Some(TimeUs(-20 * MS)), None);
                        out.push(by_input(t - 20 * MS, head));
                        out.push(ev(
                            t + 100 * MS,
                            JudgeEventKind::LnHoldEnded { lane, note: n },
                        ));
                    } else {
                        out.push(by_input(
                            t,
                            JudgeEventKind::LnHeld {
                                lane,
                                note: n,
                                judgment: Judgment::Great,
                                deviation: TimeUs(0),
                                presses: 0,
                            },
                        ));
                        // With breaks, the recent timing's long-note arms:
                        // a head 40 ms early farther than a release 30 ms
                        // early (+40), and a release 130 ms early leaving
                        // the note BAD (+130, not the head's +140); each
                        // 30 ms clear of the next judgment, so a frame
                        // reads it (`Witness::recent`).
                        // The first two are released, the last held to its
                        // end and judged by the deadline.
                        let (at, done) = match (t / (200 * MS)) % 3 {
                            1 if !clean => (
                                end.0 - 30 * MS,
                                judged(
                                    Judgment::Good,
                                    Some(TimeUs(-40 * MS)),
                                    Some(TimeUs(-30 * MS)),
                                ),
                            ),
                            2 if !clean => (
                                end.0 - 130 * MS,
                                judged(
                                    Judgment::Bad,
                                    Some(TimeUs(-140 * MS)),
                                    Some(TimeUs(-130 * MS)),
                                ),
                            ),
                            _ => (
                                end.0,
                                judged(Judgment::Perfect, Some(TimeUs(0)), Some(TimeUs(0))),
                            ),
                        };
                        let made = if at == end.0 { ev } else { by_input };
                        out.push(made(at, JudgeEventKind::LnHoldEnded { lane, note: n }));
                        out.push(made(at, done));
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

/// A track of three keys at x near `i`, growing wider and flatter; every
/// 5th turns, and every 4th jumps at 100 ms with a fourth key there.
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
    let mut keys = vec![
        key(0, x, 0),
        key(100 * MS, x + 10, 255),
        key(500 * MS, x + 40, 128),
    ];
    if i % 4 == 3 {
        keys.insert(2, key(100 * MS, x + 20, 200));
    }
    Track::new(&keys, ease, end).unwrap()
}

/// Images drawn by the elements on `when` timers, after one image per
/// built-in timer.
const WHEN_IMAGES: u16 = 5;
/// The first lane part's image, after the elements' images.
const PARTS: u16 = timer::BUILTIN + WHEN_IMAGES;
/// The lane parts' images from [`PARTS`] on: the note (a cycle of 4 frames
/// on `ready`), head, tail, active and inactive body, mine, bar line,
/// judgment line, SUDDEN+ (also HIDDEN+) and lift; then the scratch
/// beam's.
const NOTE: u16 = PARTS;
const BODY_ON: u16 = PARTS + 3;
const SUDDEN: u16 = PARTS + 8;
const LIFT: u16 = PARTS + 9;
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
/// The image choices' own images: 12, 20 and 24 frames, each with a sheet
/// cycle the choices do not use.
const CHOICES: u16 = FLICKER + 6;
/// The progress slider's image.
const PROGRESS: u16 = CHOICES + 3;
const IMAGES: u16 = PROGRESS + 1;
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

/// A frame as a map key: its uv rectangle and its layer.
fn uv_key(uv: [f32; 4], layer: u16) -> ([u32; 4], u16) {
    (uv.map(f32::to_bits), layer)
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
    shows: HashMap<([u32; 4], u16), (u16, u16)>,
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
    } else if k == COMBO_24 || k == CHOICES + 2 {
        24
    } else if k == CHOICES {
        12
    } else if k == CHOICES + 1 {
        20
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
            line: Some(part(7)),
        },
        bar_groups: vec![skin_model::BarGroupDef {
            image: part(6),
            ..lanes_support::bar_group(879, 2)
        }],
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
    // Digit offsets on the first `n` cells, signed, some growing and some
    // shrinking past zero.
    let moves = |n: i32| {
        (0..n)
            .map(|j| DigitOffset {
                dx: 3 * j - 4,
                dy: 5 - 2 * j,
                dw: j % 3 - 1,
                dh: 2 - j,
            })
            .collect::<Vec<_>>()
    };
    let strip = |image: u16, set, cells, align, pad, space, offsets: i32| DigitsDef {
        image: u32::from(image),
        set,
        cells,
        align,
        pad,
        space,
        offsets: moves(offsets),
    };
    let numbers = vec![
        (
            strip(DIGITS_10, GlyphSet::Ten, 4, Align::Right, Pad::Blank, 2, 2),
            "combo * 100",
        ),
        (
            strip(
                DIGITS_11,
                GlyphSet::Eleven,
                6,
                Align::Left,
                Pad::Glyph,
                -3,
                0,
            ),
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
                5,
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
                11,
            ),
            "if combo % 2 == 0 then -2147483647 - 1 else combo",
        ),
        (
            strip(DIGITS_10, GlyphSet::Ten, 3, Align::Center, Pad::Zero, 4, 1),
            "bp + count.bad + count.early.poor + count.late.poor - 3",
        ),
    ];
    let places = [
        Place {
            region: Region::Playfield,
            bind: Bind::Lanes,
            pivot: Anchor::Center,
            fill: Fill::None,
            native: Some(Native::NoLarger([4, 4])),
            follow: Follow::NONE,
            angle: skin_model::AngleFollow::None,
        },
        Place {
            region: Region::Left,
            pivot: Anchor::BottomRight,
            ..Place::SCREEN
        },
        Place {
            region: Region::Top,
            pivot: Anchor::Top,
            native: Some(Native::Exact([3, 5])),
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
            Place {
                follow: FOLLOWS[usize::from(i) % FOLLOWS.len()],
                angle: skin_model::AngleFollow::None,
                ..place
            },
        ));
    }
    let numbers = numbers
        .into_iter()
        .map(|(digits, src)| NumberDef {
            digits,
            value: int(src),
        })
        .collect();
    let gauges = gauges();
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
                follow: FOLLOWS[usize::from(i) % FOLLOWS.len()],
                ..Place::SCREEN
            },
        ));
    }
    let combos = [
        strip(COMBO_10, GlyphSet::Ten, 4, Align::Center, Pad::Blank, 0, 4),
        strip(
            COMBO_11,
            GlyphSet::Eleven,
            5,
            Align::Right,
            Pad::Glyph,
            2,
            3,
        ),
        strip(
            COMBO_24,
            GlyphSet::TwentyFour,
            11,
            Align::Left,
            Pad::Zero,
            -2,
            11,
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
                native: (k % 2 == 1).then_some(Native::Exact([2, 3])),
                follow: FOLLOWS[usize::from(k) % FOLLOWS.len()],
                // Slots 0, 3 and 6 attach their combos to a wider rect
                // timed as the paint, slot 3's native.
                attachment: (k % 3 == 0).then(|| {
                    let raw = |at: i64, y: i32| {
                        let mut key = grade_key(at, y);
                        key.props[0] = Some(780);
                        key.props[2] = Some(240);
                        key
                    };
                    Track::new(&[raw(0, 510), raw(200 * MS, 490)], Ease::In, End::Hold).unwrap()
                }),
            });
            j.combos[s.index()] = Some(ComboPart {
                digits: combos[s.index() % 3].clone(),
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
                native: (k == 1).then_some(Native::NoLarger([1, 1])),
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
        follow: Follow::COVER,
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
    let image_choices = image_choices(elements, int);
    skin_model::WidgetDefs {
        numbers,
        gauges,
        judges: vec![judge(true), judge(false)],
        texts: vec![
            text(
                TextValue::FieldOr {
                    field: TextField::Title,
                    empty: "NONE".into(),
                },
                Align::Center,
                Overflow::Shrink,
            ),
            text(
                TextValue::Literal("MAX".into()),
                Align::Right,
                Overflow::Spill,
            ),
        ],
        image_choices,
        cover_paints: Vec::new(),
    }
}

/// The gauges: one in each motion, the flicker's with its own images.
fn gauges() -> Vec<GaugeDef> {
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
    vec![
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
    ]
}

/// The image choice tables [`image_choices`] draws.
fn image_choice_tables(
    int: impl Fn(&str) -> skin_model::Program,
) -> Vec<skin_model::ImageChoiceDef> {
    use skin_model::{ImageChoiceDef, ImageChoicePart, ImageSpan};
    let part = |image: u16, first: u16, frames: u16| ImageChoicePart {
        span: ImageSpan {
            image: u32::from(image),
            first,
            frames,
        },
        cycle: None,
        track: None,
        native: None,
    };
    let cycle = |ms: i64, t: u16| {
        Some(CycleDef {
            period: TimeUs(ms * MS),
            timer: TimerRef::Builtin(t),
        })
    };
    let combo = ImageChoiceDef {
        value: int("elapsed(ready) / 40 % 7 - 2"),
        past_table: skin_model::PastTable::Hide,
        choices: vec![
            Some(ImageChoicePart {
                cycle: cycle(300, timer::READY),
                ..part(CHOICES, 5, 7)
            }),
            None,
            Some(ImageChoicePart {
                cycle: cycle(170, timer::JUDGE),
                track: Some(track(900, Ease::In, End::Loop { from: TimeUs(0) })),
                ..part(CHOICES + 1, 10, 10)
            }),
            Some(ImageChoicePart {
                native: Some(Native::NoLarger([6, 9])),
                ..part(CHOICES, 3, 1)
            }),
        ],
    };
    let looks = ImageChoiceDef {
        value: int(GAUGE_LOOK),
        past_table: skin_model::PastTable::Hide,
        choices: (0..6)
            .map(|k| {
                Some(ImageChoicePart {
                    track: Some(track(950 + k, Ease::Out, End::Hold)),
                    cycle: (k % 2 == 0).then_some(CycleDef {
                        period: TimeUs(90 * MS),
                        timer: TimerRef::Builtin(timer::PLAY),
                    }),
                    ..part(CHOICES + 2, 4 * k, 4)
                })
            })
            .collect(),
    };
    let tempo = ImageChoiceDef {
        value: int("if bpm.current_valid then bpm.current_x100 / 100 % 3 else -1"),
        past_table: skin_model::PastTable::Hide,
        choices: vec![Some(part(GRADES, 0, 1)), Some(part(GRADES + 1, 0, 1)), None],
    };
    vec![combo, looks, tempo]
}

/// The selector of the shown gauge's look, invalid hiding.
const GAUGE_LOOK: &str = "if gauge.valid then (if gauge.kind == assist_easy then 0 \
    else if gauge.kind == easy then 1 else if gauge.kind == normal then 2 \
    else if gauge.kind == hard then 3 else if gauge.kind == ex_hard then 4 \
    else if gauge.kind == hazard then 5 else -1) else -1";

/// The image choice tables, their elements appended to `elements`:
/// - by `elapsed(ready) / 40 % 7 − 2`: hidden, each entry (a hole among them) and past
///   the table, the entries on their own clocks (`ready`, `judge`), one
///   with its own track and one with its own native size, following each
///   way in turn;
/// - by the gauge's look, one entry per look with its own track, shared
///   by two elements;
/// - by the current tempo, while it is known.
fn image_choices(
    elements: &mut Vec<ElementDef>,
    int: impl Fn(&str) -> skin_model::Program,
) -> Vec<skin_model::ImageChoiceDef> {
    let tables = image_choice_tables(int);
    for (i, (table, place)) in [
        (0, Place::SCREEN),
        (
            0,
            Place {
                region: Region::Playfield,
                bind: Bind::Lanes,
                pivot: Anchor::Center,
                ..Place::SCREEN
            },
        ),
        (
            1,
            Place {
                region: Region::Left,
                pivot: Anchor::Right,
                ..Place::SCREEN
            },
        ),
        (1, Place::SCREEN),
        (2, Place::SCREEN),
    ]
    .into_iter()
    .enumerate()
    {
        let i = u16::try_from(i).unwrap();
        elements.push(widget(
            Source::ImageChoice(table),
            track(980 + 5 * i, Ease::Linear, End::Hold),
            TimerRef::Builtin(timer::READY),
            Place {
                follow: FOLLOWS[usize::from(i) % FOLLOWS.len()],
                angle: skin_model::AngleFollow::None,
                ..place
            },
        ));
    }
    tables
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
        "rate.current_bp >= 5000 && rate.whole_bp / 100 < 100",
        "gauge.valid && (gauge.kind == hard || gauge.kind == ex_hard)",
    ];
    let whens: Vec<_> = (0..32)
        .map(|i| bool_program(conditions[i % conditions.len()]))
        .collect();
    let visible = [
        bool_program("combo % 2 == 0"),
        bool_program("bp < 1000 && rate.current_bp % 100 < 100"),
        bool_program("on(play) && !(gauge.valid && gauge.kind == normal)"),
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
            // Half the loops start before 0, the time wrapped in `i128`.
            _ if i % 2 == 0 => End::Loop {
                from: TimeUs(-50 * MS),
            },
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
            // Every third group multiplies; its tracks start at alpha 0,
            // which a multiply quad skips.
            blend: match (i / 3) % 3 {
                0 => Blend::Alpha,
                1 => Blend::Add,
                _ => Blend::Multiply,
            },
            clip: (i % 11 == 0).then_some(ClipId(i % 4)),
            place: Place {
                region,
                pivot: Anchor::ALL[usize::from(i % 9)],
                bind,
                fill: Fill::None,
                native: match i % 5 {
                    1 => Some(Native::NoLarger([3, 2])),
                    3 => Some(Native::Exact([2, 2])),
                    _ => None,
                },
                follow: FOLLOWS[usize::from(i / 4) % FOLLOWS.len()],
                angle: skin_model::AngleFollow::None,
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
    // The lanes hide on some frames; their followers still move.
    elements.push(ElementDef {
        visible: Some(bool_program("combo % 5 != 1")),
        ..lanes_support::lanes_element()
    });
    // HIDDEN+ (drawn while its amount is above 0, trimmed at the line as
    // it rises), then SUDDEN+ painted apart: following the cover on
    // `judge`, an additive image between, a slider hidden on some frames
    // and a choice of table 0 following both with a slider; then LIFT,
    // trimmed at the lanes' bottom by its own gate track.
    elements.push(ElementDef {
        visible: Some(bool_program("hidden.permille > 0")),
        ..lanes_support::paint(3, [160, 880, 544, 800], Follow::COVER)
    });
    elements.push(ElementDef {
        timer: TimerRef::Builtin(timer::JUDGE),
        ..lanes_support::paint(0, [160, -720, 544, 800], Follow::COVER)
    });
    elements.push(ElementDef {
        blend: Blend::Add,
        ..lanes_support::image(u32::from(timer::READY), [160, 0, 544, 40], Place::SCREEN)
    });
    elements.push(ElementDef {
        visible: Some(bool_program("combo % 3 != 1")),
        ..lanes_support::paint(1, [160, 0, 544, 80], Follow::NONE)
    });
    elements.push(lanes_support::paint(
        2,
        [160, -400, 544, 480],
        Follow::LIFT_AND_COVER,
    ));
    elements.push(lanes_support::paint(4, [160, 940, 544, 120], Follow::NONE));
    elements.push(lanes_support::paint(5, [160, 1_040, 24, 24], Follow::NONE));
    let beam = lanes_support::bound(Bind::Lane(Lane::SCRATCH));
    elements.push(ElementDef {
        blend: Blend::Add,
        ..lanes_support::image(u32::from(BEAM), [152, 824, 112, 112], beam)
    });
    let mut widgets = widgets(&mut elements);
    widgets.cover_paints = cover_paints();
    load(whens, elements, widgets)
}

/// The cover paints: SUDDEN+, LIFT with a slider, a choice of table 0,
/// HIDDEN+ trimmed at the line, LIFT trimmed at the lanes' bottom by its
/// own gate track.
fn cover_paints() -> Vec<CoverPaintDef> {
    vec![
        lanes_support::moving(u32::from(SUDDEN), CoverMotion::None, CoverTrim::None),
        CoverPaintDef {
            slider: Some(skin_model::Slider::sudden(300)),
            ..lanes_support::moving(u32::from(LIFT), CoverMotion::None, CoverTrim::None)
        },
        CoverPaintDef {
            image: CoverImage::Choice(0),
            slider: Some(skin_model::Slider::sudden(0)),
            ..lanes_support::moving(0, CoverMotion::None, CoverTrim::None)
        },
        lanes_support::moving(u32::from(SUDDEN), CoverMotion::Hidden, CoverTrim::Lift(880)),
        CoverPaintDef {
            gate_track: Some(lanes_support::rect([160, 960, 544, 80])),
            ..lanes_support::moving(u32::from(LIFT), CoverMotion::Lift, CoverTrim::Fixed(960))
        },
        CoverPaintDef {
            slider: Some(skin_model::Slider {
                range: 400,
                rate: skin_model::SliderRate::Progress,
                direction: skin_model::SliderDirection::Right,
            }),
            ..lanes_support::moving(u32::from(PROGRESS), CoverMotion::None, CoverTrim::None)
        },
    ]
}

/// A skin over [`images`] with the regions, the lanes, `whens`, `elements`
/// and `widgets`, its atlas built, and every frame's image.
fn load(
    whens: Vec<skin_model::Program>,
    elements: Vec<ElementDef>,
    widgets: skin_model::WidgetDefs,
) -> Loaded {
    let (_dir, sheet) = images();
    let skin = Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: regions(),
        whens,
        sheet,
        lanes: Some(lanes()),
        elements,
        widgets,
        bomb_up_to: Some(skin_model::Grade::Bad),
        close: None,
        loadend: None,
        present: None,
    })
    .unwrap();
    // A page of 256 holds only some of the frames.
    let limits = AtlasLimits {
        max_side: 256,
        max_layers: 16,
        reserved_layers: u32::from(RESERVED),
    };
    // The sheet's pages alone: a text layer would need a side of 2048.
    let atlas = skin.sheet().atlas(Variant::Lo, limits, &|| false).unwrap();
    assert!(atlas.pages().len() >= 2, "{} pages", atlas.pages().len());
    let (_, uvs) = atlas.into_parts();
    // Frames run image by image: image k's frames follow image k - 1's.
    let mut shows = HashMap::new();
    let mut parts = HashMap::new();
    let mut next = 0;
    for k in 0..IMAGES {
        for f in 0..frames_of(k) {
            let frame = uvs.get(next).unwrap();
            assert!(
                shows
                    .insert(uv_key(frame.uv, frame.layer), (k, f))
                    .is_none()
            );
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

/// A skin for the loading runtime: its structural widgets untimed, on
/// `screen`, under conditions loading can read. A background and an image
/// on `loading`; the lanes while the cover is short of the whole lane; the
/// gauges in every motion, two of them only with a descriptor; every
/// cover paint, HIDDEN+'s while its amount is above 0; the gauge look's
/// image choice; and images on `when` timers on the progress, the
/// descriptor and the covers. The play fixture's other widgets are defined
/// but not drawn.
fn loading_skin() -> Loaded {
    // 32, as the sheet's cycles name: on the progress, the descriptor,
    // the covers, the chosen chart's difficulty and its BGA flag in turn
    // (the last two read while the frame gives them).
    let conditions = [
        "load.percent >= 50",
        "gauge.valid && gauge.kind == hard",
        "cover.enabled && lift.permille > 0",
        "chart.difficulty_valid && chart.difficulty == 3",
        "chart.bga_valid && !chart.bga",
    ];
    let whens = (0..32)
        .map(|i| bool_program(conditions[i % conditions.len()]))
        .collect();
    let untimed =
        |source, track, place| widget(source, track, TimerRef::Builtin(timer::SCREEN), place);
    let still = || track(0, Ease::Linear, End::Hold);
    let mut elements = vec![
        ElementDef {
            timer: TimerRef::Builtin(timer::SCREEN),
            ..background(timer::SCREEN, Fill::Cover)
        },
        ElementDef {
            timer: TimerRef::Builtin(timer::LOADING),
            ..background(timer::LOADING, Fill::Stretch)
        },
        ElementDef {
            visible: Some(bool_program("cover.permille < 1000")),
            timer: TimerRef::Builtin(timer::SCREEN),
            ..lanes_support::lanes_element()
        },
    ];
    for i in 0_u16..5 {
        elements.push(ElementDef {
            visible: (i % 2 == 1).then(|| bool_program("gauge.valid")),
            ..untimed(
                Source::Gauge(u32::from(i)),
                track(400 + 11 * i, Ease::Out, End::Hold),
                Place {
                    pivot: Anchor::ALL[usize::from(i)],
                    follow: FOLLOWS[usize::from(i) % FOLLOWS.len()],
                    ..Place::SCREEN
                },
            )
        });
    }
    for (paint, r, follow) in [
        (0, [160, -720, 544, 800], Follow::COVER),
        (1, [160, 0, 544, 80], Follow::NONE),
        (2, [160, -400, 544, 480], Follow::LIFT_AND_COVER),
        (3, [160, 880, 544, 800], Follow::COVER),
        (4, [160, 940, 544, 120], Follow::NONE),
    ] {
        elements.push(ElementDef {
            visible: (paint == 3).then(|| bool_program("hidden.permille > 0")),
            timer: TimerRef::Builtin(timer::SCREEN),
            ..lanes_support::paint(paint, r, follow)
        });
    }
    elements.push(untimed(Source::ImageChoice(1), still(), Place::SCREEN));
    for w in 0_u16..3 {
        elements.push(widget(
            Source::Image(u32::from(timer::BUILTIN + w)),
            still(),
            TimerRef::When(w),
            Place::SCREEN,
        ));
    }
    let int = |src: &str| {
        skin_model::compile(
            src,
            &skin_model::Scope::new(&skin_model::PLAY),
            skin_model::Ty::Int,
        )
        .unwrap()
    };
    elements.push(untimed(Source::Number(0), still(), Place::SCREEN));
    let widgets = skin_model::WidgetDefs {
        gauges: gauges(),
        image_choices: image_choice_tables(int),
        cover_paints: cover_paints(),
        numbers: vec![NumberDef {
            digits: DigitsDef {
                image: u32::from(DIGITS_10),
                set: GlyphSet::Ten,
                cells: 4,
                align: Align::Right,
                pad: Pad::Blank,
                space: 0,
                offsets: Vec::new(),
            },
            value: int("chart.level"),
        }],
        ..skin_model::WidgetDefs::default()
    };
    load(whens, elements, widgets)
}

/// What a stretch saw.
/// What a lane's visual state read, a [`Witness::lane_states`] entry.
#[derive(Clone, Copy)]
enum LaneArm {
    /// A code, set by a press-caused effect at the lane's latest press.
    KeptAtPress,
    /// A judgment's or an empty POOR's code, early and late.
    Early,
    Late,
    /// A held long note's.
    Held,
    /// An empty POOR's.
    EmptyPoor,
    /// 0, after a press later than the effect that set it.
    LaterPress,
    /// 0, after a press in the µs of the effect that set it, past the
    /// presses it counted (a reversal's press after its release).
    PressLaterInItsUs,
}

impl LaneArm {
    const COUNT: usize = 7;
}

/// One lane's input and effects as the frames feed them: when its press
/// is released, and the time of the effect that last set its visual
/// state, with the presses before it in its µs.
struct LaneTrack {
    lane: Lane,
    release_at: Option<i64>,
    set: Option<(TimeUs, u32)>,
}

struct Witness {
    split_drains: u32,
    small_drops: u32,
    other_drops: u32,
    /// Per built-in slot: whether it was on at a frame.
    on: [bool; timer::BUILTIN as usize],
    /// Per built-in slot: whether an element on it reached the batch.
    drawn: [bool; timer::BUILTIN as usize],
    when_started: bool,
    /// Whether the current rate differed from the whole rate and from its
    /// empty value, and each gauge kind read.
    rates_apart: bool,
    kinds: [bool; GaugeLook::COUNT],
    /// The frames whose recent timing read +40 (a long note's head
    /// farther than its release) and +130 (an early release leaving it
    /// BAD).
    recent: [u32; 2],
    /// The frames where a lane's visual state read, by
    /// [`LaneArm`]: a code kept at the press that set it, an early and a
    /// late code, a held long note's, an empty POOR's, 0 after a later
    /// press, and 0 after a press in the µs of the release that set it.
    lane_states: [u32; LaneArm::COUNT],
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
    /// The literal's y at the first target and the author's layout,
    /// and whether the covers moved it there.
    literal: Option<f32>,
    literal_moved: bool,
    /// Per image choice image: whether a quad showed it, and whether it
    /// showed a frame past its entry's first.
    choices: [bool; 3],
    choice_later: bool,
    /// SUDDEN+'s paint drew while HIDDEN+, which shares its image, was
    /// off; the slider paint's y at the first target and the author's
    /// layout while LIFT, which shares its image, was off, and whether it
    /// moved there.
    paint: bool,
    slider: Option<f32>,
    slider_moved: bool,
    /// The progress slider's x at the first target and the author's
    /// layout, whether it moved there, and whether the progress read 0,
    /// a share between and the whole.
    progress: Option<f32>,
    progress_moved: bool,
    progress_reads: [bool; 3],
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
            let Some(&(image, frame)) = loaded.shows.get(&uv_key(q.uv, q.layer)) else {
                continue;
            };
            match image {
                COMBO_10 | COMBO_11 | COMBO_24 => combo = true,
                DIGITS_24 if frame % 12 == 11 => self.sign = true,
                FLICKER..CHOICES => flicker += 1,
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
            rates_apart: false,
            recent: [0; 2],
            lane_states: [0; LaneArm::COUNT],
            kinds: [false; GaugeLook::COUNT],
            empty_frames_after_ready: 0,
            later_frame: false,
            later_layer: false,
            notes: false,
            active_body: false,
            turned: false,
            beam: [None; TARGETS.len()],
            literal: None,
            literal_moved: false,
            choices: [false; 3],
            choice_later: false,
            paint: false,
            slider: None,
            slider_moved: false,
            progress: None,
            progress_moved: false,
            progress_reads: [false; 3],
        }
    }
}

/// What a stretch's frames cycle through besides the target.
struct Views {
    layouts: Vec<SkinLayout>,
    lanes: [LaneView; 6],
}

impl Views {
    /// The author's layout first, then a moved and scaled playfield, a
    /// hidden judge, a lane width, an inset and a HUD pushed off the
    /// canvas; no covers, a cover, a lift, both, all three covers, and
    /// all three past 1 000 ‰.
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
        let view = |cover, lift, hidden| LaneView {
            gain: Gain::new(8, 3),
            cover,
            lift,
            hidden,
        };
        Self {
            layouts: vec![SkinLayout::AUTHOR, moved, hidden, wide, inset, pushed],
            lanes: [
                view(0, 0, 0),
                view(250, 0, 0),
                view(0, 100, 0),
                view(950, 300, 0),
                view(300, 200, 400),
                view(1_500, 1_200, 2_000),
            ],
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
    chart: &Chart,
    notes: &mut NoteFacts,
    events: &[JudgeEvent],
    tally: &mut Tally,
    gauges: &mut [GaugeState],
    batch: &mut QuadBatch,
    small: &mut QuadBatch,
    from: i64,
    to: i64,
    counted: bool,
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
    let mut input = eval::InputFacts::NONE;
    let mut lanes: [LaneTrack; 8] = std::array::from_fn(|i| LaneTrack {
        lane: Lane::new(u8::try_from(i).unwrap()).unwrap(),
        release_at: None,
        set: None,
    });
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
        // The snapshot follows the drain, its note facts too.
        for e in drain {
            play_support::follow(notes, chart, e);
            match e.kind {
                JudgeEventKind::Judged {
                    combo, judgment, ..
                } => {
                    tally.combo = combo;
                    tally.max_combo = tally.max_combo.max(combo);
                    let c = &mut tally.counts;
                    match judgment {
                        Judgment::Perfect => {
                            c.perfect += 1;
                            tally.ex_score += 2;
                        }
                        Judgment::Great => {
                            c.great += 1;
                            tally.ex_score += 1;
                        }
                        Judgment::Good => c.good += 1,
                        Judgment::Bad => c.bad += 1,
                        Judgment::Poor => c.poor += 1,
                    }
                }
                JudgeEventKind::EmptyPoor { .. } => {
                    tally.empty_poor += 1;
                    if counted {
                        tally.counts.poor += 1;
                    }
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
        let view = views.lanes[(frame_no / 23) % views.lanes.len()];
        // Lane input following the drained effects, so key-on and key-off
        // both run: each press its effects carry, released 5 ms later; a
        // long note held to its release, every other one reversed there (a
        // release, then a press in its µs); and every 11th frame a press
        // that judges nothing on a lane not held.
        for l in lanes.iter_mut() {
            if l.release_at.is_some_and(|r| r <= now) {
                input.release(l.lane, TimeUs(l.release_at.take().unwrap_or(now)));
            }
        }
        for e in drain {
            let at = e.time;
            let (lane, press, release, sets, reversed) = match e.kind {
                JudgeEventKind::Judged {
                    lane,
                    note,
                    judgment,
                    release,
                    presses,
                    ..
                } => {
                    let released = e.origin.is_some() && release.is_some();
                    let pressed = e.origin.is_some() && release.is_none();
                    let sets = (judgment != Judgment::Poor).then_some(presses);
                    (lane, pressed, released, sets, note % 2 == 0)
                }
                JudgeEventKind::LnHeld { lane, presses, .. }
                | JudgeEventKind::EmptyPoor { lane, presses, .. } => {
                    (lane, true, false, Some(presses), false)
                }
                _ => continue,
            };
            let Some(l) = lanes.get_mut(lane.index()) else {
                continue;
            };
            if press {
                if input.lane(lane).held {
                    input.release(lane, at);
                }
                input.press(lane, at);
                let long = matches!(e.kind, JudgeEventKind::LnHeld { .. });
                l.release_at = (!long).then_some(at.0 + 5 * MS);
            }
            if release {
                input.release(lane, at);
                if reversed {
                    input.press(lane, at);
                    l.release_at = Some(at.0 + 5 * MS);
                }
            }
            if let Some(presses) = sets {
                l.set = Some((at, presses));
            }
        }
        if frame_no.is_multiple_of(11) {
            let lane = bms_model::Lane::all()
                .nth((frame_no / 11) % 8)
                .unwrap_or(bms_model::Lane::SCRATCH);
            if !input.lane(lane).held
                && let Some(l) = lanes.get_mut(lane.index())
            {
                input.press(lane, TimeUs(now));
                l.release_at = Some(now + 5 * MS);
            }
        }
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            now: TimeUs(now),
            display_offset: offset,
            tally,
            gauges,
            gauge: shown,
            effects: drain,
            notes,
            offsets: skin_model::Offsets::NONE,
            input: &input,
            complete: now > last,
            target: TARGETS[target_no],
            layout: &views.layouts[layout_no],
            turntable: 0,
            lanes: view,
            readouts: stretch_readouts(frame_no, view),
            playing: true,
            // A Closing that starts with the shown survival gauge's
            // failure, drawn while that gauge shows.
            closing: gauges[1].failed_at.filter(|_| shown == 1),
        };
        let use_small = frame_no % 100 == 99;
        let out = if use_small { &mut *small } else { &mut *batch };
        out.clear();
        skin.build(play, &frame, &loaded.uvs, out)
            .expect("the frame builds");
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
                if q.uv == LITERAL_UV && target_no == 0 && layout_no == 0 {
                    let y = *w.literal.get_or_insert(q.rect[1]);
                    w.literal_moved |= y != q.rect[1];
                }
                match loaded.parts.get(&part_key(q.uv, q.layer)) {
                    Some(&NOTE) => w.notes = true,
                    Some(&SUDDEN) if view.hidden == 0 => w.paint = true,
                    Some(&LIFT) if view.lift == 0 && target_no == 0 && layout_no == 0 => {
                        let y = *w.slider.get_or_insert(q.rect[1]);
                        w.slider_moved |= y != q.rect[1];
                    }
                    Some(&BODY_ON) => w.active_body = true,
                    Some(&BEAM) if layout_no == 0 => {
                        w.beam[target_no].get_or_insert(q.rect[0]);
                    }
                    _ => {}
                }
                let Some(&(image, frame)) = loaded.shows.get(&uv_key(q.uv, q.layer)) else {
                    continue;
                };
                if let Some(d) = w.drawn.get_mut(usize::from(image)) {
                    *d = true;
                }
                if image == PROGRESS && target_no == 0 && layout_no == 0 {
                    let x = *w.progress.get_or_insert(q.rect[0]);
                    w.progress_moved |= x != q.rect[0];
                }
                if let Some(c) = image
                    .checked_sub(CHOICES)
                    .and_then(|c| w.choices.get_mut(usize::from(c)))
                {
                    *c = true;
                    // The entries start at frames 5, 10 and multiples of 4.
                    w.choice_later |= !matches!(frame, 5 | 10) && !frame.is_multiple_of(4);
                }
                w.later_frame |= frame > 0;
                w.later_layer |= q.layer > RESERVED;
            }
        }
        for (slot, on) in (0..).zip(w.on.iter_mut()) {
            *on |= play.timer(slot).is_some();
        }
        w.when_started |= play.timer(timer::BUILTIN).is_some();
        let fields = play.state().fields;
        let read = |slot: u16| fields.get(usize::from(slot)).copied();
        let (current, whole) = (read(field::RATE_CURRENT_BP), read(field::RATE_WHOLE_BP));
        w.rates_apart |= current != whole && current != Some(10_000);
        match (
            read(field::JUDGE_RECENT_MS),
            read(field::JUDGE_RECENT_IMPERFECT),
        ) {
            (Some(40), Some(1)) => w.recent[0] += 1,
            (Some(130), Some(1)) => w.recent[1] += 1,
            _ => {}
        }
        let mut seen = [false; LaneArm::COUNT];
        for l in &lanes {
            let v = read(field::lane_judge(l.lane)).unwrap_or(0);
            let pressed = input.lane(l.lane).pressed;
            let mut arm = |a: LaneArm| seen[a as usize] = true;
            match v {
                2 | 4 | 6 | 10 => arm(LaneArm::Early),
                3 | 5 | 7 | 11 => arm(LaneArm::Late),
                _ => {}
            }
            match v {
                8 => arm(LaneArm::Held),
                10 | 11 => arm(LaneArm::EmptyPoor),
                _ => {}
            }
            let presses = input.lane(l.lane).presses;
            match (v, l.set, pressed) {
                (1.., Some((at, 1..)), Some(p)) if p == at => arm(LaneArm::KeptAtPress),
                (0, Some((at, _)), Some(p)) if p > at => arm(LaneArm::LaterPress),
                (0, Some((at, n)), Some(p)) if p == at && presses > n => {
                    arm(LaneArm::PressLaterInItsUs);
                }
                _ => {}
            }
        }
        for (n, s) in w.lane_states.iter_mut().zip(seen) {
            *n += u32::from(s);
        }
        if let Some(seen) = read(field::GAUGE_KIND)
            .and_then(|k| usize::try_from(k).ok())
            .and_then(|k| w.kinds.get_mut(k))
        {
            *seen = true;
        }
        // The progress's arms: before the start, between, at the end.
        match read(field::TIME_PROGRESS_PPM) {
            Some(0) => w.progress_reads[0] = true,
            Some(skin_model::PROGRESS_FULL) => w.progress_reads[2] = true,
            Some(_) => w.progress_reads[1] = true,
            None => {}
        }
        frame_no += 1;
        now += 16 * MS;
    }
}

/// A carried round's score data: distinct values per field, so a swapped
/// field shows.
const CARRIED_SCORE: skin_model::Settled = skin_model::Settled {
    point: 200_000,
    ex_score: 6,
    rate_current_bp: 9_000,
    rate_whole_bp: 4_285,
};

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
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: &descs,
        attempt: Attempt::Full {
            lead_in: TimeUs(lead_in),
        },
        empty_poor_breaks_combo: true,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
        score: skin_model::PlayScore::NONE,
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
    // right, a round played to a full combo on the left again, its
    // empty POORs counted, and the whole chart again by autoplay.
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
                empty_poor_counted: true,
                ..full
            },
            effects(&chart, Some(&clean_round), true),
            8_000 * MS,
        ),
        (
            "autoplay",
            PlayContext {
                autoplay: true,
                meta: skin_model::ChartMetadata {
                    difficulty: Some(3),
                    judge_class: Some(1),
                    total: Some(300),
                    bga: Some(true),
                },
                ..full
            },
            effects(&chart, None, true),
            -lead_in,
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
        let counted = ctx.empty_poor_counted;
        let mut notes = NoteFacts::new(&chart);

        TRACK.with(|t| t.set(true));
        // The count works: an allocation that escapes is counted in any
        // profile.
        drop(std::hint::black_box(vec![1u8; 8]));
        assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 1, "{name}");
        stretch(
            &loaded,
            &views,
            &mut play,
            &chart,
            &mut notes,
            events,
            &mut tally,
            &mut gauges,
            &mut batch,
            &mut small,
            *from,
            to,
            counted,
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
        assert!(w.literal_moved, "{name}: the covers never moved a follower");
        assert!(w.paint, "{name}: SUDDEN+'s paint never drew");
        assert!(w.slider_moved, "{name}: the slider paint never moved");
        assert!(w.progress_moved, "{name}: the progress slider never moved");
        assert_eq!(w.choices, [true; 3], "{name}: an image choice never drew");
        assert!(w.choice_later, "{name}: no image choice entry animated");
        assert!(w.rates_apart, "{name}: the rates never moved apart");
        assert!(
            w.kinds[GaugeLook::Normal.index()] && w.kinds[GaugeLook::Hard.index()],
            "{name}: the shown gauge's kind never switched"
        );
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
        if matches!(*name, "play" | "round") {
            assert!(
                w.recent.iter().all(|&n| n > 0),
                "{name}: a long note's timing arm never read: {:?}",
                w.recent
            );
            assert!(
                w.lane_states.iter().all(|&n| n > 0),
                "{name}: a lane state's arm never read: {:?}",
                w.lane_states
            );
        }
        if matches!(*name, "clean round" | "autoplay") {
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
        for (a, b) in seen.progress_reads.iter_mut().zip(w.progress_reads) {
            *a |= b;
        }
        seen.widgets.merge(w.widgets);
    }
    // The widgets' witnesses, seen inside the counted stretches.
    let ws = seen.widgets;
    assert_eq!(
        seen.progress_reads, [true; 3],
        "the progress never read 0, a share or the whole"
    );
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
    assert_eq!(beams, [152.0, 600.0, 152.0, 152.0]);
    // Every built-in timer turned on, and an element on it was drawn, in
    // some attempt: the lanes' key-on and key-off from the fed input.
    // `loading` runs only in the loading runtime, which draws an element
    // on it in `loading_does_not_allocate`.
    let slot = usize::from(timer::LOADING);
    assert!(!seen.on[slot] && !seen.drawn[slot], "`loading` in a play");
    seen.on[slot] = true;
    seen.drawn[slot] = true;
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
    timeline_lanes_do_not_allocate(&full);
    fallback_lanes_do_not_allocate(&chart, &full);
    bar_groups_do_not_allocate(&full);
    cover_trims_do_not_allocate(&chart, &full);
    grade_attachments_do_not_allocate(&full);
    large_gauges_do_not_allocate(&full, &views, &descs);
    programs_do_not_allocate(&chart, &full);
    promoted_numerics_do_not_allocate(&chart, &full);
    slots_do_not_allocate(&chart, &full);
    carried_slots_do_not_allocate(&chart, &full);
    app_readouts_do_not_allocate(&chart, &full, &views, &descs);
    // The next attempt, prepared untracked after the stretches, builds.
    let mut play = skin.prepare(&chart, &full).unwrap();
    skin.attach_text(&mut play, text_table()).unwrap();
    let t = tally(0);
    let gauges = [descs[0].start, descs[1].start].map(state);
    batch.clear();
    let notes = NoteFacts::default();
    let frame = SkinFrame {
        app: skin_model::AppReadouts::LAUNCH,
        state_us: 0,
        now: TimeUs(5_000 * MS),
        display_offset: TimeUs(0),
        tally: &t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        notes: &notes,
        offsets: skin_model::Offsets::NONE,
        input: &eval::InputFacts::NONE,
        complete: false,
        target: TARGETS[0],
        layout: &views.layouts[0],
        turntable: 0,
        lanes: views.lanes[0],
        readouts: readouts_for(views.lanes[0]),
        playing: true,
        closing: None,
    };
    skin.build(&mut play, &frame, &loaded.uvs, &mut batch)
        .expect("the frame builds");
    assert!(!batch.quads().is_empty());
    assert_eq!(batch.dropped(), 0);
    // An inconsistent snapshot is refused, and the poisoned play after
    // it, without allocating.
    let counted = PlayContext {
        empty_poor_counted: true,
        ..full
    };
    let mut play = skin.prepare(&chart, &counted).unwrap();
    let bad = Tally {
        empty_poor: 1,
        ..tally(0)
    };
    let refused = |play: &mut Play, t: &Tally, batch: &mut QuadBatch| {
        let frame = SkinFrame { tally: t, ..frame };
        skin.build(play, &frame, &loaded.uvs, batch)
    };
    TRACK.with(|t| t.set(true));
    let first = refused(&mut play, &bad, &mut batch);
    let after = refused(&mut play, &t, &mut batch);
    TRACK.with(|t| t.set(false));
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0, "refused: allocations");
    assert_eq!(first, Err(BuildError::InconsistentTally));
    assert_eq!(after, Err(BuildError::InconsistentTally));
    readouts_do_not_allocate(&loaded, &descs);
    rhythm_does_not_allocate(&descs);
    main_bpm_does_not_allocate(&descs);
    progress_and_length_do_not_allocate(&descs);
    loading_does_not_allocate(&loaded, &views, &descs);
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
        .atlas(Variant::Lo, LIMITS_SMALL, &|| false)
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
        let notes = NoteFacts::default();
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            now: TimeUs(ms * MS),
            display_offset: TimeUs(0),
            tally: &t,
            gauges: &gauges,
            gauge: 0,
            effects: &[],
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target: TARGETS[0],
            layout: &views.layouts[0],
            turntable: 0,
            lanes: views.lanes[0],
            readouts: readouts_for(views.lanes[0]),
            playing: true,
            closing: None,
        };
        bitmap
            .build(&mut play, &frame, &uvs, &mut batch)
            .expect("the frame builds");
        glyphs += batch.quads().len();
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "bitmap text: allocations"
    );
    assert!(glyphs > 0, "no glyph drew");

    // Its loading state's bitmap lines, installed untracked, draw alike.
    let mut loading = bitmap.prepare_loading(Side::Left);
    bitmap.install_loading_bitmap_text(
        &mut loading,
        TextSlots::ALL,
        &ChartText {
            title: "ABAB",
            ..ChartText::default()
        },
    );
    let mut glyphs = 0;
    TRACK.with(|t| t.set(true));
    for ms in (0..2_000).step_by(16) {
        batch.clear();
        let frame = LoadingFrame {
            app: skin_model::AppReadouts::LAUNCH,
            display_offset: TimeUs(0),
            fix_hispeed: 0,
            difficulty: None,
            bga: None,
            length: None,
            autoplay: false,
            now: TimeUs(ms * MS),
            load: LoadState {
                since: TimeUs(0),
                percent: 0,
            },
            target: TARGETS[0],
            layout: &views.layouts[0],
            turntable: 0,
            lanes: views.lanes[0],
            covers: readouts_for(views.lanes[0]).covers,
            gauge: None,
            play_level: None,
        };
        bitmap
            .build_loading(&mut loading, &frame, &uvs, &mut batch)
            .expect("the loading frame builds");
        glyphs += batch.quads().len();
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "loading bitmap text: allocations"
    );
    assert!(glyphs > 0, "no loading glyph drew");
}

/// The loading frame `no` of a lifetime begun at `since`: the clock, the
/// progress, the target, the layout, the lanes and the covers moving, the
/// descriptor of either gauge given on two frames of three, the chosen
/// chart's play level on four of five, its difficulty on six of seven and
/// its BGA flag on three of four, the play autoplay on every other.
fn loading_frame<'a>(
    views: &'a Views,
    descs: &[GaugeDesc],
    since: TimeUs,
    no: usize,
) -> LoadingFrame<'a> {
    let view = views.lanes[(no / 7) % views.lanes.len()];
    LoadingFrame {
        app: skin_model::AppReadouts::LAUNCH,
        display_offset: TimeUs(0),
        fix_hispeed: 0,
        difficulty: (no % 7 != 6).then_some(u8::try_from(no % 6).unwrap()),
        bga: (no % 4 != 3).then_some(no % 8 < 4),
        length: None,
        autoplay: no.is_multiple_of(2),
        now: TimeUs(since.0 + i64::try_from(no).unwrap() * 16 * MS),
        load: LoadState {
            since,
            percent: i32::try_from(no % 120).unwrap(),
        },
        target: TARGETS[(no / 5) % TARGETS.len()],
        layout: &views.layouts[(no / 11) % views.layouts.len()],
        turntable: 0,
        lanes: view,
        covers: readouts_for(view).covers,
        gauge: (no % 3 != 2).then(|| LoadingGauge::of(no % 2, &descs[no % 2])),
        play_level: (no % 5 != 4).then_some(12),
    }
}

/// The builds of a loading state of `loaded`, tracked: 600 frames, then a
/// refused build and one after it, then the state started afresh in place
/// (`Loading::reset`, a new loading lifetime) and built again. `see` is
/// given each frame's number and quads while tracking, up to the refused
/// pair's, and must not allocate. The refused builds' errors.
fn track_loading(
    loaded: &Loaded,
    loading: &mut skin_model::Loading,
    views: &Views,
    descs: &[GaugeDesc],
    mut see: impl FnMut(usize, &[skin_model::Quad]),
) -> [Result<(), BuildError>; 2] {
    let skin = &loaded.skin;
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let since = TimeUs(-5_000 * MS);
    let bad = LoadingFrame {
        covers: CoverSettings {
            cover: 1_001,
            ..readouts_for(views.lanes[0]).covers
        },
        ..loading_frame(views, descs, since, 0)
    };
    TRACK.with(|t| t.set(true));
    for no in 0..600 {
        batch.clear();
        let frame = loading_frame(views, descs, since, no);
        skin.build_loading(loading, &frame, &loaded.uvs, &mut batch)
            .expect("the loading frame builds");
        see(no, batch.quads());
    }
    batch.clear();
    let first = skin.build_loading(loading, &bad, &loaded.uvs, &mut batch);
    let frame = loading_frame(views, descs, since, 1);
    let after = skin.build_loading(loading, &frame, &loaded.uvs, &mut batch);
    see(600, batch.quads());
    loading.reset();
    batch.clear();
    skin.build_loading(loading, &frame, &loaded.uvs, &mut batch)
        .expect("a state started afresh builds");
    TRACK.with(|t| t.set(false));
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0, "loading: allocations");
    [first, after]
}

/// The loading runtime's builds allocate nothing: its first build after
/// preparation and the installation of its lines, later builds as the
/// clock, the progress, the target, the layout, the covers and the gauge
/// descriptor change, with it given and taken away, and a refused build
/// and one after it, which draw nothing. Over the play fixture the
/// elements on `loading` and `screen` draw, none on `ready`; over
/// the structural fixture its background, the lanes' line, the gauges'
/// empty frames, the cover paints, the gauge look's choice, the `when`
/// images and the play level's digits draw, in the first build and in later
/// ones, the digits only on the frames that give a level.
fn loading_does_not_allocate(loaded: &Loaded, views: &Views, descs: &[GaugeDesc]) {
    let mut loading = loaded.skin.prepare_loading(Side::Left);
    loaded
        .skin
        .install_loading_text(&mut loading, TextSlots::ALL, &text_table())
        .unwrap();
    let mut drawn = 0;
    // Whether an element on `loading`, `screen` and `ready` drew.
    let mut timed = [false; 3];
    let refused = track_loading(loaded, &mut loading, views, descs, |no, quads| {
        if no < 600 {
            drawn += quads.len();
        } else {
            assert!(quads.is_empty(), "a refused build drew");
        }
        for q in quads {
            if let Some(&(image, _)) = loaded.shows.get(&uv_key(q.uv, q.layer)) {
                timed[0] |= image == timer::LOADING;
                timed[1] |= image == timer::SCREEN;
                timed[2] |= image == timer::READY;
            }
        }
    });
    assert!(drawn > 0, "no loading quad drew");
    assert_eq!(timed, [true, true, false], "loading, screen, ready");
    assert_eq!(refused, [Err(BuildError::InvalidReadouts); 2]);

    let structural = loading_skin();
    let mut loading = structural.skin.prepare_loading(Side::Left);
    // Per kind, whether it drew in the first build and in a later one:
    // the background, the lanes' line, a gauge, a cover paint, the look
    // choice, a `when` image and a play level's digit.
    let mut seen = [[false; 7]; 2];
    let refused = track_loading(&structural, &mut loading, views, descs, |no, quads| {
        if no == 600 {
            assert!(quads.is_empty(), "a refused build drew");
            return;
        }
        let at = &mut seen[usize::from(no > 0)];
        for q in quads {
            if structural.parts.get(&part_key(q.uv, q.layer)) == Some(&(PARTS + 7)) {
                at[1] = true;
            }
            let Some(&(image, frame)) = structural.shows.get(&uv_key(q.uv, q.layer)) else {
                continue;
            };
            at[0] |= image == timer::SCREEN;
            at[2] |= (LOOKS..CHOICES).contains(&image);
            at[3] |= image == SUDDEN || image == LIFT;
            at[4] |= image == CHOICES + 2;
            at[5] |= (timer::BUILTIN..timer::BUILTIN + 3).contains(&image);
            at[6] |= image == DIGITS_10;
            assert!(
                image != DIGITS_10 || (no % 5 != 4 && [1, 2].contains(&(frame % 10))),
                "frame {no} drew digit {frame} of the level"
            );
        }
    });
    assert_eq!(seen[0][..5], [true; 5], "first build: {:?}", seen[0]);
    assert!(seen[0][6], "first build: {:?}", seen[0]);
    assert_eq!(seen[1], [true; 7], "later builds");
    assert_eq!(refused, [Err(BuildError::InvalidReadouts); 2]);
}

const LIMITS_SMALL: AtlasLimits = AtlasLimits {
    max_side: 256,
    max_layers: 4,
    reserved_layers: 1,
};

/// Two bitmap texts of the title: `A` from the primary file, `B` from its
/// fallback; the second squeezed into its box.
fn bitmap_skin() -> (tempfile::TempDir, Skin) {
    let (root, sheet) = image_support::ab_fonts();
    let text = |overflow| TextDef {
        value: TextValue::Field(TextField::Title),
        font: FontRef::Bitmap(0),
        align: Align::Center,
        overflow,
    };
    let element = |i: u16| ElementDef {
        source: Source::Text(u32::from(i)),
        track: track(i, Ease::Linear, End::Hold),
        timer: TimerRef::Builtin(timer::SCREEN),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    };
    let skin = Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: regions(),
        whens: Vec::new(),
        sheet,
        lanes: None,
        elements: vec![element(0), element(1)],
        widgets: skin_model::WidgetDefs {
            texts: vec![text(Overflow::Spill), text(Overflow::Shrink)],
            ..skin_model::WidgetDefs::default()
        },
        bomb_up_to: Some(skin_model::Grade::Bad),
        close: None,
        loadend: None,
        present: None,
    })
    .unwrap();
    (root, skin)
}

/// The source player's lanes (`lanes_support::source`) on `chart` played
/// as CN, so every long note shows its end cap, with HIDDEN+'s paint over
/// the region, trimmed at its bottom as it rises, through frames every
/// 16 ms under covers that leave scroll and covers that leave none, all
/// three together among them: nothing allocated, and notes, end caps,
/// bodies and HIDDEN+ drawn.
fn source_lanes_do_not_allocate(chart: &Chart, ctx: &PlayContext<'_>) {
    let skin = lanes_support::skin_with_widgets(
        play_support::REGIONS,
        Some(lanes_support::source()),
        vec![
            lanes_support::lanes_element(),
            lanes_support::paint(0, [160, 280, 544, 600], Follow::NONE),
        ],
        skin_model::WidgetDefs {
            cover_paints: vec![lanes_support::moving(
                lanes_support::HIDDEN,
                CoverMotion::Hidden,
                CoverTrim::Lift(880),
            )],
            ..skin_model::WidgetDefs::default()
        },
    )
    .unwrap();
    let uvs = play_support::uvs(&skin);
    let mut cn = chart.clone();
    cn.meta.ln_mode = Some(bms_model::LnMode::Cn);
    let mut play = skin.prepare(&cn, ctx).unwrap();
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let t = tally(0);
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let layout = SkinLayout::AUTHOR;
    let views = [
        (0, 0, 0),
        (500, 200, 0),
        (300, 200, 500),
        (1_000, 0, 0),
        (0, 1_000, 0),
        (1_000, 1_000, 1_000),
    ];
    let tail = uvs.get(lanes_support::TAIL).unwrap().uv;
    let hidden = uvs.get(lanes_support::HIDDEN).unwrap();
    let (mut quads, mut tails, mut hiddens) = (0_usize, 0_usize, 0_usize);
    TRACK.with(|t| t.set(true));
    for (i, now) in (0..12_000 * MS).step_by(16 * MS as usize).enumerate() {
        let (cover, lift, hidden_view) = views[i / 50 % views.len()];
        batch.clear();
        let lanes = LaneView {
            gain: Gain::new(8, 3),
            cover,
            lift,
            hidden: hidden_view,
        };
        let notes = NoteFacts::default();
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            now: TimeUs(now),
            display_offset: TimeUs(0),
            tally: &t,
            gauges: &gauges,
            gauge: 0,
            effects: &[],
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target: TARGETS[i % TARGETS.len()],
            layout: &layout,
            turntable: 0,
            lanes,
            readouts: readouts_for(lanes),
            playing: true,
            closing: None,
        };
        skin.build(&mut play, &frame, &uvs, &mut batch)
            .expect("the frame builds");
        quads += batch.quads().len();
        tails += batch.quads().iter().filter(|q| q.uv == tail).count();
        hiddens += batch
            .quads()
            .iter()
            .filter(|q| {
                q.layer == hidden.layer && [q.uv[0], q.uv[2]] == [hidden.uv[0], hidden.uv[2]]
            })
            .count();
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.swap(0, Ordering::Relaxed),
        0,
        "source lanes: allocations"
    );
    assert!(
        quads > 0 && tails > 0 && hiddens > 0,
        "source lanes drew {quads} quads, {tails} end caps, {hiddens} HIDDEN+"
    );
}

/// The source player's lanes drawn wholly with generated fallback paint
/// (`lanes_support::fallback_sheet`), as an importer draws parts whose
/// ids name no image: LN then CN, each attempt prepared untracked, frames
/// every 16 ms with the hold events drained as they come, under covers
/// that leave scroll and none, at every target and into a batch of half
/// the lanes' quads; then an invalid frame poisoning the play and builds
/// refused on it. Nothing allocated; taps, end caps (CN only), bodies and
/// mines drawn.
fn fallback_lanes_do_not_allocate(chart: &Chart, ctx: &PlayContext<'_>) {
    let [tap, long, mine] = [0, 1, 2].map(|k| lanes_support::FALLBACK + k);
    let lanes = skin_model::LanesDef {
        parts: LaneParts {
            note: [tap; 8],
            head: [long; 8],
            tail: [long; 8],
            body_on: [long; 8],
            body_off: [long; 8],
            mine: [mine; 8],
            line: None,
        },
        ..lanes_support::source()
    };
    let skin = Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: play_support::REGIONS,
        whens: Vec::new(),
        sheet: lanes_support::fallback_sheet(),
        elements: vec![lanes_support::lanes_element()],
        lanes: Some(lanes),
        widgets: skin_model::WidgetDefs::default(),
        bomb_up_to: Some(skin_model::Grade::Bad),
        close: None,
        loadend: None,
        present: None,
    })
    .unwrap();
    let uvs = play_support::uvs(&skin);
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let mut small =
        QuadBatch::with_capacity(skin_model::LANE_QUADS / 2, skin_model::LANE_QUADS / 2);
    let t = tally(0);
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let layout = SkinLayout::AUTHOR;
    let events = effects(chart, None, false);
    let uv_of = |i: u32| uvs.get(i).unwrap().uv;
    let [tap_uv, long_uv, mine_uv] = [tap, long, mine].map(uv_of);
    // The most long-note quads in one frame, LN then CN.
    let mut max = [0_usize; 2];
    for (slot, mode) in max
        .iter_mut()
        .zip([bms_model::LnMode::Ln, bms_model::LnMode::Cn])
    {
        let mut c = chart.clone();
        c.meta.ln_mode = Some(mode);
        let mut play = skin.prepare(&c, ctx).unwrap();
        let mut notes = NoteFacts::new(&c);
        let mut next = 0;
        let (mut taps, mut longs, mut mines, mut max_longs) = (0_usize, 0_usize, 0_usize, 0);
        TRACK.with(|t| t.set(true));
        for (i, now) in (0..12_000 * MS).step_by(16 * MS as usize).enumerate() {
            let cover = [0, 300, 1_000][i / 50 % 3];
            let lanes = LaneView {
                gain: Gain::new(8, 3),
                cover,
                lift: 0,
                hidden: 0,
            };
            let rest = events.get(next..).unwrap_or_default();
            let n = rest.partition_point(|e| e.time.0 <= now);
            let drain = rest.get(..n).unwrap_or_default();
            next += n;
            for e in drain {
                play_support::follow(&mut notes, &c, e);
            }
            let frame = SkinFrame {
                app: skin_model::AppReadouts::LAUNCH,
                state_us: 0,
                now: TimeUs(now),
                display_offset: TimeUs(0),
                tally: &t,
                gauges: &gauges,
                gauge: 0,
                effects: drain,
                notes: &notes,
                offsets: skin_model::Offsets::NONE,
                input: &eval::InputFacts::NONE,
                complete: false,
                target: TARGETS[i % TARGETS.len()],
                layout: &layout,
                turntable: 0,
                lanes,
                readouts: readouts_for(lanes),
                playing: true,
                closing: None,
            };
            let out = if i % 50 == 49 { &mut small } else { &mut batch };
            out.clear();
            skin.build(&mut play, &frame, &uvs, out)
                .expect("the frame builds");
            let count = |uv: [f32; 4]| out.quads().iter().filter(|q| q.uv == uv).count();
            taps += count(tap_uv);
            mines += count(mine_uv);
            let l = count(long_uv);
            longs += l;
            max_longs = max_longs.max(l);
        }
        // An invalid frame poisons the play; every later build refuses.
        let lanes = LaneView {
            gain: Gain::new(8, 3),
            cover: 0,
            lift: 100,
            hidden: 0,
        };
        let mut bad = readouts_for(lanes);
        bad.covers.lift = 500;
        for _ in 0..3 {
            batch.clear();
            let frame = SkinFrame {
                app: skin_model::AppReadouts::LAUNCH,
                state_us: 0,
                now: TimeUs(1_000 * MS),
                display_offset: TimeUs(0),
                tally: &t,
                gauges: &gauges,
                gauge: 0,
                effects: &[],
                notes: &notes,
                offsets: skin_model::Offsets::NONE,
                input: &eval::InputFacts::NONE,
                complete: false,
                target: TARGETS[0],
                layout: &layout,
                turntable: 0,
                lanes,
                readouts: bad,
                playing: true,
                closing: None,
            };
            assert_eq!(
                skin.build(&mut play, &frame, &uvs, &mut batch),
                Err(BuildError::InvalidReadouts)
            );
        }
        TRACK.with(|t| t.set(false));
        assert!(
            taps > 0 && mines > 0 && longs > 0,
            "{mode:?}: {taps} {mines} {longs}"
        );
        *slot = max_longs;
    }
    assert_eq!(
        ALLOCS.swap(0, Ordering::Relaxed),
        0,
        "fallback lanes: allocations"
    );
    // CN draws its end caps besides LN's body and head.
    let [ln, cn] = max;
    assert!(cn > ln, "LN {ln}, CN {cn}");
}

/// Timeline orders build without allocating: the simple lanes with every
/// key coincident under a reversed order, and the source lanes with keys
/// overlapping by half a lane under the source's, each lane's tap its own
/// image; over notes on all eight lanes with long notes held as their
/// effects drain and a STOP, the whole chart and a section from 2 s to 5 s
/// (notes across both its edges, the STOP inside it),
/// every attempt prepared untracked; frames every 16 ms under covers that
/// leave scroll and none, at every target, into a full batch and a short
/// one; then an invalid frame poisoning the play. In each style and each
/// attempt every lane's taps drew, a held long note drew its active body
/// (its own image) and the short batch dropped.
fn timeline_lanes_do_not_allocate(full: &PlayContext<'_>) {
    use lanes_support::{
        BAR, BODY_OFF, BODY_ON, COVER, HEAD, HIDDEN, LIFT, LINE, MINE, NOTE, TAIL, WHITE,
    };
    let section = Section {
        start: TimeUs(2_000 * MS),
        end: TimeUs(5_000 * MS),
        lead_in: TimeUs(1_000 * MS),
        initial_gauges: vec![GaugeValue::FULL; 2],
    };
    let round = PlayContext {
        attempt: Attempt::Section(&section),
        ..*full
    };
    let contexts = [(full, None), (&round, Some(&section))];
    let taps = [NOTE, HEAD, TAIL, BODY_ON, BODY_OFF, MINE, BAR, LINE];
    let simple = {
        let mut def = lanes_support::simple();
        for l in def.lanes.iter_mut().skip(1) {
            *l = [256, 64];
        }
        def.note_order = skin_model::NoteOrder::Timeline {
            lanes: [7, 6, 5, 4, 3, 2, 1, 0],
        };
        def
    };
    let source = {
        let mut def = lanes_support::source();
        for (k, l) in (0_i32..).zip(def.lanes.iter_mut()).skip(1) {
            *l = [256 + 32 * (k - 1), 64];
        }
        def
    };
    let mut b = ChartBuilder::new(150_000)
        .sound("a.wav")
        .stop(TimeUs(3_000 * MS), TimeUs(500 * MS));
    for i in 0..1_600 {
        let t = TimeUs(i * 40 * MS / 8);
        b = if i % 7 == 3 {
            b.long(lane(i), t, TimeUs(t.0 + 30 * MS))
        } else {
            b.tap(lane(i), t)
        };
    }
    let chart = b.build().unwrap();
    let t = tally(0);
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let layout = SkinLayout::AUTHOR;
    // Per style and attempt: each lane's taps seen, active bodies, drops.
    let mut witnesses = Vec::new();
    for (style, mut def) in [("simple", simple), ("source", source)] {
        def.parts.note = taps;
        def.parts.body_on = [COVER; 8];
        // The other long-note parts and the mines apart from every tap.
        def.parts.head = [LIFT; 8];
        def.parts.tail = [HIDDEN; 8];
        def.parts.body_off = [WHITE; 8];
        def.parts.mine = [WHITE; 8];
        // Lane 7's tap image is the line's otherwise.
        def.parts.line = None;
        let skin = lanes_support::lanes_skin(def);
        let uvs = play_support::uvs(&skin);
        let key_of = |i: u32| {
            let f = uvs.get(i).unwrap();
            ([f.uv[0].to_bits(), f.uv[2].to_bits()], f.layer)
        };
        let keys = taps.map(key_of);
        let held_key = key_of(COVER);
        let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        let mut small = QuadBatch::with_capacity(64, 16);
        for (ctx, section) in contexts {
            let (mut seen, mut held, mut drops) = ([false; 8], 0_usize, 0_u32);
            let events = effects(&chart, section, false);
            let from = section.map_or(0, |s| s.start.0 - s.lead_in.0);
            let mut play = skin.prepare(&chart, ctx).unwrap();
            let mut notes = NoteFacts::new(&chart);
            let mut next = 0;
            TRACK.with(|t| t.set(true));
            for (i, now) in (from..from + 9_000 * MS)
                .step_by(16 * MS as usize)
                .enumerate()
            {
                let cover = [0, 300, 1_000][i / 40 % 3];
                let lanes = LaneView {
                    gain: Gain::new(8, 3),
                    cover,
                    lift: 0,
                    hidden: 0,
                };
                let rest = events.get(next..).unwrap_or_default();
                let n = rest.partition_point(|e| e.time.0 <= now).min(DRAIN);
                let drain = rest.get(..n).unwrap_or_default();
                next += n;
                for e in drain {
                    play_support::follow(&mut notes, &chart, e);
                }
                let frame = SkinFrame {
                    app: skin_model::AppReadouts::LAUNCH,
                    state_us: 0,
                    now: TimeUs(now),
                    display_offset: TimeUs(0),
                    tally: &t,
                    gauges: &gauges,
                    gauge: 0,
                    effects: drain,
                    notes: &notes,
                    offsets: skin_model::Offsets::NONE,
                    input: &eval::InputFacts::NONE,
                    complete: false,
                    target: TARGETS[i % TARGETS.len()],
                    layout: &layout,
                    turntable: 0,
                    lanes,
                    readouts: readouts_for(lanes),
                    playing: true,
                    closing: None,
                };
                let out = if i % 25 == 24 { &mut small } else { &mut batch };
                out.clear();
                skin.build(&mut play, &frame, &uvs, out)
                    .expect("the frame builds");
                drops += out.dropped();
                for q in out.quads() {
                    let key = ([q.uv[0].to_bits(), q.uv[2].to_bits()], q.layer);
                    if let Some(l) = keys.iter().position(|&k| k == key) {
                        seen[l] = true;
                    }
                    held += usize::from(key == held_key);
                }
            }
            // Repeated READY rearms, each followed by frames across the
            // STOP: the runtime starts over in place, the prepared exact
            // playhead is kept, and nothing allocates.
            let reps = play.playhead().representatives().as_ptr();
            for r in 0..3_i64 {
                play.rearm();
                notes.reset();
                for k in 0..5_i64 {
                    let lanes = LaneView {
                        gain: Gain::new(8, 3),
                        cover: 0,
                        lift: 0,
                        hidden: 0,
                    };
                    let frame = SkinFrame {
                        app: skin_model::AppReadouts::LAUNCH,
                        state_us: 0,
                        now: TimeUs(from + (k + 1) * 700 * MS),
                        display_offset: TimeUs(0),
                        tally: &t,
                        gauges: &gauges,
                        gauge: 0,
                        effects: &[],
                        notes: &notes,
                        offsets: skin_model::Offsets::NONE,
                        input: &eval::InputFacts::NONE,
                        complete: false,
                        target: TARGETS[0],
                        layout: &layout,
                        turntable: 0,
                        lanes,
                        readouts: readouts_for(lanes),
                        playing: true,
                        closing: None,
                    };
                    batch.clear();
                    skin.build(&mut play, &frame, &uvs, &mut batch)
                        .expect("a rearmed frame builds");
                    assert!(!batch.quads().is_empty(), "rearm {r} frame {k}");
                }
            }
            assert_eq!(play.playhead().representatives().as_ptr(), reps);
            // An invalid frame poisons the play; every later build refuses.
            let lanes = LaneView {
                gain: Gain::new(8, 3),
                cover: 0,
                lift: 100,
                hidden: 0,
            };
            let mut bad = readouts_for(lanes);
            bad.covers.lift = 500;
            for _ in 0..2 {
                batch.clear();
                let frame = SkinFrame {
                    app: skin_model::AppReadouts::LAUNCH,
                    state_us: 0,
                    now: TimeUs(from + 9_000 * MS),
                    display_offset: TimeUs(0),
                    tally: &t,
                    gauges: &gauges,
                    gauge: 0,
                    effects: &[],
                    notes: &notes,
                    offsets: skin_model::Offsets::NONE,
                    input: &eval::InputFacts::NONE,
                    complete: false,
                    target: TARGETS[0],
                    layout: &layout,
                    turntable: 0,
                    lanes,
                    readouts: bad,
                    playing: true,
                    closing: None,
                };
                assert_eq!(
                    skin.build(&mut play, &frame, &uvs, &mut batch),
                    Err(BuildError::InvalidReadouts)
                );
            }
            TRACK.with(|t| t.set(false));
            witnesses.push((style, section.is_some(), seen, held, drops));
        }
    }
    assert_eq!(
        ALLOCS.swap(0, Ordering::Relaxed),
        0,
        "timeline lanes: allocations"
    );
    for (style, sectioned, seen, held, drops) in witnesses {
        let at = format!("{style}, section {sectioned}");
        assert_eq!(seen, [true; 8], "{at}");
        assert!(held > 0, "{at}: no held body");
        assert!(drops > 0, "{at}: no drop");
    }
}

/// The trimmed cover paints of [`cover_trims_do_not_allocate`] over the
/// simple lanes: HIDDEN+ turning as it moves, trimmed at the line as it
/// rises; LIFT gated by its own track at the lanes' bottom, kept at a
/// native size; a choice of LIFT or HIDDEN+ on its entries' tracks,
/// trimmed at a fixed line; and a LIFT whose gate track ends, so it draws
/// nothing past 2 s.
fn trim_skin() -> Skin {
    use lanes_support::{HIDDEN, LIFT, moving, paint, rect};
    let turning = Track::new(
        &[
            KeyDef {
                at: TimeUs(0),
                props: [
                    Some(160),
                    Some(880),
                    Some(544),
                    Some(800),
                    None,
                    None,
                    None,
                    None,
                    Some(0),
                ],
            },
            KeyDef {
                at: TimeUs(3_000 * MS),
                props: [
                    Some(160),
                    Some(800),
                    Some(544),
                    Some(800),
                    None,
                    None,
                    None,
                    None,
                    Some(90),
                ],
            },
        ],
        Ease::Linear,
        End::Loop { from: TimeUs(0) },
    )
    .unwrap();
    let ending = Track::new(
        &[
            KeyDef {
                at: TimeUs(0),
                props: [
                    Some(160),
                    Some(900),
                    Some(544),
                    Some(60),
                    None,
                    None,
                    None,
                    None,
                    None,
                ],
            },
            KeyDef {
                at: TimeUs(2_000 * MS),
                props: [
                    Some(160),
                    Some(900),
                    Some(544),
                    Some(60),
                    None,
                    None,
                    None,
                    None,
                    None,
                ],
            },
        ],
        Ease::Linear,
        End::Hide,
    )
    .unwrap();
    let choice = skin_model::ImageChoiceDef {
        value: skin_model::compile(
            "combo % 3",
            &skin_model::Scope::new(&skin_model::PLAY),
            skin_model::Ty::Int,
        )
        .unwrap(),
        past_table: skin_model::PastTable::Hide,
        choices: [LIFT, HIDDEN]
            .map(|image| {
                Some(skin_model::ImageChoicePart {
                    span: skin_model::ImageSpan {
                        image,
                        first: 0,
                        frames: 1,
                    },
                    cycle: None,
                    track: Some(rect([
                        160,
                        700 + 50 * i32::try_from(image).unwrap(),
                        544,
                        300,
                    ])),
                    native: None,
                })
            })
            .into_iter()
            .chain([None])
            .collect(),
    };
    let native = Place {
        native: Some(Native::Exact([13, 32])),
        ..lanes_support::in_region(Region::Playfield)
    };
    lanes_support::skin_with_widgets(
        play_support::REGIONS,
        Some(lanes_support::simple()),
        vec![
            lanes_support::lanes_element(),
            ElementDef {
                track: turning,
                ..paint(0, [0; 4], Follow::COVER)
            },
            ElementDef {
                place: native,
                ..paint(1, [160, 900, 544, 0], Follow::NONE)
            },
            paint(2, [160, 700, 544, 300], Follow::NONE),
            paint(3, [160, 900, 544, 60], Follow::NONE),
        ],
        skin_model::WidgetDefs {
            image_choices: vec![choice],
            cover_paints: vec![
                moving(HIDDEN, CoverMotion::Hidden, CoverTrim::Lift(880)),
                CoverPaintDef {
                    gate_track: Some(rect([160, 940, 544, 40])),
                    ..moving(LIFT, CoverMotion::Lift, CoverTrim::Fixed(960))
                },
                CoverPaintDef {
                    image: CoverImage::Choice(0),
                    ..moving(0, CoverMotion::Lift, CoverTrim::Fixed(850))
                },
                CoverPaintDef {
                    gate_track: Some(ending),
                    ..moving(LIFT, CoverMotion::Lift, CoverTrim::Lift(930))
                },
            ],
            ..skin_model::WidgetDefs::default()
        },
    )
    .unwrap()
}

/// Trimmed cover paints build without allocating, each play prepared
/// untracked: the first build and later ones, the lift changing the
/// linked trims, HIDDEN+ switched on and off, choices changing, rotation,
/// every target, a full batch, a gate track that ends, and an invalid
/// frame poisoning the play. Clipped, whole and suppressed paint all
/// occur.
fn cover_trims_do_not_allocate(chart: &Chart, ctx: &PlayContext<'_>) {
    let skin = trim_skin();
    let uvs = play_support::uvs(&skin);
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let mut small =
        QuadBatch::with_capacity(skin_model::LANE_QUADS / 2, skin_model::LANE_QUADS / 2);
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let layout = SkinLayout::AUTHOR;
    let (mut clipped, mut whole, mut suppressed, mut turned) = (0_usize, 0_usize, 0_usize, 0_usize);
    let paint_uvs = [lanes_support::LIFT, lanes_support::HIDDEN].map(|i| {
        let uv = uvs.get(i).unwrap().uv;
        [uv[0].to_bits(), uv[2].to_bits()]
    });
    let mut play = skin.prepare(chart, ctx).unwrap();
    TRACK.with(|t| t.set(true));
    for (i, now) in (0..6_000 * MS).step_by(16 * MS as usize).enumerate() {
        let on = i / 40 % 2 == 0;
        let lanes = LaneView {
            gain: Gain::new(8, 3),
            cover: [0, 300, 900][i / 7 % 3],
            lift: [0, 100, 250, 1_000][i / 11 % 4],
            hidden: if on { [0, 400, 1_000][i / 13 % 3] } else { 0 },
        };
        let mut readouts = readouts_for(lanes);
        readouts.covers.hidden_enabled = on;
        let t = tally(u32::try_from(i / 17).unwrap_or(0));
        let notes = NoteFacts::default();
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            now: TimeUs(now),
            display_offset: TimeUs(0),
            tally: &t,
            gauges: &gauges,
            gauge: 0,
            effects: &[],
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target: TARGETS[i / 5 % TARGETS.len()],
            layout: &layout,
            turntable: 0,
            lanes,
            readouts,
            playing: true,
            closing: None,
        };
        let out = if i % 50 == 49 { &mut small } else { &mut batch };
        out.clear();
        skin.build(&mut play, &frame, &uvs, out)
            .expect("the frame builds");
        // The paints' quads, each with its run's clip rect.
        let mut drawn = 0;
        for r in out.runs() {
            for q in out
                .quads()
                .get(r.start as usize..r.end as usize)
                .unwrap_or_default()
            {
                if !paint_uvs.contains(&[q.uv[0].to_bits(), q.uv[2].to_bits()]) {
                    continue;
                }
                drawn += 1;
                if r.clip_rect.is_some() {
                    clipped += 1;
                } else {
                    whole += 1;
                }
                turned += usize::from(q.angle != 0.0);
            }
        }
        suppressed += usize::from(drawn < 4);
    }
    // An invalid frame poisons the play; every later build refuses.
    let lanes = LaneView {
        gain: Gain::new(8, 3),
        cover: 0,
        lift: 100,
        hidden: 0,
    };
    let mut bad = readouts_for(lanes);
    bad.covers.lift = 500;
    let t = tally(0);
    for _ in 0..3 {
        batch.clear();
        let notes = NoteFacts::default();
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            now: TimeUs(1_000 * MS),
            display_offset: TimeUs(0),
            tally: &t,
            gauges: &gauges,
            gauge: 0,
            effects: &[],
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target: TARGETS[0],
            layout: &layout,
            turntable: 0,
            lanes,
            readouts: bad,
            playing: true,
            closing: None,
        };
        assert_eq!(
            skin.build(&mut play, &frame, &uvs, &mut batch),
            Err(BuildError::InvalidReadouts)
        );
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.swap(0, Ordering::Relaxed),
        0,
        "cover trims: allocations"
    );
    assert!(
        clipped > 0 && whole > 0 && suppressed > 0 && turned > 0,
        "cover trims: {clipped} clipped, {whole} whole, {suppressed} frames with a paint \
         suppressed, {turned} turned"
    );
}

/// Bar groups of every kind: following each cover, turned as they
/// animate, kept at a native size, added, on `play`, hidden by a
/// condition and fading on `ready`.
fn bar_group_defs() -> Vec<skin_model::BarGroupDef> {
    let base = lanes_support::bar_group(879, 2);
    let keys = |angle: i32| {
        [0, 2_000].map(|t| KeyDef {
            at: TimeUs(t * MS),
            props: [
                Some(160),
                Some(800),
                Some(544),
                Some(4),
                Some(if t == 0 { 0 } else { 255 }),
                None,
                None,
                None,
                Some(if t == 0 { 0 } else { angle }),
            ],
        })
    };
    let animated = |angle| Track::new(&keys(angle), Ease::Linear, End::Loop { from: TimeUs(0) });
    let mut out: Vec<_> = FOLLOWS
        .iter()
        .map(|&follow| skin_model::BarGroupDef {
            place: Place {
                follow,
                angle: skin_model::AngleFollow::None,
                ..base.place
            },
            ..base.clone()
        })
        .collect();
    out.push(skin_model::BarGroupDef {
        track: animated(270).unwrap(),
        place: Place {
            pivot: Anchor::Center,
            ..base.place
        },
        ..base.clone()
    });
    out.push(skin_model::BarGroupDef {
        place: Place {
            native: Some(Native::Exact([4, 300])),
            bind: Bind::None,
            ..base.place
        },
        blend: Blend::Add,
        ..base.clone()
    });
    out.push(skin_model::BarGroupDef {
        timer: TimerRef::Builtin(timer::PLAY),
        place: Place {
            native: Some(Native::NoLarger([600, 1])),
            ..base.place
        },
        ..base.clone()
    });
    out.push(skin_model::BarGroupDef {
        visible: Some(bool_program("combo > 0 || cover.enabled")),
        track: animated(0).unwrap(),
        ..base
    });
    out
}

/// Bar groups build without allocating: the first build and later ones,
/// both lane styles, a STOP, the lead-in, every following, turned and
/// native quads, every budget run out, a play of another group count and
/// an invalid frame poisoning the play.
fn bar_groups_do_not_allocate(ctx: &PlayContext<'_>) {
    let groups = bar_group_defs();
    let simple = skin_model::LanesDef {
        bar_groups: groups.clone(),
        ..lanes_support::simple()
    };
    let source = skin_model::LanesDef {
        bar_groups: groups,
        ..lanes_support::source()
    };
    // Bars every 100 ms over a STOP, then a run of equal bars that spends
    // every budget.
    let mut builder = ChartBuilder::new(150_000)
        .stop(TimeUs(2_000 * MS), TimeUs(1_000 * MS))
        .tap(lane(3), TimeUs(9_000 * MS));
    for i in 0..80 {
        builder = builder.bar(TimeUs(i * 100 * MS));
    }
    for _ in 0..skin_model::LANE_VISITS {
        builder = builder.bar(TimeUs(9_000 * MS));
    }
    let chart = builder.build().unwrap();
    let t = tally(0);
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let layout = SkinLayout::AUTHOR;
    // A frame at `now` µs, shown `offset` µs ahead, on the stack.
    let notes = NoteFacts::default();
    let frame_at = |now: i64, offset: i64, target, lanes, readouts| SkinFrame {
        app: skin_model::AppReadouts::LAUNCH,
        state_us: 0,
        now: TimeUs(now),
        display_offset: TimeUs(offset),
        tally: &t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        notes: &notes,
        offsets: skin_model::Offsets::NONE,
        input: &eval::InputFacts::NONE,
        complete: false,
        target,
        layout: &layout,
        turntable: 0,
        lanes,
        readouts,
        playing: true,
        closing: None,
    };
    let bar_uv = |uvs: &UvTable| uvs.get(lanes_support::BAR).unwrap().uv;
    let (mut quads, mut turned, mut drops) = (0_usize, 0_usize, 0_u32);
    for def in [simple, source] {
        let skin = lanes_support::lanes_skin(def);
        let uvs = play_support::uvs(&skin);
        let mut play = skin.prepare(&chart, ctx).unwrap();
        let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        let bar = bar_uv(&uvs);
        TRACK.with(|t| t.set(true));
        for (i, now) in (-1_000 * MS..10_000 * MS)
            .step_by(16 * MS as usize)
            .enumerate()
        {
            batch.clear();
            let lanes = LaneView {
                gain: Gain::new(8, 3),
                cover: [0, 300, 0][i / 60 % 3],
                lift: [0, 0, 250][i / 60 % 3],
                hidden: 0,
            };
            let frame = frame_at(
                now,
                [0, 40 * MS, -25 * MS][i % 3],
                TARGETS[i % TARGETS.len()],
                lanes,
                readouts_for(lanes),
            );
            skin.build(&mut play, &frame, &uvs, &mut batch)
                .expect("the frame builds");
            quads += batch.quads().iter().filter(|q| q.uv == bar).count();
            turned += batch
                .quads()
                .iter()
                .filter(|q| q.uv == bar && q.angle != 0.0)
                .count();
            drops += batch.dropped();
        }
        // An invalid frame poisons the play; every later build refuses.
        let lanes = LaneView {
            gain: Gain::new(8, 3),
            cover: 0,
            lift: 0,
            hidden: 0,
        };
        let mut bad = readouts_for(lanes);
        bad.covers.cover = 500;
        for _ in 0..3 {
            batch.clear();
            let frame = frame_at(5_000 * MS, 0, TARGETS[0], lanes, bad);
            assert_eq!(
                skin.build(&mut play, &frame, &uvs, &mut batch),
                Err(BuildError::InvalidReadouts)
            );
        }
        TRACK.with(|t| t.set(false));
        // A play of another group count draws nothing, allocating nothing.
        let other = lanes_support::lanes_skin(lanes_support::simple());
        let mut stranger = other.prepare(&chart, ctx).unwrap();
        TRACK.with(|t| t.set(true));
        batch.clear();
        let frame = frame_at(5_000 * MS, 0, TARGETS[0], lanes, readouts_for(lanes));
        skin.build(&mut stranger, &frame, &uvs, &mut batch)
            .expect("a wrong play draws nothing");
        assert!(batch.quads().is_empty());
        TRACK.with(|t| t.set(false));
    }
    assert_eq!(
        ALLOCS.swap(0, Ordering::Relaxed),
        0,
        "bar groups: allocations"
    );
    assert!(
        quads > 0 && turned > 0 && drops > 0,
        "bar groups drew {quads} bars, {turned} turned, {drops} drops"
    );
}

/// The attached grades of [`grade_attachments_do_not_allocate`]: `A`
/// (shifting) has no full-gauge grade, so PERFECT's attached grade stands
/// in beside the full-gauge combo; `B` has no full-gauge combo, so its
/// attached full-gauge grade takes PERFECT's. Each attachment moves on
/// `judge` with its paint; each combo moves on `ready`.
fn attached_judges() -> (Vec<JudgeDef>, [Track; 2], Track) {
    let rects = |x: i32, y: i32, w: i32, h: i32, dy: i32| {
        let key = |at: i64, y: i32| {
            let mut props = [None; 9];
            props[..4].copy_from_slice(&[Some(x), Some(y), Some(w), Some(h)]);
            KeyDef {
                at: TimeUs(at),
                props,
            }
        };
        Track::new(&[key(0, y), key(500 * MS, y + dy)], Ease::Out, End::Hold).unwrap()
    };
    let combo_track = {
        let key = |at: i64, x: i32| {
            let mut props = [None; 9];
            props[..4].copy_from_slice(&[Some(x), Some(-20), Some(10), Some(20)]);
            KeyDef {
                at: TimeUs(at),
                props,
            }
        };
        Track::new(&[key(0, 0), key(10_000 * MS, 100)], Ease::Linear, End::Hold).unwrap()
    };
    let digits = DigitsDef {
        image: u32::from(COMBO_10),
        set: GlyphSet::Ten,
        cells: 1,
        align: Align::Left,
        pad: Pad::Blank,
        space: 0,
        offsets: Vec::new(),
    };
    let grade = |k: u16, paint: Track, attachment: Option<Track>| GradePart {
        image: u32::from(GRADES + k),
        track: paint,
        timer: TimerRef::Builtin(timer::JUDGE),
        pivot: Anchor::TopLeft,
        blend: Blend::Alpha,
        visible: None,
        native: None,
        follow: Follow::NONE,
        attachment,
    };
    let combo = |attach: Attach| ComboPart {
        digits: digits.clone(),
        track: combo_track.clone(),
        timer: TimerRef::Builtin(timer::READY),
        attach,
        pivot: Anchor::TopLeft,
        blend: Blend::Alpha,
        visible: None,
        native: None,
    };
    let raw_a = rects(100, 300, 80, 20, 40);
    let raw_b = rects(400, 300, 80, 20, 20);
    let mut a = JudgeDef {
        grades: Default::default(),
        combos: Default::default(),
        shift: true,
    };
    a.grades[0] = Some(grade(0, rects(130, 300, 20, 20, 40), Some(raw_a.clone())));
    a.combos[0] = Some(combo(Attach::Top));
    a.combos[6] = Some(combo(Attach::Bottom));
    let mut b = JudgeDef {
        grades: Default::default(),
        combos: Default::default(),
        shift: false,
    };
    b.grades[0] = Some(grade(0, rects(400, 300, 80, 20, 20), None));
    b.grades[6] = Some(grade(6, rects(430, 300, 20, 20, 20), Some(raw_b.clone())));
    b.combos[0] = Some(combo(Attach::Bottom));
    (vec![a, b], [raw_a, raw_b], combo_track)
}

/// Attached grades build without allocating: both full-gauge fallbacks,
/// each attachment on the grade's clock and each combo on its own, over
/// every target. At the author's 1920×1080, each combo cell is drawn on
/// its attachment's bottom edge at the combo's own sample, and the
/// shifting judge's paint half a cell left.
fn grade_attachments_do_not_allocate(ctx: &PlayContext<'_>) {
    let (judges, [raw_a, raw_b], combo_track) = attached_judges();
    let place = Place {
        region: Region::Judge,
        ..Place::SCREEN
    };
    let still = lanes_support::still([None; 9]);
    let ready = TimerRef::Builtin(timer::READY);
    // The fixture's lanes, never drawn.
    let elements = vec![
        ElementDef {
            visible: Some(bool_program("combo < 0")),
            ..lanes_support::lanes_element()
        },
        widget(Source::Judge(0), still.clone(), ready, place),
        widget(Source::Judge(1), still, ready, place),
    ];
    // The sheet's cycles name `when` timers; none of them is drawn here.
    let whens = (0..32).map(|_| bool_program("combo >= 1")).collect();
    let loaded = load(
        whens,
        elements,
        skin_model::WidgetDefs {
            judges,
            ..skin_model::WidgetDefs::default()
        },
    );
    let skin = &loaded.skin;
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let mut play = skin.prepare(&chart, ctx).unwrap();
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let before = tally(0);
    let after = Tally {
        counts: rules::PerJudgment {
            perfect: 1,
            ..before.counts
        },
        ex_score: 2,
        ..tally(1)
    };
    let effects = [
        ev(
            1_000 * MS,
            JudgeEventKind::Gauge {
                gauge: 0,
                value: GaugeValue::FULL,
                reference: GaugeValue::FULL,
            },
        ),
        ev(
            1_000 * MS,
            JudgeEventKind::Judged {
                lane: lane(1),
                note: 0,
                judgment: Judgment::Perfect,
                deviation: Some(TimeUs(0)),
                release: None,
                combo: 1,
                presses: 0,
            },
        ),
    ];
    let layout = SkinLayout::AUTHOR;
    let lanes = LaneView {
        gain: Gain::new(8, 3),
        cover: 0,
        lift: 0,
        hidden: 0,
    };
    let shown = |q: &skin_model::Quad| loaded.shows.get(&uv_key(q.uv, q.layer)).map(|s| s.0);
    let mut witnessed = 0_usize;
    TRACK.with(|t| t.set(true));
    for (i, now) in (0..3_000 * MS).step_by(16 * MS as usize).enumerate() {
        let judged = now >= 1_008 * MS;
        let target = TARGETS[i % TARGETS.len()];
        let notes = NoteFacts::default();
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            now: TimeUs(now),
            display_offset: TimeUs(0),
            tally: if judged { &after } else { &before },
            gauges: &gauges,
            gauge: 0,
            effects: if now == 1_008 * MS { &effects } else { &[] },
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target,
            layout: &layout,
            turntable: 0,
            lanes,
            readouts: readouts_for(lanes),
            playing: true,
            closing: None,
        };
        batch.clear();
        skin.build(&mut play, &frame, &loaded.uvs, &mut batch)
            .expect("the frame builds");
        let q = batch.quads();
        if now < 1_008 * MS || target != TARGETS[0] {
            continue;
        }
        // Judge A: its combo cell then PERFECT's grade; judge B: its
        // combo cell then the full-gauge grade.
        let (Some(ready_at), Some(judge_at)) = (play.timer(timer::READY), play.timer(timer::JUDGE))
        else {
            continue;
        };
        let c = combo_track.sample(now - ready_at.0).unwrap().rect;
        let gt = now - judge_at.0;
        let cell = |raw: &Track| {
            let a = raw.sample(gt).unwrap().rect;
            [a[0] + c[0], a[1] + a[3] + c[1], c[2], c[3]]
        };
        if q.len() == 4
            && [shown(&q[0]), shown(&q[1]), shown(&q[2]), shown(&q[3])]
                == [
                    Some(COMBO_10),
                    Some(GRADES),
                    Some(COMBO_10),
                    Some(GRADES + 6),
                ]
            && q[0].rect == cell(&raw_a)
            && q[2].rect == cell(&raw_b)
            && q[1].rect[0] == 125.0
        {
            witnessed += 1;
        }
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.swap(0, Ordering::Relaxed),
        0,
        "grade attachments: allocations"
    );
    // 1 008 to 2 992 ms at every fifth frame.
    assert_eq!(witnessed, 25, "attached frames witnessed");
}

/// Gauges at the widget budget build without allocating, in a play and
/// while loading: 16 383 flickering segments and 16 384 falling over all
/// of them each step of 1 µs, both following the covers, at zero, tiny,
/// half and full values as the shown gauge switches, on every target and
/// view, into a full batch and a short one. The skins are prepared before
/// counting.
fn large_gauges_do_not_allocate(ctx: &PlayContext<'_>, views: &Views, descs: &[GaugeDesc]) {
    let look = |first: u16| {
        let at = |k: u16| [u32::from(first + 2 * k), u32::from(first + 2 * k + 1)];
        GaugeParts {
            front: at(0),
            back: at(1),
            tip: at(2),
        }
    };
    let mut looks = [look(LOOKS); GaugeLook::COUNT];
    looks[GaugeLook::Hard.index()] = look(LOOKS + 6);
    let max = skin_model::MAX_SEGMENTS;
    let defs = [
        GaugeDef {
            segments: max - 1,
            looks,
            motion: Motion::Flicker {
                period: TimeUs(330 * MS),
            },
        },
        GaugeDef {
            segments: max,
            looks,
            motion: Motion::Fall {
                range: max,
                step: TimeUs(1),
            },
        },
    ];
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let values = [
        GaugeValue(0),
        GaugeValue(1),
        GaugeValue(50 * GaugeValue::PERCENT),
        GaugeValue::FULL,
    ];
    let t = tally(0);
    let mut most = [0_usize; 2];
    let mut drops = 0_u32;
    let mut loading_quads = [0_usize; 2];
    for (k, def) in defs.into_iter().enumerate() {
        let place = Place {
            follow: Follow::LIFT_AND_COVER,
            ..Place::SCREEN
        };
        let elements = vec![
            ElementDef {
                visible: Some(bool_program("combo < 0")),
                ..lanes_support::lanes_element()
            },
            widget(
                Source::Gauge(0),
                lanes_support::rect([100, 900, 600, 20]),
                TimerRef::Builtin(timer::SCREEN),
                place,
            ),
        ];
        let whens = (0..32).map(|_| bool_program("combo >= 1")).collect();
        let loaded = load(
            whens,
            elements,
            skin_model::WidgetDefs {
                gauges: vec![def],
                ..skin_model::WidgetDefs::default()
            },
        );
        let skin = &loaded.skin;
        assert_eq!(
            skin.quad_capacity(),
            skin_model::MAX_WIDGET_QUADS + skin_model::LANE_QUADS + skin_model::LANE_RESERVED
        );
        let mut play = skin.prepare(&chart, ctx).unwrap();
        let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        let mut small = QuadBatch::with_capacity(100, 8);
        let mut loading = skin.prepare_loading(Side::Left);
        let mut gauges = [state(values[0]), state(values[0])];
        TRACK.with(|t| t.set(true));
        for no in 0..240_usize {
            let now = i64::try_from(no).unwrap() * 16 * MS + i64::try_from(no % 3).unwrap();
            for (g, v) in gauges.iter_mut().zip([no / 4, no / 4 + 1]) {
                g.value = values[v % values.len()];
            }
            let view = views.lanes[(no / 7) % views.lanes.len()];
            let notes = NoteFacts::default();
            let frame = SkinFrame {
                app: skin_model::AppReadouts::LAUNCH,
                state_us: 0,
                now: TimeUs(now),
                display_offset: TimeUs(0),
                tally: &t,
                gauges: &gauges,
                gauge: (no / 16) % 2,
                effects: &[],
                notes: &notes,
                offsets: skin_model::Offsets::NONE,
                input: &eval::InputFacts::NONE,
                complete: false,
                target: TARGETS[no % TARGETS.len()],
                layout: &views.layouts[(no / 11) % views.layouts.len()],
                turntable: 0,
                lanes: view,
                readouts: readouts_for(view),
                playing: true,
                closing: None,
            };
            let out = if no % 10 == 9 { &mut small } else { &mut batch };
            out.clear();
            skin.build(&mut play, &frame, &loaded.uvs, out)
                .expect("the frame builds");
            drops += out.dropped();
            most[k] = most[k].max(out.quads().len());
        }
        for no in 0..60 {
            batch.clear();
            let frame = loading_frame(views, descs, TimeUs(-5_000 * MS), no);
            skin.build_loading(&mut loading, &frame, &loaded.uvs, &mut batch)
                .expect("the loading frame builds");
            loading_quads[k] = loading_quads[k].max(batch.quads().len());
        }
        TRACK.with(|t| t.set(false));
    }
    assert_eq!(
        ALLOCS.swap(0, Ordering::Relaxed),
        0,
        "large gauges: allocations"
    );
    // The flicker's tip beside its segments, and every segment of the
    // other; the short batch dropped the rest; loading drew every unlit
    // segment.
    assert_eq!(most, [16_384, 16_384]);
    assert!(drops > 0);
    assert_eq!(loading_quads, [16_383, 16_384]);
}

/// The readouts a stretch's frame `frame_no` gives with `view`: nothing
/// known, then tempo, hi-speed and an exact duration, then a duration
/// without a hi-speed, changing every 7 frames; a cover the view draws at
/// 0 is off with a stored amount, the adjustment mode toggles, and
/// HIDDEN+ at 0 switches on and off.
fn stretch_readouts(frame_no: usize, view: LaneView) -> FrameReadouts {
    let base = readouts_for(view);
    let off = |shown: u16, stored: u16| if shown == 0 { stored } else { shown };
    let covers = CoverSettings {
        cover: off(view.cover.min(1_000), 400),
        lift: off(view.lift.min(1_000), 150),
        cover_enabled: view.cover != 0,
        lift_enabled: view.lift != 0,
        adjusting: frame_no % 11 < 3,
        hidden_enabled: view.hidden != 0 || frame_no % 13 < 7,
        ..base.covers
    };
    let k = u32::try_from(frame_no % 1_000).unwrap_or(0);
    match frame_no / 7 % 3 {
        0 => base,
        1 => FrameReadouts {
            fix_hispeed: 0,
            bpm_current_x100: Some(15_000 + k),
            hispeed_x100: Some(100 + k),
            duration: Some(DurationRatio {
                numerator: 600 * 15_000 * u128::from(k + 1),
                denominator: u128::from(15_000 + k),
            }),
            covers,
            // Every variant known, the last two of them exactly half way
            // in ms and in green, and the native block in Floating.
            duration_variants: std::array::from_fn(|i| {
                let i = u128::try_from(i).unwrap_or(0);
                Some(DurationRatio {
                    numerator: 1_875 * (i + 1) + u128::from(k),
                    denominator: 2 + i % 2,
                })
            }),
            native: NativeReadouts {
                duration: Some(DurationRatio {
                    numerator: 1_005,
                    denominator: 2,
                }),
                target_green: Some(300 + k % 7),
                floating: true,
            },
        },
        _ => FrameReadouts {
            duration: Some(DurationRatio {
                numerator: u128::MAX,
                denominator: u128::MAX / 667,
            }),
            covers,
            ..base
        },
    }
}

/// Readouts allocate nothing per round, each prepared untracked: the
/// first build and later ones through READY, a STOP, tempo and
/// control changes and past the end, then an invalid frame and a build
/// on the poisoned play; and an invalid first build.
fn readouts_do_not_allocate(loaded: &Loaded, descs: &[GaugeDesc]) {
    let skin = &loaded.skin;
    let chart = ChartBuilder::new(150_000)
        .sound("a.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .stop(TimeUs(2_000 * MS), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(4_000 * MS))
        .bgm(TimeUs(6_000 * MS), SoundId(0))
        .build()
        .unwrap();
    let section = Section {
        start: TimeUs(1_500 * MS),
        end: TimeUs(5_000 * MS),
        lead_in: TimeUs(1_000 * MS),
        initial_gauges: vec![GaugeValue(70 * GaugeValue::PERCENT), GaugeValue::FULL],
    };
    let full = PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: descs,
        attempt: Attempt::Full {
            lead_in: TimeUs(1_000 * MS),
        },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: Some(BpmRange {
            min_x100: 15_000,
            max_x100: 15_000,
        }),
        bpm_whole_change: None,
        play_level: None,
        score: skin_model::PlayScore::NONE,
    };
    let round = PlayContext {
        attempt: Attempt::Section(&section),
        bpm_range: None,
        ..full
    };
    let t = tally(0);
    let gauges = [descs[0].start, descs[1].start].map(state);
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let lanes = LaneView {
        gain: Gain::new(8, 3),
        cover: 250,
        lift: 0,
        hidden: 0,
    };
    let notes = NoteFacts::default();
    let frame = |now: i64, readouts: FrameReadouts| SkinFrame {
        app: skin_model::AppReadouts::LAUNCH,
        state_us: 0,
        now: TimeUs(now),
        display_offset: TimeUs(0),
        tally: &t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        notes: &notes,
        offsets: skin_model::Offsets::NONE,
        input: &eval::InputFacts::NONE,
        complete: false,
        target: TARGETS[0],
        layout: &SkinLayout::AUTHOR,
        turntable: 0,
        lanes,
        readouts,
        playing: true,
        closing: None,
    };
    let invalid = FrameReadouts {
        duration: Some(DurationRatio {
            numerator: 1,
            denominator: 0,
        }),
        ..readouts_for(lanes)
    };
    // Per round: the remaining seconds at the first and last frames, and
    // whether a duration was seen known and unknown.
    let mut seen = Vec::new();
    for (name, ctx) in [("readouts", &full), ("readouts round", &round)] {
        for _round in 0..2 {
            let mut play = skin.prepare(&chart, ctx).unwrap();
            let mut results = [Ok(()); 3];
            let (mut first, mut last, mut known) = (None, 0, [false; 2]);
            TRACK.with(|t| t.set(true));
            for (i, now) in (-1_000 * MS..=8_000 * MS)
                .step_by(16 * MS as usize)
                .enumerate()
            {
                batch.clear();
                let r = skin.build(
                    &mut play,
                    &frame(now, stretch_readouts(i, lanes)),
                    &loaded.uvs,
                    &mut batch,
                );
                results[0] = results[0].and(r);
                let fields = play.state().fields;
                last = fields[usize::from(field::TIME_REMAINING_S)];
                first.get_or_insert(last);
                known[usize::from(fields[usize::from(field::DURATION_VALID)] == 1)] = true;
            }
            results[1] = skin.build(
                &mut play,
                &frame(8_100 * MS, invalid),
                &loaded.uvs,
                &mut batch,
            );
            results[2] = skin.build(
                &mut play,
                &frame(8_200 * MS, readouts_for(lanes)),
                &loaded.uvs,
                &mut batch,
            );
            TRACK.with(|t| t.set(false));
            assert_eq!(ALLOCS.load(Ordering::Relaxed), 0, "{name}: allocations");
            assert_eq!(
                results,
                [
                    Ok(()),
                    Err(BuildError::InvalidReadouts),
                    Err(BuildError::InvalidReadouts)
                ],
                "{name}"
            );
            seen.push((first, last, known));
        }
        // An invalid first build, and the poisoned play after it.
        let mut play = skin.prepare(&chart, ctx).unwrap();
        TRACK.with(|t| t.set(true));
        let first = skin.build(&mut play, &frame(0, invalid), &loaded.uvs, &mut batch);
        let after = skin.build(
            &mut play,
            &frame(16 * MS, readouts_for(lanes)),
            &loaded.uvs,
            &mut batch,
        );
        TRACK.with(|t| t.set(false));
        assert_eq!(
            ALLOCS.load(Ordering::Relaxed),
            0,
            "{name}: invalid first build"
        );
        assert_eq!(first, Err(BuildError::InvalidReadouts));
        assert_eq!(after, Err(BuildError::InvalidReadouts));
    }
    // The chart's last object at 6 s, the round's end at 5 s from 1.5 s;
    // past both nothing remains.
    assert_eq!(
        seen,
        [
            (Some(6), 0, [true; 2]),
            (Some(6), 0, [true; 2]),
            (Some(4), 0, [true; 2]),
            (Some(4), 0, [true; 2])
        ]
    );
}

/// The rhythm clock (`rhythm`) read by an element and a `when` timer,
/// frame by frame across bar lines, a tempo change, a STOP, the completed
/// tail past the progress end + 5 s, a READY rearm whose `now` goes back,
/// Closing and a terminal frame without it: nothing allocates; with a
/// positive control.
fn rhythm_does_not_allocate(descs: &[GaugeDesc]) {
    let chart = ChartBuilder::new(150_000)
        .bar(TimeUs(0))
        .bar(TimeUs(1_600 * MS))
        .bpm(TimeUs(2_000 * MS), 180_000)
        .stop(TimeUs(2_500 * MS), TimeUs(500 * MS))
        .bar(TimeUs(3_600 * MS))
        .tap(lane(1), TimeUs(4_000 * MS))
        .bar(TimeUs(4_800 * MS))
        .build()
        .unwrap();
    let skin = play_support::load(
        vec![bool_program("elapsed(rhythm) < 500")],
        vec![
            play_support::element(TimerRef::Builtin(timer::RHYTHM), 1),
            play_support::element(TimerRef::When(0), 2),
        ],
    )
    .unwrap();
    let uvs = play_support::uvs(&skin);
    let ctx = PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: descs,
        attempt: Attempt::Full {
            lead_in: TimeUs(1_000 * MS),
        },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(4_000 * MS),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
        score: skin_model::PlayScore::NONE,
    };
    let t = tally(0);
    let gauges = [descs[0].start, descs[1].start].map(state);
    let notes = NoteFacts::default();
    let lanes = LaneView::NONE;
    // (now, playing, complete, closing)
    let frame = |now: i64, playing: bool, complete: bool, closing: Option<TimeUs>| SkinFrame {
        app: skin_model::AppReadouts::LAUNCH,
        state_us: 0,
        now: TimeUs(now),
        display_offset: TimeUs(0),
        tally: &t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        notes: &notes,
        offsets: skin_model::Offsets::NONE,
        input: &eval::InputFacts::NONE,
        complete,
        target: TARGETS[0],
        layout: &SkinLayout::AUTHOR,
        turntable: 0,
        lanes,
        readouts: readouts_for(lanes),
        playing,
        closing,
    };
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let mut play = skin.prepare(&chart, &ctx).unwrap();
    // The readings seen: off, and every value bucket of 250 ms.
    let (mut off, mut on, mut drawn) = (0, [false; 4], 0);
    let mut read = |play: &Play, batch: &QuadBatch| {
        let ms = play.state().timers[usize::from(timer::RHYTHM)];
        if ms == skin_model::TIMER_OFF {
            off += 1;
        } else if let Some(b) = on.get_mut(usize::try_from(ms / 250).unwrap_or(0).min(3)) {
            *b = true;
        }
        drawn += batch.quads().len();
    };
    let mut results = Ok(());
    TRACK.with(|t| t.set(true));
    // READY, playing, then the completed tail to past 4 s + 5 s.
    for now in (-1_000 * MS..=10_000 * MS).step_by(16 * MS as usize) {
        batch.clear();
        let playing = now < 4_100 * MS;
        let f = frame(now, playing, !playing, None);
        results = results.and(skin.build(&mut play, &f, &uvs, &mut batch));
        read(&play, &batch);
    }
    // A rearm: READY again from below the cursor's row, then play into
    // Closing at 3 s, then a terminal frame without Closing.
    play.rearm();
    for now in (-1_000 * MS..=4_500 * MS).step_by(16 * MS as usize) {
        batch.clear();
        let closing = (now >= 3_000 * MS).then_some(TimeUs(3_000 * MS));
        let f = frame(now, closing.is_none(), false, closing);
        results = results.and(skin.build(&mut play, &f, &uvs, &mut batch));
        read(&play, &batch);
    }
    batch.clear();
    results = results.and(skin.build(
        &mut play,
        &frame(4_600 * MS, false, false, None),
        &uvs,
        &mut batch,
    ));
    read(&play, &batch);
    TRACK.with(|t| t.set(false));
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0, "rhythm: allocations");
    assert_eq!(results, Ok(()));
    assert!(
        off > 0 && on == [true; 4] && drawn > 0,
        "{off} {on:?} {drawn}"
    );
    // The positive control: an allocation on this thread is counted.
    TRACK.with(|t| t.set(true));
    std::hint::black_box(Vec::<u8>::with_capacity(16));
    TRACK.with(|t| t.set(false));
    assert!(ALLOCS.load(Ordering::Relaxed) > 0, "the counter counts");
    ALLOCS.store(0, Ordering::Relaxed);
}

/// The whole chart's MAIN tempo (`chart.main_bpm`, 150.7 BPM shown as 150)
/// read by a `when` timer's program and its validity by an element, over a
/// play's frames, a rearm and a practice round at k = 10: nothing
/// allocates; with a positive control.
fn main_bpm_does_not_allocate(descs: &[GaugeDesc]) {
    let chart = ChartBuilder::new(120_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .bpm(TimeUs(1_500 * MS), 180_000)
        .tap(lane(2), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let skin = play_support::load(
        vec![bool_program(
            "chart.main_bpm == 150 && chart.main_bpm_valid",
        )],
        vec![play_support::element(TimerRef::When(0), 1)],
    )
    .unwrap();
    let uvs = play_support::uvs(&skin);
    let round = Section {
        start: TimeUs(500 * MS),
        end: TimeUs(1_200 * MS),
        lead_in: TimeUs(0),
        initial_gauges: vec![descs[0].start, descs[1].start],
    };
    let score = skin_model::PlayScore {
        main: bms_model::Tempo::from_milli(150_700),
        ..skin_model::PlayScore::NONE
    };
    let ctx = |attempt| PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: descs,
        attempt,
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(2_000 * MS),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
        score,
    };
    let full = ctx(Attempt::Full {
        lead_in: TimeUs(500 * MS),
    });
    let practice = PlayContext {
        score: skin_model::PlayScore {
            speed: bms_model::Speed::from_twentieths(10).unwrap(),
            ..score
        },
        ..ctx(Attempt::Section(&round))
    };
    let t = tally(0);
    let gauges = [descs[0].start, descs[1].start].map(state);
    let notes = NoteFacts::default();
    let lanes = LaneView::NONE;
    let frame = |now: i64| SkinFrame {
        state_us: 0,
        now: TimeUs(now),
        display_offset: TimeUs(0),
        tally: &t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        notes: &notes,
        offsets: skin_model::Offsets::NONE,
        input: &eval::InputFacts::NONE,
        complete: false,
        target: TARGETS[0],
        layout: &SkinLayout::AUTHOR,
        turntable: 0,
        lanes,
        readouts: readouts_for(lanes),
        app: skin_model::AppReadouts::LAUNCH,
        playing: true,
        closing: None,
    };
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let mut plays = [
        skin.prepare(&chart, &full).unwrap(),
        skin.prepare(&chart, &practice).unwrap(),
    ];
    let (mut read, mut drawn) = (Vec::with_capacity(512), 0);
    let mut results = Ok(());
    TRACK.with(|t| t.set(true));
    for play in &mut plays {
        for pass in 0..2 {
            if pass == 1 {
                play.rearm();
            }
            for now in (-500 * MS..=2_500 * MS).step_by(16 * MS as usize) {
                batch.clear();
                results = results.and(skin.build(play, &frame(now), &uvs, &mut batch));
                let fields = play.state().fields;
                if read.len() < read.capacity() {
                    read.push((
                        fields[usize::from(field::CHART_MAIN_BPM)],
                        fields[usize::from(field::CHART_MAIN_BPM_VALID)],
                    ));
                }
                drawn += batch.quads().len();
            }
        }
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0, "main BPM: allocations");
    assert_eq!(results, Ok(()));
    assert!(
        !read.is_empty() && read.iter().all(|&r| r == (150, 1)),
        "{read:?}"
    );
    assert!(drawn > 0, "{drawn}");
    // The positive control: an allocation on this thread is counted.
    TRACK.with(|t| t.set(true));
    std::hint::black_box(Vec::<u8>::with_capacity(16));
    TRACK.with(|t| t.set(false));
    assert!(ALLOCS.load(Ordering::Relaxed) > 0, "the counter counts");
    ALLOCS.store(0, Ordering::Relaxed);
}

/// The load's and the play's progress bars (graphs 102 and 101) and the
/// chart's 1× length (`chart.length_ms`, read by a `when` timer's
/// program), over a play's frames from READY past its end, a rearm, a
/// practice round at k = 10 and loading frames with and without a chosen
/// length: nothing allocates; each bar draws, the length reads 125 400
/// throughout; with a positive control.
fn progress_and_length_do_not_allocate(descs: &[GaugeDesc]) {
    let mut chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    chart.meta.length = TimeUs(125_400 * MS);
    let rect = |r: [i32; 4]| {
        let mut props = [None; 9];
        for (p, v) in props.iter_mut().zip(r) {
            *p = Some(v);
        }
        Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props,
            }],
            Ease::Linear,
            End::Hold,
        )
        .unwrap()
    };
    let bar = |i: u32, r| ElementDef {
        source: Source::RatioBar(i),
        track: rect(r),
        ..play_support::element(TimerRef::Builtin(timer::SCREEN), 0)
    };
    let ratio_bars = vec![
        skin_model::RatioBarDef {
            image: 0,
            form: skin_model::RatioForm::Bar { vertical: false },
            source: skin_model::RatioSource::LoadProgress,
        },
        skin_model::RatioBarDef {
            image: 0,
            form: skin_model::RatioForm::Bar { vertical: true },
            source: skin_model::RatioSource::SongProgress,
        },
    ];
    let skin = Skin::new_play(
        play_support::def(
            vec![bool_program("chart.length_ms == 125400")],
            play_support::white_sheet(),
            vec![
                bar(0, [0, 0, 200, 20]),
                bar(1, [300, 0, 20, 200]),
                play_support::element(TimerRef::When(0), 400),
            ],
        ),
        skin_model::PlayDefs {
            ratio_bars,
            ..skin_model::PlayDefs::default()
        },
    )
    .unwrap();
    let uvs = play_support::uvs(&skin);
    let round = Section {
        start: TimeUs(500 * MS),
        end: TimeUs(1_200 * MS),
        lead_in: TimeUs(0),
        initial_gauges: vec![descs[0].start, descs[1].start],
    };
    let ctx = |attempt, speed| PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: descs,
        attempt,
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(2_000 * MS),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
        score: skin_model::PlayScore {
            speed,
            ..skin_model::PlayScore::NONE
        },
    };
    let full = ctx(
        Attempt::Full {
            lead_in: TimeUs(500 * MS),
        },
        bms_model::Speed::NORMAL,
    );
    let practice = ctx(
        Attempt::Section(&round),
        bms_model::Speed::from_twentieths(10).unwrap(),
    );
    let t = tally(0);
    let gauges = [descs[0].start, descs[1].start].map(state);
    let notes = NoteFacts::default();
    let lanes = LaneView::NONE;
    let frame = |now: i64| SkinFrame {
        state_us: 0,
        now: TimeUs(now),
        display_offset: TimeUs(0),
        tally: &t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        notes: &notes,
        offsets: skin_model::Offsets::NONE,
        input: &eval::InputFacts::NONE,
        complete: false,
        target: TARGETS[0],
        layout: &SkinLayout::AUTHOR,
        turntable: 0,
        lanes,
        readouts: readouts_for(lanes),
        app: skin_model::AppReadouts::LAUNCH,
        playing: true,
        closing: None,
    };
    let loading_frame = |no: i64| LoadingFrame {
        app: skin_model::AppReadouts::LAUNCH,
        display_offset: TimeUs(0),
        fix_hispeed: 0,
        difficulty: None,
        bga: None,
        length: (no % 3 != 2).then_some(TimeUs(125_400 * MS)),
        autoplay: false,
        now: TimeUs(no * 16 * MS),
        load: LoadState {
            since: TimeUs(0),
            percent: i32::try_from(no % 101).unwrap(),
        },
        target: TARGETS[0],
        layout: &SkinLayout::AUTHOR,
        turntable: 0,
        lanes,
        covers: readouts_for(lanes).covers,
        gauge: None,
        play_level: None,
    };
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let mut plays = [
        skin.prepare(&chart, &full).unwrap(),
        skin.prepare(&chart, &practice).unwrap(),
    ];
    let mut loading = skin.prepare_loading(Side::Left);
    let (mut read, mut drawn, mut loading_drawn) = (Vec::with_capacity(1_024), 0, 0);
    let mut results = Ok(());
    TRACK.with(|t| t.set(true));
    for play in &mut plays {
        for pass in 0..2 {
            if pass == 1 {
                play.rearm();
            }
            for now in (-500 * MS..=3_000 * MS).step_by(16 * MS as usize) {
                batch.clear();
                results = results.and(skin.build(play, &frame(now), &uvs, &mut batch));
                if read.len() < read.capacity() {
                    read.push(play.state().fields[usize::from(field::CHART_LENGTH_MS)]);
                }
                drawn += batch.quads().len();
            }
        }
    }
    for no in 0..300 {
        batch.clear();
        results =
            results.and(skin.build_loading(&mut loading, &loading_frame(no), &uvs, &mut batch));
        loading_drawn += batch.quads().len();
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "progress and length: allocations"
    );
    assert_eq!(results, Ok(()));
    assert!(
        !read.is_empty() && read.iter().all(|&r| r == 125_400),
        "{read:?}"
    );
    assert!(drawn > 0 && loading_drawn > 0, "{drawn} {loading_drawn}");
    // The positive control: an allocation on this thread is counted.
    TRACK.with(|t| t.set(true));
    std::hint::black_box(Vec::<u8>::with_capacity(16));
    TRACK.with(|t| t.set(false));
    assert!(ALLOCS.load(Ordering::Relaxed) > 0, "the counter counts");
    ALLOCS.store(0, Ordering::Relaxed);
}

/// A program on every path (design.md "Scope of No Allocation", the
/// program's executor): a float value and the draw after it, a sentinel
/// value and the draw it suppresses, a gate and a guard that turn false
/// and true as the combo moves, a caught error, and a branch; its first
/// frame and later ones, then a poisoned play; with a positive control.
fn programs_do_not_allocate(chart: &Chart, ctx: &PlayContext<'_>) {
    use present_support::{Builder, Cb, read, slot};
    use skin_model::present::{Arith, Cmp, Conv, Insn, MathFn, Val};
    let combo = || read("combo", skin_model::Ty::Int, None);
    let mut b = Builder::default();
    // combo / 4 * 100, a float, then its floor; and a draw after it.
    let v = b.push(Cb::code(
        vec![
            Insn::Read { dst: 0, read: 0 },
            Insn::Const { dst: 1, k: 0 },
            Insn::Arith {
                op: Arith::Div,
                dst: 0,
                a: 0,
                b: 1,
            },
            Insn::Const { dst: 1, k: 1 },
            Insn::Arith {
                op: Arith::Mul,
                dst: 0,
                a: 0,
                b: 1,
            },
            Insn::Math {
                f: MathFn::Floor,
                dst: 0,
                a: 0,
            },
            Insn::Ret { src: 0 },
        ],
        vec![Val::Int(4), Val::Float(100.0)],
        vec![combo()],
        Conv::Int,
    ));
    b.push(Cb::ret(Val::Bool(true), Conv::Truthy).after(v));
    // A sentinel and the draw it suppresses.
    let s = b.push(Cb::ret(Val::Int(i64::from(i32::MAX)), Conv::Int));
    b.push(Cb::ret(Val::Bool(true), Conv::Truthy).after(s));
    // A gate, then a guard, false at a low combo.
    b.push(Cb::ret(Val::Bool(true), Conv::Truthy).gate(read(
        "combo > 3",
        skin_model::Ty::Bool,
        None,
    )));
    b.push(Cb::code(
        vec![Insn::Read { dst: 0, read: 0 }, Insn::Ret { src: 0 }],
        vec![],
        vec![read("combo", skin_model::Ty::Int, Some("combo > 5"))],
        Conv::Int,
    ));
    // A caught error: nil + 1.
    b.push(Cb::code(
        vec![
            Insn::Const { dst: 0, k: 0 },
            Insn::Const { dst: 1, k: 1 },
            Insn::Arith {
                op: Arith::Add,
                dst: 0,
                a: 0,
                b: 1,
            },
            Insn::Ret { src: 0 },
        ],
        vec![Val::Nil, Val::Int(1)],
        vec![],
        Conv::Int,
    ));
    // A branch: combo < 2 and 1, or the combo.
    b.push(Cb::code(
        vec![
            Insn::Read { dst: 0, read: 0 },
            Insn::Const { dst: 1, k: 0 },
            Insn::Cmp {
                op: Cmp::Lt,
                dst: 1,
                a: 0,
                b: 1,
            },
            Insn::Test {
                cond: 1,
                when: false,
                to: 5,
            },
            Insn::Const { dst: 0, k: 1 },
            Insn::Ret { src: 0 },
        ],
        vec![Val::Int(2), Val::Int(1)],
        vec![combo()],
        Conv::Int,
    ));
    // Slider values (`Conv::Float`), each read by a paint's slider: the
    // combo times 1e10, a finite value drawn and, from a combo of 2, past
    // the bound; the combo over 0, non-finite (at 0) or NaN; a caught
    // error; and one unavailable at a low combo.
    let mul = |k: u16| Insn::Arith {
        op: Arith::Mul,
        dst: 0,
        a: 0,
        b: u8::try_from(k).unwrap(),
    };
    let floats = [
        b.push(Cb::code(
            vec![
                Insn::Read { dst: 0, read: 0 },
                Insn::Const { dst: 1, k: 0 },
                mul(1),
                Insn::Ret { src: 0 },
            ],
            vec![Val::Float(1e10)],
            vec![combo()],
            Conv::Float,
        )),
        b.push(Cb::code(
            vec![
                Insn::Read { dst: 0, read: 0 },
                Insn::Const { dst: 1, k: 0 },
                Insn::Arith {
                    op: Arith::Div,
                    dst: 0,
                    a: 0,
                    b: 1,
                },
                Insn::Ret { src: 0 },
            ],
            vec![Val::Int(0)],
            vec![combo()],
            Conv::Float,
        )),
        b.push(Cb::code(
            vec![
                Insn::Const { dst: 0, k: 0 },
                Insn::Const { dst: 1, k: 1 },
                Insn::Arith {
                    op: Arith::Add,
                    dst: 0,
                    a: 0,
                    b: 1,
                },
                Insn::Ret { src: 0 },
            ],
            vec![Val::Nil, Val::Int(1)],
            vec![],
            Conv::Float,
        )),
        b.push(Cb::code(
            vec![Insn::Read { dst: 0, read: 0 }, Insn::Ret { src: 0 }],
            vec![],
            vec![read("combo", skin_model::Ty::Int, Some("combo > 5"))],
            Conv::Float,
        )),
    ];
    let sliders: Vec<CoverPaintDef> = floats
        .iter()
        .map(|&k| CoverPaintDef {
            image: CoverImage::Image(0),
            slider: Some(skin_model::Slider {
                range: 100,
                rate: skin_model::SliderRate::Program(k),
                direction: skin_model::SliderDirection::Down,
            }),
            motion: CoverMotion::None,
            trim: CoverTrim::None,
            gate_track: None,
        })
        .collect();
    let sig = b.signature();
    let n = sig.len();
    let paints = (0..sliders.len()).map(|p| {
        widget(
            Source::CoverPaint(u32::try_from(p).unwrap()),
            track(0, Ease::Linear, End::Hold),
            TimerRef::Builtin(timer::READY),
            Place::SCREEN,
        )
    });
    let elements = (0..n - sliders.len())
        .map(|k| {
            let mut e = widget(
                Source::Image(0),
                track(0, Ease::Linear, End::Hold),
                TimerRef::Builtin(timer::READY),
                Place::SCREEN,
            );
            e.visible = Some(present_support::expr(
                &format!("lua.{k}.ok"),
                &sig,
                skin_model::Ty::Bool,
            ));
            e
        })
        .chain(paints)
        .collect();
    let skin = present_support::load(
        Some(b.0),
        Vec::new(),
        elements,
        skin_model::WidgetDefs {
            cover_paints: sliders,
            ..skin_model::WidgetDefs::default()
        },
    )
    .unwrap();
    let uvs = play_support::uvs(&skin);
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let notes = NoteFacts::default();
    let layout = SkinLayout::AUTHOR;
    let frame = |now: i64, t: &'static Tally| SkinFrame {
        app: skin_model::AppReadouts::LAUNCH,
        state_us: 0,
        now: TimeUs(now),
        display_offset: TimeUs(0),
        tally: t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        notes: &notes,
        offsets: skin_model::Offsets::NONE,
        input: &eval::InputFacts::NONE,
        complete: false,
        target: TARGETS[0],
        layout: &layout,
        turntable: 0,
        lanes: play_support::VIEW,
        readouts: readouts_for(play_support::VIEW),
        playing: true,
        closing: None,
    };
    let tallies: &'static [Tally] =
        Box::leak((0..10).map(tally).collect::<Vec<_>>().into_boxed_slice());
    let mut play = skin.prepare(chart, ctx).unwrap();
    // Every ok bit seen 0 and 1 where a path makes it so.
    let mut seen = vec![[false; 2]; n];
    TRACK.with(|t| t.set(true));
    for (i, now) in (0..3_000 * MS).step_by(16 * MS as usize).enumerate() {
        batch.clear();
        skin.build(
            &mut play,
            &frame(now, &tallies[i / 7 % 10]),
            &uvs,
            &mut batch,
        )
        .expect("the frame builds");
        let f = play.state().fields;
        for (k, s) in seen.iter_mut().enumerate() {
            s[usize::from(f[slot(k) + 1] != 0)] = true;
        }
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0, "programs: allocations");
    assert_eq!(
        seen,
        [
            [false, true],
            [false, true],
            [true, false],
            [true, false],
            [true, true],
            [true, true],
            [false, true],
            [false, true],
            [false, true],
            [false, true],
            [false, true],
            [true, true],
        ],
        "the paths each callback took"
    );
    // A poisoned play: refused, and refused again, without allocating.
    let counted = PlayContext {
        empty_poor_counted: true,
        ..*ctx
    };
    let mut play = skin.prepare(chart, &counted).unwrap();
    let bad: &'static Tally = Box::leak(Box::new(Tally {
        empty_poor: 1,
        ..tally(0)
    }));
    TRACK.with(|t| t.set(true));
    let first = skin.build(&mut play, &frame(0, bad), &uvs, &mut batch);
    let after = skin.build(&mut play, &frame(16 * MS, &tallies[0]), &uvs, &mut batch);
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "poisoned program: allocations"
    );
    assert_eq!(first, Err(BuildError::InconsistentTally));
    assert_eq!(after, Err(BuildError::InconsistentTally));
    // The positive control: an allocation on this thread is counted.
    TRACK.with(|t| t.set(true));
    std::hint::black_box(Vec::<u8>::with_capacity(16));
    TRACK.with(|t| t.set(false));
    assert!(ALLOCS.load(Ordering::Relaxed) > 0, "the counter counts");
    ALLOCS.store(0, Ordering::Relaxed);
}

/// The program's numerics past ±2^53 (design.md "Scope of No
/// Allocation", the program's executor; "beatoraja Lua Skins", numbers):
/// reads at `i32::MAX` multiplied past ±2^53 and past `i64`, promoted to
/// the double; that product divided; the floor of 2^60 kept a float;
/// negation and `abs` of ±2^53; and the state clock read as the source's
/// double, past 2^53 µs and negative, each converted by `to_int`. Every
/// frame's outputs are the numerics' own values, so each path ran; with a
/// positive control.
fn promoted_numerics_do_not_allocate(chart: &Chart, ctx: &PlayContext<'_>) {
    use present_support::{Builder, Cb, read, slot};
    use skin_model::present::num::{arith, clock, math, neg, to_int};
    use skin_model::present::{Arith, Conv, Insn, MathFn, Val};
    let max = || read("2147483647", skin_model::Ty::Int, None);
    let ar = |op, dst, a, b| Insn::Arith { op, dst, a, b };
    let r = Val::Int(i64::from(i32::MAX));
    let two_53 = Val::Int(1 << 53);
    let mut b = Builder::default();
    let mut want: Vec<Box<dyn Fn(i64) -> i32>> = Vec::new();
    // r * r * r: past ±2^53, then past `i64`.
    b.push(Cb::code(
        vec![
            Insn::Read { dst: 0, read: 0 },
            Insn::Read { dst: 1, read: 0 },
            ar(Arith::Mul, 0, 0, 1),
            ar(Arith::Mul, 0, 0, 1),
            Insn::Ret { src: 0 },
        ],
        vec![],
        vec![max()],
        Conv::Int,
    ));
    let rrr = arith(Arith::Mul, arith(Arith::Mul, r, r).unwrap(), r).unwrap();
    want.push(Box::new(move |_| to_int(rrr)));
    // r * r / 7.
    b.push(Cb::code(
        vec![
            Insn::Read { dst: 0, read: 0 },
            Insn::Read { dst: 1, read: 0 },
            ar(Arith::Mul, 0, 0, 1),
            Insn::Const { dst: 1, k: 0 },
            ar(Arith::Div, 0, 0, 1),
            Insn::Ret { src: 0 },
        ],
        vec![Val::Int(7)],
        vec![max()],
        Conv::Int,
    ));
    let rr7 = arith(Arith::Div, arith(Arith::Mul, r, r).unwrap(), Val::Int(7)).unwrap();
    want.push(Box::new(move |_| to_int(rr7)));
    // floor(2^60) * r.
    b.push(Cb::code(
        vec![
            Insn::Const { dst: 0, k: 0 },
            Insn::Math {
                f: MathFn::Floor,
                dst: 0,
                a: 0,
            },
            Insn::Read { dst: 1, read: 0 },
            ar(Arith::Mul, 0, 0, 1),
            Insn::Ret { src: 0 },
        ],
        vec![Val::Float(2.0_f64.powi(60))],
        vec![max()],
        Conv::Int,
    ));
    let fl = arith(
        Arith::Mul,
        math(MathFn::Floor, Val::Float(2.0_f64.powi(60))).unwrap(),
        r,
    )
    .unwrap();
    want.push(Box::new(move |_| to_int(fl)));
    // -(2^53) - abs(-(2^53)) - r: past ±2^53 again.
    b.push(Cb::code(
        vec![
            Insn::Const { dst: 0, k: 0 },
            Insn::Neg { dst: 0, a: 0 },
            Insn::Const { dst: 1, k: 1 },
            Insn::Math {
                f: MathFn::Abs,
                dst: 1,
                a: 1,
            },
            ar(Arith::Sub, 0, 0, 1),
            Insn::Read { dst: 1, read: 0 },
            ar(Arith::Sub, 0, 0, 1),
            Insn::Ret { src: 0 },
        ],
        vec![two_53, Val::Int(-(1 << 53))],
        vec![max()],
        Conv::Int,
    ));
    let na = arith(
        Arith::Sub,
        arith(
            Arith::Sub,
            neg(two_53).unwrap(),
            math(MathFn::Abs, Val::Int(-(1 << 53))).unwrap(),
        )
        .unwrap(),
        r,
    )
    .unwrap();
    want.push(Box::new(move |_| to_int(na)));
    // The state clock plus 3.
    b.push(Cb::code(
        vec![
            Insn::StateUs { dst: 0 },
            Insn::Const { dst: 1, k: 0 },
            ar(Arith::Add, 0, 0, 1),
            Insn::Ret { src: 0 },
        ],
        vec![Val::Int(3)],
        vec![],
        Conv::Int,
    ));
    want.push(Box::new(|us| {
        to_int(arith(Arith::Add, clock(us), Val::Int(3)).unwrap())
    }));
    let sig = b.signature();
    let n = sig.len();
    let elements = (0..n)
        .map(|k| {
            let mut e = widget(
                Source::Image(0),
                track(0, Ease::Linear, End::Hold),
                TimerRef::Builtin(timer::READY),
                Place::SCREEN,
            );
            e.visible = Some(present_support::expr(
                &format!("lua.{k}.ok"),
                &sig,
                skin_model::Ty::Bool,
            ));
            e
        })
        .collect();
    let skin = present_support::load(
        Some(b.0),
        Vec::new(),
        elements,
        skin_model::WidgetDefs::default(),
    )
    .unwrap();
    let uvs = play_support::uvs(&skin);
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let notes = NoteFacts::default();
    let layout = SkinLayout::AUTHOR;
    let t = tally(3);
    let frame = |now: i64, state_us: i64| SkinFrame {
        app: skin_model::AppReadouts::LAUNCH,
        state_us,
        now: TimeUs(now),
        display_offset: TimeUs(0),
        tally: &t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        notes: &notes,
        offsets: skin_model::Offsets::NONE,
        input: &eval::InputFacts::NONE,
        complete: false,
        target: TARGETS[0],
        layout: &layout,
        turntable: 0,
        lanes: play_support::VIEW,
        readouts: readouts_for(play_support::VIEW),
        playing: true,
        closing: None,
    };
    let clocks = [-1, -7_000_003, (1 << 53) + 1, (1 << 53) + 2, 1 << 53, 5_000];
    let mut play = skin.prepare(chart, ctx).unwrap();
    let mut outs = vec![[0; 2]; n * clocks.len()];
    TRACK.with(|t| t.set(true));
    for (i, &us) in clocks.iter().enumerate() {
        batch.clear();
        let now = i64::try_from(i).unwrap() * 16 * MS;
        skin.build(&mut play, &frame(now, us), &uvs, &mut batch)
            .expect("the frame builds");
        let f = play.state().fields;
        for k in 0..n {
            outs[i * n + k] = [f[slot(k)], f[slot(k) + 1]];
        }
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "promoted numerics: allocations"
    );
    for (i, &us) in clocks.iter().enumerate() {
        for (k, w) in want.iter().enumerate() {
            let v = w(us);
            let ok = i32::from(v != i32::MIN && v != i32::MAX);
            assert_eq!(outs[i * n + k], [v, ok], "callback {k} at {us} µs");
        }
    }
    // The clock past 2^53 µs and negative ran: -1 + 3 is 2, and 2^53 + 1
    // µs reads as 2^53, plus 3 rounding to 2^53 + 4.
    assert_eq!(outs[n - 1], [2, 1]);
    assert_eq!(outs[2 * n + n - 1], [4, 1]);
    // The positive control: an allocation on this thread is counted.
    TRACK.with(|t| t.set(true));
    std::hint::black_box(Vec::<u8>::with_capacity(16));
    TRACK.with(|t| t.set(false));
    assert!(ALLOCS.load(Ordering::Relaxed) > 0, "the counter counts");
    ALLOCS.store(0, Ordering::Relaxed);
}

/// The application readouts of frame `no`: the presents per second up to
/// the ring's 1 024, the local time through a day's end and a year's, and
/// the uptime from 23 h 59 min 59 s on, past `i32::MAX` ms after 300
/// frames.
fn app_for(no: usize) -> skin_model::AppReadouts {
    let k = i64::try_from(no).unwrap();
    let base = if no < 300 {
        86_399_000_000
    } else {
        2_147_483_000_000
    };
    skin_model::AppReadouts {
        fps: u16::try_from(no * 7 % 1_025).unwrap(),
        uptime_us: base + k * 16_667,
        local: skin_model::LocalTime {
            year: 2_026 + u16::from(no % 600 > 300),
            month: if no % 600 > 300 { 1 } else { 12 },
            day: if no % 600 > 300 { 1 } else { 31 },
            hour: u8::try_from(23 * (no % 2)).unwrap(),
            minute: u8::try_from(no % 60).unwrap(),
            second: u8::try_from(no / 60 % 60).unwrap(),
        },
    }
}

/// The application fields (design.md "Scope of No Allocation"): numbers on
/// each of the ten and a `when` timer reading the clock, over play frames
/// and loading frames whose readouts move every frame and cross the
/// uptime's `i32::MAX` ms: nothing allocates; with a positive control.
fn app_readouts_do_not_allocate(
    chart: &Chart,
    ctx: &PlayContext<'_>,
    views: &Views,
    descs: &[GaugeDesc],
) {
    let int = |src: &str| {
        skin_model::compile(
            src,
            &skin_model::Scope::new(&skin_model::PLAY),
            skin_model::Ty::Int,
        )
        .unwrap()
    };
    let names = [
        "app.fps",
        "clock.year",
        "clock.month",
        "clock.day",
        "clock.hour",
        "clock.minute",
        "clock.second",
        "app.uptime_h",
        "app.uptime_m",
        "app.uptime_s",
    ];
    let numbers = names
        .iter()
        .map(|name| NumberDef {
            digits: DigitsDef {
                image: u32::from(DIGITS_10),
                set: GlyphSet::Ten,
                cells: 10,
                align: Align::Right,
                pad: Pad::Blank,
                space: 0,
                offsets: Vec::new(),
            },
            value: int(name),
        })
        .collect();
    let mut elements: Vec<ElementDef> = (0..10)
        .map(|k| {
            widget(
                Source::Number(k),
                track(0, Ease::Linear, End::Hold),
                TimerRef::Builtin(timer::SCREEN),
                Place::SCREEN,
            )
        })
        .collect();
    elements.push(widget(
        Source::Image(u32::from(timer::BUILTIN)),
        track(0, Ease::Linear, End::Hold),
        TimerRef::When(0),
        Place::SCREEN,
    ));
    elements.push(lanes_support::lanes_element());
    let loaded = load(
        // The sheet's cycles name 32 `when` timers.
        (0..32)
            .map(|_| bool_program("clock.hour == 23 && app.uptime_h >= 24 && app.fps > 0"))
            .collect(),
        elements,
        skin_model::WidgetDefs {
            numbers,
            ..skin_model::WidgetDefs::default()
        },
    );
    let skin = &loaded.skin;
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let notes = NoteFacts::default();
    let t = tally(3);
    let frame = |now: i64, app| SkinFrame {
        app,
        state_us: now,
        now: TimeUs(now),
        display_offset: TimeUs(0),
        tally: &t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        notes: &notes,
        offsets: skin_model::Offsets::NONE,
        input: &eval::InputFacts::NONE,
        complete: false,
        target: TARGETS[0],
        layout: &SkinLayout::AUTHOR,
        turntable: 0,
        lanes: play_support::VIEW,
        readouts: readouts_for(play_support::VIEW),
        playing: true,
        closing: None,
    };
    let mut play = skin.prepare(chart, ctx).unwrap();
    let mut loading = skin.prepare_loading(Side::Left);
    // Per frame kind, the most digits drawn and whether the timer drew.
    let (mut digits, mut timed) = ([0; 2], [false; 2]);
    let digit = |q: &skin_model::Quad| {
        loaded
            .shows
            .get(&uv_key(q.uv, q.layer))
            .is_some_and(|&(image, _)| image == DIGITS_10)
    };
    let when = |q: &skin_model::Quad| {
        loaded
            .shows
            .get(&uv_key(q.uv, q.layer))
            .is_some_and(|&(image, _)| image == timer::BUILTIN)
    };
    TRACK.with(|t| t.set(true));
    for no in 0..600 {
        let now = i64::try_from(no).unwrap() * 16 * MS;
        batch.clear();
        skin.build(&mut play, &frame(now, app_for(no)), &loaded.uvs, &mut batch)
            .expect("the play frame builds");
        digits[0] = digits[0].max(batch.quads().iter().filter(|q| digit(q)).count());
        timed[0] |= batch.quads().iter().any(when);
    }
    for no in 0..600 {
        batch.clear();
        let f = LoadingFrame {
            app: app_for(no),
            ..loading_frame(views, descs, TimeUs(0), no * 5 + 2)
        };
        skin.build_loading(&mut loading, &f, &loaded.uvs, &mut batch)
            .expect("the loading frame builds");
        digits[1] = digits[1].max(batch.quads().iter().filter(|q| digit(q)).count());
        timed[1] |= batch.quads().iter().any(when);
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "application readouts: allocations"
    );
    // Every number drew: at least a digit each, the year four and the
    // uptime's hours six once past `i32::MAX` ms (596 523).
    assert!(digits.iter().all(|&d| d >= 20), "{digits:?}");
    assert_eq!(timed, [true, true], "the clock's timer");
    // The positive control: an allocation on this thread is counted.
    TRACK.with(|t| t.set(true));
    std::hint::black_box(Vec::<u8>::with_capacity(16));
    TRACK.with(|t| t.set(false));
    assert!(ALLOCS.load(Ordering::Relaxed) > 0, "the counter counts");
    ALLOCS.store(0, Ordering::Relaxed);
}

/// A latch on a slot, the timer of two objects of one closure, built
/// frame after frame as its condition comes and goes, through a rearm:
/// slots, timer callbacks' two runs and their timers allocate nothing;
/// with a positive control.
fn slots_do_not_allocate(chart: &Chart, ctx: &PlayContext<'_>) {
    use present_support::{Builder, Cb, read};
    use skin_model::present::{Cmp, Conv, Domain, Insn, OFF, SlotDef};
    let domain = Domain {
        off: true,
        ints: Some((0, 1 << 53)),
    };
    let mut b = Builder::default();
    b.0.slots.push(SlotDef { init: OFF, domain });
    b.0.domains.push(domain);
    let k = b.push(Cb::code(
        vec![
            Insn::LoadSlot { dst: 0, slot: 0 },
            Insn::Const { dst: 1, k: 0 },
            Insn::Cmp {
                op: Cmp::Eq,
                dst: 2,
                a: 0,
                b: 1,
            },
            Insn::Test {
                cond: 2,
                when: false,
                to: 7,
            },
            Insn::Read { dst: 2, read: 0 },
            Insn::Test {
                cond: 2,
                when: false,
                to: 7,
            },
            Insn::StateUs { dst: 0 },
            Insn::StoreSlot {
                slot: 0,
                src: 0,
                domain: 0,
            },
            Insn::Ret { src: 0 },
        ],
        vec![OFF],
        vec![read("combo >= 5", skin_model::Ty::Bool, None)],
        Conv::Timer,
    ));
    let j = b.share(k);
    let elements = [k, j]
        .into_iter()
        .map(|k| {
            widget(
                Source::Image(0),
                track(0, Ease::Linear, End::Hold),
                TimerRef::Output(k),
                Place::SCREEN,
            )
        })
        .collect();
    let skin = present_support::load(
        Some(b.0),
        Vec::new(),
        elements,
        skin_model::WidgetDefs::default(),
    )
    .unwrap();
    let uvs = play_support::uvs(&skin);
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let notes = NoteFacts::default();
    let layout = SkinLayout::AUTHOR;
    let frame = |now: i64, t: &'static Tally| SkinFrame {
        app: skin_model::AppReadouts::LAUNCH,
        state_us: now + 500_000,
        now: TimeUs(now),
        display_offset: TimeUs(0),
        tally: t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        notes: &notes,
        offsets: skin_model::Offsets::NONE,
        input: &eval::InputFacts::NONE,
        complete: false,
        target: TARGETS[0],
        layout: &layout,
        turntable: 0,
        lanes: play_support::VIEW,
        readouts: readouts_for(play_support::VIEW),
        playing: true,
        closing: None,
    };
    let tallies: &'static [Tally] =
        Box::leak((0..10).map(tally).collect::<Vec<_>>().into_boxed_slice());
    let mut play = skin.prepare(chart, ctx).unwrap();
    // The timer seen off and on.
    let mut seen = [false; 2];
    TRACK.with(|t| t.set(true));
    for (i, now) in (0..3_000 * MS).step_by(16 * MS as usize).enumerate() {
        if i == 100 {
            play.rearm();
        }
        batch.clear();
        skin.build(
            &mut play,
            &frame(now, &tallies[i / 7 % 10]),
            &uvs,
            &mut batch,
        )
        .expect("the frame builds");
        seen[usize::from(play.timer(timer::BUILTIN).is_some())] = true;
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0, "slots: allocations");
    assert_eq!(seen, [true, true], "the timer off and on");
    // The positive control: an allocation on this thread is counted.
    TRACK.with(|t| t.set(true));
    std::hint::black_box(Vec::<u8>::with_capacity(16));
    TRACK.with(|t| t.set(false));
    assert!(ALLOCS.load(Ordering::Relaxed) > 0, "the counter counts");
    ALLOCS.store(0, Ordering::Relaxed);
}

/// A later attempt of a play state carrying a latch's slot and the point
/// (design.md "beatoraja Lua Skins", the program's state), its program
/// carry-safe (its guard reads only the point): built frame after frame
/// on complete views before and after its first judgment, through a
/// rearm, its callbacks with slots run, its timer keeps the carried start,
/// its last build is a terminal witness, and nothing allocates; with a
/// positive control.
fn carried_slots_do_not_allocate(chart: &Chart, ctx: &PlayContext<'_>) {
    use present_support::{Builder, Cb, read};
    use skin_model::present::{Bank, Cmp, Conv, Domain, Insn, OFF, SlotDef, Val};
    let domain = Domain {
        off: true,
        ints: Some((0, 1 << 53)),
    };
    let mut b = Builder::default();
    b.0.slots.push(SlotDef { init: OFF, domain });
    b.0.domains.push(domain);
    let k = b.push(Cb::code(
        vec![
            Insn::LoadSlot { dst: 0, slot: 0 },
            Insn::Const { dst: 1, k: 0 },
            Insn::Cmp {
                op: Cmp::Eq,
                dst: 2,
                a: 0,
                b: 1,
            },
            Insn::Test {
                cond: 2,
                when: false,
                to: 7,
            },
            Insn::Read { dst: 2, read: 0 },
            Insn::Test {
                cond: 2,
                when: false,
                to: 7,
            },
            Insn::StateUs { dst: 0 },
            Insn::StoreSlot {
                slot: 0,
                src: 0,
                domain: 0,
            },
            Insn::Ret { src: 0 },
        ],
        vec![OFF],
        vec![read("point >= 200000", skin_model::Ty::Bool, None)],
        Conv::Timer,
    ));
    let elements = vec![widget(
        Source::Image(0),
        track(0, Ease::Linear, End::Hold),
        TimerRef::Output(k),
        Place::SCREEN,
    )];
    let skin = present_support::load(
        Some(b.0),
        Vec::new(),
        elements,
        skin_model::WidgetDefs::default(),
    )
    .unwrap();
    let uvs = play_support::uvs(&skin);
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let gauges = [state(GaugeValue::FULL), state(GaugeValue::FULL)];
    let notes = NoteFacts::default();
    let layout = SkinLayout::AUTHOR;
    let zero: &'static Tally = Box::leak(Box::new(Tally::default()));
    let judged: &'static Tally = Box::leak(Box::new(tally(5)));
    let frame = |now: i64, t: &'static Tally| SkinFrame {
        app: skin_model::AppReadouts::LAUNCH,
        state_us: now + 500_000,
        now: TimeUs(now),
        display_offset: TimeUs(0),
        tally: t,
        gauges: &gauges,
        gauge: 0,
        effects: &[],
        notes: &notes,
        offsets: skin_model::Offsets::NONE,
        input: &eval::InputFacts::NONE,
        complete: true,
        target: TARGETS[0],
        layout: &layout,
        turntable: 0,
        lanes: play_support::VIEW,
        readouts: readouts_for(play_support::VIEW),
        playing: true,
        closing: None,
    };
    // The latch set at 100 ms of the state clock in an earlier round.
    let carried = Bank::of(vec![Val::Int(100_000)]);
    let mut play = skin
        .prepare_carried(
            chart,
            ctx,
            skin_model::Continuation::Carried {
                score: Some(CARRIED_SCORE),
                bank: Some(&carried),
            },
        )
        .unwrap();
    assert!(play.carry().slots_run, "a carry-safe program runs");
    // Each frame's timer start and score fields, kept without allocating.
    let mut starts = [None; 64];
    let mut scores = [[0; 3]; 64];
    TRACK.with(|t| t.set(true));
    for (i, now) in (0..64 * 16 * MS).step_by(16 * MS as usize).enumerate() {
        if i == 32 {
            play.rearm();
        }
        batch.clear();
        let t = if i % 32 < 16 { zero } else { judged };
        skin.build(&mut play, &frame(now, t), &uvs, &mut batch)
            .expect("the frame builds");
        if let Some(s) = starts.get_mut(i) {
            *s = play.timer(timer::BUILTIN);
        }
        let fields = play.state().fields;
        if let Some(s) = scores.get_mut(i) {
            *s = [
                field::EX_SCORE,
                field::RATE_CURRENT_BP,
                field::RATE_WHOLE_BP,
            ]
            .map(|f| fields.get(usize::from(f)).copied().unwrap_or(-1));
        }
    }
    let witness = play.carry().terminal;
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "carried slots: allocations"
    );
    assert!(witness, "the last build is a terminal witness");
    assert_eq!(play.bank().values(), [Val::Int(100_000)]);
    // 100 ms of the state clock is 400 ms before chart time 0.
    assert!(
        starts.iter().all(|s| *s == Some(TimeUs(-400_000))),
        "{starts:?}"
    );
    // The carried score data before each first judgment, the rearm's
    // included; the round's own from it.
    let carried = [
        CARRIED_SCORE.ex_score,
        CARRIED_SCORE.rate_current_bp,
        CARRIED_SCORE.rate_whole_bp,
    ];
    for (i, s) in scores.iter().enumerate() {
        if i % 32 < 16 {
            assert_eq!(*s, carried, "frame {i}");
        } else {
            assert_ne!(s[0], carried[0], "frame {i}");
        }
    }
    // The positive control: an allocation on this thread is counted.
    TRACK.with(|t| t.set(true));
    std::hint::black_box(Vec::<u8>::with_capacity(16));
    TRACK.with(|t| t.set(false));
    assert!(ALLOCS.load(Ordering::Relaxed) > 0, "the counter counts");
    ALLOCS.store(0, Ordering::Relaxed);
}
