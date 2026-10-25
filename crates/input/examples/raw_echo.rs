//! Manual check of the Raw Input path: prints each key edge with its source
//! id and receive time, disconnects, arrivals and the focus gate tripping,
//! then the input thread's wake interval distribution.
//!
//! `cargo run -p input --example raw_echo [seconds]` (30 s by default).
//! A small window takes the foreground; its focus loss sets the gate's
//! latch, because the message-only receiver window is never in the
//! foreground. Every key is recorded here; a game records bound keys only.

#[cfg(not(windows))]
fn main() {
    eprintln!("raw_echo needs Windows");
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    app::run()
}

#[cfg(windows)]
mod app {
    use std::process::ExitCode;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use bms_model::{TimeSource, TimeUs};
    use eval::{ControlId, InputKind};
    use input::raw::{RI_KEY_E0, RI_KEY_E1, key_control};
    use input::{
        Capture, FocusLatch, InputThread, Percentile, QpcClock, SourceConfig, SourceReader,
        SourceSet, WAKE_HISTOGRAM_LIMIT,
    };
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GetForegroundWindow, MSG, PM_REMOVE, PeekMessageW, RegisterClassW, SetForegroundWindow,
        TranslateMessage, WINDOW_EX_STYLE, WM_ACTIVATEAPP, WM_CLOSE, WM_KILLFOCUS, WNDCLASSW,
        WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };
    use windows::core::w;

    static LATCH: OnceLock<FocusLatch> = OnceLock::new();
    static CLOSED: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match msg {
            WM_ACTIVATEAPP if w.0 == 0 => latch(),
            WM_KILLFOCUS => latch(),
            WM_CLOSE => CLOSED.store(true, Ordering::Release),
            _ => {}
        }
        // SAFETY: forwarding the arguments the system passed.
        unsafe { DefWindowProcW(hwnd, msg, w, l) }
    }

    fn latch() {
        if let Some(latch) = LATCH.get() {
            latch.latch();
        }
    }

    /// Every key: make codes 01-FE without a prefix, with E0 and with E1.
    fn every_key() -> Vec<ControlId> {
        (1..=0xFE)
            .flat_map(|m| [0, RI_KEY_E0, RI_KEY_E1].map(|f| key_control(m, f)))
            .collect()
    }

    /// What the hook keeps between wakes.
    struct Echo {
        readers: Option<Vec<SourceReader>>,
        arrivals: usize,
        tripped: bool,
    }

    impl Echo {
        fn wake(&mut self, sources: &mut SourceSet, now: TimeUs) {
            let Some(readers) = &mut self.readers else {
                // The first wake: every keyboard present takes part.
                let keys = every_key();
                let devices: Vec<_> = sources.devices().map(|(d, _)| d).collect();
                for d in devices {
                    sources.set_recorded(d, keys.clone());
                }
                let start = sources.open_recording(now);
                println!(
                    "{:>10} us  recording opened: {} keyboards, {} keys held",
                    now.0,
                    sources.participants().count(),
                    start.held.len()
                );
                self.readers = Some(sources.take_readers());
                return;
            };
            for reader in readers.iter_mut() {
                while let Some(ev) = reader.pop() {
                    match ev.kind {
                        InputKind::Control { control, value } => println!(
                            "{:>10} us  source {}  key {:#06x}  {}",
                            ev.received.0,
                            ev.source.0,
                            control.0,
                            if value == 0 { "release" } else { "press" }
                        ),
                        InputKind::Disconnected => println!(
                            "{:>10} us  source {}  Disconnected",
                            ev.received.0, ev.source.0
                        ),
                    }
                }
            }
            for a in &sources.arrivals()[self.arrivals..] {
                println!(
                    "{:>10} us  arrival: {:?} (takes part from the next play)",
                    a.at.0, a.kind
                );
            }
            self.arrivals = sources.arrivals().len();
            if sources.gate().is_tripped() && !self.tripped {
                self.tripped = true;
                println!("{:>10} us  gate tripped: nothing more is recorded", now.0);
            }
        }
    }

    pub fn run() -> ExitCode {
        let seconds = std::env::args()
            .nth(1)
            .map_or(Ok(30), |s| s.parse::<u64>())
            .unwrap_or(30);
        let clock = QpcClock::start();
        println!(
            "QPC frequency {} Hz; running {seconds} s",
            clock.frequency().hz()
        );
        let sources = SourceSet::new(SourceConfig {
            max_devices: 16,
            max_sources: 8,
            max_held: 32,
            queue_capacity: 4096,
            max_arrivals: 16,
        })
        .expect("8 sources are within the limit");
        let _ = LATCH.set(sources.gate().handle());

        let class = w!("tapestry-raw-echo");
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
                w!("raw_echo"),
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
        };
        let window = match window {
            Ok(w) => w,
            Err(e) => {
                eprintln!("CreateWindowExW failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        // SAFETY: a window of this thread.
        let foreground = unsafe {
            let _ = SetForegroundWindow(window);
            GetForegroundWindow() == window
        };
        println!("window open; in the foreground: {foreground}");

        let mut echo = Echo {
            readers: None,
            arrivals: 0,
            tripped: false,
        };
        let thread = match InputThread::spawn(clock, sources, Capture::new(), move |s, now| {
            echo.wake(s, now);
        }) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };

        let end = Instant::now() + Duration::from_secs(seconds);
        let mut msg = MSG::default();
        while Instant::now() < end && !CLOSED.load(Ordering::Acquire) {
            // SAFETY: pumping this thread's messages.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // The input thread stops first, so closing the window does not
        // print a focus loss.
        let stopped = thread.stop();
        // SAFETY: the window of this thread.
        let _ = unsafe { DestroyWindow(window) };
        let report = match stopped {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };
        let p99 = match report.intervals.p99() {
            Percentile::At(us) => format!("{us} us"),
            Percentile::Above => format!("above {WAKE_HISTOGRAM_LIMIT} us"),
            Percentile::Empty => "none".to_owned(),
        };
        println!(
            "{:>10} us  wake intervals: {} wakes, p99 {p99}, max {} us; MMCSS Games: {}",
            clock.now().0,
            report.intervals.count(),
            report.intervals.max(),
            report.mmcss
        );
        ExitCode::SUCCESS
    }
}
