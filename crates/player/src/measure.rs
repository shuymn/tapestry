//! The measurement of D11 on the game's threads: the render thread's
//! [`Measure`] keeps each play's figures in preallocated rings, fed by its
//! own frames (render CPU work, frame statistics, the frame that first drew
//! each judgment) and by queues from the logic step (input to judgment,
//! finalization delay) and the audio thread (press to sound scheduled). It
//! draws them in the overlay while the overlay key shows it and hands them
//! to the game thread when the play ends, which writes them next to the
//! saved record as `<record>.measure.toml`.
//!
//! Every sample carries the play's generation, and samples of another
//! play are dropped, so a play never counts its neighbor's. Nothing here
//! allocates while a play is measured; the figures handed over at its end
//! are built then.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bms_model::{TimeSource, TimeUs};
use eval::{JudgeEvent, JudgeEventKind};
use game::{AudioProbe, Probed, Underruns};
use input::QpcClock;
use render::FrameTiming;
use render::measure::{
    FrameStatistics, OnScreen, RefreshCount, RefreshLedger, Samples, StatsFilter, Summary,
    display_qpc,
};

/// Samples each figure's ring keeps; the percentiles cover the latest
/// this many, the count and the maximum every sample.
pub const RING: usize = 8_192;
/// Judgments in flight in the press → on screen table.
const IN_FLIGHT: usize = 4_096;
/// Logic step samples queued for the render thread: four seconds of steps
/// at 1 kHz.
pub const STEP_QUEUE: usize = 4_096;
/// Key sound samples queued for the render thread.
pub const SOUND_QUEUE: usize = 1_024;
/// How often the shown overlay's numbers are worked out again, µs.
const OVERLAY_REFRESH_US: i64 = 250_000;
/// Overlay lines, and characters per line.
const LINES: usize = 12;
const LINE_LEN: usize = 56;
/// Presents remembered to take the ones after a play's end back out of its
/// count.
const RECENT: usize = 16;

/// What one logic step measured, for the render thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepSample {
    /// The play it belongs to.
    pub generation: u64,
    /// Logic time minus the watermark, µs.
    pub finalization_us: Option<i64>,
    /// Logic time minus the oldest origin judged in the step, µs.
    pub judgment_us: Option<i64>,
}

/// The output's sizes, shown beside press → sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioInfo {
    /// One device period, µs.
    pub period_us: i64,
    /// Periods the output keeps queued, when it reports them.
    pub queued_periods: Option<u32>,
}

/// Where a play lies, for its measurement.
#[derive(Clone, Debug)]
pub struct PlayMeasure {
    /// The play's number: its samples carry it.
    pub generation: u64,
    /// Where chart time 0 lies on the clock.
    pub origin: TimeUs,
    /// What the load time runs from: the process start (0) for the first
    /// play, the retry's receipt for a retry.
    pub load_from: TimeUs,
    /// Whether the play is a retry.
    pub retry: bool,
    /// The audio thread's underruns and their count when the play began;
    /// `None` without an audio thread to ask.
    pub underruns: Option<(Underruns, u64)>,
}

/// A play's figures, as written to `.measure.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Figures {
    /// Input → judgment, µs.
    pub input_to_judgment: Option<Summary>,
    /// Finalization delay, µs.
    pub finalization: Option<Summary>,
    /// Press → sound scheduled, µs.
    pub press_to_sound: Option<Summary>,
    /// The output beside it.
    pub audio: Option<AudioInfo>,
    /// Press → on screen, µs.
    pub press_to_screen: Option<Summary>,
    /// Judgments whose frame had no accepted frame statistics sample.
    pub press_to_screen_unavailable: u64,
    /// Lost refreshes from frame statistics.
    pub refreshes: RefreshCount,
    /// Render CPU work per frame, µs.
    pub render_cpu: Option<Summary>,
    /// Audio underruns during the play.
    pub underruns: Option<u64>,
    /// The play span on the clock, from chart time 0 to the play's end.
    /// The countdown is left out: a display capture misses a swap chain's
    /// first present, which is the countdown's first frame.
    pub span: (TimeUs, TimeUs),
    /// Presents the game issued in the span.
    pub presents: u64,
    /// Allocations per thread during the play: input, logic, audio,
    /// render (dev-tools).
    pub allocations: Option<[u64; 4]>,
    /// Logic step samples the full queue refused: input to judgment and
    /// finalization delay are missing that many.
    pub steps_dropped: u64,
    /// One refresh, ns: measured from the play's accepted frame statistics
    /// (the sync times over the refreshes between them), else from the
    /// monitor's reported rate, which may be rounded to a whole hertz.
    pub refresh_period_ns: Option<i64>,
}

