//! A real window for the tests that need a swap chain (tests): a plain
//! popup on a desktop of the test thread's own, so nothing appears on the
//! screen and no foreground is taken, and a window target on it. A frame
//! presented into it is counted by the swap chain, which a window's first
//! present needs (design.md "Showing the window"); an offscreen frame is
//! never counted.

use std::num::NonZeroIsize;

use windows::Win32::Foundation::{GENERIC_ALL, HWND};
use windows::Win32::System::StationsAndDesktops::{
    CreateDesktopW, DESKTOP_CONTROL_FLAGS, SetThreadDesktop,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};
use winit::raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    Win32WindowHandle, WindowHandle,
};

/// Moves this thread, which has no window yet, to a new desktop of its
/// own: its windows are seen on no screen and take no foreground on the
/// user's desktop. The desktop goes with the process.
pub(crate) fn own_desktop() {
    let name: Vec<u16> = format!("tapestry-test-{}", std::process::id())
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

/// A borderless popup, destroyed on drop: make it on the thread that
/// drops it, after [`own_desktop`], and drop it after its renderer.
pub(crate) struct Popup(HWND);

impl Popup {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        let (w, h) = (
            i32::try_from(width).unwrap(),
            i32::try_from(height).unwrap(),
        );
        // SAFETY: a predefined class, no parent, menu or creation data.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("player test"),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                w,
                h,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        Self(hwnd)
    }

    /// A window target of `width`×`height` on the popup.
    pub(crate) fn target(&self, width: u32, height: u32) -> render::Target {
        render::Target::Window {
            window: Handle(NonZeroIsize::new(self.0.0 as isize).unwrap()).into(),
            width,
            height,
        }
    }
}

impl Drop for Popup {
    fn drop(&mut self) {
        // SAFETY: the window was made on this thread and is destroyed once,
        // after the renderer using it.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// The popup's handle, for wgpu.
struct Handle(NonZeroIsize);

impl HasWindowHandle for Handle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = RawWindowHandle::Win32(Win32WindowHandle::new(self.0));
        // SAFETY: each test's popup outlives its renderer.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for Handle {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::windows())
    }
}

/// Runs `f` on a thread of its own on a desktop of its own, so the test's
/// other threads keep theirs.
pub(crate) fn on_own_desktop<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|s| {
        s.spawn(|| {
            own_desktop();
            f()
        })
        .join()
        .unwrap()
    })
}
