//! A skin array's creation off the frame (design.md "Screen Transitions
//! and Loading", the frame-budgeted uploads): the array of a play staging
//! or of the result pages upload is created not resident, with
//! `D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT`, and made resident on the OS's
//! paging queue (`ID3D12Device3::EnqueueMakeResident`), which signals a
//! fence. The render thread polls that fence once a pass without waiting
//! and wraps the array for wgpu once it completes; the zero fill and the
//! commit run off the frame. Where the device lacks what that needs, the
//! array is created synchronously, as before (the `Sync` path).
//!
//! [`NativeCreate`] is the pure description; [`Residency`] makes the
//! native calls, on D3D12 in production and on a fake in tests. A
//! [`Residing`] resource is one whose residency was asked for and not yet
//! seen complete: it is never released before its fence completes on a
//! device that was not removed.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::{Arc, Mutex, PoisonError};

use bms_model::TimeUs;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_FEATURE_D3D12_OPTIONS7, D3D12_FEATURE_DATA_D3D12_OPTIONS7, D3D12_FENCE_FLAG_NONE,
    D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT, D3D12_HEAP_FLAG_CREATE_NOT_ZEROED, D3D12_HEAP_FLAGS,
    D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESIDENCY_FLAG_NONE,
    D3D12_RESIDENCY_FLAGS, D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_TEXTURE2D,
    D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATES,
    D3D12_TEXTURE_LAYOUT_UNKNOWN, ID3D12Device, ID3D12Device3, ID3D12Device8, ID3D12Fence,
    ID3D12Pageable, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3,
};
use windows::Win32::System::LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_PIN, GetModuleHandleExW};
use windows::core::{Interface, w};

/// How long a ticket may take from its request to its array's import
/// before it is answered with a timeout (design.md "Screen Transitions and
/// Loading", the frame-budgeted uploads): about 54 times the longest
/// request-to-ready time measured (37 ms for a 1.33 GB array), well inside
/// the load's own bound.
pub const CREATE_BOUND: TimeUs = TimeUs(2_000_000);

/// The fence value a removed device reads: reserved, never signalled.
pub(crate) const REMOVED: u64 = u64::MAX;

/// The native creation of one skin array: a 2D RGBA8 array of `side`
/// texels, `layers` layers and `levels` levels, as
/// [`skin_atlas`](crate::skin_atlas)'s `wgpu` descriptor describes it,
/// created not resident in `COPY_DEST` and made resident with no
/// over-budget refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeCreate {
    pub(crate) side: u32,
    pub(crate) layers: u32,
    pub(crate) levels: u32,
}

impl NativeCreate {
    /// The heap flags: not resident, and zeroed by the OS when it is made
    /// resident.
    pub(crate) const fn heap_flags(self) -> D3D12_HEAP_FLAGS {
        D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT
    }

    /// The residency flags: none, so residency over the process's budget
    /// pages rather than refusing.
    pub(crate) const fn residency_flags(self) -> D3D12_RESIDENCY_FLAGS {
        D3D12_RESIDENCY_FLAG_NONE
    }

    /// The default heap.
    pub(crate) fn heap_properties(self) -> D3D12_HEAP_PROPERTIES {
        D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..D3D12_HEAP_PROPERTIES::default()
        }
    }

    /// The state it is created in, which its import names.
    pub(crate) const fn initial_state(self) -> D3D12_RESOURCE_STATES {
        D3D12_RESOURCE_STATE_COPY_DEST
    }

    /// The resource's description.
    pub(crate) fn resource_desc(self) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: u64::from(self.side),
            Height: self.side,
            DepthOrArraySize: u16::try_from(self.layers).unwrap_or(u16::MAX),
            MipLevels: u16::try_from(self.levels).unwrap_or(u16::MAX),
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        }
    }

    /// Whether its heap is zeroed: it carries no `CREATE_NOT_ZEROED`, so
    /// every texel no step writes reads zero, as a texture `wgpu` creates
    /// does.
    pub(crate) fn zeroed(self) -> bool {
        !self
            .heap_flags()
            .contains(D3D12_HEAP_FLAG_CREATE_NOT_ZEROED)
    }

    /// Whether `d` describes the same texture: its format, dimension,
    /// size, levels, samples, and a usage the resource's flags allow
    /// (sampled and copied, no render target or storage), with no other
    /// view format.
    pub(crate) fn matches(self, d: &wgpu::TextureDescriptor<'_>) -> bool {
        let allowed = wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC;
        let r = self.resource_desc();
        d.format == wgpu::TextureFormat::Rgba8Unorm
            && r.Format == DXGI_FORMAT_R8G8B8A8_UNORM
            && d.dimension == wgpu::TextureDimension::D2
            && d.size.width == self.side
            && d.size.height == self.side
            && u64::from(d.size.width) == r.Width
            && d.size.height == r.Height
            && d.size.depth_or_array_layers == u32::from(r.DepthOrArraySize)
            && d.mip_level_count == u32::from(r.MipLevels)
            && d.sample_count == r.SampleDesc.Count
            && allowed.contains(d.usage)
            && d.usage.contains(wgpu::TextureUsages::COPY_DST)
            && d.view_formats.is_empty()
            && r.Flags == D3D12_RESOURCE_FLAG_NONE
    }

    /// Its logical bytes: every level of every layer, RGBA8.
    pub(crate) fn bytes(self) -> u64 {
        (0..self.levels)
            .fold(0_u64, |sum, l| {
                let s = u64::from(self.side >> l);
                sum.saturating_add(s.saturating_mul(s).saturating_mul(4))
            })
            .saturating_mul(u64::from(self.layers))
    }
}

