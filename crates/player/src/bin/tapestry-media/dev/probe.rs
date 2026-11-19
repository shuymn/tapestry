//! `--dev-probe <probe>…`: what the sandbox denies the worker and what it
//! leaves it (design.md "Threat Model" T02), for the launcher's tests. The
//! worker answers the game's launch with the hello as when it serves, runs
//! each probe the arguments name and reports each result as a log reply,
//! `probe <name> ok=<0|1> code=<code> <detail>` (`ok` whether the call
//! succeeded; `code` the system's error, an NTSTATUS in hex, or a count),
//! then `probe done`. It then tries, on the handles each command names,
//! what their access allows and what it does not, reporting the same way
//! and `probe done` after each command, until `Quit` or the pipe ends.
//!
//! The probes and their arguments:
//!
//! - `file=<path>`: opens a file for reading.
//! - `event=<name>`: opens a named event by the name, as the worker's own
//!   namespace resolves it.
//! - `nt-event=<NT path>`: opens a named event by its full NT path.
//! - `process=<pid>`: opens that process for querying (`process-query`),
//!   for reading its memory (`process-read`) and for duplicating handles
//!   into it (`process-dup`), and when that open succeeds duplicates an
//!   event of the worker's into it (`dup-in`, `value=` the handle's value
//!   there); then counts the process handles the worker holds besides one
//!   it makes to itself (`processes`).
//! - `socket`: connects to 192.0.2.1:80 (TEST-NET-1); `ok=0` only when the
//!   connect is refused before any packet leaves (10013).
//! - `child`: starts `cmd.exe /c exit 7` (`exit=` its exit code).
//! - `dynamic-code`: allocates executable memory (`rwx`) and makes
//!   writable memory executable (`rw-rx`).
//! - `dll=<path>`: loads a DLL by its path.
//! - `bogus-handle`: reports `bogus-handle-call`, then signals an event
//!   through a value that is no handle; under strict handle checks that
//!   ends the process with `STATUS_INVALID_HANDLE`, else it reports
//!   `bogus-handle` with the error.
//! - `win32k`: loads USER32 (`user32`), then GDI32 (`gdi32`), and calls
//!   `CreateCompatibleDC` (`gdi-call`) and `GetDesktopWindow`
//!   (`user-call`) through each that loaded; a call ran when it returned
//!   a value and made the thread a GUI thread.
//! - `win32k-raw`: loads win32u (`win32u`) and calls its system call stubs
//!   `NtUserGetProcessWindowStation` (`raw-user`) and
//!   `NtGdiCreateCompatibleDC` (`raw-gdi`) on a fresh thread each; a call
//!   ran when the thread became a GUI thread and the value is no error
//!   status.
//! - `environment`: the worker's environment, every variable as
//!   `name=value` joined by `;` (`code` their count).
//! - `desktop=<hwnd>`: through USER32, the worker's desktop's name
//!   (`desktop-name`); opens the input desktop (`input-desktop`) and
//!   `WinSta0` (`winsta0`); counts the top-level windows it sees
//!   (`windows`, `seen=1` when the window `hwnd` is among them).
//!
//! A command's handles: `Open`'s file is read (`file-read`), written
//! (`file-write`) and duplicated with write access (`file-wider`); `Play`'s
//! section is mapped for reading and writing (`section-map`) and duplicated
//! with the right to query it too (`section-wider`), and its event waited on
//! (`event-wait`), set (`event-set`) and duplicated with every access
//! (`event-wider`). Each is also duplicated with the right to change its
//! DACL alone (`<kind>-dac`), and its access in the worker's own table
//! comes as `<kind>-access`, its `code` the access.
//!
//! USER32, GDI32 and win32u are reached only through `GetProcAddress`: an
//! import of one would keep the worker from loading under the win32k
//! lockdown.

use std::ffi::c_void;
use std::process::ExitCode;
use std::time::Duration;

