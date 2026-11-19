//! `tapestry test-device flash <uf2> --build <id> --port <location>` and
//! `tapestry test-device bootsel --port <location>` (docs/test-device.md
//! "Host Commands", "Recovery").
//!
//! `flash` checks every block of the UF2 before anything reaches a device.
//! The device at the given location is put into its boot ROM's drive by
//! the 1200-baud touch on the COM port resolved from that location (no
//! command, so a device of any protocol version takes it), unless the drive
//! is already there. One rule for drives: the copy goes ahead only when
//! exactly one `RP2350` boot-ROM drive is present anywhere, within 10 s,
//! and its USB parent's location is the given one; a removable volume
//! whose identity cannot be resolved counts as a second drive. The drive
//! is found from the location, never by its letter: each removable volume
//! is named by its volume GUID path (`\\?\Volume{…}\`) and tied to a
//! location through its disk in the configuration manager's tree. The
//! command creates the new file once on that path, checks through the
//! handle that the file lies on that volume, and writes through that
//! handle, so a letter that changes or passes to another volume, or a
//! drive that leaves between the check and the write, sends nothing
//! elsewhere. The file is opened write-through ([`UF2_FLAGS`]), so a
//! write's full count means its bytes were written to the device, not only
//! taken into the system cache; a flush or close that fails as the drive
//! leaves counts as the boot ROM's reboot only with that evidence for every
//! byte, or after a flush that succeeded. Then it rediscovers the device
//! on that location until the link resolved from it answers `hello` of
//! this protocol version naming the build with `faults 0`.
//!
//! The OS calls sit behind [`Flasher`], so the rules are tested against a
//! stand-in.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_DEV_NOT_EXIST, ERROR_DEVICE_NOT_CONNECTED, ERROR_DEVICE_REMOVED,
    ERROR_FILE_INVALID, ERROR_NO_MORE_FILES, ERROR_NO_SUCH_DEVICE, ERROR_NOT_READY, GENERIC_WRITE,
    HANDLE, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_WRITE_THROUGH,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    FlushFileBuffers, GetDriveTypeW, GetFinalPathNameByHandleW, GetVolumePathNamesForVolumeNameW,
    VOLUME_NAME_GUID, WriteFile,
};
use windows::Win32::System::WindowsProgramming::DRIVE_REMOVABLE;
use windows::core::PCWSTR;

use super::devices::{self, System};
use super::link::{Connection, Endpoints, Facts, Refused};
use super::win::{Owned, multi_sz, until_nul, wide};

/// How long the drive and the rediscovery may take, each.
pub const WAIT: Duration = Duration::from_secs(10);
/// How often the waits look again.
pub const POLL: Duration = Duration::from_millis(100);
/// The looks within [`WAIT`].
const POLLS: u32 = 100;
/// The boot ROM's board id in `INFO_UF2.TXT`.
const BOARD: &str = "RP2350";
/// The name the UF2 is written under.
const TARGET: &str = "FIRMWARE.UF2";
/// How the UF2's file is opened: write-through, so a write returns only
/// once its bytes are written to the device, not when the system cache
/// takes them ("If `FILE_FLAG_WRITE_THROUGH` is used but
/// `FILE_FLAG_NO_BUFFERING` is not also specified, so that system caching is
/// in effect, then the data is written to the system cache but is flushed
/// to disk without delay", `CreateFile`, "Caching Behavior"). Not
/// `FILE_FLAG_NO_BUFFERING`: write-through already makes a write return
/// only after the system cache has written its bytes out, and what that
/// flag adds is a write-through request to the disk's own cache, which
/// "not all hard disk hardware supports", at the cost of every write's
/// length being a multiple of the volume's sector size and its buffer
/// sector-aligned ("File Buffering"). The device takes each UF2 block as
/// it arrives over USB, and no host flag can tell what it does with it
/// after; the rediscovery's `hello` does.
const UF2_FLAGS: FILE_FLAGS_AND_ATTRIBUTES =
    FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0 | FILE_FLAG_WRITE_THROUGH.0);

/// Whether an `INFO_UF2.TXT` names the RP2350's boot ROM.
pub fn names_board(info: &str) -> bool {
    info.lines().any(|l| {
        l.strip_prefix("Board-ID:")
            .is_some_and(|v| v.trim() == BOARD)
    })
}

/// What a removable volume is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    /// An `RP2350` boot-ROM drive whose USB parent is at `location`.
    Board { location: String },
    /// Not a boot-ROM drive: no `INFO_UF2.TXT`, or another board's.
    Other,
    /// Its `INFO_UF2.TXT` is unreadable, or its USB parent or location is
    /// unresolved: it may be a boot-ROM drive.
    Unresolved(String),
}

/// A removable volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drive {
    /// Its volume GUID path, `\\?\Volume{…}\`.
    pub volume: String,
    /// Its drive letter, for the log only.
    pub letter: Option<char>,
    pub identity: Identity,
}

