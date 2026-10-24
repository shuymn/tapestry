//! `run`: shared render-run pieces (GPU setup, recording, outputs) and the
//! offscreen run used by the WARP smoke. The windowed run is in `window`.

use crate::alloc::{self, Scope, Tag, scope};
use crate::bga::Bga;
use crate::pacing::bga_due_records;
use crate::render::Renderer;
use crate::scene::{Frame, FrameLoad, Scene};
use crate::timebase::{Clock, qpc, qpc_freq};
use serde::Serialize;
use spike_core::chart::{CHART_LEN_US, Chart, DEFAULT_SEED};
use spike_core::heavy::Timeline;
use spike_core::run::{BgaDue, EventKind, RunEvent, SpikeFrame, StreamCount, ThreadTag};
use spike_core::stats::percentile_i64;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub const COUNTDOWN_US: i64 = 5_000_000;
/// Presenting continues this long after play so PresentMon's capture
/// brackets the play window (V4); outside the gates and the allocation
/// interval.
pub const POST_PLAY_US: i64 = 2_000_000;
/// Offscreen virtual frame interval (60 Hz).
const VIRTUAL_FRAME_US: i64 = 16_667;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterChoice {
    /// The default high-performance hardware adapter.
    Hardware,
    /// The low-power adapter (an integrated GPU when there is one).
    LowPower,
    /// WARP, the DX12 software rasterizer ("Microsoft Basic Render Driver").
    Warp,
    /// The first DX12 adapter whose name contains this (case-insensitive).
    Name(String),
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
    /// Part of the monitor's name or GDI device name to run on.
    pub monitor: Option<String>,
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
    /// Due in play / displayed by a presented frame (G7 inputs).
    pub due: u64,
    pub displayed: u64,
    /// Decoded frames released without upload.
    pub superseded: u64,
    pub decoder_waits: u64,
    pub decode_us_p50: Option<i64>,
    pub decode_us_p99: Option<i64>,
    /// Offscreen: every frame showed exactly the due BGA frame, across the
    /// clip's loop boundary.
    pub advanced_every_frame: Option<bool>,
}

#[derive(Serialize, Default)]
pub struct DxgiSummary {
    pub frames: u64,
    pub samples: u64,
    pub rejected: u64,
    /// Samples off a fixed refresh grid (VRR or composition), not used as
    /// anchors.
    pub off_grid: u64,
    /// Samples paired with a frame by present count.
    pub paired: u64,
    pub prediction_error_us_p50: Option<i64>,
    pub prediction_error_us_p99_abs: Option<i64>,
}

/// `render.json`: what one render run measured.
#[derive(Serialize, Default)]
pub struct RenderSummary {
    pub mode: String,
    pub adapter: String,
    pub device_type: String,
    pub size: (u32, u32),
    pub frames: u64,
    /// Producer counts of the frame records (P1 V8).
    pub frame_records: StreamCount,
    pub render_cpu_us_p50: Option<i64>,
    pub render_cpu_us_p99: Option<i64>,
    pub present_call_us_p99: Option<i64>,
    /// Heavy-window values for the local check (G2 µs, G7 %); windowed only.
    pub heavy_render_cpu_us_p99: Option<i64>,
    pub heavy_bga_displayed_pct: Option<f64>,
    pub instances_max: u32,
    pub instances_dropped: u64,
    pub bga: Option<BgaSummary>,
    pub dxgi: Option<DxgiSummary>,
    pub app_allocs: BTreeMap<ThreadTag, u64>,
    pub library_allocs: BTreeMap<String, u64>,
    pub other_thread_app_allocs: u64,
    /// G6/G6r count allocations through Rust's global allocator only.
    pub allocs_scope: String,
    /// Caveats on the values above.
    pub notes: Vec<String>,
    /// Windowed: the monitor the run is on (exact mode), the refresh rate
    /// (mHz) its grid used, and the other active monitors' modes (mixed
    /// refresh rates can affect composition).
    pub monitor: Option<crate::display::DisplayPath>,
    pub refresh_mhz_used: Option<u32>,
    pub other_displays: Vec<crate::display::DisplayPath>,
    pub readback_distinct_colors: Option<usize>,
    pub bga_only_readback_distinct_colors: Option<usize>,
    pub events: Vec<RunEvent>,
}

