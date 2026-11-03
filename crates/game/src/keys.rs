//! The key config screen's flow (design.md "Input"): it prompts for each lane,
//! the scratch first, then retry and quit, then the play adjustments' START,
//! SELECT and auxiliary negative and positive keys, then the scratch's
//! adjustment directions, and binds the next control pressed, captured while
//! play recording is closed, so nothing captured is recorded.
//!
//! A lane takes a control of any device; retry and quit take keyboard keys
//! only, because stage 1 reads them from the window's keyboard messages;
//! START, SELECT and the auxiliary keys take a keyboard key or a
//! controller's button. A control already bound to another role, a key the
//! game reserves ([`reserved_key`](crate::reserved_key)), a controller's
//! control at the retry or quit prompt, or a controller's axis at an
//! adjustment prompt is refused with a prompt to press another: the same
//! check [`DeviceSettings::validate`](crate::DeviceSettings::validate) makes
//! at load. The controller key config belongs to one controller, the first
//! whose control is bound: a control of a controller of another product is
//! refused too, and `device.toml` saves the product with the bindings.
//! Escape ([`CANCEL_KEY`]) cancels and nothing is written; F12
//! ([`KEEP_KEY`]) keeps the role's current bindings, which is how Escape
//! stays the quit key (the default). Escape can also be bound by editing
//! `device.toml` (scan code 1).
//!
//! The scratch has two directions. An axis covers both, and is then
//! turned a few revolutions so its real range is learnt, then turned the
//! way that scrolls menus down ([`capture_keys`]); a button or a key
//! covers one, and the scratch is prompted for again for the other
//! direction, which F12 skips.
//!
//! The scratch's adjustment directions (design.md "Play Adjustments",
//! bindings) come last, for each kind of device from its scratch
//! controls: one button or key is the positive direction; of two, the one
//! pressed at the prompt is positive and the other negative (F12 keeps
//! the current choice); an axis is turned the way that raises SUDDEN+ and
//! speeds up, which sets `[scratch] adjust_reversed` (F12 keeps the
//! current setting).

use std::time::{Duration, Instant};

use eval::ControlId;
use input::hid::{HidProduct, is_axis, widened_wrap};
use input::{Capture, DeviceKind, Watched};

use crate::adjust_keys::AdjustKeyError;
use crate::adjustment::Role as Adjust;
use crate::settings::{DeviceSettings, KeyConfig, KeyError, LaneKey, Role};

/// Escape: cancels the key config.
pub const CANCEL_KEY: u32 = 0x01;
/// F12: keeps the current bindings of the role prompted for.
pub const KEEP_KEY: u32 = 0x58;

/// The roles in the order they are prompted for; the scratch's
/// adjustment directions follow.
pub const ROLES: [Role; 14] = [
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
    Role::Start,
    Role::Select,
    Role::AuxNegative,
    Role::AuxPositive,
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
    /// control in a key config the flow finished.
    pub controller_device: Option<HidProduct>,
}

impl From<&DeviceSettings> for KeyConfigs {
    fn from(d: &DeviceSettings) -> Self {
        Self {
            keyboard: d.keyboard.clone(),
            controller: d.controller.clone(),
            controller_device: d.controller_device.map(Into::into),
        }
    }
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

    /// The scratch's buttons or keys of `kind`, in binding order.
    fn scratch_digital(&self, kind: DeviceKind) -> Vec<u32> {
        self.of(kind)
            .lanes
            .iter()
            .filter(|k| k.lane == 0)
            .filter(|k| kind == DeviceKind::Keyboard || !is_axis(ControlId(k.control)))
            .map(|k| k.control)
            .collect()
    }

    /// The controller's scratch axis, if the scratch has one.
    fn scratch_axis(&self) -> Option<u32> {
        self.controller
            .lanes
            .iter()
            .find(|k| k.lane == 0 && is_axis(ControlId(k.control)))
            .map(|k| k.control)
    }
}

