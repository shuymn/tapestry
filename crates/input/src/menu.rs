//! Controller menu input (design.md "Input"): while the input thread is
//! Idle and the window has focus, the bound controls of the configured
//! controller become menu events for the select screen. They go through a
//! bounded single-producer queue to the game thread and are never
//! recorded.
//!
//! What each control does is the caller's table ([`MenuBindings`]): a
//! button press is one event, and an axis turned `step` units in one
//! direction, taken the short way round its revolution, is one event for
//! that direction; the count starts again when the direction changes.
//! A packet received in the background, any packet while the menu is not
//! [`active`](MenuTap::set_active), and a packet of any other device never
//! reach the queue.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use eval::ControlId;

use crate::gate::{DeviceId, Packet};
use crate::hid::{AxisCap, HidProduct, wrapped};

/// Events the queue holds; one past them is dropped and counted.
pub const MENU_QUEUE: usize = 64;

/// What a menu control does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEvent {
    /// Confirm the line under the cursor.
    Confirm,
    /// Go back.
    Back,
    /// Move the cursor up one line.
    Up,
    /// Move the cursor down one line.
    Down,
}

/// What one bound control sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// A button: each press sends the event.
    Press(MenuEvent),
    /// An axis: each `step` of movement toward higher values sends
    /// `forward`, toward lower values `backward`.
    Turn {
        /// The event of movement toward higher values.
        forward: MenuEvent,
        /// The event of movement toward lower values.
        backward: MenuEvent,
    },
}

/// The controls that send menu events, of one controller model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuBindings {
    /// The controller: the first connected unit of this model and function
    /// sends them, as the first one plays.
    pub product: HidProduct,
    /// Each control and what it sends.
    pub controls: Vec<(ControlId, MenuAction)>,
    /// The movement of an axis that sends one event, at least 1.
    pub step: u32,
}

/// The input thread's end: the bindings, each axis's movement so far, and
/// the queue.
#[derive(Debug)]
pub struct MenuTap {
    product: HidProduct,
    /// Each control, what it sends, and an axis's movement counted toward
    /// its next event.
    controls: Vec<(ControlId, MenuAction, i64)>,
    step: i64,
    /// The device whose movement is counted.
    counting: Option<DeviceId>,
    active: bool,
    /// Events queued since [`take_sent`](Self::take_sent).
    sent: bool,
    queue: rtrb::Producer<MenuEvent>,
    dropped: Arc<AtomicU64>,
}

/// The game thread's end of the queue.
#[derive(Debug)]
pub struct MenuReceiver {
    queue: rtrb::Consumer<MenuEvent>,
    dropped: Arc<AtomicU64>,
}

/// A tap for `bindings` and its receiver; the queue holds
/// [`MENU_QUEUE`] events. It allocates; nothing does after it.
#[must_use]
pub fn menu_queue(bindings: MenuBindings) -> (MenuTap, MenuReceiver) {
    let (producer, consumer) = rtrb::RingBuffer::new(MENU_QUEUE);
    let dropped = Arc::new(AtomicU64::new(0));
    (
        MenuTap {
            product: bindings.product,
            controls: bindings
                .controls
                .into_iter()
                .map(|(c, a)| (c, a, 0))
                .collect(),
            step: i64::from(bindings.step.max(1)),
            counting: None,
            active: false,
            sent: false,
            queue: producer,
            dropped: Arc::clone(&dropped),
        },
        MenuReceiver {
            queue: consumer,
            dropped,
        },
    )
}

impl MenuTap {
    /// Whether packets become menu events: set at each boundary, true only
    /// while the thread is Idle and the window has focus. Turning it off
    /// forgets the movement counted.
    pub fn set_active(&mut self, active: bool) {
        if !active {
            self.reset();
        }
        self.active = active;
    }

    /// Whether packets become menu events now.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// The controller model the bindings belong to.
    #[must_use]
    pub const fn product(&self) -> HidProduct {
        self.product
    }

    fn reset(&mut self) {
        self.counting = None;
        for (_, _, travel) in &mut self.controls {
            *travel = 0;
        }
    }

    /// Whether an event was queued since the last call; clears it.
    pub fn take_sent(&mut self) -> bool {
        std::mem::take(&mut self.sent)
    }

    /// One packet of the configured controller's first unit, with the
    /// axis's capability and its previous position (`None` for a button,
    /// an axis not seen yet, or a relative axis). Sends what its binding
    /// says, if the menu is active and the packet was received in the
    /// foreground.
    pub fn offer(&mut self, packet: &Packet, axis: Option<(&AxisCap, Option<i32>)>) {
        if !self.active || packet.background {
            return;
        }
        if self.counting != Some(packet.device) {
            self.reset();
            self.counting = Some(packet.device);
        }
        let step = self.step;
        let Some((_, action, travel)) = self
            .controls
            .iter_mut()
            .find(|(c, _, _)| *c == packet.control)
        else {
            return;
        };
        let event = match (*action, axis) {
            (MenuAction::Press(event), None) => (packet.value != 0).then_some(event),
            (MenuAction::Turn { forward, backward }, Some((cap, prev))) => {
                let moved = if cap.relative {
                    i64::from(packet.value)
                } else {
                    prev.map_or(0, |p| {
                        wrapped(i64::from(packet.value) - i64::from(p), cap.wrap)
                    })
                };
                if moved.signum() * travel.signum() < 0 {
                    *travel = 0;
                }
                *travel = travel.saturating_add(moved);
                if travel.abs() >= step {
                    let event = if *travel > 0 { forward } else { backward };
                    *travel = 0;
                    Some(event)
                } else {
                    None
                }
            }
            // A button bound as an axis or the other way round sends
            // nothing.
            _ => None,
        };
        if let Some(event) = event {
            self.send(event);
        }
    }

    fn send(&mut self, event: MenuEvent) {
        if self.queue.push(event).is_ok() {
            self.sent = true;
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl MenuReceiver {
    /// Takes the oldest event.
    pub fn pop(&mut self) -> Option<MenuEvent> {
        self.queue.pop().ok()
    }

    /// Events dropped so far because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
