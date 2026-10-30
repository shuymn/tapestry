//! Manual check of the Raw Input path: prints each key edge with its source
//! id and receive time, disconnects, arrivals and the focus gate tripping,
//! then what the input thread measured: its wakes per second, by cause,
//! the heartbeat's lateness and the publication intervals.
//!
//! `cargo run -p input --example raw_echo [seconds] [idle]` (30 s by
//! default). With `idle` the thread is Idle after its first wake: it wakes
//! for input only, and keys print as they arrive.
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
        Capture, FocusLatch, InputThread, Percentile, Phase, QpcClock, SourceConfig, SourceReader,
        SourceSet, WAKE_HISTOGRAM_LIMIT, WakeHistogram, Waker,
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
    static WAKER: OnceLock<Waker> = OnceLock::new();
    static CLOSED: AtomicBool = AtomicBool::new(false);

    /// `e` and each of its sources on one line, `root: source: ...`.
    fn chain(e: &dyn std::error::Error) -> String {
        let mut text = e.to_string();
        let mut cause = e.source();
        while let Some(c) = cause {
            text.push_str(": ");
            text.push_str(&c.to_string());
            cause = c.source();
        }
        text
    }

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

    /// Sets the latch, then wakes the input thread to see it.
    fn latch() {
        if let Some(latch) = LATCH.get() {
            latch.latch();
        }
        if let Some(waker) = WAKER.get() {
            waker.wake();
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
        /// The phase after every wake.
        phase: Phase,
    }

    impl Echo {
        fn wake(&mut self, sources: &mut SourceSet, now: TimeUs) -> Phase {
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
                return self.phase;
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
            self.phase
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

        let idle = std::env::args().nth(2).as_deref() == Some("idle");
        let waker = match Waker::new() {
            Ok(w) => w,
            Err(e) => {
                eprintln!("{}", chain(&e));
                return ExitCode::FAILURE;
            }
        };
        let _ = WAKER.set(waker.clone());
        let mut echo = Echo {
            readers: None,
            arrivals: 0,
            tripped: false,
            phase: if idle { Phase::Idle } else { Phase::Open },
        };
        let begun = clock.now();
        let spawned = InputThread::spawn(clock, sources, Capture::new(), &waker, move |s, now| {
            echo.wake(s, now)
        });
        let thread = match spawned {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{}", chain(&e));
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
                eprintln!("{}", chain(&e));
                return ExitCode::FAILURE;
            }
        };
        let ran_s = (clock.now().0 - begun.0) as f64 / 1e6;
        let c = report.causes;
        println!(
            "{:>10} us  {} waits in {ran_s:.1} s ({:.0}/s; {} with nothing to do); MMCSS Games: {}",
            clock.now().0,
            report.waits,
            report.waits as f64 / ran_s,
            report.rearms,
            report.mmcss
        );
        println!(
            "            wakes by cause: input {}, event {}, heartbeat {}, rest of a cut drain {}; drains cut {}",
            c.input, c.event, c.heartbeat, c.drain, report.cuts
        );
        println!("            wake intervals: {}", summary(&report.intervals));
        println!(
            "            heartbeat lateness: {}",
            summary(&report.lateness)
        );
        println!(
            "            publication intervals (Open, Tail): {}",
            summary(&report.publications)
        );
        ExitCode::SUCCESS
    }

    /// A histogram's count, p99 and maximum.
    fn summary(h: &WakeHistogram) -> String {
        let p99 = match h.p99() {
            Percentile::At(us) => format!("{us} us"),
            Percentile::Above => format!("above {WAKE_HISTOGRAM_LIMIT} us"),
            Percentile::Empty => "none".to_owned(),
        };
        format!("{} samples, p99 {p99}, max {} us", h.count(), h.max())
    }
}
