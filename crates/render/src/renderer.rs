//! The renderer: one instanced sprite pipeline per blend over the atlas
//! texture array, drawing a skin's batch, run by run in its order and
//! under each run's scissor, projected from the skin's canvas into a swap
//! chain or an offscreen target.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bms_model::Chart;
use eval::Section;
use skin_model::{AtlasLimits, BuildError};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Dxgi::{DXGI_FRAME_STATISTICS, IDXGISwapChain3};
use windows::Win32::System::Threading::WaitForSingleObject;

use crate::decl::{DeclSkin, SkinBundle};
use crate::gpu::{self, AdapterChoice, Gpu};
use crate::measure::{FrameStatistics, Presentation};
use crate::pace::Pacer;
use crate::projection::{ClipTable, Projection};
use crate::simple::layout::{HEIGHT, WIDTH};
use crate::simple::{result_screen, text_screen};
use crate::skin_atlas::{self, AtlasPages, AtlasTexture, UploadError};
use crate::staging::{Scissor, Staging};
use crate::text::MenuText;
use crate::{
    AttemptInput, Blend, FrameInput, Instance, OVERLAY_SPRITES, PlaySkin, SimpleSkin, SkinError,
    SpriteBatch,
};

/// The next [`Renderer`]'s id.
static RENDERERS: AtomicU64 = AtomicU64::new(0);

/// Sprites one frame of the built-in screens can draw, across both
/// blends: the least the renderer's batch holds ([`PlaySkin::capacity`]).
pub const MAX_SPRITES: usize = 8_192;

const SPRITE_WGSL: &str = r"
// An `Instance`: its pivot and angle share location 3, and its flags hold
// the atlas layer (low 16 bits) and the nearest-filter bit.
struct Inst {
    @location(0) rect: vec4f,
    @location(1) uv: vec4f,
    @location(2) color: vec4f,
    @location(3) turn: vec3f,
    @location(4) flags: u32,
};
struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) color: vec4f,
    @location(2) @interpolate(flat) flags: u32,
};
// Half the canvas's size, in canvas pixels.
struct Proj {
    half: vec2f,
    pad: vec2f,
};
@group(0) @binding(0) var atlas: texture_2d_array<f32>;
@group(0) @binding(1) var smooth_samp: sampler;
@group(0) @binding(2) var nearest_samp: sampler;
@group(0) @binding(3) var<uniform> proj: Proj;

// Canvas pixels (origin top left, y down) to clip space; the viewport places
// the canvas on the target. Each corner turns clockwise on screen by the
// angle about the pivot; an unturned sprite keeps its corners exactly.
@vertex
fn vs(@builtin(vertex_index) vi: u32, i: Inst) -> VOut {
    let c = vec2f(f32(vi & 1u), f32((vi >> 1u) & 1u));
    let placed = i.rect.xy + c * i.rect.zw;
    let d = placed - i.turn.xy;
    let s = sin(i.turn.z);
    let k = cos(i.turn.z);
    let turned = i.turn.xy + vec2f(d.x * k - d.y * s, d.x * s + d.y * k);
    let p = select(turned, placed, i.turn.z == 0.0);
    var o: VOut;
    o.pos = vec4f(p.x / proj.half.x - 1.0, 1.0 - p.y / proj.half.y, 0.0, 1.0);
    o.uv = mix(i.uv.xy, i.uv.zw, c);
    o.color = i.color;
    o.flags = i.flags;
    return o;
}

// The atlas holds premultiplied texels and the colour arrives premultiplied,
// so their product is premultiplied. A skin's page is sampled bilinearly
// between its mip levels, or at the nearest texel of the nearest level with
// the nearest bit; layer 0, the built-in atlas, always at its nearest
// level-0 texel. All three are sampled, so every sample stays in uniform
// control flow, and one is chosen.
@fragment
fn fs(v: VOut) -> @location(0) vec4f {
    let layer = i32(v.flags & 0xFFFFu);
    let smoothed = textureSample(atlas, smooth_samp, v.uv, layer);
    let nearest = textureSample(atlas, nearest_samp, v.uv, layer);
    let builtin = textureSampleLevel(atlas, nearest_samp, v.uv, layer, 0.0);
    let page = select(smoothed, nearest, (v.flags & 0x10000u) != 0u);
    return select(page, builtin, layer == 0) * v.color;
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
    /// The skin refused a frame ([`Renderer::frame`] only; the player
    /// builds with [`Renderer::build`], whose refusal is typed): the
    /// attempt is over, and the renderer stays usable.
    #[error("skin build")]
    Build(#[source] BuildError),
    /// A skin's atlas was not uploaded; the atlas in use stays.
    #[error("atlas")]
    Atlas(#[source] UploadError),
    /// A skin's sprites do not fit the device's largest buffer; the skin
    /// in use stays.
    #[error("capacity: {sprites} sprites, past the device's {max} bytes")]
    Capacity {
        /// The sprites the skin asked room for.
        sprites: u32,
        /// The device's largest buffer, in bytes.
        max: u64,
    },
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

impl RenderError {
    /// Whether this is a refusal of what was asked while the device stays
    /// healthy (design.md "Play Lifecycle", preparing each attempt): the
    /// skin cannot draw the attempt, its atlas is past the checks or was
    /// refused by the device for memory or validation, or its sprites do
    /// not fit a buffer. The renderer is unchanged and goes on drawing; an
    /// attempt is refused, not the screen. Anything else, the device or
    /// the surface failing, drops the screen.
    #[must_use]
    pub const fn is_refusal(&self) -> bool {
        match self {
            Self::Skin(_) | Self::Capacity { .. } => true,
            Self::Atlas(UploadError::Device(e)) => matches!(
                e,
                wgpu::Error::OutOfMemory { .. } | wgpu::Error::Validation { .. }
            ),
            Self::Atlas(_) => true,
            _ => false,
        }
    }
}

/// An installation staged by [`Renderer::stage`] and not yet committed:
/// the request it answers and the renderer, so its device, it was staged
/// on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedToken {
    /// The preparation request it answers.
    pub request: u64,
    /// [`Renderer::device_generation`] of the renderer that staged it.
    pub device: u64,
}

/// Why [`Renderer::commit`] installed nothing. The installation in use
/// stays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CommitError {
    /// Nothing is staged: it was discarded, superseded, or never staged.
    #[error("no installation is staged")]
    NothingStaged,
    /// The staged installation answers another request.
    #[error("the staged installation answers request {staged}, not {asked}")]
    Request {
        /// The request staged.
        staged: u64,
        /// The request committed.
        asked: u64,
    },
    /// The token was staged on another renderer, whose device this one
    /// replaced.
    #[error("the installation was staged on another device")]
    Device,
}

/// The GPU and CPU resources a declarative skin's installation replaces
/// together: its atlas array and bind group, and the staging and instance
/// buffer sized for its capacities, with its clips.
struct Resources {
    atlas: AtlasTexture,
    atlas_bind: wgpu::BindGroup,
    staging: Staging,
    instances: wgpu::Buffer,
}

/// An installation staged and not yet committed: the fresh skin with its
/// prepared attempt, and its own resources, or `None` when it reuses the
/// installed bundle's.
struct Pending {
    token: PreparedToken,
    skin: DeclSkin,
    resources: Option<Resources>,
}

/// What a renderer holds in memory for its play skin, in bytes, as a
/// witness of the peak a skin's replacement reaches (design.md "Memory
/// Budget"): the installed resources and the staged ones live together
/// until the commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryWitness {
    /// The installed bundle's CPU description, kept for recovery.
    pub committed_cpu: u64,
    /// The staged bundle's CPU description, when it is another.
    pub staged_cpu: u64,
    /// The installed atlas array: every layer's levels.
    pub atlas: u64,
    /// The staged atlas array, when it is a new one.
    pub staged_atlas: u64,
    /// The bytes the queue's staging belt took for the staged atlas's
    /// upload: each row padded to 256 bytes.
    pub upload: u64,
    /// The installed batch's sprites and runs, its staged instances and
    /// its instance buffer.
    pub batch: u64,
    /// The same for the staged resources.
    pub staged_batch: u64,
}

