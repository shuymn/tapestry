//! The sink (docs/test-device.md "Run Guard"): a small window of the
//! runner's own process, on a thread of its own, at a place on the run's
//! monitor, that swallows keys and the keyboard menu (no IME either), so
//! a device key held while it is in front types nothing anywhere. It
//! closes only when the runner closes it (the end, on release evidence):
//! a close from elsewhere is refused.

use std::ffi::c_void;
use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::Context as _;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows::Win32::UI::Input::Ime::{HIMC, ImmAssociateContext};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, IDC_ARROW,
    LoadCursorW, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SC_KEYMENU, WINDOW_EX_STYLE,
    WM_CHAR, WM_CLOSE, WM_DEADCHAR, WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_SYSCHAR, WM_SYSCOMMAND,
    WM_SYSDEADCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WNDCLASSW, WS_BORDER, WS_CAPTION,
    WS_POPUP, WS_VISIBLE,
};
use windows::core::w;

use super::click::Win;
use super::desktop::{hwnd, win};

/// How long the sink's thread may take to make its window, or to end.
const SINK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The runner's own close: a message only the sink's thread acts on.
const CLOSE: u32 = WM_USER + 1;

unsafe extern "system" fn proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        CLOSE => {
            // SAFETY: this thread's window.
            let _ = unsafe { DestroyWindow(h) };
            return LRESULT(0);
        }
        // Only the runner closes it.
        WM_CLOSE => return LRESULT(0),
        WM_DESTROY => {
            // SAFETY: no pointers.
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        // Keys and the keyboard menu never reach the default procedure.
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR
        | WM_DEADCHAR | WM_SYSDEADCHAR => return LRESULT(0),
        WM_SYSCOMMAND if (wp.0 & 0xFFF0) == SC_KEYMENU as usize => return LRESULT(0),
        _ => {}
    }
    // SAFETY: the arguments the system passed.
    unsafe { DefWindowProcW(h, msg, wp, lp) }
}

/// The sink's window and thread.
pub struct Sink {
    pub window: Win,
    thread: Option<JoinHandle<()>>,
}

impl Sink {
    /// Opens the sink at `area` ([x, y, w, h] on the screen).
    ///
    /// # Errors
    ///
    /// The window cannot be made.
    pub fn open(area: [i32; 4]) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let made = make(area);
            let ok = made.is_ok();
            let _ = tx.send(made.map(win));
            if !ok {
                return;
            }
            let mut msg = MSG::default();
            // SAFETY: one MSG written per call; dispatched on this thread.
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                // SAFETY: as retrieved.
                unsafe { DispatchMessageW(&msg) };
            }
        });
        let window = rx
            .recv_timeout(SINK_WAIT)
            .with_context(|| format!("the sink was not made within {SINK_WAIT:?}"))?
            .context("making the sink")?;
        Ok(Self {
            window,
            thread: Some(thread),
        })
    }

    /// Closes the sink and waits for its thread, at most [`SINK_WAIT`].
    ///
    /// # Errors
    ///
    /// The close cannot be sent, or the thread did not end in time.
    pub fn close(&mut self) -> anyhow::Result<()> {
        let Some(t) = self.thread.take() else {
            return Ok(());
        };
        // SAFETY: a message to the sink's window, which its thread handles.
        unsafe { PostMessageW(Some(hwnd(self.window)), CLOSE, WPARAM(0), LPARAM(0)) }
            .context("PostMessage")?;
        super::owner::join_within(t, "the sink's thread", SINK_WAIT).map_err(anyhow::Error::msg)
    }
}

fn make(area: [i32; 4]) -> windows::core::Result<HWND> {
    let class = w!("tapestry-test-device-sink");
    // SAFETY: the class struct is complete; registering twice fails
    // harmlessly.
    unsafe {
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            lpszClassName: class,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as isize as *mut c_void),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..WNDCLASSW::default()
        })
    };
    let [x, y, width, height] = area;
    // SAFETY: a registered class; plain values.
    let h = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("tapestry test-device sink"),
            WS_POPUP | WS_VISIBLE | WS_BORDER | WS_CAPTION,
            x,
            y,
            width,
            height,
            None,
            None,
            None,
            None,
        )
    }?;
    // No IME: a device's letter reaches no composition.
    // SAFETY: a window of this thread.
    let _ = unsafe { ImmAssociateContext(h, HIMC::default()) };
    Ok(h)
}

/// The sink's place: 480×270, centred on `monitor` ([x, y, w, h]).
pub const fn place(monitor: [i32; 4]) -> [i32; 4] {
    let (w, h) = (480, 270);
    [
        monitor[0] + (monitor[2] - w) / 2,
        monitor[1] + (monitor[3] - h) / 2,
        w,
        h,
    ]
}
