//! The media worker's sandbox (design.md "Threat Model" T02): the real
//! `tapestry-media.exe`, launched by the game's host as the game launches
//! it, from the copies the launch stages of it and its FFmpeg DLLs. Each denial
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
    Kind, LaunchError, Launched, Made, REFUSED, Restrictions, Setup, Stage, Step, change_shared,
    granted_all, media_host, policy, recipe, revoked, token_facts,
};
#[cfg(feature = "dev-tools")]
use media_support::{Probe, probe, probes};
use skin_model::media::control::{Command, FILE_ACCESS, Reply};
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

/// Waits up to `limit` for the process `process` to end: its exit code,
/// once it has.
fn exit_of(process: HANDLE, limit: Duration) -> Option<u32> {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

    let ms = u32::try_from(limit.as_millis()).unwrap();
    // SAFETY: a process handle with the right to wait on it.
    if unsafe { WaitForSingleObject(process, ms) } != WAIT_OBJECT_0 {
        return None;
    }
    let mut code = 0;
    // SAFETY: as above, with the right to query it.
    unsafe { GetExitCodeProcess(process, &raw mut code) }.unwrap();
    Some(code)
}

/// Asserts that the process the failed launch `e` made ends with
/// `REFUSED`: as the launch saw, or, when it stopped watching first, within
/// a minute on the handle it kept. One that does not is terminated before
/// the assertion fails, so it outlives no test.
fn ends_refused(e: &LaunchError) {
    use windows::Win32::System::Threading::TerminateProcess;

    match e.process {
        Made::Ended(code) => assert_eq!((code, e.unended.is_none()), (Some(REFUSED), true), "{e}"),
        Made::Pending => {
            let kept = e
                .unended
                .as_ref()
                .unwrap_or_else(|| panic!("no process kept: {e}"));
            let process = HANDLE(kept.as_raw_handle());
            let code = exit_of(process, Duration::from_secs(60));
            if code.is_none() {
                // SAFETY: the process the launch kept.
                let _ = unsafe { TerminateProcess(process, 1) };
            }
            assert_eq!(code, Some(REFUSED), "it did not end: {e}");
        }
        other => panic!("{other:?}: {e}"),
    }
}

