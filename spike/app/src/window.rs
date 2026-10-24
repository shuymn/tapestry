//! Windowed run (design D2/D3): borderless fullscreen on the primary
//! monitor (or a sized window), DX12 Fifo with maximum frame latency 1, the
//! swap chain's waitable object waited on by the render thread itself
//! (`DontWait`), an optional frame cap on a high-resolution waitable timer
//! aligned to the display's vblank grid, and the DXGI frame-statistics
//! diagnostic paired with frames by present count.
//!
//! winit only hands out the window handle on the main thread, so the wgpu
//! instance and surface are created there and moved to the render thread.

use crate::alloc::{self, Scope, Tag, scope};
use crate::display::{DisplayPath, active_paths, find_display, pick_refresh};
use crate::pacing::{
    DISPLAY_VBLANKS, DisplayPredictor, GridEvent, GridTracker, Wake, cap_anchor, cap_needed,
    cap_step, early_wake_tolerance,
};
use crate::render::Renderer;
use crate::run::{
    BgaState, COUNTDOWN_US, DxgiSummary, POST_PLAY_US, Recorder, RunArgs, adapter, bga_due, device,
    event, heavy_values, instance, print_summary, summary_common, write_outputs,
};
use crate::scene::{Frame, Scene};
use crate::timebase::{Clock, qpc, qpc_freq};
use spike_core::chart::{CHART_LEN_US, Chart, DEFAULT_SEED};
use spike_core::heavy::Timeline;
use spike_core::run::{EventKind, RunEvent, SpikeFrame};
use spike_core::stats::percentile_i64;
use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Dxgi::{DXGI_FRAME_STATISTICS, IDXGISwapChain3};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Fullscreen, Window, WindowId};

struct Shared {
    stop: AtomicBool,
    events: Mutex<Vec<RunEvent>>,
    clock: Clock,
}

struct App {
    args: RunArgs,
    shared: Arc<Shared>,
    proxy: EventLoopProxy<Result<(), String>>,
    window: Option<Arc<Window>>,
    result: Option<Result<(), String>>,
}

/// What the main thread hands to the render thread.
struct Gpu {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    window: Arc<Window>,
    /// Exact refresh rate `(numerator, denominator)` Hz of the window's
    /// display.
    refresh: (u32, u32),
    /// GDI name of the window's monitor.
    gdi_name: Option<String>,
}

impl App {
    fn start(&mut self, el: &ActiveEventLoop) -> Result<(), String> {
        // The requested monitor, matched against the active displays and
        // then against winit's monitors by GDI device name.
        let target = match &self.args.monitor {
            Some(part) => {
                let paths = active_paths();
                let gdi = find_display(part, &paths)?.gdi_name.clone();
                let m = el
                    .available_monitors()
                    .find(|m| m.name().is_some_and(|n| n.eq_ignore_ascii_case(&gdi)))
                    .ok_or_else(|| format!("no window-system monitor named {gdi}"))?;
                Some(m)
            }
            None => None,
        };
        let mut attrs = Window::default_attributes().with_title("tapestry-spike");
        attrs = match self.args.window {
            Some((w, h)) => {
                let a = attrs.with_inner_size(winit::dpi::PhysicalSize::new(w, h));
                match &target {
                    Some(m) => {
                        let p = m.position();
                        a.with_position(winit::dpi::PhysicalPosition::new(p.x + 40, p.y + 40))
                    }
                    None => a,
                }
            }
            None => attrs.with_fullscreen(Some(Fullscreen::Borderless(target.clone()))),
        };
        let window = Arc::new(
            el.create_window(attrs)
                .map_err(|e| format!("create window: {e}"))?,
        );
        self.window = Some(window.clone());
        let instance = instance();
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("create surface: {e}"))?;
        let monitor = window.current_monitor();
        let winit_mhz = monitor
            .as_ref()
            .and_then(|m| m.refresh_rate_millihertz())
            .unwrap_or(60_000);
        let name = monitor.as_ref().and_then(|m| m.name());
        let refresh = pick_refresh(name.as_deref(), winit_mhz, &active_paths());
        let gpu = Gpu {
            instance,
            surface,
            window,
            refresh,
            gdi_name: name,
        };
        let (args, shared, proxy) = (self.args.clone(), self.shared.clone(), self.proxy.clone());
        std::thread::Builder::new()
            .name("render".into())
            .spawn(move || {
                let r = render_thread(&args, &shared, gpu);
                let _ = proxy.send_event(r);
            })
            .map_err(|e| format!("spawn render thread: {e}"))?;
        Ok(())
    }

    fn push(&self, kind: EventKind) {
        if let Ok(mut e) = self.shared.events.lock() {
            e.push(event(&self.shared.clock, kind));
        }
    }
}

