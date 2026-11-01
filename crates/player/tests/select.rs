//! Song selection on the game's real threads (dev-tools): `tapestry --auto
//! --dev-headless` with a scripted select screen, a library of charts made
//! here, a recording audio output and WARP offscreen.

#![cfg(feature = "dev-tools")]

use std::fs;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use game::{DataRoot, DeviceSettings, LibrarySettings, save_settings};

/// The rule a slow load holds by, shared with it.
#[path = "../src/dev/checkpoints.rs"]
mod checkpoints;

/// A 50 ms mono 16-bit square wave of `amplitude` at 44.1 kHz.
fn write_wav(path: &Path, amplitude: i16) {
    let samples: Vec<i16> = (0..2_205)
        .map(|i| if i % 50 < 25 { amplitude } else { -amplitude })
        .collect();
    let data = u32::try_from(samples.len() * 2).unwrap();
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16_u32.to_le_bytes());
    b.extend_from_slice(&[1, 0, 1, 0]);
    b.extend_from_slice(&44_100_u32.to_le_bytes());
    b.extend_from_slice(&88_200_u32.to_le_bytes());
    b.extend_from_slice(&[2, 0, 16, 0]);
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    fs::write(path, b).unwrap();
}

/// A chart `name` in folder `folder` of `library`: 120 BPM, four notes on
/// lane 1 half a second apart from 2 s on, each sounding its folder's own
/// `key.wav`, a square wave of `amplitude`.
fn write_chart(library: &Path, folder: &str, name: &str, title: &str, amplitude: i16) -> PathBuf {
    write_notes(library, folder, name, title, amplitude, 4)
}

/// [`write_chart`] with `notes` notes spread evenly over the measure.
fn write_notes(
    library: &Path,
    folder: &str,
    name: &str,
    title: &str,
    amplitude: i16,
    notes: usize,
) -> PathBuf {
    let dir = library.join(folder);
    fs::create_dir_all(&dir).unwrap();
    write_wav(&dir.join("key.wav"), amplitude);
    let text = format!(
        "#PLAYER 1\n#TITLE {title}\n#ARTIST テスト\n#PLAYLEVEL 3\n#BPM 120\n\
         #WAV01 key.wav\n#00111:{}\n",
        "01".repeat(notes)
    );
    let path = dir.join(name);
    fs::write(&path, text).unwrap();
    path
}

/// A temporary data root whose `device.toml` scans `library`.
struct Fixture {
    _dir: tempfile::TempDir,
    appdata: PathBuf,
    library: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let appdata = dir.path().join("appdata");
    let library = dir.path().join("library");
    fs::create_dir_all(&library).unwrap();
    let root = DataRoot::new(appdata.join("tapestry"));
    fs::create_dir_all(root.path()).unwrap();
    let device = DeviceSettings {
        library: LibrarySettings {
            roots: vec![library.display().to_string()],
        },
        ..DeviceSettings::default()
    };
    save_settings(&root.device_settings(), &device).unwrap();
    Fixture {
        _dir: dir,
        appdata,
        library,
    }
}

/// `tapestry --auto --dev-headless --dev-select <steps>` and `extra`, with
/// a render bound that only catches a render thread that never answers:
/// WARP frames on a loaded machine can take tens of seconds, past the
/// production bounds.
fn select(fx: &Fixture, steps: &str, extra: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tapestry"));
    command
        .args(["--auto", "--dev-headless", "--dev-select", steps])
        .args(["--dev-render-timeout", "600"])
        .args(extra)
        .env("LOCALAPPDATA", &fx.appdata);
    command
}

fn run(fx: &Fixture, steps: &str, extra: &[&str]) -> (Output, String) {
    let out = select(fx, steps, extra).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    (out, stdout)
}

/// The lines of `stdout` that start with `prefix`.
fn lines<'a>(stdout: &'a str, prefix: &str) -> Vec<&'a str> {
    stdout.lines().filter(|l| l.starts_with(prefix)).collect()
}

/// The peaks the recording output found after each sound's onset.
fn onset_peaks(stdout: &str) -> Vec<f32> {
    let line = lines(stdout, "dev: audio onset peaks:")[0];
    line.trim_start_matches("dev: audio onset peaks:")
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect()
}

