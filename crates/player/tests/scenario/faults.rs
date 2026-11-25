//! The media worker's fault scenarios (design.md "Threat Model" T02): each
//! plays its chart twice, the first play launching the worker behind the
//! game's launch gate, and faults the second. A fault armed in the worker
//! (`--dev-serve fire …`) fires only once the test has read back a known
//! frame and acknowledged it (`TAPESTRY_DEV_FAULT_ACK`), at the fault's own
//! seam, which stamps it on the performance counter; a fault of the whole
//! worker is made by the test itself, by terminating or suspending it. The
//! confined open's scenarios (T08) follow: a chart video named outside its
//! folder, and one whose folder is swapped for a link at the open barrier.

use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME,
    PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

use super::*;

#[link(name = "ntdll")]
unsafe extern "system" {
    /// Suspends every thread of `process` (ntdll; the debuggers' and the
    /// process tools' suspend).
    fn NtSuspendProcess(process: HANDLE) -> i32;
}

/// `media:lost`'s codes (the session's `Loss`, in its order).
const LOSS_LAUNCH: i64 = 1;
const LOSS_HEARTBEAT: i64 = 5;
const LOSS_ANSWER: i64 = 6;
const LOSS_PROGRESS: i64 = 7;
const LOSS_ENDED: i64 = 8;
const LOSS_FAILED: i64 = 9;
/// Added to a chart video's code.
const LOSS_CHART: i64 = 256;

/// The game's progress and heartbeat deadlines (the session's
/// `Deadlines::GAME`), µs.
const PROGRESS_US: i64 = 2_000_000;
const HEARTBEAT_US: i64 = 2_000_000;

/// How long the game takes at most to end a worker it has given up on,
/// once it saw the loss: its job's termination, confirmed.
const REAP: Duration = Duration::from_secs(1);

/// This process's performance counter now, one counter across the
/// machine's processes.
fn qpc_now() -> i64 {
    let mut at = 0;
    // SAFETY: writes the local it is given.
    unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&raw mut at) }.unwrap();
    at
}

/// The scenario of a fault run: the chart played twice as
/// [`warmed_scenario`] plays it, the first play captured at 0 (`warm`,
/// which opens the launch gate), the second at each of `ats` and by the
/// steps `more`.
fn fault_text(ats: &[i64], more: &[&str], select: &str, play: &str) -> String {
    warmed_scenario(
        "out",
        &["capture = { at = 0, file = \"warm.png\" }"],
        ats,
        more,
        select,
        play,
    )
}

/// What a fault run's test does while the game runs.
struct Ctl<'a> {
    game: u32,
    dir: &'a Path,
    ack: &'a Path,
    acked: Option<i64>,
}

impl Ctl<'_> {
    /// The capture `name` once written.
    fn capture(&self, name: &str) -> Vec<[u8; 3]> {
        written(self.dir, name, RUN_LIMIT, self.game);
        rgb(&self.dir.join(format!("{name}.png")))
    }

    /// The capture `name` once written, checked to show the frame due of
    /// a 10 fps coded clip started at `start` µs of chart time.
    fn due(&self, name: &str, start: i64) -> Vec<[u8; 3]> {
        let px = self.capture(name);
        let due = due_frame(chart_us(self.dir, name) - start, 10).unwrap();
        let got = coded_index(&px, 7);
        assert!(
            got.is_some_and(|g| is_due(g, due)),
            "{name}: {got:?}, due {due:?}"
        );
        px
    }

    /// Acknowledges the positive capture: the game fires the worker's
    /// armed fault.
    fn fire(&mut self) {
        self.acked = Some(qpc_now());
        fs::write(self.ack, b"").unwrap();
    }

    /// The game's media worker.
    fn worker(&self) -> u32 {
        worker_of(self.game, Duration::from_secs(10))
    }
}

/// A fault run's output, its second play's marks from its choice.
struct FaultRun {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    dir: PathBuf,
    chose: i64,
    acked: Option<i64>,
}

impl FaultRun {
    /// The run ended as a scenario that completes does.
    fn ok(&self) {
        assert_eq!(self.code, Some(0), "{}\n{}", self.stdout, self.stderr);
    }

    /// The marks `tag` from the second play's choice on.
    fn marks(&self, tag: &str) -> Vec<(i64, i64)> {
        probe_marks(&self.stdout, tag)
            .into_iter()
            .filter(|m| m.0 >= self.chose)
            .collect()
    }

    /// The losses the session saw from the second play on, ordered.
    fn losses(&self) -> Vec<i64> {
        self.losses_by(i64::MAX)
    }

    /// The losses the session saw from the second play on until `until`
    /// (game clock), ordered: a worker given up on loses the videos it
    /// still holds when the play is left, after them.
    fn losses_by(&self, until: i64) -> Vec<i64> {
        let mut v: Vec<i64> = self
            .marks("media:lost")
            .iter()
            .filter(|m| m.0 <= until)
            .map(|m| m.1)
            .collect();
        v.sort_unstable();
        v
    }

    /// When the capture `name` was presented, on the game's clock.
    fn presented(&self, name: &str) -> i64 {
        side(&self.dir, name, "presented_us")
    }

    /// When the session first saw a loss, on the game's clock.
    fn seen(&self) -> i64 {
        self.marks("media:lost")
            .first()
            .unwrap_or_else(|| panic!("a loss: {}", self.stdout))
            .0
    }

    /// The one play frame that drew the panel lost: its time on the game's
    /// clock and its chart time.
    fn panel_lost(&self) -> (i64, i64) {
        let [m] = self.marks("movies:chart_lost")[..] else {
            panic!("one panel loss drawn: {}", self.stdout);
        };
        m
    }

    /// The first play frame that drew a skin movie lost: its time on the
    /// game's clock and its chart time.
    fn skin_lost(&self) -> (i64, i64) {
        *self
            .marks("movies:skin_lost")
            .first()
            .unwrap_or_else(|| panic!("a skin movie loss drawn: {}", self.stdout))
    }

    /// The first frame off play that drew a skin movie lost, on the game's
    /// clock.
    fn skin_lost_off_play(&self) -> i64 {
        self.marks("movies:skin_lost_screen")
            .first()
            .unwrap_or_else(|| panic!("a skin movie loss drawn: {}", self.stdout))
            .0
    }

    /// Checks the first play frame that drew the panel lost (the
    /// [`LOST`] capture): drawn at or after `from` on the game's clock (the
    /// session's sight of the loss, or the fault's stamp), captured at or
    /// after its chart time, and black over the BGA. Its chart time.
    fn check_lost_frame(&self, from: i64) -> i64 {
        let (drawn, lost_us) = self.panel_lost();
        assert!(from <= drawn, "drawn lost at {drawn}, before {from}");
        let t = self.chart_us("lost");
        assert!(t >= lost_us, "the lost capture at {t}, before {lost_us}");
        assert!(
            panel_black(&self.px("lost")),
            "the first frame drawn lost, at {t}"
        );
        lost_us
    }

    /// Checks the first play frame that drew a skin movie lost (the
    /// [`LOST_SKIN`] capture) as [`check_lost_frame`](Self::check_lost_frame)
    /// does the panel's: black over both of its destinations.
    fn check_skin_lost_frame(&self, from: i64) -> i64 {
        let (drawn, lost_us) = self.skin_lost();
        assert!(
            from <= drawn,
            "the skin movie drawn lost at {drawn}, before {from}"
        );
        let t = self.chart_us("lost_skin");
        assert!(
            t >= lost_us,
            "the lost_skin capture at {t}, before {lost_us}"
        );
        assert!(
            skin_black(&self.px("lost_skin")),
            "the first frame drawn with the skin movie lost, at {t}"
        );
        lost_us
    }

    /// The ring faults the render side found in the second play: each
    /// one's time on the game's clock and its kind ([`RING_FAULT_RUN`]'s
    /// numbering).
    fn ring_faults(&self) -> Vec<(i64, i64)> {
        self.marks("movies:ring_fault")
            .into_iter()
            .map(|(at, v)| (at, v & 0xff))
            .collect()
    }

    /// The fault's stamp at its seam, on the game's clock, checked to come
    /// after the test's acknowledgement (A1).
    fn fired(&self) -> i64 {
        let [(_, qpc)] = probe_marks(&self.stdout, "fault:fired")[..] else {
            panic!("one fault fired: {}", self.stdout);
        };
        let at = game_time_of(&self.stdout, qpc);
        let acked = game_time_of(&self.stdout, self.acked.expect("acknowledged"));
        assert!(
            at >= acked,
            "fired at {at}, before the acknowledgement at {acked}"
        );
        at
    }

    /// Checks the capture `name` shows the frame due of a 10 fps coded
    /// clip started at `start` µs of chart time.
    fn due(&self, name: &str, start: i64) {
        let due = due_frame(self.chart_us(name) - start, 10).unwrap();
        let got = coded_index(&self.px(name), 7);
        assert!(
            got.is_some_and(|g| is_due(g, due)),
            "{name}: {got:?}, due {due:?}
{}",
            self.stdout
        );
    }

    fn px(&self, name: &str) -> Vec<[u8; 3]> {
        rgb(&self.dir.join(format!("{name}.png")))
    }

    fn chart_us(&self, name: &str) -> i64 {
        chart_us(&self.dir, name)
    }
}

