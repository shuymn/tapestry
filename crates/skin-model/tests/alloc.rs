//! A skin's per-frame build allocates nothing: effects through the
//! reducer, the snapshot into the fields, every timer, the `when` timers
//! and the elements into the batch.
//!
//! A counting global allocator counts allocations made on threads that opt
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.
//! Profiles and the positive control: design.md "Scope of No Allocation".

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{Chart, ChartBuilder, Lane, NoteKind, SoundId, TimeUs};
use eval::{GaugeState, JudgeEvent, JudgeEventKind, Section, Tally};
use rules::{GaugeValue, Judgment, PerJudgment};
use skin_model::{
    Attempt, Blend, ClipId, Ease, ElementDef, End, GaugeDesc, KeyDef, LaneTimer, PLAY, Play,
    PlayContext, Program, QuadBatch, Scope, Skin, SkinFrame, Source, TimerRef, Track, Ty, compile,
    lane_timer, timer,
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

/// Notes every 50 ms over the 8 lanes (400 ms apart in a lane), every 4th
/// a 200 ms long note, and a mine 25 ms after every 5th, four lanes over.
fn chart() -> Chart {
    let mut b = ChartBuilder::new(150_000).sound("a.wav");
    for i in 0..NOTES {
        let t = TimeUs(i * 50 * MS);
        b = if i % 4 == 0 {
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

/// Every kind of effect for every note, with both gauges reported after
/// each judgment, sorted by time, ending with `SongEnd`.
fn effects(chart: &Chart) -> Vec<JudgeEvent> {
    let grades = [
        Judgment::Perfect,
        Judgment::Great,
        Judgment::Good,
        Judgment::Bad,
        Judgment::Poor,
    ];
    let mut out = Vec::new();
    let mut combo = 0_u32;
    let mut gauge = 50_i64;
    for (l, notes) in chart.lanes.iter().enumerate() {
        let lane = Lane::new(u8::try_from(l).unwrap()).unwrap();
        for (n, note) in notes.iter().enumerate() {
            let t = note.time.0;
            match note.kind {
                NoteKind::Mine { .. } => {
                    out.push(ev(t, JudgeEventKind::MineHit { lane, note: n }));
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
                    let judgment = grades[(t / (50 * MS)) as usize % 5];
                    combo = if judgment >= Judgment::Bad {
                        0
                    } else {
                        combo + 1
                    };
                    out.push(ev(
                        t,
                        JudgeEventKind::Judged {
                            lane,
                            note: n,
                            judgment,
                            deviation: (judgment != Judgment::Poor).then_some(TimeUs(0)),
                            release: None,
                            combo,
                        },
                    ));
                    if judgment == Judgment::Poor {
                        out.push(ev(t + 10 * MS, JudgeEventKind::EmptyPoor { lane }));
                    }
                }
                NoteKind::Long { end, .. } => {
                    let bad_head = (t / (200 * MS)) % 3 == 0;
                    if bad_head {
                        combo = 0;
                        out.push(ev(
                            t - 20 * MS,
                            JudgeEventKind::Judged {
                                lane,
                                note: n,
                                judgment: Judgment::Bad,
                                deviation: Some(TimeUs(-20 * MS)),
                                release: None,
                                combo,
                            },
                        ));
                        out.push(ev(
                            t + 100 * MS,
                            JudgeEventKind::LnHoldEnded { lane, note: n },
                        ));
                    } else {
                        combo += 1;
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
                        out.push(ev(
                            end.0,
                            JudgeEventKind::Judged {
                                lane,
                                note: n,
                                judgment: Judgment::Perfect,
                                deviation: Some(TimeUs(0)),
                                release: Some(TimeUs(0)),
                                combo,
                            },
                        ));
                    }
                }
            }
            // The gauges climb to the maximum and fall back.
            gauge = if gauge >= 100 { 60 } else { gauge + 4 };
            for g in 0..2 {
                out.push(ev(
                    t,
                    JudgeEventKind::Gauge {
                        gauge: g,
                        value: GaugeValue(gauge.min(100) * GaugeValue::PERCENT),
                        reference: GaugeValue(gauge.min(100) * GaugeValue::PERCENT),
                    },
                ));
            }
        }
    }
    out.sort_by_key(|e| e.time);
    let last = out.last().map_or(0, |e| e.time.0);
    out.push(ev(last + 500 * MS, JudgeEventKind::SongEnd));
    out
}

fn program(src: &str) -> Program {
    compile(src, &Scope::new(&PLAY), Ty::Bool).unwrap()
}

/// About 2 000 elements over every built-in timer and 32 `when`s, every
/// ease and end, both blends, clips and visibility programs.
fn skin() -> Skin {
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
        .map(|i| program(conditions[i % conditions.len()]))
        .collect();
    let visible = [
        program("combo % 2 == 0"),
        program("bp < 1000"),
        program("on(play)"),
    ];
    let eases = [Ease::Linear, Ease::In, Ease::Out, Ease::Step];
    let mut elements = Vec::new();
    for i in 0_u16..2_000 {
        let timer = if i % 5 == 4 {
            TimerRef::When(i % 32)
        } else {
            TimerRef::Builtin(i % timer::BUILTIN)
        };
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
        let end = match i % 3 {
            0 => End::Hold,
            1 => End::Hide,
            _ => End::Loop {
                from: TimeUs(100 * MS),
            },
        };
        let track = Track::new(
            &[
                key(0, i32::from(i), 0),
                key(100 * MS, i32::from(i) + 10, 255),
                key(500 * MS, i32::from(i) + 40, 128),
            ],
            eases[usize::from(i % 4)],
            end,
        )
        .unwrap();
        elements.push(ElementDef {
            source: Source::Uv([0.0, 0.0, 0.5, 0.5]),
            track,
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
        source: Source::Uv([0.0, 0.0, 1.0, 1.0]),
        track: Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props: [None; 9],
            }],
            Ease::Linear,
            End::Hold,
        )
        .unwrap(),
        timer: TimerRef::Builtin(timer::READY),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
    });
    Skin::new(whens, elements).unwrap()
}

/// What the stretch saw.
#[derive(Default)]
struct Witness {
    split_drains: u32,
    small_drops: u32,
    other_drops: u32,
    feedback: bool,
    ending: bool,
    when_started: bool,
    empty_frames_after_ready: u32,
}

#[allow(clippy::too_many_arguments)]
fn stretch(
    skin: &Skin,
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
    let mut next = 0;
    let mut now = from;
    let mut frame_no = 0_u32;
    let mut offset = TimeUs(0);
    let mut shown = 0;
    let midway = from + (to - from) / 2;
    while now <= to {
        // A 1 s stall halfway: the queue backs up and drains over frames.
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
        skin.build(play, &frame, out);
        if use_small {
            w.small_drops += out.dropped();
        } else {
            w.other_drops += out.dropped();
            if out.quads().is_empty() {
                w.empty_frames_after_ready += 1;
            }
        }
        for l in 0..8 {
            let lane = lane(l);
            w.feedback |= play.timer(lane_timer(LaneTimer::Bomb, lane)).is_some()
                && play.timer(lane_timer(LaneTimer::Hold, lane)).is_some();
        }
        w.ending |=
            play.timer(timer::FULL_COMBO).is_some() || play.timer(timer::LAST_NOTE).is_some();
        w.when_started |= play.timer(timer::BUILTIN).is_some();
        frame_no += 1;
        now += 16 * MS;
    }
}

fn fresh_tally() -> Tally {
    Tally {
        counts: PerJudgment {
            perfect: 0,
            great: 0,
            good: 0,
            bad: 0,
            poor: 0,
        },
        ex_score: 0,
        bp: 0,
        combo: 0,
        max_combo: 0,
        empty_poor: 0,
    }
}

#[test]
fn build_does_not_allocate() {
    let chart = chart();
    let events = effects(&chart);
    let skin = skin();
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
    let state = |v: GaugeValue| GaugeState {
        value: v,
        reference: v,
        failed_at: None,
    };
    let mut tally = fresh_tally();
    let mut gauges = vec![state(descs[0].start), state(descs[1].start)];
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let mut small = QuadBatch::with_capacity(8, 4);
    let lead_in = 1_000 * MS;
    let ctx = PlayContext {
        gauges: &descs,
        attempt: Attempt::Full {
            lead_in: TimeUs(lead_in),
        },
        empty_poor_breaks_combo: true,
    };
    let mut play = skin.prepare(&chart, &ctx).unwrap();
    let song_end = events.last().unwrap().time.0;
    let mut w = Witness::default();

    TRACK.with(|t| t.set(true));
    // The count works: an allocation that escapes is counted in any profile.
    drop(std::hint::black_box(vec![1u8; 8]));
    assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 1);
    stretch(
        &skin,
        &mut play,
        &events,
        &mut tally,
        &mut gauges,
        &mut batch,
        &mut small,
        -lead_in,
        song_end + 1_000 * MS,
        &mut w,
    );
    TRACK.with(|t| t.set(false));
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0, "allocations in play");

    // A retry of a practice round: prepared untracked, then counted again.
    let section = Section {
        start: TimeUs(30_000 * MS),
        end: TimeUs(60_000 * MS),
        lead_in: TimeUs(2_000 * MS),
        initial_gauges: vec![GaugeValue(70 * GaugeValue::PERCENT), GaugeValue::FULL],
    };
    let ctx = PlayContext {
        attempt: Attempt::Section(&section),
        ..ctx
    };
    let mut play = skin.prepare(&chart, &ctx).unwrap();
    let mut tally = fresh_tally();
    let mut gauges = vec![state(section.initial_gauges[0]), state(GaugeValue::FULL)];
    let first = events.partition_point(|e| e.time < section.start);
    let last = events.partition_point(|e| e.time < section.end);
    let mut w2 = Witness::default();

    TRACK.with(|t| t.set(true));
    drop(std::hint::black_box(vec![1u8; 8]));
    assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 1);
    stretch(
        &skin,
        &mut play,
        &events[first..last],
        &mut tally,
        &mut gauges,
        &mut batch,
        &mut small,
        28_000 * MS,
        61_000 * MS,
        &mut w2,
    );
    TRACK.with(|t| t.set(false));
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "allocations in the round"
    );

    for (name, w) in [("play", &w), ("round", &w2)] {
        assert!(w.split_drains > 0, "{name}: no drain was split");
        assert!(w.small_drops > 0, "{name}: the small batch dropped nothing");
        assert_eq!(w.other_drops, 0, "{name}");
        assert!(w.feedback, "{name}: no bomb with a hold");
        assert!(w.ending, "{name}: no full combo or last note");
        assert!(w.when_started, "{name}: no `when` timer started");
        assert_eq!(w.empty_frames_after_ready, 0, "{name}");
    }
}