impl MemoryWitness {
    /// Everything counted.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.committed_cpu
            .saturating_add(self.staged_cpu)
            .saturating_add(self.atlas)
            .saturating_add(self.staged_atlas)
            .saturating_add(self.upload)
            .saturating_add(self.batch)
            .saturating_add(self.staged_batch)
    }
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

/// The sprite pipelines' bindings that outlive an atlas: the layout, the
/// samplers and the projection's uniform.
struct Bindings {
    layout: wgpu::BindGroupLayout,
    /// Bilinear between mip levels, for a skin's ordinary images.
    smooth: wgpu::Sampler,
    /// The nearest texel of the nearest level, for pixel art and, at level
    /// 0, the built-in atlas.
    nearest: wgpu::Sampler,
    /// Half the canvas's size ([`Projection::half`]), padded to 16 bytes.
    projection: wgpu::Buffer,
    /// The half last written to [`projection`](Self::projection), `None`
    /// until the first write: a frame writes it only when it changes.
    uploaded: Option<[f32; 2]>,
}

impl Bindings {
    /// The bind group of `atlas`.
    fn bind(&self, device: &wgpu::Device, atlas: &AtlasTexture) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(atlas.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.smooth),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.nearest),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.projection.as_entire_binding(),
                },
            ],
        })
    }
}

