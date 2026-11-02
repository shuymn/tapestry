//! A frame's app work allocates nothing, from the first frame on (design.md
//! "Scope of No Allocation"): [`Staging::build`], the skin's build into the
//! preallocated batch, with effects, and the staging of the batch into
//! instances, the measurement overlay at its fullest after the skin's
//! sprites. `Renderer::build` runs the same function before
//! `Renderer::submit` calls into wgpu, whose own allocations are out of
//! scope, so the test drives it without a GPU. A skin's refused frame
//! allocates nothing, and counting ends on its way out before its
//! diagnostic is made. A resize applied between frames projects the
//! canvas and the clips again without allocating, and so do the first
//! frame after it and the later ones. A batch that fills up counts what it
//! refuses without allocating, the overlay still drawn whole, and the
//! result screen's page, drawn every frame while it shows, allocates
//! nothing either. A declarative skin prepared for an attempt, its clips
//! and its prepared text included, does the same with the snapshot's
//! gauges changing: its first and later frames, a refused one, the frames
//! around a resize, and a full batch with the overlay kept whole. A
//! committed installation's loading presentation, with its lanes, its
//! gauge drawn from a descriptor, progress and the chosen chart's text
//! installed, builds its first and later loading frames, a refused one,
//! and the next lifetime's first
//! frame, its state started afresh in place, without allocating; the
//! load's lines are drawn in a text pass outside this count.
//!
//! The counting allocator counts only on the thread that opted in, so the
//! harness's own threads do not disturb it. This file holds a single test
//! for the same reason. Each counted span is a [`Span`], which starts with
//! the positive control and stops counting on every way out of it, before
//! anything is asserted or reported. Profiles and the positive control:
//! design.md "Scope of No Allocation".

mod common;
mod decl_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{GAUGES, MS, attempt, context, input, lane, offscreen, view};
use decl_support::{Look, bundle};
use eval::{JudgeEvent, JudgeEventKind};
use render::{
    Blend, ClipTable, DeclSkin, FrameInput, Gpu, LoadingFrame, MAX_SPRITES, PlaySkin, Scissor,
    SimpleSkin, SkinError, Sprite, SpriteBatch, Staging,
};
use rules::Judgment;
use skin_model::{BuildError, ClipId};

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

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
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

/// A counted span of this thread's allocations. Made, it counts from a
/// positive control: an allocation kept through `black_box`, which must
/// be counted. Ended or dropped on any other way out (a panic unwinding
/// through it included), it stops counting, so what follows it, the
/// assertions and their messages, is never counted.
struct Span {
    control: u64,
}

impl Span {
    fn start() -> Self {
        ALLOCS.store(0, Ordering::Relaxed);
        TRACK.with(|t| t.set(true));
        drop(std::hint::black_box(vec![1u8; 8]));
        Self {
            control: ALLOCS.swap(0, Ordering::Relaxed),
        }
    }

    /// Stops counting: the positive control's count, then the span's.
    fn end(self) -> (u64, u64) {
        let control = self.control;
        drop(self);
        (control, ALLOCS.swap(0, Ordering::Relaxed))
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let _ = TRACK.try_with(|t| t.set(false));
    }
}

/// Runs `work` in a [`Span`]; its result and the allocations it made,
/// checked against the positive control once counting has stopped.
fn counted<R>(work: impl FnOnce() -> R) -> (R, u64) {
    let span = Span::start();
    let out = work();
    let (control, allocs) = span.end();
    assert_eq!(control, 1, "the positive control was not counted");
    (out, allocs)
}

const FRAMES: i64 = 460;
const FRAME_US: i64 = 16_667;

