//! Windows: the QPC clock, Raw Input keyboards and HID game controllers,
//! and the input thread.
//!
//! Decoding the packets is portable ([`crate::raw`]); this module only
//! makes the OS calls that fill the buffers. The layout the decoder assumes
//! is checked against the `windows` crate's definitions at compile time.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::JoinHandle;

use bms_model::{TimeSource, TimeUs};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_Interface_PropertyW, CM_LOCATE_DEVNODE_NORMAL,
    CM_Locate_DevNodeW, CR_SUCCESS,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HIDP_BUTTON_CAPS, HIDP_CAPS, HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS, HidP_GetButtonCaps,
    HidP_GetCaps, HidP_GetUsageValue, HidP_GetUsages, HidP_GetValueCaps, HidP_Input,
    PHIDP_PREPARSED_DATA,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_ContainerId, DEVPKEY_Device_InstanceId, DEVPROPTYPE,
};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE,
    SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS,
};
use windows::Win32::UI::Input::{
    GetRawInputBuffer, GetRawInputData, GetRawInputDeviceInfoW, GetRawInputDeviceList,
    GetRegisteredRawInputDevices, HRAWINPUT, RAWHID, RAWINPUT, RAWINPUTDEVICE, RAWINPUTDEVICELIST,
    RAWINPUTHEADER, RAWKEYBOARD, RID_DEVICE_INFO, RID_DEVICE_INFO_HID, RID_INPUT, RIDEV_DEVNOTIFY,
    RIDEV_INPUTSINK, RIDEV_REMOVE, RIDI_DEVICEINFO, RIDI_DEVICENAME, RIDI_PREPARSEDDATA,
    RIM_TYPEHID, RIM_TYPEKEYBOARD, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GIDC_ARRIVAL, GIDC_REMOVAL,
    HWND_MESSAGE, MSG, MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_NOREMOVE, PM_QS_SENDMESSAGE, PM_REMOVE, PeekMessageW,
    QS_RAWINPUT, QUEUE_STATUS_FLAGS, RI_KEY_BREAK, RI_KEY_E0, RI_KEY_E1, RIM_INPUTSINK,
    RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_INPUT_DEVICE_CHANGE, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use crate::WakeReport;
use crate::cadence::{self, Boundary, Cadence, Driven, Phase, Wait, WaitFailed, Waiter, Woken};
use crate::capture::Capture;
use crate::gate::DeviceId;
use crate::hid::{
    AXIS_USAGES, AxisCap, ButtonGroup, CollectionSlot, DESKTOP_PAGE, Group, HidDevice, HidLayout,
    HidReport, MAX_AXES, MAX_REPORT_BYTES, MAX_REPORT_IDS, Placement, axis_cap, axis_control,
    axis_value, group_device, in_collection, place_collection, read_report, remove_collection,
    until_nul,
};
use crate::raw;
use crate::sources::{DeviceKind, SourceSet};
use crate::time::{QpcFrequency, qpc_to_us};

// The layout `raw` decodes, checked against the OS definitions.
const _: () = {
    assert!(size_of::<RAWINPUTHEADER>() == raw::HEADER_SIZE);
    assert!(offset_of!(RAWINPUTHEADER, dwType) == 0);
    assert!(offset_of!(RAWINPUTHEADER, dwSize) == 4);
    assert!(offset_of!(RAWINPUTHEADER, hDevice) == 8);
    assert!(offset_of!(RAWINPUTHEADER, wParam) == 8 + size_of::<usize>());
    assert!(offset_of!(RAWINPUT, data) == raw::HEADER_SIZE);
    assert!(size_of::<RAWKEYBOARD>() == raw::KEYBOARD_SIZE);
    assert!(offset_of!(RAWKEYBOARD, MakeCode) == 0);
    assert!(offset_of!(RAWKEYBOARD, Flags) == 2);
    assert!(offset_of!(RAWKEYBOARD, VKey) == 6);
    assert!(RIM_TYPEKEYBOARD.0 == raw::RIM_TYPEKEYBOARD);
    assert!(RIM_TYPEHID.0 == raw::RIM_TYPEHID);
    assert!(offset_of!(RAWHID, dwSizeHid) == 0);
    assert!(offset_of!(RAWHID, dwCount) == 4);
    assert!(offset_of!(RAWHID, bRawData) == raw::HID_HEADER_SIZE);
    assert!(RIM_INPUTSINK as usize == raw::RIM_INPUTSINK);
    assert!(RI_KEY_BREAK as u16 == raw::RI_KEY_BREAK);
    assert!(RI_KEY_E0 as u16 == raw::RI_KEY_E0);
    assert!(RI_KEY_E1 as u16 == raw::RI_KEY_E1);
};

/// Why the input thread could not start or stopped abnormally.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// An OS call failed.
    #[error("{call} failed")]
    Os {
        /// The call.
        call: &'static str,
        /// Its error.
        #[source]
        error: windows::core::Error,
    },
    /// The thread could not be spawned.
    #[error("spawning the input thread failed")]
    Spawn(#[source] std::io::Error),
    /// The thread panicked.
    #[error("the input thread panicked")]
    Panicked,
    /// [`InputThread::stop`] was called from the input thread itself; the
    /// thread ends after its current wake without being joined.
    #[error("the input thread cannot join itself")]
    StopFromWorker,
}

impl InputError {
    const fn os(call: &'static str, error: windows::core::Error) -> Self {
        Self::Os { call, error }
    }
}

fn read_qpc() -> i64 {
    let mut ticks = 0;
    // SAFETY: writes one i64.
    let read = unsafe { QueryPerformanceCounter(&mut ticks) };
    // It cannot fail on Windows XP and later.
    #[expect(
        clippy::expect_used,
        reason = "QueryPerformanceCounter cannot fail on Windows XP and later"
    )]
    read.expect("QueryPerformanceCounter");
    ticks
}

/// The QPC frequency, read once per process.
#[expect(
    clippy::expect_used,
    reason = "QueryPerformanceFrequency cannot fail and gives a positive frequency on Windows XP and later"
)]
fn qpc_frequency() -> QpcFrequency {
    static FREQUENCY: OnceLock<QpcFrequency> = OnceLock::new();
    *FREQUENCY.get_or_init(|| {
        let mut hz = 0;
        // SAFETY: writes one i64.
        let read = unsafe { QueryPerformanceFrequency(&mut hz) };
        read.expect("QueryPerformanceFrequency");
        QpcFrequency::new(hz).expect("the QPC frequency is positive")
    })
}

/// The real clock: QPC in µs from the clock's origin, floored. Raw QPC
/// values never leave it, because they reveal the time since boot.
#[derive(Clone, Copy, Debug)]
pub struct QpcClock {
    origin: i64,
    frequency: QpcFrequency,
}

impl QpcClock {
    /// A clock that reads 0 now. Copies share the origin.
    #[must_use]
    pub fn start() -> Self {
        Self {
            origin: read_qpc(),
            frequency: qpc_frequency(),
        }
    }

    /// The QPC frequency the clock converts with.
    #[must_use]
    pub const fn frequency(&self) -> QpcFrequency {
        self.frequency
    }

    /// A raw QPC value another API reported (DXGI frame statistics, a
    /// display capture) as this clock's time.
    #[must_use]
    pub fn from_qpc(&self, ticks: i64) -> TimeUs {
        qpc_to_us(ticks, self.origin, self.frequency)
    }
}

impl TimeSource for QpcClock {
    fn now(&self) -> TimeUs {
        qpc_to_us(read_qpc(), self.origin, self.frequency)
    }
}

/// A high-resolution waitable timer; the system timer resolution is left
/// alone (no `timeBeginPeriod`).
struct Timer(HANDLE);

