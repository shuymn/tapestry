//! The engine against a simulated device: key sounds at the first unwritten
//! frame, BGM on the QPC timeline under drift, jitter, underruns and resume;
//! banks at every practice speed.

mod common;

use std::sync::Arc;

use audio::{
    AudioEngine, AudioOutput, BankLimits, EngineError, RecordingOutput, Resume, Scheduled,
    ScheduledKind, SimConfig, SimDevice, Sound, SoundBank, SoundCmd, Speed, load_bank_at,
};
use bms_model::{BgmEvent, SoundId, TimeUs};
use rtrb::{Producer, RingBuffer};

const RATE: u32 = 48_000;
const PERIOD: u32 = 480;
const PERIOD_US: i64 = 10_000;

const KEY: SoundId = SoundId(0);
const CLICK: SoundId = SoundId(1);
const KEY_FIRST: i16 = 12_345;
const CLICK_FIRST: i16 = -23_456;

/// A key sound (300 frames from 12 345 down by 17) and a BGM click (48
/// frames from −23 456 up by 101).
fn bank() -> Arc<SoundBank> {
    Arc::new(SoundBank::from_sounds(
        RATE,
        vec![
            Sound::mono((0..300).map(|i| KEY_FIRST - 17 * i).collect()),
            Sound::mono((0..48).map(|i| CLICK_FIRST + 101 * i).collect()),
        ],
    ))
}

fn engine(bgm: &[BgmEvent]) -> AudioEngine {
    AudioEngine::new(bank(), bgm, RATE, PERIOD, 64, 1_024).unwrap()
}

fn queue() -> (Producer<SoundCmd>, rtrb::Consumer<SoundCmd>) {
    RingBuffer::new(64)
}

fn drain(e: &mut AudioEngine, into: &mut Vec<Scheduled>) {
    e.drain_scheduled(|s| into.push(*s));
}

#[test]
fn key_sound_starts_at_first_unwritten_frame_two_to_three_periods_after_receipt() {
    let cfg = SimConfig::steady(RATE, PERIOD, TimeUs(1_000_000));
    let mut out = RecordingOutput::new(SimDevice::new(cfg), 48_000);
    let mut e = engine(&[]);
    let (mut tx, mut rx) = queue();
    // Receipts at 0 %, 50 % and 99 % of a period after a wake; each is read
    // at the next wake. By hand: that wake writes from frame (j + 1) P,
    // played (j + 2) P after the start, so the latency is (3 − φ) periods.
    let cases = [(0, 30_000), (50, 25_000), (99, 20_100)];
    let mut last_wake = out.inner().next_wake();
    e.step(&mut out, &mut rx).unwrap();
    let mut seen = 0;
    for step in 1..80 {
        let mut expect = None;
        if step % 20 == 0 {
            let (pct, latency) = cases[step / 20 - 1];
            let origin = TimeUs(last_wake.0 + PERIOD_US * pct / 100);
            tx.push(SoundCmd { sound: KEY, origin }).unwrap();
            expect = Some((origin, latency, e.written()));
        }
        last_wake = out.inner().next_wake();
        e.step(&mut out, &mut rx).unwrap();
        let mut s = Vec::new();
        drain(&mut e, &mut s);
        let Some((origin, latency, first_unwritten)) = expect else {
            assert!(s.is_empty());
            continue;
        };
        assert_eq!(s.len(), 1);
        let s = s[0];
        assert_eq!(s.kind, ScheduledKind::Key { origin });
        assert_eq!(s.frame, first_unwritten);
        let truth = out.inner().true_time(s.frame);
        assert_eq!(truth.0 - origin.0, latency);
        assert!((2 * PERIOD_US..=3 * PERIOD_US).contains(&latency));
        // The mix clock agrees with the device (no jitter, no drift).
        assert_eq!(s.at, Some(truth));
        seen += 1;
    }
    assert_eq!(seen, cases.len());
    // The first sample of each sound is intact at its start frame.
    let left: Vec<f32> = out.samples().iter().step_by(2).copied().collect();
    let starts: Vec<usize> = left
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] == 0.0 && w[1] != 0.0)
        .map(|(i, _)| i + 1)
        .collect();
    assert_eq!(starts.len(), 3);
    for i in starts {
        assert_eq!(left[i], f32::from(KEY_FIRST) / 65_536.0);
    }
}

