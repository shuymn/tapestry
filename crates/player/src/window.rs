//! The window thread: the winit window and its events (design.md "Rendering and
//! Skins"). The window is borderless fullscreen on the primary monitor and
//! takes no device events (Raw Input has one target window per device type per
//! process, and the input thread owns it). Focus, the retry and quit keys, the
//! settings keys F1 to F8 and closing go to [`Controls`], which wakes the game
//! thread for each, and the input thread on a focus change once the focus
//! latch is set (design.md "Input"). With song selection the menu keys and
//! the text typed or composed through the IME go to the select screen too
//! ([`crate::select::MenuInput`], design.md "Play Lifecycle"), through a
//! queue that drops only keys past its bound ([`MenuQueue`]); the IME is
//! allowed only while the search box has focus, and told where its caret
//! is ([`ImeState`]) so the candidate window opens beside it. The event
//! loop runs on a thread of its own, so the game keeps the main thread and
//! the loaded chart never has to move.

use std::num::NonZeroIsize;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use anyhow::Context as _;
use eval::ControlId;
use game::{Adjust, Command};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, WindowEvent};
use winit::event_loop::{ActiveEventLoop, DeviceEvents, EventLoop, EventLoopProxy};
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
use crate::select::{MenuInput, MenuKey, MenuQueue};

/// What the game and render threads ask of the window thread.
enum Ask {
    /// End the event loop.
    Exit,
    /// Allow the IME (the search box has focus), or stop it.
    Ime(bool),
    /// The caret's area on the page drawn, in layout pixels (from the
    /// render thread).
    Caret(Option<[f32; 4]>),
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
            .with_fullscreen(Some(Fullscreen::Borderless(monitor)));
        match event_loop.create_window(attrs) {
            Ok(w) => {
                w.set_cursor_visible(false);
                self.controls.focus(w.has_focus());
                let surface = surface(&w);
                let _ = ready.send(surface);
                self.window = Some(w);
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
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                key_pressed(&self.controls, event.physical_key, event.repeat, &self.keys);
                if let Some(menu) = &self.menu {
                    let key = menu_key(event.physical_key, event.repeat).map(MenuInput::Key);
                    // Printable text only: control characters come as keys.
                    let text = event
                        .text
                        .filter(|t| !t.chars().any(char::is_control))
                        .map(|t| MenuInput::Text(t.to_string()));
                    for input in key.into_iter().chain(text) {
                        send_menu(menu, &self.controls, input);
                    }
                }
            }
            WindowEvent::Ime(ime) => {
                if let Some(menu) = &self.menu {
                    let input = match ime {
                        Ime::Commit(text) => Some(MenuInput::Text(text)),
                        Ime::Preedit(text, _) => Some(MenuInput::Preedit(text)),
                        Ime::Disabled => Some(MenuInput::Preedit(String::new())),
                        Ime::Enabled => None,
                    };
                    if let Some(input) = input {
                        send_menu(menu, &self.controls, input);
                    }
                }
            }
            WindowEvent::Resized(_) => {
                if let Some(w) = &self.window {
                    self.ime.report(w);
                }
            }
            WindowEvent::CloseRequested => self.controls.close(),
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Ask) {
        match (event, &self.window) {
            (Ask::Ime(allowed), Some(w)) => self.ime.allow(w, allowed),
            (Ask::Caret(caret), Some(w)) => self.ime.caret(w, caret),
            (Ask::Ime(_) | Ask::Caret(_), None) => {}
            (Ask::Exit, _) => {
                // The window goes before the loop ends, on its own thread.
                self.window = None;
                event_loop.exit();
            }
        }
    }
}

/// The running window: ends its thread when closed.
pub struct WindowThread {
    proxy: EventLoopProxy<Ask>,
    handle: JoinHandle<()>,
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
        WindowLink(self.proxy.clone())
    }
}

