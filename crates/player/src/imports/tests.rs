//! The builds and their thread on self-made skins written here: the
//! configured folder and names, options and file choices by name, the
//! text producer, cancellation mid-Lua and abandonment past the stop
//! bound (dev-tools, through the sandbox's VM hooks), panics, and the
//! reports.

use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bms_model::{ChartBuilder, TimeUs};
use game::{Chain, SkinChoice};
use render::{AttemptInput, Renderer, SkinBundle};
use skin_beatoraja::{
    Cancel, Fact, FactId, FactRead, FactValue, ImportFacts, JudgeClass, Note, NoteKind, NoteLoc,
    Rule, Severity,
};
use skin_model::{ChartText, ScreenKind};

use crate::render_json::centre;

use super::{
    Atlas, ENDED, ImportTarget, KindOutcome, PANICKED, PendingNote, PendingPlay, Ran, STOP_WAIT,
    Shown, Summary, build, line, log_installed, notice, run_cancellable, run_job, specialize,
};

/// Whether a job's thread holds after its send, and how many wait there.
static EXIT: Mutex<(bool, usize)> = Mutex::new((false, 0));
static EXIT_OPENED: Condvar = Condvar::new();

/// The job threads started and not yet at their body's end, an abandoned
/// one included: what [`serial`] drains between tests.
static WORKERS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The jobs started and joined from this thread: a runner's own,
    /// whatever jobs other tests run meanwhile ([`own_counted`]).
    static OWN: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

/// A job thread is about to start; its [`WorkerEnd`], or
/// [`spawn_failed`], counts its end.
pub(super) fn worker_starting() {
    WORKERS.fetch_add(1, Ordering::SeqCst);
    OWN.with(|c| c.set((c.get().0 + 1, c.get().1)));
}

/// The job thread counted by [`worker_starting`] did not start.
pub(super) fn spawn_failed() {
    WORKERS.fetch_sub(1, Ordering::SeqCst);
    OWN.with(|c| c.set((c.get().0 - 1, c.get().1)));
}

/// A job's thread was joined.
pub(super) fn worker_joined() {
    OWN.with(|c| c.set((c.get().0, c.get().1 + 1)));
}

/// The jobs this thread started and joined.
#[cfg(feature = "dev-tools")]
fn own_counted() -> (u64, u64) {
    OWN.with(std::cell::Cell::get)
}

/// Held by a job thread's body: its drop, past [`exit_gate`] or in an
/// unwind that skips it, counts the thread ended.
pub(super) struct WorkerEnd;

impl Drop for WorkerEnd {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The job thread's exit barrier, after its send and before it returns.
pub(super) fn exit_gate() {
    let mut g = EXIT.lock().unwrap_or_else(PoisonError::into_inner);
    g.1 += 1;
    while g.0 {
        g = EXIT_OPENED.wait(g).unwrap_or_else(PoisonError::into_inner);
    }
    g.1 -= 1;
}

fn hold_exit(on: bool) {
    EXIT.lock().unwrap_or_else(PoisonError::into_inner).0 = on;
    EXIT_OPENED.notify_all();
}

fn exit_waiting() -> usize {
    EXIT.lock().unwrap_or_else(PoisonError::into_inner).1
}

/// Whether the next job's thread is refused its start, as the system's
/// refusal would.
static REFUSE_SPAWN: AtomicBool = AtomicBool::new(false);

/// Whether this job's thread is refused its start: once per
/// [`refuse_spawn`].
pub(super) fn spawn_refused() -> bool {
    REFUSE_SPAWN.swap(false, Ordering::SeqCst)
}

pub(crate) fn refuse_spawn() {
    REFUSE_SPAWN.store(true, Ordering::SeqCst);
}

/// The tests that run jobs, or hold the process-wide barriers, one at a
/// time; each barrier is released when the test ends, failed or not, and
/// the test's job threads, an abandoned one included, are waited for
/// then, so none reaches a barrier of the next test.
pub(crate) fn serial() -> Serial {
    static SERIAL: Mutex<()> = Mutex::new(());
    let serial = Serial(SERIAL.lock().unwrap_or_else(PoisonError::into_inner));
    assert!(
        wait_until(|| WORKERS.load(Ordering::SeqCst) == 0),
        "an earlier test's job thread is still running"
    );
    serial
}

pub(crate) struct Serial(
    #[expect(dead_code, reason = "held for its drop")] MutexGuard<'static, ()>,
);

impl Drop for Serial {
    fn drop(&mut self) {
        hold_exit(false);
        REFUSE_SPAWN.store(false, Ordering::SeqCst);
        #[cfg(feature = "dev-tools")]
        skin_beatoraja::lua::vm_hooks::release();
        wait_until(|| WORKERS.load(Ordering::SeqCst) == 0);
    }
}

/// Waits up to 30 s for `done`.
fn wait_until(done: impl Fn() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    done()
}

/// Writes a `w`×`h` PNG of one colour at `path`.
fn png(path: &Path, w: u32, h: u32, [r, g, b]: [u8; 3]) {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap();
    }
    let file = fs::File::create(path).unwrap();
    let mut e = png::Encoder::new(BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let data: Vec<u8> = (0..w * h).flat_map(|_| [r, g, b, 255]).collect();
    e.write_header().unwrap().write_image_data(&data).unwrap();
}

/// `s` as a JSON string's contents.
fn json_str(s: &str) -> String {
    s.replace('\\', "\\\\")
}

/// A destination of `id` over y-down canvas rect `[x, y, w, h]`.
fn dst(id: &str, [x, y, w, h]: [i32; 4]) -> String {
    let y = 1_080 - y - h;
    format!(r#"{{"id": "{id}", "dst": [{{"x": {x}, "y": {y}, "w": {w}, "h": {h}}}]}}"#)
}

const RED: [u8; 3] = [255, 0, 0];
const BLUE: [u8; 3] = [0, 0, 255];
const GREEN: [u8; 3] = [0, 255, 0];
const YELLOW: [u8; 3] = [255, 255, 0];
/// Where the option's image draws.
const COLOUR: [i32; 4] = [100, 100, 100, 100];
/// Where the file choice's image draws.
const BACKGROUND: [i32; 4] = [300, 100, 100, 100];

/// A play skin with option "Color" (items Red, the default, and Blue,
/// each drawing its own image at [`COLOUR`]) and file choice
/// "Background" (`bg/a.png` green, the default, and `bg/b.png` yellow,
/// drawn at [`BACKGROUND`]).
fn options_skin() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let json = format!(
        r#"{{"type": 0, "w": 1920, "h": 1080,
            "property": [{{"name": "Color", "item": [{{"name": "Red", "op": 901}},
                                                     {{"name": "Blue", "op": 902}}],
                           "def": "Red"}}],
            "filepath": [{{"name": "Background", "path": "bg/*.png", "def": "a"}}],
            "source": [{{"if": 901, "values": [{{"id": "c", "path": "red.png"}}]}},
                       {{"if": 902, "values": [{{"id": "c", "path": "blue.png"}}]}},
                       {{"id": "b", "path": "bg/*.png"}}],
            "image": [{{"id": "c", "src": "c", "x": 0, "y": 0, "w": 16, "h": 16}},
                      {{"id": "b", "src": "b", "x": 0, "y": 0, "w": 16, "h": 16}}],
            "destination": [{}, {}]}}"#,
        dst("c", COLOUR),
        dst("b", BACKGROUND)
    );
    fs::write(d.join("s.json"), json).unwrap();
    png(&d.join("red.png"), 16, 16, RED);
    png(&d.join("blue.png"), 16, 16, BLUE);
    png(&d.join("bg/a.png"), 16, 16, GREEN);
    png(&d.join("bg/b.png"), 16, 16, YELLOW);
    dir
}