/// Refreshes two syncs must lie apart before their times give the period.
const PERIOD_SPAN: u32 = 60;

/// One refresh in ns from two syncs `(QPC ticks, refresh count)` at
/// `freq` ticks a second, once they lie [`PERIOD_SPAN`] refreshes apart.
fn measured_period_ns(first: (i64, u32), last: (i64, u32), freq: i64) -> Option<i64> {
    let refreshes = last.1.wrapping_sub(first.1);
    if !(PERIOD_SPAN..=u32::MAX / 2).contains(&refreshes) || freq <= 0 {
        return None;
    }
    let ns =
        i128::from(last.0 - first.0) * 1_000_000_000 / (i128::from(freq) * i128::from(refreshes));
    i64::try_from(ns).ok().filter(|&ns| ns > 0)
}

/// One overlay line in a fixed buffer.
#[derive(Clone, Copy)]
struct Line {
    buf: [u8; LINE_LEN],
    len: usize,
}

impl Line {
    const EMPTY: Self = Self {
        buf: [b' '; LINE_LEN],
        len: 0,
    };

    fn as_str(&self) -> &str {
        // Only ASCII is written.
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len < LINE_LEN {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
        Ok(())
    }
}

/// `us` as milliseconds with one decimal, truncated toward zero.
struct Ms(i64);

impl fmt::Display for Ms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let a = self.0.unsigned_abs();
        write!(f, "{sign}{}.{}", a / 1_000, a % 1_000 / 100)
    }
}

/// A summary's three values, or N/A.
struct Percentiles<'a>(&'a Option<Summary>, bool);

impl fmt::Display for Percentiles<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.0, self.1) {
            (None, _) => f.write_str("N/A"),
            (Some(s), false) => write!(f, "P50 {} P99 {} MAX {}", s.p50, s.p99, s.max),
            (Some(s), true) => write!(f, "P50 {} P99 {} MAX {}", Ms(s.p50), Ms(s.p99), Ms(s.max)),
        }
    }
}

