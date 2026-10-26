//! The key config screen's flow, driven by scripted input: packets offered
//! to the capture slot the way the input thread offers them, one per wait
//! of the screen.

use std::collections::VecDeque;

use eval::ControlId;
use std::time::Duration;

use game::{
    Bound, DeviceSettings, KeyConfig, KeyConfigEnd, KeyConfigFlow, KeyConfigs, KeyError, KeyScreen,
    LaneKey, Press, Refusal, Role, capture_keys, capture_keys_within,
};
use input::hid::{HidProduct, axis_cap};
use input::{Capture, DeviceId, DeviceKind, Packet};

/// One step of the script: a packet offered (kind, control, pressed,
/// recording open), or the window closing.
#[derive(Clone, Copy)]
enum Step {
    Offer(DeviceKind, u32, bool, bool),
    /// The scratch axis turned far enough to be captured.
    Turn,
    /// A report of the scratch axis, as the input thread passes it to a
    /// watch.
    Report(i32),
    Close,
}

use Step::{Close, Offer, Report, Turn};

const AXIS: u32 = 0x1_0030;

const KB: DeviceKind = DeviceKind::Keyboard;
const PAD: DeviceKind = DeviceKind::Controller;

/// The model of the controller the script presses.
const MODEL_A: HidProduct = HidProduct {
    vendor_id: 0x1A2B,
    product_id: 0x3C4D,
    interface: Some(2),
};
/// Another model.
const MODEL_B: HidProduct = HidProduct {
    vendor_id: 0x5E6F,
    product_id: 0x7081,
    interface: None,
};
/// The model the current key configs belong to.
const MODEL_C: HidProduct = HidProduct {
    vendor_id: 0x92A3,
    product_id: 0xB4C5,
    interface: Some(5),
};

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
    /// The scratch axis's last report.
    last: Option<i32>,
    /// What the flow had learnt of the scratch axis's range at the end.
    learnt: Option<game::LearntRange>,
}

impl Scripted {
    fn new(capture: &Capture, steps: &[Step]) -> Self {
        Self {
            capture: capture.clone(),
            steps: steps.iter().copied().collect(),
            closed: false,
            shown: Vec::new(),
            last: None,
            learnt: None,
        }
    }
}

impl KeyScreen for Scripted {
    fn show(&mut self, flow: &KeyConfigFlow) {
        self.shown.push((flow.role(), flow.refused()));
        self.learnt = flow.learnt();
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
                match kind {
                    KB => self.capture.offer(&packet, kind, recording),
                    PAD => self
                        .capture
                        .offer_controller(&packet, Some(MODEL_A), recording),
                }
            }
            Some(Turn) => {
                let packet = Packet {
                    device: DeviceId(3),
                    control: ControlId(AXIS),
                    value: 1,
                    background: false,
                };
                self.capture.offer_controller(&packet, Some(MODEL_A), false);
            }
            Some(Report(value)) => {
                // Declared -127..=127 in 8 bits.
                let cap = axis_cap(ControlId(AXIS), true, -127, 127, 8).unwrap();
                if self.capture.watched() == Some(cap.control) {
                    self.capture.observe(&cap, self.last, value);
                }
                self.last = Some(value);
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
        controller_device: Some(MODEL_C),
    }
}

fn run(steps: &[Step]) -> (KeyConfigEnd, Scripted) {
    let capture = Capture::new();
    let mut screen = Scripted::new(&capture, steps);
    let end = capture_keys(&capture, &mut screen, current());
    if let KeyConfigEnd::Done(keys, _) = &end {
        saves(keys);
    }
    (end, screen)
}