pub fn adapter(
    instance: &wgpu::Instance,
    choice: &AdapterChoice,
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<wgpu::Adapter, String> {
    let all = || pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12));
    let request = |power_preference| {
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference,
            force_fallback_adapter: false,
            compatible_surface: surface,
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("request adapter: {e}"))
    };
    match choice {
        AdapterChoice::Warp => all()
            .into_iter()
            .find(|a| a.get_info().device_type == wgpu::DeviceType::Cpu)
            .ok_or_else(|| "no WARP adapter".into()),
        AdapterChoice::Hardware => request(wgpu::PowerPreference::HighPerformance),
        AdapterChoice::LowPower => request(wgpu::PowerPreference::LowPower),
        AdapterChoice::Name(part) => {
            let adapters = all();
            let names: Vec<String> = adapters.iter().map(|a| a.get_info().name).collect();
            adapters
                .into_iter()
                .find(|a| {
                    a.get_info()
                        .name
                        .to_lowercase()
                        .contains(&part.to_lowercase())
                })
                .ok_or_else(|| format!("no DX12 adapter named like {part:?}; adapters: {names:?}"))
        }
    }
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

pub fn device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), String> {
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .map_err(|e| format!("request device: {e}"))
}

/// Per-run recording; buffers are preallocated and every production attempt
/// is counted, so a full buffer shows up as lost records (V8), never as a
/// silent gap.
pub struct Recorder {
    pub frames: Vec<SpikeFrame>,
    /// BGA frame index each recorded frame showed.
    pub shown: Vec<Option<u64>>,
    pub frame_records: StreamCount,
    pub instances_max: u32,
    pub instances_dropped: u64,
}

impl Recorder {
    pub fn new(max_frames: usize) -> Self {
        Self {
            frames: Vec::with_capacity(max_frames),
            shown: Vec::with_capacity(max_frames),
            frame_records: StreamCount::default(),
            instances_max: 0,
            instances_dropped: 0,
        }
    }

    pub fn frame(&mut self, f: SpikeFrame, shown: Option<u64>, frame: &Frame, load: FrameLoad) {
        self.instances_max = self
            .instances_max
            .max((frame.alpha.len() + frame.additive.len()) as u32);
        self.instances_dropped += u64::from(load.dropped);
        self.frame_records.produced += 1;
        if self.frames.len() < self.frames.capacity() {
            self.frames.push(f);
            self.shown.push(shown);
        } else {
            self.frame_records.lost += 1;
        }
    }
}

/// The BGA frame on screen: uploads the frame due at `due` if a newer one
/// is available, and returns the index now in the textures.
pub struct BgaState {
    pub bga: Option<Bga>,
    pub current: Option<u64>,
}

impl BgaState {
    pub fn open(path: &Option<PathBuf>) -> Result<Self, String> {
        Ok(Self {
            bga: path.as_deref().map(Bga::open).transpose()?,
            current: None,
        })
    }

    pub fn step(
        &mut self,
        renderer: &mut Renderer,
        due: u64,
        wait: bool,
    ) -> Result<Option<u64>, String> {
        let Some(b) = &mut self.bga else {
            return Ok(None);
        };
        if let Some(i) = b.take(due, wait)? {
            let index = {
                let slot = b.slot(i);
                renderer.upload_bga(&slot.y, &slot.u, &slot.v);
                slot.index
            };
            b.release(i);
            self.current = Some(index);
        }
        Ok(self.current)
    }

    pub fn interval_us(&self) -> i64 {
        self.bga
            .as_ref()
            .map_or(crate::bga::DEFAULT_INTERVAL_US, |b| b.interval_us)
    }

    pub fn summary(&self, due: &[BgaDue]) -> Option<BgaSummary> {
        let b = self.bga.as_ref()?;
        let decode: Vec<i64> = b
            .decode_us
            .lock()
            .map(|d| d.iter().map(|&x| i64::from(x)).collect())
            .unwrap_or_default();
        Some(BgaSummary {
            width: b.width,
            height: b.height,
            decoded: b.decoded.load(std::sync::atomic::Ordering::Relaxed),
            due: due.len() as u64,
            displayed: due.iter().filter(|d| d.displayed).count() as u64,
            superseded: b.superseded,
            decoder_waits: b.decoder_waits.load(std::sync::atomic::Ordering::Relaxed),
            decode_us_p50: percentile_i64(&decode, 50),
            decode_us_p99: percentile_i64(&decode, 99),
            advanced_every_frame: None,
        })
    }
}

/// G7 due records for a play of `play_us` from what presented frames
/// showed (`rec.shown` for frames with `presented`).
pub fn bga_due(
    rec: &Recorder,
    bga: &BgaState,
    play_start: i64,
    play_us: i64,
    presented: impl Fn(&SpikeFrame) -> bool,
) -> Vec<BgaDue> {
    if bga.bga.is_none() {
        return vec![];
    }
    let interval = bga.interval_us();
    let due_count = (play_us / interval) as u64;
    let shown = rec
        .frames
        .iter()
        .zip(&rec.shown)
        .filter(|(f, _)| presented(f))
        .filter_map(|(_, s)| *s);
    bga_due_records(play_start, interval, due_count, shown)
}

