//! The window's topmost state (design.md "Display Latency and Stutter",
//! topmost while focused): the borderless fullscreen window is topmost
//! while it has focus, so no other topmost window (an auto-hidden taskbar's
//! strip) covers it and keeps it from independent flip, and stops being
//! topmost when it loses focus, behind the window that took it. A hidden
//! window is never made topmost: made topmost before its first frame, the
//! window failed to start (the render thread's first swap chain was refused
//! as an invalid surface).

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowInfo, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SetWindowPos, WINDOWINFO, WS_EX_TOPMOST,
};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// What the topmost state needs of the window: [`Window`], or a fake in
/// tests.
pub trait LevelWindow {
    /// Whether the window is fullscreen now.
    fn fullscreen(&self) -> bool;
    /// Whether the window is shown: made hidden, it gains focus before its
    /// first frame (winit activates a window made fullscreen).
    fn shown(&self) -> bool;
    /// Makes the window topmost, in front of every topmost window.
    fn raise(&self);
    /// Ends the window's topmost state, behind the window in front
    /// ([`lower`]).
    fn lower(&self);
}

impl LevelWindow for Window {
    fn fullscreen(&self) -> bool {
        Window::fullscreen(self).is_some()
    }

    fn shown(&self) -> bool {
        self.is_visible() == Some(true)
    }

    fn raise(&self) {
        if let Some(h) = hwnd(self)
            && let Err(e) = raise(h)
        {
            eprintln!("window: not made topmost: {e}");
        }
    }

    fn lower(&self) {
        // SAFETY: a plain query; a null answer (no window in front) is
        // handled by `lower`.
        let front = unsafe { GetForegroundWindow() };
        if let Some(h) = hwnd(self)
            && let Err(e) = lower(h, front)
        {
            eprintln!("window: still topmost: {e}");
        }
    }
}

/// Whether the window was made topmost.
#[derive(Debug, Default)]
pub struct Topmost {
    raised: bool,
}

impl Topmost {
    /// The window's focus is `focused`. Gained while the window is
    /// fullscreen and shown, the window is made topmost, again when it
    /// already is, so it comes in front of any topmost window brought
    /// forward since; lost, or gained while not fullscreen or hidden, the
    /// window stops being topmost if it was, and is left where it is if it
    /// was not. The show reports the focus again ([`crate::window`]), so a
    /// focus gained while hidden makes the window topmost once it shows.
    pub fn focus(&mut self, window: &impl LevelWindow, focused: bool) {
        if focused && window.fullscreen() && window.shown() {
            window.raise();
            self.raised = true;
        } else if self.raised {
            window.lower();
            self.raised = false;
        }
    }
}

/// The window's Win32 handle.
fn hwnd(window: &Window) -> Option<HWND> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut std::ffi::c_void)),
        _ => None,
    }
}

/// Makes `hwnd` topmost, in front of every topmost window, neither moved,
/// sized nor activated. The window is raised when it gains focus, so from
/// the foreground process: Windows can ignore the call, with no error,
/// from a process in the background (test processes on the development
/// PC).
///
/// # Errors
///
/// Why Windows refused.
fn raise(hwnd: HWND) -> windows::core::Result<()> {
    // SAFETY: `SetWindowPos` checks the handle; nothing is borrowed.
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    }
}