// Covers: T02
#[test]
fn worker_starts_confined_before_it_runs() {
    let stage = Stage::new();
    let worker = launch(&stage, recipe());
    let process = worker.process();
    // The launch checked these before the worker ran (else it fails);
    // they hold, read here from the running worker. It is in the job the
    // launch made, not only in one (the test runner's own holds it too).
    let job = worker.job();
    assert!(job.is_some(), "the launch made no job");
    let mut inside = windows::core::BOOL(0);
    // SAFETY: the worker's process and its job.
    unsafe { IsProcessInJob(process, job, &raw mut inside) }.unwrap();
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
    // The open's reply, and the rejection that answers it with `code`.
    let open = |worker: &mut Launched, player: u8, value: u64, code: u32| {
        let id = worker
            .send(&Command::Open {
                player,
                file: value,
            })
            .unwrap();
        let rejected = Reply::Rejected {
            id,
            player,
            code: code.cast_signed(),
        };
        (reply(worker), rejected)
    };
    let first = worker.hand_in(handle(0), Kind::File).unwrap();
    assert!(
        matches!(
            open(&mut worker, 1, first, 0).0,
            Reply::Probed { player: 1, .. }
        ),
        "the worker's log:\n{}",
        worker.log()
    );
    // Another player's file (`ERROR_BUSY`).
    let (got, rejected) = open(&mut worker, 2, first, 170);
    assert_eq!(got, rejected);
    // A ring whose section another player holds.
    let ring = Ring::new(320, 240);
    let held_section = worker.hand_in(ring.section(), Kind::Section).unwrap();
    let held_event = worker.hand_in(ring.event(), Kind::Event).unwrap();
    let play = worker
        .send(&Command::Play {
            player: 1,
            section: held_section,
            event: held_event,
        })
        .unwrap();
    assert_eq!(
        reply(&worker),
        Reply::Playing {
            id: play,
            player: 1
        }
    );
    let second = worker.hand_in(handle(1), Kind::File).unwrap();
    let probed = open(&mut worker, 3, second, 0).0;
    assert!(
        matches!(probed, Reply::Probed { player: 3, .. }),
        "{probed:?}; the worker's log:\n{}",
        worker.log()
    );
    let other_event = worker.hand_in(ring.event(), Kind::Event).unwrap();
    let play = worker
        .send(&Command::Play {
            player: 3,
            section: held_section,
            event: other_event,
        })
        .unwrap();
    assert_eq!(
        reply(&worker),
        Reply::Rejected {
            id: play,
            player: 3,
            code: 170
        }
    );
    // A closed player's file: no handle of that value is left
    // (`ERROR_INVALID_HANDLE`).
    worker.send(&Command::Close { player: 1 }).unwrap();
    loop {
        match reply(&worker) {
            Reply::Closed { player: 1, .. } => break,
            Reply::Ended { player: 1, .. } | Reply::Failed { player: 1, .. } => {}
            other => panic!("{other:?} while player 1 closes"),
        }
    }
    let (got, rejected) = open(&mut worker, 4, first, 6);
    assert_eq!(got, rejected);
    // A file handed with more than read access (`ERROR_ACCESS_DENIED`).
    let wide = worker
        .hand_in_with(handle(2), FILE_ACCESS | 0x0080)
        .unwrap();
    let (got, rejected) = open(&mut worker, 5, wide, 5);
    assert_eq!(got, rejected);
    // An event named as a file.
    let event_value = worker.hand_in(ring.event(), Kind::Event).unwrap();
    let (got, rejected) = open(&mut worker, 6, event_value, 6);
    assert_eq!(got, rejected);
    // Control: a file handed as the game hands one.
    let fresh = worker.hand_in(handle(3), Kind::File).unwrap();
    let probed = open(&mut worker, 7, fresh, 0).0;
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
        // A process made is terminated, and ends: before it ran, but at the
        // handshake, which it runs for. At `Assign` it is in no job, so its
        // termination alone ends it.
        let made = matches!(
            step,
            Step::Assign | Step::Check | Step::Watch | Step::Resume | Step::Handshake
        );
        if made {
            ends_refused(&e);
        } else {
            assert_eq!(
                (e.process, e.unended.is_none()),
                (Made::Nothing, true),
                "{step:?}: {e}"
            );
        }
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
        assert_eq!(e.step, Step::Check, "{missing:?}: {e}");
        ends_refused(&e);
        assert!(e.what.contains(named), "{missing:?}: {e}");
    }
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_launch_fails_closed_without_a_hello_in_time() {
    let stage = Stage::new();
    // A launch first, so the one timed finds the profile and the copies
    // made and spends its time at the handshake.
    drop(launch(&stage, recipe()));
    let started = Instant::now();
    // The recipe's own limit, 2 s, not the other tests' patient one.
    let Err(e) = stage.launch(&Setup::recipe().args_of(&["--dev-silent", "10000"])) else {
        panic!("a silent worker launched");
    };
    let took = started.elapsed();
    assert_eq!((e.step, e.code), (Step::Handshake, 1460), "{e}");
    ends_refused(&e);
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
fn a_launch_that_stops_watching_keeps_the_process() {
    let stage = Stage::new();
    // Watching for no time, a refused launch that made a process sees it
    // end or keeps it, and a kept one ends; some are kept, since a process
    // takes a moment to end once terminated.
    let mut kept = 0;
    for _ in 0..5 {
        for step in [Step::Assign, Step::Check, Step::Resume, Step::Handshake] {
            let setup = recipe().failing_at(step).observing(Duration::ZERO);
            let Err(e) = stage.launch(&setup) else {
                panic!("{step:?} failing, the launch succeeded");
            };
            assert_eq!(e.step, step, "{e}");
            kept += usize::from(e.process == Made::Pending);
            ends_refused(&e);
        }
    }
    assert!(kept > 0, "every process had ended before the launch looked");
}

/// The game's host and the worker each compile a copy of the handle
/// table's reader (the two binaries share no library, and `check-arch`'s
/// R6 refuses a `#[path]` module): the copies are one file.
#[test]
fn host_and_worker_read_handle_tables_alike() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let host = std::fs::read(src.join("media_host/handles.rs")).unwrap();
    let worker = std::fs::read(src.join("bin/tapestry-media/handles.rs")).unwrap();
    assert!(host == worker, "the two copies of handles.rs differ");
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
/// maker may give a new file one no higher than its own) and a DACL that
/// grants the user full access and the stage's AppContainer read and
/// execute, as the worker's copies do, so only the label can refuse it.
#[cfg(feature = "dev-tools")]
fn low_label_dll(dir: &std::path::Path, stage: &Stage) -> std::path::PathBuf {
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
    let sddl: Vec<u16> = format!(
        // 0x1200a9: `FILE_GENERIC_READ | FILE_GENERIC_EXECUTE`.
        "D:P(A;;FA;;;{})(A;;0x1200a9;;;{})S:(ML;;NW;;;LW)",
        media_host::launch::user_sid().unwrap(),
        sid_text(ContainerSid::of(stage).0)
    )
    .encode_utf16()
    .chain([0])
    .collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a nul-terminated SDDL string of that DACL and a low label that
    // refuses writes from below; the descriptor is left to the test's end.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
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
    let dlls = tempfile::tempdir().unwrap();
    let dll = low_label_dll(dlls.path(), &stage);
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
            // By the name this test gave it, and by its NT path.
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
    assert_eq!(e.step, Step::Check, "{e}");
    ends_refused(&e);
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

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_environment_is_only_the_block_the_launch_builds() {
    let stage = Stage::new();
    let worker = launch(&stage, recipe().args_of(&["--dev-probe", "environment"]));
    let log = worker.probed(1);
    let found = probes(&log);
    let environment = probe(&found, "environment", &log);
    // The game's own environment, which has more, does not reach it: the
    // worker has the Windows directory, and the AppContainer's own folder
    // as `LOCALAPPDATA` and its `Temp` as `TEMP` and `TMP`, which Windows
    // sets from the block's `LOCALAPPDATA`.
    assert!(std::env::var_os("PATH").is_some());
    let windows = std::env::var("SystemRoot").unwrap().to_lowercase();
    let detail = environment.detail.to_lowercase();
    let (allowed, listed) = detail.split_once(' ').unwrap_or((&detail, ""));
    let vars: Vec<(&str, &str)> = listed
        .split(';')
        .map(|v| v.split_once('=').unwrap_or((v, "")))
        .collect();
    let names: Vec<&str> = vars.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        (environment.ok, environment.code, allowed, names),
        (
            true,
            4,
            "allowed=1",
            vec!["localappdata", "systemroot", "temp", "tmp"]
        ),
        "{log}"
    );
    let folder = vars[0].1;
    let own = format!("\\packages\\{}\\ac", stage.profile());
    assert!(
        folder.ends_with(&own) && !folder[..folder.len() - own.len()].contains("\\packages\\"),
        "{log}"
    );
    let temp = format!("{folder}\\temp");
    assert_eq!(
        (vars[1].1, vars[2].1, vars[3].1),
        (windows.as_str(), temp.as_str(), temp.as_str()),
        "{log}"
    );
}

/// `path`, nul-terminated.
fn wide_path(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;

    path.as_os_str().encode_wide().chain([0]).collect()
}

/// The DACL of the file or directory at `path`, as SDDL.
fn dacl_of(path: &std::path::Path) -> String {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
    use windows::core::PWSTR;

    let name = wide_path(path);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a nul-terminated path; the descriptor returned is freed below.
    let read = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &raw mut descriptor,
        )
    };
    assert!(read.is_ok(), "{}: {read:?}", path.display());
    let mut text = PWSTR::null();
    // SAFETY: the descriptor just read; the string returned is freed below.
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &raw mut text,
            None,
        )
    }
    .unwrap();
    // SAFETY: the nul-terminated string the call returned.
    let sddl = unsafe { text.to_string() }.unwrap();
    // SAFETY: the string and the descriptor the calls allocated, unused
    // after this.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    sddl
}

