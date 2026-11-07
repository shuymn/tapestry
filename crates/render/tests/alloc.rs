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
//! nothing either. The simple skin's READY rearms, of a whole play and of
//! a practice round, and the frames after each allocate nothing, its
//! prepared playhead kept; so do its frames made from presentation
//! snapshots (`render::presentation::frame` counted with them), the gain,
//! the cover paints and the native line moving, a snapshot of another
//! attempt, the fields with nothing left, and the frames after a rearm.
//! The full lane fixture on a declarative skin draws every note category,
//! its bar group and each cover paint, then frames with no field left
//! and the frames after a rearm, without allocating. A declarative skin
//! prepared for an attempt, its clips and its prepared text included,
//! does the same with the snapshot's gauges changing: its first and later
//! frames, a refused one, the frames around a resize, and a full batch
//! with the overlay kept whole. A committed installation's loading
//! presentation, with its lanes, its gauge drawn from a descriptor,
//! progress and the chosen chart's rasterized text installed, builds its
//! first and later loading frames, a refused one, and the next lifetime's
//! first frame, its state started afresh in place, without allocating;
//! the load's lines are drawn in a text pass outside this count. An imported
//! skin (JSON, a bitmap font and page images through the importer) does
//! the same with its bitmap text installed: its loading frames with
//! covers moving its trimmed paint every frame, each frame's runs
//! resolved to scissors, inline clips included, as the renderer draws
//! them, across a resize, a refusal and the next lifetime; and an
//! attempt's play frames with bitmap labels, batches short of sprites and
//! of runs keeping the overlay whole. An imported skin with outline text
//! builds its play frames and then its loading frames, each with lines
//! rasterized by its producer, without allocating. A skin refused because
//! with the other screens' skins it passes the texels they share is
//! refused without allocating by the player's allocation guards
//! (`a_refused_shared_admission_alloc`), which can charge a committed skin
//! an atlas's cap without uploading one.
//!
//! The counting allocator counts only on the thread that opted in, so the
//! harness's own threads do not disturb it. This file holds a single test
//! for the same reason. Each counted span is a [`Span`], which starts with
//! the positive control and stops counting on every way out of it, before
//! anything is asserted or reported. Profiles and the positive control:
//! design.md "Scope of No Allocation".

mod common;
mod decl_support;
mod font_support;
mod json_support;
mod outline_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{GAUGES, MS, attempt, context, input, lane, offscreen, view};
use decl_support::{FULL_LANES, Look, Part, bundle, full_readouts, lane_chart, part_counts};
use eval::{JudgeEvent, JudgeEventKind};
use game::adjustment::PresentationSnapshot;
use render::{
    Blend, ClipTable, DeclSkin, FrameInput, Gpu, LoadingFrame, MAX_SPRITES, PlaySkin, Scissor,
    SimpleSkin, SkinError, Sprite, SpriteBatch, Staging,
};
use rules::Judgment;
use skin_model::{BuildError, ClipId, LaneView, ScreenKind, TextImage, TextTable};

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
                    presses: 0,
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
                    presses: 0,
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
                        presses: 0,
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
    // The attempt prepared, its playhead included, before counting.
    let mut skin = SimpleSkin::new();
    skin.prepare_attempt(&chart, None).unwrap();
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

    // READY rearmed again and again, for the whole chart and for a
    // practice round, each prepared before counting: the rearm starts the
    // effects' state over in place, the playhead kept, and the first and
    // later frames after it, fed the judgments again, allocate nothing.
    let round = eval::Section {
        start: TimeUs(1_000 * MS),
        end: TimeUs(7_000 * MS),
        lead_in: TimeUs(0),
        initial_gauges: Vec::new(),
    };
    for section in [None, Some(&round)] {
        skin.prepare_attempt(&chart, section).unwrap();
        for k in 0..3 {
            let mut again = Feed {
                chart: &chart,
                taps: &taps,
                next: 0,
                seq: 0,
                held: 0,
                ended: 0,
            };
            let (drawn, rearm_allocs) = counted(|| {
                skin.rearm();
                (0..FRAMES)
                    .map(|j| {
                        let now = TimeUs(j * FRAME_US);
                        again.fill(now, &mut effects);
                        let f = FrameInput {
                            overlay: &overlay,
                            ..input(&v, now, TimeUs(8 * MS), &effects)
                        };
                        staging
                            .build(&mut skin, &f)
                            .map(|()| staging.instances().len())
                    })
                    .sum::<Result<usize, BuildError>>()
            });
            assert!(drawn.unwrap() > glyphs);
            assert_eq!(
                rearm_allocs, 0,
                "rearm {k} of {section:?} or a frame after it allocated"
            );
            assert_eq!(again.next, taps.len(), "every tap judged again");
        }
    }

    // A batch and staging too small for a frame of the chart prepared
    // again, its notes not yet judged: the batch counts what it refuses,
    // the overlay follows whole from its reserve, and nothing allocates.
    skin.prepare_attempt(&chart, None).unwrap();
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

    simple_presented_frames(&chart, &overlay);
    declarative_frames(&chart, &taps, &overlay, glyphs);
    declarative_lane_frames();
    loading_frames();
    imported_frames(&chart, &overlay, glyphs);
    outline_frames(&chart, &overlay);
    result_frames();
}