/// The play screen renderer.
pub struct Renderer {
    /// Unique among the renderers of the process, for the menu text tied to
    /// this one's device.
    id: u64,
    gpu: Gpu,
    output: Output,
    /// The target's size as last applied, never zero.
    size: (u32, u32),
    /// The window is minimized (the last size applied was zero): nothing
    /// is configured or presented until a nonzero size comes.
    minimized: bool,
    format: wgpu::TextureFormat,
    alpha: wgpu::RenderPipeline,
    additive: wgpu::RenderPipeline,
    bindings: Bindings,
    atlas: AtlasTexture,
    atlas_bind: wgpu::BindGroup,
    instances: wgpu::Buffer,
    /// The batch, its staging copied into `instances` each frame, and the
    /// play skin's projection and clip scissors.
    staging: Staging,
    skin: Box<dyn PlaySkin>,
    /// The declarative skin's bundle installed, `None` for the simple
    /// skin or one set with [`set_skin`](Self::set_skin).
    installed: Option<Arc<SkinBundle>>,
    /// Bumped each time the atlas and the staging are replaced: a test of
    /// whether an installation reused them.
    resources: u64,
    /// The installation staged and not yet committed.
    pending: Option<Pending>,
    /// When the built frame's work began, until [`submit`](Self::submit)
    /// takes it.
    started: Option<Instant>,
    /// The skin's refusal of a frame of this attempt: no frame of it is
    /// built again, and nothing submitted, until the next
    /// [`load`](Self::load) prepares a fresh one.
    refused: Option<BuildError>,
    /// An offscreen target's frame left pending on the GPU: at most one,
    /// as a swap chain's frame latency bounds a window's.
    pacer: Pacer<wgpu::SubmissionIndex>,
    /// What each frame is cleared to: opaque black. A unit test clears to
    /// transparent black to read the blends' alpha channel.
    clear: wgpu::Color,
    /// Run by the next [`install_atlas`](Self::install_atlas) on its
    /// candidate, inside its error scopes, after the texture and its bind
    /// group exist: a unit test makes the device refuse something there.
    #[cfg(test)]
    atlas_fault: Option<fn(&Gpu)>,
    /// A staging's instance buffer is made invalid (both map modes, which
    /// every device's validation refuses), as a device refusing its
    /// allocation would: a unit test.
    #[cfg(test)]
    buffer_fault: bool,
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
                let (texture, view) = offscreen(device, size, format);
                (Output::Offscreen { texture, view }, format)
            }
        };
        let (alpha, additive, bindings) = pipelines(&gpu, format);
        let atlas = AtlasTexture::builtin(&gpu);
        let atlas_bind = bindings.bind(device, &atlas);
        let skin: Box<dyn PlaySkin> = Box::new(SimpleSkin::new());
        // The simple skin's runs are as many as its sprites, the staging's
        // default.
        let (capacity, _) = sizes(skin.as_ref());
        let staging = Staging::new(capacity, &skin.canvas([size.0, size.1]));
        let instances = instance_buffer(device, staging.instance_capacity());
        finish_uploads(&gpu)?;
        Ok(Self {
            gpu,
            output,
            size,
            minimized: false,
            format,
            alpha,
            additive,
            bindings,
            atlas,
            atlas_bind,
            instances,
            staging,
            skin,
            installed: None,
            resources: 0,
            pending: None,
            started: None,
            refused: None,
            pacer: Pacer::default(),
            clear: wgpu::Color::BLACK,
            #[cfg(test)]
            atlas_fault: None,
            #[cfg(test)]
            buffer_fault: false,
            id: RENDERERS.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// Identifies this renderer and its device among the process's
    /// renderers.
    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    /// The target's size in pixels, as last applied.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The target's size in pixels as last applied, never zero: what a
    /// frame is projected onto ([`FrameInput::target`]).
    #[must_use]
    pub const fn target(&self) -> [u32; 2] {
        [self.size.0, self.size.1]
    }

    /// Whether the window is minimized: the last size applied was zero,
    /// and frames are neither drawn nor presented until another is.
    #[must_use]
    pub const fn is_minimized(&self) -> bool {
        self.minimized
    }

    /// Applies the target's new physical size, between frames: a window's
    /// swap chain is configured again (its waitable object and swap chain
    /// taken again with it), an offscreen texture made again, and the play
    /// skin's canvas and clips projected again; the skin's play state is
    /// kept. A zero side minimizes: nothing is configured, and frames are
    /// skipped, unpresented, until a nonzero size is applied. A side past
    /// the device's largest texture is clamped to it.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            self.minimized = true;
            return;
        }
        self.minimized = false;
        let max = self.gpu.device().limits().max_texture_dimension_2d;
        let size = (width.min(max), height.min(max));
        if size == self.size {
            return;
        }
        self.size = size;
        let device = self.gpu.device();
        match &mut self.output {
            Output::Window(w) => {
                w.config.width = size.0;
                w.config.height = size.1;
                w.configure(device);
            }
            Output::Offscreen { texture, view } => {
                (*texture, *view) = offscreen(device, size, self.format);
            }
        }
        self.project();
    }

    /// Projects the play skin's canvas and clips onto the current target.
    fn project(&mut self) {
        let canvas = self.skin.canvas(self.target());
        self.staging.project(&canvas);
    }

    /// Replaces the play skin, sizing the batch, the staging and the
    /// instance buffer for its [`capacity`](PlaySkin::capacity) and
    /// [`run_capacity`](PlaySkin::run_capacity) (each at least
    /// [`MAX_SPRITES`]) with the measurement overlay's reserve after them,
    /// and projecting its canvas. Before play: it allocates. The skin is
    /// prepared with [`load`](Self::load).
    ///
    /// # Errors
    ///
    /// [`RenderError::Capacity`] when its sprites and the overlay's do not
    /// fit the device's largest buffer; the skin in use stays.
    pub fn set_skin(&mut self, skin: Box<dyn PlaySkin>) -> Result<(), RenderError> {
        let (capacity, runs) = sizes(skin.as_ref());
        let held = (usize::try_from(capacity).ok(), usize::try_from(runs).ok());
        if held
            != (
                Some(self.staging.capacity()),
                Some(self.staging.run_capacity()),
            )
        {
            let device = self.gpu.device();
            let max = device.limits().max_buffer_size;
            let instances = capacity.saturating_add(OVERLAY_SPRITES);
            let instances = usize::try_from(instances).unwrap_or(usize::MAX);
            if instance_bytes(instances) > max {
                return Err(RenderError::Capacity {
                    sprites: capacity,
                    max,
                });
            }
            let staged = self.staging.instance_capacity();
            self.staging.set_capacity(capacity, runs);
            if self.staging.instance_capacity() != staged {
                self.instances = instance_buffer(device, self.staging.instance_capacity());
            }
        }
        self.skin = skin;
        self.installed = None;
        self.refused = None;
        self.project();
        Ok(())
    }

    /// The limits a skin's atlas is built within on this device
    /// (`skin_model::Sheet::atlas`), the built-in atlas's reserved layer
    /// included.
    #[must_use]
    pub fn atlas_limits(&self) -> AtlasLimits {
        skin_atlas::atlas_limits(self.gpu.device())
    }

    /// Uploads a skin's atlas `pages` with the built-in atlas in layer 0,
    /// and takes `clips`, the fixed clips its runs name, projected onto the
    /// current target; both replace the ones in use together, once the
    /// upload has finished. Before play: it allocates and waits for the
    /// GPU.
    ///
    /// # Errors
    ///
    /// [`RenderError::Atlas`] when the pages are refused, by the checks or
    /// by the device, and [`RenderError::Poll`] when the device fails
    /// waiting for the upload; the atlas and clips in use stay, and frames
    /// go on drawing with them.
    pub fn install_atlas(
        &mut self,
        pages: &AtlasPages<'_>,
        clips: ClipTable,
    ) -> Result<(), RenderError> {
        #[cfg(test)]
        let fault = self.atlas_fault.take();
        let device = self.gpu.device();
        let layers =
            skin_atlas::check_pages(pages, &device.limits()).map_err(RenderError::Atlas)?;
        // The whole candidate inside the error scopes: the texture, its
        // view and writes, its bind group, and the one submission of the
        // uploads; the wait after them submits nothing. Nothing in use
        // changes until all of it was accepted.
        let ((atlas, bind), refused) = skin_atlas::scoped(device, || {
            let atlas = AtlasTexture::upload(&self.gpu, pages, layers);
            let bind = self.bindings.bind(device, &atlas);
            #[cfg(test)]
            if let Some(fault) = fault {
                fault(&self.gpu);
            }
            self.gpu.queue().submit([]);
            (atlas, bind)
        });
        if let Some(e) = refused {
            return Err(RenderError::Atlas(UploadError::Device(e)));
        }
        wait_for_gpu(&self.gpu)?;
        self.atlas_bind = bind;
        self.staging.set_font_scale(atlas.font_scale());
        self.staging.set_clips(clips);
        self.atlas = atlas;
        self.resources += 1;
        Ok(())
    }

    /// The atlas array in use: its page side and its layers, the reserved
    /// one included.
    #[must_use]
    pub const fn atlas_size(&self) -> (u32, u32) {
        (self.atlas.side(), self.atlas.layers())
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

    /// A chart was chosen: the play skin takes what it needs of `chart`
    /// alone ([`PlaySkin::load_chart`]), with a practice's `section`, and
    /// nothing that depends on the play preset; a declarative skin drops
    /// its previous attempt. Frames are built again after a refused one
    /// ([`build`](Self::build)): the attempt refused is gone.
    ///
    /// # Errors
    ///
    /// Returns the skin's error when it cannot draw `chart`.
    pub fn load(&mut self, chart: &Chart, section: Option<&Section>) -> Result<(), RenderError> {
        self.skin
            .load_chart(chart, section)
            .map_err(RenderError::Skin)?;
        self.refused = None;
        self.project();
        finish_uploads(&self.gpu)
    }

    /// Prepares the play skin in use for the attempt `a`, in place, before
    /// its countdown: the simple skin's way, whose preparation replaces no
    /// resource. Its frames are built again after a refused one. The
    /// target in `a` is replaced by [`target`](Self::target).
    ///
    /// # Errors
    ///
    /// The skin's refusal of the attempt ([`RenderError::Skin`]).
    pub fn prepare_attempt(&mut self, a: &AttemptInput<'_>) -> Result<(), RenderError> {
        let a = AttemptInput {
            target: self.target(),
            ..*a
        };
        self.skin
            .prepare(&self.gpu, &a)
            .map_err(RenderError::Skin)?;
        self.refused = None;
        self.project();
        finish_uploads(&self.gpu)
    }

    /// Identifies this renderer's device among the process's renderers: a
    /// renderer built again after a failure has another.
    #[must_use]
    pub const fn device_generation(&self) -> u64 {
        self.id
    }

    /// The declarative bundle installed, if any.
    #[must_use]
    pub const fn installed(&self) -> Option<&Arc<SkinBundle>> {
        self.installed.as_ref()
    }

    /// How many times the atlas and the staging were replaced.
    #[must_use]
    pub const fn resource_generation(&self) -> u64 {
        self.resources
    }

    /// The sprites and runs a frame of the play skin holds, the
    /// overlay's reserve left out.
    #[must_use]
    pub fn capacity(&self) -> (usize, usize) {
        (self.staging.capacity(), self.staging.run_capacity())
    }

    /// The play skin's fixed clips in use.
    #[must_use]
    pub const fn clips(&self) -> &ClipTable {
        self.staging.clips()
    }

    /// The installation staged and not yet committed, if any.
    #[must_use]
    pub fn staged(&self) -> Option<PreparedToken> {
        self.pending.as_ref().map(|p| p.token)
    }

    /// Stages the installation of `bundle` with a fresh attempt `a` for
    /// `request`, before the countdown, leaving what is installed drawing
    /// until [`commit`](Self::commit): a previous staging is discarded;
    /// unless `bundle` is the one installed, its atlas is uploaded with the
    /// built-in atlas in layer 0, and a staging and an instance buffer are
    /// sized for its capacities, with its clips projected; then the fresh
    /// skin is prepared for the attempt, its prepared text attached. Every
    /// upload is finished before the token is returned. Allocates and
    /// waits for the GPU. The target in `a` is replaced by
    /// [`target`](Self::target).
    ///
    /// # Errors
    ///
    /// Whatever refused the staging, which is then discarded: the
    /// installation in use is untouched and goes on drawing.
    /// [`RenderError::is_refusal`] tells a refusal on a healthy device
    /// from a failure of the device.
    pub fn stage(
        &mut self,
        bundle: &Arc<SkinBundle>,
        a: &AttemptInput<'_>,
        request: u64,
    ) -> Result<PreparedToken, RenderError> {
        self.pending = None;
        let reuse = self
            .installed
            .as_ref()
            .is_some_and(|b| Arc::ptr_eq(b, bundle));
        let resources = if reuse {
            None
        } else {
            Some(self.resources_for(bundle)?)
        };
        let mut skin = DeclSkin::new(Arc::clone(bundle));
        let a = AttemptInput {
            target: self.target(),
            ..*a
        };
        skin.prepare(&self.gpu, &a).map_err(RenderError::Skin)?;
        let token = PreparedToken {
            request,
            device: self.id,
        };
        self.pending = Some(Pending {
            token,
            skin,
            resources,
        });
        Ok(token)
    }

    /// The resources of `bundle`: a staging sized for its capacities with
    /// its clips, then, inside one transaction of error scopes, its atlas
    /// uploaded with its bind group and an instance buffer for the staging,
    /// all finished on the GPU. A device refusing any of them (memory,
    /// validation) is answered as a refusal, never an uncaptured error.
    fn resources_for(&self, bundle: &Arc<SkinBundle>) -> Result<Resources, RenderError> {
        let device = self.gpu.device();
        let skin = DeclSkin::new(Arc::clone(bundle));
        let (capacity, runs) = sizes(&skin);
        let instances =
            usize::try_from(capacity.saturating_add(OVERLAY_SPRITES)).unwrap_or(usize::MAX);
        let max = device.limits().max_buffer_size;
        if instance_bytes(instances) > max {
            return Err(RenderError::Capacity {
                sprites: capacity,
                max,
            });
        }
        let mut staging = Staging::new(capacity, &skin.canvas(self.target()));
        staging.set_capacity(capacity, runs);
        staging.set_clips(bundle.clips().clone());
        let ((atlas, atlas_bind, buffer), refused) = bundle.with_pages(|pages| {
            let layers =
                skin_atlas::check_pages(pages, &device.limits()).map_err(RenderError::Atlas)?;
            Ok::<_, RenderError>(skin_atlas::scoped(device, || {
                let atlas = AtlasTexture::upload(&self.gpu, pages, layers);
                let bind = self.bindings.bind(device, &atlas);
                #[cfg(test)]
                if let Some(fault) = self.atlas_fault {
                    fault(&self.gpu);
                }
                let buffer = self.staged_buffer(staging.instance_capacity());
                self.gpu.queue().submit([]);
                (atlas, bind, buffer)
            }))
        })?;
        if let Some(e) = refused {
            return Err(RenderError::Atlas(UploadError::Device(e)));
        }
        staging.set_font_scale(atlas.font_scale());
        wait_for_gpu(&self.gpu)?;
        Ok(Resources {
            atlas,
            atlas_bind,
            staging,
            instances: buffer,
        })
    }

    /// A staging's instance buffer of `instances` sprites.
    fn staged_buffer(&self, instances: usize) -> wgpu::Buffer {
        #[cfg(test)]
        if self.buffer_fault {
            return self.gpu.device().create_buffer(&wgpu::BufferDescriptor {
                label: Some("refused"),
                size: 4,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::MAP_WRITE,
                mapped_at_creation: false,
            });
        }
        instance_buffer(self.gpu.device(), instances)
    }

    /// Installs the staging of `token` whole, between frames: its skin
    /// with its prepared attempt, and its atlas, bind group, clips,
    /// staging and instance buffer together when it has its own; the ones
    /// they replace are dropped. Frames are built again after a refused
    /// one.
    ///
    /// # Errors
    ///
    /// [`CommitError`] when `token` is not the staging's, by its renderer
    /// or its request: nothing changes, and the staging stays staged.
    pub fn commit(&mut self, token: PreparedToken) -> Result<(), CommitError> {
        if token.device != self.id {
            return Err(CommitError::Device);
        }
        let Some(staged) = self.pending.as_ref().map(|p| p.token) else {
            return Err(CommitError::NothingStaged);
        };
        if staged.request != token.request {
            return Err(CommitError::Request {
                staged: staged.request,
                asked: token.request,
            });
        }
        let Some(pending) = self.pending.take() else {
            return Err(CommitError::NothingStaged);
        };
        if let Some(r) = pending.resources {
            self.install(r);
        }
        self.installed = Some(Arc::clone(pending.skin.bundle()));
        self.skin = Box::new(pending.skin);
        self.refused = None;
        self.started = None;
        self.project();
        Ok(())
    }

    /// Replaces the atlas, its bind group, the staging and the instance
    /// buffer with `r`, together.
    fn install(&mut self, r: Resources) {
        self.atlas = r.atlas;
        self.atlas_bind = r.atlas_bind;
        self.staging = r.staging;
        self.instances = r.instances;
        self.resources += 1;
    }

    /// Drops the staged installation, if any: its resources go, and the
    /// installation in use stays.
    pub fn discard(&mut self) {
        self.pending = None;
    }

    /// Installs `bundle` with no attempt prepared, at once: a renderer
    /// built again after a device failure restores the committed bundle
    /// from memory this way, and the next attempt is prepared afresh
    /// through [`stage`](Self::stage), which reuses its resources.
    ///
    /// # Errors
    ///
    /// As [`stage`](Self::stage); the installation in use stays.
    pub fn restore(&mut self, bundle: &Arc<SkinBundle>) -> Result<(), RenderError> {
        self.pending = None;
        let r = self.resources_for(bundle)?;
        self.install(r);
        self.skin = Box::new(DeclSkin::new(Arc::clone(bundle)));
        self.installed = Some(Arc::clone(bundle));
        self.refused = None;
        self.started = None;
        self.project();
        Ok(())
    }

    /// The bytes held for the play skin now, the staged installation's
    /// included ([`MemoryWitness`]).
    #[must_use]
    pub fn memory(&self) -> MemoryWitness {
        let staged = self.pending.as_ref();
        let installed = self.installed.as_ref();
        let other = staged
            .map(|p| p.skin.bundle())
            .filter(|b| !installed.is_some_and(|i| Arc::ptr_eq(i, b)));
        let res = staged.and_then(|p| p.resources.as_ref());
        MemoryWitness {
            committed_cpu: installed.map_or(0, |b| b.resident_bytes()),
            staged_cpu: other.map_or(0, |b| b.resident_bytes()),
            atlas: self.atlas.bytes(),
            staged_atlas: res.map_or(0, |r| r.atlas.bytes()),
            upload: res.map_or(0, |r| r.atlas.upload_bytes()),
            batch: self.staging.bytes(),
            staged_batch: res.map_or(0, |r| r.staging.bytes()),
        }
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
    /// [`RenderError::Build`] when the skin refuses the frame, which is
    /// then not submitted; otherwise as [`submit`](Self::submit).
    pub fn frame(&mut self, f: &FrameInput<'_>) -> Result<FrameTiming, RenderError> {
        self.build(f).map_err(RenderError::Build)?;
        self.submit()
    }

    /// The app's share of a frame ([`Staging::build`]): the skin builds
    /// `f`, the overlay follows, and the batch is staged, with no call into
    /// wgpu. The frame's CPU time starts here; [`submit`](Self::submit)
    /// follows. The skin sees [`target`](Self::target) as `f.target`, the
    /// size the frame is projected onto, whatever the caller put there.
    ///
    /// # Errors
    ///
    /// The skin's refusal of this frame, or of an earlier frame since the
    /// last [`load`](Self::load): the skin is not called again for that
    /// attempt, nothing is staged, and the next [`submit`](Self::submit)
    /// submits nothing. The atlas, the clips and the device stay as they
    /// are, for the next attempt's [`load`](Self::load). Nothing allocates,
    /// the error included.
    pub fn build(&mut self, f: &FrameInput<'_>) -> Result<(), BuildError> {
        self.started = None;
        if let Some(e) = self.refused {
            return Err(e);
        }
        let start = Instant::now();
        let f = FrameInput {
            target: self.target(),
            ..*f
        };
        if let Err(e) = self.staging.build(self.skin.as_mut(), &f) {
            self.refused = Some(e);
            return Err(e);
        }
        self.started = Some(start);
        Ok(())
    }

    /// Whether the skin refused a frame of the attempt prepared last: no
    /// frame of it is built or submitted until the next
    /// [`load`](Self::load).
    #[must_use]
    pub const fn refused(&self) -> Option<BuildError> {
        self.refused
    }

    /// Submits and presents the frame [`build`](Self::build) staged, once.
    /// Without a frame built since the last submit, a refused one
    /// included, nothing is acquired or submitted, and the frame is not
    /// presented (`presented` is `false`, no sprite drawn).
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
        let Some(start) = self.started.take() else {
            return Ok(FrameTiming {
                cpu: Duration::ZERO,
                sprites: 0,
                dropped: 0,
                presented: false,
                present_count: None,
            });
        };
        let projection = self.staging.projection();
        self.draw(start, None, projection)
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
        self.staging.fill(|b| text_screen(lines, b));
        self.draw(start, None, Projection::fixed(self.target()))
    }

    /// Draws the result screen: `lines` in the bitmap font and, with
    /// `graphs`, the gauge graph and the deviation histogram under them
    /// ([`result_screen`]), as [`frame`](Self::frame) draws.
    ///
    /// # Errors
    ///
    /// As [`frame`](Self::frame).
    pub fn result_frame(
        &mut self,
        lines: &[String],
        graphs: Option<&game::ResultGraphs>,
    ) -> Result<FrameTiming, RenderError> {
        let start = Instant::now();
        self.staging.fill(|b| result_screen(lines, graphs, b));
        self.draw(start, None, Projection::fixed(self.target()))
    }

    /// Draws the play screen while a chart loads before the countdown, as
    /// [`frame`](Self::frame) draws: the skin's loading frame
    /// ([`PlaySkin::build_loading`]), then in a pass of its own the text
    /// last prepared on `text`, the load's lines. Before recording opens
    /// only: no play frame draws text this way.
    ///
    /// # Errors
    ///
    /// As [`menu_frame`](Self::menu_frame).
    pub fn loading_frame(&mut self, text: &MenuText) -> Result<FrameTiming, RenderError> {
        if text.owner() != self.id {
            return Err(RenderError::OtherRenderersText);
        }
        let start = Instant::now();
        let skin = &mut self.skin;
        self.staging.fill(|b| skin.build_loading(b));
        let projection = self.staging.projection();
        self.draw(start, Some(text), projection)
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
        self.staging.fill(sprites);
        self.draw(start, Some(text), Projection::fixed(self.target()))
    }

    /// Submits and presents the staged batch through `projection`, each
    /// run under its scissor, then `text` in its own pass when there is
    /// one; `start` is when the frame's work began. While the window is
    /// minimized nothing is acquired, drawn or presented.
    fn draw(
        &mut self,
        start: Instant,
        text: Option<&MenuText>,
        projection: Projection,
    ) -> Result<FrameTiming, RenderError> {
        // Whatever is drawn now, a play frame built before it is not
        // submitted again.
        self.started = None;
        let sprites = self.staging.instances().len();
        let staged = u32::try_from(sprites).unwrap_or(u32::MAX);
        let timing = |cpu, presented| FrameTiming {
            cpu,
            sprites,
            dropped: self.staging.batch().dropped(),
            presented,
            present_count: None,
        };
        if self.minimized {
            return Ok(timing(start.elapsed(), false));
        }
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
            Output::Offscreen { view, .. } => {
                // Nothing paces offscreen frames: the frame before must be
                // done on the GPU first. One not done within the wait stays
                // pending, and this frame is skipped, not presented, as a
                // window's frame is when its swap chain image times out.
                let device = self.gpu.device();
                let admitted = self.pacer.admit(|previous| {
                    !matches!(
                        device.poll(wgpu::PollType::Wait {
                            submission_index: Some(previous.clone()),
                            timeout: Some(OFFSCREEN_FRAME_WAIT),
                        }),
                        Err(wgpu::PollError::Timeout)
                    )
                });
                if !admitted {
                    let cpu = start.elapsed().saturating_sub(acquire.elapsed());
                    return Ok(timing(cpu, false));
                }
                (None, view.clone())
            }
        };
        let acquired = acquire.elapsed();
        let queue = self.gpu.queue();
        queue.write_buffer(
            &self.instances,
            0,
            bytemuck::cast_slice(self.staging.instances()),
        );
        // Only when it changes: on the first frame, a resize, or between
        // the menus' fixed canvas and the play skin's.
        let half = projection.half();
        if self.bindings.uploaded != Some(half) {
            let [hx, hy] = half;
            queue.write_buffer(
                &self.bindings.projection,
                0,
                bytemuck::cast_slice(&[hx, hy, 0.0, 0.0]),
            );
            self.bindings.uploaded = Some(half);
        }
        let [x, y, w, h] = projection.viewport();
        let full = [0, 0, self.size.0, self.size.1];
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
                        load: wgpu::LoadOp::Clear(self.clear),
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
            // The runs in the batch's order, so each sprite is drawn over
            // every sprite pushed before it, whatever their blends. Each run
            // sets its scissor when it differs from the one set: an
            // unclipped run sets the whole target back, and a run whose clip
            // keeps no pixel is not drawn.
            let mut scissor = full;
            for run in self.staging.batch().runs() {
                let end = run.end.min(staged);
                if run.start >= end {
                    break;
                }
                let rect = match self.staging.scissor(run.key.clip) {
                    Scissor::Full => full,
                    Scissor::Rect(rect) => rect,
                    Scissor::Empty => continue,
                };
                if rect != scissor {
                    let [x, y, w, h] = rect;
                    pass.set_scissor_rect(x, y, w, h);
                    scissor = rect;
                }
                pass.set_pipeline(match run.key.blend {
                    Blend::Alpha => &self.alpha,
                    Blend::Add => &self.additive,
                });
                pass.draw(0..4, run.start..end);
            }
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
        let submitted = queue.submit([encoder.finish()]);
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
                // Offscreen: this frame is the one left pending, so a later
                // wait for the GPU (a chart's preparation, a read back)
                // waits for about one frame, as behind a swap chain.
                self.pacer.submitted(submitted);
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

/// The longest an offscreen frame waits for the previous frame's GPU work;
/// past it the frame is skipped, and the next frame waits again, so the
/// render thread still takes its messages behind a slow GPU.
const OFFSCREEN_FRAME_WAIT: Duration = Duration::from_millis(100);

/// Submits every upload queued on `gpu` and waits until the GPU has done them,
/// so none is left for the first frame of play (design.md "Display Latency and
/// Stutter").
fn finish_uploads(gpu: &Gpu) -> Result<(), RenderError> {
    gpu.queue().submit([]);
    wait_for_gpu(gpu)
}

/// Waits until the GPU has done every submission, submitting nothing.
fn wait_for_gpu(gpu: &Gpu) -> Result<(), RenderError> {
    gpu.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .map(|_| ())
        .map_err(RenderError::Poll)
}

/// The viewport `[x, y, width, height]` that fits the 1920×1080 layout
/// into a `width` × `height` target at the largest scale, centered, with
/// black bars on the other axis: the fixed canvas's
/// ([`Projection::fixed`]).
#[must_use]
pub fn letterbox(width: u32, height: u32) -> [f32; 4] {
    let (w, h) = (width as f32, height as f32);
    let s = (w / WIDTH).min(h / HEIGHT);
    let (vw, vh) = (WIDTH * s, HEIGHT * s);
    [(w - vw) / 2.0, (h - vh) / 2.0, vw, vh]
}

/// An offscreen target of `size` and its view.
fn offscreen(
    device: &wgpu::Device,
    size: (u32, u32),
    format: wgpu::TextureFormat,
) -> (wgpu::Texture, wgpu::TextureView) {
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
    (texture, view)
}

/// The sprites and runs the batch holds for `skin`, the overlay's reserve
/// left out: the skin's own, and never fewer than the built-in screens'
/// [`MAX_SPRITES`], which share the batch.
fn sizes(skin: &dyn PlaySkin) -> (u32, u32) {
    let least = MAX_SPRITES as u32;
    (skin.capacity().max(least), skin.run_capacity().max(least))
}

/// The bytes of `instances` instances.
fn instance_bytes(instances: usize) -> u64 {
    u64::try_from(instances)
        .unwrap_or(u64::MAX)
        .saturating_mul(size_of::<Instance>() as u64)
}

/// The instance buffer of `instances` sprites.
fn instance_buffer(device: &wgpu::Device, instances: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("sprites"),
        size: instance_bytes(instances),
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

const fn extent(width: u32, height: u32) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    }
}

