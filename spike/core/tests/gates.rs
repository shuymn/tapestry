//! P1 tests (a)–(e): heavy-window gating, thresholds, refresh validity,
//! NA rows and PresentMon capture integrity.

use spike_core::evaluate::{Summary, evaluate};
use spike_core::run::{Episode, EventKind, PmFrame, Rational, RunData, RunEvent, ThreadTag};
use spike_core::synth::{PLAY_START_US, T60_MS, passing_obs_run, passing_run, sync_streams};

fn assert_valid(s: &Summary) {
    let bad: Vec<_> = s.validity.iter().filter(|c| !c.ok).collect();
    assert!(bad.is_empty(), "invalid: {bad:?}");
}

#[test]
fn synthetic_run_is_valid_and_passes() {
    let s = evaluate(&passing_run());
    assert_valid(&s);
    assert!(
        s.gates.iter().all(|g| !g.applies || g.pass),
        "{:?}",
        s.gates
    );
    assert_eq!(s.exit_code, 0);
    assert_eq!((s.refresh_class_hz, s.target_hz), (Some(60), Some(60)));
}

// ---- (a) whole-run pass but heavy-window fail -> fail -----------------

/// Makes every 50th heavy-window sample bad (2% of heavy, 0.8% of the run):
/// the whole-run p99 stays good while the heavy-window p99 is bad.
fn spoil_heavy(run: &mut RunData, outside_instead: bool) {
    let tl = run.info.timeline;
    let pick = |t: i64, i: usize, every: usize| {
        (tl.is_heavy(t) != outside_instead) && i.is_multiple_of(every)
    };
    let every = if outside_instead { 3 } else { 50 };
    for (i, f) in run.pm_frames.iter_mut().enumerate() {
        if pick(f.window_time_us(), i, every) {
            f.display_change_ms = Some(40.0);
            f.gpu_busy_ms = Some(5.0);
        }
    }
    for (i, f) in run.frames.iter_mut().enumerate() {
        if pick(f.present_us.unwrap() + 5_000, i, every) {
            f.render_cpu_us = 4_000;
        }
    }
    for (i, x) in run.input.iter_mut().enumerate() {
        if pick(x.t_us, i, every) {
            x.latency_us = 5_000;
        }
    }
}

#[test]
fn a_whole_run_pass_but_heavy_window_fail_is_a_fail() {
    let mut run = passing_run();
    spoil_heavy(&mut run, false);
    let s = evaluate(&run);
    assert_valid(&s);
    // Informational whole-run p99 still looks fine.
    assert_eq!(s.records.whole_run_p99_interval_ms, Some(T60_MS));
    for id in ["G1", "G2", "G3", "G4"] {
        assert!(!s.gate(id).pass, "{id} should fail on heavy windows");
    }
    assert_eq!(s.exit_code, 1);
}

#[test]
fn a_bad_samples_outside_heavy_windows_do_not_fail_gates() {
    let mut run = passing_run();
    spoil_heavy(&mut run, true);
    let s = evaluate(&run);
    assert!(s.records.whole_run_p99_interval_ms > Some(T60_MS));
    for id in ["G1", "G2", "G3", "G4"] {
        assert!(s.gate(id).pass, "{id} must only see heavy windows");
    }
    assert_eq!(s.exit_code, 0);
}

// ---- (b) each gate at and just past its threshold ---------------------

fn gate_with(id: &str, edit: impl Fn(&mut RunData)) -> (bool, u8) {
    let mut run = passing_run();
    edit(&mut run);
    let s = evaluate(&run);
    assert_valid(&s);
    (s.gate(id).pass, s.exit_code)
}

fn set_heavy_pm(run: &mut RunData, f: impl Fn(&mut PmFrame)) {
    let tl = run.info.timeline;
    run.pm_frames
        .iter_mut()
        .filter(|p| tl.is_heavy(p.window_time_us()))
        .for_each(f);
}

#[test]
fn b_g1_at_and_above_one_refresh() {
    assert_eq!(
        gate_with("G1", |r| set_heavy_pm(r, |p| p.display_change_ms =
            Some(T60_MS))),
        (true, 0)
    );
    assert_eq!(
        gate_with("G1", |r| set_heavy_pm(r, |p| p.display_change_ms =
            Some(T60_MS + 0.0001))),
        (false, 1)
    );
}

