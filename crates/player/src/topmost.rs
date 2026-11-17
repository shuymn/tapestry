//! The window's topmost state (design.md "Display Latency and Stutter",
//! topmost while focused): the borderless fullscreen window is topmost
//! while it has focus, so no other topmost window (an auto-hidden taskbar's
//! strip) covers it and keeps it from independent flip. It stops being
//! topmost once another window is in the foreground, directly behind that
//! window. Its own focus loss and Windows' focus and foreground events from
//! other programs (watched only while the window is topmost) only make it
//! look at the foreground window then: at the focus loss Windows may not
//! have set the new foreground window yet, and its events do not match
//! activations one to one. A hidden window is never made topmost: made
//! topmost before its first frame, the window failed to start (the render
//! thread's first swap chain was refused as an invalid surface).

use std::cell::{Cell, RefCell};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_FOCUS, EVENT_SYSTEM_FOREGROUND, GetForegroundWindow, GetWindowInfo,
    HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, WINDOWINFO,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WS_EX_TOPMOST,
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
    /// The foreground window now; null when there is none.
    fn foreground(&self) -> HWND;
    /// Whether `front` is this window.
    fn is(&self, front: HWND) -> bool;
    /// Whether `front` is a topmost window.
    fn is_topmost(&self, front: HWND) -> bool;
    /// Starts watching other programs' focus and foreground ([`Watch`]);
    /// `None` when Windows refuses.
    fn watch(&self) -> Option<Watch>;
    /// Makes the window topmost, in front of every topmost window.
    fn raise(&self);
    /// Places the window directly after `after` in the order of windows (a
    /// window, or `HWND_NOTOPMOST`: [`behind`]); whether it is no longer
    /// topmost then.
    fn lower(&self, after: HWND) -> bool;
}

impl LevelWindow for Window {
    fn fullscreen(&self) -> bool {
        Window::fullscreen(self).is_some()
    }

    fn shown(&self) -> bool {
        self.is_visible() == Some(true)
    }

    fn foreground(&self) -> HWND {
        // SAFETY: a plain query.
        unsafe { GetForegroundWindow() }
    }

    fn is(&self, front: HWND) -> bool {
        hwnd(self) == Some(front)
    }

    fn is_topmost(&self, front: HWND) -> bool {
        is_topmost(front)
    }

    fn watch(&self) -> Option<Watch> {
        let watch = hwnd(self).and_then(Watch::start);
        if watch.is_none() {
            eprintln!("window: other programs' focus cannot be watched; not made topmost");
        }
        watch
    }

    fn raise(&self) {
        let Some(h) = hwnd(self) else { return };
        match raise(h) {
            Err(e) => eprintln!("window: not made topmost: {e}"),
            Ok(()) if !is_topmost(h) => eprintln!("window: not made topmost (ignored)"),
            Ok(()) => {}
        }
    }

    fn lower(&self, after: HWND) -> bool {
        let Some(h) = hwnd(self) else { return true };
        if let Err(e) = place_after(h, after) {
            eprintln!("window: still topmost: {e}");
            return false;
        }
        let lowered = !is_topmost(h);
        if !lowered {
            eprintln!("window: still topmost (placement ignored)");
        }
        lowered
    }
}

/// Whether the window was made topmost: the watch of other programs it
/// keeps while it is.
#[derive(Debug, Default)]
pub struct Topmost {
    watch: Option<Watch>,
}

impl Topmost {
    /// The window's focus is `focused`. Gained while the window is
    /// fullscreen and shown, the window starts watching and is made
    /// topmost, again when it already is, so it comes in front of any
    /// topmost window brought forward since; a window that cannot watch is
    /// never made topmost, since nothing would end it. Lost, the window
    /// looks at the foreground window ([`Topmost::look`]). The show reports
    /// the focus again ([`crate::window`]), so a focus gained while hidden
    /// makes the window topmost once it shows.
    pub fn focus(&mut self, window: &impl LevelWindow, focused: bool) {
        if !focused {
            self.look(window);
            return;
        }
        if !(window.fullscreen() && window.shown()) {
            return;
        }
        if self.watch.is_none() {
            self.watch = window.watch();
        }
        if self.watch.is_some() {
            window.raise();
        }
    }

    /// A look the watch asked for ([`signal`]) was taken from the window
    /// thread's event loop: events may ask for another from now on, and
    /// `window`, once it exists, looks ([`Topmost::look`]).
    pub fn asked(&mut self, window: Option<&impl LevelWindow>) {
        LOOK_POSTED.set(false);
        if let Some(w) = window {
            self.look(w);
        }
    }