impl Timer {
    fn new() -> Result<Self, InputError> {
        // SAFETY: plain timer creation; the handle is owned by the value.
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .map_err(|e| InputError::os("CreateWaitableTimerExW", e))?;
        Ok(Self(handle))
    }

    /// Sets the timer to fire once, `left_us` from now; setting it clears
    /// a signal left from before.
    fn set(&self, left_us: i64) -> windows::core::Result<()> {
        // Relative due time in 100 ns units.
        let due = -left_us.saturating_mul(10);
        // SAFETY: a valid timer handle and no completion routine.
        unsafe { SetWaitableTimer(self.0, &due, 0, None, None, false) }
    }
}

/// An auto-reset event, closed on drop.
#[derive(Debug)]
struct Event(HANDLE);

// SAFETY: an event handle may be signaled, waited on and closed from any
// thread; it is closed once, when the last owner drops it.
unsafe impl Send for Event {}
// SAFETY: as above; `SetEvent` and waits need no exclusive access.
unsafe impl Sync for Event {}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The input thread's wake event (design.md "Input"): signaling it wakes
/// the thread from any wait, so a request, a focus change or stop is seen
/// without a timer. A signal before the wait is kept until it.
/// Clones share the event.
#[derive(Clone, Debug)]
pub struct Waker(Arc<Event>);

impl Waker {
    /// A new event, not signaled.
    ///
    /// # Errors
    ///
    /// [`InputError::Os`] if the event cannot be created.
    pub fn new() -> Result<Self, InputError> {
        // SAFETY: an unnamed auto-reset event, owned by the value.
        let handle = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|e| InputError::os("CreateEventW", e))?;
        Ok(Self(Arc::new(Event(handle))))
    }

    /// Wakes the thread. It never blocks and allocates nothing.
    pub fn wake(&self) {
        // SAFETY: a valid event handle. It fails only for an invalid one.
        let _ = unsafe { SetEvent(self.0.0) };
    }

    fn handle(&self) -> HANDLE {
        self.0.0
    }
}

/// The OS side of [`cadence::run`]: `MsgWaitForMultipleObjectsEx` on the
/// wake event and the timer, for queued Raw Input with
/// `QS_RAWINPUT | MWMO_INPUTAVAILABLE` (device changes come through the
/// Raw Input queue too). The call that failed is kept for the error.
struct OsWaiter<'a> {
    clock: QpcClock,
    timer: &'a Timer,
    event: HANDLE,
    failed: Option<(&'static str, windows::core::Error)>,
}

impl Waiter for OsWaiter<'_> {
    fn wait(&mut self, wait: Wait) -> Result<Woken, WaitFailed> {
        let handles = [self.event, self.timer.0];
        let mut count = 1;
        if let Some(deadline) = wait.deadline {
            let left = deadline.0.saturating_sub(self.clock.now().0);
            if left <= 0 {
                return Ok(Woken::Timer);
            }
            if let Err(e) = self.timer.set(left) {
                self.failed = Some(("SetWaitableTimer", e));
                return Err(WaitFailed);
            }
            count = 2;
        }
        let (mask, flags) = if wait.input {
            (QS_RAWINPUT, MWMO_INPUTAVAILABLE)
        } else {
            (
                QUEUE_STATUS_FLAGS(0),
                MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS(0),
            )
        };
        // No timeout: the event wakes every wait that has no deadline,
        // and stop signals it after setting its flag.
        // SAFETY: valid handles that outlive the call.
        let woke =
            unsafe { MsgWaitForMultipleObjectsEx(handles.get(..count), INFINITE, mask, flags) };
        let index = woke.0.wrapping_sub(WAIT_OBJECT_0.0);
        match cadence::classify(index, count as u32, wait.input) {
            Ok(Woken::Queue) => {
                // Messages sent to this thread are handled by peeking;
                // nothing is removed, so held Raw Input stays queued.
                let mut msg = MSG::default();
                // SAFETY: peeking this thread's own messages.
                let _ =
                    unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE | PM_QS_SENDMESSAGE) };
                Ok(Woken::Queue)
            }
            Ok(woken) => Ok(woken),
            // WAIT_FAILED, or a result no wait above can give.
            Err(WaitFailed) => {
                self.failed = Some((
                    "MsgWaitForMultipleObjectsEx",
                    windows::core::Error::from_thread(),
                ));
                Err(WaitFailed)
            }
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The thread's MMCSS "Games" registration, reverted on drop.
struct Mmcss(HANDLE);

impl Mmcss {
    fn games() -> Option<Self> {
        let mut task_index = 0;
        // SAFETY: a static task name and one u32 written.
        unsafe { AvSetMmThreadCharacteristicsW(w!("Games"), &mut task_index) }
            .ok()
            .map(Self)
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        // SAFETY: the handle came from AvSetMmThreadCharacteristicsW on this
        // thread.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

unsafe extern "system" fn receiver_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: forwarding the arguments the system passed.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

fn device_id(handle: HANDLE) -> DeviceId {
    DeviceId(handle.0 as usize as u64)
}

/// Bytes the buffer for one packet read with `GetRawInputData` holds, in
/// words: a header, `RAWHID`'s two fields and the longest report read
/// ([`MAX_REPORT_BYTES`]), rounded up. A larger packet is lost input.
const ONE_WORDS: usize = (raw::HEADER_SIZE + raw::HID_HEADER_SIZE + MAX_REPORT_BYTES).div_ceil(8);
/// Bytes the buffer `GetRawInputBuffer` drains into holds, in words. A
/// packet larger than what is left stays queued and is read on its own
/// `WM_INPUT` with the buffer above.
const MANY_WORDS: usize = 8192;
/// `size_of::<RAWINPUTHEADER>()` as the calls take it.
const HEADER_BYTES: u32 = raw::HEADER_SIZE as u32;
/// The fewest bytes a packet takes in `GetRawInputBuffer`'s buffer: a
/// header and a keyboard, or a header, `RAWHID`'s fields and one report
/// byte, aligned to 8.
const MIN_PACKET_BYTES: usize = {
    let hid = raw::HID_HEADER_SIZE + 1;
    let body = if raw::KEYBOARD_SIZE < hid {
        raw::KEYBOARD_SIZE
    } else {
        hid
    };
    (raw::HEADER_SIZE + body).next_multiple_of(8)
};

fn as_bytes(words: &[u64]) -> &[u8] {
    // SAFETY: any u64 is valid as 8 bytes, and the length is in bytes.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast(), words.len() * 8) }
}

/// The Raw Input registrations: keyboards, joysticks and gamepads (usage
/// page 1, usages 6, 4 and 5).
const USAGES: [u16; 3] = [0x06, 0x04, 0x05];

/// Whether a HID top-level collection is a game controller: a joystick or
/// a gamepad.
const fn is_controller(page: u16, usage: u16) -> bool {
    page == DESKTOP_PAGE && (usage == 0x04 || usage == 0x05)
}

/// Controller collections read at once; more are not read, and a
/// recording opened while one is connected is marked as missing a device.
const MAX_CONTROLLERS: usize = 8;
/// Bytes of a collection's preparsed HID data kept, in words; a
/// collection whose data is larger is not read.
const PREPARSED_WORDS: usize = 4096;
/// Value and button capabilities read per collection.
const MAX_CAPS: usize = 64;
/// Buttons read per report.
const USAGE_LIST: usize = 256;
/// Characters of a device interface path or instance id read.
const PATH_CHARS: usize = 1024;
/// The container of devices that belong to no physical device (devices
/// the system enumerates itself): never a reason to group.
const NO_CONTAINER: u128 = 0x0000_0000_0000_0000_ffff_ffff_ffff_ffff;

/// One axis as the HID parser reads it.
#[derive(Clone, Copy, Debug, Default)]
struct AxisRead {
    page: u16,
    usage: u16,
    bits: u16,
    signed: bool,
    /// The declared logical range.
    min: i32,
    max: i32,
}

/// A connected controller collection: its handle, its source, its
/// preparsed data and its last report.
#[derive(Debug)]
struct Controller {
    handle: Option<isize>,
    /// The physical device's container and the function's interface,
    /// when the collection is grouped by them.
    group: Option<Group>,
    collection: u8,
    /// Its top-level collection's usage page and usage.
    usage: (u16, u16),
    /// Whether its reports start with a report id.
    report_ids: bool,
    /// 8-byte aligned preparsed data.
    preparsed: Box<[u64]>,
    reads: [AxisRead; MAX_AXES],
    state: HidDevice,
}

impl CollectionSlot for Controller {
    fn key(&self) -> Option<u64> {
        self.handle.map(|h| h as u64)
    }

    fn state(&self) -> &HidDevice {
        &self.state
    }

    fn free(&mut self) {
        self.handle = None;
        self.group = None;
        self.state = HidDevice::new(DeviceId(0), &EMPTY_LAYOUT);
    }
}

/// The layout of a free slot.
const EMPTY_LAYOUT: HidLayout<'static> = HidLayout {
    collection: 0,
    axes: &[],
    buttons: &[],
};

impl Controller {
    fn data(&self) -> PHIDP_PREPARSED_DATA {
        PHIDP_PREPARSED_DATA(self.preparsed.as_ptr() as isize)
    }
}

/// The controller collections, with every buffer allocated up front so a
/// controller that arrives during play allocates nothing.
#[derive(Debug)]
struct Controllers {
    slots: Vec<Controller>,
    usages: Box<[u16]>,
    report: Box<[u8]>,
    path: Box<[u16]>,
    instance: Box<[u16]>,
}

impl Controllers {
    fn new() -> Self {
        let empty = HidLayout {
            collection: 0,
            axes: &[],
            buttons: &[],
        };
        Self {
            slots: (0..MAX_CONTROLLERS)
                .map(|_| Controller {
                    handle: None,
                    group: None,
                    collection: 0,
                    usage: (0, 0),
                    report_ids: false,
                    preparsed: vec![0; PREPARSED_WORDS].into_boxed_slice(),
                    reads: [AxisRead::default(); MAX_AXES],
                    state: HidDevice::new(DeviceId(0), &empty),
                })
                .collect(),
            usages: vec![0; USAGE_LIST].into_boxed_slice(),
            report: vec![0; MAX_REPORT_BYTES].into_boxed_slice(),
            path: vec![0; PATH_CHARS].into_boxed_slice(),
            instance: vec![0; PATH_CHARS].into_boxed_slice(),
        }
    }

