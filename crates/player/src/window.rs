//! The window thread: the winit window and its events (design.md "Rendering and
//! Skins"). The window is borderless fullscreen on the primary monitor and
//! takes no device events (Raw Input has one target window per device type per
//! process, and the input thread owns it). It is made hidden and shown once
//! the render thread presented its first frame, or after [`SHOW_TIMEOUT`]
//! at the latest ([`Showing`]), so the empty window Windows would paint
//! first is never seen; a play's recording waits for the show
//! ([`WindowShown`]). Focus, the retry and quit keys, the settings keys F1
//! to F8 and closing go to [`Controls`], which wakes the game
//! thread for each, and the input thread on a focus change once the focus
//! latch is set (design.md "Input"). With song selection the menu keys and
//! the text typed or composed through the IME go to the select screen too
//! ([`crate::select::MenuInput`], design.md "Play Lifecycle"), through a
//! queue that drops only keys past its bound ([`MenuQueue`]); the IME is
//! allowed only while the search box has focus, and told where its caret
//! is ([`ImeState`]) so the candidate window opens beside it. The client
//! size in physical pixels, at the window's making, on each resize and
//! each DPI change, goes to the render thread through the slot of the
//! newest size ([`Extent`]); a minimized window publishes zero sides. The
//! event loop runs on a thread of its own, so the game keeps the main
//! thread and the loaded chart never has to move.

use std::num::NonZeroIsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use eval::ControlId;
use game::{Adjust, Command};
use input::WindowKeyEvent;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::monitor::MonitorHandle;
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    Win32WindowHandle, WindowHandle,
};
use winit::window::{Fullscreen, Window, WindowId};

use crate::controls::Controls;
use crate::menu_screen::WindowSink;
use crate::render_side::Extent;
use crate::select::{MenuInput, MenuKey, MenuQueue};
use crate::tuning::KeyEvent;

/// What the game and render threads ask of the window thread.
enum Ask {
    /// End the event loop.
    Exit,
    /// Allow the IME (the search box has focus), or stop it.
    Ime(bool),
    /// The caret moved: its area is in [`Shared::caret`] (from the render
    /// thread).
    Caret,
}

/// What the window thread shares with the threads that draw into the
/// window and wait for it.
#[derive(Debug, Default)]
struct Shared {
    /// Set by a frame presented: the hidden window looks at it every
    /// [`SHOW_POLL`] until it is shown. A flag, so a play's frame tells it
    /// with nothing allocated.
    presented: Arc<AtomicBool>,
    /// The caret's latest area in layout pixels: moves are coalesced, so
    /// the window takes the latest only.
    caret: Mutex<Option<[f32; 4]>>,
    /// An [`Ask::Caret`] is on its way for [`caret`](Self::caret).
    caret_queued: AtomicBool,
    /// The client size, for the render thread.
    extent: Arc<Extent>,
    /// Whether the window was shown, for [`WindowShown`].
    shown: Mutex<bool>,
    shown_changed: Condvar,
}

impl Shared {
    fn mark_shown(&self) {
        *self.shown.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.shown_changed.notify_all();
    }
}

/// The longest the window stays hidden waiting for its first frame: a
/// render thread that never presents must not leave a window nobody can
/// see or close.
pub const SHOW_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the hidden window looks for its first frame.
const SHOW_POLL: Duration = Duration::from_millis(10);

/// What showing the window needs of it: [`Window`], or a fake in tests.
trait ShowWindow {
    /// Shows the window and brings it to the foreground.
    fn show(&self);
    fn has_focus(&self) -> bool;
}

impl ShowWindow for Window {
    fn show(&self) {
        self.set_visible(true);
        // Made hidden, the window was not brought to the foreground when
        // it was made fullscreen.
        self.focus_window();
    }

    fn has_focus(&self) -> bool {
        Window::has_focus(self)
    }
}

/// Whether the hidden window was shown: on the first frame presented, or
/// at its deadline without one.
#[derive(Debug)]
struct Showing {
    shown: bool,
    deadline: Instant,
}

impl Showing {
    fn new(now: Instant) -> Self {
        Self {
            shown: false,
            deadline: now + SHOW_TIMEOUT,
        }
    }

    /// Shows `window` once a frame was presented or the deadline passed
    /// at `now`; when it waits on, the time to look again, `None` once it
    /// is shown.
    fn poll(
        &mut self,
        window: &impl ShowWindow,
        controls: &Controls,
        shared: &Shared,
        now: Instant,
    ) -> Option<Instant> {
        if self.shown {
            return None;
        }
        if shared.presented.load(Ordering::Acquire) {
            self.show(window, controls, shared);
            return None;
        }
        if now >= self.deadline {
            println!("window: no frame within {SHOW_TIMEOUT:?}; shown anyway");
            self.show(window, controls, shared);
            return None;
        }
        Some((now + SHOW_POLL).min(self.deadline))
    }

    /// Shows `window` and tells `controls` whether it has focus, so the
    /// focus latch and the input thread see the window gain focus even
    /// when no focus event follows the show; then wakes whoever waits for
    /// the show.
    fn show(&mut self, window: &impl ShowWindow, controls: &Controls, shared: &Shared) {
        self.shown = true;
        window.show();
        controls.focus(window.has_focus());
        shared.mark_shown();
    }
}

/// What the window's IME needs of the window: [`Window`], or a fake in
/// tests.
trait ImeWindow {
    /// The client size in pixels.
    fn client_size(&self) -> (u32, u32);
    fn allow_ime(&self, allowed: bool);
    /// The caret's area in client pixels: the IME opens its candidate
    /// window beside it.
    fn ime_area(&self, at: PhysicalPosition<i32>, size: PhysicalSize<u32>);
}

impl ImeWindow for Window {
    fn client_size(&self) -> (u32, u32) {
        let s = self.inner_size();
        (s.width, s.height)
    }

    fn allow_ime(&self, allowed: bool) {
        self.set_ime_allowed(allowed);
    }

    fn ime_area(&self, at: PhysicalPosition<i32>, size: PhysicalSize<u32>) {
        self.set_ime_cursor_area(at, size);
    }
}

/// The IME's state on the window thread: whether it is allowed and where
/// the caret is. The caret's area is reported to the IME whenever it is
/// allowed, the caret moves or the window's size changes, so its
/// candidate window opens beside the search box, not where Windows would
/// guess; a context the IME is not allowed has nothing to report to (the
/// window has no input context then).
#[derive(Debug, Default)]
struct ImeState {
    allowed: bool,
    /// The caret's area in layout pixels, as the last page drawn has it.
    caret: Option<[f32; 4]>,
}

impl ImeState {
    /// The IME allowed (the search box gained focus) or stopped (lost it).
    fn allow(&mut self, window: &impl ImeWindow, allowed: bool) {
        self.allowed = allowed;
        window.allow_ime(allowed);
        self.report(window);
    }

    /// The caret moved, or no text box has focus any more (`None`).
    fn caret(&mut self, window: &impl ImeWindow, caret: Option<[f32; 4]>) {
        self.caret = caret;
        self.report(window);
    }

    /// Reports the caret's area to an allowed IME at the window's current
    /// size (also on a resize).
    fn report(&self, window: &impl ImeWindow) {
        let Some(caret) = self.caret.filter(|_| self.allowed) else {
            return;
        };
        let (width, height) = window.client_size();
        let (at, size) = client_area(caret, width, height);
        window.ime_area(at, size);
    }
}

/// `area` (`[left, top, width, height]` in layout pixels) in the client
/// pixels of a `width` × `height` window, through the letterbox the
/// layout is drawn in ([`render::letterbox`]); at least a pixel each way.
fn client_area(
    area: [f32; 4],
    width: u32,
    height: u32,
) -> (PhysicalPosition<i32>, PhysicalSize<u32>) {
    let [x, y, w, h] = area;
    let [left, top, scaled, _] = render::letterbox(width, height);
    // The layout is 1920 pixels wide.
    let s = scaled / 1920.0;
    // Client pixels: far inside i32 and u32.
    let at = PhysicalPosition::new((left + x * s).round() as i32, (top + y * s).round() as i32);
    let size = PhysicalSize::new(
        ((w * s).round() as u32).max(1),
        ((h * s).round() as u32).max(1),
    );
    (at, size)
}

struct App {
    controls: Arc<Controls>,
    keys: Keys,
    /// The monitor to open on, by name; the primary when `None`.
    monitor: Option<String>,
    ready: Option<Sender<anyhow::Result<Surface>>>,
    window: Option<Window>,
    /// Where the select screen's input goes, with song selection.
    menu: Option<Arc<MenuQueue>>,
    ime: ImeState,
    /// The IME composition in progress and the ingress word it began
    /// under.
    composition: ImeSession,
    /// Set once the window is made.
    showing: Option<Showing>,
    shared: Arc<Shared>,
}