    /// Looks at the foreground window now: at the window's focus loss, or
    /// asked for by a focus or foreground event from another program
    /// ([`Watch`]). While the window is topmost and another window is in
    /// front, it goes directly behind that window ([`behind`]) and stops
    /// watching; when the window is itself in front, or none is (Windows
    /// has not set the new one yet), it waits for the next look. A
    /// placement Windows ignored leaves it topmost and watching, for the
    /// next look to try again. A foreground window that changed while the
    /// window was placed (the task switcher's choice taking the foreground)
    /// leaves it watching and asks for another look, which places it behind
    /// the window in front then: placed after the task switcher, the window
    /// would otherwise stay in front of the window chosen.
    fn look(&mut self, window: &impl LevelWindow) {
        if self.watch.is_none() {
            return;
        }
        let front = window.foreground();
        if front.is_invalid() || window.is(front) {
            return;
        }
        let lowered = window.lower(behind(front, window.is_topmost(front)));
        if window.foreground() != front {
            ask_look();
        } else if lowered {
            self.watch = None;
        }
    }

    /// Whether the window watches other programs (tests).
    #[cfg(test)]
    pub fn watching(&self) -> bool {
        self.watch.is_some()
    }
}

/// Windows' reports of other programs' focus changes (`EVENT_OBJECT_FOCUS`:
/// a window activated sets its focus after it is) and foreground changes
/// (`EVENT_SYSTEM_FOREGROUND`) to this thread, out of context, while the
/// window is topmost. Each only asks for a look ([`signal`]). Unhooked when
/// dropped, so the window thread's end unhooks them too.
#[derive(Debug)]
pub struct Watch {
    focus: HWINEVENTHOOK,
    foreground: HWINEVENTHOOK,
}

impl Watch {
    /// Hooks both events for this thread, other processes only, for the
    /// window `own`; `None` when Windows refuses either.
    fn start(own: HWND) -> Option<Self> {
        let watch = Self {
            focus: hook(EVENT_OBJECT_FOCUS),
            foreground: hook(EVENT_SYSTEM_FOREGROUND),
        };
        if watch.focus.is_invalid() || watch.foreground.is_invalid() {
            return None;
        }
        OWN.set(own.0.addr());
        Some(watch)
    }