/// A finished key config, written to `device.toml` as `tapestry keys`
/// writes it, is one the player accepts.
#[track_caller]
fn saves(keys: &KeyConfigs) {
    let device = DeviceSettings {
        keyboard: keys.keyboard.clone(),
        controller: keys.controller.clone(),
        controller_device: keys.controller_device.map(Into::into),
        ..DeviceSettings::default()
    };
    assert_eq!(device.validate(), Ok(()), "{keys:?}");
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
        // captured; then a controller button, one direction. Its other
        // direction: the same button again is refused; another binds.
        Offer(KB, 0x10, false, false),
        Offer(KB, 0x11, true, true),
        button(30),
        button(30),
        button(29),
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
        KeyConfigEnd::Done(
            KeyConfigs {
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
                    lanes: vec![lane(30, 0), lane(29, 0), lane(31, 7)],
                    retry: None,
                    quit: None,
                },
                controller_device: Some(MODEL_A),
            },
            None
        )
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
            (Some(l(0)), None),
            (Some(l(0)), overlap(30, l(0), l(0))),
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
    saves(&keys);
    assert_eq!(keys.keyboard, DeviceSettings::default().keyboard);
    assert_eq!(keys.controller, current().controller);
    assert_eq!(keys.controller_device, Some(MODEL_C));
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
        controller_device: current().controller_device,
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

    // A hand-edited file with a controller's retry and quit: keep keeps
    // the keyboard keys only, which `device.toml` accepts.
    let mut edited = current();
    edited.controller.retry = Some(8);
    edited.controller.quit = Some(9);
    let mut flow = KeyConfigFlow::new(edited);
    for _ in 0..8 {
        assert!(matches!(flow.press(keep), Press::Bound(..)));
    }
    assert_eq!(flow.press(keep), Press::Bound(Role::Retry, vec![kb(0x0e)]));
    let Press::Done(keys) = flow.press(keep) else {
        panic!("not done");
    };
    saves(&keys);
    assert_eq!((keys.controller.retry, keys.controller.quit), (None, None));
    assert_eq!(keys.keyboard.quit, Some(0x01));
}

/// The controller key config belongs to one controller: once a control
/// of one model is bound, a control of another model is refused, and so is
/// keeping current controller bindings of another model; a keyboard key
/// still binds. The result names the model; with no controller binding
/// it names none.
#[test]
fn controller_bindings_belong_to_one_controller() {
    let pad = |control| Bound { kind: PAD, control };
    let kb = |control| Bound { kind: KB, control };
    let keep = kb(game::KEEP_KEY);
    let mut flow = KeyConfigFlow::new(current());
    assert!(matches!(
        flow.press_from(pad(5), Some(MODEL_A)),
        Press::Bound(..)
    ));
    assert_eq!(
        flow.press_from(pad(6), Some(MODEL_B)),
        Press::Refused(Refusal::OtherController)
    );
    assert_eq!(
        flow.press_from(pad(6), None),
        Press::Refused(Refusal::UnknownController)
    );
    // The scratch's other direction: keeping skips it.
    assert!(matches!(flow.press(keep), Press::Bound(..)));
    // Key 1: keeping the current bindings is fine without a controller
    // binding among them.
    assert!(matches!(flow.press(keep), Press::Bound(..)));
    assert_eq!(flow.refused(), None);
    for control in [0x1f, 0x2d, 0x20, 0x2e, 0x21] {
        assert!(matches!(flow.press(kb(control)), Press::Bound(..)));
    }
    assert!(matches!(
        flow.press_from(pad(7), Some(MODEL_A)),
        Press::Bound(..)
    ));
    assert!(matches!(flow.press(keep), Press::Bound(..)));
    let Press::Done(keys) = flow.press(keep) else {
        panic!("not done");
    };
    saves(&keys);
    assert_eq!(keys.controller_device, Some(MODEL_A));
    assert_eq!(keys.controller.lanes.len(), 2);

    // Current key configs binding key 1 to control 22 of model C: after
    // a control of model A, keeping key 1 is refused.
    let mut flow = KeyConfigFlow::new(KeyConfigs {
        keyboard: current().keyboard,
        controller: KeyConfig {
            lanes: vec![LaneKey {
                control: 22,
                lane: 1,
            }],
            retry: None,
            quit: None,
        },
        controller_device: Some(MODEL_C),
    });
    assert!(matches!(
        flow.press_from(pad(5), Some(MODEL_A)),
        Press::Bound(..)
    ));
    assert_eq!(flow.press(keep), Press::Bound(Role::Lane(0), Vec::new()));
    assert_eq!(flow.press(keep), Press::Refused(Refusal::OtherController));

    // Keyboard keys only: no model.
    let mut flow = KeyConfigFlow::new(current());
    for control in 0x10..=0x19 {
        assert!(matches!(flow.press(kb(control)), Press::Bound(..)));
    }
    let Press::Done(keys) = flow.press(keep) else {
        panic!("not done");
    };
    saves(&keys);
    assert!(keys.controller.lanes.is_empty());
    assert_eq!(keys.controller_device, None);
}

