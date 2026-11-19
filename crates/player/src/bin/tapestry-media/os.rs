//! The handles the game hands the worker and the Windows calls on them: a
//! file read through its handle, a frame ring's section mapped, an event
//! waited on. The worker owns each handle from the command that names it
//! and closes it with its player. Also the one Windows call on FFmpeg's
//! DLLs: pointing an import of theirs at a function of the worker's.

use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MEM_IMAGE, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, PAGE_PROTECTION_FLAGS, PAGE_READWRITE, UnmapViewOfFile, VirtualProtect,
    VirtualQuery,
};
use windows::Win32::System::Threading::WaitForSingleObject;

/// The handle a command's `value` names.
fn handle(value: u64) -> HANDLE {
    HANDLE(usize::try_from(value).unwrap_or(0) as *mut core::ffi::c_void)
}

/// A handle the worker holds, closed when dropped.
pub struct Owned(HANDLE);

// SAFETY: a handle is valid on any of the process's threads, and the one
// thread an `Owned` moves to is the only one that uses or closes it.
unsafe impl Send for Owned {}

impl Owned {
    /// The handle `value` names, the worker's to close from now on.
    pub fn adopt(value: u64) -> Self {
        Self(handle(value))
    }

    /// The file this handle is, a read-only file handle the game handed the
    /// worker for one player's video: read through it, and closed with it.
    pub fn into_file(self) -> std::fs::File {
        use std::os::windows::io::FromRawHandle as _;
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: the worker holds the handle, a file handle the game handed
        // it ("Threat Model" T02); the file owns it from here, and nothing
        // else closes it.
        unsafe { std::fs::File::from_raw_handle(this.0.0) }
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