/// A result skin of two solid cells in `dir`, drawing `elements` (image,
/// left edge) as full-height bands on the screen timer.
fn result_skin(dir: &std::path::Path, elements: &[(u32, i32)], texts: usize) -> skin_model::Skin {
    use skin_model::{
        Align, Anchor, Ease, ElementDef, End, Filter, Fit, FontRef, KeyDef, Overflow, Place,
        Region, RegionDef, ResultDefs, Root, SheetBuilder, SkinDef, SolidImageDef, Source, TextDef,
        TextField, TextValue, TimerRef, Track, WidgetDefs, result_timer,
    };
    let mut b = SheetBuilder::new(&Root::new(dir).unwrap());
    for color in [[255, 0, 0, 255], [0, 255, 0, 255]] {
        b.add_solid_image(SolidImageDef {
            size: [16, 16],
            color,
            filter: Filter::Nearest,
            grid: None,
        })
        .unwrap();
    }
    let elements = elements
        .iter()
        .map(|&(image, x)| {
            let mut props = [None; 9];
            props[..4].copy_from_slice(&[Some(x), Some(0), Some(400), Some(1_080)]);
            ElementDef {
                source: Source::Image(image),
                track: Track::new(
                    &[KeyDef {
                        at: TimeUs(0),
                        props,
                    }],
                    Ease::Linear,
                    End::Hold,
                )
                .unwrap(),
                timer: TimerRef::Builtin(result_timer::SCREEN),
                visible: None,
                blend: Blend::Alpha,
                clip: None,
                place: Place::SCREEN,
            }
        })
        .collect();
    let def = SkinDef {
        fit: Fit::Fixed,
        regions: [RegionDef {
            anchor: Anchor::TopLeft,
            optional: false,
        }; Region::COUNT],
        whens: Vec::new(),
        sheet: b.finish(),
        lanes: None,
        elements,
        widgets: WidgetDefs::default(),
        bomb_up_to: None,
        close: None,
    };
    let mut def = def;
    for k in 0..texts {
        def.widgets.texts.push(TextDef {
            value: TextValue::Field(TextField::Title),
            font: FontRef::Default,
            align: Align::Left,
            overflow: Overflow::Spill,
        });
        let mut e = def.elements[0].clone();
        e.source = Source::Text(u32::try_from(k).unwrap());
        def.elements.push(e);
    }
    skin_model::Skin::new_result(def, ResultDefs::default()).unwrap()
}

/// A text producer of the stub's ([`decl_support::Text`]).
fn stub_text() -> Arc<dyn render::AttemptText> {
    Arc::new(decl_support::Text {
        calls: std::sync::atomic::AtomicU32::new(0),
        rasters: std::sync::atomic::AtomicU32::new(0),
        refuse: std::sync::atomic::AtomicBool::new(false),
        strings: std::sync::Mutex::new(Vec::new()),
    })
}

