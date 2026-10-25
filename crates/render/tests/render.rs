//! The simple skin drawn on WARP and read back: notes sit where the scroll
//! map, the hi-speed visible time and the display latency offset put them.
//!
//! Expected rows are worked out by hand from the layout: the lane runs from
//! y = 80 to the judgment line at y = 880 (800 px), a note is 16 px tall
//! and centered on its position, and the visible time is 600 ms. The
//! 1280×1024 target shows the 1920×1080 layout at 2/3 scale with 152 black
//! rows on top, so a layout row y lands on screen row 152 + 2y/3. Lane 7
//! spans x = 640..704 (screen column 448 at its middle), lane 5 x = 512..576
//! (screen column 362).

mod common;

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, input, lane, note_center, view, warp, white_rows};
use eval::{JudgeEvent, JudgeEventKind};
use rules::Judgment;

const LANE_7: u32 = 448;
const LANE_5: u32 = 362;

#[track_caller]
fn assert_row(pixels: &[u8], x: u32, expected: f64) {
    let got = note_center(pixels, x);
    assert!(
        (got - expected).abs() <= 1.0,
        "note at row {got}, expected {expected}"
    );
}

#[test]
fn note_is_drawn_where_scroll_map_says() {
    // 120 BPM, then 240 BPM from 1.5 s; one note at 2.0 s. Positions in
    // beats: 2 per second up to 1.5 s (3 beats), 4 per second after, so the
    // note is at beat 5. At 240 BPM the lane holds 600 ms = 2.4 beats.
    // The display offset is 20 ms, so a frame started at `now` shows
    // `now + 20 ms`:
    // - shown at 1.60 s (beat 3.4): 1.6 beats ahead, y = 880 - 1.6 / 2.4 ×
    //   800 = 346.67, screen row 383.11;
    // - shown at 1.85 s (beat 4.4): y = 880 - 200 = 680, screen 605.33;
    // - shown at 2.00 s: on the line, y = 880, screen 738.67.
    // Without the offset these would be rows 365.33, 587.56 and 720.89,
    // and at the initial BPM's scale the first would be above the lane.
    let chart = ChartBuilder::new(120_000)
        .bpm(TimeUs(1_500 * MS), 240_000)
        .tap(lane(7), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let mut r = warp(&chart);
    let v = view(0, 0);
    let offset = TimeUs(20 * MS);
    let mut checked = 0;
    for (now, row) in [(1_580, 383.11), (1_830, 605.33), (1_980, 738.67)] {
        let timing = r.frame(&input(&v, TimeUs(now * MS), offset, &[]));
        assert!(timing.presented);
        assert_eq!(timing.dropped, 0);
        assert_row(&r.read_back(), LANE_7, row);
        checked += 1;
    }
    assert_eq!(checked, 3);
}

#[test]
fn stop_freezes_notes() {
    // 120 BPM with a STOP from 1.0 s to 1.5 s; a note at 1.6 s, beat 2.2.
    // The lane holds 1.2 beats (600 ms at the BPM the STOP resumes at).
    // During the STOP the scroll stays at beat 2: 0.2 beats ahead, y =
    // 880 - 133.33 = 746.67, screen 649.78. Before it, at 0.95 s (beat
    // 1.9), y = 680, screen 605.33; after it, at 1.55 s (beat 2.1), y =
    // 813.33, screen 694.22.
    let chart = ChartBuilder::new(120_000)
        .stop(TimeUs(1_000 * MS), TimeUs(500 * MS))
        .tap(lane(7), TimeUs(1_600 * MS))
        .build()
        .unwrap();
    let mut r = warp(&chart);
    let v = view(0, 0);
    let cases = [
        (950, 605.33),
        (1_100, 649.78),
        (1_250, 649.78),
        (1_450, 649.78),
        (1_550, 694.22),
    ];
    let mut frozen = Vec::new();
    for (now, row) in cases {
        r.frame(&input(&v, TimeUs(now * MS), TimeUs(0), &[]));
        let pixels = r.read_back();
        assert_row(&pixels, LANE_7, row);
        if (1_000..=1_500).contains(&now) {
            frozen.push(white_rows(&pixels, LANE_7));
        }
    }
    assert_eq!(frozen.len(), 3);
    assert!(!frozen[0].is_empty());
    assert!(frozen.iter().all(|rows| *rows == frozen[0]));
}

#[test]
fn judged_note_disappears() {
    // 120 BPM; notes at 2.0 s in lanes 5 and 7, shown at 1.8 s: 0.4 beats
    // ahead of a 1.2-beat lane, y = 613.33, screen 560.89.
    let chart = ChartBuilder::new(120_000)
        .tap(lane(5), TimeUs(2_000 * MS))
        .tap(lane(7), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let mut r = warp(&chart);
    let v = view(1, 2);
    let now = TimeUs(1_800 * MS);
    r.frame(&input(&v, now, TimeUs(0), &[]));
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
            combo: 1,
        },
    };
    r.frame(&input(&v, now, TimeUs(0), &[judged]));
    let after = r.read_back();
    assert_row(&after, LANE_5, 560.89);
    assert_eq!(white_rows(&after, LANE_7), Vec::<u32>::new());
    // The bit stays set on later frames with no new effect.
    r.frame(&input(&v, TimeUs(1_850 * MS), TimeUs(0), &[]));
    let later = r.read_back();
    assert!(!white_rows(&later, LANE_5).is_empty());
    assert_eq!(white_rows(&later, LANE_7), Vec::<u32>::new());
}

#[test]
fn zero_size_target_is_one_pixel() {
    let mut r = render::Renderer::new(
        render::Target::Offscreen {
            width: 0,
            height: 0,
        },
        render::AdapterChoice::Warp,
    )
    .expect("WARP renderer");
    let v = view(0, 0);
    assert!(r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).presented);
    assert_eq!(r.read_back().len(), 4);
}
