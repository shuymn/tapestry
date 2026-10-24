//! The play threads of a windowed run (design D4): input, logic and, unless
//! `--audio null`, audio and loopback, started together with the render
//! thread behind one play barrier; their records after the run.

use crate::audio::{self, AudioArgs, LoopbackSide, RenderSide};
use crate::run::COUNTDOWN_US;
use crate::threads::{
    self, Event, HitMsg, InputStats, LogicSinks, PlaySync, Snapshot, spawn_input, spawn_logic,
};
use crate::timebase::Clock;
use rtrb::{Consumer, RingBuffer};
use serde::Serialize;
use spike_core::chart::{Chart, DEFAULT_SEED};
use spike_core::evaluate::{Check, audio_validity};
use spike_core::heavy::Timeline;
use spike_core::play::{PRESS_JITTER_US, Play};
use spike_core::run::{AudioFacts, AudioPeriod, InputLatency, StreamCount};
use spike_core::stats::percentile_i64;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub struct Session {
    sync: Arc<PlaySync>,
    input: threads::InputThread,
    logic: JoinHandle<threads::LogicOut>,
    audio: Option<audio::AudioThreads>,
    pub hits: Consumer<HitMsg>,
    pub snapshot: triple_buffer::Output<Snapshot>,
    notes: u32,
    stop: Arc<AtomicBool>,
}

/// Starts the play threads; the render thread is the barrier's last
/// member and calls `ready`.
pub fn start(
    audio_args: Option<AudioArgs>,
    clock: Clock,
    play_len: i64,
) -> Result<Session, String> {
    let chart = Chart::generate(DEFAULT_SEED);
    let (presses, judge) = Play::new(&chart, play_len).split();
    let notes = presses.len();
    let (press_tx, press_rx) = RingBuffer::new(threads::INPUT_RING);
    let (hit_tx, hit_rx) = RingBuffer::new(threads::HIT_RING);
    let (sound_tx, sound_rx) = RingBuffer::new(threads::SOUND_RING);
    let (snap_in, snap_out) = triple_buffer::triple_buffer(&Snapshot::default());
    let wake = Arc::new(Event::new()?);
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    // Input, logic, render, and audio when it runs.
    let members = 3 + usize::from(audio_args.is_some());
    let sync = Arc::new(PlaySync::new(members, clock, COUNTDOWN_US));
    let audio = match audio_args {
        Some(a) => Some(audio::spawn(
            a,
            sound_rx,
            sync.clone(),
            clock,
            play_len,
            notes,
            stop.clone(),
        )?),
        None => None,
    };
    let input = spawn_input(
        presses,
        press_tx,
        wake.clone(),
        done.clone(),
        sync.clone(),
        clock,
        play_len + PRESS_JITTER_US + 1_000,
        stop.clone(),
    )?;
    let sinks = LogicSinks {
        hits: Some(hit_tx),
        sounds: audio.is_some().then_some(sound_tx),
        snapshot: snap_in,
    };
    let logic = spawn_logic(
        judge,
        press_rx,
        wake,
        done,
        sinks,
        sync.clone(),
        clock,
        notes,
    )?;
    Ok(Session {
        sync,
        input,
        logic,
        audio,
        hits: hit_rx,
        snapshot: snap_out,
        notes: notes as u32,
        stop,
    })
}

#[derive(Serialize)]
pub struct InputSummary {
    pub notes_total: u32,
    pub judged: u32,
    pub records: StreamCount,
    pub stats: InputStatsOut,
    pub sound_overflows: u64,
    pub hit_overflows: u64,
    /// G4 over heavy windows, and over all of play.
    pub heavy_latency_us_p99: Option<i64>,
    pub latency_us_p50: Option<i64>,
    pub latency_us_p99: Option<i64>,
    pub latency_us_max: Option<i64>,
}

#[derive(Serialize)]
pub struct InputStatsOut {
    pub emitted: u64,
    pub overflows: u64,
    pub ticks: u64,
    pub raw_input_events: u64,
}

impl From<InputStats> for InputStatsOut {
    fn from(s: InputStats) -> Self {
        Self {
            emitted: s.emitted,
            overflows: s.overflows,
            ticks: s.ticks,
            raw_input_events: s.raw_input_events,
        }
    }
}

