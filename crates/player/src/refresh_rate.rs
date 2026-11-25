//! The refresh rate of the window's monitor, for the uploader's slice
//! (design.md "Memory Budget", the uploader): the active display mode's
//! rate, a rational, from the display configuration, re-read when the
//! window receives `WM_DISPLAYCHANGE` (through a window subclass, which
//! sees the messages sent to the window procedure that winit's message
//! hook never sees), moves to another monitor or changes scale, and
//! published to the [`Extent`] apart from its size ([`RateFeed`]). A
//! window whose subclass cannot be installed has no observer of a mode
//! change, so its rate is published as unknown and stays so: every pass
//! then takes the whole slice, the design's unknown-rate fallback.

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
    QueryDisplayConfig,
};
use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW,
    MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::WM_DISPLAYCHANGE;
use windows::core::PCWSTR;
use winit::event::WindowEvent;

use crate::render_side::Extent;

/// Where a rate read came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The active path's rational rate in the display configuration.
    DisplayConfig,
    /// The monitor's current display settings, whole hertz: the display
    /// configuration did not answer for it.
    DisplaySettings,
    /// Neither answered.
    Unknown,
}

impl Source {
    const fn name(self) -> &'static str {
        match self {
            Self::DisplayConfig => "display configuration",
            Self::DisplaySettings => "display settings",
            Self::Unknown => "nowhere",
        }
    }

    /// Its number for [`LAST_SOURCE`].
    const fn code(self) -> u8 {
        match self {
            Self::DisplayConfig => 1,
            Self::DisplaySettings => 2,
            Self::Unknown => 3,
        }
    }
}

/// The source of the rate published last ([`Source::code`]), 0 before
/// one: a change of source is noted even at the same rate, so a run's log
/// says where each rate came from.
static LAST_SOURCE: AtomicU8 = AtomicU8::new(0);

/// Reads the refresh rate of a window's monitor: mHz (0: unknown) and
/// where it came from. [`read`], or a test's.
pub type ReadRate = fn(HWND) -> (u32, Source);

/// The refresh rate of `hwnd`'s monitor: the display configuration's
/// rate for its active path, else its display settings' whole hertz,
/// else unknown (0).
pub fn read(hwnd: HWND) -> (u32, Source) {
    let Some(device) = monitor_device(hwnd) else {
        return (0, Source::Unknown);
    };
    if let Some(mhz) = active_rate_mhz(&device) {
        return (mhz, Source::DisplayConfig);
    }
    settings_rate_mhz(&device).map_or((0, Source::Unknown), |mhz| (mhz, Source::DisplaySettings))
}

/// Reads the rate with `read` and publishes it to `extent`; the console
/// notes each change of rate or of its source (monitor names stay out of
/// it).
fn publish(extent: &Extent, hwnd: HWND, read: ReadRate) {
    let (mhz, source) = read(hwnd);
    let rate_changed = extent.publish_rate(mhz) != mhz;
    let source_changed = LAST_SOURCE.swap(source.code(), Ordering::AcqRel) != source.code();
    if rate_changed || source_changed {
        // Written, not printed: this runs inside a window procedure, where
        // a panic cannot unwind.
        let _ = writeln!(
            std::io::stdout(),
            "window: refresh rate {mhz} mHz from the {}",
            source.name()
        );
    }
}

/// The window's refresh-rate feed: its subclass, installed first, and the
/// rate published to the [`Extent`] at the window's creation, at each
/// display change and at each event that may have moved it to another
/// monitor ([`window_event`](Self::window_event)). When the subclass
/// cannot be installed, no display change would be seen, so the rate is
/// published unknown (0) and never read again: the boundaries then take
/// the whole slice ("Screen Transitions and Loading", rate delivery).
pub struct RateFeed {
    hwnd: HWND,
    read: ReadRate,
    watch: Option<Watch>,
}

