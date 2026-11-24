//! The render thread's startup (design.md "Showing the window"): its first
//! screen built again while the window's surface refuses its configuration
//! in a way a build made again may pass, at most a few times and never past
//! the startup's deadline; the wait for it ended by the deadline, a close
//! and a panic, whose message is kept; and its report lines reaching the
//! log (design.md "Log File").

use std::sync::mpsc;

use super::*;
use crate::render_side::{STARTUP_BUILDS, spawn_until, startup_build};

/// A deadline no test reaches.
fn far() -> Instant {
    Instant::now() + Duration::from_secs(3_600)
}

/// A refusal of the window's configuration, `retryable` or not.
fn refused(retryable: bool) -> RenderError {
    RenderError::SurfaceConfigure {
        retryable,
        source: wgpu::Error::Validation {
            source: Box::new(std::io::Error::other("the swap chain was refused (a test)")),
            description: "Surface does not support the adapter's queue family (a test)".into(),
        },
    }
}

/// A screen that draws every frame, the `n`th failing (`None`: none).
struct Still {
    fail_at: Option<u32>,
    frames: u32,
}

impl Screen for Still {
    fn load(
        &mut self,
        _chart: &Chart,
        _section: Option<&eval::Section>,
    ) -> Result<(), RenderError> {
        Ok(())
    }
    fn wait(&self) -> bool {
        true
    }
    fn paced(&self) -> bool {
        false
    }
    fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
        Ok(())
    }
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        Ok(timing(true))
    }
    fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
        Ok(true)
    }
    fn menu(
        &mut self,
        _page: &crate::menu_screen::MenuPage,
        _changed: bool,
    ) -> Result<FrameTiming, RenderError> {
        self.frames += 1;
        if Some(self.frames) == self.fail_at {
            Err(RenderError::SurfaceLost)
        } else {
            Ok(timing(true))
        }
    }
    #[cfg(feature = "dev-tools")]
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        None
    }
}

/// A factory whose builds before the `ok`th give `refusal`'s error, the
/// rest a screen; the builds counted.
fn refusing(ok: u32, refusal: fn() -> RenderError) -> (Arc<AtomicU32>, MakeScreen) {
    let builds = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&builds);
    let make: MakeScreen = Box::new(move |_| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        if n < ok {
            return Err(refusal());
        }
        Ok(Box::new(Still {
            fail_at: None,
            frames: 0,
        }) as Box<dyn Screen>)
    });
    (builds, make)
}

/// Starts a render thread over `make`, its first screen waited for until
/// `deadline` and cancelled once `cancelled` says so.
fn start(
    make: MakeScreen,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<super::super::RenderLink, ScreenError> {
    let clock = QpcClock::start();
    spawn_until(
        make,
        None,
        None,
        clock,
        view(),
        measure(clock),
        Box::new(NoFrames),
        deadline,
        cancelled,
    )
}

/// W4.2: a build the window's surface refused in a way a build made again
/// may pass is built again, and the next build's screen is the render
/// thread's.
#[test]
fn a_refused_configure_is_built_again() {
    let (builds, make) = refusing(2, || refused(true));
    let link = start(make, far(), &|| false).unwrap();
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    assert_eq!(link.exit(), super::super::RenderEnd::Joined);
    // No client area yet, as a window restored from minimized has, is
    // built again too.
    let (builds, make) = refusing(2, || RenderError::NoArea);
    let link = start(make, far(), &|| false).unwrap();
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    link.exit();
}

/// W4.2: refusals past the most builds fail the startup with the last
/// build's refusal.
#[test]
fn refused_configures_fail_the_startup() {
    let (builds, mut make) = refusing(u32::MAX, || refused(true));
    // A deadline far past the builds' pauses, which an unbounded retry
    // would still end.
    let deadline = Instant::now() + Duration::from_secs(5);
    let e = startup_build(
        &mut make,
        None,
        None,
        deadline,
        &AtomicBool::new(false),
        &mut std::thread::sleep,
    )
    .err()
    .unwrap();
    assert!(
        matches!(
            e,
            RenderError::SurfaceConfigure {
                retryable: true,
                ..
            }
        ),
        "{e:?}"
    );
    assert_eq!(builds.load(Ordering::SeqCst), STARTUP_BUILDS);
}

/// W4.2: a refusal no build made again may pass fails at once.
#[test]
fn a_permanent_refusal_is_not_retried() {
    let (builds, mut make) = refusing(u32::MAX, || refused(false));
    let e = startup_build(
        &mut make,
        None,
        None,
        far(),
        &AtomicBool::new(false),
        &mut std::thread::sleep,
    )
    .err()
    .unwrap();
    assert!(
        matches!(
            e,
            RenderError::SurfaceConfigure {
                retryable: false,
                ..
            }
        ),
        "{e:?}"
    );
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}

/// W4.2: a build's other failures are not the surface's refusal and are
/// not built again.
#[test]
fn other_build_errors_are_not_retried() {
    let (builds, mut make) = refusing(u32::MAX, || RenderError::SurfaceLost);
    let e = startup_build(
        &mut make,
        None,
        None,
        far(),
        &AtomicBool::new(false),
        &mut std::thread::sleep,
    )
    .err()
    .unwrap();
    assert!(matches!(e, RenderError::SurfaceLost), "{e:?}");
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}

/// W4.2: no build starts past the deadline: a retry whose pause would end
/// past it is not made, and the refusal is the startup's failure.
#[test]
fn retries_stop_at_the_deadline() {
    let (builds, mut make) = refusing(u32::MAX, || refused(true));
    let deadline = Instant::now() + super::super::STARTUP_RETRY / 2;
    let e = startup_build(
        &mut make,
        None,
        None,
        deadline,
        &AtomicBool::new(false),
        &mut std::thread::sleep,
    )
    .err()
    .unwrap();
    assert!(e.is_retryable_configure(), "{e:?}");
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}

/// A factory whose build waits until the returned sender is used or
/// dropped, at most 10 s.
fn held() -> (mpsc::Sender<()>, MakeScreen) {
    let (release, held) = mpsc::channel::<()>();
    let held = std::sync::Mutex::new(held);
    let make: MakeScreen = Box::new(move |_| {
        // At most 10 s, so a wait that is not ended for it still ends.
        let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(10));
        Ok(Box::new(Still {
            fail_at: None,
            frames: 0,
        }) as Box<dyn Screen>)
    });
    (release, make)
}

