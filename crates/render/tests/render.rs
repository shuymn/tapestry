//! The simple skin drawn on WARP and read back: notes sit where the
//! attempt's exact playhead, the hi-speed visible time and the display
//! latency offset put them, so a note meets the judgment line at its
//! judgment µs even at a fractional-µs time; a refused preparation leaves
//! nothing of its attempt drawn; and its feedback fades with its colour
//! composited once. Sprites are drawn
//! in push order whatever their blends, premultiplied, turned about their
//! pivots.
//!
//! Expected rows are worked out by hand from the layout: the lane runs from
//! y = 80 to the judgment line at y = 880 (800 px), a note is 16 px tall
//! and centered on its position, and the visible time is 600 ms. Every
//! chart here starts at 120 BPM, so the lane holds a fixed 1.2 beats (600 ms
//! at the start BPM) for the whole play: 666.67 px per beat. The 1280×1024
//! target shows the 1920×1080 layout at 2/3 scale with 152 black rows on
//! top, so a layout row y lands on screen row 152 + 2y/3. Lane 7 spans
//! x = 640..704 (screen column 448 at its middle), lane 5 x = 512..576
//! (screen column 362).

mod common;

use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};
use common::{MS, input, lane, note_center, view, warp, white_rows};
use eval::{JudgeEvent, JudgeEventKind};
use render::text::MenuText;
use render::{Blend, Renderer, Sprite, SpriteBatch};
use rules::Judgment;

const LANE_7: u32 = 448;
const LANE_5: u32 = 362;

#[track_caller]
fn assert_row(pixels: &[u8], x: u32, expected: f64) -> f64 {
    let got = note_center(pixels, x);
    assert!(
        (got - expected).abs() <= 1.0,
        "note at row {got}, expected {expected}"
    );
    got
}

/// Draws a frame started at `now_us` with `offset_ms` of display latency
/// and reads it back.
fn draw(r: &mut Renderer, now_us: i64, offset_ms: i64) -> Vec<u8> {
    let v = view(0, 0);
    let timing = r
        .frame(&input(&v, TimeUs(now_us), TimeUs(offset_ms * MS), &[]))
        .unwrap();
    assert!(timing.presented);
    assert_eq!(timing.dropped, 0);
    r.read_back()
}

/// 120 BPM, then 240 BPM from 1.5 s; one lane 7 note at 1.75 s. Positions
/// in beats: 2 per second up to 1.5 s (3 beats), 4 per second after, so the
/// note is at beat 4.
fn bpm_change_chart() -> Chart {
    ChartBuilder::new(120_000)
        .bpm(TimeUs(1_500 * MS), 240_000)
        .tap(lane(7), TimeUs(1_750 * MS))
        .build()
        .unwrap()
}

#[test]
fn note_is_drawn_where_the_playhead_says() {
    // The display offset is 20 ms, so a frame started at `now` shows
    // `now + 20 ms`:
    // - shown at 1.45 s (beat 2.9): 1.1 beats ahead, y = 880 - 1.1 ×
    //   666.67 = 146.67, screen row 249.78;
    // - shown at 1.55 s (beat 3.2): y = 880 - 0.8 × 666.67 = 346.67,
    //   screen 383.11;
    // - shown at 1.75 s: on the line, y = 880, screen 738.67.
    // Without the offset these would be rows 232.00, 347.56 and 703.11.
    let mut r = warp(&bpm_change_chart());
    let mut checked = 0;
    for (now, row) in [(1_430, 249.78), (1_530, 383.11), (1_730, 738.67)] {
        assert_row(&draw(&mut r, now * MS, 20), LANE_7, row);
        checked += 1;
    }
    assert_eq!(checked, 3);
}

