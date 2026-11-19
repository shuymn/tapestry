//! The media worker `tapestry-media` (design.md "Threat Model" T02;
//! "Libraries", the FFmpeg row), run as the real executable over its
//! control pipe, with each frame ring and its free-slot event made here as
//! the game makes them and handed to the worker by handle, each with only
//! the access it needs. The game does not launch the worker yet, so these
//! tests run it without its sandbox and check what FFmpeg's layer holds:
//! the pinned build, the codec and container allow-list with FFmpeg's codec
//! whitelist, the pixel cap, the refused secondary open, the decoded
//! frames' stamps, the seek's back-off and the zlib-compressed containers.
//!
//! The clips are made by the worker's own `--dev-gen-clip` (dev-tools), so
//! those tests need dev-tools; the committed fixtures (`tests/media/`) and
//! the handshake do not.

#![cfg(windows)]

use std::hash::{Hash as _, Hasher as _};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use skin_model::media::control::{self, Command as Order, Reply};
use skin_model::media::{Frame, Geometry, Reader, Window};
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcess, SetEvent};
use windows::core::PCWSTR;

const WORKER: &str = env!("CARGO_BIN_EXE_tapestry-media");

/// How long a reply may take.
const REPLY: Duration = Duration::from_secs(30);

/// Read access to a file (`FILE_GENERIC_READ`).
const FILE_READ: u32 = 0x0012_0089;
/// Mapping a section for reading and writing (`SECTION_MAP_READ |
/// SECTION_MAP_WRITE`).
const SECTION_READ_WRITE: u32 = 0x0004 | 0x0002;
/// Waiting on an event (`SYNCHRONIZE`).
const WAIT: u32 = 0x0010_0000;

/// The workspace root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A running worker and the replies on its control pipe.
struct Worker {
    child: Child,
    stdin: ChildStdin,
    replies: Receiver<Reply>,
    log: Arc<Mutex<String>>,
    hello: Reply,
}

