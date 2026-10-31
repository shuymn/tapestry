//! The renderer: one instanced sprite pipeline per blend over the atlas,
//! drawing a skin's batch into a swap chain or an offscreen target.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bms_model::Chart;
use eval::Section;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Dxgi::{DXGI_FRAME_STATISTICS, IDXGISwapChain3};
use windows::Win32::System::Threading::WaitForSingleObject;

use crate::atlas::{ATLAS_SIZE, build_atlas};
use crate::gpu::{self, AdapterChoice, Gpu};
use crate::measure::{FrameStatistics, Presentation};
use crate::simple::layout::{HEIGHT, WIDTH};
use crate::simple::text_screen;
use crate::text::MenuText;
use crate::{FrameInput, PlaySkin, SimpleSkin, SkinError, Sprite, SpriteBatch};

/// The next [`Renderer`]'s id.
static RENDERERS: AtomicU64 = AtomicU64::new(0);

/// Sprites one frame can draw, across both blends.
pub const MAX_SPRITES: usize = 8_192;

const SPRITE_WGSL: &str = r"
struct Inst {
    @location(0) rect: vec4f,
    @location(1) uv: vec4f,
    @location(2) color: vec4f,
};
struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) color: vec4f,
};
@group(0) @binding(0) var atlas: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

// Layout pixels (1920x1080, origin top left) to clip space; the viewport
// letterboxes the layout into the target.
@vertex
fn vs(@builtin(vertex_index) vi: u32, i: Inst) -> VOut {
    let c = vec2f(f32(vi & 1u), f32((vi >> 1u) & 1u));
    let p = i.rect.xy + c * i.rect.zw;
    var o: VOut;
    o.pos = vec4f(p.x / 960.0 - 1.0, 1.0 - p.y / 540.0, 0.0, 1.0);
    o.uv = mix(i.uv.xy, i.uv.zw, c);
    o.color = i.color;
    return o;
}

@fragment
fn fs(v: VOut) -> @location(0) vec4f {
    return textureSample(atlas, samp, v.uv) * v.color;
}
";

/// Where frames go.
pub enum Target {
    /// A window's swap chain: FIFO, one frame of latency, the waitable object
    /// left to [`Renderer::wait_for_frame`] (design.md "Display Latency and
    /// Stutter"). The window is borderless fullscreen; making it so is the
    /// window owner's job.
    Window {
        /// The window to draw into.
        window: wgpu::SurfaceTarget<'static>,
        /// Its client size in pixels.
        width: u32,
        /// Its client size in pixels.
        height: u32,
    },
    /// A texture of this size, read back with [`Renderer::read_back`].
    Offscreen {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
    },
}

