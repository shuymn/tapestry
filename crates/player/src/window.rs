//! The window thread: the winit window and its events (D9). The window is
//! borderless fullscreen on the primary monitor and takes no device events
//! (Raw Input has one target window per device type per process, and the
//! input thread owns it). Focus, the retry and quit keys and closing go to
//! [`Controls`]. The event loop runs on a thread of its own, so the game
//! keeps the main thread and the loaded chart never has to move.

use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use game::Command;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, DeviceEvents, EventLoop, EventLoopProxy};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::window::{Fullscreen, Window, WindowId};

use crate::controls::Controls;

/// Asks the event loop to end.
struct Exit;

struct App {
    controls: Arc<Controls>,
    retry: Option<u32>,
    quit: Option<u32>,
    ready: Option<Sender<Result<Arc<Window>, String>>>,
    window: Option<Arc<Window>>,
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
                let window = Arc::new(w);
                window.set_cursor_visible(false);
                self.controls.focus(window.has_focus());
                self.window = Some(Arc::clone(&window));
                let _ = ready.send(Ok(window));
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
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                let code = event.physical_key.to_scancode();
                if code.is_some() && code == self.retry {
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
    handle: Option<JoinHandle<()>>,
}

impl WindowThread {
    /// Closes the window and waits for its thread. Call it after every user
    /// of the window (the render thread) is done.
    pub fn close(mut self) {
        let _ = self.proxy.send_event(Exit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
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
) -> Result<(WindowThread, Arc<Window>), String> {
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
            let thread = WindowThread {
                proxy,
                handle: Some(handle),
            };
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
