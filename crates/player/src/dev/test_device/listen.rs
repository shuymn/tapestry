//! `tapestry test-device listen --out <jsonl> --port <location> [--for
//! <ms>]` (docs/test-device.md "Host Commands", "Run Guard"): the
//! recorder. A message-only window of its own process registers every
//! usage the player registers (generic desktop keyboards, joysticks and
//! gamepads, `crates/input/src/win.rs` `USAGES`) and the mouse, with
//! `RIDEV_INPUTSINK | RIDEV_DEVNOTIFY`, so it hears every source whichever
//! window is in front. The device's keyboard and pad handles are bound by
//! the location: a handle is the device's only when its interface path
//! resolves to an interface of a test device whose composite parent is at
//! that location, at the start and again at each arrival. It records every
//! keyboard edge and every controller report, the device's and any other
//! (foreign input), every mouse button edge with its handle and
//! `ulExtraInformation`, each with the foreground window and its process,
//! every arrival and removal, and its own errors as `error` records naming
//! the stretch of the stream they affect. Its first record is `ready`; it
//! ends at its input's end or after `--for`, draining the Raw Input queue
//! before its `end`.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use serde_json::{Value, json};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceList, HRAWINPUT, RAWINPUTDEVICE, RAWINPUTDEVICELIST,
    RAWINPUTHEADER, RID_INPUT, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIM_TYPEHID, RIM_TYPEKEYBOARD,
    RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GIDC_ARRIVAL, GIDC_REMOVAL, GetMessageW,
    HWND_MESSAGE, KillTimer, MSG, PM_REMOVE, PeekMessageW, RegisterClassW, SetTimer,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_INPUT_DEVICE_CHANGE, WM_TIMER, WNDCLASSW,
};
use windows::core::w;

use test_device::contract::hex;

use super::desktop::{foreground, process_of};
use super::devices::{device_name, raw_location};
use super::job::this_process;
use super::owner::Stream;
use super::win::qpc;

/// The generic desktop usages registered: the player's keyboards (0x06),
/// joysticks (0x04) and gamepads (0x05), and the mouse (0x02).
pub const USAGES: [u16; 4] = [0x06, 0x04, 0x05, 0x02];

/// The registrations of `USAGES` for window `hwnd`.
pub fn registrations(hwnd: HWND) -> [RAWINPUTDEVICE; 4] {
    USAGES.map(|usage| RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: usage,
        dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
        hwndTarget: hwnd,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub out: PathBuf,
    /// The device's USB location path.
    pub port: String,
    pub for_ms: Option<u64>,
}

/// Which of the device's interfaces a handle is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ours {
    Keyboard,
    Pad,
}

impl Ours {
    fn of(mi: u8) -> Option<Self> {
        match mi {
            test_device::KEYBOARD_INTERFACE => Some(Self::Keyboard),
            test_device::PAD_INTERFACE => Some(Self::Pad),
            _ => None,
        }
    }
}

/// The device's handles, bound by the location.
#[derive(Clone, Debug, Default)]
pub struct Binding {
    pub location: String,
    handles: HashMap<u64, Ours>,
}

impl Binding {
    pub fn new(location: &str) -> Self {
        Self {
            location: location.to_owned(),
            handles: HashMap::new(),
        }
    }

    /// Binds `h` when `at` (its interface and location) is one of the
    /// device's interfaces at the bound location: whether it did.
    pub fn arrive(&mut self, h: u64, at: Option<(u8, String)>) -> bool {
        let Some((mi, location)) = at else {
            return false;
        };
        let Some(ours) = Ours::of(mi) else {
            return false;
        };
        if location != self.location {
            return false;
        }
        self.handles.insert(h, ours);
        true
    }

    /// Unbinds `h`: whether it was the device's.
    pub fn remove(&mut self, h: u64) -> bool {
        self.handles.remove(&h).is_some()
    }

    pub fn ours(&self, h: u64) -> Option<Ours> {
        self.handles.get(&h).copied()
    }

