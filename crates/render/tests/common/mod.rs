//! Shared by the WARP tests: the renderer on WARP, frames, and finding a
//! note in the read back.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use bms_model::{Chart, Lane, NoteKind, TimeUs};
use eval::{GaugeState, JudgeEvent, JudgeEventKind, NoteFacts, NoteHold, Section, Tally};
use game::PlayView;
use render::text::MenuText;
use render::{AdapterChoice, AttemptInput, FrameInput, LoadingFrame, Renderer, SkinBundle, Target};
use rules::{GaugeValue, PerJudgment};
use skin_model::{
    Attempt, ChartText, FrameReadouts, Gain, GaugeDesc, GaugeLook, LaneView, Layout, LoadState,
    PlayContext, ScreenKind, Side,
};

pub const MS: i64 = 1_000;
/// A 5:4 target: the layout is scaled by 2/3 to 1280×720 with 152 rows of
/// black above and below, so expected rows also check the letterbox.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 1024;
/// Every skin's lanes: `H` 10/3, which puts 600 ms of 120 BPM over the
/// original lane, and no cover.
pub const LANES: LaneView = LaneView {
    gain: Gain::new(10, 3),
    ..LaneView::NONE
};
/// The name WARP reports.
pub const WARP: &str = "Microsoft Basic Render Driver";

pub fn lane(i: u8) -> Lane {
    Lane::new(i).unwrap()
}

/// A renderer on WARP drawing into a `width`×`height` offscreen target.
/// No adapter is a failure, never a skip (design.md "Testing Strategy").
pub fn offscreen(width: u32, height: u32) -> Renderer {
    let r = Renderer::new(Target::Offscreen { width, height }, AdapterChoice::Warp)
        .expect("WARP renderer");
    let name = &r.gpu().adapter().name;
    println!("adapter: {name}");
    assert_eq!(name, WARP);
    assert!(!r.has_frame_waitable());
    r
}

/// A renderer on WARP ([`offscreen`], [`WIDTH`]×[`HEIGHT`]) with `chart`
/// loaded and a whole-chart attempt prepared ([`prepare`]), as a play
/// prepares one before its frames.
pub fn warp(chart: &Chart) -> Renderer {
    let mut r = offscreen(WIDTH, HEIGHT);
    r.load(chart, None).expect("load the chart");
    prepare(&mut r, chart);
    r
}

/// The one gauge the test attempts have, starting at 20 %.
pub const GAUGES: [GaugeDesc; 1] = [GaugeDesc {
    start: GaugeValue(20 * GaugeValue::PERCENT),
    max: GaugeValue::FULL,
    look: GaugeLook::Normal,
    border: GaugeValue(80 * GaugeValue::PERCENT),
}];

/// A whole-chart attempt with `gauges`, counting empty POORs as POORs
/// when `counted`: [`view`] then counts more empty POORs than POORs, a
/// snapshot a declarative skin refuses.
pub fn context(gauges: &[GaugeDesc], counted: bool) -> PlayContext<'_> {
    PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges,
        attempt: Attempt::Full {
            lead_in: TimeUs(1_000 * MS),
        },
        empty_poor_breaks_combo: false,
        empty_poor_counted: counted,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(3_600_000 * MS),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
    }
}

/// The chart labels the test attempts are prepared with.
pub const TEXT: ChartText<'static> = ChartText {
    title: "title",
    subtitle: "",
    artist: "artist",
    subartist: "",
    genre: "genre",
};

/// `chart`'s attempt for `ctx` on `r`'s target.
pub fn attempt<'a>(r: &Renderer, chart: &'a Chart, ctx: &'a PlayContext<'a>) -> AttemptInput<'a> {
    AttemptInput {
        chart,
        context: ctx,
        text: &TEXT,
        target: r.target(),
        continuation: skin_model::Continuation::Fresh,
    }
}