    /// Connects the controller collection `handle`, reading its layout
    /// from its preparsed data, as part of its physical device's source.
    /// Nothing happens for a collection connected already; one that does
    /// not fit is connected as unread.
    fn connect(&mut self, handle: HANDLE, sources: &mut SourceSet) {
        let key = handle.0 as isize;
        if self.slots.iter().any(|s| s.handle == Some(key)) {
            return;
        }
        // The collection number, the interface and the container all come
        // from this read's path, never from what an earlier read left in
        // the buffer.
        let len = read_path(handle, &mut self.path).map(<[u16]>::len);
        let container = len
            .and_then(|_| container_of(&self.path, &mut self.instance))
            .filter(|&c| c != NO_CONTAINER);
        let ids = hid_info(handle).and_then(|hid| {
            Some((
                u16::try_from(hid.dwVendorId).ok()?,
                u16::try_from(hid.dwProductId).ok()?,
            ))
        });
        let Placement {
            group,
            collection,
            product,
        } = place_collection(
            len.and_then(|n| self.path.get(..n)),
            container,
            ids,
            self.slots
                .iter()
                .filter(|s| s.handle.is_some())
                .map(|s| (s.group, s.collection)),
        );
        let Some(slot) = self.slots.iter_mut().find(|s| s.handle.is_none()) else {
            sources.connect_unread(device_id(handle), DeviceKind::Controller);
            return;
        };
        let mut size = (slot.preparsed.len() * 8) as u32;
        // SAFETY: `preparsed` holds `size` bytes.
        let got = unsafe {
            GetRawInputDeviceInfoW(
                Some(handle),
                RIDI_PREPARSEDDATA,
                Some(slot.preparsed.as_mut_ptr().cast::<c_void>()),
                &mut size,
            )
        };
        if got == 0 || got == u32::MAX {
            sources.connect_unread(device_id(handle), DeviceKind::Controller);
            return;
        }
        let mut caps = [AxisCap {
            control: eval::ControlId(0),
            relative: false,
            wrap: 0,
            min: 0,
            max: 0,
            bits: 0,
            signed: false,
        }; MAX_AXES];
        let mut groups = [ButtonGroup {
            report_id: 0,
            buttons: 0,
        }; MAX_REPORT_IDS];
        let data = slot.data();
        let (n_axes, axis_ids) = read_axes(data, collection, &mut slot.reads, &mut caps);
        let (n_groups, button_ids) = read_buttons(data, &mut groups);
        let device = group.map_or(device_id(handle), group_device);
        // A collection unread before is read now, under its source's id.
        sources.forget_unread(device_id(handle));
        sources.connect(device, DeviceKind::Controller);
        if let Some(product) = product {
            sources.set_product(device, product);
        }
        // Only axes the source keeps are read: one beyond the device's
        // table is never captured or recorded.
        #[expect(
            clippy::indexing_slicing,
            reason = "`read_axes` returns at most MAX_AXES, the table's length"
        )]
        let kept = sources.add_axes(device, &caps[..n_axes]);
        let mut n = 0;
        #[expect(
            clippy::indexing_slicing,
            reason = "`read_axes` returns at most MAX_AXES, the length of these tables, and `n` stays at most `i`"
        )]
        for i in 0..n_axes {
            if kept[i] {
                caps[n] = caps[i];
                slot.reads[n] = slot.reads[i];
                n += 1;
            }
        }
        slot.handle = Some(key);
        slot.group = group;
        slot.collection = collection;
        slot.usage = hid_usage(handle).unwrap_or((0, 0));
        slot.report_ids = axis_ids || button_ids;
        #[expect(
            clippy::indexing_slicing,
            reason = "`n` is at most `n_axes`, and `read_buttons` returns at most MAX_REPORT_IDS, the tables' lengths"
        )]
        {
            slot.state = HidDevice::new(
                device,
                &HidLayout {
                    collection,
                    axes: &caps[..n],
                    buttons: &groups[..n_groups],
                },
            )
            .with_product(product);
        }
    }

    /// Forgets the collection `handle` and disconnects its source
    /// ([`remove_collection`]): the controller's other collections, if
    /// any, connect again and take part from the next play. Returns
    /// `false` when the handle is not a collection read here.
    fn disconnect(&mut self, handle: HANDLE, sources: &mut SourceSet) -> bool {
        remove_collection(&mut self.slots, handle.0 as isize as u64, sources)
    }

    /// Turns a HID block's reports into packets.
    fn deliver(&mut self, block: &raw::HidBlock<'_>, capture: &Capture, sources: &mut SourceSet) {
        let key = block.device.0 as isize;
        let Some(slot) = self.slots.iter_mut().find(|s| s.handle == Some(key)) else {
            return;
        };
        let data = slot.data();
        for bytes in block.reports() {
            let Some(report) = read_report(bytes, &mut self.report, sources) else {
                continue;
            };
            let id = if slot.report_ids {
                report.first().copied().unwrap_or(0)
            } else {
                0
            };
            let mut len = self.usages.len() as u32;
            // SAFETY: `usages` holds `len` entries; the report and the
            // preparsed data are the device's.
            let status = unsafe {
                HidP_GetUsages(
                    HidP_Input,
                    crate::hid::BUTTON_PAGE,
                    None,
                    self.usages.as_mut_ptr(),
                    &mut len,
                    data,
                    report,
                )
            };
            let buttons = (status == HIDP_STATUS_SUCCESS)
                .then(|| self.usages.get(..len as usize))
                .flatten();
            let mut values = [None; MAX_AXES];
            let axes = slot.state.axes().len();
            for (value, read) in values.iter_mut().zip(&slot.reads).take(axes) {
                let mut raw_value = 0;
                // SAFETY: one u32 written; the report and the preparsed
                // data are the device's.
                let status = unsafe {
                    HidP_GetUsageValue(
                        HidP_Input,
                        read.page,
                        None,
                        read.usage,
                        &mut raw_value,
                        data,
                        report,
                    )
                };
                if status == HIDP_STATUS_SUCCESS {
                    *value = Some(axis_value(raw_value, read.bits, read.signed));
                }
            }
            #[expect(
                clippy::indexing_slicing,
                reason = "the state's axes are at most MAX_AXES, the length of `values`"
            )]
            let report = HidReport {
                id,
                buttons,
                axes: &values[..axes],
            };
            slot.state
                .report(&report, block.background, capture, sources);
        }
    }
}