#[test]
fn b_g2_render_cpu_1_5_ms() {
    let set = |v: u32| {
        move |r: &mut RunData| {
            let tl = r.info.timeline;
            r.frames
                .iter_mut()
                .filter(|f| tl.is_heavy(f.present_us.unwrap() + 5_000))
                .for_each(|f| f.render_cpu_us = v);
        }
    };
    assert_eq!(gate_with("G2", set(1_500)), (true, 0));
    assert_eq!(gate_with("G2", set(1_501)), (false, 1));
}

#[test]
fn b_g3_gpu_2_ms() {
    assert_eq!(
        gate_with("G3", |r| set_heavy_pm(r, |p| p.gpu_busy_ms = Some(2.0))),
        (true, 0)
    );
    assert_eq!(
        gate_with("G3", |r| set_heavy_pm(r, |p| p.gpu_busy_ms = Some(2.0001))),
        (false, 1)
    );
}

#[test]
fn b_g4_input_to_judgment_1_ms() {
    let set = |v: i64| {
        move |r: &mut RunData| {
            let tl = r.info.timeline;
            r.input
                .iter_mut()
                .filter(|x| tl.is_heavy(x.t_us))
                .for_each(|x| x.latency_us = v);
        }
    };
    assert_eq!(gate_with("G4", set(1_000)), (true, 0));
    assert_eq!(gate_with("G4", set(1_001)), (false, 1));
}

#[test]
fn b_g5_g6_zero() {
    assert_eq!(gate_with("G5", |_| {}), (true, 0));
    assert_eq!(
        gate_with("G5", |r| r.audio.episodes.push(Episode {
            start_us: PLAY_START_US + 1,
            end_us: PLAY_START_US + 2_000,
        })),
        (false, 1)
    );
    // Library-scope allocations are recorded (G6r), not gated.
    assert_eq!(
        gate_with("G6", |r| {
            r.counters
                .library_allocs
                .insert("write_texture".into(), 500);
        }),
        (true, 0)
    );
    assert_eq!(
        gate_with("G6", |r| {
            r.counters.app_allocs.insert(ThreadTag::Render, 1);
        }),
        (false, 1)
    );
}

#[test]
fn b_g7_bga_99_percent() {
    let skip = |n: usize| {
        move |r: &mut RunData| {
            let tl = r.info.timeline;
            r.bga
                .iter_mut()
                .filter(|b| tl.is_heavy(b.due_us))
                .take(n)
                .for_each(|b| b.displayed = false);
        }
    };
    let due = 240 * 60;
    assert_eq!(gate_with("G7", skip(due / 100)), (true, 0));
    assert_eq!(gate_with("G7", skip(due / 100 + 1)), (false, 1));
}

// O1/O2 thresholds are in tests/obs.rs with the OBS fixtures.

// ---- (c) refresh must be exactly 60 or 240 Hz -------------------------

fn validity_with_refresh(num: u64, den: u64, cap: Option<u32>) -> bool {
    let mut run = passing_run();
    run.info.refresh = Rational { num, den };
    run.info.fps_cap = cap;
    evaluate(&run).check("V3").ok
}

#[test]
fn c_59_94_and_239_76_are_invalid() {
    assert!(!validity_with_refresh(60_000, 1_001, None));
    assert!(!validity_with_refresh(240_000, 1_001, None));
    let mut run = passing_run();
    run.info.refresh = Rational {
        num: 60_000,
        den: 1_001,
    };
    assert_eq!(evaluate(&run).exit_code, 2);
    assert!(validity_with_refresh(60, 1, None));
    assert!(validity_with_refresh(240, 1, None));
    // Upper bound target × 1.001 is inclusive.
    assert!(validity_with_refresh(60_060, 1_000, None));
    assert!(!validity_with_refresh(60_061, 1_000, None));
    assert!(!validity_with_refresh(144, 1, None));
    // Caps must divide the refresh rate.
    assert!(validity_with_refresh(240, 1, Some(120)));
    assert!(validity_with_refresh(240, 1, Some(60)));
    assert!(!validity_with_refresh(240, 1, Some(100)));
}