/// Runs `text` (a [`fault_text`]) for `fx` with `env`, its first play's
/// launch behind the game's launch gate, a file of the run's own, until the
/// `warm` capture is written; then `during`, given the game.
fn run_fault(
    fx: &Fixture,
    text: &str,
    env: &[(&str, &str)],
    during: impl FnOnce(&mut Ctl),
) -> FaultRun {
    let dir = fx.base.join("out");
    let gate = fx.base.join("launch.gate");
    fs::write(&gate, b"").unwrap();
    let ack = fx.base.join("fault.ack");
    let mut command = fx.command(text);
    command
        .env("TAPESTRY_LOOP_PROBE", "1")
        .env("TAPESTRY_DEV_LAUNCH_GATE", &gate)
        .env("TAPESTRY_DEV_FAULT_ACK", &ack);
    for (k, v) in env {
        command.env(k, v);
    }
    let mut acked = None;
    let (out, stdout, stderr) = fx.run_during(&mut command, RUN_LIMIT, |game| {
        written(&dir, "warm", RUN_LIMIT, game);
        fs::remove_file(&gate).unwrap();
        let mut ctl = Ctl {
            game,
            dir: &dir,
            ack: &ack,
            acked: None,
        };
        during(&mut ctl);
        acked = ctl.acked;
    });
    fs::write(fx.base.join("run.out.txt"), format!("{stdout}\n{stderr}")).unwrap();
    let chose = probe_marks(&stdout, "cur:chose")
        .get(1)
        .map_or(i64::MAX, |m| m.0);
    FaultRun {
        code: out.status.code(),
        stdout,
        stderr,
        dir,
        chose,
        acked,
    }
}

/// A play skin with two overlapping `bga` destinations, each picture
/// stretched whole (stretch 0): one over the canvas's left two thirds, one
/// over its right two thirds, so x < 640 is the first's alone and x ≥ 1280
/// the second's (at a 320 × 180 capture, x < 106 and x ≥ 214).
fn install_two_bga_skin(fx: &Fixture) {
    install_play_skin(
        fx,
        r#"{"type": 0, "w": 1920, "h": 1080,
            "bga": {"id": "g"},
            "destination": [
                {"id": "g", "stretch": 0, "dst": [{"x": 0, "y": 0, "w": 1280, "h": 1080}]},
                {"id": "g", "stretch": 0, "dst": [{"x": 640, "y": 0, "w": 1280, "h": 1080}]}]}"#,
    );
}

/// Installs `json` as the play skin `s.json`.
fn install_play_skin(fx: &Fixture, json: &str) {
    let skin = fx.base.join("skin");
    fs::create_dir_all(&skin).unwrap();
    fs::write(skin.join("s.json"), json).unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.play]\nroot = '{}'\nentry = 's.json'\n",
            skin.display()
        ),
    )
    .unwrap();
}

/// The base picture's colour in [`write_base_layer_chart`].
const BASE_BLUE: [u8; 3] = [0, 0, 200];

/// A chart with a solid blue base picture and a layer video over it, both
/// from 0 s, `w/w.bms`: the layer the 10 s coded clip `clip.avi` (MJPEG,
/// 320 × 240, 10 fps), or, when `refused`, a file of that name its probe
/// refuses (bytes no container has); notes as [`write_video_chart`]'s.
fn write_base_layer_chart(library: &Path, refused: bool) -> bms_model::Chart {
    let dir = library.join("w");
    fs::create_dir_all(&dir).unwrap();
    write_wav(&dir.join("key.wav"));
    if refused {
        fs::write(dir.join("clip.avi"), [0x5a; 4096]).unwrap();
    } else {
        gen_clip(&dir.join("clip.avi"), "mjpeg avi 320x240 10 100 1");
    }
    let [r, g, b] = BASE_BLUE;
    write_rect_png(
        &dir.join("base.png"),
        [320, 240],
        [0, 0, 320, 240],
        [r, g, b, 255],
    );
    let text = "#PLAYER 1\n#TITLE layered\n#BPM 120\n#WAV01 key.wav\n\
                #BMP01 base.png\n#BMP02 clip.avi\n\
                #00138:01\n#00004:01\n#00007:02\n#00111:01010101\n#00311:01010101\n";
    fs::write(dir.join("w.bms"), text).unwrap();
    bms_parser::load_bms(text.as_bytes(), &dir, &Default::default())
        .unwrap()
        .chart
}

/// The two witness pixels of [`install_two_bga_skin`]'s destinations: the
/// first's alone and the second's alone, at mid height.
fn witnesses(px: &[[u8; 3]]) -> [[u8; 3]; 2] {
    let row = 90 * VIDEO_SIZE[0];
    [px[row + 40], px[row + 280]]
}

/// Whether `p` is within `d` of `want` in every channel.
fn near(p: [u8; 3], want: [u8; 3], d: u8) -> bool {
    p.iter().zip(want).all(|(&a, b)| a.abs_diff(b) <= d)
}

/// Whether a pixel shows a picture: a channel above black's.
fn shows(p: [u8; 3]) -> bool {
    p.iter().any(|&c| c > 24)
}

/// The panel of a chart whose worker fails goes black over everything it
/// covers (design.md "Rendering and Skins", BGA; "Threat Model" T02): with
/// a solid blue base picture under a layer video, drawn by two overlapping
/// `bga` destinations, each with an area of its own, both areas read back
/// the video and then, once the worker is terminated after that read, black
/// from the first frame drawn after the session saw it, the base picture
/// too. Its control: with the layer's file refused at its probe, the layer
/// is absent and both areas show the base's blue.
// Covers: T02
#[test]
fn worker_failure_blacks_the_panel_over_a_coloured_base() {
    const KILL_AFTER: i64 = 12 * 237;
    let ats: Vec<i64> = (1..=33).map(|k| k * 237).collect();
    let select = "select = { path = \"library/w/w.bms\" }";
    let play = "play = { input = \"logs/w.tprl\", until = \"end\" }";
    let layered = |refused| {
        let fx = Fixture::new();
        let chart = write_base_layer_chart(&fx.library(), refused);
        install_two_bga_skin(&fx);
        write_log(
            &fx.base.join("logs").join("w.tprl"),
            &hits(&chart, |_| true),
            1,
        );
        fx
    };
    let fx = layered(true);
    let _keep_control = KeepOnFailure::of(
        &fx,
        "worker_failure_blacks_the_panel_over_a_coloured_base-control",
    );
    let run = run_fault(&fx, &fault_text(&ats, &[], select, play), &[], |_| {});
    run.ok();
    for at in &ats {
        let w = witnesses(&run.px(&format!("c{at}")));
        assert!(
            w.iter().all(|&p| near(p, BASE_BLUE, 8)),
            "control c{at}: {w:?}, the base's blue"
        );
    }
    let fx = layered(false);
    let _keep = KeepOnFailure::of(&fx, "worker_failure_blacks_the_panel_over_a_coloured_base");
    let video = |p: [u8; 3]| !near(p, BASE_BLUE, 40) && shows(p);
    let mut crashed = 0;
    let more = ["capture = { chart_lost = true, file = \"lost.png\" }"];
    let run = run_fault(&fx, &fault_text(&ats, &more, select, play), &[], |c| {
        let w = witnesses(&c.capture(&format!("c{KILL_AFTER}")));
        assert!(
            w.iter().all(|&p| video(p)),
            "c{KILL_AFTER}: {w:?}, the video"
        );
        crashed = crash(c.worker());
    });
    run.ok();
    assert_eq!(run.losses(), [LOSS_ENDED + LOSS_CHART], "{}", run.stdout);
    let crashed = game_time_of(&run.stdout, crashed);
    assert!(
        (0..=CRASH_SEEN).contains(&(run.seen() - crashed)),
        "seen {}, crashed {crashed}",
        run.seen()
    );
    let (_, lost_us) = run.panel_lost();
    assert!(all_black(&run.px("lost")), "the first frame drawn lost");
    let mut after = 0;
    for at in &ats {
        let name = format!("c{at}");
        let (px, t) = (run.px(&name), run.chart_us(&name));
        if t >= lost_us {
            after += 1;
            assert!(all_black(&px), "{name} at {t}: black from {lost_us}");
        } else if *at <= KILL_AFTER {
            let w = witnesses(&px);
            assert!(w.iter().all(|&p| video(p)), "{name}: {w:?}, the video");
        }
    }
    assert!(after > 0, "a capture after the loss at {lost_us}");
    let rec = second_record(&fx);
    assert_eq!(rec.status.end, EndStatus::PlayedToEnd);
}

/// The skin of the fault scenarios: the chart's BGA over the whole canvas,
/// as [`install_video_skin`]'s, and a skin movie `movie` (made by
/// `gen_clip` from `spec`) drawn by two `image` objects over the canvas's
/// bottom corners (the skin's y grows upward: 60 from the bottom, canvas
/// rows 540 to 1020), at x 0 and 1280, 640 × 480 each: at a 320 × 180
/// capture, x < 106 and x ≥ 214 from y 90, below the coded row.
fn install_fault_skin(fx: &Fixture, movie: &str, spec: &str) {
    let skin = fx.base.join("skin");
    fs::create_dir_all(&skin).unwrap();
    gen_clip(&skin.join(movie), spec);
    install_play_skin(
        fx,
        &format!(
            r#"{{"type": 0, "w": 1920, "h": 1080,
            "source": [{{"id": "m", "path": "{movie}"}}],
            "image": [{{"id": "m1", "src": "m"}}, {{"id": "m2", "src": "m"}}],
            "bga": {{"id": "g"}},
            "destination": [
                {{"id": "g", "stretch": 0, "dst": [{{"x": 0, "y": 0, "w": 1920, "h": 1080}}]}},
                {{"id": "m1", "dst": [{{"x": 0, "y": 60, "w": 640, "h": 480}}]}},
                {{"id": "m2", "dst": [{{"x": 1280, "y": 60, "w": 640, "h": 480}}]}}]}}"#
        ),
    );
}

