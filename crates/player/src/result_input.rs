//! The menus' shared input (design.md "Input", "Menu input"): the window's
//! keys and the controller's menu events, read only by the screen that owns
//! them: selection, a result or the practice page.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

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
    /// A full play's fade phase's takeover, for the result after it, on
    /// the clock, `i64::MIN` for none: what the window or the controller
    /// observed by it is the phase's, dropped wherever and however late it
    /// is taken ([`take`](Self::take)). Shared with the controls'
    /// commands ([`sharing`](Self::sharing)), so one watermark classifies
    /// all of the result's input.
    takeover: Arc<AtomicI64>,
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
            takeover: Arc::new(AtomicI64::new(i64::MIN)),
        }
    }

    /// This input with the controls' watermark `mark`
    /// ([`crate::controls::Controls::takeover_mark`]) as its own.
    #[must_use]
    pub fn sharing(mut self, mark: Arc<AtomicI64>) -> Self {
        mark.store(self.takeover.load(Ordering::Acquire), Ordering::Release);
        self.takeover = mark;
        self
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
        self.takeover.store(i64::MIN, Ordering::Release);
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
    /// `takeover`: as [`enter`](Self::enter), but what the window and the
    /// controller observed after `takeover` is the result's own, however
    /// late the game took the queues over, and what they observed by it
    /// the phase's, however late it is published: the one check, at each
    /// take (design.md "State Model", ResultPreparing).
    pub fn enter_from(&mut self, takeover: bms_model::TimeUs) -> bool {
        if self.owner == Owner::Result {
            return false;
        }
        self.owner = Owner::Result;
        self.takeover.store(takeover.0, Ordering::Release);
        true
    }

    /// The fade phase's takeover the result's input counts from, if the
    /// result followed one ([`enter_from`](Self::enter_from)).
    pub fn takeover(&self) -> Option<bms_model::TimeUs> {
        let at = self.takeover.load(Ordering::Acquire);
        (at != i64::MIN).then_some(bms_model::TimeUs(at))
    }

    /// No screen reads the queues (a play is about to start); what arrives
    /// meanwhile is dropped by the next owner.
    pub fn release(&mut self) {
        self.owner = Owner::None;
        self.takeover.store(i64::MIN, Ordering::Release);
    }

    /// The next result command queued, while a result shows: the
    /// controller's first, then the window's; keys and text that mean
    /// nothing there are dropped, and so is what was observed by a fade
    /// phase's takeover ([`enter_from`](Self::enter_from)).
    pub fn take(&mut self) -> Option<MenuCommand> {
        if self.owner != Owner::Result {
            return None;
        }
        let takeover = self.takeover();
        let fresh = |at: bms_model::TimeUs| takeover.is_none_or(|t| at.0 > t.0);
        while let Some((event, _, _, at)) = self.pad.pop_stamped() {
            if fresh(at) {
                return Some(result_event(event));
            }
        }
        self.pad.report_dropped("result");
        while let Some(entry) = self.menu.pop_observed() {
            let Some((input, observed)) = entry else {
                continue;
            };
            if observed.is_some_and(|o| !fresh(o.at)) {
                continue;
            }
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
    /// and drops those observed by it, one published after the entry
    /// included (a window thread held between its stamp and its push).
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
        menu.push_stamped(MenuInput::Key(MenuKey::Up), at(500));
        menu.push_stamped(MenuInput::Key(MenuKey::Down), at(505));
        let mut input = ResultInput::new(Arc::clone(&menu), None);
        // Stamped by the window before the takeover, pushed after the
        // entry.
        let held = at(495);
        assert!(input.enter_from(TimeUs(500)));
        assert_eq!(input.takeover(), Some(TimeUs(500)));
        menu.push_stamped(MenuInput::Key(MenuKey::Up), held);
        menu.push_stamped(MenuInput::Key(MenuKey::Up), at(506));
        assert_eq!(input.take(), Some(MenuCommand::NextPreset));
        assert_eq!(input.take(), Some(MenuCommand::PrevPreset));
        assert_eq!(input.take(), None);
        // Entered again, it keeps what came since; another owner's entry
        // ends the watermark.
        menu.push_stamped(MenuInput::Key(MenuKey::Down), at(507));
        assert!(!input.enter_from(TimeUs(600)));
        assert_eq!(input.take(), Some(MenuCommand::NextPreset));
        input.release();
        assert_eq!(input.takeover(), None);
    }

    /// The controller's events observed by the fade phase's takeover are
    /// the phase's too, one published after the entry included; those
    /// observed after it are the result's.
    #[test]
    fn a_result_after_a_fade_drops_the_controllers_events_by_its_takeover() {
        use eval::ControlId;
        use input::hid::HidProduct;
        use input::{
            DeviceId, DeviceKind, MenuAction, MenuBindings, MenuEvent, Packet, SourceConfig,
            SourceSet,
        };
        let product = HidProduct {
            vendor_id: 1,
            product_id: 2,
            interface: None,
        };
        let (tap, receiver) = input::menu_queue(MenuBindings {
            product,
            controls: vec![
                (ControlId(1), MenuAction::Press(MenuEvent::Confirm)),
                (ControlId(2), MenuAction::Press(MenuEvent::Back)),
            ],
            step: 2,
            relative: None,
            wrap: None,
        });
        let mut set = SourceSet::new(SourceConfig {
            max_devices: 1,
            max_sources: 1,
            max_held: 4,
            queue_capacity: 8,
            max_arrivals: 1,
        })
        .unwrap();
        set.connect(DeviceId(7), DeviceKind::Controller);
        set.set_product(DeviceId(7), product);
        set.set_menu(tap);
        set.menu_mut().unwrap().set_active(true);
        let press = |set: &mut SourceSet, control: u32, at: i64| {
            set.begin_batch(TimeUs(at));
            for value in [1, 0] {
                set.packet(&Packet {
                    device: DeviceId(7),
                    control: ControlId(control),
                    value,
                    background: false,
                });
            }
            set.end_batch(TimeUs(at));
        };
        let mut input = ResultInput::new(Arc::new(MenuQueue::default()), Some(receiver));
        press(&mut set, 1, 490);
        assert!(input.enter_from(TimeUs(500)));
        // Observed by the takeover, sent after the entry.
        press(&mut set, 1, 500);
        press(&mut set, 2, 505);
        assert_eq!(input.take(), Some(MenuCommand::Back));
        assert_eq!(input.take(), None);
    }
}