#[test]
fn bpm_change_speeds_notes_up_without_a_jump() {
    // Shown at 1.45 s, 1.499 s, 1.5 s, 1.501 s and 1.55 s: beats 2.9,
    // 2.998, 3.0, 3.004 and 3.2, so y = 146.67, 212.00, 213.33, 216.00
    // and 346.67, screen rows 249.78, 293.33, 294.22, 296.00 and 383.11.
    // Before the change the note moves 44.44 rows per 50 ms, after it
    // 88.89: twice as fast at twice the BPM.
    let mut r = warp(&bpm_change_chart());
    let cases = [
        (1_450_000, 249.78),
        (1_499_000, 293.33),
        (1_500_000, 294.22),
        (1_501_000, 296.0),
        (1_550_000, 383.11),
    ];
    let rows: Vec<f64> = cases
        .iter()
        .map(|&(now, row)| assert_row(&draw(&mut r, now, 0), LANE_7, row))
        .collect();
    assert_eq!(rows.len(), 5);
    // No jump: one millisecond either side of the change moves the note
    // under 4 rows (2.67 expected).
    assert!(rows[3] - rows[1] < 4.0, "{rows:?}");
    let before = rows[2] - rows[0];
    let after = rows[4] - rows[2];
    assert!(before > 40.0);
    assert!((after / before - 2.0).abs() < 0.1, "{before} then {after}");
}

