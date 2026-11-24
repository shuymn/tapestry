//! `tapestry keys`: the key config screen (design.md "Input"). It opens the
//! window and the input thread as a play does, but never opens recording: the
//! input thread offers each press to the capture slot, `game`'s flow binds it,
//! and the screen is drawn with the bitmap font. Every role bound writes
//! `device.toml` atomically; Escape or closing the window writes nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use anyhow::Context as _;
use eval::ControlId;
use game::{
    AdjustKeyError, Bound, DeviceSettings, KeyConfigEnd, KeyConfigFlow, KeyConfigs, KeyError,
    KeyScreen, Refusal, Role, capture_keys, load_settings, save_settings,
};
use input::hid::is_axis;
use input::{Capture, DeviceKind, QpcClock};
use render::{AdapterChoice, Renderer, Target};

use crate::controls::Controls;
use crate::end_screen::key_name;
use crate::menu_screen::WindowSink as _;
use crate::window::{Readiness, StartEnd, StartFailure, WindowShown};

/// A bound control's name: a keyboard key by its scan code, a controller's
/// button by its number and its axis by its HID usage, each after its
/// collection number when it has one (`AXIS 2:0030`).
fn name(b: Bound) -> String {
    let collection = match b.control >> 24 {
        0 => String::new(),
        c => format!("{c}:"),
    };
    let control = b.control & 0x00FF_FFFF;
    match b.kind {
        DeviceKind::Keyboard => key_name(Some(b.control)),
        DeviceKind::Controller if is_axis(ControlId(b.control)) => {
            format!("AXIS {collection}{:04X}", control & 0xFFFF)
        }
        DeviceKind::Controller => format!("BUTTON {collection}{control}"),
    }
}

fn role(r: Role) -> String {
    r.to_string().to_uppercase()
}

/// A role's short name, for its row and its prompt.
fn label(r: Role) -> String {
    match r {
        Role::AuxNegative => "AUX -".to_owned(),
        Role::AuxPositive => "AUX +".to_owned(),
        Role::ScratchPositive => "SCRATCH +".to_owned(),
        Role::ScratchNegative => "SCRATCH -".to_owned(),
        r => role(r),
    }
}

fn names(bound: &[Bound]) -> String {
    if bound.is_empty() {
        return "UNBOUND".to_owned();
    }
    bound
        .iter()
        .map(|&b| name(b))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why a press was refused, and what to do.
fn refusal(r: Refusal) -> String {
    match r {
        Refusal::Key(KeyError::Overlap { first, .. }) => {
            format!("ALREADY BOUND TO {}: PRESS ANOTHER", role(first))
        }
        Refusal::Key(KeyError::Reserved { reserved_for, .. }) => format!(
            "RESERVED FOR {}: PRESS ANOTHER",
            reserved_for.to_uppercase()
        ),
        Refusal::Key(KeyError::NotAKeyboardKey(r)) => {
            format!("{} TAKES KEYBOARD KEYS ONLY: PRESS A KEY", role(r))
        }
        Refusal::Key(KeyError::NoSuchLane(l)) => format!("LANE {l} DOES NOT EXIST"),
        Refusal::Key(KeyError::Adjustment(e)) => match e {
            AdjustKeyError::Overlap { first, .. } => format!(
                "ALREADY BOUND TO {}: PRESS ANOTHER",
                first.to_string().to_uppercase()
            ),
            AdjustKeyError::Reserved { reserved_for, .. } => format!(
                "RESERVED FOR {}: PRESS ANOTHER",
                reserved_for.to_uppercase()
            ),
            AdjustKeyError::Axis(_) => "TAKES A KEY OR A BUTTON, NOT AN AXIS".to_owned(),
            AdjustKeyError::NotScratch { .. } => {
                "NOT A SCRATCH KEY OR BUTTON: PRESS ONE OF THEM".to_owned()
            }
            AdjustKeyError::TooMany => "TOO MANY ADJUSTMENT CONTROLS".to_owned(),
        },
        Refusal::NothingToKeep(r) => {
            format!("{} HAS NO BINDING TO KEEP: PRESS A CONTROL", role(r))
        }
        Refusal::OtherController => "ANOTHER CONTROLLER IS BOUND: USE THE SAME ONE".to_owned(),
        Refusal::UnknownController => {
            "THIS CONTROLLER CANNOT BE TOLD APART: PRESS ANOTHER".to_owned()
        }
        Refusal::NotATurn => "TURN THE TURNTABLE, OR F12 TO KEEP".to_owned(),
    }
}

/// The screen's lines for `flow`.
pub fn lines(flow: &KeyConfigFlow) -> Vec<String> {
    let mut l = vec!["KEY CONFIG".to_owned(), String::new()];
    // The prompt, then one line of detail: a refusal, or the scratch
    // axis's range being learnt or learnt.
    let detail = flow.refused().map_or_else(String::new, refusal);
    if let Some(seen) = flow.learning() {
        l.push("TURN THE TURNTABLE A FEW FULL TURNS EACH WAY (F12: SKIP)".to_owned());
        l.push(seen.map_or_else(
            || "WAITING FOR THE TURNTABLE".to_owned(),
            |w| {
                format!(
                    "DECLARED {}..{}, SEEN {}..{}",
                    w.cap.min, w.cap.max, w.lo, w.hi
                )
            },
        ));
    } else if flow.turning() {
        l.push("TURN THE TURNTABLE THE WAY THAT SCROLLS MENUS DOWN (F12: KEEP)".to_owned());
        l.push(String::new());
    } else if flow.reversing() {
        l.push(
            "TURN THE TURNTABLE THE WAY THAT RAISES SUDDEN+ AND SPEEDS UP (F12: KEEP)".to_owned(),
        );
        l.push(detail);
    } else {
        match flow.role() {
            Some(r @ (Role::Retry | Role::Quit)) => {
                l.push(format!("PRESS THE KEYBOARD KEY FOR {}", role(r)));
            }
            Some(Role::Lane(0)) if flow.other_direction() => l.push(
                "PRESS THE KEY OR BUTTON FOR THE OTHER SCRATCH DIRECTION (F12: NONE)".to_owned(),
            ),
            Some(Role::Lane(0)) => l.push(
                "PRESS THE KEY OR BUTTON FOR ONE SCRATCH DIRECTION, OR TURN THE AXIS".to_owned(),
            ),
            Some(Role::ScratchPositive) => l.push(format!(
                "PRESS THE SCRATCH {} THAT RAISES SUDDEN+ AND SPEEDS UP (F12: KEEP)",
                match flow.choosing() {
                    Some(DeviceKind::Controller) => "BUTTON",
                    _ => "KEY",
                }
            )),
            Some(r @ (Role::AuxNegative | Role::AuxPositive)) => l.push(format!(
                "PRESS THE KEY OR BUTTON FOR {} ({})",
                label(r),
                role(r)
            )),
            Some(r) => l.push(format!("PRESS THE KEY OR BUTTON FOR {}", role(r))),
            None => l.push("DONE".to_owned()),
        }
        l.push(match (detail.is_empty(), flow.learnt()) {
            (true, Some(r)) => format!(
                "SCRATCH DECLARED {}..{}, SEEN {}..{}, USING {}",
                r.declared.0, r.declared.1, r.observed.0, r.observed.1, r.wrap
            ),
            _ => detail,
        });
    }
    l.push(String::new());
    for (i, r) in flow.steps().iter().enumerate() {
        let value = match flow.bound().get(i) {
            Some((_, b)) => names(b),
            None if i == flow.bound().len() => "<".to_owned(),
            None => format!("(NOW {})", names(&flow.current(*r))),
        };
        let label = if i == 1 && *r == Role::Lane(0) {
            "SCRATCH 2".to_owned()
        } else {
            label(*r)
        };
        l.push(format!("{label:<8} {value}"));
    }
    l.push(String::new());
    l.push("ESCAPE: CANCEL, NOTHING IS SAVED".to_owned());
    l.push("F12: KEEP THE CURRENT BINDING".to_owned());
    l
}

/// The window's screen.
struct Screen<'a> {
    renderer: Renderer,
    controls: &'a Controls,
    /// Set by a presented frame: the hidden window shows then.
    presented: Arc<AtomicBool>,
    /// The startup's outcome, which a failed first frame fails.
    shown: WindowShown,
}

