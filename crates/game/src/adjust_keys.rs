//! The play adjustments' bindings (design.md "Play Adjustments", bindings
//! and dispatch): START, SELECT and the auxiliary keys of each kind of
//! device beside its [`KeyConfig`], the scratch's positive and negative
//! digital directions among its own lane controls, the reversal of its
//! axis, and the refusal of a control given two roles. A plan numbers the
//! bound controls for the input side's tap and the reducer, and turns the
//! tap's observations into the reducer's.

use std::fmt;

use eval::ControlId as InputControl;
use input::hid::is_axis;
use input::raw::key_identity;
use input::{AdjustmentBindings, DeviceKind, MAX_TAPPED, TapControl, TapEvent, TapObservation};

use crate::adjustment::{
    AxisQuantum, ControlId, ControlRoles, Event, HeldSet, Observation, REPEAT_US, Role as Adjust,
};
use crate::keys::KeyConfigs;
use crate::settings::{KeyConfig, Role as KeyRole, reserved_key};
use crate::tuning::Adjust as SettingsKey;

// The tap's numbers are the reducer's controls, and both sets are a u128.
const _: () = assert!(MAX_TAPPED == crate::adjustment::MAX_CONTROLS && MAX_TAPPED == 128);

/// One kind of device's adjustment roles; each a keyboard scan code or a
/// controller's button, `None` when unbound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdjustmentKeys {
    /// START.
    pub start: Option<u32>,
    /// SELECT.
    pub select: Option<u32>,
    /// The auxiliary negative key.
    pub aux_negative: Option<u32>,
    /// The auxiliary positive key.
    pub aux_positive: Option<u32>,
    /// The scratch's positive digital direction: one of the scratch lane's
    /// own controls.
    pub scratch_positive: Option<u32>,
    /// The scratch's negative digital direction, likewise.
    pub scratch_negative: Option<u32>,
}

impl AdjustmentKeys {
    /// Every keyboard's: Q and W, Up and Down, and Left Shift (the default
    /// scratch key) as the positive scratch.
    pub const KEYBOARD: Self = Self {
        start: Some(0x10),
        select: Some(0x11),
        aux_negative: Some(0xE048),
        aux_positive: Some(0xE050),
        scratch_positive: Some(0x2a),
        scratch_negative: None,
    };

    /// The controller's: nothing until captured.
    pub const CONTROLLER: Self = Self {
        start: None,
        select: None,
        aux_negative: None,
        aux_positive: None,
        scratch_positive: None,
        scratch_negative: None,
    };

    /// Every binding with its role.
    pub fn roles(&self) -> impl Iterator<Item = (u32, Adjust)> + '_ {
        [
            (self.start, Adjust::Start),
            (self.select, Adjust::Select),
            (self.aux_negative, Adjust::AuxNegative),
            (self.aux_positive, Adjust::AuxPositive),
            (self.scratch_positive, Adjust::ScratchPositive),
            (self.scratch_negative, Adjust::ScratchNegative),
        ]
        .into_iter()
        .filter_map(|(c, r)| c.map(|c| (c, r)))
    }

    /// Checks these beside `keys`, of a kind of device: START, SELECT and
    /// the auxiliary keys take buttons or keys that no lane, retry, quit
    /// or other role has, and no key the game reserves; a scratch
    /// direction takes one of the scratch lane's own buttons or keys, each
    /// direction its own. Keyboard keys are compared as the window tells
    /// them apart.
    ///
    /// # Errors
    ///
    /// The first binding that breaks a rule.
    pub fn check(&self, keys: &KeyConfig, kind: DeviceKind) -> Result<(), AdjustKeyError> {
        let identity = |c: u32| match kind {
            DeviceKind::Keyboard => key_identity(InputControl(c)).0,
            DeviceKind::Controller => c,
        };
        let mut seen: Vec<(u32, AdjustBinding)> = keys
            .roles()
            .map(|(c, r)| (identity(c), AdjustBinding::Key(r)))
            .collect();
        for (control, role) in self.roles() {
            let key = identity(control);
            if kind == DeviceKind::Controller && is_axis(InputControl(control)) {
                return Err(AdjustKeyError::Axis(role));
            }
            // A scratch direction accompanies its own lane binding.
            let scratch = matches!(role, Adjust::ScratchPositive | Adjust::ScratchNegative);
            let own = |&(k, b): &(u32, AdjustBinding)| {
                scratch && k == key && b == AdjustBinding::Key(KeyRole::Lane(0))
            };
            if scratch && !seen.iter().any(own) {
                return Err(AdjustKeyError::NotScratch { control, role });
            }
            if !scratch
                && kind == DeviceKind::Keyboard
                && let Some(reserved_for) = reserved_key(key)
            {
                return Err(AdjustKeyError::Reserved {
                    control,
                    reserved_for,
                });
            }
            if let Some(&(_, first)) = seen.iter().find(|e| e.0 == key && !own(e)) {
                return Err(AdjustKeyError::Overlap {
                    control,
                    first,
                    second: AdjustBinding::Adjustment(role),
                });
            }
            seen.push((key, AdjustBinding::Adjustment(role)));
        }
        Ok(())
    }
}

