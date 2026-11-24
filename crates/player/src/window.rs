//! The window thread: the winit window and its events (design.md "Rendering and
//! Skins"). The window is borderless fullscreen on the primary monitor and
//! takes no device events (Raw Input has one target window per device type per
//! process, and the input thread owns it). It is made hidden and shown only once
//! the render thread presented its first frame, confirmed by the swap
//! chain ([`Showing`]), so the empty window Windows would paint first is
//! never seen, and brought forward with no input sent ([`bring_forward`]).
//! The startup has one outcome ([`Readiness`], design.md "Showing the
//! window"): the first present, or a failure, never a window shown without
//! a frame; the game waits for it before anything else, and no play opens
//! without it ([`WindowShown`]). Focus, the retry and quit keys, the settings keys F1
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
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::monitor::MonitorHandle;
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    Win32WindowHandle, WindowHandle,
};
use winit::window::{Fullscreen, Window, WindowAttributes, WindowId};

use crate::controls::Controls;
use crate::menu_screen::WindowSink;
use crate::pointer::{Capture, PointerShared};
use crate::render_side::Extent;
use crate::select::{MenuInput, MenuKey, MenuQueue};
use crate::topmost::{LevelWindow, Topmost};
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
    /// Show the cursor, or hide it (a play runs).
    Cursor(bool),
    /// Look at the foreground window: another program's focus or
    /// foreground changed while the window is topmost ([`crate::topmost`]).
    Look,
}

/// The startup's outcome (design.md "Showing the window"): `Pending`
/// moves to `Showing` when the window thread takes the first confirmed
/// present before the deadline, to `Closed` when the window is closed
/// first, or to `Failed`; `Showing` ends in `FirstPresent`, set once the
/// window is shown and its focus published, so a waiter released by it
/// sees the window's focus, or in `Failed` when the show is still held at
/// the deadline. One lock arbitrates every move, so a close and a show
/// never both win.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// No frame presented yet, no failure.
    Pending,
    /// The window thread is showing the window at its first present.
    Showing,
    /// The window is shown, its focus published.
    FirstPresent,
    /// The window was closed before its first present: the run ends as at
    /// any close, the window never shown.
    Closed,
    /// The startup failed: the window is never shown.
    Failed(StartFailure),
}

/// Why the startup failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StartFailure {
    /// No frame was presented by the deadline.
    #[error("no frame was presented within {0:?}")]
    NoFrame(Duration),
    /// The render thread could not build or draw its first screen.
    #[error("{0}")]
    Render(String),
}

/// How the game's wait for the startup ended without its first present.
#[derive(Debug, PartialEq, Eq)]
pub enum StartEnd {
    /// The startup failed.
    Failed(StartFailure),
    /// The window closed first: the run ends as at any close.
    Closed,
}

/// What the window thread shares with the threads that draw into the
/// window and wait for it.
#[derive(Debug)]
struct Shared {
    /// Set by a frame presented, confirmed by the swap chain: the hidden
    /// window looks at it every [`SHOW_POLL`] until the startup's outcome.
    /// A flag, so a play's frame tells it with nothing allocated.
    presented: Arc<AtomicBool>,
    /// The caret's latest area in layout pixels: moves are coalesced, so
    /// the window takes the latest only.
    caret: Mutex<Option<[f32; 4]>>,
    /// An [`Ask::Caret`] is on its way for [`caret`](Self::caret).
    caret_queued: AtomicBool,
    /// The client size, for the render thread.
    extent: Arc<Extent>,
    /// The startup's outcome.
    readiness: Mutex<Readiness>,
    readiness_changed: Condvar,
    /// The startup's bound, and its deadline from the window's making.
    bound: Duration,
    deadline: Instant,
}

impl Shared {
    /// A window's shared state, its startup due within `bound` from now.
    fn new(bound: Duration) -> Self {
        Self {
            presented: Arc::default(),
            caret: Mutex::default(),
            caret_queued: AtomicBool::new(false),
            extent: Arc::default(),
            readiness: Mutex::new(Readiness::Pending),
            readiness_changed: Condvar::new(),
            bound,
            deadline: Instant::now() + bound,
        }
    }

    fn readiness(&self) -> Readiness {
        self.readiness
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Moves `Pending` to `to` when `allowed`; whether it did. Waiters are
    /// told.
    fn advance(&self, to: Readiness, allowed: bool) -> bool {
        let mut r = self
            .readiness
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *r != Readiness::Pending || !allowed {
            return false;
        }
        *r = to;
        self.readiness_changed.notify_all();
        true
    }

    /// Takes the first present at `now` for the show: only while
    /// `Pending` and before the deadline, compared here, so a present
    /// looked at past it fails the startup even if the look came late.
    fn claim(&self, now: Instant) -> bool {
        self.advance(Readiness::Showing, now < self.deadline)
    }

    /// The window was shown and its focus published: `Showing` ends in
    /// `FirstPresent`, and the waiters are released. Whether it did: a
    /// show still held at the deadline was failed by then
    /// ([`give_up`](Self::give_up)).
    fn settle(&self) -> bool {
        let mut r = self
            .readiness
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *r != Readiness::Showing {
            return false;
        }
        *r = Readiness::FirstPresent;
        self.readiness_changed.notify_all();
        true
    }

    /// Fails the startup for `why`, unless the first present was taken,
    /// the window closed or it failed already; whether it did.
    fn fail(&self, why: StartFailure) -> bool {
        self.advance(Readiness::Failed(why), true)
    }

    /// The window was closed: a startup still `Pending` ends `Closed`, so
    /// no present taken later shows it; whether it did.
    fn close(&self) -> bool {
        self.advance(Readiness::Closed, true)
    }

    /// Fails the startup for `why` at its deadline, a show still held then
    /// included (`Showing`): the outcome is due by the deadline, whatever
    /// the window thread is doing; whether it did.
    fn give_up(&self, why: StartFailure) -> bool {
        let mut r = self
            .readiness
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !matches!(*r, Readiness::Pending | Readiness::Showing) {
            return false;
        }
        *r = Readiness::Failed(why);
        self.readiness_changed.notify_all();
        true
    }
}

/// How often the hidden window looks for its first frame.
const SHOW_POLL: Duration = Duration::from_millis(10);

/// The startup's bound `--dev-render-timeout` sets (dev-tools), set once
/// before the window opens ([`set_startup_bound`]).
#[cfg(feature = "dev-tools")]
static STARTUP_BOUND: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

/// Bounds the startup of the window that opens at `bound` in place of
/// [`crate::render_side::STARTUP_TIMEOUT`] (`--dev-render-timeout`,
/// dev-tools); the first bound set stays.
#[cfg(feature = "dev-tools")]
pub fn set_startup_bound(bound: Duration) {
    let _ = STARTUP_BOUND.set(bound);
}

/// The startup's bound of the window that opens.
fn startup_bound() -> Duration {
    #[cfg(feature = "dev-tools")]
    if let Some(b) = STARTUP_BOUND.get() {
        return *b;
    }
    crate::render_side::STARTUP_TIMEOUT
}

/// What showing the window needs of it: [`Window`], or a fake in tests.
/// None of it sends input (design.md "Rendering and Skins", Showing the
/// window): winit's `focus_window` taps Alt with `SendInput` to get around
/// Windows' foreground lock, and that Alt reaches the window in front.
trait ShowWindow {
    /// Shows the window active: Windows brings it to the foreground when
    /// the player may take it.
    fn show_active(&self);
    fn has_focus(&self) -> bool;
    /// Asks Windows for the foreground (`SetForegroundWindow`): whether it
    /// was granted.
    fn ask_foreground(&self) -> bool;
    /// Flashes the taskbar button until the window comes to the
    /// foreground.
    fn flash(&self);
    /// Hides the window again: its startup failed while it was shown.
    fn hide(&self);
}

impl ShowWindow for Window {
    fn show_active(&self) {
        // Made with winit's default `active`, so shown with `SW_SHOW`.
        self.set_visible(true);
    }

    fn has_focus(&self) -> bool {
        Window::has_focus(self)
    }

    fn ask_foreground(&self) -> bool {
        let Some(h) = hwnd(self) else {
            return false;
        };
        // SAFETY: the window is open: it is `self`.
        unsafe { windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(h) }.as_bool()
    }

