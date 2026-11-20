//! A measurement, not a check (ignored; run by hand): how the media worker
//! keeps up under CPU load at each priority class, to decide its priority
//! and its deadlines (design.md "Threat Model" T02). Launches the real
//! worker under the recipe, idle and with normal-priority busy threads
//! filling every logical CPU and more, at below-normal and normal priority:
//! launch to hello (cold: a new profile and data folder; warm), open to
//! probe, play to the first frame, sustained playback of two videos at
//! once polled as the render thread polls, the control loop's beats, and
//! close to closed. The videos come from `MEDIA_MEASURE_DIR`; the results
//! go to `MEDIA_MEASURE_OUT` as one JSON object a line. The other
//! measurements here: the worker's CPU bound (`media_worker_cpu_bound`), a
//! screen's preparation and the loops' pass ends through the game's session
//! (`media_session_entry_and_loops`), and the game's own loops under the
//! worker's worst admitted load (`media_game_loops_under_the_worst_load`).

#![cfg(all(windows, feature = "dev-tools"))]

mod media_support;

use std::io::Write as _;
use std::os::windows::io::{AsRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use media_support::{Kind, Launched, Setup, Stage, media_host, recipe};
use skin_model::media::control::{Command as Order, Reply};
use skin_model::media::{Geometry, Reader, Window};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{
    FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows::Win32::System::Threading::SetEvent;

const REPLY: Duration = Duration::from_secs(60);

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn count(name: &str, default: usize) -> usize {
    env_or(name, &default.to_string()).parse().unwrap()
}

struct Out(std::fs::File);

impl Out {
    fn line(&mut self, json: &serde_json::Value) {
        let text = json.to_string();
        println!("{text}");
        writeln!(self.0, "{text}").unwrap();
        self.0.flush().unwrap();
    }
}

/// Busy threads at normal priority, until dropped.
struct Load {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Load {
    fn new(n: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..n)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let mut x = 0_u64;
                    while !stop.load(Ordering::Relaxed) {
                        x = std::hint::black_box(
                            x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1),
                        );
                    }
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(500));
        Self { stop, threads }
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

struct Ring {
    section: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    event: OwnedHandle,
    reader: Reader,
}

impl Ring {
    fn new(width: u32, height: u32) -> Self {
        let geometry = Geometry::new(width, height).unwrap();
        let (section, event) =
            media_host::ring_objects(u64::try_from(geometry.bytes()).unwrap()).unwrap();
        // SAFETY: the section just made, mapped whole.
        let view = unsafe {
            MapViewOfFile(
                HANDLE(section.as_raw_handle()),
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                0,
            )
        };
        assert!(!view.Value.is_null());
        let len = geometry.words();
        let reader = Reader::new(
            Self::words(view, len),
            geometry,
            Window {
                first: 0,
                last: i64::MAX,
            },
        )
        .unwrap();
        Self {
            section,
            view,
            len,
            event,
            reader,
        }
    }

    fn words<'a>(view: MEMORY_MAPPED_VIEW_ADDRESS, len: usize) -> &'a [AtomicU64] {
        // SAFETY: the view holds the ring's words while the ring lives.
        unsafe { std::slice::from_raw_parts(view.Value.cast::<AtomicU64>(), len) }
    }

    /// The stamp staged at `due`, if a new frame was.
    fn poll(&mut self, due: i64) -> Option<i64> {
        let poll = self
            .reader
            .poll(Self::words(self.view, self.len), due)
            .unwrap_or_else(|f| panic!("the ring failed its player: {f}"));
        if poll.freed {
            // SAFETY: the ring's event.
            unsafe { SetEvent(HANDLE(self.event.as_raw_handle())) }.unwrap();
        }
        poll.staged.map(|f| f.stamp)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: the view this ring mapped.
        let _ = unsafe { UnmapViewOfFile(self.view) };
    }
}

/// A reply other than `Ended`s, waiting up to [`REPLY`].
fn reply(w: &Launched) -> (Reply, Instant) {
    loop {
        let r = w
            .recv(REPLY)
            .unwrap_or_else(|e| panic!("no reply ({e}):\n{}", w.log()));
        if !matches!(r, Reply::Ended { .. }) {
            return (r, Instant::now());
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

fn launch_ms(stage: &Stage, setup: &Setup) -> (Result<Launched, String>, f64) {
    let t = Instant::now();
    let r = stage.launch(setup).map_err(|e| e.to_string());
    (r, ms(t.elapsed()))
}

struct Player {
    id: u8,
    ring: Ring,
    _file: std::fs::File,
    /// Stamps staged in the current pass, with when.
    staged: Vec<(i64, Instant)>,
    lags: Vec<f64>,
    skipped: usize,
    expected: usize,
    gaps: Vec<f64>,
    passes: usize,
    duration: i64,
}

fn open(w: &mut Launched, id: u8, path: &Path, out: &mut Out, tag: &serde_json::Value) -> Player {
    let file = std::fs::File::open(path).unwrap();
    let handle = w.hand_in(HANDLE(file.as_raw_handle()), Kind::File).unwrap();
    let t = Instant::now();
    w.send(&Order::Open {
        player: id,
        file: handle,
    })
    .unwrap();
    let (r, at) = reply(w);
    let Reply::Probed {
        width,
        height,
        duration,
        ..
    } = r
    else {
        panic!("{r:?}");
    };
    let probe_ms = ms(at - t);
    let ring = Ring::new(width, height);
    let section = w
        .hand_in(HANDLE(ring.section.as_raw_handle()), Kind::Section)
        .unwrap();
    let event = w
        .hand_in(HANDLE(ring.event.as_raw_handle()), Kind::Event)
        .unwrap();
    let t = Instant::now();
    w.send(&Order::Play {
        player: id,
        section,
        event,
        looping: false,
    })
    .unwrap();
    let (r, _) = reply(w);
    assert!(matches!(r, Reply::Playing { .. }), "{r:?}");
    let mut p = Player {
        id,
        ring,
        _file: file,
        staged: Vec::new(),
        lags: Vec::new(),
        skipped: 0,
        expected: 0,
        gaps: Vec::new(),
        passes: 0,
        duration,
    };
    let deadline = Instant::now() + REPLY;
    let first = loop {
        if let Some(s) = p.ring.poll(0) {
            break s;
        }
        assert!(Instant::now() < deadline, "no first frame");
        std::thread::sleep(Duration::from_micros(500));
    };
    out.line(&serde_json::json!({
        "kind": "open", "tag": tag, "player": id, "file": path.file_name().unwrap().to_string_lossy(),
        "width": width, "height": height, "duration_us": duration,
        "probe_ms": probe_ms, "first_frame_ms": ms(t.elapsed()), "first_stamp": first,
    }));
    p
}

/// Plays `players` for `seconds`, polling every `poll_us` with due = wall
/// time since each pass's start; a pass ends `duration` in and restarts
/// with a seek to 0 in a new run.
/// Returns the gaps between the poller's iterations, sorted, in ms: the
/// poller's own delay under the load.
fn sustain(w: &mut Launched, players: &mut [Player], seconds: u64, poll_us: u64) -> Vec<f64> {
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut origins: Vec<Instant> = players.iter().map(|_| Instant::now()).collect();
    let poll = Duration::from_micros(poll_us);
    let mut next = Instant::now();
    let mut iterations = Vec::new();
    let mut last = Instant::now();
    while Instant::now() < end {
        iterations.push(ms(last.elapsed()));
        last = Instant::now();
        for (p, origin) in players.iter_mut().zip(origins.iter_mut()) {
            let due = i64::try_from(origin.elapsed().as_micros()).unwrap();
            if due >= p.duration - 20_000 {
                finish_pass(p, due);
                let run = p.ring.reader.start_run(Window {
                    first: 0,
                    last: i64::MAX,
                });
                w.send(&Order::Seek {
                    player: p.id,
                    run,
                    at: 0,
                })
                .unwrap();
                loop {
                    let (r, _) = reply(w);
                    if matches!(r, Reply::Sought { player, .. } if player == p.id) {
                        break;
                    }
                }
                *origin = Instant::now();
                continue;
            }
            if let Some(s) = p.ring.poll(due) {
                p.staged.push((s, Instant::now()));
            }
            if let Some(&(s, _)) = p.staged.last() {
                p.lags.push((due - s) as f64 / 1000.0);
            }
        }
        while let Some(r) = w.try_recv() {
            assert!(
                matches!(r, Reply::Ended { .. } | Reply::Sought { .. }),
                "{r:?}"
            );
        }
        next += poll;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    for (p, origin) in players.iter_mut().zip(&origins) {
        let due = i64::try_from(origin.elapsed().as_micros()).unwrap();
        finish_pass(p, due);
    }
    iterations.sort_by(f64::total_cmp);
    iterations
}

/// Counts the frames due in the pass that were never staged, at 60 fps
/// and 30 fps alike: a frame is due when its stamp is at most `due`; the
/// stamps the worker published are those staged plus those skipped, and
/// the interval is the median gap between staged stamps.
fn finish_pass(p: &mut Player, due: i64) {
    let staged = std::mem::take(&mut p.staged);
    if staged.len() >= 2 {
        let mut deltas: Vec<i64> = staged.windows(2).map(|w| w[1].0 - w[0].0).collect();
        deltas.sort_unstable();
        let interval = deltas[0].max(1);
        let expected = usize::try_from(due / interval + 1).unwrap_or(0);
        p.expected += expected;
        p.skipped += expected.saturating_sub(staged.len());
        p.gaps
            .extend(staged.windows(2).map(|w| ms(w[1].1 - w[0].1)));
    }
    p.passes += 1;
}

fn close(w: &mut Launched, id: u8) -> f64 {
    let t = Instant::now();
    w.send(&Order::Close { player: id }).unwrap();
    loop {
        let (r, at) = reply(w);
        if matches!(r, Reply::Closed { player, .. } if player == id) {
            return ms(at - t);
        }
    }
}

fn beat_gaps(beats: &[Duration], from: Duration) -> Vec<f64> {
    let after: Vec<Duration> = beats.iter().copied().filter(|b| *b >= from).collect();
    let mut g: Vec<f64> = after.windows(2).map(|w| ms(w[1] - w[0])).collect();
    g.sort_by(f64::total_cmp);
    g
}

#[test]
#[ignore = "a measurement, run by hand"]
fn media_worker_under_load() {
    let dir = PathBuf::from(env_or("MEDIA_MEASURE_DIR", "target/ms1a"));
    let mut out =
        Out(std::fs::File::create(env_or("MEDIA_MEASURE_OUT", "media-measure.jsonl")).unwrap());
    let launches = count("MEDIA_MEASURE_LAUNCHES", 20);
    let seconds = u64::try_from(count("MEDIA_MEASURE_SECONDS", 60)).unwrap();
    let busy = count("MEDIA_MEASURE_BUSY", 20);
    let cpus = std::thread::available_parallelism().map_or(0, usize::from);
    let videos: Vec<PathBuf> = env_or("MEDIA_MEASURE_VIDEOS", "default.mp4,generic.mp4")
        .split(',')
        .map(|v| dir.join(v))
        .collect();
    out.line(&serde_json::json!({"kind": "setup", "cpus": cpus, "busy": busy, "launches": launches, "seconds": seconds}));
    for loaded in [false, true] {
        let _load = loaded.then(|| Load::new(busy));
        for normal in [false, true] {
            let setup = if normal {
                recipe()
            } else {
                recipe().at_below_normal_priority()
            };
            let tag = serde_json::json!({"loaded": loaded, "priority": if normal { "normal" } else { "below_normal" }});
            // Cold launches: each in a new profile and data folder.
            for i in 0..launches {
                let stage = Stage::new();
                let (r, t) = launch_ms(&stage, &setup);
                out.line(&serde_json::json!({"kind": "launch", "tag": tag, "cold": true, "i": i, "ms": t, "error": r.err()}));
            }
            // Warm launches: the copies and the profile made.
            let stage = Stage::new();
            drop(launch_ms(&stage, &setup).0);
            for i in 0..launches {
                let (r, t) = launch_ms(&stage, &setup);
                out.line(&serde_json::json!({"kind": "launch", "tag": tag, "cold": false, "i": i, "ms": t, "error": r.err()}));
            }
            // Playback in one worker.
            let mut w = stage.launch(&setup).unwrap_or_else(|e| panic!("{e}"));
            let start = Duration::from_secs(0);
            let mut players: Vec<Player> = videos
                .iter()
                .zip(1_u8..)
                .map(|(v, id)| open(&mut w, id, v, &mut out, &tag))
                .collect();
            let from = w.beats().last().copied().unwrap_or(start);
            let iterations = sustain(&mut w, &mut players, seconds, 4_167);
            out.line(&serde_json::json!({
                "kind": "poller", "tag": tag,
                "iteration_ms": {"p50": pct(&iterations, 0.5), "p99": pct(&iterations, 0.99), "max": pct(&iterations, 1.0)},
            }));
            for p in &mut players {
                let mut lags = std::mem::take(&mut p.lags);
                lags.sort_by(f64::total_cmp);
                let mut gaps = std::mem::take(&mut p.gaps);
                gaps.sort_by(f64::total_cmp);
                out.line(&serde_json::json!({
                    "kind": "sustain", "tag": tag, "player": p.id, "passes": p.passes,
                    "expected": p.expected, "skipped": p.skipped,
                    "lag_ms": {"p50": pct(&lags, 0.5), "p99": pct(&lags, 0.99), "max": pct(&lags, 1.0)},
                    "publication_gap_ms": {"p50": pct(&gaps, 0.5), "p99": pct(&gaps, 0.99), "max": pct(&gaps, 1.0)},
                }));
            }
            let ids: Vec<u8> = players.iter().map(|p| p.id).collect();
            let closes: Vec<f64> = ids.iter().map(|&id| close(&mut w, id)).collect();
            drop(players);
            let gaps = beat_gaps(&w.beats(), from);
            out.line(&serde_json::json!({
                "kind": "beats", "tag": tag, "count": gaps.len() + 1,
                "gap_ms": {"p50": pct(&gaps, 0.5), "p99": pct(&gaps, 0.99), "max": pct(&gaps, 1.0)},
                "close_ms": closes,
            }));
            // Idle beats: a worker with no players for 10 s.
            let idle_from = w.beats().last().copied().unwrap_or(start);
            std::thread::sleep(Duration::from_secs(10));
            let gaps = beat_gaps(&w.beats(), idle_from);
            out.line(&serde_json::json!({
                "kind": "idle_beats", "tag": tag, "count": gaps.len() + 1,
                "gap_ms": {"p50": pct(&gaps, 0.5), "max": pct(&gaps, 1.0)},
            }));
        }
    }
}

/// A stand-in loop at normal priority: every `period`, `work` of busy
/// work; returns each iteration's lateness against its schedule, in ms.
fn stand_in(period: Duration, work: Duration, stop: &AtomicBool) -> Vec<f64> {
    let mut late = Vec::new();
    let mut next = Instant::now() + period;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        let start = Instant::now();
        late.push(ms(start.saturating_duration_since(next)));
        while start.elapsed() < work {
            std::hint::spin_loop();
        }
        next += period;
        if next < Instant::now() {
            next = Instant::now() + period;
        }
    }
    late.sort_by(f64::total_cmp);
    late
}

fn stats(v: &[f64]) -> serde_json::Value {
    serde_json::json!({"p50": pct(v, 0.5), "p99": pct(v, 0.99), "p999": pct(v, 0.999), "max": pct(v, 1.0), "n": v.len()})
}

/// Limits `process` to the logical CPUs in `mask`.
fn pin(process: HANDLE, mask: usize) {
    // SAFETY: a process handle with every access.
    unsafe { windows::Win32::System::Threading::SetProcessAffinityMask(process, mask) }.unwrap();
}

/// Limits `process` to the logical CPUs in `mask`, when its job allows
/// them (logged otherwise).
fn try_pin(process: HANDLE, mask: usize) {
    // SAFETY: a process handle with every access.
    let pinned =
        unsafe { windows::Win32::System::Threading::SetProcessAffinityMask(process, mask) };
    if let Err(e) = pinned {
        println!("the worker's CPUs stay its job's: {mask:#x} refused ({e})");
    }
}

/// Caps `job`'s CPU at `rate` hundredths of a percent of the machine.
fn cap(job: HANDLE, rate: u32) {
    use windows::Win32::System::JobObjects::{
        JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
        JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JobObjectCpuRateControlInformation,
        SetInformationJobObject,
    };
    let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
        ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
        ..Default::default()
    };
    info.Anonymous.CpuRate = rate;
    // SAFETY: the worker's job and a struct of the size given.
    unsafe {
        SetInformationJobObject(
            job,
            JobObjectCpuRateControlInformation,
            (&raw const info).cast(),
            u32::try_from(size_of_val(&info)).unwrap(),
        )
    }
    .unwrap();
}

/// The CPU time `process` used so far, in ms.
fn cpu_ms(process: HANDLE) -> f64 {
    use windows::Win32::Foundation::FILETIME;
    let (mut a, mut b, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: a process handle with every access, and four out values.
    unsafe {
        windows::Win32::System::Threading::GetProcessTimes(
            process, &raw mut a, &raw mut b, &raw mut k, &raw mut u,
        )
    }
    .unwrap();
    let t = |f: FILETIME| {
        ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as f64 / 10_000.0
    };
    t(k) + t(u)
}

/// Plays `players` from their start for `seconds`, polled every
/// `poll_us` with due = wall time: skips and lag per player.
fn play_for(players: &mut [Player], seconds: f64, poll_us: u64) {
    let origin = Instant::now();
    let end = origin + Duration::from_secs_f64(seconds);
    let poll = Duration::from_micros(poll_us);
    let mut next = Instant::now();
    while Instant::now() < end {
        let due = i64::try_from(origin.elapsed().as_micros()).unwrap();
        for p in players.iter_mut() {
            if let Some(s) = p.ring.poll(due) {
                p.staged.push((s, Instant::now()));
            }
            if let Some(&(s, _)) = p.staged.last() {
                p.lags.push((due - s) as f64 / 1000.0);
            }
        }
        next += poll;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    let due = i64::try_from(origin.elapsed().as_micros()).unwrap();
    for p in players.iter_mut() {
        finish_pass(p, due);
    }
}

#[test]
#[ignore = "a measurement, run by hand"]
fn media_worker_cpu_bound() {
    let dir = PathBuf::from(env_or("MEDIA_MEASURE_DIR", "target/ms1c"));
    let mut out =
        Out(std::fs::File::create(env_or("MEDIA_MEASURE_OUT", "media-cpu.jsonl")).unwrap());
    let mask: usize = count("MEDIA_MEASURE_MASK", 0xF);
    let seconds: f64 = env_or("MEDIA_MEASURE_SECONDS", "9").parse().unwrap();
    // SAFETY: this process's pseudo handle.
    pin(
        unsafe { windows::Win32::System::Threading::GetCurrentProcess() },
        mask,
    );
    out.line(&serde_json::json!({"kind": "setup", "mask": mask, "seconds": seconds}));
    let loads: [(&str, Vec<&str>); 2] = [
        ("cap4", vec!["cap2048.mp4"; 4]),
        ("real2", vec!["default.mp4", "generic.mp4"]),
    ];
    // Each configuration: the worker's CPUs (a subset of `mask`) and its
    // job's hard cap; a rate of `Some(0)` is the baseline, no worker.
    let configs: Vec<(usize, Option<u32>)> =
        env_or("MEDIA_MEASURE_CONFIGS", "f:0,f:-,f:5000,f:2500,f:1250")
            .split(',')
            .map(|c| {
                let (m, r) = c.split_once(':').unwrap();
                (
                    usize::from_str_radix(m, 16).unwrap(),
                    (r != "-").then(|| r.parse().unwrap()),
                )
            })
            .collect();
    for (load, files) in &loads {
        for &(worker_mask, rate) in &configs {
            let tag = serde_json::json!({"load": load, "rate": rate, "worker_mask": worker_mask});
            let stop = Arc::new(AtomicBool::new(false));
            let game = {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    stand_in(Duration::from_millis(1), Duration::from_micros(300), &stop)
                })
            };
            let render = {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    stand_in(
                        Duration::from_micros(4_167),
                        Duration::from_millis(2),
                        &stop,
                    )
                })
            };
            let stage = Stage::new();
            let mut cpu = None;
            if rate == Some(0) {
                std::thread::sleep(Duration::from_secs_f64(seconds));
            } else {
                let mut w = stage.launch(&recipe()).unwrap_or_else(|e| panic!("{e}"));
                // The recipe's job already holds the worker to the upper
                // half of this process's CPUs; a mask outside it is refused.
                try_pin(w.process(), worker_mask);
                if let Some(rate) = rate {
                    cap(w.job().unwrap(), rate);
                }
                let mut players: Vec<Player> = files
                    .iter()
                    .zip(1_u8..)
                    .map(|(f, id)| open(&mut w, id, &dir.join(f), &mut out, &tag))
                    .collect();
                let before = cpu_ms(w.process());
                play_for(&mut players, seconds, 16_667);
                cpu = Some((cpu_ms(w.process()) - before) / (seconds * 1000.0));
                for p in &mut players {
                    let mut lags = std::mem::take(&mut p.lags);
                    lags.sort_by(f64::total_cmp);
                    out.line(&serde_json::json!({
                        "kind": "movie", "tag": tag, "player": p.id,
                        "expected": p.expected, "skipped": p.skipped, "lag_ms": stats(&lags),
                    }));
                }
                let ids: Vec<u8> = players.iter().map(|p| p.id).collect();
                for id in ids {
                    close(&mut w, id);
                }
            }
            stop.store(true, Ordering::Relaxed);
            let game = game.join().unwrap();
            let render = render.join().unwrap();
            out.line(&serde_json::json!({
                "kind": "loops", "tag": tag, "worker_cpus": cpu,
                "game_late_ms": stats(&game), "render_late_ms": stats(&render),
            }));
        }
    }
}

/// Opens `path` as player `id` and plays it looping into a ring of its
/// own: the ring, once the first frame came.
fn open_looping(w: &mut Launched, id: u8, path: &Path) -> (Ring, std::fs::File) {
    let file = std::fs::File::open(path).unwrap();
    let handle = w.hand_in(HANDLE(file.as_raw_handle()), Kind::File).unwrap();
    w.send(&Order::Open {
        player: id,
        file: handle,
    })
    .unwrap();
    let (r, _) = reply(w);
    let Reply::Probed { width, height, .. } = r else {
        panic!("{r:?}");
    };
    let ring = Ring::new(width, height);
    let section = w
        .hand_in(HANDLE(ring.section.as_raw_handle()), Kind::Section)
        .unwrap();
    let event = w
        .hand_in(HANDLE(ring.event.as_raw_handle()), Kind::Event)
        .unwrap();
    w.send(&Order::Play {
        player: id,
        section,
        event,
        looping: true,
    })
    .unwrap();
    let (r, _) = reply(w);
    assert!(matches!(r, Reply::Playing { .. }), "{r:?}");
    (ring, file)
}

/// A chart of `measures` measures at 120 BPM (2 s each), a note on every
/// beat of lane 1, and its key sound, in `dir`.
fn write_chart(dir: &Path, measures: u32) -> PathBuf {
    let samples: Vec<i16> = (0..2_205)
        .map(|i| if i % 50 < 25 { 8_000 } else { -8_000 })
        .collect();
    let data_len = u32::try_from(samples.len() * 2).unwrap();
    let mut wav = Vec::new();
    wav.extend(b"RIFF");
    wav.extend((36 + data_len).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16_u32.to_le_bytes());
    wav.extend(1_u16.to_le_bytes());
    wav.extend(1_u16.to_le_bytes());
    wav.extend(44_100_u32.to_le_bytes());
    wav.extend(88_200_u32.to_le_bytes());
    wav.extend(2_u16.to_le_bytes());
    wav.extend(16_u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(data_len.to_le_bytes());
    for s in samples {
        wav.extend(s.to_le_bytes());
    }
    std::fs::write(dir.join("click.wav"), wav).unwrap();
    let mut chart = String::from("#PLAYER 1\n#TITLE load\n#BPM 120\n#WAV01 click.wav\n");
    for m in 1..=measures {
        chart.push_str(&format!("#{m:03}11:01010101\n"));
    }
    let path = dir.join("load.bms");
    std::fs::write(&path, chart).unwrap();
    path
}

/// Every `*.measure.toml` under `dir`.
fn measure_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.to_string_lossy().ends_with(".measure.toml") {
                found.push(p);
            }
        }
    }
    found
}

