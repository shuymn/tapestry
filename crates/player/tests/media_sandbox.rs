//! The media worker's sandbox (design.md "Threat Model" T02): the real
//! `tapestry-media.exe`, launched by the game's host as the game launches
//! it, staged with its FFmpeg DLLs in a directory of its own. Each denial
//! is asserted with the system's code under the recipe, beside a control
//! that removes only the restriction that denies it (the test-only setup),
//! so the boundary is measured rather than assumed; the worker's
//! `--dev-probe` (dev-tools) tries each denied operation from inside.

#![cfg(windows)]

mod media_support;

#[cfg(feature = "dev-tools")]
use std::collections::BTreeMap;
#[cfg(feature = "dev-tools")]
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, OwnedHandle};
use std::sync::atomic::AtomicU64;
use std::time::Duration;
#[cfg(feature = "dev-tools")]
use std::time::Instant;

use media_support::{
    Kind, Launched, REFUSED, Restrictions, Setup, Stage, Step, media_host, policy, recipe,
    token_facts,
};
#[cfg(feature = "dev-tools")]
use media_support::{Probe, probe, probes};
use skin_model::media::control::{Command, FILE_ACCESS, Failure, Reply};
#[cfg(feature = "dev-tools")]
use skin_model::media::control::{EVENT_ACCESS, SECTION_ACCESS};
use skin_model::media::{Geometry, Reader, Window};
use windows::Win32::Foundation::{CloseHandle, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE};
use windows::Win32::System::JobObjects::IsProcessInJob;
use windows::Win32::System::Memory::{
    FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    ProcessChildProcessPolicy, ProcessDynamicCodePolicy, ProcessExtensionPointDisablePolicy,
    ProcessImageLoadPolicy, ProcessStrictHandleCheckPolicy, ProcessSystemCallDisablePolicy,
};
use windows::core::PCWSTR;

/// How long a reply may take.
const REPLY: Duration = Duration::from_secs(30);

/// A handle this test made, closed when dropped.
struct Own(HANDLE);

impl Drop for Own {
    fn drop(&mut self) {
        // SAFETY: a handle this value made, unused after this.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The value a handle of this process has, as a table gives it.
fn value(h: HANDLE) -> u64 {
    h.0.expose_provenance() as u64
}

/// Launches the worker in `stage` under `setup`.
fn launch(stage: &Stage, setup: Setup) -> Launched {
    stage.launch(&setup).unwrap_or_else(|e| panic!("{e}"))
}

// Covers: T02
#[test]
fn worker_starts_confined_before_it_runs() {
    let stage = Stage::new();
    let worker = launch(&stage, recipe());
    let process = worker.process();
    // The launch checked these before the worker ran (else it fails);
    // they hold, read here from the running worker.
    let mut inside = windows::core::BOOL(0);
    // SAFETY: the worker's process and its job.
    unsafe { IsProcessInJob(process, worker.job(), &raw mut inside) }.unwrap();
    assert!(inside.as_bool(), "the worker is in its job");
    // An AppContainer with no capability at low integrity (0x1000).
    assert_eq!(token_facts(process), (true, 0, 0x1000));
    // Each policy's flags hold the recipe's bits.
    for (which, bits) in [
        (ProcessDynamicCodePolicy, 0b1),
        (ProcessSystemCallDisablePolicy, 0b1),
        (ProcessStrictHandleCheckPolicy, 0b11),
        (ProcessImageLoadPolicy, 0b111),
        (ProcessExtensionPointDisablePolicy, 0b1),
        (ProcessChildProcessPolicy, 0b1),
    ] {
        let flags = policy(process, which);
        assert_eq!(flags & bits, bits, "{which:?}: {flags:#x}");
    }
    assert!(!worker.hello().version.is_empty());
}

/// An inheritable event and an inheritable handle to this process, which
/// a worker that inherits more than its list would hold.
fn bait() -> (Own, Own) {
    let inheritable = windows::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<windows::Win32::Security::SECURITY_ATTRIBUTES>()).unwrap(),
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    // SAFETY: an unnamed event, closed by `Own`.
    let event =
        Own(
            unsafe { CreateEventW(Some(&raw const inheritable), true, false, PCWSTR::null()) }
                .unwrap(),
        );
    let mut game = HANDLE::default();
    // SAFETY: a handle to this process from its pseudo handle, closed by
    // `Own`.
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentProcess(),
            GetCurrentProcess(),
            &raw mut game,
            (PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ).0,
            true,
            DUPLICATE_HANDLE_OPTIONS(0),
        )
    }
    .unwrap();
    (event, Own(game))
}