use skin_model::media::control::Command;
use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::System::Threading::NtOpenEvent;
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, FARPROC, FreeLibrary, GetLastError,
    HANDLE, HMODULE, UNICODE_STRING, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    LoadLibraryW,
};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MapViewOfFile,
    PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, PAGE_READWRITE,
    UnmapViewOfFile, VirtualAlloc, VirtualFree, VirtualProtect,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessW, DETACHED_PROCESS, EVENT_ALL_ACCESS, GetCurrentProcess,
    GetCurrentThreadId, GetExitCodeProcess, OpenEventW, OpenProcess, PROCESS_ACCESS_RIGHTS,
    PROCESS_DUP_HANDLE, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    STARTUPINFOW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows::core::{PCSTR, PCWSTR, PWSTR, s, w};

use crate::handles;

/// `--dev-probe`.
pub fn run(args: &[String]) -> ExitCode {
    let _ = crate::hello(crate::ffmpeg::MAX_ALLOC);
    for arg in args {
        let (name, value) = arg.split_once('=').unwrap_or((arg, ""));
        match name {
            "file" => file(value),
            "event" => event(value),
            "nt-event" => nt_event(value),
            "process" => match value.parse() {
                Ok(pid) => process(pid),
                Err(e) => report("process", false, 0, &format!("{value}: {e}")),
            },
            "socket" => socket(),
            "child" => child(),
            "dynamic-code" => dynamic_code(),
            "dll" => dll(value),
            "bogus-handle" => bogus_handle(),
            "win32k" => win32k(),
            "win32k-raw" => win32k_raw(),
            "desktop" => desktop(value.parse().unwrap_or(0)),
            "environment" => environment(),
            other => report(other, false, 0, "no such probe"),
        }
    }
    crate::say("probe done");
    let mut input = std::io::stdin().lock();
    while let Some(command) = crate::read_command(&mut input) {
        match command {
            Command::Open { file, .. } => file_handle(file),
            Command::Play { section, event, .. } => {
                section_handle(section);
                event_handle(event);
            }
            Command::Quit => break,
            Command::Seek { .. } | Command::Close { .. } => {}
        }
        crate::say("probe done");
    }
    ExitCode::SUCCESS
}

/// The worker's environment.
fn environment() {
    let vars: Vec<String> = std::env::vars_os()
        .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
        .collect();
    report("environment", true, vars.len(), &vars.join(";"));
}

/// Reports one probe's result.
fn report(name: &str, ok: bool, code: impl std::fmt::Display, detail: &str) {
    let line = format!("probe {name} ok={} code={code} {detail}", u8::from(ok));
    crate::say(line.trim_end());
}

/// The system's error a failed call returned.
fn error_of(e: &windows::core::Error) -> u32 {
    let code = e.code().0.cast_unsigned();
    // A Win32 error wrapped as an HRESULT.
    if code & 0xffff_0000 == 0x8007_0000 {
        code & 0xffff
    } else {
        code
    }
}

/// The calling thread's last error.
fn last_error() -> u32 {
    // SAFETY: a plain query.
    unsafe { GetLastError() }.0
}

/// `s`, nul-terminated, as Windows' wide strings are.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// The handle a command's `value` names.
fn handle(value: u64) -> HANDLE {
    HANDLE(std::ptr::with_exposed_provenance_mut(
        usize::try_from(value).unwrap_or(0),
    ))
}

/// Closes a handle a probe made.
fn close(h: HANDLE) {
    // SAFETY: a handle the probe made, unused after this.
    let _ = unsafe { CloseHandle(h) };
}

fn file(path: &str) {
    match std::fs::File::open(path) {
        Ok(_) => report("file", true, 0, path),
        Err(e) => report("file", false, e.raw_os_error().unwrap_or(-1), path),
    }
}

fn event(name: &str) {
    let name16 = wide(name);
    // SAFETY: a nul-terminated name; a handle opened is closed here.
    match unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name16.as_ptr())) } {
        Ok(h) => {
            close(h);
            report("event", true, 0, name);
        }
        Err(e) => report("event", false, error_of(&e), name),
    }
}