/// Current controller bindings that name no controller (a file written
/// before, or by hand) cannot be kept: F12 on every role would otherwise
/// write a `device.toml` the player refuses. Pressing the role's controls
/// again finishes a key config the player accepts.
#[test]
fn controller_bindings_without_a_controller_are_not_kept() {
    let unnamed = KeyConfigs {
        controller_device: None,
        ..current()
    };
    let kb = |control| Bound { kind: KB, control };
    let keep = kb(game::KEEP_KEY);
    let mut flow = KeyConfigFlow::new(unnamed);
    assert_eq!(flow.press(keep), Press::Refused(Refusal::UnknownController));
    assert!(matches!(
        flow.press_from(
            Bound {
                kind: PAD,
                control: 21
            },
            Some(MODEL_B)
        ),
        Press::Bound(..)
    ));
    for _ in 0..10 {
        if let Press::Done(keys) = flow.press(keep) {
            saves(&keys);
            assert_eq!(keys.controller_device, Some(MODEL_B));
            return;
        }
    }
    panic!("not done");
}

/// F12 for every role after the scratch: keep the current bindings.
fn keep_the_rest() -> Vec<Step> {
    vec![key(0x58); 9]
}

/// Runs `steps` with `limit` for the range learning.
fn run_within(steps: &[Step], limit: Duration) -> (KeyConfigEnd, Scripted) {
    let capture = Capture::new();
    let mut screen = Scripted::new(&capture, steps);
    let end = capture_keys_within(&capture, &mut screen, current(), limit);
    if let KeyConfigEnd::Done(keys, _) = &end {
        saves(keys);
    }
    (end, screen)
}

const HOUR: Duration = Duration::from_secs(3_600);

#[test]
fn an_axis_covers_both_directions_and_learns_its_range() {
    // Turned for the scratch: no second prompt. Then turned through its
    // boundary forward, and only late backward; it reported -128 under
    // its declared -127..=127, so it is played with 256 values.
    let mut steps = vec![Report(100), Turn];
    steps.extend([110, 127, -128, -120, -60, 0, 60].map(Report));
    steps.extend([-90, -127, -128, 127, 110].map(Report));
    steps.extend(keep_the_rest());
    let (end, screen) = run_within(&steps, HOUR);
    let KeyConfigEnd::Done(keys, widened) = end else {
        panic!("{end:?}");
    };
    assert_eq!(
        keys.controller.lanes,
        [LaneKey {
            control: AXIS,
            lane: 0
        }]
    );
    assert_eq!(widened, Some(256));
    assert_eq!(
        screen.learnt,
        Some(game::LearntRange {
            declared: (-127, 127),
            observed: (-128, 127),
            wrap: 256,
            widened: true,
        })
    );
}

#[test]
fn a_genuine_255_position_axis_keeps_its_range() {
    // Crossing 127 to -127 both ways, never beyond: nothing to save.
    let mut steps = vec![Report(0), Turn];
    steps.extend([100, 127, -127, -100, -127, 127].map(Report));
    steps.extend(keep_the_rest());
    let (end, screen) = run_within(&steps, HOUR);
    assert!(matches!(end, KeyConfigEnd::Done(_, None)), "{end:?}");
    assert_eq!(
        screen.learnt.map(|l| (l.wrap, l.widened)),
        Some((255, false))
    );
}

