//! The desktop the runs act on: DPI awareness, the mouse buttons'
//! orientation, monitors, windows, the foreground and its thread, and
//! tagged `SendInput` calls: the real [`Desk`] of the guarded click.

use anyhow::{Context as _, bail};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, ClientToScreen, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
    GetDIBits, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_PER_MONITOR_AWARE,
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput,
    VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOT, GUI_INMENUMODE, GUI_POPUPMENUMODE, GUI_SYSTEMMENUMODE, GUITHREADINFO,
    GW_HWNDPREV, GetAncestor, GetClassNameW, GetClientRect, GetCursorPos, GetForegroundWindow,
    GetGUIThreadInfo, GetSystemMetrics, GetWindow, GetWindowRect, GetWindowThreadProcessId,
    IsWindowVisible, PostMessageW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_SWAPBUTTON,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetForegroundWindow, WM_CLOSE, WindowFromPoint,
};
use windows::core::BOOL;

use super::click::{Desk, Gui, Input, Win};
use super::win::until_nul;

/// Makes the process per-monitor DPI aware (v2), so every coordinate is a
/// physical pixel, and checks that this thread reads that awareness back:
/// a scaled coordinate would put a click elsewhere.
///
/// # Errors
///
/// The thread is not per-monitor aware.
pub fn dpi_aware() -> anyhow::Result<()> {
    // SAFETY: no pointers. It fails when the awareness is already set;
    // the read below decides.
    let set = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    // SAFETY: no arguments; the context is the thread's own.
    let awareness = unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) };
    if awareness != DPI_AWARENESS_PER_MONITOR_AWARE {
        bail!(
            "this thread is not per-monitor DPI aware (awareness {}; setting it: {set:?})",
            awareness.0
        );
    }
    Ok(())
}

/// Refuses a desktop whose mouse buttons are swapped: the physical left
/// button the runner reads would be another than the logical one it
/// sends.
///
/// # Errors
///
/// `swapped`: the buttons are swapped.
pub fn unswapped(swapped: bool) -> anyhow::Result<()> {
    if swapped {
        bail!("the mouse buttons are swapped: the runner refuses to click");
    }
    Ok(())
}

/// Whether the system swaps the mouse buttons.
pub fn swapped() -> bool {
    // SAFETY: no pointers.
    unsafe { GetSystemMetrics(SM_SWAPBUTTON) != 0 }
}

/// A monitor: its device name and its area [x, y, w, h].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub name: String,
    pub area: [i32; 4],
}

/// Every monitor of the desktop.
pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the Vec passed below, alive for the call.
        let out = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = u32::try_from(size_of::<MONITORINFOEXW>()).unwrap_or(0);
        // SAFETY: the struct's size is set.
        if unsafe { GetMonitorInfoW(m, (&raw mut info).cast()) }.as_bool() {
            let r = info.monitorInfo.rcMonitor;
            out.push(Monitor {
                name: until_nul(&info.szDevice),
                area: [r.left, r.top, r.right - r.left, r.bottom - r.top],
            });
        }
        BOOL(1)
    }
    let mut out: Vec<Monitor> = Vec::new();
    // SAFETY: the callback reads `out` only during the call.
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(each), LPARAM((&raw mut out) as isize)) };
    out
}

/// The monitor named `name`.
///
/// # Errors
///
/// No such monitor.
pub fn monitor(name: &str) -> anyhow::Result<Monitor> {
    let all = monitors();
    all.iter()
        .find(|m| m.name.eq_ignore_ascii_case(name))
        .cloned()
        .with_context(|| format!("no monitor {name}; there are {all:?}"))
}

/// `w` as a window handle.
pub fn hwnd(w: Win) -> HWND {
    HWND(w as usize as *mut core::ffi::c_void)
}

/// `h`'s value.
pub fn win(h: HWND) -> Win {
    h.0 as usize as u64
}