fn nt_event(path: &str) {
    let mut path16: Vec<u16> = path.encode_utf16().collect();
    let bytes = u16::try_from(path16.len() * 2).unwrap_or(u16::MAX);
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: PWSTR(path16.as_mut_ptr()),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).unwrap_or(0),
        ObjectName: &raw const name,
        ..OBJECT_ATTRIBUTES::default()
    };
    let mut h = HANDLE::default();
    // SAFETY: `attributes` names `path16`, both alive through the call; a
    // handle opened is closed here.
    let status = unsafe {
        NtOpenEvent(
            &raw mut h,
            SYNCHRONIZATION_SYNCHRONIZE.0,
            &raw const attributes,
        )
    };
    if status.is_ok() {
        close(h);
    }
    report(
        "nt-event",
        status.is_ok(),
        format!("{:#010x}", status.0.cast_unsigned()),
        path,
    );
}

fn process(pid: u32) {
    let opens: [(&str, PROCESS_ACCESS_RIGHTS); 3] = [
        ("process-query", PROCESS_QUERY_LIMITED_INFORMATION),
        ("process-read", PROCESS_VM_READ),
        ("process-dup", PROCESS_DUP_HANDLE),
    ];
    for (name, access) in opens {
        // SAFETY: a process opened is closed here.
        match unsafe { OpenProcess(access, false, pid) } {
            Ok(h) => {
                report(name, true, 0, "");
                if access == PROCESS_DUP_HANDLE {
                    dup_in(h);
                }
                close(h);
            }
            Err(e) => report(name, false, error_of(&e), ""),
        }
    }
    processes();
}

/// Duplicates an event of the worker's into the process `into`.
fn dup_in(into: HANDLE) {
    // SAFETY: an unnamed event, closed here.
    let event = match unsafe { CreateEventW(None, false, false, PCWSTR::null()) } {
        Ok(event) => event,
        Err(e) => return report("dup-in", false, error_of(&e), "the event"),
    };
    let mut theirs = HANDLE::default();
    // SAFETY: `event` is the worker's and `into` a process handle with
    // `PROCESS_DUP_HANDLE`.
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            event,
            into,
            &raw mut theirs,
            SYNCHRONIZATION_SYNCHRONIZE.0,
            false,
            DUPLICATE_HANDLE_OPTIONS(0),
        )
    };
    close(event);
    match duplicated {
        Ok(()) => report(
            "dup-in",
            true,
            0,
            &format!("value={}", theirs.0.expose_provenance()),
        ),
        Err(e) => report("dup-in", false, error_of(&e), ""),
    }
}

/// Counts the process handles the worker holds besides one it makes to
/// itself, whose type is the process type.
fn processes() {
    let mut me = HANDLE::default();
    // SAFETY: a real handle to the worker's own process, from its pseudo
    // handle; closed here.
    let made = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentProcess(),
            GetCurrentProcess(),
            &raw mut me,
            PROCESS_QUERY_LIMITED_INFORMATION.0,
            false,
            DUPLICATE_HANDLE_OPTIONS(0),
        )
    };
    if let Err(e) = made {
        return report("processes", false, error_of(&e), "a handle to itself");
    }
    let mine = me.0.expose_provenance() as u64;
    // SAFETY: the pseudo handle of the worker's own process.
    let table = handles::table(unsafe { GetCurrentProcess() });
    close(me);
    let Ok(table) = table else {
        return report("processes", false, 0, "the handle table");
    };
    let process_type = table.iter().find(|e| e.value == mine).map(|e| e.type_index);
    let held = table
        .iter()
        .filter(|e| Some(e.type_index) == process_type && e.value != mine)
        .count();
    report("processes", true, held, "");
}

fn socket() {
    let to = std::net::SocketAddr::from(([192, 0, 2, 1], 80));
    let result = std::net::TcpStream::connect_timeout(&to, Duration::from_secs(1));
    let code = result
        .as_ref()
        .err()
        .and_then(std::io::Error::raw_os_error)
        .unwrap_or(0);
    let detail = match &result {
        Ok(_) => "connected".to_owned(),
        Err(e) => e.kind().to_string(),
    };
    report("socket", code != 10013, code, &detail);
}