// Covers: T02
#[test]
fn worker_inherits_exactly_the_listed_handles() {
    let stage = Stage::new();
    let (event, game) = bait();
    let worker = launch(&stage, recipe());
    let mut held: Vec<u64> = worker.suspended().iter().map(|e| e.value).collect();
    held.sort_unstable();
    let mut ends = worker.pipe_ends().to_vec();
    ends.sort_unstable();
    // Exactly the control pipe's two ends: no process or thread, no bait.
    assert_eq!(held, ends);
    drop(worker);
    // Control: without the list it inherits the bait too.
    let worker = launch(&stage, recipe().without(Restrictions::HANDLE_LIST));
    let held: Vec<u64> = worker.suspended().iter().map(|e| e.value).collect();
    for (bait, what) in [(&event, "the event"), (&game, "the game's process")] {
        assert!(
            held.contains(&value(bait.0)),
            "{what} {:#x} not among {held:#x?}",
            value(bait.0)
        );
    }
}

/// A video for the worker: one of the committed fixtures.
fn video() -> std::path::PathBuf {
    media_support::root().join("crates/player/tests/media/zlib/plain.mkv")
}

/// A frame ring as the game makes one ([`media_host::ring_objects`]): its
/// section, mapped here with its header written, and its free-slot event.
struct Ring {
    section: OwnedHandle,
    event: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl Ring {
    /// A ring for `width` × `height` frames.
    fn new(width: u32, height: u32) -> Self {
        let geometry = Geometry::new(width, height).unwrap();
        let (section, event) =
            media_host::ring_objects(u64::try_from(geometry.bytes()).unwrap()).unwrap();
        // SAFETY: the section just made, mapped whole until drop.
        let view = unsafe {
            MapViewOfFile(
                HANDLE(section.as_raw_handle()),
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                0,
            )
        };
        assert!(!view.Value.is_null());
        // SAFETY: the view holds the ring's words while it is mapped, every
        // access through the atomics.
        let words =
            unsafe { std::slice::from_raw_parts(view.Value.cast::<AtomicU64>(), geometry.words()) };
        Reader::new(
            words,
            geometry,
            Window {
                first: 0,
                last: i64::MAX,
            },
        )
        .unwrap();
        Self {
            section,
            event,
            view,
        }
    }
}

impl Ring {
    fn section(&self) -> HANDLE {
        HANDLE(self.section.as_raw_handle())
    }