/// A play skin `entry` whose play-critical EX score number reads its
/// digits from `path`, and `n.png` (the digits, red).
fn critical_skin(dir: &Path, entry: &str, path: &str) {
    let json = format!(
        r#"{{"type": 0, "w": 1920, "h": 1080,
            "source": [{{"id": "n", "path": "{}"}}],
            "value": [{{"id": "ex", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                        "ref": 71}}],
            "destination": [{}]}}"#,
        json_str(path),
        dst("ex", [0, 0, 120, 20])
    );
    fs::write(dir.join(entry), json).unwrap();
    png(&dir.join("n.png"), 120, 20, RED);
}

/// The choice of `entry` in `root`, nothing named.
fn choice(root: impl Into<PathBuf>, entry: &str) -> SkinChoice {
    SkinChoice {
        root: root.into(),
        entry: entry.to_owned(),
        options: Default::default(),
        files: Default::default(),
    }
}

/// What `r` packs for, with the generated default font.
fn target(r: &Renderer) -> ImportTarget {
    ImportTarget {
        limits: r.atlas_limits(),
        size: [1_920, 1_080],
        default_font: || Ok(crate::render_outline::default_font()),
    }
}

/// Builds `choice` as a play skin for `t`.
fn play(choice: &SkinChoice, t: &ImportTarget) -> KindOutcome {
    build(ScreenKind::Play, choice, false, t, &Cancel::new())
}

#[track_caller]
fn bundled(o: KindOutcome) -> (Arc<SkinBundle>, Summary) {
    match o {
        KindOutcome::Bundle(b, s) => (b, s),
        other => panic!("{other:?}"),
    }
}

#[track_caller]
fn refused(o: KindOutcome) -> String {
    match o {
        KindOutcome::Refused(why) => why,
        other => panic!("{other:?}"),
    }
}

#[track_caller]
fn pending(o: KindOutcome) -> (Arc<PendingPlay>, PendingNote) {
    match o {
        KindOutcome::Pending(p, note) => (p, note),
        other => panic!("{other:?}"),
    }
}

/// A frame of `b`'s attempt on `r`, read back.
fn frame(r: &mut Renderer, b: &Arc<SkinBundle>, request: u64) -> Vec<u8> {
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let ctx = crate::render_common::context(&crate::render_common::GAUGES, false);
    let text = ChartText {
        title: "",
        subtitle: "",
        artist: "",
        subartist: "",
        genre: "",
    };
    let a = AttemptInput {
        chart: &chart,
        context: &ctx,
        text: &text,
        target: r.target(),
    };
    let token = r.stage(b, &a, request).unwrap();
    r.commit(token).unwrap();
    let v = crate::render_common::view(0, 0);
    let input = crate::render_common::input(&v, TimeUs(0), TimeUs(0), &[]);
    assert!(r.frame(&input).unwrap().presented);
    crate::render_common::read_back(r)
}

/// A frame of `b` staged for an attempt at `loaded`'s chart, through its
/// attempt context, as a play stages it.
fn frame_of(r: &mut Renderer, b: &Arc<SkinBundle>, request: u64, loaded: &game::Loaded) -> Vec<u8> {
    let owned = crate::attempt_context::AttemptContext::new(
        loaded.attempt(),
        crate::attempt_context::SIMPLE_SCRATCH,
        0,
        false,
    );
    let ctx = owned.context();
    let text = owned.text();
    let a = AttemptInput {
        chart: loaded.chart(),
        context: &ctx,
        text: &text,
        target: r.target(),
    };
    let token = r.stage(b, &a, request).unwrap();
    r.commit(token).unwrap();
    let v = crate::render_common::view(0, 0);
    let input = crate::render_common::input(&v, TimeUs(0), TimeUs(0), &[]);
    assert!(r.frame(&input).unwrap().presented);
    crate::render_common::read_back(r)
}

/// A skin of another kind than the one it is built for refuses, and a
/// result skin imports for its own kind.
#[test]
fn a_skin_builds_only_for_its_own_kind_and_importer() {
    // Its Lua header pass opens a VM, which the barrier tests count.
    let _serial = serial();
    let r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let dir = options_skin();
    fs::write(
        dir.path().join("r.json"),
        r#"{"type": 7, "w": 1920, "h": 1080}"#,
    )
    .unwrap();
    let c = Cancel::new();
    let as_result = build(
        ScreenKind::Result,
        &choice(dir.path(), "s.json"),
        false,
        &t,
        &c,
    );
    assert_eq!(
        refused(as_result),
        "a 7-key play skin in the single-chart result slot"
    );
    // A result skin imports for its kind (design D10): one with neither an
    // EX score nor a status is refused for it.
    let r = refused(build(
        ScreenKind::Result,
        &choice(dir.path(), "r.json"),
        false,
        &t,
        &c,
    ));
    assert!(r.contains("has no EX score"), "{r}");
    // Its configuration and its configured Lua pass are its own: an
    // option it does not declare, or a configured pass that fails as soon
    // as it runs, refuses it; the Lua skin runs both passes (with
    // dev-tools, this thread's passes count them).
    fs::write(
        dir.path().join("bad.json"),
        r#"{"type": 7, "w": 1920, "h": 1080,
            "property": [{"name": "Color", "item": [{"name": "Red", "op": 901}], "def": "Red"}]}"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("r.luaskin"),
        "local skin = { type = 7, name = \"r\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         error(\"the configured pass ran\")\n",
    )
    .unwrap();
    let mut bad = choice(dir.path(), "bad.json");
    bad.options.insert("Color".to_owned(), "Green".to_owned());
    for (c2, entry) in [
        (bad, "bad.json"),
        (choice(dir.path(), "r.luaskin"), "r.luaskin"),
    ] {
        #[cfg(feature = "dev-tools")]
        let passes = skin_beatoraja::lua::vm_hooks::thread_passes();
        let why = refused(build(ScreenKind::Result, &c2, false, &t, &c));
        assert!(!why.is_empty(), "{entry}");
        #[cfg(feature = "dev-tools")]
        assert_eq!(
            skin_beatoraja::lua::vm_hooks::thread_passes() - passes,
            if entry == "r.luaskin" { 2 } else { 0 },
            "{entry}"
        );
    }
}