/// Adds the ACE `ace` (SDDL) to the DACL of the file or directory at
/// `path`, as the user may.
fn widen(path: &std::path::Path, ace: &str) {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetNamedSecurityInfoW,
    };
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    let sddl: Vec<u16> = format!("{}{ace}", dacl_of(path))
        .encode_utf16()
        .chain([0])
        .collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a nul-terminated SDDL string; the descriptor returned is freed
    // below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    }
    .unwrap();
    let (mut present, mut defaulted) = (false.into(), false.into());
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: the descriptor just made, whose DACL lies inside it.
    unsafe {
        GetSecurityDescriptorDacl(
            descriptor,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    }
    .unwrap();
    let name = wide_path(path);
    // SAFETY: a nul-terminated path and the DACL above.
    let set = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
    };
    // SAFETY: the descriptor made above, unused after this.
    let _ = unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
    assert!(set.is_ok(), "{}: {set:?}", path.display());
}

/// How many names the file at `path` has.
fn links(path: &std::path::Path) -> u32 {
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let file = std::fs::File::open(path).unwrap();
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: an open handle, and a struct the call fills.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &raw mut info) }.unwrap();
    info.nNumberOfLinks
}

/// `sid` as text.
fn sid_text(sid: windows::Win32::Security::PSID) -> String {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::core::PWSTR;

    let mut text = PWSTR::null();
    // SAFETY: a valid SID; the string returned is freed below.
    unsafe { ConvertSidToStringSidW(sid, &raw mut text) }.unwrap();
    // SAFETY: the nul-terminated string the call returned.
    let copy = unsafe { text.to_string() }.unwrap();
    // SAFETY: as above, unused after this.
    let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    copy
}

