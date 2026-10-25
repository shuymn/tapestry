//! P1 test (f): OBS log evidence (V7), O1 and O2, plus their thresholds.
//!
//! The logs are synthetic but follow the line shapes in OBS's sources
//! (libobs `obs_reset_video`, `log_frame_info`, `log_skipped`; the x264,
//! obs-nvenc, texture-amf and qsv encoder plugins); P5 refreshes them from a
//! real (sanitized) log.

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

#[derive(Clone, Copy, PartialEq)]
enum Enc {
    X264,
    Nvenc,
    Amf,
    Qsv,
}

fn tab(kvs: &[(&str, &str)], width: usize) -> Vec<String> {
    kvs.iter()
        .map(|(k, v)| format!("\t{:<width$}{v}", format!("{k}:")))
        .collect()
}

impl Enc {
    fn class(self) -> EncoderClass {
        match self {
            Enc::X264 => EncoderClass::X264,
            _ => EncoderClass::Hardware,
        }
    }

    /// Lines as each plugin logs them at encoder creation, with the
    /// procedure's settings (§4.1).
    fn lines(self) -> Vec<String> {
        let name = "advanced_video_stream";
        match self {
            Enc::X264 => {
                let p = format!("[x264 encoder: '{name}']");
                let mut l = vec![
                    "---------------------------------".to_string(),
                    format!("{p} preset: veryfast"),
                    format!("{p} profile: high"),
                    format!("{p} tune: (null)"),
                    format!("{p} settings:"),
                ];
                l.extend(tab(
                    &[
                        ("rate_control", "CBR"),
                        ("bitrate", "6000"),
                        ("buffer size", "6000"),
                        ("crf", "23"),
                        ("fps_num", "60"),
                        ("fps_den", "1"),
                        ("width", "1920"),
                        ("height", "1080"),
                        ("keyint", "120"),
                    ],
                    14,
                ));
                l.push(String::new());
                l
            }
            Enc::Nvenc => {
                let mut l = vec![format!("[obs-nvenc: '{name}'] settings:")];
                l.extend(tab(
                    &[
                        ("codec", "H264"),
                        ("rate_control", "CBR"),
                        ("bitrate", "6000"),
                        ("keyint", "120"),
                        ("preset", "p5"),
                        ("tuning", "hq"),
                        ("multipass", "qres"),
                        ("profile", "high"),
                        ("width", "1920"),
                        ("height", "1080"),
                        ("b-frames", "2"),
                        ("b-ref-mode", "0"),
                        ("lookahead", "false (0 frames)"),
                        ("aq", "true"),
                    ],
                    14,
                ));
                l
            }
            Enc::Amf => {
                // AMF logs no fps; the common video block carries it.
                let mut l = vec![format!("[texture-amf-h264: '{name}'] settings:")];
                l.extend(tab(
                    &[
                        ("rate_control", "CBR"),
                        ("bitrate", "6000"),
                        ("cqp", "20"),
                        ("keyint", "120"),
                        ("preset", "quality"),
                        ("profile", "high"),
                        ("level", "auto"),
                        ("b-frames", "0"),
                        ("width", "1920"),
                        ("height", "1080"),
                        ("pre-analysis", "off"),
                        ("params", ""),
                    ],
                    14,
                ));
                l
            }
            Enc::Qsv => {
                let p = format!("[qsv encoder: '{name}']");
                let mut l = vec![format!("{p} settings:")];
                // `codec` and `rate_control` come with the prefixed call;
                // the rest are separate unprefixed blog lines.
                l.extend(tab(&[("codec", "H.264"), ("rate_control", "CBR")], 16));
                l.extend(tab(&[("target_bitrate", "6000")], 16));
                l.extend(tab(
                    &[
                        ("target_usage", "TU4"),
                        ("profile", "high"),
                        ("keyint", "2"),
                        ("latency", "normal"),
                        ("b-frames", "3"),
                        ("fps_num", "60"),
                        ("fps_den", "1"),
                        ("width", "1920"),
                        ("height", "1080"),
                    ],
                    16,
                ));
                l.push(format!("{p} debug info:"));
                l
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Shutdown {
    /// OBS still running when the log was taken.
    No,
    /// Separator, then the final `Number of memory leaks:` line.
    AfterStop,
    /// Separator before the stream's stop block; final line at the end.
    BeforeStop,
    /// Separator after the stop, log cut before the final line.
    Truncated,
}

struct Log {
    attempt_ms: i64,
    connect_ms: i64,
    start_ms: i64,
    stop_ms: i64,
    enc: Enc,
    /// `(from, to)` substitutions applied to the encoder and video lines.
    edits: Vec<(&'static str, &'static str)>,
    totals: Vec<String>,
    /// Lines logged shortly after the streaming start marker.
    during: Vec<String>,
    stop: bool,
    /// Lines logged after the stream's stop marker, 1 s apart.
    after: Vec<String>,
    /// How the log ends.
    shutdown: Shutdown,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            attempt_ms: PLAY.start_ms - 10_200,
            connect_ms: PLAY.start_ms - 10_000,
            start_ms: PLAY.start_ms - 9_900,
            stop_ms: PLAY.end_ms + 20_000,
            enc: Enc::X264,
            edits: vec![],
            totals: vec![
                "Output 'adv_stream': Total frames output: 37800".into(),
                "Output 'adv_stream': Total drawn frames: 37800".into(),
            ],
            during: vec![],
            stop: true,
            after: vec![],
            shutdown: Shutdown::AfterStop,
        }
    }
}

impl Log {
    fn text(&self) -> String {
        let at = |t: i64, m: &str| format!("{}: {m}", ts(t));
        let edit = |s: String| self.edits.iter().fold(s, |s, (a, b)| s.replace(a, b));
        let t0 = self.attempt_ms - 60_000;
        let mut l = vec![
            at(t0, "OBS 31.0.0 (64-bit, windows)"),
            at(t0, "---------------------------------"),
        ];
        for v in [
            "video settings reset:",
            "\tbase resolution:   2560x1440",
            "\toutput resolution: 1920x1080",
            "\tdownscale filter:  Bicubic",
            "\tfps:               60/1",
            "\tformat:            NV12",
            "\tYUV mode:          709/Partial",
        ] {
            l.push(at(t0 + 5, &edit(v.to_string())));
        }
        let rtmp = "[rtmp stream: 'adv_stream']";
        l.push(at(
            self.attempt_ms,
            &format!("{rtmp} Connecting to RTMP URL rtmp://192.0.2.10/live..."),
        ));
        l.push(at(
            self.connect_ms,
            &format!("{rtmp} Connection to rtmp://192.0.2.10/live successful"),
        ));
        for e in self.enc.lines() {
            l.push(at(self.connect_ms + 20, &edit(e)));
        }
        l.push(at(
            self.start_ms,
            "==== Streaming Start ===============================================",
        ));
        for d in &self.during {
            l.push(at(self.start_ms + 500, d));
        }
        l.push(at(
            self.start_ms + 900,
            "[game-capture: 'Game'] attempting to hook process",
        ));
        let separator = "==== Shutting down ==================================================";
        if self.shutdown == Shutdown::BeforeStop {
            // closeEvent prints the separator, then drains posted events,
            // which can still log the stream's stop.
            l.push(at(self.stop_ms - 100, separator));
        }
        if self.stop {
            let e = self.stop_ms;
            l.push(at(e - 20, &format!("{rtmp} User stopped the stream")));
            l.push(at(e - 10, "Output 'adv_stream': stopping"));
            for t in &self.totals {
                l.push(at(e - 10, t));
            }
            l.push(at(
                e,
                "==== Streaming Stop ================================================",
            ));
            for (k, a) in (1..).zip(&self.after) {
                l.push(at(e + k * 1_000, a));
            }
        }
        let t = self.stop_ms + 300_000;
        if matches!(self.shutdown, Shutdown::AfterStop | Shutdown::Truncated) {
            l.push(at(t, separator));
        }
        if matches!(self.shutdown, Shutdown::AfterStop | Shutdown::BeforeStop) {
            l.push(at(t + 100, "Number of memory leaks: 0"));
        }
        l.join("\n") + "\n"
    }
}

fn with_log(log: &Log) -> (RunData, Summary) {
    let mut run = passing_obs_run();
    run.info.obs_encoder = Some(log.enc.class());
    run.obs = Some(derive(&log.text(), PLAY));
    let s = evaluate(&run);
    (run, s)
}

fn reasons(log: &Log) -> Vec<String> {
    with_log(log).1.check("V7").reasons.clone()
}

#[test]
fn f_every_encoder_fixture_is_valid_with_zero_lag() {
    for enc in [Enc::X264, Enc::Nvenc, Enc::Amf, Enc::Qsv] {
        let log = Log {
            enc,
            ..Log::default()
        };
        let (run, s) = with_log(&log);
        assert!(s.check("V7").ok, "{:?}", s.check("V7").reasons);
        assert_eq!(s.gate("O1").value, Some(0.0));
        assert_eq!(s.gate("O2").value, Some(0.0));
        assert_eq!(s.exit_code, 0);
        let sess = run.obs.unwrap().session.unwrap();
        assert_eq!(
            (sess.connect_lead_ms, sess.start_lead_ms, sess.post_roll_ms),
            (Some(10_000), Some(9_900), Some(20_000))
        );
    }
}

#[test]
fn f_encoder_workload_mismatch_is_invalid() {
    let cases: [(Enc, &str, &str); 10] = [
        (Enc::X264, "preset: veryfast", "preset: medium"),
        (Enc::X264, "profile: high", "profile: main"),
        (Enc::X264, "tune: (null)", "tune: zerolatency"),
        (Enc::X264, "6000", "4500"),
        (Enc::Nvenc, "p5", "p4"),
        (Enc::Nvenc, "qres", "fullres"),
        (Enc::Nvenc, "H264", "HEVC"),
        (Enc::Amf, "quality", "speed"),
        (Enc::Qsv, "TU4", "TU7"),
        (Enc::Qsv, "CBR", "VBR"),
    ];
    for (enc, from, to) in cases {
        let log = Log {
            enc,
            edits: vec![(from, to)],
            ..Log::default()
        };
        assert_eq!(
            reasons(&log),
            vec!["obs-settings-mismatch"],
            "{from} -> {to}"
        );
    }
    // Hardware log in the x264 cell, and the reverse.
    let mut run = passing_obs_run();
    let nvenc = Log {
        enc: Enc::Nvenc,
        ..Log::default()
    };
    run.obs = Some(derive(&nvenc.text(), PLAY));
    assert_eq!(
        evaluate(&run).check("V7").reasons,
        vec!["obs-settings-mismatch"]
    );
    run.info.obs_encoder = Some(EncoderClass::Hardware);
    run.obs = Some(derive(&Log::default().text(), PLAY));
    assert_eq!(
        evaluate(&run).check("V7").reasons,
        vec!["obs-settings-mismatch"]
    );
}

#[test]
fn f_encoder_scaling_below_the_output_size_is_invalid() {
    // The canvas output stays 1080p but the encoder rescales to 720p.
    for enc in [Enc::X264, Enc::Nvenc, Enc::Amf, Enc::Qsv] {
        let log = Log {
            enc,
            edits: vec![("1920", "1280"), ("1080", "720")],
            ..Log::default()
        };
        // Restore the common video block: only the encoder lines say 720p.
        let text = log.text().replace(
            "output resolution: 1280x720",
            "output resolution: 1920x1080",
        );
        let mut run = passing_obs_run();
        run.info.obs_encoder = Some(enc.class());
        run.obs = Some(derive(&text, PLAY));
        assert_eq!(
            evaluate(&run).check("V7").reasons,
            vec!["obs-settings-mismatch"]
        );
    }
}

#[test]
fn f_common_video_settings_come_from_the_video_block() {
    for (from, to) in [
        ("fps:               60/1", "fps:               30/1"),
        (
            "output resolution: 1920x1080",
            "output resolution: 1280x720",
        ),
        ("Bicubic", "Bilinear"),
    ] {
        for enc in [Enc::Amf, Enc::X264] {
            let log = Log {
                enc,
                edits: vec![(from, to)],
                ..Log::default()
            };
            assert_eq!(reasons(&log), vec!["obs-settings-mismatch"], "{to}");
        }
    }
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
    let (_, s) = with_log(&log);
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
    assert_eq!(
        with_log(&log).1.gate("O1").value,
        Some(100.0 * 100.0 / 36_100.0)
    );
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
    let (_, s) = with_log(&log);
    assert!(s.gate("O1").pass, "the diluted ratio alone would pass");
    assert_eq!(s.check("V7").reasons, vec!["obs-post-roll"]);
    assert_eq!(s.exit_code, 2);
}

#[test]
fn f_coverage_starts_at_the_connection_and_streaming_start() {
    // A slow connect: the attempt is 40 s before play but the connection
    // and the streaming start are within 30 s.
    let log = Log {
        attempt_ms: PLAY.start_ms - 40_000,
        connect_ms: PLAY.start_ms - 25_000,
        start_ms: PLAY.start_ms - 24_900,
        ..Log::default()
    };
    assert!(reasons(&log).is_empty(), "{:?}", reasons(&log));
    // Connected 31 s before play: too much pre-roll.
    let log = Log {
        attempt_ms: PLAY.start_ms - 31_200,
        connect_ms: PLAY.start_ms - 31_000,
        start_ms: PLAY.start_ms - 29_000,
        ..Log::default()
    };
    assert_eq!(reasons(&log), vec!["obs-pre-roll"]);
    // Connected, or streaming started, after play started.
    let log = Log {
        attempt_ms: PLAY.start_ms - 1_000,
        connect_ms: PLAY.start_ms + 500,
        start_ms: PLAY.start_ms + 600,
        ..Log::default()
    };
    assert_eq!(reasons(&log), vec!["obs-pre-roll"]);
    let log = Log {
        start_ms: PLAY.start_ms + 1,
        ..Log::default()
    };
    assert_eq!(reasons(&log), vec!["obs-pre-roll"]);
}

#[test]
fn f_recording_during_the_session_is_invalid_and_not_mixed_in() {
    // A recording ran through the session: its 90,000 frames (none lagged)
    // must not dilute the stream's 0.8% lag.
    let log = Log {
        during: vec!["==== Recording Start ===============================================".into()],
        totals: vec![
            "Output 'adv_stream': Total frames output: 36000".into(),
            "Output 'adv_stream': Total drawn frames: 36000 (36290 attempted)".into(),
            "Output 'adv_stream': Number of lagged frames due to rendering lag/stalls: 290 (0.8%)"
                .into(),
            "Output 'adv_file_output': stopping".into(),
            "Output 'adv_file_output': Total frames output: 90000".into(),
            "Output 'adv_file_output': Total drawn frames: 90000".into(),
            "==== Recording Stop ================================================".into(),
        ],
        ..Log::default()
    };
    let (run, s) = with_log(&log);
    let sess = run.obs.unwrap().session.unwrap();
    assert_eq!(
        (sess.frames_output, sess.drawn_attempted),
        (Some(36_000), Some(36_290))
    );
    assert_eq!(s.check("V7").reasons, vec!["obs-other-output-active"]);
    assert!(!s.gate("O1").pass);
    // Counters of another output alone also reveal it.
    let log = Log {
        totals: vec![
            "Output 'adv_file_output': Total frames output: 90000".into(),
            "Output 'adv_stream': Total frames output: 37800".into(),
            "Output 'adv_stream': Total drawn frames: 37800".into(),
        ],
        ..Log::default()
    };
    assert_eq!(reasons(&log), vec!["obs-other-output-active"]);
}

const STREAM_LAG: &str =
    "Video stopped, number of skipped frames due to encoding lag: 300/36000 (0.8%)";
const RECORDING_LAG: &str =
    "Video stopped, number of skipped frames due to encoding lag: 1/36000 (0.0%)";

/// A recording made after the stream stopped.
fn recording_after(lag: Option<&str>) -> Vec<String> {
    let mut l = vec![
        "==== Recording Start ===============================================".to_string(),
        "Output 'adv_file_output': stopping".to_string(),
        "Output 'adv_file_output': Total frames output: 36000".to_string(),
    ];
    l.extend(lag.map(String::from));
    l.push("==== Recording Stop ================================================".to_string());
    l
}

#[test]
fn f_stream_lag_summary_after_its_stop_marker_is_the_streams() {
    // libobs logs the summary from an unjoined teardown thread, so it can
    // follow the frontend's stop marker.
    let log = Log {
        after: vec![STREAM_LAG.into()],
        ..Log::default()
    };
    let (run, s) = with_log(&log);
    let sess = run.obs.unwrap().session.unwrap();
    assert_eq!(
        (sess.encode_skipped, sess.encode_total),
        (Some(300), Some(36_000))
    );
    assert!(s.check("V7").ok, "{:?}", s.check("V7").reasons);
    assert!(!s.gate("O2").pass);
    assert_eq!(s.exit_code, 1);
}

#[test]
fn f_stream_lag_summary_after_the_shutdown_separator_is_the_streams() {
    // Separator, stop marker, the stream's summary, then the final line.
    let log = Log {
        shutdown: Shutdown::BeforeStop,
        after: vec![STREAM_LAG.into()],
        ..Log::default()
    };
    let (run, s) = with_log(&log);
    let sess = run.obs.unwrap().session.unwrap();
    assert_eq!(
        (sess.encode_skipped, sess.encode_total),
        (Some(300), Some(36_000))
    );
    assert!(s.check("V7").ok, "{:?}", s.check("V7").reasons);
    assert!(!s.gate("O2").pass);
}

#[test]
fn f_later_recording_lag_does_not_replace_the_streams() {
    let mut after = vec![STREAM_LAG.to_string()];
    after.extend(recording_after(Some(RECORDING_LAG)));
    let log = Log {
        after,
        ..Log::default()
    };
    let (run, s) = with_log(&log);
    let sess = run.obs.unwrap().session.unwrap();
    assert_eq!(
        (sess.encode_skipped, sess.encode_total),
        (Some(300), Some(36_000))
    );
    assert!(!sess.other_output_active);
    assert!(s.check("V7").ok);
    assert!(!s.gate("O2").pass);
}

#[test]
fn f_lag_summary_after_another_output_started_is_ambiguous() {
    // The stream logged none before the recording started; the recording's
    // summary could be the stream's late one.
    let log = Log {
        after: recording_after(Some(RECORDING_LAG)),
        ..Log::default()
    };
    assert_eq!(reasons(&log), vec!["obs-encode-lag-ambiguous"]);
}

#[test]
fn f_no_lag_summary_counts_as_zero_only_after_obs_shut_down() {
    for shutdown in [Shutdown::No, Shutdown::Truncated] {
        let log = Log {
            shutdown,
            ..Log::default()
        };
        assert_eq!(reasons(&log), vec!["obs-teardown-unconfirmed"]);
    }
    // The separator before or after the stop marker, then the final line.
    for shutdown in [Shutdown::BeforeStop, Shutdown::AfterStop] {
        let log = Log {
            shutdown,
            ..Log::default()
        };
        let (_, s) = with_log(&log);
        assert!(s.check("V7").ok, "{:?}", s.check("V7").reasons);
        assert_eq!(s.gate("O2").value, Some(0.0));
    }
    // A later recording without any summary, then OBS exits: zero.
    let log = Log {
        after: recording_after(None),
        ..Log::default()
    };
    let (_, s) = with_log(&log);
    assert!(s.check("V7").ok, "{:?}", s.check("V7").reasons);
    assert_eq!(s.gate("O2").value, Some(0.0));
}

#[test]
fn f_missing_stop_block_is_invalid() {
    let log = Log {
        stop: false,
        ..Log::default()
    };
    let (_, s) = with_log(&log);
    assert!(
        s.check("V7")
            .reasons
            .contains(&"obs-session-incomplete".to_string()),
        "{:?}",
        s.check("V7").reasons
    );
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
        attempt_ms: PLAY.start_ms - 900_200,
        connect_ms: PLAY.start_ms - 900_000,
        start_ms: PLAY.start_ms - 899_900,
        stop_ms: PLAY.start_ms - 300_000,
        ..Log::default()
    };
    let (run, s) = with_log(&log);
    assert_eq!(run.obs.unwrap().sessions_in_log, 1);
    assert_eq!(
        s.check("V7").reasons,
        vec!["no-obs-session-overlapping-play"]
    );
}

#[test]
fn f_bandwidth_drop_is_invalid() {
    let mut log = Log::default();
    log.totals.push(
        "Output 'adv_stream': Number of dropped frames due to insufficient bandwidth/connection stalls: 3 (0.0%)"
            .into(),
    );
    assert_eq!(reasons(&log), vec!["obs-bandwidth-drop"]);
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
        attempt_ms: play.start_ms - 5_200,
        connect_ms: play.start_ms - 5_000,
        start_ms: play.start_ms - 4_900,
        stop_ms: play.end_ms + 5_000,
        ..Log::default()
    };
    let s = derive(&log.text(), play).session.unwrap();
    assert_eq!(
        (s.connect_lead_ms, s.post_roll_ms),
        (Some(5_000), Some(5_000))
    );
}

