//! The guarded click (docs/test-device.md "Run Guard"): the host injects
//! only left clicks, each a press and its release in one `SendInput` call
//! made after it checked the point, the window and the monitor, so no
//! injected press outlives the call that made it, apart from the limits
//! the document names. The checks are UI test drivers' actionability
//! checks (the point is visible, stable and the action's hit target);
//! every input carries a 32-bit tag (`dwExtraInfo`) that Raw Input returns
//! to the recorder, so the run's own clicks are told from any other.
//!
//! The decisions are pure ([`check`], [`to_abs`]); [`click`] runs them
//! against a [`Desk`]: the real desktop, or a stand-in in tests.

use std::fmt;

/// A window, by its handle's value.
pub type Win = u64;

/// One input of a `SendInput` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// An absolute move on the virtual desktop, 0..=65535 on each axis.
    Move {
        x: u16,
        y: u16,
    },
    Down,
    Up,
}

/// The tag of call `call` of the run whose prefix is `prefix`.
#[must_use]
pub fn tag(prefix: u16, call: u16) -> u32 {
    u32::from(prefix) << 16 | u32::from(call)
}

/// What the foreground thread is doing that a click must not meet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gui {
    /// A window of another process captures the mouse.
    pub foreign_capture: bool,
    /// A menu is active.
    pub menu: bool,
}

/// What a [`Desk`] reads and does.
pub trait Desk {
    /// Where the pointer is, in screen pixels.
    fn cursor(&mut self) -> Option<(i32, i32)>;
    /// Whether any physical mouse button is down.
    fn buttons_down(&mut self) -> bool;
    /// The run's monitor, enumerated again by name: [x, y, w, h].
    fn monitor(&mut self) -> Option<[i32; 4]>;
    /// The window's client area on the screen: [x, y, w, h].
    fn client(&mut self, w: Win) -> Option<[i32; 4]>;
    /// The root window under a screen pixel.
    fn root_at(&mut self, x: i32, y: i32) -> Win;
    fn foreground(&mut self) -> Win;
    fn gui(&mut self) -> Gui;
    /// The virtual desktop: [x, y, w, h].
    fn virtual_screen(&mut self) -> [i32; 4];
    /// One `SendInput` call, every input tagged `tag`: the count inserted.
    fn send(&mut self, inputs: &[Input], tag: u32) -> u32;
    /// Records that call `tag` is about to be made, before it is.
    fn note(&mut self, tag: u32, what: &str);
    /// Waits `ms` milliseconds.
    fn sleep(&mut self, ms: u64);
}

/// What one click reads before it acts.
#[derive(Clone, Copy, Debug)]
pub struct Scene {
    /// The run's monitor at the run's start.
    pub monitor_then: [i32; 4],
    pub monitor_now: Option<[i32; 4]>,
    /// The window's client area now.
    pub client: Option<[i32; 4]>,
    /// The client point clicked.
    pub point: (i32, i32),
    /// The run's window: the click's target.
    pub window: Win,
    pub root: Win,
    pub foreground: Win,
    /// A focus click: any foreground is accepted (the click brings the
    /// window to the front); the root under the point must still be it.
    pub focus: bool,
    pub cursor: Option<(i32, i32)>,
    pub buttons_down: bool,
    pub gui: Gui,
}