impl Worker {
    /// Starts the worker with `args` and reads its hello.
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(WORKER)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, replies) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let mut prefix = [0; 2];
                if stdout.read_exact(&mut prefix).is_err() {
                    return;
                }
                let len = control::frame_len(prefix).expect("a frame of the protocol");
                let mut message = vec![0; len];
                if stdout.read_exact(&mut message).is_err() {
                    return;
                }
                let reply = Reply::decode(&message).expect("a reply of the protocol");
                if tx.send(reply).is_err() {
                    return;
                }
            }
        });
        let log = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&log);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { return };
                let mut log = sink.lock().unwrap();
                log.push_str(&line);
                log.push('\n');
            }
        });
        let hello = replies.recv_timeout(REPLY).expect("the worker's hello");
        Self {
            child,
            stdin,
            replies,
            log,
            hello,
        }
    }

    fn send(&mut self, order: &Order) {
        let bytes = control::frame(&order.encode()).unwrap();
        self.stdin.write_all(&bytes).unwrap();
        self.stdin.flush().unwrap();
    }

    /// The next reply.
    fn reply(&self) -> Reply {
        match self.replies.recv_timeout(REPLY) {
            Ok(r) => r,
            Err(e) => panic!("no reply ({e}); the worker's log:\n{}", self.log()),
        }
    }

    /// A reply that has come, without waiting.
    fn try_reply(&self) -> Option<Reply> {
        self.replies.try_recv().ok()
    }

    /// What the worker wrote to standard error so far.
    fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    /// A duplicate of `handle` in the worker with `access`, as the game
    /// hands it the handles a command names.
    fn give(&self, handle: HANDLE, access: u32) -> u64 {
        let mut theirs = HANDLE::default();
        // SAFETY: `handle` is open in this process, and the child's process
        // handle has every access.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                handle,
                HANDLE(self.child.as_raw_handle()),
                &raw mut theirs,
                access,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
        }
        .unwrap();
        theirs.0 as u64
    }

    /// The worker's peak commit so far, in bytes.
    #[cfg(feature = "dev-tools")]
    fn peak_commit(&self) -> usize {
        use windows::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };

        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        // SAFETY: the child's process handle, which has every access, and a
        // buffer of the size given.
        unsafe {
            GetProcessMemoryInfo(
                HANDLE(self.child.as_raw_handle()),
                &raw mut counters,
                u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).unwrap(),
            )
        }
        .unwrap();
        counters.PeakPagefileUsage
    }

    /// Opens `path` as player `player`: the probe or the refusal.
    fn open(&mut self, player: u8, path: &Path) -> Reply {
        let file = std::fs::File::open(path).unwrap();
        let handle = self.give(HANDLE(file.as_raw_handle()), FILE_READ);
        self.send(&Order::Open {
            player,
            file: handle,
        });
        self.reply()
    }

    /// Hands player `player` its ring.
    fn play(&mut self, player: u8, ring: &Ring) -> Reply {
        let section = self.give(ring.section, SECTION_READ_WRITE);
        let event = self.give(ring.event, WAIT);
        self.send(&Order::Play {
            player,
            section,
            event,
        });
        self.reply()
    }

    /// Closes player `player`, past any reply of its still coming.
    fn close(&mut self, player: u8) {
        self.send(&Order::Close { player });
        loop {
            match self.reply() {
                Reply::Closed { player: p } if p == player => return,
                Reply::Ended { player: p, .. } if p == player => {}
                other => panic!("{other:?} while player {player} closes"),
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(bytes) = control::frame(&Order::Quit.encode()) {
            let _ = self.stdin.write_all(&bytes);
            let _ = self.stdin.flush();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A frame ring as the game makes it: a section of its own, mapped here,
/// its header written, and its free-slot event.
struct Ring {
    section: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    event: HANDLE,
    reader: Reader,
}

impl Ring {
    /// A ring for `width` × `height` frames, whose first run's stamps start
    /// at 0.
    fn new(width: u32, height: u32) -> Self {
        let geometry = Geometry::new(width, height).unwrap();
        let bytes = u64::try_from(geometry.bytes()).unwrap();
        // SAFETY: a new section backed by the paging file, of the ring's
        // size.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                u32::try_from(bytes >> 32).unwrap(),
                u32::try_from(bytes & 0xffff_ffff).unwrap(),
                PCWSTR::null(),
            )
        }
        .unwrap();
        // SAFETY: the section just made, mapped whole.
        let view = unsafe { MapViewOfFile(section, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        assert!(!view.Value.is_null());
        // SAFETY: an auto-reset event, unsignalled.
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.unwrap();
        let len = geometry.words();
        // SAFETY: the view holds the ring's words and stays mapped while the
        // ring lives; every access goes through the atomics.
        let words = unsafe { std::slice::from_raw_parts(view.Value.cast::<AtomicU64>(), len) };
        let reader = Reader::new(
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
            view,
            len,
            event,
            reader,
        }
    }

    fn words<'a>(view: MEMORY_MAPPED_VIEW_ADDRESS, len: usize) -> &'a [AtomicU64] {
        // SAFETY: as in `new`; the caller keeps the ring alive.
        unsafe { std::slice::from_raw_parts(view.Value.cast::<AtomicU64>(), len) }
    }

    /// Polls the ring at `due`, signalling the worker when slots were
    /// freed: the frame staged, if any.
    fn poll(&mut self, due: i64) -> Option<Frame> {
        let words = Self::words(self.view, self.len);
        let poll = self
            .reader
            .poll(words, due)
            .unwrap_or_else(|f| panic!("the ring failed its player: {f}"));
        if poll.freed {
            // SAFETY: the ring's event.
            unsafe { SetEvent(self.event) }.unwrap();
        }
        poll.staged
    }

    /// Starts the next run, its stamps from `first`: the run's id.
    fn start_run(&mut self, first: i64) -> u64 {
        self.reader.start_run(Window {
            first,
            last: i64::MAX,
        })
    }

    /// Each slot's state, sequence, run and stamp, for a failure's message.
    fn slots(&self) -> Vec<[u64; 4]> {
        let words = Self::words(self.view, self.len);
        let geometry = self.reader.geometry();
        (0..skin_model::media::SLOTS)
            .map(|s| {
                let base = geometry.frame_start(s) - skin_model::media::SLOT_HEADER_WORDS;
                std::array::from_fn(|i| words[base + i].load(std::sync::atomic::Ordering::Relaxed))
            })
            .collect()
    }

    /// The staged frame's bytes, I420.
    fn staged(&self) -> Vec<u8> {
        let (_, words) = self.reader.staged().unwrap();
        let mut bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        bytes.truncate(self.reader.geometry().frame_bytes());
        bytes
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: the view and handles this ring made, unused after this.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.section);
            let _ = CloseHandle(self.event);
        }
    }
}

/// A frame taken from a ring.
struct Taken {
    stamp: i64,
    bytes: Vec<u8>,
}

impl Taken {
    fn hash(&self) -> u64 {
        let mut h = std::hash::DefaultHasher::new();
        self.bytes.hash(&mut h);
        h.finish()
    }
}

/// Takes every frame the worker publishes into `ring` for `player` from
/// `first` µs on, the frames `interval` µs apart, until it answers that the
/// video ended: each poll is due half an interval past the last frame
/// taken, so it stages the next frame and no later one.
fn take_all(worker: &Worker, ring: &mut Ring, player: u8, first: i64, interval: i64) -> Vec<Taken> {
    let mut taken = Vec::new();
    let mut due = first + interval / 2;
    let mut ended = false;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(
            Instant::now() < deadline,
            "{} frames in two minutes, slots {:?}; the worker's log:\n{}",
            taken.len(),
            ring.slots(),
            worker.log()
        );
        if let Some(frame) = ring.poll(due) {
            taken.push(Taken {
                stamp: frame.stamp,
                bytes: ring.staged(),
            });
            due = frame.stamp + interval * 3 / 2;
            continue;
        }
        if ended {
            return taken;
        }
        match worker.try_reply() {
            Some(Reply::Ended { player: p, .. }) if p == player => ended = true,
            Some(other) => panic!("{other:?} while player {player} plays"),
            None => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// Polls `ring` at `due` until a frame is staged.
fn take_one(worker: &Worker, ring: &mut Ring, due: i64) -> Taken {
    let deadline = Instant::now() + REPLY;
    loop {
        if let Some(frame) = ring.poll(due) {
            return Taken {
                stamp: frame.stamp,
                bytes: ring.staged(),
            };
        }
        assert!(
            Instant::now() < deadline,
            "no frame due at {due}, slots {:?}; the worker's log:\n{}",
            ring.slots(),
            worker.log()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The index `--dev-gen-clip` paints in a frame `width` wide: bit `b` in
/// block `b` of the top row of 16 × 16 blocks.
#[cfg(feature = "dev-tools")]
fn painted(bytes: &[u8], width: usize) -> u32 {
    (0..16)
        .take_while(|b| (b + 1) * 16 <= width)
        .map(|b| u32::from(bytes[8 * width + b * 16 + 8] > 128) << b)
        .sum()
}

/// `i` frames at `rate` a second in µs, rounded as FFmpeg rescales.
#[cfg(feature = "dev-tools")]
fn stamp_of(i: i64, rate: i64) -> i64 {
    (i * 1_000_000 + rate / 2) / rate
}

/// Makes a clip with the worker's `--dev-gen-clip` in `dir`.
#[cfg(feature = "dev-tools")]
fn gen_clip(dir: &Path, name: &str, spec: &str) -> PathBuf {
    let path = dir.join(name);
    let out = Command::new(WORKER)
        .arg("--dev-gen-clip")
        .arg(&path)
        .args(spec.split(' '))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "--dev-gen-clip {name} {spec}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

// Covers: T02
#[test]
fn worker_loads_the_pinned_build() {
    let worker = Worker::start(&[]);
    let text = std::fs::read_to_string(root().join("ffmpeg.toml")).unwrap();
    let pin: toml::Table = toml::from_str(&text).unwrap();
    let describe = pin["source"]["describe"].as_str().unwrap();
    let revision = pin["release"]["revision"].as_integer().unwrap();
    let build = &pin["build"];
    let dir = build["dir"].as_str().unwrap();
    // The configure line the build ran: ffmpeg.toml's flags, then the
    // revision and the paths of the build's own zlib and mcfgthread.
    let mut configure: Vec<String> = build["configure"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap().to_owned())
        .collect();
    configure.extend([
        format!("--extra-version={revision}"),
        format!("--extra-ldflags=-L{dir}/deps/mcf"),
        format!("--extra-cflags=-I{dir}/deps/zlib/include"),
        format!("--extra-ldflags=-L{dir}/deps/zlib/lib"),
    ]);
    let Reply::Hello {
        version,
        configuration,
    } = &worker.hello
    else {
        panic!("{:?} for a hello", worker.hello);
    };
    assert_eq!(version, &format!("{describe}-{revision}"));
    let configure: Vec<String> = configure.iter().map(|a| recorded(a)).collect();
    assert_eq!(
        configuration.split_whitespace().collect::<Vec<_>>(),
        configure
    );
}

/// A configure argument as FFmpeg's configure records it: the value after
/// its first `=` (the whole argument when it has none) single-quoted when
/// it holds a character outside `A-Za-z0-9_/.+-`.
fn recorded(arg: &str) -> String {
    let (name, value) = match arg.find('=') {
        Some(i) => arg.split_at(i + 1),
        None => ("", arg),
    };
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_/.+-".contains(c))
    {
        format!("{name}{value}")
    } else {
        format!("{name}'{}'", value.replace('\'', "'\\''"))
    }
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_decodes_generated_mpeg1_with_exact_stamps() {
    let dir = tempfile::tempdir().unwrap();
    // MPEG-1 in MPEG-PS (90 kHz stamps) and MJPEG in AVI (a stamp a frame),
    // at rates whose frame times do not fall on whole microseconds.
    let clips = [
        (
            "a.mpg",
            "mpeg1video mpeg 320x240 24 48 12",
            "mpeg1video",
            "mpeg",
            320,
            240,
            24,
            48,
        ),
        (
            "b.avi",
            "mjpeg avi 176x96 15 30 1",
            "mjpeg",
            "avi",
            176,
            96,
            15,
            30,
        ),
    ];
    let mut worker = Worker::start(&[]);
    for (player, (name, spec, codec, container, width, height, rate, frames)) in (1..).zip(clips) {
        let path = gen_clip(dir.path(), name, spec);
        let probe = worker.open(player, &path);
        let Reply::Probed {
            player: p,
            width: w,
            height: h,
            codec: c,
            container: f,
            ..
        } = &probe
        else {
            panic!("{name}: {probe:?}; the worker's log:\n{}", worker.log());
        };
        assert_eq!((*p, *w, *h), (player, width, height), "{name}");
        assert_eq!((c.as_str(), f.as_str()), (codec, container), "{name}");
        let mut ring = Ring::new(width, height);
        assert_eq!(worker.play(player, &ring), Reply::Playing { player });
        let interval = stamp_of(1, rate);
        let taken = take_all(&worker, &mut ring, player, 0, interval);
        let stamps: Vec<i64> = taken.iter().map(|t| t.stamp).collect();
        let want: Vec<i64> = (0..frames).map(|i| stamp_of(i, rate)).collect();
        assert_eq!(stamps, want, "{name}");
        let shown: Vec<u32> = taken
            .iter()
            .map(|t| painted(&t.bytes, width as usize))
            .collect();
        let want: Vec<u32> = (0..).take(want.len()).collect();
        assert_eq!(shown, want, "{name}: the frames' painted indices");
        worker.close(player);
    }
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_seek_lands_on_target_after_back_off() {
    let dir = tempfile::tempdir().unwrap();
    // 15-frame GOPs at 30 fps: a key frame every 500 ms.
    let path = gen_clip(dir.path(), "s.mpg", "mpeg1video mpeg 320x240 30 150 15");
    let mut worker = Worker::start(&[]);
    let probe = worker.open(3, &path);
    assert!(
        matches!(probe, Reply::Probed { player: 3, .. }),
        "{probe:?}"
    );
    let mut ring = Ring::new(320, 240);
    assert_eq!(worker.play(3, &ring), Reply::Playing { player: 3 });
    let first = take_one(&worker, &mut ring, 0);
    assert_eq!((first.stamp, painted(&first.bytes, 320)), (0, 0));

    let run = ring.start_run(1_500_000);
    worker.send(&Order::Seek {
        player: 3,
        run,
        at: 1_500_000,
    });
    // The first attempt's first frame is already past the target, so the
    // seek starts again a second earlier and decodes up to the target.
    assert_eq!(
        worker.reply(),
        Reply::Sought {
            player: 3,
            run,
            landed: 1_500_000,
            attempts: 2,
        },
        "the worker's log:\n{}",
        worker.log()
    );
    for (i, due) in [(45, 1_500_000), (46, 1_533_333), (47, 1_566_667)] {
        let taken = take_one(&worker, &mut ring, due);
        assert_eq!((taken.stamp, painted(&taken.bytes, 320)), (due, i));
    }
    worker.close(3);
}

/// `file` with every MPEG sequence header's size set to 4 095 × 4 095.
#[cfg(feature = "dev-tools")]
fn oversize(file: &Path) -> usize {
    let mut bytes = std::fs::read(file).unwrap();
    let mut headers = 0;
    let mut i = 0;
    while i + 7 <= bytes.len() {
        if bytes[i..i + 4] == [0, 0, 1, 0xb3] {
            bytes[i + 4..i + 7].copy_from_slice(&[0xff, 0xff, 0xff]);
            headers += 1;
        }
        i += 1;
    }
    std::fs::write(file, bytes).unwrap();
    headers
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_refuses_unlisted_codec_container_and_oversize() {
    use skin_model::media::control::Refusal;

    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("image.png");
    {
        let file = std::fs::File::create(&png).unwrap();
        let mut encoder = png::Encoder::new(file, 4, 3);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[90; 36]).unwrap();
    }
    let raw = gen_clip(dir.path(), "raw.avi", "rawvideo avi 64x48 24 12 1");
    let h263 = gen_clip(dir.path(), "h263.avi", "h263 avi 176x144 24 24 12");
    let big = gen_clip(dir.path(), "big.mpg", "mpeg1video mpeg 64x48 24 24 12");
    assert!(oversize(&big) > 0, "the clip has a sequence header");
    // Real frames of 2 896 × 2 896 (8 Mi pixels) in MPEG-PS, whose demuxer
    // adds its streams while FFmpeg probes.
    let large = gen_clip(dir.path(), "large.mpg", "mpeg1video mpeg 2896x2896 24 2 1");
    let good = gen_clip(dir.path(), "good.mpg", "mpeg1video mpeg 96x64 24 24 12");

    let mut worker = Worker::start(&[]);
    for (player, path, why) in [
        (1, &png, Refusal::Container),
        (2, &raw, Refusal::Codec),
        (3, &h263, Refusal::Codec),
        (4, &big, Refusal::Size),
        (6, &large, Refusal::Size),
    ] {
        match worker.open(player, path) {
            Reply::Refused {
                player: p, why: w, ..
            } if p == player && w == why => {}
            other => panic!(
                "{}: {other:?}, not refused for {why:?}; the worker's log:\n{}",
                path.display(),
                worker.log()
            ),
        }
        worker.close(player);
    }
    // H.263 is built (MPEG-4's decoder is built on it) but off the list:
    // FFmpeg refuses to open it while it probes.
    let log = worker.log();
    assert!(
        log.contains("Codec (h263) not on whitelist"),
        "FFmpeg's whitelist refusal of H.263 is missing; the worker's log:\n{log}"
    );
    // The decoder FFmpeg opens while it probes the MPEG-PS stream has the
    // pixel cap too: it refuses the frame instead of decoding it.
    assert!(
        log.contains("Picture size 2896x2896 exceeds specified max pixel count 4194304"),
        "FFmpeg's probe decoded the large frame without the cap; the worker's log:\n{log}"
    );
    // The worker lives and plays a listed video.
    let probe = worker.open(5, &good);
    assert!(
        matches!(
            probe,
            Reply::Probed {
                player: 5,
                width: 96,
                height: 64,
                ..
            }
        ),
        "{probe:?}"
    );
    let mut ring = Ring::new(96, 64);
    assert_eq!(worker.play(5, &ring), Reply::Playing { player: 5 });
    assert_eq!(take_one(&worker, &mut ring, 0).stamp, 0);
    worker.close(5);
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_caps_the_pixels_of_frames_after_the_probe() {
    use skin_model::media::control::Failure;

    let dir = tempfile::tempdir().unwrap();
    // Ten seconds at 64 × 48, then frames of 2 896 × 2 896 (8 Mi pixels),
    // which the probe never reads.
    let small = gen_clip(
        dir.path(),
        "small.m1v",
        "mpeg1video mpeg1video 64x48 24 240 12",
    );
    let grown = gen_clip(
        dir.path(),
        "grown.m1v",
        "mpeg1video mpeg1video 2896x2896 24 4 2",
    );
    let path = concat(dir.path(), "both.m1v", &[small, grown]);

    let mut worker = Worker::start(&[]);
    let probe = worker.open(1, &path);
    assert!(
        matches!(
            probe,
            Reply::Probed {
                player: 1,
                width: 64,
                height: 48,
                ..
            }
        ),
        "{probe:?}; the worker's log:\n{}",
        worker.log()
    );
    let mut ring = Ring::new(64, 48);
    let before = worker.peak_commit();
    assert_eq!(worker.play(1, &ring), Reply::Playing { player: 1 });
    let deadline = Instant::now() + REPLY;
    let mut taken = Vec::new();
    // The newest frame due each time, until the video ends or fails.
    let end = loop {
        assert!(
            Instant::now() < deadline,
            "{} frames, the last at {:?}; slots {:?}; the worker's log:\n{}",
            taken.len(),
            taken.last(),
            ring.slots(),
            worker.log()
        );
        if let Some(frame) = ring.poll(i64::MAX) {
            taken.push(frame.stamp);
            continue;
        }
        if let Some(reply) = worker.try_reply() {
            break reply;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    while let Some(frame) = ring.poll(i64::MAX) {
        taken.push(frame.stamp);
    }
    // The decoder refuses the first frame past the cap, which fails the
    // player; every frame before it came, and no later one.
    assert_eq!(
        end,
        Reply::Failed {
            player: 1,
            why: Failure::Decode,
            code: -22,
        },
        "the worker's log:\n{}",
        worker.log()
    );
    assert_eq!(taken.last(), Some(&stamp_of(239, 24)), "{taken:?}");
    // Nothing near a frame of the grown size (12 MiB) was allocated.
    let peak = worker.peak_commit();
    assert!(
        peak < before + (16 << 20),
        "the worker's peak commit rose from {before} to {peak}"
    );
    worker.close(1);
}

/// Writes `parts` one after another into `dir/name`: a stream whose later
/// frames the probe never reads.
#[cfg(feature = "dev-tools")]
fn concat(dir: &Path, name: &str, parts: &[PathBuf]) -> PathBuf {
    let path = dir.join(name);
    let bytes: Vec<u8> = parts
        .iter()
        .flat_map(|p| std::fs::read(p).unwrap())
        .collect();
    std::fs::write(&path, bytes).unwrap();
    path
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn decoder_enomem_fails_one_player_and_the_worker_lives() {
    use skin_model::media::control::Failure;

    let dir = tempfile::tempdir().unwrap();
    // Player 1 plays ten seconds at 64 × 48 and then 1 920 × 1 080 frames,
    // whose planes are larger than the lowered cap on one allocation;
    // player 2 plays 64 × 48 frames throughout.
    let small = gen_clip(
        dir.path(),
        "small.m1v",
        "mpeg1video mpeg1video 64x48 24 240 12",
    );
    let large = gen_clip(
        dir.path(),
        "large.m1v",
        "mpeg1video mpeg1video 1920x1080 24 4 2",
    );
    let grows = concat(dir.path(), "grows.m1v", &[small, large]);
    let steady = gen_clip(dir.path(), "steady.mpg", "mpeg1video mpeg 64x48 24 1440 12");

    let mut worker = Worker::start(&["--dev-max-alloc", "1048576"]);
    let mut rings = Vec::new();
    for (player, path) in [(1, &grows), (2, &steady)] {
        let probe = worker.open(player, path);
        assert!(
            matches!(
                probe,
                Reply::Probed {
                    width: 64,
                    height: 48,
                    ..
                }
            ),
            "{probe:?}; the worker's log:\n{}",
            worker.log()
        );
        let ring = Ring::new(64, 48);
        assert_eq!(worker.play(player, &ring), Reply::Playing { player });
        rings.push(ring);
    }
    let [one, two] = &mut rings[..] else {
        unreachable!()
    };
    let deadline = Instant::now() + REPLY;
    let mut frames = [0, 0];
    let failed = loop {
        assert!(
            Instant::now() < deadline,
            "the worker's log:\n{}",
            worker.log()
        );
        frames[0] += usize::from(one.poll(i64::MAX).is_some());
        frames[1] += usize::from(two.poll(i64::MAX).is_some());
        if let Some(reply) = worker.try_reply() {
            break reply;
        }
    };
    assert_eq!(
        failed,
        Reply::Failed {
            player: 1,
            why: Failure::Memory,
            code: -12,
        },
        "the worker's log:\n{}",
        worker.log()
    );
    assert!(frames[0] > 0, "player 1 showed no frame before it failed");
    // The worker goes on: player 2's frames keep coming, and it answers a
    // new open.
    let before = frames[1];
    while frames[1] < before + 24 {
        assert!(
            Instant::now() < deadline,
            "the worker's log:\n{}",
            worker.log()
        );
        frames[1] += usize::from(two.poll(i64::MAX).is_some());
        assert_eq!(worker.try_reply(), None, "player 2 ended or failed early");
    }
    assert!(worker.child.try_wait().unwrap().is_none());
    let probe = worker.open(3, &steady);
    assert!(
        matches!(probe, Reply::Probed { player: 3, .. }),
        "{probe:?}"
    );
    for player in [1, 2, 3] {
        worker.close(player);
    }
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_plays_every_listed_container() {
    let dir = tempfile::tempdir().unwrap();
    // Each listed container by FFmpeg's demuxer name, with the listed codecs
    // the build has an encoder for.
    let clips = [
        ("ps.mpg", "mpeg2video mpeg", "mpeg2video", "mpeg"),
        ("es.m1v", "mpeg1video mpeg1video", "mpeg1video", "mpegvideo"),
        ("wmv2.asf", "wmv2 asf", "wmv2", "asf"),
        ("wmv1.asf", "wmv1 asf", "wmv1", "asf"),
        ("mpeg4.mov", "mpeg4 mov", "mpeg4", "mov,mp4,m4a,3gp,3g2,mj2"),
        ("msmpeg4.avi", "msmpeg4 avi", "msmpeg4v3", "avi"),
        ("msmpeg4v2.avi", "msmpeg4v2 avi", "msmpeg4v2", "avi"),
        ("cinepak.avi", "cinepak avi", "cinepak", "avi"),
        ("msvideo1.avi", "msvideo1 avi", "msvideo1", "avi"),
        ("mjpeg.mkv", "mjpeg matroska", "mjpeg", "matroska,webm"),
    ];
    let mut worker = Worker::start(&[]);
    for (player, (name, spec, codec, container)) in (1..).zip(clips) {
        let path = gen_clip(dir.path(), name, &format!("{spec} 256x144 25 10 5"));
        let probe = worker.open(player, &path);
        let Reply::Probed {
            width,
            height,
            codec: c,
            container: f,
            ..
        } = &probe
        else {
            panic!("{name}: {probe:?}; the worker's log:\n{}", worker.log());
        };
        assert_eq!((*width, *height), (256, 144), "{name}");
        assert_eq!((c.as_str(), f.as_str()), (codec, container), "{name}");
        let mut ring = Ring::new(256, 144);
        assert_eq!(worker.play(player, &ring), Reply::Playing { player });
        for i in 0..2 {
            let due = stamp_of(i, 25);
            let taken = take_one(&worker, &mut ring, due);
            assert_eq!(
                (taken.stamp, painted(&taken.bytes, 256)),
                (due, u32::try_from(i).unwrap()),
                "{name}"
            );
        }
        worker.close(player);
    }
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_secondary_opens_are_refused_by_layer() {
    let dir = tempfile::tempdir().unwrap();
    let clip = gen_clip(dir.path(), "c.mpg", "mpeg1video mpeg 64x48 24 12 12");
    let open = |io: &str| {
        let out = Command::new(WORKER)
            .arg("--dev-secondary-open")
            .arg(&clip)
            .arg(io)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{io}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        (
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    // The worker's `io_open` refuses a playlist's or a reference's open of
    // another file with EPERM, and says so.
    let (code, log) = open("refuse");
    assert_eq!(code.trim(), "secondary open: -1");
    assert!(
        log.contains("refused a secondary open of \"segment-0.ts\""),
        "{log}"
    );
    // Without it, FFmpeg's own open finds no protocol in the build.
    let not_found = -i32::from_le_bytes([0xf8, b'P', b'R', b'O']);
    let (code, _) = open("default");
    assert_eq!(code.trim(), format!("secondary open: {not_found}"));
}

// Covers: T02
#[test]
fn worker_decodes_zlib_compressed_containers() {
    let fixtures = root().join("crates/player/tests/media/zlib");
    let mut worker = Worker::start(&[]);
    let interval = 33_333;
    let mut player = 0;
    let mut play = |name: &str| {
        player += 1;
        let probe = worker.open(player, &fixtures.join(name));
        let Reply::Probed { width, height, .. } = probe else {
            panic!("{name}: {probe:?}; the worker's log:\n{}", worker.log());
        };
        assert_eq!((width, height), (320, 240), "{name}");
        let mut ring = Ring::new(width, height);
        assert_eq!(worker.play(player, &ring), Reply::Playing { player });
        let frames = take_all(&worker, &mut ring, player, 0, interval);
        let run = ring.start_run(2_000_000);
        worker.send(&Order::Seek {
            player,
            run,
            at: 2_000_000,
        });
        let landed = match worker.reply() {
            Reply::Sought { landed, .. } => landed,
            other => panic!("{name}: {other:?}; the worker's log:\n{}", worker.log()),
        };
        let at = take_one(&worker, &mut ring, landed);
        worker.close(player);
        (frames, (at.stamp, at.hash()))
    };
    for (compressed, plain) in [
        ("mpeg4-cmov.mov", "plain.mov"),
        ("mpeg4-zlib.mkv", "plain.mkv"),
    ] {
        let (want, want_seek) = play(plain);
        let (got, got_seek) = play(compressed);
        // Four seconds at 30 fps, every frame its own.
        assert_eq!(want.len(), 120, "{plain}");
        let mut distinct: Vec<u64> = want.iter().map(Taken::hash).collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), want.len(), "{plain}: frames repeat");
        let frames = |t: &[Taken]| t.iter().map(|f| (f.stamp, f.hash())).collect::<Vec<_>>();
        assert_eq!(frames(&got), frames(&want), "{compressed} against {plain}");
        assert_eq!(got_seek, want_seek, "{compressed}: the seek to 2 s");
        assert!(
            want_seek.0 >= 2_000_000,
            "{plain}: landed at {}",
            want_seek.0
        );
    }
}