// ---- matrix eligibility (M1) ------------------------------------------

#[test]
fn m1_capped_240_hz_baseline_is_invalid_not_a_pass() {
    let mut run = passing_run();
    run.info.refresh = Rational { num: 240, den: 1 };
    run.info.fps_cap = Some(60);
    let s = evaluate(&run);
    assert_eq!(s.check("M1").reasons, vec!["baseline-capped"]);
    // T stays 1/240 s for R1, so the 60 fps frames also fail G1.
    assert_eq!(s.target_hz, Some(240));
    assert!(!s.gate("G1").pass);
    assert_eq!(s.exit_code, 2);
}

#[test]
fn m1_required_runs_need_the_matrix_configuration() {
    use spike_core::run::{Profile, RunKind};
    let reasons = |edit: &dyn Fn(&mut RunData)| {
        let mut run = passing_run();
        edit(&mut run);
        evaluate(&run).check("M1").reasons.clone()
    };
    assert_eq!(
        reasons(&|r| r.info.profile = Profile::Streaming),
        vec!["baseline-not-standard-profile"]
    );
    assert_eq!(reasons(&|r| r.info.timeline.loops = 1), vec!["not-4-loops"]);
    assert_eq!(
        reasons(&|r| {
            r.info.kind = RunKind::Obs;
            r.info.profile = Profile::NoBga;
            r.info.fps_cap = Some(60);
        }),
        vec!["no-bga-is-diagnostic-only"]
    );
    assert_eq!(
        reasons(&|r| {
            r.info.kind = RunKind::Obs;
            r.info.fps_cap = Some(60);
        }),
        vec!["standard-profile-capped"]
    );
    assert_eq!(
        reasons(&|r| {
            r.info.kind = RunKind::Obs;
            r.info.profile = Profile::Streaming;
        }),
        vec!["streaming-profile-uncapped"]
    );
    // The same configurations are fine as diagnostics.
    assert!(
        reasons(&|r| {
            r.info.kind = RunKind::Diagnostic;
            r.info.profile = Profile::NoBga;
            r.info.timeline.loops = 1;
            r.info.fps_cap = Some(60);
        })
        .is_empty()
    );
}

#[test]
fn g1_target_is_the_cap_only_for_streaming_obs_runs() {
    use spike_core::run::Profile;
    let mut run = passing_obs_run();
    run.info.refresh = Rational { num: 240, den: 1 };
    run.info.profile = Profile::Streaming;
    run.info.fps_cap = Some(60);
    let s = evaluate(&run);
    assert_eq!(s.target_hz, Some(60));
    assert!(s.check("M1").ok);
    assert!(s.gate("G1").pass);
    run.info.profile = Profile::Standard;
    run.info.fps_cap = None;
    let s = evaluate(&run);
    assert_eq!(s.target_hz, Some(240));
    assert!(!s.gate("G1").pass);
}

// ---- G2 window assignment ------------------------------------------------

#[test]
fn g2_assigns_frames_by_display_time_like_g1_and_g3() {
    // Every frame is displayed 30 s after its present, so present-time
    // windows and display-time windows differ. Spoil exactly the frames
    // displayed in heavy windows: G2 must see them.
    let mut run = passing_run();
    let tl = run.info.timeline;
    for f in &mut run.pm_frames {
        f.display_us = Some(f.present_us + 30_000_000);
    }
    for f in &mut run.frames {
        let p = f.present_us.unwrap();
        if tl.is_heavy(p + 30_000_000) && !tl.is_heavy(p) {
            f.render_cpu_us = 5_000;
        }
    }
    let s = evaluate(&run);
    assert_valid(&s);
    assert!(!s.gate("G2").pass);
}

// ---- complete evidence (V8) -------------------------------------------