/// Why a click was refused or failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    MonitorGone,
    MonitorChanged {
        then: [i32; 4],
        now: [i32; 4],
    },
    WindowGone,
    WindowOffMonitor {
        client: [i32; 4],
        monitor: [i32; 4],
    },
    PointOutside {
        point: (i32, i32),
        client: [i32; 4],
    },
    Covered {
        root: Win,
    },
    Foreground {
        foreground: Win,
    },
    CursorMoved {
        cursor: Option<(i32, i32)>,
        want: (i32, i32),
    },
    ButtonDown,
    ForeignCapture,
    Menu,
    /// `SendInput` inserted `got` of `of` inputs.
    Inserted {
        got: u32,
        of: u32,
    },
    /// A focus click's window did not come to the front.
    NotInFront,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MonitorGone => f.write_str("the run's monitor has gone"),
            Self::MonitorChanged { then, now } => {
                write!(f, "the run's monitor moved from {then:?} to {now:?}")
            }
            Self::WindowGone => f.write_str("the run's window has gone"),
            Self::WindowOffMonitor { client, monitor } => write!(
                f,
                "the window's client area {client:?} is not inside the monitor {monitor:?}"
            ),
            Self::PointOutside { point, client } => {
                write!(f, "client point {point:?} is outside {client:?}")
            }
            Self::Covered { root } => {
                write!(f, "the window under the point ({root:#x}) is not the run's")
            }
            Self::Foreground { foreground } => {
                write!(
                    f,
                    "the foreground ({foreground:#x}) is not the run's window"
                )
            }
            Self::CursorMoved { cursor, want } => {
                write!(f, "the pointer is at {cursor:?}, not {want:?}")
            }
            Self::ButtonDown => f.write_str("a mouse button is down"),
            Self::ForeignCapture => f.write_str("another process's window captures the mouse"),
            Self::Menu => f.write_str("a menu is active"),
            Self::Inserted { got, of } => write!(f, "SendInput inserted {got} of {of} inputs"),
            Self::NotInFront => f.write_str("the run's window did not come to the front"),
        }
    }
}

/// Whether the screen rectangle `inner` lies within `outer` ([x, y, w, h]).
#[must_use]
pub const fn contains(outer: [i32; 4], inner: [i32; 4]) -> bool {
    let [ox, oy, ow, oh] = outer;
    let [ix, iy, iw, ih] = inner;
    ix >= ox && iy >= oy && ix + iw <= ox + ow && iy + ih <= oy + oh
}

/// The screen pixel of a click, or why it must not happen: the monitor is
/// the one the run started on, the window lies inside it and the point
/// inside the window's client area, the root under the pixel and the
/// foreground are the run's window (for a focus click any foreground), the
/// pointer is at the pixel, no button is down, and no other process
/// captures the mouse and no menu is active.
///
/// # Errors
///
/// The first rule the scene breaks.
pub fn check(s: &Scene) -> Result<(i32, i32), Refusal> {
    let monitor = s.monitor_now.ok_or(Refusal::MonitorGone)?;
    if monitor != s.monitor_then {
        return Err(Refusal::MonitorChanged {
            then: s.monitor_then,
            now: monitor,
        });
    }
    let client = s.client.ok_or(Refusal::WindowGone)?;
    if !contains(monitor, client) {
        return Err(Refusal::WindowOffMonitor { client, monitor });
    }
    let [l, t, w, h] = client;
    let (x, y) = s.point;
    if !(0..w).contains(&x) || !(0..h).contains(&y) {
        return Err(Refusal::PointOutside {
            point: s.point,
            client,
        });
    }
    if s.root == 0 || s.root != s.window {
        return Err(Refusal::Covered { root: s.root });
    }
    if !s.focus && s.foreground != s.window {
        return Err(Refusal::Foreground {
            foreground: s.foreground,
        });
    }
    let want = (l + x, t + y);
    if s.cursor != Some(want) {
        return Err(Refusal::CursorMoved {
            cursor: s.cursor,
            want,
        });
    }
    if s.buttons_down {
        return Err(Refusal::ButtonDown);
    }
    if s.gui.foreign_capture {
        return Err(Refusal::ForeignCapture);
    }
    if s.gui.menu {
        return Err(Refusal::Menu);
    }
    Ok(want)
}

/// The absolute coordinate (0..=65535) of pixel `p` on an axis of the
/// virtual desktop that starts at `origin` and is `size` pixels long: the
/// least value that the system's mapping, `origin + a·size / 65536`
/// rounded down, takes to `p`.
#[must_use]
pub fn to_abs(p: i32, origin: i32, size: i32) -> u16 {
    let k = u64::try_from(p - origin).unwrap_or(0);
    let size = u64::try_from(size).unwrap_or(1).max(1);
    let a = (k * 65536).div_ceil(size);
    u16::try_from(a.min(65535)).unwrap_or(u16::MAX)
}

/// The pixel the system's mapping takes absolute coordinate `a` to.
#[must_use]
pub fn from_abs(a: u16, origin: i32, size: i32) -> i32 {
    origin + i32::try_from(i64::from(a) * i64::from(size) / 65536).unwrap_or(0)
}