/// MS3: the game's own logic and render loops (a whole play, headless,
/// with generated input, its `.measure.toml`), alone and while the worker
/// decodes its worst admitted load under the recipe (its CPU bound
/// included): four players at the pixel cap, looping, polled at 60 Hz.
#[test]
#[ignore = "a measurement, run by hand"]
fn media_game_loops_under_the_worst_load() {
    let cap = PathBuf::from(env_or("MEDIA_MEASURE_CAP", "target/ms1c/cap2048.mp4"));
    let mut out =
        Out(std::fs::File::create(env_or("MEDIA_MEASURE_OUT", "media-game.jsonl")).unwrap());
    let repeats = count("MEDIA_MEASURE_REPEATS", 3);
    let measures: u32 = env_or("MEDIA_MEASURE_MEASURES", "15").parse().unwrap();
    out.line(&serde_json::json!({
        "kind": "setup", "repeats": repeats, "measures": measures,
        "worker_cpus": media_host::launch::placement().ok().map(|p| p.worker),
    }));
    for run in 0..repeats {
        for load in ["none", "cap4"] {
            let dir = tempfile::Builder::new()
                .prefix("ms3-game-")
                .tempdir()
                .unwrap();
            let chart = write_chart(dir.path(), measures);
            let appdata = dir.path().join("appdata");
            std::fs::create_dir_all(&appdata).unwrap();
            let stage = Stage::new();
            let mut worker = None;
            let mut rings = Vec::new();
            if load == "cap4" {
                let mut w = stage.launch(&recipe()).unwrap_or_else(|e| panic!("{e}"));
                for id in 1..=4 {
                    rings.push(open_looping(&mut w, id, &cap));
                }
                worker = Some(w);
            }
            let cpu_before = worker.as_ref().map(|w| cpu_ms(w.process()));
            let started = Instant::now();
            let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tapestry"))
                .arg("play")
                .arg(&chart)
                .args([
                    "--dev-perfect",
                    "--dev-headless",
                    "--dev-render-timeout",
                    "600",
                ])
                .env("LOCALAPPDATA", &appdata)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            // The render side's stand-in for the four movies, at 60 Hz, as
            // long as the play runs.
            let origin = Instant::now();
            let mut staged = 0_u64;
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                let due = i64::try_from(origin.elapsed().as_micros()).unwrap();
                for (ring, _) in &mut rings {
                    staged += u64::from(ring.poll(due).is_some());
                }
                std::thread::sleep(Duration::from_micros(16_667));
            };
            let seconds = started.elapsed().as_secs_f64();
            let worker_cpus = worker
                .as_ref()
                .zip(cpu_before)
                .map(|(w, before)| (cpu_ms(w.process()) - before) / (seconds * 1000.0));
            let output = child.wait_with_output().unwrap();
            assert!(
                status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let files = measure_files(&appdata);
            assert_eq!(files.len(), 1, "{files:?}");
            let figures: toml::Table = std::fs::read_to_string(&files[0]).unwrap().parse().unwrap();
            out.line(&serde_json::json!({
                "kind": "game", "run": run, "load": load, "seconds": seconds,
                "movie_frames_staged": staged, "worker_cpus": worker_cpus,
                "measure": serde_json::to_value(&figures).unwrap(),
            }));
            if let Some(mut w) = worker {
                for id in 1..=4 {
                    close(&mut w, id);
                }
            }
        }
    }
}