/// The overlay's lines for `fig`, written into `lines`; returns how many.
/// `load` is the play's load time and whether it is a retry's.
fn write_overlay(fig: &Figures, load: Option<(bool, i64)>, lines: &mut [Line; LINES]) -> usize {
    for l in lines.iter_mut() {
        l.len = 0;
    }
    let mut n = 0;
    let mut line = |args: fmt::Arguments<'_>| {
        if n < LINES {
            let _ = lines[n].write_fmt(args);
            n += 1;
        }
    };
    line(format_args!("MEASUREMENT (F9)"));
    line(format_args!(
        "INPUT>JUDGE US {}",
        Percentiles(&fig.input_to_judgment, false)
    ));
    line(format_args!(
        "FINALIZE US {}",
        Percentiles(&fig.finalization, false)
    ));
    line(format_args!(
        "PRESS>SOUND MS {}",
        Percentiles(&fig.press_to_sound, true)
    ));
    match fig.audio {
        Some(AudioInfo {
            period_us,
            queued_periods: Some(q),
        }) => line(format_args!(" PERIOD {} MS, {q} QUEUED", Ms(period_us))),
        Some(AudioInfo { period_us, .. }) => {
            line(format_args!(" PERIOD {} MS, QUEUED N/A", Ms(period_us)));
        }
        None => line(format_args!(" PERIOD N/A")),
    }
    line(format_args!(
        "PRESS>SCREEN MS {}",
        Percentiles(&fig.press_to_screen, true)
    ));
    line(format_args!(
        " NO FRAME STATISTICS {}",
        fig.press_to_screen_unavailable
    ));
    let r = fig.refreshes;
    match r.lost() {
        Some(lost) => line(format_args!(
            "LOST REFRESHES {lost} IN {} LONGEST {} N/A {}",
            r.counted, r.longest, r.unavailable
        )),
        None => line(format_args!("LOST REFRESHES N/A ({})", r.unavailable)),
    }
    line(format_args!(
        "RENDER CPU US {}",
        Percentiles(&fig.render_cpu, false)
    ));
    match fig.underruns {
        Some(n) => line(format_args!("UNDERRUNS {n}")),
        None => line(format_args!("UNDERRUNS N/A")),
    }
    match load {
        Some((false, ms)) => line(format_args!("LOAD FIRST {ms} MS")),
        Some((true, ms)) => line(format_args!("LOAD RETRY {ms} MS")),
        None => line(format_args!("LOAD N/A")),
    }
    if let Some([i, l, a, r]) = fig.allocations {
        line(format_args!("ALLOC IN {i} LOGIC {l} AUDIO {a} RENDER {r}"));
    }
    n
}

/// One frame the render thread drew, for [`FrameSink`]s.
///
/// [`FrameSink`]: crate::render_side::FrameSink
#[derive(Clone, Copy, Debug)]
pub struct Drawn<'a> {
    /// The chart time of a play frame, `None` for a text screen.
    pub play: Option<TimeUs>,
    /// What the frame cost and whether it was presented.
    pub timing: FrameTiming,
    /// The swap chain's frame statistics, read right after the frame.
    pub statistics: Option<FrameStatistics>,
    /// The effects the frame drew.
    pub effects: &'a [JudgeEvent],
    /// When the frame was done, on the clock.
    pub at: TimeUs,
}

/// The render thread's measurement of the current play.
pub struct Measure {
    clock: QpcClock,
    /// The refresh rate, mHz, and one refresh in QPC ticks, when known.
    refresh_mhz: Option<u32>,
    period_qpc: Option<i64>,
    /// The play's first and latest accepted syncs: QPC ticks and refresh
    /// count.
    first_sync: Option<(i64, u32)>,
    last_sync: Option<(i64, u32)>,
    audio: Option<AudioInfo>,
    steps: rtrb::Consumer<StepSample>,
    sounds: rtrb::Consumer<AudioProbe>,
    overlay: Arc<AtomicBool>,
    play: Option<PlayMeasure>,
    /// Set when the game thread took the figures: nothing more counts.
    frozen: bool,
    input_to_judgment: Samples,
    finalization: Samples,
    press_to_sound: Samples,
    render_cpu: Samples,
    on_screen: OnScreen,
    filter: StatsFilter,
    refreshes: RefreshLedger,
    /// The last present count, to notice a new swap chain.
    last_present: Option<u32>,
    load_ms: Option<i64>,
    presents: u64,
    recent: [i64; RECENT],
    recent_next: usize,
    lines: [Line; LINES],
    shown: usize,
    refreshed_at: Option<TimeUs>,
    /// The allocation counts when the play started.
    alloc_base: Option<[u64; 4]>,
}