    fn event(&self) -> HANDLE {
        HANDLE(self.event.as_raw_handle())
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: the view this ring mapped, unused after this.
        let _ = unsafe { UnmapViewOfFile(self.view) };
    }
}

/// The next reply other than a log line, or a panic naming the log.
fn reply(worker: &Launched) -> Reply {
    worker
        .recv(REPLY)
        .unwrap_or_else(|e| panic!("no reply ({e}); the worker's log:\n{}", worker.log()))
}

// Covers: T02
#[test]
fn worker_takes_a_handle_only_of_its_kind_and_access_unheld() {
    let stage = Stage::new();
    let mut worker = launch(&stage, recipe());
    // The game opens a video's file for each player: duplicates of one
    // handle would share its position.
    let file = || std::fs::File::open(video()).unwrap();
    let files = [file(), file(), file(), file()];
    let handle = |i: usize| HANDLE(files[i].as_raw_handle());
    let open = |worker: &mut Launched, player: u8, value: u64| {
        worker
            .send(&Command::Open {
                player,
                file: value,
            })
            .unwrap();
        reply(worker)
    };
    let refused = |player: u8, code: u32| Reply::Failed {
        player,
        why: Failure::Command,
        code: code.cast_signed(),
    };
    let first = worker.hand_in(handle(0), Kind::File).unwrap();
    assert!(
        matches!(open(&mut worker, 1, first), Reply::Probed { player: 1, .. }),
        "the worker's log:\n{}",
        worker.log()
    );
    // Another player's file (`ERROR_BUSY`).
    assert_eq!(open(&mut worker, 2, first), refused(2, 170));
    // A ring whose section another player holds.
    let ring = Ring::new(320, 240);
    let held_section = worker.hand_in(ring.section(), Kind::Section).unwrap();
    let held_event = worker.hand_in(ring.event(), Kind::Event).unwrap();
    worker
        .send(&Command::Play {
            player: 1,
            section: held_section,
            event: held_event,
        })
        .unwrap();
    assert_eq!(reply(&worker), Reply::Playing { player: 1 });
    let second = worker.hand_in(handle(1), Kind::File).unwrap();
    let probed = open(&mut worker, 3, second);
    assert!(
        matches!(probed, Reply::Probed { player: 3, .. }),
        "{probed:?}; the worker's log:\n{}",
        worker.log()
    );
    let other_event = worker.hand_in(ring.event(), Kind::Event).unwrap();
    worker
        .send(&Command::Play {
            player: 3,
            section: held_section,
            event: other_event,
        })
        .unwrap();
    assert_eq!(reply(&worker), refused(3, 170));
    // A closed player's file: no handle of that value is left
    // (`ERROR_INVALID_HANDLE`).
    worker.send(&Command::Close { player: 1 }).unwrap();
    loop {
        match reply(&worker) {
            Reply::Closed { player: 1 } => break,
            Reply::Ended { player: 1, .. } | Reply::Failed { player: 1, .. } => {}
            other => panic!("{other:?} while player 1 closes"),
        }
    }
    assert_eq!(open(&mut worker, 4, first), refused(4, 6));
    // A file handed with more than read access (`ERROR_ACCESS_DENIED`).
    let wide = worker
        .hand_in_with(handle(2), FILE_ACCESS | 0x0080)
        .unwrap();
    assert_eq!(open(&mut worker, 5, wide), refused(5, 5));
    // An event named as a file.
    let event_value = worker.hand_in(ring.event(), Kind::Event).unwrap();
    assert_eq!(open(&mut worker, 6, event_value), refused(6, 6));
    // Control: a file handed as the game hands one.
    let fresh = worker.hand_in(handle(3), Kind::File).unwrap();
    let probed = open(&mut worker, 7, fresh);
    assert!(
        matches!(probed, Reply::Probed { player: 7, .. }),
        "{probed:?}; the worker's log:\n{}",
        worker.log()
    );
}

/// Launches `--dev-probe` with `probes` under `setup` and waits for them:
/// the results and the log.
#[cfg(feature = "dev-tools")]
fn probe_run(
    stage: &Stage,
    setup: Setup,
    args: &[&str],
) -> (Launched, BTreeMap<String, Probe>, String) {
    let mut all = vec!["--dev-probe"];
    all.extend_from_slice(args);
    let worker = launch(stage, setup.args_of(&all));
    let log = worker.probed(1);
    (worker, probes(&log), log)
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_holds_its_handles_with_their_least_access() {
    let stage = Stage::new();
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), b"a video's bytes").unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file.path())
        .unwrap();
    let ring = Ring::new(16, 16);
    // `wide` hands each handle with every access a control needs.
    let run = |wide: bool| {
        let (mut worker, _, _) = probe_run(&stage, recipe(), &[]);
        let access = |kind: Kind| {
            if !wide {
                return kind.access();
            }
            match kind {
                // `FILE_READ_DATA | FILE_WRITE_DATA | SYNCHRONIZE`.
                Kind::File => 0x0010_0003,
                // `SECTION_QUERY | SECTION_MAP_WRITE | SECTION_MAP_READ`.
                Kind::Section => 0x0007,
                // `EVENT_ALL_ACCESS`.
                Kind::Event => 0x001f_0003,
            }
        };
        let file_value = worker
            .hand_in_with(HANDLE(file.as_raw_handle()), access(Kind::File))
            .unwrap();
        worker
            .send(&Command::Open {
                player: 0,
                file: file_value,
            })
            .unwrap();
        let _ = worker.probed(2);
        let section_value = worker
            .hand_in_with(ring.section(), access(Kind::Section))
            .unwrap();
        let event_value = worker
            .hand_in_with(ring.event(), access(Kind::Event))
            .unwrap();
        worker
            .send(&Command::Play {
                player: 0,
                section: section_value,
                event: event_value,
            })
            .unwrap();
        let log = worker.probed(3);
        (probes(&log), log)
    };
    let (least, log) = run(false);
    let p = |name: &str| probe(&least, name, &log);
    // Each handle has exactly its kind's access, in the worker's own
    // table.
    let accesses = [
        (p("file-access").code, FILE_ACCESS),
        (p("section-access").code, SECTION_ACCESS),
        (p("event-access").code, EVENT_ACCESS),
    ];
    assert!(
        accesses.iter().all(|&(got, want)| got == u64::from(want)),
        "{accesses:x?}; the worker's log:\n{log}"
    );
    // Its use works; a write, a wider right or the right to change its
    // DACL fails (`ERROR_ACCESS_DENIED`).
    for allowed in ["file-read", "section-map", "event-wait"] {
        assert!(p(allowed).ok, "{allowed}; the worker's log:\n{log}");
    }
    for denied in [
        "file-write",
        "file-wider",
        "file-dac",
        "section-wider",
        "section-dac",
        "event-set",
        "event-wider",
        "event-dac",
    ] {
        let probe = p(denied);
        assert!(
            !probe.ok && probe.code == 5,
            "{denied}; the worker's log:\n{log}"
        );
    }
    // Control: handed with those rights, the same calls succeed.
    let (wide, log) = run(true);
    for name in [
        "file-write",
        "file-wider",
        "section-wider",
        "event-set",
        "event-wider",
    ] {
        assert!(
            probe(&wide, name, &log).ok,
            "{name}; the worker's log:\n{log}"
        );
    }
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_cannot_hand_a_handle_back_to_the_game() {
    let stage = Stage::new();
    let pid = std::process::id().to_string();
    let arg = format!("process={pid}");
    let (_worker, found, log) = probe_run(&stage, recipe(), &[&arg]);
    let p = |name: &str| probe(&found, name, &log);
    // It cannot open the game for duplicating a handle into it, or for
    // anything else, and holds no handle to a process but one to itself.
    for name in ["process-query", "process-read", "process-dup"] {
        let probe = p(name);
        assert!(!probe.ok && probe.code == 5, "{name}: {probe:?}");
    }
    assert!(!found.contains_key("dup-in"), "{log}");
    assert_eq!(p("processes").code, 0);
    // Control: outside its AppContainer it opens the game and duplicates
    // an event into it.
    let (_worker, found, log) = probe_run(
        &stage,
        recipe().without(Restrictions::APP_CONTAINER),
        &[&arg],
    );
    let dup = probe(&found, "dup-in", &log);
    assert!(dup.ok, "{dup:?}");
    let theirs: u64 = dup.detail.strip_prefix("value=").unwrap().parse().unwrap();
    // SAFETY: the pseudo handle of this process.
    let ours = media_host::handles::table(unsafe { GetCurrentProcess() }).unwrap();
    assert!(
        ours.iter().any(|e| e.value == theirs),
        "{theirs:#x} is not in this process"
    );
    let theirs = HANDLE(std::ptr::with_exposed_provenance_mut(
        usize::try_from(theirs).unwrap(),
    ));
    // SAFETY: the event the worker duplicated into this process.
    let _ = unsafe { CloseHandle(theirs) };
}