/// The configured folder is the user's absolute choice, taken with
/// forward, back or mixed separators, and refused naming it when it is
/// not a folder; the entry and every name a skin's play-critical image
/// gives stay under the folder's name and containment rules, a drive,
/// UNC, verbatim or stream name, or a link out of the folder, refusing
/// the whole build with the located reason.
#[test]
fn the_folder_is_taken_as_given_and_names_stay_inside_it() {
    let r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let outer = tempfile::tempdir().unwrap();
    let skin = outer.path().join("skin");
    fs::create_dir_all(&skin).unwrap();
    critical_skin(&skin, "s.json", "n.png");
    let back = skin.display().to_string();
    let forward = back.replace('\\', "/");
    let mixed = forward.replacen('/', "\\", 2);
    assert!(forward.contains('/') && mixed.contains('/') && mixed.contains('\\'));
    for root in [&back, &forward, &mixed] {
        bundled(play(&choice(root, "s.json"), &t));
    }
    // Not a folder.
    let missing = skin.join("missing");
    let why = refused(play(&choice(&missing, "s.json"), &t));
    assert!(why.contains(&missing.display().to_string()), "{why}");
    let why = refused(play(&choice(skin.join("n.png"), "s.json"), &t));
    assert!(why.ends_with("not a folder"), "{why}");

    // An outside file a link inside points to.
    fs::write(outer.path().join("outside.json"), "{}").unwrap();
    png(&outer.path().join("outside.png"), 120, 20, RED);
    std::os::windows::fs::symlink_file(outer.path().join("outside.json"), skin.join("out.json"))
        .unwrap();
    std::os::windows::fs::symlink_file(outer.path().join("outside.png"), skin.join("out.png"))
        .unwrap();
    let rejected = "the name is not a relative path";
    let escapes = "the name resolves outside the skin folder";
    for (entry, says) in [
        ("C:s.json", rejected),
        (r"\\server\share\s.json", rejected),
        (r"\\?\C:\s.json", rejected),
        ("s.json:stream", rejected),
        ("out.json", escapes),
    ] {
        let why = refused(play(&choice(&skin, entry), &t));
        assert!(why.contains(says), "{entry}: {why}");
    }
    // The importer reads a play-critical image through the same rules and
    // refuses the whole skin at the element: a name of the wrong form as a
    // file it cannot read, a link out of the folder by the folder's rule.
    let unreadable = "bad.json:5:29: the EX score number cannot be imported: \
                      a missing or unreadable file";
    let outside = "bad.json:5:29: a play-critical image: file 0 resolves outside the skin folder";
    for (name, says) in [
        ("C:n.png", unreadable),
        (r"\\server\share\n.png", unreadable),
        (r"\\?\C:\n.png", unreadable),
        ("n.png:stream", unreadable),
        ("out.png", outside),
    ] {
        critical_skin(&skin, "bad.json", name);
        let why = refused(play(&choice(&skin, "bad.json"), &t));
        assert_eq!(why, says, "{name}");
    }
    // A font file's name: the text set in it is decorative, so it is
    // dropped at its location, and no font is read for it.
    for path in ["C:ttf.ttf", "out.ttf"] {
        let fonts = [crate::render_outline::FontRec {
            id: "f",
            path,
            kind: None,
        }];
        let texts = [crate::render_outline::text(
            "f",
            100,
            crate::render_outline::Value::Ref(10),
            [100, 100, 1_000, 100],
        )];
        let dir = crate::render_outline::folder(&[], &fonts, &texts);
        fs::write(
            outer.path().join("outside.ttf"),
            crate::font_support::build(&crate::font_support::latin("Skin")),
        )
        .unwrap();
        if path == "out.ttf" {
            std::os::windows::fs::symlink_file(
                outer.path().join("outside.ttf"),
                dir.path().join("out.ttf"),
            )
            .unwrap();
        }
        let (b, summary) = bundled(play(&choice(dir.path(), "s.json"), &t));
        assert!(b.text_residency().is_none(), "{path}");
        let dropped: Vec<_> = summary
            .notes
            .iter()
            .map(|n| (n.at.map(|at| at.source), n.severity, n.kind))
            .collect();
        assert_eq!(
            dropped,
            [(Some(0), Severity::Dropped, NoteKind::MissingAsset)],
            "{path}"
        );
    }
}

/// Options and file choices by name: Blue's image and `b.png` draw, the
/// report names `b.png` as configured; the defaults draw Red and `a.png`;
/// unknown names refuse with their diagnostics.
#[test]
fn options_and_file_choices_are_chosen_by_name() {
    let mut r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let dir = options_skin();
    let mut c = choice(dir.path(), "s.json");
    let (b, summary) = bundled(play(&c, &t));
    let p = frame(&mut r, &b, 1);
    assert_eq!((centre(&p, COLOUR), centre(&p, BACKGROUND)), (RED, GREEN));
    assert_eq!(summary.files[0].candidate.as_deref(), Some("a.png"));
    assert_eq!(summary.files[0].rule, Rule::Default);

    c.options.insert("Color".into(), "Blue".into());
    c.files.insert("Background".into(), "b.png".into());
    let (b, summary) = bundled(play(&c, &t));
    let p = frame(&mut r, &b, 2);
    assert_eq!((centre(&p, COLOUR), centre(&p, BACKGROUND)), (BLUE, YELLOW));
    assert_eq!(summary.files.len(), 1);
    let f = &summary.files[0];
    assert_eq!(
        (f.choice.as_str(), f.candidate.as_deref(), f.rule),
        ("Background", Some("b.png"), Rule::Configured)
    );

    for (option, item, says) in [
        (
            "Colour",
            "Blue",
            r#"option "Colour" is not in the skin's header"#,
        ),
        ("Color", "Green", r#"option "Color" has no item "Green""#),
    ] {
        let mut c = choice(dir.path(), "s.json");
        c.options.insert(option.into(), item.into());
        assert_eq!(refused(play(&c, &t)), says);
    }
    for (file, candidate, says) in [
        (
            "Background",
            "c.png",
            r#"file choice "Background" has no candidate "c.png""#,
        ),
        (
            "Backdrop",
            "a.png",
            r#"file choice "Backdrop" is not in the skin's header"#,
        ),
    ] {
        let mut c = choice(dir.path(), "s.json");
        c.files.insert(file.into(), candidate.into());
        assert_eq!(refused(play(&c, &t)), says);
    }
}

/// Where each of [`tagged_skin`]'s choices draws: Cover, Lift, Hidden
/// and Frame.
const TAGGED: [[i32; 4]; 4] = [
    [100, 300, 100, 100],
    [300, 300, 100, 100],
    [500, 300, 100, 100],
    [700, 300, 100, 100],
];

/// A play skin with three tagged file choices over `parts/*.png`, Cover,
/// Lift and Hidden (`a.png` green, their default, and `b.png` yellow),
/// and the folder choice Frame over `frames/*` (`x/f.png` red, the
/// default, and `y/f.png` blue), each drawing at its place in [`TAGGED`].
fn tagged_skin() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let [cover, lift, hidden, frame] = TAGGED;
    let json = format!(
        r#"{{"type": 0, "w": 1920, "h": 1080,
            "filepath": [{{"name": "Cover", "path": "parts/*.png|cover|", "def": "a"}},
                         {{"name": "Lift", "path": "parts/*.png|lift|", "def": "a"}},
                         {{"name": "Hidden", "path": "parts/*.png|hidden|", "def": "a"}},
                         {{"name": "Frame", "path": "frames/*", "def": "x"}}],
            "source": [{{"id": "c", "path": "parts/*.png|cover|"}},
                       {{"id": "l", "path": "parts/*.png|lift|"}},
                       {{"id": "h", "path": "parts/*.png|hidden|"}},
                       {{"id": "f", "path": "frames/*/f.png"}}],
            "image": [{{"id": "c", "src": "c", "x": 0, "y": 0, "w": 16, "h": 16}},
                      {{"id": "l", "src": "l", "x": 0, "y": 0, "w": 16, "h": 16}},
                      {{"id": "h", "src": "h", "x": 0, "y": 0, "w": 16, "h": 16}},
                      {{"id": "f", "src": "f", "x": 0, "y": 0, "w": 16, "h": 16}}],
            "destination": [{}, {}, {}, {}]}}"#,
        dst("c", cover),
        dst("l", lift),
        dst("h", hidden),
        dst("f", frame)
    );
    fs::write(d.join("s.json"), json).unwrap();
    png(&d.join("parts/a.png"), 16, 16, GREEN);
    png(&d.join("parts/b.png"), 16, 16, YELLOW);
    png(&d.join("frames/x/f.png"), 16, 16, RED);
    png(&d.join("frames/y/f.png"), 16, 16, BLUE);
    dir
}

