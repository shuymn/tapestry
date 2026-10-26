//! The input thread's view of devices: their physical state across plays,
//! the frozen set of sources while recording is open, and the arrivals
//! list.

use bms_model::TimeUs;
use eval::{ControlId, InputKind, InputStart, PhysicalInputEvent, SourceId};

use crate::gate::{Admit, DeviceId, FocusGate, Packet};
use crate::hid::{AxisCap, MAX_AXES};
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
    /// Controls with a nonzero value, at most `max_held`; not absolute
    /// axes, whose positions are kept in `positions`.
    held: Vec<(ControlId, i32)>,
    /// A controller's axes, from [`SourceSet::add_axes`].
    axes: [AxisCap; MAX_AXES],
    n_axes: usize,
    /// Each absolute axis's last position, 0 included; `None` until seen.
    positions: [Option<i32>; MAX_AXES],
    /// Its source while it takes part in the open recording.
    source: Option<SourceId>,
    /// The controls recorded from it: its lane and system bindings, set by
    /// [`SourceSet::set_recorded`].
    recorded: Vec<ControlId>,
    /// A held control did not fit, so `held` may miss one. Kept until the
    /// device is removed; every play it takes part in is incomplete.
    state_lost: bool,
    /// Which connection this is: a reconnect of the same device gets a new
    /// number.
    connection: u64,
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
    /// Axis values it reported outside their declared range.
    out_of_range: u64,
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
    /// Where the play's time 0 lies on the batch clock: every time recorded
    /// or published is the batch time minus this.
    origin: TimeUs,
}