/// How a fence reading stands against a value asked of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Completion {
    /// Not yet.
    Pending,
    /// The value was signalled.
    Done,
    /// The fence reads [`REMOVED`]: the device was removed, and the
    /// value is never taken as signalled.
    Removed,
}

/// How `completed`, a fence's reading, stands against `value`.
#[must_use]
pub(crate) const fn completion(completed: u64, value: u64) -> Completion {
    if completed == REMOVED {
        Completion::Removed
    } else if completed >= value {
        Completion::Done
    } else {
        Completion::Pending
    }
}

/// The values a fence is signalled with: from 1, one per signal, never
/// [`REMOVED`].
#[derive(Debug)]
pub(crate) struct FenceValues {
    next: u64,
}

impl FenceValues {
    pub(crate) const fn new() -> Self {
        Self { next: 1 }
    }

    /// Whether a value is left for a request's residency signal.
    pub(crate) const fn left(&self) -> bool {
        self.next < REMOVED
    }

    /// The next value, if one is left.
    pub(crate) const fn take(&mut self) -> Option<u64> {
        if !self.left() {
            return None;
        }
        let v = self.next;
        self.next += 1;
        Some(v)
    }

    /// Tests: the next value set to `next`.
    #[cfg(test)]
    pub(crate) const fn set_next(&mut self, next: u64) {
        self.next = next;
    }
}

/// A fence the renderer reads: its completed value and whether its device
/// was removed.
pub(crate) trait FenceRead: Send + Sync {
    /// Its completed value; [`REMOVED`] once its device is removed.
    fn completed(&self) -> u64;
    /// Whether its device reports itself removed
    /// (`GetDeviceRemovedReason`).
    fn removed(&self) -> bool;
}

/// A fence shared by every resource whose residency it signals.
pub(crate) type Fence = Arc<dyn FenceRead>;

/// A native resource.
pub(crate) enum Native {
    D3d12(ID3D12Resource),
    /// Tests and development builds: a resource the fake numbers.
    #[cfg(any(test, feature = "dev-tools"))]
    Fake(u64),
}

impl Native {
    /// The D3D12 resource; none for a test's fake.
    const fn d3d12(&self) -> Option<&ID3D12Resource> {
        match self {
            Self::D3d12(r) => Some(r),
            #[cfg(any(test, feature = "dev-tools"))]
            Self::Fake(_) => None,
        }
    }

    /// [`d3d12`](Self::d3d12), taken.
    fn into_d3d12(self) -> Option<ID3D12Resource> {
        match self {
            Self::D3d12(r) => Some(r),
            #[cfg(any(test, feature = "dev-tools"))]
            Self::Fake(_) => None,
        }
    }
}

/// A resource whose residency was asked for: held, with its fence and the
/// value its residency signals, until that value completes, then
/// released at a boundary. Charged at its logical bytes meanwhile.
pub(crate) struct Residing {
    /// Held, never read: released with the entry once its fence completes.
    #[expect(dead_code, reason = "an owner held for its release, never read")]
    pub(crate) native: Native,
    pub(crate) fence: Fence,
    pub(crate) value: u64,
    pub(crate) bytes: u64,
    /// The level-0 texels admission charged for it.
    pub(crate) charge: u64,
    /// When its ticket was taken.
    pub(crate) requested: TimeUs,
    /// When its ticket's deadline passes: past it, READY's cutoff no
    /// longer waits for it.
    pub(crate) deadline: bms_model::Deadline,
}

