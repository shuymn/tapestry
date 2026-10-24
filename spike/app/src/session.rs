//! The play threads of a windowed run (design D4): input, logic and, unless
//! `--audio null`, audio and loopback, started together with the render
//! thread behind one play barrier; their records after the run.

use crate::audio::{self, AudioArgs, LoopbackSide, RenderSide};
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
    input: Option<threads::InputThread>,
    logic: Option<JoinHandle<threads::LogicOut>>,
    audio: Option<audio::AudioThreads>,
    pub hits: Consumer<HitMsg>,
    pub snapshot: triple_buffer::Output<Snapshot>,
    notes: u32,
    stop: Arc<AtomicBool>,
}

/// A session dropped without `finish` (a failed start, or a render thread
/// that failed before play) releases the barrier and joins every thread it
/// started.
impl Drop for Session {
    fn drop(&mut self) {
        self.sync.abort("the run ended before play".into());
        self.stop.store(true, Ordering::Relaxed);
        if let Some(i) = self.input.take() {
            let _ = i.handle.join();
        }
        if let Some(l) = self.logic.take() {
            let _ = l.join();
        }
        if let Some(a) = self.audio.take() {
            let _ = a.render.join();
            let _ = a.loopback.join();
        }
    }
}

/// Starts the play threads; the render thread is the barrier's last
/// member and calls `ready`.
pub fn start(
    audio_args: Option<AudioArgs>,
    clock: Clock,
    countdown_us: i64,
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
    let sync = Arc::new(PlaySync::new(members, clock, countdown_us, play_len));
    let has_audio = audio_args.is_some();
    // Threads join the session as they start, so an error below drops it
    // and releases and joins them.
    let mut session = Session {
        sync: sync.clone(),
        input: None,
        logic: None,
        audio: None,
        hits: hit_rx,
        snapshot: snap_out,
        notes: notes as u32,
        stop: stop.clone(),
    };
    if let Some(a) = audio_args {
        session.audio = Some(audio::spawn(
            a,
            sound_rx,
            sync.clone(),
            clock,
            play_len,
            notes,
            stop.clone(),
        )?);
    }
    session.input = Some(spawn_input(
        presses,
        press_tx,
        wake.clone(),
        done.clone(),
        sync.clone(),
        clock,
        play_len + PRESS_JITTER_US + 1_000,
        stop.clone(),
    )?);
    let sinks = LogicSinks {
        hits: Some(hit_tx),
        sounds: has_audio.then_some(sound_tx),
        snapshot: snap_in,
    };
    session.logic = Some(spawn_logic(
        judge, press_rx, wake, done, sinks, sync, clock, notes,
    )?);
    Ok(session)
}

#[derive(Serialize)]
pub struct InputSummary {
    pub notes_total: u32,
    pub judged: u32,
    pub records: StreamCount,
    pub stats: InputStats,
    pub sound_overflows: u64,
    pub hit_overflows: u64,
    /// Longest dequeue → judgment completion (part of G4).
    pub judge_us_max: i64,
    /// G4 over heavy windows, and over all of play.
    pub heavy_latency_us_p99: Option<i64>,
    pub latency_us_p50: Option<i64>,
    pub latency_us_p99: Option<i64>,
    pub latency_us_max: Option<i64>,
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
    /// The run's own audio verdict (S0 checks, audio evidence).
    pub check: Option<AudioCheck>,
}

/// A windowed run's audio verdict: `exit` 2 if the audio evidence itself
/// failed (a stream or loopback error), 1 if an S0 check failed (injected
/// gaps not each found once, or a withholding without its episode), else 0.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AudioCheck {
    pub exit: u8,
    pub reasons: Vec<String>,
}

