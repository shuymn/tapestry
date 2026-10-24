//! P1 test (f): OBS log evidence (V7), O1 and O2, plus their thresholds.
//!
//! The logs are synthetic but follow OBS's line shapes; P5 refreshes them
//! from a real (sanitized) log.

use spike_core::evaluate::{Summary, evaluate};
use spike_core::obslog::{WallWindow, derive};
use spike_core::run::{EncoderClass, RunData};
use spike_core::synth::passing_obs_run;

const H: i64 = 3_600_000;
/// Play runs 21:00:10.000 – 21:10:10.000 (600 s).
const PLAY: WallWindow = WallWindow {
    start_ms: 21 * H + 10_000,
    end_ms: 21 * H + 610_000,
};

fn ts(ms: i64) -> String {
    let ms = ms.rem_euclid(24 * H);
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / H,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

struct Log {
    begin_ms: i64,
    stop_ms: i64,
    encoder: &'static str,
    bitrate: u32,
    totals: Vec<String>,
    stop: bool,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            begin_ms: PLAY.start_ms - 10_000,
            stop_ms: PLAY.end_ms + 20_000,
            encoder: "x264",
            bitrate: 6000,
            totals: vec![
                "Output 'adv_stream': Total frames output: 37800".into(),
                "Output 'adv_stream': Total drawn frames: 37800".into(),
            ],
            stop: true,
        }
    }
}

impl Log {
    fn text(&self) -> String {
        let b = self.begin_ms;
        let mut l = vec![
            format!("{}: Start streaming clicked", ts(b - 5)),
            format!(
                "{}: [rtmp stream: 'adv_stream'] Connecting to RTMP URL rtmp://192.0.2.10/live...",
                ts(b)
            ),
            format!(
                "{}: [rtmp stream: 'adv_stream'] Connection to rtmp://192.0.2.10/live successful",
                ts(b + 40)
            ),
        ];
        let label = match self.encoder {
            "x264" => "x264 encoder",
            _ => "obs-nvenc",
        };
        l.push(format!(
            "{}: [{label}: 'advanced_video_stream'] settings:",
            ts(b + 60)
        ));
        for kv in [
            "rate_control: CBR".to_string(),
            format!("bitrate:      {}", self.bitrate),
            "buffer size:  6000".into(),
            "fps_num:      60".into(),
            "fps_den:      1".into(),
            "width:        1920".into(),
            "height:       1080".into(),
            "keyint:       120".into(),
        ] {
            l.push(format!("{}: \t{kv}", ts(b + 60)));
        }
        l.push(format!("{}: ", ts(b + 60)));
        l.push(format!(
            "{}: ==== Streaming Start ===============================================",
            ts(b + 100)
        ));
        l.push(format!(
            "{}: [game-capture: 'Game'] attempting to hook process",
            ts(b + 900)
        ));
        if self.stop {
            let e = self.stop_ms;
            l.push(format!(
                "{}: [rtmp stream: 'adv_stream'] User stopped the stream",
                ts(e - 20)
            ));
            l.push(format!("{}: Output 'adv_stream': stopping", ts(e - 10)));
            for t in &self.totals {
                l.push(format!("{}: {t}", ts(e - 10)));
            }
            l.push(format!(
                "{}: ==== Streaming Stop ================================================",
                ts(e)
            ));
        }
        l.join("\n") + "\n"
    }
}

fn with_log(log: &Log, class: EncoderClass) -> (RunData, Summary) {
    let mut run = passing_obs_run();
    run.info.obs_encoder = Some(class);
    run.obs = Some(derive(&log.text(), PLAY));
    let s = evaluate(&run);
    (run, s)
}

fn reasons(s: &Summary) -> Vec<String> {
    s.check("V7").reasons.clone()
}