/// C11's renderer half (S10-09i): the result's commit point allocates
/// nothing: the commit of a layout staged on the pages installed, at its
/// text capacity, the installation of its staged lines, and its frames
/// with the strip at its fullest, the first after the commit included; and
/// the commit of a layout on another array.
fn result_frames() {
    use skin_model::{
        BestView, LampKey, Layout, ResultChart, ResultFrame, ResultLabels, ResultScore,
        ResultStatus, ResultTimers, ResultView, Side, Variant,
    };
    let mut r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let a = result_skin(dir.path(), &[(0, 0), (1, 1_500)], 1);
    let atlas = a.atlas(Variant::Lo, r.atlas_limits(), &|| false).unwrap();
    let texts = skin_model::MAX_TEXT_ELEMENTS;
    let b = result_skin(dir.path(), &[(1, 1_000)], texts);
    assert_eq!(b.text_slots(), texts);
    let rebound = atlas.rebind(b.sheet(), Variant::Lo, true).unwrap();
    let none = || ClipTable::new(Vec::new()).unwrap();
    let bundle = |s, atlas| {
        Arc::new(
            render::SkinBundle::new(Arc::new(s), atlas, none(), Side::Left)
                .unwrap()
                .with_text(stub_text())
                .unwrap(),
        )
    };
    let a = bundle(a, atlas);
    let b = bundle(b, rebound);
    let token = r.install(ScreenKind::Result, &a, 1).unwrap();
    r.commit(token).unwrap();
    let view = ResultView {
        status: ResultStatus::Ready,
        chart: ResultChart::default(),
        score: ResultScore {
            clear: LampKey::Normal,
            ..ResultScore::default()
        },
        best: BestView::Unavailable,
        timing: None,
        gauge: None,
        deviations: None,
        timing_areas: None,
        timelines: None,
    };
    // B prepared beside A: staged on its pages, its state and lines.
    let token = r.install(ScreenKind::Result, &b, 2).unwrap();
    let mut res = b.skin().prepare_result();
    let labels = ResultLabels {
        title: "T".to_owned(),
        ..ResultLabels::default()
    };
    b.skin().update_result(&mut res, &view, &labels).unwrap();
    r.prepare_result_text(&mut res).unwrap();
    let layout = Layout::AUTHOR;
    let strip = [Sprite {
        rect: [0.0, 0.0, 1_920.0, 42.0],
        uv: render::solid_uv(),
        color: [0.0, 0.0, 0.0, 0.75],
        ..Sprite::default()
    }; render::RESULT_STRIP_SPRITES];
    let ((), commit) = counted(|| r.commit(token).unwrap());
    let (installed, text) = counted(|| r.install_staged_result_text());
    assert!(installed);
    let ((), allocs) = counted(|| {
        for k in 0..60 {
            let f = ResultFrame {
                now: TimeUs(k * 16_667),
                target: [1_920, 1_080],
                layout: &layout,
                timers: ResultTimers {
                    screen: Some(TimeUs(0)),
                    fadeout: (k >= 30).then_some(TimeUs(30 * 16_667)),
                },
            };
            r.build_result(&mut res, &f, &strip).unwrap();
        }
    });
    assert_eq!(commit, 0, "the result's commit allocated");
    assert_eq!(text, 0, "the result's staged lines' installation allocated");
    assert_eq!(allocs, 0, "the result's frames allocated");
    // A layout on another array: the commit keeps what it replaces.
    let other = tempfile::tempdir().unwrap();
    let c = result_skin(other.path(), &[(0, 0)], 0);
    let c_atlas = c.atlas(Variant::Lo, r.atlas_limits(), &|| false).unwrap();
    let c = Arc::new(render::SkinBundle::new(Arc::new(c), c_atlas, none(), Side::Left).unwrap());
    let token = r.install(ScreenKind::Result, &c, 3).unwrap();
    let ((), commit) = counted(|| r.commit(token).unwrap());
    assert_eq!(commit, 0, "a commit on another array allocated");
}

/// Every run's scissor as the renderer resolves it while drawing
/// ([`Staging::run_scissor`]): the runs that draw somewhere, and those
/// with an inline clip rect.
fn resolve(staging: &Staging) -> (usize, usize) {
    staging
        .batch()
        .runs()
        .iter()
        .fold((0, 0), |(drawn, inline), run| {
            let drawn = drawn + usize::from(staging.run_scissor(run) != Scissor::Empty);
            (drawn, inline + usize::from(run.clip_rect.is_some()))
        })
}

