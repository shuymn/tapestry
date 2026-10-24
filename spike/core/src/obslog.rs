//! OBS log parser (design V7, O1, O2).
//!
//! Reads the lines OBS writes for a streaming session and reduces them to
//! counts and settings (`ObsDerived`), which is what leaves the PC; raw lines
//! (URLs, paths, device names) never do. Line shapes follow OBS's sources
//! (after the `HH:MM:SS.mmm: ` prefix):
//!
//! - `video settings reset:` then tab-indented `base resolution`, `output
//!   resolution`, `downscale filter`, `fps` (libobs `obs_reset_video`)
//! - `[rtmp stream: '<out>'] Connecting to RTMP URL …` (session begins)
//! - `[rtmp stream: '<out>'] Connection to … successful`
//! - `==== Streaming Start ====…` / `==== Streaming Stop ====…`, and the
//!   same markers for `Recording`, `Replay Buffer` and `Virtual Camera`
//! - encoder lines `[<label>: '<name>'] …`: x264 logs `preset:`, `profile:`
//!   and `tune:` on their own lines before its `settings:` block; NVENC
//!   (`obs-nvenc`), AMF (`texture-amf-h264`) and QSV (`qsv encoder`) log
//!   everything inside `settings:` as tab-indented `key: value` lines (QSV
//!   continues the block with unprefixed tab lines)
//! - `Output '<out>': stopping`, `Total frames output: X [(Y attempted)]`,
//!   `Total drawn frames: X [(Y attempted)]`, `Number of lagged frames due
//!   to rendering lag/stalls: N (…)`, `Number of dropped frames due to
//!   insufficient bandwidth/connection stalls: N (…)` (libobs
//!   `log_frame_info`)
//! - `Video stopped, number of skipped frames due to encoding lag: N/M (…)`
//!   (libobs `log_skipped`)
//! - `Number of memory leaks: N`, the last line OBS 31 logs, after
//!   `obs_shutdown` has destroyed every output (joining their teardown)
//!
//! Encoding-lag ownership: libobs logs the encoding-lag summary from the
//! output's teardown thread, which it does not join, so the line may come
//! before or after the frontend's `Streaming Stop` marker. The stream owns
//! the first summary logged while it runs or after its stop marker, until
//! the next output or encoder starts (another output's or a new session's
//! start marker or connection, or an encoder being created). A summary
//! logged after that while the stream has none is ambiguous, which V7
//! rejects. A stream with no summary has zero skipped frames only if the log
//! reaches OBS's final `Number of memory leaks:` line after the stream
//! stopped (every teardown has finished by then); otherwise V7 rejects it as
//! unconfirmed. The `==== Shutting down` separator does not confirm
//! anything: OBS prints it at the start of closing the window, before core
//! shutdown, and may still log the stream's stop after it.
//!
//! OBS prints the lag and drop lines only when the count is nonzero, so in a
//! completed session their absence means zero. Only the streaming output's
//! counters are taken; any other output active during the session (for
//! example a recording, whose cumulative counters would dilute the ratios)
//! is reported so V7 can reject it. Lines are matched with fixed substrings;
//! a regex crate would add nothing for these shapes.

use serde::{Deserialize, Serialize};