/// Reads the interface path of `handle` into `buf`, nul-terminated, and
/// returns it without the nul.
fn read_path(handle: HANDLE, buf: &mut [u16]) -> Option<&[u16]> {
    let mut chars = buf.len() as u32;
    // SAFETY: `buf` holds `chars` characters.
    let got = unsafe {
        GetRawInputDeviceInfoW(
            Some(handle),
            RIDI_DEVICENAME,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            &mut chars,
        )
    };
    if got == 0 || got == u32::MAX {
        return None;
    }
    until_nul(buf)
}

/// The container id of the physical device the interface `path`
/// (nul-terminated) belongs to, through its device instance.
fn container_of(path: &[u16], instance: &mut [u16]) -> Option<u128> {
    let mut kind = DEVPROPTYPE(0);
    let mut bytes = (instance.len() * 2) as u32;
    // SAFETY: `path` is nul-terminated; `instance` holds `bytes` bytes.
    let found = unsafe {
        CM_Get_Device_Interface_PropertyW(
            PCWSTR(path.as_ptr()),
            &DEVPKEY_Device_InstanceId,
            &mut kind,
            Some(instance.as_mut_ptr().cast::<u8>()),
            &mut bytes,
            0,
        )
    };
    if found != CR_SUCCESS {
        return None;
    }
    let mut node = 0;
    // SAFETY: `instance` now holds a nul-terminated instance id.
    let located = unsafe {
        CM_Locate_DevNodeW(
            &mut node,
            PCWSTR(instance.as_ptr()),
            CM_LOCATE_DEVNODE_NORMAL,
        )
    };
    if located != CR_SUCCESS {
        return None;
    }
    let mut guid = windows::core::GUID::zeroed();
    let mut bytes = size_of::<windows::core::GUID>() as u32;
    // SAFETY: `guid` holds `bytes` bytes.
    let read = unsafe {
        CM_Get_DevNode_PropertyW(
            node,
            &DEVPKEY_Device_ContainerId,
            &mut kind,
            Some((&raw mut guid).cast::<u8>()),
            &mut bytes,
            0,
        )
    };
    (read == CR_SUCCESS).then(|| guid.to_u128())
}

/// Reads the generic desktop axes ([`AXIS_USAGES`]) of a collection's
/// input reports into `reads` and `caps`, with controls in `collection`,
/// without duplicates and without axes whose range cannot be represented
/// ([`axis_cap`]). Returns how many there are, and whether the value
/// capabilities name report ids.
fn read_axes(
    data: PHIDP_PREPARSED_DATA,
    collection: u8,
    reads: &mut [AxisRead; MAX_AXES],
    caps: &mut [AxisCap; MAX_AXES],
) -> (usize, bool) {
    let mut info = HIDP_CAPS::default();
    // SAFETY: the preparsed data was filled by the system; one struct
    // written.
    if unsafe { HidP_GetCaps(data, &mut info) } != HIDP_STATUS_SUCCESS {
        return (0, false);
    }
    let mut values = [HIDP_VALUE_CAPS::default(); MAX_CAPS];
    let mut len = info.NumberInputValueCaps.min(MAX_CAPS as u16);
    // SAFETY: `values` holds `len` entries.
    let status = unsafe { HidP_GetValueCaps(HidP_Input, values.as_mut_ptr(), &mut len, data) };
    if status != HIDP_STATUS_SUCCESS {
        return (0, false);
    }
    #[expect(
        clippy::indexing_slicing,
        reason = "`len` starts at most MAX_CAPS, the table's length, and the call only lowers it"
    )]
    let values = &values[..usize::from(len)];
    let ids = values.iter().any(|v| v.ReportID != 0);
    let mut n = 0;
    for v in values {
        if v.UsagePage != DESKTOP_PAGE {
            continue;
        }
        // SAFETY: the union holds the variant `IsRange` names.
        let (min, max) = unsafe {
            if v.IsRange {
                (v.Anonymous.Range.UsageMin, v.Anonymous.Range.UsageMax)
            } else {
                (v.Anonymous.NotRange.Usage, v.Anonymous.NotRange.Usage)
            }
        };
        let axes = min.max(*AXIS_USAGES.start())..=max.min(*AXIS_USAGES.end());
        for usage in axes {
            let control = in_collection(axis_control(v.UsagePage, usage), collection);
            let Some(cap) = axis_cap(control, v.IsAbsolute, v.LogicalMin, v.LogicalMax, v.BitSize)
            else {
                continue;
            };
            if n == MAX_AXES || caps.iter().take(n).any(|c| c.control == control) {
                continue;
            }
            #[expect(
                clippy::indexing_slicing,
                reason = "the check above skips the axis once `n` reaches MAX_AXES, the length of both tables"
            )]
            {
                reads[n] = AxisRead {
                    page: v.UsagePage,
                    usage,
                    bits: v.BitSize,
                    signed: v.LogicalMin < 0,
                    min: v.LogicalMin,
                    max: v.LogicalMax,
                };
                caps[n] = cap;
            }
            n += 1;
        }
    }
    (n, ids)
}

/// Reads which buttons (page 9, up to [`crate::hid::MAX_BUTTON`]) each
/// report id of a collection carries into `groups`. Returns how many
/// groups there are, and whether the button capabilities name report ids.
fn read_buttons(
    data: PHIDP_PREPARSED_DATA,
    groups: &mut [ButtonGroup; MAX_REPORT_IDS],
) -> (usize, bool) {
    let mut info = HIDP_CAPS::default();
    // SAFETY: the preparsed data was filled by the system; one struct
    // written.
    if unsafe { HidP_GetCaps(data, &mut info) } != HIDP_STATUS_SUCCESS {
        return (0, false);
    }
    let mut buttons = [HIDP_BUTTON_CAPS::default(); MAX_CAPS];
    let mut len = info.NumberInputButtonCaps.min(MAX_CAPS as u16);
    // SAFETY: `buttons` holds `len` entries.
    let status = unsafe { HidP_GetButtonCaps(HidP_Input, buttons.as_mut_ptr(), &mut len, data) };
    if status != HIDP_STATUS_SUCCESS {
        return (0, false);
    }
    #[expect(
        clippy::indexing_slicing,
        reason = "`len` starts at most MAX_CAPS, the table's length, and the call only lowers it"
    )]
    let buttons = &buttons[..usize::from(len)];
    let ids = buttons.iter().any(|b| b.ReportID != 0);
    let mut n = 0;
    for b in buttons
        .iter()
        .filter(|b| b.UsagePage == crate::hid::BUTTON_PAGE)
    {
        // SAFETY: the union holds the variant `IsRange` names.
        let (min, max) = unsafe {
            if b.IsRange {
                (b.Anonymous.Range.UsageMin, b.Anonymous.Range.UsageMax)
            } else {
                (b.Anonymous.NotRange.Usage, b.Anonymous.NotRange.Usage)
            }
        };
        let bits = (min.max(1)..=max.min(crate::hid::MAX_BUTTON))
            .fold(0_u128, |bits, u| bits | 1 << (u - 1));
        if let Some(g) = groups
            .iter_mut()
            .take(n)
            .find(|g| g.report_id == b.ReportID)
        {
            g.buttons |= bits;
        } else if n < MAX_REPORT_IDS {
            #[expect(
                clippy::indexing_slicing,
                reason = "the branch requires `n` below MAX_REPORT_IDS"
            )]
            {
                groups[n] = ButtonGroup {
                    report_id: b.ReportID,
                    buttons: bits,
                };
            }
            n += 1;
        }
    }
    (n, ids)
}