impl ApplicationHandler<Ask> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let Some(ready) = self.ready.take() else {
            return;
        };
        let monitor = choose_monitor(event_loop, self.monitor.as_deref());
        if let Some(m) = &monitor {
            // The console only: monitor names stay out of files.
            println!(
                "window: monitor {} at {} mHz",
                m.name().unwrap_or_default(),
                m.refresh_rate_millihertz().unwrap_or(0)
            );
        }
        let attrs = Window::default_attributes()
            .with_title("tapestry")
            .with_visible(false)
            .with_fullscreen(Some(Fullscreen::Borderless(monitor)));
        match event_loop.create_window(attrs) {
            Ok(w) => {
                w.set_cursor_visible(false);
                self.controls.focus(w.has_focus());
                let surface = surface(&w);
                let _ = ready.send(surface);
                publish(&self.shared.extent, &w);
                self.window = Some(w);
                self.showing = Some(Showing::new(Instant::now()));
                self.poll_showing(event_loop);
            }
            Err(e) => {
                let _ = ready.send(Err(e).context("the window cannot be created"));
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::Focused(focused) => {
                self.controls.focus(focused);
                #[cfg(feature = "dev-tools")]
                if !focused {
                    crate::dev::print_foreground(self.controls.now());
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                keyboard(
                    &self.controls,
                    &mut self.keys,
                    self.menu.as_deref(),
                    KeyInput {
                        key: event.physical_key,
                        pressed: event.state == ElementState::Pressed,
                        repeat: event.repeat,
                        synthetic: is_synthetic,
                        text: event.text.as_deref(),
                    },
                );
            }
            WindowEvent::Ime(ime) => {
                if let Some(menu) = &self.menu {
                    ime_input(&self.controls, &mut self.composition, menu, ime);
                }
            }
            WindowEvent::Resized(size) => {
                self.shared.extent.publish(size.width, size.height);
                if let Some(w) = &self.window {
                    self.ime.report(w);
                }
            }
            // The new size follows as a resize too; the size read here is
            // the newest either way.
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(w) = &self.window {
                    publish(&self.shared.extent, w);
                }
            }
            WindowEvent::CloseRequested => self.controls.close(),
            _ => {}
        }
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, _cause: StartCause) {
        self.poll_showing(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Ask) {
        match (event, &self.window) {
            (Ask::Ime(allowed), Some(w)) => self.ime.allow(w, allowed),
            (Ask::Caret, Some(w)) => {
                // Cleared first: a move after the read sends again.
                self.shared.caret_queued.store(false, Ordering::Release);
                let caret = *self
                    .shared
                    .caret
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                self.ime.caret(w, caret);
            }
            (Ask::Ime(_) | Ask::Caret, None) => {}
            (Ask::Exit, _) => {
                // The window goes before the loop ends, on its own thread.
                self.window = None;
                event_loop.exit();
            }
        }
    }
}

impl App {
    /// Shows the hidden window once its first frame was presented, or at
    /// its deadline; until then the loop wakes to look again.
    fn poll_showing(&mut self, event_loop: &ActiveEventLoop) {
        if let (Some(w), Some(showing)) = (&self.window, &mut self.showing) {
            let flow = match showing.poll(w, &self.controls, &self.shared, Instant::now()) {
                Some(next) => ControlFlow::WaitUntil(next),
                None => ControlFlow::Wait,
            };
            event_loop.set_control_flow(flow);
        }
    }
}

/// The running window: ends its thread when closed.
pub struct WindowThread {
    proxy: EventLoopProxy<Ask>,
    handle: JoinHandle<()>,
    shared: Arc<Shared>,
}

/// Allows or stops the window's IME from the game thread.
#[derive(Clone)]
pub struct ImeSwitch(EventLoopProxy<Ask>);

impl ImeSwitch {
    /// Allows the IME while the search box has focus, or stops it; a
    /// window already closed ignores it.
    pub fn set(&self, allowed: bool) {
        let _ = self.0.send_event(Ask::Ime(allowed));
    }
}

impl WindowThread {
    /// Closes the window and waits for its thread. Call it after every user
    /// of the window (the render thread) is done.
    pub fn close(self) {
        let _ = self.proxy.send_event(Ask::Exit);
        let _ = self.handle.join();
    }

    /// Closes the window once its renderer is gone (`rendered`), or else
    /// leaves it open to the process's exit: a render thread that did not
    /// end may still draw into it.
    pub fn close_after(self, rendered: bool) {
        if rendered {
            self.close();
        }
    }

    /// The switch of the window's IME.
    pub fn ime(&self) -> ImeSwitch {
        ImeSwitch(self.proxy.clone())
    }

    /// Where the render thread tells the window what it drew.
    pub fn link(&self) -> WindowLink {
        WindowLink {
            proxy: self.proxy.clone(),
            shared: Arc::clone(&self.shared),
        }
    }

    /// Where the game waits for the window to show.
    pub fn shown(&self) -> WindowShown {
        WindowShown(Arc::clone(&self.shared))
    }

    /// The window's client size, for the render thread to follow
    /// ([`crate::render_side::RenderLink::follow`]).
    pub fn extent(&self) -> Arc<Extent> {
        Arc::clone(&self.shared.extent)
    }
}

/// Publishes `window`'s client size in physical pixels.
fn publish(extent: &Extent, window: &Window) {
    let size = window.inner_size();
    extent.publish(size.width, size.height);
}

/// The render thread's line to the window: the caret's area of each menu
/// page drawn, the latest only, and the flag a presented frame sets; a
/// window already closed ignores them.
#[derive(Clone)]
pub struct WindowLink {
    proxy: EventLoopProxy<Ask>,
    shared: Arc<Shared>,
}

impl WindowSink for WindowLink {
    fn caret(&self, area: Option<[f32; 4]>) {
        *self
            .shared
            .caret
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = area;
        if !self.shared.caret_queued.swap(true, Ordering::AcqRel) {
            let _ = self.proxy.send_event(Ask::Caret);
        }
    }

    fn presented(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.presented)
    }
}

/// Whether the window was shown: a play's recording opens only once it
/// is, so the focus it gains when shown is the play's (a hidden window
/// has none, and would latch the play's focus loss at its opening).
#[derive(Clone)]
pub struct WindowShown(Arc<Shared>);

impl WindowShown {
    /// Waits until the window is shown before a play opens, at most
    /// [`SHOW_TIMEOUT`]: by then the window has shown itself, unless its
    /// thread ended.
    pub fn wait_to_open(&self) {
        if !self.wait(SHOW_TIMEOUT) {
            eprintln!("window: not shown within {SHOW_TIMEOUT:?}; the play opens anyway");
        }
    }

    /// Waits until the window is shown, at most `timeout`; whether it was.
    fn wait(&self, timeout: Duration) -> bool {
        let shown = self.0.shown.lock().unwrap_or_else(PoisonError::into_inner);
        let (shown, _) = self
            .0
            .shown_changed
            .wait_timeout_while(shown, timeout, |shown| !*shown)
            .unwrap_or_else(PoisonError::into_inner);
        *shown
    }
}

/// The monitor named `wanted` (`device.toml`'s `monitor`), or the primary
/// one when it is not set or no monitor has that name; the names there are
/// printed to the console so the setting can be corrected.
fn choose_monitor(event_loop: &ActiveEventLoop, wanted: Option<&str>) -> Option<MonitorHandle> {
    let Some(wanted) = wanted else {
        return event_loop.primary_monitor();
    };
    let mut monitors: Vec<MonitorHandle> = event_loop.available_monitors().collect();
    let names: Vec<Option<String>> = monitors.iter().map(MonitorHandle::name).collect();
    if let Some(i) = monitor_index(&names, wanted) {
        return Some(monitors.swap_remove(i));
    }
    let names: Vec<String> = names.into_iter().flatten().collect();
    eprintln!(
        "window: no monitor named {} (there are {}); using the primary one",
        wanted.trim(),
        names.join(", ")
    );
    event_loop.primary_monitor()
}

/// Which of the monitors named `names` is `wanted`, surrounding spaces
/// ignored.
fn monitor_index(names: &[Option<String>], wanted: &str) -> Option<usize> {
    let wanted = wanted.trim();
    names.iter().position(|n| n.as_deref() == Some(wanted))
}