    fn first(&self, which: Ours) -> Option<u64> {
        let mut hs: Vec<u64> = self
            .handles
            .iter()
            .filter(|&(_, &o)| o == which)
            .map(|(&h, _)| h)
            .collect();
        hs.sort_unstable();
        hs.first().copied()
    }
}

/// One Raw Input packet, as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Packet {
    Key {
        h: u64,
        make: u16,
        flags: u16,
    },
    /// A HID packet: its reports' bytes, as read.
    Hid {
        h: u64,
        reports: Vec<Vec<u8>>,
    },
    Mouse {
        h: u64,
        buttons: u16,
        extra: u32,
    },
}

/// `RAWKEYBOARD` flags: a break, and the E0 prefix.
const RI_KEY_BREAK: u16 = 1;
const RI_KEY_E0: u16 = 2;
/// `RAWMOUSE.usButtonFlags`' button edges (buttons 1-5 down and up); the
/// wheel's flags are left out.
pub const BUTTON_EDGES: u16 = 0x03FF;

/// The header's size, and where the data starts, on this target.
const HEADER: usize = size_of::<RAWINPUTHEADER>();

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The packet in `b`, a `RAWINPUT` as `GetRawInputData` wrote it: `None`
/// when it is short or of another type.
pub fn parse(b: &[u8]) -> Option<Packet> {
    let kind = u32_at(b, 0)?;
    // hDevice follows dwType and dwSize.
    let h = u64::from_le_bytes(b.get(8..16)?.try_into().ok()?);
    let d = HEADER;
    if kind == RIM_TYPEKEYBOARD.0 {
        // RAWKEYBOARD: MakeCode, Flags, Reserved, VKey, Message, Extra.
        return Some(Packet::Key {
            h,
            make: u16_at(b, d)?,
            flags: u16_at(b, d + 2)?,
        });
    }
    if kind == RIM_TYPEMOUSE.0 {
        // RAWMOUSE: usFlags (2, padded to 4), usButtonFlags, usButtonData,
        // ulRawButtons, lLastX, lLastY, ulExtraInformation.
        return Some(Packet::Mouse {
            h,
            buttons: u16_at(b, d + 4)?,
            extra: u32_at(b, d + 20)?,
        });
    }
    if kind == RIM_TYPEHID.0 {
        let each = u32_at(b, d)? as usize;
        let count = u32_at(b, d + 4)? as usize;
        let mut reports = Vec::with_capacity(count);
        for i in 0..count {
            let at = d + 8 + i * each;
            reports.push(b.get(at..at + each)?.to_vec());
        }
        return Some(Packet::Hid { h, reports });
    }
    None
}

/// The foreground window and its process.
pub type Fg = (u64, u32);

fn fg_json(fg: Fg) -> Value {
    json!({ "window": fg.0, "pid": fg.1 })
}

/// The records of one packet read at `q` with `fg` in front: every
/// keyboard edge and controller report, the device's (`ours`) and any
/// other's, and every mouse button edge with its tag.
pub fn records(p: &Packet, q: i64, fg: Fg, binding: &Binding) -> Vec<Value> {
    match p {
        Packet::Key { h, make, flags } => {
            let code = if flags & RI_KEY_E0 != 0 {
                0xE000 | make
            } else {
                *make
            };
            vec![json!({
                "t": "key", "q": q, "h": h, "ours": binding.ours(*h) == Some(Ours::Keyboard),
                "code": code, "down": flags & RI_KEY_BREAK == 0, "fg": fg_json(fg),
            })]
        }
        Packet::Hid { h, reports } => reports
            .iter()
            .map(|r| {
                json!({
                    "t": "pad", "q": q, "h": h, "ours": binding.ours(*h) == Some(Ours::Pad),
                    "bytes": hex(r), "fg": fg_json(fg),
                })
            })
            .collect(),
        Packet::Mouse { h, buttons, extra } => {
            let edges = buttons & BUTTON_EDGES;
            if edges == 0 {
                return Vec::new();
            }
            vec![json!({
                "t": "mouse", "q": q, "h": h, "buttons": edges, "extra": extra,
                "fg": fg_json(fg),
            })]
        }
    }
}