#[test]
fn f_complete_log_without_lag_lines_is_valid_with_zero_lag() {
    let (run, s) = with_log(&Log::default(), EncoderClass::X264);
    assert!(s.check("V7").ok, "{:?}", reasons(&s));
    assert_eq!(s.gate("O1").value, Some(0.0));
    assert_eq!(s.gate("O2").value, Some(0.0));
    assert!(s.gate("O1").pass && s.gate("O2").pass);
    assert_eq!(s.exit_code, 0);
    let sess = run.obs.unwrap().session.unwrap();
    assert_eq!(
        (sess.pre_roll_ms, sess.post_roll_ms),
        (10_000, Some(20_000))
    );
    assert_eq!(s.records.obs_post_roll_ms, Some(20_000));
}

#[test]
fn f_render_lag_uses_the_attempted_denominator() {
    let log = Log {
        totals: vec![
            "Output 'adv_stream': Total frames output: 36000".into(),
            "Output 'adv_stream': Total drawn frames: 36000 (36300 attempted)".into(),
            "Output 'adv_stream': Number of lagged frames due to rendering lag/stalls: 300 (0.8%)"
                .into(),
        ],
        ..Log::default()
    };
    let (_, s) = with_log(&log, EncoderClass::X264);
    assert!(s.check("V7").ok);
    let o1 = s.gate("O1");
    assert_eq!(o1.samples, 36_300);
    assert_eq!(o1.value, Some(300.0 * 100.0 / 36_300.0));
    assert!(!o1.pass);
    assert_eq!(s.exit_code, 1);
    // Attempted count without a lagged line: lagged = attempted − drawn.
    let log = Log {
        totals: vec![
            "Output 'adv_stream': Total frames output: 36000".into(),
            "Output 'adv_stream': Total drawn frames: 36000 (36100 attempted)".into(),
        ],
        ..Log::default()
    };
    let (_, s) = with_log(&log, EncoderClass::X264);
    assert_eq!(s.gate("O1").value, Some(100.0 * 100.0 / 36_100.0));
}

#[test]
fn f_lagged_play_diluted_by_idle_post_roll_is_invalid_not_a_pass() {
    // 600 s of play with 0.8% lag (288 of 36,000), then 600 s idle with no
    // lag: diluted to 0.4% over the session, which would pass O1.
    let log = Log {
        stop_ms: PLAY.end_ms + 600_000,
        totals: vec![
            "Output 'adv_stream': Total frames output: 72000".into(),
            "Output 'adv_stream': Total drawn frames: 71712 (72000 attempted)".into(),
            "Output 'adv_stream': Number of lagged frames due to rendering lag/stalls: 288 (0.4%)"
                .into(),
        ],
        ..Log::default()
    };
    let (_, s) = with_log(&log, EncoderClass::X264);
    assert!(s.gate("O1").pass, "the diluted ratio alone would pass");
    assert_eq!(reasons(&s), vec!["obs-post-roll"]);
    assert_eq!(s.exit_code, 2);
}

#[test]
fn f_pre_roll_over_30_s_is_invalid() {
    let log = Log {
        begin_ms: PLAY.start_ms - 31_000,
        ..Log::default()
    };
    assert_eq!(
        reasons(&with_log(&log, EncoderClass::X264).1),
        vec!["obs-pre-roll"]
    );
    // Session starting after play starts does not bracket it.
    let log = Log {
        begin_ms: PLAY.start_ms + 1,
        ..Log::default()
    };
    assert_eq!(
        reasons(&with_log(&log, EncoderClass::X264).1),
        vec!["obs-pre-roll"]
    );
}

#[test]
fn f_missing_stop_block_is_invalid() {
    let log = Log {
        stop: false,
        ..Log::default()
    };
    let (_, s) = with_log(&log, EncoderClass::X264);
    let r = reasons(&s);
    assert!(r.contains(&"obs-session-incomplete".to_string()), "{r:?}");
    assert_eq!(s.exit_code, 2);
}