/// The alpha and additive sprite pipelines and their bindings. Both take
/// premultiplied colour (design.md "Rendering and Skins"): alpha blending
/// is RGB source 1, destination 1 − source alpha; additive is RGB source
/// 1, destination 1; in both the alpha channel composites over.
fn pipelines(
    gpu: &Gpu,
    format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::RenderPipeline, Bindings) {
    let device = gpu.device();
    let smooth = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("smooth"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        ..wgpu::SamplerDescriptor::default()
    });
    // Nearest within and between levels: the default.
    let nearest = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("nearest"),
        ..wgpu::SamplerDescriptor::default()
    });
    let projection = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("projection"),
        size: PROJECTION_BYTES,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let sampler = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("atlas"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            },
            sampler(1),
            sampler(2),
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(PROJECTION_BYTES),
                },
                count: None,
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
        // `Instance`: rect, uv and color, pivot and angle together, then
        // the flags.
        let attrs = wgpu::vertex_attr_array![
            0 => Float32x4,
            1 => Float32x4,
            2 => Float32x4,
            3 => Float32x3,
            4 => Uint32,
        ];
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sprites"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<Instance>() as u64,
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
    // Both components over (source 1, destination 1 − source alpha).
    let alpha = pipeline(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
    // Colour source 1, destination 1; the alpha channel still over, not
    // `ADDITIVE`'s 1, 1.
    let additive = pipeline(wgpu::BlendState {
        alpha: wgpu::BlendComponent::OVER,
        ..wgpu::BlendState::ADDITIVE
    });
    let bindings = Bindings {
        layout,
        smooth,
        nearest,
        projection,
        uploaded: None,
    };
    (alpha, additive, bindings)
}

