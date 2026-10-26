//! The key config screen's flow (stage1.md PR16, D6): it prompts for each
//! lane, the scratch first, then retry and quit, and binds the next
//! control pressed, captured while play recording is closed, so nothing
//! captured is recorded.
//!
//! A lane takes a control of any device; retry and quit take keyboard keys
//! only, because stage 1 reads them from the window's keyboard messages.
//! A control already bound to another role, a key the game reserves
//! ([`reserved_key`](crate::reserved_key)), or a controller's control at
//! the retry or quit prompt is refused with a prompt to press another.
//! The controller key config belongs to one controller, the first whose
//! control is bound: a control of a controller of another product is
//! refused too, and `device.toml` saves the product with the bindings.
//! Escape ([`CANCEL_KEY`]) cancels and nothing is written; F12
//! ([`KEEP_KEY`]) keeps the role's current bindings, which is how Escape
//! stays the quit key (the default). Escape can also be bound by editing
//! `device.toml` (scan code 1).
//!
//! The scratch has two directions. An axis covers both, and is then
//! turned a few revolutions so its real range is learnt
//! ([`capture_keys`]); a button or a key covers one, and the scratch is
//! prompted for again for the other direction, which F12 skips.

use std::time::{Duration, Instant};

use eval::ControlId;
use input::hid::{HidProduct, is_axis, widened_wrap};
use input::{Capture, DeviceKind, Watched};

use crate::settings::{KeyConfig, KeyError, LaneKey, Role};

/// Escape: cancels the key config.
pub const CANCEL_KEY: u32 = 0x01;
/// F12: keeps the current bindings of the role prompted for.
pub const KEEP_KEY: u32 = 0x58;

/// The roles in the order they are prompted for.
pub const ROLES: [Role; 10] = [
    Role::Lane(0),
    Role::Lane(1),
    Role::Lane(2),
    Role::Lane(3),
    Role::Lane(4),
    Role::Lane(5),
    Role::Lane(6),
    Role::Lane(7),
    Role::Retry,
    Role::Quit,
];

/// A control of a kind of device: what a press binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bound {
    /// The kind of device.
    pub kind: DeviceKind,
    /// The control: a keyboard scan code or a controller's control.
    pub control: u32,
}

/// The two key configs `device.toml` holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyConfigs {
    /// Every keyboard's.
    pub keyboard: KeyConfig,
    /// The controller's.
    pub controller: KeyConfig,
    /// The controller `controller` belongs to; known whenever it binds a
    /// lane in a key config the flow finished.
    pub controller_device: Option<HidProduct>,
}

impl KeyConfigs {
    /// The key config of `kind`.
    #[must_use]
    pub const fn of(&self, kind: DeviceKind) -> &KeyConfig {
        match kind {
            DeviceKind::Keyboard => &self.keyboard,
            DeviceKind::Controller => &self.controller,
        }
    }

    /// Checks both ([`KeyConfig::check`]).
    ///
    /// # Errors
    ///
    /// The first binding that breaks a rule.
    pub fn check(&self) -> Result<(), KeyError> {
        self.keyboard.check(DeviceKind::Keyboard)?;
        self.controller.check(DeviceKind::Controller)
    }
}

/// Why a press was not bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// It would break the key config: bound to another role already,
    /// reserved, or not a keyboard key at the retry or quit prompt.
    Key(KeyError),
    /// Keep was pressed for a lane the current key configs do not bind.
    NothingToKeep(Role),
    /// A control of a controller other than the one already bound: the
    /// controller key config belongs to one controller.
    OtherController,
    /// A control of a controller whose model is unknown, or current
    /// controller bindings that name no controller: `device.toml` could not
    /// say which controller plays them.
    UnknownController,
}

/// What a press did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Press {
    /// The role is bound to these controls (none, when an unbound retry or
    /// quit was kept); the next role is prompted for.
    Bound(Role, Vec<Bound>),
    /// Refused: the same role is prompted for again.
    Refused(Refusal),
    /// The last role is bound: the new key configs.
    Done(KeyConfigs),
    /// Escape: nothing is written.
    Cancelled,
}