impl Measure {
    /// The measurement, reading the queues `steps` and `sounds`, the
    /// overlay switch `overlay`, and frame statistics at `refresh_mhz`.
    pub fn new(
        clock: QpcClock,
        refresh_mhz: Option<u32>,
        audio: Option<AudioInfo>,
        steps: rtrb::Consumer<StepSample>,
        sounds: rtrb::Consumer<AudioProbe>,
        overlay: Arc<AtomicBool>,
    ) -> Self {
        let freq = i128::from(clock.frequency().hz());
        let period_qpc = refresh_mhz
            .filter(|&m| m > 0)
            .map(|m| i64::try_from(freq * 1_000 / i128::from(m)).unwrap_or(i64::MAX));
        Self {
            clock,
            refresh_mhz: refresh_mhz.filter(|&m| m > 0),
            period_qpc,
            first_sync: None,
            last_sync: None,
            audio,
            steps,
            sounds,
            overlay,
            play: None,
            frozen: true,
            input_to_judgment: Samples::with_capacity(RING),
            finalization: Samples::with_capacity(RING),
            press_to_sound: Samples::with_capacity(RING),
            render_cpu: Samples::with_capacity(RING),
            on_screen: OnScreen::with_capacity(IN_FLIGHT),
            filter: StatsFilter::default(),
            refreshes: RefreshLedger::default(),
            last_present: None,
            load_ms: None,
            presents: 0,
            recent: [i64::MIN; RECENT],
            recent_next: 0,
            lines: [Line::EMPTY; LINES],
            shown: 0,
            refreshed_at: None,
            alloc_base: None,
        }
    }

    /// A play starts: every figure starts over.
    pub fn start(&mut self, play: PlayMeasure) {
        self.input_to_judgment.clear();
        self.finalization.clear();
        self.press_to_sound.clear();
        self.render_cpu.clear();
        self.on_screen.clear();
        self.filter.reset();
        self.refreshes.clear();
        self.last_present = None;
        self.first_sync = None;
        self.last_sync = None;
        self.load_ms = None;
        self.presents = 0;
        self.recent = [i64::MIN; RECENT];
        self.refreshed_at = None;
        self.frozen = false;
        self.alloc_base = allocations();
        self.play = Some(play);
        self.drain();
    }

    /// Takes the queued samples of the current play; others are dropped.
    fn drain(&mut self) {
        let current = self.play.as_ref().filter(|_| !self.frozen);
        let generation = current.map(|p| p.generation);
        while let Ok(s) = self.steps.pop() {
            if Some(s.generation) != generation {
                continue;
            }
            if let Some(v) = s.judgment_us {
                self.input_to_judgment.push(v);
            }
            if let Some(v) = s.finalization_us {
                self.finalization.push(v);
            }
        }
        while let Ok(p) = self.sounds.pop() {
            if Some(p.generation) != generation {
                continue;
            }
            let Probed::KeyScheduled { latency } = p.kind;
            self.press_to_sound.push(latency.0);
        }
    }

    /// The overlay lines to draw over the next frame, worked out again
    /// every quarter second while the overlay shows; empty when hidden.
    pub fn overlay(&mut self) -> [&str; LINES] {
        let mut out = [""; LINES];
        if !self.overlay.load(Ordering::Acquire) || self.frozen {
            return out;
        }
        let now = self.clock.now();
        if self
            .refreshed_at
            .is_none_or(|t| now.0 - t.0 >= OVERLAY_REFRESH_US)
        {
            self.refreshed_at = Some(now);
            self.drain();
            let fig = self.figures_now(now);
            let load = self
                .load_ms
                .map(|ms| (self.play.as_ref().is_some_and(|p| p.retry), ms));
            self.shown = write_overlay(&fig, load, &mut self.lines);
        }
        for (o, l) in out.iter_mut().zip(&self.lines[..self.shown]) {
            *o = l.as_str();
        }
        out
    }

