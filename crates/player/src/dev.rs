//! Development features, compiled only with the `dev-tools` feature.
//!
//! `play <chart> --dev-scripted-input <log>` plays the chart on the same
//! threads as the game, without a window or devices: the input log's
//! events drive the input side as a script on the play's timeline, fed by
//! the clocked input thread, the audio engine plays into a recording of a
//! simulated device paced by the clock, and the renderer draws offscreen
//! on WARP. `play --auto <chart> --dev-headless` runs autoplay the same
//! way. `--dev-frames <dir>` also writes a countdown frame, a play frame
//! and the end screen there as PNG files. `--dev-overlay` shows the
//! measurement overlay from the start.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context as _;
use audio::{AudioOutput, OutputError, RecordingOutput, SimConfig, SimDevice, Wake};
use bms_model::{TimeSource, TimeUs};
use game::Wakes;
use input::{DeviceKind, QpcClock, ScriptedInput};
use replay::DecodeLimits;

use crate::args::{Command, UsageError};
use crate::measure::Drawn;
use crate::render_side::{FrameSink, Screen};

/// Prints, to the console only, which window took the foreground when the
/// game's window lost focus: its process's executable name and its window
/// class, with the time on the game's clock (µs since start). A focus loss
/// aborts the play (design.md "Play Lifecycle"), so this tells what did it.
pub fn print_foreground(now: TimeUs) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId,
    };
    use windows::core::PWSTR;

    // SAFETY: plain queries of the foreground window and its process; the
    // buffers outlive the calls and the process handle is closed.
    let (class, process) = unsafe {
        let hwnd = GetForegroundWindow();
        let mut class = [0_u16; 256];
        let n = usize::try_from(GetClassNameW(hwnd, &mut class)).unwrap_or(0);
        let class = String::from_utf16_lossy(&class[..n]);
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&raw mut pid));
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .ok()
            .and_then(|h| {
                let mut buf = [0_u16; 1024];
                let mut len = buf.len() as u32;
                let ok = QueryFullProcessImageNameW(
                    h,
                    PROCESS_NAME_WIN32,
                    PWSTR(buf.as_mut_ptr()),
                    &raw mut len,
                )
                .is_ok();
                let _ = CloseHandle(h);
                ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
            });
        (class, process)
    };
    // The executable's name only: its path may hold the user's name.
    let name = process
        .as_deref()
        .and_then(|p| Path::new(p).file_name())
        .map_or_else(
            || "unknown".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
    println!(
        "dev: focus lost at {} us to {name} (window class {class})",
        now.0
    );
}

/// `--dev-tools-probe`: says the build has the development features.
pub fn probe() -> ExitCode {
    // CI searches release binaries for this exact string to prove the
    // feature is absent from production builds.
    println!("tapestry dev-tools enabled");
    ExitCode::SUCCESS
}

/// The arguments of `measure play <chart>`, after `measure`.
///
/// # Errors
///
/// The usage error for anything but `play` and one chart.
pub fn measure_args(p: &mut lexopt::Parser) -> Result<Command, UsageError> {
    const MEASURE_USAGE: &str = "usage: tapestry measure play <chart.bms>";
    match p.next()? {
        Some(lexopt::Arg::Value(v)) if v == "play" => {}
        Some(other) => return Err(other.unexpected().into()),
        None => return Err(UsageError::Usage(MEASURE_USAGE.to_owned())),
    }
    let chart = match p.next()? {
        Some(lexopt::Arg::Value(v)) => PathBuf::from(v),
        Some(other) => return Err(other.unexpected().into()),
        None => {
            return Err(UsageError::Usage(format!(
                "measure play needs a chart file\n{MEASURE_USAGE}"
            )));
        }
    };
    crate::args::end(p)?;
    Ok(Command::Measure { chart })
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
    /// Show the measurement overlay from the start, without its key.
    pub overlay: bool,
    /// Run autoplay headless: a recording of a simulated audio device and
    /// WARP offscreen, as a scripted run.
    pub headless: bool,
}

impl Options {
    /// The session's input with the development options applied: an input
    /// log in place of the devices.
    ///
    /// # Errors
    ///
    /// A usage error for options that cannot go together: an input log with
    /// autoplay, or `--dev-headless` without autoplay.
    pub fn play_input(&self, input: crate::PlayInput) -> Result<crate::PlayInput, UsageError> {
        let log = self.scripted_input.clone();
        match input {
            crate::PlayInput::Autoplay if log.is_some() => Err(UsageError::Usage(
                "--auto and --dev-scripted-input cannot be used together".to_owned(),
            )),
            crate::PlayInput::Autoplay => Ok(input),
            _ if self.headless => Err(UsageError::Usage("--dev-headless needs --auto".to_owned())),
            _ => Ok(log.map_or(input, crate::PlayInput::Script)),
        }
    }