/// Taps 25 ms apart over every lane, then a long note per key lane, with
/// BPM changes and a STOP, so frames draw notes, long note bodies, text
/// and the effects of the judgments fed to them.
fn chart() -> (Chart, Vec<(u8, TimeUs)>) {
    let mut b = ChartBuilder::new(150_000)
        .bpm(TimeUs(2_000 * MS), 180_000)
        .stop(TimeUs(3_000 * MS), TimeUs(250 * MS));
    let mut taps = Vec::new();
    for i in 0..240 {
        let l = u8::try_from(i % 8).unwrap();
        let t = TimeUs(500 * MS + i64::from(i) * 25 * MS);
        b = b.tap(lane(l), t);
        taps.push((l, t));
    }
    for l in 1..8 {
        let start = 6_500 + i64::from(l) * 50;
        b = b.long(lane(l), TimeUs(start * MS), TimeUs((start + 400) * MS));
    }
    (b.build().unwrap(), taps)
}

/// What the play has judged so far, fed to the frames as their effects.
struct Feed<'a> {
    chart: &'a Chart,
    taps: &'a [(u8, TimeUs)],
    next: usize,
    seq: u64,
    held: u32,
    ended: u32,
}

impl Feed<'_> {
    /// Fills `effects` with frame `now`'s: every tap that has reached the
    /// line judged, and each long note held from its head and finished at
    /// its end.
    fn fill(&mut self, now: TimeUs, effects: &mut Vec<JudgeEvent>) {
        effects.clear();
        while self.next < self.taps.len() && self.taps[self.next].1 <= now && effects.len() < 32 {
            let (l, t) = self.taps[self.next];
            let note = self.chart.lanes[usize::from(l)]
                .iter()
                .position(|n| n.time == t)
                .unwrap();
            effects.push(JudgeEvent {
                seq: self.seq,
                time: t,
                origin: Some(t),
                kind: JudgeEventKind::Judged {
                    lane: lane(l),
                    note,
                    judgment: Judgment::ALL[self.next % 5],
                    deviation: Some(TimeUs(0)),
                    release: None,
                    combo: u32::try_from(self.next).unwrap(),
                },
            });
            self.next += 1;
            self.seq += 1;
        }
        for l in 1..8u8 {
            let notes = &self.chart.lanes[usize::from(l)];
            let (index, ln) = (notes.len() - 1, notes[notes.len() - 1]);
            let end = ln.end();
            // Odd lanes are released 100 ms early, even lanes at the end.
            let release = if l % 2 == 1 {
                TimeUs(end.0 - 100 * MS)
            } else {
                end
            };
            let kinds: &[JudgeEventKind] = if ln.time.0 > now.0 - FRAME_US && ln.time <= now {
                self.held += 1;
                &[JudgeEventKind::LnHeld {
                    lane: lane(l),
                    note: index,
                    judgment: Judgment::Great,
                    deviation: TimeUs(0),
                }]
            } else if release.0 > now.0 - FRAME_US && release <= now {
                self.ended += 1;
                &[
                    JudgeEventKind::LnHoldEnded {
                        lane: lane(l),
                        note: index,
                    },
                    JudgeEventKind::Judged {
                        lane: lane(l),
                        note: index,
                        judgment: Judgment::Great,
                        deviation: Some(TimeUs(0)),
                        release: Some(TimeUs(release.0 - end.0)),
                        combo: 1,
                    },
                ]
            } else {
                &[]
            };
            // A release at the end does not end the visual hold early.
            let skip = usize::from(release == end && kinds.len() == 2);
            for &kind in &kinds[skip..] {
                effects.push(JudgeEvent {
                    seq: self.seq,
                    time: now,
                    origin: None,
                    kind,
                });
                self.seq += 1;
            }
        }
    }
}

/// The measurement overlay at its fullest: 13 lines of 56 glyphs, none a
/// space, and the glyphs they draw.
fn overlay_lines() -> (Vec<String>, usize) {
    let lines: Vec<String> = (0..13).map(|i| format!("{i:0>56}")).collect();
    (lines, 13 * 56)
}

