//! The media worker `tapestry-media` (design.md "Threat Model" T02;
//! "Libraries", the FFmpeg row), the real executable launched by the
//! game's host under the full recipe, its sandbox, and commanded over its
//! control pipe, with each frame ring and its free-slot event made as the
//! game makes them and handed to the worker by handle, each with only the
//! access it needs. They check what the worker does under its sandbox and
//! what FFmpeg's layer holds: the pinned build, the codec and container
//! allow-list, the worker's open of the decoders FFmpeg's probe opens, the
//! pixel cap on the size FFmpeg allocates, the probe's read budget, the
//! refused allocations a demuxer goes on from, the refused secondary open,
//! the decoded frames' stamps, the seek's back-off and the zlib-compressed
//! containers (`media_sandbox.rs` checks the sandbox itself).
//!
//! The clips are made by the worker's own `--dev-gen-clip` (dev-tools), run
//! as a command of its own, so those tests need dev-tools; the committed
//! fixtures (`tests/media/`), the files built here and the handshake do
//! not.

#![cfg(windows)]

mod media_support;

use std::hash::{Hash as _, Hasher as _};
use std::os::windows::io::{AsRawHandle as _, OwnedHandle};
use std::path::Path;
#[cfg(feature = "dev-tools")]
use std::path::PathBuf;
#[cfg(feature = "dev-tools")]
use std::process::Command;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

