//! The media worker `tapestry-media` (design.md "Threat Model" T02;
//! "Libraries", the FFmpeg row), the real executable launched by the
//! game's host under the full recipe, its sandbox, and commanded over its
//! control pipe, with each frame ring and its free-slot event made as the
//! game makes them and handed to the worker by handle, each with only the
//! access it needs. They check what the worker does under its sandbox and
//! what FFmpeg's layer holds: the pinned build, the codec and container
//! allow-list, the worker's open of the decoders FFmpeg's probe opens, the
//! pixel cap on the size FFmpeg allocates, the probe's read budget, the
//! refused allocations a demuxer or a decoder goes on from, a refusal
//! recorded while a decoder parks at a video's end, the refused secondary
//! open, the decoded frames' stamps, the seek's back-off and the
//! zlib-compressed containers (`media_sandbox.rs` checks the sandbox
//! itself). They check the game's supervision of the worker too: the job
//! watcher's two paths to ending a worker at its memory cap, each apart, a
//! job for each launch, the peak of the four largest players, and the
//! strikes that keep a file from being opened again once it has been open
//! in two workers that ended without the game asking.
//!
//! The clips are made by the worker's own `--dev-gen-clip` (dev-tools), run
//! as a command of its own, and the memory the job's cap is tested with by
//! its `--dev-balloon`, so those tests need dev-tools; the committed
//! fixtures (`tests/media/`), the files built here and the handshake do
//! not.

#![cfg(windows)]

mod media_support;

use std::collections::BTreeMap;
use std::hash::{Hash as _, Hasher as _};
use std::os::windows::io::{AsRawHandle as _, OwnedHandle};
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "dev-tools")]
use std::process::Command;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

