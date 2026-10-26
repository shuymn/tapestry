//! Windows: the QPC clock, Raw Input keyboards and HID game controllers,
//! and the input thread.
//!
//! Decoding the packets is portable ([`crate::raw`]); this module only
//! makes the OS calls that fill the buffers. The layout the decoder assumes
//! is checked against the `windows` crate's definitions at compile time.

use std::ffi::c_void;
use std::fmt;
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
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::Win32::UI::Input::{
    GetRawInputBuffer, GetRawInputData, GetRawInputDeviceInfoW, GetRawInputDeviceList,
    GetRegisteredRawInputDevices, HRAWINPUT, RAWHID, RAWINPUT, RAWINPUTDEVICE, RAWINPUTDEVICELIST,
    RAWINPUTHEADER, RAWKEYBOARD, RID_DEVICE_INFO, RID_INPUT, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK,
    RIDEV_REMOVE, RIDI_DEVICEINFO, RIDI_DEVICENAME, RIDI_PREPARSEDDATA, RIM_TYPEHID,
    RIM_TYPEKEYBOARD, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GIDC_ARRIVAL, GIDC_REMOVAL,
    HWND_MESSAGE, MSG, PM_REMOVE, PeekMessageW, RI_KEY_BREAK, RI_KEY_E0, RI_KEY_E1, RIM_INPUTSINK,
    RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_INPUT_DEVICE_CHANGE, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use crate::capture::Capture;
use crate::gate::DeviceId;
use crate::hid::{
    AXIS_USAGES, AxisCap, ButtonGroup, CollectionSlot, DESKTOP_PAGE, HidDevice, HidLayout,
    HidReport, MAX_AXES, MAX_REPORT_BYTES, MAX_REPORT_IDS, axis_cap, axis_control, axis_value,
    collection_of, in_collection, read_report, remove_collection,
};
use crate::raw;
use crate::sources::{DeviceKind, SourceSet};
use crate::time::{QpcFrequency, WakeHistogram, qpc_to_us};

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
#[derive(Debug)]
pub enum InputError {
    /// An OS call failed.
    Os {
        /// The call.
        call: &'static str,
        /// Its error.
        error: windows::core::Error,
    },
    /// The thread could not be spawned.
    Spawn(std::io::Error),
    /// The thread panicked.
    Panicked,
    /// [`InputThread::stop`] was called from the input thread itself; the
    /// thread ends after its current wake without being joined.
    StopFromWorker,
}

impl InputError {
    const fn os(call: &'static str, error: windows::core::Error) -> Self {
        Self::Os { call, error }
    }
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Os { call, error } => write!(f, "{call} failed: {error}"),
            Self::Spawn(e) => write!(f, "spawning the input thread failed: {e}"),
            Self::Panicked => f.write_str("the input thread panicked"),
            Self::StopFromWorker => f.write_str("the input thread cannot join itself"),
        }
    }
}

impl std::error::Error for InputError {}

fn read_qpc() -> i64 {
    let mut ticks = 0;
    // SAFETY: writes one i64.
    let read = unsafe { QueryPerformanceCounter(&mut ticks) };
    // It cannot fail on Windows XP and later.
    read.expect("QueryPerformanceCounter");
    ticks
}