/// A role a control can have, for [`AdjustKeyError::Overlap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdjustBinding {
    /// A lane, retry or quit.
    Key(KeyRole),
    /// An adjustment role.
    Adjustment(Adjust),
}

impl fmt::Display for AdjustBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(r) => r.fmt(f),
            Self::Adjustment(r) => f.write_str(name(*r)),
        }
    }
}

/// The adjustment role's name, for messages.
const fn name(role: Adjust) -> &'static str {
    match role {
        Adjust::Start => "START",
        Adjust::Select => "SELECT",
        Adjust::AuxNegative => "the auxiliary negative key",
        Adjust::AuxPositive => "the auxiliary positive key",
        Adjust::ScratchPositive => "the scratch's positive direction",
        Adjust::ScratchNegative => "the scratch's negative direction",
        Adjust::White => "a white key",
        Adjust::Black => "a black key",
        Adjust::Slower => "slower",
        Adjust::Faster => "faster",
    }
}

/// Why adjustment bindings cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdjustKeyError {
    /// One control given two roles.
    #[error("control {control:#x} is bound to both {first} and {second}")]
    Overlap {
        /// The control.
        control: u32,
        /// The role it had first.
        first: AdjustBinding,
        /// The role it was given again.
        second: AdjustBinding,
    },
    /// A keyboard key the game reserves for something else.
    #[error("key {control:#x} is reserved for {reserved_for}")]
    Reserved {
        /// The key's scan code.
        control: u32,
        /// What the game uses it for.
        reserved_for: &'static str,
    },
    /// A digital role bound to a controller's axis.
    #[error("{} takes a button or a key, not an axis", name(*.0))]
    Axis(Adjust),
    /// A scratch direction bound to a control the scratch lane does not
    /// have.
    #[error("{} takes one of the scratch's own controls, not {control:#x}", name(*role))]
    NotScratch {
        /// The control.
        control: u32,
        /// The direction.
        role: Adjust,
    },
    /// More controls than the tap observes.
    #[error("more than {MAX_TAPPED} adjustment controls")]
    TooMany,
}

/// Both kinds of device's adjustment bindings and the scratch axis's
/// reversal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdjustmentConfig {
    /// Every keyboard's.
    pub keyboard: AdjustmentKeys,
    /// The controller's.
    pub controller: AdjustmentKeys,
    /// Whether the scratch axis adjusts the other way from its movement.
    pub reversed: bool,
}

impl Default for AdjustmentConfig {
    fn default() -> Self {
        Self {
            keyboard: AdjustmentKeys::KEYBOARD,
            controller: AdjustmentKeys::CONTROLLER,
            reversed: false,
        }
    }
}

impl AdjustmentConfig {
    /// Checks both kinds ([`AdjustmentKeys::check`]).
    ///
    /// # Errors
    ///
    /// The first binding that breaks a rule.
    pub fn check(&self, keys: &KeyConfigs) -> Result<(), AdjustKeyError> {
        self.keyboard.check(&keys.keyboard, DeviceKind::Keyboard)?;
        self.controller
            .check(&keys.controller, DeviceKind::Controller)
    }