/// Keyboards and HID game controllers through Raw Input: a message-only
/// window registered for keyboards, joysticks and gamepads with
/// `RIDEV_INPUTSINK | RIDEV_DEVNOTIFY`, so background input arrives marked
/// `RIM_INPUTSINK` (and is rejected by the focus gate) and device arrivals
/// and removals arrive as messages. Each keyboard and each controller is a
/// device of its own, by handle.
///
/// The window belongs to the thread that creates the receiver, which
/// polls and destroys it, so the receiver is not `Send` and stays on the
/// input thread; [`InputThread`] is the handle other threads hold.
#[derive(Debug)]
struct RawInput {
    /// The receiver window; `HWND` keeps the receiver on its thread.
    hwnd: HWND,
    clock: QpcClock,
    capture: Capture,
    /// 8-byte aligned, as `GetRawInputBuffer` requires.
    one: Box<[u64]>,
    many: Box<[u64]>,
    controllers: Controllers,
}

impl RawInput {
    /// Creates the receiver window on this thread and registers keyboards
    /// and game controllers.
    /// Batches end with a publication from `clock`; presses go to
    /// `capture` while it is armed and recording is closed.
    ///
    /// # Errors
    ///
    /// [`InputError::Os`] if the window or the registration fails.
    fn new(clock: QpcClock, capture: Capture) -> Result<Self, InputError> {
        let class = w!("tapestry-raw-input");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(receiver_proc),
            lpszClassName: class,
            ..Default::default()
        };
        // SAFETY: the class name and window procedure are static. It
        // returns 0 when an earlier adapter registered the class already.
        unsafe { RegisterClassW(&wc) };
        // SAFETY: a message-only window of the class registered above.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .map_err(|e| InputError::os("CreateWindowExW", e))?;
        let devices = USAGES.map(|usage| RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: usage,
            dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
            hwndTarget: hwnd,
        });
        // SAFETY: valid device descriptions.
        let registered =
            unsafe { RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32) };
        if let Err(e) = registered {
            // SAFETY: the window was created on this thread above.
            let _ = unsafe { DestroyWindow(hwnd) };
            return Err(InputError::os("RegisterRawInputDevices", e));
        }
        Ok(Self {
            hwnd,
            clock,
            capture,
            one: vec![0; ONE_WORDS].into_boxed_slice(),
            many: vec![0; MANY_WORDS].into_boxed_slice(),
            controllers: Controllers::new(),
        })
    }

    /// Connects the keyboards and controllers present now, in the order the
    /// OS lists them.
    /// The registration also announces them as arrivals, which then change
    /// nothing. Call it before recording opens: it allocates.
    ///
    /// # Errors
    ///
    /// [`InputError::Os`] if the device list cannot be read.
    fn connect_present(&mut self, sources: &mut SourceSet) -> Result<(), InputError> {
        for d in device_list()? {
            self.connect(d.hDevice, sources);
        }
        Ok(())
    }

    /// Connects a keyboard or a game controller; other devices are left
    /// out.
    fn connect(&mut self, handle: HANDLE, sources: &mut SourceSet) {
        match device_kind(handle) {
            Some(DeviceKind::Keyboard) => sources.connect(device_id(handle), DeviceKind::Keyboard),
            Some(DeviceKind::Controller) => self.controllers.connect(handle, sources),
            None => {}
        }
    }

    /// Reads the packet of one `WM_INPUT` taken off the queue.
    fn read_one(&mut self, handle: HRAWINPUT, sources: &mut SourceSet) {
        let mut size = (self.one.len() * 8) as u32;
        // SAFETY: `one` holds `size` bytes.
        let got = unsafe {
            GetRawInputData(
                handle,
                RID_INPUT,
                Some(self.one.as_mut_ptr().cast::<c_void>()),
                &mut size,
                HEADER_BYTES,
            )
        };
        if got == u32::MAX {
            // Larger than the supported packet, or unreadable: lost.
            sources.input_lost();
            return;
        }
        if got == 0 {
            return;
        }
        if let Some(block) = as_bytes(&self.one).get(..got as usize) {
            deliver(block, &self.capture, &mut self.controllers, sources);
        }
    }

    /// Reads the packets still queued, in blocks, at most `budget` of
    /// them, and returns how many it read. Each read offers only as many
    /// bytes as `budget` smallest packets take, so it cannot read more.
    fn drain(&mut self, sources: &mut SourceSet, budget: u32) -> u32 {
        let mut read = 0;
        while read < budget {
            let room = ((budget - read) as usize)
                .saturating_mul(MIN_PACKET_BYTES)
                .min(self.many.len() * 8);
            let mut size = room as u32;
            // SAFETY: `many` holds at least `size` bytes and is 8-byte
            // aligned.
            let count = unsafe {
                GetRawInputBuffer(
                    Some(self.many.as_mut_ptr().cast::<RAWINPUT>()),
                    &mut size,
                    HEADER_BYTES,
                )
            };
            // On failure, such as a next packet larger than the room
            // offered, the packets stay queued and arrive as their own
            // `WM_INPUT` or at the next drain.
            if count == 0 || count == u32::MAX {
                break;
            }
            for block in raw::blocks(as_bytes(&self.many), count as usize) {
                deliver(block, &self.capture, &mut self.controllers, sources);
            }
            read += count;
        }
        read
    }

    fn device_change(&mut self, change: u32, handle: HANDLE, sources: &mut SourceSet) {
        match change {
            GIDC_ARRIVAL => self.connect(handle, sources),
            GIDC_REMOVAL if !self.controllers.disconnect(handle, sources) => {
                sources.disconnect(device_id(handle));
            }
            _ => {}
        }
    }
}

/// The usage page and usage of a HID device's top-level collection.
fn hid_usage(handle: HANDLE) -> Option<(u16, u16)> {
    hid_info(handle).map(|hid| (hid.usUsagePage, hid.usUsage))
}

/// What Raw Input says of a HID device; `None` for another device.
fn hid_info(handle: HANDLE) -> Option<RID_DEVICE_INFO_HID> {
    let mut info = RID_DEVICE_INFO {
        cbSize: size_of::<RID_DEVICE_INFO>() as u32,
        ..Default::default()
    };
    let mut size = info.cbSize;
    // SAFETY: `info` holds `size` bytes.
    let got = unsafe {
        GetRawInputDeviceInfoW(
            Some(handle),
            RIDI_DEVICEINFO,
            Some((&raw mut info).cast::<c_void>()),
            &mut size,
        )
    };
    if got == u32::MAX || info.dwType != RIM_TYPEHID {
        return None;
    }
    // SAFETY: the system filled the HID variant.
    Some(unsafe { info.Anonymous.hid })
}