    fn flash(&self) {
        use windows::Win32::UI::WindowsAndMessaging::{
            FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx,
        };
        let Some(h) = hwnd(self) else {
            return;
        };
        let info = FLASHWINFO {
            cbSize: u32::try_from(size_of::<FLASHWINFO>()).unwrap_or(u32::MAX),
            hwnd: h,
            dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        // SAFETY: the window is open (it is `self`) and `info` outlives
        // the call.
        let _ = unsafe { FlashWindowEx(&raw const info) };
    }

    fn hide(&self) {
        self.set_visible(false);
    }
}

/// The window's Win32 handle.
pub(crate) fn hwnd(window: &Window) -> Option<windows::Win32::Foundation::HWND> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(windows::Win32::Foundation::HWND(
            h.hwnd.get() as *mut std::ffi::c_void
        )),
        _ => None,
    }
}

/// Shows `window` and brings it forward without input of its own: shown
/// active; when Windows kept another window in front, the foreground
/// asked for once; refused again, its taskbar button flashed. Whether it
/// has focus.
fn bring_forward(window: &impl ShowWindow) -> bool {
    window.show_active();
    if window.has_focus() || (window.ask_foreground() && window.has_focus()) {
        return true;
    }
    window.flash();
    println!("window: shown behind the window in front; its taskbar button flashes");
    false
}

/// The window thread's side of the startup's outcome: whether it is
/// settled.
#[derive(Debug, Default)]
struct Showing {
    settled: bool,
}

/// What a look at the startup's outcome found ([`Showing::poll`]).
#[derive(Debug, PartialEq, Eq)]
enum Polled {
    /// Still waiting: look again then.
    Again(Instant),
    /// Settled by this look: the game is woken, once.
    Settled,
    /// Settled before: nothing more to do.
    Idle,
}

impl Showing {
    /// Looks at `now` for the startup's outcome: a confirmed present taken
    /// before the deadline shows `window`, tells `focus` whether it has
    /// focus (the window's focus change, [`App::focus`]), so the focus
    /// latch and the input thread see the window gain focus even when no
    /// focus event follows the show, then settles `FirstPresent`; a show
    /// held past the deadline, which the game failed meanwhile
    /// ([`Shared::give_up`]), hides the window again and tells `focus` it
    /// has none; the deadline without a present fails the startup and
    /// `close`s the controls, which wakes the game wherever it waits; a
    /// failure the game set or a close is taken as it is.
    fn poll(
        &mut self,
        window: &impl ShowWindow,
        mut focus: impl FnMut(bool),
        close: impl FnOnce(),
        shared: &Shared,
        now: Instant,
    ) -> Polled {
        if self.settled {
            return Polled::Idle;
        }
        if let Readiness::Failed(_) | Readiness::Closed = shared.readiness() {
            self.settled = true;
            return Polled::Settled;
        }
        if shared.presented.load(Ordering::Acquire) && shared.claim(now) {
            self.settled = true;
            focus(bring_forward(window));
            if !shared.settle() {
                window.hide();
                focus(false);
                println!("window: the startup failed while the window was shown; hidden again");
            }
            return Polled::Settled;
        }
        if now >= shared.deadline {
            self.settled = true;
            if shared.fail(StartFailure::NoFrame(shared.bound)) {
                println!("window: no frame within {:?}; not shown", shared.bound);
                close();
            }
            return Polled::Settled;
        }
        Polled::Again((now + SHOW_POLL).min(shared.deadline))
    }
}