#[test]
fn learning_ends_on_f12_or_at_its_time_limit() {
    // F12 while turning: decided from what it reported so far.
    let mut steps = vec![Report(0), Turn, Report(-128), key(0x58)];
    steps.extend(keep_the_rest());
    let (end, screen) = run_within(&steps, HOUR);
    assert!(matches!(end, KeyConfigEnd::Done(_, Some(256))), "{end:?}");
    assert_eq!(screen.learnt.map(|l| l.observed), Some((-128, -128)));
    // Out of time at once: decided after one wait, from nothing seen.
    let mut steps = vec![Report(0), Turn, Close];
    steps.extend(keep_the_rest());
    let (end, _) = run_within(&steps[..3], Duration::ZERO);
    assert_eq!(end, KeyConfigEnd::Closed);
    let mut steps = vec![Report(0), Turn, Report(5)];
    steps.extend(keep_the_rest());
    let (end, screen) = run_within(&steps, Duration::ZERO);
    assert!(matches!(end, KeyConfigEnd::Done(_, None)), "{end:?}");
    assert_eq!(
        screen.learnt.map(|l| (l.observed, l.wrap)),
        Some(((5, 5), 255))
    );
    // Escape while turning cancels.
    let (end, _) = run_within(&[Report(0), Turn, key(0x01)], HOUR);
    assert_eq!(end, KeyConfigEnd::Cancelled);
}

#[test]
fn a_button_or_key_scratch_is_asked_for_both_directions() {
    // Two buttons: both on lane 0.
    let mut steps = vec![button(40), button(41)];
    steps.extend(vec![key(0x58); 9]);
    let (end, _) = run(&steps);
    let KeyConfigEnd::Done(keys, None) = end else {
        panic!("{end:?}");
    };
    let lane = |control, lane| LaneKey { control, lane };
    assert_eq!(keys.controller.lanes, [lane(40, 0), lane(41, 0)]);
    // A key, then F12 for none: one direction only.
    let mut steps = vec![key(0x36), key(0x58)];
    steps.extend(vec![key(0x58); 9]);
    let (end, _) = run(&steps);
    let KeyConfigEnd::Done(keys, None) = end else {
        panic!("{end:?}");
    };
    assert_eq!(
        keys.keyboard.lanes.iter().filter(|k| k.lane == 0).count(),
        1
    );
    // A duplicate in the second prompt is refused.
    let mut flow = KeyConfigFlow::new(current());
    let b = Bound {
        kind: PAD,
        control: 40,
    };
    assert_eq!(
        flow.press_from(b, Some(MODEL_A)),
        Press::Bound(Role::Lane(0), vec![b])
    );
    assert!(flow.other_direction());
    assert!(matches!(
        flow.press_from(b, Some(MODEL_A)),
        Press::Refused(Refusal::Key(_))
    ));
    assert!(flow.other_direction());
}

#[test]
fn the_overlay_key_is_refused_for_either_scratch_direction() {
    let mut flow = KeyConfigFlow::new(current());
    let f9 = Bound {
        kind: KB,
        control: game::OVERLAY_KEY,
    };
    let refused = Press::Refused(Refusal::Key(KeyError::Reserved {
        control: game::OVERLAY_KEY,
        reserved_for: "the measurement overlay",
    }));
    assert_eq!(flow.press(f9), refused);
    let b = Bound {
        kind: PAD,
        control: 40,
    };
    assert!(matches!(
        flow.press_from(b, Some(MODEL_A)),
        Press::Bound(..)
    ));
    assert!(flow.other_direction());
    assert_eq!(flow.press(f9), refused);
    assert!(flow.other_direction());
}