// Covers: T02
#[test]
fn worker_launch_fails_closed() {
    let stage = Stage::new();
    for step in Step::ALL {
        let Err(e) = stage.launch(&recipe().failing_at(step)) else {
            panic!("{step:?} failing, the launch succeeded");
        };
        assert_eq!(e.step, step, "{e}");
        // A process made is terminated: before it ran, but at the
        // handshake, which it runs for.
        let made = matches!(
            step,
            Step::Assign | Step::Check | Step::Resume | Step::Handshake
        );
        assert_eq!(e.exit, made.then_some(REFUSED), "{step:?}: {e}");
    }
    // The check refuses each restriction it reads when it is missing, and
    // names it: the job, each mitigation policy, the AppContainer, and the
    // handle list, which left out lets the worker inherit this bait. An
    // AppContainer turns strict handle checks on by itself, so they are
    // left out with it.
    let _bait = bait();
    for (missing, named) in [
        (Restrictions::JOB, "job"),
        (Restrictions::NO_DYNAMIC_CODE, "dynamic code"),
        (Restrictions::NO_WIN32K, "win32k system calls"),
        (
            Restrictions::STRICT_HANDLES.with(Restrictions::APP_CONTAINER),
            "strict handle checks",
        ),
        (Restrictions::IMAGE_LOAD, "image loads"),
        (Restrictions::NO_EXTENSION_POINTS, "extension points"),
        (Restrictions::NO_CHILD, "child processes"),
        (Restrictions::APP_CONTAINER, "AppContainer"),
        (Restrictions::HANDLE_LIST, "not only the pipe's"),
    ] {
        let setup = recipe().applying(Restrictions::RECIPE.without(missing));
        let Err(e) = stage.launch(&setup) else {
            panic!("{missing:?} left out, the launch succeeded");
        };
        assert_eq!(
            (e.step, e.exit),
            (Step::Check, Some(REFUSED)),
            "{missing:?}: {e}"
        );
        assert!(e.what.contains(named), "{missing:?}: {e}");
    }
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_launch_fails_closed_without_a_hello_in_time() {
    let stage = Stage::new();
    let started = Instant::now();
    // The recipe's own limit, 2 s, not the other tests' patient one.
    let Err(e) = stage.launch(&Setup::recipe().args_of(&["--dev-silent", "10000"])) else {
        panic!("a silent worker launched");
    };
    let took = started.elapsed();
    assert_eq!(
        (e.step, e.code, e.exit),
        (Step::Handshake, 1460, Some(REFUSED)),
        "{e}"
    );
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(8),
        "{took:?}"
    );
    // One that answers with what is no reply fails it too.
    let Err(e) = stage.launch(&Setup::recipe().args_of(&["--dev-tools-probe"])) else {
        panic!("a worker without a hello launched");
    };
    assert_eq!(e.step, Step::Handshake, "{e}");
}