/// Why the renderer could not start or load a chart.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// No adapter matched the choice.
    #[error("adapter")]
    Adapter(#[source] wgpu::RequestAdapterError),
    /// The WARP adapter was chosen, and none of the adapters is one.
    #[error("adapter: no WARP adapter among {names:?}")]
    NoSoftwareAdapter {
        /// The adapters found.
        names: Vec<String>,
    },
    /// The adapter refused the device.
    #[error("device")]
    Device(#[source] wgpu::RequestDeviceError),
    /// The device failed waiting for its uploads.
    #[error("device")]
    Poll(#[source] wgpu::PollError),
    /// The window's surface could not be created.
    #[error("surface")]
    Surface(#[source] wgpu::CreateSurfaceError),
    /// The adapter cannot configure the window's surface.
    #[error("surface: unsupported by the adapter")]
    SurfaceUnsupported,
    /// The skin cannot draw the chart.
    #[error("skin")]
    Skin(#[source] SkinError),
    /// The window's surface was lost or failed validation; the renderer
    /// must be built again.
    #[error("surface lost")]
    SurfaceLost,
    /// A menu's text could not be drawn as prepared.
    #[error("text")]
    Text(#[source] glyphon::RenderError),
    /// A menu's text was made for another renderer, whose device it uses.
    #[error("text: made for another renderer")]
    OtherRenderersText,
    /// A menu's text could not be prepared ([`crate::text::MenuText::prepare`]).
    #[error(transparent)]
    TextPrepare(#[from] crate::text::TextError),
}

/// What one frame cost and drew.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameTiming {
    /// CPU time of update, build and submit, waits excluded: acquiring the swap
    /// chain image and presenting are not counted (design.md "Latency
    /// Figures").
    pub cpu: Duration,
    /// Sprites drawn.
    pub sprites: usize,
    /// Pushes the full batch refused: an overflow indicator, a lower bound
    /// on what the frame left out (see [`SpriteBatch::dropped`]).
    pub dropped: u32,
    /// Whether the frame reached the target; `false` when the swap chain
    /// image could not be acquired.
    pub presented: bool,
    /// The swap chain's present count of this frame, read right after its
    /// present; `None` offscreen, for a frame not presented, or when the call
    /// failed. It pairs the frame with its frame statistics (design.md "Latency
    /// Figures").
    pub present_count: Option<u32>,
}

struct Window {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    /// The swap chain's frame latency waitable object, as a raw value so
    /// the renderer stays `Send`.
    waitable: Option<usize>,
    /// The swap chain itself, for its present count and frame statistics.
    chain: Option<IDXGISwapChain3>,
}

impl Window {
    fn configure(&mut self, device: &wgpu::Device) {
        self.surface.configure(device, &self.config);
        // SAFETY: the handle is only read here; it belongs to the swap chain
        // just configured, which lives as long as the surface.
        let hal = unsafe { self.surface.as_hal::<wgpu_hal::api::Dx12>() };
        // SAFETY: as above; the swap chain reference is counted.
        self.waitable = hal
            .as_ref()
            .and_then(|s| unsafe { s.waitable_handle() })
            .map(|h| h.0 as usize);
        self.chain = hal.and_then(|s| s.swap_chain());
    }
}

/// Where the renderer draws.
enum Output {
    Window(Window),
    Offscreen {
        texture: wgpu::Texture,
        view: wgpu::TextureView,
    },
}

/// The play screen renderer.
pub struct Renderer {
    /// Unique among the renderers of the process, for the menu text tied to
    /// this one's device.
    id: u64,
    gpu: Gpu,
    output: Output,
    size: (u32, u32),
    format: wgpu::TextureFormat,
    alpha: wgpu::RenderPipeline,
    additive: wgpu::RenderPipeline,
    atlas_bind: wgpu::BindGroup,
    instances: wgpu::Buffer,
    batch: SpriteBatch,
    /// The batch in drawing order, copied into `instances` each frame.
    staging: Vec<Sprite>,
    skin: Box<dyn PlaySkin>,
    /// When the built frame's work began, until [`submit`](Self::submit)
    /// takes it.
    started: Option<Instant>,
}

impl Renderer {
    /// Sets up the adapter, device, target, pipelines and atlas, with the
    /// simple skin and no chart.
    ///
    /// # Errors
    ///
    /// Returns an error when no adapter matches `adapter`, the device is
    /// refused, or the window's surface cannot be used.
    pub fn new(target: Target, adapter: AdapterChoice) -> Result<Self, RenderError> {
        let instance = gpu::instance();
        let (surface, size) = match target {
            Target::Window {
                window,
                width,
                height,
            } => {
                let s = instance
                    .create_surface(window)
                    .map_err(RenderError::Surface)?;
                (Some(s), (width.max(1), height.max(1)))
            }
            Target::Offscreen { width, height } => (None, (width.max(1), height.max(1))),
        };
        let adapter = gpu::adapter(&instance, adapter, surface.as_ref())?;
        let gpu = gpu::device(&adapter)?;
        let device = gpu.device();
        let (output, format) = match surface {
            Some(surface) => {
                let mut config = surface
                    .get_default_config(&adapter, size.0, size.1)
                    .ok_or(RenderError::SurfaceUnsupported)?;
                let caps = surface.get_capabilities(&adapter);
                if caps.formats.contains(&wgpu::TextureFormat::Bgra8Unorm) {
                    config.format = wgpu::TextureFormat::Bgra8Unorm;
                }
                config.present_mode = wgpu::PresentMode::Fifo;
                config.desired_maximum_frame_latency = 1;
                let format = config.format;
                let mut w = Window {
                    surface,
                    config,
                    waitable: None,
                    chain: None,
                };
                w.configure(device);
                (Output::Window(w), format)
            }
            None => {
                let format = wgpu::TextureFormat::Rgba8Unorm;
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("offscreen"),
                    size: extent(size.0, size.1),
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                (Output::Offscreen { texture, view }, format)
            }
        };
        let (alpha, additive, atlas_bind) = pipelines(&gpu, format);
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sprites"),
            size: (MAX_SPRITES * size_of::<Sprite>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        finish_uploads(&gpu)?;
        Ok(Self {
            gpu,
            output,
            size,
            format,
            alpha,
            additive,
            atlas_bind,
            instances,
            batch: SpriteBatch::with_capacity(MAX_SPRITES),
            staging: Vec::with_capacity(MAX_SPRITES),
            skin: Box::new(SimpleSkin::new()),
            started: None,
            id: RENDERERS.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// Identifies this renderer and its device among the process's
    /// renderers.
    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    /// The target's size in pixels.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The target's pixel format.
    pub(crate) const fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// The device, queue and adapter in use.
    #[must_use]
    pub const fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    /// Prepares the skin for `chart` and resets its play state, before the
    /// countdown and at each retry; with a practice round's `section`, only
    /// its targets are drawn.
    ///
    /// # Errors
    ///
    /// Returns the skin's error when it cannot draw `chart`.
    pub fn load(&mut self, chart: &Chart, section: Option<&Section>) -> Result<(), RenderError> {
        self.skin
            .prepare(&self.gpu, chart, section)
            .map_err(RenderError::Skin)?;
        finish_uploads(&self.gpu)
    }

    /// Waits up to `timeout_ms` for the swap chain to take another frame
    /// (its frame latency waitable object). Returns `true` when a frame may
    /// start: at once for an offscreen target or a swap chain without the
    /// object.
    #[must_use]
    pub fn wait_for_frame(&self, timeout_ms: u32) -> bool {
        let waitable = match &self.output {
            Output::Window(w) => w.waitable,
            Output::Offscreen { .. } => None,
        };
        match waitable {
            // SAFETY: the handle belongs to the live swap chain, which is
            // only reconfigured through `&mut self`.
            Some(h) => unsafe {
                WaitForSingleObject(HANDLE(h as *mut _), timeout_ms) == WAIT_OBJECT_0
            },
            None => true,
        }
    }

    /// Whether [`wait_for_frame`](Self::wait_for_frame) waits on a swap
    /// chain's frame latency waitable object.
    #[must_use]
    pub fn has_frame_waitable(&self) -> bool {
        matches!(&self.output, Output::Window(w) if w.waitable.is_some())
    }

    /// Builds, submits and presents one frame: [`build`](Self::build), then
    /// [`submit`](Self::submit).
    ///
    /// # Errors
    ///
    /// As [`submit`](Self::submit).
    pub fn frame(&mut self, f: &FrameInput<'_>) -> Result<FrameTiming, RenderError> {
        self.build(f);
        self.submit()
    }

    /// The app's share of a frame ([`build_frame`]): the skin builds `f`
    /// and the batch is staged, with no call into wgpu. The frame's CPU
    /// time starts here; [`submit`](Self::submit) must follow.
    pub fn build(&mut self, f: &FrameInput<'_>) {
        self.started = Some(Instant::now());
        build_frame(self.skin.as_mut(), f, &mut self.batch, &mut self.staging);
    }

    /// Submits and presents the frame [`build`](Self::build) staged.
    /// Submitting without a build first is a logic error (a debug build
    /// asserts it); the staged batch is drawn again, timed from here.
    ///
    /// A frame whose swap chain image times out or is occluded is skipped
    /// (`presented` is `false`); an outdated swap chain is configured again
    /// and the frame skipped.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::SurfaceLost`] when the surface is lost or
    /// fails validation: the renderer must be built again.
    pub fn submit(&mut self) -> Result<FrameTiming, RenderError> {
        debug_assert!(self.started.is_some(), "submit without a build");
        let start = self.started.take().unwrap_or_else(Instant::now);
        self.draw(start, None)
    }

    /// Draws a screen of text lines in the bitmap font instead of the skin
    /// (the end screen; see [`text_screen`]), as [`frame`](Self::frame)
    /// draws.
    ///
    /// # Errors
    ///
    /// As [`frame`](Self::frame).
    pub fn text_frame(&mut self, lines: &[&str]) -> Result<FrameTiming, RenderError> {
        let start = Instant::now();
        self.batch.clear();
        text_screen(lines, &mut self.batch);
        self.batch.stage(&mut self.staging);
        self.draw(start, None)
    }

    /// Draws a menu screen, as [`frame`](Self::frame) draws: the sprites
    /// `sprites` pushes, then in a pass of its own the text last prepared
    /// on `text` (design.md "Rendering and Skins"). Menus only; play
    /// frames never draw text this way.
    ///
    /// # Errors
    ///
    /// As [`frame`](Self::frame); [`RenderError::OtherRenderersText`],
    /// drawing nothing, when `text` was made for another renderer (one this
    /// replaced: the screen makes and prepares its text again); and
    /// [`RenderError::Text`] when glyphon cannot draw it as prepared.
    pub fn menu_frame(
        &mut self,
        sprites: impl FnOnce(&mut SpriteBatch),
        text: &MenuText,
    ) -> Result<FrameTiming, RenderError> {
        if text.owner() != self.id {
            return Err(RenderError::OtherRenderersText);
        }
        let start = Instant::now();
        self.batch.clear();
        sprites(&mut self.batch);
        self.batch.stage(&mut self.staging);
        self.draw(start, Some(text))
    }

    /// Submits and presents the staged batch, then `text` in its own pass
    /// when there is one; `start` is when the frame's work began.
    fn draw(
        &mut self,
        start: Instant,
        text: Option<&MenuText>,
    ) -> Result<FrameTiming, RenderError> {
        let alpha = self.batch.alpha().len() as u32;
        let sprites = self.staging.len();
        let timing = |cpu, presented| FrameTiming {
            cpu,
            sprites,
            dropped: self.batch.dropped(),
            presented,
            present_count: None,
        };
        let acquire = Instant::now();
        let (frame, view) = match &mut self.output {
            Output::Window(w) => match w.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t)
                | wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                    let view = t
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    (Some(t), view)
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    let cpu = start.elapsed().saturating_sub(acquire.elapsed());
                    return Ok(timing(cpu, false));
                }
                wgpu::CurrentSurfaceTexture::Outdated => {
                    w.configure(self.gpu.device());
                    let cpu = start.elapsed().saturating_sub(acquire.elapsed());
                    return Ok(timing(cpu, false));
                }
                wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Validation => {
                    return Err(RenderError::SurfaceLost);
                }
            },
            Output::Offscreen { view, .. } => (None, view.clone()),
        };
        let acquired = acquire.elapsed();
        let queue = self.gpu.queue();
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&self.staging));
        let [x, y, w, h] = letterbox(self.size.0, self.size.1);
        let mut encoder = self
            .gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_viewport(x, y, w, h, 0.0, 1.0);
            pass.set_bind_group(0, &self.atlas_bind, &[]);
            pass.set_vertex_buffer(0, self.instances.slice(..));
            pass.set_pipeline(&self.alpha);
            pass.draw(0..4, 0..alpha);
            pass.set_pipeline(&self.additive);
            pass.draw(0..4, alpha..sprites as u32);
        }
        if let Some(text) = text {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("menu text"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            text.render(&mut pass).map_err(RenderError::Text)?;
        }
        queue.submit([encoder.finish()]);
        let cpu = start.elapsed() - acquired;
        let mut out = timing(cpu, true);
        match frame {
            Some(t) => {
                queue.present(t);
                out.present_count = self.chain().and_then(|c| {
                    // SAFETY: a plain query of the live swap chain.
                    unsafe { c.GetLastPresentCount() }.ok()
                });
            }
            None => {
                // Offscreen: let the device reclaim finished frames.
                let _ = self.gpu.device().poll(wgpu::PollType::Poll);
            }
        }
        Ok(out)
    }

    fn chain(&self) -> Option<&IDXGISwapChain3> {
        match &self.output {
            Output::Window(w) => w.chain.as_ref(),
            Output::Offscreen { .. } => None,
        }
    }

    /// How the swap chain presents, from its configuration; `None`
    /// offscreen.
    #[must_use]
    pub fn presentation(&self) -> Option<Presentation> {
        let Output::Window(w) = &self.output else {
            return None;
        };
        Some(match w.config.present_mode {
            wgpu::PresentMode::Fifo => Presentation {
                sync_interval: 1,
                tearing: false,
            },
            wgpu::PresentMode::FifoRelaxed => Presentation {
                sync_interval: 1,
                tearing: true,
            },
            wgpu::PresentMode::Immediate => Presentation {
                sync_interval: 0,
                tearing: true,
            },
            _ => Presentation {
                sync_interval: 0,
                tearing: false,
            },
        })
    }

    /// The swap chain's latest DXGI frame statistics (design.md "Latency
    /// Figures", "Mapping the Three Clocks"), raw: `None` offscreen or when the
    /// call fails. Pair them with frames through [`FrameTiming::present_count`]
    /// and filter them with [`StatsFilter`](crate::measure::StatsFilter).
    #[must_use]
    pub fn frame_statistics(&self) -> Option<FrameStatistics> {
        let chain = self.chain()?;
        let mut s = DXGI_FRAME_STATISTICS::default();
        // SAFETY: the live swap chain writes one struct.
        unsafe { chain.GetFrameStatistics(&raw mut s) }.ok()?;
        Some(FrameStatistics {
            present_count: s.PresentCount,
            present_refresh_count: s.PresentRefreshCount,
            sync_refresh_count: s.SyncRefreshCount,
            sync_qpc_time: s.SyncQPCTime,
        })
    }

    /// The offscreen target's pixels, RGBA8, row by row from the top, after
    /// every submitted frame finished. A window target has none and gives
    /// an empty vector.
    #[must_use]
    #[expect(
        clippy::expect_used,
        clippy::indexing_slicing,
        reason = "read-back for frame dumps and tests, outside play: a failed map is a GPU fault, and each row lies inside the padded buffer"
    )]
    pub fn read_back(&self) -> Vec<u8> {
        let Output::Offscreen { texture, .. } = &self.output else {
            return Vec::new();
        };
        let device = self.gpu.device();
        let (width, height) = self.size;
        let row = width * 4;
        let padded =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("read back"),
            size: u64::from(padded * height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            extent(width, height),
        );
        self.gpu.queue().submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map the read back"));
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("wait for the read back");
        let data = slice.get_mapped_range().expect("mapped read back");
        let mut out = Vec::with_capacity((row * height) as usize);
        for y in 0..height as usize {
            let start = y * padded as usize;
            out.extend_from_slice(&data[start..start + row as usize]);
        }
        out
    }
}

/// The app's share of a frame, everything [`Renderer::build`] does before
/// [`Renderer::submit`] calls into wgpu: `skin` builds into the emptied `batch`, which is then
/// staged into `staging` in drawing order. It allocates nothing once `batch`
/// and `staging` have room for the batch's capacity.
pub fn build_frame(
    skin: &mut dyn PlaySkin,
    f: &FrameInput<'_>,
    batch: &mut SpriteBatch,
    staging: &mut Vec<Sprite>,
) {
    batch.clear();
    skin.build(f, batch);
    batch.stage(staging);
}

/// Submits every upload queued on `gpu` and waits until the GPU has done them,
/// so none is left for the first frame of play (design.md "Display Latency and
/// Stutter").
fn finish_uploads(gpu: &Gpu) -> Result<(), RenderError> {
    gpu.queue().submit([]);
    gpu.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .map(|_| ())
        .map_err(RenderError::Poll)
}

/// The viewport `[x, y, width, height]` that fits the 1920×1080 layout
/// into a `width` × `height` target at the largest scale, centered, with
/// black bars on the other axis.
#[must_use]
pub fn letterbox(width: u32, height: u32) -> [f32; 4] {
    let (w, h) = (width as f32, height as f32);
    let s = (w / WIDTH).min(h / HEIGHT);
    let (vw, vh) = (WIDTH * s, HEIGHT * s);
    [(w - vw) / 2.0, (h - vh) / 2.0, vw, vh]
}

const fn extent(width: u32, height: u32) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    }
}