/// The projection's uniform: half the canvas's size, padded to 16 bytes.
const PROJECTION_BYTES: u64 = 16;

#[cfg(test)]
mod tests {
    use std::error::Error;

    use wgpu::wgc::instance as core;

    use super::{AtlasPages, ClipTable, RenderError, Renderer, Target, letterbox};
    use crate::text::MenuText;
    use crate::{AdapterChoice, Blend, SkinError, Sprite, UploadError, solid_uv};

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

        let e = RenderError::Atlas(UploadError::Side {
            side: 100,
            max: 8_192,
        });
        let source = the_cause::<UploadError>(&e, "atlas");
        assert!(matches!(source, UploadError::Side { side: 100, .. }));

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
        let e = RenderError::Capacity {
            sprites: 9_000,
            max: 256,
        };
        assert!(e.source().is_none());
        assert_eq!(
            e.to_string(),
            "capacity: 9000 sprites, past the device's 256 bytes"
        );
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

    /// Both blends composite the alpha channel over: source 1, destination
    /// 1 − source alpha. On a transparent target, white at alpha 0.5 is
    /// drawn, then white at alpha 0.25 over it, blended alpha on the left
    /// and additive on the right: 0.25 + 0.5 × 0.75 = 0.625, 159.4 (or
    /// 160.0 when the 0.5 below was stored as 128), so 159 within 2 on
    /// both sides. An additive alpha channel of 1, 1 (`ADDITIVE`'s) would
    /// give 0.75, 191, on the right; one blended like the colour of
    /// straight alpha (source alpha, 1 − source alpha) would give 0.25 for
    /// the first sprite and 0.0625 + 0.1875 = 0.25, 64, on the left.
    #[test]
    fn both_blends_composite_alpha_over() {
        let mut r = Renderer::new(
            Target::Offscreen {
                width: 960,
                height: 540,
            },
            AdapterChoice::Warp,
        )
        .expect("WARP renderer");
        r.clear = wgpu::Color::TRANSPARENT;
        let solid = |rect, alpha| Sprite {
            rect,
            uv: solid_uv(),
            color: [1.0, 1.0, 1.0, alpha],
            ..Sprite::default()
        };
        let text = MenuText::new(&r);
        let timing = r
            .menu_frame(
                |b| {
                    b.push(Blend::Alpha, solid([0.0, 0.0, 400.0, 400.0], 0.5));
                    b.push(Blend::Alpha, solid([0.0, 0.0, 200.0, 400.0], 0.25));
                    b.push(Blend::Add, solid([200.0, 0.0, 200.0, 400.0], 0.25));
                },
                &text,
            )
            .unwrap();
        assert!(timing.presented);
        let pixels = r.read_back();
        // Layout (100, 200) and (300, 200): target pixels (50, 100) and
        // (150, 100) at half scale.
        let alpha = |x: usize| pixels[(100 * 960 + x) * 4 + 3];
        assert!(alpha(50).abs_diff(159) <= 2, "alpha blend: {}", alpha(50));
        assert!(alpha(150).abs_diff(159) <= 2, "additive: {}", alpha(150));
    }