/// Runs `e` on `dev` until the device passes `until`, returning every
/// scheduled start.
fn run(e: &mut AudioEngine, dev: &mut SimDevice, until: TimeUs) -> Vec<Scheduled> {
    let (_tx, mut rx) = queue();
    let mut all = Vec::new();
    while dev.next_wake() <= until {
        e.step(dev, &mut rx).unwrap();
        drain(e, &mut all);
    }
    all
}

fn bgm_every_half_second(from_s: i64, to_s: i64) -> Vec<BgmEvent> {
    (2 * from_s..2 * to_s)
        .map(|k| BgmEvent {
            time: TimeUs(k * 500_000),
            sound: CLICK,
        })
        .collect()
}

/// The largest |true start − intended time| over the BGM starts, after
/// checking their count.
fn worst_bgm_error(starts: &[Scheduled], dev: &SimDevice, offset: i64, count: usize) -> i64 {
    let bgm: Vec<_> = starts
        .iter()
        .filter_map(|s| match s.kind {
            ScheduledKind::Bgm { time } => Some((time, s.frame)),
            ScheduledKind::Key { .. } => None,
        })
        .collect();
    assert_eq!(bgm.len(), count);
    bgm.iter()
        .map(|&(time, frame)| (dev.true_time(frame).0 - (time.0 - offset)).abs())
        .max()
        .unwrap()
}

#[test]
fn bgm_follows_qpc_under_100ppm_drift() {
    // 180 s with BGM every 0.5 s from 1 s: 358 starts (by hand). Without
    // re-anchoring, 100 ppm would move the late ones by 18 ms.
    for drift in [100, -100] {
        let bgm = bgm_every_half_second(1, 180);
        let mut dev = SimDevice::new(SimConfig {
            drift_ppm: drift,
            jitter_us: 2_000,
            seed: 7,
            ..SimConfig::steady(RATE, PERIOD, TimeUs(-1_000_000))
        });
        let mut e = engine(&bgm);
        let starts = run(&mut e, &mut dev, TimeUs(181_000_000));
        let worst = worst_bgm_error(&starts, &dev, 0, 358);
        assert!(worst <= PERIOD_US, "drift {drift}: worst {worst} us");
        let estimate = e.clock().drift_ppm().unwrap();
        println!(
            "drift: injected {drift} ppm, estimated {estimate:.2} ppm; worst BGM start error {worst} us"
        );
        assert_eq!(e.underruns(), 0);
    }
}

#[test]
fn bgm_is_scheduled_by_chart_time_minus_output_offset() {
    let bgm = bgm_every_half_second(1, 5);
    let mut dev = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let mut e = engine(&bgm);
    e.set_output_offset(TimeUs(3_000));
    let starts = run(&mut e, &mut dev, TimeUs(6_000_000));
    // A steady device: each start lies within one frame (20.8 µs) before
    // its time minus the offset (the frame mapping floors).
    let worst = worst_bgm_error(&starts, &dev, 3_000, 8);
    assert!(worst <= 21, "{worst}");
}