/// Play window in ms since midnight of the day of the log's first line (the
/// app derives it from the private wall clock).
#[derive(Clone, Copy, Debug)]
pub struct WallWindow {
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EncoderFamily {
    X264,
    Nvenc,
    Amf,
    Qsv,
}

/// Workload-defining encoder settings as logged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObsEncoderSettings {
    pub family: Option<EncoderFamily>,
    /// Lowercased label, for example `texture-amf-h264` (AMF has no codec
    /// line; its label names the codec).
    pub label: Option<String>,
    pub codec: Option<String>,
    pub rate_control: Option<String>,
    /// `bitrate` (x264, NVENC, AMF) or `target_bitrate` (QSV).
    pub bitrate_kbps: Option<u32>,
    pub preset: Option<String>,
    pub profile: Option<String>,
    /// x264 `tune`.
    pub tune: Option<String>,
    /// NVENC `tuning` and `multipass`.
    pub tuning: Option<String>,
    pub multipass: Option<String>,
    /// QSV `target_usage`.
    pub target_usage: Option<String>,
    /// Encoded frame size. Every supported encoder logs it; it must be the
    /// output size, or the encoder scales the stream (for example to 720p
    /// while the canvas is 1080p).
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// The output video settings (`video settings reset:` block).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObsVideo {
    pub base_width: Option<u32>,
    pub base_height: Option<u32>,
    pub output_width: Option<u32>,
    pub output_height: Option<u32>,
    pub fps_num: Option<u32>,
    pub fps_den: Option<u32>,
    pub downscale_filter: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObsSession {
    pub start_marker: bool,
    pub connected: bool,
    pub stop_marker: bool,
    /// Play start minus the successful connection (negative: connected
    /// after play started).
    pub connect_lead_ms: Option<i64>,
    /// Play start minus the `Streaming Start` marker.
    pub start_lead_ms: Option<i64>,
    /// Session stop minus play end; `None` without a stop marker.
    pub post_roll_ms: Option<i64>,
    /// Another output (recording, replay buffer, virtual camera) was active
    /// during the session.
    pub other_output_active: bool,
    pub video: Option<ObsVideo>,
    pub encoder: Option<ObsEncoderSettings>,
    pub frames_output: Option<u64>,
    pub drawn: Option<u64>,
    pub drawn_attempted: Option<u64>,
    pub lagged: Option<u64>,
    pub encode_skipped: Option<u64>,
    pub encode_total: Option<u64>,
    pub bandwidth_dropped: Option<u64>,
    /// The log reaches OBS's final line after the stream stopped, so the
    /// stream's teardown (and any encoding-lag summary) has been logged.
    pub teardown_confirmed: bool,
    /// An encoding-lag summary came after another output started while the
    /// stream had none: it may be the stream's or the other output's.
    pub encode_lag_ambiguous: bool,
}

impl ObsSession {
    /// Start, connect and stop blocks all present with the output counters.
    pub fn completed(&self) -> bool {
        self.start_marker
            && self.connected
            && self.stop_marker
            && self.frames_output.is_some()
            && self.drawn.is_some()
    }

    /// O1 counts `(lagged, attempted)`: attempted is Y of "X (Y attempted)",
    /// or X when OBS printed no attempted count (no lag).
    pub fn render_lag(&self) -> (u64, u64) {
        let drawn = self.drawn.unwrap_or(0);
        let attempted = self.drawn_attempted.unwrap_or(drawn);
        let lagged = self.lagged.unwrap_or(attempted.saturating_sub(drawn));
        (lagged, attempted)
    }

