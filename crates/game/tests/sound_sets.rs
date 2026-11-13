//! Sound sets: the scan, each kind's resolution, the recovery state that
//! quarantines a set a launch ended while decoding, and one pick's decode
//! under its markers (design.md "Audio", system sounds; "Threat Model",
//! T18). The sounds are tiny WAV files written here.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use audio::{BankLimits, Decoded, FileRefusal, GenId, PickBudget, SystemKind};
use game::DataRoot;
use game::sound_sets::{
    DecodeFile, DiskFs, KeptSounds, OneFile, PickDecode, Recovery, RecoveryFs, Refused, Resolution,
    Resolved, ScanCap, ScanLimits, SetPick, SetRole, Source, Unavailable, Why, choose, decode_pick,
    random_index, resolve, scan,
};

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("sound-sets")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
}

/// A 16-bit mono WAV at 48 kHz holding `frames` frames of `value`.
fn wav(frames: usize, value: i16) -> Vec<u8> {
    let data = u32::try_from(frames * 2).unwrap();
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&48_000u32.to_le_bytes());
    b.extend_from_slice(&96_000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for _ in 0..frames {
        b.extend_from_slice(&value.to_le_bytes());
    }
    b
}

/// Writes a short sound at `dir/name`, making its folders.
fn put(dir: &Path, name: &str) -> PathBuf {
    put_bytes(dir, name, &wav(64, 100))
}

fn put_bytes(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    path
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap()
}