// Covers: T02
#[test]
fn launch_writes_nothing_to_the_installation() {
    let stage = Stage::new();
    let installed: Vec<std::path::PathBuf> = std::iter::once(stage.dir().to_path_buf())
        .chain(media_host::stage::files().map(|f| stage.dir().join(f)))
        .collect();
    let before: Vec<String> = installed.iter().map(|f| dacl_of(f)).collect();
    let worker = launch(&stage, recipe());
    let after: Vec<String> = installed.iter().map(|f| dacl_of(f)).collect();
    drop(worker);
    // The installation's DACLs are as they were, none naming the worker's
    // AppContainer; its copies' directory is granted to it instead.
    assert_eq!(after, before);
    let sid = sid_text(ContainerSid::of(&stage).0);
    assert!(before.iter().all(|d| !d.contains(&sid)), "{before:?}");
    assert!(dacl_of(&stage.copies()).contains(&sid));
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_reads_its_copies_and_no_file_of_the_installation() {
    let stage = Stage::new();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "the user's").unwrap();
    // A name linked into the installation, and, once the copies are staged,
    // one linked into their directory.
    std::fs::hard_link(&secret, stage.dir().join("linked.txt")).unwrap();
    drop(launch(&stage, recipe()));
    let copies = stage.copies();
    std::fs::hard_link(&secret, copies.join("linked.txt")).unwrap();
    for (path, readable) in [
        (copies.join("tapestry-media.exe"), true),
        (stage.dir().join("tapestry-media.exe"), false),
        (stage.dir().join("linked.txt"), false),
        (copies.join("linked.txt"), false),
        (secret.clone(), false),
    ] {
        let arg = format!("file={}", path.display());
        let worker = launch(&stage, recipe().args_of(&["--dev-probe", &arg]));
        let log = worker.probed(1);
        let found = probes(&log);
        let file = probe(&found, "file", &log);
        let want = if readable { (true, 0) } else { (false, 5) };
        assert_eq!((file.ok, file.code), want, "{}: {log}", path.display());
    }
}