impl Recording {
    /// `at` on the batch clock as the play's time.
    const fn play_time(&self, at: TimeUs) -> TimeUs {
        TimeUs(at.0.saturating_sub(self.origin.0))
    }
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
    /// Devices the adapter has no room to read, connected now; each is
    /// counted once in `untracked`.
    unread: Vec<DeviceId>,
    /// The number the next connection gets.
    next_connection: u64,
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
        let unread = Vec::with_capacity(config.max_devices);
        let spare_held = (0..config.max_devices)
            .map(|_| Vec::with_capacity(config.max_held))
            .collect();
        Ok(Self {
            config,
            devices,
            spare_held,
            untracked: 0,
            unread,
            next_connection: 0,
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

    /// Ends the batch: each open source publishes `fresh − 1 µs` (on the
    /// play's timeline), where `fresh` is a clock read taken after the batch
    /// was pushed. A later
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
        let up_to = TimeUs(rec.play_time(fresh).0.saturating_sub(1));
        for p in rec.participants.iter().filter(|p| !p.closed) {
            p.writer.publish(up_to);
        }
    }

    /// A packet of the current batch. The device's physical state follows
    /// every packet, admitted or not, so it stays true across plays. The
    /// packet is recorded when the gate admits it, recording is open, its
    /// device takes part, and its control is recorded. A press repeated
    /// while the control is held, such as a key's auto-repeat, is recorded
    /// like any packet: recording applies no filter (design.md "Input",
    /// debounce), and input conversion ignores it.
    pub fn packet(&mut self, packet: &Packet) -> Admit {
        let admit = self.gate.admit(packet);
        let max_held = self.config.max_held;
        let Some(device) = self.devices.iter_mut().find(|d| d.id == packet.device) else {
            return admit;
        };
        let source = device.source;
        let axis = device.axes[..device.n_axes]
            .iter()
            .position(|a| a.control == packet.control);
        if let Some(i) = axis {
            // A relative axis has no position to keep.
            if !device.axes[i].relative {
                device.positions[i] = Some(packet.value);
            }
        } else if !track(&mut device.held, max_held, packet.control, packet.value) {
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
                axes: [AxisCap {
                    control: ControlId(0),
                    relative: false,
                    wrap: 0,
                    min: 0,
                    max: 0,
                }; MAX_AXES],
                n_axes: 0,
                positions: [None; MAX_AXES],
                source: None,
                recorded: Vec::new(),
                state_lost: false,
                connection: self.next_connection,
            });
            self.next_connection += 1;
        } else {
            // No room for its state: kept out like an unread device.
            self.connect_unread(device, kind);
            return;
        }
        self.arrived(kind);
    }

    /// A device was found that the adapter has no room to read: like a
    /// device beyond the state table, it never takes part, and a recording
    /// opened while it is connected is marked as missing a device (the
    /// source limit). An arrival during recording is listed as any. Finding
    /// it again changes nothing; its [`disconnect`](Self::disconnect)
    /// clears it. Beyond `max_devices` unread devices, each is counted
    /// without being told apart.
    pub fn connect_unread(&mut self, device: DeviceId, kind: DeviceKind) {
        if self.unread.contains(&device) {
            return;
        }
        if self.unread.len() < self.config.max_devices {
            self.unread.push(device);
        }
        self.untracked += 1;
        self.arrived(kind);
    }

    /// Adds axes to a connected controller's, for the held-input snapshot
    /// and the evaluator's settings: an absolute axis's position is kept
    /// even at 0, and a relative axis's reports are not a state. At most
    /// [`MAX_AXES`] are kept per device, over all its collections; the
    /// adapter does not read an axis that is not kept, so it can be neither
    /// captured nor recorded. Returns whether each of `axes` is kept, in
    /// order; none for a device that is not connected.
    pub fn add_axes(&mut self, device: DeviceId, axes: &[AxisCap]) -> [bool; MAX_AXES] {
        let mut kept = [false; MAX_AXES];
        let Some(d) = self.devices.iter_mut().find(|d| d.id == device) else {
            return kept;
        };
        for (a, k) in axes.iter().zip(&mut kept) {
            if d.axes[..d.n_axes].iter().any(|b| b.control == a.control) {
                *k = true;
            } else if d.n_axes < MAX_AXES {
                d.axes[d.n_axes] = *a;
                d.n_axes += 1;
                *k = true;
            }
        }
        kept
    }

    /// The axes of a connected device ([`add_axes`](Self::add_axes)).
    #[must_use]
    pub fn axes(&self, device: DeviceId) -> &[AxisCap] {
        self.devices
            .iter()
            .find(|d| d.id == device)
            .map_or(&[], |d| &d.axes[..d.n_axes])
    }

    /// Input received in the background whose packets may all be filtered
    /// out as unchanged: trips the focus gate all the same.
    pub fn background(&mut self) {
        self.gate.background();
    }

    /// An absolute axis of `device` reported a value outside its declared
    /// range: its descriptor is wrong, so its conversion may be off. The
    /// value is recorded as reported; a participating source counts it,
    /// and its record says so ([`axis_out_of_range`](Self::axis_out_of_range)).
    pub fn axis_value_out_of_range(&mut self, device: DeviceId) {
        if let Some(rec) = &mut self.recording
            && let Some(p) = rec.participants.iter_mut().find(|p| p.device == device)
        {
            p.out_of_range = p.out_of_range.saturating_add(1);
        }
    }

    /// How many axis values each participating source of the open
    /// recording reported outside their declared range, indexed by
    /// [`SourceId`].
    pub fn axis_out_of_range(&self) -> impl Iterator<Item = u64> + '_ {
        self.recording
            .iter()
            .flat_map(|r| r.participants.iter().map(|p| p.out_of_range))
    }

    /// Input that could not be read (a packet larger than the adapter's
    /// buffers, or a failed read): the open recording is incomplete.
    pub fn input_lost(&mut self) {
        if let Some(rec) = &mut self.recording {
            rec.overflowed = true;
        }
    }

    /// Puts an arrival during the open recording on its list.
    fn arrived(&mut self, kind: DeviceKind) {
        if let Some(rec) = &mut self.recording {
            if rec.arrivals.len() < self.config.max_arrivals {
                rec.arrivals.push(Arrival {
                    at: rec.play_time(self.batch_at),
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
    /// from the next opening: callers must set it before recording opens,
    /// and again after the device reconnects, which starts it empty. The
    /// call moves `controls` in and allocates nothing itself. Returns
    /// `false` for a device that is not connected.
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
        if let Some(i) = self.unread.iter().position(|&d| d == device) {
            self.unread.swap_remove(i);
            self.untracked = self.untracked.saturating_sub(1);
            return;
        }
        let Some(i) = self.devices.iter().position(|d| d.id == device) else {
            // Unknown: one of the devices kept out but not told apart, if
            // there are any; otherwise nothing.
            if self.untracked > self.unread.len() {
                self.untracked -= 1;
            }
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
        if let Some(rec) = &mut self.recording {
            let at = rec.play_time(self.batch_at);
            let p = &mut rec.participants[source.index()];
            p.writer.close(at);
            p.closed = true;
        }
    }

    fn push(&mut self, source: SourceId, kind: InputKind) {
        let Some(rec) = &mut self.recording else {
            return;
        };
        let received = rec.play_time(self.batch_at);
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
    /// [`take_readers`](Self::take_readers). The batch clock is the play's
    /// timeline.
    pub fn open_recording(&mut self, now: TimeUs) -> InputStart {
        self.open_play(now, TimeUs(0))
    }

    /// [`open_recording`](Self::open_recording) for a play whose time 0
    /// lies at `origin` on the batch clock. Until recording closes, every
    /// receive time recorded, every publication, every final watermark and
    /// every arrival is the batch time minus `origin`: the logic thread
    /// sees the play's timeline while the input thread keeps reading one
    /// clock across plays (stage1.md D13: each play's chart time 0 lies
    /// `lead_in` after its countdown starts). The snapshot's `opened_at` is
    /// `now − origin`.
    pub fn open_play(&mut self, now: TimeUs, origin: TimeUs) -> InputStart {
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
            held.extend(
                device.axes[..device.n_axes]
                    .iter()
                    .zip(device.positions)
                    .filter(|(a, _)| recorded.contains(&a.control))
                    .filter_map(|(a, p)| p.map(|v| (source, a.control, v))),
            );
            let (writer, reader) = source_queue(self.config.queue_capacity);
            participants.push(Participant {
                device: device.id,
                kind: device.kind,
                writer,
                closed: false,
                recorded,
                out_of_range: 0,
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
            origin,
        });
        InputStart {
            opened_at: TimeUs(now.0.saturating_sub(origin.0)),
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

    /// Whether recording is open.
    #[must_use]
    pub const fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// The connected devices whose state is kept, in the order they were
    /// found, which is the order of source ids at the next opening.
    pub fn devices(&self) -> impl Iterator<Item = (DeviceId, DeviceKind)> + '_ {
        self.devices.iter().map(|d| (d.id, d.kind))
    }

    /// The participating sources' devices and kinds, indexed by
    /// [`SourceId`].
    pub fn participants(&self) -> impl Iterator<Item = (DeviceId, DeviceKind)> + '_ {
        self.recording
            .iter()
            .flat_map(|r| r.participants.iter().map(|p| (p.device, p.kind)))
    }

    /// Which connection of `device` is current, if it is connected and its
    /// state kept, with or without an open recording. Each connection gets
    /// a new number, so a device removed and connected again between two
    /// looks is seen as removed.
    #[must_use]
    pub fn connection(&self, device: DeviceId) -> Option<u64> {
        self.devices
            .iter()
            .find(|d| d.id == device)
            .map(|d| d.connection)
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