// Covers: T02
#[test]
fn job_close_kills_the_worker() {
    let stage = Stage::new();
    let mut worker = launch(&stage, recipe());
    worker.close_job();
    assert!(
        worker.wait_exit(Duration::from_secs(5)).is_some(),
        "the worker outlived its job"
    );
    // Control: without the job it keeps running.
    let mut worker = launch(&stage, recipe().without(Restrictions::JOB));
    worker.close_job();
    assert!(worker.wait_exit(Duration::from_millis(500)).is_none());
}

/// A named event of this process's: the handle and its name, and its full
/// NT path, which the system gives.
#[cfg(feature = "dev-tools")]
fn named_event() -> (Own, String, String) {
    use windows::Wdk::Foundation::{NtQueryObject, OBJECT_INFORMATION_CLASS};

    let name = format!(
        "tapestry-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let name16: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a nul-terminated name; the event is closed by `Own`.
    let event = Own(unsafe { CreateEventW(None, true, false, PCWSTR(name16.as_ptr())) }.unwrap());
    let mut buffer = vec![0u64; 128];
    let mut bytes = 0;
    // SAFETY: `ObjectNameInformation` (1) into a buffer of the size given.
    let status = unsafe {
        NtQueryObject(
            Some(event.0),
            OBJECT_INFORMATION_CLASS(1),
            Some(buffer.as_mut_ptr().cast()),
            u32::try_from(buffer.len() * 8).unwrap(),
            Some(&raw mut bytes),
        )
    };
    assert!(status.is_ok(), "{status:?}");
    // SAFETY: the buffer holds a `UNICODE_STRING` and the name it points to.
    let path = unsafe {
        let name = &*buffer
            .as_ptr()
            .cast::<windows::Win32::Foundation::UNICODE_STRING>();
        String::from_utf16(std::slice::from_raw_parts(
            name.Buffer.0,
            usize::from(name.Length / 2),
        ))
        .unwrap()
    };
    (event, name, path)
}

/// A copy of a system DLL in `dir`, made with a low mandatory label (its
/// maker may give a new file one no higher than its own).
#[cfg(feature = "dev-tools")]
fn low_label_dll(dir: &std::path::Path) -> std::path::PathBuf {
    use std::io::Write as _;
    use std::os::windows::io::FromRawHandle as _;

    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_MODE,
    };

    let dll = dir.join("lowlabel.dll");
    let bytes = std::fs::read(r"C:\Windows\System32\version.dll").unwrap();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: an SDDL string of a low label that refuses writes from below
    // (its DACL left to inheritance); the descriptor is left to the test's
    // end.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            windows::core::w!("S:(ML;;NW;;;LW)"),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    }
    .unwrap();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap(),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let path: Vec<u16> = dll.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a nul-terminated path and the descriptor; the handle is owned
    // by the file below.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            Some(&raw const attributes),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .unwrap();
    // SAFETY: the file just made, owned from here.
    let mut file = unsafe { std::fs::File::from_raw_handle(handle.0) };
    file.write_all(&bytes).unwrap();
    dll
}