/// Tagged and folder choices by their names and their candidates' entry
/// names: only the configured choice changes; a value with the tag, a
/// path or another letter case is no candidate, and a key that is no
/// choice's name is not in the header.
#[test]
fn tagged_and_directory_choices_are_chosen_by_name() {
    let mut r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let dir = tagged_skin();
    let colours = |p: &[u8]| TAGGED.map(|at| centre(p, at));
    let c = choice(dir.path(), "s.json");
    let (b, _) = bundled(play(&c, &t));
    let p = frame(&mut r, &b, 1);
    assert_eq!(colours(&p), [GREEN, GREEN, GREEN, RED]);

    let mut c = choice(dir.path(), "s.json");
    c.files.insert("Lift".into(), "b.png".into());
    let (b, summary) = bundled(play(&c, &t));
    let p = frame(&mut r, &b, 2);
    assert_eq!(colours(&p), [GREEN, YELLOW, GREEN, RED]);
    let lift = summary.files.iter().find(|f| f.choice == "Lift").unwrap();
    assert_eq!(
        (lift.candidate.as_deref(), lift.rule),
        (Some("b.png"), Rule::Configured)
    );

    let mut c = choice(dir.path(), "s.json");
    c.files.insert("Frame".into(), "y".into());
    let (b, _) = bundled(play(&c, &t));
    let p = frame(&mut r, &b, 3);
    assert_eq!(colours(&p), [GREEN, GREEN, GREEN, BLUE]);

    for (file, candidate) in [
        ("Lift", "b.png|lift|"),
        ("Lift", "parts/b.png"),
        ("Lift", "B.png"),
        ("Frame", "Y"),
        ("Frame", "frames/y"),
    ] {
        let mut c = choice(dir.path(), "s.json");
        c.files.insert(file.into(), candidate.into());
        assert_eq!(
            refused(play(&c, &t)),
            format!(r#"file choice "{file}" has no candidate "{candidate}""#)
        );
    }
    for file in ["parts/*.png|lift|", "lift"] {
        let mut c = choice(dir.path(), "s.json");
        c.files.insert(file.into(), "b.png".into());
        assert_eq!(
            refused(play(&c, &t)),
            format!(r#"file choice "{file}" is not in the skin's header"#)
        );
    }
}

/// A Lua play skin whose configured pass runs `body` with `ms` the host
/// module, drawing a 64×64 red `a.png` at `x`, which `body` sets.
fn lua_skin(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let entry = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }} }}\n\
         local x = 0\n\
         {body}\n\
         skin.destination = {{ {{ id = \"i\", dst = {{ {{ x = x, y = 0, w = 64, h = 64 }} }} }} }}\n\
         return skin\n"
    );
    fs::write(dir.path().join("play.luaskin"), entry).unwrap();
    png(&dir.path().join("a.png"), 64, 64, RED);
    dir
}

/// A Lua play skin builds against the base facts: autoplay (option 33,
/// on) is known, so a skin reading it builds for the session's value; a
/// skin reading the chart's judgment class (option 180) or its play level
/// (number 96), known only once a chart is chosen, ends pending, naming
/// the fact and the files read so far.
#[test]
fn a_lua_skin_builds_on_the_base_facts() {
    // Its VMs count, and are held, with the hooks' tests'.
    let _serial = serial();
    let mut r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let dir = lua_skin("if ms.option(33) then x = 500 end");
    let c = choice(dir.path(), "play.luaskin");
    // Source y 0 is the canvas's bottom: the image lies on rows 1016 to 1079.
    let at = |p: &[u8], x| crate::render_common::rgb_at(p, 1_920, (x, 1_048));
    for (autoplay, x, request) in [(false, 32, 1), (true, 532, 2)] {
        let (b, _) = bundled(build(ScreenKind::Play, &c, autoplay, &t, &Cancel::new()));
        let p = frame(&mut r, &b, request);
        assert_eq!(at(&p, x), RED, "autoplay {autoplay}");
    }
    for (body, fact) in [
        ("if ms.option(180) then x = 500 end", FactId::JudgeClass),
        ("x = ms.number(96)", FactId::PlayLevel),
    ] {
        let dir = lua_skin(body);
        let (_, note) = pending(play(&choice(dir.path(), "play.luaskin"), &t));
        assert_eq!(note.fact, fact, "{body}");
        assert_eq!(note.dependencies, ["play.luaskin"], "{body}");
        assert!(note.usage.instructions > 0, "{body}: {:?}", note.usage);
    }
}

/// Where [`class_skin`] draws its image while a frame reads the chart as
/// easy.
const FRAME_CLASS: [i32; 4] = [300, 100, 100, 100];

/// A Lua play skin whose configured pass draws `blue.png` for an easy
/// chart (option 183) and `red.png` otherwise, at [`COLOUR`], and which
/// draws the same image at [`FRAME_CLASS`] while a frame's `option(183)`
/// holds.
fn class_skin() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let [x, y, w, h] = COLOUR;
    let [fx, fy, fw, fh] = FRAME_CLASS;
    let entry = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         local path = \"red.png\"\n\
         if ms.option(183) then path = \"blue.png\" end\n\
         skin.source = {{ {{ id = \"a\", path = path }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 16, h = 16 }} }}\n\
         skin.destination = {{ {{ id = \"i\", dst = {{ {{ x = {x}, y = {}, w = {w}, h = {h} }} }} }},\n\
           {{ id = \"i\", draw = function() return ms.option(183) end,\n\
              dst = {{ {{ x = {fx}, y = {}, w = {fw}, h = {fh} }} }} }} }}\n\
         return skin\n",
        1_080 - y - h,
        1_080 - fy - fh
    );
    fs::write(dir.path().join("play.luaskin"), entry).unwrap();
    png(&dir.path().join("red.png"), 16, 16, RED);
    png(&dir.path().join("blue.png"), 16, 16, BLUE);
    dir
}

/// A pending skin reading the judgment class, specialized for the facts of
/// loaded charts, draws its class's image: red for `#RANK 2` (normal) and
/// for no rank, blue for `#RANK 3` and `#DEFEXRANK 120` (easy, 90 once the
/// reference normalizes it) and for `#DEFEXRANK 1` (0, then 100), under
/// either preset; each specialization records the class it read, which
/// another chart of the class holds. A frame staged for the same loaded
/// chart reads the same class: the image at [`FRAME_CLASS`] is drawn for
/// the easy charts alone.
#[test]
fn a_specialization_draws_its_charts_class() {
    let _serial = serial();
    let mut r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let dir = class_skin();
    let (p, note) = pending(play(&choice(dir.path(), "play.luaskin"), &t));
    assert_eq!(note.fact, FactId::JudgeClass);
    let facts =
        |text| super::facts::attempt_facts(&super::facts::tests::loaded(text, "tapestry"), false);
    let mut request = 0;
    for (text, preset, colour, class) in [
        ("#RANK 2\n", "tapestry", RED, JudgeClass::Normal),
        ("", "tapestry", RED, JudgeClass::Normal),
        ("#RANK 3\n", "tapestry", BLUE, JudgeClass::Easy),
        (
            "#DEFEXRANK 120\n",
            "beatoraja-style",
            BLUE,
            JudgeClass::Easy,
        ),
        ("#DEFEXRANK 120\n", "tapestry", BLUE, JudgeClass::Easy),
        ("#DEFEXRANK 1\n", "beatoraja-style", BLUE, JudgeClass::Easy),
    ] {
        let loaded = super::facts::tests::loaded(text, preset);
        let f = super::facts::attempt_facts(&loaded, false);
        let s = specialize(&p, &f, &Cancel::new()).unwrap();
        assert_eq!(s.summary.sources, ["play.luaskin"], "{text:?}");
        request += 1;
        let pixels = frame_of(&mut r, &s.bundle, request, &loaded);
        assert_eq!(centre(&pixels, COLOUR), colour, "{text:?} {preset}");
        let at_frame = centre(&pixels, FRAME_CLASS);
        if class == JudgeClass::Easy {
            assert_eq!(at_frame, BLUE, "{text:?} {preset}");
        } else {
            assert!(at_frame != RED && at_frame != BLUE, "{text:?} {preset}");
        }
        assert_eq!(
            s.facts.reads,
            [FactRead {
                fact: FactId::JudgeClass,
                value: FactValue::Class(Some(class)),
            }],
            "{text:?}"
        );
        assert!(s.facts.holds(&f), "{text:?}");
        assert_eq!(
            s.facts.holds(&facts("#RANK 3\n")),
            class == JudgeClass::Easy
        );
    }
}