#[cfg(feature = "dev-tools")]
use media_support::WORKER;
use media_support::{Kind, Launched, Stage, media_host, recipe, root};
use skin_model::media::control::{Command as Order, Reply};
use skin_model::media::{Frame, Geometry, Reader, Window};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{
    FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows::Win32::System::Threading::SetEvent;

/// How long a reply may take.
const REPLY: Duration = Duration::from_secs(30);

/// A worker the game's host launched under the recipe, staged in a
/// directory of its own, and the replies on its control pipe.
struct Worker {
    launched: Launched,
    _stage: Stage,
}

impl Worker {
    /// Launches the worker with `args` and reads its hello.
    fn start(args: &[&str]) -> Self {
        let stage = Stage::new();
        let launched = stage
            .launch(&recipe().args_of(args))
            .unwrap_or_else(|e| panic!("{e}"));
        Self {
            launched,
            _stage: stage,
        }
    }

    fn send(&mut self, order: &Order) {
        self.launched.send(order).unwrap();
    }

    /// The next reply.
    fn reply(&self) -> Reply {
        match self.launched.recv(REPLY) {
            Ok(r) => r,
            Err(e) => panic!("no reply ({e}); the worker's log:\n{}", self.log()),
        }
    }

    /// A reply that has come, without waiting.
    fn try_reply(&self) -> Option<Reply> {
        self.launched.try_recv()
    }

    /// The worker's log lines so far.
    fn log(&self) -> String {
        self.launched.log()
    }

    /// `handle` duplicated into the worker as the game hands it one of
    /// `kind`.
    fn give(&self, handle: HANDLE, kind: Kind) -> u64 {
        self.launched.hand_in(handle, kind).unwrap()
    }

    /// The worker's peak commit so far, in bytes.
    fn peak_commit(&self) -> usize {
        use windows::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };

        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        // SAFETY: the worker's process handle, which has every access, and
        // a buffer of the size given.
        unsafe {
            GetProcessMemoryInfo(
                self.launched.process(),
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
        let handle = self.give(HANDLE(file.as_raw_handle()), Kind::File);
        self.send(&Order::Open {
            player,
            file: handle,
        });
        self.reply()
    }

    /// Hands player `player` its ring.
    fn play(&mut self, player: u8, ring: &Ring) -> Reply {
        let section = self.give(HANDLE(ring.section.as_raw_handle()), Kind::Section);
        let event = self.give(HANDLE(ring.event.as_raw_handle()), Kind::Event);
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

/// A frame ring as the game makes it ([`media_host::ring_objects`]): a
/// section of its own, mapped here, its header written, and its free-slot
/// event.
struct Ring {
    section: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    event: OwnedHandle,
    reader: Reader,
}

impl Ring {
    /// A ring for `width` × `height` frames, whose first run's stamps start
    /// at 0.
    fn new(width: u32, height: u32) -> Self {
        let geometry = Geometry::new(width, height).unwrap();
        let (section, event) =
            media_host::ring_objects(u64::try_from(geometry.bytes()).unwrap()).unwrap();
        // SAFETY: the section just made, mapped whole.
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
            unsafe { SetEvent(HANDLE(self.event.as_raw_handle())) }.unwrap();
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
        // SAFETY: the view this ring mapped, unused after this.
        let _ = unsafe { UnmapViewOfFile(self.view) };
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
fn worker_operations_succeed_under_the_recipe() {
    let mut worker = Worker::start(&[]);
    // It runs confined: in its job, an AppContainer's token with no
    // capability at low integrity, win32k system calls disabled.
    let process = worker.launched.process();
    assert_eq!(media_support::token_facts(process), (true, 0, 0x1000));
    assert_eq!(
        media_support::policy(
            process,
            windows::Win32::System::Threading::ProcessSystemCallDisablePolicy
        ) & 1,
        1
    );
    // Its hello came, so it loaded the shipped DLLs; it reads its pipe,
    // reads the handed file through its I/O context, maps its section,
    // decodes into it, waits on its event as slots are freed, seeks and
    // answers each command.
    let fixture = root().join("crates/player/tests/media/zlib/plain.mkv");
    let probe = worker.open(1, &fixture);
    let Reply::Probed { width, height, .. } = probe else {
        panic!("{probe:?}; the worker's log:\n{}", worker.log());
    };
    let mut ring = Ring::new(width, height);
    assert_eq!(worker.play(1, &ring), Reply::Playing { player: 1 });
    let frames = take_all(&worker, &mut ring, 1, 0, 33_333);
    assert_eq!(frames.len(), 120);
    let run = ring.start_run(2_000_000);
    worker.send(&Order::Seek {
        player: 1,
        run,
        at: 2_000_000,
    });
    let landed = match worker.reply() {
        Reply::Sought { landed, .. } => landed,
        other => panic!("{other:?}; the worker's log:\n{}", worker.log()),
    };
    assert!(take_one(&worker, &mut ring, landed).stamp >= 2_000_000);
    worker.close(1);
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
    let media_host::Hello {
        version,
        configuration,
    } = worker.launched.hello();
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
    // the worker's open refuses the decoder FFmpeg's probe would open.
    let log = worker.log();
    assert!(
        log.contains("tapestry-media: refused a decoder for h263"),
        "the worker's refusal of H.263's decoder is missing; the worker's log:\n{log}"
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
    assert!(!worker.launched.ended());
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

/// Bits written most significant first, as H.264's syntax writes them.
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    used: u32,
}

impl Bits {
    fn bit(&mut self, one: bool) -> &mut Self {
        if self.used == 0 {
            self.bytes.push(0);
        }
        if one {
            *self.bytes.last_mut().unwrap() |= 0x80 >> self.used;
        }
        self.used = (self.used + 1) % 8;
        self
    }

    fn u(&mut self, n: u32, v: u64) -> &mut Self {
        for i in (0..n).rev() {
            self.bit(v >> i & 1 == 1);
        }
        self
    }

    /// An unsigned Exp-Golomb code.
    fn ue(&mut self, v: u64) -> &mut Self {
        let x = v + 1;
        let n = 64 - x.leading_zeros();
        self.u(n - 1, 0).u(n, x)
    }

    /// A signed Exp-Golomb code.
    fn se(&mut self, v: i64) -> &mut Self {
        let m = v.unsigned_abs() * 2;
        self.ue(if v > 0 { m - 1 } else { m })
    }

    /// The payload: the bits, a stop bit, and zeros to the byte.
    fn rbsp(&mut self) -> Vec<u8> {
        self.bit(true);
        while self.used != 0 {
            self.bit(false);
        }
        std::mem::take(&mut self.bytes)
    }
}

/// A NAL unit with `header` and `rbsp`, after a start code, with H.264's
/// emulation prevention.
fn nal(header: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1, header];
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// The parameter sets (their payloads) of an H.264 sequence that codes
/// `coded` pixels and crops them to `shown`.
fn h264_sets(coded: (u32, u32), shown: (u32, u32)) -> (Vec<u8>, Vec<u8>) {
    // Baseline, level 5.1; frame numbers of 4 bits, picture order type 2,
    // one reference; frames only, cropped on the right and at the bottom
    // in units of two pixels; no VUI.
    let sps = Bits::default()
        .u(8, 66)
        .u(8, 0)
        .u(8, 51)
        .ue(0)
        .ue(0)
        .ue(2)
        .ue(1)
        .bit(false)
        .ue(u64::from(coded.0 / 16 - 1))
        .ue(u64::from(coded.1 / 16 - 1))
        .bit(true)
        .bit(true)
        .bit(true)
        .ue(0)
        .ue(u64::from((coded.0 - shown.0) / 2))
        .ue(0)
        .ue(u64::from((coded.1 - shown.1) / 2))
        .bit(false)
        .rbsp();
    // CAVLC, one slice group, one reference, no weighting, QP 26, the
    // deblocking control present.
    let pps = Bits::default()
        .ue(0)
        .ue(0)
        .bit(false)
        .bit(false)
        .ue(0)
        .ue(0)
        .ue(0)
        .bit(false)
        .u(2, 0)
        .se(0)
        .se(0)
        .se(0)
        .bit(true)
        .bit(false)
        .bit(false)
        .rbsp();
    (sps, pps)
}

/// The payload of IDR slice `i`: an I slice of the whole picture,
/// deblocking off, then macroblock bits that do not decode, which the
/// decoder allocates its frame for before it reads them.
fn h264_idr(i: u64) -> Vec<u8> {
    Bits::default()
        .ue(0)
        .ue(7)
        .ue(0)
        .u(4, 0)
        .ue(i)
        .bit(false)
        .bit(false)
        .se(0)
        .ue(1)
        .u(16, 0xa5c3)
        .rbsp()
}

/// `frames` access units of an H.264 stream (Annex B) whose sequence codes
/// `coded` pixels and crops them to `shown`: the parameter sets, then IDR
/// slices.
fn h264_cropped(coded: (u32, u32), shown: (u32, u32), frames: u64) -> Vec<Vec<u8>> {
    let (sps, pps) = h264_sets(coded, shown);
    (0..frames)
        .map(|i| {
            let mut unit = Vec::new();
            if i == 0 {
                unit.extend(nal(0x67, &sps));
                unit.extend(nal(0x68, &pps));
            }
            unit.extend(nal(0x65, &h264_idr(i)));
            unit
        })
        .collect()
}

/// An AVI file of one video stream of `fourcc` at `size` and `rate`
/// frames a second, its frames `frames`, without an index.
fn avi(size: (u32, u32), rate: u32, fourcc: &[u8; 4], frames: &[Vec<u8>]) -> Vec<u8> {
    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = id.to_vec();
        c.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
        c.extend(data);
        if data.len() % 2 == 1 {
            c.push(0);
        }
        c
    }
    fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        chunk(b"LIST", &[kind.as_slice(), body].concat())
    }
    let count = u32::try_from(frames.len()).unwrap();
    let words = |w: &[u32]| w.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
    let avih = words(&[
        1_000_000 / rate,
        0,
        0,
        0,
        count,
        0,
        1,
        1 << 20,
        size.0,
        size.1,
        0,
        0,
        0,
        0,
    ]);
    let strh = [
        b"vids".as_slice(),
        fourcc,
        &words(&[0, 0, 0, 1, rate, 0, count, 1 << 20, u32::MAX, 0]),
        &[0; 8],
    ]
    .concat();
    let strf = [
        words(&[40, size.0, size.1]),
        vec![1, 0, 24, 0],
        fourcc.to_vec(),
        words(&[size.0 * size.1 * 3, 0, 0, 0, 0]),
    ]
    .concat();
    let strl = list(
        b"strl",
        &[chunk(b"strh", &strh), chunk(b"strf", &strf)].concat(),
    );
    let hdrl = list(b"hdrl", &[chunk(b"avih", &avih), strl].concat());
    let movi: Vec<u8> = frames.iter().flat_map(|f| chunk(b"00dc", f)).collect();
    chunk(
        b"RIFF",
        &[b"AVI ".as_slice(), &hdrl, &list(b"movi", &movi)].concat(),
    )
}

/// A chunk of a [`mov`] file: the parameter sets of its sample
/// description, and the frame numbers of its IDR slices.
type Chunk = ((Vec<u8>, Vec<u8>), std::ops::Range<u64>);

/// A QuickTime file of one H.264 video stream at `size`, its time scale
/// `rate`, whose chunks each have a sample description of their own (the
/// parameter sets given) and IDR slices of the frame numbers given, the
/// first frame one tick long and the others two.
fn mov(size: (u16, u16), rate: u32, chunks: &[Chunk]) -> Vec<u8> {
    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = u32::try_from(body.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        b.extend(kind);
        b.extend(body);
        b
    }
    // A box with a version (0) and flags.
    fn full(kind: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
        boxed(kind, &[flags.to_be_bytes().as_slice(), body].concat())
    }
    fn be(v: usize) -> [u8; 4] {
        u32::try_from(v).unwrap().to_be_bytes()
    }
    // A NAL unit without its start code.
    let bare = |header: u8, rbsp: &[u8]| nal(header, rbsp)[4..].to_vec();
    let samples: Vec<Vec<Vec<u8>>> = chunks
        .iter()
        .map(|(_, frames)| {
            frames
                .clone()
                .map(|i| {
                    let unit = bare(0x65, &h264_idr(i));
                    [be(unit.len()).as_slice(), &unit].concat()
                })
                .collect()
        })
        .collect();
    let all: Vec<&Vec<u8>> = samples.iter().flatten().collect();
    let count = all.len();
    let ftyp = boxed(b"ftyp", b"qt  \0\0\0\0qt  ");
    let mdat = boxed(
        b"mdat",
        &all.iter().copied().flatten().copied().collect::<Vec<u8>>(),
    );
    let mut offsets = Vec::new();
    let mut at = ftyp.len() + 8;
    for chunk in &samples {
        offsets.extend(be(at));
        at += chunk.iter().map(Vec::len).sum::<usize>();
    }
    let duration = 2 * count - 1;
    let matrix: Vec<u8> = [0x0001_0000_u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000]
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .collect();
    let rate = usize::try_from(rate).unwrap();
    let mvhd = full(
        b"mvhd",
        0,
        &[
            [0_u8; 8].as_slice(),
            &be(rate),
            &be(duration),
            &be(0x0001_0000),
            &[1, 0],
            &[0; 10],
            &matrix,
            &[0; 24],
            &be(2),
        ]
        .concat(),
    );
    let (w, h) = (usize::from(size.0), usize::from(size.1));
    let tkhd = full(
        b"tkhd",
        3,
        &[
            [0_u8; 8].as_slice(),
            &be(1),
            &[0; 4],
            &be(duration),
            &[0; 16],
            &matrix,
            &be(w << 16),
            &be(h << 16),
        ]
        .concat(),
    );
    let mdhd = full(
        b"mdhd",
        0,
        &[
            [0_u8; 8].as_slice(),
            &be(rate),
            &be(duration),
            &[0x55, 0xc4, 0, 0],
        ]
        .concat(),
    );
    let hdlr = full(
        b"hdlr",
        0,
        &[[0_u8; 4].as_slice(), b"vide", &[0; 13]].concat(),
    );
    let entries: Vec<u8> = chunks
        .iter()
        .flat_map(|((sps, pps), _)| {
            let (sps, pps) = (bare(0x67, sps), bare(0x68, pps));
            let length = |unit: &[u8]| u16::try_from(unit.len()).unwrap().to_be_bytes();
            let avcc = boxed(
                b"avcC",
                &[
                    [1_u8, sps[1], sps[2], sps[3], 0xff, 0xe1].as_slice(),
                    &length(&sps),
                    &sps,
                    &[1],
                    &length(&pps),
                    &pps,
                ]
                .concat(),
            );
            boxed(
                b"avc1",
                &[
                    [0_u8, 0, 0, 0, 0, 0, 0, 1].as_slice(),
                    &[0; 16],
                    &size.0.to_be_bytes(),
                    &size.1.to_be_bytes(),
                    &be(0x0048_0000),
                    &be(0x0048_0000),
                    &[0; 4],
                    &[0, 1],
                    &[0; 32],
                    &[0, 0x18, 0xff, 0xff],
                    &avcc,
                ]
                .concat(),
            )
        })
        .collect();
    let stsd = full(
        b"stsd",
        0,
        &[be(chunks.len()).as_slice(), &entries].concat(),
    );
    let stts = full(
        b"stts",
        0,
        &[be(2), be(1), be(1), be(count - 1), be(2)].concat(),
    );
    let stsc: Vec<u8> = samples
        .iter()
        .enumerate()
        .flat_map(|(i, chunk)| [be(i + 1), be(chunk.len()), be(i + 1)].concat())
        .collect();
    let stsc = full(b"stsc", 0, &[be(samples.len()).as_slice(), &stsc].concat());
    let sizes: Vec<u8> = all.iter().flat_map(|s| be(s.len())).collect();
    let stsz = full(
        b"stsz",
        0,
        &[[0_u8; 4].as_slice(), &be(count), &sizes].concat(),
    );
    let stco = full(
        b"stco",
        0,
        &[be(samples.len()).as_slice(), &offsets].concat(),
    );
    let stbl = boxed(b"stbl", &[stsd, stts, stsc, stsz, stco].concat());
    let dinf = boxed(
        b"dinf",
        &full(
            b"dref",
            0,
            &[be(1).as_slice(), &full(b"url ", 1, &[])].concat(),
        ),
    );
    let minf = boxed(b"minf", &[full(b"vmhd", 1, &[0; 8]), dinf, stbl].concat());
    let mdia = boxed(b"mdia", &[mdhd, hdlr, minf].concat());
    let trak = boxed(b"trak", &[tkhd, mdia].concat());
    [ftyp, mdat, boxed(b"moov", &[mvhd, trak].concat())].concat()
}

// Covers: T02
#[test]
fn worker_refuses_a_coded_size_over_the_cap_behind_a_crop() {
    use skin_model::media::control::Refusal;

    let dir = tempfile::tempdir().unwrap();
    // Coded at 8 192 × 4 096 (32 Mi pixels, 48 MiB a frame), shown at
    // 1 920 × 1 080: FFmpeg's own cap reads the shown size, and allocates
    // the coded one.
    let path = dir.path().join("cropped.avi");
    let frames = h264_cropped((8192, 4096), (1920, 1080), 3);
    std::fs::write(&path, avi((1920, 1080), 24, b"H264", &frames)).unwrap();

    let mut worker = Worker::start(&[]);
    let before = worker.peak_commit();
    assert_eq!(
        worker.open(1, &path),
        Reply::Refused {
            player: 1,
            why: Refusal::Size,
            code: 0,
        },
        "the worker's log:\n{}",
        worker.log()
    );
    let log = worker.log();
    assert!(log.contains("refused a frame of 8192 x 4096"), "{log}");
    // Nothing of the frame was allocated: the peak rose by less than its
    // luma plane.
    let peak = worker.peak_commit();
    assert!(
        peak < before + (32 << 20),
        "the worker's peak commit rose from {before} to {peak}"
    );
    worker.close(1);
}

// Covers: T02
#[test]
fn worker_caps_a_decoder_ffmpeg_opens_again_for_a_new_sample_description() {
    use skin_model::media::control::Refusal;

    let dir = tempfile::tempdir().unwrap();
    // Two frames under a sample description of 64 × 48, then four under
    // one of 8 192 × 4 096 (32 Mi pixels, 48 MiB a frame). The frames'
    // uneven durations keep FFmpeg's probe reading the whole file, and it
    // decodes H.264's first frames to learn their delay: at the second
    // description it closes its decoder, opens it again, and decodes on.
    let path = dir.path().join("described.mov");
    let chunks = [
        (h264_sets((64, 48), (64, 48)), 0..2),
        (h264_sets((8192, 4096), (8192, 4096)), 2..6),
    ];
    std::fs::write(&path, mov((64, 48), 24, &chunks)).unwrap();

    let mut worker = Worker::start(&[]);
    let before = worker.peak_commit();
    let reply = worker.open(1, &path);
    // Nothing of a large frame was allocated: the peak rose by less than
    // its luma plane.
    let peak = worker.peak_commit();
    assert!(
        peak < before + (32 << 20),
        "the worker's peak commit rose from {before} to {peak}; the worker's log:\n{}",
        worker.log()
    );
    // The stream's size is the second description's.
    assert_eq!(
        reply,
        Reply::Refused {
            player: 1,
            why: Refusal::Size,
            code: 0,
        },
        "the worker's log:\n{}",
        worker.log()
    );
    worker.close(1);
}

/// `clip`, an MPEG-PS file, with a run of padding packets `bytes` long
/// after its first pack: one read of the demuxer scans it whole.
#[cfg(feature = "dev-tools")]
fn padded(clip: &Path, bytes: usize) -> PathBuf {
    let data = std::fs::read(clip).unwrap();
    let second = (4..data.len() - 4)
        .find(|&i| data[i..i + 4] == [0, 0, 1, 0xba])
        .expect("a second pack");
    let mut out = data[..second].to_vec();
    let mut left = bytes;
    while left > 0 {
        let len = left.min(65_535 + 6) - 6;
        out.extend([0, 0, 1, 0xbe]);
        out.extend(u16::try_from(len).unwrap().to_be_bytes());
        out.extend(std::iter::repeat_n(0xff, len));
        left -= len + 6;
    }
    out.extend(&data[second..]);
    let path = clip.with_extension(format!("{bytes}.mpg"));
    std::fs::write(&path, out).unwrap();
    path
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_refuses_a_probe_past_its_read_budget() {
    use skin_model::media::control::Refusal;

    let dir = tempfile::tempdir().unwrap();
    let clip = gen_clip(dir.path(), "short.mpg", "mpeg1video mpeg 64x48 24 24 12");
    // 65 MiB of padding after the first pack: more than the probe's 64 MiB.
    let long = padded(&clip, 65 << 20);
    let mut worker = Worker::start(&[]);
    assert_eq!(
        worker.open(1, &long),
        Reply::Refused {
            player: 1,
            why: Refusal::Read,
            code: -27,
        },
        "the worker's log:\n{}",
        worker.log()
    );
    worker.close(1);
    // The same clip padded within the budget is probed.
    let short = padded(&clip, 1 << 20);
    let probe = worker.open(2, &short);
    assert!(
        matches!(probe, Reply::Probed { player: 2, .. }),
        "{probe:?}; the worker's log:\n{}",
        worker.log()
    );
    worker.close(2);
}

/// `clip`, a Matroska file, with its segment's size made unknown and a
/// cluster after its end whose one block claims 64 MiB and 1 byte, more
/// than one allocation may take.
#[cfg(feature = "dev-tools")]
fn with_huge_block(clip: &Path) -> PathBuf {
    let mut data = std::fs::read(clip).unwrap();
    let segment = (0..data.len() - 4)
        .find(|&i| data[i..i + 4] == [0x18, 0x53, 0x80, 0x67])
        .expect("a segment");
    // FFmpeg writes the segment's size in eight bytes.
    assert_eq!(data[segment + 4], 0x01, "an eight-byte segment size");
    data[segment + 4..segment + 12].copy_from_slice(&[1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    // A cluster of unknown size: its time code, then a simple block of
    // 64 MiB and 1 byte (a four-byte size), of which a few bytes follow.
    data.extend([
        0x1f, 0x43, 0xb6, 0x75, 1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    ]);
    data.extend([0xe7, 0x82, 0x7f, 0xff]);
    data.extend([0xa3, 0x14, 0, 0, 1, 0x81, 0, 0, 0x80]);
    data.extend([0x55; 16]);
    let path = clip.with_extension("huge.mkv");
    std::fs::write(&path, data).unwrap();
    path
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn demuxer_enomem_fails_one_player_and_the_worker_lives() {
    use skin_model::media::control::Failure;

    let dir = tempfile::tempdir().unwrap();
    // Player 1 plays ten seconds of 64 × 48 (more than FFmpeg's probe
    // reads) and then meets a block too large to allocate, which
    // Matroska's demuxer would step over as damaged; player 2 plays
    // 64 × 48 frames throughout.
    let clip = gen_clip(dir.path(), "short.mkv", "mjpeg matroska 64x48 24 240 1");
    let huge = with_huge_block(&clip);
    let steady = gen_clip(dir.path(), "steady.mpg", "mpeg1video mpeg 64x48 24 1440 12");

    let mut worker = Worker::start(&[]);
    let mut rings = Vec::new();
    for (player, path) in [(1, &huge), (2, &steady)] {
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
    let end = loop {
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
        end,
        Reply::Failed {
            player: 1,
            why: Failure::Memory,
            code: -12,
        },
        "the worker's log:\n{}",
        worker.log()
    );
    assert!(frames[0] > 0, "player 1 showed no frame before it failed");
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
    for player in [1, 2] {
        worker.close(player);
    }
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_refuses_an_unlisted_decoder_without_the_codec_whitelist() {
    use skin_model::media::control::Refusal;

    let dir = tempfile::tempdir().unwrap();
    // H.263 is built (MPEG-4's decoder is built on it) but off the list.
    let h263 = gen_clip(dir.path(), "h263.avi", "h263 avi 176x144 24 24 12");
    // FFmpeg's codec whitelist unset, as a refused allocation while it is
    // set would leave it: the worker's own open still refuses the decoder
    // FFmpeg's probe would open.
    let mut worker = Worker::start(&["--dev-no-codec-list"]);
    assert_eq!(
        worker.open(1, &h263),
        Reply::Refused {
            player: 1,
            why: Refusal::Codec,
            code: 0,
        },
        "the worker's log:\n{}",
        worker.log()
    );
    let log = worker.log();
    assert!(log.contains("refused a decoder for h263"), "{log}");
    assert!(!log.contains("not on whitelist"), "{log}");
    worker.close(1);
}