/// A folder link: a junction on Windows, a symbolic link elsewhere.
fn link_dir(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// A file link, or `None` when this system does not let the test make one.
fn link_file(link: &Path, target: &Path) -> Option<()> {
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(target, link);
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    match made {
        Ok(()) => Some(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => None,
        // ERROR_PRIVILEGE_NOT_HELD: file links need Developer Mode or
        // elevation.
        Err(e) if cfg!(windows) && e.raw_os_error() == Some(1314) => None,
        Err(e) => panic!("cannot create a file link: {e}"),
    }
}

fn pick(sound: Option<&Path>, bgm: Option<&Path>, defaults: &Path) -> SetPick {
    SetPick {
        sound_set: sound.map(Path::to_owned),
        bgm_set: bgm.map(Path::to_owned),
        defaults: defaults.to_owned(),
        id: GenId(7),
        rate: 48_000,
        limits: BankLimits::default(),
        skipped: Vec::new(),
    }
}

fn resolved(r: &Resolution, kind: SystemKind) -> &Resolved {
    &r.kinds[kind.index()]
}

fn found(r: &Resolution, kind: SystemKind) -> (PathBuf, Source) {
    match resolved(r, kind) {
        Resolved::Found { path, source } => (path.clone(), *source),
        other => panic!("{kind:?}: {other:?}"),
    }
}

/// ST1: a set is a folder whose own marker resolves, at any depth and the
/// root included; a folder holding only other kinds is not one; a link or
/// junction is never entered, a linked root included. A link back to the
/// root would run the scan down to its depth cap if it were entered.
// Covers: T18
#[test]
fn scan_finds_sets_at_any_depth_and_never_enters_links() {
    let base = tmp();
    let root = base.path().join("sound");
    let outside = base.path().join("outside");
    put(&root, "clear.wav");
    put(&root.join("a/b/c"), "clear.ogg");
    put(&root.join("x"), "scratch.wav");
    put(&outside, "clear.wav");
    link_dir(&root.join("linked"), &outside);
    link_dir(&root.join("loop"), &root);
    let limits = ScanLimits::default();
    let scanned = scan(&root, SetRole::Sound, &limits);
    assert_eq!(scanned.sets, [canon(&root), canon(&root.join("a/b/c"))]);
    assert_eq!(scanned.caps, []);
    // The BGM marker is `select`: none here.
    assert_eq!(
        scan(&root, SetRole::Bgm, &limits).sets,
        Vec::<PathBuf>::new()
    );
    let linked_root = base.path().join("linked-root");
    link_dir(&linked_root, &root);
    assert_eq!(
        scan(&linked_root, SetRole::Sound, &limits).sets,
        Vec::<PathBuf>::new()
    );
    assert_eq!(
        scan(&base.path().join("absent"), SetRole::Sound, &limits).sets,
        Vec::<PathBuf>::new()
    );
}

/// ST2: each cap at its bound: a set at depth 8 is found and one at depth
/// 9 is not, while a set listed after that deep branch is still found (the
/// depth cap prunes, it does not stop the scan); the 4 096th folder is
/// listed and the 4 097th is not; the 65 536th entry is listed and the
/// 65 537th is not. Each cap is named once.
#[test]
fn scan_caps_stop_at_their_bounds() {
    let limits = ScanLimits::default();
    assert_eq!(
        (limits.depth, limits.folders, limits.entries),
        (8, 4_096, 65_536)
    );

    let base = tmp();
    let root = base.path().join("deep");
    let eight: PathBuf = (1..=8).map(|d| format!("d{d}")).collect();
    put(&root.join(&eight), "clear.wav");
    put(&root.join(&eight).join("d9"), "clear.wav");
    put(&root.join(&eight).join("e9"), "clear.wav");
    put(&root.join("z"), "clear.wav");
    let scanned = scan(&root, SetRole::Sound, &limits);
    assert_eq!(
        scanned.sets,
        [canon(&root.join(&eight)), canon(&root.join("z"))]
    );
    assert_eq!(scanned.caps, [ScanCap::Depth]);

    // The root and 4 096 folders: the root is listed first, so the last
    // folder in order is the 4 097th.
    let root = base.path().join("wide");
    for i in 0..4_096 {
        fs::create_dir_all(root.join(format!("f{i:04}"))).unwrap();
    }
    for i in [0, 4_094, 4_095] {
        put(&root.join(format!("f{i:04}")), "clear.wav");
    }
    let scanned = scan(&root, SetRole::Sound, &limits);
    assert_eq!(
        scanned.sets,
        [canon(&root.join("f0000")), canon(&root.join("f4094"))]
    );
    assert_eq!(scanned.caps, [ScanCap::Folders]);

    // The root lists 65 533 files and the folders `a` and `b`; `a` lists
    // its marker, the 65 536th entry, and `b`'s marker is the 65 537th.
    let root = base.path().join("many");
    fs::create_dir_all(&root).unwrap();
    for i in 0..65_533 {
        fs::File::create(root.join(format!("n{i:05}"))).unwrap();
    }
    put(&root.join("a"), "clear.wav");
    put(&root.join("b"), "clear.wav");
    let scanned = scan(&root, SetRole::Sound, &limits);
    assert_eq!(scanned.sets, [canon(&root.join("a"))]);
    assert_eq!(scanned.caps, [ScanCap::Entries]);
    put(&root.join("a"), "fail.wav");
    let scanned = scan(&root, SetRole::Sound, &limits);
    assert_eq!(scanned.sets, Vec::<PathBuf>::new(), "a's listing is cut");
    assert_eq!(scanned.caps, [ScanCap::Entries]);
}

/// ST3: the marker is looked up in the folder alone: an empty folder is not
/// a set though `defaultsound\` holds the marker.
#[test]
fn markers_are_set_local() {
    let base = tmp();
    let data = DataRoot::new(base.path().join("data"));
    put(&data.default_sounds(), "clear.wav");
    fs::create_dir_all(data.sound().join("empty")).unwrap();
    let scanned = scan(&data.sound(), SetRole::Sound, &ScanLimits::default());
    assert_eq!(scanned.sets, Vec::<PathBuf>::new());
}

/// ST4: a kind's file is its `.wav` name, then `.flac`, `.ogg` and `.mp3`,
/// in the picked set and then in `defaultsound\`, where the four
/// extensions are tried too; a name without an extension is not a
/// candidate.
#[test]
fn resolution_follows_the_sources_order() {
    let base = tmp();
    let sound = base.path().join("sound/set");
    let bgm = base.path().join("bgm/set");
    let defaults = base.path().join("defaultsound");
    for name in ["fail.flac", "fail.ogg", "fail.mp3", "clear", "clear.wav"] {
        put(&sound, name);
    }
    put(&bgm, "select.wav");
    put(&defaults, "decide.wav");
    put(&defaults, "clear.ogg");
    let r = resolve(&pick(Some(&sound), Some(&bgm), &defaults), &mut |_| false);
    assert_eq!(
        found(&r, SystemKind::Fail),
        (canon(&sound.join("fail.flac")), Source::SoundSet)
    );
    assert_eq!(
        found(&r, SystemKind::Clear),
        (canon(&sound.join("clear.wav")), Source::SoundSet)
    );
    assert_eq!(
        found(&r, SystemKind::Select),
        (canon(&bgm.join("select.wav")), Source::BgmSet)
    );
    assert_eq!(
        found(&r, SystemKind::Decide),
        (canon(&defaults.join("decide.wav")), Source::Defaults)
    );
    assert_eq!(resolved(&r, SystemKind::Scratch), &Resolved::Missing);
    assert_eq!(r.refused, []);

    let only = base.path().join("only-defaults");
    put(&only, "select.ogg");
    let r = resolve(&pick(None, None, &only), &mut |_| false);
    assert_eq!(
        found(&r, SystemKind::Select),
        (canon(&only.join("select.ogg")), Source::Defaults)
    );
    assert_eq!(resolved(&r, SystemKind::Decide), &Resolved::Missing);

    // One kind with all four: each removed in turn gives the next.
    let four = base.path().join("four");
    for ext in ["mp3", "ogg", "flac", "wav"] {
        put(&four, &format!("scratch.{ext}"));
    }
    for (ext, next) in [("wav", "flac"), ("flac", "ogg"), ("ogg", "mp3")] {
        let r = resolve(&pick(Some(&four), None, &only), &mut |_| false);
        assert_eq!(
            found(&r, SystemKind::Scratch),
            (
                canon(&four.join(format!("scratch.{ext}"))),
                Source::SoundSet
            )
        );
        fs::remove_file(four.join(format!("scratch.{ext}"))).unwrap();
        let r = resolve(&pick(Some(&four), None, &only), &mut |_| false);
        assert_eq!(
            found(&r, SystemKind::Scratch).0,
            canon(&four.join(format!("scratch.{next}")))
        );
    }
}

/// ST5: a candidate that is a folder, a link out of its folder or a FIFO is
/// refused without being opened, named once, and the next location is
/// tried; a kind whose every candidate is refused is silent, not missing.
// Covers: T18
#[test]
fn refused_candidates_are_named_and_skipped() {
    let base = tmp();
    let sound = base.path().join("sound/set");
    let defaults = base.path().join("defaultsound");
    let outside = base.path().join("outside");
    fs::create_dir_all(sound.join("clear.wav")).unwrap();
    fs::create_dir_all(sound.join("scratch.wav")).unwrap();
    let out = put(&outside, "fail.wav");
    let linked = link_file(&sound.join("fail.wav"), &out).is_some();
    #[cfg(unix)]
    {
        let status = std::process::Command::new("mkfifo")
            .arg(sound.join("o-open.wav"))
            .status()
            .expect("this test needs `mkfifo`");
        assert!(status.success());
    }
    for name in ["clear.wav", "fail.wav", "o-open.wav"] {
        put(&defaults, name);
    }
    let p = pick(Some(&sound), None, &defaults);
    // Resolution opens nothing, so a FIFO cannot block it.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(resolve(&p, &mut |_| false));
    });
    let r = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("resolution blocked");
    let csound = canon(&sound);
    let mut want: Vec<Refused> = vec![
        Refused {
            path: csound.join("clear.wav"),
            why: Why::NotAFile,
        },
        Refused {
            path: csound.join("scratch.wav"),
            why: Why::NotAFile,
        },
    ];
    assert_eq!(found(&r, SystemKind::Clear).1, Source::Defaults);
    assert_eq!(resolved(&r, SystemKind::Scratch), &Resolved::Refused);
    if linked {
        want.push(Refused {
            path: csound.join("fail.wav"),
            why: Why::Escapes,
        });
        assert_eq!(found(&r, SystemKind::Fail).1, Source::Defaults);
    }
    if cfg!(unix) {
        want.push(Refused {
            path: csound.join("o-open.wav"),
            why: Why::NotAFile,
        });
        assert_eq!(found(&r, SystemKind::OptionOpen).1, Source::Defaults);
    }
    let mut got = r.refused.clone();
    got.sort_by(|a, b| a.path.cmp(&b.path));
    want.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(got, want, "each refused once");
}