/// The imported skin ([`json_support`]: JSON, a bitmap font and page
/// images through the importer, its atlas and bundle), committed on a
/// renderer. Its loading presentation, the chosen chart's bitmap text
/// prepared and installed between frames, builds its first and later
/// frames with LIFT's paint and its follower moving with covers changing
/// every frame, crossing the trim line and leaving it, and one at full
/// coverage, each frame's runs resolved to scissors, the inline ones
/// included; then an applied resize and the frames after it, a refused
/// frame and the poisoned one after it, and the next lifetime's first
/// frame. An attempt prepared with bitmap labels builds its first and
/// later play frames with the overlay, and a batch short of sprites and
/// one short of runs with sprites to spare keep the overlay whole. None
/// allocates; preparation and GPU work stay outside the counts.
fn imported_frames(chart: &Chart, overlay: &[&str], glyphs: usize) {
    let mut r = offscreen(1_920, 1_080);
    let (_dir, b) = json_support::bundle(&r);
    let token = r.install(ScreenKind::Play, &b, 1).unwrap();
    r.commit(token).unwrap();
    r.enter_loading(1);
    let labels = skin_model::ChartText {
        title: "AB",
        subtitle: "C",
        artist: "D",
        subartist: "",
        genre: "E",
    };
    r.prepare_loading_text(1, 1, skin_model::TextSlots::ALL, &labels)
        .unwrap();
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    let mut skin = DeclSkin::of(&r.installation(ScreenKind::Play).unwrap());
    let mut staging = Staging::with_runs(
        skin.capacity(),
        skin.run_capacity(),
        &skin.canvas([1920, 1080]),
    );
    staging.set_clips(b.clips().clone());
    // Lift cycling 0 to 990 ‰ by 30 ‰ a frame: the paint below, across
    // and above its line.
    let lifted = |lifetime: u64, k: i64, lift: u16| {
        let mut f = common::loading_frame(lifetime, TimeUs(k * FRAME_US), TimeUs(0));
        f.lanes.lift = lift;
        f.covers.lift = lift;
        f.covers.lift_enabled = true;
        f.text = 1;
        f
    };
    let at = |lifetime: u64, k: i64| {
        lifted(
            lifetime,
            k,
            u16::try_from((k * 30).rem_euclid(1_000)).unwrap(),
        )
    };
    let loading = |staging: &mut Staging, skin: &mut DeclSkin, f: &LoadingFrame<'_>| {
        staging
            .build_loading(skin, f)
            .map(|()| (staging.instances().len(), resolve(staging)))
    };
    skin.enter_loading(1);
    let (first, first_allocs) = counted(|| loading(&mut staging, &mut skin, &at(1, 0)));
    let (quads, (drawn, _)) = first.unwrap();
    assert!(quads > 4 && drawn > 0, "{quads} {drawn}");
    assert_eq!(first_allocs, 0, "an imported loading frame allocated");
    let (later, later_allocs) = counted(|| {
        (1..FRAMES)
            .map(|k| loading(&mut staging, &mut skin, &at(1, k)).map(|(_, (_, i))| i))
            .sum::<Result<usize, BuildError>>()
    });
    assert!(later.unwrap() > 0, "no run carried an inline clip");
    assert_eq!(later_allocs, 0, "a later imported loading frame allocated");
    // Full coverage: the lift at 1 000 ‰ (on), no field left above it.
    let (full, full_allocs) =
        counted(|| loading(&mut staging, &mut skin, &lifted(1, FRAMES, 1_000)));
    assert!(full.unwrap().0 > 0);
    assert_eq!(full_allocs, 0, "a fully covered imported frame allocated");

    let (resized, resize_allocs) = counted(|| {
        staging.project(&skin.canvas([2560, 1080]));
        (FRAMES..FRAMES + 60)
            .map(|k| loading(&mut staging, &mut skin, &at(1, k)).map(|(_, (_, i))| i))
            .sum::<Result<usize, BuildError>>()
    });
    assert!(resized.unwrap() > 0);
    assert_eq!(resize_allocs, 0, "an imported resize allocated");
    assert_eq!(staging.projection().target(), [2560, 1080]);

    let mut bad = at(1, FRAMES + 60);
    bad.covers.lift = bad.lanes.lift + 1;
    let (refused, error_allocs) = counted(|| {
        let first = staging.build_loading(&mut skin, &bad);
        (first, staging.build_loading(&mut skin, &at(1, FRAMES + 61)))
    });
    assert_eq!(
        refused,
        (
            Err(BuildError::InvalidReadouts),
            Err(BuildError::InvalidReadouts)
        )
    );
    assert_eq!(
        error_allocs, 0,
        "a refused imported loading frame allocated"
    );
    assert!(staging.instances().is_empty());
    let (second, second_allocs) = counted(|| {
        skin.enter_loading(2);
        loading(&mut staging, &mut skin, &at(2, FRAMES + 62))
    });
    assert!(second.unwrap().0 > 0);
    assert_eq!(second_allocs, 0, "an imported second lifetime allocated");

    // An attempt with bitmap labels, prepared outside the count.
    let ctx = context(&GAUGES, false);
    let a = render::AttemptInput {
        chart,
        context: &ctx,
        text: &labels,
        target: [1_920, 1_080],
    };
    let mut skin = DeclSkin::new(b.clone());
    skin.prepare(r.gpu(), &a).unwrap();
    let mut staging = Staging::with_runs(
        skin.capacity(),
        skin.run_capacity(),
        &skin.canvas([1920, 1080]),
    );
    staging.set_clips(b.clips().clone());
    let v = view(0, 0);
    let frame = |staging: &mut Staging, skin: &mut DeclSkin, k: i64| {
        let f = FrameInput {
            overlay,
            ..input(&v, TimeUs(k * FRAME_US), TimeUs(8 * MS), &[])
        };
        staging
            .build(skin, &f)
            .map(|()| (staging.instances().len(), resolve(staging)))
    };
    let (first, first_allocs) = counted(|| frame(&mut staging, &mut skin, 0));
    let (quads, _) = first.unwrap();
    // Four lines of bitmap glyphs, "AB" and "AB C" among them.
    assert!(quads >= glyphs + 9, "{quads}");
    assert_eq!(
        first_allocs, 0,
        "an imported attempt's first frame allocated"
    );
    let (later, later_allocs) = counted(|| {
        (1..FRAMES)
            .map(|k| frame(&mut staging, &mut skin, k).map(|(n, _)| n))
            .sum::<Result<usize, BuildError>>()
    });
    assert!(later.unwrap() > 0);
    assert_eq!(
        later_allocs, 0,
        "an imported attempt's later frame allocated"
    );
    assert_eq!(overlay_drawn(&staging), glyphs, "the overlay was cut");

    // A READY rearm (design.md "Play Lifecycle", the start gate): after
    // lane 1's long note was held (the skin draws a held body with
    // another image, from the note facts' hold), the attempt starts over
    // in place, its prepared bitmap text still attached: the frame inside
    // that note after the rearm, from the rearmed candidate's fresh view,
    // is a fresh attempt's, its body not held, and nothing allocates.
    let (index, long) = chart.lanes[1]
        .iter()
        .enumerate()
        .find(|(_, n)| matches!(n.kind, bms_model::NoteKind::Long { .. }))
        .unwrap();
    let t = long.time;
    let inside = FrameInput {
        overlay,
        ..input(&v, TimeUs(t.0 + 100 * MS), TimeUs(8 * MS), &[])
    };
    let mut fresh = DeclSkin::new(b.clone());
    fresh.prepare(r.gpu(), &a).unwrap();
    staging.build(&mut fresh, &inside).unwrap();
    let first_frame = staging.instances().to_vec();
    let held = [JudgeEvent {
        seq: 0,
        time: t,
        origin: Some(t),
        kind: JudgeEventKind::LnHeld {
            lane: lane(1),
            note: index,
            judgment: Judgment::Great,
            deviation: TimeUs(0),
            presses: 0,
        },
    }];
    let held_view = common::with_notes(view(0, 0), chart, &held);
    let holding = FrameInput {
        overlay,
        ..input(&held_view, t, TimeUs(8 * MS), &held)
    };
    staging.build(&mut skin, &holding).unwrap();
    // Held, its body draws the active image.
    let inside_held = FrameInput {
        overlay,
        ..input(&held_view, TimeUs(t.0 + 100 * MS), TimeUs(8 * MS), &[])
    };
    staging.build(&mut skin, &inside_held).unwrap();
    assert_ne!(
        staging.instances(),
        &first_frame[..],
        "the hold changed nothing"
    );
    let (rearmed, rearm_allocs) = counted(|| {
        skin.rearm();
        staging
            .build(&mut skin, &inside)
            .map(|()| (staging.instances().len(), resolve(&staging)))
    });
    let (quads, _) = rearmed.unwrap();
    assert!(quads >= glyphs + 9, "the bitmap text was dropped: {quads}");
    assert_eq!(
        staging.instances(),
        &first_frame[..],
        "the runtime did not start over"
    );
    assert_eq!(rearm_allocs, 0, "a rearm or the frame after it allocated");

    // A batch of 4 sprites in 2 runs: the skin's quads refused and
    // counted, the overlay whole from its reserve.
    let mut small = Staging::with_runs(4, 2, &skin.canvas([1920, 1080]));
    small.set_clips(b.clips().clone());
    let f = FrameInput {
        overlay,
        ..input(&v, TimeUs(0), TimeUs(8 * MS), &[])
    };
    let (dropped, full_allocs) = counted(|| {
        small
            .build(&mut skin, &f)
            .map(|()| (small.batch().dropped(), resolve(&small)))
    });
    assert!(dropped.unwrap().0 > 0, "the full batch counted no refusal");
    assert_eq!(overlay_drawn(&small), glyphs, "the overlay was cut");
    assert_eq!(full_allocs, 0, "an imported full batch allocated");

    // Room for every sprite but one run: the additive mark needs a second
    // run, so it is refused and counted, as is any later quad that cannot
    // extend the one run (an alpha unclipped quad still can), while
    // sprites are left over; the overlay follows whole from the reserve.
    let mut narrow = Staging::with_runs(skin.capacity(), 1, &skin.canvas([1920, 1080]));
    narrow.set_clips(b.clips().clone());
    let (refused, run_allocs) = counted(|| {
        narrow
            .build(&mut skin, &f)
            .map(|()| (narrow.batch().dropped(), resolve(&narrow)))
    });
    let (dropped, (drawn, _)) = refused.unwrap();
    assert!(dropped > 0, "the run-limited batch counted no refusal");
    let skin_quads = narrow.instances().len() - glyphs;
    assert!(
        skin_quads < usize::try_from(skin.capacity()).unwrap(),
        "no sprite left over: {skin_quads}"
    );
    assert_eq!(drawn, narrow.batch().runs().len(), "every run drawn");
    assert_eq!(overlay_drawn(&narrow), glyphs, "the overlay was cut");
    assert_eq!(run_allocs, 0, "a run-limited imported batch allocated");
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
    let token = r.install(ScreenKind::Play, &b, 1).unwrap();
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
    // The chart text's lines rasterized into the text layer: the title,
    // the artist, the genre and the full title.
    assert_eq!(r.text_report().counts.placements, 4);
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    let mut skin = DeclSkin::of(&r.installation(ScreenKind::Play).unwrap());
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
        play_level: Some(12),
        ..common::loading_frame(1, TimeUs(k * FRAME_US), TimeUs(0))
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
    // The frame's play level reaches the skin: its mark draws with it, and
    // not in a frame without one.
    staging.build_loading(&mut skin, &at(FRAMES)).unwrap();
    let with_level = staging.instances().len();
    let without = LoadingFrame {
        play_level: None,
        ..at(FRAMES)
    };
    staging.build_loading(&mut skin, &without).unwrap();
    assert_eq!(staging.instances().len() + 1, with_level);
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
    let next = common::loading_frame(2, TimeUs((FRAMES + 2) * FRAME_US), TimeUs(0));
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