/// The client area of `w` on the screen: [x, y, w, h].
pub fn client_rect(w: Win) -> Option<[i32; 4]> {
    let mut r = RECT::default();
    // SAFETY: one RECT written.
    unsafe { GetClientRect(hwnd(w), &mut r) }.ok()?;
    let mut origin = POINT::default();
    // SAFETY: one POINT updated.
    if !unsafe { ClientToScreen(hwnd(w), &mut origin) }.as_bool() {
        return None;
    }
    Some([origin.x, origin.y, r.right - r.left, r.bottom - r.top])
}

/// The foreground window.
pub fn foreground() -> Win {
    // SAFETY: no arguments.
    win(unsafe { GetForegroundWindow() })
}

/// The process of window `w`.
pub fn process_of(w: Win) -> u32 {
    let mut pid = 0;
    // SAFETY: one u32 written.
    unsafe { GetWindowThreadProcessId(hwnd(w), Some(&raw mut pid)) };
    pid
}

/// The visible top-level windows of process `pid`.
pub fn windows_of(pid: u32) -> Vec<Win> {
    struct Find {
        pid: u32,
        out: Vec<Win>,
    }
    unsafe extern "system" fn each(h: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the Find passed below, alive for the call.
        let f = unsafe { &mut *(data.0 as *mut Find) };
        // SAFETY: a window handle the system gave.
        if process_of(win(h)) == f.pid && unsafe { IsWindowVisible(h) }.as_bool() {
            f.out.push(win(h));
        }
        BOOL(1)
    }
    let mut f = Find {
        pid,
        out: Vec::new(),
    };
    // SAFETY: the callback reads `f` only during the call.
    let _ = unsafe { EnumWindows(Some(each), LPARAM((&raw mut f) as isize)) };
    f.out
}

/// The visible top-level window of `pid` with the largest client area, if
/// it has one: a process can show other windows, of no size, before (or
/// beside) its own.
pub fn main_window_of(pid: u32) -> Option<Win> {
    windows_of(pid)
        .into_iter()
        .filter_map(|w| {
            let [_, _, cw, ch] = client_rect(w)?;
            (cw > 0 && ch > 0).then_some((i64::from(cw) * i64::from(ch), w))
        })
        .max_by_key(|&(area, _)| area)
        .map(|(_, w)| w)
}