/// Selection lists the library, a chosen chart plays by autoplay to its
/// result, and back on the result returns to selection, which lists the
/// library again; the select screen was drawn (its frame dumped), and the
/// window's close ends the run with 0.
#[test]
fn choosing_a_chart_plays_it_and_returns() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 8_000);
    let frames = fx.appdata.join("frames");
    let frames_arg = frames.display().to_string();
    let (_, stdout) = run(
        &fx,
        "pick=a.bms,enter,drawn,back,wait=a.bms,close",
        &["--dev-frames", &frames_arg],
    );
    let at = |line: &str| {
        stdout
            .lines()
            .position(|l| l.starts_with(line))
            .unwrap_or_else(|| {
                panic!(
                    "no {line}:
{stdout}"
                )
            })
    };
    let back = stdout
        .lines()
        .collect::<Vec<_>>()
        .iter()
        .rposition(|l| *l == "select: 1 charts listed")
        .unwrap();
    assert!(at("select: chose a.bms") < at("play started"), "{stdout}");
    assert!(
        at("play started") < at("play 1: Completed; perfect 4 "),
        "{stdout}"
    );
    assert!(at("play 1: ") < back, "{stdout}");
    assert!(frames.join("select.png").is_file());
    assert!(frames.join("end.png").is_file());
    // Autoplay saves nothing.
    assert!(!fx.appdata.join("tapestry").join("records").exists());
}

/// On the real threads, a result switches presets with the arrow keys
/// (scripted): the play under the default beatoraja-style shows as played,
/// then lr2-style and tapestry as what-ifs, wrapping past the last, and
/// lr2-style again from its cached summary; back returns to selection.
#[test]
fn result_switches_presets_on_the_real_threads() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 8_000);
    let (_, stdout) = run(
        &fx,
        "pick=a.bms,enter,next,next,prev,back,wait=a.bms,close",
        &[],
    );
    let viewing = lines(&stdout, "result: viewing ");
    let expected = [
        "result: viewing beatoraja-style (as played)",
        "result: viewing lr2-style (what-if, ",
        "result: viewing tapestry (what-if, ",
        "result: viewing lr2-style (what-if, ",
    ];
    assert_eq!(viewing.len(), expected.len(), "{stdout}");
    for (line, start) in viewing.iter().zip(expected) {
        assert!(line.starts_with(start), "{line}: {stdout}");
    }
    let evaluated = lines(&stdout, "result: evaluated ");
    assert_eq!(evaluated.len(), 2, "{stdout}");
    assert!(evaluated[0].starts_with("result: evaluated lr2-style in "));
    assert!(evaluated[1].starts_with("result: evaluated tapestry in "));
    assert_eq!(lines(&stdout, "play started").len(), 1, "{stdout}");
    // Back returns to selection. The first entry may list the chart only
    // once the scan has read it, so only the listing after the result
    // counts.
    let all: Vec<&str> = stdout.lines().collect();
    let last_view = all.iter().rposition(|l| l.starts_with("result: viewing "));
    let back = all.iter().rposition(|l| *l == "select: 1 charts listed");
    assert!(
        matches!((last_view, back), (Some(v), Some(b)) if v < b),
        "{stdout}"
    );
}

/// Two charts on the same threads: the second plays its own sound, never
/// the first's, though both name it `#WAV01`, and its own input, four
/// times longer than the first's, every note of it on time; each chart's
/// load time is printed; and every countdown, a retry's too, starts only after the
/// scan's pause was acknowledged (or no scan ran).
#[test]
fn a_second_chart_plays_only_its_own_sounds() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 24_000);
    write_notes(&fx.library, "b", "b.bms", "譜面B", 1_000, 16);
    let (_, stdout) = run(
        &fx,
        "pick=a.bms,enter,retry,back,pick=b.bms,enter,back,close",
        &[],
    );
    assert_eq!(lines(&stdout, "play started").len(), 3, "{stdout}");
    assert_eq!(
        lines(&stdout, "load: the chart is ready ").len(),
        2,
        "{stdout}"
    );
    let plays = lines(&stdout, "play 1: ");
    assert!(plays[0].starts_with("play 1: Completed; perfect 4 great 0 good 0 bad 0 poor 0"));
    assert!(
        plays[1].starts_with("play 1: Completed; perfect 16 great 0 good 0 bad 0 poor 0"),
        "{stdout}"
    );
    // Two plays of chart a, four notes each, one of b's sixteen.
    let peaks = onset_peaks(&stdout);
    assert_eq!(peaks.len(), 24, "{stdout}");
    let (a, b) = peaks.split_at(8);
    assert!(a.iter().all(|&p| p > 0.2), "{peaks:?}");
    // Each at its own measured gain: b's quiet sound, at the +6 dB cap,
    // stays far below a's.
    assert!(b.iter().all(|&p| p < 0.1), "{peaks:?}");
    // Each countdown follows a pause line since the one before.
    let mut paused = false;
    for line in stdout.lines() {
        if line.starts_with("library: the scan paused before the countdown")
            || line.starts_with("library: no scan before the countdown")
        {
            paused = true;
        } else if line == "play started" {
            assert!(paused, "a countdown without a pause:\n{stdout}");
            paused = false;
        }
    }
}

/// The gain in dB a `loudness: <name> measured in` line gives, as a factor.
fn measured_gain(line: &str) -> f64 {
    let db: f64 = line
        .split("; gain ")
        .nth(1)
        .and_then(|r| r.split(" dB").next())
        .unwrap()
        .parse()
        .unwrap();
    10_f64.powf(db / 20.0)
}

