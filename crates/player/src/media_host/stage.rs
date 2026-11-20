//! The worker's copies (design.md "Threat Model" T02): the game launches
//! the worker from copies it makes of its files, never from the
//! installation directory, on which it grants nothing. Before each launch
//! [`stage`] reads the worker's executable and the build's FFmpeg DLLs in
//! the installation and copies them, byte for byte, into a directory of
//! that build's own under the game's data folder: `media\` and 32 hex
//! digits of the SHA-256 of their names and bytes, so builds never share
//! one. The directory is created with a DACL protected from inheritance,
//! granting the user and SYSTEM full access and the worker's AppContainer
//! read and execute, none of them inherited by what is put in it; each
//! copy is created with the same DACL as its own. The launch writes no
//! DACL on anything that exists, so nothing propagates an ACE, and no grant
//! reaches a file that another name links to.
//!
//! A directory whose DACL differs, or that is a link, refuses the launch.
//! A copy is kept when it holds the installation's bytes, exactly that DACL
//! and one name; any other is written again, as a new file moved over it,
//! and a copy that cannot be (a worker of that build still runs from it)
//! refuses the launch. Each launch reads the installation's files and the
//! copies whole to compare them.
//!
//! The copies and their directory stay open from that check until the
//! launch drops the [`Staged`] value, after the worker has loaded them. The
//! copies, opened to read their data and sharing neither writing nor
//! deletion, cannot be written, renamed or deleted meanwhile; the directory,
//! which shares writing so a new copy can be moved into it, but not
//! deletion, cannot be renamed or deleted, nor can the folders above it be
//! renamed. The system's loader, which only reads the copies, opens them all
//! the same.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use sha2::{Digest as _, Sha256};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GENERIC_WRITE, HANDLE,
};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_MODE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetFileInformationByHandle, MOVEFILE_REPLACE_EXISTING, MoveFileExW,
    READ_CONTROL,
};
use windows::core::{PCWSTR, PWSTR};

use super::launch::{
    DIRECTORY_ACCESS, EXECUTABLE, Failure, Local, code_of, failed, user_sid, wide,
};

/// The FFmpeg DLLs of the build, without `.dll`: those ffmpeg.toml lists,
/// which the build script copies beside the binaries.
const DLLS: &str = env!("TAPESTRY_FFMPEG_DLLS");

/// The worker's files, as the installation holds them beside the game:
/// its executable and the build's DLLs.
pub fn files() -> impl Iterator<Item = String> {
    std::iter::once(EXECUTABLE.to_owned()).chain(DLLS.split(',').map(|d| format!("{d}.dll")))
}

/// The worker's copies, staged for one launch: held open until dropped.
pub struct Staged {
    /// The copies' directory.
    path: PathBuf,
    /// The directory, held so it cannot be renamed or deleted.
    _dir: File,
    /// The copies, held so they cannot be written, renamed or deleted.
    _copies: Vec<File>,
}

impl Staged {
    /// The directory the worker is launched from.
    pub fn dir(&self) -> &Path {
        &self.path
    }
}

/// Stages the worker's files in `installation` into the game's data folder
/// `data` for the AppContainer `container` (the module's doc).
///
/// # Errors
///
/// When a file cannot be read or written, the directory differs, or a copy
/// that differs cannot be written again.
pub fn stage(installation: &Path, data: &Path, container: PSID) -> Result<Staged, Failure> {
    let mut digest = Sha256::new();
    let mut sources = Vec::new();
    for name in files() {
        let path = installation.join(&name);
        let bytes = std::fs::read(&path).map_err(io_failure(&path))?;
        digest.update(u64::try_from(name.len()).unwrap_or(u64::MAX).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
        digest.update(&bytes);
        sources.push((name, bytes));
    }
    let hex: String = digest
        .finalize()
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect();
    let media = data.join("media");
    std::fs::create_dir_all(&media).map_err(io_failure(&media))?;
    let path = media.join(hex);
    let dacl = Dacl::new(container)?;
    let dir = directory(&path, &dacl)?;
    let mut copies = Vec::new();
    for (name, bytes) in &sources {
        copies.push(copy(&path.join(name), bytes, &dacl)?);
    }
    Ok(Staged {
        path,
        _dir: dir,
        _copies: copies,
    })
}

/// A failure of an I/O call on `path`.
fn io_failure(path: &Path) -> impl FnOnce(std::io::Error) -> Failure + '_ {
    move |e| {
        let code = e.raw_os_error().and_then(|c| u32::try_from(c).ok());
        (code.unwrap_or(0), path.display().to_string())
    }
}