/// How long [`bring_to_front`] waits for the window to come in front.
const FRONT_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Asks for window `w` in front (`SetForegroundWindow`) and waits up to
/// [`FRONT_WAIT`], looking every 10 ms, for the foreground to be `w`:
/// Windows may refuse a process that is not in front.
///
/// # Errors
///
/// Another window stayed in front.
pub fn bring_to_front(w: Win) -> Result<(), String> {
    // SAFETY: no pointers. Its result is not the evidence; the
    // foreground read below is.
    let _ = unsafe { SetForegroundWindow(hwnd(w)) };
    let end = std::time::Instant::now() + FRONT_WAIT;
    loop {
        let fg = foreground();
        if fg == w {
            return Ok(());
        }
        if std::time::Instant::now() >= end {
            return Err(format!(
                "window {fg:#x} of process {} stayed in front for {FRONT_WAIT:?}",
                process_of(fg)
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Asks window `w` to close (`WM_CLOSE`), as its close button would.
///
/// # Errors
///
/// The message cannot be posted.
pub fn close(w: Win) -> anyhow::Result<()> {
    // SAFETY: no pointers.
    unsafe { PostMessageW(Some(hwnd(w)), WM_CLOSE, WPARAM(0), LPARAM(0)) }
        .context("PostMessage(WM_CLOSE)")
}

/// A top-level window above the run's window in the z-order, as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Above {
    pub window: Win,
    pub pid: u32,
    pub class: String,
    pub visible: bool,
    /// DWM does not draw it (another virtual desktop, a suspended app).
    pub cloaked: bool,
    /// Its drawn bounds on the screen [x, y, w, h].
    pub rect: [i32; 4],
}

/// Whether two rectangles [x, y, w, h] share a pixel.
pub const fn meet(a: [i32; 4], b: [i32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

/// The windows of `above` (the z-order above the run's window, nearest
/// first) that may draw over `area` on the screen: each visible and not
/// cloaked with its bounds meeting it, described. Any is a doubt: a screen
/// copy of `area` then need not be the run's window's pixels.
pub fn covering_of(above: &[Above], area: [i32; 4]) -> Vec<String> {
    above
        .iter()
        .filter(|a| a.visible && !a.cloaked && meet(a.rect, area))
        .map(|a| {
            format!(
                "window {:#x} ({}) of process {} at {:?} is above the run's window over {area:?}",
                a.window, a.class, a.pid, a.rect
            )
        })
        .collect()
}

/// The top-level windows above `w` in the z-order, nearest first, read now
/// (at most 4096: the chain ends there however the z-order changes during
/// the walk).
pub fn above(w: Win) -> Vec<Above> {
    let mut out = Vec::new();
    // SAFETY: a window handle; GetWindow reads the z-order.
    let mut h = unsafe { GetWindow(hwnd(w), GW_HWNDPREV) }.ok();
    while let Some(next) = h
        && out.len() < 4096
    {
        let mut cloaked = 0_u32;
        // SAFETY: one u32 written, its size given.
        let cloak = unsafe {
            DwmGetWindowAttribute(
                next,
                DWMWA_CLOAKED,
                (&raw mut cloaked).cast(),
                u32::try_from(size_of::<u32>()).unwrap_or(0),
            )
        };
        let mut r = RECT::default();
        // SAFETY: one RECT written, its size given.
        let bounds = unsafe {
            DwmGetWindowAttribute(
                next,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&raw mut r).cast(),
                u32::try_from(size_of::<RECT>()).unwrap_or(0),
            )
        };
        if bounds.is_err() {
            // SAFETY: one RECT written.
            let _ = unsafe { GetWindowRect(next, &mut r) };
        }
        let mut class = [0_u16; 128];
        // SAFETY: the buffer's length is given.
        let n = unsafe { GetClassNameW(next, &mut class) };
        out.push(Above {
            window: win(next),
            pid: process_of(win(next)),
            class: String::from_utf16_lossy(&class[..usize::try_from(n).unwrap_or(0)]),
            // SAFETY: a window handle.
            visible: unsafe { IsWindowVisible(next) }.as_bool(),
            // A window whose cloak cannot be read counts as drawn.
            cloaked: cloak.is_ok() && cloaked != 0,
            rect: [r.left, r.top, r.right - r.left, r.bottom - r.top],
        });
        // SAFETY: as above.
        h = unsafe { GetWindow(next, GW_HWNDPREV) }.ok();
    }
    out
}

/// Why a screen copy of `client` (the run's window `w`'s client area, as
/// read before the copy) might not be `w`'s pixels, read now: `w` not in
/// front, its client area moved, or a window above it over that area. None
/// is the evidence that the area is `w`'s.
pub fn doubts(w: Win, client: [i32; 4]) -> Vec<String> {
    let mut out = Vec::new();
    let fg = foreground();
    if fg != w {
        out.push(format!(
            "window {fg:#x} of process {} is in front, not the run's window",
            process_of(fg)
        ));
    }
    let now = client_rect(w);
    if now != Some(client) {
        out.push(format!(
            "the run's window's client area is {now:?}, not {client:?}"
        ));
    }
    out.extend(covering_of(&above(w), client));
    out
}

/// The screen's pixels in `rect` ([x, y, w, h]) by GDI `BitBlt` from the
/// screen's DC, as 32-bit BGRA rows from the top.
///
/// # Errors
///
/// An empty rectangle, or a GDI call failed.
pub fn capture(rect: [i32; 4]) -> anyhow::Result<Vec<u8>> {
    let [x, y, w, h] = rect;
    if w <= 0 || h <= 0 {
        bail!("an empty rectangle {rect:?}");
    }
    // SAFETY: the screen's DC, released below.
    let screen = unsafe { GetDC(None) };
    // SAFETY: a valid DC.
    let mem = unsafe { CreateCompatibleDC(Some(screen)) };
    // SAFETY: as above.
    let bitmap = unsafe { CreateCompatibleBitmap(screen, w, h) };
    // SAFETY: both handles were just made.
    let old = unsafe { SelectObject(mem, bitmap.into()) };
    // SAFETY: both DCs are valid; the bitmap is `w`×`h`.
    let blit = unsafe { BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY) };
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(0),
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    let mut pixels =
        vec![0_u8; usize::try_from(w).unwrap_or(0) * usize::try_from(h).unwrap_or(0) * 4];
    // SAFETY: the bitmap is deselected first; the buffer holds `h` rows of
    // `w` 32-bit pixels.
    let rows = unsafe {
        SelectObject(mem, old);
        GetDIBits(
            mem,
            bitmap,
            0,
            u32::try_from(h).unwrap_or(0),
            Some(pixels.as_mut_ptr().cast::<core::ffi::c_void>()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    // SAFETY: each handle was made above and is released once.
    unsafe {
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
    blit.context("BitBlt")?;
    if rows != h {
        bail!("GetDIBits read {rows} of {h} rows");
    }
    Ok(pixels)
}

/// The mouse input of one [`Input`], tagged.
fn mouse(input: Input, tag: u32) -> INPUT {
    let (dx, dy, flags) = match input {
        Input::Move { x, y } => (
            i32::from(x),
            i32::from(y),
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        ),
        Input::Down => (0, 0, MOUSEEVENTF_LEFTDOWN),
        Input::Up => (0, 0, MOUSEEVENTF_LEFTUP),
    };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                dwFlags: flags,
                dwExtraInfo: tag as usize,
                ..MOUSEINPUT::default()
            },
        },
    }
}

/// Records a call (its tag, what it sends and the client point it aims
/// at) before it is made.
pub type Note<'a> = dyn FnMut(u32, &str, (i32, i32)) + 'a;

/// The real desktop, for the run's monitor `monitor` (by name).
pub struct Real<'a> {
    pub monitor: String,
    /// Records a call before it is made (the run's `call` record).
    pub note: &'a mut Note<'a>,
}

impl Desk for Real<'_> {
    fn cursor(&mut self) -> Option<(i32, i32)> {
        let mut p = POINT::default();
        // SAFETY: one POINT written.
        unsafe { GetCursorPos(&mut p) }.ok()?;
        Some((p.x, p.y))
    }

    fn buttons_down(&mut self) -> bool {
        // The physical buttons, the left one as the logical left (the
        // runner refuses swapped buttons).
        [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
            .into_iter()
            // SAFETY: no pointers.
            .any(|vk| unsafe { GetAsyncKeyState(i32::from(vk.0)) } < 0)
    }

    fn monitor(&mut self) -> Option<[i32; 4]> {
        monitor(&self.monitor).ok().map(|m| m.area)
    }

    fn client(&mut self, w: Win) -> Option<[i32; 4]> {
        client_rect(w)
    }

    fn root_at(&mut self, x: i32, y: i32) -> Win {
        // SAFETY: no pointers.
        let h = unsafe { WindowFromPoint(POINT { x, y }) };
        // SAFETY: a window handle the system gave, or null.
        win(unsafe { GetAncestor(h, GA_ROOT) })
    }

    fn foreground(&mut self) -> Win {
        foreground()
    }

    fn gui(&mut self) -> Gui {
        let mut info = GUITHREADINFO {
            cbSize: u32::try_from(size_of::<GUITHREADINFO>()).unwrap_or(0),
            ..GUITHREADINFO::default()
        };
        // The foreground thread's state (thread 0).
        // SAFETY: the struct's size is set.
        if unsafe { GetGUIThreadInfo(0, &mut info) }.is_err() {
            // Unreadable: refuse as if a menu were up.
            return Gui {
                foreign_capture: false,
                menu: true,
            };
        }
        let capture = win(info.hwndCapture);
        Gui {
            foreign_capture: capture != 0 && process_of(capture) != std::process::id(),
            menu: info.flags.0 & (GUI_INMENUMODE.0 | GUI_POPUPMENUMODE.0 | GUI_SYSTEMMENUMODE.0)
                != 0,
        }
    }

    fn virtual_screen(&mut self) -> [i32; 4] {
        // SAFETY: no pointers.
        unsafe {
            [
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            ]
        }
    }

    fn send(&mut self, inputs: &[Input], tag: u32) -> u32 {
        let batch: Vec<INPUT> = inputs.iter().map(|&i| mouse(i, tag)).collect();
        // SAFETY: initialised inputs of the given size.
        unsafe { SendInput(&batch, i32::try_from(size_of::<INPUT>()).unwrap_or(0)) }
    }

    fn note(&mut self, tag: u32, what: &str, point: (i32, i32)) {
        (self.note)(tag, what, point);
    }

    fn sleep(&mut self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn above_of(window: Win, visible: bool, cloaked: bool, rect: [i32; 4]) -> Above {
        Above {
            window,
            pid: 7,
            class: "C".into(),
            visible,
            cloaked,
            rect,
        }
    }

    /// A window above the run's window that is drawn and meets the area is
    /// a doubt; one that is hidden, cloaked or beside the area is not. The
    /// area's edges are exclusive on the right and bottom.
    #[test]
    fn a_drawn_window_over_the_area_is_a_doubt() {
        let area = [0, 0, 1920, 1080];
        assert_eq!(
            covering_of(&[above_of(1, true, false, [100, 100, 300, 80])], area).len(),
            1
        );
        for a in [
            above_of(2, false, false, [100, 100, 300, 80]),
            above_of(3, true, true, [100, 100, 300, 80]),
            above_of(4, true, false, [1920, 0, 300, 80]),
            above_of(5, true, false, [0, 1080, 300, 80]),
            above_of(6, true, false, [-300, -80, 300, 80]),
        ] {
            assert!(
                covering_of(std::slice::from_ref(&a), area).is_empty(),
                "{a:?}"
            );
        }
        // One pixel in.
        assert_eq!(
            covering_of(&[above_of(7, true, false, [1919, 1079, 5, 5])], area).len(),
            1
        );
    }

    /// The real z-order: a topmost window that does not activate, laid over
    /// a window of this test, is read above it and is a doubt over the
    /// area it covers, not over an area beside it.
    #[test]
    fn a_window_laid_over_another_is_read_above_it() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
            WS_POPUP, WS_VISIBLE,
        };
        use windows::core::w;
        let make = |ex, x, y| {
            // SAFETY: a predefined class; no parent; the handle is
            // destroyed below.
            unsafe {
                CreateWindowExW(
                    ex,
                    w!("STATIC"),
                    w!(""),
                    WS_POPUP | WS_VISIBLE,
                    x,
                    y,
                    200,
                    100,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap()
        };
        let under = make(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, 40, 40);
        let over = make(WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, 140, 90);
        let read = above(win(under));
        let doubt = covering_of(&read, [40, 40, 200, 100]);
        let beside = covering_of(&read, [500, 500, 50, 50]);
        // SAFETY: this thread's windows.
        unsafe {
            let _ = DestroyWindow(over);
            let _ = DestroyWindow(under);
        }
        assert!(
            read.iter().any(|a| a.window == win(over)),
            "the topmost window is not read above: {read:?}"
        );
        let mark = format!("window {:#x} ", win(over));
        assert!(doubt.iter().any(|d| d.starts_with(&mark)), "{doubt:?}");
        assert!(!beside.iter().any(|d| d.starts_with(&mark)), "{beside:?}");
    }

    #[test]
    fn swapped_buttons_are_refused() {
        assert!(unswapped(false).is_ok());
        let e = unswapped(true).unwrap_err().to_string();
        assert!(e.contains("swapped"), "{e}");
    }

    /// Each input carries its call's 32-bit tag whole, the high bit
    /// included, and a move is absolute on the virtual desktop.
    #[test]
    fn an_input_carries_its_tag_whole() {
        for tag in [1, 0x7FFF_FFFF, 0x8000_0000, u32::MAX] {
            // SAFETY: reading the union's mouse member, which `mouse` set.
            let mi = unsafe { mouse(Input::Down, tag).Anonymous.mi };
            assert_eq!(mi.dwExtraInfo, tag as usize);
        }
        // SAFETY: as above.
        let mi = unsafe { mouse(Input::Move { x: 7, y: 9 }, 1).Anonymous.mi };
        assert_eq!((mi.dx, mi.dy), (7, 9));
        assert!(
            mi.dwFlags
                .contains(MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)
        );
    }
}