    /// The figures so far, without freezing (no allocation).
    fn figures_now(&mut self, end: TimeUs) -> Figures {
        let underruns = self
            .play
            .as_ref()
            .and_then(|p| p.underruns.as_ref())
            .map(|(u, at_start)| u.get().saturating_sub(*at_start));
        let start = self.play.as_ref().map_or(TimeUs(0), |p| p.origin);
        let after_end = self.recent.iter().filter(|&&t| t >= end.0).count() as u64;
        Figures {
            input_to_judgment: self.input_to_judgment.summary(),
            finalization: self.finalization.summary(),
            press_to_sound: self.press_to_sound.summary(),
            audio: self.audio,
            press_to_screen: self.on_screen.latencies().summary(),
            press_to_screen_unavailable: self.on_screen.unavailable(),
            refreshes: self.refreshes.count(),
            render_cpu: self.render_cpu.summary(),
            underruns,
            span: (start, end),
            presents: self.presents.saturating_sub(after_end),
            allocations: allocations()
                .zip(self.alloc_base)
                .map(|(now, base)| std::array::from_fn(|i| now[i].saturating_sub(base[i]))),
            steps_dropped: 0,
            refresh_period_ns: self
                .first_sync
                .zip(self.last_sync)
                .and_then(|(f, l)| measured_period_ns(f, l, self.clock.frequency().hz()))
                .or_else(|| self.refresh_mhz.map(|m| 1_000_000_000_000 / i64::from(m))),
        }
    }

    /// The play ended at `end` on the clock: its figures, after which
    /// nothing more counts until the next play. Judgments whose frame has
    /// no sample yet are N/A.
    pub fn finish(&mut self, end: TimeUs) -> Option<Figures> {
        if self.frozen {
            return None;
        }
        self.drain();
        self.on_screen.finish();
        let fig = self.figures_now(end);
        self.frozen = true;
        Some(fig)
    }

    /// One frame drawn.
    pub fn frame(&mut self, d: &Drawn<'_>) {
        let Some(play) = &self.play else { return };
        if self.frozen || d.play.is_none() {
            return;
        }
        let origin = play.origin;
        let load_from = play.load_from;
        self.drain();
        for ev in d.effects {
            if let (JudgeEventKind::Judged { .. }, Some(o)) = (ev.kind, ev.origin) {
                self.on_screen.drawn(ev.seq, o.0);
            }
        }
        self.render_cpu
            .push(i64::try_from(d.timing.cpu.as_micros()).unwrap_or(i64::MAX));
        if !d.timing.presented {
            return;
        }
        if self.load_ms.is_none() {
            self.load_ms = Some((d.at.0 - load_from.0) / 1_000);
        }
        if d.at >= origin {
            self.presents += 1;
            self.recent[self.recent_next] = d.at.0;
            self.recent_next = (self.recent_next + 1) % RECENT;
        }
        let Some(count) = d.timing.present_count else {
            // No present count to pair a sample with: N/A.
            self.on_screen.finish();
            return;
        };
        if self.last_present.is_some_and(|last| count <= last) {
            // A new swap chain counts again from the start.
            self.on_screen.finish();
            self.filter.reset();
        }
        self.last_present = Some(count);
        self.on_screen.presented(count);
        if let Some(s) = self.filter.accept(d.statistics) {
            self.refreshes.accepted(&s);
            let sync = (s.sync_qpc_time, s.sync_refresh_count);
            let first = *self.first_sync.get_or_insert(sync);
            self.last_sync = Some(sync);
            // The measured period once the syncs span enough refreshes; the
            // monitor's reported rate may be rounded to a whole hertz.
            let refreshes = sync.1.wrapping_sub(first.1);
            let measured = (PERIOD_SPAN..=u32::MAX / 2)
                .contains(&refreshes)
                .then(|| (sync.0 - first.0) / i64::from(refreshes));
            if let Some(period) = measured.or(self.period_qpc) {
                let display = self.clock.from_qpc(display_qpc(&s, period));
                self.on_screen.sample(s.present_count, display.0 - origin.0);
            }
        }
    }
}

#[cfg(feature = "dev-tools")]
fn allocations() -> Option<[u64; 4]> {
    Some(crate::alloc_count::counts())
}

#[cfg(not(feature = "dev-tools"))]
const fn allocations() -> Option<[u64; 4]> {
    None
}

/// `<record>.measure.toml` beside the record at `record`.
pub fn measure_path(record: &Path) -> PathBuf {
    record.with_extension("measure.toml")
}