/// W3.3: a first screen not built by the deadline fails the startup; the
/// thread is left detached.
#[test]
fn a_startup_build_past_the_deadline_fails() {
    let (release, make) = held();
    let asked = Instant::now();
    let e = start(make, asked + Duration::from_millis(200), &|| false)
        .err()
        .unwrap();
    assert!(matches!(e, ScreenError::NoStart), "{e:?}");
    assert!(asked.elapsed() >= Duration::from_millis(200));
    drop(release);
}

/// W3.3: a close ends the wait for the first screen at once, long before
/// the deadline.
#[test]
fn a_close_ends_the_build_wait() {
    let (release, make) = held();
    let closed = std::sync::atomic::AtomicBool::new(false);
    let asked = Instant::now();
    let e = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            closed.store(true, Ordering::SeqCst);
        });
        start(make, far(), &|| closed.load(Ordering::SeqCst))
            .err()
            .unwrap()
    });
    assert!(matches!(e, ScreenError::Cancelled), "{e:?}");
    assert!(asked.elapsed() < Duration::from_secs(60));
    drop(release);
}

/// W2.1: a panic while the first screen is built is the startup's error,
/// with the panic's message.
#[test]
fn a_startup_panic_is_reported_with_its_message() {
    let make: MakeScreen = Box::new(|_| panic!("the first build fails (a test)"));
    let e = start(make, far(), &|| false).err().unwrap();
    match e {
        ScreenError::PanickedWhileStarting(m) => {
            assert_eq!(m, "the first build fails (a test)");
        }
        e => panic!("{e:?}"),
    }
}

/// W1.1: a frame the render thread failed reaches the log, through its
/// reporter, by the time the thread's end is joined.
#[cfg(feature = "dev-tools")]
#[test]
fn a_failed_frame_reaches_the_log() {
    let lines = crate::result_skin::chart_tests::logged("render: surface lost", || {
        let make: MakeScreen = Box::new(|_| {
            Ok(Box::new(Still {
                fail_at: Some(2),
                frames: 0,
            }) as Box<dyn Screen>)
        });
        let link = start(make, far(), &|| false).unwrap();
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| link.failed()));
        assert_eq!(link.exit(), super::super::RenderEnd::Joined);
    });
    assert_eq!(lines, ["render: surface lost"], "{lines:?}");
}

/// W5.5: a retry whose pause ran past the deadline (a scheduler's delay, a
/// report line held up on stderr) starts no build: the deadline is looked
/// at once more right before the factory is called.
#[test]
fn a_retry_held_past_the_deadline_is_not_built() {
    let (builds, mut make) = refusing(u32::MAX, || refused(true));
    // Far enough for the retry to be planned before the pause.
    let deadline = Instant::now() + super::super::STARTUP_RETRY * 4;
    let mut held = |_| std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    let e = startup_build(
        &mut make,
        None,
        None,
        deadline,
        &AtomicBool::new(false),
        &mut held,
    )
    .err()
    .unwrap();
    assert!(e.is_retryable_configure(), "{e:?}");
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}

