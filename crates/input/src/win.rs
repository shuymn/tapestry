//! Windows: the QPC clock, Raw Input keyboards and the input thread.
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
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::Win32::UI::Input::{
    GetRawInputBuffer, GetRawInputData, GetRawInputDeviceInfoW, GetRawInputDeviceList,
    GetRegisteredRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTDEVICELIST,
    RAWINPUTHEADER, RAWKEYBOARD, RID_DEVICE_INFO, RID_INPUT, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK,
    RIDEV_REMOVE, RIDI_DEVICEINFO, RIM_TYPEKEYBOARD, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GIDC_ARRIVAL, GIDC_REMOVAL,
    HWND_MESSAGE, MSG, PM_REMOVE, PeekMessageW, RI_KEY_BREAK, RI_KEY_E0, RI_KEY_E1, RIM_INPUTSINK,
    RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_INPUT_DEVICE_CHANGE, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use crate::capture::Capture;
use crate::gate::DeviceId;
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

/// Bytes in the buffer for one packet read with `GetRawInputData`.
const ONE_WORDS: usize = 128;
/// Bytes in the buffer `GetRawInputBuffer` drains into.
const MANY_WORDS: usize = 2048;
/// `size_of::<RAWINPUTHEADER>()` as the calls take it.
const HEADER_BYTES: u32 = raw::HEADER_SIZE as u32;

fn as_bytes(words: &[u64]) -> &[u8] {
    // SAFETY: any u64 is valid as 8 bytes, and the length is in bytes.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast(), words.len() * 8) }
}

/// Keyboards through Raw Input: a message-only window registered for
/// keyboards with `RIDEV_INPUTSINK | RIDEV_DEVNOTIFY`, so background input
/// arrives marked `RIM_INPUTSINK` (and is rejected by the focus gate) and
/// device arrivals and removals arrive as messages. Each keyboard is a
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
}

impl RawInput {
    /// Creates the receiver window on this thread and registers keyboards.
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
        let device = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x06,
            dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
            hwndTarget: hwnd,
        };
        // SAFETY: one valid device description.
        let registered =
            unsafe { RegisterRawInputDevices(&[device], size_of::<RAWINPUTDEVICE>() as u32) };
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
        })
    }

    /// Connects the keyboards present now, in the order the OS lists them.
    /// The registration also announces them as arrivals, which then change
    /// nothing. Call it before recording opens: it allocates.
    ///
    /// # Errors
    ///
    /// [`InputError::Os`] if the device list cannot be read.
    fn connect_present(&self, sources: &mut SourceSet) -> Result<(), InputError> {
        let entry = size_of::<RAWINPUTDEVICELIST>() as u32;
        let failed =
            || InputError::os("GetRawInputDeviceList", windows::core::Error::from_thread());
        let mut count = 0;
        // SAFETY: a null list asks for the count only.
        let asked = unsafe { GetRawInputDeviceList(None, &mut count, entry) };
        if asked == u32::MAX {
            return Err(failed());
        }
        // Room for keyboards that arrive between the two calls.
        count += 8;
        let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize];
        // SAFETY: `list` holds `count` entries.
        let got = unsafe { GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut count, entry) };
        if got == u32::MAX {
            return Err(failed());
        }
        for d in list.iter().take(got as usize) {
            if d.dwType == RIM_TYPEKEYBOARD {
                sources.connect(device_id(d.hDevice), DeviceKind::Keyboard);
            }
        }
        Ok(())
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
        if got == 0 || got == u32::MAX {
            return;
        }
        if let Some(block) = as_bytes(&self.one).get(..got as usize) {
            deliver(block, &self.capture, sources);
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
            if count == 0 || count == u32::MAX {
                return;
            }
            for block in raw::blocks(as_bytes(&self.many), count as usize) {
                deliver(block, &self.capture, sources);
            }
        }
    }

    fn device_change(change: u32, handle: HANDLE, sources: &mut SourceSet) {
        match change {
            GIDC_ARRIVAL if is_keyboard(handle) => {
                sources.connect(device_id(handle), DeviceKind::Keyboard);
            }
            GIDC_REMOVAL => sources.disconnect(device_id(handle)),
            _ => {}
        }
    }
}

fn is_keyboard(handle: HANDLE) -> bool {
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
    got != u32::MAX && info.dwType == RIM_TYPEKEYBOARD
}

/// One decoded block to the capture slot and the sources.
fn deliver(block: &[u8], capture: &Capture, sources: &mut SourceSet) {
    if let Some(packet) = raw::decode_keyboard(block) {
        capture.offer(&packet, DeviceKind::Keyboard, sources.is_recording());
        sources.packet(&packet);
    }
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
                WM_INPUT_DEVICE_CHANGE => Self::device_change(
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
        let device = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x06,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        };
        // SAFETY: removes this process's keyboard registration, then
        // destroys the window this thread created.
        unsafe {
            let _ = RegisterRawInputDevices(&[device], size_of::<RAWINPUTDEVICE>() as u32);
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
    /// Starts the thread with `sources`, connecting the keyboards present.
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
                let setup = RawInput::new(clock, capture).and_then(|raw| {
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
