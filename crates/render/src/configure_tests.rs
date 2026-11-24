//! The window surface's configuration on a real swap chain: a window on a
//! desktop of the test's own (nothing on the screen, no foreground taken),
//! a WARP renderer on it, and a configure the surface refuses after every
//! check that comes before the call (design.md "Showing the window").

use std::num::NonZeroIsize;

use wgpu::rwh::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    Win32WindowHandle, WindowHandle,
};
use windows::Win32::Foundation::{GENERIC_ALL, HWND};
use windows::Win32::System::StationsAndDesktops::{
    CreateDesktopW, DESKTOP_CONTROL_FLAGS, SetThreadDesktop,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use super::{Output, Renderer, Target};
use crate::{AdapterChoice, RenderError};

/// Moves this thread, which has no window yet, to a new desktop of its
/// own: its windows are seen on no screen and take no foreground. The
/// desktop goes with the process.
fn own_desktop() {
    let name: Vec<u16> = format!("tapestry-render-test-{}", std::process::id())
        .encode_utf16()
        .chain([0])
        .collect();
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let desktop = unsafe {
        CreateDesktopW(
            PCWSTR(name.as_ptr()),
            PCWSTR::null(),
            None,
            DESKTOP_CONTROL_FLAGS(0),
            GENERIC_ALL.0,
            None,
        )
    }
    .unwrap();
    // SAFETY: the desktop was just made, and this thread has no window or
    // hook.
    unsafe { SetThreadDesktop(desktop) }.unwrap();
}

/// A borderless popup, destroyed on drop.
struct Popup(HWND);

impl Popup {
    fn new(width: i32, height: i32) -> Self {
        // SAFETY: a predefined class, no parent, menu or creation data.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("render test"),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                width,
                height,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        Self(hwnd)
    }

    fn target(&self, width: u32, height: u32) -> Target {
        Target::Window {
            window: Handle(NonZeroIsize::new(self.0.0 as isize).unwrap()).into(),
            width,
            height,
        }
    }
}

impl Drop for Popup {
    fn drop(&mut self) {
        // SAFETY: the window was created on this thread and is destroyed
        // once, after the renderer using it.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// The window's handle, for wgpu.
struct Handle(NonZeroIsize);

impl HasWindowHandle for Handle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = RawWindowHandle::Win32(Win32WindowHandle::new(self.0));
        // SAFETY: each test's window outlives its renderer.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for Handle {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::windows())
    }
}

/// Runs `f` on a thread of its own on a desktop of its own, so the
/// process's other threads keep theirs.
fn on_own_desktop(f: impl FnOnce() + Send) {
    std::thread::scope(|s| {
        s.spawn(|| {
            own_desktop();
            f();
        })
        .join()
        .unwrap();
    });
}

/// W4.1: a configuration that passes every check made before the call
/// and that wgpu-core's validation refuses (a view format the surface's
/// format cannot be viewed as) is a permanent configure refusal, returned:
/// without the validation scope, wgpu's uncaptured-error handler panics.
#[test]
fn a_refused_configure_is_an_error() {
    on_own_desktop(|| {
        let popup = Popup::new(320, 180);
        let mut r = Renderer::new(popup.target(320, 180), AdapterChoice::Warp).unwrap();
        let device = r.gpu.device().clone();
        let Output::Window(w) = &mut r.output else {
            panic!("a window renderer has a window");
        };
        w.config.view_formats = vec![wgpu::TextureFormat::Rgba16Float];
        let e = w.configure(&device).unwrap_err();
        assert!(
            matches!(
                e,
                RenderError::SurfaceConfigure {
                    retryable: false,
                    ..
                }
            ),
            "{e:?}"
        );
        drop(r);
    });
}

/// W4.1: a resize the surface refuses keeps the renderer's old size, and
/// its next frame returns the refusal, so the screen is built again.
#[test]
fn a_refused_resize_keeps_the_old_size() {
    on_own_desktop(|| {
        let popup = Popup::new(320, 180);
        let mut r = Renderer::new(popup.target(320, 180), AdapterChoice::Warp).unwrap();
        crate::refuse_configures(1);
        r.resize(160, 90);
        assert_eq!(r.size(), (320, 180));
        let e = r.surface_refused.take().unwrap();
        assert!(e.is_retryable_configure(), "{e:?}");
        let Output::Window(w) = &r.output else {
            panic!("a window renderer has a window");
        };
        assert_eq!((w.config.width, w.config.height), (320, 180));
        // The refusal is taken: the next resize configures.
        r.resize(160, 90);
        assert_eq!(r.size(), (160, 90));
        assert!(r.surface_refused.is_none());
        drop(r);
    });
}

/// W4.3: a refusal injected as DXGI's reaches whoever makes the renderer
/// through its first configure, and the next one is made.
#[test]
fn an_injected_refusal_reaches_the_caller() {
    on_own_desktop(|| {
        let popup = Popup::new(320, 180);
        crate::refuse_configures(1);
        let e = Renderer::new(popup.target(320, 180), AdapterChoice::Warp)
            .err()
            .unwrap();
        assert!(
            matches!(
                e,
                RenderError::SurfaceConfigure {
                    retryable: true,
                    ..
                }
            ),
            "{e:?}"
        );
        let r = Renderer::new(popup.target(320, 180), AdapterChoice::Warp).unwrap();
        drop(r);
    });
}