impl Residing {
    /// Its residency as its fence reads now.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn completion(&self) -> Completion {
        completion(self.fence.completed(), self.value)
    }

    /// Whether it may be released: its residency completed.
    pub(crate) fn done(&self) -> bool {
        self.completion() == Completion::Done
    }

    /// Whether it may be released without its fence: its device's removal
    /// confirmed.
    pub(crate) fn device_removed(&self) -> bool {
        self.completion() == Completion::Removed && self.fence.removed()
    }
}

/// Why a native call did not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The runtime does not support it for this resource
    /// (`E_NOTIMPL`, `E_INVALIDARG`): the request takes the `Sync` path.
    Unsupported,
    /// The device refused it otherwise (out of memory, removal).
    Failed,
}

/// DXGI's local segment as the process stands: its budget and its
/// current usage, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// The process's budget.
    pub budget: u64,
    /// The process's current usage.
    pub usage: u64,
}

/// Why a ticket was answered with a timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutPhase {
    /// Waiting for the room its admission needs.
    Room,
    /// Waiting for its array's residency.
    Residency,
}

impl std::fmt::Display for TimeoutPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Room => "room",
            Self::Residency => "residency",
        })
    }
}

/// The asynchronous creation's own refusals.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum ResidencyError {
    /// The ticket's deadline passed in `phase`, [`CREATE_BOUND`] after its
    /// request.
    #[error("the array's {phase} took longer than {} ms", CREATE_BOUND.0 / 1_000)]
    Timeout {
        /// What it was waiting for.
        phase: TimeoutPhase,
    },
    /// The device refused the not-resident creation or its residency.
    #[error("the device refused the array's residency")]
    Refused,
}

/// What a residency line reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidencyEvent {
    /// Its residency asked for.
    Request,
    /// Its residency seen complete.
    Ready,
    /// Its ticket past its deadline, this long after it was taken.
    TimedOut { elapsed: TimeUs },
}

/// A line of the render report: an array's residency asked for, seen
/// complete or timed out, with DXGI's budget and usage then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResidencyLine {
    pub event: ResidencyEvent,
    /// The array's native allocation size, in bytes.
    pub native: u64,
    /// Its logical bytes, which admission counts in texels.
    pub logical: u64,
    /// DXGI's local budget and usage, when read.
    pub budget: Option<Budget>,
}

impl std::fmt::Display for ResidencyLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const MIB: u64 = 1 << 20;
        f.write_str("render: array residency ")?;
        match self.event {
            ResidencyEvent::Request => f.write_str("request")?,
            ResidencyEvent::Ready => f.write_str("ready")?,
            ResidencyEvent::TimedOut { elapsed } => {
                write!(f, "timeout after {} ms", elapsed.0 / 1_000)?;
            }
        }
        write!(
            f,
            " {} MiB (logical {} MiB)",
            self.native / MIB,
            self.logical / MIB,
        )?;
        match self.budget {
            Some(b) => write!(
                f,
                ": budget {} MiB, usage {} MiB",
                b.budget / MIB,
                b.usage / MIB
            ),
            None => f.write_str(": budget unread"),
        }
    }
}

/// The error of a timeout in `phase`.
#[must_use]
pub const fn timeout(phase: TimeoutPhase) -> crate::RenderError {
    crate::RenderError::Atlas(crate::UploadError::Residency(ResidencyError::Timeout {
        phase,
    }))
}

/// The native calls of the asynchronous path.
pub(crate) trait Residency: Send {
    /// Creates `n` not resident: nothing is committed yet.
    fn create(&mut self, n: NativeCreate) -> Result<Native, Refusal>;
    /// Asks for `native`'s residency, its fence signalled with `value`.
    fn enqueue(
        &mut self,
        native: &Native,
        value: u64,
        flags: D3D12_RESIDENCY_FLAGS,
    ) -> Result<(), Refusal>;
    /// The fence residency signals.
    fn fence(&self) -> &Fence;
    /// DXGI's local budget and usage, when they can be read.
    fn budget(&self) -> Option<Budget>;
    /// `n`'s native allocation size.
    fn native_size(&self, n: NativeCreate) -> u64;
    /// Wraps `native`, created as `desc` describes, initialized and in
    /// `COPY_DEST`, as `device`'s texture.
    fn import(
        &mut self,
        device: &wgpu::Device,
        native: Native,
        desc: &wgpu::TextureDescriptor<'_>,
    ) -> wgpu::Texture;
}