/// Whether a mouse edge with tag `extra` is a click a runner made: its tag
/// is one of the runner's recorded calls'. The handle says nothing:
/// injected input and a precision touchpad's both come with a null one.
pub fn a_runner_s(extra: u32, calls: &[u32]) -> bool {
    calls.contains(&extra)
}

/// What the recorder reads, one at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    /// A packet's bytes as read, or why it could not be read.
    Input {
        q: i64,
        read: Result<Vec<u8>, String>,
    },
    Change {
        q: i64,
        h: u64,
        arrival: bool,
    },
}

/// The recorder's state between messages.
pub struct Recorder<'a> {
    pub binding: Binding,
    write: &'a mut dyn FnMut(&Value),
    resolve: &'a dyn Fn(u64) -> Option<(u8, String)>,
    fg: &'a dyn Fn() -> Fg,
    /// The host counter of the last record written.
    last: i64,
}

impl<'a> Recorder<'a> {
    pub fn new(
        binding: Binding,
        write: &'a mut dyn FnMut(&Value),
        resolve: &'a dyn Fn(u64) -> Option<(u8, String)>,
        fg: &'a dyn Fn() -> Fg,
    ) -> Self {
        Self {
            binding,
            write,
            resolve,
            fg,
            last: 0,
        }
    }

    fn put(&mut self, v: &Value, q: i64) {
        (self.write)(v);
        self.last = q;
    }

    /// Records a handle present at the start.
    pub fn present(&mut self, q: i64, h: u64) {
        let ours = self.binding.arrive(h, (self.resolve)(h));
        self.put(
            &json!({ "t": "dev", "q": q, "h": h, "change": "present", "ours": ours }),
            q,
        );
    }

    pub fn take(&mut self, m: Msg) {
        match m {
            Msg::Input { q, read: Ok(bytes) } => {
                let Some(p) = parse(&bytes) else {
                    let from = self.last;
                    self.put(
                        &json!({
                            "t": "error", "from": from, "to": q,
                            "what": format!("a packet of {} bytes that does not parse", bytes.len()),
                        }),
                        q,
                    );
                    return;
                };
                let fg = (self.fg)();
                for v in records(&p, q, fg, &self.binding) {
                    self.put(&v, q);
                }
            }
            Msg::Input { q, read: Err(why) } => {
                let from = self.last;
                self.put(
                    &json!({ "t": "error", "from": from, "to": q, "what": why }),
                    q,
                );
            }
            Msg::Change { q, h, arrival } => {
                let ours = if arrival {
                    self.binding.arrive(h, (self.resolve)(h))
                } else {
                    self.binding.remove(h)
                };
                let change = if arrival { "arrival" } else { "removal" };
                self.put(
                    &json!({ "t": "dev", "q": q, "h": h, "change": change, "ours": ours }),
                    q,
                );
            }
        }
    }

    /// Takes every message still queued, then writes `end`.
    pub fn end(mut self, pending: &mut dyn FnMut() -> Option<Msg>, now: &dyn Fn() -> i64) {
        while let Some(m) = pending() {
            self.take(m);
        }
        let q = now();
        self.put(&json!({ "t": "end", "q": q }), q);
    }
}

/// Reads the packet of one `WM_INPUT`.
fn read(raw: HRAWINPUT, buf: &mut [u64]) -> Result<Vec<u8>, String> {
    let header = u32::try_from(HEADER).unwrap_or(0);
    let mut size = u32::try_from(buf.len() * 8).unwrap_or(0);
    // SAFETY: the buffer holds `size` bytes, 8-byte aligned.
    let got = unsafe {
        GetRawInputData(
            raw,
            RID_INPUT,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            &mut size,
            header,
        )
    };
    if got == u32::MAX {
        return Err(format!(
            "GetRawInputData failed: {}",
            windows::core::Error::from_thread()
        ));
    }
    // SAFETY: the buffer's bytes, of which `got` were written.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len() * 8) };
    bytes
        .get(..got as usize)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| format!("GetRawInputData wrote {got} bytes"))
}