/// A pending skin whose header pass loads `hdr.lua` and includes
/// `props.json` (its options), and whose configured pass loads `cfg.lua`,
/// includes `img.json` (its images) and then reads the play level: each
/// module names the image file its source draws, `h` at x 0 and `c` at
/// x 200, and `img.json`'s image `j` at x 400 takes source `r` (red) or `b`
/// (blue).
fn sources_skin() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    fs::write(
        d.join("play.luaskin"),
        "local skin = { type = 0, name = \"t\", w = 1920, h = 1080,\n\
         property = { include = \"props.json\" }, image = { include = \"img.json\" } }\n\
         local hdr = require(\"hdr\")\n\
         if not skin_config then return skin end\n\
         local cfg = require(\"cfg\")\n\
         local level = require(\"main_state\").number(96)\n\
         skin.source = { { id = \"h\", path = hdr }, { id = \"c\", path = cfg },\n\
         { id = \"r\", path = \"red.png\" }, { id = \"b\", path = \"blue.png\" } }\n\
         skin.destination = {\n\
         { id = \"h\", dst = { { x = 0, y = 0, w = 64, h = 64 } } },\n\
         { id = \"c\", dst = { { x = 200, y = 0, w = 64, h = 64 } } },\n\
         { id = \"j\", dst = { { x = 400, y = 0, w = 64, h = 64 } } } }\n\
         return skin\n",
    )
    .unwrap();
    fs::write(d.join("hdr.lua"), "return \"red.png\"").unwrap();
    fs::write(d.join("cfg.lua"), "return \"red.png\"").unwrap();
    fs::write(d.join("props.json"), r#"[{"name": "o"}]"#).unwrap();
    fs::write(d.join("img.json"), images_json("r")).unwrap();
    png(&d.join("red.png"), 64, 64, RED);
    png(&d.join("blue.png"), 64, 64, BLUE);
    dir
}

/// [`sources_skin`]'s `img.json`, image `j` taking source `j_src`.
fn images_json(j_src: &str) -> String {
    format!(
        r#"[{{"id": "h", "src": "h", "x": 0, "y": 0, "w": 64, "h": 64}},
            {{"id": "c", "src": "c", "x": 0, "y": 0, "w": 64, "h": 64}},
            {{"id": "j", "src": "{j_src}", "x": 0, "y": 0, "w": 64, "h": 64}}]"#
    )
}

/// A specialization starts from the header pass's store: a module and a
/// JSON include the header pass read keep the bytes read at startup (an
/// include broken since is not read), while a module and an include only
/// the configured pass reads are read again, an edit seen and a removal
/// refused naming the file. A specialization already made is not changed
/// by the edits, and its facts still hold.
#[test]
fn a_specialization_reads_again_only_what_the_header_pass_did_not() {
    let _serial = serial();
    let mut r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let dir = sources_skin();
    let d = dir.path();
    let (p, note) = pending(play(&choice(d, "play.luaskin"), &t));
    assert_eq!(note.fact, FactId::PlayLevel);
    let mut facts = ImportFacts::base(ScreenKind::Play, Fact::Known(false));
    facts.judge_class = Fact::Known(None);
    facts.play_level = Fact::Known(12);
    // Source y 0 is the canvas's bottom: the images lie on rows 1016 to 1079.
    let at = |p: &[u8]| [32, 232, 432].map(|x| crate::render_common::rgb_at(p, 1_920, (x, 1_048)));

    let first = specialize(&p, &facts, &Cancel::new()).unwrap();
    assert_eq!(at(&frame(&mut r, &first.bundle, 1)), [RED, RED, RED]);
    assert_eq!(
        first.facts.reads,
        [FactRead {
            fact: FactId::PlayLevel,
            value: FactValue::Level(12),
        }]
    );

    fs::write(d.join("hdr.lua"), "return \"blue.png\"").unwrap();
    fs::write(d.join("cfg.lua"), "return \"blue.png\"").unwrap();
    fs::write(d.join("img.json"), images_json("b")).unwrap();
    fs::write(d.join("props.json"), "[{").unwrap();
    let edited = specialize(&p, &facts, &Cancel::new()).unwrap();
    assert_eq!(at(&frame(&mut r, &edited.bundle, 2)), [RED, BLUE, BLUE]);
    // The earlier specialization is unchanged, and fits the same facts.
    assert_eq!(at(&frame(&mut r, &first.bundle, 3)), [RED, RED, RED]);
    assert!(first.facts.holds(&facts));

    fs::remove_file(d.join("cfg.lua")).unwrap();
    let why = specialize(&p, &facts, &Cancel::new()).unwrap_err();
    assert!(why.contains("cannot read cfg.lua"), "{why}");
}

/// A specialization whose facts leave a fact pending refuses, as an
/// internal error, and a cancelled one stops.
#[test]
fn a_specialization_refuses_a_pending_fact_and_stops_when_cancelled() {
    let _serial = serial();
    let r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let dir = class_skin();
    let (p, _) = pending(play(&choice(dir.path(), "play.luaskin"), &t));
    let base = ImportFacts::base(ScreenKind::Play, Fact::Known(false));
    assert_eq!(
        specialize(&p, &base, &Cancel::new()).unwrap_err(),
        "the skin reads the chart's judgment class, which the attempt's facts leave pending"
    );
    let mut facts = base;
    facts.judge_class = Fact::Known(None);
    let cancel = Cancel::new();
    cancel.cancel();
    let why = specialize(&p, &facts, &cancel).unwrap_err();
    assert!(
        why.ends_with(&skin_beatoraja::LuaWhy::Cancelled.to_string()),
        "{why}"
    );
}

/// A skin without text, with bitmap text only, and with text in a font
/// file and in the default font each bundle; only the last has a text
/// producer.
#[test]
fn a_text_producer_comes_only_with_rasterized_text() {
    use crate::render_outline::{Value, distance_field, text, ttf};
    let r = crate::render_common::offscreen(1_920, 1_080);
    let t = target(&r);
    let plain = options_skin();
    let bitmap = crate::render_json::folder();
    let outline = crate::render_outline::folder(
        &[(
            "ttf.ttf",
            crate::font_support::build(&crate::font_support::latin("Skin")),
        )],
        &[ttf(), distance_field()],
        &[
            text("f", 100, Value::Ref(10), [100, 100, 1_000, 100]),
            text("d", 100, Value::Ref(10), [100, 300, 1_000, 100]),
        ],
    );
    let mut producers = Vec::new();
    for dir in [&plain, &bitmap, &outline] {
        let (b, _) = bundled(play(&choice(dir.path(), "s.json"), &t));
        producers.push((b.text_layer().is_some(), b.text_residency().is_some()));
    }
    assert_eq!(producers, [(false, false), (false, false), (true, true)]);
}

/// A Lua skin whose header pass loops for ever in metered Lua: each
/// comparison is one instruction over 1 MiB, so the loop runs to the
/// sandbox's time cap (seconds) unless cancelled, and a cancellation is
/// seen within a check's 100 instructions.
#[cfg(feature = "dev-tools")]
pub(super) fn looping() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("s.luaskin"),
        "local function rep(s, n) local r, p = \"\", s \
         while n > 0 do if n % 2 == 1 then r = r .. p end n = n // 2 \
         if n > 0 then p = p .. p end end return r end\n\
         local s, t = rep(\"x\", 1 << 20), rep(\"x\", 1 << 20)\n\
         while true do if s == t then end end\n",
    )
    .unwrap();
    dir
}