    /// A watch that hooks nothing (tests).
    #[cfg(test)]
    pub fn fake() -> Self {
        Self {
            focus: HWINEVENTHOOK::default(),
            foreground: HWINEVENTHOOK::default(),
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        for hook in [self.focus, self.foreground] {
            if !hook.is_invalid() {
                // SAFETY: the hook was set on this thread and is unhooked
                // once.
                let _ = unsafe { UnhookWinEvent(hook) };
            }
        }
    }
}

/// Hooks `event` for this thread, out of context, other processes only;
/// invalid when Windows refuses.
fn hook(event: u32) -> HWINEVENTHOOK {
    // SAFETY: an out-of-context hook needs no module: Windows calls
    // `signalled` on this thread while it waits for messages.
    unsafe {
        SetWinEventHook(
            event,
            event,
            None,
            Some(signalled),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    }
}

/// Where a look is asked for: the window thread's event loop, which hands
/// it to [`Topmost::asked`].
type Sink = Box<dyn Fn()>;

thread_local! {
    /// Where a look is asked for on this thread ([`on_look`]).
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
    /// The watching window's handle.
    static OWN: Cell<usize> = const { Cell::new(0) };
    /// Whether a look was asked for and not yet taken from the event loop
    /// ([`Topmost::asked`]).
    static LOOK_POSTED: Cell<bool> = const { Cell::new(false) };
}

/// Sends each look the watch asks for on this thread to `sink`: the window
/// thread's event loop.
pub fn on_look(sink: impl Fn() + 'static) {
    SINK.with(|s| *s.borrow_mut() = Some(Box::new(sink)));
}

/// The watch's callback, for either event: whatever window the event
/// names, the decision is made from the foreground window at the look.
unsafe extern "system" fn signalled(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _window: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    // SAFETY: a plain query.
    signal(unsafe { GetForegroundWindow() });
}

/// Asks for a look when `front`, the foreground window now, is another
/// window than the watching one ([`ask_look`]): while the window is in
/// front (the play), the events of other programs ask for nothing.
fn signal(front: HWND) {
    if front.is_invalid() || front.0.addr() == OWN.get() {
        return;
    }
    ask_look();
}

/// Asks the sink for one look, unless one is asked for and not yet taken
/// from the event loop: one waits at a time.
fn ask_look() {
    if LOOK_POSTED.get() {
        return;
    }
    SINK.with(|s| {
        if let Some(sink) = &*s.borrow() {
            LOOK_POSTED.set(true);
            sink();
        }
    });
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

/// Places `hwnd` directly after `after` in the order of windows, neither
/// moved, sized nor activated.
///
/// # Errors
///
/// Why Windows refused.
fn place_after(hwnd: HWND, after: HWND) -> windows::core::Result<()> {
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

/// Where the window goes when it stops being topmost, `front` being the
/// foreground window: directly behind `front` when that is an ordinary
/// window, since a topmost window placed after an ordinary one stops being
/// topmost; behind a topmost one (`front_topmost`, such as the task
/// switcher) `HWND_NOTOPMOST`, the top of the ordinary windows, behind
/// every topmost one. `HWND_NOTOPMOST` with an ordinary window in front
/// would put the window in front of every ordinary window, the one in
/// front included.
fn behind(front: HWND, front_topmost: bool) -> HWND {
    if front_topmost { HWND_NOTOPMOST } else { front }
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
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GW_HWNDNEXT, GetWindow, WINDOW_EX_STYLE, WS_POPUP,
    };
    use windows::core::w;

    use super::{LevelWindow, Topmost, Watch};

    /// No window, the window's own handle, an ordinary window's and a
    /// topmost window's, as the fake knows them.
    const NONE: usize = 0;
    const OWN: usize = 1;
    const ORDINARY: usize = 2;
    const TOPMOST: usize = 3;

    fn handle(h: usize) -> HWND {
        HWND(std::ptr::without_provenance_mut(h))
    }

    /// The window as the topmost state sees it: whether it is fullscreen,
    /// hidden or refused a watch, the foreground window, the one that
    /// takes the foreground while the window is placed, whether Windows
    /// ignores its placement, and what was done to it.
    struct FakeLevel {
        fullscreen: bool,
        hidden: Cell<bool>,
        unwatchable: bool,
        front: Cell<usize>,
        front_after_lower: Cell<Option<usize>>,
        stuck: Cell<bool>,
        done: RefCell<Vec<String>>,
    }

    impl FakeLevel {
        fn fullscreen() -> Self {
            Self {
                fullscreen: true,
                ..Self::windowed()
            }
        }

        fn windowed() -> Self {
            Self {
                fullscreen: false,
                hidden: Cell::new(false),
                unwatchable: false,
                front: Cell::new(OWN),
                front_after_lower: Cell::new(None),
                stuck: Cell::new(false),
                done: RefCell::default(),
            }
        }

        fn take(&self) -> Vec<String> {
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

        fn foreground(&self) -> HWND {
            handle(self.front.get())
        }

        fn is(&self, front: HWND) -> bool {
            front == handle(OWN)
        }

        fn is_topmost(&self, front: HWND) -> bool {
            front == handle(TOPMOST)
        }

        fn watch(&self) -> Option<Watch> {
            self.done.borrow_mut().push("watch".into());
            (!self.unwatchable).then(Watch::fake)
        }

        fn raise(&self) {
            self.done.borrow_mut().push("raise".into());
        }

        fn lower(&self, after: HWND) -> bool {
            let after = if after == super::HWND_NOTOPMOST {
                "HWND_NOTOPMOST".to_owned()
            } else {
                format!("{}", after.0.addr())
            };
            self.done.borrow_mut().push(format!("after {after}"));
            if let Some(front) = self.front_after_lower.take() {
                self.front.set(front);
            }
            !self.stuck.get()
        }
    }

    /// The fullscreen window starts watching and is made topmost when it
    /// gains focus. Lost while Windows names no foreground window yet, it
    /// waits; at the next look, an ordinary window in front, it goes
    /// directly behind that window and stops watching.
    #[test]
    fn a_fullscreen_window_is_topmost_until_another_window_is_in_front() {
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        t.focus(&w, true);
        assert_eq!(w.take(), ["watch", "raise"]);
        w.front.set(NONE);
        t.focus(&w, false);
        assert_eq!(w.take(), Vec::<String>::new());
        assert!(t.watching());
        w.front.set(ORDINARY);
        t.look(&w);
        assert_eq!(w.take(), [format!("after {ORDINARY}")]);
        assert!(!t.watching());
    }

    /// Lost while another window is already in front, the window goes
    /// behind it at the focus loss itself.
    #[test]
    fn a_focus_loss_with_another_window_in_front_lowers_the_window_at_once() {
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        t.focus(&w, true);
        w.take();
        w.front.set(ORDINARY);
        t.focus(&w, false);
        assert_eq!(w.take(), [format!("after {ORDINARY}")]);
        assert!(!t.watching());
    }

    /// A topmost window in front (the task switcher, a topmost program
    /// clicked) leaves the window at the top of the ordinary windows,
    /// behind every topmost one.
    #[test]
    fn a_topmost_window_in_front_leaves_the_window_on_top_of_the_ordinary_ones() {
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        t.focus(&w, true);
        w.take();
        w.front.set(TOPMOST);
        t.look(&w);
        assert_eq!(w.take(), ["after HWND_NOTOPMOST"]);
        assert!(!t.watching());
    }

    /// A look while the window is itself in front (an event for a window
    /// that was refused the foreground) or while no window is changes
    /// nothing: the window stays topmost and watching.
    #[test]
    fn a_look_while_no_other_window_is_in_front_changes_nothing() {
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        t.focus(&w, true);
        w.take();
        t.look(&w);
        w.front.set(NONE);
        t.look(&w);
        assert_eq!(w.take(), Vec::<String>::new());
        assert!(t.watching());
    }

    /// A placement Windows ignored leaves the window topmost and watching,
    /// and the next look places it again.
    #[test]
    fn a_placement_left_topmost_is_tried_again_at_the_next_look() {
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        t.focus(&w, true);
        w.take();
        w.front.set(ORDINARY);
        w.stuck.set(true);
        t.look(&w);
        assert!(t.watching());
        w.stuck.set(false);
        t.look(&w);
        assert_eq!(
            w.take(),
            [format!("after {ORDINARY}"), format!("after {ORDINARY}")]
        );
        assert!(!t.watching());
    }

    /// Focus regained makes the window topmost again, watching again, and
    /// a gain reported while it already is raises it again, in front of
    /// any topmost window brought forward since.
    #[test]
    fn regained_focus_makes_the_window_topmost_again() {
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        t.focus(&w, true);
        w.front.set(ORDINARY);
        t.focus(&w, false);
        w.take();
        w.front.set(OWN);
        t.focus(&w, true);
        assert_eq!(w.take(), ["watch", "raise"]);
        t.focus(&w, true);
        assert_eq!(w.take(), ["raise"]);
    }

    /// A window that is not topmost is left where it is: before it was ever
    /// raised, and after it went behind one window, at the next look.
    #[test]
    fn a_window_not_topmost_is_left_where_it_is() {
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        w.front.set(ORDINARY);
        t.look(&w);
        t.focus(&w, false);
        assert_eq!(w.take(), Vec::<String>::new());
        w.front.set(OWN);
        t.focus(&w, true);
        w.front.set(ORDINARY);
        t.look(&w);
        w.take();
        w.front.set(TOPMOST);
        t.look(&w);
        t.focus(&w, false);
        assert_eq!(w.take(), Vec::<String>::new());
    }

    /// A window that cannot watch is never made topmost: nothing would end
    /// it.
    #[test]
    fn a_window_that_cannot_watch_is_not_made_topmost() {
        let w = FakeLevel {
            unwatchable: true,
            ..FakeLevel::fullscreen()
        };
        let mut t = Topmost::default();
        t.focus(&w, true);
        assert_eq!(w.take(), ["watch"]);
        assert!(!t.watching());
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
        assert_eq!(w.take(), Vec::<String>::new());
        w.hidden.set(false);
        t.focus(&w, true);
        assert_eq!(w.take(), ["watch", "raise"]);
    }

    /// A window that is not fullscreen (`--dev-window`) is never topmost,
    /// focused or not.
    #[test]
    fn a_window_that_is_not_fullscreen_is_never_topmost() {
        let w = FakeLevel::windowed();
        let mut t = Topmost::default();
        t.focus(&w, true);
        w.front.set(ORDINARY);
        t.focus(&w, false);
        t.look(&w);
        assert_eq!(w.take(), Vec::<String>::new());
    }

    /// An event asks for one look only while another window is in front:
    /// none while the window itself or no window is, and no second one
    /// until the first was taken.
    #[test]
    fn an_event_asks_for_one_look_while_another_window_is_in_front() {
        let looks = Rc::new(Cell::new(0));
        let sink = Rc::clone(&looks);
        super::on_look(move || sink.set(sink.get() + 1));
        super::OWN.set(OWN);
        super::signal(handle(OWN));
        super::signal(handle(NONE));
        assert_eq!(looks.get(), 0);
        super::signal(handle(ORDINARY));
        super::signal(handle(TOPMOST));
        assert_eq!(looks.get(), 1);
        Topmost::default().asked(None::<&FakeLevel>);
        super::signal(handle(ORDINARY));
        assert_eq!(looks.get(), 2);
    }

    /// A look asked for stays the only one until the event loop takes it:
    /// the focus loss's own look and a new watch do not let events ask for
    /// a second one; taken, the next event asks again.
    #[test]
    fn a_look_asked_for_stays_the_only_one_until_the_event_loop_takes_it() {
        let looks = Rc::new(Cell::new(0));
        let sink = Rc::clone(&looks);
        super::on_look(move || sink.set(sink.get() + 1));
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        t.focus(&w, true);
        super::OWN.set(OWN);
        super::signal(handle(ORDINARY));
        assert_eq!(looks.get(), 1);
        w.front.set(NONE);
        t.focus(&w, false);
        let window = Popup::new();
        drop(Watch::start(window.0).expect("both hooks"));
        super::signal(handle(ORDINARY));
        assert_eq!(looks.get(), 1);
        t.asked(Some(&w));
        super::signal(handle(ORDINARY));
        assert_eq!(looks.get(), 2);
    }

    /// A foreground window that changes while the window is placed (the
    /// task switcher's choice taking the foreground just after the look saw
    /// the switcher) leaves the window watching and asks for another look,
    /// which puts it directly behind the window in front then and ends the
    /// watch.
    #[test]
    fn a_foreground_changed_during_the_placement_is_looked_at_again() {
        let looks = Rc::new(Cell::new(0));
        let sink = Rc::clone(&looks);
        super::on_look(move || sink.set(sink.get() + 1));
        let w = FakeLevel::fullscreen();
        let mut t = Topmost::default();
        t.focus(&w, true);
        w.take();
        w.front.set(TOPMOST);
        w.front_after_lower.set(Some(ORDINARY));
        t.focus(&w, false);
        assert_eq!(w.take(), ["after HWND_NOTOPMOST"]);
        assert!(t.watching());
        assert_eq!(looks.get(), 1);
        t.asked(Some(&w));
        assert_eq!(w.take(), [format!("after {ORDINARY}")]);
        assert!(!t.watching());
        assert_eq!(looks.get(), 1);
    }

    /// The window goes directly behind an ordinary window in front, and to
    /// the top of the ordinary windows when a topmost window is.
    #[test]
    fn the_window_goes_behind_an_ordinary_window_in_front() {
        let front = handle(ORDINARY);
        assert_eq!(super::behind(front, false), front);
        assert_eq!(super::behind(front, true), super::HWND_NOTOPMOST);
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

    /// On real windows: placed after an ordinary window that came after
    /// it, the window lands behind that window and is not topmost. The
    /// window is ordinary here, not raised first: Windows can ignore a
    /// raise from a test process in the background ([`super::raise`]), so
    /// the test needs no topmost window. Placed only "not topmost", an
    /// ordinary window would not move at all.
    #[test]
    fn a_lowered_window_lands_behind_the_window_in_front() {
        let front = Popup::new();
        let window = Popup::new();
        assert!(front.is_behind(&window));
        assert!(!super::is_topmost(front.0));
        let after = super::behind(front.0, super::is_topmost(front.0));
        super::place_after(window.0, after).unwrap();
        assert!(window.is_behind(&front));
        assert!(!super::is_topmost(window.0));
    }

    /// On this thread Windows grants the watch's two hooks, and dropping
    /// it unhooks both.
    #[test]
    fn other_programs_can_be_watched() {
        let window = Popup::new();
        let watch = Watch::start(window.0).expect("both hooks");
        let hooks = [watch.focus, watch.foreground];
        drop(watch);
        for hook in hooks {
            // SAFETY: the hook was set on this thread; already unhooked,
            // the call fails.
            assert!(!unsafe { super::UnhookWinEvent(hook) }.as_bool());
        }
    }
}