/// A chart of the fault scenarios, `f/f.bms`: the coded clip `clip` (made
/// by `gen_clip` from `spec`) on the base, started by the channel line
/// `start` (`#00004:01`: at 0 s), and the notes `notes` (channel lines).
fn write_fault_chart(
    library: &Path,
    clip: &str,
    spec: &str,
    start: &str,
    notes: &str,
) -> bms_model::Chart {
    let dir = library.join("f");
    fs::create_dir_all(&dir).unwrap();
    write_wav(&dir.join("key.wav"));
    gen_clip(&dir.join(clip), spec);
    let text = format!(
        "#PLAYER 1\n#TITLE fault\n#BPM 120\n#WAV01 key.wav\n#BMP01 {clip}\n\
         #BMP02 poor.png\n#00138:01\n{start}\n{notes}\n"
    );
    let [w, h] = [320, 240];
    write_rect_png(
        &dir.join("poor.png"),
        [w, h],
        [0, 0, w, h],
        [200, 0, 0, 255],
    );
    fs::write(dir.join("f.bms"), &text).unwrap();
    bms_parser::load_bms(text.as_bytes(), &dir, &Default::default())
        .unwrap()
        .chart
}

/// The 10 s coded clip of the fault scenarios (MJPEG, 320 × 240, 10 fps,
/// frame `i` showing `i`), and their skin movie (MPEG-1, 320 × 240, 30 fps,
/// looping).
const FAULT_CLIP: &str = "mjpeg avi 320x240 10 100 1";
const FAULT_MOVIE: &str = "mpeg1video mpeg 320x240 30 300 15";

/// The notes of the fault chart: 2.0 to 3.5 s and 6.0 to 7.5 s (120 BPM).
const FAULT_NOTES: &str = "#00111:01010101\n#00311:01010101";

/// The steps of a fault scenario's play of [`write_fault_chart`]'s chart.
const FAULT_SELECT: &str = "select = { path = \"library/f/f.bms\" }";
const FAULT_PLAY: &str = "play = { input = \"logs/f.tprl\", until = \"end\" }";

/// The capture of the first play frame that drew the chart's panel lost.
const LOST: &str = "capture = { chart_lost = true, file = \"lost.png\" }";
/// The capture of the first play frame that drew a skin movie lost.
const LOST_SKIN: &str = "capture = { skin_lost = true, file = \"lost_skin.png\" }";

/// The kind of ring fault the render side marks for a frame of a run not
/// started (`movies:ring_fault`, the run check's rejection).
const RING_FAULT_RUN: i64 = 5;

/// The fault scenarios' captures: every 237 ms, out of step with the
/// clip's 100 ms frames, from 0 s to 7.8 s.
fn fault_ats() -> Vec<i64> {
    (0..=33).map(|k| k * 237).collect()
}

/// A fixture with [`write_fault_chart`]'s chart (the fault clip, started by
/// `start`), [`install_fault_skin`]'s skin with the fault movie and an
/// input log hitting every note.
fn fault_fixture(start: &str) -> Fixture {
    let fx = Fixture::new();
    let chart = write_fault_chart(&fx.library(), "clip.avi", FAULT_CLIP, start, FAULT_NOTES);
    install_fault_skin(&fx, "m.mpg", FAULT_MOVIE);
    write_log(
        &fx.base.join("logs").join("f.tprl"),
        &hits(&chart, |_| true),
        1,
    );
    fx
}

/// The skin movie's witness pixels in a capture of [`install_fault_skin`]'s
/// skin: one inside each of its two destinations.
fn skin_witnesses(px: &[[u8; 3]]) -> [[u8; 3]; 2] {
    let row = 135 * VIDEO_SIZE[0];
    [px[row + 50], px[row + 266]]
}

/// Whether the BGA's own rows of a capture of [`install_fault_skin`]'s
/// skin (y < 90, above the skin movie's) read back black.
fn panel_black(px: &[[u8; 3]]) -> bool {
    all_black(&px[..90 * VIDEO_SIZE[0]])
}

/// Whether both destinations of [`install_fault_skin`]'s skin movie read
/// back black, every pixel of each: rows 90 to 169 at x < 106 and x ≥ 214
/// of a 320 × 180 capture.
fn skin_black(px: &[[u8; 3]]) -> bool {
    (90..170).all(|y| {
        let row = y * VIDEO_SIZE[0];
        (0..106)
            .chain(214..VIDEO_SIZE[0])
            .all(|x| px[row + x].iter().all(|&c| c <= 8))
    })
}

/// Checks the BGA of each capture of `ats` in `run`, its chart video
/// started at `start` µs of chart time and its panel lost at `lost` (none:
/// never): black from `lost`; before it and before `due_until`, the frame
/// due, or the placeholder (black) before the start. Returns how many
/// captures came after the loss.
fn check_panel(
    run: &FaultRun,
    ats: &[i64],
    start: i64,
    due_until: i64,
    lost: Option<i64>,
) -> usize {
    let mut after = 0;
    for at in ats {
        let name = format!("c{at}");
        let (px, t) = (run.px(&name), run.chart_us(&name));
        if lost.is_some_and(|l| t >= l) {
            assert!(panel_black(&px), "{name} at {t}: black from {lost:?}");
            after += 1;
            continue;
        }
        if t >= due_until {
            continue;
        }
        let Some(due) = due_frame(t - start, 10) else {
            assert!(
                panel_black(&px),
                "{name} at {t}: the placeholder before {start}"
            );
            continue;
        };
        let got = coded_index(&px, 7);
        assert!(
            got.is_some_and(|g| is_due(g, due)),
            "{name} at {t}: {got:?}, due {due:?}"
        );
    }
    after
}

/// Checks that the skin movie of [`install_fault_skin`]'s skin plays
/// through each capture of `ats` in `run`: both witnesses show a picture,
/// and they change from one capture to the next in at least three of four.
fn check_skin_movie_plays(run: &FaultRun, ats: &[i64]) {
    let mut before: Option<[[u8; 3]; 2]> = None;
    let mut changed = 0;
    for at in ats {
        let w = skin_witnesses(&run.px(&format!("c{at}")));
        assert!(w.iter().all(|&p| shows(p)), "c{at}: {w:?}, the skin movie");
        if before.is_some_and(|b| b.iter().zip(&w).any(|(&a, &c)| !near(a, c, 8))) {
            changed += 1;
        }
        before = Some(w);
    }
    assert!(
        4 * changed >= 3 * (ats.len() - 1),
        "the skin movie advanced in {changed} of {}",
        ats.len() - 1
    );
}

/// The record of `fx`'s second play.
fn second_record(fx: &Fixture) -> PlayRecord {
    let (records, _) = fx.played_records();
    let [_, record] = &records[..] else {
        panic!("two played records: {records:?}");
    };
    replay::decode(&fs::read(record).unwrap(), &DecodeLimits::default()).unwrap()
}

/// Whether `rec`, a record of [`fault_fixture`]'s chart in `fx`, is
/// complete and evaluable: a played attempt, no reason it is incomplete,
/// no `#RANDOM` result, and evaluated from the chart without an error.
fn check_complete(fx: &Fixture, rec: &PlayRecord) -> Result<(), String> {
    if !rec.played() {
        return Err("no played attempt".to_owned());
    }
    if rec.status.incomplete.any() {
        return Err(format!("incomplete: {:?}", rec.status.incomplete));
    }
    if !rec.chart.identity.random_results.is_empty() {
        return Err("#RANDOM results".to_owned());
    }
    let dir = fx.library().join("f");
    let text = fs::read(dir.join("f.bms")).unwrap();
    let chart = bms_parser::load_bms(&text, &dir, &Default::default())
        .unwrap()
        .chart;
    let input = eval::EvalInput {
        events: rec.events.clone(),
        device: eval::DeviceProfile {
            sources: rec
                .devices
                .sources
                .iter()
                .map(|s| s.profile.clone())
                .collect(),
        },
        start: rec.start.clone(),
        ..eval::EvalInput::default()
    };
    eval::evaluate(&chart, &input, &rules::beatoraja_style())
        .map(|_| ())
        .map_err(|e| format!("not evaluated: {e:?}"))
}

/// The record of `fx`'s second play, checked complete and evaluable
/// ([`check_complete`]), with the two values a play draws or reads itself
/// set apart: the chart's random seed, which selects nothing in a chart
/// without `#RANDOM` (its results are checked empty), and the play clock's
/// time when the record was finalized. Every other field is compared.
fn comparable_record(fx: &Fixture) -> PlayRecord {
    let mut rec = second_record(fx);
    if let Err(why) = check_complete(fx, &rec) {
        panic!("{why}: {rec:?}");
    }
    rec.chart.random_seed = 0;
    rec.status.finalized = TimeUs(0);
    rec
}