/// The overlay's glyphs at the end of the staged batch: 15×21 sprites
/// right of x 1000.
fn overlay_drawn(staging: &Staging) -> usize {
    staging
        .batch()
        .sprites()
        .iter()
        .rev()
        .take_while(|s| s.rect[0] >= 1000.0 && s.rect[2] == 15.0 && s.rect[3] == 21.0)
        .count()
}

/// A skin that pushes a sprite, then refuses the frame.
struct Refusing;

impl PlaySkin for Refusing {
    fn prepare(&mut self, _: &Gpu, _: &render::AttemptInput<'_>) -> Result<(), SkinError> {
        Ok(())
    }

    fn build(&mut self, _: &FrameInput<'_>, out: &mut SpriteBatch) -> Result<(), BuildError> {
        out.push(Blend::Alpha, Sprite::default());
        Err(BuildError::InvalidReadouts)
    }
}

#[test]
fn frames_do_not_allocate_from_the_first() {
    let (chart, taps) = chart();
    let mut skin = SimpleSkin::new();
    skin.load(&chart, None);
    let capacity = u32::try_from(MAX_SPRITES).unwrap();
    let mut staging = Staging::new(capacity, &skin.canvas([1920, 1080]));
    // A clip over the lanes' rows: its scissor is projected with the
    // canvas.
    staging.set_clips(ClipTable::new(vec![[0.0, 80.0, 1920.0, 800.0]]).unwrap());
    let v = view(42, 1_234);
    let mut effects: Vec<JudgeEvent> = Vec::with_capacity(64);
    let mut feed = Feed {
        chart: &chart,
        taps: &taps,
        next: 0,
        seq: 0,
        held: 0,
        ended: 0,
    };
    let (lines, glyphs) = overlay_lines();
    let overlay: Vec<&str> = lines.iter().map(String::as_str).collect();
    // What `Renderer::build` does, before `submit` calls into wgpu, for
    // frame `k`: the instances it staged, the overlay's included.
    let mut frame = |staging: &mut Staging, skin: &mut SimpleSkin, k: i64| {
        let now = TimeUs(k * FRAME_US);
        feed.fill(now, &mut effects);
        let f = FrameInput {
            overlay: &overlay,
            ..input(&v, now, TimeUs(8 * MS), &effects)
        };
        staging.build(skin, &f).map(|()| staging.instances().len())
    };

    // The first frame, with the batch and the staging fresh from their
    // reservation.
    let (first, first_allocs) = counted(|| frame(&mut staging, &mut skin, 0));
    assert!(first.unwrap() > glyphs);
    assert_eq!(first_allocs, 0, "the first frame allocated");
    // Every later frame.
    let (drawn, later_allocs) = counted(|| {
        (1..FRAMES)
            .map(|k| frame(&mut staging, &mut skin, k))
            .sum::<Result<usize, BuildError>>()
    });
    assert!(drawn.unwrap() > 0);
    assert_eq!(later_allocs, 0, "a later frame allocated");
    assert_eq!(overlay_drawn(&staging), glyphs, "the overlay was cut");

    // A skin refusing a frame after pushing a sprite: the refusal leaves
    // nothing staged and allocates nothing. Counting ended on the error's
    // way out, before anything formats it: the diagnostic made after it
    // is not counted.
    let mut refusing = Refusing;
    let f = FrameInput {
        overlay: &overlay,
        ..input(&v, TimeUs(0), TimeUs(8 * MS), &[])
    };
    let (refused, error_allocs) = counted(|| staging.build(&mut refusing, &f));
    let reason = refused.unwrap_err().to_string();
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "counting went on past the refusal"
    );
    assert!(!reason.is_empty());
    assert_eq!(error_allocs, 0, "a refused frame allocated");
    assert!(staging.instances().is_empty());

    // A resize to 21:9 applied between frames, as `Renderer::resize`
    // applies it: the canvas and the clip projected again, then the first
    // frame after it, then later ones.
    let (resized, resize_allocs) = counted(|| {
        staging.project(&skin.canvas([2560, 1080]));
        frame(&mut staging, &mut skin, FRAMES)
    });
    assert!(resized.unwrap() > glyphs);
    assert_eq!(
        resize_allocs, 0,
        "the resize or the frame after it allocated"
    );
    // The layout letterboxed at scale 1 with 320 columns of bars: the clip
    // moves with it.
    assert_eq!(staging.projection().target(), [2560, 1080]);
    assert_eq!(
        staging.scissor(Some(ClipId(0))),
        Scissor::Rect([320, 80, 1920, 800])
    );
    let (drawn, after_allocs) = counted(|| {
        (FRAMES + 1..FRAMES + 60)
            .map(|k| frame(&mut staging, &mut skin, k))
            .sum::<Result<usize, BuildError>>()
    });
    assert!(drawn.unwrap() > 0);
    assert_eq!(after_allocs, 0, "a later frame after the resize allocated");
    assert_eq!(feed.next, taps.len(), "every tap judged");
    assert_eq!(
        (feed.held, feed.ended),
        (7, 7),
        "every long note held and finished"
    );

    // A batch and staging too small for a frame of the chart loaded again,
    // its notes not yet judged: the batch counts what it refuses, the
    // overlay follows whole from its reserve, and nothing allocates.
    skin.load(&chart, None);
    let mut small = Staging::new(32, &skin.canvas([1920, 1080]));
    let f = FrameInput {
        overlay: &overlay,
        ..input(&v, TimeUs(3_000 * MS), TimeUs(8 * MS), &[])
    };
    let (dropped, full_allocs) =
        counted(|| small.build(&mut skin, &f).map(|()| small.batch().dropped()));
    assert!(dropped.unwrap() > 0, "the full batch counted no refusal");
    assert_eq!(small.instances().len(), 32 + glyphs);
    assert_eq!(overlay_drawn(&small), glyphs, "the overlay was cut");
    assert_eq!(full_allocs, 0, "a full batch allocated");

    // The result screen's page, its graphs at their fullest, allocates
    // nothing either: built from lines already made and graphs held.
    let mut graphs = game::ResultGraphs::EMPTY;
    graphs.traces.fill(Some(game::GaugeTrace {
        gauge: 0,
        columns: [game::GaugeColumn {
            low: rules::GaugeValue(0),
            high: rules::GaugeValue::FULL,
            last: rules::GaugeValue::FULL,
        }; game::GRAPH_COLUMNS],
        mark: rules::GaugeKind::Survival {
            fails_below: rules::GaugeValue(1),
        },
        failed: Some(100),
    }));
    graphs.emphasized = Some(2);
    graphs.no_input_from = Some(200);
    graphs.timing.bins = [3; game::TIMING_BINS];
    let lines = vec!["RESULT".repeat(12); 27];
    let (staged, result_allocs) = counted(|| {
        for _ in 0..3 {
            staging.fill(|b| render::result_screen(&lines, Some(&graphs), b));
        }
        staging.instances().len()
    });
    assert!(staged > 27 * 60);
    assert_eq!(result_allocs, 0, "the result page allocated");

    declarative_frames(&chart, &taps, &overlay, glyphs);
    loading_frames();
}