fn summary_toml(out: &mut String, table: &str, s: Option<&Summary>) {
    let _ = writeln!(out, "\n[{table}]");
    match s {
        Some(s) => {
            let _ = writeln!(
                out,
                "count = {}\np50 = {}\np99 = {}\nmax = {}",
                s.count, s.p50, s.p99, s.max
            );
        }
        None => out.push_str("count = 0\n"),
    }
}

/// The figures as TOML, numbers only; a figure without evidence (N/A)
/// has no key. `load` is the first load and the latest retry, ms.
pub fn to_toml(fig: &Figures, load: (Option<u64>, Option<u64>)) -> String {
    let mut out = String::from("# Measurement of the play (D11): numbers only, N/A left out.\n");
    summary_toml(
        &mut out,
        "input_to_judgment_us",
        fig.input_to_judgment.as_ref(),
    );
    let _ = writeln!(out, "dropped = {}", fig.steps_dropped);
    summary_toml(&mut out, "finalization_delay_us", fig.finalization.as_ref());
    let _ = writeln!(out, "dropped = {}", fig.steps_dropped);
    summary_toml(
        &mut out,
        "press_to_sound_scheduled_us",
        fig.press_to_sound.as_ref(),
    );
    if let Some(a) = fig.audio {
        let _ = writeln!(out, "device_period_us = {}", a.period_us);
        if let Some(q) = a.queued_periods {
            let _ = writeln!(out, "queued_periods = {q}");
        }
    }
    summary_toml(&mut out, "press_to_screen_us", fig.press_to_screen.as_ref());
    let _ = writeln!(out, "unavailable = {}", fig.press_to_screen_unavailable);
    let r = fig.refreshes;
    out.push_str("\n[lost_refreshes_frame_statistics]\n");
    if let Some(lost) = r.lost() {
        let _ = writeln!(out, "lost = {lost}\nlongest = {}", r.longest);
    }
    let _ = writeln!(
        out,
        "intervals = {}\nunavailable_intervals = {}",
        r.counted, r.unavailable
    );
    summary_toml(&mut out, "render_cpu_us", fig.render_cpu.as_ref());
    out.push_str("\n[audio]\n");
    if let Some(n) = fig.underruns {
        let _ = writeln!(out, "underruns = {n}");
    }
    out.push_str("\n[load_ms]\n");
    if let Some(ms) = load.0 {
        let _ = writeln!(out, "first = {ms}");
    }
    if let Some(ms) = load.1 {
        let _ = writeln!(out, "retry = {ms}");
    }
    if let Some([i, l, a, r]) = fig.allocations {
        let _ = writeln!(
            out,
            "\n[allocations]\ninput = {i}\nlogic = {l}\naudio = {a}\nrender = {r}"
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Figures, LINES, Line, write_overlay};

    fn lines_of(fig: &Figures, load: Option<(bool, i64)>) -> Vec<String> {
        let mut lines = [Line::EMPTY; LINES];
        let n = write_overlay(fig, load, &mut lines);
        lines[..n].iter().map(|l| l.as_str().to_owned()).collect()
    }

    /// Without evidence every figure reads N/A, never 0.
    #[test]
    fn an_empty_play_shows_na() {
        let lines = lines_of(&Figures::default(), None);
        for (i, want) in [
            (1, "INPUT>JUDGE US N/A"),
            (2, "FINALIZE US N/A"),
            (3, "PRESS>SOUND MS N/A"),
            (4, " PERIOD N/A"),
            (5, "PRESS>SCREEN MS N/A"),
            (7, "LOST REFRESHES N/A (0)"),
            (8, "RENDER CPU US N/A"),
            (9, "UNDERRUNS N/A"),
            (10, "LOAD N/A"),
        ] {
            assert_eq!(lines[i], want);
        }
    }

    /// The overlay's numbers come from the rings without allocating: the
    /// rings, the percentiles and the lines are all preallocated. The
    /// counting allocator counts this thread's allocations.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn overlay_percentiles_without_allocation() {
        use super::AudioInfo;
        use crate::alloc_count::this_thread;
        use render::measure::{RefreshCount, Samples};
        let mut judge = Samples::with_capacity(super::RING);
        let mut sound = Samples::with_capacity(super::RING);
        let mut lines = [Line::EMPTY; LINES];
        let before = this_thread();
        // 1..=200 µs and 20.0..=29.9 ms, pushed backwards.
        for v in (1..=200).rev() {
            judge.push(v);
        }
        for v in (0..100).rev() {
            sound.push(20_000 + v * 100);
        }
        let fig = Figures {
            input_to_judgment: judge.summary(),
            press_to_sound: sound.summary(),
            audio: Some(AudioInfo {
                period_us: 10_000,
                queued_periods: Some(2),
            }),
            refreshes: RefreshCount {
                lost: 3,
                counted: 1_000,
                unavailable: 4,
                longest: 2,
            },
            underruns: Some(1),
            allocations: Some([0, 1, 2, 3]),
            ..Figures::default()
        };
        let n = write_overlay(&fig, Some((true, 85)), &mut lines);
        let after = this_thread();
        assert_eq!(after - before, 0, "allocations while measuring");
        let shown: Vec<&str> = lines[..n].iter().map(Line::as_str).collect();
        // Nearest rank of 1..=200: p50 is the 100th, p99 the 198th.
        assert_eq!(shown[1], "INPUT>JUDGE US P50 100 P99 198 MAX 200");
        // 20 000 + 100 v: the 50th is 24 900, the 99th 29 800.
        assert_eq!(shown[3], "PRESS>SOUND MS P50 24.9 P99 29.8 MAX 29.9");
        assert_eq!(shown[4], " PERIOD 10.0 MS, 2 QUEUED");
        assert_eq!(shown[7], "LOST REFRESHES 3 IN 1000 LONGEST 2 N/A 4");
        assert_eq!(shown[9], "UNDERRUNS 1");
        assert_eq!(shown[10], "LOAD RETRY 85 MS");
        assert_eq!(shown[11], "ALLOC IN 0 LOGIC 1 AUDIO 2 RENDER 3");
    }
}