#[test]
fn underrun_resets_the_clock_and_keeps_starts_on_time() {
    // Wakes for passes 600-603 never come: passes 602 and 603 and the
    // first woken pass, 604, find the buffer empty, so later frames play
    // 3 periods late. BGM right after the underrun, within the 8
    // observations a late jump needs to rebase, must still be on time.
    let bgm: Vec<BgmEvent> = [5_500_000, 6_070_000, 6_100_000, 7_000_000]
        .into_iter()
        .map(|t| BgmEvent {
            time: TimeUs(t),
            sound: CLICK,
        })
        .collect();
    let mut dev = SimDevice::new(SimConfig {
        missed_wakes: vec![600, 601, 602, 603],
        ..SimConfig::steady(RATE, PERIOD, TimeUs(0))
    });
    let mut e = engine(&bgm);
    let starts = run(&mut e, &mut dev, TimeUs(8_000_000));
    assert_eq!(dev.underruns(), 3);
    assert_eq!(e.underruns(), 1);
    let worst = worst_bgm_error(&starts, &dev, 0, 4);
    assert!(worst <= PERIOD_US, "{worst}");
}

#[test]
fn resume_discontinuity_rebases_the_clock() {
    // From pass 300 every pass comes 50 ms later. The clock rebases after
    // the run of late observations; BGM after that is on time again.
    let bgm: Vec<BgmEvent> = [2_000_000, 3_500_000, 4_000_000]
        .into_iter()
        .map(|t| BgmEvent {
            time: TimeUs(t),
            sound: CLICK,
        })
        .collect();
    let mut dev = SimDevice::new(SimConfig {
        resumes: vec![Resume {
            at_pass: 300,
            gap_us: 50_000,
        }],
        jitter_us: 500,
        seed: 3,
        ..SimConfig::steady(RATE, PERIOD, TimeUs(0))
    });
    let mut e = engine(&bgm);
    let starts = run(&mut e, &mut dev, TimeUs(5_000_000));
    assert_eq!(e.clock().rebases(), 1);
    assert_eq!(e.underruns(), 0);
    let worst = worst_bgm_error(&starts, &dev, 0, 3);
    assert!(worst <= PERIOD_US, "{worst}");
}