impl KeyScreen for Screen<'_> {
    fn show(&mut self, flow: &KeyConfigFlow) {
        let lines = lines(flow);
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let drawn = self.renderer.text_frame(&refs);
        framed(drawn, &self.presented, &self.shown, self.controls);
    }

    fn wait(&mut self) {
        wait_frame(&self.renderer);
    }

    fn closed(&self) -> bool {
        self.controls.quitting()
    }
}

/// What a key config frame `drawn` tells: a present the swap chain
/// confirmed tells the window (`presented`), which shows then; a failed
/// frame loses the screen, so the key config ends as if the window closed,
/// and before the first present it fails the startup with why (`shown`,
/// design.md "Showing the window"), which fails the run.
fn framed(
    drawn: Result<render::FrameTiming, render::RenderError>,
    presented: &AtomicBool,
    shown: &WindowShown,
    controls: &Controls,
) {
    match drawn {
        Ok(t) if t.confirmed() => presented.store(true, Ordering::Release),
        Ok(_) => {}
        Err(e) => {
            let line = format!("render: {}", game::Chain(&e));
            eprintln!("{line}");
            shown.fail(StartFailure::Render(line));
            controls.close();
        }
    }
}

/// Waits for `renderer`'s next frame, or about one at 60 Hz without its
/// waitable.
fn wait_frame(renderer: &Renderer) {
    if !renderer.has_frame_waitable() || !renderer.wait_for_frame(100) {
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

/// How the key config's first screen ended ([`first_screen`]).
enum FirstScreen<R> {
    /// The window was shown at its first present: the renderer, handed
    /// back.
    Shown(R),
    /// The startup ended without it (`end`); `left` when its worker had
    /// not ended by the deadline, so the window it may still draw into
    /// stays open.
    Ended { end: StartEnd, left: bool },
}

/// Whether the startup `shown` has no outcome yet.
fn starting(shown: &WindowShown) -> bool {
    matches!(shown.readiness(), Readiness::Pending | Readiness::Showing)
}

/// What the key config's first screen worker hands the game thread as it
/// ends ([`first_screen`]).
enum Handoff<R> {
    /// The window was shown: the renderer, for the key config.
    Shown(R),
    /// The startup ended without it, and the worker has dropped its
    /// renderer, if it made one; why the worker failed, if it did.
    Ended(Option<String>),
}

/// How often the worker looks for the startup's outcome once its first
/// present was counted.
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(2);

/// Makes the key config's renderer through `make` and draws its first
/// screen through `frame`, `pause` between frames, on a worker `spawn`
/// starts (design.md "Showing the window"): a driver stalled in the first
/// frame holds the worker, not the game thread. A frame tells the window of
/// its present and fails the startup when it fails ([`framed`]); a renderer
/// not made fails it with the error, and the worker's panic with its
/// message. Once a present is counted the worker draws no more and waits
/// for the outcome, so nothing it does then can stall in the driver. The
/// game thread waits for the outcome ([`WindowShown::wait_ready`]), woken
/// by the worker's end and the window's: the first present (the worker
/// hands the renderer back), a close, a failure, or the deadline. A worker
/// that cannot be started fails the startup with why. Without a present,
/// the worker drops its renderer itself, so a surface's destruction that
/// waits on the driver holds the worker only, and says when it is done;
/// the game thread waits for that until the deadline only, the worker then
/// left (`left`) with the window it may still use.
fn first_screen<R: Send + 'static>(
    shown: &WindowShown,
    controls: &Arc<Controls>,
    presented: Arc<AtomicBool>,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    make: impl FnOnce() -> Result<R, render::RenderError> + Send + 'static,
    mut frame: impl FnMut(&mut R) -> Result<render::FrameTiming, render::RenderError> + Send + 'static,
    mut pause: impl FnMut(&R) + Send + 'static,
) -> FirstScreen<R> {
    let (tx, rx) = mpsc::sync_channel::<Handoff<R>>(1);
    let worker_shown = shown.clone();
    let worker_controls = Arc::clone(controls);
    let worker = move || {
        let drawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut r = make().map_err(|e| format!("render: {}", game::Chain(&e)))?;
            let open = || starting(&worker_shown) && !worker_controls.quitting();
            let counted = || presented.load(Ordering::Acquire);
            while open() && !counted() {
                framed(frame(&mut r), &presented, &worker_shown, &worker_controls);
                if open() && !counted() {
                    pause(&r);
                }
            }
            // A counted present: the window settles the outcome by the
            // deadline at the latest, and no more frames are drawn.
            while open() {
                std::thread::sleep(SETTLE_POLL);
            }
            if worker_shown.readiness() == Readiness::FirstPresent {
                return Ok(Some(r));
            }
            drop(r);
            Ok(None)
        }));
        let handoff = match drawn {
            Ok(Ok(Some(r))) => Handoff::Shown(r),
            Ok(Ok(None)) => Handoff::Ended(None),
            Ok(Err(why)) => Handoff::Ended(Some(why)),
            Err(payload) => Handoff::Ended(Some(format!(
                "the render thread panicked while starting: {}",
                game::coord::JobPanic::of(&*payload).0
            ))),
        };
        if let Handoff::Ended(Some(why)) = &handoff {
            eprintln!("{why}");
            worker_shown.fail(StartFailure::Render(why.clone()));
        }
        let _ = tx.send(handoff);
        worker_controls.wake_game();
    };
    if let Err(e) = spawn(Box::new(worker)) {
        let why = format!("the render thread cannot start: {e}");
        eprintln!("{why}");
        shown.fail(StartFailure::Render(why));
    }
    let end = match shown.wait_ready(&|| controls.quitting(), &|| None) {
        Ok(()) => {
            // The worker draws nothing once the present was counted, so
            // its handoff follows the outcome within its poll.
            let end = match rx.recv() {
                Ok(Handoff::Shown(r)) => return FirstScreen::Shown(r),
                Ok(Handoff::Ended(Some(why))) => StartEnd::Failed(StartFailure::Render(why)),
                Ok(Handoff::Ended(None)) if controls.quitting() => StartEnd::Closed,
                Ok(Handoff::Ended(None)) | Err(mpsc::RecvError) => StartEnd::Failed(
                    StartFailure::Render("the render thread ended while starting".to_owned()),
                ),
            };
            return FirstScreen::Ended { end, left: false };
        }
        Err(end) => end,
    };
    // The worker's end, its renderer dropped there, waited for until the
    // deadline only.
    let left = rx
        .recv_timeout(
            shown
                .deadline()
                .saturating_duration_since(std::time::Instant::now()),
        )
        .is_err_and(|e| e == mpsc::RecvTimeoutError::Timeout);
    FirstScreen::Ended { end, left }
}