#[test]
fn v8_streams_must_reach_play_edges_within_two_of_their_periods() {
    let start = PLAY_START_US;
    let v8 = |edit: &dyn Fn(&mut RunData)| {
        let mut run = passing_run();
        edit(&mut run);
        sync_streams(&mut run);
        evaluate(&run).check("V8").reasons.clone()
    };
    let end = passing_run().info.timeline.play_end_us();
    // Audio periods every 10 ms: first record 20 ms late is fine, 30 ms not.
    assert!(v8(&|r| r.audio_periods.retain(|p| p.t_us >= start + 20_000)).is_empty());
    assert_eq!(
        v8(&|r| r.audio_periods.retain(|p| p.t_us >= start + 30_000)),
        vec!["audio-periods-incomplete"]
    );
    assert!(v8(&|r| r.audio_periods.retain(|p| p.t_us <= end - 20_000)).is_empty());
    assert_eq!(
        v8(&|r| r.audio_periods.retain(|p| p.t_us <= end - 30_000)),
        vec!["audio-periods-incomplete"]
    );
    // BGA due every 1/60 s: two intervals late is fine, three not.
    assert!(
        v8(&|r| {
            r.bga.drain(..2);
        })
        .is_empty()
    );
    assert_eq!(
        v8(&|r| {
            r.bga.drain(..3);
        }),
        vec!["bga-incomplete"]
    );
    assert_eq!(
        v8(&|r| {
            let n = r.bga.len();
            r.bga.truncate(n - 3);
        }),
        vec!["bga-incomplete"]
    );
}

#[test]
fn v8_missing_or_partial_evidence_is_invalid() {
    use spike_core::run::{AudioPeriod, InputLatency};
    type Edit = fn(&mut RunData);
    let cases: &[(&str, Edit)] = &[
        ("input-incomplete", |r| {
            r.input.pop();
            sync_streams(r);
        }),
        ("input-incomplete", |r| r.streams.input.lost = 1),
        ("input-incomplete", |r| {
            // One sample cannot satisfy G4.
            r.input = vec![InputLatency {
                t_us: PLAY_START_US + 50_000_000,
                latency_us: 100,
            }];
            sync_streams(r);
        }),
        ("audio-periods-incomplete", |r| {
            r.audio_periods = vec![AudioPeriod {
                t_us: PLAY_START_US + 50_000_000,
                active_voices: 200,
            }];
            sync_streams(r);
        }),
        ("audio-periods-incomplete", |r| {
            let a = PLAY_START_US + 100_000_000;
            r.audio_periods
                .retain(|p| !(a..a + 60_000).contains(&p.t_us));
            sync_streams(r);
        }),
        ("audio-periods-incomplete", |r| {
            r.streams.audio_periods.produced += 1
        }),
        ("bga-incomplete", |r| {
            let a = PLAY_START_US + 45_000_000;
            r.bga.retain(|b| !(a..a + 100_000).contains(&b.due_us));
            sync_streams(r);
        }),
        ("frames-incomplete", |r| {
            let end = r.info.timeline.play_end_us();
            r.frames.retain(|f| f.render_us < end - 5_000_000);
            sync_streams(r);
        }),
        ("frames-incomplete", |r| r.streams.frames.lost = 2),
        ("allocation-counters-missing", |r| {
            r.counters.app_allocs.remove(&ThreadTag::Audio);
        }),
        ("gpu-time-missing", |r| {
            let tl = r.info.timeline;
            let f = r
                .pm_frames
                .iter_mut()
                .find(|f| tl.is_heavy(f.window_time_us()))
                .unwrap();
            f.gpu_busy_ms = None;
        }),
    ];
    for &(reason, edit) in cases {
        let mut run = passing_run();
        edit(&mut run);
        let s = evaluate(&run);
        assert_eq!(s.check("V8").reasons, vec![reason]);
        assert_eq!(s.exit_code, 2, "{reason}");
    }
}

// ---- (d) NA rows count as not displayed, never dropped ----------------

#[test]
fn d_na_rows_are_counted_as_not_displayed() {
    let mut run = passing_run();
    let tl = run.info.timeline;
    let mut n = 0;
    for f in run
        .pm_frames
        .iter_mut()
        .filter(|f| tl.is_heavy(f.present_us))
        .step_by(100)
    {
        f.display_us = None;
        f.display_change_ms = None;
        n += 1;
    }
    let s = evaluate(&run);
    assert_valid(&s);
    assert_eq!(s.records.not_displayed, n);
    assert_eq!(s.records.missed_deadlines, n);
    let heavy_rows = run
        .pm_frames
        .iter()
        .filter(|f| tl.is_heavy(f.window_time_us()))
        .count() as u64;
    assert_eq!(s.records.heavy_frames, heavy_rows);
    assert_eq!(s.gate("G1").samples, heavy_rows - n);
}

