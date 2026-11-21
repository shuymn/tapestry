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
    r.load(&chart, None).unwrap();
    common::prepare(&mut r, &chart);
    assert!(r.has_frame_waitable());
    let v = view(0, 0);
    let mut presented = 0;
    for frame in 0..10 {
        assert!(r.wait_for_frame(1_000), "frame {frame} waited too long");
        let now = TimeUs(500 * MS + frame * 16_667);
        if r.frame(&input(&v, now, TimeUs(0), &[]))
            .unwrap()
            .presented()
        {
            presented += 1;
        }
    }
    assert!(presented > 0);
    assert!(r.read_back().is_empty());
    drop(r);
    drop(popup);
}

/// A tap that copies the frames whose numbers it is given, and keeps what
/// the renderer hands it.
#[cfg(feature = "dev-tools")]
#[derive(Default)]
struct Tap {
    /// The frame being drawn.
    frame: std::sync::atomic::AtomicUsize,
    want: std::sync::Mutex<Vec<usize>>,
    presented: std::sync::Mutex<Vec<(usize, Option<u32>)>>,
    delivered: std::sync::Mutex<Vec<(usize, Result<render::PresentFrame, String>)>>,
}

#[cfg(feature = "dev-tools")]
impl render::PresentTap for Tap {
    fn wanted(&self) -> bool {
        let f = self.frame.load(std::sync::atomic::Ordering::SeqCst);
        self.want.lock().unwrap().contains(&f)
    }
    fn presented(&self, count: Option<u32>) {
        let f = self.frame.load(std::sync::atomic::Ordering::SeqCst);
        self.presented.lock().unwrap().push((f, count));
    }
    fn deliver(&self, frame: Result<render::PresentFrame, String>) {
        let f = self.frame.load(std::sync::atomic::Ordering::SeqCst);
        self.delivered.lock().unwrap().push((f, frame));
    }
}

/// A present tap on a real DX12 swap chain (the high-performance adapter,
/// then WARP) copies exactly the frame it is drawn into: each frame fills
/// the window with its own colour, and each copy, of two successive frames
/// and a later one, holds that frame's colour in every pixel at the window's
/// size, with the frame's present count, each count after the one before.
/// A frame the tap does not want is not copied.
#[cfg(feature = "dev-tools")]
#[test]
#[ignore = "requires an interactive desktop"]
fn a_present_tap_copies_the_frame_it_presents() {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use render::text::MenuText;
    use render::{Blend, Sprite};

    let colour = |f: usize| -> [u8; 3] {
        let f = u8::try_from(f).unwrap();
        [40 + 20 * f, 200 - 15 * f, 30 + 10 * (f % 7)]
    };
    for choice in [AdapterChoice::HighPerformance, AdapterChoice::Warp] {
        let popup = Popup::new(480, 270);
        let mut r = Renderer::new(
            Target::Window {
                window: popup.handle().into(),
                width: 480,
                height: 270,
            },
            choice,
        )
        .expect("a renderer on a window");
        let tap = Arc::new(Tap::default());
        *tap.want.lock().unwrap() = vec![3, 4, 7];
        r.set_present_tap(Arc::clone(&tap) as Arc<dyn render::PresentTap>)
            .unwrap();
        let text = MenuText::new(&r);
        for f in 0..9 {
            assert!(
                r.wait_for_frame(1_000),
                "{choice:?}: frame {f} waited too long"
            );
            tap.frame.store(f, Ordering::SeqCst);
            let [cr, cg, cb] = colour(f).map(|c| f32::from(c) / 255.0);
            let timing = r
                .menu_frame(
                    |b| {
                        b.push(
                            Blend::Alpha,
                            Sprite {
                                rect: [0.0, 0.0, 1920.0, 1080.0],
                                uv: render::solid_uv(),
                                color: [cr, cg, cb, 1.0],
                                ..Sprite::default()
                            },
                        );
                    },
                    &text,
                )
                .unwrap();
            assert!(timing.presented(), "{choice:?}: frame {f}");
        }
        let delivered = std::mem::take(&mut *tap.delivered.lock().unwrap());
        assert_eq!(
            delivered.iter().map(|(f, _)| *f).collect::<Vec<_>>(),
            [3, 4, 7],
            "{choice:?}"
        );
        for (f, frame) in delivered {
            let frame = frame.unwrap_or_else(|e| panic!("{choice:?}: frame {f}: {e}"));
            assert_eq!((frame.width, frame.height), (480, 270), "{choice:?}");
            assert_eq!(frame.rgba.len(), 480 * 270 * 4, "{choice:?}");
            let want = colour(f);
            let off: Vec<usize> = frame
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .enumerate()
                .filter(|(_, p)| p.iter().zip(want).any(|(&got, w)| got.abs_diff(w) > 1))
                .map(|(i, _)| i)
                .take(3)
                .collect();
            assert!(
                off.is_empty(),
                "{choice:?}: frame {f} is not its colour {want:?} at {off:?}"
            );
        }
        let presented = std::mem::take(&mut *tap.presented.lock().unwrap());
        assert_eq!(
            presented.iter().map(|(f, _)| *f).collect::<Vec<_>>(),
            [3, 4, 7],
            "{choice:?}"
        );
        let counts: Vec<u32> = presented.iter().map(|(_, c)| c.unwrap()).collect();
        assert!(counts.is_sorted_by(|a, b| a < b), "{choice:?}: {counts:?}");
        drop(text);
        drop(r);
        drop(popup);
    }
}