/// Whether a device with `device3`, `device8` and `options7` (a successful
/// `D3D12_OPTIONS7` query) supports the asynchronous path, or why not: the
/// heap flag needs `ID3D12Device8` or that query, and the residency call
/// `ID3D12Device3`.
pub(crate) const fn capable(
    device3: bool,
    device8: bool,
    options7: bool,
) -> Result<(), &'static str> {
    if !device3 {
        return Err("no ID3D12Device3");
    }
    if !(device8 || options7) {
        return Err("no ID3D12Device8 or D3D12_OPTIONS7");
    }
    Ok(())
}

/// How the renderer creates its skin arrays.
pub(crate) enum Path {
    /// Synchronously, through `wgpu`, for the reason given.
    Sync(&'static str),
    /// Off the frame.
    Async(Box<dyn Residency>),
}

impl Path {
    /// The path's name and its reason, for the render report.
    pub(crate) const fn describe(&self) -> (&'static str, &'static str) {
        match self {
            Self::Sync(why) => ("sync", why),
            Self::Async(_) => ("residency", "ID3D12Device3 and the not-resident heap flag"),
        }
    }
}

/// The D3D12 path of `device` (with `adapter` for DXGI's budget), or the
/// `Sync` path and why.
pub(crate) fn path(device: &wgpu::Device, adapter: Option<IDXGIAdapter3>) -> Path {
    // SAFETY: the raw device is cloned (AddRef) and the guard dropped at
    // once; nothing of wgpu's state is touched.
    let raw = unsafe { device.as_hal::<wgpu_hal::api::Dx12>() }.map(|d| d.raw_device().clone());
    let Some(raw) = raw else {
        return Path::Sync("not a D3D12 device");
    };
    let device3 = raw.cast::<ID3D12Device3>().ok();
    let device8 = raw.cast::<ID3D12Device8>().is_ok();
    let mut o7 = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
    // SAFETY: a feature query into a local of the size passed.
    let options7 = unsafe {
        raw.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS7,
            std::ptr::from_mut(&mut o7).cast(),
            u32::try_from(size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS7>()).unwrap_or(0),
        )
    }
    .is_ok();
    if let Err(why) = capable(device3.is_some(), device8, options7) {
        return Path::Sync(why);
    }
    let Some(device3) = device3 else {
        return Path::Sync("no ID3D12Device3");
    };
    if !pin_libraries() {
        return Path::Sync("d3d12.dll or dxgi.dll not pinned");
    }
    // SAFETY: a fence made on the live device.
    let Ok(fence) = (unsafe { raw.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) }) else {
        return Path::Sync("no residency fence");
    };
    let shared: Fence = Arc::new(D3d12Fence {
        fence: fence.clone(),
        device: raw.clone(),
    });
    Path::Async(Box::new(D3d12 {
        device: raw,
        device3,
        raw_fence: fence,
        fence: shared,
        adapter,
    }))
}

/// Keeps `d3d12.dll` and `dxgi.dll` loaded until the process ends, as a
/// program linked to them keeps them. `wgpu` loads both at run time and
/// unloads them with its last instance, but this path's device, fence and
/// adapter references and the arrays in a `ResidencyHolder` outlive the
/// renderer: released after the unload, each `Release` would call into
/// unmapped code.
fn pin_libraries() -> bool {
    [w!("d3d12.dll"), w!("dxgi.dll")].into_iter().all(|name| {
        let mut module = HMODULE::default();
        // SAFETY: a lookup of a module `wgpu` has loaded, by a static name,
        // into a local; pinning only adds a reference the process keeps.
        unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN, name, &raw mut module) }.is_ok()
    })
}

/// A D3D12 fence and its device.
struct D3d12Fence {
    fence: ID3D12Fence,
    device: ID3D12Device,
}

// SAFETY: D3D12 fences and devices are free-threaded; the reads below are
// `GetCompletedValue` and `GetDeviceRemovedReason`.
unsafe impl Sync for D3d12Fence {}
// SAFETY: as above.
unsafe impl Send for D3d12Fence {}

impl FenceRead for D3d12Fence {
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn completed(&self) -> u64 {
        // SAFETY: a read of the fence's completed value.
        unsafe { self.fence.GetCompletedValue() }
    }

    fn removed(&self) -> bool {
        // SAFETY: a read of the device's removal reason.
        unsafe { self.device.GetDeviceRemovedReason() }.is_err()
    }
}

