//! The handles the game hands the worker and the Windows calls on them: a
//! file read through its handle, a frame ring's section mapped, an event
//! waited on (design.md "Threat Model" T02).
//!
//! The game duplicates each handle into the worker with exactly the access
//! the control protocol names for its kind and sends its value. The worker
//! takes a value ([`Owned::take`]) only when its own handle table holds a
//! handle of that value, of that kind and with exactly that access, that no
//! player holds already: another player's value, a closed player's or any
//! other is refused and left alone, since under the worker's strict handle
//! checks using a value that is no handle ends the process. A taken handle
//! is closed with its player, and its value can be taken again once it is
//! closed.

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, OnceLock, PoisonError};

use skin_model::media::control::{EVENT_ACCESS, FILE_ACCESS, SECTION_ACCESS};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BUSY, ERROR_INVALID_HANDLE, ERROR_NOT_SUPPORTED,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION,
    MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcess, WaitForSingleObject};
use windows::core::PCWSTR;

use crate::handles;

/// The handle a command's `value` names.
fn handle(value: u64) -> HANDLE {
    HANDLE(usize::try_from(value).unwrap_or(0) as *mut core::ffi::c_void)
}

/// A kind of handle a command names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A video's file, read.
    File,
    /// A frame ring's section, mapped for reading and writing.
    Section,
    /// A frame ring's free-slot event, waited on.
    Event,
}

impl Kind {
    /// The access the game duplicates a handle of this kind with.
    const fn access(self) -> u32 {
        match self {
            Self::File => FILE_ACCESS,
            Self::Section => SECTION_ACCESS,
            Self::Event => EVENT_ACCESS,
        }
    }
}

/// The system's type index of each kind.
#[derive(Clone, Copy, Debug)]
struct Types {
    file: u32,
    section: u32,
    event: u32,
}

impl Types {
    const fn of(self, kind: Kind) -> u32 {
        match kind {
            Kind::File => self.file,
            Kind::Section => self.section,
            Kind::Event => self.event,
        }
    }
}

/// The types [`learn`] found, or `None` when it could not: then every value
/// is refused.
static TYPES: OnceLock<Option<Types>> = OnceLock::new();

/// The values of the handles the players hold.
static HELD: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());

/// A handle the worker made and closes when dropped.
struct Made(HANDLE);

impl Drop for Made {
    fn drop(&mut self) {
        // SAFETY: a handle this value made, unused after this.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Learns the system's type of a file, a section and an event before the
/// first command: its standard input's (the control pipe, a file object),
/// and an event's and a section's it makes and closes.
///
/// # Errors
///
/// What failed; the worker then refuses every handle a command names.
pub fn learn() -> Result<(), String> {
    let types = learn_types();
    let _ = TYPES.set(types.as_ref().ok().copied());
    types.map(|_| ())
}

/// `what` failed with `e`'s code. Only the code: formatting a `windows`
/// error reads its message through OLE Automation (`oleaut32.dll`), which
/// the worker does not import (docs/setup.md "FFmpeg", `check-exes`).
fn failed(what: &str) -> impl FnOnce(windows::core::Error) -> String + '_ {
    move |e| format!("{what}: error {:#010x}", e.code().0)
}

fn learn_types() -> Result<Types, String> {
    // SAFETY: an unnamed auto-reset event, closed when `event` drops.
    let event = Made(
        unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.map_err(failed("an event"))?,
    );
    // SAFETY: an unnamed section of one page backed by the paging file,
    // closed when `section` drops.
    let section = Made(
        unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                4096,
                PCWSTR::null(),
            )
        }
        .map_err(failed("a section"))?,
    );
    // SAFETY: a plain query.
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.map_err(failed("standard input"))?;
    // SAFETY: the pseudo handle of the worker's own process.
    let table = handles::table(unsafe { GetCurrentProcess() })
        .map_err(|s| format!("the handle table: {:#010x}", s.0))?;
    let index = |h: HANDLE, what: &str| {
        table
            .iter()
            .find(|e| e.value == h.0 as u64)
            .map(|e| e.type_index)
            .ok_or_else(|| format!("{what} is not in the handle table"))
    };
    Ok(Types {
        file: index(input, "standard input")?,
        section: index(section.0, "the section")?,
        event: index(event.0, "the event")?,
    })
}