impl RateFeed {
    /// Starts the feed of `hwnd` (a window of this thread) to `extent`,
    /// reading with `read`.
    pub fn start(hwnd: HWND, extent: &Arc<Extent>, read: ReadRate) -> Self {
        let watch = Watch::install(hwnd, Arc::clone(extent), read);
        if watch.is_some() {
            publish(extent, hwnd, read);
        } else {
            extent.publish_rate(0);
            LAST_SOURCE.store(Source::Unknown.code(), Ordering::Release);
            let _ = writeln!(
                std::io::stdout(),
                "window: the display-change watch could not be installed: refresh rate unknown"
            );
        }
        Self { hwnd, read, watch }
    }

    /// Whether display changes are observed: the subclass is installed
    /// (tests).
    #[cfg(test)]
    pub const fn watching(&self) -> bool {
        self.watch.is_some()
    }

    /// The window's `event`: one that may have changed its monitor reads
    /// the rate again ([`rereads`]).
    pub fn window_event(&self, extent: &Extent, event: &WindowEvent) {
        if rereads(event) {
            self.reread(extent);
        }
    }

    /// Reads the rate again and publishes it, while display changes are
    /// observed; an unobserved window's stays unknown.
    pub fn reread(&self, extent: &Extent) {
        if self.watch.is_some() {
            publish(extent, self.hwnd, self.read);
        }
    }
}

/// Whether `event` may have changed the window's monitor or its mode, so
/// the rate is read again: a move (to another monitor, perhaps), a scale
/// change (which a move across monitors brings).
pub const fn rereads(event: &WindowEvent) -> bool {
    matches!(
        event,
        WindowEvent::Moved(_) | WindowEvent::ScaleFactorChanged { .. }
    )
}

/// mHz of a rational rate in Hz, computed whole before dividing (60 000 /
/// 1 001 Hz is 59 940 mHz); `None` for a zero denominator or a rate past
/// `u32`.
pub fn rational_mhz(numerator: u32, denominator: u32) -> Option<u32> {
    if denominator == 0 {
        return None;
    }
    let mhz = u64::from(numerator) * 1_000 / u64::from(denominator);
    u32::try_from(mhz).ok()
}

/// The GDI device name of `hwnd`'s monitor (the nearest one).
fn monitor_device(hwnd: HWND) -> Option<[u16; 32]> {
    // SAFETY: any window handle is accepted; a stale one gives the
    // nearest monitor or none.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = u32::try_from(size_of::<MONITORINFOEXW>()).ok()?;
    // SAFETY: `info` is a MONITORINFOEXW, its size given, and outlives the
    // call.
    unsafe { GetMonitorInfoW(monitor, &raw mut info.monitorInfo) }
        .as_bool()
        .then_some(info.szDevice)
}

/// The rate of the active display path whose source is `device`, from
/// the display configuration.
fn active_rate_mhz(device: &[u16; 32]) -> Option<u32> {
    let (paths, _modes) = active_paths()?;
    paths.iter().find_map(|path| {
        let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: u32::try_from(size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>()).ok()?,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: the header heads a DISPLAYCONFIG_SOURCE_DEVICE_NAME of the
        // size it states, which outlives the call.
        let found = unsafe { DisplayConfigGetDeviceInfo(&raw mut name.header) };
        if found != 0 || !same_name(&name.viewGdiDeviceName, device) {
            return None;
        }
        let rate = path.targetInfo.refreshRate;
        rational_mhz(rate.Numerator, rate.Denominator)
    })
}

/// The active display paths and their modes; asked again (a few times)
/// when the configuration grew between the size query and the query.
fn active_paths() -> Option<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)> {
    for _ in 0..3 {
        let (mut path_count, mut mode_count) = (0_u32, 0_u32);
        // SAFETY: both counts outlive the call.
        let sized = unsafe {
            GetDisplayConfigBufferSizes(
                QDC_ONLY_ACTIVE_PATHS,
                &raw mut path_count,
                &raw mut mode_count,
            )
        };
        if sized != ERROR_SUCCESS {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // SAFETY: each array holds the count passed with it, and all of
        // them outlive the call.
        let queried = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &raw mut path_count,
                paths.as_mut_ptr(),
                &raw mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if queried == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if queried != ERROR_SUCCESS {
            return None;
        }
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        return Some((paths, modes));
    }
    None
}

/// The rate of `device`'s current display settings, whole hertz; none
/// for the hardware default (0 or 1).
fn settings_rate_mhz(device: &[u16; 32]) -> Option<u32> {
    let mut mode = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).ok()?,
        ..Default::default()
    };
    // SAFETY: `device` is a NUL-terminated name in its array (a
    // MONITORINFOEXW's), and `mode` states its size; both outlive the
    // call.
    let found = unsafe {
        EnumDisplaySettingsW(
            PCWSTR(device.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &raw mut mode,
        )
    };
    let hz = mode.dmDisplayFrequency;
    (found.as_bool() && hz > 1).then(|| hz.saturating_mul(1_000))
}