/// The production path.
struct D3d12 {
    device: ID3D12Device,
    device3: ID3D12Device3,
    raw_fence: ID3D12Fence,
    fence: Fence,
    adapter: Option<IDXGIAdapter3>,
}

/// Whether an `HRESULT` means the call is not supported here.
fn unsupported(e: &windows::core::Error) -> bool {
    e.code() == windows::Win32::Foundation::E_NOTIMPL
        || e.code() == windows::Win32::Foundation::E_INVALIDARG
}

impl Residency for D3d12 {
    fn create(&mut self, n: NativeCreate) -> Result<Native, Refusal> {
        let heap = n.heap_properties();
        let desc = n.resource_desc();
        let mut res: Option<ID3D12Resource> = None;
        // SAFETY: plain descriptions on the stack; the result is written to
        // a local.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &raw const heap,
                n.heap_flags(),
                &raw const desc,
                n.initial_state(),
                None,
                &raw mut res,
            )
        };
        match (made, res) {
            (Ok(()), Some(r)) => Ok(Native::D3d12(r)),
            (Err(e), _) if unsupported(&e) => Err(Refusal::Unsupported),
            _ => Err(Refusal::Failed),
        }
    }

    fn enqueue(
        &mut self,
        native: &Native,
        value: u64,
        flags: D3D12_RESIDENCY_FLAGS,
    ) -> Result<(), Refusal> {
        let Some(r) = native.d3d12() else {
            return Err(Refusal::Failed);
        };
        let Ok(pageable) = r.cast::<ID3D12Pageable>() else {
            return Err(Refusal::Failed);
        };
        // SAFETY: a live resource and fence of this device.
        let queued = unsafe {
            self.device3
                .EnqueueMakeResident(flags, &[Some(pageable)], &self.raw_fence, value)
        };
        match queued {
            Ok(()) => Ok(()),
            Err(e) if unsupported(&e) => Err(Refusal::Unsupported),
            Err(_) => Err(Refusal::Failed),
        }
    }

    fn fence(&self) -> &Fence {
        &self.fence
    }

    fn budget(&self) -> Option<Budget> {
        let a = self.adapter.as_ref()?;
        let mut i = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: a plain query of the adapter's memory use into a local.
        unsafe { a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut i) }.ok()?;
        Some(Budget {
            budget: i.Budget,
            usage: i.CurrentUsage,
        })
    }

    fn native_size(&self, n: NativeCreate) -> u64 {
        let desc = n.resource_desc();
        // SAFETY: a size query of a description on the stack.
        unsafe { self.device.GetResourceAllocationInfo(0, &[desc]) }.SizeInBytes
    }

    fn import(
        &mut self,
        device: &wgpu::Device,
        native: Native,
        desc: &wgpu::TextureDescriptor<'_>,
    ) -> wgpu::Texture {
        let Some(resource) = native.into_d3d12() else {
            return device.create_texture(desc);
        };
        // SAFETY: `resource` was created on this device as `desc` describes
        // (`NativeCreate::matches`), in a zeroed heap (`NativeCreate::zeroed`)
        // whose residency completed, and is in `COPY_DEST`, its initial
        // state, with no work of ours on it.
        unsafe {
            let hal = wgpu_hal::dx12::Device::texture_from_raw(
                resource,
                desc.format,
                desc.dimension,
                desc.size,
                desc.mip_level_count,
                desc.sample_count,
            );
            // `NativeCreate::initial_state`'s use.
            device.create_texture_from_hal::<wgpu_hal::api::Dx12>(
                hal,
                desc,
                wgpu::TextureUses::COPY_DST,
            )
        }
    }
}

/// The pending native work a renderer leaves on a healthy rebuild
/// (design.md "Memory Budget", the worker handoff): owned by the render
/// loop, which outlives every renderer, so a resource whose residency is
/// still in flight is never released before its fence completes. The next
/// renderer counts its bytes in its admission and releases each at a
/// boundary once its fence completes.
#[derive(Clone, Default)]
pub struct ResidencyHolder {
    held: Arc<Mutex<Vec<Residing>>>,
}

impl std::fmt::Debug for ResidencyHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResidencyHolder")
            .field("pending", &self.pending())
            .finish()
    }
}

impl ResidencyHolder {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Residing>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes `r` until its fence completes.
    pub(crate) fn hold(&self, r: Residing) {
        self.lock().push(r);
    }

