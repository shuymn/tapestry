//! The simple skin drawn on WARP and read back: notes sit where the scroll
//! map, the hi-speed visible time and the display latency offset put them.
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

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, input, lane, note_center, view, warp, white_rows};
use eval::{JudgeEvent, JudgeEventKind};
use render::Renderer;
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
fn note_is_drawn_where_scroll_map_says() {
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