/// The message of one queued window message, if it is the recorder's.
fn message(msg: &MSG, buf: &mut [u64]) -> Option<Msg> {
    let q = qpc();
    match msg.message {
        WM_INPUT => Some(Msg::Input {
            q,
            read: read(HRAWINPUT(msg.lParam.0 as *mut c_void), buf),
        }),
        WM_INPUT_DEVICE_CHANGE => {
            let change = u32::try_from(msg.wParam.0).unwrap_or(0);
            (change == GIDC_ARRIVAL || change == GIDC_REMOVAL).then_some(Msg::Change {
                q,
                h: msg.lParam.0 as u64,
                arrival: change == GIDC_ARRIVAL,
            })
        }
        _ => None,
    }
}

/// The window's procedure: the default one.
unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: the arguments the system passed.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Every Raw Input device present.
fn present() -> Vec<u64> {
    let size = u32::try_from(size_of::<RAWINPUTDEVICELIST>()).unwrap_or(0);
    let mut count = 0;
    // SAFETY: a count query.
    let _ = unsafe { GetRawInputDeviceList(None, &mut count, size) };
    let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize + 8];
    let mut cap = u32::try_from(list.len()).unwrap_or(0);
    // SAFETY: the list holds `cap` entries.
    let got = unsafe { GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut cap, size) };
    if got == u32::MAX {
        return Vec::new();
    }
    list.truncate(got as usize);
    list.iter().map(|d| d.hDevice.0 as u64).collect()
}

fn resolve(h: u64) -> Option<(u8, String)> {
    raw_location(&device_name(HANDLE(h as usize as *mut c_void))?)
}

fn fg_now() -> Fg {
    let w = foreground();
    (w, process_of(w))
}