pub fn summary_common(
    mode: &str,
    info: &wgpu::AdapterInfo,
    size: (u32, u32),
    rec: &Recorder,
) -> RenderSummary {
    let cpu: Vec<i64> = rec
        .frames
        .iter()
        .map(|f| i64::from(f.render_cpu_us))
        .collect();
    let present: Vec<i64> = rec
        .frames
        .iter()
        .filter(|f| f.present_us.is_some())
        .map(|f| i64::from(f.present_call_us))
        .collect();
    let (app_allocs, library_allocs) = alloc::snapshot();
    RenderSummary {
        mode: mode.into(),
        adapter: info.name.clone(),
        device_type: format!("{:?}", info.device_type),
        size,
        frames: rec.frames.len() as u64,
        frame_records: rec.frame_records,
        render_cpu_us_p50: percentile_i64(&cpu, 50),
        render_cpu_us_p99: percentile_i64(&cpu, 99),
        present_call_us_p99: percentile_i64(&present, 99),
        instances_max: rec.instances_max,
        instances_dropped: rec.instances_dropped,
        app_allocs,
        library_allocs,
        other_thread_app_allocs: alloc::other_app(),
        allocs_scope: "Rust global allocator only (FFmpeg and D3D12 native heaps not counted)"
            .into(),
        ..RenderSummary::default()
    }
}

/// Heavy-window G2 (render CPU p99) and G7 (% displayed) for windowed runs.
pub fn heavy_values(
    rec: &Recorder,
    due: &[BgaDue],
    timeline: Timeline,
) -> (Option<i64>, Option<f64>) {
    let cpu: Vec<i64> = rec
        .frames
        .iter()
        .filter(|f| timeline.is_heavy(f.render_us))
        .map(|f| i64::from(f.render_cpu_us))
        .collect();
    let heavy: Vec<&BgaDue> = due.iter().filter(|b| timeline.is_heavy(b.due_us)).collect();
    let pct = (!heavy.is_empty())
        .then(|| heavy.iter().filter(|b| b.displayed).count() as f64 * 100.0 / heavy.len() as f64);
    (percentile_i64(&cpu, 99), pct)
}

pub fn write_outputs(
    out: &Path,
    rec: &Recorder,
    due: &[BgaDue],
    summary: &RenderSummary,
) -> Result<(), String> {
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
    for b in due {
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
        "frames: {} (records produced {}, lost {}; render cpu p50 {:?} us, p99 {:?} us; instances max {}, dropped {})",
        s.frames,
        s.frame_records.produced,
        s.frame_records.lost,
        s.render_cpu_us_p50,
        s.render_cpu_us_p99,
        s.instances_max,
        s.instances_dropped
    );
    if let Some(p) = s.present_call_us_p99 {
        println!("present call p99: {p} us");
    }
    match &s.bga {
        Some(b) => println!(
            "bga: {}x{} decoded {} due {} displayed {} superseded {} decoder-waits {} decode p50 {:?} us p99 {:?} us advanced {:?}",
            b.width,
            b.height,
            b.decoded,
            b.due,
            b.displayed,
            b.superseded,
            b.decoder_waits,
            b.decode_us_p50,
            b.decode_us_p99,
            b.advanced_every_frame
        ),
        None => println!("bga: off"),
    }
    if let Some(d) = &s.dxgi {
        println!(
            "dxgi: {} samples for {} frames ({} rejected, {} off the refresh grid, {} paired); display prediction error p50 {:?} us, |p99| {:?} us",
            d.samples,
            d.frames,
            d.rejected,
            d.off_grid,
            d.paired,
            d.prediction_error_us_p50,
            d.prediction_error_us_p99_abs
        );
    }
    if s.mode != "offscreen" {
        println!(
            "heavy windows: G2 render cpu p99 {:?} us, G7 bga displayed {:?} %",
            s.heavy_render_cpu_us_p99, s.heavy_bga_displayed_pct
        );
    }
    println!(
        "allocations during play: app {:?} (other threads {}), library {:?} [{}]",
        s.app_allocs, s.other_thread_app_allocs, s.library_allocs, s.allocs_scope
    );
    if let Some(mhz) = s.refresh_mhz_used {
        let on = s
            .monitor
            .as_ref()
            .map_or("unknown".to_owned(), |m| m.describe());
        let others: Vec<String> = s.other_displays.iter().map(|d| d.describe()).collect();
        println!(
            "display: {on} (grid {mhz} mHz); other displays: {}",
            if others.is_empty() {
                "none".to_owned()
            } else {
                others.join("; ")
            }
        );
    }
    for n in &s.notes {
        println!("note: {n}");
    }
    if let Some(c) = s.readback_distinct_colors {
        println!(
            "readback: {} ({c} distinct colors)",
            if c >= 16 { "non-blank" } else { "blank" }
        );
    }
    if let Some(c) = s.bga_only_readback_distinct_colors {
        println!(
            "bga-only readback: {} ({c} distinct colors)",
            if c >= 16 { "non-blank" } else { "blank" }
        );
    }
}