/// Whether two NUL-terminated device names in their arrays are the same.
fn same_name(a: &[u16; 32], b: &[u16; 32]) -> bool {
    let end = |n: &[u16; 32]| n.iter().position(|&c| c == 0).unwrap_or(n.len());
    a.get(..end(a)) == b.get(..end(b))
}

/// The subclass's identifier on the window.
const SUBCLASS_ID: usize = 1;

/// What the subclass procedure publishes to.
struct Watched {
    extent: Arc<Extent>,
    read: ReadRate,
}

/// The window's subclass that publishes the rate at each
/// `WM_DISPLAYCHANGE`; removed when dropped, before the window goes.
pub struct Watch {
    hwnd: HWND,
    watched: *mut Watched,
}

impl Watch {
    /// Subclasses `hwnd` (a window of this thread) to publish its rate,
    /// read with `read`, to `extent` at each `WM_DISPLAYCHANGE`; `None`
    /// when Windows refuses.
    pub fn install(hwnd: HWND, extent: Arc<Extent>, read: ReadRate) -> Option<Self> {
        let watched = Box::into_raw(Box::new(Watched { extent, read }));
        // SAFETY: the window is this thread's; `watched` stays alive until
        // the subclass is removed (`Drop`).
        let set = unsafe { SetWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID, watched as usize) };
        if set.as_bool() {
            Some(Self { hwnd, watched })
        } else {
            // SAFETY: made above and never handed out.
            drop(unsafe { Box::from_raw(watched) });
            None
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: on the window's thread. A window already destroyed lost
        // its subclasses at its destruction, so the procedure no longer
        // runs either way.
        let _ = unsafe { RemoveWindowSubclass(self.hwnd, Some(subclass), SUBCLASS_ID) };
        // SAFETY: made by `install`, and the subclass that read it is gone.
        drop(unsafe { Box::from_raw(self.watched) });
    }
}