/// A chart whose loudness the library misses is measured during its load,
/// before its first countdown: its first play and its retry sound at the
/// measured gain, not the 0.5 headroom. Its next load finds the
/// measurement stored at that countdown and measures nothing. Judgment is
/// the same at either gain.
#[test]
fn the_first_play_has_the_measured_gain() {
    let fx = fixture();
    // The same square wave, b's four times louder.
    write_chart(&fx.library, "a", "a.bms", "譜面A", 4_000);
    write_chart(&fx.library, "b", "b.bms", "譜面B", 16_000);
    let (_, stdout) = run(
        &fx,
        "pick=a.bms,enter,retry,back,pick=b.bms,enter,back,pick=a.bms,enter,back,close",
        &[],
    );
    assert_eq!(lines(&stdout, "play started").len(), 4, "{stdout}");
    // Each measured once, during its first load, before its countdown.
    for name in ["a.bms", "b.bms"] {
        let line = format!("loudness: measuring {name}");
        assert_eq!(lines(&stdout, &line).len(), 1, "{stdout}");
        let at = stdout.find(&line).unwrap();
        let next = stdout[at..].find("play started").map(|p| at + p);
        let load = stdout[at..]
            .find("load: the chart is ready ")
            .map(|p| at + p);
        assert!(load < next && load.is_some(), "{stdout}");
    }
    let gain = |name: &str| {
        let measured = lines(&stdout, &format!("loudness: {name} measured in "));
        assert_eq!(measured.len(), 1, "{stdout}");
        measured_gain(measured[0])
    };
    let (a_gain, b_gain) = (gain("a.bms"), gain("b.bms"));
    assert!(a_gain > 1.0 && a_gain <= 1.996, "{a_gain}");
    assert!(a_gain / b_gain > 2.0, "{a_gain} {b_gain}");
    // a's second load: the stored measurement.
    let before = lines(&stdout, "loudness: a.bms: gain ");
    assert_eq!(before.len(), 1, "{stdout}");
    assert!(before[0].ends_with("measured before"), "{stdout}");
    // Judgment is unchanged.
    let plays: Vec<_> = stdout
        .lines()
        .filter(|l| l.starts_with("play ") && l.contains(": Completed; "))
        .collect();
    assert_eq!(plays.len(), 4, "{stdout}");
    for play in plays {
        assert!(
            play.contains("Completed; perfect 4 great 0 good 0 bad 0 poor 0"),
            "{stdout}"
        );
    }
    // Four onsets a play. Every play of a peaks alike, its first too; the
    // wave's peak, resampling ripple included, scales with its amplitude
    // times its gain, so a's over b's is (1/4)(a's gain over b's), within
    // the printed precision: at the headroom on its first play, a would
    // have peaked a's gain over 0.5 lower.
    let peaks = onset_peaks(&stdout);
    assert_eq!(peaks.len(), 16, "{stdout}");
    let (a, rest) = peaks.split_at(8);
    let (b, again) = rest.split_at(4);
    assert!(a.iter().chain(again).all(|&p| p == a[0]), "{peaks:?}");
    assert!(b.iter().all(|&p| p == b[0]), "{peaks:?}");
    let ratio = f64::from(a[0]) / f64::from(b[0]);
    let expected = a_gain / b_gain / 4.0;
    assert!(
        (ratio / expected - 1.0).abs() < 0.02,
        "{peaks:?} {a_gain} {b_gain}"
    );
    // Below the clipper's knee.
    assert!(a[0] < 0.8 && b[0] < 0.8, "{peaks:?}");
}

/// Closing the window while a chart's loudness is measured cancels the
/// measurement and the load: nothing more is prepared (the chart is never
/// ready, no countdown starts), nothing is stored, and the run ends at
/// once with 0. The next run measures the chart again.
#[test]
fn closing_during_the_measurement_leaves_the_load() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 4_000);
    let (_, stdout) = run(&fx, "pick=a.bms,close-measuring,enter", &[]);
    assert_eq!(
        lines(&stdout, "loudness: measuring a.bms").len(),
        1,
        "{stdout}"
    );
    assert_eq!(
        lines(&stdout, "loudness: a.bms: the measurement was cancelled").len(),
        1,
        "{stdout}"
    );
    assert_eq!(lines(&stdout, "load: cancelled").len(), 1, "{stdout}");
    assert!(
        lines(&stdout, "load: the chart is ready").is_empty(),
        "{stdout}"
    );
    assert!(lines(&stdout, "play started").is_empty(), "{stdout}");
    let (_, again) = run(&fx, "pick=a.bms,enter,back,close", &[]);
    assert_eq!(
        lines(&again, "loudness: measuring a.bms").len(),
        1,
        "{again}"
    );
    assert_eq!(lines(&again, "play started").len(), 1, "{again}");
}