/// The recorder.
///
/// # Errors
///
/// Its file, window or registration cannot be made, or a write failed.
pub fn listen(args: &Args) -> anyhow::Result<()> {
    let out = Stream::create(&args.out)?;
    let me = this_process()?;
    // SAFETY: a static class name and the default procedure. It returns
    // 0 when the class exists already.
    unsafe {
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            lpszClassName: w!("tapestry-test-device-listen"),
            ..WNDCLASSW::default()
        })
    };
    // SAFETY: a message-only window of the class registered above.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("tapestry-test-device-listen"),
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    }
    .context("CreateWindowEx")?;
    let size = u32::try_from(size_of::<RAWINPUTDEVICE>()).unwrap_or(0);
    // SAFETY: initialised registrations for this thread's window.
    unsafe { RegisterRawInputDevices(&registrations(hwnd), size) }
        .context("RegisterRawInputDevices")?;
    let stop = Arc::new(AtomicBool::new(false));
    {
        // The input's end, or a failed read of it, ends the recorder.
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut buf = [0_u8; 256];
            let mut input = std::io::stdin().lock();
            while matches!(input.read(&mut buf), Ok(n) if n > 0) {}
            stop.store(true, Ordering::Release);
        });
    }
    let until = args
        .for_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut write = |v: &Value| out.write(v);
    let mut rec = Recorder::new(Binding::new(&args.port), &mut write, &resolve, &fg_now);
    let handles = present();
    let ready_q = qpc();
    let mut bound = Binding::new(&args.port);
    for &h in &handles {
        bound.arrive(h, resolve(h));
    }
    (rec.write)(&json!({
        "t": "ready", "q": ready_q, "process": { "pid": me.pid, "created": me.created },
        "window": hwnd.0 as u64, "location": args.port,
        "keyboard": bound.first(Ours::Keyboard), "pad": bound.first(Ours::Pad),
    }));
    for h in handles {
        rec.present(qpc(), h);
    }
    // SAFETY: a timer on this thread's window.
    unsafe { SetTimer(Some(hwnd), 1, 10, None) };
    let mut buf = vec![0_u64; 1024];
    let mut msg = MSG::default();
    loop {
        // SAFETY: one MSG written per call.
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if got.0 <= 0 {
            break;
        }
        if msg.message == WM_TIMER {
            if stop.load(Ordering::Acquire) || until.is_some_and(|u| Instant::now() >= u) {
                break;
            }
            continue;
        }
        if let Some(m) = message(&msg, &mut buf) {
            rec.take(m);
        }
        // SAFETY: as retrieved; for WM_INPUT this lets the default
        // procedure clean up.
        unsafe { DispatchMessageW(&msg) };
    }
    // SAFETY: the timer set above.
    let _ = unsafe { KillTimer(Some(hwnd), 1) };
    let mut pending = || loop {
        let mut m = MSG::default();
        // SAFETY: one MSG written.
        if !unsafe { PeekMessageW(&mut m, None, 0, 0, PM_REMOVE) }.as_bool() {
            return None;
        }
        let taken = message(&m, &mut buf);
        // SAFETY: as retrieved.
        unsafe { DispatchMessageW(&m) };
        if taken.is_some() {
            return taken;
        }
    };
    rec.end(&mut pending, &qpc);
    if let Some(e) = out.close() {
        bail!("the recorder's file: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// `RI_MOUSE_LEFT_BUTTON_DOWN`, `RI_MOUSE_LEFT_BUTTON_UP`.
    const LEFT_DOWN: u16 = 0x0001;
    const LEFT_UP: u16 = 0x0002;
    const HERE: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(3)";
    const ELSEWHERE: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(4)";

    fn header(kind: u32, h: u64) -> Vec<u8> {
        let mut b = vec![0_u8; HEADER];
        b[0..4].copy_from_slice(&kind.to_le_bytes());
        b[8..16].copy_from_slice(&h.to_le_bytes());
        b
    }

    fn key(h: u64, make: u16, flags: u16) -> Vec<u8> {
        let mut b = header(RIM_TYPEKEYBOARD.0, h);
        b.extend(make.to_le_bytes());
        b.extend(flags.to_le_bytes());
        b.extend([0; 12]);
        b
    }

    fn mouse(h: u64, buttons: u16, extra: u32) -> Vec<u8> {
        let mut b = header(RIM_TYPEMOUSE.0, h);
        b.extend([0; 4]);
        b.extend(buttons.to_le_bytes());
        b.extend([0; 14]);
        b.extend(extra.to_le_bytes());
        b
    }

    fn hid(h: u64, reports: &[&[u8]]) -> Vec<u8> {
        let mut b = header(RIM_TYPEHID.0, h);
        let each = reports.first().map_or(0, |r| r.len());
        b.extend(u32::try_from(each).unwrap().to_le_bytes());
        b.extend(u32::try_from(reports.len()).unwrap().to_le_bytes());
        for r in reports {
            b.extend(*r);
        }
        b
    }

    /// Handles 1 (keyboard) and 2 (pad) are the device's at HERE; 3 and 4
    /// are its interfaces at ELSEWHERE; 5 is another keyboard.
    fn resolve(h: u64) -> Option<(u8, String)> {
        match h {
            1 => Some((test_device::KEYBOARD_INTERFACE, HERE.into())),
            2 => Some((test_device::PAD_INTERFACE, HERE.into())),
            3 => Some((test_device::KEYBOARD_INTERFACE, ELSEWHERE.into())),
            4 => Some((test_device::PAD_INTERFACE, ELSEWHERE.into())),
            _ => None,
        }
    }

    fn run(msgs: Vec<Msg>, queued: Vec<Msg>) -> Vec<Value> {
        let out = RefCell::new(Vec::new());
        let mut write = |v: &Value| out.borrow_mut().push(v.clone());
        let fg = || (77, 7);
        let mut rec = Recorder::new(Binding::new(HERE), &mut write, &resolve, &fg);
        for h in 1..=5 {
            rec.present(0, h);
        }
        for m in msgs {
            rec.take(m);
        }
        let mut queued = queued.into_iter();
        rec.end(&mut || queued.next(), &|| 999);
        out.into_inner()
    }

    fn input(q: i64, b: Vec<u8>) -> Msg {
        Msg::Input { q, read: Ok(b) }
    }

    #[test]
    fn the_player_s_usages_are_registered() {
        let regs = registrations(HWND::default());
        // The player's (crates/input/src/win.rs USAGES) and the mouse.
        for usage in [0x06, 0x04, 0x05, 0x02] {
            let r = regs
                .iter()
                .find(|r| r.usUsage == usage)
                .unwrap_or_else(|| panic!("usage {usage:#04x} unregistered"));
            assert_eq!(r.usUsagePage, 0x01);
            assert_eq!(r.dwFlags, RIDEV_INPUTSINK | RIDEV_DEVNOTIFY);
        }
    }

    #[test]
    fn only_the_bound_device_s_handles_are_its() {
        let recs = run(
            vec![
                input(10, key(1, 0x76, 0)),
                input(11, key(3, 0x76, 0)),
                input(12, key(2, 0x76, 0)),
                input(13, hid(2, &[&[0, 1, 2]])),
                input(14, hid(4, &[&[0, 1, 2]])),
                input(15, hid(1, &[&[0, 1, 2]])),
            ],
            vec![],
        );
        let ours: Vec<(i64, bool)> = recs
            .iter()
            .filter(|v| v["t"] == "key" || v["t"] == "pad")
            .map(|v| (v["q"].as_i64().unwrap(), v["ours"].as_bool().unwrap()))
            .collect();
        assert_eq!(
            ours,
            [
                (10, true),
                (11, false),
                (12, false),
                (13, true),
                (14, false),
                (15, false)
            ]
        );
    }

    /// The device's handle bound again after its arrival, and none after
    /// its removal.
    #[test]
    fn an_arrival_binds_and_a_removal_unbinds() {
        let recs = run(
            vec![
                Msg::Change {
                    q: 10,
                    h: 1,
                    arrival: false,
                },
                input(11, key(1, 0x76, 0)),
                Msg::Change {
                    q: 12,
                    h: 1,
                    arrival: true,
                },
                input(13, key(1, 0x76, 0)),
            ],
            vec![],
        );
        let ours: Vec<bool> = recs
            .iter()
            .filter(|v| v["t"] == "key")
            .map(|v| v["ours"].as_bool().unwrap())
            .collect();
        assert_eq!(ours, [false, true]);
    }

    #[test]
    fn foreign_input_is_recorded() {
        let recs = run(
            vec![
                input(10, key(5, 0x1E, 0)),
                input(11, key(5, 0x1E, RI_KEY_BREAK)),
                input(12, hid(4, &[&[9, 9], &[8, 8]])),
                input(13, key(0, 0x1C, RI_KEY_E0)),
            ],
            vec![],
        );
        let kinds: Vec<(&str, i64, bool)> = recs
            .iter()
            .filter(|v| v["t"] == "key" || v["t"] == "pad")
            .map(|v| {
                (
                    v["t"].as_str().unwrap(),
                    v["q"].as_i64().unwrap(),
                    v["ours"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            [
                ("key", 10, false),
                ("key", 11, false),
                ("pad", 12, false),
                ("pad", 12, false),
                ("key", 13, false)
            ]
        );
        let last_key = recs.iter().rfind(|v| v["t"] == "key").unwrap();
        assert_eq!(last_key["code"], 0xE01C);
        assert_eq!(last_key["fg"], json!({"window": 77, "pid": 7}));
        let pad = recs.iter().find(|v| v["t"] == "pad").unwrap();
        assert_eq!(pad["bytes"], "0909");
    }

    /// A left edge is a runner's only by a tag the runner recorded: a null
    /// handle (injected, or a touchpad) says nothing.
    #[test]
    fn a_left_edge_without_a_recorded_tag_is_foreign() {
        let calls = [0xBEEF_0001, 0xBEEF_0002];
        assert!(a_runner_s(0xBEEF_0001, &calls));
        assert!(!a_runner_s(0, &calls));
        assert!(!a_runner_s(0xBEEF_0003, &calls));
        // The recorder keeps the handle and the tag of every edge.
        let recs = run(
            vec![
                input(10, mouse(0, LEFT_DOWN, 0)),
                input(11, mouse(9, LEFT_UP, 0xBEEF_0001)),
                // A move with no button edge is not recorded.
                input(12, mouse(0, 0, 0xBEEF_0002)),
            ],
            vec![],
        );
        let m: Vec<(u64, u64, u64)> = recs
            .iter()
            .filter(|v| v["t"] == "mouse")
            .map(|v| {
                (
                    v["h"].as_u64().unwrap(),
                    v["buttons"].as_u64().unwrap(),
                    v["extra"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(m, [(0, 1, 0), (9, 2, 0xBEEF_0001)]);
    }

    #[test]
    fn a_failed_read_is_an_error_record() {
        let recs = run(
            vec![
                input(10, key(1, 0x76, 0)),
                Msg::Input {
                    q: 20,
                    read: Err("GetRawInputData failed".into()),
                },
                input(30, vec![1, 2, 3]),
            ],
            vec![],
        );
        let errors: Vec<(i64, i64)> = recs
            .iter()
            .filter(|v| v["t"] == "error")
            .map(|v| (v["from"].as_i64().unwrap(), v["to"].as_i64().unwrap()))
            .collect();
        // Each names the stretch from the last record before it.
        assert_eq!(errors, [(10, 20), (20, 30)]);
    }

    #[test]
    fn the_recorder_drains_before_its_end() {
        let recs = run(
            vec![input(10, key(1, 0x76, 0))],
            vec![
                input(20, key(1, 0x76, RI_KEY_BREAK)),
                input(21, hid(2, &[&[0; 3]])),
            ],
        );
        let tail: Vec<&str> = recs
            .iter()
            .rev()
            .take(3)
            .map(|v| v["t"].as_str().unwrap())
            .collect();
        assert_eq!(tail, ["end", "pad", "key"]);
        assert_eq!(recs.last().unwrap()["q"], 999);
    }

    /// Every tag the runner makes comes back whole through Raw Input, the
    /// high bit included: move-only `SendInput` calls to where the cursor
    /// is, read back from `RAWMOUSE.ulExtraInformation`.
    #[test]
    #[ignore = "injects mouse moves: needs an interactive desktop and the real-app lock"]
    fn distinct_tags_round_trip_through_raw_input() {
        use super::super::click::{Desk as _, Input, tag, to_abs};
        use super::super::desktop::Real;
        let tags = [
            tag(0x0000, 0x0001),
            tag(0x7FFF, 0xFFFF),
            tag(0x8000, 0x0000),
            tag(0xFFFF, 0xFFFF),
        ];
        assert_eq!(tags, [1, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF]);
        // SAFETY: a message-only window of a registered class.
        unsafe {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(proc),
                lpszClassName: w!("tapestry-test-device-listen-test"),
                ..WNDCLASSW::default()
            })
        };
        // SAFETY: as above.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("tapestry-test-device-listen-test"),
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .unwrap();
        let size = u32::try_from(size_of::<RAWINPUTDEVICE>()).unwrap();
        // SAFETY: registrations for this thread's window.
        unsafe { RegisterRawInputDevices(&registrations(hwnd), size) }.unwrap();
        let mut note = |_: u32, _: &str| {};
        let mut desk = Real {
            monitor: String::new(),
            note: &mut note,
        };
        for t in tags {
            // A move to where the cursor is: nothing on the desktop changes.
            let (cx, cy) = desk.cursor().unwrap();
            let [vx, vy, vw, vh] = desk.virtual_screen();
            let at = Input::Move {
                x: to_abs(cx, vx, vw),
                y: to_abs(cy, vy, vh),
            };
            assert_eq!(desk.send(&[at], t), 1);
        }
        let mut seen = Vec::new();
        let mut buf = vec![0_u64; 1024];
        let end = Instant::now() + Duration::from_secs(5);
        while seen.len() < tags.len() && Instant::now() < end {
            let mut m = MSG::default();
            // SAFETY: one MSG written.
            if unsafe { PeekMessageW(&mut m, None, 0, 0, PM_REMOVE) }.as_bool() {
                if let Some(Msg::Input { read: Ok(b), .. }) = message(&m, &mut buf)
                    && let Some(Packet::Mouse { h: 0, extra, .. }) = parse(&b)
                    && tags.contains(&extra)
                {
                    seen.push(extra);
                }
                // SAFETY: as retrieved.
                unsafe { DispatchMessageW(&m) };
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(seen, tags);
    }
}