/// A healthy run of [`fault_fixture`]'s chart, its video from 0 s: the
/// frame due in every capture of `ats`, the skin movie playing, no loss;
/// its second play's [`comparable_record`]. A record of it whose input
/// failed is refused as incomplete.
fn healthy_fault_run(ats: &[i64]) -> PlayRecord {
    let fx = fault_fixture("#00004:01");
    let _keep = KeepOnFailure::of(&fx, "healthy_fault_run");
    let run = run_fault(
        &fx,
        &fault_text(ats, &[], FAULT_SELECT, FAULT_PLAY),
        &[],
        |_| {},
    );
    run.ok();
    assert!(run.losses().is_empty(), "{}", run.stdout);
    check_panel(&run, ats, 0, i64::MAX, None);
    check_skin_movie_plays(&run, ats);
    let rec = comparable_record(&fx);
    let mut failed = rec.clone();
    failed.status.incomplete.input_failed = true;
    assert!(
        check_complete(&fx, &failed).is_err(),
        "a record whose input failed is refused"
    );
    rec
}

/// Waits until `done`, bounded by the run's limit and failing as soon as
/// the game `game` has ended; `what` names it.
fn until(game: u32, what: &str, done: impl Fn() -> bool) {
    let begun = std::time::Instant::now();
    while !done() {
        assert!(!ended(game), "{what}: the game ended first");
        assert!(begun.elapsed() < RUN_LIMIT, "{what}: past the limit");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The screens' preparation deadline, as the game's (the media session's
/// `PREPARATION`).
const PREPARATION: Duration = Duration::from_secs(3);

/// A worker whose launch fails its self-check plays nothing (design.md
/// "Threat Model" T02): the second play's launch is held at the launch
/// gate past its preparation's deadline, which the preparation gate holds
/// for the launch's own outcome, so its two videos are lost for the launch
/// (`Launch`); the panel is drawn lost from chart time 0, the chart's BGA
/// and the skin movie's destinations read back black, and the play
/// completes with the record a healthy run of the same input makes, whose
/// captures show the chart video's frame due and the skin movie playing.
// Covers: T02
#[test]
fn failed_launch_blacks_the_panel_from_chart_time_zero() {
    let ats = fault_ats();
    let healthy = healthy_fault_run(&ats);
    let fx = fault_fixture("#00004:01");
    let _keep = KeepOnFailure::of(&fx, "failed_launch_blacks_the_panel_from_chart_time_zero");
    let text = fault_text(&ats, &[LOST, LOST_SKIN], FAULT_SELECT, FAULT_PLAY);
    let gate = fx.base.join("launch.gate");
    let reached = fx.base.join("launch.gate.reached");
    let hold = fx.base.join("preparation.gate");
    let run = run_fault(
        &fx,
        &text,
        &[
            ("TAPESTRY_DEV_WORKER_CHECK", "mismatch"),
            ("TAPESTRY_DEV_PREPARATION_GATE", hold.to_str().unwrap()),
        ],
        |c| {
            until(c.game, "the first launch past its gate", || {
                !reached.exists()
            });
            fs::write(&hold, b"").unwrap();
            fs::write(&gate, b"").unwrap();
            // The second play's launch starts once its preparation has
            // opened its videos, so its deadline passes within
            // `PREPARATION` of the launch reaching the gate. A first launch
            // still under way then fails them itself, and no second one
            // comes.
            let first = c.dir.join("c0.json");
            until(c.game, "the second launch at its gate", || {
                reached.exists() || first.exists()
            });
            if reached.exists() {
                std::thread::sleep(PREPARATION + Duration::from_millis(500));
            }
            fs::remove_file(&gate).unwrap();
        },
    );
    run.ok();
    assert_eq!(
        run.losses(),
        [LOSS_LAUNCH, LOSS_LAUNCH + LOSS_CHART],
        "{}",
        run.stdout
    );
    // Lost while the play was prepared, both are drawn lost from its first
    // play frame, captured black, whatever drew them lost before it.
    let seen = run.seen();
    let lost = run.check_lost_frame(seen);
    assert!(lost <= 0, "drawn lost from {lost}, after chart time 0");
    let skin = run.check_skin_lost_frame(seen);
    assert!(
        skin <= 0,
        "the skin movie drawn lost from {skin}, after chart time 0"
    );
    for at in &ats {
        assert!(
            all_black(&run.px(&format!("c{at}"))),
            "c{at}: black from chart time 0"
        );
    }
    assert_eq!(comparable_record(&fx), healthy);
}

/// A chart video lost while a miss shows the poor picture is drawn black
/// over it (design.md "Rendering and Skins", BGA: the lost panel draws one
/// opaque black quad, the poor frame included): every note from 2 s to
/// 3.75 s is missed, 250 ms apart, so the solid red poor picture holds the
/// panel from the first miss to at least 4.25 s; the worker is terminated
/// after a capture showed the poor picture, the session sees it inside that
/// window, and every frame from the one that first drew the panel lost
/// reads back black, inside the window and after it.
// Covers: T02
#[test]
fn worker_failure_during_an_active_poor_window_blacks_the_panel() {
    const KILL_AFTER: i64 = 11 * 237;
    let fx = Fixture::new();
    let _keep = KeepOnFailure::of(
        &fx,
        "worker_failure_during_an_active_poor_window_blacks_the_panel",
    );
    let notes = "#00006:02\n#00111:0101010101010101\n#00412:01";
    let chart = write_fault_chart(&fx.library(), "clip.avi", FAULT_CLIP, "#00004:01", notes);
    install_video_skin(&fx);
    let last = hits(&chart, |t| t.0 >= 7_000_000);
    write_log(&fx.base.join("logs").join("f.tprl"), &last, 1);
    let ats: Vec<i64> = (1..=33).map(|k| k * 237).collect();
    let more = ["capture = { chart_lost = true, file = \"lost.png\" }"];
    let red = |px: &[[u8; 3]]| near(px[90 * VIDEO_SIZE[0] + 160], [200, 0, 0], 40);
    let mut crashed = 0;
    let run = run_fault(
        &fx,
        &fault_text(&ats, &more, FAULT_SELECT, FAULT_PLAY),
        &[],
        |c| {
            let px = c.capture(&format!("c{KILL_AFTER}"));
            assert!(red(&px), "c{KILL_AFTER}: the poor picture");
            crashed = crash(c.worker());
        },
    );
    run.ok();
    assert_eq!(run.losses(), [LOSS_ENDED + LOSS_CHART], "{}", run.stdout);
    let crashed = game_time_of(&run.stdout, crashed);
    assert!(
        (0..=CRASH_SEEN).contains(&(run.seen() - crashed)),
        "seen {}",
        run.seen()
    );
    let (_, lost_us) = run.panel_lost();
    assert!(
        lost_us < 4_250_000,
        "lost at {lost_us}, after the poor window"
    );
    assert!(all_black(&run.px("lost")), "the first frame drawn lost");
    let (mut inside, mut past) = (0, 0);
    for at in &ats {
        let name = format!("c{at}");
        let (px, t) = (run.px(&name), run.chart_us(&name));
        if t < lost_us {
            assert!(
                shows(px[90 * VIDEO_SIZE[0] + 160]),
                "{name} at {t}: a picture before the loss"
            );
            continue;
        }
        assert!(all_black(&px), "{name} at {t}: black from {lost_us}");
        if t < 4_250_000 {
            inside += 1;
        } else {
            past += 1;
        }
    }
    assert!(
        inside > 0 && past > 0,
        "captures inside the window ({inside}) and after it ({past})"
    );
}

/// A skin movie that stops making progress, drawn by two `image` objects,
/// is lost at the progress deadline (design.md "Threat Model" T02): its
/// player's thread is frozen while it decodes, after the test read back the
/// chart video's frame due and the skin movie and acknowledged them, and
/// the worker's control loop goes on beating. The session loses that player
/// alone (`Progress`), within the deadline, its tick and an allowance of the
/// fault's stamp; both its destinations read back black from a capture on,
/// never one without the other; the worker lives, so the chart video, a
/// player of the same worker, keeps showing the frame due.
// Covers: T02
#[test]
fn skin_movie_failure_blacks_its_destinations() {
    let ats = fault_ats();
    let fx = fault_fixture("#00004:01");
    let _keep = KeepOnFailure::of(&fx, "skin_movie_failure_blacks_its_destinations");
    let args = [(
        "TAPESTRY_DEV_WORKER_ARGS",
        "--dev-serve fire freeze=mpeg1video:decoding",
    )];
    let run = run_fault(
        &fx,
        &fault_text(&ats, &[LOST_SKIN], FAULT_SELECT, FAULT_PLAY),
        &args,
        |c| {
            let px = c.due("c1185", 0);
            assert!(
                skin_witnesses(&px).iter().all(|&p| shows(p)),
                "c1185: the skin movie"
            );
            c.fire();
        },
    );
    run.ok();
    let end = run.presented(&format!("c{}", ats[ats.len() - 1]));
    assert_eq!(run.losses_by(end), [LOSS_PROGRESS], "{}", run.stdout);
    let waited = run.seen() - run.fired();
    assert!(
        (0..=PROGRESS_US + CRASH_SEEN).contains(&waited),
        "lost {waited} after the fire"
    );
    assert!(run.marks("movies:chart_lost").is_empty(), "{}", run.stdout);
    check_panel(&run, &ats, 0, i64::MAX, None);
    let lost = run.check_skin_lost_frame(run.seen());
    let mut after = 0;
    for at in &ats {
        let name = format!("c{at}");
        let (px, t) = (run.px(&name), run.chart_us(&name));
        if t >= lost {
            assert!(skin_black(&px), "{name} at {t}: black from {lost}");
            after += 1;
        } else {
            let w = skin_witnesses(&px);
            assert!(w.iter().all(|&p| shows(p)), "{name} at {t}: {w:?}");
        }
    }
    assert!(after > 0, "a capture after {lost}");
}

/// One chart player's allocation refused, after frames it showed, fails
/// that player alone (design.md "Threat Model" T02): the worker's own
/// allocator refuses its next decode an allocation, which the decode's
/// check finds as a decoder's refusal; the session sees the player failed
/// at once; the BGA reads back black from the first frame that drew it
/// lost, the frame due before; the worker lives on, so the skin movie keeps
/// playing; and the record is a healthy run's of the same input.
// Covers: T02
#[test]
fn decoder_alloc_refusal_blacks_only_its_panel() {
    let ats = fault_ats();
    let healthy = healthy_fault_run(&ats);
    let fx = fault_fixture("#00004:01");
    let _keep = KeepOnFailure::of(&fx, "decoder_alloc_refusal_blacks_only_its_panel");
    let args = [(
        "TAPESTRY_DEV_WORKER_ARGS",
        "--dev-serve fire refuse-alloc=mjpeg",
    )];
    let run = run_fault(
        &fx,
        &fault_text(&ats, &[LOST], FAULT_SELECT, FAULT_PLAY),
        &args,
        |c| {
            c.due("c1185", 0);
            c.fire();
        },
    );
    run.ok();
    assert_eq!(run.losses(), [LOSS_FAILED + LOSS_CHART], "{}", run.stdout);
    let waited = run.seen() - run.fired();
    assert!(
        (0..=CRASH_SEEN).contains(&waited),
        "lost {waited} after the fire"
    );
    let lost = run.check_lost_frame(run.seen());
    assert!(
        check_panel(&run, &ats, 0, i64::MAX, Some(lost)) > 0,
        "a capture after {lost}"
    );
    check_skin_movie_plays(&run, &ats);
    assert_eq!(comparable_record(&fx), healthy);
}

/// A frame written into a chart video's ring in a run not started, after
/// frames it showed, is caught by the run check alone (design.md "Threat
/// Model" T02): the BGA reads back black from the first frame that drew it
/// lost, never the stray frame, the frame due before, within the tick and
/// an allowance of the fault's stamp; the skin movie keeps playing; and the
/// record is a healthy run's of the same input.
// Covers: T02
#[test]
fn garbage_ring_fault_never_corrupts_play() {
    let ats = fault_ats();
    let healthy = healthy_fault_run(&ats);
    let fx = fault_fixture("#00004:01");
    let _keep = KeepOnFailure::of(&fx, "garbage_ring_fault_never_corrupts_play");
    let args = [("TAPESTRY_DEV_WORKER_ARGS", "--dev-serve fire garbage=mjpeg")];
    let run = run_fault(
        &fx,
        &fault_text(&ats, &[LOST], FAULT_SELECT, FAULT_PLAY),
        &args,
        |c| {
            c.due("c1185", 0);
            c.fire();
        },
    );
    run.ok();
    let fired = run.fired();
    let (drawn, _) = run.panel_lost();
    let waited = drawn - fired;
    assert!(
        (0..=CRASH_SEEN).contains(&waited),
        "drawn lost {waited} after the fire"
    );
    // The run check alone rejected it: one fault, of a run not started,
    // found from the fault's stamp to the frame that drew the panel lost.
    let faults = run.ring_faults();
    let [(found, kind)] = faults[..] else {
        panic!("one ring fault: {faults:?}");
    };
    assert_eq!(kind, RING_FAULT_RUN, "{faults:?}");
    assert!(
        (fired..=drawn).contains(&found),
        "found at {found}, fired at {fired}, drawn lost at {drawn}"
    );
    let lost = run.check_lost_frame(fired);
    assert!(
        check_panel(&run, &ats, 0, i64::MAX, Some(lost)) > 0,
        "a capture after {lost}"
    );
    check_skin_movie_plays(&run, &ats);
    assert_eq!(comparable_record(&fx), healthy);
}

/// A chart decoder that stops making progress, after frames it showed, is
/// lost at the progress deadline (design.md "Threat Model" T02): its
/// player's thread is frozen while it decodes, after the test acknowledged
/// the frame due, and the worker's control loop goes on beating; the
/// session loses that player alone (`Progress`), from 2 s after the fault's
/// stamp to 2 s, its tick and an allowance after it; its panel reads back
/// black from the first frame that drew it lost; the worker lives, so the
/// skin movie keeps playing. Its control: a video whose ring is full from
/// the preparation and held through more than the progress window, its
/// start at 4 s, is not lost and shows the frame due from its start.
// Covers: T02
#[test]
fn decoder_progress_stall_blacks_its_panel_only() {
    let ats = fault_ats();
    let fx = fault_fixture("#00204:01");
    let _keep_control =
        KeepOnFailure::of(&fx, "decoder_progress_stall_blacks_its_panel_only-control");
    let run = run_fault(
        &fx,
        &fault_text(&ats, &[], FAULT_SELECT, FAULT_PLAY),
        &[],
        |_| {},
    );
    run.ok();
    assert!(run.losses().is_empty(), "{}", run.stdout);
    check_panel(&run, &ats, 4_000_000, i64::MAX, None);
    check_skin_movie_plays(&run, &ats);
    let fx = fault_fixture("#00004:01");
    let _keep = KeepOnFailure::of(&fx, "decoder_progress_stall_blacks_its_panel_only");
    let args = [(
        "TAPESTRY_DEV_WORKER_ARGS",
        "--dev-serve fire freeze=mjpeg:decoding",
    )];
    let run = run_fault(
        &fx,
        &fault_text(&ats, &[LOST], FAULT_SELECT, FAULT_PLAY),
        &args,
        |c| {
            c.due("c1185", 0);
            c.fire();
        },
    );
    run.ok();
    let end = run.presented(&format!("c{}", ats[ats.len() - 1]));
    assert_eq!(
        run.losses_by(end),
        [LOSS_PROGRESS + LOSS_CHART],
        "{}",
        run.stdout
    );
    let waited = run.seen() - run.fired();
    assert!(
        (PROGRESS_US - 200_000..=PROGRESS_US + CRASH_SEEN).contains(&waited),
        "lost {waited} after the fire"
    );
    // From the freeze to the loss, the panel holds the last frame the
    // player published.
    let lost = run.check_lost_frame(run.seen());
    let positive = run.chart_us("c1185") + 1;
    assert!(
        check_panel(&run, &ats, 0, positive, Some(lost)) > 0,
        "a capture after {lost}"
    );
    check_skin_movie_plays(&run, &ats);
}

/// A worker that commits memory past its job's cap, after frames it
/// showed, is terminated by the game (design.md "Threat Model" T02): the
/// balloon starts at the fault's stamp, after the test acknowledged the
/// frame due; every video the worker played is lost (`Ended`), so the BGA
/// and the skin movie read back black from the first frame that drew the
/// panel lost; and the retry launches a new worker process whose chart
/// video shows the frame due again.
// Covers: T02
#[test]
fn job_cap_terminates_the_worker_and_blacks_its_panels() {
    let ats: Vec<i64> = (0..=25).map(|k| k * 237).collect();
    let again: Vec<i64> = (1..=8).map(|k| k * 237).collect();
    let fx = fault_fixture("#00004:01");
    let _keep = KeepOnFailure::of(&fx, "job_cap_terminates_the_worker_and_blacks_its_panels");
    let mut steps: Vec<String> = [
        "capture = { at = 0, file = \"warm.png\" }",
        FAULT_SELECT,
        FAULT_PLAY,
        "result = \"back\"",
    ]
    .map(str::to_owned)
    .to_vec();
    steps.extend(
        ats.iter()
            .map(|at| format!("capture = {{ at = {at}, file = \"c{at}.png\" }}")),
    );
    steps.extend([LOST, LOST_SKIN, FAULT_SELECT].map(str::to_owned));
    steps.extend(retried(6_200, "r", &again));
    steps.extend([FAULT_PLAY, "result = \"back\"", "quit = {}"].map(str::to_owned));
    let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
    let text = video_text(&steps);
    let args = [(
        "TAPESTRY_DEV_WORKER_ARGS",
        "--dev-serve fire balloon=32:1024",
    )];
    let mut workers = Vec::new();
    let run = run_fault(&fx, &text, &args, |c| {
        c.due("c1185", 0);
        workers.push(c.worker());
        c.fire();
        c.due(&format!("r{}", again[0]), 0);
        workers.push(c.worker());
    });
    run.ok();
    assert_ne!(
        workers[0], workers[1],
        "the retry's worker is a new process"
    );
    assert_eq!(
        run.losses(),
        [LOSS_ENDED, LOSS_ENDED + LOSS_CHART],
        "{}",
        run.stdout
    );
    let waited = run.seen() - run.fired();
    assert!(
        (0..=1_000_000 + CRASH_SEEN).contains(&waited),
        "lost {waited} after the fire"
    );
    let panel = run.check_lost_frame(run.seen());
    let skin = run.check_skin_lost_frame(run.seen());
    let mut after = 0;
    for at in &ats {
        let name = format!("c{at}");
        let (px, t) = (run.px(&name), run.chart_us(&name));
        if t >= panel {
            assert!(panel_black(&px), "{name} at {t}: black from {panel}");
        }
        if t >= skin {
            assert!(skin_black(&px), "{name} at {t}: black from {skin}");
        }
        if t >= panel.max(skin) {
            after += 1;
        }
    }
    assert!(after > 0, "a capture after {panel} and {skin}");
    check_panel(&run, &ats, 0, panel, None);
    for at in &again {
        let name = format!("r{at}");
        let due = due_frame(run.chart_us(&name), 10).unwrap();
        let got = coded_index(&run.px(&name), 7);
        assert!(
            got.is_some_and(|g| is_due(g, due)),
            "{name}: {got:?}, due {due:?}"
        );
    }
}

/// When the game ends while a chart video decodes, its worker ends with it
/// (design.md "Threat Model" T02): the game is terminated after a capture
/// showed the frame due, and the worker's process is gone within 5 s.
// Covers: T02
#[test]
fn game_exit_during_decode_reaps_the_worker() {
    let fx = fault_fixture("#00004:01");
    let mut worker = None;
    let run = run_fault(
        &fx,
        &fault_text(&[1_185], &[], FAULT_SELECT, FAULT_PLAY),
        &[],
        |c| {
            c.due("c1185", 0);
            let pid = c.worker();
            // SAFETY: a process id; the handle is closed below.
            let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.unwrap();
            crash(c.game);
            // SAFETY: the process just opened to wait on.
            let ended = unsafe { WaitForSingleObject(process, 5_000) };
            // SAFETY: the handle, unused after this.
            let _ = unsafe { CloseHandle(process) };
            worker = Some(ended);
        },
    );
    assert_ne!(run.code, Some(0), "{}\n{}", run.stdout, run.stderr);
    assert_eq!(
        worker,
        Some(WAIT_OBJECT_0),
        "the worker ended with the game"
    );
}

/// The scenario of `steps` at the video captures' size, written to `out`.
fn video_text(steps: &[&str]) -> String {
    scenario(steps).replacen(
        "output = \"out\"\n",
        "output = \"out\"\nsize = [320, 180]\n",
        1,
    )
}

/// The steps of a play of [`write_fault_chart`]'s chart left at `retry_ms`
/// (the quit key, onto its result) and retried from its result, the next
/// attempt captured at each of `ats` as `<prefix><ms>`.
fn retried(retry_ms: i64, prefix: &str, ats: &[i64]) -> Vec<String> {
    let mut steps = vec![format!(
        "play = {{ input = \"logs/f.tprl\", until = {{ at_ms = {retry_ms}, command = \"quit\" }} }}"
    )];
    steps.extend(
        ats.iter()
            .map(|at| format!("capture = {{ at = {at}, file = \"{prefix}{at}.png\" }}")),
    );
    steps.push("result = \"retry\"".to_owned());
    steps
}

/// A field of the capture `name`'s JSON.
fn side(dir: &Path, name: &str, key: &str) -> i64 {
    side_num(
        &fs::read_to_string(dir.join(format!("{name}.json"))).unwrap(),
        key,
    )
}

/// The number of handles the game process `pid` holds.
fn handles(pid: u32) -> u32 {
    // SAFETY: a process id; the handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.unwrap();
    let mut n = 0;
    // SAFETY: the process just opened and a local.
    unsafe { GetProcessHandleCount(process, &raw mut n) }.unwrap();
    // SAFETY: the handle, unused after this.
    let _ = unsafe { CloseHandle(process) };
    n
}

/// Whether process `pid` has ended (or is gone).
fn ended(pid: u32) -> bool {
    // SAFETY: a process id; the handle is closed below.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return true;
    };
    // SAFETY: the process just opened, polled.
    let done = unsafe { WaitForSingleObject(process, 0) } == WAIT_OBJECT_0;
    // SAFETY: the handle, unused after this.
    let _ = unsafe { CloseHandle(process) };
    done
}

/// The play's Closing in these scenarios' skins, µs: the quit key's
/// screen leaves 1 s after it (the log's `closing:` line).
const CLOSING_US: i64 = 1_000_000;

/// A decoder that hangs where it would answer its close, its worker's
/// control loop beating, is given up on at the close's deadline when the
/// play is left (design.md "Threat Model" T02): the chart player's thread
/// is frozen after the test acknowledged the frame due, and the play is
/// left at 1 s (the quit key), so its screen closes the player after its
/// Closing, before the progress deadline, and retried from its result; the
/// session loses that player for its missing answer (`Answer`) within the
/// Closing, the 1 s close deadline, its tick and an allowance of the
/// departure, and ends the worker: the
/// retry launches a new process, whose chart video shows the frame due, and
/// the game holds no more handles than it did before the fault.
// Covers: T02
#[test]
fn decoder_hang_with_healthy_heartbeat_then_departure_and_return() {
    let fx = fault_fixture("#00004:01");
    let _keep = KeepOnFailure::of(
        &fx,
        "decoder_hang_with_healthy_heartbeat_then_departure_and_return",
    );
    let mut steps = vec![
        "capture = { at = 0, file = \"warm.png\" }",
        FAULT_SELECT,
        FAULT_PLAY,
        "result = \"back\"",
        "capture = { at = 237, file = \"c237.png\" }",
        "capture = { at = 950, file = \"d950.png\" }",
        FAULT_SELECT,
    ];
    let again = [1_185, 2_133];
    // The result holds the retry for 2 s: the return comes after the
    // departure's close deadline, so the session has given up on the
    // worker before the next attempt's opens.
    let mut retry = retried(1_000, "r", &again);
    retry.insert(
        1,
        "capture = { result_at = 2000, file = \"held.png\" }".to_owned(),
    );
    steps.extend(retry.iter().map(String::as_str));
    steps.extend([FAULT_PLAY, "result = \"back\"", "quit = {}"]);
    let args = [(
        "TAPESTRY_DEV_WORKER_ARGS",
        "--dev-serve fire freeze=mjpeg:decoding",
    )];
    let (mut first, mut second, mut before, mut after) = (0, 0, 0, 0);
    let mut reaped = false;
    let run = run_fault(&fx, &video_text(&steps), &args, |c| {
        c.due("c237", 0);
        first = c.worker();
        before = handles(c.game);
        c.fire();
        c.capture("r1185");
        second = c.worker();
        reaped = ended(first);
        c.capture("r2133");
        after = handles(c.game);
    });
    run.ok();
    assert!(reaped, "the hung worker is reaped: {}", run.stdout);
    run.due("r1185", 0);
    run.due("r2133", 0);
    assert_ne!(first, second, "the retry's worker is a new process");
    assert_eq!(run.losses(), [LOSS_ANSWER + LOSS_CHART], "{}", run.stdout);
    let fired = run.fired();
    let left = side(&run.dir, "d950", "presented_us") + 50_000;
    assert!(
        fired < left,
        "fired at {fired}, after the departure at {left}"
    );
    let waited = run.seen() - left;
    assert!(
        waited <= CLOSING_US + 1_000_000 + CRASH_SEEN,
        "lost {waited} after the departure"
    );
    assert!(
        after <= before,
        "{after} handles after the return, {before} before the fault"
    );
}

/// A gap clip: frame 0 at 0 s, then frame 1 and the rest from 30.1 s (10
/// fps, `gap=1:300`), so once frame 0 is shown every frame its ring holds is
/// a future one for 30 s: the render side frees no slot and the session's
/// waiting excuse holds.
const GAP_CLIP: &str = "mjpeg matroska 320x240 10 20 1 gap=1:300";
/// The first stamp after the gap, µs.
const GAP_END: i64 = 30_100_000;

/// The checks of a starved worker's run: the session answered the waiting
/// check for each live player (`players` of them, chart videos `charts`),
/// before the suspension at `suspended` (game clock) and after the test's
/// request; every loss is the heartbeat's, of each player, its freed count
/// the one the check saw; enough of the gap remained at the suspension,
/// the movies having started no earlier than `start`.
fn check_starved(run: &FaultRun, suspended: i64, start: i64, players: usize, charts: usize) {
    let waiting = run.marks("media:waiting");
    let freed = run.marks("media:waiting_freed");
    assert_eq!(waiting.len(), players, "{}", run.stdout);
    assert!(
        waiting.iter().all(|m| m.0 < suspended),
        "{waiting:?} before {suspended}"
    );
    let charts_seen = waiting.iter().filter(|m| m.1 & 256 != 0).count();
    assert_eq!(charts_seen, charts, "{waiting:?}");
    let mut want: Vec<i64> = (0..players)
        .map(|i| LOSS_HEARTBEAT + if i < charts { LOSS_CHART } else { 0 })
        .collect();
    want.sort_unstable();
    assert_eq!(run.losses(), want, "{}", run.stdout);
    let lost = run.marks("media:lost");
    let lost_freed = run.marks("media:lost_freed");
    for (l, f) in lost.iter().zip(&lost_freed) {
        let chart = l.1 & 256 != 0;
        let (i, _) = waiting
            .iter()
            .enumerate()
            .find(|(_, w)| (w.1 & 256 != 0) == chart)
            .unwrap_or_else(|| panic!("a waiting mark for {l:?}"));
        assert_eq!(
            f.1, freed[i].1,
            "freed between the check and the loss: {l:?}"
        );
    }
    let seen = run.seen();
    let bound = HEARTBEAT_US + CRASH_SEEN;
    assert!(
        (0..=bound).contains(&(seen - suspended)),
        "lost {} after the suspension",
        seen - suspended
    );
    let reap = i64::try_from(REAP.as_micros()).unwrap();
    assert!(
        suspended - start + bound + reap < GAP_END,
        "the gap ends before the bound: suspended {suspended}, started {start}"
    );
}

/// Suspends the worker `pid` once the session answered the waiting check
/// asked at `ask` (bounded at 2 s, failing otherwise): the performance
/// counter just before the call. Waits then for the game to end the worker,
/// within the heartbeat bound, its tick, the allowance and the reap.
fn suspend_when_waiting(ask: &Path, pid: u32) -> i64 {
    // SAFETY: a process id; the handle is closed below.
    let process =
        unsafe { OpenProcess(PROCESS_SUSPEND_RESUME | PROCESS_SYNCHRONIZE, false, pid) }.unwrap();
    fs::write(ask, b"").unwrap();
    let mut ready = ask.as_os_str().to_owned();
    ready.push(".ready");
    let begun = std::time::Instant::now();
    while !Path::new(&ready).exists() {
        assert!(
            begun.elapsed() < Duration::from_secs(2),
            "no full waiting ring within 2 s"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let at = qpc_now();
    // SAFETY: the process opened with the suspend right.
    assert_eq!(unsafe { NtSuspendProcess(process) }, 0);
    let limit = Duration::from_micros(u64::try_from(HEARTBEAT_US + CRASH_SEEN).unwrap()) + REAP;
    let limit = u32::try_from(limit.as_millis()).unwrap();
    // SAFETY: the process opened to wait on.
    let done = unsafe { WaitForSingleObject(process, limit) };
    // SAFETY: the handle, unused after this.
    let _ = unsafe { CloseHandle(process) };
    assert_eq!(done, WAIT_OBJECT_0, "the starved worker ended");
    at
}

/// A worker suspended whole, after a presented movie frame, is lost at the
/// heartbeat deadline (design.md "Threat Model" T02), during play and on
/// the selection screen. Every live video is a gap clip, so once its first
/// frame is shown its ring stays full of future frames and the waiting
/// excuse holds: the test captures that frame, asks the session for a full
/// waiting ring seen after its request (a check on the session's thread),
/// and suspends the worker at the answer. Every video of the worker is lost
/// for the heartbeat alone, with no frame freed since the check, within the
/// deadline, its tick and an allowance, and the worker ends; the play's
/// panel and the skin movie read back black after it; leaving the play and
/// playing again launches a new worker whose video shows its frame.
// Covers: T02
#[test]
fn starved_worker_after_a_frame_blacks_its_panels() {
    // During play: the chart video and the play skin's movie.
    let fx = Fixture::new();
    let _keep = KeepOnFailure::of(&fx, "starved_worker_after_a_frame_blacks_its_panels");
    let chart = write_fault_chart(
        &fx.library(),
        "clip.webm",
        GAP_CLIP,
        "#00004:01",
        FAULT_NOTES,
    );
    install_fault_skin(&fx, "m.webm", GAP_CLIP);
    write_log(
        &fx.base.join("logs").join("f.tprl"),
        &hits(&chart, |_| true),
        1,
    );
    let ask = fx.base.join("wait.ask");
    let ats: Vec<i64> = (5..=33).map(|k| k * 237).collect();
    let mut steps = vec![
        "capture = { at = 0, file = \"warm.png\" }",
        FAULT_SELECT,
        FAULT_PLAY,
        "result = \"back\"",
    ];
    let captures: Vec<String> = ats
        .iter()
        .map(|at| format!("capture = {{ at = {at}, file = \"c{at}.png\" }}"))
        .collect();
    steps.extend(captures.iter().map(String::as_str));
    steps.extend([LOST, LOST_SKIN]);
    steps.extend([FAULT_SELECT, FAULT_PLAY, "result = \"back\""]);
    steps.extend([
        "capture = { at = 474, file = \"n474.png\" }",
        FAULT_SELECT,
        FAULT_PLAY,
    ]);
    steps.extend(["result = \"back\"", "quit = {}"]);
    let env = [("TAPESTRY_DEV_WAIT_ASK", ask.to_str().unwrap())];
    let (mut suspended, mut workers) = (0, Vec::new());
    let run = run_fault(&fx, &video_text(&steps), &env, |c| {
        let px = c.capture("c1185");
        assert_eq!(
            coded_index(&px, 7),
            Some(0),
            "c1185: the frame before the gap"
        );
        assert!(
            skin_witnesses(&px).iter().all(|&p| shows(p)),
            "c1185: the skin movie"
        );
        workers.push(c.worker());
        suspended = suspend_when_waiting(&ask, workers[0]);
        let px = c.capture("n474");
        assert_eq!(
            coded_index(&px, 7),
            Some(0),
            "n474: the frame of the new worker"
        );
        workers.push(c.worker());
    });
    run.ok();
    assert_ne!(
        workers[0], workers[1],
        "the return's worker is a new process"
    );
    let suspended = game_time_of(&run.stdout, suspended);
    check_starved(&run, suspended, run.chose, 2, 1);
    let seen = run.seen();
    let panel = run.check_lost_frame(seen);
    let skin = run.check_skin_lost_frame(seen);
    let mut after = 0;
    for at in &ats {
        let name = format!("c{at}");
        let (px, t) = (run.px(&name), run.chart_us(&name));
        if t >= panel {
            assert!(panel_black(&px), "{name} at {t}: black from {panel}");
        }
        if t >= skin {
            assert!(skin_black(&px), "{name} at {t}: black from {skin}");
        }
        if t >= panel.max(skin) {
            after += 1;
        }
    }
    assert!(after > 0, "a capture after {panel} and {skin}");

    // On the selection screen: its skin movie alone.
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let skin = fx.base.join("selection");
    fs::create_dir_all(&skin).unwrap();
    write_png(&skin.join("s.png"), 64, 16);
    write_png(&skin.join("f.png"), 16, 16);
    fs::write(
        skin.join("f.fnt"),
        "info face=\"t\" size=16 padding=0,0,0,0\n\
         common lineHeight=16 base=16 scaleW=16 scaleH=16 pages=1\n\
         page id=0 file=\"f.png\"\nchars count=1\n\
         char id=97 x=0 y=0 width=16 height=16 xoffset=0 yoffset=0 xadvance=16 page=0 chnl=15\n",
    )
    .unwrap();
    gen_clip(&skin.join("m.webm"), GAP_CLIP);
    fs::write(skin.join("s.luaskin"), MOVIE_SELECTION).unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.selection]\nroot = '{}'\nentry = 's.luaskin'\n",
            skin.display()
        ),
    )
    .unwrap();
    let ask = fx.base.join("wait.ask");
    let shots: Vec<String> = (1..=48)
        .map(|k| {
            format!(
                "capture = {{ select_at = {}, file = \"s{k}.png\" }}",
                250 * k
            )
        })
        .collect();
    let mut steps: Vec<&str> = vec!["cursor = { path = \"library/a/a.bms\" }"];
    steps.extend(shots.iter().map(String::as_str));
    steps.push("quit = {}");
    let witness = |px: &[[u8; 3]]| px[90 * VIDEO_SIZE[0] + 40];
    let mut first_shown = None;
    let mut suspended = 0;
    let dir = fx.base.join("out");
    let mut command = fx.command(&video_text(&steps));
    command
        .env("TAPESTRY_LOOP_PROBE", "1")
        .env("TAPESTRY_DEV_WAIT_ASK", &ask);
    let (out, stdout, stderr) = fx.run_during(&mut command, RUN_LIMIT, |game| {
        let shown = (1..=40)
            .find(|k| {
                written(&dir, &format!("s{k}"), RUN_LIMIT, game);
                shows(witness(&rgb(&dir.join(format!("s{k}.png")))))
            })
            .expect("the selection's movie shown");
        first_shown = Some(shown);
        suspended = suspend_when_waiting(&ask, worker_of(game, Duration::from_secs(10)));
    });
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let run = FaultRun {
        code: Some(0),
        stdout,
        stderr,
        dir,
        chose: 0,
        acked: None,
    };
    let suspended = game_time_of(&run.stdout, suspended);
    check_starved(&run, suspended, 0, 1, 0);
    // The selection frame that first drew the movie lost is marked when it
    // is built, after the frame before it was presented: every capture
    // presented from that mark on is of that frame or a later one.
    let seen = run.seen();
    let drawn = run.skin_lost_off_play();
    assert!(seen <= drawn, "drawn lost at {drawn}, before {seen}");
    let mut after = 0;
    for k in first_shown.unwrap()..=48 {
        let name = format!("s{k}");
        if side(&run.dir, &name, "presented_us") >= drawn {
            after += 1;
            assert!(
                !shows(witness(&run.px(&name))),
                "{name}: black from the frame drawn lost at {drawn}"
            );
        }
    }
    assert!(after > 0, "a capture after the loss drawn at {drawn}");
}

/// A selection skin whose only picture is a movie, `m.webm`, over the
/// canvas's left half.
const MOVIE_SELECTION: &str = "local skin = { type = 5, name = \"t\", w = 1920, h = 1080 }
if not skin_config then return skin end
skin.source = { { id = \"s\", path = \"s.png\" }, { id = \"m\", path = \"m.webm\" } }
skin.font = { { id = \"f\", path = \"f.fnt\" } }
skin.image = { { id = \"b\", src = \"s\", x = 0, y = 0, w = 8, h = 8 }, { id = \"mv\", src = \"m\" } }
skin.imageset = { { id = \"set\", images = { \"b\", \"b\", \"b\", \"b\", \"b\", \"b\" } } }
skin.text = { { id = \"t\", font = \"f\", size = 30, ref = 10 } }
skin.songlist = { id = \"list\", center = 0, liston = { { id = \"set\", dst = { { x = 1000, y = 500, w = 800, h = 80 } } } },
  listoff = { { id = \"set\", dst = { { x = 1000, y = 500, w = 800, h = 80 } } } },
  text = { { id = \"t\", dst = { { x = 10, y = 10, w = 700, h = 40 } } } } }
skin.destination = {
  { id = \"list\", dst = { { x = 0, y = 0, w = 0, h = 0 } } },
  { id = \"mv\", dst = { { x = 0, y = 0, w = 960, h = 1080 } } },
}
return skin
";

/// The next attempt after a failure draws again, unless its file has two
/// strikes (design.md "Threat Model" T02): a chart video whose decoder is
/// refused an allocation (a decoder's failure, the worker kept) draws
/// black in that attempt and again in the retry, from the same worker; a
/// crash of that worker in the retry gives the file its first strike, and
/// the next retry, with a new worker, draws again; a second crash gives it
/// its second, and the retry after it reads black from its start, the file
/// not opened.
// Covers: T02
#[test]
fn next_attempt_recovers_a_live_worker() {
    let fx = Fixture::new();
    let _keep = KeepOnFailure::of(&fx, "next_attempt_recovers_a_live_worker");
    let chart = write_fault_chart(
        &fx.library(),
        "clip.avi",
        FAULT_CLIP,
        "#00004:01",
        FAULT_NOTES,
    );
    install_video_skin(&fx);
    write_log(
        &fx.base.join("logs").join("f.tprl"),
        &hits(&chart, |_| true),
        1,
    );
    let ats = [474, 1_185, 2_607];
    let mut steps: Vec<String> = [
        "capture = { at = 0, file = \"warm.png\" }",
        FAULT_SELECT,
        FAULT_PLAY,
        "result = \"back\"",
    ]
    .map(str::to_owned)
    .to_vec();
    steps.extend(
        ats.iter()
            .map(|at| format!("capture = {{ at = {at}, file = \"a{at}.png\" }}")),
    );
    steps.push(FAULT_SELECT.to_owned());
    for prefix in ["b", "c", "d"] {
        steps.extend(retried(3_000, prefix, &ats));
    }
    steps.extend([FAULT_PLAY, "result = \"back\"", "quit = {}"].map(str::to_owned));
    let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
    let args = [(
        "TAPESTRY_DEV_WORKER_ARGS",
        "--dev-serve fire refuse-alloc=mjpeg",
    )];
    let mut workers = Vec::new();
    let run = run_fault(&fx, &video_text(&steps), &args, |c| {
        c.due("a474", 0);
        workers.push(c.worker());
        c.fire();
        c.due("b1185", 0);
        workers.push(c.worker());
        crash(workers[1]);
        c.due("c1185", 0);
        workers.push(c.worker());
        crash(workers[2]);
        for at in ats {
            assert!(
                all_black(&c.capture(&format!("d{at}"))),
                "d{at}: black from the start"
            );
        }
    });
    run.ok();
    assert_eq!(
        workers[0], workers[1],
        "the decoder's failure keeps its worker"
    );
    assert_ne!(
        workers[1], workers[2],
        "the crash's retry launches a new worker"
    );
    assert!(
        all_black(&run.px("a2607")),
        "the refused attempt draws black"
    );
    let log = fs::read_to_string(fx.data().join("logs").join("tapestry.log")).unwrap();
    assert!(
        log.contains("a video is lost (Struck)"),
        "the struck file is not opened: {log}"
    );
}

/// Makes `link` a junction to the folder `target` (no privilege needed).
fn junction(link: &Path, target: &Path) {
    let made = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
}

/// The game's log of `fx`'s run.
fn game_log(fx: &Fixture) -> String {
    fs::read_to_string(fx.data().join("logs").join("tapestry.log")).unwrap()
}

/// A chart video named outside its chart's folder is refused when the name
/// is resolved, before anything opens it (design.md "Threat Model" T08):
/// the base plays `sub/clip.avi` from 0 s, `sub` a junction to a folder
/// outside the chart's holding a coded clip, then the in-folder coded clip
/// `in.avi` from 4 s. The outside name is logged absent for resolving
/// outside the folder and never refused at its open (the open is not
/// reached), so the worker is handed no file of it: the panel reads back
/// black until 4 s, never a frame of the outside clip. The in-folder clip,
/// whose opened handle's final path is its canonical path, shows the frame
/// due from 4 s.
// Covers: T08
#[test]
fn a_confined_open_refuses_a_name_outside_the_chart_folder() {
    let fx = Fixture::new();
    let _keep = KeepOnFailure::of(
        &fx,
        "a_confined_open_refuses_a_name_outside_the_chart_folder",
    );
    let outside = fx.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    gen_clip(&outside.join("clip.avi"), FAULT_CLIP);
    let dir = fx.library().join("f");
    fs::create_dir_all(&dir).unwrap();
    junction(&dir.join("sub"), &outside);
    write_wav(&dir.join("key.wav"));
    gen_clip(&dir.join("in.avi"), FAULT_CLIP);
    let text = format!(
        "#PLAYER 1\n#TITLE confined\n#BPM 120\n#WAV01 key.wav\n\
         #BMP01 sub/clip.avi\n#BMP02 in.avi\n#00138:01\n#00004:01\n#00204:02\n{FAULT_NOTES}\n"
    );
    fs::write(dir.join("f.bms"), &text).unwrap();
    let chart = bms_parser::load_bms(text.as_bytes(), &dir, &Default::default())
        .unwrap()
        .chart;
    install_video_skin(&fx);
    write_log(
        &fx.base.join("logs").join("f.tprl"),
        &hits(&chart, |_| true),
        1,
    );
    let ats = fault_ats();
    let run = run_fault(
        &fx,
        &fault_text(&ats, &[], FAULT_SELECT, FAULT_PLAY),
        &[],
        |_| {},
    );
    run.ok();
    let log = game_log(&fx);
    assert!(
        log.contains("bga: sub/clip.avi is absent: the name resolves outside the chart folder"),
        "refused at resolution: {log}"
    );
    assert!(!log.contains("is not played"), "no open refused: {log}");
    check_panel(&run, &ats, 4_000_000, i64::MAX, None);
    assert!(
        ats.iter()
            .any(|at| run.chart_us(&format!("c{at}")) >= 4_000_000),
        "a capture of the in-folder clip"
    );
}

/// A chart video's folder swapped for a link between the name's resolution
/// and its open is refused at the open (design.md "Threat Model" T08): the
/// base plays `sub/clip.avi`, a coded clip inside the chart's folder. On
/// the second play the game waits at the open barrier
/// (`TAPESTRY_DEV_OPEN_GATE`, a file of the test's own) after resolving
/// it; the test replaces `sub` with a junction to a folder outside the
/// chart's holding another coded clip of that name, then opens the
/// barrier. The opened handle's final path is the outside clip's, not the
/// canonical path the folder check passed, so the open is refused and
/// logged: every capture of that play reads back black, never a frame of
/// the outside clip.
// Covers: T08
#[test]
fn a_name_swapped_after_resolution_is_refused_at_open() {
    let fx = Fixture::new();
    let _keep = KeepOnFailure::of(&fx, "a_name_swapped_after_resolution_is_refused_at_open");
    let outside = fx.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    gen_clip(&outside.join("clip.avi"), FAULT_CLIP);
    let dir = fx.library().join("f");
    fs::create_dir_all(dir.join("sub")).unwrap();
    let chart = write_fault_chart(
        &fx.library(),
        "sub/clip.avi",
        FAULT_CLIP,
        "#00004:01",
        FAULT_NOTES,
    );
    install_video_skin(&fx);
    write_log(
        &fx.base.join("logs").join("f.tprl"),
        &hits(&chart, |_| true),
        1,
    );
    let gate = fx.base.join("open.gate");
    let reached = fx.base.join("open.gate.reached");
    let ats = fault_ats();
    let run = run_fault(
        &fx,
        &fault_text(&ats, &[], FAULT_SELECT, FAULT_PLAY),
        &[("TAPESTRY_DEV_OPEN_GATE", gate.to_str().unwrap())],
        |c| {
            // The first play has opened its clip: the next open is the
            // second play's.
            fs::write(&gate, b"").unwrap();
            until(c.game, "the open at its barrier", || reached.exists());
            fs::rename(dir.join("sub"), dir.join("sub.before")).unwrap();
            junction(&dir.join("sub"), &outside);
            fs::remove_file(&gate).unwrap();
        },
    );
    run.ok();
    let log = game_log(&fx);
    assert!(
        log.lines()
            .any(|l| l.contains("clip.avi is not played: the file opened is")
                && l.contains("outside")),
        "refused at the open: {log}"
    );
    for at in &ats {
        let px = run.px(&format!("c{at}"));
        assert!(all_black(&px), "c{at}: {:?}, black", coded_index(&px, 7));
    }
}