/// Where `line` first starts a line of `stdout`, from line `from` on.
fn find_from(stdout: &str, from: usize, line: &str) -> Option<usize> {
    stdout
        .lines()
        .enumerate()
        .skip(from)
        .find(|(_, l)| l.starts_with(line))
        .map(|(i, _)| i)
}

/// The frames a `dev: <screen> frames <n>:` line counts, each report.
fn frames_of(stdout: &str, screen: &str) -> Vec<u64> {
    lines(stdout, &format!("dev: {screen} frames "))
        .iter()
        .map(|l| {
            l.trim_start_matches(&format!("dev: {screen} frames "))
                .split(':')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect()
}

/// The pixels of a frame the dump wrote (`dev::png`'s stored deflate
/// blocks, unfiltered RGBA rows): its width, height and RGBA bytes.
fn dumped_rgba(path: &Path) -> (usize, usize, Vec<u8>) {
    let png = fs::read(path).unwrap();
    let be = |b: &[u8]| u32::from_be_bytes(b.try_into().unwrap()) as usize;
    let (mut at, mut size, mut z) = (8, (0, 0), Vec::new());
    while at < png.len() {
        let len = be(&png[at..at + 4]);
        let kind = &png[at + 4..at + 8];
        let data = &png[at + 8..at + 8 + len];
        match kind {
            b"IHDR" => size = (be(&data[..4]), be(&data[4..8])),
            b"IDAT" => z.extend_from_slice(data),
            _ => {}
        }
        at += 12 + len;
    }
    // Past the zlib header: stored blocks of a 5-byte header each.
    let (mut raw, mut i) = (Vec::new(), 2);
    loop {
        let last = z[i] & 1 == 1;
        let n = usize::from(u16::from_le_bytes([z[i + 1], z[i + 2]]));
        raw.extend_from_slice(&z[i + 5..i + 5 + n]);
        i += 5 + n;
        if last {
            break;
        }
    }
    let (w, h) = size;
    let rgba = raw
        .chunks(w * 4 + 1)
        .flat_map(|row| row[1..].to_vec())
        .collect();
    (w, h, rgba)
}

/// Whether a row of `rgba` holds a run of at least `len` red pixels: the
/// play screen's judgment line.
fn has_red_run((w, _, rgba): &(usize, usize, Vec<u8>), len: usize) -> bool {
    rgba.chunks(w * 4).any(|row| {
        let mut run = 0;
        row.chunks(4).any(|p| {
            run = if p[0] > 200 && p[1] < 110 && p[2] < 110 {
                run + 1
            } else {
                0
            };
            run >= len
        })
    })
}

/// Choosing a chart switches at once to the play screen in its loading
/// state, the lanes and judgment line drawn without notes, the chart's
/// lines saying each stage over it, drawn on (its frame dumped) while a
/// slow load holds, until the countdown.
#[test]
fn the_play_screen_loads_from_the_choice_through_a_slow_load() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 8_000);
    let frames = fx.appdata.join("frames");
    let frames_arg = frames.display().to_string();
    let (_, stdout) = run(
        &fx,
        "pick=a.bms,slow-load=1500,enter,back,close",
        &["--dev-frames", &frames_arg],
    );
    let at = |from: usize, line: &str| {
        find_from(&stdout, from, line).unwrap_or_else(|| panic!("no {line}:\n{stdout}"))
    };
    let chose = at(0, "select: chose a.bms");
    let shown = at(chose, "load: the play screen loads a.bms");
    let read = at(shown, "load: reading the chart");
    let held = at(read, "dev: the load holds 1500 ms");
    let ready = at(held, "load: the chart is ready ");
    at(ready, "play started");
    assert!(lines(&stdout, "load: cancelled").is_empty(), "{stdout}");
    // Frames keep being presented through the hold: its checkpoints show
    // them following one another, by the rule the hold itself waits for
    // (at most 20 s), so a slow machine passes and a screen that drew
    // once and stopped fails.
    let presented: Vec<u64> = lines(&stdout, "dev: presented loading frames: ")
        .iter()
        .map(|l| {
            l.trim_start_matches("dev: presented loading frames: ")
                .parse()
                .unwrap()
        })
        .collect();
    assert!(
        checkpoints::frames_follow(&presented),
        "{presented:?}\n{stdout}"
    );
    assert_eq!(frames_of(&stdout, "loading").len(), 1, "{stdout}");
    // The loading frame is the play screen's: its judgment line spans the
    // lanes, as in play, and the select screen has none.
    let loading = dumped_rgba(&frames.join("loading.png"));
    assert!(has_red_run(&loading, loading.0 / 5), "no judgment line");
    let select = dumped_rgba(&frames.join("select.png"));
    assert!(!has_red_run(&select, select.0 / 5));
}

