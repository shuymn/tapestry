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

    /// `owner` takes the queues as a screen is entered, every time: what
    /// was queued, for it or for another, is dropped.
    pub fn take_over(&mut self, owner: Owner) {
        self.clear();
        self.owner = owner;
    }

    /// A result shows: on entry, what was queued during the play is
    /// dropped; drawn again, it keeps what came since. Whether this was the
    /// entry.
    pub fn enter(&mut self) -> bool {
        if self.owner == Owner::Result {
            return false;
        }
        self.take_over(Owner::Result);
        true
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
