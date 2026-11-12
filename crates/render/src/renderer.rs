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
use skin_model::{
    AtlasId, AtlasLimits, BitmapLines, BuildError, ChartText, Layout, PackedPages, ResultFrame,
    ResultScreen, ScreenKind, Selection, SelectionFrame, TextSlots, TextTable, TextTableError,
};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Dxgi::{DXGI_FRAME_STATISTICS, IDXGISwapChain3};
use windows::Win32::System::Threading::WaitForSingleObject;

use crate::decl::{DeclSkin, Installation, SkinBundle, page_levels, pages_bytes, pages_texels};
use crate::gpu::{self, AdapterChoice, Gpu};
use crate::measure::{FrameStatistics, Presentation};
use crate::pace::Pacer;
use crate::projection::{ClipTable, Projection};
use crate::simple::layout::{HEIGHT, WIDTH};
use crate::simple::{result_screen, text_screen};
use crate::skin_atlas::{self, AtlasPages, AtlasTexture, UploadError};
use crate::staging::{Scissor, Staging};
use crate::text::MenuText;
use crate::text_cache::{Holder, TextCache};
use crate::text_upload::{self, TextReport, TextScratch};
use crate::{
    AttemptInput, Blend, FrameInput, Instance, LoadingFrame, OVERLAY_SPRITES, PlaySkin, SimpleSkin,
    SkinError, Sprite, SpriteBatch,
};

/// The most sprites the selection screen's strip draws over its skin (a
/// band under its search line, one under its notice, and the
/// composition's underline), from the reserve the skin cannot fill
/// ([`Renderer::build_selection`]).
pub const STRIP_SPRITES: usize = 3;

/// The most sprites the result screen's strip draws over its skin, one
/// band per fact that applies, from the same reserve
/// ([`Renderer::build_result`]).
pub const RESULT_STRIP_SPRITES: usize = 13;

const _: () = assert!(RESULT_STRIP_SPRITES <= OVERLAY_SPRITES as usize);

/// `rect` in layout pixels (1920×1080, fitted to the whole `target` as
/// the menus' text is) in the canvas pixels of `skin`'s projection.
fn layout_to_canvas([x, y, w, h]: [f32; 4], skin: &Projection, target: [u32; 2]) -> [f32; 4] {
    let [fx, fy, fw, _] = Projection::fixed(target).viewport();
    let fs = fw / 1_920.0;
    let [sx, sy, sw, _] = skin.viewport();
    let [hw, _] = skin.half();
    let ss = (sw / (2.0 * hw)).max(f32::MIN_POSITIVE);
    [
        (fx + x * fs - sx) / ss,
        (fy + y * fs - sy) / ss,
        w * fs / ss,
        h * fs / ss,
    ]
}

/// The first `N` of `strip`, in layout pixels, placed on the canvas of
/// `projection` for `target`, and how many there are. Allocates nothing.
fn placed_strip<const N: usize>(
    strip: &[Sprite],
    projection: &Projection,
    target: [u32; 2],
) -> ([Sprite; N], usize) {
    let mut placed = [Sprite::default(); N];
    let mut n = 0;
    for (out, s) in placed.iter_mut().zip(strip) {
        *out = Sprite {
            rect: layout_to_canvas(s.rect, projection, target),
            ..*s
        };
        n += 1;
    }
    (placed, n)
}

/// The next [`Renderer`]'s id.
static RENDERERS: AtomicU64 = AtomicU64::new(0);

/// Sprites one frame of the built-in screens can draw, across both
/// blends: the least the renderer's batch holds ([`PlaySkin::capacity`]).
pub const MAX_SPRITES: usize = 8_192;

/// The level-0 texels every screen kind's committed installation shares
/// with a candidate ([`Renderer::admit`]): 832 Mi, sized so that the
/// largest skins of each kind fit together with their VM charges, a play
/// skin's chart layer included, and at least twice one atlas array's cap
/// ([`skin_model::MAX_ATLAS_TEXELS`]), so any skin that fits alone is
/// admitted on its own and two at that cap fit together. A provisional
/// limit, raised when a real case hits it.
pub const SHARED_TEXELS: u64 = 832 << 20;

// Two kinds' installations at the per-atlas cap must fit in the shared
// budget at once: a candidate at the cap beside another kind committed at
// it. (A replacement leaves its own kind's charge out, so this is not
// about the replaced installation.)
const _: () = assert!(SHARED_TEXELS >= 2 * skin_model::MAX_ATLAS_TEXELS);

/// What an installation keeping a Lua VM is charged beside its atlas, in
/// level-0 texels: a VM's memory cap (64 MiB) at the 4 bytes of an RGBA8
/// texel. A fixed charge, not a measurement.
pub const VM_TEXELS: u64 = 16 << 20;

/// The screen kinds in their order: the order of
/// [`Admission::committed`] and of a recovery's restores.
const KINDS: [ScreenKind; 4] = [
    ScreenKind::Play,
    ScreenKind::Selection,
    ScreenKind::Decision,
    ScreenKind::Result,
];

/// What `bundle` is charged against [`SHARED_TEXELS`] while committed:
/// its atlas array's level-0 texels, and [`VM_TEXELS`] when it keeps a
/// VM.
fn charge_of(bundle: &SkinBundle) -> u64 {
    let vm = if bundle.keeps_vm() { VM_TEXELS } else { 0 };
    bundle.atlas_texels().saturating_add(vm)
}

// The sprite shader reads the built-in atlas at its side, 128.
const _: () = assert!(crate::atlas::ATLAS_SIZE == 128);

const SPRITE_WGSL: &str = r"
// An `Instance`: its pivot and angle share location 3, and its flags hold
// the atlas layer (low 16 bits) and the nearest and base filter bits.
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
// the nearest bit, or at the nearest texel of level 0 with the base bit;
// layer 0, the built-in atlas of 128 texels at its top left, is read at its
// nearest level-0 texel by integer texel: its uv, the built-in atlas's own,
// times 128, exact whatever the array's side. The two implicit-derivative
// samples are always taken, so they stay in uniform control flow; the
// base read has an explicit level, which needs no derivatives, so only a
// sprite with the base bit takes it. One result is chosen.
fn tinted(v: VOut) -> vec4f {
    let layer = i32(v.flags & 0xFFFFu);
    let smoothed = textureSample(atlas, smooth_samp, v.uv, layer);
    let nearest = textureSample(atlas, nearest_samp, v.uv, layer);
    let texel = clamp(vec2i(floor(v.uv * 128.0)), vec2i(0), vec2i(127));
    let builtin = textureLoad(atlas, texel, 0, 0);
    var page = select(smoothed, nearest, (v.flags & 0x10000u) != 0u);
    if (v.flags & 0x20000u) != 0u {
        page = textureSampleLevel(atlas, nearest_samp, v.uv, layer, 0.0);
    }
    return select(page, builtin, layer == 0) * v.color;
}

@fragment
fn fs(v: VOut) -> @location(0) vec4f {
    return tinted(v);
}

// The multiply blend takes the straight colour, texel times tint: the
// premultiplied product divided by its alpha, black where that is 0 (a
// transparent texel's colour is gone; the batch holds no quad of tint
// alpha 0).
@fragment
fn fs_multiply(v: VOut) -> @location(0) vec4f {
    let c = tinted(v);
    return vec4f(select(vec3f(0.0), c.rgb / c.a, c.a > 0.0), c.a);
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

/// What a loading text table is checked against as it installs
/// ([`Renderer::install_loading_text`]): the installation it was prepared
/// for, by its loading presentation, and the renderer and resources (the
/// atlas) its lines were laid out on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadingTextKey {
    installation: u64,
    device: u64,
    resources: u64,
}

/// The chosen chart's loading text prepared for one loading lifetime and
/// text revision on the installation its key names, waiting for the next
/// frame's start to install it.
struct StagedText {
    key: LoadingTextKey,
    lifetime: u64,
    revision: u64,
    which: TextSlots,
    /// The producer's lines; `None` clears the rasterized slots.
    table: Option<TextTable>,
    /// The bitmap-font slots' lines, laid out by the skin.
    bitmap: BitmapLines,
    /// The text layer's lines its table draws, held until it is installed
    /// or dropped.
    holder: Holder,
}

impl StagedText {
    /// The bytes it holds: itself, its rasterized lines with their
    /// holder's list, and its bitmap lines with their glyphs.
    fn bytes(&self) -> u64 {
        let lines = self
            .table
            .as_ref()
            .map_or(0, |t| t.len() * size_of::<Option<skin_model::TextImage>>());
        let held = self.holder.bytes();
        u64::try_from(size_of::<Self>() + lines + held + self.bitmap.heap_bytes())
            .unwrap_or(u64::MAX)
    }
}

/// Why loading text was not installed: nothing changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoadingTextError {
    /// It was prepared for another installation, or the installation's
    /// renderer or resources were replaced since.
    #[error("the loading text belongs to another installation or its replaced resources")]
    Stale,
    /// It was prepared for another loading lifetime than the one the
    /// presentation is in.
    #[error("the loading text belongs to another loading lifetime")]
    Lifetime,
    /// A newer revision of the chosen chart's text is installed.
    #[error("the loading text's revision {revision} is older than the installed {installed}")]
    Older {
        /// The revision installed.
        installed: u64,
        /// The revision refused.
        revision: u64,
    },
    /// The table does not fit the skin's text slots.
    #[error(transparent)]
    Table(#[from] TextTableError),
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
    /// A hardware adapter was asked for, and the one given renders on the
    /// CPU (a development run's admission).
    #[error("adapter: hardware was asked for, and the adapter is a CPU adapter: {name}")]
    CpuAdapter {
        /// The adapter given.
        name: String,
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
    /// The device was lost or destroyed while chart images were written:
    /// they stay unavailable, and the screen's recovery follows.
    #[error("device lost")]
    DeviceLost,
    /// No committed play installation keeps a chart layer: no chart image
    /// was published.
    #[error("chart images: no play installation keeps a chart layer")]
    NoChartLayer,
    /// A chart image does not fit its slot of the chart layer, or lacks a
    /// mip level: the images are absent for the installation.
    #[error("chart images: the {which:?} image does not fit its slot")]
    ChartImage {
        /// The image.
        which: skin_model::chart_image::ChartImage,
    },
    /// The play installation, its pages or the device changed while the
    /// chart images were written: they stay unavailable.
    #[error("chart images: the installation changed during the write")]
    ChartChanged,
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
    /// An installation was asked for a request older than one already
    /// staged or committed: the newer stays, and the older is dropped.
    #[error("the installation of request {request} is superseded by request {newer}")]
    Superseded {
        /// The request refused.
        request: u64,
        /// The newer request staged or committed.
        newer: u64,
    },
    /// A skin of `kind` with every other kind's committed skin would
    /// pass the texels the screens share ([`Renderer::admit`]): nothing
    /// was allocated, and every installation in use stays.
    #[error("the {kind} skin needs {needed} texels with the other screens' skins, past {limit}")]
    Admission {
        /// The kind the skin was asked for.
        kind: ScreenKind,
        /// The other kinds' committed charges and the skin's.
        needed: u64,
        /// [`SHARED_TEXELS`].
        limit: u64,
    },
    /// The result kind is disabled for the run
    /// ([`Renderer::disable_result`]): nothing of it is installed, staged,
    /// committed or restored.
    #[error("the result skin is disabled for this run")]
    ResultDisabled,
    /// A pages-only installation asked for a kind other than the result's,
    /// the only kind that has one ([`Renderer::install_pages`]).
    #[error("the {kind} screen has no pages-only installation")]
    PagesKind {
        /// The kind asked for.
        kind: ScreenKind,
    },
}

impl RenderError {
    /// Whether this is a refusal of what was asked while the device stays
    /// healthy (design.md "Play Lifecycle", preparing each attempt): the
    /// skin cannot draw the attempt, its atlas is past the checks or was
    /// refused by the device for memory or validation, its sprites do
    /// not fit a buffer, or it is past the texels the screens share. The
    /// renderer is unchanged and goes on drawing; an attempt is refused,
    /// not the screen. Anything else, the device or the surface failing,
    /// drops the screen.
    #[must_use]
    pub const fn is_refusal(&self) -> bool {
        match self {
            Self::Skin(_)
            | Self::Capacity { .. }
            | Self::Superseded { .. }
            | Self::Admission { .. }
            | Self::ResultDisabled
            | Self::PagesKind { .. }
            | Self::ChartImage { .. } => true,
            Self::Atlas(UploadError::Device(e)) => matches!(
                e,
                wgpu::Error::OutOfMemory { .. } | wgpu::Error::Validation { .. }
            ),
            Self::Atlas(_) => true,
            _ => false,
        }
    }
}

/// Where chart images are published ([`Renderer::publish_chart_images`]):
/// the committed play installation's array, or a staged one's own array,
/// named by its token (S10-12 design §5.3, a pending play skin's
/// specialization).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartTarget {
    /// The committed play installation.
    Committed,
    /// The staged play installation `token` names, with its own array.
    Staged(PreparedToken),
}

/// What a chart publication's failure left
/// ([`RenderError::chart_outcome`]), which the player's wait before an
/// attempt's commit reads (S10-12 design §5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartOutcome {
    /// An all-absent table was published: the images are absent for the
    /// target, their fields read false, and the presentation without them
    /// shows. Terminal.
    Absent,
    /// Nothing was published: the images stay unavailable until a
    /// publication succeeds (a device lost, a failing wait, a target
    /// changed). Not terminal.
    Unavailable,
    /// The target keeps no chart layer, or is not there: nothing changed.
    NoTarget,
}

impl RenderError {
    /// What a [`Renderer::publish_chart_images`] failure left, by its
    /// variant rather than [`is_refusal`](Self::is_refusal): a slot misfit
    /// and every refused write published an all-absent table, whatever
    /// the device's error; any other failure published nothing.
    #[must_use]
    pub const fn chart_outcome(&self) -> ChartOutcome {
        match self {
            Self::ChartImage { .. } | Self::Atlas(_) => ChartOutcome::Absent,
            Self::NoChartLayer => ChartOutcome::NoTarget,
            _ => ChartOutcome::Unavailable,
        }
    }
}

/// An installation staged by [`Renderer::stage`] or
/// [`Renderer::install`] and not yet committed: the request it answers,
/// the renderer, so its device, it was staged on, and the screen kind it
/// installs for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedToken {
    /// The preparation request it answers.
    pub request: u64,
    /// [`Renderer::device_generation`] of the renderer that staged it.
    pub device: u64,
    /// The screen kind it installs for.
    pub kind: ScreenKind,
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
    /// The staged installation is for another screen kind.
    #[error("the staged installation is for the {staged} screen, not the {asked} screen")]
    Kind {
        /// The kind staged.
        staged: ScreenKind,
        /// The kind committed.
        asked: ScreenKind,
    },
    /// The token was staged on another renderer, whose device this one
    /// replaced.
    #[error("the installation was staged on another device")]
    Device,
    /// The token's request is not newer than the one committed last.
    #[error("request {asked} is not newer than request {committed}, committed")]
    Superseded {
        /// The request committed last.
        committed: u64,
        /// The request committed.
        asked: u64,
    },
    /// The token installs the result kind, disabled for the run
    /// ([`Renderer::disable_result`]).
    #[error("the result skin is disabled for this run")]
    ResultDisabled,
}

/// Chart images published into a play installation's chart layer, and
/// the array they describe: its installation's atlas and the renderer's
/// resource generation when they were written (S10-12 design §5.3).
#[derive(Clone, Copy, Debug)]
struct PublishedChart {
    key: skin_model::chart_image::ChartKey,
    table: skin_model::chart_image::ChartTable,
}

/// The GPU and CPU resources a declarative skin's installation replaces
/// together: its atlas array and bind group, and the staging and instance
/// buffer sized for its capacities, with its clips.
struct Resources {
    atlas: AtlasTexture,
    atlas_bind: wgpu::BindGroup,
    staging: Staging,
    instances: wgpu::Buffer,
    /// The text layer's cache: empty with the atlas.
    text_cache: TextCache,
}

/// The result kind's pages-only installation ([`Renderer::install_pages`]):
/// its prepared atlas uploaded, with no layout drawing it yet; a layout
/// staged on the same pages takes its array.
struct PagesRes {
    pages: Arc<PackedPages>,
    atlas: AtlasTexture,
    atlas_bind: wgpu::BindGroup,
    /// The text layer's cache: empty with the atlas.
    text_cache: TextCache,
}

/// What a commit replaced, kept with its device resources until the next
/// staging or [`Renderer::drain_retired`] drops it, so no commit drops any
/// of the device's: a layout replaced on the same array (its staging and
/// instance buffer), or an installation or a pages-only installation
/// replaced on another array.
#[expect(
    clippy::large_enum_variant,
    reason = "held once and moved only by a commit, which boxing would make allocate"
)]
enum Retired {
    Layout {
        staging: Staging,
        /// Held only so that it is dropped outside the commit.
        _instances: wgpu::Buffer,
    },
    Slot(Slot),
    Pages(PagesRes),
}

impl Retired {
    /// The array it holds, if it holds one.
    fn atlas_id(&self) -> Option<AtlasId> {
        match self {
            Self::Layout { .. } => None,
            Self::Slot(s) => Some(s.installation.bundle().atlas_id()),
            Self::Pages(p) => Some(p.pages.id()),
        }
    }
}

/// What a staging holds besides its skin.
enum PendingRes {
    /// Nothing of its own: it is its kind's committed bundle, whose
    /// resources it keeps.
    Installed,
    /// A fresh layout on its kind's committed atlas array, whose pages it
    /// shares: its staging and instance buffer, with nothing uploaded.
    Layout {
        staging: Box<Staging>,
        instances: wgpu::Buffer,
    },
    /// Its own atlas array and layout.
    Own(Box<Resources>),
    /// A pages-only installation's atlas array; `None` when its pages are
    /// the ones installed.
    Pages(Option<Box<PagesRes>>),
}

/// A screen kind's committed installation other than play's: its
/// installation and its own resources; the selection screen's frames
/// draw its ([`Renderer::build_selection`]).
struct Slot {
    installation: Installation,
    resources: Resources,
    /// The lines the screen's text tables draw, in its resources' cache.
    text: Holder,
}

/// An installation staged and not yet committed, of the kind its token
/// names: the fresh skin with its prepared attempt, or none for a
/// chart-independent installation ([`Renderer::install`]; a kind other
/// than play's is never prepared, its skin only carrying its
/// installation), sharing the installation's loading presentation, and
/// its own resources, or `None` when it reuses its kind's committed
/// bundle's.
struct Pending {
    token: PreparedToken,
    /// Its skin; `None` for a pages-only installation.
    skin: Option<DeclSkin>,
    /// A pages-only installation's pages.
    pages: Option<Arc<PackedPages>>,
    res: PendingRes,
    /// The lines its attempt's table draws, in its resources' cache or the
    /// installed one.
    text: Holder,
    /// The chart images published into its own array
    /// ([`ChartTarget::Staged`]), of no array yet: its commit keys them
    /// with the generation it makes, so they are valid from the commit
    /// on, and a discard drops them with it.
    chart: Option<skin_model::chart_image::ChartTable>,
}

/// The level-0 texels charged against [`SHARED_TEXELS`] now
/// ([`Renderer::admission`]): what [`Renderer::admit`] sums.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Admission {
    /// Each kind's committed installation, in [`ScreenKind`] order (play,
    /// selection, decision, result); 0 for none, and for play's built-in
    /// skin.
    pub committed: [u64; 4],
    /// The staged installation's kind and charge, if any.
    pub candidate: Option<(ScreenKind, u64)>,
}

impl Admission {
    /// `kind`'s committed charge.
    #[must_use]
    pub const fn charge(&self, kind: ScreenKind) -> u64 {
        let [play, selection, decision, result] = self.committed;
        match kind {
            ScreenKind::Play => play,
            ScreenKind::Selection => selection,
            ScreenKind::Decision => decision,
            ScreenKind::Result => result,
        }
    }

    /// Every kind's committed charge together.
    #[must_use]
    pub const fn committed_total(&self) -> u64 {
        let [play, selection, decision, result] = self.committed;
        play.saturating_add(selection)
            .saturating_add(decision)
            .saturating_add(result)
    }
}