/// The quit key during a load cancels it and returns to selection: the
/// chart is not readied and no countdown starts; Enter then plays it.
#[test]
fn the_quit_key_during_a_load_returns_to_selection() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 8_000);
    let (_, stdout) = run(
        &fx,
        "pick=a.bms,slow-load=2000,quit-loading,enter,pick=a.bms,enter,back,close",
        &[],
    );
    let at = |from: usize, line: &str| {
        find_from(&stdout, from, line).unwrap_or_else(|| panic!("no {line}:\n{stdout}"))
    };
    let shown = at(0, "load: the play screen loads a.bms");
    let cancelled = at(shown, "load: cancelled");
    let back = at(cancelled, "select: 1 charts listed");
    let again = at(back, "select: chose a.bms");
    let ready = at(again, "load: the chart is ready ");
    at(ready, "play started");
    assert_eq!(lines(&stdout, "load: cancelled").len(), 1, "{stdout}");
    assert_eq!(lines(&stdout, "load: the chart is ready ").len(), 1);
    assert_eq!(lines(&stdout, "play started").len(), 1, "{stdout}");
    // Its loading state was drawn.
    assert!(!frames_of(&stdout, "loading").is_empty(), "{stdout}");
}

/// A chart `name` in folder `folder` whose `files` notes each sound a file
/// of its own.
fn write_many_sounds(library: &Path, folder: &str, name: &str, files: usize) {
    let dir = library.join(folder);
    fs::create_dir_all(&dir).unwrap();
    let mut text = "#PLAYER 1\n#TITLE 多い\n#BPM 120\n".to_owned();
    let mut notes = String::new();
    for i in 1..=files {
        // Distinct contents, so none is stored as another's duplicate.
        let amplitude = i16::try_from(1_000 + i * 50).unwrap();
        write_wav(&dir.join(format!("s{i}.wav")), amplitude);
        text.push_str(&format!("#WAV{i:02} s{i}.wav\n"));
        notes.push_str(&format!("{i:02}"));
    }
    text.push_str(&format!("#00101:{notes}\n#00211:01\n"));
    fs::write(dir.join(name), text).unwrap();
}

/// The quit key during the sound decode stops it at the next file: the
/// load returns to selection without decoding the rest or measuring the
/// loudness, and nothing is kept, so the next choice decodes and measures
/// the chart whole and plays it.
#[test]
fn the_quit_key_stops_the_sound_decode() {
    let fx = fixture();
    write_many_sounds(&fx.library, "m", "m.bms", 64);
    let (_, stdout) = run(
        &fx,
        "pick=m.bms,quit-decoding,enter,pick=m.bms,enter,back,close",
        &[],
    );
    let at = |from: usize, line: &str| {
        find_from(&stdout, from, line).unwrap_or_else(|| panic!("no {line}:\n{stdout}"))
    };
    let first = at(0, "load: the play screen loads m.bms");
    let stopped = at(first, "load: cancelled while loading the sounds");
    let back = at(stopped, "select: 1 charts listed");
    assert!(
        find_from(&stdout, first, "loudness: measuring m.bms").is_some_and(|m| m > back),
        "measured before the cancel:\n{stdout}"
    );
    let again = at(back, "load: the play screen loads m.bms");
    let ready = at(again, "load: the chart is ready ");
    at(ready, "play started");
    assert_eq!(
        stdout.lines().filter(|l| *l == "load: cancelled").count(),
        1
    );
    assert_eq!(lines(&stdout, "loudness: measuring m.bms").len(), 1);
}

/// A chart of one sound whose loudness is stored: the quit key as its only
/// file finishes decoding, after the decode's last check, still cancels
/// the load there. The bank is not installed and the stored measurement
/// not even looked up; selection follows, and the next choice plays.
#[test]
fn a_quit_after_the_last_file_is_decoded_cancels_the_load() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 4_000);
    // The first run measures the chart and stores it at its countdown.
    let (_, first) = run(&fx, "pick=a.bms,enter,back,close", &[]);
    assert_eq!(lines(&first, "loudness: a.bms measured in ").len(), 1);
    let (_, stdout) = run(
        &fx,
        "pick=a.bms,quit-decoding,enter,pick=a.bms,enter,back,close",
        &[],
    );
    let at = |from: usize, line: &str| {
        find_from(&stdout, from, line).unwrap_or_else(|| panic!("no {line}:\n{stdout}"))
    };
    let shown = at(0, "load: the play screen loads a.bms");
    let stopped = at(shown, "load: cancelled while loading the sounds");
    let back = at(stopped, "select: 1 charts listed");
    // Nothing of the cancelled load past its sounds.
    for line in ["loudness: a.bms: gain ", "load: the chart is ready "] {
        assert!(
            find_from(&stdout, shown, line).is_some_and(|l| l > back),
            "{line} before the cancel:\n{stdout}"
        );
    }
    let ready = at(back, "load: the chart is ready ");
    at(ready, "play started");
    assert_eq!(lines(&stdout, "play started").len(), 1, "{stdout}");
    assert!(lines(&stdout, "loudness: measuring").is_empty(), "{stdout}");
}