    /// How many resources it holds.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.lock().len()
    }

    /// The texels admission counts for them.
    pub(crate) fn charge(&self) -> u64 {
        self.lock()
            .iter()
            .fold(0, |sum, r| sum.saturating_add(r.charge))
    }

    /// Whether a resource it holds may be released: its fence completed,
    /// or its device's removal is confirmed.
    pub(crate) fn releasable(&self) -> bool {
        self.lock().iter().any(|r| r.done() || r.device_removed())
    }

    /// Releases one resource whose fence completed, or whose device's
    /// removal is confirmed: whether one was.
    pub(crate) fn release_one(&self) -> bool {
        let mut held = self.lock();
        let Some(i) = held.iter().position(|r| r.done() || r.device_removed()) else {
            return false;
        };
        drop(held.swap_remove(i));
        true
    }

    /// How many it holds and when the oldest's ticket was taken.
    #[must_use]
    pub fn oldest(&self) -> Option<(usize, TimeUs)> {
        let held = self.lock();
        let oldest = held.iter().map(|r| r.requested).min()?;
        Some((held.len(), oldest))
    }
}

/// Tests and development builds: a [`Residency`] whose fence, refusals
/// and budget the caller sets, recording every native call.
#[cfg(any(test, feature = "dev-tools"))]
pub mod fake {
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    use windows::Win32::Graphics::Direct3D12::{
        D3D12_RESIDENCY_FLAG_DENY_OVERBUDGET, D3D12_RESIDENCY_FLAGS,
    };

    use super::{Budget, Fence, FenceRead, Native, NativeCreate, Refusal, Residency};