fn distinct_colors(pixels: &[u8]) -> usize {
    let mut colors: Vec<u32> = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .step_by(97)
        .map(|p| u32::from_le_bytes(*p))
        .collect();
    colors.sort_unstable();
    colors.dedup();
    colors.len()
}

/// Offscreen run (the WARP smoke): `frames` frames on a virtual 60 Hz clock;
/// each frame waits for its due BGA frame so the result does not depend on
/// the software rasterizer's speed. Fails on a decoder error or stall.
pub fn offscreen(args: &RunArgs, size: (u32, u32)) -> Result<ExitCode, String> {
    alloc::tag_thread(Tag::Render);
    let clock = Clock::start();
    let instance = instance();
    let adapter = adapter(&instance, &args.adapter, None)?;
    let info = adapter.get_info();
    let (dev, queue) = device(&adapter)?;
    let mut bga = BgaState::open(&args.bga)?;
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let dims = bga.bga.as_ref().map(|b| (b.width, b.height));
    let mut renderer = Renderer::new(dev, queue, format, dims);
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
    let interval = bga.interval_us();
    let play_start = clock.now_us();
    let mut advanced = true;
    for i in 0..n {
        if i == 1 {
            // Frame 0 creates lazily initialized state; count from frame 1.
            alloc::arm();
        }
        let virtual_us = i as i64 * VIRTUAL_FRAME_US;
        let due = (virtual_us / interval) as u64;
        let t0 = qpc();
        let load = scene.build(virtual_us, &mut frame);
        let shown = bga.step(&mut renderer, due, true)?;
        advanced &= bga.bga.is_none() || shown == Some(due);
        renderer.draw(&view, size, &frame);
        let t1 = qpc();
        scope(Scope::OtherLibrary, || {
            let _ = renderer.device().poll(wgpu::PollType::wait_indefinitely());
        });
        rec.frame(
            SpikeFrame {
                render_us: clock.to_us(t0),
                present_us: None,
                render_cpu_us: ((t1 - t0) * 1_000_000 / qpc_freq()) as u32,
                present_call_us: 0,
                present_count: None,
                visible_notes: load.visible_notes,
                effect_instances: load.effect_instances,
                lanes_with_effects: load.lanes_with_effects,
                bga_frame_shown: shown.is_some(),
                bga_index: shown,
            },
            shown,
            &frame,
            load,
        );
    }
    alloc::disarm();
    let readback = distinct_colors(&renderer.read_back(&target, size));
    // The BGA alone (no sprites), so its pixels are checked independently.
    let bga_only = bga.bga.is_some().then(|| {
        frame.alpha.clear();
        frame.additive.clear();
        renderer.draw(&view, size, &frame);
        distinct_colors(&renderer.read_back(&target, size))
    });
    let virtual_play = n as i64 * VIRTUAL_FRAME_US;
    let due = bga_due(&rec, &bga, play_start, virtual_play, |_| true);
    let mut summary = summary_common("offscreen", &info, size, &rec);
    summary.bga = bga.summary(&due).map(|mut b| {
        b.advanced_every_frame = Some(advanced);
        b
    });
    summary.readback_distinct_colors = Some(readback);
    summary.bga_only_readback_distinct_colors = bga_only;
    print_summary(&summary);
    write_outputs(&args.out, &rec, &due, &summary)?;
    let bga_ok = bga.bga.is_none() || (advanced && bga_only.is_some_and(|c| c >= 16));
    Ok(if readback >= 16 && bga_ok {
        ExitCode::SUCCESS
    } else {
        eprintln!("run: smoke failed (blank readback, BGA not advancing, or blank BGA)");
        ExitCode::from(1)
    })
}

/// A window event that V5 cares about.
pub fn event(clock: &Clock, kind: EventKind) -> RunEvent {
    RunEvent {
        t_us: clock.now_us(),
        kind,
    }
}
