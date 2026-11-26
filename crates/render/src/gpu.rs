//! Instance, adapter and device setup on DX12.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::RenderError;

/// The queue's submissions, numbered by the one function that makes them
/// ([`Gpu::submit`]), and the newest number seen complete (design.md
/// "Memory Budget": a retired array is destroyed only once its last use is
/// covered).
#[derive(Debug, Default)]
struct Submissions {
    /// The number the last submission took; numbers start at 1.
    count: AtomicU64,
    /// The newest number seen complete: by a wait for the queue, or by the
    /// rolling watch's poll ([`Gpu::poll_watch`]).
    seen: AtomicU64,
    /// A write is queued that no submission has carried yet: the next
    /// submission, whatever makes it, carries it.
    queued: AtomicBool,
    /// The last submission's number and the queue's index for it.
    latest: Mutex<Option<(u64, wgpu::SubmissionIndex)>>,
    /// The writes queued so far, every one through the uploader's write
    /// function (`upload`).
    writes: AtomicU64,
}

/// What a non-blocking look at a watched submission found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watched {
    /// Complete: every submission up to the number given is done.
    Done(u64),
    /// Not complete yet.
    Pending,
}

/// Which DX12 adapter to render with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterChoice {
    /// The default high-performance hardware adapter.
    HighPerformance,
    /// The low-power adapter (an integrated GPU when there is one).
    LowPower,
    /// WARP, the DX12 software rasterizer ("Microsoft Basic Render
    /// Driver"), chosen explicitly for tests (design.md "Testing Strategy").
    Warp,
}

/// The device and queue a skin prepares its resources on.
#[derive(Clone, Debug)]
pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    info: wgpu::AdapterInfo,
    /// Set by the device's lost callback: on the pinned wgpu a device lost
    /// or destroyed while writes are queued drops them and a wait still
    /// returns, so a write that must reach the GPU checks this after its
    /// wait (the chart images' publication, S10-12).
    lost: Arc<AtomicBool>,
    /// The numbered submissions, shared by every clone of this device.
    submissions: Arc<Submissions>,
}

impl Gpu {
    /// Submits `buffers` and numbers the submission: every submission the
    /// renderer makes goes through here, whatever its site, so the number a
    /// use records is never earlier than the submission that carries it.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn submit<I: IntoIterator<Item = wgpu::CommandBuffer>>(
        &self,
        buffers: I,
    ) -> (u64, wgpu::SubmissionIndex) {
        let index = self.queue.submit(buffers);
        let s = &self.submissions;
        let number = s.count.fetch_add(1, Ordering::AcqRel) + 1;
        s.queued.store(false, Ordering::Release);
        *s.latest.lock().unwrap_or_else(PoisonError::into_inner) = Some((number, index.clone()));
        (number, index)
    }

    /// The number of the submission that will carry a write queued now with
    /// no submission of its own (a pool's creation or row, a decision label
    /// row): the next one, whatever makes it.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn queued_use(&self) -> u64 {
        let s = &self.submissions;
        s.queued.store(true, Ordering::Release);
        s.count.load(Ordering::Acquire) + 1
    }

    /// Counts a write queued ([`crate::upload`]'s write function alone).
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn count_write(&self) {
        self.submissions.writes.fetch_add(1, Ordering::Relaxed);
    }

    /// The writes queued on this device so far.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn writes(&self) -> u64 {
        self.submissions.writes.load(Ordering::Relaxed)
    }

    /// Whether a write is queued that no submission has carried yet.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn write_queued(&self) -> bool {
        self.submissions.queued.load(Ordering::Acquire)
    }

    /// The number the last submission took (0 before any).
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn latest(&self) -> u64 {
        self.submissions.count.load(Ordering::Acquire)
    }

    /// The newest submission number seen complete.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn seen(&self) -> u64 {
        self.submissions.seen.load(Ordering::Acquire)
    }

    /// Records that every submission up to `number` is complete.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn saw(&self, number: u64) {
        self.submissions.seen.fetch_max(number, Ordering::AcqRel);
    }

    /// The last submission's number and index, to watch it.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn latest_index(&self) -> Option<(u64, wgpu::SubmissionIndex)> {
        self.submissions
            .latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Looks, without blocking, whether the watched submission `index`
    /// (number `number`) is complete, and records what it finds: a queue
    /// found empty covers every submission made before the look.
    ///
    /// # Errors
    ///
    /// The device's error from the poll (a lost device among them).
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn poll_watch(
        &self,
        number: u64,
        index: &wgpu::SubmissionIndex,
    ) -> Result<Watched, wgpu::PollError> {
        let before = self.latest();
        match self.device.poll(wgpu::PollType::Wait {
            submission_index: Some(index.clone()),
            timeout: Some(Duration::ZERO),
        }) {
            Ok(wgpu::PollStatus::QueueEmpty) => {
                let done = before.max(number);
                self.saw(done);
                Ok(Watched::Done(done))
            }
            Ok(_) => {
                self.saw(number);
                Ok(Watched::Done(number))
            }
            Err(wgpu::PollError::Timeout) => Ok(Watched::Pending),
            Err(e) => Err(e),
        }
    }

    /// Latches the loss: a fence read the device's removal.
    pub(crate) fn mark_lost(&self) {
        self.lost.store(true, Ordering::Release);
    }

    /// Whether the device was lost or destroyed since it was created.
    #[must_use]
    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    /// The device.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The queue.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// The adapter in use, as reported by the driver.
    #[must_use]
    pub const fn adapter(&self) -> &wgpu::AdapterInfo {
        &self.info
    }
}

