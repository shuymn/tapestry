//! Raw Input wakes an idle input thread, with no timer. A separate test
//! binary: Raw Input has one keyboard registration per process.

#![cfg(windows)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use input::{Capture, InputThread, Phase, QpcClock, SourceConfig, SourceSet, Waker};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, SendInput,
};

/// F24's scan code (set 1): a key no keyboard has and no program binds.
const F24_SCAN: u16 = 0x76;

/// Waits (at most 5 s) until `f` holds.
fn until(f: impl Fn() -> bool) -> bool {
    let begun = Instant::now();
    while begun.elapsed() < Duration::from_secs(5) {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

/// A press and release of F24. Injected input names no device, so the
/// input thread reads it and records nothing; it still wakes the thread.
fn press_f24() {
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wScan: F24_SCAN,
                dwFlags: flags | KEYEVENTF_SCANCODE,
                ..Default::default()
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    // SAFETY: two keyboard inputs of the size given.
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    assert_eq!(sent, 2, "SendInput");
}

/// An idle Raw Input thread is woken by input and processes it with no
/// timer: after its first wake, no heartbeat. A controller that reports
/// on its own wakes it too; the press makes sure some input arrives.
#[test]
#[ignore = "injects an F24 key press: needs an interactive desktop"]
fn raw_input_wakes_an_idle_thread_without_the_timer() {
    let sources = SourceSet::new(SourceConfig {
        max_devices: 16,
        max_sources: 8,
        max_held: 8,
        queue_capacity: 64,
        max_arrivals: 4,
    })
    .unwrap();
    let wakes = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&wakes);
    let waker = Waker::new().unwrap();
    let spawned = InputThread::spawn(
        QpcClock::start(),
        sources,
        Capture::new(),
        &waker,
        move |_, _| {
            counted.fetch_add(1, Ordering::AcqRel);
            Phase::Idle
        },
    );
    let thread = spawned.unwrap();
    // The first wake (the heartbeat, or input) reports Idle: no timer from
    // then on.
    assert!(until(|| wakes.load(Ordering::Acquire) >= 1));
    std::thread::sleep(Duration::from_millis(100));
    let before = wakes.load(Ordering::Acquire);
    press_f24();
    assert!(until(|| wakes.load(Ordering::Acquire) > before));
    let report = thread.stop().unwrap();
    println!(
        "idle for 100 ms: {before} wakes; report: waits {}, causes {:?}, rearms {}",
        report.waits, report.causes, report.rearms
    );
    assert!(report.causes.input >= 1, "{:?}", report.causes);
    assert!(report.causes.heartbeat <= 1, "{:?}", report.causes);
}