/// What a device is: a keyboard, a game controller (a HID joystick or
/// gamepad), or neither.
fn device_kind(handle: HANDLE) -> Option<DeviceKind> {
    let mut info = RID_DEVICE_INFO {
        cbSize: size_of::<RID_DEVICE_INFO>() as u32,
        ..Default::default()
    };
    let mut size = info.cbSize;
    // SAFETY: `info` holds `size` bytes.
    let got = unsafe {
        GetRawInputDeviceInfoW(
            Some(handle),
            RIDI_DEVICEINFO,
            Some((&raw mut info).cast::<c_void>()),
            &mut size,
        )
    };
    if got == u32::MAX {
        return None;
    }
    if info.dwType == RIM_TYPEKEYBOARD {
        return Some(DeviceKind::Keyboard);
    }
    // SAFETY: the system filled the variant `dwType` names.
    let hid = unsafe { info.Anonymous.hid };
    (info.dwType == RIM_TYPEHID && is_controller(hid.usUsagePage, hid.usUsage))
        .then_some(DeviceKind::Controller)
}

/// One decoded block to the capture slot and the sources.
fn deliver(
    block: &[u8],
    capture: &Capture,
    controllers: &mut Controllers,
    sources: &mut SourceSet,
) {
    if let Some(packet) = raw::decode_keyboard(block) {
        capture.offer(&packet, DeviceKind::Keyboard, sources.is_recording());
        sources.packet(&packet);
    } else if let Some(hid) = raw::decode_hid(block) {
        controllers.deliver(&hid, capture, sources);
    }
}

/// A controller collection as the input thread reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionInfo {
    /// Whether it is grouped with its device's other collections by the
    /// device's container; `false` when it is a source of its own.
    pub grouped: bool,
    /// Its collection number in its controls.
    pub collection: u8,
    /// Its top-level collection's usage page and usage.
    pub usage: (u16, u16),
    /// Its axes: usages in its collection, whether relative, and the
    /// values in a revolution of an absolute one.
    pub axes: Vec<AxisCap>,
    /// Each axis's declared logical minimum and maximum and its field's
    /// bits, as the descriptor states them.
    pub declared: Vec<(i32, i32, u16)>,
    /// Its source's device: the collections of one physical controller
    /// share it, and the input thread's [`SourceSet`] knows the source by
    /// it.
    pub device: DeviceId,
}

/// Every game controller collection present, in the order the OS lists
/// them, read and grouped as the input thread does. Nothing that names a
/// device is returned. It allocates; call it outside play.
///
/// # Errors
///
/// [`InputError::Os`] if the device list cannot be read.
pub fn controller_layouts() -> Result<Vec<CollectionInfo>, InputError> {
    let list = device_list()?;
    let mut controllers = Controllers::new();
    #[expect(
        clippy::expect_used,
        reason = "listing outside play: MAX_CONTROLLERS is within the source limit"
    )]
    let mut sources = SourceSet::new(crate::SourceConfig {
        max_devices: MAX_CONTROLLERS,
        max_sources: MAX_CONTROLLERS,
        max_held: 1,
        queue_capacity: 1,
        max_arrivals: 1,
    })
    .expect("the controllers are within the limit");
    for d in &list {
        if device_kind(d.hDevice) == Some(DeviceKind::Controller) {
            controllers.connect(d.hDevice, &mut sources);
        }
    }
    Ok(controllers
        .slots
        .iter()
        .filter(|s| s.handle.is_some())
        .map(|s| CollectionInfo {
            grouped: s.group.is_some(),
            collection: s.collection,
            usage: s.usage,
            axes: s.state.axes().to_vec(),
            declared: s
                .reads
                .iter()
                .take(s.state.axes().len())
                .map(|r| (r.min, r.max, r.bits))
                .collect(),
            device: s.state.device(),
        })
        .collect())
}

/// The usage page and usage of every HID device present that is not a
/// keyboard or a mouse, and whether each is read as a game controller.
/// Nothing that names a device is returned.
///
/// # Errors
///
/// [`InputError::Os`] if the device list cannot be read.
pub fn hid_usages() -> Result<Vec<(u16, u16, bool)>, InputError> {
    Ok(device_list()?
        .iter()
        .filter(|d| d.dwType == RIM_TYPEHID)
        .filter_map(|d| hid_usage(d.hDevice))
        .map(|(page, usage)| (page, usage, is_controller(page, usage)))
        .collect())
}

/// The devices present, in the order the OS lists them. It allocates.
fn device_list() -> Result<Vec<RAWINPUTDEVICELIST>, InputError> {
    let entry = size_of::<RAWINPUTDEVICELIST>() as u32;
    let failed = || InputError::os("GetRawInputDeviceList", windows::core::Error::from_thread());
    let mut count = 0;
    // SAFETY: a null list asks for the count only.
    if unsafe { GetRawInputDeviceList(None, &mut count, entry) } == u32::MAX {
        return Err(failed());
    }
    // Room for devices that arrive between the two calls.
    let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize + 8];
    let mut count = list.len() as u32;
    // SAFETY: `list` holds `count` entries.
    let got = unsafe { GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut count, entry) };
    if got == u32::MAX {
        return Err(failed());
    }
    list.truncate(got as usize);
    Ok(list)
}

impl RawInput {
    /// One batch ([`cadence::batch`]): when `read`, what is queued now, up to
    /// [`cadence::DRAIN_CUT`] messages, stamped `now`, then the
    /// publication from a fresh clock read. Returns that read and how many
    /// messages were read. It plays the part of
    /// [`InputAdapter::poll`](crate::InputAdapter::poll), which needs a
    /// `Send` adapter.
    fn poll(&mut self, now: TimeUs, read: bool, sources: &mut SourceSet) -> (TimeUs, u32) {
        let clock = self.clock;
        cadence::batch(&clock, sources, now, read, |sources, budget| {
            self.next(sources, budget)
        })
    }