/// The DACL of the copies and their directory: as a descriptor to create
/// them with, and as SDDL, to compare a found one with.
struct Dacl {
    descriptor: PSECURITY_DESCRIPTOR,
    sddl: String,
    _descriptor: Local,
}

impl Dacl {
    /// Protected from inheritance: the user and SYSTEM full access,
    /// `container` read and execute, none of it inherited.
    fn new(container: PSID) -> Result<Self, Failure> {
        let text = format!(
            "D:P(A;;FA;;;{})(A;;FA;;;SY)(A;;0x{DIRECTORY_ACCESS:x};;;{})",
            user_sid()?,
            sid_text(container)?
        );
        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a nul-terminated SDDL string; the descriptor returned is
        // freed by `Local`.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        }
        .map_err(failed("the copies' DACL"))?;
        let owned = Local(descriptor.0);
        Ok(Self {
            descriptor,
            sddl: sddl(descriptor)?,
            _descriptor: owned,
        })
    }

    /// Attributes creating an object with this DACL.
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: self.descriptor.0,
            bInheritHandle: false.into(),
        }
    }

    /// Whether the file or directory `file` has exactly this DACL.
    fn holds(&self, file: &File) -> Result<bool, Failure> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: an open handle with `READ_CONTROL`; the descriptor
        // returned is freed by `Local`.
        let read = unsafe {
            GetSecurityInfo(
                HANDLE(file.as_raw_handle()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                Some(&raw mut descriptor),
            )
        };
        let _descriptor = Local(descriptor.0);
        if read.is_err() {
            return Err((read.0, "a copy's DACL".to_owned()));
        }
        Ok(sddl(descriptor)? == self.sddl)
    }
}

/// The DACL of `descriptor` as SDDL, its protection included.
fn sddl(descriptor: PSECURITY_DESCRIPTOR) -> Result<String, Failure> {
    let mut text = PWSTR::null();
    // SAFETY: a valid descriptor; the string returned is freed by `Local`.
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &raw mut text,
            None,
        )
    }
    .map_err(failed("a DACL as text"))?;
    let _text = Local(text.0.cast());
    // SAFETY: the nul-terminated string the call returned.
    unsafe { text.to_string() }.map_err(|_| (0, "a DACL as text".to_owned()))
}

/// `sid` as text.
fn sid_text(sid: PSID) -> Result<String, Failure> {
    let mut text = PWSTR::null();
    // SAFETY: a valid SID; the string returned is freed by `Local`.
    unsafe { ConvertSidToStringSidW(sid, &raw mut text) }
        .map_err(failed("the AppContainer's SID as text"))?;
    let _text = Local(text.0.cast());
    // SAFETY: the nul-terminated string the call returned.
    unsafe { text.to_string() }.map_err(|_| (0, "the AppContainer's SID as text".to_owned()))
}

/// Opens `path` itself, not what a link there names, to read its data and
/// DACL, sharing reading alone (`share` adds to it).
fn open_held(path: &Path, data: u32, share: FILE_SHARE_MODE) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .access_mode(data | READ_CONTROL.0 | FILE_READ_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | share).0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
}

/// `file`'s attributes and link count.
fn information(file: &File) -> Result<BY_HANDLE_FILE_INFORMATION, Failure> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: an open handle, and a struct the call fills.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &raw mut info) }
        .map_err(failed("a copy's information"))?;
    Ok(info)
}