#[test]
fn visible_time_is_exact_at_the_start_bpm() {
    // 120 BPM, 240 BPM from 1.5 s; a lane 7 note at 1.0 s, beat 2. Shown at
    // 0.4 s (beat 0.8), exactly 600 ms before it, the note is centered on
    // the lane top (y = 80) and only its lower half shows, y = 80..88,
    // screen rows 205.33..210.67, middle 208.0. 6 ms earlier it is wholly
    // above the lane.
    let chart = ChartBuilder::new(120_000)
        .bpm(TimeUs(1_500 * MS), 240_000)
        .tap(lane(7), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let mut r = warp(&chart);
    assert_eq!(
        white_rows(&draw(&mut r, 394 * MS, 0), LANE_7),
        Vec::<u32>::new()
    );
    let top = draw(&mut r, 400 * MS, 0);
    assert_row(&top, LANE_7, 208.0);
    assert!(white_rows(&top, LANE_7).len() <= 6);
    assert_row(&draw(&mut r, 1_000 * MS, 0), LANE_7, 738.67);
}

#[test]
fn stop_freezes_notes() {
    // 120 BPM with a STOP from 1.0 s to 1.5 s; a note at 1.6 s, beat 2.2.
    // During the STOP the scroll stays at beat 2: 0.2 beats ahead, y =
    // 880 - 133.33 = 746.67, screen 649.78. Before it, at 0.95 s (beat
    // 1.9), y = 680, screen 605.33, and at 0.999 s y = 745.33, screen
    // 648.89; after it, at 1.501 s, y = 748, screen 650.67, and at 1.55 s
    // (beat 2.1), y = 813.33, screen 694.22.
    let chart = ChartBuilder::new(120_000)
        .stop(TimeUs(1_000 * MS), TimeUs(500 * MS))
        .tap(lane(7), TimeUs(1_600 * MS))
        .build()
        .unwrap();
    let mut r = warp(&chart);
    let cases = [
        (950_000, 605.33),
        (999_000, 648.89),
        (1_000_000, 649.78),
        (1_001_000, 649.78),
        (1_250_000, 649.78),
        (1_499_000, 649.78),
        (1_500_000, 649.78),
        (1_501_000, 650.67),
        (1_550_000, 694.22),
    ];
    let mut frozen = Vec::new();
    let mut rows = Vec::new();
    for (now, row) in cases {
        let pixels = draw(&mut r, now, 0);
        rows.push(assert_row(&pixels, LANE_7, row));
        if (1_000_000..=1_500_000).contains(&now) {
            frozen.push(white_rows(&pixels, LANE_7));
        }
    }
    assert_eq!(frozen.len(), 5);
    assert!(!frozen[0].is_empty());
    assert!(frozen.iter().all(|r| *r == frozen[0]));
    // No jump into or out of the STOP.
    assert!(
        rows[2] - rows[1] < 2.0 && rows[7] - rows[6] < 2.0,
        "{rows:?}"
    );
}

#[test]
fn judged_note_disappears() {
    // 120 BPM; notes at 2.0 s in lanes 5 and 7, shown at 1.8 s: 0.4 beats
    // ahead, y = 613.33, screen 560.89.
    let chart = ChartBuilder::new(120_000)
        .tap(lane(5), TimeUs(2_000 * MS))
        .tap(lane(7), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let mut r = warp(&chart);
    let v = view(1, 2);
    let now = TimeUs(1_800 * MS);
    r.frame(&input(&v, now, TimeUs(0), &[])).unwrap();
    let before = r.read_back();
    assert_row(&before, LANE_5, 560.89);
    assert_row(&before, LANE_7, 560.89);
    // An early PERFECT on the lane 7 note.
    let judged = JudgeEvent {
        seq: 0,
        time: TimeUs(1_790 * MS),
        origin: Some(TimeUs(1_789 * MS)),
        kind: JudgeEventKind::Judged {
            lane: lane(7),
            note: 0,
            judgment: Judgment::Perfect,
            deviation: Some(TimeUs(-210 * MS)),
            release: None,
            combo: 1,
        },
    };
    r.frame(&input(&v, now, TimeUs(0), &[judged])).unwrap();
    let after = r.read_back();
    assert_row(&after, LANE_5, 560.89);
    assert_eq!(white_rows(&after, LANE_7), Vec::<u32>::new());
    // The bit stays set on later frames with no new effect.
    r.frame(&input(&v, TimeUs(1_850 * MS), TimeUs(0), &[]))
        .unwrap();
    let later = r.read_back();
    assert!(!white_rows(&later, LANE_5).is_empty());
    assert_eq!(white_rows(&later, LANE_7), Vec::<u32>::new());
}

/// A READY rearm (design.md "Play Lifecycle", the start gate) through
/// `Renderer::rearm`: the simple skin forgets what the cancelled
/// candidate's effects built up, so a note it judged is drawn again, while
/// its chart stays loaded.
#[test]
fn a_rearm_draws_a_judged_note_again() {
    let chart = ChartBuilder::new(120_000)
        .tap(lane(5), TimeUs(2_000 * MS))
        .tap(lane(7), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let mut r = warp(&chart);
    let v = view(1, 2);
    let now = TimeUs(1_800 * MS);
    let judged = JudgeEvent {
        seq: 0,
        time: TimeUs(1_790 * MS),
        origin: Some(TimeUs(1_789 * MS)),
        kind: JudgeEventKind::Judged {
            lane: lane(7),
            note: 0,
            judgment: Judgment::Perfect,
            deviation: Some(TimeUs(-210 * MS)),
            release: None,
            combo: 1,
        },
    };
    r.frame(&input(&v, now, TimeUs(0), &[judged])).unwrap();
    assert_eq!(white_rows(&r.read_back(), LANE_7), Vec::<u32>::new());
    let memory = r.memory();
    r.rearm();
    assert_eq!(r.memory(), memory, "the rearm kept the attempt's memory");
    r.frame(&input(&v, now, TimeUs(0), &[])).unwrap();
    let rearmed = r.read_back();
    assert_row(&rearmed, LANE_5, 560.89);
    assert_row(&rearmed, LANE_7, 560.89);
}

/// A practice round's READY rearmed through `Renderer::rearm`: the note
/// the cancelled candidate judged is drawn again, and the notes before and
/// after the section stay hidden, as its load left them.
#[test]
fn a_rearmed_practice_round_draws_only_its_targets() {
    let chart = ChartBuilder::new(120_000)
        .tap(lane(7), TimeUs(1_900 * MS))
        .tap(lane(7), TimeUs(2_000 * MS))
        .tap(lane(7), TimeUs(2_100 * MS))
        .build()
        .unwrap();
    let section = eval::Section {
        start: TimeUs(1_950 * MS),
        end: TimeUs(2_050 * MS),
        lead_in: TimeUs(0),
        initial_gauges: Vec::new(),
    };
    let mut r = common::offscreen(common::WIDTH, common::HEIGHT);
    r.load(&chart, Some(&section)).unwrap();
    common::prepare_section(&mut r, &chart, &section);
    let v = view(1, 2);
    let now = TimeUs(1_800 * MS);
    let judged = JudgeEvent {
        seq: 0,
        time: TimeUs(1_790 * MS),
        origin: Some(TimeUs(1_789 * MS)),
        kind: JudgeEventKind::Judged {
            lane: lane(7),
            note: 1,
            judgment: Judgment::Perfect,
            deviation: Some(TimeUs(-210 * MS)),
            release: None,
            combo: 1,
        },
    };
    r.frame(&input(&v, now, TimeUs(0), &[judged])).unwrap();
    assert_eq!(white_rows(&r.read_back(), LANE_7), Vec::<u32>::new());
    let memory = r.memory();
    r.rearm();
    assert_eq!(r.memory(), memory, "the rearm kept the attempt's memory");
    r.frame(&input(&v, now, TimeUs(0), &[])).unwrap();
    assert_row(&r.read_back(), LANE_7, 560.89);
}

/// A 1920×1080 target: the layout at scale 1, layout row y on target row
/// y, and each lane's middle column at layout x.
const FULL: (u32, u32) = (1_920, 1_080);
const FULL_KEY_1: u32 = 288;
const FULL_KEY_3: u32 = 416;
const FULL_KEY_4: u32 = 480;
const FULL_KEY_5: u32 = 544;
const FULL_KEY_7: u32 = 672;

/// Loads BMS `text` as a session does: the loader, then the transform and
/// the validation of its output.
fn loaded(text: &str) -> Chart {
    use game::ChartTransform;
    let dir = tempfile::tempdir().unwrap();
    let opts = bms_parser::LoadOptions::default();
    let chart = bms_parser::load_bms(text.as_bytes(), dir.path(), &opts)
        .unwrap()
        .chart;
    let chart = game::Passthrough.apply(chart, 0);
    chart.validate().unwrap();
    chart
}

/// Quarter-beat measures at 9,990,400 BPM: measure m starts at m ×
/// 1.5014416... µs, judged at the floored µs, so measures 1, 3, 5, 7 and 9
/// at 1, 4, 7, 10 and 13 µs, each about half a µs early. A tap on key 7
/// at measure 1, on key 5 at measure 3 and on key 3 at measure 7, and a
/// long note on key 1 from measure 5 to measure 9.
fn fractional() -> Chart {
    let mut text = String::from("#BPM 9990400\n#LNTYPE 1\n#WAV01 a.wav\n");
    for m in 0..=9 {
        text.push_str(&format!("#{m:03}02:0.0625\n"));
    }
    text.push_str("#00119:01\n#00315:01\n#00713:01\n#00551:01\n#00951:01\n");
    loaded(&text)
}

/// The middle rows of the runs of rows in column `x` of a [`FULL`] read
/// back whose RGB `keep` accepts, top to bottom.
fn runs(pixels: &[u8], x: u32, keep: impl Fn([u8; 3]) -> bool) -> Vec<f64> {
    let mut out = Vec::new();
    let mut start = None;
    for y in 0..=FULL.1 {
        let on = y < FULL.1 && keep(common::rgb_at(pixels, FULL.0, (x, y)));
        match (on, start) {
            (true, None) => start = Some(y),
            (false, Some(s)) => {
                out.push(f64::from(s + y) / 2.0);
                start = None;
            }
            _ => {}
        }
    }
    out
}

fn white(c: [u8; 3]) -> bool {
    c.iter().all(|&v| v > 200)
}

/// A bar line's grey, 0.5 opaque.
fn grey(c: [u8; 3]) -> bool {
    c.iter().all(|v| (100..=160).contains(v))
}

/// `got` holds `want`'s rows, each within a row.
#[track_caller]
fn assert_rows(got: &[f64], want: &[f64]) {
    assert_eq!(got.len(), want.len(), "rows {got:?}, expected {want:?}");
    for (g, w) in got.iter().zip(want) {
        assert!((g - w).abs() <= 1.0, "rows {got:?}, expected {want:?}");
    }
}

/// The default renderer meets every endpoint at its judgment µs: a frame
/// displayed at an endpoint's µs draws it centred on the line (row 880),
/// whatever the display offset, long-note ends and a practice round's
/// targets included, and the other notes and the bar lines at their
/// stored distances from it. With 10 µs visible a µs of scroll at the
/// start BPM is 80 px and a measure (a quarter beat) 120.115 px. The
/// sampled scroll map drew each endpoint at its µs about half a µs of
/// scroll short of its beat, 40 px above the line (rows 839.9, 839.7,
/// 839.4, 839.2 and 839.0 for measures 1, 3, 5, 7 and 9), and everything
/// else the same 40 px off.
#[test]
fn endpoints_meet_the_line_at_fractional_us() {
    let chart = fractional();
    let measure = 80.0 * 15e6 / 9_990_400.0;
    // Measure m's row when measure k is on the line.
    let y = |m: i32, k: i32| 880.0 - f64::from(m - k) * measure;
    let mut r = common::offscreen(FULL.0, FULL.1);
    r.load(&chart, None).unwrap();
    common::prepare(&mut r, &chart);
    let v = view(0, 0);
    let draw = |r: &mut Renderer, now: i64, offset: i64| {
        let f = render::FrameInput {
            visible_time: TimeUs(10),
            target: [FULL.0, FULL.1],
            ..input(&v, TimeUs(now), TimeUs(offset), &[])
        };
        let timing = r.frame(&f).unwrap();
        assert!(timing.presented);
        r.read_back()
    };
    let notes = |p: &[u8], x| runs(p, x, white);

    // Shown at 1 µs: measure 1's tap on the line, the long note's head
    // (its end above the lanes) and the other taps above it, and the bar
    // lines of measures 2-7 (measure 1's is under the judgment line, 0's
    // and 8's outside the lanes).
    let at_1 = draw(&mut r, 1, 0);
    assert_rows(&notes(&at_1, FULL_KEY_7), &[880.0]);
    assert_rows(&notes(&at_1, FULL_KEY_5), &[y(3, 1)]);
    assert_rows(&notes(&at_1, FULL_KEY_3), &[y(7, 1)]);
    assert_rows(&notes(&at_1, FULL_KEY_1), &[y(5, 1)]);
    let bars: Vec<f64> = (2..=7).rev().map(|m| y(m, 1)).collect();
    assert_rows(&runs(&at_1, FULL_KEY_4, grey), &bars);

    // Started at 1 µs with a 3 µs display offset, shown at 4 µs: measure
    // 3's tap on the line, the long note whole above it, and measure 1's
    // tap below the lanes.
    let offset = draw(&mut r, 1, 3);
    assert_rows(&notes(&offset, FULL_KEY_5), &[880.0]);
    assert_rows(&notes(&offset, FULL_KEY_1), &[y(9, 3), y(5, 3)]);
    assert_rows(&notes(&offset, FULL_KEY_3), &[y(7, 3)]);
    assert_rows(&notes(&offset, FULL_KEY_7), &[]);

    // The long note at its head's µs, its end 4 measures above, and at its
    // end's µs, both ends on the line.
    assert_rows(&notes(&draw(&mut r, 7, 0), FULL_KEY_1), &[y(9, 5), 880.0]);
    assert_rows(&notes(&draw(&mut r, 13, 0), FULL_KEY_1), &[880.0]);

    // A practice round from 9 µs to 12 µs: its one target, measure 7's
    // tap, on the line at its µs, the notes outside it not drawn.
    let section = eval::Section {
        start: TimeUs(9),
        end: TimeUs(12),
        lead_in: TimeUs(0),
        initial_gauges: Vec::new(),
    };
    common::prepare_section(&mut r, &chart, &section);
    let round = draw(&mut r, 10, 0);
    assert_rows(&notes(&round, FULL_KEY_3), &[880.0]);
    for x in [FULL_KEY_1, FULL_KEY_5, FULL_KEY_7] {
        assert_rows(&notes(&round, x), &[]);
    }
}

/// An attempt the simple skin refuses (a row past the playhead's domain)
/// is a refusal of the attempt, not of the screen: the previous attempt
/// is gone, frames go on being drawn and presented with no note, the
/// attempt's memory is the skin's own, and the next attempt draws again.
#[test]
fn a_refused_preparation_keeps_the_renderer_and_draws_no_attempt() {
    let chart = ChartBuilder::new(120_000)
        .tap(lane(7), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let past = ChartBuilder::new(120_000)
        .tap(lane(7), TimeUs(2_000 * MS))
        .tap(lane(5), TimeUs(bms_model::DOMAIN + 1))
        .build()
        .unwrap();
    let mut r = warp(&chart);
    assert_row(&draw(&mut r, 1_800 * MS, 0), LANE_7, 560.89);
    r.load(&past, None).unwrap();
    let ctx = common::context(&common::GAUGES, false);
    let refused = r
        .prepare_attempt(&common::attempt(&r, &past, &ctx))
        .unwrap_err();
    assert!(refused.is_refusal(), "{refused}");
    let render::RenderError::Skin(why) = &refused else {
        panic!("{refused:?}");
    };
    assert!(why.reason.contains("outside the playhead's range"), "{why}");
    assert_eq!(
        r.memory().attempt,
        size_of::<render::SimpleSkin>() as u64,
        "the refused attempt kept memory"
    );
    assert_eq!(
        white_rows(&draw(&mut r, 1_800 * MS, 0), LANE_7),
        Vec::<u32>::new()
    );
    r.load(&chart, None).unwrap();
    common::prepare(&mut r, &chart);
    assert_row(&draw(&mut r, 1_800 * MS, 0), LANE_7, 560.89);
}

#[test]
fn zero_size_target_is_one_pixel() {
    let mut r = Renderer::new(
        render::Target::Offscreen {
            width: 0,
            height: 0,
        },
        render::AdapterChoice::Warp,
    )
    .expect("WARP renderer");
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert!(timing.presented);
    assert_eq!(r.read_back().len(), 4);
}

#[test]
fn render_cpu_indicator() {
    // Render CPU work per frame on WARP (design.md "Latency Figures"), printed,
    // never compared: 400 frames at 60 Hz over a dense chart with BPM changes
    // and a STOP.
    const FRAMES: i64 = 400;
    let mut b = ChartBuilder::new(150_000)
        .bpm(TimeUs(2_000 * MS), 180_000)
        .stop(TimeUs(3_000 * MS), TimeUs(250 * MS));
    for i in 0..400 {
        let l = u8::try_from(i % 8).unwrap();
        b = b.tap(lane(l), TimeUs(300 * MS + i64::from(i) * 17 * MS));
    }
    let mut r = warp(&b.build().unwrap());
    let v = view(42, 1_234);
    let mut cpu: Vec<_> = (0..FRAMES)
        .map(|k| {
            let f = input(&v, TimeUs(k * 16_667), TimeUs(8 * MS), &[]);
            r.frame(&f).unwrap().cpu
        })
        .collect();
    assert_eq!(cpu.len(), FRAMES as usize);
    cpu.sort();
    let us = |q: usize| cpu[(cpu.len() - 1) * q / 100].as_secs_f64() * 1e6;
    println!(
        "render cpu per frame on WARP over {FRAMES} frames: p50 {:.0} us, p99 {:.0} us, max {:.0} us",
        us(50),
        us(99),
        us(100)
    );
}

/// Offscreen pacing on WARP (design.md "Headless runs"), printed, never
/// compared: frames submitted as fast as the loop goes, then the time a
/// read back waits for the GPU. The bound itself is the pacer's unit tests
/// (`pace.rs`); timings here vary with the machine and its load.
#[test]
fn offscreen_pacing_indicator() {
    const FRAMES: i64 = 60;
    let mut r = Renderer::new(
        render::Target::Offscreen {
            width: 1_920,
            height: 1_080,
        },
        render::AdapterChoice::Warp,
    )
    .expect("WARP renderer");
    let mut b = ChartBuilder::new(150_000);
    for i in 0..400 {
        let l = u8::try_from(i % 8).unwrap();
        b = b.tap(lane(l), TimeUs(i64::from(i) * 5 * MS));
    }
    let chart = b.build().unwrap();
    r.load(&chart, None).unwrap();
    common::prepare(&mut r, &chart);
    let v = view(42, 1_234);
    let (mut slowest, mut skipped) = (std::time::Duration::ZERO, 0);
    let began = std::time::Instant::now();
    for k in 0..FRAMES {
        let at = std::time::Instant::now();
        let timing = r
            .frame(&input(&v, TimeUs(k * 1_000), TimeUs(0), &[]))
            .unwrap();
        slowest = slowest.max(at.elapsed());
        skipped += usize::from(!timing.presented);
    }
    let frames = began.elapsed();
    let at = std::time::Instant::now();
    let _ = r.read_back();
    let read = at.elapsed();
    println!(
        "{FRAMES} frames ({skipped} skipped) in {} ms, the slowest {} ms; read back in {} ms",
        frames.as_millis(),
        slowest.as_millis(),
        read.as_millis()
    );
    assert!(!r.read_back().is_empty());
}

/// A 960×540 target: the layout at 1/2 scale, no letterbox, so layout pixel
/// (x, y) is target pixel (x/2, y/2).
const HALF_W: u32 = 960;
const HALF_H: u32 = 540;

/// The sprites `push` pushes, drawn on WARP into the half-scale target,
/// read back.
fn sprites_frame(push: impl FnOnce(&mut SpriteBatch)) -> Vec<u8> {
    let mut r = Renderer::new(
        render::Target::Offscreen {
            width: HALF_W,
            height: HALF_H,
        },
        render::AdapterChoice::Warp,
    )
    .expect("WARP renderer");
    let text = MenuText::new(&r);
    let timing = r.menu_frame(push, &text).unwrap();
    assert!(timing.presented);
    assert_eq!(timing.dropped, 0);
    r.read_back()
}

/// A solid sprite over `rect`, in layout pixels.
fn solid(rect: [f32; 4], color: [f32; 4]) -> Sprite {
    Sprite {
        rect,
        uv: render::solid_uv(),
        color,
        ..Sprite::default()
    }
}

/// The RGB of the target pixel under layout pixel (x, y).
fn rgb(pixels: &[u8], x: u32, y: u32) -> [u8; 3] {
    let i = ((y / 2 * HALF_W + x / 2) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

/// `got` is `want` within 2 per channel, the GPU's rounding.
#[track_caller]
fn assert_rgb(got: [u8; 3], want: [u8; 3]) {
    assert!(
        got.iter().zip(want).all(|(&g, w)| g.abs_diff(w) <= 2),
        "{got:?}, expected {want:?}"
    );
}

#[test]
fn alpha_pushed_after_additive_is_drawn_over_it() {
    // An opaque grey (0.2: 51), red added over it at alpha 0.6, and an
    // opaque blue alpha sprite pushed last over the right half. Where the
    // blue is not, 0.6 + 0.2 = 0.8 red: (204, 51, 51). Under it, blue
    // alone: (0, 0, 255). Drawn with every additive sprite after every
    // alpha one, the red would be added over the blue: (153, 0, 255).
    let pixels = sprites_frame(|b| {
        b.push(
            Blend::Alpha,
            solid([100.0, 100.0, 400.0, 200.0], [0.2, 0.2, 0.2, 1.0]),
        );
        b.push(
            Blend::Add,
            solid([100.0, 100.0, 400.0, 200.0], [1.0, 0.0, 0.0, 0.6]),
        );
        b.push(
            Blend::Alpha,
            solid([300.0, 100.0, 200.0, 200.0], [0.0, 0.0, 1.0, 1.0]),
        );
    });
    assert_rgb(rgb(&pixels, 200, 200), [204, 51, 51]);
    assert_rgb(rgb(&pixels, 400, 200), [0, 0, 255]);
}

#[test]
fn translucent_sprite_composites_its_colour_once() {
    // (1.0, 0.6, 0.2) at alpha 0.25, premultiplied (0.25, 0.15, 0.05), over
    // an opaque (0.0, 0.4, 1.0) background, stored as (0, 102, 255), and
    // over the cleared black target. Over the background: R 0.25 → 63.75,
    // G 0.15 + 0.4 × 0.75 = 0.45 → 114.75, B 0.05 + 0.75 = 0.8 → 204, so
    // (64, 115, 204). Over black: (64, 38, 13). Alpha applied twice would
    // darken it to (16, 86, 194) over the background; not at all, it would
    // be (255, 230, 242).
    let pixels = sprites_frame(|b| {
        b.push(
            Blend::Alpha,
            solid([100.0, 100.0, 200.0, 200.0], [0.0, 0.4, 1.0, 1.0]),
        );
        b.push(
            Blend::Alpha,
            solid([100.0, 100.0, 400.0, 200.0], [1.0, 0.6, 0.2, 0.25]),
        );
    });
    assert_rgb(rgb(&pixels, 200, 200), [64, 115, 204]);
    assert_rgb(rgb(&pixels, 400, 200), [64, 38, 13]);
}

#[test]
fn rotated_sprite_turns_clockwise_about_its_pivot() {
    // A white bar 200 × 40 from (600, 300), turned a quarter clockwise about
    // its top left corner: it hangs down from there, x 560..600, y
    // 300..500. Turned the other way it would stand up, x 600..640, y
    // 100..300.
    let pixels = sprites_frame(|b| {
        b.push(
            Blend::Alpha,
            Sprite {
                pivot: [600.0, 300.0],
                angle: std::f32::consts::FRAC_PI_2,
                ..solid([600.0, 300.0, 200.0, 40.0], [1.0; 4])
            },
        );
    });
    assert_rgb(rgb(&pixels, 580, 450), [255, 255, 255]);
    assert_rgb(rgb(&pixels, 700, 320), [0, 0, 0]);
    assert_rgb(rgb(&pixels, 620, 150), [0, 0, 0]);
}

#[test]
fn fading_beam_adds_its_colour_once() {
    // A press on lane 7 at 1.0 s lights its beam, (0.25, 0.3, 0.55) at
    // alpha 0.8, fading over 100 ms, added over the lane's background,
    // (0.04, 0.04, 0.05) stored as (10, 10, 13). Sampled at layout (672,
    // 300), screen (448, 352):
    // - 25 ms on, alpha 0.6, premultiplied (0.15, 0.18, 0.33): 38.25 + 10,
    //   45.9 + 10, 84.15 + 13, so (48, 56, 97);
    // - 75 ms on, alpha 0.2, (0.05, 0.06, 0.11): (23, 25, 41);
    // - 100 ms on, faded: the background, (10, 10, 13).
    // Alpha applied twice would give (33, 38, 63) at 25 ms; not at all,
    // (74, 87, 153).
    let chart = ChartBuilder::new(120_000)
        .tap(lane(1), TimeUs(60_000 * MS))
        .build()
        .unwrap();
    let mut r = warp(&chart);
    let v = view(0, 0);
    let press = JudgeEvent {
        seq: 0,
        time: TimeUs(1_000 * MS),
        origin: Some(TimeUs(1_000 * MS)),
        kind: JudgeEventKind::PlaySound {
            lane: lane(7),
            sound: SoundId(0),
        },
    };
    let mut at = |now_ms: i64, effects: &[JudgeEvent]| {
        r.frame(&input(&v, TimeUs(now_ms * MS), TimeUs(0), effects))
            .unwrap();
        let pixels = r.read_back();
        let i = ((352 * common::WIDTH + LANE_7) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };
    assert_rgb(at(1_025, &[press]), [48, 56, 97]);
    assert_rgb(at(1_075, &[]), [23, 25, 41]);
    assert_rgb(at(1_100, &[]), [10, 10, 13]);
}