impl ApplicationHandler<Result<(), String>> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() || self.result.is_some() {
            return;
        }
        if let Err(e) = self.start(el) {
            self.result = Some(Err(e));
            el.exit();
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, result: Result<(), String>) {
        self.result = Some(result);
        el.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => self.shared.stop.store(true, Ordering::Relaxed),
            WindowEvent::Focused(false) => self.push(EventKind::FocusLoss),
            // The swap chain is not reconfigured; a size change during play
            // invalidates the run (V5 rejects swap-chain recreation).
            WindowEvent::Resized(_) => self.push(EventKind::SwapChainRecreate),
            _ => {}
        }
    }
}

pub fn windowed(args: &RunArgs) -> Result<ExitCode, String> {
    let el = EventLoop::<Result<(), String>>::with_user_event()
        .build()
        .map_err(|e| e.to_string())?;
    let mut app = App {
        args: args.clone(),
        shared: Arc::new(Shared {
            stop: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
            clock: Clock::start(),
        }),
        proxy: el.create_proxy(),
        window: None,
        result: None,
    };
    el.run_app(&mut app).map_err(|e| e.to_string())?;
    app.result
        .unwrap_or_else(|| Err("event loop ended without a result".into()))?;
    Ok(ExitCode::SUCCESS)
}

/// High-resolution waitable timer for the frame cap.
struct CapTimer(HANDLE);