/// A file created on a volume, written through its handle.
pub trait NewFile {
    /// The file's path as its handle resolves it, with the volume named by
    /// its GUID (`GetFinalPathNameByHandle` with `VOLUME_NAME_GUID`).
    ///
    /// # Errors
    ///
    /// The path cannot be read through the handle.
    fn final_path(&self) -> anyhow::Result<String>;

    /// One write of `bytes`: how many it took. Through a write-through
    /// handle ([`Self::write_through`]) they were written to the device;
    /// otherwise they may be only in the system cache.
    ///
    /// # Errors
    ///
    /// The write failed.
    fn write(&mut self, bytes: &[u8]) -> windows::core::Result<usize>;

    /// Whether the handle was opened write-through
    /// (`FILE_FLAG_WRITE_THROUGH`).
    fn write_through(&self) -> bool;

    /// Flushes what was written to the volume.
    ///
    /// # Errors
    ///
    /// The flush failed (the boot ROM may reboot as its last block lands).
    fn flush(&mut self) -> windows::core::Result<()>;

    /// Closes the file's handle.
    ///
    /// # Errors
    ///
    /// The close failed (the boot ROM may reboot as its last block lands).
    fn close(self: Box<Self>) -> windows::core::Result<()>;
}

/// The OS calls `flash` and `bootsel` make: the link's endpoints, the
/// removable volumes, a new file, and the pause between looks.
pub trait Flasher: Endpoints {
    /// Every removable volume present, each with its identity.
    ///
    /// # Errors
    ///
    /// The volumes cannot be listed.
    fn drives(&self) -> anyhow::Result<Vec<Drive>>;

    /// Creates the new file `path` (it must not exist) for writing, with
    /// `flags` (`CreateFile`'s flags and attributes).
    ///
    /// # Errors
    ///
    /// The file cannot be created.
    fn create_new(
        &self,
        path: &str,
        flags: FILE_FLAGS_AND_ATTRIBUTES,
    ) -> anyhow::Result<Box<dyn NewFile>>;

    /// Waits [`POLL`] before the next look.
    fn pause(&self);
}

/// The one drive the rule allows writing to the device at `location`.
///
/// # Errors
///
/// Why none may be written: an unresolved removable volume, no `RP2350`
/// drive or several anywhere, or the one on another location.
pub fn choose<'a>(drives: &'a [Drive], location: &str) -> Result<&'a Drive, String> {
    if let Some((d, why)) = drives.iter().find_map(|d| match &d.identity {
        Identity::Unresolved(why) => Some((d, why)),
        _ => None,
    }) {
        return Err(format!(
            "removable volume {} cannot be identified ({why}): it counts as another drive",
            d.volume
        ));
    }
    let boards: Vec<(&Drive, &str)> = drives
        .iter()
        .filter_map(|d| match &d.identity {
            Identity::Board { location } => Some((d, location.as_str())),
            _ => None,
        })
        .collect();
    let [(drive, at)] = boards.as_slice() else {
        let at: Vec<&str> = boards.iter().map(|(_, at)| *at).collect();
        return Err(format!(
            "{} {BOARD} drives are present (at {})",
            boards.len(),
            at.join(", ")
        ));
    };
    if *at != location {
        return Err(format!(
            "the {BOARD} drive {} is at location {at}, not {location}",
            drive.volume
        ));
    }
    Ok(drive)
}

/// Waits up to [`WAIT`] for the rule to allow one drive at `location`.
fn wait_drive<F: Flasher + ?Sized>(host: &F, location: &str) -> anyhow::Result<Drive> {
    let mut last = String::new();
    for _ in 0..POLLS {
        match choose(&host.drives()?, location) {
            Ok(d) => return Ok(d.clone()),
            Err(why) => last = why,
        }
        host.pause();
    }
    bail!("no {BOARD} drive to write at {location} within {WAIT:?}: {last}")
}

/// Puts the device at `location` into its boot ROM's drive, unless the
/// drive is there already, and returns the drive.
///
/// # Errors
///
/// No device at the location (nothing is touched), or the drive rule not
/// met within [`WAIT`].
pub fn enter_bootsel<F: Flasher + ?Sized>(host: &F, location: &str) -> anyhow::Result<Drive> {
    let why = match choose(&host.drives()?, location) {
        Ok(d) => return Ok(d.clone()),
        Err(why) => why,
    };
    let link = Connection::bind(host, Some(location)).with_context(|| {
        format!(
            "no drive to write ({why}), and no device at location {location} to put into \
             its boot ROM's drive (for the first flash, connect the board with BOOTSEL \
             held: docs/test-device.md \"Hardware\")"
        )
    })?;
    println!("bootsel: the 1200-baud touch on {}", link.com());
    // The device reboots as the line coding arrives, so the rest of the
    // port's setup may fail on a device that has gone: the drive decides.
    if let Err(e) = link.touch_bootsel() {
        eprintln!("bootsel: the touch ended with {e:#} (the device may have gone already)");
    }
    wait_drive(host, location)
}

