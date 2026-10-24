//! `run`: the render loop (offscreen for the WARP smoke, or windowed on the
//! display), frame metrics into the P1 record types, and the outputs.

use crate::alloc::{self, Scope, Tag, scope};
use crate::bga::Bga;
use crate::render::Renderer;
use crate::scene::{Frame, FrameLoad, Scene};
use crate::timebase::{Clock, qpc, qpc_freq};
use serde::Serialize;
use spike_core::chart::{CHART_LEN_US, Chart, DEFAULT_SEED};
use spike_core::heavy::Timeline;
use spike_core::run::{BgaDue, EventKind, RunEvent, SpikeFrame, ThreadTag};
use spike_core::stats::percentile_i64;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub const COUNTDOWN_US: i64 = 5_000_000;
/// Offscreen virtual frame interval (60 Hz).
const VIRTUAL_FRAME_US: i64 = 16_667;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterChoice {
    /// The default high-performance hardware adapter.
    Hardware,
    /// WARP, the DX12 software rasterizer ("Microsoft Basic Render Driver").
    Warp,
}

#[derive(Clone, Debug)]
pub struct RunArgs {
    pub offscreen: Option<(u32, u32)>,
    pub window: Option<(u32, u32)>,
    pub adapter: AdapterChoice,
    pub frames: Option<u64>,
    pub duration_s: Option<u64>,
    pub loops: u32,
    pub fps_cap: Option<u32>,
    pub bga: Option<PathBuf>,
    pub out: PathBuf,
}

impl RunArgs {
    /// Play length after the countdown.
    pub fn play_us(&self) -> i64 {
        self.duration_s
            .map_or(i64::from(self.loops) * CHART_LEN_US, |s| {
                s as i64 * 1_000_000
            })
    }
}

#[derive(Serialize, Default)]
pub struct BgaSummary {
    pub width: u32,
    pub height: u32,
    pub decoded: u64,
    pub displayed: u64,
    pub skipped: u64,
    pub decoder_waits: u64,
}

#[derive(Serialize, Default)]
pub struct DxgiSummary {
    pub frames: u64,
    pub estimated: u64,
    pub rejected: u64,
}

/// `render.json`: what one render run measured.
#[derive(Serialize, Default)]
pub struct RenderSummary {
    pub mode: String,
    pub adapter: String,
    pub device_type: String,
    pub size: (u32, u32),
    pub frames: u64,
    pub render_cpu_us_p50: Option<i64>,
    pub render_cpu_us_p99: Option<i64>,
    pub present_call_us_p99: Option<i64>,
    /// Heavy-window values for the local check (G2 µs, G7 %).
    pub heavy_render_cpu_us_p99: Option<i64>,
    pub heavy_bga_displayed_pct: Option<f64>,
    pub instances_max: u32,
    pub instances_dropped: u64,
    pub bga: Option<BgaSummary>,
    pub dxgi: Option<DxgiSummary>,
    pub app_allocs: BTreeMap<ThreadTag, u64>,
    pub library_allocs: BTreeMap<String, u64>,
    pub other_thread_app_allocs: u64,
    pub readback_distinct_colors: Option<usize>,
    pub events: Vec<RunEvent>,
}

pub fn instance() -> wgpu::Instance {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::DX12;
    // The render loop waits on the waitable object itself (design D3 open
    // choice 1).
    desc.backend_options.dx12.latency_waitable_object =
        wgpu::Dx12UseFrameLatencyWaitableObject::DontWait;
    wgpu::Instance::new(desc)
}

pub fn adapter(
    instance: &wgpu::Instance,
    choice: AdapterChoice,
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<wgpu::Adapter, String> {
    match choice {
        AdapterChoice::Warp => {
            pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12))
                .into_iter()
                .find(|a| a.get_info().device_type == wgpu::DeviceType::Cpu)
                .ok_or_else(|| "no WARP adapter".into())
        }
        AdapterChoice::Hardware => {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: surface,
                apply_limit_buckets: false,
            }))
            .map_err(|e| format!("request adapter: {e}"))
        }
    }
}

pub fn device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), String> {
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .map_err(|e| format!("request device: {e}"))
}

/// Per-run recording state shared by both modes; all buffers preallocated.
pub struct Recorder {
    pub frames: Vec<SpikeFrame>,
    pub bga: Vec<BgaDue>,
    pub instances_max: u32,
    pub instances_dropped: u64,
}