fn child() {
    let app = wide(r"C:\Windows\System32\cmd.exe");
    let mut command = wide(r"C:\Windows\System32\cmd.exe /c exit 7");
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        ..STARTUPINFOW::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: nul-terminated names, a writable command line, and structs of
    // their sizes; the handles a creation returns are closed here.
    let created = unsafe {
        CreateProcessW(
            PCWSTR(app.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS,
            None,
            PCWSTR::null(),
            &raw const startup,
            &raw mut info,
        )
    };
    match created {
        Ok(()) => {
            let mut exit = 0;
            // SAFETY: the process just created, whose handles are closed
            // after.
            unsafe {
                let _ = WaitForSingleObject(info.hProcess, 5_000);
                let _ = GetExitCodeProcess(info.hProcess, &raw mut exit);
            }
            close(info.hProcess);
            close(info.hThread);
            report("child", true, 0, &format!("exit={exit}"));
        }
        Err(e) => report("child", false, error_of(&e), ""),
    }
}

fn dynamic_code() {
    // SAFETY: a fresh allocation, released here.
    let p = unsafe { VirtualAlloc(None, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) };
    if p.is_null() {
        report("rwx", false, last_error(), "");
    } else {
        // SAFETY: the allocation just made.
        let _ = unsafe { VirtualFree(p, 0, MEM_RELEASE) };
        report("rwx", true, 0, "");
    }
    // SAFETY: a fresh allocation, released here.
    let p = unsafe { VirtualAlloc(None, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
    if p.is_null() {
        return report("rw-rx", false, last_error(), "the writable allocation");
    }
    let mut old = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: the allocation just made, then released.
    let protected = unsafe { VirtualProtect(p, 4096, PAGE_EXECUTE_READ, &raw mut old) };
    // SAFETY: as above.
    let _ = unsafe { VirtualFree(p, 0, MEM_RELEASE) };
    match protected {
        Ok(()) => report("rw-rx", true, 0, ""),
        Err(e) => report("rw-rx", false, error_of(&e), ""),
    }
}

fn dll(path: &str) {
    let path16 = wide(path);
    // SAFETY: a nul-terminated path; a module loaded is freed here.
    let loaded = unsafe {
        LoadLibraryExW(
            PCWSTR(path16.as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    match loaded {
        Ok(module) => {
            // SAFETY: the module just loaded, unused after this.
            let _ = unsafe { FreeLibrary(module) };
            report("dll", true, 0, path);
        }
        Err(e) => report("dll", false, error_of(&e), path),
    }
}

fn bogus_handle() {
    report("bogus-handle-call", true, 0, "");
    let bogus = HANDLE(std::ptr::without_provenance_mut(0x7fff_fff0));
    // SAFETY: deliberately a value that is no handle: the call fails, and
    // under strict handle checks it raises and the process ends.
    let set = unsafe { SetEvent(bogus) };
    report(
        "bogus-handle",
        set.is_ok(),
        set.err().map_or(0, |e| error_of(&e)),
        "returned",
    );
}

/// Whether the calling thread is a GUI thread: win32k sets
/// `TEB.Win32ThreadInfo`, at 0x78 on x64, when it accepts the thread's
/// first win32k system call.
fn gui_thread() -> bool {
    let teb: usize;
    // SAFETY: reads the calling thread's TEB address, which x64 Windows
    // keeps at gs:[0x30].
    unsafe {
        std::arch::asm!(
            "mov {}, gs:[0x30]",
            out(reg) teb,
            options(nostack, readonly, preserves_flags)
        );
    }
    // SAFETY: the TEB is the calling thread's, mapped while it lives, and
    // holds a pointer at 0x78.
    unsafe { std::ptr::with_exposed_provenance::<usize>(teb + 0x78).read_volatile() != 0 }
}

/// Whether `value` is an error status a refused win32k stub returns
/// (`STATUS_INVALID_SYSTEM_SERVICE`, zero-extended), not a result.
const fn error_status(value: usize) -> bool {
    value >> 32 == 0 && value & 0xc000_0000 == 0xc000_0000
}

/// A function of no argument returning a value, as USER32's, GDI32's and
/// win32u's are called here.
type Call0 = unsafe extern "system" fn() -> usize;
/// The same with one argument.
type Call1 = unsafe extern "system" fn(usize) -> usize;

/// `name` in `module`, if exported.
fn find(module: HMODULE, name: PCSTR) -> FARPROC {
    // SAFETY: a module loaded and a nul-terminated name.
    unsafe { GetProcAddress(module, name) }
}

fn win32k() {
    // SAFETY: loads a system DLL by its name.
    let user = unsafe { LoadLibraryW(w!("user32.dll")) };
    report(
        "user32",
        user.is_ok(),
        user.as_ref().err().map_or(0, error_of),
        "",
    );
    // SAFETY: as above.
    let gdi = unsafe { LoadLibraryW(w!("gdi32.dll")) };
    report(
        "gdi32",
        gdi.is_ok(),
        gdi.as_ref().err().map_or(0, error_of),
        "",
    );
    if let Ok(gdi) = gdi
        && let Some(f) = find(gdi, s!("CreateCompatibleDC"))
    {
        // SAFETY: `CreateCompatibleDC` takes one handle (0: the screen's) and
        // returns one.
        let call = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, Call1>(f) };
        let before = gui_thread();
        // SAFETY: as above; the DC it may make is left to the process.
        let value = unsafe { call(0) };
        let after = gui_thread();
        report(
            "gdi-call",
            value != 0 && after && !error_status(value),
            format!("{value:#x}"),
            &format!("gui before={} after={}", u8::from(before), u8::from(after)),
        );
    }
    if let Ok(user) = user
        && let Some(f) = find(user, s!("GetDesktopWindow"))
    {
        // SAFETY: `GetDesktopWindow` takes nothing and returns a window.
        let call = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, Call0>(f) };
        let before = gui_thread();
        // SAFETY: as above.
        let value = unsafe { call() };
        let after = gui_thread();
        report(
            "user-call",
            value != 0 && after && !error_status(value),
            format!("{value:#x}"),
            &format!("gui before={} after={}", u8::from(before), u8::from(after)),
        );
    }
}

fn win32k_raw() {
    // SAFETY: loads a system DLL from System32.
    let win32u = unsafe { LoadLibraryExW(w!("win32u.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) };
    let win32u = match win32u {
        Ok(module) => {
            report("win32u", true, 0, "");
            module
        }
        Err(e) => return report("win32u", false, error_of(&e), ""),
    };
    let stubs = [
        ("raw-user", s!("NtUserGetProcessWindowStation"), false),
        ("raw-gdi", s!("NtGdiCreateCompatibleDC"), true),
    ];
    for (name, stub, takes_one) in stubs {
        let Some(f) = find(win32u, stub) else {
            report(name, false, last_error(), "not exported");
            continue;
        };
        let ran = std::thread::spawn(move || {
            let before = gui_thread();
            let value = if takes_one {
                // SAFETY: `NtGdiCreateCompatibleDC` takes one handle (0) and
                // returns one.
                unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, Call1>(f)(0) }
            } else {
                // SAFETY: `NtUserGetProcessWindowStation` takes nothing and
                // returns a handle.
                unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, Call0>(f)() }
            };
            (value, before, gui_thread())
        })
        .join();
        match ran {
            Ok((value, before, after)) => report(
                name,
                after && value != 0 && !error_status(value),
                format!("{value:#x}"),
                &format!("gui before={} after={}", u8::from(before), u8::from(after)),
            ),
            Err(_) => report(name, false, 0, "the thread panicked"),
        }
    }
}

/// What the window enumeration found.
struct Seen {
    count: usize,
    target: usize,
    seen: bool,
}

/// `EnumWindows`' callback: counts each window and looks for the target.
unsafe extern "system" fn each_window(window: usize, state: isize) -> i32 {
    // SAFETY: `state` is the address of the `Seen` `desktop` passed, alive
    // while it enumerates.
    let seen =
        unsafe { &mut *std::ptr::with_exposed_provenance_mut::<Seen>(state.cast_unsigned()) };
    seen.count += 1;
    seen.seen |= window == seen.target;
    1
}

fn desktop(target: usize) {
    // SAFETY: loads a system DLL by its name.
    let user = match unsafe { LoadLibraryW(w!("user32.dll")) } {
        Ok(user) => user,
        Err(e) => return report("desktop", false, error_of(&e), "USER32 does not load"),
    };
    let funcs = (
        find(user, s!("GetThreadDesktop")),
        find(user, s!("GetUserObjectInformationW")),
        find(user, s!("OpenInputDesktop")),
        find(user, s!("CloseDesktop")),
        find(user, s!("OpenWindowStationW")),
        find(user, s!("CloseWindowStation")),
        find(user, s!("EnumWindows")),
    );
    let (
        Some(thread_desktop),
        Some(info),
        Some(input),
        Some(close_desktop),
        Some(station),
        Some(close_station),
        Some(enumerate),
    ) = funcs
    else {
        return report("desktop", false, last_error(), "USER32's exports");
    };
    type ThreadDesktop = unsafe extern "system" fn(u32) -> usize;
    type ObjectInformation =
        unsafe extern "system" fn(usize, i32, *mut c_void, u32, *mut u32) -> i32;
    type OpenInput = unsafe extern "system" fn(u32, i32, u32) -> usize;
    type CloseObject = unsafe extern "system" fn(usize) -> i32;
    type OpenStation = unsafe extern "system" fn(*const u16, i32, u32) -> usize;
    type Enumerate =
        unsafe extern "system" fn(unsafe extern "system" fn(usize, isize) -> i32, isize) -> i32;
    type Export = unsafe extern "system" fn() -> isize;
    // SAFETY: each export's signature, as USER32 declares it.
    let (thread_desktop, info, input, close_desktop, station, close_station, enumerate) = unsafe {
        (
            std::mem::transmute::<Export, ThreadDesktop>(thread_desktop),
            std::mem::transmute::<Export, ObjectInformation>(info),
            std::mem::transmute::<Export, OpenInput>(input),
            std::mem::transmute::<Export, CloseObject>(close_desktop),
            std::mem::transmute::<Export, OpenStation>(station),
            std::mem::transmute::<Export, CloseObject>(close_station),
            std::mem::transmute::<Export, Enumerate>(enumerate),
        )
    };
    let mut name = [0u16; 128];
    let mut needed = 0;
    // SAFETY: the calling thread's desktop, and a buffer of the size given
    // (`UOI_NAME` is 2).
    let named = unsafe {
        let desk = thread_desktop(GetCurrentThreadId());
        desk != 0
            && info(
                desk,
                2,
                name.as_mut_ptr().cast(),
                u32::try_from(size_of_val(&name)).unwrap_or(0),
                &raw mut needed,
            ) != 0
    };
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    report(
        "desktop-name",
        named,
        if named { 0 } else { last_error() },
        &String::from_utf16_lossy(&name[..end]),
    );
    // SAFETY: opens the input desktop for reading its objects
    // (`DESKTOP_READOBJECTS`), closed here.
    let desk = unsafe { input(0, 0, 0x0001) };
    report(
        "input-desktop",
        desk != 0,
        if desk == 0 { last_error() } else { 0 },
        "",
    );
    if desk != 0 {
        // SAFETY: the desktop just opened.
        let _ = unsafe { close_desktop(desk) };
    }
    let winsta0 = wide("WinSta0");
    // SAFETY: opens `WinSta0` to enumerate its desktops and read its
    // attributes, closed here.
    let station = unsafe { station(winsta0.as_ptr(), 0, 0x0001 | 0x0002) };
    report(
        "winsta0",
        station != 0,
        if station == 0 { last_error() } else { 0 },
        "",
    );
    if station != 0 {
        // SAFETY: the window station just opened.
        let _ = unsafe { close_station(station) };
    }
    let mut seen = Seen {
        count: 0,
        target,
        seen: false,
    };
    // SAFETY: the callback reads `seen`, alive through the call.
    let _ = unsafe {
        enumerate(
            each_window,
            (&raw mut seen).expose_provenance().cast_signed(),
        )
    };
    report(
        "windows",
        true,
        seen.count,
        &format!("seen={}", u8::from(seen.seen)),
    );
}

/// Reports `value`'s access in the worker's own handle table as
/// `<kind>-access`.
fn access(kind: &str, value: u64) {
    // SAFETY: the pseudo handle of the worker's own process.
    let table = handles::table(unsafe { GetCurrentProcess() });
    let found = table
        .ok()
        .and_then(|t| t.into_iter().find(|e| e.value == value));
    report(
        &format!("{kind}-access"),
        found.is_some(),
        found.map_or(0, |e| e.access),
        "",
    );
}

/// The right to change an object's DACL.
const WRITE_DAC: u32 = 0x0004_0000;

/// Duplicates `value` with `access`, wider than its own, reporting it as
/// `name`.
fn wider(name: &str, value: u64, access: u32) {
    let mut new = HANDLE::default();
    // SAFETY: a handle the game handed the worker, duplicated within the
    // worker; a duplicate made is closed here.
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle(value),
            GetCurrentProcess(),
            &raw mut new,
            access,
            false,
            DUPLICATE_HANDLE_OPTIONS(0),
        )
    };
    match duplicated {
        Ok(()) => {
            close(new);
            report(name, true, 0, "");
        }
        Err(e) => report(name, false, error_of(&e), ""),
    }
}

fn file_handle(value: u64) {
    access("file", value);
    let mut byte = [0u8; 1];
    let mut n = 0;
    // SAFETY: the file handle the game handed the worker, and a buffer of
    // one byte.
    let read = unsafe { ReadFile(handle(value), Some(&mut byte), Some(&raw mut n), None) };
    report(
        "file-read",
        read.is_ok(),
        read.err().map_or(0, |e| error_of(&e)),
        "",
    );
    // SAFETY: as above.
    let written = unsafe { WriteFile(handle(value), Some(&byte), Some(&raw mut n), None) };
    report(
        "file-write",
        written.is_ok(),
        written.err().map_or(0, |e| error_of(&e)),
        "",
    );
    // `FILE_READ_DATA | FILE_WRITE_DATA`.
    wider("file-wider", value, 0x0001 | 0x0002);
    wider("file-dac", value, WRITE_DAC);
}

fn section_handle(value: u64) {
    access("section", value);
    // SAFETY: the section handle the game handed the worker, mapped whole
    // and unmapped here.
    let view = unsafe { MapViewOfFile(handle(value), FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0) };
    if view.Value.is_null() {
        report("section-map", false, last_error(), "");
    } else {
        // SAFETY: the view just mapped.
        let _ = unsafe { UnmapViewOfFile(view) };
        report("section-map", true, 0, "");
    }
    // `SECTION_QUERY | SECTION_MAP_WRITE | SECTION_MAP_READ`.
    wider("section-wider", value, 0x0001 | 0x0002 | 0x0004);
    wider("section-dac", value, WRITE_DAC);
}

fn event_handle(value: u64) {
    access("event", value);
    // SAFETY: the event handle the game handed the worker.
    let waited = unsafe { WaitForSingleObject(handle(value), 0) };
    let ok = waited == WAIT_OBJECT_0 || waited == WAIT_TIMEOUT;
    report("event-wait", ok, if ok { 0 } else { last_error() }, "");
    // SAFETY: as above.
    let set = unsafe { SetEvent(handle(value)) };
    report(
        "event-set",
        set.is_ok(),
        set.err().map_or(0, |e| error_of(&e)),
        "",
    );
    wider("event-wider", value, EVENT_ALL_ACCESS.0);
    wider("event-dac", value, WRITE_DAC);
}