/// Injected gaps against episodes, in the capture's frame domain: gap `k`
/// is expected at capture position `onset + frame_k - pilot_start` (the
/// render stream's pilot steps in at its frame `pilot_start`).
#[derive(Serialize)]
pub struct InjectionCheck {
    pub injected: usize,
    pub episodes: usize,
    /// Detector window in frames (±1 window is the tolerance).
    pub window_frames: u64,
    /// Injected gaps with an episode starting within one window of them.
    pub matched: usize,
    /// Each episode's start minus its nearest expected gap, µs.
    pub offsets_us: Vec<i64>,
    /// The same by the two streams' timestamps (mix clock against loopback
    /// packet stamps), informational: they differ by the engine's
    /// buffering.
    pub timestamp_offsets_us: Vec<i64>,
}

#[derive(Serialize)]
pub struct AudioSummary {
    /// Run µs of play start (episode times are run µs).
    pub play_start_us: i64,
    pub render: Option<RenderSide>,
    pub render_error: Option<String>,
    pub loopback: Option<LoopbackSide>,
    pub loopback_error: Option<String>,
    pub facts: AudioFacts,
    pub v6: Check,
    pub g5_episodes: usize,
    pub period_records: StreamCount,
    pub judge_to_mix_us_p50: Option<i64>,
    pub judge_to_mix_us_p99: Option<i64>,
    pub judge_to_mix_us_min: Option<i64>,
    pub judge_to_mix_us_max: Option<i64>,
    pub injection: Option<InjectionCheck>,
}

#[derive(Serialize)]
pub struct PlaySummary {
    pub input: InputSummary,
    pub audio: Option<AudioSummary>,
}

pub struct SessionOut {
    pub summary: PlaySummary,
    pub input: Vec<InputLatency>,
    pub periods: Vec<AudioPeriod>,
    /// Private: mix-clock anchors (device frame, run µs) and loopback
    /// packet anchors (capture position, run µs).
    pub mix_anchors: Vec<(u64, i64)>,
    pub capture_anchors: Vec<(u64, i64)>,
}

impl Session {
    /// The barrier (render thread): returns the play start.
    pub fn ready(&self) -> i64 {
        self.sync.ready()
    }

    /// Stops the threads (if still running) and collects their records.
    pub fn finish(self, play_start: i64, play_len: i64) -> Result<SessionOut, String> {
        self.stop.store(true, Ordering::Relaxed);
        let stats = self
            .input
            .handle
            .join()
            .map_err(|_| "input thread panicked")??;
        let logic = self.logic.join().map_err(|_| "logic thread panicked")?;
        let loops = (play_len as u64)
            .div_ceil(spike_core::chart::CHART_LEN_US as u64)
            .max(1) as u32;
        let tl = Timeline {
            play_start_us: play_start,
            loops,
        };
        let lat: Vec<i64> = logic.input.iter().map(|x| x.latency_us).collect();
        let heavy: Vec<i64> = logic
            .input
            .iter()
            .filter(|x| tl.is_heavy(x.t_us))
            .map(|x| x.latency_us)
            .collect();
        let input = InputSummary {
            notes_total: self.notes,
            judged: logic.judged,
            records: logic.input_records,
            stats: stats.into(),
            sound_overflows: logic.sound_overflows,
            hit_overflows: logic.hit_overflows,
            heavy_latency_us_p99: percentile_i64(&heavy, 99),
            latency_us_p50: percentile_i64(&lat, 50),
            latency_us_p99: percentile_i64(&lat, 99),
            latency_us_max: lat.iter().max().copied(),
        };
        let mut periods = vec![];
        let (mut mix_anchors, mut capture_anchors) = (vec![], vec![]);
        let audio = match self.audio {
            None => None,
            Some(a) => {
                let render = a.render.join().map_err(|_| "audio thread panicked")?;
                let loopback = a.loopback.join().map_err(|_| "loopback thread panicked")?;
                let (render_side, render_error, period_records, j2m, rate) = match render {
                    Ok(r) => {
                        periods = r.periods;
                        mix_anchors = r.anchors;
                        let rate = r.side.rate_hz;
                        (
                            Some(r.side),
                            None,
                            r.period_records,
                            r.judge_to_mix_us,
                            rate,
                        )
                    }
                    Err(e) => (None, Some(e), StreamCount::default(), vec![], 0),
                };
                let (loop_side, loop_error, facts) = match loopback {
                    Ok(l) => {
                        capture_anchors = l.anchors;
                        (Some(l.side), None, l.facts)
                    }
                    Err(e) => (None, Some(e), AudioFacts::default()),
                };
                let v6 = audio_validity(&facts, (play_start, play_start + play_len), rate);
                let injection = match (&render_side, &loop_side) {
                    (Some(r), Some(l)) if !r.injected_frames.is_empty() => {
                        let starts: Vec<i64> = facts
                            .episodes
                            .iter()
                            .map(|e| e.start_us - play_start)
                            .collect();
                        Some(injection_check(r, l, &starts))
                    }
                    _ => None,
                };
                Some(AudioSummary {
                    play_start_us: play_start,
                    render: render_side,
                    render_error,
                    loopback: loop_side,
                    loopback_error: loop_error,
                    g5_episodes: facts.episodes.len(),
                    v6,
                    facts,
                    period_records,
                    judge_to_mix_us_p50: percentile_i64(&j2m, 50),
                    judge_to_mix_us_p99: percentile_i64(&j2m, 99),
                    judge_to_mix_us_min: j2m.iter().min().copied(),
                    judge_to_mix_us_max: j2m.iter().max().copied(),
                    injection,
                })
            }
        };
        Ok(SessionOut {
            summary: PlaySummary { input, audio },
            input: logic.input,
            periods,
            mix_anchors,
            capture_anchors,
        })
    }
}

