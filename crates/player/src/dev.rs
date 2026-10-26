//! Development features, compiled only with the `dev-tools` feature.
//!
//! `play <chart> --dev-scripted-input <log>` plays the chart on the same
//! threads as the game, without a window or devices: the input log's
//! events drive the input side as a script on the play's timeline, the
//! audio engine plays into a recording of a simulated device paced by the
//! clock, and the renderer draws offscreen on WARP. `--dev-frames <dir>`
//! also writes a countdown frame, a play frame and the end screen there as
//! PNG files.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use audio::{AudioOutput, OutputError, RecordingOutput, SimConfig, SimDevice, Wake};
use bms_model::{TimeSource, TimeUs};
use game::Wakes;
use input::{DeviceKind, QpcClock, ScriptedInput, SourceSet};
use replay::DecodeLimits;

use crate::input_side::Hook;
use crate::render_side::{FrameSink, Screen};

/// Handles a development-only argument, or returns `None` for any other one.
pub fn handle_arg(arg: &OsStr) -> Option<ExitCode> {
    if arg == "--dev-tools-probe" {
        // CI searches release binaries for this exact string to prove the
        // feature is absent from production builds.
        println!("tapestry dev-tools enabled");
        return Some(ExitCode::SUCCESS);
    }
    None
}

/// The development options of `play`.
#[derive(Debug, Default)]
pub struct Options {
    /// The input log to play.
    pub scripted_input: Option<PathBuf>,
    /// Where to write frames.
    pub frames: Option<PathBuf>,
    /// Retry once at the first end screen and quit at the second, to
    /// measure the retry time without a key press.
    pub auto_retry: bool,
    /// Settings keys pressed by themselves when the first play reaches
    /// `adjust_at_ms`.
    pub adjust_in_play: Vec<game::Adjust>,
    /// Settings keys pressed by themselves at the first end screen.
    pub adjust_on_end: Vec<game::Adjust>,
    /// The chart time, ms, at which `adjust_in_play` is pressed.
    pub adjust_at_ms: i64,
}

impl Options {
    /// Takes `arg` and its value if it is a development option of `play`.
    ///
    /// # Errors
    ///
    /// A message for an option without its value.
    pub fn take(
        &mut self,
        arg: &OsStr,
        rest: &mut impl Iterator<Item = OsString>,
    ) -> Result<bool, String> {
        if arg == "--dev-auto-retry" {
            self.auto_retry = true;
            return Ok(true);
        }
        if arg == "--dev-adjust-at" {
            let value = rest
                .next()
                .ok_or_else(|| "--dev-adjust-at needs a value".to_owned())?;
            self.adjust_at_ms = value
                .to_string_lossy()
                .parse()
                .map_err(|_| "--dev-adjust-at takes milliseconds".to_owned())?;
            return Ok(true);
        }
        let adjust = if arg == "--dev-adjust" {
            Some(&mut self.adjust_in_play)
        } else if arg == "--dev-adjust-end" {
            Some(&mut self.adjust_on_end)
        } else {
            None
        };
        if let Some(list) = adjust {
            let value = rest
                .next()
                .ok_or_else(|| format!("{} needs a value", arg.to_string_lossy()))?;
            *list = adjust_keys(&value.to_string_lossy())?;
            return Ok(true);
        }
        let slot = if arg == "--dev-scripted-input" {
            &mut self.scripted_input
        } else if arg == "--dev-frames" {
            &mut self.frames
        } else {
            return Ok(false);
        };
        let value = rest
            .next()
            .ok_or_else(|| format!("{} needs a value", arg.to_string_lossy()))?;
        *slot = Some(PathBuf::from(value));
        Ok(true)
    }
}

/// The scripted play's input: the log's events as a script, one device per
/// source.
///
/// # Errors
///
/// Why the log cannot be read.
pub fn script(log: &Path) -> Result<ScriptedInput, String> {
    let limits = DecodeLimits::default();
    let bytes = read_bounded(log, limits.max_compressed)?;
    let rec = replay::decode(&bytes, &limits).map_err(|e| format!("{}: {e}", log.display()))?;
    let kinds: Vec<DeviceKind> = rec
        .devices
        .sources
        .iter()
        .map(|s| match s.kind {
            replay::DeviceKind::Keyboard => DeviceKind::Keyboard,
            replay::DeviceKind::Controller => DeviceKind::Controller,
        })
        .collect();
    Ok(ScriptedInput::from_log(&rec.start, &rec.events, &kinds))
}

/// Reads `path` if it holds at most `limit` bytes, reading no more than one
/// byte past the limit: an input log is untrusted (D14).
fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let fail = |e: String| format!("{}: {e}", path.display());
    let file = fs::File::open(path).map_err(|e| fail(e.to_string()))?;
    read_limited(file, limit).map_err(fail)
}

/// Everything `reader` holds if it is at most `limit` bytes, reading at most
/// one byte past the limit.
fn read_limited(reader: impl std::io::Read, limit: usize) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err(format!(
            "larger than {limit} bytes, the largest input log accepted"
        ));
    }
    Ok(bytes)
}

