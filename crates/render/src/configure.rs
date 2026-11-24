//! The window surface's configuration (design.md "Showing the window"):
//! every `Surface::configure` runs inside a validation scope, so a refusal
//! is a typed [`RenderError::SurfaceConfigure`] the caller recovers from,
//! never wgpu's uncaptured-error handler, which panics. A refusal is
//! classified by wgpu-core's reason: one a surface and device made again
//! may pass (DXGI refused the swap chain, or the wait for the GPU timed
//! out) is retryable; one the configuration itself causes is not.

#[cfg(any(test, feature = "dev-tools"))]
use std::sync::atomic::{AtomicU32, Ordering};

use wgpu::wgc::present::ConfigureSurfaceError as Refusal;

use crate::RenderError;

/// Configures `surface` with `config` on `device` inside a validation and
/// an out-of-memory scope (the scopes [`crate::skin_atlas::scoped`] pushes),
/// after the checks wgpu panics on instead of reporting: a zero side, or
/// one past the device's largest texture.
///
/// # Errors
///
/// [`RenderError::SurfaceConfigure`] with what refused it.
pub(crate) fn configure(
    surface: &wgpu::Surface<'_>,
    device: &wgpu::Device,
    config: &wgpu::SurfaceConfiguration,
) -> Result<(), RenderError> {
    let max = device.limits().max_texture_dimension_2d;
    if config.width == 0 || config.height == 0 {
        return Err(RenderError::surface_configure(refusal(Refusal::ZeroArea)));
    }
    if config.width > max || config.height > max {
        return Err(RenderError::surface_configure(refusal(Refusal::TooLarge {
            width: config.width,
            height: config.height,
            max_texture_dimension_2d: max,
        })));
    }
    #[cfg(any(test, feature = "dev-tools"))]
    if take_fault() {
        return Err(RenderError::surface_configure(refusal(
            Refusal::InvalidSurface,
        )));
    }
    let ((), refused) = crate::skin_atlas::scoped(device, || surface.configure(device, config));
    match refused {
        Some(e) => Err(RenderError::surface_configure(e)),
        None => Ok(()),
    }
}

/// Whether the configure refusal `e` may pass on a surface and device
/// made again: DXGI refused the swap chain (wgpu-core's `InvalidSurface`,
/// its mapping of an outdated, lost, occluded or timed-out surface and of
/// a refused creation), the wait for the GPU before it timed out, or the
/// device failed. Every other reason is the configuration's and stays.
pub(crate) fn retryable(e: &wgpu::Error) -> bool {
    let mut next: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(cause) = next {
        if let Some(r) = cause.downcast_ref::<Refusal>() {
            return matches!(
                r,
                Refusal::InvalidSurface | Refusal::GpuWaitTimeout | Refusal::Device(_)
            );
        }
        next = cause.source();
    }
    false
}

/// A refusal for `reason` in the shape wgpu's validation scope reports
/// one: a validation error whose source is wgpu-core's context of
/// `Surface::configure` around the reason.
pub(crate) fn refusal(reason: Refusal) -> wgpu::Error {
    let description = reason.to_string();
    wgpu::Error::Validation {
        source: Box::new(wgpu::wgc::error::ContextError {
            fn_ident: "Surface::configure",
            source: Box::new(reason),
            label: String::new(),
        }),
        description,
    }
}

/// Configures refused from now on, each taking one: the fault
/// `TAPESTRY_DEV_CONFIGURE_FAULTS` sets (dev-tools) and tests.
#[cfg(any(test, feature = "dev-tools"))]
static FAULTS: AtomicU32 = AtomicU32::new(0);

/// Refuses the next `n` configures that pass the checks, as DXGI's
/// refusal of the swap chain (`InvalidSurface`), wherever they come from
/// (dev-tools, tests).
#[cfg(any(test, feature = "dev-tools"))]
pub fn refuse_configures(n: u32) {
    FAULTS.store(n, Ordering::SeqCst);
}

/// Takes one refusal of [`refuse_configures`], if any is left.
#[cfg(any(test, feature = "dev-tools"))]
fn take_fault() -> bool {
    FAULTS
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

/// What runs before each new renderer's first configure of a window
/// (dev-tools): the startup cause probe's gate and its record of the
/// window's state.
#[cfg(feature = "dev-tools")]
static BEFORE_FIRST: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

/// Runs `hook` before each new renderer's first configure of a window,
/// from now on; the first hook set stays (dev-tools).
#[cfg(feature = "dev-tools")]
pub fn on_first_configure(hook: Box<dyn Fn() + Send + Sync>) {
    let _ = BEFORE_FIRST.set(hook);
}

/// Runs the hook [`on_first_configure`] set, if any.
#[cfg(feature = "dev-tools")]
pub(crate) fn before_first_configure() {
    if let Some(hook) = BEFORE_FIRST.get() {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::{Refusal, refusal, retryable};
    use crate::RenderError;

    /// Each reason wgpu-core gives, wrapped as the scope returns it, is
    /// classified: a refused or lost swap chain, the GPU wait and the
    /// device are retryable; the configuration's own faults are not.
    #[test]
    fn configure_refusals_are_classified() {
        let format = wgpu::TextureFormat::Bgra8Unorm;
        let cases = [
            (Refusal::InvalidSurface, true),
            (Refusal::GpuWaitTimeout, true),
            (Refusal::Device(wgpu::wgc::device::DeviceError::Lost), true),
            (Refusal::ZeroArea, false),
            (
                Refusal::TooLarge {
                    width: 9,
                    height: 9,
                    max_texture_dimension_2d: 8,
                },
                false,
            ),
            (Refusal::PreviousOutputExists, false),
            (Refusal::UnsupportedQueueFamily, false),
            (
                Refusal::InvalidViewFormat(wgpu::TextureFormat::Rgba16Float, format),
                false,
            ),
        ];
        for (reason, expected) in cases {
            let text = reason.to_string();
            let e = RenderError::surface_configure(refusal(reason));
            assert!(
                matches!(e, RenderError::SurfaceConfigure { retryable: r, .. } if r == expected),
                "{text}: {e:?}"
            );
            assert_eq!(e.is_retryable_configure(), expected, "{text}");
        }
        // Not wgpu-core's refusal at all: not retryable.
        let other = wgpu::Error::Validation {
            source: Box::new(std::io::Error::other("elsewhere")),
            description: "elsewhere".to_owned(),
        };
        assert!(!retryable(&other));
    }
}