// Covers: T02
#[test]
fn a_changed_copy_is_written_again_before_the_launch() {
    use std::os::windows::fs::OpenOptionsExt as _;

    let stage = Stage::new();
    drop(launch(&stage, recipe()));
    let copies = stage.copies();
    let dlls = media_support::dlls();
    let [first, second, third] = [0, 1, 2].map(|i| format!("{}.dll", dlls[i]));
    let installed = |name: &str| std::fs::read(stage.dir().join(name)).unwrap();
    let dacl = dacl_of(&copies.join(&third));
    // One copy's bytes changed (one byte, so its size stays), another's DACL
    // widened, and the executable's copy given a second name.
    let mut bytes = std::fs::read(copies.join(&first)).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(copies.join(&first), &bytes).unwrap();
    widen(&copies.join(&second), "(A;;FR;;;WD)");
    let elsewhere = tempfile::tempdir_in(stage.data()).unwrap();
    std::fs::hard_link(
        copies.join("tapestry-media.exe"),
        elsewhere.path().join("again.exe"),
    )
    .unwrap();
    drop(launch(&stage, recipe()));
    for name in [first.as_str(), second.as_str(), "tapestry-media.exe"] {
        let copy = copies.join(name);
        assert!(std::fs::read(&copy).unwrap() == installed(name), "{name}");
        assert_eq!((dacl_of(&copy), links(&copy)), (dacl.clone(), 1), "{name}");
    }
    // A changed copy that cannot be written again, here held open, refuses
    // the launch before it makes a process; released, it is written again.
    std::fs::write(copies.join(&first), &bytes).unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0)
        .open(copies.join(&first))
        .unwrap();
    let Err(e) = stage.launch(&recipe()) else {
        panic!("a launch ran over a copy it could not write again");
    };
    assert_eq!((e.step, e.process), (Step::Stage, Made::Nothing), "{e}");
    drop(held);
    drop(launch(&stage, recipe()));
    assert!(std::fs::read(copies.join(&first)).unwrap() == installed(&first));
}

// Covers: T02
#[test]
fn a_copies_directory_with_another_dacl_refuses_the_launch() {
    let stage = Stage::new();
    drop(launch(&stage, recipe()));
    widen(&stage.copies(), "(A;;FR;;;WD)");
    let Err(e) = stage.launch(&recipe()) else {
        panic!("a launch ran from a directory with another DACL");
    };
    assert_eq!((e.step, e.process), (Step::Stage, Made::Nothing), "{e}");
    assert!(e.what.contains("another DACL"), "{e}");
}

// Covers: T02
#[test]
fn a_copies_directory_that_is_a_link_refuses_the_launch() {
    let stage = Stage::new();
    drop(launch(&stage, recipe()));
    // The copies' directory moved aside, and a junction to it put at its
    // path, as the user may.
    let copies = stage.copies();
    let aside = copies.with_extension("aside");
    std::fs::rename(&copies, &aside).unwrap();
    let status = std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&copies)
        .arg(&aside)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let Err(e) = stage.launch(&recipe()) else {
        panic!("a launch ran from a link");
    };
    assert_eq!((e.step, e.process), (Step::Stage, Made::Nothing), "{e}");
    assert!(e.what.contains("is a link"), "{e}");
}

// Covers: T02
#[test]
fn held_copies_cannot_be_written_or_renamed() {
    let stage = Stage::new();
    let sid = ContainerSid::of(&stage);
    let staged = media_host::stage::stage(stage.dir(), stage.data(), sid.0).unwrap();
    let copies = staged.dir().to_path_buf();
    let exe = copies.join("tapestry-media.exe");
    let media = stage.data().join("media");
    // While the launch holds them, a copy cannot be written, renamed or
    // deleted, nor its directory or the folder above renamed.
    let refused = |what: &str, result: std::io::Result<()>| {
        let code = result.err().and_then(|e| e.raw_os_error());
        assert!(matches!(code, Some(5 | 32)), "{what}: {code:?}");
    };
    refused(
        "writing a copy",
        std::fs::OpenOptions::new().write(true).open(&exe).map(drop),
    );
    refused(
        "renaming a copy",
        std::fs::rename(&exe, copies.join("other.exe")),
    );
    refused("deleting a copy", std::fs::remove_file(&exe));
    refused(
        "renaming the directory",
        std::fs::rename(&copies, copies.with_extension("moved")),
    );
    refused(
        "renaming the folder above",
        std::fs::rename(&media, media.with_extension("moved")),
    );
    // Released, they can.
    drop(staged);
    std::fs::rename(&media, media.with_extension("moved")).unwrap();
}

