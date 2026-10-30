//! A skin's per-frame build allocates nothing: effects through the
//! reducer, the snapshot into the fields, every timer, the `when` timers,
//! each image's frame and its place in the atlas, and the elements into
//! the batch.
//!
//! A counting global allocator counts allocations made on threads that opt
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.
//! Profiles and the positive control: design.md "Scope of No Allocation".

mod play_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{Chart, ChartBuilder, Lane, NoteKind, SoundId, TimeUs};
use eval::{GaugeState, JudgeEvent, JudgeEventKind, Section, Tally};
use image::{ImageFormat, Rgba, RgbaImage};
use play_support::{bool_program, state, tally};
use rules::{GaugeValue, Judgment};
use skin_model::{
    AtlasLimits, Attempt, Blend, ClipId, CycleDef, Ease, ElementDef, End, FileDef, Filter,
    GaugeDesc, ImageDef, KeyDef, LaneTimer, Play, PlayContext, QuadBatch, Sheet, Skin, SkinFrame,
    Source, TimerRef, Track, UvTable, Variant, lane_timer, timer,
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

/// A 3-key track at x near `i`.
fn track(i: u16, ease: Ease, end: End) -> Track {
    let key = |at: i64, x: i32, alpha: i32| {
        let mut props = [None; 9];
        props[0] = Some(x);
        props[1] = Some(x / 2);
        props[2] = Some(64);
        props[3] = Some(64);
        props[4] = Some(alpha);
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
/// The reserved layers of the atlas.
const RESERVED: u16 = 1;

/// A uv rectangle as a map key.
fn uv_key(uv: [f32; 4]) -> [u32; 4] {
    uv.map(f32::to_bits)
}

/// A skin, its atlas's uv table and, per uv, the image and frame it shows.
struct Loaded {
    skin: Skin,
    uvs: UvTable,
    shows: HashMap<[u32; 4], (u16, u16)>,
}

/// One image per built-in timer, then [`WHEN_IMAGES`] more: each a file of
/// its own, every third a cycle of 4 to 8 frames on its own built-in timer
/// or on a `when` timer, the filters alternating. Built in a folder that
/// lives until the atlas is built.
fn images() -> (tempfile::TempDir, Sheet) {
    let d = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    let mut images = Vec::new();
    for k in 0..timer::BUILTIN + WHEN_IMAGES {
        let cols = if k % 3 == 0 { 4 + k % 5 } else { 1 };
        let (w, h) = (u32::from(12 + k % 5), u32::from(10 + k % 3));
        let img = RgbaImage::from_pixel(w * u32::from(cols), h, Rgba([k as u8, 0, 0, 255]));
        let name = format!("{k}.png");
        img.save_with_format(d.path().join(&name), ImageFormat::Png)
            .unwrap();
        files.push(FileDef {
            path: name,
            hi: None,
        });
        let timer = if k < timer::BUILTIN && k % 2 == 0 {
            TimerRef::Builtin(k)
        } else {
            TimerRef::When(k % 32)
        };
        images.push(ImageDef {
            file: u32::from(k),
            rect: None,
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
        });
    }
    let sheet = Sheet::new(d.path(), files, images).unwrap();
    (d, sheet)
}

/// 2 000 elements, 30 on every built-in timer and 350 over 32 `when`s,
/// with every ease and end, both blends, clips and visibility programs,
/// and a background on `ready`; each shows its timer's image.
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
    });
    let (_dir, sheet) = images();
    let skin = Skin::new(whens, sheet, elements).unwrap();
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
    let mut next = 0;
    for k in 0..timer::BUILTIN + WHEN_IMAGES {
        let frames = if k % 3 == 0 { 4 + k % 5 } else { 1 };
        for f in 0..frames {
            let uv = uvs.get(next).unwrap().uv;
            assert!(shows.insert(uv_key(uv), (k, f)).is_none());
            next += 1;
        }
    }
    assert_eq!(usize::try_from(next).unwrap(), uvs.frames().len());
    Loaded { skin, uvs, shows }
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
}

impl Witness {
    fn new() -> Self {
        Self {
            split_drains: 0,
            small_drops: 0,
            other_drops: 0,
            on: [false; timer::BUILTIN as usize],
            drawn: [false; timer::BUILTIN as usize],
            when_started: false,
            empty_frames_after_ready: 0,
            later_frame: false,
            later_layer: false,
        }
    }
}

/// One attempt's frames, every 16 ms from `from` to `to`, each draining at
/// most [`DRAIN`] effects; a 1 s stall halfway backs the queue up, fails
/// gauge 1, shows it and changes the display offset.
#[allow(clippy::too_many_arguments)]
fn stretch(
    loaded: &Loaded,
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
    let mut frame_no = 0_u32;
    let mut offset = TimeUs(0);
    let mut shown = 0;
    let midway = from + (to - from) / 2;
    while now <= to {
        if (midway..midway + 16 * MS).contains(&now) {
            now += 1_000 * MS;
            gauges[1].failed_at = Some(TimeUs(now));
            offset = TimeUs(40 * MS);
            shown = 1;
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
        let frame = SkinFrame {
            now: TimeUs(now),
            display_offset: offset,
            tally,
            gauges,
            gauge: shown,
            effects: drain,
        };
        let use_small = frame_no % 100 == 99;
        let out = if use_small { &mut *small } else { &mut *batch };
        out.clear();
        skin.build(play, &frame, &loaded.uvs, out);
        if use_small {
            w.small_drops += out.dropped();
        } else {
            w.other_drops += out.dropped();
            if out.quads().is_empty() {
                w.empty_frames_after_ready += 1;
            }
            for q in out.quads() {
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
    let descs = [
        GaugeDesc {
            start: GaugeValue(50 * GaugeValue::PERCENT),
            max: GaugeValue::FULL,
        },
        GaugeDesc {
            start: GaugeValue::FULL,
            max: GaugeValue::FULL,
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
    // (name, context, effects, first frame): the whole chart
    // with breaks, a practice round with breaks after a retry, and a round
    // played to a full combo.
    let attempts = [
        ("play", full, effects(&chart, None, false), -lead_in),
        (
            "round",
            PlayContext {
                attempt: Attempt::Section(&round),
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
    for (name, ctx, events, from) in &attempts {
        // Prepared untracked, as between plays.
        let mut play = skin.prepare(&chart, ctx).unwrap();
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
    }
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
}
