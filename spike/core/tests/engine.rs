//! The audio thread's signal path end to end, without a device: the chart's
//! judged presses trigger key sounds through the engine (mixer, drift
//! resampler, clamp, pilot, injected gaps), and the pilot detector reads
//! the output as the loopback capture would.

use spike_core::chart::{Chart, DEFAULT_SEED};
use spike_core::engine::{Engine, Injection};
use spike_core::pilot::{Detector, DetectorReport, calibrate};
use spike_core::play::Play;
use spike_core::sounds::key_bank;

const RATE: u32 = 48_000;
const PERIOD: usize = 480;
const LEAD_S: u64 = 2;

/// `play_s` of play after a 2 s pilot-only lead-in, rendered in device
/// periods; sounds are triggered a period ahead at their note time.
/// Returns the left channel, the engine and the play's judgment count.
fn render(play_s: u64, injection: Option<Injection>) -> (Vec<f32>, Engine, u32) {
    let chart = Chart::generate(DEFAULT_SEED);
    let mut play = Play::new(&chart, play_s as i64 * 1_000_000);
    let mut engine = Engine::new(RATE, key_bank(11, RATE), injection);
    let lead = LEAD_S * u64::from(RATE);
    let total = lead + (play_s + 1) * u64::from(RATE);
    let mut out = vec![0.0f32; PERIOD * 2];
    let mut left = Vec::with_capacity(total as usize);
    let mut judged = 0;
    while (engine.device_frames()) < total {
        let now = engine.device_frames();
        let ahead_us =
            ((now + 2 * PERIOD as u64).saturating_sub(lead) * 1_000_000 / u64::from(RATE)) as i64;
        if now + 2 * PERIOD as u64 >= lead {
            play.advance_to(ahead_us, |j| {
                judged += 1;
                let at = lead + (j.note_us.max(0) as u64 * u64::from(RATE)) / 1_000_000;
                engine.trigger_at_device(j.sound, at);
            });
        }
        engine.render(&mut out, 2, PERIOD);
        left.extend(out.iter().step_by(2));
    }
    (left, engine, judged)
}

fn detect(left: &[f32]) -> DetectorReport {
    let cal = calibrate(&left[..RATE as usize], RATE).expect("calibrates");
    let mut det = Detector::new(RATE, cal);
    for c in left.chunks(PERIOD) {
        det.push(c);
    }
    det.finish()
}

#[test]
fn engine_output_has_no_episodes_and_no_clamping() {
    // H1 (40–70 s) is the densest part: 60 s covers it.
    let (left, engine, judged) = render(60, None);
    let report = detect(&left);
    assert_eq!(report.episodes, vec![], "steals {}", engine.mixer().steals);
    assert_eq!(engine.clamped, 0);
    assert!(engine.mixer().steals > 0, "the cap is reached");
    assert!(judged > 5_000, "{judged}");
}

#[test]
fn injected_gaps_are_each_one_episode_at_their_frame() {
    let first = LEAD_S * u64::from(RATE) + 5 * u64::from(RATE);
    let inj = Injection {
        first,
        every: 10 * u64::from(RATE),
        len: 2 * u64::from(RATE) / 1000,
        count: 6,
    };
    let (left, _, _) = render(60, Some(inj));
    let report = detect(&left);
    let window = 48;
    let starts: Vec<u64> = report.episodes.iter().map(|e| e.start_sample).collect();
    assert_eq!(starts.len(), 6, "{:?}", report.episodes);
    for (k, s) in starts.iter().enumerate() {
        let want = inj.start(k as u64);
        assert!(
            s.abs_diff(want) <= window,
            "gap {k}: episode at {s}, injected at {want}"
        );
    }
}