#[test]
fn f_no_session_is_invalid() {
    let mut run = passing_obs_run();
    run.obs = Some(derive(
        "21:00:00.000: OBS 31.0.0 (64-bit, windows)\n21:00:01.000: Loading up modules\n",
        PLAY,
    ));
    let s = evaluate(&run);
    assert_eq!(
        s.check("V7").reasons,
        vec!["no-obs-session-overlapping-play"]
    );
    run.obs = None;
    assert_eq!(evaluate(&run).exit_code, 2);
}

#[test]
fn f_non_overlapping_session_is_invalid() {
    let log = Log {
        begin_ms: PLAY.start_ms - 900_000,
        stop_ms: PLAY.start_ms - 300_000,
        ..Log::default()
    };
    let (run, s) = with_log(&log, EncoderClass::X264);
    assert_eq!(run.obs.unwrap().sessions_in_log, 1);
    assert_eq!(reasons(&s), vec!["no-obs-session-overlapping-play"]);
}

#[test]
fn f_settings_mismatch_is_invalid() {
    let log = Log {
        bitrate: 4500,
        ..Log::default()
    };
    assert_eq!(
        reasons(&with_log(&log, EncoderClass::X264).1),
        vec!["obs-settings-mismatch"]
    );
    // Hardware encoder log in the x264 cell, and the reverse.
    let hw = Log {
        encoder: "nvenc",
        ..Log::default()
    };
    assert_eq!(
        reasons(&with_log(&hw, EncoderClass::X264).1),
        vec!["obs-settings-mismatch"]
    );
    assert!(with_log(&hw, EncoderClass::Hardware).1.check("V7").ok);
    assert_eq!(
        reasons(&with_log(&Log::default(), EncoderClass::Hardware).1),
        vec!["obs-settings-mismatch"]
    );
}

#[test]
fn f_bandwidth_drop_is_invalid() {
    let mut log = Log::default();
    log.totals.push(
        "Output 'adv_stream': Number of dropped frames due to insufficient bandwidth/connection stalls: 3 (0.0%)"
            .into(),
    );
    assert_eq!(
        reasons(&with_log(&log, EncoderClass::X264).1),
        vec!["obs-bandwidth-drop"]
    );
}

#[test]
fn f_owner_must_confirm_capture() {
    let mut run = passing_obs_run();
    run.obs = Some(derive(&Log::default().text(), PLAY));
    run.owner.audio_present = None;
    assert_eq!(
        evaluate(&run).check("V7").reasons,
        vec!["owner-did-not-confirm-capture"]
    );
}

#[test]
fn f_session_across_midnight() {
    let play = WallWindow {
        start_ms: 23 * H + 59 * 60_000 + 30_000,
        end_ms: 24 * H + 9 * 60_000 + 30_000,
    };
    let log = Log {
        begin_ms: play.start_ms - 5_000,
        stop_ms: play.end_ms + 5_000,
        ..Log::default()
    };
    let d = derive(&log.text(), play);
    let s = d.session.unwrap();
    assert_eq!((s.pre_roll_ms, s.post_roll_ms), (5_000, Some(5_000)));
}

// (b) for O1 and O2: below 0.5% passes, 0.5% exactly fails.
#[test]
fn b_o1_o2_thresholds() {
    let o = |drawn: u64, lagged: u64, skipped: u64| {
        let mut log = Log {
            totals: vec![
                "Output 'adv_stream': Total frames output: 36000".into(),
                format!("Output 'adv_stream': Total drawn frames: {drawn} (36000 attempted)"),
                format!(
                    "Output 'adv_stream': Number of lagged frames due to rendering lag/stalls: {lagged} (0.5%)"
                ),
            ],
            ..Log::default()
        };
        log.totals.push(format!(
            "Video stopped, number of skipped frames due to encoding lag: {skipped}/36000 (0.5%)"
        ));
        let (_, s) = with_log(&log, EncoderClass::X264);
        (s.gate("O1").pass, s.gate("O2").pass)
    };
    assert_eq!(o(36000 - 179, 179, 179), (true, true));
    assert_eq!(o(36000 - 180, 180, 179), (false, true));
    assert_eq!(o(36000 - 179, 179, 180), (true, false));
}