/// A declarative installation committed on a renderer, its loading
/// presentation built through the installed skin over a staging sized for
/// it: the first loading frame (its lifetime entered before the count),
/// the later ones advancing on the loading clock, and a refused one
/// (covers its lanes disagree with) and the same lifetime's next, and the
/// next lifetime entered and its first frame built, all allocating
/// nothing.
fn loading_frames() {
    let mut r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let look = Look {
        copies: 63,
        loading: true,
        structure: true,
        chart_text: true,
        progress: true,
        ..Look::left([255, 0, 0, 255])
    };
    let (b, _) = bundle(&r, dir.path(), &look);
    let token = r.install(&b, 1).unwrap();
    r.commit(token).unwrap();
    // The chosen chart's text prepared and installed for lifetime 1
    // between frames, outside the count.
    r.enter_loading(1);
    let chart = skin_model::ChartText {
        title: "T",
        subtitle: "S",
        artist: "A",
        subartist: "",
        genre: "G",
    };
    r.prepare_loading_text(1, 1, skin_model::TextSlots::ALL, &chart)
        .unwrap();
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    let mut skin = DeclSkin::of(&r.installation().unwrap());
    let mut staging = Staging::with_runs(
        skin.capacity(),
        skin.run_capacity(),
        &skin.canvas([1920, 1080]),
    );
    staging.set_clips(b.clips().clone());
    // Each frame with the shown gauge's descriptor, progress at 40 % and
    // the text's revision.
    let at = |k: i64| LoadingFrame {
        load: skin_model::LoadState {
            since: TimeUs(0),
            percent: 40,
        },
        gauge: Some(skin_model::LoadingGauge::of(0, &GAUGES[0])),
        text: 1,
        ..LoadingFrame::interim(1, TimeUs(k * FRAME_US), TimeUs(0), common::VISIBLE)
    };
    skin.enter_loading(1);
    let (first, first_allocs) = counted(|| {
        staging
            .build_loading(&mut skin, &at(0))
            .map(|()| staging.instances().len())
    });
    assert!(first.unwrap() > 0);
    assert_eq!(first_allocs, 0, "the first loading frame allocated");
    let (later, later_allocs) = counted(|| {
        (1..FRAMES)
            .map(|k| staging.build_loading(&mut skin, &at(k)).map(|()| 1))
            .sum::<Result<usize, BuildError>>()
    });
    assert!(later.unwrap() > 0);
    assert_eq!(later_allocs, 0, "a later loading frame allocated");
    let mut bad = at(FRAMES);
    bad.covers.cover_enabled = true;
    bad.covers.cover = 100;
    let (refused, error_allocs) = counted(|| {
        let first = staging.build_loading(&mut skin, &bad);
        (first, staging.build_loading(&mut skin, &at(FRAMES + 1)))
    });
    assert_eq!(
        refused,
        (
            Err(BuildError::InvalidReadouts),
            Err(BuildError::InvalidReadouts)
        )
    );
    assert_eq!(error_allocs, 0, "a refused loading frame allocated");
    assert!(staging.instances().is_empty());
    // The next lifetime: its state started afresh in place, then its first
    // frame built, as the renderer's loading frame does both.
    let next = LoadingFrame::interim(
        2,
        TimeUs((FRAMES + 2) * FRAME_US),
        TimeUs(0),
        common::VISIBLE,
    );
    let (second, second_allocs) = counted(|| {
        skin.enter_loading(2);
        staging
            .build_loading(&mut skin, &next)
            .map(|()| staging.instances().len())
    });
    assert!(second.unwrap() > 0);
    assert_eq!(
        second_allocs, 0,
        "a second lifetime's first frame allocated"
    );
}

