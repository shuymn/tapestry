//! A real HID game controller through Raw Input, on the development PC
//! (`--ignored`): its buttons and its axis arrive as one source. One test
//! per process: Raw Input has one registration per device type per
//! process.
//!
//! Run it with the controller connected, then press a few buttons and turn
//! the turntable during the 20 seconds it records. A small window takes the
//! foreground, because input received in the background is never recorded
//! (the focus gate); its focus loss sets the gate's latch, as the game's
//! window does. It prints only usage numbers and counts, never what names
//! the device.

#![cfg(windows)]

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bms_model::TimeUs;
use eval::{ControlId, InputKind};
use input::hid::{axis_control, button_control, in_collection, is_axis};
use input::{
    Capture, DeviceKind, FocusLatch, InputThread, QpcClock, SourceConfig, SourceReader, SourceSet,
    controller_layouts, hid_usages,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetForegroundWindow, MSG, PM_REMOVE, PeekMessageW, RegisterClassW, SetForegroundWindow,
    WINDOW_EX_STYLE, WM_ACTIVATEAPP, WM_KILLFOCUS, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::core::w;

static LATCH: OnceLock<FocusLatch> = OnceLock::new();

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let lost = (msg == WM_ACTIVATEAPP && w.0 == 0) || msg == WM_KILLFOCUS;
    if lost && let Some(latch) = LATCH.get() {
        latch.latch();
    }
    // SAFETY: forwarding the arguments the system passed.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

/// A small window of this thread in the foreground, if the system lets it.
fn foreground_window() -> HWND {
    let class = w!("tapestry-hid-round-trip");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: the class name and window procedure are static.
    unsafe { RegisterClassW(&wc) };
    // SAFETY: a top-level window of the class registered above.
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("hid_controller_round_trip"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            360,
            120,
            None,
            None,
            None,
            None,
        )
    }
    .unwrap();
    // SAFETY: a window of this thread.
    let foreground = unsafe {
        let _ = SetForegroundWindow(window);
        GetForegroundWindow() == window
    };
    println!("window in the foreground: {foreground}");
    window
}

fn pump() {
    let mut msg = MSG::default();
    // SAFETY: pumping this thread's messages.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

#[test]
#[ignore = "needs a HID game controller and someone playing it"]
fn hid_controller_round_trip() {
    let usages = hid_usages().unwrap();
    let controllers = usages.iter().filter(|u| u.2).count();
    println!(
        "HID devices: {}, read as game controllers: {controllers}",
        usages.len()
    );
    for (page, usage, controller) in &usages {
        println!("  usage page {page:#06x} usage {usage:#06x} controller {controller}");
    }
    assert!(controllers > 0, "no game controller is connected");
    let layouts = controller_layouts().unwrap();
    // Declared values per revolution by axis control, for the summary.
    let mut declared = BTreeMap::new();
    for l in &layouts {
        println!(
            "collection {} of controller source {}: grouped by container {}, {} axes",
            l.collection,
            l.source,
            l.grouped,
            l.axes.len()
        );
        for a in &l.axes {
            println!(
                "  axis {:#010x}: relative {}, declared values per revolution {}",
                a.control.0, a.relative, a.wrap
            );
            declared.insert(a.control.0, a.wrap);
        }
    }

    let sources = SourceSet::new(SourceConfig {
        max_devices: 16,
        max_sources: 16,
        max_held: 64,
        queue_capacity: 1 << 16,
        max_arrivals: 4,
    })
    .unwrap();
    let _ = LATCH.set(sources.gate().handle());
    let window = foreground_window();
    let mut recorded: Vec<ControlId> = Vec::new();
    for c in layouts.iter().map(|l| l.collection) {
        recorded.extend((1..=32).map(|b| in_collection(button_control(b), c)));
        recorded.extend((0x30..=0x38).map(|u| in_collection(axis_control(0x01, u), c)));
    }
    let (tx, rx) = mpsc::channel::<(Vec<SourceReader>, Vec<DeviceKind>)>();
    let (tripped_tx, tripped_rx) = mpsc::channel::<()>();
    let mut opened = false;
    let thread = InputThread::spawn(
        QpcClock::start(),
        sources,
        Capture::new(),
        move |sources: &mut SourceSet, now: TimeUs| {
            if opened {
                if sources.gate().is_tripped() {
                    let _ = tripped_tx.send(());
                }
                return;
            }
            opened = true;
            let devices: Vec<_> = sources.devices().collect();
            for (d, kind) in devices {
                if kind == DeviceKind::Controller {
                    sources.set_recorded(d, recorded.clone());
                }
            }
            sources.open_recording(now);
            let kinds = sources.participants().map(|(_, k)| k).collect();
            let _ = tx.send((sources.take_readers(), kinds));
        },
    )
    .unwrap();
    let (mut readers, kinds) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    println!("press buttons and turn the turntable for 20 s");
    // Per source: button presses by number, and reports per axis.
    let mut buttons: Vec<BTreeMap<u32, usize>> = vec![BTreeMap::new(); readers.len()];
    // Per axis: reports, and the lowest and highest value seen.
    let mut axes: Vec<BTreeMap<u32, (usize, i32, i32)>> = vec![BTreeMap::new(); readers.len()];
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        pump();
        for (i, reader) in readers.iter_mut().enumerate() {
            while let Some(e) = reader.pop() {
                assert_eq!(e.source.index(), i);
                if let InputKind::Control { control, value } = e.kind {
                    if is_axis(control) {
                        let a = axes[i].entry(control.0).or_insert((0, value, value));
                        *a = (a.0 + 1, a.1.min(value), a.2.max(value));
                    } else if value != 0 {
                        *buttons[i].entry(control.0).or_default() += 1;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    thread.stop().unwrap();
    // SAFETY: the window of this thread.
    let _ = unsafe { DestroyWindow(window) };
    if tripped_rx.try_recv().is_ok() {
        println!("the focus gate tripped: input after the focus loss was not recorded");
    }
    let mut complete = 0;
    for (i, kind) in kinds.iter().enumerate() {
        if *kind != DeviceKind::Controller {
            continue;
        }
        println!("source {i}: controller");
        for (b, n) in &buttons[i] {
            println!("  button {b}: {n} presses");
        }
        for (a, (n, min, max)) in &axes[i] {
            println!(
                "  axis {a:#010x}: {n} reports, observed {min}..={max}, declared {} values",
                declared.get(a).copied().unwrap_or(0)
            );
        }
        if !buttons[i].is_empty() && !axes[i].is_empty() {
            complete += 1;
        }
    }
    assert!(
        complete > 0,
        "no controller source delivered both a button and an axis"
    );
}
