//! A real HID game controller through Raw Input, on the development PC
//! (`--ignored`): its buttons and its axis arrive as one source. Only that
//! test uses Raw Input in this process, which has one registration per
//! device type; the other checks the printed summary.
//!
//! Run it with the controller connected, then press a few buttons and turn
//! the turntable during the 20 seconds it records. A small window takes the
//! foreground, because input received in the background is never recorded
//! (the focus gate); its focus loss sets the gate's latch, as the game's
//! window does. It prints only usage numbers and counts, never what names
//! the device.

#![cfg(windows)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bms_model::TimeUs;
use eval::{ControlId, InputKind};
use input::hid::{AxisCap, axis_control, button_control, in_collection, is_axis};
use input::{
    Capture, DeviceId, DeviceKind, FocusLatch, InputThread, QpcClock, SourceConfig, SourceReader,
    SourceSet, controller_layouts, hid_usages,
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
    // What opening gave, by source id: the readers, and each participant's
    // device, kind and the axes the input thread uses for it.
    type Opened = (Vec<SourceReader>, Vec<(DeviceId, DeviceKind, Vec<AxisCap>)>);
    let (tx, rx) = mpsc::channel::<Opened>();
    let (tripped_tx, tripped_rx) = mpsc::channel::<()>();
    // Each source's count of axis values outside their declared range,
    // published by the hook at every wake.
    let out_of_range: Arc<Vec<AtomicU64>> = Arc::new((0..16).map(|_| AtomicU64::new(0)).collect());
    let counts = Arc::clone(&out_of_range);
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
                for (c, n) in counts.iter().zip(sources.axis_out_of_range()) {
                    c.store(n, Ordering::Release);
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
            let participants = sources
                .participants()
                .map(|(d, k)| (d, k, sources.axes(d).to_vec()))
                .collect();
            let _ = tx.send((sources.take_readers(), participants));
        },
    )
    .unwrap();
    let (mut readers, participants) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
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
    // One numbering: the play's source ids. Each controller source is
    // labeled by its collections' usages, and each axis compared with the
    // range the input thread declared for it.
    let mut complete = 0;
    let mut wrong = Vec::new();
    let mut blocks = Vec::new();
    for (i, (device, kind, caps)) in participants.iter().enumerate() {
        if *kind != DeviceKind::Controller {
            blocks.push(Block {
                source: i,
                header: "keyboard".to_owned(),
                lines: Vec::new(),
            });
            continue;
        }
        let own: Vec<_> = layouts.iter().filter(|l| l.device == *device).collect();
        let label: Vec<String> = own
            .iter()
            .map(|l| {
                format!(
                    "collection {} usage {:#06x}:{:#06x}{}",
                    l.collection,
                    l.usage.0,
                    l.usage.1,
                    if l.grouped { "" } else { " (not grouped)" }
                )
            })
            .collect();
        let header = format!("controller, {}", label.join(", "));
        let mut lines = Vec::new();
        for cap in caps {
            let bits = own
                .iter()
                .find_map(|l| {
                    let k = l.axes.iter().position(|a| a.control == cap.control)?;
                    Some(l.declared[k].2)
                })
                .unwrap_or(0);
            if cap.relative {
                lines.push(format!(
                    "axis {:#010x}: relative, {bits} bits",
                    cap.control.0
                ));
            } else {
                lines.push(format!(
                    "axis {:#010x}: declared {}..={} in {bits} bits, {} values a revolution",
                    cap.control.0, cap.min, cap.max, cap.wrap
                ));
            }
        }
        for (b, n) in &buttons[i] {
            lines.push(format!("button {b:#x}: {n} presses"));
        }
        for (a, (n, min, max)) in &axes[i] {
            let cap = caps.iter().find(|c| c.control.0 == *a);
            lines.push(format!(
                "axis {a:#010x}: {n} reports, observed {min}..={max}"
            ));
            // Observed against declared, for an absolute axis that moved
            // enough: inside the declared range, or else counted by the
            // input thread as a wrong descriptor and marked on the source.
            if let Some(cap) = cap.filter(|c| !c.relative)
                && *n >= 20
            {
                let inside = (cap.min..=cap.max).contains(min) && (cap.min..=cap.max).contains(max);
                let counted = out_of_range[i].load(Ordering::Acquire);
                if inside {
                    lines.push("  inside the declared range".to_owned());
                } else {
                    lines.push(format!(
                        "  OUTSIDE the declared range: {counted} values counted for the record"
                    ));
                    if counted == 0 {
                        wrong.push(format!(
                            "source {i} axis {a:#010x}: observed {min}..={max} outside {}..={} but not counted",
                            cap.min, cap.max
                        ));
                    }
                }
            }
        }
        if !buttons[i].is_empty() && !axes[i].is_empty() {
            complete += 1;
        }
        blocks.push(Block {
            source: i,
            header,
            lines,
        });
    }
    print!("{}", summary(&blocks));
    assert!(wrong.is_empty(), "axes not handled: {wrong:#?}");
    assert!(
        complete > 0,
        "no controller source delivered both a button and an axis"
    );
}

/// One source's part of the summary: its header and its lines.
struct Block {
    source: usize,
    header: String,
    lines: Vec<String>,
}

/// The summary of every source, each once, in source order: its header,
/// then its lines indented.
fn summary(blocks: &[Block]) -> String {
    let mut sorted: Vec<&Block> = blocks.iter().collect();
    sorted.sort_by_key(|b| b.source);
    sorted.dedup_by_key(|b| b.source);
    let mut out = String::new();
    for b in sorted {
        out.push_str(&format!("source {}: {}\n", b.source, b.header));
        for l in &b.lines {
            out.push_str(&format!("  {l}\n"));
        }
    }
    out
}

#[test]
fn summary_prints_each_source_once_in_order() {
    let block = |source, header: &str, lines: &[&str]| Block {
        source,
        header: header.to_owned(),
        lines: lines.iter().map(|&l| l.to_owned()).collect(),
    };
    let blocks = [
        block(
            2,
            "controller, collection 2",
            &["axis 0x02010030", "button 0x1"],
        ),
        block(0, "controller, collection 0", &["axis 0x00010030"]),
        block(1, "keyboard", &[]),
        block(2, "controller, collection 2", &["axis 0x02010030"]),
    ];
    assert_eq!(
        summary(&blocks),
        "source 0: controller, collection 0\n  axis 0x00010030\n\
         source 1: keyboard\n\
         source 2: controller, collection 2\n  axis 0x02010030\n  button 0x1\n"
    );
}
