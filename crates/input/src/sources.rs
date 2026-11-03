//! The input thread's view of devices: their physical state across plays,
//! the frozen set of sources while recording is open, and the arrivals
//! list.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;
use eval::{ControlId, InputKind, InputStart, PhysicalInputEvent, SourceId};

use crate::adjust::{AdjustmentTap, PresentationFocus};
use crate::gate::{Admit, DeviceId, FocusGate, Packet};
use crate::hid::{AxisCap, HidProduct, MAX_AXES, is_axis};
use crate::menu::MenuTap;
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

/// Why [`SourceSet::reopen`] refused to open the next opening.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReopenError {
    /// No attempt is open.
    #[error("no attempt is open")]
    NotRecording,
    /// The origin is not after the open opening's: origins are never
    /// reused.
    #[error("the next opening's origin is not after the last one's")]
    Origin,
    /// The readers were taken and not given back.
    #[error("the readers have not been given back")]
    ReadersOut,
    /// The readers given back are not the attempt's, in its order.
    #[error("the readers given back are not the attempt's")]
    Readers,
    /// A source still has events of the sealed opening.
    #[error("source {} still has events of the sealed opening", .0.0)]
    Undrained(SourceId),
    /// START or SELECT is held, or may be: the next READY candidate waits
    /// for both to be released ([`SourceSet::reopen_candidate`]).
    #[error("START or SELECT is held")]
    Held,
}

/// Why [`SourceSet::new`] refused a configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `max_sources` exceeds [`SOURCE_LIMIT`].
    #[error("{0} sources exceed the limit of {SOURCE_LIMIT}")]
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
    /// Each last position of an axis converted as absolute (`device.toml`'s
    /// mode, else the declared one), 0 included; `None` until seen.
    positions: [Option<i32>; MAX_AXES],
    /// Its source while it takes part in the open recording.
    source: Option<SourceId>,
    /// The controls recorded from it: its lane and system bindings, set by
    /// [`SourceSet::set_recorded`].
    recorded: Vec<ControlId>,
    /// Axes converted with a range other than the declared one
    /// ([`SourceSet::set_axis_ranges`]).
    axis_ranges: Vec<(ControlId, Option<(i32, i32)>)>,
    /// A controller's product ([`SourceSet::set_product`]).
    product: Option<HidProduct>,
    /// Whether it is a source of the next opening
    /// ([`SourceSet::set_takes_part`]).
    takes_part: bool,
    /// Whether its input reaches the adjustment tap: a source of the open
    /// recording, else a device that would be one if recording opened now
    /// ([`SourceSet::set_adjustment`]).
    adjusts: bool,
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
    /// Its axis ranges other than the declared ones, frozen at the opening.
    axis_ranges: Vec<(ControlId, Option<(i32, i32)>)>,
    /// Axis values it reported outside the range their conversion uses.
    out_of_range: u64,
    /// Those of them counted against a range from
    /// [`SourceSet::set_axis_ranges`] rather than the declared one.
    out_of_set_range: u64,
}

/// The open recording.
#[derive(Debug)]
struct Recording {
    participants: Vec<Participant>,
    readers: Vec<SourceReader>,
    arrivals: Vec<Arrival>,
    arrivals_dropped: usize,
    overflowed: bool,
    /// Devices taking part that got no source: beyond `max_sources`, or
    /// not tracked at all.
    left_out: usize,
    /// Where the opening's time 0 lies on the batch clock: every time
    /// recorded or published is the batch time minus this.
    origin: TimeUs,
    /// The opening's held-input snapshot, its storage reserved with the
    /// attempt.
    start: InputStart,
    /// Handed off to a completed play's tail ([`SourceSet::hand_off`]):
    /// what the record keeps is frozen, and the queues take input for the
    /// tail's feedback only.
    handed_off: bool,
    /// Events a full queue refused once handed off.
    feedback_dropped: u64,
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
    /// Between [`begin_batch`](Self::begin_batch) and
    /// [`end_batch`](Self::end_batch).
    in_batch: bool,
    recording: Option<Recording>,
    /// Where the configured controller's menu controls go while recording
    /// is closed ([`set_menu`](Self::set_menu)).
    menu: Option<MenuTap>,
    /// The mode `device.toml` sets for the bound axes of the configured
    /// controller model ([`set_axis_mode`](Self::set_axis_mode)).
    axis_mode: Option<AxisMode>,
    /// Where the adjustment controls go, recording open or not
    /// ([`set_adjustment`](Self::set_adjustment)).
    adjust: Option<AdjustmentTap>,
    /// The window's presentation focus, sampled for the tap at each batch
    /// and revalidated at an opening.
    focus: PresentationFocus,
    /// Added to the batch clock to give the presentation clock the tap
    /// stamps on ([`set_presentation_offset`](Self::set_presentation_offset)).
    presentation_offset: TimeUs,
    /// A wake due besides the tap's ([`set_wake_at`](Self::set_wake_at)).
    wake_at: Option<TimeUs>,
}

