//! The menus' shared input (design.md "Input", "Menu input"): the window's
//! keys and the controller's menu events, read only by the screen that owns
//! them: selection, a result or the practice page.

use std::sync::Arc;

use game::{MenuCommand, PracticeInput};

use crate::practice_page::{practice_event, practice_key};
use crate::select::{MenuInput, MenuQueue, result_event, result_key};

/// The controller's menu events from the input thread, shared by the select
/// screen, the result screen and the practice page.
#[derive(Default)]
pub struct Pad {
    /// Its queue; none without a configured controller.
    pub receiver: Option<input::MenuReceiver>,
    /// The events its queue dropped, as last reported.
    pub dropped: u64,
}

impl Pad {
    /// Takes the next event.
    pub fn pop(&mut self) -> Option<input::MenuEvent> {
        self.receiver.as_mut()?.pop()
    }

    /// Drops what is queued.
    pub fn clear(&mut self) {
        while self.pop().is_some() {}
    }

    /// Reports events the full queue dropped since the last report.
    pub fn report_dropped(&mut self, screen: &str) {
        let Some(pad) = &self.receiver else {
            return;
        };
        let dropped = pad.dropped();
        if dropped > self.dropped {
            let line = format!(
                "{screen}: {} controller menu events dropped, their queue was full",
                dropped - self.dropped
            );
            eprintln!("{line}");
            log::warn!("{line}");
            self.dropped = dropped;
        }
    }
}

/// Which screen reads the menus' queues.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Owner {
    /// None: a play, or loading.
    #[default]
    None,
    /// The select screen.
    Select,
    /// A result.
    Result,
    /// The practice page.
    Practice,
}

/// The menus' input (design.md "Input"): the window's keys and the
/// controller's menu events, read only by their owner, so no screen takes
/// another's input.
pub struct ResultInput {
    /// What the window hands the menus.
    pub menu: Arc<MenuQueue>,
    /// The controller's menu events.
    pub pad: Pad,
    /// Which screen reads them.
    owner: Owner,
}

impl ResultInput {
    /// Input with nothing queued, no controller, and no owner.
    pub fn new(menu: Arc<MenuQueue>, pad: Option<input::MenuReceiver>) -> Self {
        Self {
            menu,
            pad: Pad {
                receiver: pad,
                dropped: 0,
            },
            owner: Owner::None,
        }
    }

    /// Drops everything queued: what arrived while no screen read it
    /// belongs to no screen.
    pub fn clear(&mut self) {
        while self.menu.pop().is_some() {}
        self.pad.clear();
    }

    /// Which screen reads the queues.
    pub const fn owner(&self) -> Owner {
        self.owner
    }

    /// `owner` reads the queues from now on: on a change, what was queued
    /// for the one before is dropped. Whether it changed.
    pub fn enter_as(&mut self, owner: Owner) -> bool {
        if self.owner == owner {
            return false;
        }
        self.clear();
        self.owner = owner;
        true
    }

    /// A result shows: on entry, what was queued during the play is
    /// dropped. Whether this was the entry.
    pub fn enter(&mut self) -> bool {
        self.enter_as(Owner::Result)
    }

    /// No screen reads the queues (a play is about to start); what arrives
    /// meanwhile is dropped by the next owner.
    pub fn release(&mut self) {
        self.owner = Owner::None;
    }

    /// The next result command queued, while a result shows: the
    /// controller's first, then the window's; keys and text that mean
    /// nothing there are dropped.
    pub fn take(&mut self) -> Option<MenuCommand> {
        if self.owner != Owner::Result {
            return None;
        }
        if let Some(event) = self.pad.pop() {
            return Some(result_event(event));
        }
        self.pad.report_dropped("result");
        while let Some(input) = self.menu.pop() {
            if let MenuInput::Key(key) = input
                && let Some(c) = result_key(key)
            {
                return Some(c);
            }
        }
        None
    }

