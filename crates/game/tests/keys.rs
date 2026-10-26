//! The key config screen's flow, driven by scripted input: packets offered
//! to the capture slot the way the input thread offers them, one per wait
//! of the screen.

use std::collections::VecDeque;

use eval::ControlId;
use game::{
    Bound, DeviceSettings, KeyConfig, KeyConfigEnd, KeyConfigFlow, KeyConfigs, KeyError, KeyScreen,
    LaneKey, Press, Refusal, Role, capture_keys,
};
use input::{Capture, DeviceId, DeviceKind, Packet};

/// One step of the script: a packet offered (kind, control, pressed,
/// recording open), or the window closing.
#[derive(Clone, Copy)]
enum Step {
    Offer(DeviceKind, u32, bool, bool),
    Close,
}

use Step::{Close, Offer};

const KB: DeviceKind = DeviceKind::Keyboard;
const PAD: DeviceKind = DeviceKind::Controller;

fn key(control: u32) -> Step {
    Offer(KB, control, true, false)
}

fn button(control: u32) -> Step {
    Offer(PAD, control, true, false)
}

/// A screen that offers the next scripted packet at each wait and keeps
/// what it was shown.
struct Scripted {
    capture: Capture,
    steps: VecDeque<Step>,
    closed: bool,
    /// The role prompted for and the refusal at each show.
    shown: Vec<(Option<Role>, Option<Refusal>)>,
}

impl Scripted {
    fn new(capture: &Capture, steps: &[Step]) -> Self {
        Self {
            capture: capture.clone(),
            steps: steps.iter().copied().collect(),
            closed: false,
            shown: Vec::new(),
        }
    }
}

impl KeyScreen for Scripted {
    fn show(&mut self, flow: &KeyConfigFlow) {
        self.shown.push((flow.role(), flow.refused()));
    }

    fn wait(&mut self) {
        match self.steps.pop_front() {
            Some(Offer(kind, control, pressed, recording)) => {
                let packet = Packet {
                    device: DeviceId(3),
                    control: ControlId(control),
                    value: i32::from(pressed),
                    background: false,
                };
                self.capture.offer(&packet, kind, recording);
            }
            Some(Close) => self.closed = true,
            // A script that runs out never ends the screen: fail instead.
            None => panic!("the script ran out"),
        }
    }

    fn closed(&self) -> bool {
        self.closed
    }
}

fn current() -> KeyConfigs {
    let d = DeviceSettings::default();
    KeyConfigs {
        keyboard: d.keyboard,
        controller: KeyConfig {
            lanes: vec![LaneKey {
                control: 21,
                lane: 0,
            }],
            retry: None,
            quit: None,
        },
    }
}

fn run(steps: &[Step]) -> (KeyConfigEnd, Scripted) {
    let capture = Capture::new();
    let mut screen = Scripted::new(&capture, steps);
    let end = capture_keys(&capture, &mut screen, current());
    (end, screen)
}

