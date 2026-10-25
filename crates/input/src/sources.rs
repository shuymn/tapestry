//! The input thread's view of devices: their physical state across plays,
//! the frozen set of sources while recording is open, and the arrivals
//! list.

use bms_model::TimeUs;
use eval::{ControlId, InputKind, InputStart, PhysicalInputEvent, SourceId};

use crate::gate::{Admit, DeviceId, FocusGate, Packet};
use crate::queue::{SourceReader, SourceWriter, source_queue};

/// What kind of device a source is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    /// A keyboard.
    Keyboard,
    /// A HID game controller.
    Controller,
}

/// A device that arrived while recording was open. It does not take part
/// in the play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arrival {
    /// The receive time of the batch it arrived in.
    pub at: TimeUs,
    /// What kind of device it is.
    pub kind: DeviceKind,
}

/// The most sources a play can have: one per [`SourceId`].
pub const SOURCE_LIMIT: usize = 1 << 16;

/// Why [`SourceSet::new`] refused a configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// `max_sources` exceeds [`SOURCE_LIMIT`].
    TooManySources(usize),
}

/// Fixed sizes of a [`SourceSet`]; nothing grows beyond them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceConfig {
    /// Devices whose physical state is kept.
    pub max_devices: usize,
    /// Sources that take part in one play.
    pub max_sources: usize,
    /// Controls held at once per device that the state keeps.
    pub max_held: usize,
    /// Events each source's queue holds.
    pub queue_capacity: usize,
    /// Arrivals kept per play.
    pub max_arrivals: usize,
}

/// A known device and its physical state.
#[derive(Debug)]
struct Device {
    id: DeviceId,
    kind: DeviceKind,
    /// Controls with a nonzero value, at most `max_held`.
    held: Vec<(ControlId, i32)>,
    /// Its source while it takes part in the open recording.
    source: Option<SourceId>,
    /// The controls recorded from it: its lane and system bindings, set by
    /// [`SourceSet::set_recorded`].
    recorded: Vec<ControlId>,
    /// A held control did not fit, so `held` may miss one. Kept until the
    /// device is removed; every play it takes part in is incomplete.
    state_lost: bool,
}

/// One participating source of the open recording.
#[derive(Debug)]
struct Participant {
    device: DeviceId,
    kind: DeviceKind,
    writer: SourceWriter,
    closed: bool,
    /// The controls recorded from this source, frozen at the opening.
    recorded: Vec<ControlId>,
}

/// The open recording.
#[derive(Debug)]
struct Recording {
    participants: Vec<Participant>,
    readers: Vec<SourceReader>,
    arrivals: Vec<Arrival>,
    arrivals_dropped: usize,
    overflowed: bool,
    source_limited: bool,
}

/// Devices, their state across plays, and the sources of the open
/// recording. The input thread calls it in batches: [`begin_batch`]
/// (the focus check and the batch's receive time), then packets and
/// device changes, then [`end_batch`] (the publication). Every source
/// publishes at the end of every batch, so a quiet source never stalls the
/// watermark.
///
/// [`begin_batch`]: Self::begin_batch
/// [`end_batch`]: Self::end_batch
#[derive(Debug)]
pub struct SourceSet {
    config: SourceConfig,
    devices: Vec<Device>,
    /// Held lists for devices still to connect, allocated up front so a
    /// device that arrives during play allocates nothing.
    spare_held: Vec<Vec<(ControlId, i32)>>,
    /// Connected devices the state table had no room for.
    untracked: usize,
    gate: FocusGate,
    batch_at: TimeUs,
    recording: Option<Recording>,
}

impl SourceSet {
    /// An empty set. Every buffer the batches use is allocated here or in
    /// [`open_recording`](Self::open_recording).
    ///
    /// # Errors
    ///
    /// [`ConfigError::TooManySources`] if `max_sources` exceeds
    /// [`SOURCE_LIMIT`].
    pub fn new(config: SourceConfig) -> Result<Self, ConfigError> {
        if config.max_sources > SOURCE_LIMIT {
            return Err(ConfigError::TooManySources(config.max_sources));
        }
        let devices = Vec::with_capacity(config.max_devices);
        let spare_held = (0..config.max_devices)
            .map(|_| Vec::with_capacity(config.max_held))
            .collect();
        Ok(Self {
            config,
            devices,
            spare_held,
            untracked: 0,
            gate: FocusGate::new(),
            batch_at: TimeUs(i64::MIN),
            recording: None,
        })
    }

    /// The focus gate.
    #[must_use]
    pub const fn gate(&self) -> &FocusGate {
        &self.gate
    }