/// A job building `dir`'s looping skin, its outcome kept by a witness
/// that records the thread it is dropped on.
#[cfg(feature = "dev-tools")]
fn looping_job(
    dir: &Path,
    dropped: &Arc<Mutex<Option<String>>>,
) -> impl FnOnce(&Cancel) -> Witness + Send + 'static {
    let c = choice(dir, "s.luaskin");
    let dropped = Arc::clone(dropped);
    move |cancel| {
        let t = ImportTarget {
            limits: skin_model::AtlasLimits {
                max_side: 4_096,
                max_layers: 16,
                reserved_layers: render::RESERVED_LAYERS,
            },
            size: [1_920, 1_080],
            default_font: render::DefaultFont::runtime,
        };
        Witness(
            Some(build(ScreenKind::Play, &c, false, &t, cancel)),
            dropped,
        )
    }
}

/// An outcome that notes the thread it is dropped on.
#[cfg(feature = "dev-tools")]
struct Witness(Option<KindOutcome>, Arc<Mutex<Option<String>>>);

#[cfg(feature = "dev-tools")]
impl Drop for Witness {
    fn drop(&mut self) {
        let name = std::thread::current().name().map(str::to_owned);
        *self.1.lock().unwrap() = Some(name.unwrap_or_default());
    }
}

/// The signals [`run_job`] asks, which a test sets: the attempt's
/// cancellation, and the run's end (the caller folds the window's close
/// and the threads' failures into one).
#[derive(Clone, Default)]
struct Signals {
    attempt: Arc<AtomicBool>,
    end: Arc<AtomicBool>,
}

/// Runs `job` through [`run_job`] on another thread with the signals the
/// test sets: the signals and the runner's answer.
fn started_job<T: Send + 'static>(
    job: impl FnOnce(&Cancel) -> T + Send + 'static,
) -> (Signals, std::sync::mpsc::Receiver<Ran<T>>) {
    let signals = Signals::default();
    let s = signals.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ran = run_job(
            job,
            || s.attempt.load(Ordering::SeqCst),
            || s.end.load(Ordering::SeqCst),
        );
        let _ = tx.send(ran);
    });
    (signals, rx)
}

/// The jobs started and joined so far (dev-tools).
#[cfg(feature = "dev-tools")]
fn counted() -> (u64, u64) {
    (super::jobs_started(), super::jobs_joined())
}

/// Runs `job` on another thread with a stop the test sets: the runner's
/// thread, the stop and the runner's answer.
fn started<T: Send + 'static>(
    job: impl FnOnce(&Cancel) -> T + Send + 'static,
) -> (Arc<AtomicBool>, std::sync::mpsc::Receiver<Ran<T>>) {
    let stop = Arc::new(AtomicBool::new(false));
    let theirs = Arc::clone(&stop);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ran = run_cancellable(job, move || theirs.load(Ordering::SeqCst));
        let _ = tx.send(ran);
    });
    (stop, rx)
}

/// A runner's answer with the jobs it started and joined itself.
#[cfg(feature = "dev-tools")]
type OwnAnswer<T> = std::sync::mpsc::Receiver<(Ran<T>, (u64, u64))>;

/// [`started`], the runner also answering the jobs it started and joined
/// itself: its own job's, unmoved by other tests' jobs.
#[cfg(feature = "dev-tools")]
fn started_own<T: Send + 'static>(
    job: impl FnOnce(&Cancel) -> T + Send + 'static,
) -> (Arc<AtomicBool>, OwnAnswer<T>) {
    let stop = Arc::new(AtomicBool::new(false));
    let theirs = Arc::clone(&stop);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ran = run_cancellable(job, move || theirs.load(Ordering::SeqCst));
        let _ = tx.send((ran, own_counted()));
    });
    (stop, rx)
}

/// The refusal a cancelled Lua pass gives, as its outcome's text ends.
#[cfg(feature = "dev-tools")]
#[track_caller]
fn cancelled(ran: Ran<Witness>) {
    let Ran::Done(mut w) = ran else {
        panic!("{ran:?}");
    };
    let why = refused(w.0.take().unwrap());
    let lua = skin_beatoraja::LuaWhy::Cancelled.to_string();
    assert!(why.ends_with(&lua), "{why}");
}

#[cfg(feature = "dev-tools")]
impl std::fmt::Debug for Witness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Waits until the looping pass's VM is open beyond the `base` open
/// before and its Lua is executing the loop: the instruction hook has run
/// [`LOOP_STEPS`] steps since the VM was seen, more than building its
/// strings takes.
#[cfg(feature = "dev-tools")]
#[track_caller]
pub(super) fn executing(base: usize) {
    use skin_beatoraja::lua::vm_hooks;
    assert!(wait_until(|| vm_hooks::live() > base));
    let from = vm_hooks::steps();
    assert!(wait_until(|| vm_hooks::steps() >= from + LOOP_STEPS));
}

/// Instruction hook steps of 100 instructions: the strings take a few.
#[cfg(feature = "dev-tools")]
const LOOP_STEPS: u64 = 20;

/// A stop while the looping pass's Lua executes its loop cancels it: the
/// job ends with the Lua cancellation, not a cap. With the VM's destruction held, the
/// runner stays blocked midway, the VM still counted, and returns only
/// once it is released and gone; with the job's thread held after its
/// send, the runner stays blocked until that is released too.
#[cfg(feature = "dev-tools")]
#[test]
fn a_stop_mid_lua_cancels_and_returns_after_the_vm_and_the_thread() {
    use skin_beatoraja::lua::vm_hooks;
    let _serial = serial();
    let dir = looping();
    let dropped = Arc::new(Mutex::new(None));
    let base = vm_hooks::live();
    let midway = Duration::from_millis(300);

    let (stop, rx) = started(looping_job(dir.path(), &dropped));
    executing(base);
    stop.store(true, Ordering::SeqCst);
    cancelled(rx.recv_timeout(STOP_WAIT).unwrap());
    assert_eq!(vm_hooks::live(), base);

    // The VM's destruction held, released before the stop bound.
    vm_hooks::hold();
    let (stop, rx) = started(looping_job(dir.path(), &dropped));
    executing(base);
    stop.store(true, Ordering::SeqCst);
    assert!(wait_until(|| vm_hooks::waiting() == 1));
    assert!(
        rx.recv_timeout(midway).is_err(),
        "returned before the VM ended"
    );
    assert_eq!(vm_hooks::live(), base + 1);
    vm_hooks::release();
    cancelled(rx.recv_timeout(STOP_WAIT).unwrap());
    assert_eq!(vm_hooks::live(), base);

    // The thread held after its send.
    hold_exit(true);
    let (stop, rx) = started(looping_job(dir.path(), &dropped));
    executing(base);
    stop.store(true, Ordering::SeqCst);
    assert!(wait_until(|| exit_waiting() == 1));
    let early = rx.recv_timeout(midway);
    hold_exit(false);
    assert!(
        early.is_err(),
        "returned before the thread ended: {early:?}"
    );
    cancelled(rx.recv_timeout(STOP_WAIT).unwrap());
}

/// How late past [`STOP_WAIT`] a runner that gives up at the bound may
/// return on a loaded machine: its slices and the scheduler. The jobs it
/// is checked against cannot end before the test releases them, which it
/// does only after waiting [`HELD_FOR`]: a runner that waited for its job
/// would answer nothing within that wait.
const LATE: Duration = Duration::from_millis(750);
/// How long the abandonment tests wait for the runner, the job held
/// meanwhile; past [`STOP_WAIT`] + [`LATE`].
const HELD_FOR: Duration = Duration::from_secs(7);

