//! The result screen's input (design.md "Input", "Play Lifecycle"): the
//! window's keys and the controller's menu events, which the select screen
//! reads too, taken as result commands only while a result shows.

use std::sync::Arc;

use game::MenuCommand;

use crate::select::{MenuInput, MenuQueue, result_event, result_key};

/// The controller's menu events from the input thread, shared by the select
/// screen and the result screen.
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

/// The result screen's input (design.md "Input"): the window's keys and
/// the controller's menu events, read only while a result shows, so the
/// select screen's input is never taken by a result.
pub struct ResultInput {
    /// What the window hands the menus; the select screen reads it too.
    pub menu: Arc<MenuQueue>,
    /// The controller's menu events.
    pub pad: Pad,
    /// Whether a result shows.
    pub showing: bool,
}

impl ResultInput {
    /// Input with nothing queued, no controller, and no result showing.
    pub fn new(menu: Arc<MenuQueue>, pad: Option<input::MenuReceiver>) -> Self {
        Self {
            menu,
            pad: Pad {
                receiver: pad,
                dropped: 0,
            },
            showing: false,
        }
    }

    /// Drops everything queued: what arrived while no screen read it
    /// belongs to no screen.
    pub fn clear(&mut self) {
        while self.menu.pop().is_some() {}
        self.pad.clear();
    }

    /// A result shows: on entry, what was queued during the play is
    /// dropped. Whether this was the entry.
    pub fn enter(&mut self) -> bool {
        if self.showing {
            return false;
        }
        self.clear();
        self.showing = true;
        true
    }

    /// The next result command queued, while a result shows: the
    /// controller's first, then the window's; keys and text that mean
    /// nothing there are dropped.
    pub fn take(&mut self) -> Option<MenuCommand> {
        if !self.showing {
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
}