/// Asserts each `(name, ok, code)` of `want` against `found`.
#[cfg(feature = "dev-tools")]
fn expect(
    found: &BTreeMap<String, Probe>,
    log: &str,
    what: &str,
    want: &[(&str, bool, Option<u64>)],
) {
    for &(name, ok, code) in want {
        let probe = probe(found, name, log);
        assert!(
            probe.ok == ok && code.is_none_or(|c| c == probe.code),
            "{what}: {name} {probe:?}, not ok={ok} code={code:?}; the worker's log:\n{log}"
        );
    }
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_denials_hold_with_their_codes() {
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, CloseWindowStation, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
        OpenInputDesktop, OpenWindowStationW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WS_EX_TOOLWINDOW, WS_POPUP,
    };

    let stage = Stage::new();
    let outside = tempfile::NamedTempFile::new().unwrap();
    let (_event, event_name, event_path) = named_event();
    let dll = low_label_dll(stage.dir());
    let args = [
        format!("file={}", outside.path().display()),
        format!("event={event_name}"),
        format!("nt-event={event_path}"),
        format!("process={}", std::process::id()),
        "socket".to_owned(),
        "child".to_owned(),
        "dynamic-code".to_owned(),
        format!("dll={}", dll.display()),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let run = |setup: Setup| {
        let (_worker, found, log) = probe_run(&stage, setup, &args);
        (found, log)
    };
    // Under the recipe each is refused with its code.
    let (found, log) = run(recipe());
    expect(
        &found,
        &log,
        "the recipe",
        &[
            ("file", false, Some(5)),
            // A name it opens resolves in its AppContainer's own namespace.
            ("event", false, Some(5)),
            ("nt-event", false, Some(0xc000_0022)),
            ("process-query", false, Some(5)),
            ("process-read", false, Some(5)),
            ("socket", false, Some(10_013)),
            ("child", false, Some(367)),
            ("rwx", false, Some(1_655)),
            ("rw-rx", false, Some(1_655)),
            ("dll", false, Some(5)),
        ],
    );
    // Each beside its control, only the restriction that refuses it
    // removed.
    let (found, log) = run(recipe().without(Restrictions::APP_CONTAINER));
    expect(
        &found,
        &log,
        "without the AppContainer",
        &[
            ("file", true, None),
            ("event", true, None),
            ("nt-event", true, None),
            ("process-query", true, None),
            ("process-read", true, None),
            ("socket", true, None),
        ],
    );
    // Without the child policy the job's one-process limit refuses it
    // (`ERROR_NOT_ENOUGH_QUOTA`); without both it runs.
    let (found, log) = run(recipe().without(Restrictions::NO_CHILD));
    expect(
        &found,
        &log,
        "without the child policy",
        &[("child", false, Some(1_816))],
    );
    let (found, log) = run(recipe()
        .without(Restrictions::NO_CHILD)
        .without(Restrictions::ONE_PROCESS));
    expect(
        &found,
        &log,
        "without the child policy and the limit",
        &[("child", true, None)],
    );
    assert_eq!(probe(&found, "child", &log).detail, "exit=7");
    let (found, log) = run(recipe().without(Restrictions::NO_DYNAMIC_CODE));
    expect(
        &found,
        &log,
        "without the dynamic code policy",
        &[("rwx", true, None), ("rw-rx", true, None)],
    );
    let (found, log) = run(recipe().without(Restrictions::IMAGE_LOAD));
    expect(
        &found,
        &log,
        "without the image load policy",
        &[("dll", true, None)],
    );
    // An invalid handle raises `STATUS_INVALID_HANDLE`, which ends the
    // worker; without strict handle checks and the AppContainer, which
    // turns them on by itself, the call only fails.
    let worker = launch(&stage, recipe().args_of(&["--dev-probe", "bogus-handle"]));
    let log = worker.probed(1);
    assert_eq!(worker.wait_exit(REPLY), Some(0xc000_0008), "{log}");
    let found = probes(&log);
    assert!(
        found.contains_key("bogus-handle-call") && !found.contains_key("bogus-handle"),
        "{log}"
    );
    drop(worker);
    let (found, log) = run_bogus(&stage);
    expect(
        &found,
        &log,
        "without strict handle checks",
        &[("bogus-handle", false, Some(6))],
    );

    // The desktop's probes need USER32, which loads only with the win32k
    // lockdown removed. The worker's desktop is its own: it cannot open the
    // input desktop or `WinSta0` and sees no window.
    // SAFETY: a hidden tool window of this process's, destroyed below.
    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            windows::core::w!("STATIC"),
            windows::core::w!("tapestry probe"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            None,
            None,
        )
    }
    .unwrap();
    let desktop = [format!("desktop={}", window.0.expose_provenance())];
    let desktop: Vec<&str> = desktop.iter().map(String::as_str).collect();
    let run = |setup: Setup| {
        let (_worker, found, log) =
            probe_run(&stage, setup.without(Restrictions::NO_WIN32K), &desktop);
        (found, log)
    };
    let (found, log) = run(recipe());
    expect(
        &found,
        &log,
        "the recipe without the lockdown",
        &[
            ("desktop-name", true, None),
            ("input-desktop", false, Some(1)),
            ("winsta0", false, Some(5)),
            ("windows", true, Some(0)),
        ],
    );
    assert!(
        probe(&found, "desktop-name", &log)
            .detail
            .starts_with("tapestry-media-"),
        "{log}"
    );
    // Controls, each only where this process can do it itself.
    // SAFETY: opens the input desktop for reading, closed here.
    let input = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) };
    if let Ok(input) = input {
        // SAFETY: the desktop just opened.
        let _ = unsafe { CloseDesktop(input) };
        let (found, log) = run(recipe().without(Restrictions::OWN_DESKTOP));
        expect(
            &found,
            &log,
            "without its own desktop",
            &[("desktop-name", true, None), ("input-desktop", true, None)],
        );
        assert_eq!(probe(&found, "desktop-name", &log).detail, "Default");
    } else {
        eprintln!("this session has no input desktop to open: its control is not run");
    }
    // SAFETY: opens `WinSta0` to read it, closed here.
    let station =
        unsafe { OpenWindowStationW(windows::core::w!("WinSta0"), false, 0x0001 | 0x0002) };
    if let Ok(station) = station {
        // SAFETY: the window station just opened.
        let _ = unsafe { CloseWindowStation(station) };
        let (found, log) = run(recipe().without(Restrictions::APP_CONTAINER));
        expect(
            &found,
            &log,
            "without the AppContainer",
            &[("winsta0", true, None)],
        );
    } else {
        eprintln!("this session cannot open WinSta0: its control is not run");
    }
    // No single restriction shows the windows: the unconfined worker sees
    // this process's.
    let (found, log) = run(recipe()
        .applying(Restrictions::NONE)
        .expecting(Restrictions::NONE));
    let windows = probe(&found, "windows", &log);
    assert!(windows.code > 0 && windows.detail == "seen=1", "{log}");
    // SAFETY: the window made above.
    let _ = unsafe { DestroyWindow(window) };
}