    /// The next practice page input queued, while the page owns the
    /// queues: the controller's first, then the window's; keys and text
    /// that mean nothing there are dropped.
    pub fn take_practice(&mut self) -> Option<PracticeInput> {
        if self.owner != Owner::Practice {
            return None;
        }
        if let Some(event) = self.pad.pop() {
            return Some(practice_event(event));
        }
        self.pad.report_dropped("practice");
        while let Some(input) = self.menu.pop() {
            if let MenuInput::Key(key) = input
                && let Some(i) = practice_key(key)
            {
                return Some(i);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use eval::ControlId;
    use game::{MenuCommand, PracticeInput};
    use input::hid::HidProduct;
    use input::{
        DeviceId, DeviceKind, MenuAction, MenuBindings, MenuEvent, Packet, SourceConfig, SourceSet,
    };

    use super::{Owner, ResultInput};
    use crate::select::{MenuInput, MenuKey, MenuQueue};

    /// The menus with a controller whose control 1 confirms and 2 moves
    /// down, and its source set to press them.
    fn menus() -> (ResultInput, SourceSet) {
        let product = HidProduct {
            vendor_id: 1,
            product_id: 2,
            interface: None,
        };
        let (tap, pad) = input::menu_queue(MenuBindings {
            product,
            controls: vec![
                (ControlId(1), MenuAction::Press(MenuEvent::Confirm)),
                (ControlId(2), MenuAction::Press(MenuEvent::Down)),
            ],
            step: 2,
            relative: None,
            wrap: None,
        });
        let mut set = SourceSet::new(SourceConfig {
            max_devices: 1,
            max_sources: 1,
            max_held: 4,
            queue_capacity: 4,
            max_arrivals: 1,
        })
        .unwrap();
        set.connect(DeviceId(7), DeviceKind::Controller);
        set.set_product(DeviceId(7), product);
        set.set_menu(tap);
        set.menu_mut().unwrap().set_active(true);
        (
            ResultInput::new(Arc::new(MenuQueue::default()), Some(pad)),
            set,
        )
    }

    fn press(set: &mut SourceSet, control: u32) {
        for value in [1, 0] {
            set.packet(&Packet {
                device: DeviceId(7),
                control: ControlId(control),
                value,
                background: false,
            });
        }
    }

    /// After a round's opening was refused, the practice page owns the
    /// queues again: a pad Down queued during the refused play is dropped,
    /// one pressed after reaches the page, and a result reads nothing.
    #[test]
    fn controller_navigates_the_practice_page_after_a_logic_refusal() {
        let (mut menus, mut set) = menus();
        assert!(menus.enter_as(Owner::Practice));
        // Start: the play opens and is refused at its opening.
        menus.release();
        press(&mut set, 2);
        // The routine of every return.
        menus.enter_as(Owner::Practice);
        menus.clear();
        assert_eq!(menus.owner(), Owner::Practice);
        assert_eq!(menus.take_practice(), None);
        press(&mut set, 2);
        assert_eq!(menus.take(), None);
        assert_eq!(menus.take_practice(), Some(PracticeInput::Down));
        assert_eq!(menus.take_practice(), None);
    }

    /// A round's result owns the queues while it shows, and Back returns
    /// them to the practice page: the pad then reaches the page, never the
    /// result, and the window's arrows too.
    #[test]
    fn controller_navigates_after_result_back_to_practice() {
        let (mut menus, mut set) = menus();
        menus.enter_as(Owner::Practice);
        menus.release();
        assert!(menus.enter());
        press(&mut set, 2);
        assert_eq!(menus.take_practice(), None);
        assert_eq!(menus.take(), Some(MenuCommand::NextPreset));
        // Back to the page: what the result left queued is dropped.
        press(&mut set, 1);
        menus.menu.push(MenuInput::Key(MenuKey::Left));
        assert!(menus.enter_as(Owner::Practice));
        menus.clear();
        assert_eq!(menus.take_practice(), None);
        press(&mut set, 2);
        menus.menu.push(MenuInput::Key(MenuKey::Right));
        assert_eq!(menus.take(), None);
        assert_eq!(menus.take_practice(), Some(PracticeInput::Down));
        assert_eq!(menus.take_practice(), Some(PracticeInput::Right));
        // The select screen's keys mean nothing on the page.
        menus.menu.push(MenuInput::Key(MenuKey::Tab));
        menus.menu.push(MenuInput::Text("p".to_owned()));
        assert_eq!(menus.take_practice(), None);
    }
}