/// The alpha and additive sprite pipelines and the atlas binding, with the
/// atlas uploaded.
fn pipelines(
    gpu: &Gpu,
    format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::RenderPipeline, wgpu::BindGroup) {
    use wgpu::util::DeviceExt;

    let device = gpu.device();
    let atlas = device.create_texture_with_data(
        gpu.queue(),
        &wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: extent(ATLAS_SIZE, ATLAS_SIZE),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        },
        wgpu::util::TextureDataOrder::LayerMajor,
        &build_atlas(),
    );
    // Nearest sampling keeps the bitmap font's texels sharp.
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("atlas"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("atlas"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("sprites"),
        source: wgpu::ShaderSource::Wgsl(SPRITE_WGSL.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("sprites"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = |blend| {
        let attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sprites"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<Sprite>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attrs,
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(blend),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..wgpu::PrimitiveState::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
    };
    let alpha = pipeline(wgpu::BlendState::ALPHA_BLENDING);
    let additive = pipeline(wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::SrcAlpha,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent::OVER,
    });
    (alpha, additive, bind)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use wgpu::wgc::instance as core;

    use super::{RenderError, letterbox};
    use crate::SkinError;

    /// Checks that `e` names only its step, and that its source is the
    /// wrapped `T`, where the chain ends.
    #[track_caller]
    fn the_cause<'e, T: Error + 'static>(e: &'e RenderError, step: &str) -> &'e T {
        assert_eq!(e.to_string(), step);
        let source = e.source().expect("the wrapped error is the source");
        assert!(!e.to_string().contains(&source.to_string()));
        assert!(source.source().is_none());
        source
            .downcast_ref::<T>()
            .expect("the source keeps its type")
    }

    /// Each wrapped failure is the source, printed once in the chain. The
    /// device and surface errors are built from their wgpu-core forms,
    /// which the DX12 backend compiles in.
    #[test]
    fn wrapped_failures_are_the_source() {
        let adapter = RenderError::Adapter(wgpu::RequestAdapterError::EnvNotSet);
        assert_eq!(
            the_cause::<wgpu::RequestAdapterError>(&adapter, "adapter"),
            &wgpu::RequestAdapterError::EnvNotSet
        );

        let unsupported = core::RequestDeviceError::UnsupportedFeature(wgpu::Features::empty());
        let text = unsupported.to_string();
        let device = RenderError::Device(unsupported.into());
        let source = the_cause::<wgpu::RequestDeviceError>(&device, "device");
        assert_eq!(source.to_string(), text);

        let poll = RenderError::Poll(wgpu::PollError::Timeout);
        let source = the_cause::<wgpu::PollError>(&poll, "device");
        assert!(matches!(source, wgpu::PollError::Timeout));

        let mismatch = core::CreateSurfaceError::MismatchingDisplayHandle;
        let text = mismatch.to_string();
        let surface = RenderError::Surface(mismatch.into());
        let source = the_cause::<wgpu::CreateSurfaceError>(&surface, "surface");
        assert_eq!(source.to_string(), text);

        let skin = SkinError {
            reason: "the chart has too many lanes".to_owned(),
        };
        let e = RenderError::Skin(skin.clone());
        assert_eq!(the_cause::<SkinError>(&e, "skin"), &skin);

        let changed = glyphon::RenderError::ScreenResolutionChanged;
        let e = RenderError::Text(changed);
        assert_eq!(the_cause::<glyphon::RenderError>(&e, "text"), &changed);
    }

    #[test]
    fn leaf_failures_name_their_reason() {
        assert!(RenderError::SurfaceLost.source().is_none());
        assert!(RenderError::SurfaceUnsupported.source().is_none());
        let e = RenderError::OtherRenderersText;
        assert!(e.source().is_none());
        assert_eq!(e.to_string(), "text: made for another renderer");
        let e = RenderError::NoSoftwareAdapter {
            names: vec!["GPU".to_owned()],
        };
        assert_eq!(e.to_string(), "adapter: no WARP adapter among [\"GPU\"]");
    }

    #[test]
    fn letterbox_fits_the_layout_centered() {
        assert_eq!(letterbox(1920, 1080), [0.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(letterbox(960, 540), [0.0, 0.0, 960.0, 540.0]);
        // 5:4: scaled to the width (2/3), bars above and below.
        assert_eq!(letterbox(1280, 1024), [0.0, 152.0, 1280.0, 720.0]);
        // 21:9: scaled to the height, bars left and right.
        assert_eq!(letterbox(2560, 1080), [320.0, 0.0, 1920.0, 1080.0]);
    }
}