// (b) for O1 and O2: below 0.5% passes, 0.5% exactly fails.
#[test]
fn b_o1_o2_thresholds() {
    let o = |drawn: u64, lagged: u64, skipped: u64| {
        let log = Log {
            totals: vec![
                "Output 'adv_stream': Total frames output: 36000".into(),
                format!("Output 'adv_stream': Total drawn frames: {drawn} (36000 attempted)"),
                format!(
                    "Output 'adv_stream': Number of lagged frames due to rendering lag/stalls: {lagged} (0.5%)"
                ),
                format!(
                    "Video stopped, number of skipped frames due to encoding lag: {skipped}/36000 (0.5%)"
                ),
            ],
            ..Log::default()
        };
        let (_, s) = with_log(&log);
        (s.gate("O1").pass, s.gate("O2").pass)
    };
    assert_eq!(o(36000 - 179, 179, 179), (true, true));
    assert_eq!(o(36000 - 180, 180, 179), (false, true));
    assert_eq!(o(36000 - 179, 179, 180), (true, false));
}

#[test]
fn unattended_runs_are_provisional_until_the_owner_confirms() {
    let mut run = passing_obs_run();
    run.owner = spike_core::run::OwnerPublic::default();
    // Attended: no confirmation is invalid.
    let s = evaluate(&run);
    assert!(!s.check("V7").ok);
    assert_eq!(s.exit_code, 2);
    // Unattended: pending, provisional, never a pass.
    run.info.unattended = true;
    let s = evaluate(&run);
    assert!(s.check("V7").ok);
    assert!(s.owner_review_pending);
    assert_eq!(s.exit_code, spike_core::evaluate::PROVISIONAL_EXIT);
    let md = spike_core::export::markdown(&s, &run, &Default::default());
    assert!(md.contains("provisional"), "{md}");
    // The owner confirms: a pass.
    run.owner.game_visible = Some(true);
    run.owner.audio_present = Some(true);
    let s = evaluate(&run);
    assert!(!s.owner_review_pending);
    assert_eq!(s.exit_code, 0);
    // The owner reports a problem: invalid.
    run.owner.audio_present = Some(false);
    assert_eq!(evaluate(&run).exit_code, 2);
    // A failed gate stays a failure while the review is pending.
    run.owner = spike_core::run::OwnerPublic::default();
    run.audio.episodes.push(spike_core::run::Episode {
        start_us: run.info.timeline.play_start_us + 1_000,
        end_us: run.info.timeline.play_start_us + 2_000,
    });
    let s = evaluate(&run);
    assert!(s.owner_review_pending);
    assert_eq!(s.exit_code, 1);
}