/// W5.5: a close during the pause between builds starts no build after
/// it: the startup's wait gives up at the close, and the builder, left
/// detached, looks at that before its next build.
#[test]
fn a_close_during_the_backoff_starts_no_build() {
    let (builds, make) = refusing(u32::MAX, || refused(true));
    let counted = Arc::clone(&builds);
    let e = start(make, far(), &|| counted.load(Ordering::SeqCst) >= 1)
        .err()
        .unwrap();
    assert!(matches!(e, ScreenError::Cancelled), "{e:?}");
    // Past the pause: a builder that did not look would have built again.
    std::thread::sleep(super::super::STARTUP_RETRY * 3);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}

/// A screen whose menu frames wait until the returned sender is used or
/// dropped, at most 10 s: a renderer stuck after its build's reply.
fn stuck() -> (mpsc::Sender<()>, MakeScreen) {
    struct Stuck(std::sync::Mutex<mpsc::Receiver<()>>);
    impl Screen for Stuck {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            let _ = self.0.lock().unwrap().recv_timeout(Duration::from_secs(10));
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }
    let (release, held) = mpsc::channel::<()>();
    let held = std::sync::Mutex::new(Some(held));
    let make: MakeScreen = Box::new(move |_| {
        let rx = held.lock().unwrap().take().unwrap();
        Ok(Box::new(Stuck(std::sync::Mutex::new(rx))) as Box<dyn Screen>)
    });
    (release, make)
}

/// W5.1: a render thread stuck after its build's reply (in its first
/// frame) is waited for at its end only until the instant given, the
/// startup's deadline, and left detached: the outcome is not held up by
/// the render thread's own end bound.
#[test]
fn a_renderer_stuck_after_its_build_reply_is_left_by_the_deadline() {
    let (release, make) = stuck();
    let link = start(make, far(), &|| false).unwrap();
    link.send(RenderMsg::Menu(Box::default()));
    std::thread::sleep(Duration::from_millis(100));
    let by = Instant::now() + Duration::from_millis(200);
    let end = link.exit_by(by);
    let late = Instant::now().saturating_duration_since(by);
    assert_eq!(end, super::super::RenderEnd::Stuck);
    assert!(late < Duration::from_secs(1), "{late:?} past the instant");
    drop(release);
}

/// W5.3: a frame the render thread fails wakes the game at once, so a
/// startup waiting on its first present fails with it rather than at its
/// deadline.
#[test]
fn a_failed_frame_wakes_the_game() {
    let make: MakeScreen = Box::new(|_| {
        Ok(Box::new(Still {
            fail_at: Some(1),
            frames: 0,
        }) as Box<dyn Screen>)
    });
    let link = start(make, far(), &|| false).unwrap();
    let (tx, woken) = mpsc::sync_channel::<()>(8);
    link.set_game_wake(Arc::new(move || {
        let _ = tx.try_send(());
    }));
    link.send(RenderMsg::Menu(Box::default()));
    assert!(
        woken.recv_timeout(Duration::from_secs(10)).is_ok(),
        "the game was not woken"
    );
    assert!(link.failed());
    link.exit();
}

/// W5.7: during a startup build, wgpu's error records are taken into the
/// report lines only from inside the window surface's configure: a record
/// the build makes elsewhere (here before its renderer, as device creation
/// could) is rejected, and the hardware layer's record of a refused swap
/// chain (a second flip-model swap chain on one window) is taken.
#[test]
fn wgpu_errors_are_taken_only_inside_the_configure() {
    let logs = tempfile::tempdir().unwrap();
    crate::log_file::start(logs.path(), "test");
    let lines = crate::test_window::on_own_desktop(|| {
        let popup = crate::test_window::Popup::new(64, 36);
        // The window's one flip-model swap chain.
        let first =
            render::Renderer::new(popup.target(64, 36), render::AdapterChoice::Warp).unwrap();
        let (tx, lines) = mpsc::channel::<String>();
        let (producer, reporter) = crate::render_report::start(Box::new(move |l| {
            let _ = tx.send(l.to_owned());
        }))
        .unwrap();
        crate::render_report::install(producer);
        let target = popup.target(64, 36);
        let mut target = Some(target);
        let mut make: MakeScreen = Box::new(move |_| {
            log::error!(target: "wgpu_core::device", "outside the configure (a test)");
            let t = target.take().ok_or(RenderError::NoArea)?;
            render::Renderer::new(t, render::AdapterChoice::Warp)?;
            Err(RenderError::NoArea)
        });
        let e = startup_build(
            &mut make,
            None,
            None,
            Instant::now(),
            &AtomicBool::new(false),
            &mut std::thread::sleep,
        )
        .err()
        .unwrap();
        assert!(e.is_retryable_configure(), "{e:?}");
        crate::render_report::stderr_only();
        assert!(reporter.join(Duration::from_secs(10)));
        drop(first);
        drop(popup);
        lines.try_iter().collect::<Vec<String>>()
    });
    assert!(
        lines.iter().all(|l| !l.contains("outside the configure")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("render: wgpu_hal")),
        "{lines:?}"
    );
}
