//! Instance, adapter and device setup on DX12.

use crate::RenderError;

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
}

impl Gpu {
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
/// object to the render thread, which waits on it itself (D9).
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
        .map_err(|e| RenderError::Adapter(e.to_string()))
    };
    match choice {
        AdapterChoice::HighPerformance => request(wgpu::PowerPreference::HighPerformance),
        AdapterChoice::LowPower => request(wgpu::PowerPreference::LowPower),
        AdapterChoice::Warp => {
            let mut all = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12));
            let infos: Vec<wgpu::AdapterInfo> = all.iter().map(wgpu::Adapter::get_info).collect();
            match first_software(infos.iter().map(|i| i.device_type)) {
                Some(i) => Ok(all.swap_remove(i)),
                None => {
                    let names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();
                    Err(RenderError::Adapter(format!(
                        "no WARP adapter among {names:?}"
                    )))
                }
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
            .map_err(|e| RenderError::Device(e.to_string()))?;
    Ok(Gpu {
        device,
        queue,
        info: adapter.get_info(),
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