/// The render thread's line to the window: the caret's area of each menu
/// page drawn; a window already closed ignores it.
#[derive(Clone)]
pub struct WindowLink(EventLoopProxy<Ask>);

impl WindowSink for WindowLink {
    fn caret(&self, area: Option<[f32; 4]>) {
        let _ = self.0.send_event(Ask::Caret(area));
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
            let thread = WindowThread { proxy, handle };
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
    settings: [(PhysicalKey, Adjust); 8],
    overlay: PhysicalKey,
}

impl Keys {
    /// The keys for the retry and quit controls and the settings keys.
    fn new(retry: Option<u32>, quit: Option<u32>) -> Self {
        Self {
            retry: retry.map(window_key),
            quit: quit.map(window_key),
            settings: Adjust::ALL.map(|a| (window_key(a.key()), a)),
            overlay: window_key(game::OVERLAY_KEY),
        }
    }
}

/// A key pressed in the window. A settings key steps again while held
/// (key repeat), and wakes the game thread to show it; the overlay key,
/// retry and quit act on the first press only.
fn key_pressed(controls: &Controls, key: PhysicalKey, repeat: bool, keys: &Keys) {
    if let Some(&(_, a)) = keys.settings.iter().find(|(k, _)| *k == key) {
        controls.tuning().adjust(a);
        controls.wake_game();
    } else if repeat {
        // The others take the first press only.
    } else if key == keys.overlay {
        controls.toggle_overlay();
    } else if Some(key) == keys.retry {
        controls.command(Command::Retry);
    } else if Some(key) == keys.quit {
        controls.command(Command::Quit);
    }
}

/// Hands `input` to the select screen and wakes the game thread.
fn send_menu(menu: &MenuQueue, controls: &Controls, input: MenuInput) {
    menu.push(input);
    controls.wake_game();
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
        KeyCode::Backspace => (MenuKey::Backspace, true),
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

    /// Pause bound to retry and NumLock to quit, as `tapestry keys`
    /// captures them: the key config is valid, and in the window Pause
    /// retries and NumLock quits, never the other way round.
    #[test]
    fn pause_and_num_lock_mean_the_same_keys_in_the_window() {
        let mut config = DeviceSettings::default().keyboard;
        config.retry = Some(PAUSE);
        config.quit = Some(NUM_LOCK);
        assert_eq!(config.check(DeviceKind::Keyboard), Ok(()));
        let keys = Keys::new(config.retry, config.quit);

        let c = Controls::new(QpcClock::start());
        let pause = PhysicalKey::Code(KeyCode::Pause);
        key_pressed(&c, pause, false, &keys);
        assert_eq!(c.take(), Some(Command::Retry));
        assert!(!c.quitting());
        // A held Pause repeats nothing.
        key_pressed(&c, pause, true, &keys);
        assert_eq!(c.take(), None);
        key_pressed(&c, PhysicalKey::Code(KeyCode::NumLock), false, &keys);
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
            key_pressed(&c, pressed, false, &Keys::new(Some(control), None));
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
            KeyCode::F1,
            KeyCode::F2,
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
        // F2 (hi-speed up) steps the visible time in the window.
        let c = Controls::new(QpcClock::start());
        key_pressed(
            &c,
            PhysicalKey::Code(KeyCode::F2),
            true,
            &Keys::new(None, None),
        );
        assert_eq!(c.tuning().get().visible_time_ms, 569);
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
        let keys = Keys::new(Some(0x0E), Some(0x01));
        let f9 = PhysicalKey::Code(KeyCode::F9);
        assert_eq!(window_key(game::OVERLAY_KEY), f9);
        assert!(!c.overlay().load(Ordering::Acquire));
        key_pressed(&c, f9, false, &keys);
        assert!(c.overlay().load(Ordering::Acquire));
        key_pressed(&c, f9, true, &keys);
        assert!(c.overlay().load(Ordering::Acquire));
        key_pressed(&c, f9, false, &keys);
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
}