#[cfg(test)]
mod frame_tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use bms_model::{Lane, TimeUs};
    use eval::{JudgeEvent, JudgeEventKind};
    use input::QpcClock;
    use render::FrameTiming;
    use render::measure::FrameStatistics;
    use rules::Judgment;

    use super::{Drawn, Measure, PlayMeasure, StepSample, measured_period_ns};

    /// 59.951 Hz at a 10 MHz QPC: 600 refreshes take 100 082 567 ticks,
    /// 16 680 427 ns each (by hand: 100 082 567 × 100 ÷ 600, floored).
    /// Syncs closer than 60 refreshes give no period.
    #[test]
    fn the_refresh_period_is_measured_from_syncs() {
        let f = 10_000_000;
        assert_eq!(
            measured_period_ns((5_000, 1_000), (5_000 + 100_082_567, 1_600), f),
            Some(16_680_427)
        );
        assert_eq!(measured_period_ns((0, 1_000), (980_000, 1_059), f), None);
        assert_eq!(
            measured_period_ns((0, 1_000), (1_000_000, 1_060), f),
            Some(1_666_666)
        );
    }

    fn judged(seq: u64, origin: i64) -> JudgeEvent {
        JudgeEvent {
            seq,
            time: TimeUs(origin),
            origin: Some(TimeUs(origin)),
            kind: JudgeEventKind::Judged {
                lane: Lane::new(1).unwrap(),
                note: 0,
                judgment: Judgment::Great,
                deviation: Some(TimeUs(0)),
                combo: 1,
            },
        }
    }

    fn drawn<'a>(
        effects: &'a [JudgeEvent],
        count: u32,
        cpu_us: u64,
        statistics: Option<FrameStatistics>,
        at: i64,
    ) -> Drawn<'a> {
        Drawn {
            play: Some(TimeUs(0)),
            timing: FrameTiming {
                cpu: Duration::from_micros(cpu_us),
                sprites: 0,
                dropped: 0,
                presented: true,
                present_count: Some(count),
            },
            statistics,
            effects,
            at: TimeUs(at),
        }
    }

    /// The render thread's path: a judgment drawn on the frame presented
    /// as 10 takes that frame's display time from the sample that arrives
    /// with the next frame, step samples of the play count and those of
    /// another play do not, and nothing of it allocates once the play has
    /// started.
    #[test]
    fn frames_pair_judgments_with_their_statistics() {
        let clock = QpcClock::start();
        let (mut steps_tx, steps) = rtrb::RingBuffer::new(8);
        let (_sounds_tx, sounds) = rtrb::RingBuffer::new(8);
        let overlay = Arc::new(AtomicBool::new(true));
        // 250 Hz: 4 ms a refresh.
        let mut m = Measure::new(clock, Some(250_000), None, steps, sounds, overlay);
        let origin = TimeUs(5_000_000);
        m.start(PlayMeasure {
            generation: 3,
            origin,
            load_from: TimeUs(0),
            retry: false,
            underruns: None,
        });
        for (generation, judgment) in [(3, 120), (2, 999_999)] {
            let s = StepSample {
                generation,
                finalization_us: Some(judgment + 1),
                judgment_us: Some(judgment),
            };
            steps_tx.push(s).unwrap();
        }
        let freq = clock.frequency().hz();
        let period = freq * 4 / 1_000;
        // Synced 2 refreshes after present 10 reached the screen.
        let sync = clock_qpc(&clock, TimeUs(5_100_000)) + 2 * period;
        let stats = FrameStatistics {
            present_count: 10,
            present_refresh_count: 500,
            sync_refresh_count: 502,
            sync_qpc_time: sync,
        };
        let effects = [judged(7, 60_000)];
        #[cfg(feature = "dev-tools")]
        let before = crate::alloc_count::this_thread();
        m.frame(&drawn(&effects, 10, 300, None, 5_090_000));
        let _ = m.overlay();
        m.frame(&drawn(&[], 11, 500, Some(stats), 5_095_000));
        let _ = m.overlay();
        #[cfg(feature = "dev-tools")]
        assert_eq!(crate::alloc_count::this_thread() - before, 0);
        let fig = m.finish(TimeUs(5_094_000)).unwrap();
        // Displayed at 5.1 s on the clock, 0.1 s into the chart; pressed at
        // 60 ms: 40 ms.
        let s = fig.press_to_screen.unwrap();
        assert_eq!((s.count, s.p50), (1, 40_000));
        assert_eq!(fig.press_to_screen_unavailable, 0);
        let j = fig.input_to_judgment.unwrap();
        assert_eq!((j.count, j.max), (1, 120));
        assert_eq!(fig.finalization.unwrap().max, 121);
        let cpu = fig.render_cpu.unwrap();
        assert_eq!((cpu.count, cpu.max), (2, 500));
        // The second present came after the end.
        assert_eq!(fig.presents, 1);
        assert_eq!(fig.span, (origin, TimeUs(5_094_000)));
        // Taken once: nothing more counts.
        assert_eq!(m.finish(TimeUs(5_200_000)), None);
    }

    /// The raw QPC value that `clock` reads as `t`.
    fn clock_qpc(clock: &QpcClock, t: TimeUs) -> i64 {
        let freq = clock.frequency().hz();
        // Search from an estimate: from_qpc floors, so step up to t.
        let base = clock.from_qpc(0).0;
        let mut ticks =
            i64::try_from(i128::from(t.0 - base) * i128::from(freq) / 1_000_000).unwrap();
        while clock.from_qpc(ticks).0 < t.0 {
            ticks += 1;
        }
        while clock.from_qpc(ticks - 1).0 >= t.0 {
            ticks -= 1;
        }
        ticks
    }
}