    /// Numbers every adjustment control of `keys` for the tap and the
    /// reducer: these roles, the lane keys as white and black keys, F1
    /// and F2, and the controller's scratch axis: a wrapping axis steps
    /// by its revolution as each packet counted it, any other by `quantum`
    /// (none without one),
    /// its movement counted the short way round `wrap` (the declared
    /// revolution when `None`). The controller's controls are tapped on
    /// its model only.
    ///
    /// # Errors
    ///
    /// As [`check`](Self::check), or more controls than [`MAX_TAPPED`].
    pub fn plan(
        &self,
        keys: &KeyConfigs,
        quantum: Option<AxisQuantum>,
        wrap: Option<u32>,
    ) -> Result<AdjustmentPlan, AdjustKeyError> {
        self.check(keys)?;
        let mut plan = AdjustmentPlan {
            roles: ControlRoles::default(),
            bindings: AdjustmentBindings {
                controls: Vec::new(),
                latched: 0,
                wrap,
                repeating: 0,
                repeat_us: REPEAT_US,
            },
            axis: None,
            reversed: self.reversed,
        };
        let speed = [
            (SettingsKey::HiSpeedDown.key(), Adjust::Slower),
            (SettingsKey::HiSpeedUp.key(), Adjust::Faster),
        ];
        let keyboard = self
            .keyboard
            .roles()
            .chain(lane_roles(&keys.keyboard))
            .chain(speed);
        for (control, role) in keyboard {
            plan.add(TapControl::Key(InputControl(control)), Some(role))?;
        }
        let Some(product) = keys.controller_device else {
            return Ok(plan);
        };
        let tapped = |control| TapControl::Controller {
            product,
            control: InputControl(control),
        };
        for (control, role) in self.controller.roles().chain(lane_roles(&keys.controller)) {
            plan.add(tapped(control), Some(role))?;
        }
        {
            let axis = keys
                .controller
                .lanes
                .iter()
                .find(|k| k.lane == 0 && is_axis(InputControl(k.control)));
            if let Some(axis) = axis {
                let n = plan.add(tapped(axis.control), None)?;
                plan.axis = Some((n, quantum));
            }
        }
        Ok(plan)
    }
}

/// The lane keys as white (1, 3, 5, 7) and black (2, 4, 6) keys; the
/// scratch's controls act only through its directions.
fn lane_roles(keys: &KeyConfig) -> impl Iterator<Item = (u32, Adjust)> + '_ {
    keys.lanes.iter().filter_map(|k| match k.lane {
        1 | 3 | 5 | 7 => Some((k.control, Adjust::White)),
        2 | 4 | 6 => Some((k.control, Adjust::Black)),
        _ => None,
    })
}

/// The adjustment controls numbered for an attempt: the reducer's roles,
/// the tap's bindings and how the tap's observations become the
/// reducer's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdjustmentPlan {
    /// Each number's role.
    pub roles: ControlRoles,
    /// The tap's bindings ([`input::adjustment_queue`]).
    pub bindings: AdjustmentBindings,
    axis: Option<(u8, Option<AxisQuantum>)>,
    reversed: bool,
}

impl AdjustmentPlan {
    /// Gives `control` the next number.
    fn add(&mut self, control: TapControl, role: Option<Adjust>) -> Result<u8, AdjustKeyError> {
        let controls = &mut self.bindings.controls;
        let n = u8::try_from(controls.len())
            .ok()
            .filter(|&n| usize::from(n) < MAX_TAPPED)
            .ok_or(AdjustKeyError::TooMany)?;
        controls.push((control, n));
        if let Some(role) = role {
            self.roles.set(ControlId(n), role);
            if matches!(role, Adjust::Start | Adjust::Select) {
                self.bindings.latched |= 1 << n;
            }
            if matches!(role, Adjust::ScratchPositive | Adjust::ScratchNegative) {
                self.bindings.repeating |= 1 << n;
            }
        }
        Ok(n)
    }

    /// The scratch axis's number, if it is tapped.
    #[must_use]
    pub fn axis(&self) -> Option<u8> {
        self.axis.map(|(n, _)| n)
    }

    /// The reducer's observation of the tap's `o`: its time and sequence,
    /// the scratch axis's movement signed by the reversal and given its
    /// quantum. `None` for an axis the plan does not tap or cannot step, and
    /// for the lifecycle's markers.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn observation(&self, o: &TapObservation) -> Option<Observation> {
        let event = match o.event {
            TapEvent::Press(n) => Event::Press(ControlId(n)),
            TapEvent::Release(n) => Event::Release(ControlId(n)),
            TapEvent::Repeat(n) => Event::Repeat(ControlId(n)),
            TapEvent::Axis {
                control,
                delta,
                revolution,
            } => {
                let (n, quantum) = self.axis.filter(|&(n, _)| n == control)?;
                // A wrapping axis by the revolution it was counted on, so
                // its controller need not be connected at the plan.
                let quantum = AxisQuantum::wrapping(revolution).or(quantum)?;
                Event::Axis {
                    control: ControlId(n),
                    delta: if self.reversed {
                        delta.saturating_neg()
                    } else {
                        delta
                    },
                    quantum,
                }
            }
            TapEvent::Tick => Event::Tick,
            TapEvent::Lost => Event::Lost,
            TapEvent::Resync { held, verified } => Event::Resync {
                held: HeldSet(held),
                verified,
            },
            // The lifecycle's markers are the driver's.
            TapEvent::Opened { .. } | TapEvent::Cancelled { .. } | TapEvent::Fence(_) => {
                return None;
            }
        };
        Some(Observation {
            time: o.at,
            seq: o.seq,
            event,
        })
    }
}