#[test]
fn starts_beyond_the_pending_capacity_wait_for_the_next_step() {
    // 1100 one-frame sounds within the first two-period fill at 96 kHz
    // (frame 0 plays at 10 ms), once as BGM 10 µs apart from 10 ms and once
    // as key commands in one step: more than the 1024 starts the mixer
    // holds. Each is its own sound (a start of the same one would stop the
    // last). None is dropped: every start is applied at its frame, where as
    // many sound as the 128 voices hold.
    const N: usize = 1_100;
    const RATE96: u32 = 96_000;
    let bank = || {
        Arc::new(SoundBank::from_sounds(
            RATE96,
            (0..N).map(|_| Sound::mono(vec![100])).collect(),
        ))
    };
    for as_bgm in [true, false] {
        let bgm: Vec<BgmEvent> = if as_bgm {
            (0..N as i64)
                .map(|k| BgmEvent {
                    time: TimeUs(10_000 + k * 10),
                    sound: SoundId(k as u16),
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut e = AudioEngine::new(bank(), &bgm, RATE96, 960, 128, 2 * N).unwrap();
        let dev = SimDevice::new(SimConfig::steady(RATE96, 960, TimeUs(0)));
        let mut out = RecordingOutput::new(dev, 96_000);
        let (mut tx, mut rx) = RingBuffer::new(2 * N);
        if !as_bgm {
            for k in 0..N as i64 {
                tx.push(SoundCmd {
                    sound: SoundId(k as u16),
                    origin: TimeUs(k),
                })
                .unwrap();
            }
        }
        let mut starts = Vec::new();
        let (mut bgm_started, mut commands) = (0, 0);
        for _ in 0..20 {
            let r = e.step(&mut out, &mut rx).unwrap();
            bgm_started += r.bgm_started;
            commands += r.commands;
            drain(&mut e, &mut starts);
        }
        assert_eq!(starts.len(), N, "bgm {as_bgm}");
        assert_eq!(if as_bgm { bgm_started } else { commands }, N as u32);
        assert_eq!(e.mixer().dropped_starts(), 0);
        assert_eq!(e.mixer().cuts(), 0);
        let left: Vec<f32> = out.samples().iter().step_by(2).copied().collect();
        // A sample of 100, halved by the headroom; 128 of them, 0.2, stay
        // below the clipper's knee, and every such sum is exact in f32.
        let one = 100.0 / 65_536.0;
        let mut per_frame = vec![0usize; left.len()];
        for s in &starts {
            per_frame[s.frame as usize] += 1;
        }
        let first = starts[0].frame as usize;
        if as_bgm {
            // At most two share a frame, except the 76 that waited, which
            // all start at the first frame of the next block: nothing is
            // stolen.
            assert_eq!(per_frame[first + 1_920], 76);
            assert_eq!(e.mixer().steals(), 0);
        } else {
            // Key sounds start at the first unwritten frame: 1024 in the
            // first fill, of which the 128 voices sound 128 (each other
            // start steals a voice that has not sounded: 896 steals), and
            // the other 76 at the start of the next block.
            assert_eq!(per_frame[first], 1_024);
            assert_eq!(per_frame[first + 1_920], 76);
            assert_eq!(left[first], 128.0 * one);
            assert_eq!(left[first + 1_920], 76.0 * one);
            assert_eq!(e.mixer().steals(), 896);
        }
        // Each frame holds one sample per start there, up to the 128 voices,
        // and is silent elsewhere.
        for (f, (&x, &n)) in left.iter().zip(&per_frame).enumerate() {
            assert_eq!(x, n.min(128) as f32 * one, "bgm {as_bgm}, frame {f}");
        }
    }
}

#[test]
fn a_bgm_event_and_a_scheduled_key_sound_at_one_time_sound_once() {
    // Autoplay merges its key sounds into the BGM schedule: a BGM event and
    // a key entry on the same sound at 100 ms are two scheduled starts at
    // one frame but a single instance, sample for sample.
    let event = BgmEvent {
        time: TimeUs(100_000),
        sound: KEY,
    };
    let mut e =
        AudioEngine::with_schedule(bank(), vec![event; 2], RATE, PERIOD, 64, 1_024).unwrap();
    let dev = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let mut out = RecordingOutput::new(dev, 48_000);
    let (_tx, mut rx) = queue();
    let mut starts = Vec::new();
    while out.inner().next_wake() <= TimeUs(300_000) {
        e.step(&mut out, &mut rx).unwrap();
        drain(&mut e, &mut starts);
    }
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0].frame, starts[1].frame);
    let left: Vec<f32> = out.samples().iter().step_by(2).copied().collect();
    let first = starts[0].frame as usize;
    assert!(left[..first].iter().all(|&x| x == 0.0));
    for (i, &x) in left[first..first + 300].iter().enumerate() {
        let want = f32::from(KEY_FIRST - 17 * i as i16) / 65_536.0;
        assert_eq!(x, want, "frame {}", first + i);
    }
    assert!(left[first + 300..].iter().all(|&x| x == 0.0));
    assert_eq!(e.mixer().cuts(), 1);
}

#[test]
fn engine_refuses_zero_rate_or_period_and_a_bank_at_another_rate() {
    let new = |rate, period| AudioEngine::new(bank(), &[], rate, period, 4, 4).map(|_| ());
    assert_eq!(new(0, PERIOD), Err(EngineError::ZeroRateOrPeriod));
    assert_eq!(new(RATE, 0), Err(EngineError::ZeroRateOrPeriod));
    assert_eq!(
        new(44_100, 441),
        Err(EngineError::RateMismatch {
            bank: RATE,
            output: 44_100
        })
    );
    assert_eq!(new(RATE, PERIOD), Ok(()));
}

#[test]
fn report_tracks_voices_bgm_and_a_full_schedule_log() {
    let bgm = bgm_every_half_second(1, 2);
    // Sounds 0 and 2 outlast a block (2000 frames); sound 1 is the click.
    let long = || Sound::mono(vec![100; 2_000]);
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![long(), Sound::mono(vec![-100; 48]), long()],
    ));
    let mut e = AudioEngine::new(bank, &bgm, RATE, PERIOD, 4, 1).unwrap();
    let mut dev = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let (mut tx, mut rx) = queue();
    let r = e.step(&mut dev, &mut rx).unwrap();
    assert_eq!((r.frames, r.bgm_left, r.active_voices), (960, 2, 0));
    // Two key sounds and one the bank lacks: two voices, one log entry
    // kept (capacity 1), one dropped, the unknown sound ignored.
    for sound in [KEY, SoundId(2), SoundId(9)] {
        tx.push(SoundCmd {
            sound,
            origin: TimeUs(0),
        })
        .unwrap();
    }
    let r = e.step(&mut dev, &mut rx).unwrap();
    assert_eq!((r.commands, r.frames, r.active_voices), (3, 480, 2));
    assert_eq!(e.scheduled_dropped(), 1);
    // Through both BGM events and past every sound: the tail is over.
    let mut last = r;
    while dev.next_wake() <= TimeUs(2_000_000) {
        last = e.step(&mut dev, &mut rx).unwrap();
    }
    assert_eq!((last.bgm_left, last.active_voices), (0, 0));
    // A retry with chart time 0 at 3 s rewinds the BGM: its events start
    // at 4 s and 4.5 s on the device's timeline.
    drain(&mut e, &mut Vec::new());
    e.restart(TimeUs(3_000_000));
    let r = e.step(&mut dev, &mut rx).unwrap();
    assert_eq!(r.bgm_left, 2);
    let mut starts = Vec::new();
    while dev.next_wake() <= TimeUs(5_000_000) {
        e.step(&mut dev, &mut rx).unwrap();
        drain(&mut e, &mut starts);
    }
    let times: Vec<i64> = starts.iter().map(|s| dev.true_time(s.frame).0).collect();
    assert_eq!(times.len(), 2);
    for (t, want) in times.into_iter().zip([4_000_000, 4_500_000]) {
        assert!((want - 21..=want).contains(&t), "{t} vs {want}");
    }
}