/// What a renderer holds in memory for its play skin, the result's
/// installation and the one staged installation of any kind, in bytes, as
/// a witness of the peak a skin's replacement reaches (design.md "Memory
/// Budget"): the installed resources and the staged ones live together
/// until the commit, and what a commit replaces until the next staging.
/// Pages shared by several holders are counted once ([`shared_cpu`]), and
/// each bundle's own bytes in one term. The selection and decision
/// screens' committed installations are reported only through
/// [`Renderer::admission`], but for the selection screen's lines and the
/// selection skin's state, which change with each update it draws.
///
/// [`shared_cpu`]: Self::shared_cpu
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
    /// The same for the staged resources, or a layout staged on the
    /// array installed.
    pub staged_batch: u64,
    /// The installed skin's prepared attempt: its state and every buffer
    /// it holds ([`PlaySkin::attempt_bytes`]).
    pub attempt: u64,
    /// The staged attempt's, beside it until the commit, even when the
    /// staging reuses the installed resources.
    pub staged_attempt: u64,
    /// The installed installation's loading presentation: its state and
    /// its text. The render thread's copy kept for recovery is this very
    /// state, shared, so it is counted here once.
    pub loading: u64,
    /// The staged installation's, when it is another installation's.
    pub staged_loading: u64,
    /// The chosen chart's loading text prepared and waiting to install
    /// ([`Renderer::prepare_loading_text`]), with its holder's list.
    pub staged_text: u64,
    /// The default font, shared by every text producer: counted once, in
    /// no bundle.
    pub default_font: u64,
    /// The text preparation's line and rectangle buffers.
    pub text_scratch: u64,
    /// The producer's glyph mask, live beside them while a line is
    /// rasterized: its bound, a line's pixels ([`MAX_TEXT_MASK_BYTES`]),
    /// once a preparation has rasterized.
    ///
    /// [`MAX_TEXT_MASK_BYTES`]: crate::MAX_TEXT_MASK_BYTES
    pub text_mask: u64,
    /// The bytes the queue staged for the last text preparation's writes.
    pub text_upload: u64,
    /// The installed text layer's cache: its entries, aliases and the
    /// holders of the installed attempt and the installed loading lines.
    pub text_cache: u64,
    /// The staged resources' text cache, when they are new, and the staged
    /// attempt's holder.
    pub staged_text_cache: u64,
    /// The committed selection installation's lines held in its text
    /// layer's cache: the holder of the lines its frames draw.
    pub selection_text: u64,
    /// The selection skin's state the render thread keeps for the screen's
    /// lifetime, apart from the renderer ([`Renderer::memory_with`]).
    pub selection_state: u64,
    /// The pages on the CPU, shared by every bundle on them: counted once
    /// for each atlas identity ([`AtlasId`]) play's installation, the
    /// staging or the result's installation holds
    /// ([`SkinBundle::shared_bytes`]).
    pub shared_cpu: u64,
    /// The result's committed bundle's own CPU description, counted here
    /// alone when it is also staged.
    pub result_cpu: u64,
    /// The result's committed layout: its batch, staged instances and
    /// instance buffer, and its clips.
    pub result_batch: u64,
    /// The result's committed atlas array, a layout's or a pages-only
    /// installation's: every layer's levels.
    pub result_atlas: u64,
    /// The result's text layer's cache, a layout's or a pages-only
    /// installation's, with the lines its frames draw and the ones
    /// prepared to replace them.
    pub result_text: u64,
    /// The loading presentations of the result's committed installation
    /// and of a retired one, each distinct presentation once; a staging
    /// sharing one of them adds nothing to `staged_loading`.
    pub result_loading: u64,
    /// What commits replaced and keep until the next staging or
    /// `drain_retired()`, whichever comes first: a layout's
    /// staging and instance buffer on the same array, or a whole
    /// installation on another array (its array, layout, text cache,
    /// lines and own CPU description) or a pages-only one.
    pub retired: u64,
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
            .saturating_add(self.attempt)
            .saturating_add(self.staged_attempt)
            .saturating_add(self.loading)
            .saturating_add(self.staged_loading)
            .saturating_add(self.staged_text)
            .saturating_add(self.default_font)
            .saturating_add(self.text_scratch)
            .saturating_add(self.text_mask)
            .saturating_add(self.text_upload)
            .saturating_add(self.text_cache)
            .saturating_add(self.staged_text_cache)
            .saturating_add(self.selection_text)
            .saturating_add(self.selection_state)
            .saturating_add(self.shared_cpu)
            .saturating_add(self.result_cpu)
            .saturating_add(self.result_atlas)
            .saturating_add(self.result_batch)
            .saturating_add(self.result_text)
            .saturating_add(self.result_loading)
            .saturating_add(self.retired)
    }
}