/// The mode `device.toml` sets for a controller model's bound axes.
#[derive(Debug)]
struct AxisMode {
    product: HidProduct,
    controls: Vec<ControlId>,
    relative: bool,
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
            in_batch: false,
            recording: None,
            menu: None,
            axis_mode: None,
            adjust: None,
            focus: PresentationFocus::new(),
            presentation_offset: TimeUs(0),
            wake_at: None,
        })
    }

    /// Sets the events each source's queue holds from the next opening on,
    /// between plays: for a session whose input stream changes with its
    /// chart (autoplay of the chart chosen on the select screen). The open
    /// recording's queues keep their size.
    pub const fn set_queue_capacity(&mut self, capacity: usize) {
        self.config.queue_capacity = capacity;
    }

    /// Starts over with `config`, between plays: every device and its
    /// queues go, as in a new set, while the focus gate (and every latch
    /// handle taken from it), the menu, the adjustment tap and the axis
    /// mode stay. For a
    /// session whose input changes with its chart (a development scenario
    /// replaying a log for one chart and autoplay for the next).
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new); the set is unchanged then.
    pub fn reset(&mut self, config: SourceConfig) -> Result<(), ConfigError> {
        let mut fresh = Self::new(config)?;
        std::mem::swap(&mut fresh.gate, &mut self.gate);
        fresh.menu = self.menu.take();
        fresh.axis_mode = self.axis_mode.take();
        fresh.focus = self.focus.clone();
        fresh.presentation_offset = self.presentation_offset;
        if let Some(tap) = self.adjust.take() {
            // An ownership change: the tap sends a new baseline.
            fresh.adjust = Some(tap);
            fresh.recount_adjustment(true);
        }
        *self = fresh;
        Ok(())
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
        self.in_batch = true;
        let open = self.gate.check();
        let (focused, losses) = self.focus.read();
        let at = self.presentation_time(received);
        if let Some(tap) = &mut self.adjust {
            tap.set_focus(focused, losses);
            tap.send_owed(at);
        }
        open
    }

    /// Ends the batch: each open source publishes `fresh − 1 µs` (on the
    /// play's timeline), where `fresh` is a clock read taken after the batch
    /// was pushed. A later
    /// batch reads the clock after this, so its events are stamped at or
    /// above `fresh` and stay above the publication. The latch is checked
    /// here too, so a focus loss is seen even when no batch follows it;
    /// nothing is published once the gate has tripped, so the play ends at
    /// what was already finalized. The adjustment tap sends what it owes,
    /// and a tick at the batch's time while a tapped control is held (as
    /// [`tick_presentation`](Self::tick_presentation)).
    pub fn end_batch(&mut self, fresh: TimeUs) {
        self.in_batch = false;
        let open = self.gate.check();
        let at = self.presentation_time(self.batch_at);
        if let Some(tap) = &mut self.adjust {
            tap.tick(at);
        }
        if !open {
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
    ///
    /// While recording is closed, a packet of the first connected
    /// controller of the menu's model is offered to the menu
    /// ([`set_menu`](Self::set_menu)), which acts on it only while active.
    ///
    /// A tapped control's packet of a device that reaches the adjustment
    /// tap ([`set_adjustment`](Self::set_adjustment)) goes to it, recording
    /// open or not: a button's change of held state, or an axis's signed movement
    /// since its last report, read before the position is replaced and
    /// before any conversion to a lane.
    pub fn packet(&mut self, packet: &Packet) -> Admit {
        let admit = self.gate.admit(packet);
        self.offer_menu(packet);
        let max_held = self.config.max_held;
        let at = self.presentation_time(self.batch_at);
        let Some(device) = self.devices.iter_mut().find(|d| d.id == packet.device) else {
            return admit;
        };
        let source = device.source;
        let reaches = device.adjusts;
        let tapped = self
            .adjust
            .as_ref()
            .filter(|_| reaches)
            .and_then(|t| t.number(device.kind, device.product, packet.control));
        // Background input is a loss for the tap; whether focus is in is
        // the presentation focus sampled for the batch.
        let foreground = !packet.background;
        let axis = device
            .axes
            .iter()
            .take(device.n_axes)
            .position(|a| a.control == packet.control);
        if let Some(i) = axis {
            // An axis converted as relative has no position to keep; one
            // converted as absolute keeps it for the start snapshot,
            // whatever it declares.
            #[expect(
                clippy::indexing_slicing,
                reason = "`i` is a position below `n_axes`, at most MAX_AXES"
            )]
            let (cap, slot) = (&device.axes[i], &mut device.positions[i]);
            let value = i64::from(packet.value);
            let (moved, wrap) = if resolved_relative(self.axis_mode.as_ref(), device.product, cap) {
                (value, None)
            } else {
                let prev = slot.replace(packet.value);
                let declared = if cap.relative { 0 } else { cap.wrap };
                (prev.map_or(0, |p| value - i64::from(p)), Some(declared))
            };
            if let (Some(n), Some(tap)) = (tapped, &mut self.adjust) {
                tap.axis(n, moved, wrap, at, foreground);
            }
        } else {
            if let Some(was) = track(&mut device.held, max_held, packet.control, packet.value) {
                if let (Some(n), Some(tap)) = (tapped, &mut self.adjust) {
                    tap.button(n, was, packet.value != 0, at, foreground);
                }
            } else {
                // The state table is full; snapshots of this device may
                // miss the control from now on, and so may the latch.
                device.state_lost = true;
                if reaches && let Some(tap) = &mut self.adjust {
                    tap.set_unverified(true);
                }
                if source.is_some()
                    && let Some(rec) = self.recording.as_mut().filter(|r| !r.handed_off)
                {
                    rec.overflowed = true;
                }
            }
        }
        #[expect(
            clippy::indexing_slicing,
            reason = "a source id is its device's position in `participants`, set when recording opens"
        )]
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

    /// Sends the configured controller's bound controls to `tap` as menu
    /// events from now on, while it is active and recording is closed.
    /// Replaces an earlier one.
    pub fn set_menu(&mut self, tap: MenuTap) {
        self.menu = Some(tap);
    }

    /// Takes the menu out, for a set that replaces this one.
    pub const fn take_menu(&mut self) -> Option<MenuTap> {
        self.menu.take()
    }

    /// The menu, to set it active at each boundary and see what it sent.
    pub const fn menu_mut(&mut self) -> Option<&mut MenuTap> {
        self.menu.as_mut()
    }

    /// Sends the adjustment controls to `tap` from now on, recording open
    /// or not, while it is active; replaces an earlier one. Only the
    /// devices whose input the play may record reach it: the sources of
    /// the open recording, else the devices that would be its sources if
    /// it opened now (those that take part,
    /// [`set_takes_part`](Self::set_takes_part), controllers first, up to
    /// `max_sources`). The controls they hold now are counted as its
    /// holders.
    pub fn set_adjustment(&mut self, tap: AdjustmentTap) {
        self.adjust = Some(tap);
        self.recount_adjustment(false);
    }

    /// Marks again the devices that reach the adjustment tap and counts
    /// its holders again; `owners_changed` when one that reached it left,
    /// or its controls now map differently, which the marking cannot see.
    /// A new baseline this makes within a batch is sent at once, before
    /// the batch's later packets.
    fn recount_adjustment(&mut self, owners_changed: bool) {
        let reach = if self.recording.is_some() {
            Reach::Sources
        } else {
            Reach::Taking(self.config.max_sources)
        };
        recount(
            self.adjust.as_mut(),
            &mut self.devices,
            reach,
            owners_changed,
        );
        let at = self.presentation_time(self.batch_at);
        if self.in_batch
            && let Some(tap) = &mut self.adjust
        {
            tap.send_owed(at);
        }
    }

    /// The adjustment tap, to set it active at each boundary.
    pub const fn adjustment_mut(&mut self) -> Option<&mut AdjustmentTap> {
        self.adjust.as_mut()
    }

    /// Reads the window's presentation focus from `focus` from now on
    /// (focused until set).
    pub fn set_presentation_focus(&mut self, focus: PresentationFocus) {
        self.focus = focus;
    }

    /// Stamps the tap's observations and markers on the presentation clock
    /// from now on: the batch clock plus `offset` (0 when the batch clock
    /// is the process clock; a script's origin on it for a scripted one).
    pub const fn set_presentation_offset(&mut self, offset: TimeUs) {
        self.presentation_offset = offset;
    }

    /// The batch clock's time `at` on the presentation clock.
    #[must_use]
    pub const fn presentation_time(&self, at: TimeUs) -> TimeUs {
        TimeUs(at.0.saturating_add(self.presentation_offset.0))
    }

    /// The input thread woke at `now` on the presentation clock, whether
    /// or not a batch ran: the tap samples the focus, sends what it owes
    /// and its due ticks ([`AdjustmentTap::tick`]).
    pub fn tick_presentation(&mut self, now: TimeUs) {
        let (focused, losses) = self.focus.read();
        if let Some(tap) = &mut self.adjust {
            tap.set_focus(focused, losses);
            tap.tick(now);
        }
    }

    /// When the input thread is next due a wake, on the presentation clock:
    /// the tap's next tick ([`AdjustmentTap::next_due`]), or the deadline
    /// set with [`set_wake_at`](Self::set_wake_at), whichever is sooner.
    #[must_use]
    pub fn next_due(&self) -> Option<TimeUs> {
        let tap = self.adjust.as_ref().and_then(AdjustmentTap::next_due);
        match (tap, self.wake_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// A wake the input thread is due at `at` on the presentation clock
    /// besides the tap's, such as a timed script's next item; `None` for
    /// none.
    pub const fn set_wake_at(&mut self, at: Option<TimeUs>) {
        self.wake_at = at;
    }

    /// Whether the presentation focus is in now and no loss was counted
    /// since the batch sampled it: what an opening revalidates.
    fn focus_held(&self) -> bool {
        let Some(tap) = &self.adjust else {
            return true;
        };
        let (focused, losses) = self.focus.read();
        focused && tap.focused() && losses == tap.focus_losses()
    }

    /// Offers `packet` to the menu: only while recording is closed, and
    /// only from the first connected controller of the menu's model, with
    /// an axis's capability.
    fn offer_menu(&mut self, packet: &Packet) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        if self.recording.is_some() || !menu.is_active() {
            return;
        }
        let product = menu.product();
        let Some(device) = self
            .devices
            .iter()
            .find(|d| d.kind == DeviceKind::Controller && d.product == Some(product))
            .filter(|d| d.id == packet.device)
        else {
            return;
        };
        let axis = device
            .axes
            .iter()
            .take(device.n_axes)
            .find(|a| a.control == packet.control);
        menu.offer(packet, axis);
    }

    /// A device was found. While recording is open it does not take part;
    /// it goes to the arrivals list, which drops what exceeds its bound.
    pub fn connect(&mut self, device: DeviceId, kind: DeviceKind) {
        if self.devices.iter().any(|d| d.id == device) {
            return;
        }
        // An unread device is not connected: read now, it leaves the
        // exclusion table.
        self.forget_unread(device);
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
                    bits: 0,
                    signed: false,
                }; MAX_AXES],
                n_axes: 0,
                positions: [None; MAX_AXES],
                source: None,
                recorded: Vec::new(),
                axis_ranges: Vec::new(),
                product: None,
                takes_part: true,
                adjusts: false,
                state_lost: false,
                connection: self.next_connection,
            });
            self.next_connection += 1;
            self.recount_adjustment(false);
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
        if self.unread.contains(&device) || self.devices.iter().any(|d| d.id == device) {
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
            if d.axes.iter().take(d.n_axes).any(|b| b.control == a.control) {
                *k = true;
            } else if d.n_axes < MAX_AXES {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "the branch requires `n_axes` below MAX_AXES"
                )]
                {
                    d.axes[d.n_axes] = *a;
                }
                d.n_axes += 1;
                *k = true;
            }
        }
        kept
    }

    /// The axes of a connected device ([`add_axes`](Self::add_axes)).
    #[must_use]
    #[expect(
        clippy::indexing_slicing,
        reason = "`add_axes` keeps `n_axes` at most MAX_AXES"
    )]
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

    /// Axis `cap` of `device` reported `value`. A value outside the range
    /// its conversion uses (from [`set_axis_ranges`](Self::set_axis_ranges),
    /// else the declared one; none for a relative axis) means that range is
    /// wrong, so the conversion may be off. The value is recorded as
    /// reported; a participating source counts it, and its record says so
    /// ([`axis_out_of_range`](Self::axis_out_of_range)).
    pub fn axis_value(&mut self, device: DeviceId, cap: &AxisCap, value: i32) {
        if let Some(rec) = self.recording.as_mut().filter(|r| !r.handed_off)
            && let Some(p) = rec.participants.iter_mut().find(|p| p.device == device)
        {
            let set = p.axis_ranges.iter().find(|(c, _)| *c == cap.control);
            let range = match set {
                Some(&(_, range)) => range,
                None => (!cap.relative).then_some((cap.min, cap.max)),
            };
            if range.is_some_and(|(min, max)| !(min..=max).contains(&value)) {
                p.out_of_range = p.out_of_range.saturating_add(1);
                if set.is_some() {
                    p.out_of_set_range = p.out_of_set_range.saturating_add(1);
                }
            }
        }
    }

    /// Sets whether the axes `controls` of every controller of `product`
    /// report movement (`relative`) or position, whatever they declare: the
    /// mode `device.toml` gives the axes bound to lanes, which their
    /// conversion uses. Their reports are then filtered in that mode
    /// ([`axis_relative`](Self::axis_relative)). Set it before the input
    /// thread starts; it allocates.
    pub fn set_axis_mode(&mut self, product: HidProduct, controls: Vec<ControlId>, relative: bool) {
        self.axis_mode = Some(AxisMode {
            product,
            controls,
            relative,
        });
        let remapped = self
            .devices
            .iter()
            .any(|d| d.adjusts && d.product == Some(product));
        self.recount_adjustment(remapped);
    }

    /// Whether the axis `cap` of `device` is taken as relative: the mode
    /// [`set_axis_mode`](Self::set_axis_mode) sets for it, else the
    /// declared one.
    #[must_use]
    pub fn axis_relative(&self, device: DeviceId, cap: &AxisCap) -> bool {
        let product = self
            .devices
            .iter()
            .find(|d| d.id == device)
            .and_then(|d| d.product);
        resolved_relative(self.axis_mode.as_ref(), product, cap)
    }

    /// Sets the axes of the connected `device` that the conversion takes
    /// with a range other than the declared one: each control with its
    /// range, `None` for one with no range (relative, or not wrapping).
    /// The next opening freezes them; [`axis_value`](Self::axis_value)
    /// counts against them. Returns `false` for a device that is not
    /// connected.
    pub fn set_axis_ranges(
        &mut self,
        device: DeviceId,
        ranges: Vec<(ControlId, Option<(i32, i32)>)>,
    ) -> bool {
        match self.devices.iter_mut().find(|d| d.id == device) {
            Some(d) => {
                d.axis_ranges = ranges;
                true
            }
            None => false,
        }
    }

    /// How many axis values each participating source of the open
    /// recording reported outside the range their conversion uses,
    /// indexed by [`SourceId`].
    pub fn axis_out_of_range(&self) -> impl Iterator<Item = u64> + '_ {
        self.recording
            .iter()
            .flat_map(|r| r.participants.iter().map(|p| p.out_of_range))
    }

    /// Of [`axis_out_of_range`](Self::axis_out_of_range), how many each
    /// source's axes counted against a range from
    /// [`set_axis_ranges`](Self::set_axis_ranges); the rest were outside
    /// a declared range.
    pub fn axis_out_of_set_range(&self) -> impl Iterator<Item = u64> + '_ {
        self.recording
            .iter()
            .flat_map(|r| r.participants.iter().map(|p| p.out_of_set_range))
    }

    /// Input that could not be read (a packet larger than the adapter's
    /// buffers, or a failed read): the open recording is incomplete.
    pub fn input_lost(&mut self) {
        if let Some(rec) = self.recording.as_mut().filter(|r| !r.handed_off) {
            rec.overflowed = true;
        }
    }

    /// Takes `device` out of the unread devices, if it is there: the
    /// adapter reads it now, possibly as part of a source with another id
    /// (a controller's collections grouped into one physical device).
    pub fn forget_unread(&mut self, device: DeviceId) {
        if let Some(i) = self.unread.iter().position(|&d| d == device) {
            self.unread.swap_remove(i);
            self.untracked = self.untracked.saturating_sub(1);
        }
    }

    /// Puts an arrival during the open recording on its list.
    fn arrived(&mut self, kind: DeviceKind) {
        if let Some(rec) = self.recording.as_mut().filter(|r| !r.handed_off) {
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

    /// Sets the product of the connected controller `device`, as the
    /// adapter reads it: a source holds the collections of one function,
    /// so it has one. Returns `false` for a device that is not connected.
    pub fn set_product(&mut self, device: DeviceId, product: HidProduct) -> bool {
        match self.devices.iter_mut().find(|d| d.id == device) {
            Some(d) => {
                let remapped = d.adjusts && d.product != Some(product);
                d.product = Some(product);
                self.recount_adjustment(remapped);
                true
            }
            None => false,
        }
    }

    /// Whether the connected controller `device` is of `product`
    /// ([`set_product`](Self::set_product)).
    #[must_use]
    pub fn has_product(&self, device: DeviceId, product: HidProduct) -> bool {
        self.devices
            .iter()
            .find(|d| d.id == device)
            .is_some_and(|d| d.product == Some(product))
    }

    /// Sets whether `device` is a source of the next opening. A device
    /// that takes no part is left out of the play as if it were not
    /// connected: nothing of it is recorded, it is not counted against
    /// `max_sources`, and it does not make the play source limited. Every
    /// device takes part until this is set; like
    /// [`set_recorded`](Self::set_recorded) it is set before recording
    /// opens, and a reconnected device starts taking part again. Returns
    /// `false` for a device that is not connected.
    pub fn set_takes_part(&mut self, device: DeviceId, takes_part: bool) -> bool {
        match self.devices.iter_mut().find(|d| d.id == device) {
            Some(d) => {
                d.takes_part = takes_part;
                self.recount_adjustment(false);
                true
            }
            None => false,
        }
    }

    /// A device was removed. A participating source records `Disconnected`
    /// at the batch time, publishes that time as its final watermark, and
    /// closes; once the gate has tripped, nothing is recorded or published.
    pub fn disconnect(&mut self, device: DeviceId) {
        if let Some(menu) = &mut self.menu {
            menu.removed(device);
        }
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
        let at = self.presentation_time(self.batch_at);
        if let Some(tap) = &mut self.adjust
            && removed.adjusts
        {
            // What it held is released.
            for &(c, _) in &removed.held {
                if let Some(n) = tap.number(removed.kind, removed.product, c) {
                    tap.button(n, true, false, at, true);
                }
            }
        }
        // The latch is verified again once no device that lost its state
        // reaches the tap.
        self.recount_adjustment(removed.adjusts);
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
            #[expect(
                clippy::indexing_slicing,
                reason = "a source id is its device's position in `participants`, set when recording opens"
            )]
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
        #[expect(
            clippy::indexing_slicing,
            reason = "a source id is its device's position in `participants`, set when recording opens"
        )]
        if rec.participants[source.index()].writer.push(ev).is_err() {
            if rec.handed_off {
                rec.feedback_dropped = rec.feedback_dropped.saturating_add(1);
            } else {
                rec.overflowed = true;
            }
        }
    }

    /// Opens recording at a boundary between two batches: the connected
    /// devices that take part ([`set_takes_part`](Self::set_takes_part))
    /// become the sources, controllers first and each kind in the order
    /// they were found, up to `max_sources`, and the recorded controls
    /// they hold are copied into the returned snapshot. Everything after this call, in stream order,
    /// belongs to the play. `now` is the clock read at the boundary.
    /// Readers for the logic thread are then taken with
    /// [`take_readers`](Self::take_readers). The batch clock is the play's
    /// timeline.
    pub fn open_recording(&mut self, now: TimeUs) -> InputStart {
        self.open_play(now, TimeUs(0))
    }

    /// [`open_recording`](Self::open_recording) for a play whose time 0 lies at
    /// `origin` on the batch clock: [`open_attempt`](Self::open_attempt)
    /// with the snapshot copied out, which allocates. Until recording
    /// closes, every receive time recorded, every publication, every final
    /// watermark and every arrival is the batch time minus `origin`: the
    /// logic thread sees the play's timeline while the input thread keeps
    /// reading one clock across plays (design.md "Play Lifecycle": each
    /// play's chart time 0 lies `lead_in` after its READY state starts).
    /// The snapshot's `opened_at` is `now − origin`.
    pub fn open_play(&mut self, now: TimeUs, origin: TimeUs) -> InputStart {
        self.open_attempt(now, origin).clone()
    }

    /// Opens the attempt's one source table and its first opening, whose
    /// time 0 lies at `origin` on the batch clock, as
    /// [`open_play`](Self::open_play) does: the queues, the readers, the
    /// arrivals list and the held-input snapshot's storage are allocated
    /// here, once per attempt, and every later opening of the attempt
    /// ([`reopen`](Self::reopen)) reuses them. Returns the opening's
    /// snapshot.
    pub fn open_attempt(&mut self, now: TimeUs, origin: TimeUs) -> &InputStart {
        // Closed, the devices that would be sources are marked, as the
        // adjustment tap takes them (`recount`'s `Reach::Taking`).
        self.close_recording();
        let taking = self.devices.iter().filter(|d| d.takes_part).count();
        let n = self.devices.iter().filter(|d| d.adjusts).count();
        let left_out = taking - n + self.untracked;
        let capacity = self.config.queue_capacity;
        let mut overflowed = false;
        let mut participants = Vec::with_capacity(n);
        let mut readers = Vec::with_capacity(n);
        for kind in [DeviceKind::Controller, DeviceKind::Keyboard] {
            let devices = self.devices.iter_mut();
            for device in devices.filter(|d| d.adjusts && d.kind == kind) {
                let i = participants.len();
                #[expect(
                    clippy::expect_used,
                    reason = "`SourceSet::new` bounds `max_sources` by SOURCE_LIMIT, which fits in u16"
                )]
                let source = SourceId(u16::try_from(i).expect("source ids fit in u16"));
                device.source = Some(source);
                overflowed |= device.state_lost;
                // An axis is recorded only if the device declares it now: the
                // recording window was sized for these, and an axis a late
                // collection adds is not recorded in this attempt.
                let declared = device.axes.iter().take(device.n_axes);
                let recorded: Vec<ControlId> = device
                    .recorded
                    .iter()
                    .copied()
                    .filter(|&c| {
                        device.kind != DeviceKind::Controller
                            || !is_axis(c)
                            || declared.clone().any(|a| a.control == c)
                    })
                    .collect();
                let (writer, reader) = source_queue(capacity);
                participants.push(Participant {
                    device: device.id,
                    kind: device.kind,
                    writer,
                    closed: false,
                    recorded,
                    axis_ranges: device.axis_ranges.clone(),
                    out_of_range: 0,
                    out_of_set_range: 0,
                });
                readers.push(reader);
            }
        }
        // Each source holds at most `max_held` controls and MAX_AXES axis
        // positions, so no opening's snapshot grows past this.
        let held = n.saturating_mul(self.config.max_held.saturating_add(MAX_AXES));
        let rec = self.recording.insert(Recording {
            participants,
            readers,
            arrivals: Vec::with_capacity(self.config.max_arrivals),
            arrivals_dropped: 0,
            overflowed,
            left_out,
            origin,
            start: InputStart {
                opened_at: TimeUs(0),
                held: Vec::with_capacity(held),
            },
            handed_off: false,
            feedback_dropped: 0,
        });
        snapshot(rec, &self.devices, now);
        recount(
            self.adjust.as_mut(),
            &mut self.devices,
            Reach::Sources,
            false,
        );
        &rec.start
    }

    /// Seals the attempt's open opening and opens the next on the same
    /// source table, its time 0 at `origin` on the batch clock, without
    /// allocating: the readers must be back ([`return_readers`]) and
    /// drained, so every event of the sealed opening was taken. Each
    /// queue starts over on the new timeline; arrivals and the
    /// out-of-range counts start empty, while an overflow and the sources
    /// left out stay for the attempt. A source whose device was removed
    /// keeps its id and reports absent: `Disconnected` at the opening
    /// time, then closed. A device connected since stays an arrival until
    /// the next attempt. Returns the opening's snapshot.
    ///
    /// # Errors
    ///
    /// [`ReopenError`], changing nothing.
    ///
    /// [`return_readers`]: Self::return_readers
    pub fn reopen(&mut self, now: TimeUs, origin: TimeUs) -> Result<&InputStart, ReopenError> {
        let tripped = self.gate.is_tripped();
        let Some(rec) = self.recording.as_mut().filter(|r| !r.handed_off) else {
            return Err(ReopenError::NotRecording);
        };
        if origin <= rec.origin {
            return Err(ReopenError::Origin);
        }
        if rec.readers.len() != rec.participants.len() {
            return Err(ReopenError::ReadersOut);
        }
        for (p, r) in rec.participants.iter().zip(&rec.readers) {
            // Checked for every source before any is changed.
            if !p.writer.pairs(r) {
                return Err(ReopenError::Readers);
            }
        }
        if let Some(i) = rec.readers.iter().position(|r| !r.is_empty()) {
            return Err(ReopenError::Undrained(SourceId(
                u16::try_from(i).unwrap_or(u16::MAX),
            )));
        }
        rec.origin = origin;
        rec.arrivals.clear();
        rec.arrivals_dropped = 0;
        let opened_at = rec.play_time(now);
        for ((p, r), id) in rec.participants.iter_mut().zip(&rec.readers).zip(0_u16..) {
            let rearmed = p.writer.rearm(r);
            debug_assert!(rearmed, "each reader was checked above");
            p.out_of_range = 0;
            p.out_of_set_range = 0;
            let source = SourceId(id);
            p.closed |= !self.devices.iter().any(|d| d.source == Some(source));
            if p.closed {
                let gone = PhysicalInputEvent {
                    received: opened_at,
                    source,
                    kind: InputKind::Disconnected,
                };
                if !tripped && p.writer.push(gone).is_err() {
                    rec.overflowed = true;
                }
                p.writer.close(opened_at);
            }
        }
        snapshot(rec, &self.devices, now);
        Ok(&rec.start)
    }

    /// The open attempt's held-input snapshot storage: the most entries
    /// any of its openings' snapshots holds. 0 without one.
    #[must_use]
    pub fn snapshot_capacity(&self) -> usize {
        self.recording
            .as_ref()
            .map_or(0, |r| r.start.held.capacity())
    }

    /// Whether START or SELECT is held, or may be, as the adjustment tap
    /// counts the devices that reach it now: what defers a READY
    /// candidate's opening. `false` without a tap.
    #[must_use]
    pub fn holding(&self) -> bool {
        self.adjust.as_ref().is_some_and(AdjustmentTap::holding)
    }

    /// Opens the attempt ([`open_attempt`](Self::open_attempt)) with its
    /// first READY candidate `generation` at this boundary, only if START
    /// and SELECT are verifiably released here (design.md "Play
    /// Lifecycle", the start gate), and arms the tap's start gate for it.
    /// `None` when one is held or may be, or the presentation focus is away
    /// or was lost since the batch sampled it: the opening is deferred and
    /// nothing opens.
    pub fn open_candidate(
        &mut self,
        now: TimeUs,
        origin: TimeUs,
        generation: u32,
    ) -> Option<&InputStart> {
        if self.holding() || !self.focus_held() {
            return None;
        }
        let _ = self.open_attempt(now, origin);
        let (at, origin) = (self.presentation_time(now), self.presentation_time(origin));
        if let Some(tap) = &mut self.adjust {
            // The opening's marker is sent here, at the same boundary as
            // the input-side hook would, its origin mapped through the
            // presentation offset.
            tap.gate().arm(generation);
            tap.opened(generation, origin, at);
        }
        self.recording.as_ref().map(|r| &r.start)
    }

    /// Seals the open candidate and opens candidate `generation` on the
    /// attempt's source table ([`reopen`](Self::reopen)), only if START
    /// and SELECT are verifiably released at this boundary, and arms the
    /// tap's start gate for it. Without allocating. Either opening sends
    /// the tap's [`TapEvent::Opened`](crate::TapEvent::Opened) with its
    /// chart time 0, `origin`, on the presentation clock.
    ///
    /// # Errors
    ///
    /// [`ReopenError::Held`] when one is held or may be, or the focus is
    /// away or was lost (as for [`open_candidate`](Self::open_candidate)),
    /// and what [`reopen`](Self::reopen) refuses; nothing changes.
    pub fn reopen_candidate(
        &mut self,
        now: TimeUs,
        origin: TimeUs,
        generation: u32,
    ) -> Result<&InputStart, ReopenError> {
        if self.holding() || !self.focus_held() {
            return Err(ReopenError::Held);
        }
        let _ = self.reopen(now, origin)?;
        let (at, origin) = (self.presentation_time(now), self.presentation_time(origin));
        if let Some(tap) = &mut self.adjust {
            // The opening's marker is sent here, at the same boundary as
            // the input-side hook would, its origin mapped through the
            // presentation offset.
            tap.gate().arm(generation);
            tap.opened(generation, origin, at);
        }
        self.recording
            .as_ref()
            .map(|r| &r.start)
            .ok_or(ReopenError::NotRecording)
    }

    /// The readers of the open recording, indexed by [`SourceId`]; empty
    /// after the first call until they are returned.
    pub fn take_readers(&mut self) -> Vec<SourceReader> {
        self.recording
            .as_mut()
            .map(|r| std::mem::take(&mut r.readers))
            .unwrap_or_default()
    }

    /// Gives back the readers [`take_readers`](Self::take_readers) took,
    /// in the same order, for the next opening of the attempt
    /// ([`reopen`](Self::reopen) checks them). Dropped when recording is
    /// closed or the readers are already back.
    pub fn return_readers(&mut self, readers: Vec<SourceReader>) {
        if let Some(rec) = &mut self.recording
            && rec.readers.is_empty()
        {
            rec.readers = readers;
        }
    }

    /// Hands the open recording off to a completed play's tail, at a
    /// boundary between two batches (design.md "Play Lifecycle", the
    /// audio tail): what the record keeps is frozen here (the arrivals,
    /// the overflow, the sources left out and the out-of-range counts, as
    /// they read now), and the start gate is disarmed, but every source
    /// keeps its device, its queue, its timeline and its publications, so
    /// the controls held, the axes' positions and the input after this
    /// boundary reach the same queues with no gap and no press repeated.
    /// From here a full queue drops the event and counts it
    /// ([`feedback_dropped`](Self::feedback_dropped)), which the record
    /// never sees. [`close_recording`](Self::close_recording) ends it.
    /// Allocates nothing; does nothing while recording is closed.
    pub fn hand_off(&mut self) {
        if let Some(rec) = &mut self.recording {
            rec.handed_off = true;
            if let Some(tap) = &self.adjust {
                tap.gate().disarm();
            }
        }
    }

    /// Whether the open recording was handed off to a completed play's
    /// tail ([`hand_off`](Self::hand_off)).
    #[must_use]
    pub fn is_handed_off(&self) -> bool {
        self.recording.as_ref().is_some_and(|r| r.handed_off)
    }

    /// Events a full queue dropped since the recording was handed off
    /// ([`hand_off`](Self::hand_off)).
    #[must_use]
    pub fn feedback_dropped(&self) -> u64 {
        self.recording.as_ref().map_or(0, |r| r.feedback_dropped)
    }

    /// Closes the recording. Devices keep their physical state.
    pub fn close_recording(&mut self) {
        self.recording = None;
        if let Some(tap) = &self.adjust {
            tap.gate().disarm();
        }
        for d in &mut self.devices {
            d.source = None;
        }
        self.recount_adjustment(false);
    }

    /// Whether recording is open.
    #[must_use]
    pub const fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// The connected devices whose state is kept, in the order they were
    /// found ([`open_play`](Self::open_play) numbers sources from it).
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

    /// The open opening's held-input snapshot, as
    /// [`open_attempt`](Self::open_attempt) or [`reopen`](Self::reopen)
    /// returned it.
    #[must_use]
    pub fn opening(&self) -> Option<&InputStart> {
        self.recording.as_ref().map(|r| &r.start)
    }

    /// Devices that arrived during the open recording.
    #[must_use]
    pub fn arrivals(&self) -> &[Arrival] {
        self.recording.as_ref().map_or(&[], |r| &r.arrivals)
    }

    /// The most arrivals an open recording keeps: the bound of
    /// [`arrivals`](Self::arrivals).
    #[must_use]
    pub const fn max_arrivals(&self) -> usize {
        self.config.max_arrivals
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
        self.left_out() > 0
    }

    /// How many devices that take part got no source in the open
    /// recording: beyond `max_sources` (controllers come first, so these
    /// are keyboards when any is), or beyond what the adapter or the
    /// state table can track.
    #[must_use]
    pub fn left_out(&self) -> usize {
        self.recording.as_ref().map_or(0, |r| r.left_out)
    }
}