/// The copies' directory at `path`, made with `dacl` or found with exactly
/// it, and held. Others may add files to it (the launch's own new copies
/// among them), but not rename or delete it.
fn directory(path: &Path, dacl: &Dacl) -> Result<File, Failure> {
    let attributes = dacl.attributes();
    let name = wide(path);
    // SAFETY: a nul-terminated path, and attributes whose descriptor lives
    // through the call.
    let made = unsafe { CreateDirectoryW(PCWSTR(name.as_ptr()), Some(&raw const attributes)) };
    if let Err(e) = made
        && e.code() != ERROR_ALREADY_EXISTS.to_hresult()
    {
        return Err(failed("the copies' directory")(e));
    }
    let dir = open_held(path, FILE_LIST_DIRECTORY.0, FILE_SHARE_WRITE).map_err(io_failure(path))?;
    if information(&dir)?.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err((0, format!("{} is a link", path.display())));
    }
    if !dacl.holds(&dir)? {
        return Err((0, format!("{} has another DACL", path.display())));
    }
    Ok(dir)
}

/// The copy at `path` of `bytes`, kept when it holds them with exactly
/// `dacl` and one name, otherwise written again; held.
fn copy(path: &Path, bytes: &[u8], dacl: &Dacl) -> Result<File, Failure> {
    match open_held(path, FILE_READ_DATA.0, FILE_SHARE_MODE(0)) {
        Ok(mut found) => {
            if is_copy(&mut found, bytes, dacl)? {
                return Ok(found);
            }
        }
        Err(e)
            if e.raw_os_error().is_some_and(|c| {
                [ERROR_FILE_NOT_FOUND.0, ERROR_PATH_NOT_FOUND.0].contains(&c.cast_unsigned())
            }) => {}
        Err(e) => return Err(io_failure(path)(e)),
    }
    write(path, bytes, dacl)?;
    let mut written =
        open_held(path, FILE_READ_DATA.0, FILE_SHARE_MODE(0)).map_err(io_failure(path))?;
    if !is_copy(&mut written, bytes, dacl)? {
        return Err((0, format!("{} differs once written", path.display())));
    }
    Ok(written)
}

/// Whether `file` is a copy of `bytes`: a plain file with one name, exactly
/// `dacl`, and those bytes.
fn is_copy(file: &mut File, bytes: &[u8], dacl: &Dacl) -> Result<bool, Failure> {
    let info = information(file)?;
    let size = u64::from(info.nFileSizeHigh) << 32 | u64::from(info.nFileSizeLow);
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || info.nNumberOfLinks != 1
        || size != u64::try_from(bytes.len()).unwrap_or(u64::MAX)
        || !dacl.holds(file)?
    {
        return Ok(false);
    }
    let mut chunk = vec![0u8; 1 << 16];
    let mut at = 0;
    loop {
        let n = file.read(&mut chunk).map_err(|e| {
            (
                e.raw_os_error().map_or(0, i32::cast_unsigned),
                "a copy".to_owned(),
            )
        })?;
        if n == 0 {
            return Ok(at == bytes.len());
        }
        let (Some(read), Some(want)) = (chunk.get(..n), bytes.get(at..at + n)) else {
            return Ok(false);
        };
        if read != want {
            return Ok(false);
        }
        at += n;
    }
}

/// Names each new copy the launch writes.
static WRITES: AtomicU32 = AtomicU32::new(0);

/// Writes `bytes` with `dacl` to a new file beside `path`, then moves it
/// over `path`. Neither is flushed to the disk: a copy a crash lost or tore
/// differs at the next launch's check, which writes it again.
fn write(path: &Path, bytes: &[u8], dacl: &Dacl) -> Result<(), Failure> {
    let new = path.with_extension(format!(
        "new-{}-{}",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let name = wide(&new);
    let attributes = dacl.attributes();
    // SAFETY: a nul-terminated path, and attributes whose descriptor lives
    // through the call; the handle is owned by the file below.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            Some(&raw const attributes),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(failed("a new copy"))?;
    // SAFETY: the handle just made, owned by the file from here.
    let mut file = unsafe { File::from_raw_handle(handle.0) };
    let written = file.write_all(bytes);
    drop(file);
    let to = wide(path);
    let moved = written.map_err(io_failure(&new)).and_then(|()| {
        // SAFETY: two nul-terminated paths.
        unsafe {
            MoveFileExW(
                PCWSTR(name.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING,
            )
        }
        .map_err(|e| (code_of(&e), format!("{}, written again", path.display())))
    });
    if moved.is_err() {
        let _ = std::fs::remove_file(&new);
    }
    moved
}