    /// The focus gate, to reset it between plays.
    pub const fn gate_mut(&mut self) -> &mut FocusGate {
        &mut self.gate
    }

    /// Starts a batch received at `received`. Returns whether it may be
    /// recorded: `false` once the focus gate has tripped.
    pub fn begin_batch(&mut self, received: TimeUs) -> bool {
        self.batch_at = received;
        self.gate.check()
    }

    /// Ends the batch: each open source publishes `fresh − 1 µs`, where
    /// `fresh` is a clock read taken after the batch was pushed. A later
    /// batch reads the clock after this, so its events are stamped at or
    /// above `fresh` and stay above the publication. The latch is checked
    /// here too, so a focus loss is seen even when no batch follows it;
    /// nothing is published once the gate has tripped, so the play ends at
    /// what was already finalized.
    pub fn end_batch(&mut self, fresh: TimeUs) {
        if !self.gate.check() {
            return;
        }
        let Some(rec) = &self.recording else {
            return;
        };
        let up_to = TimeUs(fresh.0.saturating_sub(1));
        for p in rec.participants.iter().filter(|p| !p.closed) {
            p.writer.publish(up_to);
        }
    }

    /// A packet of the current batch. The device's physical state follows
    /// every packet, admitted or not, so it stays true across plays. The
    /// packet is recorded when the gate admits it, recording is open, its
    /// device takes part, and its control is recorded.
    pub fn packet(&mut self, packet: &Packet) -> Admit {
        let admit = self.gate.admit(packet);
        let max_held = self.config.max_held;
        let Some(device) = self.devices.iter_mut().find(|d| d.id == packet.device) else {
            return admit;
        };
        let source = device.source;
        if !track(&mut device.held, max_held, packet.control, packet.value) {
            // The state table is full; snapshots of this device may miss
            // the control from now on.
            device.state_lost = true;
            if source.is_some()
                && let Some(rec) = &mut self.recording
            {
                rec.overflowed = true;
            }
        }
        if admit == Admit::Record
            && let Some(source) = source
            && let Some(rec) = &self.recording
            && rec.participants[source.index()]
                .recorded
                .contains(&packet.control)
        {
            self.push(
                source,
                InputKind::Control {
                    control: packet.control,
                    value: packet.value,
                },
            );
        }
        admit
    }

    /// A device was found. While recording is open it does not take part;
    /// it goes to the arrivals list, which drops what exceeds its bound.
    pub fn connect(&mut self, device: DeviceId, kind: DeviceKind) {
        if self.devices.iter().any(|d| d.id == device) {
            return;
        }
        if let Some(held) = self.spare_held.pop() {
            self.devices.push(Device {
                id: device,
                kind,
                held,
                source: None,
                recorded: Vec::new(),
                state_lost: false,
            });
        } else {
            self.untracked += 1;
        }
        if let Some(rec) = &mut self.recording {
            if rec.arrivals.len() < self.config.max_arrivals {
                rec.arrivals.push(Arrival {
                    at: self.batch_at,
                    kind,
                });
            } else {
                rec.arrivals_dropped += 1;
            }
        }
    }

    /// Sets the controls recorded from `device`: its lane and system
    /// bindings, worked out by the caller (keyboards share one list by scan
    /// code; each controller has its own). Other controls change only the
    /// physical state, and a device without a list records nothing. The
    /// list is frozen per source when recording opens, so a change applies
    /// from the next opening. Call it outside play: it may allocate.
    /// Returns `false` for a device that is not connected.
    pub fn set_recorded(&mut self, device: DeviceId, controls: Vec<ControlId>) -> bool {
        match self.devices.iter_mut().find(|d| d.id == device) {
            Some(d) => {
                d.recorded = controls;
                true
            }
            None => false,
        }
    }

    /// A device was removed. A participating source records `Disconnected`
    /// at the batch time, publishes that time as its final watermark, and
    /// closes; once the gate has tripped, nothing is recorded or published.
    pub fn disconnect(&mut self, device: DeviceId) {
        let Some(i) = self.devices.iter().position(|d| d.id == device) else {
            self.untracked = self.untracked.saturating_sub(1);
            return;
        };
        // `remove` keeps the order devices were found in, which gives
        // source ids at the next open.
        let mut removed = self.devices.remove(i);
        removed.held.clear();
        self.spare_held.push(removed.held);
        let Some(source) = removed.source else {
            return;
        };
        if self.gate.is_tripped() {
            return;
        }
        self.push(source, InputKind::Disconnected);
        let at = self.batch_at;
        if let Some(rec) = &mut self.recording {
            let p = &mut rec.participants[source.index()];
            p.writer.close(at);
            p.closed = true;
        }
    }