/// A movie's frames as the render side's stand-in stages them: each
/// stamp and when.
struct Drawn {
    stop: AtomicBool,
    staged: std::sync::Mutex<Vec<(i64, Instant)>>,
}

/// Polls `movie`'s ring every 4 ms at the media time since its first
/// poll, telling the session what it freed and staged, until stopped.
fn draw(movie: &media_host::session::Movie, drawn: &Drawn) {
    let mut reader: Option<Reader> = None;
    let mut origin = None;
    while !drawn.stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(4));
        if movie.status() == media_host::session::Status::Lost {
            continue;
        }
        let Some(live) = movie.live() else { continue };
        if reader.is_none() {
            reader = live.reader.lock().unwrap().take();
        }
        let Some(r) = reader.as_mut() else { continue };
        let origin = *origin.get_or_insert_with(Instant::now);
        let due = i64::try_from(origin.elapsed().as_micros()).unwrap();
        match r.poll(live.ring.words(), due) {
            Ok(p) => {
                if let Some(f) = p.staged {
                    drawn.staged.lock().unwrap().push((f.stamp, Instant::now()));
                    movie.staged_first();
                }
                if p.freed {
                    movie.freed(r.freed());
                }
            }
            Err(_) => {
                movie.lose(media_host::session::Loss::Fault);
            }
        }
    }
}

