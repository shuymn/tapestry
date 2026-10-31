//! The measurement of design.md "Latency Figures" on the game's threads: the
//! render thread's [`Measure`] keeps each play's figures in preallocated
//! whole-play histograms, fed by its own frames (render CPU work, frame
//! statistics, the frame that first drew each judgment) and by queues from the
//! logic step (input to judgment, finalization delay) and the audio thread
//! (press to sound scheduled). It draws them in the overlay while the overlay
//! key shows it and hands them to the game thread when the play ends, which
//! writes them next to the saved record as `<record>.measure.toml`.
//!
//! Every sample carries the play's generation, and samples of another
//! play are dropped, so a play never counts its neighbor's. Nothing here
//! allocates while a play is measured; the figures handed over at its end
//! are built then.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bms_model::{TimeSource, TimeUs};
use eval::{JudgeEvent, JudgeEventKind};
use game::{AudioCounters, AudioProbe, Probed};
use input::QpcClock;
use render::FrameTiming;
use render::measure::{
    FrameStatistics, OnScreen, Presentation, RefreshCount, RefreshLedger, Samples, StatsFilter,
    Summary, display_qpc,
};

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
const LINES: usize = 13;
const LINE_LEN: usize = 56;
/// Presents remembered to take the ones after a play's end back out of its
/// count.
const RECENT: usize = 16;

/// What the logic step measured, for the render thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepSample {
    /// The play it belongs to.
    pub generation: u64,
    /// The figure and its value.
    pub kind: Step,
}

/// One logic step figure, µs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A step's logic time minus its watermark.
    Finalization(i64),
    /// A judgment's delivery time minus its press's origin, one per judgment
    /// of a press.
    Judgment(i64),
}

/// The input → judgment samples of one step's `events`, delivered at
/// play time `delivered`: one per judgment of a press.
pub fn judgment_samples(
    events: &[JudgeEvent],
    delivered: TimeUs,
) -> impl Iterator<Item = i64> + '_ {
    events
        .iter()
        .filter_map(move |e| press_origin(e).map(|o| delivered.0 - o.0))
}

/// The receive time of the press `e` judges: a judgment a press made, or a
/// long note's head held. A long note judged by its release or its end is
/// no press's judgment, so it gives no press latency.
fn press_origin(e: &JudgeEvent) -> Option<TimeUs> {
    match e.kind {
        JudgeEventKind::Judged { release: None, .. } | JudgeEventKind::LnHeld { .. } => e.origin,
        _ => None,
    }
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
    /// When the countdown started (recording opened), on the clock.
    pub countdown_at: TimeUs,
    /// When the play was set up and about to be handed to audio and the
    /// screen, on the clock: its setup ran from `countdown_at` to here.
    pub started: TimeUs,
    /// The process's page fault count at `started`; `None` when it could
    /// not be read.
    pub page_faults: Option<u32>,
    /// What the load time runs from: the load's start for the first play,
    /// the retry's receipt for a retry.
    pub load_from: TimeUs,
    /// Whether the play is a retry.
    pub retry: bool,
    /// The audio thread's counters when the play began; `None` without an
    /// audio thread.
    pub audio: Option<AudioBase>,
    /// Periods the output keeps queued for this play, when it says; it
    /// replaces the figure the output opened with.
    pub queued_periods: Option<u32>,
}

/// The audio thread's counters and what they held when a play began, so
/// the play's figures are their growth since.
#[derive(Clone, Debug)]
pub struct AudioBase {
    counters: AudioCounters,
    underruns: u64,
    probes_dropped: u64,
    deferred_wakes: u64,
    steals: u64,
}

impl AudioBase {
    /// The counters' values now.
    pub fn new(counters: AudioCounters) -> Self {
        Self {
            underruns: counters.underruns(),
            probes_dropped: counters.probes_dropped(),
            deferred_wakes: counters.deferred_wakes(),
            steals: counters.steals(),
            counters,
        }
    }
}

/// The countdown's figures, µs from its start (recording opened).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Countdown {
    /// Until the play was set up, as it was handed to audio and the screen.
    pub setup_us: i64,
    /// Until the clock read once the play's first frame was presented;
    /// `None` when none was.
    pub first_frame_us: Option<i64>,
}