/// Whether `path` lies on volume `volume` (both GUID paths).
fn on_volume(path: &str, volume: &str) -> bool {
    path.get(..volume.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(volume))
}

/// Creates the UF2's file on `drive` by its volume GUID path, checks
/// through the handle that it lies there, and writes through that handle.
///
/// # Errors
///
/// The file cannot be created, or its handle resolves elsewhere: nothing
/// is written then. Or the copy failed ([`copy`]).
fn write<F: Flasher + ?Sized>(host: &F, drive: &Drive, image: &[u8]) -> anyhow::Result<()> {
    let path = format!("{}{TARGET}", drive.volume);
    let file = host.create_new(&path, UF2_FLAGS)?;
    let resolved = file.final_path()?;
    if !on_volume(&resolved, &drive.volume) {
        bail!(
            "the new file resolves to {resolved}, not to volume {}: nothing written",
            drive.volume
        );
    }
    copy(file, image)
}

/// The errors of a volume whose device has gone: what the flush or the
/// close of a file meets when the boot ROM reboots as its last block lands.
const GONE: [WIN32_ERROR; 6] = [
    ERROR_DEVICE_NOT_CONNECTED,
    ERROR_NO_SUCH_DEVICE,
    ERROR_DEV_NOT_EXIST,
    ERROR_DEVICE_REMOVED,
    ERROR_FILE_INVALID,
    ERROR_NOT_READY,
];

/// Whether `e` says the volume's device has gone ([`GONE`]).
fn gone(e: &windows::core::Error) -> bool {
    WIN32_ERROR::from_error(e).is_some_and(|w| GONE.contains(&w))
}

/// Writes every byte of `image` through `file`, then flushes and closes it.
/// The boot ROM reboots once the last block is written, so a flush or
/// close failing as the device goes ([`GONE`]) is that reboot only when
/// every byte is known to have been written to the device: each write
/// returned its full count through a write-through handle, or the flush
/// succeeded. It is logged then, and the rediscovery decides. That
/// evidence shows the bytes were written to the device; it does not show
/// that the boot ROM took them as a whole image, which only the
/// rediscovery's `hello` naming the build shows.
///
/// # Errors
///
/// A write failed or took nothing before every byte was written, the flush
/// or the close failed as the device went before every byte was known to
/// be on it, or either failed otherwise.
fn copy(mut file: Box<dyn NewFile>, image: &[u8]) -> anyhow::Result<()> {
    let mut done = 0;
    while let Some(rest) = image.get(done..).filter(|r| !r.is_empty()) {
        let taken = match file.write(rest) {
            Ok(n) if n == 0 || n > rest.len() => Err(anyhow!("the volume took {n} bytes")),
            Ok(n) => Ok(n),
            Err(e) => Err(anyhow::Error::new(e).context("writing the UF2")),
        };
        done += taken.with_context(|| {
            format!(
                "the copy failed after {done} of {} bytes: nothing reached the drive whole",
                image.len()
            )
        })?;
    }
    let after =
        |step: &str, result: windows::core::Result<()>, delivered: bool| -> anyhow::Result<()> {
            match result {
                Ok(()) => Ok(()),
                Err(e) if gone(&e) && delivered => {
                    eprintln!(
                        "flash: {step} the UF2 ended with {e} after all {} bytes were written to \
                     the device: the boot ROM's reboot",
                        image.len()
                    );
                    Ok(())
                }
                Err(e) if gone(&e) => Err(anyhow::Error::new(e).context(format!(
                    "{step} the UF2: the drive left before the {} bytes it took were known to \
                 be on it",
                    image.len()
                ))),
                Err(e) => Err(anyhow::Error::new(e).context(format!("{step} the UF2"))),
            }
        };
    // Every byte is counted: through a write-through handle the count is of
    // bytes on the device; otherwise only a flush that succeeds shows it.
    let flushed = file.flush();
    let delivered = file.write_through() || flushed.is_ok();
    after("flushing", flushed, delivered)?;
    after("closing", file.close(), delivered)
}

/// Whether a failure to reach the device may pass on its own: it is not
/// there yet, its port is not there yet, or the port cannot be opened yet.
fn transient(e: &anyhow::Error) -> bool {
    match (
        e.downcast_ref::<Refused>(),
        e.downcast_ref::<devices::Refusal>(),
    ) {
        (Some(Refused::Absent(_) | Refused::NoPort(_)), _) => true,
        (Some(_), _) => false,
        (None, Some(r)) => *r == devices::Refusal::NoPort,
        (None, None) => true,
    }
}