/// What learning the scratch axis's range found: its declared range,
/// what it reported, and the revolution used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LearntRange {
    /// The declared lowest and highest values.
    pub declared: (i32, i32),
    /// The lowest and highest values it reported.
    pub observed: (i32, i32),
    /// The values in a revolution it is played with.
    pub wrap: u32,
    /// Whether that differs from the declared range, so it is saved.
    pub widened: bool,
}

/// The key configs being captured, one role at a time.
#[derive(Clone, Debug)]
pub struct KeyConfigFlow {
    current: KeyConfigs,
    /// The roles prompted for, in order; the scratch twice when it is
    /// bound to a button or a key.
    steps: Vec<Role>,
    bound: Vec<(Role, Vec<Bound>)>,
    refused: Option<Refusal>,
    /// Once a controller's control is bound: its product.
    device: Option<HidProduct>,
    /// While the scratch axis is turned to learn its range: what it has
    /// shown so far.
    learning: Option<Option<Watched>>,
    learnt: Option<LearntRange>,
}

impl KeyConfigFlow {
    /// A flow that starts at the scratch; `current` is what keep keeps.
    #[must_use]
    pub fn new(current: KeyConfigs) -> Self {
        Self {
            current,
            steps: ROLES.to_vec(),
            bound: Vec::new(),
            refused: None,
            device: None,
            learning: None,
            learnt: None,
        }
    }

    /// The roles prompted for, in order: [`ROLES`], with the scratch
    /// twice once it is bound to a button or a key.
    #[must_use]
    pub fn steps(&self) -> &[Role] {
        &self.steps
    }

    /// Whether the prompt is for the scratch's other direction.
    #[must_use]
    pub fn other_direction(&self) -> bool {
        self.bound.len() == 1 && self.role() == Some(Role::Lane(0))
    }

    /// While the scratch axis is turned to learn its range: what it has
    /// shown so far (`Some(None)` before it reports).
    #[must_use]
    pub const fn learning(&self) -> Option<Option<Watched>> {
        self.learning
    }

    /// What learning the scratch axis's range found, once done.
    #[must_use]
    pub const fn learnt(&self) -> Option<LearntRange> {
        self.learnt
    }

    /// The scratch axis's revolution to save as its override in
    /// `device.toml`: learnt beyond its declared range.
    #[must_use]
    pub fn widened(&self) -> Option<u32> {
        self.learnt.filter(|l| l.widened).map(|l| l.wrap)
    }

    /// The role prompted for; `None` once every role is bound.
    #[must_use]
    pub fn role(&self) -> Option<Role> {
        self.steps.get(self.bound.len()).copied()
    }

    /// The roles bound so far, in order.
    #[must_use]
    pub fn bound(&self) -> &[(Role, Vec<Bound>)] {
        &self.bound
    }

    /// Why the latest press was refused, until a press is bound.
    #[must_use]
    pub const fn refused(&self) -> Option<Refusal> {
        self.refused
    }

    /// The controls the current key configs bind to `role`: keyboard
    /// keys, then controllers' controls. Retry and quit have keyboard keys
    /// only, whatever a hand-edited file says, so keeping them never keeps
    /// what `device.toml` refuses.
    #[must_use]
    pub fn current(&self, role: Role) -> Vec<Bound> {
        let mut found = Vec::new();
        for kind in [DeviceKind::Keyboard, DeviceKind::Controller] {
            if kind != DeviceKind::Keyboard && !matches!(role, Role::Lane(_)) {
                continue;
            }
            let bound = self.current.of(kind).roles().filter(|&(_, r)| r == role);
            found.extend(bound.map(|(control, _)| Bound { kind, control }));
        }
        found
    }