/// What the recovery seam and the decode seam saw, in order.
#[derive(Default)]
struct Seen {
    ops: Vec<String>,
    writes: usize,
}

/// The disk, with each lock, read and write logged and chosen writes
/// failing.
struct SeamFs {
    disk: DiskFs,
    seen: Arc<Mutex<Seen>>,
    /// Writes (from 1) that fail.
    fail_writes: Vec<usize>,
}

impl SeamFs {
    fn new(seen: &Arc<Mutex<Seen>>, fail_writes: &[usize]) -> Box<Self> {
        Box::new(Self {
            disk: DiskFs::default(),
            seen: Arc::clone(seen),
            fail_writes: fail_writes.to_vec(),
        })
    }
}

impl RecoveryFs for SeamFs {
    fn lock(&mut self, path: &Path) -> io::Result<bool> {
        self.seen.lock().unwrap().ops.push("lock".to_owned());
        self.disk.lock(path)
    }

    fn read(&mut self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.seen.lock().unwrap().ops.push("read".to_owned());
        self.disk.read(path)
    }

    fn write(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut seen = self.seen.lock().unwrap();
        seen.writes += 1;
        let n = seen.writes;
        let text = String::from_utf8_lossy(bytes);
        let what = if text.contains("decoding") {
            "mark"
        } else {
            "commit"
        };
        if self.fail_writes.contains(&n) {
            seen.ops.push(format!("{what} failed"));
            return Err(io::Error::other("refused (test)"));
        }
        seen.ops.push(what.to_owned());
        drop(seen);
        self.disk.write(path, bytes)
    }
}

