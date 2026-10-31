//! `tapestry keys`: the key config screen (design.md "Input"). It opens the
//! window and the input thread as a play does, but never opens recording: the
//! input thread offers each press to the capture slot, `game`'s flow binds it,
//! and the screen is drawn with the bitmap font. Every role bound writes
//! `device.toml` atomically; Escape or closing the window writes nothing.

use std::sync::Arc;

use anyhow::Context as _;
use eval::ControlId;
use game::{
    Bound, DeviceSettings, KeyConfigEnd, KeyConfigFlow, KeyConfigs, KeyError, KeyScreen, Refusal,
    Role, capture_keys, load_settings, save_settings,
};
use input::hid::is_axis;
use input::{Capture, DeviceKind, QpcClock};
use render::{AdapterChoice, Renderer, Target};

use crate::controls::Controls;
use crate::end_screen::key_name;

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
        Refusal::NothingToKeep(r) => {
            format!("{} HAS NO BINDING TO KEEP: PRESS A CONTROL", role(r))
        }
        Refusal::OtherController => "ANOTHER CONTROLLER IS BOUND: USE THE SAME ONE".to_owned(),
        Refusal::UnknownController => {
            "THIS CONTROLLER CANNOT BE TOLD APART: PRESS ANOTHER".to_owned()
        }
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
            role(*r)
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
}

impl KeyScreen for Screen<'_> {
    fn show(&mut self, flow: &KeyConfigFlow) {
        let lines = lines(flow);
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        if let Err(e) = self.renderer.text_frame(&refs) {
            // The screen is lost: nothing more can be shown, so the key
            // config ends as if the window closed.
            eprintln!("render: {}", game::Chain(&e));
            self.controls.close();
        }
    }

    fn wait(&mut self) {
        if !self.renderer.has_frame_waitable() || !self.renderer.wait_for_frame(100) {
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    }

    fn closed(&self) -> bool {
        self.controls.quitting()
    }
}

/// Puts a finished key config into `device`: the bindings, the scratch
/// axis's revolution when it was learnt beyond its declared range, and its
/// menu direction when it was turned for it (else the current one stays).
fn apply(
    device: &mut DeviceSettings,
    keys: KeyConfigs,
    widened: Option<u32>,
    menu_reversed: Option<bool>,
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
    let renderer = match Renderer::new(target, AdapterChoice::HighPerformance) {
        Ok(r) => r,
        Err(e) => {
            drop(input);
            window_thread.close();
            return Err(e.into());
        }
    };
    #[cfg(feature = "dev-tools")]
    let script = script.map(|s| s.spawn(capture.clone(), Arc::clone(&controls)));
    let mut screen = Screen {
        renderer,
        controls: &controls,
    };
    let current = KeyConfigs {
        keyboard: device.keyboard.clone(),
        controller: device.controller.clone(),
        controller_device: device.controller_device.map(Into::into),
    };
    let end = capture_keys(&capture, &mut screen, current);
    drop(screen);
    #[cfg(feature = "dev-tools")]
    drop(script);
    drop(input);
    window_thread.close();
    match end {
        KeyConfigEnd::Done(keys, widened, menu_reversed) => {
            apply(&mut device, keys, widened, menu_reversed);
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
    /// is asked for.
    struct Scripted {
        capture: Capture,
        steps: std::vec::IntoIter<Result<u32, i32>>,
        asked: Vec<String>,
    }

    impl KeyScreen for Scripted {
        fn show(&mut self, flow: &KeyConfigFlow) {
            if flow.turning() {
                self.asked = lines(flow);
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
    /// (F12), `answer` at the menu direction prompt, and the rest kept,
    /// written over `device` as `tapestry keys` writes it.
    fn configured(answer: Result<u32, i32>, device: &mut DeviceSettings) -> Vec<String> {
        let capture = Capture::new();
        let mut steps = vec![Err(1), Ok(game::KEEP_KEY), answer];
        steps.extend([Ok(game::KEEP_KEY); 9]);
        let mut screen = Scripted {
            capture: capture.clone(),
            steps: steps.into_iter(),
            asked: Vec::new(),
        };
        let current = KeyConfigs {
            keyboard: device.keyboard.clone(),
            controller: device.controller.clone(),
            controller_device: device.controller_device.map(Into::into),
        };
        let end = capture_keys_within(
            &capture,
            &mut screen,
            current,
            Duration::ZERO,
            Duration::ZERO,
        );
        let KeyConfigEnd::Done(keys, widened, menu_reversed) = end else {
            panic!("{end:?}");
        };
        apply(device, keys, widened, menu_reversed);
        assert_eq!(device.validate(), Ok(()));
        screen.asked
    }

    /// The axis turned toward lower values at the prompt writes the menu
    /// direction reversed, toward higher values not reversed, and F12
    /// keeps what `device.toml` had.
    #[test]
    fn key_config_writes_the_menu_direction_turned() {
        let mut device = DeviceSettings::default();
        let asked = configured(Err(-1), &mut device);
        assert_eq!(
            asked[2],
            "TURN THE TURNTABLE THE WAY THAT SCROLLS MENUS DOWN (F12: KEEP)"
        );
        assert!(device.scratch.menu_reversed);
        configured(Ok(game::KEEP_KEY), &mut device);
        assert!(device.scratch.menu_reversed, "F12 keeps it");
        configured(Err(1), &mut device);
        assert!(!device.scratch.menu_reversed);
        configured(Ok(game::KEEP_KEY), &mut device);
        assert!(!device.scratch.menu_reversed, "F12 keeps it");
    }

    #[test]
    fn the_screen_shows_prompt_refusal_and_bindings() {
        let mut flow = KeyConfigFlow::new(KeyConfigs {
            keyboard: DeviceSettings::default().keyboard,
            controller: KeyConfig {
                lanes: Vec::new(),
                retry: None,
                quit: None,
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
}
