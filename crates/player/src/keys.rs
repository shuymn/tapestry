//! `tapestry keys`: the key config screen (stage1.md PR16). It opens the
//! window and the input thread as a play does, but never opens recording:
//! the input thread offers each press to the capture slot, `game`'s flow
//! binds it, and the screen is drawn with the bitmap font. Every role
//! bound writes `device.toml` atomically; Escape or closing the window
//! writes nothing.

use std::sync::Arc;

use eval::ControlId;
use game::{
    Bound, DeviceSettings, KeyConfigEnd, KeyConfigFlow, KeyConfigs, KeyError, KeyScreen, ROLES,
    Refusal, Role, capture_keys, load_settings, save_settings,
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
    }
}

/// The screen's lines for `flow`.
pub fn lines(flow: &KeyConfigFlow) -> Vec<String> {
    let mut l = vec!["KEY CONFIG".to_owned(), String::new()];
    match flow.role() {
        Some(r @ (Role::Retry | Role::Quit)) => {
            l.push(format!("PRESS THE KEYBOARD KEY FOR {}", role(r)));
        }
        Some(r @ Role::Lane(0)) => l.push(format!(
            "PRESS THE KEY OR BUTTON, OR TURN THE AXIS, FOR {}",
            role(r)
        )),
        Some(r) => l.push(format!("PRESS THE KEY OR BUTTON FOR {}", role(r))),
        None => l.push("DONE".to_owned()),
    }
    l.push(flow.refused().map_or_else(String::new, refusal));
    l.push(String::new());
    for (i, r) in ROLES.iter().enumerate() {
        let value = match flow.bound().get(i) {
            Some((_, b)) => names(b),
            None if i == flow.bound().len() => "<".to_owned(),
            None => format!("(NOW {})", names(&flow.current(*r))),
        };
        l.push(format!("{:<8} {value}", role(*r)));
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
            eprintln!("render: {e}");
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
) -> Result<(), String> {
    let root = crate::data_root()?;
    let path = root.device_settings();
    // Read without the key config checks, so a key config that `play`
    // refuses can be set again here.
    let mut device: DeviceSettings =
        load_settings(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let controls = Arc::new(Controls::new(clock));
    let capture = Capture::new();
    let (window_thread, surface, input) = crate::open_window_and_input(
        &controls,
        clock,
        crate::sources(),
        capture.clone(),
        |_, _| {},
        (None, None),
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
            return Err(e.to_string());
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
    };
    let end = capture_keys(&capture, &mut screen, current);
    drop(screen);
    #[cfg(feature = "dev-tools")]
    drop(script);
    drop(input);
    window_thread.close();
    match end {
        KeyConfigEnd::Done(keys) => {
            device.keyboard = keys.keyboard;
            device.controller = keys.controller;
            save_settings(&path, &device).map_err(|e| format!("{}: {e}", path.display()))?;
            println!("keys: saved to {}", path.display());
        }
        KeyConfigEnd::Cancelled => println!("keys: cancelled, nothing written"),
        KeyConfigEnd::Closed => println!("keys: the window closed, nothing written"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use game::{Bound, DeviceSettings, KeyConfig, KeyConfigFlow, KeyConfigs};
    use input::DeviceKind;

    use super::{lines, name};

    #[test]
    fn the_screen_shows_prompt_refusal_and_bindings() {
        let mut flow = KeyConfigFlow::new(KeyConfigs {
            keyboard: DeviceSettings::default().keyboard,
            controller: KeyConfig {
                lanes: Vec::new(),
                retry: None,
                quit: None,
            },
        });
        let pad = Bound {
            kind: DeviceKind::Controller,
            control: 12,
        };
        flow.press(pad);
        flow.press(pad);
        let l = lines(&flow);
        assert_eq!(l[2], "PRESS THE KEY OR BUTTON FOR KEY 1");
        assert_eq!(l[3], "ALREADY BOUND TO SCRATCH: PRESS ANOTHER");
        assert_eq!(l[5], "SCRATCH  BUTTON 12");
        assert_eq!(l[6], "KEY 1    <");
        assert_eq!(l[7], "KEY 2    (NOW KEY 001F)");
        assert_eq!(l[13], "RETRY    (NOW BACKSPACE)");
        assert_eq!(l[14], "QUIT     (NOW ESCAPE)");
        for k in [0x2c, 0x1f, 0x2d, 0x20, 0x2e, 0x21, 0x2f] {
            flow.press(Bound {
                kind: DeviceKind::Keyboard,
                control: k,
            });
        }
        flow.press(pad);
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
        });
        assert_eq!(
            lines(&flow)[2],
            "PRESS THE KEY OR BUTTON, OR TURN THE AXIS, FOR SCRATCH"
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