#[cfg(feature = "dev-tools")]
use media_support::WORKER;
use media_support::media_host::strikes::{FileId, Strikes};
use media_support::media_host::verdict;
use media_support::media_host::watch::{self, End, Event, LINE};
use media_support::{Kind, Launched, Setup, Stage, media_host, read_tight, recipe, root};
use skin_model::media::control::{Command as Order, Matrix, Range, Reply};
use skin_model::media::{Frame, Geometry, Reader, Window};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{
    FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows::Win32::System::Threading::SetEvent;

/// How long a reply may take.
const REPLY: Duration = Duration::from_secs(30);

/// Three and a half of the job watcher's periods: three of its reads.
const WATCHED: Duration = Duration::from_millis(350);

/// A worker the game's host launched under the recipe, staged in a
/// directory of its own, and the replies on its control pipe.
struct Worker {
    launched: Launched,
    _stage: Stage,
    /// The id of the last command sent.
    last: u32,
}

impl Worker {
    /// Launches the worker with `args` and reads its hello.
    fn start(args: &[&str]) -> Self {
        Self::on(Stage::new(), &recipe().args_of(args))
    }

    /// Launches the worker staged in `stage` under `setup` and reads its
    /// hello.
    fn on(stage: Stage, setup: &Setup) -> Self {
        let launched = stage.launch(setup).unwrap_or_else(|e| panic!("{e}"));
        Self {
            launched,
            _stage: stage,
            last: 0,
        }
    }

    fn send(&mut self, order: &Order) {
        self.last = self.launched.send(order).unwrap();
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
        self.play_as(player, ring, false)
    }

    /// Hands player `player` its ring, the video looping when `looping`.
    fn play_as(&mut self, player: u8, ring: &Ring, looping: bool) -> Reply {
        let section = self.give(HANDLE(ring.section.as_raw_handle()), Kind::Section);
        let event = self.give(HANDLE(ring.event.as_raw_handle()), Kind::Event);
        self.send(&Order::Play {
            player,
            section,
            event,
            looping,
        });
        self.reply()
    }

    /// Closes player `player`, past any reply of its still coming.
    fn close(&mut self, player: u8) {
        self.send(&Order::Close { player });
        loop {
            match self.reply() {
                Reply::Closed { id, player: p } if p == player && id == self.last => return,
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
    /// The last frame taken, I420.
    frame: Vec<u8>,
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
            frame: Vec::new(),
        }
    }

    fn words<'a>(view: MEMORY_MAPPED_VIEW_ADDRESS, len: usize) -> &'a [AtomicU64] {
        // SAFETY: as in `new`; the caller keeps the ring alive.
        unsafe { std::slice::from_raw_parts(view.Value.cast::<AtomicU64>(), len) }
    }

    /// Polls the ring at `due`, signalling the worker when slots were
    /// freed: the frame taken, if any.
    fn poll(&mut self, due: i64) -> Option<Frame> {
        let words = Self::words(self.view, self.len);
        let (taken, freed) = read_tight(&mut self.reader, words, due, &mut self.frame)
            .unwrap_or_else(|f| panic!("the ring failed its player: {f}"));
        if freed {
            // SAFETY: the ring's event.
            unsafe { SetEvent(HANDLE(self.event.as_raw_handle())) }.unwrap();
        }
        taken
    }

    /// The highest sequence the worker has published, its watermark.
    #[cfg(feature = "dev-tools")]
    fn published(&self) -> u64 {
        skin_model::media::watermark(Self::words(self.view, self.len)).unwrap()
    }

    /// The ring's section's size in bytes.
    fn bytes(&self) -> usize {
        self.reader.geometry().bytes()
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

    /// The last frame taken's bytes, I420.
    fn staged(&self) -> Vec<u8> {
        assert_eq!(self.frame.len(), self.reader.geometry().frame_bytes());
        self.frame.clone()
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
    let (taken, last) = take_to_last(worker, ring, player, first, interval);
    assert!(
        matches!(last, Reply::Ended { player: p, .. } if p == player),
        "{last:?} while player {player} plays; the worker's log:\n{}",
        worker.log()
    );
    taken
}

/// As [`take_all`], until the worker answers that the video ended or that
/// the player failed: the frames and that answer.
fn take_to_last(
    worker: &Worker,
    ring: &mut Ring,
    player: u8,
    first: i64,
    interval: i64,
) -> (Vec<Taken>, Reply) {
    let mut taken = Vec::new();
    let mut due = first + interval / 2;
    let mut last = None;
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
        if let Some(last) = last {
            return (taken, last);
        }
        match worker.try_reply() {
            Some(reply @ (Reply::Ended { player: p, .. } | Reply::Failed { player: p, .. }))
                if p == player =>
            {
                last = Some(reply)
            }
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
    assert_eq!(
        worker.play(1, &ring),
        Reply::Playing {
            id: worker.last,
            player: 1
        }
    );
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
        assert_eq!(
            worker.play(player, &ring),
            Reply::Playing {
                id: worker.last,
                player
            }
        );
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
    assert_eq!(
        worker.play(3, &ring),
        Reply::Playing {
            id: worker.last,
            player: 3
        }
    );
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
            id: worker.last,
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

/// A committed colour clip (`xtask/src/media.rs`).
fn colour_clip(name: &str) -> PathBuf {
    root().join("crates/player/tests/media").join(name)
}

/// The colour the solid colour clips are made of, Y, U and V, decoded
/// exactly (lossless H.264).
const SOLID: [u8; 3] = [100, 90, 190];

/// `yuv` of matrix and range `from`, as matrix and range `to` give the same
/// colour: through R'G'B' in floating point, with no rounding or clipping,
/// independent of FFmpeg's conversion.
fn converted(yuv: [u8; 3], from: (Matrix, Range), to: (Matrix, Range)) -> [f64; 3] {
    let k = |m| match m {
        Matrix::Bt709 => (0.2126, 0.0722),
        Matrix::Bt601 => (0.299, 0.114),
    };
    let [y, u, v] = yuv.map(f64::from);
    let (y, pb, pr) = match from.1 {
        Range::Limited => ((y - 16.0) / 219.0, (u - 128.0) / 224.0, (v - 128.0) / 224.0),
        Range::Full => (y / 255.0, (u - 128.0) / 255.0, (v - 128.0) / 255.0),
    };
    let (kr, kb) = k(from.0);
    let r = y + 2.0 * (1.0 - kr) * pr;
    let b = y + 2.0 * (1.0 - kb) * pb;
    let g = (y - kr * r - kb * b) / (1.0 - kr - kb);
    let (kr, kb) = k(to.0);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let pb = (b - y) / (2.0 * (1.0 - kb));
    let pr = (r - y) / (2.0 * (1.0 - kr));
    match to.1 {
        Range::Limited => [16.0 + 219.0 * y, 128.0 + 224.0 * pb, 128.0 + 224.0 * pr],
        Range::Full => [255.0 * y, 128.0 + 255.0 * pb, 128.0 + 255.0 * pr],
    }
}

/// Asserts every byte of each plane of an I420 frame `width` × `height` is
/// within 1 of `want`'s value for that plane.
#[track_caller]
fn assert_planes(bytes: &[u8], width: usize, height: usize, want: [f64; 3], what: &str) {
    let luma = width * height;
    let chroma = width.div_ceil(2) * height.div_ceil(2);
    let planes = [
        &bytes[..luma],
        &bytes[luma..luma + chroma],
        &bytes[luma + chroma..luma + 2 * chroma],
    ];
    for ((plane, want), name) in planes.into_iter().zip(want).zip(["Y", "U", "V"]) {
        let worst = plane
            .iter()
            .map(|b| (f64::from(*b) - want).abs())
            .fold(0.0, f64::max);
        assert!(
            worst <= 1.0,
            "{what}: a {name} byte {worst:.2} from {want:.2} (first {})",
            plane[0]
        );
    }
}

// Covers: T02
#[test]
fn probed_reports_the_colour_contract() {
    use Matrix::{Bt601, Bt709};
    use Range::{Full, Limited};

    // A matrix tag wins over the size; without one, the size guesses (709
    // from 1280 wide or over 576 high); a range tag wins; a JPEG format is
    // full range and an unsignalled range limited.
    let cases = [
        ("colour/bt709-640x480.mkv", Bt709, Limited),
        ("colour/bt601-1920x1080.mkv", Bt601, Limited),
        ("colour/untagged-1280x720.mkv", Bt709, Limited),
        ("colour/untagged-640x480.mkv", Bt601, Limited),
        ("colour/full-320x240.mkv", Bt601, Full),
        ("colour/mjpeg-320x240.avi", Bt601, Full),
        ("zlib/plain.mov", Bt601, Limited),
    ];
    let mut worker = Worker::start(&[]);
    for (player, (name, matrix, range)) in (1..).zip(cases) {
        match worker.open(player, &colour_clip(name)) {
            Reply::Probed {
                matrix: m,
                range: r,
                ..
            } => assert_eq!((m, r), (matrix, range), "{name}"),
            other => panic!("{name}: {other:?}; the worker's log:\n{}", worker.log()),
        }
    }
    for player in 1..=7 {
        worker.close(player);
    }
}

// Covers: T02
#[test]
fn full_range_i420_reaches_the_ring_as_reported() {
    let mut worker = Worker::start(&[]);
    let probe = worker.open(1, &colour_clip("colour/full-320x240.mkv"));
    assert!(
        matches!(
            probe,
            Reply::Probed {
                range: Range::Full,
                ..
            }
        ),
        "{probe:?}"
    );
    let mut ring = Ring::new(320, 240);
    assert_eq!(
        worker.play(1, &ring),
        Reply::Playing {
            id: worker.last,
            player: 1
        }
    );
    // Full-range white: luma 255, not squeezed to 235.
    let taken = take_one(&worker, &mut ring, 0);
    assert_planes(&taken.bytes, 320, 240, [255.0, 128.0, 128.0], "white");
    assert!(taken.bytes[..320 * 240].iter().all(|b| *b == 255));
    worker.close(1);
}

// Covers: T02
#[test]
fn colour_changes_mid_stream_are_converted_to_the_contract() {
    use Matrix::{Bt601, Bt709};
    use Range::{Full, Limited};

    // Two frames of one colour, then two whose range alone or matrix alone
    // differs. The stream's tags, which the container carries and the
    // probe reports, are the second half's (its parameter sets came last),
    // so the first two frames differ from the contract: those reach the
    // ring converted to it, the others as decoded. The gamut clip's colour
    // is outside the R'G'B' cube, its G' negative: converted with nothing
    // clipped before the end, it is (53, 59, 180), clipped first about
    // (62, 101, 173).
    for (name, solid, probed, first) in [
        (
            "colour/range-change-128x96.mkv",
            SOLID,
            (Bt601, Full),
            (Bt601, Limited),
        ),
        (
            "colour/matrix-change-128x96.mkv",
            SOLID,
            (Bt601, Limited),
            (Bt709, Limited),
        ),
        (
            "colour/gamut-change-128x96.mkv",
            [50, 64, 176],
            (Bt601, Limited),
            (Bt709, Limited),
        ),
    ] {
        let mut worker = Worker::start(&[]);
        match worker.open(1, &colour_clip(name)) {
            Reply::Probed { matrix, range, .. } => {
                assert_eq!((matrix, range), probed, "{name}");
            }
            other => panic!("{name}: {other:?}"),
        }
        let mut ring = Ring::new(128, 96);
        assert_eq!(
            worker.play(1, &ring),
            Reply::Playing {
                id: worker.last,
                player: 1
            }
        );
        let same = solid.map(f64::from);
        let into = converted(solid, first, probed);
        assert!(
            into.iter().zip(same).any(|(a, b)| (a - b).abs() > 3.0),
            "{name}: the change must show"
        );
        for (i, want) in [into, into, same, same].into_iter().enumerate() {
            let due = i64::try_from(i).unwrap() * 100_000;
            let taken = take_one(&worker, &mut ring, due);
            assert_eq!(taken.stamp, due, "{name}");
            assert_planes(&taken.bytes, 128, 96, want, &format!("{name}, frame {i}"));
        }
        worker.close(1);
    }
}

/// The clip the looping tests play: two seconds at 24 fps, whose last frame
/// carries its own stamp (as in
/// `worker_fails_a_player_refused_while_its_decoder_parks_at_the_end`).
#[cfg(feature = "dev-tools")]
const LOOPED: &str = "mpeg1video mpeg 320x240 24 48 12";

/// Polls `ring` at `due` until a frame is staged: its sequence, stamp and
/// painted index.
#[cfg(feature = "dev-tools")]
fn take_seq(worker: &Worker, ring: &mut Ring, due: i64) -> (u64, i64, u32) {
    let deadline = Instant::now() + REPLY;
    loop {
        if let Some(frame) = ring.poll(due) {
            return (frame.seq, frame.stamp, painted(&ring.staged(), 320));
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

/// A play whose clock is already a second in when the game first polls,
/// and that jumps on after a stall (design.md "Rendering and Skins", skin
/// movies, "Video frames"): the first frame the game takes is the one
/// due, and the worker dropped the frames behind the clock rather than
/// publish each for the game to free, so at most a ring's slots of them
/// (published before it saw the clock) came before it. After the stall the
/// game takes the newest frame the ring holds, late, and then the frame
/// due, the worker publishing none between them.
// Revert: the worker publishing every frame -> "frames behind the clock
// published".
#[cfg(feature = "dev-tools")]
#[test]
fn worker_drops_frames_behind_the_game_s_clock() {
    use skin_model::media::SLOTS;
    let dir = tempfile::tempdir().unwrap();
    let path = gen_clip(dir.path(), "b.mpg", "mpeg1video mpeg 320x240 24 48 12");
    let mut worker = Worker::start(&[]);
    let probe = worker.open(1, &path);
    assert!(
        matches!(probe, Reply::Probed { player: 1, .. }),
        "{probe:?}"
    );
    let mut ring = Ring::new(320, 240);
    assert_eq!(
        worker.play(1, &ring),
        Reply::Playing {
            id: worker.last,
            player: 1
        }
    );
    let slots = u64::try_from(SLOTS).unwrap();
    // The ring full of the clip's first frames, published before the game's
    // first poll.
    let deadline = Instant::now() + REPLY;
    while ring.published() < slots {
        assert!(Instant::now() < deadline, "the ring never filled");
        std::thread::sleep(Duration::from_millis(1));
    }
    let first = take_seq(&worker, &mut ring, stamp_of(24, 24));
    assert_eq!(
        (first.1, first.2),
        (stamp_of(24, 24), 24),
        "the frame due first"
    );
    assert!(
        first.0 <= slots + 1,
        "frames behind the clock published: frame 24 is sequence {}; the worker's log:\n{}",
        first.0,
        worker.log()
    );
    // A stall: the ring refills with frames 25 to 28 while the clock runs
    // to 40.
    std::thread::sleep(Duration::from_millis(300));
    let late = take_seq(&worker, &mut ring, stamp_of(40, 24));
    assert_eq!(
        (late.1, late.2),
        (stamp_of(28, 24), 28),
        "the newest late frame"
    );
    let later = take_seq(&worker, &mut ring, stamp_of(40, 24));
    assert_eq!((later.1, later.2), (stamp_of(40, 24), 40));
    assert!(
        later.0 == late.0 + 1,
        "frames behind the clock published: frame 40 is sequence {} after {}; the worker's log:\n{}",
        later.0,
        late.0,
        worker.log()
    );
    worker.close(1);
}

/// A seek back starts a run whose frames the worker judges only by the
/// game's time of that run (design.md "Rendering and Skins", video
/// frames): after the game has shown 5 s of a 10 s clip, a seek to 1 s
/// shows the frame there, not frames the worker published late after
/// dropping the run's frames by the earlier run's 5 s.
// Revert: the worker reading the game's time whatever its run -> "no frame
// due at".
#[cfg(feature = "dev-tools")]
#[test]
fn a_seek_back_is_judged_by_its_own_run_s_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = gen_clip(dir.path(), "k.mpg", "mpeg1video mpeg 320x240 24 240 12");
    let mut worker = Worker::start(&[]);
    let probe = worker.open(1, &path);
    assert!(
        matches!(probe, Reply::Probed { player: 1, .. }),
        "{probe:?}"
    );
    let mut ring = Ring::new(320, 240);
    assert_eq!(
        worker.play(1, &ring),
        Reply::Playing {
            id: worker.last,
            player: 1
        }
    );
    let shown = take_seq(&worker, &mut ring, stamp_of(120, 24));
    assert_eq!(shown.2, 120);
    let at = stamp_of(24, 24);
    let run = ring.start_run(at);
    worker.send(&Order::Seek { player: 1, run, at });
    let landed = match worker.reply() {
        Reply::Sought { landed, .. } => landed,
        other => panic!("{other:?}; the worker's log:\n{}", worker.log()),
    };
    assert_eq!(landed, at);
    // The worker decodes the new run before the game polls it, with only
    // the earlier run's time to read.
    std::thread::sleep(Duration::from_millis(200));
    let taken = take_seq(&worker, &mut ring, landed);
    assert_eq!((taken.1, taken.2), (at, 24));
    worker.close(1);
}

/// A decoder slower than the game's clock (each frame's conversion takes
/// 100 ms, a 24 fps clip's frame lasting 42 ms) shows frames that advance
/// (design.md "Rendering and Skins", video frames: a late frame is dropped
/// only for a newer one): the worker publishes a frame at least once a
/// catch-up bound, late, and the game takes each, so over two seconds of
/// polls on a real-time clock the panel shows several frames, each newer
/// than the last.
// Revert: the worker dropping every late frame, or the game freeing a late
// frame the worker published knowing the time -> "a slow decoder shows
// advancing frames".
#[cfg(feature = "dev-tools")]
#[test]
fn a_slow_decoder_shows_advancing_late_frames() {
    let dir = tempfile::tempdir().unwrap();
    let path = gen_clip(dir.path(), "w.mpg", "mpeg1video mpeg 320x240 24 240 12");
    let mut worker = Worker::start(&["--dev-serve", "slow-convert=100"]);
    let probe = worker.open(1, &path);
    assert!(
        matches!(probe, Reply::Probed { player: 1, .. }),
        "{probe:?}"
    );
    let mut ring = Ring::new(320, 240);
    assert_eq!(
        worker.play(1, &ring),
        Reply::Playing {
            id: worker.last,
            player: 1
        }
    );
    let origin = Instant::now();
    let mut shown = Vec::new();
    while origin.elapsed() < Duration::from_secs(2) {
        let due = i64::try_from(origin.elapsed().as_micros()).unwrap();
        if let Some(frame) = ring.poll(due) {
            shown.push(frame.stamp);
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    assert!(
        shown.len() >= 4 && shown.windows(2).all(|w| w[0] < w[1]),
        "a slow decoder shows advancing frames: {shown:?}; the worker's log:\n{}",
        worker.log()
    );
    worker.close(1);
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn looping_play_restarts_with_rising_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let path = gen_clip(dir.path(), "l.mpg", LOOPED);
    let mut worker = Worker::start(&[]);
    let probe = worker.open(1, &path);
    assert!(
        matches!(probe, Reply::Probed { player: 1, .. }),
        "{probe:?}"
    );
    let mut ring = Ring::new(320, 240);
    assert_eq!(
        worker.play_as(1, &ring, true),
        Reply::Playing {
            id: worker.last,
            player: 1
        }
    );
    // Three passes in one run: each frame shows its index in the clip, and
    // each pass's stamps follow the last's by the clip's length, 2 s.
    for pass in 0..3 {
        for i in 0..48 {
            let due = pass * 2_000_000 + stamp_of(i, 24);
            let taken = take_one(&worker, &mut ring, due);
            assert_eq!(
                (taken.stamp, painted(&taken.bytes, 320)),
                (due, u32::try_from(i).unwrap()),
                "pass {pass}; the worker's log:\n{}",
                worker.log()
            );
        }
    }
    // A looping play never ends.
    while let Some(reply) = worker.try_reply() {
        assert!(!matches!(reply, Reply::Ended { .. }), "{reply:?}");
    }
    worker.close(1);
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn looping_play_starts_each_pass_at_the_first_usable_frame() {
    let dir = tempfile::tempdir().unwrap();
    let whole = std::fs::read(gen_clip(dir.path(), "l.mpg", LOOPED)).unwrap();
    // The clip from its third pack on: the stream starts 6 frames before
    // its first key frame with frames that need the group of pictures
    // before them, so the first the decoder gives is frame 24, the key
    // frame, 6 frames after the stream's start.
    let packs: Vec<usize> = whole
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 1, 0xBA])
        .map(|(i, _)| i)
        .collect();
    let path = dir.path().join("cut.mpg");
    std::fs::write(&path, &whole[packs[2]..]).unwrap();
    let mut worker = Worker::start(&[]);
    let probe = worker.open(1, &path);
    assert!(
        matches!(probe, Reply::Probed { player: 1, .. }),
        "{probe:?}"
    );
    let mut ring = Ring::new(320, 240);
    assert_eq!(
        worker.play_as(1, &ring, true),
        Reply::Playing {
            id: worker.last,
            player: 1
        }
    );
    // Three passes of frames 24 to 47: each pass's first frame follows the
    // last pass's last frame by one frame, with no gap for the frames
    // before the first usable one, and shows frame 24 again.
    for m in 0..72 {
        let due = stamp_of(6 + m, 24);
        let taken = take_one(&worker, &mut ring, due);
        assert_eq!(
            (taken.stamp, painted(&taken.bytes, 320)),
            (due, u32::try_from(24 + m % 24).unwrap()),
            "frame {m}; the worker's log:\n{}",
            worker.log()
        );
    }
    worker.close(1);
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn looping_play_parks_and_reports_a_refusal_before_the_next_pass() {
    use skin_model::media::control::Failure;

    let dir = tempfile::tempdir().unwrap();
    let path = gen_clip(dir.path(), "l.mpg", LOOPED);
    // A refusal recorded at the park seam, where the decoder parks at the
    // clip's end before the loop seeks back.
    let mut worker = Worker::start(&["--dev-refuse-at-park"]);
    let probe = worker.open(1, &path);
    assert!(
        matches!(probe, Reply::Probed { player: 1, .. }),
        "{probe:?}"
    );
    let mut ring = Ring::new(320, 240);
    assert_eq!(
        worker.play_as(1, &ring, true),
        Reply::Playing {
            id: worker.last,
            player: 1
        }
    );
    // The first pass whole, then the refusal, and no frame of the next.
    let (taken, last) = take_to_last(&worker, &mut ring, 1, 0, stamp_of(1, 24));
    let taken: Vec<i64> = taken.iter().map(|t| t.stamp).collect();
    let pass: Vec<i64> = (0..48).map(|i| stamp_of(i, 24)).collect();
    assert_eq!(taken, pass, "{last:?}; the worker's log:\n{}", worker.log());
    assert_eq!(
        last,
        Reply::Failed {
            player: 1,
            why: Failure::Memory,
            code: -12,
        },
        "the worker's log:\n{}",
        worker.log()
    );
    assert_eq!(ring.poll(4_000_000), None);
    worker.close(1);
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn worker_fails_a_player_refused_while_its_decoder_parks_at_the_end() {
    use skin_model::media::control::Failure;

    let dir = tempfile::tempdir().unwrap();
    // Two seconds at 24 fps, a key frame every half second, at a size whose
    // last frame starts a packet of the file and so carries its own stamp:
    // at a smaller size it shares its packet with the frames before it, has
    // no stamp when the decoder gives it, and is skipped.
    let path = gen_clip(dir.path(), "p.mpg", "mpeg1video mpeg 320x240 24 48 12");
    let stamps: Vec<i64> = (0..48).map(|i| stamp_of(i, 24)).collect();
    // Without a seam each player ends. With the park seam, which records a
    // refusal for the player each time it parks its decoder at a video's
    // end, after the decoder gave the end (as a frame thread still decoding
    // when its other thread gave the end would), each fails there instead.
    for seam in [false, true] {
        let args: &[&str] = if seam { &["--dev-refuse-at-park"] } else { &[] };
        let mut worker = Worker::start(args);
        let refused = |player| Reply::Failed {
            player,
            why: Failure::Memory,
            code: -12,
        };

        // Played to its end: every frame, then the answer.
        let probe = worker.open(1, &path);
        assert!(
            matches!(probe, Reply::Probed { player: 1, .. }),
            "{probe:?}"
        );
        let mut ring = Ring::new(320, 240);
        assert_eq!(
            worker.play(1, &ring),
            Reply::Playing {
                id: worker.last,
                player: 1
            }
        );
        let (taken, last) = take_to_last(&worker, &mut ring, 1, 0, stamp_of(1, 24));
        let taken: Vec<i64> = taken.iter().map(|t| t.stamp).collect();
        assert_eq!(
            taken,
            stamps,
            "seam {seam}: {last:?}; the worker's log:\n{}",
            worker.log()
        );
        if seam {
            assert_eq!(last, refused(1), "the worker's log:\n{}", worker.log());
        } else {
            assert!(matches!(last, Reply::Ended { player: 1, .. }), "{last:?}");
        }

        // Sought past its end: the answer, with the ring full and the
        // player waiting for a slot short of the end.
        let probe = worker.open(2, &path);
        assert!(
            matches!(probe, Reply::Probed { player: 2, .. }),
            "{probe:?}"
        );
        let mut ring = Ring::new(320, 240);
        assert_eq!(
            worker.play(2, &ring),
            Reply::Playing {
                id: worker.last,
                player: 2
            }
        );
        let run = ring.start_run(10_000_000);
        worker.send(&Order::Seek {
            player: 2,
            run,
            at: 10_000_000,
        });
        let want = if seam {
            refused(2)
        } else {
            Reply::Ended { player: 2, run }
        };
        assert_eq!(worker.reply(), want, "the worker's log:\n{}", worker.log());
        for player in [1, 2] {
            worker.close(player);
        }
    }
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
    assert_eq!(
        worker.play(5, &ring),
        Reply::Playing {
            id: worker.last,
            player: 5
        }
    );
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
    assert_eq!(
        worker.play(1, &ring),
        Reply::Playing {
            id: worker.last,
            player: 1
        }
    );
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

/// How far the ENOMEM tests take player 2's frames while player 1 runs to
/// its failure: half of player 2's clip. Each player decodes on a thread of
/// its own, as fast as its ring is emptied, so player 2's ring stays full
/// past this stamp instead, and it cannot end first however the threads are
/// scheduled.
#[cfg(feature = "dev-tools")]
const HOLD: i64 = 30_000_000;

/// Player 2's media time once player 1 has failed: on from [`HOLD`] in
/// real time since `resumed`, so its frames come due one by one rather than
/// all behind the clock and dropped (design.md "Rendering and Skins", video
/// frames).
#[cfg(feature = "dev-tools")]
fn after_hold(resumed: Instant) -> i64 {
    HOLD + i64::try_from(resumed.elapsed().as_micros()).unwrap()
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
        assert_eq!(
            worker.play(player, &ring),
            Reply::Playing {
                id: worker.last,
                player
            }
        );
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
        frames[1] += usize::from(two.poll(HOLD).is_some());
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
    let resumed = Instant::now();
    while frames[1] < before + 24 {
        assert!(
            Instant::now() < deadline,
            "the worker's log:\n{}",
            worker.log()
        );
        frames[1] += usize::from(two.poll(after_hold(resumed)).is_some());
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
    // The refusal was FFmpeg's own: the job refused nothing, and no job
    // message came.
    let events = worker.launched.watch_events();
    assert!(
        !events.iter().any(|e| matches!(e, Event::Message { .. })),
        "{events:?}"
    );
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
        assert_eq!(
            worker.play(player, &ring),
            Reply::Playing {
                id: worker.last,
                player
            }
        );
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
        assert_eq!(
            worker.play(player, &ring),
            Reply::Playing {
                id: worker.last,
                player
            }
        );
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

/// The payload of IDR slice `i` of a grey picture of `coded` pixels, which
/// decodes: an I slice of the whole picture, deblocking off, each
/// macroblock predicted from its neighbours' mean (I_16x16, DC) with no
/// residual.
fn h264_grey(i: u64, coded: (u32, u32)) -> Vec<u8> {
    let mut bits = Bits::default();
    bits.ue(0)
        .ue(7)
        .ue(0)
        .u(4, 0)
        .ue(i)
        .bit(false)
        .bit(false)
        .se(0)
        .ue(1);
    for _ in 0..(coded.0 / 16) * (coded.1 / 16) {
        // The macroblock's type, its chroma's DC prediction, no change of
        // quantizer, and no coefficient in its luma's DC block.
        bits.ue(3).ue(0).se(0).bit(true);
    }
    bits.rbsp()
}

/// One part of an H.264 stream: the pixels its sequence codes, the pixels
/// it crops them to, and how many grey IDR pictures it holds.
type Part = ((u32, u32), (u32, u32), u64);

/// The access units of an H.264 stream (Annex B) in parts, each a
/// sequence that codes `coded` pixels, crops them to `shown` and holds
/// `frames` grey IDR pictures, its parameter sets before its first.
fn h264_parts(parts: &[Part]) -> Vec<Vec<u8>> {
    let mut units = Vec::new();
    for &(coded, shown, frames) in parts {
        let (sps, pps) = h264_sets(coded, shown);
        for f in 0..frames {
            let mut unit = Vec::new();
            if f == 0 {
                unit.extend(nal(0x67, &sps));
                unit.extend(nal(0x68, &pps));
            }
            let i = u64::try_from(units.len()).unwrap();
            unit.extend(nal(0x65, &h264_grey(i, coded)));
            units.push(unit);
        }
    }
    units
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
            id: worker.last,
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
            id: worker.last,
            player: 1,
            why: Refusal::Size,
            code: 0,
        },
        "the worker's log:\n{}",
        worker.log()
    );
    worker.close(1);
}

// Covers: T02
#[test]
fn worker_caps_the_coded_size_of_h264_frames_after_the_probe() {
    use skin_model::media::control::Failure;

    let dir = tempfile::tempdir().unwrap();
    // Ten seconds of H.264 at 64 × 48, then a sequence the probe never
    // reads, coded at 8 192 × 4 096 (48 MiB a frame) and cropped to
    // 1 920 × 1 080: FFmpeg's own cap reads the shown size, the worker's
    // allocation refuses the coded one on the frame thread FFmpeg started
    // for the player, and the decoder goes on from each such frame as from
    // a damaged one.
    let path = dir.path().join("grows.avi");
    let units = h264_parts(&[((64, 48), (64, 48), 240), ((8192, 4096), (1920, 1080), 4)]);
    std::fs::write(&path, avi((64, 48), 24, b"H264", &units)).unwrap();

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
    assert_eq!(
        worker.play(1, &ring),
        Reply::Playing {
            id: worker.last,
            player: 1
        }
    );
    let deadline = Instant::now() + REPLY;
    let mut taken = Vec::new();
    let end = loop {
        assert!(
            Instant::now() < deadline,
            "{} frames; the worker's log:\n{}",
            taken.len(),
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
    // The first frame past the cap fails the player, with the error the
    // worker's allocation gave; every frame before it came.
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
    assert!(worker.log().contains("refused a frame of 8192 x 4096"));
    assert_eq!(
        taken.last(),
        Some(&((239 * 1_000_000 + 12) / 24)),
        "{taken:?}"
    );
    // Nothing of a frame of the coded size was allocated.
    let peak = worker.peak_commit();
    assert!(
        peak < before + (32 << 20),
        "the worker's peak commit rose from {before} to {peak}"
    );
    worker.close(1);
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn h264_decoder_enomem_fails_one_player_and_the_worker_lives() {
    use skin_model::media::control::Failure;

    let dir = tempfile::tempdir().unwrap();
    // Player 1 plays ten seconds of H.264 at 64 × 48 and then 1 920 × 1 080
    // frames, whose planes are larger than the lowered cap on one
    // allocation. FFmpeg's decoder, on the frame threads it started for
    // the player, goes on from a picture it could not allocate as from a
    // damaged one; the refusal, recorded for the player, fails it. Player
    // 2 plays 64 × 48 frames throughout.
    let grows = dir.path().join("grows.avi");
    let units = h264_parts(&[((64, 48), (64, 48), 240), ((1920, 1088), (1920, 1080), 4)]);
    std::fs::write(&grows, avi((64, 48), 24, b"H264", &units)).unwrap();
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
        assert_eq!(
            worker.play(player, &ring),
            Reply::Playing {
                id: worker.last,
                player
            }
        );
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
        frames[1] += usize::from(two.poll(HOLD).is_some());
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
    // Player 2's frames keep coming.
    let before = frames[1];
    let resumed = Instant::now();
    while frames[1] < before + 24 {
        assert!(
            Instant::now() < deadline,
            "the worker's log:\n{}",
            worker.log()
        );
        frames[1] += usize::from(two.poll(after_hold(resumed)).is_some());
        assert_eq!(worker.try_reply(), None, "player 2 ended or failed early");
    }
    for player in [1, 2] {
        worker.close(player);
    }
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
            id: worker.last,
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
        assert_eq!(
            worker.play(player, &ring),
            Reply::Playing {
                id: worker.last,
                player
            }
        );
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
        frames[1] += usize::from(two.poll(HOLD).is_some());
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
    let resumed = Instant::now();
    while frames[1] < before + 24 {
        assert!(
            Instant::now() < deadline,
            "the worker's log:\n{}",
            worker.log()
        );
        frames[1] += usize::from(two.poll(after_hold(resumed)).is_some());
        assert_eq!(worker.try_reply(), None, "player 2 ended or failed early");
    }
    for player in [1, 2] {
        worker.close(player);
    }
    // The refusal was the worker's own: the job refused nothing, and no job
    // message came.
    let events = worker.launched.watch_events();
    assert!(
        !events.iter().any(|e| matches!(e, Event::Message { .. })),
        "{events:?}"
    );
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
            id: worker.last,
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

/// A worker committing memory with `--dev-balloon args` (MiB), staged in
/// `stage`, its job's watcher changed as `variation` says.
#[cfg(feature = "dev-tools")]
fn balloon(stage: &Stage, args: &str, variation: watch::Variation) -> Launched {
    let mut all = vec!["--dev-balloon"];
    all.extend(args.split(' '));
    stage
        .launch(&recipe().args_of(&all).watching_as(variation))
        .unwrap_or_else(|e| panic!("{e}"))
}

/// Waits until `worker` has logged a line holding `text`, or its watcher
/// has seen it end: its log.
#[cfg(feature = "dev-tools")]
fn logged(worker: &Launched, text: &str) -> String {
    let deadline = Instant::now() + REPLY;
    loop {
        let log = worker.log();
        if log.contains(text) || worker.end().is_some() {
            return log;
        }
        assert!(
            Instant::now() < deadline,
            "no {text:?} in the worker's log:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until `worker`'s watcher has seen it end: how it ended.
fn ended(worker: &Launched) -> End {
    let deadline = Instant::now() + REPLY;
    loop {
        if let Some(end) = worker.end() {
            return end;
        }
        assert!(
            Instant::now() < deadline,
            "the worker did not end; its watcher saw {:?}; its log:\n{}",
            worker.watch_events(),
            worker.log()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The peaks the watcher's reads found, in order.
#[cfg(feature = "dev-tools")]
fn reads(events: &[Event]) -> Vec<usize> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Read { peak, .. } => Some(*peak),
            _ => None,
        })
        .collect()
}

/// Checks that the watcher's reads keep to its schedule of one every
/// 100 ms from its start: each read is due at a tick of that schedule,
/// counted from 1, the ticks rise from read to read (a packet's wake adds
/// no read), and no read comes before its tick. How late a read comes is
/// the scheduler's and is not checked: a loaded machine delays it.
#[cfg(feature = "dev-tools")]
fn on_schedule(events: &[Event]) {
    let reads: Vec<(Duration, u32)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Read { at, tick, .. } => Some((*at, *tick)),
            _ => None,
        })
        .collect();
    assert!(
        reads
            .iter()
            .all(|&(at, tick)| tick >= 1 && at >= Duration::from_millis(100) * tick)
            && reads.windows(2).all(|w| w[0].1 < w[1].1),
        "{events:?}"
    );
}

/// The commit `process` holds now, in bytes.
#[cfg(feature = "dev-tools")]
fn commit_now(process: HANDLE) -> usize {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};

    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    // SAFETY: the worker's process handle, which has every access, and a
    // buffer of the size given.
    unsafe {
        GetProcessMemoryInfo(
            process,
            &raw mut counters,
            u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).unwrap(),
        )
    }
    .unwrap();
    counters.PagefileUsage
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn job_memory_cap_refuses_and_the_parent_terminates() {
    let stage = Stage::new();
    // Both of the watcher's paths off: the job refuses the commit that
    // would take the worker past its cap (ERROR_COMMITMENT_LIMIT), and the
    // worker goes on.
    let off = watch::Variation {
        no_message: true,
        no_accounting: true,
        ..watch::Variation::default()
    };
    let worker = balloon(&stage, "32 640", off);
    let log = logged(&worker, "refused with");
    assert!(log.contains("balloon: 32 MiB refused with 1455"), "{log}");
    std::thread::sleep(WATCHED);
    assert!(!worker.ended(), "{}", worker.log());
    assert_eq!(worker.end(), None);
    let peak = worker.job_peak().unwrap();
    assert!((LINE..=512 << 20).contains(&peak), "{peak}");
    drop(worker);
    // Both on: the watcher ends the worker once its peak has reached the
    // line, on the job's message if that came before the next scheduled
    // read, else at that read.
    let worker = balloon(&stage, "32 640", watch::Variation::default());
    let end = ended(&worker);
    let code = worker.wait_exit(REPLY);
    let events = worker.watch_events();
    let peak = worker.job_peak().unwrap();
    assert!(peak >= LINE, "ended at a peak of {peak}: {events:?}");
    let message = events.iter().any(|e| matches!(e, Event::Message { .. }));
    let over = reads(&events).iter().any(|&p| p >= LINE);
    match end {
        End::Message => {
            assert_eq!(code, Some(verdict::ENDED_BY_MESSAGE));
            assert!(message && !over, "{events:?}");
        }
        End::Line => {
            assert_eq!(code, Some(verdict::ENDED_AT_LINE));
            assert!(!message && over, "{events:?}");
        }
        End::Exited(c) => panic!("the worker ended by itself ({c:#x}): {events:?}"),
    }
    eprintln!("ended on {end:?} at a peak of {peak}; the watcher saw {events:?}");
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn job_memory_message_alone_terminates_below_the_line() {
    use windows::Win32::System::SystemServices::JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT;

    let stage = Stage::new();
    // The handler: below the line, where the accounting read cannot act,
    // the job's message, posted to the job's port as the job posts it,
    // ends the worker.
    let worker = balloon(&stage, "32 384", watch::Variation::default());
    let log = logged(&worker, "balloon: done");
    assert!(log.contains("balloon: committed 384 MiB"), "{log}");
    let peak = worker.job_peak().unwrap();
    assert!(peak < LINE, "{peak}");
    // The same message about another process wakes the watcher and ends
    // nothing, nor moves its reads.
    worker
        .post_job_message_about(JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT, worker.pid() ^ 4)
        .unwrap();
    std::thread::sleep(WATCHED);
    let events = worker.watch_events();
    assert!(!worker.ended() && worker.end().is_none(), "{events:?}");
    assert!(
        !events.iter().any(|e| matches!(e, Event::Message { .. })),
        "{events:?}"
    );
    worker
        .post_job_message(JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT)
        .unwrap();
    assert_eq!(ended(&worker), End::Message);
    assert_eq!(worker.wait_exit(REPLY), Some(verdict::ENDED_BY_MESSAGE));
    let events = worker.watch_events();
    assert!(reads(&events).iter().all(|&p| p < LINE), "{events:?}");
    on_schedule(&events);
    drop(worker);
    // The system's own message, observed rather than required: 128 MiB at
    // once past the cap is refused, and the peak stays below the line,
    // which a refused commit does not raise. A message that comes ends the
    // worker, maybe before it logs the refusal, which the message itself
    // then reports.
    let worker = balloon(&stage, "32 384 128", watch::Variation::default());
    let log = logged(&worker, "balloon: done");
    std::thread::sleep(WATCHED);
    let peak = worker.job_peak().unwrap();
    let events = worker.watch_events();
    assert!(peak < LINE, "{peak}: {events:?}");
    assert!(reads(&events).iter().all(|&p| p < LINE), "{events:?}");
    match worker.end() {
        Some(End::Message) => {
            let at = events.iter().find_map(|e| match e {
                Event::Message { at, .. } => Some(*at),
                _ => None,
            });
            eprintln!("the job's own message came at {at:?} and ended the worker");
        }
        None => {
            assert!(log.contains("balloon: 128 MiB refused with 1455"), "{log}");
            eprintln!("the job's own message did not come; the worker lives");
        }
        other => panic!("{other:?}: {events:?}"),
    }
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn job_memory_cap_terminates_when_the_message_is_lost() {
    let dir = tempfile::tempdir().unwrap();
    let clip = gen_clip(dir.path(), "steady.mpg", "mpeg1video mpeg 64x48 24 240 12");
    let file = std::fs::File::open(&clip).unwrap();
    let mut strikes = Strikes::default();
    let stage = Stage::new();
    // The job's message discarded before the watcher sees it: the first
    // scheduled read at or past the line ends the worker.
    let lost = watch::Variation {
        discard_message: true,
        ..watch::Variation::default()
    };
    let mut worker = balloon(&stage, "32 640", lost);
    worker.open(1, &file, &strikes).unwrap();
    assert_eq!(ended(&worker), End::Line);
    assert_eq!(worker.wait_exit(REPLY), Some(verdict::ENDED_AT_LINE));
    let events = worker.watch_events();
    let peaks = reads(&events);
    let (last, before) = peaks.split_last().unwrap();
    assert!(
        *last >= LINE && before.iter().all(|&p| p < LINE),
        "{events:?}"
    );
    let watched: Vec<_> = events
        .iter()
        .filter(|e| {
            !matches!(
                e,
                Event::Message {
                    discarded: true,
                    ..
                }
            )
        })
        .collect();
    assert!(
        matches!(watched.last(), Some(Event::Ended { end: End::Line, .. })),
        "{events:?}"
    );
    assert!(
        !watched.iter().any(|e| matches!(e, Event::Message { .. })),
        "{events:?}"
    );
    on_schedule(&events);
    eprintln!("the watcher saw {events:?}");
    // The game records what it records on the message: the player failed,
    // and its file has a strike.
    let mut failed = Vec::new();
    assert_eq!(
        worker.reap(&mut strikes, |p| failed.push(p)),
        Some(End::Line)
    );
    assert_eq!(failed, [1]);
    assert_eq!(strikes.of(FileId::of(&file).unwrap()), 1);
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn each_launch_gets_a_job_of_its_own() {
    let stage = Stage::new();
    let first = balloon(&stage, "32 640", watch::Variation::default());
    assert!(matches!(ended(&first), End::Message | End::Line));
    assert!(first.job_peak().unwrap() >= LINE);
    drop(first);
    // The next launch's worker is in a job of its own: its reads find its
    // own peak, below the line, and it survives them and decodes.
    let mut worker = Worker::on(stage, &recipe());
    std::thread::sleep(WATCHED);
    let events = worker.launched.watch_events();
    let peaks = reads(&events);
    assert!(
        peaks.len() >= 3 && peaks.iter().all(|&p| p < LINE),
        "{events:?}"
    );
    on_schedule(&events);
    let fixture = root().join("crates/player/tests/media/zlib/plain.mkv");
    let probe = worker.open(1, &fixture);
    let Reply::Probed { width, height, .. } = probe else {
        panic!("{probe:?}; the worker's log:\n{}", worker.log());
    };
    let mut ring = Ring::new(width, height);
    assert_eq!(
        worker.play(1, &ring),
        Reply::Playing {
            id: worker.last,
            player: 1
        }
    );
    assert_eq!(take_all(&worker, &mut ring, 1, 0, 33_333).len(), 120);
    worker.close(1);
    assert_eq!(worker.launched.end(), None);
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn job_memory_cap_terminates_on_a_crossing_released_between_reads() {
    let stage = Stage::new();
    // The watcher's reads an hour apart, so it reads only when the test
    // asks it to. 416 MiB, below the line, then 64 MiB at once, past it,
    // released at once; once the release is logged and the worker's commit
    // is back below the line, the read the test asks for finds the peak at
    // the line and ends the worker.
    let slow = watch::Variation {
        period: Some(Duration::from_secs(3600)),
        ..watch::Variation::default()
    };
    let worker = balloon(&stage, "32 416 64 free", slow);
    let log = logged(&worker, "balloon: done");
    assert!(
        log.contains("balloon: committed 480 MiB and released 64 MiB (Ok(()))"),
        "{log}"
    );
    let now = commit_now(worker.process());
    assert!(now < LINE, "{now}");
    assert!(worker.job_peak().unwrap() >= 480 << 20);
    assert_eq!(worker.end(), None, "{:?}", worker.watch_events());
    worker.read_now().unwrap();
    assert_eq!(ended(&worker), End::Line);
    assert_eq!(worker.wait_exit(REPLY), Some(verdict::ENDED_AT_LINE));
    let events = worker.watch_events();
    assert!(
        matches!(
            events.as_slice(),
            [
                Event::Read { tick: 0, peak, .. },
                Event::Ended { end: End::Line, .. }
            ] if *peak >= 480 << 20
        ),
        "{events:?}"
    );
}

/// Ends the worker whose process `process` is, as a crash does, and waits
/// until it has ended: the hook a test runs just before the watcher ends
/// the worker's job.
#[cfg(feature = "dev-tools")]
fn crash_now(process: HANDLE) {
    use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};

    // SAFETY: the watcher's handle to the worker's process, which has
    // every access.
    unsafe {
        let _ = TerminateProcess(process, CRASH);
        let _ = WaitForSingleObject(process, 30_000);
    }
}

// Covers: T02
#[test]
fn a_memory_message_after_a_crash_records_the_crash() {
    use windows::Win32::System::SystemServices::JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT;
    use windows::Win32::System::Threading::TerminateProcess;

    // The watcher's reads an hour apart, so none comes while the test acts:
    // the job's memory message, queued after the worker crashed, is the
    // first thing the watcher takes. Its termination finds the worker
    // ended, and the end recorded is the crash.
    let stage = Stage::new();
    let slow = watch::Variation {
        period: Some(Duration::from_secs(3600)),
        ..watch::Variation::default()
    };
    let worker = stage.launch(&recipe().watching_as(slow)).unwrap();
    // SAFETY: the worker's process handle, which has every access.
    unsafe { TerminateProcess(worker.process(), CRASH) }.unwrap();
    assert_eq!(worker.wait_exit(REPLY), Some(CRASH));
    worker
        .post_job_message(JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT)
        .unwrap();
    assert_eq!(ended(&worker), End::Exited(CRASH));
    let events = worker.watch_events();
    assert!(
        matches!(
            events.as_slice(),
            [
                Event::Message {
                    discarded: false,
                    ..
                },
                Event::Ended {
                    end: End::Exited(CRASH),
                    ..
                }
            ]
        ),
        "{events:?}"
    );
}

// Covers: T02
#[cfg(feature = "dev-tools")]
#[test]
fn a_crash_before_the_line_termination_records_the_crash() {
    // The job's message discarded, a read finds the peak at the line, and
    // the worker crashes after the watcher last looked at it and before the
    // watcher ends its job (a hook the watcher runs there): the end
    // recorded is the crash.
    let stage = Stage::new();
    let racing = watch::Variation {
        discard_message: true,
        before_terminate: Some(crash_now),
        ..watch::Variation::default()
    };
    let worker = balloon(&stage, "32 640", racing);
    assert_eq!(ended(&worker), End::Exited(CRASH));
    assert_eq!(worker.wait_exit(REPLY), Some(CRASH));
    let events = worker.watch_events();
    assert!(
        reads(&events).last().is_some_and(|&p| p >= LINE),
        "{events:?}"
    );
}

// Covers: T02
#[test]
fn a_failed_termination_keeps_the_worker_watched() {
    use windows::Win32::System::SystemServices::JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT;
    use windows::Win32::System::Threading::TerminateProcess;

    let stage = Stage::new();
    // The watcher's first termination fails: the worker lives, and the
    // watcher's next read asks again, which ends it with the message's exit
    // code, the end recorded.
    let once = watch::Variation {
        failed_terminations: 1,
        ..watch::Variation::default()
    };
    let worker = stage.launch(&recipe().watching_as(once)).unwrap();
    worker
        .post_job_message(JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT)
        .unwrap();
    assert_eq!(ended(&worker), End::Message);
    assert_eq!(worker.wait_exit(REPLY), Some(verdict::ENDED_BY_MESSAGE));
    drop(worker);
    // Every termination fails: nothing is recorded while the worker lives,
    // and the end it comes to by itself is recorded as its own.
    let never = watch::Variation {
        failed_terminations: u8::MAX,
        ..watch::Variation::default()
    };
    let worker = stage.launch(&recipe().watching_as(never)).unwrap();
    worker
        .post_job_message(JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT)
        .unwrap();
    std::thread::sleep(WATCHED);
    let events = worker.watch_events();
    assert!(!worker.ended() && worker.end().is_none(), "{events:?}");
    // SAFETY: the worker's process handle, which has every access.
    unsafe { TerminateProcess(worker.process(), CRASH) }.unwrap();
    assert_eq!(ended(&worker), End::Exited(CRASH));
}

// Covers: T02
#[test]
fn a_watch_whose_stop_packet_is_lost_still_stops() {
    // Neither stop packet is posted, to the watcher's port or to the port
    // the job's messages are discarded from: both threads stop at their
    // next timed wake, close their handles, and closing the job ends the
    // worker.
    let stage = Stage::new();
    let lost = watch::Variation {
        discard_message: true,
        failed_stop_post: true,
        ..watch::Variation::default()
    };
    let mut launched = stage.launch(&recipe().watching_as(lost)).unwrap();
    let mut worker = launched.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        worker.close_job();
        let _ = tx.send(worker);
    });
    let worker = rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|e| panic!("the watcher did not stop ({e})"));
    assert!(
        launched.wait_exit(REPLY).is_some(),
        "the job kept the worker"
    );
    drop(worker);
}

/// How many views of a section `bytes` long `process` has mapped.
fn views(process: HANDLE, bytes: usize) -> usize {
    use windows::Win32::System::Memory::{MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQueryEx};

    let mut mapped: BTreeMap<usize, usize> = BTreeMap::new();
    let mut at = 0_usize;
    loop {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: the worker's process handle, which has every access, and
        // a struct of the size given.
        let n = unsafe {
            VirtualQueryEx(
                process,
                Some(std::ptr::without_provenance(at)),
                &raw mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if n == 0 {
            break;
        }
        if info.Type == MEM_MAPPED {
            *mapped.entry(info.AllocationBase.addr()).or_default() += info.RegionSize;
        }
        at = info.BaseAddress.addr() + info.RegionSize;
    }
    let size = bytes.next_multiple_of(4096);
    mapped.values().filter(|&&s| s == size).count()
}

/// Opens `path` as player `player` and hands it a ring of its size.
fn played(worker: &mut Worker, player: u8, path: &Path) -> Ring {
    let probe = worker.open(player, path);
    let Reply::Probed { width, height, .. } = probe else {
        panic!("{probe:?}; the worker's log:\n{}", worker.log());
    };
    let ring = Ring::new(width, height);
    assert_eq!(
        worker.play(player, &ring),
        Reply::Playing {
            id: worker.last,
            player
        }
    );
    ring
}

/// Empties every ring as fast as the worker fills it until each player's
/// video has ended: the frames each staged.
fn drain(worker: &Worker, rings: &mut BTreeMap<u8, Ring>) -> BTreeMap<u8, usize> {
    let mut shown: BTreeMap<u8, usize> = rings.keys().map(|&p| (p, 0)).collect();
    let mut playing: Vec<u8> = rings.keys().copied().collect();
    let deadline = Instant::now() + Duration::from_secs(120);
    while !playing.is_empty() {
        assert!(
            Instant::now() < deadline,
            "{playing:?} still playing; the worker's log:\n{}",
            worker.log()
        );
        for (player, ring) in rings.iter_mut() {
            if ring.poll(i64::MAX).is_some() {
                *shown.get_mut(player).unwrap() += 1;
            }
        }
        match worker.try_reply() {
            Some(Reply::Ended { player, .. }) if playing.contains(&player) => {
                playing.retain(|&p| p != player);
            }
            Some(other) => panic!("{other:?} while {playing:?} play"),
            None => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    shown
}

// Covers: T02
#[test]
fn four_largest_players_stay_below_the_termination_line() {
    let h264 = root().join("crates/player/tests/media/h264");
    let wide = h264.join("h264-2560x1080.mp4");
    let full = h264.join("h264-1920x1080.mp4");
    let mut worker = Worker::start(&[]);
    // Two players at 2 560 × 1 080 and two at 1 920 × 1 080, in H.264 with
    // B-frames, each decoding into its ring, all four at once.
    let mut rings = BTreeMap::new();
    for (player, path) in [(1, &wide), (2, &wide), (3, &full), (4, &full)] {
        rings.insert(player, played(&mut worker, player, path));
    }
    let shown = drain(&worker, &mut rings);
    assert!(shown.values().all(|&n| n > 0), "{shown:?}");
    let process = worker.launched.process();
    let bytes = rings[&1].bytes();
    assert_eq!(views(process, bytes), 2);
    // One closed and opened again: the worker holds no view of the closed
    // player's section, and the new one decodes beside the others.
    worker.close(1);
    rings.remove(&1);
    assert_eq!(views(process, bytes), 1);
    let mut again = BTreeMap::from([(1, played(&mut worker, 1, &wide))]);
    drain(&worker, &mut again);
    assert_eq!(views(process, bytes), 2);
    let peak = worker.launched.job_peak().unwrap();
    eprintln!("the job's peak: {peak} bytes ({} MiB)", peak >> 20);
    assert!(peak <= LINE - (32 << 20), "{peak}");
    for player in [1, 2, 3, 4] {
        worker.close(player);
    }
}

/// The exit code the strikes test ends its workers with: an access
/// violation's, as a crash's.
const CRASH: u32 = 0xc000_0005;

// Covers: T02
#[test]
fn a_file_with_two_strikes_is_not_reopened_that_run() {
    use media_host::OpenError;
    use windows::Win32::System::Threading::TerminateProcess;

    // Each open opens its file anew, as the game does: duplicates of one
    // handle share its position.
    let fixture = root().join("crates/player/tests/media/zlib/plain.mkv");
    let other_fixture = root().join("crates/player/tests/media/zlib/plain.mov");
    let open = |path: &Path| std::fs::File::open(path).unwrap();
    let id = FileId::of(&open(&fixture)).unwrap();
    let other_id = FileId::of(&open(&other_fixture)).unwrap();
    let mut strikes = Strikes::default();
    let stage = Stage::new();
    // The file is open in two workers that crash, in each as two players
    // through handles of their own: a strike for each worker, and both
    // players fail. Another, which each closed before it crashed, gets
    // none.
    for strike in 1..=2 {
        let mut worker = stage.launch(&recipe()).unwrap();
        worker.open(1, &open(&fixture), &strikes).unwrap();
        worker.open(2, &open(&other_fixture), &strikes).unwrap();
        worker.open(3, &open(&fixture), &strikes).unwrap();
        worker.close(2).unwrap();
        loop {
            match worker.recv(REPLY).unwrap() {
                Reply::Closed { player: 2, .. } => break,
                Reply::Probed { .. } => {}
                reply => panic!("{reply:?}"),
            }
        }
        // SAFETY: the worker's process handle, which has every access.
        unsafe { TerminateProcess(worker.process(), CRASH) }.unwrap();
        let deadline = Instant::now() + REPLY;
        let mut failed = Vec::new();
        let end = loop {
            if let Some(end) = worker.reap(&mut strikes, |p| failed.push(p)) {
                break end;
            }
            assert!(Instant::now() < deadline, "{:?}", worker.watch_events());
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(end, End::Exited(CRASH));
        assert_eq!(failed, [1, 3]);
        assert_eq!(strikes.of(id), strike);
        assert_eq!(strikes.of(other_id), 0);
    }
    // It is not opened again in the run, whatever handle opens it; the other
    // file is.
    let mut worker = stage.launch(&recipe()).unwrap();
    assert!(matches!(
        worker.open(1, &open(&fixture), &strikes),
        Err(OpenError::Struck)
    ));
    worker.open(2, &open(&other_fixture), &strikes).unwrap();
    let probe = worker.recv(REPLY).unwrap();
    assert!(
        matches!(probe, Reply::Probed { player: 2, .. }),
        "{probe:?}"
    );
}

/// Ends `worker` as a crash does and reaps it: the players that failed.
fn crash_and_reap(worker: &mut Launched, strikes: &mut Strikes) -> Vec<u8> {
    use windows::Win32::System::Threading::TerminateProcess;

    // SAFETY: the worker's process handle, which has every access.
    unsafe { TerminateProcess(worker.process(), CRASH) }.unwrap();
    let deadline = Instant::now() + REPLY;
    let mut failed = Vec::new();
    loop {
        if let Some(end) = worker.reap(strikes, |p| failed.push(p)) {
            assert_eq!(end, End::Exited(CRASH));
            return failed;
        }
        assert!(Instant::now() < deadline, "{:?}", worker.watch_events());
        std::thread::sleep(Duration::from_millis(5));
    }
}

// Covers: T02
#[test]
fn a_refused_open_strikes_only_the_file_open() {
    use media_host::OpenError;

    let a_path = root().join("crates/player/tests/media/zlib/plain.mkv");
    let b_path = root().join("crates/player/tests/media/zlib/plain.mov");
    let open = |path: &Path| std::fs::File::open(path).unwrap();
    let a = FileId::of(&open(&a_path)).unwrap();
    let b = FileId::of(&open(&b_path)).unwrap();
    let mut strikes = Strikes::default();
    let stage = Stage::new();
    // An open under a player's id while that player is open is refused by
    // the host, and the player keeps its file: a crash strikes it, and not
    // the file refused.
    let mut worker = stage.launch(&recipe()).unwrap();
    worker.open(1, &open(&a_path), &strikes).unwrap();
    let probe = worker.recv(REPLY).unwrap();
    assert!(
        matches!(probe, Reply::Probed { player: 1, .. }),
        "{probe:?}"
    );
    assert!(matches!(
        worker.open(1, &open(&b_path), &strikes),
        Err(OpenError::Occupied)
    ));
    assert_eq!(crash_and_reap(&mut worker, &mut strikes), [1]);
    assert_eq!((strikes.of(a), strikes.of(b)), (1, 0));
    // An open the worker refuses before its probe, past the eight players
    // it holds, frees its id once the game takes the refusal: a crash
    // strikes the file the eight have open, once, and not the one refused.
    let mut worker = stage.launch(&recipe()).unwrap();
    for player in 1..=8 {
        worker.open(player, &open(&a_path), &strikes).unwrap();
    }
    worker.open(9, &open(&b_path), &strikes).unwrap();
    let mut probed = std::collections::BTreeSet::new();
    let mut refused = false;
    while probed.len() < 8 || !refused {
        match worker.recv(REPLY).unwrap() {
            Reply::Probed { player, .. } => {
                probed.insert(player);
            }
            Reply::Rejected { player: 9, .. } => refused = true,
            reply => panic!("{reply:?}"),
        }
    }
    assert_eq!(
        crash_and_reap(&mut worker, &mut strikes),
        [1, 2, 3, 4, 5, 6, 7, 8]
    );
    assert_eq!((strikes.of(a), strikes.of(b)), (2, 0));
}

// Covers: T02
#[test]
fn a_second_close_keeps_the_player_closing() {
    use media_host::players::State;

    // A second close of a player that is closing is rejected; its
    // rejection answers that close, not the open, so the player stays
    // closing with its file open in the worker until the first close's
    // answer.
    let fixture = root().join("crates/player/tests/media/zlib/plain.mkv");
    let file = std::fs::File::open(&fixture).unwrap();
    let a = FileId::of(&file).unwrap();
    let strikes = Strikes::default();
    let stage = Stage::new();
    let mut worker = stage.launch(&recipe()).unwrap();
    let open = worker.open(1, &file, &strikes).unwrap();
    let first = worker.close(1).unwrap();
    let second = worker.close(1).unwrap();
    let mut rejected = false;
    loop {
        match worker.recv(REPLY).unwrap() {
            Reply::Rejected { id, player: 1, .. } if id == second => {
                rejected = true;
                assert_eq!(
                    worker.player_state(1),
                    Some(State::Closing {
                        open,
                        close: first,
                        file: Some(a)
                    })
                );
            }
            Reply::Probed { id, player: 1, .. } if id == open => {}
            Reply::Closed { id, player: 1 } if id == first => break,
            reply => panic!("{reply:?}"),
        }
    }
    assert!(
        rejected,
        "the second close's rejection came after the first's answer"
    );
    assert_eq!(worker.player_state(1), None);
}

// Covers: T02
#[test]
fn a_refused_file_is_not_struck_and_its_id_stays_taken() {
    use media_host::OpenError;
    use media_host::players::State;

    // A file refused at its probe is closed in the worker: workers that
    // crash afterwards give it no strike, and fail its player, whose id
    // stays taken until it is closed.
    let dir = tempfile::tempdir().unwrap();
    let text = dir.path().join("not-a-video.txt");
    std::fs::write(&text, "not a video\n".repeat(64)).unwrap();
    let id = FileId::of(&std::fs::File::open(&text).unwrap()).unwrap();
    let other = root().join("crates/player/tests/media/zlib/plain.mkv");
    let mut strikes = Strikes::default();
    let stage = Stage::new();
    for _ in 0..2 {
        let mut worker = stage.launch(&recipe()).unwrap();
        let file = std::fs::File::open(&text).unwrap();
        let open = worker.open(1, &file, &strikes).unwrap();
        let reply = worker.recv(REPLY).unwrap();
        assert!(
            matches!(reply, Reply::Refused { id, player: 1, .. } if id == open),
            "{reply:?}"
        );
        assert_eq!(worker.player_state(1), Some(State::Refused { open }));
        let again = std::fs::File::open(&other).unwrap();
        assert!(matches!(
            worker.open(1, &again, &strikes),
            Err(OpenError::Occupied)
        ));
        assert_eq!(crash_and_reap(&mut worker, &mut strikes), [1]);
        assert_eq!(strikes.of(id), 0);
    }
}

// Covers: T02
#[test]
fn a_worker_ending_with_the_watchers_code_is_its_own_end() {
    use windows::Win32::System::SystemServices::JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT;
    use windows::Win32::System::Threading::TerminateProcess;

    // Every termination the watcher asks for fails, and the worker then
    // ends with the code the watcher would have ended it with: no
    // termination of the watcher's was accepted, so the end is recorded as
    // the worker's own.
    let stage = Stage::new();
    let never = watch::Variation {
        failed_terminations: u8::MAX,
        ..watch::Variation::default()
    };
    let worker = stage.launch(&recipe().watching_as(never)).unwrap();
    worker
        .post_job_message(JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT)
        .unwrap();
    std::thread::sleep(WATCHED);
    assert_eq!(worker.end(), None, "{:?}", worker.watch_events());
    // SAFETY: the worker's process handle, which has every access.
    unsafe { TerminateProcess(worker.process(), verdict::ENDED_BY_MESSAGE) }.unwrap();
    assert_eq!(ended(&worker), End::Exited(verdict::ENDED_BY_MESSAGE));
}