/// Puts a finished key config into `device`: the bindings, the scratch
/// axis's revolution when it was learnt beyond its declared range, and its
/// menu and adjustment directions when it was turned for them (else the
/// current ones stay).
fn apply(
    device: &mut DeviceSettings,
    keys: KeyConfigs,
    widened: Option<u32>,
    menu_reversed: Option<bool>,
    adjust_reversed: Option<bool>,
) {
    device.keyboard = keys.keyboard;
    device.controller = keys.controller;
    device.controller_device = keys.controller_device.map(Into::into);
    if let Some(wrap) = widened {
        device.scratch.wrap = Some(wrap);
        println!(
            "keys: the scratch axis reported beyond its declared range; its revolution is set to {wrap} values"
        );
    }
    if let Some(reversed) = menu_reversed {
        device.scratch.menu_reversed = reversed;
    }
    if let Some(reversed) = adjust_reversed {
        device.scratch.adjust_reversed = reversed;
    }
}

/// Runs the key config screen. `script` (dev-tools) offers presses in
/// place of a person.
///
/// # Errors
///
/// Why the data root, `device.toml`, the window, the input thread or the
/// screen cannot be used, or the file cannot be written.
pub fn run(
    clock: QpcClock,
    #[cfg(feature = "dev-tools")] script: Option<crate::dev::KeyScript>,
) -> anyhow::Result<()> {
    let root = crate::data_root()?;
    let path = root.device_settings();
    // Read without the key config checks, so a key config that `play`
    // refuses can be set again here.
    let mut device: DeviceSettings =
        load_settings(&path).with_context(|| path.display().to_string())?;
    let controls = Arc::new(Controls::new(clock));
    // The startup's wait parks this thread: the window's outcome and the
    // render worker's end wake it.
    controls.set_game(std::thread::current());
    let capture = Capture::new();
    let waker = input::Waker::new().context("input")?;
    // Nothing reads the thread's publications here: it wakes for input
    // alone, which it offers to the capture.
    let (window_thread, surface, input) = crate::open_window_and_input(
        &controls,
        clock,
        crate::sources(),
        capture.clone(),
        &waker,
        |_, _| input::Phase::Idle,
        (None, None),
        device.monitor.clone(),
        None,
    )?;
    let target = Target::Window {
        window: surface.into(),
        width: surface.width,
        height: surface.height,
    };
    let shown = window_thread.shown();
    let presented = window_thread.link().presented();
    let first = lines(&KeyConfigFlow::new(KeyConfigs::from(&device)));
    let started = first_screen(
        &shown,
        &controls,
        Arc::clone(&presented),
        |worker| {
            std::thread::Builder::new()
                .name("render".into())
                .spawn(worker)
                .map(drop)
        },
        move || {
            // The startup faults a menu's render thread takes (dev-tools):
            // a refused configure, a build held or a panic in it.
            #[cfg(feature = "dev-tools")]
            {
                crate::dev::read_render_faults();
                crate::dev::startup_faults();
            }
            Renderer::new(target, AdapterChoice::HighPerformance)
        },
        move |r| {
            let refs: Vec<&str> = first.iter().map(String::as_str).collect();
            r.text_frame(&refs)
        },
        wait_frame,
    );
    let renderer = match started {
        FirstScreen::Shown(r) => r,
        // Never shown: a failure fails the key config, a close ends it,
        // through a failed startup's end, as a play's: the input thread
        // left unjoined, the window thread waited for until the deadline,
        // and a worker not ended by then reported as a render thread that
        // did not end, the window it may still draw into kept open.
        FirstScreen::Ended { end, left } => {
            if end == StartEnd::Closed {
                println!("keys: the window closed, nothing written");
            }
            let rendered = if left {
                crate::render_side::RenderEnd::Stuck
            } else {
                crate::render_side::RenderEnd::Joined
            };
            return crate::end_startup(
                end,
                shown.deadline(),
                crate::InputThreads::raw(input),
                |_| {},
                |_| rendered,
                |released, by| window_thread.close_by(released, by),
            );
        }
    };
    #[cfg(feature = "dev-tools")]
    let script = script.map(|s| s.spawn(capture.clone(), Arc::clone(&controls)));
    let mut screen = Screen {
        renderer,
        controls: &controls,
        presented,
        shown,
    };
    let current = KeyConfigs::from(&device);
    let end = capture_keys(&capture, &mut screen, current, device.scratch.wrap);
    drop(screen);
    #[cfg(feature = "dev-tools")]
    drop(script);
    // Not joined, as in `play`.
    input.detach();
    window_thread.close();
    match end {
        KeyConfigEnd::Done(keys, widened, menu_reversed, adjust_reversed) => {
            apply(&mut device, keys, widened, menu_reversed, adjust_reversed);
            save_settings(&path, &device).with_context(|| path.display().to_string())?;
            println!("keys: saved to {}", path.display());
        }
        KeyConfigEnd::Cancelled => println!("keys: cancelled, nothing written"),
        KeyConfigEnd::Closed => println!("keys: the window closed, nothing written"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use eval::ControlId;
    use game::{
        Bound, DeviceSettings, KeyConfig, KeyConfigEnd, KeyConfigFlow, KeyConfigs, KeyScreen,
        capture_keys_within,
    };
    use input::{Capture, DeviceId, DeviceKind, Packet};

    use super::{apply, lines, name};

    const MODEL: input::hid::HidProduct = input::hid::HidProduct {
        vendor_id: 0x1A2B,
        product_id: 0x3C4D,
        interface: None,
    };
    const AXIS: u32 = 0x1_0030;

    /// Offers one packet per wait, as the input thread offers them: a
    /// keyboard key (`Ok`) or the scratch axis turned (`Err`, its value's
    /// sign the direction). Keeps the lines shown while the menu direction
    /// and the adjustment direction are asked for.
    struct Scripted {
        capture: Capture,
        steps: std::vec::IntoIter<Result<u32, i32>>,
        asked: Vec<String>,
        reversing: Vec<String>,
    }

    impl KeyScreen for Scripted {
        fn show(&mut self, flow: &KeyConfigFlow) {
            if flow.turning() {
                self.asked = lines(flow);
            }
            if flow.reversing() {
                self.reversing = lines(flow);
            }
        }

        fn wait(&mut self) {
            let (control, value, kind) = match self.steps.next().expect("the script ran out") {
                Ok(key) => (key, 1, DeviceKind::Keyboard),
                Err(value) => (AXIS, value, DeviceKind::Controller),
            };
            let packet = Packet {
                device: DeviceId(3),
                control: ControlId(control),
                value,
                background: false,
            };
            match kind {
                DeviceKind::Keyboard => self.capture.offer(&packet, kind, false),
                DeviceKind::Controller => {
                    self.capture.offer_controller(&packet, Some(MODEL), false)
                }
            }
        }

        fn closed(&self) -> bool {
            false
        }
    }

    /// Key config with the scratch bound to the axis, its range skipped
    /// (F12), `answer` at the menu direction prompt, the rest kept, and
    /// `adjust` at the adjustment direction prompt, written over `device` as
    /// `tapestry keys` writes it. The lines of both prompts.
    fn configured(
        answer: Result<u32, i32>,
        adjust: Result<u32, i32>,
        device: &mut DeviceSettings,
    ) -> (Vec<String>, Vec<String>) {
        let capture = Capture::new();
        let mut steps = vec![Err(1), Ok(game::KEEP_KEY), answer];
        steps.extend([Ok(game::KEEP_KEY); 13]);
        steps.push(adjust);
        let mut screen = Scripted {
            capture: capture.clone(),
            steps: steps.into_iter(),
            asked: Vec::new(),
            reversing: Vec::new(),
        };
        let current = KeyConfigs::from(&*device);
        let end = capture_keys_within(
            &capture,
            &mut screen,
            current,
            device.scratch.wrap,
            Duration::ZERO,
            Duration::ZERO,
        );
        let KeyConfigEnd::Done(keys, widened, menu_reversed, adjust_reversed) = end else {
            panic!("{end:?}");
        };
        apply(device, keys, widened, menu_reversed, adjust_reversed);
        assert_eq!(device.validate(), Ok(()));
        (screen.asked, screen.reversing)
    }

    /// A key config written by `tapestry keys` keeps `speculation` as
    /// `device.toml` had it: absent, `true` or `false`.
    #[test]
    fn keys_keep_speculation_as_read() {
        let keep = Ok(game::KEEP_KEY);
        let dir = tempfile::tempdir().unwrap();
        let root = game::DataRoot::new(dir.path().to_owned());
        for speculation in [None, Some(true), Some(false)] {
            let mut device = DeviceSettings {
                speculation,
                ..DeviceSettings::default()
            };
            configured(keep, keep, &mut device);
            game::save_settings(&root.device_settings(), &device).unwrap();
            let read = game::Settings::load(&root).unwrap().device;
            assert_eq!(read.speculation, speculation);
        }
    }

    /// The axis turned toward lower values at the prompt writes the menu
    /// direction reversed, toward higher values not reversed, and F12
    /// keeps what `device.toml` had.
    #[test]
    fn key_config_writes_the_menu_direction_turned() {
        let keep = Ok(game::KEEP_KEY);
        let mut device = DeviceSettings::default();
        let (asked, _) = configured(Err(-1), keep, &mut device);
        assert_eq!(
            asked[2],
            "TURN THE TURNTABLE THE WAY THAT SCROLLS MENUS DOWN (F12: KEEP)"
        );
        assert!(device.scratch.menu_reversed);
        configured(keep, keep, &mut device);
        assert!(device.scratch.menu_reversed, "F12 keeps it");
        configured(Err(1), keep, &mut device);
        assert!(!device.scratch.menu_reversed);
        configured(keep, keep, &mut device);
        assert!(!device.scratch.menu_reversed, "F12 keeps it");
        assert!(!device.scratch.adjust_reversed);
    }

    /// The axis turned toward lower values at the last prompt writes the
    /// adjustment direction reversed, toward higher values not reversed,
    /// and F12 keeps what `device.toml` had; the menu direction is apart.
    #[test]
    fn key_config_writes_the_adjustment_direction_turned() {
        let keep = Ok(game::KEEP_KEY);
        let mut device = DeviceSettings::default();
        let (_, reversing) = configured(Err(1), Err(-1), &mut device);
        assert_eq!(
            reversing[2],
            "TURN THE TURNTABLE THE WAY THAT RAISES SUDDEN+ AND SPEEDS UP (F12: KEEP)"
        );
        assert!(device.scratch.adjust_reversed);
        assert!(!device.scratch.menu_reversed);
        configured(keep, keep, &mut device);
        assert!(device.scratch.adjust_reversed, "F12 keeps it");
        configured(keep, Err(1), &mut device);
        assert!(!device.scratch.adjust_reversed);
    }

    /// After quit: START, SELECT, the auxiliary keys, then the positive
    /// direction of a scratch on two buttons, each named on its row.
    #[test]
    fn the_screen_prompts_for_the_adjustment_roles() {
        let kb = |control| Bound {
            kind: DeviceKind::Keyboard,
            control,
        };
        let pad = |control| Bound {
            kind: DeviceKind::Controller,
            control,
        };
        let model = Some(MODEL);
        let mut flow = KeyConfigFlow::new(KeyConfigs::from(&DeviceSettings::default()));
        flow.press_from(pad(40), model);
        flow.press_from(pad(41), model);
        for _ in 0..9 {
            flow.press(kb(game::KEEP_KEY));
        }
        let l = lines(&flow);
        assert_eq!(l[2], "PRESS THE KEY OR BUTTON FOR START");
        assert_eq!(l[16], "START    <");
        assert_eq!(l[17], "SELECT   (NOW KEY 0011)");
        assert_eq!(l[18], "AUX -    (NOW KEY E048)");
        assert_eq!(l[19], "AUX +    (NOW KEY E050)");
        // START on a lane key is refused.
        flow.press(kb(0x2c));
        assert_eq!(lines(&flow)[3], "ALREADY BOUND TO KEY 1: PRESS ANOTHER");
        flow.press(kb(game::KEEP_KEY));
        flow.press(kb(game::KEEP_KEY));
        assert_eq!(
            lines(&flow)[2],
            "PRESS THE KEY OR BUTTON FOR AUX - (THE AUXILIARY NEGATIVE KEY)"
        );
        flow.press(kb(game::KEEP_KEY));
        flow.press(kb(game::KEEP_KEY));
        let l = lines(&flow);
        assert_eq!(
            l[2],
            "PRESS THE SCRATCH BUTTON THAT RAISES SUDDEN+ AND SPEEDS UP (F12: KEEP)"
        );
        assert_eq!(l[20], "SCRATCH + <");
        flow.press_from(pad(3), model);
        assert_eq!(
            lines(&flow)[3],
            "NOT A SCRATCH KEY OR BUTTON: PRESS ONE OF THEM"
        );
        assert!(matches!(
            flow.press_from(pad(41), model),
            game::Press::Done(_)
        ));
        assert_eq!(lines(&flow)[20], "SCRATCH + BUTTON 41");
    }

    #[test]
    fn the_screen_shows_prompt_refusal_and_bindings() {
        let mut flow = KeyConfigFlow::new(KeyConfigs {
            keyboard: DeviceSettings::default().keyboard,
            controller: KeyConfig {
                lanes: Vec::new(),
                retry: None,
                quit: None,
                ..KeyConfig::default()
            },
            controller_device: None,
        });
        let pad = Bound {
            kind: DeviceKind::Controller,
            control: 12,
        };
        let model = Some(input::hid::HidProduct {
            vendor_id: 0x1A2B,
            product_id: 0x3C4D,
            interface: None,
        });
        // A controller that cannot be told apart is refused.
        flow.press(pad);
        assert_eq!(
            lines(&flow)[3],
            "THIS CONTROLLER CANNOT BE TOLD APART: PRESS ANOTHER"
        );
        flow.press_from(pad, model);
        flow.press_from(pad, model);
        let l = lines(&flow);
        assert_eq!(
            l[2],
            "PRESS THE KEY OR BUTTON FOR THE OTHER SCRATCH DIRECTION (F12: NONE)"
        );
        assert_eq!(l[3], "ALREADY BOUND TO SCRATCH: PRESS ANOTHER");
        assert_eq!(l[5], "SCRATCH  BUTTON 12");
        assert_eq!(l[6], "SCRATCH 2 <");
        assert_eq!(l[7], "KEY 1    (NOW KEY 002C)");
        assert_eq!(l[14], "RETRY    (NOW BACKSPACE)");
        assert_eq!(l[15], "QUIT     (NOW ESCAPE)");
        // F12 skips the other direction.
        flow.press(Bound {
            kind: DeviceKind::Keyboard,
            control: game::KEEP_KEY,
        });
        assert_eq!(lines(&flow)[6], "SCRATCH 2 UNBOUND");
        for k in [0x2c, 0x1f, 0x2d, 0x20, 0x2e, 0x21, 0x2f] {
            flow.press(Bound {
                kind: DeviceKind::Keyboard,
                control: k,
            });
        }
        flow.press_from(pad, model);
        let l = lines(&flow);
        assert_eq!(l[2], "PRESS THE KEYBOARD KEY FOR RETRY");
        assert_eq!(l[3], "RETRY TAKES KEYBOARD KEYS ONLY: PRESS A KEY");
    }

    #[test]
    fn the_scratch_prompt_takes_an_axis_named_by_usage() {
        let flow = KeyConfigFlow::new(KeyConfigs {
            keyboard: DeviceSettings::default().keyboard,
            controller: KeyConfig {
                lanes: Vec::new(),
                retry: None,
                quit: None,
                ..KeyConfig::default()
            },
            controller_device: None,
        });
        assert_eq!(
            lines(&flow)[2],
            "PRESS THE KEY OR BUTTON FOR ONE SCRATCH DIRECTION, OR TURN THE AXIS"
        );
        let axis = Bound {
            kind: DeviceKind::Controller,
            control: 0x1_0030,
        };
        assert_eq!(name(axis), "AXIS 0030");
        let second = |control| Bound {
            kind: DeviceKind::Controller,
            control,
        };
        assert_eq!(name(second(0x0201_0030)), "AXIS 2:0030");
        assert_eq!(name(second(0x0200_0005)), "BUTTON 2:5");
        assert_eq!(name(second(5)), "BUTTON 5");
    }

    /// Key config's frame tells the hidden window it was presented, as a
    /// menu's or a play's does, once the swap chain counted it: its window
    /// shows then. An offscreen frame, which no swap chain counts, does
    /// not (design.md "Showing the window").
    #[test]
    fn a_key_config_frame_tells_the_window() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use render::{AdapterChoice, Renderer, Target};

        let flow = KeyConfigFlow::new(KeyConfigs {
            keyboard: DeviceSettings::default().keyboard,
            controller: DeviceSettings::default().controller,
            controller_device: None,
        });
        let told = |target: Target| {
            let controls = crate::controls::Controls::new(input::QpcClock::start());
            let presented = Arc::new(AtomicBool::new(false));
            let mut screen = super::Screen {
                renderer: Renderer::new(target, AdapterChoice::Warp).unwrap(),
                controls: &controls,
                presented: Arc::clone(&presented),
                shown: crate::window::WindowShown::settled(crate::window::Readiness::Pending),
            };
            assert!(!presented.load(Ordering::Acquire));
            // A swap chain may skip its first frames; a few are enough.
            for _ in 0..10 {
                screen.show(&flow);
                if presented.load(Ordering::Acquire) {
                    break;
                }
            }
            assert!(!controls.quitting());
            presented.load(Ordering::Acquire)
        };
        let offscreen = Target::Offscreen {
            width: 64,
            height: 36,
        };
        assert!(!told(offscreen), "an offscreen frame told the window");
        let window = crate::test_window::on_own_desktop(|| {
            let popup = crate::test_window::Popup::new(64, 36);
            let told = told(popup.target(64, 36));
            drop(popup);
            told
        });
        assert!(window, "a window's frame did not tell it");
    }

    /// W5.4: a key config frame that fails before the first present fails
    /// the startup with why, so the run fails rather than ending as a
    /// close; one failing after it ends the key config as a close.
    #[test]
    fn a_failed_first_key_config_frame_fails_the_startup() {
        use std::sync::atomic::AtomicBool;

        use crate::window::{Readiness, StartFailure, WindowShown};

        let controls = crate::controls::Controls::new(input::QpcClock::start());
        let shown = WindowShown::settled(Readiness::Pending);
        let presented = AtomicBool::new(false);
        super::framed(
            Err(render::RenderError::SurfaceLost),
            &presented,
            &shown,
            &controls,
        );
        assert_eq!(
            shown.readiness(),
            Readiness::Failed(StartFailure::Render("render: surface lost".to_owned()))
        );
        assert!(controls.quitting());
        // Past the first present: a close only.
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        let shown = WindowShown::settled(Readiness::FirstPresent);
        super::framed(
            Err(render::RenderError::SurfaceLost),
            &presented,
            &shown,
            &controls,
        );
        assert_eq!(shown.readiness(), Readiness::FirstPresent);
        assert!(controls.quitting());
    }

    /// A frame the swap chain counted, or one it did not (`confirmed`).
    fn frame_timing(confirmed: bool) -> render::FrameTiming {
        render::FrameTiming {
            cpu: Duration::ZERO,
            sprites: 0,
            dropped: 0,
            outcome: render::FrameOutcome::Shown,
            present_count: confirmed.then_some(1),
            movies: [false; skin_model::movie::SLOTS],
        }
    }

    /// Starts a first screen's worker on a thread of its own.
    fn spawned(worker: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
        std::thread::Builder::new().spawn(worker).map(drop)
    }

    /// The controls of a key config, its game thread the caller.
    fn game_controls() -> std::sync::Arc<crate::controls::Controls> {
        let c = crate::controls::Controls::new(input::QpcClock::start());
        c.set_game(std::thread::current());
        std::sync::Arc::new(c)
    }

    /// The startup's end of a first screen that was not shown.
    fn ended<R>(started: super::FirstScreen<R>) -> (crate::window::StartEnd, bool) {
        match started {
            super::FirstScreen::Ended { end, left } => (end, left),
            super::FirstScreen::Shown(_) => panic!("the first screen was shown"),
        }
    }

    /// W6.3: the key config's first frame is drawn on a worker until the
    /// startup has an outcome: a frame stalled in the driver holds the
    /// worker, and the game thread's wait still ends at the deadline,
    /// failing the startup there, the worker left running.
    #[test]
    fn a_first_key_config_frame_stalled_in_the_driver_ends_by_the_deadline() {
        use std::sync::atomic::AtomicU32;
        use std::time::Instant;

        use crate::window::{StartEnd, StartFailure, WindowShown};

        let controls = game_controls();
        let bound = Duration::from_millis(200);
        let shown = WindowShown::pending_for(bound);
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = std::sync::Mutex::new(held);
        let frames = AtomicU32::new(0);
        // The first frame stalls (at most 10 s); any later one fails, so
        // a first frame drawn on the game thread ends too.
        let frame = move |(): &mut ()| {
            if frames.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(10));
                return Ok(frame_timing(false));
            }
            Err(render::RenderError::SurfaceLost)
        };
        let started = super::first_screen(
            &shown,
            &controls,
            std::sync::Arc::default(),
            spawned,
            || Ok(()),
            frame,
            |()| {},
        );
        let late = Instant::now().saturating_duration_since(shown.deadline());
        drop(release);
        let (end, left) = ended(started);
        assert_eq!(end, StartEnd::Failed(StartFailure::NoFrame(bound)));
        assert!(left, "a worker stalled in its frame was not left");
        assert!(late < Duration::from_secs(1), "{late:?} past the deadline");
    }

    /// W6.3: the first screen's wait sees a close at once: the startup
    /// ends as a close long before its deadline, and the worker, which
    /// stops drawing at the close, hands its renderer back.
    #[test]
    fn a_close_ends_the_first_key_config_screen() {
        use std::time::Instant;

        use crate::window::{StartEnd, WindowShown};

        let controls = game_controls();
        let shown = WindowShown::pending_for(Duration::from_secs(10));
        let closer = std::sync::Arc::clone(&controls);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            closer.close();
        });
        let asked = Instant::now();
        let started = super::first_screen(
            &shown,
            &controls,
            std::sync::Arc::default(),
            spawned,
            || Ok(()),
            |()| Ok(frame_timing(false)),
            |()| std::thread::sleep(Duration::from_millis(5)),
        );
        let (end, left) = ended(started);
        assert_eq!(end, StartEnd::Closed);
        assert!(!left, "a worker that stopped at the close was left");
        assert!(
            asked.elapsed() < Duration::from_secs(5),
            "{:?}",
            asked.elapsed()
        );
    }

    /// W6.3: the first screen's failures are told apart from the deadline,
    /// each failing the startup at once with why: a worker that cannot
    /// start, a renderer not made, and the worker's panic with its message;
    /// a counted present shows the window, and the worker hands its
    /// renderer back.
    #[test]
    fn the_first_key_config_screens_failures_are_told_apart() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Instant;

        use crate::window::{StartEnd, StartFailure, WindowShown};

        let render = |why: &str| StartEnd::Failed(StartFailure::Render(why.to_owned()));
        let controls = game_controls();
        let first = |spawn: fn(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
                     make: fn() -> Result<(), render::RenderError>| {
            let shown = WindowShown::pending_for(Duration::from_secs(10));
            let asked = Instant::now();
            let (end, left) = ended(super::first_screen(
                &shown,
                &controls,
                std::sync::Arc::default(),
                spawn,
                make,
                // No frame comes before these failures; one drawn would
                // fail the startup with a reason of its own.
                |()| Err(render::RenderError::SurfaceLost),
                |()| {},
            ));
            assert!(!left);
            assert!(asked.elapsed() < Duration::from_secs(5), "{end:?}");
            end
        };
        assert_eq!(
            first(
                |_| Err(std::io::Error::other("no threads (a test)")),
                || Ok(())
            ),
            render("the render thread cannot start: no threads (a test)")
        );
        assert_eq!(
            first(spawned, || Err(render::RenderError::SurfaceLost)),
            render("render: surface lost")
        );
        assert_eq!(
            first(spawned, || panic!("the renderer fails (a test)")),
            render("the render thread panicked while starting: the renderer fails (a test)")
        );

        let shown = WindowShown::pending_for(Duration::from_secs(10));
        let presented = std::sync::Arc::new(AtomicBool::new(false));
        let window_side = (std::sync::Arc::clone(&presented), shown.clone());
        // The window thread's part: the show at the counted present.
        std::thread::spawn(move || {
            let (presented, shown) = window_side;
            while !presented.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(shown.show_now());
        });
        let started = super::first_screen(
            &shown,
            &controls,
            presented,
            spawned,
            || Ok(7_u32),
            |_| Ok(frame_timing(true)),
            |_| std::thread::sleep(Duration::from_millis(1)),
        );
        assert!(matches!(started, super::FirstScreen::Shown(7)));
    }

    /// A renderer whose drop waits until its sender is used or dropped (at
    /// most 10 s), as a surface's destruction waits on the driver, noting
    /// the thread it was dropped on.
    struct HeldDrop {
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        dropped_on: std::sync::Arc<std::sync::Mutex<Option<std::thread::ThreadId>>>,
    }

    impl Drop for HeldDrop {
        fn drop(&mut self) {
            *self.dropped_on.lock().unwrap() = Some(std::thread::current().id());
            if let Ok(rx) = self.release.lock() {
                let _ = rx.recv_timeout(Duration::from_secs(10));
            }
        }
    }

    /// W7.1: a key config renderer whose startup failed (its first frame)
    /// is dropped on the worker, not the game thread: a destruction that
    /// waits on the driver holds the worker, the game thread's wait ends
    /// by the deadline, and the worker is left with the window.
    #[test]
    fn an_unshown_key_config_renderer_is_dropped_on_its_worker() {
        use std::time::Instant;

        use crate::window::{StartEnd, StartFailure, WindowShown};

        let controls = game_controls();
        let shown = WindowShown::pending_for(Duration::from_millis(300));
        let (release, held) = std::sync::mpsc::channel::<()>();
        let dropped_on = std::sync::Arc::default();
        let renderer = HeldDrop {
            release: std::sync::Mutex::new(held),
            dropped_on: std::sync::Arc::clone(&dropped_on),
        };
        let started = super::first_screen(
            &shown,
            &controls,
            std::sync::Arc::default(),
            spawned,
            move || Ok(renderer),
            |_| Err(render::RenderError::SurfaceLost),
            |_| {},
        );
        let late = Instant::now().saturating_duration_since(shown.deadline());
        let (end, left) = ended(started);
        drop(release);
        assert_eq!(
            end,
            StartEnd::Failed(StartFailure::Render("render: surface lost".to_owned()))
        );
        assert!(left, "a worker still dropping its renderer was not left");
        assert!(late < Duration::from_secs(1), "{late:?} past the deadline");
        let on = *dropped_on.lock().unwrap();
        assert!(on.is_some(), "the renderer was not dropped");
        assert_ne!(
            on,
            Some(std::thread::current().id()),
            "dropped on the game thread"
        );
    }

    /// W7.2: once its first present is counted, the key config's worker
    /// draws no more frame before the window settles the outcome, so the
    /// renderer's handoff cannot wait on a frame stalled in the driver.
    #[test]
    fn no_key_config_frame_is_drawn_past_the_first_counted_present() {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::time::Instant;

        use crate::window::WindowShown;

        let controls = game_controls();
        let shown = WindowShown::pending_for(Duration::from_secs(10));
        let presented = std::sync::Arc::new(AtomicBool::new(false));
        let window_side = (std::sync::Arc::clone(&presented), shown.clone());
        // The window thread's part, late: it shows 100 ms after the count.
        std::thread::spawn(move || {
            let (presented, shown) = window_side;
            while !presented.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(100));
            assert!(shown.show_now());
        });
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = std::sync::Mutex::new(held);
        let frames = std::sync::Arc::new(AtomicU32::new(0));
        let counted = std::sync::Arc::clone(&frames);
        // The first frame is counted; a second one stalls (at most 10 s).
        let frame = move |(): &mut ()| {
            if counted.fetch_add(1, Ordering::SeqCst) > 0 {
                let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(10));
            }
            Ok(frame_timing(true))
        };
        let asked = Instant::now();
        let started = super::first_screen(
            &shown,
            &controls,
            presented,
            spawned,
            || Ok(()),
            frame,
            |()| {},
        );
        let took = asked.elapsed();
        drop(release);
        assert!(matches!(started, super::FirstScreen::Shown(())));
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert_eq!(frames.load(Ordering::SeqCst), 1);
    }
}