/// Prepares the play skin in use for an attempt of `chart` with
/// [`GAUGES`].
pub fn prepare(r: &mut Renderer, chart: &Chart) {
    let ctx = context(&GAUGES, false);
    r.prepare_attempt(&attempt(r, chart, &ctx))
        .expect("prepare the attempt");
}

/// Prepares the play skin in use for a practice round of `chart` over
/// `section`, which gives [`GAUGES`] their initial values.
pub fn prepare_section(r: &mut Renderer, chart: &Chart, section: &Section) {
    let ctx = PlayContext {
        attempt: Attempt::Section(section),
        ..context(&GAUGES, false)
    };
    r.prepare_attempt(&attempt(r, chart, &ctx))
        .expect("prepare the round");
}

pub fn view(combo: u32, ex_score: u32) -> PlayView {
    PlayView {
        generation: 0,
        origin: None,
        finalized: None,
        tally: Tally {
            counts: PerJudgment {
                perfect: 3,
                great: 5,
                good: 7,
                bad: 11,
                poor: 13,
            },
            ex_score,
            bp: 17,
            combo,
            max_combo: combo + 19,
            empty_poor: 23,
            ..Default::default()
        },
        gauges: vec![GaugeState {
            value: GaugeValue(GaugeValue::PERCENT * 62),
            reference: GaugeValue(GaugeValue::PERCENT * 71),
            failed_at: None,
        }],
        notes: NoteFacts::default(),
        input: eval::InputFacts::NONE,
        complete: false,
        playing: true,
        closing: None,
        events: 31,
        offsets: game::OffsetRing::EMPTY,
    }
}

/// What the evaluator's note facts become after `ev`, for tests that
/// drive frames with effects: a judged tap or a hit mine is done, a long
/// note's head press before its end (held, or a BAD head) takes it and
/// starts the lane's hold, and its hold's end or its completion ends it. Drops are modelled
/// by changing the facts without effects.
pub fn follow(notes: &mut NoteFacts, chart: &Chart, ev: &JudgeEvent) {
    let end_of = |lane: Lane, note: usize| match chart.lanes[lane.index()].get(note)?.kind {
        NoteKind::Long { end, .. } => Some(end),
        NoteKind::Tap | NoteKind::Mine { .. } => None,
    };
    match ev.kind {
        JudgeEventKind::Judged {
            lane,
            note,
            deviation,
            release,
            ..
        } => {
            notes.set_done(lane, note);
            let Some(end) = end_of(lane, note) else {
                return;
            };
            if deviation.is_some() && release.is_none() && ev.time < end {
                notes.set_hold(
                    lane,
                    Some(NoteHold {
                        note,
                        start: ev.time,
                        end,
                    }),
                );
            } else if notes.hold(lane).is_some_and(|h| h.note == note) {
                // Its completion ends its hold.
                notes.set_hold(lane, None);
            }
        }
        JudgeEventKind::LnHeld { lane, note, .. } => {
            notes.set_done(lane, note);
            if let Some(end) = end_of(lane, note) {
                notes.set_hold(
                    lane,
                    Some(NoteHold {
                        note,
                        start: ev.time,
                        end,
                    }),
                );
            }
        }
        JudgeEventKind::LnHoldEnded { lane, note } => {
            if notes.hold(lane).is_some_and(|h| h.note == note) {
                notes.set_hold(lane, None);
            }
        }
        JudgeEventKind::MineHit { lane, note } => notes.set_done(lane, note),
        JudgeEventKind::EmptyPoor { .. }
        | JudgeEventKind::PlaySound { .. }
        | JudgeEventKind::Gauge { .. }
        | JudgeEventKind::SongEnd => {}
    }
}

/// `v` with note facts of `chart` following `effects` ([`follow`]).
pub fn with_notes(v: PlayView, chart: &Chart, effects: &[JudgeEvent]) -> PlayView {
    let mut notes = NoteFacts::new(chart);
    for ev in effects {
        follow(&mut notes, chart, ev);
    }
    PlayView { notes, ..v }
}