/// How many lines of `stdout` are exactly `line`.
fn count(stdout: &str, line: &str) -> usize {
    stdout.lines().filter(|l| *l == line).count()
}

/// The quit key during a slow loudness measurement stops it at the next
/// block and returns to selection at once, before the measurement could
/// have ended; the chart is then measured in the background and its
/// result kept, so the next choice measures nothing and plays it, and
/// stored at that countdown, so the next run measures nothing either.
#[test]
fn the_quit_key_leaves_the_measurement_to_the_background() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 4_000);
    let (_, stdout) = run(
        &fx,
        "slow-measuring=1500,pick=a.bms,quit-measuring,enter,background=idle,\
         pick=a.bms,enter,back,close",
        &[],
    );
    let at = |from: usize, line: &str| {
        find_from(&stdout, from, line).unwrap_or_else(|| panic!("no {line}:\n{stdout}"))
    };
    let measuring = at(0, "loudness: measuring a.bms");
    let stopped = at(measuring, "loudness: a.bms: the measurement was cancelled");
    let left = at(stopped, "loudness: a.bms is left to the background");
    let cancelled = at(left, "load: cancelled");
    let back = at(cancelled, "select: 1 charts listed");
    // Measured after selection was back: the quit did not wait for it.
    let measured = at(back, "loudness: a.bms measured in the background in ");
    let again = at(measured, "select: chose a.bms");
    let before = at(again, "loudness: a.bms: gain ");
    assert!(
        stdout
            .lines()
            .nth(before)
            .unwrap()
            .ends_with("measured before")
    );
    at(before, "play started");
    assert_eq!(count(&stdout, "loudness: measuring a.bms"), 1, "{stdout}");
    assert_eq!(
        count(&stdout, "loudness: measuring a.bms in the background"),
        1
    );
    assert_eq!(lines(&stdout, "loudness: a.bms measured in ").len(), 1);
    assert_eq!(lines(&stdout, "play started").len(), 1, "{stdout}");
    // Stored: the next run measures nothing.
    let (_, next) = run(&fx, "pick=a.bms,enter,back,close", &[]);
    assert!(lines(&next, "loudness: measuring").is_empty(), "{next}");
    assert_eq!(lines(&next, "play started").len(), 1, "{next}");
}

/// Choosing the chart again while its background measurement runs waits
/// for that measurement, saying it measures the loudness, and does not
/// measure the chart a second time.
#[test]
fn choosing_the_chart_again_waits_for_its_background_measurement() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 4_000);
    let (_, stdout) = run(
        &fx,
        "slow-measuring=3000,pick=a.bms,quit-measuring,enter,background=running,\
         pick=a.bms,enter,back,close",
        &[],
    );
    let at = |from: usize, line: &str| {
        find_from(&stdout, from, line).unwrap_or_else(|| panic!("no {line}:\n{stdout}"))
    };
    let cancelled = at(0, "load: cancelled");
    let again = at(cancelled, "select: chose a.bms");
    let waiting = at(
        again,
        "loudness: waiting for a.bms's measurement in the background",
    );
    let measured = at(waiting, "loudness: a.bms measured in the background in ");
    let before = at(measured, "loudness: a.bms: gain ");
    assert!(
        stdout
            .lines()
            .nth(before)
            .unwrap()
            .ends_with("measured before")
    );
    at(before, "play started");
    // The loading state said so before the sounds.
    let stage = at(again, "load: measuring the loudness");
    assert!(stage < measured, "{stdout}");
    assert_eq!(count(&stdout, "loudness: measuring a.bms"), 1, "{stdout}");
    assert_eq!(
        count(&stdout, "loudness: measuring a.bms in the background"),
        1
    );
    assert_eq!(lines(&stdout, "loudness: a.bms measured in ").len(), 1);
    assert_eq!(lines(&stdout, "play started").len(), 1, "{stdout}");
}

/// Closing the window while the background measures stops it: the run
/// ends without waiting for it, nothing of it is stored, and the next run
/// measures the chart in its load.
#[test]
fn closing_while_measuring_in_the_background_stores_nothing() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 4_000);
    let out = select(
        &fx,
        "slow-measuring=5000,pick=a.bms,quit-measuring,enter,background=running,close",
        &[],
    )
    .output()
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(
        count(&stdout, "loudness: measuring a.bms in the background"),
        1,
        "{stdout}"
    );
    assert!(
        lines(&stdout, "loudness: a.bms measured in ").is_empty(),
        "{stdout}"
    );
    assert!(!stderr.contains("did not stop"), "{stderr}");
    let (_, next) = run(&fx, "pick=a.bms,enter,back,close", &[]);
    assert_eq!(count(&next, "loudness: measuring a.bms"), 1, "{next}");
    assert_eq!(lines(&next, "play started").len(), 1, "{next}");
}