/// The process's page fault count now (`GetProcessMemoryInfo`); `None`
/// when it cannot be read.
pub fn process_page_faults() -> Option<u32> {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let cb = u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?;
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb,
        ..PROCESS_MEMORY_COUNTERS::default()
    };
    // SAFETY: the current process's pseudo handle needs no closing, and
    // `counters` is a writable `PROCESS_MEMORY_COUNTERS` of `cb` bytes.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, cb) }.ok()?;
    Some(counters.PageFaultCount)
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
    /// Render CPU work per frame, µs: the renderer's update, build and
    /// submit, waits excluded; the overlay text and the measurement's own
    /// bookkeeping are not in it.
    pub render_cpu: Option<Summary>,
    /// Audio underruns during the play.
    pub underruns: Option<u64>,
    /// Press → sound samples the full probe queue dropped during the play.
    pub sounds_dropped: Option<u64>,
    /// Audio wakes during the play that left a due start for a later wake,
    /// the mixer's pending starts full: those sounds started late.
    pub deferred_wakes: Option<u64>,
    /// Voices the mixer stole at its voice limit during the play.
    pub voices_stolen: Option<u64>,
    /// Play frames whose sprite batch filled, leaving the rest of the frame
    /// undrawn.
    pub sprite_full_frames: u64,
    /// How the swap chain presented, when known.
    pub presentation: Option<Presentation>,
    /// When each presented frame of the play was done, µs on the clock,
    /// when they were kept (`measure play`) and all fitted.
    #[cfg(feature = "dev-tools")]
    pub present_times: Option<Vec<i64>>,
    /// The play span on the clock, from chart time 0 to the play's end.
    /// The countdown is left out: a display capture misses a swap chain's
    /// first present, which is the countdown's first frame.
    pub span: (TimeUs, TimeUs),
    /// Presents the game issued in the span.
    pub presents: u64,
    /// Allocations per thread during the play: input, logic, audio,
    /// render (dev-tools). Whole threads, diagnostic only: the render
    /// thread's include wgpu, the end screen and the frame dump.
    pub allocations: Option<[u64; 4]>,
    /// Allocations inside the render thread's app region during the play
    /// (dev-tools): its own per-frame work, which allocates nothing
    /// (design.md "Scope of No Allocation").
    pub render_app_allocations: Option<u64>,
    /// Logic step samples the full queue refused: input to judgment and
    /// finalization delay are missing that many.
    pub steps_dropped: u64,
    /// The countdown's setup and first frame.
    pub countdown: Option<Countdown>,
    /// Page faults of the whole process from the play's handover to audio
    /// and the screen until it was over (diagnostic: every thread, wgpu and
    /// the driver included).
    pub page_faults: Option<u64>,
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
    // 20 Hz to 1 kHz only.
    i64::try_from(ns)
        .ok()
        .filter(|ns| (1_000_000..=50_000_000).contains(ns))
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

    #[expect(
        clippy::indexing_slicing,
        reason = "`write_str` keeps `len` at most LINE_LEN"
    )]
    fn as_str(&self) -> &str {
        // Only ASCII is written.
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len < LINE_LEN {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "the check above keeps `len` below LINE_LEN"
                )]
                {
                    self.buf[self.len] = b;
                }
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
            #[expect(
                clippy::indexing_slicing,
                reason = "the check above keeps `n` below LINES"
            )]
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
        "RENDER CPU US (DRAW) {}",
        Percentiles(&fig.render_cpu, false)
    ));
    line(format_args!(
        " SPRITE CAP FULL {} FRAMES",
        fig.sprite_full_frames
    ));
    match (fig.underruns, fig.deferred_wakes, fig.voices_stolen) {
        (Some(n), Some(d), Some(s)) => {
            line(format_args!("UNDERRUNS {n} DEFERRED {d} STOLEN {s}"));
        }
        _ => line(format_args!("UNDERRUNS N/A")),
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
    /// The chart time of a play frame, `None` for a text or menu screen.
    pub play: Option<TimeUs>,
    /// Whether it was a menu screen (song selection), for the frame dump
    /// (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub menu: bool,
    /// The generation of the play whose figures the frame counts in, until
    /// they are taken (dev-tools: a scenario's captures).
    #[cfg(feature = "dev-tools")]
    pub generation: Option<u64>,
    /// What the frame cost and whether it was presented.
    pub timing: FrameTiming,
    /// The swap chain's frame statistics, read right after the frame.
    pub statistics: Option<FrameStatistics>,
    /// How the swap chain presents, when it says.
    pub presentation: Option<Presentation>,
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
    presentation: Option<Presentation>,
    /// Present times kept for a display capture (`measure play`): none
    /// kept when its capacity is 0.
    #[cfg(feature = "dev-tools")]
    present_log: Vec<i64>,
    /// Whether the play's presents outgrew the log.
    #[cfg(feature = "dev-tools")]
    present_log_full: bool,
    /// The last plausible measured refresh period, QPC ticks.
    measured_qpc: Option<i64>,
    steps: rtrb::Consumer<StepSample>,
    sounds: rtrb::Consumer<AudioProbe>,
    overlay: Arc<AtomicBool>,
    /// The play being measured; `None` before the first play and once the
    /// game thread took the figures, when nothing counts.
    play: Option<PlayMeasure>,
    input_to_judgment: Samples,
    finalization: Samples,
    press_to_sound: Samples,
    render_cpu: Samples,
    sprite_full_frames: u64,
    on_screen: OnScreen,
    filter: StatsFilter,
    refreshes: RefreshLedger,
    /// The last present count, to notice a new swap chain.
    last_present: Option<u32>,
    /// When the play's first frame was presented, on the clock: the load
    /// time and the countdown's first frame run to it.
    first_frame: Option<TimeUs>,
    presents: u64,
    recent: [i64; RECENT],
    recent_next: usize,
    lines: [Line; LINES],
    shown: usize,
    refreshed_at: Option<TimeUs>,
    /// The allocation counts when the play started.
    alloc_base: Option<[u64; 4]>,
    /// This thread's app-region allocations when the play started: the
    /// render thread starts and finishes each play.
    app_base: Option<u64>,
    /// Whether each play frame's measurement allocates, for the render
    /// thread's app region tests.
    #[cfg(all(test, feature = "dev-tools"))]
    allocate_in_frames: bool,
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
            presentation: None,
            #[cfg(feature = "dev-tools")]
            present_log: Vec::new(),
            #[cfg(feature = "dev-tools")]
            present_log_full: false,
            measured_qpc: None,
            steps,
            sounds,
            overlay,
            play: None,
            input_to_judgment: Samples::new(),
            finalization: Samples::new(),
            press_to_sound: Samples::new(),
            render_cpu: Samples::new(),
            sprite_full_frames: 0,
            on_screen: OnScreen::with_capacity(IN_FLIGHT),
            filter: StatsFilter::default(),
            refreshes: RefreshLedger::default(),
            last_present: None,
            first_frame: None,
            presents: 0,
            recent: [i64::MIN; RECENT],
            recent_next: 0,
            lines: [Line::EMPTY; LINES],
            shown: 0,
            refreshed_at: None,
            alloc_base: None,
            app_base: None,
            #[cfg(all(test, feature = "dev-tools"))]
            allocate_in_frames: false,
        }
    }

    /// A play starts: every figure starts over.
    pub fn start(&mut self, play: PlayMeasure) {
        self.input_to_judgment.clear();
        self.finalization.clear();
        self.press_to_sound.clear();
        self.render_cpu.clear();
        self.sprite_full_frames = 0;
        self.on_screen.clear();
        self.filter.reset();
        self.refreshes.clear();
        self.last_present = None;
        self.first_sync = None;
        self.last_sync = None;
        self.first_frame = None;
        self.presents = 0;
        self.recent = [i64::MIN; RECENT];
        self.refreshed_at = None;
        self.presentation = None;
        #[cfg(feature = "dev-tools")]
        {
            self.present_log.clear();
            self.present_log_full = false;
        }
        self.measured_qpc = None;
        self.alloc_base = allocations();
        self.app_base = app_allocations();
        if let (Some(audio), Some(q)) = (self.audio.as_mut(), play.queued_periods) {
            audio.queued_periods = Some(q);
        }
        self.play = Some(play);
        self.drain();
    }

    /// The measured play's generation, until its figures are taken
    /// (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn generation(&self) -> Option<u64> {
        self.play.as_ref().map(|p| p.generation)
    }

    /// Takes the queued samples of the current play; others are dropped.
    fn drain(&mut self) {
        let generation = self.play.as_ref().map(|p| p.generation);
        while let Ok(s) = self.steps.pop() {
            if Some(s.generation) != generation {
                continue;
            }
            match s.kind {
                Step::Judgment(v) => self.input_to_judgment.push(v),
                Step::Finalization(v) => self.finalization.push(v),
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
        if !self.overlay.load(Ordering::Acquire) || self.play.is_none() {
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
                .play
                .as_ref()
                .zip(self.first_frame)
                .map(|(p, t)| (p.retry, (t.0 - p.load_from.0) / 1_000));
            self.shown = write_overlay(&fig, load, &mut self.lines);
        }
        for (o, l) in out.iter_mut().zip(self.lines.iter().take(self.shown)) {
            *o = l.as_str();
        }
        out
    }

    /// The figures so far, without freezing (no allocation).
    fn figures_now(&mut self, end: TimeUs) -> Figures {
        let audio = self.play.as_ref().and_then(|p| p.audio.as_ref());
        let underruns = audio.map(|a| a.counters.underruns().saturating_sub(a.underruns));
        let sounds_dropped =
            audio.map(|a| a.counters.probes_dropped().saturating_sub(a.probes_dropped));
        let deferred_wakes =
            audio.map(|a| a.counters.deferred_wakes().saturating_sub(a.deferred_wakes));
        let voices_stolen = audio.map(|a| a.counters.steals().saturating_sub(a.steals));
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
            sounds_dropped,
            deferred_wakes,
            voices_stolen,
            sprite_full_frames: self.sprite_full_frames,
            presentation: self.presentation,
            #[cfg(feature = "dev-tools")]
            present_times: None,
            span: (start, end),
            presents: self.presents.saturating_sub(after_end),
            allocations: allocations().zip(self.alloc_base).map(|(now, base)| {
                let mut delta = now;
                for (d, b) in delta.iter_mut().zip(base) {
                    *d = d.saturating_sub(b);
                }
                delta
            }),
            render_app_allocations: app_allocations()
                .zip(self.app_base)
                .map(|(now, base)| now.saturating_sub(base)),
            steps_dropped: 0,
            countdown: self.play.as_ref().map(|p| Countdown {
                setup_us: p.started.0 - p.countdown_at.0,
                first_frame_us: self.first_frame.map(|t| t.0 - p.countdown_at.0),
            }),
            page_faults: None,
            refresh_period_ns: self
                .first_sync
                .zip(self.last_sync)
                .and_then(|(f, l)| measured_period_ns(f, l, self.clock.frequency().hz()))
                .or_else(|| self.refresh_mhz.map(|m| 1_000_000_000_000 / i64::from(m))),
        }
    }

    /// Makes each play frame's measurement allocate, as a fault for the
    /// render thread's app region tests to find.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn allocate_in_frames(&mut self) {
        self.allocate_in_frames = true;
    }

    /// Keeps the times of up to `capacity` presents a play, for a display
    /// capture to be checked against (`measure play`).
    #[cfg(feature = "dev-tools")]
    pub fn keep_presents(&mut self, capacity: usize) {
        self.present_log = Vec::with_capacity(capacity);
    }

    /// The play ended at `end` on the clock: its figures, after which
    /// nothing more counts until the next play. Judgments whose frame has
    /// no sample yet are N/A.
    pub fn finish(&mut self, end: TimeUs) -> Option<Figures> {
        self.play.as_ref()?;
        self.drain();
        self.on_screen.finish();
        let mut fig = self.figures_now(end);
        fig.page_faults = self
            .play
            .as_ref()
            .and_then(|p| p.page_faults)
            .zip(process_page_faults())
            .map(|(base, now)| u64::from(now.wrapping_sub(base)));
        #[cfg(feature = "dev-tools")]
        {
            let kept = self.present_log.capacity() > 0 && !self.present_log_full;
            fig.present_times = kept.then(|| self.present_log.clone());
        }
        self.play = None;
        Some(fig)
    }

    /// The play ended with `effects` not yet drawn: they never reached the
    /// screen in it, and are N/A as a failed frame's are.
    pub fn never_drawn(&mut self, effects: &[JudgeEvent]) {
        if !effects.is_empty() {
            self.failed(effects);
        }
    }

    /// The frame being drawn failed (the screen is lost): the judgments it
    /// drew never reached the screen through it and are N/A.
    pub fn failed(&mut self, effects: &[JudgeEvent]) {
        if self.play.is_none() {
            return;
        }
        for ev in effects {
            if let Some(o) = press_origin(ev) {
                self.on_screen.drawn(ev.seq, o.0);
            }
        }
        self.on_screen.presented_unknown();
    }

    /// One frame drawn.
    pub fn frame(&mut self, d: &Drawn<'_>) {
        let Some(play) = &self.play else { return };
        if d.play.is_none() {
            return;
        }
        let origin = play.origin;
        #[cfg(all(test, feature = "dev-tools"))]
        if self.allocate_in_frames {
            drop(std::hint::black_box(vec![0u8; 8]));
        }
        self.drain();
        if let Some(count) = d.timing.present_count.filter(|_| d.timing.presented)
            && self.last_present.is_some_and(|last| count <= last)
        {
            // A new swap chain counts again from the start: what the old
            // one presented and has no sample yet never will.
            self.on_screen.finish();
            self.filter.reset();
        }
        for ev in d.effects {
            if let Some(o) = press_origin(ev) {
                self.on_screen.drawn(ev.seq, o.0);
            }
        }
        self.render_cpu
            .push(i64::try_from(d.timing.cpu.as_micros()).unwrap_or(i64::MAX));
        if d.timing.dropped > 0 {
            self.sprite_full_frames += 1;
        }
        if !d.timing.presented {
            return;
        }
        if d.presentation.is_some() {
            self.presentation = d.presentation;
        }
        #[cfg(feature = "dev-tools")]
        if self.present_log.len() < self.present_log.capacity() {
            self.present_log.push(d.at.0);
        } else if self.present_log.capacity() > 0 {
            // Full: the capture cannot be checked against it. The log stays
            // for the next play.
            self.present_log_full = true;
        }
        if self.first_frame.is_none() {
            self.first_frame = Some(d.at);
        }
        if d.at >= origin {
            self.presents += 1;
            #[expect(
                clippy::indexing_slicing,
                reason = "the modulo below keeps `recent_next` below RECENT"
            )]
            {
                self.recent[self.recent_next] = d.at.0;
            }
            self.recent_next = (self.recent_next + 1) % RECENT;
        }
        match d.timing.present_count {
            Some(count) => {
                self.last_present = Some(count);
                self.on_screen.presented(count);
            }
            // No present count to pair this frame with: what it drew is
            // N/A, while the statistics it carries still resolve earlier
            // frames.
            None => self.on_screen.presented_unknown(),
        }
        if let Some(s) = self.filter.accept(d.statistics) {
            let sync = (s.sync_qpc_time, s.sync_refresh_count);
            // A sync whose refresh count disagrees with the time since the
            // last one by more than a quarter of the reported refresh, or of
            // the period measured so far without one (the count of a
            // composed window and of independent flip differ), starts the
            // period over from it.
            if let (Some(last), Some(rough)) =
                (self.last_sync, self.period_qpc.or(self.measured_qpc))
            {
                let n = i64::from(sync.1.wrapping_sub(last.1));
                if n <= 0
                    || n > i64::from(u32::MAX / 2)
                    || ((sync.0 - last.0) - n * rough).abs() > n * rough / 4
                {
                    self.first_sync = Some(sync);
                }
            }
            let first = *self.first_sync.get_or_insert(sync);
            self.last_sync = Some(sync);
            // The measured period once the syncs span enough refreshes; the
            // monitor's reported rate may be rounded to a whole hertz.
            let refreshes = sync.1.wrapping_sub(first.1);
            let freq = self.clock.frequency().hz();
            let measured = (PERIOD_SPAN..=u32::MAX / 2)
                .contains(&refreshes)
                .then(|| (sync.0 - first.0) / i64::from(refreshes));
            let measured = match measured {
                // Refresh rates of 20 Hz to 1 kHz only; anything else came
                // from counts that do not belong together.
                Some(t) if (freq / 1_000..=freq / 20).contains(&t) => Some(t),
                Some(_) => {
                    self.first_sync = Some(sync);
                    None
                }
                None => None,
            };
            if measured.is_some() {
                self.measured_qpc = measured;
            }
            let period = measured.or(self.measured_qpc).or(self.period_qpc);
            // Lost refreshes count only under a confirmed countable
            // presentation (vsync every refresh, no tearing).
            let countable = self.presentation.is_some_and(Presentation::countable);
            self.refreshes.accepted(&s, period.filter(|_| countable));
            if let Some(period) = period {
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

#[cfg(feature = "dev-tools")]
fn app_allocations() -> Option<u64> {
    Some(crate::alloc_count::app_allocations())
}

#[cfg(not(feature = "dev-tools"))]
const fn app_allocations() -> Option<u64> {
    None
}

/// The console's line of what a play lost to the sprite and mixer caps
/// (design.md "Latency Figures"); N/A without an audio thread.
pub fn limits_line(fig: &Figures) -> String {
    let na = |n: Option<u64>| n.map_or_else(|| "N/A".to_owned(), |n| n.to_string());
    format!(
        "limits: sprite cap full in {} frames; starts deferred in {} audio wakes; {} voices stolen",
        fig.sprite_full_frames,
        na(fig.deferred_wakes),
        na(fig.voices_stolen)
    )
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
    let mut out = String::from(
        "# Measurement of the play (design.md \"Latency Figures\"): numbers only, N/A left out.\n",
    );
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
    if let Some(n) = fig.sounds_dropped {
        let _ = writeln!(out, "dropped = {n}");
    }
    summary_toml(&mut out, "press_to_screen_us", fig.press_to_screen.as_ref());
    let _ = writeln!(out, "unavailable = {}", fig.press_to_screen_unavailable);
    let r = fig.refreshes;
    out.push_str("\n[lost_refreshes_frame_statistics]\n");
    if let Some(p) = fig.presentation {
        let _ = writeln!(
            out,
            "sync_interval = {}\ntearing = {}",
            p.sync_interval,
            u8::from(p.tearing)
        );
    }
    if let Some(lost) = r.lost() {
        let _ = writeln!(out, "lost = {lost}\nlongest = {}", r.longest);
    }
    let _ = writeln!(
        out,
        "intervals = {}\nunavailable_intervals = {}",
        r.counted, r.unavailable
    );
    summary_toml(&mut out, "render_cpu_us", fig.render_cpu.as_ref());
    let _ = writeln!(
        out,
        "\n[sprite_cap]\nfull_frames = {}",
        fig.sprite_full_frames
    );
    out.push_str("\n[audio]\n");
    if let Some(n) = fig.underruns {
        let _ = writeln!(out, "underruns = {n}");
    }
    if let Some(n) = fig.deferred_wakes {
        let _ = writeln!(out, "deferred_wakes = {n}");
    }
    if let Some(n) = fig.voices_stolen {
        let _ = writeln!(out, "voices_stolen = {n}");
    }
    out.push_str("\n[load_ms]\n");
    if let Some(ms) = load.0 {
        let _ = writeln!(out, "first = {ms}");
    }
    if let Some(ms) = load.1 {
        let _ = writeln!(out, "retry = {ms}");
    }
    out.push_str("\n[countdown]\n");
    if let Some(c) = fig.countdown {
        let _ = writeln!(out, "setup_us = {}", c.setup_us);
        if let Some(us) = c.first_frame_us {
            let _ = writeln!(out, "first_frame_us = {us}");
        }
    }
    out.push_str("\n[faults]\n");
    if let Some(n) = fig.page_faults {
        let _ = writeln!(out, "process = {n}");
    }
    if let Some([i, l, a, r]) = fig.allocations {
        let _ = writeln!(
            out,
            "\n[allocations]\ninput = {i}\nlogic = {l}\naudio = {a}\nrender = {r}"
        );
        if let Some(app) = fig.render_app_allocations {
            let _ = writeln!(out, "render_app = {app}");
        }
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
            (8, "RENDER CPU US (DRAW) N/A"),
            (9, " SPRITE CAP FULL 0 FRAMES"),
            (10, "UNDERRUNS N/A"),
            (11, "LOAD N/A"),
        ] {
            assert_eq!(lines[i], want);
        }
    }

    /// The countdown's setup and first frame and the process's page faults
    /// are written in their own tables; a figure without evidence has no
    /// key.
    #[test]
    fn to_toml_writes_the_countdown_and_fault_figures() {
        use super::{Countdown, to_toml};
        let fig = Figures {
            countdown: Some(Countdown {
                setup_us: 2_345,
                first_frame_us: Some(18_000),
            }),
            page_faults: Some(812),
            ..Figures::default()
        };
        let t: toml::Table = to_toml(&fig, (None, None)).parse().unwrap();
        let int = |table: &str, key: &str| t[table].get(key).and_then(toml::Value::as_integer);
        assert_eq!(int("countdown", "setup_us"), Some(2_345));
        assert_eq!(int("countdown", "first_frame_us"), Some(18_000));
        assert_eq!(int("faults", "process"), Some(812));

        let fig = Figures {
            countdown: Some(Countdown {
                setup_us: 2_345,
                first_frame_us: None,
            }),
            ..Figures::default()
        };
        let t: toml::Table = to_toml(&fig, (None, None)).parse().unwrap();
        assert_eq!(t["countdown"].as_table().unwrap().len(), 1);
        assert!(t["faults"].as_table().unwrap().is_empty());
        let t: toml::Table = to_toml(&Figures::default(), (None, None)).parse().unwrap();
        assert!(t["countdown"].as_table().unwrap().is_empty());
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
        let mut judge = Samples::new();
        let mut sound = Samples::new();
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
            deferred_wakes: Some(2),
            voices_stolen: Some(3),
            sprite_full_frames: 4,
            allocations: Some([0, 1, 2, 3]),
            ..Figures::default()
        };
        let n = write_overlay(&fig, Some((true, 85)), &mut lines);
        let after = this_thread();
        assert_eq!(after - before, 0, "allocations while measuring");
        let shown: Vec<&str> = lines[..n].iter().map(Line::as_str).collect();
        // Nearest rank of 1..=200: p50 is 100, p99 198, read as the middle
        // of their buckets: [100, 102) and [196, 200).
        assert_eq!(shown[1], "INPUT>JUDGE US P50 101 P99 198 MAX 200");
        // 20 000 + 100 v: the 50th, 24 900, in [24 576, 25 088); the 99th,
        // 29 800, in [29 696, 30 208), whose middle is above the maximum.
        assert_eq!(shown[3], "PRESS>SOUND MS P50 24.8 P99 29.9 MAX 29.9");
        assert_eq!(shown[4], " PERIOD 10.0 MS, 2 QUEUED");
        assert_eq!(shown[7], "LOST REFRESHES 3 IN 1000 LONGEST 2 N/A 4");
        assert_eq!(shown[9], " SPRITE CAP FULL 4 FRAMES");
        assert_eq!(shown[10], "UNDERRUNS 1 DEFERRED 2 STOLEN 3");
        assert_eq!(shown[11], "LOAD RETRY 85 MS");
        assert_eq!(shown[12], "ALLOC IN 0 LOGIC 1 AUDIO 2 RENDER 3");
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
    use render::measure::{FrameStatistics, Presentation};
    use rules::Judgment;

    use super::{Drawn, Measure, PlayMeasure, Step, StepSample, measured_period_ns};

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
                release: None,
                combo: 1,
            },
        }
    }

    #[test]
    fn press_samples_come_from_held_heads_and_press_judgments_only() {
        let lane = Lane::new(1).unwrap();
        let with = |e: JudgeEvent, origin: Option<i64>, kind| JudgeEvent {
            origin: origin.map(TimeUs),
            kind,
            ..e
        };
        let events = [
            // A long note's head held, pressed at 10 ms.
            with(
                judged(0, 10_000),
                Some(10_000),
                JudgeEventKind::LnHeld {
                    lane,
                    note: 0,
                    judgment: Judgment::Great,
                    deviation: TimeUs(0),
                },
            ),
            // Its judgment at the release (20 ms): no press latency.
            with(
                judged(1, 20_000),
                Some(20_000),
                JudgeEventKind::Judged {
                    lane,
                    note: 0,
                    judgment: Judgment::Good,
                    deviation: Some(TimeUs(0)),
                    release: Some(TimeUs(-150_000)),
                    combo: 1,
                },
            ),
            // A tap pressed at 30 ms, and a miss with no origin.
            judged(2, 30_000),
            with(
                judged(3, 40_000),
                None,
                JudgeEventKind::Judged {
                    lane,
                    note: 1,
                    judgment: Judgment::Poor,
                    deviation: None,
                    release: None,
                    combo: 0,
                },
            ),
        ];
        let samples: Vec<i64> = super::judgment_samples(&events, TimeUs(50_000)).collect();
        assert_eq!(samples, [40_000, 20_000]);
    }

    const FIFO: Option<Presentation> = Some(Presentation {
        sync_interval: 1,
        tearing: false,
    });

    fn drawn(
        effects: &[JudgeEvent],
        count: Option<u32>,
        cpu_us: u64,
        statistics: Option<FrameStatistics>,
        at: i64,
    ) -> Drawn<'_> {
        Drawn {
            play: Some(TimeUs(0)),
            #[cfg(feature = "dev-tools")]
            menu: false,
            #[cfg(feature = "dev-tools")]
            generation: Some(1),
            timing: FrameTiming {
                cpu: Duration::from_micros(cpu_us),
                sprites: 0,
                dropped: 0,
                presented: true,
                present_count: count,
            },
            statistics,
            presentation: FIFO,
            effects,
            at: TimeUs(at),
        }
    }

    fn play(generation: u64, origin: TimeUs) -> PlayMeasure {
        PlayMeasure {
            generation,
            origin,
            countdown_at: TimeUs(origin.0 - 1_000_000),
            started: TimeUs(origin.0 - 990_000),
            page_faults: None,
            load_from: TimeUs(0),
            retry: false,
            audio: None,
            queued_periods: None,
        }
    }

    /// Each play shows the periods its output keeps queued, which may
    /// differ from what the output opened with.
    #[test]
    fn each_play_shows_the_queue_it_played_with() {
        use super::AudioInfo;
        let clock = QpcClock::start();
        let (_steps, steps) = rtrb::RingBuffer::new(1);
        let (_sounds, sounds) = rtrb::RingBuffer::new(1);
        let opened = AudioInfo {
            period_us: 9_166,
            queued_periods: Some(2),
        };
        let overlay = Arc::new(AtomicBool::new(true));
        let mut m = Measure::new(clock, None, Some(opened), steps, sounds, overlay);
        let mut first = play(1, TimeUs(1_000_000));
        first.queued_periods = Some(3);
        m.start(first);
        let fig = m.finish(TimeUs(1_100_000)).unwrap();
        assert_eq!(
            fig.audio,
            Some(AudioInfo {
                period_us: 9_166,
                queued_periods: Some(3),
            })
        );
        let mut lines = [super::Line::EMPTY; super::LINES];
        let n = super::write_overlay(&fig, None, &mut lines);
        assert!(
            lines[..n]
                .iter()
                .any(|l| l.as_str() == " PERIOD 9.1 MS, 3 QUEUED"),
            "{:?}",
            lines[..n]
                .iter()
                .map(super::Line::as_str)
                .collect::<Vec<_>>()
        );
        // A play whose output cannot say keeps the last figure.
        m.start(play(2, TimeUs(2_000_000)));
        let fig = m.finish(TimeUs(2_100_000)).unwrap();
        assert_eq!(fig.audio.unwrap().queued_periods, Some(3));
    }

    struct Rig {
        clock: QpcClock,
        m: Measure,
        steps: rtrb::Producer<StepSample>,
    }

    fn rig() -> Rig {
        // 250 Hz: 4 ms a refresh.
        rig_at(Some(250_000))
    }

    fn rig_at(refresh_mhz: Option<u32>) -> Rig {
        let clock = QpcClock::start();
        let (steps, rx) = rtrb::RingBuffer::new(16);
        let (_sounds, sounds) = rtrb::RingBuffer::new(8);
        let overlay = Arc::new(AtomicBool::new(true));
        #[cfg_attr(not(feature = "dev-tools"), allow(unused_mut))]
        let mut m = Measure::new(clock, refresh_mhz, None, rx, sounds, overlay);
        #[cfg(feature = "dev-tools")]
        m.keep_presents(8);
        Rig { clock, m, steps }
    }

    fn step(generation: u64, kind: Step) -> StepSample {
        StepSample { generation, kind }
    }

    /// The raw QPC value that `clock` reads as `t`.
    fn clock_qpc(clock: &QpcClock, t: TimeUs) -> i64 {
        let freq = clock.frequency().hz();
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

    /// The render thread's path: a judgment drawn on the frame presented
    /// as 10 takes that frame's display time from the sample that arrives
    /// with the next frame, step samples of the play count, a late sample
    /// of an older play does not, and nothing of it allocates once the play
    /// has started.
    #[test]
    fn frames_pair_judgments_with_their_statistics() {
        let mut r = rig();
        let origin = TimeUs(5_000_000);
        r.m.start(play(3, origin));
        for s in [
            step(3, Step::Judgment(120)),
            step(3, Step::Judgment(80)),
            step(3, Step::Finalization(121)),
            step(2, Step::Judgment(999_999)),
        ] {
            r.steps.push(s).unwrap();
        }
        let period = r.clock.frequency().hz() * 4 / 1_000;
        // Synced 2 refreshes after present 10 reached the screen.
        let sync = clock_qpc(&r.clock, TimeUs(5_100_000)) + 2 * period;
        let stats = FrameStatistics {
            present_count: 10,
            present_refresh_count: 500,
            sync_refresh_count: 502,
            sync_qpc_time: sync,
        };
        let effects = [judged(7, 60_000)];
        #[cfg(feature = "dev-tools")]
        let before = crate::alloc_count::this_thread();
        r.m.frame(&drawn(&effects, Some(10), 300, None, 5_090_000));
        let _ = r.m.overlay();
        r.m.frame(&drawn(&[], Some(11), 500, Some(stats), 5_095_000));
        let _ = r.m.overlay();
        #[cfg(feature = "dev-tools")]
        assert_eq!(crate::alloc_count::this_thread() - before, 0);
        let fig = r.m.finish(TimeUs(5_094_000)).unwrap();
        // Displayed at 5.1 s on the clock, 0.1 s into the chart; pressed at
        // 60 ms: 40 ms.
        let s = fig.press_to_screen.unwrap();
        assert_eq!((s.count, s.max), (1, 40_000));
        assert_eq!(fig.press_to_screen_unavailable, 0);
        // Two judgments of play 3; play 2's late one is dropped.
        let j = fig.input_to_judgment.unwrap();
        assert_eq!((j.count, j.max), (2, 120));
        assert_eq!(fig.finalization.unwrap().max, 121);
        let cpu = fig.render_cpu.unwrap();
        assert_eq!((cpu.count, cpu.max), (2, 500));
        // The second present came after the end.
        assert_eq!(fig.presents, 1);
        assert_eq!(fig.span, (origin, TimeUs(5_094_000)));
        assert_eq!(fig.presentation, FIFO);
        #[cfg(feature = "dev-tools")]
        assert_eq!(
            fig.present_times.as_deref(),
            Some(&[5_090_000, 5_095_000][..])
        );
        // Taken once: nothing more counts.
        assert_eq!(r.m.finish(TimeUs(5_200_000)), None);
    }

    /// Two real play starts: the second starts every figure over and takes
    /// only its own samples, including ones queued before it started.
    #[test]
    fn a_second_play_starts_over() {
        let mut r = rig();
        r.m.start(play(1, TimeUs(1_000_000)));
        r.steps.push(step(1, Step::Judgment(500))).unwrap();
        let effects = [judged(1, 0)];
        let mut full = drawn(&effects, Some(3), 100, None, 1_100_000);
        full.timing.dropped = 5;
        r.m.frame(&full);
        r.m.frame(&drawn(&[], Some(4), 100, None, 1_150_000));
        let first = r.m.finish(TimeUs(1_200_000)).unwrap();
        assert_eq!(first.input_to_judgment.unwrap().count, 1);
        // Frames, not refused pushes.
        assert_eq!(first.sprite_full_frames, 1);
        r.steps.push(step(1, Step::Judgment(600))).unwrap();
        r.steps.push(step(2, Step::Judgment(700))).unwrap();
        r.m.start(play(2, TimeUs(2_000_000)));
        r.m.frame(&drawn(&[], Some(5), 100, None, 2_100_000));
        let second = r.m.finish(TimeUs(2_200_000)).unwrap();
        assert_eq!(second.sprite_full_frames, 0);
        let j = second.input_to_judgment.unwrap();
        assert_eq!((j.count, j.max), (1, 700));
        assert_eq!(second.render_cpu.unwrap().count, 1);
        assert_eq!(second.press_to_screen_unavailable, 0);
        #[cfg(feature = "dev-tools")]
        assert_eq!(second.present_times.as_deref(), Some(&[2_100_000][..]));
    }

    /// A swap chain moving from composition to independent flip changes
    /// the refresh count's domain (this PC: 8 605 919 to 1 050 175 581
    /// within one refresh): the period is measured again from there, at
    /// 4 ms, and refreshes count on it, with the monitor's rate reported
    /// or not.
    #[test]
    fn a_jump_in_refresh_counts_measures_the_period_again() {
        jump_at(Some(250_000));
        jump_at(None);
    }

    fn jump_at(refresh_mhz: Option<u32>) {
        let mut r = rig_at(refresh_mhz);
        r.m.start(play(1, TimeUs(1_000_000)));
        let clock = r.clock;
        let ticks = |us: i64| clock_qpc(&clock, TimeUs(us));
        let stats = |present: u32, count: u32, us: i64| FrameStatistics {
            present_count: present,
            present_refresh_count: count,
            sync_refresh_count: count,
            sync_qpc_time: ticks(us),
        };
        let mut at = 1_100_000;
        r.m.frame(&drawn(&[], Some(1), 100, Some(stats(1, 8_605_918, at)), at));
        at += 4_000;
        r.m.frame(&drawn(
            &[],
            Some(2),
            100,
            Some(stats(2, 1_050_175_581, at)),
            at,
        ));
        for k in 3..140_u32 {
            at += 4_000;
            let s = stats(k, 1_050_175_579 + k, at);
            r.m.frame(&drawn(&[], Some(k), 100, Some(s), at));
        }
        // A second, small jump of 30 counts in one refresh: still a period
        // of 4 ms, measured again from it (without a reported rate, judged
        // against the period measured so far).
        for k in 140..280_u32 {
            at += 4_000;
            let s = stats(k, 1_050_175_609 + k, at);
            r.m.frame(&drawn(&[], Some(k), 100, Some(s), at));
        }
        let fig = r.m.finish(TimeUs(at + 1)).unwrap();
        // 4 ms within the clock's 1 µs flooring.
        let period = fig.refresh_period_ns.unwrap();
        assert!((3_990_000..=4_010_000).contains(&period), "{period}");
        // The interval across the jump has no evidence; most after it
        // count (without a reported rate, once 60 refreshes measured the
        // period), none lost.
        assert_eq!(fig.refreshes.lost(), Some(0));
        assert!(fig.refreshes.counted >= 60, "{:?}", fig.refreshes);
    }

    /// More presents than the log holds leave the play without present
    /// times (its capture cannot be checked), and the log, kept, serves
    /// the next play.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_full_present_log_drops_that_play_only() {
        let mut r = rig();
        r.m.start(play(1, TimeUs(1_000_000)));
        for k in 0..9 {
            r.m.frame(&drawn(
                &[],
                Some(k + 1),
                100,
                None,
                1_100_000 + i64::from(k),
            ));
        }
        assert_eq!(r.m.finish(TimeUs(2_000_000)).unwrap().present_times, None);
        r.m.start(play(2, TimeUs(3_000_000)));
        r.m.frame(&drawn(&[], Some(1), 100, None, 3_100_000));
        let fig = r.m.finish(TimeUs(4_000_000)).unwrap();
        assert_eq!(fig.present_times.as_deref(), Some(&[3_100_000][..]));
    }

    /// A frame whose present count cannot be read still carries frame
    /// statistics for an earlier frame: they resolve that frame's
    /// judgment, while the unreadable frame's own judgment is N/A.
    #[test]
    fn statistics_of_a_frame_without_a_present_count_still_count() {
        let mut r = rig();
        r.m.start(play(1, TimeUs(3_000_000)));
        let s20 = FrameStatistics {
            present_count: 20,
            present_refresh_count: 120,
            sync_refresh_count: 120,
            sync_qpc_time: clock_qpc(&r.clock, TimeUs(3_060_000)),
        };
        r.m.frame(&drawn(&[judged(1, 10_000)], Some(20), 100, None, 3_050_000));
        r.m.frame(&drawn(
            &[judged(2, 20_000)],
            None,
            100,
            Some(s20),
            3_054_000,
        ));
        let fig = r.m.finish(TimeUs(3_063_000)).unwrap();
        // 60 ms − 10 ms.
        assert_eq!(
            fig.press_to_screen.map(|s| (s.count, s.max)),
            Some((1, 50_000))
        );
        assert_eq!(fig.press_to_screen_unavailable, 1);
    }

    /// A frame presented without a present count makes only its own
    /// judgments N/A; a frame that failed makes its judgments N/A; a new
    /// swap chain (the present count going back) makes the old chain's
    /// unresolved judgments N/A and starts its frame statistics over.
    #[test]
    fn failed_frames_and_new_swap_chains_are_accounted_for() {
        let mut r = rig();
        r.m.start(play(1, TimeUs(3_000_000)));
        let clock = r.clock;
        let stats = |count: u32, at: i64| FrameStatistics {
            present_count: count,
            present_refresh_count: 100 + count,
            sync_refresh_count: 100 + count,
            sync_qpc_time: clock_qpc(&clock, TimeUs(at)),
        };
        r.m.frame(&drawn(&[judged(1, 10_000)], Some(20), 100, None, 3_050_000));
        r.m.frame(&drawn(&[judged(2, 20_000)], None, 100, None, 3_054_000));
        r.m.failed(&[judged(3, 30_000)]);
        let s20 = stats(20, 3_060_000);
        r.m.frame(&drawn(&[], Some(21), 100, Some(s20), 3_062_000));
        let fig = r.m.figures_now(TimeUs(3_063_000));
        // Judgment 1 kept its pairing: 60 ms − 10 ms.
        assert_eq!(fig.press_to_screen.map(|s| s.max), Some(50_000));
        assert_eq!(fig.press_to_screen_unavailable, 2);
        // A new swap chain: present counts start again.
        r.m.frame(&drawn(&[judged(4, 40_000)], Some(22), 100, None, 3_066_000));
        r.m.frame(&drawn(&[judged(5, 50_000)], Some(1), 100, None, 3_070_000));
        let fig = r.m.figures_now(TimeUs(3_071_000));
        assert_eq!(fig.press_to_screen_unavailable, 3);
        // Present 1's sample is taken after the reset: 74 ms − 50 ms.
        let s1 = stats(1, 3_074_000);
        r.m.frame(&drawn(&[], Some(2), 100, Some(s1), 3_075_000));
        let fig = r.m.finish(TimeUs(3_080_000)).unwrap();
        let s = fig.press_to_screen.unwrap();
        assert_eq!((s.count, s.max), (2, 50_000));
    }
}