    /// The key configs of the roles bound so far, the controller's bound
    /// to `device`.
    fn configs(bound: &[(Role, Vec<Bound>)], device: Option<HidProduct>) -> KeyConfigs {
        let empty = || KeyConfig {
            lanes: Vec::new(),
            retry: None,
            quit: None,
        };
        let mut c = KeyConfigs {
            keyboard: empty(),
            controller: empty(),
            controller_device: None,
        };
        for (role, controls) in bound {
            for b in controls {
                let keys = match b.kind {
                    DeviceKind::Keyboard => &mut c.keyboard,
                    DeviceKind::Controller => &mut c.controller,
                };
                match *role {
                    Role::Lane(lane) => keys.lanes.push(LaneKey {
                        control: b.control,
                        lane,
                    }),
                    Role::Retry => keys.retry = Some(b.control),
                    Role::Quit => keys.quit = Some(b.control),
                }
            }
        }
        if !c.controller.lanes.is_empty() {
            c.controller_device = device;
        }
        c
    }

    fn refuse(&mut self, r: Refusal) -> Press {
        self.refused = Some(r);
        Press::Refused(r)
    }

    /// Takes a pressed control for the role prompted for, a controller's
    /// of unknown product ([`press_from`](Self::press_from)).
    pub fn press(&mut self, pressed: Bound) -> Press {
        self.press_from(pressed, None)
    }

    /// Takes a pressed control for the role prompted for; a controller's
    /// comes from a controller of `product` (`None`: unknown). A
    /// controller's control of unknown product is refused, and so is
    /// keeping current controller bindings that name no controller, so a
    /// finished key config always names the controller it binds. Once a
    /// controller's control is bound, a control of a controller of another
    /// product is refused, and so is keeping the current controller
    /// bindings when they belong to another controller.
    pub fn press_from(&mut self, pressed: Bound, product: Option<HidProduct>) -> Press {
        let Some(role) = self.role() else {
            return Press::Done(Self::configs(&self.bound, self.device));
        };
        let keyboard = pressed.kind == DeviceKind::Keyboard;
        if keyboard && pressed.control == CANCEL_KEY {
            return Press::Cancelled;
        }
        let keep = keyboard && pressed.control == KEEP_KEY;
        let (chosen, from) = if keep && self.other_direction() {
            // Skipped: the scratch keeps the one control it has.
            (Vec::new(), None)
        } else if keep {
            let kept = self.current(role);
            if kept.is_empty() && matches!(role, Role::Lane(_)) {
                return self.refuse(Refusal::NothingToKeep(role));
            }
            (kept, self.current.controller_device)
        } else {
            (vec![pressed], product)
        };
        let controller = chosen.iter().any(|b| b.kind == DeviceKind::Controller);
        let device = if controller {
            let Some(from) = from else {
                return self.refuse(Refusal::UnknownController);
            };
            if self.device.is_some_and(|d| d != from) {
                return self.refuse(Refusal::OtherController);
            }
            Some(from)
        } else {
            self.device
        };
        self.bound.push((role, chosen.clone()));
        let candidate = Self::configs(&self.bound, device);
        if let Err(e) = candidate.check() {
            self.bound.pop();
            return self.refuse(Refusal::Key(e));
        }
        self.device = device;
        self.refused = None;
        // A button or a key for the scratch plays one direction: the
        // other is prompted for next.
        let one_direction = !keep
            && !(pressed.kind == DeviceKind::Controller && is_axis(ControlId(pressed.control)));
        if role == Role::Lane(0) && self.bound.len() == 1 && one_direction {
            self.steps.insert(1, Role::Lane(0));
        }
        if self.role().is_none() {
            return Press::Done(candidate);
        }
        Press::Bound(role, chosen)
    }
}

/// What the key config screen draws on and waits with.
pub trait KeyScreen {
    /// Shows the flow: the role prompted for, the roles bound so far and
    /// any refusal.
    fn show(&mut self, flow: &KeyConfigFlow);
    /// Waits a little: a frame, or a step of a script.
    fn wait(&mut self);
    /// Whether the window is closing.
    fn closed(&self) -> bool;
}

/// How the key config screen ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyConfigEnd {
    /// Every role was bound; with the scratch axis's revolution to save,
    /// when it was learnt beyond its declared range
    /// ([`KeyConfigFlow::widened`]).
    Done(KeyConfigs, Option<u32>),
    /// Escape was pressed.
    Cancelled,
    /// The window closed.
    Closed,
}