/// A declarative skin prepared for an attempt of `chart`, as the
/// renderer stages it: 64 image elements under a clip and a text element
/// with its prepared line, over a staging sized for the skin. Its frames,
/// with `taps` judged as effects and the shown gauge's value changing
/// every frame, allocate nothing from the first; nor does a refused one, a
/// resize and the frames after it, or a batch too small with the overlay
/// following whole.
fn declarative_frames(chart: &Chart, taps: &[(u8, TimeUs)], overlay: &[&str], glyphs: usize) {
    let r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let look = Look {
        copies: 63,
        text: true,
        ..Look::left([255, 0, 0, 255])
    };
    let (b, text) = bundle(&r, dir.path(), &look);
    let ctx = context(&GAUGES, false);
    let mut skin = DeclSkin::new(b.clone());
    skin.prepare(r.gpu(), &attempt(&r, chart, &ctx)).unwrap();
    assert_eq!(text.unwrap().calls.load(Ordering::SeqCst), 1);
    let (capacity, runs) = (skin.capacity(), skin.run_capacity());
    assert_eq!(capacity, 65);
    let mut staging = Staging::with_runs(capacity, runs, &skin.canvas([1920, 1080]));
    staging.set_clips(b.clips().clone());
    // Views whose shown gauge differs frame to frame.
    let views: Vec<_> = (0..4)
        .map(|k| {
            let mut v = view(k, 100 * k);
            v.gauges[0].value = rules::GaugeValue(rules::GaugeValue::PERCENT * i64::from(20 * k));
            v
        })
        .collect();
    let mut effects: Vec<JudgeEvent> = Vec::with_capacity(64);
    let mut feed = Feed {
        chart,
        taps,
        next: 0,
        seq: 0,
        held: 0,
        ended: 0,
    };
    let mut frame = |staging: &mut Staging, skin: &mut DeclSkin, k: i64| {
        let now = TimeUs(k * FRAME_US);
        feed.fill(now, &mut effects);
        let v = &views[usize::try_from(k).unwrap() % views.len()];
        let f = FrameInput {
            overlay,
            ..input(v, now, TimeUs(8 * MS), &effects)
        };
        staging.build(skin, &f).map(|()| staging.instances().len())
    };

    let (first, first_allocs) = counted(|| frame(&mut staging, &mut skin, 0));
    assert_eq!(first.unwrap(), 65 + glyphs, "every quad and the overlay");
    assert_eq!(
        first_allocs, 0,
        "a declarative skin's first frame allocated"
    );
    let (drawn, later_allocs) = counted(|| {
        (1..FRAMES)
            .map(|k| frame(&mut staging, &mut skin, k))
            .sum::<Result<usize, BuildError>>()
    });
    assert!(drawn.unwrap() > 0);
    assert_eq!(
        later_allocs, 0,
        "a declarative skin's later frame allocated"
    );
    assert_eq!(overlay_drawn(&staging), glyphs, "the overlay was cut");

    let (resized, resize_allocs) = counted(|| {
        staging.project(&skin.canvas([2560, 1080]));
        (FRAMES..FRAMES + 60)
            .map(|k| frame(&mut staging, &mut skin, k))
            .sum::<Result<usize, BuildError>>()
    });
    assert!(resized.unwrap() > 0);
    assert_eq!(resize_allocs, 0, "a declarative skin's resize allocated");
    // The fixed canvas letterboxed with 320 columns of bars: the clip of
    // the left half moves with it.
    assert_eq!(
        staging.scissor(Some(ClipId(0))),
        Scissor::Rect([320, 0, 960, 1080])
    );

    // An attempt counting empty POORs as POORs, with a snapshot counting
    // more of them than POORs: refused, allocating nothing.
    let refusing_ctx = context(&GAUGES, true);
    let mut refusing = DeclSkin::new(b.clone());
    refusing
        .prepare(r.gpu(), &attempt(&r, chart, &refusing_ctx))
        .unwrap();
    let v = view(0, 0);
    let f = FrameInput {
        overlay,
        ..input(&v, TimeUs(0), TimeUs(8 * MS), &[])
    };
    let (refused, error_allocs) = counted(|| staging.build(&mut refusing, &f));
    assert_eq!(refused, Err(BuildError::InconsistentTally));
    assert_eq!(
        error_allocs, 0,
        "a declarative skin's refused frame allocated"
    );
    assert!(staging.instances().is_empty());

    // A batch of 8 sprites for 65 quads: the batch counts what it refuses
    // and the overlay follows whole from its reserve.
    let mut small = Staging::new(8, &skin.canvas([1920, 1080]));
    small.set_clips(b.clips().clone());
    let (dropped, full_allocs) =
        counted(|| small.build(&mut skin, &f).map(|()| small.batch().dropped()));
    assert!(dropped.unwrap() > 0, "the full batch counted no refusal");
    assert_eq!(small.instances().len(), 8 + glyphs);
    assert_eq!(overlay_drawn(&small), glyphs, "the overlay was cut");
    assert_eq!(full_allocs, 0, "a declarative skin's full batch allocated");
}