/// Fills the opening's snapshot: `opened_at` is `now` on its timeline, and
/// each source whose device is still connected gives the recorded
/// controls it holds and its axes' positions, in source order. The
/// attempt reserved room for all of them, so nothing is allocated.
fn snapshot(rec: &mut Recording, devices: &[Device], now: TimeUs) {
    rec.start.opened_at = rec.play_time(now);
    let held = &mut rec.start.held;
    held.clear();
    for (p, id) in rec.participants.iter().zip(0_u16..) {
        let source = SourceId(id);
        let Some(device) = devices.iter().find(|d| d.source == Some(source)) else {
            continue;
        };
        let recorded = &p.recorded;
        held.extend(
            device
                .held
                .iter()
                .filter(|(c, _)| recorded.contains(c))
                .map(|&(c, v)| (source, c, v)),
        );
        held.extend(
            device
                .axes
                .iter()
                .take(device.n_axes)
                .zip(device.positions)
                .filter(|(a, _)| recorded.contains(&a.control))
                .filter_map(|(a, p)| p.map(|v| (source, a.control, v))),
        );
    }
}

/// Which devices reach the adjustment tap.
#[derive(Clone, Copy)]
enum Reach {
    /// The sources of the open recording.
    Sources,
    /// Those that would be sources if recording opened now: the devices
    /// that take part, controllers first so keyboards' many handles never
    /// push a controller out at the limit, each kind in the order found,
    /// up to this many. [`SourceSet::open_attempt`] makes the marked ones
    /// its sources.
    Taking(usize),
}