// ---- (e) PresentMon capture integrity (V4) vs a slow run --------------

#[test]
fn e_truncated_capture_is_invalid() {
    let mut run = passing_run();
    run.info.presentmon_error = Some("malformed or truncated PresentMon row at line 9".into());
    assert!(!evaluate(&run).check("V4").ok);
    // Cut at a row boundary before play ends: does not bracket.
    let mut run = passing_run();
    let end = run.info.timeline.play_end_us();
    run.pm_frames.retain(|f| f.present_us < end - 10_000_000);
    let s = evaluate(&run);
    assert!(
        s.check("V4")
            .reasons
            .contains(&"presentmon-does-not-bracket-play".to_string())
    );
    assert_eq!(s.exit_code, 2);
}

#[test]
fn e_non_bracketing_capture_is_invalid() {
    let mut run = passing_run();
    run.pm_frames
        .retain(|f| f.present_us > PLAY_START_US + 100_000);
    assert!(!evaluate(&run).check("V4").ok);
}

#[test]
fn e_missing_presents_are_invalid() {
    let mut run = passing_run();
    let mid = PLAY_START_US + 300_000_000;
    run.pm_frames
        .retain(|f| !(mid..mid + 50_000).contains(&f.present_us));
    let s = evaluate(&run);
    assert_eq!(s.check("V4").reasons, vec!["presents-missing-from-capture"]);
    assert_eq!(s.exit_code, 2);
}

/// Present counts on every frame; `lost` frame records are dropped (the
/// spike lost them) and `pm_missing` PresentMon rows for the same frames.
fn counted_run(lost: usize, pm_missing: usize) -> RunData {
    let mut run = passing_run();
    for (i, f) in run.frames.iter_mut().enumerate() {
        f.present_count = Some(1_000 + i as u32);
    }
    let mid = run.frames.len() / 2;
    let gone: Vec<i64> = run.frames[mid..mid + lost]
        .iter()
        .filter_map(|f| f.present_us)
        .collect();
    run.frames.drain(mid..mid + lost);
    let pm_gone = &gone[..pm_missing];
    run.pm_frames.retain(|f| !pm_gone.contains(&f.present_us));
    sync_streams(&mut run);
    run
}

#[test]
fn e_presents_counted_by_present_count_must_all_be_captured() {
    // Lost frame records alone: PresentMon still has every present.
    assert!(evaluate(&counted_run(3, 0)).check("V4").ok);
    // The same presents also missing from PresentMon: time matching sees
    // nothing wrong (no record to match), the present count does.
    assert_eq!(
        evaluate(&counted_run(3, 3)).check("V4").reasons,
        vec!["presents-missing-from-capture"]
    );
}

#[test]
fn e_capture_gap_while_presenting_is_invalid() {
    let mut run = passing_run();
    let mid = PLAY_START_US + 100_000_000;
    run.pm_frames
        .retain(|f| !(mid..mid + 1_500_000).contains(&f.present_us));
    assert!(
        evaluate(&run)
            .check("V4")
            .reasons
            .contains(&"capture-gap-over-1s".to_string())
    );
}