    /// A native call the fake saw.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Call {
        Create(NativeCreate),
        Enqueue {
            id: u64,
            value: u64,
            flags: D3D12_RESIDENCY_FLAGS,
        },
        Import(u64),
    }

    /// What the test controls.
    #[derive(Debug, Default)]
    pub(crate) struct State {
        pub(crate) completed: u64,
        pub(crate) removed: bool,
        pub(crate) refuse_create: Option<Refusal>,
        pub(crate) refuse_enqueue: Option<Refusal>,
        pub(crate) budget: Option<Budget>,
        pub(crate) calls: Vec<Call>,
        next_id: u64,
    }

    /// The caller's handle on the fake: its fence held at 0 until
    /// [`complete`](Self::complete) moves it.
    #[derive(Clone, Default)]
    pub struct Fake(Arc<Mutex<State>>);

    impl Fake {
        pub(crate) fn state(&self) -> MutexGuard<'_, State> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// The fence completed up to `v`.
        pub fn complete(&self, v: u64) {
            self.state().completed = v;
        }

        /// The device removed: the fence reads [`REMOVED`](super::REMOVED)
        /// and the device reports its removal.
        #[cfg(feature = "dev-tools")]
        pub fn remove(&self) {
            let mut s = self.state();
            s.completed = super::REMOVED;
            s.removed = true;
        }

        /// How many native calls it saw.
        #[cfg(feature = "dev-tools")]
        #[must_use]
        pub fn calls(&self) -> usize {
            self.state().calls.len()
        }

        /// The values its residency asked to be signalled, in order.
        #[cfg(test)]
        pub(crate) fn enqueued(&self) -> Vec<u64> {
            self.state()
                .calls
                .iter()
                .filter_map(|c| match c {
                    Call::Enqueue { value, .. } => Some(*value),
                    _ => None,
                })
                .collect()
        }

        /// The creations it saw.
        #[cfg(test)]
        pub(crate) fn creates(&self) -> Vec<NativeCreate> {
            self.state()
                .calls
                .iter()
                .filter_map(|c| match c {
                    Call::Create(n) => Some(*n),
                    _ => None,
                })
                .collect()
        }

        /// The imports it made.
        #[cfg(test)]
        pub(crate) fn imports(&self) -> usize {
            self.state()
                .calls
                .iter()
                .filter(|c| matches!(c, Call::Import(_)))
                .count()
        }

        /// The renderer's path on it.
        pub(crate) fn residency(&self) -> Box<dyn Residency> {
            Box::new(FakeResidency {
                state: self.clone(),
                fence: Arc::new(FakeFence(self.clone())),
            })
        }
    }

    struct FakeFence(Fake);

    impl FenceRead for FakeFence {
        fn completed(&self) -> u64 {
            self.0.state().completed
        }

        fn removed(&self) -> bool {
            self.0.state().removed
        }
    }

    struct FakeResidency {
        state: Fake,
        fence: Fence,
    }

    impl Residency for FakeResidency {
        fn create(&mut self, n: NativeCreate) -> Result<Native, Refusal> {
            let mut s = self.state.state();
            s.calls.push(Call::Create(n));
            if let Some(r) = s.refuse_create {
                return Err(r);
            }
            s.next_id += 1;
            Ok(Native::Fake(s.next_id))
        }

        fn enqueue(
            &mut self,
            native: &Native,
            value: u64,
            flags: D3D12_RESIDENCY_FLAGS,
        ) -> Result<(), Refusal> {
            let mut s = self.state.state();
            let id = match native {
                Native::Fake(id) => *id,
                Native::D3d12(_) => 0,
            };
            s.calls.push(Call::Enqueue { id, value, flags });
            if let Some(r) = s.refuse_enqueue {
                return Err(r);
            }
            // Over budget, only the refusing flag refuses.
            if flags.contains(D3D12_RESIDENCY_FLAG_DENY_OVERBUDGET)
                && s.budget.is_some_and(|b| b.usage > b.budget)
            {
                return Err(Refusal::Failed);
            }
            Ok(())
        }

        fn fence(&self) -> &Fence {
            &self.fence
        }

        fn budget(&self) -> Option<Budget> {
            self.state.state().budget
        }

        fn native_size(&self, n: NativeCreate) -> u64 {
            n.bytes()
        }

        fn import(
            &mut self,
            device: &wgpu::Device,
            native: Native,
            desc: &wgpu::TextureDescriptor<'_>,
        ) -> wgpu::Texture {
            let id = match native {
                Native::Fake(id) => id,
                Native::D3d12(_) => 0,
            };
            self.state.state().calls.push(Call::Import(id));
            device.create_texture(desc)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use bms_model::{Deadline, TimeUs};
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT, D3D12_HEAP_FLAG_CREATE_NOT_ZEROED,
        D3D12_HEAP_TYPE_DEFAULT, D3D12_RESIDENCY_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST,
    };

    use super::{
        CREATE_BOUND, Completion, Fence, FenceRead, FenceValues, Native, NativeCreate, REMOVED,
        ResidencyHolder, Residing, capable, completion,
    };

    const KEY: NativeCreate = NativeCreate {
        side: 1_024,
        layers: 3,
        levels: 4,
    };

    /// A fence the test moves.
    #[derive(Default)]
    struct Moved {
        completed: AtomicU64,
        removed: AtomicBool,
    }

    impl FenceRead for Moved {
        fn completed(&self) -> u64 {
            self.completed.load(Ordering::SeqCst)
        }

        fn removed(&self) -> bool {
            self.removed.load(Ordering::SeqCst)
        }
    }

    fn residing(fence: &Arc<Moved>, value: u64, charge: u64) -> Residing {
        let f: Fence = Arc::clone(fence) as Fence;
        Residing {
            native: Native::Fake(value),
            fence: f,
            value,
            bytes: KEY.bytes(),
            charge,
            requested: TimeUs(0),
            deadline: Deadline::after(TimeUs(0), CREATE_BOUND),
        }
    }

    /// S-R1: the native creation is not resident.
    #[test]
    fn native_create_flags_are_not_resident() {
        assert!(
            KEY.heap_flags()
                .contains(D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT)
        );
        assert_eq!(KEY.heap_properties().Type, D3D12_HEAP_TYPE_DEFAULT);
        assert_eq!(KEY.initial_state(), D3D12_RESOURCE_STATE_COPY_DEST);
    }

    /// S-R11: its heap is zeroed: no `CREATE_NOT_ZEROED`.
    #[test]
    fn native_create_is_zeroed() {
        assert!(!KEY.heap_flags().contains(D3D12_HEAP_FLAG_CREATE_NOT_ZEROED));
        assert!(KEY.zeroed());
    }

    /// S-R7: residency is asked with no over-budget refusal.
    #[test]
    fn native_create_residency_flag_is_none() {
        assert_eq!(KEY.residency_flags(), D3D12_RESIDENCY_FLAG_NONE);
    }

    /// S-R13: the native description matches the `wgpu` descriptor field
    /// by field, and a descriptor that differs in any of them does not
    /// match.
    #[test]
    fn native_create_matches_the_descriptor() {
        let d = crate::skin_atlas::descriptor(KEY.side, KEY.layers, KEY.levels);
        assert!(KEY.matches(&d));
        let r = KEY.resource_desc();
        assert_eq!(r.Width, u64::from(KEY.side));
        assert_eq!(r.Height, KEY.side);
        assert_eq!(u32::from(r.DepthOrArraySize), KEY.layers);
        assert_eq!(u32::from(r.MipLevels), KEY.levels);
        let mut other = d.clone();
        other.size.width += 1;
        assert!(!KEY.matches(&other), "width");
        let mut other = d.clone();
        other.size.depth_or_array_layers += 1;
        assert!(!KEY.matches(&other), "layers");
        let mut other = d.clone();
        other.mip_level_count += 1;
        assert!(!KEY.matches(&other), "levels");
        let mut other = d.clone();
        other.format = wgpu::TextureFormat::Bgra8Unorm;
        assert!(!KEY.matches(&other), "format");
        let mut other = d.clone();
        other.usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
        assert!(!KEY.matches(&other), "usage");
        let mut other = d;
        other.sample_count = 4;
        assert!(!KEY.matches(&other), "samples");
    }

    /// S-R6: a reading of exactly `UINT64_MAX` is the device's removal,
    /// never a completion, whatever the value asked.
    #[test]
    fn a_removed_device_is_not_a_completion() {
        assert_eq!(completion(REMOVED, 1), Completion::Removed);
        assert_eq!(completion(REMOVED, REMOVED - 1), Completion::Removed);
        assert_eq!(completion(0, 1), Completion::Pending);
    }

    /// S-R6: values just below the reserved one and with the high bit set
    /// complete as any other.
    #[test]
    fn fence_values_near_the_top_are_completions() {
        assert_eq!(completion(REMOVED - 1, REMOVED - 1), Completion::Done);
        assert_eq!(completion(REMOVED - 1, 5), Completion::Done);
        let high = 1_u64 << 63;
        assert_eq!(completion(high, high), Completion::Done);
        assert_eq!(completion(high + 7, high), Completion::Done);
        assert_eq!(completion(high, high + 1), Completion::Pending);
        let mut v = FenceValues::new();
        v.set_next(REMOVED - 1);
        assert_eq!(v.take(), Some(REMOVED - 1));
        assert!(!v.left(), "the reserved value is never given");
        assert_eq!(v.take(), None);
    }

    /// S-R10: the capability needs `ID3D12Device3`.
    #[test]
    fn a_missing_device3_falls_back_to_sync() {
        assert!(capable(false, true, true).is_err());
    }

    /// S-R10: the heap flag needs `ID3D12Device8` or the OPTIONS7 query.
    #[test]
    fn a_missing_device8_falls_back_to_sync() {
        assert!(capable(true, false, false).is_err());
        assert!(capable(true, true, false).is_ok());
        assert!(capable(true, false, true).is_ok());
    }

    /// S-R6: the holder releases nothing before its fence completes; a
    /// confirmed removal (the fence reading `UINT64_MAX` and the device
    /// reporting itself removed) permits the release without it, and the
    /// reading alone does not.
    #[test]
    fn a_confirmed_removal_permits_terminal_cleanup() {
        let fence = Arc::new(Moved::default());
        let h = ResidencyHolder::default();
        h.hold(residing(&fence, 1, 10));
        h.hold(residing(&fence, 2, 20));
        assert_eq!(h.charge(), 30);
        assert!(!h.releasable());
        assert!(!h.release_one());
        fence.completed.store(REMOVED, Ordering::SeqCst);
        assert!(!h.releasable(), "the reading alone is no removal");
        assert!(!h.release_one());
        fence.removed.store(true, Ordering::SeqCst);
        assert!(h.release_one());
        assert!(h.release_one());
        assert_eq!(h.pending(), 0);
        assert_eq!(h.charge(), 0);
    }

    /// The holder releases one resource at a time, each only once its own
    /// value completed, its charge counted until then.
    #[test]
    fn the_holder_releases_each_once_its_fence_completes() {
        let fence = Arc::new(Moved::default());
        let h = ResidencyHolder::default();
        h.hold(residing(&fence, 1, 10));
        h.hold(residing(&fence, 2, 20));
        assert_eq!(h.oldest(), Some((2, TimeUs(0))));
        fence.completed.store(1, Ordering::SeqCst);
        assert!(h.release_one());
        assert!(!h.release_one(), "value 2 is still in flight");
        assert_eq!(h.charge(), 20);
        fence.completed.store(2, Ordering::SeqCst);
        assert!(h.release_one());
        assert_eq!(h.oldest(), None);
    }
}