    /// Takes one message off the queue and what it carries, at most
    /// `budget` packets; returns how many messages and packets were read,
    /// 0 when the queue is empty.
    fn next(&mut self, sources: &mut SourceSet, budget: u32) -> u32 {
        let mut msg = MSG::default();
        // No message filter, so WM_INPUT_DEVICE_CHANGE is never dropped.
        // SAFETY: pumping this thread's own messages.
        if !unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            return 0;
        }
        let mut read = 1;
        match msg.message {
            WM_INPUT => {
                self.read_one(HRAWINPUT(msg.lParam.0 as *mut c_void), sources);
                read += self.drain(sources, budget.saturating_sub(1));
            }
            WM_INPUT_DEVICE_CHANGE => self.device_change(
                msg.wParam.0 as u32,
                HANDLE(msg.lParam.0 as *mut c_void),
                sources,
            ),
            _ => {}
        }
        // SAFETY: a message just retrieved; for WM_INPUT this lets
        // DefWindowProc clean up.
        unsafe { DispatchMessageW(&msg) };
        read
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        let devices = USAGES.map(|usage| RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: usage,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        });
        // SAFETY: removes this process's registrations, then destroys the
        // window this thread created.
        unsafe {
            let _ = RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// The loop both input threads run until `stop` is set: under MMCSS
/// "Games", [`cadence::run`] on the wake event, the timer and, for
/// [`Driven::Input`], the thread's Raw Input queue. `wake` is dropped when
/// the loop ends, however it ends.
///
/// # Errors
///
/// [`InputError::Os`] when a wait or setting the timer failed: the thread
/// ends, and its hook with it.
fn wake_loop(
    clock: QpcClock,
    driven: Driven,
    timer: &Timer,
    waker: &Waker,
    stop: &AtomicBool,
    wake: impl FnMut(TimeUs, bool) -> Boundary,
) -> Result<WakeReport, InputError> {
    let mmcss = Mmcss::games();
    let mut report = WakeReport::new();
    report.mmcss = mmcss.is_some();
    let mut waiter = OsWaiter {
        clock,
        timer,
        event: waker.handle(),
        failed: None,
    };
    let mut cadence = Cadence::new(driven, clock.now());
    let ran = cadence::run(&clock, &mut waiter, stop, &mut cadence, &mut report, wake);
    drop(mmcss);
    match (ran, waiter.failed) {
        (Ok(()), _) => Ok(report),
        (Err(WaitFailed), Some((call, error))) => Err(InputError::os(call, error)),
        (Err(WaitFailed), None) => Err(InputError::os(
            "MsgWaitForMultipleObjectsEx",
            windows::core::Error::from_thread(),
        )),
    }
}

/// A running input thread: the signal that stops it, the event that wakes
/// it and its handle. Dropping it stops and joins the thread.
#[derive(Debug)]
struct Worker {
    stop: Arc<AtomicBool>,
    waker: Waker,
    /// `None` once joined.
    handle: Option<JoinHandle<Result<WakeReport, InputError>>>,
}

impl Worker {
    /// Signals the thread to stop, wakes it, and joins it. From the thread
    /// itself (its hook, or a destructor while it unwinds) joining would
    /// never return, so the thread is detached and ends after its wake.
    fn shutdown(&mut self) -> Result<WakeReport, InputError> {
        self.stop.store(true, Ordering::Release);
        // After the flag: a wait that has not begun yet returns at once
        // on the kept signal, and one under way returns now.
        self.waker.wake();
        // Only a second call finds the handle gone.
        let handle = self.handle.take().ok_or(InputError::Panicked)?;
        if handle.thread().id() == std::thread::current().id() {
            return Err(InputError::StopFromWorker);
        }
        handle.join().map_err(|_| InputError::Panicked)?
    }

    /// Signals the thread to stop, wakes it, and lets it go without
    /// joining: a wake that never returns cannot hold up the caller.
    fn detach(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.waker.wake();
        drop(self.handle.take());
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// What a worker's setup gives its loop: the timer, a value for the
/// caller, and the wake.
type Setup<W> = Result<(Timer, usize, W), InputError>;

/// Spawns a thread named `name` that runs `setup`, reports its outcome,
/// and on success runs [`wake_loop`] with the timer and the wake `setup`
/// returned, woken through `waker`. Returns, with the value `setup` gave,
/// once it has reported.
fn spawn_worker<S, W>(
    name: &str,
    clock: QpcClock,
    driven: Driven,
    waker: &Waker,
    setup: S,
) -> Result<(Worker, usize), InputError>
where
    S: FnOnce() -> Setup<W> + Send + 'static,
    W: FnMut(TimeUs, bool) -> Boundary,
{
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_waker = waker.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let handle = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || match setup() {
            Ok((timer, value, wake)) => {
                let _ = ready_tx.send(Ok(value));
                wake_loop(clock, driven, &timer, &thread_waker, &thread_stop, wake)
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                Ok(WakeReport::default())
            }
        })
        .map_err(InputError::Spawn)?;
    match ready_rx.recv() {
        Ok(Ok(value)) => Ok((
            Worker {
                stop,
                waker: waker.clone(),
                handle: Some(handle),
            },
            value,
        )),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => Err(InputError::Panicked),
    }
}

/// The input thread: under MMCSS "Games" it wakes on Raw Input, on the
/// wake event, and while Open or Tail on a high-resolution timer
/// heartbeat ([`cadence`]); it reads QPC, receives what Raw Input queued
/// as one batch stamped with that read, publishes, and calls the caller's
/// hook at the boundary after the batch. This handle may move between
/// threads; dropping it stops and joins the thread, as
/// [`stop`](Self::stop) does.
#[derive(Debug)]
pub struct InputThread {
    worker: Worker,
    /// The receiver window, as a raw value.
    receiver: usize,
}

impl InputThread {
    /// Starts the thread with `sources`, connecting the keyboards and
    /// controllers present; `waker` wakes it (requests, focus changes),
    /// and stopping it signals `waker` too.
    /// `on_wake` runs after each batch's publication with a clock read
    /// taken after it, which is the QPC read of that boundary (the batch
    /// itself keeps its wake time); it is the boundary where the caller
    /// opens and closes recording, sets recorded controls and wakes the
    /// logic thread, and it must not allocate during play. It returns the
    /// thread's phase from then on; the thread starts in
    /// [`Phase::Tail`].
    ///
    /// # Errors
    ///
    /// [`InputError`] if the thread, the receiver window, the registration
    /// or the timer cannot be set up.
    pub fn spawn<F>(
        clock: QpcClock,
        mut sources: SourceSet,
        capture: Capture,
        waker: &Waker,
        mut on_wake: F,
    ) -> Result<Self, InputError>
    where
        F: FnMut(&mut SourceSet, TimeUs) -> Phase + Send + 'static,
    {
        let (worker, receiver) = spawn_worker("input", clock, Driven::Input, waker, move || {
            let mut raw = RawInput::new(clock, capture)?;
            raw.connect_present(&mut sources)?;
            let timer = Timer::new()?;
            let receiver = raw.hwnd.0 as usize;
            Ok((timer, receiver, move |now, may_read| {
                let (published, read) = raw.poll(now, may_read, &mut sources);
                // Polling may take a while after a scheduling stall; the
                // boundary is read after it.
                let phase = on_wake(&mut sources, clock.now());
                Boundary {
                    published,
                    read: Some(read),
                    phase,
                }
            }))
        })?;
        Ok(Self { worker, receiver })
    }

    /// The receiver window the keyboards are registered to, as a raw
    /// `HWND` value, for comparing with [`keyboard_target`].
    #[must_use]
    pub const fn receiver(&self) -> usize {
        self.receiver
    }

    /// Stops the thread after its current wake and returns what it
    /// measured.
    ///
    /// # Errors
    ///
    /// [`InputError::Panicked`] if the thread panicked,
    /// [`InputError::StopFromWorker`] if called from the input thread
    /// itself, which then ends after its current wake, and
    /// [`InputError::Os`] if a wait failed and ended the thread.
    pub fn stop(mut self) -> Result<WakeReport, InputError> {
        self.worker.shutdown()
    }

    /// Signals the thread to stop after its current wake and returns at
    /// once, without waiting for it or its report: for the process's exit,
    /// which a stuck wake must not hold up.
    pub fn detach(mut self) {
        self.worker.detach();
    }
}

/// An input thread without Raw Input, for input that comes from no device
/// (autoplay): under MMCSS "Games" as [`InputThread`], it wakes on the
/// high-resolution timer heartbeat while Open or Tail and on the wake
/// event only while Idle ([`Driven::Timer`]), calling the caller's hook
/// with the clock read at each wake; the hook feeds the sources and
/// publishes. Nothing is registered, so no device is received. Dropping
/// the handle stops and joins the thread, as [`stop`](Self::stop) does.
#[derive(Debug)]
pub struct ClockedThread {
    worker: Worker,
}

impl ClockedThread {
    /// Starts the thread with `sources`, woken through `waker` as
    /// [`InputThread::spawn`]. `on_wake` runs at every wake with the clock
    /// read there: it feeds the sources and ends with a publication, opens
    /// and closes recording and wakes the logic thread, must not allocate
    /// during play, and returns the thread's phase from then on.
    ///
    /// # Errors
    ///
    /// [`InputError`] if the thread or the timer cannot be set up.
    pub fn spawn<F>(
        clock: QpcClock,
        mut sources: SourceSet,
        waker: &Waker,
        mut on_wake: F,
    ) -> Result<Self, InputError>
    where
        F: FnMut(&mut SourceSet, TimeUs) -> Phase + Send + 'static,
    {
        let (worker, _) = spawn_worker("clocked input", clock, Driven::Timer, waker, move || {
            Ok((Timer::new()?, 0, move |now, _| Boundary {
                published: now,
                read: None,
                phase: on_wake(&mut sources, now),
            }))
        })?;
        Ok(Self { worker })
    }

    /// Stops the thread after its current wake and returns what it
    /// measured.
    ///
    /// # Errors
    ///
    /// As [`InputThread::stop`].
    pub fn stop(mut self) -> Result<WakeReport, InputError> {
        self.worker.shutdown()
    }

    /// As [`InputThread::detach`].
    pub fn detach(mut self) {
        self.worker.detach();
    }
}

/// The window this process's keyboard Raw Input registration targets, as a
/// raw `HWND` value; `None` when keyboards are not registered (or are
/// registered without a target window). Registration is per process, so
/// another registration of keyboards, such as a window library's, replaces
/// the input thread's: compare with [`InputThread::receiver`].
#[must_use]
pub fn keyboard_target() -> Option<usize> {
    let entry = size_of::<RAWINPUTDEVICE>() as u32;
    let mut list = [RAWINPUTDEVICE::default(); 16];
    let mut count = list.len() as u32;
    // SAFETY: `list` holds `count` entries.
    let got = unsafe { GetRegisteredRawInputDevices(Some(list.as_mut_ptr()), &mut count, entry) };
    if got == u32::MAX {
        return None;
    }
    list.iter()
        .take(got as usize)
        .find(|d| d.usUsagePage == 0x01 && d.usUsage == 0x06)
        .map(|d| d.hwndTarget.0 as usize)
        .filter(|&h| h != 0)
}

#[cfg(test)]
mod qpc_tests {
    use bms_model::TimeSource;

    use super::{QpcClock, read_qpc};

    /// A QPC value read between two clock reads converts to a time between
    /// them; one second of ticks later is one second later.
    #[test]
    fn raw_qpc_converts_onto_the_clock() {
        let clock = QpcClock::start();
        let before = clock.now();
        let ticks = read_qpc();
        let after = clock.now();
        let t = clock.from_qpc(ticks);
        assert!(before <= t && t <= after, "{before:?} {t:?} {after:?}");
        let later = clock.from_qpc(ticks + clock.frequency().hz());
        assert_eq!(later.0 - t.0, 1_000_000);
    }
}

#[cfg(test)]
mod clocked_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use bms_model::{TimeSource, TimeUs};

    use super::{ClockedThread, QpcClock, Waker};
    use crate::{AUTOPLAY_DEVICE, DeviceKind, Phase, SourceConfig, SourceSet};

    fn set() -> SourceSet {
        SourceSet::new(SourceConfig {
            max_devices: 1,
            max_sources: 1,
            max_held: 1,
            queue_capacity: 1,
            max_arrivals: 1,
        })
        .unwrap()
    }

    /// Sets its flag when dropped, which happens when the thread's hook is.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    /// The clocked thread wakes about every millisecond with the clock's
    /// time and the source set it was given while in its tail, and stops
    /// when asked; every wake is a heartbeat.
    #[test]
    fn the_clocked_thread_wakes_with_the_clock_and_its_sources() {
        let clock = QpcClock::start();
        let mut set = set();
        set.connect(AUTOPLAY_DEVICE, DeviceKind::Keyboard);
        let last = Arc::new(AtomicI64::new(i64::MIN));
        let seen = Arc::clone(&last);
        let start = clock.now();
        let waker = Waker::new().unwrap();
        let thread = ClockedThread::spawn(clock, set, &waker, move |s, now| {
            assert_eq!(s.devices().count(), 1);
            seen.store(now.0, Ordering::Release);
            Phase::Tail
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let report = thread.stop().unwrap();
        let woke = last.load(Ordering::Acquire);
        assert!(TimeUs(woke) > start && TimeUs(woke) <= clock.now());
        // About 100 wakes; a loaded machine may skip some.
        let wakes = report.causes.heartbeat;
        assert!((20..=150).contains(&wakes), "{wakes}");
        assert_eq!(report.causes.input, 0);
        assert_eq!(report.lateness.count(), wakes);
    }

    /// An idle worker waits on its wake event alone, with no timer. Stop
    /// wakes it and joins it, and so does dropping its handle: each
    /// returns within a bound, with the hook dropped.
    #[test]
    fn stop_and_drop_join_an_idle_worker() {
        for by_drop in [false, true] {
            let waker = Waker::new().unwrap();
            let dropped = Arc::new(AtomicBool::new(false));
            let flag = DropFlag(Arc::clone(&dropped));
            let (idle_tx, idle_rx) = mpsc::channel();
            let thread = ClockedThread::spawn(QpcClock::start(), set(), &waker, move |_, _| {
                let _keep = &flag;
                let _ = idle_tx.send(());
                Phase::Idle
            })
            .unwrap();
            // The first heartbeat reports Idle; no wake follows it.
            idle_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            assert!(idle_rx.try_recv().is_err(), "an idle worker woke");
            let (done_tx, done_rx) = mpsc::channel();
            let begun = Instant::now();
            let stopper = std::thread::spawn(move || {
                let waits = if by_drop {
                    drop(thread);
                    None
                } else {
                    Some(thread.stop().unwrap().waits)
                };
                let _ = done_tx.send(waits);
            });
            let waits = done_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("the idle worker was not joined (drop: {by_drop})"));
            assert!(
                begun.elapsed() < Duration::from_secs(1),
                "{:?}",
                begun.elapsed()
            );
            assert!(dropped.load(Ordering::Acquire));
            // The heartbeat that went Idle and the stop's wake.
            if let Some(waits) = waits {
                assert!(waits <= 3, "{waits}");
            }
            stopper.join().unwrap();
        }
    }

    /// Detaching returns while a wake is blocked, and the thread still
    /// stops by itself once the wake returns.
    #[test]
    fn detach_returns_while_a_wake_is_blocked() {
        let clock = QpcClock::start();
        let (entered_tx, entered) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        // Disconnected once the thread has ended and dropped its wake.
        let (_ended_tx, ended) = mpsc::channel::<()>();
        let waker = Waker::new().unwrap();
        let thread = ClockedThread::spawn(clock, set(), &waker, move |_, _| {
            let _held = &_ended_tx;
            let _ = entered_tx.send(());
            // Blocked until released, within a bound.
            let _ = released.recv_timeout(Duration::from_secs(10));
            Phase::Tail
        })
        .unwrap();
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let start = Instant::now();
        thread.detach();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
        release.send(()).unwrap();
        assert_eq!(
            ended.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
    }
}

#[cfg(test)]
mod error_tests {
    use std::error::Error as _;

    use windows::Win32::Foundation::E_FAIL;

    use super::InputError;

    /// The OS error and the spawn error are sources, and each wrapper's
    /// text leaves them out.
    #[test]
    fn os_and_spawn_failures_are_sources_printed_once() {
        let error = windows::core::Error::from_hresult(E_FAIL);
        let e = InputError::os("RegisterRawInputDevices", error.clone());
        assert_eq!(e.to_string(), "RegisterRawInputDevices failed");
        let source = e.source().expect("the OS error is the source");
        assert_eq!(
            source
                .downcast_ref::<windows::core::Error>()
                .map(windows::core::Error::code),
            Some(error.code())
        );
        assert!(!e.to_string().contains(&source.to_string()));

        let e = InputError::Spawn(std::io::Error::other("no threads"));
        assert_eq!(e.to_string(), "spawning the input thread failed");
        let source = e.source().expect("the spawn error is the source");
        assert_eq!(
            source
                .downcast_ref::<std::io::Error>()
                .map(ToString::to_string),
            Some("no threads".to_owned())
        );
        assert!(!e.to_string().contains(&source.to_string()));

        assert!(InputError::Panicked.source().is_none());
        assert!(InputError::StopFromWorker.source().is_none());
    }
}