/// Each prompt binds the next press, of a keyboard or a controller for a
/// lane, of a keyboard only for retry and quit. Releases, presses while
/// recording is open, a key bound already, a reserved key and a
/// controller button at the retry prompt bind nothing, and the prompt
/// stays where it was.
#[test]
fn key_config_capture_binds_next_press() {
    let steps = [
        // Scratch: a release and a press while recording is open are not
        // captured; then a controller button.
        Offer(KB, 0x10, false, false),
        Offer(KB, 0x11, true, true),
        button(30),
        // Key 1: the same button again is refused; a keyboard key binds.
        button(30),
        key(0x1e),
        // Key 2: F1 (hi-speed) and F8 (input offset) are reserved.
        key(0x3b),
        key(0x42),
        key(0x1f),
        key(0x20),
        key(0x21),
        key(0x22),
        key(0x23),
        // Key 7: a controller button.
        button(31),
        // Retry: a controller button is refused, then a key bound to key 3
        // is refused; then Enter.
        button(32),
        key(0x20),
        key(0x1c),
        // Quit: Enter again is refused; then Space.
        key(0x1c),
        key(0x39),
    ];
    let (end, screen) = run(&steps);
    let lane = |control, lane| LaneKey { control, lane };
    assert_eq!(
        end,
        KeyConfigEnd::Done(KeyConfigs {
            keyboard: KeyConfig {
                lanes: vec![
                    lane(0x1e, 1),
                    lane(0x1f, 2),
                    lane(0x20, 3),
                    lane(0x21, 4),
                    lane(0x22, 5),
                    lane(0x23, 6),
                ],
                retry: Some(0x1c),
                quit: Some(0x39),
            },
            controller: KeyConfig {
                lanes: vec![lane(30, 0), lane(31, 7)],
                retry: None,
                quit: None,
            },
        })
    );
    // What the screen showed after each wait: the prompt and the refusal.
    let overlap = |control, first, second| {
        Some(Refusal::Key(KeyError::Overlap {
            control,
            first,
            second,
        }))
    };
    let reserved = |control, reserved_for| {
        Some(Refusal::Key(KeyError::Reserved {
            control,
            reserved_for,
        }))
    };
    let l = Role::Lane;
    assert_eq!(
        screen.shown,
        [
            (Some(l(0)), None),
            (Some(l(0)), None),
            (Some(l(0)), None),
            (Some(l(1)), None),
            (Some(l(1)), overlap(30, l(0), l(1))),
            (Some(l(2)), None),
            (Some(l(2)), reserved(0x3b, "hi-speed")),
            (Some(l(2)), reserved(0x42, "input offset")),
            (Some(l(3)), None),
            (Some(l(4)), None),
            (Some(l(5)), None),
            (Some(l(6)), None),
            (Some(l(7)), None),
            (Some(Role::Retry), None),
            (
                Some(Role::Retry),
                Some(Refusal::Key(KeyError::NotAKeyboardKey(Role::Retry)))
            ),
            (Some(Role::Retry), overlap(0x20, l(3), Role::Retry)),
            (Some(Role::Quit), None),
            (Some(Role::Quit), overlap(0x1c, Role::Retry, Role::Quit)),
        ]
    );
}

/// Escape cancels at any prompt, and closing the window ends the screen;
/// either way nothing comes back to write.
#[test]
fn escape_and_closing_end_without_a_key_config() {
    let (end, screen) = run(&[key(0x2c), key(0x01)]);
    assert_eq!(end, KeyConfigEnd::Cancelled);
    assert_eq!(screen.shown.len(), 2);
    // Escape from a controller is a button like any other.
    let (end, _) = run(&[button(1), Close]);
    assert_eq!(end, KeyConfigEnd::Closed);
}

/// F12 keeps the role's current bindings, keyboard and controller alike;
/// for a lane with none it is refused, and for an unbound retry it keeps
/// retry unbound.
#[test]
fn keep_keeps_the_current_bindings() {
    let mut flow = KeyConfigFlow::new(current());
    let kb = |control| Bound { kind: KB, control };
    let keep = kb(game::KEEP_KEY);
    // The scratch: Left Shift and controller control 21.
    assert_eq!(
        flow.press(keep),
        Press::Bound(
            Role::Lane(0),
            vec![
                kb(0x2a),
                Bound {
                    kind: PAD,
                    control: 21
                }
            ]
        )
    );
    for (lane, control) in (1..=7).zip([0x2c, 0x1f, 0x2d, 0x20, 0x2e, 0x21, 0x2f]) {
        assert_eq!(
            flow.press(keep),
            Press::Bound(Role::Lane(lane), vec![kb(control)])
        );
    }
    assert_eq!(flow.press(keep), Press::Bound(Role::Retry, vec![kb(0x0e)]));
    // Escape stays the quit key.
    let Press::Done(keys) = flow.press(keep) else {
        panic!("not done");
    };
    assert_eq!(keys.keyboard, DeviceSettings::default().keyboard);
    assert_eq!(keys.controller, current().controller);
    // Done stays done.
    assert_eq!(flow.press(kb(0x30)), Press::Done(keys));

    // Nothing to keep for an unbound lane; an unbound retry stays unbound.
    let mut flow = KeyConfigFlow::new(KeyConfigs {
        keyboard: KeyConfig {
            lanes: Vec::new(),
            retry: None,
            quit: None,
        },
        controller: current().controller,
    });
    assert_eq!(
        flow.press(keep),
        Press::Bound(
            Role::Lane(0),
            vec![Bound {
                kind: PAD,
                control: 21
            }]
        )
    );
    assert_eq!(
        flow.press(keep),
        Press::Refused(Refusal::NothingToKeep(Role::Lane(1)))
    );
    for control in 0x10..0x17 {
        assert!(matches!(flow.press(kb(control)), Press::Bound(..)));
    }
    assert_eq!(flow.press(keep), Press::Bound(Role::Retry, Vec::new()));
}