/// Waits up to [`WAIT`] for the device at `location` to answer `hello` on
/// the link resolved from that location, naming `build` with `faults 0`.
///
/// # Errors
///
/// Another build or a fault build answers, a device elsewhere or several,
/// a port that resolves back elsewhere, or no answer within [`WAIT`].
pub fn rediscover<F: Flasher + ?Sized>(
    host: &F,
    location: &str,
    build: &str,
) -> anyhow::Result<Facts> {
    let mut last = String::new();
    for _ in 0..POLLS {
        match Connection::bind(host, Some(location)) {
            Ok(mut link) => match link.check_hello(Some(build)) {
                Ok(hello) => return Ok(hello.clone()),
                Err(e) if !transient(&e) => {
                    return Err(e.context(format!("the device at {location} after the copy")));
                }
                Err(e) => last = format!("{e:#}"),
            },
            Err(e) if transient(&e) => last = format!("{e:#}"),
            Err(e) => return Err(e.context(format!("the device at {location} after the copy"))),
        }
        host.pause();
    }
    bail!("the device did not answer at {location} within {WAIT:?}: {last}")
}

/// Flashes `image` to the device at `location` and checks it comes back
/// there as `build`.
///
/// # Errors
///
/// A UF2 block breaking a rule (before any device action), any step
/// failing, or another build answering.
pub fn flash_image<F: Flasher + ?Sized>(
    host: &F,
    image: &[u8],
    build: &str,
    location: &str,
) -> anyhow::Result<Facts> {
    let summary = test_device::uf2::check(image)
        .map_err(|e| anyhow!("not a UF2 for the RP2350's flash: {e}"))?;
    println!(
        "flash: {} blocks for {:#010x}..{:#010x}",
        summary.blocks, summary.start, summary.end
    );
    let drive = enter_bootsel(host, location)?;
    println!(
        "flash: drive {} ({}) at {location}",
        drive.volume,
        drive
            .letter
            .map_or_else(|| "no letter".to_owned(), |l| format!("{l}:"))
    );
    write(host, &drive, image)?;
    let hello = rediscover(host, location, build)?;
    println!("flash: {}", hello.line);
    Ok(hello)
}

/// `flash <uf2> --build <id> --port <location>`.
///
/// # Errors
///
/// As [`flash_image`], or the file cannot be read.
pub fn flash(uf2: &Path, build: &str, location: &str) -> anyhow::Result<()> {
    let image = std::fs::read(uf2).with_context(|| uf2.display().to_string())?;
    flash_image(&System, &image, build, location)
        .with_context(|| uf2.display().to_string())
        .map(drop)
}

/// `bootsel --port <location>`.
///
/// # Errors
///
/// As [`enter_bootsel`].
pub fn bootsel(location: &str) -> anyhow::Result<()> {
    let drive = enter_bootsel(&System, location)?;
    println!("bootsel: drive {} at {location}", drive.volume);
    Ok(())
}

/// A file created on a volume, and whether it was opened write-through.
struct VolumeFile(Owned, bool);

impl NewFile for VolumeFile {
    fn final_path(&self) -> anyhow::Result<String> {
        let mut buf = vec![0_u16; 1024];
        // SAFETY: an open file handle; the buffer's length is passed with it.
        let n = unsafe { GetFinalPathNameByHandleW(self.0.0, &mut buf, VOLUME_NAME_GUID) };
        if n == 0 || n as usize >= buf.len() {
            bail!(
                "GetFinalPathNameByHandle: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(until_nul(&buf))
    }

    fn write(&mut self, bytes: &[u8]) -> windows::core::Result<usize> {
        let mut written = 0;
        // SAFETY: the buffer outlives the synchronous write.
        unsafe { WriteFile(self.0.0, Some(bytes), Some(&mut written), None) }?;
        Ok(written as usize)
    }

    fn write_through(&self) -> bool {
        self.1
    }

    fn flush(&mut self) -> windows::core::Result<()> {
        // SAFETY: an open file handle.
        unsafe { FlushFileBuffers(self.0.0) }
    }

    fn close(mut self: Box<Self>) -> windows::core::Result<()> {
        // The handle leaves `Owned`, so its drop does not close it again.
        let handle = std::mem::take(&mut self.0.0);
        // SAFETY: an open file handle, closed once.
        unsafe { CloseHandle(handle) }
    }
}

/// The volumes `first` and then `next` name, until the enumeration ends
/// (`ERROR_NO_MORE_FILES`), with those `keep` takes.
///
/// # Errors
///
/// A step of the enumeration failed otherwise: discovery fails closed.
fn enumerate_volumes(
    first: String,
    mut next: impl FnMut() -> windows::core::Result<String>,
    keep: impl Fn(&str) -> bool,
) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut volume = first;
    loop {
        if keep(&volume) {
            out.push(volume);
        }
        match next() {
            Ok(v) => volume = v,
            Err(e) if WIN32_ERROR::from_error(&e) == Some(ERROR_NO_MORE_FILES) => return Ok(out),
            Err(e) => return Err(anyhow::Error::new(e).context("FindNextVolume")),
        }
    }
}

/// The removable volumes' GUID paths.
fn removable_volumes() -> anyhow::Result<Vec<String>> {
    let mut name = [0_u16; 64];
    // SAFETY: the buffer's length is passed with it.
    let find = unsafe { FindFirstVolumeW(&mut name) }.context("FindFirstVolume")?;
    let first = until_nul(&name);
    let out = enumerate_volumes(
        first,
        || {
            // SAFETY: the search handle is open; the buffer's length is passed.
            unsafe { FindNextVolumeW(find, &mut name) }?;
            Ok(until_nul(&name))
        },
        |volume| {
            let w = wide(volume);
            // SAFETY: a nul-terminated volume GUID path.
            let kind = unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) };
            kind == DRIVE_REMOVABLE
        },
    );
    // SAFETY: the search handle is open and closed once.
    let _ = unsafe { FindVolumeClose(find) };
    out
}