/// Why a press was not bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// It would break the key config: bound to another role already,
    /// reserved, not a keyboard key at the retry or quit prompt, an axis at
    /// an adjustment prompt, or not one of the scratch's controls at the
    /// prompt for its positive direction.
    Key(KeyError),
    /// Keep was pressed for a lane, or the scratch's positive direction,
    /// the current key configs do not bind.
    NothingToKeep(Role),
    /// A control of a controller other than the one already bound: the
    /// controller key config belongs to one controller.
    OtherController,
    /// A control of a controller whose model is unknown, or current
    /// controller bindings that name no controller: `device.toml` could not
    /// say which controller plays them.
    UnknownController,
    /// A press while the scratch axis is to be turned for its adjustment
    /// direction: a turn answers, or F12.
    NotATurn,
}

/// What a press did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Press {
    /// The role is bound to these controls (none, when an unbound role was
    /// kept); the next role is prompted for, or the scratch axis is to be
    /// turned for its adjustment direction
    /// ([`reversing`](KeyConfigFlow::reversing)).
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
    /// bound to a button or a key, and its positive direction once for
    /// each kind of device with two or more scratch buttons or keys.
    steps: Vec<Role>,
    bound: Vec<(Role, Vec<Bound>)>,
    /// The scratch directions decided without a prompt: the only scratch
    /// button or key of a kind, and the other of two.
    directions: Vec<(Role, Bound)>,
    /// The kinds of device whose positive scratch direction is prompted
    /// for, in prompt order; set once [`ROLES`] are bound.
    choosing: Option<Vec<DeviceKind>>,
    refused: Option<Refusal>,
    /// Once a controller's control is bound: its product.
    device: Option<HidProduct>,
    /// While the scratch axis is turned to learn its range: what it has
    /// shown so far.
    learning: Option<Option<Watched>>,
    learnt: Option<LearntRange>,
    /// While the scratch axis is to be turned the way that scrolls menus
    /// down.
    turning: bool,
    /// The menu direction that turn showed: whether down is toward lower
    /// values.
    menu_reversed: Option<bool>,
    /// While the scratch axis is to be turned the way that raises SUDDEN+
    /// and speeds up.
    reversing: bool,
    /// The adjustment direction that turn showed: whether raising is
    /// toward lower values.
    adjust_reversed: Option<bool>,
}

impl KeyConfigFlow {
    /// A flow that starts at the scratch; `current` is what keep keeps.
    #[must_use]
    pub fn new(current: KeyConfigs) -> Self {
        Self {
            current,
            steps: ROLES.to_vec(),
            bound: Vec::new(),
            directions: Vec::new(),
            choosing: None,
            refused: None,
            device: None,
            learning: None,
            learnt: None,
            turning: false,
            menu_reversed: None,
            reversing: false,
            adjust_reversed: None,
        }
    }

    /// The roles prompted for, in order: [`ROLES`], with the scratch
    /// twice once it is bound to a button or a key, and then the scratch's
    /// positive direction for each kind of device with two or more
    /// scratch buttons or keys.
    #[must_use]
    pub fn steps(&self) -> &[Role] {
        &self.steps
    }

    /// Whether the prompt is for the scratch's other direction.
    #[must_use]
    pub fn other_direction(&self) -> bool {
        self.bound.len() == 1 && self.role() == Some(Role::Lane(0))
    }

