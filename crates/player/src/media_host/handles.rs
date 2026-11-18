//! A process's handle table (design.md "Threat Model" T02): the launch
//! check reads the suspended media worker's, and the worker reads its own
//! before it takes a handle a command names. Each handle comes with its
//! value, the access it was granted and its object's type as the system
//! indexes types, so a caller compares the type with that of a handle whose
//! type it knows.
//!
//! The query is `NtQueryInformationProcess(ProcessHandleInformation)`, the
//! one the system's process snapshots make (`PssCaptureSnapshot` with
//! `PSS_CAPTURE_HANDLES`); the snapshot API would also query each handle's
//! names, which can block on a pipe, where this reads only the table.

use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessHandleInformation};
use windows::Win32::Foundation::{HANDLE, NTSTATUS, STATUS_INFO_LENGTH_MISMATCH};

/// One handle of a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its value in the process.
    pub value: u64,
    /// The access it was granted.
    pub access: u32,
    /// Its object's type, as the system indexes types.
    pub type_index: u32,
}

/// `PROCESS_HANDLE_TABLE_ENTRY_INFO`, as the system writes it; three of its
/// fields are read.
#[repr(C)]
#[derive(Clone, Copy)]
struct Raw {
    value: usize,
    handle_count: usize,
    pointer_count: usize,
    granted_access: u32,
    object_type_index: u32,
    attributes: u32,
    reserved: u32,
}

/// The largest table read, in bytes: far more handles than the worker or
/// the game holds.
const MAX_BYTES: usize = 16 << 20;

/// The handles `process` holds. `process` needs
/// `PROCESS_QUERY_INFORMATION`.
///
/// # Errors
///
/// The query's status when it fails.
pub fn table(process: HANDLE) -> Result<Vec<Entry>, NTSTATUS> {
    // `PROCESS_HANDLE_SNAPSHOT_INFORMATION`: the handle count and a reserved
    // word, then the entries.
    let header = 2 * size_of::<usize>();
    let mut bytes = 32 << 10;
    loop {
        // Words of 8 bytes, so every entry is aligned.
        let mut buf: Vec<u64> = vec![0; bytes / 8];
        let mut returned = 0u32;
        // SAFETY: `buf` holds `bytes` bytes, which the query fills, and
        // `returned` is a plain out-parameter.
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                ProcessHandleInformation,
                buf.as_mut_ptr().cast(),
                u32::try_from(bytes).unwrap_or(u32::MAX),
                &raw mut returned,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH && bytes < MAX_BYTES {
            let needed = usize::try_from(returned).unwrap_or(MAX_BYTES);
            bytes = needed.next_multiple_of(8).max(bytes * 2).min(MAX_BYTES);
            continue;
        }
        if status.is_err() {
            return Err(status);
        }
        let filled = usize::try_from(returned).unwrap_or(0).min(bytes);
        let count = buf
            .first()
            .and_then(|&n| usize::try_from(n).ok())
            .unwrap_or(0)
            .min(filled.saturating_sub(header) / size_of::<Raw>());
        let base = buf.as_ptr().cast::<u8>();
        let entries = (0..count)
            .map(|i| {
                // SAFETY: entry `i` lies inside the bytes the query filled
                // (`count` entries fit after the header), at a multiple of 8
                // from the start of `buf`, so aligned for `Raw`.
                let raw = unsafe { base.add(header + i * size_of::<Raw>()).cast::<Raw>().read() };
                Entry {
                    value: raw.value as u64,
                    access: raw.granted_access,
                    type_index: raw.object_type_index,
                }
            })
            .collect();
        return Ok(entries);
    }
}