/// The presentation snapshots the simple skin's frames are made from:
/// Floating at target green 300 with `H` from 1/100 to 3, covers moving
/// on and off, then no field left (SUDDEN+ or LIFT at 1 000 ‰).
fn snapshots() -> Vec<PresentationSnapshot> {
    let mut out = Vec::new();
    for k in 0..40_u16 {
        let covers = game::Covers {
            sudden: game::Cover {
                amount: k * 20,
                on: k % 3 != 0,
            },
            lift: game::Cover {
                amount: k * 10,
                on: k % 4 == 1,
            },
            hidden: game::Cover {
                amount: 1_000 - k * 20,
                on: k % 2 == 0,
            },
        };
        out.push(snapshot(covers, u64::from(k) * 3 + 1));
    }
    for (sudden, lift) in [(1_000, 0), (0, 1_000), (1_000, 1_000)] {
        let covers = game::Covers {
            sudden: game::Cover {
                amount: sudden,
                on: true,
            },
            lift: game::Cover {
                amount: lift,
                on: true,
            },
            hidden: game::Cover::default(),
        };
        out.push(snapshot(covers, 1));
    }
    out
}

/// A snapshot with `covers` and `H` `h`/40.
fn snapshot(covers: game::Covers, h: u64) -> PresentationSnapshot {
    PresentationSnapshot {
        options: game::PlayOptions::new(game::HiSpeedSettings::default(), covers).unwrap(),
        hispeed: game::Ratio::new(h, 40),
        target: None,
        display_offset_us: 0,
        adjusting: h.is_multiple_of(2),
        generation: 0,
        sequence: h,
        epoch: 0,
    }
}