#[test]
fn recording_output_counts_what_exceeds_its_capacity() {
    let mut out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        100,
    );
    out.wait();
    out.write(|buf| buf.fill(0.5)).unwrap();
    assert_eq!(out.samples().len(), 200);
    assert!(out.samples().iter().all(|&x| x == 0.5));
    assert_eq!(out.dropped(), 860);
}

#[test]
fn a_simulated_device_that_queues_nothing_is_refused() {
    for (queue_periods, period, rate) in [(0, PERIOD, RATE), (2, 0, RATE), (2, PERIOD, 0)] {
        let cfg = SimConfig {
            queue_periods,
            ..SimConfig::steady(rate, period, TimeUs(0))
        };
        let refused = std::panic::catch_unwind(|| SimDevice::new(cfg));
        assert!(refused.is_err(), "{queue_periods} {period} {rate}");
    }
    let _ = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
}

/// A simulated device whose first wake is marked priming.
struct Priming {
    inner: SimDevice,
    primed: bool,
}

impl AudioOutput for Priming {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }

    fn period(&self) -> u32 {
        self.inner.period()
    }

    fn wait(&mut self) -> audio::Wake {
        let mut w = self.inner.wait();
        w.priming = !std::mem::replace(&mut self.primed, true);
        w
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), audio::OutputError> {
        self.inner.write(f)
    }
}