/// The audio verdict of a run with `audio` on, given whether gaps were
/// injected and writes withheld.
pub fn audio_check(a: Option<&AudioSummary>, inject: bool, starve: bool) -> AudioCheck {
    let mut c = AudioCheck::default();
    let Some(a) = a else {
        c.exit = 2;
        c.reasons.push("no audio summary".into());
        return c;
    };
    if let Some(e) = &a.render_error {
        c.exit = 2;
        c.reasons.push(format!("render stream failed: {e}"));
    }
    if let Some(e) = &a.loopback_error {
        c.exit = 2;
        c.reasons.push(format!("loopback capture failed: {e}"));
    }
    if c.exit == 2 {
        return c;
    }
    if inject {
        match &a.injection {
            Some(i) if i.injected > 0 && i.matched == i.injected && i.episodes == i.injected => {}
            Some(i) => c.reasons.push(format!(
                "injection: {} gaps, {} episodes, {} matched (each gap must be found exactly once)",
                i.injected, i.episodes, i.matched
            )),
            None => c.reasons.push("injection: no gaps were injected".into()),
        }
    }
    if starve {
        let starves = a.render.as_ref().map_or(&[][..], |r| &r.starves[..]);
        if starves.is_empty() {
            c.reasons
                .push("starvation: writes were never withheld".into());
        }
        for st in starves {
            let to = st.at_us + i64::from(st.withheld_ms) * 1_000 + STARVE_EPISODE_SLACK_US;
            let episodes = a
                .facts
                .episodes
                .iter()
                .filter(|e| (st.at_us..to).contains(&(e.start_us - a.play_start_us)))
                .count();
            if st.exceeds_queued && (episodes == 0 || st.padding_zero_wakes == 0) {
                c.reasons.push(format!(
                    "starvation at {} us: {episodes} episodes, {} empty-buffer wakes (both must be found)",
                    st.at_us, st.padding_zero_wakes
                ));
            }
        }
    }
    if !c.reasons.is_empty() {
        c.exit = 1;
    }
    c
}

/// Episodes are stamped by the loopback, which trails the mix clock by the
/// engine's buffering (tens of ms).
const STARVE_EPISODE_SLACK_US: i64 = 100_000;

pub struct SessionOut {
    pub summary: PlaySummary,
    pub input: Vec<InputLatency>,
    pub periods: Vec<AudioPeriod>,
    /// (judgment run µs, judgment → scheduled mix µs) per sound.
    pub judge_to_mix: Vec<(i64, i64)>,
    /// Private: mix-clock anchors (device frame, run µs) and loopback
    /// packet anchors (capture position, run µs).
    pub mix_anchors: Vec<(u64, i64)>,
    pub capture_anchors: Vec<(u64, i64)>,
}

impl Session {
    /// The barrier (render thread): returns the play start, or the error of
    /// a play thread whose setup failed.
    pub fn ready(&self) -> Result<i64, String> {
        self.sync.ready()
    }