/// The first drive letter volume `volume` is mounted at.
fn letter_of(volume: &str) -> Option<char> {
    let w = wide(volume);
    let mut buf = [0_u16; 256];
    let mut len = u32::try_from(buf.len()).unwrap_or(0);
    // SAFETY: a nul-terminated path; the buffer holds `len` characters.
    unsafe { GetVolumePathNamesForVolumeNameW(PCWSTR(w.as_ptr()), Some(&mut buf), &mut len) }
        .ok()?;
    multi_sz(&buf).into_iter().find_map(|p| match p.as_bytes() {
        [l, b':', b'\\'] if l.is_ascii_alphabetic() => Some(char::from(*l)),
        _ => None,
    })
}

/// What removable volume `volume` is.
fn identify(volume: &str) -> Identity {
    match std::fs::read_to_string(format!("{volume}INFO_UF2.TXT")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Identity::Other,
        Err(e) => Identity::Unresolved(format!("INFO_UF2.TXT: {e}")),
        Ok(info) if !names_board(&info) => Identity::Other,
        Ok(_) => devices::volume_location(volume).map_or_else(
            || Identity::Unresolved("its USB parent's location cannot be resolved".to_owned()),
            |location| Identity::Board { location },
        ),
    }
}

impl Flasher for System {
    fn drives(&self) -> anyhow::Result<Vec<Drive>> {
        Ok(removable_volumes()?
            .into_iter()
            .map(|volume| Drive {
                letter: letter_of(&volume),
                identity: identify(&volume),
                volume,
            })
            .collect())
    }