/// Marks the `devices` that reach `tap` by `reach` and counts its holders
/// again over them. A change in which devices reach it, or
/// `owners_changed`, ends the tap's ownership: it sends a new baseline.
fn recount(
    tap: Option<&mut AdjustmentTap>,
    devices: &mut [Device],
    reach: Reach,
    owners_changed: bool,
) {
    let mut changed = owners_changed;
    let mut taken = 0;
    for kind in [DeviceKind::Controller, DeviceKind::Keyboard] {
        for d in devices.iter_mut().filter(|d| d.kind == kind) {
            let adjusts = match reach {
                Reach::Sources => d.source.is_some(),
                Reach::Taking(max) => {
                    let within = d.takes_part && taken < max;
                    taken += usize::from(d.takes_part);
                    within
                }
            };
            changed |= adjusts != d.adjusts;
            d.adjusts = adjusts;
        }
    }
    let Some(tap) = tap else {
        return;
    };
    let reaching = || devices.iter().filter(|d| d.adjusts);
    tap.recount(
        reaching().flat_map(|d| d.held.iter().map(|&(c, _)| (d.kind, d.product, c))),
        reaching().any(|d| d.state_lost),
        changed,
    );
}

/// Whether the axis `cap` of a device of `product` is converted as
/// relative: the mode `set` gives it, else the declared one.
fn resolved_relative(set: Option<&AxisMode>, product: Option<HidProduct>, cap: &AxisCap) -> bool {
    set.filter(|m| product == Some(m.product) && m.controls.contains(&cap.control))
        .map_or(cap.relative, |m| m.relative)
}

/// Updates one control in a device's held list. Returns whether it was
/// held before, or `None` when a newly held control does not fit.
fn track(
    held: &mut Vec<(ControlId, i32)>,
    max: usize,
    control: ControlId,
    value: i32,
) -> Option<bool> {
    let found = held.iter().position(|&(c, _)| c == control);
    match found {
        Some(i) if value == 0 => {
            held.swap_remove(i);
        }
        #[expect(clippy::indexing_slicing, reason = "`i` was just found by `position`")]
        Some(i) => held[i].1 = value,
        None if value == 0 => {}
        None if held.len() < max => held.push((control, value)),
        None => return None,
    }
    Some(found.is_some())
}