pub fn input<'a>(
    view: &'a PlayView,
    now: TimeUs,
    display_offset: TimeUs,
    effects: &'a [JudgeEvent],
) -> FrameInput<'a> {
    FrameInput {
        state_us: 0,
        now,
        display_offset,
        lanes: LANES,
        readouts: FrameReadouts::UNAVAILABLE,
        view,
        effects,
        gauge: 0,
        overlay: &[],
        target: [WIDTH, HEIGHT],
        turntable: 0,
    }
}

/// A loading frame of lifetime `lifetime` at `now` on the loading clock,
/// from its origin `since`: the author's layout, every cover 0 and off and
/// not being adjusted, no gain, no gauge descriptor, progress 0 and no
/// text. The renderer applies its own target.
pub const fn loading_frame(lifetime: u64, now: TimeUs, since: TimeUs) -> LoadingFrame<'static> {
    LoadingFrame {
        display_offset: TimeUs(0),
        fix_hispeed: 0,
        difficulty: None,
        bga: None,
        autoplay: false,
        lifetime,
        now,
        load: LoadState { since, percent: 0 },
        target: [1_920, 1_080],
        layout: &Layout::AUTHOR,
        turntable: 0,
        lanes: LaneView::NONE,
        covers: FrameReadouts::UNAVAILABLE.covers,
        gauge: None,
        text: 0,
        play_level: None,
    }
}

/// `r`'s read back once its submitted frames finished, within 10 s.
#[track_caller]
pub fn read_back(r: &Renderer) -> Vec<u8> {
    let done = r.gpu().device().poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(Duration::from_secs(10)),
    });
    assert!(done.is_ok(), "frames: {done:?}");
    r.read_back()
}

/// Draws loading frame `f` with `text` and reads it back.
#[track_caller]
pub fn loading(r: &mut Renderer, text: &MenuText, f: &LoadingFrame<'_>) -> Vec<u8> {
    let timing = r.loading_frame(f, text).unwrap().unwrap();
    assert!(timing.presented);
    read_back(r)
}

/// Installs `b` chart-independently for `request` and commits it.
pub fn install(r: &mut Renderer, b: &Arc<SkinBundle>, request: u64) {
    let token = r.install(ScreenKind::Play, b, request).unwrap();
    r.commit(token).unwrap();
}

/// The RGB of pixel `(x, y)` of a `width`-wide RGBA8 read back.
pub fn rgb_at(pixels: &[u8], width: u32, (x, y): (u32, u32)) -> [u8; 3] {
    let i = ((y * width + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

/// Rows of pixel column `x` that are note white (every channel above 200),
/// in a read back of a [`WIDTH`]×[`HEIGHT`] target.
pub fn white_rows(pixels: &[u8], x: u32) -> Vec<u32> {
    white_rows_in(pixels, (WIDTH, HEIGHT), x)
}

/// [`white_rows`] in a read back of a `width`×`height` target.
pub fn white_rows_in(pixels: &[u8], (width, height): (u32, u32), x: u32) -> Vec<u32> {
    (0..height)
        .filter(|&y| {
            let i = ((y * width + x) * 4) as usize;
            pixels[i..i + 3].iter().all(|&c| c > 200)
        })
        .collect()
}

/// The middle of the single run of note rows in column `x`, in a read
/// back of a [`WIDTH`]×[`HEIGHT`] target.
#[track_caller]
pub fn note_center(pixels: &[u8], x: u32) -> f64 {
    note_center_in(pixels, (WIDTH, HEIGHT), x)
}

/// [`note_center`] in a read back of a `width`×`height` target.
#[track_caller]
pub fn note_center_in(pixels: &[u8], size: (u32, u32), x: u32) -> f64 {
    let rows = white_rows_in(pixels, size, x);
    assert!(!rows.is_empty(), "no note in column {x}");
    let (first, last) = (rows[0], rows[rows.len() - 1]);
    assert_eq!(
        (last - first + 1) as usize,
        rows.len(),
        "one contiguous note in column {x}: {rows:?}"
    );
    f64::from(first + last + 1) / 2.0
}
