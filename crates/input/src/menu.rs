//! Controller menu input (design.md "Input"): while the input thread is
//! Idle and the window has focus, the bound controls of the configured
//! controller become menu events for the select screen. They go through a
//! bounded single-producer queue to the game thread and are never
//! recorded.
//!
//! What each control does is the caller's table ([`MenuBindings`]): a
//! button press is one event, and an axis sends one event for each whole
//! `step` of movement in one direction, taken the short way round its
//! revolution as play converts it (the revolution and whether it is
//! relative as `device.toml` sets them, else as the device declares
//! them); the remainder counts toward the next event, and the count starts
//! again when the direction changes. A packet received in the background,
//! any packet while the menu is not [`active`](MenuTap::set_active), and a
//! packet of any other device never reach the queue. Turning the menu off,
//! another device sending, or the device's removal forgets the movement
//! counted and every axis's position, so the next report of an axis only
//! says where it is.

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
    /// Whether the axes report movement rather than position, when set
    /// (`device.toml`); else as each declares.
    pub relative: Option<bool>,
    /// The values in an absolute axis's revolution, 0 if it does not wrap,
    /// when set (`device.toml`); else its declared range.
    pub wrap: Option<u32>,
}

/// One bound control and what is counted for it.
#[derive(Debug)]
struct Bound {
    control: ControlId,
    action: MenuAction,
    /// An axis's movement toward its next event.
    travel: i64,
    /// An absolute axis's last position; `None` until it reports.
    last: Option<i32>,
}

/// The input thread's end: the bindings, each axis's movement so far, and
/// the queue.
#[derive(Debug)]
pub struct MenuTap {
    product: HidProduct,
    controls: Vec<Bound>,
    step: i64,
    relative: Option<bool>,
    wrap: Option<u32>,
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
                .map(|(control, action)| Bound {
                    control,
                    action,
                    travel: 0,
                    last: None,
                })
                .collect(),
            step: i64::from(bindings.step.max(1)),
            relative: bindings.relative,
            wrap: bindings.wrap,
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
        if self.active && !active {
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
        for b in &mut self.controls {
            b.travel = 0;
            b.last = None;
        }
    }

    /// `device` was removed: what was counted for it is forgotten, so a
    /// reconnection starts from nothing.
    pub fn removed(&mut self, device: DeviceId) {
        if self.counting == Some(device) {
            self.reset();
        }
    }

    /// Whether an event was queued since the last call; clears it.
    pub fn take_sent(&mut self) -> bool {
        std::mem::take(&mut self.sent)
    }

    /// One packet of the configured controller's first unit, with the
    /// axis's capability (`None` for a button). Sends what its binding
    /// says, if the menu is active and the packet was received in the
    /// foreground.
    pub fn offer(&mut self, packet: &Packet, axis: Option<&AxisCap>) {
        if !self.active || packet.background {
            return;
        }
        if self.counting != Some(packet.device) {
            self.reset();
            self.counting = Some(packet.device);
        }
        let (step, relative, wrap) = (self.step, self.relative, self.wrap);
        let Some(b) = self
            .controls
            .iter_mut()
            .find(|b| b.control == packet.control)
        else {
            return;
        };
        let (event, count) = match (b.action, axis) {
            (MenuAction::Press(event), None) => (event, u64::from(packet.value != 0)),
            (MenuAction::Turn { forward, backward }, Some(cap)) => {
                let value = i64::from(packet.value);
                let moved = if relative.unwrap_or(cap.relative) {
                    value
                } else {
                    // As play converts it: the revolution set, else the
                    // declared one, else none.
                    let wrap = wrap.unwrap_or(if cap.relative { 0 } else { cap.wrap });
                    let prev = b.last.replace(packet.value);
                    prev.map_or(0, |p| wrapped(value - i64::from(p), wrap))
                };
                if moved.signum() * b.travel.signum() < 0 {
                    b.travel = 0;
                }
                b.travel = b.travel.saturating_add(moved);
                // Every whole step sends; the remainder counts on.
                let steps = b.travel / step;
                b.travel -= steps * step;
                let event = if steps > 0 { forward } else { backward };
                (event, steps.unsigned_abs())
            }
            // A button bound as an axis or the other way round sends
            // nothing.
            _ => return,
        };
        self.send(event, count);
    }

    /// Sends `event` `count` times; those past the queue's room are
    /// dropped and counted.
    fn send(&mut self, event: MenuEvent, count: u64) {
        let room = self.queue.slots() as u64;
        let mut over = count.saturating_sub(room);
        for _ in 0..count.min(room) {
            if self.queue.push(event).is_ok() {
                self.sent = true;
            } else {
                over += 1;
            }
        }
        if over > 0 {
            self.dropped.fetch_add(over, Ordering::Relaxed);
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