/// The simple skin's frames made as the player makes them, each from a
/// presentation snapshot through [`render::presentation::frame`] at the
/// chart's tempo: the gain, the cover paints and the native line moving
/// from frame to frame, a snapshot of another attempt, the fields with
/// nothing left, and the frames after a rearm, from the first, allocate
/// nothing.
fn simple_presented_frames(chart: &Chart, overlay: &[&str]) {
    let snapshots = snapshots();
    let mut skin = SimpleSkin::new();
    skin.prepare_attempt(chart, None).unwrap();
    let mut staging = Staging::new(
        u32::try_from(MAX_SPRITES).unwrap(),
        &skin.canvas([1920, 1080]),
    );
    let v = view(42, 1_234);
    let refs = game::References::all(game::Ratio::integer(150));
    let frame = |staging: &mut Staging, skin: &mut SimpleSkin, k: i64| {
        let i = usize::try_from(k).unwrap() % snapshots.len();
        let (lanes, readouts) = render::presentation::frame(
            &snapshots[i],
            Some(game::Ratio::integer(150)),
            Some(refs),
            bms_model::Speed::NORMAL,
            k % 7 != 6,
        );
        let f = FrameInput {
            lanes,
            readouts,
            overlay,
            ..input(&v, TimeUs(500 * MS + k * FRAME_US), TimeUs(8 * MS), &[])
        };
        staging.build(skin, &f).map(|()| staging.instances().len())
    };
    let (first, first_allocs) = counted(|| frame(&mut staging, &mut skin, 0));
    assert!(first.unwrap() > 0);
    assert_eq!(first_allocs, 0, "a presented first frame allocated");
    let (drawn, later_allocs) = counted(|| {
        (1..FRAMES)
            .map(|k| frame(&mut staging, &mut skin, k))
            .sum::<Result<usize, BuildError>>()
    });
    assert!(drawn.unwrap() > 0);
    assert_eq!(later_allocs, 0, "a presented later frame allocated");
    for k in 0..3 {
        let (drawn, rearm_allocs) = counted(|| {
            skin.rearm();
            (0..i64::try_from(snapshots.len()).unwrap())
                .map(|j| frame(&mut staging, &mut skin, j))
                .sum::<Result<usize, BuildError>>()
        });
        assert!(drawn.unwrap() > 0);
        assert_eq!(rearm_allocs, 0, "rearm {k} or a presented frame allocated");
    }
}