/// How long the scratch axis is watched at most while it is turned to
/// learn its range.
pub const LEARN_TIME: Duration = Duration::from_secs(10);

/// Runs the key config screen: each next press `capture` takes (recording
/// is closed, so none is recorded) goes to the flow, until every role is
/// bound, Escape is pressed, or the screen closes. Nothing is written
/// here; the caller writes a [`KeyConfigEnd::Done`].
///
/// When the scratch is bound to an axis, the player turns it a few full
/// revolutions: its reports are watched until it has crossed its boundary
/// both ways, [`LEARN_TIME`] has passed, or F12 skips, and the revolution
/// is decided from what it reported ([`LearntRange`]).
pub fn capture_keys(
    capture: &Capture,
    screen: &mut impl KeyScreen,
    current: KeyConfigs,
) -> KeyConfigEnd {
    capture_keys_within(capture, screen, current, LEARN_TIME)
}

/// [`capture_keys`] with `limit` for the range learning.
pub fn capture_keys_within(
    capture: &Capture,
    screen: &mut impl KeyScreen,
    current: KeyConfigs,
    limit: Duration,
) -> KeyConfigEnd {
    let mut flow = KeyConfigFlow::new(current);
    capture.arm();
    loop {
        screen.show(&flow);
        screen.wait();
        if screen.closed() {
            return KeyConfigEnd::Closed;
        }
        let Some((control, kind)) = capture.take() else {
            continue;
        };
        let pressed = Bound {
            kind,
            control: control.0,
        };
        let product = match kind {
            DeviceKind::Keyboard => None,
            DeviceKind::Controller => capture.product(),
        };
        match flow.press_from(pressed, product) {
            Press::Done(keys) => return KeyConfigEnd::Done(keys, flow.widened()),
            Press::Cancelled => return KeyConfigEnd::Cancelled,
            Press::Bound(Role::Lane(0), ref chosen)
                if flow.bound.len() == 1
                    && chosen.len() == 1
                    && chosen[0].kind == DeviceKind::Controller
                    && is_axis(ControlId(chosen[0].control)) =>
            {
                if let Some(end) = learn_range(capture, screen, &mut flow, control, limit) {
                    return end;
                }
                capture.arm();
            }
            Press::Bound(..) | Press::Refused(_) => capture.arm(),
        }
    }
}

/// Watches the scratch axis `control` while the player turns it, then
/// decides its revolution: the smallest standard span holding its declared
/// range and what it reported ([`widened_wrap`]), else the declared one.
/// Returns how the screen ended if it was cancelled or closed meanwhile.
fn learn_range(
    capture: &Capture,
    screen: &mut impl KeyScreen,
    flow: &mut KeyConfigFlow,
    control: ControlId,
    limit: Duration,
) -> Option<KeyConfigEnd> {
    capture.watch(control);
    capture.arm();
    let started = Instant::now();
    let seen = loop {
        flow.learning = Some(capture.watch_result());
        screen.show(flow);
        screen.wait();
        if screen.closed() {
            capture.unwatch();
            return Some(KeyConfigEnd::Closed);
        }
        if let Some((key, DeviceKind::Keyboard)) = capture.take() {
            match key.0 {
                CANCEL_KEY => {
                    capture.unwatch();
                    return Some(KeyConfigEnd::Cancelled);
                }
                KEEP_KEY => break capture.watch_result(),
                _ => {}
            }
        }
        // Anything else captured meanwhile, the turning axis included, is
        // not a binding.
        capture.arm();
        let seen = capture.watch_result();
        if seen.is_some_and(|w| w.forward && w.backward) || started.elapsed() >= limit {
            break seen;
        }
    };
    capture.unwatch();
    flow.learning = None;
    flow.learnt = seen.map(|w| {
        let widened = widened_wrap(&w.cap, w.lo, w.hi);
        LearntRange {
            declared: (w.cap.min, w.cap.max),
            observed: (w.lo, w.hi),
            wrap: widened.unwrap_or(w.cap.wrap),
            widened: widened.is_some(),
        }
    });
    None
}