/// How long a focus click waits for its window to come to the front, and
/// how often it looks.
const FOCUS_WAIT_MS: u64 = 1000;
const FOCUS_LOOK_MS: u64 = 10;

/// One guarded click of the run's `window` at client `point`, as call
/// `call` of the run whose tag prefix is `prefix` (two calls: the move,
/// then the click). `monitor_then` is the run's monitor at its start.
///
/// # Errors
///
/// A [`Refusal`] before the click (nothing pressed), or after it (the
/// click landed but cannot be vouched for, or a partial insert whose
/// corrective release was sent).
pub fn click<D: Desk>(
    desk: &mut D,
    window: Win,
    point: (i32, i32),
    focus: bool,
    monitor_then: [i32; 4],
    prefix: u16,
    call: &mut u16,
) -> Result<(), Refusal> {
    let client = desk.client(window).ok_or(Refusal::WindowGone)?;
    let pixel = (client[0] + point.0, client[1] + point.1);
    let [vx, vy, vw, vh] = desk.virtual_screen();
    let to = Input::Move {
        x: to_abs(pixel.0, vx, vw),
        y: to_abs(pixel.1, vy, vh),
    };
    // The move alone.
    let t = tag(prefix, *call);
    *call = call.wrapping_add(1);
    desk.note(t, "move");
    let got = desk.send(&[to], t);
    if got != 1 {
        return Err(Refusal::Inserted { got, of: 1 });
    }
    let scene = Scene {
        monitor_then,
        monitor_now: desk.monitor(),
        client: desk.client(window),
        point,
        window,
        root: desk.root_at(pixel.0, pixel.1),
        foreground: desk.foreground(),
        focus,
        cursor: desk.cursor(),
        buttons_down: desk.buttons_down(),
        gui: desk.gui(),
    };
    check(&scene)?;
    // The click: the move pins the press to the checked pixel.
    let t = tag(prefix, *call);
    *call = call.wrapping_add(1);
    desk.note(t, "click");
    let got = desk.send(&[to, Input::Down, Input::Up], t);
    match got {
        3 => {}
        2 => {
            // The down went in without its up: release it at once,
            // before anything else.
            let _ = desk.send(&[Input::Up], t);
            return Err(Refusal::Inserted { got, of: 3 });
        }
        _ => return Err(Refusal::Inserted { got, of: 3 }),
    }
    // Post-check.
    let cursor = desk.cursor();
    if cursor != Some(pixel) {
        return Err(Refusal::CursorMoved {
            cursor,
            want: pixel,
        });
    }
    let root = desk.root_at(pixel.0, pixel.1);
    if root != window {
        return Err(Refusal::Covered { root });
    }
    if focus {
        // The window's thread activates it once it takes the click.
        let mut waited = 0;
        while desk.foreground() != window {
            if waited >= FOCUS_WAIT_MS {
                return Err(Refusal::NotInFront);
            }
            desk.sleep(FOCUS_LOOK_MS);
            waited += FOCUS_LOOK_MS;
        }
    } else {
        let foreground = desk.foreground();
        if foreground != window {
            return Err(Refusal::Foreground { foreground });
        }
    }
    Ok(())
}

/// A stand-in desktop for the tests: it records every call in order.
#[cfg(test)]
pub mod fake {
    use super::{Desk, Gui, Input, Win};