/// The subclass procedure: publishes the rate at `WM_DISPLAYCHANGE`, and
/// passes every message on.
unsafe extern "system" fn subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    if msg == WM_DISPLAYCHANGE {
        // SAFETY: `data` is the `Watched` installed with this subclass,
        // alive until the subclass is removed.
        let watched = unsafe { &*(data as *const Watched) };
        publish(&watched.extent, hwnd, watched.read);
    }
    // SAFETY: the arguments this procedure was called with.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, SendMessageW, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_DISPLAYCHANGE,
    };
    use windows::core::w;
    use winit::dpi::PhysicalPosition;
    use winit::event::WindowEvent;

    use super::{RateFeed, Source, Watch, rational_mhz, rereads};
    use crate::render_side::Extent;

    /// A hidden window of this thread.
    fn hidden_window() -> HWND {
        // SAFETY: a predefined class, no parent, no creation data.
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap()
    }

    /// C3d: the active mode's rational rate, whole before the division;
    /// a zero denominator is no rate.
    #[test]
    fn the_active_rate_is_rational() {
        assert_eq!(rational_mhz(143_856, 1_000), Some(143_856));
        assert_eq!(rational_mhz(60_000, 1_001), Some(59_940));
        assert_eq!(rational_mhz(240_085_000, 1_000_000), Some(240_085));
        assert_eq!(rational_mhz(60, 0), None);
        assert_eq!(rational_mhz(u32::MAX, 1), None);
    }

    /// The rates `moves`' read gives, in turn.
    static MOVES: AtomicU32 = AtomicU32::new(0);

    fn moves(_hwnd: HWND) -> (u32, Source) {
        let rate = match MOVES.fetch_add(1, Ordering::SeqCst) {
            0 => 60_000,
            1 => 143_856,
            _ => 240_085,
        };
        (rate, Source::DisplayConfig)
    }

    /// C3c: the window's feed publishes the rate read at its start, and
    /// again at a move and at a scale change (which comes with a move
    /// across monitors), each through the event handler the window calls;
    /// a resize reads nothing. winit makes no scale-change event outside
    /// itself, so its rereading is driven through the same handler's
    /// rereading.
    #[test]
    fn a_move_publishes_the_rate() {
        let hwnd = hidden_window();
        let extent = Arc::new(Extent::default());
        let feed = RateFeed::start(hwnd, &extent, moves);
        assert!(feed.watching());
        assert_eq!(extent.rate_mhz(), 60_000, "read at the start");
        feed.window_event(
            &extent,
            &WindowEvent::Moved(PhysicalPosition::new(1_920, 0)),
        );
        assert_eq!(extent.rate_mhz(), 143_856, "read at a move");
        feed.window_event(
            &extent,
            &WindowEvent::Resized(winit::dpi::PhysicalSize::new(1_280, 720)),
        );
        assert_eq!(extent.rate_mhz(), 143_856, "a resize reads nothing");
        feed.reread(&extent);
        assert_eq!(extent.rate_mhz(), 240_085, "read at a scale change");
        assert!(!rereads(&WindowEvent::CloseRequested));
        drop(feed);
        // SAFETY: made above, on this thread.
        unsafe { DestroyWindow(hwnd) }.unwrap();
    }

    fn at_240(_hwnd: HWND) -> (u32, Source) {
        (240_085, Source::DisplayConfig)
    }

    /// M1: a window whose subclass cannot be installed (no window at all
    /// here, which `SetWindowSubclass` refuses) publishes the rate as
    /// unknown, over one known before, and keeps it unknown through a
    /// move: with no observer of a display change, no stale rate is
    /// advertised.
    #[test]
    fn a_failed_watch_leaves_the_rate_unknown() {
        let extent = Arc::new(Extent::default());
        extent.publish_rate(60_000);
        let feed = RateFeed::start(HWND::default(), &extent, at_240);
        assert!(!feed.watching());
        assert_eq!(extent.rate_mhz(), 0, "unknown at the start");
        feed.window_event(&extent, &WindowEvent::Moved(PhysicalPosition::new(0, 0)));
        feed.reread(&extent);
        assert_eq!(extent.rate_mhz(), 0, "still unknown after a move");
    }

    /// The rates the test's read gives, in turn.
    static READS: AtomicU32 = AtomicU32::new(0);

    fn rates(_hwnd: HWND) -> (u32, Source) {
        let rate = match READS.fetch_add(1, Ordering::SeqCst) {
            0 => 143_856,
            _ => 240_085,
        };
        (rate, Source::DisplayConfig)
    }

    /// C3b: a hidden window of this thread, subclassed, publishes the
    /// rate it reads at each `WM_DISPLAYCHANGE` sent to it, the size
    /// untouched; once the subclass is gone, a message publishes nothing.
    #[test]
    fn a_sent_display_change_publishes_the_rate() {
        let hwnd = hidden_window();
        let extent = Arc::new(Extent::default());
        let watch = Watch::install(hwnd, Arc::clone(&extent), rates).unwrap();
        assert_eq!(extent.rate_mhz(), 0);
        let send = || {
            // SAFETY: the window is this thread's and open.
            unsafe { SendMessageW(hwnd, WM_DISPLAYCHANGE, Some(WPARAM(32)), Some(LPARAM(0))) };
        };
        send();
        assert_eq!(extent.rate_mhz(), 143_856);
        send();
        assert_eq!(extent.rate_mhz(), 240_085);
        assert_eq!(extent.latest(), None, "no size published");
        drop(watch);
        extent.publish_rate(0);
        send();
        assert_eq!(extent.rate_mhz(), 0, "published after the subclass went");
        // SAFETY: made above, on this thread.
        unsafe { DestroyWindow(hwnd) }.unwrap();
    }
}
