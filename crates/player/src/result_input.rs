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

    /// Takes the next event with its panel, the ingress word it was sent
    /// under and the time it was observed
    /// ([`input::MenuReceiver::pop_stamped`]).
    pub fn pop_stamped(
        &mut self,
    ) -> Option<(
        input::MenuEvent,
        Option<usize>,
        input::Ingress,
        bms_model::TimeUs,
    )> {
        self.receiver.as_mut()?.pop_stamped()
    }

    /// Drops what is queued.
    pub fn clear(&mut self) {
        while self.pop().is_some() {}
    }

    /// Drops the events observed by `at`, at it included.
    pub fn drop_through(&mut self, at: bms_model::TimeUs) {
        if let Some(r) = self.receiver.as_mut() {
            r.drop_through(at);
        }
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

    /// A result shows after a full play's fade phase that ended at
    /// `takeover`: as [`enter`](Self::enter), keeping what the window and
    /// the controller observed after `takeover`, the result's own
    /// however late the game took the queues over (design.md "State
    /// Model", ResultPreparing).
    pub fn enter_from(&mut self, takeover: bms_model::TimeUs) -> bool {
        if self.owner == Owner::Result {
            return false;
        }
        self.menu.drop_through(takeover);
        self.pad.drop_through(takeover);
        self.owner = Owner::Result;
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bms_model::TimeUs;
    use game::MenuCommand;

    use super::ResultInput;
    use crate::select::{MenuInput, MenuKey, MenuQueue, Observed};

    /// A result entered after a fade phase keeps the keys the window
    /// observed after the phase's takeover, however late it is entered,
    /// and drops those observed by it and the game thread's own.
    #[test]
    fn a_result_entered_after_a_fade_keeps_what_came_after_its_takeover() {
        let menu = Arc::new(MenuQueue::default());
        let at = |us| {
            Some(Observed {
                stamp: input::Ingress::INITIAL,
                at: TimeUs(us),
            })
        };
        menu.push_stamped(MenuInput::Key(MenuKey::Up), at(490));
        menu.push(MenuInput::Key(MenuKey::Up));
        menu.push_stamped(MenuInput::Key(MenuKey::Up), at(500));
        menu.push_stamped(MenuInput::Key(MenuKey::Down), at(505));
        menu.push_stamped(MenuInput::Key(MenuKey::Up), at(506));
        let mut input = ResultInput::new(Arc::clone(&menu), None);
        assert!(input.enter_from(TimeUs(500)));
        assert_eq!(input.take(), Some(MenuCommand::NextPreset));
        assert_eq!(input.take(), Some(MenuCommand::PrevPreset));
        assert_eq!(input.take(), None);
        // Entered again, it keeps what came since.
        menu.push_stamped(MenuInput::Key(MenuKey::Down), at(400));
        assert!(!input.enter_from(TimeUs(500)));
        assert_eq!(input.take(), Some(MenuCommand::NextPreset));
    }
}