/// Ends `hwnd`'s topmost state, behind `front` (the window that took
/// focus) as [`behind`] places it.
///
/// # Errors
///
/// Why Windows refused.
fn lower(hwnd: HWND, front: HWND) -> windows::core::Result<()> {
    let after = behind(hwnd, front, is_topmost(front));
    // SAFETY: `SetWindowPos` checks both handles; nothing is borrowed.
    unsafe {
        SetWindowPos(
            hwnd,
            Some(after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    }
}

/// Where `hwnd` goes when it stops being topmost: directly behind `front`
/// (the window that took focus) when that is an ordinary window, since a
/// topmost window placed after an ordinary one stops being topmost;
/// otherwise (`front_topmost`, such as the task switcher, or no window in
/// front) `HWND_NOTOPMOST`, the top of the ordinary windows, behind every
/// topmost one. `HWND_NOTOPMOST` with an ordinary window in front would put
/// `hwnd` in front of every ordinary window, the one that took focus
/// included.
fn behind(hwnd: HWND, front: HWND, front_topmost: bool) -> HWND {
    if front.is_invalid() || front == hwnd || front_topmost {
        HWND_NOTOPMOST
    } else {
        front
    }
}

/// Whether `hwnd` is a topmost window; a window that cannot be read counts
/// as not.
fn is_topmost(hwnd: HWND) -> bool {
    let mut info = WINDOWINFO {
        cbSize: u32::try_from(size_of::<WINDOWINFO>()).unwrap_or(u32::MAX),
        ..WINDOWINFO::default()
    };
    // SAFETY: `info` is a sized `WINDOWINFO` that outlives the call.
    unsafe { GetWindowInfo(hwnd, &raw mut info) }.is_ok() && info.dwExStyle.contains(WS_EX_TOPMOST)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GW_HWNDNEXT, GetWindow, WINDOW_EX_STYLE, WS_POPUP,
    };
    use windows::core::w;

    use super::{LevelWindow, Topmost};

    /// The window as the topmost state sees it: whether it is fullscreen
    /// and hidden, and what was done to it.
    #[derive(Default)]
    struct FakeLevel {
        fullscreen: bool,
        hidden: std::cell::Cell<bool>,
        done: RefCell<Vec<&'static str>>,
    }

    impl FakeLevel {
        fn fullscreen() -> Self {
            Self {
                fullscreen: true,
                ..Self::default()
            }
        }

        fn take(&self) -> Vec<&'static str> {
            self.done.take()
        }
    }

    impl LevelWindow for FakeLevel {
        fn fullscreen(&self) -> bool {
            self.fullscreen
        }

        fn shown(&self) -> bool {
            !self.hidden.get()
        }

        fn raise(&self) {
            self.done.borrow_mut().push("raise");
        }

        fn lower(&self) {
            self.done.borrow_mut().push("lower");
        }
    }

    /// The fullscreen window is made topmost when it gains focus and stops
    /// being topmost when it loses it.
    #[test]
    fn a_fullscreen_window_is_topmost_while_it_has_focus() {
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        t.focus(&w, true);
        assert_eq!(w.take(), ["raise"]);
        t.focus(&w, false);
        assert_eq!(w.take(), ["lower"]);
    }

    /// Focus regained makes the window topmost again, and a gain reported
    /// while it already is raises it again, in front of any topmost window
    /// brought forward since.
    #[test]
    fn regained_focus_makes_the_window_topmost_again() {
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        t.focus(&w, true);
        t.focus(&w, false);
        w.take();
        t.focus(&w, true);
        assert_eq!(w.take(), ["raise"]);
        t.focus(&w, true);
        assert_eq!(w.take(), ["raise"]);
    }

    /// A window that was never made topmost is left where it is when focus
    /// goes (the hidden window's first report is no focus), and a loss
    /// reported again lowers nothing more.
    #[test]
    fn a_window_not_topmost_is_left_where_it_is() {
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        t.focus(&w, false);
        assert_eq!(w.take(), Vec::<&str>::new());
        t.focus(&w, true);
        t.focus(&w, false);
        w.take();
        t.focus(&w, false);
        assert_eq!(w.take(), Vec::<&str>::new());
    }

    /// A hidden window that gains focus (winit activates a window made
    /// fullscreen before its first frame) is not made topmost; shown, a
    /// focus gain makes it topmost.
    #[test]
    fn a_hidden_window_is_not_made_topmost() {
        let w = FakeLevel::fullscreen();
        w.hidden.set(true);
        let mut t = Topmost::default();
        t.focus(&w, true);
        t.focus(&w, false);
        assert_eq!(w.take(), Vec::<&str>::new());
        w.hidden.set(false);
        t.focus(&w, true);
        assert_eq!(w.take(), ["raise"]);
    }

    /// A window that is not fullscreen (`--dev-window`) is never topmost,
    /// focused or not.
    #[test]
    fn a_window_that_is_not_fullscreen_is_never_topmost() {
        let w = FakeLevel::default();
        let mut t = Topmost::default();
        for focused in [true, false, true, true, false] {
            t.focus(&w, focused);
        }
        assert_eq!(w.take(), Vec::<&str>::new());
    }

    /// A hidden popup window of this thread, destroyed on drop.
    struct Popup(HWND);

    impl Popup {
        fn new() -> Self {
            // SAFETY: a predefined class, no parent, menu or creation data.
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("topmost test"),
                    WS_POPUP,
                    0,
                    0,
                    8,
                    8,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .expect("create a window");
            Self(hwnd)
        }

        /// Whether `self` is somewhere behind `front` in the order of
        /// windows.
        fn is_behind(&self, front: &Self) -> bool {
            let mut at = front.0;
            // SAFETY: plain queries of the order of windows; the walk ends
            // at the last window.
            while let Ok(next) = unsafe { GetWindow(at, GW_HWNDNEXT) } {
                if next == self.0 {
                    return true;
                }
                at = next;
            }
            false
        }
    }

    impl Drop for Popup {
        fn drop(&mut self) {
            // SAFETY: the window was created on this thread and is
            // destroyed once.
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    /// The window goes directly behind an ordinary window that took focus,
    /// and to the top of the ordinary windows when a topmost window took
    /// it, when none did, or when the window in front is its own.
    #[test]
    fn a_lowered_window_goes_behind_an_ordinary_window_in_front() {
        let (window, front) = (
            HWND(std::ptr::without_provenance_mut(1)),
            HWND(std::ptr::without_provenance_mut(2)),
        );
        assert_eq!(super::behind(window, front, false), front);
        for (front, front_topmost) in [(front, true), (HWND::default(), false), (window, false)] {
            assert_eq!(
                super::behind(window, front, front_topmost),
                super::HWND_NOTOPMOST,
                "{front:?} {front_topmost}"
            );
        }
    }

    /// On real windows: lowered while an ordinary window that came after
    /// it has focus, the window goes behind that window and is not topmost.
    /// The window is ordinary here, not raised first: Windows can ignore
    /// a raise from a test process in the background ([`super::raise`]),
    /// so the test needs no topmost window. Placed only "not topmost", an
    /// ordinary window would not move at all.
    #[test]
    fn a_lowered_window_lands_behind_the_window_in_front() {
        let front = Popup::new();
        let window = Popup::new();
        assert!(front.is_behind(&window));
        assert!(!super::is_topmost(front.0));
        super::lower(window.0, front.0).unwrap();
        assert!(window.is_behind(&front));
        assert!(!super::is_topmost(window.0));
    }
}
