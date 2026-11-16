//! The loaded pattern (docs/test-device.md "Patterns"): items queued per
//! endpoint, each sent as its own report once due, so a change never
//! overwrites one not yet sent.

use crate::protocol::Item;
use crate::trace::Ep;

/// Why an item was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The endpoint's queue is full.
    Full,
    /// The item is earlier than the last one on its endpoint.
    Order,
    /// The pattern is armed.
    Armed,
}

/// One endpoint's items in order, each with its index in the pattern.
#[derive(Debug)]
struct Queue<const N: usize> {
    items: [Option<(u32, Item)>; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Queue<N> {
    const fn new() -> Self {
        Self {
            items: [None; N],
            head: 0,
            len: 0,
        }
    }

    fn last_at(&self) -> Option<u64> {
        let i = (self.head + self.len).checked_sub(1)? % N;
        self.items
            .get(i)
            .copied()
            .flatten()
            .map(|(_, item)| item.at)
    }

    fn push(&mut self, index: u32, item: Item) -> Result<(), Refused> {
        if self.len == N {
            return Err(Refused::Full);
        }
        if self.last_at().is_some_and(|last| item.at < last) {
            return Err(Refused::Order);
        }
        if let Some(slot) = self.items.get_mut((self.head + self.len) % N) {
            *slot = Some((index, item));
            self.len += 1;
        }
        Ok(())
    }

    fn front(&self) -> Option<(u32, Item)> {
        if self.len == 0 {
            return None;
        }
        self.items.get(self.head).copied().flatten()
    }

    fn pop(&mut self) {
        if self.len > 0 {
            if let Some(slot) = self.items.get_mut(self.head) {
                *slot = None;
            }
            self.head = (self.head + 1) % N;
            self.len -= 1;
        }
    }

    fn clear(&mut self) {
        *self = Self::new();
    }
}

/// The loaded pattern: up to `N` items per endpoint.
#[derive(Debug)]
pub struct Schedule<const N: usize> {
    keyboard: Queue<N>,
    pad: Queue<N>,
    loaded: u32,
    start: Option<u64>,
}

impl<const N: usize> Default for Schedule<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Schedule<N> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            keyboard: Queue::new(),
            pad: Queue::new(),
            loaded: 0,
            start: None,
        }
    }

    const fn queue(&mut self, ep: Ep) -> &mut Queue<N> {
        match ep {
            Ep::Keyboard => &mut self.keyboard,
            Ep::Pad => &mut self.pad,
        }
    }

    /// Forgets the pattern and its start: `load`, and every disarm.
    pub fn clear(&mut self) {
        self.keyboard.clear();
        self.pad.clear();
        self.loaded = 0;
        self.start = None;
    }

    /// Appends `item`.
    ///
    /// # Errors
    ///
    /// [`Refused`].
    pub fn push(&mut self, item: Item) -> Result<(), Refused> {
        if self.start.is_some() {
            return Err(Refused::Armed);
        }
        let index = self.loaded;
        self.queue(item.ep()).push(index, item)?;
        self.loaded += 1;
        Ok(())
    }

    /// Plays the pattern from device time `start`.
    pub const fn arm(&mut self, start: u64) {
        self.start = Some(start);
    }

    #[must_use]
    pub const fn armed(&self) -> bool {
        self.start.is_some()
    }

    /// When `ep`'s next item is due, while armed.
    #[must_use]
    pub fn next_due_at(&mut self, ep: Ep) -> Option<u64> {
        let start = self.start?;
        self.queue(ep)
            .front()
            .map(|(_, item)| start.saturating_add(item.at))
    }

    /// `ep`'s next item and its index in the pattern, once due at `now`.
    pub fn take_due(&mut self, ep: Ep, now: u64) -> Option<(u32, Item)> {
        let due = self.next_due_at(ep)?;
        if now < due {
            return None;
        }
        let q = self.queue(ep);
        let front = q.front();
        q.pop();
        front
    }

    /// Whether every armed item has been taken.
    #[must_use]
    pub const fn finished(&self) -> bool {
        self.keyboard.len == 0 && self.pad.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Change, Control};

    fn item(at: u64, control: Control) -> Item {
        Item::new(at, &[Change { control, value: 1 }]).unwrap()
    }

    #[test]
    fn items_come_due_in_order_per_endpoint() {
        let mut s = Schedule::<4>::new();
        s.push(item(0, Control::Button(1))).unwrap();
        s.push(item(5, Control::Key(4))).unwrap();
        s.push(item(5, Control::Button(2))).unwrap();
        assert_eq!(s.push(item(4, Control::Button(3))), Err(Refused::Order));
        assert_eq!(s.take_due(Ep::Pad, 1_000), None, "not armed");
        s.arm(100);
        assert_eq!(s.push(item(9, Control::Button(3))), Err(Refused::Armed));
        assert_eq!(s.take_due(Ep::Pad, 100).map(|(i, _)| i), Some(0));
        assert_eq!(s.take_due(Ep::Pad, 104), None);
        assert_eq!(s.next_due_at(Ep::Pad), Some(105));
        assert_eq!(
            s.take_due(Ep::Pad, 200).map(|(i, _)| i),
            Some(2),
            "one item per call"
        );
        assert_eq!(s.take_due(Ep::Keyboard, 200).map(|(i, _)| i), Some(1));
        assert!(s.finished());
    }

    #[test]
    fn a_full_queue_refuses_and_a_clear_disarms() {
        let mut s = Schedule::<2>::new();
        s.push(item(0, Control::Button(1))).unwrap();
        s.push(item(1, Control::Button(1))).unwrap();
        assert_eq!(s.push(item(2, Control::Button(1))), Err(Refused::Full));
        s.push(item(2, Control::Key(4))).unwrap();
        s.arm(0);
        s.clear();
        assert!(!s.armed());
        assert_eq!(s.take_due(Ep::Pad, u64::MAX), None);
        assert!(s.finished());
    }
}
