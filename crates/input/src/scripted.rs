//! Fake input: a scenario or a recorded log fed by the clock, and autoplay.

use bms_model::{Chart, Lane, NoteKind, TimeUs};
use eval::{ControlId, InputKind, InputStart, PhysicalInputEvent, SourceId, SourceProfile};

use crate::InputAdapter;
use crate::gate::{DeviceId, Packet};
use crate::sources::{DeviceKind, SourceSet};

/// Something that happens to the input side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// A device is found.
    Connect(DeviceId, DeviceKind),
    /// A device is removed.
    Disconnect(DeviceId),
    /// A device reports a control change.
    Packet(Packet),
    /// The window loses focus: the gate's latch is set. Items after it,
    /// even at the same time, belong to a new batch.
    FocusLoss,
}

/// An action at a receive time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScriptItem {
    /// When it is received.
    pub at: TimeUs,
    /// What happens.
    pub action: Action,
}

/// Feeds a script to a [`SourceSet`] as the clock reaches each item. Items
/// with the same time form one batch, and every poll ends with a
/// publication, as the input thread's wake does.
#[derive(Clone, Debug)]
pub struct ScriptedInput {
    items: Vec<ScriptItem>,
    next: usize,
}

impl ScriptedInput {
    /// A script of `items`, ordered by time; items with equal times keep
    /// their order.
    #[must_use]
    pub fn new(mut items: Vec<ScriptItem>) -> Self {
        items.sort_by_key(|i| i.at);
        Self { items, next: 0 }
    }

    /// A script that replays a recorded play. Source `i` becomes device
    /// `DeviceId(i)` of kind `kinds[i]`; the devices connect and press the
    /// held controls at the first poll, which must come before recording
    /// opens at `start.opened_at`. The events follow at their receive
    /// times, so polls after the opening record them again.
    #[must_use]
    pub fn from_log(
        start: &InputStart,
        events: &[PhysicalInputEvent],
        kinds: &[DeviceKind],
    ) -> Self {
        let device = |s: SourceId| DeviceId(u64::from(s.0));
        let first = TimeUs(i64::MIN);
        let mut items: Vec<ScriptItem> = kinds
            .iter()
            .enumerate()
            .map(|(i, &kind)| ScriptItem {
                at: first,
                action: Action::Connect(DeviceId(i as u64), kind),
            })
            .collect();
        items.extend(start.held.iter().map(|&(s, control, value)| ScriptItem {
            at: first,
            action: Action::Packet(Packet {
                device: device(s),
                control,
                value,
                background: false,
            }),
        }));
        items.extend(events.iter().map(|ev| ScriptItem {
            at: ev.received,
            action: match ev.kind {
                InputKind::Control { control, value } => Action::Packet(Packet {
                    device: device(ev.source),
                    control,
                    value,
                    background: false,
                }),
                InputKind::Disconnected => Action::Disconnect(device(ev.source)),
            },
        }));
        Self::new(items)
    }

    /// Whether every item has been fed.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.next == self.items.len()
    }
}

impl InputAdapter for ScriptedInput {
    fn poll(&mut self, now: TimeUs, sources: &mut SourceSet) {
        while let Some(&ScriptItem { at, .. }) = self.items.get(self.next)
            && at <= now
        {
            sources.begin_batch(at);
            while let Some(item) = self.items.get(self.next)
                && item.at == at
            {
                self.next += 1;
                match item.action {
                    Action::Connect(device, kind) => sources.connect(device, kind),
                    Action::Disconnect(device) => sources.disconnect(device),
                    Action::Packet(p) => {
                        sources.packet(&p);
                    }
                    Action::FocusLoss => {
                        sources.gate().latch();
                        break;
                    }
                }
            }
        }
        sources.end_batch(now);
    }
}

/// How long autoplay holds a tap, unless the lane's next note comes first.
const TAP_HOLD: TimeUs = TimeUs(50_000);

/// A note time plus the hold or the input offset left the `i64` range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeOverflow;

/// Inputs that hit every tap and long note of `chart` on time, as source 0
/// with `profile`'s bindings and input offset: each note's control is
/// pressed at the note and released after [`TAP_HOLD`] or at the lane's
/// next note, whichever is first, or at a long note's end. Mines are left
/// alone, and lanes without a binding are skipped. The events are in
/// receive-time order.
///
/// # Errors
///
/// [`TimeOverflow`] if a receive time (note time plus the hold and the
/// input offset) is outside the `i64` range.
pub fn autoplay(
    chart: &Chart,
    profile: &SourceProfile,
) -> Result<Vec<PhysicalInputEvent>, TimeOverflow> {
    let add = |a: TimeUs, b: TimeUs| a.0.checked_add(b.0).map(TimeUs).ok_or(TimeOverflow);
    let ev = |at: TimeUs, control: ControlId, value: i32| {
        Ok(PhysicalInputEvent {
            received: add(at, profile.input_offset)?,
            source: SourceId(0),
            kind: InputKind::Control { control, value },
        })
    };
    let mut events = Vec::new();
    for lane in Lane::all() {
        let Some(control) = profile
            .bindings
            .iter()
            .find(|b| b.lane == lane)
            .map(|b| b.control)
        else {
            continue;
        };
        let notes = &chart.lanes[lane.index()];
        for (i, note) in notes.iter().enumerate() {
            let release = match note.kind {
                NoteKind::Mine { .. } => continue,
                NoteKind::Long { end, .. } => end,
                NoteKind::Tap => {
                    let held = add(note.time, TAP_HOLD)?;
                    notes.get(i + 1).map_or(held, |next| held.min(next.time))
                }
            };
            events.push(ev(note.time, control, 1)?);
            events.push(ev(release, control, 0)?);
        }
    }
    // Stable: a release and the lane's next press at the same time stay in
    // that order.
    events.sort_by_key(|e| e.received);
    Ok(events)
}
