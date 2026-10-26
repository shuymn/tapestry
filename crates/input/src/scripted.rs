//! Fake input: a scenario or a recorded log fed by the clock, and autoplay.

use bms_model::{Chart, Lane, NoteKind, TimeUs};
use eval::{
    AxisMode, ControlId, InputKind, InputStart, PhysicalInputEvent, SourceId, SourceProfile,
};

use crate::InputAdapter;
use crate::gate::{DeviceId, Packet};
use crate::hid::AxisCap;
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
/// publication, as the input thread's wake does. A device the script
/// connects gets the recorded controls given by
/// [`with_recorded`](Self::with_recorded), prepared for each connection
/// beforehand and moved in when it happens, so polling allocates nothing.
#[derive(Clone, Debug)]
pub struct ScriptedInput {
    items: Vec<ScriptItem>,
    next: usize,
    /// The recorded controls for each `Connect` item, by item index.
    connect_lists: Vec<Option<Vec<ControlId>>>,
    /// The axes each `Connect` item declares, by item index.
    connect_axes: Vec<Vec<AxisCap>>,
}

impl ScriptedInput {
    /// A script of `items`, ordered by time; items with equal times keep
    /// their order.
    #[must_use]
    pub fn new(mut items: Vec<ScriptItem>) -> Self {
        items.sort_by_key(|i| i.at);
        let connect_lists = vec![None; items.len()];
        let connect_axes = vec![Vec::new(); items.len()];
        Self {
            items,
            next: 0,
            connect_lists,
            connect_axes,
        }
    }

    /// Declares `axes` for `device` each time the script connects it, as
    /// the adapter declares a controller's axes
    /// ([`SourceSet::add_axes`]): only declared axes are recorded.
    #[must_use]
    pub fn with_axes(mut self, device: DeviceId, axes: &[AxisCap]) -> Self {
        for (item, list) in self.items.iter().zip(&mut self.connect_axes) {
            if matches!(item.action, Action::Connect(d, _) if d == device) {
                list.clear();
                list.extend_from_slice(axes);
            }
        }
        self
    }

    /// Records `controls` from `device` each time the script connects it.
    #[must_use]
    pub fn with_recorded(mut self, device: DeviceId, controls: Vec<ControlId>) -> Self {
        for (item, list) in self.items.iter().zip(&mut self.connect_lists) {
            if matches!(item.action, Action::Connect(d, _) if d == device) {
                *list = Some(controls.clone());
            }
        }
        self
    }

    /// A script that replays a recorded play. Source `i` becomes device
    /// `DeviceId(i)` of kind `kinds[i]`; the devices connect and press the
    /// held controls at the first poll, which must come before recording
    /// opens at `start.opened_at`. Each device records the controls its
    /// source has in the log. The events follow at their receive times, so
    /// polls after the opening record them again.
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
        // The controls of each source in the log, in one pass.
        let mut buckets: Vec<Vec<ControlId>> = vec![Vec::new(); kinds.len()];
        let controls = start
            .held
            .iter()
            .map(|&(s, c, _)| (s, c))
            .chain(events.iter().filter_map(|e| match e.kind {
                InputKind::Control { control, .. } => Some((e.source, control)),
                InputKind::Disconnected => None,
            }));
        for (source, control) in controls {
            if let Some(bucket) = buckets.get_mut(source.index()) {
                bucket.push(control);
            }
        }
        let mut script = Self::new(items);
        for (i, mut bucket) in buckets.into_iter().enumerate() {
            bucket.sort_unstable();
            bucket.dedup();
            script = script.with_recorded(DeviceId(i as u64), bucket);
        }
        script
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
                let index = self.next;
                self.next += 1;
                match item.action {
                    Action::Connect(device, kind) => {
                        sources.connect(device, kind);
                        if let Some(controls) = self.connect_lists[index].take() {
                            sources.set_recorded(device, controls);
                        }
                        sources.add_axes(device, &self.connect_axes[index]);
                    }
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

/// The axes a recorded source with `profile` declared, as far as its
/// evaluation reads them: each bound axis, relative or absolute with the
/// revolution its scratch settings resolved to ([`ScriptedInput::with_axes`]
/// for a replay).
#[must_use]
pub fn declared_axes(profile: &SourceProfile) -> Vec<AxisCap> {
    profile
        .axes
        .iter()
        .map(|a| match a.scratch.mode {
            AxisMode::Relative => AxisCap {
                control: a.control,
                relative: true,
                wrap: 0,
                min: 0,
                max: 0,
                bits: 0,
                signed: false,
            },
            AxisMode::Absolute { wrap } => AxisCap {
                control: a.control,
                relative: false,
                wrap,
                min: 0,
                max: i32::try_from(wrap.wrapping_sub(1)).unwrap_or(i32::MAX),
                bits: 0,
                signed: false,
            },
        })
        .collect()
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
                    // The minimum is taken in i128, so a next note before
                    // an unrepresentable hold end still gives a release.
                    let held = i128::from(note.time.0) + i128::from(TAP_HOLD.0);
                    let release = notes
                        .get(i + 1)
                        .map_or(held, |next| held.min(i128::from(next.time.0)));
                    TimeUs(i64::try_from(release).map_err(|_| TimeOverflow)?)
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