    /// O2 counts `(skipped, total)`; absent line means none skipped.
    pub fn encode_lag(&self) -> (u64, u64) {
        (
            self.encode_skipped.unwrap_or(0),
            self.encode_total.or(self.frames_output).unwrap_or(0),
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObsDerived {
    pub sessions_in_log: u32,
    /// The first session overlapping the play window, if any.
    pub session: Option<ObsSession>,
}

/// Owner procedure §4.1 output settings.
pub const OUTPUT_WIDTH: u32 = 1_920;
pub const OUTPUT_HEIGHT: u32 = 1_080;
pub const OUTPUT_FPS: u32 = 60;
pub const BITRATE_KBPS: u32 = 6_000;

fn is(v: &Option<String>, want: &str) -> bool {
    v.as_deref()
        .is_some_and(|x| x.trim().eq_ignore_ascii_case(want))
}

fn is_h264(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|x| {
        let n: String = x
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        n == "h264" || n == "avc"
    })
}

impl ObsVideo {
    /// 1920×1080 output at 60/1 with the bicubic downscale filter.
    pub fn matches_procedure(&self) -> bool {
        self.output_width == Some(OUTPUT_WIDTH)
            && self.output_height == Some(OUTPUT_HEIGHT)
            && self.fps_num == Some(OUTPUT_FPS)
            && self.fps_den == Some(1)
            && is(&self.downscale_filter, "bicubic")
    }
}

impl ObsEncoderSettings {
    /// Encoder workload of §4.1: H.264 CBR 6000 kbps and, per family, x264
    /// veryfast/high/no tune, NVENC P5/HQ/two-pass quarter resolution/high,
    /// AMF quality/high, QSV balanced (TU4)/high.
    pub fn matches_procedure(&self) -> bool {
        let common = is(&self.rate_control, "CBR")
            && self.bitrate_kbps == Some(BITRATE_KBPS)
            && self.width == Some(OUTPUT_WIDTH)
            && self.height == Some(OUTPUT_HEIGHT);
        let family = match self.family {
            Some(EncoderFamily::X264) => {
                is(&self.preset, "veryfast")
                    && is(&self.profile, "high")
                    && self
                        .tune
                        .as_deref()
                        .is_none_or(|t| matches!(t.trim(), "" | "(null)" | "none"))
            }
            Some(EncoderFamily::Nvenc) => {
                is_h264(&self.codec)
                    && is(&self.preset, "p5")
                    && is(&self.tuning, "hq")
                    && is(&self.multipass, "qres")
                    && is(&self.profile, "high")
            }
            Some(EncoderFamily::Amf) => {
                self.label.as_deref().is_some_and(|l| l.ends_with("h264"))
                    && is(&self.preset, "quality")
                    && is(&self.profile, "high")
            }
            Some(EncoderFamily::Qsv) => {
                is_h264(&self.codec) && is(&self.target_usage, "TU4") && is(&self.profile, "high")
            }
            None => false,
        };
        common && family
    }
}

#[derive(Default)]
struct Raw {
    begin_ms: i64,
    output: Option<String>,
    connect_ms: Option<i64>,
    start_ms: Option<i64>,
    stop_ms: Option<i64>,
    /// After the stop marker, until the next output or encoder starts: a
    /// trailing encoding-lag summary is the stream's.
    owns_lag: bool,
    s: ObsSession,
}

/// Another output or encoder started: stopped streams stop owning trailing
/// encoding-lag summaries.
fn end_ownership(sessions: &mut [Raw]) {
    sessions.iter_mut().for_each(|r| r.owns_lag = false);
}

/// Start and stop marker prefixes of outputs other than the stream.
const OTHER_OUTPUT_MARKERS: [(&str, &str); 3] = [
    ("==== Recording Start", "==== Recording Stop"),
    ("==== Replay Buffer Start", "==== Replay Buffer Stop"),
    ("==== Virtual Camera Start", "==== Virtual Camera Stop"),
];

#[derive(Clone, Copy, PartialEq)]
enum Block {
    None,
    Video,
    Encoder,
}

/// Parses an OBS log and reduces the session overlapping `play`.
pub fn derive(log: &str, play: WallWindow) -> ObsDerived {
    let mut sessions: Vec<Raw> = Vec::new();
    let mut video: Option<ObsVideo> = None;
    // Encoder being described, before it is attached to a session.
    let mut pending: Option<ObsEncoderSettings> = None;
    let mut block = Block::None;
    // Other outputs currently running (by marker).
    let mut others_running = 0u32;
    let mut clock = Clock::default();
    for line in log.lines() {
        let Some((t, msg)) = clock.split(line) else {
            continue;
        };
        let open = sessions.last().is_some_and(|r| r.stop_ms.is_none());

        if let Some(kv) = msg.strip_prefix('\t') {
            if let Some((k, v)) = kv.split_once(':') {
                let (k, v) = (k.trim(), v.trim());
                match block {
                    Block::Video => apply_video(video.get_or_insert_with(ObsVideo::default), k, v),
                    Block::Encoder => {
                        let enc = encoder_slot(&mut sessions, &mut pending);
                        if let Some(enc) = enc {
                            apply_encoder(enc, k, v);
                        }
                    }
                    Block::None => {}
                }
            }
            continue;
        }
        block = Block::None;

        if let Some((n, m)) = msg
            .strip_prefix("Video stopped, number of skipped frames due to encoding lag: ")
            .and_then(|r| r.split_once('/'))
        {
            let lag = (leading_u64(n), leading_u64(m));
            let owner = sessions
                .iter()
                .rposition(|r| r.owns_lag)
                .or_else(|| sessions.iter().rposition(|r| r.stop_ms.is_none()));
            if let Some(i) = owner
                && sessions[i].s.encode_skipped.is_none()
            {
                let r = &mut sessions[i];
                (r.s.encode_skipped, r.s.encode_total) = lag;
                r.owns_lag = false;
            }
            for (k, r) in sessions.iter_mut().enumerate() {
                if Some(k) != owner && r.stop_ms.is_some() && r.s.encode_skipped.is_none() {
                    r.s.encode_lag_ambiguous = true;
                }
            }
            continue;
        }
        if msg.starts_with("Number of memory leaks:") {
            for r in sessions.iter_mut().filter(|r| r.stop_ms.is_some()) {
                r.s.teardown_confirmed = true;
            }
            continue;
        }
        if msg == "video settings reset:" {
            video = Some(ObsVideo::default());
            block = Block::Video;
            continue;
        }
        if let Some((family, label, rest)) = encoder_line(msg) {
            // A new encoder instance starts with its first line after a
            // completed description (x264: `preset:`; others: `settings:`).
            let starts_new =
                rest == "settings:" && family != EncoderFamily::X264 || rest.starts_with("preset:");
            let slot = encoder_slot(&mut sessions, &mut pending);
            let needs_new = match slot {
                None => true,
                Some(e) => starts_new || e.family != Some(family),
            };
            if needs_new {
                end_ownership(&mut sessions);
                let enc = ObsEncoderSettings {
                    family: Some(family),
                    label: Some(label),
                    ..ObsEncoderSettings::default()
                };
                match sessions.last_mut().filter(|r| r.stop_ms.is_none()) {
                    Some(r) => r.s.encoder = Some(enc),
                    None => pending = Some(enc),
                }
            }
            if let Some(enc) = encoder_slot(&mut sessions, &mut pending) {
                if rest == "settings:" {
                    block = Block::Encoder;
                } else if let Some((k, v)) = rest.split_once(':') {
                    apply_encoder(enc, k.trim(), v.trim());
                }
            }
            continue;
        }

        for (start, stop) in OTHER_OUTPUT_MARKERS {
            if msg.starts_with(start) {
                end_ownership(&mut sessions);
                others_running += 1;
                if open && let Some(r) = sessions.last_mut() {
                    r.s.other_output_active = true;
                }
            } else if msg.starts_with(stop) {
                others_running = others_running.saturating_sub(1);
            }
        }

        let rtmp_output = msg
            .strip_prefix("[rtmp stream: '")
            .and_then(|r| r.split_once("'] "));
        let begins = rtmp_output.is_some_and(|(_, m)| m.starts_with("Connecting to RTMP URL"))
            || msg.starts_with("==== Streaming Start");
        if begins && !open {
            end_ownership(&mut sessions);
            sessions.push(Raw {
                begin_ms: t,
                s: ObsSession {
                    encoder: pending.take(),
                    video: video.clone(),
                    other_output_active: others_running > 0,
                    ..ObsSession::default()
                },
                ..Raw::default()
            });
        }
        let Some(r) = sessions.last_mut() else {
            continue;
        };
        if let Some((out, m)) = rtmp_output {
            r.output.get_or_insert_with(|| out.to_owned());
            if m.starts_with("Connection to ")
                && m.ends_with("successful")
                && r.connect_ms.is_none()
            {
                r.s.connected = true;
                r.connect_ms = Some(t);
            }
        } else if msg.starts_with("==== Streaming Start") {
            if r.start_ms.is_none() {
                r.s.start_marker = true;
                r.start_ms = Some(t);
            }
        } else if msg.starts_with("==== Streaming Stop") {
            if r.stop_ms.is_none() {
                r.s.stop_marker = true;
                r.stop_ms = Some(t);
                r.owns_lag = true;
            }
        } else if let Some(rest) = msg.strip_prefix("Output '") {
            let Some((out, m)) = rest.split_once("': ") else {
                continue;
            };
            if r.output.as_deref() != Some(out) {
                // Another output's counters are never mixed into the
                // stream's; one reporting during the session overlapped it.
                if r.stop_ms.is_none() {
                    r.s.other_output_active = true;
                }
                continue;
            }
            let s = &mut r.s;
            if let Some(rest) = m.strip_prefix("Total frames output: ") {
                s.frames_output = count_attempted(rest).0;
            } else if let Some(rest) = m.strip_prefix("Total drawn frames: ") {
                (s.drawn, s.drawn_attempted) = count_attempted(rest);
            } else if let Some(rest) =
                m.strip_prefix("Number of lagged frames due to rendering lag/stalls: ")
            {
                s.lagged = leading_u64(rest);
            } else if let Some(rest) = m.strip_prefix(
                "Number of dropped frames due to insufficient bandwidth/connection stalls: ",
            ) {
                s.bandwidth_dropped = leading_u64(rest);
            }
        }
    }
    let sessions_in_log = sessions.len() as u32;
    let session = sessions
        .into_iter()
        .find(|r| r.begin_ms <= play.end_ms && r.stop_ms.is_none_or(|e| e >= play.start_ms))
        .map(|r| ObsSession {
            connect_lead_ms: r.connect_ms.map(|c| play.start_ms - c),
            start_lead_ms: r.start_ms.map(|c| play.start_ms - c),
            post_roll_ms: r.stop_ms.map(|e| e - play.end_ms),
            ..r.s
        });
    ObsDerived {
        sessions_in_log,
        session,
    }
}

/// The encoder being described: the open session's, else the pending one.
fn encoder_slot<'a>(
    sessions: &'a mut [Raw],
    pending: &'a mut Option<ObsEncoderSettings>,
) -> Option<&'a mut ObsEncoderSettings> {
    match sessions.last_mut().filter(|r| r.stop_ms.is_none()) {
        Some(r) if r.s.encoder.is_some() => r.s.encoder.as_mut(),
        _ => pending.as_mut(),
    }
}

/// `[<label>: '<name>'] <rest>` for a known video encoder family.
fn encoder_line(msg: &str) -> Option<(EncoderFamily, String, &str)> {
    let rest = msg.strip_prefix('[')?;
    let (label, rest) = rest.split_once(": '")?;
    let (_, rest) = rest.split_once("'] ")?;
    let label = label.to_ascii_lowercase();
    let family = if label.contains("x264") {
        EncoderFamily::X264
    } else if label.contains("nvenc") {
        EncoderFamily::Nvenc
    } else if label.contains("amf") {
        EncoderFamily::Amf
    } else if label.contains("qsv") {
        EncoderFamily::Qsv
    } else {
        return None;
    };
    Some((family, label, rest))
}

fn apply_encoder(enc: &mut ObsEncoderSettings, key: &str, value: &str) {
    let s = || Some(value.to_owned());
    match key {
        "codec" => enc.codec = s(),
        "rate_control" => enc.rate_control = s(),
        "bitrate" | "target_bitrate" => {
            enc.bitrate_kbps = leading_u64(value).and_then(|v| u32::try_from(v).ok());
        }
        "preset" => enc.preset = s(),
        "profile" => enc.profile = s(),
        "tune" => enc.tune = s(),
        "tuning" => enc.tuning = s(),
        "multipass" => enc.multipass = s(),
        "target_usage" => enc.target_usage = s(),
        "width" => enc.width = leading_u64(value).and_then(|v| u32::try_from(v).ok()),
        "height" => enc.height = leading_u64(value).and_then(|v| u32::try_from(v).ok()),
        _ => {}
    }
}

fn apply_video(v: &mut ObsVideo, key: &str, value: &str) {
    let pair = |sep: char| {
        let (a, b) = value.split_once(sep)?;
        Some((leading_u64(a)? as u32, leading_u64(b)? as u32))
    };
    match key {
        "base resolution" => (v.base_width, v.base_height) = pair('x').unzip(),
        "output resolution" => (v.output_width, v.output_height) = pair('x').unzip(),
        "fps" => (v.fps_num, v.fps_den) = pair('/').unzip(),
        "downscale filter" => v.downscale_filter = Some(value.to_owned()),
        _ => {}
    }
}

/// Splits the `HH:MM:SS.mmm: ` prefix and unwraps midnight crossings.
#[derive(Default)]
struct Clock {
    last_ms: Option<i64>,
    day_offset: i64,
}

impl Clock {
    fn split<'a>(&mut self, line: &'a str) -> Option<(i64, &'a str)> {
        let b = line.as_bytes();
        if b.len() < 14 || b[2] != b':' || b[5] != b':' || b[8] != b'.' || &b[12..14] != b": " {
            return None;
        }
        let num = |r: std::ops::Range<usize>| line.get(r)?.parse::<i64>().ok();
        let ms = ((num(0..2)? * 60 + num(3..5)?) * 60 + num(6..8)?) * 1000 + num(9..12)?;
        const DAY: i64 = 86_400_000;
        if let Some(last) = self.last_ms
            && ms + self.day_offset < last - DAY / 2
        {
            self.day_offset += DAY;
        }
        let t = ms + self.day_offset;
        self.last_ms = Some(t);
        Some((t, line.get(14..)?.trim_end_matches('\r')))
    }
}

fn leading_u64(s: &str) -> Option<u64> {
    let s = s.trim_start();
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// `"X"` or `"X (Y attempted)"`.
fn count_attempted(rest: &str) -> (Option<u64>, Option<u64>) {
    let x = leading_u64(rest);
    let y = rest
        .split_once('(')
        .filter(|(_, r)| r.contains("attempted"))
        .and_then(|(_, r)| leading_u64(r));
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_unwraps_midnight() {
        let mut c = Clock::default();
        assert_eq!(c.split("23:59:59.900: a").map(|x| x.0), Some(86_399_900));
        assert_eq!(c.split("00:00:00.100: b").map(|x| x.0), Some(86_400_100));
        assert_eq!(c.split("not a line"), None);
    }

    #[test]
    fn counts_with_and_without_attempted() {
        assert_eq!(count_attempted("36000"), (Some(36000), None));
        assert_eq!(
            count_attempted("35800 (36000 attempted)"),
            (Some(35800), Some(36000))
        );
    }
}