    /// The kind of device whose positive scratch direction is prompted
    /// for, while it is.
    #[must_use]
    pub fn choosing(&self) -> Option<DeviceKind> {
        if self.role() != Some(Role::ScratchPositive) {
            return None;
        }
        let asked = self
            .bound
            .iter()
            .filter(|(r, _)| *r == Role::ScratchPositive)
            .count();
        self.choosing.as_ref()?.get(asked).copied()
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

    /// Whether the scratch axis is to be turned the way that scrolls menus
    /// down.
    #[must_use]
    pub const fn turning(&self) -> bool {
        self.turning
    }

    /// The scratch axis's menu direction to save in `device.toml`: whether
    /// it scrolls down toward lower values, as it was turned; `None` when
    /// it was not (F12 keeps the current setting).
    #[must_use]
    pub const fn menu_reversed(&self) -> Option<bool> {
        self.menu_reversed
    }

    /// Whether the scratch axis is to be turned the way that raises
    /// SUDDEN+ and speeds up: every role is bound, and the flow ends with
    /// [`reverse`](Self::reverse) or F12.
    #[must_use]
    pub const fn reversing(&self) -> bool {
        self.reversing
    }

    /// The scratch axis's adjustment direction to save as `[scratch]
    /// adjust_reversed`: whether raising SUDDEN+ is toward lower values, as
    /// it was turned; `None` when it was not (F12 keeps the current
    /// setting).
    #[must_use]
    pub const fn adjust_reversed(&self) -> Option<bool> {
        self.adjust_reversed
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
            if kind != DeviceKind::Keyboard && matches!(role, Role::Retry | Role::Quit) {
                continue;
            }
            let bound = self.current.of(kind).roles().filter(|&(_, r)| r == role);
            found.extend(bound.map(|(control, _)| Bound { kind, control }));
        }
        found
    }

    /// The key configs of the roles bound so far and the scratch
    /// directions decided, the controller's bound to `device`.
    fn configs(
        bound: &[(Role, Vec<Bound>)],
        directions: &[(Role, Bound)],
        device: Option<HidProduct>,
    ) -> KeyConfigs {
        let mut c = KeyConfigs {
            keyboard: KeyConfig::default(),
            controller: KeyConfig::default(),
            controller_device: None,
        };
        let each = bound
            .iter()
            .flat_map(|(role, controls)| controls.iter().map(|b| (*role, *b)))
            .chain(directions.iter().copied());
        for (role, b) in each {
            let keys = match b.kind {
                DeviceKind::Keyboard => &mut c.keyboard,
                DeviceKind::Controller => &mut c.controller,
            };
            let control = Some(b.control);
            match role {
                Role::Lane(lane) => keys.lanes.push(LaneKey {
                    control: b.control,
                    lane,
                }),
                Role::Retry => keys.retry = control,
                Role::Quit => keys.quit = control,
                Role::Start => keys.start = control,
                Role::Select => keys.select = control,
                Role::AuxNegative => keys.aux_negative = control,
                Role::AuxPositive => keys.aux_positive = control,
                Role::ScratchPositive => keys.scratch_positive = control,
                Role::ScratchNegative => keys.scratch_negative = control,
            }
        }
        if c.controller.roles().next().is_some() {
            c.controller_device = device;
        }
        c
    }

