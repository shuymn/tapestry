//! The handles the game hands the worker and the Windows calls on them: a
//! file read through its handle, a frame ring's section mapped, an event
//! waited on. The worker owns each handle from the command that names it
//! and closes it with its player.

use std::sync::atomic::AtomicU64;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, UnmapViewOfFile, VirtualQuery,
};
use windows::Win32::System::Threading::WaitForSingleObject;

/// The handle a command's `value` names.
fn handle(value: u64) -> HANDLE {
    HANDLE(usize::try_from(value).unwrap_or(0) as *mut core::ffi::c_void)
}

/// A handle the worker holds, closed when dropped.
pub struct Owned(HANDLE);

impl Owned {
    /// The handle `value` names, the worker's to close from now on.
    pub fn adopt(value: u64) -> Self {
        Self(handle(value))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the worker holds the handle (it adopted it from the
            // game's command) and nothing uses it after this.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// The file behind `value`, a read-only file handle the game handed the
/// worker: read through it, and closed with it.
pub fn file(value: u64) -> std::fs::File {
    use std::os::windows::io::FromRawHandle as _;
    // SAFETY: the game hands the worker this file handle for one player's
    // video ("Threat Model" T02) and the worker owns it from the command
    // on: nothing else closes it.
    unsafe { std::fs::File::from_raw_handle(handle(value).0) }
}

/// A frame ring's section mapped into the worker, read and write.
pub struct View {
    base: MEMORY_MAPPED_VIEW_ADDRESS,
    words: usize,
    _section: Owned,
}

impl View {
    /// Maps the whole of `section`.
    ///
    /// # Errors
    ///
    /// When the section cannot be mapped.
    pub fn map(section: Owned) -> std::io::Result<Self> {
        // SAFETY: a section handle the game handed the worker, mapped whole
        // for reading and writing; the view lives until `drop`.
        let base = unsafe { MapViewOfFile(section.0, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0) };
        if base.Value.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `base` starts the view just mapped, and `info` is a buffer
        // of the size given.
        let n = unsafe {
            VirtualQuery(
                Some(base.Value),
                &raw mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        let view = Self {
            base,
            words: info.RegionSize / 8,
            _section: section,
        };
        if n == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(view)
    }

    /// The view's words, which the game reads and writes too: every access
    /// goes through the atomics.
    pub fn words(&self) -> &[AtomicU64] {
        // SAFETY: the view stays mapped while `self` lives, starts on a
        // page boundary (so is aligned for u64) and holds `words` words;
        // the memory is shared with the game, and every access to it goes
        // through `AtomicU64`.
        unsafe { std::slice::from_raw_parts(self.base.Value.cast::<AtomicU64>(), self.words) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: the view this value mapped, unused after this.
        let _ = unsafe { UnmapViewOfFile(self.base) };
    }
}

/// The event the game signals when it frees a slot of a ring.
pub struct Event(Owned);

impl Event {
    /// The event handle `value` names, the worker's from now on.
    pub fn adopt(value: u64) -> Self {
        Self(Owned::adopt(value))
    }

    /// Waits up to `ms` milliseconds for the event: whether it came.
    pub fn wait(&self, ms: u32) -> bool {
        // SAFETY: an event handle the worker holds.
        unsafe { WaitForSingleObject(self.0.0, ms) == WAIT_OBJECT_0 }
    }
}