/// A VM held past the stop bound when the run ends: the runner gives up
/// at the bound, the VM still counted and its job started but not joined
/// (counted by the runner itself, so another test's job meanwhile does not
/// move it). Ending the test's serialization releases the VM and waits for
/// the abandoned thread: by then its outcome, finding nobody, has been
/// dropped on its own thread, and the runner never joins it. Unheld and
/// uncancelled, the looping fixture would run to the sandbox's 5 s Lua
/// time cap; held, it never ends before the release.
#[cfg(feature = "dev-tools")]
#[test]
fn a_job_past_the_stop_wait_is_abandoned() {
    use skin_beatoraja::lua::vm_hooks;
    let serial = serial();
    let dir = looping();
    let dropped = Arc::new(Mutex::new(None));
    let base = vm_hooks::live();
    vm_hooks::hold();
    let (stop, rx) = started_own(looping_job(dir.path(), &dropped));
    executing(base);
    let stopped = Instant::now();
    stop.store(true, Ordering::SeqCst);
    let answered = rx.recv_timeout(HELD_FOR);
    let after = stopped.elapsed();
    let Ok((ran, own)) = answered else {
        panic!("{answered:?}");
    };
    assert!(matches!(ran, Ran::Abandoned), "{ran:?}");
    assert!(after >= STOP_WAIT, "{after:?}");
    assert!(after < STOP_WAIT + LATE, "{after:?}");
    assert_eq!(vm_hooks::live(), base + 1);
    assert_eq!(own, (1, 0), "its job started, not joined");
    drop(serial);
    assert_eq!(WORKERS.load(Ordering::SeqCst), 0);
    assert_eq!(dropped.lock().unwrap().as_deref(), Some("skin-import"));
    assert_eq!(vm_hooks::live(), base);
}

/// An outcome already received does not end the wait for the thread: held
/// after its send past the stop bound, the job is abandoned at the bound.
#[test]
fn a_received_outcome_still_waits_within_the_stop_bound() {
    let _serial = serial();
    hold_exit(true);
    let (stop, rx) = started(|_: &Cancel| 7_u32);
    assert!(wait_until(|| exit_waiting() == 1));
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    let stopped = Instant::now();
    stop.store(true, Ordering::SeqCst);
    let ran = rx.recv_timeout(HELD_FOR);
    let after = stopped.elapsed();
    hold_exit(false);
    assert!(matches!(ran, Ok(Ran::Abandoned)), "{ran:?}");
    assert!(after >= STOP_WAIT && after < STOP_WAIT + LATE, "{after:?}");
    // Released, an unstopped job's outcome comes once its thread ends.
    let (_stop, rx) = started(|_: &Cancel| 7_u32);
    assert!(matches!(rx.recv_timeout(STOP_WAIT), Ok(Ran::Done(7))));
}

/// Cancelling the attempt while the looping pass's Lua executes cancels
/// it, and the runner waits for the job's thread however long it takes:
/// with the VM's destruction held past the stop bound it has not
/// returned, and once released it returns the Lua cancellation, the job's
/// thread joined and its VM gone.
#[cfg(feature = "dev-tools")]
#[test]
fn an_attempts_cancel_waits_for_the_thread_past_the_stop_bound() {
    use skin_beatoraja::lua::vm_hooks;
    let _serial = serial();
    let dir = looping();
    let dropped = Arc::new(Mutex::new(None));
    let base = vm_hooks::live();
    let (started, joined) = counted();
    vm_hooks::hold();
    let (signals, rx) = started_job(looping_job(dir.path(), &dropped));
    executing(base);
    signals.attempt.store(true, Ordering::SeqCst);
    assert!(wait_until(|| vm_hooks::waiting() == 1));
    let early = rx.recv_timeout(STOP_WAIT + LATE);
    assert!(
        early.is_err(),
        "returned before the thread ended: {early:?}"
    );
    assert_eq!(vm_hooks::live(), base + 1);
    assert_eq!(counted(), (started + 1, joined));
    vm_hooks::release();
    cancelled(rx.recv_timeout(STOP_WAIT).unwrap());
    assert_eq!(vm_hooks::live(), base);
    assert_eq!(counted(), (started + 1, joined + 1));
}

/// An outcome already received under the attempt's cancellation still
/// waits for its thread past the stop bound: the runner returns the
/// outcome only once the thread, held after its send, is released.
#[test]
fn an_attempts_cancel_never_abandons_a_held_thread() {
    let _serial = serial();
    hold_exit(true);
    let (signals, rx) = started_job(|_: &Cancel| 7_u32);
    assert!(wait_until(|| exit_waiting() == 1));
    signals.attempt.store(true, Ordering::SeqCst);
    let early = rx.recv_timeout(STOP_WAIT + LATE);
    hold_exit(false);
    assert!(
        early.is_err(),
        "returned before the thread ended: {early:?}"
    );
    assert!(matches!(rx.recv_timeout(STOP_WAIT), Ok(Ran::Done(7))));
}

/// The escalation: the attempt cancelled and the VM's destruction held
/// past the stop bound, then the run's end (a close or a thread's
/// failure) without releasing it: the runner abandons the job within the
/// stop bound of that end, started and never joined. Ending the test's
/// serialization releases the VM and waits for the abandoned thread.
#[cfg(feature = "dev-tools")]
#[test]
fn the_runs_end_during_an_attempts_cancel_abandons_within_the_bound() {
    use skin_beatoraja::lua::vm_hooks;
    let serial = serial();
    let dir = looping();
    let dropped = Arc::new(Mutex::new(None));
    let base = vm_hooks::live();
    let (started, joined) = counted();
    vm_hooks::hold();
    let (signals, rx) = started_job(looping_job(dir.path(), &dropped));
    executing(base);
    signals.attempt.store(true, Ordering::SeqCst);
    assert!(wait_until(|| vm_hooks::waiting() == 1));
    let early = rx.recv_timeout(STOP_WAIT + Duration::from_millis(500));
    assert!(early.is_err(), "{early:?}");
    let ended = Instant::now();
    signals.end.store(true, Ordering::SeqCst);
    let ran = rx.recv_timeout(HELD_FOR);
    let after = ended.elapsed();
    assert!(matches!(ran, Ok(Ran::Abandoned)), "{ran:?}");
    assert!(after >= STOP_WAIT && after < STOP_WAIT + LATE, "{after:?}");
    assert_eq!(counted(), (started + 1, joined));
    drop(serial);
    assert_eq!(WORKERS.load(Ordering::SeqCst), 0);
    assert_eq!(vm_hooks::live(), base);
    assert_eq!(counted(), (started + 1, joined));
}

/// A thread that cannot start refuses at once, its job never run, and
/// counts as neither started nor joined; a normal end counts one start
/// and one join.
#[test]
fn a_spawn_failure_refuses_and_counts_nothing() {
    let _serial = serial();
    #[cfg(feature = "dev-tools")]
    let (started, joined) = counted();
    let ran = Arc::new(AtomicBool::new(false));
    refuse_spawn();
    let theirs = Arc::clone(&ran);
    match run_job(
        move |_: &Cancel| theirs.store(true, Ordering::SeqCst),
        || false,
        || false,
    ) {
        Ran::Refused(why) => {
            assert_eq!(why, "the import thread cannot start: refused by the test");
        }
        other => panic!("{other:?}"),
    }
    assert!(!ran.load(Ordering::SeqCst));
    #[cfg(feature = "dev-tools")]
    assert_eq!(counted(), (started, joined));

    assert!(matches!(
        run_job(|_: &Cancel| 7_u32, || false, || false),
        Ran::Done(7)
    ));
    #[cfg(feature = "dev-tools")]
    assert_eq!(counted(), (started + 1, joined + 1));
}