    /// Stops the threads (if still running) and collects their records.
    pub fn finish(mut self, play_start: i64, play_len: i64) -> Result<SessionOut, String> {
        self.stop.store(true, Ordering::Relaxed);
        let input = self.input.take().ok_or("input thread missing")?;
        let stats = input.handle.join().map_err(|_| "input thread panicked")??;
        let logic = self
            .logic
            .take()
            .ok_or("logic thread missing")?
            .join()
            .map_err(|_| "logic thread panicked")?;
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
            stats,
            sound_overflows: logic.sound_overflows,
            hit_overflows: logic.hit_overflows,
            judge_us_max: logic.judge_us_max,
            heavy_latency_us_p99: percentile_i64(&heavy, 99),
            latency_us_p50: percentile_i64(&lat, 50),
            latency_us_p99: percentile_i64(&lat, 99),
            latency_us_max: lat.iter().max().copied(),
        };
        let mut periods = vec![];
        let mut judge_to_mix = vec![];
        let (mut mix_anchors, mut capture_anchors) = (vec![], vec![]);
        let audio = match self.audio.take() {
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
                let j2m_us: Vec<i64> = j2m.iter().map(|x| x.1).collect();
                judge_to_mix = j2m;
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
                    judge_to_mix_us_p50: percentile_i64(&j2m_us, 50),
                    judge_to_mix_us_p99: percentile_i64(&j2m_us, 99),
                    judge_to_mix_us_min: j2m_us.iter().min().copied(),
                    judge_to_mix_us_max: j2m_us.iter().max().copied(),
                    injection,
                })
            }
        };
        Ok(SessionOut {
            summary: PlaySummary {
                input,
                audio,
                check: None,
            },
            input: logic.input,
            periods,
            judge_to_mix,
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
        let mut w = csv_out("judge_to_mix.csv")?;
        w.write_record(["judged_us", "judge_to_mix_us"])
            .map_err(|e| e.to_string())?;
        for (t, d) in &s.judge_to_mix {
            w.write_record([t.to_string(), d.to_string()])
                .map_err(|e| e.to_string())?;
        }
        w.flush().map_err(|e| e.to_string())?;
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
        "input: {} notes, {} judged; records produced {} lost {}; overflows input {} sound {} hit {}; raw input events {}; MMCSS {}",
        i.notes_total,
        i.judged,
        i.records.produced,
        i.records.lost,
        i.stats.overflows,
        i.sound_overflows,
        i.hit_overflows,
        i.stats.raw_input_events,
        i.stats.mmcss
    );
    println!(
        "G4 input->judgment: heavy p99 {:?} us; all p50 {:?} us p99 {:?} us max {:?} us (judgment itself up to {} us)",
        i.heavy_latency_us_p99,
        i.latency_us_p50,
        i.latency_us_p99,
        i.latency_us_max,
        i.judge_us_max
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
        println!(
            "audio: queue {} periods; MMCSS {}; mix clock resets {}; wake lateness vs the mix clock p50 {:?} us p99 {:?} us max {:?} us",
            r.queue_periods,
            r.mmcss,
            r.mix_clock_rebases,
            r.wake_lateness_us_p50,
            r.wake_lateness_us_p99,
            r.wake_lateness_us_max
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
            // engine's buffering.
            let to = st.at_us + i64::from(st.withheld_ms) * 1_000 + STARVE_EPISODE_SLACK_US;
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
            "loopback: calibration {:?}, packets {} (silent {}, discontinuity {}, timestamp errors {}); time map error {:?} us over {} stamps; stop cut at {:?} (late {}){}",
            l.calibration_amplitude,
            l.packets,
            l.silent_packets,
            l.discontinuity_packets,
            l.timestamp_errors,
            l.map_error_us,
            l.anchors_used,
            l.cut_position,
            l.cut_late,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Starve;
    use spike_core::run::Episode;

    fn summary() -> AudioSummary {
        AudioSummary {
            play_start_us: 5_000_000,
            render: Some(RenderSide::default()),
            render_error: None,
            loopback: Some(LoopbackSide::default()),
            loopback_error: None,
            facts: AudioFacts::default(),
            v6: Check {
                id: "V6".into(),
                ok: true,
                reasons: vec![],
            },
            g5_episodes: 0,
            period_records: StreamCount::default(),
            judge_to_mix_us_p50: None,
            judge_to_mix_us_p99: None,
            judge_to_mix_us_min: None,
            judge_to_mix_us_max: None,
            injection: None,
        }
    }

    fn injection(injected: usize, episodes: usize, matched: usize) -> Option<InjectionCheck> {
        Some(InjectionCheck {
            injected,
            episodes,
            window_frames: 48,
            matched,
            offsets_us: vec![],
            timestamp_offsets_us: vec![],
        })
    }

    #[test]
    fn s0_injection_needs_every_gap_found_exactly_once() {
        let mut a = summary();
        assert_eq!(audio_check(Some(&a), false, false).exit, 0);
        a.injection = injection(12, 12, 12);
        assert_eq!(audio_check(Some(&a), true, false).exit, 0);
        for (e, m) in [(11, 11), (13, 12), (12, 11)] {
            a.injection = injection(12, e, m);
            assert_eq!(audio_check(Some(&a), true, false).exit, 1, "{e} {m}");
        }
        a.injection = None;
        assert_eq!(audio_check(Some(&a), true, false).exit, 1);
    }

    #[test]
    fn s0_starvation_needs_an_episode_and_empty_buffer_wakes() {
        let mut a = summary();
        let starve = |padding_zero_wakes| Starve {
            at_us: 10_000_000,
            withheld_ms: 50,
            queued_us: 20_000,
            exceeds_queued: true,
            padding_zero_wakes,
        };
        // Never withheld.
        assert_eq!(audio_check(Some(&a), false, true).exit, 1);
        a.render.as_mut().unwrap().starves = vec![starve(3)];
        // Withheld, no episode.
        assert_eq!(audio_check(Some(&a), false, true).exit, 1);
        a.facts.episodes = vec![Episode {
            start_us: 5_000_000 + 10_030_000,
            end_us: 5_000_000 + 10_060_000,
        }];
        assert_eq!(audio_check(Some(&a), false, true).exit, 0);
        a.render.as_mut().unwrap().starves = vec![starve(0)];
        assert_eq!(audio_check(Some(&a), false, true).exit, 1);
    }

    #[test]
    fn failed_audio_evidence_is_invalid() {
        let mut a = summary();
        a.loopback_error = Some("device lost".into());
        assert_eq!(audio_check(Some(&a), false, false).exit, 2);
        assert_eq!(audio_check(None, false, false).exit, 2);
    }
}