#[test]
fn a_priming_wake_fills_silence_and_leaves_timing_to_the_first_timed_wake() {
    let mut e = engine(&[BgmEvent {
        time: TimeUs(0),
        sound: CLICK,
    }]);
    let dev = Priming {
        inner: SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        primed: false,
    };
    let mut out = RecordingOutput::new(dev, 4 * PERIOD as usize);
    let (mut tx, mut rx) = queue();
    tx.push(SoundCmd {
        sound: KEY,
        origin: TimeUs(0),
    })
    .unwrap();
    let r = e.step(&mut out, &mut rx).unwrap();
    // Two silent periods, no command taken, no BGM started, no clock.
    assert_eq!((r.frames, r.commands, r.bgm_started), (960, 0, 0));
    assert_eq!(e.clock().time_at(0), None);
    assert_eq!(e.written(), 960);
    assert_eq!(out.samples().len(), 1_920);
    assert!(out.samples().iter().all(|&x| x == 0.0));
    let r = e.step(&mut out, &mut rx).unwrap();
    assert_eq!((r.frames, r.commands, r.bgm_started), (480, 1, 1));
    assert!(e.clock().time_at(0).is_some());
}

/// A bank holding one 10 ms 44.1 kHz click converted for `RATE` at `speed`.
fn click_bank_at(speed: Speed) -> SoundBank {
    let dir = common::tmp();
    let path = common::put(
        dir.path(),
        "click.wav",
        &common::wav(44_100, 1, &common::ramp(441, -9_000, 41)),
    );
    load_bank_at(
        &common::chart_of(&[Some(path)]),
        RATE,
        speed,
        &BankLimits::default(),
    )
    .unwrap()
}

#[test]
fn engine_accepts_a_bank_at_any_speed() {
    for speed in Speed::ladder() {
        let k = speed.twentieths();
        let bank = click_bank_at(speed);
        // The bank stays at the device rate; only its sound lengths change:
        // 480 device frames become floor(480 × 20/k).
        assert_eq!((bank.rate(), bank.speed()), (RATE, speed));
        let frames = bank.get(SoundId(0)).unwrap().frames();
        assert_eq!(frames, 480 * 20 / u64::from(k));
        let bgm = [BgmEvent {
            time: TimeUs(0),
            sound: SoundId(0),
        }];
        let mut e = AudioEngine::new(Arc::new(bank), &bgm, RATE, PERIOD, 4, 4).unwrap();
        let mut dev = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
        // The click starts and, 20 ms at most later, has played out.
        let starts = run(&mut e, &mut dev, TimeUs(100_000));
        assert_eq!(starts.len(), 1, "speed {k}");
        assert_eq!(e.mixer().sounding(), 0, "speed {k}");
    }
}

#[test]
fn starts_align_across_the_ladder() {
    // Exact chart times mapped as the loader maps them, floor(t × 20/k),
    // started after a retry's origin: each start frame lies within one
    // frame of the exact t × rate / 1 000 000 after the origin's frame,
    // compared without rounding (1.234567 s is 59 259.216 frames).
    let exact = [1_000_000i64, 1_234_567, 2_500_001, 7_777_777];
    let origin = TimeUs(500_000);
    for speed in Speed::ladder() {
        let k = i64::from(speed.twentieths());
        let mapped: Vec<i64> = exact.iter().map(|t| t * 20 / k).collect();
        let bgm: Vec<BgmEvent> = mapped
            .iter()
            .map(|&t| BgmEvent {
                time: TimeUs(t),
                sound: SoundId(0),
            })
            .collect();
        let bank = Arc::new(click_bank_at(speed));
        let mut e = AudioEngine::new(bank, &bgm, RATE, PERIOD, 8, 64).unwrap();
        let mut dev = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
        e.restart(origin);
        let starts = run(&mut e, &mut dev, TimeUs(origin.0 + 16_000_000));
        assert_eq!(starts.len(), mapped.len(), "speed {k}");
        let origin_frame = e.clock().frame_at(origin).unwrap() as i64;
        for (s, &t) in starts.iter().zip(&mapped) {
            let got = s.frame as i64 - origin_frame;
            let off = got * 1_000_000 - t * i64::from(RATE);
            assert!(
                off.abs() <= 1_000_000,
                "speed {k}: frame {got} for {t} us is {off} / 1 000 000 frames off"
            );
        }
    }
}