/// A panic in the job is a refusal received from its thread; a thread
/// that ends without sending (its panic's payload panicking as it is
/// dropped) is another. Each counts one start and one join.
#[test]
fn a_panic_and_a_silent_end_are_told_apart() {
    struct Bomb;
    impl Drop for Bomb {
        fn drop(&mut self) {
            panic!("the payload's drop panics");
        }
    }
    let _serial = serial();
    #[cfg(feature = "dev-tools")]
    let (started_before, joined) = counted();
    let (_stop, rx) = started(|_: &Cancel| -> u32 { panic!("the job panics") });
    match rx.recv_timeout(STOP_WAIT).unwrap() {
        Ran::Refused(why) => assert_eq!(why, PANICKED),
        other => panic!("{other:?}"),
    }
    #[cfg(feature = "dev-tools")]
    assert_eq!(counted(), (started_before + 1, joined + 1));
    let (_stop, rx) = started(|_: &Cancel| -> u32 { std::panic::panic_any(Bomb) });
    match rx.recv_timeout(STOP_WAIT).unwrap() {
        Ran::Refused(why) => assert_eq!(why, ENDED),
        other => panic!("{other:?}"),
    }
    #[cfg(feature = "dev-tools")]
    assert_eq!(counted(), (started_before + 2, joined + 2));
}

/// A logger that keeps each record's message.
#[derive(Default)]
struct Kept(Mutex<Vec<String>>);

impl log::Log for Kept {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        assert_eq!(record.target(), "tapestry");
        self.0.lock().unwrap().push(record.args().to_string());
    }

    fn flush(&self) {}
}

/// An installed skin's log: its summary line with its atlas's texels and
/// the committed total against the shared limit, counters, files and
/// choices; then at most 100 notes, located, and a count of the rest
/// with the ones the import kept only as a count.
#[test]
fn an_installed_skin_logs_its_summary_and_notes() {
    let r = crate::render_common::offscreen(1_920, 1_080);
    let dir = options_skin();
    let mut c = choice(dir.path(), "s.json");
    c.files.insert("Background".into(), "b.png".into());
    let (b, mut summary) = bundled(play(&c, &target(&r)));
    let note = Note {
        at: Some(NoteLoc {
            source: 0,
            line: 3,
            col: 7,
        }),
        severity: Severity::Ignored,
        kind: NoteKind::MissingAsset,
    };
    summary.notes = vec![note; 150];
    summary.suppressed = 2;
    let kept = Kept::default();
    let texels = b.atlas_texels();
    let atlas = Atlas {
        texels,
        side: b.side(),
    };
    let layers = texels / u64::from(b.side()).pow(2);
    log_installed(ScreenKind::Play, &summary, atlas, texels + 5, &kept);
    let logged = kept.0.into_inner().unwrap();
    assert_eq!(logged.len(), 1 + 100 + 1, "{logged:?}");
    let head = &logged[0];
    assert!(
        head.starts_with(&format!(
            "skin: play: installed; atlas {texels} texels (side {}, {layers} layers), committed {} of {} texels; ",
            b.side(),
            texels + 5,
            render::SHARED_TEXELS
        )),
        "{head}"
    );
    assert!(
        head.ends_with("; files s.json; choices Background = b.png (Configured)"),
        "{head}"
    );
    assert_eq!(
        logged[1],
        format!(
            "skin: play: s.json:3:7: Ignored: {}",
            NoteKind::MissingAsset
        )
    );
    assert_eq!(
        logged[101],
        "skin: play: 50 more notes not logged (150 kept), 2 more not kept"
    );
    // Within the cap and none suppressed: no count.
    summary.notes.truncate(2);
    summary.suppressed = 0;
    let kept = Kept::default();
    log_installed(ScreenKind::Play, &summary, atlas, texels, &kept);
    assert_eq!(kept.0.into_inner().unwrap().len(), 3);
}

/// Through the run's sink, which cuts a record at 1 KiB, a skin of many
/// definition files keeps its texel figures and counters in the summary
/// line: they come before the lists that may be cut.
#[test]
fn the_texel_figures_survive_a_long_summary() {
    let r = crate::render_common::offscreen(1_920, 1_080);
    let dir = options_skin();
    let (b, mut summary) = bundled(play(&choice(dir.path(), "s.json"), &target(&r)));
    summary.sources = (0..60)
        .map(|i| format!("parts/a-rather-long-definition-name-{i:02}.json"))
        .collect();
    let texels = b.atlas_texels();
    let atlas = Atlas {
        texels,
        side: b.side(),
    };
    let layers = texels / u64::from(b.side()).pow(2);
    let lines = crate::log_file::through_the_sink(|log| {
        log_installed(ScreenKind::Play, &summary, atlas, texels + 5, log);
    });
    let head = &lines[0];
    let u = &summary.usage;
    let figures = format!(
        "skin: play: installed; atlas {texels} texels (side {}, {layers} layers), committed {} of {} texels; \
         elements {} images {} frames {} files {} fonts {} texts {} notes {}; files ",
        b.side(),
        texels + 5,
        render::SHARED_TEXELS,
        u.elements,
        u.images,
        u.frames,
        u.files,
        u.fonts,
        u.texts,
        u.notes
    );
    assert!(head.starts_with(&figures), "{head}");
    assert!(
        !head.contains("parts/a-rather-long-definition-name-59.json"),
        "{head}"
    );
}

/// The reports' text: each kind's line, a non-play kind's notice, and an
/// admission refusal reaching play's notice as its text only.
#[test]
fn the_reports_carry_text_only() {
    assert_eq!(
        line(ScreenKind::Play, &Shown::Installed),
        "skin: play: installed"
    );
    assert_eq!(
        line(ScreenKind::Result, &Shown::BuiltIn("why".into())),
        "skin: result: built-in: why"
    );
    assert_eq!(
        line(ScreenKind::Decision, &Shown::Refused("why".into())),
        "skin: decision: refused: why"
    );
    for (kind, shown) in [
        (
            ScreenKind::Play,
            "プレイのスキンは使えません（内蔵画面）: why",
        ),
        (
            ScreenKind::Selection,
            "選曲のスキンは使えません（内蔵画面）: why",
        ),
        (
            ScreenKind::Decision,
            "決定のスキンは使えません（内蔵画面）: why",
        ),
        (
            ScreenKind::Result,
            "リザルトのスキンは使えません（内蔵画面）: why",
        ),
    ] {
        assert_eq!(notice(kind, "why"), shown);
    }
    let admission = render::RenderError::Admission {
        kind: ScreenKind::Play,
        needed: render::SHARED_TEXELS + 1,
        limit: render::SHARED_TEXELS,
    };
    let reason = Chain(&admission).to_string();
    let state =
        crate::render_side::SkinState::answered(Ok(crate::render_side::refusal(reason.clone())));
    let crate::render_side::SkinState::Refused(shown) = state else {
        panic!("{state:?}");
    };
    assert_eq!(shown, format!("スキンを使えません: {reason}"));
    let notice_of = |answer| match crate::render_side::SkinState::answered(answer) {
        crate::render_side::SkinState::Refused(shown) => shown,
        state => panic!("{state:?}"),
    };
    assert_eq!(
        notice_of(Ok(game::Preparation::Cancelled)),
        "スキンの準備は取り消されました"
    );
    assert_eq!(
        notice_of(Err(crate::render_side::ScreenError::Ended)),
        "スキンを準備できません: the render thread ended"
    );
}

mod attempt;
mod startup;