/// A run rendering a frame every `render_period_us` at full load and
/// presenting every `present_every`-th one, with a complete capture whose
/// displayed intervals follow the 60 Hz refresh grid.
fn slow_run(render_period_us: i64, present_every: usize) -> RunData {
    let mut run = passing_run();
    let tl = run.info.timeline;
    let from = tl.play_start_us - 1_000_000;
    let renders: Vec<i64> = (from..tl.play_end_us() + 2_000_000)
        .step_by(render_period_us as usize)
        .collect();
    let refresh_us = 1_000_000.0 / 60.0;
    let template_frame = run.frames[0];
    run.frames = renders
        .iter()
        .enumerate()
        .map(|(i, &t)| spike_core::run::SpikeFrame {
            render_us: t,
            present_us: i.is_multiple_of(present_every).then_some(t + 3_000),
            ..template_frame
        })
        .collect();
    let mut prev_vsync: Option<i64> = None;
    run.pm_frames = run
        .frames
        .iter()
        .filter_map(|f| f.present_us)
        .map(|t| {
            let vsync = ((t - from) as f64 / refresh_us).ceil() as i64;
            let change = prev_vsync.map(|p| (vsync - p) as f64 * T60_MS);
            prev_vsync = Some(vsync);
            PmFrame {
                present_us: t,
                display_us: Some(from + (vsync as f64 * refresh_us) as i64),
                display_change_ms: change.or(Some(T60_MS)),
                gpu_busy_ms: Some(1.0),
            }
        })
        .collect();
    sync_streams(&mut run);
    run
}

#[test]
fn e_complete_capture_of_a_50_fps_run_on_60_hz_fails_g1_but_is_valid() {
    let s = evaluate(&slow_run(20_000, 1));
    assert_valid(&s);
    assert!(!s.gate("G1").pass);
    assert!(s.gate("G1").value > Some(30.0));
    assert!(s.records.missed_deadlines > 0);
    assert_eq!(s.exit_code, 1);
}

#[test]
fn e_full_load_run_presenting_every_2_s_is_valid_and_fails_g1() {
    // V2 looks at rendered frames, so the load is proven although only one
    // frame in 120 reaches the screen.
    let s = evaluate(&slow_run(1_000_000 / 60, 120));
    assert_valid(&s);
    assert!(!s.gate("G1").pass);
    assert_eq!(s.exit_code, 1);
}

// ---- other validity rules ----------------------------------------------

#[test]
fn v1_v2_v5_v6_invalidate() {
    type Edit = fn(&mut RunData);
    let cases: &[(&str, Edit)] = &[
        ("V1", |r| r.counters.notes_played -= 1),
        ("V1", |r| r.counters.notes_total = 9_999),
        ("V2", |r| {
            let tl = r.info.timeline;
            let w = tl.window(5);
            r.audio_periods
                .iter_mut()
                .filter(|p| (w.start_us..w.start_us + 2_000_000).contains(&p.t_us))
                .for_each(|p| p.active_voices = 99);
        }),
        ("V2", |r| {
            let w = r.info.timeline.window(3);
            r.frames
                .iter_mut()
                .filter(|f| (w.start_us + 7_000_000..w.start_us + 8_000_000).contains(&f.render_us))
                .for_each(|f| f.lanes_with_effects = 7);
        }),
        ("V2", |r| r.info.bga_height = Some(720)),
        ("V5", |r| {
            r.events.push(RunEvent {
                t_us: PLAY_START_US + 5,
                kind: EventKind::FocusLoss,
            })
        }),
        ("V5", |r| r.counters.sound_overflows = 1),
        ("V5", |r| r.counters.input_overflows = 1),
        ("V6", |r| r.audio.pilot_calibrated = false),
        ("V6", |r| r.audio.unverifiable_spans = 1),
        ("V6", |r| r.info.mix_rate_hz = 32_000),
        // The detector's warm-up or end must not fall inside play.
        ("V6", |r| r.audio.capture_from_us = Some(PLAY_START_US + 1)),
        ("V6", |r| {
            r.audio.capture_to_us = Some(r.info.timeline.play_end_us() - 1)
        }),
        ("V6", |r| r.audio.capture_to_us = None),
    ];
    for &(id, edit) in cases {
        let mut run = passing_run();
        edit(&mut run);
        let s = evaluate(&run);
        assert!(!s.check(id).ok, "{id}");
        assert_eq!(s.exit_code, 2, "{id}");
    }
    // Events outside play and BGA-pool exhaustion do not invalidate.
    let mut run = passing_run();
    run.events.push(RunEvent {
        t_us: 0,
        kind: EventKind::FocusLoss,
    });
    run.counters.bga_pool_exhausted = 3;
    assert_eq!(evaluate(&run).exit_code, 0);
}
