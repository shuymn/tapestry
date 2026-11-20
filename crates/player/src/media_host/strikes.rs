//! Strikes (design.md "Threat Model" T02): a file open in a worker that
//! ended without the game asking (a crash, or the memory cap) gets a
//! strike, and a file with two is not opened again in that run of the
//! game. A file is known by its identity on its volume, which every path
//! and handle to it share.

use std::collections::BTreeMap;
use std::os::windows::io::AsRawHandle as _;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx};

/// The strikes that keep a file from being opened again.
pub const LIMIT: u8 = 2;

/// A file's identity: its volume's serial number and its 128-bit ID there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId {
    volume: u64,
    id: [u8; 16],
}

impl FileId {
    /// The identity of the file `file` is open on.
    ///
    /// # Errors
    ///
    /// When the system does not give it.
    pub fn of(file: &std::fs::File) -> windows::core::Result<Self> {
        let mut info = FILE_ID_INFO::default();
        // SAFETY: an open file's handle, and a struct of the size given.
        unsafe {
            GetFileInformationByHandleEx(
                HANDLE(file.as_raw_handle()),
                FileIdInfo,
                (&raw mut info).cast(),
                u32::try_from(size_of_val(&info)).unwrap_or(0),
            )
        }?;
        Ok(Self {
            volume: info.VolumeSerialNumber,
            id: info.FileId.Identifier,
        })
    }
}

/// The strikes of one run of the game.
#[derive(Debug, Default)]
pub struct Strikes(BTreeMap<FileId, u8>);

impl Strikes {
    /// Whether `file` has the strikes that keep it from being opened.
    pub fn out(&self, file: FileId) -> bool {
        self.0.get(&file).is_some_and(|&n| n >= LIMIT)
    }

    /// Gives `file` a strike.
    pub fn strike(&mut self, file: FileId) {
        let n = self.0.entry(file).or_insert(0);
        *n = n.saturating_add(1);
    }

    /// The strikes `file` has.
    #[cfg(test)]
    pub fn of(&self, file: FileId) -> u8 {
        self.0.get(&file).copied().unwrap_or(0)
    }
}
