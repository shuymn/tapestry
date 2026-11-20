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
//!
//! Also the one Windows call on FFmpeg's DLLs: pointing an import of theirs
//! at a function of the worker's.

use std::collections::BTreeSet;
use std::ffi::{CStr, c_void};
use std::io::Read;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use skin_model::media::control::{EVENT_ACCESS, FILE_ACCESS, SECTION_ACCESS};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BUSY, ERROR_INVALID_HANDLE, ERROR_NOT_SUPPORTED,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::GetFileSizeEx;
use windows::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEM_IMAGE, MEMORY_BASIC_INFORMATION,
    MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_PROTECTION_FLAGS, PAGE_READWRITE,
    UnmapViewOfFile, VirtualProtect, VirtualQuery,
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

/// The size of `file`, asked of its handle. A read-only handle that may
/// read the file's data and nothing else gives it too: `File::metadata`
/// would also read the file's attributes, which Windows refuses that
/// handle.
pub fn file_size(file: &std::fs::File) -> Option<u64> {
    use std::os::windows::io::AsRawHandle as _;
    let mut size = 0i64;
    // SAFETY: the file's own handle, open while `file` lives, and the size
    // the call writes.
    unsafe { GetFileSizeEx(HANDLE(file.as_raw_handle()), &raw mut size) }.ok()?;
    u64::try_from(size).ok()
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

    /// The file it reads, lent to move in or to ask its size: only the
    /// source closes it.
    pub fn file(&self) -> &std::fs::File {
        &self.file
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

/// What an import table that does not read as a loaded 64-bit image's is
/// reported as.
const UNREADABLE: &str = "an image whose headers or imports do not read";

/// A loaded module, read where the loader mapped it.
struct Image {
    base: *mut u8,
    /// The image's size in memory: every read stays below it.
    size: usize,
    /// Where its import directory lies, and its length.
    imports: usize,
    imports_len: usize,
}

impl Image {
    /// The image holding `inside`, an address in a loaded module, when its
    /// headers are those of a 64-bit x86 image.
    fn holding(inside: *const c_void) -> Option<Self> {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: a query on an address; `info` is a buffer of the size
        // given.
        let n = unsafe {
            VirtualQuery(
                Some(inside),
                &raw mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if n == 0 || info.Type != MEM_IMAGE || info.AllocationBase.is_null() {
            return None;
        }
        // The headers lie in the image's first page, which the loader maps
        // readable; the reads of them stay inside it.
        let mut image = Self {
            base: info.AllocationBase.cast(),
            size: 4096,
            imports: 0,
            imports_len: 0,
        };
        // "MZ", then the NT headers' "PE\0\0", the x86-64 machine and the
        // PE32+ optional header.
        if image.read::<u16>(0)? != 0x5a4d {
            return None;
        }
        let nt = usize::try_from(image.read::<u32>(0x3c)?).ok()?;
        if image.read::<u32>(nt)? != 0x4550 || image.read::<u16>(nt + 4)? != 0x8664 {
            return None;
        }
        let optional = nt + 24;
        if image.read::<u16>(optional)? != 0x20b || image.read::<u32>(optional + 108)? < 2 {
            return None;
        }
        // The import directory is the second of the data directories.
        image.imports = usize::try_from(image.read::<u32>(optional + 120)?).ok()?;
        image.imports_len = usize::try_from(image.read::<u32>(optional + 124)?).ok()?;
        image.size = usize::try_from(image.read::<u32>(optional + 56)?).ok()?;
        Some(image)
    }

    /// The `T` at `at` bytes into the image, when it lies inside.
    fn read<T: Copy>(&self, at: usize) -> Option<T> {
        let end = at.checked_add(size_of::<T>())?;
        if end > self.size {
            return None;
        }
        // SAFETY: inside the image the loader mapped, `size` bytes from
        // `base`; read without assuming alignment.
        Some(unsafe { self.base.add(at).cast::<T>().read_unaligned() })
    }

    /// Whether the NUL-terminated name at `at` is `name`.
    fn names(&self, at: usize, name: &CStr) -> Option<bool> {
        for (k, &byte) in name.to_bytes_with_nul().iter().enumerate() {
            if self.read::<u8>(at.checked_add(k)?)? != byte {
                return Some(false);
            }
        }
        Some(true)
    }

    /// Where the module calls each of its imports of the function `name`
    /// through (imports by name; one by ordinal names nothing), and the
    /// address those slots hold.
    fn slots(&self, name: &CStr) -> Result<(Vec<usize>, Option<u64>), &'static str> {
        let mut slots = Vec::new();
        let mut held = None;
        for d in 0..self.imports_len / 20 {
            let at = self.imports + 20 * d;
            let names = self.read::<u32>(at).ok_or(UNREADABLE)?;
            let module = self.read::<u32>(at + 12).ok_or(UNREADABLE)?;
            let calls = self.read::<u32>(at + 16).ok_or(UNREADABLE)?;
            if module == 0 && calls == 0 {
                break;
            }
            if names == 0 {
                return Err("an import table without names");
            }
            let (names, calls) = (names as usize, calls as usize);
            for k in 0.. {
                let entry = self.read::<u64>(names + 8 * k).ok_or(UNREADABLE)?;
                if entry == 0 {
                    break;
                }
                if entry >> 63 == 1 {
                    continue;
                }
                // The entry names a hint and the name after it.
                let hint = usize::try_from(entry & 0x7fff_ffff).map_err(|_| UNREADABLE)?;
                if !self.names(hint + 2, name).ok_or(UNREADABLE)? {
                    continue;
                }
                let slot = calls + 8 * k;
                if slot % 8 != 0 {
                    return Err("an import slot out of alignment");
                }
                let address = self.read::<u64>(slot).ok_or(UNREADABLE)?;
                if held.is_some_and(|h| h != address) {
                    return Err("imports of one name that hold different addresses");
                }
                held = Some(address);
                slots.push(slot);
            }
        }
        Ok((slots, held))
    }
}

/// Whether the module holding `inside` imports a function named `name`.
///
/// # Errors
///
/// When the module's headers or imports do not read as a loaded 64-bit
/// image's, or its imports of `name` hold different addresses.
pub fn imports(inside: *const c_void, name: &CStr) -> Result<bool, &'static str> {
    let image = Image::holding(inside).ok_or(UNREADABLE)?;
    Ok(!image.slots(name)?.0.is_empty())
}

/// Points every import of the function `name` by the module holding
/// `inside` at `to`, and gives the address those imports held, the same in
/// each: `None` when the module imports no function of that name.
///
/// # Errors
///
/// When the module's headers or imports do not read as a loaded 64-bit
/// image's, its imports of `name` hold different addresses, or one cannot
/// be made writable.
///
/// # Safety
///
/// No other thread runs the module's code while this runs, and `to` is a
/// function of the type of the one `name` names.
pub unsafe fn redirect_import(
    inside: *const c_void,
    name: &CStr,
    to: *const c_void,
) -> Result<Option<usize>, &'static str> {
    let image = Image::holding(inside).ok_or(UNREADABLE)?;
    let (slots, held) = image.slots(name)?;
    for slot in slots {
        // SAFETY: a slot of the image's import table, inside the image
        // (`slots` read it); made writable for the write and given back its
        // protection, while no other thread calls through it.
        unsafe {
            let at = image.base.add(slot);
            let mut was = PAGE_PROTECTION_FLAGS::default();
            VirtualProtect(at.cast(), 8, PAGE_READWRITE, &raw mut was)
                .map_err(|_| "an import that cannot be made writable")?;
            (*at.cast::<AtomicU64>()).store(to.addr() as u64, Ordering::SeqCst);
            let mut ours = PAGE_PROTECTION_FLAGS::default();
            VirtualProtect(at.cast(), 8, was, &raw mut ours)
                .map_err(|_| "an import whose protection cannot be given back")?;
        }
    }
    Ok(held.map(|h| h as usize))
}

#[cfg(test)]
mod tests {
    use std::io::{Seek as _, Write as _};
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};

    use windows::Win32::Foundation::{DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::FILE_READ_DATA;
    use windows::Win32::System::Threading::GetCurrentProcess;

    #[test]
    fn a_file_size_is_read_through_a_handle_that_reads_data_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("video");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&[7; 5_000])
            .unwrap();
        let opened = std::fs::File::open(&path).unwrap();
        let mut reading = HANDLE::default();
        // SAFETY: a handle this test opened, duplicated within its own
        // process to read the file's data and nothing else.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                HANDLE(opened.as_raw_handle()),
                GetCurrentProcess(),
                &raw mut reading,
                FILE_READ_DATA.0,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
        }
        .unwrap();
        // SAFETY: the duplicate, which the file owns from here.
        let mut file = unsafe { std::fs::File::from_raw_handle(reading.0) };
        // The control: the file's metadata needs a right the handle lacks.
        assert!(file.metadata().is_err());
        assert_eq!(super::file_size(&file), Some(5_000));
        assert_eq!(file.stream_position().unwrap(), 0);
    }
}