/// Closing the window during a load cancels it and ends the run at once
/// with 0, before any countdown.
#[test]
fn closing_during_a_load_ends_the_run() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 8_000);
    let (_, stdout) = run(&fx, "pick=a.bms,slow-load=2000,close-loading,enter", &[]);
    assert!(
        find_from(&stdout, 0, "load: the play screen loads a.bms").is_some(),
        "{stdout}"
    );
    assert_eq!(lines(&stdout, "load: cancelled").len(), 1, "{stdout}");
    assert!(
        lines(&stdout, "load: the chart is ready").is_empty(),
        "{stdout}"
    );
    assert!(lines(&stdout, "play started").is_empty(), "{stdout}");
}

/// A chart that cannot be loaded returns to selection with its reason
/// shown, and stays selected: Enter chooses it again. The threads keep
/// running: a valid chart plays after it.
#[test]
fn refused_chart_shows_its_reason() {
    let fx = fixture();
    let bad = fx.library.join("bad");
    fs::create_dir_all(&bad).unwrap();
    fs::write(
        bad.join("bad.bms"),
        "#TITLE 壊れた譜面\n#LNTYPE 2\n#00111:01\n",
    )
    .unwrap();
    write_chart(&fx.library, "good", "good.bms", "譜面", 8_000);
    let (_, stdout) = run(
        &fx,
        "pick=bad.bms,enter,sleep=500,enter,pick=good.bms,enter,back,close",
        &[],
    );
    let refused = lines(&stdout, "select: refused bad.bms: ");
    assert_eq!(refused.len(), 2, "{stdout}");
    assert!(
        refused[0].ends_with("the chart cannot be loaded: line 2: unsupported #LNTYPE 2"),
        "{stdout}"
    );
    // The select screen drew the reason.
    assert!(
        lines(&stdout, "dev: drawn notice: bad.bms: ")
            .iter()
            .any(|l| l.ends_with("unsupported #LNTYPE 2")),
        "{stdout}"
    );
    assert_eq!(lines(&stdout, "select: chose bad.bms").len(), 2, "{stdout}");
    assert_eq!(lines(&stdout, "play started").len(), 1, "{stdout}");
    // Each choice showed the loading state, which the refusal left.
    let mut from = 0;
    for _ in 0..2 {
        let shown = find_from(&stdout, from, "load: the play screen loads bad.bms");
        let shown = shown.unwrap_or_else(|| panic!("{stdout}"));
        from = find_from(&stdout, shown, "select: refused bad.bms: ").unwrap();
    }
}

/// After a refusal, selection releases the scan's pause: a chart added
/// once the refusal is shown is listed by the next pass, though no play
/// was completed.
#[test]
fn refused_load_releases_the_scan_pause() {
    let fx = fixture();
    let bad = fx.library.join("bad");
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join("bad.bms"), "#LNTYPE 2\n#00111:01\n").unwrap();
    let mut child = select(
        &fx,
        "pick=bad.bms,enter,wait=added.bms,close",
        &["--dev-rescan-ms", "50"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let mut stdout = String::new();
    let mut added = false;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        if line.starts_with("select: refused bad.bms") && !added {
            write_chart(&fx.library, "added", "added.bms", "追加", 8_000);
            added = true;
        }
        stdout.push_str(&line);
        stdout.push('\n');
    }
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(added, "{stdout}\n{stderr}");
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(lines(&stdout, "play started").is_empty(), "{stdout}");
}

/// The input thread drops its heartbeat on the select screen: at the first
/// entry, after a refused chart and after a completed play, it wakes only
/// for what it is asked.
#[test]
fn the_input_thread_idles_on_the_select_screen() {
    let fx = fixture();
    let bad = fx.library.join("bad");
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join("bad.bms"), "#LNTYPE 2\n#00111:01\n").unwrap();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 8_000);
    let (_, stdout) = run(
        &fx,
        "wait=a.bms,idle=1000,pick=bad.bms,enter,idle=1000,pick=a.bms,enter,back,idle=1000,close",
        &[],
    );
    let woke: Vec<u64> = lines(&stdout, "dev: the input thread woke ")
        .iter()
        .map(|l| {
            l.trim_start_matches("dev: the input thread woke ")
                .split(' ')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    assert_eq!(woke.len(), 3, "{stdout}");
    // A heartbeat would wake it about a thousand times a second.
    assert!(woke.iter().all(|&n| n < 50), "{woke:?}\n{stdout}");
}

/// A scripted pick of a chart further down than the window's queue holds
/// keys moves there in batches and chooses that very chart.
#[test]
fn a_pick_past_the_key_queue_chooses_its_chart() {
    let fx = fixture();
    for k in 0..70 {
        write_chart(
            &fx.library,
            "many",
            &format!("c{k:02}.bms"),
            &format!("t{k:02}"),
            8_000,
        );
    }
    let (_, stdout) = run(&fx, "pick=c69.bms,enter,back,close", &[]);
    assert_eq!(lines(&stdout, "select: chose ").len(), 1, "{stdout}");
    assert!(
        lines(&stdout, "select: chose c69.bms ").len() == 1,
        "{stdout}"
    );
}

/// F12 on the select screen chooses the next play preset, lr2-style after
/// the default beatoraja-style: `player.toml` holds it at once, while
/// selection still shows, and the chart chosen after it plays under it.
#[test]
fn choosing_a_preset_writes_player_toml() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 8_000);
    let player = DataRoot::new(fx.appdata.join("tapestry")).player_settings();
    let mut child = select(
        &fx,
        "next-preset,sleep=2000,pick=a.bms,enter,back,wait=a.bms,close",
        &[],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let mut stdout = String::new();
    let mut while_idle = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        // Read while the sleep step keeps selection idle: neither a play's
        // end nor the run's end could have written it.
        if line == "select: preset lr2-style" {
            while_idle = Some(fs::read_to_string(&player).unwrap_or_default());
        }
        stdout.push_str(&line);
        stdout.push('\n');
    }
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let while_idle = while_idle.unwrap_or_else(|| panic!("no step:\n{stdout}"));
    assert!(lines(&stdout, "play started").len() == 1, "{stdout}");
    assert!(
        while_idle.contains("preset = \"lr2-style\""),
        "{while_idle}\n{stdout}"
    );
    let saved = fs::read_to_string(&player).unwrap();
    assert!(saved.contains("preset = \"lr2-style\""), "{saved}");
    assert_eq!(
        lines(&stdout, "result: viewing ").first().copied(),
        Some("result: viewing lr2-style (as played)"),
        "{stdout}"
    );
}