/// Opens the window on its own thread. `retry` and `quit` are the keyboard
/// scan codes bound to them; `monitor` names the monitor to open on; with
/// `menu`, the select screen's input goes there.
///
/// # Errors
///
/// Why the event loop or the window could not start.
pub fn open(
    controls: Arc<Controls>,
    retry: Option<u32>,
    quit: Option<u32>,
    monitor: Option<String>,
    menu: Option<Arc<MenuQueue>>,
) -> anyhow::Result<(WindowThread, Surface)> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (proxy_tx, proxy_rx) = mpsc::channel();
    let shared = Arc::new(Shared::default());
    let thread_shared = Arc::clone(&shared);
    let handle = std::thread::Builder::new()
        .name("window".into())
        .spawn(move || {
            let event_loop = match EventLoop::<Ask>::with_user_event()
                .with_any_thread(true)
                .build()
            {
                Ok(l) => l,
                Err(e) => {
                    let _ = ready_tx.send(Err(e).context("the event loop cannot start"));
                    return;
                }
            };
            event_loop.listen_device_events(DeviceEvents::Never);
            let _ = proxy_tx.send(event_loop.create_proxy());
            let mut app = App {
                controls: Arc::clone(&controls),
                keys: Keys::new(retry, quit),
                monitor,
                ready: Some(ready_tx),
                window: None,
                menu,
                ime: ImeState::default(),
                composition: ImeSession::default(),
                showing: None,
                shared: thread_shared,
            };
            if let Err(e) = event_loop.run_app(&mut app) {
                eprintln!("the event loop failed: {e}");
            }
            // However the loop ended, the game quits.
            controls.close();
        })
        .context("the window thread cannot start")?;
    let ended = || anyhow::anyhow!("the window thread ended");
    let window = ready_rx.recv().map_err(|_| ended()).and_then(|r| r);
    // The loop sends its proxy before it makes the window.
    let proxy = proxy_rx.recv().map_err(|_| ended());
    let window = match (window, proxy) {
        (Ok(w), Ok(proxy)) => {
            let thread = WindowThread {
                proxy,
                handle,
                shared,
            };
            return Ok((thread, w));
        }
        (Err(e), Ok(proxy)) => {
            let _ = proxy.send_event(Ask::Exit);
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
    /// The refresh rate of the window's monitor, mHz, when known.
    pub refresh_mhz: Option<u32>,
}

fn surface(window: &Window) -> anyhow::Result<Surface> {
    let handle = window.window_handle().context("the window has no handle")?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        anyhow::bail!("the window is not a Win32 window");
    };
    let size = window.inner_size();
    Ok(Surface {
        hwnd: h.hwnd,
        width: size.width,
        height: size.height,
        refresh_mhz: window
            .current_monitor()
            .and_then(|m| m.refresh_rate_millihertz()),
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

/// The window's key for a keyboard control as Raw Input names it: through
/// the scan code the window's messages carry for it
/// ([`input::raw::window_scan_code`]), so a key bound by `tapestry keys` is
/// the same key here, aliases included (winit names several scan codes as
/// one key).
fn window_key(control: u32) -> PhysicalKey {
    PhysicalKey::from_scancode(input::raw::window_scan_code(ControlId(control)))
}

/// The keys the window acts on, as winit names them.
struct Keys {
    retry: Option<PhysicalKey>,
    quit: Option<PhysicalKey>,
    settings: [(PhysicalKey, Adjust); 6],
    overlay: PhysicalKey,
    /// F1 and F2.
    speed: [PhysicalKey; 2],
    /// The scene epoch each settings key's press was taken in, while it is
    /// held: its repeats act only while that epoch is current.
    pressed_in: [Option<u64>; 6],
    /// The last key pressed for the menus and the scene epoch it was
    /// pressed in: only it repeats, while that epoch is current.
    menu_press: Option<(PhysicalKey, u64)>,
}

impl Keys {
    /// The keys for the retry and quit controls and the settings keys.
    fn new(retry: Option<u32>, quit: Option<u32>) -> Self {
        Self {
            retry: retry.map(window_key),
            quit: quit.map(window_key),
            settings: Adjust::ALL.map(|a| (window_key(a.key()), a)),
            overlay: window_key(game::OVERLAY_KEY),
            speed: game::SPEED_KEYS.map(window_key),
            pressed_in: [None; 6],
            menu_press: None,
        }
    }

    /// F1 (0) or F2 (1), if `key` is one.
    fn speed_key(&self, key: PhysicalKey) -> Option<u8> {
        self.speed
            .iter()
            .position(|k| *k == key)
            .and_then(|i| u8::try_from(i).ok())
    }
}

/// One keyboard event as the window received it.
#[derive(Clone, Copy, Debug)]
struct KeyInput<'a> {
    key: PhysicalKey,
    pressed: bool,
    repeat: bool,
    /// Made up by the window system for a focus change.
    synthetic: bool,
    /// The text it typed, if any.
    text: Option<&'a str>,
}

/// One keyboard event: F1 and F2 to the tap's window queue (autoplay),
/// stamped as observed; a release to tuning; a real press to tuning and
/// the system controls, then to the menus as a key and its printable text,
/// stamped with the ingress word as observed (design.md "Play Lifecycle",
/// the decision screen). While the word is closed nothing goes to the
/// menus, but Escape once the decision screen lets it through; a repeat
/// goes only while the scene of its key's press is current, so a key held
/// across a scene change needs a fresh press; a made-up press goes nowhere
/// but the held state.
fn keyboard(controls: &Controls, keys: &mut Keys, menu: Option<&MenuQueue>, e: KeyInput<'_>) {
    // F1 and F2 reach the adjustments only through the tap, or in an
    // autoplay session through the window's queue; a made-up event only
    // updates what the window holds.
    if let Some(key) = keys.speed_key(e.key) {
        let event = match (e.pressed, e.repeat) {
            (true, false) => WindowKeyEvent::Press,
            (true, true) => WindowKeyEvent::Repeat,
            (false, _) => WindowKeyEvent::Release,
        };
        controls.window_key(key, event, e.synthetic);
    }
    if !e.pressed {
        if !e.synthetic {
            key_released(controls, e.key, keys);
        }
        return;
    }
    if e.synthetic {
        return;
    }
    key_pressed(controls, e.key, e.repeat, keys);
    let scene = controls.scene();
    if !e.repeat {
        keys.menu_press = Some((e.key, scene));
    } else if keys.menu_press != Some((e.key, scene)) {
        return;
    }
    let Some(menu) = menu else {
        return;
    };
    let key = menu_key(e.key, e.repeat).map(MenuInput::Key);
    // Printable text only: control characters come as keys.
    let text = e
        .text
        .filter(|t| !t.chars().any(char::is_control))
        .map(|t| MenuInput::Text(t.to_owned()));
    let stamp = controls.ingress();
    for input in key.into_iter().chain(text) {
        let escape = matches!(input, MenuInput::Key(MenuKey::Escape)) && controls.escaping();
        if stamp.is_open() || escape {
            send_menu(menu, controls, input, Some(stamp));
        }
    }
}

/// An IME event through the window's `composition`, its input stamped with
/// the word its composition began under.
fn ime_input(controls: &Controls, composition: &mut ImeSession, menu: &MenuQueue, ime: Ime) {
    if let Some((input, stamp)) = composition.event(ime, controls.ingress()) {
        send_menu(menu, controls, input, Some(stamp));
    }
}

/// A key pressed in the window. A settings key goes to tuning, which takes
/// it by its hold class ([`crate::tuning::LiveTuning::key`]); its repeats
/// go only while the scene its press was taken in is current (the
/// held-input guard). The overlay key, retry and quit act on the first
/// press only.
fn key_pressed(controls: &Controls, key: PhysicalKey, repeat: bool, keys: &mut Keys) {
    if let Some(i) = keys.settings.iter().position(|(k, _)| *k == key) {
        let scene = controls.scene();
        let Some((Some(pressed_in), &(_, a))) = keys
            .pressed_in
            .get_mut(i)
            .zip(keys.settings.get(i))
            .map(|(p, s)| {
                if !repeat {
                    *p = Some(scene);
                }
                (*p, s)
            })
        else {
            return;
        };
        if pressed_in != scene {
            return;
        }
        let event = if repeat {
            KeyEvent::Repeat
        } else {
            KeyEvent::Press
        };
        controls.settings_key(a, event);
    } else if repeat {
        // The others take the first press only.
    } else if key == keys.overlay {
        // Discarded in the decision screen's epoch, as every key is.
        controls.overlay_key();
    } else if Some(key) == keys.retry {
        controls.command(Command::Retry);
    } else if Some(key) == keys.quit {
        controls.command(Command::Quit);
    }
}

/// A key released in the window: a settings key's release goes to tuning.
fn key_released(controls: &Controls, key: PhysicalKey, keys: &Keys) {
    if let Some(&(_, a)) = keys.settings.iter().find(|(k, _)| *k == key) {
        controls.settings_key(a, KeyEvent::Release);
    }
}

/// Hands `input`, observed under the ingress word `stamp`, to the select
/// screen and wakes the game thread.
fn send_menu(
    menu: &MenuQueue,
    controls: &Controls,
    input: MenuInput,
    stamp: Option<input::Ingress>,
) {
    menu.push_stamped(input, stamp);
    controls.wake_game();
}

/// An IME composition and the ingress word it began under (design.md
/// "Play Lifecycle", the decision screen): its preedits and its commit
/// carry that stamp, so a composition open at the choice, or begun in the
/// decision screen, commits nothing after it. The window decides nothing;
/// the stamp is validated where the input is taken.
#[derive(Debug, Default)]
struct ImeSession {
    /// The composition's stamp, and whether it was cleared (an empty
    /// preedit, which comes before its commit too).
    current: Option<(input::Ingress, bool)>,
}

impl ImeSession {
    /// The input for `ime` and its stamp, `now` the ingress word as the
    /// window observes it.
    fn event(&mut self, ime: Ime, now: input::Ingress) -> Option<(MenuInput, input::Ingress)> {
        match ime {
            Ime::Preedit(text, _) if !text.is_empty() => {
                let stamp = match self.current {
                    Some((stamp, false)) => stamp,
                    _ => now,
                };
                self.current = Some((stamp, false));
                Some((MenuInput::Preedit(text), stamp))
            }
            Ime::Preedit(_, _) => {
                let stamp = self.current.map_or(now, |(s, _)| s);
                self.current = Some((stamp, true));
                Some((MenuInput::Preedit(String::new()), stamp))
            }
            Ime::Commit(text) => {
                let stamp = self.current.take().map_or(now, |(s, _)| s);
                Some((MenuInput::Text(text), stamp))
            }
            Ime::Disabled => {
                let stamp = self.current.take().map_or(now, |(s, _)| s);
                Some((MenuInput::Preedit(String::new()), stamp))
            }
            Ime::Enabled => None,
        }
    }
}

/// The select screen's key for a key pressed in the window: navigation
/// repeats while held, the others act on the first press only.
fn menu_key(key: PhysicalKey, repeat: bool) -> Option<MenuKey> {
    let PhysicalKey::Code(code) = key else {
        return None;
    };
    let (key, repeats) = match code {
        KeyCode::ArrowUp => (MenuKey::Up, true),
        KeyCode::ArrowDown => (MenuKey::Down, true),
        KeyCode::PageUp => (MenuKey::PageUp, true),
        KeyCode::PageDown => (MenuKey::PageDown, true),
        KeyCode::Backspace if repeat => (MenuKey::BackspaceHeld, true),
        KeyCode::Backspace => (MenuKey::Backspace, false),
        KeyCode::ArrowLeft => (MenuKey::Left, false),
        KeyCode::ArrowRight => (MenuKey::Right, false),
        KeyCode::Home => (MenuKey::Home, false),
        KeyCode::End => (MenuKey::End, false),
        KeyCode::Enter | KeyCode::NumpadEnter => (MenuKey::Enter, false),
        KeyCode::Tab => (MenuKey::Tab, false),
        KeyCode::Escape => (MenuKey::Escape, false),
        KeyCode::KeyP => (MenuKey::Practice, false),
        KeyCode::F11 => (MenuKey::PrevPreset, false),
        KeyCode::F12 => (MenuKey::NextPreset, false),
        _ => return None,
    };
    (repeats || !repeat).then_some(key)
}

#[cfg(test)]
mod tests {
    /// The window as the IME handler sees it: its size, and what it was
    /// asked.
    struct FakeWindow {
        size: std::cell::Cell<(u32, u32)>,
        asked: std::cell::RefCell<Vec<String>>,
    }

    impl FakeWindow {
        fn new(width: u32, height: u32) -> Self {
            Self {
                size: std::cell::Cell::new((width, height)),
                asked: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn take(&self) -> Vec<String> {
            self.asked.take()
        }
    }

    impl super::ImeWindow for FakeWindow {
        fn client_size(&self) -> (u32, u32) {
            self.size.get()
        }

        fn allow_ime(&self, allowed: bool) {
            self.asked.borrow_mut().push(format!("allow {allowed}"));
        }

        fn ime_area(
            &self,
            at: winit::dpi::PhysicalPosition<i32>,
            size: winit::dpi::PhysicalSize<u32>,
        ) {
            self.asked.borrow_mut().push(format!(
                "area {} {} {} {}",
                at.x, at.y, size.width, size.height
            ));
        }
    }

    /// The hidden window as showing sees it.
    #[derive(Default)]
    struct FakeShow {
        shows: std::cell::Cell<u32>,
        focused: bool,
    }

    impl super::ShowWindow for FakeShow {
        fn show(&self) {
            self.shows.set(self.shows.get() + 1);
        }

        fn has_focus(&self) -> bool {
            self.focused && self.shows.get() > 0
        }
    }

    /// The window side once a frame was presented: the hidden window shows
    /// at its next look, once, its focus reaches the controls, and a play
    /// waiting to open goes on.
    #[test]
    fn a_presented_frame_shows_the_window_and_lets_the_play_open() {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let c = Controls::new(QpcClock::start());
        let w = FakeShow {
            focused: true,
            ..FakeShow::default()
        };
        let shared = std::sync::Arc::new(super::Shared::default());
        let shown = super::WindowShown(std::sync::Arc::clone(&shared));
        let start = Instant::now();
        let mut showing = super::Showing::new(start);
        // Made hidden, the window had no focus.
        c.focus(false);
        // Nothing presented: it looks again soon, still hidden.
        assert_eq!(
            showing.poll(&w, &c, &shared, start),
            Some(start + super::SHOW_POLL)
        );
        assert_eq!(w.shows.get(), 0);
        assert!(!shown.wait(Duration::ZERO));
        shared.presented.store(true, Ordering::Release);
        assert_eq!(showing.poll(&w, &c, &shared, start), None);
        assert_eq!(w.shows.get(), 1);
        assert!(c.focused());
        assert!(shown.wait(Duration::ZERO));
        assert_eq!(showing.poll(&w, &c, &shared, start), None);
        assert_eq!(w.shows.get(), 1);
    }

    /// A play that asks to open before the window shows waits for it: the
    /// window thread's show, made later, ends the wait.
    #[test]
    fn a_play_opening_before_the_show_waits_for_it() {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let shared = std::sync::Arc::new(super::Shared::default());
        let shown = super::WindowShown(std::sync::Arc::clone(&shared));
        let window_side = std::sync::Arc::clone(&shared);
        let opener = std::thread::spawn(move || shown.wait_to_open());
        std::thread::sleep(Duration::from_millis(100));
        // Still waiting: nothing showed the window.
        assert!(!opener.is_finished());
        let c = Controls::new(QpcClock::start());
        let mut showing = super::Showing::new(Instant::now());
        window_side.presented.store(true, Ordering::Release);
        let showed = Instant::now();
        assert_eq!(
            showing.poll(&FakeShow::default(), &c, &window_side, showed),
            None
        );
        opener.join().unwrap();
        // Woken by the show, not by the wait's bound.
        assert!(
            showed.elapsed() < Duration::from_secs(1),
            "{:?}",
            showed.elapsed()
        );
    }

    /// Without a frame the window is shown at its deadline, and not before;
    /// a play never waits past the deadline either.
    #[test]
    fn a_window_without_a_frame_is_shown_at_its_deadline() {
        use std::time::{Duration, Instant};
        let c = Controls::new(QpcClock::start());
        let w = FakeShow::default();
        let shared = super::Shared::default();
        let start = Instant::now();
        let mut showing = super::Showing::new(start);
        let late = start + super::SHOW_TIMEOUT - Duration::from_millis(1);
        assert_eq!(
            showing.poll(&w, &c, &shared, late),
            Some(start + super::SHOW_TIMEOUT)
        );
        assert_eq!(w.shows.get(), 0);
        assert_eq!(
            showing.poll(&w, &c, &shared, start + super::SHOW_TIMEOUT),
            None
        );
        assert_eq!(w.shows.get(), 1);
        assert!(!c.focused());
        // A window thread that never shows: the wait ends at its bound.
        let never = super::WindowShown(std::sync::Arc::new(super::Shared::default()));
        let asked = Instant::now();
        assert!(!never.wait(Duration::from_millis(50)));
        assert!(asked.elapsed() >= Duration::from_millis(50));
    }

    /// A layout area lands on the window's pixels through the letterbox:
    /// scaled with the layout, shifted past the bars, at least a pixel.
    #[test]
    fn the_caret_area_maps_through_the_letterbox() {
        let area = [300.0, 72.0, 60.0, 42.0];
        let mapped = |w, h| {
            let (at, size) = super::client_area(area, w, h);
            (at.x, at.y, size.width, size.height)
        };
        assert_eq!(mapped(1920, 1080), (300, 72, 60, 42));
        assert_eq!(mapped(3840, 2160), (600, 144, 120, 84));
        assert_eq!(mapped(2560, 1440), (400, 96, 80, 56));
        // Bars left and right (21:9): 320 pixels each side.
        assert_eq!(mapped(2560, 1080), (620, 72, 60, 42));
        // Bars above and below (5:4): scaled by 2/3, 152 pixels down.
        assert_eq!(mapped(1280, 1024), (200, 200, 40, 28));
        // A sliver of an area is still a pixel.
        let (_, size) = super::client_area([0.0, 0.0, 0.1, 0.1], 960, 540);
        assert_eq!((size.width, size.height), (1, 1));
    }

    /// The search box's focus allows the IME and reports where its caret
    /// is; a caret move and a resize report it again, and losing focus
    /// stops the IME with nothing more reported.
    #[test]
    fn focus_reports_the_caret_to_the_ime_and_leaving_stops_it() {
        let w = FakeWindow::new(1920, 1080);
        let mut ime = super::ImeState::default();
        // The page is drawn before the focus reaches the window: nothing to
        // report to yet.
        ime.caret(&w, Some([200.0, 72.0, 1.0, 42.0]));
        assert!(w.take().is_empty());
        ime.allow(&w, true);
        assert_eq!(w.take(), ["allow true", "area 200 72 1 42"]);
        ime.caret(&w, Some([230.0, 72.0, 60.0, 42.0]));
        assert_eq!(w.take(), ["area 230 72 60 42"]);
        w.size.set((3840, 2160));
        ime.report(&w);
        assert_eq!(w.take(), ["area 460 144 120 84"]);
        ime.allow(&w, false);
        assert_eq!(w.take(), ["allow false"]);
        ime.caret(&w, None);
        ime.report(&w);
        assert!(w.take().is_empty());
        // Focus again before the next page: that page's caret is reported.
        ime.allow(&w, true);
        assert_eq!(w.take(), ["allow true"]);
        ime.caret(&w, Some([200.0, 72.0, 1.0, 42.0]));
        assert_eq!(w.take(), ["area 400 144 2 84"]);
    }
    use game::{Adjust, Command, DeviceSettings};
    use input::{DeviceKind, QpcClock};
    use winit::keyboard::{KeyCode, PhysicalKey};
    use winit::platform::scancode::PhysicalKeyExtScancode;

    use super::{Keys, key_pressed, window_key};
    use crate::controls::Controls;
    use crate::tuning::HoldClass::{FocusDisarmed, Fresh, GapDiscarded, PreLoading};
    use crate::tuning::KeyEvent;

    /// Raw Input's controls for Pause and NumLock (`input::raw::key_control`).
    const PAUSE: u32 = 0xE11D;
    const NUM_LOCK: u32 = 0x45;

    /// P is the practice key, on its first press only.
    #[test]
    fn p_is_the_practice_key_on_its_first_press() {
        use crate::select::MenuKey;
        let p = PhysicalKey::Code(KeyCode::KeyP);
        assert_eq!(super::menu_key(p, false), Some(MenuKey::Practice));
        assert_eq!(super::menu_key(p, true), None);
    }

    /// F11 and F12 step the play preset, on their first press only; they
    /// are the keys the game reserves for it, so no binding takes them.
    #[test]
    fn f11_and_f12_step_the_preset_on_their_first_press() {
        use crate::select::MenuKey;
        for (code, key, raw) in [
            (KeyCode::F11, MenuKey::PrevPreset, game::PREV_PRESET_KEY),
            (KeyCode::F12, MenuKey::NextPreset, game::NEXT_PRESET_KEY),
        ] {
            let pressed = PhysicalKey::Code(code);
            assert_eq!(super::menu_key(pressed, false), Some(key));
            assert_eq!(super::menu_key(pressed, true), None);
            assert_eq!(window_key(raw), pressed);
            assert!(game::reserved_key(raw).is_some());
        }
    }

    /// Backspace is one press and then its repeats, which only the search box
    /// reads: a held key does not climb the folders.
    #[test]
    fn a_held_backspace_is_its_own_key() {
        use crate::select::MenuKey;
        let backspace = PhysicalKey::Code(KeyCode::Backspace);
        assert_eq!(super::menu_key(backspace, false), Some(MenuKey::Backspace));
        assert_eq!(
            super::menu_key(backspace, true),
            Some(MenuKey::BackspaceHeld)
        );
    }

    /// Pause bound to retry and NumLock to quit, as `tapestry keys`
    /// captures them: the key config is valid, and in the window Pause
    /// retries and NumLock quits, never the other way round.
    #[test]
    fn pause_and_num_lock_mean_the_same_keys_in_the_window() {
        let mut config = DeviceSettings::default().keyboard;
        config.retry = Some(PAUSE);
        config.quit = Some(NUM_LOCK);
        assert_eq!(config.check(DeviceKind::Keyboard), Ok(()));
        let mut keys = Keys::new(config.retry, config.quit);

        let c = Controls::new(QpcClock::start());
        let pause = PhysicalKey::Code(KeyCode::Pause);
        key_pressed(&c, pause, false, &mut keys);
        assert_eq!(c.take(), Some(Command::Retry));
        assert!(!c.quitting());
        // A held Pause repeats nothing.
        key_pressed(&c, pause, true, &mut keys);
        assert_eq!(c.take(), None);
        key_pressed(&c, PhysicalKey::Code(KeyCode::NumLock), false, &mut keys);
        assert_eq!(c.take(), None);
        assert!(c.quitting());
    }

    /// Every key Raw Input can name (set 1 make codes, with `E0`, and
    /// Pause), bound to retry, retries when the window reports the key
    /// winit makes of the scan code its messages carry for it; and the
    /// settings keys are F1 to F8 in the window too.
    #[test]
    fn every_raw_input_key_bound_to_retry_retries_in_the_window() {
        let raw = (0x01..=0xFE).chain(0xE001..=0xE0FE).chain([PAUSE]);
        let mut known = 0;
        for control in raw {
            // What the window's message carries for that key: winit then
            // names it from this scan code.
            let carried = match control {
                PAUSE => 0x45,
                NUM_LOCK => 0xE045,
                c => c,
            };
            let pressed = PhysicalKey::from_scancode(carried);
            if matches!(pressed, PhysicalKey::Code(_)) {
                known += 1;
            }
            let c = Controls::new(QpcClock::start());
            key_pressed(&c, pressed, false, &mut Keys::new(Some(control), None));
            // A settings key's scan code steps the setting instead, and
            // the overlay key's toggles the overlay.
            if Adjust::from_key(control).is_none() && control != game::OVERLAY_KEY {
                assert_eq!(c.take(), Some(Command::Retry), "{control:#x}");
            }
        }
        assert!(known > 100, "{known} keys known to winit");
        assert_eq!(window_key(PAUSE), PhysicalKey::Code(KeyCode::Pause));
        assert_eq!(window_key(NUM_LOCK), PhysicalKey::Code(KeyCode::NumLock));
        let f = [
            KeyCode::F3,
            KeyCode::F4,
            KeyCode::F5,
            KeyCode::F6,
            KeyCode::F7,
            KeyCode::F8,
        ];
        for (a, k) in Adjust::ALL.into_iter().zip(f) {
            assert_eq!(window_key(a.key()), PhysicalKey::Code(k));
            assert_eq!(PhysicalKey::Code(k).to_scancode(), Some(a.key()));
        }
        // F1 and F2 (hi-speed) step nothing in the window.
        let c = Controls::new(QpcClock::start());
        let before = c.tuning().get();
        for k in [KeyCode::F1, KeyCode::F2] {
            key_pressed(&c, PhysicalKey::Code(k), true, &mut Keys::new(None, None));
        }
        assert_eq!(c.tuning().get(), before);
        assert_eq!(c.take(), None);
    }

    /// The held-input guard: a settings key repeats while the scene its
    /// press was taken in is current; once the scene changes its repeats
    /// step nothing until it is pressed afresh, and a repeat without a
    /// press seen steps nothing.
    #[test]
    fn a_settings_key_held_across_a_scene_change_repeats_nothing() {
        let c = Controls::new(QpcClock::start());
        let mut keys = Keys::new(None, None);
        let f6 = PhysicalKey::Code(KeyCode::F6);
        let offset = |c: &Controls| c.tuning().get().display_offset_us;
        key_pressed(&c, f6, true, &mut keys);
        assert_eq!(offset(&c), 0, "no press seen");
        key_pressed(&c, f6, false, &mut keys);
        key_pressed(&c, f6, true, &mut keys);
        assert_eq!(offset(&c), 2_000);
        c.next_scene();
        key_pressed(&c, f6, true, &mut keys);
        assert_eq!(offset(&c), 2_000, "held across the scene change");
        key_pressed(&c, f6, false, &mut keys);
        assert_eq!(offset(&c), 3_000, "pressed afresh");
    }

    /// Controls on the clock, the settings keys starting from 100 %
    /// volume and no offsets, with the loading state begun now.
    fn loading_controls() -> Controls {
        use bms_model::TimeSource as _;
        let clock = QpcClock::start();
        let tuning = crate::tuning::LiveTuning::new(game::Tuning {
            volume_percent: 100,
            display_offset_us: 0,
            input_offset_us: 0,
        });
        let c = Controls::with_tuning(clock, std::sync::Arc::new(tuning));
        c.tuning().enter_loading(clock.now());
        c
    }

    /// Settings key `a` pressed (or repeated) in the window.
    fn press(c: &Controls, keys: &mut Keys, a: Adjust, repeat: bool) {
        key_pressed(c, window_key(a.key()), repeat, keys);
    }

    /// Settings key `a` released in the window.
    fn release(c: &Controls, keys: &Keys, a: Adjust) {
        super::key_released(c, window_key(a.key()), keys);
    }

    /// The window side's last activity, µs.
    fn last(c: &Controls) -> i64 {
        c.tuning().snapshot().last_activity.unwrap().0
    }

    /// A1: a settings key pressed in the window while loading, focused and
    /// open, is a fresh hold: it steps its value, holds the loading state,
    /// and moves the revision and the last activity.
    #[test]
    fn an_accepted_press_holds_counts_and_adjusts() {
        let c = loading_controls();
        let mut keys = Keys::new(None, None);
        let before = c.tuning().snapshot();
        assert!(before.quiet_at(bms_model::TimeUs(0)));
        press(&c, &mut keys, Adjust::DisplayOffsetUp, false);
        let after = c.tuning().snapshot();
        assert_eq!(c.tuning().held(), [(Adjust::DisplayOffsetUp, Fresh)]);
        assert!(after.held && !after.overflow && after.focused);
        assert_eq!(after.revision.window, before.revision.window + 1);
        assert_eq!(after.values.unwrap().display_offset_us, 1_000);
        assert!(after.last_activity.is_some());
        assert!(!after.quiet_at(bms_model::TimeUs(i64::MAX)), "held");
    }

    /// A2: a fresh hold's repeats at the value's limit step nothing but
    /// count, each moving the last activity, so the quiet wait restarts
    /// from the latest; it runs from the release once released.
    #[test]
    fn held_at_the_limit_and_repeat_restart_the_quiet_wait() {
        let c = loading_controls();
        let mut keys = Keys::new(None, None);
        press(&c, &mut keys, Adjust::VolumeUp, false);
        let pressed = c.tuning().snapshot();
        assert_eq!(pressed.values.unwrap().volume_percent, 100, "at the limit");
        assert_eq!(c.tuning().held(), [(Adjust::VolumeUp, Fresh)]);
        let first = last(&c);
        press(&c, &mut keys, Adjust::VolumeUp, true);
        let repeated = c.tuning().snapshot();
        assert_eq!(repeated.revision.window, pressed.revision.window + 1);
        assert_eq!(repeated.values, pressed.values);
        let second = last(&c);
        assert!(second >= first);
        assert_eq!(c.tuning().held(), [(Adjust::VolumeUp, Fresh)]);
        assert!(
            !repeated.quiet_at(bms_model::TimeUs(second + game::QUIET.0)),
            "held"
        );
        release(&c, &keys, Adjust::VolumeUp);
        let released = c.tuning().snapshot();
        let third = last(&c);
        assert!(third >= second);
        assert_eq!(released.revision.window, repeated.revision.window + 1);
        assert!(c.tuning().held().is_empty());
        assert!(!released.quiet_at(bms_model::TimeUs(third + game::QUIET.0 - 1)));
        assert!(released.quiet_at(bms_model::TimeUs(third + game::QUIET.0)));
    }

    /// A3: the loading state's tuning is quiet one second after the last
    /// release, not a microsecond earlier.
    #[test]
    fn ready_one_second_after_the_last_release() {
        let c = loading_controls();
        let mut keys = Keys::new(None, None);
        press(&c, &mut keys, Adjust::VolumeDown, false);
        assert_eq!(c.tuning().held(), [(Adjust::VolumeDown, Fresh)]);
        release(&c, &keys, Adjust::VolumeDown);
        assert!(c.tuning().held().is_empty());
        let s = c.tuning().snapshot();
        let at = last(&c);
        assert!(!s.held);
        assert_eq!(s.values.unwrap().volume_percent, 95);
        assert!(!s.quiet_at(bms_model::TimeUs(at + game::QUIET.0 - 1)));
        assert!(s.quiet_at(bms_model::TimeUs(at + game::QUIET.0)));
    }

    /// A4: a settings key down when the loading state begins holds it
    /// until released and steps nothing meanwhile, its repeats
    /// neither; its release counts, and the next press is fresh.
    #[test]
    fn a_key_held_from_before_loading_blocks_until_released_and_never_adjusts() {
        use bms_model::TimeSource as _;
        let clock = QpcClock::start();
        let c = Controls::with_tuning(
            clock,
            std::sync::Arc::new(crate::tuning::LiveTuning::new(game::Tuning {
                volume_percent: 100,
                display_offset_us: 0,
                input_offset_us: 0,
            })),
        );
        let mut keys = Keys::new(None, None);
        // Pressed on the select screen: it steps there.
        press(&c, &mut keys, Adjust::DisplayOffsetUp, false);
        assert_eq!(c.tuning().get().display_offset_us, 1_000);
        // The loading state begins, as `entered_loading` begins it.
        c.next_scene();
        c.tuning().enter_loading(clock.now());
        assert_eq!(c.tuning().held(), [(Adjust::DisplayOffsetUp, PreLoading)]);
        let began = c.tuning().snapshot();
        assert!(began.held);
        // Its repeats, past the held-input guard or not, step nothing and
        // count nothing.
        press(&c, &mut keys, Adjust::DisplayOffsetUp, true);
        c.settings_key(Adjust::DisplayOffsetUp, KeyEvent::Repeat);
        assert_eq!(c.tuning().snapshot(), began);
        assert_eq!(c.tuning().held(), [(Adjust::DisplayOffsetUp, PreLoading)]);
        // Its release counts, the quiet wait running from it.
        release(&c, &keys, Adjust::DisplayOffsetUp);
        let released = c.tuning().snapshot();
        assert!(c.tuning().held().is_empty());
        assert!(!released.held);
        assert_eq!(released.revision.window, began.revision.window + 1);
        assert_eq!(released.values, began.values);
        let at = last(&c);
        assert!(at >= began.last_activity.unwrap().0);
        assert!(!released.quiet_at(bms_model::TimeUs(at + game::QUIET.0 - 1)));
        // The next press is fresh.
        press(&c, &mut keys, Adjust::DisplayOffsetUp, false);
        assert_eq!(c.tuning().held(), [(Adjust::DisplayOffsetUp, Fresh)]);
        assert_eq!(c.tuning().get().display_offset_us, 2_000);
    }

    /// A5, through the window's routing: focus lost while a fresh key is
    /// down empties the held set, the key disarmed, and counts; focus back
    /// counts and the quiet wait runs from it; the key's stale repeat does
    /// nothing; a fresh press of it steps and holds again.
    #[test]
    fn focus_loss_then_return_follows_the_disarmed_rule() {
        let c = loading_controls();
        let mut keys = Keys::new(None, None);
        press(&c, &mut keys, Adjust::DisplayOffsetUp, false);
        assert_eq!(c.tuning().held(), [(Adjust::DisplayOffsetUp, Fresh)]);
        let held = c.tuning().snapshot();
        c.focus(false);
        assert_eq!(
            c.tuning().held(),
            [(Adjust::DisplayOffsetUp, FocusDisarmed)]
        );
        let lost = c.tuning().snapshot();
        assert!(!lost.held && !lost.focused);
        assert_eq!(lost.revision.window, held.revision.window + 1);
        assert!(c.tuning().focus_lost_since_reopen());
        c.focus(true);
        assert_eq!(
            c.tuning().held(),
            [(Adjust::DisplayOffsetUp, FocusDisarmed)]
        );
        let back = c.tuning().snapshot();
        assert!(back.focused && !back.held);
        assert_eq!(back.revision.window, lost.revision.window + 1);
        let at = last(&c);
        assert!(at >= held.last_activity.unwrap().0);
        assert!(!back.quiet_at(bms_model::TimeUs(at + game::QUIET.0 - 1)));
        assert!(back.quiet_at(bms_model::TimeUs(at + game::QUIET.0)));
        // The stale repeat: nothing, past the guard or not.
        press(&c, &mut keys, Adjust::DisplayOffsetUp, true);
        c.settings_key(Adjust::DisplayOffsetUp, KeyEvent::Repeat);
        assert_eq!(c.tuning().snapshot(), back);
        assert_eq!(
            c.tuning().held(),
            [(Adjust::DisplayOffsetUp, FocusDisarmed)]
        );
        // A fresh press: A1.
        press(&c, &mut keys, Adjust::DisplayOffsetUp, false);
        assert_eq!(c.tuning().held(), [(Adjust::DisplayOffsetUp, Fresh)]);
        let fresh = c.tuning().snapshot();
        assert!(fresh.held);
        assert_eq!(fresh.revision.window, back.revision.window + 1);
        assert_eq!(fresh.values.unwrap().display_offset_us, 2_000);
    }

    /// A13, the window's half: while acceptance is closed a settings key's
    /// press is discarded (stepping, holding and counting nothing), and so
    /// are its repeats and release; once reopened, its next press steps.
    #[test]
    fn a_press_in_the_closed_gap_is_discarded_and_rearms() {
        let c = loading_controls();
        let mut keys = Keys::new(None, None);
        let open = c.tuning().snapshot();
        let closed = c.tuning().close(4);
        assert_eq!(closed, open);
        press(&c, &mut keys, Adjust::DisplayOffsetUp, false);
        assert_eq!(c.tuning().held(), [(Adjust::DisplayOffsetUp, GapDiscarded)]);
        assert_eq!(c.tuning().snapshot(), open);
        press(&c, &mut keys, Adjust::DisplayOffsetUp, true);
        assert_eq!(c.tuning().snapshot(), open);
        release(&c, &keys, Adjust::DisplayOffsetUp);
        assert!(c.tuning().held().is_empty());
        assert_eq!(c.tuning().snapshot(), open);
        // Down across the reopening: still discarded.
        press(&c, &mut keys, Adjust::DisplayOffsetUp, false);
        c.tuning().reopen();
        press(&c, &mut keys, Adjust::DisplayOffsetUp, true);
        assert_eq!(c.tuning().snapshot(), open);
        release(&c, &keys, Adjust::DisplayOffsetUp);
        // Rearmed: the next press steps.
        press(&c, &mut keys, Adjust::DisplayOffsetUp, false);
        assert_eq!(c.tuning().held(), [(Adjust::DisplayOffsetUp, Fresh)]);
        assert_eq!(c.tuning().get().display_offset_us, 1_000);
        assert_eq!(
            c.tuning().snapshot().revision.window,
            open.revision.window + 1
        );
    }

    /// The window's routing of the settings keys into tuning, presses,
    /// repeats, releases and focus changes, allocates nothing.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_held_set_routes_without_allocating_alloc() {
        let c = loading_controls();
        let mut keys = Keys::new(None, None);
        let round = |c: &Controls, keys: &mut Keys| {
            for a in Adjust::ALL {
                press(c, keys, a, false);
                press(c, keys, a, true);
            }
            c.focus(false);
            c.focus(true);
            for a in Adjust::ALL {
                press(c, keys, a, false);
                release(c, keys, a);
            }
            let s = c.tuning().close(1);
            press(c, keys, Adjust::VolumeDown, false);
            release(c, keys, Adjust::VolumeDown);
            c.tuning().reopen();
            let _ = c.tuning().close_unchanged(&s, 2, bms_model::TimeUs(0));
            c.tuning().reopen();
        };
        // Warmed up once: the routing's first call.
        round(&c, &mut keys);
        let before = crate::alloc_count::this_thread();
        round(&c, &mut keys);
        assert_eq!(crate::alloc_count::this_thread() - before, 0);
    }

    /// A volume key stores only the volume; an offset key only its offset.
    #[test]
    fn a_volume_step_stores_only_the_volume() {
        let c = Controls::new(QpcClock::start());
        let before = c.tuning().get();
        assert!(c.tuning().adjust(Adjust::VolumeDown));
        let after = c.tuning().get();
        assert_eq!(after.display_offset_us, before.display_offset_us);
        assert_eq!(after.input_offset_us, before.input_offset_us);
        assert_ne!(after.volume_percent, before.volume_percent);
    }

    /// `device.toml`'s monitor picks a monitor by its exact device name,
    /// and nothing for a name no monitor has (the primary one is used).
    #[test]
    fn the_monitor_setting_picks_by_device_name() {
        let names = [
            Some(r"\.\DISPLAY2".to_owned()),
            None,
            Some(r"\.\DISPLAY1".to_owned()),
        ];
        assert_eq!(super::monitor_index(&names, r"\.\DISPLAY1"), Some(2));
        assert_eq!(super::monitor_index(&names, r" \.\DISPLAY2 "), Some(0));
        assert_eq!(super::monitor_index(&names, r"\.\DISPLAY10"), None);
        assert_eq!(super::monitor_index(&names, "DISPLAY1"), None);
    }

    /// F9 shows the measurement overlay and hides it again, on first
    /// presses only, whatever retry and quit are bound to.
    #[test]
    fn the_overlay_key_toggles_the_overlay() {
        use std::sync::atomic::Ordering;
        let c = Controls::new(QpcClock::start());
        let mut keys = Keys::new(Some(0x0E), Some(0x01));
        let f9 = PhysicalKey::Code(KeyCode::F9);
        assert_eq!(window_key(game::OVERLAY_KEY), f9);
        assert!(!c.overlay().load(Ordering::Acquire));
        key_pressed(&c, f9, false, &mut keys);
        assert!(c.overlay().load(Ordering::Acquire));
        key_pressed(&c, f9, true, &mut keys);
        assert!(c.overlay().load(Ordering::Acquire));
        key_pressed(&c, f9, false, &mut keys);
        assert!(!c.overlay().load(Ordering::Acquire));
        assert_eq!(c.take(), None);
        assert!(!c.quitting());
    }

    /// Two Raw Input controls are one key in the window exactly when they
    /// have one identity, the identity key configs are checked on; five
    /// keys have two names.
    #[test]
    fn the_window_tells_keys_apart_as_key_configs_do() {
        use eval::ControlId;
        use input::raw::key_identity;
        let raw: Vec<u32> = (0x01..=0xFE)
            .chain(0xE001..=0xE0FE)
            .chain([PAUSE])
            .collect();
        let keys: Vec<PhysicalKey> = raw.iter().map(|&c| window_key(c)).collect();
        let ids: Vec<ControlId> = raw.iter().map(|&c| key_identity(ControlId(c))).collect();
        let mut pairs = 0;
        for i in 0..raw.len() {
            for j in i + 1..raw.len() {
                let one_key = keys[i] == keys[j];
                assert_eq!(ids[i] == ids[j], one_key, "{:#x} {:#x}", raw[i], raw[j]);
                pairs += usize::from(one_key);
            }
        }
        assert_eq!(pairs, 5);
    }

    /// Counts the carets the window thread is asked to take, and what it
    /// reads then.
    #[cfg(feature = "dev-tools")]
    struct CaretAsks {
        shared: std::sync::Arc<super::Shared>,
        read: Vec<Option<[f32; 4]>>,
    }

    #[cfg(feature = "dev-tools")]
    impl winit::application::ApplicationHandler<super::Ask> for CaretAsks {
        fn resumed(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {}

        fn window_event(
            &mut self,
            _event_loop: &winit::event_loop::ActiveEventLoop,
            _id: winit::window::WindowId,
            _event: winit::event::WindowEvent,
        ) {
        }

        fn user_event(
            &mut self,
            _event_loop: &winit::event_loop::ActiveEventLoop,
            ask: super::Ask,
        ) {
            use std::sync::atomic::Ordering;
            if matches!(ask, super::Ask::Caret) {
                self.shared.caret_queued.store(false, Ordering::Release);
                self.read.push(*self.shared.caret.lock().unwrap());
            }
        }
    }

    /// Over the window's real event loop proxy: the first frame presented,
    /// a play's included, tells the window with nothing allocated, and
    /// caret moves made before the window takes one reach it as one ask,
    /// with the latest area. (The only test that makes an event loop: one
    /// per process.)
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_play_frame_tells_the_window_unallocated_and_carets_coalesce_alloc() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        use render::{AdapterChoice, Renderer, Target};
        use winit::event_loop::EventLoop;
        use winit::platform::pump_events::EventLoopExtPumpEvents;
        use winit::platform::windows::EventLoopBuilderExtWindows;

        use crate::menu_screen::{MenuScreen, WindowSink};

        let _warp = crate::warp_turn::take();
        let mut event_loop = EventLoop::<super::Ask>::with_user_event()
            .with_any_thread(true)
            .build()
            .unwrap();
        let shared = std::sync::Arc::new(super::Shared::default());
        let link = super::WindowLink {
            proxy: event_loop.create_proxy(),
            shared: std::sync::Arc::clone(&shared),
        };
        let target = Target::Offscreen {
            width: 64,
            height: 36,
        };
        let renderer = Renderer::new(target, AdapterChoice::Warp).unwrap();
        let screen = MenuScreen::new(renderer, None).with_sink(Box::new(link.clone()));

        let before = crate::alloc_count::this_thread();
        assert!(screen.shown(true));
        assert_eq!(crate::alloc_count::this_thread() - before, 0);
        assert!(shared.presented.load(Ordering::Acquire));

        let mut asks = CaretAsks {
            shared: std::sync::Arc::clone(&shared),
            read: Vec::new(),
        };
        for x in [1.0, 2.0, 3.0] {
            link.caret(Some([x, 0.0, 1.0, 1.0]));
        }
        for _ in 0..10 {
            let _ = event_loop.pump_app_events(Some(Duration::from_millis(10)), &mut asks);
        }
        assert_eq!(asks.read, [Some([3.0, 0.0, 1.0, 1.0])]);
        link.caret(None);
        for _ in 0..10 {
            let _ = event_loop.pump_app_events(Some(Duration::from_millis(10)), &mut asks);
        }
        assert_eq!(asks.read.last(), Some(&None));
        assert_eq!(asks.read.len(), 2);
    }

    /// A queue validated by a fresh tuning's ingress word, and the window's
    /// IME session.
    fn gated() -> (
        std::sync::Arc<crate::tuning::LiveTuning>,
        crate::select::MenuQueue,
        super::ImeSession,
    ) {
        let tuning = std::sync::Arc::clone(
            crate::controls::Controls::new(input::QpcClock::start()).tuning(),
        );
        let queue = crate::select::MenuQueue::gated(std::sync::Arc::clone(&tuning));
        (tuning, queue, super::ImeSession::default())
    }

    /// `ime` through the session, published under the word now.
    fn publish(
        tuning: &crate::tuning::LiveTuning,
        queue: &crate::select::MenuQueue,
        session: &mut super::ImeSession,
        ime: winit::event::Ime,
    ) {
        let now = tuning.activity().ingress();
        if let Some((input, stamp)) = session.event(ime, now) {
            queue.push_stamped(input, Some(stamp));
        }
    }

    fn preedit(text: &str) -> winit::event::Ime {
        winit::event::Ime::Preedit(text.to_owned(), None)
    }

    fn commit(text: &str) -> winit::event::Ime {
        winit::event::Ime::Commit(text.to_owned())
    }

    /// E17: a composition open at the choice, and one begun during the
    /// decision screen, apply neither their preedits nor their commits
    /// after it (committed after the deadline or after a refusal's return
    /// alike: both reopen the word); a composition begun after the
    /// reopening commits (the positive control).
    #[test]
    fn an_ime_composition_from_the_epoch_commits_nothing() {
        use crate::select::MenuInput;
        for begun_in_epoch in [false, true] {
            let (tuning, queue, mut session) = gated();
            if !begun_in_epoch {
                publish(&tuning, &queue, &mut session, preedit("か"));
                assert_eq!(queue.pop(), Some(MenuInput::Preedit("か".to_owned())));
            }
            tuning.close_ingress(|| {});
            if begun_in_epoch {
                publish(&tuning, &queue, &mut session, preedit("か"));
            }
            tuning.open_ingress(|| {});
            publish(&tuning, &queue, &mut session, preedit("かな"));
            publish(&tuning, &queue, &mut session, preedit(""));
            publish(&tuning, &queue, &mut session, commit("仮名"));
            assert_eq!(queue.pop(), None, "begun in the epoch {begun_in_epoch}");
            publish(&tuning, &queue, &mut session, preedit("あ"));
            publish(&tuning, &queue, &mut session, commit("亜"));
            assert_eq!(queue.pop(), Some(MenuInput::Preedit("あ".to_owned())));
            assert_eq!(queue.pop(), Some(MenuInput::Text("亜".to_owned())));
        }
    }

    /// E17b: a preedit and a commit stamped while the word was open, and
    /// published only after a choice and a reopening (the window thread
    /// held between its stamp and its enqueue), are not applied where the
    /// queue is taken; a fresh session's commit is.
    #[test]
    fn a_stamped_commit_published_after_the_epoch_is_not_applied() {
        use crate::select::MenuInput;
        let (tuning, queue, mut session) = gated();
        let stamp = tuning.activity().ingress();
        let held = [
            session.event(preedit("か"), stamp),
            session.event(commit("可"), stamp),
        ];
        tuning.close_ingress(|| {});
        tuning.open_ingress(|| {});
        for (input, stamp) in held.into_iter().flatten() {
            queue.push_stamped(input, Some(stamp));
        }
        assert_eq!(queue.pop(), None);
        publish(&tuning, &queue, &mut session, commit("化"));
        assert_eq!(queue.pop(), Some(MenuInput::Text("化".to_owned())));
    }

    /// The window's controls, keys and select queue, as the window has
    /// them.
    fn window() -> (
        crate::controls::Controls,
        super::Keys,
        crate::select::MenuQueue,
    ) {
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        controls.quit_leaves();
        let queue = crate::select::MenuQueue::gated(std::sync::Arc::clone(controls.tuning()));
        (controls, super::Keys::new(None, None), queue)
    }

    fn code(code: winit::keyboard::KeyCode) -> winit::keyboard::PhysicalKey {
        winit::keyboard::PhysicalKey::Code(code)
    }

    /// A press of `key` in the window, `repeat` and `synthetic` as given,
    /// typing `text`.
    fn key_press(
        w: &mut (
            crate::controls::Controls,
            super::Keys,
            crate::select::MenuQueue,
        ),
        key: winit::keyboard::PhysicalKey,
        repeat: bool,
        synthetic: bool,
        text: Option<&str>,
    ) {
        let (controls, keys, queue) = w;
        super::keyboard(
            controls,
            keys,
            Some(queue),
            super::KeyInput {
                key,
                pressed: true,
                repeat,
                synthetic,
                text,
            },
        );
    }

    /// The scene moves at each of the decision screen's edges, as the rig
    /// moves it: the choice's attempt entry, the deadline, a return.
    fn close(w: &crate::controls::Controls) {
        w.close_epoch();
        w.next_scene();
    }

    fn open(w: &crate::controls::Controls) {
        w.open_epoch();
        w.next_scene();
    }

    /// The overlay key toggles the overlay outside the decision screen and
    /// does nothing inside it.
    #[test]
    fn the_overlay_key_does_nothing_in_the_decision_epoch() {
        use std::sync::atomic::Ordering;
        let mut w = window();
        let overlay = super::window_key(game::OVERLAY_KEY);
        let shown = std::sync::Arc::clone(w.0.overlay());
        key_press(&mut w, overlay, false, false, None);
        assert!(shown.load(Ordering::SeqCst), "the positive control");
        close(&w.0);
        key_press(&mut w, overlay, false, false, None);
        assert!(shown.load(Ordering::SeqCst), "toggled in the epoch");
        open(&w.0);
        key_press(&mut w, overlay, false, false, None);
        assert!(!shown.load(Ordering::SeqCst));
    }

    /// Menu keys and typed text reach the select screen outside the
    /// decision screen and not inside it; a key pressed inside it and held
    /// across its end repeats nothing until pressed afresh, whose repeats
    /// then act.
    #[test]
    fn menu_keys_and_text_need_a_fresh_press_after_the_epoch() {
        use crate::select::{MenuInput, MenuKey};
        use winit::keyboard::KeyCode;
        let mut w = window();
        key_press(&mut w, code(KeyCode::ArrowDown), false, false, None);
        key_press(&mut w, code(KeyCode::KeyA), false, false, Some("a"));
        assert_eq!(w.2.pop(), Some(MenuInput::Key(MenuKey::Down)));
        assert_eq!(w.2.pop(), Some(MenuInput::Text("a".to_owned())));
        close(&w.0);
        key_press(&mut w, code(KeyCode::ArrowDown), false, false, None);
        key_press(&mut w, code(KeyCode::KeyA), false, false, Some("a"));
        assert_eq!(w.2.pop(), None, "published in the epoch");
        open(&w.0);
        key_press(&mut w, code(KeyCode::ArrowDown), true, false, None);
        assert_eq!(w.2.pop(), None, "a repeat of a press from the epoch");
        key_press(&mut w, code(KeyCode::ArrowDown), false, false, None);
        key_press(&mut w, code(KeyCode::ArrowDown), true, false, None);
        assert_eq!(w.2.pop(), Some(MenuInput::Key(MenuKey::Down)));
        assert_eq!(w.2.pop(), Some(MenuInput::Key(MenuKey::Down)));
    }

    /// A press the window system makes up for a focus change reaches no
    /// menu; a real one does.
    #[test]
    fn a_made_up_press_reaches_no_menu() {
        use crate::select::{MenuInput, MenuKey};
        use winit::keyboard::KeyCode;
        let mut w = window();
        key_press(&mut w, code(KeyCode::ArrowDown), false, true, None);
        assert_eq!(w.2.pop(), None);
        key_press(&mut w, code(KeyCode::ArrowDown), false, false, None);
        assert_eq!(w.2.pop(), Some(MenuInput::Key(MenuKey::Down)));
    }

    /// Past the decision screen's time, while practice waits for its read:
    /// an Escape queued before the screen (the practice choice's own
    /// queue) is stale and does not leave; one pressed in the screen
    /// before the escape is let through is not published; one pressed after
    /// it leaves.
    #[test]
    fn only_a_fresh_escape_leaves_the_waiting_decision_screen() {
        use winit::keyboard::KeyCode;
        let mut w = window();
        key_press(&mut w, code(KeyCode::Escape), false, false, None);
        close(&w.0);
        key_press(&mut w, code(KeyCode::Escape), false, false, None);
        w.0.let_escape();
        assert!(
            !w.2.take_escape(|s| w.0.admits_escape(s)),
            "a stale Escape left"
        );
        key_press(&mut w, code(KeyCode::Escape), false, false, None);
        assert!(
            w.2.take_escape(|s| w.0.admits_escape(s)),
            "a fresh Escape did not"
        );
    }
}
