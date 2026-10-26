//! The window target on WARP: the swap chain (FIFO, one frame of latency,
//! the waitable object left to the caller) takes frames, and a window has
//! nothing to read back. Ignored by default: a swap chain needs an
//! interactive desktop, which a service session (such as a CI runner) may
//! not have. Run it with `--ignored` on the development PC.

mod common;

use std::num::NonZeroIsize;

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, WARP, input, lane, view};
use render::{AdapterChoice, Renderer, Target};
use wgpu::rwh::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    Win32WindowHandle, WindowHandle,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP, WS_VISIBLE,
};
use windows::core::w;

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
        .expect("create a window");
        Self(hwnd)
    }

    fn handle(&self) -> Handle {
        Handle(NonZeroIsize::new(self.0.0 as isize).expect("a window handle"))
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
        // SAFETY: the window outlives the renderer in the test below.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for Handle {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::windows())
    }
}

#[test]
#[ignore = "requires an interactive desktop"]
fn window_target_presents_and_reads_back_nothing() {
    let chart = ChartBuilder::new(120_000)
        .tap(lane(7), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let popup = Popup::new(480, 270);
    let mut r = Renderer::new(
        Target::Window {
            window: popup.handle().into(),
            width: 480,
            height: 270,
        },
        AdapterChoice::Warp,
    )
    .expect("WARP renderer on a window");
    assert_eq!(r.gpu().adapter().name, WARP);
    r.load(&chart).unwrap();
    assert!(r.has_frame_waitable());
    let v = view(0, 0);
    let mut presented = 0;
    for frame in 0..10 {
        assert!(r.wait_for_frame(1_000), "frame {frame} waited too long");
        let now = TimeUs(500 * MS + frame * 16_667);
        if r.frame(&input(&v, now, TimeUs(0), &[])).unwrap().presented {
            presented += 1;
        }
    }
    assert!(presented > 0);
    assert!(r.read_back().is_empty());
    drop(r);
    drop(popup);
}