/// Settings keys by name, comma-separated: `F1` to `F8`.
///
/// # Errors
///
/// A name that is not a settings key.
pub fn adjust_keys(list: &str) -> Result<Vec<game::Adjust>, String> {
    list.split(',')
        .map(|name| {
            name.strip_prefix('F')
                .and_then(|n| n.parse::<u32>().ok())
                .and_then(|n| game::Adjust::from_key(0x3A + n))
                .ok_or_else(|| format!("{name} is not a settings key (F1 to F8)"))
        })
        .collect()
}

/// One step of a scripted key config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyStep {
    /// A keyboard key pressed and released, by scan code.
    Key(u32),
    /// A controller's control pressed and released.
    Button(u32),
    /// The window closes.
    Close,
}

/// `keys --dev-scripted-keys <steps>`: presses offered to the capture slot
/// in place of a person, one every 300 ms, comma-separated: a keyboard
/// scan code in hex (`2a`), `b<n>` for a controller's control `n`, or
/// `close` to close the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyScript(Vec<KeyStep>);

impl KeyScript {
    /// Reads the steps.
    ///
    /// # Errors
    ///
    /// A step that is none of the three forms.
    pub fn parse(list: &str) -> Result<Self, String> {
        list.split(',')
            .map(|s| {
                if s == "close" {
                    Ok(KeyStep::Close)
                } else if let Some(n) = s.strip_prefix('b') {
                    n.parse().map(KeyStep::Button).map_err(|_| s.to_owned())
                } else {
                    u32::from_str_radix(s, 16)
                        .map(KeyStep::Key)
                        .map_err(|_| s.to_owned())
                }
            })
            .collect::<Result<_, _>>()
            .map(Self)
            .map_err(|s| format!("{s} is not a scan code, b<n> or close"))
    }

    /// Plays the steps on a thread of its own, as the input thread offers
    /// presses; dropping the handle stops it.
    pub fn spawn(self, capture: input::Capture, controls: Arc<crate::controls::Controls>) -> Stop {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            for step in self.0 {
                for _ in 0..30 {
                    if thread_stop.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let (kind, control) = match step {
                    KeyStep::Key(c) => (DeviceKind::Keyboard, c),
                    KeyStep::Button(c) => (DeviceKind::Controller, c),
                    KeyStep::Close => {
                        controls.close();
                        return;
                    }
                };
                for value in [1, 0] {
                    let packet = input::Packet {
                        device: input::DeviceId(1),
                        control: eval::ControlId(control),
                        value,
                        background: false,
                    };
                    capture.offer(&packet, kind, false);
                }
            }
        });
        Stop {
            stop,
            handle: Some(handle),
        }
    }
}

/// Stops a scripted thread when dropped.
pub struct Stop {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Stop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The scripted input thread: the input thread's boundary every
/// millisecond, the script fed by the hook. Stops when dropped.
pub struct ScriptThread {
    _stop: Stop,
}

impl ScriptThread {
    /// Starts it with `sources` and `hook` (in script mode).
    ///
    /// # Errors
    ///
    /// If the thread cannot start.
    pub fn spawn(clock: QpcClock, mut sources: SourceSet, mut hook: Hook) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("scripted input".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    hook.wake(&mut sources, clock.now());
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
            .map_err(|e| format!("the scripted input thread cannot start: {e}"))?;
        Ok(Self {
            _stop: Stop {
                stop,
                handle: Some(handle),
            },
        })
    }
}

/// A simulated device paced by the clock: each wait sleeps until the
/// device's next buffer event is due on the clock, so the engine runs in
/// real time as it does with WASAPI.
pub struct Paced<O> {
    inner: O,
    clock: QpcClock,
}

/// The recording output of a scripted run: 48 kHz, 10 ms periods, waking
/// first now, recording up to `seconds` of audio.
pub fn recording_output(clock: QpcClock, seconds: usize) -> Paced<RecordingOutput<SimDevice>> {
    let device = SimDevice::new(SimConfig::steady(48_000, 480, clock.now()));
    Paced {
        inner: RecordingOutput::new(device, 48_000 * seconds),
        clock,
    }
}

impl<O: AudioOutput + Wakes> AudioOutput for Paced<O> {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }

    fn period(&self) -> u32 {
        self.inner.period()
    }

    fn wait(&mut self) -> Wake {
        let left = self.inner.next_wake().0 - self.clock.now().0;
        if left > 0 {
            std::thread::sleep(Duration::from_micros(left.unsigned_abs()));
        }
        self.inner.wait()
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        self.inner.write(f)
    }
}

/// Prints what the recording output kept.
pub fn report_audio(out: &Paced<RecordingOutput<SimDevice>>) {
    let samples = out.inner.samples();
    let peak = samples.iter().fold(0.0_f32, |p, s| p.max(s.abs()));
    println!(
        "dev: audio recorded {} frames, peak {peak:.3}",
        samples.len() / audio::CHANNELS
    );
}

