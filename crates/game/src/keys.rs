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
//! Escape ([`CANCEL_KEY`]) cancels and nothing is written; F12
//! ([`KEEP_KEY`]) keeps the role's current bindings, which is how Escape
//! stays the quit key (the default). Escape can also be bound by editing
//! `device.toml` (scan code 1).

use input::{Capture, DeviceKind};

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
    /// Controllers'.
    pub controller: KeyConfig,
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

/// The key configs being captured, one role at a time.
#[derive(Clone, Debug)]
pub struct KeyConfigFlow {
    current: KeyConfigs,
    bound: Vec<(Role, Vec<Bound>)>,
    refused: Option<Refusal>,
}

impl KeyConfigFlow {
    /// A flow that starts at the scratch; `current` is what keep keeps.
    #[must_use]
    pub const fn new(current: KeyConfigs) -> Self {
        Self {
            current,
            bound: Vec::new(),
            refused: None,
        }
    }

    /// The role prompted for; `None` once every role is bound.
    #[must_use]
    pub fn role(&self) -> Option<Role> {
        ROLES.get(self.bound.len()).copied()
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
            let keys = self.current.of(kind);
            let controls: Vec<u32> = match role {
                Role::Lane(l) => keys
                    .lanes
                    .iter()
                    .filter(|k| k.lane == l)
                    .map(|k| k.control)
                    .collect(),
                _ if kind != DeviceKind::Keyboard => Vec::new(),
                Role::Retry => keys.retry.into_iter().collect(),
                Role::Quit => keys.quit.into_iter().collect(),
            };
            found.extend(controls.into_iter().map(|control| Bound { kind, control }));
        }
        found
    }

    /// The key configs of the roles bound so far.
    fn configs(bound: &[(Role, Vec<Bound>)]) -> KeyConfigs {
        let empty = || KeyConfig {
            lanes: Vec::new(),
            retry: None,
            quit: None,
        };
        let mut c = KeyConfigs {
            keyboard: empty(),
            controller: empty(),
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
        c
    }

    fn refuse(&mut self, r: Refusal) -> Press {
        self.refused = Some(r);
        Press::Refused(r)
    }

    /// Takes a pressed control for the role prompted for.
    pub fn press(&mut self, pressed: Bound) -> Press {
        let Some(role) = self.role() else {
            return Press::Done(Self::configs(&self.bound));
        };
        let keyboard = pressed.kind == DeviceKind::Keyboard;
        if keyboard && pressed.control == CANCEL_KEY {
            return Press::Cancelled;
        }
        let chosen = if keyboard && pressed.control == KEEP_KEY {
            let kept = self.current(role);
            if kept.is_empty() && matches!(role, Role::Lane(_)) {
                return self.refuse(Refusal::NothingToKeep(role));
            }
            kept
        } else {
            vec![pressed]
        };
        self.bound.push((role, chosen.clone()));
        if let Err(e) = Self::configs(&self.bound).check() {
            self.bound.pop();
            return self.refuse(Refusal::Key(e));
        }
        self.refused = None;
        if self.role().is_none() {
            return Press::Done(Self::configs(&self.bound));
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
    /// Every role was bound.
    Done(KeyConfigs),
    /// Escape was pressed.
    Cancelled,
    /// The window closed.
    Closed,
}

/// Runs the key config screen: each next press `capture` takes (recording
/// is closed, so none is recorded) goes to the flow, until every role is
/// bound, Escape is pressed, or the screen closes. Nothing is written
/// here; the caller writes a [`KeyConfigEnd::Done`].
pub fn capture_keys(
    capture: &Capture,
    screen: &mut impl KeyScreen,
    current: KeyConfigs,
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
        match flow.press(pressed) {
            Press::Done(keys) => return KeyConfigEnd::Done(keys),
            Press::Cancelled => return KeyConfigEnd::Cancelled,
            Press::Bound(..) | Press::Refused(_) => capture.arm(),
        }
    }
}