/// Each episode against its nearest injected gap, by capture position
/// (and, informationally, by time).
pub fn injection_check(r: &RenderSide, l: &LoopbackSide, episode_us: &[i64]) -> InjectionCheck {
    let rate = i64::from(r.rate_hz.max(1));
    let window = (u64::from(r.rate_hz) + 500) / 1000 / 4 * 4;
    let nearest = |e: i64, xs: &mut dyn Iterator<Item = i64>| {
        xs.map(|x| e - x)
            .min_by_key(|d| d.abs())
            .unwrap_or(i64::MAX)
    };
    // Capture position of the stream's frame 0.
    let base = l
        .onset_position
        .map(|o| o.wrapping_sub(r.pilot_start_frame));
    let (offsets, matched) = match base {
        Some(onset) => (
            l.episode_positions
                .iter()
                .map(|&e| {
                    let mut expected = r.injected_frames.iter().map(|&f| (onset + f) as i64);
                    nearest(e as i64, &mut expected) * 1_000_000 / rate
                })
                .collect(),
            r.injected_frames
                .iter()
                .filter(|&&f| {
                    l.episode_positions
                        .iter()
                        .any(|&e| e.abs_diff(onset + f) <= window)
                })
                .count(),
        ),
        None => (vec![], 0),
    };
    InjectionCheck {
        injected: r.injected_frames.len(),
        episodes: l.episode_positions.len(),
        window_frames: window,
        matched,
        offsets_us: offsets,
        timestamp_offsets_us: episode_us
            .iter()
            .map(|&e| nearest(e, &mut r.injected_us.iter().copied()))
            .collect(),
    }
}