/// A movie, its stand-in's frames and the stand-in's thread.
type Drawing = (
    Arc<media_host::session::Movie>,
    Arc<Drawn>,
    std::thread::JoinHandle<()>,
);

/// A session over `stage` at the game's deadlines.
fn session_on(stage: &Stage) -> media_host::session::Session {
    media_host::session::Session::start(
        media_host::session::Launching {
            installation: stage.dir().to_owned(),
            data: stage.data().to_owned(),
            setup: recipe().in_profile(stage.profile()),
        },
        media_host::session::Deadlines::GAME,
    )
    .unwrap()
}

/// Opens each of `files` for the screen of `generation` and draws it:
/// the time from the open to each one's first staged frame (`None` when
/// it settled otherwise or not within 10 s), and the movies with their
/// stand-ins.
fn enter_with(
    session: &media_host::session::Session,
    generation: u64,
    files: &[PathBuf],
) -> (Vec<Option<f64>>, Vec<Drawing>) {
    session.enter(generation);
    let started = Instant::now();
    let movies: Vec<_> = files
        .iter()
        .map(|f| {
            let movie = media_host::session::Movie::new(generation);
            session.open(&movie, std::fs::File::open(f).unwrap(), true);
            let drawn = Arc::new(Drawn {
                stop: AtomicBool::new(false),
                staged: std::sync::Mutex::new(Vec::new()),
            });
            let thread = {
                let (movie, drawn) = (Arc::clone(&movie), Arc::clone(&drawn));
                std::thread::spawn(move || draw(&movie, &drawn))
            };
            (movie, drawn, thread)
        })
        .collect();
    let mut firsts = vec![None; movies.len()];
    let deadline = started + Duration::from_secs(10);
    while Instant::now() < deadline && movies.iter().any(|(m, ..)| !m.settled()) {
        std::thread::sleep(Duration::from_millis(1));
    }
    for (first, (m, d, _)) in firsts.iter_mut().zip(&movies) {
        if m.status() == media_host::session::Status::Live
            && let Some(&(_, at)) = d.staged.lock().unwrap().first()
        {
            *first = Some(ms(at - started));
        }
    }
    (firsts, movies)
}