impl CapTimer {
    fn new() -> Result<Self, String> {
        // SAFETY: plain timer creation; the handle is owned by this value.
        let h = unsafe {
            CreateWaitableTimerExW(
                None,
                windows::core::PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .map_err(|e| format!("CreateWaitableTimerExW: {e}"))?;
        Ok(Self(h))
    }

    /// Sleeps until QPC `deadline` (no spinning).
    fn wait_until(&self, deadline: i64) {
        let left = deadline - qpc();
        if left <= 0 {
            return;
        }
        // Relative due time in 100 ns units (negative).
        let due = -((i128::from(left) * 10_000_000 / i128::from(qpc_freq())) as i64);
        // SAFETY: valid timer handle; no completion routine.
        unsafe {
            if SetWaitableTimer(self.0, &due, 0, None, None, false).is_ok() {
                WaitForSingleObject(self.0, 1000);
            }
        }
    }
}

/// DXGI frame-statistics diagnostic (§3.2): display time of the present
/// numbered `PresentCount` = SyncQPCTime − (SyncRefreshCount −
/// PresentRefreshCount) × period, rejected if PresentCount did not advance,
/// the refresh gap is over 4, or the call fails. Samples are paired with
/// frames by present count.
struct Dxgi {
    chain: Option<IDXGISwapChain3>,
    last_present_count: u32,
    period_qpc: i64,
    /// (present count, estimated display QPC); preallocated.
    samples: Vec<(u32, i64)>,
    /// Raw statistics for the private trace: PresentCount,
    /// PresentRefreshCount, SyncRefreshCount, SyncQPCTime.
    raw: Vec<(u32, u32, u32, i64)>,
    summary: DxgiSummary,
    /// Trusted refresh grid; its anchor (latest trusted SyncQPCTime and
    /// SyncRefreshCount) anchors the cap and the prediction.
    grid: GridTracker,
    /// Anchor changes for the private trace: (sync QPC, old, new, reason).
    anchors: Vec<(i64, Option<i64>, i64, &'static str)>,
}

impl Dxgi {
    fn present_count(&self) -> Option<u32> {
        let chain = self.chain.as_ref()?;
        // SAFETY: valid swap chain.
        scope(Scope::OtherLibrary, || unsafe {
            chain.GetLastPresentCount()
        })
        .ok()
    }

    fn sample(&mut self) {
        self.summary.frames += 1;
        let Some(chain) = &self.chain else {
            self.summary.rejected += 1;
            return;
        };
        let mut s = DXGI_FRAME_STATISTICS::default();
        // SAFETY: valid swap chain; writes one struct.
        let ok = scope(Scope::OtherLibrary, || unsafe {
            chain.GetFrameStatistics(&mut s)
        })
        .is_ok();
        let gap = s.SyncRefreshCount.wrapping_sub(s.PresentRefreshCount);
        if !ok || s.PresentCount == self.last_present_count || gap > 4 {
            self.summary.rejected += 1;
            return;
        }
        self.last_present_count = s.PresentCount;
        // Only a sync on the trusted refresh grid anchors the cap and the
        // prediction (with VRR or composition, refresh counts follow
        // presents instead).
        let old = self.grid.anchor().map(|a| a.0);
        let ev = self
            .grid
            .observe_event((s.SyncQPCTime, s.SyncRefreshCount), self.period_qpc);
        if ev == GridEvent::OffGrid {
            self.summary.off_grid += 1;
        }
        if let Some((q, _)) = self.grid.anchor()
            && old != Some(q)
            && self.anchors.len() < self.anchors.capacity()
        {
            let reason = match ev {
                GridEvent::Acquired if old.is_some() => "reacquired",
                GridEvent::Acquired => "acquired",
                _ => "on-grid",
            };
            self.anchors.push((s.SyncQPCTime, old, q, reason));
        }
        self.summary.samples += 1;
        if self.raw.len() < self.raw.capacity() {
            self.raw.push((
                s.PresentCount,
                s.PresentRefreshCount,
                s.SyncRefreshCount,
                s.SyncQPCTime,
            ));
        }
        if self.samples.len() < self.samples.capacity() {
            let display = s.SyncQPCTime - i64::from(gap) * self.period_qpc;
            self.samples.push((s.PresentCount, display));
        }
    }
}

fn render_thread(args: &RunArgs, shared: &Shared, gpu: Gpu) -> Result<(), String> {
    alloc::tag_thread(Tag::Render);
    let clock = shared.clock;
    let Gpu {
        instance,
        surface,
        window,
        refresh,
        gdi_name,
    } = gpu;
    let refresh_mhz = (u64::from(refresh.0) * 1000 / u64::from(refresh.1.max(1))) as u32;
    let adapter = adapter(&instance, &args.adapter, Some(&surface))?;
    let info = adapter.get_info();
    let (dev, queue) = device(&adapter)?;
    let size = window.inner_size();
    let size = (size.width.max(1), size.height.max(1));
    let mut config = surface
        .get_default_config(&adapter, size.0, size.1)
        .ok_or("surface unsupported by adapter")?;
    let caps = surface.get_capabilities(&adapter);
    config.format = caps
        .formats
        .iter()
        .copied()
        .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
        .unwrap_or(config.format);
    config.present_mode = wgpu::PresentMode::Fifo;
    config.desired_maximum_frame_latency = 1;
    surface.configure(&dev, &config);
    // SAFETY: the hal surface is only read while `surface` is alive and not
    // reconfigured; the waitable handle and swap chain belong to it.
    let (waitable, chain) = unsafe {
        match surface.as_hal::<wgpu_hal::api::Dx12>() {
            Some(s) => (s.waitable_handle(), s.swap_chain()),
            None => (None, None),
        }
    };
    let period_qpc = qpc_freq() * i64::from(refresh.1) / i64::from(refresh.0.max(1));
    let mut bga = BgaState::open(&args.bga)?;
    let dims = bga.bga.as_ref().map(|b| (b.width, b.height));
    let mut renderer = Renderer::new(dev, queue, config.format, dims);
    let mut scene = Scene::new(Chart::generate(DEFAULT_SEED));
    let mut frame = Frame::new();
    let play_len = args.play_us();
    // Frames: countdown + play + post-play at up to 500 Hz, with margin.
    let max_frames = ((COUNTDOWN_US + play_len + POST_PLAY_US) / 2_000 + 1_000) as usize;
    let mut rec = Recorder::new(max_frames);
    // Private trace per frame: start QPC, wake kind, predicted display QPC,
    // present count.
    let mut trace: Vec<(i64, &'static str, i64, Option<u32>)> = Vec::with_capacity(max_frames);
    let mut dxgi = Dxgi {
        chain,
        last_present_count: 0,
        period_qpc,
        samples: Vec::with_capacity(max_frames),
        raw: Vec::with_capacity(max_frames),
        summary: DxgiSummary::default(),
        grid: GridTracker::default(),
        anchors: Vec::with_capacity(max_frames),
    };
    // The cap period in whole refreshes (V3: caps divide the refresh rate).
    let cap = match args.fps_cap {
        Some(hz) if cap_needed(hz, refresh_mhz) => {
            let hz = u64::from(hz);
            let k = ((u64::from(refresh_mhz) + hz * 500) / (hz * 1000)).max(1);
            Some((CapTimer::new()?, k as u32))
        }
        _ => None,
    };
    let mut cap_anchor_fallback: Option<i64> = None;
    let mut cap_phase: Option<u32> = None;
    let mut last_slot: Option<i64> = None;
    let mut predictor = DisplayPredictor::new(early_wake_tolerance(period_qpc, qpc_freq()));
    // Cap decisions for the private trace: (now, anchor, slot, wait until,
    // wake).
    let mut cap_trace: Vec<(i64, i64, i64, Option<i64>, i64)> = Vec::with_capacity(max_frames);
    let interval = bga.interval_us();
    // First BGA frame uploaded before the countdown (design D2).
    bga.step(&mut renderer, 0, true)?;
    let play_start = clock.now_us() + COUNTDOWN_US;
    let mut armed = false;
    while !shared.stop.load(Ordering::Relaxed) {
        let mut wake = Wake::Late;
        if let Some(h) = waitable {
            // Already signaled (the previous frame overran its vblank): the
            // frame is released late, not by a vblank.
            // SAFETY: the waitable object belongs to the live swap chain.
            if unsafe { WaitForSingleObject(h, 0) } != WAIT_OBJECT_0 {
                // SAFETY: as above.
                if unsafe { WaitForSingleObject(h, 1000) } != WAIT_OBJECT_0 {
                    continue;
                }
                wake = Wake::Vblank;
            }
        }
        if let Some((timer, k)) = &cap {
            let now = qpc();
            let anchor = match dxgi.grid.anchor() {
                // Keep the cap's refresh-count phase across re-anchoring.
                Some((q, c)) => {
                    let phase = *cap_phase.get_or_insert(c % k);
                    cap_anchor(q, c, phase, *k, period_qpc)
                }
                _ => *cap_anchor_fallback.get_or_insert(now),
            };
            let (wait, slot) = cap_step(now, anchor, i64::from(*k) * period_qpc, last_slot);
            if let Some(w) = wait {
                timer.wait_until(w);
                wake = Wake::CapSlot(slot);
            }
            if cap_trace.len() < cap_trace.capacity() {
                cap_trace.push((now, anchor, slot, wait, qpc()));
            }
            last_slot = Some(slot);
        }
        let t0 = qpc();
        let now_us = clock.to_us(t0);
        let play_us = now_us - play_start;
        if play_us >= play_len + POST_PLAY_US {
            break;
        }
        let in_play = (0..play_len).contains(&play_us);
        if in_play != armed {
            if in_play {
                alloc::arm();
            } else {
                alloc::disarm();
            }
            armed = in_play;
        }
        let load = scene.build(play_us.min(play_len), &mut frame);
        let predicted_qpc =
            predictor.predict(t0, wake, dxgi.grid.anchor().map(|a| a.0), period_qpc);
        let predicted_play = clock.to_us(predicted_qpc) - play_start;
        let shown = if play_us >= 0 {
            bga.step(
                &mut renderer,
                (predicted_play.max(0) / interval) as u64,
                false,
            )?
        } else {
            bga.current
        };
        let a0 = qpc();
        let tex = match scope(Scope::GetCurrentTexture, || surface.get_current_texture()) {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            _ => {
                shared
                    .events
                    .lock()
                    .map_err(|e| e.to_string())?
                    .push(event(&clock, EventKind::SwapChainRecreate));
                continue;
            }
        };
        let a1 = qpc();
        let view = scope(Scope::OtherLibrary, || {
            tex.texture.create_view(&Default::default())
        });
        renderer.draw(&view, size, &frame);
        drop(view);
        let t1 = qpc();
        let p0 = qpc();
        scope(Scope::Present, || renderer.queue().present(tex));
        let p1 = qpc();
        let present_count = dxgi.present_count();
        dxgi.sample();
        if trace.len() < trace.capacity() {
            trace.push((t0, wake_label(wake), predicted_qpc, present_count));
        }
        let us = |q: i64| (q * 1_000_000 / qpc_freq()) as u32;
        rec.frame(
            SpikeFrame {
                render_us: now_us,
                present_us: Some(clock.to_us(p0)),
                render_cpu_us: us((t1 - t0) - (a1 - a0)),
                present_call_us: us(p1 - p0),
                present_count,
                visible_notes: load.visible_notes,
                effect_instances: load.effect_instances,
                lanes_with_effects: load.lanes_with_effects,
                bga_frame_shown: shown.is_some(),
                bga_index: shown,
            },
            &frame,
            load,
        );
    }
    alloc::disarm();
    // Pair DXGI display estimates with frames by present count.
    let by_count: HashMap<u32, i64> = trace
        .iter()
        .filter_map(|&(_, _, p, c)| Some((c?, p)))
        .collect();
    let errors: Vec<i64> = dxgi
        .samples
        .iter()
        .filter_map(|(c, display)| {
            by_count
                .get(c)
                .map(|p| (display - p) * 1_000_000 / qpc_freq())
        })
        .collect();
    dxgi.summary.paired = errors.len() as u64;
    dxgi.summary.prediction_error_us_p50 = percentile_i64(&errors, 50);
    let abs: Vec<i64> = errors.iter().map(|e| e.abs()).collect();
    dxgi.summary.prediction_error_us_p99_abs = percentile_i64(&abs, 99);
    let loops = (play_len as u64).div_ceil(CHART_LEN_US as u64).max(1) as u32;
    let timeline = Timeline {
        play_start_us: play_start,
        loops,
    };
    // G7 from frames actually presented during play.
    let in_play = |f: &SpikeFrame| {
        f.present_us
            .is_some_and(|p| (play_start..play_start + play_len).contains(&p))
    };
    let due = bga_due(&rec, &bga, play_start, play_len, in_play);
    let mode = if args.window.is_some() {
        "windowed"
    } else {
        "borderless-fullscreen"
    };
    let mut summary = summary_common(mode, &info, size, &rec);
    (
        summary.heavy_render_cpu_us_p99,
        summary.heavy_bga_displayed_pct,
    ) = heavy_values(&rec, &due, timeline);
    summary.bga = bga.summary(&due);
    summary.dxgi = Some(dxgi.summary);
    let paths = active_paths();
    let on = |p: &&DisplayPath| {
        gdi_name
            .as_deref()
            .is_some_and(|n| p.gdi_name.eq_ignore_ascii_case(n))
    };
    summary.monitor = paths.iter().find(on).cloned();
    summary.other_displays = paths.iter().filter(|p| !on(p)).cloned().collect();
    summary.refresh_mhz_used = Some(refresh_mhz);
    summary.notes = vec![
        "G7 'displayed' means shown by a presented frame; P5 reconciles it with PresentMon display rows".into(),
        format!(
            "display prediction uses DISPLAY_VBLANKS = {DISPLAY_VBLANKS}, uncalibrated; P5 calibrates it against PresentMon at 60 and 240 Hz with and without a cap using private/trace.csv"
        ),
    ];
    summary.events = shared.events.lock().map_err(|e| e.to_string())?.clone();
    print_summary(&summary);
    write_outputs(&args.out, &rec, &due, &summary)?;
    write_private_trace(&args.out, &trace, &dxgi.raw, &dxgi.anchors, &cap_trace)?;
    Ok(())
}

/// Wake kind for the private trace.
fn wake_label(wake: Wake) -> &'static str {
    match wake {
        Wake::Vblank => "vblank",
        Wake::CapSlot(_) => "cap",
        Wake::Late => "late",
    }
}

/// `private/`: raw QPC values for P5's reconciliation with PresentMon, never
/// part of the export. `trace.csv` (per frame: start, wake kind, predicted
/// display, present count), `dxgi.csv` (frame statistics), `anchors.csv` (refresh
/// grid anchor changes and why), `cap.csv` (per capped frame: decision
/// time, anchor, slot, wait target, wake time), `qpc.txt` (frequency).
fn write_private_trace(
    out: &std::path::Path,
    trace: &[(i64, &'static str, i64, Option<u32>)],
    raw: &[(u32, u32, u32, i64)],
    anchors: &[(i64, Option<i64>, i64, &'static str)],
    cap: &[(i64, i64, i64, Option<i64>, i64)],
) -> Result<(), String> {
    use std::fmt::Write as _;
    let dir = out.join("private");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut t = String::from("frame_start_qpc,wake,predicted_display_qpc,present_count\n");
    for (s, w, p, c) in trace {
        let c = c.map_or(String::new(), |c| c.to_string());
        let _ = writeln!(t, "{s},{w},{p},{c}");
    }
    std::fs::write(dir.join("trace.csv"), t).map_err(|e| e.to_string())?;
    let mut d = String::from("present_count,present_refresh_count,sync_refresh_count,sync_qpc\n");
    for (a, b, c, q) in raw {
        let _ = writeln!(d, "{a},{b},{c},{q}");
    }
    std::fs::write(dir.join("dxgi.csv"), d).map_err(|e| e.to_string())?;
    let mut a = String::from("sync_qpc,old_anchor_qpc,new_anchor_qpc,reason\n");
    for (at, old, new, reason) in anchors {
        let old = old.map_or(String::new(), |o| o.to_string());
        let _ = writeln!(a, "{at},{old},{new},{reason}");
    }
    std::fs::write(dir.join("anchors.csv"), a).map_err(|e| e.to_string())?;
    let mut c = String::from("now_qpc,anchor_qpc,slot_qpc,wait_until_qpc,wake_qpc\n");
    for (now, anchor, slot, wait, wake) in cap {
        let wait = wait.map_or(String::new(), |w| w.to_string());
        let _ = writeln!(c, "{now},{anchor},{slot},{wait},{wake}");
    }
    std::fs::write(dir.join("cap.csv"), c).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("qpc.txt"), format!("frequency {}\n", qpc_freq()))
        .map_err(|e| e.to_string())?;
    Ok(())
}