    fn create_new(
        &self,
        path: &str,
        flags: FILE_FLAGS_AND_ATTRIBUTES,
    ) -> anyhow::Result<Box<dyn NewFile>> {
        let w = wide(path);
        // SAFETY: a nul-terminated path; no security attributes or template.
        let handle: HANDLE = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                CREATE_NEW,
                flags,
                None,
            )
        }
        .with_context(|| format!("creating {path}"))?;
        let through = flags.contains(FILE_FLAG_WRITE_THROUGH);
        Ok(Box::new(VolumeFile(Owned(handle), through)))
    }

    fn pause(&self) {
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_DEVICE_NOT_CONNECTED, ERROR_NO_MORE_FILES, WIN32_ERROR,
    };

    use super::super::link::fake::{self, Dev, hello};
    use super::super::link::{Endpoints, Link, Port};
    use super::*;

    const HERE: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(3)";
    const ELSEWHERE: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(4)";
    const V1: &str = r"\\?\Volume{11111111-0000-0000-0000-000000000001}\";
    const V2: &str = r"\\?\Volume{22222222-0000-0000-0000-000000000002}\";
    const BUILD: &str = "0123456789ab";

    fn board(volume: &str, letter: char, location: &str) -> Drive {
        Drive {
            volume: volume.to_owned(),
            letter: Some(letter),
            identity: Identity::Board {
                location: location.to_owned(),
            },
        }
    }

    /// The scene a stand-in host plays: devices and drives present now,
    /// what a touch and a complete write change, and what was written.
    #[derive(Default)]
    struct Scene {
        devs: Vec<Dev>,
        drives: Vec<Drive>,
        after_touch: Vec<Drive>,
        after_write: Vec<Dev>,
        letters: HashMap<char, String>,
        /// A letter that passes to another volume when a file is created.
        letter_moves: Option<(char, String)>,
        /// The volume a created file's handle resolves to, when not its own.
        resolves_to: Option<String>,
        written: HashMap<String, Vec<u8>>,
        opened: Vec<String>,
        /// The bytes a file takes before its next write fails as the device
        /// leaves.
        write_fails_after: Option<usize>,
        /// The error the flush, and the close, fail with.
        flush_fails: Option<WIN32_ERROR>,
        close_fails: Option<WIN32_ERROR>,
    }

    #[derive(Clone, Default)]
    struct Host(Rc<RefCell<Scene>>);

    impl Host {
        fn new(f: impl FnOnce(&mut Scene)) -> Self {
            let h = Self::default();
            f(&mut h.0.borrow_mut());
            h
        }

        fn written(&self) -> HashMap<String, Vec<u8>> {
            self.0.borrow().written.clone()
        }

        fn touched(&self) -> bool {
            self.0
                .borrow()
                .devs
                .iter()
                .any(|d| d.device.with(|d| d.touched))
        }
    }

    impl Endpoints for Host {
        fn find(&self, bound: Option<&str>) -> anyhow::Result<Option<(String, Option<Link>)>> {
            fake::find(&self.0.borrow().devs, bound)
        }

        fn open(&self, name: &str) -> anyhow::Result<Box<dyn Port>> {
            let mut s = self.0.borrow_mut();
            s.opened.push(name.to_owned());
            fake::open(&s.devs, name)
        }

        fn location_of(&self, name: &str) -> anyhow::Result<Option<String>> {
            Ok(fake::location_of(&self.0.borrow().devs, name))
        }
    }

    /// A file on a stand-in volume. Written through, each write delivers
    /// its bytes to the drive before it returns; otherwise its writes go
    /// into a cache that only a flush that succeeds delivers.
    struct File {
        host: Host,
        volume: String,
        through: bool,
        /// The bytes its writes took.
        taken: usize,
        cached: Vec<u8>,
    }

    impl NewFile for File {
        fn final_path(&self) -> anyhow::Result<String> {
            let s = self.host.0.borrow();
            let volume = s.resolves_to.as_ref().unwrap_or(&self.volume);
            Ok(format!("{volume}{TARGET}"))
        }

        fn write(&mut self, bytes: &[u8]) -> windows::core::Result<usize> {
            let mut s = self.host.0.borrow_mut();
            let n = match s.write_fails_after {
                Some(k) if self.taken >= k => {
                    drop(s);
                    self.leave();
                    return Err(ERROR_DEVICE_NOT_CONNECTED.into());
                }
                Some(k) => bytes.len().min(k - self.taken),
                None => bytes.len(),
            };
            self.taken += n;
            if self.through {
                s.written
                    .entry(self.volume.clone())
                    .or_default()
                    .extend_from_slice(&bytes[..n]);
            } else {
                self.cached.extend_from_slice(&bytes[..n]);
            }
            Ok(n)
        }

        fn write_through(&self) -> bool {
            self.through
        }

        /// A flush that succeeds delivers the cache; one that fails as the
        /// drive leaves loses it.
        fn flush(&mut self) -> windows::core::Result<()> {
            let fails = self.host.0.borrow().flush_fails;
            if fails.is_none() {
                let cached = std::mem::take(&mut self.cached);
                self.host
                    .0
                    .borrow_mut()
                    .written
                    .entry(self.volume.clone())
                    .or_default()
                    .extend_from_slice(&cached);
            }
            self.leave();
            fails.map_or(Ok(()), |e| Err(e.into()))
        }

        fn close(self: Box<Self>) -> windows::core::Result<()> {
            self.leave();
            self.host
                .0
                .borrow()
                .close_fails
                .map_or(Ok(()), |e| Err(e.into()))
        }
    }

    impl File {
        /// The boot ROM reboots: its drive leaves and the device comes
        /// back.
        fn leave(&self) {
            let mut s = self.host.0.borrow_mut();
            if !s.drives.is_empty() {
                s.drives.clear();
                s.devs = s.after_write.clone();
            }
        }
    }

    impl Flasher for Host {
        fn drives(&self) -> anyhow::Result<Vec<Drive>> {
            if self.touched() {
                let mut s = self.0.borrow_mut();
                s.devs.clear();
                s.drives = s.after_touch.clone();
            }
            Ok(self.0.borrow().drives.clone())
        }

        fn create_new(
            &self,
            path: &str,
            flags: FILE_FLAGS_AND_ATTRIBUTES,
        ) -> anyhow::Result<Box<dyn NewFile>> {
            let mut s = self.0.borrow_mut();
            if let Some((letter, volume)) = s.letter_moves.take() {
                s.letters.insert(letter, volume);
            }
            let volume = if let Some(rest) = path.strip_prefix(r"\\?\Volume{") {
                let end = rest.find('}').context("no volume")?;
                format!(r"\\?\Volume{{{}}}\", rest.get(..end).context("no volume")?)
            } else {
                let letter = path.chars().next().context("no letter")?;
                s.letters
                    .get(&letter)
                    .cloned()
                    .or_else(|| {
                        s.drives
                            .iter()
                            .find(|d| d.letter == Some(letter))
                            .map(|d| d.volume.clone())
                    })
                    .context("no such letter")?
            };
            drop(s);
            Ok(Box::new(File {
                host: self.clone(),
                volume,
                through: flags.contains(FILE_FLAG_WRITE_THROUGH),
                taken: 0,
                cached: Vec::new(),
            }))
        }

        fn pause(&self) {}
    }

    fn image() -> Vec<u8> {
        test_device::uf2::block(0, 1, test_device::uf2::FLASH_BASE, &[0x5a; 256]).to_vec()
    }

    fn device(location: &str, com: &str, build: &str) -> Dev {
        Dev::new(location, com, hello(build, false))
    }

    /// A drive on another location is never written, whether it is there
    /// already or appears after the touch.
    #[test]
    fn a_drive_off_the_given_location_gets_no_write() {
        let there = Host::new(|s| {
            s.drives = vec![board(V1, 'E', ELSEWHERE)];
            s.after_write = vec![device(HERE, "COM5", BUILD)];
        });
        let e = flash_image(&there, &image(), BUILD, HERE).unwrap_err();
        assert!(there.written().is_empty(), "{e:#}");
        let after_touch = Host::new(|s| {
            s.devs = vec![device(HERE, "COM5", "old")];
            s.after_touch = vec![board(V1, 'E', ELSEWHERE)];
            s.after_write = vec![device(HERE, "COM5", BUILD)];
        });
        let e = flash_image(&after_touch, &image(), BUILD, HERE).unwrap_err();
        assert!(after_touch.written().is_empty(), "{e:#}");
        assert!(format!("{e:#}").contains("not PCIROOT"), "{e:#}");
    }

    /// Two boot-ROM drives anywhere refuse the copy, even with one at the
    /// given location listed first.
    #[test]
    fn a_second_drive_anywhere_gets_no_write() {
        let host = Host::new(|s| {
            s.drives = vec![board(V1, 'E', HERE), board(V2, 'F', ELSEWHERE)];
            s.after_write = vec![device(HERE, "COM5", BUILD)];
        });
        let e = flash_image(&host, &image(), BUILD, HERE).unwrap_err();
        assert!(host.written().is_empty(), "{e:#}");
        assert!(format!("{e:#}").contains("2 RP2350 drives"), "{e:#}");
    }

    /// A removable volume that cannot be identified counts as a second
    /// drive.
    #[test]
    fn an_unresolved_candidate_drive_gets_no_write() {
        let host = Host::new(|s| {
            s.drives = vec![
                board(V1, 'E', HERE),
                Drive {
                    volume: V2.to_owned(),
                    letter: None,
                    identity: Identity::Unresolved("INFO_UF2.TXT: not ready".to_owned()),
                },
            ];
            s.after_write = vec![device(HERE, "COM5", BUILD)];
        });
        let e = flash_image(&host, &image(), BUILD, HERE).unwrap_err();
        assert!(host.written().is_empty(), "{e:#}");
        assert!(format!("{e:#}").contains("cannot be identified"), "{e:#}");
    }

    /// The file is created by the volume's GUID path and written through
    /// its handle: a letter that passes to another volume between the
    /// lookup and the creation gets nothing, and the volume is written only
    /// while its GUID path still resolves to it.
    #[test]
    fn the_write_goes_through_the_verified_volume_handle() {
        let moved = Host::new(|s| {
            s.devs = vec![device(HERE, "COM5", "old")];
            s.after_touch = vec![board(V1, 'E', HERE)];
            s.letters.insert('E', V1.to_owned());
            s.letter_moves = Some(('E', V2.to_owned()));
            s.after_write = vec![device(HERE, "COM5", BUILD)];
        });
        let hello = flash_image(&moved, &image(), BUILD, HERE).unwrap();
        assert_eq!(hello.build, BUILD);
        let written = moved.written();
        assert_eq!(written.get(V1), Some(&image()));
        assert_eq!(written.get(V2), None);
        let elsewhere = Host::new(|s| {
            s.drives = vec![board(V1, 'E', HERE)];
            s.letters.insert('E', V1.to_owned());
            s.resolves_to = Some(V2.to_owned());
            s.after_write = vec![device(HERE, "COM5", BUILD)];
        });
        let e = flash_image(&elsewhere, &image(), BUILD, HERE).unwrap_err();
        assert!(elsewhere.written().is_empty(), "{e:#}");
        assert!(format!("{e:#}").contains("nothing written"), "{e:#}");
    }

    /// The copy must complete before the device's answer counts: a write
    /// that fails before every byte is on the drive fails the flash, even
    /// when the same build answers after it. Once every byte is written, a
    /// flush or close that fails as the drive leaves is the boot ROM's
    /// reboot; any other failure there fails the flash.
    #[test]
    fn an_interrupted_copy_fails_the_flash() {
        let host = |f: fn(&mut Scene)| {
            Host::new(|s| {
                s.drives = vec![board(V1, 'E', HERE)];
                s.after_write = vec![device(HERE, "COM5", BUILD)];
                f(s);
            })
        };
        let cut = host(|s| s.write_fails_after = Some(100));
        let e = flash_image(&cut, &image(), BUILD, HERE).unwrap_err();
        assert_eq!(cut.written().get(V1).map(Vec::len), Some(100), "{e:#}");
        assert!(format!("{e:#}").contains("after 100 of 512 bytes"), "{e:#}");
        for gone in [
            host(|s| s.close_fails = Some(ERROR_DEVICE_NOT_CONNECTED)),
            host(|s| s.flush_fails = Some(ERROR_DEVICE_NOT_CONNECTED)),
        ] {
            let hello = flash_image(&gone, &image(), BUILD, HERE).unwrap();
            assert_eq!(hello.build, BUILD);
            assert_eq!(gone.written().get(V1), Some(&image()));
        }
        let denied = host(|s| s.close_fails = Some(ERROR_ACCESS_DENIED));
        let e = flash_image(&denied, &image(), BUILD, HERE).unwrap_err();
        assert!(format!("{e:#}").contains("closing the UF2"), "{e:#}");
    }

    /// A full write count shows the bytes reached the drive only when they
    /// were written through. With the flush failing as the drive leaves
    /// after a full count, a copy whose bytes the file only took into its
    /// cache fails, and `flash` goes on with every byte delivered.
    #[test]
    fn a_failed_flush_passes_only_bytes_written_through() {
        let host = || {
            Host::new(|s| {
                s.drives = vec![board(V1, 'E', HERE)];
                s.after_write = vec![device(HERE, "COM5", BUILD)];
                s.flush_fails = Some(ERROR_DEVICE_NOT_CONNECTED);
            })
        };
        let cached = host();
        let file = cached
            .create_new(&format!("{V1}{TARGET}"), FILE_ATTRIBUTE_NORMAL)
            .unwrap();
        let e = copy(file, &image()).unwrap_err();
        assert_eq!(cached.written().get(V1), None, "{e:#}");
        assert!(format!("{e:#}").contains("known to be on it"), "{e:#}");
        let through = host();
        let hello = flash_image(&through, &image(), BUILD, HERE).unwrap();
        assert_eq!(hello.build, BUILD);
        assert_eq!(through.written().get(V1), Some(&image()));
    }

    /// Only the end of the volumes ends their enumeration: another failure
    /// fails discovery, so a drive is never missed.
    #[test]
    fn a_failed_volume_enumeration_fails_discovery() {
        let mut steps = [Ok(V2.to_owned()), Err(ERROR_ACCESS_DENIED.into())].into_iter();
        let e = enumerate_volumes(V1.to_owned(), || steps.next().unwrap(), |_| true).unwrap_err();
        assert!(format!("{e:#}").contains("FindNextVolume"), "{e:#}");
        let mut steps = [Ok(V2.to_owned()), Err(ERROR_NO_MORE_FILES.into())].into_iter();
        let found = enumerate_volumes(V1.to_owned(), || steps.next().unwrap(), |v| v == V2);
        assert_eq!(found.unwrap(), [V2]);
    }

    /// After the copy only the device at the given location counts: one
    /// answering the build elsewhere fails the flash, and so does another
    /// build at the location.
    #[test]
    fn the_device_after_the_copy_is_checked_on_the_location() {
        let elsewhere = Host::new(|s| {
            s.devs = vec![device(HERE, "COM5", "old")];
            s.after_touch = vec![board(V1, 'E', HERE)];
            s.after_write = vec![device(ELSEWHERE, "COM7", BUILD)];
        });
        let e = flash_image(&elsewhere, &image(), BUILD, HERE).unwrap_err();
        assert!(format!("{e:#}").contains("after the copy"), "{e:#}");
        let other_build = Host::new(|s| {
            s.drives = vec![board(V1, 'E', HERE)];
            s.after_write = vec![device(HERE, "COM5", "fedcba987654")];
        });
        let e = flash_image(&other_build, &image(), BUILD, HERE).unwrap_err();
        assert!(format!("{e:#}").contains("not 0123456789ab"), "{e:#}");
        let faults = Host::new(|s| {
            s.drives = vec![board(V1, 'E', HERE)];
            s.after_write = vec![Dev::new(HERE, "COM5", hello(BUILD, true))];
        });
        let e = flash_image(&faults, &image(), BUILD, HERE).unwrap_err();
        assert!(format!("{e:#}").contains("faults 1"), "{e:#}");
    }

    /// A UF2 that breaks a block rule is refused before any device action,
    /// and a location with no device and no drive gets no touch and no
    /// write.
    #[test]
    fn nothing_reaches_a_device_before_the_checks_pass() {
        let host = Host::new(|s| {
            s.devs = vec![device(HERE, "COM5", "old")];
            s.after_touch = vec![board(V1, 'E', HERE)];
        });
        let mut bad = image();
        if let Some(b) = bad.get_mut(0) {
            *b ^= 1;
        }
        let e = flash_image(&host, &bad, BUILD, HERE).unwrap_err();
        assert!(format!("{e}").contains("block 0: magic word"), "{e:#}");
        assert!(host.0.borrow().opened.is_empty());
        assert!(!host.touched());
        let e = flash_image(&host, &image(), BUILD, ELSEWHERE).unwrap_err();
        assert!(host.0.borrow().opened.is_empty(), "{e:#}");
        assert!(!host.touched());
        assert!(host.written().is_empty());
    }

    #[test]
    fn the_boot_rom_drive_is_told_by_its_board_id() {
        let info = "UF2 Bootloader v1.0\r\nModel: Raspberry Pi RP2350\r\nBoard-ID: RP2350\r\n";
        assert!(names_board(info));
        assert!(!names_board("Board-ID: RPI-RP2\n"));
        assert!(!names_board("Model: RP2350\n"));
        assert!(on_volume(
            &format!("{}FIRMWARE.UF2", V1.to_ascii_uppercase()),
            V1
        ));
        assert!(!on_volume(&format!("{V2}FIRMWARE.UF2"), V1));
    }
}