/// The full lane fixture on a declarative skin: its first frame at a
/// field draws every note category, the bar group and each cover paint
/// as quads, then frames moving the playhead, the gain and the covers,
/// and frames with no field left (LIFT or SUDDEN+ at 1 000 ‰), which
/// draw no note geometry and keep SUDDEN+'s paint, all allocate nothing.
fn declarative_lane_frames() {
    let r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let look = Look {
        structure: true,
        full_lanes: true,
        ..Look::left([255, 0, 0, 255])
    };
    let (b, _) = bundle(&r, dir.path(), &look);
    let chart = lane_chart();
    let ctx = context(&GAUGES, false);
    let mut skin = DeclSkin::new(b.clone());
    skin.prepare(r.gpu(), &attempt(&r, &chart, &ctx)).unwrap();
    let canvas = skin.canvas([1920, 1080]);
    let mut staging = Staging::with_runs(skin.capacity(), skin.run_capacity(), &canvas);
    staging.set_clips(b.clips().clone());
    let v = view(0, 0);
    let frame = |staging: &mut Staging, skin: &mut DeclSkin, now_ms: i64, lanes: LaneView| {
        let f = FrameInput {
            lanes,
            readouts: full_readouts(lanes),
            ..input(&v, TimeUs(now_ms * MS), TimeUs(0), &[])
        };
        staging.build(skin, &f)
    };
    let (first, first_allocs) = counted(|| frame(&mut staging, &mut skin, 900, FULL_LANES));
    assert_eq!(first, Ok(()));
    assert_eq!(
        first_allocs, 0,
        "the full lane fixture's first frame allocated"
    );
    let counts = part_counts(&r, &b, staging.instances());
    assert!(counts.iter().all(|&n| n > 0), "{counts:?}");
    let (later, later_allocs) = counted(|| {
        (1..FRAMES).try_for_each(|k| {
            let lanes = LaneView {
                gain: skin_model::Gain::new(1 + u64::try_from(k % 20).unwrap(), 3),
                cover: u16::try_from(k % 900).unwrap(),
                ..FULL_LANES
            };
            frame(&mut staging, &mut skin, 900 + k, lanes)
        })
    });
    assert_eq!(later, Ok(()));
    assert_eq!(
        later_allocs, 0,
        "the full lane fixture's later frames allocated"
    );
    for (cover, lift) in [(200, 1_000), (1_000, 100), (1_000, 1_000)] {
        // The field first draws every category.
        assert_eq!(frame(&mut staging, &mut skin, 900, FULL_LANES), Ok(()));
        let counts = part_counts(&r, &b, staging.instances());
        assert!(counts.iter().all(|&n| n > 0), "{counts:?}");
        let zero = LaneView {
            cover,
            lift,
            ..FULL_LANES
        };
        let (built, allocs) = counted(|| frame(&mut staging, &mut skin, 900, zero));
        assert_eq!(built, Ok(()));
        assert_eq!(allocs, 0, "a frame with no field allocated");
        let counts = part_counts(&r, &b, staging.instances());
        let case = format!("cover {cover}, lift {lift}: {counts:?}");
        for p in Part::NOTES {
            assert_eq!(counts[p as usize - 1], 0, "{p:?}, {case}");
        }
        assert!(counts[Part::Sudden as usize - 1] > 0, "{case}");
    }
    // READY rearmed: the first and later frames after it, at a field and
    // at none, allocate nothing, and the first draws every category.
    for k in 0..3 {
        let (built, allocs) = counted(|| {
            skin.rearm();
            frame(&mut staging, &mut skin, 900, FULL_LANES)
        });
        assert_eq!(built, Ok(()));
        assert_eq!(allocs, 0, "rearm {k} or its first frame allocated");
        let counts = part_counts(&r, &b, staging.instances());
        assert!(counts.iter().all(|&n| n > 0), "rearm {k}: {counts:?}");
        let (later, allocs) = counted(|| {
            (1..60).try_for_each(|j| {
                let lanes = LaneView {
                    cover: if j % 10 == 0 { 1_000 } else { 200 },
                    ..FULL_LANES
                };
                frame(&mut staging, &mut skin, 900 + j * 17, lanes)
            })
        });
        assert_eq!(later, Ok(()));
        assert_eq!(allocs, 0, "a frame after rearm {k} allocated");
    }
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
    let (b, _) = bundle(&r, dir.path(), &look);
    let ctx = context(&GAUGES, false);
    let mut skin = DeclSkin::new(b.clone());
    skin.prepare(r.gpu(), &attempt(&r, chart, &ctx)).unwrap();
    // The text element's line: frame 0, as a staging would attach one.
    let line = TextImage::new(b.uvs().get(0).unwrap(), [0.0, 0.0, 1.0, 1.0], 1.0);
    skin.attach_text(TextTable::new(vec![line])).unwrap();
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

/// Play frames of an imported skin whose title, artist and a literal are
/// rasterized outline text, staged and committed on WARP: the renderer's
/// builds (what each frame does before `submit` calls into wgpu) draw the
/// lines and allocate nothing, from the first frame on; then its loading
/// frames with the chosen chart's lines prepared and installed.
fn outline_frames(chart: &Chart, overlay: &[&str]) {
    use outline_support::{Value, default_font, text, ttf};
    let mut r = offscreen(1_920, 1_080);
    let font = font_support::build(&font_support::latin("Skin"));
    let dir = outline_support::folder(
        &[("ttf.ttf", font)],
        &[ttf(), outline_support::distance_field()],
        &[
            text("f", 100, Value::Ref(10), [100, 100, 800, 100]),
            text("f", 60, Value::Ref(16), [100, 300, 800, 60]),
            text(
                "d",
                40,
                Value::Literal("Lit".to_owned()),
                [100, 400, 800, 40],
            ),
        ],
    );
    let b = outline_support::bundle_in(&r, dir.path(), &default_font());
    let labels = skin_model::ChartText {
        title: "Title",
        subtitle: "",
        artist: "Artist",
        subartist: "",
        genre: "",
    };
    let ctx = context(&GAUGES, false);
    let a = render::AttemptInput {
        chart,
        context: &ctx,
        text: &labels,
        target: r.target(),
    };
    let token = r.stage(&b, &a, 1).unwrap();
    r.commit(token).unwrap();
    assert_eq!(r.text_report().counts.placements, 3);
    let v = view(0, 0);
    let frame = |k: i64| FrameInput {
        overlay,
        ..input(&v, TimeUs(k * FRAME_US), TimeUs(0), &[])
    };
    let (first, first_allocs) = counted(|| r.build(&frame(0)));
    first.unwrap();
    assert_eq!(first_allocs, 0, "an outline text frame allocated");
    let (later, later_allocs) = counted(|| {
        (1..FRAMES)
            .try_for_each(|k| r.build(&frame(k)))
            .map(|()| FRAMES - 1)
    });
    assert!(later.unwrap() > 0);
    assert_eq!(later_allocs, 0, "a later outline text frame allocated");
    r.submit().unwrap();

    // The loading presentation's chart text rasterized by the same
    // producer into the same layer and installed between frames: the
    // renderer's loading builds draw it and allocate nothing either.
    r.enter_loading(1);
    let chosen = skin_model::ChartText {
        title: "Chosen",
        ..labels
    };
    r.prepare_loading_text(1, 1, skin_model::TextSlots::ALL, &chosen)
        .unwrap();
    // The new title placed; the artist and the literal hits.
    let counts = r.text_report().counts;
    assert_eq!((counts.placements, counts.hits), (1, 2), "{counts:?}");
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    let at = |k: i64| common::loading_frame(1, TimeUs(k * FRAME_US), TimeUs(0));
    let (first, first_allocs) = counted(|| r.build_loading(&at(0)));
    first.unwrap();
    assert_eq!(first_allocs, 0, "an outline text loading frame allocated");
    let (later, later_allocs) = counted(|| {
        (1..FRAMES)
            .try_for_each(|k| r.build_loading(&at(k)))
            .map(|()| FRAMES - 1)
    });
    assert!(later.unwrap() > 0);
    assert_eq!(
        later_allocs, 0,
        "a later outline text loading frame allocated"
    );
}
