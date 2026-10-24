//! Windowed run (design D2/D3): borderless fullscreen on the primary
//! monitor (or a sized window), DX12 Fifo with maximum frame latency 1, the
//! swap chain's waitable object waited on by the render thread itself
//! (`DontWait`), an optional frame cap on a high-resolution waitable timer
//! aligned to predicted vblank, and the DXGI frame-statistics diagnostic.

use crate::alloc::{self, Scope, Tag, scope};
use crate::render::Renderer;
use crate::run::{
    COUNTDOWN_US, DxgiSummary, Recorder, RunArgs, adapter, bga_step, device, event, instance,
    print_summary, summary_common, write_outputs,
};
use crate::scene::{Frame, Scene};
use crate::timebase::{Clock, qpc, qpc_freq};
use spike_core::chart::{CHART_LEN_US, Chart, DEFAULT_SEED};
use spike_core::heavy::Timeline;
use spike_core::run::{EventKind, RunEvent, SpikeFrame};
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

impl ApplicationHandler<Result<(), String>> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes().with_title("tapestry-spike");
        attrs = match self.args.window {
            Some((w, h)) => attrs.with_inner_size(winit::dpi::PhysicalSize::new(w, h)),
            None => attrs.with_fullscreen(Some(Fullscreen::Borderless(None))),
        };
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.result = Some(Err(format!("create window: {e}")));
                el.exit();
                return;
            }
        };
        self.window = Some(window.clone());
        let (args, shared, proxy) = (self.args.clone(), self.shared.clone(), self.proxy.clone());
        std::thread::Builder::new()
            .name("render".into())
            .spawn(move || {
                let r = render_thread(&args, &shared, window);
                let _ = proxy.send_event(r);
            })
            .expect("spawn render thread");
    }

    fn user_event(&mut self, el: &ActiveEventLoop, result: Result<(), String>) {
        self.result = Some(result);
        el.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => self.shared.stop.store(true, Ordering::Relaxed),
            WindowEvent::Focused(false) => self.push(EventKind::FocusLoss),
            WindowEvent::Resized(_) => self.push(EventKind::SwapChainRecreate),
            _ => {}
        }
    }
}

impl App {
    fn push(&self, kind: EventKind) {
        if let Ok(mut e) = self.shared.events.lock() {
            e.push(event(&self.shared.clock, kind));
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

/// DXGI frame-statistics diagnostic (§3.2): display time of the last
/// present = SyncQPCTime − (SyncRefreshCount − PresentRefreshCount) ×
/// period, rejected if PresentCount did not advance, the refresh gap is
/// over 4, or the call fails.
struct Dxgi {
    chain: Option<IDXGISwapChain3>,
    last_present_count: u32,
    period_qpc: i64,
    summary: DxgiSummary,
    /// Latest SyncQPCTime, the anchor for the cap timer.
    last_sync_qpc: Option<i64>,
}

impl Dxgi {
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
        self.last_sync_qpc = Some(s.SyncQPCTime);
        let _display_qpc = s.SyncQPCTime - i64::from(gap) * self.period_qpc;
        self.summary.estimated += 1;
    }
}

fn render_thread(args: &RunArgs, shared: &Shared, window: Arc<Window>) -> Result<(), String> {
    alloc::tag_thread(Tag::Render);
    let clock = shared.clock;
    let instance = instance();
    let surface = instance
        .create_surface(window.clone())
        .map_err(|e| format!("create surface: {e}"))?;
    let adapter = adapter(&instance, args.adapter, Some(&surface))?;
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
    let refresh_mhz = window
        .current_monitor()
        .and_then(|m| m.refresh_rate_millihertz())
        .unwrap_or(60_000);
    let period_qpc = qpc_freq() * 1000 / i64::from(refresh_mhz);
    let mut bga = args.bga.as_deref().map(crate::bga::Bga::open).transpose()?;
    let mut renderer = Renderer::new(
        dev,
        queue,
        config.format,
        bga.as_ref().map(|b| (b.width, b.height)),
    );
    let mut scene = Scene::new(Chart::generate(DEFAULT_SEED));
    let mut frame = Frame::new();
    let play_len = args.play_us();
    // Frames: countdown + play at up to 500 Hz, with margin.
    let max_frames = ((COUNTDOWN_US + play_len) / 2_000 + 1_000) as usize;
    let mut rec = Recorder::new(max_frames);
    let mut dxgi = Dxgi {
        chain,
        last_present_count: 0,
        period_qpc,
        summary: DxgiSummary::default(),
        last_sync_qpc: None,
    };
    let cap = match args.fps_cap {
        Some(hz) => Some((CapTimer::new()?, qpc_freq() / i64::from(hz))),
        None => None,
    };
    let mut next_deadline: Option<i64> = None;
    // First BGA frame uploaded before the countdown (design D2).
    let mut first_bga = Vec::new();
    bga_step(&mut bga, &mut renderer, 0, true, 0, &mut first_bga);
    let play_start = clock.now_us() + COUNTDOWN_US;
    let mut armed = false;
    while !shared.stop.load(Ordering::Relaxed) {
        if let Some(h) = waitable {
            // SAFETY: the waitable object belongs to the live swap chain.
            let r = unsafe { WaitForSingleObject(h, 1000) };
            if r != WAIT_OBJECT_0 {
                continue;
            }
        }
        if let Some((timer, period)) = &cap {
            let anchor = dxgi.last_sync_qpc.unwrap_or_else(qpc);
            let now = qpc();
            let deadline = match next_deadline {
                Some(d) if d > now => d,
                // Next vblank-aligned deadline after now.
                _ => anchor + ((now - anchor) / period + 1) * period,
            };
            timer.wait_until(deadline);
            next_deadline = Some(deadline + period);
        }
        let t0 = qpc();
        let now_us = clock.to_us(t0);
        let play_us = now_us - play_start;
        if play_us >= play_len {
            break;
        }
        if play_us >= 0 && !armed {
            alloc::arm();
            armed = true;
        }
        let load = scene.build(play_us, &mut frame);
        // The frame is displayed about one refresh after it starts (max
        // latency 1): take the BGA frame due then.
        let predicted = play_us + period_qpc * 1_000_000 / qpc_freq();
        let shown = play_us >= 0
            && bga_step(
                &mut bga,
                &mut renderer,
                predicted,
                false,
                play_start,
                &mut rec.bga,
            );
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
        dxgi.sample();
        let us = |q: i64| (q * 1_000_000 / qpc_freq()) as u32;
        rec.frame(
            SpikeFrame {
                render_us: now_us,
                present_us: Some(clock.to_us(p0)),
                render_cpu_us: us((t1 - t0) - (a1 - a0)),
                present_call_us: us(p1 - p0),
                visible_notes: load.visible_notes,
                effect_instances: load.effect_instances,
                lanes_with_effects: load.lanes_with_effects,
                bga_frame_shown: shown,
            },
            &frame,
            load,
        );
    }
    alloc::disarm();
    let loops = (play_len as u64).div_ceil(CHART_LEN_US as u64).max(1) as u32;
    let timeline = Timeline {
        play_start_us: play_start,
        loops,
    };
    let mut summary = summary_common("windowed", &info, size, &rec, &bga, timeline);
    summary.dxgi = Some(dxgi.summary);
    summary.events = shared.events.lock().map_err(|e| e.to_string())?.clone();
    print_summary(&summary);
    write_outputs(&args.out, &rec, &summary)?;
    Ok(())
}