impl Recorder {
    pub fn new(max_frames: usize) -> Self {
        Self {
            frames: Vec::with_capacity(max_frames),
            bga: Vec::with_capacity(max_frames * 2),
            instances_max: 0,
            instances_dropped: 0,
        }
    }

    pub fn frame(&mut self, f: SpikeFrame, frame: &Frame, load: FrameLoad) {
        self.instances_max = self
            .instances_max
            .max((frame.alpha.len() + frame.additive.len()) as u32);
        self.instances_dropped += u64::from(load.dropped);
        if self.frames.len() < self.frames.capacity() {
            self.frames.push(f);
        }
    }
}

/// Uploads the BGA frame due at `video_us`, if any.
pub fn bga_step(
    bga: &mut Option<Bga>,
    renderer: &mut Renderer,
    video_us: i64,
    wait: bool,
    base_us: i64,
    records: &mut Vec<BgaDue>,
) -> bool {
    let Some(b) = bga else {
        return false;
    };
    let due = b.take_due(video_us, wait, base_us, records);
    let Some(i) = due.slot else {
        return false;
    };
    {
        let slot = b.slot(i);
        renderer.upload_bga(&slot.y, &slot.u, &slot.v);
    }
    b.release(i);
    true
}

pub fn summary_common(
    mode: &str,
    info: &wgpu::AdapterInfo,
    size: (u32, u32),
    rec: &Recorder,
    bga: &Option<Bga>,
    timeline: Timeline,
) -> RenderSummary {
    let cpu: Vec<i64> = rec
        .frames
        .iter()
        .map(|f| i64::from(f.render_cpu_us))
        .collect();
    let heavy_cpu: Vec<i64> = rec
        .frames
        .iter()
        .filter(|f| timeline.is_heavy(f.render_us))
        .map(|f| i64::from(f.render_cpu_us))
        .collect();
    let present: Vec<i64> = rec
        .frames
        .iter()
        .filter(|f| f.present_us.is_some())
        .map(|f| i64::from(f.present_call_us))
        .collect();
    let heavy_due: Vec<&BgaDue> = rec
        .bga
        .iter()
        .filter(|b| timeline.is_heavy(b.due_us))
        .collect();
    let (app_allocs, library_allocs) = alloc::snapshot();
    RenderSummary {
        mode: mode.into(),
        adapter: info.name.clone(),
        device_type: format!("{:?}", info.device_type),
        size,
        frames: rec.frames.len() as u64,
        render_cpu_us_p50: percentile_i64(&cpu, 50),
        render_cpu_us_p99: percentile_i64(&cpu, 99),
        present_call_us_p99: percentile_i64(&present, 99),
        heavy_render_cpu_us_p99: percentile_i64(&heavy_cpu, 99),
        heavy_bga_displayed_pct: (!heavy_due.is_empty()).then(|| {
            heavy_due.iter().filter(|b| b.displayed).count() as f64 * 100.0 / heavy_due.len() as f64
        }),
        instances_max: rec.instances_max,
        instances_dropped: rec.instances_dropped,
        bga: bga.as_ref().map(|b| BgaSummary {
            width: b.width,
            height: b.height,
            decoded: b.decoded.load(std::sync::atomic::Ordering::Relaxed),
            displayed: b.displayed,
            skipped: b.skipped,
            decoder_waits: b.decoder_waits.load(std::sync::atomic::Ordering::Relaxed),
        }),
        dxgi: None,
        app_allocs,
        library_allocs,
        other_thread_app_allocs: alloc::other_app(),
        readback_distinct_colors: None,
        events: vec![],
    }
}