/// Writes frames as PNG files: `countdown.png` at the first frame of the
/// lead-in, `play.png` at the first frame at or after `play_at`, and
/// `end.png` at the first end screen frame.
pub struct FrameDump {
    dir: PathBuf,
    play_at: TimeUs,
    countdown: bool,
    play: bool,
    end: bool,
}

impl FrameDump {
    /// Frames into `dir`, the play frame at chart time `play_at`.
    pub const fn new(dir: PathBuf, play_at: TimeUs) -> Self {
        Self {
            dir,
            play_at,
            countdown: false,
            play: false,
            end: false,
        }
    }

    fn save(&self, screen: &dyn Screen, name: &str) {
        let path = self.dir.join(name);
        let Some((w, h, pixels)) = screen.read_back() else {
            eprintln!(
                "dev: {}: the screen has nothing to read back",
                path.display()
            );
            return;
        };
        let written =
            fs::create_dir_all(&self.dir).and_then(|()| fs::write(&path, png(w, h, &pixels)));
        match written {
            Ok(()) => println!("dev: frame {}", path.display()),
            Err(e) => eprintln!("dev: {}: {e}", path.display()),
        }
    }
}

impl FrameSink for FrameDump {
    fn frame(&mut self, screen: &dyn Screen, play: Option<TimeUs>) {
        match play {
            Some(t) if t.0 < 0 && !self.countdown => {
                self.countdown = true;
                self.save(screen, "countdown.png");
            }
            Some(t) if t >= self.play_at && !self.play => {
                self.play = true;
                self.save(screen, "play.png");
            }
            None if !self.end => {
                self.end = true;
                self.save(screen, "end.png");
            }
            _ => {}
        }
    }
}

/// An RGBA8 image as a PNG file, stored without compression (deflate's
/// stored blocks): enough for a few development screenshots without an
/// image library.
fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let row = width as usize * 4;
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for line in rgba.chunks(row) {
        raw.push(0); // no filter
        raw.extend_from_slice(line);
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(0xFFFF).peekable();
    while let Some(b) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        // `chunks(0xFFFF)` keeps every block within u16.
        let len = b.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", &ihdr[..]), (b"IDAT", &z[..]), (b"IEND", &[][..])] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1_u32, 0_u32);
    for &x in data {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    b << 16 | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0_u32;
    for &x in data {
        c ^= u32::from(x);
        for _ in 0..8 {
            c = if c & 1 == 1 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::{adler32, crc32, png, read_bounded, read_limited};

    /// A 1 MB source that counts the bytes taken from it.
    struct Counted<'a>(&'a std::cell::Cell<u64>);

    impl std::io::Read for Counted<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let left = 1_000_000 - self.0.get();
            let n = buf.len().min(usize::try_from(left).unwrap());
            buf[..n].fill(7);
            self.0.set(self.0.get() + n as u64);
            Ok(n)
        }
    }

    #[test]
    fn a_log_over_the_limit_is_refused_after_reading_one_byte_more() {
        let taken = std::cell::Cell::new(0);
        let e = read_limited(Counted(&taken), 10).unwrap_err();
        assert!(e.contains("larger than 10 bytes"), "{e}");
        assert_eq!(taken.get(), 11);
        assert_eq!(read_limited(&[7_u8; 11][..], 11).unwrap().len(), 11);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.tprl");
        std::fs::write(&path, [7_u8; 11]).unwrap();
        let e = read_bounded(&path, 10).unwrap_err();
        assert!(e.contains("big.tprl: larger than 10 bytes"), "{e}");
    }

    #[test]
    fn checksums_match_their_reference_values() {
        // The published check values: CRC-32 of "123456789" is CBF43926,
        // Adler-32 of "Wikipedia" is 11E60398.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn png_splits_rows_into_stored_blocks() {
        // 20 000 px × 1 row: 80 001 raw bytes, more than one stored block.
        let out = png(20_000, 1, &vec![7_u8; 80_000]);
        assert_eq!(&out[..8], b"\x89PNG\r\n\x1a\n");
        // IDAT follows the signature and the 25-byte IHDR chunk.
        let idat = &out[33..];
        let len = u32::from_be_bytes(idat[..4].try_into().unwrap()) as usize;
        assert_eq!(&idat[4..8], b"IDAT");
        let z = &idat[8..8 + len];
        // The zlib header, a non-final block of 65 535 bytes, a final one
        // of 80 001 − 65 535 = 14 466, then the Adler-32.
        assert_eq!(&z[..7], [0x78, 0x01, 0x00, 0xFF, 0xFF, 0x00, 0x00]);
        let second = 2 + 5 + 65_535;
        assert_eq!(z[second], 1);
        assert_eq!(z[second + 1..second + 3], 14_466_u16.to_le_bytes());
        assert_eq!(z.len(), second + 5 + 14_466 + 4);
    }
}