// Covers: T02
#[test]
fn launches_take_a_mutex_of_the_users_own() {
    use std::os::windows::io::FromRawHandle as _;

    use media_host::launch::Launches;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};

    // A holder that ends without releasing it leaves it abandoned, and the
    // next takes it all the same.
    std::thread::spawn(|| std::mem::forget(Launches::hold().unwrap()))
        .join()
        .unwrap();
    let held = Launches::hold().unwrap();
    // It is the user's, in the global namespace, so the user's launches in
    // every session take it, and its DACL, protected, grants the user alone.
    let user = media_host::launch::user_sid().unwrap();
    let name: Vec<u16> = format!("Global\\tapestry-media-{user}")
        .encode_utf16()
        .chain([0])
        .collect();
    // SAFETY: a nul-terminated name; the handle is owned below.
    let opened =
        unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) }.unwrap();
    // SAFETY: the handle just opened, closed when this drops.
    drop(unsafe { OwnedHandle::from_raw_handle(opened.0) });
    // Compared as Windows writes it: SDDL names some accounts by an alias
    // (the built-in Administrator, which runs CI, reads `LA`).
    assert_eq!(
        kernel_dacl(held.handle()),
        canonical_sddl(&format!("D:P(A;;0x1f0001;;;{user})"))
    );
}

/// `sddl` as Windows writes it back, aliases and all.
fn canonical_sddl(sddl: &str) -> String {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW,
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
    use windows::core::PWSTR;

    let wide: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a nul-terminated string; the descriptor returned is freed below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wide.as_ptr()),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    }
    .unwrap();
    let mut text = PWSTR::null();
    // SAFETY: the descriptor just made; the string returned is freed below.
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &raw mut text,
            None,
        )
    }
    .unwrap();
    // SAFETY: the nul-terminated string the call returned.
    let written = unsafe { text.to_string() }.unwrap();
    // SAFETY: the string and the descriptor the calls allocated.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    written
}