fn stop_all(movies: Vec<Drawing>) {
    for (_, d, t) in movies {
        d.stop.store(true, Ordering::Release);
        t.join().unwrap();
    }
}

/// MS3: a screen's preparation through the game's session on the real
/// skin movies (the select screen's 1080p60 and the play screen's
/// 720p60): from the open to each first frame, cold (a new profile and
/// data folder each time) and warm (the session's worker kept); and the
/// looping plays' pass ends: the longest wait between new frames around
/// each, for a 15 s and a 96 s movie.
#[test]
#[ignore = "a measurement, run by hand"]
fn media_session_entry_and_loops() {
    let dir = PathBuf::from(env_or("MEDIA_MEASURE_DIR", "target/ms3"));
    let mut out =
        Out(std::fs::File::create(env_or("MEDIA_MEASURE_OUT", "media-session.jsonl")).unwrap());
    let n = count("MEDIA_MEASURE_ENTRIES", 10);
    let screen = [dir.join("default.mp4"), dir.join("generic.mp4")];
    for i in 0..n {
        let stage = Stage::new();
        let session = session_on(&stage);
        let (firsts, movies) = enter_with(&session, 1, &screen);
        out.line(&serde_json::json!({"kind": "entry", "cold": true, "i": i, "first_ms": firsts}));
        stop_all(movies);
        drop(session);
    }
    let stage = Stage::new();
    let session = session_on(&stage);
    let (_, movies) = enter_with(&session, 1, &screen);
    stop_all(movies);
    for i in 0..n {
        let generation = u64::try_from(i).unwrap() + 2;
        let (firsts, movies) = enter_with(&session, generation, &screen);
        out.line(&serde_json::json!({"kind": "entry", "cold": false, "i": i, "first_ms": firsts}));
        stop_all(movies);
    }
    // The loops.
    let seconds: u64 = env_or("MEDIA_MEASURE_LOOP_SECONDS", "200").parse().unwrap();
    let files = [dir.join("TYPE-M.mp4"), dir.join("default.mp4")];
    let (_, movies) = enter_with(&session, 1_000, &files);
    std::thread::sleep(Duration::from_secs(seconds));
    for ((m, d, _), f) in movies.iter().zip(&files) {
        let duration = m.live().map_or(0, |l| l.duration);
        let staged = d.staged.lock().unwrap().clone();
        let mut gaps: Vec<f64> = staged.windows(2).map(|w| ms(w[1].1 - w[0].1)).collect();
        // Around each pass end: the wait between the last frame of a pass
        // and the first of the next.
        let ends: Vec<f64> = staged
            .windows(2)
            .filter(|w| duration > 0 && w[1].0 / duration != w[0].0 / duration)
            .map(|w| ms(w[1].1 - w[0].1))
            .collect();
        gaps.sort_by(f64::total_cmp);
        out.line(&serde_json::json!({
            "kind": "loop", "file": f.file_name().unwrap().to_string_lossy(),
            "duration_us": duration, "status": format!("{:?}", m.status()), "loss": format!("{:?}", m.loss()),
            "frames": staged.len(), "last_stamp": staged.last().map(|s| s.0),
            "pass_end_gaps_ms": ends, "gap_ms": stats(&gaps),
        }));
    }
    stop_all(movies);
}