/// A `player.toml` that cannot be replaced (read-only): the step still
/// chooses the preset, which the chart then plays, the failure is
/// reported, and the file keeps what it held.
#[test]
fn a_preset_that_cannot_be_written_still_plays() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 8_000);
    let player = DataRoot::new(fx.appdata.join("tapestry")).player_settings();
    save_settings(&player, &game::PlayerSettings::default()).unwrap();
    let held = fs::read_to_string(&player).unwrap();
    let mut read_only = fs::metadata(&player).unwrap().permissions();
    read_only.set_readonly(true);
    fs::set_permissions(&player, read_only.clone()).unwrap();
    let out = select(
        &fx,
        "next-preset,pick=a.bms,enter,back,wait=a.bms,close",
        &[],
    )
    .output()
    .unwrap();
    // Writable again, so the folder can go.
    #[expect(clippy::permissions_set_readonly_false, reason = "a test's own file")]
    read_only.set_readonly(false);
    fs::set_permissions(&player, read_only).unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(lines(&stdout, "select: preset ").len(), 1, "{stdout}");
    assert!(stderr.contains("player.toml"), "{stderr}");
    assert!(
        lines(&stdout, "settings: wrote player.toml").is_empty(),
        "{stdout}"
    );
    assert_eq!(fs::read_to_string(&player).unwrap(), held);
    assert_eq!(
        lines(&stdout, "result: viewing ").first().copied(),
        Some("result: viewing lr2-style (as played)"),
        "{stdout}"
    );
}

/// In a window, whose swap chain paces frames with one frame of latency,
/// the quit key (bound to Q) on a result returns to a select screen that is
/// drawn: each of three results left that way is followed by select
/// frames, reported as the screen is left for the next chart or the run
/// ends. Leaving a result while the render thread waits for an end frame
/// once lost the frame the wait granted, and nothing was drawn after.
#[test]
#[ignore = "opens a window: needs an interactive desktop"]
fn the_select_screen_is_drawn_after_a_results_quit_key() {
    let fx = fixture();
    write_chart(&fx.library, "a", "a.bms", "譜面A", 8_000);
    let mut device = DeviceSettings {
        library: LibrarySettings {
            roots: vec![fx.library.display().to_string()],
        },
        ..DeviceSettings::default()
    };
    // Q's scan code, as `tapestry keys` captures it.
    device.keyboard.quit = Some(0x10);
    let root = DataRoot::new(fx.appdata.join("tapestry"));
    save_settings(&root.device_settings(), &device).unwrap();
    let round = "pick=a.bms,enter,drawn,back,wait=a.bms,sleep=1000";
    let steps = format!("{round},{round},{round},close");
    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .args(["--auto", "--dev-select", &steps])
        .env("LOCALAPPDATA", &fx.appdata)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let all: Vec<&str> = stdout.lines().collect();
    let results: Vec<usize> = (0..all.len())
        .filter(|&i| all[i].starts_with("result: viewing "))
        .collect();
    assert_eq!(results.len(), 3, "{stdout}");
    for (n, &at) in results.iter().enumerate() {
        let next = results.get(n + 1).copied().unwrap_or(all.len());
        assert!(
            all[at..next]
                .iter()
                .any(|l| l.starts_with("dev: select frames ")),
            "no select frames after result {}:\n{stdout}",
            n + 1
        );
    }
}