    /// The key configs of everything bound so far.
    fn result(&self) -> KeyConfigs {
        Self::configs(&self.bound, &self.directions, self.device)
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
    /// bindings when they belong to another controller. While the scratch
    /// axis is to be turned for its adjustment direction, F12 keeps the
    /// current setting and ends the flow, and any other press is refused.
    pub fn press_from(&mut self, pressed: Bound, product: Option<HidProduct>) -> Press {
        let keyboard = pressed.kind == DeviceKind::Keyboard;
        if self.reversing {
            return match (keyboard, pressed.control) {
                (true, CANCEL_KEY) => Press::Cancelled,
                (true, KEEP_KEY) => self.reverse(None),
                _ => self.refuse(Refusal::NotATurn),
            };
        }
        let Some(role) = self.role() else {
            return Press::Done(self.result());
        };
        if keyboard && pressed.control == CANCEL_KEY {
            return Press::Cancelled;
        }
        let keep = keyboard && pressed.control == KEEP_KEY;
        if role == Role::ScratchPositive {
            return self.choose_positive(pressed, keep, product);
        }
        if role == Role::ScratchNegative {
            return self.choose_negative(pressed, keep, product);
        }
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
        let candidate = Self::configs(&self.bound, &self.directions, device);
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
        self.next(role, chosen)
    }

    /// Whether a controller's `pressed` control may be bound, from a
    /// controller of `product`: its model must be known, and the one the
    /// controller key config belongs to. A keyboard's always may.
    fn controller_of(&self, pressed: Bound, product: Option<HidProduct>) -> Result<(), Refusal> {
        if pressed.kind != DeviceKind::Controller {
            return Ok(());
        }
        let Some(from) = product else {
            return Err(Refusal::UnknownController);
        };
        if self.device.is_some_and(|d| d != from) {
            return Err(Refusal::OtherController);
        }
        Ok(())
    }

    /// Takes `pressed`, or F12 (`keep`), as the positive scratch direction
    /// of the kind of device prompted for: one of its scratch buttons or
    /// keys, a controller's from the controller already bound. Of two, the
    /// other is negative; of more, a distinct negative is prompted for
    /// next, and the others stay lane-only. F12 keeps both current
    /// directions where they are among the scratch's controls.
    fn choose_positive(
        &mut self,
        pressed: Bound,
        keep: bool,
        product: Option<HidProduct>,
    ) -> Press {
        let Some(kind) = self.choosing() else {
            return Press::Done(self.result());
        };
        let controls = self.result().scratch_digital(kind);
        let chosen = if keep {
            match self.current.of(kind).scratch_positive {
                Some(c) if controls.contains(&c) => c,
                _ => return self.refuse(Refusal::NothingToKeep(Role::ScratchPositive)),
            }
        } else if pressed.kind == kind && controls.contains(&pressed.control) {
            if let Err(r) = self.controller_of(pressed, product) {
                return self.refuse(r);
            }
            pressed.control
        } else {
            return self.refuse(Refusal::Key(KeyError::Adjustment(
                AdjustKeyError::NotScratch {
                    control: pressed.control,
                    role: Adjust::ScratchPositive,
                },
            )));
        };
        let positive = Bound {
            kind,
            control: chosen,
        };
        let kept_negative = self
            .current
            .of(kind)
            .scratch_negative
            .filter(|&c| keep && c != chosen && controls.contains(&c));
        match (kept_negative, &controls[..]) {
            (Some(control), _) => self
                .directions
                .push((Role::ScratchNegative, Bound { kind, control })),
            (None, [a, b]) => {
                let control = if *a == chosen { *b } else { *a };
                self.directions
                    .push((Role::ScratchNegative, Bound { kind, control }));
            }
            // More than two, not kept: the negative is prompted for next.
            (None, _) if !keep && controls.len() > 2 => {
                let at = self.bound.len() + 1;
                self.steps.insert(at, Role::ScratchNegative);
            }
            (None, _) => {}
        }
        self.bound.push((Role::ScratchPositive, vec![positive]));
        self.refused = None;
        self.next(Role::ScratchPositive, vec![positive])
    }

    /// Takes `pressed`, or F12 (`keep`), as the negative scratch direction
    /// of the kind whose positive direction was just chosen from more than
    /// two controls: another of its scratch buttons or keys.
    fn choose_negative(
        &mut self,
        pressed: Bound,
        keep: bool,
        product: Option<HidProduct>,
    ) -> Press {
        let Some(&(_, ref positive)) = self
            .bound
            .iter()
            .rev()
            .find(|(r, _)| *r == Role::ScratchPositive)
        else {
            return Press::Done(self.result());
        };
        let Some(&positive) = positive.first() else {
            return Press::Done(self.result());
        };
        let kind = positive.kind;
        let controls = self.result().scratch_digital(kind);
        let usable = |c: u32| c != positive.control && controls.contains(&c);
        let chosen = if keep {
            match self.current.of(kind).scratch_negative {
                Some(c) if usable(c) => c,
                _ => return self.refuse(Refusal::NothingToKeep(Role::ScratchNegative)),
            }
        } else if pressed.kind == kind && usable(pressed.control) {
            if let Err(r) = self.controller_of(pressed, product) {
                return self.refuse(r);
            }
            pressed.control
        } else {
            return self.refuse(Refusal::Key(KeyError::Adjustment(
                AdjustKeyError::NotScratch {
                    control: pressed.control,
                    role: Adjust::ScratchNegative,
                },
            )));
        };
        let negative = Bound {
            kind,
            control: chosen,
        };
        self.bound.push((Role::ScratchNegative, vec![negative]));
        self.refused = None;
        self.next(Role::ScratchNegative, vec![negative])
    }

    /// After `role` is bound to `chosen`: the next prompt, or once
    /// [`ROLES`] are bound, the scratch's directions; done when nothing is
    /// left.
    fn next(&mut self, role: Role, chosen: Vec<Bound>) -> Press {
        if self.role().is_none() && self.choosing.is_none() {
            let configs = self.result();
            let mut choosing = Vec::new();
            for kind in [DeviceKind::Keyboard, DeviceKind::Controller] {
                match configs.scratch_digital(kind)[..] {
                    [] => {}
                    [control] => self
                        .directions
                        .push((Role::ScratchPositive, Bound { kind, control })),
                    _ => {
                        choosing.push(kind);
                        self.steps.push(Role::ScratchPositive);
                    }
                }
            }
            self.choosing = Some(choosing);
        }
        if self.role().is_some() {
            return Press::Bound(role, chosen);
        }
        if self.result().scratch_axis().is_some() {
            self.reversing = true;
            return Press::Bound(role, chosen);
        }
        Press::Done(self.result())
    }

    /// Ends the scratch axis's adjustment direction: `reversed` as it was
    /// turned, `None` to keep the current setting. The new key configs.
    pub fn reverse(&mut self, reversed: Option<bool>) -> Press {
        self.reversing = false;
        self.refused = None;
        self.adjust_reversed = reversed;
        Press::Done(self.result())
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
    /// ([`KeyConfigFlow::widened`]), its menu direction to save, when it
    /// was turned for it ([`KeyConfigFlow::menu_reversed`]), and its
    /// adjustment direction to save, likewise
    /// ([`KeyConfigFlow::adjust_reversed`]).
    Done(KeyConfigs, Option<u32>, Option<bool>, Option<bool>),
    /// Escape was pressed.
    Cancelled,
    /// The window closed.
    Closed,
}

/// How long the scratch axis is watched at most while it is turned to
/// learn its range.
pub const LEARN_TIME: Duration = Duration::from_secs(10);

/// How long a turn of the scratch axis is not taken as its menu or
/// adjustment direction after that prompt shows, so the turning that ended
/// the range learning does not answer it.
pub const SETTLE_TIME: Duration = Duration::from_millis(500);

/// Runs the key config screen: each next press `capture` takes (recording
/// is closed, so none is recorded) goes to the flow, until every role is
/// bound, Escape is pressed, or the screen closes. Nothing is written
/// here; the caller writes a [`KeyConfigEnd::Done`].
///
/// When the scratch is bound to an axis, the player turns it a few full
/// revolutions: its reports are watched until it has crossed its boundary
/// both ways, [`LEARN_TIME`] has passed, or F12 skips, and the revolution
/// is decided from what it reported ([`LearntRange`]). Then the player
/// turns it the way that scrolls menus down: the direction of its first
/// capture turn ([`Capture::lower`]) begun after [`SETTLE_TIME`] decides
/// it, counted in the revolution menus will count it in (the one learnt
/// beyond the declared range, else `wrap`, the one `device.toml` sets,
/// else the declared one), and F12 keeps the current setting. Once every
/// role is bound, the scratch axis is turned the way that raises SUDDEN+
/// and speeds up, taken the same way.
pub fn capture_keys(
    capture: &Capture,
    screen: &mut impl KeyScreen,
    current: KeyConfigs,
    wrap: Option<u32>,
) -> KeyConfigEnd {
    capture_keys_within(capture, screen, current, wrap, LEARN_TIME, SETTLE_TIME)
}

/// [`capture_keys`] with `limit` for the range learning and `settle`
/// before the menu and adjustment directions are taken.
pub fn capture_keys_within(
    capture: &Capture,
    screen: &mut impl KeyScreen,
    current: KeyConfigs,
    wrap: Option<u32>,
    limit: Duration,
    settle: Duration,
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
        let done = |flow: &KeyConfigFlow, keys| {
            KeyConfigEnd::Done(
                keys,
                flow.widened(),
                flow.menu_reversed,
                flow.adjust_reversed,
            )
        };
        match flow.press_from(pressed, product) {
            Press::Done(keys) => return done(&flow, keys),
            Press::Cancelled => return KeyConfigEnd::Cancelled,
            Press::Bound(Role::Lane(0), ref chosen)
                if flow.bound.len() == 1
                    && chosen.len() == 1
                    && chosen[0].kind == DeviceKind::Controller
                    && is_axis(ControlId(chosen[0].control)) =>
            {
                // The axis bound: the one turned, or the one kept with F12.
                let axis = ControlId(chosen[0].control);
                if let Some(end) = learn_range(capture, screen, &mut flow, axis, limit) {
                    return end;
                }
                let wrap = flow.widened().or(wrap);
                let turn = Turn::Menu;
                if let Some(end) =
                    learn_direction(capture, screen, &mut flow, axis, wrap, settle, turn)
                {
                    return end;
                }
                capture.arm();
            }
            Press::Bound(..) if flow.reversing() => {
                // The flow reverses only with a scratch axis.
                if let Some(axis) = flow.result().scratch_axis() {
                    let wrap = flow.widened().or(wrap);
                    let (axis, turn) = (ControlId(axis), Turn::Adjust);
                    if let Some(end) =
                        learn_direction(capture, screen, &mut flow, axis, wrap, settle, turn)
                    {
                        return end;
                    }
                }
                flow.reversing = false;
                return done(&flow, flow.result());
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

/// Which way the scratch axis is turned for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Turn {
    /// The way that scrolls menus down.
    Menu,
    /// The way that raises SUDDEN+ and speeds up.
    Adjust,
}

/// Asks for the scratch axis `control` to be turned the way `turn` names,
/// and keeps the direction of its first capture turn begun after
/// `settle`: the way it moved, net and the short way round its revolution
/// as menus count it (`wrap`, else the declared one). At `settle` the
/// capture is armed again, dropping a turn captured or counted before it.
/// F12 keeps the current setting; any other control is not an answer.
/// Returns how the screen ended if it was cancelled or closed meanwhile.
fn learn_direction(
    capture: &Capture,
    screen: &mut impl KeyScreen,
    flow: &mut KeyConfigFlow,
    control: ControlId,
    wrap: Option<u32>,
    settle: Duration,
    turn: Turn,
) -> Option<KeyConfigEnd> {
    flow.turning = turn == Turn::Menu;
    let mut answer = None;
    if let Some(wrap) = wrap {
        capture.set_revolution(control, wrap);
    }
    capture.arm();
    let started = Instant::now();
    // Armed at the start: with no settle time, already settled.
    let mut settled = settle.is_zero();
    let end = loop {
        screen.show(flow);
        screen.wait();
        if screen.closed() {
            break Some(KeyConfigEnd::Closed);
        }
        // Settling now: whatever the axis did before counts for nothing.
        let settling = !settled && started.elapsed() >= settle;
        settled |= settling;
        match capture.take() {
            Some((ControlId(CANCEL_KEY), DeviceKind::Keyboard)) => {
                break Some(KeyConfigEnd::Cancelled);
            }
            Some((ControlId(KEEP_KEY), DeviceKind::Keyboard)) => break None,
            Some((c, DeviceKind::Controller))
                if settled && !settling && c == control && capture.product() == flow.device =>
            {
                answer = Some(capture.lower());
                break None;
            }
            Some(_) => capture.arm(),
            None if settling => capture.arm(),
            None => {}
        }
    };
    capture.clear_revolution();
    flow.turning = false;
    if end.is_none() {
        match turn {
            Turn::Menu => flow.menu_reversed = answer,
            Turn::Adjust => {
                flow.reverse(answer);
            }
        }
    }
    end
}