/// The QPC frequency, read once per process.
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

    /// Sleeps until `clock` reads `deadline`, without spinning.
    fn wait_until(&self, clock: &QpcClock, deadline: TimeUs) {
        let left = deadline.0.saturating_sub(clock.now().0);
        if left <= 0 {
            return;
        }
        // Relative due time in 100 ns units.
        let due = -left.saturating_mul(10);
        // SAFETY: a valid timer handle and no completion routine.
        unsafe {
            if SetWaitableTimer(self.0, &due, 0, None, None, false).is_ok() {
                WaitForSingleObject(self.0, 1000);
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
    /// The physical device's container, when the collection is grouped by
    /// it.
    container: Option<u128>,
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
        self.container = None;
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

/// The source of a controller in `container`: one per physical device.
/// Handles are pointers, below 2^63; these ids are above.
const fn container_device(container: u128) -> DeviceId {
    DeviceId((container as u64 ^ (container >> 64) as u64) | 1 << 63)
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
                    container: None,
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
        let collection = read_path(handle, &mut self.path).map(collection_of);
        let container = collection
            .and_then(|_| container_of(&self.path, &mut self.instance))
            .filter(|&c| c != NO_CONTAINER)
            // Two collections of one device with one number cannot share
            // a source: each keeps its own.
            .filter(|&c| {
                !self.slots.iter().any(|s| {
                    s.handle.is_some() && s.container == Some(c) && Some(s.collection) == collection
                })
            });
        let collection = collection.unwrap_or(0);
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
        }; MAX_AXES];
        let mut groups = [ButtonGroup {
            report_id: 0,
            buttons: 0,
        }; MAX_REPORT_IDS];
        let data = slot.data();
        let (n_axes, axis_ids) = read_axes(data, collection, &mut slot.reads, &mut caps);
        let (n_groups, button_ids) = read_buttons(data, &mut groups);
        let device = container.map_or(device_id(handle), container_device);
        // A collection unread before is read now, under its source's id.
        sources.forget_unread(device_id(handle));
        sources.connect(device, DeviceKind::Controller);
        // Only axes the source keeps are read: one beyond the device's
        // table is never captured or recorded.
        let kept = sources.add_axes(device, &caps[..n_axes]);
        let mut n = 0;
        for i in 0..n_axes {
            if kept[i] {
                caps[n] = caps[i];
                slot.reads[n] = slot.reads[i];
                n += 1;
            }
        }
        slot.handle = Some(key);
        slot.container = container;
        slot.collection = collection;
        slot.usage = hid_usage(handle).unwrap_or((0, 0));
        slot.report_ids = axis_ids || button_ids;
        slot.state = HidDevice::new(
            device,
            &HidLayout {
                collection,
                axes: &caps[..n],
                buttons: &groups[..n_groups],
            },
        );
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
    let len = buf.iter().position(|&c| c == 0)?;
    Some(&buf[..len])
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
            let cap = axis_cap(control, v.IsAbsolute, v.LogicalMin, v.LogicalMax, v.BitSize);
            if n == MAX_AXES || caps[..n].iter().any(|c| c.control == control) || cap.is_none() {
                continue;
            }
            reads[n] = AxisRead {
                page: v.UsagePage,
                usage,
                bits: v.BitSize,
                signed: v.LogicalMin < 0,
                min: v.LogicalMin,
                max: v.LogicalMax,
            };
            let Some(cap) = cap else {
                continue;
            };
            caps[n] = cap;
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
        if let Some(g) = groups[..n].iter_mut().find(|g| g.report_id == b.ReportID) {
            g.buttons |= bits;
        } else if n < MAX_REPORT_IDS {
            groups[n] = ButtonGroup {
                report_id: b.ReportID,
                buttons: bits,
            };
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

    /// Reads every packet still queued, in blocks.
    fn drain(&mut self, sources: &mut SourceSet) {
        loop {
            let mut size = (self.many.len() * 8) as u32;
            // SAFETY: `many` holds `size` bytes and is 8-byte aligned.
            let count = unsafe {
                GetRawInputBuffer(
                    Some(self.many.as_mut_ptr().cast::<RAWINPUT>()),
                    &mut size,
                    HEADER_BYTES,
                )
            };
            // On failure, such as a next packet larger than the buffer,
            // the packets stay queued and arrive as their own `WM_INPUT`.
            if count == 0 || count == u32::MAX {
                return;
            }
            for block in raw::blocks(as_bytes(&self.many), count as usize) {
                deliver(block, &self.capture, &mut self.controllers, sources);
            }
        }
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
    let hid = unsafe { info.Anonymous.hid };
    Some((hid.usUsagePage, hid.usUsage))
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
            grouped: s.container.is_some(),
            collection: s.collection,
            usage: s.usage,
            axes: s.state.axes().to_vec(),
            declared: s.reads[..s.state.axes().len()]
                .iter()
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
    /// One batch: everything queued now, stamped `now`, then the
    /// publication from a fresh clock read. It plays the part of
    /// [`InputAdapter::poll`](crate::InputAdapter::poll), which needs a
    /// `Send` adapter.
    fn poll(&mut self, now: TimeUs, sources: &mut SourceSet) {
        sources.begin_batch(now);
        let mut msg = MSG::default();
        // No message filter, so WM_INPUT_DEVICE_CHANGE is never dropped.
        // SAFETY: pumping this thread's own messages.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            match msg.message {
                WM_INPUT => {
                    self.read_one(HRAWINPUT(msg.lParam.0 as *mut c_void), sources);
                    self.drain(sources);
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
        }
        sources.end_batch(self.clock.now());
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

/// The input thread's wake period: 1 kHz.
const WAKE_PERIOD: TimeUs = TimeUs(1000);

/// What the input thread measured.
#[derive(Clone, Debug, Default)]
pub struct WakeReport {
    /// The intervals between consecutive wakes.
    pub intervals: WakeHistogram,
    /// Whether the MMCSS "Games" registration succeeded.
    pub mmcss: bool,
}

/// The input thread: wakes at 1 kHz on a high-resolution timer under MMCSS
/// "Games", reads QPC, receives everything Raw Input queued as one batch
/// stamped with that read, publishes, and calls the owner's hook at the
/// boundary after the batch. This handle may move between threads;
/// dropping it stops and joins the thread, as [`stop`](Self::stop) does.
#[derive(Debug)]
pub struct InputThread {
    stop: Arc<AtomicBool>,
    /// `None` once joined.
    handle: Option<JoinHandle<WakeReport>>,
    /// The receiver window, as a raw value.
    receiver: usize,
}

impl InputThread {
    /// Starts the thread with `sources`, connecting the keyboards and
    /// controllers present.
    /// `on_wake` runs after each batch's publication with a clock read
    /// taken after it, which is the QPC read of that boundary (the batch
    /// itself keeps its wake time); it is the boundary where the owner opens and closes recording, sets
    /// recorded controls and wakes the logic thread, and it must not
    /// allocate during play.
    ///
    /// # Errors
    ///
    /// [`InputError`] if the thread, the receiver window, the registration
    /// or the timer cannot be set up.
    pub fn spawn<F>(
        clock: QpcClock,
        mut sources: SourceSet,
        capture: Capture,
        mut on_wake: F,
    ) -> Result<Self, InputError>
    where
        F: FnMut(&mut SourceSet, TimeUs) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let handle = std::thread::Builder::new()
            .name("input".into())
            .spawn(move || {
                let setup = RawInput::new(clock, capture).and_then(|mut raw| {
                    raw.connect_present(&mut sources)?;
                    Ok((raw, Timer::new()?))
                });
                let (mut raw, timer) = match setup {
                    Ok(ready) => {
                        let _ = ready_tx.send(Ok(ready.0.hwnd.0 as usize));
                        ready
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return WakeReport::default();
                    }
                };
                let mmcss = Mmcss::games();
                let mut report = WakeReport {
                    intervals: WakeHistogram::new(),
                    mmcss: mmcss.is_some(),
                };
                let mut last = clock.now();
                let mut next = last;
                while !thread_stop.load(Ordering::Acquire) {
                    next = TimeUs(next.0 + WAKE_PERIOD.0);
                    timer.wait_until(&clock, next);
                    let now = clock.now();
                    // Wakes keep their phase, and a missed one is skipped
                    // rather than made up in a burst.
                    let missed = (now.0 - next.0).div_euclid(WAKE_PERIOD.0).max(0);
                    next = TimeUs(next.0 + missed * WAKE_PERIOD.0);
                    report.intervals.record(now.0.abs_diff(last.0));
                    last = now;
                    raw.poll(now, &mut sources);
                    // Polling may take a while after a scheduling stall;
                    // the boundary is read after it.
                    on_wake(&mut sources, clock.now());
                }
                drop(mmcss);
                report
            })
            .map_err(InputError::Spawn)?;
        match ready_rx.recv() {
            Ok(Ok(receiver)) => Ok(Self {
                stop,
                handle: Some(handle),
                receiver,
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => Err(InputError::Panicked),
        }
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
    /// [`InputError::Panicked`] if the thread panicked, and
    /// [`InputError::StopFromWorker`] if called from the input thread
    /// itself, which then ends after its current wake.
    pub fn stop(mut self) -> Result<WakeReport, InputError> {
        self.shutdown()
    }

    /// Signals the thread to stop and joins it. From the input thread
    /// itself (its hook, or a destructor while it unwinds) joining would
    /// never return, so the thread is detached and ends after its wake.
    fn shutdown(&mut self) -> Result<WakeReport, InputError> {
        self.stop.store(true, Ordering::Release);
        // Only a second call finds the handle gone.
        let handle = self.handle.take().ok_or(InputError::Panicked)?;
        if handle.thread().id() == std::thread::current().id() {
            return Err(InputError::StopFromWorker);
        }
        handle.join().map_err(|_| InputError::Panicked)
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        let _ = self.shutdown();
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
