//! The window thread: the winit window and its events (D9). The window is
//! borderless fullscreen on the primary monitor and takes no device events
//! (Raw Input has one target window per device type per process, and the
//! input thread owns it). Focus, the retry and quit keys, the settings
//! keys F1 to F8 and closing go to [`Controls`]. The event loop runs on a thread of its own, so the game
//! keeps the main thread and the loaded chart never has to move.

use std::num::NonZeroIsize;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use game::{Adjust, Command};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, DeviceEvents, EventLoop, EventLoopProxy};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    Win32WindowHandle, WindowHandle,
};
use winit::window::{Fullscreen, Window, WindowId};

use crate::controls::Controls;

/// Asks the event loop to end.
struct Exit;

struct App {
    controls: Arc<Controls>,
    retry: Option<u32>,
    quit: Option<u32>,
    ready: Option<Sender<Result<Surface, String>>>,
    window: Option<Window>,
}

impl ApplicationHandler<Exit> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let Some(ready) = self.ready.take() else {
            return;
        };
        let attrs = Window::default_attributes()
            .with_title("tapestry")
            .with_fullscreen(Some(Fullscreen::Borderless(event_loop.primary_monitor())));
        match event_loop.create_window(attrs) {
            Ok(w) => {
                w.set_cursor_visible(false);
                self.controls.focus(w.has_focus());
                let surface = surface(&w);
                let _ = ready.send(surface);
                self.window = Some(w);
            }
            Err(e) => {
                let _ = ready.send(Err(format!("the window cannot be created: {e}")));
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::Focused(focused) => self.controls.focus(focused),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let code = event.physical_key.to_scancode();
                // A settings key steps again while held (key repeat);
                // retry and quit act on the first press only.
                if let Some(a) = code.and_then(Adjust::from_key) {
                    self.controls.tuning().adjust(a);
                } else if event.repeat {
                    // Retry and quit take the first press only.
                } else if code.is_some() && code == self.retry {
                    self.controls.command(Command::Retry);
                } else if code.is_some() && code == self.quit {
                    self.controls.command(Command::Quit);
                }
            }
            WindowEvent::CloseRequested => self.controls.close(),
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: Exit) {
        // The window goes before the loop ends, on its own thread.
        self.window = None;
        event_loop.exit();
    }
}

/// The running window: ends its thread when closed.
pub struct WindowThread {
    proxy: EventLoopProxy<Exit>,
    handle: JoinHandle<()>,
}

impl WindowThread {
    /// Closes the window and waits for its thread. Call it after every user
    /// of the window (the render thread) is done.
    pub fn close(self) {
        let _ = self.proxy.send_event(Exit);
        let _ = self.handle.join();
    }
}

/// Opens the window on its own thread. `retry` and `quit` are the keyboard
/// scan codes bound to them.
///
/// # Errors
///
/// Why the event loop or the window could not start.
pub fn open(
    controls: Arc<Controls>,
    retry: Option<u32>,
    quit: Option<u32>,
) -> Result<(WindowThread, Surface), String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (proxy_tx, proxy_rx) = mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("window".into())
        .spawn(move || {
            let event_loop = match EventLoop::<Exit>::with_user_event()
                .with_any_thread(true)
                .build()
            {
                Ok(l) => l,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("the event loop cannot start: {e}")));
                    return;
                }
            };
            event_loop.listen_device_events(DeviceEvents::Never);
            let _ = proxy_tx.send(event_loop.create_proxy());
            let mut app = App {
                controls: Arc::clone(&controls),
                retry,
                quit,
                ready: Some(ready_tx),
                window: None,
            };
            if let Err(e) = event_loop.run_app(&mut app) {
                eprintln!("the event loop failed: {e}");
            }
            // However the loop ended, the game quits.
            controls.close();
        })
        .map_err(|e| format!("the window thread cannot start: {e}"))?;
    let ended = || "the window thread ended".to_owned();
    let window = ready_rx.recv().map_err(|_| ended()).and_then(|r| r);
    // The loop sends its proxy before it makes the window.
    let proxy = proxy_rx.recv().map_err(|_| ended());
    let window = match (window, proxy) {
        (Ok(w), Ok(proxy)) => {
            let thread = WindowThread { proxy, handle };
            return Ok((thread, w));
        }
        (Err(e), Ok(proxy)) => {
            let _ = proxy.send_event(Exit);
            Err(e)
        }
        (Err(e), Err(_)) | (Ok(_), Err(e)) => Err(e),
    };
    let _ = handle.join();
    window
}

/// The window as the render thread's surface target: its handle and size,
/// read on the window thread, because winit hands out the handle only
/// there. The window outlives the renderer: [`WindowThread::close`] comes
/// after the render thread has ended.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    hwnd: NonZeroIsize,
    /// The client size in pixels.
    pub width: u32,
    /// See `width`.
    pub height: u32,
}

fn surface(window: &Window) -> Result<Surface, String> {
    let handle = window
        .window_handle()
        .map_err(|e| format!("the window has no handle: {e}"))?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return Err("the window is not a Win32 window".to_owned());
    };
    let size = window.inner_size();
    Ok(Surface {
        hwnd: h.hwnd,
        width: size.width,
        height: size.height,
    })
}

impl HasWindowHandle for Surface {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = RawWindowHandle::Win32(Win32WindowHandle::new(self.hwnd));
        // SAFETY: the window stays open until the renderer using this
        // handle is gone (see `Surface`).
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for Surface {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::windows())
    }
}