/// The window's close button: a startup still `Pending` ends `Closed` in
/// the same move a show would take ([`Shared::close`]), then the controls
/// close, which wakes the game.
fn close_requested(shared: &Shared, controls: &Controls) {
    shared.close();
    controls.close();
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

/// What the cursor's switch needs of the window: [`Window`], or a fake in
/// tests.
trait CursorWindow {
    fn show_cursor(&self, shown: bool);
}

impl CursorWindow for Window {
    fn show_cursor(&self, shown: bool) {
        self.set_cursor_visible(shown);
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
    /// The pointer's state shared with the game and render threads.
    pointer: Arc<PointerShared>,
    /// The mouse's held button, position and wheel fraction.
    capture: Capture,
    /// Whether the window was made topmost.
    topmost: Topmost,
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
        match make_window(event_loop, monitor) {
            Ok(w) => {
                self.focus(w.has_focus());
                let surface = surface(&w);
                let _ = ready.send(surface);
                publish(&self.shared.extent, &w);
                self.window = Some(w);
                self.showing = Some(Showing::default());
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
                self.focus(focused);
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
            WindowEvent::CloseRequested => close_requested(&self.shared, &self.controls),
            WindowEvent::CursorMoved { position, .. } => {
                // Client pixels: far inside i32.
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a window position, saturated at i32's ends"
                )]
                let at = [position.x.floor() as i32, position.y.floor() as i32];
                let word = self.pointer.word();
                self.capture
                    .moved(&self.pointer, at, word, self.controls.scene());
            }
            WindowEvent::CursorLeft { .. } => {
                let word = self.pointer.word();
                self.capture
                    .left(&self.pointer, word, self.controls.scene());
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let (Some(menu), Some(button)) = (&self.menu, pointer_button(button)) {
                    let word = self.pointer.word();
                    let controls = &self.controls;
                    self.capture.button(
                        &self.pointer,
                        button,
                        state == ElementState::Pressed,
                        word,
                        controls.scene(),
                        |input| send_pointer(menu, controls, input),
                    );
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(menu) = &self.menu {
                    let word = self.pointer.word();
                    let controls = &self.controls;
                    self.capture.wheel(
                        &self.pointer,
                        wheel_lines(delta),
                        word,
                        controls.scene(),
                        |input| send_pointer(menu, controls, input),
                    );
                }
            }
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
            (Ask::Cursor(shown), Some(w)) => show_cursor(w, shown),
            (Ask::Look, w) => self.topmost.asked(w.as_ref()),
            (Ask::Ime(_) | Ask::Caret | Ask::Cursor(_), None) => {}
            (Ask::Exit, _) => {
                // The window goes before the loop ends, on its own thread,
                // and its watch of the foreground with it.
                self.topmost = Topmost::default();
                self.window = None;
                event_loop.exit();
            }
        }
    }
}

impl App {
    /// The window's focus is `focused` ([`focus_changed`]).
    fn focus(&mut self, focused: bool) {
        let window = self.window.as_ref();
        focus_changed(
            &mut self.capture,
            &self.pointer,
            &self.controls,
            &mut self.topmost,
            window,
            focused,
        );
    }

    /// Shows the hidden window once its first frame was presented, or
    /// fails the startup at its deadline; until then the loop wakes to
    /// look again. Either way the game thread is woken, once, and the
    /// window thread looks no more.
    fn poll_showing(&mut self, event_loop: &ActiveEventLoop) {
        let Some(w) = &self.window else {
            return;
        };
        let Some(showing) = &mut self.showing else {
            return;
        };
        let (capture, pointer, controls, topmost) = (
            &mut self.capture,
            &self.pointer,
            &self.controls,
            &mut self.topmost,
        );
        let focus = |f| focus_changed(capture, pointer, controls, topmost, Some(w), f);
        let close = || controls.close();
        let polled = showing.poll(w, focus, close, &self.shared, Instant::now());
        let flow = match polled {
            Polled::Again(next) => ControlFlow::WaitUntil(next),
            Polled::Settled | Polled::Idle => ControlFlow::Wait,
        };
        event_loop.set_control_flow(flow);
        if polled == Polled::Settled {
            self.showing = None;
            // The show's time on the run's clock, against the first
            // present's (dev-tools, the startup's order).
            #[cfg(feature = "dev-tools")]
            if self.shared.readiness() == Readiness::FirstPresent {
                println!("dev: window shown at {} us", self.controls.now().0);
            }
            self.controls.wake_game();
        }
    }
}

/// The window's focus is `focused`, as every focus change the window
/// reports goes: the pointer cancelled first on a loss, then the controls
/// told, which wake the game thread ([`Capture::focus_changed`]), then
/// `window`'s topmost state ([`Topmost::focus`]), after the controls so a
/// play's abort waits for nothing.
fn focus_changed(
    capture: &mut Capture,
    pointer: &PointerShared,
    controls: &Controls,
    topmost: &mut Topmost,
    window: Option<&impl LevelWindow>,
    focused: bool,
) {
    capture.focus_changed(pointer, focused, |f| controls.focus(f));
    if let Some(w) = window {
        topmost.focus(w, focused);
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

/// Shows or hides the window's cursor from the game thread.
#[derive(Clone)]
pub struct CursorSwitch(CursorSink);

#[derive(Clone)]
enum CursorSink {
    Window(EventLoopProxy<Ask>),
    /// What the game thread asked, in order (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    Seen(Arc<Mutex<Vec<bool>>>),
}

impl CursorSwitch {
    /// Shows the cursor, or hides it while a play runs; a window already
    /// closed ignores it.
    pub fn set(&self, shown: bool) {
        match &self.0 {
            CursorSink::Window(proxy) => {
                let _ = proxy.send_event(Ask::Cursor(shown));
            }
            #[cfg(all(test, feature = "dev-tools"))]
            CursorSink::Seen(seen) => seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(shown),
        }
    }

    /// A switch that records what it is asked, and the record (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn recording() -> (Self, Arc<Mutex<Vec<bool>>>) {
        let seen = Arc::default();
        (Self(CursorSink::Seen(Arc::clone(&seen))), seen)
    }
}

/// Shows or hides `window`'s cursor, as the game thread asked.
fn show_cursor(window: &impl CursorWindow, shown: bool) {
    window.show_cursor(shown);
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

    /// [`close_after`](Self::close_after), waiting for the window thread
    /// until `by` at the latest ([`crate::join_by`]): a window thread still
    /// held then (a show stalled in Windows) is left with its window to
    /// the process's exit.
    pub fn close_by(self, rendered: bool, by: Instant) {
        if rendered {
            let _ = self.proxy.send_event(Ask::Exit);
            let _ = crate::join_by(self.handle, by);
        }
    }

    /// The switch of the window's IME.
    pub fn ime(&self) -> ImeSwitch {
        ImeSwitch(self.proxy.clone())
    }

    /// The switch of the window's cursor.
    pub fn cursor(&self) -> CursorSwitch {
        CursorSwitch(CursorSink::Window(self.proxy.clone()))
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

/// The startup's outcome, for the game: it waits for it once after the
/// render thread started ([`WindowShown::wait_ready`]), and a play opens
/// only once it is `FirstPresent`, so the focus the window gained when
/// shown is the play's (a hidden window has none, and would latch the
/// play's focus loss at its opening).
#[derive(Clone)]
pub struct WindowShown(Arc<Shared>);

impl WindowShown {
    /// The startup's outcome now.
    pub fn readiness(&self) -> Readiness {
        self.0.readiness()
    }

    /// Fails the startup for `why` unless it was settled: the window is
    /// then never shown.
    pub fn fail(&self, why: StartFailure) -> bool {
        self.0.fail(why)
    }

    /// The startup's deadline: the render thread's first screen is waited
    /// for until it too.
    pub fn deadline(&self) -> Instant {
        self.0.deadline
    }

    /// Waits on the game thread for the startup's outcome: `FirstPresent`,
    /// a failure, or a close (`Closed`, or `quitting`). A render thread
    /// that failed (`render_failure` says why) fails it at once. The thread
    /// parks until the deadline, woken by the window thread's outcome and a
    /// close (through [`Controls::wake_game`]) and by the render thread's
    /// failure (the caller sets its wake to the same); a missed wake costs
    /// at most the deadline. At the deadline it fails the startup itself,
    /// a show the window thread still holds included, so the outcome is
    /// never later than the deadline.
    ///
    /// # Errors
    ///
    /// How it ended without the first present.
    pub fn wait_ready(
        &self,
        quitting: &dyn Fn() -> bool,
        render_failure: &dyn Fn() -> Option<String>,
    ) -> Result<(), StartEnd> {
        loop {
            match self.0.readiness() {
                Readiness::FirstPresent => return Ok(()),
                Readiness::Failed(why) => return Err(StartEnd::Failed(why)),
                Readiness::Closed => return Err(StartEnd::Closed),
                Readiness::Pending | Readiness::Showing => {}
            }
            if quitting() {
                return Err(StartEnd::Closed);
            }
            if let Some(why) = render_failure()
                && self.0.fail(StartFailure::Render(why))
            {
                continue;
            }
            let now = Instant::now();
            if now >= self.0.deadline {
                self.0.give_up(StartFailure::NoFrame(self.0.bound));
                continue;
            }
            std::thread::park_timeout(self.0.deadline - now);
        }
    }

    /// Waits until the outcome is no longer `Pending` or `Showing`, at
    /// most `timeout`; the outcome then.
    #[cfg(test)]
    fn wait(&self, timeout: Duration) -> Readiness {
        let r = self
            .0
            .readiness
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (r, _) = self
            .0
            .readiness_changed
            .wait_timeout_while(r, timeout, |r| {
                matches!(r, Readiness::Pending | Readiness::Showing)
            })
            .unwrap_or_else(PoisonError::into_inner);
        r.clone()
    }

    /// A startup whose outcome is `r`, its deadline far (tests).
    #[cfg(test)]
    pub(crate) fn settled(r: Readiness) -> Self {
        let shared = Shared::new(Duration::from_secs(3_600));
        *shared
            .readiness
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = r;
        Self(Arc::new(shared))
    }
}

/// The client size `--dev-window` asks for (dev-tools), set once before
/// the window opens ([`set_dev_window`]).
#[cfg(feature = "dev-tools")]
static DEV_WINDOW: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();

/// Makes the window that opens a window of client size `size`, in
/// physical pixels, in place of the borderless fullscreen one
/// (`--dev-window`, dev-tools); the first size set stays.
#[cfg(feature = "dev-tools")]
pub fn set_dev_window(size: (u32, u32)) {
    let _ = DEV_WINDOW.set(size);
}

/// Whether the window opens borderless fullscreen: every window but a
/// `--dev-window` one (dev-tools), and nothing changes it once open.
#[cfg(feature = "dev-tools")]
pub fn opens_fullscreen() -> bool {
    DEV_WINDOW.get().is_none()
}

/// The window's attributes, hidden until shown, and the fullscreen it
/// takes once made: borderless fullscreen on `monitor`; with
/// `--dev-window` (dev-tools), [`dev_window`]'s and none. The fullscreen
/// is not an attribute: winit brings a window made fullscreen to the
/// foreground by tapping Alt, as `focus_window` does ([`ShowWindow`]).
fn attributes(monitor: Option<MonitorHandle>) -> (WindowAttributes, Option<Fullscreen>) {
    #[cfg(feature = "dev-tools")]
    if let Some(&size) = DEV_WINDOW.get() {
        let at = monitor.as_ref().map(MonitorHandle::position);
        println!("window: a {}x{} window (--dev-window)", size.0, size.1);
        return (dev_window(size, at), None);
    }
    let attributes = Window::default_attributes()
        .with_title("tapestry")
        .with_visible(false);
    (attributes, Some(Fullscreen::Borderless(monitor)))
}

/// Makes the hidden window on `monitor`: made with [`attributes`], then
/// given their fullscreen, before its size is read (winit makes a window
/// fullscreen the same way just after making it).
fn make_window(
    event_loop: &ActiveEventLoop,
    monitor: Option<MonitorHandle>,
) -> Result<Window, winit::error::OsError> {
    let (attributes, fullscreen) = attributes(monitor);
    let w = event_loop.create_window(attributes)?;
    if fullscreen.is_some() {
        w.set_fullscreen(fullscreen);
    }
    Ok(w)
}

/// A window of client size `size` in physical pixels at `at` (the chosen
/// monitor's top-left corner), hidden until shown, undecorated so its
/// client area is the whole window, and fixed in size, so the render
/// target is `size` (dev-tools).
#[cfg(feature = "dev-tools")]
fn dev_window(size: (u32, u32), at: Option<PhysicalPosition<i32>>) -> WindowAttributes {
    let attrs = Window::default_attributes()
        .with_title("tapestry")
        .with_visible(false)
        .with_decorations(false)
        .with_resizable(false)
        .with_inner_size(PhysicalSize::new(size.0, size.1));
    match at {
        Some(at) => attrs.with_position(at),
        None => attrs,
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
/// `menu`, the select screen's input goes there, the pointer's while
/// `pointer`'s word is active.
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
    pointer: Arc<PointerShared>,
) -> anyhow::Result<(WindowThread, Surface)> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (proxy_tx, proxy_rx) = mpsc::channel();
    let shared = Arc::new(Shared::new(startup_bound()));
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
            let looks = event_loop.create_proxy();
            crate::topmost::on_look(move || {
                let _ = looks.send_event(Ask::Look);
            });
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
                pointer,
                capture: Capture::default(),
                topmost: Topmost::default(),
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

impl Surface {
    /// A surface of `width`×`height` over a handle nothing draws into
    /// (tests: a factory whose renderer step is replaced).
    #[cfg(test)]
    pub(crate) fn unused(width: u32, height: u32) -> Self {
        Self {
            hwnd: NonZeroIsize::MIN,
            width,
            height,
            refresh_mhz: None,
        }
    }

    /// The window's handle, for the startup cause probe (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub const fn hwnd(&self) -> NonZeroIsize {
        self.hwnd
    }
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
/// the decision screen), which is checked where it is taken; a repeat goes
/// only while the scene of its key's press is current, so a key held
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
    // Every input goes with the word and the time it was observed at;
    // where it is taken, one from the decision screen is dropped, and an
    // Escape observed past its time is the screen's to take.
    let stamp = controls.ingress();
    for input in key.into_iter().chain(text) {
        send_menu(menu, controls, input, Some(stamp));
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
    let at = controls.now();
    menu.push_stamped(
        input,
        stamp.map(|stamp| crate::select::Observed { stamp, at }),
    );
    controls.wake_game();
}

/// Hands the pointer's `input` to the select screen, stamped with the
/// ingress word as keys are, and wakes the game thread when it was
/// admitted; the presses admitted so far, or none when it was refused.
fn send_pointer(menu: &MenuQueue, controls: &Controls, input: MenuInput) -> Option<u64> {
    let observed = crate::select::Observed {
        stamp: controls.ingress(),
        at: controls.now(),
    };
    let admitted = menu.push_stamped(input, Some(observed));
    if admitted.is_some() {
        controls.wake_game();
    }
    admitted
}

/// The pointer's button for a mouse button; none for another.
const fn pointer_button(button: MouseButton) -> Option<skin_model::Button> {
    Some(match button {
        MouseButton::Left => skin_model::Button::Left,
        MouseButton::Right => skin_model::Button::Right,
        MouseButton::Middle => skin_model::Button::Middle,
        MouseButton::Back => skin_model::Button::Back,
        MouseButton::Forward => skin_model::Button::Forward,
        MouseButton::Other(_) => return None,
    })
}

/// A wheel's turn in lines, away from the user positive: a pixel delta
/// counts a line each 120 pixels, a wheel notch's delta on Windows.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a wheel's turn is far inside f32"
)]
fn wheel_lines(delta: MouseScrollDelta) -> f32 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => y,
        MouseScrollDelta::PixelDelta(p) => (p.y / 120.0) as f32,
    }
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
    /// The window is borderless fullscreen on its monitor and sized by
    /// nothing else, made fullscreen once it exists: winit taps Alt for a
    /// window made fullscreen.
    #[test]
    fn the_window_is_made_borderless_fullscreen_after_it_exists() {
        let (a, fullscreen) = super::attributes(None);
        assert_eq!(
            fullscreen,
            Some(winit::window::Fullscreen::Borderless(None))
        );
        assert_eq!(a.fullscreen, None);
        assert_eq!(a.inner_size, None);
        assert!(!a.visible);
        assert!(a.active, "shown with SW_SHOW, not SW_SHOWNOACTIVATE");
    }

    /// `--dev-window`'s window: the asked client size in physical pixels
    /// at the monitor's corner, not fullscreen, undecorated and fixed in
    /// size, hidden until shown.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_dev_window_has_the_asked_client_size_at_the_monitors_corner() {
        use winit::dpi::{PhysicalPosition, PhysicalSize, Position, Size};
        let at = PhysicalPosition::new(-13, 1080);
        let a = super::dev_window((1920, 1080), Some(at));
        assert_eq!(
            a.inner_size,
            Some(Size::Physical(PhysicalSize::new(1920, 1080)))
        );
        assert_eq!(a.position, Some(Position::Physical(at)));
        assert_eq!(a.fullscreen, None);
        assert!(!a.decorations);
        assert!(!a.resizable);
        assert!(!a.visible);
        assert_eq!(super::dev_window((1280, 720), None).position, None);
    }

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

    impl super::CursorWindow for FakeWindow {
        fn show_cursor(&self, shown: bool) {
            self.asked.borrow_mut().push(format!("cursor {shown}"));
        }
    }

    /// P10: the window shows or hides its cursor as asked, and the game
    /// thread asks for it shown on entering selection and the practice
    /// page (a play hides it at its opening and shows it again at its
    /// result and at its end).
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_cursor_shows_on_the_menus() {
        let w = FakeWindow::new(1_920, 1_080);
        super::show_cursor(&w, false);
        super::show_cursor(&w, true);
        assert_eq!(w.take(), ["cursor false", "cursor true"]);
        let (switch, seen) = super::CursorSwitch::recording();
        let ((), _) = crate::tests::on_rig(game::DeviceSettings::default(), |rig, _| {
            use game::AppRig as _;
            rig.cursor = Some(switch);
            rig.adjust = None;
            rig.show_cursor(false);
            rig.entered_selection();
            rig.show_cursor(false);
            let c = bms_model::ChartBuilder::new(120_000).build().unwrap();
            rig.entered_practice(&game::PracticeScreen::new(&c, &rules::tapestry(), 0));
        });
        assert_eq!(*seen.lock().unwrap(), [false, true, false, true]);
    }

    /// P10: the decision screen keeps the cursor shown: entering it from
    /// selection asks for nothing.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_cursor_stays_shown_on_the_decision_screen() {
        let (switch, seen) = super::CursorSwitch::recording();
        let ((), _) = crate::tests::on_rig(game::DeviceSettings::default(), |rig, _| {
            use game::AppRig as _;
            rig.cursor = Some(switch);
            rig.adjust = None;
            rig.entered_selection();
            let _ = rig.entered_decision(
                &game::Pick::Play(std::path::PathBuf::from("chart.bms")),
                game::Decision::off(),
            );
        });
        assert_eq!(*seen.lock().unwrap(), [true]);
    }

    /// The hidden window as showing sees it: whether Windows brings it to
    /// the foreground when it is shown and when it asks, and what was done
    /// to it.
    #[derive(Default)]
    struct FakeShow {
        front_when_shown: bool,
        granted: bool,
        front: std::cell::Cell<bool>,
        done: std::cell::RefCell<Vec<&'static str>>,
    }

    impl FakeShow {
        fn shows(&self) -> usize {
            self.done.borrow().iter().filter(|&&d| d == "show").count()
        }
    }

    impl super::ShowWindow for FakeShow {
        fn show_active(&self) {
            self.done.borrow_mut().push("show");
            self.front.set(self.front_when_shown);
        }

        fn has_focus(&self) -> bool {
            self.front.get()
        }

        fn ask_foreground(&self) -> bool {
            self.done.borrow_mut().push("ask");
            self.front.set(self.granted);
            self.granted
        }

        fn flash(&self) {
            self.done.borrow_mut().push("flash");
        }

        fn hide(&self) {
            self.done.borrow_mut().push("hide");
        }
    }

    /// Showing sends no input: the window is shown active, asks for the
    /// foreground once only when Windows kept another window in front,
    /// and flashes its taskbar button only when that is refused too, and
    /// the focus reported is the window's. Nothing else is done to it
    /// (`ShowWindow` has no input to send, where winit's `focus_window`
    /// taps Alt).
    #[test]
    fn showing_brings_the_window_forward_without_input() {
        for (front_when_shown, granted, done, focused) in [
            (true, false, &["show"][..], true),
            (false, true, &["show", "ask"][..], true),
            (false, false, &["show", "ask", "flash"][..], false),
        ] {
            let w = FakeShow {
                front_when_shown,
                granted,
                ..FakeShow::default()
            };
            assert_eq!(super::bring_forward(&w), focused, "{done:?}");
            assert_eq!(*w.done.borrow(), done);
        }
    }

    /// How many times the process called `SendInput` since
    /// [`SendInputCount::install`]: [`counted`] takes the calls.
    static SEND_INPUT_CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// `SendInput`'s stand-in: counts the call and reports every input
    /// sent, so no caller tries again.
    unsafe extern "system" fn counted(
        count: u32,
        _inputs: *const std::ffi::c_void,
        _size: i32,
    ) -> u32 {
        SEND_INPUT_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        count
    }

    /// The process's `SendInput`, counted and never sent: every slot of the
    /// executable's import table holding it points at [`counted`] until
    /// dropped. winit's calls, as this crate's, go through those slots.
    struct SendInputCount {
        slots: Vec<*mut usize>,
        original: usize,
    }

    impl SendInputCount {
        fn install() -> Self {
            use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
            use windows::core::{PCWSTR, s, w};
            // SAFETY: user32 is loaded (winit links it), and both names
            // are NUL-terminated literals.
            let original = unsafe {
                let user32 = GetModuleHandleW(w!("user32.dll")).unwrap();
                GetProcAddress(user32, s!("SendInput")).unwrap() as usize
            };
            // SAFETY: a null name asks for the executable's own module.
            let base = unsafe { GetModuleHandleW(PCWSTR::null()) }.unwrap().0 as usize;
            // SAFETY: `base` is the loaded executable, a PE32+ image.
            let slots = unsafe { import_slots(base, original) };
            assert!(!slots.is_empty(), "SendInput is not in the import table");
            let to = counted as unsafe extern "system" fn(u32, *const std::ffi::c_void, i32) -> u32;
            for &slot in &slots {
                // SAFETY: an import table slot of this image.
                unsafe { write_slot(slot, to as usize) };
            }
            Self { slots, original }
        }

        fn calls(&self) -> u32 {
            SEND_INPUT_CALLS.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Drop for SendInputCount {
        fn drop(&mut self) {
            for &slot in &self.slots {
                // SAFETY: an import table slot of this image, given back
                // what it held.
                unsafe { write_slot(slot, self.original) };
            }
        }
    }

    /// The import table slots of the PE32+ image at `base` that hold
    /// `target`.
    ///
    /// # Safety
    ///
    /// `base` is a loaded PE32+ image.
    unsafe fn import_slots(base: usize, target: usize) -> Vec<*mut usize> {
        let u32_at = |at: usize| -> usize {
            // SAFETY: an offset the PE format fixes, inside the image's
            // headers or import directory, which the loader mapped.
            let value = unsafe { (at as *const u32).read_unaligned() };
            value as usize
        };
        let nt = base + u32_at(base + 0x3C);
        assert_eq!(u32_at(nt), 0x4550, "the PE signature");
        // The optional header follows the 4-byte signature and the 20-byte
        // file header; a PE32+ one has its data directories at 112, the
        // import directory second.
        let optional = nt + 24;
        assert_eq!(u32_at(optional) & 0xFFFF, 0x20B, "a PE32+ image");
        let mut descriptor = base + u32_at(optional + 112 + 8);
        let mut slots = Vec::new();
        // 20-byte import descriptors, the last all zero: its name at 12,
        // its import address table at 16.
        while u32_at(descriptor + 12) != 0 {
            let mut slot = base + u32_at(descriptor + 16);
            loop {
                // SAFETY: a slot of the descriptor's table, which ends at a
                // zero slot.
                let held = unsafe { (slot as *const usize).read() };
                if held == 0 {
                    break;
                }
                if held == target {
                    slots.push(slot as *mut usize);
                }
                slot += size_of::<usize>();
            }
            descriptor += 20;
        }
        slots
    }

    /// Points import table slot `slot` at `to`.
    ///
    /// # Safety
    ///
    /// `slot` is an import table slot of this process's image.
    unsafe fn write_slot(slot: *mut usize, to: usize) {
        use windows::Win32::System::Memory::{
            PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect,
        };
        let at = slot.cast::<std::ffi::c_void>().cast_const();
        let mut old = PAGE_PROTECTION_FLAGS(0);
        let mut unused = PAGE_PROTECTION_FLAGS(0);
        // SAFETY: the slot is one aligned pointer-sized word of the image,
        // writable only between the two protection changes.
        unsafe {
            VirtualProtect(at, size_of::<usize>(), PAGE_READWRITE, &raw mut old).unwrap();
            slot.write_volatile(to);
            VirtualProtect(at, size_of::<usize>(), old, &raw mut unused).unwrap();
        }
    }

    /// The window made as the player makes it, at the event loop's first
    /// resume.
    struct Made(Option<Result<winit::window::Window, winit::error::OsError>>);

    impl winit::application::ApplicationHandler<super::Ask> for Made {
        fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
            if self.0.is_none() {
                self.0 = Some(super::make_window(event_loop, None));
            }
        }

        fn window_event(
            &mut self,
            _event_loop: &winit::event_loop::ActiveEventLoop,
            _id: winit::window::WindowId,
            _event: winit::event::WindowEvent,
        ) {
        }
    }

    /// Making and showing a real window as the player does calls
    /// `SendInput` nowhere (design.md "Rendering and Skins", Showing the
    /// window), where winit taps Alt through it for a window made
    /// fullscreen by its attributes and in `focus_window`. The window
    /// lives on a desktop of the test's own, so nothing appears on the
    /// screen and no foreground is taken, and `SendInput` is counted and
    /// never sent. (It makes an event loop, which a process makes once:
    /// nextest gives the test a process of its own.)
    #[test]
    fn a_real_window_is_made_and_shown_without_send_input() {
        use std::time::{Duration, Instant};

        use winit::event_loop::EventLoop;
        use winit::platform::pump_events::EventLoopExtPumpEvents;
        use winit::platform::windows::EventLoopBuilderExtWindows;

        let run = std::thread::spawn(|| {
            crate::test_window::own_desktop();
            let sent = SendInputCount::install();
            let mut event_loop = EventLoop::<super::Ask>::with_user_event()
                .with_any_thread(true)
                .build()
                .unwrap();
            let mut made = Made(None);
            let begun = Instant::now();
            while made.0.is_none() && begun.elapsed() < Duration::from_secs(5) {
                let _ = event_loop.pump_app_events(Some(Duration::from_millis(10)), &mut made);
            }
            let window = made.0.take().expect("the event loop resumed").unwrap();
            let _ = super::bring_forward(&window);
            for _ in 0..10 {
                let _ = event_loop.pump_app_events(Some(Duration::from_millis(10)), &mut made);
            }
            assert_eq!(
                sent.calls(),
                0,
                "SendInput was called while the window was made or shown"
            );
            drop(window);
        });
        if let Err(panic) = run.join() {
            std::panic::resume_unwind(panic);
        }
    }

    /// A startup bound no test reaches.
    const FAR: std::time::Duration = std::time::Duration::from_secs(3_600);

    /// W3.2: the window side once a frame was presented and confirmed: the
    /// hidden window shows at its next look, once, its focus reaches the
    /// controls, and the outcome is the first present.
    #[test]
    fn a_presented_frame_shows_the_window_and_lets_the_play_open() {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let c = Controls::new(QpcClock::start());
        let w = FakeShow {
            front_when_shown: true,
            ..FakeShow::default()
        };
        let shared = std::sync::Arc::new(super::Shared::new(FAR));
        let shown = super::WindowShown(std::sync::Arc::clone(&shared));
        let start = Instant::now();
        let mut showing = super::Showing::default();
        // Made hidden, the window had no focus.
        c.focus(false);
        // Nothing presented: it looks again soon, still hidden.
        assert_eq!(
            showing.poll(&w, |f| c.focus(f), || c.close(), &shared, start),
            super::Polled::Again(start + super::SHOW_POLL)
        );
        assert_eq!(w.shows(), 0);
        assert_eq!(shown.wait(Duration::ZERO), super::Readiness::Pending);
        shared.presented.store(true, Ordering::Release);
        assert_eq!(
            showing.poll(&w, |f| c.focus(f), || c.close(), &shared, start),
            super::Polled::Settled
        );
        assert_eq!(w.shows(), 1);
        assert!(c.focused());
        assert!(!c.quitting());
        assert_eq!(shown.wait(Duration::ZERO), super::Readiness::FirstPresent);
        // Settled once: a later look does nothing, so the game is woken
        // and the show marked once.
        assert_eq!(
            showing.poll(&w, |f| c.focus(f), || c.close(), &shared, start),
            super::Polled::Idle
        );
        assert_eq!(w.shows(), 1);
    }

    /// W3.2: a waiter is released only once the window is shown and its
    /// focus published: while the show is held, the outcome is `Showing`,
    /// a failure is refused, and the waiter waits on; released, the
    /// focus is set before the waiter returns.
    #[test]
    fn the_waiters_are_released_after_the_focus() {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        /// A window whose show holds until the test releases it.
        struct HeldShow {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl super::ShowWindow for HeldShow {
            fn show_active(&self) {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            fn has_focus(&self) -> bool {
                true
            }
            fn ask_foreground(&self) -> bool {
                true
            }
            fn flash(&self) {}
            fn hide(&self) {}
        }

        let c = std::sync::Arc::new(Controls::new(QpcClock::start()));
        c.focus(false);
        let shared = std::sync::Arc::new(super::Shared::new(FAR));
        shared.presented.store(true, Ordering::Release);
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let window_side = std::sync::Arc::clone(&shared);
        let wc = std::sync::Arc::clone(&c);
        let window = std::thread::spawn(move || {
            let w = HeldShow {
                entered: entered_tx,
                release: release_rx,
            };
            let mut showing = super::Showing::default();
            showing.poll(
                &w,
                |f| wc.focus(f),
                || wc.close(),
                &window_side,
                Instant::now(),
            )
        });
        entered.recv().unwrap();
        // The show is held: taken, not settled.
        assert_eq!(shared.readiness(), super::Readiness::Showing);
        assert!(!shared.fail(super::StartFailure::NoFrame(FAR)));
        let shown = super::WindowShown(std::sync::Arc::clone(&shared));
        let wc = std::sync::Arc::clone(&c);
        let waiter = std::thread::spawn(move || {
            let r = shown.wait(Duration::from_secs(60));
            (r, wc.focused())
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "released before the show");
        release.send(()).unwrap();
        assert_eq!(window.join().unwrap(), super::Polled::Settled);
        let (r, focused) = waiter.join().unwrap();
        assert_eq!(r, super::Readiness::FirstPresent);
        assert!(focused, "the waiter saw the outcome before the focus");
    }

    /// W3.1, W3.3: without a frame the window is never shown: at its
    /// deadline the startup fails and the controls close, which wakes the
    /// game, and nothing before the deadline does either.
    #[test]
    fn the_deadline_without_a_frame_never_shows() {
        use std::time::Duration;
        let c = Controls::new(QpcClock::start());
        let w = FakeShow::default();
        let bound = Duration::from_millis(200);
        let shared = super::Shared::new(bound);
        let deadline = shared.deadline;
        let mut showing = super::Showing::default();
        let late = deadline - Duration::from_millis(1);
        assert_eq!(
            showing.poll(&w, |f| c.focus(f), || c.close(), &shared, late),
            super::Polled::Again(deadline)
        );
        assert!(!c.quitting());
        assert_eq!(
            showing.poll(&w, |f| c.focus(f), || c.close(), &shared, deadline),
            super::Polled::Settled
        );
        assert_eq!(w.shows(), 0);
        assert!(c.quitting(), "the controls were not closed");
        assert_eq!(
            shared.readiness(),
            super::Readiness::Failed(super::StartFailure::NoFrame(bound))
        );
    }

    /// W3.2: a present the window thread looks at only past the deadline
    /// is not shown, even though it is still `Pending`: the claim compares
    /// the time with the deadline itself.
    #[test]
    fn a_present_seen_after_the_deadline_never_shows() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        let c = Controls::new(QpcClock::start());
        let w = FakeShow::default();
        let shared = super::Shared::new(Duration::from_millis(200));
        shared.presented.store(true, Ordering::Release);
        let mut showing = super::Showing::default();
        let late = shared.deadline + Duration::from_millis(1);
        assert_eq!(
            showing.poll(&w, |f| c.focus(f), || c.close(), &shared, late),
            super::Polled::Settled
        );
        assert_eq!(w.shows(), 0);
        assert!(matches!(shared.readiness(), super::Readiness::Failed(_)));
    }

    /// W3.2: once the startup failed, a frame presented later never shows
    /// the window, and the window thread closes nothing more.
    #[test]
    fn a_late_present_after_a_failure_never_shows() {
        use std::sync::atomic::Ordering;
        use std::time::Instant;
        let c = Controls::new(QpcClock::start());
        let w = FakeShow::default();
        let shared = super::Shared::new(FAR);
        assert!(shared.fail(super::StartFailure::Render("refused".to_owned())));
        shared.presented.store(true, Ordering::Release);
        let mut showing = super::Showing::default();
        assert_eq!(
            showing.poll(&w, |f| c.focus(f), || c.close(), &shared, Instant::now()),
            super::Polled::Settled
        );
        assert_eq!(w.shows(), 0);
        assert!(!c.quitting());
        assert_eq!(
            shared.readiness(),
            super::Readiness::Failed(super::StartFailure::Render("refused".to_owned()))
        );
    }

    /// W3.3: the game's wait ends at once on each of its ends: the first
    /// present, the window thread's failure, a close and the render
    /// thread's failure, which fails the startup; past the deadline with no
    /// window thread to settle it, the wait fails it itself.
    #[test]
    fn the_readiness_wait_ends_on_each_end() {
        use std::time::{Duration, Instant};
        let never = || None;
        let open = || false;
        // Settled first present.
        let s = super::WindowShown(std::sync::Arc::new(super::Shared::new(FAR)));
        assert!(s.0.claim(Instant::now()));
        s.0.settle();
        assert_eq!(s.wait_ready(&open, &never), Ok(()));
        // A failure.
        let s = super::WindowShown(std::sync::Arc::new(super::Shared::new(FAR)));
        assert!(s.fail(super::StartFailure::NoFrame(FAR)));
        assert_eq!(
            s.wait_ready(&open, &never),
            Err(super::StartEnd::Failed(super::StartFailure::NoFrame(FAR)))
        );
        // A close.
        let s = super::WindowShown(std::sync::Arc::new(super::Shared::new(FAR)));
        assert_eq!(s.wait_ready(&|| true, &never), Err(super::StartEnd::Closed));
        // The render thread's failure, at once: a deadline a wait that
        // missed it would reach instead.
        let s = super::WindowShown(std::sync::Arc::new(super::Shared::new(
            Duration::from_secs(5),
        )));
        let asked = Instant::now();
        assert_eq!(
            s.wait_ready(&open, &|| Some("gone".to_owned())),
            Err(super::StartEnd::Failed(super::StartFailure::Render(
                "gone".to_owned()
            )))
        );
        assert!(asked.elapsed() < Duration::from_secs(5));
        // No window thread: given up at the deadline itself, not later.
        let bound = Duration::from_millis(50);
        let s = super::WindowShown(std::sync::Arc::new(super::Shared::new(bound)));
        assert_eq!(
            s.wait_ready(&open, &never),
            Err(super::StartEnd::Failed(super::StartFailure::NoFrame(bound)))
        );
        let late = Instant::now().saturating_duration_since(s.deadline());
        assert!(
            late < Duration::from_millis(500),
            "{late:?} past the deadline"
        );
    }

    /// W3.8: a show the window thread still holds at the deadline (its
    /// show or focus call stalled) fails the startup there: the game's
    /// wait publishes the failure itself and returns at the deadline; the
    /// show, once it returns, hides the window again with no focus.
    #[test]
    fn a_show_held_past_the_deadline_fails_the_startup() {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        /// A window whose show holds until the test releases it.
        struct HeldShow {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            done: std::cell::RefCell<Vec<&'static str>>,
        }
        impl super::ShowWindow for HeldShow {
            fn show_active(&self) {
                self.done.borrow_mut().push("show");
                self.entered.send(()).unwrap();
                let _ = self.release.recv_timeout(Duration::from_secs(60));
            }
            fn has_focus(&self) -> bool {
                true
            }
            fn ask_foreground(&self) -> bool {
                true
            }
            fn flash(&self) {}
            fn hide(&self) {
                self.done.borrow_mut().push("hide");
            }
        }

        let c = std::sync::Arc::new(Controls::new(QpcClock::start()));
        let bound = Duration::from_millis(300);
        let shared = std::sync::Arc::new(super::Shared::new(bound));
        shared.presented.store(true, Ordering::Release);
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let window_side = std::sync::Arc::clone(&shared);
        let wc = std::sync::Arc::clone(&c);
        let window = std::thread::spawn(move || {
            let w = HeldShow {
                entered: entered_tx,
                release: release_rx,
                done: std::cell::RefCell::default(),
            };
            let mut showing = super::Showing::default();
            let polled = showing.poll(
                &w,
                |f| wc.focus(f),
                || wc.close(),
                &window_side,
                Instant::now(),
            );
            (polled, w.done.take(), wc.focused())
        });
        entered.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_eq!(shared.readiness(), super::Readiness::Showing);
        // The game's wait, on a thread of its own: a wait that cannot fail
        // a held show never returns.
        let shown = super::WindowShown(std::sync::Arc::clone(&shared));
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let r = shown.wait_ready(&|| false, &|| None);
            let _ = tx.send((r, Instant::now()));
        });
        let ended = rx.recv_timeout(Duration::from_secs(10));
        release.send(()).unwrap();
        let (r, at) = ended.expect("the wait did not end at the deadline");
        assert_eq!(
            r,
            Err(super::StartEnd::Failed(super::StartFailure::NoFrame(bound)))
        );
        let late = at.saturating_duration_since(shared.deadline);
        assert!(
            late < Duration::from_millis(500),
            "{late:?} past the deadline"
        );
        let (polled, done, focused) = window.join().unwrap();
        assert_eq!(polled, super::Polled::Settled);
        assert_eq!(done, ["show", "hide"]);
        assert!(!focused, "a window hidden again kept its focus");
        assert!(matches!(shared.readiness(), super::Readiness::Failed(_)));
    }

    /// W3.9: a close settles the startup in the same move a show would
    /// take: a present counted after the close and before the window
    /// thread's next look never shows the window, and the game's wait ends
    /// as at a close even before it asks the controls.
    #[test]
    fn a_close_wins_over_a_present_counted_before_the_next_poll() {
        use std::sync::atomic::Ordering;
        use std::time::Instant;
        let c = Controls::new(QpcClock::start());
        let w = FakeShow {
            front_when_shown: true,
            ..FakeShow::default()
        };
        let shared = std::sync::Arc::new(super::Shared::new(FAR));
        let shown = super::WindowShown(std::sync::Arc::clone(&shared));
        let mut showing = super::Showing::default();
        super::close_requested(&shared, &c);
        shared.presented.store(true, Ordering::Release);
        assert_eq!(
            showing.poll(&w, |f| c.focus(f), || c.close(), &shared, Instant::now()),
            super::Polled::Settled
        );
        assert_eq!(w.shows(), 0);
        assert_eq!(shared.readiness(), super::Readiness::Closed);
        assert!(c.quitting());
        assert_eq!(
            shown.wait_ready(&|| false, &|| None),
            Err(super::StartEnd::Closed)
        );
    }

    /// W3.3: the window thread's outcome and the wake the window loop then
    /// sends end the game's parked wait long before its own bound.
    #[test]
    fn the_outcome_wakes_the_parked_game() {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let c = std::sync::Arc::new(Controls::new(QpcClock::start()));
        let shared = std::sync::Arc::new(super::Shared::new(FAR));
        let shown = super::WindowShown(std::sync::Arc::clone(&shared));
        let (tx, rx) = std::sync::mpsc::channel();
        let wc = std::sync::Arc::clone(&c);
        let game = std::thread::spawn(move || {
            wc.set_game(std::thread::current());
            tx.send(()).unwrap();
            let q = std::sync::Arc::clone(&wc);
            shown.wait_ready(&|| q.quitting(), &|| None)
        });
        rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        shared.presented.store(true, Ordering::Release);
        let mut showing = super::Showing::default();
        let w = FakeShow {
            front_when_shown: true,
            ..FakeShow::default()
        };
        let asked = Instant::now();
        assert_eq!(
            showing.poll(&w, |f| c.focus(f), || c.close(), &shared, Instant::now()),
            super::Polled::Settled
        );
        c.wake_game();
        assert_eq!(game.join().unwrap(), Ok(()));
        assert!(asked.elapsed() < Duration::from_secs(60));
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
            system: game::Volume {
                percent: 100,
                intent: false,
            },
            key: game::Volume {
                percent: 100,
                intent: false,
            },
            background: game::Volume {
                percent: 100,
                intent: false,
            },
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
        assert_eq!(pressed.values.unwrap().key.percent, 100, "at the limit");
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
        assert_eq!(s.values.unwrap().key.percent, 95);
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
                system: game::Volume {
                    percent: 100,
                    intent: false,
                },
                key: game::Volume {
                    percent: 100,
                    intent: false,
                },
                background: game::Volume {
                    percent: 100,
                    intent: false,
                },
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
        assert_ne!(after.key, before.key);
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
    /// with the latest area. (It makes an event loop, which a process makes
    /// once: nextest gives the test a process of its own.)
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
        let shared = std::sync::Arc::new(super::Shared::new(FAR));
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

        // W3.2: a present the swap chain did not count (an offscreen
        // frame's, or a count still 0) is presented but is not the first.
        let timing = |present_count| render::FrameTiming {
            cpu: Duration::ZERO,
            sprites: 0,
            dropped: 0,
            outcome: render::FrameOutcome::Shown,
            present_count,
            movies: [false; skin_model::movie::SLOTS],
        };
        for unconfirmed in [None, Some(0)] {
            assert!(screen.shown(&timing(unconfirmed)));
            assert!(!shared.presented.load(Ordering::Acquire));
        }
        // A frame the swap chain counted, as a window's first is.
        let confirmed = timing(Some(1));
        crate::alloc_count::room_for_output();
        let before = crate::alloc_count::this_thread();
        assert!(screen.shown(&confirmed));
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
            queue.push_stamped(
                input,
                Some(crate::select::Observed {
                    stamp,
                    at: bms_model::TimeUs(0),
                }),
            );
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
            queue.push_stamped(
                input,
                Some(crate::select::Observed {
                    stamp,
                    at: bms_model::TimeUs(0),
                }),
            );
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
        key_press(&mut w, code(KeyCode::KeyA), false, false, Some("a"));
        // Down is the last key pressed, in the epoch's scene.
        key_press(&mut w, code(KeyCode::ArrowDown), false, false, None);
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

    /// Past the decision screen's time `T`, while practice waits for its
    /// read, an Escape is taken by the time the window observed it: one
    /// observed after `T` leaves however late the screen looks (no look
    /// arms anything before it); one observed before `T` (in the screen,
    /// or queued with the choice) does not, though taken after it.
    #[test]
    fn an_escape_is_taken_by_its_observation_time() {
        use winit::keyboard::KeyCode;
        let mut w = window();
        // Observed before T, with the choice and in the screen.
        key_press(&mut w, code(KeyCode::Escape), false, false, None);
        close(&w.0);
        key_press(&mut w, code(KeyCode::Escape), false, false, None);
        let before = w.0.now();
        let deadline = bms_model::TimeUs(before.0 + 1);
        while w.0.now().0 < deadline.0 {
            std::thread::yield_now();
        }
        assert!(!w.2.take_escape(deadline), "an Escape from before T left");
        // Observed after T, before any look of the screen.
        key_press(&mut w, code(KeyCode::Escape), false, false, None);
        assert!(w.2.take_escape(deadline), "an Escape after T did not leave");
    }

    /// No window, the handles of the window itself, an ordinary window
    /// and a topmost window, as the fakes know them.
    const NONE: usize = 0;
    const OWN: usize = 1;
    const ORDINARY: usize = 2;
    const TOPMOST: usize = 3;

    fn handle(h: usize) -> windows::Win32::Foundation::HWND {
        windows::Win32::Foundation::HWND(std::ptr::without_provenance_mut(h))
    }

    /// The fullscreen window as the topmost state sees it: the foreground
    /// window, and what was done to it, with whether the controls had
    /// focus then.
    struct LevelSeen<'a> {
        controls: &'a Controls,
        front: std::cell::Cell<usize>,
        done: std::cell::RefCell<Vec<(&'static str, bool)>>,
    }

    impl<'a> LevelSeen<'a> {
        fn new(controls: &'a Controls) -> Self {
            Self {
                controls,
                front: std::cell::Cell::new(OWN),
                done: std::cell::RefCell::default(),
            }
        }

        fn push(&self, what: &'static str) {
            let focused = self.controls.focused();
            self.done.borrow_mut().push((what, focused));
        }

        fn what(&self) -> Vec<&'static str> {
            self.done.take().into_iter().map(|(what, _)| what).collect()
        }
    }

    impl crate::topmost::LevelWindow for LevelSeen<'_> {
        fn fullscreen(&self) -> bool {
            true
        }

        fn shown(&self) -> bool {
            true
        }

        fn foreground(&self) -> windows::Win32::Foundation::HWND {
            handle(self.front.get())
        }

        fn is(&self, front: windows::Win32::Foundation::HWND) -> bool {
            front == handle(OWN)
        }

        fn is_topmost(&self, front: windows::Win32::Foundation::HWND) -> bool {
            front == handle(TOPMOST)
        }

        fn watch(&self) -> Option<crate::topmost::Watch> {
            self.push("watch");
            Some(crate::topmost::Watch::fake())
        }

        fn raise(&self) {
            self.push("raise");
        }

        fn lower(&self, after: windows::Win32::Foundation::HWND) -> bool {
            self.push(if after == handle(ORDINARY) {
                "behind the ordinary window"
            } else if after == windows::Win32::UI::WindowsAndMessaging::HWND_NOTOPMOST {
                "on top of the ordinary windows"
            } else {
                "elsewhere"
            });
            true
        }
    }

    /// A look the watch asks for reaches the topmost state, which decides
    /// from the foreground window then: the window itself in front changes
    /// nothing; a focus loss while no window is in front waits; an ordinary
    /// window in front puts the window directly behind it; after focus came
    /// back, a topmost window in front (the task switcher) puts it on top
    /// of the ordinary windows; with no window yet, nothing happens.
    #[test]
    fn a_look_places_the_window_behind_the_window_in_front() {
        let c = Controls::new(QpcClock::start());
        let w = LevelSeen::new(&c);
        let mut capture = crate::pointer::Capture::default();
        let pointer = crate::pointer::PointerShared::default();
        let mut topmost = crate::topmost::Topmost::default();
        topmost.asked(None::<&LevelSeen<'_>>);
        super::focus_changed(&mut capture, &pointer, &c, &mut topmost, Some(&w), true);
        topmost.asked(Some(&w));
        w.front.set(NONE);
        super::focus_changed(&mut capture, &pointer, &c, &mut topmost, Some(&w), false);
        w.front.set(ORDINARY);
        topmost.asked(Some(&w));
        w.front.set(OWN);
        super::focus_changed(&mut capture, &pointer, &c, &mut topmost, Some(&w), true);
        w.front.set(TOPMOST);
        topmost.asked(Some(&w));
        assert_eq!(
            w.what(),
            [
                "watch",
                "raise",
                "behind the ordinary window",
                "watch",
                "raise",
                "on top of the ordinary windows"
            ]
        );
    }

    /// Every focus change the window reports reaches the controls, then
    /// the window's topmost state: gained, the window watches and is made
    /// topmost with the controls focused; lost with another window in
    /// front, it goes behind that window once the controls, and so a play's
    /// focus latch, have seen the loss; regained, it is topmost again.
    /// Before the window exists the controls are told all the same.
    #[test]
    fn a_focus_change_reaches_the_controls_then_the_topmost_state() {
        let c = Controls::new(QpcClock::start());
        let w = LevelSeen::new(&c);
        let mut capture = crate::pointer::Capture::default();
        let pointer = crate::pointer::PointerShared::default();
        let mut topmost = crate::topmost::Topmost::default();
        let mut focus = |window: Option<&LevelSeen<'_>>, focused| {
            super::focus_changed(&mut capture, &pointer, &c, &mut topmost, window, focused);
        };
        focus(None, false);
        assert!(!c.focused());
        focus(Some(&w), true);
        w.front.set(ORDINARY);
        focus(Some(&w), false);
        assert!(!c.focused());
        w.front.set(OWN);
        focus(Some(&w), true);
        assert_eq!(
            *w.done.borrow(),
            [
                ("watch", true),
                ("raise", true),
                ("behind the ordinary window", false),
                ("watch", true),
                ("raise", true)
            ]
        );
        focus(None, false);
        assert!(!c.focused());
    }

    /// The hidden fullscreen window as showing and the topmost state see
    /// it: whether Windows refuses it the foreground when it is shown and
    /// when it asks, whether it is shown and in front, and what was done to
    /// it, in order.
    #[derive(Default)]
    struct HiddenActive {
        refused: bool,
        visible: std::cell::Cell<bool>,
        front: std::cell::Cell<bool>,
        done: std::cell::RefCell<Vec<&'static str>>,
    }

    impl super::ShowWindow for HiddenActive {
        fn show_active(&self) {
            self.visible.set(true);
            self.front.set(!self.refused);
            self.done.borrow_mut().push("show");
        }

        fn has_focus(&self) -> bool {
            self.front.get()
        }

        fn ask_foreground(&self) -> bool {
            self.done.borrow_mut().push("ask");
            self.front.set(!self.refused);
            !self.refused
        }

        fn flash(&self) {
            self.done.borrow_mut().push("flash");
        }

        fn hide(&self) {
            self.visible.set(false);
            self.done.borrow_mut().push("hide");
        }
    }

    impl crate::topmost::LevelWindow for HiddenActive {
        fn fullscreen(&self) -> bool {
            true
        }

        fn shown(&self) -> bool {
            self.visible.get()
        }

        fn foreground(&self) -> windows::Win32::Foundation::HWND {
            handle(OWN)
        }

        fn is(&self, front: windows::Win32::Foundation::HWND) -> bool {
            front == handle(OWN)
        }

        fn is_topmost(&self, _: windows::Win32::Foundation::HWND) -> bool {
            false
        }

        fn watch(&self) -> Option<crate::topmost::Watch> {
            self.done.borrow_mut().push("watch");
            Some(crate::topmost::Watch::fake())
        }

        fn raise(&self) {
            self.done.borrow_mut().push("raise");
        }

        fn lower(&self, _: windows::Win32::Foundation::HWND) -> bool {
            self.done.borrow_mut().push("lower");
            true
        }
    }

    /// The window is never made topmost before it shows: a focus reported
    /// while it is hidden, before its first frame (when the render thread
    /// sets up its swap chain), raises nothing, and the show, once a frame
    /// was presented and Windows let the window take the foreground, makes
    /// it topmost after it is shown.
    #[test]
    fn the_window_is_made_topmost_only_once_shown() {
        use std::sync::atomic::Ordering;
        use std::time::Instant;
        let c = Controls::new(QpcClock::start());
        let w = HiddenActive::default();
        let mut capture = crate::pointer::Capture::default();
        let pointer = crate::pointer::PointerShared::default();
        let mut topmost = crate::topmost::Topmost::default();
        let shared = super::Shared::new(FAR);
        let start = Instant::now();
        let mut showing = super::Showing::default();
        let mut focus = |focused| {
            super::focus_changed(&mut capture, &pointer, &c, &mut topmost, Some(&w), focused);
        };
        focus(true);
        assert!(matches!(
            showing.poll(&w, &mut focus, || {}, &shared, start),
            super::Polled::Again(_)
        ));
        assert_eq!(*w.done.borrow(), Vec::<&str>::new());
        shared.presented.store(true, Ordering::Release);
        assert_eq!(
            showing.poll(&w, &mut focus, || {}, &shared, start),
            super::Polled::Settled
        );
        assert_eq!(*w.done.borrow(), ["show", "watch", "raise"]);
    }

    /// A window shown behind the window in front (Windows refused it the
    /// foreground, and again when it asked) is not made topmost: the show
    /// reports no focus, and it waits for the player to bring it forward.
    #[test]
    fn a_window_shown_behind_the_window_in_front_is_not_made_topmost() {
        use std::sync::atomic::Ordering;
        use std::time::Instant;
        let c = Controls::new(QpcClock::start());
        let w = HiddenActive {
            refused: true,
            ..HiddenActive::default()
        };
        let mut capture = crate::pointer::Capture::default();
        let pointer = crate::pointer::PointerShared::default();
        let mut topmost = crate::topmost::Topmost::default();
        let shared = super::Shared::new(FAR);
        let start = Instant::now();
        let mut showing = super::Showing::default();
        let mut focus = |focused| {
            super::focus_changed(&mut capture, &pointer, &c, &mut topmost, Some(&w), focused);
        };
        shared.presented.store(true, Ordering::Release);
        assert_eq!(
            showing.poll(&w, &mut focus, || {}, &shared, start),
            super::Polled::Settled
        );
        assert_eq!(*w.done.borrow(), ["show", "ask", "flash"]);
        assert!(!c.focused());
    }

    /// W3.1: the crash notes' sequence: no frame by the deadline while the
    /// hidden window gains the foreground. Nothing is shown, raised or made
    /// topmost, and the startup fails.
    #[test]
    fn a_window_without_a_frame_is_never_shown_or_raised() {
        let c = Controls::new(QpcClock::start());
        let w = HiddenActive::default();
        let mut capture = crate::pointer::Capture::default();
        let pointer = crate::pointer::PointerShared::default();
        let mut topmost = crate::topmost::Topmost::default();
        let shared = super::Shared::new(std::time::Duration::from_millis(200));
        let mut showing = super::Showing::default();
        let mut focus = |focused| {
            super::focus_changed(&mut capture, &pointer, &c, &mut topmost, Some(&w), focused);
        };
        focus(true);
        assert_eq!(
            showing.poll(&w, &mut focus, || {}, &shared, shared.deadline),
            super::Polled::Settled
        );
        assert_eq!(*w.done.borrow(), Vec::<&str>::new());
        assert!(matches!(
            shared.readiness(),
            super::Readiness::Failed(super::StartFailure::NoFrame(_))
        ));
    }
}