    /// Takes the long option `name` (without its dashes) and its value as a
    /// development option of `play`.
    ///
    /// # Errors
    ///
    /// An option `play` does not take, or a missing or bad value.
    pub fn take(&mut self, name: &str, p: &mut lexopt::Parser) -> Result<(), UsageError> {
        match name {
            "dev-auto-retry" => self.auto_retry = true,
            "dev-overlay" => self.overlay = true,
            "dev-headless" => self.headless = true,
            "dev-adjust-at" => {
                self.adjust_at_ms = p.value()?.to_string_lossy().parse().map_err(|_| {
                    UsageError::Usage("--dev-adjust-at takes milliseconds".to_owned())
                })?;
            }
            "dev-adjust" => {
                self.adjust_in_play =
                    adjust_keys(&p.value()?.to_string_lossy()).map_err(UsageError::Usage)?;
            }
            "dev-adjust-end" => {
                self.adjust_on_end =
                    adjust_keys(&p.value()?.to_string_lossy()).map_err(UsageError::Usage)?;
            }
            "dev-scripted-input" => self.scripted_input = Some(PathBuf::from(p.value()?)),
            "dev-frames" => self.frames = Some(PathBuf::from(p.value()?)),
            _ => return Err(UsageError::Unknown(format!("--{name}"))),
        }
        Ok(())
    }
}

/// The scripted play's input: the log's events as a script, one device per
/// source.
///
/// # Errors
///
/// Why the log cannot be read.
pub fn script(log: &Path) -> anyhow::Result<ScriptedInput> {
    let limits = DecodeLimits::default();
    let bytes = read_bounded(log, limits.max_compressed)?;
    let rec = replay::decode(&bytes, &limits).with_context(|| log.display().to_string())?;
    let kinds: Vec<DeviceKind> = rec
        .devices
        .sources
        .iter()
        .map(|s| match s.kind {
            replay::DeviceKind::Keyboard => DeviceKind::Keyboard,
            replay::DeviceKind::Controller => DeviceKind::Controller,
        })
        .collect();
    // Each controller declares the axes its record binds, as the adapter
    // would: only declared axes are recorded.
    let script = ScriptedInput::from_log(&rec.start, &rec.events, &kinds);
    Ok(rec
        .devices
        .sources
        .iter()
        .enumerate()
        .fold(script, |script, (i, s)| {
            script.with_axes(input::DeviceId(i as u64), &input::declared_axes(&s.profile))
        }))
}

/// Reads `path` if it holds at most `limit` bytes, reading no more than one
/// byte past the limit: an input log is untrusted (design.md "Input Log
/// Format").
fn read_bounded(path: &Path, limit: usize) -> anyhow::Result<Vec<u8>> {
    let read = || read_limited(fs::File::open(path)?, limit);
    read().with_context(|| path.display().to_string())
}

/// Everything `reader` holds if it is at most `limit` bytes, reading at most
/// one byte past the limit.
fn read_limited(reader: impl std::io::Read, limit: usize) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= limit,
        "larger than {limit} bytes, the largest input log accepted"
    );
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

/// The model a scripted controller press comes from: made up, so a key
/// config scripted with controller controls names a controller and
/// `device.toml` accepts it.
const SCRIPTED_CONTROLLER: input::hid::HidProduct = input::hid::HidProduct {
    vendor_id: 0xFFFE,
    product_id: 0x0001,
    interface: None,
};

/// `keys --dev-scripted-keys <steps>`: presses offered to the capture slot
/// in place of a person, one every 300 ms, comma-separated: a keyboard
/// scan code in hex (`2a`), `b<n>` for a controller's control `n`, or
/// `close` to close the window. Controller presses come from
/// [`SCRIPTED_CONTROLLER`].
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
                    match kind {
                        DeviceKind::Keyboard => capture.offer(&packet, kind, false),
                        DeviceKind::Controller => {
                            capture.offer_controller(&packet, Some(SCRIPTED_CONTROLLER), false);
                        }
                    }
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

    fn queue_periods(&self) -> Option<u32> {
        self.inner.queue_periods()
    }

    fn set_queue_periods(&mut self, periods: u32) -> Option<u32> {
        self.inner.set_queue_periods(periods)
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
    let onsets = onsets(samples);
    let listed: Vec<String> = onsets.iter().take(64).map(ToString::to_string).collect();
    println!("dev: audio onsets {}: {}", onsets.len(), listed.join(" "));
}

/// The frames where sound starts after at least 10 ms (at 48 kHz) of
/// silence on the left channel: when sounds started, to compare with the
/// chart's times.
fn onsets(samples: &[f32]) -> Vec<usize> {
    const QUIET: usize = 480;
    let mut found = Vec::new();
    let mut silent = QUIET;
    for (frame, s) in samples.iter().step_by(audio::CHANNELS).enumerate() {
        if s.abs() > 1e-3 {
            if silent >= QUIET {
                found.push(frame);
            }
            silent = 0;
        } else {
            silent += 1;
        }
    }
    found
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
    fn frame(&mut self, screen: &dyn Screen, drawn: &Drawn<'_>) {
        if !drawn.timing.presented {
            return;
        }
        match drawn.play {
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
        let e = format!("{:#}", read_limited(Counted(&taken), 10).unwrap_err());
        assert!(e.contains("larger than 10 bytes"), "{e}");
        assert_eq!(taken.get(), 11);
        assert_eq!(read_limited(&[7_u8; 11][..], 11).unwrap().len(), 11);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.tprl");
        std::fs::write(&path, [7_u8; 11]).unwrap();
        let e = format!("{:#}", read_bounded(&path, 10).unwrap_err());
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