/// The real decode, with each file logged.
struct LoggedDecode(Arc<Mutex<Seen>>);

impl DecodeFile for LoggedDecode {
    fn decode(
        &self,
        path: &Path,
        rate: u32,
        budget: &PickBudget,
        limits: &BankLimits,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> Result<Decoded, FileRefusal> {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        self.0.lock().unwrap().ops.push(format!("decode {name}"));
        OneFile.decode(path, rate, budget, limits, cancel)
    }
}

fn ops(seen: &Arc<Mutex<Seen>>) -> Vec<String> {
    std::mem::take(&mut seen.lock().unwrap().ops)
}

/// A data root with a sound set holding `clear.wav` and `fail.wav`.
struct Fixture {
    _base: tempfile::TempDir,
    data: DataRoot,
    set: PathBuf,
}

fn fixture() -> Fixture {
    let base = tmp();
    let data = DataRoot::new(base.path().join("data"));
    let set = data.sound().join("set");
    put_bytes(&set, "clear.wav", &wav(480, 200));
    put_bytes(&set, "fail.wav", &wav(240, -200));
    Fixture {
        _base: base,
        data,
        set,
    }
}

fn set_pick(f: &Fixture) -> SetPick {
    pick(Some(&f.set), None, &f.data.default_sounds())
}

fn run(
    f: &Fixture,
    recovery: &mut Recovery,
    seen: &Arc<Mutex<Seen>>,
) -> game::sound_sets::PickReport {
    let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
    decode_pick(
        &set_pick(f),
        &KeptSounds::default(),
        &budget,
        recovery,
        &|| false,
        &LoggedDecode(Arc::clone(seen)),
    )
}

fn identity_of(f: &Fixture) -> game::sound_sets::SourceIdentity {
    resolve(&set_pick(f), &mut |_| false).sources[0]
        .clone()
        .unwrap()
}

fn state(f: &Fixture) -> String {
    fs::read_to_string(f.data.sound_sets()).unwrap()
}

/// A change made to a fixture's set, giving the set's folder after it.
type Change = fn(&Fixture) -> PathBuf;

/// ST10: the recovery cases, through the file-system seam.
// Covers: T18
#[test]
fn recovery_cases() {
    // Q2: no file: it is created and committed before the first decode; a
    // pick commits a marker before reading the set's files and clears it
    // after.
    let f = fixture();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mut recovery = Recovery::open(&f.data, SeamFs::new(&seen, &[]))
        .unwrap()
        .recovery;
    let report = run(&f, &mut recovery, &seen);
    assert!(
        matches!(report.decode, PickDecode::Ready(_)),
        "{:?}",
        report.decode
    );
    assert_eq!(report.recovery, Ok(()));
    assert_eq!(
        ops(&seen),
        [
            "lock",
            "read",
            "commit",
            "mark",
            "decode clear.wav",
            "decode fail.wav",
            "commit"
        ]
    );
    assert_eq!(state(&f), "version = 1\n");

    // Q1: a second owner cannot take the lock and decodes nothing.
    let second = Recovery::open(&f.data, Box::new(DiskFs::default()));
    assert!(
        matches!(second, Err(Unavailable::Locked { .. })),
        "{second:?}"
    );
    drop(recovery);
    assert!(
        Recovery::open(&f.data, Box::new(DiskFs::default())).is_ok(),
        "the lock ends with its owner"
    );

    // Q3: another version, or garbage: refused, the file untouched.
    for (text, version) in [("version = 2\n", true), ("not = [toml", false)] {
        fs::write(f.data.sound_sets(), text).unwrap();
        let got = Recovery::open(&f.data, Box::new(DiskFs::default()));
        match got {
            Err(Unavailable::Version { found: Some(2), .. }) if version => {}
            Err(Unavailable::Parse { .. }) if !version => {}
            other => panic!("{other:?} for {text}"),
        }
        assert_eq!(state(&f), text);
    }
    let e = Recovery::open(&f.data, Box::new(DiskFs::default()))
        .unwrap_err()
        .to_string();
    assert!(e.contains("move it aside"), "{e}");
    fs::remove_file(f.data.sound_sets()).unwrap();

    // Q4: a marker left by a launch that ended while decoding is
    // quarantined at the next launch: the set is not decoded again.
    let identity = identity_of(&f);
    {
        let mut r = Recovery::open(&f.data, Box::new(DiskFs::default()))
            .unwrap()
            .recovery;
        r.mark(&identity).unwrap();
    }
    let opened = Recovery::open(&f.data, SeamFs::new(&seen, &[])).unwrap();
    assert_eq!(opened.quarantined, Some(identity.clone()));
    assert_eq!(
        opened.recovery.quarantined(),
        std::slice::from_ref(&identity)
    );
    assert!(!state(&f).contains("decoding"), "{}", state(&f));
    let mut recovery = opened.recovery;
    ops(&seen);
    let report = run(&f, &mut recovery, &seen);
    assert_eq!(
        ops(&seen),
        Vec::<String>::new(),
        "the quarantined set was read"
    );
    let PickDecode::Ready(generation) = report.decode else {
        panic!()
    };
    assert_eq!(generation.sound(SystemKind::Clear), None);
    drop(recovery);

    // Q8: a change of name, size, modification time or path makes another
    // identity: the set is tried again, and its stale entry is dropped at
    // the next commit.
    let changes: [(&str, Change); 4] = [
        ("name", |f| {
            put(&f.set, "scratch.wav");
            f.set.clone()
        }),
        ("size", |f| {
            put_bytes(&f.set, "fail.wav", &wav(250, -200));
            f.set.clone()
        }),
        ("time", |f| {
            let file = fs::File::options()
                .write(true)
                .open(f.set.join("clear.wav"))
                .unwrap();
            let then = file.metadata().unwrap().modified().unwrap();
            file.set_modified(then + std::time::Duration::from_secs(61))
                .unwrap();
            f.set.clone()
        }),
        ("path", |f| {
            let moved = f.set.with_file_name("moved");
            for name in ["clear.wav", "fail.wav"] {
                put_bytes(&moved, name, &fs::read(f.set.join(name)).unwrap());
            }
            moved
        }),
    ];
    for (what, change) in changes {
        let f = fixture();
        let identity = identity_of(&f);
        {
            let mut r = Recovery::open(&f.data, Box::new(DiskFs::default()))
                .unwrap()
                .recovery;
            r.mark(&identity).unwrap();
        }
        let mut recovery = Recovery::open(&f.data, Box::new(DiskFs::default()))
            .unwrap()
            .recovery;
        let set = change(&f);
        let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
        let report = decode_pick(
            &pick(Some(&set), None, &f.data.default_sounds()),
            &KeptSounds::default(),
            &budget,
            &mut recovery,
            &|| false,
            &OneFile,
        );
        let PickDecode::Ready(generation) = report.decode else {
            panic!("{what}")
        };
        assert!(
            generation.sound(SystemKind::Clear).is_some(),
            "{what}: not tried again"
        );
        let written = state(&f);
        assert_eq!(
            written.contains("[[quarantined]]"),
            what == "path",
            "{what}: {written}"
        );
    }

    // An interrupted decode of `defaultsound\` alone, across two launches
    // with no set: the next launch quarantines it.
    let f = fixture();
    put(&f.data.default_sounds(), "decide.wav");
    let defaults = pick(None, None, &f.data.default_sounds());
    let marker = resolve(&defaults, &mut |_| false).sources[2]
        .clone()
        .unwrap();
    {
        let mut r = Recovery::open(&f.data, Box::new(DiskFs::default()))
            .unwrap()
            .recovery;
        r.mark(&marker).unwrap();
    }
    let opened = Recovery::open(&f.data, Box::new(DiskFs::default())).unwrap();
    assert_eq!(opened.quarantined, Some(marker));
    let mut recovery = opened.recovery;
    let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
    let report = decode_pick(
        &defaults,
        &KeptSounds::default(),
        &budget,
        &mut recovery,
        &|| false,
        &LoggedDecode(Arc::clone(&seen)),
    );
    ops(&seen);
    let PickDecode::Ready(generation) = report.decode else {
        panic!()
    };
    assert_eq!(generation.sound(SystemKind::Decide), None);
    drop(recovery);

    // Q5: the marker's commit fails: nothing of the set is read.
    let f = fixture();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mut recovery = Recovery::open(&f.data, SeamFs::new(&seen, &[2]))
        .unwrap()
        .recovery;
    let report = run(&f, &mut recovery, &seen);
    assert!(
        matches!(report.recovery, Err(Unavailable::Commit { .. })),
        "{:?}",
        report.recovery
    );
    assert_eq!(ops(&seen), ["lock", "read", "commit", "mark failed"]);
    drop(recovery);

    // Q6: the clear fails, after a ready decode and after a refusal: the
    // recovery is unavailable, whatever the decode came to.
    for refused in [false, true] {
        let f = fixture();
        if refused {
            put_bytes(&f.set, "fail.wav", b"not a sound");
        }
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut recovery = Recovery::open(&f.data, SeamFs::new(&seen, &[3]))
            .unwrap()
            .recovery;
        let report = run(&f, &mut recovery, &seen);
        assert!(
            matches!(report.recovery, Err(Unavailable::Commit { .. })),
            "{:?}",
            report.recovery
        );
        assert!(matches!(report.decode, PickDecode::Ready(_)));
        assert_eq!(report.refused.len(), usize::from(refused));
        assert_eq!(ops(&seen).last().map(String::as_str), Some("commit failed"));
    }

    // Q7: the quarantine's commit fails at startup.
    let f = fixture();
    let identity = identity_of(&f);
    {
        let mut r = Recovery::open(&f.data, Box::new(DiskFs::default()))
            .unwrap()
            .recovery;
        r.mark(&identity).unwrap();
    }
    let seen = Arc::new(Mutex::new(Seen::default()));
    let got = Recovery::open(&f.data, SeamFs::new(&seen, &[1]));
    assert!(matches!(got, Err(Unavailable::Commit { .. })), "{got:?}");
}

/// ST10b: a pick cancelled mid-file clears its marker and reports
/// `Cancelled` with the recovery whole; with the clear failing, the
/// recovery is unavailable.
#[test]
fn a_cancelled_pick_clears_its_marker() {
    for clear_fails in [false, true] {
        let f = fixture();
        // 20 s: many packets.
        put_bytes(&f.set, "clear.wav", &wav(960_000, 300));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let fail: &[usize] = if clear_fails { &[3] } else { &[] };
        let mut recovery = Recovery::open(&f.data, SeamFs::new(&seen, fail))
            .unwrap()
            .recovery;
        let asked = std::sync::atomic::AtomicUsize::new(0);
        // The tenth question is inside the first file's packets.
        let cancel = || asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 >= 10;
        let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
        let report = decode_pick(
            &set_pick(&f),
            &KeptSounds::default(),
            &budget,
            &mut recovery,
            &cancel,
            &LoggedDecode(Arc::clone(&seen)),
        );
        assert!(
            matches!(report.decode, PickDecode::Cancelled),
            "{:?}",
            report.decode
        );
        assert_eq!(
            ops(&seen),
            [
                "lock",
                "read",
                "commit",
                "mark",
                "decode clear.wav",
                if clear_fails {
                    "commit failed"
                } else {
                    "commit"
                }
            ]
        );
        if clear_fails {
            assert!(
                matches!(report.recovery, Err(Unavailable::Commit { .. })),
                "{:?}",
                report.recovery
            );
        } else {
            assert_eq!(report.recovery, Ok(()));
            assert_eq!(recovery.decoding(), None);
            assert_eq!(state(&f), "version = 1\n");
        }
    }
}

/// A set whose identity is quarantined is never chosen; the others are.
#[test]
fn choose_leaves_out_quarantined_sets() {
    let f = fixture();
    let other = f.data.sound().join("other");
    put(&other, "clear.wav");
    let identity = identity_of(&f);
    {
        let mut r = Recovery::open(&f.data, Box::new(DiskFs::default()))
            .unwrap()
            .recovery;
        r.mark(&identity).unwrap();
    }
    let mut recovery = Recovery::open(&f.data, Box::new(DiskFs::default()))
        .unwrap()
        .recovery;
    let sets = scan(&f.data.sound(), SetRole::Sound, &ScanLimits::default()).sets;
    assert_eq!(sets.len(), 2);
    for at in [0, 1, 5] {
        let chosen = choose(&sets, SetRole::Sound, &mut recovery, &[], &mut |n| {
            assert_eq!(n, 1);
            at
        });
        assert_eq!(chosen, Some(canon(&other)));
    }
    let chosen = choose(
        &sets,
        SetRole::Sound,
        &mut recovery,
        &[identity_of_set(&other)],
        &mut |_| 0,
    );
    assert_eq!(chosen, None, "a skipped set is chosen");
    assert!(random_index(3) < 3);
}

fn identity_of_set(set: &Path) -> game::sound_sets::SourceIdentity {
    resolve(&pick(Some(set), None, Path::new("absent")), &mut |_| false).sources[0]
        .clone()
        .unwrap()
}

/// A kind the new pick finds no candidate for keeps the previous pick's
/// sound, the same PCM charged to the new pick; a kind whose candidate is
/// refused is silent; a kept sound whose source is skipped is not kept.
#[test]
fn a_kind_with_no_candidate_keeps_its_sound() {
    let f = fixture();
    put(&f.set, "scratch.wav");
    let mut recovery = Recovery::open(&f.data, Box::new(DiskFs::default()))
        .unwrap()
        .recovery;
    let first = PickBudget::new(game::sound_sets::PICK_BYTES);
    let a = decode_pick(
        &set_pick(&f),
        &KeptSounds::default(),
        &first,
        &mut recovery,
        &|| false,
        &OneFile,
    );
    let PickDecode::Ready(a_gen) = a.decode else {
        panic!()
    };
    let b_set = f.data.sound().join("b");
    put(&b_set, "clear.wav");
    fs::create_dir_all(b_set.join("fail.wav")).unwrap();
    let b_pick = pick(Some(&b_set), None, &f.data.default_sounds());
    let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
    let b = decode_pick(
        &b_pick,
        &a.kept,
        &budget,
        &mut recovery,
        &|| false,
        &OneFile,
    );
    let PickDecode::Ready(b_gen) = b.decode else {
        panic!()
    };
    let kept = b_gen.sound(SystemKind::Scratch).expect("scratch kept");
    assert!(Arc::ptr_eq(kept, a_gen.sound(SystemKind::Scratch).unwrap()));
    assert_eq!(
        b_gen.sound(SystemKind::Fail),
        None,
        "a refused candidate keeps nothing"
    );
    assert_eq!(budget.used(), b_gen.bytes());
    assert_eq!(
        b_gen.bytes(),
        kept.bytes() + b_gen.sound(SystemKind::Clear).unwrap().bytes()
    );

    let mut skipping = b_pick.clone();
    skipping
        .skipped
        .push(a.kept.get(SystemKind::Scratch).unwrap().source.clone());
    let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
    let c = decode_pick(
        &skipping,
        &a.kept,
        &budget,
        &mut recovery,
        &|| false,
        &OneFile,
    );
    let PickDecode::Ready(c_gen) = c.decode else {
        panic!()
    };
    assert_eq!(
        c_gen.sound(SystemKind::Scratch),
        None,
        "a skipped source's sound kept"
    );
}

/// A file with the same bytes as a refused one is refused and named too,
/// in the same source and in `defaultsound\`: the pick never takes it as
/// the same contents as a sound it does not hold.
// Covers: T18
#[test]
fn a_copy_of_a_refused_file_is_named_too() {
    let f = fixture();
    let garbage = b"RIFF not really a wave";
    put_bytes(&f.set, "clear.wav", garbage);
    put_bytes(&f.set, "fail.wav", garbage);
    put_bytes(&f.data.default_sounds(), "scratch.wav", garbage);
    let mut recovery = Recovery::open(&f.data, Box::new(DiskFs::default()))
        .unwrap()
        .recovery;
    let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
    let report = decode_pick(
        &set_pick(&f),
        &KeptSounds::default(),
        &budget,
        &mut recovery,
        &|| false,
        &OneFile,
    );
    let mut named: Vec<PathBuf> = report
        .refused
        .iter()
        .map(|r| {
            assert!(
                matches!(r.why, Why::File(FileRefusal::Undecodable(_))),
                "{r:?}"
            );
            r.path.clone()
        })
        .collect();
    named.sort();
    let mut want = vec![
        canon(&f.set.join("clear.wav")),
        canon(&f.set.join("fail.wav")),
        canon(&f.data.default_sounds().join("scratch.wav")),
    ];
    want.sort();
    assert_eq!(named, want, "each refused file named once");
    assert_eq!(budget.used(), 0);
}
