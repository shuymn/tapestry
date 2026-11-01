//! Shared by the WARP tests: the renderer on WARP, frames, and finding a
//! note in the read back.

#![allow(dead_code)]

use bms_model::{Chart, Lane, TimeUs};
use eval::{GaugeState, JudgeEvent, Tally};
use game::PlayView;
use render::{AdapterChoice, AttemptInput, FrameInput, Renderer, Target};
use rules::{GaugeValue, PerJudgment};
use skin_model::{Attempt, ChartText, GaugeDesc, GaugeLook, PlayContext, Side};

pub const MS: i64 = 1_000;
/// A 5:4 target: the layout is scaled by 2/3 to 1280×720 with 152 rows of
/// black above and below, so expected rows also check the letterbox.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 1024;
pub const VISIBLE: TimeUs = TimeUs(600 * MS);
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
/// loaded.
pub fn warp(chart: &Chart) -> Renderer {
    let mut r = offscreen(WIDTH, HEIGHT);
    r.load(chart, None).expect("load the chart");
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
    }
}

/// Prepares the play skin in use for an attempt of `chart` with
/// [`GAUGES`].
pub fn prepare(r: &mut Renderer, chart: &Chart) {
    let ctx = context(&GAUGES, false);
    r.prepare_attempt(&attempt(r, chart, &ctx))
        .expect("prepare the attempt");
}

pub fn view(combo: u32, ex_score: u32) -> PlayView {
    PlayView {
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
        },
        gauges: vec![GaugeState {
            value: GaugeValue(GaugeValue::PERCENT * 62),
            reference: GaugeValue(GaugeValue::PERCENT * 71),
            failed_at: None,
        }],
        complete: false,
        playing: true,
        events: 31,
    }
}

pub fn input<'a>(
    view: &'a PlayView,
    now: TimeUs,
    display_offset: TimeUs,
    effects: &'a [JudgeEvent],
) -> FrameInput<'a> {
    FrameInput {
        now,
        display_offset,
        visible_time: VISIBLE,
        view,
        effects,
        gauge: 0,
        overlay: &[],
        target: [WIDTH, HEIGHT],
    }
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