/// `bogus-handle` without strict handle checks or the AppContainer.
#[cfg(feature = "dev-tools")]
fn run_bogus(stage: &Stage) -> (BTreeMap<String, Probe>, String) {
    let (_worker, found, log) = probe_run(
        stage,
        recipe()
            .without(Restrictions::STRICT_HANDLES)
            .without(Restrictions::APP_CONTAINER),
        &["bogus-handle"],
    );
    (found, log)
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_win32k_is_locked_from_creation() {
    let stage = Stage::new();
    let (worker, found, log) = probe_run(&stage, recipe(), &["win32k", "win32k-raw"]);
    // The launch's check read the lockdown before the worker ran; it holds.
    assert_eq!(
        policy(worker.process(), ProcessSystemCallDisablePolicy) & 1,
        1
    );
    // USER32, loaded first, fails its initialization
    // (`ERROR_DLL_INIT_FAILED`), and GDI32 after it with 8; win32u's system
    // calls are refused without making the thread a GUI thread.
    expect(
        &found,
        &log,
        "the recipe",
        &[
            ("user32", false, Some(1_114)),
            ("gdi32", false, Some(8)),
            ("win32u", true, None),
            ("raw-user", false, None),
            ("raw-gdi", false, None),
        ],
    );
    for raw in ["raw-user", "raw-gdi"] {
        assert!(
            probe(&found, raw, &log).detail.ends_with("after=0"),
            "{raw}: {log}"
        );
    }
    drop(worker);
    // Negative control of the check: a launch that leaves the lockdown out
    // while the check expects it is terminated before it runs.
    let Err(e) =
        stage.launch(&recipe().applying(Restrictions::RECIPE.without(Restrictions::NO_WIN32K)))
    else {
        panic!("a launch without the lockdown passed its check");
    };
    assert_eq!((e.step, e.exit), (Step::Check, Some(REFUSED)), "{e}");
    // Control of the calls: with the lockdown and its check removed, the
    // same loads and calls succeed.
    let (_worker, found, log) = probe_run(
        &stage,
        recipe().without(Restrictions::NO_WIN32K),
        &["win32k", "win32k-raw"],
    );
    expect(
        &found,
        &log,
        "without the lockdown",
        &[
            ("user32", true, None),
            ("gdi32", true, None),
            ("gdi-call", true, None),
            ("user-call", true, None),
            ("raw-user", true, None),
            ("raw-gdi", true, None),
        ],
    );
}