    /// A staging whose instance buffer the device refuses, past its atlas's
    /// upload, is a refusal on a healthy device, typed, never an uncaptured
    /// error that would panic: nothing is staged, and the installation in
    /// use draws the next frame, red on the left half.
    #[test]
    fn a_refused_instance_buffer_keeps_the_installation() {
        use crate::test_common::{GAUGES, attempt, context, input, offscreen, view};
        use crate::test_decl::{Look, bundle};
        use bms_model::TimeUs;

        let chart = bms_model::ChartBuilder::new(120_000).build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut r = offscreen(1_920, 1_080);
        let (a, _) = bundle(&r, dir.path(), &Look::left([255, 0, 0, 255]));
        let (b, _) = bundle(&r, dir.path(), &Look::left([0, 0, 255, 255]));
        let ctx = context(&GAUGES, false);
        let token = r.stage(&a, &attempt(&r, &chart, &ctx), 1).unwrap();
        r.commit(token).unwrap();
        let generation = r.resource_generation();

        r.buffer_fault = true;
        let e = r.stage(&b, &attempt(&r, &chart, &ctx), 2).unwrap_err();
        r.buffer_fault = false;
        assert!(
            matches!(e, RenderError::Atlas(UploadError::Device(_))),
            "{e:?}"
        );
        assert!(e.is_refusal());
        assert!(r.staged().is_none());
        assert_eq!(r.resource_generation(), generation);
        let v = view(0, 0);
        let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
        assert_eq!(timing.sprites, 1);
        let p = r.read_back();
        let at = |x: usize| [p[(540 * 1_920 + x) * 4], p[(540 * 1_920 + x) * 4 + 2]];
        assert_eq!((at(480), at(1_440)), ([255, 0], [0, 0]));
    }