/// A handle a command named that the worker took, closed when dropped.
pub struct Owned(HANDLE);

// SAFETY: a handle is a value any thread of the process may use; the one
// `Owned` holds is used by one thread at a time, the one that owns it.
unsafe impl Send for Owned {}

impl Owned {
    /// Takes the handle `value` names as one of `kind`.
    ///
    /// # Errors
    ///
    /// The system error that says why not, the value left alone:
    /// `ERROR_BUSY` when a player holds it, `ERROR_INVALID_HANDLE` when the
    /// worker holds no handle of that value and kind, `ERROR_ACCESS_DENIED`
    /// when its access is not exactly the kind's, `ERROR_NOT_SUPPORTED`
    /// when [`learn`] failed.
    pub fn take(value: u64, kind: Kind) -> Result<Self, u32> {
        let Some(types) = TYPES.get().copied().flatten() else {
            return Err(ERROR_NOT_SUPPORTED.0);
        };
        let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
        if held.contains(&value) {
            return Err(ERROR_BUSY.0);
        }
        // SAFETY: the pseudo handle of the worker's own process.
        let table =
            handles::table(unsafe { GetCurrentProcess() }).map_err(|_| ERROR_INVALID_HANDLE.0)?;
        let entry = table
            .iter()
            .find(|e| e.value == value && e.type_index == types.of(kind))
            .ok_or(ERROR_INVALID_HANDLE.0)?;
        if entry.access != kind.access() {
            return Err(ERROR_ACCESS_DENIED.0);
        }
        held.insert(value);
        Ok(Self(handle(value)))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: a handle the worker took, which nothing uses after this;
        // its value stays held until it is closed, so no command takes it
        // meanwhile.
        let _ = unsafe { CloseHandle(self.0) };
        held.remove(&(self.0.0 as u64));
    }
}

/// The file an input reads: a file handle the worker took, or, for a
/// dev-tools command, a file it opened.
pub struct Source {
    file: ManuallyDrop<std::fs::File>,
    taken: Option<Owned>,
}

impl Source {
    /// Reads through the file handle the worker took, closed with the
    /// source.
    pub fn taken(owned: Owned) -> Self {
        use std::os::windows::io::FromRawHandle as _;
        // SAFETY: a file handle the worker took; the `File` never closes it
        // (it stays in `ManuallyDrop`), `taken` does.
        let file = unsafe { std::fs::File::from_raw_handle(owned.0.0) };
        Self {
            file: ManuallyDrop::new(file),
            taken: Some(owned),
        }
    }

    /// Reads `file`, closed with the source.
    #[cfg(feature = "dev-tools")]
    pub fn opened(file: std::fs::File) -> Self {
        Self {
            file: ManuallyDrop::new(file),
            taken: None,
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if self.taken.is_none() {
            // SAFETY: an opened file, dropped once, here.
            unsafe { ManuallyDrop::drop(&mut self.file) };
        }
    }
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&*self.file).read(buf)
    }
}

impl Seek for Source {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        (&*self.file).seek(to)
    }
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
        // SAFETY: a section handle the worker took, mapped whole for reading
        // and writing; the view lives until `drop`.
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
    /// Waits on the event handle the worker took.
    pub const fn new(owned: Owned) -> Self {
        Self(owned)
    }

    /// Waits up to `ms` milliseconds for the event: whether it came.
    pub fn wait(&self, ms: u32) -> bool {
        // SAFETY: an event handle the worker took.
        unsafe { WaitForSingleObject(self.0.0, ms) == WAIT_OBJECT_0 }
    }
}
