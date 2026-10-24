//! OBS log parser (design V7, O1, O2).
//!
//! Reads the lines OBS writes for a streaming session and reduces them to
//! counts and settings (`ObsDerived`), which is what leaves the PC; raw lines
//! (URLs, paths, device names) never do. Line shapes handled (after the
//! `HH:MM:SS.mmm: ` prefix):
//!
//! - `[rtmp stream: '…'] Connecting to RTMP URL …` (session begins)
//! - `[rtmp stream: '…'] Connection to … successful`
//! - `==== Streaming Start ====…` and `==== Streaming Stop ====…`
//! - `[<encoder>: '…'] settings:` followed by tab-indented `key: value` lines
//! - `Output '…': Total frames output: X` with optional ` (Y attempted)`
//! - `Output '…': Total drawn frames: X` with optional ` (Y attempted)`
//! - `Output '…': Number of lagged frames due to rendering lag/stalls: N (…)`
//! - `Output '…': Number of dropped frames due to insufficient bandwidth/connection stalls: N (…)`
//! - `Video stopped, number of skipped frames due to encoding lag: N/M (…)`
//!
//! OBS prints the lag and drop lines only when the count is nonzero, so in a
//! completed session their absence means zero. Lines are matched with fixed
//! substrings; a regex crate would add nothing for these shapes.

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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObsEncoderSettings {
    pub family: Option<EncoderFamily>,
    pub rate_control: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps_num: Option<u32>,
    pub fps_den: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObsSession {
    pub start_marker: bool,
    pub connected: bool,
    pub stop_marker: bool,
    /// Play start minus session start (negative: the session began late).
    pub pre_roll_ms: i64,
    /// Session stop minus play end; `None` without a stop marker.
    pub post_roll_ms: Option<i64>,
    pub encoder: Option<ObsEncoderSettings>,
    pub frames_output: Option<u64>,
    pub drawn: Option<u64>,
    pub drawn_attempted: Option<u64>,
    pub lagged: Option<u64>,
    pub encode_skipped: Option<u64>,
    pub encode_total: Option<u64>,
    pub bandwidth_dropped: Option<u64>,
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

struct Raw {
    begin_ms: i64,
    stop_ms: Option<i64>,
    s: ObsSession,
}

/// Parses an OBS log and reduces the session overlapping `play`.
pub fn derive(log: &str, play: WallWindow) -> ObsDerived {
    let mut sessions: Vec<Raw> = Vec::new();
    let mut pending_encoder: Option<ObsEncoderSettings> = None;
    let mut in_settings = false;
    let mut clock = Clock::default();
    for line in log.lines() {
        let Some((t, msg)) = clock.split(line) else {
            continue;
        };
        // Settings blocks: tab-indented key/value lines after the header.
        if in_settings {
            if let Some(kv) = msg.strip_prefix('\t') {
                let enc = sessions
                    .last_mut()
                    .filter(|r| r.stop_ms.is_none())
                    .and_then(|r| r.s.encoder.as_mut())
                    .or(pending_encoder.as_mut());
                if let (Some(enc), Some((k, v))) = (enc, kv.split_once(':')) {
                    apply_setting(enc, k.trim(), v.trim());
                }
                continue;
            }
            in_settings = false;
        }
        let open = sessions.last().is_some_and(|r| r.stop_ms.is_none());
        let begins =
            msg.contains("Connecting to RTMP URL") || msg.starts_with("==== Streaming Start");
        if begins && !open {
            sessions.push(Raw {
                begin_ms: t,
                stop_ms: None,
                s: ObsSession {
                    encoder: pending_encoder.take(),
                    ..ObsSession::default()
                },
            });
        }
        if let Some(family) = settings_header(msg) {
            in_settings = true;
            let enc = ObsEncoderSettings {
                family: Some(family),
                ..ObsEncoderSettings::default()
            };
            match sessions.last_mut().filter(|r| r.stop_ms.is_none()) {
                Some(r) => r.s.encoder = Some(enc),
                None => pending_encoder = Some(enc),
            }
            continue;
        }
        let Some(r) = sessions.last_mut() else {
            continue;
        };
        let s = &mut r.s;
        if msg.starts_with("==== Streaming Start") {
            s.start_marker = true;
        } else if msg.starts_with("==== Streaming Stop") {
            if r.stop_ms.is_none() {
                s.stop_marker = true;
                r.stop_ms = Some(t);
            }
        } else if msg.starts_with("[rtmp stream")
            && msg.contains("] Connection to ")
            && msg.ends_with("successful")
        {
            s.connected = true;
        } else if let Some(rest) = after(msg, "Total frames output: ") {
            let (x, _) = count_attempted(rest);
            s.frames_output = x;
        } else if let Some(rest) = after(msg, "Total drawn frames: ") {
            let (x, y) = count_attempted(rest);
            s.drawn = x;
            s.drawn_attempted = y;
        } else if let Some(rest) =
            after(msg, "Number of lagged frames due to rendering lag/stalls: ")
        {
            s.lagged = leading_u64(rest);
        } else if let Some(rest) = after(
            msg,
            "Number of dropped frames due to insufficient bandwidth/connection stalls: ",
        ) {
            s.bandwidth_dropped = leading_u64(rest);
        } else if let Some((n, m)) = after(msg, "number of skipped frames due to encoding lag: ")
            .and_then(|r| r.split_once('/'))
        {
            s.encode_skipped = leading_u64(n);
            s.encode_total = leading_u64(m);
        }
    }
    let sessions_in_log = sessions.len() as u32;
    let session = sessions
        .into_iter()
        .find(|r| r.begin_ms <= play.end_ms && r.stop_ms.is_none_or(|e| e >= play.start_ms))
        .map(|r| ObsSession {
            pre_roll_ms: play.start_ms - r.begin_ms,
            post_roll_ms: r.stop_ms.map(|e| e - play.end_ms),
            ..r.s
        });
    ObsDerived {
        sessions_in_log,
        session,
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
        Some((t, line.get(14..)?))
    }
}

fn settings_header(msg: &str) -> Option<EncoderFamily> {
    if !msg.starts_with('[') || !msg.ends_with("] settings:") {
        return None;
    }
    let label = msg[1..].split(':').next()?.to_ascii_lowercase();
    if label.contains("x264") {
        Some(EncoderFamily::X264)
    } else if label.contains("nvenc") {
        Some(EncoderFamily::Nvenc)
    } else if label.contains("amf") {
        Some(EncoderFamily::Amf)
    } else if label.contains("qsv") {
        Some(EncoderFamily::Qsv)
    } else {
        None
    }
}

fn apply_setting(enc: &mut ObsEncoderSettings, key: &str, value: &str) {
    let n = || leading_u64(value).and_then(|v| u32::try_from(v).ok());
    match key {
        "rate_control" => enc.rate_control = Some(value.to_owned()),
        "bitrate" => enc.bitrate_kbps = n(),
        "width" => enc.width = n(),
        "height" => enc.height = n(),
        "fps_num" => enc.fps_num = n(),
        "fps_den" => enc.fps_den = n(),
        _ => {}
    }
}

fn after<'a>(msg: &'a str, needle: &str) -> Option<&'a str> {
    msg.find(needle).map(|i| &msg[i + needle.len()..])
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