pub fn write_outputs(out: &Path, rec: &Recorder, summary: &RenderSummary) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let csv_out = |name: &str| {
        csv::Writer::from_path(out.join(name)).map_err(|e| format!("write {name}: {e}"))
    };
    let mut w = csv_out("frames.csv")?;
    for f in &rec.frames {
        w.serialize(f).map_err(|e| e.to_string())?;
    }
    w.flush().map_err(|e| e.to_string())?;
    let mut w = csv_out("bga.csv")?;
    for b in &rec.bga {
        w.serialize(b).map_err(|e| e.to_string())?;
    }
    w.flush().map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(summary).map_err(|e| e.to_string())?;
    std::fs::write(out.join("render.json"), json).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn print_summary(s: &RenderSummary) {
    println!("adapter: {} ({})", s.adapter, s.device_type);
    println!("mode: {} {}x{}", s.mode, s.size.0, s.size.1);
    println!(
        "frames: {} (render cpu p50 {:?} us, p99 {:?} us; instances max {}, dropped {})",
        s.frames, s.render_cpu_us_p50, s.render_cpu_us_p99, s.instances_max, s.instances_dropped
    );
    if let Some(p) = s.present_call_us_p99 {
        println!("present call p99: {p} us");
    }
    if let Some(b) = &s.bga {
        println!(
            "bga: {}x{} decoded {} displayed {} skipped {} decoder-waits {}",
            b.width, b.height, b.decoded, b.displayed, b.skipped, b.decoder_waits
        );
    } else {
        println!("bga: off");
    }
    if let Some(d) = &s.dxgi {
        println!(
            "dxgi display-time estimates: {} of {} frames ({} rejected)",
            d.estimated, d.frames, d.rejected
        );
    }
    println!(
        "heavy windows: G2 render cpu p99 {:?} us, G7 bga displayed {:?} %",
        s.heavy_render_cpu_us_p99, s.heavy_bga_displayed_pct
    );
    println!(
        "allocations after countdown: app {:?} (other threads {}), library {:?}",
        s.app_allocs, s.other_thread_app_allocs, s.library_allocs
    );
    if let Some(c) = s.readback_distinct_colors {
        println!(
            "readback: {} ({c} distinct colors)",
            if c >= 16 { "non-blank" } else { "blank" }
        );
    }
}

fn open_bga(path: &Option<PathBuf>) -> Result<Option<Bga>, String> {
    path.as_deref().map(Bga::open).transpose()
}

/// Offscreen run (the WARP smoke): `frames` frames on a virtual 60 Hz clock;
/// the BGA waits for its due frame so the result does not depend on the
/// software rasterizer's speed.
pub fn offscreen(args: &RunArgs, size: (u32, u32)) -> Result<ExitCode, String> {
    alloc::tag_thread(Tag::Render);
    let clock = Clock::start();
    let instance = instance();
    let adapter = adapter(&instance, args.adapter, None)?;
    let info = adapter.get_info();
    let (dev, queue) = device(&adapter)?;
    let mut bga = open_bga(&args.bga)?;
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let mut renderer = Renderer::new(
        dev,
        queue,
        format,
        bga.as_ref().map(|b| (b.width, b.height)),
    );
    let target = renderer.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let mut scene = Scene::new(Chart::generate(DEFAULT_SEED));
    let mut frame = Frame::new();
    let n = args.frames.unwrap_or(300) as usize;
    let mut rec = Recorder::new(n);
    // Play starts at the first frame; video time equals play time.
    let play_start = clock.now_us();
    for i in 0..n {
        if i == 1 {
            // Frame 0 creates lazily initialized state; count from frame 1.
            alloc::arm();
        }
        let virtual_us = i as i64 * VIRTUAL_FRAME_US;
        let t0 = qpc();
        let load = scene.build(virtual_us, &mut frame);
        let shown = bga_step(
            &mut bga,
            &mut renderer,
            virtual_us,
            true,
            play_start,
            &mut rec.bga,
        );
        renderer.draw(&view, size, &frame);
        let t1 = qpc();
        scope(Scope::OtherLibrary, || {
            let _ = renderer.device().poll(wgpu::PollType::wait_indefinitely());
        });
        let render_us = clock.to_us(t0);
        rec.frame(
            SpikeFrame {
                render_us,
                present_us: None,
                render_cpu_us: ((t1 - t0) * 1_000_000 / qpc_freq()) as u32,
                present_call_us: 0,
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
    let pixels = renderer.read_back(&target, size);
    let mut colors: Vec<u32> = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .step_by(97)
        .map(|p| u32::from_le_bytes(*p))
        .collect();
    colors.sort_unstable();
    colors.dedup();
    let timeline = Timeline {
        play_start_us: play_start,
        loops: 1,
    };
    let mut summary = summary_common("offscreen", &info, size, &rec, &bga, timeline);
    summary.readback_distinct_colors = Some(colors.len());
    print_summary(&summary);
    write_outputs(&args.out, &rec, &summary)?;
    let bga_ok = bga.as_ref().is_none_or(|b| b.displayed > 0);
    Ok(if colors.len() >= 16 && bga_ok {
        ExitCode::SUCCESS
    } else {
        eprintln!("run: smoke failed (blank readback or no BGA frame displayed)");
        ExitCode::from(1)
    })
}

/// Records a window event that V5 cares about.
pub fn event(clock: &Clock, kind: EventKind) -> RunEvent {
    RunEvent {
        t_us: clock.now_us(),
        kind,
    }
}