    /// A refusal after the texture exists, injected between the candidate's
    /// bind group and the upload's submission (a buffer whose usage mixes
    /// both map modes, which every device's validation refuses), is the
    /// typed `Atlas(Device)` error, not an uncaptured one that would panic,
    /// and the built-in atlas stays: 128 texels, one layer, its font scale
    /// 1, so the solid block still draws white at layout (100, 100), target
    /// pixel (50, 50). With the refused 256-texel array's font scale (1/2)
    /// taken anyway, the solid uv would read texel 62 of the built-in atlas,
    /// a glyph cell's empty corner. The same pages without the fault are
    /// then installed: 256 texels, two layers.
    #[test]
    fn a_refusal_after_the_texture_keeps_the_atlas() {
        let mut r = Renderer::new(
            Target::Offscreen {
                width: 960,
                height: 540,
            },
            AdapterChoice::Warp,
        )
        .expect("WARP renderer");
        let (l0, l1, l2, l3) = (
            vec![0u8; 256 * 256 * 4],
            vec![0u8; 128 * 128 * 4],
            vec![0u8; 64 * 64 * 4],
            vec![0u8; 32 * 32 * 4],
        );
        let page: crate::PageLevels<'_> = [&l0, &l1, &l2, &l3];
        let pages = AtlasPages {
            side: 256,
            pages: &[page],
        };
        r.atlas_fault = Some(|gpu| {
            let _ = gpu.device().create_buffer(&wgpu::BufferDescriptor {
                label: Some("refused"),
                size: 4,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::MAP_WRITE,
                mapped_at_creation: false,
            });
        });
        let e = r.install_atlas(&pages, ClipTable::default()).unwrap_err();
        assert!(
            matches!(e, RenderError::Atlas(UploadError::Device(_))),
            "{e:?}"
        );
        assert_eq!(r.atlas_size(), (128, 1));
        let text = MenuText::new(&r);
        let timing = r
            .menu_frame(
                |b| {
                    b.push(
                        Blend::Alpha,
                        Sprite {
                            rect: [0.0, 0.0, 400.0, 400.0],
                            uv: solid_uv(),
                            color: [1.0; 4],
                            ..Sprite::default()
                        },
                    );
                },
                &text,
            )
            .unwrap();
        assert!(timing.presented);
        let pixels = r.read_back();
        let i = (50 * 960 + 50) * 4;
        assert_eq!(&pixels[i..i + 3], &[255, 255, 255]);
        r.install_atlas(&pages, ClipTable::default()).unwrap();
        assert_eq!(r.atlas_size(), (256, 2));
    }
}
