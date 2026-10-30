//! Song selection on the game's real threads (dev-tools): `tapestry --auto
//! --dev-headless` with a scripted select screen, a library of charts made
//! here, a recording audio output and WARP offscreen.

#![cfg(feature = "dev-tools")]

use std::fs;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use game::{DataRoot, DeviceSettings, LibrarySettings, save_settings};

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

/// `tapestry --auto --dev-headless --dev-select <steps>` and `extra`.
fn select(fx: &Fixture, steps: &str, extra: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tapestry"));
    command
        .args(["--auto", "--dev-headless", "--dev-select", steps])
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
        "pick=a.bms,enter,back,wait=a.bms,close",
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
    assert!(b.iter().all(|&p| p < 0.05), "{peaks:?}");
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