/// The DACL of the kernel object `object`, as SDDL.
fn kernel_dacl(object: HANDLE) -> String {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_KERNEL_OBJECT,
    };
    use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
    use windows::core::PWSTR;

    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a handle with `READ_CONTROL`; the descriptor returned is freed
    // below.
    let read = unsafe {
        GetSecurityInfo(
            object,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            Some(&raw mut descriptor),
        )
    };
    assert!(read.is_ok(), "{read:?}");
    let mut text = PWSTR::null();
    // SAFETY: the descriptor just read; the string returned is freed below.
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &raw mut text,
            None,
        )
    }
    .unwrap();
    // SAFETY: the nul-terminated string the call returned.
    let sddl = unsafe { text.to_string() }.unwrap();
    // SAFETY: the string and the descriptor the calls allocated.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    sddl
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_replies_past_the_queue_fail_the_worker() {
    let stage = Stage::new();
    // More replies than the pipe holds, which the game does not read,
    // from the worker's first command on.
    let mut worker = launch(&stage, recipe().args_of(&["--dev-flood", "1000000"]));
    worker.send(&Command::Close { player: 0 }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !worker.overflowed() {
        assert!(
            Instant::now() < deadline,
            "the replies never overflowed; the worker's log:\n{}",
            worker.log()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // The queue held its capacity, and the replies have ended.
    let mut queued = 0;
    while worker.try_recv().is_some() {
        queued += 1;
    }
    assert_eq!(queued, 64);
    assert_eq!(
        worker.recv(Duration::from_secs(5)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    );
    // The pipe it replies on closed, its next reply ends the worker.
    assert!(
        worker.wait_exit(Duration::from_secs(10)).is_some(),
        "the worker went on"
    );
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn dropping_a_worker_never_waits_on_it() {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let stage = Stage::new();
    let mut launched = launch(&stage, recipe().args_of(&["--dev-deaf"]));
    // The worker reads none of its commands: write until a write waits.
    let mut pipe = launched.commands_again();
    let written = Arc::new(AtomicUsize::new(0));
    let filler = {
        let written = Arc::clone(&written);
        std::thread::spawn(move || {
            let chunk = [0_u8; 4096];
            while pipe.write_all(&chunk).is_ok() {
                written.fetch_add(chunk.len(), Ordering::Relaxed);
            }
        })
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = (0, Instant::now());
    loop {
        let now = written.load(Ordering::Relaxed);
        if now != last.0 {
            last = (now, Instant::now());
        } else if now > 0 && last.1.elapsed() > Duration::from_millis(300) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the pipe never filled: {now} bytes"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Dropping it returns at once, though the pipe is full.
    let worker = launched.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        drop(worker);
        let _ = tx.send(started.elapsed());
    });
    let took = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("dropping the worker waited on it");
    assert!(
        took < Duration::from_secs(1),
        "dropping the worker took {took:?}"
    );
    // Its job closes after the grace, whatever the pipe does: the worker
    // ends, and the waiting write fails.
    assert!(
        launched.wait_exit(Duration::from_secs(10)).is_some(),
        "the worker went on"
    );
    filler.join().unwrap();
}

// Covers: T02
#[test]
fn worker_job_reads_back_exactly_its_limits() {
    use windows::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS,
        JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES,
        JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
        JOB_OBJECT_UILIMIT_WRITECLIPBOARD, JOBOBJECT_BASIC_UI_RESTRICTIONS,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
        JobObjectExtendedLimitInformation, QueryInformationJobObject,
    };

    let stage = Stage::new();
    let worker = launch(&stage, recipe());
    let job = worker.job().expect("the recipe's job");
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: the worker's job, and a struct of the size given.
    unsafe {
        QueryInformationJobObject(
            Some(job),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            u32::try_from(size_of_val(&limits)).unwrap(),
            None,
        )
    }
    .unwrap();
    // The memory cap, the end on the job's closing and on an unhandled
    // exception, one process, and no breakaway.
    let basic = limits.BasicLimitInformation;
    assert_eq!(
        basic.LimitFlags,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
    );
    assert_eq!(
        (limits.ProcessMemoryLimit, basic.ActiveProcessLimit),
        (512 << 20, 1)
    );
    let mut ui = JOBOBJECT_BASIC_UI_RESTRICTIONS::default();
    // SAFETY: as above.
    unsafe {
        QueryInformationJobObject(
            Some(job),
            JobObjectBasicUIRestrictions,
            (&raw mut ui).cast(),
            u32::try_from(size_of_val(&ui)).unwrap(),
            None,
        )
    }
    .unwrap();
    assert_eq!(
        ui.UIRestrictionsClass,
        JOB_OBJECT_UILIMIT_DESKTOP
            | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
            | JOB_OBJECT_UILIMIT_EXITWINDOWS
            | JOB_OBJECT_UILIMIT_GLOBALATOMS
            | JOB_OBJECT_UILIMIT_HANDLES
            | JOB_OBJECT_UILIMIT_READCLIPBOARD
            | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
            | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
    );
}

/// An AppContainer's SID, derived from its name, freed when dropped.
struct ContainerSid(windows::Win32::Security::PSID);

impl ContainerSid {
    /// The SID of the AppContainer `name`, which needs no profile.
    fn named(name: &str) -> Self {
        let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
        // SAFETY: a nul-terminated name; the SID returned is freed on drop.
        let sid = unsafe {
            windows::Win32::Security::Isolation::DeriveAppContainerSidFromAppContainerName(PCWSTR(
                name.as_ptr(),
            ))
        }
        .unwrap();
        Self(sid)
    }

    /// The SID the stage's workers run as.
    fn of(stage: &Stage) -> Self {
        Self::named(stage.profile())
    }
}

impl Drop for ContainerSid {
    fn drop(&mut self) {
        // SAFETY: a SID the derivation allocated, unused after this.
        let _ = unsafe { windows::Win32::Security::FreeSid(self.0) };
    }
}

/// Whether the DACL of the window object `object` holds an allowed ACE for
/// `sid`. A read that another process's change makes fail
/// (`ERROR_INSUFFICIENT_BUFFER`: the DACL grew between the call's two
/// reads) is made again.
fn names(object: HANDLE, sid: windows::Win32::Security::PSID) -> bool {
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_WINDOW_OBJECT};
    use windows::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, PSECURITY_DESCRIPTOR,
    };
    use windows::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    for _ in 0..100 {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a window object this process holds; the descriptor
        // returned is freed below.
        let read = unsafe {
            GetSecurityInfo(
                object,
                SE_WINDOW_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&raw mut dacl),
                None,
                Some(&raw mut descriptor),
            )
        };
        if read == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        assert!(read.is_ok(), "the DACL's read: {read:?}");
        let mut size = ACL_SIZE_INFORMATION::default();
        // SAFETY: the DACL just read, and a struct of the size given.
        unsafe {
            GetAclInformation(
                dacl,
                (&raw mut size).cast(),
                u32::try_from(size_of_val(&size)).unwrap(),
                AclSizeInformation,
            )
        }
        .unwrap();
        let found = (0..size.AceCount).any(|i| {
            let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: ACE `i` of the DACL, which holds `AceCount` of them.
            unsafe { GetAce(dacl, i, &raw mut ace) }.unwrap();
            // SAFETY: each ACE starts with its header; an allowed ACE has
            // its SID from `SidStart`.
            unsafe {
                let header = ace.cast::<ACE_HEADER>().read_unaligned();
                u32::from(header.AceType) == ACCESS_ALLOWED_ACE_TYPE
                    && EqualSid(
                        windows::Win32::Security::PSID(
                            (&raw mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast(),
                        ),
                        sid,
                    )
                    .is_ok()
            }
        });
        // SAFETY: the descriptor the read allocated, unused after this.
        let _ = unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
        return found;
    }
    panic!("the DACL kept changing while it was read");
}

/// The name of the window station `station`.
fn station_name(station: HANDLE) -> String {
    use windows::Win32::System::StationsAndDesktops::{GetUserObjectInformationW, UOI_NAME};

    let mut name = [0_u16; 256];
    // SAFETY: a window station this process holds, and a buffer of the
    // size given.
    unsafe {
        GetUserObjectInformationW(
            station,
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            u32::try_from(size_of_val(&name)).unwrap(),
            None,
        )
    }
    .unwrap();
    let end = name.iter().position(|&c| c == 0).unwrap();
    String::from_utf16(&name[..end]).unwrap()
}

// Covers: T02
#[test]
fn launch_grants_its_desktop_and_leaves_the_shared_station_alone() {
    let stage = Stage::new();
    let sid = ContainerSid::of(&stage);
    // Under the recipe the worker's AppContainer is granted its own desktop
    // and nothing on the window station, the session's alternate one, which
    // every launch and other programs' sandboxes share.
    let worker = launch(&stage, recipe());
    let objects = worker.desktop().expect("the recipe made no desktop");
    assert!(
        names(objects.desktop(), sid.0),
        "the desktop is not granted"
    );
    assert!(
        !names(objects.station(), sid.0),
        "the recipe granted the shared window station"
    );
    // A test's setup without the lockdown, whose worker loads USER32 and so
    // connects to the station, grants the station while its workers live,
    // and revokes the grant when the last of them drops.
    let unlocked = launch(&stage, recipe().without(Restrictions::NO_WIN32K));
    let station = unlocked.desktop().unwrap().station();
    assert_eq!(station_name(station), station_name(objects.station()));
    assert!(
        names(objects.station(), sid.0),
        "the setup without the lockdown did not grant the window station"
    );
    let second = launch(&stage, recipe().without(Restrictions::NO_WIN32K));
    drop(unlocked);
    assert!(
        names(objects.station(), sid.0),
        "a worker's drop revoked the grant another worker of its AppContainer uses"
    );
    drop(second);
    assert!(
        !names(objects.station(), sid.0),
        "the setup's grant of the window station outlived its workers"
    );
}

// Covers: T02
#[test]
fn shared_dacl_changes_at_once_lose_none() {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const THREADS: usize = 8;
    const ROUNDS: usize = 50;
    // The tests' grants and revocations of the shared window station, many
    // at once, here on a worker's own desktop, which nothing else changes:
    // each read back holds, and none is undone by another.
    let stage = Stage::new();
    let worker = launch(&stage, recipe());
    let desktop = value(worker.desktop().unwrap().desktop());
    let lost = AtomicUsize::new(0);
    let start = Barrier::new(THREADS);
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let (lost, start) = (&lost, &start);
            s.spawn(move || {
                let sid = ContainerSid::named(&format!("tapestry.media.dacl-test.{t}"));
                let desktop = HANDLE(std::ptr::with_exposed_provenance_mut(
                    usize::try_from(desktop).unwrap(),
                ));
                start.wait();
                for _ in 0..ROUNDS {
                    change_shared(desktop, granted_all(sid.0), "the desktop").unwrap();
                    if !names(desktop, sid.0) {
                        lost.fetch_add(1, Ordering::Relaxed);
                    }
                    change_shared(desktop, revoked(sid.0), "the desktop").unwrap();
                    if names(desktop, sid.0) {
                        lost.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    assert_eq!(lost.into_inner(), 0, "changes were lost");
}