    fn push(&mut self, source: SourceId, kind: InputKind) {
        let received = self.batch_at;
        let Some(rec) = &mut self.recording else {
            return;
        };
        let ev = PhysicalInputEvent {
            received,
            source,
            kind,
        };
        if rec.participants[source.index()].writer.push(ev).is_err() {
            rec.overflowed = true;
        }
    }

    /// Opens recording at a boundary between two batches: the connected
    /// devices become the sources, in the order they were found, up to
    /// `max_sources`, and the recorded controls they hold are copied into
    /// the returned snapshot. Everything after this call, in stream order,
    /// belongs to the play. `now` is the clock read at the boundary.
    /// Readers for the logic thread are then taken with
    /// [`take_readers`](Self::take_readers).
    pub fn open_recording(&mut self, now: TimeUs) -> InputStart {
        self.close_recording();
        let n = self.devices.len().min(self.config.max_sources);
        let source_limited = self.devices.len() > n || self.untracked > 0;
        let mut overflowed = false;
        let mut participants = Vec::with_capacity(n);
        let mut readers = Vec::with_capacity(n);
        let mut held = Vec::new();
        for (i, device) in self.devices.iter_mut().take(n).enumerate() {
            // `SourceSet::new` bounds `max_sources` by `SOURCE_LIMIT`.
            let source = SourceId(u16::try_from(i).expect("source ids fit in u16"));
            device.source = Some(source);
            overflowed |= device.state_lost;
            let recorded = device.recorded.clone();
            held.extend(
                device
                    .held
                    .iter()
                    .filter(|(c, _)| recorded.contains(c))
                    .map(|&(c, v)| (source, c, v)),
            );
            let (writer, reader) = source_queue(self.config.queue_capacity);
            participants.push(Participant {
                device: device.id,
                kind: device.kind,
                writer,
                closed: false,
                recorded,
            });
            readers.push(reader);
        }
        self.recording = Some(Recording {
            participants,
            readers,
            arrivals: Vec::with_capacity(self.config.max_arrivals),
            arrivals_dropped: 0,
            overflowed,
            source_limited,
        });
        InputStart {
            opened_at: now,
            held,
        }
    }

    /// The readers of the open recording, indexed by [`SourceId`]; empty
    /// after the first call.
    pub fn take_readers(&mut self) -> Vec<SourceReader> {
        self.recording
            .as_mut()
            .map(|r| std::mem::take(&mut r.readers))
            .unwrap_or_default()
    }

    /// Closes the recording. Devices keep their physical state.
    pub fn close_recording(&mut self) {
        self.recording = None;
        for d in &mut self.devices {
            d.source = None;
        }
    }

    /// The participating sources' devices and kinds, indexed by
    /// [`SourceId`].
    pub fn participants(&self) -> impl Iterator<Item = (DeviceId, DeviceKind)> + '_ {
        self.recording
            .iter()
            .flat_map(|r| r.participants.iter().map(|p| (p.device, p.kind)))
    }

    /// Devices that arrived during the open recording.
    #[must_use]
    pub fn arrivals(&self) -> &[Arrival] {
        self.recording.as_ref().map_or(&[], |r| &r.arrivals)
    }

    /// Arrivals beyond the list's bound, which were not kept.
    #[must_use]
    pub fn arrivals_dropped(&self) -> usize {
        self.recording.as_ref().map_or(0, |r| r.arrivals_dropped)
    }

    /// Whether an event could not be queued or a participating device's
    /// held state was lost (now or before the opening): the log is
    /// incomplete.
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.recording.as_ref().is_some_and(|r| r.overflowed)
    }

    /// Whether a connected device did not take part because of the source
    /// limit: the log is incomplete.
    #[must_use]
    pub fn source_limited(&self) -> bool {
        self.recording.as_ref().is_some_and(|r| r.source_limited)
    }
}

/// Updates one control in a device's held list. Returns `false` when a
/// newly held control does not fit.
fn track(held: &mut Vec<(ControlId, i32)>, max: usize, control: ControlId, value: i32) -> bool {
    match held.iter().position(|&(c, _)| c == control) {
        Some(i) if value == 0 => {
            held.swap_remove(i);
        }
        Some(i) => held[i].1 = value,
        None if value == 0 => {}
        None if held.len() < max => held.push((control, value)),
        None => return false,
    }
    true
}