/// A DX12-only instance whose swap chains leave the frame latency waitable
/// object to the render thread, which waits on it itself (design.md "Display
/// Latency and Stutter").
pub(crate) fn instance() -> wgpu::Instance {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::DX12;
    desc.backend_options.dx12.latency_waitable_object =
        wgpu::Dx12UseFrameLatencyWaitableObject::DontWait;
    wgpu::Instance::new(desc)
}

pub(crate) fn adapter(
    instance: &wgpu::Instance,
    choice: AdapterChoice,
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<wgpu::Adapter, RenderError> {
    let request = |power_preference| {
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference,
            force_fallback_adapter: false,
            compatible_surface: surface,
            apply_limit_buckets: false,
        }))
        .map_err(RenderError::Adapter)
    };
    match choice {
        AdapterChoice::HighPerformance => request(wgpu::PowerPreference::HighPerformance),
        AdapterChoice::LowPower => request(wgpu::PowerPreference::LowPower),
        AdapterChoice::Warp => {
            let mut all = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12));
            let infos: Vec<wgpu::AdapterInfo> = all.iter().map(wgpu::Adapter::get_info).collect();
            match first_software(infos.iter().map(|i| i.device_type)) {
                Some(i) => Ok(all.swap_remove(i)),
                None => Err(RenderError::NoSoftwareAdapter {
                    names: infos.into_iter().map(|i| i.name).collect(),
                }),
            }
        }
    }
}

/// The index of the first software (WARP) adapter.
fn first_software(types: impl Iterator<Item = wgpu::DeviceType>) -> Option<usize> {
    types
        .enumerate()
        .find_map(|(i, t)| (t == wgpu::DeviceType::Cpu).then_some(i))
}

pub(crate) fn device(adapter: &wgpu::Adapter) -> Result<Gpu, RenderError> {
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(RenderError::Device)?;
    let lost = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&lost);
    device.set_device_lost_callback(move |_, _| flag.store(true, Ordering::Release));
    // The transition probe's memory readings (`TAPESTRY_LOOP_PROBE`).
    #[cfg(feature = "dev-tools")]
    if crate::probe::on()
        // SAFETY: the adapter outlives this call; the raw adapter is cloned (AddRef).
        && let Some(a) = unsafe { adapter.as_hal::<wgpu_hal::api::Dx12>() }
    {
        crate::probe::set_dxgi(a.as_raw().clone());
    }
    Ok(Gpu {
        device,
        queue,
        info: adapter.get_info(),
        lost,
        submissions: Arc::default(),
    })
}

#[cfg(test)]
mod tests {
    use wgpu::DeviceType::{Cpu, DiscreteGpu, IntegratedGpu, Other};

    use super::first_software;

    #[test]
    fn warp_is_the_software_adapter_or_none() {
        assert_eq!(
            first_software([DiscreteGpu, IntegratedGpu].into_iter()),
            None
        );
        assert_eq!(first_software([].into_iter()), None);
        assert_eq!(
            first_software([DiscreteGpu, Other, Cpu, Cpu].into_iter()),
            Some(2)
        );
    }
}