#[cfg(feature = "dev-tools")]
thread_local! {
    /// When this thread's last selection or result build began: the start
    /// its frame's `cpu` counts from (dev-tools).
    static BUILD_START: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// When this thread's last [`Renderer::build_selection`] or
/// [`Renderer::build_result`] began, taken (dev-tools: the order of a
/// frame's spans).
#[cfg(feature = "dev-tools")]
pub fn take_build_start() -> Option<Instant> {
    BUILD_START.with(std::cell::Cell::take)
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
    /// The adapter choice it was built with.
    choice: AdapterChoice,
    output: Output,
    /// The target's size as last applied, never zero.
    size: (u32, u32),
    /// The window is minimized (the last size applied was zero): nothing
    /// is configured or presented until a nonzero size comes.
    minimized: bool,
    format: wgpu::TextureFormat,
    alpha: wgpu::RenderPipeline,
    additive: wgpu::RenderPipeline,
    multiply: wgpu::RenderPipeline,
    bindings: Bindings,
    atlas: AtlasTexture,
    atlas_bind: wgpu::BindGroup,
    instances: wgpu::Buffer,
    /// The batch, its staging copied into `instances` each frame, and the
    /// play skin's projection and clip scissors.
    staging: Staging,
    /// The text layer's cache of the atlas in use.
    text_cache: TextCache,
    /// The lines the installed attempt's table draws.
    installed_text: Holder,
    /// The lines the installed presentation's loading text slots draw,
    /// slot by slot.
    installed_loading: Holder,
    /// The text preparation's buffers.
    text_scratch: TextScratch,
    /// What the last text preparation did.
    text_report: TextReport,
    skin: Box<dyn PlaySkin>,
    /// The declarative installation in use, its bundle with its loading
    /// presentation; `None` for the simple skin or one set with
    /// [`set_skin`](Self::set_skin).
    installed: Option<Installation>,
    /// The selection, decision and result screens' committed
    /// installations, each with its own atlas array, so changing screens
    /// never swaps an atlas ([`screen`](Self::screen) gives a kind's).
    screens: [Option<Slot>; 3],
    /// The request committed last ([`commit`](Self::commit)): no staging
    /// or commit of a request up to it is taken.
    committed: u64,
    /// The newest request a staging was asked for, kept through its
    /// discard or refusal, and so never below `committed`: no staging of
    /// a request up to it is taken.
    requested: u64,
    /// Bumped each time the atlas and the staging are replaced: a test of
    /// whether an installation reused them.
    resources: u64,
    /// The installation staged and not yet committed.
    pending: Option<Pending>,
    /// The result kind's pages-only installation, in place of its slot.
    result_pages: Option<PagesRes>,
    /// What the last commit replaced, until the next staging or a drain.
    retired: Option<Retired>,
    /// Play's admission released the result's installation
    /// ([`take_result_release`](Self::take_result_release)).
    result_released: bool,
    /// The result kind is disabled for the run ([`disable_result`](Self::disable_result)).
    result_disabled: bool,
    /// The result's lines prepared and not yet drawn, with the pages
    /// whose text layer holds them
    /// ([`prepare_result_text`](Self::prepare_result_text)).
    staged_result_text: Option<(AtlasId, Holder)>,
    /// Tests and development builds: atlas arrays uploaded so far.
    #[cfg(any(test, feature = "dev-tools"))]
    atlas_uploads: u64,
    /// Tests and development builds: the last atlas array upload's timing
    /// (the S10-09i design's §8.1 G1), until
    /// [`take_upload_timing`](Self::take_upload_timing) takes it.
    #[cfg(any(test, feature = "dev-tools"))]
    upload_timing: Option<UploadTiming>,
    /// When the built frame's work began, until [`submit`](Self::submit)
    /// takes it.
    started: Option<Instant>,
    /// The skin's refusal of a frame of this attempt: no frame of it is
    /// built again, and nothing submitted, until the next
    /// [`load`](Self::load) prepares a fresh one.
    refused: Option<BuildError>,
    /// The skin's refusal of a loading frame, with its loading lifetime:
    /// no loading frame of that lifetime is built again on this skin.
    loading_refused: Option<(u64, BuildError)>,
    /// The chosen chart's loading text prepared and not yet installed:
    /// at most one ([`prepare_loading_text`](Self::prepare_loading_text)).
    staged_text: Option<StagedText>,
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
    /// Run by the next [`publish_chart_images`](Self::publish_chart_images)
    /// inside its error scopes, after its writes are queued: a unit test
    /// makes the device refuse something there.
    #[cfg(test)]
    chart_fault: Option<fn(&Gpu)>,
    /// The installation's chart images as the last publication's writes
    /// were queued: a unit test checks they were unavailable then.
    #[cfg(test)]
    chart_during_write: Option<Option<skin_model::chart_image::ChartTable>>,
    /// The image each of the last publication's submissions wrote, with
    /// the waits finished before it was queued: a unit test checks there
    /// is one image per submission, each waited for before the next.
    #[cfg(test)]
    chart_submissions: Vec<(skin_model::chart_image::ChartImage, usize)>,
    /// The waits the last publication finished.
    #[cfg(test)]
    chart_waits: usize,
    /// Returned by the next chart publication's wait in place of the
    /// device's answer: a unit test makes a wait fail.
    #[cfg(test)]
    chart_wait_fault: Option<fn() -> RenderError>,
    /// The chart images last published into the play installation's
    /// chart layer, with the array they were written into
    /// ([`Renderer::publish_chart_images`]).
    chart: Option<PublishedChart>,
    /// A staging's instance buffer is made invalid (both map modes, which
    /// every device's validation refuses), as a device refusing its
    /// allocation would: a unit test.
    #[cfg(test)]
    buffer_fault: bool,
    /// A unit test's hooks into the text upload.
    #[cfg(test)]
    pub(crate) text_hooks: text_upload::Hooks,
    /// Development builds: the next text preparation's wait for the GPU
    /// fails ([`fail_next_text_wait`](Self::fail_next_text_wait)).
    #[cfg(feature = "dev-tools")]
    text_wait_fault: bool,
    /// Development builds: the lines the last selection text preparation
    /// asked for.
    #[cfg(feature = "dev-tools")]
    selection_requests: Vec<String>,
    /// Development builds: the lines it rasterized.
    #[cfg(feature = "dev-tools")]
    selection_rasterized: usize,
    /// Development builds: the next selection frame, or text preparation,
    /// is refused as a skin refuses one.
    #[cfg(feature = "dev-tools")]
    selection_faults: (bool, bool),
    /// Tests and development builds: the next result frame, or text
    /// preparation, is refused as a skin refuses one.
    #[cfg(any(test, feature = "dev-tools"))]
    result_faults: (bool, bool),
    /// Tests and development builds: the charges stated for bundles
    /// ([`state_charge`](Self::state_charge)).
    #[cfg(any(test, feature = "dev-tools"))]
    stated: Vec<(Arc<SkinBundle>, u64)>,
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
        let choice = adapter;
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
        let (alpha, additive, multiply, bindings) = pipelines(&gpu, format);
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
            choice,
            output,
            size,
            minimized: false,
            format,
            alpha,
            additive,
            multiply,
            bindings,
            atlas,
            atlas_bind,
            instances,
            staging,
            text_cache: TextCache::none(),
            installed_text: Holder::default(),
            installed_loading: Holder::default(),
            text_scratch: TextScratch::default(),
            text_report: TextReport::default(),
            skin,
            installed: None,
            screens: [None, None, None],
            committed: 0,
            requested: 0,
            resources: 0,
            pending: None,
            result_pages: None,
            retired: None,
            result_released: false,
            result_disabled: false,
            staged_result_text: None,
            #[cfg(any(test, feature = "dev-tools"))]
            atlas_uploads: 0,
            #[cfg(any(test, feature = "dev-tools"))]
            upload_timing: None,
            started: None,
            refused: None,
            loading_refused: None,
            staged_text: None,
            pacer: Pacer::default(),
            clear: wgpu::Color::BLACK,
            #[cfg(test)]
            atlas_fault: None,
            #[cfg(test)]
            chart_fault: None,
            #[cfg(test)]
            chart_during_write: None,
            #[cfg(test)]
            chart_submissions: Vec::new(),
            #[cfg(test)]
            chart_waits: 0,
            #[cfg(test)]
            chart_wait_fault: None,
            chart: None,
            #[cfg(test)]
            buffer_fault: false,
            #[cfg(test)]
            text_hooks: text_upload::Hooks::default(),
            #[cfg(feature = "dev-tools")]
            text_wait_fault: false,
            #[cfg(feature = "dev-tools")]
            selection_requests: Vec::new(),
            #[cfg(feature = "dev-tools")]
            selection_rasterized: 0,
            #[cfg(feature = "dev-tools")]
            selection_faults: (false, false),
            #[cfg(any(test, feature = "dev-tools"))]
            result_faults: (false, false),
            #[cfg(any(test, feature = "dev-tools"))]
            stated: Vec::new(),
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
        let target = self.target();
        for slot in self.screens.iter_mut().flatten() {
            let canvas = slot.installation.bundle().skin().canvas(target);
            slot.resources.staging.project(&canvas);
        }
    }

    /// Projects the play skin's canvas and clips onto the current target.
    fn project(&mut self) {
        let canvas = self.skin.canvas(self.target());
        self.staging.project(&canvas);
    }

    /// Where `kind`'s slot is in [`screens`](Self::screens); `None` for
    /// play, whose installation is the renderer's own fields.
    const fn screen(kind: ScreenKind) -> Option<usize> {
        match kind {
            ScreenKind::Play => None,
            ScreenKind::Selection => Some(0),
            ScreenKind::Decision => Some(1),
            ScreenKind::Result => Some(2),
        }
    }

    /// `kind`'s committed installation, if any.
    fn committed_installation(&self, kind: ScreenKind) -> Option<&Installation> {
        match Self::screen(kind) {
            None => self.installed.as_ref(),
            Some(i) => self
                .screens
                .get(i)
                .and_then(Option::as_ref)
                .map(|s| &s.installation),
        }
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
        self.chart = None;
        self.installed_text = Holder::default();
        self.installed_loading = Holder::default();
        self.refused = None;
        self.loading_refused = None;
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
        self.staging.set_clips(clips);
        self.atlas = atlas;
        self.advance_resources();
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

    /// The adapter choice it was built with; [`Self::gpu`] says which
    /// adapter that gave.
    #[must_use]
    pub const fn choice(&self) -> AdapterChoice {
        self.choice
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
    /// its READY state: the simple skin's way, whose preparation replaces no
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

    /// The declarative bundle committed for `kind`, if any.
    #[must_use]
    pub fn installed(&self, kind: ScreenKind) -> Option<&Arc<SkinBundle>> {
        self.committed_installation(kind).map(Installation::bundle)
    }

    /// `kind`'s committed declarative installation, its bundle with its
    /// loading presentation, shared: what the render thread keeps to
    /// restore on a renderer built again ([`restore`](Self::restore)).
    #[must_use]
    pub fn installation(&self, kind: ScreenKind) -> Option<Installation> {
        self.committed_installation(kind).cloned()
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
    /// `request`, before READY, leaving what is installed drawing
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
        self.stage_with(ScreenKind::Play, bundle, Some(a), request)
    }

    /// Stages `bundle`'s chart-independent installation of `kind` for
    /// `request`: as [`stage`](Self::stage), the atlas with the built-in
    /// layer, the clips, the staging, the instance buffer and the loading
    /// presentation, with no attempt and no `Play`. Play's frames draw
    /// nothing of a play until an attempt is staged and committed; its
    /// loading frames draw its presentation. Another kind's installation
    /// takes its own slot at its commit and is not drawn yet. Staging
    /// `kind`'s committed bundle keeps its resources and its presentation.
    ///
    /// # Errors
    ///
    /// As [`stage`](Self::stage).
    pub fn install(
        &mut self,
        kind: ScreenKind,
        bundle: &Arc<SkinBundle>,
        request: u64,
    ) -> Result<PreparedToken, RenderError> {
        self.stage_with(kind, bundle, None, request)
    }

    /// Whether `bundle` as `kind`'s installation fits the texels the
    /// screens share: every other kind's committed charge and `bundle`'s
    /// ([`Admission`]), at most [`SHARED_TEXELS`]. The candidate replaces
    /// its kind's charge, so the state after its commit is what is
    /// admitted. `kind`'s committed bundle itself allocates nothing again
    /// and is admitted as it is. Allocates nothing.
    ///
    /// A read-only budget query: `Ok` means the hypothetical replacement
    /// fits the charges committed now. It reserves nothing, changes no
    /// staging, validates no device resource, and does not promise that a
    /// later installation succeeds; every staging checks admission itself.
    ///
    /// # Errors
    ///
    /// [`RenderError::Admission`] with the sum, past the limit.
    pub fn admit(&self, kind: ScreenKind, bundle: &SkinBundle) -> Result<(), RenderError> {
        if self
            .committed_installation(kind)
            .is_some_and(|i| std::ptr::eq(Arc::as_ptr(i.bundle()), bundle))
        {
            return Ok(());
        }
        self.admit_charge(kind, charge_of(bundle), None)
    }

    /// Whether `charge` as `kind`'s fits beside every other kind's
    /// committed charge but `without`'s.
    fn admit_charge(
        &self,
        kind: ScreenKind,
        charge: u64,
        without: Option<ScreenKind>,
    ) -> Result<(), RenderError> {
        let others = KINDS
            .iter()
            .filter(|&&k| k != kind && Some(k) != without)
            .fold(0u64, |sum, &k| sum.saturating_add(self.charge(k)));
        let needed = others.saturating_add(charge);
        if needed > SHARED_TEXELS {
            return Err(RenderError::Admission {
                kind,
                needed,
                limit: SHARED_TEXELS,
            });
        }
        Ok(())
    }

    /// `kind`'s committed charge: its installation's, or the result's
    /// pages-only installation's array.
    fn charge(&self, kind: ScreenKind) -> u64 {
        match self.committed_installation(kind) {
            Some(i) => self.committed_charge(i.bundle()),
            None if kind == ScreenKind::Result => self
                .result_pages
                .as_ref()
                .map_or(0, |p| pages_texels(&p.pages)),
            None => 0,
        }
    }

    /// Admits `bundle` as `kind`'s ([`admit`](Self::admit)); for play, when
    /// only the result's committed installation keeps it out, that
    /// installation is released first (design.md "Memory Budget"): no
    /// result is drawn from Preparing until the result, and its owner
    /// installs it again after play's commit, if it is admitted then.
    fn admit_or_release(
        &mut self,
        kind: ScreenKind,
        bundle: &Arc<SkinBundle>,
    ) -> Result<(), RenderError> {
        match self.admit(kind, bundle) {
            Err(_)
                if kind == ScreenKind::Play
                    && self.charge(ScreenKind::Result) > 0
                    && self
                        .admit_charge(kind, charge_of(bundle), Some(ScreenKind::Result))
                        .is_ok() =>
            {
                self.remove(ScreenKind::Result);
                self.result_released = true;
                Ok(())
            }
            other => other,
        }
    }

    /// The pages `kind`'s committed atlas array holds, if any: its
    /// installation's, or the result's pages-only installation's. Play's
    /// array is never shared with another staging.
    fn committed_atlas(&self, kind: ScreenKind) -> Option<AtlasId> {
        let i = Self::screen(kind)?;
        match self.screens.get(i).and_then(Option::as_ref) {
            Some(slot) => Some(slot.installation.bundle().atlas_id()),
            None if kind == ScreenKind::Result => self.result_pages.as_ref().map(|p| p.pages.id()),
            None => None,
        }
    }

    /// What a committed `bundle` is charged: its own charge, or in tests
    /// and development builds the one stated for it.
    fn committed_charge(&self, bundle: &Arc<SkinBundle>) -> u64 {
        #[cfg(any(test, feature = "dev-tools"))]
        if let Some((_, texels)) = self.stated.iter().find(|(b, _)| Arc::ptr_eq(b, bundle)) {
            return *texels;
        }
        charge_of(bundle)
    }

    /// Tests and development builds: charges `bundle` `texels` whenever it
    /// is a kind's committed installation, in place of its own charge, so
    /// the shared limit is tested through [`admit`](Self::admit) without
    /// committing arrays that large. A candidate is charged its own.
    #[cfg(any(test, feature = "dev-tools"))]
    pub fn state_charge(&mut self, bundle: &Arc<SkinBundle>, texels: u64) {
        self.stated.retain(|(b, _)| !Arc::ptr_eq(b, bundle));
        self.stated.push((Arc::clone(bundle), texels));
    }

    /// The charges [`admit`](Self::admit) sums now, and the staged
    /// installation's.
    #[must_use]
    pub fn admission(&self) -> Admission {
        Admission {
            committed: KINDS.map(|k| self.charge(k)),
            candidate: self.pending.as_ref().map(|p| {
                let charge = match (&p.skin, &p.pages) {
                    (Some(s), _) => charge_of(s.bundle()),
                    (None, Some(pages)) => pages_texels(pages),
                    (None, None) => 0,
                };
                (p.token.kind, charge)
            }),
        }
    }

    /// [`stage`](Self::stage) with attempt `a`, or
    /// [`install`](Self::install) without one. A request not newer than
    /// every one asked for or committed (staged, discarded or refused
    /// included) is refused, the staging kept; any other refusal drops
    /// the staging. Admission ([`admit`](Self::admit)) is checked before
    /// anything is allocated. `kind`'s committed bundle keeps its loading
    /// presentation; another gets a fresh one.
    fn stage_with(
        &mut self,
        kind: ScreenKind,
        bundle: &Arc<SkinBundle>,
        a: Option<&AttemptInput<'_>>,
        request: u64,
    ) -> Result<PreparedToken, RenderError> {
        // Never below `committed`: a commit takes only a staging's
        // request, and every staging was asked for first.
        if kind == ScreenKind::Result && self.result_disabled {
            return Err(RenderError::ResultDisabled);
        }
        self.begin_staging(request)?;
        // An attempt's staging that fails, at any step, leaves no earlier
        // preparation's text report.
        if a.is_some() {
            self.text_report = TextReport::default();
        }
        self.admit_or_release(kind, bundle)?;
        let installed = self
            .committed_installation(kind)
            .filter(|i| Arc::ptr_eq(i.bundle(), bundle))
            .cloned();
        let (installation, mut res) = match installed {
            Some(i) => (i, PendingRes::Installed),
            // Another bundle on the pages installed: a fresh layout on
            // their array, nothing uploaded.
            None if self.committed_atlas(kind) == Some(bundle.atlas_id()) => {
                let (staging, instances) = self.layout_for(bundle)?;
                (
                    Installation::new(Arc::clone(bundle)),
                    PendingRes::Layout {
                        staging: Box::new(staging),
                        instances,
                    },
                )
            }
            None => (
                Installation::new(Arc::clone(bundle)),
                PendingRes::Own(Box::new(self.resources_for(bundle)?)),
            ),
        };
        let mut skin = DeclSkin::of(&installation);
        let mut text = Holder::default();
        if let Some(a) = a {
            let a = AttemptInput {
                target: self.target(),
                ..*a
            };
            skin.prepare(&self.gpu, &a).map_err(RenderError::Skin)?;
            if let Some(producer) = bundle.text_producer() {
                let key = self.loading_text_key();
                // The staging's own resources, or the installed ones,
                // whose installed attempt's lines stay, with the installed
                // loading lines and the loading text staged for them.
                let (atlas, cache, held) = match &mut res {
                    PendingRes::Own(r) => (&r.atlas, &mut r.text_cache, Vec::<&Holder>::new()),
                    _ => {
                        let staged = self
                            .staged_text
                            .as_ref()
                            .filter(|t| key == Some(t.key))
                            .map(|t| &t.holder);
                        let held = [
                            Some(&self.installed_text),
                            Some(&self.installed_loading),
                            staged,
                        ];
                        let held = held.into_iter().flatten().collect();
                        (&self.atlas, &mut self.text_cache, held)
                    }
                };
                let prepared = text_upload::prepare(
                    text_upload::Prepare {
                        gpu: &self.gpu,
                        atlas,
                        cache,
                        scratch: &mut self.text_scratch,
                        producer: producer.as_ref(),
                        scope: bundle.text_scope(),
                        held: &held,
                        #[cfg(test)]
                        hooks: &mut self.text_hooks,
                        #[cfg(feature = "dev-tools")]
                        wait_fault: &mut self.text_wait_fault,
                    },
                    &bundle
                        .skin()
                        .text_requests(a.text, a.target, &Layout::AUTHOR),
                    bundle.skin().text_slots(),
                );
                let (table, holder, report) = prepared?;
                self.text_report = report;
                skin.attach_text(table).map_err(RenderError::Skin)?;
                text = holder;
            }
        }
        let token = PreparedToken {
            request,
            device: self.id,
            kind,
        };
        self.pending = Some(Pending {
            token,
            skin: Some(skin),
            pages: None,
            res,
            text,
            chart: None,
        });
        #[cfg(test)]
        self.text_hooks.steps.push(text_upload::Step::StageReturned);
        Ok(token)
    }

    /// What the last preparation of rasterized text did, an attempt's
    /// ([`stage`](Self::stage)) or the loading text's
    /// ([`prepare_loading_text`](Self::prepare_loading_text)): its notes,
    /// counts, upload bytes and submission.
    #[must_use]
    pub const fn text_report(&self) -> &TextReport {
        &self.text_report
    }

    /// The resources of `bundle`: a staging sized for its capacities with
    /// its clips, then, inside one transaction of error scopes, its atlas
    /// uploaded with its bind group and an instance buffer for the staging,
    /// all finished on the GPU. A device refusing any of them (memory,
    /// validation) is answered as a refusal, never an uncaptured error.
    fn resources_for(&mut self, bundle: &Arc<SkinBundle>) -> Result<Resources, RenderError> {
        let staging = self.staging_for(bundle)?;
        let (atlas, atlas_bind, buffer, text_cache) =
            self.upload_array(bundle.atlas_pages(), Some(staging.instance_capacity()))?;
        #[expect(
            clippy::expect_used,
            reason = "upload_array makes the instance buffer it is given a size for"
        )]
        let instances = buffer.expect("an instance buffer was asked for");
        Ok(Resources {
            atlas,
            atlas_bind,
            staging,
            instances,
            text_cache,
        })
    }

    /// The one upload transaction of an atlas array: `pages` checked, then,
    /// inside one set of error scopes, the array uploaded with the built-in
    /// layer, its bind group and, with `instances`, a staging's instance
    /// buffer of that many sprites, all submitted once and finished on the
    /// GPU; and the empty cache of its text layer. A device refusing any of
    /// them (memory, validation) is answered as a refusal.
    fn upload_array(
        &mut self,
        pages: &PackedPages,
        instances: Option<usize>,
    ) -> Result<
        (
            AtlasTexture,
            wgpu::BindGroup,
            Option<wgpu::Buffer>,
            TextCache,
        ),
        RenderError,
    > {
        #[cfg(any(test, feature = "dev-tools"))]
        {
            self.atlas_uploads += 1;
        }
        #[cfg(any(test, feature = "dev-tools"))]
        let began = Instant::now();
        #[cfg(any(test, feature = "dev-tools"))]
        let done = Arc::new(std::sync::OnceLock::new());
        #[cfg(any(test, feature = "dev-tools"))]
        let mut submitted = began;
        let device = self.gpu.device();
        let levels = page_levels(pages);
        let upload = AtlasPages {
            side: pages.side(),
            pages: &levels,
            text_layer: pages.text_layer().is_some(),
            chart_layer: pages.chart_layer().is_some(),
        };
        let layers =
            skin_atlas::check_pages(&upload, &device.limits()).map_err(RenderError::Atlas)?;
        let ((atlas, atlas_bind, buffer), refused) = skin_atlas::scoped(device, || {
            let atlas = AtlasTexture::upload(&self.gpu, &upload, layers);
            let bind = self.bindings.bind(device, &atlas);
            #[cfg(test)]
            if let Some(fault) = self.atlas_fault {
                fault(&self.gpu);
            }
            let buffer = instances.map(|n| self.staged_buffer(n));
            self.gpu.queue().submit([]);
            #[cfg(any(test, feature = "dev-tools"))]
            {
                submitted = Instant::now();
                let done = Arc::clone(&done);
                self.gpu.queue().on_submitted_work_done(move || {
                    let _ = done.set(Instant::now());
                });
            }
            (atlas, bind, buffer)
        });
        if let Some(e) = refused {
            return Err(RenderError::Atlas(UploadError::Device(e)));
        }
        wait_for_gpu(&self.gpu)?;
        #[cfg(any(test, feature = "dev-tools"))]
        {
            self.upload_timing = Some(UploadTiming {
                written: submitted.saturating_duration_since(began),
                done: done.get().map(|d| d.saturating_duration_since(submitted)),
            });
        }
        let text_cache = pages
            .text_layer()
            .map_or_else(TextCache::none, |layer| TextCache::new(atlas.side(), layer));
        Ok((atlas, atlas_bind, buffer, text_cache))
    }

    /// A staging sized for `bundle`'s capacities, with its clips, projected
    /// on the current target.
    ///
    /// # Errors
    ///
    /// [`RenderError::Capacity`] when its sprites do not fit a buffer.
    fn staging_for(&self, bundle: &Arc<SkinBundle>) -> Result<Staging, RenderError> {
        let skin = bundle.skin();
        let (capacity, runs) = (
            skin.quad_capacity().max(MAX_SPRITES as u32),
            skin.run_capacity().max(MAX_SPRITES as u32),
        );
        let instances =
            usize::try_from(capacity.saturating_add(OVERLAY_SPRITES)).unwrap_or(usize::MAX);
        let max = self.gpu.device().limits().max_buffer_size;
        if instance_bytes(instances) > max {
            return Err(RenderError::Capacity {
                sprites: capacity,
                max,
            });
        }
        let mut staging = Staging::with_runs(capacity, runs, &skin.canvas(self.target()));
        staging.set_clips(bundle.clips().clone());
        Ok(staging)
    }

    /// A fresh layout for `bundle` on its kind's committed atlas array:
    /// its staging and an instance buffer, inside error scopes, with no
    /// texture uploaded.
    fn layout_for(&self, bundle: &Arc<SkinBundle>) -> Result<(Staging, wgpu::Buffer), RenderError> {
        let staging = self.staging_for(bundle)?;
        let device = self.gpu.device();
        let (buffer, refused) = skin_atlas::scoped(device, || {
            let buffer = self.staged_buffer(staging.instance_capacity());
            self.gpu.queue().submit([]);
            buffer
        });
        if let Some(e) = refused {
            return Err(RenderError::Atlas(UploadError::Device(e)));
        }
        wait_for_gpu(&self.gpu)?;
        Ok((staging, buffer))
    }

    /// `pages` uploaded as an atlas array with the built-in layer and its
    /// bind group, inside error scopes, finished on the GPU.
    fn pages_for(&mut self, pages: &Arc<PackedPages>) -> Result<PagesRes, RenderError> {
        let (atlas, atlas_bind, _, text_cache) = self.upload_array(pages, None)?;
        Ok(PagesRes {
            pages: Arc::clone(pages),
            atlas,
            atlas_bind,
            text_cache,
        })
    }

    /// Takes `request` for a staging: refused unless newer than every
    /// request asked for or committed, then the earlier staging and what
    /// the last commit replaced are dropped.
    fn begin_staging(&mut self, request: u64) -> Result<(), RenderError> {
        // Never below `committed`: a commit takes only a staging's
        // request, and every staging was asked for first.
        let newer = self.requested;
        if request <= newer {
            return Err(RenderError::Superseded { request, newer });
        }
        self.requested = request;
        self.pending = None;
        self.retired = None;
        Ok(())
    }

    /// Stages the result kind's pages-only installation of `pages` for
    /// `request`: its prepared atlas, uploaded before any layout of it is
    /// staged ([`stage`](Self::stage)'s rules for requests). When `pages`
    /// are the ones installed nothing is uploaded; otherwise the kind's
    /// earlier installation is released first, since nothing of it is
    /// drawn meanwhile, then the pages are admitted and uploaded. The
    /// commit makes them the kind's installation, drawing nothing until a
    /// layout on them is committed. Allocates and waits for the GPU.
    ///
    /// # Errors
    ///
    /// [`RenderError::PagesKind`] for a kind other than the result's,
    /// [`RenderError::ResultDisabled`] while it is disabled, and what
    /// refuses a staging; the staging is then discarded.
    pub fn install_pages(
        &mut self,
        kind: ScreenKind,
        pages: &Arc<PackedPages>,
        request: u64,
    ) -> Result<PreparedToken, RenderError> {
        if kind != ScreenKind::Result {
            return Err(RenderError::PagesKind { kind });
        }
        if self.result_disabled {
            return Err(RenderError::ResultDisabled);
        }
        self.begin_staging(request)?;
        let res = if self.committed_atlas(kind) == Some(pages.id()) {
            PendingRes::Pages(None)
        } else {
            self.release(kind);
            self.admit_charge(kind, pages_texels(pages), None)?;
            PendingRes::Pages(Some(Box::new(self.pages_for(pages)?)))
        };
        let token = PreparedToken {
            request,
            device: self.id,
            kind,
        };
        self.pending = Some(Pending {
            token,
            skin: None,
            pages: Some(Arc::clone(pages)),
            res,
            text: Holder::default(),
            chart: None,
        });
        Ok(token)
    }

    /// Drops `kind`'s committed installation, the result's pages-only one
    /// and its staged lines included, leaving a staging in place.
    fn release(&mut self, kind: ScreenKind) {
        if let Some(slot) = Self::screen(kind).and_then(|i| self.screens.get_mut(i)) {
            *slot = None;
        }
        if kind == ScreenKind::Result {
            self.result_pages = None;
            self.staged_result_text = None;
        }
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

    /// Installs the staging of `token` whole, between frames. Play's: its
    /// skin with its prepared attempt, and its atlas, bind group, clips,
    /// staging and instance buffer together when it has its own; the ones
    /// they replace are dropped, and frames are built again after a
    /// refused one. Another kind's: its installation and resources become
    /// that kind's slot, nothing of play's changing; a staging of the
    /// kind's committed bundle keeps the slot.
    ///
    /// # Errors
    ///
    /// [`CommitError`] when `token` is not the staging's, by its renderer,
    /// its request or its kind: nothing changes, and the staging stays
    /// staged.
    pub fn commit(&mut self, token: PreparedToken) -> Result<(), CommitError> {
        if token.device != self.id {
            return Err(CommitError::Device);
        }
        if token.kind == ScreenKind::Result && self.result_disabled {
            return Err(CommitError::ResultDisabled);
        }
        if token.request <= self.committed {
            return Err(CommitError::Superseded {
                committed: self.committed,
                asked: token.request,
            });
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
        if staged.kind != token.kind {
            return Err(CommitError::Kind {
                staged: staged.kind,
                asked: token.kind,
            });
        }
        let Some(pending) = self.pending.take() else {
            return Err(CommitError::NothingStaged);
        };
        self.committed = token.request;
        let target = self.target();
        match Self::screen(token.kind) {
            None => {
                let staged_chart = pending.chart;
                if let PendingRes::Own(r) = pending.res {
                    self.replace_resources(*r);
                }
                self.installed_text = pending.text;
                if let Some(skin) = pending.skin {
                    self.activate_decl_skin(skin);
                }
                // Images published into the staged array, keyed only now:
                // after the activation, the live key names the incoming
                // installation and its atlas with the generation this
                // commit made (before it, `installed` was still the
                // outgoing one), so the attempt takes them from its commit
                // on (S10-12 design P12).
                if let Some(table) = staged_chart
                    && let Some(key) = self.chart_key()
                {
                    self.chart = Some(PublishedChart {
                        key,
                        table: table.keyed(key),
                    });
                    self.sync_chart();
                }
            }
            Some(i) => {
                let installation = pending.skin.as_ref().map(DeclSkin::installation);
                let result = token.kind == ScreenKind::Result;
                // What it replaces keeps its device resources until the
                // next staging or a drain: a commit drops none of them.
                let old = self.screens.get_mut(i).and_then(Option::take);
                let pages = if result {
                    self.result_pages.take()
                } else {
                    None
                };
                match (pending.res, installation) {
                    (PendingRes::Own(resources), Some(installation)) => {
                        let mut resources = *resources;
                        // Projected afresh, as play's commit projects its
                        // own: the target may have been resized since the
                        // staging.
                        let canvas = installation.bundle().skin().canvas(target);
                        resources.staging.project(&canvas);
                        if let Some(slot) = self.screens.get_mut(i) {
                            *slot = Some(Slot {
                                installation,
                                resources,
                                text: Holder::default(),
                            });
                        }
                        self.retired = old.map(Retired::Slot).or_else(|| pages.map(Retired::Pages));
                    }
                    (
                        PendingRes::Layout {
                            mut staging,
                            instances,
                        },
                        Some(installation),
                    ) => {
                        // The installed array, with the lines it holds
                        // until the new layout's own are installed.
                        let held = match (old, pages) {
                            (Some(s), _) => {
                                self.retired = Some(Retired::Layout {
                                    staging: s.resources.staging,
                                    _instances: s.resources.instances,
                                });
                                Some((
                                    s.resources.atlas,
                                    s.resources.atlas_bind,
                                    s.resources.text_cache,
                                    s.text,
                                ))
                            }
                            (None, Some(p)) => {
                                Some((p.atlas, p.atlas_bind, p.text_cache, Holder::default()))
                            }
                            (None, None) => None,
                        };
                        let canvas = installation.bundle().skin().canvas(target);
                        staging.project(&canvas);
                        if let (Some((atlas, atlas_bind, text_cache, text)), Some(slot)) =
                            (held, self.screens.get_mut(i))
                        {
                            *slot = Some(Slot {
                                installation,
                                resources: Resources {
                                    atlas,
                                    atlas_bind,
                                    staging: *staging,
                                    instances,
                                    text_cache,
                                },
                                text,
                            });
                        }
                    }
                    (PendingRes::Pages(Some(p)), _) => {
                        // `install_pages` released any installation first.
                        self.staged_result_text = None;
                        self.result_pages = Some(*p);
                    }
                    (_, _) => {
                        // Its own installation stays: the same bundle, or
                        // the same pages.
                        if let Some(slot) = self.screens.get_mut(i) {
                            *slot = old;
                        }
                        if result {
                            self.result_pages = pages;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Puts `skin` in use, its installation the one installed, and builds
    /// frames afresh: no refusal kept, no frame started, the projection
    /// made again for its canvas. A loading refusal stays only with the
    /// installation's own presentation, so the same installation keeps it
    /// for its lifetime and another starts afresh.
    fn activate_decl_skin(&mut self, skin: DeclSkin) {
        self.installed = Some(skin.installation());
        self.skin = Box::new(skin);
        self.refused = None;
        self.loading_refused = None;
        self.started = None;
        self.project();
        // The chart images valid for the array now in use, or none.
        self.sync_chart();
    }

    /// Replaces the atlas, its bind group, the staging and the instance
    /// buffer with `r`, together. The installed loading lines' holder goes
    /// with the cache it held lines in: the new resources' key makes the
    /// presentation's loading text stale, and the player clears it before
    /// the next loading frame is built.
    fn replace_resources(&mut self, r: Resources) {
        self.atlas = r.atlas;
        self.atlas_bind = r.atlas_bind;
        self.staging = r.staging;
        self.instances = r.instances;
        self.text_cache = r.text_cache;
        self.installed_loading = Holder::default();
        self.advance_resources();
    }

    /// The one step every replacement of the play array passes: a new
    /// resource generation, and any frame built against the old one
    /// cancelled, so no staged quad, a chart image's included, is
    /// submitted against an array it was not built for (S10-12 design
    /// §5.3).
    fn advance_resources(&mut self) {
        self.resources += 1;
        self.started = None;
    }

    /// Drops the staged installation, if any: its resources go, and the
    /// installation in use stays.
    pub fn discard(&mut self) {
        self.pending = None;
    }

    /// Installs `installation` as `kind`'s with no attempt prepared, at
    /// once: a renderer built again after a device failure restores each
    /// kind's committed installation from memory this way, its loading
    /// presentation the very state, `when` starts included, and play's
    /// next attempt is prepared afresh through [`stage`](Self::stage),
    /// which reuses its resources. A staging is dropped. Admission is
    /// checked as for a staging: kinds restored one at a time, in
    /// [`Admission::committed`]'s order, are each a subset of a set
    /// admitted together.
    ///
    /// # Errors
    ///
    /// As [`stage`](Self::stage); every installation in use stays.
    pub fn restore(
        &mut self,
        kind: ScreenKind,
        installation: &Installation,
    ) -> Result<(), RenderError> {
        self.pending = None;
        if kind == ScreenKind::Result && self.result_disabled {
            return Err(RenderError::ResultDisabled);
        }
        self.admit(kind, installation.bundle())?;
        let r = self.resources_for(installation.bundle())?;
        match Self::screen(kind) {
            None => {
                self.replace_resources(r);
                self.installed_text = Holder::default();
                self.activate_decl_skin(DeclSkin::of(installation));
            }
            Some(i) => {
                if let Some(slot) = self.screens.get_mut(i) {
                    *slot = Some(Slot {
                        installation: installation.clone(),
                        resources: r,
                        text: Holder::default(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Drops `kind`'s committed installation and its charge, and a
    /// staging of `kind`: its screen draws its built-in presentation.
    /// Play's installation is never removed this way: for play it does
    /// nothing.
    pub fn remove(&mut self, kind: ScreenKind) {
        if Self::screen(kind).is_none() {
            return;
        }
        self.release(kind);
        if self.pending.as_ref().is_some_and(|p| p.token.kind == kind) {
            self.pending = None;
        }
    }

    /// Restores the result kind's pages-only installation of `pages` at
    /// once, as [`restore`](Self::restore) restores an installation: a
    /// renderer built again restores the result's prepared atlas this way.
    ///
    /// # Errors
    ///
    /// As [`install_pages`](Self::install_pages); every installation in
    /// use stays.
    pub fn restore_pages(
        &mut self,
        kind: ScreenKind,
        pages: &Arc<PackedPages>,
    ) -> Result<(), RenderError> {
        self.pending = None;
        if kind != ScreenKind::Result {
            return Err(RenderError::PagesKind { kind });
        }
        if self.result_disabled {
            return Err(RenderError::ResultDisabled);
        }
        self.release(kind);
        self.admit_charge(kind, pages_texels(pages), None)?;
        self.result_pages = Some(self.pages_for(pages)?);
        Ok(())
    }

    /// Publishes the chart's own images into the committed play
    /// installation's chart layer (S10-12 design §5.3), outside every
    /// frame's build: `images` holds the fitted stage file and back image,
    /// `None` for one that is absent.
    ///
    /// **The rule.** A published table describes the texels of one GPU
    /// array: it is valid only for the installation, its atlas and the
    /// resource generation it was written into, on a device not lost
    /// since ([`PublishedChart`]). The renderer keeps it, and copies it
    /// only into what draws from that array: the installation's loading
    /// presentation and the committed attempt, each time the play
    /// installation is activated ([`sync_chart`](Self::sync_chart)). So a
    /// staged attempt committed on the same array takes the current one,
    /// and an array made again (a restore, new resources, another
    /// renderer) leaves the images unavailable until they are published
    /// into it.
    ///
    /// 1. **Invalidate**: before the first byte, the installation's images
    ///    become unavailable (its loading frames draw neither them nor what
    ///    their fields condition) and the committed attempt's absent.
    /// 2. **Write** one image at a time: its levels into its slot inside
    ///    error scopes, level `k` at the slot's origin `>> k`, submit, and
    ///    wait for the GPU before the next image is queued, so the queue
    ///    stages one image's levels at most (design note §4.4).
    /// 3. **Re-check** that the device was not lost, and that the
    ///    installation, its atlas and the resource generation are the ones
    ///    the write began on.
    /// 4. **Publish** the table.
    ///
    /// The images never change during an attempt: the caller publishes
    /// only before one opens.
    ///
    /// `target` names the array: the committed installation's, or a staged
    /// installation's own ([`ChartTarget::Staged`]), whose table is held
    /// with the staging, of no array, and keyed at its commit; a staging's
    /// table is not presented before it, so step 1 there only drops it.
    ///
    /// # Errors
    ///
    /// [`RenderError::chart_outcome`] tells what each left.
    ///
    /// - [`RenderError::NoChartLayer`] with no committed play installation
    ///   keeping a chart layer, or a staging without its own array keeping
    ///   one: nothing changes. [`RenderError::ChartChanged`] for a token
    ///   that is not the staging's.
    /// - [`RenderError::ChartImage`] for an image that does not fit its
    ///   slot, and [`RenderError::Atlas`] for a write the device refused
    ///   (after its submitted work is waited for): both refusals
    ///   ([`RenderError::is_refusal`]), after which the images are absent
    ///   for the installation, its fields reading false (S10-12 design
    ///   D-chart-5); nothing stale draws.
    /// - [`RenderError::DeviceLost`] for a device lost or destroyed during
    ///   the publication, a failing wait's error, or
    ///   [`RenderError::ChartChanged`]: the images stay unavailable, as the
    ///   first step left them.
    pub fn publish_chart_images(
        &mut self,
        target: ChartTarget,
        images: [Option<&skin_model::chart_image::Fitted>; 2],
    ) -> Result<skin_model::chart_image::ChartTable, RenderError> {
        match target {
            ChartTarget::Committed => self.publish_committed_chart(images),
            ChartTarget::Staged(token) => {
                let Some(mut pending) = self.pending.take() else {
                    return Err(RenderError::ChartChanged);
                };
                let out = self.publish_staged_chart(&mut pending, token, images);
                self.pending = Some(pending);
                out
            }
        }
    }

    /// [`publish_chart_images`](Self::publish_chart_images) into the
    /// committed installation.
    fn publish_committed_chart(
        &mut self,
        images: [Option<&skin_model::chart_image::Fitted>; 2],
    ) -> Result<skin_model::chart_image::ChartTable, RenderError> {
        use skin_model::chart_image::{ChartImage, ChartTable, ChartUv};
        let Some(installed) = self.installed.clone() else {
            return Err(RenderError::NoChartLayer);
        };
        let bundle = Arc::clone(installed.bundle());
        let (Some(layer), Some(at)) = (bundle.chart_layer(), self.atlas.chart_layer()) else {
            return Err(RenderError::NoChartLayer);
        };
        if u32::from(layer) != at {
            return Err(RenderError::NoChartLayer);
        }
        let Some(key) = self.chart_key() else {
            return Err(RenderError::DeviceLost);
        };
        let began = PublishedChart {
            key,
            table: ChartTable::ABSENT.keyed(key),
        };
        // 1. Unavailable from the first byte.
        self.chart = None;
        self.sync_chart();
        #[cfg(test)]
        {
            self.chart_during_write = None;
            self.chart_submissions.clear();
            self.chart_waits = 0;
        }
        let side = self.atlas.side();
        let mut uvs = [None; 2];
        for (which, image) in ChartImage::ALL.into_iter().zip(images) {
            let Some(f) = image else {
                continue;
            };
            // Every level the array has, no more and no fewer.
            let uv = ChartUv::of(f, which, side, layer)
                .filter(|_| u32::try_from(f.levels().len()).ok() == Some(self.atlas.levels()));
            let Some(uv) = uv else {
                self.chart = Some(began);
                self.sync_chart();
                return Err(RenderError::ChartImage { which });
            };
            if let Some(slot) = uvs.get_mut(which.index()) {
                *slot = Some(uv);
            }
        }
        // 2. One image at a time, each waited for before the next.
        for (which, image) in ChartImage::ALL.into_iter().zip(images) {
            let (Some(f), Some((origin, _))) = (image, which.slot(side)) else {
                continue;
            };
            let ((), refused) = skin_atlas::scoped(self.gpu.device(), || {
                for ((k, bytes), level) in (0_u32..).zip(f.levels()).zip(0..) {
                    let Some(size) = f.level_size(level) else {
                        continue;
                    };
                    skin_atlas::write_region(
                        self.gpu.queue(),
                        self.atlas.texture(),
                        (at, k),
                        origin.map(|o| o >> k),
                        size,
                        bytes,
                    );
                }
                #[cfg(test)]
                {
                    self.chart_during_write = Some(installed.loading().chart_images());
                    self.chart_submissions.push((which, self.chart_waits));
                    if let Some(fault) = self.chart_fault.take() {
                        fault(&self.gpu);
                    }
                }
                self.gpu.queue().submit([]);
            });
            // Submitted work is waited for, a refused write's included.
            let waited = self.wait_chart();
            if waited.is_err() || self.gpu.lost() {
                return Err(waited.err().unwrap_or(RenderError::DeviceLost));
            }
            if let Some(e) = refused {
                self.chart = Some(began);
                self.sync_chart();
                return Err(RenderError::Atlas(UploadError::Device(e)));
            }
        }
        // 3. Still the installation, its atlas and the array written into,
        // on a device not lost.
        let same = self.installed.as_ref().is_some_and(|i| {
            Arc::ptr_eq(i.bundle(), &bundle) && i.loading().same(installed.loading())
        }) && self.chart_key() == Some(key);
        if !same {
            return Err(RenderError::ChartChanged);
        }
        // 4. Published.
        let [stage, back] = uvs;
        let table = ChartTable::new(stage, back).keyed(key);
        self.chart = Some(PublishedChart { table, ..began });
        self.sync_chart();
        // A loss after the re-check discards the publication: it is the
        // device's failure, never a success.
        if self.valid_chart() != Some(table) {
            return Err(RenderError::DeviceLost);
        }
        Ok(table)
    }

    /// [`publish_chart_images`](Self::publish_chart_images) into the array
    /// of `pending`, the staging taken out for the write, which `token`
    /// must name: the same steps, its table held with it, of no array.
    fn publish_staged_chart(
        &mut self,
        pending: &mut Pending,
        token: PreparedToken,
        images: [Option<&skin_model::chart_image::Fitted>; 2],
    ) -> Result<skin_model::chart_image::ChartTable, RenderError> {
        use skin_model::chart_image::{ChartImage, ChartTable, ChartUv};
        if pending.token != token {
            return Err(RenderError::ChartChanged);
        }
        let (PendingRes::Own(res), Some(skin)) = (&pending.res, &pending.skin) else {
            return Err(RenderError::NoChartLayer);
        };
        let bundle = Arc::clone(skin.installation().bundle());
        let (Some(layer), Some(at)) = (bundle.chart_layer(), res.atlas.chart_layer()) else {
            return Err(RenderError::NoChartLayer);
        };
        if u32::from(layer) != at {
            return Err(RenderError::NoChartLayer);
        }
        if self.gpu.lost() {
            return Err(RenderError::DeviceLost);
        }
        // 1. Unavailable from the first byte.
        pending.chart = None;
        let side = res.atlas.side();
        let mut uvs = [None; 2];
        for (which, image) in ChartImage::ALL.into_iter().zip(images) {
            let Some(f) = image else {
                continue;
            };
            let uv = ChartUv::of(f, which, side, layer)
                .filter(|_| u32::try_from(f.levels().len()).ok() == Some(res.atlas.levels()));
            let Some(uv) = uv else {
                pending.chart = Some(ChartTable::ABSENT);
                return Err(RenderError::ChartImage { which });
            };
            if let Some(slot) = uvs.get_mut(which.index()) {
                *slot = Some(uv);
            }
        }
        // 2. One image at a time into the staging's own array, each waited
        // for before the next.
        for (which, image) in ChartImage::ALL.into_iter().zip(images) {
            let (Some(f), Some((origin, _))) = (image, which.slot(side)) else {
                continue;
            };
            let ((), refused) = skin_atlas::scoped(self.gpu.device(), || {
                for ((k, bytes), level) in (0_u32..).zip(f.levels()).zip(0..) {
                    let Some(size) = f.level_size(level) else {
                        continue;
                    };
                    skin_atlas::write_region(
                        self.gpu.queue(),
                        res.atlas.texture(),
                        (at, k),
                        origin.map(|o| o >> k),
                        size,
                        bytes,
                    );
                }
                #[cfg(test)]
                if let Some(fault) = self.chart_fault.take() {
                    fault(&self.gpu);
                }
                self.gpu.queue().submit([]);
            });
            let waited = self.wait_chart();
            if waited.is_err() || self.gpu.lost() {
                return Err(waited.err().unwrap_or(RenderError::DeviceLost));
            }
            if let Some(e) = refused {
                pending.chart = Some(ChartTable::ABSENT);
                return Err(RenderError::Atlas(UploadError::Device(e)));
            }
        }
        // 3. The staging is the one written into: it was held out for the
        // write. 4. Held with it, keyed at its commit.
        let [stage, back] = uvs;
        let table = ChartTable::new(stage, back);
        pending.chart = Some(table);
        Ok(table)
    }

    /// Publishes an all-absent table into `target` with no write: the
    /// images are absent there, their fields false, and the presentation
    /// without them shows (S10-12 design §5.3, a terminal absence: a
    /// refused or failed read, or a target with no chart layer, which it
    /// needs none for).
    ///
    /// # Errors
    ///
    /// [`RenderError::NoChartLayer`] with no committed play installation,
    /// [`RenderError::DeviceLost`] once the device is lost, and
    /// [`RenderError::ChartChanged`] for a token that is not the
    /// staging's: nothing changes.
    pub fn publish_chart_absent(
        &mut self,
        target: ChartTarget,
    ) -> Result<skin_model::chart_image::ChartTable, RenderError> {
        use skin_model::chart_image::ChartTable;
        match target {
            ChartTarget::Committed => {
                if self.installed.is_none() {
                    return Err(RenderError::NoChartLayer);
                }
                let Some(key) = self.chart_key() else {
                    return Err(RenderError::DeviceLost);
                };
                let table = ChartTable::ABSENT.keyed(key);
                self.chart = Some(PublishedChart { key, table });
                self.sync_chart();
                Ok(table)
            }
            ChartTarget::Staged(token) => match self.pending.as_mut() {
                // A lost device's staging never commits a drawable table:
                // the absence is not published, as the committed target's
                // is not.
                Some(p) if p.token == token && self.gpu.lost() => Err(RenderError::DeviceLost),
                Some(p) if p.token == token => {
                    p.chart = Some(ChartTable::ABSENT);
                    Ok(ChartTable::ABSENT)
                }
                _ => Err(RenderError::ChartChanged),
            },
        }
    }

    /// Makes the chart images unavailable, the committed installation's and
    /// a staging's: a new chart was chosen, and nothing of the last one may
    /// draw for it (S10-12 design §5.3; the source disposes the last
    /// chart's images before it reads the next's). Its loading presentation
    /// then has none, and the committed attempt reads them absent.
    pub fn invalidate_chart_images(&mut self) {
        self.chart = None;
        if let Some(p) = self.pending.as_mut() {
            p.chart = None;
        }
        self.sync_chart();
    }

    /// Waits for the GPU between a chart publication's images.
    fn wait_chart(&mut self) -> Result<(), RenderError> {
        #[cfg(test)]
        {
            self.chart_waits += 1;
            if let Some(fault) = self.chart_wait_fault.take() {
                return Err(fault());
            }
        }
        wait_for_gpu(&self.gpu)
    }

    /// The chart images valid for the play installation in use: the
    /// published table, if it was written into the array in use (its
    /// installation's atlas, this resource generation) on a device not
    /// lost since.
    fn valid_chart(&self) -> Option<skin_model::chart_image::ChartTable> {
        let p = self.chart?;
        (self.chart_key() == Some(p.key)).then_some(p.table)
    }

    /// The live key of the play installation's chart layer: this renderer,
    /// the installation (its presentation's number), its atlas and the
    /// resource generation; `None` without an installation or once the
    /// device is lost. Frames are built against it
    /// ([`PlaySkin::set_chart_live`]), so a chart table drawn from any
    /// copy describes this very array.
    #[must_use]
    pub fn chart_key(&self) -> Option<skin_model::chart_image::ChartKey> {
        let installed = self.installed.as_ref()?;
        (!self.gpu.lost()).then(|| skin_model::chart_image::ChartKey {
            renderer: self.id,
            installation: installed.loading().id(),
            atlas: installed.bundle().atlas_id().0,
            generation: self.resources,
        })
    }

    /// Copies the valid chart images ([`valid_chart`](Self::valid_chart))
    /// into the play installation's loading presentation, `None` without
    /// them, and into the committed attempt, absent without them.
    fn sync_chart(&mut self) {
        let valid = self.valid_chart();
        if valid.is_none() {
            self.chart = None;
        }
        if let Some(installed) = &self.installed {
            installed.loading().set_chart_images(valid);
        }
        self.skin
            .set_chart_images(valid.unwrap_or(skin_model::chart_image::ChartTable::ABSENT));
    }

    /// The result kind's pages-only installation's pages, if it has one:
    /// what the render thread keeps to restore on a renderer built again.
    #[must_use]
    pub fn installed_pages(&self, kind: ScreenKind) -> Option<&Arc<PackedPages>> {
        (kind == ScreenKind::Result)
            .then_some(self.result_pages.as_ref())
            .flatten()
            .map(|p| &p.pages)
    }

    /// Disables the result kind for the rest of this renderer's life
    /// (design.md "Rendering and Skins", a screen skin refused on a healthy
    /// device): its installation, pages-only installation, staged lines
    /// and staging are dropped, and from now on its pages-only
    /// installations, stagings, commits and restores are refused
    /// ([`RenderError::ResultDisabled`], [`CommitError::ResultDisabled`]).
    /// Its owner keeps the run's register and disables a renderer built
    /// again before restoring anything.
    pub fn disable_result(&mut self) {
        self.result_disabled = true;
        self.remove(ScreenKind::Result);
    }

    /// Drops what the last commit replaced (a layout on the same array, or
    /// an installation or pages-only installation on another), outside the
    /// commit point: dropping device resources allocates. Returns the array
    /// it released, if any, so the owner can acknowledge it (the S10-09i
    /// design's §2.1 Q3) before another atlas build is considered. The next
    /// staging drains it too.
    pub fn drain_retired(&mut self) -> Option<AtlasId> {
        self.retired.take().and_then(|r| r.atlas_id())
    }

    /// Whether play's admission released the result's installation since
    /// the last call, which clears it: a staging of play that only the
    /// result's charge kept out releases it before anything else of the
    /// staging is done, so the release stands whether the staging then
    /// succeeds or not. The owner reconciles what it keeps to restore
    /// before its next frame or recovery.
    pub fn take_result_release(&mut self) -> bool {
        std::mem::take(&mut self.result_released)
    }

    /// Whether the result kind is disabled ([`disable_result`](Self::disable_result)).
    #[must_use]
    pub const fn result_disabled(&self) -> bool {
        self.result_disabled
    }

    /// The atlas identities the renderer holds for `kind`: its committed
    /// array's (an installation's, or the result's pages-only one's) and
    /// the staging's, if it has its own array or is a pages-only one.
    /// Allocates nothing.
    #[must_use]
    pub fn retained_atlases(&self, kind: ScreenKind) -> [Option<AtlasId>; 3] {
        let staged = self
            .pending
            .as_ref()
            .filter(|p| p.token.kind == kind)
            .and_then(|p| match (&p.res, &p.skin) {
                (PendingRes::Own(_), Some(skin)) => Some(skin.bundle().atlas_id()),
                (PendingRes::Pages(Some(r)), _) => Some(r.pages.id()),
                _ => None,
            });
        let retired = self.retired.as_ref().and_then(|r| match r {
            Retired::Slot(s) => (Self::screen(kind)
                == Self::screen(s.installation.bundle().skin().kind()))
            .then(|| s.installation.bundle().atlas_id()),
            Retired::Pages(p) => (kind == ScreenKind::Result).then(|| p.pages.id()),
            Retired::Layout { .. } => None,
        });
        [self.committed_atlas(kind), staged, retired]
    }

    /// The bytes `id`'s pages keep resident wherever the renderer holds
    /// them, counted once: their CPU pages ([`pages_bytes`]) and their
    /// atlas array on the GPU; 0 when it holds none.
    #[must_use]
    pub fn shared_bytes(&self, id: AtlasId) -> u64 {
        let mut cpu = None;
        let mut gpu = 0u64;
        for (i, c, g) in self.shared_holders().into_iter().flatten() {
            if i == id {
                cpu.get_or_insert(c);
                gpu = gpu.saturating_add(g);
            }
        }
        cpu.map_or(0, |c| c.saturating_add(gpu))
    }

    /// Every holder of pages the witness counts, in its order: play's
    /// installation, the staging, the result's installation and the
    /// result's pages-only installation, each with its pages' identity,
    /// their CPU bytes and their array's bytes on the GPU (0 when the
    /// array is another holder's).
    fn shared_holders(&self) -> [Option<(AtlasId, u64, u64)>; 5] {
        let result = Self::screen(ScreenKind::Result)
            .and_then(|i| self.screens.get(i))
            .and_then(Option::as_ref);
        let play = self.installed.as_ref().map(|i| {
            let b = i.bundle();
            (b.atlas_id(), b.shared_bytes(), self.atlas.bytes())
        });
        let staged = self.pending.as_ref().and_then(|p| {
            let gpu = match &p.res {
                PendingRes::Own(r) => r.atlas.bytes(),
                PendingRes::Pages(Some(r)) => r.atlas.bytes(),
                _ => 0,
            };
            match (&p.skin, &p.pages) {
                (Some(s), _) => Some((s.bundle().atlas_id(), s.bundle().shared_bytes(), gpu)),
                (None, Some(pages)) => Some((pages.id(), pages_bytes(pages), gpu)),
                (None, None) => None,
            }
        });
        let slot = result.map(|s| {
            let b = s.installation.bundle();
            (b.atlas_id(), b.shared_bytes(), s.resources.atlas.bytes())
        });
        let pages = self
            .result_pages
            .as_ref()
            .map(|p| (p.pages.id(), pages_bytes(&p.pages), p.atlas.bytes()));
        let retired = self.retired.as_ref().and_then(|r| match r {
            Retired::Slot(s) => {
                let b = s.installation.bundle();
                Some((b.atlas_id(), b.shared_bytes(), s.resources.atlas.bytes()))
            }
            Retired::Pages(p) => Some((p.pages.id(), pages_bytes(&p.pages), p.atlas.bytes())),
            Retired::Layout { .. } => None,
        });
        [play, staged, slot, pages, retired]
    }

    /// The bytes held for the play skin now, the staged installation's
    /// included ([`MemoryWitness`]). The staged bundle's CPU description
    /// is counted unless it is play's installed bundle, and its loading
    /// presentation unless it is play's installed presentation: another
    /// kind's staging of play's bundle shares its pages but has its own
    /// presentation.
    #[must_use]
    pub fn memory(&self) -> MemoryWitness {
        self.memory_with(None)
    }

    /// [`memory`](Self::memory) with `selection`, the selection skin's
    /// state its owner keeps beside the renderer (the render thread, for
    /// the screen's lifetime), counted.
    #[must_use]
    pub fn memory_with(&self, selection: Option<&Selection>) -> MemoryWitness {
        let bytes = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        // A text layer's cache with the lines its holders keep.
        let cache = |c: &TextCache, held: &[&Holder]| {
            held.iter()
                .fold(c.bytes(), |sum, h| sum.saturating_add(bytes(h.bytes())))
        };
        let selection_text = Self::screen(ScreenKind::Selection)
            .and_then(|i| self.screens.get(i))
            .and_then(Option::as_ref)
            .map_or(0, |slot| slot.text.bytes());
        let result_slot = Self::screen(ScreenKind::Result)
            .and_then(|i| self.screens.get(i))
            .and_then(Option::as_ref);
        let result_installation = result_slot.map(|s| &s.installation);
        let retired_slot = match &self.retired {
            Some(Retired::Slot(s)) => Some(s),
            _ => None,
        };
        let installed = self.installed.as_ref();
        let pending = self.pending.as_ref();
        let staged = pending.and_then(|p| p.skin.as_ref().map(DeclSkin::installation));
        // The staging's own resources, by what it holds: (its array, its
        // upload, its layout, its text cache).
        let (staged_atlas, upload, staged_batch, staged_cache) =
            pending.map_or((0, 0, 0, 0), |p| match &p.res {
                PendingRes::Own(r) => (
                    r.atlas.bytes(),
                    r.atlas.upload_bytes(),
                    r.staging.bytes(),
                    r.text_cache.bytes(),
                ),
                PendingRes::Pages(Some(r)) => (
                    r.atlas.bytes(),
                    r.atlas.upload_bytes(),
                    0,
                    r.text_cache.bytes(),
                ),
                PendingRes::Layout { staging, .. } => (0, 0, staging.bytes(), 0),
                PendingRes::Installed | PendingRes::Pages(None) => (0, 0, 0, 0),
            });
        // Each committed bundle's own bytes are counted with it: the staged
        // one's only when it is none of them.
        let committed = [installed, result_installation];
        let staged_cpu = staged.as_ref().filter(|s| {
            !committed
                .into_iter()
                .flatten()
                .any(|i| Arc::ptr_eq(i.bundle(), s.bundle()))
        });
        // Each distinct loading presentation once: play's, the result's,
        // a retired installation's, then the staging's.
        let staged_loading = staged.as_ref().filter(|s| {
            ![
                installed,
                result_installation,
                retired_slot.map(|r| &r.installation),
            ]
            .into_iter()
            .flatten()
            .any(|i| i.loading().same(s.loading()))
        });
        let result_loading = result_installation
            .map_or(0, |i| i.loading().bytes())
            .saturating_add(
                retired_slot
                    .map(|r| &r.installation)
                    .filter(|r| !result_installation.is_some_and(|i| i.loading().same(r.loading())))
                    .map_or(0, |r| r.loading().bytes()),
            );
        // The pages each distinct identity keeps, once; an array on the
        // GPU is counted with its holder's resources.
        let mut seen: [Option<AtlasId>; 5] = [None; 5];
        let mut shared_cpu = 0u64;
        for (k, (id, cpu, _)) in self.shared_holders().into_iter().flatten().enumerate() {
            if !seen.contains(&Some(id)) {
                shared_cpu = shared_cpu.saturating_add(cpu);
            }
            if let Some(s) = seen.get_mut(k) {
                *s = Some(id);
            }
        }
        // The default font, shared by every producer, once: any bundle
        // still held, a retired one's included, keeps it.
        let default_font = [
            installed.map(Installation::bundle),
            staged.as_ref().map(Installation::bundle),
            result_installation.map(Installation::bundle),
            retired_slot.map(|r| r.installation.bundle()),
        ]
        .into_iter()
        .flatten()
        .filter_map(|b| b.text_producer())
        .map(|t| t.resident().default_font)
        .max()
        .unwrap_or(0);
        let retired = self.retired.as_ref().map_or(0, |r| match r {
            Retired::Layout { staging, .. } => staging.bytes(),
            Retired::Slot(s) => s
                .resources
                .atlas
                .bytes()
                .saturating_add(s.resources.staging.bytes())
                .saturating_add(cache(&s.resources.text_cache, &[&s.text]))
                .saturating_add(s.installation.bundle().resident_bytes()),
            Retired::Pages(p) => p.atlas.bytes().saturating_add(p.text_cache.bytes()),
        });
        let staged_result_text = self.staged_result_text.as_ref().map(|(_, h)| h);
        MemoryWitness {
            default_font,
            text_scratch: self.text_scratch.bytes(),
            text_mask: self.text_scratch.mask_bound(),
            text_upload: self.text_report.upload_bytes,
            text_cache: cache(
                &self.text_cache,
                &[&self.installed_text, &self.installed_loading],
            ),
            staged_text_cache: staged_cache
                .saturating_add(pending.map_or(0, |p| bytes(p.text.bytes()))),
            committed_cpu: installed.map_or(0, |i| i.bundle().resident_bytes()),
            staged_cpu: staged_cpu.map_or(0, |s| s.bundle().resident_bytes()),
            loading: installed.map_or(0, |i| i.loading().bytes()),
            staged_loading: staged_loading.map_or(0, |s| s.loading().bytes()),
            staged_text: self.staged_text.as_ref().map_or(0, StagedText::bytes),
            atlas: self.atlas.bytes(),
            staged_atlas,
            upload,
            batch: self.staging.bytes(),
            staged_batch,
            attempt: self.skin.attempt_bytes(),
            staged_attempt: pending
                .and_then(|p| p.skin.as_ref())
                .map_or(0, DeclSkin::attempt_bytes),
            selection_text: bytes(selection_text),
            selection_state: selection.map_or(0, Selection::resident_bytes),
            shared_cpu,
            result_cpu: result_installation.map_or(0, |i| i.bundle().resident_bytes()),
            result_batch: result_slot.map_or(0, |s| s.resources.staging.bytes()),
            result_atlas: result_slot.map_or_else(
                || self.result_pages.as_ref().map_or(0, |p| p.atlas.bytes()),
                |s| s.resources.atlas.bytes(),
            ),
            result_text: result_slot
                .map_or(0, |s| cache(&s.resources.text_cache, &[&s.text]))
                .saturating_add(
                    self.result_pages
                        .as_ref()
                        .map_or(0, |p| p.text_cache.bytes()),
                )
                .saturating_add(staged_result_text.map_or(0, |h| bytes(h.bytes()))),
            result_loading,
            retired,
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

    /// The play's READY candidate was rearmed (design.md "Play Lifecycle",
    /// the start gate): the skin's runtime state for the attempt starts
    /// over ([`PlaySkin::rearm`]), its chart, text, atlas and clips kept.
    /// Nothing allocates.
    pub fn rearm(&mut self) {
        self.skin.rearm();
    }

    /// What the play skin in use's attempt gives the next attempt of its
    /// play state ([`PlaySkin::carry`]).
    #[must_use]
    pub fn carry(&self) -> Option<skin_model::Carry<'_>> {
        self.skin.carry()
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
        let live = self.chart_key();
        self.skin.set_chart_live(live);
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

    /// Draws the play screen while a chart loads before READY, as
    /// [`frame`](Self::frame) draws: the skin's loading frame `f`
    /// ([`enter_loading`](Self::enter_loading), then
    /// [`build_loading`](Self::build_loading)), then in a pass of its own
    /// the text last prepared on `text`, the load's lines. Before
    /// recording opens only: no play frame draws text this way.
    ///
    /// # Errors
    ///
    /// The outer error as [`menu_frame`](Self::menu_frame): the device or
    /// the surface failed. The inner one is the skin's refusal of the
    /// loading frame: nothing of it, its lines included, is drawn or
    /// submitted, the renderer stays usable, and no loading frame of that
    /// lifetime is built again.
    pub fn loading_frame(
        &mut self,
        f: &LoadingFrame<'_>,
        text: &MenuText,
    ) -> Result<Result<FrameTiming, BuildError>, RenderError> {
        if text.owner() != self.id {
            return Err(RenderError::OtherRenderersText);
        }
        let start = Instant::now();
        self.enter_loading(f.lifetime);
        if let Err(e) = self.build_loading(f) {
            return Ok(Err(e));
        }
        let projection = self.staging.projection();
        self.draw(start, Some(text), projection).map(Ok)
    }

    /// Loading lifetime `lifetime` draws next, between frames: a skin's
    /// loading state of another lifetime starts afresh
    /// ([`PlaySkin::enter_loading`]), in place, and a refusal of
    /// another lifetime is forgotten.
    pub fn enter_loading(&mut self, lifetime: u64) {
        if self.loading_refused.is_some_and(|(l, _)| l != lifetime) {
            self.loading_refused = None;
        }
        self.skin.enter_loading(lifetime);
    }

    /// The app's share of a loading frame: the skin builds `f`
    /// ([`Staging::build_loading`]) at the applied target, with no call
    /// into wgpu; nothing allocates, the refusal included.
    ///
    /// # Errors
    ///
    /// The skin's refusal of this frame, or of an earlier one of its
    /// lifetime: the staging is left empty and the skin is not called
    /// again for that lifetime.
    pub fn build_loading(&mut self, f: &LoadingFrame<'_>) -> Result<(), BuildError> {
        // Whatever is staged now, a play frame built before it is not
        // submitted.
        self.started = None;
        if let Some(e) = self.loading_refused(f.lifetime) {
            self.staging.fill(|_| {});
            return Err(e);
        }
        let f = LoadingFrame {
            target: self.target(),
            ..*f
        };
        let live = self.chart_key();
        self.skin.set_chart_live(live);
        if let Err(e) = self.staging.build_loading(self.skin.as_mut(), &f) {
            self.loading_refused = Some((f.lifetime, e));
            return Err(e);
        }
        Ok(())
    }

    /// What loading text laid out for the installation in use now is
    /// checked against; `None` without a declarative installation.
    #[must_use]
    pub fn loading_text_key(&self) -> Option<LoadingTextKey> {
        let installed = self.installed.as_ref()?;
        Some(LoadingTextKey {
            installation: installed.loading().id(),
            device: self.id,
            resources: self.resources,
        })
    }

    /// Installs `table`'s lines into the installed presentation's slots
    /// `which` names, from its next loading frame on, if `key` is still
    /// the installation's ([`loading_text_key`](Self::loading_text_key)):
    /// text prepared for another installation, or on resources since
    /// replaced, never draws.
    ///
    /// # Errors
    ///
    /// [`LoadingTextError::Stale`] for another key, and
    /// [`LoadingTextError::Table`] for a table that does not fit the
    /// skin's slots: nothing changes.
    pub fn install_loading_text(
        &mut self,
        key: LoadingTextKey,
        which: TextSlots,
        table: &TextTable,
    ) -> Result<(), LoadingTextError> {
        if self.loading_text_key() != Some(key) {
            return Err(LoadingTextError::Stale);
        }
        let Some(installed) = self.installed.as_ref() else {
            return Err(LoadingTextError::Stale);
        };
        installed
            .loading()
            .install_text(installed.bundle(), which, table)?;
        // Those slots no longer draw the text layer's lines.
        self.installed_loading
            .release_named(installed.bundle().skin(), which);
        Ok(())
    }

    /// Prepares the chosen chart's text `chart` of `revision` for loading
    /// lifetime `lifetime`, the slots `which` names, held staged until
    /// [`install_staged_loading_text`](Self::install_staged_loading_text),
    /// replacing text staged before: at most one preparation waits. The
    /// rasterized slots' lines are placed in the installed resources' text
    /// layer and uploaded through the bundle's producer, as an attempt's
    /// are ([`stage`](Self::stage)), never writing to or freeing a line the
    /// installed attempt, the attempt staged on these resources or the
    /// installed loading lines hold; the staged text holds its own until
    /// it is installed or dropped. The bitmap-font lines are laid out by
    /// the installation's skin. Before recording opens and outside a
    /// frame's build, such as in a loading frame's call before it builds:
    /// it allocates and waits for the GPU. Nothing is staged without a
    /// declarative installation; without a producer the staged text clears
    /// the rasterized slots.
    ///
    /// # Errors
    ///
    /// As an attempt's text ([`stage`](Self::stage)): the producer's
    /// refusal of a line ([`RenderError::Skin`]), the device's refusal of
    /// a write ([`RenderError::Atlas`]), both refusals
    /// ([`RenderError::is_refusal`]), or the device failing while it is
    /// waited for. Nothing is staged, and what was installed stays.
    pub fn prepare_loading_text(
        &mut self,
        lifetime: u64,
        revision: u64,
        which: TextSlots,
        chart: &ChartText<'_>,
    ) -> Result<(), RenderError> {
        self.staged_text = None;
        let (Some(key), Some(installed)) = (self.loading_text_key(), self.installed.as_ref())
        else {
            return Ok(());
        };
        let bundle = Arc::clone(installed.bundle());
        let skin = bundle.skin();
        let (table, holder) = match bundle.text_producer() {
            Some(producer) => {
                // A failure leaves no earlier preparation's report.
                self.text_report = TextReport::default();
                let mut requests = skin.text_requests(chart, self.target(), &Layout::AUTHOR);
                requests.retain(|r| skin.names_text_slot(which, r.slot));
                // The attempt staged on these resources keeps its lines.
                let pending = self
                    .pending
                    .as_ref()
                    .filter(|p| {
                        p.token.kind == ScreenKind::Play && matches!(p.res, PendingRes::Installed)
                    })
                    .map(|p| &p.text);
                let held = [
                    Some(&self.installed_text),
                    pending,
                    Some(&self.installed_loading),
                ];
                let held: Vec<&Holder> = held.into_iter().flatten().collect();
                let (table, holder, report) = text_upload::prepare(
                    text_upload::Prepare {
                        gpu: &self.gpu,
                        atlas: &self.atlas,
                        cache: &mut self.text_cache,
                        scratch: &mut self.text_scratch,
                        producer: producer.as_ref(),
                        scope: bundle.text_scope(),
                        held: &held,
                        #[cfg(test)]
                        hooks: &mut self.text_hooks,
                        #[cfg(feature = "dev-tools")]
                        wait_fault: &mut self.text_wait_fault,
                    },
                    &requests,
                    skin.text_slots(),
                )?;
                self.text_report = report;
                (Some(table), holder)
            }
            None => (None, Holder::default()),
        };
        let bitmap = skin.loading_bitmap_lines(which, chart);
        self.staged_text = Some(StagedText {
            key,
            lifetime,
            revision,
            which,
            table,
            bitmap,
            holder,
        });
        Ok(())
    }

    /// Development builds: the last frame's list, if its skin keeps one.
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn display_list(&self) -> Option<skin_model::DisplayList> {
        self.skin
            .display_list(self.staging.batch().quads(), self.staging.clips())
    }

    /// Development builds: the next preparation of rasterized text that
    /// uploads fails waiting for the GPU as a failing device does
    /// ([`RenderError::Poll`], not a refusal), its cache closed until the
    /// renderer is built again.
    #[cfg(feature = "dev-tools")]
    pub fn fail_next_text_wait(&mut self) {
        self.text_wait_fault = true;
    }

    /// The loading lifetime and revision of the text staged, if any.
    #[must_use]
    pub fn staged_loading_text(&self) -> Option<(u64, u64)> {
        self.staged_text.as_ref().map(|t| (t.lifetime, t.revision))
    }

    /// Installs the text staged, taking it, from the next loading frame
    /// on, if it is still the installation's (its key), of the lifetime
    /// its presentation is in, and no older than the revision installed:
    /// text of another bundle, renderer, resources, lifetime or an older
    /// revision is dropped. `Ok(false)` with nothing staged.
    ///
    /// # Errors
    ///
    /// Why the staged text was dropped.
    pub fn install_staged_loading_text(&mut self) -> Result<bool, LoadingTextError> {
        let Some(staged) = self.staged_text.take() else {
            return Ok(false);
        };
        if self.loading_text_key() != Some(staged.key) {
            return Err(LoadingTextError::Stale);
        }
        let Some(installed) = self.installed.as_ref() else {
            return Err(LoadingTextError::Stale);
        };
        installed.loading().install_revision(
            installed.bundle(),
            staged.lifetime,
            staged.revision,
            staged.which,
            staged.table.as_ref(),
            staged.bitmap,
        )?;
        // Its slots draw its lines now, and no longer the ones before.
        self.installed_loading
            .release_named(installed.bundle().skin(), staged.which);
        self.installed_loading.extend(staged.holder);
        Ok(true)
    }

    /// Clears the installed presentation's slots `which` names, so they
    /// draw nothing from its next loading frame until text is installed
    /// for them again, and lets go of their lines in the text layer; no
    /// timer changes. Nothing without a declarative installation.
    pub fn clear_loading_text(&mut self, which: TextSlots) {
        if let Some(installed) = &self.installed {
            installed.loading().clear_text(installed.bundle(), which);
            self.installed_loading
                .release_named(installed.bundle().skin(), which);
        }
    }

    /// The loading refusal kept for lifetime `lifetime`, if any.
    #[must_use]
    pub fn loading_refused(&self, lifetime: u64) -> Option<BuildError> {
        self.loading_refused
            .filter(|(l, _)| *l == lifetime)
            .map(|(_, e)| e)
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
        let start = self.build_menu(sprites);
        self.submit_menu(start, text)
    }

    /// The app's share of a menu frame: the sprites `sprites` pushes,
    /// staged with no call into wgpu, allocating nothing of its own.
    /// Returns when the frame's work began, for
    /// [`submit_menu`](Self::submit_menu).
    pub fn build_menu(&mut self, sprites: impl FnOnce(&mut SpriteBatch)) -> Instant {
        let start = Instant::now();
        self.staging.fill(sprites);
        start
    }

    /// Submits and presents the menu frame
    /// [`build_menu`](Self::build_menu) staged at `start`, with `text`
    /// over it.
    ///
    /// # Errors
    ///
    /// As [`menu_frame`](Self::menu_frame).
    pub fn submit_menu(
        &mut self,
        start: Instant,
        text: &MenuText,
    ) -> Result<FrameTiming, RenderError> {
        if text.owner() != self.id {
            return Err(RenderError::OtherRenderersText);
        }
        self.draw(start, Some(text), Projection::fixed(self.target()))
    }

    /// The selection screen's committed bundle, if any: what its frames
    /// draw ([`build_selection`](Self::build_selection)).
    #[must_use]
    pub fn selection_bundle(&self) -> Option<&Arc<SkinBundle>> {
        self.committed_installation(ScreenKind::Selection)
            .map(Installation::bundle)
    }

    /// Prepares `sel`'s lines, as its last view and labels left them
    /// ([`Skin::update_selection`](skin_model::Skin::update_selection)),
    /// in the selection installation's text layer, and attaches them with
    /// its bitmap-font lines. Lines already in the layer cost no raster;
    /// the lines the selection drew before are let go once the new ones
    /// are placed, never before, so a refusal leaves them drawing. On a
    /// view change outside a frame's build: it allocates and waits for
    /// the GPU. Nothing without a selection installation.
    ///
    /// # Errors
    ///
    /// As [`prepare_loading_text`](Self::prepare_loading_text): a refusal
    /// of a line or a write, or of the tables
    /// ([`RenderError::Skin`]), both refusals
    /// ([`RenderError::is_refusal`]), or the device failing while it is
    /// waited for.
    pub fn prepare_selection_text(&mut self, sel: &mut Selection) -> Result<(), RenderError> {
        let target = self.target();
        #[cfg(feature = "dev-tools")]
        if std::mem::take(&mut self.selection_faults.1) {
            return Err(RenderError::Skin(SkinError {
                reason: "the selection text was refused (development fault)".to_owned(),
            }));
        }
        let Some(Some(slot)) =
            Self::screen(ScreenKind::Selection).and_then(|i| self.screens.get_mut(i))
        else {
            return Ok(());
        };
        let bundle = Arc::clone(slot.installation.bundle());
        let skin = bundle.skin();
        let requests = skin.selection_text_requests(sel, target, &Layout::AUTHOR);
        #[cfg(feature = "dev-tools")]
        {
            self.selection_requests = requests
                .texts
                .iter()
                .chain(&requests.rows)
                .map(|r| r.text.clone())
                .collect();
        }
        let refused = |e: TextTableError| {
            RenderError::Skin(SkinError {
                reason: e.to_string(),
            })
        };
        if let Some(producer) = bundle.text_producer() {
            self.text_report = TextReport::default();
            let (texts, held, report) = text_upload::prepare(
                text_upload::Prepare {
                    gpu: &self.gpu,
                    atlas: &slot.resources.atlas,
                    cache: &mut slot.resources.text_cache,
                    scratch: &mut self.text_scratch,
                    producer: producer.as_ref(),
                    scope: bundle.text_scope(),
                    held: &[&slot.text],
                    #[cfg(test)]
                    hooks: &mut self.text_hooks,
                    #[cfg(feature = "dev-tools")]
                    wait_fault: &mut self.text_wait_fault,
                },
                &requests.texts,
                skin.text_slots(),
            )?;
            let (rows, row_held, rows_report) = text_upload::prepare(
                text_upload::Prepare {
                    gpu: &self.gpu,
                    atlas: &slot.resources.atlas,
                    cache: &mut slot.resources.text_cache,
                    scratch: &mut self.text_scratch,
                    producer: producer.as_ref(),
                    scope: bundle.text_scope(),
                    held: &[&slot.text, &held],
                    #[cfg(test)]
                    hooks: &mut self.text_hooks,
                    #[cfg(feature = "dev-tools")]
                    wait_fault: &mut self.text_wait_fault,
                },
                &requests.rows,
                skin.song_list_slots(),
            )?;
            #[cfg(feature = "dev-tools")]
            {
                let placed = report
                    .counts
                    .placements
                    .saturating_add(rows_report.counts.placements);
                self.selection_rasterized = usize::try_from(placed).unwrap_or(usize::MAX);
            }
            self.text_report = rows_report;
            let _ = report;
            skin.attach_selection_text(sel, texts, rows)
                .map_err(refused)?;
            slot.text = held;
            slot.text.extend(row_held);
        } else {
            let empty = |n: usize| TextTable::new(vec![None; n]);
            skin.attach_selection_text(
                sel,
                empty(skin.text_slots()),
                empty(skin.song_list_slots()),
            )
            .map_err(refused)?;
            slot.text = Holder::default();
        }
        skin.attach_selection_bitmap_text(sel);
        Ok(())
    }

    /// The app's share of a selection frame: the selection installation's
    /// skin builds `f` of `sel` into its slot's staging, and `strip`, in
    /// layout pixels (1920×1080 over the whole target), follows from the
    /// reserve the skin cannot fill, with no call into wgpu. Nothing
    /// allocates, the refusal included. At most [`STRIP_SPRITES`] of the
    /// strip are drawn.
    ///
    /// # Errors
    ///
    /// The skin's refusal of the frame, or [`BuildError::Kind`] without a
    /// selection installation: nothing of it is staged.
    pub fn build_selection(
        &mut self,
        sel: &mut Selection,
        f: &SelectionFrame<'_>,
        strip: &[Sprite],
    ) -> Result<(), BuildError> {
        self.started = None;
        let start = Instant::now();
        #[cfg(feature = "dev-tools")]
        BUILD_START.with(|b| b.set(Some(start)));
        let target = self.target();
        #[cfg(feature = "dev-tools")]
        if std::mem::take(&mut self.selection_faults.0) {
            return Err(BuildError::Kind);
        }
        let Some(Some(slot)) =
            Self::screen(ScreenKind::Selection).and_then(|i| self.screens.get_mut(i))
        else {
            return Err(BuildError::Kind);
        };
        let f = SelectionFrame { target, ..*f };
        let (placed, n) =
            placed_strip::<STRIP_SPRITES>(strip, &slot.resources.staging.projection(), target);
        let bundle = slot.installation.bundle();
        let uvs = bundle.uvs();
        let skin = bundle.skin();
        slot.resources.staging.build_with(
            |quads| skin.build_selection(sel, &f, uvs, quads),
            placed.get(..n).unwrap_or_default(),
        )?;
        self.started = Some(start);
        Ok(())
    }

    /// Submits and presents the selection frame
    /// [`build_selection`](Self::build_selection) staged, then each of
    /// `texts` in a pass of its own over it (the strip's lines). Without a
    /// frame built since, nothing is acquired or submitted, as
    /// [`submit`](Self::submit).
    ///
    /// # Errors
    ///
    /// As [`menu_frame`](Self::menu_frame).
    pub fn submit_selection(&mut self, texts: &[&MenuText]) -> Result<FrameTiming, RenderError> {
        self.submit_screen(ScreenKind::Selection, texts)
    }

    /// Submits and presents the frame of `kind`'s slot staged last, then
    /// each of `texts` in a pass of its own over it. Without a frame built
    /// since, or without the slot, nothing is acquired or submitted.
    fn submit_screen(
        &mut self,
        kind: ScreenKind,
        texts: &[&MenuText],
    ) -> Result<FrameTiming, RenderError> {
        if texts.iter().any(|t| t.owner() != self.id) {
            return Err(RenderError::OtherRenderersText);
        }
        let screen = Self::screen(kind);
        let none = FrameTiming {
            cpu: Duration::ZERO,
            sprites: 0,
            dropped: 0,
            presented: false,
            present_count: None,
        };
        let Some(start) = self.started.take() else {
            return Ok(none);
        };
        let Some(Some(slot)) = screen.and_then(|i| self.screens.get(i)) else {
            return Ok(none);
        };
        let projection = slot.resources.staging.projection();
        self.draw_on(start, texts, projection, screen)
    }

    /// Development builds: the next selection frame is refused
    /// ([`build_selection`](Self::build_selection)) as a skin refuses one,
    /// or with `text` the next selection text preparation.
    #[cfg(feature = "dev-tools")]
    pub fn refuse_next_selection(&mut self, text: bool) {
        if text {
            self.selection_faults.1 = true;
        } else {
            self.selection_faults.0 = true;
        }
    }

    /// Development builds: the selection frame last built, its quads in
    /// drawing order, the strip's included; `None` without a selection
    /// installation.
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn selection_quads(&self) -> Option<Vec<skin_model::Quad>> {
        let i = Self::screen(ScreenKind::Selection)?;
        let slot = self.screens.get(i)?.as_ref()?;
        Some(slot.resources.staging.batch().quads().quads().to_vec())
    }

    /// Development builds: what the selection frame last built observed
    /// of the skin's elements, in drawing order; `None` without a
    /// selection installation.
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn selection_observations(&self) -> Option<Vec<skin_model::ElementObservation>> {
        let i = Self::screen(ScreenKind::Selection)?;
        let slot = self.screens.get(i)?.as_ref()?;
        Some(
            slot.resources
                .staging
                .batch()
                .quads()
                .observations()
                .to_vec(),
        )
    }

    /// Development builds: the lines the last
    /// [`prepare_selection_text`](Self::prepare_selection_text) asked
    /// for, the text elements' then the rows', and how many it
    /// rasterized.
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn selection_requests(&self) -> (&[String], usize) {
        (&self.selection_requests, self.selection_rasterized)
    }

    /// The result screen's committed bundle, if any: what its frames draw
    /// ([`build_result`](Self::build_result)).
    #[must_use]
    pub fn result_bundle(&self) -> Option<&Arc<SkinBundle>> {
        self.committed_installation(ScreenKind::Result)
            .map(Installation::bundle)
    }

    /// Prepares `res`'s lines, as its last view and labels left them
    /// ([`Skin::update_result`](skin_model::Skin::update_result)), for the
    /// result bundle staged, or else the committed one, in the text layer
    /// of the array it draws from, and attaches them with its bitmap-font
    /// lines to `res`. The lines are staged, not drawn: the frames go on
    /// drawing the lines installed, held meanwhile, until
    /// [`install_staged_result_text`](Self::install_staged_result_text)
    /// installs these. Lines already in the layer cost no raster. For a
    /// presentation's preparation outside a frame's build: it allocates
    /// and waits for the GPU. Nothing without a result layout.
    ///
    /// # Errors
    ///
    /// As [`prepare_selection_text`](Self::prepare_selection_text).
    pub fn prepare_result_text(&mut self, res: &mut ResultScreen) -> Result<(), RenderError> {
        #[cfg(any(test, feature = "dev-tools"))]
        if std::mem::take(&mut self.result_faults.1) {
            return Err(RenderError::Skin(SkinError {
                reason: "the result text was refused (development fault)".to_owned(),
            }));
        }
        let target = self.target();
        let pending = self
            .pending
            .as_mut()
            .filter(|p| p.token.kind == ScreenKind::Result);
        let staged = pending
            .as_ref()
            .and_then(|p| p.skin.as_ref())
            .map(|s| Arc::clone(s.bundle()));
        let slot = Self::screen(ScreenKind::Result)
            .and_then(|i| self.screens.get_mut(i))
            .and_then(Option::as_mut);
        let Some(bundle) =
            staged.or_else(|| slot.as_ref().map(|s| Arc::clone(s.installation.bundle())))
        else {
            return Ok(());
        };
        // Where the lines go: the staging's own array, else the installed
        // one, which a layout staged on the same pages draws from too, else
        // the pages-only one.
        let (atlas, cache, held) = match (
            pending.map(|p| &mut p.res),
            slot,
            self.result_pages.as_mut(),
        ) {
            (Some(PendingRes::Own(r)), _, _) => (&r.atlas, &mut r.text_cache, None),
            (_, Some(s), _) => {
                let Slot {
                    resources, text, ..
                } = s;
                (&resources.atlas, &mut resources.text_cache, Some(&*text))
            }
            (_, None, Some(p)) => (&p.atlas, &mut p.text_cache, None),
            _ => return Ok(()),
        };
        let skin = bundle.skin();
        let refused = |e: TextTableError| {
            RenderError::Skin(SkinError {
                reason: e.to_string(),
            })
        };
        let holder = match bundle.text_producer() {
            Some(producer) => {
                self.text_report = TextReport::default();
                let requests = skin.result_text_requests(res, target, &Layout::AUTHOR);
                let (table, holder, report) = text_upload::prepare(
                    text_upload::Prepare {
                        gpu: &self.gpu,
                        atlas,
                        cache,
                        scratch: &mut self.text_scratch,
                        producer: producer.as_ref(),
                        scope: bundle.text_scope(),
                        held: held.as_slice(),
                        #[cfg(test)]
                        hooks: &mut self.text_hooks,
                        #[cfg(feature = "dev-tools")]
                        wait_fault: &mut self.text_wait_fault,
                    },
                    &requests,
                    skin.text_slots(),
                )?;
                self.text_report = report;
                skin.attach_result_text(res, table).map_err(refused)?;
                holder
            }
            None => {
                skin.attach_result_text(res, TextTable::new(vec![None; skin.text_slots()]))
                    .map_err(refused)?;
                Holder::default()
            }
        };
        skin.attach_result_bitmap_text(res);
        self.staged_result_text = Some((bundle.atlas_id(), holder));
        Ok(())
    }

    /// Installs the result's lines [`prepare_result_text`]
    /// (Self::prepare_result_text) staged, in place of the ones its frames
    /// draw, which are let go: the commit point of a presentation, between
    /// frames. Returns whether they were installed: not when none are
    /// staged, nor when the array they were placed in is no longer the
    /// result's (they are dropped). Allocates nothing.
    pub fn install_staged_result_text(&mut self) -> bool {
        let Some((id, holder)) = self.staged_result_text.take() else {
            return false;
        };
        match Self::screen(ScreenKind::Result)
            .and_then(|i| self.screens.get_mut(i))
            .and_then(Option::as_mut)
        {
            Some(slot) if slot.installation.bundle().atlas_id() == id => {
                slot.text = holder;
                true
            }
            _ => false,
        }
    }

    /// The app's share of a result frame: the result installation's skin
    /// builds `f` of `res` into its slot's staging, and `strip`, in layout
    /// pixels (1920×1080 over the whole target), follows from the reserve
    /// the skin cannot fill, with no call into wgpu. Nothing allocates,
    /// the refusal included. At most [`RESULT_STRIP_SPRITES`] of the strip
    /// are drawn.
    ///
    /// # Errors
    ///
    /// The skin's refusal of the frame, or [`BuildError::Kind`] without a
    /// result layout: nothing of it is staged.
    pub fn build_result(
        &mut self,
        res: &mut ResultScreen,
        f: &ResultFrame<'_>,
        strip: &[Sprite],
    ) -> Result<(), BuildError> {
        self.started = None;
        let start = Instant::now();
        #[cfg(feature = "dev-tools")]
        BUILD_START.with(|b| b.set(Some(start)));
        let target = self.target();
        #[cfg(any(test, feature = "dev-tools"))]
        if std::mem::take(&mut self.result_faults.0) {
            return Err(BuildError::Kind);
        }
        let Some(Some(slot)) =
            Self::screen(ScreenKind::Result).and_then(|i| self.screens.get_mut(i))
        else {
            return Err(BuildError::Kind);
        };
        let f = ResultFrame { target, ..*f };
        let (placed, n) = placed_strip::<RESULT_STRIP_SPRITES>(
            strip,
            &slot.resources.staging.projection(),
            target,
        );
        let bundle = slot.installation.bundle();
        let uvs = bundle.uvs();
        let skin = bundle.skin();
        slot.resources.staging.build_with(
            |quads| skin.build_result(res, &f, uvs, quads),
            placed.get(..n).unwrap_or_default(),
        )?;
        self.started = Some(start);
        Ok(())
    }

    /// Submits and presents the result frame
    /// [`build_result`](Self::build_result) staged, then each of `texts`
    /// in a pass of its own over it (the strip's lines). Without a frame
    /// built since, nothing is acquired or submitted, as
    /// [`submit`](Self::submit).
    ///
    /// # Errors
    ///
    /// As [`menu_frame`](Self::menu_frame).
    pub fn submit_result(&mut self, texts: &[&MenuText]) -> Result<FrameTiming, RenderError> {
        self.submit_screen(ScreenKind::Result, texts)
    }

    /// Tests and development builds: the next result frame is refused
    /// ([`build_result`](Self::build_result)) as a skin refuses one, or
    /// with `text` the next result text preparation.
    #[cfg(any(test, feature = "dev-tools"))]
    pub fn refuse_next_result(&mut self, text: bool) {
        if text {
            self.result_faults.1 = true;
        } else {
            self.result_faults.0 = true;
        }
    }

    /// Development builds: the result frame last built, the skin's quads
    /// in drawing order; `None` without a result layout.
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn result_quads(&self) -> Option<Vec<skin_model::Quad>> {
        let i = Self::screen(ScreenKind::Result)?;
        let slot = self.screens.get(i)?.as_ref()?;
        Some(slot.resources.staging.batch().quads().quads().to_vec())
    }

    /// Tests: the bytes of the result's committed text layer's cache.
    #[cfg(test)]
    pub(crate) fn result_text_cache_bytes(&self) -> u64 {
        Self::screen(ScreenKind::Result)
            .and_then(|i| self.screens.get(i))
            .and_then(Option::as_ref)
            .map_or(0, |s| s.resources.text_cache.bytes())
    }

    /// Tests: makes every staging's instance buffer invalid, as a device
    /// refusing its allocation would, while `on`.
    #[cfg(test)]
    pub(crate) fn fail_staged_buffers(&mut self, on: bool) {
        self.buffer_fault = on;
    }

    /// Tests and development builds: how many atlas arrays were uploaded,
    /// a skin's or a pages-only installation's.
    #[cfg(any(test, feature = "dev-tools"))]
    #[must_use]
    pub const fn atlas_uploads(&self) -> u64 {
        self.atlas_uploads
    }

    /// Tests and development builds: the timing of the atlas array upload
    /// since the last call, if one ran.
    #[cfg(any(test, feature = "dev-tools"))]
    pub const fn take_upload_timing(&mut self) -> Option<UploadTiming> {
        self.upload_timing.take()
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
        self.draw_on(start, text.as_slice(), projection, None)
    }

    /// [`draw`](Self::draw) of the staging, instances and atlas of
    /// `screen`'s slot (play's own with `None`), with each of `texts` in
    /// a pass of its own after the sprites.
    fn draw_on(
        &mut self,
        start: Instant,
        texts: &[&MenuText],
        projection: Projection,
        screen: Option<usize>,
    ) -> Result<FrameTiming, RenderError> {
        // Whatever is drawn now, a play frame built before it is not
        // submitted again.
        self.started = None;
        let (staging, instances, atlas_bind) = match screen
            .and_then(|i| self.screens.get(i))
            .and_then(Option::as_ref)
        {
            Some(slot) => (
                &slot.resources.staging,
                &slot.resources.instances,
                &slot.resources.atlas_bind,
            ),
            None => (&self.staging, &self.instances, &self.atlas_bind),
        };
        let sprites = staging.instances().len();
        let staged = u32::try_from(sprites).unwrap_or(u32::MAX);
        let timing = |cpu, presented| FrameTiming {
            cpu,
            sprites,
            dropped: staging.batch().dropped(),
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
        queue.write_buffer(instances, 0, bytemuck::cast_slice(staging.instances()));
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
            pass.set_bind_group(0, atlas_bind, &[]);
            pass.set_vertex_buffer(0, instances.slice(..));
            // The runs in the batch's order, so each sprite is drawn over
            // every sprite pushed before it, whatever their blends. Each run
            // sets its scissor, its fixed clip's intersected with its inline
            // clip rect's, when it differs from the one set: an unclipped
            // run sets the whole target back, and a run whose clips keep no
            // pixel is not drawn.
            let mut scissor = full;
            for run in staging.batch().runs() {
                let end = run.end.min(staged);
                if run.start >= end {
                    break;
                }
                let rect = match staging.run_scissor(run) {
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
                    Blend::Multiply => &self.multiply,
                });
                pass.draw(0..4, run.start..end);
            }
        }
        for text in texts {
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

/// Tests and development builds: one atlas array upload's timing (the
/// S10-09i design's §8.1 G1).
#[cfg(any(test, feature = "dev-tools"))]
#[derive(Clone, Copy, Debug)]
pub struct UploadTiming {
    /// From the upload's start to its submission: the texel writes.
    pub written: Duration,
    /// From the submission to the queue's report that the submitted work
    /// is done (`on_submitted_work_done`); `None` if the wait returned
    /// without the report.
    pub done: Option<Duration>,
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
pub(crate) fn wait_for_gpu(gpu: &Gpu) -> Result<(), RenderError> {
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

/// The alpha, additive and multiply sprite pipelines and their bindings.
/// All take premultiplied colour (design.md "Rendering and Skins"): alpha
/// blending is RGB source 1, destination 1 − source alpha; additive is RGB
/// source 1, destination 1; in both the alpha channel composites over.
/// Multiply's fragment unpremultiplies its colour, and blending is RGB
/// source 0, destination source colour, the alpha channel kept.
fn pipelines(
    gpu: &Gpu,
    format: wgpu::TextureFormat,
) -> (
    wgpu::RenderPipeline,
    wgpu::RenderPipeline,
    wgpu::RenderPipeline,
    Bindings,
) {
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
    let pipeline = |blend, fragment| {
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
                entry_point: Some(fragment),
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
    let alpha = pipeline(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING, "fs");
    // Colour source 1, destination 1; the alpha channel still over, not
    // `ADDITIVE`'s 1, 1.
    let additive = pipeline(
        wgpu::BlendState {
            alpha: wgpu::BlendComponent::OVER,
            ..wgpu::BlendState::ADDITIVE
        },
        "fs",
    );
    // Colour source 0, destination the source's straight colour
    // (`skin/Skin.java:663` at `0.8.9`); the destination's alpha kept.
    let multiply = pipeline(
        wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::Src,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        },
        "fs_multiply",
    );
    let bindings = Bindings {
        layout,
        smooth,
        nearest,
        projection,
        uploaded: None,
    };
    (alpha, additive, multiply, bindings)
}

/// The projection's uniform: half the canvas's size, padded to 16 bytes.
const PROJECTION_BYTES: u64 = 16;

#[cfg(test)]
mod tests {
    use std::error::Error;

    use wgpu::wgc::instance as core;

    use super::{
        AtlasPages, ChartOutcome, ChartTarget, ClipTable, RenderError, Renderer, Target, letterbox,
    };
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
        // It says what it was built with, and WARP renders on the CPU.
        assert_eq!(r.choice(), AdapterChoice::Warp);
        assert_eq!(r.gpu().adapter().device_type, wgpu::DeviceType::Cpu);
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

    /// Multiply takes the destination's RGB times the sprite's straight
    /// colour and keeps the destination's alpha (`skin/Skin.java:663` at
    /// `0.8.9`; O1 A, Q2). On a transparent target an alpha sprite lays
    /// RGB 0.8, 0.6, 0.4 at alpha 0.5 (stored 102, 77, 51 at 128) under
    /// four multiply sprites, at layout x 100, 300, 500 and 700 (target
    /// x 50, 150, 250 and 350 at half scale):
    /// - grey 0.5 at alpha 1 halves the RGB: 51, 38, 26;
    /// - grey 0.5 at alpha 0.25 halves it too, the tint unpremultiplied
    ///   (an eighth without it);
    /// - an uploaded texel of straight grey 0.5 at alpha 128 (stored 64 at
    ///   128) halves it: unpremultiplied by the product's alpha, not the
    ///   tint's alone (a quarter);
    /// - a transparent texel of the built-in atlas gives 0.
    ///
    /// The alpha stays 128 under all four: not 255 as over, nor scaled
    /// as `GL_SRC_COLOR` scales it. Colour factors 1, 0 would give the
    /// grey.
    #[test]
    fn multiply_takes_the_straight_colour_and_keeps_the_alpha() {
        let mut r = Renderer::new(
            Target::Offscreen {
                width: 960,
                height: 540,
            },
            AdapterChoice::Warp,
        )
        .expect("WARP renderer");
        r.clear = wgpu::Color::TRANSPARENT;
        let levels: Vec<Vec<u8>> = [128_usize, 64, 32, 16]
            .iter()
            .map(|s| [64, 64, 64, 128].repeat(s * s))
            .collect();
        let page: crate::PageLevels<'_> = [&levels[0], &levels[1], &levels[2], &levels[3]];
        let pages = AtlasPages {
            side: 128,
            pages: &[page],
            text_layer: false,
            chart_layer: false,
        };
        r.install_atlas(&pages, ClipTable::default()).unwrap();
        let sprite = |x, color| Sprite {
            rect: [x, 0.0, 200.0, 400.0],
            uv: solid_uv(),
            color,
            ..Sprite::default()
        };
        // Texel (4, 4) of layer 0: inside the space's empty cell.
        let empty = 4.5 / 128.0;
        let text = MenuText::new(&r);
        let timing = r
            .menu_frame(
                |b| {
                    let mut under = sprite(0.0, [0.8, 0.6, 0.4, 0.5]);
                    under.rect = [0.0, 0.0, 800.0, 400.0];
                    b.push(Blend::Alpha, under);
                    b.push(Blend::Multiply, sprite(0.0, [0.5, 0.5, 0.5, 1.0]));
                    b.push(Blend::Multiply, sprite(200.0, [0.5, 0.5, 0.5, 0.25]));
                    b.push(
                        Blend::Multiply,
                        Sprite {
                            uv: [0.5; 4],
                            layer: 1,
                            ..sprite(400.0, [1.0; 4])
                        },
                    );
                    b.push(
                        Blend::Multiply,
                        Sprite {
                            uv: [empty; 4],
                            ..sprite(600.0, [1.0; 4])
                        },
                    );
                },
                &text,
            )
            .unwrap();
        assert!(timing.presented);
        let pixels = r.read_back();
        let at = |x: usize| -> [u8; 4] {
            let i = (100 * 960 + x) * 4;
            pixels[i..i + 4].try_into().unwrap()
        };
        let near = |got: [u8; 4], want: [u8; 4], what: &str| {
            assert!(
                got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 2),
                "{what}: {got:?}, expected {want:?}"
            );
        };
        near(at(50), [51, 38, 26, 128], "opaque tint");
        near(at(150), [51, 38, 26, 128], "translucent tint");
        near(at(250), [51, 38, 26, 128], "translucent texel");
        near(at(350), [0, 0, 0, 128], "transparent texel");
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
        let at = |x: u32| {
            let [r, _, b] = crate::test_common::rgb_at(&p, 1_920, (x, 540));
            [r, b]
        };
        assert_eq!((at(480), at(1_440)), ([255, 0], [0, 0]));
    }

    /// Another kind's staging is projected at its commit onto the target as
    /// it is then: staged at 1 920×1 080 and committed after a resize to
    /// 960×540, its slot's viewport and clip scissor are the new size's,
    /// and a later resize projects the slot again.
    #[test]
    fn a_slot_is_projected_at_its_commit() {
        use crate::Scissor;
        use crate::test_common::offscreen;
        use crate::test_decl::{Look, bundle};
        use skin_model::{ClipId, ScreenKind};

        let slot = |r: &Renderer| {
            let i = Renderer::screen(ScreenKind::Selection).unwrap();
            let slot = r.screens.get(i).and_then(Option::as_ref).unwrap();
            (
                slot.resources.staging.projection(),
                slot.resources.staging.scissor(Some(ClipId(0))),
            )
        };
        let dir = tempfile::tempdir().unwrap();
        let mut r = offscreen(1_920, 1_080);
        let (green, _) = bundle(&r, dir.path(), &Look::left([0, 255, 0, 255]));
        let token = r.install(ScreenKind::Selection, &green, 1).unwrap();
        r.resize(960, 540);
        r.commit(token).unwrap();
        let (projection, scissor) = slot(&r);
        assert_eq!(projection.target(), [960, 540]);
        assert_eq!(projection.viewport(), [0.0, 0.0, 960.0, 540.0]);
        assert_eq!(scissor, Scissor::Rect([0, 0, 480, 540]));
        r.resize(1_920, 1_080);
        assert_eq!(slot(&r).1, Scissor::Rect([0, 0, 960, 1_080]));
    }

    /// A refusal after the texture exists, injected between the candidate's
    /// bind group and the upload's submission (a buffer whose usage mixes
    /// both map modes, which every device's validation refuses), is the
    /// typed `Atlas(Device)` error, not an uncaptured one that would panic,
    /// and the built-in atlas stays: 128 texels, one layer, so the solid
    /// block still draws white at layout (100, 100), target pixel (50, 50).
    /// The same pages without the fault are then installed: 256 texels,
    /// two layers.
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
            text_layer: false,
            chart_layer: false,
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

    /// The built-in atlas is read texel for texel on an array whose side,
    /// 384, is not a power of two: drawn whole at 1:1 at the top left, at
    /// 2:1 beside it, and at 1:2, where every pixel's centre falls on a
    /// texel edge (pixel `i` reads texel `2i + 1`), every target pixel is
    /// the built-in texel under it
    /// (white glyph texels opaque, the rest transparent over the black
    /// clear), as on the built-in atlas alone.
    #[test]
    fn the_built_in_atlas_reads_exactly_on_a_side_between_powers_of_two() {
        let mut r = Renderer::new(
            Target::Offscreen {
                width: 1_920,
                height: 1_080,
            },
            AdapterChoice::Warp,
        )
        .expect("WARP renderer");
        let draw = |r: &mut Renderer| {
            let text = MenuText::new(r);
            let whole = |rect| Sprite {
                rect,
                uv: [0.0, 0.0, 1.0, 1.0],
                color: [1.0; 4],
                ..Sprite::default()
            };
            let timing = r
                .menu_frame(
                    |b| {
                        b.push(Blend::Alpha, whole([0.0, 0.0, 128.0, 128.0]));
                        b.push(Blend::Alpha, whole([200.0, 0.0, 256.0, 256.0]));
                        b.push(Blend::Alpha, whole([500.0, 0.0, 64.0, 64.0]));
                    },
                    &text,
                )
                .unwrap();
            assert!(timing.presented);
            r.read_back()
        };
        let atlas = crate::atlas::build_atlas();
        let check = |pixels: &[u8]| {
            let mut opaque = 0;
            for y in 0..256_usize {
                for x in 0..564_usize {
                    let texel = if x < 128 && y < 128 {
                        Some((x, y))
                    } else if (200..456).contains(&x) {
                        Some(((x - 200) / 2, y / 2))
                    } else if x >= 500 && y < 64 {
                        Some((2 * (x - 500) + 1, 2 * y + 1))
                    } else {
                        None
                    };
                    let Some((tx, ty)) = texel else { continue };
                    let a = atlas[(ty * 128 + tx) * 4 + 3];
                    let i = (y * 1_920 + x) * 4;
                    let want = if a == 255 { [255; 3] } else { [0; 3] };
                    assert_eq!(&pixels[i..i + 3], &want, "pixel ({x}, {y})");
                    opaque += usize::from(a == 255);
                }
            }
            assert!(opaque > 1_000, "{opaque}");
        };
        check(&draw(&mut r));
        let (l0, l1, l2, l3) = (
            vec![0u8; 384 * 384 * 4],
            vec![0u8; 192 * 192 * 4],
            vec![0u8; 96 * 96 * 4],
            vec![0u8; 48 * 48 * 4],
        );
        let page: crate::PageLevels<'_> = [&l0, &l1, &l2, &l3];
        let pages = AtlasPages {
            side: 384,
            pages: &[page],
            text_layer: false,
            chart_layer: false,
        };
        r.install_atlas(&pages, ClipTable::default()).unwrap();
        assert_eq!(r.atlas_size(), (384, 2));
        check(&draw(&mut r));
    }
    /// A producer laying a string out by its first letter: `T`, `U` and
    /// `V` as 2032 × 1000 lines (half the 2048 layer each), anything else
    /// 100 × 50; every line full coverage. It fails its rasterization
    /// number `fail_at` (from 1), counting from its creation.
    #[derive(Debug)]
    struct Sized {
        rasters: std::sync::atomic::AtomicU32,
        fail_at: std::sync::atomic::AtomicU32,
    }

    impl crate::AttemptText for Sized {
        fn layout(
            &self,
            req: &skin_model::TextRequest,
            em_px: f32,
            _: crate::LineBounds,
        ) -> crate::LineFit {
            let size = if req.text.starts_with(['T', 'U', 'V']) {
                [2_032, 1_000]
            } else {
                [100, 50]
            };
            crate::LineFit::Fits(crate::LineLayout {
                font: req.font,
                size,
                ink: [0.0, 0.0, 1.0, 1.0],
                advance: 1.0,
                raster_em: em_px,
                glyphs: Box::new([]),
            })
        }

        fn raster(&self, _: &crate::LineLayout, out: &mut [u8]) -> Result<(), SkinError> {
            use std::sync::atomic::Ordering;
            let n = self.rasters.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.fail_at.load(Ordering::SeqCst) {
                return Err(SkinError {
                    reason: "an injected failure".to_owned(),
                });
            }
            out.fill(255);
            Ok(())
        }

        fn resident(&self) -> crate::TextResidency {
            crate::TextResidency::default()
        }
    }

    /// An imported skin of `n` texts showing the title, the artist, the
    /// genre and literals, with the [`Sized`] producer.
    fn sized_bundle(
        r: &Renderer,
        n: usize,
    ) -> (
        tempfile::TempDir,
        std::sync::Arc<crate::SkinBundle>,
        std::sync::Arc<Sized>,
    ) {
        use crate::outline_support::{Value, folder, text, ttf};
        let refs = [10, 16, 13];
        let texts: Vec<_> = (0..n)
            .map(|i| {
                let value = refs
                    .get(i)
                    .map_or_else(|| Value::Literal(format!("x{i}")), |&r| Value::Ref(r));
                let y = i32::try_from(i).unwrap() * 60;
                text("f", 50, value, [0, y, 100, 50])
            })
            .collect();
        let font = crate::font_support::build(&crate::font_support::latin("Skin"));
        let dir = folder(&[("ttf.ttf", font)], &[ttf()], &texts);
        let producer = std::sync::Arc::new(Sized {
            rasters: std::sync::atomic::AtomicU32::new(0),
            fail_at: std::sync::atomic::AtomicU32::new(0),
        });
        let p = std::sync::Arc::clone(&producer);
        let b = crate::outline_support::bundle_with(r, dir.path(), move |_| {
            p as std::sync::Arc<dyn crate::AttemptText>
        });
        (dir, b, producer)
    }

    fn labels<'a>(title: &'a str, artist: &'a str, genre: &'a str) -> skin_model::ChartText<'a> {
        skin_model::ChartText {
            title,
            subtitle: "",
            artist,
            subartist: "",
            genre,
        }
    }

    fn stage_labels(
        r: &mut Renderer,
        b: &std::sync::Arc<crate::SkinBundle>,
        text: &skin_model::ChartText<'_>,
        request: u64,
    ) -> Result<super::PreparedToken, RenderError> {
        use crate::test_common::{GAUGES, context};
        let chart = bms_model::ChartBuilder::new(120_000).build().unwrap();
        let ctx = context(&GAUGES, false);
        let a = crate::AttemptInput {
            chart: &chart,
            context: &ctx,
            text,
            target: r.target(),
            continuation: skin_model::Continuation::Fresh,
        };
        r.stage(b, &a, request)
    }

    /// A buffer whose usage mixes both map modes: every device's
    /// validation refuses it.
    fn refuse(gpu: &crate::Gpu) {
        let _ = gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("refused"),
            size: 4,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::MAP_WRITE,
            mapped_at_creation: false,
        });
    }

    /// Level `k` of the atlas's text layer, read back: RGBA8, `side >> k`
    /// texels square.
    fn read_text_layer(r: &Renderer, k: u32) -> Vec<u8> {
        read_layer(r, r.atlas.text_layer().unwrap(), k)
    }

    /// A `w`×`h` picture of `rgb`, read and fitted into `which`'s slot of
    /// a chart layer of side `side`.
    fn chart_picture(
        which: skin_model::chart_image::ChartImage,
        side: u32,
        [w, h]: [u32; 2],
        [r, g, b]: [u8; 3],
    ) -> skin_model::chart_image::Fitted {
        let d = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(d.path().join("p.png")).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let texels = [r, g, b, 255].repeat(usize::try_from(w * h).unwrap());
        e.write_header().unwrap().write_image_data(&texels).unwrap();
        let dir = skin_model::chart_image::ChartDir::new(d.path()).unwrap();
        let slot = which.slot(side).unwrap().1;
        skin_model::chart_image::read(&dir, "p.png", &[], slot, &|| false, &|_| None)
            .image
            .unwrap()
    }

    /// A renderer with a committed play installation keeping a chart
    /// layer, and the folder its image lives in.
    fn chart_renderer() -> (Renderer, tempfile::TempDir, u32) {
        let dir = tempfile::tempdir().unwrap();
        let mut r = crate::test_common::offscreen(1_920, 1_080);
        let look = crate::test_decl::Look {
            chart: true,
            ..crate::test_decl::Look::left([255, 0, 0, 255])
        };
        let (b, _) = crate::test_decl::bundle(&r, dir.path(), &look);
        crate::test_common::install(&mut r, &b, 1);
        let side = b.side();
        (r, dir, side)
    }

    /// Each level `k` of a chart image is written at its slot's origin
    /// `>> k`, its whole allocation and nothing past it: the stage file's
    /// at the top left, the back image's at the top right. While the
    /// writes were queued, the installation's images were unavailable.
    #[test]
    fn chart_images_are_written_at_their_slots_at_every_level() {
        use skin_model::chart_image::{ChartImage, ChartTable};
        let (mut r, _dir, side) = chart_renderer();
        let stage = chart_picture(ChartImage::Stage, side, [5, 3], [10, 20, 30]);
        let back = chart_picture(ChartImage::Back, side, [5, 3], [40, 50, 60]);
        assert_eq!(stage.padded(), [24, 24]);
        let table = r
            .publish_chart_images(ChartTarget::Committed, [Some(&stage), Some(&back)])
            .unwrap();
        // One image per submission, each waited for before the next is
        // queued: its waits before it, then its own.
        assert_eq!(
            r.chart_submissions,
            [(ChartImage::Stage, 0), (ChartImage::Back, 1)]
        );
        assert_eq!(r.chart_waits, 2);
        assert_eq!(
            r.chart_during_write,
            Some(None),
            "unavailable while written"
        );
        assert_eq!(
            r.installed.as_ref().unwrap().loading().chart_images(),
            Some(table)
        );
        assert_ne!(table, ChartTable::ABSENT);
        let layer = r.atlas.chart_layer().unwrap();
        for k in 0..4 {
            let level = read_layer(&r, layer, k);
            let s = side >> k;
            let at = |x: u32, y: u32| &level[((y * s + x) * 4) as usize..][..4];
            assert_eq!(
                at(0, 0),
                [10, 20, 30, 255],
                "level {k}: the stage file's origin"
            );
            let end = 24 >> k;
            assert_eq!(at(end - 1, end - 1), [10, 20, 30, 255], "level {k}");
            assert_eq!(at(end, 0), [0; 4], "level {k}: nothing past the allocation");
            assert_eq!(
                at(s / 2, 0),
                [40, 50, 60, 255],
                "level {k}: the back image's origin"
            );
            assert_eq!(at(s / 2 - 1, 0), [0; 4], "level {k}");
        }
        // A publication over published images: unavailable from its first
        // byte, never the old table over the texels being replaced.
        r.publish_chart_images(ChartTarget::Committed, [None, Some(&back)])
            .unwrap();
        assert_eq!(
            r.chart_during_write,
            Some(None),
            "invalidated before the write"
        );
    }

    /// A write the device refuses, or a picture fitted for another slot,
    /// leaves the images absent for the installation: a refusal, and
    /// nothing stale drawn.
    #[test]
    fn a_refused_chart_publication_leaves_the_images_absent() {
        use skin_model::chart_image::{ChartImage, ChartTable};
        let (mut r, _dir, side) = chart_renderer();
        let stage = chart_picture(ChartImage::Stage, side, [64, 64], [1, 2, 3]);
        r.publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap();
        r.chart_fault = Some(refuse);
        let e = r
            .publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap_err();
        assert!(
            matches!(e, RenderError::Atlas(UploadError::Device(_))),
            "{e:?}"
        );
        assert!(e.is_refusal());
        let absent = ChartTable::ABSENT.keyed(r.chart_key().unwrap());
        let presented = || r.installed.as_ref().unwrap().loading().chart_images();
        assert_eq!(presented(), Some(absent));

        r.publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap();
        let wide = chart_picture(ChartImage::Stage, side * 2, [side + 100, 8], [1, 2, 3]);
        let e = r
            .publish_chart_images(ChartTarget::Committed, [Some(&wide), None])
            .unwrap_err();
        assert!(
            matches!(
                e,
                RenderError::ChartImage {
                    which: ChartImage::Stage
                }
            ),
            "{e:?}"
        );
        assert!(e.is_refusal());
        let presented = r.installed.as_ref().unwrap().loading().chart_images();
        assert_eq!(presented, Some(absent));
    }

    /// A device destroyed while the writes are queued: the wait may still
    /// return, but the publication fails as the device's and the images
    /// stay unavailable, never published over texels that were dropped.
    #[test]
    fn a_device_lost_during_a_chart_publication_publishes_nothing() {
        use skin_model::chart_image::ChartImage;
        let (mut r, _dir, side) = chart_renderer();
        let stage = chart_picture(ChartImage::Stage, side, [64, 64], [1, 2, 3]);
        r.publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap();
        r.chart_fault = Some(|gpu| gpu.device().destroy());
        let e = r
            .publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap_err();
        assert!(!e.is_refusal(), "{e:?}");
        assert!(r.gpu.lost());
        assert_eq!(r.installed.as_ref().unwrap().loading().chart_images(), None);
        assert_eq!(r.valid_chart(), None);
    }

    /// What each failure left, by its variant: a slot misfit and every
    /// refused write (whatever the device's error) an absent table; a lost
    /// device, a failing wait and a changed target nothing; no layer no
    /// target. A failing wait leaves the images unavailable.
    #[test]
    fn each_chart_failure_tells_what_it_left() {
        use skin_model::chart_image::ChartImage;
        let internal = || {
            RenderError::Atlas(UploadError::Device(wgpu::Error::Internal {
                source: "internal".into(),
                description: "internal".to_owned(),
            }))
        };
        assert_eq!(internal().chart_outcome(), ChartOutcome::Absent);
        assert!(!internal().is_refusal(), "not a refusal, yet absent");
        assert_eq!(
            RenderError::ChartImage {
                which: ChartImage::Back
            }
            .chart_outcome(),
            ChartOutcome::Absent
        );
        assert_eq!(
            RenderError::DeviceLost.chart_outcome(),
            ChartOutcome::Unavailable
        );
        assert_eq!(
            RenderError::ChartChanged.chart_outcome(),
            ChartOutcome::Unavailable
        );
        assert_eq!(
            RenderError::NoChartLayer.chart_outcome(),
            ChartOutcome::NoTarget
        );

        let (mut r, _dir, side) = chart_renderer();
        let stage = chart_picture(ChartImage::Stage, side, [64, 64], [1, 2, 3]);
        r.publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap();
        r.chart_wait_fault = Some(|| RenderError::Poll(wgpu::PollError::Timeout));
        let e = r
            .publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap_err();
        assert!(matches!(e, RenderError::Poll(_)), "{e:?}");
        assert_eq!(e.chart_outcome(), ChartOutcome::Unavailable);
        assert_eq!(r.installed.as_ref().unwrap().loading().chart_images(), None);
        assert_eq!(r.valid_chart(), None);
    }

    /// An absence published into a staging once its device is known lost
    /// is refused as the device's, as the committed target's is, never a
    /// success its commit could not keep.
    #[test]
    fn a_staged_absence_on_a_lost_device_is_refused() {
        let (mut r, _dir, _side) = chart_renderer();
        let dir = tempfile::tempdir().unwrap();
        let look = crate::test_decl::Look {
            chart: true,
            ..crate::test_decl::Look::left([0, 0, 255, 255])
        };
        let (b, _) = crate::test_decl::bundle(&r, dir.path(), &look);
        let text = labels("T", "A", "G");
        let token = stage_labels(&mut r, &b, &text, 2).unwrap();
        r.gpu.device().destroy();
        // The device reports its loss at its next poll.
        let _ = r.gpu.device().poll(wgpu::PollType::wait_indefinitely());
        assert!(r.gpu.lost());
        let e = r
            .publish_chart_absent(ChartTarget::Staged(token))
            .unwrap_err();
        assert!(matches!(e, RenderError::DeviceLost), "{e:?}");
        assert_eq!(e.chart_outcome(), ChartOutcome::Unavailable);
        assert_eq!(r.pending.as_ref().map(|p| p.chart), Some(None));
    }

    /// A loss after a publication: every copy of the table stays where it
    /// is, but no frame draws it, since frames are built against the live
    /// key, which a lost device has none of; a publication then fails as
    /// the device's.
    #[test]
    fn a_loss_after_publication_leaves_no_drawable_copy() {
        use skin_model::chart_image::ChartImage;
        let (mut r, _dir, side) = chart_renderer();
        let stage = chart_picture(ChartImage::Stage, side, [64, 64], [1, 2, 3]);
        let table = r
            .publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap();
        assert!(table.drawable(r.chart_key()));
        r.gpu.device().destroy();
        // The device reports its loss at its next poll.
        let _ = r.gpu.device().poll(wgpu::PollType::wait_indefinitely());
        assert!(r.gpu.lost());
        assert_eq!(r.chart_key(), None);
        let copy = r.installed.as_ref().unwrap().loading().chart_images();
        assert_eq!(copy, Some(table), "the copy is still there");
        assert!(!table.drawable(r.chart_key()), "but draws nowhere");
        let e = r
            .publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap_err();
        assert!(matches!(e, RenderError::DeviceLost), "{e:?}");
    }

    /// The play pages uploaded again through `install_atlas` after a
    /// publication: a new array, zeroed, so the published table no longer
    /// draws, though its copies stay, until it is published again.
    #[test]
    fn an_atlas_upload_after_publication_leaves_no_drawable_copy() {
        use skin_model::chart_image::ChartImage;
        let (mut r, _dir, side) = chart_renderer();
        let stage = chart_picture(ChartImage::Stage, side, [64, 64], [1, 2, 3]);
        let table = r
            .publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap();
        let bundle = std::sync::Arc::clone(r.installed.as_ref().unwrap().bundle());
        let levels = crate::decl::page_levels(bundle.atlas_pages());
        let pages = AtlasPages {
            side: bundle.side(),
            pages: &levels,
            text_layer: bundle.text_layer().is_some(),
            chart_layer: true,
        };
        r.install_atlas(&pages, bundle.clips().clone()).unwrap();
        assert!(!table.drawable(r.chart_key()), "another generation's array");
        assert_eq!(r.valid_chart(), None);
        let again = r
            .publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap();
        assert!(again.drawable(r.chart_key()));
    }

    /// A play frame built against the published images, then the play
    /// pages uploaded again (a new, zeroed chart layer), then submitted:
    /// the built frame was cancelled with the old array, so nothing it
    /// staged is drawn against the new one.
    #[test]
    fn a_frame_built_before_an_atlas_upload_is_never_submitted() {
        use crate::test_common::{GAUGES, attempt, context, input, view};
        use skin_model::chart_image::ChartImage;
        let (mut r, _dir, side) = chart_renderer();
        let stage = chart_picture(ChartImage::Stage, side, [64, 64], [1, 2, 3]);
        r.publish_chart_images(ChartTarget::Committed, [Some(&stage), None])
            .unwrap();
        let chart = bms_model::ChartBuilder::new(120_000).build().unwrap();
        let ctx = context(&GAUGES, false);
        let bundle = std::sync::Arc::clone(r.installed.as_ref().unwrap().bundle());
        let token = r.stage(&bundle, &attempt(&r, &chart, &ctx), 2).unwrap();
        r.commit(token).unwrap();
        let v = view(0, 0);
        let f = input(&v, bms_model::TimeUs(0), bms_model::TimeUs(0), &[]);
        r.build(&f).unwrap();
        let levels = crate::decl::page_levels(bundle.atlas_pages());
        let pages = AtlasPages {
            side: bundle.side(),
            pages: &levels,
            text_layer: bundle.text_layer().is_some(),
            chart_layer: true,
        };
        r.install_atlas(&pages, bundle.clips().clone()).unwrap();
        let timing = r.submit().unwrap();
        assert_eq!(timing.sprites, 0, "the frame built at the old generation");
        // A frame built now is submitted, against the new array.
        r.build(&f).unwrap();
        assert!(r.submit().unwrap().sprites > 0);
    }

    /// Level `k` of atlas layer `z`, read back: RGBA8, `side >> k` texels
    /// square.
    fn read_layer(r: &Renderer, z: u32, k: u32) -> Vec<u8> {
        let device = r.gpu.device();
        let side = r.atlas.side() >> k;
        let row = side * 4;
        let padded =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("text layer"),
            size: u64::from(padded * side),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: r.atlas.texture(),
                mip_level: k,
                origin: wgpu::Origin3d { x: 0, y: 0, z },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(side),
                },
            },
            super::extent(side, side),
        );
        r.gpu.queue().submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |m| m.unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = slice.get_mapped_range().unwrap();
        let mut out = Vec::with_capacity((row * side) as usize);
        for y in 0..side as usize {
            let start = y * padded as usize;
            out.extend_from_slice(&data[start..start + row as usize]);
        }
        out
    }

    /// What [`observe`] saw at each step: the step, the cache's
    /// unpublished entries and free texels, and whether the GPU had
    /// reported the upload's submission done.
    static SEEN: std::sync::Mutex<Vec<(super::text_upload::Step, usize, i64, bool)>> =
        std::sync::Mutex::new(Vec::new());
    /// Set by the queue's own callback once the submitted work is done: it
    /// runs only while the device is polled, so only the production wait
    /// can have run it by the wait's return.
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn observe(gpu: &crate::Gpu, cache: &super::TextCache, step: super::text_upload::Step) {
        use std::sync::atomic::Ordering;
        if step == super::text_upload::Step::Submitted {
            DONE.store(false, Ordering::SeqCst);
            gpu.queue()
                .on_submitted_work_done(|| DONE.store(true, Ordering::SeqCst));
        }
        SEEN.lock().unwrap().push((
            step,
            cache.unpublished(),
            cache.free(),
            DONE.load(Ordering::SeqCst),
        ));
    }

    /// The upload's boundaries in order: every write, then the one
    /// submission inside the scopes, the wait's return, the publication,
    /// and only then `stage`'s return. Until the wait has returned, the
    /// new line is allocated and unpublished; at its return the queue has
    /// reported the submitted work done (its callback, run only by the
    /// production wait's poll), and only then is the line published, or,
    /// after a fault inside the scopes, released, the layer's free area as
    /// before. The line is a tall one, so its upload is not done at once.
    #[test]
    fn upload_ordered_before_return() {
        use super::text_upload::Step::{
            Published, RolledBack, StageReturned, Submitted, WaitReturned, Written,
        };
        let mut r = crate::test_common::offscreen(1_920, 1_080);
        let (_dir, b, _) = sized_bundle(&r, 1);
        crate::test_common::install(&mut r, &b, 1);
        let free = r.text_cache.free();
        r.text_hooks.observe = Some(observe);
        r.text_hooks.steps.clear();
        r.text_hooks.fault_after = Some((1, refuse));
        let e = stage_labels(&mut r, &b, &labels("T", "", ""), 2).unwrap_err();
        assert!(
            matches!(e, RenderError::Atlas(UploadError::Device(_))),
            "{e:?}"
        );
        assert!(e.is_refusal());
        let seen = std::mem::take(&mut *SEEN.lock().unwrap());
        let steps: Vec<_> = seen.iter().map(|s| s.0).collect();
        assert_eq!(
            steps,
            [
                Written,
                Written,
                Written,
                Written,
                Submitted,
                WaitReturned,
                RolledBack
            ]
        );
        for &(step, unpublished, f, done) in &seen[..6] {
            assert_eq!(unpublished, 1, "{step:?}");
            assert!(f < free, "{step:?}: the line's rectangle is taken");
            assert_eq!(done, step == WaitReturned, "{step:?}");
        }
        assert_eq!((seen[6].1, seen[6].2), (0, free), "released after the wait");

        r.text_hooks.fault_after = None;
        r.text_hooks.steps.clear();
        DONE.store(false, std::sync::atomic::Ordering::SeqCst);
        let token = stage_labels(&mut r, &b, &labels("T", "", ""), 3).unwrap();
        r.text_hooks.observe = None;
        let seen = std::mem::take(&mut *SEEN.lock().unwrap());
        assert_eq!(
            r.text_hooks.steps,
            [
                Written,
                Written,
                Written,
                Written,
                Submitted,
                WaitReturned,
                Published,
                StageReturned
            ]
        );
        for &(step, unpublished, f, done) in &seen[..6] {
            assert_eq!(unpublished, 1, "{step:?}");
            assert!(f < free, "{step:?}");
            assert_eq!(done, step == WaitReturned, "{step:?}");
        }
        assert_eq!(seen[6].1, 0, "published");
        assert!(r.text_report().submission.is_some());
        r.commit(token).unwrap();
        let level = read_text_layer(&r, 0);
        let (_, rect, _) = r.text_cache.rects()[0].clone();
        let side = r.atlas.side();
        let at = |x: u32, y: u32| &level[((y * side + x) * 4) as usize..][..4];
        let g = skin_model::GUTTER;
        assert_eq!(at(rect[0] + g, rect[1] + g), [255; 4]);
        assert_eq!(at(rect[0], rect[1]), [0; 4], "the gutter");
    }

    /// The device failing while the upload is waited for, unlike a
    /// validation refusal inside the scopes: not a refusal (the renderer
    /// is built again), the new line neither published nor released, as
    /// its writes may still be pending, and the cache places nothing more.
    #[test]
    fn a_device_failure_mid_upload_closes_the_cache() {
        let mut r = crate::test_common::offscreen(1_920, 1_080);
        let (_dir, b, _) = sized_bundle(&r, 1);
        crate::test_common::install(&mut r, &b, 1);
        r.text_hooks.wait = Some(RenderError::Poll(wgpu::PollError::Timeout));
        let e = stage_labels(&mut r, &b, &labels("T", "", ""), 2).unwrap_err();
        assert!(matches!(e, RenderError::Poll(_)), "{e:?}");
        assert!(!e.is_refusal());
        use super::text_upload::Step::{Abandoned, WaitReturned};
        assert_eq!(
            r.text_hooks.steps.iter().rev().take(2).collect::<Vec<_>>(),
            [&Abandoned, &WaitReturned]
        );
        assert!(r.text_cache.closed());
        assert!(r.staged().is_none());
        // The next staging places no line: every request is refused.
        let token = stage_labels(&mut r, &b, &labels("s", "", ""), 3).unwrap();
        assert_eq!(r.text_report().counts.placements, 0);
        assert_eq!(r.text_report().notes, [(0, crate::TextNote::LayerFull)]);
        r.commit(token).unwrap();
    }

    /// Failures in the middle of an upload, with no frame drawn between
    /// them: the producer's at its third line, then the device's after two
    /// writes. Each time the GPU is waited for before the new lines are
    /// released, and the layer's free area comes back to what it was; a
    /// final staging places every line.
    #[test]
    fn repeated_mid_raster_failures() {
        use super::text_upload::Step::{RolledBack, WaitReturned};
        use std::sync::atomic::Ordering;
        let mut r = crate::test_common::offscreen(1_920, 1_080);
        let (_dir, b, p) = sized_bundle(&r, 5);
        crate::test_common::install(&mut r, &b, 1);
        // An earlier attempt placed: its report is replaced by a failure.
        let token = stage_labels(&mut r, &b, &labels("p", "q", "r"), 2).unwrap();
        r.commit(token).unwrap();
        assert!(r.text_report().upload_bytes > 0);
        let (free, placed) = (r.text_cache.free(), r.text_cache.len());
        let before = p.rasters.load(Ordering::SeqCst);
        p.fail_at.store(before + 3, Ordering::SeqCst);
        r.text_hooks.steps.clear();
        let e = stage_labels(&mut r, &b, &labels("a", "b", "c"), 3).unwrap_err();
        assert!(matches!(e, RenderError::Skin(_)), "{e:?}");
        assert_eq!(
            (r.text_report().upload_bytes, r.memory().text_upload),
            (0, 0),
            "an earlier preparation's report"
        );
        assert_eq!(p.rasters.load(Ordering::SeqCst), before + 3);
        assert_eq!(
            r.text_hooks.steps.iter().rev().take(2).collect::<Vec<_>>(),
            [&RolledBack, &WaitReturned]
        );
        assert_eq!((r.text_cache.free(), r.text_cache.len()), (free, placed));

        p.fail_at.store(0, Ordering::SeqCst);
        r.text_hooks.fault_after = Some((2, refuse));
        r.text_hooks.steps.clear();
        let e = stage_labels(&mut r, &b, &labels("a", "b", "c"), 4).unwrap_err();
        assert!(
            matches!(e, RenderError::Atlas(UploadError::Device(_))),
            "{e:?}"
        );
        assert_eq!(
            r.text_hooks.steps.iter().rev().take(2).collect::<Vec<_>>(),
            [&RolledBack, &WaitReturned]
        );
        assert_eq!((r.text_cache.free(), r.text_cache.len()), (free, placed));

        r.text_hooks.fault_after = None;
        let token = stage_labels(&mut r, &b, &labels("a", "b", "c"), 5).unwrap();
        r.commit(token).unwrap();
        assert_eq!(r.text_cache.len(), placed + 3, "the literals hit");
        assert!(r.text_report().notes.is_empty());
    }

    /// A full-width tall line comes back on a shelf rounded up, and a
    /// freed tall rectangle taken by a small line shows nothing of the
    /// tall one at any level: the whole returned rectangle is written.
    #[test]
    fn shelf_rounding_and_reuse() {
        // 4096 × 1040 asked for on a 4096 layer: 1088 tall.
        let mut c = crate::text_cache::TextCache::new(4_096, 2);
        let tall = |req: &skin_model::TextRequest, em: f32, _: crate::LineBounds| {
            crate::LineFit::Fits(crate::LineLayout {
                font: req.font,
                size: [4_080, 1_024],
                ink: [0.0, 0.0, 1.0, 1.0],
                advance: 1.0,
                raster_em: em,
                glyphs: Box::new([]),
            })
        };
        let req = skin_model::TextRequest {
            slot: 0,
            text: "T".to_owned(),
            font: skin_model::FontRef::Default,
            em_px: 100.0,
        };
        let p = c.place(
            std::slice::from_ref(&req),
            crate::LineBounds::for_side(4_096),
            &[],
            0,
            tall,
        );
        assert_eq!(p.new[0].rect[2..], [4_096, 1_088]);

        let mut r = crate::test_common::offscreen(1_920, 1_080);
        let (_dir, b, _) = sized_bundle(&r, 1);
        crate::test_common::install(&mut r, &b, 1);
        for (request, title) in [(2, "T"), (3, "U")] {
            let token = stage_labels(&mut r, &b, &labels(title, "", ""), request).unwrap();
            r.commit(token).unwrap();
        }
        let rects = r.text_cache.rects();
        let t = rects.iter().find(|(s, ..)| s == "T").unwrap().1;
        // A small line, with U installed: T is evicted, and its space is
        // reused.
        let token = stage_labels(&mut r, &b, &labels("s", "", ""), 4).unwrap();
        assert!(r.text_report().counts.evictions >= 1);
        r.commit(token).unwrap();
        let (_, s, size) = r
            .text_cache
            .rects()
            .into_iter()
            .find(|(s, ..)| s == "s")
            .unwrap();
        assert!(
            s[0] >= t[0]
                && s[1] >= t[1]
                && s[0] + s[2] <= t[0] + t[2]
                && s[1] + s[3] <= t[1] + t[3]
        );
        let side = r.atlas.side();
        let g = skin_model::GUTTER;
        for k in 0..4_u32 {
            let level = read_text_layer(&r, k);
            let at = |x: u32, y: u32| level[(((y >> k) * (side >> k) + (x >> k)) * 4) as usize];
            // The rectangle's last texel, past the small line: zero, where
            // the tall line was white.
            assert_eq!(at(s[0] + s[2] - 1, s[1] + s[3] - 1), 0, "level {k}");
            assert_eq!(at(s[0], s[1]), 0, "level {k}");
            if k == 0 {
                assert_eq!(at(s[0] + g, s[1] + g), 255);
                assert_eq!(at(s[0] + g + size[0], s[1] + g), 0);
            }
        }
    }

    /// A staging refused before its text is prepared, by the device
    /// refusing its new atlas or by the shared admission, leaves no
    /// earlier preparation's report.
    #[test]
    fn an_early_refusal_clears_the_text_report() {
        let mut r = crate::test_common::offscreen(1_920, 1_080);
        let (_dir, a, _) = sized_bundle(&r, 1);
        let token = stage_labels(&mut r, &a, &labels("s", "", ""), 1).unwrap();
        r.commit(token).unwrap();
        assert!(r.text_report().upload_bytes > 0);
        let (_other, b, _) = sized_bundle(&r, 1);
        r.atlas_fault = Some(refuse);
        let e = stage_labels(&mut r, &b, &labels("s", "", ""), 2).unwrap_err();
        r.atlas_fault = None;
        assert!(
            matches!(e, RenderError::Atlas(UploadError::Device(_))),
            "{e:?}"
        );
        assert_eq!(r.text_report().upload_bytes, 0);
        assert_eq!(r.text_report().counts, crate::TextCounts::default());
        assert_eq!(r.memory().text_upload, 0);

        // A refusal by the shared admission, the first step that can
        // refuse: a selection skin committed, charged what the shared limit
        // leaves beside an atlas's cap and a tiny one's more, and a play
        // candidate whose atlas is the cap.
        let token = stage_labels(&mut r, &a, &labels("t", "", ""), 3).unwrap();
        r.commit(token).unwrap();
        assert!(r.text_report().upload_bytes > 0);
        let tiny = crate::test_decl::solid_bundle(
            &r,
            &crate::test_decl::Look::left([0, 255, 0, 255]),
            [16, 16],
        );
        let token = r
            .install(skin_model::ScreenKind::Selection, &tiny, 4)
            .unwrap();
        r.commit(token).unwrap();
        let cap = skin_model::MAX_ATLAS_TEXELS;
        r.state_charge(&tiny, crate::SHARED_TEXELS - cap + tiny.atlas_texels());
        let big = crate::test_decl::atlas_cap_bundle(&r, [0, 0, 255, 255]);
        let e = stage_labels(&mut r, &big, &labels("t", "", ""), 5).unwrap_err();
        assert!(matches!(e, RenderError::Admission { .. }), "{e:?}");
        assert_eq!(r.text_report().upload_bytes, 0);
        assert_eq!(r.memory().text_upload, 0);
    }
}