    /// What a [`Desk`] call did, in order.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Did {
        Note(u32, String),
        Send(Vec<Input>, u32),
        Sleep(u64),
    }

    #[derive(Clone, Debug)]
    pub struct Fake {
        pub monitor: Option<[i32; 4]>,
        pub client: Option<[i32; 4]>,
        pub window: Win,
        /// The root under the point, before and after the click.
        pub root: Win,
        pub root_after: Win,
        pub foreground: Win,
        /// The foreground after this many looks once clicked.
        pub front_after: Option<u32>,
        pub cursor: Option<(i32, i32)>,
        pub cursor_after: Option<(i32, i32)>,
        pub buttons_down: bool,
        pub gui: Gui,
        pub screen: [i32; 4],
        /// What each `send` inserts, in turn; a full count after.
        pub inserts: Vec<u32>,
        pub did: Vec<Did>,
        clicked: bool,
        looks: u32,
    }

    impl Fake {
        /// A 1920×1080 window at the monitor's top left, its pointer at
        /// the pixel the test clicks: client point (300, 657).
        pub fn new() -> Self {
            Self {
                monitor: Some([0, 0, 2560, 1440]),
                client: Some([0, 0, 1920, 1080]),
                window: 7,
                root: 7,
                root_after: 7,
                foreground: 7,
                front_after: None,
                cursor: Some((300, 657)),
                cursor_after: Some((300, 657)),
                buttons_down: false,
                gui: Gui::default(),
                screen: [0, 0, 2560, 1440],
                inserts: Vec::new(),
                did: Vec::new(),
                clicked: false,
                looks: 0,
            }
        }

        pub fn sends(&self) -> Vec<(Vec<Input>, u32)> {
            self.did
                .iter()
                .filter_map(|d| match d {
                    Did::Send(i, t) => Some((i.clone(), *t)),
                    _ => None,
                })
                .collect()
        }
    }

    impl Desk for Fake {
        fn cursor(&mut self) -> Option<(i32, i32)> {
            if self.clicked {
                self.cursor_after
            } else {
                self.cursor
            }
        }
        fn buttons_down(&mut self) -> bool {
            self.buttons_down
        }
        fn monitor(&mut self) -> Option<[i32; 4]> {
            self.monitor
        }
        fn client(&mut self, _: Win) -> Option<[i32; 4]> {
            self.client
        }
        fn root_at(&mut self, _: i32, _: i32) -> Win {
            if self.clicked {
                self.root_after
            } else {
                self.root
            }
        }
        fn foreground(&mut self) -> Win {
            if self.clicked
                && let Some(n) = self.front_after
            {
                self.looks += 1;
                if self.looks > n {
                    return self.window;
                }
            }
            self.foreground
        }
        fn gui(&mut self) -> Gui {
            self.gui
        }
        fn virtual_screen(&mut self) -> [i32; 4] {
            self.screen
        }
        fn send(&mut self, inputs: &[Input], tag: u32) -> u32 {
            self.did.push(Did::Send(inputs.to_vec(), tag));
            let n = if self.inserts.is_empty() {
                u32::try_from(inputs.len()).unwrap_or(0)
            } else {
                self.inserts.remove(0)
            };
            if inputs.contains(&Input::Down) && n >= 2 {
                self.clicked = true;
            }
            n
        }
        fn note(&mut self, tag: u32, what: &str) {
            self.did.push(Did::Note(tag, what.to_owned()));
        }
        fn sleep(&mut self, ms: u64) {
            self.did.push(Did::Sleep(ms));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Did, Fake};
    use super::*;

    const MONITOR: [i32; 4] = [0, 0, 2560, 1440];
    const POINT: (i32, i32) = (300, 657);

    fn run(f: &mut Fake, focus: bool) -> Result<(), Refusal> {
        let mut call = 5;
        click(f, 7, POINT, focus, MONITOR, 0xBEEF, &mut call)
    }

    #[test]
    fn a_click_lands_at_its_client_point_when_every_rule_holds() {
        let mut f = Fake::new();
        run(&mut f, false).unwrap();
        let sends = f.sends();
        let to = Input::Move {
            x: to_abs(300, 0, 2560),
            y: to_abs(657, 0, 1440),
        };
        assert_eq!(
            sends,
            [
                (vec![to], tag(0xBEEF, 5)),
                (vec![to, Input::Down, Input::Up], tag(0xBEEF, 6)),
            ]
        );
    }

    /// A partial insert (the move and the down) is followed by the
    /// corrective up at once, before any other call, record or read.
    #[test]
    fn a_partial_insert_is_followed_by_its_up_before_anything_else() {
        let mut f = Fake::new();
        f.inserts = vec![1, 2];
        assert_eq!(run(&mut f, false), Err(Refusal::Inserted { got: 2, of: 3 }));
        let at = f
            .did
            .iter()
            .position(|d| matches!(d, Did::Send(i, _) if i.contains(&Input::Down)))
            .unwrap();
        assert_eq!(
            f.did.get(at + 1),
            Some(&Did::Send(vec![Input::Up], tag(0xBEEF, 6)))
        );
        assert_eq!(f.did.len(), at + 2, "{:?}", f.did);
    }

    /// Anything but the full count fails the click; a count of 0 or 1
    /// sends no corrective up (no down went in).
    #[test]
    fn a_click_needs_its_full_count() {
        for got in [0, 1] {
            let mut f = Fake::new();
            f.inserts = vec![1, got];
            assert_eq!(run(&mut f, false), Err(Refusal::Inserted { got, of: 3 }));
            assert_eq!(f.sends().len(), 2, "{:?}", f.did);
        }
        let mut f = Fake::new();
        f.inserts = vec![0];
        assert_eq!(run(&mut f, false), Err(Refusal::Inserted { got: 0, of: 1 }));
        assert_eq!(f.sends().len(), 1);
    }

    /// After the call: the pointer still at the pixel, the root under it
    /// and the foreground still the run's window.
    #[test]
    fn the_post_check_needs_the_pixel_the_root_and_the_foreground() {
        let mut f = Fake::new();
        f.cursor_after = Some((301, 657));
        assert!(matches!(
            run(&mut f, false),
            Err(Refusal::CursorMoved { .. })
        ));
        let mut f = Fake::new();
        f.root_after = 9;
        assert_eq!(run(&mut f, false), Err(Refusal::Covered { root: 9 }));
        struct After(Fake);
        impl Desk for After {
            fn cursor(&mut self) -> Option<(i32, i32)> {
                self.0.cursor()
            }
            fn buttons_down(&mut self) -> bool {
                self.0.buttons_down()
            }
            fn monitor(&mut self) -> Option<[i32; 4]> {
                self.0.monitor()
            }
            fn client(&mut self, w: Win) -> Option<[i32; 4]> {
                self.0.client(w)
            }
            fn root_at(&mut self, x: i32, y: i32) -> Win {
                self.0.root_at(x, y)
            }
            fn foreground(&mut self) -> Win {
                let clicked = self.0.sends().len() > 1;
                if clicked { 9 } else { 7 }
            }
            fn gui(&mut self) -> Gui {
                self.0.gui()
            }
            fn virtual_screen(&mut self) -> [i32; 4] {
                self.0.virtual_screen()
            }
            fn send(&mut self, inputs: &[Input], tag: u32) -> u32 {
                self.0.send(inputs, tag)
            }
            fn note(&mut self, tag: u32, what: &str) {
                self.0.note(tag, what);
            }
            fn sleep(&mut self, ms: u64) {
                self.0.sleep(ms);
            }
        }
        let mut d = After(Fake::new());
        let mut call = 0;
        assert_eq!(
            click(&mut d, 7, POINT, false, MONITOR, 1, &mut call),
            Err(Refusal::Foreground { foreground: 9 })
        );
    }

    /// A focus click accepts any foreground before it, then waits for its
    /// window to come to the front, looking every 10 ms for up to 1 s.
    #[test]
    fn a_focus_click_awaits_the_foreground() {
        let mut f = Fake::new();
        f.foreground = 3;
        f.front_after = Some(4);
        run(&mut f, true).unwrap();
        let sleeps = f.did.iter().filter(|d| matches!(d, Did::Sleep(10))).count();
        assert_eq!(sleeps, 4);
        let mut f = Fake::new();
        f.foreground = 3;
        f.front_after = Some(1000);
        assert_eq!(run(&mut f, true), Err(Refusal::NotInFront));
        // Not a focus click: another foreground refuses it before any
        // button.
        let mut f = Fake::new();
        f.foreground = 3;
        assert_eq!(
            run(&mut f, false),
            Err(Refusal::Foreground { foreground: 3 })
        );
        assert_eq!(f.sends().len(), 1);
    }

    #[test]
    fn no_click_while_a_button_is_down() {
        let mut f = Fake::new();
        f.buttons_down = true;
        assert_eq!(run(&mut f, false), Err(Refusal::ButtonDown));
        assert_eq!(f.sends().len(), 1, "only the move went in");
    }

    #[test]
    fn no_click_under_a_foreign_capture_or_a_menu() {
        let mut f = Fake::new();
        f.gui.foreign_capture = true;
        assert_eq!(run(&mut f, false), Err(Refusal::ForeignCapture));
        let mut f = Fake::new();
        f.gui.menu = true;
        assert_eq!(run(&mut f, false), Err(Refusal::Menu));
        assert_eq!(f.sends().len(), 1);
    }

    /// Every input of every call carries its call's tag, recorded before
    /// the call.
    #[test]
    fn every_input_carries_its_call_s_tag() {
        let mut f = Fake::new();
        run(&mut f, false).unwrap();
        let mut noted = None;
        for d in &f.did {
            match d {
                Did::Note(t, _) => noted = Some(*t),
                Did::Send(_, t) => assert_eq!(Some(*t), noted, "{:?}", f.did),
                Did::Sleep(_) => {}
            }
        }
        assert_eq!(tag(0x8000, 0xFFFF), 0x8000_FFFF);
        assert_eq!(tag(0, 1), 1);
    }

    /// The runner's inverse of the system's absolute mapping hits every
    /// pixel of each axis of common sizes, the virtual desktop's origin
    /// off zero included. (Only this inverse: the system's own mapping is
    /// verified on the desktop by the move's read back.)
    #[test]
    fn every_pixel_maps_to_a_coordinate_that_maps_back() {
        for (origin, size) in [
            (0, 1920),
            (0, 2560),
            (-13, 4480),
            (-1920, 3840),
            (0, 1080),
            (0, 1),
        ] {
            for p in origin..origin + size {
                let a = to_abs(p, origin, size);
                assert_eq!(from_abs(a, origin, size), p, "{origin} {size} {p}");
            }
        }
    }

    fn scene() -> Scene {
        Scene {
            monitor_then: MONITOR,
            monitor_now: Some(MONITOR),
            client: Some([0, 0, 1920, 1080]),
            point: POINT,
            window: 7,
            root: 7,
            foreground: 7,
            focus: false,
            cursor: Some(POINT),
            buttons_down: false,
            gui: Gui::default(),
        }
    }

    /// Each rule of [`check`], broken alone, refuses the click.
    #[test]
    fn each_broken_rule_refuses_the_click() {
        assert_eq!(check(&scene()), Ok(POINT));
        let cases: [(fn(&mut Scene), fn(&Refusal) -> bool); 10] = [
            (|s| s.monitor_now = None, |r| *r == Refusal::MonitorGone),
            (
                |s| s.monitor_now = Some([0, 0, 1920, 1080]),
                |r| matches!(r, Refusal::MonitorChanged { .. }),
            ),
            (|s| s.client = None, |r| *r == Refusal::WindowGone),
            (
                |s| s.client = Some([2000, 0, 1920, 1080]),
                |r| matches!(r, Refusal::WindowOffMonitor { .. }),
            ),
            (
                |s| s.point = (1920, 5),
                |r| matches!(r, Refusal::PointOutside { .. }),
            ),
            (|s| s.root = 9, |r| *r == Refusal::Covered { root: 9 }),
            (
                |s| s.foreground = 9,
                |r| *r == Refusal::Foreground { foreground: 9 },
            ),
            (
                |s| s.cursor = Some((301, 657)),
                |r| matches!(r, Refusal::CursorMoved { .. }),
            ),
            (|s| s.buttons_down = true, |r| *r == Refusal::ButtonDown),
            (|s| s.gui.menu = true, |r| *r == Refusal::Menu),
        ];
        for (i, (spoil, want)) in cases.iter().enumerate() {
            let mut s = scene();
            spoil(&mut s);
            let got = check(&s);
            assert!(got.as_ref().is_err_and(want), "case {i}: {got:?}");
        }
        // A focus click takes any foreground, never another root.
        let mut s = scene();
        s.focus = true;
        s.foreground = 9;
        assert_eq!(check(&s), Ok(POINT));
        s.root = 9;
        assert_eq!(check(&s), Err(Refusal::Covered { root: 9 }));
    }
}