pub fn write(out: &Path, s: &SessionOut) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let csv_out = |name: &str| {
        csv::Writer::from_path(out.join(name)).map_err(|e| format!("write {name}: {e}"))
    };
    let mut w = csv_out("input.csv")?;
    for r in &s.input {
        w.serialize(r).map_err(|e| e.to_string())?;
    }
    w.flush().map_err(|e| e.to_string())?;
    if s.summary.audio.is_some() {
        let mut w = csv_out("audio_periods.csv")?;
        for r in &s.periods {
            w.serialize(r).map_err(|e| e.to_string())?;
        }
        w.flush().map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(&s.summary).map_err(|e| e.to_string())?;
    std::fs::write(out.join("play.json"), json).map_err(|e| e.to_string())?;
    if s.summary.audio.is_some() {
        use std::fmt::Write as _;
        let dir = out.join("private");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut t = String::from("stream,frame,run_us\n");
        for (name, rows) in [("mix", &s.mix_anchors), ("capture", &s.capture_anchors)] {
            for (f, us) in rows {
                let _ = writeln!(t, "{name},{f},{us}");
            }
        }
        std::fs::write(dir.join("audio_anchors.csv"), t).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn print(s: &PlaySummary) {
    let i = &s.input;
    println!(
        "input: {} notes, {} judged; records produced {} lost {}; overflows input {} sound {} hit {}; raw input events {}",
        i.notes_total,
        i.judged,
        i.records.produced,
        i.records.lost,
        i.stats.overflows,
        i.sound_overflows,
        i.hit_overflows,
        i.stats.raw_input_events
    );
    println!(
        "G4 input->judgment: heavy p99 {:?} us; all p50 {:?} us p99 {:?} us max {:?} us",
        i.heavy_latency_us_p99, i.latency_us_p50, i.latency_us_p99, i.latency_us_max
    );
    let Some(a) = &s.audio else {
        println!("audio: null");
        return;
    };
    if let Some(e) = &a.render_error {
        println!("audio: render failed: {e}");
    }
    if let Some(e) = &a.loopback_error {
        println!("audio: loopback failed: {e}");
    }
    if let Some(r) = &a.render {
        println!(
            "audio: {} at {} Hz x{} (period {} us, buffer {} frames, resampler delay {} frames); engine periods {:?}; endpoint volume {:?} dB muted {:?}",
            r.backend,
            r.rate_hz,
            r.channels,
            r.period_us,
            r.buffer_frames,
            r.resampler_delay_frames,
            r.periods,
            r.endpoint_volume_db,
            r.endpoint_muted
        );
        let d = &r.diagnostics;
        println!(
            "audio diagnostics: wakes {}, padding 0 {} (at {:?} us), wake gaps > 1.5 periods {} (at {:?} us), GetBuffer failures {}, mix > period {}, sounds at earliest {}; steals {} hard cuts {} clamped {} sound commands {}",
            d.wakes,
            d.wakes_padding_zero,
            &d.padding_zero_at_us[..d.padding_zero_at_us.len().min(8)],
            d.wake_gaps_over_1_5_periods,
            &d.wake_gap_at_us[..d.wake_gap_at_us.len().min(8)],
            d.get_buffer_failures,
            d.mix_over_period,
            d.sounds_at_earliest,
            r.voice_steals,
            r.hard_cuts,
            r.clamped,
            r.sound_commands
        );
        for st in &r.starves {
            // Episodes by timestamps, which trail the mix clock by the
            // engine's buffering (tens of ms): within 100 ms after.
            let to = st.at_us + i64::from(st.withheld_ms) * 1_000 + 100_000;
            let episodes = a
                .facts
                .episodes
                .iter()
                .filter(|e| (st.at_us..to).contains(&(e.start_us - a.play_start_us)))
                .count();
            println!(
                "starve: at {} us withheld {} ms, queued {} us (exceeds: {}); padding-0 wakes {}; episodes {}",
                st.at_us,
                st.withheld_ms,
                st.queued_us,
                st.exceeds_queued,
                st.padding_zero_wakes,
                episodes
            );
        }
    }
    if let Some(l) = &a.loopback {
        println!(
            "loopback: calibration {:?}, packets {} (silent {}, discontinuity {}, timestamp errors {}){}",
            l.calibration_amplitude,
            l.packets,
            l.silent_packets,
            l.discontinuity_packets,
            l.timestamp_errors,
            l.error
                .as_ref()
                .map_or(String::new(), |e| format!(", error: {e}"))
        );
    }
    let f = &a.facts;
    println!(
        "G5 episodes {}; V6 {} {:?} (capture {:?}..{:?} us, unverifiable {})",
        a.g5_episodes,
        if a.v6.ok { "valid" } else { "invalid" },
        a.v6.reasons,
        f.capture_from_us,
        f.capture_to_us,
        f.unverifiable_spans
    );
    println!(
        "audio periods: produced {} lost {}; judgment->mix p50 {:?} us p99 {:?} us min {:?} max {:?}",
        a.period_records.produced,
        a.period_records.lost,
        a.judge_to_mix_us_p50,
        a.judge_to_mix_us_p99,
        a.judge_to_mix_us_min,
        a.judge_to_mix_us_max
    );
    if let Some(c) = &a.injection {
        println!(
            "injection: {} gaps, {} episodes, {} matched within one window ({} frames); offsets {:?} us (by timestamps {:?} us)",
            c.injected,
            c.episodes,
            c.matched,
            c.window_frames,
            c.offsets_us,
            c.timestamp_offsets_us
        );
    }
}
